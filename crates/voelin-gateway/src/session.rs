//! One WebSocket client.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, close_code};
use base64::prelude::*;
use rand::RngExt;
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;
use tracing::{Instrument, Level, debug, info, info_span, warn};
use voelin_gateway_proto::{ClientMsg, Envelope, ErrorCode, EventInfo, ServerMsg, feature};
use voelin_model::{ChatTarget, Presence};
use voelin_observer::ObserverEvent;

use crate::hub::{Denied, Feature, Hub, HubEvent, User};

/// Requests within a sliding window.
#[derive(Default)]
struct RateLimit(Vec<Instant>);

impl RateLimit {
	/// Count one request; `false` if over `max` per `window` (`max` 0: no limit).
	fn allow(&mut self, max: u64, window_secs: u64) -> bool {
		if max == 0 {
			return true;
		}
		let window = Duration::from_secs(window_secs);
		self.0.retain(|t| t.elapsed() < window);
		if self.0.len() as u64 >= max {
			return false;
		}
		self.0.push(Instant::now());
		true
	}
}

struct Session {
	hub: Arc<Hub>,
	nonce: String,
	user: Option<User>,
	presence: Option<(Presence, u64)>,
	open_chats: HashSet<ChatTarget>,
	/// Extension features the client asked for ([`ClientMsg::Enable`]).
	enabled: Option<HashSet<String>>,
	events: bool,
	streams: bool,
	activity: bool,
	/// Directory entries this session registered.
	own_streams: Vec<String>,
	posts: RateLimit,
	actions: RateLimit,
}

/// Enough of an envelope to answer one that did not parse, and to name it
/// in the log.
#[derive(Deserialize)]
struct RawEnvelope {
	id: Option<u64>,
	/// Any JSON value: a `type` that is no string must not cost the `id`.
	#[serde(rename = "type")]
	kind: Option<serde_json::Value>,
}

/// Requests that take longer are logged.
const SLOW: Duration = Duration::from_secs(1);

/// Where a connection comes from, for the log.
#[derive(Debug)]
pub struct Peer {
	/// Counts connections since the start.
	pub id: u64,
	/// The address the connection came from (a proxy's, behind one).
	pub addr: Option<SocketAddr>,
	pub forwarded_for: Option<String>,
	pub user_agent: Option<String>,
}

impl Peer {
	/// [`Peer::addr`] for the log.
	pub fn address(&self) -> String {
		self.addr.map_or_else(|| "unknown".to_string(), |a| a.to_string())
	}
}

/// Serve one connection until it closes or `stop` turns true.
pub async fn run(hub: Arc<Hub>, socket: WebSocket, peer: Peer, stop: watch::Receiver<bool>) {
	let span = info_span!("ws", id = peer.id, peer = %peer.address());
	serve(hub, socket, peer, stop).instrument(span).await;
}

