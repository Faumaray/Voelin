//! Chat: tabs per session (server, channels, private chats), their
//! messages, and what a gateway adds to them (pins, reactions, topics).
//!
//! The engine keeps the history: [`Event::ChatHistory`] is an upsert of
//! messages by their local id (see `voelin_core::history`), so a tab holds
//! the messages it was given, ordered by `(ts_ms, id)`, and the lines of
//! the selected tab are rebuilt from them with [`vm::list::sync`], which
//! touches only the rows that changed. Sessions the engine keeps no history
//! for (it does not know the server yet) fall back to [`Event::Chat`].
//!
//! Messages are read while their tab is on screen in the focused window
//! ([`App::track_reading`]). A tab remembers the newest message read
//! (`Tab::read`, kept in the store's `chat_reads`), counts what came after
//! it, and when it comes on screen shows a "New" divider above the first
//! of those until it is left.

use std::collections::HashMap;
use std::path::PathBuf;

use slint::{ComponentHandle, ModelRc};
use voelin_core::gateway::Pin;
use voelin_core::settings::CACHE_FETCH_IMAGES;
use voelin_core::{
	Command, DownloadTo, GatewayRequest, GatewayUpdate, HistoryMessage, HistorySource,
	TransferState,
};
use voelin_gateway_proto::{ErrorCode, TopicInfo, feature};
use voelin_model::{ChatMessage, ChatTarget, HostMessageMode, ServerDetails};
use voelin_store::ChatRead;

use crate::app::{App, Bridge, ChatLine, ChatTab, FileItem, Msg, Nav, PinItem, SessionView, Tab};
use crate::settings::UI_IMAGE_PREVIEW_KB;
use crate::vm;
use crate::vm::bbcode::LinkKind;
use crate::vm::chat::{LineCache, LineCtx, Previous};

/// Where the server's welcome and host message keep what their lines were
/// built from (`LineCache`), apart from the messages' ids.
const SERVER_LINES: [i64; 2] = [i64::MIN, i64::MIN + 1];

/// What the server says to everyone who connects, with where its line keeps
/// its parts: its welcome message, and its host message when that goes to
/// the chat log.
pub(crate) fn server_texts(details: &ServerDetails) -> Vec<(&str, i64)> {
	let host = details.host_message_mode == HostMessageMode::Log;
	[Some(&details.welcome_message), host.then_some(&details.host_message)]
		.into_iter()
		.zip(SERVER_LINES)
		.filter_map(|(text, id)| Some((text.filter(|t| !t.trim().is_empty())?.as_str(), id)))
		.collect()
}

/// The text of the server's line `key` (-1, -2, …) of a server chat
/// ([`App::lines_of`]), if `tab` is one.
pub(crate) fn server_text<'a>(view: &'a SessionView, tab: &Tab, key: i32) -> Option<&'a str> {
	if tab.target != ChatTarget::Server || key >= 0 {
		return None;
	}
	server_texts(&view.presence.server).get((-1 - key) as usize).map(|(text, _)| *text)
}

/// The store's name of a chat.
pub(crate) fn store_target(target: &ChatTarget) -> voelin_store::ChatTarget {
	match target {
		ChatTarget::Server => voelin_store::ChatTarget::Server,
		ChatTarget::Channel(cid) => voelin_store::ChatTarget::Channel(*cid),
		ChatTarget::Private(uid) => voelin_store::ChatTarget::Private(uid.clone()),
	}
}

/// A file being downloaded from a chat message.
pub(crate) struct Download {
	/// The message's handle and which of its links this is.
	pub key: i32,
	pub index: usize,
	pub name: String,
	pub state: TransferState,
}

/// A pinned message of a chat, with who pinned it.
pub(crate) struct PinRow {
	pub key: i32,
	pub message: HistoryMessage,
	pub by: String,
	pub ts_ms: i64,
}

/// "3 min ago", "yesterday", "12 Mar".
fn ago(ts_ms: i64) -> String {
	let Some(then) = chrono::DateTime::from_timestamp_millis(ts_ms) else { return String::new() };
	let minutes = (chrono::Utc::now() - then).num_minutes();
	match minutes {
		i64::MIN..=0 => "just now".into(),
		1 => "1 min ago".into(),
		2..=59 => format!("{minutes} min ago"),
		60..=119 => "1 hour ago".into(),
		120..=1439 => format!("{} hours ago", minutes / 60),
		1440..=2879 => "yesterday".into(),
		_ => then.with_timezone(&chrono::Local).format("%e %b").to_string().trim().into(),
	}
}

impl App {
	pub(crate) fn open_chat(&mut self, target: ChatTarget, focus: bool) {
		let Some(id) = self.current else { return };
		let title = match &target {
			ChatTarget::Channel(cid) => {
				let name = self
					.sessions
					.get(&id)
					.and_then(|v| v.presence.channels.get(cid))
					.map(|c| vm::tree::channel_title(c).0.to_owned());
				format!("#{}", name.unwrap_or_else(|| cid.to_string()))
			}
			ChatTarget::Server => "Server".into(),
			ChatTarget::Private(uid) => format!("@{}", self.peer_name(uid, None)),
		};
		let view = self.sessions.entry(id).or_default();
		let index = match view.tabs.iter().position(|t| t.target == target) {
			Some(i) => i,
			None => {
				let tab = view.new_tab(target.clone(), title);
				view.tabs.push(tab);
				if !self.demo_ui {
					self.engine.send(Command::OpenChat { session: id as u64, target });
				}
				view.tabs.len() - 1
			}
		};
		if focus {
			view.current_tab = index;
			self.track_reading();
		}
		self.refresh_chat();
		self.refresh_servers();
	}

