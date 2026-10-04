//! Data carried by the extension messages: pins, reactions, topics, events,
//! the stream directory, the activity feed and administration.
//!
//! Enums that a newer gateway may extend have an `Unknown` fallback, so an
//! older client can still read the message. Kinds that are expected to grow
//! (activity) are plain strings.

use serde::{Deserialize, Serialize};
use voelin_model::{ChannelId, ChatTarget, ClientId};

use crate::messages::HistoryEntry;

/// Feature names in [`crate::ServerMsg::Hello`] and [`crate::ServerMsg::AuthOk`].
pub mod feature {
	/// Presence stream ([`crate::ClientMsg::SubscribePresence`]).
	pub const PRESENCE: &str = "presence";
	/// Channel and server chat through relays.
	pub const RELAY: &str = "relay";
	/// Stored chat history, [`crate::ClientMsg::QueryHistory`] and [`crate::ClientMsg::Sync`].
	pub const HISTORY: &str = "history";
	pub const PINS: &str = "pins";
	pub const REACTIONS: &str = "reactions";
	pub const TOPICS: &str = "topics";
	pub const EVENTS: &str = "events";
	pub const STREAMS: &str = "streams";
	pub const ACTIVITY: &str = "activity";
	/// Only in [`crate::ServerMsg::AuthOk`]: the user may change the gateway's
	/// configuration and permission rules.
	pub const ADMIN: &str = "admin";
}

/// Activity kinds in [`ActivityEntry::kind`]. Clients show unknown kinds by
/// their [`ActivityEntry::text`].
pub mod activity_kind {
	pub const STREAM_STARTED: &str = "stream_started";
	pub const STREAM_ENDED: &str = "stream_ended";
	pub const EVENT_CREATED: &str = "event_created";
	pub const EVENT_CANCELLED: &str = "event_cancelled";
	pub const EVENT_STARTING: &str = "event_starting";
	pub const TOPIC_CREATED: &str = "topic_created";
	pub const PINNED: &str = "pinned";
}

fn is_false(b: &bool) -> bool {
	!*b
}

/// A user as the gateway knows them: unique id and last nickname.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRef {
	pub uid: String,
	pub name: String,
}

/// One emoji on a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionCount {
	/// Any Unicode string, usually one emoji.
	pub emoji: String,
	pub count: u32,
	/// The requesting user reacted with this emoji.
	#[serde(default, skip_serializing_if = "is_false")]
	pub me: bool,
}

/// Parameters of [`crate::ClientMsg::QueryHistory`].
///
/// Without cursors the latest `limit` messages are returned. With `before`
/// (or `before_ms`) only, the `limit` messages right before it. With `after`
/// (or `after_ms`), the `limit` messages right after it, oldest first, up to
/// `before` if given. Pages are always ordered oldest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryQuery {
	pub target: ChatTarget,
	/// Messages with a smaller id.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub before: Option<i64>,
	/// Messages with a larger id.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub after: Option<i64>,
	/// Messages sent before this time (Unix milliseconds).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub before_ms: Option<i64>,
	/// Messages sent after this time (Unix milliseconds).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub after_ms: Option<i64>,
	/// Page size. Absent: everything in range, unless the gateway's admin set
	/// a maximum page size.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub limit: Option<u32>,
	/// Only the messages of this topic.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub topic: Option<i64>,
	/// Only messages outside topics.
	#[serde(default, skip_serializing_if = "is_false")]
	pub exclude_topics: bool,
}

impl HistoryQuery {
	/// The latest messages of `target`.
	pub fn latest(target: ChatTarget, limit: Option<u32>) -> Self {
		Self {
			target,
			before: None,
			after: None,
			before_ms: None,
			after_ms: None,
			limit,
			topic: None,
			exclude_topics: false,
		}
	}
}

/// Answer to [`crate::ClientMsg::QueryHistory`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
	pub target: ChatTarget,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub topic: Option<i64>,
	/// Oldest first.
	pub messages: Vec<HistoryEntry>,
	/// More messages exist beyond this page in the paging direction (older
	/// without `after`, newer with it).
	#[serde(default)]
	pub has_more: bool,
}

