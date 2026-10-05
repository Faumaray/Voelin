//! The gateway source: presence and relayed chat through `tsgw`, and
//! everything else a gateway offers (history, pins, reactions, topics,
//! events, the stream directory, the activity feed, administration).
//!
//! [`GatewayClient`] (from `voelin-gateway-proto`, feature `client`) has one
//! async method per request and delivers pushes as [`Push`];
//! [`GatewayClient::capabilities`] says which features the gateway and user
//! have. The data types (`HistoryQuery`, `PinInfo`, `EventSpec`,
//! `StreamEntry`, …) are in `voelin_gateway_proto`. [`connect`] logs in
//! with the user's TeamSpeak identity.
//!
//! # In the engine
//!
//! [`crate::Command::ObserveGateway`] starts a session's gateway: invisible
//! presence, relayed chat, chat history ([`crate::history`]). The session
//! then takes [`crate::Command::Gateway`] with a [`GatewayRequest`] and
//! reports answers and pushes as [`crate::Event::Gateway`] with a
//! [`GatewayUpdate`]:
//!
//! - [`GatewayUpdate::Connected`] after login, with the capabilities
//!   ([`voelin_gateway_proto::feature`]; hide what is missing);
//!   [`GatewayUpdate::Capabilities`] when they change;
//!   [`GatewayUpdate::Disconnected`] when the gateway is gone.
//! - Each request is answered with its update (e.g. `Pins` for
//!   [`GatewayRequest::Pins`]), [`GatewayUpdate::Done`] for requests without
//!   data, or [`GatewayUpdate::Failed`] (gateway error code and message).
//! - Pushes: pins, reactions and topic changes of open chats, event
//!   changes and reminders, stream directory and activity changes. The
//!   engine subscribes to events, streams and activity when the gateway
//!   has them (the `Subscribe*` requests turn that off and on).
//! - Messages the gateway returns (posts, pins, topic history) and message
//!   changes (pins, reactions) are stored and also emitted as
//!   [`crate::Event::ChatHistory`], so the chat view updates by local id.
//!   Message ids in requests are the gateway's
//!   ([`crate::history::HistoryMessage::remote_id`]).
//! - Our own stream (TeamSpeak 6) registers itself in the directory when it
//!   goes live and the gateway has `streams` ([`GatewayUpdate::StreamRegistered`]),
//!   and is removed when it ends. The directory is also where the session's
//!   streams look for streams that started before we joined, next to the
//!   server's `requeststreaminfo`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use voelin_gateway_proto::client::Login;
pub use voelin_gateway_proto::client::{ClientError, GatewayClient, Push};
use voelin_gateway_proto::{
	Action, ActivityEntry, ClientMsg, ConfigEntry, ErrorCode, EventInfo, EventQuery, EventSpec,
	HistoryEntry, HistoryQuery, PermRule, PermRuleInfo, PinInfo, RsvpStatus, StreamEntry,
	StreamSpec, TopicInfo, UserRef,
};
use voelin_model::{ChannelId, ChatTarget, Presence};
use voelin_store::WriteOutcome;

use crate::Event;
use crate::history::{ChatCtx, HistoryMessage, HistorySource, gateway_message, store_target};

pub(crate) enum GatewayCmd {
	OpenChat(ChatTarget),
	CloseChat(ChatTarget),
	SendChat(ChatTarget, String),
	Stop,
}

pub(crate) enum GatewayEvent {
	/// Logged in at the URL; requests go through the client.
	Connected(GatewayClient, String),
	Presence(Box<Presence>),
	/// A push other than presence.
	Push(Box<Push>),
	/// The gateway could not be reached, or the connection was lost; the
	/// next attempt follows by itself.
	Retrying(String),
	/// Stopped, or the gateway refused the login (the reason).
	Disconnected(Option<String>),
}

/// Connect to a gateway (`ws://…/v1` or `wss://…/v1`) and log in with a
/// TeamSpeak identity.
pub async fn connect(
	url: &str,
	identity: &tsclientlib::Identity,
) -> Result<(GatewayClient, mpsc::UnboundedReceiver<Push>), ClientError> {
	let login = Login::Identity { key: identity.key().clone(), key_offset: identity.counter() };
	voelin_gateway_proto::client::connect(url, login).await
}

