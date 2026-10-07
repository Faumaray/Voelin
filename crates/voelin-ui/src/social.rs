//! People across servers: contacts and where they are (Friends, home), the
//! bell's notifications, pokes, and the search (Ctrl+K). The direct
//! messages are in `messages.rs`, the home page in `home.rs`, a server's
//! events in `events.rs`.
//!
//! Every contact's whereabouts come from the sessions' presence (voice,
//! gateway or query), so they are current for friends and the others alike;
//! [`Event::FriendPresence`] adds when a friend came online, for "12m ago"
//! and the notification.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tracing::debug;
use voelin_core::{
	Command, Contact, Event, FriendSpot, HistoryMessage, HistorySource, Relation, VoiceState,
};
use voelin_gateway_proto::{
	Action, ActivityEntry, ConfigEntry, EventInfo, PermRuleInfo, StreamEntry,
};
use voelin_model::{ChatMessage, ChatTarget};

use crate::app::{
	App, Bridge, ChatLine, ContactItem, ConversationItem, HappeningItem, LiveItem, Nav, NoticeItem,
	Page, RecordingItem, SearchItem, SettingsSection,
};
use crate::settings::{
	NOTIFY_EVENTS, NOTIFY_FRIENDS, NOTIFY_MENTIONS, NOTIFY_MESSAGES, NOTIFY_POKES, NotifyLevel,
};
use crate::vm;
use crate::vm::social::{ago, matches, mentions, plain};

/// What a session's gateway and voice connection add for these screens.
#[derive(Default)]
pub(crate) struct SessionExtra {
	/// The gateway's stream directory.
	pub directory: Vec<StreamEntry>,
	/// The gateway's events by start time, and those whose answers are shown.
	pub events: Vec<EventInfo>,
	pub events_loaded: bool,
	pub expanded: HashSet<i64>,
	/// The activity feed, newest first.
	pub activity: Vec<ActivityEntry>,
	/// Our unique id at the gateway, and what we may do there.
	pub gateway_uid: Option<String>,
	pub actions: Vec<Action>,
	/// Gateway administration: its keys and permission rules.
	pub config: Vec<ConfigEntry>,
	pub perms: Vec<PermRuleInfo>,
	/// Offline messages (voice), and whether they were asked for.
	pub inbox: Vec<voelin_core::OfflineMessageInfo>,
	pub inbox_asked: bool,
	/// Pictures linked in chat, by link (`previews.rs`).
	pub previews: HashMap<String, crate::previews::Preview>,
	/// When this session's presence first came (ms): friends found in it
	/// then are not "coming online".
	pub presence_since: i64,
	/// "TeamSpeak 6", "TeamSpeak 3" once the server told its version.
	pub flavor: String,
}

/// A poke we got.
pub(crate) struct Poke {
	pub session: i64,
	pub uid: Option<String>,
	pub name: String,
	pub message: String,
	pub ts_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoticeKind {
	Mention,
	Poke,
	Message,
	Event,
	Friend,
}

impl NoticeKind {
	fn name(self) -> &'static str {
		match self {
			Self::Mention => "mention",
			Self::Poke => "poke",
			Self::Message => "message",
			Self::Event => "event",
			Self::Friend => "friend",
		}
	}
}

/// Where a notification leads.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NoticeTarget {
	Chat(i64, ChatTarget),
	Dm(i64, String),
	Event(i64, i64),
	Contact(String),
}

/// A notification of the bell.
pub(crate) struct Notice {
	pub key: i32,
	pub kind: NoticeKind,
	pub title: String,
	pub body: String,
	pub ts_ms: i64,
	pub unread: bool,
	pub target: NoticeTarget,
	/// Whose avatar (initials from the name).
	pub name: String,
	pub uid: Option<String>,
}

/// What a search result opens.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Found {
	Server(i64),
	Channel(i64, u64),
	Person(i64, u16, Option<String>),
	Contact(String),
	Setting(SettingsSection),
}

/// Where someone is in one session.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Spot {
	pub session: i64,
	pub server: String,
	pub client: u16,
	pub channel: u64,
	/// The channel's name as shown (a spacer's text).
	pub channel_title: String,
	pub away: Option<String>,
	pub streaming: bool,
	/// We are connected with voice there (messages, pokes, moving).
	pub voice: bool,
}

/// State of the home, friends, messages, bell and search screens.
#[derive(Default)]
pub(crate) struct Social {
	/// Friends that are somewhere ([`Event::FriendPresence`]), and since when
	/// (this run).
	pub online_since: HashMap<String, i64>,
	pub pokes: Vec<Poke>,
	/// Newest first.
	pub notices: Vec<Notice>,
	pub next_notice: i32,
	/// Friends page: the tab (online, all, blocked), the search, the
	/// selected contact.
	pub friends_tab: i32,
	pub contact_filter: String,
	pub selected: Option<String>,
	/// What the search results open.
	pub found: Vec<Found>,
	/// Our unique ids (TeamSpeak 3 and 6 of every identity).
	pub own_uids: HashSet<String>,
	/// The direct messages (`messages.rs`).
	pub dm: crate::messages::Dms,
}

