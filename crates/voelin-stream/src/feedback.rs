//! Feedback from the stream sessions to the encoders of our stream's layers:
//! keyframe requests and bitrate targets per [`LayerId`], and the video
//! codecs the viewers negotiated.
//!
//! [`LayerFeedback`] is shared between the stream session (which writes) and
//! encoder threads (which read every frame). Both sides only use atomics;
//! the keyframe requests are a bitmap over all layer ids, the bitrate targets
//! a table whose parts are allocated when a layer list is prepared, so
//! nothing allocates per frame and any `u16` layer id works.

use std::array;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU64, Ordering};

use crate::layer::{LayerId, LayerSet};
use crate::peer::VideoCodec;

/// Words of the keyframe bitmap: one bit per layer id.
const WORDS: usize = (LayerId::MAX as usize + 1) / 64;
/// Summary words: one bit per bitmap word.
const SUMMARY: usize = WORDS / 64;
/// Layer ids per bitrate chunk.
const CHUNK: usize = 256;

/// Keyframe requests and bitrate targets per layer, see the module docs.
pub struct LayerFeedback {
	/// Bit `l % 64` of word `l / 64`: layer `l` needs a keyframe.
	words: Box<[AtomicU64]>,
	/// Bit `w % 64` of `summary[w / 64]`: word `w` may have bits set.
	summary: [AtomicU64; SUMMARY],
	/// Bit `s`: `summary[s]` may have bits set.
	top: AtomicU16,
	/// Bitrate targets (bit/s, 0: none) in chunks of [`CHUNK`] layer ids.
	bitrates: Box<[OnceLock<Box<[AtomicU64]>>]>,
	/// [`VideoCodec::bit`]s of the codecs connected viewers negotiated.
	codecs: AtomicU8,
}

impl Default for LayerFeedback {
	fn default() -> Self {
		Self::new()
	}
}

impl std::fmt::Debug for LayerFeedback {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("LayerFeedback").finish_non_exhaustive()
	}
}

impl LayerFeedback {
	pub fn new() -> Self {
		Self {
			words: (0..WORDS).map(|_| AtomicU64::new(0)).collect(),
			summary: array::from_fn(|_| AtomicU64::new(0)),
			top: AtomicU16::new(0),
			bitrates: (0..=LayerId::MAX as usize / CHUNK).map(|_| OnceLock::new()).collect(),
			codecs: AtomicU8::new(0),
		}
	}

	/// Allocate the bitrate slots of `layers`, so setting and reading their
	/// targets never allocates.
	pub fn prepare(&self, layers: impl IntoIterator<Item = LayerId>) {
		for layer in layers {
			self.chunk(layer);
		}
	}

	fn chunk(&self, layer: LayerId) -> &[AtomicU64] {
		self.bitrates[usize::from(layer) / CHUNK]
			.get_or_init(|| (0..CHUNK).map(|_| AtomicU64::new(0)).collect())
	}

	/// A viewer of `layer` needs a keyframe.
	pub fn request_keyframe(&self, layer: LayerId) {
		let word = usize::from(layer) / 64;
		self.words[word].fetch_or(1 << (layer % 64), Ordering::Release);
		self.summary[word / 64].fetch_or(1 << (word % 64), Ordering::Release);
		self.top.fetch_or(1 << (word / 64), Ordering::Release);
	}

	/// Call `f` with every layer a keyframe was requested for since the last
	/// call, and forget the requests.
	pub fn drain_keyframes(&self, mut f: impl FnMut(LayerId)) {
		let mut top = self.top.swap(0, Ordering::AcqRel);
		while top != 0 {
			let s = top.trailing_zeros() as usize;
			top &= top - 1;
			let mut summary = self.summary[s].swap(0, Ordering::AcqRel);
			while summary != 0 {
				let w = s * 64 + summary.trailing_zeros() as usize;
				summary &= summary - 1;
				let mut bits = self.words[w].swap(0, Ordering::AcqRel);
				while bits != 0 {
					f((w * 64 + bits.trailing_zeros() as usize) as LayerId);
					bits &= bits - 1;
				}
			}
		}
	}