	pub(crate) fn select_tab(&mut self, index: usize) {
		if let Some(view) = self.view_mut()
			&& index < view.tabs.len()
		{
			view.current_tab = index;
			view.tabs[index].topic = None;
		}
		if let Some(id) = self.current {
			self.fetch_previews(id);
		}
		self.track_reading();
		self.refresh_chat();
		self.refresh_servers();
	}

	pub(crate) fn close_tab(&mut self, index: usize) {
		let Some(id) = self.current else { return };
		let view = self.sessions.entry(id).or_default();
		if index == 0 || index >= view.tabs.len() {
			return;
		}
		self.save_read(id, index);
		let view = self.sessions.entry(id).or_default();
		let tab = view.tabs.remove(index);
		view.current_tab = view.current_tab.min(view.tabs.len() - 1);
		self.engine.send(Command::CloseChat { session: id as u64, target: tab.target });
		self.track_reading();
		self.refresh_chat();
		self.refresh_servers();
	}

	pub(crate) fn send_message(&mut self, text: String) {
		let Some(id) = self.current else { return };
		let Some(view) = self.sessions.get(&id) else { return };
		let target = view.tabs[view.current_tab].target.clone();
		self.engine.send(Command::SendChat { session: id as u64, target, text });
	}

	/// The index of `target`'s tab, opening one if it is new.
	fn tab_for(
		view: &mut SessionView,
		target: &ChatTarget,
		title: impl FnOnce() -> String,
	) -> usize {
		match view.tabs.iter().position(|t| t.target == *target) {
			Some(i) => i,
			None => {
				let tab = view.new_tab(target.clone(), title());
				view.tabs.push(tab);
				view.tabs.len() - 1
			}
		}
	}

	fn tab_title(view: &SessionView, target: &ChatTarget, author: &str) -> String {
		match target {
			ChatTarget::Channel(cid) => format!(
				"#{}",
				view.presence
					.channels
					.get(cid)
					.map(|c| vm::tree::channel_title(c).0.to_owned())
					.unwrap_or_else(|| cid.to_string())
			),
			ChatTarget::Private(_) => format!("@{author}"),
			ChatTarget::Server => "Server".into(),
		}
	}

	/// A live message of a session without stored history: appended as it
	/// comes, without an id.
	pub(crate) fn add_message(&mut self, id: i64, message: ChatMessage) {
		let current = self.current == Some(id);
		let reading = self.reads_now(id);
		let new = !self.is_own(id, &message);
		let view = self.sessions.entry(id).or_default();
		let title = Self::tab_title(view, &message.target, &message.author_name);
		let index = Self::tab_for(view, &message.target, || title);
		let seen = reading && view.current_tab == index;
		let tab = &mut view.tabs[index];
		let line = vm::chat::line(&message, tab.last.as_ref());
		tab.last = Some(Previous::of(&message));
		tab.lines.push(line);
		let counted = new && !seen;
		if counted {
			tab.unread += 1;
		}
		if current {
			self.refresh_chat();
		}
		if counted {
			self.refresh_servers();
		}
		self.studio_chat_changed(id, &message.target);
	}

	/// Messages the engine stored: added or updated by their local id.
	pub(crate) fn history_batch(
		&mut self,
		id: i64,
		target: &ChatTarget,
		messages: Vec<HistoryMessage>,
		source: HistorySource,
		complete: bool,
	) {
		let current = self.current == Some(id);
		let reading = self.reads_now(id);
		// A private chat is named after the peer, whoever wrote first.
		let peer = match target {
			ChatTarget::Private(uid) => Some(format!(
				"@{}",
				self.peer_name(
					uid,
					messages.iter().find(|m| m.message.author_uid.as_deref() == Some(uid.as_str()))
				)
			)),
			_ => None,
		};
		let view = self.sessions.entry(id).or_default();
		let author = messages.first().map_or("", |m| m.message.author_name.as_str());
		let title = peer.unwrap_or_else(|| Self::tab_title(view, target, author));
		let index = Self::tab_for(view, target, || title);
		let seen = reading && view.current_tab == index;
		let own_client = view.state.own_client;
		let tab = &mut view.tabs[index];
		if source == HistorySource::Gateway {
			tab.synced = true;
		}
		if source != HistorySource::Live {
			tab.loading = false;
			tab.complete = tab.complete || complete;
		}
		// Without a marker, what the tab holds was read.
		if tab.read.is_none() {
			tab.read = tab.newest();
		}
		for message in messages {
			match tab.messages.iter().position(|m| m.message.id == message.id) {
				Some(i) => tab.messages[i].message = message,
				None => {
					let key = tab.take_key();
					tab.messages.push(Msg { key, message });
				}
			}
		}
		tab.messages.sort_by_key(|m| (m.message.message.ts_ms, m.message.id));
		// ...and so is a first stored page; a first live message is new.
		if tab.read.is_none() {
			tab.read = match source {
				HistorySource::Live => tab
					.messages
					.first()
					.map(|m| (m.message.message.ts_ms, m.message.id.saturating_sub(1))),
				_ => tab.newest(),
			};
		}
		let unread = tab.unread;
		let own =
			|m: &ChatMessage| crate::social::own_message(&self.social.own_uids, own_client, m);
		if seen && source == HistorySource::Live {
			tab.catch_up();
		} else {
			tab.count_unread(own);
			// What came while away (a stored or gateway page) gets the
			// divider also when its chat is on screen already.
			if seen {
				tab.enter(own);
			}
		}
		let counted = tab.unread != unread;
		if current {
			self.fetch_previews(id);
			self.refresh_chat();
		}
		if counted {
			self.refresh_servers();
		}
		self.studio_chat_changed(id, target);
	}

