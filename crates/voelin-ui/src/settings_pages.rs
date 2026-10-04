//! The settings pages added with the new design: My Account, Profiles
//! (identities), the camera and stream quality of Voice & Video, Streaming,
//! Devices, Notifications, Privacy & Safety, Integrations (gateways,
//! gateway administration, encoders) and Advanced. Every control reads and
//! writes a setting (`voelin_core::settings`, `crate::settings`) and
//! applies at once; the forms are filled again when a setting changes
//! anywhere ([`App::page_setting_changed`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use slint::{ComponentHandle, Model, SharedString};
use tracing::warn;
use voelin_core::settings::{
	AudioSourceKindSetting, AudioSourceSetting, BlockMode, CACHE_FETCH_IMAGES, CACHE_MAX_MB,
	CHAT_DEDUPE_TOLERANCE_MS, CHAT_HISTORY_PAGE, CHAT_RETENTION_DAYS, CHAT_STORE_HISTORY,
	CaptureChoice, CodecChoice, FILES_PROGRESS_MS, Key, LayerSetting, PRIVACY_BLOCK_MODE,
	PRIVACY_POKES, PRIVACY_PRIVATE_MESSAGES, SRTP_PROFILES, STREAM_AUDIO_SOURCES,
	STREAM_BITRATE_KBPS, STREAM_CAPTURE_BACKEND, STREAM_CODEC, STREAM_DECODER_BACKEND,
	STREAM_ENCODER_BACKEND, STREAM_FPS, STREAM_HARDWARE_ACCELERATION, STREAM_HARDWARE_DECODING,
	STREAM_LAYERS, STREAM_PERMISSIONS, STREAM_SRTP_PROFILES, STUDIO_RECORDING_DIR,
	STUDIO_REPLAY_MEMORY_MB, STUDIO_REPLAY_SECONDS, Source as SettingSource, StreamPermissions,
};
use voelin_core::{Command, GatewayRequest};
use voelin_gateway_proto::{Action, ConfigSource, PermRule, UniqueIds, feature};

use crate::app::{
	AccountForm, AdvancedForm, App, AudioSourceItem, BookmarkIdentity, Bridge, ConfigItem,
	EncoderItem, FoundIdentity, GatewayItem, IdentityItem, LayerItem, NotifyForm, PermItem,
	PrivacyForm, SettingItem, StreamingForm, VideoForm, later, model,
};
use crate::settings::{
	IDENTITY_DEFAULT, NOTIFY_EVENTS, NOTIFY_FRIENDS, NOTIFY_MENTIONS, NOTIFY_MESSAGES,
	NOTIFY_POKES, NotifyLevel, UI_IMAGE_PREVIEW_KB, VIDEO_BACKGROUND, VIDEO_CAMERA, VIDEO_MIRROR,
	VIDEO_RESOLUTION, page_keys, parse_size,
};
use crate::social::{allowed_index, allowed_of};

/// Sizes of the stream quality presets (Auto: the source's size).
const PRESETS: [(u32, u32); 3] = [(1280, 720), (1920, 1080), (2560, 1440)];

/// An identity whose level is being raised.
pub(crate) struct Improving {
	pub target: u8,
	pub reached: u8,
	pub cancel: Arc<AtomicBool>,
}

/// State of the settings pages.
#[derive(Default)]
pub(crate) struct Pages {
	pub improving: HashMap<i64, Improving>,
	/// Identities found in other clients' files, picked for import.
	pub found: Vec<(voelin_core::identity::Found, bool)>,
	pub settings_filter: String,
	/// The gateway being administered.
	pub admin: Option<i64>,
	/// Camera ids by the index of the cameras list.
	pub cameras: Vec<String>,
	pub camera: Option<crate::camera::Preview>,
	pub encoders_loading: bool,
	/// Applications playing audio (the audio sources picker), while the
	/// Streaming page is open.
	pub audio_apps: Option<voelin_core::media::AudioApps>,
}

/// "1920x1080" or "0.5" for a layer.
fn layer_size(l: &LayerSetting) -> String {
	match l.size {
		Some((w, h)) => format!("{w}x{h}"),
		None => format!("{}", l.scale),
	}
}

/// kbit/s of bit/s, as typed.
fn kbps(bits: u64) -> String {
	(bits / 1000).to_string()
}

pub(crate) fn layer_item(l: &LayerSetting) -> LayerItem {
	LayerItem {
		size: layer_size(l).into(),
		fps: l.max_fps.map(|f| f.to_string()).unwrap_or_default().into(),
		bitrate: kbps(l.bitrate).into(),
		min_bitrate: kbps(l.min_bitrate).into(),
		max_bitrate: l.max_bitrate.map(kbps).unwrap_or_default().into(),
	}
}

/// A layer from what was typed; `None` when it is not a layer.
pub(crate) fn layer_of(item: &LayerItem, base: &LayerSetting) -> Option<LayerSetting> {
	let mut l = base.clone();
	match parse_size(&item.size) {
		Some(size) => l.size = Some(size),
		None => {
			l.size = None;
			l.scale = item.size.trim().parse().ok().filter(|s: &f32| *s > 0.0)?;
		}
	}
	let number = |t: &str| t.trim().parse::<u64>().ok();
	l.max_fps = number(&item.fps).map(|f| f as u32).filter(|f| *f > 0);
	l.bitrate = number(&item.bitrate).filter(|b| *b > 0)? * 1000;
	l.min_bitrate = number(&item.min_bitrate).unwrap_or(0) * 1000;
	l.max_bitrate = number(&item.max_bitrate).map(|b| b * 1000).filter(|m| *m >= l.bitrate);
	Some(l)
}

/// The preset of a list of layers: 0 Auto (none), 1..3 a preset size, -1
/// other layers.
pub(crate) fn quality_of(layers: &[LayerSetting]) -> i32 {
	match layers {
		[] => 0,
		[one] => PRESETS.iter().position(|p| one.size == Some(*p)).map_or(-1, |i| i as i32 + 1),
		_ => -1,
	}
}

fn source_item(s: &AudioSourceSetting, app_name: &str) -> AudioSourceItem {
	let (kind, name, detail) = match &s.kind {
		AudioSourceKindSetting::Desktop => {
			("desktop", format!("Desktop without {app_name}"), "Everything that plays".to_owned())
		}
		AudioSourceKindSetting::App { name, pid } => (
			"app",
			name.clone().unwrap_or_else(|| format!("Process {}", pid.unwrap_or_default())),
			"Application".to_owned(),
		),
		AudioSourceKindSetting::Window => {
			("window", "The shared window's app".to_owned(), "Where the system tells".to_owned())
		}
		AudioSourceKindSetting::Microphone => {
			("microphone", "Microphone".to_owned(), "After noise suppression".to_owned())
		}
		AudioSourceKindSetting::Synthetic { frequency } => {
			("synthetic", "Test tone".to_owned(), format!("{frequency} Hz"))
		}
	};
	AudioSourceItem {
		kind: kind.into(),
		name: name.into(),
		detail: detail.into(),
		gain: s.gain * 100.0,
		muted: s.muted,
	}
}

