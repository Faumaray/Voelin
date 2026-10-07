//! Direct messages (design mockup 02) and the recent chats of the home
//! page and its sidebar: every chat the sessions hold and the newest ones
//! in the history store, across servers; the open private chat with who it
//! is with; TS3 and TS6 offline messages (mail the server keeps).
//!
//! A private chat is a chat tab of its session (`ChatTarget::Private`
//! stored under the peer's unique id), so opening one selects its server
//! and its tab: the chat view's machinery (history pages, the composer)
//! serves it. The chat strip of the server page leaves these tabs out, so
//! leaving the messages page makes the strip's chat current again
//! ([`App::close_messages`]), and coming back the open one
//! ([`App::messages_shown`]). A chat only in the store (its server is not
//! connected, or no longer a bookmark) is read from there and shown
//! read-only.

use std::collections::HashMap;

use slint::{ComponentHandle, Image, SharedString};
use voelin_core::{Command, HistoryMessage, OfflineMessage, OfflineMessageInfo, VoiceState};
use voelin_model::ChatTarget;
use voelin_store::PageQuery;

use crate::app::{
	App, Bridge, ConversationItem, DmPeer, FileItem, Nav, OfflineForm, Page, SessionView, later,
};
use crate::vm;
use crate::vm::social::{ago, list_time, matches, plain, preview};

/// Keys of offline messages in the conversation list start here.
const MAIL: i32 = 10_000;

/// A chat of any server.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChatRef {
	/// The bookmark (session) it belongs to, when known.
	pub session: Option<i64>,
	pub server_uid: Option<String>,
	pub target: ChatTarget,
	pub last: Option<HistoryMessage>,
	pub unread: i32,
}

/// The open conversation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Open {
	pub session: Option<i64>,
	pub server_uid: Option<String>,
	pub uid: String,
	/// An offline message (session, id) shown instead of the chat.
	pub mail: Option<(i64, u32)>,
	/// Read from the store: no session holds the chat.
	pub stored: bool,
}

#[derive(Default)]
pub(crate) struct Dms {
	/// The newest stored message of every chat, with its server's unique id.
	pub stored: Vec<(String, HistoryMessage)>,
	/// Bookmarks by the server unique id remembered for their address.
	pub bookmark_of: HashMap<String, i64>,
	/// Every chat, newest first (the keys of the lists).
	pub chats: Vec<ChatRef>,
	/// The Inbox tab's offline messages: session and message.
	pub inbox: Vec<(i64, OfflineMessageInfo)>,
	/// 0 all, 1 unread, 2 inbox.
	pub tab: i32,
	pub filter: String,
	pub open: Option<Open>,
	/// The text of the open offline message.
	pub mail: Option<OfflineMessage>,
	/// Offline message requests in flight: session and what for.
	pub requests: HashMap<u64, (i64, &'static str)>,
	pub next_request: u64,
}

impl App {
	/// Read the newest chats of the history store (start, and when the
	/// lists are shown).
	pub(crate) fn load_recent_chats(&mut self) {
		if self.demo_ui {
			return;
		}
		self.social.dm.bookmark_of.clear();
		for b in &self.bookmarks {
			for alias in [
				Some(format!("voice:{}", b.address)),
				b.gateway_url.as_ref().map(|u| format!("gateway:{u}")),
			]
			.into_iter()
			.flatten()
			{
				if let Ok(Some(uid)) = self.store.server_alias(&alias) {
					self.social.dm.bookmark_of.insert(uid, b.id);
				}
			}
		}
		let history = self.engine.history();
		self.engine.runtime().spawn(async move {
			let result = history.recent_chats(60).await;
			later(move |app| match result {
				Ok(list) => {
					app.social.dm.stored = list;
					app.refresh_chats();
					app.refresh_home();
				}
				Err(e) => tracing::warn!("cannot read recent chats: {e}"),
			});
		});
	}

