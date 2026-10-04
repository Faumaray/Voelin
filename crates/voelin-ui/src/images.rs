//! Decoded images by key (emoji, avatars, icons and banners): each
//! is decoded once and kept while it fits the budget (setting
//! `ui.image_cache_mb`); the least recently used go first.
//!
//! The cache lives on the UI thread apart from the app state, because Slint
//! asks for images (`Images.emoji`) while it evaluates bindings, which may
//! happen while the app state is borrowed.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;

use slint::Image;

use crate::emoji;

/// An LRU map with a byte budget. Generic over the value for tests.
pub struct Lru<V> {
	budget: usize,
	used: usize,
	tick: u64,
	entries: HashMap<String, (V, usize, u64)>,
	/// Keys by last use.
	order: BTreeMap<u64, String>,
}

impl<V: Clone> Lru<V> {
	pub fn new(budget: usize) -> Self {
		Self { budget, used: 0, tick: 0, entries: HashMap::new(), order: BTreeMap::new() }
	}

	/// Bytes in use and the budget.
	pub fn usage(&self) -> (usize, usize) {
		(self.used, self.budget)
	}

	pub fn len(&self) -> usize {
		self.entries.len()
	}

	pub fn set_budget(&mut self, budget: usize) {
		self.budget = budget;
		self.evict();
	}

	/// The value of `key`, or the loaded one (`load` gives it with its cost
	/// in bytes). A value larger than the budget is returned but not kept.
	pub fn get_or_load(
		&mut self,
		key: &str,
		load: impl FnOnce() -> Option<(V, usize)>,
	) -> Option<V> {
		self.tick += 1;
		let tick = self.tick;
		if let Some((value, _, last)) = self.entries.get_mut(key) {
			self.order.remove(last);
			*last = tick;
			self.order.insert(tick, key.to_owned());
			return Some(value.clone());
		}
		let (value, cost) = load()?;
		if cost <= self.budget {
			self.entries.insert(key.to_owned(), (value.clone(), cost, tick));
			self.order.insert(tick, key.to_owned());
			self.used += cost;
			self.evict();
		}
		Some(value)
	}

	/// Drop `key`, so the next use loads it again.
	pub fn remove(&mut self, key: &str) {
		if let Some((_, cost, tick)) = self.entries.remove(key) {
			self.order.remove(&tick);
			self.used -= cost;
		}
	}

	fn evict(&mut self) {
		while self.used > self.budget {
			let Some((_, key)) = self.order.pop_first() else { break };
			if let Some((_, cost, _)) = self.entries.remove(&key) {
				self.used -= cost;
			}
		}
	}
}

/// What an SVG costs in the cache: the renderer keeps rasterized copies at
/// the sizes shown, so count a 64×64 RGBA picture besides the source.
fn svg_cost(svg_len: usize) -> usize {
	svg_len + 64 * 64 * 4
}

thread_local! {
	static CACHE: RefCell<Lru<Image>> = RefCell::new(Lru::new(64 << 20));
}

/// Set the budget in megabytes.
pub fn set_budget_mb(mb: u32) {
	CACHE.with(|c| c.borrow_mut().set_budget(mb as usize * (1 << 20)));
}

/// "3.2 of 64 MB (120 images)"
pub fn usage_text() -> String {
	CACHE.with(|c| {
		let c = c.borrow();
		let (used, budget) = c.usage();
		format!("{:.1} of {} MB ({} images)", used as f64 / (1 << 20) as f64, budget >> 20, c.len())
	})
}

/// What remembering a picture that cannot be decoded costs in the cache.
const UNDECODABLE_COST: usize = 256;

/// A picture the engine put in its cache (avatars, group and client icons),
/// decoded once and kept by its path.
pub fn file(path: &std::path::Path) -> Image {
	let key = path.to_string_lossy();
	CACHE
		.with(|c| {
			c.borrow_mut().get_or_load(&key, || {
				// By content, not by name: the cache's files have no extension
				// (`avatars/<md5>`, `icons/<id>`), and servers keep PNG, JPEG
				// and SVG icons alike.
				let bytes = std::fs::read(path).ok()?;
				// One that cannot be shown (too large, not a picture) is
				// remembered as such, not read again on every refresh; a
				// changed file is forgotten first (`forget`).
				Some(decode(&bytes).unwrap_or((Image::default(), UNDECODABLE_COST)))
			})
		})
		.unwrap_or_default()
}

