//! Media for TeamSpeak 6 streams: frames, pixel conversion, video codecs and
//! screen / system-audio capture.
//!
//! - [`frame`]: [`VideoFrame`] (I420, NV12, BGRA, RGBA) and [`AudioBuffer`]
//!   (interleaved `f32`, 48 kHz)
//! - [`convert`]: YUV ↔ RGB conversion (BT.601 limited range, the WebRTC
//!   default), e.g. RGBA for rendering and I420 for encoding
//! - [`codec`]: [`VideoEncoder`] / [`VideoDecoder`] traits, codec preference
//!   and the backends: libvpx (VP8/VP9, feature `vpx`), Cisco's OpenH264
//!   loaded at runtime (H.264, feature `openh264`), dav1d (AV1 decoding,
//!   feature `av1`)
//! - [`queue`]: the bounded, drop-oldest channel capture backends deliver on
//!
//! Encoded frames go to `tsc_stream::Peer::write` with an RTP time of
//! [`EncodedFrame::pts_90khz`] (90 kHz clock); received `MediaFrame`s go to
//! [`VideoDecoder::decode`].

pub mod codec;
pub mod convert;
pub mod frame;
pub mod queue;

pub use codec::{
	Codec, Codecs, ContentHint, EncodedFrame, EncoderBackend, EncoderConfig, VideoDecoder,
	VideoEncoder,
};
pub use frame::{AudioBuffer, FrameData, PixelFormat, Plane, VideoFrame};
pub use queue::{FrameReceiver, FrameSender, frame_channel};

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("invalid frame: {0}")]
	InvalidFrame(String),
	#[error("pixel conversion failed: {0}")]
	Convert(String),
	#[error("{codec} is not available: {reason}")]
	CodecUnavailable { codec: Codec, reason: String },
	#[error("{codec} encoder: {message}")]
	Encoder { codec: Codec, message: String },
	#[error("{codec} decoder: {message}")]
	Decoder { codec: Codec, message: String },
	#[error("{backend} capture is not available: {reason}")]
	CaptureUnavailable { backend: &'static str, reason: String },
	#[error("{backend} capture: {message}")]
	Capture { backend: &'static str, message: String },
	#[error("the user cancelled the capture")]
	Cancelled,
	#[error("download failed: {0}")]
	Download(String),
	#[error("I/O: {0}")]
	Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
