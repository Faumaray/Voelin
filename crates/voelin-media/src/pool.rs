//! Recycled I420 frames.
//!
//! A [`FramePool`] belongs to one producer (e.g. the capture thread). It
//! hands out frames of one size and format for writing; the producer shares them as
//! `Arc<VideoFrame>`, and once every consumer dropped its clone the frame is
//! free again. Frames are allocated only when all are in use or the size
//! changes, so a steady stream reuses the same few buffers.

use std::sync::Arc;
use std::time::Duration;

use crate::frame::{FrameData, PixelFormat, Plane, VideoFrame, chroma_size};

/// Recycled frames of one size and pixel format (tightly packed planes).
pub struct FramePool {
	frames: Vec<Arc<VideoFrame>>,
	size: (u32, u32),
	format: PixelFormat,
	allocated: u64,
}

impl Default for FramePool {
	fn default() -> Self {
		Self { frames: Vec::new(), size: (0, 0), format: PixelFormat::I420, allocated: 0 }
	}
}

impl FramePool {
	pub fn new() -> Self {
		Self::default()
	}

	/// A `width` x `height` I420 frame nobody else holds, to write into.
	/// Its pixels are whatever an earlier user left; clone the `Arc` to
	/// share it. Changing the size drops the pool's other frames (those still
	/// in use are freed by their last user).
	pub fn get(&mut self, width: u32, height: u32) -> &mut Arc<VideoFrame> {
		self.get_format(width, height, PixelFormat::I420)
	}

	/// As [`FramePool::get`], of `format` (I420, NV12, BGRA or RGBA).
	pub fn get_format(
		&mut self,
		width: u32,
		height: u32,
		format: PixelFormat,
	) -> &mut Arc<VideoFrame> {
		if self.size != (width, height) || self.format != format {
			self.frames.clear();
			self.size = (width, height);
			self.format = format;
		}
		let free = self.frames.iter_mut().position(|f| Arc::get_mut(f).is_some());
		let index = match free {
			Some(i) => i,
			None => {
				self.allocated += 1;
				self.frames.push(Arc::new(blank(width, height, format)));
				self.frames.len() - 1
			}
		};
		&mut self.frames[index]
	}

	/// Frames allocated since the pool was created.
	pub fn allocated(&self) -> u64 {
		self.allocated
	}

	/// Frames in the pool (free or in use).
	pub fn len(&self) -> usize {
		self.frames.len()
	}

	pub fn is_empty(&self) -> bool {
		self.frames.is_empty()
	}
}

/// A zeroed frame with tightly packed planes.
fn blank(width: u32, height: u32, format: PixelFormat) -> VideoFrame {
	let (w, h) = (width as usize, height as usize);
	let (cw, ch) = chroma_size(width, height);
	let data = match format {
		PixelFormat::I420 => FrameData::I420 {
			y: Plane::filled(w, h, 0),
			u: Plane::filled(cw, ch, 0),
			v: Plane::filled(cw, ch, 0),
		},
		PixelFormat::Nv12 => {
			FrameData::Nv12 { y: Plane::filled(w, h, 0), uv: Plane::filled(cw * 2, ch, 0) }
		}
		PixelFormat::Bgra => FrameData::Bgra(Plane::filled(w * 4, h, 0)),
		PixelFormat::Rgba => FrameData::Rgba(Plane::filled(w * 4, h, 0)),
	};
	VideoFrame { width, height, timestamp: Duration::ZERO, data }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reuses_released_frames() {
		let mut pool = FramePool::new();
		let a = pool.get(64, 48).clone();
		let b = pool.get(64, 48).clone();
		assert!(!Arc::ptr_eq(&a, &b), "a is still in use");
		assert_eq!(pool.len(), 2);
		drop(a);
		let c = pool.get(64, 48).clone();
		assert_eq!(pool.allocated(), 2, "the released frame came back");
		assert!(Arc::get_mut(pool.get(64, 48)).is_some());
		drop((b, c));
		// A new size starts over.
		let d = pool.get(32, 16).clone();
		assert_eq!((d.width, d.height), (32, 16));
		d.validate().unwrap();
		assert_eq!(pool.len(), 1);
		// So does a new format.
		drop(d);
		let e = pool.get_format(32, 16, PixelFormat::Rgba).clone();
		assert_eq!(e.format(), PixelFormat::Rgba);
		e.validate().unwrap();
		assert_eq!(pool.len(), 1);
		drop(e);
		assert!(pool.get_format(32, 16, PixelFormat::Rgba).format() == PixelFormat::Rgba);
		assert_eq!(pool.allocated(), 5, "a frame is reused within one size and format");
	}
}
