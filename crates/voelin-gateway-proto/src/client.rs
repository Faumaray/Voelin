//! A typed client (feature `client`): log in, send requests and await their
//! answers, receive pushes.
//!
//! [`GatewayClient`] is a cheap handle to a task that owns the WebSocket.
//! Requests get an envelope id; the answer with the same id completes the
//! request. Everything else (pushes, answers to requests sent with
//! [`GatewayClient::send`]) arrives on the [`Push`] receiver.
//!
//! ```no_run
//! # async fn demo(key: tsproto_types::crypto::EccKeyPrivP256) -> Result<(), voelin_gateway_proto::client::ClientError> {
//! use voelin_gateway_proto::client::{Login, connect};
//! use voelin_model::ChatTarget;
//! let login = Login::Identity { key, key_offset: 0 };
//! let (client, mut pushes) = connect("ws://127.0.0.1:7788/v1", login).await?;
//! if client.has(voelin_gateway_proto::feature::PINS) {
//!     let pins = client.pins(ChatTarget::Channel(1)).await?;
//! }
//! while let Some(push) = pushes.recv().await { /* ... */ }
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::{self, Message};
use tsproto_types::crypto::EccKeyPrivP256;
use voelin_model::{ChannelId, ChatMessage, ChatTarget, PresenceDelta, PresenceSnapshot};

use crate::messages::{ClientMsg, Envelope, ErrorCode, HistoryEntry, ServerMsg};
use crate::types::*;

