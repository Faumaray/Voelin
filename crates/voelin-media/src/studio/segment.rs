//! Background replacement: blur, an image or a colour behind the person.
//!
//! A [`Segmenter`] produces a small greyscale [`Mask`] — 255 where the person
//! is — on a thread of its own and at a lower rate than the video, because
//! segmentation costs far more than a frame does and a mask is still good
//! enough a few frames later. The frame path takes the newest mask through a
//! latest-wins handoff, upsamples and feathers it, and mixes the person over a
//! background it makes with [`blur_rgba`], an image or a colour.
//!
//! The blur is a three-pass box blur (which approximates a Gaussian closely
//! enough that nobody sees the difference at these radii), separable and with
//! a running sum, so its cost does not grow with the radius. It runs in bands
//! on a [`Workers`] pool over pooled buffers: no allocation per frame.
//!
//! # What segments
//!
//! [`Ellipse`] is the segmenter that is always there: a centred oval, which is
//! roughly where a person sits in front of a camera. It is honest about being
//! a placeholder — it exercises the mask path and the tests, and gives a
//! usable "blur everything but the middle" — but it does not find a person.
//!
//! A real segmenter is a small neural network (the MediaPipe-style selfie
//! segmentation models are what this is sized for: one 256x144 or 256x256
//! input, one mask output). That is not in this build: nothing here loads an
//! ONNX model yet, so [`Background::Blur`](crate::studio::scene::Background)
//! blurs around an oval rather than around a person. The seam is
//! [`Segmenter`]; a model-backed implementation drops in behind it without
//! touching the frame path.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tracing::warn;

use crate::frame::{FrameData, VideoFrame};
use crate::handoff::Handoff;
use crate::studio::scene::{Background, Colour};
use crate::studio::source::image_frame;
use crate::workers::{Slots, Workers};
use crate::{Error, Result};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How often a mask is computed by default, however fast the video runs.
pub const DEFAULT_MASK_FPS: u32 = 10;
/// The width a frame is reduced to before segmentation; the height follows
/// the aspect ratio.
const SEGMENT_WIDTH: u32 = 256;

/// A greyscale coverage map: 255 where the subject is, 0 where the background
/// is. Usually much smaller than the frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mask {
	pub width: u32,
	pub height: u32,
	/// `width * height` bytes.
	pub data: Vec<u8>,
}

impl Mask {
	pub fn new(width: u32, height: u32) -> Self {
		let (width, height) = (width.max(1), height.max(1));
		Self { width, height, data: vec![0; width as usize * height as usize] }
	}

	/// Resize the buffer for `width` x `height`, keeping the allocation.
	pub fn resize(&mut self, width: u32, height: u32) {
		let (width, height) = (width.max(1), height.max(1));
		self.width = width;
		self.height = height;
		self.data.clear();
		self.data.resize(width as usize * height as usize, 0);
	}

	/// The mask value at a point of a `width` x `height` frame, bilinear.
	fn sample(&self, x: u32, y: u32, width: u32, height: u32) -> u8 {
		let fx = (u64::from(x) * u64::from(self.width) / u64::from(width.max(1))) as u32;
		let fy = (u64::from(y) * u64::from(self.height) / u64::from(height.max(1))) as u32;
		let (fx, fy) = (fx.min(self.width - 1), fy.min(self.height - 1));
		self.data[fy as usize * self.width as usize + fx as usize]
	}
}

/// Something that says where the subject is.
pub trait Segmenter: Send {
	/// For logs and the UI.
	fn name(&self) -> &'static str;

	/// Fill `mask` from `frame`: packed RGBA, the source's picture reduced
	/// to at most 256 pixels wide with its aspect ratio kept. A model with a
	/// fixed input size scales it to that itself; the mask may have any size.
	fn mask(&mut self, frame: &VideoFrame, mask: &mut Mask) -> Result<()>;
}

/// The segmenter that is always available: a centred oval covering most of
/// the frame. See the [module docs](self) for what it is and is not.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ellipse {
	/// Half-width of the oval as a fraction of the frame.
	pub half_width: f32,
	/// Half-height, as a fraction.
	pub half_height: f32,
}

impl Ellipse {
	/// The proportions a person in front of a camera roughly fills.
	pub fn person() -> Self {
		Self { half_width: 0.33, half_height: 0.48 }
	}
}

