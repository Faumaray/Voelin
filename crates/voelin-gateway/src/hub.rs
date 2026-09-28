//! State shared by all sessions: one observer, one relay pool, one query
//! session for lookups, storage, runtime settings and fan-out of events.
//!
//! The chat features (pins, reactions, topics), events and the activity
//! feed are in `features.rs`, the stream directory in `streams.rs`, the
//! background tasks in `tasks.rs`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use anyhow::{Context, Result};
use base64::prelude::*;
use rand::RngExt;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tracing::{info, warn};
use voelin_gateway_proto::{
	Action, ActivityEntry, ErrorCode, EventInfo, HistoryEntry, PinInfo, StreamEntry, TopicInfo,
	UniqueIds, UserRef, feature, identity_level, verify_auth,
};
use voelin_model::{ChannelId, ChatMessage, ChatTarget, Presence, ServerFlavor, relay_text};
use voelin_observer::{Observer, ObserverConfig, RelayConfig, RelayPool};
use voelin_query::{Command, Connect, QueryClient};

use crate::config::{Bootstrap, Layers, Runtime};
use crate::db::{Db, TokenInfo};
use crate::perms::{
	ChannelAccess, GroupResolver, PermIds, PermResolver, UserGroups, channel_access, effective,
	granted, rule_allows,
};
use crate::settings::Settings;
use crate::streams::Directory;

pub fn now_secs() -> i64 {
	now_ms() / 1000
}

pub fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or_default()
}

/// A logged-in user.
#[derive(Clone, Debug)]
pub struct User {
	pub uid: String,
	pub cldbid: u64,
	/// Last nickname the server saw; used in relayed posts (not user-chosen,
	/// so nobody can post under someone else's name).
	pub nickname: String,
	/// Effective `i_channel_subscribe_power`, used to filter presence.
	pub subscribe_power: i64,
}

impl User {
	pub fn user_ref(&self) -> UserRef {
		UserRef { uid: self.uid.clone(), name: self.nickname.clone() }
	}
}

/// An error to send to the client.
#[derive(Debug)]
pub struct Denied(pub ErrorCode, pub String);

impl Denied {
	pub fn new(code: ErrorCode, msg: impl Into<String>) -> Self {
		Self(code, msg.into())
	}

	pub fn forbidden(msg: impl Into<String>) -> Self {
		Self::new(ErrorCode::Forbidden, msg)
	}

	pub fn not_found(what: &str) -> Self {
		Self::new(ErrorCode::NotFound, format!("no such {what}"))
	}

	pub fn bad(msg: impl Into<String>) -> Self {
		Self::new(ErrorCode::BadRequest, msg)
	}

	pub fn quota(msg: impl Into<String>) -> Self {
		Self::new(ErrorCode::QuotaExceeded, msg)
	}
}

impl From<voelin_query::Error> for Denied {
	fn from(e: voelin_query::Error) -> Self {
		Denied::new(ErrorCode::Unavailable, e.to_string())
	}
}

impl From<rusqlite::Error> for Denied {
	fn from(e: rusqlite::Error) -> Self {
		warn!(error = %e, "database error");
		Denied::new(ErrorCode::Internal, "database error")
	}
}

/// Something sessions may want to forward; each session filters by what it
/// opened, subscribed to and may see.
#[derive(Clone, Debug)]
pub enum HubEvent {
	Chat(HistoryEntry),
	Pinned(PinInfo),
	Unpinned {
		target: ChatTarget,
		message_id: i64,
		by: UserRef,
	},
	Reaction {
		target: ChatTarget,
		message_id: i64,
		emoji: String,
		user: UserRef,
		added: bool,
		count: u32,
	},
	Topic(TopicInfo),
	Event(EventInfo),
	EventDeleted {
		id: i64,
		channel: Option<ChannelId>,
	},
	Reminder {
		event: EventInfo,
		starts_in_ms: i64,
	},
	StreamStarted(StreamEntry),
	StreamUpdated(StreamEntry),
	StreamEnded {
		id: String,
		channel: Option<ChannelId>,
		reason: String,
	},
	Activity(ActivityEntry),
	/// Features changed; sessions recompute their capabilities.
	Capabilities,
}

