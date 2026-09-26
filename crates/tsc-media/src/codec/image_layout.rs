//! Where the Y, U and V samples of a 4:2:0 image sit in a codec's linear
//! buffer, and copies between such buffers and [`VideoFrame`]s.
//!
//! Android's MediaCodec describes its byte-buffer layout either by a color
//! format (19 = planar I420, 21 = semi-planar NV12) plus `stride` and
//! `slice-height`, or exactly by a `MediaImage2` struct in the format's
//! `image-data` entry. Both become an [`ImageLayout`] here; this module has
//! no Android dependency so it is tested on every platform.

use crate::convert;
use crate::frame::{FrameData, Plane, VideoFrame, chroma_size};
use crate::{Error, Result};

/// `COLOR_FormatYUV420Planar`.
pub const COLOR_FORMAT_I420: i32 = 19;
/// `COLOR_FormatYUV420SemiPlanar`.
pub const COLOR_FORMAT_NV12: i32 = 21;
/// `COLOR_FormatYUV420Flexible`: the real layout is in `image-data`.
pub const COLOR_FORMAT_FLEXIBLE: i32 = 0x7F42_0888;

/// One plane: first sample at `offset`, next sample in the row `col_inc`
/// bytes further, next row `row_inc` bytes further.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneLayout {
	pub offset: usize,
	pub col_inc: usize,
	pub row_inc: usize,
}

/// An 8-bit YUV 4:2:0 image of `width` x `height` in a linear buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageLayout {
	pub width: u32,
	pub height: u32,
	pub y: PlaneLayout,
	pub u: PlaneLayout,
	pub v: PlaneLayout,
}

/// Size of `MediaImage2` (`media/hardware/VideoAPI.h`): six `u32` fields
/// and four planes of five 32-bit fields.
const MEDIA_IMAGE2_SIZE: usize = 6 * 4 + 4 * 5 * 4;
const MEDIA_IMAGE_TYPE_YUV: u32 = 1;

impl ImageLayout {
	/// Planar I420: Y rows of `stride` bytes, `slice_height` rows, then U and
	/// V with half the stride and half the rows.
	pub fn i420(width: u32, height: u32, stride: usize, slice_height: usize) -> Self {
		let stride = stride.max(width as usize);
		let slice_height = slice_height.max(height as usize);
		let c_stride = stride.div_ceil(2);
		let u = stride * slice_height;
		let v = u + c_stride * slice_height.div_ceil(2);
		Self {
			width,
			height,
			y: PlaneLayout { offset: 0, col_inc: 1, row_inc: stride },
			u: PlaneLayout { offset: u, col_inc: 1, row_inc: c_stride },
			v: PlaneLayout { offset: v, col_inc: 1, row_inc: c_stride },
		}
	}

	/// Semi-planar NV12: Y, then interleaved U/V rows of `stride` bytes.
	pub fn nv12(width: u32, height: u32, stride: usize, slice_height: usize) -> Self {
		let stride = stride.max(width as usize);
		let slice_height = slice_height.max(height as usize);
		let uv = stride * slice_height;
		Self {
			width,
			height,
			y: PlaneLayout { offset: 0, col_inc: 1, row_inc: stride },
			u: PlaneLayout { offset: uv, col_inc: 2, row_inc: stride },
			v: PlaneLayout { offset: uv + 1, col_inc: 2, row_inc: stride },
		}
	}

	/// The layout of a color format with `stride` / `slice-height` (0 when
	/// unknown). Flexible and vendor formats need `image-data` instead.
	pub fn from_color_format(
		color_format: i32,
		width: u32,
		height: u32,
		stride: usize,
		slice_height: usize,
	) -> Option<Self> {
		match color_format {
			COLOR_FORMAT_I420 => Some(Self::i420(width, height, stride, slice_height)),
			COLOR_FORMAT_NV12 => Some(Self::nv12(width, height, stride, slice_height)),
			_ => None,
		}
	}