async fn serve(hub: Arc<Hub>, mut socket: WebSocket, peer: Peer, mut stop: watch::Receiver<bool>) {
	let connected = Instant::now();
	info!(
		forwarded_for = peer.forwarded_for.as_deref(),
		user_agent = peer.user_agent.as_deref(),
		"websocket connected"
	);
	let nonce_bytes: [u8; 24] = rand::rng().random();
	let mut session = Session {
		nonce: BASE64_URL_SAFE_NO_PAD.encode(nonce_bytes),
		hub: hub.clone(),
		user: None,
		presence: None,
		open_chats: HashSet::new(),
		enabled: None,
		events: false,
		streams: false,
		activity: false,
		own_streams: Vec::new(),
		posts: RateLimit::default(),
		actions: RateLimit::default(),
	};
	let hello = ServerMsg::Hello {
		gateway_id: hub.gateway_id.clone(),
		server_uid: hub.server_uid.clone(),
		server_name: hub.server_name.clone(),
		nonce: session.nonce.clone(),
		capabilities: hub.capabilities(),
	};
	if send(&mut socket, None, hello).await.is_err() {
		info!(reason = "could not send", "websocket closed");
		return;
	}
	let mut observer = hub.observer.subscribe();
	let mut events = hub.subscribe();

	let reason = 'outer: loop {
		tokio::select! {
			msg = socket.recv() => {
				let text = match msg {
					Some(Ok(Message::Text(t))) => t,
					Some(Ok(Message::Close(_))) => break "closed by the app".to_string(),
					Some(Ok(_)) => continue,
					Some(Err(e)) => break format!("connection failed: {e}"),
					None => break "connection ended".to_string(),
				};
				let replies = match serde_json::from_str::<Envelope<ClientMsg>>(text.as_str()) {
					Ok(env) => {
						let started = Instant::now();
						let replies = session.handle(env.msg).await;
						log_request(text.as_str(), started.elapsed(), &replies);
						replies.into_iter().map(|m| (env.id, m)).collect()
					}
					Err(e) => {
						let id = serde_json::from_str::<RawEnvelope>(text.as_str()).ok().and_then(|r| r.id);
						let code = if e.to_string().starts_with("unknown variant") {
							ErrorCode::UnknownType
						} else {
							ErrorCode::BadRequest
						};
						vec![(id, error(code, &e.to_string()))]
					}
				};
				for (id, reply) in replies {
					if send(&mut socket, id, reply).await.is_err() {
						break 'outer "could not send".to_string();
					}
				}
			}
			event = observer.recv(), if session.presence.is_some() => {
				let resync = matches!(event, Err(RecvError::Lagged(_)) | Ok(ObserverEvent::Snapshot(_)));
				if let Err(RecvError::Closed) = event {
					break "the gateway stopped".to_string();
				}
				for msg in session.presence_update(resync) {
					if send(&mut socket, None, msg).await.is_err() {
						break 'outer "could not send".to_string();
					}
				}
			}
			event = events.recv() => {
				let event = match event {
					Ok(event) => event,
					Err(RecvError::Lagged(_)) => continue,
					Err(RecvError::Closed) => break "the gateway stopped".to_string(),
				};
				for msg in session.push(&event).await {
					if send(&mut socket, None, msg).await.is_err() {
						break 'outer "could not send".to_string();
					}
				}
			}
			// Turns true once, at shutdown.
			_ = stop.changed() => {
				// Apps connect again at once instead of finding out later.
				let frame = CloseFrame { code: close_code::AWAY, reason: "the gateway is restarting".into() };
				let _ = socket.send(Message::Close(Some(frame))).await;
				break "the gateway is shutting down".to_string();
			}
		}
	};
	info!(
		uid = session.user.as_ref().map(|u| u.uid.as_str()),
		connected_s = connected.elapsed().as_secs(),
		%reason,
		open_chats = session.open_chats.len(),
		"websocket closed"
	);
	session.close();
}

/// Log a request that failed on the gateway's side or took long, and every
/// request at debug level.
fn log_request(text: &str, elapsed: Duration, replies: &[ServerMsg]) {
	let failure = replies.iter().find_map(|m| match m {
		ServerMsg::Error {
			code: code @ (ErrorCode::Unavailable | ErrorCode::Internal),
			message,
		} => Some((code, message.as_str())),
		_ => None,
	});
	let slow = elapsed >= SLOW;
	if !slow && failure.is_none() && !tracing::enabled!(Level::DEBUG) {
		return;
	}
	let kind = serde_json::from_str::<RawEnvelope>(text).ok().and_then(|r| r.kind);
	let request = kind.as_ref().and_then(serde_json::Value::as_str).unwrap_or("unknown");
	let elapsed_ms = elapsed.as_millis() as u64;
	match failure {
		// Logged as a refused login.
		Some(_) if matches!(request, "auth" | "resume") => {}
		Some((code, error)) => warn!(request, ?code, error, elapsed_ms, "request failed"),
		None if slow => warn!(request, elapsed_ms, "slow request"),
		None => debug!(request, elapsed_ms, "request"),
	}
}

async fn send(socket: &mut WebSocket, id: Option<u64>, msg: ServerMsg) -> Result<(), axum::Error> {
	let env = Envelope { v: voelin_gateway_proto::VERSION, id, msg };
	let text = serde_json::to_string(&env).expect("messages serialize");
	socket.send(Message::Text(text.into())).await
}

fn error(code: ErrorCode, message: &str) -> ServerMsg {
	ServerMsg::Error { code, message: message.to_string() }
}