/// The models of these screens, set on the Bridge once.
pub(crate) struct SocialModels {
	pub friends_online: Rc<VecModel<ContactItem>>,
	pub friend_activity: Rc<VecModel<ContactItem>>,
	pub contacts: Rc<VecModel<ContactItem>>,
	pub blocked: Rc<VecModel<ContactItem>>,
	pub live: Rc<VecModel<LiveItem>>,
	pub recent: Rc<VecModel<ConversationItem>>,
	pub sidebar_dms: Rc<VecModel<ConversationItem>>,
	pub conversations: Rc<VecModel<ConversationItem>>,
	pub dm_lines: Rc<VecModel<ChatLine>>,
	pub happenings: Rc<VecModel<HappeningItem>>,
	pub recordings: Rc<VecModel<RecordingItem>>,
	pub notices: Rc<VecModel<NoticeItem>>,
	pub mentions: Rc<VecModel<NoticeItem>>,
	pub search: Rc<VecModel<SearchItem>>,
	pub events: Rc<VecModel<crate::app::EventItem>>,
}

impl SocialModels {
	pub fn new(bridge: &Bridge) -> Self {
		let m = Self {
			friends_online: Rc::default(),
			friend_activity: Rc::default(),
			contacts: Rc::default(),
			blocked: Rc::default(),
			live: Rc::default(),
			recent: Rc::default(),
			sidebar_dms: Rc::default(),
			conversations: Rc::default(),
			dm_lines: Rc::default(),
			happenings: Rc::default(),
			recordings: Rc::default(),
			notices: Rc::default(),
			mentions: Rc::default(),
			search: Rc::default(),
			events: Rc::default(),
		};
		bridge.set_friends_online(ModelRc::from(m.friends_online.clone()));
		bridge.set_friend_activity(ModelRc::from(m.friend_activity.clone()));
		bridge.set_contacts(ModelRc::from(m.contacts.clone()));
		bridge.set_blocked(ModelRc::from(m.blocked.clone()));
		bridge.set_live_rooms(ModelRc::from(m.live.clone()));
		bridge.set_recent_chats(ModelRc::from(m.recent.clone()));
		bridge.set_sidebar_dms(ModelRc::from(m.sidebar_dms.clone()));
		bridge.set_conversations(ModelRc::from(m.conversations.clone()));
		bridge.set_dm_messages(ModelRc::from(m.dm_lines.clone()));
		bridge.set_happenings(ModelRc::from(m.happenings.clone()));
		bridge.set_recordings(ModelRc::from(m.recordings.clone()));
		bridge.set_notifications(ModelRc::from(m.notices.clone()));
		bridge.set_home_mentions(ModelRc::from(m.mentions.clone()));
		bridge.set_search_results(ModelRc::from(m.search.clone()));
		bridge.set_events(ModelRc::from(m.events.clone()));
		m
	}
}

/// Which screens an event touched.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Touched {
	pub people: bool,
	pub chats: bool,
	pub home: bool,
}

pub(crate) fn now_ms() -> i64 {
	chrono::Utc::now().timestamp_millis()
}

/// Whether a message is ours: by one of our unique ids, or by our client in
/// its session (`own_client`).
pub(crate) fn own_message(
	own_uids: &HashSet<String>,
	own_client: Option<u16>,
	message: &ChatMessage,
) -> bool {
	message.author_uid.as_ref().is_some_and(|u| own_uids.contains(u))
		|| (own_client.is_some() && message.author_id == own_client)
}

/// The settings pages a search finds, with words that lead to them.
const SETTING_PAGES: [(&str, &str, SettingsSection); 11] = [
	("My Account", "account identity unique id nickname myteamspeak", SettingsSection::Account),
	("Profiles", "identities import export security level", SettingsSection::Profiles),
	("Appearance", "theme dark light text size font layout", SettingsSection::Appearance),
	(
		"Voice & Video",
		"microphone speakers output input camera noise echo gain volume quality",
		SettingsSection::Voice,
	),
	(
		"Streaming",
		"stream fps bitrate codec layers simulcast audio sources",
		SettingsSection::Streaming,
	),
	("Devices", "devices microphone speakers camera screen capture", SettingsSection::Devices),
	("Notifications", "notifications bell mentions pokes alerts", SettingsSection::Notifications),
	("Privacy & Safety", "privacy block blocked pokes messages crash", SettingsSection::Privacy),
	("Keybinds", "keybinds push to talk hotkey shortcut transmit", SettingsSection::Keybinds),
	("Integrations", "gateway integrations encoders ffmpeg admin", SettingsSection::Integrations),
	(
		"Advanced",
		"advanced srtp history cache logs settings experimental",
		SettingsSection::Advanced,
	),
];

