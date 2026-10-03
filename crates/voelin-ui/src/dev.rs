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
//!   client once connected), `panel` / `no-panel` (the members panel),
//!   `voice` (the voice channel view), `pins`, `topics` (the drawers),
//!   `topic:<id>` (a topic's messages), `member` (the member card of the
//!   first other client), `watch` (the first stream; with sample data the
//!   local test pattern in its place), `popout` (the same, popped out),
//!   `tab:<home|servers|chat|activity|you>` (phone layout),
//!   `studio[:<what>]`: the Stream Studio (with `VOELIN_DEMO_UI` a demo
//!   studio from synthetic sources), `studio:window` in a window of its own,
//!   `studio:live` going live, `studio:record`, or a dialog: `studio:source`,
//!   `studio:audio`, `studio:camera`, `studio:scene`, `studio:settings`.
//!   A screenshot also saves the studio's window (`<name>-window.png`) and
//!   draws it over the main window.
//! - `VOELIN_AUTOWATCH=1`: watch the first stream that shows up.
//! - `VOELIN_AUTOSHARE=test-pattern`: share the test pattern (accepting
//!   everyone) once connected to a TeamSpeak 6 server.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};
use voelin_core::gateway::Pin;
use voelin_core::stream::{StreamInfo, StreamKind};
use voelin_core::{
	Event, GatewayUpdate, HistoryMessage, HistorySource, ObserveState, SessionState, Source,
	VoiceState,
};
use voelin_gateway_proto::{ReactionCount, StreamEntry, StreamSource, TopicInfo, UserRef, feature};
use voelin_model::{
	ChannelInfo, ChatMessage, ChatTarget, ClientInfo, GroupInfo, Presence, ServerFlavor,
};
use voelin_store::{Bookmark, MessageSource};

use crate::app::{App, Bridge, MainWindow, MobileTab, Nav, Page, SettingsSection, with_app};

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
	_resize: Option<slint::Timer>,
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
			"voice" => nav.invoke_show_voice(true),
			"pins" => nav.invoke_show_pins(true),
			"topics" => nav.invoke_show_topics(true),
			"topic" => {
				with_app(|app| {
					app.open_topic(arg.parse().unwrap_or(1));
					// No gateway answers in demo mode.
					if switches.demo_ui {
						demo_topic(app);
					}
				});
			}
			"member" => {
				with_app(|app| app.open_first_member());
			}
			"watch" | "popout" => {
				with_app(|app| {
					if switches.demo_ui { app.demo_watch() } else { app.watch_first_stream() }
				});
				ui.global::<Bridge>().set_viewer_popped(what == "popout");
			}
			"studio" => {
				if arg != "window" {
					nav.invoke_open_studio();
				}
				with_app(|app| app.studio_dev(arg));
			}
			// Opened once connected (settings_page.rs).
			"client" => {}
			other => eprintln!("VOELIN_OPEN: unknown screen {other:?}"),
		}
	}
	// A window manager can refuse the size set before the window opened,
	// so ask again once it is up. (Under `xvfb-run` in a Wayland session,
	// remove WAYLAND_DISPLAY, or winit opens the window on the desktop.)
	let resize = switches.window_size.map(|(w, h)| {
		let weak = ui.as_weak();
		let timer = slint::Timer::default();
		timer.start(
			slint::TimerMode::SingleShot,
			Duration::from_millis(switches.screenshot_delay * 1000 / 2),
			move || {
				if let Some(ui) = weak.upgrade() {
					ui.window().set_size(slint::LogicalSize::new(w, h));
				}
			},
		);
		timer
	});
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
	Running { _screenshot: screenshot, _resize: resize }
}

fn save_screenshot(ui: &MainWindow, path: &std::path::Path) -> Result<()> {
	let mut image = ui.window().take_snapshot()?;
	if let Some(studio) = with_app(|app| app.studio_snapshot()).flatten() {
		let stem = path.file_stem().unwrap_or_default().to_string_lossy();
		save_png(&studio, &path.with_file_name(format!("{stem}-window.png")))?;
		paste(&mut image, &studio);
	}
	save_png(&image, path)
}

