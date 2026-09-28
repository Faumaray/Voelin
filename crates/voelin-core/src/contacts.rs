//! Contacts: friends, blocked people and per-person settings, by unique id,
//! across all sessions.
//!
//! # Contract for the UI
//!
//! - [`crate::Command::SetContact`] adds or changes a contact (all fields;
//!   `added_ms` 0 is set to now; `last_seen_*` and an empty nickname keep the stored
//!   one), [`crate::Command::RemoveContact`] removes it. Both, and loading
//!   the contacts of a newly attached database, emit
//!   [`crate::Event::ContactsChanged`] with the full list;
//!   [`crate::Engine::contacts`] reads it any time.
//! - [`crate::Event::FriendPresence`] tells where a friend is: every
//!   session whose presence (voice, gateway or query) shows them, with
//!   client, channel, away and streaming state; an empty list when they
//!   left the last one. It comes when that changes, and for every friend
//!   when the contacts change.
//! - Blocked contacts: their server and channel messages are reported with
//!   [`voelin_model::ChatMessage::blocked`] set (live and from history);
//!   their private messages and pokes are dropped or flagged per
//!   `privacy.block_mode`.
//! - A contact's `muted` and `volume` apply to their voice in every session
//!   (as [`crate::Command::SetClientMuted`] / `SetClientVolume` would).
//! - `stream.permissions = friends` accepts friends' requests to watch our
//!   stream and asks for everyone else.
//!
//! Contacts live in the client database ([`crate::History`]'s, table
//! `contacts`), in memory for lookups. When a contact appears on or leaves a
//! server, `last_seen_ms` / `last_server` are updated (in one write per
//! presence change).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use tokio::sync::{broadcast, watch};
use tracing::warn;
use voelin_model::{ChannelId, Presence};
use voelin_store::ContactSeen;
pub use voelin_store::{Contact, Relation};

use crate::history::{self, SharedHistory};
use crate::{Event, SessionId};

/// Where a friend is in one session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FriendSpot {
	pub session: SessionId,
	/// The server's name as the session sees it.
	pub server_name: String,
	pub client: u16,
	pub nickname: String,
	pub channel: ChannelId,
	pub channel_name: String,
	/// Away, with the message (empty without one).
	pub away: Option<String>,
	pub streaming: bool,
}

#[derive(Default)]
struct Inner {
	contacts: HashMap<String, Contact>,
	/// Last presence per session.
	sessions: BTreeMap<SessionId, Arc<Presence>>,
	/// Contacts present per session (for sightings).
	present: HashMap<SessionId, HashSet<String>>,
	/// Friends' spots as last reported (friends seen somewhere only).
	reported: HashMap<String, Vec<FriendSpot>>,
}

/// See the [module docs](self). Cheap to clone.
#[derive(Clone)]
pub(crate) struct Contacts {
	inner: Arc<Mutex<Inner>>,
	events: broadcast::Sender<Event>,
	history: SharedHistory,
	/// Bumped on every change of the contacts, for sessions.
	revision: watch::Sender<u64>,
}