/// Optional features, for [`Hub::feature_enabled`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
	History,
	Pins,
	Reactions,
	Topics,
	Events,
	Streams,
	Activity,
}

impl Feature {
	pub fn name(self) -> &'static str {
		match self {
			Feature::History => feature::HISTORY,
			Feature::Pins => feature::PINS,
			Feature::Reactions => feature::REACTIONS,
			Feature::Topics => feature::TOPICS,
			Feature::Events => feature::EVENTS,
			Feature::Streams => feature::STREAMS,
			Feature::Activity => feature::ACTIVITY,
		}
	}

	pub fn enabled(self, rt: &Runtime) -> bool {
		let f = &rt.features;
		match self {
			Feature::History => rt.history.enabled,
			// These live on stored messages.
			Feature::Pins => rt.history.enabled && f.pins,
			Feature::Reactions => rt.history.enabled && f.reactions,
			Feature::Topics => rt.history.enabled && f.topics,
			Feature::Events => f.events,
			Feature::Streams => f.streams,
			Feature::Activity => f.activity,
		}
	}

	pub const ALL: [Feature; 7] = [
		Feature::History,
		Feature::Pins,
		Feature::Reactions,
		Feature::Topics,
		Feature::Events,
		Feature::Streams,
		Feature::Activity,
	];
}

pub struct Hub {
	pub boot: Bootstrap,
	pub settings: Settings,
	pub gateway_id: String,
	pub server_uid: String,
	pub server_name: String,
	pub is_ts6: bool,
	needed_level: u8,
	default_channel: ChannelId,
	pub observer: Observer,
	connect: Connect,
	relays: RwLock<RelayPool>,
	/// Serializes rebuilding the relay pool.
	relay_rebuild: tokio::sync::Mutex<()>,
	lookup: QueryClient,
	perms: PermResolver,
	groups: GroupResolver,
	pub db: Db,
	events: broadcast::Sender<Arc<HubEvent>>,
	/// Readers per relayed channel, and when the last one left.
	readers: Mutex<HashMap<ChannelId, (usize, Instant)>>,
	/// Our own query clients, whose messages are echoes.
	own_clients: Mutex<HashSet<u16>>,
	/// Message ids while history is off.
	next_message_id: Mutex<i64>,
	pub(crate) directory: Mutex<Directory>,
}

impl Hub {
	/// Connect to the server, open the database and start the background tasks.
	pub async fn start(layers: Layers) -> Result<Arc<Self>> {
		let boot = layers.bootstrap()?;
		let db = Db::open(&boot.db_path)
			.with_context(|| format!("failed to open {}", boot.db_path.display()))?;
		let settings = Settings::new(layers, &db)?;
		let rt = settings.current();

		let connect = boot.query_connect();
		let (lookup, _) =
			QueryClient::connect(&connect).await.context("query login for lookups failed")?;
		let info = lookup.send(&Command::new("serverinfo")).await?;
		let info = info.first().context("empty serverinfo")?;
		let version = info.get("virtualserver_version").unwrap_or_default();
		let is_ts6 = matches!(ServerFlavor::from_version_string(version), ServerFlavor::Ts6(_));
		let server_uid =
			info.get("virtualserver_unique_identifier").unwrap_or_default().to_string();
		let server_name = info.get("virtualserver_name").unwrap_or_default().to_string();
		let needed_level = info.parse("virtualserver_needed_identity_security_level").unwrap_or(8);
		let channels = lookup.send(&Command::new("channellist").flag("flags")).await?;
		let default_channel = channels
			.iter()
			.find(|r| r.flag("channel_flag_default") == Some(true))
			.and_then(|r| r.parse("cid"))
			.unwrap_or(1);
		let perms = PermResolver::new(PermIds::load(&lookup).await?);
		let own_id = lookup.own_client_id().await?;
		set_lookup_nickname(&lookup, &rt.relay.nickname).await;

		let observer = Observer::spawn(ObserverConfig::new(connect.clone()));
		let relays = new_relay_pool(&connect, &rt.relay.nickname);
		let directory = Directory::load(&db);

		info!(%server_name, %server_uid, version, is_ts6, "gateway connected to server");
		let hub = Arc::new(Self {
			gateway_id: boot.gateway_id(),
			boot,
			settings,
			server_uid,
			server_name,
			is_ts6,
			needed_level,
			default_channel,
			observer,
			connect,
			relays: RwLock::new(relays),
			relay_rebuild: Default::default(),
			lookup,
			perms,
			groups: GroupResolver::default(),
			db,
			events: broadcast::channel(4096).0,
			readers: Default::default(),
			own_clients: Mutex::new([own_id].into()),
			next_message_id: Mutex::new(0),
			directory: Mutex::new(directory),
		});
		crate::tasks::spawn_all(&hub);
		for &cid in &rt.relay.pinned_channels {
			if let Err(error) = hub.relays().open(cid).await {
				warn!(cid, %error, "could not open pinned relay");
			}
		}
		Ok(hub)
	}