impl Segmenter for Ellipse {
	fn name(&self) -> &'static str {
		"ellipse"
	}

	fn mask(&mut self, frame: &VideoFrame, mask: &mut Mask) -> Result<()> {
		mask.resize(frame.width, frame.height);
		let (w, h) = (mask.width as f32, mask.height as f32);
		let (cx, cy) = (w / 2.0, h * 0.55);
		let (rx, ry) =
			((self.half_width.max(0.01) * w).max(1.0), (self.half_height.max(0.01) * h).max(1.0));
		for y in 0..mask.height {
			for x in 0..mask.width {
				let dx = (x as f32 + 0.5 - cx) / rx;
				let dy = (y as f32 + 0.5 - cy) / ry;
				let d = dx * dx + dy * dy;
				// 1 inside, fading to 0 over the last tenth of the radius.
				let coverage = ((1.1 - d) / 0.2).clamp(0.0, 1.0);
				mask.data[y as usize * mask.width as usize + x as usize] = (coverage * 255.0) as u8;
			}
		}
		Ok(())
	}
}

/// Blur `plane` (packed RGBA, `stride` bytes per row) in place with a
/// three-pass box blur of radius `radius`, using `scratch` (at least as
/// large) as the second buffer. Allocates nothing.
pub fn blur_rgba(
	workers: &mut Workers,
	plane: &mut [u8],
	scratch: &mut [u8],
	width: usize,
	height: usize,
	stride: usize,
	radius: usize,
) -> Result<()> {
	if radius == 0 || width == 0 || height == 0 {
		return Ok(());
	}
	let needed = stride * (height - 1) + width * 4;
	if plane.len() < needed || scratch.len() < needed {
		return Err(Error::InvalidFrame("the blur buffers are too small".into()));
	}
	// Three box passes approximate a Gaussian; each pass is horizontal then
	// vertical, so six sweeps in all.
	for _ in 0..3 {
		blur_rows(workers, plane, scratch, width, height, stride, radius);
		blur_columns(workers, scratch, plane, width, height, stride, radius);
	}
	Ok(())
}

/// One horizontal box pass from `src` into `dst`.
fn blur_rows(
	workers: &mut Workers,
	src: &[u8],
	dst: &mut [u8],
	width: usize,
	height: usize,
	stride: usize,
	radius: usize,
) {
	let tasks = workers.tasks(height);
	let band = height.div_ceil(tasks);
	let tasks = height.div_ceil(band);
	let slots = Slots::new(dst.chunks_mut(band * stride).take(tasks).enumerate());
	workers.run(slots.len(), &|task| {
		let Some((i, out)) = slots.take(task) else { return };
		let rows = i * band..((i + 1) * band).min(height);
		for y in rows.clone() {
			let row = &src[y * stride..][..width * 4];
			let line = &mut out[(y - rows.start) * stride..][..width * 4];
			// A running sum over the window, one channel at a time.
			let mut sum = [0u32; 4];
			let mut count = 0u32;
			for x in 0..=radius.min(width - 1) {
				for c in 0..4 {
					sum[c] += u32::from(row[x * 4 + c]);
				}
				count += 1;
			}
			for x in 0..width {
				for c in 0..4 {
					line[x * 4 + c] = ((sum[c] + count / 2) / count) as u8;
				}
				// Slide: drop the pixel leaving, add the one entering.
				if x >= radius {
					let out_x = x - radius;
					for c in 0..4 {
						sum[c] -= u32::from(row[out_x * 4 + c]);
					}
					count -= 1;
				}
				let in_x = x + radius + 1;
				if in_x < width {
					for c in 0..4 {
						sum[c] += u32::from(row[in_x * 4 + c]);
					}
					count += 1;
				}
			}
		}
	});
}