	/// The session a server unique id belongs to.
	fn session_of_server(&self, server_uid: &str) -> Option<i64> {
		self.sessions
			.iter()
			.find(|(_, v)| v.state.server_uid.as_deref() == Some(server_uid))
			.map(|(id, _)| *id)
			.or_else(|| self.social.dm.bookmark_of.get(server_uid).copied())
	}

	/// What we call the peer of a private chat.
	pub(crate) fn peer_name(&self, uid: &str, last: Option<&HistoryMessage>) -> String {
		if let Some(c) = self.contacts.get(uid).filter(|c| !c.nickname.is_empty()) {
			return c.nickname.clone();
		}
		if let Some(s) = self.spots_of(uid).first() {
			return self.sessions[&s.session].nickname(s.client);
		}
		let from_peer = |m: &HistoryMessage| {
			(m.message.author_uid.as_deref() == Some(uid)).then(|| m.message.author_name.clone())
		};
		last.and_then(from_peer)
			.or_else(|| {
				self.sessions.values().flat_map(|v| &v.tabs).find_map(|t| {
					(t.target == ChatTarget::Private(uid.to_owned()))
						.then(|| t.messages.iter().rev().find_map(|m| from_peer(&m.message)))
						.flatten()
				})
			})
			.unwrap_or_else(|| uid.chars().take(8).collect())
	}

	/// Every chat, newest first; then the lists that show them.
	pub(crate) fn refresh_chats(&mut self) {
		let mut chats: Vec<ChatRef> = Vec::new();
		for b in &self.bookmarks {
			let Some(view) = self.sessions.get(&b.id) else { continue };
			for tab in &view.tabs {
				let last = tab.messages.last().map(|m| m.message.clone());
				if last.is_none() && !matches!(tab.target, ChatTarget::Private(_)) {
					continue;
				}
				chats.push(ChatRef {
					session: Some(b.id),
					server_uid: view.state.server_uid.clone(),
					target: tab.target.clone(),
					last,
					unread: tab.unread,
				});
			}
		}
		for (server_uid, m) in &self.social.dm.stored {
			let known = chats.iter().any(|c| {
				c.target == m.message.target && c.server_uid.as_deref() == Some(server_uid.as_str())
			});
			if !known {
				chats.push(ChatRef {
					session: self.session_of_server(server_uid),
					server_uid: Some(server_uid.clone()),
					target: m.message.target.clone(),
					last: Some(m.clone()),
					unread: 0,
				});
			}
		}
		chats.sort_by_key(|c| std::cmp::Reverse(c.last.as_ref().map_or(0, |m| m.message.ts_ms)));
		self.social.dm.chats = chats;
		self.social.dm.inbox = self
			.bookmarks
			.iter()
			.filter_map(|b| self.sessions.get(&b.id).map(|v| (b.id, v)))
			.flat_map(|(id, v)| v.extra.inbox.iter().map(move |m| (id, m.clone())))
			.collect();
		self.social.dm.inbox.sort_by_key(|(_, m)| std::cmp::Reverse(m.ts_s));
		self.refresh_conversations();
		self.refresh_dm();
	}