impl App {
	/// The avatar picture of someone by unique id, from any session.
	pub(crate) fn avatar_of(&self, uid: &str) -> slint::Image {
		vm::avatar::image(self.sessions.values().find_map(|v| v.avatars.get(uid)))
	}

	/// A server's name: its bookmark's, else what the session calls it.
	pub(crate) fn server_name(&self, session: i64) -> String {
		self.bookmark(session).map(|b| b.name.clone()).unwrap_or_else(|| {
			self.sessions.get(&session).map(|v| v.presence.server_name.clone()).unwrap_or_default()
		})
	}

	/// Every session where the client with this unique id is, in bookmark
	/// order.
	pub(crate) fn spots_of(&self, uid: &str) -> Vec<Spot> {
		let mut ids: Vec<i64> = self.sessions.keys().copied().collect();
		ids.sort_by_key(|id| self.bookmarks.iter().position(|b| b.id == *id).unwrap_or(usize::MAX));
		let mut spots = Vec::new();
		for id in ids {
			let view = &self.sessions[&id];
			let Some(c) = view.presence.clients.values().find(|c| c.uid.as_deref() == Some(uid))
			else {
				continue;
			};
			let channel = view.presence.channels.get(&c.channel);
			spots.push(Spot {
				session: id,
				server: self.server_name(id),
				client: c.id,
				channel: c.channel,
				channel_title: channel
					.map(|ch| vm::tree::channel_title(ch).0.to_owned())
					.unwrap_or_default(),
				away: c.away.clone(),
				streaming: c.streaming == Some(true),
				voice: view.state.voice == VoiceState::Connected,
			});
		}
		spots
	}

	/// The title of what someone streams, if a directory or the voice
	/// connection says.
	fn stream_title(&self, spot: &Spot, uid: &str) -> Option<String> {
		let view = self.sessions.get(&spot.session)?;
		view.streams
			.iter()
			.find(|s| s.streamer.0 == spot.client)
			.map(|s| s.name.clone())
			.or_else(|| {
				view.extra.directory.iter().find(|e| e.streamer.uid == uid).map(|e| e.title.clone())
			})
			.filter(|t| !t.is_empty())
	}

	/// A contact for the screens.
	pub(crate) fn contact_item(&self, contact: &Contact) -> ContactItem {
		let spots = self.spots_of(&contact.uid);
		let first = spots.first();
		let name = if contact.nickname.is_empty() {
			first
				.map(|s| self.sessions[&s.session].nickname(s.client))
				.unwrap_or_else(|| contact.uid.chars().take(8).collect())
		} else {
			contact.nickname.clone()
		};
		let streaming = spots.iter().find(|s| s.streaming);
		let status = match (first, streaming) {
			(_, Some(s)) => match self.stream_title(s, &contact.uid) {
				Some(title) => format!("Streaming {title}"),
				None => "Streaming".into(),
			},
			(Some(s), None) => match &s.away {
				Some(m) if !m.is_empty() => format!("Away: {m}"),
				Some(_) => "Away".into(),
				None if s.channel_title.is_empty() => "Online".into(),
				None => format!("In {}", s.channel_title),
			},
			(None, None) => "Offline".into(),
		};
		let place = match first {
			Some(s) => s.server.clone(),
			None if contact.last_seen_ms > 0 => match &contact.last_server {
				Some(server) => format!("Last seen {} on {server}", ago(contact.last_seen_ms)),
				None => format!("Last seen {}", ago(contact.last_seen_ms)),
			},
			None => String::new(),
		};
		let when = match first {
			Some(_) => {
				self.social.online_since.get(&contact.uid).map(|t| ago(*t)).unwrap_or_default()
			}
			None => ago(contact.last_seen_ms),
		};
		let since = chrono::DateTime::from_timestamp_millis(contact.added_ms)
			.filter(|_| contact.added_ms > 0)
			.map(|t| {
				format!("Contact since {}", t.with_timezone(&chrono::Local).format("%-d %b %Y"))
			})
			.unwrap_or_default();
		let spot_texts: Vec<SharedString> = spots
			.iter()
			.map(|s| {
				if s.channel_title.is_empty() {
					s.server.clone().into()
				} else {
					format!("{} · {}", s.server, s.channel_title).into()
				}
			})
			.collect();
		ContactItem {
			uid: contact.uid.clone().into(),
			name: name.clone().into(),
			initials: vm::avatar::initials(&name).into(),
			tint: vm::avatar::tint(&name),
			avatar: self.avatar_of(&contact.uid),
			relation: contact.relation.as_str().into(),
			online: first.is_some(),
			away: first.is_some_and(|s| s.away.is_some()),
			streaming: streaming.is_some(),
			status: status.into(),
			place: place.into(),
			when: when.into(),
			note: contact.note.clone().into(),
			volume: contact.volume * 100.0,
			muted: contact.muted,
			since: since.into(),
			spots: crate::app::model(spot_texts),
			reachable: spots.iter().any(|s| s.voice),
			selected: self.social.selected.as_deref() == Some(contact.uid.as_str()),
		}
	}