/// One vertical box pass from `src` into `dst`.
///
/// Split into bands of rows, like every other pass, so each task writes its
/// own slice: a band restarts the running sum at its first row, which costs
/// `radius` extra reads per column per band and keeps the pass free of
/// `unsafe`.
fn blur_columns(
	workers: &mut Workers,
	src: &[u8],
	dst: &mut [u8],
	width: usize,
	height: usize,
	stride: usize,
	radius: usize,
) {
	let tasks = workers.tasks(height);
	let band = height.div_ceil(tasks);
	let tasks = height.div_ceil(band);
	let slots = Slots::new(dst.chunks_mut(band * stride).take(tasks).enumerate());
	workers.run(slots.len(), &|task| {
		let Some((i, out)) = slots.take(task) else { return };
		let rows = i * band..((i + 1) * band).min(height);
		for x in 0..width {
			let mut sum = [0u32; 4];
			let mut count = 0u32;
			let first = rows.start.saturating_sub(radius);
			let last = (rows.start + radius).min(height - 1);
			for y in first..=last {
				for c in 0..4 {
					sum[c] += u32::from(src[y * stride + x * 4 + c]);
				}
				count += 1;
			}
			for y in rows.clone() {
				let at = (y - rows.start) * stride + x * 4;
				for c in 0..4 {
					out[at + c] = ((sum[c] + count / 2) / count) as u8;
				}
				if y >= radius {
					let leaving = y - radius;
					for c in 0..4 {
						sum[c] -= u32::from(src[leaving * stride + x * 4 + c]);
					}
					count -= 1;
				}
				let entering = y + radius + 1;
				if entering < height {
					for c in 0..4 {
						sum[c] += u32::from(src[entering * stride + x * 4 + c]);
					}
					count += 1;
				}
			}
		}
	});
}

/// What goes behind the subject.
enum Backdrop {
	/// The frame itself, blurred by this radius.
	Blur(usize),
	/// An image, already scaled to the frame (packed RGBA).
	Image(VideoFrame),
	Colour([u8; 4]),
}

/// Replaces the background of the frames of one source; see the
/// [module docs](self).
pub struct BackgroundFilter {
	mode: Arc<Mutex<Background>>,
	/// Newest frame for the segmenter, and newest mask for the frame path.
	to_segmenter: Arc<Handoff<VideoFrame>>,
	masks: Arc<Handoff<Mask>>,
	mask: Option<Arc<Mask>>,
	workers: Workers,
	/// Scratch for the blur (two planes: the blurred copy and its pass
	/// buffer).
	scratch: Vec<u8>,
	scaler: crate::studio::compose::RgbaScaler,
	backdrop: Option<(Background, Backdrop)>,
	thread: Option<std::thread::JoinHandle<()>>,
	stop: Arc<std::sync::atomic::AtomicBool>,
	masks_made: Arc<AtomicU64>,
	mask_fps: Arc<AtomicU32>,
	segmenter: &'static str,
}

impl BackgroundFilter {
	/// A filter running `segmenter` at [`DEFAULT_MASK_FPS`].
	pub fn new(mode: Background, segmenter: Box<dyn Segmenter>) -> Self {
		let to_segmenter = Arc::new(Handoff::new());
		let masks = Arc::new(Handoff::new());
		let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
		let masks_made = Arc::new(AtomicU64::new(0));
		let mask_fps = Arc::new(AtomicU32::new(DEFAULT_MASK_FPS));
		let name = segmenter.name();
		let thread = {
			let (frames, out) = (to_segmenter.clone(), masks.clone());
			let (stop, made, fps) = (stop.clone(), masks_made.clone(), mask_fps.clone());
			std::thread::Builder::new()
				.name("voelin-studio-segment".into())
				.spawn(move || segment_loop(segmenter, &frames, &out, &stop, &made, &fps))
				.ok()
		};
		Self {
			mode: Arc::new(Mutex::new(mode)),
			to_segmenter,
			masks,
			mask: None,
			workers: Workers::new("voelin-blur", 0),
			scratch: Vec::new(),
			scaler: crate::studio::compose::RgbaScaler::new(2),
			backdrop: None,
			thread,
			stop,
			masks_made,
			mask_fps,
			segmenter: name,
		}
	}

	/// A filter with the built-in [`Ellipse`] segmenter.
	pub fn with_default_segmenter(mode: Background) -> Self {
		Self::new(mode, Box::new(Ellipse::person()))
	}

	/// The mode, changed while running (a scene edit).
	pub fn set_mode(&self, mode: Background) {
		*lock(&self.mode) = mode;
	}

	pub fn mode(&self) -> Background {
		lock(&self.mode).clone()
	}

