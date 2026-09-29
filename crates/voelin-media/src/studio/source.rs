//! The live inputs behind a scene's sources.
//!
//! [`Input`] starts whatever a [`SourceKind`] needs and publishes pictures
//! into a [`Feed`] the compositor reads:
//!
//! - colour, text and image are drawn once (they never change by themselves)
//!   and put into the feed; a changed text or colour is drawn again
//! - screen, window, portal and the test pattern run one of the
//!   [`crate::capture`] backends, which hands every frame to a [`FeedSink`]
//!   on its own thread
//! - camera runs one of the [`crate::studio::camera`] backends
//!
//! Nothing here fails loudly: a camera that is gone or an image that cannot
//! be read leaves [`Feed::error`] set and the source draws nothing, so one
//! broken source never stops the studio.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ab_glyph::{Font, FontRef, Glyph, PxScale, ScaleFont, point};
use tracing::{debug, warn};

use crate::capture::{
	CaptureOptions, FramePacer, FrameSink, ScreenCapture, synthetic::SyntheticScreen,
};
use crate::frame::{FrameData, FrameRef, PixelFormat, PixelsRef, Plane, VideoFrame};
use crate::pool::FramePool;
use crate::studio::camera;
use crate::studio::compose::Feed;
use crate::studio::scene::{Align, Colour, SourceKind};
use crate::{Error, Result};

/// The UI's font, for text sources without one of their own. The same file
/// `crates/voelin-ui` bundles (its notice is in `about-assets.md`); reached
/// from here because tools without the UI crate rasterise text too.
const DEFAULT_FONT: &[u8] = include_bytes!("../../../voelin-ui/ui/assets/fonts/Inter-Regular.ttf");

/// An RGBA frame of one flat colour.
pub fn colour_frame(colour: Colour, size: (u32, u32)) -> VideoFrame {
	let (width, height) = (size.0.max(1), size.1.max(1));
	let rgba = colour.to_rgba();
	let mut data = Vec::with_capacity(width as usize * height as usize * 4);
	for _ in 0..u64::from(width) * u64::from(height) {
		data.extend_from_slice(&rgba);
	}
	VideoFrame {
		width,
		height,
		timestamp: Duration::ZERO,
		data: FrameData::Rgba(Plane::new(data, width as usize * 4)),
	}
}

/// An RGBA frame from an image file (PNG, JPEG, ...), alpha kept.
pub fn image_frame(path: &Path) -> Result<VideoFrame> {
	let image = image::open(path)
		.map_err(|e| Error::InvalidFrame(format!("cannot read {}: {e}", path.display())))?
		.into_rgba8();
	let (width, height) = (image.width(), image.height());
	if width == 0 || height == 0 {
		return Err(Error::InvalidFrame(format!("{} is empty", path.display())));
	}
	Ok(VideoFrame {
		width,
		height,
		timestamp: Duration::ZERO,
		data: FrameData::Rgba(Plane::new(image.into_raw(), width as usize * 4)),
	})
}

/// Straight-alpha source-over of `src` (with coverage `coverage`) onto `dst`.
fn over(dst: &mut [u8], src: [u8; 4], coverage: f32) {
	let sa = f32::from(src[3]) / 255.0 * coverage.clamp(0.0, 1.0);
	if sa <= 0.0 {
		return;
	}
	let da = f32::from(dst[3]) / 255.0;
	let out = sa + da * (1.0 - sa);
	for c in 0..3 {
		let s = f32::from(src[c]) * sa;
		let d = f32::from(dst[c]) * da * (1.0 - sa);
		dst[c] = ((s + d) / out).round().clamp(0.0, 255.0) as u8;
	}
	dst[3] = (out * 255.0).round().clamp(0.0, 255.0) as u8;
}