/// A pinned message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinInfo {
	pub entry: HistoryEntry,
	pub by: UserRef,
	pub ts_ms: i64,
}

/// A topic (thread) in a chat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicInfo {
	pub id: i64,
	pub target: ChatTarget,
	pub title: String,
	pub creator: UserRef,
	pub created_ms: i64,
	/// The message the topic was started from.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub root_message_id: Option<i64>,
	pub last_activity_ms: i64,
	pub message_count: u64,
	#[serde(default, skip_serializing_if = "is_false")]
	pub archived: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
	#[default]
	General,
	/// A scheduled stream; linked to the stream directory once live.
	Stream,
	/// Sent by a newer gateway.
	#[serde(other)]
	Unknown,
}

/// What the creator of an event sets.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSpec {
	pub title: String,
	#[serde(default)]
	pub description: String,
	/// Unix milliseconds.
	pub start_ms: i64,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub end_ms: Option<i64>,
	/// Server-wide when absent.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub channel: Option<ChannelId>,
	#[serde(default)]
	pub kind: EventKind,
	/// For streams: the expected stream title.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stream_title: Option<String>,
	/// For streams: the game or subject.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stream_game: Option<String>,
	/// Whose stream links to the event; the creator when absent.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub host_uid: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RsvpStatus {
	Going,
	Maybe,
	NotGoing,
	#[serde(other)]
	Unknown,
}

impl RsvpStatus {
	pub fn as_str(self) -> &'static str {
		match self {
			RsvpStatus::Going => "going",
			RsvpStatus::Maybe => "maybe",
			RsvpStatus::NotGoing => "not_going",
			RsvpStatus::Unknown => "unknown",
		}
	}

	pub fn parse(s: &str) -> Self {
		match s {
			"going" => RsvpStatus::Going,
			"maybe" => RsvpStatus::Maybe,
			"not_going" => RsvpStatus::NotGoing,
			_ => RsvpStatus::Unknown,
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attendee {
	pub user: UserRef,
	pub status: RsvpStatus,
	pub ts_ms: i64,
}

/// A scheduled event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventInfo {
	pub id: i64,
	#[serde(flatten)]
	pub spec: EventSpec,
	pub creator: UserRef,
	pub created_ms: i64,
	pub updated_ms: i64,
	#[serde(default)]
	pub going: u32,
	#[serde(default)]
	pub maybe: u32,
	#[serde(default)]
	pub not_going: u32,
	/// The requesting user's answer.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub my_rsvp: Option<RsvpStatus>,
	/// Everyone who answered; only in [`crate::ClientMsg::GetEvent`] replies.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub attendees: Vec<Attendee>,
	/// Directory id of the stream while the host is live.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub live_stream: Option<String>,
}

/// Parameters of [`crate::ClientMsg::ListEvents`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventQuery {
	/// Events that end (or, without an end, start) at or after this time.
	/// Default: now.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub from_ms: Option<i64>,
	/// Events that start before this time.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub to_ms: Option<i64>,
	/// Only events in this channel.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub channel: Option<ChannelId>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub limit: Option<u32>,
}

/// What a streamer registers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSpec {
	/// The TeamSpeak 6 stream id (from `notifystreamstarted`).
	pub stream_id: String,
	/// The streamer's voice client id, so the gateway can drop the entry
	/// when the client leaves or stops streaming.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub client_id: Option<ClientId>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub channel: Option<ChannelId>,
	#[serde(default)]
	pub title: String,
	/// Free form, e.g. `screen`, `window`, `camera`, `game`.
	#[serde(default)]
	pub kind: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub viewers: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamSource {
	/// Registered by the streamer's app; has a stream id.
	Registered,
	/// Seen by the gateway (`client_is_streaming`) without a registration;
	/// the stream id is unknown.
	Detected,
	#[serde(other)]
	Unknown,
}

