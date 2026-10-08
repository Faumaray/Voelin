//! The UI's settings keys (besides the engine's, in
//! `voelin_core::settings`), and their conversion to and from the forms of
//! the settings page.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use voelin_audio::process::NoiseLevel;
use voelin_core::settings::{Key, Kind};
use voelin_core::{AudioSettings, TransmitMode};

use crate::app::AudioForm;

/// The window's settings, one value (the `ui` blob of earlier versions).
pub static UI: Key<UiSettings> = Key::new(
	"ui",
	Kind::Json,
	"The window's settings: push-to-talk key, H.264, the share dialog's choices.",
	UiSettings::default,
);

/// Volume and mute of clients, by unique id.
pub static CLIENT_PLAYBACK: Key<ClientPlaybackMap> = Key::new(
	"client_playback",
	Kind::Json,
	"Volume and mute of clients, by unique id.",
	ClientPlaybackMap::new,
);

/// Colours of the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
	#[default]
	Dark,
	Light,
	/// The system's light or dark scheme.
	System,
}

impl ThemeChoice {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Dark => "dark",
			Self::Light => "light",
			Self::System => "system",
		}
	}

	pub fn parse(text: &str) -> Self {
		match text {
			"light" => Self::Light,
			"system" => Self::System,
			_ => Self::Dark,
		}
	}
}

/// `ui.theme`. (In a config file write `"ui.theme" = "light"` at the top:
/// a `[ui]` table would be read as the `ui` blob.)
pub static UI_THEME: Key<ThemeChoice> = Key::new(
	"ui.theme",
	Kind::Choice(&["dark", "light", "system"]),
	"Colours of the window: dark, light, or the system's scheme.",
	ThemeChoice::default,
);

fn positive_scale(v: &f32) -> Result<(), String> {
	if v.is_finite() && *v > 0.0 { Ok(()) } else { Err("must be a number above 0".into()) }
}

/// `ui.font_scale`: text size, 1 = normal (no upper limit).
pub static UI_FONT_SCALE: Key<f32> =
	Key::new("ui.font_scale", Kind::Json, "Text size as a factor (a number; 1 = normal).", || 1.0)
		.validated(positive_scale);

/// `ui.narrow_breakpoint`: below this window width (logical pixels) the
/// phone layout is used.
pub static UI_NARROW_BREAKPOINT: Key<u32> = Key::new(
	"ui.narrow_breakpoint",
	Kind::UInt { min: 0 },
	"Window width in pixels below which the phone layout is used.",
	|| 800,
);

/// `ui.image_cache_mb`: memory for decoded images (avatars, icons, emoji).
pub static UI_IMAGE_CACHE_MB: Key<u32> = Key::new(
	"ui.image_cache_mb",
	Kind::UInt { min: 0 },
	"Memory in MB for decoded images (avatars, icons, banners, emoji); 0 keeps none.",
	|| 256,
);

/// `ui.members_width`: how wide the members and streams panel is, in
/// pixels. Dragging its handle stores it; there is no built-in maximum.
pub static UI_MEMBERS_WIDTH: Key<u32> = Key::new(
	"ui.members_width",
	Kind::UInt { min: 0 },
	"Width of the members and streams panel in pixels.",
	|| 280,
);

/// `ui.voice_compact`: the voice channel view's smaller stage (a strip of
/// people and stream rows, more room for the chat). Its toggle stores it.
pub static UI_VOICE_COMPACT: Key<bool> = Key::new(
	"ui.voice_compact",
	Kind::Bool,
	"The voice channel view's smaller stage, leaving more room for the chat.",
	|| false,
);

/// The UI's keys besides [`UI`] and [`CLIENT_PLAYBACK`], for registering.
pub fn appearance_keys() -> [&'static dyn voelin_core::settings::Setting; 6] {
	[
		&UI_THEME,
		&UI_FONT_SCALE,
		&UI_NARROW_BREAKPOINT,
		&UI_IMAGE_CACHE_MB,
		&UI_MEMBERS_WIDTH,
		&UI_VOICE_COMPACT,
	]
}

