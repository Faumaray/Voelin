//! Application state on the UI thread, wired to the engine: setup, the
//! event loop glue and the state the screens share.
//!
//! The screens' logic is elsewhere: servers and the channel tree in
//! `servers.rs`, chat in `chat.rs`, streams in `streams.rs`, the settings
//! page in `settings_page.rs` and `appearance.rs`, the view models in `vm/`,
//! callbacks in `bind/`, development switches in `dev.rs`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Runtime;
use tracing::warn;
use voelin_core::settings::{AUDIO, CRASH_REPORTS, Settings};
use voelin_core::stream::StreamInfo;
use voelin_core::{
	AudioSettings, Command, Engine, Event, History, HistoryMessage, SessionState, VoiceState,
};
use voelin_model::{Capabilities, ChannelId, ChatTarget, GroupInfo, Presence};
use voelin_platform::crash;
use voelin_store::{Bookmark, MemorySecrets, Secrets, Store};

use crate::hotkey::GlobalPtt;
use crate::settings::{
	CLIENT_PLAYBACK, ClientPlaybackMap, DeviceChoices, UI, UiSettings, appearance_keys,
};
use crate::streams::{Share, Watch};
use crate::video::Video;
use crate::vm::chat::Previous;

slint::include_modules!();

/// The settings service on the client database: the UI's keys registered,
/// then a config file (`VOELIN_CONFIG`, else `<data dir>/config.toml` if it
/// exists), `VOELIN_SETTING_*` variables and `overrides` below the stored
/// values. Without the database the settings live in memory.
fn open_settings(dir: &Path, overrides: &[String]) -> Settings {
	let prefs = Settings::open(&dir.join("client.db")).unwrap_or_else(|e| {
		warn!(%e, "settings are not stored this time");
		Settings::in_memory()
	});
	prefs.register(&UI);
	prefs.register(&CLIENT_PLAYBACK);
	for key in appearance_keys() {
		prefs.register(key);
	}
	for key in crate::settings::page_keys() {
		prefs.register(key);
	}
	prefs.register(&crate::studio::STUDIO_UI);
	let config = std::env::var_os("VOELIN_CONFIG")
		.map(PathBuf::from)
		.or_else(|| Some(dir.join("config.toml")).filter(|p| p.exists()));
	let mut errors = match config.map(|path| prefs.load_config_file(&path)) {
		Some(Ok(errors)) => errors,
		Some(Err(e)) => vec![e],
		None => Vec::new(),
	};
	errors.extend(prefs.apply_env(std::env::vars()));
	errors.extend(prefs.apply_overrides(overrides.iter().map(String::as_str)));
	for e in errors {
		warn!(%e, "setting ignored");
	}
	prefs
}

/// Secrets in the OS keyring, or in memory when there is none (headless
/// Linux without a Secret Service).
#[cfg(not(target_os = "android"))]
struct FallbackSecrets {
	keyring: voelin_store::KeyringSecrets,
	memory: MemorySecrets,
}

/// The secrets store when [`RunOptions::secrets`] is not given: the OS
/// keyring on desktop, memory elsewhere.
fn default_secrets() -> Box<dyn Secrets> {
	#[cfg(not(target_os = "android"))]
	return Box::new(FallbackSecrets {
		keyring: voelin_store::KeyringSecrets::new("voelin"),
		memory: MemorySecrets::default(),
	});
	#[cfg(target_os = "android")]
	return Box::new(MemorySecrets::default());
}

#[cfg(not(target_os = "android"))]
impl Secrets for FallbackSecrets {
	fn get(&self, key: &str) -> voelin_store::Result<Option<String>> {
		match self.memory.get(key)? {
			Some(v) => Ok(Some(v)),
			None => Ok(self.keyring.get(key).unwrap_or(None)),
		}
	}

	fn set(&self, key: &str, value: &str) -> voelin_store::Result<()> {
		if let Err(error) = self.keyring.set(key, value) {
			warn!(%error, "keyring unavailable, keeping the secret for this session only");
			return self.memory.set(key, value);
		}
		Ok(())
	}

	fn delete(&self, key: &str) -> voelin_store::Result<()> {
		let _ = self.keyring.delete(key);
		self.memory.delete(key)
	}
}

