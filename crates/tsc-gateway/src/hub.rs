//! State shared by all sessions: one observer, one relay pool, one query
//! session for lookups, chat storage and fan-out.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::prelude::*;
use rand::RngExt;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tracing::{info, warn};
use tsc_gateway_proto::{ErrorCode, UniqueIds, identity_level, verify_auth};
use tsc_model::{ChannelId, ChatMessage, ChatTarget, Presence, ServerFlavor, relay_text};
use tsc_observer::{Observer, ObserverConfig, ObserverEvent, RelayConfig, RelayEvent, RelayPool};
use tsc_query::{Command, QueryClient};

use crate::config::Config;
use crate::db::{Db, TokenInfo};
use crate::perms::{ChannelAccess, PermIds, PermResolver, channel_access, effective};

pub fn now_secs() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or_default()
}

fn now_ms() -> i64 {
	now_secs() * 1000
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

/// An error to send to the client.
#[derive(Debug)]
pub struct Denied(pub ErrorCode, pub String);

impl Denied {
	fn new(code: ErrorCode, msg: impl Into<String>) -> Self {
		Self(code, msg.into())
	}
}

impl From<tsc_query::Error> for Denied {
	fn from(e: tsc_query::Error) -> Self {
		Denied::new(ErrorCode::Unavailable, e.to_string())
	}
}

pub struct Hub {
	pub config: Config,
	pub gateway_id: String,
	pub server_uid: String,
	pub server_name: String,
	pub is_ts6: bool,
	needed_level: u8,
	default_channel: ChannelId,
	pub observer: Observer,
	relays: RelayPool,
	lookup: QueryClient,
	perms: PermResolver,
	pub db: Option<Db>,
	chat: broadcast::Sender<(i64, ChatMessage)>,
	/// Readers per relayed channel, and when the last one left.
	readers: Mutex<HashMap<ChannelId, (usize, Instant)>>,
	/// Our own query clients, whose messages are echoes.
	own_clients: Mutex<HashSet<u16>>,
	next_message_id: Mutex<i64>,
}

impl Hub {
	pub async fn start(config: Config) -> Result<Arc<Self>> {
		let connect = config.query_connect();
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
		let _ = lookup
			.send(
				&Command::new("clientupdate")
					.arg("client_nickname", format!("{} Gateway", config.relay.nickname)),
			)
			.await;

		let db = if config.history.enabled {
			let db = Db::open(&config.history.path)
				.with_context(|| format!("failed to open {}", config.history.path.display()))?;
			db.prune(now_secs(), config.history.retention_days)?;
			Some(db)
		} else {
			None
		};

		let observer = Observer::spawn(ObserverConfig::new(connect.clone()));
		let mut relay_config = RelayConfig::new(connect);
		relay_config.nickname = config.relay.nickname.clone();
		relay_config.format = config.relay.format.clone();
		let relays = RelayPool::new(relay_config);

		info!(%server_name, %server_uid, version, is_ts6, "gateway connected to server");
		let hub = Arc::new(Self {
			gateway_id: config.gateway_id(),
			config,
			server_uid,
			server_name,
			is_ts6,
			needed_level,
			default_channel,
			observer,
			relays,
			lookup,
			perms,
			db,
			chat: broadcast::channel(1024).0,
			readers: Default::default(),
			own_clients: Mutex::new([own_id].into()),
			next_message_id: Mutex::new(0),
		});
		tokio::spawn(hub.clone().pump_observer_chat());
		tokio::spawn(hub.clone().pump_relays());
		tokio::spawn(hub.clone().teardown_idle_relays());
		for &cid in &hub.config.relay.pinned_channels {
			if let Err(error) = hub.relays.open(cid).await {
				warn!(cid, %error, "could not open pinned relay");
			}
		}
		Ok(hub)
	}

	pub fn subscribe_chat(&self) -> broadcast::Receiver<(i64, ChatMessage)> {
		self.chat.subscribe()
	}

	/// Store (if history is on) and fan out a message.
	fn publish(&self, msg: ChatMessage) {
		let id = match &self.db {
			Some(db) => db.add_message(&msg).unwrap_or_else(|error| {
				warn!(%error, "failed to store message");
				-1
			}),
			None => {
				let mut next = self.next_message_id.lock().unwrap();
				*next += 1;
				*next
			}
		};
		let _ = self.chat.send((id, msg));
	}

	/// Server chat seen by the observer.
	async fn pump_observer_chat(self: Arc<Self>) {
		let mut events = self.observer.subscribe();
		loop {
			match events.recv().await {
				Ok(ObserverEvent::Chat(msg)) if msg.target == ChatTarget::Server => {
					let own = msg
						.author_id
						.is_some_and(|id| self.own_clients.lock().unwrap().contains(&id));
					if !own {
						self.publish(msg);
					}
				}
				Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
				Err(broadcast::error::RecvError::Closed) => return,
			}
		}
	}

	/// Channel chat seen by relays.
	async fn pump_relays(self: Arc<Self>) {
		let mut events = self.relays.subscribe();
		loop {
			match events.recv().await {
				Ok(RelayEvent::Message(msg)) => {
					let own = msg
						.author_id
						.is_some_and(|id| self.own_clients.lock().unwrap().contains(&id));
					if !own {
						self.publish(msg);
					}
				}
				Ok(RelayEvent::Closed { channel, reason }) => {
					warn!(channel, %reason, "relay closed");
				}
				Err(broadcast::error::RecvError::Lagged(_)) => {}
				Err(broadcast::error::RecvError::Closed) => return,
			}
		}
	}

	async fn teardown_idle_relays(self: Arc<Self>) {
		let idle = Duration::from_secs(self.config.relay.idle_teardown_secs);
		loop {
			tokio::time::sleep(Duration::from_secs(15).min(idle)).await;
			let idle_channels: Vec<ChannelId> = self
				.readers
				.lock()
				.unwrap()
				.iter()
				.filter(|(cid, (n, since))| {
					*n == 0
						&& since.elapsed() >= idle
						&& !self.config.relay.pinned_channels.contains(cid)
				})
				.map(|(cid, _)| *cid)
				.collect();
			for cid in idle_channels {
				self.readers.lock().unwrap().remove(&cid);
				self.relays.close(cid).await;
			}
		}
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
		let needed = self.needed_level.max(self.config.auth.min_security_level);
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
		let hash = hash_token(token);
		let info = self
			.db
			.as_ref()
			.and_then(|db| db.token(&hash, now_secs()).ok().flatten())
			.ok_or_else(|| Denied::new(ErrorCode::AuthFailed, "unknown or expired token"))?;
		let user = self.lookup_user(&info.uid).await?;
		self.audit(Some(&user.uid), "login", "token");
		Ok((user, token.to_string(), info.expires))
	}

	fn issue_token(&self, user: &User) -> Result<(String, i64), Denied> {
		let bytes: [u8; 32] = rand::rng().random();
		let token = BASE64_URL_SAFE_NO_PAD.encode(bytes);
		let expires = now_secs() + self.config.auth.token_ttl_hours as i64 * 3600;
		if let Some(db) = &self.db {
			db.add_token(
				&hash_token(&token),
				&TokenInfo { uid: user.uid.clone(), cldbid: user.cldbid, expires },
			)
			.map_err(|e| Denied::new(ErrorCode::Internal, e.to_string()))?;
		}
		Ok((token, expires))
	}

	async fn lookup_user(&self, uid: &str) -> Result<User, Denied> {
		if self.config.auth.deny_uids.iter().any(|d| d == uid) {
			return Err(Denied::new(ErrorCode::Forbidden, "not allowed on this gateway"));
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
		if !self.config.auth.require_server_groups.is_empty() {
			let groups = self
				.lookup
				.send(&Command::new("servergroupsbyclientid").arg("cldbid", cldbid))
				.await?;
			let allowed = groups
				.iter()
				.filter_map(|r| r.parse::<u64>("sgid"))
				.any(|g| self.config.auth.require_server_groups.contains(&g));
			if !allowed {
				return Err(Denied::new(ErrorCode::Forbidden, "missing required server group"));
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
				return Err(Denied::new(ErrorCode::BadRequest, format!("no channel {cid}")));
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
		Presence { server_name: p.server_name.clone(), channels, clients }
	}

	// Chat

	/// Start reading a channel for one more session.
	pub async fn add_reader(&self, cid: ChannelId) -> Result<(), Denied> {
		let open = self.readers.lock().unwrap().len();
		let already = self.readers.lock().unwrap().contains_key(&cid);
		if !already && open >= self.config.relay.max_channel_relays {
			return Err(Denied::new(
				ErrorCode::Unavailable,
				"too many relayed channels, try later",
			));
		}
		self.relays.open(cid).await?;
		self.readers.lock().unwrap().entry(cid).or_insert((0, Instant::now())).0 += 1;
		Ok(())
	}

	pub fn remove_reader(&self, cid: ChannelId) {
		if let Some(entry) = self.readers.lock().unwrap().get_mut(&cid) {
			entry.0 = entry.0.saturating_sub(1);
			entry.1 = Instant::now();
		}
	}

	/// Post on behalf of `user`; the message is published to readers directly
	/// (the relay's echo is dropped).
	pub async fn post(&self, user: &User, target: &ChatTarget, text: &str) -> Result<(), Denied> {
		match target {
			ChatTarget::Server => {
				if !self.can_post_server_chat(user).await? {
					return Err(Denied::new(ErrorCode::Forbidden, "no permission for server chat"));
				}
				let msg = relay_text(&self.config.relay.format, &user.nickname, text);
				for part in tsc_model::split_message(&msg, 1024) {
					self.lookup
						.send(
							&Command::new("sendtextmessage").arg("targetmode", 3).arg("msg", part),
						)
						.await?;
				}
			}
			ChatTarget::Channel(cid) => {
				if !self.channel_access(user, *cid).await?.post {
					return Err(Denied::new(ErrorCode::Forbidden, "no permission to post here"));
				}
				if !self.readers.lock().unwrap().contains_key(cid) {
					// Posting without reading still needs a relay in the channel.
					self.add_reader(*cid).await?;
					self.remove_reader(*cid);
				}
				self.relays.send(*cid, &user.nickname, text).await?;
			}
			ChatTarget::Private(_) => {
				return Err(Denied::new(ErrorCode::BadRequest, "private chat is not relayed"));
			}
		}
		self.audit(Some(&user.uid), "post", &format!("{target:?}"));
		self.publish(ChatMessage {
			target: target.clone(),
			author_name: user.nickname.clone(),
			author_uid: Some(user.uid.clone()),
			author_id: None,
			text: text.to_string(),
			ts_ms: now_ms(),
			via_relay: true,
		});
		Ok(())
	}

	pub fn audit(&self, uid: Option<&str>, action: &str, detail: &str) {
		if let Some(db) = &self.db {
			db.audit(now_secs(), uid, action, detail);
		}
	}
}

fn hash_token(token: &str) -> String {
	BASE64_STANDARD.encode(Sha256::digest(token.as_bytes()))
}
