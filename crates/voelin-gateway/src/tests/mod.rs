//! The gateway in-process: hub, observer, relays and sessions against a
//! fake ServerQuery server, driven by the typed client over a real
//! WebSocket.

mod fake;
mod live;

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;
use tsproto_types::crypto::EccKeyPrivP256;
use voelin_gateway_proto::client::{ClientError, GatewayClient, Login, Push, connect};
use voelin_gateway_proto::{
	Action, ErrorCode, EventKind, EventSpec, HistoryQuery, PermRule, RsvpStatus, StreamSource,
	StreamSpec, UniqueIds, feature,
};
use voelin_model::ChatTarget;

use self::fake::{FakeServer, Online, VIRTUALSERVER_MODIFY_NAME};
use crate::config::{Layers, file_values};
use crate::hub::{Hub, now_ms};

const LOBBY: ChatTarget = ChatTarget::Channel(1);

struct Fixture {
	fake: FakeServer,
	boot: crate::config::Bootstrap,
	hub: Arc<Hub>,
	url: String,
	dir: std::path::PathBuf,
	people: [Person; 3],
}

struct Person {
	key: EccKeyPrivP256,
	uid: String,
}

impl Person {
	fn new() -> Self {
		let key = EccKeyPrivP256::create();
		let uid = UniqueIds::from_omega(&key.to_pub().to_ts()).ts3;
		Self { key, uid }
	}

	async fn connect(&self, fx: &Fixture) -> (GatewayClient, mpsc::UnboundedReceiver<Push>) {
		connect(&fx.url, Login::Identity { key: self.key.clone(), key_offset: 0 }).await.unwrap()
	}
}

impl Fixture {
	async fn start(name: &str, extra: &str) -> Self {
		let fake = FakeServer::start().await;
		let dir = std::env::temp_dir().join(format!("tsgw-it-{name}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let toml = format!(
			"[query]\ntransport = \"raw\"\naddr = \"{}\"\nallowlisted = true\n\
			 [history]\npath = \"{}\"\n{extra}",
			fake.addr,
			dir.join("tsgw.db").display()
		);
		// Alice (admin, online in the lobby as client 10), Bob (online as
		// 11, server group 7) and Carol (not online).
		let people = [Person::new(), Person::new(), Person::new()];
		let [alice, bob, carol] = &people;
		fake.add_known(&alice.uid, 1, "Alice", &[6]);
		fake.add_known(&bob.uid, 2, "Bob", &[7]);
		fake.add_known(&carol.uid, 3, "Carol", &[8]);
		fake.grant(1, VIRTUALSERVER_MODIFY_NAME, 1);
		for (clid, p, nick, groups) in [(10, alice, "Alice", vec![6]), (11, bob, "Bob", vec![7])] {
			fake.add_online(Online {
				clid,
				cid: 1,
				uid: p.uid.clone(),
				nickname: nick.into(),
				server_groups: groups,
				streaming: false,
			});
		}
		let layers = Layers { file: file_values(&toml).unwrap(), ..Default::default() };
		let boot = layers.bootstrap().unwrap();
		let hub = Hub::start(layers).await.unwrap();
		// Wait for the observer's first snapshot.
		for _ in 0..100 {
			if !hub.observer.presence().read().unwrap().clients.is_empty() {
				break;
			}
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let url = format!("ws://{}/v1", listener.local_addr().unwrap());
		let app = crate::router(crate::App::started(boot.clone(), hub.clone()))
			.into_make_service_with_connect_info::<std::net::SocketAddr>();
		tokio::spawn(async move { axum::serve(listener, app).await });
		Self { fake, boot, hub, url, dir, people }
	}

	fn people(&self) -> (&Person, &Person, &Person) {
		let [a, b, c] = &self.people;
		(a, b, c)
	}
}

impl Drop for Fixture {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.dir);
	}
}