	/// Contacts sorted for lists: online first (streaming, then present,
	/// then away), then by name.
	fn sorted_contacts(&self, items: &mut [ContactItem]) {
		items.sort_by_key(|c| {
			(!c.online, !c.streaming, c.away, c.relation != "friend", c.name.to_lowercase())
		});
	}

	/// Friends page, home's friends and activity, the block list.
	pub(crate) fn refresh_people(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let m = &self.models.social;
		let mut all: Vec<ContactItem> =
			self.contacts.values().map(|c| self.contact_item(c)).collect();
		self.sorted_contacts(&mut all);
		let friends: Vec<ContactItem> =
			all.iter().filter(|c| c.relation == "friend").cloned().collect();
		let online: Vec<ContactItem> = friends.iter().filter(|c| c.online).cloned().collect();
		vm::list::sync(&m.friends_online, &online[..online.len().min(20)]);
		// Online friends first, then the others by when they were seen.
		let mut activity = friends.clone();
		activity.sort_by_key(|c| {
			let seen = self.contacts.get(c.uid.as_str()).map_or(0, |c| c.last_seen_ms);
			let since = self.social.online_since.get(c.uid.as_str()).copied().unwrap_or(0);
			(!c.online, !c.streaming, std::cmp::Reverse(if c.online { since } else { seen }))
		});
		vm::list::sync(&m.friend_activity, &activity[..activity.len().min(7)]);
		let blocked: Vec<ContactItem> =
			all.iter().filter(|c| c.relation == "blocked").cloned().collect();
		vm::list::sync(&m.blocked, &blocked);
		bridge.set_contacts_online(
			all.iter().filter(|c| c.online && c.relation != "blocked").count() as i32,
		);
		bridge.set_contacts_all(all.len() as i32);
		bridge.set_contacts_blocked(blocked.len() as i32);
		let filter = &self.social.contact_filter;
		let shown: Vec<ContactItem> = all
			.iter()
			.filter(|c| match self.social.friends_tab {
				0 => c.online && c.relation != "blocked",
				2 => c.relation == "blocked",
				_ => true,
			})
			.filter(|c| matches(filter, &[&c.name, &c.uid, &c.note]))
			.cloned()
			.collect();
		vm::list::sync(&m.contacts, &shown);
		let selected = self
			.social
			.selected
			.as_ref()
			.and_then(|uid| all.iter().find(|c| c.uid.as_str() == uid))
			.cloned()
			.unwrap_or_default();
		bridge.set_contact(selected);
	}

	pub(crate) fn friends_tab(&mut self, tab: i32) {
		self.social.friends_tab = tab;
		self.refresh_people();
	}

	pub(crate) fn search_contacts(&mut self, text: String) {
		self.social.contact_filter = text;
		self.refresh_people();
	}

	pub(crate) fn select_contact(&mut self, uid: String) {
		self.social.selected = (!uid.is_empty()).then_some(uid);
		self.refresh_people();
	}

	/// Change a contact (or make one of someone we see).
	fn update_contact(&mut self, uid: &str, f: impl FnOnce(&mut Contact)) {
		let mut contact = self.contacts.get(uid).cloned().unwrap_or_else(|| {
			let mut c = Contact::new(uid);
			if let Some(s) = self.spots_of(uid).first() {
				c.nickname = self.sessions[&s.session].nickname(s.client);
			}
			c
		});
		f(&mut contact);
		self.contacts.insert(uid.to_owned(), contact.clone());
		if !self.demo_ui {
			self.engine.send(Command::SetContact { contact: Box::new(contact) });
		}
		self.refresh_people();
		self.refresh_member_card();
		self.refresh_dm();
	}

	/// The buttons of a contact: "message", "poke", "join", "watch",
	/// "friend", "block", "neutral", "remove", "mute".
	pub(crate) fn contact_action(&mut self, uid: &str, action: &str) {
		let spots = self.spots_of(uid);
		let voice = spots.iter().find(|s| s.voice).cloned();
		match action {
			"message" => self.message_person(uid),
			"poke" => {
				let Some(s) = voice else {
					self.set_status("Connect with voice to a server they are on to poke them.");
					return;
				};
				self.ask_poke(s.session, s.client);
			}
			"join" => self.join_person(&spots),
			"watch" => self.watch_person(&spots),
			"friend" => self.update_contact(uid, |c| c.relation = Relation::Friend),
			"block" => self.update_contact(uid, |c| c.relation = Relation::Blocked),
			"neutral" => self.update_contact(uid, |c| c.relation = Relation::Neutral),
			"mute" => self.update_contact(uid, |c| c.muted = !c.muted),
			"remove" => {
				self.contacts.remove(uid);
				if self.social.selected.as_deref() == Some(uid) {
					self.social.selected = None;
				}
				if !self.demo_ui {
					self.engine.send(Command::RemoveContact { uid: uid.to_owned() });
				}
				self.refresh_people();
			}
			_ => {}
		}
	}