	/// One chat for a list.
	fn conversation_item(&self, key: usize, chat: &ChatRef) -> ConversationItem {
		let session = chat.session;
		let server =
			session.map(|s| self.server_name(s)).unwrap_or_else(|| "Another server".into());
		let (title, avatar, online, kind) = match &chat.target {
			ChatTarget::Private(uid) => (
				self.peer_name(uid, chat.last.as_ref()),
				self.avatar_of(uid),
				!self.spots_of(uid).is_empty(),
				2,
			),
			ChatTarget::Channel(cid) => {
				let name = session
					.and_then(|s| self.sessions.get(&s))
					.and_then(|v| v.presence.channels.get(cid))
					.map_or_else(
						|| format!("Channel {cid}"),
						|c| vm::tree::channel_title(c).0.to_owned(),
					);
				(name, slint::Image::default(), false, 1)
			}
			ChatTarget::Server => (server.clone(), slint::Image::default(), false, 0),
		};
		let (text, ts) = match &chat.last {
			Some(m) => {
				let mine = session.is_some_and(|s| self.is_own(s, &m.message));
				let files = m.message.file_refs();
				let mut text = preview(&m.message.text, mine, &files);
				if kind != 2 && !mine {
					text = format!("{}: {text}", m.message.author_name);
				}
				(text, m.message.ts_ms)
			}
			None => (String::new(), 0),
		};
		let open = self.social.dm.open.as_ref();
		ConversationItem {
			key: key as i32,
			kind,
			title: title.clone().into(),
			server: server.into(),
			preview: text.into(),
			time: list_time(ts).into(),
			ago: ago(ts).into(),
			unread: chat.unread,
			initials: vm::avatar::initials(&title).into(),
			// A server's chat in the server's colour.
			tint: match (kind, session) {
				(0, Some(s)) => self.server_tint(s),
				_ => vm::avatar::tint(&title),
			},
			avatar,
			online,
			selected: open.is_some_and(|o| {
				o.mail.is_none()
					&& ChatTarget::Private(o.uid.clone()) == chat.target
					&& (o.session == chat.session || o.server_uid == chat.server_uid)
			}),
		}
	}

	/// The DM list, the recent chats of home, the private chats of its
	/// sidebar and the unread counts.
	fn refresh_conversations(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let dm = &self.social.dm;
		let recent: Vec<ConversationItem> = dm
			.chats
			.iter()
			.enumerate()
			.filter(|(_, c)| c.last.is_some())
			.take(4)
			.map(|(i, c)| self.conversation_item(i, c))
			.collect();
		vm::list::sync(&self.models.social.recent, &recent);
		// Shown beside other pages: none is the open one.
		let sidebar: Vec<ConversationItem> = vm::social::sidebar_dms(&dm.chats, 5)
			.into_iter()
			.map(|i| ConversationItem {
				selected: false,
				..self.conversation_item(i, &dm.chats[i])
			})
			.collect();
		vm::list::sync(&self.models.social.sidebar_dms, &sidebar);
		let private = |c: &&ChatRef| matches!(c.target, ChatTarget::Private(_));
		let items: Vec<ConversationItem> = if dm.tab == 2 {
			dm.inbox
				.iter()
				.enumerate()
				.map(|(i, (session, m))| {
					let name = self.peer_name(&m.from_uid, None);
					ConversationItem {
						key: MAIL + i as i32,
						kind: 3,
						title: name.clone().into(),
						server: self.server_name(*session).into(),
						preview: m.subject.clone().into(),
						time: list_time(m.ts_s * 1000).into(),
						ago: ago(m.ts_s * 1000).into(),
						unread: i32::from(!m.read),
						// A letter in the sender's colour, no initials or
						// picture under it.
						initials: SharedString::new(),
						tint: vm::avatar::tint(&name),
						avatar: Image::default(),
						online: false,
						selected: dm
							.open
							.as_ref()
							.is_some_and(|o| o.mail == Some((*session, m.id))),
					}
				})
				.filter(|c| matches(&dm.filter, &[&c.title, &c.preview]))
				.collect()
		} else {
			dm.chats
				.iter()
				.enumerate()
				.filter(|(_, c)| private(c) && (dm.tab == 0 || c.unread > 0))
				.map(|(i, c)| self.conversation_item(i, c))
				.filter(|c| matches(&dm.filter, &[&c.title, &c.preview]))
				.collect()
		};
		vm::list::sync(&self.models.social.conversations, &items);
		bridge.set_dm_unread(self.dm_unread());
		bridge.set_inbox_unread(dm.inbox.iter().filter(|(_, m)| !m.read).count() as i32);
	}