	pub fn runtime(&self) -> Arc<Runtime> {
		self.settings.current()
	}

	pub fn subscribe(&self) -> broadcast::Receiver<Arc<HubEvent>> {
		self.events.subscribe()
	}

	pub fn emit(&self, event: HubEvent) {
		let _ = self.events.send(Arc::new(event));
	}

	pub fn relays(&self) -> RelayPool {
		self.relays.read().unwrap().clone()
	}

	pub(crate) fn is_own_client(&self, id: u16) -> bool {
		self.own_clients.lock().unwrap().contains(&id)
	}

	/// Features for the `hello` message.
	pub fn capabilities(&self) -> Vec<String> {
		let rt = self.runtime();
		let mut caps = vec![feature::PRESENCE.to_string(), feature::RELAY.to_string()];
		caps.extend(
			Feature::ALL.into_iter().filter(|f| f.enabled(&rt)).map(|f| f.name().to_string()),
		);
		caps
	}

	/// Features for one user: [`Hub::capabilities`] plus `admin`.
	pub async fn user_capabilities(&self, user: &User) -> Vec<String> {
		let mut caps = self.capabilities();
		if self.is_admin(user).await.unwrap_or(false) {
			caps.push(feature::ADMIN.to_string());
		}
		caps
	}

	pub fn feature_enabled(&self, feature: Feature) -> bool {
		feature.enabled(&self.runtime())
	}

	/// Fail with `feature_disabled` unless the feature is on.
	pub fn require(&self, feature: Feature) -> Result<(), Denied> {
		if self.feature_enabled(feature) {
			Ok(())
		} else {
			Err(Denied::new(
				ErrorCode::FeatureDisabled,
				format!("{} is turned off on this gateway", feature.name()),
			))
		}
	}

	/// Rebuild the relay pool, e.g. for a new nickname; open relays move over.
	pub(crate) async fn rebuild_relays(self: &Arc<Self>, nickname: &str) {
		let _guard = self.relay_rebuild.lock().await;
		let pool = new_relay_pool(&self.connect, nickname);
		tokio::spawn(crate::tasks::pump_relays(self.clone(), pool.subscribe()));
		let old = std::mem::replace(&mut *self.relays.write().unwrap(), pool);
		let channels = old.channels().await;
		old.close_all().await;
		drop(old);
		set_lookup_nickname(&self.lookup, nickname).await;
		let pool = self.relays();
		for cid in channels {
			if let Err(error) = pool.open(cid).await {
				warn!(cid, %error, "could not reopen relay");
			}
		}
		info!(nickname, "relays renamed");
	}

	/// Pinned channels changed: open new ones, let removed ones idle out.
	pub(crate) async fn apply_pinned_channels(&self, old: &[u64], new: &[u64]) {
		let pool = self.relays();
		for cid in new.iter().filter(|c| !old.contains(c)) {
			if let Err(error) = pool.open(*cid).await {
				warn!(cid, %error, "could not open pinned relay");
			}
		}
		for cid in old.iter().filter(|c| !new.contains(c)) {
			// The idle teardown closes it once nobody reads it.
			self.readers.lock().unwrap().entry(*cid).or_insert((0, Instant::now()));
		}
	}

	/// Channels whose relay nobody needs any more.
	pub(crate) fn idle_relays(&self, rt: &Runtime) -> Vec<ChannelId> {
		let idle = std::time::Duration::from_secs(rt.relay.idle_teardown_secs);
		let mut readers = self.readers.lock().unwrap();
		let channels: Vec<ChannelId> = readers
			.iter()
			.filter(|(cid, (n, since))| {
				*n == 0 && since.elapsed() >= idle && !rt.relay.pinned_channels.contains(cid)
			})
			.map(|(cid, _)| *cid)
			.collect();
		for cid in &channels {
			readers.remove(cid);
		}
		channels
	}