	/// How often a mask is computed.
	pub fn set_mask_fps(&self, fps: u32) {
		self.mask_fps.store(fps.max(1), Ordering::Relaxed);
	}

	pub fn masks(&self) -> u64 {
		self.masks_made.load(Ordering::Relaxed)
	}

	pub fn segmenter(&self) -> &'static str {
		self.segmenter
	}

	/// Replace the background of `frame` (packed RGBA, in place). Does
	/// nothing while the mode is [`Background::Keep`] or before the first
	/// mask.
	pub fn apply(&mut self, frame: &mut VideoFrame) -> Result<()> {
		let mode = lock(&self.mode).clone();
		if !mode.needs_mask() {
			return Ok(());
		}
		if !matches!(frame.data, FrameData::Rgba(_)) {
			return Err(Error::InvalidFrame("background replacement needs RGBA".into()));
		}
		// Offer the segmenter a small copy and take its newest mask.
		self.feed_segmenter(frame)?;
		if let Some(mask) = self.masks.take() {
			self.mask = Some(mask);
		}
		let Some(mask) = self.mask.clone() else { return Ok(()) };
		let (width, height) = (frame.width as usize, frame.height as usize);
		self.make_backdrop(&mode, frame)?;
		let FrameData::Rgba(plane) = &mut frame.data else { unreachable!("checked above") };
		let stride = plane.stride;
		let backdrop = self.backdrop.as_ref().map(|(_, b)| b);
		match backdrop {
			Some(Backdrop::Blur(radius)) => {
				let needed = stride * height;
				if self.scratch.len() < needed * 2 {
					self.scratch.resize(needed * 2, 0);
				}
				let (blurred, scratch) = self.scratch.split_at_mut(needed);
				blurred[..plane.data.len().min(needed)]
					.copy_from_slice(&plane.data[..plane.data.len().min(needed)]);
				blur_rgba(&mut self.workers, blurred, scratch, width, height, stride, *radius)?;
				mix(&mut self.workers, &mut plane.data, blurred, stride, width, height, &mask);
			}
			Some(Backdrop::Image(image)) => {
				let FrameData::Rgba(behind) = &image.data else {
					return Err(Error::InvalidFrame("the background image is not RGBA".into()));
				};
				mix(&mut self.workers, &mut plane.data, &behind.data, stride, width, height, &mask);
			}
			Some(Backdrop::Colour(rgba)) => {
				let rgba = *rgba;
				fill_behind(&mut self.workers, &mut plane.data, stride, width, height, &mask, rgba);
			}
			None => {}
		}
		Ok(())
	}

	/// Hand the segmenter a reduced copy of the frame, if it wants one.
	fn feed_segmenter(&mut self, frame: &VideoFrame) -> Result<()> {
		if self.to_segmenter.is_closed() {
			return Ok(());
		}
		let width = frame.width.clamp(1, SEGMENT_WIDTH);
		let height = (u64::from(width) * u64::from(frame.height) / u64::from(frame.width.max(1)))
			.max(1) as u32;
		let small = self.scaler.scale(frame, width, height)?;
		self.to_segmenter.put(small);
		Ok(())
	}

	/// Build (or reuse) what goes behind the subject.
	fn make_backdrop(&mut self, mode: &Background, frame: &VideoFrame) -> Result<()> {
		let stale = match (&self.backdrop, mode) {
			(Some((had, Backdrop::Image(image))), Background::Image { .. }) => {
				had != mode || image.width != frame.width || image.height != frame.height
			}
			(Some((had, _)), _) => had != mode,
			(None, _) => true,
		};
		if !stale {
			// A blur radius follows the frame height, which may have changed.
			if let (Some((_, Backdrop::Blur(radius))), Background::Blur { strength }) =
				(&mut self.backdrop, mode)
			{
				*radius = blur_radius(*strength, frame.height);
			}
			return Ok(());
		}
		let backdrop = match mode {
			Background::Keep => return Ok(()),
			Background::Blur { strength } => Backdrop::Blur(blur_radius(*strength, frame.height)),
			Background::Colour { colour } => Backdrop::Colour(opaque(*colour)),
			Background::Image { path } => {
				let image = image_frame(path)?;
				// Cover the frame, so no edge of it shows.
				let scaled = self.scaler.scale(&image, frame.width, frame.height)?;
				Backdrop::Image((*scaled).clone())
			}
		};
		self.backdrop = Some((mode.clone(), backdrop));
		Ok(())
	}
}