	/// Unread messages in the private chats of every server (the rail and
	/// the chat strip leave them out).
	pub(crate) fn dm_unread(&self) -> i32 {
		self.bookmarks
			.iter()
			.filter_map(|b| self.sessions.get(&b.id))
			.map(SessionView::private_unread)
			.sum()
	}

	/// The messages page is shown: the open private chat a session holds is
	/// the current chat again (leaving the page made the strip's current).
	pub(crate) fn messages_shown(&mut self) {
		let Some(Open { session: Some(id), uid, mail: None, stored: false, .. }) =
			self.social.dm.open.clone()
		else {
			return;
		};
		let target = ChatTarget::Private(uid);
		// Not for a session that is gone (its server was deleted).
		let Some(view) = self.sessions.get(&id) else { return };
		if !view.tabs.iter().any(|t| t.target == target)
			|| (self.current == Some(id)
				&& view.tabs.get(view.current_tab).is_some_and(|t| t.target == target))
		{
			return;
		}
		self.focus_private(id, target);
	}

	/// Make the private chat `target` session `id`'s current chat, and that
	/// server the current one. The tab first: selecting the server reads
	/// the chat it shows, which must not be the strip's.
	fn focus_private(&mut self, id: i64, target: ChatTarget) {
		let index = self.chat_tab(id, target.clone());
		self.sessions.entry(id).or_default().set_current_tab(index);
		if self.current != Some(id) {
			self.select_server(id);
		}
		self.open_chat(target, true);
	}

	/// The messages page was left: a session whose current chat is a
	/// private one goes back to the chat strip's
	/// ([`SessionView::channel_tab`]).
	pub(crate) fn close_messages(&mut self) {
		let mut changed = false;
		for view in self.sessions.values_mut() {
			changed |= view.leave_private();
		}
		if !changed {
			return;
		}
		self.refresh_chat();
		self.refresh_servers();
		// What is read after the callback: a server picked on the rail
		// (`Nav.select-server` leaves the page first) is selected by then,
		// and the chat that came back here was never on screen.
		later(|app| {
			if let Some(id) = app.current {
				app.fetch_previews(id);
			}
			app.prefetch_counts();
			app.chat_shown_changed();
		});
	}

	pub(crate) fn dm_tab(&mut self, tab: i32) {
		self.social.dm.tab = tab;
		if tab == 2 {
			for view in self.sessions.values_mut() {
				view.extra.inbox_asked = false;
			}
			self.ask_inboxes();
		}
		self.refresh_conversations();
	}

	pub(crate) fn search_conversations(&mut self, text: String) {
		self.social.dm.filter = text;
		self.refresh_conversations();
	}

	/// Open a chat of the lists (`-1` closes the open one).
	pub(crate) fn open_conversation(&mut self, key: i32) {
		if key < 0 {
			self.social.dm.open = None;
			self.refresh_dm();
			return;
		}
		if key >= MAIL {
			let Some((session, m)) = self.social.dm.inbox.get((key - MAIL) as usize).cloned()
			else {
				return;
			};
			self.open_mail(session, &m);
			return;
		}
		let Some(chat) = self.social.dm.chats.get(key as usize).cloned() else { return };
		match &chat.target {
			ChatTarget::Private(uid) => self.open_dm(chat.session, chat.server_uid.clone(), uid),
			target => {
				let Some(session) = chat.session else {
					self.set_status("That server is not in your bookmarks.");
					return;
				};
				self.show_server(session);
				self.open_chat(target.clone(), true);
			}
		}
	}

	/// Message someone: where they are with voice, else the newest
	/// conversation with them, else where they were seen.
	pub(crate) fn message_person(&mut self, uid: &str) {
		let spots = self.spots_of(uid);
		let session = spots.iter().find(|s| s.voice).or(spots.first()).map(|s| s.session);
		self.open_dm_with(session, uid);
	}

