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
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::net::TcpStream;
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
	connect_watched(url, login, &Progress::default()).await
}

/// [`connect`], telling `progress` each step as it is reached, so a
/// timeout around it can say which step hung, and how long each took.
pub async fn connect_watched(
	url: &str,
	login: Login,
	progress: &Progress,
) -> Result<(GatewayClient, mpsc::UnboundedReceiver<Push>), ClientError> {
	use tokio_tungstenite::tungstenite::client::IntoClientRequest;
	use tokio_tungstenite::tungstenite::error::UrlError;
	let mut request = url.into_client_request()?;
	request.headers_mut().insert(
		"Sec-WebSocket-Protocol",
		tungstenite::http::HeaderValue::from_static(crate::SUBPROTOCOL),
	);
	// As tokio-tungstenite's connect_async, a step at a time.
	let uri = request.uri();
	let host = uri.host().ok_or(tungstenite::Error::Url(UrlError::NoHostName))?;
	let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
	let port = uri
		.port_u16()
		.or_else(|| match uri.scheme_str() {
			Some("wss") => Some(443),
			Some("ws") => Some(80),
			_ => None,
		})
		.ok_or(tungstenite::Error::Url(UrlError::UnsupportedUrlScheme))?;
	let tcp = open(host, port, progress).await?;
	progress.enter(Stage::WebSocket);
	let (ws, _) = tokio_tungstenite::client_async_tls_with_config(request, tcp, None, None).await?;
	GatewayClient::login(ws, login, progress).await
}

/// The TCP connection to `host`: each of its addresses in turn.
async fn open(host: &str, port: u16, progress: &Progress) -> Result<TcpStream, ClientError> {
	let io = |e| ClientError::WebSocket(tungstenite::Error::Io(e));
	progress.enter(Stage::Dns);
	let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await.map_err(io)?.collect();
	progress.enter(Stage::Tcp);
	let mut error = None;
	for addr in addrs {
		progress.peer(addr);
		match TcpStream::connect(addr).await {
			Ok(tcp) => {
				// Small frames, each answered: no waiting to fill packets.
				let _ = tcp.set_nodelay(true);
				return Ok(tcp);
			}
			Err(e) => error = Some(e),
		}
	}
	Err(io(error.unwrap_or_else(|| {
		std::io::Error::new(std::io::ErrorKind::NotFound, format!("no address for {host}"))
	})))
}

/// A step of logging in to a gateway ([`connect_watched`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
	/// Looking up the host's addresses.
	#[default]
	Dns,
	/// The TCP connection.
	Tcp,
	/// TLS (`wss://`) and the WebSocket upgrade.
	WebSocket,
	/// Waiting for the gateway's `hello`.
	Hello,
	/// Waiting for the answer to the login.
	Auth,
	/// Logged in.
	Done,
}

impl Stage {
	pub fn name(self) -> &'static str {
		match self {
			Stage::Dns => "dns",
			Stage::Tcp => "tcp",
			Stage::WebSocket => "websocket",
			Stage::Hello => "hello",
			Stage::Auth => "auth",
			Stage::Done => "done",
		}
	}
}

impl fmt::Display for Stage {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.name())
	}
}

/// How far a login ([`connect_watched`]) got and how long each step took;
/// clones share it. Shown as e.g. `dns 2 ms, tcp 31 ms, websocket 64 ms,
/// hello 1 ms, auth 134 ms`, a step still under way as `hello for 19900 ms`.
#[derive(Clone, Debug, Default)]
pub struct Progress(Arc<Mutex<Steps>>);

#[derive(Debug, Default)]
struct Steps {
	/// The step under way, since when.
	stage: Stage,
	since: Option<Instant>,
	/// The steps done, with how long they took.
	done: Vec<(Stage, Duration)>,
	/// The address connected to (or being tried).
	peer: Option<SocketAddr>,
}

impl Progress {
	/// The step under way (the one that hung, after a timeout), or
	/// [`Stage::Done`].
	pub fn stage(&self) -> Stage {
		self.0.lock().unwrap().stage
	}

	/// The gateway's address, once the TCP connection is being made.
	pub fn peer_addr(&self) -> Option<SocketAddr> {
		self.0.lock().unwrap().peer
	}

	fn enter(&self, stage: Stage) {
		let mut steps = self.0.lock().unwrap();
		let now = Instant::now();
		if let Some(since) = steps.since {
			let step = steps.stage;
			steps.done.push((step, now - since));
		}
		steps.stage = stage;
		steps.since = (stage != Stage::Done).then_some(now);
	}

	fn peer(&self, addr: SocketAddr) {
		self.0.lock().unwrap().peer = Some(addr);
	}
}