/// Observe through the first of `urls` (best first, as published) that
/// logs in.
pub(crate) async fn run(
	mut urls: Vec<String>,
	identity: tsclientlib::Identity,
	mut commands: mpsc::UnboundedReceiver<GatewayCmd>,
	events: mpsc::UnboundedSender<GatewayEvent>,
) {
	// A gateway that is away (down, restarting, a network change, a proxy
	// in front of it that fails) is tried again until the session stops
	// observing; only a refused login ends it. A round tries every URL in
	// turn without waiting; only a round that fails waits.
	let mut chats = Chats::default();
	let mut failures = 0;
	// What was logged per URL: once per cause, not every minute of a long
	// absence.
	let mut told: Vec<(String, String)> = Vec::new();
	let reason = 'observe: loop {
		let mut lost = None;
		let mut index = 0;
		while index < urls.len() {
			let url = urls[index].clone();
			match attempt(&url, &identity, &mut commands, &events, &mut chats).await {
				Ended::Stopped => break 'observe None,
				Ended::Refused(reason) => break 'observe Some(reason),
				Ended::Lost { reason, logged_in } => {
					let next = urls.get(index + 1);
					if told.iter().any(|(u, r)| *u == url && *r == reason) {
						debug!(%url, error = %reason, ?next, "gateway still unreachable");
					} else {
						warn!(%url, error = %reason, ?next, "gateway unreachable");
						told.retain(|(u, _)| *u != url);
						told.push((url.clone(), reason.clone()));
					}
					lost = Some(reason);
					if logged_in {
						// It worked: it is tried first again, after a wait.
						urls[..=index].rotate_right(1);
						failures = 0;
						break;
					}
					index += 1;
				}
			}
		}
		let Some(reason) = lost else { break None };
		failures += 1;
		let delay = retry_delay(failures);
		let _ = events.send(GatewayEvent::Retrying(reason));
		if !chats.wait(delay, &mut commands).await {
			break None;
		}
	};
	let _ = events.send(GatewayEvent::Disconnected(reason));
}

/// How one connection to the gateway ended.
enum Ended {
	/// The session stopped observing.
	Stopped,
	/// The gateway refused the login: trying again would not help.
	Refused(String),
	/// Not reached, or lost (after logging in): try again.
	Lost { reason: String, logged_in: bool },
}

/// One attempt to reach a gateway and log in (TCP, TLS, the WebSocket
/// upgrade, the login's own queries at the gateway) gives up after this
/// long, so a gateway that never answers does not hold up the next one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// The wait before the next attempt after `failures` failed ones in a row.
fn retry_delay(failures: u32) -> Duration {
	const SECONDS: [u64; 5] = [2, 5, 15, 30, 60];
	let index = (failures.max(1) as usize - 1).min(SECONDS.len() - 1);
	Duration::from_secs(SECONDS[index])
}

/// Messages written while the gateway is away, at most this many, go out
/// when it is back.
const MAX_UNSENT: usize = 64;

/// What a new connection restores: the chats open at the gateway, and
/// messages written while it was away.
#[derive(Default)]
struct Chats {
	open: Vec<ChatTarget>,
	unsent: Vec<(ChatTarget, String)>,
}

impl Chats {
	/// Note a chat command; it goes out when connected.
	fn note(&mut self, cmd: GatewayCmd) {
		match cmd {
			GatewayCmd::OpenChat(target) => {
				if !self.open.contains(&target) {
					self.open.push(target);
				}
			}
			GatewayCmd::CloseChat(target) => self.open.retain(|open| *open != target),
			GatewayCmd::SendChat(target, text) => {
				if self.unsent.len() < MAX_UNSENT {
					self.unsent.push((target, text));
				}
			}
			GatewayCmd::Stop => {}
		}
	}

