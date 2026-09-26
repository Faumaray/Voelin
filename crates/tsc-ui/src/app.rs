//! Application state on the UI thread, wired to the engine.
//!
//! Streams (the streams panel, sharing, the viewer) are in `streams.rs`,
//! the settings page's data in `settings.rs`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::runtime::Runtime;
use tracing::warn;
use tsc_core::stream::StreamInfo;
use tsc_core::{
	AudioSettings, Command, Engine, Event, ObserveState, SessionState, Source, VoiceOptions,
	VoiceState,
};
use tsc_model::{Capabilities, ChannelId, ChatMessage, ChatTarget, Presence, TreeRow, tree_rows};
use tsc_platform::{crash, notices};
use tsc_store::{Bookmark, MemorySecrets, QueryConfig, QueryTransport, Secrets, Store};

use crate::hotkey::GlobalPtt;
use crate::settings::{
	self, AUDIO_KEY, CLIENT_PLAYBACK_KEY, ClientPlayback, ClientPlaybackMap, DeviceChoices, UI_KEY,
	UiSettings,
};
use crate::streams::{Share, Watch};
use crate::video::Video;

slint::include_modules!();

/// Secrets in the OS keyring, or in memory when there is none (headless
/// Linux without a Secret Service).
struct FallbackSecrets {
	keyring: tsc_store::KeyringSecrets,
	memory: MemorySecrets,
}

impl Secrets for FallbackSecrets {
	fn get(&self, key: &str) -> tsc_store::Result<Option<String>> {
		match self.memory.get(key)? {
			Some(v) => Ok(Some(v)),
			None => Ok(self.keyring.get(key).unwrap_or(None)),
		}
	}

	fn set(&self, key: &str, value: &str) -> tsc_store::Result<()> {
		if let Err(error) = self.keyring.set(key, value) {
			warn!(%error, "keyring unavailable, keeping the secret for this session only");
			return self.memory.set(key, value);
		}
		Ok(())
	}

	fn delete(&self, key: &str) -> tsc_store::Result<()> {
		let _ = self.keyring.delete(key);
		self.memory.delete(key)
	}
}

struct Tab {
	target: ChatTarget,
	title: String,
	lines: Vec<ChatLine>,
	unread: i32,
}

/// What we know about one server session.
pub(crate) struct SessionView {
	pub state: SessionState,
	pub capabilities: Capabilities,
	pub presence: Arc<Presence>,
	collapsed: HashSet<ChannelId>,
	talking: HashSet<u16>,
	tabs: Vec<Tab>,
	current_tab: usize,
	/// The own channel's tab was focused for this voice connection.
	focused_own_channel: bool,
	/// The streams in our channel (TeamSpeak 6).
	pub streams: Vec<StreamInfo>,
	/// Clients whose stored volume was sent for this voice connection.
	applied_playback: HashSet<u16>,
}

impl Default for SessionView {
	fn default() -> Self {
		Self {
			state: SessionState::default(),
			capabilities: Capabilities::default(),
			presence: Arc::default(),
			collapsed: HashSet::new(),
			talking: HashSet::new(),
			tabs: vec![Tab {
				target: ChatTarget::Server,
				title: "Server".into(),
				lines: Vec::new(),
				unread: 0,
			}],
			current_tab: 0,
			focused_own_channel: false,
			streams: Vec::new(),
			applied_playback: HashSet::new(),
		}
	}
}

impl SessionView {
	/// Streams can be watched and shared: TeamSpeak 6, connected with voice.
	pub fn streams_available(&self) -> bool {
		self.capabilities.streams && self.state.voice == VoiceState::Connected
	}

	/// A client's nickname, or its id.
	pub fn nickname(&self, client: u16) -> String {
		self.presence
			.clients
			.get(&client)
			.map_or_else(|| format!("client {client}"), |c| c.nickname.clone())
	}
}

pub(crate) struct App {
	pub ui: slint::Weak<MainWindow>,
	store: Store,
	secrets: Box<dyn Secrets>,
	pub engine: Engine,
	identity: tsclientlib::Identity,
	bookmarks: Vec<Bookmark>,
	pub current: Option<i64>,
	pub sessions: HashMap<i64, SessionView>,
	status: String,
	pub settings: UiSettings,
	audio: AudioSettings,
	/// Audio settings changed since they were last stored.
	audio_dirty: bool,
	inputs: DeviceChoices,
	outputs: DeviceChoices,
	playback: ClientPlaybackMap,
	/// The client in the volume dialog: session, client id, unique id.
	playback_dialog: Option<(i64, u16, Option<String>)>,
	mic_test: bool,
	ptt: GlobalPtt,
	pub video: Video,
	/// Our stream.
	pub share: Option<Share>,
	/// The capture for a share is being started.
	pub share_busy: bool,
	pub share_error: String,
	/// The stream in the viewer.
	pub watch: Option<Watch>,
	/// Stream volume in percent, kept between streams.
	pub stream_volume: f32,
	/// `TSC_DEMO_STREAM`: the viewer shows a local test stream.
	pub demo: bool,
	/// `TSC_AUTOSHARE=test-pattern`: share the test pattern once streams work.
	pub autoshare: bool,
	/// The notices are in the About page's model.
	notices_loaded: bool,
}