/// What a kind of notification does: nothing, the bell, or the bell and a
/// desktop notification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyLevel {
	Off,
	App,
	#[default]
	Desktop,
}

impl NotifyLevel {
	pub fn index(self) -> i32 {
		self as i32
	}

	pub fn from_index(index: i32) -> Self {
		match index {
			0 => Self::Off,
			1 => Self::App,
			_ => Self::Desktop,
		}
	}
}

const NOTIFY_LEVELS: Kind = Kind::Choice(&["off", "app", "desktop"]);

/// `notify.*`: what the bell and the desktop say about each kind.
pub static NOTIFY_MENTIONS: Key<NotifyLevel> = Key::new(
	"notify.mentions",
	NOTIFY_LEVELS,
	"Our name in a channel or server chat: off, app (the bell) or desktop.",
	NotifyLevel::default,
);
pub static NOTIFY_MESSAGES: Key<NotifyLevel> = Key::new(
	"notify.private_messages",
	NOTIFY_LEVELS,
	"Private messages: off, app (the bell) or desktop.",
	NotifyLevel::default,
);
pub static NOTIFY_POKES: Key<NotifyLevel> =
	Key::new("notify.pokes", NOTIFY_LEVELS, "Pokes: off, app (the bell) or desktop.", || {
		NotifyLevel::Desktop
	});
pub static NOTIFY_EVENTS: Key<NotifyLevel> = Key::new(
	"notify.event_reminders",
	NOTIFY_LEVELS,
	"Reminders of scheduled events: off, app (the bell) or desktop.",
	NotifyLevel::default,
);
pub static NOTIFY_FRIENDS: Key<NotifyLevel> = Key::new(
	"notify.friends_online",
	NOTIFY_LEVELS,
	"Friends coming online: off, app (the bell) or desktop.",
	|| NotifyLevel::App,
);

/// `video.camera`: the camera of the preview and the default of camera
/// sources (a device id; empty: the first camera).
pub static VIDEO_CAMERA: Key<String> = Key::new(
	"video.camera",
	Kind::Text { suggestions: &[] },
	"Camera device (empty: the first one).",
	String::new,
);

/// `video.background`: the camera's background effect.
pub static VIDEO_BACKGROUND: Key<String> = Key::new(
	"video.background",
	Kind::Choice(&["none", "blur"]),
	"Background effect of the camera: none or blur.",
	|| "none".into(),
);

/// `video.resolution`: the camera's capture size, `auto` or `WIDTHxHEIGHT`.
pub static VIDEO_RESOLUTION: Key<String> = Key::new(
	"video.resolution",
	Kind::Text { suggestions: &["auto", "1280x720", "1920x1080", "2560x1440"] },
	"Camera resolution: auto or WIDTHxHEIGHT.",
	|| "auto".into(),
)
.validated(valid_resolution);

/// `video.mirror`: show our camera mirrored, as in a mirror.
pub static VIDEO_MIRROR: Key<bool> =
	Key::new("video.mirror", Kind::Bool, "Show our camera mirrored.", || true);

/// `ui.image_preview_kb`: linked pictures up to this size are downloaded
/// and shown in the chat (0: never). No maximum.
pub static UI_IMAGE_PREVIEW_KB: Key<u32> = Key::new(
	"ui.image_preview_kb",
	Kind::UInt { min: 0 },
	"Show pictures linked in chat up to this size in KB (0: never).",
	|| 8192,
);

#[allow(clippy::ptr_arg)] // a validation of `Key<String>` is `fn(&String)`
fn valid_resolution(text: &String) -> Result<(), String> {
	if text == "auto" || parse_size(text).is_some() {
		Ok(())
	} else {
		Err("auto or WIDTHxHEIGHT, e.g. 1280x720".into())
	}
}

