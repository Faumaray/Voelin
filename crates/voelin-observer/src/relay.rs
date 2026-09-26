//! Channel-chat relays: one query session sitting in each relayed channel.
//!
//! TeamSpeak only delivers channel chat to clients in that channel, and a
//! client can only post to its own channel. A relay session is an invisible
//! query client moved into the channel; it forwards what is said there and
//! posts messages on behalf of users as `[Nick] text`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use voelin_model::{ChannelId, ChatMessage, ChatTarget, relay_text, split_message};
use voelin_query::{Command, Connect, Notification, QueryClient};

use crate::convert::is_text_message;

/// `clientupdate client_nickname=` with a name someone else has.
const ERR_NICKNAME_IN_USE: u32 = 513;
/// `clientmove` into the channel the client is already in.
const ERR_ALREADY_MEMBER: u32 = 770;

/// TeamSpeak's limit for one text message.
const MAX_MESSAGE_BYTES: usize = 1024;

#[derive(Clone, Debug)]
pub struct RelayConfig {
	pub connect: Connect,
	/// Base nickname; the channel id is appended because nicknames are unique.
	pub nickname: String,
	/// How posts appear, with `{nick}` and `{text}` placeholders.
	pub format: String,
}

impl RelayConfig {
	pub fn new(connect: Connect) -> Self {
		Self { connect, nickname: "Chat Relay".into(), format: "[{nick}] {text}".into() }
	}
}

#[derive(Clone, Debug, PartialEq)]
pub enum RelayEvent {
	/// Someone wrote in a relayed channel (our own posts are not echoed).
	Message(ChatMessage),
	/// A relay stopped (connection lost or closed).
	Closed { channel: ChannelId, reason: String },
}

struct Session {
	client: QueryClient,
	task: JoinHandle<()>,
}

impl Drop for Session {
	fn drop(&mut self) {
		self.task.abort();
	}
}

/// Manages relay sessions, at most one per channel.
#[derive(Clone)]
pub struct RelayPool {
	config: Arc<RelayConfig>,
	sessions: Arc<Mutex<HashMap<ChannelId, Session>>>,
	events: broadcast::Sender<RelayEvent>,
}

impl RelayPool {
	pub fn new(config: RelayConfig) -> Self {
		let (events, _) = broadcast::channel(1024);
		Self { config: Arc::new(config), sessions: Default::default(), events }
	}

	pub fn subscribe(&self) -> broadcast::Receiver<RelayEvent> {
		self.events.subscribe()
	}

	/// Channels with an open relay.
	pub async fn channels(&self) -> Vec<ChannelId> {
		self.sessions.lock().await.keys().copied().collect()
	}

	/// Start relaying a channel (no-op if already open).
	pub async fn open(&self, channel: ChannelId) -> voelin_query::Result<()> {
		let mut sessions = self.sessions.lock().await;
		if sessions.get(&channel).is_some_and(|s| !s.task.is_finished()) {
			return Ok(());
		}
		let (client, notifications) = QueryClient::connect(&self.config.connect).await?;
		let Some(notifications) = notifications else {
			return Err(voelin_query::Error::Unsupported("relays need a transport with events"));
		};
		let own_id = client.own_client_id().await?;
		set_nickname(&client, &format!("{} {channel}", self.config.nickname)).await;
		match client.send(&Command::new("clientmove").arg("clid", own_id).arg("cid", channel)).await
		{
			// Query clients start in the default channel.
			Err(voelin_query::Error::Query(e)) if e.id == ERR_ALREADY_MEMBER => {}
			other => {
				other?;
			}
		}
		client.send(&Command::new("servernotifyregister").arg("event", "textchannel")).await?;
		info!(channel, own_id, "relay opened");
		let task = tokio::spawn(forward(channel, own_id, notifications, self.events.clone()));
		sessions.insert(channel, Session { client, task });
		Ok(())
	}

	/// Stop relaying a channel.
	pub async fn close(&self, channel: ChannelId) {
		let session = self.sessions.lock().await.remove(&channel);
		if let Some(session) = session {
			session.client.quit().await;
			info!(channel, "relay closed");
		}
	}

	/// Stop all relays.
	pub async fn close_all(&self) {
		let sessions: Vec<_> = self.sessions.lock().await.drain().collect();
		for (channel, session) in sessions {
			session.client.quit().await;
			info!(channel, "relay closed");
		}
	}

	/// Post `text` in `channel` on behalf of `nick`, opening a relay if needed.
	pub async fn send(
		&self,
		channel: ChannelId,
		nick: &str,
		text: &str,
	) -> voelin_query::Result<()> {
		self.open(channel).await?;
		let client = match self.sessions.lock().await.get(&channel) {
			Some(s) => s.client.clone(),
			None => return Err(voelin_query::Error::Closed),
		};
		let full = relay_text(&self.config.format, nick, text);
		for part in split_message(&full, MAX_MESSAGE_BYTES) {
			client
				.send(
					&Command::new("sendtextmessage")
						.arg("targetmode", 2)
						.arg("target", channel)
						.arg("msg", part),
				)
				.await?;
		}
		Ok(())
	}
}

/// Nicknames are unique per server; try a few numbered variants. The name is
/// cosmetic, so failing keeps the server-assigned one.
async fn set_nickname(client: &QueryClient, base: &str) {
	for attempt in 1..=5 {
		let nickname = if attempt == 1 { base.to_string() } else { format!("{base} ({attempt})") };
		match client.send(&Command::new("clientupdate").arg("client_nickname", &nickname)).await {
			Ok(_) => return,
			Err(voelin_query::Error::Query(e)) if e.id == ERR_NICKNAME_IN_USE => continue,
			Err(e) => {
				warn!(%e, %nickname, "could not set relay nickname");
				return;
			}
		}
	}
	warn!(base, "all relay nicknames in use");
}

async fn forward(
	channel: ChannelId,
	own_id: u16,
	mut notifications: mpsc::UnboundedReceiver<Notification>,
	events: broadcast::Sender<RelayEvent>,
) {
	while let Some(n) = notifications.recv().await {
		let now = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_millis() as i64)
			.unwrap_or_default();
		let Some(mut msg) = is_text_message(&n, now) else { continue };
		if msg.author_id == Some(own_id) {
			continue;
		}
		if msg.target == ChatTarget::Channel(0) {
			msg.target = ChatTarget::Channel(channel);
		}
		debug!(channel, author = %msg.author_name, "relayed message");
		let _ = events.send(RelayEvent::Message(msg));
	}
	let _ = events.send(RelayEvent::Closed { channel, reason: "connection closed".into() });
}
