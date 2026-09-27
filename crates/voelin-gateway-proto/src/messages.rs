//! Messages in both directions.
//!
//! The first block of each enum is the original protocol; everything after
//! it was added later. A client only receives the newer pushes after it sent
//! [`ClientMsg::Enable`] (chat extensions) or the matching subscribe request,
//! so older clients never see a message type they do not know.

use serde::{Deserialize, Serialize};
use voelin_model::{ChannelId, ChatMessage, ChatTarget, PresenceDelta, PresenceSnapshot};

use crate::types::*;

/// Every frame: `{"v":1,"id":7,"type":"send_chat","data":{...}}`.
///
/// `id` is chosen by the client for requests; the gateway copies it into the
/// answer ([`ServerMsg::Ok`], [`ServerMsg::Error`], [`ServerMsg::History`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<M> {
	pub v: u32,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub id: Option<u64>,
	#[serde(flatten)]
	pub msg: M,
}

impl<M> Envelope<M> {
	pub fn new(msg: M) -> Self {
		Self { v: crate::VERSION, id: None, msg }
	}
	pub fn with_id(id: u64, msg: M) -> Self {
		Self { v: crate::VERSION, id: Some(id), msg }
	}
}

/// Client to gateway.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientMsg {
	/// Answer to [`ServerMsg::Hello`].
	Auth {
		/// Public key, TeamSpeak "omega" format (base64 libtomcrypt DER).
		omega: String,
		/// Hash-cash counter (`client_key_offset`) that gives the identity its level.
		key_offset: u64,
		/// Unix time in seconds, part of the signed challenge.
		ts: i64,
		/// Base64 DER ECDSA signature over [`crate::challenge`].
		signature: String,
		/// Nickname for relayed messages.
		nickname: String,
	},
	/// Log in again with a token from [`ServerMsg::AuthOk`] instead of a signature.
	Resume {
		token: String,
		nickname: String,
	},
	/// Start (or restart) the presence stream: a snapshot, then deltas.
	SubscribePresence,
	UnsubscribePresence,
	/// Start receiving a chat.
	OpenChat {
		target: ChatTarget,
	},
	CloseChat {
		target: ChatTarget,
	},
	SendChat {
		target: ChatTarget,
		text: String,
	},
	/// Up to `limit` messages before message id `before` (latest when absent).
	/// [`ClientMsg::QueryHistory`] has more ways to page.
	History {
		target: ChatTarget,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		before: Option<i64>,
		limit: u32,
	},
	Ping,

	// Extensions. Gateways without them answer `error` with `unknown_type`;
	// the capabilities in `hello` and `auth_ok` say what is there.
	/// Receive the extension pushes for open chats: [`ServerMsg::Message`]
	/// instead of [`ServerMsg::ChatEvent`], and pins, reactions and topics of
	/// the listed features (empty: all). Answer: [`ServerMsg::Enabled`].
	Enable {
		#[serde(default)]
		features: Vec<String>,
	},
	/// What this user may do, server-wide or in a channel. Answer:
	/// [`ServerMsg::Permissions`].
	Permissions {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		channel: Option<ChannelId>,
	},
	/// Page through stored messages. Answer: [`ServerMsg::HistoryPage`].
	QueryHistory(HistoryQuery),
	/// Messages of a chat that are new or changed (pins, reactions, topic)
	/// since revision `since_rev`, by revision. Answer: [`ServerMsg::SyncPage`].
	Sync {
		target: ChatTarget,
		#[serde(default)]
		since_rev: i64,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		limit: Option<u32>,
	},
	/// Like [`ClientMsg::SendChat`], optionally into a topic; answered with
	/// the stored message ([`ServerMsg::Posted`]).
	Post {
		target: ChatTarget,
		text: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		topic: Option<i64>,
	},
	Pin {
		message_id: i64,
	},
	Unpin {
		message_id: i64,
	},
	/// Answer: [`ServerMsg::Pins`].
	ListPins {
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
	/// Who reacted with `emoji`. Answer: [`ServerMsg::Reactors`].
	Reactors {
		message_id: i64,
		emoji: String,
	},
	/// Start a topic, from a message or standalone. Answer: [`ServerMsg::Topic`].
	CreateTopic {
		target: ChatTarget,
		title: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		message_id: Option<i64>,
	},
	/// Rename or (un)archive a topic. Answer: [`ServerMsg::Topic`].
	UpdateTopic {
		topic_id: i64,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		title: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		archived: Option<bool>,
	},
	/// Answer: [`ServerMsg::Topics`], most recently active first.
	ListTopics {
		target: ChatTarget,
		#[serde(default)]
		include_archived: bool,
	},
	/// Answer: [`ServerMsg::Event`].
	CreateEvent {
		event: EventSpec,
	},
	/// Answer: [`ServerMsg::Event`].
	UpdateEvent {
		id: i64,
		event: EventSpec,
	},
	DeleteEvent {
		id: i64,
	},
	/// One event with its attendees. Answer: [`ServerMsg::Event`].
	GetEvent {
		id: i64,
	},
	/// Answer: [`ServerMsg::Events`], by start time.
	ListEvents(EventQuery),
	/// Answer an invitation; `None` withdraws the answer. Answer: [`ServerMsg::Event`].
	Rsvp {
		event_id: i64,
		#[serde(default)]
		status: Option<RsvpStatus>,
	},
	/// Receive [`ServerMsg::EventUpdated`], [`ServerMsg::EventDeleted`] and
	/// [`ServerMsg::EventReminder`].
	SubscribeEvents,
	UnsubscribeEvents,
	/// Add the user's running stream to the directory. Answer: [`ServerMsg::Stream`].
	RegisterStream(StreamSpec),
	/// Answer: [`ServerMsg::Stream`].
	UpdateStream {
		stream_id: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		title: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		viewers: Option<u32>,
	},
	UnregisterStream {
		stream_id: String,
	},
	/// Answer: [`ServerMsg::Streams`].
	ListStreams,
	/// Answered with [`ServerMsg::Streams`], then [`ServerMsg::StreamStarted`],
	/// [`ServerMsg::StreamUpdated`] and [`ServerMsg::StreamEnded`] as they happen.
	SubscribeStreams,
	UnsubscribeStreams,
	/// Newest first, entries with an id below `before`. Answer: [`ServerMsg::Activity`].
	ListActivity {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		before: Option<i64>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		limit: Option<u32>,
	},
	/// Receive [`ServerMsg::ActivityAdded`].
	SubscribeActivity,
	UnsubscribeActivity,
	/// Administration (needs the `admin` capability). Answer: [`ServerMsg::Config`].
	ConfigList,
	/// Answer: [`ServerMsg::ConfigValue`].
	ConfigGet {
		key: String,
	},
	/// Store a value in the gateway's database; it wins over the command
	/// line, environment and file and applies at once. Answer:
	/// [`ServerMsg::ConfigValue`].
	ConfigSet {
		key: String,
		value: serde_json::Value,
	},
	/// Drop the stored value (back to command line, environment, file or
	/// default). Answer: [`ServerMsg::ConfigValue`].
	ConfigReset {
		key: String,
	},
	/// Read the file again (like `SIGHUP`). Answer: [`ServerMsg::Config`].
	ConfigReload,
	/// Answer: [`ServerMsg::PermRules`].
	PermList,
	/// Set the rule of an action. Answer: [`ServerMsg::PermRules`].
	PermSet {
		action: Action,
		rule: PermRule,
	},
	/// Back to the file's rule or the built-in default. Answer: [`ServerMsg::PermRules`].
	PermReset {
		action: Action,
	},
}