/// Forget the decoded picture of a file that changed (a reloaded banner).
pub fn forget(path: &std::path::Path) {
	CACHE.with(|c| c.borrow_mut().remove(&path.to_string_lossy()));
}

/// Compressed pictures can be small on disk but huge when decoded. Check
/// raster dimensions before Slint allocates pixels on the UI thread. SVGs
/// keep Slint's native vector loader and are rasterized at the displayed size.
fn decode(bytes: &[u8]) -> Option<(Image, usize)> {
	let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
	let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
	let cost = if reader.format().is_some() {
		let (width, height) = reader.into_dimensions().ok()?;
		// At most the default decoded-image cache (64 MiB of RGBA).
		if width == 0
			|| height == 0
			|| width > 16_384
			|| height > 16_384
			|| u64::from(width) * u64::from(height) * 4 > 64 << 20
		{
			tracing::debug!(width, height, "image exceeds the decoded raster size limit");
			return None;
		}
		width as usize * height as usize * 4
	} else {
		let start = bytes.trim_ascii_start();
		if !start.starts_with(b"<svg") && !start.starts_with(b"<?xml") {
			return None;
		}
		// Intrinsic SVG dimensions describe coordinates, not an allocated
		// pixel buffer. Use the same vector cost as the emoji cache.
		return Image::load_from_svg_data(start).ok().map(|image| (image, svg_cost(bytes.len())));
	};
	Image::load_from_data(bytes, None).ok().map(|image| (image, cost))
}

/// A picture from encoded bytes (PNG, JPEG, GIF, WebP), decoded once and
/// kept by `key`; an empty image if it cannot be decoded.
pub fn picture(key: &str, bytes: &[u8]) -> Image {
	let cache_key = format!("picture:{key}");
	CACHE.with(|c| c.borrow_mut().get_or_load(&cache_key, || decode(bytes))).unwrap_or_default()
}

