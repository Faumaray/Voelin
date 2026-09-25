//! Voice pipeline building blocks.
//!
//! - [`pcm`]: sample-rate constants, tone generation and detection helpers
//! - [`encode`]: Opus encoding of 20 ms frames into TeamSpeak voice packets
//! - [`framer`]: slice an arbitrary sample stream into codec frames
//! - [`resample`]: sample-rate conversion to the 48 kHz the codec uses
//! - [`wav`]: WAV file input/output
//! - [`Mixer`]: per-client jitter buffer, decoder and mixer (from the vendored
//!   `tsclientlib::audio::AudioHandler`)
//! - `device` (feature `device`): capture and playback through cpal

pub mod encode;
pub mod framer;
pub mod pcm;
pub mod resample;
pub mod wav;

#[cfg(feature = "device")]
pub mod device;

pub use encode::{VoiceCodec, VoiceEncoder};
pub use framer::Framer;
pub use tsclientlib::audio::AudioHandler;

/// Jitter buffer, decoder and mixer for incoming voice, keyed by sender.
pub type Mixer = AudioHandler<tsclientlib::ClientId>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("opus: {0}")]
	Opus(#[from] opus2::Error),
	#[error("wav: {0}")]
	Wav(#[from] hound::Error),
	#[error("resampling failed: {0}")]
	Resample(String),
	#[error("audio device: {0}")]
	Device(String),
	#[error("invalid input: {0}")]
	Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;
