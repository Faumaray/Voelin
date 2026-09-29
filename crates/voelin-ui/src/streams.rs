//! Streams (TeamSpeak 6): the streams panel, sharing our screen and the
//! viewer. The engine runs the stream sessions; `video.rs` captures,
//! encodes and decodes.

use slint::{ComponentHandle, Model};
use tracing::warn;
use voelin_core::media::audio_source_specs;
use voelin_core::settings::{STREAM_AUDIO_SOURCES, STREAM_BITRATE_KBPS, STREAM_FPS};
use voelin_core::stream::{EndReason, LeaveReason, StreamSetup, ViewerInfo, ViewerState};
use voelin_core::{Command, Event, StreamState, WatchState};

use crate::app::{App, Bridge, ShareForm, SourceItem, StreamItem, ViewerItem, later, model};
use crate::settings::{
	BITRATE_CHOICES, FPS_CHOICES, ShareDefaults, nearest_choice, parse_positive,
};
use crate::video::{self, Capture, CaptureRequest, Decoder};

/// Our stream.
pub(crate) struct Share {
	pub session: i64,
	/// Stops capturing when dropped.
	capture: Capture,
	/// The server confirmed the stream.
	live: bool,
	viewers: Vec<ViewerInfo>,
	/// The capture ended by itself and the stream is being stopped.
	stopping: bool,
}

/// The stream in the viewer.
pub(crate) struct Watch {
	/// `None` for the local demo stream.
	session: Option<i64>,
	stream_id: String,
	title: String,
	streamer: String,
	/// Shown over the picture; empty while it plays.
	status: String,
	connected: bool,
	ended: bool,
	has_frame: bool,
	/// Shown instead of the chat.
	shown: bool,
	decoder: Option<Decoder>,
}

fn end_text(reason: &EndReason, streamer: &str) -> String {
	match reason {
		EndReason::Local => "You left the stream.".into(),
		EndReason::Denied => format!("{streamer} did not let you watch."),
		EndReason::Stopped => "The stream has ended.".into(),
		EndReason::Removed(Some(LeaveReason::Kicked | LeaveReason::Banned)) => {
			format!("{streamer} removed you from the stream.")
		}
		EndReason::Removed(_) => "You were removed from the stream.".into(),
		EndReason::Failed(e) => format!("The stream failed: {e}"),
	}
}

fn share_end_text(reason: &EndReason) -> String {
	match reason {
		EndReason::Failed(e) => format!("Sharing failed: {e}"),
		_ => "Sharing ended.".into(),
	}
}

impl App {
	pub(crate) fn stream_event(&mut self, event: Event) {
		match event {
			Event::StreamsChanged { session, streams } => {
				let view = self.sessions.entry(session as i64).or_default();
				// Development switch: watch the first stream that shows up.
				let autowatch = std::env::var("VOELIN_AUTOWATCH").is_ok_and(|v| v == "1")
					&& self.watch.is_none()
					&& self.current == Some(session as i64);
				let first = streams
					.iter()
					.find(|s| view.state.own_client != Some(s.streamer.0))
					.map(|s| s.id.clone());
				view.streams = streams;
				self.refresh_streams();
				self.refresh_servers();
				if autowatch && let Some(id) = first {
					self.watch_stream(id);
				}
			}
			Event::StreamState { session, state } => self.share_state(session as i64, state),
			Event::StreamViewerRequest { session, viewer, .. } => {
				let name = self.sessions.entry(session as i64).or_default().nickname(viewer);
				self.set_status(format!("{name} wants to watch your stream"));
			}
			Event::StreamViewers { session, viewers } => {
				if let Some(share) = self.share.as_mut().filter(|s| s.session == session as i64) {
					share.viewers = viewers;
					self.refresh_streams();
				}
			}
			Event::WatchState { session, stream_id, state } => {
				self.watch_state(session as i64, &stream_id, state);
			}
			// Also flagged on the sink, which the encoder reads.
			Event::StreamKeyframeRequest { .. } => {}
			_ => {}
		}
	}