	// Authentication

	/// Verify a signed challenge and look the identity up on the server.
	pub async fn authenticate(
		&self,
		omega: &str,
		key_offset: u64,
		ts: i64,
		signature: &str,
		nonce: &str,
	) -> Result<(User, String, i64), Denied> {
		let ids: UniqueIds = verify_auth(
			omega,
			signature,
			&self.gateway_id,
			&self.server_uid,
			nonce,
			ts,
			now_secs(),
		)
		.map_err(|e| Denied::new(ErrorCode::AuthFailed, e.to_string()))?;
		let uid = ids.for_server(self.is_ts6).to_string();
		let needed = self.needed_level.max(self.runtime().auth.min_security_level);
		let level = identity_level(omega, key_offset);
		if level < needed {
			return Err(Denied::new(
				ErrorCode::LevelTooLow,
				format!("identity level {level}, server needs {needed}"),
			));
		}
		let user = self.lookup_user(&uid).await?;
		let (token, expires) = self.issue_token(&user)?;
		self.audit(Some(&user.uid), "login", "signature");
		Ok((user, token, expires))
	}

	/// Log in with a token from an earlier login.
	pub async fn resume(&self, token: &str) -> Result<(User, String, i64), Denied> {
		let info = self
			.db
			.token(&hash_token(token), now_secs())?
			.ok_or_else(|| Denied::new(ErrorCode::AuthFailed, "unknown or expired token"))?;
		let user = self.lookup_user(&info.uid).await?;
		self.audit(Some(&user.uid), "login", "token");
		Ok((user, token.to_string(), info.expires))
	}

	fn issue_token(&self, user: &User) -> Result<(String, i64), Denied> {
		let bytes: [u8; 32] = rand::rng().random();
		let token = BASE64_URL_SAFE_NO_PAD.encode(bytes);
		let expires = now_secs() + self.runtime().auth.token_ttl_hours as i64 * 3600;
		self.db.add_token(
			&hash_token(&token),
			&TokenInfo { uid: user.uid.clone(), cldbid: user.cldbid, expires },
		)?;
		Ok((token, expires))
	}

	async fn lookup_user(&self, uid: &str) -> Result<User, Denied> {
		let rt = self.runtime();
		if rt.auth.deny_uids.iter().any(|d| d == uid) {
			return Err(Denied::forbidden("not allowed on this gateway"));
		}
		let bans = self.lookup.send(&Command::new("banlist")).await.unwrap_or_default();
		if bans.iter().any(|b| b.get("uid") == Some(uid)) {
			return Err(Denied::new(ErrorCode::Banned, "banned on this server"));
		}
		let found =
			self.lookup.send(&Command::new("clientdbfind").arg("pattern", uid).flag("uid")).await?;
		let cldbid: u64 = found.first().and_then(|r| r.parse("cldbid")).ok_or_else(|| {
			Denied::new(ErrorCode::UnknownIdentity, "connect to the server with voice once first")
		})?;
		let info = self.lookup.send(&Command::new("clientdbinfo").arg("cldbid", cldbid)).await?;
		let nickname =
			info.first().and_then(|r| r.get("client_nickname")).unwrap_or("unknown").to_string();
		if !rt.auth.require_server_groups.is_empty() {
			let groups = self.groups.server_groups(&self.lookup, cldbid).await?;
			if !groups.iter().any(|g| rt.auth.require_server_groups.contains(g)) {
				return Err(Denied::forbidden("missing required server group"));
			}
		}
		let entries = self.perms.entries(&self.lookup, cldbid, self.default_channel).await?;
		let subscribe_power = effective(&entries, self.perms.ids.subscribe_power);
		Ok(User { uid: uid.to_string(), cldbid, nickname, subscribe_power })
	}

	// Authorization