/// An RGBA frame with `text` rasterised: lines split at `\n`, `padding`
/// pixels around them, glyphs in `colour` over `backdrop`.
///
/// `font`: a TrueType / OpenType file; `None` uses the UI's font.
pub fn text_frame(
	text: &str,
	font: Option<&Path>,
	size_px: f32,
	colour: Colour,
	backdrop: Colour,
	align: Align,
	padding: u32,
) -> Result<VideoFrame> {
	let owned = match font {
		Some(path) => Some(std::fs::read(path).map_err(|e| {
			Error::InvalidFrame(format!("cannot read the font {}: {e}", path.display()))
		})?),
		None => None,
	};
	let bytes = owned.as_deref().unwrap_or(DEFAULT_FONT);
	let font = FontRef::try_from_slice(bytes)
		.map_err(|e| Error::InvalidFrame(format!("not a usable font: {e}")))?;
	let scaled = font.as_scaled(PxScale::from(size_px.clamp(1.0, 4096.0)));
	let line_height = scaled.height() + scaled.line_gap();
	// Lay every line out at the origin first, to know how wide it gets.
	let mut lines: Vec<(Vec<Glyph>, f32)> = Vec::new();
	for line in text.split('\n') {
		let mut glyphs = Vec::new();
		let mut caret = 0.0;
		let mut previous = None;
		for ch in line.chars() {
			if ch.is_control() {
				continue;
			}
			let mut glyph = scaled.scaled_glyph(ch);
			if let Some(previous) = previous {
				caret += scaled.kern(previous, glyph.id);
			}
			previous = Some(glyph.id);
			glyph.position = point(caret, 0.0);
			caret += scaled.h_advance(glyph.id);
			glyphs.push(glyph);
		}
		lines.push((glyphs, caret));
	}
	let pad = padding as f32;
	let widest = lines.iter().map(|(_, w)| *w).fold(0.0f32, f32::max);
	let width = (widest + 2.0 * pad).ceil().max(1.0) as u32;
	let height = (lines.len() as f32 * line_height + 2.0 * pad).ceil().max(1.0) as u32;
	let mut frame = colour_frame(backdrop, (width, height));
	let FrameData::Rgba(plane) = &mut frame.data else { unreachable!("colour_frame is RGBA") };
	let (stride, rgba) = (plane.stride, colour.to_rgba());
	for (i, (glyphs, line_width)) in lines.iter().enumerate() {
		let free = widest - line_width;
		let offset = pad
			+ match align {
				Align::Left => 0.0,
				Align::Center => free / 2.0,
				Align::Right => free,
			};
		let baseline = pad + scaled.ascent() + i as f32 * line_height;
		for glyph in glyphs {
			let mut placed = glyph.clone();
			placed.position = point(glyph.position.x + offset, baseline);
			let Some(outline) = scaled.outline_glyph(placed) else { continue };
			let bounds = outline.px_bounds();
			outline.draw(|x, y, coverage| {
				let px = bounds.min.x as i64 + i64::from(x);
				let py = bounds.min.y as i64 + i64::from(y);
				if px < 0 || py < 0 || px >= i64::from(width) || py >= i64::from(height) {
					return;
				}
				let at = py as usize * stride + px as usize * 4;
				over(&mut plane.data[at..at + 4], rgba, coverage);
			});
		}
	}
	Ok(frame)
}

/// Copies every captured frame into a pooled frame for a [`Feed`]: BGRA and
/// RGBA as they are (the compositor takes both), anything else converted to
/// RGBA. Allocates nothing per frame once the pool is warm.
pub struct FeedSink {
	feed: Arc<Feed>,
	pool: FramePool,
	fps: Arc<AtomicU32>,
	pacer: FramePacer,
	rate: u32,
}

impl FeedSink {
	/// `fps` is shared with the studio, so the rate can change while
	/// capturing.
	pub fn new(feed: Arc<Feed>, fps: Arc<AtomicU32>) -> Self {
		let rate = fps.load(Ordering::Relaxed).max(1);
		Self { feed, pool: FramePool::new(), fps, pacer: FramePacer::new(Some(rate)), rate }
	}

	fn follow_rate(&mut self) -> u32 {
		let rate = self.fps.load(Ordering::Relaxed).max(1);
		if rate != self.rate {
			self.rate = rate;
			self.pacer.set_fps(Some(rate));
		}
		rate
	}
}

impl FrameSink for FeedSink {
	fn max_fps(&self) -> u32 {
		self.fps.load(Ordering::Relaxed).max(1)
	}

	fn wants(&mut self, timestamp: Duration) -> bool {
		if self.feed.is_closed() {
			return false;
		}
		self.follow_rate();
		self.pacer.due(timestamp)
	}