/// Gateway to client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerMsg {
	/// First message after connecting.
	Hello {
		gateway_id: String,
		server_uid: String,
		server_name: String,
		nonce: String,
		/// Enabled features, see [`crate::feature`].
		#[serde(default)]
		capabilities: Vec<String>,
	},
	AuthOk {
		uid: String,
		/// Opaque; send it in [`ClientMsg::Resume`].
		token: String,
		/// Unix time in seconds.
		token_expires: i64,
		/// Features for this user: those of `hello`, plus `admin` if the user
		/// may administer the gateway.
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		capabilities: Vec<String>,
	},
	/// Presence restricted to what this user may see. `seq` restarts at 0 with
	/// each snapshot and increases by one per delta.
	PresenceSnapshot {
		seq: u64,
		snapshot: PresenceSnapshot,
	},
	PresenceDelta {
		seq: u64,
		delta: PresenceDelta,
	},
	/// A message in an open chat. `id` orders messages and pages history.
	ChatEvent {
		id: i64,
		message: ChatMessage,
	},
	History {
		messages: Vec<HistoryEntry>,
	},
	/// Request succeeded.
	Ok,
	Error {
		code: ErrorCode,
		message: String,
	},
	Pong,

	// Extensions: answers to the new requests, and pushes (without `id`) for
	// sessions that asked for them.
	Enabled {
		features: Vec<String>,
	},
	/// Push: the features available to this user changed.
	Capabilities {
		capabilities: Vec<String>,
	},
	Permissions {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		channel: Option<ChannelId>,
		actions: Vec<Action>,
	},
	HistoryPage(HistoryPage),
	SyncPage {
		target: ChatTarget,
		/// By revision.
		messages: Vec<HistoryEntry>,
		/// Pass as `since_rev` next time.
		rev: i64,
		#[serde(default)]
		has_more: bool,
	},
	/// Push: a message in an open chat, for sessions that sent [`ClientMsg::Enable`].
	Message {
		entry: HistoryEntry,
	},
	Posted {
		entry: HistoryEntry,
	},
	/// Push to sessions with the chat open.
	Pinned {
		target: ChatTarget,
		pin: PinInfo,
	},
	/// Push to sessions with the chat open.
	Unpinned {
		target: ChatTarget,
		message_id: i64,
		by: UserRef,
	},
	Pins {
		target: ChatTarget,
		pins: Vec<PinInfo>,
	},
	/// Push to sessions with the chat open: `user` added or removed `emoji`;
	/// `count` is the new total for that emoji.
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
	Topic {
		topic: TopicInfo,
	},
	/// Push to sessions with the chat open: created, renamed, archived or
	/// new activity.
	TopicUpdated {
		topic: TopicInfo,
	},
	Topics {
		target: ChatTarget,
		topics: Vec<TopicInfo>,
	},
	Event {
		event: EventInfo,
	},
	Events {
		events: Vec<EventInfo>,
	},
	/// Push: created or changed (including RSVP counts and going live).
	EventUpdated {
		event: EventInfo,
	},
	/// Push.
	EventDeleted {
		id: i64,
	},
	/// Push at the configured times before an event starts (0: it starts).
	EventReminder {
		event: EventInfo,
		starts_in_ms: i64,
	},
	Stream {
		stream: StreamEntry,
	},
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
	/// Newest first.
	Activity {
		entries: Vec<ActivityEntry>,
		#[serde(default)]
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

/// A stored message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
	/// Stable gateway id; orders the messages of a chat.
	pub id: i64,
	pub message: ChatMessage,
	/// The topic the message was posted in.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub topic_id: Option<i64>,
	/// In order of the first reaction.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub reactions: Vec<ReactionCount>,
	#[serde(default, skip_serializing_if = "is_false")]
	pub pinned: bool,
	/// Revision of the last change (post, pin, reaction, topic), for
	/// [`ClientMsg::Sync`].
	#[serde(default, skip_serializing_if = "is_zero")]
	pub rev: i64,
}