fn config_source(s: ConfigSource) -> &'static str {
	match s {
		ConfigSource::Db => "set at runtime",
		ConfigSource::Cli => "command line",
		ConfigSource::Env => "environment",
		ConfigSource::File => "file",
		ConfigSource::Default => "default",
		ConfigSource::Unknown => "?",
	}
}

fn rule_text(rule: &PermRule) -> String {
	let mut parts = Vec::new();
	if rule.everyone {
		parts.push("everyone".to_owned());
	}
	let list = |ids: &[u64]| ids.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
	if !rule.server_groups.is_empty() {
		parts.push(format!("server groups {}", list(&rule.server_groups)));
	}
	if !rule.channel_groups.is_empty() {
		parts.push(format!("channel groups {}", list(&rule.channel_groups)));
	}
	if parts.is_empty() { "nobody".into() } else { parts.join("; ") }
}

/// "6, 7" → [6, 7] (other words are left out).
pub(crate) fn group_ids(text: &str) -> Vec<u64> {
	text.split([',', ' ']).filter_map(|t| t.trim().parse().ok()).collect()
}

/// A value typed into a JSON field: JSON when it parses, else a string.
pub(crate) fn json_of(text: &str) -> serde_json::Value {
	serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_owned()))
}

impl App {
	/// Store a setting; a refusal shows in the status line.
	fn put<T>(&mut self, key: &'static Key<T>, value: T)
	where
		T: serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
	{
		if let Err(e) = self.prefs.set(key, value) {
			warn!(%e, "setting refused");
			self.set_status(e.to_string());
		}
	}

	/// A section of the settings was shown: load what it needs.
	pub(crate) fn settings_section(&mut self, section: i32) {
		for key in page_keys() {
			self.prefs.register(key);
		}
		match section {
			0 | 7 => {
				self.load_cameras();
				if section == 7 {
					self.load_capture_sources();
				}
			}
			2 => self.watch_audio_apps(),
			5 | 6 => self.refresh_identities(),
			9 => {
				if self.pages.encoders_loading
					|| self
						.ui
						.upgrade()
						.is_some_and(|ui| ui.global::<Bridge>().get_encoders().row_count() == 0)
				{
					self.refresh_encoders();
				}
				self.refresh_gateways();
			}
			10 => self.refresh_all_settings(),
			_ => {}
		}
		if section != 0 && section != 7 {
			self.stop_camera();
		}
		if section != 2 {
			self.pages.audio_apps = None;
		}
		self.refresh_pages();
	}

	/// Every form of the settings pages, from the settings.
	pub(crate) fn refresh_pages(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let p = &self.prefs;
		let layers = p.get(&STREAM_LAYERS);
		let camera = p.get(&VIDEO_CAMERA);
		bridge.set_video(VideoForm {
			camera: self.pages.cameras.iter().position(|c| *c == camera).unwrap_or(0) as i32,
			background: i32::from(p.get(&VIDEO_BACKGROUND) == "blur"),
			resolution: p.get(&VIDEO_RESOLUTION).into(),
			fps: p.get(&STREAM_FPS).to_string().into(),
			codec: codec_index(p.get(&STREAM_CODEC)),
			hardware: p.get(&STREAM_HARDWARE_ACCELERATION),
			mirror: p.get(&VIDEO_MIRROR),
			permissions: match p.get(&STREAM_PERMISSIONS) {
				StreamPermissions::Everyone => 0,
				StreamPermissions::Friends => 1,
				StreamPermissions::Channel => 2,
				StreamPermissions::Nobody => 3,
			},
			quality: quality_of(&layers),
			bitrate: p.get(&STREAM_BITRATE_KBPS) as f32,
			preview: self.pages.camera.is_some(),
		});
		bridge.set_streaming(StreamingForm {
			fps: p.get(&STREAM_FPS).to_string().into(),
			bitrate: p.get(&STREAM_BITRATE_KBPS).to_string().into(),
			codec: codec_index(p.get(&STREAM_CODEC)),
			capture: match p.get(&STREAM_CAPTURE_BACKEND) {
				CaptureChoice::Auto => 0,
				CaptureChoice::Portal => 1,
				CaptureChoice::Wlroots => 2,
				CaptureChoice::X11 => 3,
			},
			replay_seconds: p.get(&STUDIO_REPLAY_SECONDS).to_string().into(),
			replay_memory: p.get(&STUDIO_REPLAY_MEMORY_MB).to_string().into(),
			recording_dir: p.get(&STUDIO_RECORDING_DIR).into(),
		});
		crate::vm::list::sync(
			&self.models.layers,
			&layers.iter().map(layer_item).collect::<Vec<_>>(),
		);
		let sources: Vec<AudioSourceItem> = p
			.get(&STREAM_AUDIO_SOURCES)
			.iter()
			.map(|s| source_item(s, voelin_platform::APP_NAME))
			.collect();
		crate::vm::list::sync(&self.models.audio_sources, &sources);
		bridge.set_has_studio(true);
		bridge.set_notify(NotifyForm {
			mentions: p.get(&NOTIFY_MENTIONS).index(),
			pokes: p.get(&NOTIFY_POKES).index(),
			messages: p.get(&NOTIFY_MESSAGES).index(),
			events: p.get(&NOTIFY_EVENTS).index(),
			friends: p.get(&NOTIFY_FRIENDS).index(),
		});
		bridge.set_privacy(PrivacyForm {
			block_mode: i32::from(p.get(&PRIVACY_BLOCK_MODE) == BlockMode::Flag),
			messages: allowed_index(p.get(&PRIVACY_PRIVATE_MESSAGES)),
			pokes: allowed_index(p.get(&PRIVACY_POKES)),
		});
		let srtp = p.get(&STREAM_SRTP_PROFILES);
		bridge.set_srtp_profiles(model(srtp.iter().map(SharedString::from).collect()));
		bridge.set_srtp_unused(model(
			SRTP_PROFILES
				.iter()
				.filter(|n| !srtp.iter().any(|s| s == *n))
				.map(|n| SharedString::from(*n))
				.collect(),
		));
		bridge.set_advanced(AdvancedForm {
			encoder: p.get(&STREAM_ENCODER_BACKEND).into(),
			hardware: p.get(&STREAM_HARDWARE_ACCELERATION),
			decoder: p.get(&STREAM_DECODER_BACKEND).into(),
			hardware_decoding: p.get(&STREAM_HARDWARE_DECODING),
			store_history: p.get(&CHAT_STORE_HISTORY),
			history_page: p.get(&CHAT_HISTORY_PAGE).to_string().into(),
			dedupe_ms: p.get(&CHAT_DEDUPE_TOLERANCE_MS).to_string().into(),
			retention_days: p.get(&CHAT_RETENTION_DAYS).to_string().into(),
			cache_mb: p.get(&CACHE_MAX_MB).to_string().into(),
			fetch_images: p.get(&CACHE_FETCH_IMAGES),
			progress_ms: p.get(&FILES_PROGRESS_MS).to_string().into(),
			preview_kb: p.get(&UI_IMAGE_PREVIEW_KB).to_string().into(),
			logs: "Logs go to the terminal the app was started from (standard error); RUST_LOG=voelin_core=debug,voelin_ui=debug shows more.".into(),
			crash_dir: voelin_platform::crash::dir().map(|d| d.display().to_string()).unwrap_or_default().into(),
		});
		self.refresh_account();
	}

	/// A setting changed anywhere: fill the forms, apply what the engine
	/// does not read itself.
	pub(crate) fn page_setting_changed(&mut self, key: &str) {
		if key == STREAM_SRTP_PROFILES.name() {
			self.apply_srtp();
		}
		if [STREAM_HARDWARE_ACCELERATION.name(), STREAM_ENCODER_BACKEND.name(), STREAM_CODEC.name()]
			.contains(&key)
		{
			self.video.set_encoder_preference(
				voelin_core::media::encoder_preference(&self.prefs),
				voelin_core::media::configured_codec(&self.prefs),
			);
		}
		if key == IDENTITY_DEFAULT.name() {
			self.load_default_identity();
		}
		if key == VIDEO_CAMERA.name()
			|| key == VIDEO_BACKGROUND.name()
			|| key == VIDEO_RESOLUTION.name()
		{
			self.restart_camera();
		}
		self.refresh_pages();
		if key.starts_with("stream.")
			|| key.starts_with("privacy.")
			|| key.starts_with("chat.")
			|| key.starts_with("cache.")
		{
			self.refresh_all_settings();
		}
	}

	/// The SRTP profiles of the setting, for the engine (it does not read
	/// the setting itself).
	pub(crate) fn apply_srtp(&self) {
		let profiles: Vec<_> = self
			.prefs
			.get(&STREAM_SRTP_PROFILES)
			.iter()
			.filter_map(|n| voelin_core::stream::SrtpProfile::from_name(n))
			.collect();
		self.engine.send(Command::SetSrtpProfiles(profiles));
	}

	pub(crate) fn video_changed(&mut self, form: &crate::app::VideoForm) {
		if let Some(id) =
			usize::try_from(form.camera).ok().and_then(|i| self.pages.cameras.get(i)).cloned()
			&& self.prefs.get(&VIDEO_CAMERA) != id
		{
			self.put(&VIDEO_CAMERA, id);
		}
		let background = if form.background == 1 { "blur" } else { "none" };
		if self.prefs.get(&VIDEO_BACKGROUND) != background {
			self.put(&VIDEO_BACKGROUND, background.to_owned());
		}
		if self.prefs.get(&VIDEO_RESOLUTION) != form.resolution.as_str() {
			self.put(&VIDEO_RESOLUTION, form.resolution.to_string());
		}
		if let Some(fps) = crate::settings::parse_positive(&form.fps)
			&& self.prefs.get(&STREAM_FPS) != fps
		{
			self.put(&STREAM_FPS, fps);
		}
		let codec = codec_of(form.codec);
		if self.prefs.get(&STREAM_CODEC) != codec {
			self.put(&STREAM_CODEC, codec);
		}
		if self.prefs.get(&STREAM_HARDWARE_ACCELERATION) != form.hardware {
			self.put(&STREAM_HARDWARE_ACCELERATION, form.hardware);
		}
		if self.prefs.get(&VIDEO_MIRROR) != form.mirror {
			self.put(&VIDEO_MIRROR, form.mirror);
		}
		let permissions = match form.permissions {
			0 => StreamPermissions::Everyone,
			1 => StreamPermissions::Friends,
			3 => StreamPermissions::Nobody,
			_ => StreamPermissions::Channel,
		};
		if self.prefs.get(&STREAM_PERMISSIONS) != permissions {
			self.put(&STREAM_PERMISSIONS, permissions);
		}
		let bitrate = form.bitrate.round().max(1.0) as u32;
		if self.prefs.get(&STREAM_BITRATE_KBPS) != bitrate {
			self.put(&STREAM_BITRATE_KBPS, bitrate);
		}
		// A preset: one layer of that size at the bitrate; Auto: none.
		let layers = self.prefs.get(&STREAM_LAYERS);
		let wanted = match form.quality {
			0 => Some(Vec::new()),
			q @ 1..=3 => Some(vec![LayerSetting {
				size: Some(PRESETS[q as usize - 1]),
				bitrate: u64::from(bitrate) * 1000,
				..LayerSetting::default()
			}]),
			_ => None,
		};
		if let Some(wanted) = wanted.filter(|w| *w != layers) {
			self.put(&STREAM_LAYERS, wanted);
		}
		self.refresh_pages();
	}

	pub(crate) fn streaming_changed(&mut self, form: &StreamingForm) {
		if let Some(fps) = crate::settings::parse_positive(&form.fps) {
			self.put(&STREAM_FPS, fps);
		}
		if let Some(bitrate) = crate::settings::parse_positive(&form.bitrate) {
			self.put(&STREAM_BITRATE_KBPS, bitrate);
		}
		self.put(&STREAM_CODEC, codec_of(form.codec));
		self.put(
			&STREAM_CAPTURE_BACKEND,
			match form.capture {
				1 => CaptureChoice::Portal,
				2 => CaptureChoice::Wlroots,
				3 => CaptureChoice::X11,
				_ => CaptureChoice::Auto,
			},
		);
		if let Ok(seconds) = form.replay_seconds.trim().parse() {
			self.put(&STUDIO_REPLAY_SECONDS, seconds);
		}
		if let Ok(mb) = form.replay_memory.trim().parse() {
			self.put(&STUDIO_REPLAY_MEMORY_MB, mb);
		}
		self.put(&STUDIO_RECORDING_DIR, form.recording_dir.trim().to_owned());
		self.refresh_pages();
	}

	pub(crate) fn layer_changed(&mut self, index: i32, item: &LayerItem) {
		let mut layers = self.prefs.get(&STREAM_LAYERS);
		let Some(slot) = usize::try_from(index).ok().filter(|i| *i < layers.len()) else { return };
		match layer_of(item, &layers[slot]) {
			Some(layer) => {
				layers[slot] = layer;
				self.put(&STREAM_LAYERS, layers);
			}
			None => {
				self.set_status("A layer needs a size (1280x720) or scale (0.5) and a bitrate.")
			}
		}
		self.refresh_pages();
	}

	pub(crate) fn add_layer(&mut self) {
		let mut layers = self.prefs.get(&STREAM_LAYERS);
		let bitrate = u64::from(self.prefs.get(&STREAM_BITRATE_KBPS)) * 1000;
		// Each new layer half the size and a quarter of the bitrate.
		let scale = 0.5f32.powi(layers.len() as i32);
		layers.push(LayerSetting {
			scale,
			bitrate: (bitrate / 4u64.pow(layers.len() as u32)).max(100_000),
			..LayerSetting::default()
		});
		self.put(&STREAM_LAYERS, layers);
		self.refresh_pages();
	}

	pub(crate) fn remove_layer(&mut self, index: i32) {
		let mut layers = self.prefs.get(&STREAM_LAYERS);
		if let Some(i) = usize::try_from(index).ok().filter(|i| *i < layers.len()) {
			layers.remove(i);
			self.put(&STREAM_LAYERS, layers);
		}
		self.refresh_pages();
	}

	pub(crate) fn audio_source_changed(&mut self, index: i32, item: &AudioSourceItem) {
		let mut sources = self.prefs.get(&STREAM_AUDIO_SOURCES);
		if let Some(s) = usize::try_from(index).ok().and_then(|i| sources.get_mut(i)) {
			s.gain = (item.gain / 100.0).max(0.0);
			s.muted = item.muted;
			self.put(&STREAM_AUDIO_SOURCES, sources);
		}
		self.refresh_pages();
	}

	pub(crate) fn add_audio_source(&mut self, kind: &str, name: &str) {
		let kind = match kind {
			"desktop" => AudioSourceKindSetting::Desktop,
			"window" => AudioSourceKindSetting::Window,
			"microphone" => AudioSourceKindSetting::Microphone,
			"app" if !name.is_empty() => {
				AudioSourceKindSetting::App { name: Some(name.to_owned()), pid: None }
			}
			_ => return,
		};
		let mut sources = self.prefs.get(&STREAM_AUDIO_SOURCES);
		sources.push(AudioSourceSetting::new(kind));
		self.put(&STREAM_AUDIO_SOURCES, sources);
		self.refresh_pages();
	}

	pub(crate) fn remove_audio_source(&mut self, index: i32) {
		let mut sources = self.prefs.get(&STREAM_AUDIO_SOURCES);
		if let Some(i) = usize::try_from(index).ok().filter(|i| *i < sources.len()) {
			sources.remove(i);
			self.put(&STREAM_AUDIO_SOURCES, sources);
		}
		self.refresh_pages();
	}

	/// Follow the applications that play audio while the Streaming page is
	/// open (`tick` checks for changes).
	fn watch_audio_apps(&mut self) {
		if self.pages.audio_apps.is_none() && !self.demo_ui {
			match voelin_core::media::audio_apps() {
				Ok(apps) => self.pages.audio_apps = Some(apps),
				Err(e) => warn!("cannot list applications that play audio: {e}"),
			}
		}
		self.refresh_audio_apps();
	}

	pub(crate) fn refresh_audio_apps(&mut self) {
		let Some(apps) = self.pages.audio_apps.as_mut() else { return };
		let names: Vec<SharedString> = apps
			.current()
			.into_iter()
			.map(|a| SharedString::from(a.binary.unwrap_or(a.name)))
			.collect();
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_audio_apps(model(names));
		}
	}