/// How to log in.
pub enum Login {
	/// Sign the challenge with a TeamSpeak identity.
	Identity {
		key: EccKeyPrivP256,
		/// Hash-cash counter of the identity.
		key_offset: u64,
	},
	/// A token from an earlier login ([`GatewayClient::token`]).
	Token(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
	/// The gateway answered with an error.
	#[error("{code:?}: {message}")]
	Gateway { code: ErrorCode, message: String },
	/// The gateway answered with a message that does not fit the request.
	#[error("unexpected answer: {0}")]
	Unexpected(String),
	#[error("connection closed")]
	Closed,
	#[error("websocket: {0}")]
	WebSocket(#[from] tungstenite::Error),
	#[error("invalid message: {0}")]
	Json(#[from] serde_json::Error),
}

impl ClientError {
	/// The gateway's error code, if the gateway refused the request.
	pub fn code(&self) -> Option<ErrorCode> {
		match self {
			ClientError::Gateway { code, .. } => Some(*code),
			_ => None,
		}
	}

	fn unexpected(msg: ServerMsg) -> Self {
		ClientError::Unexpected(format!("{msg:?}"))
	}
}

/// Messages the gateway sends on its own, or answers to requests nobody
/// waits for.
#[derive(Clone, Debug, PartialEq)]
pub enum Push {
	PresenceSnapshot(PresenceSnapshot),
	PresenceDelta(PresenceDelta),
	/// A message in an open chat (sessions without [`GatewayClient::enable`]).
	Chat {
		id: i64,
		message: ChatMessage,
	},
	/// A message in an open chat, with its gateway metadata.
	Message(HistoryEntry),
	Pinned {
		target: ChatTarget,
		pin: PinInfo,
	},
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
	TopicUpdated(TopicInfo),
	EventUpdated(EventInfo),
	EventDeleted(i64),
	EventReminder {
		event: EventInfo,
		starts_in_ms: i64,
	},
	StreamStarted(StreamEntry),
	StreamUpdated(StreamEntry),
	StreamEnded {
		id: String,
		reason: String,
	},
	ActivityAdded(ActivityEntry),
	/// The features available to this user changed; also in
	/// [`GatewayClient::capabilities`].
	Capabilities(Vec<String>),
	/// A request sent with [`GatewayClient::send`] failed.
	Error {
		code: ErrorCode,
		message: String,
	},
	/// Anything else, e.g. `ok` answers to [`GatewayClient::send`].
	Other(ServerMsg),
	/// The connection ended; the last push.
	Disconnected(Option<String>),
}

impl From<ServerMsg> for Push {
	fn from(msg: ServerMsg) -> Self {
		match msg {
			ServerMsg::PresenceSnapshot { snapshot, .. } => Push::PresenceSnapshot(snapshot),
			ServerMsg::PresenceDelta { delta, .. } => Push::PresenceDelta(delta),
			ServerMsg::ChatEvent { id, message } => Push::Chat { id, message },
			ServerMsg::Message { entry } => Push::Message(entry),
			ServerMsg::Pinned { target, pin } => Push::Pinned { target, pin },
			ServerMsg::Unpinned { target, message_id, by } => {
				Push::Unpinned { target, message_id, by }
			}
			ServerMsg::Reaction { target, message_id, emoji, user, added, count } => {
				Push::Reaction { target, message_id, emoji, user, added, count }
			}
			ServerMsg::TopicUpdated { topic } => Push::TopicUpdated(topic),
			ServerMsg::EventUpdated { event } => Push::EventUpdated(event),
			ServerMsg::EventDeleted { id } => Push::EventDeleted(id),
			ServerMsg::EventReminder { event, starts_in_ms } => {
				Push::EventReminder { event, starts_in_ms }
			}
			ServerMsg::StreamStarted { stream } => Push::StreamStarted(stream),
			ServerMsg::StreamUpdated { stream } => Push::StreamUpdated(stream),
			ServerMsg::StreamEnded { id, reason } => Push::StreamEnded { id, reason },
			ServerMsg::ActivityAdded { entry } => Push::ActivityAdded(entry),
			ServerMsg::Capabilities { capabilities } => Push::Capabilities(capabilities),
			ServerMsg::Error { code, message } => Push::Error { code, message },
			other => Push::Other(other),
		}
	}
}

/// What the gateway said about itself and the user at login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
	pub gateway_id: String,
	pub server_uid: String,
	pub server_name: String,
	/// The user's unique id on the server.
	pub uid: String,
	pub token: String,
	/// Unix seconds.
	pub token_expires: i64,
}

struct Outgoing {
	id: u64,
	msg: ClientMsg,
	reply: Option<oneshot::Sender<ServerMsg>>,
}

/// Handle to a logged-in gateway connection. Clones share the connection;
/// it closes when the last handle is dropped.
#[derive(Clone)]
pub struct GatewayClient {
	out: mpsc::UnboundedSender<Outgoing>,
	info: Arc<SessionInfo>,
	capabilities: Arc<RwLock<Vec<String>>>,
	next_id: Arc<AtomicU64>,
}

/// Connect to `url` (e.g. `ws://127.0.0.1:7788/v1`) and log in.
pub async fn connect(
	url: &str,
	login: Login,
) -> Result<(GatewayClient, mpsc::UnboundedReceiver<Push>), ClientError> {
	use tokio_tungstenite::tungstenite::client::IntoClientRequest;
	let mut request = url.into_client_request()?;
	request.headers_mut().insert(
		"Sec-WebSocket-Protocol",
		tungstenite::http::HeaderValue::from_static(crate::SUBPROTOCOL),
	);
	let (ws, _) = tokio_tungstenite::connect_async(request).await?;
	GatewayClient::start(ws, login).await
}

fn now_secs() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or_default()
}

async fn recv<S>(ws: &mut S) -> Result<Envelope<ServerMsg>, ClientError>
where
	S: Stream<Item = Result<Message, tungstenite::Error>> + Unpin,
{
	loop {
		match ws.next().await {
			Some(Ok(Message::Text(text))) => return Ok(serde_json::from_str(text.as_str())?),
			Some(Ok(Message::Close(_))) | None => return Err(ClientError::Closed),
			Some(Ok(_)) => continue,
			Some(Err(e)) => return Err(e.into()),
		}
	}
}

async fn send_frame<S>(ws: &mut S, id: u64, msg: ClientMsg) -> Result<(), ClientError>
where
	S: Sink<Message, Error = tungstenite::Error> + Unpin,
{
	let text = serde_json::to_string(&Envelope::with_id(id, msg))?;
	ws.send(Message::Text(text.into())).await?;
	Ok(())
}

impl GatewayClient {
	/// Log in over an open WebSocket (subprotocol [`crate::SUBPROTOCOL`]) and
	/// start the connection task.
	pub async fn start<S>(
		mut ws: S,
		login: Login,
	) -> Result<(Self, mpsc::UnboundedReceiver<Push>), ClientError>
	where
		S: Stream<Item = Result<Message, tungstenite::Error>>
			+ Sink<Message, Error = tungstenite::Error>
			+ Unpin
			+ Send
			+ 'static,
	{
		let (gateway_id, server_uid, server_name, nonce) = match recv(&mut ws).await?.msg {
			ServerMsg::Hello { gateway_id, server_uid, server_name, nonce, .. } => {
				(gateway_id, server_uid, server_name, nonce)
			}
			other => return Err(ClientError::unexpected(other)),
		};
		let msg = match login {
			Login::Identity { key, key_offset } => {
				let ts = now_secs();
				ClientMsg::Auth {
					omega: key.to_pub().to_ts(),
					key_offset,
					ts,
					signature: crate::sign_challenge(&key, &gateway_id, &server_uid, &nonce, ts),
					nickname: String::new(),
				}
			}
			Login::Token(token) => ClientMsg::Resume { token, nickname: String::new() },
		};
		send_frame(&mut ws, 1, msg).await?;
		let (uid, token, token_expires, capabilities) = match recv(&mut ws).await?.msg {
			ServerMsg::AuthOk { uid, token, token_expires, capabilities } => {
				(uid, token, token_expires, capabilities)
			}
			ServerMsg::Error { code, message } => {
				return Err(ClientError::Gateway { code, message });
			}
			other => return Err(ClientError::unexpected(other)),
		};
		let info = Arc::new(SessionInfo {
			gateway_id,
			server_uid,
			server_name,
			uid,
			token,
			token_expires,
		});
		let capabilities = Arc::new(RwLock::new(capabilities));
		let (out, out_rx) = mpsc::unbounded_channel();
		let (push_tx, pushes) = mpsc::unbounded_channel();
		tokio::spawn(run(ws, out_rx, push_tx, capabilities.clone()));
		Ok((Self { out, info, capabilities, next_id: Arc::new(AtomicU64::new(2)) }, pushes))
	}