thread_local! {
	static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

pub(crate) fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
	APP.with(|app| app.borrow_mut().as_mut().map(f))
}

/// Run `f` on the app from another thread, on the UI thread.
pub(crate) fn later(f: impl FnOnce(&mut App) + Send + 'static) {
	let _ = slint::invoke_from_event_loop(move || {
		with_app(f);
	});
}

/// Start options for [`run`].
#[derive(Clone, Debug, Default)]
pub struct RunOptions {
	/// Where the client database lives. Default: `TSC_DATA_DIR`, else
	/// `<data dir>/tsc`. Mobile platforms pass their app storage directory.
	pub data_dir: Option<std::path::PathBuf>,
}

/// Run the app on the current thread until the window closes. The Slint
/// platform must be set up before (the default backend on desktop).
pub fn run(options: RunOptions) -> Result<()> {
	// Crash reports: the hook first, recording once the setting is known.
	crash::set_app_version(env!("CARGO_PKG_VERSION"));
	crash::install(crash::default_dir(), false);
	let runtime = Runtime::new()?;
	let engine = runtime.block_on(async { Engine::start() });

	// Development switches: TSC_DATA_DIR isolates the database,
	// TSC_AUTOCONNECT=voice|observe connects the first bookmark on start,
	// TSC_SCREENSHOT=<png> saves the window after TSC_SCREENSHOT_DELAY seconds and exits,
	// TSC_DEMO_STREAM=1 shows a local test stream in the viewer,
	// TSC_OPEN=share|settings[:<tab>]|about opens that on start, TSC_AUTOWATCH=1
	// watches the first stream that shows up, TSC_AUTOSHARE=test-pattern shares
	// the test pattern (accepting everyone) once connected to a TeamSpeak 6 server.
	let dir: PathBuf = match (options.data_dir, std::env::var_os("TSC_DATA_DIR")) {
		(Some(dir), _) => dir,
		(None, Some(dir)) => dir.into(),
		(None, None) => dirs::data_dir().unwrap_or_else(|| ".".into()).join("tsc"),
	};
	let store =
		Store::open(&dir.join("client.db")).context("failed to open the client database")?;
	let identity = match store.identities()?.first() {
		Some(entry) => store.identity(entry.id)?,
		None => {
			// Creating an identity takes a moment (security level 8).
			let identity = tsclientlib::Identity::create();
			store.add_identity("Default", &identity)?;
			identity
		}
	};
	let settings: UiSettings = store.setting(UI_KEY).ok().flatten().unwrap_or_default();
	crash::set_enabled(settings.crash_reports);
	crash::test_crash_if_requested();
	let audio: AudioSettings = store.setting(AUDIO_KEY).ok().flatten().unwrap_or_default();
	let playback: ClientPlaybackMap =
		store.setting(CLIENT_PLAYBACK_KEY).ok().flatten().unwrap_or_default();
	engine.send(Command::SetAudioSettings(Box::new(audio.clone())));

	let ui = MainWindow::new()?;
	// Wayland and X11 find the desktop file (and icon) by this id.
	#[cfg(not(target_os = "android"))]
	if let Err(e) = slint::set_xdg_app_id(tsc_platform::APP_ID) {
		warn!("cannot set the app id: {e}");
	}
	let bookmarks = store.bookmarks()?;
	let ptt = GlobalPtt::start(
		&runtime,
		|status| later(move |app| app.set_ptt_status(status)),
		|on| later(move |app| app.command(|session| Command::SetTransmitting { session, on })),
	);
	if settings.global_ptt {
		ptt.set(Some(settings.ptt_key.clone()));
	}
	let video = Video::new(&dir, settings.openh264);
	let demo = std::env::var("TSC_DEMO_STREAM").is_ok_and(|v| v == "1");
	let app = App {
		ui: ui.as_weak(),
		store,
		secrets: Box::new(FallbackSecrets {
			keyring: tsc_store::KeyringSecrets::new("tsc-client"),
			memory: MemorySecrets::default(),
		}),
		engine: engine.clone(),
		identity,
		current: bookmarks.first().map(|b| b.id),
		bookmarks,
		sessions: HashMap::new(),
		status: "Ready".into(),
		settings,
		audio,
		audio_dirty: false,
		inputs: DeviceChoices::default(),
		outputs: DeviceChoices::default(),
		playback,
		playback_dialog: None,
		mic_test: false,
		ptt,
		video,
		share: None,
		share_busy: false,
		share_error: String::new(),
		watch: None,
		stream_volume: 100.0,
		demo,
		autoshare: std::env::var("TSC_AUTOSHARE").is_ok_and(|v| v == "test-pattern"),
		notices_loaded: false,
	};
	APP.with(|a| *a.borrow_mut() = Some(app));
	wire_callbacks(&ui);
	with_app(|app| {
		app.refresh_all();
		app.refresh_settings_flags();
		app.refresh_crash_notice();
		if app.demo {
			app.start_demo();
		}
	});

	// Engine events -> UI thread.
	let mut events = engine.subscribe();
	runtime.spawn(async move {
		loop {
			match events.recv().await {
				Ok(event) => later(move |app| app.handle_event(event)),
				Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
				Err(_) => break,
			}
		}
	});
	// Once a second: stream statistics, stored settings.
	let ticker = slint::Timer::default();
	ticker.start(slint::TimerMode::Repeated, Duration::from_secs(1), || {
		with_app(|app| app.tick());
	});

	match std::env::var("TSC_AUTOCONNECT").as_deref() {
		Ok("voice") => {
			with_app(|app| app.connect_voice());
		}
		Ok("observe") => {
			with_app(|app| app.toggle_observe());
		}
		_ => {}
	}
	match std::env::var("TSC_OPEN").as_deref() {
		Ok("share") => ui.invoke_open_share_dialog(),
		Ok("about") => {
			with_app(|app| app.load_notices());
			ui.set_show_about(true);
		}
		Ok(open) if open.starts_with("settings") => {
			with_app(|app| app.open_settings());
			let tab = open.strip_prefix("settings:").and_then(|t| t.parse().ok());
			ui.set_settings_tab(tab.unwrap_or(0));
			ui.set_settings_open(true);
		}
		_ => {}
	}
	let _screenshot_timer = std::env::var_os("TSC_SCREENSHOT").map(|path| {
		let delay =
			std::env::var("TSC_SCREENSHOT_DELAY").ok().and_then(|d| d.parse().ok()).unwrap_or(4);
		let weak = ui.as_weak();
		let timer = slint::Timer::default();
		timer.start(slint::TimerMode::SingleShot, Duration::from_secs(delay), move || {
			if let Some(ui) = weak.upgrade() {
				if let Err(e) = save_screenshot(&ui, std::path::Path::new(&path)) {
					eprintln!("screenshot failed: {e}");
				}
				let _ = ui.hide();
			}
		});
		timer
	});

	ui.run()?;
	// Leave servers properly, so no ghost client stays behind until it times out.
	let open = with_app(|app| {
		app.save_audio();
		app.watch = None;
		app.share = None;
		for id in app.sessions.keys() {
			app.engine.send(Command::CloseSession { session: *id as u64 });
		}
		!app.sessions.is_empty()
	});
	APP.with(|a| a.borrow_mut().take());
	if open == Some(true) {
		runtime.block_on(async { tokio::time::sleep(Duration::from_millis(500)).await });
	}
	runtime.shutdown_timeout(Duration::from_secs(2));
	Ok(())
}