impl Contacts {
	pub fn new(events: broadcast::Sender<Event>, history: SharedHistory) -> Self {
		Self { inner: Arc::default(), events, history, revision: watch::Sender::new(0) }
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
		self.inner.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Told on every change of the contacts.
	pub fn watch(&self) -> watch::Receiver<u64> {
		self.revision.subscribe()
	}

	pub fn list(&self) -> Vec<Contact> {
		let mut list: Vec<_> = self.lock().contacts.values().cloned().collect();
		list.sort_by(|a, b| a.uid.cmp(&b.uid));
		list
	}

	pub fn get(&self, uid: &str) -> Option<Contact> {
		self.lock().contacts.get(uid).cloned()
	}

	pub fn relation(&self, uid: &str) -> Relation {
		self.lock().contacts.get(uid).map_or(Relation::Neutral, |c| c.relation)
	}

	pub fn is_blocked(&self, uid: Option<&str>) -> bool {
		uid.is_some_and(|uid| self.relation(uid) == Relation::Blocked)
	}

	/// Read the contacts of the current database (after it was attached).
	pub fn load(&self) {
		let this = self.clone();
		self.history.current().run_then(
			false,
			|s| s.contacts(),
			move |result| match result {
				Ok(list) => {
					this.lock().contacts = list.into_iter().map(|c| (c.uid.clone(), c)).collect();
					this.changed();
				}
				Err(e) => warn!("cannot read contacts: {e}"),
			},
		);
	}

	/// Add or change a contact.
	pub fn set(&self, mut contact: Contact) {
		{
			let mut inner = self.lock();
			if let Some(old) = inner.contacts.get(&contact.uid) {
				if contact.added_ms == 0 {
					contact.added_ms = old.added_ms;
				}
				if contact.nickname.is_empty() {
					contact.nickname = old.nickname.clone();
				}
				contact.last_seen_ms = contact.last_seen_ms.max(old.last_seen_ms);
				if contact.last_server.is_none() {
					contact.last_server = old.last_server.clone();
				}
			}
			if contact.added_ms == 0 {
				contact.added_ms = history::now_ms();
			}
			inner.contacts.insert(contact.uid.clone(), contact.clone());
		}
		self.history.current().run_then(
			false,
			move |s| s.put_contact(&contact),
			|r| {
				if let Err(e) = r {
					warn!("cannot store a contact: {e}");
				}
			},
		);
		self.changed();
	}

	pub fn remove(&self, uid: &str) {
		if self.lock().contacts.remove(uid).is_none() {
			return;
		}
		let uid = uid.to_owned();
		self.history.current().run_then(
			false,
			move |s| s.delete_contact(&uid),
			|r| {
				if let Err(e) = r {
					warn!("cannot remove a contact: {e}");
				}
			},
		);
		self.changed();
	}

	/// The contacts changed: tell the UI and the sessions, update friends.
	fn changed(&self) {
		self.revision.send_modify(|r| *r += 1);
		let _ = self.events.send(Event::ContactsChanged { contacts: Arc::new(self.list()) });
		self.update_friends(true);
	}

	/// A session published presence (empty when it has none).
	pub fn presence(&self, session: SessionId, presence: Arc<Presence>) {
		let now = history::now_ms();
		let server = Some(presence.server_name.clone()).filter(|n| !n.is_empty());
		let seen = {
			let mut inner = self.lock();
			let here: HashMap<&str, &str> = presence
				.clients
				.values()
				.filter_map(|c| {
					let uid = c.uid.as_deref()?;
					inner.contacts.contains_key(uid).then_some((uid, c.nickname.as_str()))
				})
				.collect();
			let before = inner.present.remove(&session).unwrap_or_default();
			// Sightings: contacts that came or went.
			let mut seen: Vec<ContactSeen> = Vec::new();
			for (uid, nick) in &here {
				if !before.contains(*uid) {
					seen.push(ContactSeen {
						uid: (*uid).to_owned(),
						nickname: (*nick).to_owned(),
						ts_ms: now,
						server: server.clone(),
					});
				}
			}
			for uid in before.iter().filter(|u| !here.contains_key(u.as_str())) {
				seen.push(ContactSeen {
					uid: uid.clone(),
					nickname: String::new(),
					ts_ms: now,
					server: server.clone(),
				});
			}
			for s in &seen {
				if let Some(c) = inner.contacts.get_mut(&s.uid) {
					c.last_seen_ms = c.last_seen_ms.max(now);
					if server.is_some() {
						c.last_server = server.clone();
					}
					if !s.nickname.is_empty() {
						c.nickname = s.nickname.clone();
					}
				}
			}
			let present = here.keys().map(|u| (*u).to_owned()).collect();
			inner.present.insert(session, present);
			inner.sessions.insert(session, presence);
			seen
		};
		if !seen.is_empty() {
			self.history.current().run_then(
				false,
				move |s| s.touch_contacts(&seen),
				|r| {
					if let Err(e) = r {
						warn!("cannot store when contacts were seen: {e}");
					}
				},
			);
		}
		self.update_friends(false);
	}

	/// A session ended: its friends are gone from it.
	pub fn session_closed(&self, session: SessionId) {
		self.presence(session, Arc::new(Presence::default()));
		{
			let mut inner = self.lock();
			inner.sessions.remove(&session);
			inner.present.remove(&session);
		}
		self.update_friends(false);
	}

	/// Report friends whose spots changed (every friend with `all`).
	fn update_friends(&self, all: bool) {
		let mut out: Vec<(String, Vec<FriendSpot>)> = Vec::new();
		{
			let inner = &mut *self.lock();
			let friends: HashSet<String> = inner
				.contacts
				.values()
				.filter(|c| c.relation == Relation::Friend)
				.map(|c| c.uid.clone())
				.collect();
			let mut now: HashMap<String, Vec<FriendSpot>> = HashMap::new();
			for (session, presence) in &inner.sessions {
				for (uid, spot) in spots_in(*session, presence, &friends) {
					now.entry(uid).or_default().push(spot);
				}
			}
			let mut uids: HashSet<String> = now.keys().cloned().collect();
			uids.extend(inner.reported.keys().cloned());
			if all {
				uids.extend(friends.iter().cloned());
			}
			for uid in uids {
				let spots = now.remove(&uid).unwrap_or_default();
				let before = inner.reported.get(&uid);
				if !all && before.map_or(spots.is_empty(), |b| *b == spots) {
					continue;
				}
				if spots.is_empty() {
					inner.reported.remove(&uid);
				} else {
					inner.reported.insert(uid.clone(), spots.clone());
				}
				out.push((uid, spots));
			}
		}
		out.sort_by(|a, b| a.0.cmp(&b.0));
		for (uid, sessions) in out {
			let _ = self.events.send(Event::FriendPresence { uid, sessions });
		}
	}
}

/// Friends' spots in one session's presence.
fn spots_in(
	session: SessionId,
	presence: &Presence,
	friends: &HashSet<String>,
) -> HashMap<String, FriendSpot> {
	let mut spots = HashMap::new();
	for c in presence.clients.values() {
		let Some(uid) = c.uid.as_ref().filter(|u| friends.contains(*u)) else { continue };
		// Several connections of one identity: the first.
		spots.entry(uid.clone()).or_insert_with(|| FriendSpot {
			session,
			server_name: presence.server_name.clone(),
			client: c.id,
			nickname: c.nickname.clone(),
			channel: c.channel,
			channel_name: presence
				.channels
				.get(&c.channel)
				.map(|ch| ch.name.clone())
				.unwrap_or_default(),
			away: c.away.clone(),
			streaming: c.streaming.unwrap_or(false),
		});
	}
	spots
}

#[cfg(test)]
mod tests {
	use voelin_model::{ChannelInfo, ClientInfo};