	/// Open the private chat with `uid`, in `session` if given.
	pub(crate) fn open_dm_with(&mut self, session: Option<i64>, uid: &str) {
		let target = ChatTarget::Private(uid.to_owned());
		let known = self
			.social
			.dm
			.chats
			.iter()
			.find(|c| c.target == target && (session.is_none() || c.session == session));
		let session = session.or_else(|| known.and_then(|c| c.session));
		let server_uid = session
			.and_then(|s| self.sessions.get(&s))
			.and_then(|v| v.state.server_uid.clone())
			.or_else(|| known.and_then(|c| c.server_uid.clone()));
		self.open_dm(session, server_uid, uid);
	}

	/// Open a private chat: in its session when one holds it, else from
	/// the store.
	pub(crate) fn open_dm(&mut self, session: Option<i64>, server_uid: Option<String>, uid: &str) {
		let live = session
			.and_then(|s| self.sessions.get(&s))
			.is_some_and(|v| v.has_history() || v.state.voice == VoiceState::Connected);
		self.social.dm.open = Some(Open {
			session,
			server_uid: server_uid.clone(),
			uid: uid.to_owned(),
			mail: None,
			stored: !live,
		});
		self.social.dm.mail = None;
		vm::list::sync(&self.models.social.dm_lines, &[]);
		let target = ChatTarget::Private(uid.to_owned());
		if let (true, Some(id)) = (live, session) {
			self.focus_private(id, target);
		} else if let Some(server_uid) = server_uid.filter(|_| !self.demo_ui) {
			let history = self.engine.history();
			let uid = uid.to_owned();
			self.engine.runtime().spawn(async move {
				let query = PageQuery { limit: Some(200), ..PageQuery::default() };
				let page = history.page(&server_uid, &target, query, false).await;
				later(move |app| app.stored_dm(&uid, page.unwrap_or_default()));
			});
		}
		self.navigate(|nav| nav.invoke_show(Page::Messages));
		self.refresh_chats();
	}

	/// The stored messages of a chat no session holds.
	pub(crate) fn stored_dm(&mut self, uid: &str, messages: Vec<HistoryMessage>) {
		if !self.social.dm.open.as_ref().is_some_and(|o| o.stored && o.uid == uid) {
			return;
		}
		let mut previous = None;
		let lines: Vec<_> = messages
			.iter()
			.enumerate()
			.map(|(i, m)| {
				let ctx = vm::chat::LineCtx {
					key: -1 - i as i32,
					avatar: m
						.message
						.author_uid
						.as_deref()
						.map(|u| self.avatar_of(u))
						.unwrap_or_default(),
					..Default::default()
				};
				let line = vm::chat::history_line(m, previous.as_ref(), &ctx);
				previous = Some(vm::chat::Previous::of(&m.message));
				line
			})
			.collect();
		vm::list::sync(&self.models.social.dm_lines, &lines);
		self.refresh_dm();
	}

	/// Whether the private chat with `uid` in `session` is on screen.
	pub(crate) fn dm_shown(&self, session: i64, uid: &str) -> bool {
		let page = self.ui.upgrade().map(|ui| ui.global::<Nav>().get_page());
		page == Some(Page::Messages)
			&& self
				.social
				.dm
				.open
				.as_ref()
				.is_some_and(|o| o.mail.is_none() && o.uid == uid && o.session == Some(session))
	}

