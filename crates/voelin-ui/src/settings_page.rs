//! The settings page (audio, push-to-talk, H.264, crash reports), the
//! About page's notices, and the local volume of clients.

use slint::{ComponentHandle, SharedString};
use tracing::warn;
use voelin_core::settings::{AUDIO, CRASH_REPORTS};
use voelin_core::{Command, Source, VoiceState};
use voelin_platform::{crash, notices};

use crate::app::{App, AudioForm, Bridge, ClientAudio, later, model};
use crate::settings::{self, CLIENT_PLAYBACK, ClientPlayback, DeviceChoices};

impl App {
	pub(crate) fn open_settings(&mut self) {
		let list = |devices: voelin_audio::Result<Vec<voelin_audio::device::DeviceInfo>>,
		            input: bool| {
			let devices = devices.unwrap_or_else(|e| {
				warn!("cannot list audio devices: {e}");
				Vec::new()
			});
			devices
				.into_iter()
				.map(|d| {
					let default = if input { d.default_input } else { d.default_output };
					(d.id, d.name, default)
				})
				.collect::<Vec<_>>()
		};
		let inputs = list(voelin_audio::device::input_devices(), true);
		let outputs = list(voelin_audio::device::output_devices(), false);
		self.inputs = DeviceChoices::new(&inputs, self.audio.input_device.as_deref());
		self.outputs = DeviceChoices::new(&outputs, self.audio.output_device.as_deref());
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let names = |c: &DeviceChoices| model(c.names.iter().map(SharedString::from).collect());
		bridge.set_input_devices(names(&self.inputs));
		bridge.set_output_devices(names(&self.outputs));
		bridge.set_audio(settings::audio_form(&self.audio, &self.inputs, &self.outputs));
		bridge.set_global_ptt(self.settings.global_ptt);
		bridge.set_ptt_key(self.settings.ptt_key.clone().into());
		bridge.set_h264_enabled(self.settings.openh264);
		bridge.set_image_cache_usage(crate::images::usage_text().into());
		self.refresh_h264();
		self.refresh_identities();
	}

	pub(crate) fn close_settings(&mut self) {
		self.set_mic_test(false);
		self.save_audio();
	}

	pub(crate) fn audio_changed(&mut self, form: &AudioForm) {
		let audio = settings::apply_audio_form(form, &self.audio, &self.inputs, &self.outputs);
		if audio == self.audio {
			return;
		}
		self.audio = audio;
		self.audio_dirty = true;
		self.engine.send(Command::SetAudioSettings(Box::new(self.audio.clone())));
	}

	pub(crate) fn save_audio(&mut self) {
		if !self.audio_dirty {
			return;
		}
		self.audio_dirty = false;
		if let Err(e) = self.prefs.set(&AUDIO, self.audio.clone()) {
			warn!(%e, "could not store audio settings");
		}
	}

	pub(crate) fn set_mic_test(&mut self, on: bool) {
		if self.mic_test == on {
			return;
		}
		self.mic_test = on;
		self.engine.send(Command::TestMicrophone { on });
		if let Some(ui) = self.ui.upgrade() {
			let bridge = ui.global::<Bridge>();
			bridge.set_mic_test(on);
			if !on {
				bridge.set_input_level(-100.0);
			}
		}
	}

	pub(crate) fn apply_ptt(&mut self, enabled: bool, key: String) {
		let key = key.trim().to_owned();
		self.settings.global_ptt = enabled;
		if !key.is_empty() {
			self.settings.ptt_key = key.clone();
		}
		self.store_settings();
		self.ptt.set((enabled && !key.is_empty()).then_some(key));
	}