	/// Carry out a chat command on a connected gateway.
	fn apply(&mut self, client: &GatewayClient, cmd: GatewayCmd) -> Result<(), ClientError> {
		match cmd {
			GatewayCmd::OpenChat(target) => {
				if !self.open.contains(&target) {
					self.open.push(target.clone());
				}
				client.send(ClientMsg::OpenChat { target })
			}
			GatewayCmd::CloseChat(target) => {
				self.open.retain(|open| *open != target);
				client.send(ClientMsg::CloseChat { target })
			}
			GatewayCmd::SendChat(target, text) => client.send(ClientMsg::SendChat { target, text }),
			GatewayCmd::Stop => Ok(()),
		}
	}

	/// Open the chats again on a new connection and send what waited.
	fn restore(&mut self, client: &GatewayClient) -> Result<(), ClientError> {
		for target in &self.open {
			client.send(ClientMsg::OpenChat { target: target.clone() })?;
		}
		for (target, text) in std::mem::take(&mut self.unsent) {
			client.send(ClientMsg::SendChat { target, text })?;
		}
		Ok(())
	}

	/// Wait `delay` before the next attempt, noting the commands that come
	/// meanwhile; false when the session stops observing.
	async fn wait(
		&mut self,
		delay: Duration,
		commands: &mut mpsc::UnboundedReceiver<GatewayCmd>,
	) -> bool {
		let sleep = tokio::time::sleep(delay);
		tokio::pin!(sleep);
		loop {
			tokio::select! {
				() = &mut sleep => return true,
				cmd = commands.recv() => match cmd {
					None | Some(GatewayCmd::Stop) => return false,
					Some(cmd) => self.note(cmd),
				},
			}
		}
	}
}

/// Features whose pushes for open chats need [`ClientMsg::Enable`].
const CHAT_EXTENSIONS: [&str; 4] = [
	voelin_gateway_proto::feature::HISTORY,
	voelin_gateway_proto::feature::PINS,
	voelin_gateway_proto::feature::REACTIONS,
	voelin_gateway_proto::feature::TOPICS,
];

/// One connection: log in, then relay until it ends.
async fn attempt(
	url: &str,
	identity: &tsclientlib::Identity,
	commands: &mut mpsc::UnboundedReceiver<GatewayCmd>,
	events: &mpsc::UnboundedSender<GatewayEvent>,
	chats: &mut Chats,
) -> Ended {
	let lost = |reason: String| Ended::Lost { reason, logged_in: false };
	let (client, mut pushes) =
		match tokio::time::timeout(CONNECT_TIMEOUT, connect(url, identity)).await {
			Ok(Ok(connected)) => connected,
			Ok(Err(ClientError::Gateway { code, message })) => {
				warn!(%url, ?code, %message, "gateway refused the login");
				return Ended::Refused(format!("gateway refused login: {code:?}: {message}"));
			}
			Ok(Err(e)) => return lost(e.to_string()),
			Err(_) => return lost(format!("no answer within {} s", CONNECT_TIMEOUT.as_secs())),
		};
	info!(%url, server_name = %client.info().server_name, capabilities = ?client.capabilities(), "gateway connected");
	let reason = match relay(url, &client, &mut pushes, commands, events, chats).await {
		Ok(Ended::Lost { reason, .. }) => reason,
		Ok(ended) => return ended,
		Err(e) => e.to_string(),
	};
	Ended::Lost { reason, logged_in: true }
}

