//! Directory of running streams.
//!
//! TeamSpeak 6 does not tell clients that join later about streams that
//! are already running. Voelin streamers register theirs here; the gateway
//! drops entries when the streamer's client leaves or stops streaming
//! (`client_is_streaming`, when the server reports it), and lists clients
//! that stream without a registration as `detected` (no stream id).

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use serde_json::json;
use tracing::{debug, warn};
use voelin_gateway_proto::{
	Action, ErrorCode, StreamEntry, StreamSource, StreamSpec, UserRef, activity_kind,
};
use voelin_model::{ClientId, ClientInfo, Presence};

use crate::db::{Db, NewActivity};
use crate::hub::{Denied, Feature, Hub, HubEvent, User, now_ms};

/// Presence may lag behind a registration (query sessions only see
/// `client_is_streaming` when they poll); give it this long.
pub const GRACE: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Directory {
	entries: BTreeMap<String, StreamEntry>,
	/// When registered entries were added or confirmed by presence.
	seen: HashMap<String, Instant>,
}

impl Directory {
	/// The entries stored before a restart; checked against presence once
	/// the observer has a snapshot.
	pub fn load(db: &Db) -> Self {
		let entries = db.streams().unwrap_or_else(|error| {
			warn!(%error, "could not read the stream directory");
			Vec::new()
		});
		let now = Instant::now();
		Self {
			seen: entries.iter().map(|e| (e.id.clone(), now)).collect(),
			entries: entries.into_iter().map(|e| (e.id.clone(), e)).collect(),
		}
	}

	pub fn list(&self) -> Vec<StreamEntry> {
		self.entries.values().cloned().collect()
	}

	fn by_client(&self, client: ClientId) -> Vec<&StreamEntry> {
		self.entries.values().filter(|e| e.client_id == Some(client)).collect()
	}
}

fn detected_id(client: ClientId) -> String {
	format!("client/{client}")
}

/// What changed after comparing the directory with presence.
enum Change {
	Start(StreamEntry),
	Update(StreamEntry),
	End(String, &'static str),
}

impl Hub {
	pub fn streams_for(&self, user: &User) -> Vec<StreamEntry> {
		let list = self.directory.lock().unwrap().list();
		list.into_iter()
			.filter(|e| e.channel.is_none_or(|c| self.channel_visible(user, c)))
			.collect()
	}

	pub async fn register_stream(
		&self,
		user: &User,
		spec: StreamSpec,
	) -> Result<StreamEntry, Denied> {
		self.require(Feature::Streams)?;
		self.require_allowed(user, Action::Stream, spec.channel).await?;
		if spec.stream_id.is_empty() || spec.stream_id.starts_with("client/") {
			return Err(Denied::bad("invalid stream id"));
		}
		let mut channel = spec.channel;
		if let Some(clid) = spec.client_id {
			let presence = self.observer.presence();
			let p = presence.read().unwrap();
			if let Some(client) = p.clients.get(&clid) {
				if client.uid.as_deref() != Some(user.uid.as_str()) {
					return Err(Denied::forbidden("that client is someone else"));
				}
				channel = channel.or(Some(client.channel));
			}
		}
		let quota = self.runtime().quota.streams_per_user;
		{
			let dir = self.directory.lock().unwrap();
			if let Some(existing) = dir.entries.get(&spec.stream_id)
				&& existing.streamer.uid != user.uid
			{
				return Err(Denied::forbidden("someone else registered this stream"));
			}
			let mine = dir
				.entries
				.values()
				.filter(|e| e.streamer.uid == user.uid && e.id != spec.stream_id)
				.count() as u64;
			if quota > 0 && mine >= quota {
				return Err(Denied::quota(format!("at most {quota} streams per user")));
			}
		}
		let entry = StreamEntry {
			id: spec.stream_id.clone(),
			stream_id: Some(spec.stream_id),
			streamer: user.user_ref(),
			client_id: spec.client_id,
			channel,
			title: spec.title,
			kind: spec.kind,
			started_ms: now_ms(),
			viewers: spec.viewers,
			source: StreamSource::Registered,
			event_id: None,
		};
		// A detected entry for the same client is replaced.
		if let Some(clid) = spec.client_id {
			self.end_stream(&detected_id(clid), "registered");
		}
		let entry = self.start_stream(entry);
		self.audit(Some(&user.uid), "stream", &entry.id);
		Ok(entry)
	}