	fn frame(&mut self, frame: FrameRef<'_>) -> bool {
		if self.feed.is_closed() {
			return false;
		}
		self.pacer.keep(frame.timestamp);
		match deliver(&mut self.pool, &self.feed, &frame) {
			Ok(()) => true,
			Err(e) => {
				warn!("cannot take a studio source frame: {e}");
				self.feed.set_error(e);
				true
			}
		}
	}
}

/// Copy `frame` into a pooled frame and publish it.
pub fn deliver(pool: &mut FramePool, feed: &Feed, frame: &FrameRef<'_>) -> Result<()> {
	frame.validate()?;
	let (width, height) = (frame.width, frame.height);
	let row = width as usize * 4;
	let format = match frame.pixels {
		PixelsRef::Bgra(_) => PixelFormat::Bgra,
		// I420, NV12 and the rest become RGBA.
		_ => PixelFormat::Rgba,
	};
	let slot = pool.get_format(width, height, format);
	let target = Arc::get_mut(slot).expect("the pool hands out unshared frames");
	target.timestamp = frame.timestamp;
	let (FrameData::Bgra(plane) | FrameData::Rgba(plane)) = &mut target.data else {
		return Err(Error::InvalidFrame("the studio source pool is not packed".into()));
	};
	match frame.pixels {
		PixelsRef::Bgra(src) | PixelsRef::Rgba(src) => {
			for y in 0..height as usize {
				plane.data[y * plane.stride..][..row].copy_from_slice(src.row(y, row));
			}
		}
		_ => crate::convert::to_rgba_ref(frame, &mut plane.data, plane.stride)?,
	}
	feed.put(slot.clone());
	Ok(())
}

/// The screen capture backend a source asks for by name.
fn screen_backend(name: Option<&str>) -> Result<Box<dyn ScreenCapture>> {
	let unavailable = |backend: &'static str| Error::CaptureUnavailable {
		backend,
		reason: format!("{backend} capture is not in this build"),
	};
	match name.map(str::trim).filter(|n| !n.is_empty() && *n != "auto") {
		None => crate::capture::default_screen_capture(),
		#[cfg(all(target_os = "linux", feature = "pipewire"))]
		Some("portal") => Ok(Box::new(crate::capture::portal::PortalCapture::new())),
		#[cfg(all(target_os = "linux", feature = "wlroots"))]
		Some("wlroots") => Ok(Box::new(crate::capture::wlroots::WlrootsCapture::new())),
		#[cfg(all(target_os = "linux", feature = "x11"))]
		Some("x11") => Ok(Box::new(crate::capture::x11::X11Capture::new())),
		#[cfg(windows)]
		Some("windows") => Ok(Box::new(crate::capture::windows::WindowsCapture::new())),
		#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
		Some("portal") => Err(unavailable("portal")),
		#[cfg(not(all(target_os = "linux", feature = "wlroots")))]
		Some("wlroots") => Err(unavailable("wlroots")),
		#[cfg(not(all(target_os = "linux", feature = "x11")))]
		Some("x11") => Err(unavailable("x11")),
		#[cfg(not(windows))]
		Some("windows") => Err(unavailable("windows")),
		Some(other) => Err(Error::CaptureUnavailable {
			backend: "screen",
			reason: format!("no capture backend named {other:?}"),
		}),
	}
}

/// A source's live input: its [`Feed`] and whatever produces the pictures.
pub struct Input {
	pub feed: Arc<Feed>,
	kind: SourceKind,
	fps: Arc<AtomicU32>,
	/// Stopped when dropped.
	screen: Option<Box<dyn ScreenCapture>>,
	camera: Option<camera::Capture>,
	/// The portal's token for this choice, to skip its dialog next time.
	restore_token: Option<String>,
}

impl Input {
	/// Start the input of `kind` at up to `fps`. Never fails: a source that
	/// cannot start has its reason in [`Feed::error`] and draws nothing.
	///
	/// Must run on a Tokio runtime for the portal kinds (they talk D-Bus).
	pub async fn start(kind: SourceKind, fps: u32) -> Self {
		let feed = Arc::new(Feed::new());
		let mut input = Self {
			feed: feed.clone(),
			fps: Arc::new(AtomicU32::new(fps.max(1))),
			kind,
			screen: None,
			camera: None,
			restore_token: None,
		};
		if let Err(e) = input.begin().await {
			warn!(kind = input.kind.label(), "studio source: {e}");
			feed.set_error(e);
		}
		input
	}