async fn relay(
	url: &str,
	client: &GatewayClient,
	pushes: &mut mpsc::UnboundedReceiver<Push>,
	commands: &mut mpsc::UnboundedReceiver<GatewayCmd>,
	events: &mpsc::UnboundedSender<GatewayEvent>,
	chats: &mut Chats,
) -> Result<Ended, ClientError> {
	let lost = |reason: String| Ended::Lost { reason, logged_in: true };
	// Messages with their gateway ids, pins, reactions and topics of open
	// chats (older gateways answer `unknown_type` as a push; ignored).
	if CHAT_EXTENSIONS.iter().any(|f| client.has(f)) {
		client.send(ClientMsg::Enable { features: Vec::new() })?;
	}
	let _ = events.send(GatewayEvent::Connected(client.clone(), url.to_owned()));
	client.subscribe_presence()?;
	chats.restore(client)?;
	let mut presence = Presence::default();

	loop {
		tokio::select! {
			push = pushes.recv() => match push {
				None | Some(Push::Disconnected(None)) => {
					return Ok(lost("the gateway closed the connection".into()));
				}
				Some(Push::Disconnected(Some(reason))) => return Ok(lost(reason)),
				Some(Push::PresenceSnapshot(snapshot)) => {
					presence = Presence::from_snapshot(snapshot);
					let _ = events.send(GatewayEvent::Presence(Box::new(presence.clone())));
				}
				Some(Push::PresenceDelta(delta)) => {
					presence.apply(&delta);
					let _ = events.send(GatewayEvent::Presence(Box::new(presence.clone())));
				}
				// Answers to requests sent without waiting.
				Some(Push::Other(_) | Push::Error { code: ErrorCode::UnknownType, .. }) => {}
				// The gateway forgot the login (restarted): log in again.
				Some(Push::Error { code: ErrorCode::NotAuthenticated, message }) => {
					return Ok(lost(format!("gateway session lost: {message}")));
				}
				Some(push) => {
					let _ = events.send(GatewayEvent::Push(Box::new(push)));
				}
			},
			cmd = commands.recv() => match cmd {
				None | Some(GatewayCmd::Stop) => return Ok(Ended::Stopped),
				Some(cmd) => chats.apply(client, cmd)?,
			},
		}
	}
}

/// Something to ask a session's gateway ([`crate::Command::Gateway`]).
/// Message ids are the gateway's. Serialized in snake case, e.g.
/// `{"pin":{"message_id":7}}`, `"config_list"`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayRequest {
	/// Report the capabilities again ([`GatewayUpdate::Capabilities`]).
	Capabilities,
	/// What the user may do, server-wide or in `channel`
	/// ([`GatewayUpdate::Permissions`]).
	Permissions {
		#[serde(default)]
		channel: Option<ChannelId>,
	},
	/// Post into a chat, optionally into a topic ([`GatewayUpdate::Posted`]).
	Post {
		target: ChatTarget,
		text: String,
		#[serde(default)]
		topic: Option<i64>,
	},
	Pin {
		message_id: i64,
	},
	Unpin {
		message_id: i64,
	},
	/// [`GatewayUpdate::Pins`].
	Pins {
		target: ChatTarget,
	},
	React {
		message_id: i64,
		emoji: String,
	},
	Unreact {
		message_id: i64,
		emoji: String,
	},
	/// Who reacted with `emoji` ([`GatewayUpdate::Reactors`]).
	Reactors {
		message_id: i64,
		emoji: String,
	},
	/// Start a topic, from a message or standalone ([`GatewayUpdate::Topic`]).
	CreateTopic {
		target: ChatTarget,
		title: String,
		#[serde(default)]
		message_id: Option<i64>,
	},
	/// Rename or (un)archive a topic ([`GatewayUpdate::Topic`]).
	UpdateTopic {
		topic_id: i64,
		#[serde(default)]
		title: Option<String>,
		#[serde(default)]
		archived: Option<bool>,
	},
	/// [`GatewayUpdate::Topics`], most recently active first.
	Topics {
		target: ChatTarget,
		#[serde(default)]
		include_archived: bool,
	},
	/// A topic's messages before gateway message `before` (the latest
	/// without), `limit` of them (`chat.history_page` without)
	/// ([`GatewayUpdate::TopicHistory`]).
	TopicHistory {
		target: ChatTarget,
		topic: i64,
		#[serde(default)]
		before: Option<i64>,
		#[serde(default)]
		limit: Option<u32>,
	},
	/// [`GatewayUpdate::Event`].
	CreateEvent {
		event: EventSpec,
	},
	/// [`GatewayUpdate::Event`].
	UpdateEvent {
		id: i64,
		event: EventSpec,
	},
	DeleteEvent {
		id: i64,
	},
	/// One event with its attendees ([`GatewayUpdate::Event`]).
	GetEvent {
		id: i64,
	},
	/// [`GatewayUpdate::Events`], by start time.
	Events {
		#[serde(default)]
		query: EventQuery,
	},
	/// Answer an invitation; `None` withdraws the answer ([`GatewayUpdate::Event`]).
	Rsvp {
		event_id: i64,
		#[serde(default)]
		status: Option<RsvpStatus>,
	},
	/// Event changes and reminders as pushes (on by default).
	SubscribeEvents {
		on: bool,
	},
	/// The stream directory ([`GatewayUpdate::Streams`]).
	Streams,
	/// Directory changes as pushes (on by default).
	SubscribeStreams {
		on: bool,
	},
	/// Add a stream to the directory (our own stream registers itself;
	/// [`GatewayUpdate::StreamRegistered`]).
	RegisterStream {
		stream: StreamSpec,
	},
	UpdateStream {
		stream_id: String,
		#[serde(default)]
		title: Option<String>,
		#[serde(default)]
		viewers: Option<u32>,
	},
	UnregisterStream {
		stream_id: String,
	},
	/// Newest first, entries with an id below `before` ([`GatewayUpdate::Activity`]).
	Activity {
		#[serde(default)]
		before: Option<i64>,
		#[serde(default)]
		limit: Option<u32>,
	},
	/// New activity as pushes (on by default).
	SubscribeActivity {
		on: bool,
	},
	/// Administration (capability `admin`): [`GatewayUpdate::Config`].
	ConfigList,
	/// [`GatewayUpdate::ConfigValue`].
	ConfigGet {
		key: String,
	},
	/// Store a runtime value at the gateway ([`GatewayUpdate::ConfigValue`]).
	ConfigSet {
		key: String,
		value: serde_json::Value,
	},
	/// Drop the runtime value ([`GatewayUpdate::ConfigValue`]).
	ConfigReset {
		key: String,
	},
	/// Read the gateway's file again ([`GatewayUpdate::Config`]).
	ConfigReload,
	/// [`GatewayUpdate::PermRules`].
	PermList,
	PermSet {
		action: Action,
		rule: PermRule,
	},
	PermReset {
		action: Action,
	},
}

