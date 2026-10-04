//! Scaling I420 frames, and the pyramid of sizes simulcast layers need.
//!
//! [`PlaneScaler`] resizes one plane with a separable filter whose weights
//! are computed once per size pair: area averaging for downscaling (every
//! source pixel counts by how much of it an output pixel covers, so text
//! and fine lines do not alias at any ratio) and bilinear interpolation for
//! upscaling; exactly half the size takes a 2x2 box fast path. Work is split
//! into bands of output rows on a [`Workers`] pool.
//!
//! [`Pyramid`] turns one captured frame into I420 frames of every size the
//! layers ask for: it converts the capture once at full size
//! ([`Converter`]), then derives each smaller size from the nearest larger
//! one, reusing pooled frames, so a steady stream allocates nothing.

use std::sync::{Arc, Mutex, PoisonError};

use crate::convert::Converter;
use crate::frame::{FrameData, FrameRef, PlaneRef, VideoFrame, chroma_size};
use crate::pool::FramePool;
use crate::workers::{Slots, Workers};
use crate::{Error, Result};

/// Fixed-point scale of filter weights.
const ONE: u32 = 1 << 14;
/// Fractional bits kept between the vertical and the horizontal pass.
/// [`crate::studio::compose`] uses the same two-pass layout for RGBA.
pub(crate) const MID_BITS: u32 = 6;

/// Filter taps of one output coordinate: source index of the first tap, and
/// where its weights start.
#[derive(Clone, Copy, Debug)]
struct Taps {
	start: u32,
	len: u32,
	weights: u32,
}

/// The filter along one axis.
#[derive(Clone, Debug, Default)]
pub(crate) struct Axis {
	taps: Vec<Taps>,
	weights: Vec<u32>,
}

impl Axis {
	/// Weights (summing to [`ONE`]) mapping `src` samples to `dst`.
	pub(crate) fn new(src: usize, dst: usize) -> Self {
		let mut axis = Self { taps: Vec::with_capacity(dst), weights: Vec::new() };
		let ratio = src as f64 / dst as f64;
		let mut scratch: Vec<(usize, f64)> = Vec::new();
		for o in 0..dst {
			scratch.clear();
			if ratio >= 1.0 {
				// Area: source pixels overlapping [o, o + 1) * ratio.
				let (lo, hi) = (o as f64 * ratio, (o as f64 + 1.0) * ratio);
				let first = lo.floor() as usize;
				let last = (hi.ceil() as usize).min(src);
				for i in first..last {
					let overlap = hi.min(i as f64 + 1.0) - lo.max(i as f64);
					if overlap > 1e-9 {
						scratch.push((i, overlap / ratio));
					}
				}
			} else {
				// Bilinear between the two nearest source pixels.
				let s = ((o as f64 + 0.5) * ratio - 0.5).clamp(0.0, (src - 1) as f64);
				let i = s.floor() as usize;
				let f = s - i as f64;
				scratch.push((i, 1.0 - f));
				if i + 1 < src && f > 1e-9 {
					scratch.push((i + 1, f));
				}
			}
			axis.push(&scratch);
		}
		axis
	}

	/// Quantize one output's weights so they sum to exactly [`ONE`].
	fn push(&mut self, taps: &[(usize, f64)]) {
		let offset = self.weights.len() as u32;
		let mut sum = 0;
		for &(_, w) in taps {
			let q = (w * f64::from(ONE)).round() as u32;
			self.weights.push(q);
			sum += q;
		}
		// Rounding error goes to the largest weight.
		let slice = &mut self.weights[offset as usize..];
		if let Some(largest) = slice.iter_mut().max_by_key(|w| **w) {
			*largest = (*largest + ONE).saturating_sub(sum);
		}
		self.taps.push(Taps { start: taps[0].0 as u32, len: taps.len() as u32, weights: offset });
	}

	/// Source index of the first tap of output `o`, and its weights.
	pub(crate) fn get(&self, o: usize) -> (usize, &[u32]) {
		let t = self.taps[o];
		(t.start as usize, &self.weights[t.weights as usize..(t.weights + t.len) as usize])
	}
}

/// Resizes planes of one source size to one destination size.
pub struct PlaneScaler {
	src: (usize, usize),
	dst: (usize, usize),
	x: Axis,
	y: Axis,
	/// Exactly half the size: 2x2 box average.
	half: bool,
	/// One row of vertically filtered samples per task.
	scratch: Vec<Mutex<Vec<u16>>>,
}