	pub fn info(&self) -> &SessionInfo {
		&self.info
	}

	/// The user's unique id on the server.
	pub fn uid(&self) -> &str {
		&self.info.uid
	}

	/// Log in again with this instead of a signature ([`Login::Token`]).
	pub fn token(&self) -> &str {
		&self.info.token
	}

	/// Features available to this user (see [`crate::feature`]); kept up to
	/// date while connected. Hide what is missing.
	pub fn capabilities(&self) -> Vec<String> {
		self.capabilities.read().unwrap().clone()
	}

	pub fn has(&self, feature: &str) -> bool {
		self.capabilities.read().unwrap().iter().any(|c| c == feature)
	}

	pub fn is_connected(&self) -> bool {
		!self.out.is_closed()
	}

	/// Send without waiting; the answer arrives as a [`Push`].
	pub fn send(&self, msg: ClientMsg) -> Result<(), ClientError> {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		self.out.send(Outgoing { id, msg, reply: None }).map_err(|_| ClientError::Closed)
	}

	/// Send and wait for the answer. An `error` answer is returned as
	/// [`ClientError::Gateway`].
	pub async fn request(&self, msg: ClientMsg) -> Result<ServerMsg, ClientError> {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		let (reply, rx) = oneshot::channel();
		self.out.send(Outgoing { id, msg, reply: Some(reply) }).map_err(|_| ClientError::Closed)?;
		match rx.await.map_err(|_| ClientError::Closed)? {
			ServerMsg::Error { code, message } => Err(ClientError::Gateway { code, message }),
			msg => Ok(msg),
		}
	}

	async fn ok(&self, msg: ClientMsg) -> Result<(), ClientError> {
		match self.request(msg).await? {
			ServerMsg::Ok => Ok(()),
			other => Err(ClientError::unexpected(other)),
		}
	}
}

/// Typed requests. Chat and presence pushes need the matching
/// open/subscribe request; extension pushes for open chats need
/// [`GatewayClient::enable`].
impl GatewayClient {
	/// Start the presence stream; the snapshot arrives as a push.
	pub fn subscribe_presence(&self) -> Result<(), ClientError> {
		self.send(ClientMsg::SubscribePresence)
	}

	pub async fn unsubscribe_presence(&self) -> Result<(), ClientError> {
		self.ok(ClientMsg::UnsubscribePresence).await
	}

