//! Voice pipeline building blocks.
//!
//! - [`pcm`]: sample-rate constants, tone generation and detection helpers
//! - [`encode`]: Opus encoding of 20 ms frames into TeamSpeak voice packets
//! - [`framer`]: slice an arbitrary sample stream into codec frames
//! - [`resample`]: sample-rate conversion to the 48 kHz the codec uses
//! - [`wav`]: WAV file input/output
//! - [`process`]: echo cancellation, noise suppression and gain control
//! - [`vad`]: voice activity detection
//! - [`mixer`]: per-client jitter buffer, decoder, volume and mixing (on the
//!   vendored `tsclientlib::audio::AudioHandler`)
//! - [`settings`]: all user audio settings in one serde struct
//! - `device` (feature `device`): capture and playback through cpal, device
//!   lists and loss detection

pub mod encode;
pub mod framer;
pub mod mixer;
pub mod pcm;
pub mod process;
pub mod resample;
pub mod settings;
pub mod vad;
pub mod wav;

#[cfg(feature = "device")]
pub mod device;

pub use encode::{VoiceCodec, VoiceEncoder};
pub use framer::Framer;
pub use mixer::Mixer;
pub use process::{ProcessingSettings, Processor};
pub use settings::AudioSettings;
pub use tsclientlib::audio::AudioHandler;
pub use vad::{Vad, VadSettings};

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