/// `1280x720` → (1280, 720), both above 0.
pub fn parse_size(text: &str) -> Option<(u32, u32)> {
	let (w, h) = text.trim().split_once(['x', 'X', '×'])?;
	let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
	(w > 0 && h > 0).then_some((w, h))
}

/// The keys of the home, messages and settings pages, for registering.
pub fn page_keys() -> [&'static dyn voelin_core::settings::Setting; 10] {
	[
		&NOTIFY_MENTIONS,
		&NOTIFY_MESSAGES,
		&NOTIFY_POKES,
		&NOTIFY_EVENTS,
		&NOTIFY_FRIENDS,
		&VIDEO_CAMERA,
		&VIDEO_BACKGROUND,
		&VIDEO_RESOLUTION,
		&VIDEO_MIRROR,
		&UI_IMAGE_PREVIEW_KB,
	]
}

/// Frame rates and bitrates (kbit/s; 0: automatic) the share dialog
/// offers, in the order of its lists (`dialogs.slint`); any other value can
/// be typed in.
pub const FPS_CHOICES: [u32; 7] = [15, 30, 60, 120, 144, 240, 320];
pub const BITRATE_CHOICES: [u32; 8] = [0, 2500, 4608, 8000, 10_000, 20_000, 40_000, 60_000];

/// The share dialog's lists before they grew, which [`ShareDefaults`]
/// indexes for older versions.
pub const LEGACY_FPS_CHOICES: [u32; 3] = [15, 30, 60];
pub const LEGACY_BITRATE_CHOICES: [u32; 4] = [2500, 4608, 8000, 10_000];

/// The index of the choice closest to `value`.
pub fn nearest_choice(choices: &[u32], value: u32) -> usize {
	(0..choices.len()).min_by_key(|&i| choices[i].abs_diff(value)).unwrap_or(0)
}

/// A whole number above 0 typed into a form field.
pub fn parse_positive(text: &str) -> Option<u32> {
	text.trim().parse().ok().filter(|v| *v > 0)
}

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
	/// "Continue without an account" was chosen on the login page: it does
	/// not show at start any more (signing in clears it).
	pub skip_account_prompt: bool,
	/// The voice channel we were in last (Home's "Continue where you left
	/// off").
	pub last_voice: Option<LastVoice>,
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
			skip_account_prompt: false,
			last_voice: None,
		}
	}
}

/// Where we were with voice last: the bookmark with the address it had
/// then (one that now points elsewhere does not resume), and the channel's
/// path, its names from the top as the server has them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastVoice {
	pub bookmark: i64,
	pub address: String,
	pub channel: Vec<String>,
}

/// The last choices in the share dialog. Frame rate and bitrate are the
/// settings `stream.fps` and `stream.bitrate_kbps`; the indices of the
/// closest choices of the old lists ([`LEGACY_FPS_CHOICES`],
/// [`LEGACY_BITRATE_CHOICES`]) are kept for older versions.
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

/// The form's number for a transmit mode (`AudioForm.transmit`).
pub fn transmit_index(mode: TransmitMode) -> i32 {
	match mode {
		TransmitMode::PushToTalk => 0,
		TransmitMode::VoiceActivation => 1,
		TransmitMode::Continuous => 2,
	}
}