	/// Who the open conversation is with, and what can be done.
	pub(crate) fn refresh_dm(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let Some(open) = &self.social.dm.open else {
			bridge.set_dm(DmPeer::default());
			return;
		};
		let uid = open.uid.as_str();
		let contact = self.contacts.get(uid);
		let spots = self.spots_of(uid);
		let here = open.session.and_then(|s| spots.iter().find(|sp| sp.session == s));
		let view = open.session.and_then(|s| self.sessions.get(&s));
		let client = here.and_then(|s| self.sessions[&s.session].presence.clients.get(&s.client));
		let tab =
			view.and_then(|v| v.tabs.iter().find(|t| t.target == ChatTarget::Private(uid.into())));
		let name = self.peer_name(uid, tab.and_then(|t| t.messages.last().map(|m| &m.message)));
		let server =
			open.session.map(|s| self.server_name(s)).unwrap_or_else(|| "another server".into());
		let voice = view.is_some_and(|v| v.state.voice == VoiceState::Connected);
		let blocked = contact.is_some_and(|c| c.relation == voelin_core::Relation::Blocked);
		let status = match here.or(spots.first()) {
			Some(s) => match &s.away {
				Some(m) if !m.is_empty() => format!("Away: {m} · {}", s.server),
				Some(_) => format!("Away · {}", s.server),
				None if s.channel_title.is_empty() => format!("Online · {}", s.server),
				None => format!("Online · {} · {}", s.server, s.channel_title),
			},
			None => match contact.filter(|c| c.last_seen_ms > 0) {
				Some(c) => format!("Offline · last seen {}", ago(c.last_seen_ms)),
				None => "Offline".into(),
			},
		};
		let groups: Vec<SharedString> = match (client, view) {
			(Some(c), Some(v)) => c
				.server_groups
				.iter()
				.filter_map(|g| v.server_groups.iter().find(|i| i.id == *g))
				.map(|g| g.name.clone().into())
				.collect(),
			_ => Vec::new(),
		};
		let servers: Vec<SharedString> = if spots.is_empty() {
			contact
				.and_then(|c| c.last_server.clone())
				.map(|s| {
					vec![
						format!("{s} · last seen {}", ago(contact.map_or(0, |c| c.last_seen_ms)))
							.into(),
					]
				})
				.unwrap_or_default()
		} else {
			spots
				.iter()
				.map(|s| {
					if s.channel_title.is_empty() {
						s.server.clone().into()
					} else {
						format!("{} · {}", s.server, s.channel_title).into()
					}
				})
				.collect()
		};
		// The pictures that are here (newest first), and the other files.
		let mut media = Vec::new();
		let mut files = Vec::new();
		for m in tab.iter().flat_map(|t| t.messages.iter().rev()) {
			let message = &m.message.message;
			for f in message.file_refs() {
				if let Some(image) = view.and_then(|v| crate::previews::image_of(v, &f)) {
					media.push(image);
					continue;
				}
				let when = vm::chat::time_of(message.ts_ms);
				files.push(FileItem {
					picture: vm::chat::is_picture(&f.name),
					detail: match f.size {
						Some(size) => format!("{} · {when}", vm::chat::size_text(size)),
						None => when,
					}
					.into(),
					name: f.name.into(),
					..Default::default()
				});
			}
		}
		let media_more = if media.len() > 6 { media.len() - 5 } else { 0 };
		if media_more > 0 {
			media.truncate(5);
		}
		let can_send = voice && here.is_some() && !blocked;
		let offline =
			voice && here.is_none() && view.is_some_and(|v| v.capabilities.offline_messages);
		let in_bookmarks = open.session.is_some_and(|s| self.bookmark(s).is_some());
		let notice = if open.session.is_none() {
			"This server is not in your bookmarks: the messages are the ones stored on this device."
				.to_owned()
		} else if !voice {
			format!("Connect to {server} with voice to send messages.")
		} else if here.is_none() {
			format!(
				"{name} is not on {server} now: TeamSpeak delivers private messages only to people online on the server."
			)
		} else if blocked {
			format!("You blocked {name}.")
		} else {
			String::new()
		};
		let mail = open.mail.and_then(|(s, id)| {
			self.social
				.dm
				.inbox
				.iter()
				.find(|(ms, m)| *ms == s && m.id == id)
				.map(|(_, m)| m.clone())
		});
		bridge.set_dm(DmPeer {
			open: true,
			uid: uid.into(),
			name: name.clone().into(),
			initials: vm::avatar::initials(&name).into(),
			tint: vm::avatar::tint(&name),
			avatar: self.avatar_of(uid),
			online: !spots.is_empty(),
			status: status.into(),
			description: client.and_then(|c| c.description.clone()).unwrap_or_default().into(),
			groups: crate::app::model(groups),
			note: contact.map(|c| c.note.clone()).unwrap_or_default().into(),
			since: contact
				.filter(|c| c.added_ms > 0)
				.and_then(|c| chrono::DateTime::from_timestamp_millis(c.added_ms))
				.map(|t| {
					format!("Contact since {}", t.with_timezone(&chrono::Local).format("%-d %b %Y"))
				})
				.unwrap_or_default()
				.into(),
			servers: crate::app::model(servers),
			media: crate::app::model(media),
			media_more: media_more as i32,
			files: crate::app::model(files),
			friend: contact.is_some_and(|c| c.relation == voelin_core::Relation::Friend),
			blocked,
			volume: contact.map_or(100.0, |c| c.volume * 100.0),
			muted: contact.is_some_and(|c| c.muted),
			can_send,
			notice: notice.into(),
			offline,
			can_connect: !voice && in_bookmarks,
			stored: open.stored,
			mail: open.mail.is_some(),
			mail_subject: mail.as_ref().map(|m| m.subject.clone()).unwrap_or_default().into(),
			mail_text: self
				.social
				.dm
				.mail
				.as_ref()
				.map(|m| plain(&m.text))
				.unwrap_or_default()
				.into(),
			mail_time: mail.map(|m| vm::chat::time_of(m.ts_s * 1000)).unwrap_or_default().into(),
		});
	}

