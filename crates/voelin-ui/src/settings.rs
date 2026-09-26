//! Settings kept in the store (besides bookmarks and identities), and their
//! conversion to and from the forms of the settings page.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use voelin_audio::process::NoiseLevel;
use voelin_core::{AudioSettings, TransmitMode};

use crate::app::AudioForm;

/// Store keys.
pub const AUDIO_KEY: &str = "audio";
pub const UI_KEY: &str = "ui";
pub const CLIENT_PLAYBACK_KEY: &str = "client_playback";

/// Frame rates and bitrates (kbit/s) of the share dialog.
pub const FPS_CHOICES: [u32; 3] = [15, 30, 60];
pub const BITRATE_CHOICES: [u32; 4] = [2500, 4608, 8000, 10_000];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
	/// Global push-to-talk hotkey (besides the button in the window).
	pub global_ptt: bool,
	/// E.g. `Ctrl+Shift+T` (`voelin_platform::Hotkey` syntax).
	pub ptt_key: String,
	/// H.264 through Cisco's OpenH264, downloaded on request.
	pub openh264: bool,
	/// The ScreenCast portal's token for the last shared screen.
	pub portal_restore_token: Option<String>,
	pub share: ShareDefaults,
	/// Write crash reports to disk (opt-in; never uploaded).
	pub crash_reports: bool,
}

impl Default for UiSettings {
	fn default() -> Self {
		Self {
			global_ptt: true,
			ptt_key: "Ctrl+Shift+T".into(),
			openh264: false,
			portal_restore_token: None,
			share: ShareDefaults::default(),
			crash_reports: false,
		}
	}
}

/// The last choices in the share dialog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareDefaults {
	pub fps_index: usize,
	pub bitrate_index: usize,
	pub audio: bool,
	pub auto_accept: bool,
}

impl Default for ShareDefaults {
	fn default() -> Self {
		Self { fps_index: 1, bitrate_index: 1, audio: true, auto_accept: false }
	}
}

/// Local playback of one client, kept by its unique id.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientPlayback {
	/// Linear, 1 = unchanged.
	pub volume: f32,
	pub muted: bool,
}

impl Default for ClientPlayback {
	fn default() -> Self {
		Self { volume: 1.0, muted: false }
	}
}

impl ClientPlayback {
	pub fn is_default(&self) -> bool {
		*self == Self::default()
	}
}

pub type ClientPlaybackMap = BTreeMap<String, ClientPlayback>;

/// Audio devices as listed in the settings: the system default first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceChoices {
	pub names: Vec<String>,
	/// Device ids by index; `None` for the system default.
	pub ids: Vec<Option<String>>,
}

impl DeviceChoices {
	/// `devices` as `(id, name, is_default)`. A selected device that is not
	/// connected stays in the list, so the choice survives.
	pub fn new(devices: &[(String, String, bool)], selected: Option<&str>) -> Self {
		let default_name = devices.iter().find(|d| d.2).map(|d| d.1.as_str());
		let mut choices = Self {
			names: vec![match default_name {
				Some(name) => format!("System default ({name})"),
				None => "System default".into(),
			}],
			ids: vec![None],
		};
		for (id, name, _) in devices {
			choices.names.push(name.clone());
			choices.ids.push(Some(id.clone()));
		}
		if let Some(id) = selected
			&& !choices.ids.iter().any(|i| i.as_deref() == Some(id))
		{
			choices.names.push(format!("{id} (not connected)"));
			choices.ids.push(Some(id.to_owned()));
		}
		choices
	}

	pub fn index_of(&self, id: Option<&str>) -> usize {
		self.ids.iter().position(|i| i.as_deref() == id).unwrap_or(0)
	}

	pub fn id_at(&self, index: i32) -> Option<String> {
		usize::try_from(index).ok().and_then(|i| self.ids.get(i).cloned()).flatten()
	}
}

fn noise_index(level: NoiseLevel) -> i32 {
	match level {
		NoiseLevel::Low => 0,
		NoiseLevel::Moderate => 1,
		NoiseLevel::High => 2,
		NoiseLevel::VeryHigh => 3,
	}
}

fn noise_level(index: i32) -> NoiseLevel {
	match index {
		0 => NoiseLevel::Low,
		2 => NoiseLevel::High,
		3 => NoiseLevel::VeryHigh,
		_ => NoiseLevel::Moderate,
	}
}

