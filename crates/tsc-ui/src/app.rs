//! Application state on the UI thread, wired to the engine.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context, Result};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::runtime::Runtime;
use tracing::warn;
use tsc_core::{
	Command, Engine, Event, ObserveState, SessionState, Source, VoiceOptions, VoiceState,
};
use tsc_model::{ChannelId, ChatMessage, ChatTarget, Presence, TreeRow, tree_rows};
use tsc_store::{Bookmark, MemorySecrets, QueryConfig, QueryTransport, Secrets, Store};

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
struct SessionView {
	state: SessionState,
	presence: Arc<Presence>,
	collapsed: HashSet<ChannelId>,
	talking: HashSet<u16>,
	tabs: Vec<Tab>,
	current_tab: usize,
	/// The own channel's tab was focused for this voice connection.
	focused_own_channel: bool,
}

impl Default for SessionView {
	fn default() -> Self {
		Self {
			state: SessionState::default(),
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
		}
	}
}

struct App {
	ui: slint::Weak<MainWindow>,
	store: Store,
	secrets: Box<dyn Secrets>,
	engine: Engine,
	identity: tsclientlib::Identity,
	bookmarks: Vec<Bookmark>,
	current: Option<i64>,
	sessions: HashMap<i64, SessionView>,
	status: String,
}

thread_local! {
	static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
	APP.with(|app| app.borrow_mut().as_mut().map(f))
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
	let runtime = Runtime::new()?;
	let engine = runtime.block_on(async { Engine::start() });

	// Development switches: TSC_DATA_DIR isolates the database,
	// TSC_AUTOCONNECT=voice|observe connects the first bookmark on start,
	// TSC_SCREENSHOT=<png> saves the window after TSC_SCREENSHOT_DELAY seconds and exits.
	let dir = match (options.data_dir, std::env::var_os("TSC_DATA_DIR")) {
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

	let ui = MainWindow::new()?;
	let bookmarks = store.bookmarks()?;
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
	};
	APP.with(|a| *a.borrow_mut() = Some(app));
	wire_callbacks(&ui);
	with_app(|app| app.refresh_all());

	// Engine events -> UI thread.
	let mut events = engine.subscribe();
	runtime.spawn(async move {
		loop {
			match events.recv().await {
				Ok(event) => {
					let _ = slint::invoke_from_event_loop(move || {
						with_app(|app| app.handle_event(event));
					});
				}
				Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
				Err(_) => break,
			}
		}
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
	let _screenshot_timer = std::env::var_os("TSC_SCREENSHOT").map(|path| {
		let delay =
			std::env::var("TSC_SCREENSHOT_DELAY").ok().and_then(|d| d.parse().ok()).unwrap_or(4);
		let weak = ui.as_weak();
		let timer = slint::Timer::default();
		timer.start(
			slint::TimerMode::SingleShot,
			std::time::Duration::from_secs(delay),
			move || {
				if let Some(ui) = weak.upgrade() {
					if let Err(e) = save_screenshot(&ui, std::path::Path::new(&path)) {
						eprintln!("screenshot failed: {e}");
					}
					let _ = ui.hide();
				}
			},
		);
		timer
	});

	ui.run()?;
	APP.with(|a| a.borrow_mut().take());
	runtime.shutdown_timeout(std::time::Duration::from_secs(2));
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

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
	ModelRc::from(Rc::new(VecModel::from(items)))
}

impl App {
	fn view_mut(&mut self) -> Option<&mut SessionView> {
		let id = self.current?;
		Some(self.sessions.entry(id).or_default())
	}

	fn bookmark(&self, id: i64) -> Option<&Bookmark> {
		self.bookmarks.iter().find(|b| b.id == id)
	}

	fn command(&self, f: impl FnOnce(u64) -> Command) {
		if let Some(id) = self.current {
			self.engine.send(f(id as u64));
		}
	}

	fn set_status(&mut self, text: impl Into<String>) {
		self.status = text.into();
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_status_text(self.status.clone().into());
		}
	}

	fn connect_voice(&mut self) {
		let Some(b) = self.current.and_then(|id| self.bookmark(id)).cloned() else { return };
		let mut options = VoiceOptions::new(&b.address, &b.nickname);
		options.identity = Some(self.identity.clone());
		options.server_password = self.secrets.get(&b.server_password_key()).ok().flatten();
		options.channel = b.default_channel.clone();
		options.audio = true;
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
			}
			Event::ServerInfo { session, name, flavor, .. } => {
				if self.current == Some(session as i64) {
					let kind = match flavor {
						tsc_model::ServerFlavor::Ts3(v) => format!("TeamSpeak 3 {v}"),
						tsc_model::ServerFlavor::Ts6(v) => format!("TeamSpeak 6 {v}"),
						tsc_model::ServerFlavor::Unknown(v) => v,
					};
					self.set_status(format!("Connected to {name} ({kind})"));
				}
			}
			Event::Presence { session, presence } => {
				self.sessions.entry(session as i64).or_default().presence = presence;
				if self.current == Some(session as i64) {
					self.refresh_tree();
					self.refresh_chat();
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
			Event::Chat { session, message } => self.add_message(session as i64, message),
			Event::Error { message, .. } => self.set_status(message),
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

	fn refresh_all(&mut self) {
		self.refresh_servers();
		self.refresh_toolbar();
		self.refresh_tree();
		self.refresh_chat();
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
		let state = self
			.current
			.and_then(|id| self.sessions.get(&id))
			.map(|v| v.state.clone())
			.unwrap_or_default();
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
	}

	fn refresh_tree(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let Some(view) = self.current.and_then(|id| self.sessions.get(&id)) else {
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
					..Default::default()
				},
				TreeRow::Client { depth, client } => TreeItem {
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
					..Default::default()
				},
			})
			.collect();
		ui.global::<Bridge>().set_tree(model(rows));
	}

	fn refresh_chat(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let Some(view) = self.current.and_then(|id| self.sessions.get(&id)) else {
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
}