	/// The page of messages before the oldest one shown.
	pub(crate) fn load_older(&mut self) {
		let Some(id) = self.current else { return };
		let Some(view) = self.sessions.get_mut(&id) else { return };
		let tab = &mut view.tabs[view.current_tab];
		if tab.loading || tab.complete {
			return;
		}
		tab.loading = true;
		let target = tab.target.clone();
		let oldest = tab.messages.first().map(|m| &m.message);
		if self.demo_ui {
			// No engine answers in demo mode.
			crate::dev::answer_older(id, target, oldest.cloned());
		} else {
			let before = oldest.map(|m| m.id);
			self.engine.send(Command::LoadOlderHistory { session: id as u64, target, before });
		}
		self.refresh_chat();
	}

	/// The lines of a tab's messages (`main`) or of its open topic, grouped
	/// and with everything the row shows (also the stream chat of the
	/// Stream Studio, studio.rs). The server chat starts with what the
	/// server says to everyone who connects ([`Self::server_lines`]).
	pub(crate) fn lines_of(&self, view: &SessionView, tab: &Tab, main: bool) -> Vec<ChatLine> {
		let messages = if main { &tab.messages } else { &tab.topic_messages };
		let gateway = view.gateway_has(feature::PINS)
			|| view.gateway_has(feature::REACTIONS)
			|| view.gateway_has(feature::TOPICS);
		// Pictures on the web show only while pictures are fetched and
		// shown.
		let shown = self.prefs.get(&CACHE_FETCH_IMAGES) && self.prefs.get(&UI_IMAGE_PREVIEW_KB) > 0;
		let pictures = shown.then_some(&view.pictures);
		let cache = main.then_some(&tab.cache);
		let mut previous: Option<Previous> = None;
		let mut lines: Vec<(i64, ChatLine)> = Vec::with_capacity(messages.len());
		for m in messages {
			let avatar =
				m.message.message.author_uid.as_ref().and_then(|uid| view.avatars.get(uid));
			let topic = m
				.message
				.topic_id
				.filter(|_| tab.topic.is_none())
				.and_then(|t| tab.topics.iter().find(|i| i.id == t))
				.map(|t| t.title.clone())
				.unwrap_or_default();
			let previews = m
				.message
				.message
				.file_refs()
				.iter()
				.enumerate()
				.filter_map(|(i, f)| crate::previews::image_of(view, f).map(|image| (i, image)))
				.collect();
			let ctx = LineCtx {
				key: m.key,
				avatar: vm::avatar::image(avatar),
				gateway,
				marked: tab.marked == Some(m.message.id),
				topic,
				downloads: self.downloads_of(view, m.key),
				previews,
				pictures,
				cache,
				unread_start: main && tab.divider == Some(m.message.id),
			};
			lines.push((
				m.message.message.ts_ms,
				vm::chat::history_line(&m.message, previous.as_ref(), &ctx),
			));
			previous = Some(Previous::of(&m.message.message));
		}
		if let Some(cache) = cache {
			let ids = messages.iter().map(|m| m.message.id).chain(SERVER_LINES);
			cache.keep(ids, messages.len());
		}
		// Pokes of a private chat's peer show among its messages.
		if let (ChatTarget::Private(uid), Some(session)) = (&tab.target, self.current) {
			lines.extend(self.poke_lines(session, uid));
			lines.sort_by_key(|(ts, _)| *ts);
		}
		let server = if main && tab.target == ChatTarget::Server {
			Self::server_lines(view, pictures, cache)
		} else {
			Vec::new()
		};
		server.into_iter().chain(lines.into_iter().map(|(_, line)| line)).collect()
	}

	/// What the server says to everyone who connects ([`server_texts`]) as
	/// the first lines of the server chat: by the server, without reactions
	/// or pins.
	fn server_lines(
		view: &SessionView,
		pictures: Option<&HashMap<String, PathBuf>>,
		cache: Option<&LineCache>,
	) -> Vec<ChatLine> {
		let name = match view.presence.server_name.as_str() {
			"" => "Server",
			name => name,
		};
		server_texts(&view.presence.server)
			.into_iter()
			.enumerate()
			.map(|(i, (text, id))| {
				let ctx = LineCtx { key: -1 - i as i32, pictures, cache, ..Default::default() };
				vm::chat::server_line(name, text, id, i > 0, &ctx)
			})
			.collect()
	}

	/// The file cards of a message that has downloads running.
	fn downloads_of(&self, view: &SessionView, key: i32) -> Vec<(usize, FileItem)> {
		view.downloads
			.values()
			.filter(|d| d.key == key)
			.map(|d| {
				let (detail, status, progress) = match &d.state {
					TransferState::Requested => ("Starting…".to_owned(), "running", 0.0),
					TransferState::Started { size, offset } => (
						format!(
							"{} of {}",
							vm::chat::size_text(*offset),
							vm::chat::size_text(*size)
						),
						"running",
						if *size > 0 { *offset as f32 / *size as f32 } else { 0.0 },
					),
					TransferState::Progress { done, size } => (
						format!("{} of {}", vm::chat::size_text(*done), vm::chat::size_text(*size)),
						"running",
						if *size > 0 { *done as f32 / *size as f32 } else { 0.0 },
					),
					TransferState::Done { size, .. } => {
						(format!("Downloaded · {}", vm::chat::size_text(*size)), "done", 1.0)
					}
					TransferState::Failed(e) => (e.clone(), "failed", 0.0),
					TransferState::Cancelled => ("Cancelled".to_owned(), "failed", 0.0),
				};
				(
					d.index,
					FileItem {
						name: d.name.clone().into(),
						detail: detail.into(),
						index: d.index as i32,
						state: status.into(),
						progress,
						..Default::default()
					},
				)
			})
			.collect()
	}