	/// Called once a second: a changed list of audio applications.
	pub(crate) fn tick_pages(&mut self) {
		if self.pages.audio_apps.as_ref().is_some_and(|a| a.has_changed()) {
			self.refresh_audio_apps();
		}
	}

	pub(crate) fn notify_changed(&mut self, form: &NotifyForm) {
		self.put(&NOTIFY_MENTIONS, NotifyLevel::from_index(form.mentions));
		self.put(&NOTIFY_POKES, NotifyLevel::from_index(form.pokes));
		self.put(&NOTIFY_MESSAGES, NotifyLevel::from_index(form.messages));
		self.put(&NOTIFY_EVENTS, NotifyLevel::from_index(form.events));
		self.put(&NOTIFY_FRIENDS, NotifyLevel::from_index(form.friends));
		self.refresh_pages();
	}

	pub(crate) fn test_notification(&mut self) {
		let n =
			voelin_platform::Notification::new(voelin_platform::APP_NAME, "Notifications work.");
		self.engine.runtime().spawn(async move {
			let result = voelin_platform::notify(&n).await;
			later(move |app| {
				app.set_status(match result {
					Ok(()) => "A desktop notification was sent.".to_owned(),
					Err(e) => format!("No desktop notification: {e}"),
				});
			});
		});
	}