/// Changes that count toward `limits.actions_per_window`.
fn is_action(msg: &ClientMsg) -> bool {
	matches!(
		msg,
		ClientMsg::Pin { .. }
			| ClientMsg::Unpin { .. }
			| ClientMsg::React { .. }
			| ClientMsg::Unreact { .. }
			| ClientMsg::CreateTopic { .. }
			| ClientMsg::UpdateTopic { .. }
			| ClientMsg::CreateEvent { .. }
			| ClientMsg::UpdateEvent { .. }
			| ClientMsg::DeleteEvent { .. }
			| ClientMsg::Rsvp { .. }
			| ClientMsg::RegisterStream(_)
			| ClientMsg::UpdateStream { .. }
			| ClientMsg::UnregisterStream { .. }
	)
}

impl Session {
	async fn handle(&mut self, msg: ClientMsg) -> Vec<ServerMsg> {
		let login = match &msg {
			ClientMsg::Auth { .. } => Some("signature"),
			ClientMsg::Resume { .. } => Some("token"),
			_ => None,
		};
		let started = Instant::now();
		match self.handle_inner(msg).await {
			Ok(msgs) => msgs,
			Err(Denied(code, message)) => {
				if let Some(via) = login {
					let elapsed_ms = started.elapsed().as_millis() as u64;
					warn!(?code, %message, via, elapsed_ms, "login refused");
				}
				vec![error(code, &message)]
			}
		}
	}

	fn user(&self) -> Result<User, Denied> {
		self.user.clone().ok_or(Denied(ErrorCode::NotAuthenticated, "log in first".into()))
	}

	/// Extension pushes of `feature` go to this session.
	fn wants(&self, feature: Feature) -> bool {
		self.enabled.as_ref().is_some_and(|f| f.is_empty() || f.contains(feature.name()))
			&& self.hub.feature_enabled(feature)
	}