	pub async fn open_chat(&self, target: ChatTarget) -> Result<(), ClientError> {
		self.ok(ClientMsg::OpenChat { target }).await
	}

	pub async fn close_chat(&self, target: ChatTarget) -> Result<(), ClientError> {
		self.ok(ClientMsg::CloseChat { target }).await
	}

	pub async fn send_chat(&self, target: ChatTarget, text: String) -> Result<(), ClientError> {
		self.ok(ClientMsg::SendChat { target, text }).await
	}

	pub async fn ping(&self) -> Result<(), ClientError> {
		match self.request(ClientMsg::Ping).await? {
			ServerMsg::Pong => Ok(()),
			other => Err(ClientError::unexpected(other)),
		}
	}

	/// Receive the extension pushes for open chats; returns the features enabled.
	pub async fn enable(&self, features: Vec<String>) -> Result<Vec<String>, ClientError> {
		match self.request(ClientMsg::Enable { features }).await? {
			ServerMsg::Enabled { features } => Ok(features),
			other => Err(ClientError::unexpected(other)),
		}
	}

	/// What the user may do, server-wide or in `channel`.
	pub async fn permissions(
		&self,
		channel: Option<ChannelId>,
	) -> Result<Vec<Action>, ClientError> {
		match self.request(ClientMsg::Permissions { channel }).await? {
			ServerMsg::Permissions { actions, .. } => Ok(actions),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn history(&self, query: HistoryQuery) -> Result<HistoryPage, ClientError> {
		match self.request(ClientMsg::QueryHistory(query)).await? {
			ServerMsg::HistoryPage(page) => Ok(page),
			other => Err(ClientError::unexpected(other)),
		}
	}

	/// New and changed messages since `since_rev`; returns them with the
	/// revision to pass next time and whether more are waiting.
	pub async fn sync(
		&self,
		target: ChatTarget,
		since_rev: i64,
		limit: Option<u32>,
	) -> Result<(Vec<HistoryEntry>, i64, bool), ClientError> {
		match self.request(ClientMsg::Sync { target, since_rev, limit }).await? {
			ServerMsg::SyncPage { messages, rev, has_more, .. } => Ok((messages, rev, has_more)),
			other => Err(ClientError::unexpected(other)),
		}
	}

	/// Post, optionally into a topic; returns the stored message.
	pub async fn post(
		&self,
		target: ChatTarget,
		text: String,
		topic: Option<i64>,
	) -> Result<HistoryEntry, ClientError> {
		match self.request(ClientMsg::Post { target, text, topic }).await? {
			ServerMsg::Posted { entry } => Ok(entry),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn pin(&self, message_id: i64) -> Result<(), ClientError> {
		self.ok(ClientMsg::Pin { message_id }).await
	}

	pub async fn unpin(&self, message_id: i64) -> Result<(), ClientError> {
		self.ok(ClientMsg::Unpin { message_id }).await
	}

	pub async fn pins(&self, target: ChatTarget) -> Result<Vec<PinInfo>, ClientError> {
		match self.request(ClientMsg::ListPins { target }).await? {
			ServerMsg::Pins { pins, .. } => Ok(pins),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn react(&self, message_id: i64, emoji: String) -> Result<(), ClientError> {
		self.ok(ClientMsg::React { message_id, emoji }).await
	}

	pub async fn unreact(&self, message_id: i64, emoji: String) -> Result<(), ClientError> {
		self.ok(ClientMsg::Unreact { message_id, emoji }).await
	}

	pub async fn reactors(
		&self,
		message_id: i64,
		emoji: String,
	) -> Result<Vec<UserRef>, ClientError> {
		match self.request(ClientMsg::Reactors { message_id, emoji }).await? {
			ServerMsg::Reactors { users, .. } => Ok(users),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn create_topic(
		&self,
		target: ChatTarget,
		title: String,
		message_id: Option<i64>,
	) -> Result<TopicInfo, ClientError> {
		self.topic(ClientMsg::CreateTopic { target, title, message_id }).await
	}

	pub async fn update_topic(
		&self,
		topic_id: i64,
		title: Option<String>,
		archived: Option<bool>,
	) -> Result<TopicInfo, ClientError> {
		self.topic(ClientMsg::UpdateTopic { topic_id, title, archived }).await
	}

	async fn topic(&self, msg: ClientMsg) -> Result<TopicInfo, ClientError> {
		match self.request(msg).await? {
			ServerMsg::Topic { topic } => Ok(topic),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn topics(
		&self,
		target: ChatTarget,
		include_archived: bool,
	) -> Result<Vec<TopicInfo>, ClientError> {
		match self.request(ClientMsg::ListTopics { target, include_archived }).await? {
			ServerMsg::Topics { topics, .. } => Ok(topics),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn create_event(&self, event: EventSpec) -> Result<EventInfo, ClientError> {
		self.event_reply(ClientMsg::CreateEvent { event }).await
	}

	pub async fn update_event(&self, id: i64, event: EventSpec) -> Result<EventInfo, ClientError> {
		self.event_reply(ClientMsg::UpdateEvent { id, event }).await
	}

	pub async fn delete_event(&self, id: i64) -> Result<(), ClientError> {
		self.ok(ClientMsg::DeleteEvent { id }).await
	}

	/// One event with its attendees.
	pub async fn event(&self, id: i64) -> Result<EventInfo, ClientError> {
		self.event_reply(ClientMsg::GetEvent { id }).await
	}

	pub async fn events(&self, query: EventQuery) -> Result<Vec<EventInfo>, ClientError> {
		match self.request(ClientMsg::ListEvents(query)).await? {
			ServerMsg::Events { events } => Ok(events),
			other => Err(ClientError::unexpected(other)),
		}
	}

	/// Answer an event; `None` withdraws the answer.
	pub async fn rsvp(
		&self,
		event_id: i64,
		status: Option<RsvpStatus>,
	) -> Result<EventInfo, ClientError> {
		self.event_reply(ClientMsg::Rsvp { event_id, status }).await
	}

	async fn event_reply(&self, msg: ClientMsg) -> Result<EventInfo, ClientError> {
		match self.request(msg).await? {
			ServerMsg::Event { event } => Ok(event),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn subscribe_events(&self) -> Result<(), ClientError> {
		self.ok(ClientMsg::SubscribeEvents).await
	}

	pub async fn unsubscribe_events(&self) -> Result<(), ClientError> {
		self.ok(ClientMsg::UnsubscribeEvents).await
	}

	pub async fn register_stream(&self, stream: StreamSpec) -> Result<StreamEntry, ClientError> {
		self.stream_reply(ClientMsg::RegisterStream(stream)).await
	}

	pub async fn update_stream(
		&self,
		stream_id: String,
		title: Option<String>,
		viewers: Option<u32>,
	) -> Result<StreamEntry, ClientError> {
		self.stream_reply(ClientMsg::UpdateStream { stream_id, title, viewers }).await
	}

	async fn stream_reply(&self, msg: ClientMsg) -> Result<StreamEntry, ClientError> {
		match self.request(msg).await? {
			ServerMsg::Stream { stream } => Ok(stream),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn unregister_stream(&self, stream_id: String) -> Result<(), ClientError> {
		self.ok(ClientMsg::UnregisterStream { stream_id }).await
	}

	pub async fn streams(&self) -> Result<Vec<StreamEntry>, ClientError> {
		self.streams_reply(ClientMsg::ListStreams).await
	}

	/// Returns the directory; changes follow as pushes.
	pub async fn subscribe_streams(&self) -> Result<Vec<StreamEntry>, ClientError> {
		self.streams_reply(ClientMsg::SubscribeStreams).await
	}

	async fn streams_reply(&self, msg: ClientMsg) -> Result<Vec<StreamEntry>, ClientError> {
		match self.request(msg).await? {
			ServerMsg::Streams { streams } => Ok(streams),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn unsubscribe_streams(&self) -> Result<(), ClientError> {
		self.ok(ClientMsg::UnsubscribeStreams).await
	}

	/// Newest first; returns the entries and whether older ones exist.
	pub async fn activity(
		&self,
		before: Option<i64>,
		limit: Option<u32>,
	) -> Result<(Vec<ActivityEntry>, bool), ClientError> {
		match self.request(ClientMsg::ListActivity { before, limit }).await? {
			ServerMsg::Activity { entries, has_more } => Ok((entries, has_more)),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn subscribe_activity(&self) -> Result<(), ClientError> {
		self.ok(ClientMsg::SubscribeActivity).await
	}

	pub async fn unsubscribe_activity(&self) -> Result<(), ClientError> {
		self.ok(ClientMsg::UnsubscribeActivity).await
	}

	pub async fn config_list(&self) -> Result<Vec<ConfigEntry>, ClientError> {
		self.config_entries(ClientMsg::ConfigList).await
	}

	/// Read the gateway's file again; returns the resulting configuration.
	pub async fn config_reload(&self) -> Result<Vec<ConfigEntry>, ClientError> {
		self.config_entries(ClientMsg::ConfigReload).await
	}

	async fn config_entries(&self, msg: ClientMsg) -> Result<Vec<ConfigEntry>, ClientError> {
		match self.request(msg).await? {
			ServerMsg::Config { entries } => Ok(entries),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn config_get(&self, key: String) -> Result<ConfigEntry, ClientError> {
		self.config_value(ClientMsg::ConfigGet { key }).await
	}

	pub async fn config_set(
		&self,
		key: String,
		value: serde_json::Value,
	) -> Result<ConfigEntry, ClientError> {
		self.config_value(ClientMsg::ConfigSet { key, value }).await
	}

	pub async fn config_reset(&self, key: String) -> Result<ConfigEntry, ClientError> {
		self.config_value(ClientMsg::ConfigReset { key }).await
	}

	async fn config_value(&self, msg: ClientMsg) -> Result<ConfigEntry, ClientError> {
		match self.request(msg).await? {
			ServerMsg::ConfigValue { entry } => Ok(entry),
			other => Err(ClientError::unexpected(other)),
		}
	}

	pub async fn perm_list(&self) -> Result<Vec<PermRuleInfo>, ClientError> {
		self.perm_rules(ClientMsg::PermList).await
	}

	pub async fn perm_set(
		&self,
		action: Action,
		rule: PermRule,
	) -> Result<Vec<PermRuleInfo>, ClientError> {
		self.perm_rules(ClientMsg::PermSet { action, rule }).await
	}

	pub async fn perm_reset(&self, action: Action) -> Result<Vec<PermRuleInfo>, ClientError> {
		self.perm_rules(ClientMsg::PermReset { action }).await
	}

	async fn perm_rules(&self, msg: ClientMsg) -> Result<Vec<PermRuleInfo>, ClientError> {
		match self.request(msg).await? {
			ServerMsg::PermRules { rules } => Ok(rules),
			other => Err(ClientError::unexpected(other)),
		}
	}
}

/// The connection task: frames out, answers to their requests, pushes to
/// the receiver.
async fn run<S>(
	mut ws: S,
	mut out: mpsc::UnboundedReceiver<Outgoing>,
	pushes: mpsc::UnboundedSender<Push>,
	capabilities: Arc<RwLock<Vec<String>>>,
) where
	S: Stream<Item = Result<Message, tungstenite::Error>>
		+ Sink<Message, Error = tungstenite::Error>
		+ Unpin
		+ Send
		+ 'static,
{
	let mut pending: HashMap<u64, oneshot::Sender<ServerMsg>> = HashMap::new();
	let reason = loop {
		tokio::select! {
			frame = recv(&mut ws) => {
				let env = match frame {
					Ok(env) => env,
					// A push of a type this version does not know.
					Err(ClientError::Json(_)) => continue,
					Err(ClientError::Closed) => break None,
					Err(e) => break Some(e.to_string()),
				};
				if let ServerMsg::Capabilities { capabilities: caps } = &env.msg {
					*capabilities.write().unwrap() = caps.clone();
				}
				match env.id.and_then(|id| pending.remove(&id)) {
					Some(waiter) => {
						let _ = waiter.send(env.msg);
					}
					None => {
						let _ = pushes.send(env.msg.into());
					}
				}
			}
			msg = out.recv() => {
				let Some(Outgoing { id, msg, reply }) = msg else {
					let _ = ws.close().await;
					break None;
				};
				if let Some(reply) = reply {
					pending.insert(id, reply);
				}
				if let Err(e) = send_frame(&mut ws, id, msg).await {
					break Some(e.to_string());
				}
			}
		}
	};
	let _ = pushes.send(Push::Disconnected(reason));
}