	/// Parse a `MediaImage2` (little-endian, as on every Android ABI).
	/// Only 8-bit YUV 4:2:0 with three planes is accepted.
	pub fn from_media_image2(data: &[u8]) -> Option<Self> {
		if data.len() < MEDIA_IMAGE2_SIZE {
			return None;
		}
		let word = |i: usize| u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap());
		let (kind, planes, width, height, depth, allocated) =
			(word(0), word(1), word(2), word(3), word(4), word(5));
		if kind != MEDIA_IMAGE_TYPE_YUV || planes != 3 || depth != 8 || allocated != 8 {
			return None;
		}
		let plane = |p: usize, subsampling: u32| -> Option<PlaneLayout> {
			let base = 6 + p * 5;
			let (offset, col_inc, row_inc) =
				(word(base), word(base + 1) as i32, word(base + 2) as i32);
			if word(base + 3) != subsampling || word(base + 4) != subsampling {
				return None;
			}
			Some(PlaneLayout {
				offset: offset as usize,
				col_inc: usize::try_from(col_inc).ok().filter(|&c| c > 0)?,
				row_inc: usize::try_from(row_inc).ok().filter(|&r| r > 0)?,
			})
		};
		Some(Self { width, height, y: plane(0, 1)?, u: plane(1, 2)?, v: plane(2, 2)? })
	}

	/// The same buffer layout for a smaller visible area (a crop from the
	/// top-left corner), e.g. a decoder's 1920x1088 buffer showing 1080 rows.
	pub fn cropped(mut self, width: u32, height: u32) -> Self {
		self.width = width.min(self.width);
		self.height = height.min(self.height);
		self
	}

	/// Bytes a buffer needs to hold this image.
	pub fn buffer_len(&self) -> usize {
		let (cw, ch) = chroma_size(self.width, self.height);
		let end = |p: &PlaneLayout, w: usize, h: usize| {
			if w == 0 || h == 0 { 0 } else { p.offset + (h - 1) * p.row_inc + (w - 1) * p.col_inc + 1 }
		};
		let (w, h) = (self.width as usize, self.height as usize);
		end(&self.y, w, h).max(end(&self.u, cw, ch)).max(end(&self.v, cw, ch))
	}

	fn check(&self, len: usize) -> Result<()> {
		if len < self.buffer_len() {
			return Err(Error::InvalidFrame(format!(
				"codec buffer has {len} bytes, the {}x{} image needs {}",
				self.width,
				self.height,
				self.buffer_len()
			)));
		}
		Ok(())
	}

	/// Copy an image out of a codec buffer: NV12 when the layout is NV12,
	/// I420 otherwise.
	pub fn read(&self, buf: &[u8]) -> Result<VideoFrame> {
		self.check(buf.len())?;
		let (w, h) = (self.width as usize, self.height as usize);
		let (cw, ch) = chroma_size(self.width, self.height);
		let y = read_plane(buf, &self.y, w, h);
		let data = if self.u.col_inc == 2
			&& self.v.col_inc == 2
			&& self.v.offset == self.u.offset + 1
			&& self.u.row_inc == self.v.row_inc
		{
			let uv = PlaneLayout { col_inc: 1, ..self.u };
			FrameData::Nv12 { y, uv: read_plane(buf, &uv, cw * 2, ch) }
		} else {
			FrameData::I420 { y, u: read_plane(buf, &self.u, cw, ch), v: read_plane(buf, &self.v, cw, ch) }
		};
		let frame = VideoFrame {
			width: self.width,
			height: self.height,
			timestamp: std::time::Duration::ZERO,
			data,
		};
		frame.validate()?;
		Ok(frame)
	}

	/// Write `frame` (any pixel format, same size as the layout) into a codec
	/// buffer. Returns the bytes used.
	pub fn write(&self, frame: &VideoFrame, buf: &mut [u8]) -> Result<usize> {
		if (frame.width, frame.height) != (self.width, self.height) {
			return Err(Error::InvalidFrame(format!(
				"{}x{} frame for a {}x{} codec buffer",
				frame.width, frame.height, self.width, self.height
			)));
		}
		self.check(buf.len())?;
		let i420 = convert::to_i420(frame)?;
		let FrameData::I420 { y, u, v } = &i420.data else {
			unreachable!("to_i420 returns I420");
		};
		let (w, h) = (self.width as usize, self.height as usize);
		let (cw, ch) = chroma_size(self.width, self.height);
		write_plane(buf, &self.y, y, w, h);
		write_plane(buf, &self.u, u, cw, ch);
		write_plane(buf, &self.v, v, cw, ch);
		Ok(self.buffer_len())
	}
}

fn read_plane(buf: &[u8], p: &PlaneLayout, width: usize, rows: usize) -> Plane {
	let mut data = Vec::with_capacity(width * rows);
	for row in 0..rows {
		let start = p.offset + row * p.row_inc;
		if p.col_inc == 1 {
			data.extend_from_slice(&buf[start..start + width]);
		} else {
			data.extend((0..width).map(|x| buf[start + x * p.col_inc]));
		}
	}
	Plane::new(data, width)
}