	pub async fn channel_access(
		&self,
		user: &User,
		cid: ChannelId,
	) -> Result<ChannelAccess, Denied> {
		let has_password = {
			let presence = self.observer.presence();
			let presence = presence.read().unwrap();
			let Some(channel) = presence.channels.get(&cid) else {
				return Err(Denied::bad(format!("no channel {cid}")));
			};
			if channel.needed_subscribe_power as i64 > user.subscribe_power {
				return Ok(ChannelAccess { read: false, post: false });
			}
			channel.has_password
		};
		let entries = self.perms.entries(&self.lookup, user.cldbid, cid).await?;
		Ok(channel_access(&entries, &self.perms.ids, has_password))
	}

	pub async fn can_post_server_chat(&self, user: &User) -> Result<bool, Denied> {
		let entries = self.perms.entries(&self.lookup, user.cldbid, self.default_channel).await?;
		Ok(effective(&entries, self.perms.ids.server_text_send) > 0)
	}

	/// Whether the user may see a channel at all (presence filter).
	pub fn channel_visible(&self, user: &User, cid: ChannelId) -> bool {
		let presence = self.observer.presence();
		let p = presence.read().unwrap();
		p.channels
			.get(&cid)
			.is_some_and(|c| c.needed_subscribe_power as i64 <= user.subscribe_power)
	}

	/// The user may read `target`.
	pub async fn require_read(&self, user: &User, target: &ChatTarget) -> Result<(), Denied> {
		match target {
			ChatTarget::Server => Ok(()),
			ChatTarget::Channel(cid) => {
				if self.channel_access(user, *cid).await?.read {
					Ok(())
				} else {
					Err(Denied::forbidden("no access to this channel"))
				}
			}
			ChatTarget::Private(_) => Err(Denied::bad("private chat is not relayed")),
		}
	}

	/// The user may write in `target`.
	pub async fn require_post(&self, user: &User, target: &ChatTarget) -> Result<(), Denied> {
		let allowed = match target {
			ChatTarget::Server => self.can_post_server_chat(user).await?,
			ChatTarget::Channel(cid) => self.channel_access(user, *cid).await?.post,
			ChatTarget::Private(_) => return Err(Denied::bad("private chat is not relayed")),
		};
		if allowed { Ok(()) } else { Err(Denied::forbidden("no permission to post here")) }
	}

	async fn user_groups(
		&self,
		user: &User,
		channel: Option<ChannelId>,
	) -> Result<UserGroups, Denied> {
		let online = {
			let presence = self.observer.presence();
			let p = presence.read().unwrap();
			p.clients
				.values()
				.find(|c| {
					c.uid.as_deref() == Some(user.uid.as_str()) && !c.server_groups.is_empty()
				})
				.map(|c| c.server_groups.clone())
		};
		let server = match online {
			Some(groups) => groups,
			None => self.groups.server_groups(&self.lookup, user.cldbid).await?,
		};
		let channel = match channel {
			Some(cid) => self.groups.channel_groups(&self.lookup, user.cldbid, cid).await?,
			None => Vec::new(),
		};
		Ok(UserGroups { server, channel })
	}

	async fn rule_allows(
		&self,
		user: &User,
		action: Action,
		channel: Option<ChannelId>,
	) -> Result<Option<bool>, Denied> {
		let rt = self.runtime();
		let Some(rule) = rt.perm.get(action) else { return Ok(None) };
		let needs_channel = !rule.channel_groups.is_empty() && !rule.everyone;
		let groups = self.user_groups(user, channel.filter(|_| needs_channel)).await?;
		Ok(Some(rule_allows(rule, &groups)))
	}

	pub async fn is_admin(&self, user: &User) -> Result<bool, Denied> {
		if let Some(allowed) = self.rule_allows(user, Action::Admin, None).await? {
			return Ok(allowed);
		}
		let entries = self.perms.entries(&self.lookup, user.cldbid, self.default_channel).await?;
		Ok(granted(&entries, self.perms.ids.server_modify_name))
	}