	pub(crate) fn contact_note(&mut self, uid: &str, note: String) {
		self.update_contact(uid, |c| c.note = note);
		self.set_status("Note saved");
	}

	pub(crate) fn contact_volume(&mut self, uid: &str, percent: f32) {
		self.update_contact(uid, |c| c.volume = (percent / 100.0).max(0.0));
	}

	/// Go to someone's channel: move there with voice, or connect to that
	/// server into it (`join_channel`, which asks for a password first).
	fn join_person(&mut self, spots: &[Spot]) {
		let Some(s) = spots.iter().find(|s| s.voice).or(spots.first()).cloned() else {
			self.set_status("They are not on any of your servers right now.");
			return;
		};
		self.show_server(s.session);
		self.join_channel(s.session, s.channel);
	}

	/// Watch someone's stream: from any channel of a server we are on with
	/// voice (once the lookup brings it), else join them first and watch it
	/// when it shows.
	fn watch_person(&mut self, spots: &[Spot]) {
		let Some(s) = spots.iter().find(|s| s.streaming).cloned() else { return };
		self.show_server(s.session);
		if s.voice {
			self.watch_client(s.client);
		} else {
			self.pending_watch = Some((s.session, s.client));
			self.join_person(&[s]);
		}
	}

	/// The server page of a session.
	pub(crate) fn show_server(&mut self, session: i64) {
		if self.current != Some(session) {
			self.select_server(session);
		}
		self.navigate(|nav| nav.invoke_show(Page::Server));
	}

	/// Navigate after the current callback: `Nav`'s functions call back
	/// into the app (`Bridge.open-*`), which is borrowed now.
	pub(crate) fn navigate(&self, f: impl FnOnce(&Nav) + Send + 'static) {
		let weak = self.ui.clone();
		let _ = slint::invoke_from_event_loop(move || {
			if let Some(ui) = weak.upgrade() {
				f(&ui.global::<Nav>());
			}
		});
	}

	// The bell.

	/// Add a notification as the kind's setting says.
	#[allow(clippy::too_many_arguments)]
	pub(crate) fn notify(
		&mut self,
		kind: NoticeKind,
		title: String,
		body: String,
		target: NoticeTarget,
		name: String,
		uid: Option<String>,
	) {
		let key = match kind {
			NoticeKind::Mention => &NOTIFY_MENTIONS,
			NoticeKind::Poke => &NOTIFY_POKES,
			NoticeKind::Message => &NOTIFY_MESSAGES,
			NoticeKind::Event => &NOTIFY_EVENTS,
			NoticeKind::Friend => &NOTIFY_FRIENDS,
		};
		let level = self.prefs.get(key);
		if level == NotifyLevel::Off {
			return;
		}
		// The sample data of VOELIN_DEMO_UI never reaches the desktop.
		if level == NotifyLevel::Desktop && !self.demo_ui {
			let n = voelin_platform::Notification {
				urgent: kind == NoticeKind::Poke || kind == NoticeKind::Message,
				..voelin_platform::Notification::new(title.clone(), body.clone())
			};
			self.engine.runtime().spawn(async move {
				if let Err(e) = voelin_platform::notify(&n).await {
					debug!("no desktop notification: {e}");
				}
			});
		}
		self.social.next_notice += 1;
		self.social.notices.insert(
			0,
			Notice {
				key: self.social.next_notice,
				kind,
				title,
				body,
				ts_ms: now_ms(),
				unread: true,
				target,
				name,
				uid,
			},
		);
		self.social.notices.truncate(100);
		self.refresh_notices();
	}

	/// The bell's notices, and home's unread mentions (the newest three).
	pub(crate) fn refresh_notices(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let item = |n: &Notice| NoticeItem {
			key: n.key,
			kind: n.kind.name().into(),
			title: n.title.clone().into(),
			body: n.body.clone().into(),
			time: ago(n.ts_ms).into(),
			unread: n.unread,
			initials: vm::avatar::initials(&n.name).into(),
			tint: vm::avatar::tint(&n.name),
			avatar: n.uid.as_deref().map(|u| self.avatar_of(u)).unwrap_or_default(),
		};
		let notices = &self.social.notices;
		let items: Vec<NoticeItem> = notices.iter().map(item).collect();
		vm::list::sync(&self.models.social.notices, &items);
		let mentions: Vec<NoticeItem> = notices
			.iter()
			.filter(|n| n.unread && n.kind == NoticeKind::Mention)
			.take(3)
			.map(item)
			.collect();
		vm::list::sync(&self.models.social.mentions, &mentions);
		let unread = notices.iter().filter(|n| n.unread).count();
		ui.global::<Bridge>().set_notifications_unread(unread as i32);
	}

	pub(crate) fn mark_notices_read(&mut self) {
		for n in &mut self.social.notices {
			n.unread = false;
		}
		self.refresh_notices();
	}