impl PlaneScaler {
	pub fn new(src: (usize, usize), dst: (usize, usize)) -> Self {
		let (src, dst) = ((src.0.max(1), src.1.max(1)), (dst.0.max(1), dst.1.max(1)));
		let half = src.0 == dst.0 * 2 && src.1 == dst.1 * 2;
		let (x, y) = if half {
			(Axis::default(), Axis::default())
		} else {
			(Axis::new(src.0, dst.0), Axis::new(src.1, dst.1))
		};
		Self { src, dst, x, y, half, scratch: Vec::new() }
	}

	pub fn sizes(&self) -> ((usize, usize), (usize, usize)) {
		(self.src, self.dst)
	}

	/// Scale `src` into `dst` (`dst_stride` bytes per row).
	pub fn scale(
		&mut self,
		workers: &mut Workers,
		src: PlaneRef<'_>,
		dst: &mut [u8],
		dst_stride: usize,
	) -> Result<()> {
		let ((sw, sh), (dw, dh)) = (self.src, self.dst);
		if src.stride < sw || src.data.len() < src.stride * (sh - 1) + sw {
			return Err(Error::InvalidFrame("scaler source plane too small".into()));
		}
		if dst_stride < dw || dst.len() < dst_stride * (dh - 1) + dw {
			return Err(Error::InvalidFrame("scaler destination plane too small".into()));
		}
		let tasks = workers.tasks(dh);
		let band = dh.div_ceil(tasks);
		let tasks = dh.div_ceil(band);
		if !self.half {
			while self.scratch.len() < tasks {
				self.scratch.push(Mutex::new(vec![0; sw]));
			}
		}
		let slots = Slots::new(dst.chunks_mut(band * dst_stride).take(tasks).enumerate());
		let this = &*self;
		workers.run(slots.len(), &|task| {
			let Some((i, out)) = slots.take(task) else { return };
			let rows = i * band..((i + 1) * band).min(dh);
			if this.half {
				half_rows(src, out, dst_stride, dw, rows);
			} else {
				let mut tmp = this.scratch[i].lock().unwrap_or_else(PoisonError::into_inner);
				this.filter_rows(src, out, dst_stride, rows, &mut tmp);
			}
		});
		Ok(())
	}

	fn filter_rows(
		&self,
		src: PlaneRef<'_>,
		out: &mut [u8],
		stride: usize,
		rows: std::ops::Range<usize>,
		tmp: &mut [u16],
	) {
		let (sw, dw) = (self.src.0, self.dst.0);
		let first = rows.start;
		for oy in rows {
			// Vertical pass: tmp = weighted sum of source rows, with
			// MID_BITS fractional bits.
			let (start, weights) = self.y.get(oy);
			const CHUNK: usize = 64;
			let mut acc = [0u32; CHUNK];
			let mut x0 = 0;
			while x0 < sw {
				let n = CHUNK.min(sw - x0);
				let acc = &mut acc[..n];
				acc.fill(0);
				for (k, &w) in weights.iter().enumerate() {
					let row = &src.row(start + k, sw)[x0..x0 + n];
					for (a, &p) in acc.iter_mut().zip(row) {
						*a += w * u32::from(p);
					}
				}
				for (t, &a) in tmp[x0..x0 + n].iter_mut().zip(acc.iter()) {
					*t = ((a + (1 << (13 - MID_BITS))) >> (14 - MID_BITS)) as u16;
				}
				x0 += n;
			}
			// Horizontal pass.
			let line = &mut out[(oy - first) * stride..][..dw];
			for (ox, px) in line.iter_mut().enumerate() {
				let (start, weights) = self.x.get(ox);
				let sum: u32 =
					weights.iter().zip(&tmp[start..]).map(|(&w, &t)| w * u32::from(t)).sum();
				*px = ((sum + (1 << (13 + MID_BITS))) >> (14 + MID_BITS)).min(255) as u8;
			}
		}
	}
}

/// 2x2 box average of output rows `rows` (source exactly twice the size).
fn half_rows(
	src: PlaneRef<'_>,
	out: &mut [u8],
	stride: usize,
	dw: usize,
	rows: std::ops::Range<usize>,
) {
	let first = rows.start;
	for oy in rows {
		let a = src.row(oy * 2, dw * 2);
		let b = src.row(oy * 2 + 1, dw * 2);
		let line = &mut out[(oy - first) * stride..][..dw];
		for ((px, a), b) in line.iter_mut().zip(a.chunks_exact(2)).zip(b.chunks_exact(2)) {
			let sum = u16::from(a[0]) + u16::from(a[1]) + u16::from(b[0]) + u16::from(b[1]);
			*px = ((sum + 2) >> 2) as u8;
		}
	}
}