/// An entry in the stream directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEntry {
	/// Directory id: the stream id when known, otherwise `client/<clid>`.
	pub id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub stream_id: Option<String>,
	pub streamer: UserRef,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub client_id: Option<ClientId>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub channel: Option<ChannelId>,
	#[serde(default)]
	pub title: String,
	#[serde(default)]
	pub kind: String,
	pub started_ms: i64,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub viewers: Option<u32>,
	pub source: StreamSource,
	/// The scheduled event this stream belongs to.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub event_id: Option<i64>,
}

/// An entry in the server's activity feed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActivityEntry {
	pub id: i64,
	pub ts_ms: i64,
	/// One of [`activity_kind`], or something newer.
	pub kind: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub actor: Option<UserRef>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub channel: Option<ChannelId>,
	/// Id of what the entry is about (message, topic, event or stream id).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub ref_id: Option<String>,
	/// Human-readable summary, for kinds the client does not know.
	pub text: String,
	/// Kind-specific details.
	#[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
	pub data: serde_json::Value,
}

/// Where a configuration value comes from, highest precedence first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
	/// Set at runtime ([`crate::ClientMsg::ConfigSet`]), stored in the gateway's database.
	Db,
	/// `tsgw --set key=value`.
	Cli,
	/// `TSGW_<KEY>` environment variable.
	Env,
	/// `tsgw.toml`.
	File,
	Default,
	#[serde(other)]
	Unknown,
}

/// One configuration key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigEntry {
	/// Dotted key, e.g. `history.retention_days`.
	pub key: String,
	/// Effective value (secrets are masked).
	pub value: serde_json::Value,
	pub source: ConfigSource,
	pub default: serde_json::Value,
	/// `bool`, `int`, `string`, `int_list`, `string_list` or `rule`.
	#[serde(rename = "type")]
	pub value_type: String,
	pub description: String,
	/// Only read from the file, environment or command line at start;
	/// cannot be set at runtime.
	#[serde(default, skip_serializing_if = "is_false")]
	pub bootstrap: bool,
}

/// Something a permission rule can allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
	/// React to messages.
	React,
	/// Pin and unpin messages.
	Pin,
	/// Start topics.
	CreateTopic,
	/// Schedule events.
	CreateEvent,
	/// Answer event invitations.
	Rsvp,
	/// Register streams in the directory.
	Stream,
	/// Edit and delete other users' topics, events and pins.
	Moderate,
	/// Change the gateway's configuration and permission rules.
	Admin,
	#[serde(other)]
	Unknown,
}

impl Action {
	/// Every action that rules can be set for.
	pub const ALL: [Action; 8] = [
		Action::React,
		Action::Pin,
		Action::CreateTopic,
		Action::CreateEvent,
		Action::Rsvp,
		Action::Stream,
		Action::Moderate,
		Action::Admin,
	];

	pub fn as_str(self) -> &'static str {
		match self {
			Action::React => "react",
			Action::Pin => "pin",
			Action::CreateTopic => "create_topic",
			Action::CreateEvent => "create_event",
			Action::Rsvp => "rsvp",
			Action::Stream => "stream",
			Action::Moderate => "moderate",
			Action::Admin => "admin",
			Action::Unknown => "unknown",
		}
	}

	pub fn parse(s: &str) -> Option<Self> {
		Self::ALL.into_iter().find(|a| a.as_str() == s)
	}
}

/// Who may do an action: members of any of the listed TeamSpeak server
/// groups, or of any listed channel group in the channel concerned, or
/// everyone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermRule {
	#[serde(default, skip_serializing_if = "is_false")]
	pub everyone: bool,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub server_groups: Vec<u64>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub channel_groups: Vec<u64>,
}

/// The rule for one action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermRuleInfo {
	pub action: Action,
	/// The configured rule; `None` when the built-in default applies.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub rule: Option<PermRule>,
	/// Where the rule comes from.
	pub source: ConfigSource,
	/// What the built-in default allows.
	pub default: String,
}
