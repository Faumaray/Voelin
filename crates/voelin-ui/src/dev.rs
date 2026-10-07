//! Development switches (environment variables), for trying screens and
//! taking screenshots without a server:
//!
//! - `VOELIN_DATA_DIR=<dir>`: the database (app.rs).
//! - `VOELIN_AUTOCONNECT=voice|observe`: connect the first bookmark on start.
//! - `VOELIN_SCREENSHOT=<png>`: save the window after
//!   `VOELIN_SCREENSHOT_DELAY` seconds (default 4) and exit.
//! - `VOELIN_WINDOW_SIZE=<w>x<h>`: the window's size (e.g. `390x844` for the
//!   phone layout).
//! - `VOELIN_DEMO_UI=1`: sample servers, channels, chat (two pages of older
//!   messages answer scrolling up) and streams, no server needed (nothing is
//!   stored).
//! - `VOELIN_DEMO_STREAM=1`: a local test stream in the viewer.
//! - `VOELIN_OPEN=<what>[,<what>...]`: open screens on start: `home`,
//!   `server` (`server:chat`: its server chat), `settings[:<section>]`
//!   (voice, keybinds, streaming, privacy, appearance, or 0-4), `about`,
//!   `share`, `bookmark` (add a server, also `join`;
//!   `bookmark:edit` the current one, Advanced open),
//!   `emoji` (the picker), `client` (the volume dialog of the first other
//!   client once connected), `panel` / `no-panel` (the members panel),
//!   `voice` (the voice channel view; on the phone its own screen),
//!   `members` (the phone's members page), `notification` (a tapped voice
//!   notification), `shared:<text>` (text shared to the app on Android),
//!   `pins`, `topics` (the drawers),
//!   `topic:<id>` (a topic's messages), `member` (the member card of the
//!   first other client), `poke` (the poke dialog: for the contact shown
//!   after `friends`, else for the first other client, its card open),
//!   `channel-password` (the password dialog of the first locked channel,
//!   the sample's Officers; `channel-password:wrong` as after a refused
//!   one), `actions` (the last message's actions, as if hovered),
//!   `first-run` (home's banner as before the first server, over the
//!   sample data),
//!   `unread` (the current chat read up to five messages before its end:
//!   the "New" divider, scrolled up to; `unread:end` ten messages before,
//!   at the list's end, under the bar that counts the new messages),
//!   `link-confirm[:<url>]` (the question before a masked link opens, by
//!   default the sample's raid board), `link:<url>` (a TeamSpeak link
//!   opened: the server dialog filled in from it, or in voice on its
//!   server a move into its channel), `watch` (the first stream; with
//!   sample data the local test pattern in its place), `popout` (the same,
//!   popped out),
//!   `tab:<home|servers|chat|activity|you>` (phone layout); `friends[:<uid>]`,
//!   `messages[:<uid>]` (a private chat), `inbox` (offline messages),
//!   `library`, `events`, `event-form`, `search[:<text>]`,
//!   `notifications` (the bell), `offline` (an offline message), `picture`
//!   (a chat picture opened large), `camera` (the settings' camera
//!   preview); settings sections also `account`,
//!   `profiles` (also `identities`), `devices`, `notifications`,
//!   `integrations`, `advanced` (5-10);
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
	Contact, Event, GatewayUpdate, HistoryMessage, HistorySource, JoinFailure, ObserveState,
	OfflineMessageInfo, Relation, SessionState, Source, VoiceState,
};
use voelin_gateway_proto::{
	Action, Attendee, ConfigEntry, ConfigSource, EventInfo, EventKind, EventSpec, PermRule,
	PermRuleInfo, ReactionCount, RsvpStatus, StreamEntry, StreamSource, TopicInfo, UserRef,
	feature,
};
use voelin_model::{
	BannerMode, ChannelInfo, ChatMessage, ChatTarget, ClientInfo, GroupInfo, Presence, ServerFlavor,
};
use voelin_store::{Bookmark, MessageSource};