/// Scalers for the three planes of an I420 frame (U and V share one).
struct I420Scaler {
	luma: PlaneScaler,
	chroma: PlaneScaler,
}

impl I420Scaler {
	fn new(src: (u32, u32), dst: (u32, u32)) -> Self {
		let (sc, dc) = (chroma_size(src.0, src.1), chroma_size(dst.0, dst.1));
		Self {
			luma: PlaneScaler::new(
				(src.0 as usize, src.1 as usize),
				(dst.0 as usize, dst.1 as usize),
			),
			chroma: PlaneScaler::new(sc, dc),
		}
	}

	fn scale(
		&mut self,
		workers: &mut Workers,
		src: &VideoFrame,
		dst: &mut VideoFrame,
	) -> Result<()> {
		let (FrameData::I420 { y, u, v }, FrameData::I420 { y: dy, u: du, v: dv }) =
			(&src.data, &mut dst.data)
		else {
			return Err(Error::InvalidFrame("scaling needs I420 frames".into()));
		};
		self.luma.scale(workers, y.view(), &mut dy.data, dy.stride)?;
		self.chroma.scale(workers, u.view(), &mut du.data, du.stride)?;
		self.chroma.scale(workers, v.view(), &mut dv.data, dv.stride)?;
		dst.timestamp = src.timestamp;
		Ok(())
	}
}

/// Scale an I420 frame into another of a different size (allocates filter
/// tables; for repeated use see [`Pyramid`]).
pub fn scale_i420(workers: &mut Workers, src: &VideoFrame, dst: &mut VideoFrame) -> Result<()> {
	src.validate()?;
	dst.validate()?;
	I420Scaler::new((src.width, src.height), (dst.width, dst.height)).scale(workers, src, dst)
}

/// One size of the pyramid.
struct Level {
	size: (u32, u32),
	/// The level this one is scaled from; `None`: from the full-size frame.
	source: Option<usize>,
	scaler: Option<I420Scaler>,
	pool: FramePool,
	/// This frame's picture at this size.
	current: Option<Arc<VideoFrame>>,
	/// Needed for this frame (an output is due, or a needed level is scaled
	/// from it).
	needed: bool,
}

/// Turns captured frames into I420 frames of several sizes; see the
/// [module docs](self).
pub struct Pyramid {
	converter: Converter,
	input: (u32, u32),
	full: FramePool,
	levels: Vec<Level>,
	/// For each output: its level.
	outputs: Vec<usize>,
	/// The output sizes the plan was made for.
	planned: Vec<(u32, u32)>,
}

impl Pyramid {
	/// `threads` as in [`Workers::new`](crate::workers::Workers::new) (0:
	/// one per CPU).
	pub fn new(threads: usize) -> Self {
		Self {
			converter: Converter::new(threads),
			input: (0, 0),
			full: FramePool::new(),
			levels: Vec::new(),
			outputs: Vec::new(),
			planned: Vec::new(),
		}
	}

	/// Threads converting and scaling, the caller included.
	pub fn threads(&self) -> usize {
		self.converter.threads()
	}

	/// Plan the levels for these output sizes and this input size.
	fn plan(&mut self, input: (u32, u32), sizes: &[(u32, u32)]) {
		self.input = input;
		self.planned.clear();
		self.planned.extend_from_slice(sizes);
		let mut distinct: Vec<(u32, u32)> = sizes.iter().copied().filter(|&s| s != input).collect();
		distinct.sort_unstable_by_key(|&(w, h)| std::cmp::Reverse(u64::from(w) * u64::from(h)));
		distinct.dedup();
		self.levels.clear();
		for size in distinct {
			// The smallest planned level that covers this size, else the
			// full frame.
			let source = self
				.levels
				.iter()
				.enumerate()
				.filter(|(_, l)| l.size.0 >= size.0 && l.size.1 >= size.1)
				.min_by_key(|(_, l)| u64::from(l.size.0) * u64::from(l.size.1))
				.map(|(i, _)| i);
			let from = source.map_or(input, |i| self.levels[i].size);
			self.levels.push(Level {
				size,
				source,
				scaler: Some(I420Scaler::new(from, size)),
				pool: FramePool::new(),
				current: None,
				needed: false,
			});
		}
		self.outputs.clear();
		for &size in sizes {
			let level = self.levels.iter().position(|l| l.size == size).unwrap_or(usize::MAX);
			self.outputs.push(level);
		}
	}

