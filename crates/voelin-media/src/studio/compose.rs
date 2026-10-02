//! Compositing a scene into one picture.
//!
//! Each source runs on its own thread and publishes its newest picture into a
//! [`Feed`] (a one-slot latest-wins [`Handoff`]); the compositor takes the
//! newest one of every source and draws them back to front into a pooled
//! RGBA frame at the output size. A source that has not delivered a new
//! picture keeps the one it had, so a 5 fps camera does not blink in a 60 fps
//! composite.
//!
//! The plan (which source rectangle goes where, and the filter weights for
//! it) is rebuilt only when the scene, a source size or the output size
//! changes, and the canvas comes from a [`FramePool`], so a steady composite
//! allocates nothing: [`Compositor::compose`] is on the video path.
//!
//! Drawing is split into bands of output rows on a [`Workers`] pool; every
//! band draws every source that reaches into it, in scene order, so the
//! result is the same as drawing the whole canvas source by source.
//!
//! Scaling uses the same separable fixed-point filter as [`crate::scale`]
//! (area averaging down, bilinear up) with an inner loop for interleaved
//! RGBA, and a straight copy at 1:1. Sources with an alpha channel (images
//! and text) and sources below full opacity are blended over what is
//! already there.

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::frame::{FrameData, PixelFormat, PlaneRef, VideoFrame};
use crate::handoff::Handoff;
use crate::pool::FramePool;
use crate::scale::{Axis, MID_BITS};
use crate::studio::scene::{Fit, Scene, SourceKind};
use crate::workers::{Slots, Workers};
use crate::{Error, Result};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the compositor draws one source from: its newest picture, plus what
/// the source thread wants to report.
///
/// Sources publish BGRA or RGBA frames (that is what compositing needs);
/// [`crate::convert::to_rgba`] turns a camera's YUV into one.
#[derive(Default)]
pub struct Feed {
	frames: Handoff<VideoFrame>,
	delivered: AtomicU64,
	/// `width << 32 | height` of the last frame.
	size: AtomicU64,
	/// Why the source shows nothing (no such camera, the window closed, a
	/// file that cannot be read, ...).
	error: Mutex<Option<String>>,
}

impl Feed {
	pub fn new() -> Self {
		Self::default()
	}

	/// Offer the newest picture; an older one the compositor has not taken is
	/// replaced.
	pub fn put(&self, frame: Arc<VideoFrame>) {
		self.size.store(u64::from(frame.width) << 32 | u64::from(frame.height), Ordering::Relaxed);
		self.delivered.fetch_add(1, Ordering::Relaxed);
		self.frames.put(frame);
	}

	/// The newest picture, if one arrived since the last call.
	pub fn take(&self) -> Option<Arc<VideoFrame>> {
		self.frames.take()
	}

	/// Frames the source delivered.
	pub fn delivered(&self) -> u64 {
		self.delivered.load(Ordering::Relaxed)
	}

	/// Frames replaced before the compositor took them (the source is faster
	/// than the composite).
	pub fn dropped(&self) -> u64 {
		self.frames.replaced()
	}

	/// Size of the last frame, or `(0, 0)`.
	pub fn size(&self) -> (u32, u32) {
		let size = self.size.load(Ordering::Relaxed);
		((size >> 32) as u32, size as u32)
	}

	pub fn set_error(&self, error: impl ToString) {
		*lock(&self.error) = Some(error.to_string());
	}

	pub fn clear_error(&self) {
		*lock(&self.error) = None;
	}

	pub fn error(&self) -> Option<String> {
		lock(&self.error).clone()
	}

	/// Tell the source thread to stop.
	pub fn close(&self) {
		self.frames.close();
	}

	pub fn is_closed(&self) -> bool {
		self.frames.is_closed()
	}
}

/// `v / 255`, rounded.
#[inline]
fn div255(v: u32) -> u8 {
	let t = v + 128;
	((t + (t >> 8)) >> 8) as u8
}

/// Write one RGB pixel with coverage `a` (0..=255) over `dst`.
#[inline]
fn put_pixel(dst: &mut [u8], rgb: [u8; 3], a: u32) {
	if a == 255 {
		dst[..3].copy_from_slice(&rgb);
	} else if a > 0 {
		for c in 0..3 {
			dst[c] = div255(u32::from(rgb[c]) * a + u32::from(dst[c]) * (255 - a));
		}
	}
	// The canvas is opaque: it becomes I420 for the encoders.
	dst[3] = 255;
}

/// One source rectangle mapped onto the canvas, with its filter weights.
struct Blit {
	/// Which of the scene's sources this draws.
	source: usize,
	/// Source rectangle after the crop, in source pixels.
	sx: usize,
	sy: usize,
	sw: usize,
	sh: usize,
	/// Where the unclipped destination starts on the canvas (may be
	/// negative).
	dx: isize,
	dy: isize,
	/// Size of the unclipped destination.
	dw: usize,
	dh: usize,
	/// Destination columns and rows on the canvas, in destination
	/// coordinates.
	cols: Range<usize>,
	rows: Range<usize>,
	x: Axis,
	y: Axis,
	/// Same size: copy rows instead of filtering.
	direct: bool,
	/// The source's bytes are B, G, R, A.
	bgra: bool,
	/// The source's alpha channel means something (images, text).
	alpha: bool,
	/// 0..=255.
	opacity: u8,
	/// Mirror horizontally (front cameras).
	mirror: bool,
}