impl GatewayRequest {
	/// The request's name, as in [`GatewayUpdate::Done`] and [`GatewayUpdate::Failed`].
	pub fn name(&self) -> &'static str {
		match self {
			GatewayRequest::Capabilities => "capabilities",
			GatewayRequest::Permissions { .. } => "permissions",
			GatewayRequest::Post { .. } => "post",
			GatewayRequest::Pin { .. } => "pin",
			GatewayRequest::Unpin { .. } => "unpin",
			GatewayRequest::Pins { .. } => "pins",
			GatewayRequest::React { .. } => "react",
			GatewayRequest::Unreact { .. } => "unreact",
			GatewayRequest::Reactors { .. } => "reactors",
			GatewayRequest::CreateTopic { .. } => "create_topic",
			GatewayRequest::UpdateTopic { .. } => "update_topic",
			GatewayRequest::Topics { .. } => "topics",
			GatewayRequest::TopicHistory { .. } => "topic_history",
			GatewayRequest::CreateEvent { .. } => "create_event",
			GatewayRequest::UpdateEvent { .. } => "update_event",
			GatewayRequest::DeleteEvent { .. } => "delete_event",
			GatewayRequest::GetEvent { .. } => "get_event",
			GatewayRequest::Events { .. } => "events",
			GatewayRequest::Rsvp { .. } => "rsvp",
			GatewayRequest::SubscribeEvents { .. } => "subscribe_events",
			GatewayRequest::Streams => "streams",
			GatewayRequest::SubscribeStreams { .. } => "subscribe_streams",
			GatewayRequest::RegisterStream { .. } => "register_stream",
			GatewayRequest::UpdateStream { .. } => "update_stream",
			GatewayRequest::UnregisterStream { .. } => "unregister_stream",
			GatewayRequest::Activity { .. } => "activity",
			GatewayRequest::SubscribeActivity { .. } => "subscribe_activity",
			GatewayRequest::ConfigList => "config_list",
			GatewayRequest::ConfigGet { .. } => "config_get",
			GatewayRequest::ConfigSet { .. } => "config_set",
			GatewayRequest::ConfigReset { .. } => "config_reset",
			GatewayRequest::ConfigReload => "config_reload",
			GatewayRequest::PermList => "perm_list",
			GatewayRequest::PermSet { .. } => "perm_set",
			GatewayRequest::PermReset { .. } => "perm_reset",
		}
	}
}

