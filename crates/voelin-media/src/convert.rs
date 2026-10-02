//! Pixel format conversion through `yuv` (yuvutils-rs, SIMD: AVX2 / SSE4.1
//! / NEON picked at runtime).
//!
//! All YUV data uses BT.601 limited range: WebRTC endpoints assume it for VP8,
//! VP9 and H.264 unless the stream signals something else, and libwebrtc's
//! capture pipeline produces it.
//!
//! [`Converter`] is the streaming path: it converts a borrowed frame (e.g. a
//! mapped capture buffer, any stride) straight into a preallocated I420
//! frame, in bands of rows on all cores, without allocating.

use std::borrow::Cow;
use std::sync::{Mutex, PoisonError};

use yuv::{
	BufferStoreMut, YuvBiPlanarImage, YuvConversionMode, YuvPlanarImage, YuvPlanarImageMut,
	YuvRange, YuvStandardMatrix,
};

use crate::frame::{FrameData, FrameRef, PixelsRef, Plane, VideoFrame, chroma_size};
use crate::workers::{Slots, Workers};
use crate::{Error, Result};

const RANGE: YuvRange = YuvRange::Limited;
const MATRIX: YuvStandardMatrix = YuvStandardMatrix::Bt601;

fn error(e: yuv::YuvError) -> Error {
	Error::Convert(e.to_string())
}

fn stride_u32(stride: usize) -> Result<u32> {
	u32::try_from(stride).map_err(|_| Error::InvalidFrame(format!("stride {stride} too large")))
}

/// Write the frame as RGBA (alpha 255) into `out`, `out_stride` bytes per row.
///
/// Suits a Slint `SharedPixelBuffer<Rgba8Pixel>`:
/// `to_rgba(&frame, buffer.make_mut_bytes(), width as usize * 4)`.
pub fn to_rgba(frame: &VideoFrame, out: &mut [u8], out_stride: usize) -> Result<()> {
	to_rgba_ref(&frame.view(), out, out_stride)
}

/// Write a borrowed frame (e.g. a mapped capture buffer) as RGBA (alpha 255)
/// into `out`, `out_stride` bytes per row. Allocates nothing.
pub fn to_rgba_ref(frame: &FrameRef<'_>, out: &mut [u8], out_stride: usize) -> Result<()> {
	frame.validate()?;
	let (w, h) = (frame.width, frame.height);
	let row = w as usize * 4;
	let needed = out_stride * (h as usize - 1) + row;
	if out_stride < row || out.len() < needed {
		return Err(Error::InvalidFrame(format!(
			"RGBA output of {} bytes with stride {out_stride} is too small for {w}x{h}",
			out.len()
		)));
	}
	let out_stride_u32 = stride_u32(out_stride)?;
	match &frame.pixels {
		PixelsRef::I420 { y, u, v } => {
			let image = YuvPlanarImage {
				y_plane: y.data,
				y_stride: stride_u32(y.stride)?,
				u_plane: u.data,
				u_stride: stride_u32(u.stride)?,
				v_plane: v.data,
				v_stride: stride_u32(v.stride)?,
				width: w,
				height: h,
			};
			yuv::yuv420_to_rgba(&image, out, out_stride_u32, RANGE, MATRIX).map_err(error)
		}
		PixelsRef::Nv12 { y, uv } => {
			let image = YuvBiPlanarImage {
				y_plane: y.data,
				y_stride: stride_u32(y.stride)?,
				uv_plane: uv.data,
				uv_stride: stride_u32(uv.stride)?,
				width: w,
				height: h,
			};
			yuv::yuv_nv12_to_rgba(&image, out, out_stride_u32, RANGE, MATRIX, mode()).map_err(error)
		}
		PixelsRef::Bgra(p) => {
			for y in 0..h as usize {
				let src = p.row(y, row);
				let dst = &mut out[y * out_stride..y * out_stride + row];
				for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
					d.copy_from_slice(&[s[2], s[1], s[0], 255]);
				}
			}
			Ok(())
		}
		PixelsRef::Rgba(p) => {
			for y in 0..h as usize {
				out[y * out_stride..y * out_stride + row].copy_from_slice(p.row(y, row));
			}
			Ok(())
		}
	}
}