	/// Produce frames for the outputs of this capture: `sizes[i]` is output
	/// `i`'s size, and `out[i]` gets its frame if `due[i]` (else `None`).
	/// Outputs of the same size share one frame; the capture is converted
	/// once. Allocates only when sizes change or all pooled frames are in
	/// use.
	pub fn process(
		&mut self,
		frame: &FrameRef<'_>,
		sizes: &[(u32, u32)],
		due: &[bool],
		out: &mut [Option<Arc<VideoFrame>>],
	) -> Result<()> {
		let input = (frame.width, frame.height);
		if input != self.input || sizes != self.planned.as_slice() {
			self.plan(input, sizes);
		}
		for level in &mut self.levels {
			level.needed = false;
			level.current = None;
		}
		let mut full_needed = false;
		for (i, &level) in self.outputs.iter().enumerate() {
			if !due.get(i).copied().unwrap_or(false) {
				continue;
			}
			let mut at = level;
			if at == usize::MAX {
				full_needed = true;
				continue;
			}
			// Mark the chain of sources.
			loop {
				self.levels[at].needed = true;
				match self.levels[at].source {
					Some(s) => at = s,
					None => {
						full_needed = true;
						break;
					}
				}
			}
		}
		let full = if full_needed {
			let slot = self.full.get(input.0, input.1);
			let target = Arc::get_mut(slot).expect("the pool hands out unshared frames");
			self.converter.to_i420_into(frame, target)?;
			Some(slot.clone())
		} else {
			None
		};
		for i in 0..self.levels.len() {
			if !self.levels[i].needed {
				continue;
			}
			let source = match self.levels[i].source {
				Some(s) => self.levels[s].current.clone(),
				None => full.clone(),
			}
			.expect("sources come first and are needed");
			let level = &mut self.levels[i];
			let slot = level.pool.get(level.size.0, level.size.1);
			let target = Arc::get_mut(slot).expect("the pool hands out unshared frames");
			let scaler = level.scaler.as_mut().expect("planned");
			scaler.scale(self.converter.workers(), &source, target)?;
			level.current = Some(slot.clone());
		}
		for (i, slot) in out.iter_mut().enumerate() {
			*slot = None;
			if !due.get(i).copied().unwrap_or(false) {
				continue;
			}
			*slot = match self.outputs.get(i) {
				Some(&usize::MAX) => full.clone(),
				Some(&level) => self.levels[level].current.clone(),
				None => None,
			};
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;
	use crate::convert;
	use crate::frame::{PixelsRef, Plane};

	fn gray(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Plane {
		let mut data = vec![0; w * h];
		for y in 0..h {
			for x in 0..w {
				data[y * w + x] = f(x, y);
			}
		}
		Plane::new(data, w)
	}

	fn scale_plane(src: &Plane, sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
		let mut workers = Workers::new("test-scale", 3);
		let mut scaler = PlaneScaler::new((sw, sh), (dw, dh));
		let mut out = vec![0; dw * dh];
		scaler.scale(&mut workers, src.view(), &mut out, dw).unwrap();
		out
	}

	#[test]
	fn flat_stays_flat_at_any_ratio() {
		for (sw, sh, dw, dh) in
			[(64, 48, 32, 24), (1920, 1080, 1280, 720), (97, 55, 30, 17), (40, 30, 64, 45)]
		{
			let src = gray(sw, sh, |_, _| 77);
			let out = scale_plane(&src, sw, sh, dw, dh);
			assert!(out.iter().all(|&p| p == 77), "{sw}x{sh} -> {dw}x{dh}");
		}
	}

	#[test]
	fn area_average_and_half() {
		// Alternating columns 0/200 average to 100 at half and a third of
		// the width.
		let src = gray(12, 4, |x, _| if x % 2 == 0 { 0 } else { 200 });
		assert!(scale_plane(&src, 12, 4, 6, 2).iter().all(|&p| p == 100));
		let third = scale_plane(&src, 12, 4, 4, 4);
		// Each output covers three columns: 0,200,0 or 200,0,200.
		assert_eq!(third[..4], [67, 133, 67, 133]);
		// A gradient keeps its mean.
		let src = gray(300, 10, |x, _| (x * 255 / 299) as u8);
		let out = scale_plane(&src, 300, 10, 128, 7);
		let mean = |d: &[u8]| d.iter().map(|&p| f64::from(p)).sum::<f64>() / d.len() as f64;
		assert!((mean(&out) - mean(&src.data)).abs() < 1.0);
		assert!(out.windows(2).take(127).all(|w| w[0] <= w[1]), "monotonic");
	}

	fn bgra(w: u32, h: u32, n: u32) -> VideoFrame {
		let mut data = vec![0; (w * h * 4) as usize];
		for y in 0..h {
			for x in 0..w {
				let i = ((y * w + x) * 4) as usize;
				let inside = x / 16 % 2 == (y / 16 + n) % 2;
				let c = if inside { [30, 90, 240, 255] } else { [200, 200, 200, 255] };
				data[i..i + 4].copy_from_slice(&c);
			}
		}
		VideoFrame::from_bgra(w, h, (w * 4) as usize, data)
			.unwrap()
			.with_timestamp(Duration::from_millis(u64::from(n)))
	}

	#[test]
	fn pyramid_outputs_share_and_recycle() {
		let mut pyramid = Pyramid::new(3);
		let sizes = [(320, 240), (160, 120), (320, 240), (106, 80)];
		let mut out = vec![None; 4];
		let src = bgra(320, 240, 0);
		pyramid.process(&src.view(), &sizes, &[true, true, true, true], &mut out).unwrap();
		let frames: Vec<Arc<VideoFrame>> = out.iter().map(|f| f.clone().unwrap()).collect();
		assert!(Arc::ptr_eq(&frames[0], &frames[2]), "same size, same frame");
		for (f, &(w, h)) in frames.iter().zip(&sizes) {
			assert_eq!((f.width, f.height), (w, h));
			f.validate().unwrap();
		}
		// The full size matches a direct conversion.
		let direct = convert::to_i420(&src).unwrap();
		assert_eq!(*frames[0], *direct);
		// Scaled levels look like the source scaled directly.
		for i in [1, 3] {
			let mut expected = VideoFrame::black_i420(sizes[i].0, sizes[i].1);
			scale_i420(&mut Workers::new("test", 1), &frames[0], &mut expected).unwrap();
			let psnr = convert::psnr(&frames[i], &expected).unwrap();
			assert!(psnr > 35.0, "level {i}: {psnr:.1} dB");
		}

		// Not due: no frame. Frames still held are not reused.
		pyramid
			.process(&bgra(320, 240, 1).view(), &sizes, &[false, true, false, false], &mut out)
			.unwrap();
		assert!(out[0].is_none() && out[3].is_none());
		assert!(!Arc::ptr_eq(out[1].as_ref().unwrap(), &frames[1]));
		assert_eq!(out[1].as_ref().unwrap().timestamp, Duration::from_millis(1));
		drop(frames);
		// Steady state: the pools stop growing.
		for n in 2..10 {
			pyramid.process(&bgra(320, 240, n).view(), &sizes, &[true; 4], &mut out).unwrap();
		}
		let allocated = pyramid.full.allocated()
			+ pyramid.levels.iter().map(|l| l.pool.allocated()).sum::<u64>();
		for n in 10..20 {
			pyramid.process(&bgra(320, 240, n).view(), &sizes, &[true; 4], &mut out).unwrap();
		}
		let after = pyramid.full.allocated()
			+ pyramid.levels.iter().map(|l| l.pool.allocated()).sum::<u64>();
		assert_eq!(allocated, after);
	}

	#[test]
	fn converter_handles_strides_and_unpadded_last_rows() {
		let src = bgra(66, 34, 3);
		let FrameData::Bgra(plane) = &src.data else { unreachable!() };
		// Re-pack with a 300-byte stride and no padding after the last row.
		let stride = 300;
		let mut data = vec![0xee; stride * 33 + 66 * 4];
		for y in 0..34 {
			let row = plane.row(y, 66 * 4);
			data[y * stride..y * stride + 66 * 4].copy_from_slice(row);
		}
		let view = FrameRef {
			width: 66,
			height: 34,
			timestamp: src.timestamp,
			pixels: PixelsRef::Bgra(PlaneRef::new(&data, stride)),
		};
		let mut converter = Converter::new(4);
		let mut pool = FramePool::new();
		let target = Arc::get_mut(pool.get(66, 34)).unwrap();
		converter.to_i420_into(&view, target).unwrap();
		assert_eq!(*target, *convert::to_i420(&src).unwrap());
		// Odd height, RGBA.
		let odd = VideoFrame::from_rgba(33, 17, 33 * 4, vec![128; 33 * 17 * 4]).unwrap();
		let target = Arc::get_mut(pool.get(33, 17)).unwrap();
		converter.to_i420_into(&odd.view(), target).unwrap();
		assert_eq!(*target, *convert::to_i420(&odd).unwrap());
		// I420 and NV12 sources are copied.
		let i420 = convert::to_i420(&src).unwrap().into_owned();
		let target = Arc::get_mut(pool.get(66, 34)).unwrap();
		converter.to_i420_into(&i420.view(), target).unwrap();
		assert_eq!(*target, i420);
		assert!(converter.to_i420_into(&odd.view(), target).is_err(), "size mismatch");
	}
}