/// The settings page's form for `settings`.
pub fn audio_form(
	settings: &AudioSettings,
	inputs: &DeviceChoices,
	outputs: &DeviceChoices,
) -> AudioForm {
	let p = &settings.processing;
	AudioForm {
		input_index: inputs.index_of(settings.input_device.as_deref()) as i32,
		output_index: outputs.index_of(settings.output_device.as_deref()) as i32,
		transmit: match settings.transmit {
			TransmitMode::PushToTalk => 0,
			TransmitMode::VoiceActivation => 1,
			TransmitMode::Continuous => 2,
		},
		vad_threshold: settings.vad.threshold_db,
		echo_cancellation: p.echo_cancellation,
		noise_suppression: p.noise_suppression,
		noise_level: noise_index(p.noise_level),
		auto_gain: p.auto_gain,
		input_gain: p.input_gain_db,
		output_volume: settings.output_volume * 100.0,
		playback_buffer: settings.playback_buffer_ms as f32,
	}
}

/// `settings` changed as the form says; what the form does not show stays.
pub fn apply_audio_form(
	form: &AudioForm,
	settings: &AudioSettings,
	inputs: &DeviceChoices,
	outputs: &DeviceChoices,
) -> AudioSettings {
	let mut s = settings.clone();
	s.input_device = inputs.id_at(form.input_index);
	s.output_device = outputs.id_at(form.output_index);
	s.transmit = match form.transmit {
		1 => TransmitMode::VoiceActivation,
		2 => TransmitMode::Continuous,
		_ => TransmitMode::PushToTalk,
	};
	s.vad.threshold_db = form.vad_threshold;
	s.processing.echo_cancellation = form.echo_cancellation;
	s.processing.noise_suppression = form.noise_suppression;
	s.processing.noise_level = noise_level(form.noise_level);
	s.processing.auto_gain = form.auto_gain;
	s.processing.input_gain_db = form.input_gain;
	s.output_volume = (form.output_volume / 100.0).clamp(0.0, 4.0);
	s.playback_buffer_ms = form.playback_buffer.round().clamp(20.0, 180.0) as u32;
	s
}

#[cfg(test)]
mod tests {
	use super::*;

	fn devices() -> Vec<(String, String, bool)> {
		vec![
			("alsa:hw:0,0".into(), "Built-in".into(), true),
			("alsa:hw:1,0".into(), "Headset".into(), false),
		]
	}

	#[test]
	fn device_choices() {
		let choices = DeviceChoices::new(&devices(), None);
		assert_eq!(choices.names, ["System default (Built-in)", "Built-in", "Headset"]);
		assert_eq!(choices.index_of(Some("alsa:hw:1,0")), 2);
		assert_eq!(choices.id_at(0), None);
		assert_eq!(choices.id_at(2).as_deref(), Some("alsa:hw:1,0"));
		assert_eq!(choices.id_at(9), None);
		// An unplugged device stays selectable.
		let choices = DeviceChoices::new(&devices(), Some("usb:mic"));
		assert_eq!(choices.names.last().unwrap(), "usb:mic (not connected)");
		assert_eq!(choices.index_of(Some("usb:mic")), 3);
	}

	#[test]
	fn audio_form_roundtrip() {
		let inputs = DeviceChoices::new(&devices(), None);
		let outputs = DeviceChoices::new(&devices(), None);
		let mut settings = AudioSettings {
			transmit: TransmitMode::VoiceActivation,
			input_device: Some("alsa:hw:1,0".into()),
			output_volume: 0.5,
			playback_buffer_ms: 80,
			..Default::default()
		};
		settings.vad.threshold_db = -35.0;
		settings.processing.noise_level = NoiseLevel::High;
		settings.processing.input_gain_db = 6.0;
		let form = audio_form(&settings, &inputs, &outputs);
		assert_eq!((form.input_index, form.output_index, form.transmit), (2, 0, 1));
		assert_eq!(form.output_volume, 50.0);
		assert_eq!(apply_audio_form(&form, &settings, &inputs, &outputs), settings);

		let form =
			AudioForm { transmit: 2, echo_cancellation: false, playback_buffer: 500.0, ..form };
		let changed = apply_audio_form(&form, &settings, &inputs, &outputs);
		assert_eq!(changed.transmit, TransmitMode::Continuous);
		assert!(!changed.processing.echo_cancellation);
		assert_eq!(changed.playback_buffer_ms, 180);
	}

	#[test]
	fn ui_settings_defaults() {
		let parsed: UiSettings = serde_json::from_str(r#"{"openh264":true}"#).unwrap();
		assert!(parsed.openh264);
		assert!(!parsed.crash_reports, "crash reports are opt-in");
		assert_eq!(parsed.ptt_key, "Ctrl+Shift+T");
		assert_eq!(parsed.share, ShareDefaults::default());
		let map: ClientPlaybackMap =
			serde_json::from_str(r#"{"uid=":{"volume":0.5,"muted":false}}"#).unwrap();
		assert_eq!(map["uid="].volume, 0.5);
	}
}