/// The frame as tightly packed RGBA (`width * 4` bytes per row).
pub fn to_rgba_vec(frame: &VideoFrame) -> Result<Vec<u8>> {
	let stride = frame.width as usize * 4;
	let mut out = vec![0; stride * frame.height as usize];
	to_rgba(frame, &mut out, stride)?;
	Ok(out)
}

fn mode() -> YuvConversionMode {
	YuvConversionMode::Balanced
}

/// The frame as I420 with equal U and V strides (what the encoders take).
/// Borrows I420 input that already has that layout.
pub fn to_i420(frame: &VideoFrame) -> Result<Cow<'_, VideoFrame>> {
	frame.validate()?;
	let (w, h) = (frame.width, frame.height);
	let (cw, ch) = chroma_size(w, h);
	let data = match &frame.data {
		FrameData::I420 { u, v, .. } if u.stride == v.stride => return Ok(Cow::Borrowed(frame)),
		FrameData::I420 { y, u, v } => {
			FrameData::I420 { y: y.clone(), u: repack(u, cw, ch), v: repack(v, cw, ch) }
		}
		FrameData::Nv12 { y, uv } => {
			let mut u = Plane::filled(cw, ch, 0);
			let mut v = Plane::filled(cw, ch, 0);
			for row in 0..ch {
				let src = uv.row(row, cw * 2);
				let (u_row, v_row) = (&mut u.data[row * cw..][..cw], &mut v.data[row * cw..][..cw]);
				for (i, pair) in src.chunks_exact(2).enumerate() {
					u_row[i] = pair[0];
					v_row[i] = pair[1];
				}
			}
			FrameData::I420 { y: y.clone(), u, v }
		}
		FrameData::Bgra(p) | FrameData::Rgba(p) => {
			let mut image = YuvPlanarImageMut::<u8> {
				y_plane: BufferStoreMut::Owned(vec![0; w as usize * h as usize]),
				y_stride: w,
				u_plane: BufferStoreMut::Owned(vec![0; cw * ch]),
				u_stride: cw as u32,
				v_plane: BufferStoreMut::Owned(vec![0; cw * ch]),
				v_stride: cw as u32,
				width: w,
				height: h,
			};
			let stride = stride_u32(p.stride)?;
			let convert = if matches!(frame.data, FrameData::Bgra(_)) {
				yuv::bgra_to_yuv420
			} else {
				yuv::rgba_to_yuv420
			};
			convert(&mut image, &p.data, stride, RANGE, MATRIX, mode()).map_err(error)?;
			FrameData::I420 {
				y: Plane::new(owned(image.y_plane), w as usize),
				u: Plane::new(owned(image.u_plane), cw),
				v: Plane::new(owned(image.v_plane), cw),
			}
		}
	};
	Ok(Cow::Owned(VideoFrame { width: w, height: h, timestamp: frame.timestamp, data }))
}

fn owned(buffer: BufferStoreMut<'_, u8>) -> Vec<u8> {
	match buffer {
		BufferStoreMut::Owned(v) => v,
		BufferStoreMut::Borrowed(b) => b.to_vec(),
	}
}

fn repack(plane: &Plane, width: usize, rows: usize) -> Plane {
	let mut out = Plane::filled(width, rows, 0);
	for y in 0..rows {
		out.data[y * width..(y + 1) * width].copy_from_slice(plane.row(y, width));
	}
	out
}

/// Rows `r0 .. r0 + rows` of an I420 destination (`r0` even).
struct Band<'a> {
	y: &'a mut [u8],
	u: &'a mut [u8],
	v: &'a mut [u8],
	y_stride: usize,
	u_stride: usize,
	v_stride: usize,
	width: usize,
	r0: usize,
	rows: usize,
}

