//! Encoder and decoder factories found at runtime.
//!
//! - FFmpeg (feature `ffmpeg`, desktop): VA-API, NVENC, Quick Sync, AMF,
//!   Media Foundation and VideoToolbox encoders, plus software ones FFmpeg
//!   wraps (x264, OpenH264, SVT-AV1, rav1e, libaom); one factory per backend
//!   that passed its self-test ([`crate::ffmpeg::probe`]). Decoders likewise:
//!   VA-API, NVDEC, D3D11VA, DXVA2 and VideoToolbox, then dav1d and FFmpeg's
//!   own ([`crate::ffmpeg::decoder::probe`]).
//! - Android: MediaCodec ([`super::mediacodec`]).

use crate::Result;
use crate::codec::{
	Codec, DecoderBackend, EncoderBackend, EncoderConfig, VideoDecoder, VideoEncoder,
};

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

	/// Whether the automatic choice may use it (not for encoders with a long
	/// delay; those are used only when named in the settings).
	fn is_automatic(&self) -> bool {
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

/// Creates decoders of one backend for one codec: FFmpeg's, or one an
/// application adds ([`Codecs::with_decoder`](super::Codecs::with_decoder)).
pub trait DecoderFactory: Send + Sync {
	/// The backend its decoders are (for the ladder, logs and settings).
	fn backend(&self) -> DecoderBackend;

	fn codec(&self) -> Codec;

	/// The API or library behind it (`VA-API`, `dav1d`, ...), for the UI.
	fn api(&self) -> &'static str;

	/// A GPU decoder: left out when hardware decoding is off.
	fn is_hardware(&self) -> bool;

	fn create(&self) -> Result<Box<dyn VideoDecoder>>;
}

/// Decoder factories usable on this machine: FFmpeg's decoders that passed
/// their self-test, hardware first.
pub fn probe_decoders() -> Vec<Box<dyn DecoderFactory>> {
	#[allow(unused_mut)]
	let mut found: Vec<Box<dyn DecoderFactory>> = Vec::new();
	#[cfg(feature = "ffmpeg")]
	for factory in crate::ffmpeg::decoder::FfmpegDecoderFactory::available() {
		found.push(Box::new(factory));
	}
	found
}