fn wire_callbacks(ui: &MainWindow) {
	let bridge = ui.global::<Bridge>();
	bridge.on_select_server(|id| {
		with_app(|app| {
			app.current = Some(id as i64);
			app.refresh_all();
		});
	});
	bridge.on_connect_voice(|| {
		with_app(|app| app.connect_voice());
	});
	bridge.on_disconnect_voice(|| {
		with_app(|app| app.command(|session| Command::DisconnectVoice { session }));
	});
	bridge.on_toggle_observe(|| {
		with_app(|app| app.toggle_observe());
	});
	bridge.on_join_channel(|cid| {
		with_app(|app| {
			app.command(|session| Command::MoveToChannel {
				session,
				channel: cid as ChannelId,
				password: None,
			})
		});
	});
	bridge.on_open_channel_chat(|cid| {
		with_app(|app| app.open_chat(ChatTarget::Channel(cid as ChannelId), true));
	});
	bridge.on_toggle_collapse(|cid| {
		with_app(|app| {
			if let Some(view) = app.view_mut() {
				let cid = cid as ChannelId;
				if !view.collapsed.remove(&cid) {
					view.collapsed.insert(cid);
				}
			}
			app.refresh_tree();
		});
	});
	bridge.on_select_tab(|i| {
		with_app(|app| {
			if let Some(view) = app.view_mut() {
				view.current_tab = i as usize;
				if let Some(tab) = view.tabs.get_mut(i as usize) {
					tab.unread = 0;
				}
			}
			app.refresh_chat();
		});
	});
	bridge.on_close_tab(|i| {
		with_app(|app| app.close_tab(i as usize));
	});
	bridge.on_send_message(|text| {
		with_app(|app| app.send_message(text.to_string()));
	});
	bridge.on_set_input_muted(|muted| {
		with_app(|app| app.command(|session| Command::SetInputMuted { session, muted }));
	});
	bridge.on_set_output_muted(|muted| {
		with_app(|app| app.command(|session| Command::SetOutputMuted { session, muted }));
	});
	bridge.on_push_to_talk(|on| {
		with_app(|app| app.command(|session| Command::SetTransmitting { session, on }));
	});
	bridge.on_edit_bookmark(|id| with_app(|app| app.bookmark_form(id as i64)).unwrap_or_default());
	bridge.on_save_bookmark(|form| {
		with_app(|app| app.save_bookmark(form));
	});
	bridge.on_delete_bookmark(|id| {
		with_app(|app| app.delete_bookmark(id as i64));
	});

	// Streams.
	bridge.on_open_share(|| with_app(|app| app.open_share()).unwrap_or_default());
	bridge.on_start_share(|form| {
		with_app(|app| app.start_share(form));
	});
	bridge.on_stop_share(|| {
		with_app(|app| app.stop_share());
	});
	bridge.on_respond_viewer(|viewer, accept| {
		with_app(|app| app.respond_viewer(viewer as u16, accept));
	});
	bridge.on_kick_viewer(|viewer| {
		with_app(|app| app.kick_viewer(viewer as u16));
	});
	bridge.on_watch_stream(|id| {
		with_app(|app| app.watch_stream(id.to_string()));
	});
	bridge.on_show_viewer(|| {
		with_app(|app| app.show_viewer(true));
	});
	bridge.on_hide_viewer(|| {
		with_app(|app| app.show_viewer(false));
	});
	bridge.on_leave_stream(|| {
		with_app(|app| app.leave_stream());
	});
	bridge.on_set_stream_volume(|volume| {
		with_app(|app| app.set_stream_volume(volume));
	});
	bridge.on_toggle_fullscreen(|| {
		with_app(|app| app.toggle_fullscreen());
	});

	// Settings.
	bridge.on_open_settings(|| {
		with_app(|app| app.open_settings());
	});
	bridge.on_close_settings(|| {
		with_app(|app| app.close_settings());
	});
	bridge.on_audio_changed(|form| {
		with_app(|app| app.audio_changed(&form));
	});
	bridge.on_toggle_mic_test(|| {
		with_app(|app| app.set_mic_test(!app.mic_test));
	});
	bridge.on_apply_ptt(|enabled, key| {
		with_app(|app| app.apply_ptt(enabled, key.to_string()));
	});
	bridge.on_set_h264(|enabled| {
		with_app(|app| app.set_h264(enabled));
	});
	bridge.on_download_h264(|| {
		with_app(|app| app.download_h264());
	});
	bridge.on_open_client(|id| with_app(|app| app.open_client(id as u16)).unwrap_or(false));
	bridge.on_client_audio_changed(|form| {
		with_app(|app| app.client_playback_changed(&form));
	});

	// Crash reports and notices.
	bridge.on_set_crash_reports(|enabled| {
		with_app(|app| app.set_crash_reports(enabled));
	});
	bridge.on_open_crash_folder(|| {
		with_app(|app| {
			let result = crash::dir().ok_or_else(|| "no crash report folder".to_owned());
			if let Err(e) = result.and_then(|dir| open_path(&dir).map_err(|e| e.to_string())) {
				app.set_status(format!("Cannot open the crash report folder: {e}"));
			}
		});
	});
	bridge.on_delete_crash_reports(|| {
		with_app(|app| {
			match crash::clear() {
				Ok(n) => app.set_status(format!("Deleted {n} crash reports")),
				Err(e) => app.set_status(format!("Cannot delete the crash reports: {e}")),
			}
			app.refresh_crash_notice();
		});
	});
	bridge.on_dismiss_crash_notice(|| {
		with_app(|app| {
			if let Some(ui) = app.ui.upgrade() {
				ui.global::<Bridge>().set_crash_notice(SharedString::new());
			}
		});
	});
	bridge.on_open_about(|| {
		with_app(|app| app.load_notices());
	});
}