use crate::app::{
	App, Bridge, MainWindow, MobileTab, Nav, Page, RecordingItem, SettingsSection, with_app,
};
use crate::settings::LastVoice;

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
		"5" | "account" => SettingsSection::Account,
		"6" | "profiles" | "identities" => SettingsSection::Profiles,
		"7" | "devices" => SettingsSection::Devices,
		"8" | "notifications" => SettingsSection::Notifications,
		"9" | "integrations" => SettingsSection::Integrations,
		"10" | "advanced" => SettingsSection::Advanced,
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
	_scroll: Option<slint::Timer>,
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
			with_app(|app| {
				if let Some(id) = app.current {
					app.observe(id);
				}
			});
		}
		_ => {}
	}
	let nav = ui.global::<Nav>();
	for open in &switches.open {
		let (what, arg) = open.split_once(':').unwrap_or((open.as_str(), ""));
		match what {
			"login" if switches.demo_ui => {
				let bridge = ui.global::<Bridge>();
				let mut account = bridge.get_myts();
				account.available = true;
				account.prompt = arg != "account";
				if arg == "account" {
					account.signed_in = true;
					account.username = "Alex Example".into();
					account.email = "alex@example.test".into();
					account.uuid = "00000000-0000-0000-0000-000000000001".into();
					account.myts_id = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB".into();
					account.identity_status = 1;
					account.initials = "AE".into();
					account.description = "Usually in the Raid channel after 8.".into();
					account.member_since = "2019-03-14".into();
					account.last_login = "2026-10-03".into();
					let strings = |items: &[&str]| {
						slint::ModelRc::new(slint::VecModel::from(
							items.iter().map(|s| slint::SharedString::from(*s)).collect::<Vec<_>>(),
						))
					};
					account.badges = strings(&["TeamSpeak 6 Beta", "Early supporter"]);
					account.devices =
						strings(&["Voelin · EU · 2026-10-03", "TeamSpeak · EU · 2026-09-28"]);
					nav.invoke_open_settings(SettingsSection::Account);
				}
				account.otp_required = arg == "otp";
				account.can_forget = arg == "saved" || arg == "account";
				account.status = if arg == "otp" {
					3
				} else if arg == "saved" {
					2
				} else {
					0
				};
				bridge.set_myts(account);
			}
			"home" => nav.invoke_show(Page::Home),
			"server" => {
				nav.invoke_show(Page::Server);
				// The server chat, which starts with the welcome message.
				if arg == "chat" {
					with_app(|app| app.select_tab(0));
				}
			}
			"settings" => nav.invoke_open_settings(section(arg)),
			"about" => nav.invoke_open_about(),
			"share" => {
				nav.invoke_open_share();
				// The sample share of the test pattern, live at once.
				if arg == "live" && switches.demo_ui {
					with_app(|app| app.demo_share());
				}
			}
			// The current server's dialog, Advanced open.
			"bookmark" if arg == "edit" => {
				if let Some(id) = with_app(|app| app.current).flatten() {
					nav.invoke_edit_server(id as i32);
				}
			}
			// `join`: the same dialog (Join a server was one of its own).
			"bookmark" | "join" => nav.invoke_add_server(),
			"emoji" => nav.set_emoji_open(true),
			"panel" => nav.set_right_panel_open(true),
			"no-panel" => nav.set_right_panel_open(false),
			"tab" => nav.set_mobile_tab(mobile_tab(arg)),
			"voice" => nav.invoke_show_voice(true),
			"pins" => nav.invoke_show_pins(true),
			// The phone's members page.
			"members" => nav.set_members_open(true),
			// What Android hands over: a tapped voice notification, text
			// shared to the app.
			"notification" => crate::inbox::request(crate::inbox::Request::ShowVoice),
			"shared" => crate::inbox::request(crate::inbox::Request::Share {
				text: Some(arg.to_owned()),
				files: Vec::new(),
			}),
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
			// After `friends` the contact path (Kairo without a contact
			// chosen), else the member card's.
			"poke" => {
				let friends = nav.get_page() == Page::Friends;
				with_app(|app| {
					if friends {
						let uid = app.social.selected.clone().unwrap_or_else(|| "demo-3".into());
						app.contact_action(&uid, "poke");
					} else {
						app.open_first_member();
						app.member_action("poke");
					}
				});
			}
			"channel-password" => {
				with_app(|app| open_channel_password(app, arg == "wrong"));
			}
			// A screenshot cannot hover.
			"actions" => nav.set_show_actions(true),
			// Home as before the first server, over the sample data.
			"first-run" => nav.set_first_run(true),
			// `end`: further back, so the divider is above the view.
			"unread" => {
				with_app(|app| open_unread(app, if arg == "end" { 10 } else { 5 }));
			}
			"link-confirm" => {
				let url = if arg.is_empty() { DEMO_RAID_BOARD } else { arg };
				with_app(|app| app.open_link_text(url, true));
			}
			// As a link from the platform comes.
			"link" => crate::inbox::request(crate::inbox::Request::OpenLink(arg.to_owned())),
			"watch" | "popout" => {
				with_app(|app| {
					if switches.demo_ui { app.demo_watch() } else { app.watch_first_stream() }
				});
				ui.global::<Bridge>().set_viewer_popped(what == "popout");
			}
			// Home, friends, messages, events, the bell, the search.
			"friends" => {
				with_app(|app| app.select_contact(arg.to_owned()));
				nav.invoke_show(Page::Friends);
			}
			"messages" | "dm" => {
				let uid = if arg.is_empty() { "demo-3" } else { arg };
				with_app(|app| app.open_dm_with(Some(DEMO), uid));
				nav.invoke_show(Page::Messages);
			}
			"inbox" => {
				nav.set_dm_tab(2);
				with_app(|app| app.dm_tab(2));
				nav.invoke_show(Page::Messages);
			}
			"library" => nav.invoke_show(Page::Library),
			"events" => nav.invoke_show(Page::Events),
			"event-form" => nav.invoke_create_event(),
			"search" => nav.invoke_open_search(arg.into()),
			"notifications" => nav.set_notifications_open(true),
			"offline" => nav.invoke_write_offline("demo-ari".into(), "Ari".into()),
			"picture" => {
				let picture = demo_picture(1280, 720, 16);
				let image = crate::images::picture("demo:lightbox", &picture);
				nav.set_lightbox(image);
				nav.set_lightbox_open(true);
			}
			// The camera preview of the settings (the test pattern).
			"camera" => {
				with_app(|app| {
					app.start_camera();
					app.refresh_pages();
				});
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
	// `unread`: up to the divider once the list is laid out (after the
	// resize above).
	let scroll = switches.open.iter().any(|o| o == "unread").then(|| {
		let weak = ui.as_weak();
		let timer = slint::Timer::default();
		timer.start(
			slint::TimerMode::SingleShot,
			Duration::from_millis(switches.screenshot_delay * 1000 * 3 / 4),
			move || {
				if let Some(ui) = weak.upgrade() {
					let bridge = ui.global::<Bridge>();
					bridge.set_jump_index(bridge.get_unread_index());
					bridge.set_jump_requests(bridge.get_jump_requests().wrapping_add(1));
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
					// Also when the studio's own window is open.
					let _ = slint::quit_event_loop();
				}
			},
		);
		timer
	});
	Running { _screenshot: screenshot, _resize: resize, _scroll: scroll }
}

/// The current chat's read marker `back` messages before its end, as if
/// they came while away, its "New" divider shown (`VOELIN_OPEN=unread`).
fn open_unread(app: &mut App, back: usize) {
	let Some(id) = app.current else { return };
	let own_uids = app.social.own_uids.clone();
	let Some(view) = app.sessions.get_mut(&id) else { return };
	let own_client = view.state.own_client;
	let own = |m: &ChatMessage| crate::social::own_message(&own_uids, own_client, m);
	let index = view.current_tab;
	let tab = &mut view.tabs[index];
	let Some(at) = tab.messages.len().checked_sub(back + 1) else { return };
	let read = &tab.messages[at].message;
	tab.read = Some((read.message.ts_ms, read.id));
	tab.divider = None;
	tab.count_unread(own);
	tab.enter(own);
	app.refresh_chat();
	app.refresh_servers();
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

/// `VOELIN_OPEN=channel-password`: join the current server's first locked
/// channel, which asks for its password; `wrong` as if the server had
/// refused one.
fn open_channel_password(app: &mut App, wrong: bool) {
	let Some(session) = app.current else { return };
	let locked = app
		.view()
		.and_then(|v| v.presence.channels.values().filter(|c| c.has_password).map(|c| c.id).min());
	let Some(channel) = locked else { return };
	if wrong {
		if let Some(view) = app.view_mut() {
			view.channel_passwords.insert(channel, "guess".into());
		}
		app.join_failed(session, channel, JoinFailure::Password);
	} else {
		app.join_channel(session, channel);
	}
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
		cached_server_icon: None,
	};
	app.bookmarks = vec![
		Bookmark {
			gateway_url: Some("wss://gw.nightfall.example/v1".into()),
			..bookmark(DEMO, "Nightfall Guild", "ts.nightfall.example:9987")
		},
		bookmark(DEMO + 1, "Pixel Lounge", "pixel.example"),
		bookmark(DEMO + 2, "Dev TS3", "127.0.0.1:9987"),
	];
	// Home's "Continue where you left off": a channel of a server without
	// voice, so it shows Join.
	app.settings.last_voice = Some(LastVoice {
		bookmark: DEMO + 1,
		address: "pixel.example".into(),
		channel: vec!["Art Corner".into()],
	});
	app.current = Some(DEMO);
	let session = DEMO as u64;

	let mut p = Presence { server_name: "Nightfall Guild".into(), ..Default::default() };
	p.server.welcome_message =
		"Welcome to [b]Nightfall Guild[/b]! Raids on Tuesdays and Fridays, all levels welcome."
			.into();
	// The host banner, a server icon, backgrounds on three channels and an icon
	// on another; the pictures arrive below (`demo_pictures`).
	p.server.banner_gfx_url = DEMO_HOST_BANNER.into();
	p.server.banner_mode = BannerMode::KeepAspect;
	p.server.icon = DEMO_SERVER_ICON;
	let mut chill = channel(2, 0, 1, "Chill Zone");
	chill.max_clients = Some(50);
	chill.topic = Some("Hang out, chat, and explore the worlds beyond.".into());
	chill.banner_gfx_url = Some(DEMO_CHILL_BANNER.into());
	chill.banner_mode = BannerMode::IgnoreAspect;
	let mut raid = channel(4, 3, 0, "Raid Night");
	raid.banner_gfx_url = Some(DEMO_RAID_BANNER.into());
	raid.banner_mode = BannerMode::KeepAspect;
	// Only the raid leads speak there: Zeph cannot (the members panel's hand).
	raid.needed_talk_power = 50;
	let mut music = channel(5, 0, 3, "Music");
	music.icon = DEMO_MUSIC_ICON;
	music.banner_gfx_url = Some(DEMO_MUSIC_BANNER.into());
	music.banner_mode = BannerMode::NoAdjust;
	let mut locked = channel(7, 0, 9, "Officers");
	locked.has_password = true;
	// Spacers as servers name them: a centred heading, one on the right and
	// a line.
	for c in [
		channel(1, 0, 0, "Lobby"),
		chill,
		channel(3, 0, 2, "[cspacer0]Gaming"),
		raid,
		music,
		channel(9, 0, 5, "[rspacer2]Staff"),
		locked,
		channel(8, 0, 7, "[*spacer1]---"),
		channel(6, 0, 8, "AFK"),
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
	// Live in other channels (the gateway's directory has their streams).
	clients[5].streaming = Some(true);
	clients[6].streaming = Some(true);
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
	// myTeamSpeak badges; their pictures arrive below.
	clients[0].badges = DEMO_BADGES[..2].iter().map(|g| g.to_string()).collect();
	clients[1].badges = DEMO_BADGES[2..].iter().map(|g| g.to_string()).collect();
	for c in clients {
		p.clients.insert(c.id, c);
	}

	let group = |id: u64, sort_id: i32, name: &str| GroupInfo {
		id,
		name: name.into(),
		icon: match id {
			GROUP_ADMIN => DEMO_ADMIN_ICON,
			GROUP_MOD => DEMO_MOD_ICON,
			_ => 0,
		},
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
					feature::EVENTS.into(),
					feature::STREAMS.into(),
					feature::ACTIVITY.into(),
					feature::ADMIN.into(),
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
				viewers: Some(12),
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
	demo_pictures(app);
	demo_chat(app, session);
	demo_social(app);
	// No toast over the screenshots.
	app.set_status("");
	app.refresh_all();
}

/// Sample people, private chats, pokes, notifications, events, a picture
/// and recordings for the home, friends, messages, events and settings
/// screens (`VOELIN_DEMO_UI`).
fn demo_social(app: &mut App) {
	let session = DEMO as u64;
	let now = chrono::Utc::now().timestamp_millis();
	let min = 60_000;
	// The second sample server is observed: a few people there.
	let mut lounge = Presence { server_name: "Pixel Lounge".into(), ..Default::default() };
	for c in [channel(1, 0, 0, "Lobby"), channel(2, 0, 1, "Art Corner"), channel(3, 0, 2, "Music")]
	{
		lounge.channels.insert(c.id, c);
	}
	let mut ivy = client(21, 2, "Ivy");
	ivy.uid = Some("demo-ivy".into());
	let mut sora = client(22, 3, "Sora");
	sora.uid = Some("demo-sora".into());
	sora.away = Some("painting".into());
	let mut pixel = client(23, 1, "Pixel");
	pixel.uid = Some("demo-pixel".into());
	for c in [ivy, sora, pixel] {
		lounge.clients.insert(c.id, c);
	}
	app.handle_event(Event::Presence { session: session + 1, presence: Arc::new(lounge) });
	app.handle_event(Event::State {
		session: session + 1,
		state: SessionState {
			observe: ObserveState::Observing,
			presence_source: Some(Source::Gateway),
			server_uid: Some("demo-lounge".into()),
			..Default::default()
		},
	});
	app.handle_event(Event::ServerInfo {
		session: session + 1,
		name: "Pixel Lounge".into(),
		flavor: ServerFlavor::from_version_string("3.13.8 [Build: 1]"),
		capabilities: Default::default(),
	});

	// Contacts: friends here and there, some offline, one blocked.
	let contact =
		|uid: &str, nick: &str, relation: Relation, seen_min: i64, server: &str| Contact {
			uid: uid.into(),
			nickname: nick.into(),
			relation,
			note: String::new(),
			muted: false,
			volume: 1.0,
			added_ms: now - 400 * 86_400_000,
			last_seen_ms: if seen_min > 0 { now - seen_min * min } else { 0 },
			last_server: (!server.is_empty()).then(|| server.to_owned()),
		};
	let mut kairo = contact("demo-3", "Kairo", Relation::Friend, 1, "Nightfall Guild");
	kairo.note = "Tank on Tuesday raids. Always down for co-op.".into();
	let contacts = vec![
		contact("demo-2", "Lumen", Relation::Friend, 1, "Nightfall Guild"),
		kairo,
		contact("demo-4", "Mira", Relation::Friend, 1, "Nightfall Guild"),
		contact("demo-5", "dex", Relation::Friend, 1, "Nightfall Guild"),
		contact("demo-6", "Talon", Relation::Friend, 1, "Nightfall Guild"),
		contact("demo-7", "Zeph", Relation::Friend, 1, "Nightfall Guild"),
		contact("demo-ivy", "Ivy", Relation::Friend, 1, "Pixel Lounge"),
		contact("demo-sora", "Sora", Relation::Friend, 1, "Pixel Lounge"),
		contact("demo-ari", "Ari", Relation::Friend, 130, "Nightfall Guild"),
		contact("demo-neon", "Neon", Relation::Friend, 26 * 60, "Dev TS3"),
		contact("demo-9", "Rin", Relation::Neutral, 1, "Nightfall Guild"),
		contact("demo-spam", "FreeSkinsBot", Relation::Blocked, 3 * 24 * 60, "Pixel Lounge"),
	];
	app.handle_event(Event::ContactsChanged { contacts: Arc::new(contacts) });
	for (uid, ago) in [
		("demo-2", 12),
		("demo-3", 28),
		("demo-4", 42),
		("demo-5", 64),
		("demo-6", 95),
		("demo-7", 130),
		("demo-ivy", 8),
		("demo-sora", 50),
	] {
		app.social.online_since.insert(uid.into(), now - ago * min);
	}

	// Private chats: with Kairo (a picture, a file), Mira, Lumen, dex,
	// and Ivy on the other server; Neon's is only stored.
	let picture = file_link(2, "night-castle.png", 412_331);
	let private =
		|id: i64, peer: &str, from_me: bool, author: (&str, u16), text: String, ago: i64| {
			HistoryMessage {
				id,
				message: ChatMessage {
					target: ChatTarget::Private(peer.into()),
					author_name: author.0.into(),
					author_uid: Some(if from_me { "demo-1".into() } else { peer.into() }),
					author_id: (author.1 > 0).then_some(author.1),
					text,
					ts_ms: now - ago * min,
					via_relay: false,
					blocked: false,
				},
				source: MessageSource::Voice,
				remote_id: None,
				topic_id: None,
				reactions: Vec::new(),
				pinned: false,
				rev: 0,
			}
		};
	let nova = ("Nova", 1);
	let kairo = ("Kairo", 3);
	let lines = vec![
		private(
			300,
			"demo-3",
			true,
			nova,
			format!("Hey! Check out this shot I got last night while exploring 👀 {picture}"),
			95,
		),
		private(301, "demo-3", false, kairo, "That looks incredible! Where was this?".into(), 93),
		private(
			302,
			"demo-3",
			true,
			nova,
			"In the northern region. The lighting was perfect.".into(),
			91,
		),
		private(
			303,
			"demo-3",
			false,
			kairo,
			"We should explore together this weekend! I found some cool locations too.".into(),
			89,
		),
		private(
			304,
			"demo-3",
			false,
			kairo,
			format!(
				"Here are the spots I marked. {}",
				file_link(2, "exploration-routes.pdf", 2_516_582)
			),
			88,
		),
		private(305, "demo-3", true, nova, "That sunset looks unreal. Let's do it! 🙌".into(), 6),
		private(
			310,
			"demo-4",
			false,
			("Mira", 4),
			"Working on a new video today, want to see a sneak peek?".into(),
			70,
		),
		private(311, "demo-2", false, ("Lumen", 2), "Let's run it tonight!".into(), 120),
		private(312, "demo-5", false, ("dex", 5), file_link(2, "map-notes.png", 96_120), 26 * 60),
	];
	for m in lines {
		let target = m.message.target.clone();
		app.handle_event(Event::ChatHistory {
			session,
			target,
			messages: vec![m],
			source: HistorySource::Gateway,
			complete: true,
		});
	}
	app.handle_event(Event::ChatHistory {
		session: session + 1,
		target: ChatTarget::Private("demo-ivy".into()),
		messages: vec![HistoryMessage {
			message: ChatMessage {
				target: ChatTarget::Private("demo-ivy".into()),
				author_name: "Ivy".into(),
				author_uid: Some("demo-ivy".into()),
				author_id: Some(21),
				text: "Same here! The new channel is great.".into(),
				ts_ms: now - 30 * min,
				via_relay: true,
				blocked: false,
			},
			..private(320, "demo-ivy", false, ("Ivy", 21), String::new(), 30)
		}],
		source: HistorySource::Gateway,
		complete: true,
	});
	let mut neon = private(
		330,
		"demo-neon",
		false,
		("Neon", 0),
		"Sent you the config, check the server folder.".into(),
		3 * 24 * 60,
	);
	neon.message.author_id = None;
	app.social.dm.stored.push(("demo-dev".into(), neon));
	app.social.dm.bookmark_of.insert("demo-dev".into(), DEMO + 2);
	// The samples came one by one, as new ones do: all are read (no "New"
	// line, which a gateway page on screen gets), but for Mira's and
	// Lumen's messages, which came while away.
	for view in app.sessions.values_mut() {
		for tab in &mut view.tabs {
			tab.divider = None;
			tab.catch_up();
			match &tab.target {
				ChatTarget::Private(uid) if uid == "demo-4" => tab.unread = 1,
				ChatTarget::Private(uid) if uid == "demo-2" => tab.unread = 2,
				_ => continue,
			}
			tab.read = None;
		}
	}
	// A poke from Kairo, shown among the messages and in the bell.
	app.handle_event(Event::Poke {
		session,
		from: 3,
		from_uid: Some("demo-3".into()),
		from_name: "Kairo".into(),
		message: "raid in 5?".into(),
		blocked: false,
	});
	if let Some(p) = app.social.pokes.last_mut() {
		p.ts_ms = now - 7 * min;
	}
	// Offline messages the server keeps (the Inbox tab).
	let mail = |id: u32, from: &str, subject: &str, ago_min: i64, read: bool| OfflineMessageInfo {
		id,
		from_uid: from.into(),
		subject: subject.into(),
		ts_s: (now - ago_min * min) / 1000,
		read,
	};
	app.handle_event(Event::OfflineMessages {
		session,
		request: 0,
		result: Ok(vec![
			mail(1, "demo-ari", "Raid roster for Friday", 3 * 60, false),
			mail(2, "demo-neon", "The config for the dev server", 26 * 60, true),
			mail(3, "demo-4", "Thumbnails for the stream", 3 * 24 * 60, true),
		]),
	});

	// The pictures of the links, as if downloaded.
	if let Some(view) = app.sessions.get_mut(&DEMO) {
		for (channel, name) in
			[(2, "night-castle.png"), (2, "guild-banner.png"), (2, "map-notes.png")]
		{
			let file = voelin_model::FileRef {
				channel,
				path: "/".into(),
				name: name.into(),
				..Default::default()
			};
			view.extra.previews.insert(
				crate::previews::link_key(&file),
				crate::previews::Preview::Ready(voelin_core::Bytes(
					demo_picture(640, 360, name.len() as u32).into(),
				)),
			);
		}
	}

	// The gateway's events (one is live), its configuration and rules.
	let by = |name: &str, uid: &str| UserRef { uid: uid.into(), name: name.into() };
	let event =
		|id: i64, title: &str, start: i64, hours: i64, channel: Option<u64>, stream: bool| {
			EventInfo {
				id,
				spec: EventSpec {
					title: title.into(),
					description: String::new(),
					start_ms: start,
					end_ms: Some(start + hours * 3_600_000),
					channel,
					kind: if stream { EventKind::Stream } else { EventKind::General },
					stream_title: None,
					stream_game: None,
					host_uid: None,
				},
				creator: by("Nova", "demo-1"),
				created_ms: now - 86_400_000,
				updated_ms: now - 86_400_000,
				going: 0,
				maybe: 0,
				not_going: 0,
				my_rsvp: None,
				attendees: Vec::new(),
				live_stream: None,
			}
		};
	let hour = 3_600_000;
	let tomorrow = (now / 86_400_000 + 1) * 86_400_000 + 18 * hour;
	let mut raid = event(1, "Raid Night: The Frozen Citadel", tomorrow, 3, Some(4), false);
	raid.spec.description = "Bring elixirs and fire resistance. Voice in Raid Night.".into();
	raid.going = 12;
	raid.maybe = 4;
	raid.not_going = 1;
	raid.my_rsvp = Some(RsvpStatus::Going);
	raid.attendees = vec![
		Attendee { user: by("Nova", "demo-1"), status: RsvpStatus::Going, ts_ms: now },
		Attendee { user: by("Kairo", "demo-3"), status: RsvpStatus::Going, ts_ms: now },
		Attendee { user: by("Mira", "demo-4"), status: RsvpStatus::Maybe, ts_ms: now },
		Attendee { user: by("dex", "demo-5"), status: RsvpStatus::NotGoing, ts_ms: now },
	];
	let mut live = event(2, "Exploring the Lands Between", now - 12 * min, 2, Some(2), true);
	live.spec.stream_title = Some("Exploring the Lands Between".into());
	live.spec.stream_game = Some("Elden Ring".into());
	live.creator = by("Lumen", "demo-2");
	live.going = 8;
	live.live_stream = Some("demo-stream".into());
	let mut night = event(3, "Community Game Night", tomorrow + 2 * 86_400_000, 3, None, false);
	night.spec.description = "Party games for everyone: drop in any time.".into();
	night.creator = by("Talon", "demo-6");
	night.going = 23;
	night.maybe = 9;
	let mut lofi = event(4, "Late Night Lofi & Chat", now + 50 * min, 2, Some(5), true);
	lofi.spec.stream_title = Some("Lofi beats to raid to".into());
	lofi.creator = by("Mira", "demo-4");
	lofi.going = 5;
	lofi.maybe = 2;
	app.handle_event(Event::Gateway {
		session,
		update: GatewayUpdate::Events { events: vec![raid, live, night, lofi] },
	});
	app.handle_event(Event::Gateway {
		session,
		update: GatewayUpdate::Permissions {
			channel: None,
			actions: vec![
				Action::React,
				Action::Pin,
				Action::CreateEvent,
				Action::Rsvp,
				Action::Moderate,
				Action::Admin,
			],
		},
	});
	let config = |key: &str, value: serde_json::Value, source: ConfigSource, description: &str| {
		ConfigEntry {
			key: key.into(),
			default: value.clone(),
			value,
			source,
			value_type: "int".into(),
			description: description.into(),
			bootstrap: false,
		}
	};
	app.handle_event(Event::Gateway {
		session,
		update: GatewayUpdate::Config {
			entries: vec![
				config(
					"history.retention_days",
					serde_json::json!(90),
					ConfigSource::Db,
					"Days of chat history the gateway keeps.",
				),
				config(
					"events.reminders_min",
					serde_json::json!([60, 15, 0]),
					ConfigSource::File,
					"Minutes before an event when reminders go out.",
				),
				config(
					"streams.directory",
					serde_json::json!(true),
					ConfigSource::Default,
					"Keep a directory of the server's streams.",
				),
			],
		},
	});
	app.handle_event(Event::Gateway {
		session,
		update: GatewayUpdate::PermRules {
			rules: vec![
				PermRuleInfo {
					action: Action::CreateEvent,
					rule: Some(PermRule {
						everyone: false,
						server_groups: vec![GROUP_ADMIN, GROUP_MOD],
						channel_groups: vec![],
					}),
					source: ConfigSource::Db,
					default: "server admins".into(),
				},
				PermRuleInfo {
					action: Action::Pin,
					rule: None,
					source: ConfigSource::Default,
					default: "moderators".into(),
				},
			],
		},
	});

	// Notifications: two mentions (unread: home shows them), a message, an
	// event, a friend online (and the poke above).
	use crate::social::{NoticeKind, NoticeTarget};
	app.notify(
		NoticeKind::Mention,
		"Talon mentioned you".into(),
		"Nova, can you open the raid at 20:00? — Chill Zone · Nightfall Guild".into(),
		NoticeTarget::Chat(DEMO, ChatTarget::Channel(2)),
		"Talon".into(),
		Some("demo-6".into()),
	);
	app.notify(
		NoticeKind::Mention,
		"Zeph mentioned you".into(),
		"Nova, are you healing tonight? — Raid Night · Nightfall Guild".into(),
		NoticeTarget::Chat(DEMO, ChatTarget::Channel(4)),
		"Zeph".into(),
		Some("demo-7".into()),
	);
	app.notify(
		NoticeKind::Event,
		"Late Night Lofi & Chat starts in 50 min".into(),
		"on Nightfall Guild".into(),
		NoticeTarget::Event(DEMO, 4),
		"Mira".into(),
		Some("demo-4".into()),
	);
	app.notify(
		NoticeKind::Friend,
		"Ivy is online".into(),
		"on Pixel Lounge · Art Corner".into(),
		NoticeTarget::Contact("demo-ivy".into()),
		"Ivy".into(),
		Some("demo-ivy".into()),
	);
	app.notify(
		NoticeKind::Message,
		"Mira".into(),
		"Working on a new video today, want to see a sneak peek?".into(),
		NoticeTarget::Dm(DEMO, "demo-4".into()),
		"Mira".into(),
		Some("demo-4".into()),
	);
	for (i, n) in app.social.notices.iter_mut().enumerate() {
		n.ts_ms = now - (i as i64 * 9 + 2) * min;
		n.unread = i < 3 || n.kind == NoticeKind::Mention;
	}
	app.refresh_notices();

	// Recordings and clips of the Studio.
	let recording = |name: &str, detail: &str| RecordingItem {
		name: name.into(),
		detail: detail.into(),
		clip: name.contains("clip"),
		path: format!("/tmp/{name}").into(),
	};
	crate::vm::list::sync(
		&app.models.social.recordings,
		&[
			recording("raid-night-2026-10-02.webm", "1.2 GB · 2 Oct 22:41"),
			recording("clip-boss-down.mkv", "38.4 MB · 2 Oct 21:57"),
			recording("lands-between-stream.mkv", "2.8 GB · 28 Sep 20:12"),
			recording("clip-lucky-parry.mkv", "12.1 MB · 28 Sep 19:40"),
		],
	);
	app.refresh_chats();
	app.refresh_people();
	app.refresh_home();
}

/// Addresses and icon ids of the sample server's pictures.
const DEMO_HOST_BANNER: &str = "https://nightfall.example/banner.png";
const DEMO_CHILL_BANNER: &str = "https://nightfall.example/chill.png";
const DEMO_MUSIC_BANNER: &str = "https://nightfall.example/music.png";
const DEMO_RAID_BANNER: &str = "https://nightfall.example/raid.png";
/// A picture a message shows (`[img]`).
const DEMO_LOOT_PICTURE: &str = "https://nightfall.example/loot.png";
/// The sample's masked link (`VOELIN_OPEN=link-confirm`).
const DEMO_RAID_BOARD: &str = "https://nightfall.example/raids";
const DEMO_SERVER_ICON: u32 = 3_120_211_001;
const DEMO_ADMIN_ICON: u32 = 3_120_211_002;
const DEMO_MOD_ICON: u32 = 3_120_211_003;
const DEMO_MUSIC_ICON: u32 = 3_120_211_004;
/// Real badge GUIDs: 20th Anniversary, TeamSpeak Jedi (Nova's), Gamescom
/// 2019, Pride and Year of the Tiger 2022 (Lumen's).
const DEMO_BADGES: [&str; 5] = [
	"4b27be5a-b92a-4b30-8b2d-14b59653f427",
	"64221fd1-706c-4bb2-ba55-996c39effa79",
	"b82a45a5-b235-4926-be77-de102222e5eb",
	"ceee2445-4fbf-4f06-9421-286f0f4e875a",
	"92356386-0451-4a97-87d9-10ff4f43260c",
];

/// Avatars, icons, banners and badges as the engine reports them once they
/// are in its cache (here a folder of synthetic pictures in the temporary
/// directory).
fn demo_pictures(app: &mut App) {
	let session = DEMO as u64;
	let dir = std::env::temp_dir().join("voelin-demo-pictures");
	let _ = std::fs::create_dir_all(&dir);
	let save = |name: &str, png: Vec<u8>| {
		let path = dir.join(name);
		let _ = std::fs::write(&path, png);
		path
	};
	let mut events = vec![Event::ServerDetails {
		session,
		address: app.bookmarks.iter().find(|b| b.id == DEMO).unwrap().address.clone(),
		details: Arc::new(voelin_model::ServerDetails {
			icon: DEMO_SERVER_ICON,
			..Default::default()
		}),
	}];
	for (url, picture) in [
		(DEMO_HOST_BANNER, demo_picture(256, 256, 3)),
		(DEMO_CHILL_BANNER, demo_picture(900, 120, 11)),
		(DEMO_RAID_BANNER, demo_picture(240, 240, 5)),
		(DEMO_MUSIC_BANNER, demo_picture(180, 36, 7)),
		(DEMO_LOOT_PICTURE, demo_picture(640, 300, 9)),
	] {
		let path = save(url.rsplit('/').next().unwrap_or(url), picture);
		events.push(Event::PictureReady { session, url: url.into(), path });
	}
	for (icon, color, shape) in [
		(DEMO_SERVER_ICON, [70, 92, 255], Shape::Moon),
		(DEMO_ADMIN_ICON, [242, 178, 44], Shape::Diamond),
		(DEMO_MOD_ICON, [45, 196, 132], Shape::Disc),
		(DEMO_MUSIC_ICON, [214, 76, 182], Shape::Ring),
	] {
		let path = save(&icon.to_string(), demo_icon(color, shape));
		events.push(Event::IconReady { session, icon, path });
	}
	// Stand-ins for the badges' pictures on TeamSpeak's server.
	let badge_looks = [
		([242, 178, 44], Shape::Ring),
		([90, 170, 255], Shape::Moon),
		([45, 196, 132], Shape::Diamond),
		([240, 110, 160], Shape::Disc),
		([255, 140, 90], Shape::Diamond),
	];
	for (guid, (color, shape)) in DEMO_BADGES.into_iter().zip(badge_looks) {
		let Some(url) = voelin_model::badges::icon_url(guid) else { continue };
		let path = save(&format!("badge-{guid}"), demo_icon(color, shape));
		events.push(Event::PictureReady { session, url, path });
	}
	// Some people have pictures, the rest initials.
	for (sample_session, uid, seed) in [
		(session, "demo-2", 1),
		(session, "demo-3", 2),
		(session, "demo-4", 3),
		(session, "demo-7", 4),
		(session, "demo-9", 5),
		(session + 1, "demo-ivy", 6),
	] {
		let path = save(&format!("avatar-{uid}"), demo_avatar(seed));
		events.push(Event::AvatarReady {
			session: sample_session,
			client_uid: uid.into(),
			path,
			hash: String::new(),
		});
	}
	for event in events {
		app.handle_event(event);
	}
}

/// Shapes of the sample icons.
#[derive(Clone, Copy)]
enum Shape {
	Moon,
	Diamond,
	Disc,
	Ring,
}

/// A 32×32 icon: a shape in `color` on transparency; PNG bytes.
fn demo_icon(color: [u8; 3], shape: Shape) -> Vec<u8> {
	let size = 32u32;
	let mut rgba = Vec::with_capacity((size * size * 4) as usize);
	for y in 0..size {
		for x in 0..size {
			let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
			let d = (dx * dx + dy * dy).sqrt();
			let inside = match shape {
				Shape::Moon => d < 14.0 && ((dx - 6.0).powi(2) + (dy + 5.0).powi(2)).sqrt() > 10.0,
				Shape::Diamond => dx.abs() + dy.abs() < 15.0,
				Shape::Disc => d < 13.0,
				Shape::Ring => (8.0..14.0).contains(&d) || d < 4.0,
			};
			let [r, g, b] = color;
			rgba.extend(if inside { [r, g, b, 255] } else { [0, 0, 0, 0] });
		}
	}
	png_of(size, size, &rgba)
}

/// A 96×96 portrait: a head and shoulders on a gradient; PNG bytes.
fn demo_avatar(seed: u32) -> Vec<u8> {
	const SKY: [[f32; 3]; 6] = [
		[255.0, 140.0, 90.0],
		[90.0, 170.0, 255.0],
		[170.0, 110.0, 255.0],
		[60.0, 200.0, 170.0],
		[250.0, 200.0, 80.0],
		[240.0, 110.0, 160.0],
	];
	let size = 96u32;
	let sky = SKY[seed as usize % SKY.len()];
	let mut rgba = Vec::with_capacity((size * size * 4) as usize);
	for y in 0..size {
		for x in 0..size {
			let (fx, fy) = (x as f32, y as f32);
			let t = (fx + fy) / (2.0 * size as f32);
			let mut c =
				[sky[0] * (1.0 - 0.45 * t), sky[1] * (1.0 - 0.45 * t), sky[2] * (1.0 - 0.3 * t)];
			let head = ((fx - 48.0).powi(2) + (fy - 40.0).powi(2)).sqrt() < 19.0;
			let shoulders = ((fx - 48.0) / 36.0).powi(2) + ((fy - 98.0) / 30.0).powi(2) < 1.0;
			if head || shoulders {
				c = [250.0 - c[0] * 0.25, 244.0 - c[1] * 0.25, 238.0 - c[2] * 0.2];
			}
			rgba.extend([c[0] as u8, c[1] as u8, c[2] as u8, 255]);
		}
	}
	png_of(size, size, &rgba)
}

/// RGBA pixels as PNG bytes.
fn png_of(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
	let mut out = Vec::new();
	let mut encoder = png::Encoder::new(&mut out, width, height);
	encoder.set_color(png::ColorType::Rgba);
	encoder.set_depth(png::BitDepth::Eight);
	if let Ok(mut writer) = encoder.write_header() {
		let _ = writer.write_image_data(rgba);
	}
	out
}

/// A painted night: sky, moon, stars, hills and a castle; PNG bytes.
fn demo_picture(width: u32, height: u32, seed: u32) -> Vec<u8> {
	let mut rgba = Vec::with_capacity((width * height * 4) as usize);
	let (w, h) = (width as f32, height as f32);
	let (mx, my, mr) = (w * 0.68, h * 0.3, h * 0.17);
	let hash = |x: u32, y: u32| {
		let mut v = x.wrapping_mul(374_761_393)
			^ y.wrapping_mul(668_265_263)
			^ seed.wrapping_mul(2_654_435_761);
		v = (v ^ (v >> 13)).wrapping_mul(1_274_126_177);
		v ^ (v >> 16)
	};
	for y in 0..height {
		for x in 0..width {
			let (fx, fy) = (x as f32, y as f32);
			let t = fy / h;
			// Sky: deep blue to violet at the horizon.
			let mut c = [12.0 + 50.0 * t, 18.0 + 30.0 * t, 60.0 + 90.0 * t];
			// Moon and its glow.
			let d = ((fx - mx).powi(2) + (fy - my).powi(2)).sqrt();
			if d < mr {
				let shade = 1.0 - 0.25 * ((fx - mx + mr * 0.3) / mr).max(0.0);
				c = [200.0 * shade, 210.0 * shade, 245.0 * shade];
			} else {
				let glow = (1.0 - (d - mr) / (mr * 2.5)).max(0.0) * 60.0;
				c = [c[0] + glow, c[1] + glow, c[2] + glow * 1.2];
			}
			if hash(x, y) % 900 == 0 && t < 0.6 {
				c = [235.0, 235.0, 255.0];
			}
			// Hills, then a castle on the middle one.
			let far = h * 0.62 + (fx / w * 9.0 + seed as f32).sin() * h * 0.05;
			let near = h * 0.78 + (fx / w * 5.0 + 1.3).sin() * h * 0.06;
			let castle = (fx > w * 0.36
				&& fx < w * 0.52
				&& fy > h * 0.38
				&& ((((fx - w * 0.36) / (w * 0.02)) as u32).is_multiple_of(2) || fy > h * 0.45))
				|| (fx > w * 0.42 && fx < w * 0.46 && fy > h * 0.24);
			if fy > near {
				c = [6.0, 10.0, 26.0];
			} else if fy > far || castle {
				c = [16.0, 20.0, 52.0];
				// Lit windows.
				if castle && hash(x / 6, y / 8) % 7 == 0 && (x % 6) < 3 && (y % 8) < 4 {
					c = [255.0, 190.0, 90.0];
				}
			}
			rgba.extend([c[0].min(255.0) as u8, c[1].min(255.0) as u8, c[2].min(255.0) as u8, 255]);
		}
	}
	png_of(width, height, &rgba)
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
		// Formatting as TeamSpeak clients send it (BBCode).
		(
			DEMO,
			"Nova",
			1,
			format!(
				"[b]Raid night[/b] moves from [s]20:00[/s] to [color=#ff8a3d]20:30[/color], \
				 sign up on the [url={DEMO_RAID_BOARD}]raid board[/url]."
			),
			9,
			false,
			vec![react("👍", 4, true)],
			true,
			None,
		),
		(
			DEMO,
			"Kairo",
			3,
			"[quote=Nova]bring elixirs[/quote]\nAlready stocked up 🧪".into(),
			8,
			false,
			Vec::new(),
			false,
			None,
		),
		(
			DEMO,
			"Mira",
			4,
			"Roles for tonight:\n[list]\n[*][b]Tank:[/b] Kairo\n[*][b]Healer:[/b] Mira\n\
			 [*][i]Everyone else:[/i] damage[/list]\nPull timer: [code]/pull 10[/code]"
				.into(),
			7,
			false,
			Vec::new(),
			false,
			None,
		),
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
		(
			DEMO,
			"Talon",
			6,
			format!("Last week's loot [img]{DEMO_LOOT_PICTURE}[/img]"),
			6,
			false,
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
			streams: vec![
				StreamEntry {
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
				},
				StreamEntry {
					id: "demo-stream-2".into(),
					stream_id: Some("demo-stream-2".into()),
					streamer: UserRef { uid: "demo-6".into(), name: "Talon".into() },
					client_id: Some(6),
					channel: Some(3),
					title: "Ranked Grind & Vibes".into(),
					kind: "screen".into(),
					started_ms: now - 41 * 60_000,
					viewers: Some(38),
					source: StreamSource::Registered,
					event_id: None,
				},
				StreamEntry {
					id: "demo-stream-3".into(),
					stream_id: Some("demo-stream-3".into()),
					streamer: UserRef { uid: "demo-7".into(), name: "Zeph".into() },
					client_id: Some(7),
					channel: Some(4),
					title: "Raid prep: gear check".into(),
					kind: "camera".into(),
					started_ms: now - 8 * 60_000,
					viewers: Some(9),
					source: StreamSource::Registered,
					event_id: None,
				},
			],
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

/// The sample messages before the sample chats' first ones: how many, how
/// many a page, and their ids (the sample chats use ids below 500).
const OLDER: i64 = 30;
const OLDER_PAGE: i64 = 15;
const OLDER_ID: i64 = 500;

/// The page before `oldest` of a sample chat, as the engine answers
/// `Command::LoadOlderHistory`: older small talk, a page at a time, the last
/// one `complete`. The first page goes back from `oldest` (from `now_ms` in
/// an empty chat), the next ones from the sample message before.
pub(crate) fn demo_older(
	target: &ChatTarget,
	oldest: Option<&HistoryMessage>,
	now_ms: i64,
) -> (Vec<HistoryMessage>, bool) {
	const PEOPLE: [(&str, u16); 6] =
		[("Nova", 1), ("Kairo", 3), ("Mira", 4), ("dex", 5), ("Talon", 6), ("Lumen", 2)];
	const TEXTS: [&str; 10] = [
		"Anyone still around?",
		"Patch notes are up, the healer changes look good.",
		"Who has the key for the vault run?",
		"I can do Thursday, not Friday.",
		"Lag spike on my side, back in a sec.",
		"gg everyone, that was close 😅",
		"Is the new map in the rotation yet?",
		"Uploading the clip from last night.",
		"Coffee first, then raids ☕",
		"Same time next week?",
	];
	let (first, from_ms) = match oldest {
		Some(m) if m.id >= OLDER_ID => (m.id - OLDER_ID + 1, m.message.ts_ms),
		Some(m) => (0, m.message.ts_ms),
		None => (0, now_ms),
	};
	let last = (first + OLDER_PAGE).min(OLDER);
	let messages = (first..last)
		.map(|n| {
			// Two in a row by each person.
			let (author, client) = PEOPLE[(n / 2 % 6) as usize];
			HistoryMessage {
				id: OLDER_ID + n,
				message: ChatMessage {
					target: target.clone(),
					author_name: author.into(),
					author_uid: Some(format!("demo-{client}")),
					author_id: Some(client),
					text: TEXTS[(n % 10) as usize].into(),
					ts_ms: from_ms - (n - first + 1) * 23 * 60_000,
					via_relay: false,
					blocked: false,
				},
				source: MessageSource::Voice,
				remote_id: Some(OLDER_ID + n),
				topic_id: None,
				reactions: Vec::new(),
				pinned: false,
				rev: 1,
			}
		})
		.collect();
	(messages, last >= OLDER)
}

/// Answer [`App::load_older`] in demo mode (no engine runs) with
/// [`demo_older`], a moment later as a server would.
pub(crate) fn answer_older(session: i64, target: ChatTarget, oldest: Option<HistoryMessage>) {
	slint::Timer::single_shot(Duration::from_millis(600), move || {
		let now = chrono::Utc::now().timestamp_millis();
		let (messages, complete) = demo_older(&target, oldest.as_ref(), now);
		with_app(|app| {
			app.handle_event(Event::ChatHistory {
				session: session as u64,
				target,
				messages,
				source: HistorySource::Gateway,
				complete,
			});
		});
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Two pages back from the sample chat's first message, each older than
	/// the one before, the second one the last.
	#[test]
	fn demo_older_pages_in_order_and_completes() {
		let target = ChatTarget::Channel(2);
		let (first, complete) = demo_older(&target, None, 1_000_000_000);
		assert_eq!(first.len(), OLDER_PAGE as usize);
		assert!(!complete, "a second page follows");
		let ids: Vec<i64> = first.iter().map(|m| m.id).collect();
		assert_eq!(ids, (OLDER_ID..OLDER_ID + OLDER_PAGE).collect::<Vec<_>>());
		assert!(first.windows(2).all(|w| w[1].message.ts_ms < w[0].message.ts_ms));
		assert!(
			first.iter().all(|m| m.message.target == target && m.message.ts_ms < 1_000_000_000)
		);

		let oldest = first.last().unwrap();
		let (second, complete) = demo_older(&target, Some(oldest), 0);
		assert!(complete);
		assert_eq!(second.len(), (OLDER - OLDER_PAGE) as usize);
		assert_eq!(second[0].id, oldest.id + 1);
		assert!(second.iter().all(|m| m.message.ts_ms < oldest.message.ts_ms));

		// A sample chat's own message: back from it.
		let sample = HistoryMessage { id: 3, ..oldest.clone() };
		let (again, _) = demo_older(&target, Some(&sample), 0);
		assert_eq!(again[0].id, OLDER_ID);
		assert!(again[0].message.ts_ms < sample.message.ts_ms);

		// Nothing after the last page.
		let (rest, complete) = demo_older(&target, second.last(), 0);
		assert!(rest.is_empty() && complete);
	}
}