/// A message of a chat with the handle the UI knows it by: the engine's
/// local ids are 64 bit, a Slint `int` is not.
pub(crate) struct Msg {
	pub key: i32,
	pub message: HistoryMessage,
}

/// A chat of a session: its lines are the model the chat view shows while
/// the tab is selected, so a new message is one row appended.
pub(crate) struct Tab {
	pub target: ChatTarget,
	pub title: String,
	pub lines: Rc<VecModel<ChatLine>>,
	/// The messages, oldest first, by `(ts_ms, id)`.
	pub messages: Vec<Msg>,
	/// Live messages of a session that keeps no history (no server unique
	/// id yet): grouped from the message before, without ids.
	pub last: Option<Previous>,
	pub unread: i32,
	/// Nothing older exists as far as the engine can tell.
	pub complete: bool,
	/// A gateway page arrived: the chat is not only what we saw.
	pub synced: bool,
	/// An older page was asked for.
	pub loading: bool,
	/// The next message handle.
	next_key: i32,
	/// The pins of this chat, once asked for.
	pub pins: Vec<crate::chat::PinRow>,
	pub pins_loaded: bool,
	/// The gateway topics of this chat, and the open one with its messages.
	pub topics: Vec<voelin_gateway_proto::TopicInfo>,
	pub topics_loaded: bool,
	pub topic: Option<i64>,
	pub topic_messages: Vec<Msg>,
	/// The message jumped to from the pins.
	pub marked: Option<i64>,
}

impl Tab {
	pub fn new(target: ChatTarget, title: String) -> Self {
		Self {
			target,
			title,
			lines: Rc::new(VecModel::default()),
			messages: Vec::new(),
			last: None,
			unread: 0,
			complete: false,
			synced: false,
			loading: false,
			next_key: 1,
			pins: Vec::new(),
			pins_loaded: false,
			topics: Vec::new(),
			topics_loaded: false,
			topic: None,
			topic_messages: Vec::new(),
			marked: None,
		}
	}

	/// A handle for a message that has none yet.
	pub fn take_key(&mut self) -> i32 {
		self.next_key += 1;
		self.next_key
	}

	pub fn message(&self, key: i32) -> Option<&HistoryMessage> {
		self.messages.iter().chain(&self.topic_messages).find(|m| m.key == key).map(|m| &m.message)
	}
}

/// What we know about one server session.
pub(crate) struct SessionView {
	pub state: SessionState,
	pub capabilities: Capabilities,
	pub presence: Arc<Presence>,
	pub collapsed: HashSet<ChannelId>,
	pub talking: HashSet<u16>,
	pub tabs: Vec<Tab>,
	pub current_tab: usize,
	/// The own channel's tab was focused for this voice connection.
	pub focused_own_channel: bool,
	/// The streams in our channel (TeamSpeak 6).
	pub streams: Vec<StreamInfo>,
	/// Clients whose stored volume was sent for this voice connection.
	pub applied_playback: HashSet<u16>,
	/// Avatar pictures in the engine's cache, by unique id
	/// ([`Event::AvatarReady`]).
	pub avatars: HashMap<String, PathBuf>,
	/// Icons in the engine's cache, by icon id ([`Event::IconReady`]).
	pub icons: HashMap<u32, PathBuf>,
	/// The server groups in display order ([`Event::Groups`]).
	pub server_groups: Vec<GroupInfo>,
	/// What the session's gateway offers (`voelin_gateway_proto::feature`).
	pub gateway_caps: Vec<String>,
	/// The client whose member card is open.
	pub member_card: Option<u16>,
	/// Viewers of the streams in the gateway's directory, by stream id.
	pub stream_viewers: HashMap<String, u32>,
	/// Downloads started from chat, by transfer id.
	pub downloads: HashMap<u64, crate::chat::Download>,
	/// The next transfer id.
	pub next_transfer: u64,
	/// What the home, messages and events screens keep of the session.
	pub extra: crate::social::SessionExtra,
}