/// Open a file or folder with the desktop's default application.
fn open_path(path: &Path) -> std::io::Result<()> {
	#[cfg(windows)]
	let mut command = std::process::Command::new("explorer");
	#[cfg(target_os = "macos")]
	let mut command = std::process::Command::new("open");
	#[cfg(not(any(windows, target_os = "macos")))]
	let mut command = std::process::Command::new("xdg-open");
	let mut child = command.arg(path).spawn()?;
	// Reap it without blocking the UI.
	std::thread::spawn(move || child.wait());
	Ok(())
}

fn save_screenshot(ui: &MainWindow, path: &std::path::Path) -> Result<()> {
	let image = ui.window().take_snapshot()?;
	let file = std::io::BufWriter::new(std::fs::File::create(path)?);
	let mut encoder = png::Encoder::new(file, image.width(), image.height());
	encoder.set_color(png::ColorType::Rgba);
	encoder.set_depth(png::BitDepth::Eight);
	encoder.write_header()?.write_image_data(image.as_bytes())?;
	Ok(())
}

fn time_of(ts_ms: i64) -> String {
	chrono::DateTime::from_timestamp_millis(ts_ms)
		.map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
		.unwrap_or_default()
}

pub(crate) fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
	ModelRc::from(Rc::new(VecModel::from(items)))
}

impl App {
	fn view_mut(&mut self) -> Option<&mut SessionView> {
		let id = self.current?;
		Some(self.sessions.entry(id).or_default())
	}

	pub fn view(&self) -> Option<&SessionView> {
		self.current.and_then(|id| self.sessions.get(&id))
	}

	pub fn bookmark(&self, id: i64) -> Option<&Bookmark> {
		self.bookmarks.iter().find(|b| b.id == id)
	}

