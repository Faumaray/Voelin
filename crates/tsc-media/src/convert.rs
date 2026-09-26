//! Pixel format conversion through `yuv` (yuvutils-rs).
//!
//! All YUV data uses BT.601 limited range: WebRTC endpoints assume it for VP8,
//! VP9 and H.264 unless the stream signals something else, and libwebrtc's
//! capture pipeline produces it.

use std::borrow::Cow;

use yuv::{
	BufferStoreMut, YuvBiPlanarImage, YuvConversionMode, YuvPlanarImage, YuvPlanarImageMut,
	YuvRange, YuvStandardMatrix,
};

use crate::frame::{FrameData, Plane, VideoFrame, chroma_size};
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
	match &frame.data {
		FrameData::I420 { y, u, v } => {
			let image = YuvPlanarImage {
				y_plane: &y.data,
				y_stride: stride_u32(y.stride)?,
				u_plane: &u.data,
				u_stride: stride_u32(u.stride)?,
				v_plane: &v.data,
				v_stride: stride_u32(v.stride)?,
				width: w,
				height: h,
			};
			yuv::yuv420_to_rgba(&image, out, out_stride_u32, RANGE, MATRIX).map_err(error)
		}
		FrameData::Nv12 { y, uv } => {
			let image = YuvBiPlanarImage {
				y_plane: &y.data,
				y_stride: stride_u32(y.stride)?,
				uv_plane: &uv.data,
				uv_stride: stride_u32(uv.stride)?,
				width: w,
				height: h,
			};
			yuv::yuv_nv12_to_rgba(&image, out, out_stride_u32, RANGE, MATRIX, mode()).map_err(error)
		}
		FrameData::Bgra(p) => {
			for y in 0..h as usize {
				let src = p.row(y, row);
				let dst = &mut out[y * out_stride..y * out_stride + row];
				for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
					d.copy_from_slice(&[s[2], s[1], s[0], 255]);
				}
			}
			Ok(())
		}
		FrameData::Rgba(p) => {
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
