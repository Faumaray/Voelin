//! Channel-chat relays: one query session sitting in each relayed channel.
//!
//! TeamSpeak only delivers channel chat to clients in that channel, and a
//! client can only post to its own channel. A relay session is an invisible
//! query client moved into the channel; it forwards what is said there and
//! posts messages on behalf of users under their own nickname ([`post_as`]).

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
/// TeamSpeak's limits for a nickname, in characters.
const MIN_NICKNAME_CHARS: usize = 3;
const MAX_NICKNAME_CHARS: usize = 30;

#[derive(Clone, Debug)]
pub struct RelayConfig {
	pub connect: Connect,
	/// Base nickname; the channel id is appended because nicknames are unique.
	pub nickname: String,
	/// How posts appear when the relay cannot take the author's nickname
	/// ([`post_as`]), with `{nick}` and `{text}` placeholders.
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
	/// The relay's own nickname, taken back after each post.
	nickname: String,
	/// One post at a time: each borrows the author's nickname.
	posting: Arc<Mutex<()>>,
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
		let nickname = set_nickname(&client, &format!("{} {channel}", self.config.nickname)).await;
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
		sessions.insert(channel, Session { client, nickname, posting: Default::default(), task });
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

	/// Post `text` in `channel` as `nick` wrote it ([`post_as`]), opening a
	/// relay if needed.
	pub async fn send(
		&self,
		channel: ChannelId,
		nick: &str,
		text: &str,
	) -> voelin_query::Result<()> {
		self.post(channel, nick, text, &self.config.format).await
	}

	/// [`RelayPool::send`] with another fallback `format` than the pool's.
	pub async fn post(
		&self,
		channel: ChannelId,
		nick: &str,
		text: &str,
		format: &str,
	) -> voelin_query::Result<()> {
		self.open(channel).await?;
		let (client, nickname, posting) = match self.sessions.lock().await.get(&channel) {
			Some(s) => (s.client.clone(), s.nickname.clone(), s.posting.clone()),
			None => return Err(voelin_query::Error::Closed),
		};
		let _posting = posting.lock().await;
		post_as(&client, &nickname, nick, text, format, |part| {
			Command::new("sendtextmessage")
				.arg("targetmode", 2)
				.arg("target", channel)
				.arg("msg", part)
		})
		.await
	}
}

/// Post `text` through the query session `client` as if `nick` wrote it: the
/// session takes the nickname for the post (or `nick1`, `nick2`, … while
/// someone has it, as TeamSpeak names a second client of the same name) and
/// goes back to `own_nickname` afterwards, so it never keeps someone's name.
/// When it cannot take any (too short, refused), the post goes out under the
/// session's own name as `format` (`{nick}`, `{text}`). `command` makes the
/// `sendtextmessage` for one part of the text.
///
/// Callers serialize posts through one session: each borrows a nickname.
pub async fn post_as(
	client: &QueryClient,
	own_nickname: &str,
	nick: &str,
	text: &str,
	format: &str,
	command: impl Fn(String) -> Command,
) -> voelin_query::Result<()> {
	let taken = take_nickname(client, nick).await;
	let full = if taken { text.to_owned() } else { relay_text(format, nick, text) };
	let mut result = Ok(());
	for part in split_message(&full, MAX_MESSAGE_BYTES) {
		result = client.send(&command(part)).await.map(drop);
		if result.is_err() {
			break;
		}
	}
	if taken && !own_nickname.is_empty() {
		set_nickname(client, own_nickname).await;
	}
	result
}

/// Name the session `nick` or a numbered variant; whether it did.
async fn take_nickname(client: &QueryClient, nick: &str) -> bool {
	for nickname in author_nicknames(nick) {
		match client.send(&Command::new("clientupdate").arg("client_nickname", &nickname)).await {
			Ok(_) => return true,
			Err(voelin_query::Error::Query(e)) if e.id == ERR_NICKNAME_IN_USE => continue,
			Err(e) => {
				debug!(%e, "could not take the author's nickname");
				return false;
			}
		}
	}
	false
}

/// The names a post by `nick` may go out under, in order: the nickname
/// itself, then `nick1` to `nick3`, each within TeamSpeak's length limits.
fn author_nicknames(nick: &str) -> Vec<String> {
	let nick = nick.trim();
	if nick.chars().count() < MIN_NICKNAME_CHARS {
		return Vec::new();
	}
	let fit = |suffix: &str| {
		let head: String = nick.chars().take(MAX_NICKNAME_CHARS - suffix.len()).collect();
		format!("{}{suffix}", head.trim_end())
	};
	std::iter::once(fit("")).chain((1..=3).map(|n| fit(&n.to_string()))).collect()
}

/// Nicknames are unique per server; try a few numbered variants. The name is
/// cosmetic, so failing keeps the server-assigned one. The name it got.
async fn set_nickname(client: &QueryClient, base: &str) -> String {
	for attempt in 1..=5 {
		let nickname = if attempt == 1 { base.to_string() } else { format!("{base} ({attempt})") };
		match client.send(&Command::new("clientupdate").arg("client_nickname", &nickname)).await {
			Ok(_) => return nickname,
			Err(voelin_query::Error::Query(e)) if e.id == ERR_NICKNAME_IN_USE => continue,
			Err(e) => {
				warn!(%e, %nickname, "could not set relay nickname");
				return base.to_owned();
			}
		}
	}
	warn!(base, "all relay nicknames in use");
	base.to_owned()
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn posts_go_out_under_the_authors_name_then_numbered_variants() {
		assert_eq!(author_nicknames(" Alice "), ["Alice", "Alice1", "Alice2", "Alice3"]);
		// Too short for TeamSpeak: the relay's own name and the format.
		assert!(author_nicknames("Al").is_empty());
		let long = "x".repeat(40);
		let names = author_nicknames(&long);
		assert_eq!(names[0], "x".repeat(30));
		assert_eq!(names[1], format!("{}1", "x".repeat(29)));
		assert!(names.iter().all(|n| n.chars().count() <= MAX_NICKNAME_CHARS));
		// Characters, not bytes.
		assert_eq!(author_nicknames(&"é".repeat(31))[0].chars().count(), 30);
	}
}
