//! Cut a continuous sample stream into fixed-size frames.

/// Buffers samples until a whole frame is available.
#[derive(Debug)]
pub struct Framer {
	frame_len: usize,
	buf: Vec<f32>,
}

impl Framer {
	/// `frame_len` is in samples (all channels, interleaved).
	pub fn new(frame_len: usize) -> Self {
		assert!(frame_len > 0);
		Self { frame_len, buf: Vec::with_capacity(frame_len * 2) }
	}

	/// Append samples and call `f` for every complete frame.
	pub fn push(&mut self, samples: &[f32], mut f: impl FnMut(&[f32])) {
		self.buf.extend_from_slice(samples);
		let whole = self.buf.len() / self.frame_len * self.frame_len;
		for frame in self.buf[..whole].chunks_exact(self.frame_len) {
			f(frame);
		}
		self.buf.drain(..whole);
	}

	/// Pad the remaining samples with silence into one last frame, if any.
	pub fn flush(&mut self, f: impl FnOnce(&[f32])) {
		if !self.buf.is_empty() {
			self.buf.resize(self.frame_len, 0.0);
			f(&self.buf);
			self.buf.clear();
		}
	}

	pub fn buffered(&self) -> usize {
		self.buf.len()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn frames_and_flushes() {
		let mut framer = Framer::new(4);
		let mut frames = Vec::new();
		framer.push(&[1.0, 2.0, 3.0], |f| frames.push(f.to_vec()));
		assert!(frames.is_empty());
		framer.push(&[4.0, 5.0, 6.0, 7.0, 8.0, 9.0], |f| frames.push(f.to_vec()));
		assert_eq!(frames, vec![vec![1.0, 2.0, 3.0, 4.0], vec![5.0, 6.0, 7.0, 8.0]]);
		assert_eq!(framer.buffered(), 1);
		framer.flush(|f| frames.push(f.to_vec()));
		assert_eq!(frames[2], vec![9.0, 0.0, 0.0, 0.0]);
	}
}
