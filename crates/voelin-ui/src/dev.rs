//! Development switches (environment variables), for trying screens and
//! taking screenshots without a server:
//!
//! - `VOELIN_DATA_DIR=<dir>`: the database (app.rs).
//! - `VOELIN_AUTOCONNECT=voice|observe`: connect the first bookmark on start.
//! - `VOELIN_SCREENSHOT=<png>`: save the window after
//!   `VOELIN_SCREENSHOT_DELAY` seconds (default 4) and exit.
//! - `VOELIN_WINDOW_SIZE=<w>x<h>`: the window's size (e.g. `390x844` for the
//!   phone layout).
//! - `VOELIN_DEMO_UI=1`: sample servers, channels, chat and streams, no
//!   server needed (nothing is stored).
//! - `VOELIN_DEMO_STREAM=1`: a local test stream in the viewer.
//! - `VOELIN_OPEN=<what>[,<what>...]`: open screens on start: `home`,
//!   `server`, `settings[:<section>]` (voice, keybinds, streaming, privacy,
//!   appearance, or 0-4), `about`, `share`, `bookmark` (add a server),
//!   `emoji` (the picker), `client` (the volume dialog of the first other
//!   client once connected), `panel` / `no-panel` (the right panel),
//!   `tab:<home|servers|chat|activity|you>` (phone layout).
//! - `VOELIN_AUTOWATCH=1`: watch the first stream that shows up.
//! - `VOELIN_AUTOSHARE=test-pattern`: share the test pattern (accepting
//!   everyone) once connected to a TeamSpeak 6 server.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use slint::ComponentHandle;
use voelin_core::stream::{StreamInfo, StreamKind};
use voelin_core::{Event, ObserveState, SessionState, Source, VoiceState};
use voelin_model::{ChannelInfo, ChatMessage, ChatTarget, ClientInfo, Presence, ServerFlavor};
use voelin_store::Bookmark;

use crate::app::{App, MainWindow, MobileTab, Nav, Page, SettingsSection, with_app};

/// The switches read at start.
#[derive(Clone, Debug, Default)]
pub(crate) struct Switches {
	pub demo_stream: bool,
	pub demo_ui: bool,
	pub autoshare: bool,
	pub open_client: bool,
	open: Vec<String>,
	autoconnect: Option<String>,
	screenshot: Option<std::path::PathBuf>,
	screenshot_delay: u64,
	window_size: Option<(f32, f32)>,
}

fn flag(name: &str) -> bool {
	std::env::var(name).is_ok_and(|v| v == "1")
}