	async fn handle_inner(&mut self, msg: ClientMsg) -> Result<Vec<ServerMsg>, Denied> {
		let hub = self.hub.clone();
		let rt = hub.runtime();
		if is_action(&msg)
			&& !self.actions.allow(rt.limits.actions_per_window, rt.limits.action_window_secs)
		{
			return Err(Denied(ErrorCode::RateLimited, "slow down".into()));
		}
		let ok = || Ok(vec![ServerMsg::Ok]);
		match msg {
			ClientMsg::Ping => Ok(vec![ServerMsg::Pong]),
			ClientMsg::Auth { omega, key_offset, ts, signature, .. } => {
				let started = Instant::now();
				let (user, token, token_expires) =
					hub.authenticate(&omega, key_offset, ts, &signature, &self.nonce).await?;
				self.logged_in(user, token, token_expires, "signature", started).await
			}
			ClientMsg::Resume { token, .. } => {
				let started = Instant::now();
				let (user, token, token_expires) = hub.resume(&token).await?;
				self.logged_in(user, token, token_expires, "token", started).await
			}
			ClientMsg::SubscribePresence => {
				self.user()?;
				self.presence = Some((Presence::default(), 0));
				Ok(self.presence_update(true))
			}
			ClientMsg::UnsubscribePresence => {
				self.presence = None;
				ok()
			}
			ClientMsg::OpenChat { target } => {
				let user = self.user()?;
				if self.open_chats.contains(&target) {
					return ok();
				}
				hub.require_read(&user, &target).await?;
				if let ChatTarget::Channel(cid) = target {
					hub.add_reader(cid).await?;
				}
				debug!(uid = %user.uid, ?target, "chat opened");
				self.open_chats.insert(target);
				ok()
			}
			ClientMsg::CloseChat { target } => {
				if self.open_chats.remove(&target)
					&& let ChatTarget::Channel(cid) = target
				{
					hub.remove_reader(cid);
				}
				ok()
			}
			ClientMsg::SendChat { target, text } => {
				let user = self.user()?;
				if !self.posts.allow(rt.limits.posts_per_window, rt.limits.post_window_secs) {
					return Err(Denied(ErrorCode::Unavailable, "slow down".into()));
				}
				hub.post(&user, &target, &text, None).await?;
				ok()
			}
			ClientMsg::History { target, before, limit } => {
				let user = self.user()?;
				let messages = hub.history_v1(&user, &target, before, limit).await?;
				Ok(vec![ServerMsg::History { messages }])
			}
			ClientMsg::Enable { features } => {
				self.user()?;
				let all: Vec<String> = hub
					.capabilities()
					.into_iter()
					.filter(|c| {
						[feature::PINS, feature::REACTIONS, feature::TOPICS].contains(&c.as_str())
					})
					.collect();
				let chosen: HashSet<String> = if features.is_empty() {
					all.iter().cloned().collect()
				} else {
					features.into_iter().filter(|f| all.contains(f)).collect()
				};
				let mut list: Vec<String> = chosen.iter().cloned().collect();
				list.sort();
				self.enabled = Some(chosen);
				Ok(vec![ServerMsg::Enabled { features: list }])
			}
			ClientMsg::Permissions { channel } => {
				let user = self.user()?;
				let actions = hub.permissions(&user, channel).await?;
				Ok(vec![ServerMsg::Permissions { channel, actions }])
			}
			ClientMsg::QueryHistory(q) => {
				let user = self.user()?;
				Ok(vec![ServerMsg::HistoryPage(hub.history(&user, q).await?)])
			}
			ClientMsg::Sync { target, since_rev, limit } => {
				let user = self.user()?;
				let (messages, rev, has_more) = hub.sync(&user, &target, since_rev, limit).await?;
				Ok(vec![ServerMsg::SyncPage { target, messages, rev, has_more }])
			}
			ClientMsg::Post { target, text, topic } => {
				let user = self.user()?;
				if !self.posts.allow(rt.limits.posts_per_window, rt.limits.post_window_secs) {
					return Err(Denied(ErrorCode::RateLimited, "slow down".into()));
				}
				let entry = hub.post(&user, &target, &text, topic).await?;
				Ok(vec![ServerMsg::Posted { entry }])
			}
			ClientMsg::Pin { message_id } => {
				hub.pin(&self.user()?, message_id).await?;
				ok()
			}
			ClientMsg::Unpin { message_id } => {
				hub.unpin(&self.user()?, message_id).await?;
				ok()
			}
			ClientMsg::ListPins { target } => {
				let pins = hub.pins(&self.user()?, &target).await?;
				Ok(vec![ServerMsg::Pins { target, pins }])
			}
			ClientMsg::React { message_id, emoji } => {
				hub.react(&self.user()?, message_id, &emoji, true).await?;
				ok()
			}
			ClientMsg::Unreact { message_id, emoji } => {
				hub.react(&self.user()?, message_id, &emoji, false).await?;
				ok()
			}
			ClientMsg::Reactors { message_id, emoji } => {
				let users = hub.reactors(&self.user()?, message_id, &emoji).await?;
				Ok(vec![ServerMsg::Reactors { message_id, emoji, users }])
			}
			ClientMsg::CreateTopic { target, title, message_id } => {
				let topic = hub.create_topic(&self.user()?, &target, &title, message_id).await?;
				Ok(vec![ServerMsg::Topic { topic }])
			}
			ClientMsg::UpdateTopic { topic_id, title, archived } => {
				let topic =
					hub.update_topic(&self.user()?, topic_id, title.as_deref(), archived).await?;
				Ok(vec![ServerMsg::Topic { topic }])
			}
			ClientMsg::ListTopics { target, include_archived } => {
				let topics = hub.topics(&self.user()?, &target, include_archived).await?;
				Ok(vec![ServerMsg::Topics { target, topics }])
			}
			ClientMsg::CreateEvent { event } => {
				Ok(vec![ServerMsg::Event { event: hub.create_event(&self.user()?, event).await? }])
			}
			ClientMsg::UpdateEvent { id, event } => Ok(vec![ServerMsg::Event {
				event: hub.update_event(&self.user()?, id, event).await?,
			}]),
			ClientMsg::DeleteEvent { id } => {
				hub.delete_event(&self.user()?, id).await?;
				ok()
			}
			ClientMsg::GetEvent { id } => {
				Ok(vec![ServerMsg::Event { event: hub.get_event(&self.user()?, id).await? }])
			}
			ClientMsg::ListEvents(q) => {
				Ok(vec![ServerMsg::Events { events: hub.events(&self.user()?, q).await? }])
			}
			ClientMsg::Rsvp { event_id, status } => Ok(vec![ServerMsg::Event {
				event: hub.rsvp(&self.user()?, event_id, status).await?,
			}]),
			ClientMsg::SubscribeEvents => {
				self.user()?;
				hub.require(Feature::Events)?;
				self.events = true;
				ok()
			}
			ClientMsg::UnsubscribeEvents => {
				self.events = false;
				ok()
			}
			ClientMsg::RegisterStream(spec) => {
				let stream = hub.register_stream(&self.user()?, spec).await?;
				self.own_streams.push(stream.id.clone());
				Ok(vec![ServerMsg::Stream { stream }])
			}
			ClientMsg::UpdateStream { stream_id, title, viewers } => {
				let stream = hub.update_stream(&self.user()?, &stream_id, title, viewers).await?;
				Ok(vec![ServerMsg::Stream { stream }])
			}
			ClientMsg::UnregisterStream { stream_id } => {
				hub.unregister_stream(&self.user()?, &stream_id).await?;
				self.own_streams.retain(|s| *s != stream_id);
				ok()
			}
			ClientMsg::ListStreams => {
				let user = self.user()?;
				hub.require(Feature::Streams)?;
				Ok(vec![ServerMsg::Streams { streams: hub.streams_for(&user) }])
			}
			ClientMsg::SubscribeStreams => {
				let user = self.user()?;
				hub.require(Feature::Streams)?;
				self.streams = true;
				Ok(vec![ServerMsg::Streams { streams: hub.streams_for(&user) }])
			}
			ClientMsg::UnsubscribeStreams => {
				self.streams = false;
				ok()
			}
			ClientMsg::ListActivity { before, limit } => {
				let (entries, has_more) = hub.activity(&self.user()?, before, limit).await?;
				Ok(vec![ServerMsg::Activity { entries, has_more }])
			}
			ClientMsg::SubscribeActivity => {
				self.user()?;
				hub.require(Feature::Activity)?;
				self.activity = true;
				ok()
			}
			ClientMsg::UnsubscribeActivity => {
				self.activity = false;
				ok()
			}
			ClientMsg::ConfigList => {
				Ok(vec![ServerMsg::Config { entries: hub.config_list(&self.user()?).await? }])
			}
			ClientMsg::ConfigGet { key } => Ok(vec![ServerMsg::ConfigValue {
				entry: hub.config_get(&self.user()?, &key).await?,
			}]),
			ClientMsg::ConfigSet { key, value } => Ok(vec![ServerMsg::ConfigValue {
				entry: hub.config_set(&self.user()?, &key, value).await?,
			}]),
			ClientMsg::ConfigReset { key } => Ok(vec![ServerMsg::ConfigValue {
				entry: hub.config_reset(&self.user()?, &key).await?,
			}]),
			ClientMsg::ConfigReload => {
				Ok(vec![ServerMsg::Config { entries: hub.config_reload(&self.user()?).await? }])
			}
			ClientMsg::PermList => {
				Ok(vec![ServerMsg::PermRules { rules: hub.perm_list(&self.user()?).await? }])
			}
			ClientMsg::PermSet { action, rule } => Ok(vec![ServerMsg::PermRules {
				rules: hub.perm_set(&self.user()?, action, Some(rule)).await?,
			}]),
			ClientMsg::PermReset { action } => Ok(vec![ServerMsg::PermRules {
				rules: hub.perm_set(&self.user()?, action, None).await?,
			}]),
		}
	}