	/// Tabs and the shown tab's messages.
	pub(crate) fn refresh_chat(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let Some(view) = self.view() else {
			vm::list::sync(
				&self.models.tabs,
				&[ChatTab { title: "Server".into(), ..Default::default() }],
			);
			let empty = ModelRc::from(self.models.no_chat.clone());
			if bridge.get_messages() != empty {
				bridge.set_messages(empty);
			}
			bridge.set_unread_index(-1);
			return;
		};
		let tabs: Vec<ChatTab> = view
			.tabs
			.iter()
			.map(|t| {
				let (last, time) = t
					.messages
					.last()
					.map(|m| vm::chat::preview(&m.message.message))
					.unwrap_or_default();
				(t, last, time)
			})
			.map(|(t, last, time)| {
				let channel = match t.target {
					ChatTarget::Channel(cid) => view.presence.channels.get(&cid),
					_ => None,
				};
				ChatTab {
					last: last.into(),
					time: time.into(),
					title: t.title.clone().into(),
					name: t.title.trim_start_matches(['#', '@']).into(),
					kind: match t.target {
						ChatTarget::Server => 0,
						ChatTarget::Channel(_) => 1,
						ChatTarget::Private(_) => 2,
					},
					id: match t.target {
						ChatTarget::Channel(cid) => cid as i32,
						_ => 0,
					},
					unread: t.unread,
					topic: channel.and_then(|c| c.topic.clone()).unwrap_or_default().into(),
					banner: vm::tree::banner(
						&view.pictures,
						channel.and_then(|c| c.banner_gfx_url.as_deref()),
					),
					banner_mode: channel.map_or(0, |c| vm::tree::banner_mode(c.banner_mode)),
				}
			})
			.collect();
		vm::list::sync(&self.models.tabs, &tabs);
		let current = view.current_tab as i32;
		if bridge.get_current_tab() != current {
			bridge.set_current_tab(current);
		}
		let tab = &view.tabs[view.current_tab];
		// Sessions without stored history push their lines themselves.
		let mut divider = None;
		if view.has_history() {
			let lines = self.lines_of(view, tab, true);
			divider = lines.iter().position(|l| l.unread_start);
			vm::list::sync(&tab.lines, &lines);
		}
		let lines = ModelRc::from(tab.lines.clone());
		if bridge.get_messages() != lines {
			bridge.set_messages(lines);
		}
		// The "New" divider: how many came from there on, and when.
		let new = tab
			.divider
			.and_then(|d| tab.messages.iter().position(|m| m.message.id == d))
			.map_or(&[][..], |i| &tab.messages[i..]);
		let session = self.current.unwrap_or_default();
		let count = new.iter().filter(|m| !self.is_own(session, &m.message.message)).count();
		bridge.set_unread_index(divider.map_or(-1, |i| i as i32));
		bridge.set_unread_count(count as i32);
		bridge.set_unread_since(
			new.first()
				.map(|m| vm::chat::since_time(m.message.message.ts_ms))
				.unwrap_or_default()
				.into(),
		);
		bridge.set_more_history(view.has_history() && !tab.complete);
		bridge.set_loading_history(tab.loading);
		bridge.set_local_history(
			view.has_history() && !tab.synced && view.gateway_has(feature::HISTORY),
		);
		bridge.set_has_pins(view.gateway_has(feature::PINS));
		bridge.set_has_reactions(view.gateway_has(feature::REACTIONS));
		bridge.set_has_topics(view.gateway_has(feature::TOPICS));
		bridge.set_pins_loading(!tab.pins_loaded && view.gateway_has(feature::PINS));
		bridge.set_topics_loading(!tab.topics_loaded && view.gateway_has(feature::TOPICS));
		let pins: Vec<PinItem> = tab
			.pins
			.iter()
			.map(|p| {
				let avatar =
					p.message.message.author_uid.as_ref().and_then(|u| view.avatars.get(u));
				let ctx = LineCtx {
					key: p.key,
					avatar: vm::avatar::image(avatar),
					gateway: true,
					..Default::default()
				};
				PinItem {
					line: vm::chat::history_line(&p.message, None, &ctx),
					by: format!("Pinned by {} · {}", p.by, vm::chat::time_of(p.ts_ms)).into(),
				}
			})
			.collect();
		vm::list::sync(&self.models.pins, &pins);
		let filter = self.topic_filter.trim().to_lowercase();
		let now = chrono::Utc::now().timestamp_millis();
		let topics: Vec<crate::app::TopicItem> = tab
			.topics
			.iter()
			.filter(|t| filter.is_empty() || t.title.to_lowercase().contains(&filter))
			.map(|t| crate::app::TopicItem {
				id: t.id as i32,
				title: t.title.clone().into(),
				creator: t.creator.name.clone().into(),
				detail: format!(
					"{} {} · by {}",
					t.message_count,
					if t.message_count == 1 { "message" } else { "messages" },
					t.creator.name
				)
				.into(),
				activity: format!("Last active {}", ago(t.last_activity_ms)).into(),
				recent: now - t.last_activity_ms < 3_600_000,
				archived: t.archived,
			})
			.collect();
		vm::list::sync(&self.models.topics, &topics);
		bridge.set_current_topic(tab.topic.map_or(-1, |t| t as i32));
		bridge.set_topic_title(
			tab.topic
				.and_then(|t| tab.topics.iter().find(|i| i.id == t))
				.map(|t| t.title.clone())
				.unwrap_or_default()
				.into(),
		);
		vm::list::sync(&self.models.topic_messages, &self.lines_of(view, tab, false));
	}