	pub(crate) fn clear_notices(&mut self) {
		self.social.notices.clear();
		self.refresh_notices();
	}

	/// Open what a notification is about.
	pub(crate) fn open_notice(&mut self, key: i32) {
		let Some(n) = self.social.notices.iter_mut().find(|n| n.key == key) else { return };
		n.unread = false;
		let target = n.target.clone();
		self.refresh_notices();
		match target {
			NoticeTarget::Chat(session, chat) => {
				self.show_server(session);
				self.open_chat(chat, true);
			}
			NoticeTarget::Dm(session, uid) => self.open_dm_with(Some(session), &uid),
			NoticeTarget::Event(session, _) => {
				self.select_server(session);
				self.navigate(|nav| nav.invoke_show(Page::Events));
			}
			NoticeTarget::Contact(uid) => {
				self.social.selected = Some(uid);
				self.refresh_people();
				self.navigate(|nav| nav.invoke_show(Page::Friends));
			}
		}
	}

	/// Our nickname in a session.
	fn own_nick(&self, session: i64) -> String {
		let view = self.sessions.get(&session);
		view.and_then(|v| v.state.own_client.map(|c| v.nickname(c)))
			.or_else(|| self.bookmark(session).map(|b| b.nickname.clone()))
			.unwrap_or_default()
	}

	/// Whether a message is ours.
	pub(crate) fn is_own(&self, session: i64, message: &ChatMessage) -> bool {
		let own_client = self.sessions.get(&session).and_then(|v| v.state.own_client);
		own_message(&self.social.own_uids, own_client, message)
	}

	/// A new chat message: a private message, or one that mentions us.
	fn chat_notice(&mut self, session: i64, message: &ChatMessage) {
		if message.blocked || self.is_own(session, message) {
			return;
		}
		let server = self.server_name(session);
		match &message.target {
			ChatTarget::Private(uid) => {
				if self.dm_shown(session, uid) {
					return;
				}
				self.notify(
					NoticeKind::Message,
					message.author_name.clone(),
					plain(&message.text),
					NoticeTarget::Dm(session, uid.clone()),
					message.author_name.clone(),
					Some(uid.clone()),
				);
			}
			target => {
				if !mentions(&message.text, &self.own_nick(session)) {
					return;
				}
				let place = match target {
					ChatTarget::Channel(cid) => self
						.sessions
						.get(&session)
						.and_then(|v| v.presence.channels.get(cid))
						.map(|c| format!("{} · {server}", vm::tree::channel_title(c).0))
						.unwrap_or(server),
					_ => server,
				};
				self.notify(
					NoticeKind::Mention,
					format!("{} mentioned you", message.author_name),
					format!("{} — {place}", plain(&message.text)),
					NoticeTarget::Chat(session, target.clone()),
					message.author_name.clone(),
					message.author_uid.clone(),
				);
			}
		}
	}

	/// Look at an event before the app handles it: notifications, pokes,
	/// friends' whereabouts, offline messages. Returns what to refresh after.
	pub(crate) fn social_event(&mut self, event: &Event) -> Touched {
		let mut touched = Touched::default();
		match event {
			Event::Presence { session, .. } => {
				let view = self.sessions.entry(*session as i64).or_default();
				if view.extra.presence_since == 0 {
					view.extra.presence_since = now_ms();
				}
				touched.people = true;
				touched.home = true;
			}
			Event::State { session, state } => {
				let id = *session as i64;
				let was = self.sessions.get(&id).map(|v| v.state.voice);
				if state.voice != VoiceState::Connected {
					if let Some(view) = self.sessions.get_mut(&id) {
						view.extra.inbox_asked = false;
					}
				} else if was != Some(VoiceState::Connected) {
					touched.chats = true;
				}
				touched.people = true;
				touched.home = true;
			}
			Event::ServerInfo { session, flavor, .. } => {
				self.sessions.entry(*session as i64).or_default().extra.flavor = match flavor {
					voelin_model::ServerFlavor::Ts3(_) => "TeamSpeak 3".into(),
					voelin_model::ServerFlavor::Ts6(_) => "TeamSpeak 6".into(),
					voelin_model::ServerFlavor::Unknown(_) => String::new(),
				};
				touched.people = true;
				touched.home = true;
			}
			Event::ContactsChanged { .. } | Event::AvatarReady { .. } => {
				touched.people = true;
				touched.home = true;
				touched.chats = true;
			}
			Event::StreamsChanged { .. } => {
				touched.home = true;
				touched.people = true;
			}
			Event::FriendPresence { uid, sessions } => self.friend_presence(uid, sessions),
			Event::Poke { session, from_uid, from_name, message, .. } => {
				let id = *session as i64;
				self.social.pokes.push(Poke {
					session: id,
					uid: from_uid.clone(),
					name: from_name.clone(),
					message: message.clone(),
					ts_ms: now_ms(),
				});
				let body = if message.is_empty() {
					format!("on {}", self.server_name(id))
				} else {
					message.clone()
				};
				let target = match from_uid {
					Some(uid) => NoticeTarget::Dm(id, uid.clone()),
					None => NoticeTarget::Chat(id, ChatTarget::Server),
				};
				self.notify(
					NoticeKind::Poke,
					format!("{from_name} poked you"),
					body,
					target,
					from_name.clone(),
					from_uid.clone(),
				);
				touched.chats = true;
			}
			Event::Chat { session, message } => {
				let id = *session as i64;
				if !self.sessions.get(&id).is_some_and(|v| v.has_history()) {
					self.chat_notice(id, message);
					touched.chats = true;
				}
			}
			Event::ChatHistory { session, target, messages, source, .. } => {
				let id = *session as i64;
				if *source == HistorySource::Live {
					let known: HashSet<i64> = self
						.sessions
						.get(&id)
						.and_then(|v| v.tabs.iter().find(|t| t.target == *target))
						.map(|t| t.messages.iter().map(|m| m.message.id).collect())
						.unwrap_or_default();
					let fresh: Vec<HistoryMessage> =
						messages.iter().filter(|m| !known.contains(&m.id)).cloned().collect();
					for m in &fresh {
						self.chat_notice(id, &m.message);
					}
				}
				touched.chats = true;
			}
			Event::OfflineMessages { session, result, .. } => {
				match result {
					Ok(list) => {
						self.sessions.entry(*session as i64).or_default().extra.inbox =
							list.clone();
					}
					Err(e) => debug!("offline messages: {e}"),
				}
				touched.chats = true;
			}
			Event::OfflineMessage { session, result, .. } => {
				self.mail_arrived(*session as i64, result.clone());
				touched.chats = true;
			}
			Event::RequestDone { session, request, result } => {
				self.request_done(*session as i64, *request, result.clone());
			}
			_ => {}
		}
		touched
	}

