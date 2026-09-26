//! All user audio settings in one serde struct, e.g. for
//! `voelin_store::Store::set_setting("audio", &settings)`. Missing fields take
//! their defaults, so stored settings survive new options.

use serde::{Deserialize, Serialize};

use crate::process::ProcessingSettings;
use crate::vad::VadSettings;

/// When the microphone is sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransmitMode {
	/// While the push-to-talk key or button is held.
	#[default]
	PushToTalk,
	/// While the voice activity detector hears speech.
	VoiceActivation,
	/// Always (unless muted).
	Continuous,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
	pub transmit: TransmitMode,
	pub processing: ProcessingSettings,
	pub vad: VadSettings,
	/// Microphone by device id (`device::DeviceInfo::id`); `None` follows
	/// the system default.
	pub input_device: Option<String>,
	/// Speakers by device id; `None` follows the system default.
	pub output_device: Option<String>,
	/// Output volume for everyone, linear (1 = unchanged), applied before
	/// the echo canceller sees the signal.
	pub output_volume: f32,
	/// Audio kept queued for the speakers. Lower is snappier, higher
	/// survives scheduling hiccups.
	pub playback_buffer_ms: u32,
}

impl Default for AudioSettings {
	fn default() -> Self {
		Self {
			transmit: TransmitMode::PushToTalk,
			processing: ProcessingSettings::default(),
			vad: VadSettings::default(),
			input_device: None,
			output_device: None,
			output_volume: 1.0,
			playback_buffer_ms: 60,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn json_roundtrip_and_defaults() {
		let mut s = AudioSettings {
			transmit: TransmitMode::VoiceActivation,
			input_device: Some("alsa:hw:1,0".into()),
			..Default::default()
		};
		s.processing.echo_cancellation = false;
		s.vad.threshold_db = -35.0;
		let json = serde_json::to_string(&s).unwrap();
		assert_eq!(serde_json::from_str::<AudioSettings>(&json).unwrap(), s);

		// Older or partial settings fill in defaults.
		let partial: AudioSettings =
			serde_json::from_str(r#"{"transmit":"continuous","vad":{"hangover_ms":100}}"#).unwrap();
		assert_eq!(partial.transmit, TransmitMode::Continuous);
		assert_eq!(partial.vad.hangover_ms, 100);
		assert_eq!(partial.vad.threshold_db, VadSettings::default().threshold_db);
		assert_eq!(partial.processing, ProcessingSettings::default());
	}
}