/// The next push matching `pred`, skipping others.
async fn expect(
	rx: &mut mpsc::UnboundedReceiver<Push>,
	what: &str,
	pred: impl Fn(&Push) -> bool,
) -> Push {
	let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
	loop {
		match tokio::time::timeout_at(deadline, rx.recv()).await {
			Ok(Some(push)) if pred(&push) => return push,
			Ok(Some(_)) => {}
			Ok(None) => panic!("connection closed waiting for {what}"),
			Err(_) => panic!("timed out waiting for {what}"),
		}
	}
}

/// No push matching `pred` arrives for a moment.
async fn expect_none(rx: &mut mpsc::UnboundedReceiver<Push>, pred: impl Fn(&Push) -> bool) {
	let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
	while let Ok(Some(push)) = tokio::time::timeout_at(deadline, rx.recv()).await {
		assert!(!pred(&push), "unexpected {push:?}");
	}
}

fn code(result: Result<impl std::fmt::Debug, ClientError>) -> ErrorCode {
	match result {
		Err(e) => e.code().unwrap_or_else(|| panic!("{e}")),
		Ok(v) => panic!("expected an error, got {v:?}"),
	}
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_pins_reactions_topics() {
	let fx = Fixture::start("chat", "").await;
	let (alice, bob, carol) = fx.people();
	let (a, mut a_rx) = alice.connect(&fx).await;
	let (b, mut b_rx) = bob.connect(&fx).await;
	let (c, mut c_rx) = carol.connect(&fx).await;
	for f in [feature::PINS, feature::REACTIONS, feature::TOPICS, feature::EVENTS, feature::STREAMS]
	{
		assert!(b.has(f), "{f}");
	}
	assert!(a.has(feature::ADMIN));
	assert!(!b.has(feature::ADMIN));
	assert_eq!(b.enable(Vec::new()).await.unwrap(), ["pins", "reactions", "topics"]);
	a.enable(vec!["pins".into(), "reactions".into(), "topics".into()]).await.unwrap();
	// Carol is an older client: no enable.
	for client in [&a, &b, &c] {
		client.open_chat(LOBBY).await.unwrap();
	}

	let hello = b.post(LOBBY, "hello".into(), None).await.unwrap();
	assert_eq!((hello.message.author_name.as_str(), hello.message.text.as_str()), ("Bob", "hello"));
	assert!(hello.id > 0 && hello.rev > 0);
	let Push::Message(got) = expect(&mut a_rx, "message", |p| matches!(p, Push::Message(_))).await
	else {
		unreachable!()
	};
	assert_eq!(got.id, hello.id);
	let Push::Chat { id, message } =
		expect(&mut c_rx, "chat event", |p| matches!(p, Push::Chat { .. })).await
	else {
		unreachable!()
	};
	assert_eq!((id, message.text.as_str()), (hello.id, "hello"));
	// Under Bob's name: "Bob1", as Bob is online with voice as "Bob". The
	// relay takes its own name back after the post.
	assert!(fx.fake.posted().contains(&(2, 1, "hello".into())));
	assert!(fx.fake.posted_by().contains(&("Bob1".into(), "hello".into())));
	assert_eq!(fx.fake.state.lock().unwrap().nicknames.last().unwrap(), "Chat Relay 1");

	// Pins: moderators only by default; Alice is an admin.
	assert_eq!(code(b.pin(hello.id).await), ErrorCode::Forbidden);
	a.pin(hello.id).await.unwrap();
	let Push::Pinned { pin, .. } =
		expect(&mut b_rx, "pinned", |p| matches!(p, Push::Pinned { .. })).await
	else {
		unreachable!()
	};
	assert_eq!((pin.entry.id, pin.by.name.as_str()), (hello.id, "Alice"));
	assert_eq!(b.pins(LOBBY).await.unwrap().len(), 1);
	expect_none(&mut c_rx, |p| matches!(p, Push::Pinned { .. })).await;

	// Reactions: any emoji, counts, who reacted.
	b.react(hello.id, "🦀".into()).await.unwrap();
	a.react(hello.id, "🦀".into()).await.unwrap();
	let push =
		expect(&mut b_rx, "reaction", |p| matches!(p, Push::Reaction { count: 2, .. })).await;
	assert!(matches!(push, Push::Reaction { added: true, ref emoji, .. } if emoji == "🦀"));
	assert_eq!(b.reactors(hello.id, "🦀".into()).await.unwrap().len(), 2);
	assert_eq!(code(b.react(hello.id, "no spaces".into()).await), ErrorCode::BadRequest);
	let page = b.history(HistoryQuery::latest(LOBBY, None)).await.unwrap();
	let entry = page.messages.iter().find(|e| e.id == hello.id).unwrap();
	assert!(entry.pinned);
	assert_eq!((entry.reactions[0].count, entry.reactions[0].me), (2, true));
	a.unreact(hello.id, "🦀".into()).await.unwrap();
	expect(&mut b_rx, "unreact", |p| matches!(p, Push::Reaction { added: false, count: 1, .. }))
		.await;

	// Topics: relayed with a prefix, readable for official clients.
	let topic = b.create_topic(LOBBY, "Plans".into(), Some(hello.id)).await.unwrap();
	let Push::TopicUpdated(t) =
		expect(&mut a_rx, "topic", |p| matches!(p, Push::TopicUpdated(_))).await
	else {
		unreachable!()
	};
	assert_eq!(t.id, topic.id);
	let in_topic = b.post(LOBBY, "saturday?".into(), Some(topic.id)).await.unwrap();
	assert_eq!(in_topic.topic_id, Some(topic.id));
	assert!(fx.fake.posted_by().contains(&("Bob1".into(), "[#Plans] saturday?".into())));
	expect(
		&mut c_rx,
		"prefixed chat event",
		|p| matches!(p, Push::Chat { message, .. } if message.text == "[#Plans] saturday?"),
	)
	.await;
	// Someone in TeamSpeak answers in the topic format.
	fx.fake.say_in_channel(1, "Dave", "dave-uid", "[#Plans] yes!");
	let Push::Message(answer) = expect(
		&mut a_rx,
		"answer",
		|p| matches!(p, Push::Message(e) if e.message.author_name == "Dave"),
	)
	.await
	else {
		unreachable!()
	};
	assert_eq!((answer.topic_id, answer.message.text.as_str()), (Some(topic.id), "yes!"));
	let q = HistoryQuery { topic: Some(topic.id), ..HistoryQuery::latest(LOBBY, None) };
	let texts: Vec<String> =
		b.history(q).await.unwrap().messages.into_iter().map(|e| e.message.text).collect();
	assert_eq!(texts, ["saturday?", "yes!"]);
	let topics = a.topics(LOBBY, false).await.unwrap();
	assert_eq!((topics[0].message_count, topics[0].root_message_id), (2, Some(hello.id)));
	assert_eq!(
		code(c.update_topic(topic.id, Some("Mine".into()), None).await),
		ErrorCode::Forbidden
	);
	b.update_topic(topic.id, None, Some(true)).await.unwrap();
	assert_eq!(code(b.post(LOBBY, "late".into(), Some(topic.id)).await), ErrorCode::Forbidden);

	// Sync: what changed since the first post.
	let (changed, rev, more) = c.sync(LOBBY, hello.rev, None).await.unwrap();
	assert!(!more && rev > hello.rev);
	assert_eq!(changed.first().map(|e| e.id), Some(hello.id), "reactions bumped the revision");
	let (none, same, _) = c.sync(LOBBY, rev, None).await.unwrap();
	assert!(none.is_empty() && same == rev);

	// Cursors: after an id, oldest first.
	let q =
		HistoryQuery { after: Some(hello.id), limit: Some(1), ..HistoryQuery::latest(LOBBY, None) };
	let page = c.history(q).await.unwrap();
	assert_eq!((page.messages.len(), page.has_more), (1, true));
	assert_eq!(page.messages[0].id, in_topic.id);

	// Access: nobody reads a channel above their subscribe power.
	assert_eq!(code(c.open_chat(ChatTarget::Channel(3)).await), ErrorCode::Forbidden);
	assert_eq!(code(c.pin(999_999).await), ErrorCode::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_types_keep_the_connection() {
	use futures_util::{SinkExt, StreamExt};
	use tokio_tungstenite::tungstenite::Message;
	let fx = Fixture::start("unknown", "").await;
	let (ws, _) = tokio_tungstenite::connect_async(fx.url.as_str()).await.unwrap();
	let (mut tx, rx) = ws.split();
	let rx = rx.filter_map(async |m| match m {
		Ok(Message::Text(t)) => serde_json::from_str::<serde_json::Value>(t.as_str()).ok(),
		_ => None,
	});
	let mut rx = std::pin::pin!(rx);
	assert_eq!(rx.next().await.unwrap()["type"], "hello");
	for (frame, want) in [
		(r#"{"v":1,"id":5,"type":"teleport","data":{}}"#, "unknown_type"),
		(r#"{"v":1,"id":6,"type":"pin","data":{"message_id":"x"}}"#, "bad_request"),
		(r#"{"v":1,"id":7,"type":"list_streams"}"#, "not_authenticated"),
	] {
		tx.send(Message::Text(frame.into())).await.unwrap();
		let reply = rx.next().await.unwrap();
		assert_eq!(
			(reply["type"].as_str(), reply["data"]["code"].as_str()),
			(Some("error"), Some(want))
		);
		assert!(reply["id"].is_u64(), "{reply}");
	}
	tx.send(Message::Text(r#"{"v":1,"id":8,"type":"ping"}"#.into())).await.unwrap();
	let pong = rx.next().await.unwrap();
	assert_eq!((pong["type"].as_str(), pong["id"].as_u64()), (Some("pong"), Some(8)));
}

#[tokio::test(flavor = "multi_thread")]
async fn events_streams_activity() {
	let fx = Fixture::start("events", "[events]\nreminder_minutes = [30]\n").await;
	let (alice, bob, _) = fx.people();
	let (a, mut a_rx) = alice.connect(&fx).await;
	let (b, mut b_rx) = bob.connect(&fx).await;
	a.subscribe_events().await.unwrap();
	a.subscribe_activity().await.unwrap();
	assert!(a.subscribe_streams().await.unwrap().is_empty());
	b.subscribe_streams().await.unwrap();

	let start = now_ms() + 3_000;
	let spec = EventSpec {
		title: "Speedrun".into(),
		start_ms: start,
		end_ms: Some(start + 3_600_000),
		channel: Some(1),
		kind: EventKind::Stream,
		stream_game: Some("Celeste".into()),
		..Default::default()
	};
	let event = a.create_event(spec.clone()).await.unwrap();
	expect(
		&mut a_rx,
		"event created activity",
		|p| matches!(p, Push::ActivityAdded(e) if e.kind == "event_created"),
	)
	.await;
	let bob_view = b.rsvp(event.id, Some(RsvpStatus::Going)).await.unwrap();
	assert_eq!((bob_view.going, bob_view.my_rsvp), (1, Some(RsvpStatus::Going)));
	let push =
		expect(&mut a_rx, "rsvp", |p| matches!(p, Push::EventUpdated(e) if e.going == 1)).await;
	assert!(matches!(push, Push::EventUpdated(e) if e.my_rsvp.is_none()));
	assert_eq!(a.event(event.id).await.unwrap().attendees.len(), 1);
	assert_eq!(code(b.delete_event(event.id).await), ErrorCode::Forbidden);
	let bad = EventSpec { end_ms: Some(start - 1), ..spec.clone() };
	assert_eq!(code(a.create_event(bad).await), ErrorCode::BadRequest);
	assert_eq!(b.events(Default::default()).await.unwrap().len(), 1);

	// Reminder times change at runtime: now "when it starts".
	a.config_set("events.reminder_minutes".into(), json!([0])).await.unwrap();
	let Push::EventReminder { event: reminded, .. } =
		expect(&mut a_rx, "reminder", |p| matches!(p, Push::EventReminder { .. })).await
	else {
		unreachable!()
	};
	assert_eq!(reminded.id, event.id);
	expect(
		&mut a_rx,
		"starting activity",
		|p| matches!(p, Push::ActivityAdded(e) if e.kind == "event_starting"),
	)
	.await;

	// Alice (client 10) goes live: the directory lists it and links the event.
	assert_eq!(
		code(
			b.register_stream(StreamSpec {
				stream_id: "s-1".into(),
				client_id: Some(10),
				..Default::default()
			})
			.await
		),
		ErrorCode::Forbidden
	);
	let stream = a
		.register_stream(StreamSpec {
			stream_id: "s-1".into(),
			client_id: Some(10),
			title: "Any%".into(),
			kind: "game".into(),
			..Default::default()
		})
		.await
		.unwrap();
	assert_eq!((stream.channel, stream.event_id), (Some(1), Some(event.id)));
	let Push::StreamStarted(seen) =
		expect(&mut b_rx, "stream", |p| matches!(p, Push::StreamStarted(_))).await
	else {
		unreachable!()
	};
	assert_eq!(seen.source, StreamSource::Registered);
	expect(
		&mut a_rx,
		"live event",
		|p| matches!(p, Push::EventUpdated(e) if e.live_stream.as_deref() == Some("s-1")),
	)
	.await;
	b.update_stream("s-1".into(), None, Some(3)).await.unwrap_err();
	a.update_stream("s-1".into(), None, Some(3)).await.unwrap();
	expect(&mut b_rx, "viewers", |p| matches!(p, Push::StreamUpdated(s) if s.viewers == Some(3)))
		.await;
	// A late client finds it.
	assert_eq!(b.streams().await.unwrap()[0].id, "s-1");

	// Alice's client leaves: the entry goes, the event is no longer live.
	fx.fake.notify_observers("notifyclientleftview cfid=1 ctid=0 reasonid=8 clid=10");
	expect(&mut b_rx, "stream ended", |p| matches!(p, Push::StreamEnded { id, .. } if id == "s-1"))
		.await;
	expect(
		&mut a_rx,
		"not live",
		|p| matches!(p, Push::EventUpdated(e) if e.id == event.id && e.live_stream.is_none()),
	)
	.await;
	assert!(b.streams().await.unwrap().is_empty());

	// A client streaming without registering shows up as detected.
	fx.fake.notify_observers(
		"notifycliententerview cfid=0 ctid=2 reasonid=0 clid=12 client_unique_identifier=erin \
		 client_nickname=Erin client_type=0 client_is_streaming=1",
	);
	let Push::StreamStarted(detected) =
		expect(&mut b_rx, "detected", |p| matches!(p, Push::StreamStarted(_))).await
	else {
		unreachable!()
	};
	assert_eq!(
		(detected.source, detected.stream_id, detected.channel),
		(StreamSource::Detected, None, Some(2))
	);

	let (feed, _) = b.activity(None, None).await.unwrap();
	let kinds: Vec<&str> = feed.iter().map(|e| e.kind.as_str()).collect();
	for kind in ["event_created", "event_starting", "stream_started", "stream_ended"] {
		assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
	}
	a.delete_event(event.id).await.unwrap();
	expect(&mut a_rx, "deleted", |p| matches!(p, Push::EventDeleted(id) if *id == event.id)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_settings_apply_live() {
	let fx = Fixture::start("admin", "[relay]\nformat = \"<{nick}> {text}\"\n").await;
	let (alice, bob, _) = fx.people();
	let (a, _a_rx) = alice.connect(&fx).await;
	let (b, mut b_rx) = bob.connect(&fx).await;
	b.enable(Vec::new()).await.unwrap();
	b.open_chat(LOBBY).await.unwrap();

	// Only admins administer.
	assert_eq!(code(b.config_list().await), ErrorCode::Forbidden);
	assert_eq!(
		code(b.config_set("features.pins".into(), json!(false)).await),
		ErrorCode::Forbidden
	);
	let entries = a.config_list().await.unwrap();
	let format = entries.iter().find(|e| e.key == "relay.format").unwrap();
	assert_eq!(format.value, "<{nick}> {text}");
	assert_eq!(format.source, voelin_gateway_proto::ConfigSource::File);
	assert!(entries.iter().find(|e| e.key == "query.addr").unwrap().bootstrap);
	assert_eq!(code(a.config_set("query.addr".into(), json!("x:1")).await), ErrorCode::BadRequest);
	assert_eq!(code(a.config_set("no.such.key".into(), json!(1)).await), ErrorCode::NotFound);
	assert_eq!(
		code(a.config_set("quota.history_page".into(), json!("many")).await),
		ErrorCode::BadRequest
	);

	// Feature switches reach connected clients at once.
	let first = b.post(LOBBY, "one".into(), None).await.unwrap();
	assert!(fx.fake.posted_by().contains(&("Bob1".into(), "one".into())));
	let entry = a.config_set("features.pins".into(), json!(false)).await.unwrap();
	assert_eq!(entry.source, voelin_gateway_proto::ConfigSource::Db);
	expect(
		&mut b_rx,
		"capabilities",
		|p| matches!(p, Push::Capabilities(c) if !c.iter().any(|c| c == "pins")),
	)
	.await;
	assert!(!b.has(feature::PINS));
	assert_eq!(code(a.pin(first.id).await), ErrorCode::FeatureDisabled);
	let entry = a.config_reset("features.pins".into()).await.unwrap();
	assert_eq!(entry.source, voelin_gateway_proto::ConfigSource::Default);
	expect(
		&mut b_rx,
		"pins back",
		|p| matches!(p, Push::Capabilities(c) if c.iter().any(|c| c == "pins")),
	)
	.await;

	// Permission rules by server group.
	assert!(!b.permissions(Some(1)).await.unwrap().contains(&Action::Pin));
	let rule = PermRule { server_groups: vec![7], ..Default::default() };
	let rules = a.perm_set(Action::Pin, rule).await.unwrap();
	let pin_rule = rules.iter().find(|r| r.action == Action::Pin).unwrap();
	assert_eq!(pin_rule.rule.as_ref().unwrap().server_groups, [7]);
	assert!(b.permissions(Some(1)).await.unwrap().contains(&Action::Pin));
	b.pin(first.id).await.unwrap();
	a.perm_set(Action::React, PermRule::default()).await.unwrap();
	assert_eq!(code(b.react(first.id, "👍".into()).await), ErrorCode::Forbidden);
	a.perm_reset(Action::React).await.unwrap();
	b.react(first.id, "👍".into()).await.unwrap();

	// Quotas.
	for i in 0..3 {
		b.post(LOBBY, format!("m{i}"), None).await.unwrap();
	}
	a.config_set("quota.history_page".into(), json!(2)).await.unwrap();
	let page = b.history(HistoryQuery::latest(LOBBY, Some(100))).await.unwrap();
	assert_eq!((page.messages.len(), page.has_more), (2, true));
	a.config_set("quota.pins_per_channel".into(), json!(1)).await.unwrap();
	assert_eq!(code(b.pin(page.messages[0].id).await), ErrorCode::QuotaExceeded);
	// Bob posted four times so far: one more within a minute.
	a.config_set("limits.post_window_secs".into(), json!(60)).await.unwrap();
	a.config_set("limits.posts_per_window".into(), json!(5)).await.unwrap();
	b.post(LOBBY, "allowed".into(), None).await.unwrap();
	assert_eq!(code(b.post(LOBBY, "too fast".into(), None).await), ErrorCode::RateLimited);
	// 0: no limit.
	a.config_set("limits.posts_per_window".into(), json!(0)).await.unwrap();

	// Relay settings apply without a restart. The format is for posts the
	// relay cannot make under Bob's name: every variant of it is taken.
	for (clid, nick) in [(21, "Bob1"), (22, "Bob2"), (23, "Bob3")] {
		fx.fake.add_online(Online {
			clid,
			cid: 2,
			uid: format!("other-{nick}"),
			nickname: nick.into(),
			server_groups: Vec::new(),
			streaming: false,
		});
	}
	a.config_set("relay.format".into(), json!("{nick}: {text}")).await.unwrap();
	b.post(LOBBY, "two".into(), None).await.unwrap();
	assert!(fx.fake.posted().contains(&(2, 1, "Bob: two".into())));
	assert!(fx.fake.posted_by().contains(&("Chat Relay 1".into(), "Bob: two".into())));
	a.config_set("relay.nickname".into(), json!("Bridge")).await.unwrap();
	for _ in 0..100 {
		if fx.fake.state.lock().unwrap().nicknames.iter().any(|n| n == "Bridge 1") {
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	assert!(fx.fake.state.lock().unwrap().nicknames.iter().any(|n| n == "Bridge 1"));
	b.post(LOBBY, "three".into(), None).await.unwrap();
	assert!(fx.fake.posted().contains(&(2, 1, "Bob: three".into())));
	// Channel chat from TeamSpeak still arrives through the new relay.
	fx.fake.say_in_channel(1, "Dave", "dave", "after rename");
	expect(
		&mut b_rx,
		"relayed after rename",
		|p| matches!(p, Push::Message(e) if e.message.text == "after rename"),
	)
	.await;

	// Settings survive a restart of the hub's settings (they are in the database).
	let stored = fx.hub.db.config_values().unwrap();
	assert!(stored.iter().any(|(k, v)| k == "relay.nickname" && v == "Bridge"));
}

/// The gateway's answer at `/.well-known/tsgw`, as an app asks for it.
async fn well_known(fx: &Fixture) -> String {
	use tokio::io::{AsyncReadExt, AsyncWriteExt};
	let host = fx.url.trim_start_matches("ws://").trim_end_matches("/v1");
	let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
	let request =
		format!("GET /.well-known/tsgw HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
	stream.write_all(request.as_bytes()).await.unwrap();
	let mut response = String::new();
	stream.read_to_string(&mut response).await.unwrap();
	assert!(response.starts_with("HTTP/1.1 200"), "{response}");
	let body = response.split_once("\r\n\r\n").unwrap().1;
	let answer: serde_json::Value = serde_json::from_str(body).unwrap();
	answer["url"].as_str().unwrap().to_owned()
}

/// Apps that know only the server's host ask the gateway on its port where
/// it is (`voelin_core::discover`), then log in there.
#[tokio::test(flavor = "multi_thread")]
async fn tells_apps_where_it_is() {
	let fx = Fixture::start("well-known", "").await;
	let url = well_known(&fx).await;
	assert_eq!(url, fx.url);
	let (alice, _, _) = fx.people();
	let login = Login::Identity { key: alice.key.clone(), key_offset: 0 };
	let (client, _pushes) = connect(&url, login).await.unwrap();
	assert!(client.has(feature::PRESENCE));

	// Behind a proxy it gives the public URL instead.
	let public = "[listen]\npublic_url = \"wss://gw.example.test/v1\"\n";
	let fx = Fixture::start("well-known-public", public).await;
	assert_eq!(well_known(&fx).await, "wss://gw.example.test/v1");
}

/// The TeamSpeak server drops the gateway's query connections (a restart):
/// logins go on working, through a new lookup connection.
#[tokio::test(flavor = "multi_thread")]
async fn logins_work_after_the_server_dropped_the_gateway() {
	let fx = Fixture::start("reconnect", "").await;
	let (alice, bob, _) = fx.people();
	let (b, mut b_rx) = bob.connect(&fx).await;
	b.open_chat(LOBBY).await.unwrap();
	b.open_chat(ChatTarget::Server).await.unwrap();
	assert!(fx.hub.lookup.is_up());
	let old_id = fx.hub.lookup.own_id().unwrap();
	fx.fake.drop_connections();
	// At once: the login waits for the new connection.
	let (a, _a_rx) = alice.connect(&fx).await;
	assert!(a.has(feature::ADMIN));
	assert!(fx.hub.lookup.is_up());
	a.post(ChatTarget::Server, "back again".into(), None).await.unwrap();
	assert!(fx.fake.posted().contains(&(3, 1, "back again".into())));

	// Bob still reads the lobby: its relay comes back.
	for _ in 0..100 {
		if fx.fake.relayed_channels().contains(&1) {
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	fx.fake.say_in_channel(1, "Dave", "dave", "after the restart");
	expect(
		&mut b_rx,
		"relayed after the restart",
		|p| matches!(p, Push::Chat { message, .. } if message.text == "after the restart"),
	)
	.await;

	// A restarted server hands out client ids from 1 again: a user may get
	// the one the gateway had. Their server chat is not the gateway's own.
	assert_ne!(fx.hub.lookup.own_id(), Some(old_id));
	for _ in 0..100 {
		if fx.fake.observed() {
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	fx.fake.notify_observers(&format!(
		"notifytextmessage targetmode=3 msg=hi\\sall invokerid={old_id} invokername=Erin \
		 invokeruid=erin"
	));
	expect(
		&mut b_rx,
		"server chat from the gateway's old id",
		|p| matches!(p, Push::Chat { message, .. } if message.text == "hi all"),
	)
	.await;
}

/// The server's flood protection refuses looking up an optional permission,
/// also when asked again after the wait: the gateway asks for no more, which
/// would get it banned for 10 minutes.
#[tokio::test(flavor = "multi_thread")]
async fn optional_permissions_stop_at_the_flood_protection() {
	let fake = FakeServer::start().await;
	fake.refuse("b_channel_modify_name", 2);
	let connect = voelin_query::Connect {
		transport: voelin_query::Transport::Raw,
		addr: fake.addr.to_string(),
		user: "serveradmin".into(),
		secret: None,
		server_port: None,
		server_id: None,
		// Taken to be on the allowlist.
		line: voelin_query::LineOptions { rate_limit: None, ..Default::default() },
	};
	let lookup = crate::lookup::Lookup::open(connect, "Gateway".into()).await;
	let ids = crate::perms::PermIds::load(&lookup).await.unwrap();
	assert_eq!(ids.subscribe_power, fake::SUBSCRIBE_POWER);
	assert_eq!((ids.channel_modify_name, ids.server_modify_name), (0, 0));
	let asked = fake.commands();
	let count =
		|name: &str| asked.iter().filter(|c| c.ends_with(&format!("permsid={name}"))).count();
	assert_eq!(count("b_channel_modify_name"), 2, "{asked:?}");
	assert_eq!(count("b_virtualserver_modify_name"), 0, "{asked:?}");
}

/// `/health` says what is missing until the gateway is logged in.
#[tokio::test(flavor = "multi_thread")]
async fn health_tells_whether_the_gateway_is_up() {
	use axum::extract::State;
	use axum::http::StatusCode;
	let fx = Fixture::start("health", "").await;
	let app = crate::App::started(fx.boot.clone(), fx.hub.clone());
	assert_eq!(crate::health(State(app)).await, (StatusCode::OK, "ok".to_string()));
	let starting = crate::App::new(fx.boot.clone());
	let (status, text) = crate::health(State(starting)).await;
	assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
	assert!(text.starts_with("starting"), "{text}");
}