impl Band<'_> {
	/// Rows `from ..` of this band (`from` even).
	fn skip(&mut self, from: usize) -> Band<'_> {
		Band {
			y: &mut self.y[from * self.y_stride..],
			u: &mut self.u[from / 2 * self.u_stride..],
			v: &mut self.v[from / 2 * self.v_stride..],
			y_stride: self.y_stride,
			u_stride: self.u_stride,
			v_stride: self.v_stride,
			width: self.width,
			r0: self.r0 + from,
			rows: self.rows - from,
		}
	}

	/// Convert `rows` rows of packed pixels (`src` starts at the band's
	/// first row) with yuv's SIMD code.
	fn packed(&mut self, bgra: bool, src: &[u8], stride: usize, rows: usize) -> Result<()> {
		let mut image = YuvPlanarImageMut {
			y_plane: BufferStoreMut::Borrowed(&mut *self.y),
			y_stride: stride_u32(self.y_stride)?,
			u_plane: BufferStoreMut::Borrowed(&mut *self.u),
			u_stride: stride_u32(self.u_stride)?,
			v_plane: BufferStoreMut::Borrowed(&mut *self.v),
			v_stride: stride_u32(self.v_stride)?,
			width: self.width as u32,
			height: rows as u32,
		};
		let convert = if bgra { yuv::bgra_to_yuv420 } else { yuv::rgba_to_yuv420 };
		convert(&mut image, src, stride_u32(stride)?, RANGE, MATRIX, mode()).map_err(error)
	}
}

/// Convert one band of `src` into `band`. `tail` is scratch memory for a
/// last row pair that is not padded to the stride.
fn convert_band(src: &PixelsRef<'_>, mut band: Band<'_>, tail: &Mutex<Vec<u8>>) -> Result<()> {
	let (w, r0, rows) = (band.width, band.r0, band.rows);
	let (c0, crows) = (r0 / 2, rows.div_ceil(2));
	let cw = w.div_ceil(2);
	match *src {
		PixelsRef::Bgra(p) | PixelsRef::Rgba(p) => {
			let bgra = matches!(src, PixelsRef::Bgra(_));
			let data = &p.data[r0 * p.stride..];
			let padded = rows * p.stride;
			// yuv converts pairs of rows in `2 * stride` chunks, so it would
			// skip a final pair whose last row ends before the stride does
			// (a buffer without padding after the last row). An odd last
			// row is converted on its own and needs no padding.
			if data.len() >= padded || rows % 2 == 1 {
				band.packed(bgra, &data[..data.len().min(padded)], p.stride, rows)
			} else {
				let head = rows - 2;
				if head > 0 {
					band.packed(bgra, &data[..head * p.stride], p.stride, head)?;
				}
				let mut copy = tail.lock().unwrap_or_else(PoisonError::into_inner);
				copy.clear();
				copy.extend_from_slice(&data[head * p.stride..][..w * 4]);
				copy.extend_from_slice(&data[(head + 1) * p.stride..][..w * 4]);
				band.skip(head).packed(bgra, &copy, w * 4, 2)
			}
		}
		PixelsRef::I420 { y, u, v } => {
			for r in 0..rows {
				band.y[r * band.y_stride..][..w].copy_from_slice(y.row(r0 + r, w));
			}
			for r in 0..crows {
				band.u[r * band.u_stride..][..cw].copy_from_slice(u.row(c0 + r, cw));
				band.v[r * band.v_stride..][..cw].copy_from_slice(v.row(c0 + r, cw));
			}
			Ok(())
		}
		PixelsRef::Nv12 { y, uv } => {
			for r in 0..rows {
				band.y[r * band.y_stride..][..w].copy_from_slice(y.row(r0 + r, w));
			}
			for r in 0..crows {
				let src = uv.row(c0 + r, cw * 2);
				let u_row = &mut band.u[r * band.u_stride..][..cw];
				let v_row = &mut band.v[r * band.v_stride..][..cw];
				for ((pair, u), v) in src.chunks_exact(2).zip(u_row).zip(v_row) {
					*u = pair[0];
					*v = pair[1];
				}
			}
			Ok(())
		}
	}
}

/// Converts borrowed frames of any format (any stride) into I420 frames the
/// caller provides, in bands of rows on a [`Workers`] pool. Allocates nothing
/// per frame.
pub struct Converter {
	workers: Workers,
	tail: Mutex<Vec<u8>>,
}