	/// The buttons of the open conversation.
	pub(crate) fn dm_action(&mut self, action: &str) {
		let Some(open) = self.social.dm.open.clone() else { return };
		let uid = open.uid.clone();
		match action {
			"poke" | "join" | "mute" => self.contact_action(&uid, action),
			"friend" | "block" => {
				let current = self.contacts.get(&uid).map(|c| c.relation);
				let wanted = if action == "friend" {
					voelin_core::Relation::Friend
				} else {
					voelin_core::Relation::Blocked
				};
				let next = if current == Some(wanted) { "neutral" } else { action };
				self.contact_action(&uid, next);
			}
			"connect" => {
				if let Some(session) = open.session {
					self.select_server(session);
					self.connect_voice();
				}
			}
			"delete-mail" | "unread-mail" => {
				let Some((session, id)) = open.mail else { return };
				let request = self.offline_request(
					session,
					if action == "delete-mail" { "delete" } else { "unread" },
				);
				if !self.demo_ui {
					self.engine.send(if action == "delete-mail" {
						Command::DeleteOfflineMessage { session: session as u64, request, id }
					} else {
						Command::SetOfflineMessageRead {
							session: session as u64,
							request,
							id,
							read: false,
						}
					});
				}
				if action == "delete-mail" {
					self.social.dm.open = None;
				}
				self.refresh_chats();
			}
			_ => {}
		}
	}

	pub(crate) fn dm_volume(&mut self, percent: f32) {
		if let Some(uid) = self.social.dm.open.as_ref().map(|o| o.uid.clone()) {
			self.contact_volume(&uid, percent);
		}
	}

	fn offline_request(&mut self, session: i64, what: &'static str) -> u64 {
		self.social.dm.next_request += 1;
		let request = self.social.dm.next_request;
		self.social.dm.requests.insert(request, (session, what));
		request
	}

	/// Send an offline message through the open conversation's server.
	pub(crate) fn send_offline(&mut self, form: OfflineForm) {
		let session =
			self.social.dm.open.as_ref().and_then(|o| o.session).or(self.current).filter(|s| {
				self.sessions.get(s).is_some_and(|v| v.state.voice == VoiceState::Connected)
			});
		let Some(session) = session else {
			self.set_status("Connect with voice to send an offline message.");
			return;
		};
		let request = self.offline_request(session, "send");
		if !self.demo_ui {
			self.engine.send(Command::SendOfflineMessage {
				session: session as u64,
				request,
				to_uid: form.uid.to_string(),
				subject: if form.subject.is_empty() {
					"Message".into()
				} else {
					form.subject.to_string()
				},
				text: form.text.to_string(),
			});
		}
		self.set_status(format!("Sending an offline message to {}…", form.to));
	}