	async fn begin(&mut self) -> Result<()> {
		match &self.kind {
			SourceKind::Colour { colour, size } => {
				self.feed.put(Arc::new(colour_frame(*colour, *size)));
			}
			SourceKind::Image { path } => {
				self.feed.put(Arc::new(image_frame(path)?));
			}
			SourceKind::Text { text, font, size_px, colour, backdrop, align, padding } => {
				let frame = text_frame(
					text,
					font.as_deref(),
					*size_px,
					*colour,
					*backdrop,
					*align,
					*padding,
				)?;
				self.feed.put(Arc::new(frame));
			}
			SourceKind::Camera { device, size, fps, .. } => {
				let rate = fps.unwrap_or_else(|| self.fps.load(Ordering::Relaxed));
				let capture =
					camera::Capture::start(device, *size, rate, self.feed.clone()).await?;
				debug!(device = %capture.backend(), "studio camera");
				self.camera = Some(capture);
			}
			kind => {
				let source = kind.capture_source().expect("every other kind captures a screen");
				let (backend, cursor) = match kind {
					SourceKind::Screen { backend, cursor, .. }
					| SourceKind::Window { backend, cursor, .. } => (backend.as_deref(), *cursor),
					SourceKind::Portal { cursor, .. } => (Some("portal"), *cursor),
					_ => (None, false),
				};
				let options = CaptureOptions {
					fps: self.fps.load(Ordering::Relaxed),
					cursor,
					..CaptureOptions::default()
				};
				let sink = Box::new(FeedSink::new(self.feed.clone(), self.fps.clone()));
				match kind {
					SourceKind::Pattern { size } => {
						let mut screen = SyntheticScreen::new(size.0.max(2), size.1.max(2));
						screen.start_sink(&source, &options, sink).await?;
						self.screen = Some(Box::new(screen));
					}
					// Its own arm: the token it grants comes off the backend.
					#[cfg(all(target_os = "linux", feature = "pipewire"))]
					SourceKind::Portal { restore_token, .. } => {
						use crate::capture::portal::PortalCapture;
						let mut portal = PortalCapture::with_restore_token(restore_token.clone());
						portal.start_sink(&source, &options, sink).await?;
						self.restore_token = portal.restore_token().map(str::to_owned);
						self.screen = Some(Box::new(portal));
					}
					_ => {
						let mut screen = screen_backend(backend)?;
						screen.start_sink(&source, &options, sink).await?;
						self.screen = Some(screen);
					}
				}
			}
		}
		Ok(())
	}

	pub fn kind(&self) -> &SourceKind {
		&self.kind
	}

	/// Frame-rate cap; the capture follows without restarting.
	pub fn set_fps(&self, fps: u32) {
		self.fps.store(fps.max(1), Ordering::Relaxed);
	}

	/// The portal's token for this choice, if it granted one.
	pub fn restore_token(&self) -> Option<&str> {
		self.restore_token.as_deref()
	}

	/// The camera device this input actually opened, for the UI.
	pub fn camera_id(&self) -> Option<&str> {
		self.camera.as_ref().map(camera::Capture::device)
	}
}