impl Converter {
	/// `threads` as in [`Workers::new`] (0: one per CPU).
	pub fn new(threads: usize) -> Self {
		Self { workers: Workers::new("voelin-convert", threads), tail: Mutex::new(Vec::new()) }
	}

	/// The thread pool, for other per-frame work (e.g. scaling).
	pub fn workers(&mut self) -> &mut Workers {
		&mut self.workers
	}

	/// Threads that convert, the caller included.
	pub fn threads(&self) -> usize {
		self.workers.threads()
	}

	/// Convert `src` into `dst`, an I420 frame of the same size whose planes
	/// hold `stride * rows` bytes each (e.g. from a
	/// [`FramePool`](crate::pool::FramePool)). Takes the timestamp too.
	pub fn to_i420_into(&mut self, src: &FrameRef<'_>, dst: &mut VideoFrame) -> Result<()> {
		src.validate()?;
		if (dst.width, dst.height) != (src.width, src.height) {
			return Err(Error::InvalidFrame(format!(
				"destination is {}x{}, source {}x{}",
				dst.width, dst.height, src.width, src.height
			)));
		}
		let (w, h) = (src.width as usize, src.height as usize);
		let (_, ch) = chroma_size(src.width, src.height);
		let FrameData::I420 { y, u, v } = &mut dst.data else {
			return Err(Error::InvalidFrame("the destination is not I420".into()));
		};
		let padded = |p: &Plane, width: usize, rows: usize| {
			p.stride >= width && p.data.len() >= p.stride * rows
		};
		if !padded(y, w, h) || !padded(u, w.div_ceil(2), ch) || !padded(v, w.div_ceil(2), ch) {
			return Err(Error::InvalidFrame("destination planes are too small".into()));
		}
		dst.timestamp = src.timestamp;
		let tasks = self.workers.tasks(ch);
		let band_pairs = ch.div_ceil(tasks);
		let band_rows = band_pairs * 2;
		let (y_stride, u_stride, v_stride) = (y.stride, u.stride, v.stride);
		let bands = y
			.data
			.chunks_mut(band_rows * y_stride)
			.zip(u.data.chunks_mut(band_pairs * u_stride))
			.zip(v.data.chunks_mut(band_pairs * v_stride))
			.take(ch.div_ceil(band_pairs));
		let slots = Slots::new(bands.enumerate());
		let failed = Mutex::new(None);
		let tail = &self.tail;
		self.workers.run(slots.len(), &|task| {
			let Some((i, ((y, u), v))) = slots.take(task) else { return };
			let r0 = i * band_rows;
			let rows = band_rows.min(h - r0);
			let band = Band { y, u, v, y_stride, u_stride, v_stride, width: w, r0, rows };
			if let Err(e) = convert_band(&src.pixels, band, tail) {
				failed.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert(e);
			}
		});
		match failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
			Some(e) => Err(e),
			None => Ok(()),
		}
	}
}

/// Peak signal-to-noise ratio in dB between two frames of the same size,
/// over the RGB channels (for tests and quality checks).
pub fn psnr(a: &VideoFrame, b: &VideoFrame) -> Result<f64> {
	if (a.width, a.height) != (b.width, b.height) {
		return Err(Error::InvalidFrame(format!(
			"size mismatch: {}x{} vs {}x{}",
			a.width, a.height, b.width, b.height
		)));
	}
	let (a, b) = (to_rgba_vec(a)?, to_rgba_vec(b)?);
	let mut sum = 0.0;
	let mut n = 0.0;
	for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
		for c in 0..3 {
			let d = f64::from(pa[c]) - f64::from(pb[c]);
			sum += d * d;
			n += 1.0;
		}
	}
	let mse = sum / n;
	Ok(if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() })
}

#[cfg(test)]
mod tests {
	use super::*;