	pub(crate) fn set_ptt_status(&mut self, status: String) {
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_ptt_status(status.into());
		}
	}

	pub(crate) fn set_h264(&mut self, enabled: bool) {
		self.settings.openh264 = enabled;
		self.store_settings();
		self.video.set_h264(enabled);
		self.refresh_h264();
	}

	pub(crate) fn download_h264(&mut self) {
		let runtime = self.engine.runtime().clone();
		self.video.download_h264(&runtime, |download| {
			later(move |app| {
				app.video.downloaded(download, app.settings.openh264);
				app.refresh_h264();
			});
		});
		self.refresh_h264();
	}

	fn refresh_h264(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let (loaded, status) = self.video.h264_status();
		bridge.set_h264_loaded(loaded);
		bridge.set_h264_busy(status.starts_with("Downloading"));
		bridge.set_h264_status(status.into());
	}

	/// Switches the About page and the settings show.
	pub(crate) fn refresh_settings_flags(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		bridge.set_h264_enabled(self.settings.openh264);
		bridge.set_h264_attribution(notices::OPENH264_ATTRIBUTION.into());
		bridge.set_crash_reports(self.settings.crash_reports);
	}

	/// The transmit mode for the voice controls (Hold to talk shows for
	/// push-to-talk) before the settings page fills the whole form: only
	/// `transmit` changes, as the device lists are not read yet.
	pub(crate) fn refresh_transmit(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let mut form = bridge.get_audio();
		form.transmit = settings::transmit_index(self.audio.transmit);
		bridge.set_audio(form);
	}

	pub(crate) fn set_crash_reports(&mut self, enabled: bool) {
		self.settings.crash_reports = enabled;
		// Also in the blob, for older versions.
		self.store_settings();
		if let Err(e) = self.prefs.set(&CRASH_REPORTS, enabled) {
			warn!(%e, "could not store the crash report setting");
		}
		crash::set_enabled(enabled);
	}

	/// A notice while crash reports are on disk.
	pub(crate) fn refresh_crash_notice(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let text = match crash::pending_reports() {
			Ok(reports) if !reports.is_empty() => {
				let newest = &reports[reports.len() - 1];
				let saved = match reports.len() {
					1 => "1 crash report is saved".to_owned(),
					n => format!("{n} crash reports are saved"),
				};
				format!("The app crashed earlier ({}). {saved} on this device.", newest.summary)
			}
			_ => String::new(),
		};
		ui.global::<Bridge>().set_crash_notice(text.into());
	}

	pub(crate) fn open_crash_folder(&mut self) {
		let result = crash::dir().ok_or_else(|| "no crash report folder".to_owned());
		if let Err(e) =
			result.and_then(|dir| crate::app::open_path(&dir).map_err(|e| e.to_string()))
		{
			self.set_status(format!("Cannot open the crash report folder: {e}"));
		}
	}

	pub(crate) fn delete_crash_reports(&mut self) {
		match crash::clear() {
			Ok(n) => self.set_status(format!("Deleted {n} crash reports")),
			Err(e) => self.set_status(format!("Cannot delete the crash reports: {e}")),
		}
		self.refresh_crash_notice();
	}

	/// The third-party notices, one model entry per line.
	pub(crate) fn load_notices(&mut self) {
		if self.notices_loaded {
			return;
		}
		let Some(ui) = self.ui.upgrade() else { return };
		let lines: Vec<SharedString> = notices::text().lines().map(SharedString::from).collect();
		ui.global::<Bridge>().set_notices(model(lines));
		self.notices_loaded = true;
	}

	// Local volume and mute of clients.

	/// Fill the volume dialog for `client` of the current session.
	pub(crate) fn open_client(&mut self, client: u16) -> bool {
		let Some(id) = self.current else { return false };
		let Some(info) = self.view().and_then(|v| v.presence.clients.get(&client)).cloned() else {
			return false;
		};
		let playback =
			info.uid.as_ref().and_then(|uid| self.playback.get(uid)).copied().unwrap_or_default();
		self.playback_dialog = Some((id, client, info.uid.clone()));
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_client_audio(ClientAudio {
				id: client as i32,
				name: info.nickname.into(),
				volume: playback.volume * 100.0,
				muted: playback.muted,
				streaming: info.streaming == Some(true),
			});
		}
		true
	}

	pub(crate) fn client_playback_changed(&mut self, form: &ClientAudio) {
		let Some((session, client, uid)) = self.playback_dialog.clone() else { return };
		let playback =
			ClientPlayback { volume: (form.volume / 100.0).clamp(0.0, 4.0), muted: form.muted };
		let session = session as u64;
		self.engine.send(Command::SetClientVolume { session, client, volume: playback.volume });
		self.engine.send(Command::SetClientMuted { session, client, muted: playback.muted });
		// Kept by unique id; clients without one only for this connection.
		if let Some(uid) = uid {
			if playback.is_default() {
				self.playback.remove(&uid);
			} else {
				self.playback.insert(uid, playback);
			}
			if let Err(e) = self.prefs.set(&CLIENT_PLAYBACK, self.playback.clone()) {
				warn!(%e, "could not store client volumes");
			}
		}
		self.refresh_tree();
	}

	/// `VOELIN_OPEN=client`: the volume dialog of the first client that is not us.
	pub(crate) fn open_first_client(&mut self) {
		let Some(view) = self.view().filter(|v| v.state.own_client.is_some()) else { return };
		let own = view.state.own_client;
		let mut others: Vec<_> =
			view.presence.clients.values().filter(|c| Some(c.id) != own && !c.is_query).collect();
		others.sort_by_key(|c| c.id);
		let Some(id) = others.first().map(|c| c.id) else { return };
		self.open_client_pending = false;
		if self.open_client(id)
			&& let Some(ui) = self.ui.upgrade()
		{
			ui.global::<crate::app::Nav>().set_client_open(true);
		}
	}

	/// Send the stored volumes of clients that appeared in a voice session.
	pub(crate) fn apply_client_playback(&mut self, session: i64) {
		let Some(view) = self.sessions.get_mut(&session) else { return };
		if view.state.voice != VoiceState::Connected
			|| view.state.presence_source != Some(Source::Voice)
		{
			return;
		}
		let presence = view.presence.clone();
		view.applied_playback.retain(|id| presence.clients.contains_key(id));
		for client in presence.clients.values() {
			if !view.applied_playback.insert(client.id) {
				continue;
			}
			let stored = client.uid.as_ref().and_then(|uid| self.playback.get(uid));
			let Some(playback) = stored else { continue };
			let (s, c) = (session as u64, client.id);
			self.engine.send(Command::SetClientVolume {
				session: s,
				client: c,
				volume: playback.volume,
			});
			if playback.muted {
				self.engine.send(Command::SetClientMuted { session: s, client: c, muted: true });
			}
		}
	}
}