impl Drop for BackgroundFilter {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
		self.to_segmenter.close();
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

/// The blur radius in pixels of a strength given as a fraction of the height.
fn blur_radius(strength: f32, height: u32) -> usize {
	let radius = (strength.clamp(0.0, 1.0) * height as f32).round();
	(radius as usize).min(height as usize / 2).max(if strength > 0.0 { 1 } else { 0 })
}

fn opaque(colour: Colour) -> [u8; 4] {
	[colour.r, colour.g, colour.b, 255]
}

/// `frame = frame * mask + behind * (1 - mask)`, in bands of rows.
fn mix(
	workers: &mut Workers,
	frame: &mut [u8],
	behind: &[u8],
	stride: usize,
	width: usize,
	height: usize,
	mask: &Mask,
) {
	let tasks = workers.tasks(height);
	let band = height.div_ceil(tasks);
	let tasks = height.div_ceil(band);
	let slots = Slots::new(frame.chunks_mut(band * stride).take(tasks).enumerate());
	workers.run(slots.len(), &|task| {
		let Some((i, out)) = slots.take(task) else { return };
		let rows = i * band..((i + 1) * band).min(height);
		for y in rows.clone() {
			let line = &mut out[(y - rows.start) * stride..][..width * 4];
			let back = &behind[y * stride..][..width * 4];
			for x in 0..width {
				let m = u32::from(mask.sample(x as u32, y as u32, width as u32, height as u32));
				if m == 255 {
					continue;
				}
				for c in 0..4 {
					let front = u32::from(line[x * 4 + c]) * m;
					let rear = u32::from(back[x * 4 + c]) * (255 - m);
					line[x * 4 + c] = ((front + rear + 127) / 255) as u8;
				}
				line[x * 4 + 3] = 255;
			}
		}
	});
}

/// As [`mix`], with a flat colour behind.
fn fill_behind(
	workers: &mut Workers,
	frame: &mut [u8],
	stride: usize,
	width: usize,
	height: usize,
	mask: &Mask,
	rgba: [u8; 4],
) {
	let tasks = workers.tasks(height);
	let band = height.div_ceil(tasks);
	let tasks = height.div_ceil(band);
	let slots = Slots::new(frame.chunks_mut(band * stride).take(tasks).enumerate());
	workers.run(slots.len(), &|task| {
		let Some((i, out)) = slots.take(task) else { return };
		let rows = i * band..((i + 1) * band).min(height);
		for y in rows.clone() {
			let line = &mut out[(y - rows.start) * stride..][..width * 4];
			for x in 0..width {
				let m = u32::from(mask.sample(x as u32, y as u32, width as u32, height as u32));
				if m == 255 {
					continue;
				}
				for c in 0..4 {
					let front = u32::from(line[x * 4 + c]) * m;
					let rear = u32::from(rgba[c]) * (255 - m);
					line[x * 4 + c] = ((front + rear + 127) / 255) as u8;
				}
				line[x * 4 + 3] = 255;
			}
		}
	});
}

/// The segmenter's thread: the newest frame in, a mask out, at its own rate.
fn segment_loop(
	mut segmenter: Box<dyn Segmenter>,
	frames: &Handoff<VideoFrame>,
	masks: &Handoff<Mask>,
	stop: &std::sync::atomic::AtomicBool,
	made: &AtomicU64,
	fps: &AtomicU32,
) {
	let mut mask = Mask::default();
	let mut next = Instant::now();
	while !stop.load(Ordering::Relaxed) {
		let Some(frame) = frames.wait_timeout(Duration::from_millis(100)) else { continue };
		let now = Instant::now();
		if now < next {
			continue;
		}
		next = now + Duration::from_secs(1) / fps.load(Ordering::Relaxed).max(1);
		match segmenter.mask(&frame, &mut mask) {
			Ok(()) => {
				masks.put(Arc::new(mask.clone()));
				made.fetch_add(1, Ordering::Relaxed);
			}
			Err(e) => warn!("segmentation failed: {e}"),
		}
	}
}

/// A `width` x `height` RGBA frame of one colour.
#[cfg(test)]
fn flat_rgba(width: u32, height: u32, rgba: [u8; 4]) -> VideoFrame {
	let mut frame = VideoFrame::black_i420(width, height);
	let mut data = Vec::with_capacity(width as usize * height as usize * 4);
	for _ in 0..u64::from(width) * u64::from(height) {
		data.extend_from_slice(&rgba);
	}
	frame.data = FrameData::Rgba(crate::frame::Plane::new(data, width as usize * 4));
	frame
}

#[cfg(test)]
mod tests {
	use super::*;

