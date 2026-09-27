//! Where a streamer's encoded frames come from.
//!
//! [`FrameSource`] is the seam between capture/encoding (`voelin-media`) and the
//! stream sessions. [`SyntheticSource`] produces decodable placeholder media
//! without any encoder, for tests and `voelinctl stream start --synthetic`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::media::{Frequency, MediaKind, MediaTime};

use crate::layer::{LayerId, LayerSpec};

/// One encoded video or audio frame.
#[derive(Clone, Debug)]
pub struct EncodedFrame {
	pub kind: MediaKind,
	/// RTP time: 90 kHz for video, 48 kHz for Opus.
	pub time: MediaTime,
	pub data: Arc<[u8]>,
	/// The simulcast layer of a video frame (0 without simulcast, and for
	/// audio).
	pub layer: LayerId,
	/// A video keyframe: viewers can start decoding (or switch to this layer)
	/// here.
	pub keyframe: bool,
}

/// Produces encoded frames in real time.
pub trait FrameSource: Send {
	/// Append the frames that are due at `now`.
	fn poll_frames(&mut self, now: Instant, out: &mut Vec<EncodedFrame>);

	/// A viewer needs a keyframe soon.
	fn request_keyframe(&mut self) {}

	/// A viewer of simulcast layer `layer` needs a keyframe soon.
	fn request_layer_keyframe(&mut self, layer: LayerId) {
		let _ = layer;
		self.request_keyframe();
	}

	/// The bandwidth estimates of the viewers of `layer` allow `bitrate` bit/s.
	fn set_layer_bitrate(&mut self, layer: LayerId, bitrate: u64) {
		let _ = (layer, bitrate);
	}

	/// How often `poll_frames` should be called.
	fn interval(&self) -> Duration {
		Duration::from_millis(10)
	}
}

/// A complete 1x1 VP8 keyframe (the image of the smallest lossy WebP).
/// Decoders accept it, so browsers count decoded frames.
pub const VP8_KEYFRAME_1X1: [u8; 22] = [
	0x30, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x01, 0x00, 0x01, 0x00, 0x0e, 0xc0, 0xfe, 0x25, 0xa4, 0x00,
	0x03, 0x70, 0x00, 0x00, 0x00, 0x00,
];

/// A 20 ms Opus frame of silence (CELT, fullband, mono).
pub const OPUS_SILENCE: [u8; 3] = [0xf8, 0xff, 0xfe];

/// Opus frame length at 48 kHz (20 ms).
const OPUS_FRAME: u64 = 960;

/// Synthetic media: every video frame is [`VP8_KEYFRAME_1X1`] padded to a
/// chosen size (large frames span several RTP packets), audio is
/// [`OPUS_SILENCE`] every 20 ms. Byte `VP8_KEYFRAME_1X1.len()` of each video
/// frame holds the low byte of its sequence number.
///
/// With [layers](Self::with_layers) every video tick has a frame of each
/// layer (at its `max_fps`), with the low byte of the layer id in the byte
/// after the sequence number, sized for the layer's bitrate; all are flagged
/// as keyframes (they are). [`FrameSource::set_layer_bitrate`] resizes a
/// layer's frames, so the source follows the viewers' bandwidth.
pub struct SyntheticSource {
	start: Instant,
	fps: u32,
	audio: bool,
	video_frames: u64,
	audio_frames: u64,
	layers: Vec<SyntheticLayer>,
}

struct SyntheticLayer {
	id: LayerId,
	/// Frame rate (at most the source's).
	fps: u32,
	/// Bytes per frame.
	size: usize,
}

/// The smallest synthetic video frame: the keyframe, the sequence and layer bytes.
const MIN_FRAME: usize = VP8_KEYFRAME_1X1.len() + 2;

impl SyntheticSource {
	/// `fps` video frames of `video_size` bytes per second (layer 0), plus
	/// audio if `audio`.
	pub fn new(fps: u32, video_size: usize, audio: bool) -> Self {
		let fps = fps.max(1);
		let size = video_size.max(VP8_KEYFRAME_1X1.len() + 1);
		let layers = vec![SyntheticLayer { id: 0, fps, size }];
		Self { start: Instant::now(), fps, audio, video_frames: 0, audio_frames: 0, layers }
	}

	/// A frame of each of `layers` per video tick (layer 0 of `video_size`
	/// bytes if `layers` is empty); see [`set_layers`](Self::set_layers).
	pub fn with_layers(fps: u32, video_size: usize, audio: bool, layers: &[LayerSpec]) -> Self {
		let mut source = Self::new(fps, video_size, audio);
		source.set_layers(layers);
		source
	}

	/// Produce frames of `layers` from now on: each at its `max_fps` (at most
	/// the source's), `bitrate / 8 / fps` bytes per frame.
	pub fn set_layers(&mut self, layers: &[LayerSpec]) {
		if layers.is_empty() {
			return;
		}
		self.layers = layers
			.iter()
			.map(|l| {
				let fps = l.max_fps.map_or(self.fps, |f| f.clamp(1, self.fps));
				SyntheticLayer { id: l.id, fps, size: Self::frame_size(l.bitrate, fps) }
			})
			.collect();
	}