impl fmt::Display for Progress {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let steps = self.0.lock().unwrap();
		let mut parts: Vec<String> = steps
			.done
			.iter()
			.map(|(stage, took)| format!("{stage} {} ms", took.as_millis()))
			.collect();
		if let Some(since) = steps.since {
			parts.push(format!("{} for {} ms", steps.stage, since.elapsed().as_millis()));
		}
		if parts.is_empty() {
			return f.write_str("not started");
		}
		f.write_str(&parts.join(", "))
	}
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
		ws: S,
		login: Login,
	) -> Result<(Self, mpsc::UnboundedReceiver<Push>), ClientError>
	where
		S: Stream<Item = Result<Message, tungstenite::Error>>
			+ Sink<Message, Error = tungstenite::Error>
			+ Unpin
			+ Send
			+ 'static,
	{
		Self::login(ws, login, &Progress::default()).await
	}

	async fn login<S>(
		mut ws: S,
		login: Login,
		progress: &Progress,
	) -> Result<(Self, mpsc::UnboundedReceiver<Push>), ClientError>
	where
		S: Stream<Item = Result<Message, tungstenite::Error>>
			+ Sink<Message, Error = tungstenite::Error>
			+ Unpin
			+ Send
			+ 'static,
	{
		progress.enter(Stage::Hello);
		let (gateway_id, server_uid, server_name, nonce) = match recv(&mut ws).await?.msg {
			ServerMsg::Hello { gateway_id, server_uid, server_name, nonce, .. } => {
				(gateway_id, server_uid, server_name, nonce)
			}
			other => return Err(ClientError::unexpected(other)),
		};
		progress.enter(Stage::Auth);
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
		progress.enter(Stage::Done);
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

#[cfg(test)]
mod tests {
	use tokio::net::TcpListener;
	use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

	use super::*;

	/// Wait (without a clock: tokio's `time` is not enabled here) until the
	/// login in progress reaches `stage`.
	async fn reaches(progress: &Progress, stage: Stage) {
		for _ in 0..100_000 {
			if progress.stage() == stage {
				return;
			}
			tokio::task::yield_now().await;
		}
		panic!("at {} instead of {stage}", progress.stage());
	}

	#[tokio::test]
	async fn a_closed_port_fails_at_the_tcp_step() {
		let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
		let progress = Progress::default();
		assert_eq!(progress.to_string(), "not started");
		let login = Login::Token("t".into());
		let result = connect_watched(&format!("ws://127.0.0.1:{port}/v1"), login, &progress).await;
		assert!(matches!(result, Err(ClientError::WebSocket(_))));
		assert_eq!(progress.stage(), Stage::Tcp);
		assert_eq!(progress.peer_addr(), Some(([127, 0, 0, 1], port).into()));
		assert!(progress.to_string().starts_with("dns "), "{progress}");
	}

	/// A gateway that takes each step only when told: the login waits at
	/// each, and tells which.
	#[tokio::test]
	async fn each_step_is_told_until_logged_in() {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let url = format!("ws://{}/v1", listener.local_addr().unwrap());
		let (go, mut steps) = mpsc::unbounded_channel::<()>();
		let server = tokio::spawn(async move {
			let (tcp, _) = listener.accept().await.unwrap();
			steps.recv().await;
			#[allow(clippy::result_large_err)] // tungstenite's callback type
			let callback = |_: &Request, mut response: Response| {
				response
					.headers_mut()
					.insert("Sec-WebSocket-Protocol", crate::SUBPROTOCOL.parse().unwrap());
				Ok(response)
			};
			let mut ws = tokio_tungstenite::accept_hdr_async(tcp, callback).await.unwrap();
			steps.recv().await;
			let hello = ServerMsg::Hello {
				gateway_id: "gw".into(),
				server_uid: "server".into(),
				server_name: "Server".into(),
				nonce: "n".into(),
				capabilities: Vec::new(),
			};
			let text = serde_json::to_string(&Envelope::new(hello)).unwrap();
			ws.send(Message::Text(text.into())).await.unwrap();
			let Some(Ok(Message::Text(auth))) = ws.next().await else { panic!("no login") };
			let auth: Envelope<ClientMsg> = serde_json::from_str(auth.as_str()).unwrap();
			assert!(matches!(auth.msg, ClientMsg::Resume { .. }));
			steps.recv().await;
			let ok = ServerMsg::AuthOk {
				uid: "me".into(),
				token: "t".into(),
				token_expires: 0,
				capabilities: Vec::new(),
			};
			let text = serde_json::to_string(&Envelope::with_id(auth.id.unwrap(), ok)).unwrap();
			ws.send(Message::Text(text.into())).await.unwrap();
			// Open until the client is done.
			while let Some(Ok(_)) = ws.next().await {}
		});
		let progress = Progress::default();
		let login = {
			let progress = progress.clone();
			tokio::spawn(async move {
				connect_watched(&url, Login::Token("t".into()), &progress).await.map(|(c, _)| c)
			})
		};
		reaches(&progress, Stage::WebSocket).await;
		go.send(()).unwrap();
		reaches(&progress, Stage::Hello).await;
		go.send(()).unwrap();
		reaches(&progress, Stage::Auth).await;
		assert!(progress.to_string().contains(", auth for "), "{progress}");
		go.send(()).unwrap();
		let client = login.await.unwrap().expect("logged in");
		assert_eq!(client.uid(), "me");
		assert_eq!(progress.stage(), Stage::Done);
		let shown = progress.to_string();
		let names: Vec<&str> =
			shown.split(", ").map(|step| step.split(' ').next().unwrap()).collect();
		assert_eq!(names, ["dns", "tcp", "websocket", "hello", "auth"], "{shown}");
		drop(client);
		server.await.unwrap();
	}
}