impl Blit {
	/// Destination rows of this blit inside canvas rows `band`.
	fn band_rows(&self, band: &Range<usize>) -> Range<usize> {
		let first = (band.start as isize - self.dy).max(self.rows.start as isize) as usize;
		let last = (band.end as isize - self.dy).clamp(0, self.rows.end as isize) as usize;
		first..last.max(first)
	}

	/// Draw the rows of this blit that fall in canvas rows `band`. `out`
	/// starts at canvas row `band.start`; `tmp` is scratch for one
	/// vertically filtered source row.
	fn draw(
		&self,
		src: PlaneRef<'_>,
		out: &mut [u8],
		out_stride: usize,
		band: &Range<usize>,
		tmp: &mut [u16],
	) {
		let rows = self.band_rows(band);
		if rows.is_empty() || self.cols.is_empty() {
			return;
		}
		let opaque = self.opacity == 255 && !self.alpha;
		for r in rows {
			let line = ((self.dy + r as isize) as usize - band.start) * out_stride;
			let out_x = (self.dx + self.cols.start as isize) as usize * 4;
			let line = &mut out[line + out_x..][..self.cols.len() * 4];
			if self.direct {
				self.copy_row(src, line, r, opaque);
			} else {
				self.filter_row(src, line, r, opaque, tmp);
			}
		}
	}

	/// One destination row at 1:1.
	fn copy_row(&self, src: PlaneRef<'_>, line: &mut [u8], r: usize, opaque: bool) {
		let row = src.row(self.sy + r, (self.sx + self.sw) * 4);
		let first = if self.mirror { self.dw - self.cols.end } else { self.cols.start };
		let row = &row[(self.sx + first) * 4..][..self.cols.len() * 4];
		if opaque && !self.bgra && !self.mirror {
			line.copy_from_slice(row);
			for px in line.chunks_exact_mut(4) {
				px[3] = 255;
			}
			return;
		}
		let pixels = row.chunks_exact(4);
		// Mirrored: the destination's first column is the source's last.
		let opacity = u32::from(self.opacity);
		let write = |px: &[u8], dst: &mut [u8]| {
			let rgb = if self.bgra { [px[2], px[1], px[0]] } else { [px[0], px[1], px[2]] };
			let a = if self.alpha { opacity * u32::from(px[3]) / 255 } else { opacity };
			put_pixel(dst, rgb, a);
		};
		if self.mirror {
			for (px, dst) in pixels.rev().zip(line.chunks_exact_mut(4)) {
				write(px, dst);
			}
		} else {
			for (px, dst) in pixels.zip(line.chunks_exact_mut(4)) {
				write(px, dst);
			}
		}
	}

	/// One destination row through the separable filter: a vertical pass into
	/// `tmp` (with [`MID_BITS`] fractional bits), then a horizontal one.
	fn filter_row(
		&self,
		src: PlaneRef<'_>,
		line: &mut [u8],
		r: usize,
		opaque: bool,
		tmp: &mut [u16],
	) {
		let bytes = self.sw * 4;
		let (start, weights) = self.y.get(r);
		const CHUNK: usize = 64;
		let mut acc = [0u32; CHUNK];
		let mut x0 = 0;
		while x0 < bytes {
			let n = CHUNK.min(bytes - x0);
			let acc = &mut acc[..n];
			acc.fill(0);
			for (k, &w) in weights.iter().enumerate() {
				let row = src.row(self.sy + start + k, (self.sx + self.sw) * 4);
				for (a, &p) in acc.iter_mut().zip(&row[self.sx * 4 + x0..][..n]) {
					*a += w * u32::from(p);
				}
			}
			for (t, &a) in tmp[x0..x0 + n].iter_mut().zip(acc.iter()) {
				*t = ((a + (1 << (13 - MID_BITS))) >> (14 - MID_BITS)) as u16;
			}
			x0 += n;
		}
		let opacity = u32::from(self.opacity);
		for (i, dst) in line.chunks_exact_mut(4).enumerate() {
			let ox = self.cols.start + i;
			let (start, weights) = self.x.get(if self.mirror { self.dw - 1 - ox } else { ox });
			let mut px = [0u32; 4];
			for (k, &w) in weights.iter().enumerate() {
				let tap = &tmp[(start + k) * 4..][..4];
				for (p, &t) in px.iter_mut().zip(tap) {
					*p += w * u32::from(t);
				}
			}
			const ROUND: u32 = 1 << (13 + MID_BITS);
			let s = px.map(|p| ((p + ROUND) >> (14 + MID_BITS)).min(255) as u8);
			let rgb = if self.bgra { [s[2], s[1], s[0]] } else { [s[0], s[1], s[2]] };
			let a = if opaque {
				255
			} else if self.alpha {
				opacity * u32::from(s[3]) / 255
			} else {
				opacity
			};
			put_pixel(dst, rgb, a);
		}
	}
}