	/// A link in a message was clicked (`masked`: its text shows something
	/// else, [`vm::bbcode::is_masked`]). A page on the web opens in the
	/// browser, a masked one once the user saw where it goes (Nav.link-open);
	/// a TeamSpeak link is copied until Voelin opens them. Returns what to
	/// copy ("": nothing).
	pub(crate) fn open_link_text(&mut self, link: &str, masked: bool) -> String {
		match vm::bbcode::classify_link(link) {
			LinkKind::Web(url) if masked => {
				if let Some(ui) = self.ui.upgrade() {
					let nav = ui.global::<Nav>();
					nav.set_link_host(vm::bbcode::link_host(&url).into());
					nav.set_link_url(url.into());
					nav.set_link_open(true);
				}
				String::new()
			}
			LinkKind::Web(url) => {
				if let Err(e) = self.open_url(&url) {
					self.set_status(format!("Cannot open the link: {e}"));
				}
				String::new()
			}
			LinkKind::Server(url) => {
				self.copy_note = Some("TeamSpeak links open in Voelin soon; copied".to_owned());
				url
			}
			// A file opens from its card.
			LinkKind::File | LinkKind::Refused => String::new(),
		}
	}

	// What is read.

	/// The current chat's messages are on screen (`Nav.chat-shown`).
	fn chat_shown(&self) -> bool {
		self.ui.upgrade().is_some_and(|ui| ui.global::<Nav>().get_chat_shown())
	}

	/// New messages of session `id` in its current tab are read as they
	/// come: they are on screen in the focused window.
	fn reads_now(&self, id: i64) -> bool {
		self.focused && self.current == Some(id) && self.chat_shown()
	}

	/// After anything that can change which chat is on screen (a tab, a
	/// server, the page, the window's focus): the chat left is stored and
	/// loses its divider; the one on screen, in the focused window, is read
	/// (its divider above what was new). `true` if a tab changed.
	pub(crate) fn track_reading(&mut self) -> bool {
		let open = self.current.filter(|_| self.chat_shown()).and_then(|id| {
			let view = self.sessions.get(&id)?;
			Some((id, view.tabs.get(view.current_tab)?.target.clone()))
		});
		let mut changed = false;
		if open != self.reading {
			if let Some((id, target)) = self.reading.take()
				&& let Some(index) = self.tab_index(id, &target)
			{
				let tab = &mut self.sessions.entry(id).or_default().tabs[index];
				changed |= tab.divider.take().is_some();
				self.save_read(id, index);
			}
			self.reading = open.clone();
		}
		if let (true, Some((id, target))) = (self.focused, open)
			&& let Some(index) = self.tab_index(id, &target)
		{
			let own_uids = &self.social.own_uids;
			let view = self.sessions.entry(id).or_default();
			let own_client = view.state.own_client;
			let tab = &mut view.tabs[index];
			let before = (tab.unread, tab.divider);
			tab.enter(|m| crate::social::own_message(own_uids, own_client, m));
			changed |= (tab.unread, tab.divider) != before;
		}
		changed
	}

	/// Where the tab of `target` is in session `id`.
	fn tab_index(&self, id: i64, target: &ChatTarget) -> Option<usize> {
		self.sessions.get(&id)?.tabs.iter().position(|t| t.target == *target)
	}

	/// The current chat came on screen or left it (`Nav.chat-shown`).
	pub(crate) fn chat_shown_changed(&mut self) {
		if self.track_reading() {
			self.refresh_chat();
			self.refresh_servers();
		}
	}

	/// The window gained or lost the focus (winit's events: the desktop
	/// only).
	#[cfg(not(target_os = "android"))]
	pub(crate) fn window_focused(&mut self, focused: bool) {
		self.focused = focused;
		if self.track_reading() {
			self.refresh_chat();
			self.refresh_servers();
		}
	}

	/// The current chat is read (the bar over its messages): the divider
	/// goes.
	pub(crate) fn mark_read(&mut self) {
		let Some(id) = self.current else { return };
		let Some(view) = self.sessions.get_mut(&id) else { return };
		let index = view.current_tab;
		let tab = &mut view.tabs[index];
		tab.divider = None;
		tab.catch_up();
		self.save_read(id, index);
		self.refresh_chat();
		self.refresh_servers();
	}

	/// Store where tab `index` of session `id` was read, if that moved (not
	/// for the sample data of VOELIN_DEMO_UI).
	pub(crate) fn save_read(&mut self, id: i64, index: usize) {
		if self.demo_ui {
			return;
		}
		let Some(view) = self.sessions.get_mut(&id) else { return };
		let Some(server_uid) = view.state.server_uid.as_deref() else { return };
		let Some(tab) = view.tabs.get_mut(index) else { return };
		let Some((ts_ms, message)) = tab.read.filter(|r| tab.stored_read != Some(*r)) else {
			return;
		};
		let read =
			ChatRead { ts_ms, id: message, updated_ms: chrono::Utc::now().timestamp_millis() };
		let target = store_target(&tab.target);
		match self.store.set_chat_read(server_uid, &target, &read) {
			Ok(()) => {
				tab.stored_read = tab.read;
				// A tab closed and opened again starts from here.
				view.reads.insert(target.key(), (ts_ms, message));
			}
			Err(e) => tracing::warn!(%e, "could not keep where a chat was read"),
		}
	}