/// A pinned message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Pin {
	/// The message as stored ([`HistoryMessage::id`] is the local id).
	pub message: HistoryMessage,
	pub by: UserRef,
	pub ts_ms: i64,
}

/// What a session's gateway reports ([`crate::Event::Gateway`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayUpdate {
	/// Logged in.
	Connected {
		/// Where: the first of the published URLs that logged in.
		url: String,
		gateway_id: String,
		server_uid: String,
		server_name: String,
		/// Our unique id on the server.
		uid: String,
		/// See [`voelin_gateway_proto::feature`].
		capabilities: Vec<String>,
	},
	/// The features available to us changed.
	Capabilities {
		capabilities: Vec<String>,
	},
	/// The gateway connection ended (nothing of it is available).
	Disconnected {
		reason: Option<String>,
	},
	/// A request without data succeeded (e.g. `pin`, `react`, `subscribe_*`).
	Done {
		request: String,
	},
	/// A request failed; `code` is the gateway's (`None`: not connected,
	/// connection lost, unexpected answer).
	Failed {
		request: String,
		code: Option<ErrorCode>,
		message: String,
	},
	Permissions {
		channel: Option<ChannelId>,
		actions: Vec<Action>,
	},
	/// Our post, as stored.
	Posted {
		message: HistoryMessage,
	},
	Pins {
		target: ChatTarget,
		pins: Vec<Pin>,
	},
	/// Push (chat open) or our pin.
	Pinned {
		target: ChatTarget,
		pin: Pin,
	},
	/// Push (chat open).
	Unpinned {
		target: ChatTarget,
		message_id: i64,
		by: UserRef,
	},
	/// Push (chat open): `user` added or removed `emoji`; `count` is the new total.
	Reaction {
		target: ChatTarget,
		message_id: i64,
		emoji: String,
		user: UserRef,
		added: bool,
		count: u32,
	},
	Reactors {
		message_id: i64,
		emoji: String,
		users: Vec<UserRef>,
	},
	/// A topic: created, changed (also as a push), or new activity.
	Topic {
		topic: TopicInfo,
	},
	Topics {
		target: ChatTarget,
		topics: Vec<TopicInfo>,
	},
	/// A page of a topic's messages, oldest first, stored.
	TopicHistory {
		target: ChatTarget,
		topic: i64,
		messages: Vec<HistoryMessage>,
		has_more: bool,
	},
	/// An event: the answer to create, update, get and rsvp, or a push
	/// (created or changed, RSVP counts, going live).
	Event {
		event: EventInfo,
	},
	Events {
		events: Vec<EventInfo>,
	},
	/// Push.
	EventDeleted {
		id: i64,
	},
	/// Push at the gateway's reminder times before an event starts (0: it starts).
	EventReminder {
		event: EventInfo,
		starts_in_ms: i64,
	},
	/// The whole directory (answer to `streams`, and after subscribing).
	Streams {
		streams: Vec<StreamEntry>,
	},
	/// Push.
	StreamStarted {
		stream: StreamEntry,
	},
	/// Push.
	StreamUpdated {
		stream: StreamEntry,
	},
	/// Push.
	StreamEnded {
		id: String,
		reason: String,
	},
	/// A stream we registered (our own stream when it goes live).
	StreamRegistered {
		stream: StreamEntry,
	},
	/// Newest first.
	Activity {
		entries: Vec<ActivityEntry>,
		has_more: bool,
	},
	/// Push.
	ActivityAdded {
		entry: ActivityEntry,
	},
	Config {
		entries: Vec<ConfigEntry>,
	},
	ConfigValue {
		entry: ConfigEntry,
	},
	PermRules {
		rules: Vec<PermRuleInfo>,
	},
}

/// Carry out a request and report the answer.
pub(crate) async fn execute(ctx: ChatCtx, client: GatewayClient, request: GatewayRequest) {
	let name = request.name();
	let update = match answer(&ctx, &client, request).await {
		Ok(Some(update)) => update,
		Ok(None) => GatewayUpdate::Done { request: name.into() },
		Err(e) => {
			GatewayUpdate::Failed { request: name.into(), code: e.code(), message: e.to_string() }
		}
	};
	ctx.emit(Event::Gateway { session: ctx.session, update });
}

