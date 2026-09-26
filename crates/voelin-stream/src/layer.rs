//! Simulcast layers: several encodings of one stream, each with its own size,
//! frame rate and bitrate.
//!
//! The streamer encodes every layer of its [`LayerSpec`] list and tags each
//! [`EncodedFrame`](crate::EncodedFrame) with its [`LayerId`]. Each viewer
//! connection gets the layer that fits its bandwidth estimate; peers that
//! negotiate RID simulcast (`a=simulcast`) get all of them. There is no limit
//! on the number of layers.

use std::fmt;

/// A simulcast layer. Layer 0 is the one a stream without simulcast has.
pub type LayerId = u16;

/// One encoding of the stream. Compared bit for bit (`scale` by its bits),
/// so it is `Eq`.
#[derive(Clone, Debug)]
pub struct LayerSpec {
	pub id: LayerId,
	/// Output size relative to the source (1.0: the source's size). Ignored
	/// when [`size`](Self::size) is set.
	pub scale: f32,
	/// Exact output size in pixels, instead of `scale`.
	pub size: Option<(u32, u32)>,
	/// Frame rate cap; `None`: the source's frame rate.
	pub max_fps: Option<u32>,
	/// Bitrate the encoder starts with, in bit/s. Bandwidth estimates move it
	/// down (and back up to this value, or [`max_bitrate`](Self::max_bitrate)).
	pub bitrate: u64,
	/// Highest bitrate bandwidth estimates may raise the layer to; `None`: no
	/// maximum.
	pub max_bitrate: Option<u64>,
	/// Estimate (bit/s) a viewer needs to receive this layer; below it the
	/// viewer moves to the next smaller layer.
	pub min_bitrate: u64,
	/// RTP stream id for peers that negotiate RID simulcast.
	pub rid: Option<String>,
}

impl PartialEq for LayerSpec {
	fn eq(&self, other: &Self) -> bool {
		self.id == other.id
			&& self.scale.to_bits() == other.scale.to_bits()
			&& self.size == other.size
			&& self.max_fps == other.max_fps
			&& self.bitrate == other.bitrate
			&& self.max_bitrate == other.max_bitrate
			&& self.min_bitrate == other.min_bitrate
			&& self.rid == other.rid
	}
}

impl Eq for LayerSpec {}

impl LayerSpec {
	/// A stream without simulcast: layer 0 at the source's size and rate.
	pub fn single(bitrate: u64) -> Self {
		Self {
			id: 0,
			scale: 1.0,
			size: None,
			max_fps: None,
			bitrate,
			max_bitrate: None,
			min_bitrate: 0,
			rid: None,
		}
	}

	/// Output size for a source of `width` x `height`: [`size`](Self::size),
	/// or the scaled source size rounded down to even numbers (I420), at least
	/// 2x2.
	pub fn output_size(&self, width: u32, height: u32) -> (u32, u32) {
		let (w, h) = self.size.unwrap_or_else(|| {
			let scale = if self.scale.is_finite() && self.scale > 0.0 { self.scale } else { 1.0 };
			((width as f32 * scale) as u32, (height as f32 * scale) as u32)
		});
		((w & !1).max(2), (h & !1).max(2))
	}
}

/// A set of layers, e.g. those waiting for a keyframe. Grows as needed.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct LayerSet {
	words: Vec<u64>,
}

impl LayerSet {
	pub const fn new() -> Self {
		Self { words: Vec::new() }
	}

	/// Adds `layer`; `true` if it was not in the set.
	pub fn insert(&mut self, layer: LayerId) -> bool {
		let (word, bit) = Self::position(layer);
		if self.words.len() <= word {
			self.words.resize(word + 1, 0);
		}
		let new = self.words[word] & bit == 0;
		self.words[word] |= bit;
		new
	}

	/// Removes `layer`; `true` if it was in the set.
	pub fn remove(&mut self, layer: LayerId) -> bool {
		let (word, bit) = Self::position(layer);
		match self.words.get_mut(word) {
			Some(w) if *w & bit != 0 => {
				*w &= !bit;
				true
			}
			_ => false,
		}
	}

	pub fn contains(&self, layer: LayerId) -> bool {
		let (word, bit) = Self::position(layer);
		self.words.get(word).is_some_and(|w| w & bit != 0)
	}

	pub fn is_empty(&self) -> bool {
		self.words.iter().all(|w| *w == 0)
	}

	pub fn len(&self) -> usize {
		self.words.iter().map(|w| w.count_ones() as usize).sum()
	}

	/// Empties the set, keeping its memory.
	pub fn clear(&mut self) {
		self.words.fill(0);
	}

	/// Adds every layer of `other`.
	pub fn union_with(&mut self, other: &LayerSet) {
		if self.words.len() < other.words.len() {
			self.words.resize(other.words.len(), 0);
		}
		for (w, o) in self.words.iter_mut().zip(&other.words) {
			*w |= o;
		}
	}

	/// The layers in ascending order.
	pub fn iter(&self) -> impl Iterator<Item = LayerId> + '_ {
		self.words.iter().enumerate().flat_map(|(i, &word)| {
			(0..64).filter(move |b| word & (1 << b) != 0).map(move |b| (i * 64 + b) as LayerId)
		})
	}

	fn position(layer: LayerId) -> (usize, u64) {
		(usize::from(layer) / 64, 1 << (layer % 64))
	}
}

impl FromIterator<LayerId> for LayerSet {
	fn from_iter<I: IntoIterator<Item = LayerId>>(iter: I) -> Self {
		let mut set = Self::new();
		for layer in iter {
			set.insert(layer);
		}
		set
	}
}

impl fmt::Debug for LayerSet {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_set().entries(self.iter()).finish()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn layer_set() {
		let mut set = LayerSet::new();
		assert!(set.is_empty());
		assert!(set.insert(3));
		assert!(!set.insert(3));
		assert!(set.insert(200));
		assert!(set.insert(LayerId::MAX));
		assert_eq!(set.iter().collect::<Vec<_>>(), [3, 200, LayerId::MAX]);
		assert_eq!(set.len(), 3);
		assert!(set.contains(200) && !set.contains(4));
		assert!(set.remove(200) && !set.remove(200));
		let other: LayerSet = [1, 3].into_iter().collect();
		set.union_with(&other);
		assert_eq!(format!("{set:?}"), format!("{{1, 3, {}}}", LayerId::MAX));
		set.clear();
		assert!(set.is_empty());
	}

	#[test]
	fn output_size() {
		let mut layer = LayerSpec::single(1_000_000);
		assert_eq!(layer.output_size(1921, 1081), (1920, 1080));
		layer.scale = 0.5;
		assert_eq!(layer.output_size(2560, 1440), (1280, 720));
		layer.scale = 0.0001;
		assert_eq!(layer.output_size(640, 360), (2, 2));
		layer.size = Some((853, 480));
		assert_eq!(layer.output_size(1920, 1080), (852, 480));
	}
}