	fn start_stream(&self, mut entry: StreamEntry) -> StreamEntry {
		let rt = self.runtime();
		let window = rt.events.stream_link_minutes as i64 * 60_000;
		let now = now_ms();
		if let Ok(events) = self.db.linkable_events(&entry.streamer.uid, now, window) {
			for id in events {
				if let Err(error) = self.db.set_live_stream(id, Some(&entry.id), now) {
					warn!(%error, "could not link stream to event");
					continue;
				}
				entry.event_id = entry.event_id.or(Some(id));
				if let Ok(Some(event)) = self.db.event(id, None, false) {
					self.emit(HubEvent::Event(event));
				}
			}
		}
		let replaced = {
			let mut dir = self.directory.lock().unwrap();
			dir.seen.insert(entry.id.clone(), Instant::now());
			dir.entries.insert(entry.id.clone(), entry.clone()).is_some()
		};
		if let Err(error) = self.db.save_stream(&entry) {
			warn!(%error, "could not store stream");
		}
		if replaced {
			self.emit(HubEvent::StreamUpdated(entry.clone()));
		} else {
			self.emit(HubEvent::StreamStarted(entry.clone()));
			let title =
				if entry.title.is_empty() { String::new() } else { format!(": {}", entry.title) };
			self.add_activity(NewActivity {
				kind: activity_kind::STREAM_STARTED,
				actor: Some(&entry.streamer),
				channel: entry.channel,
				ref_id: Some(entry.id.clone()),
				text: format!("{} started streaming{title}", entry.streamer.name),
				data: json!({ "title": entry.title, "kind": entry.kind, "event_id": entry.event_id,
					"source": entry.source }),
			});
		}
		entry
	}

	pub async fn update_stream(
		&self,
		user: &User,
		id: &str,
		title: Option<String>,
		viewers: Option<u32>,
	) -> Result<StreamEntry, Denied> {
		self.require(Feature::Streams)?;
		let entry = self.own_stream(user, id).await?;
		let entry = StreamEntry {
			title: title.unwrap_or(entry.title),
			viewers: viewers.or(entry.viewers),
			..entry
		};
		self.directory.lock().unwrap().entries.insert(entry.id.clone(), entry.clone());
		if let Err(error) = self.db.save_stream(&entry) {
			warn!(%error, "could not store stream");
		}
		self.emit(HubEvent::StreamUpdated(entry.clone()));
		Ok(entry)
	}

	pub async fn unregister_stream(&self, user: &User, id: &str) -> Result<(), Denied> {
		self.require(Feature::Streams)?;
		self.own_stream(user, id).await?;
		self.end_stream(id, "stopped");
		Ok(())
	}

	/// An entry the user registered, or may moderate.
	async fn own_stream(&self, user: &User, id: &str) -> Result<StreamEntry, Denied> {
		let entry = self.directory.lock().unwrap().entries.get(id).cloned();
		let entry = entry.ok_or_else(|| Denied::not_found("stream"))?;
		if entry.streamer.uid != user.uid
			&& !self.allowed(user, Action::Moderate, entry.channel).await?
		{
			return Err(Denied::new(ErrorCode::Forbidden, "not your stream"));
		}
		Ok(entry)
	}