/// Store gateway messages; as stored (local id 0 if storing failed).
async fn stored(ctx: &ChatCtx, entries: &[HistoryEntry]) -> Vec<HistoryMessage> {
	match ctx.store_entries(entries).await {
		Ok(written) => written.into_iter().map(|w| w.message.into()).collect(),
		Err(e) => {
			tracing::warn!("cannot store gateway messages: {e}");
			entries.iter().map(HistoryMessage::unstored).collect()
		}
	}
}

async fn answer(
	ctx: &ChatCtx,
	client: &GatewayClient,
	request: GatewayRequest,
) -> Result<Option<GatewayUpdate>, ClientError> {
	use GatewayRequest as R;
	use GatewayUpdate as U;
	Ok(Some(match request {
		R::Capabilities => U::Capabilities { capabilities: client.capabilities() },
		R::Permissions { channel } => {
			U::Permissions { channel, actions: client.permissions(channel).await? }
		}
		R::Post { target, text, topic } => {
			let entry = client.post(target.clone(), text, topic).await?;
			let message = stored(ctx, std::slice::from_ref(&entry)).await.remove(0);
			ctx.emit_batch(&target, vec![message.clone()], HistorySource::Live, false);
			U::Posted { message }
		}
		R::Pin { message_id } => return client.pin(message_id).await.map(|()| None),
		R::Unpin { message_id } => return client.unpin(message_id).await.map(|()| None),
		R::Pins { target } => {
			let pins = client.pins(target.clone()).await?;
			let entries: Vec<HistoryEntry> = pins.iter().map(|p| p.entry.clone()).collect();
			let messages = stored(ctx, &entries).await;
			let pins = pins
				.into_iter()
				.zip(messages)
				.map(|(p, message)| Pin { message, by: p.by, ts_ms: p.ts_ms })
				.collect();
			U::Pins { target, pins }
		}
		R::React { message_id, emoji } => {
			return client.react(message_id, emoji).await.map(|()| None);
		}
		R::Unreact { message_id, emoji } => {
			return client.unreact(message_id, emoji).await.map(|()| None);
		}
		R::Reactors { message_id, emoji } => {
			let users = client.reactors(message_id, emoji.clone()).await?;
			U::Reactors { message_id, emoji, users }
		}
		R::CreateTopic { target, title, message_id } => {
			U::Topic { topic: client.create_topic(target, title, message_id).await? }
		}
		R::UpdateTopic { topic_id, title, archived } => {
			U::Topic { topic: client.update_topic(topic_id, title, archived).await? }
		}
		R::Topics { target, include_archived } => {
			let topics = client.topics(target.clone(), include_archived).await?;
			U::Topics { target, topics }
		}
		R::TopicHistory { target, topic, before, limit } => {
			let mut query = HistoryQuery::latest(target.clone(), limit.or(ctx.page_limit()));
			query.before = before;
			query.topic = Some(topic);
			let page = client.history(query).await?;
			let messages = stored(ctx, &page.messages).await;
			U::TopicHistory { target, topic, messages, has_more: page.has_more }
		}
		R::CreateEvent { event } => U::Event { event: client.create_event(event).await? },
		R::UpdateEvent { id, event } => U::Event { event: client.update_event(id, event).await? },
		R::DeleteEvent { id } => return client.delete_event(id).await.map(|()| None),
		R::GetEvent { id } => U::Event { event: client.event(id).await? },
		R::Events { query } => U::Events { events: client.events(query).await? },
		R::Rsvp { event_id, status } => U::Event { event: client.rsvp(event_id, status).await? },
		R::SubscribeEvents { on: true } => return client.subscribe_events().await.map(|()| None),
		R::SubscribeEvents { on: false } => {
			return client.unsubscribe_events().await.map(|()| None);
		}
		R::Streams => U::Streams { streams: client.streams().await? },
		R::SubscribeStreams { on: true } => {
			U::Streams { streams: client.subscribe_streams().await? }
		}
		R::SubscribeStreams { on: false } => {
			return client.unsubscribe_streams().await.map(|()| None);
		}
		R::RegisterStream { stream } => {
			U::StreamRegistered { stream: client.register_stream(stream).await? }
		}
		R::UpdateStream { stream_id, title, viewers } => {
			U::StreamUpdated { stream: client.update_stream(stream_id, title, viewers).await? }
		}
		R::UnregisterStream { stream_id } => {
			return client.unregister_stream(stream_id).await.map(|()| None);
		}
		R::Activity { before, limit } => {
			let (entries, has_more) = client.activity(before, limit).await?;
			U::Activity { entries, has_more }
		}
		R::SubscribeActivity { on: true } => {
			return client.subscribe_activity().await.map(|()| None);
		}
		R::SubscribeActivity { on: false } => {
			return client.unsubscribe_activity().await.map(|()| None);
		}
		R::ConfigList => U::Config { entries: client.config_list().await? },
		R::ConfigGet { key } => U::ConfigValue { entry: client.config_get(key).await? },
		R::ConfigSet { key, value } => {
			U::ConfigValue { entry: client.config_set(key, value).await? }
		}
		R::ConfigReset { key } => U::ConfigValue { entry: client.config_reset(key).await? },
		R::ConfigReload => U::Config { entries: client.config_reload().await? },
		R::PermList => U::PermRules { rules: client.perm_list().await? },
		R::PermSet { action, rule } => U::PermRules { rules: client.perm_set(action, rule).await? },
		R::PermReset { action } => U::PermRules { rules: client.perm_reset(action).await? },
	}))
}