impl HistoryEntry {
	pub fn new(id: i64, message: ChatMessage) -> Self {
		Self { id, message, topic_id: None, reactions: Vec::new(), pinned: false, rev: 0 }
	}
}

fn is_false(b: &bool) -> bool {
	!*b
}

fn is_zero(n: &i64) -> bool {
	*n == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
	/// Malformed or unexpected message.
	BadRequest,
	/// Signature, timestamp or token invalid.
	AuthFailed,
	/// The server's database has no client with this unique id: connect
	/// with voice once first.
	UnknownIdentity,
	/// Identity security level below what the server requires.
	LevelTooLow,
	/// The user is banned on the server.
	Banned,
	/// Not allowed by the user's server permissions or the gateway config.
	Forbidden,
	NotAuthenticated,
	/// Temporary problem talking to the TeamSpeak server.
	Unavailable,
	Internal,
	/// The message type is not known to this gateway.
	UnknownType,
	/// The message, topic, event, stream or key does not exist (any more).
	NotFound,
	/// A quota set by the gateway's admin is reached.
	QuotaExceeded,
	/// Too many requests; try again later.
	RateLimited,
	/// The feature is turned off on this gateway.
	FeatureDisabled,
	/// Sent by a newer gateway.
	#[serde(other)]
	Unknown,
}

#[cfg(test)]
mod tests;