	pub(crate) fn privacy_changed(&mut self, form: &PrivacyForm) {
		self.put(
			&PRIVACY_BLOCK_MODE,
			if form.block_mode == 1 { BlockMode::Flag } else { BlockMode::Hide },
		);
		self.put(&PRIVACY_PRIVATE_MESSAGES, allowed_of(form.messages));
		self.put(&PRIVACY_POKES, allowed_of(form.pokes));
		self.refresh_pages();
	}

	pub(crate) fn advanced_changed(&mut self, form: &AdvancedForm) {
		let number = |t: &SharedString| t.trim().parse::<u64>().ok();
		if !form.encoder.trim().is_empty() {
			self.put(&STREAM_ENCODER_BACKEND, form.encoder.trim().to_owned());
		}
		self.put(&STREAM_HARDWARE_ACCELERATION, form.hardware);
		if !form.decoder.trim().is_empty() {
			self.put(&STREAM_DECODER_BACKEND, form.decoder.trim().to_owned());
		}
		self.put(&STREAM_HARDWARE_DECODING, form.hardware_decoding);
		self.put(&CHAT_STORE_HISTORY, form.store_history);
		if let Some(v) = number(&form.history_page) {
			self.put(&CHAT_HISTORY_PAGE, v as u32);
		}
		if let Some(v) = number(&form.dedupe_ms) {
			self.put(&CHAT_DEDUPE_TOLERANCE_MS, v);
		}
		if let Some(v) = number(&form.retention_days) {
			self.put(&CHAT_RETENTION_DAYS, v as u32);
		}
		if let Some(v) = number(&form.cache_mb) {
			self.put(&CACHE_MAX_MB, v);
		}
		self.put(&CACHE_FETCH_IMAGES, form.fetch_images);
		if let Some(v) = number(&form.progress_ms) {
			self.put(&FILES_PROGRESS_MS, v as u32);
		}
		if let Some(v) = number(&form.preview_kb) {
			self.put(&UI_IMAGE_PREVIEW_KB, v as u32);
		}
		self.refresh_pages();
	}