	/// Store where every chat of session `id` was read (it ended, or the
	/// app exits).
	pub(crate) fn save_reads(&mut self, id: i64) {
		let count = self.sessions.get(&id).map_or(0, |v| v.tabs.len());
		for index in 0..count {
			self.save_read(id, index);
		}
	}

	/// Where the chats of session `id` were read, from the store, once its
	/// server is known (or another one: the gateway's id wins): its tabs
	/// start from there, now and when they open.
	pub(crate) fn load_reads(&mut self, id: i64) {
		let Some(view) = self.sessions.get_mut(&id) else { return };
		if self.demo_ui || view.state.server_uid.is_none() || view.reads_of == view.state.server_uid
		{
			return;
		}
		view.reads_of = view.state.server_uid.clone();
		let server_uid = view.reads_of.as_deref().unwrap_or_default();
		view.reads = match self.store.chat_reads(server_uid) {
			Ok(reads) => reads.into_iter().map(|(t, r)| (t.key(), (r.ts_ms, r.id))).collect(),
			Err(e) => {
				tracing::warn!(%e, "could not read where the chats were read");
				HashMap::new()
			}
		};
		for tab in &mut view.tabs {
			tab.read_from(&view.reads);
		}
	}

	// Gateway features of a chat.

	/// A request to the current session's gateway.
	fn gateway(&self, request: GatewayRequest) {
		self.command(|session| Command::Gateway { session, request });
	}

	pub(crate) fn gateway_update(&mut self, id: i64, update: GatewayUpdate) {
		let current = self.current == Some(id);
		let view = self.sessions.entry(id).or_default();
		match update {
			GatewayUpdate::Connected { capabilities, .. }
			| GatewayUpdate::Capabilities { capabilities } => view.gateway_caps = capabilities,
			GatewayUpdate::Disconnected { .. } => {
				view.gateway_caps.clear();
				for tab in &mut view.tabs {
					tab.pins_loaded = false;
					tab.topics_loaded = false;
				}
			}
			GatewayUpdate::Pins { target, pins } => {
				let index = Self::tab_for(view, &target, || "Server".into());
				let tab = &mut view.tabs[index];
				tab.pins_loaded = true;
				tab.pins = pins.into_iter().map(|p| pin_row(tab.take_key(), p)).collect();
				tab.pins.sort_by_key(|p| -p.ts_ms);
			}
			GatewayUpdate::Pinned { target, pin } => {
				let index = Self::tab_for(view, &target, || "Server".into());
				let tab = &mut view.tabs[index];
				let row = pin_row(tab.take_key(), pin);
				tab.pins.retain(|p| p.message.id != row.message.id);
				tab.pins.insert(0, row);
			}
			GatewayUpdate::Unpinned { target, message_id, .. } => {
				let index = Self::tab_for(view, &target, || "Server".into());
				view.tabs[index].pins.retain(|p| p.message.remote_id != Some(message_id));
			}
			GatewayUpdate::Topics { target, topics } => {
				let index = Self::tab_for(view, &target, || "Server".into());
				view.tabs[index].topics = topics;
				view.tabs[index].topics_loaded = true;
			}
			GatewayUpdate::Topic { topic } => {
				let index = Self::tab_for(view, &topic.target.clone(), || "Server".into());
				upsert_topic(&mut view.tabs[index].topics, topic);
			}
			GatewayUpdate::TopicHistory { target, topic, messages, .. } => {
				let index = Self::tab_for(view, &target, || "Server".into());
				let tab = &mut view.tabs[index];
				if tab.topic != Some(topic) {
					return;
				}
				for message in messages {
					match tab.topic_messages.iter().position(|m| m.message.id == message.id) {
						Some(i) => tab.topic_messages[i].message = message,
						None => {
							let key = tab.take_key();
							tab.topic_messages.push(Msg { key, message });
						}
					}
				}
				tab.topic_messages.sort_by_key(|m| (m.message.message.ts_ms, m.message.id));
			}
			GatewayUpdate::Failed { request, code, message } => {
				tracing::warn!(%request, ?code, %message, "gateway request failed");
				if let Some(text) = failure_text(&request, code, &message) {
					self.set_status(text);
				}
				return;
			}
			// The stream directory: how many watch each stream.
			GatewayUpdate::Streams { streams } => {
				view.stream_viewers = streams.iter().filter_map(viewers_of).collect();
				return self.stream_viewers_changed(current);
			}
			GatewayUpdate::StreamStarted { stream }
			| GatewayUpdate::StreamUpdated { stream }
			| GatewayUpdate::StreamRegistered { stream } => {
				if let Some((id, viewers)) = viewers_of(&stream) {
					view.stream_viewers.insert(id, viewers);
				}
				return self.stream_viewers_changed(current);
			}
			GatewayUpdate::StreamEnded { id, .. } => {
				view.stream_viewers.remove(&id);
				return self.stream_viewers_changed(current);
			}
			// Posts, pins and reactions also arrive as stored messages.
			_ => return,
		}
		if current {
			self.refresh_chat();
		}
	}

	fn stream_viewers_changed(&self, current: bool) {
		if current {
			self.refresh_streams();
			self.refresh_viewer();
		}
	}

	/// The message a Slint row stands for, in the current chat.
	fn message_of(&self, key: i32) -> Option<&HistoryMessage> {
		let view = self.view()?;
		view.tabs.get(view.current_tab)?.message(key)
	}