fn transmit_mode(index: i32) -> TransmitMode {
	match index {
		1 => TransmitMode::VoiceActivation,
		2 => TransmitMode::Continuous,
		_ => TransmitMode::PushToTalk,
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
		transmit: transmit_index(settings.transmit),
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
	s.transmit = transmit_mode(form.transmit);
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
	fn transmit_modes() {
		for (mode, index) in [
			(TransmitMode::PushToTalk, 0),
			(TransmitMode::VoiceActivation, 1),
			(TransmitMode::Continuous, 2),
		] {
			assert_eq!(transmit_index(mode), index);
			assert_eq!(transmit_mode(index), mode);
		}
		// Any other number is the default, push-to-talk (Hold to talk shows).
		assert_eq!(transmit_mode(7), TransmitMode::PushToTalk);
		assert_eq!(transmit_mode(-1), TransmitMode::PushToTalk);
		assert_eq!(transmit_index(AudioSettings::default().transmit), 0);
	}

	#[test]
	fn free_values() {
		assert_eq!(nearest_choice(&FPS_CHOICES, 60), 2);
		assert_eq!(nearest_choice(&FPS_CHOICES, 144), 4);
		assert_eq!(nearest_choice(&FPS_CHOICES, 1000), 6);
		assert_eq!(nearest_choice(&BITRATE_CHOICES, 0), 0, "automatic");
		assert_eq!(nearest_choice(&BITRATE_CHOICES, 3000), 1);
		assert_eq!(nearest_choice(&BITRATE_CHOICES, 55_000), 7);
		assert_eq!(nearest_choice(&BITRATE_CHOICES, 120_000), 7);
		assert_eq!(parse_positive(" 144 "), Some(144));
		assert_eq!(parse_positive("0"), None);
		assert_eq!(parse_positive("fast"), None);
	}

	#[test]
	fn page_keys() {
		assert_eq!(parse_size("1280x720"), Some((1280, 720)));
		assert_eq!(parse_size(" 1920 × 1080 "), Some((1920, 1080)));
		assert_eq!(parse_size("0x720"), None);
		assert_eq!(parse_size("big"), None);
		assert!(VIDEO_RESOLUTION.validate(&"auto".into()).is_ok());
		assert!(VIDEO_RESOLUTION.validate(&"wide".into()).is_err());
		assert_eq!(serde_json::to_value(NotifyLevel::App).unwrap(), "app");
		assert_eq!(NotifyLevel::from_index(NotifyLevel::Off.index()), NotifyLevel::Off);
		assert_eq!(NotifyLevel::from_index(7), NotifyLevel::Desktop);
	}

	#[test]
	fn voice_compact_key() {
		let settings = voelin_core::settings::Settings::in_memory();
		for key in appearance_keys() {
			settings.register(key);
		}
		assert_eq!(settings.get_json("ui.voice_compact"), Some(false.into()), "the full stage");
		assert!(settings.apply_overrides(["ui.voice_compact=true"]).is_empty());
		assert!(settings.get(&UI_VOICE_COMPACT));
		assert!(settings.set_json("ui.voice_compact", "small".into()).is_err());
	}

	#[test]
	fn ui_settings_defaults() {
		let parsed: UiSettings = serde_json::from_str(r#"{"openh264":true}"#).unwrap();
		assert!(parsed.openh264);
		assert!(!parsed.crash_reports, "crash reports are opt-in");
		assert_eq!(parsed.ptt_key, "Ctrl+Shift+T");
		assert_eq!(parsed.share, ShareDefaults::default());
		assert_eq!(parsed.last_voice, None, "older settings have no last channel");
		let last = LastVoice {
			bookmark: 3,
			address: "ts.example".into(),
			channel: vec!["Games".into(), "Chess".into()],
		};
		let settings = UiSettings { last_voice: Some(last.clone()), ..UiSettings::default() };
		let json = serde_json::to_string(&settings).unwrap();
		assert_eq!(serde_json::from_str::<UiSettings>(&json).unwrap().last_voice, Some(last));
		assert_eq!(serde_json::to_value(ThemeChoice::System).unwrap(), "system");
		assert_eq!(ThemeChoice::parse("light"), ThemeChoice::Light);
		assert_eq!(ThemeChoice::parse("??"), ThemeChoice::Dark);
		assert!(UI_FONT_SCALE.validate(&0.0).is_err());
		assert!(UI_FONT_SCALE.validate(&3.5).is_ok());
		let map: ClientPlaybackMap =
			serde_json::from_str(r#"{"uid=":{"volume":0.5,"muted":false}}"#).unwrap();
		assert_eq!(map["uid="].volume, 0.5);
	}
}