	/// BGRA test image with flat colour blocks (conversion is exact enough
	/// only away from chroma edges).
	fn blocks(w: u32, h: u32) -> VideoFrame {
		let colors: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [200, 200, 40]];
		let mut data = vec![0; (w * h * 4) as usize];
		for y in 0..h {
			for x in 0..w {
				let c = colors[((x * 2 / w) + 2 * (y * 2 / h)) as usize];
				let i = ((y * w + x) * 4) as usize;
				data[i..i + 4].copy_from_slice(&[c[2], c[1], c[0], 0]);
			}
		}
		VideoFrame::from_bgra(w, h, (w * 4) as usize, data).unwrap()
	}

	fn pixel(rgba: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
		let i = ((y * w + x) * 4) as usize;
		rgba[i..i + 4].try_into().unwrap()
	}

	fn close(a: [u8; 4], b: [u8; 3], tolerance: i32) -> bool {
		(0..3).all(|c| (i32::from(a[c]) - i32::from(b[c])).abs() <= tolerance)
	}

	#[test]
	fn bgra_to_rgba_swaps_and_sets_alpha() {
		let frame = VideoFrame::from_bgra(1, 1, 4, vec![1, 2, 3, 0]).unwrap();
		assert_eq!(to_rgba_vec(&frame).unwrap(), [3, 2, 1, 255]);
	}

	#[test]
	fn rgb_yuv_roundtrip() {
		let src = blocks(64, 48);
		let i420 = to_i420(&src).unwrap();
		assert!(matches!(i420, Cow::Owned(_)));
		let rgba = to_rgba_vec(&i420).unwrap();
		for (x, y, c) in [(10, 10, [255, 0, 0]), (50, 10, [0, 255, 0]), (10, 40, [0, 0, 255])] {
			let p = pixel(&rgba, 64, x, y);
			assert!(close(p, c, 3), "pixel at {x},{y} is {p:?}, expected {c:?}");
			assert_eq!(p[3], 255);
		}
		assert!(psnr(&src, &i420).unwrap() > 30.0);
		// Already I420: borrowed.
		assert!(matches!(to_i420(&i420).unwrap(), Cow::Borrowed(_)));
	}

	#[test]
	fn limited_range_bt601_values() {
		// Pure white and black map to Y 235 / 16, neutral chroma.
		let frame = VideoFrame::from_rgba(
			2,
			2,
			8,
			[[255; 4], [255; 4], [0, 0, 0, 255], [0, 0, 0, 255]].concat(),
		)
		.unwrap();
		let i420 = to_i420(&frame).unwrap();
		let FrameData::I420 { y, u, v } = &i420.data else { panic!("not I420") };
		assert_eq!(y.data[..2], [235, 235]);
		assert_eq!(y.data[2..4], [16, 16]);
		assert!((i32::from(u.data[0]) - 128).abs() <= 1);
		assert!((i32::from(v.data[0]) - 128).abs() <= 1);
	}

	#[test]
	fn nv12_matches_i420() {
		let i420 = to_i420(&blocks(32, 16)).unwrap().into_owned();
		let FrameData::I420 { y, u, v } = &i420.data else { panic!("not I420") };
		let uv: Vec<u8> = u.data.iter().zip(&v.data).flat_map(|(&u, &v)| [u, v]).collect();
		let nv12 = VideoFrame {
			data: FrameData::Nv12 { y: y.clone(), uv: Plane::new(uv, 32) },
			..i420.clone()
		};
		assert_eq!(to_i420(&nv12).unwrap().into_owned(), i420);
		assert_eq!(to_rgba_vec(&nv12).unwrap(), to_rgba_vec(&i420).unwrap());
	}

	#[test]
	fn odd_sizes_and_strides() {
		let src = blocks(33, 17);
		let i420 = to_i420(&src).unwrap();
		let FrameData::I420 { u, .. } = &i420.data else { panic!("not I420") };
		assert_eq!(u.stride, 17);
		// Padded output rows.
		let mut out = vec![0; 40 * 4 * 16 + 33 * 4];
		to_rgba(&i420, &mut out, 40 * 4).unwrap();
		assert!(close(pixel(&out, 40, 2, 2), [255, 0, 0], 3));
		assert!(to_rgba(&i420, &mut out[..100], 40 * 4).is_err());
	}
}
