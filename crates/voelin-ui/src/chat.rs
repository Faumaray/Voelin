//! Chat: tabs per session (server, channels, private chats), their
//! messages, and what a gateway adds to them (pins, reactions, topics).
//!
//! The engine keeps the history: [`Event::ChatHistory`] is an upsert of
//! messages by their local id (see `voelin_core::history`), so a tab holds
//! the messages it was given, ordered by `(ts_ms, id)`, and the lines of
//! the selected tab are rebuilt from them with [`vm::list::sync`], which
//! touches only the rows that changed. Sessions the engine keeps no history
//! for (it does not know the server yet) fall back to [`Event::Chat`].

use std::path::PathBuf;

use slint::{ComponentHandle, ModelRc};
use voelin_core::gateway::Pin;
use voelin_core::{
	Command, DownloadTo, GatewayRequest, GatewayUpdate, HistoryMessage, HistorySource,
	TransferState,
};
use voelin_gateway_proto::{TopicInfo, feature};
use voelin_model::{ChatMessage, ChatTarget};

use crate::app::{App, Bridge, ChatLine, ChatTab, FileItem, Msg, PinItem, SessionView, Tab};
use crate::vm;
use crate::vm::chat::{LineCtx, Previous};

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
				view.tabs.push(Tab::new(target.clone(), title));
				if !self.demo_ui {
					self.engine.send(Command::OpenChat { session: id as u64, target });
				}
				view.tabs.len() - 1
			}
		};
		if focus {
			view.current_tab = index;
			view.tabs[index].unread = 0;
		}
		self.refresh_chat();
		self.refresh_servers();
	}

	pub(crate) fn select_tab(&mut self, index: usize) {
		if let Some(view) = self.view_mut()
			&& index < view.tabs.len()
		{
			view.current_tab = index;
			view.tabs[index].unread = 0;
			view.tabs[index].topic = None;
		}
		self.refresh_chat();
		self.refresh_servers();
	}

	pub(crate) fn close_tab(&mut self, index: usize) {
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
				view.tabs.push(Tab::new(target.clone(), title()));
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
					.map(|c| c.name.clone())
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
		let view = self.sessions.entry(id).or_default();
		let title = Self::tab_title(view, &message.target, &message.author_name);
		let index = Self::tab_for(view, &message.target, || title);
		let shown = current && view.current_tab == index;
		let tab = &mut view.tabs[index];
		let line = vm::chat::line(&message, tab.last.as_ref());
		tab.last = Some(Previous::of(&message));
		tab.lines.push(line);
		if !shown {
			tab.unread += 1;
		}
		if current {
			self.refresh_chat();
		}
		if !shown {
			self.refresh_servers();
		}
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
		let view = self.sessions.entry(id).or_default();
		let author = messages.first().map_or("", |m| m.message.author_name.as_str());
		let title = Self::tab_title(view, target, author);
		let index = Self::tab_for(view, target, || title);
		let shown = current && view.current_tab == index;
		let tab = &mut view.tabs[index];
		if source == HistorySource::Gateway {
			tab.synced = true;
		}
		if source != HistorySource::Live {
			tab.loading = false;
			tab.complete = tab.complete || complete;
		}
		let mut fresh = 0;
		for message in messages {
			match tab.messages.iter().position(|m| m.message.id == message.id) {
				Some(i) => tab.messages[i].message = message,
				None => {
					let key = tab.take_key();
					if source == HistorySource::Live {
						fresh += 1;
					}
					tab.messages.push(Msg { key, message });
				}
			}
		}
		tab.messages.sort_by_key(|m| (m.message.message.ts_ms, m.message.id));
		if !shown {
			tab.unread += fresh;
		}
		if current {
			self.refresh_chat();
		}
		if fresh > 0 && !shown {
			self.refresh_servers();
		}
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
		let before = tab.messages.first().map(|m| m.message.id);
		self.engine.send(Command::LoadOlderHistory { session: id as u64, target, before });
		self.refresh_chat();
	}

	/// The lines of a tab, grouped and with everything the row shows.
	fn lines_of(&self, view: &SessionView, tab: &Tab, messages: &[Msg]) -> Vec<ChatLine> {
		let gateway = view.gateway_has(feature::PINS)
			|| view.gateway_has(feature::REACTIONS)
			|| view.gateway_has(feature::TOPICS);
		let mut previous: Option<Previous> = None;
		let mut lines = Vec::with_capacity(messages.len());
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
			let ctx = LineCtx {
				key: m.key,
				avatar: vm::avatar::image(avatar),
				gateway,
				marked: tab.marked == Some(m.message.id),
				topic,
				downloads: self.downloads_of(view, m.key),
			};
			lines.push(vm::chat::history_line(&m.message, previous.as_ref(), &ctx));
			previous = Some(Previous::of(&m.message.message));
		}
		lines
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
			return;
		};
		let tabs: Vec<ChatTab> = view
			.tabs
			.iter()
			.map(|t| ChatTab {
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
				topic: match t.target {
					ChatTarget::Channel(cid) => view
						.presence
						.channels
						.get(&cid)
						.and_then(|c| c.topic.clone())
						.unwrap_or_default()
						.into(),
					_ => Default::default(),
				},
			})
			.collect();
		vm::list::sync(&self.models.tabs, &tabs);
		let current = view.current_tab as i32;
		if bridge.get_current_tab() != current {
			bridge.set_current_tab(current);
		}
		let tab = &view.tabs[view.current_tab];
		// Sessions without stored history push their lines themselves.
		if view.has_history() {
			vm::list::sync(&tab.lines, &self.lines_of(view, tab, &tab.messages));
		}
		let lines = ModelRc::from(tab.lines.clone());
		if bridge.get_messages() != lines {
			bridge.set_messages(lines);
		}
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
		vm::list::sync(&self.models.topic_messages, &self.lines_of(view, tab, &tab.topic_messages));
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
			GatewayUpdate::Failed { request, message, .. } => {
				self.set_status(format!("{request}: {message}"));
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
		self.refresh_chat();
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
	fn upload(&mut self, id: i64, channel: u64, from: PathBuf) {
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
}