/// How the compositor is doing. Read through [`Compositor::stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ComposeStats {
	pub frames: u64,
	/// Mean time to composite a frame.
	pub compose_time: Duration,
	/// Times the plan was rebuilt (a scene change, a source that resized).
	pub replans: u64,
	/// Canvases the pool had to allocate.
	pub allocated: u64,
	pub threads: usize,
}

/// Composites a scene into pooled RGBA frames; see the [module docs](self).
pub struct Compositor {
	workers: Workers,
	pool: FramePool,
	size: (u32, u32),
	/// Each scene source's newest picture (an empty feed keeps the last one).
	held: Vec<Option<Arc<VideoFrame>>>,
	/// The source ids `held` belongs to.
	held_ids: Vec<u64>,
	blits: Vec<Blit>,
	/// One vertically filtered source row per band.
	scratch: Vec<Mutex<Vec<u16>>>,
	/// The background colour as one output row, memcpy'd per row.
	background: Vec<u8>,
	/// What the plan was made for.
	planned: Option<(u64, (u32, u32))>,
	planned_sizes: Vec<(u32, u32)>,
	frames: u64,
	compose_ns: u64,
	replans: u64,
}

impl Compositor {
	/// `threads` as in [`Workers::new`] (0: one per CPU).
	pub fn new(threads: usize) -> Self {
		Self {
			workers: Workers::new("voelin-compose", threads),
			pool: FramePool::new(),
			size: (0, 0),
			held: Vec::new(),
			held_ids: Vec::new(),
			blits: Vec::new(),
			scratch: Vec::new(),
			background: Vec::new(),
			planned: None,
			planned_sizes: Vec::new(),
			frames: 0,
			compose_ns: 0,
			replans: 0,
		}
	}

	/// Output size of the composite; at least 2x2 and even.
	pub fn set_size(&mut self, width: u32, height: u32) {
		self.size = (width.max(2) & !1, height.max(2) & !1);
	}

	pub fn size(&self) -> (u32, u32) {
		self.size
	}

	pub fn stats(&self) -> ComposeStats {
		ComposeStats {
			frames: self.frames,
			compose_time: match self.frames {
				0 => Duration::ZERO,
				n => Duration::from_nanos(self.compose_ns / n),
			},
			replans: self.replans,
			allocated: self.pool.allocated(),
			threads: self.workers.threads(),
		}
	}

	/// Drop every picture held from an earlier scene.
	pub fn reset(&mut self) {
		self.held.clear();
		self.held_ids.clear();
		self.planned = None;
	}

	/// Composite `scene` into a pooled RGBA frame stamped `timestamp`.
	///
	/// `feeds[i]` is the live input of `scene.sources[i]` (`None`: not
	/// running, drawn as nothing). `revision` must change whenever `scene`
	/// does; the plan and its filter weights are kept while it and the source
	/// sizes stay the same.
	pub fn compose(
		&mut self,
		scene: &Scene,
		revision: u64,
		feeds: &[Option<Arc<Feed>>],
		timestamp: Duration,
	) -> Result<Arc<VideoFrame>> {
		let (width, height) = self.size;
		if width == 0 || height == 0 {
			return Err(Error::InvalidFrame("the studio output size is not set".into()));
		}
		let started = Instant::now();
		self.collect(scene, feeds);
		if self.planned != Some((revision, self.size))
			|| self.planned_sizes.len() != scene.sources.len()
			|| self.held.iter().zip(&self.planned_sizes).any(|(held, planned)| {
				held.as_ref().map_or((0, 0), |f| (f.width, f.height)) != *planned
			}) {
			self.plan(scene, revision);
		}
		let canvas = self.pool.get_format(width, height, PixelFormat::Rgba);
		let frame = Arc::get_mut(canvas).expect("the pool hands out unshared frames");
		frame.timestamp = timestamp;
		let FrameData::Rgba(plane) = &mut frame.data else {
			return Err(Error::InvalidFrame("the studio canvas is not RGBA".into()));
		};
		let stride = plane.stride;
		let tasks = self.workers.tasks(height as usize);
		let band = (height as usize).div_ceil(tasks);
		let tasks = (height as usize).div_ceil(band);
		while self.scratch.len() < tasks {
			self.scratch.push(Mutex::new(Vec::new()));
		}
		let longest = self.blits.iter().map(|b| b.sw * 4).max().unwrap_or(0);
		for slot in &mut self.scratch {
			let tmp = slot.get_mut().unwrap_or_else(PoisonError::into_inner);
			if tmp.len() < longest {
				tmp.resize(longest, 0);
			}
		}
		let slots = Slots::new(plane.data.chunks_mut(band * stride).take(tasks).enumerate());
		let (blits, held, background, scratch) =
			(&self.blits, &self.held, &self.background, &self.scratch);
		self.workers.run(slots.len(), &|task| {
			let Some((i, out)) = slots.take(task) else { return };
			let rows = i * band..((i + 1) * band).min(height as usize);
			for row in 0..rows.len() {
				out[row * stride..][..background.len()].copy_from_slice(background);
			}
			let mut tmp = lock(&scratch[i]);
			for blit in blits {
				let Some(frame) = held[blit.source].as_ref() else { continue };
				let plane = match &frame.data {
					FrameData::Bgra(p) | FrameData::Rgba(p) => p.view(),
					// The plan only makes blits for packed sources.
					_ => continue,
				};
				blit.draw(plane, out, stride, &rows, &mut tmp);
			}
		});
		self.frames += 1;
		self.compose_ns += started.elapsed().as_nanos() as u64;
		Ok(canvas.clone())
	}