	fn share_state(&mut self, session: i64, state: StreamState) {
		let Some(share) = self.share.as_mut().filter(|s| s.session == session) else { return };
		match state {
			StreamState::Starting => {}
			StreamState::Live { sink, .. } => {
				share.live = true;
				share.capture.attach(sink);
				self.set_status("You are sharing your screen");
			}
			StreamState::Ended(reason) => {
				let local = reason == EndReason::Local || share.stopping;
				self.share = None;
				self.share_error = if local { String::new() } else { share_end_text(&reason) };
				self.set_status(if local {
					"You stopped sharing".into()
				} else {
					share_end_text(&reason)
				});
			}
		}
		self.refresh_streams();
	}

	fn watch_state(&mut self, session: i64, stream_id: &str, state: WatchState) {
		let Some(watch) = self
			.watch
			.as_mut()
			.filter(|w| w.session == Some(session) && w.stream_id == stream_id && !w.ended)
		else {
			return;
		};
		match state {
			WatchState::Requested => {
				watch.status = format!("Waiting for {} to let you watch…", watch.streamer);
			}
			WatchState::Connecting => {
				watch.status = format!("Connecting to {}…", watch.streamer);
			}
			WatchState::Connected => {
				watch.connected = true;
				if !watch.has_frame {
					watch.status = "Waiting for the picture…".into();
				}
			}
			WatchState::Ended(reason) => {
				watch.ended = true;
				watch.decoder = None;
				watch.status = end_text(&reason, &watch.streamer);
			}
		}
		self.refresh_viewer();
		self.refresh_streams();
	}

	/// The streams panel, the share dialog's live part and the toolbar.
	pub(crate) fn refresh_streams(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let view = self.view();
		let current = self.current;
		let mut items: Vec<StreamItem> = Vec::new();
		if let Some(view) = view.filter(|v| v.streams_available()) {
			for s in &view.streams {
				let watching = self
					.watch
					.as_ref()
					.is_some_and(|w| w.session == current && w.stream_id == s.id && !w.ended);
				items.push(StreamItem {
					id: s.id.clone().into(),
					name: s.name.clone().into(),
					streamer: view.nickname(s.streamer.0).into(),
					audio: s.audio,
					watching,
					own: view.state.own_client == Some(s.streamer.0),
				});
			}
			// Streams that started before we joined: the server did not
			// announce them, and the engine is looking them up (they arrive
			// in `StreamsChanged` like the others). Until then only the
			// streaming flag is known.
			let channel = view.state.own_channel;
			for c in view.presence.clients.values() {
				let announced = view.streams.iter().any(|s| s.streamer.0 == c.id);
				if c.streaming == Some(true)
					&& Some(c.channel) == channel
					&& !announced && view.state.own_client != Some(c.id)
				{
					items.push(StreamItem {
						name: c.nickname.clone().into(),
						streamer: c.nickname.clone().into(),
						..StreamItem::default()
					});
				}
			}
		}
		if self.demo {
			items.push(StreamItem {
				id: "demo".into(),
				name: "Test pattern".into(),
				streamer: "local preview".into(),
				audio: false,
				watching: self.watch.as_ref().is_some_and(|w| w.session.is_none() && !w.ended),
				own: false,
			});
		}
		bridge.set_streams_available(self.demo || view.is_some_and(|v| v.streams_available()));
		crate::vm::list::sync(&self.models.streams, &items);
		bridge.set_can_share(video::AVAILABLE);

		let share = self.share.as_ref().filter(|s| Some(s.session) == current);
		let state = match share {
			Some(s) if s.live => "live",
			Some(_) => "starting",
			None if self.share_busy => "starting",
			None => "",
		};
		bridge.set_share_state(state.into());
		bridge.set_share_busy(self.share_busy);
		bridge.set_share_error(self.share_error.clone().into());
		let viewers: Vec<ViewerItem> = share
			.map(|s| {
				s.viewers
					.iter()
					.map(|v| ViewerItem {
						client: i32::from(v.client.0),
						name: view
							.map_or_else(String::new, |view| view.nickname(v.client.0))
							.into(),
						state: match v.state {
							ViewerState::Requested => "requested",
							ViewerState::Connecting => "connecting",
							ViewerState::Connected => "connected",
						}
						.into(),
						message: v.message.clone().into(),
					})
					.collect()
			})
			.unwrap_or_default();
		let requests = viewers.iter().filter(|v| v.state == "requested").count();
		bridge.set_share_requests(requests as i32);
		crate::vm::list::sync(&self.models.viewers, &viewers);
		let status = match share {
			Some(s) if s.live => s.capture.status(),
			Some(s) => format!("Starting the stream of {}…", s.capture.source_name()),
			None if self.share_busy => "Starting the capture…".into(),
			None => String::new(),
		};
		bridge.set_share_status(status.into());
	}

