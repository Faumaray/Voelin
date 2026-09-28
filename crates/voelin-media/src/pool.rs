//! Recycled I420 frames.
//!
//! A [`FramePool`] belongs to one producer (e.g. the capture thread). It
//! hands out frames of one size for writing; the producer shares them as
//! `Arc<VideoFrame>`, and once every consumer dropped its clone the frame is
//! free again. Frames are allocated only when all are in use or the size
//! changes, so a steady stream reuses the same few buffers.

use std::sync::Arc;
use std::time::Duration;

use crate::frame::{FrameData, Plane, VideoFrame, chroma_size};

/// Recycled I420 frames of one size (tightly packed planes).
#[derive(Default)]
pub struct FramePool {
	frames: Vec<Arc<VideoFrame>>,
	size: (u32, u32),
	allocated: u64,
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
		if self.size != (width, height) {
			self.frames.clear();
			self.size = (width, height);
		}
		let free = self.frames.iter_mut().position(|f| Arc::get_mut(f).is_some());
		let index = match free {
			Some(i) => i,
			None => {
				self.allocated += 1;
				self.frames.push(Arc::new(blank_i420(width, height)));
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

/// A zeroed I420 frame with tightly packed planes.
fn blank_i420(width: u32, height: u32) -> VideoFrame {
	let (cw, ch) = chroma_size(width, height);
	VideoFrame {
		width,
		height,
		timestamp: Duration::ZERO,
		data: FrameData::I420 {
			y: Plane::filled(width as usize, height as usize, 0),
			u: Plane::filled(cw, ch, 0),
			v: Plane::filled(cw, ch, 0),
		},
	}
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
	}
}
