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
//! - [`capture`]: [`ScreenCapture`] and [`AudioCapture`] with a synthetic
//!   test source, X11 (feature `x11`), the Wayland ScreenCast portal and
//!   PipeWire system audio (feature `pipewire`), and Windows Graphics Capture
//!   plus WASAPI loopback on Windows
//! - [`mix`]: the stream's audio mixer ([`mix::StreamMixer`]): any number
//!   of sources at their own pace and rate into 48 kHz stereo, with gains,
//!   mutes, a soft limiter and lock-free meters
//! - [`queue`]: the bounded, drop-oldest channel capture backends deliver on
//! - the streaming path: [`workers`] (a fork-join pool that allocates
//!   nothing per job), [`pool`] (recycled I420 frames), [`handoff`]
//!   (one-slot latest-wins handoff between threads), [`scale`] (plane
//!   scaling and the [`scale::Pyramid`] of simulcast sizes), and
//!   [`convert::Converter`] (borrowed capture buffers straight to I420)
//!
//! Encoded frames go to `voelin_stream::Peer::write` with an RTP time of
//! [`EncodedFrame::pts_90khz`] (90 kHz clock); received `MediaFrame`s go to
//! [`VideoDecoder::decode`].

pub mod capture;
pub mod codec;
pub mod convert;
pub mod frame;
pub mod handoff;
pub mod mix;
pub mod pool;
pub mod queue;
pub mod scale;
pub mod workers;

pub use capture::{AudioCapture, CaptureOptions, CaptureSource, ScreenCapture, SourceId};
pub use codec::{
	Codec, Codecs, ContentHint, EncodedChunk, EncodedFrame, EncoderBackend, EncoderConfig,
	VideoDecoder, VideoEncoder,
};
pub use frame::{
	AudioBuffer, FrameData, FrameRef, PixelFormat, PixelsRef, Plane, PlaneRef, VideoFrame,
};
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
	#[error("capture source not found: {0:?}")]
	SourceNotFound(SourceId),
	#[error("the user cancelled the capture")]
	Cancelled,
	#[error("download failed: {0}")]
	Download(String),
	#[error("I/O: {0}")]
	Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