	fn refresh_viewer(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let Some(watch) = &self.watch else {
			bridge.set_viewer_open(false);
			bridge.set_viewer_has_frame(false);
			bridge.set_viewer_frame(slint::Image::default());
			return;
		};
		bridge.set_viewer_open(
			watch.shown && (watch.session.is_none() || watch.session == self.current),
		);
		bridge.set_viewer_title(watch.title.clone().into());
		bridge.set_viewer_status(watch.status.clone().into());
		bridge.set_viewer_ended(watch.ended);
		bridge.set_viewer_has_frame(watch.has_frame);
		bridge.set_viewer_volume(self.stream_volume);
		let info = watch.decoder.as_ref().map(Decoder::info).unwrap_or_default();
		let info = [format!("by {}", watch.streamer), info]
			.into_iter()
			.filter(|s| !s.is_empty())
			.collect::<Vec<_>>()
			.join(" · ");
		bridge.set_viewer_info(info.into());
	}

	/// Once a second: statistics, a capture that ended, decoder problems.
	pub(crate) fn tick_streams(&mut self) {
		if let Some(share) = &mut self.share
			&& share.live
			&& !share.stopping
			&& share.capture.ended()
		{
			share.stopping = true;
			self.engine.send(Command::StopStream { session: share.session as u64 });
		}
		if let Some(watch) = &mut self.watch
			&& watch.connected
			&& !watch.has_frame
			&& let Some(error) = watch.decoder.as_ref().and_then(Decoder::error)
		{
			watch.status = if error.contains("H264") || error.contains("OpenH264") {
				format!("{error}. Enable H.264 in Settings → Streaming to watch this stream.")
			} else {
				format!("Cannot show the picture: {error}")
			};
		}
		if self.share.is_some() {
			self.refresh_streams();
		}
		if self.watch.is_some() {
			self.refresh_viewer();
		}
	}

	// Sharing.

	/// Fill the share dialog: sources and the last choices.
	pub(crate) fn open_share(&mut self) -> ShareForm {
		let test_pattern = cfg!(debug_assertions)
			|| self.demo
			|| std::env::var("VOELIN_TEST_PATTERN").is_ok_and(|v| v == "1");
		let sources = match self.video.sources(test_pattern) {
			Ok(sources) => {
				if self.share.is_none() {
					self.share_error.clear();
				}
				sources
			}
			Err(e) => {
				self.share_error = e;
				Vec::new()
			}
		};
		if let Some(ui) = self.ui.upgrade() {
			let items: Vec<SourceItem> = sources
				.into_iter()
				.map(|(name, detail)| SourceItem { name: name.into(), detail: detail.into() })
				.collect();
			ui.global::<Bridge>().set_share_sources(model(items));
		}
		self.refresh_streams();
		let defaults = &self.settings.share;
		let nickname = self.current.and_then(|id| self.bookmark(id)).map(|b| b.nickname.clone());
		let (fps, bitrate) = (self.prefs.get(&STREAM_FPS), self.prefs.get(&STREAM_BITRATE_KBPS));
		ShareForm {
			source: 0,
			name: match nickname {
				Some(nick) if !nick.is_empty() => format!("{nick}'s screen").into(),
				_ => "Screen".into(),
			},
			fps_index: nearest_choice(&FPS_CHOICES, fps) as i32,
			fps: fps.to_string().into(),
			bitrate_index: nearest_choice(&BITRATE_CHOICES, bitrate) as i32,
			bitrate: bitrate.to_string().into(),
			audio: defaults.audio,
			auto_accept: defaults.auto_accept,
		}
	}

	/// Development switch `VOELIN_AUTOSHARE`: share the test pattern once.
	pub(crate) fn autoshare(&mut self) {
		if !self.autoshare || !self.view().is_some_and(|v| v.streams_available()) {
			return;
		}
		self.autoshare = false;
		let mut form = self.open_share();
		let Some(ui) = self.ui.upgrade() else { return };
		let sources = ui.global::<Bridge>().get_share_sources();
		let pattern = (0..sources.row_count())
			.find(|&i| sources.row_data(i).is_some_and(|s| s.name == "Test pattern"));
		let Some(index) = pattern else { return };
		form.source = index as i32;
		form.auto_accept = true;
		self.start_share(form);
	}