	/// Remove an entry (no-op if it is not there).
	pub fn end_stream(&self, id: &str, reason: &str) {
		let entry = {
			let mut dir = self.directory.lock().unwrap();
			dir.seen.remove(id);
			dir.entries.remove(id)
		};
		let Some(entry) = entry else { return };
		debug!(id, reason, "stream ended");
		if let Err(error) = self.db.delete_stream(id) {
			warn!(%error, "could not delete stream");
		}
		let now = now_ms();
		for event in self.db.events_with_stream(id).unwrap_or_default() {
			let _ = self.db.set_live_stream(event, None, now);
			if let Ok(Some(event)) = self.db.event(event, None, false) {
				self.emit(HubEvent::Event(event));
			}
		}
		self.emit(HubEvent::StreamEnded {
			id: id.to_string(),
			channel: entry.channel,
			reason: reason.to_string(),
		});
		self.add_activity(NewActivity {
			kind: activity_kind::STREAM_ENDED,
			actor: Some(&entry.streamer),
			channel: entry.channel,
			ref_id: Some(entry.id.clone()),
			text: format!("{} stopped streaming", entry.streamer.name),
			data: json!({ "reason": reason, "duration_ms": now - entry.started_ms }),
		});
	}

	/// Bring the directory in line with presence: drop entries of clients
	/// that left or stopped streaming, follow moves, list unregistered
	/// streamers. `full` means `presence` is complete (a snapshot or resync).
	pub(crate) fn reconcile_streams(&self, presence: &Presence, full: bool) {
		if !self.feature_enabled(Feature::Streams) || presence.clients.is_empty() {
			return;
		}
		let changes = {
			let mut dir = self.directory.lock().unwrap();
			let mut changes = Vec::new();
			let mut confirmed = Vec::new();
			for entry in dir.entries.values() {
				let Some(clid) = entry.client_id else { continue };
				let young = dir.seen.get(&entry.id).is_some_and(|t| t.elapsed() < GRACE);
				match presence.clients.get(&clid) {
					None if full && !young => changes.push(Change::End(entry.id.clone(), "left")),
					None => {}
					Some(c) if c.streaming == Some(false) && !young => {
						changes.push(Change::End(entry.id.clone(), "stopped streaming"));
					}
					Some(c) => {
						if c.streaming == Some(true) {
							// Presence confirms it: the grace starts again for later lags.
							confirmed.push(entry.id.clone());
						}
						if Some(c.channel) != entry.channel {
							let moved = StreamEntry { channel: Some(c.channel), ..entry.clone() };
							changes.push(Change::Update(moved));
						}
					}
				}
			}
			for id in confirmed {
				dir.seen.insert(id, Instant::now());
			}
			let unregistered = presence.clients.values().filter(|c| {
				c.streaming == Some(true) && !c.is_query && dir.by_client(c.id).is_empty()
			});
			changes.extend(unregistered.map(|c| Change::Start(detected(c))));
			changes
		};
		for change in changes {
			match change {
				Change::Start(entry) => {
					self.start_stream(entry);
				}
				Change::Update(entry) => {
					self.directory.lock().unwrap().entries.insert(entry.id.clone(), entry.clone());
					let _ = self.db.save_stream(&entry);
					self.emit(HubEvent::StreamUpdated(entry));
				}
				Change::End(id, reason) => self.end_stream(&id, reason),
			}
		}
	}

	/// A client left: its streams end at once.
	pub(crate) fn client_left(&self, client: ClientId) {
		let ids: Vec<String> =
			self.directory.lock().unwrap().by_client(client).iter().map(|e| e.id.clone()).collect();
		for id in ids {
			self.end_stream(&id, "left");
		}
	}

	/// Entries without a client id end with the session that registered them.
	pub(crate) fn session_streams_closed(&self, ids: &[String]) {
		for id in ids {
			let orphan = self
				.directory
				.lock()
				.unwrap()
				.entries
				.get(id)
				.is_some_and(|e| e.client_id.is_none());
			if orphan {
				self.end_stream(id, "streamer disconnected");
			}
		}
	}
}

fn detected(client: &ClientInfo) -> StreamEntry {
	StreamEntry {
		id: detected_id(client.id),
		stream_id: None,
		streamer: UserRef {
			uid: client.uid.clone().unwrap_or_default(),
			name: client.nickname.clone(),
		},
		client_id: Some(client.id),
		channel: Some(client.channel),
		title: String::new(),
		kind: String::new(),
		started_ms: now_ms(),
		viewers: None,
		source: StreamSource::Detected,
		event_id: None,
	}
}