impl Drop for Input {
	fn drop(&mut self) {
		self.feed.close();
		if let Some(screen) = &mut self.screen {
			screen.stop();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn pixel(frame: &VideoFrame, x: u32, y: u32) -> [u8; 4] {
		let FrameData::Rgba(p) = &frame.data else { panic!("not RGBA") };
		p.row(y as usize, frame.width as usize * 4)[x as usize * 4..][..4].try_into().unwrap()
	}

	#[test]
	fn colour_source_is_flat() {
		let frame = colour_frame(Colour::rgba(1, 2, 3, 4), (3, 2));
		assert_eq!((frame.width, frame.height), (3, 2));
		assert_eq!(frame.format(), PixelFormat::Rgba);
		frame.validate().unwrap();
		assert_eq!(pixel(&frame, 2, 1), [1, 2, 3, 4]);
		// A zero size is rounded up, not an empty frame.
		assert_eq!((colour_frame(Colour::BLACK, (0, 0)).width, 1), (1, 1));
	}

	#[test]
	fn text_is_rasterised_with_the_bundled_font() {
		let frame =
			text_frame("Hi", None, 48.0, Colour::WHITE, Colour::CLEAR, Align::Left, 4).unwrap();
		assert!(frame.width > 20 && frame.height > 20, "{}x{}", frame.width, frame.height);
		frame.validate().unwrap();
		let FrameData::Rgba(p) = &frame.data else { panic!() };
		// Some glyph pixels are opaque white, the padding is transparent.
		let opaque = p.data.chunks_exact(4).filter(|px| px[3] > 200).count();
		assert!(opaque > 20, "only {opaque} glyph pixels");
		assert!(p.data.chunks_exact(4).all(|px| px[3] == 0 || px[0] > 100));
		assert_eq!(pixel(&frame, 0, 0), [0, 0, 0, 0], "the padding is clear");

		// Two lines are taller, and centring is symmetric.
		let two = text_frame("Hi\nHi", None, 48.0, Colour::WHITE, Colour::CLEAR, Align::Center, 0)
			.unwrap();
		assert!(two.height > frame.height);
		// A backdrop fills everything.
		let filled =
			text_frame("x", None, 24.0, Colour::WHITE, Colour::rgb(9, 9, 9), Align::Right, 2)
				.unwrap();
		assert_eq!(pixel(&filled, 0, 0), [9, 9, 9, 255]);
		// An empty text still gives a usable frame.
		text_frame("", None, 24.0, Colour::WHITE, Colour::CLEAR, Align::Left, 0)
			.unwrap()
			.validate()
			.unwrap();
		// A font that is not one says so.
		assert!(
			text_frame(
				"x",
				Some(Path::new("/nonexistent.ttf")),
				24.0,
				Colour::WHITE,
				Colour::CLEAR,
				Align::Left,
				0
			)
			.is_err()
		);
	}

	#[test]
	fn images_are_read_with_their_alpha() {
		let dir = std::env::temp_dir().join(format!("voelin-studio-image-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("half.png");
		// 2x1 PNG: opaque red, half-transparent green.
		let mut image = image::RgbaImage::new(2, 1);
		image.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
		image.put_pixel(1, 0, image::Rgba([0, 255, 0, 128]));
		image.save(&path).unwrap();
		let frame = image_frame(&path).unwrap();
		assert_eq!((frame.width, frame.height), (2, 1));
		assert_eq!(pixel(&frame, 0, 0), [255, 0, 0, 255]);
		assert_eq!(pixel(&frame, 1, 0), [0, 255, 0, 128]);
		assert!(image_frame(&dir.join("missing.png")).is_err());
		std::fs::remove_dir_all(&dir).ok();
	}

	#[test]
	fn unknown_backends_are_named() {
		let Err(e) = screen_backend(Some("nonsense")) else { panic!("a made-up backend started") };
		assert!(e.to_string().contains("nonsense"), "{e}");
		// `auto` and an empty name are the default backend.
		assert_eq!(screen_backend(Some("auto")).is_ok(), screen_backend(None).is_ok());
	}

	#[tokio::test]
	async fn the_test_pattern_and_static_kinds_feed() {
		let input = Input::start(SourceKind::Pattern { size: (64, 48) }, 30).await;
		let started = std::time::Instant::now();
		while input.feed.delivered() == 0 && started.elapsed() < Duration::from_secs(5) {
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
		assert!(input.feed.delivered() > 0, "the test pattern delivered nothing");
		assert_eq!(input.feed.size(), (64, 48));
		assert_eq!(input.feed.error(), None);
		let frame = input.feed.take().expect("a frame");
		assert!(matches!(frame.data, FrameData::Bgra(_) | FrameData::Rgba(_)));
		input.set_fps(5);

		let colour =
			Input::start(SourceKind::Colour { colour: Colour::rgb(4, 5, 6), size: (8, 8) }, 30)
				.await;
		assert_eq!(colour.feed.delivered(), 1, "drawn once");
		assert_eq!(pixel(&colour.feed.take().unwrap(), 0, 0), [4, 5, 6, 255]);

		// A broken source reports and draws nothing.
		let broken = Input::start(SourceKind::Image { path: "/nonexistent.png".into() }, 30).await;
		assert!(broken.feed.error().is_some());
		assert!(broken.feed.take().is_none());
	}
}