	/// Take each source's newest picture, keeping the last one of a source
	/// that has nothing new.
	fn collect(&mut self, scene: &Scene, feeds: &[Option<Arc<Feed>>]) {
		if self.held_ids.len() != scene.sources.len()
			|| self.held_ids.iter().ne(scene.sources.iter().map(|s| &s.id))
		{
			// The scene's sources changed: keep each source's picture by id.
			let old: Vec<(u64, Option<Arc<VideoFrame>>)> =
				self.held_ids.drain(..).zip(self.held.drain(..)).collect();
			for source in &scene.sources {
				self.held_ids.push(source.id);
				let kept = old.iter().find(|(id, _)| *id == source.id);
				self.held.push(kept.and_then(|(_, frame)| frame.clone()));
			}
		}
		for (i, source) in scene.sources.iter().enumerate() {
			// No live input, or not drawn: nothing, not even what it had.
			let Some(Some(feed)) = feeds.get(i).filter(|_| source.drawn()) else {
				self.held[i] = None;
				continue;
			};
			if let Some(frame) = feed.take() {
				self.held[i] = Some(frame);
			}
		}
	}

	/// Work out where every source goes and the filter weights for it.
	fn plan(&mut self, scene: &Scene, revision: u64) {
		let (width, height) = (self.size.0 as usize, self.size.1 as usize);
		self.blits.clear();
		self.planned_sizes.clear();
		self.planned = Some((revision, self.size));
		self.replans += 1;
		let rgba = scene.background.to_rgba();
		self.background.clear();
		self.background.reserve(width * 4);
		for _ in 0..width {
			self.background.extend_from_slice(&[rgba[0], rgba[1], rgba[2], 255]);
		}
		for (i, source) in scene.sources.iter().enumerate() {
			let size = self.held[i].as_ref().map_or((0, 0), |f| (f.width, f.height));
			self.planned_sizes.push(size);
			let Some(frame) = &self.held[i] else { continue };
			if !matches!(frame.data, FrameData::Bgra(_) | FrameData::Rgba(_)) {
				continue;
			}
			let (cx, cy, mut cw, mut ch) = source.crop.apply(size.0, size.1);
			let (mut sx, mut sy) = (cx as usize, cy as usize);
			let t = &source.transform;
			// The box the source is drawn into.
			let bw = t.width.unwrap_or(cw as f32 * t.scale).max(1.0);
			let bh = t.height.unwrap_or(ch as f32 * t.scale).max(1.0);
			let (mut dx, mut dy) = (t.x, t.y);
			let (mut dw, mut dh) = (bw, bh);
			match t.fit {
				Fit::Stretch => {}
				Fit::Contain => {
					// The whole source inside the box, centred.
					let s = (bw / cw as f32).min(bh / ch as f32);
					dw = (cw as f32 * s).max(1.0);
					dh = (ch as f32 * s).max(1.0);
					dx += (bw - dw) / 2.0;
					dy += (bh - dh) / 2.0;
				}
				Fit::Cover => {
					// Fill the box: crop the source to the box's aspect.
					let s = (bw / cw as f32).max(bh / ch as f32);
					let (kw, kh) = ((bw / s).round() as u32, (bh / s).round() as u32);
					let (kw, kh) = (kw.clamp(1, cw), kh.clamp(1, ch));
					sx += ((cw - kw) / 2) as usize;
					sy += ((ch - kh) / 2) as usize;
					(cw, ch) = (kw, kh);
				}
			}
			let (dw, dh) = (dw.round().max(1.0) as usize, dh.round().max(1.0) as usize);
			let (dx, dy) = (dx.round() as isize, dy.round() as isize);
			// Clip to the canvas, in destination coordinates.
			let cols = clip(dx, dw, width);
			let rows = clip(dy, dh, height);
			if cols.is_empty() || rows.is_empty() {
				continue;
			}
			let (sw, sh) = (cw as usize, ch as usize);
			let mirror = matches!(source.kind, SourceKind::Camera { mirror: true, .. });
			self.blits.push(Blit {
				source: i,
				sx,
				sy,
				sw,
				sh,
				dx,
				dy,
				dw,
				dh,
				cols,
				rows,
				x: if sw == dw { Axis::default() } else { Axis::new(sw, dw) },
				y: if sh == dh { Axis::default() } else { Axis::new(sh, dh) },
				direct: sw == dw && sh == dh,
				bgra: matches!(frame.data, FrameData::Bgra(_)),
				alpha: matches!(source.kind, SourceKind::Image { .. } | SourceKind::Text { .. }),
				opacity: (source.opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
				mirror,
			});
		}
	}
}

/// The part of a `len`-wide destination starting at `at` that lies within
/// `0 .. limit`, in destination coordinates.
fn clip(at: isize, len: usize, limit: usize) -> Range<usize> {
	let start = (-at).clamp(0, len as isize) as usize;
	let end = (limit as isize - at).clamp(0, len as isize) as usize;
	start..end.max(start)
}

/// Resizes packed RGBA / BGRA images into RGBA, with the filter of
/// [`Compositor`]. Used for the studio's preview tap; weights are computed
/// once per size pair.
pub struct RgbaScaler {
	workers: Workers,
	pool: FramePool,
	blit: Option<Blit>,
	scratch: Vec<Mutex<Vec<u16>>>,
}

impl RgbaScaler {
	/// `threads` as in [`Workers::new`] (0: one per CPU).
	pub fn new(threads: usize) -> Self {
		Self {
			workers: Workers::new("voelin-rgba-scale", threads),
			pool: FramePool::new(),
			blit: None,
			scratch: Vec::new(),
		}
	}