	pub fn command(&self, f: impl FnOnce(u64) -> Command) {
		if let Some(id) = self.current {
			self.engine.send(f(id as u64));
		}
	}

	pub fn set_status(&mut self, text: impl Into<String>) {
		self.status = text.into();
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_status_text(self.status.clone().into());
		}
	}

	pub fn store_settings(&self) {
		if let Err(e) = self.store.set_setting(UI_KEY, &self.settings) {
			warn!(%e, "could not store settings");
		}
	}

	fn connect_voice(&mut self) {
		let Some(b) = self.current.and_then(|id| self.bookmark(id)).cloned() else { return };
		let mut options = VoiceOptions::new(&b.address, &b.nickname);
		options.identity = Some(self.identity.clone());
		options.server_password = self.secrets.get(&b.server_password_key()).ok().flatten();
		options.channel = b.default_channel.clone();
		options.audio = true;
		options.stream_peer = self.video.peer_config(options.stream_peer);
		self.engine
			.send(Command::ConnectVoice { session: b.id as u64, options: Box::new(options) });
		self.set_status(format!("Connecting to {}…", b.address));
	}

	fn toggle_observe(&mut self) {
		let Some(b) = self.current.and_then(|id| self.bookmark(id)).cloned() else { return };
		let session = b.id as u64;
		let observing =
			self.sessions.get(&b.id).is_some_and(|v| v.state.observe != ObserveState::Off);
		if observing {
			self.engine.send(Command::StopObserving { session });
			return;
		}
		if let Some(url) = &b.gateway_url {
			self.engine.send(Command::ObserveGateway {
				session,
				url: url.clone(),
				identity: Box::new(self.identity.clone()),
			});
		} else if let Some(q) = &b.query {
			let secret = self.secrets.get(&b.query_password_key()).ok().flatten();
			let transport = match q.transport {
				QueryTransport::Raw => tsc_query::Transport::Raw,
				QueryTransport::Ssh => tsc_query::Transport::Ssh,
				QueryTransport::Http => tsc_query::Transport::Http,
			};
			let connect = tsc_query::Connect {
				transport,
				addr: format!("{}:{}", q.host, q.port),
				user: q.user.clone(),
				secret,
				server_port: q.server_port,
				server_id: None,
				line: Default::default(),
			};
			self.engine.send(Command::ObserveQuery { session, connect: Box::new(connect) });
		}
		self.set_status("Observing invisibly…");
	}

	fn open_chat(&mut self, target: ChatTarget, focus: bool) {
		let Some(id) = self.current else { return };
		let title = match &target {
			ChatTarget::Channel(cid) => {
				let name = self
					.sessions
					.get(&id)
					.and_then(|v| v.presence.channels.get(cid).map(|c| c.name.clone()));
				format!("#{}", name.unwrap_or_else(|| cid.to_string()))
			}
			ChatTarget::Server => "Server".into(),
			ChatTarget::Private(uid) => format!("@{uid}"),
		};
		let view = self.sessions.entry(id).or_default();
		let index = match view.tabs.iter().position(|t| t.target == target) {
			Some(i) => i,
			None => {
				view.tabs.push(Tab { target: target.clone(), title, lines: Vec::new(), unread: 0 });
				self.engine.send(Command::OpenChat { session: id as u64, target });
				view.tabs.len() - 1
			}
		};
		if focus {
			view.current_tab = index;
			view.tabs[index].unread = 0;
		}
		self.refresh_chat();
	}

	fn close_tab(&mut self, index: usize) {
		let Some(id) = self.current else { return };
		let view = self.sessions.entry(id).or_default();
		if index == 0 || index >= view.tabs.len() {
			return;
		}
		let tab = view.tabs.remove(index);
		view.current_tab = view.current_tab.min(view.tabs.len() - 1);
		self.engine.send(Command::CloseChat { session: id as u64, target: tab.target });
		self.refresh_chat();
	}

	fn send_message(&mut self, text: String) {
		let Some(id) = self.current else { return };
		let Some(view) = self.sessions.get(&id) else { return };
		let target = view.tabs[view.current_tab].target.clone();
		self.engine.send(Command::SendChat { session: id as u64, target, text });
	}

	fn bookmark_form(&self, id: i64) -> BookmarkForm {
		let Some(b) = self.bookmark(id) else {
			return BookmarkForm {
				id: -1,
				nickname: std::env::var("USER").unwrap_or_default().into(),
				query_transport: "none".into(),
				query_user: "serveradmin".into(),
				..Default::default()
			};
		};
		let secret = |key: String| -> SharedString {
			self.secrets.get(&key).ok().flatten().unwrap_or_default().into()
		};
		let (transport, addr, user) = match &b.query {
			Some(q) => (
				match q.transport {
					QueryTransport::Raw => "raw",
					QueryTransport::Ssh => "ssh",
					QueryTransport::Http => "http",
				},
				format!("{}:{}", q.host, q.port),
				q.user.clone(),
			),
			None => ("none", String::new(), "serveradmin".to_string()),
		};
		BookmarkForm {
			id: b.id as i32,
			name: b.name.clone().into(),
			address: b.address.clone().into(),
			nickname: b.nickname.clone().into(),
			server_password: secret(b.server_password_key()),
			gateway_url: b.gateway_url.clone().unwrap_or_default().into(),
			query_transport: transport.into(),
			query_addr: addr.into(),
			query_user: user.into(),
			query_password: secret(b.query_password_key()),
		}
	}

	fn save_bookmark(&mut self, form: BookmarkForm) {
		let query = match form.query_transport.as_str() {
			"none" | "" => None,
			t => {
				let (host, port) = form
					.query_addr
					.rsplit_once(':')
					.map_or((form.query_addr.to_string(), None), |(h, p)| {
						(h.to_string(), p.parse().ok())
					});
				let transport = match t {
					"raw" => QueryTransport::Raw,
					"http" => QueryTransport::Http,
					_ => QueryTransport::Ssh,
				};
				let default_port = match transport {
					QueryTransport::Raw => 10011,
					QueryTransport::Ssh => 10022,
					QueryTransport::Http => 10080,
				};
				Some(QueryConfig {
					transport,
					host,
					port: port.unwrap_or(default_port),
					user: form.query_user.to_string(),
					server_port: None,
				})
			}
		};
		let mut bookmark = Bookmark {
			id: form.id as i64,
			name: if form.name.is_empty() {
				form.address.to_string()
			} else {
				form.name.to_string()
			},
			address: form.address.to_string(),
			nickname: form.nickname.to_string(),
			identity: None,
			default_channel: None,
			gateway_url: Some(form.gateway_url.to_string()).filter(|u| !u.is_empty()),
			query,
			client_version: None,
		};
		let result = if bookmark.id < 0 {
			self.store.add_bookmark(&bookmark).map(|id| bookmark.id = id)
		} else {
			self.store.update_bookmark(&bookmark)
		};
		if let Err(e) = result {
			self.set_status(format!("Could not save: {e}"));
			return;
		}
		for (key, value) in [
			(bookmark.server_password_key(), &form.server_password),
			(bookmark.query_password_key(), &form.query_password),
		] {
			let result = if value.is_empty() {
				self.secrets.delete(&key)
			} else {
				self.secrets.set(&key, value)
			};
			if let Err(e) = result {
				warn!(%e, "could not store secret");
			}
		}
		self.current = Some(bookmark.id);
		self.bookmarks = self.store.bookmarks().unwrap_or_default();
		self.refresh_all();
	}

	fn delete_bookmark(&mut self, id: i64) {
		self.engine.send(Command::CloseSession { session: id as u64 });
		if let Some(b) = self.bookmark(id).cloned() {
			let _ = self.secrets.delete(&b.server_password_key());
			let _ = self.secrets.delete(&b.query_password_key());
		}
		let _ = self.store.delete_bookmark(id);
		self.sessions.remove(&id);
		self.bookmarks = self.store.bookmarks().unwrap_or_default();
		self.current = self.bookmarks.first().map(|b| b.id);
		self.refresh_all();
	}

	fn handle_event(&mut self, event: Event) {
		match event {
			Event::State { session, state } => {
				let id = session as i64;
				let view = self.sessions.entry(id).or_default();
				let was_voice = view.state.voice;
				view.state = state.clone();
				if state.voice != VoiceState::Connected {
					view.focused_own_channel = false;
				}
				if state.voice == VoiceState::Connected && was_voice != VoiceState::Connected {
					// A new connection: volumes are sent again.
					view.applied_playback.clear();
				}
				let focus = !view.focused_own_channel;
				if state.voice == VoiceState::Connected && was_voice != VoiceState::Connected {
					self.set_status("Connected");
				}
				if self.current == Some(id) {
					// The voice channel's chat is always at hand, and focused
					// once per connection.
					if let (Some(cid), VoiceState::Connected) = (state.own_channel, state.voice) {
						self.open_chat(ChatTarget::Channel(cid), focus);
						self.sessions.entry(id).or_default().focused_own_channel = true;
					}
				}
				self.refresh_servers();
				self.refresh_toolbar();
				self.refresh_streams();
				self.autoshare();
			}
			Event::ServerInfo { session, name, flavor, capabilities } => {
				self.sessions.entry(session as i64).or_default().capabilities = capabilities;
				if self.current == Some(session as i64) {
					let kind = match flavor {
						tsc_model::ServerFlavor::Ts3(v) => format!("TeamSpeak 3 {v}"),
						tsc_model::ServerFlavor::Ts6(v) => format!("TeamSpeak 6 {v}"),
						tsc_model::ServerFlavor::Unknown(v) => v,
					};
					self.set_status(format!("Connected to {name} ({kind})"));
				}
				self.refresh_streams();
				self.autoshare();
			}
			Event::Presence { session, presence } => {
				self.sessions.entry(session as i64).or_default().presence = presence;
				self.apply_client_playback(session as i64);
				if self.current == Some(session as i64) {
					self.refresh_tree();
					self.refresh_chat();
					self.refresh_streams();
				}
			}
			Event::Talking { session, client, talking } => {
				let view = self.sessions.entry(session as i64).or_default();
				if talking {
					view.talking.insert(client);
				} else {
					view.talking.remove(&client);
				}
				if self.current == Some(session as i64) {
					self.refresh_tree();
				}
			}
			Event::InputLevel { session, level_db, sending } => {
				let shown = match session {
					Some(s) => self.current == Some(s as i64),
					None => self.mic_test,
				};
				if shown && let Some(ui) = self.ui.upgrade() {
					let bridge = ui.global::<Bridge>();
					bridge.set_input_level(level_db);
					bridge.set_input_sending(sending);
				}
			}
			Event::MicrophoneTestError { message } => {
				self.set_status(format!("Microphone test: {message}"))
			}
			Event::Chat { session, message } => self.add_message(session as i64, message),
			Event::Error { message, .. } => self.set_status(message),
			event => self.stream_event(event),
		}
	}

	fn add_message(&mut self, id: i64, message: ChatMessage) {
		let current = self.current == Some(id);
		let view = self.sessions.entry(id).or_default();
		let index = match view.tabs.iter().position(|t| t.target == message.target) {
			Some(i) => i,
			None => {
				let title = match &message.target {
					ChatTarget::Channel(cid) => format!(
						"#{}",
						view.presence
							.channels
							.get(cid)
							.map(|c| c.name.clone())
							.unwrap_or_else(|| cid.to_string())
					),
					ChatTarget::Private(_) => format!("@{}", message.author_name),
					ChatTarget::Server => "Server".into(),
				};
				view.tabs.push(Tab {
					target: message.target.clone(),
					title,
					lines: Vec::new(),
					unread: 0,
				});
				view.tabs.len() - 1
			}
		};
		let tab = &mut view.tabs[index];
		tab.lines.push(ChatLine {
			author: message.author_name.into(),
			text: message.text.into(),
			time: time_of(message.ts_ms).into(),
			relayed: message.via_relay,
		});
		if !(current && view.current_tab == index) {
			tab.unread += 1;
		}
		if current {
			self.refresh_chat();
		}
	}

	/// Once a second.
	fn tick(&mut self) {
		self.tick_streams();
		if self.audio_dirty {
			self.save_audio();
		}
	}

	fn refresh_all(&mut self) {
		self.refresh_servers();
		self.refresh_toolbar();
		self.refresh_tree();
		self.refresh_chat();
		self.refresh_streams();
	}

	fn refresh_servers(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let items = self
			.bookmarks
			.iter()
			.map(|b| {
				let state = self.sessions.get(&b.id).map(|v| v.state.clone()).unwrap_or_default();
				let status = match (state.voice, state.observe) {
					(VoiceState::Connected, _) => "connected",
					(VoiceState::Connecting, _) | (_, ObserveState::Connecting) => "connecting",
					(_, ObserveState::Observing) => "observing",
					_ => "offline",
				};
				ServerItem {
					id: b.id as i32,
					name: b.name.clone().into(),
					address: b.address.clone().into(),
					status: status.into(),
				}
			})
			.collect();
		bridge.set_servers(model(items));
		bridge.set_current_server(self.current.map_or(-1, |id| id as i32));
	}

	fn refresh_toolbar(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let bookmark = self.current.and_then(|id| self.bookmark(id));
		let state = self.view().map(|v| v.state.clone()).unwrap_or_default();
		bridge.set_server_title(
			bookmark.map_or("No server selected".into(), |b| b.name.clone()).into(),
		);
		bridge.set_voice_connected(state.voice == VoiceState::Connected);
		bridge.set_voice_connecting(state.voice == VoiceState::Connecting);
		bridge.set_observing(state.observe != ObserveState::Off);
		bridge.set_can_observe(
			bookmark.is_some_and(|b| b.gateway_url.is_some() || b.query.is_some()),
		);
		bridge.set_input_muted(state.input_muted);
		bridge.set_output_muted(state.output_muted);
		bridge.set_transmitting(state.transmitting);
		bridge.set_presence_source(
			match state.presence_source {
				Some(Source::Voice) => "live (voice)",
				Some(Source::Gateway) => "invisible (gateway)",
				Some(Source::Query) => "invisible (query)",
				None => "",
			}
			.into(),
		);
		bridge.set_status_text(self.status.clone().into());
		if state.voice != VoiceState::Connected {
			bridge.set_input_level(-100.0);
			bridge.set_input_sending(false);
		}
	}

	fn refresh_tree(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let Some(view) = self.view() else {
			ui.global::<Bridge>().set_tree(model(Vec::new()));
			return;
		};
		let own_channel = view.state.own_channel;
		let rows = tree_rows(&view.presence, &|cid| view.collapsed.contains(&cid))
			.into_iter()
			.map(|row| match row {
				TreeRow::Channel { depth, channel } => TreeItem {
					is_channel: true,
					depth: depth as i32,
					id: channel.id as i32,
					name: channel.name.clone().into(),
					own: own_channel == Some(channel.id),
					locked: channel.has_password,
					collapsed: view.collapsed.contains(&channel.id),
					local_volume: 100,
					..Default::default()
				},
				TreeRow::Client { depth, client } => {
					let playback = client
						.uid
						.as_ref()
						.and_then(|uid| self.playback.get(uid))
						.copied()
						.unwrap_or_default();
					TreeItem {
						is_channel: false,
						depth: depth as i32,
						id: client.id as i32,
						name: client.nickname.clone().into(),
						own: false,
						talking: view.talking.contains(&client.id) || client.talking == Some(true),
						muted: client.input_muted,
						sound_off: client.output_muted,
						away: client.away.is_some(),
						streaming: client.streaming == Some(true),
						local_muted: playback.muted,
						local_volume: (playback.volume * 100.0).round() as i32,
						myself: view.state.own_client == Some(client.id),
						..Default::default()
					}
				}
			})
			.collect();
		ui.global::<Bridge>().set_tree(model(rows));
	}

	fn refresh_chat(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let Some(view) = self.view() else {
			bridge.set_tabs(model(vec![ChatTab { title: "Server".into(), ..Default::default() }]));
			bridge.set_messages(model(Vec::new()));
			return;
		};
		let tabs = view
			.tabs
			.iter()
			.map(|t| ChatTab {
				title: t.title.clone().into(),
				kind: matches!(t.target, ChatTarget::Channel(_)) as i32,
				id: match t.target {
					ChatTarget::Channel(cid) => cid as i32,
					_ => 0,
				},
				unread: t.unread,
			})
			.collect();
		bridge.set_tabs(model(tabs));
		bridge.set_current_tab(view.current_tab as i32);
		bridge.set_messages(model(view.tabs[view.current_tab].lines.clone()));
	}

	// Settings page.

	fn open_settings(&mut self) {
		let list = |devices: tsc_audio::Result<Vec<tsc_audio::device::DeviceInfo>>, input: bool| {
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
		let inputs = list(tsc_audio::device::input_devices(), true);
		let outputs = list(tsc_audio::device::output_devices(), false);
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
		self.refresh_h264();
	}

	fn close_settings(&mut self) {
		self.set_mic_test(false);
		self.save_audio();
	}

	fn audio_changed(&mut self, form: &AudioForm) {
		let audio = settings::apply_audio_form(form, &self.audio, &self.inputs, &self.outputs);
		if audio == self.audio {
			return;
		}
		self.audio = audio;
		self.audio_dirty = true;
		self.engine.send(Command::SetAudioSettings(Box::new(self.audio.clone())));
	}

	fn save_audio(&mut self) {
		if !self.audio_dirty {
			return;
		}
		self.audio_dirty = false;
		if let Err(e) = self.store.set_setting(AUDIO_KEY, &self.audio) {
			warn!(%e, "could not store audio settings");
		}
	}

	fn set_mic_test(&mut self, on: bool) {
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

	fn apply_ptt(&mut self, enabled: bool, key: String) {
		let key = key.trim().to_owned();
		self.settings.global_ptt = enabled;
		if !key.is_empty() {
			self.settings.ptt_key = key.clone();
		}
		self.store_settings();
		self.ptt.set((enabled && !key.is_empty()).then_some(key));
	}

	fn set_ptt_status(&mut self, status: String) {
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_ptt_status(status.into());
		}
	}

	fn set_h264(&mut self, enabled: bool) {
		self.settings.openh264 = enabled;
		self.store_settings();
		self.video.set_h264(enabled);
		self.refresh_h264();
	}

	fn download_h264(&mut self) {
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
	fn refresh_settings_flags(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		bridge.set_h264_enabled(self.settings.openh264);
		bridge.set_h264_attribution(notices::OPENH264_ATTRIBUTION.into());
		bridge.set_crash_reports(self.settings.crash_reports);
	}

	fn set_crash_reports(&mut self, enabled: bool) {
		self.settings.crash_reports = enabled;
		self.store_settings();
		crash::set_enabled(enabled);
	}

	/// A notice while crash reports are on disk.
	fn refresh_crash_notice(&self) {
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

	/// The third-party notices, one model entry per line.
	fn load_notices(&mut self) {
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
	fn open_client(&mut self, client: u16) -> bool {
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
			});
		}
		true
	}

	fn client_playback_changed(&mut self, form: &ClientAudio) {
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
			if let Err(e) = self.store.set_setting(CLIENT_PLAYBACK_KEY, &self.playback) {
				warn!(%e, "could not store client volumes");
			}
		}
		self.refresh_tree();
	}

	/// Send the stored volumes of clients that appeared in a voice session.
	fn apply_client_playback(&mut self, session: i64) {
		let Some(view) = self.sessions.get_mut(&session) else { return };
		if view.state.voice != VoiceState::Connected || view.state.presence_source != Some(Source::Voice)
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
			self.engine.send(Command::SetClientVolume { session: s, client: c, volume: playback.volume });
			if playback.muted {
				self.engine.send(Command::SetClientMuted { session: s, client: c, muted: true });
			}
		}
	}
}