	pub(crate) fn start_share(&mut self, form: ShareForm) {
		let Some(session) = self.current else { return };
		if self.share_busy || self.share.is_some() {
			return;
		}
		// Typed values (no maximum), else the chosen presets.
		let choice = |i: i32, choices: &[u32]| {
			choices[usize::try_from(i).unwrap_or(0).min(choices.len() - 1)]
		};
		let fps = parse_positive(&form.fps).unwrap_or_else(|| choice(form.fps_index, &FPS_CHOICES));
		let bitrate = parse_positive(&form.bitrate)
			.unwrap_or_else(|| choice(form.bitrate_index, &BITRATE_CHOICES));
		// The next share starts from these.
		for result in
			[self.prefs.set(&STREAM_FPS, fps), self.prefs.set(&STREAM_BITRATE_KBPS, bitrate)]
		{
			if let Err(e) = result {
				warn!(%e, "could not store the share settings");
			}
		}
		let defaults = ShareDefaults {
			fps_index: nearest_choice(&FPS_CHOICES, fps),
			bitrate_index: nearest_choice(&BITRATE_CHOICES, bitrate),
			audio: form.audio,
			auto_accept: form.auto_accept,
		};
		// No sources configured: no audio.
		let audio_sources = audio_source_specs(&self.prefs.get(&STREAM_AUDIO_SOURCES));
		let request = CaptureRequest {
			fps,
			bitrate_kbps: bitrate,
			audio: form.audio && !audio_sources.is_empty(),
			audio_sources,
			restore_token: self.settings.portal_restore_token.clone(),
		};
		self.settings.share = defaults;
		self.store_settings();
		self.share_busy = true;
		self.share_error.clear();
		self.refresh_streams();
		let setup = StreamSetup {
			name: form.name.trim().to_owned(),
			bitrate,
			audio: form.audio,
			..StreamSetup::default()
		};
		let auto_accept = form.auto_accept;
		let runtime = self.engine.runtime().clone();
		let source = usize::try_from(form.source).unwrap_or(0);
		self.video.start_capture(&runtime, source, request, move |result| {
			later(move |app| app.capture_ready(session, setup, auto_accept, result));
		});
	}

	/// The capture started (or not): go live.
	fn capture_ready(
		&mut self,
		session: i64,
		mut setup: StreamSetup,
		auto_accept: bool,
		result: Result<Capture, String>,
	) {
		self.share_busy = false;
		match result {
			Err(e) => {
				self.share_error = format!("Could not start the capture: {e}");
			}
			Ok(capture) => {
				if let Some(token) = capture.restore_token()
					&& self.settings.portal_restore_token.as_ref() != Some(&token)
				{
					self.settings.portal_restore_token = Some(token);
					self.store_settings();
				}
				if setup.audio && !capture.has_audio() {
					setup.audio = false;
					let reason = capture.audio_error().unwrap_or_default();
					self.set_status(format!("Sharing without sound: {reason}"));
				}
				self.engine.send(Command::StartStream {
					session: session as u64,
					setup,
					auto_accept,
				});
				self.share = Some(Share {
					session,
					capture,
					live: false,
					viewers: Vec::new(),
					stopping: false,
				});
			}
		}
		self.refresh_streams();
	}

	pub(crate) fn stop_share(&mut self) {
		if let Some(share) = &mut self.share {
			share.stopping = true;
			self.engine.send(Command::StopStream { session: share.session as u64 });
		}
	}

	pub(crate) fn respond_viewer(&mut self, viewer: u16, accept: bool) {
		if let Some(share) = &self.share {
			let session = share.session as u64;
			self.engine.send(Command::AcceptViewer { session, viewer, accept });
		}
	}

	pub(crate) fn kick_viewer(&mut self, viewer: u16) {
		if let Some(share) = &self.share {
			self.engine.send(Command::KickViewer { session: share.session as u64, viewer });
		}
	}

	// Watching.