	/// Whether a rule (or the action's default) lets the user act, server-wide
	/// or in `channel`. Access to the chat itself is checked separately.
	pub async fn allowed(
		&self,
		user: &User,
		action: Action,
		channel: Option<ChannelId>,
	) -> Result<bool, Denied> {
		if action == Action::Unknown {
			return Ok(false);
		}
		if self.is_admin(user).await? {
			return Ok(true);
		}
		if action == Action::Admin {
			return Ok(false);
		}
		if action == Action::Pin && Box::pin(self.allowed(user, Action::Moderate, channel)).await? {
			return Ok(true);
		}
		if let Some(allowed) = self.rule_allows(user, action, channel).await? {
			return Ok(allowed);
		}
		Ok(match action {
			Action::React | Action::CreateTopic | Action::Rsvp | Action::Stream => true,
			Action::CreateEvent => match channel {
				Some(cid) => self.channel_access(user, cid).await?.post,
				None => self.can_post_server_chat(user).await?,
			},
			Action::Moderate => match channel {
				Some(cid) => {
					let entries = self.perms.entries(&self.lookup, user.cldbid, cid).await?;
					granted(&entries, self.perms.ids.channel_modify_name)
				}
				None => false,
			},
			// Moderators, handled above.
			Action::Pin => false,
			Action::Admin | Action::Unknown => false,
		})
	}

	pub async fn require_allowed(
		&self,
		user: &User,
		action: Action,
		channel: Option<ChannelId>,
	) -> Result<(), Denied> {
		if self.allowed(user, action, channel).await? {
			Ok(())
		} else {
			Err(Denied::forbidden(format!("not allowed to {}", action.as_str().replace('_', " "))))
		}
	}

	/// Presence as this user may see it: channels within their subscribe
	/// power, no query clients.
	pub fn filtered_presence(&self, user: &User) -> Presence {
		let presence = self.observer.presence();
		let p = presence.read().unwrap();
		let channels: std::collections::BTreeMap<_, _> = p
			.channels
			.iter()
			.filter(|(_, c)| c.needed_subscribe_power as i64 <= user.subscribe_power)
			.map(|(id, c)| (*id, c.clone()))
			.collect();
		let clients = p
			.clients
			.iter()
			.filter(|(_, c)| !c.is_query && channels.contains_key(&c.channel))
			.map(|(id, c)| (*id, c.clone()))
			.collect();
		Presence { server_name: p.server_name.clone(), channels, clients, ..Presence::default() }
	}

	// Chat

	/// Store (if history is on) and fan out a message.
	pub fn publish(&self, msg: ChatMessage, topic_id: Option<i64>) -> HistoryEntry {
		let entry = if self.runtime().history.enabled {
			self.db.add_message(msg.clone(), topic_id).unwrap_or_else(|error| {
				warn!(%error, "failed to store message");
				HistoryEntry { topic_id, ..HistoryEntry::new(-1, msg) }
			})
		} else {
			let mut next = self.next_message_id.lock().unwrap();
			*next += 1;
			HistoryEntry { topic_id, ..HistoryEntry::new(*next, msg) }
		};
		if let Some(topic) = topic_id.and_then(|t| self.db.topic(t).ok().flatten()) {
			self.emit(HubEvent::Topic(topic));
		}
		self.emit(HubEvent::Chat(entry.clone()));
		entry
	}

	/// A message seen in TeamSpeak. Posts in the topic format (from official
	/// clients) join that topic.
	pub(crate) fn publish_incoming(&self, mut msg: ChatMessage) {
		let mut topic_id = None;
		if self.feature_enabled(Feature::Topics)
			&& let Some((title, text)) =
				parse_topic_post(&self.runtime().topics.relay_format, &msg.text)
			&& let Ok(Some(topic)) = self.db.topic_by_title(&msg.target, title)
		{
			topic_id = Some(topic.id);
			msg.text = text.to_string();
		}
		self.publish(msg, topic_id);
	}

	/// Start reading a channel for one more session.
	pub async fn add_reader(&self, cid: ChannelId) -> Result<(), Denied> {
		let max = self.runtime().relay.max_channel_relays as usize;
		{
			let readers = self.readers.lock().unwrap();
			if max > 0 && !readers.contains_key(&cid) && readers.len() >= max {
				return Err(Denied::new(
					ErrorCode::Unavailable,
					"too many relayed channels, try later",
				));
			}
		}
		self.relays().open(cid).await?;
		self.readers.lock().unwrap().entry(cid).or_insert((0, Instant::now())).0 += 1;
		Ok(())
	}

	pub fn remove_reader(&self, cid: ChannelId) {
		if let Some(entry) = self.readers.lock().unwrap().get_mut(&cid) {
			entry.0 = entry.0.saturating_sub(1);
			entry.1 = Instant::now();
		}
	}