	/// Move an SRTP profile up (`-1`) or down (`1`).
	pub(crate) fn srtp_move(&mut self, index: i32, by: i32) {
		let mut list = self.prefs.get(&STREAM_SRTP_PROFILES);
		let (Ok(from), Ok(to)) = (usize::try_from(index), usize::try_from(index + by)) else {
			return;
		};
		if from < list.len() && to < list.len() {
			list.swap(from, to);
			self.put(&STREAM_SRTP_PROFILES, list);
		}
		self.refresh_pages();
	}

	/// Offer a profile, or stop offering it (one stays).
	pub(crate) fn srtp_toggle(&mut self, name: &str) {
		let mut list = self.prefs.get(&STREAM_SRTP_PROFILES);
		match list.iter().position(|p| p == name) {
			Some(i) if list.len() > 1 => {
				list.remove(i);
			}
			Some(_) => return,
			None => list.push(name.to_owned()),
		}
		self.put(&STREAM_SRTP_PROFILES, list);
		self.refresh_pages();
	}

	/// Every registered setting (Advanced), filtered.
	pub(crate) fn refresh_all_settings(&self) {
		let filter = self.pages.settings_filter.trim().to_lowercase();
		let items: Vec<SettingItem> = self
			.prefs
			.keys()
			.into_iter()
			.filter(|k| {
				filter.is_empty()
					|| k.name().contains(&filter)
					|| k.doc().to_lowercase().contains(&filter)
			})
			.map(|k| SettingItem {
				key: k.name().into(),
				doc: k.doc().into(),
				value: self
					.prefs
					.get_json(k.name())
					.map(|v| v.to_string())
					.unwrap_or_default()
					.into(),
				source: match self.prefs.source(k.name()) {
					Some(SettingSource::Runtime) => "set here",
					Some(SettingSource::Override) => "command line",
					Some(SettingSource::Config) => "config file",
					_ => "default",
				}
				.into(),
			})
			.collect();
		crate::vm::list::sync(&self.models.all_settings, &items);
	}

	pub(crate) fn search_settings(&mut self, text: String) {
		self.pages.settings_filter = text;
		self.refresh_all_settings();
	}

	/// Set a key from its JSON text; why not, when it is refused.
	pub(crate) fn setting_set(&mut self, key: &str, text: &str) -> String {
		match self.prefs.set_json(key, json_of(text)) {
			Ok(()) => {
				self.refresh_all_settings();
				String::new()
			}
			Err(e) => {
				let message = e.to_string();
				self.set_status(message.clone());
				message
			}
		}
	}

	pub(crate) fn setting_reset(&mut self, key: &str) {
		if let Err(e) = self.prefs.reset(key) {
			self.set_status(e.to_string());
		}
		self.refresh_all_settings();
		self.refresh_pages();
	}

	pub(crate) fn open_logs(&mut self) {
		self.open_crash_folder();
	}

	// Integrations.

	/// The gateways of our servers and the one being administered.
	pub(crate) fn refresh_gateways(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let items: Vec<GatewayItem> = self
			.bookmarks
			.iter()
			.filter_map(|b| {
				let url = b.gateway_url.clone()?;
				let view = self.sessions.get(&b.id);
				let caps = view.map(|v| v.gateway_caps.clone()).unwrap_or_default();
				let status = match view.map(|v| v.state.observe) {
					_ if !caps.is_empty() => "connected",
					Some(voelin_core::ObserveState::Connecting) => "connecting",
					_ => "off",
				};
				Some(GatewayItem {
					session: b.id as i32,
					server: b.name.clone().into(),
					url: url.into(),
					status: status.into(),
					admin: caps.iter().any(|c| c == feature::ADMIN),
					features: model(caps.into_iter().map(SharedString::from).collect()),
				})
			})
			.collect();
		bridge.set_gateways(model(items));
		bridge.set_admin_session(self.pages.admin.map_or(-1, |s| s as i32));
		let extra = self.pages.admin.and_then(|s| self.sessions.get(&s)).map(|v| &v.extra);
		let config: Vec<ConfigItem> = extra
			.map(|e| {
				e.config
					.iter()
					.map(|c| ConfigItem {
						key: c.key.clone().into(),
						value: c.value.to_string().into(),
						source: config_source(c.source).into(),
						description: c.description.clone().into(),
						kind: c.value_type.clone().into(),
						bootstrap: c.bootstrap,
					})
					.collect()
			})
			.unwrap_or_default();
		crate::vm::list::sync(&self.models.gateway_config, &config);
		let perms: Vec<PermItem> = extra
			.map(|e| {
				e.perms
					.iter()
					.map(|r| {
						let rule = r.rule.clone().unwrap_or_default();
						let list = |ids: &[u64]| {
							ids.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
						};
						PermItem {
							action: r.action.as_str().into(),
							rule: match &r.rule {
								Some(rule) => rule_text(rule),
								None => format!("Default: {}", r.default),
							}
							.into(),
							source: config_source(r.source).into(),
							everyone: rule.everyone,
							server_groups: list(&rule.server_groups).into(),
							channel_groups: list(&rule.channel_groups).into(),
						}
					})
					.collect()
			})
			.unwrap_or_default();
		crate::vm::list::sync(&self.models.gateway_perms, &perms);
	}

	/// Administer a gateway (`-1`: close).
	pub(crate) fn gateway_admin(&mut self, session: i32) {
		self.pages.admin = (session >= 0).then_some(i64::from(session));
		if let Some(id) = self.pages.admin {
			self.gateway_to(id, GatewayRequest::ConfigList);
			self.gateway_to(id, GatewayRequest::PermList);
		}
		self.refresh_gateways();
	}

	pub(crate) fn config_set(&mut self, key: &str, text: &str) {
		if let Some(id) = self.pages.admin {
			self.gateway_to(
				id,
				GatewayRequest::ConfigSet { key: key.to_owned(), value: json_of(text) },
			);
		}
	}

	pub(crate) fn config_reset(&mut self, key: &str) {
		if let Some(id) = self.pages.admin {
			self.gateway_to(id, GatewayRequest::ConfigReset { key: key.to_owned() });
		}
	}

	pub(crate) fn config_reload(&mut self) {
		if let Some(id) = self.pages.admin {
			self.gateway_to(id, GatewayRequest::ConfigReload);
		}
	}