	/// Add or remove our reaction to a message.
	pub(crate) fn react(&mut self, key: i32, emoji: String) {
		let Some(message) = self.message_of(key) else { return };
		let Some(message_id) = message.remote_id else { return };
		let had = message.reactions.iter().any(|r| r.emoji == emoji && r.me);
		self.gateway(if had {
			GatewayRequest::Unreact { message_id, emoji }
		} else {
			GatewayRequest::React { message_id, emoji }
		});
	}

	pub(crate) fn pin(&mut self, key: i32, on: bool) {
		let Some(message) = self.message_of(key) else { return };
		let Some(message_id) = message.remote_id else { return };
		self.gateway(if on {
			GatewayRequest::Pin { message_id }
		} else {
			GatewayRequest::Unpin { message_id }
		});
	}

	/// Open the pins of the current chat (loading them once).
	pub(crate) fn open_pins(&mut self) {
		let Some(view) = self.view() else { return };
		let tab = &view.tabs[view.current_tab];
		if !tab.pins_loaded {
			let target = tab.target.clone();
			self.gateway(GatewayRequest::Pins { target });
		}
		self.refresh_chat();
	}

	/// Highlight a pinned message in the chat.
	pub(crate) fn jump_to_pin(&mut self, key: i32) {
		let Some(view) = self.view_mut() else { return };
		let tab = &mut view.tabs[view.current_tab];
		tab.marked = tab.pins.iter().find(|p| p.key == key).map(|p| p.message.id);
		// The message is in the chat, not in an open topic.
		if tab.topic.take().is_some() {
			tab.topic_messages.clear();
		}
		// Older than what is loaded: only highlighted once it is. The server
		// chat's first lines are the server's.
		let row = tab.marked.and_then(|id| tab.messages.iter().position(|m| m.message.id == id));
		let first = match tab.target {
			ChatTarget::Server => server_texts(&view.presence.server).len(),
			_ => 0,
		};
		let row = row.map(|row| row + first);
		self.refresh_chat();
		if let (Some(row), Some(ui)) = (row, self.ui.upgrade()) {
			let bridge = ui.global::<Bridge>();
			bridge.set_jump_index(row as i32);
			bridge.set_jump_requests(bridge.get_jump_requests().wrapping_add(1));
		}
	}

	/// The topics drawer's search.
	pub(crate) fn search_topics(&mut self, text: String) {
		if self.topic_filter != text {
			self.topic_filter = text;
			self.refresh_chat();
		}
	}

	pub(crate) fn open_topics(&mut self) {
		let Some(view) = self.view() else { return };
		let tab = &view.tabs[view.current_tab];
		if !tab.topics_loaded {
			let target = tab.target.clone();
			self.gateway(GatewayRequest::Topics { target, include_archived: false });
		}
		self.refresh_chat();
	}

	/// Show a topic's messages (`-1` goes back to the chat).
	pub(crate) fn open_topic(&mut self, id: i32) {
		let Some(view) = self.view_mut() else { return };
		let tab = &mut view.tabs[view.current_tab];
		tab.topic_messages.clear();
		tab.topic = (id >= 0).then_some(id as i64);
		let target = tab.target.clone();
		if let Some(topic) = tab.topic {
			self.gateway(GatewayRequest::TopicHistory { target, topic, before: None, limit: None });
		}
		self.refresh_chat();
	}

	pub(crate) fn send_topic_message(&mut self, text: String) {
		let Some(view) = self.view() else { return };
		let tab = &view.tabs[view.current_tab];
		let (Some(topic), target) = (tab.topic, tab.target.clone()) else { return };
		self.gateway(GatewayRequest::Post { target, text, topic: Some(topic) });
	}

	/// Start a topic from a message (`key` < 0: a standalone topic).
	pub(crate) fn create_topic(&mut self, key: i32, title: String) {
		let Some(view) = self.view() else { return };
		let target = view.tabs[view.current_tab].target.clone();
		let message_id = self.message_of(key).and_then(|m| m.remote_id);
		self.gateway(GatewayRequest::CreateTopic { target, title, message_id });
	}

	// Files in chat.

	/// Download a file a message links, into the download folder.
	pub(crate) fn download_file(&mut self, key: i32, index: i32) {
		let Some(id) = self.current else { return };
		let index = index.max(0) as usize;
		let Some(file) =
			self.message_of(key).and_then(|m| m.message.file_refs().into_iter().nth(index))
		else {
			return;
		};
		let dir = dirs::download_dir().unwrap_or_else(|| PathBuf::from("."));
		let path = dir.join(&file.name);
		let view = self.sessions.entry(id).or_default();
		let transfer = view.next_transfer;
		view.next_transfer += 1;
		view.downloads.insert(
			transfer,
			Download { key, index, name: file.name.clone(), state: TransferState::Requested },
		);
		self.engine.send(Command::DownloadChatFile {
			session: id as u64,
			transfer,
			file,
			password: None,
			to: DownloadTo::Path { path, resume: false },
		});
		self.refresh_chat();
	}