	/// `frame` (BGRA or RGBA) as a pooled RGBA frame of `width` x `height`,
	/// keeping its timestamp. Allocates only when a size changes or every
	/// pooled frame is in use.
	pub fn scale(
		&mut self,
		frame: &VideoFrame,
		width: u32,
		height: u32,
	) -> Result<Arc<VideoFrame>> {
		let (width, height) = (width.max(1), height.max(1));
		let plane = match &frame.data {
			FrameData::Bgra(p) | FrameData::Rgba(p) => p.view(),
			_ => return Err(Error::InvalidFrame("the preview scaler needs BGRA or RGBA".into())),
		};
		let (sw, sh) = (frame.width as usize, frame.height as usize);
		let (dw, dh) = (width as usize, height as usize);
		let bgra = matches!(frame.data, FrameData::Bgra(_));
		let stale = self
			.blit
			.as_ref()
			.is_none_or(|b| (b.sw, b.sh, b.dw, b.dh, b.bgra) != (sw, sh, dw, dh, bgra));
		if stale {
			self.blit = Some(Blit {
				source: 0,
				sx: 0,
				sy: 0,
				sw,
				sh,
				dx: 0,
				dy: 0,
				dw,
				dh,
				cols: 0..dw,
				rows: 0..dh,
				x: if sw == dw { Axis::default() } else { Axis::new(sw, dw) },
				y: if sh == dh { Axis::default() } else { Axis::new(sh, dh) },
				direct: sw == dw && sh == dh,
				bgra,
				alpha: false,
				opacity: 255,
				mirror: false,
			});
		}
		let blit = self.blit.as_ref().expect("just set");
		let out = self.pool.get_format(width, height, PixelFormat::Rgba);
		let target = Arc::get_mut(out).expect("the pool hands out unshared frames");
		target.timestamp = frame.timestamp;
		let FrameData::Rgba(dst) = &mut target.data else {
			return Err(Error::InvalidFrame("the preview canvas is not RGBA".into()));
		};
		let stride = dst.stride;
		let tasks = self.workers.tasks(dh);
		let band = dh.div_ceil(tasks);
		let tasks = dh.div_ceil(band);
		while self.scratch.len() < tasks {
			self.scratch.push(Mutex::new(Vec::new()));
		}
		for slot in &mut self.scratch {
			let tmp = slot.get_mut().unwrap_or_else(PoisonError::into_inner);
			if tmp.len() < sw * 4 {
				tmp.resize(sw * 4, 0);
			}
		}
		let slots = Slots::new(dst.data.chunks_mut(band * stride).take(tasks).enumerate());
		let scratch = &self.scratch;
		self.workers.run(slots.len(), &|task| {
			let Some((i, out)) = slots.take(task) else { return };
			let rows = i * band..((i + 1) * band).min(dh);
			blit.draw(plane, out, stride, &rows, &mut lock(&scratch[i]));
		});
		Ok(out.clone())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::frame::Plane;
	use crate::studio::scene::{Colour, Crop, Source, Transform};

	/// A flat `width` x `height` RGBA frame.
	fn flat(width: u32, height: u32, rgba: [u8; 4]) -> Arc<VideoFrame> {
		let mut data = Vec::with_capacity(width as usize * height as usize * 4);
		for _ in 0..width * height {
			data.extend_from_slice(&rgba);
		}
		Arc::new(VideoFrame {
			width,
			height,
			timestamp: Duration::ZERO,
			data: FrameData::Rgba(Plane::new(data, width as usize * 4)),
		})
	}

	/// A BGRA frame whose pixel `(x, y)` is `f(x, y)` as RGBA.
	fn painted(
		width: u32,
		height: u32,
		bgra: bool,
		f: impl Fn(u32, u32) -> [u8; 4],
	) -> Arc<VideoFrame> {
		let mut data = Vec::with_capacity(width as usize * height as usize * 4);
		for y in 0..height {
			for x in 0..width {
				let p = f(x, y);
				data.extend_from_slice(&if bgra { [p[2], p[1], p[0], p[3]] } else { p });
			}
		}
		let plane = Plane::new(data, width as usize * 4);
		Arc::new(VideoFrame {
			width,
			height,
			timestamp: Duration::ZERO,
			data: if bgra { FrameData::Bgra(plane) } else { FrameData::Rgba(plane) },
		})
	}

	fn pixel(frame: &VideoFrame, x: u32, y: u32) -> [u8; 4] {
		let FrameData::Rgba(p) = &frame.data else { panic!("not RGBA") };
		let row = p.row(y as usize, frame.width as usize * 4);
		row[x as usize * 4..][..4].try_into().unwrap()
	}

	/// Composite a scene whose sources are fed `frames` (one per source).
	fn compose(
		compositor: &mut Compositor,
		scene: &Scene,
		revision: u64,
		frames: &[Option<Arc<VideoFrame>>],
	) -> Arc<VideoFrame> {
		let feeds: Vec<Option<Arc<Feed>>> = frames
			.iter()
			.map(|frame| {
				let feed = Arc::new(Feed::new());
				if let Some(frame) = frame {
					feed.put(frame.clone());
				}
				Some(feed)
			})
			.collect();
		compositor.compose(scene, revision, &feeds, Duration::from_millis(1)).unwrap()
	}

	#[test]
	fn draws_sources_where_they_belong() {
		let mut compositor = Compositor::new(4);
		compositor.set_size(64, 32);
		let mut scene = Scene::new(1, "t");
		scene.background = Colour::rgb(10, 20, 30);
		// Full-size red background source and a green box at (16, 8) 8x4.
		scene.sources.push(Source {
			transform: Transform::full(64, 32),
			..Source::new(1, SourceKind::Colour { colour: Colour::WHITE, size: (64, 32) })
		});
		scene.sources.push(Source {
			transform: Transform::box_at(16.0, 8.0, 8.0, 4.0),
			..Source::new(2, SourceKind::Colour { colour: Colour::WHITE, size: (8, 4) })
		});
		let red = flat(64, 32, [200, 0, 0, 255]);
		let green = flat(8, 4, [0, 200, 0, 255]);
		let out = compose(&mut compositor, &scene, 1, &[Some(red), Some(green)]);
		assert_eq!((out.width, out.height), (64, 32));
		assert_eq!(out.timestamp, Duration::from_millis(1));
		assert_eq!(pixel(&out, 0, 0), [200, 0, 0, 255], "the full-size source");
		assert_eq!(pixel(&out, 16, 8), [0, 200, 0, 255], "the box's first pixel");
		assert_eq!(pixel(&out, 23, 11), [0, 200, 0, 255], "the box's last pixel");
		assert_eq!(pixel(&out, 24, 8), [200, 0, 0, 255], "one past the box");
		assert_eq!(pixel(&out, 16, 12), [200, 0, 0, 255], "one below the box");

		// Without the background source the scene's background shows.
		scene.sources.remove(0);
		let green = flat(8, 4, [0, 200, 0, 255]);
		let out = compose(&mut compositor, &scene, 2, &[Some(green)]);
		assert_eq!(pixel(&out, 0, 0), [10, 20, 30, 255]);
		assert_eq!(pixel(&out, 16, 8), [0, 200, 0, 255]);
	}

	#[test]
	fn blends_alpha_and_opacity() {
		let mut compositor = Compositor::new(2);
		compositor.set_size(8, 4);
		let mut scene = Scene::new(1, "t");
		scene.background = Colour::rgb(0, 0, 0);
		// A half-transparent white image over black, and a fully opaque one
		// at half opacity.
		scene.sources.push(Source {
			transform: Transform::box_at(0.0, 0.0, 4.0, 4.0),
			..Source::new(1, SourceKind::Image { path: "x.png".into() })
		});
		scene.sources.push(Source {
			transform: Transform::box_at(4.0, 0.0, 4.0, 4.0),
			opacity: 0.5,
			..Source::new(2, SourceKind::Colour { colour: Colour::WHITE, size: (4, 4) })
		});
		let half = flat(4, 4, [255, 255, 255, 128]);
		let solid = flat(4, 4, [255, 255, 255, 255]);
		let out = compose(&mut compositor, &scene, 1, &[Some(half), Some(solid)]);
		// 255 * 128/255 = 128 over black.
		assert_eq!(pixel(&out, 0, 0), [128, 128, 128, 255], "image alpha");
		// Opacity 0.5 -> 128/255 coverage.
		assert_eq!(pixel(&out, 4, 0), [128, 128, 128, 255], "source opacity");

		// An invisible source and a zero-opacity one draw nothing.
		scene.sources[0].visible = false;
		scene.sources[1].opacity = 0.0;
		let out = compose(
			&mut compositor,
			&scene,
			2,
			&[Some(flat(4, 4, [255, 255, 255, 128])), Some(flat(4, 4, [255, 255, 255, 255]))],
		);
		assert_eq!(pixel(&out, 0, 0), [0, 0, 0, 255]);
		assert_eq!(pixel(&out, 4, 0), [0, 0, 0, 255]);
	}

	#[test]
	fn scales_crops_clips_and_swaps_channels() {
		let mut compositor = Compositor::new(4);
		compositor.set_size(16, 16);
		let mut scene = Scene::new(1, "t");
		// A 4x4 source of four quadrant colours, drawn at 16x16: each
		// quadrant is 8x8 in the output.
		scene.sources.push(Source {
			transform: Transform { fit: Fit::Stretch, ..Transform::full(16, 16) },
			..Source::new(1, SourceKind::Colour { colour: Colour::WHITE, size: (4, 4) })
		});
		let quad = painted(4, 4, true, |x, y| match (x < 2, y < 2) {
			(true, true) => [255, 0, 0, 255],
			(false, true) => [0, 255, 0, 255],
			(true, false) => [0, 0, 255, 255],
			(false, false) => [255, 255, 0, 255],
		});
		let out = compose(&mut compositor, &scene, 1, &[Some(quad.clone())]);
		// BGRA input came out as RGBA, upscaled.
		assert_eq!(pixel(&out, 1, 1), [255, 0, 0, 255]);
		assert_eq!(pixel(&out, 14, 1), [0, 255, 0, 255]);
		assert_eq!(pixel(&out, 1, 14), [0, 0, 255, 255]);
		assert_eq!(pixel(&out, 14, 14), [255, 255, 0, 255]);

		// Crop to the bottom-right quadrant: it fills the output.
		scene.sources[0].crop = Crop { left: 2, top: 2, right: 0, bottom: 0 };
		let out = compose(&mut compositor, &scene, 2, &[Some(quad.clone())]);
		assert_eq!(pixel(&out, 0, 0), [255, 255, 0, 255]);
		assert_eq!(pixel(&out, 15, 15), [255, 255, 0, 255]);

		// Half off the left edge: only the right half of the source shows.
		scene.sources[0].crop = Crop::default();
		scene.sources[0].transform =
			Transform { fit: Fit::Stretch, ..Transform::box_at(-8.0, 0.0, 16.0, 16.0) };
		let out = compose(&mut compositor, &scene, 3, &[Some(quad)]);
		// Output column 4 is the source's column 2.6: the right half.
		assert_eq!(pixel(&out, 4, 1), [0, 255, 0, 255], "the source's right half");
		assert_eq!(pixel(&out, 7, 14), [255, 255, 0, 255]);
		assert_eq!(pixel(&out, 8, 1), [0, 0, 0, 255], "past the source");

		// Entirely outside: nothing is drawn.
		scene.sources[0].transform = Transform::box_at(100.0, 100.0, 4.0, 4.0);
		let out = compose(&mut compositor, &scene, 4, &[Some(flat(4, 4, [9, 9, 9, 255]))]);
		assert_eq!(pixel(&out, 0, 0), [0, 0, 0, 255]);
	}

	#[test]
	fn fit_letterboxes_and_covers() {
		let mut compositor = Compositor::new(2);
		compositor.set_size(16, 16);
		let mut scene = Scene::new(1, "t");
		// A 16x8 source in a 16x16 box.
		scene.sources.push(Source {
			transform: Transform { fit: Fit::Contain, ..Transform::full(16, 16) },
			..Source::new(1, SourceKind::Colour { colour: Colour::WHITE, size: (16, 8) })
		});
		let wide =
			painted(16, 8, false, |_, y| if y < 4 { [255, 0, 0, 255] } else { [0, 0, 255, 255] });
		let out = compose(&mut compositor, &scene, 1, &[Some(wide.clone())]);
		// Contain: 16x8 centred, rows 4..12 drawn, the rest background.
		assert_eq!(pixel(&out, 0, 0), [0, 0, 0, 255], "letterbox above");
		assert_eq!(pixel(&out, 0, 4), [255, 0, 0, 255]);
		assert_eq!(pixel(&out, 0, 11), [0, 0, 255, 255]);
		assert_eq!(pixel(&out, 0, 12), [0, 0, 0, 255], "letterbox below");

		// Cover: the box is filled, the source's sides are cropped.
		scene.sources[0].transform = Transform { fit: Fit::Cover, ..Transform::full(16, 16) };
		let out = compose(&mut compositor, &scene, 2, &[Some(wide)]);
		assert_eq!(pixel(&out, 0, 0), [255, 0, 0, 255], "no letterbox");
		assert_eq!(pixel(&out, 15, 15), [0, 0, 255, 255]);
	}

	#[test]
	fn a_source_keeps_its_last_picture() {
		let mut compositor = Compositor::new(2);
		compositor.set_size(4, 4);
		let mut scene = Scene::new(1, "t");
		scene.sources.push(Source {
			transform: Transform::full(4, 4),
			..Source::new(1, SourceKind::Colour { colour: Colour::WHITE, size: (4, 4) })
		});
		let feeds = vec![Some(Arc::new(Feed::new()))];
		let feed = feeds[0].clone().unwrap();
		feed.put(flat(4, 4, [7, 8, 9, 255]));
		let out = compositor.compose(&scene, 1, &feeds, Duration::ZERO).unwrap();
		assert_eq!(pixel(&out, 0, 0), [7, 8, 9, 255]);
		drop(out);
		// Nothing new: the same picture again.
		let out = compositor.compose(&scene, 1, &feeds, Duration::from_millis(33)).unwrap();
		assert_eq!(pixel(&out, 0, 0), [7, 8, 9, 255]);
		assert_eq!(out.timestamp, Duration::from_millis(33));
		assert_eq!(feed.delivered(), 1);
		drop(out);
		// A feed that never delivered draws nothing but does not fail.
		let out = compositor.compose(&scene, 2, &[None], Duration::ZERO).unwrap();
		assert_eq!(pixel(&out, 0, 0), [0, 0, 0, 255]);
	}

	#[test]
	fn plans_only_when_something_changed() {
		let mut compositor = Compositor::new(2);
		compositor.set_size(8, 8);
		let mut scene = Scene::new(1, "t");
		scene.sources.push(Source {
			transform: Transform::full(8, 8),
			..Source::new(1, SourceKind::Colour { colour: Colour::WHITE, size: (8, 8) })
		});
		let feeds = vec![Some(Arc::new(Feed::new()))];
		let feed = feeds[0].clone().unwrap();
		for _ in 0..5 {
			feed.put(flat(8, 8, [1, 2, 3, 255]));
			drop(compositor.compose(&scene, 1, &feeds, Duration::ZERO).unwrap());
		}
		assert_eq!(compositor.stats().replans, 1, "the plan was kept");
		assert_eq!(compositor.stats().frames, 5);
		// A source that resizes replans.
		feed.put(flat(4, 4, [1, 2, 3, 255]));
		drop(compositor.compose(&scene, 1, &feeds, Duration::ZERO).unwrap());
		assert_eq!(compositor.stats().replans, 2);
		// So does a scene change.
		feed.put(flat(4, 4, [1, 2, 3, 255]));
		drop(compositor.compose(&scene, 2, &feeds, Duration::ZERO).unwrap());
		assert_eq!(compositor.stats().replans, 3);
		assert!(compositor.stats().compose_time > Duration::ZERO);
		assert_eq!(compositor.stats().threads, 2);
	}

	#[test]
	fn mirrors_a_camera() {
		let mut compositor = Compositor::new(1);
		compositor.set_size(4, 2);
		let mut scene = Scene::new(1, "t");
		scene.sources.push(Source {
			transform: Transform { fit: Fit::Stretch, ..Transform::full(4, 2) },
			..Source::new(
				1,
				SourceKind::Camera { device: String::new(), size: None, fps: None, mirror: true },
			)
		});
		// A column ramp, so mirroring is visible at 1:1 and when scaled.
		let ramp = painted(4, 2, false, |x, _| [(x as u8 + 1) * 10, 0, 0, 255]);
		let out = compose(&mut compositor, &scene, 1, &[Some(ramp.clone())]);
		assert_eq!(pixel(&out, 0, 0)[0], 40);
		assert_eq!(pixel(&out, 3, 0)[0], 10);
		// Scaled to 8 wide: still mirrored.
		compositor.set_size(8, 2);
		let out = compose(&mut compositor, &scene, 2, &[Some(ramp)]);
		assert!(pixel(&out, 0, 0)[0] > pixel(&out, 7, 0)[0], "mirrored while scaling");
	}

	#[test]
	fn preview_scaler_downscales() {
		let mut scaler = RgbaScaler::new(2);
		// Left half red, right half blue; halved, the halves stay.
		let frame =
			painted(16, 8, true, |x, _| if x < 8 { [255, 0, 0, 255] } else { [0, 0, 255, 255] });
		let out = scaler.scale(&frame, 8, 4).unwrap();
		assert_eq!((out.width, out.height), (8, 4));
		assert_eq!(out.format(), PixelFormat::Rgba);
		assert_eq!(pixel(&out, 0, 0), [255, 0, 0, 255]);
		assert_eq!(pixel(&out, 7, 3), [0, 0, 255, 255]);
		drop(out);
		// 1:1 copies and keeps the timestamp.
		let frame = VideoFrame { timestamp: Duration::from_millis(7), ..(*frame).clone() };
		let out = scaler.scale(&frame, 16, 8).unwrap();
		assert_eq!(out.timestamp, Duration::from_millis(7));
		assert_eq!(pixel(&out, 0, 0), [255, 0, 0, 255]);
		assert_eq!(pixel(&out, 15, 7), [0, 0, 255, 255]);
		// I420 is not a preview input.
		assert!(scaler.scale(&VideoFrame::black_i420(4, 4), 2, 2).is_err());
	}

	#[test]
	fn an_unset_output_size_fails() {
		let mut compositor = Compositor::new(1);
		let scene = Scene::new(1, "t");
		assert!(compositor.compose(&scene, 1, &[], Duration::ZERO).is_err());
		compositor.set_size(1, 1);
		// Rounded up to the smallest even size.
		assert_eq!(compositor.size(), (2, 2));
		compositor.reset();
		assert!(compositor.compose(&scene, 1, &[], Duration::ZERO).is_ok());
	}
}
