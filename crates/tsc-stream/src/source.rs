//! Where a streamer's encoded frames come from.
//!
//! [`FrameSource`] is the seam between capture/encoding (`tsc-media`) and the
//! stream sessions. [`SyntheticSource`] produces decodable placeholder media
//! without any encoder, for tests and `tsctl stream start --synthetic`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use str0m::media::{Frequency, MediaKind, MediaTime};

/// One encoded video or audio frame.
#[derive(Clone, Debug)]
pub struct EncodedFrame {
	pub kind: MediaKind,
	/// RTP time: 90 kHz for video, 48 kHz for Opus.
	pub time: MediaTime,
	pub data: Arc<[u8]>,
}

/// Produces encoded frames in real time.
pub trait FrameSource: Send {
	/// Append the frames that are due at `now`.
	fn poll_frames(&mut self, now: Instant, out: &mut Vec<EncodedFrame>);

	/// A viewer needs a keyframe soon.
	fn request_keyframe(&mut self) {}

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
pub struct SyntheticSource {
	start: Instant,
	fps: u32,
	video_size: usize,
	audio: bool,
	video_frames: u64,
	audio_frames: u64,
}

impl SyntheticSource {
	/// `fps` video frames of `video_size` bytes per second, plus audio if `audio`.
	pub fn new(fps: u32, video_size: usize, audio: bool) -> Self {
		Self {
			start: Instant::now(),
			fps: fps.max(1),
			video_size: video_size.max(VP8_KEYFRAME_1X1.len() + 1),
			audio,
			video_frames: 0,
			audio_frames: 0,
		}
	}

	pub fn video_frame(seq: u64, size: usize) -> Vec<u8> {
		let mut frame = VP8_KEYFRAME_1X1.to_vec();
		frame.push(seq as u8);
		frame.extend((frame.len()..size).map(|i| (i as u8).wrapping_mul(31) ^ seq as u8));
		frame
	}
}

impl FrameSource for SyntheticSource {
	fn poll_frames(&mut self, now: Instant, out: &mut Vec<EncodedFrame>) {
		let elapsed = now.saturating_duration_since(self.start);
		let due_video = elapsed.as_micros() as u64 * u64::from(self.fps) / 1_000_000 + 1;
		while self.video_frames < due_video {
			let n = self.video_frames;
			out.push(EncodedFrame {
				kind: MediaKind::Video,
				time: MediaTime::from_90khz(n * 90_000 / u64::from(self.fps)),
				data: Self::video_frame(n, self.video_size).into(),
			});
			self.video_frames += 1;
		}
		if self.audio {
			let due_audio = elapsed.as_millis() as u64 / 20 + 1;
			while self.audio_frames < due_audio {
				let time =
					MediaTime::new(self.audio_frames * OPUS_FRAME, Frequency::FORTY_EIGHT_KHZ);
				let data = Arc::from(&OPUS_SILENCE[..]);
				out.push(EncodedFrame { kind: MediaKind::Audio, time, data });
				self.audio_frames += 1;
			}
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
}