/// Draw `top` over the bottom right of `image` with a border, as a second
/// window sits on a desktop.
fn paste(image: &mut SharedPixelBuffer<Rgba8Pixel>, top: &SharedPixelBuffer<Rgba8Pixel>) {
	let (w, h) = (image.width() as usize, image.height() as usize);
	let (tw, th) = (top.width() as usize, top.height() as usize);
	let (x0, y0) = (w.saturating_sub(tw + 24), h.saturating_sub(th + 24));
	let border = Rgba8Pixel { r: 0x3f, g: 0x7b, b: 0xff, a: 0xff };
	let pixels = image.make_mut_slice();
	for y in y0.saturating_sub(1)..(y0 + th + 1).min(h) {
		for x in x0.saturating_sub(1)..(x0 + tw + 1).min(w) {
			let (ty, tx) = (y.wrapping_sub(y0), x.wrapping_sub(x0));
			pixels[y * w + x] =
				if ty < th && tx < tw { top.as_slice()[ty * tw + tx] } else { border };
		}
	}
}

fn save_png(image: &SharedPixelBuffer<Rgba8Pixel>, path: &std::path::Path) -> Result<()> {
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
	// Groups, badges and descriptions, as the members panel shows them.
	clients[0].server_groups = vec![GROUP_MOD];
	clients[0].talk_power = 75;
	clients[0].description = Some("Raid lead. Ask me about the Tuesday runs.".into());
	clients[1].server_groups = vec![GROUP_ADMIN];
	clients[1].talk_power = 100;
	clients[1].priority_speaker = true;
	clients[1].description = Some("Streaming most evenings.".into());
	clients[2].server_groups = vec![GROUP_MOD];
	clients[2].channel_commander = true;
	clients[2].talk_power = 75;
	clients[3].server_groups = vec![GROUP_MEMBER];
	clients[3].recording = true;
	clients[4].server_groups = vec![GROUP_MEMBER];
	clients[5].server_groups = vec![GROUP_MEMBER];
	clients[6].server_groups = vec![GROUP_GUEST];
	clients[7].server_groups = vec![GROUP_GUEST];
	for c in clients {
		p.clients.insert(c.id, c);
	}

	let group = |id: u64, sort_id: i32, name: &str| GroupInfo {
		id,
		name: name.into(),
		icon: 0,
		sort_id,
		..Default::default()
	};
	let flavor = ServerFlavor::from_version_string("6.0.0-beta13.1 [Build: 1]");
	let capabilities = flavor.capabilities();
	let events = [
		Event::ServerInfo { session, name: "Nightfall Guild".into(), flavor, capabilities },
		Event::Groups {
			session,
			server_groups: Arc::new(vec![
				group(GROUP_ADMIN, 10, "Server Admin"),
				group(GROUP_MOD, 20, "Moderator"),
				group(GROUP_MEMBER, 30, "Member"),
				group(GROUP_GUEST, 40, "Guest"),
			]),
			channel_groups: Arc::new(Vec::new()),
		},
		Event::Presence { session, presence: Arc::new(p) },
		Event::State {
			session,
			state: SessionState {
				voice: VoiceState::Connected,
				presence_source: Some(Source::Voice),
				own_channel: Some(2),
				own_client: Some(1),
				// The engine keeps history for a server it knows, so the
				// chat is driven by `Event::ChatHistory` like a real one.
				server_uid: Some("demo-server".into()),
				..Default::default()
			},
		},
		// A gateway with everything the chat screens need.
		Event::Gateway {
			session,
			update: GatewayUpdate::Connected {
				gateway_id: "demo".into(),
				server_uid: "demo-server".into(),
				server_name: "Nightfall Guild".into(),
				uid: "demo-1".into(),
				capabilities: vec![
					feature::HISTORY.into(),
					feature::PINS.into(),
					feature::REACTIONS.into(),
					feature::TOPICS.into(),
				],
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
	demo_chat(app, session);
	// No toast over the screenshots.
	app.set_status("");
	app.refresh_all();
}

/// Server group ids of the sample server.
const GROUP_ADMIN: u64 = 6;
const GROUP_MOD: u64 = 7;
const GROUP_MEMBER: u64 = 8;
const GROUP_GUEST: u64 = 9;

/// A file link as TeamSpeak clients post it.
fn file_link(channel: u64, name: &str, size: u64) -> String {
	voelin_model::FileRef {
		channel,
		path: "/".into(),
		name: name.into(),
		size: Some(size),
		..Default::default()
	}
	.to_bbcode()
}

/// One sample message: chat, author, its client, text, how many minutes
/// ago, relayed, reactions, pinned, and the topic it belongs to.
type SampleLine =
	(i64, &'static str, u16, String, i64, bool, Vec<ReactionCount>, bool, Option<i64>);

/// The sample chats, as stored messages with reactions, pins and topics.
fn demo_chat(app: &mut App, session: u64) {
	let now = chrono::Utc::now().timestamp_millis();
	let react =
		|emoji: &str, count: u32, me: bool| ReactionCount { emoji: emoji.into(), count, me };
	let lines: Vec<SampleLine> = vec![
		(
			DEMO,
			"Nova",
			1,
			"Beautiful morning for a game. Who's up for a session later? ☀️".into(),
			60,
			false,
			vec![react("👍", 3, false), react("☀️", 1, true)],
			false,
			None,
		),
		(
			DEMO,
			"Kairo",
			3,
			"Definitely! I'll be on after lunch.".into(),
			56,
			false,
			Vec::new(),
			false,
			None,
		),
		(DEMO, "Kairo", 3, "Bringing snacks 🍕🍩".into(), 56, false, Vec::new(), false, None),
		(
			DEMO,
			"Mira",
			4,
			format!(
				"Route for tonight, print it out: {}",
				file_link(2, "raid-route.pdf", 2_411_724)
			),
			40,
			false,
			vec![react("🎉", 5, true)],
			true,
			None,
		),
		(
			DEMO,
			"Nova",
			1,
			"Reminder: raid starts 20:00 sharp, bring elixirs.".into(),
			30,
			false,
			Vec::new(),
			true,
			Some(1),
		),
		(
			DEMO,
			"dex",
			5,
			"Can somebody look at the wiki page? The old link is dead.".into(),
			26,
			false,
			Vec::new(),
			false,
			None,
		),
		(
			DEMO,
			"Kairo",
			3,
			"Fixed, it points at the new host now.".into(),
			24,
			false,
			vec![react("👍", 2, false)],
			false,
			None,
		),
		(
			DEMO,
			"Mira",
			4,
			format!("And the banner I promised: {}", file_link(2, "guild-banner.png", 486_912)),
			20,
			false,
			Vec::new(),
			false,
			None,
		),
		(
			DEMO,
			"Talon",
			6,
			"That looks great. Putting it on the site tonight.".into(),
			17,
			false,
			vec![react("❤️", 4, true)],
			false,
			None,
		),
		(
			DEMO,
			"Lumen",
			2,
			"Going live in Chill Zone — exploring the Lands Between. Come hang! 🔥🗡️".into(),
			12,
			false,
			vec![react("🔥", 8, false)],
			false,
			None,
		),
		(DEMO, "dex", 5, "🎉🎉".into(), 10, false, Vec::new(), false, None),
		(
			DEMO,
			"Ari",
			0,
			"Anyone want to run some co-op later? 👀 Posting from the web.".into(),
			5,
			true,
			Vec::new(),
			false,
			None,
		),
		(DEMO + 1, "Ivy", 0, "Welcome to the lounge!".into(), 30, false, Vec::new(), false, None),
		(
			DEMO + 1,
			"Ivy",
			0,
			"New channel for artists 🎨".into(),
			29,
			false,
			Vec::new(),
			false,
			None,
		),
	];
	let mut id = 1;
	let mut pinned: Vec<HistoryMessage> = Vec::new();
	for (chat, author, client, text, ago, relay, reactions, pin, topic) in lines {
		let target = if chat == DEMO { ChatTarget::Channel(2) } else { ChatTarget::Server };
		let message = HistoryMessage {
			id,
			message: ChatMessage {
				target: target.clone(),
				author_name: author.into(),
				author_uid: Some(format!("demo-{client}")),
				author_id: (client > 0).then_some(client),
				text,
				ts_ms: now - ago * 60_000,
				via_relay: relay,
				blocked: false,
			},
			source: MessageSource::Voice,
			remote_id: Some(id),
			topic_id: topic,
			reactions,
			pinned: pin,
			rev: 1,
		};
		if pin {
			pinned.push(message.clone());
		}
		app.handle_event(Event::ChatHistory {
			session: if chat == DEMO { session } else { session + 1 },
			target,
			messages: vec![message],
			source: HistorySource::Gateway,
			complete: false,
		});
		id += 1;
	}
	// A server chat besides the channel chat.
	app.handle_event(Event::ChatHistory {
		session,
		target: ChatTarget::Server,
		messages: vec![HistoryMessage {
			id: 100,
			message: ChatMessage {
				target: ChatTarget::Server,
				author_name: "Talon".into(),
				author_uid: Some("demo-6".into()),
				author_id: Some(6),
				text: "Server restart tonight at 23:00.".into(),
				ts_ms: now - 40 * 60_000,
				via_relay: false,
				blocked: false,
			},
			source: MessageSource::Voice,
			remote_id: Some(100),
			topic_id: None,
			reactions: Vec::new(),
			pinned: false,
			rev: 1,
		}],
		source: HistorySource::Gateway,
		complete: true,
	});
	// The gateway's answers (no gateway runs in demo mode): the stream
	// directory, and the pins and topics for the drawers.
	app.handle_event(Event::Gateway {
		session,
		update: GatewayUpdate::Streams {
			streams: vec![StreamEntry {
				id: "demo-stream".into(),
				stream_id: Some("demo-stream".into()),
				streamer: UserRef { uid: "demo-2".into(), name: "Lumen".into() },
				client_id: Some(2),
				channel: Some(2),
				title: "Exploring the Lands Between".into(),
				kind: "screen".into(),
				started_ms: now - 12 * 60_000,
				viewers: Some(12),
				source: StreamSource::Registered,
				event_id: None,
			}],
		},
	});
	let by = |name: &str| UserRef { uid: format!("demo-{name}"), name: name.into() };
	app.handle_event(Event::Gateway {
		session,
		update: GatewayUpdate::Pins {
			target: ChatTarget::Channel(2),
			pins: pinned
				.into_iter()
				.map(|message| Pin {
					ts_ms: message.message.ts_ms + 60_000,
					by: by("Nova"),
					message,
				})
				.collect(),
		},
	});
	let topic = |id: i64, title: &str, count: u64, ago: i64| TopicInfo {
		id,
		target: ChatTarget::Channel(2),
		title: title.into(),
		creator: by("Nova"),
		created_ms: now - 5 * 86_400_000,
		root_message_id: Some(5),
		last_activity_ms: now - ago * 60_000,
		message_count: count,
		archived: false,
	};
	app.handle_event(Event::Gateway {
		session,
		update: GatewayUpdate::Topics {
			target: ChatTarget::Channel(2),
			topics: vec![
				topic(1, "Tonight's raid", 14, 3),
				topic(2, "Build ideas for the new patch", 42, 95),
				topic(3, "Server rules", 6, 2880),
			],
		},
	});
}

/// The messages of the sample topic, as the gateway answers when it is
/// opened.
fn demo_topic(app: &mut App) {
	let now = chrono::Utc::now().timestamp_millis();
	app.handle_event(Event::Gateway {
		session: DEMO as u64,
		update: GatewayUpdate::TopicHistory {
			target: ChatTarget::Channel(2),
			topic: 1,
			messages: (0..4)
				.map(|i| HistoryMessage {
					id: 200 + i,
					message: ChatMessage {
						target: ChatTarget::Channel(2),
						author_name: ["Nova", "Kairo", "Mira", "dex"][i as usize].into(),
						author_uid: Some(format!("demo-{}", [1, 3, 4, 5][i as usize])),
						author_id: Some([1, 3, 4, 5][i as usize]),
						text: [
							"Reminder: raid starts 20:00 sharp, bring elixirs.",
							"I can tank if nobody else wants to.",
							"Recording it again, say if you would rather not be in it.",
							"Bringing the good soup 🍲",
						][i as usize]
							.into(),
						ts_ms: now - (20 - i * 4) * 60_000,
						via_relay: false,
						blocked: false,
					},
					source: MessageSource::Gateway,
					remote_id: Some(200 + i),
					topic_id: Some(1),
					reactions: Vec::new(),
					pinned: false,
					rev: 1,
				})
				.collect(),
			has_more: false,
		},
	});
}
