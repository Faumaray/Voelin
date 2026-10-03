//! Decoded images by key (emoji now; avatars and server icons later): each
//! is decoded once and kept while it fits the budget (setting
//! `ui.image_cache_mb`); the least recently used go first.
//!
//! The cache lives on the UI thread apart from the app state, because Slint
//! asks for images (`Images.emoji`) while it evaluates bindings, which may
//! happen while the app state is borrowed.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

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

/// A picture the engine put in its cache (avatars, group and client icons),
/// decoded once and kept by its path.
pub fn file(path: &std::path::Path) -> Image {
	let key = path.to_string_lossy();
	CACHE
		.with(|c| {
			c.borrow_mut().get_or_load(&key, || {
				let image = Image::load_from_path(path).ok()?;
				let size = image.size();
				Some((image, size.width as usize * size.height as usize * 4))
			})
		})
		.unwrap_or_default()
}

/// A picture from encoded bytes (PNG, JPEG, GIF, WebP), decoded once and
/// kept by `key`; an empty image if it cannot be decoded.
pub fn picture(key: &str, bytes: &[u8]) -> Image {
	let cache_key = format!("picture:{key}");
	CACHE
		.with(|c| {
			c.borrow_mut().get_or_load(&cache_key, || {
				let image = Image::load_from_data(bytes, None).ok()?;
				let size = image.size();
				Some((image, size.width as usize * size.height as usize * 4))
			})
		})
		.unwrap_or_default()
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
	}

	#[test]
	fn emoji_images_decode() {
		let image = emoji("1f600");
		assert!(image.size().width > 0);
		assert_eq!(emoji("nope").size().width, 0);
		assert!(usage_text().contains("of 64 MB"));
	}
}