/// `390x844` → (390, 844).
fn parse_size(text: &str) -> Option<(f32, f32)> {
	let (w, h) = text.split_once(['x', 'X'])?;
	Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

impl Switches {
	pub fn from_env() -> Self {
		let open: Vec<String> = std::env::var("VOELIN_OPEN")
			.unwrap_or_default()
			.split(',')
			.map(|s| s.trim().to_owned())
			.filter(|s| !s.is_empty())
			.collect();
		Self {
			demo_stream: flag("VOELIN_DEMO_STREAM"),
			demo_ui: flag("VOELIN_DEMO_UI"),
			autoshare: std::env::var("VOELIN_AUTOSHARE").is_ok_and(|v| v == "test-pattern"),
			open_client: open.iter().any(|o| o == "client"),
			open,
			autoconnect: std::env::var("VOELIN_AUTOCONNECT").ok(),
			screenshot: std::env::var_os("VOELIN_SCREENSHOT").map(Into::into),
			screenshot_delay: std::env::var("VOELIN_SCREENSHOT_DELAY")
				.ok()
				.and_then(|d| d.parse().ok())
				.unwrap_or(4),
			window_size: std::env::var("VOELIN_WINDOW_SIZE").ok().as_deref().and_then(parse_size),
		}
	}
}

fn section(name: &str) -> SettingsSection {
	match name {
		"1" | "keybinds" | "ptt" => SettingsSection::Keybinds,
		"2" | "streaming" => SettingsSection::Streaming,
		"3" | "privacy" => SettingsSection::Privacy,
		"4" | "appearance" => SettingsSection::Appearance,
		_ => SettingsSection::Voice,
	}
}

fn mobile_tab(name: &str) -> MobileTab {
	match name {
		"home" => MobileTab::Home,
		"servers" => MobileTab::Servers,
		"activity" => MobileTab::Activity,
		"you" => MobileTab::You,
		_ => MobileTab::Chat,
	}
}

/// Timers that must live while the window runs.
pub(crate) struct Running {
	_screenshot: Option<slint::Timer>,
}

/// Apply the switches once the app is set up.
pub(crate) fn start(ui: &MainWindow, switches: &Switches) -> Running {
	if let Some((w, h)) = switches.window_size {
		ui.window().set_size(slint::LogicalSize::new(w, h));
	}
	if switches.demo_ui {
		with_app(demo_ui);
	}
	match switches.autoconnect.as_deref() {
		Some("voice") => {
			with_app(|app| app.connect_voice());
		}
		Some("observe") => {
			with_app(|app| app.toggle_observe());
		}
		_ => {}
	}
	let nav = ui.global::<Nav>();
	for open in &switches.open {
		let (what, arg) = open.split_once(':').unwrap_or((open.as_str(), ""));
		match what {
			"home" => nav.invoke_show(Page::Home),
			"server" => nav.invoke_show(Page::Server),
			"settings" => nav.invoke_open_settings(section(arg)),
			"about" => nav.invoke_open_about(),
			"share" => nav.invoke_open_share(),
			"bookmark" => nav.invoke_add_server(),
			"emoji" => nav.set_emoji_open(true),
			"panel" => nav.set_right_panel_open(true),
			"no-panel" => nav.set_right_panel_open(false),
			"tab" => nav.set_mobile_tab(mobile_tab(arg)),
			// Opened once connected (settings_page.rs).
			"client" => {}
			other => eprintln!("VOELIN_OPEN: unknown screen {other:?}"),
		}
	}
	let screenshot = switches.screenshot.clone().map(|path| {
		let weak = ui.as_weak();
		let timer = slint::Timer::default();
		timer.start(
			slint::TimerMode::SingleShot,
			Duration::from_secs(switches.screenshot_delay),
			move || {
				if let Some(ui) = weak.upgrade() {
					if let Err(e) = save_screenshot(&ui, &path) {
						eprintln!("screenshot failed: {e}");
					}
					let _ = ui.hide();
				}
			},
		);
		timer
	});
	Running { _screenshot: screenshot }
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

const DEMO: i64 = 9001;

fn channel(id: u64, parent: u64, order: u64, name: &str) -> ChannelInfo {
	ChannelInfo { id, parent, order, name: name.into(), ..Default::default() }
}

fn client(id: u16, channel: u64, name: &str) -> ClientInfo {
	ClientInfo {
		id,
		channel,
		nickname: name.into(),
		uid: Some(format!("demo-{id}")),
		..Default::default()
	}
}

/// Sample data through the same events a server would send.
fn demo_ui(app: &mut App) {
	let bookmark = |id: i64, name: &str, address: &str| Bookmark {
		id,
		name: name.into(),
		address: address.into(),
		nickname: "Nova".into(),
		identity: None,
		default_channel: None,
		gateway_url: None,
		query: None,
		client_version: None,
	};
	app.bookmarks = vec![
		bookmark(DEMO, "Nightfall Guild", "ts.nightfall.example:9987"),
		bookmark(DEMO + 1, "Pixel Lounge", "pixel.example"),
		bookmark(DEMO + 2, "Dev TS3", "127.0.0.1:9987"),
	];
	app.current = Some(DEMO);
	let session = DEMO as u64;

	let mut p = Presence { server_name: "Nightfall Guild".into(), ..Default::default() };
	let mut chill = channel(2, 0, 1, "Chill Zone");
	chill.max_clients = Some(50);
	chill.topic = Some("Hang out, chat, and explore the worlds beyond.".into());
	let mut locked = channel(7, 0, 6, "Officers");
	locked.has_password = true;
	for c in [
		channel(1, 0, 0, "Lobby"),
		chill,
		channel(3, 0, 2, "Gaming"),
		channel(4, 3, 0, "Raid Night"),
		channel(5, 0, 3, "Music"),
		channel(6, 0, 5, "AFK"),
		locked,
	] {
		p.channels.insert(c.id, c);
	}
	let mut clients = vec![
		client(1, 2, "Nova"),
		client(2, 2, "Lumen"),
		client(3, 2, "Kairo"),
		client(4, 2, "Mira"),
		client(5, 2, "dex"),
		client(6, 3, "Talon"),
		client(7, 4, "Zeph"),
		client(8, 6, "Blaze"),
		client(9, 1, "Rin"),
	];
	clients[1].streaming = Some(true);
	clients[4].input_muted = true;
	clients[7].away = Some("brb".into());
	clients[8].output_muted = true;
	for c in clients {
		p.clients.insert(c.id, c);
	}

	let flavor = ServerFlavor::from_version_string("6.0.0-beta13.1 [Build: 1]");
	let capabilities = flavor.capabilities();
	let events = [
		Event::ServerInfo { session, name: "Nightfall Guild".into(), flavor, capabilities },
		Event::Presence { session, presence: Arc::new(p) },
		Event::State {
			session,
			state: SessionState {
				voice: VoiceState::Connected,
				presence_source: Some(Source::Voice),
				own_channel: Some(2),
				own_client: Some(1),
				..Default::default()
			},
		},
		Event::Talking { session, client: 3, talking: true },
		Event::StreamsChanged {
			session,
			streams: vec![StreamInfo {
				id: "demo-stream".into(),
				streamer: tsclientlib::ClientId(2),
				name: "Exploring the Lands Between".into(),
				kind: StreamKind::Screen,
				bitrate: 8000,
				viewer_limit: 0,
				audio: true,
			}],
		},
		Event::State {
			session: session + 1,
			state: SessionState { observe: ObserveState::Observing, ..Default::default() },
		},
	];
	for event in events {
		app.handle_event(event);
	}
	let now = chrono::Utc::now().timestamp_millis();
	let lines = [
		(
			DEMO,
			ChatTarget::Channel(2),
			"Nova",
			"Beautiful morning for a game. Who's up for a session later? ☀️",
			0,
			false,
		),
		(DEMO, ChatTarget::Channel(2), "Kairo", "Definitely! I'll be on after lunch.", 4, false),
		(DEMO, ChatTarget::Channel(2), "Kairo", "Bringing snacks 🍕🍩", 4, false),
		(
			DEMO,
			ChatTarget::Channel(2),
			"Mira",
			"Working on a new video today — here's a sneak peek soon! 🎬",
			7,
			false,
		),
		(
			DEMO,
			ChatTarget::Channel(2),
			"Lumen",
			"Going live in Chill Zone — exploring the Lands Between. Come hang! 🔥🗡️",
			48,
			false,
		),
		(DEMO, ChatTarget::Channel(2), "dex", "🎉🎉", 50, false),
		(
			DEMO,
			ChatTarget::Channel(2),
			"Ari",
			"Anyone want to run some co-op later? 👀 Posting from the web.",
			55,
			true,
		),
		(DEMO, ChatTarget::Server, "Talon", "Server restart tonight at 23:00.", 20, false),
		(DEMO + 1, ChatTarget::Server, "Ivy", "Welcome to the lounge!", 30, false),
		(DEMO + 1, ChatTarget::Server, "Ivy", "New channel for artists 🎨", 31, false),
	];
	for (id, target, author, text, minute, relay) in lines {
		let message = ChatMessage {
			target,
			author_name: author.into(),
			author_uid: None,
			author_id: None,
			text: text.into(),
			ts_ms: now - (60 - minute) * 60_000,
			via_relay: relay,
			blocked: false,
		};
		app.add_message(id, message);
	}
	// No toast over the screenshots.
	app.set_status("");
	app.refresh_all();
}