	/// Refresh what an event touched (after the app handled it).
	pub(crate) fn social_refresh(&mut self, touched: Touched) {
		if touched.people {
			self.refresh_people();
		}
		if touched.chats {
			self.ask_inboxes();
			self.refresh_chats();
		}
		if touched.home {
			self.refresh_home();
		}
	}

	fn friend_presence(&mut self, uid: &str, sessions: &[FriendSpot]) {
		let was = self.social.online_since.contains_key(uid);
		if sessions.is_empty() {
			self.social.online_since.remove(uid);
		} else if !was {
			let now = now_ms();
			self.social.online_since.insert(uid.to_owned(), now);
			// Friends found in a session's first presence were there before.
			let new = sessions.iter().all(|s| {
				self.sessions.get(&(s.session as i64)).is_some_and(|v| {
					v.extra.presence_since > 0 && now - v.extra.presence_since > 10_000
				})
			});
			if new {
				let s = &sessions[0];
				let channel = self
					.sessions
					.get(&(s.session as i64))
					.and_then(|v| v.presence.channels.get(&s.channel))
					.map_or(s.channel_name.as_str(), |c| vm::tree::channel_title(c).0);
				let place = if channel.is_empty() {
					format!("on {}", self.server_name(s.session as i64))
				} else {
					format!("on {} · {channel}", self.server_name(s.session as i64))
				};
				self.notify(
					NoticeKind::Friend,
					format!("{} is online", s.nickname),
					place,
					NoticeTarget::Contact(uid.to_owned()),
					s.nickname.clone(),
					Some(uid.to_owned()),
				);
			}
		}
		self.refresh_people();
		self.refresh_home();
	}

	// The search (Ctrl+K).