	pub(crate) fn perm_set(&mut self, action: &str, everyone: bool, server: &str, channel: &str) {
		let (Some(id), Some(action)) = (self.pages.admin, Action::parse(action)) else { return };
		let rule = PermRule {
			everyone,
			server_groups: group_ids(server),
			channel_groups: group_ids(channel),
		};
		self.gateway_to(id, GatewayRequest::PermSet { action, rule });
		self.gateway_to(id, GatewayRequest::PermList);
	}

	pub(crate) fn perm_reset(&mut self, action: &str) {
		let (Some(id), Some(action)) = (self.pages.admin, Action::parse(action)) else { return };
		self.gateway_to(id, GatewayRequest::PermReset { action });
		self.gateway_to(id, GatewayRequest::PermList);
	}

	/// Probe the encoders off the UI thread (FFmpeg's probe can take a
	/// while).
	pub(crate) fn refresh_encoders(&mut self) {
		self.pages.encoders_loading = true;
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_encoders_loading(true);
		}
		let codecs = self.video.codecs();
		std::thread::spawn(move || {
			let report = codecs.report();
			later(move |app| app.encoders_ready(report));
		});
	}

	fn encoders_ready(&mut self, report: voelin_core::media::voelin_media::EncoderReport) {
		self.pages.encoders_loading = false;
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		bridge.set_encoders_loading(false);
		bridge.set_ffmpeg_status(
			match &report.ffmpeg {
				Ok(v) => format!("FFmpeg: {v}"),
				Err(e) => format!("FFmpeg is not used: {e}"),
			}
			.into(),
		);
		bridge.set_zero_copy_status(
			match &report.zero_copy {
				Ok(()) => "Screen capture goes to the GPU encoder without a copy.".to_owned(),
				Err(e) => format!("No zero-copy capture: {e}"),
			}
			.into(),
		);
		// A decoder's entry is an encoder's (its rank: the place in its
		// codec's ladder).
		let items = |list: &[voelin_core::media::voelin_media::EncoderInfo]| {
			let items: Vec<EncoderItem> = list
				.iter()
				.map(|e| EncoderItem {
					name: e.name.clone().into(),
					api: e.api.clone().into(),
					codec: format!("{:?}", e.codec).to_uppercase().into(),
					hardware: e.hardware,
					ok: e.status.is_ok(),
					status: match (&e.status, e.rank) {
						(Err(why), _) => why.clone(),
						(Ok(()), Some(0)) => format!("{} · used first", e.api),
						(Ok(()), Some(n)) => format!("{} · choice {}", e.api, n + 1),
						(Ok(()), None) => format!("{} · not used", e.api),
					}
					.into(),
				})
				.collect();
			model(items)
		};
		bridge.set_encoders(items(&report.encoders));
		bridge.set_decoders(items(&report.decoders));
	}

	/// The screens and windows that can be shared (Devices).
	fn load_capture_sources(&mut self) {
		let names: Vec<SharedString> = match self.video.sources(false, false) {
			Ok(list) => list
				.into_iter()
				.map(|(n, d)| {
					SharedString::from(if d.is_empty() { n } else { format!("{n} · {d}") })
				})
				.collect(),
			Err(e) => vec![e.into()],
		};
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_capture_sources(model(names));
		}
	}

	// Identities (My Account, Profiles).

	/// The identity used unless a bookmark names another: `identity.default`,
	/// else the first.
	pub(crate) fn load_default_identity(&mut self) {
		let wanted = self.prefs.get(&IDENTITY_DEFAULT) as i64;
		let entries = self.store.identities().unwrap_or_default();
		let id = entries.iter().find(|e| e.id == wanted).or(entries.first()).map(|e| e.id);
		if let Some(identity) = id.and_then(|id| self.store.identity(id).ok()) {
			self.identity = identity;
		}
		self.load_own_uids();
	}

	/// Our unique ids (every identity, both server generations), to tell
	/// our own messages.
	pub(crate) fn load_own_uids(&mut self) {
		let mut uids = std::collections::HashSet::new();
		for e in self.store.identities().unwrap_or_default() {
			if let Ok(identity) = self.store.identity(e.id) {
				let ids = UniqueIds::from_omega(&identity.key().to_pub().to_ts());
				uids.insert(ids.ts3);
				uids.insert(ids.ts6);
			}
		}
		self.social.own_uids = uids;
	}

	/// The identity a bookmark connects with.
	pub(crate) fn identity_for(&self, bookmark: Option<i64>) -> tsclientlib::Identity {
		bookmark
			.and_then(|b| self.bookmark(b))
			.and_then(|b| b.identity)
			.and_then(|id| self.store.identity(id).ok())
			.unwrap_or_else(|| self.identity.clone())
	}

	fn default_identity_id(&self) -> Option<i64> {
		let entries = self.store.identities().unwrap_or_default();
		let wanted = self.prefs.get(&IDENTITY_DEFAULT) as i64;
		entries.iter().find(|e| e.id == wanted).or(entries.first()).map(|e| e.id)
	}

	pub(crate) fn refresh_account(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let bookmark = self.current.and_then(|id| self.bookmark(id));
		let identity = self.identity_for(self.current);
		let ids = UniqueIds::from_omega(&identity.key().to_pub().to_ts());
		let entry_id = bookmark.and_then(|b| b.identity).or_else(|| self.default_identity_id());
		let name = self
			.store
			.identities()
			.unwrap_or_default()
			.into_iter()
			.find(|e| Some(e.id) == entry_id)
			.map(|e| e.name)
			.unwrap_or_default();
		bridge.set_account(AccountForm {
			nickname: bookmark.map(|b| b.nickname.clone()).unwrap_or_default().into(),
			identity: name.into(),
			uid: ids.ts3.into(),
			uid6: ids.ts6.into(),
			level: i32::from(identity.level()),
			server: bookmark.map(|b| b.name.clone()).unwrap_or_default().into(),
		});
	}

	/// The identities, the bookmarks' choices, what an import found.
	pub(crate) fn refresh_identities(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let entries = self.store.identities().unwrap_or_default();
		let default = self.default_identity_id();
		let items: Vec<IdentityItem> = entries
			.iter()
			.map(|e| {
				let identity = self.store.identity(e.id).ok();
				let ids =
					identity.as_ref().map(|i| UniqueIds::from_omega(&i.key().to_pub().to_ts()));
				let improving = self.pages.improving.get(&e.id);
				let used: Vec<&str> = self
					.bookmarks
					.iter()
					.filter(|b| {
						b.identity == Some(e.id) || (b.identity.is_none() && Some(e.id) == default)
					})
					.map(|b| b.name.as_str())
					.collect();
				IdentityItem {
					id: e.id as i32,
					name: e.name.clone().into(),
					uid: e.uid.clone().into(),
					uid6: ids.map(|i| i.ts6).unwrap_or_default().into(),
					level: i32::from(identity.map_or(e.level, |i| i.level())),
					default: Some(e.id) == default,
					improving: improving.map_or(0, |i| i32::from(i.target)),
					reached: improving.map_or(0, |i| i32::from(i.reached)),
					used_by: used.join(", ").into(),
				}
			})
			.collect();
		crate::vm::list::sync(&self.models.identities, &items);
		let mut names = vec![SharedString::from("Default identity")];
		names.extend(entries.iter().map(|e| SharedString::from(e.name.as_str())));
		bridge.set_identity_names(model(names));
		let choices: Vec<BookmarkIdentity> = self
			.bookmarks
			.iter()
			.map(|b| BookmarkIdentity {
				id: b.id as i32,
				name: b.name.clone().into(),
				identity: b
					.identity
					.and_then(|id| entries.iter().position(|e| e.id == id))
					.map_or(0, |i| i as i32 + 1),
			})
			.collect();
		bridge.set_bookmark_identities(model(choices));
		let known: std::collections::HashSet<&str> =
			entries.iter().map(|e| e.uid.as_str()).collect();
		let found: Vec<FoundIdentity> = self
			.pages
			.found
			.iter()
			.map(|(f, picked)| {
				let uid = f.uid();
				FoundIdentity {
					name: f.nickname.clone().into(),
					known: known.contains(uid.as_str()),
					uid: uid.into(),
					level: i32::from(f.level()),
					source: f.source.display().to_string().into(),
					picked: *picked,
				}
			})
			.collect();
		bridge.set_found_identities(model(found));
		self.refresh_account();
	}

	/// "default", "delete", "export", "stop".
	pub(crate) fn identity_action(&mut self, id: i32, action: &str) {
		let id = i64::from(id);
		match action {
			"default" => self.put(&IDENTITY_DEFAULT, id as u64),
			"delete" => {
				if self.default_identity_id() == Some(id) {
					self.set_status(
						"The default identity cannot be deleted; make another one the default first.",
					);
					return;
				}
				if let Err(e) = self.store.delete_identity(id) {
					self.set_status(format!("Could not delete the identity: {e}"));
				}
				self.load_own_uids();
			}
			"export" => self.export_identity(id),
			"stop" => {
				if let Some(i) = self.pages.improving.remove(&id) {
					i.cancel.store(true, Ordering::Relaxed);
				}
			}
			_ => {}
		}
		self.refresh_identities();
	}

	fn export_identity(&mut self, id: i64) {
		let entries = self.store.identities().unwrap_or_default();
		let (Some(entry), Ok(identity)) =
			(entries.iter().find(|e| e.id == id), self.store.identity(id))
		else {
			return;
		};
		let dir = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| ".".into());
		let file = entry
			.name
			.chars()
			.map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
			.collect::<String>();
		let path = dir.join(format!("{file}.ini"));
		match voelin_core::identity::write_export(&path, &entry.name, &identity) {
			Ok(()) => self.set_identity_status(format!(
				"Exported to {} (it holds the private key: keep it safe).",
				path.display()
			)),
			Err(e) => self.set_identity_status(format!("Could not export: {e}")),
		}
	}

	fn set_identity_status(&mut self, text: String) {
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_identity_status(text.clone().into());
		}
		self.set_status(text);
	}

	pub(crate) fn rename_identity(&mut self, id: i32, name: &str) {
		let name = name.trim();
		if name.is_empty() {
			return;
		}
		if let Err(e) = self.store.rename_identity(i64::from(id), name) {
			self.set_status(format!("Could not rename: {e}"));
		}
		self.refresh_identities();
	}

	/// Raise an identity's level to `target` on every core, in the
	/// background.
	pub(crate) fn improve_identity(&mut self, id: i32, target: i32) {
		let id = i64::from(id);
		let Ok(identity) = self.store.identity(id) else { return };
		let Ok(target) = u8::try_from(target) else { return };
		if target <= identity.level() || self.pages.improving.contains_key(&id) {
			self.set_identity_status(format!(
				"The identity is at level {} already.",
				identity.level()
			));
			return;
		}
		let cancel = Arc::new(AtomicBool::new(false));
		self.pages
			.improving
			.insert(id, Improving { target, reached: identity.level(), cancel: cancel.clone() });
		std::thread::Builder::new()
			.name("identity-level".into())
			.spawn(move || {
				let better =
					voelin_core::identity::improve_level(&identity, target, &cancel, |level| {
						later(move |app| {
							if let Some(i) = app.pages.improving.get_mut(&id) {
								i.reached = i.reached.max(level);
							}
							app.refresh_identities();
						});
					});
				later(move |app| app.level_reached(id, better));
			})
			.ok();
		self.refresh_identities();
	}

	fn level_reached(&mut self, id: i64, better: Option<tsclientlib::Identity>) {
		self.pages.improving.remove(&id);
		if let Some(identity) = better {
			match self.store.update_identity(id, &identity) {
				Ok(()) => {
					self.set_identity_status(format!(
						"Security level {} reached.",
						identity.level()
					));
					if self.default_identity_id() == Some(id) {
						self.identity = identity;
					}
				}
				Err(e) => self.set_identity_status(format!("Could not store the identity: {e}")),
			}
		}
		self.refresh_identities();
	}

	/// Look for identities in the official clients' files.
	pub(crate) fn find_identities(&mut self) {
		let mut found = Vec::new();
		let mut errors = Vec::new();
		for path in voelin_core::identity::discover() {
			match voelin_core::identity::read(&path) {
				Ok(list) => found.extend(list),
				Err(e) => errors.push(e.to_string()),
			}
		}
		let known: std::collections::HashSet<String> =
			self.store.identities().unwrap_or_default().into_iter().map(|e| e.uid).collect();
		let status = match (found.len(), errors.first()) {
			(0, Some(e)) => format!("Nothing found ({e})."),
			(0, None) => "No TeamSpeak client settings found on this device.".to_owned(),
			(n, _) => format!("{n} identities found."),
		};
		self.pages.found = found
			.into_iter()
			.map(|f| {
				let new = !known.contains(&f.uid());
				(f, new)
			})
			.collect();
		self.set_identity_status(status);
		self.refresh_identities();
	}

	pub(crate) fn pick_found(&mut self, index: i32, on: bool) {
		if let Some(f) = usize::try_from(index).ok().and_then(|i| self.pages.found.get_mut(i)) {
			f.1 = on;
		}
		self.refresh_identities();
	}

	pub(crate) fn import_identities(&mut self) {
		let picked: Vec<_> =
			self.pages.found.iter().filter(|(_, p)| *p).map(|(f, _)| f.clone()).collect();
		match voelin_core::identity::import(&self.store, &picked) {
			Ok(results) => {
				let added = results
					.iter()
					.filter(|r| matches!(r, voelin_core::identity::Imported::Added(_)))
					.count();
				self.set_identity_status(format!("{added} identities imported."));
			}
			Err(e) => self.set_identity_status(format!("Could not import: {e}")),
		}
		self.pages.found.clear();
		self.load_own_uids();
		self.refresh_identities();
	}

	/// The identity a bookmark connects with (0: the default).
	pub(crate) fn bookmark_identity(&mut self, bookmark: i32, index: i32) {
		let entries = self.store.identities().unwrap_or_default();
		let identity = usize::try_from(index - 1).ok().and_then(|i| entries.get(i)).map(|e| e.id);
		let Some(mut b) = self.bookmark(i64::from(bookmark)).cloned() else { return };
		b.identity = identity;
		if let Err(e) = self.store.update_bookmark(&b) {
			self.set_status(format!("Could not save: {e}"));
			return;
		}
		self.bookmarks = self.store.bookmarks().unwrap_or_default();
		self.refresh_identities();
	}

	// Camera (Voice & Video, Devices).

	fn load_cameras(&mut self) {
		let mut cameras = crate::camera::list();
		// Sample data never opens a real camera.
		if self.demo_ui {
			cameras.retain(|(id, _)| id == "synthetic");
		}
		self.pages.cameras = cameras.iter().map(|(id, _)| id.clone()).collect();
		let names: Vec<SharedString> =
			cameras.into_iter().map(|(_, n)| SharedString::from(n)).collect();
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_cameras(model(names));
		}
	}

	pub(crate) fn toggle_camera_preview(&mut self) {
		if self.pages.camera.is_some() {
			self.stop_camera();
		} else {
			self.start_camera();
		}
		self.refresh_pages();
	}

	pub(crate) fn start_camera(&mut self) {
		let device =
			if self.demo_ui { "synthetic".to_owned() } else { self.prefs.get(&VIDEO_CAMERA) };
		let size = parse_size(&self.prefs.get(&VIDEO_RESOLUTION));
		let blur = self.prefs.get(&VIDEO_BACKGROUND) == "blur";
		let mirror = self.prefs.get(&VIDEO_MIRROR);
		self.pages.camera =
			Some(crate::camera::Preview::start(self.engine.runtime(), &device, size, blur, mirror));
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_camera_status("Starting the camera…".into());
		}
	}

	pub(crate) fn stop_camera(&mut self) {
		if self.pages.camera.take().is_some()
			&& let Some(ui) = self.ui.upgrade()
		{
			let bridge = ui.global::<Bridge>();
			bridge.set_camera_frame(slint::Image::default());
			bridge.set_camera_status(SharedString::new());
		}
	}

	fn restart_camera(&mut self) {
		if self.pages.camera.is_some() {
			self.stop_camera();
			self.start_camera();
		}
	}

	/// The camera preview's newest picture (called by its timer).
	pub(crate) fn camera_frame(&mut self) {
		let Some(preview) = &self.pages.camera else { return };
		let (frame, status) = (preview.take_picture(), preview.status());
		if let Some(ui) = self.ui.upgrade() {
			let bridge = ui.global::<Bridge>();
			if let Some(frame) = frame {
				bridge.set_camera_frame(frame);
			}
			bridge.set_camera_status(status.into());
		}
	}

	/// Play a short tone on the speakers (the Test Sound button).
	pub(crate) fn test_sound(&mut self) {
		let device = self.audio.output_device.clone();
		let volume = self.audio.output_volume;
		std::thread::spawn(move || {
			let result = crate::camera::play_tone(device.as_deref(), volume);
			if let Err(e) = result {
				later(move |app| app.set_status(format!("Cannot play the test sound: {e}")));
			}
		});
	}
}