	async fn logged_in(
		&mut self,
		user: User,
		token: String,
		token_expires: i64,
		via: &str,
		started: Instant,
	) -> Result<Vec<ServerMsg>, Denied> {
		let capabilities = self.hub.user_capabilities(&user).await;
		let uid = user.uid.clone();
		let elapsed_ms = started.elapsed().as_millis() as u64;
		info!(uid = %user.uid, nickname = %user.nickname, via, elapsed_ms, "user logged in");
		self.user = Some(user);
		Ok(vec![ServerMsg::AuthOk { uid, token, token_expires, capabilities }])
	}

	/// What to send this session for a hub event.
	async fn push(&mut self, event: &HubEvent) -> Vec<ServerMsg> {
		let Some(user) = self.user.clone() else { return Vec::new() };
		let hub = &self.hub;
		let open = |target: &ChatTarget| self.open_chats.contains(target);
		let visible = |channel: Option<u64>| channel.is_none_or(|c| hub.channel_visible(&user, c));
		let msg = match event {
			HubEvent::Chat(entry) if open(&entry.message.target) => {
				if self.enabled.is_some() {
					ServerMsg::Message { entry: entry.clone() }
				} else {
					let mut message = entry.message.clone();
					// Show older clients which topic a post belongs to.
					if let Some(topic) = entry.topic_id.and_then(|t| hub.db.topic(t).ok().flatten())
					{
						message.text = hub
							.runtime()
							.topics
							.relay_format
							.replace("{topic}", &topic.title)
							.replace("{text}", &message.text);
					}
					ServerMsg::ChatEvent { id: entry.id, message }
				}
			}
			HubEvent::Pinned(pin)
				if self.wants(Feature::Pins) && open(&pin.entry.message.target) =>
			{
				ServerMsg::Pinned { target: pin.entry.message.target.clone(), pin: pin.clone() }
			}
			HubEvent::Unpinned { target, message_id, by }
				if self.wants(Feature::Pins) && open(target) =>
			{
				ServerMsg::Unpinned {
					target: target.clone(),
					message_id: *message_id,
					by: by.clone(),
				}
			}
			HubEvent::Reaction { target, message_id, emoji, user: by, added, count }
				if self.wants(Feature::Reactions) && open(target) =>
			{
				ServerMsg::Reaction {
					target: target.clone(),
					message_id: *message_id,
					emoji: emoji.clone(),
					user: by.clone(),
					added: *added,
					count: *count,
				}
			}
			HubEvent::Topic(topic) if self.wants(Feature::Topics) && open(&topic.target) => {
				ServerMsg::TopicUpdated { topic: topic.clone() }
			}
			HubEvent::Event(event) if self.events && visible(event.spec.channel) => {
				ServerMsg::EventUpdated { event: self.personal(event) }
			}
			HubEvent::EventDeleted { id, channel } if self.events && visible(*channel) => {
				ServerMsg::EventDeleted { id: *id }
			}
			HubEvent::Reminder { event, starts_in_ms }
				if self.events && visible(event.spec.channel) =>
			{
				ServerMsg::EventReminder {
					event: self.personal(event),
					starts_in_ms: *starts_in_ms,
				}
			}
			HubEvent::StreamStarted(stream) if self.streams && visible(stream.channel) => {
				ServerMsg::StreamStarted { stream: stream.clone() }
			}
			HubEvent::StreamUpdated(stream) if self.streams && visible(stream.channel) => {
				ServerMsg::StreamUpdated { stream: stream.clone() }
			}
			HubEvent::StreamEnded { id, channel, reason } if self.streams && visible(*channel) => {
				ServerMsg::StreamEnded { id: id.clone(), reason: reason.clone() }
			}
			HubEvent::Activity(entry) if self.activity && visible(entry.channel) => {
				ServerMsg::ActivityAdded { entry: entry.clone() }
			}
			HubEvent::Capabilities
				if self.enabled.is_some() || self.events || self.streams || self.activity =>
			{
				let capabilities = hub.user_capabilities(&user).await;
				ServerMsg::Capabilities { capabilities }
			}
			_ => return Vec::new(),
		};
		vec![msg]
	}