	fn pixel(frame: &VideoFrame, x: u32, y: u32) -> [u8; 4] {
		let FrameData::Rgba(p) = &frame.data else { panic!("not RGBA") };
		p.row(y as usize, frame.width as usize * 4)[x as usize * 4..][..4].try_into().unwrap()
	}

	#[test]
	fn the_blur_spreads_a_dot_and_keeps_a_flat_field() {
		let mut workers = Workers::new("test-blur", 4);
		let (w, h) = (64usize, 48usize);
		let stride = w * 4;
		// A flat field stays flat (the box blur is normalised).
		let mut flat = vec![0u8; stride * h];
		for px in flat.chunks_exact_mut(4) {
			px.copy_from_slice(&[40, 80, 120, 255]);
		}
		let mut scratch = vec![0u8; stride * h];
		blur_rgba(&mut workers, &mut flat, &mut scratch, w, h, stride, 5).unwrap();
		assert!(flat.chunks_exact(4).all(|px| px[..3] == [40, 80, 120]), "a flat field changed");

		// A white block spreads and dims (a single pixel would not survive
		// three integer passes, and no real picture looks like one).
		let mut dot = vec![0u8; stride * h];
		for px in dot.chunks_exact_mut(4) {
			px[3] = 255;
		}
		for y in h / 2 - 4..h / 2 + 4 {
			for x in w / 2 - 4..w / 2 + 4 {
				dot[y * stride + x * 4..][..3].copy_from_slice(&[255, 255, 255]);
			}
		}
		blur_rgba(&mut workers, &mut dot, &mut scratch, w, h, stride, 4).unwrap();
		let at = |x: usize, y: usize| dot[y * stride + x * 4];
		assert!(at(w / 2, h / 2) > 0 && at(w / 2, h / 2) < 255, "the block did not dim");
		assert!(at(w / 2 + 6, h / 2) > 0, "the block did not spread sideways");
		assert!(at(w / 2, h / 2 + 6) > 0, "the block did not spread down");
		assert_eq!(at(0, 0), 0, "the corner should still be black");
		// Radius 0 is a no-op, and buffers that are too small are refused.
		let mut small = vec![0u8; 8];
		assert!(blur_rgba(&mut workers, &mut small, &mut scratch, w, h, stride, 1).is_err());
		blur_rgba(&mut workers, &mut small, &mut scratch, w, h, stride, 0).unwrap();
	}

	#[test]
	fn the_ellipse_covers_the_middle_and_not_the_corners() {
		let mut segmenter = Ellipse::person();
		let mut mask = Mask::default();
		segmenter.mask(&flat_rgba(64, 64, [1, 2, 3, 255]), &mut mask).unwrap();
		assert_eq!((mask.width, mask.height), (64, 64));
		assert_eq!(mask.data[35 * 64 + 32], 255, "the middle is the subject");
		assert_eq!(mask.data[0], 0, "the corner is background");
		assert_eq!(mask.data[63 * 64 + 63], 0);
		assert_eq!(segmenter.name(), "ellipse");
		// The edge fades rather than stepping.
		let edge: Vec<u8> = (0..64).map(|x| mask.data[35 * 64 + x]).collect();
		assert!(edge.iter().any(|&v| v > 0 && v < 255), "no feathering: {edge:?}");
	}