	/// Add the layers a keyframe was requested for to `layers`.
	pub fn take_keyframes(&self, layers: &mut LayerSet) {
		self.drain_keyframes(|l| {
			layers.insert(l);
		});
	}

	/// Whether a keyframe was requested for any layer; forgets the requests.
	pub fn take_any_keyframe(&self) -> bool {
		let mut any = false;
		self.drain_keyframes(|_| any = true);
		any
	}

	/// Set the bitrate target of `layer`; `None`: no target (no viewer with
	/// an estimate).
	pub fn set_bitrate(&self, layer: LayerId, bitrate: Option<u64>) {
		let slot = &self.chunk(layer)[usize::from(layer) % CHUNK];
		slot.store(bitrate.map_or(0, |b| b.max(1)), Ordering::Relaxed);
	}

	/// The bitrate target of `layer` (bit/s).
	pub fn bitrate(&self, layer: LayerId) -> Option<u64> {
		let chunk = self.bitrates[usize::from(layer) / CHUNK].get()?;
		let bitrate = chunk[usize::from(layer) % CHUNK].load(Ordering::Relaxed);
		(bitrate != 0).then_some(bitrate)
	}

	/// The video codecs the viewers negotiated.
	pub fn set_codecs(&self, codecs: impl IntoIterator<Item = VideoCodec>) {
		let bits = codecs.into_iter().fold(0, |bits, c| bits | c.bit());
		self.codecs.store(bits, Ordering::Relaxed);
	}

	/// Whether a viewer negotiated `codec`.
	pub fn has_codec(&self, codec: VideoCodec) -> bool {
		self.codecs.load(Ordering::Relaxed) & codec.bit() != 0
	}

	/// Whether any viewer negotiated a codec yet.
	pub fn has_codecs(&self) -> bool {
		self.codecs.load(Ordering::Relaxed) != 0
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keyframes() {
		let f = LayerFeedback::new();
		assert!(!f.take_any_keyframe());
		for l in [0, 1, 63, 64, 4095, 4096, LayerId::MAX, 1] {
			f.request_keyframe(l);
		}
		let mut set = LayerSet::new();
		f.take_keyframes(&mut set);
		assert_eq!(set.iter().collect::<Vec<_>>(), [0, 1, 63, 64, 4095, 4096, LayerId::MAX]);
		set.clear();
		f.take_keyframes(&mut set);
		assert!(set.is_empty());
		f.request_keyframe(7);
		assert!(f.take_any_keyframe());
		assert!(!f.take_any_keyframe());
	}

	#[test]
	fn bitrates() {
		let f = LayerFeedback::new();
		assert_eq!(f.bitrate(3), None);
		f.prepare([3, 300]);
		assert_eq!(f.bitrate(3), None);
		f.set_bitrate(3, Some(1_500_000));
		f.set_bitrate(LayerId::MAX, Some(u64::MAX));
		assert_eq!(f.bitrate(3), Some(1_500_000));
		assert_eq!(f.bitrate(LayerId::MAX), Some(u64::MAX));
		assert_eq!(f.bitrate(300), None);
		f.set_bitrate(3, None);
		assert_eq!(f.bitrate(3), None);
	}

	#[test]
	fn codecs() {
		let f = LayerFeedback::new();
		assert!(!f.has_codecs());
		f.set_codecs([VideoCodec::Vp8, VideoCodec::H265, VideoCodec::Vp8]);
		assert!(f.has_codec(VideoCodec::Vp8) && f.has_codec(VideoCodec::H265));
		assert!(!f.has_codec(VideoCodec::H264));
		f.set_codecs([]);
		assert!(!f.has_codecs());
	}
}