/// Pushes about messages of open chats: stored, then reported, in the
/// order they arrived (the writer thread runs jobs in order).
impl ChatCtx {
	pub(crate) fn pinned(&self, target: ChatTarget, pin: PinInfo) {
		let mut entry = pin.entry.clone();
		entry.pinned = true;
		let ctx = self.clone();
		self.history.write(
			self.memory(),
			vec![gateway_message(&self.server_uid, &entry)],
			self.tolerance_ms(),
			move |result| {
				let message = match result.ok().and_then(|mut w| w.pop()) {
					Some(w) => {
						let changed = w.outcome != WriteOutcome::Unchanged;
						let message = HistoryMessage::from(w.message);
						if changed {
							ctx.emit_batch(
								&target,
								vec![message.clone()],
								HistorySource::Live,
								false,
							);
						}
						message
					}
					None => HistoryMessage::unstored(&entry),
				};
				let pin = Pin { message, by: pin.by, ts_ms: pin.ts_ms };
				let update = GatewayUpdate::Pinned { target, pin };
				ctx.emit(Event::Gateway { session: ctx.session, update });
			},
		);
	}

	pub(crate) fn unpinned(&self, target: ChatTarget, message_id: i64, by: UserRef) {
		let (uid, key, ctx) = (self.server_uid.clone(), store_target(&target), self.clone());
		self.history.run_then(
			self.memory(),
			move |s| s.set_pinned(&uid, &key, message_id, false),
			move |result| {
				ctx.emit_changed(result.ok().flatten());
				let update = GatewayUpdate::Unpinned { target, message_id, by };
				ctx.emit(Event::Gateway { session: ctx.session, update });
			},
		);
	}

	/// A reaction push; `own_uid` tells whether it was ours.
	pub(crate) fn reaction(&self, push: ReactionPush, own_uid: Option<&str>) {
		let ReactionPush { target, message_id, emoji, user, added, count } = push;
		let me = (own_uid == Some(user.uid.as_str())).then_some(added);
		let (uid, key, ctx) = (self.server_uid.clone(), store_target(&target), self.clone());
		let e = emoji.clone();
		self.history.run_then(
			self.memory(),
			move |s| s.set_reaction(&uid, &key, message_id, &e, count, me),
			move |result| {
				ctx.emit_changed(result.ok().flatten());
				let update =
					GatewayUpdate::Reaction { target, message_id, emoji, user, added, count };
				ctx.emit(Event::Gateway { session: ctx.session, update });
			},
		);
	}
}

/// The fields of [`Push::Reaction`].
pub(crate) struct ReactionPush {
	pub target: ChatTarget,
	pub message_id: i64,
	pub emoji: String,
	pub user: UserRef,
	pub added: bool,
	pub count: u32,
}