	/// Fill the results for `text`: servers, channels, people, contacts,
	/// settings pages ("@…": people only).
	pub(crate) fn search(&mut self, text: String) {
		let people_only = text.starts_with('@');
		let query = text.trim_start_matches('@').trim().to_owned();
		let mut items: Vec<SearchItem> = Vec::new();
		let mut found: Vec<Found> = Vec::new();
		let mut group =
			|items: &mut Vec<SearchItem>, header: &str, mut add: Vec<(SearchItem, Found)>| {
				add.truncate(8);
				for (i, (mut item, f)) in add.into_iter().enumerate() {
					if i == 0 {
						item.header = header.into();
					}
					items.push(item);
					found.push(f);
				}
			};
		if !people_only {
			let servers = self
				.bookmarks
				.iter()
				.zip(vm::servers::tints(&self.bookmarks))
				.filter(|(b, _)| matches(&query, &[&b.name, &b.address]))
				.map(|(b, tint)| {
					(
						SearchItem {
							kind: "server".into(),
							title: b.name.clone().into(),
							subtitle: b.address.clone().into(),
							initials: vm::avatar::initials(&b.name).into(),
							tint,
							avatar: self.server_list_icon(b),
							..Default::default()
						},
						Found::Server(b.id),
					)
				})
				.collect();
			group(&mut items, "Servers", servers);
		}
		if !query.is_empty() && !people_only {
			let mut channels = Vec::new();
			for b in &self.bookmarks {
				let Some(view) = self.sessions.get(&b.id) else { continue };
				for (channel, title) in
					vm::social::found_channels(view.presence.channels.values(), &query)
				{
					channels.push((
						SearchItem {
							kind: "channel".into(),
							title: title.into(),
							subtitle: b.name.clone().into(),
							..Default::default()
						},
						Found::Channel(b.id, channel),
					));
				}
			}
			group(&mut items, "Channels", channels);
		}
		if !query.is_empty() || people_only {
			let mut people = Vec::new();
			let mut seen: HashSet<String> = HashSet::new();
			for b in &self.bookmarks {
				let Some(view) = self.sessions.get(&b.id) else { continue };
				for c in view.presence.clients.values() {
					if c.is_query || Some(c.id) == view.state.own_client {
						continue;
					}
					if !matches(&query, &[&c.nickname]) {
						continue;
					}
					if let Some(uid) = &c.uid {
						seen.insert(uid.clone());
					}
					let channel = view
						.presence
						.channels
						.get(&c.channel)
						.map_or("", |ch| vm::tree::channel_title(ch).0);
					people.push((
						SearchItem {
							kind: "person".into(),
							title: c.nickname.clone().into(),
							subtitle: format!("{} · {channel}", b.name).into(),
							initials: vm::avatar::initials(&c.nickname).into(),
							tint: vm::avatar::tint(&c.nickname),
							avatar: vm::avatar::image(view.avatar(c.id)),
							..Default::default()
						},
						Found::Person(b.id, c.id, c.uid.clone()),
					));
				}
			}
			for c in self.contacts.values() {
				if seen.contains(&c.uid) || !matches(&query, &[&c.nickname, &c.note]) {
					continue;
				}
				let name = if c.nickname.is_empty() { c.uid.clone() } else { c.nickname.clone() };
				people.push((
					SearchItem {
						kind: "contact".into(),
						title: name.clone().into(),
						subtitle: match c.relation {
							Relation::Friend => "Friend · offline".into(),
							Relation::Blocked => "Blocked".into(),
							Relation::Neutral => "Contact · offline".into(),
						},
						initials: vm::avatar::initials(&name).into(),
						tint: vm::avatar::tint(&name),
						avatar: self.avatar_of(&c.uid),
						..Default::default()
					},
					Found::Contact(c.uid.clone()),
				));
			}
			group(&mut items, "People", people);
		}
		if !people_only {
			let pages = SETTING_PAGES
				.iter()
				.filter(|(title, words, _)| matches(&query, &[title, words]))
				.map(|(title, _, section)| {
					(
						SearchItem {
							kind: "setting".into(),
							title: (*title).into(),
							subtitle: "Settings".into(),
							..Default::default()
						},
						Found::Setting(*section),
					)
				})
				.collect();
			group(&mut items, "Settings", pages);
		}
		self.social.found = found;
		vm::list::sync(&self.models.social.search, &items);
	}

	/// Open a search result.
	pub(crate) fn search_activate(&mut self, index: i32) {
		let Some(found) =
			usize::try_from(index).ok().and_then(|i| self.social.found.get(i)).cloned()
		else {
			return;
		};
		match found {
			Found::Server(id) => self.show_server(id),
			Found::Channel(id, channel) => {
				self.show_server(id);
				self.open_chat(ChatTarget::Channel(channel), true);
			}
			Found::Person(id, client, Some(uid)) => {
				let _ = client;
				self.open_dm_with(Some(id), &uid);
			}
			Found::Person(id, client, None) => {
				self.show_server(id);
				self.open_member(i32::from(client));
			}
			Found::Contact(uid) => {
				self.social.selected = Some(uid);
				self.refresh_people();
				self.navigate(|nav| nav.invoke_show(Page::Friends));
			}
			Found::Setting(section) => self.navigate(move |nav| nav.invoke_open_settings(section)),
		}
	}
}

/// What a privacy or notification choice is as an index, and back.
pub(crate) fn allowed_index(a: voelin_core::settings::Allowed) -> i32 {
	match a {
		voelin_core::settings::Allowed::Everyone => 0,
		voelin_core::settings::Allowed::Friends => 1,
		voelin_core::settings::Allowed::Nobody => 2,
	}
}

pub(crate) fn allowed_of(index: i32) -> voelin_core::settings::Allowed {
	match index {
		1 => voelin_core::settings::Allowed::Friends,
		2 => voelin_core::settings::Allowed::Nobody,
		_ => voelin_core::settings::Allowed::Everyone,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn privacy_indices() {
		for i in 0..3 {
			assert_eq!(allowed_index(allowed_of(i)), i);
		}
		assert_eq!(allowed_of(9), voelin_core::settings::Allowed::Everyone);
		assert_eq!(NoticeKind::Poke.name(), "poke");
		assert!(SETTING_PAGES.iter().any(|(t, _, _)| *t == "Voice & Video"));
	}
}