	pub(crate) fn transfer_progress(&mut self, id: i64, transfer: u64, state: TransferState) {
		// An upload from the composer: post the file's link once it is up.
		if let Some((_, channel, name)) = self.uploads.get(&transfer).cloned() {
			match &state {
				TransferState::Done { size, .. } => {
					self.uploads.remove(&transfer);
					let file = voelin_model::FileRef {
						channel,
						path: "/".into(),
						name: name.clone(),
						size: Some(*size),
						..Default::default()
					};
					self.engine.send(Command::SendChat {
						session: id as u64,
						target: ChatTarget::Channel(channel),
						text: file.to_bbcode(),
					});
					self.set_status(format!("{name} shared"));
				}
				TransferState::Failed(e) => {
					self.uploads.remove(&transfer);
					self.set_status(format!("Could not upload {name}: {e}"));
				}
				TransferState::Cancelled => {
					self.uploads.remove(&transfer);
				}
				_ => {}
			}
			return;
		}
		if self.preview_progress(id, transfer, &state) {
			return;
		}
		let done = matches!(state, TransferState::Done { .. });
		let view = self.sessions.entry(id).or_default();
		let Some(entry) = view.downloads.get_mut(&transfer) else { return };
		entry.state = state;
		let name = entry.name.clone();
		if done {
			self.set_status(format!("{name} downloaded"));
		}
		if self.current == Some(id) {
			self.refresh_chat();
		}
	}

	/// Attach a file to the current channel chat: ask for one, upload it
	/// into the channel and post its link.
	pub(crate) fn attach_file(&mut self) {
		let Some(id) = self.current else { return };
		let Some(view) = self.sessions.get(&id) else { return };
		let ChatTarget::Channel(channel) = view.tabs[view.current_tab].target else {
			self.set_status("Files can be attached to a channel's chat.");
			return;
		};
		if !voelin_platform::files::available() {
			self.set_status("No file chooser on this platform.");
			return;
		}
		let runtime = self.engine.runtime().clone();
		runtime.spawn(async move {
			let picked = voelin_platform::files::pick_file("Attach a file").await;
			crate::app::later(move |app| match picked {
				Ok(Some(path)) => app.upload(id, channel, path),
				Ok(None) => {}
				Err(e) => app.set_status(format!("Could not open the file chooser: {e}")),
			});
		});
	}

	/// Upload a file into a channel and link it in the chat once it is up.
	pub(crate) fn upload(&mut self, id: i64, channel: u64, from: PathBuf) {
		let Some(name) = from.file_name().map(|n| n.to_string_lossy().to_string()) else { return };
		let view = self.sessions.entry(id).or_default();
		let transfer = view.next_transfer;
		view.next_transfer += 1;
		self.uploads.insert(transfer, (id, channel, name.clone()));
		self.engine.send(Command::UploadFile {
			session: id as u64,
			transfer,
			channel,
			password: None,
			path: format!("/{name}"),
			from,
			overwrite: true,
			resume: false,
		});
		self.set_status(format!("Uploading {name}…"));
	}
}

/// A directory entry's stream id and viewer count, when it has both.
/// What a failed gateway request tells the user. What the app asked for by
/// itself (history, lists, subscriptions) fails quietly; what the user did
/// says so in plain words, never the gateway's; the gateway's own
/// administration page shows its message as it is.
fn failure_text(request: &str, code: Option<ErrorCode>, message: &str) -> Option<String> {
	if request.starts_with("config_") || request.starts_with("perm_") {
		return Some(format!("{request}: {message}"));
	}
	let done_by_user = matches!(
		request,
		"post"
			| "pin" | "unpin"
			| "react" | "unreact"
			| "create_topic"
			| "update_topic"
			| "create_event"
			| "update_event"
			| "delete_event"
			| "rsvp"
	);
	if !done_by_user {
		return None;
	}
	let text = match code {
		Some(ErrorCode::Forbidden) => "Not allowed on this server.",
		Some(ErrorCode::RateLimited) => "Too many requests; try again in a moment.",
		Some(ErrorCode::QuotaExceeded) => "This server's limit is reached.",
		Some(ErrorCode::NotFound) => "That is no longer there.",
		Some(ErrorCode::FeatureDisabled) => "This server does not offer that.",
		_ => "That did not work; try again later.",
	};
	Some(text.to_owned())
}

fn viewers_of(entry: &voelin_gateway_proto::StreamEntry) -> Option<(String, u32)> {
	Some((entry.stream_id.clone()?, entry.viewers?))
}

fn pin_row(key: i32, pin: Pin) -> PinRow {
	PinRow { key, message: pin.message, by: pin.by.name, ts_ms: pin.ts_ms }
}

fn upsert_topic(topics: &mut Vec<TopicInfo>, topic: TopicInfo) {
	match topics.iter().position(|t| t.id == topic.id) {
		Some(i) => topics[i] = topic,
		None => topics.insert(0, topic),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn relative_times() {
		let now = chrono::Utc::now().timestamp_millis();
		assert_eq!(ago(now), "just now");
		assert_eq!(ago(now - 3 * 60_000), "3 min ago");
		assert_eq!(ago(now - 3 * 3_600_000), "3 hours ago");
		assert_eq!(ago(now - 36 * 3_600_000), "yesterday");
	}

	#[test]
	fn failures_say_what_the_user_did_never_the_gateway() {
		let lost = "websocket: IO error: Connection refused (os error 111)";
		// The app's own requests fail quietly.
		for request in ["history", "sync", "pins", "events", "subscribe_streams"] {
			assert_eq!(failure_text(request, None, lost), None, "{request}");
		}
		// The user's own, in plain words.
		assert_eq!(
			failure_text("pin", Some(ErrorCode::Forbidden), "no b_pin").as_deref(),
			Some("Not allowed on this server.")
		);
		let text = failure_text("post", None, lost).unwrap();
		assert!(!text.contains("websocket") && !text.to_lowercase().contains("gateway"), "{text}");
		// The gateway's administration page shows its message.
		assert_eq!(
			failure_text("config_set", Some(ErrorCode::BadRequest), "bad value").as_deref(),
			Some("config_set: bad value")
		);
	}
}