	/// An event with this user's own answer.
	fn personal(&self, event: &EventInfo) -> EventInfo {
		let uid = self.user.as_ref().map(|u| u.uid.as_str());
		self.hub.db.event(event.id, uid, false).ok().flatten().unwrap_or_else(|| event.clone())
	}

	/// Send what changed in this user's view of the server (or a snapshot).
	fn presence_update(&mut self, snapshot: bool) -> Vec<ServerMsg> {
		let Some(user) = &self.user else { return Vec::new() };
		let Some((last, seq)) = &mut self.presence else { return Vec::new() };
		let current = self.hub.filtered_presence(user);
		if snapshot {
			*seq = 0;
			*last = current.clone();
			return vec![ServerMsg::PresenceSnapshot { seq: 0, snapshot: current.snapshot() }];
		}
		let deltas = last.diff(&current);
		*last = current;
		deltas
			.into_iter()
			.map(|delta| {
				*seq += 1;
				ServerMsg::PresenceDelta { seq: *seq, delta }
			})
			.collect()
	}

	fn close(&mut self) {
		for target in self.open_chats.drain() {
			if let ChatTarget::Channel(cid) = target {
				self.hub.remove_reader(cid);
			}
		}
		self.hub.session_streams_closed(&self.own_streams);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_bad_request_keeps_its_id() {
		let raw = |text: &str| serde_json::from_str::<RawEnvelope>(text).unwrap();
		assert_eq!(raw(r#"{"id":7,"type":5}"#).id, Some(7));
		assert_eq!(raw(r#"{"id":8,"type":"ping","extra":[]}"#).id, Some(8));
		assert_eq!(raw(r#"{"type":"ping"}"#).id, None);
	}
}