	pub(crate) fn watch_stream(&mut self, stream_id: String) {
		if stream_id == "demo" && self.demo {
			self.start_demo();
			return;
		}
		let Some(session) = self.current else { return };
		let Some(view) = self.sessions.get(&session) else { return };
		let Some(info) = view.streams.iter().find(|s| s.id == stream_id) else { return };
		let streamer = view.nickname(info.streamer.0);
		let title =
			if info.name.is_empty() { format!("{streamer}'s stream") } else { info.name.clone() };
		self.leave_stream();
		self.engine
			.send(Command::WatchStream { session: session as u64, stream_id: stream_id.clone() });
		if self.stream_volume != 100.0 {
			self.engine.send(Command::SetStreamVolume {
				session: session as u64,
				stream_id: stream_id.clone(),
				volume: self.stream_volume / 100.0,
			});
		}
		let wake = || later(|app| app.show_picture());
		let decoder = self.video.watch(&self.engine, session as u64, &stream_id, wake);
		self.watch = Some(Watch {
			session: Some(session),
			stream_id,
			title,
			status: format!("Asking {streamer} to let you watch…"),
			streamer,
			connected: false,
			ended: false,
			has_frame: false,
			shown: true,
			decoder: Some(decoder),
		});
		self.refresh_viewer();
		self.refresh_streams();
	}

	/// The local test stream in the viewer (`VOELIN_DEMO_STREAM`).
	pub(crate) fn start_demo(&mut self) {
		self.leave_stream();
		self.watch = Some(Watch {
			session: None,
			stream_id: "demo".into(),
			title: "Test pattern".into(),
			streamer: "local preview".into(),
			status: "Starting the local preview…".into(),
			connected: true,
			ended: false,
			has_frame: false,
			shown: true,
			decoder: None,
		});
		let runtime = self.engine.runtime().clone();
		let wake = || later(|app| app.show_picture());
		self.video.demo(&runtime, wake, |result| {
			later(move |app| {
				let Some(watch) = app.watch.as_mut().filter(|w| w.session.is_none()) else {
					return;
				};
				match result {
					Ok(decoder) => watch.decoder = Some(decoder),
					Err(e) => watch.status = format!("The preview failed: {e}"),
				}
				app.refresh_viewer();
			});
		});
		self.refresh_viewer();
		self.refresh_streams();
	}

	/// A decoded picture is waiting.
	fn show_picture(&mut self) {
		let Some(watch) = &mut self.watch else { return };
		let Some(image) = watch.decoder.as_ref().and_then(Decoder::take_picture) else { return };
		let first = !watch.has_frame;
		watch.has_frame = true;
		if first {
			watch.status.clear();
		}
		if let Some(ui) = self.ui.upgrade() {
			let bridge = ui.global::<Bridge>();
			bridge.set_viewer_frame(image);
			if first {
				self.refresh_viewer();
			}
		}
	}

	pub(crate) fn show_viewer(&mut self, shown: bool) {
		if let Some(watch) = &mut self.watch {
			watch.shown = shown;
		}
		if !shown {
			self.set_fullscreen(false);
		}
		self.refresh_viewer();
		self.refresh_streams();
	}

	/// Stop watching and close the viewer.
	pub(crate) fn leave_stream(&mut self) {
		if let Some(watch) = self.watch.take()
			&& let Some(session) = watch.session
			&& !watch.ended
		{
			self.engine
				.send(Command::LeaveStream { session: session as u64, stream_id: watch.stream_id });
		}
		self.set_fullscreen(false);
		self.refresh_viewer();
		self.refresh_streams();
	}

	pub(crate) fn set_stream_volume(&mut self, percent: f32) {
		self.stream_volume = percent.round().clamp(0.0, 200.0);
		if let Some(watch) = &self.watch
			&& let Some(session) = watch.session
		{
			self.engine.send(Command::SetStreamVolume {
				session: session as u64,
				stream_id: watch.stream_id.clone(),
				volume: self.stream_volume / 100.0,
			});
		}
	}

	pub(crate) fn toggle_fullscreen(&mut self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let full = !ui.global::<Bridge>().get_viewer_fullscreen();
		self.set_fullscreen(full);
	}

	fn set_fullscreen(&self, full: bool) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		if bridge.get_viewer_fullscreen() == full {
			return;
		}
		bridge.set_viewer_fullscreen(full);
		ui.window().set_fullscreen(full);
	}
}