/// A Twemoji by key; an empty image if there is none.
pub fn emoji(key: &str) -> Image {
	if key.is_empty() {
		return Image::default();
	}
	let cache_key = format!("emoji:{key}");
	CACHE
		.with(|c| {
			c.borrow_mut().get_or_load(&cache_key, || {
				let svg = emoji::archive().svg(key)?;
				let image = Image::load_from_svg_data(&svg).ok()?;
				Some((image, svg_cost(svg.len())))
			})
		})
		.unwrap_or_default()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn lru_budget() {
		let mut lru: Lru<u32> = Lru::new(10);
		let loads = std::cell::Cell::new(0);
		let get = |lru: &mut Lru<u32>, k: &str, cost: usize| {
			lru.get_or_load(k, || {
				loads.set(loads.get() + 1);
				Some((k.len() as u32, cost))
			})
		};
		get(&mut lru, "a", 4);
		get(&mut lru, "b", 4);
		get(&mut lru, "a", 4); // a is now newer than b
		get(&mut lru, "c", 4); // evicts b
		assert_eq!(lru.usage(), (8, 10));
		assert_eq!(lru.len(), 2);
		get(&mut lru, "a", 4);
		get(&mut lru, "b", 4); // loaded again
		// Too large to keep, still returned.
		assert_eq!(get(&mut lru, "huge", 100), Some(4));
		assert!(lru.len() <= 2);
		assert_eq!(loads.get(), 5);
		lru.set_budget(0);
		assert_eq!(lru.usage(), (0, 0));
		// Unknown keys are not cached.
		assert_eq!(lru.get_or_load("none", || None), None);
		lru.set_budget(10);
		get(&mut lru, "a", 4);
		lru.remove("a");
		lru.remove("never");
		assert_eq!((lru.usage(), lru.len()), ((0, 10), 0));
		get(&mut lru, "a", 4);
		assert_eq!(loads.get(), 7, "loaded again after remove");
	}

	#[test]
	fn pictures_decode() {
		let mut png = Vec::new();
		let mut encoder = png::Encoder::new(&mut png, 4, 2);
		encoder.set_color(png::ColorType::Rgba);
		encoder.set_depth(png::BitDepth::Eight);
		encoder.write_header().unwrap().write_image_data(&[200; 32]).unwrap();
		assert_eq!(picture("test:4x2", &png).size().width, 4);
		assert_eq!(picture("test:junk", b"not a picture").size().width, 0);
	}

	#[test]
	fn svg_with_bom_and_whitespace_decodes() {
		let svg = b"\xef\xbb\xbf \n<svg xmlns='http://www.w3.org/2000/svg' width='32' height='16'><rect width='32' height='16'/></svg>";
		assert_eq!(picture("test:svg-bom", svg).size().width, 32);
	}

	#[test]
	fn large_svg_coordinates_do_not_overflow_the_cache_cost() {
		let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="3000000000" height="3000000000"><rect width="1" height="1"/></svg>"#;
		let (image, cost) = decode(svg).unwrap();
		assert!(image.size().width > 0);
		assert_eq!(cost, svg_cost(svg.len()));
		assert!(picture("test:large-svg", svg).size().width > 0);
	}

	#[test]
	fn oversized_rasters_are_refused_before_pixels_are_read() {
		use std::io::Write;
		for (width, height) in [(6000, 6000), (16_385, 1), (1, 16_385)] {
			let mut bytes = Vec::new();
			let mut encoder = png::Encoder::new(&mut bytes, width, height);
			encoder.set_color(png::ColorType::Rgba);
			encoder.set_depth(png::BitDepth::Eight);
			let mut writer = encoder.write_header().unwrap();
			let mut stream = writer.stream_writer().unwrap();
			let row = vec![255; width as usize * 4];
			for _ in 0..height {
				stream.write_all(&row).unwrap();
			}
			stream.finish().unwrap();
			writer.finish().unwrap();
			// A valid solid picture fits the download cap but not the
			// decoded budget. Generate it a row at a time, without ever
			// allocating the large pixel buffer this test guards against.
			assert!(bytes.len() < 4 << 20);
			assert_eq!(
				image::ImageReader::new(Cursor::new(&bytes))
					.with_guessed_format()
					.unwrap()
					.into_dimensions()
					.unwrap(),
				(width, height)
			);
			assert!(decode(&bytes).is_none());
		}
	}

	/// Cached avatars and icons are named without an extension.
	#[test]
	fn cached_files_decode_by_content() {
		let dir = std::env::temp_dir().join(format!("voelin-images-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let mut png = Vec::new();
		let mut encoder = png::Encoder::new(&mut png, 3, 2);
		encoder.set_color(png::ColorType::Rgba);
		encoder.set_depth(png::BitDepth::Eight);
		encoder.write_header().unwrap().write_image_data(&[90; 24]).unwrap();
		let svg = br#"<?xml version="1.0" encoding="iso-8859-1"?>
<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16"/></svg>"#;
		// GIF avatars and banners are common on TeamSpeak 3 servers.
		let gif = b"GIF89a\x01\0\x01\0\x80\0\0\xff\xff\xff\0\0\0!\xf9\x04\x01\0\0\0\0,\0\0\0\0\x01\0\x01\0\0\x02\x02D\x01\0;";
		let mut webp = Vec::new();
		image::ImageEncoder::write_image(
			image::codecs::webp::WebPEncoder::new_lossless(&mut webp),
			&[90; 24],
			3,
			2,
			image::ExtendedColorType::Rgba8,
		)
		.unwrap();
		for (name, bytes, width) in [
			("287478770", &png[..], 3),
			("413901487", &svg[..], 16),
			("gif", &gif[..], 1),
			("webp", &webp[..], 3),
		] {
			let path = dir.join(name);
			std::fs::write(&path, bytes).unwrap();
			assert_eq!(file(&path).size().width, width, "{name}");
		}
		// A file that changed is decoded again once forgotten.
		let path = dir.join("287478770");
		let mut wider = Vec::new();
		let mut encoder = png::Encoder::new(&mut wider, 5, 2);
		encoder.set_color(png::ColorType::Rgba);
		encoder.set_depth(png::BitDepth::Eight);
		encoder.write_header().unwrap().write_image_data(&[90; 40]).unwrap();
		std::fs::write(&path, wider).unwrap();
		assert_eq!(file(&path).size().width, 3, "kept until forgotten");
		forget(&path);
		assert_eq!(file(&path).size().width, 5);
		// A file that is no picture is remembered as such until forgotten,
		// not read again on every refresh.
		let junk = dir.join("junk");
		std::fs::write(&junk, b"<!DOCTYPE html>").unwrap();
		assert_eq!(file(&junk).size().width, 0);
		std::fs::write(&junk, &png).unwrap();
		assert_eq!(file(&junk).size().width, 0, "remembered");
		forget(&junk);
		assert_eq!(file(&junk).size().width, 3);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn emoji_images_decode() {
		let image = emoji("1f600");
		assert!(image.size().width > 0);
		assert_eq!(emoji("nope").size().width, 0);
		assert!(usage_text().contains("of 64 MB"));
	}
}
