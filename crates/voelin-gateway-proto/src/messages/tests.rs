use serde_json::json;
use voelin_model::ClientInfo;

use super::*;

fn chat(text: &str) -> ChatMessage {
	ChatMessage {
		target: ChatTarget::Channel(5),
		author_name: "Alice".into(),
		author_uid: Some("uid-a".into()),
		author_id: None,
		text: text.into(),
		ts_ms: 1_700_000_000_123,
		via_relay: true,
	}
}

fn entry() -> HistoryEntry {
	HistoryEntry {
		id: 42,
		message: chat("hi"),
		topic_id: Some(3),
		reactions: vec![ReactionCount { emoji: "👍".into(), count: 2, me: true }],
		pinned: true,
		rev: 1_700_000_000_123_001,
	}
}

fn user() -> UserRef {
	UserRef { uid: "uid-a".into(), name: "Alice".into() }
}

fn topic() -> TopicInfo {
	TopicInfo {
		id: 3,
		target: ChatTarget::Channel(5),
		title: "Raid".into(),
		creator: user(),
		created_ms: 1,
		root_message_id: Some(42),
		last_activity_ms: 2,
		message_count: 7,
		archived: false,
	}
}

fn event() -> EventInfo {
	EventInfo {
		id: 9,
		spec: EventSpec {
			title: "Movie night".into(),
			description: "bring snacks".into(),
			start_ms: 1000,
			end_ms: Some(2000),
			channel: Some(5),
			kind: EventKind::Stream,
			stream_title: Some("Film".into()),
			stream_game: None,
			host_uid: None,
		},
		creator: user(),
		created_ms: 1,
		updated_ms: 2,
		going: 3,
		maybe: 1,
		not_going: 0,
		my_rsvp: Some(RsvpStatus::Going),
		attendees: vec![Attendee { user: user(), status: RsvpStatus::Going, ts_ms: 5 }],
		live_stream: Some("s-1".into()),
	}
}

fn stream() -> StreamEntry {
	StreamEntry {
		id: "s-1".into(),
		stream_id: Some("s-1".into()),
		streamer: user(),
		client_id: Some(7),
		channel: Some(5),
		title: "Coding".into(),
		kind: "screen".into(),
		started_ms: 10,
		viewers: Some(2),
		source: StreamSource::Registered,
		event_id: Some(9),
	}
}

fn activity() -> ActivityEntry {
	ActivityEntry {
		id: 1,
		ts_ms: 10,
		kind: activity_kind::STREAM_STARTED.into(),
		actor: Some(user()),
		channel: Some(5),
		ref_id: Some("s-1".into()),
		text: "Alice started streaming".into(),
		data: json!({"title": "Coding"}),
	}
}

fn config_entry() -> ConfigEntry {
	ConfigEntry {
		key: "history.retention_days".into(),
		value: json!(0),
		source: ConfigSource::Db,
		default: json!(0),
		value_type: "int".into(),
		description: "days".into(),
		bootstrap: false,
	}
}

fn rule() -> PermRule {
	PermRule { everyone: false, server_groups: vec![6], channel_groups: vec![5] }
}

fn roundtrip<M>(msg: M)
where
	M: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug + Clone,
{
	let env = Envelope::with_id(3, msg);
	let json = serde_json::to_string(&env).unwrap();
	let back: Envelope<M> = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{json}: {e}"));
	assert_eq!(back, env, "{json}");
}