	fn frame_size(bitrate: u64, fps: u32) -> usize {
		usize::try_from(bitrate / 8 / u64::from(fps.max(1))).unwrap_or(usize::MAX).max(MIN_FRAME)
	}

	pub fn video_frame(seq: u64, size: usize) -> Vec<u8> {
		let mut frame = VP8_KEYFRAME_1X1.to_vec();
		frame.push(seq as u8);
		frame.extend((frame.len()..size).map(|i| (i as u8).wrapping_mul(31) ^ seq as u8));
		frame
	}

	/// A frame of `layer`: [`video_frame`](Self::video_frame) with the low
	/// byte of the layer id after the sequence number.
	pub fn layer_frame(seq: u64, layer: LayerId, size: usize) -> Vec<u8> {
		let mut frame = Self::video_frame(seq, size.max(MIN_FRAME));
		frame[VP8_KEYFRAME_1X1.len() + 1] = layer as u8;
		frame
	}

	/// The layer id byte of a frame from [`layer_frame`](Self::layer_frame).
	pub fn frame_layer(data: &[u8]) -> Option<u8> {
		data.get(VP8_KEYFRAME_1X1.len() + 1).copied()
	}
}

impl FrameSource for SyntheticSource {
	fn poll_frames(&mut self, now: Instant, out: &mut Vec<EncodedFrame>) {
		let elapsed = now.saturating_duration_since(self.start);
		let due_video = elapsed.as_micros() as u64 * u64::from(self.fps) / 1_000_000 + 1;
		let layered = self.layers.len() > 1 || self.layers[0].id != 0;
		while self.video_frames < due_video {
			let n = self.video_frames;
			let time = MediaTime::from_90khz(n * 90_000 / u64::from(self.fps));
			for layer in &self.layers {
				// This tick starts a frame period of the layer.
				let (fps, layer_fps) = (u64::from(self.fps), u64::from(layer.fps));
				if n > 0 && n * layer_fps / fps == (n - 1) * layer_fps / fps {
					continue;
				}
				let data = if layered {
					Self::layer_frame(n, layer.id, layer.size)
				} else {
					Self::video_frame(n, layer.size)
				};
				out.push(EncodedFrame {
					kind: MediaKind::Video,
					time,
					data: data.into(),
					layer: layer.id,
					keyframe: true,
				});
			}
			self.video_frames += 1;
		}
		if self.audio {
			let due_audio = elapsed.as_millis() as u64 / 20 + 1;
			while self.audio_frames < due_audio {
				let time =
					MediaTime::new(self.audio_frames * OPUS_FRAME, Frequency::FORTY_EIGHT_KHZ);
				let data = Arc::from(&OPUS_SILENCE[..]);
				out.push(EncodedFrame {
					kind: MediaKind::Audio,
					time,
					data,
					layer: 0,
					keyframe: false,
				});
				self.audio_frames += 1;
			}
		}
	}

	fn set_layer_bitrate(&mut self, layer: LayerId, bitrate: u64) {
		if let Some(l) = self.layers.iter_mut().find(|l| l.id == layer) {
			l.size = Self::frame_size(bitrate, l.fps);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn paced_frames() {
		let mut source = SyntheticSource::new(30, 3000, true);
		let start = source.start;
		let mut out = Vec::new();
		source.poll_frames(start, &mut out);
		assert_eq!(out.len(), 2, "one video and one audio frame at the start");
		out.clear();
		source.poll_frames(start + Duration::from_millis(1000), &mut out);
		let video: Vec<_> = out.iter().filter(|f| f.kind == MediaKind::Video).collect();
		let audio = out.len() - video.len();
		assert_eq!((video.len(), audio), (30, 50));
		assert_eq!(video[29].time, MediaTime::from_90khz(90_000));
		assert_eq!(video[0].data.len(), 3000);
		assert_eq!(video[0].data[..22], VP8_KEYFRAME_1X1);
		assert_eq!(video[0].data[22], 1);
	}

	#[test]
	fn layered_frames() {
		let full = LayerSpec { id: 3, ..LayerSpec::single(2_400_000) };
		let half = LayerSpec { id: 7, max_fps: Some(15), ..LayerSpec::single(600_000) };
		let mut source = SyntheticSource::with_layers(30, 1000, false, &[full, half]);
		let start = source.start;
		let mut out = Vec::new();
		source.poll_frames(start + Duration::from_millis(999), &mut out);
		let of = |id| out.iter().filter(move |f| f.layer == id);
		assert_eq!((of(3).count(), of(7).count()), (30, 15));
		assert!(out.iter().all(|f| f.keyframe && f.kind == MediaKind::Video));
		let f = of(7).nth(1).unwrap();
		assert_eq!(SyntheticSource::frame_layer(&f.data), Some(7));
		assert_eq!(f.data.len(), 600_000 / 8 / 15);
		assert_eq!(f.time, MediaTime::from_90khz(2 * 3000), "same clock as layer 3");
		assert_eq!(of(3).next().unwrap().data.len(), 2_400_000 / 8 / 30);
		source.set_layer_bitrate(3, 1);
		out.clear();
		source.poll_frames(start + Duration::from_millis(1030), &mut out);
		assert_eq!(out[0].data.len(), MIN_FRAME);
	}
}