	#[test]
	fn a_colour_background_replaces_everything_but_the_subject() {
		let mut filter = BackgroundFilter::with_default_segmenter(Background::Colour {
			colour: Colour::rgb(200, 0, 0),
		});
		assert_eq!(filter.segmenter(), "ellipse");
		let mut frame = flat_rgba(64, 64, [0, 255, 0, 255]);
		// Wait for the first mask.
		let started = Instant::now();
		while filter.masks() == 0 && started.elapsed() < Duration::from_secs(5) {
			filter.apply(&mut frame.clone()).unwrap();
			std::thread::sleep(Duration::from_millis(10));
		}
		assert!(filter.masks() > 0, "the segmenter produced no mask");
		// Give the handoff a moment, then apply for real.
		for _ in 0..10 {
			filter.apply(&mut frame).unwrap();
			if pixel(&frame, 0, 0)[0] > 100 {
				break;
			}
			std::thread::sleep(Duration::from_millis(20));
		}
		assert_eq!(pixel(&frame, 32, 35), [0, 255, 0, 255], "the subject is untouched");
		assert_eq!(pixel(&frame, 0, 0), [200, 0, 0, 255], "the corner became the colour");

		// Keep does nothing at all.
		filter.set_mode(Background::Keep);
		let mut green = flat_rgba(8, 8, [0, 255, 0, 255]);
		filter.apply(&mut green).unwrap();
		assert_eq!(pixel(&green, 0, 0), [0, 255, 0, 255]);
		assert_eq!(filter.mode(), Background::Keep);
		// Keep does not even look at the pixels, so I420 passes through.
		filter.apply(&mut VideoFrame::black_i420(8, 8)).unwrap();
		// Any other mode needs RGBA and says so.
		filter.set_mode(Background::Blur { strength: 0.05 });
		assert!(filter.apply(&mut VideoFrame::black_i420(8, 8)).is_err());
	}

	#[test]
	fn an_image_background_shows_behind_the_subject() {
		let dir = std::env::temp_dir().join(format!("voelin-backdrop-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("behind.png");
		image::RgbaImage::from_pixel(16, 16, image::Rgba([0, 0, 220, 255])).save(&path).unwrap();
		let mut filter =
			BackgroundFilter::with_default_segmenter(Background::Image { path: path.clone() });
		let started = Instant::now();
		let mut frame = flat_rgba(64, 64, [0, 255, 0, 255]);
		while pixel(&frame, 0, 0) != [0, 0, 220, 255] {
			assert!(started.elapsed() < Duration::from_secs(5), "the image never showed");
			frame = flat_rgba(64, 64, [0, 255, 0, 255]);
			filter.apply(&mut frame).unwrap();
			std::thread::sleep(Duration::from_millis(10));
		}
		assert_eq!(pixel(&frame, 32, 35), [0, 255, 0, 255], "the subject is untouched");
		std::fs::remove_dir_all(&dir).ok();
	}

	#[test]
	fn a_blurred_background_keeps_the_subject_sharp() {
		let mut filter =
			BackgroundFilter::with_default_segmenter(Background::Blur { strength: 0.1 });
		filter.set_mask_fps(60);
		// A checkerboard, so blurring is visible.
		let mut frame = flat_rgba(64, 64, [0, 0, 0, 255]);
		{
			let FrameData::Rgba(p) = &mut frame.data else { panic!() };
			for y in 0..64usize {
				for x in 0..64usize {
					if (x / 2 + y / 2) % 2 == 0 {
						p.data[y * 256 + x * 4..][..3].copy_from_slice(&[255, 255, 255]);
					}
				}
			}
		}
		let original = frame.clone();
		let started = Instant::now();
		loop {
			filter.apply(&mut frame).unwrap();
			if filter.masks() > 0 && pixel(&frame, 0, 0) != pixel(&original, 0, 0) {
				break;
			}
			assert!(started.elapsed() < Duration::from_secs(10), "the blur never ran");
			std::thread::sleep(Duration::from_millis(20));
			frame = original.clone();
		}
		// The corner is background: blurred towards grey.
		let corner = pixel(&frame, 1, 1);
		assert!((60..200).contains(&corner[0]), "the corner is not blurred: {corner:?}");
		// The middle is the subject: still full contrast between neighbours.
		let a = pixel(&frame, 32, 35)[0];
		let b = pixel(&frame, 34, 35)[0];
		assert!(a.abs_diff(b) > 100, "the subject lost its contrast: {a} vs {b}");
		assert!(blur_radius(0.1, 720) > 1);
		assert_eq!(blur_radius(0.0, 720), 0);
	}
}