	use super::*;
	use crate::History;

	fn presence(server: &str, clients: &[(u16, &str, &str)]) -> Arc<Presence> {
		let mut p = Presence { server_name: server.into(), ..Presence::default() };
		p.channels.insert(1, ChannelInfo { id: 1, name: "Lobby".into(), ..Default::default() });
		for (id, uid, nick) in clients {
			let c = ClientInfo {
				id: *id,
				uid: Some((*uid).into()),
				nickname: (*nick).into(),
				channel: 1,
				..Default::default()
			};
			p.clients.insert(*id, c);
		}
		Arc::new(p)
	}

	fn friend(uid: &str) -> Contact {
		Contact { relation: Relation::Friend, ..Contact::new(uid) }
	}

	type Spots = Vec<(SessionId, u16)>;

	/// The friend events so far: uid and (session, client) per spot.
	fn friend_events(rx: &mut broadcast::Receiver<Event>) -> Vec<(String, Spots)> {
		let mut out = Vec::new();
		while let Ok(e) = rx.try_recv() {
			if let Event::FriendPresence { uid, sessions } = e {
				out.push((uid, sessions.iter().map(|s| (s.session, s.client)).collect()));
			}
		}
		out
	}

	#[tokio::test]
	async fn friends_across_sessions() {
		let (events, mut rx) = broadcast::channel(64);
		let history = SharedHistory::new(History::in_memory());
		let contacts = Contacts::new(events, history.clone());
		contacts.set(friend("f="));
		contacts.set(Contact { relation: Relation::Blocked, ..Contact::new("b=") });
		assert_eq!(contacts.relation("b="), Relation::Blocked);
		assert!(contacts.is_blocked(Some("b=")));
		assert!(!contacts.is_blocked(Some("f=")) && !contacts.is_blocked(None));
		// Changing the contacts reports every friend (offline here).
		assert!(friend_events(&mut rx).contains(&("f=".into(), vec![])));

		contacts.presence(1, presence("A", &[(5, "f=", "Fred"), (6, "x=", "X")]));
		assert_eq!(friend_events(&mut rx), [("f=".into(), vec![(1, 5)])]);
		// Nothing new: nothing reported.
		contacts.presence(1, presence("A", &[(5, "f=", "Fred"), (7, "y=", "Y")]));
		assert!(friend_events(&mut rx).is_empty());
		contacts.presence(2, presence("B", &[(9, "f=", "Fred")]));
		assert_eq!(friend_events(&mut rx), [("f=".into(), vec![(1, 5), (2, 9)])]);
		contacts.session_closed(1);
		assert_eq!(friend_events(&mut rx), [("f=".into(), vec![(2, 9)])]);
		// No friend any more: reported gone.
		contacts.set(Contact::new("f="));
		assert!(friend_events(&mut rx).contains(&("f=".into(), vec![])));

		// Sightings reach the database.
		history.current().flush();
		let stored = history.current().run(false, |s| s.contact("f=")).await.unwrap().unwrap();
		assert_eq!(stored.last_server.as_deref(), Some("B"));
		assert_eq!(stored.nickname, "Fred");
		assert!(stored.last_seen_ms > 0 && stored.added_ms > 0);
	}

	#[tokio::test]
	async fn load_and_remove() {
		let (events, mut rx) = broadcast::channel(64);
		let history = SharedHistory::new(History::in_memory());
		history.current().run(false, |s| s.put_contact(&friend("a="))).await.unwrap();
		let contacts = Contacts::new(events, history.clone());
		let mut revision = contacts.watch();
		contacts.load();
		let loaded = loop {
			if let Event::ContactsChanged { contacts } = rx.recv().await.unwrap() {
				break contacts;
			}
		};
		assert_eq!(loaded.len(), 1);
		assert!(revision.has_changed().unwrap());
		revision.mark_unchanged();
		contacts.remove("a=");
		assert!(revision.has_changed().unwrap());
		assert!(contacts.list().is_empty());
		assert!(history.current().run(false, |s| s.contacts()).await.unwrap().is_empty());
	}
}