fn codec_index(c: CodecChoice) -> i32 {
	match c {
		CodecChoice::Auto => 0,
		CodecChoice::Vp8 => 1,
		CodecChoice::Vp9 => 2,
		CodecChoice::H264 => 3,
		CodecChoice::Av1 => 4,
	}
}

fn codec_of(i: i32) -> CodecChoice {
	match i {
		1 => CodecChoice::Vp8,
		2 => CodecChoice::Vp9,
		3 => CodecChoice::H264,
		4 => CodecChoice::Av1,
		_ => CodecChoice::Auto,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn layers_round_trip() {
		let base = LayerSetting::default();
		let item = LayerItem {
			size: "1280x720".into(),
			fps: "30".into(),
			bitrate: "2500".into(),
			min_bitrate: "800".into(),
			max_bitrate: "".into(),
		};
		let layer = layer_of(&item, &base).unwrap();
		assert_eq!(
			(layer.size, layer.max_fps, layer.bitrate),
			(Some((1280, 720)), Some(30), 2_500_000)
		);
		assert_eq!(layer.min_bitrate, 800_000);
		assert_eq!(layer_item(&layer), item);
		let scaled = layer_of(
			&LayerItem { size: "0.5".into(), bitrate: "900".into(), ..item.clone() },
			&base,
		)
		.unwrap();
		assert_eq!((scaled.size, scaled.scale), (None, 0.5));
		assert!(layer_of(&LayerItem { size: "big".into(), ..item.clone() }, &base).is_none());
		assert!(layer_of(&LayerItem { bitrate: "0".into(), ..item }, &base).is_none());
	}

	#[test]
	fn quality_presets() {
		assert_eq!(quality_of(&[]), 0);
		let layer = |size| LayerSetting { size: Some(size), ..LayerSetting::default() };
		assert_eq!(quality_of(&[layer((1920, 1080))]), 2);
		assert_eq!(quality_of(&[layer((800, 600))]), -1);
		assert_eq!(quality_of(&[layer((1280, 720)), layer((640, 360))]), -1);
		for i in 0..5 {
			assert_eq!(codec_index(codec_of(i)), i);
		}
	}

	#[test]
	fn typed_values() {
		assert_eq!(group_ids("6, 7 x 9"), [6, 7, 9]);
		assert_eq!(json_of("30"), serde_json::json!(30));
		assert_eq!(json_of("[1,2]"), serde_json::json!([1, 2]));
		assert_eq!(json_of("hello"), serde_json::json!("hello"));
		let rule = PermRule { everyone: false, server_groups: vec![6], channel_groups: vec![] };
		assert_eq!(rule_text(&rule), "server groups 6");
		assert_eq!(rule_text(&PermRule::default()), "nobody");
	}
}
