//! Encoder factories found at runtime.
//!
//! - FFmpeg (feature `ffmpeg`, desktop): VA-API, NVENC, Quick Sync, AMF,
//!   Media Foundation and VideoToolbox encoders, plus software ones FFmpeg
//!   wraps (x264, OpenH264, SVT-AV1, rav1e, libaom); one factory per backend
//!   that passed its self-test ([`crate::ffmpeg::probe`]).
//! - Android: MediaCodec ([`super::mediacodec`]).

use crate::Result;
use crate::codec::{Codec, EncoderBackend, EncoderConfig, VideoEncoder};

/// Creates encoders of one backend.
pub trait EncoderFactory: Send + Sync {
	/// Short name for logs and settings (`"h264_vaapi"`, `"mediacodec"`).
	fn name(&self) -> &'static str;

	/// Codecs this backend encodes, best first.
	fn codecs(&self) -> Vec<Codec>;

	fn create(&self, codec: Codec, config: &EncoderConfig) -> Result<Box<dyn VideoEncoder>>;

	/// A GPU / OS encoder (the default), as opposed to a software encoder
	/// reached through the same API (FFmpeg's libx264, ...).
	fn is_hardware(&self) -> bool {
		true
	}

	/// The backend its encoders report.
	fn backend(&self) -> EncoderBackend {
		EncoderBackend::Hardware(self.name())
	}
}

/// Encoder factories usable on this machine: FFmpeg's backends that passed
/// their self-test, then MediaCodec (Android).
pub fn probe() -> Vec<Box<dyn EncoderFactory>> {
	#[allow(unused_mut)]
	let mut found: Vec<Box<dyn EncoderFactory>> = Vec::new();
	#[cfg(feature = "ffmpeg")]
	for factory in crate::ffmpeg::FfmpegFactory::available() {
		found.push(Box::new(factory));
	}
	#[cfg(target_os = "android")]
	if let Some(f) = super::mediacodec::probe() {
		found.push(Box::new(f));
	}
	found
}