	/// Show an offline message (and ask the server for its text).
	fn open_mail(&mut self, session: i64, m: &OfflineMessageInfo) {
		self.social.dm.open = Some(Open {
			session: Some(session),
			server_uid: None,
			uid: m.from_uid.clone(),
			mail: Some((session, m.id)),
			stored: false,
		});
		self.social.dm.mail = None;
		let request = self.offline_request(session, "get");
		if !self.demo_ui {
			self.engine.send(Command::GetOfflineMessage {
				session: session as u64,
				request,
				id: m.id,
			});
		}
		self.refresh_chats();
	}

	pub(crate) fn mail_arrived(&mut self, session: i64, result: Result<OfflineMessage, String>) {
		match result {
			Ok(m) => {
				if let Some(view) = self.sessions.get_mut(&session)
					&& let Some(info) = view.extra.inbox.iter_mut().find(|i| i.id == m.id)
				{
					info.read = true;
				}
				self.social.dm.mail = Some(m);
			}
			Err(e) => self.set_status(format!("Could not read the offline message: {e}")),
		}
	}

	pub(crate) fn request_done(&mut self, session: i64, request: u64, result: Result<(), String>) {
		let Some((_, what)) = self.social.dm.requests.remove(&request) else { return };
		match (what, result) {
			("send", Ok(())) => self.set_status("Offline message sent"),
			("send", Err(e)) => self.set_status(format!("Could not send the offline message: {e}")),
			(_, Err(e)) => self.set_status(format!("Offline messages: {e}")),
			(_, Ok(())) => {
				if let Some(view) = self.sessions.get_mut(&session) {
					view.extra.inbox_asked = false;
				}
				self.ask_inboxes();
			}
		}
	}

	/// List the offline messages of voice sessions whose server keeps them.
	pub(crate) fn ask_inboxes(&mut self) {
		if self.demo_ui {
			return;
		}
		let ids: Vec<i64> = self
			.sessions
			.iter()
			.filter(|(_, v)| {
				v.state.voice == VoiceState::Connected
					&& v.capabilities.offline_messages
					&& !v.extra.inbox_asked
			})
			.map(|(id, _)| *id)
			.collect();
		for id in ids {
			let request = self.offline_request(id, "list");
			if let Some(view) = self.sessions.get_mut(&id) {
				view.extra.inbox_asked = true;
			}
			self.engine.send(Command::ListOfflineMessages { session: id as u64, request });
		}
	}

	/// Pokes from `uid` as lines of their private chat, among its messages
	/// (`lines` and `times` in step, oldest first).
	pub(crate) fn poke_lines(&self, session: i64, uid: &str) -> Vec<(i64, crate::app::ChatLine)> {
		self.social
			.pokes
			.iter()
			.enumerate()
			.filter(|(_, p)| p.session == session && p.uid.as_deref() == Some(uid))
			.map(|(i, p)| {
				let text = if p.message.is_empty() {
					"👋 poked you".to_owned()
				} else {
					format!("👋 poked you: {}", p.message)
				};
				let message = voelin_model::ChatMessage {
					target: ChatTarget::Private(uid.to_owned()),
					author_name: p.name.clone(),
					author_uid: p.uid.clone(),
					author_id: None,
					text,
					ts_ms: p.ts_ms,
					via_relay: false,
					blocked: false,
				};
				let mut line = vm::chat::line(&message, None);
				line.key = -1_000_000 - i as i32;
				line.avatar = self.avatar_of(uid);
				(p.ts_ms, line)
			})
			.collect()
	}
}