impl Default for SessionView {
	fn default() -> Self {
		Self {
			state: SessionState::default(),
			capabilities: Capabilities::default(),
			presence: Arc::default(),
			collapsed: HashSet::new(),
			talking: HashSet::new(),
			tabs: vec![Tab::new(ChatTarget::Server, "Server".into())],
			current_tab: 0,
			focused_own_channel: false,
			streams: Vec::new(),
			applied_playback: HashSet::new(),
			avatars: HashMap::new(),
			icons: HashMap::new(),
			server_groups: Vec::new(),
			gateway_caps: Vec::new(),
			member_card: None,
			stream_viewers: HashMap::new(),
			downloads: HashMap::new(),
			next_transfer: 1,
			extra: Default::default(),
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

	/// Unread messages in all chats.
	pub fn unread(&self) -> i32 {
		self.tabs.iter().map(|t| t.unread).sum()
	}

	/// The session's gateway offers this feature
	/// (`voelin_gateway_proto::feature`).
	pub fn gateway_has(&self, feature: &str) -> bool {
		self.gateway_caps.iter().any(|c| c == feature)
	}

	/// The chat history is kept by the engine (it knows the server): its
	/// messages come with ids, reactions and pins.
	pub fn has_history(&self) -> bool {
		self.state.server_uid.is_some()
	}

	/// The avatar picture of a client, if the engine fetched one.
	pub fn avatar(&self, client: u16) -> Option<&PathBuf> {
		let uid = self.presence.clients.get(&client)?.uid.as_deref()?;
		self.avatars.get(uid)
	}
}

/// The models the Bridge shows, created once and updated row by row
/// (`vm::list::sync`).
pub(crate) struct Models {
	pub servers: Rc<VecModel<ServerItem>>,
	pub tree: Rc<VecModel<TreeItem>>,
	pub members: Rc<VecModel<MemberItem>>,
	pub server_members: Rc<VecModel<MemberItem>>,
	pub tabs: Rc<VecModel<ChatTab>>,
	pub streams: Rc<VecModel<StreamItem>>,
	pub viewers: Rc<VecModel<ViewerItem>>,
	pub pins: Rc<VecModel<PinItem>>,
	pub topics: Rc<VecModel<TopicItem>>,
	pub topic_messages: Rc<VecModel<ChatLine>>,
	/// The quality choices of the watched stream.
	pub qualities: Rc<VecModel<slint::SharedString>>,
	/// Shown when there is no chat.
	pub no_chat: Rc<VecModel<ChatLine>>,
	/// Home, friends, messages, the bell, the search, events.
	pub social: crate::social::SocialModels,
	/// Settings pages.
	pub layers: Rc<VecModel<LayerItem>>,
	pub audio_sources: Rc<VecModel<AudioSourceItem>>,
	pub identities: Rc<VecModel<IdentityItem>>,
	pub gateway_config: Rc<VecModel<ConfigItem>>,
	pub gateway_perms: Rc<VecModel<PermItem>>,
	pub all_settings: Rc<VecModel<SettingItem>>,
}

impl Models {
	fn new(bridge: &Bridge) -> Self {
		let models = Self {
			servers: Rc::default(),
			tree: Rc::default(),
			members: Rc::default(),
			server_members: Rc::default(),
			tabs: Rc::default(),
			streams: Rc::default(),
			viewers: Rc::default(),
			pins: Rc::default(),
			topics: Rc::default(),
			topic_messages: Rc::default(),
			qualities: Rc::default(),
			no_chat: Rc::default(),
			social: crate::social::SocialModels::new(bridge),
			layers: Rc::default(),
			audio_sources: Rc::default(),
			identities: Rc::default(),
			gateway_config: Rc::default(),
			gateway_perms: Rc::default(),
			all_settings: Rc::default(),
		};
		bridge.set_servers(ModelRc::from(models.servers.clone()));
		bridge.set_tree(ModelRc::from(models.tree.clone()));
		bridge.set_members(ModelRc::from(models.members.clone()));
		bridge.set_server_members(ModelRc::from(models.server_members.clone()));
		bridge.set_tabs(ModelRc::from(models.tabs.clone()));
		bridge.set_streams(ModelRc::from(models.streams.clone()));
		bridge.set_share_viewers(ModelRc::from(models.viewers.clone()));
		bridge.set_pins(ModelRc::from(models.pins.clone()));
		bridge.set_topics(ModelRc::from(models.topics.clone()));
		bridge.set_topic_messages(ModelRc::from(models.topic_messages.clone()));
		bridge.set_viewer_qualities(ModelRc::from(models.qualities.clone()));
		bridge.set_messages(ModelRc::from(models.no_chat.clone()));
		bridge.set_layers(ModelRc::from(models.layers.clone()));
		bridge.set_audio_sources(ModelRc::from(models.audio_sources.clone()));
		bridge.set_identities(ModelRc::from(models.identities.clone()));
		bridge.set_gateway_config(ModelRc::from(models.gateway_config.clone()));
		bridge.set_gateway_perms(ModelRc::from(models.gateway_perms.clone()));
		bridge.set_all_settings(ModelRc::from(models.all_settings.clone()));
		models
	}
}

pub(crate) struct App {
	pub ui: slint::Weak<MainWindow>,
	pub store: Store,
	pub secrets: Box<dyn Secrets>,
	pub engine: Engine,
	pub identity: tsclientlib::Identity,
	pub bookmarks: Vec<Bookmark>,
	pub current: Option<i64>,
	pub sessions: HashMap<i64, SessionView>,
	pub models: Models,
	pub status: String,
	/// The members panel's search.
	pub member_filter: String,
	/// The topics drawer's search.
	pub topic_filter: String,
	/// The voice channel view is shown (its people come first in the
	/// members panel).
	pub voice_view: bool,
	/// The settings service (shared with the engine); `settings`, `audio`
	/// and `playback` are working copies of its keys.
	pub prefs: Settings,
	pub settings: UiSettings,
	pub audio: AudioSettings,
	/// Audio settings changed since they were last stored.
	pub audio_dirty: bool,
	pub inputs: DeviceChoices,
	pub outputs: DeviceChoices,
	pub playback: ClientPlaybackMap,
	/// The engine's contacts, by unique id ([`Event::ContactsChanged`]).
	pub contacts: HashMap<String, voelin_core::Contact>,
	/// The client in the volume dialog: session, client id, unique id.
	pub playback_dialog: Option<(i64, u16, Option<String>)>,
	pub mic_test: bool,
	pub ptt: GlobalPtt,
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
	/// `VOELIN_DEMO_STREAM`: the viewer shows a local test stream.
	pub demo: bool,
	/// `VOELIN_DEMO_UI`: sample servers, channels and chat (dev.rs).
	pub demo_ui: bool,
	/// `VOELIN_AUTOSHARE=test-pattern`: share the test pattern once streams work.
	pub autoshare: bool,
	/// `VOELIN_OPEN=client`: open the volume dialog of the first other client.
	pub open_client_pending: bool,
	/// The notices are in the About page's model.
	pub notices_loaded: bool,
	/// Files being uploaded to a channel from the composer: session,
	/// channel and name, so the link can be posted when they are up.
	pub uploads: HashMap<u64, (i64, u64, String)>,
	/// Home, friends, messages, the bell and the search.
	pub social: crate::social::Social,
	/// The settings pages of the new design.
	pub pages: crate::settings_pages::Pages,
	/// The Stream Studio (studio.rs).
	pub studio: crate::studio::StudioState,
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
#[derive(Default)]
pub struct RunOptions {
	/// Where the client database lives. Default: `VOELIN_DATA_DIR`, else
	/// `<data dir>/voelin`. Mobile platforms pass their app storage directory.
	pub data_dir: Option<std::path::PathBuf>,
	/// Where passwords are kept. Default: the OS keyring, in memory when it
	/// is unavailable.
	pub secrets: Option<Box<dyn Secrets>>,
	/// An engine the caller keeps running across windows (Android: voice
	/// goes on in a service while the activity is recreated). Default: an
	/// engine that stops with the window.
	pub engine: Option<HostedEngine>,
	/// Setting overrides as `key=value` (e.g. `--set` on the command line);
	/// values set in the app take precedence.
	pub setting_overrides: Vec<String>,
}

impl std::fmt::Debug for RunOptions {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("RunOptions")
			.field("data_dir", &self.data_dir)
			.field("secrets", &self.secrets.is_some())
			.field("engine", &self.engine.is_some())
			.field("setting_overrides", &self.setting_overrides)
			.finish()
	}
}

/// An engine owned by the caller of [`run`].
pub struct HostedEngine {
	pub engine: Engine,
	/// The runtime the engine runs on; the window's tasks run there too.
	pub runtime: tokio::runtime::Handle,
	/// The engine's events for this window: first events that rebuild the
	/// current state of every session, then new ones. The window stops
	/// reading when it closes.
	pub events: tokio::sync::mpsc::UnboundedReceiver<Event>,
}

impl HostedEngine {
	/// A new engine on `runtime`, all of whose events go to the window.
	fn start(runtime: &Runtime) -> Self {
		let engine = runtime.block_on(async { Engine::start() });
		let (tx, events) = tokio::sync::mpsc::unbounded_channel();
		let mut rx = engine.subscribe();
		runtime.spawn(async move {
			loop {
				match rx.recv().await {
					Ok(event) => {
						if tx.send(event).is_err() {
							break;
						}
					}
					Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
					Err(_) => break,
				}
			}
		});
		Self { engine, runtime: runtime.handle().clone(), events }
	}
}

/// Run the app on the current thread until the window closes. The Slint
/// platform must be set up before (the default backend on desktop).
pub fn run(options: RunOptions) -> Result<()> {
	// Crash reports: the hook first, recording once the setting is known.
	crash::set_app_version(env!("CARGO_PKG_VERSION"));
	crash::install(crash::default_dir(), false);
	let (own_runtime, hosted) = match options.engine {
		Some(hosted) => (None, hosted),
		None => {
			let runtime = Runtime::new()?;
			let hosted = HostedEngine::start(&runtime);
			(Some(runtime), hosted)
		}
	};
	let HostedEngine { engine, runtime, mut events } = hosted;

	// VOELIN_DATA_DIR isolates the database; the other development
	// switches are in dev.rs.
	// A host that names the data directory (Android) has no platform cache
	// directory either: avatars and icons go next to the database.
	if let Some(dir) = &options.data_dir {
		engine.send(Command::AttachCache(dir.join("images")));
	}
	let dir: PathBuf = match (options.data_dir, std::env::var_os("VOELIN_DATA_DIR")) {
		(Some(dir), _) => dir,
		(None, Some(dir)) => dir.into(),
		(None, None) => dirs::data_dir().unwrap_or_else(|| ".".into()).join("voelin"),
	};
	let store =
		Store::open(&dir.join("client.db")).context("failed to open the client database")?;
	// Chat history and contacts live in the client database; without this
	// the engine would keep them for this run only.
	match History::open(&dir.join("client.db")) {
		Ok(history) => engine.send(Command::AttachHistory(history)),
		Err(e) => warn!(%e, "chat history is not stored this time"),
	}
	let identity = match store.identities()?.first() {
		Some(entry) => store.identity(entry.id)?,
		None => {
			// Creating an identity takes a moment (security level 8).
			let identity = tsclientlib::Identity::create();
			store.add_identity("Default", &identity)?;
			identity
		}
	};
	let prefs = open_settings(&dir, &options.setting_overrides);
	engine.send(Command::AttachSettings(prefs.clone()));
	let mut settings: UiSettings = (*prefs.get_arc(&UI)).clone();
	settings.crash_reports = prefs.get(&CRASH_REPORTS);
	crash::set_enabled(settings.crash_reports);
	crash::test_crash_if_requested();
	let audio: AudioSettings = prefs.get(&AUDIO);
	let playback: ClientPlaybackMap = prefs.get(&CLIENT_PLAYBACK);
	engine.send(Command::SetAudioSettings(Box::new(audio.clone())));

	let ui = MainWindow::new()?;
	let bridge = ui.global::<Bridge>();
	bridge.set_app_name(voelin_platform::APP_NAME.into());
	bridge.set_app_version(env!("CARGO_PKG_VERSION").into());
	let models = Models::new(&bridge);
	// Wayland and X11 find the desktop file (and icon) by this id.
	#[cfg(not(target_os = "android"))]
	if let Err(e) = slint::set_xdg_app_id(voelin_platform::APP_ID) {
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
	let switches = crate::dev::Switches::from_env();
	let app = App {
		ui: ui.as_weak(),
		store,
		secrets: options.secrets.unwrap_or_else(default_secrets),
		engine: engine.clone(),
		identity,
		current: bookmarks.first().map(|b| b.id),
		bookmarks,
		sessions: HashMap::new(),
		models,
		status: String::new(),
		member_filter: String::new(),
		topic_filter: String::new(),
		voice_view: false,
		prefs,
		settings,
		audio,
		audio_dirty: false,
		inputs: DeviceChoices::default(),
		outputs: DeviceChoices::default(),
		playback,
		contacts: HashMap::new(),
		playback_dialog: None,
		mic_test: false,
		ptt,
		video,
		share: None,
		share_busy: false,
		share_error: String::new(),
		watch: None,
		stream_volume: 100.0,
		demo: switches.demo_stream,
		demo_ui: switches.demo_ui,
		autoshare: switches.autoshare,
		open_client_pending: switches.open_client,
		notices_loaded: false,
		uploads: HashMap::new(),
		social: Default::default(),
		pages: Default::default(),
		studio: Default::default(),
	};
	APP.with(|a| *a.borrow_mut() = Some(app));
	crate::bind::wire(&ui);
	with_app(|app| {
		app.apply_appearance();
		app.load_default_identity();
		app.apply_srtp();
		app.refresh_all();
		app.refresh_settings_flags();
		app.refresh_crash_notice();
		app.load_recent_chats();
		app.refresh_people();
		app.refresh_home();
		app.refresh_notices();
		app.refresh_pages();
		if app.demo {
			app.start_demo();
		}
	});

	// Engine events -> UI thread, until the event loop is gone.
	runtime.spawn(async move {
		while let Some(event) = events.recv().await {
			let posted = slint::invoke_from_event_loop(move || {
				with_app(|app| app.handle_event(event));
			});
			if posted.is_err() {
				break;
			}
		}
	});
	// Once a second: stream statistics, stored settings.
	let ticker = slint::Timer::default();
	ticker.start(slint::TimerMode::Repeated, Duration::from_secs(1), || {
		with_app(|app| app.tick());
	});

	let _dev = crate::dev::start(&ui, &switches);

	ui.run()?;
	// With our own engine, leave servers properly, so no ghost client stays
	// behind until it times out. A hosted engine keeps its sessions.
	let own = own_runtime.is_some();
	let open = with_app(|app| {
		app.save_audio();
		if let Err(e) = app.prefs.flush() {
			warn!(%e, "could not store settings");
		}
		app.watch = None;
		app.share = None;
		if own && !app.demo_ui {
			for id in app.sessions.keys() {
				app.engine.send(Command::CloseSession { session: *id as u64 });
			}
		}
		own && !app.sessions.is_empty() && !app.demo_ui
	});
	APP.with(|a| a.borrow_mut().take());
	if let Some(runtime) = own_runtime {
		if open == Some(true) {
			runtime.block_on(async { tokio::time::sleep(Duration::from_millis(500)).await });
		}
		runtime.shutdown_timeout(Duration::from_secs(2));
	}
	Ok(())
}

/// Open a file or folder with the desktop's default application.
pub(crate) fn open_path(path: &Path) -> std::io::Result<()> {
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

pub(crate) fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
	ModelRc::from(Rc::new(VecModel::from(items)))
}

impl App {
	pub fn view_mut(&mut self) -> Option<&mut SessionView> {
		let id = self.current?;
		Some(self.sessions.entry(id).or_default())
	}

	pub fn view(&self) -> Option<&SessionView> {
		self.current.and_then(|id| self.sessions.get(&id))
	}

	pub fn bookmark(&self, id: i64) -> Option<&Bookmark> {
		self.bookmarks.iter().find(|b| b.id == id)
	}

	/// Send a command for the current session. Not for the sample
	/// sessions of VOELIN_DEMO_UI: the engine would open them for real.
	pub fn command(&self, f: impl FnOnce(u64) -> Command) {
		if let Some(id) = self.current.filter(|_| !self.demo_ui) {
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
		if let Err(e) = self.prefs.set(&UI, self.settings.clone()) {
			warn!(%e, "could not store settings");
		}
	}

	/// A setting changed, maybe elsewhere (a command, another window): take
	/// the values the window keeps copies of. The engine applies audio
	/// settings itself.
	fn setting_changed(&mut self, key: &str) {
		if key == AUDIO.name() && !self.audio_dirty {
			self.audio = self.prefs.get(&AUDIO);
		} else if key == CRASH_REPORTS.name() {
			let enabled = self.prefs.get(&CRASH_REPORTS);
			if enabled != self.settings.crash_reports {
				self.settings.crash_reports = enabled;
				crash::set_enabled(enabled);
				self.refresh_settings_flags();
			}
		} else if key == CLIENT_PLAYBACK.name() {
			self.playback = self.prefs.get(&CLIENT_PLAYBACK);
		} else if appearance_keys().iter().any(|k| k.name() == key) {
			self.apply_appearance();
		}
		self.page_setting_changed(key);
	}

	pub(crate) fn handle_event(&mut self, event: Event) {
		let touched = self.social_event(&event);
		self.dispatch(event);
		self.social_refresh(touched);
	}

	fn dispatch(&mut self, event: Event) {
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
				self.refresh_tree();
				self.refresh_streams();
				self.autoshare();
				if self.open_client_pending && self.current == Some(id) {
					self.open_first_client();
				}
			}
			Event::ServerInfo { session, name, flavor, capabilities } => {
				self.sessions.entry(session as i64).or_default().capabilities = capabilities;
				if self.current == Some(session as i64) {
					let kind = match flavor {
						voelin_model::ServerFlavor::Ts3(v) => format!("TeamSpeak 3 {v}"),
						voelin_model::ServerFlavor::Ts6(v) => format!("TeamSpeak 6 {v}"),
						voelin_model::ServerFlavor::Unknown(v) => v,
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
					self.refresh_toolbar();
					self.refresh_tree();
					self.refresh_chat();
					self.refresh_streams();
					if self.open_client_pending {
						self.open_first_client();
					}
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
			// Only for sessions the engine keeps no history for (it does not
			// know the server yet): those get ids and reactions instead.
			Event::Chat { session, message } => {
				let id = session as i64;
				if !self.sessions.entry(id).or_default().has_history() {
					self.add_message(id, message);
				}
			}
			Event::ChatHistory { session, target, messages, source, complete } => {
				self.history_batch(session as i64, &target, messages, source, complete);
			}
			Event::Gateway { session, update } => {
				self.gateway_extra(session as i64, &update);
				self.gateway_update(session as i64, update);
			}
			Event::Groups { session, server_groups, .. } => {
				let view = self.sessions.entry(session as i64).or_default();
				view.server_groups = (*server_groups).clone();
				// Display order, as TeamSpeak sorts them.
				view.server_groups.sort_by_key(|g| (g.sort_id, g.id));
				if self.current == Some(session as i64) {
					self.refresh_tree();
				}
			}
			Event::AvatarReady { session, client_uid, path, .. } => {
				let view = self.sessions.entry(session as i64).or_default();
				view.avatars.insert(client_uid, path);
				if self.current == Some(session as i64) {
					self.refresh_tree();
					self.refresh_chat();
				}
			}
			Event::IconReady { session, icon, path } => {
				let view = self.sessions.entry(session as i64).or_default();
				view.icons.insert(icon, path);
				if self.current == Some(session as i64) {
					self.refresh_tree();
				}
			}
			Event::Transfer { session, transfer, state } => {
				self.transfer_progress(session as i64, transfer, state);
			}
			Event::ContactsChanged { contacts } => {
				self.contacts = contacts.iter().map(|c| (c.uid.clone(), c.clone())).collect();
				self.refresh_member_card();
			}
			// The sample sessions of VOELIN_DEMO_UI are unknown to the engine.
			Event::Error { .. } if self.demo_ui => {}
			Event::Error { message, .. } => self.set_status(message),
			Event::SettingChanged { key } => self.setting_changed(&key),
			Event::SettingRejected { key, message } => {
				self.set_status(format!("Setting {key}: {message}"));
			}
			// The Stream Studio's stream (studio.rs).
			Event::StreamState { session, state } if self.studio_streams_to(session as i64) => {
				self.studio_stream_state(state);
			}
			Event::StreamViewers { session, viewers } if self.studio_streams_to(session as i64) => {
				self.studio_viewers(viewers);
			}
			event => self.stream_event(event),
		}
	}

	/// Once a second.
	fn tick(&mut self) {
		self.tick_streams();
		self.tick_pages();
		if self.audio_dirty {
			self.save_audio();
		}
	}

	pub fn refresh_all(&mut self) {
		self.refresh_servers();
		self.refresh_toolbar();
		self.refresh_tree();
		self.refresh_chat();
		self.refresh_streams();
	}
}