#[test]
fn envelope_json_shape() {
	let env = Envelope::with_id(
		7,
		ClientMsg::SendChat { target: ChatTarget::Channel(5), text: "hi".into() },
	);
	let json = serde_json::to_string(&env).unwrap();
	assert_eq!(
		json,
		r#"{"v":1,"id":7,"type":"send_chat","data":{"target":{"kind":"channel","id":5},"text":"hi"}}"#
	);
	let back: Envelope<ClientMsg> = serde_json::from_str(&json).unwrap();
	assert_eq!(back, env);

	let ping = serde_json::to_string(&Envelope::new(ClientMsg::Ping)).unwrap();
	assert_eq!(ping, r#"{"v":1,"type":"ping"}"#);
	assert_eq!(
		serde_json::to_string(&Envelope::new(ServerMsg::Ok)).unwrap(),
		r#"{"v":1,"type":"ok"}"#
	);
}

#[test]
fn ignores_unknown_fields() {
	let json = r#"{"v":1,"type":"history","data":{"target":{"kind":"server"},"limit":5,"future":true},"extra":1}"#;
	let env: Envelope<ClientMsg> = serde_json::from_str(json).unwrap();
	assert_eq!(env.msg, ClientMsg::History { target: ChatTarget::Server, before: None, limit: 5 });
}

#[test]
fn every_client_message_roundtrips() {
	let target = ChatTarget::Channel(5);
	let msgs = vec![
		ClientMsg::Auth {
			omega: "o".into(),
			key_offset: 1,
			ts: 2,
			signature: "s".into(),
			nickname: "n".into(),
		},
		ClientMsg::Resume { token: "t".into(), nickname: "n".into() },
		ClientMsg::SubscribePresence,
		ClientMsg::UnsubscribePresence,
		ClientMsg::OpenChat { target: target.clone() },
		ClientMsg::CloseChat { target: ChatTarget::Server },
		ClientMsg::SendChat { target: target.clone(), text: "x".into() },
		ClientMsg::History { target: target.clone(), before: Some(3), limit: 10 },
		ClientMsg::Ping,
		ClientMsg::Enable { features: vec![feature::PINS.into()] },
		ClientMsg::Permissions { channel: Some(5) },
		ClientMsg::QueryHistory(HistoryQuery {
			target: target.clone(),
			before: Some(100),
			after: Some(10),
			before_ms: Some(5),
			after_ms: Some(1),
			limit: Some(500),
			topic: Some(3),
			exclude_topics: true,
		}),
		ClientMsg::QueryHistory(HistoryQuery::latest(ChatTarget::Server, None)),
		ClientMsg::Sync { target: target.clone(), since_rev: 77, limit: None },
		ClientMsg::Post { target: target.clone(), text: "t".into(), topic: Some(3) },
		ClientMsg::Pin { message_id: 1 },
		ClientMsg::Unpin { message_id: 1 },
		ClientMsg::ListPins { target: target.clone() },
		ClientMsg::React { message_id: 1, emoji: "🏳️‍🌈".into() },
		ClientMsg::Unreact { message_id: 1, emoji: "👍🏽".into() },
		ClientMsg::Reactors { message_id: 1, emoji: "👍".into() },
		ClientMsg::CreateTopic { target: target.clone(), title: "t".into(), message_id: Some(1) },
		ClientMsg::UpdateTopic { topic_id: 3, title: None, archived: Some(true) },
		ClientMsg::ListTopics { target: target.clone(), include_archived: true },
		ClientMsg::CreateEvent { event: event().spec },
		ClientMsg::UpdateEvent { id: 9, event: EventSpec::default() },
		ClientMsg::DeleteEvent { id: 9 },
		ClientMsg::GetEvent { id: 9 },
		ClientMsg::ListEvents(EventQuery {
			from_ms: Some(1),
			to_ms: None,
			channel: Some(5),
			limit: None,
		}),
		ClientMsg::Rsvp { event_id: 9, status: Some(RsvpStatus::Maybe) },
		ClientMsg::Rsvp { event_id: 9, status: None },
		ClientMsg::SubscribeEvents,
		ClientMsg::UnsubscribeEvents,
		ClientMsg::RegisterStream(StreamSpec {
			stream_id: "s-1".into(),
			client_id: Some(7),
			channel: Some(5),
			title: "t".into(),
			kind: "screen".into(),
			viewers: None,
		}),
		ClientMsg::UpdateStream {
			stream_id: "s-1".into(),
			title: Some("u".into()),
			viewers: Some(4),
		},
		ClientMsg::UnregisterStream { stream_id: "s-1".into() },
		ClientMsg::ListStreams,
		ClientMsg::SubscribeStreams,
		ClientMsg::UnsubscribeStreams,
		ClientMsg::ListActivity { before: Some(10), limit: Some(20) },
		ClientMsg::SubscribeActivity,
		ClientMsg::UnsubscribeActivity,
		ClientMsg::ConfigList,
		ClientMsg::ConfigGet { key: "a.b".into() },
		ClientMsg::ConfigSet { key: "a.b".into(), value: json!([1, 2]) },
		ClientMsg::ConfigReset { key: "a.b".into() },
		ClientMsg::ConfigReload,
		ClientMsg::PermList,
		ClientMsg::PermSet { action: Action::Pin, rule: rule() },
		ClientMsg::PermReset { action: Action::Admin },
	];
	for msg in msgs {
		roundtrip(msg);
	}
}

#[test]
fn every_server_message_roundtrips() {
	let target = ChatTarget::Channel(5);
	let msgs = vec![
		ServerMsg::Hello {
			gateway_id: "g".into(),
			server_uid: "s".into(),
			server_name: "n".into(),
			nonce: "x".into(),
			capabilities: vec![feature::PINS.into()],
		},
		ServerMsg::AuthOk {
			uid: "u".into(),
			token: "t".into(),
			token_expires: 5,
			capabilities: vec![feature::ADMIN.into()],
		},
		ServerMsg::PresenceSnapshot {
			seq: 0,
			snapshot: PresenceSnapshot {
				server_name: "s".into(),
				channels: vec![],
				clients: vec![ClientInfo { id: 1, nickname: "a".into(), ..Default::default() }],
			},
		},
		ServerMsg::PresenceDelta { seq: 1, delta: PresenceDelta::ClientLeft { id: 1 } },
		ServerMsg::ChatEvent { id: 1, message: chat("x") },
		ServerMsg::History { messages: vec![entry(), HistoryEntry::new(1, chat("y"))] },
		ServerMsg::Ok,
		ServerMsg::Error { code: ErrorCode::QuotaExceeded, message: "m".into() },
		ServerMsg::Pong,
		ServerMsg::Enabled { features: vec!["pins".into()] },
		ServerMsg::Capabilities { capabilities: vec!["events".into()] },
		ServerMsg::Permissions { channel: None, actions: vec![Action::React, Action::Moderate] },
		ServerMsg::HistoryPage(HistoryPage {
			target: target.clone(),
			topic: Some(3),
			messages: vec![entry()],
			has_more: true,
		}),
		ServerMsg::SyncPage {
			target: target.clone(),
			messages: vec![entry()],
			rev: 9,
			has_more: false,
		},
		ServerMsg::Message { entry: entry() },
		ServerMsg::Posted { entry: entry() },
		ServerMsg::Pinned {
			target: target.clone(),
			pin: PinInfo { entry: entry(), by: user(), ts_ms: 4 },
		},
		ServerMsg::Unpinned { target: target.clone(), message_id: 42, by: user() },
		ServerMsg::Pins {
			target: target.clone(),
			pins: vec![PinInfo { entry: entry(), by: user(), ts_ms: 4 }],
		},
		ServerMsg::Reaction {
			target: target.clone(),
			message_id: 42,
			emoji: "🎉".into(),
			user: user(),
			added: true,
			count: 3,
		},
		ServerMsg::Reactors { message_id: 42, emoji: "🎉".into(), users: vec![user()] },
		ServerMsg::Topic { topic: topic() },
		ServerMsg::TopicUpdated { topic: topic() },
		ServerMsg::Topics { target: target.clone(), topics: vec![topic()] },
		ServerMsg::Event { event: event() },
		ServerMsg::Events { events: vec![event()] },
		ServerMsg::EventUpdated { event: event() },
		ServerMsg::EventDeleted { id: 9 },
		ServerMsg::EventReminder { event: event(), starts_in_ms: 900_000 },
		ServerMsg::Stream { stream: stream() },
		ServerMsg::Streams { streams: vec![stream()] },
		ServerMsg::StreamStarted { stream: stream() },
		ServerMsg::StreamUpdated { stream: stream() },
		ServerMsg::StreamEnded { id: "s-1".into(), reason: "left".into() },
		ServerMsg::Activity { entries: vec![activity()], has_more: true },
		ServerMsg::ActivityAdded { entry: activity() },
		ServerMsg::Config { entries: vec![config_entry()] },
		ServerMsg::ConfigValue { entry: config_entry() },
		ServerMsg::PermRules {
			rules: vec![
				PermRuleInfo {
					action: Action::Pin,
					rule: Some(rule()),
					source: ConfigSource::Db,
					default: "moderators".into(),
				},
				PermRuleInfo {
					action: Action::React,
					rule: None,
					source: ConfigSource::Default,
					default: "everyone".into(),
				},
			],
		},
	];
	for msg in msgs {
		roundtrip(msg);
	}
}

#[test]
fn new_json_shapes() {
	let json = serde_json::to_value(Envelope::with_id(
		4,
		ClientMsg::QueryHistory(HistoryQuery {
			after: Some(10),
			limit: Some(2),
			..HistoryQuery::latest(ChatTarget::Channel(1), None)
		}),
	))
	.unwrap();
	assert_eq!(
		json,
		json!({"v":1,"id":4,"type":"query_history","data":{"target":{"kind":"channel","id":1},"after":10,"limit":2}})
	);
	// The event spec is flattened into the event.
	let json = serde_json::to_value(event()).unwrap();
	assert_eq!(json["title"], "Movie night");
	assert_eq!(json["kind"], "stream");
	assert_eq!(json["my_rsvp"], "going");
	let json =
		serde_json::to_value(ClientMsg::PermSet { action: Action::CreateTopic, rule: rule() })
			.unwrap();
	assert_eq!(
		json,
		json!({"type":"perm_set","data":{"action":"create_topic","rule":{"server_groups":[6],"channel_groups":[5]}}})
	);
	// Plain entries stay as small as before.
	let json = serde_json::to_value(HistoryEntry::new(1, chat("x"))).unwrap();
	assert_eq!(json.as_object().unwrap().keys().collect::<Vec<_>>(), ["id", "message"]);
}

#[test]
fn old_clients_read_extended_replies() {
	// What a client built against the first protocol version sees.
	#[derive(Deserialize)]
	#[serde(tag = "type", content = "data", rename_all = "snake_case")]
	enum OldServerMsg {
		AuthOk { uid: String },
		History { messages: Vec<OldEntry> },
	}
	#[derive(Deserialize)]
	struct OldEntry {
		id: i64,
	}
	let auth = serde_json::to_string(&ServerMsg::AuthOk {
		uid: "u".into(),
		token: "t".into(),
		token_expires: 1,
		capabilities: vec!["admin".into()],
	})
	.unwrap();
	assert!(
		matches!(serde_json::from_str(&auth).unwrap(), OldServerMsg::AuthOk { uid } if uid == "u")
	);
	let history = serde_json::to_string(&ServerMsg::History { messages: vec![entry()] }).unwrap();
	match serde_json::from_str(&history).unwrap() {
		OldServerMsg::History { messages } => assert_eq!(messages[0].id, 42),
		_ => panic!(),
	}
}

#[test]
fn newer_enum_values_do_not_break_parsing() {
	let msg: ServerMsg =
		serde_json::from_str(r#"{"type":"error","data":{"code":"from_the_future","message":"m"}}"#)
			.unwrap();
	assert_eq!(msg, ServerMsg::Error { code: ErrorCode::Unknown, message: "m".into() });
	let spec: EventSpec =
		serde_json::from_str(r#"{"title":"t","start_ms":1,"kind":"tournament"}"#).unwrap();
	assert_eq!(spec.kind, EventKind::Unknown);
	let a: Vec<Action> = serde_json::from_str(r#"["pin","launch_rockets"]"#).unwrap();
	assert_eq!(a, [Action::Pin, Action::Unknown]);
	assert_eq!(Action::parse("create_event"), Some(Action::CreateEvent));
	assert_eq!(Action::parse("unknown"), None);
	for a in Action::ALL {
		assert_eq!(serde_json::to_value(a).unwrap(), a.as_str());
	}
	for s in [RsvpStatus::Going, RsvpStatus::Maybe, RsvpStatus::NotGoing] {
		assert_eq!(RsvpStatus::parse(s.as_str()), s);
		assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
	}
}