	/// Post on behalf of `user`, optionally into a topic; the message is
	/// published to readers directly (the relay's echo is dropped).
	pub async fn post(
		&self,
		user: &User,
		target: &ChatTarget,
		text: &str,
		topic: Option<i64>,
	) -> Result<HistoryEntry, Denied> {
		let rt = self.runtime();
		if rt.quota.message_bytes > 0 && text.len() as u64 > rt.quota.message_bytes {
			return Err(Denied::quota(format!(
				"messages are limited to {} bytes",
				rt.quota.message_bytes
			)));
		}
		self.require_post(user, target).await?;
		let relayed = match topic {
			Some(id) => {
				self.require(Feature::Topics)?;
				let topic = self.db.topic(id)?.ok_or_else(|| Denied::not_found("topic"))?;
				if topic.target != *target {
					return Err(Denied::bad("the topic belongs to another chat"));
				}
				if topic.archived {
					return Err(Denied::forbidden("the topic is archived"));
				}
				rt.topics.relay_format.replace("{topic}", &topic.title).replace("{text}", text)
			}
			None => text.to_string(),
		};
		let full = relay_text(&rt.relay.format, &user.nickname, &relayed);
		match target {
			ChatTarget::Server => {
				for part in voelin_model::split_message(&full, 1024) {
					self.lookup
						.send(
							&Command::new("sendtextmessage").arg("targetmode", 3).arg("msg", part),
						)
						.await?;
				}
			}
			ChatTarget::Channel(cid) => {
				if !self.readers.lock().unwrap().contains_key(cid) {
					// Posting without reading still needs a relay in the channel.
					self.add_reader(*cid).await?;
					self.remove_reader(*cid);
				}
				// The pool's format is `{text}`: `full` is already formatted.
				self.relays().send(*cid, &user.nickname, &full).await?;
			}
			ChatTarget::Private(_) => unreachable!("checked by require_post"),
		}
		self.audit(Some(&user.uid), "post", &format!("{target:?}"));
		Ok(self.publish(
			ChatMessage {
				target: target.clone(),
				author_name: user.nickname.clone(),
				author_uid: Some(user.uid.clone()),
				author_id: None,
				text: text.to_string(),
				ts_ms: now_ms(),
				via_relay: true,
				blocked: false,
			},
			topic,
		))
	}

	pub fn audit(&self, uid: Option<&str>, action: &str, detail: &str) {
		self.db.audit(now_secs(), uid, action, detail);
	}
}

fn new_relay_pool(connect: &Connect, nickname: &str) -> RelayPool {
	let mut config = RelayConfig::new(connect.clone());
	config.nickname = nickname.to_string();
	// Posts are formatted by the hub, with the current settings.
	config.format = "{text}".into();
	RelayPool::new(config)
}

async fn set_lookup_nickname(lookup: &QueryClient, nickname: &str) {
	let _ = lookup
		.send(&Command::new("clientupdate").arg("client_nickname", format!("{nickname} Gateway")))
		.await;
}

/// Split `text` by a topic format like `[#{topic}] {text}` into title and
/// the rest.
pub fn parse_topic_post<'a>(format: &str, text: &'a str) -> Option<(&'a str, &'a str)> {
	let (prefix, rest) = format.split_once("{topic}")?;
	let (middle, suffix) = rest.split_once("{text}")?;
	if middle.is_empty() {
		return None;
	}
	let body = text.strip_prefix(prefix)?.strip_suffix(suffix)?;
	let (title, text) = body.split_once(middle)?;
	(!title.is_empty()).then_some((title, text))
}

pub fn hash_token(token: &str) -> String {
	BASE64_STANDARD.encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn topic_posts_parse() {
		assert_eq!(
			parse_topic_post("[#{topic}] {text}", "[#Raid] go now"),
			Some(("Raid", "go now"))
		);
		assert_eq!(parse_topic_post("[#{topic}] {text}", "[#Raid]go"), None);
		assert_eq!(parse_topic_post("[#{topic}] {text}", "hello"), None);
		assert_eq!(parse_topic_post("{text}", "hello"), None);
		assert_eq!(parse_topic_post("{topic}: {text} <<", "T: x <<"), Some(("T", "x")));
	}
}