fn write_plane(buf: &mut [u8], p: &PlaneLayout, src: &Plane, width: usize, rows: usize) {
	for row in 0..rows {
		let line = src.row(row, width);
		let start = p.offset + row * p.row_inc;
		if p.col_inc == 1 {
			buf[start..start + width].copy_from_slice(line);
		} else {
			for (x, &sample) in line.iter().enumerate() {
				buf[start + x * p.col_inc] = sample;
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A 6x4 I420 frame with distinct samples per plane.
	fn frame() -> VideoFrame {
		let (w, h) = (6usize, 4usize);
		let (cw, ch) = chroma_size(6, 4);
		VideoFrame {
			width: 6,
			height: 4,
			timestamp: std::time::Duration::ZERO,
			data: FrameData::I420 {
				y: Plane::new((0..w * h).map(|i| i as u8).collect(), w),
				u: Plane::new((0..cw * ch).map(|i| 100 + i as u8).collect(), cw),
				v: Plane::new((0..cw * ch).map(|i| 200 + i as u8).collect(), cw),
			},
		}
	}

	fn as_i420(frame: &VideoFrame) -> VideoFrame {
		convert::to_i420(frame).unwrap().into_owned()
	}

	#[test]
	fn i420_and_nv12_round_trip_with_padding() {
		let src = frame();
		for layout in [ImageLayout::i420(6, 4, 8, 6), ImageLayout::nv12(6, 4, 8, 6)] {
			let mut buf = vec![0xEE; layout.buffer_len()];
			assert_eq!(layout.write(&src, &mut buf).unwrap(), buf.len());
			let back = layout.read(&buf).unwrap();
			assert_eq!(as_i420(&back).data, src.data, "{layout:?}");
		}
		// NV12 reads back as NV12 without conversion.
		let layout = ImageLayout::nv12(6, 4, 6, 4);
		let mut buf = vec![0; layout.buffer_len()];
		layout.write(&src, &mut buf).unwrap();
		assert!(matches!(layout.read(&buf).unwrap().data, FrameData::Nv12 { .. }));
		assert_eq!(&buf[24..30], &[100, 200, 101, 201, 102, 202]);
	}

	#[test]
	fn media_image2_describes_nv21() {
		// 6x4, stride 8, 4 rows: V first in the interleaved plane (NV21).
		let mut raw = Vec::new();
		for w in [1u32, 3, 6, 4, 8, 8] {
			raw.extend(w.to_le_bytes());
		}
		for (offset, col, row, sub) in [(0u32, 1i32, 8i32, 1u32), (33, 2, 8, 2), (32, 2, 8, 2)] {
			raw.extend(offset.to_le_bytes());
			raw.extend(col.to_le_bytes());
			raw.extend(row.to_le_bytes());
			raw.extend(sub.to_le_bytes());
			raw.extend(sub.to_le_bytes());
		}
		raw.extend([0; 20]);
		let layout = ImageLayout::from_media_image2(&raw).unwrap();
		assert_eq!(layout.u, PlaneLayout { offset: 33, col_inc: 2, row_inc: 8 });
		let mut buf = vec![0; layout.buffer_len()];
		layout.write(&frame(), &mut buf).unwrap();
		assert_eq!(&buf[32..36], &[200, 100, 201, 101]);
		let back = layout.read(&buf).unwrap();
		assert!(matches!(back.data, FrameData::I420 { .. }));
		assert_eq!(back.data, frame().data);

		// Not YUV, too short, or a 10-bit image: rejected.
		let mut rgb = raw.clone();
		rgb[0] = 3;
		assert!(ImageLayout::from_media_image2(&rgb).is_none());
		assert!(ImageLayout::from_media_image2(&raw[..100]).is_none());
		let mut deep = raw.clone();
		deep[16] = 10;
		assert!(ImageLayout::from_media_image2(&deep).is_none());
	}

	#[test]
	fn crop_and_size_checks() {
		// A decoder buffer of 8x6 (stride 8) showing 6x4.
		let layout = ImageLayout::from_color_format(COLOR_FORMAT_I420, 8, 6, 8, 6)
			.unwrap()
			.cropped(6, 4);
		assert_eq!((layout.width, layout.height), (6, 4));
		assert_eq!(layout.u.offset, 48);
		assert!(ImageLayout::from_color_format(COLOR_FORMAT_FLEXIBLE, 8, 6, 8, 6).is_none());
		let short = vec![0; 10];
		assert!(matches!(layout.read(&short), Err(Error::InvalidFrame(_))));
		let mut buf = vec![0; 100];
		assert!(layout.write(&VideoFrame::black_i420(4, 4), &mut buf).is_err());
	}
}
