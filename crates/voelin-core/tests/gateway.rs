//! Chat history and gateway features through the engine, against a fake
//! gateway (a WebSocket server speaking the tsgw protocol from memory).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::broadcast::Receiver;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use voelin_core::gateway::{GatewayRequest, GatewayUpdate};
use voelin_core::history::MessageSource;
use voelin_core::settings::Settings;
use voelin_core::{Command, Engine, Event, History, HistoryMessage, HistorySource, ObserveState};
use voelin_gateway_proto::{
	ClientMsg, Envelope, ErrorCode, HistoryEntry, HistoryPage, PinInfo, ReactionCount, ServerMsg,
	TopicInfo, UserRef, feature,
};
use voelin_model::{ChatMessage, ChatTarget, PresenceSnapshot};

const LOBBY: ChatTarget = ChatTarget::Channel(1);
const ME: &str = "me-uid";

/// A connected session: its pushes and open chats.
type Session = (mpsc::UnboundedSender<ServerMsg>, Arc<Mutex<HashSet<ChatTarget>>>);

/// The gateway's data.
#[derive(Default)]
struct State {
	messages: Vec<HistoryEntry>,
	rev: i64,
	/// (message, emoji) → who reacted.
	reactions: BTreeMap<(i64, String), BTreeSet<String>>,
	topics: Vec<TopicInfo>,
	/// Push channels of the connected sessions, with their open chats.
	sessions: Vec<Session>,
	/// Types of the requests received.
	requests: Vec<String>,
	/// Close connections at once (the gateway is down).
	refuse: bool,
}

impl State {
	fn next_rev(&mut self) -> i64 {
		self.rev += 1;
		self.rev
	}

	/// A message as `uid` sees it.
	fn view(&self, entry: &HistoryEntry, uid: &str) -> HistoryEntry {
		let mut e = entry.clone();
		e.reactions = self
			.reactions
			.iter()
			.filter(|((id, _), users)| *id == entry.id && !users.is_empty())
			.map(|((_, emoji), users)| ReactionCount {
				emoji: emoji.clone(),
				count: users.len() as u32,
				me: users.contains(uid),
			})
			.collect();
		e
	}

	fn add(&mut self, target: ChatTarget, author: &str, text: &str, ts_ms: i64) -> HistoryEntry {
		let rev = self.next_rev();
		let message = ChatMessage {
			target,
			author_name: author.into(),
			author_uid: Some(format!("{author}-uid")),
			author_id: None,
			text: text.into(),
			ts_ms,
			via_relay: true,
		};
		let entry =
			HistoryEntry { rev, ..HistoryEntry::new(self.messages.len() as i64 + 1, message) };
		self.messages.push(entry.clone());
		self.push_open(&entry.message.target, ServerMsg::Message { entry: entry.clone() });
		entry
	}

	fn push_open(&self, target: &ChatTarget, msg: ServerMsg) {
		for (tx, open) in &self.sessions {
			if open.lock().unwrap().contains(target) {
				let _ = tx.send(msg.clone());
			}
		}
	}

	fn page(&self, q: &voelin_gateway_proto::HistoryQuery) -> HistoryPage {
		let mut all: Vec<&HistoryEntry> = self
			.messages
			.iter()
			.filter(|e| e.message.target == q.target)
			.filter(|e| q.topic.is_none_or(|t| e.topic_id == Some(t)))
			.filter(|e| q.before.is_none_or(|b| e.id < b))
			.filter(|e| q.before_ms.is_none_or(|b| e.message.ts_ms < b))
			.filter(|e| q.after.is_none_or(|a| e.id > a))
			.collect();
		let mut has_more = false;
		if let Some(limit) = q.limit.map(|l| l as usize)
			&& all.len() > limit
		{
			has_more = true;
			all = all.split_off(all.len() - limit);
		}
		HistoryPage {
			target: q.target.clone(),
			topic: q.topic,
			messages: all.into_iter().map(|e| self.view(e, ME)).collect(),
			has_more,
		}
	}
}

struct FakeGateway {
	state: Arc<Mutex<State>>,
	url: String,
}

impl FakeGateway {
	async fn start() -> Self {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let url = format!("ws://{}/v1", listener.local_addr().unwrap());
		let state: Arc<Mutex<State>> = Arc::default();
		let st = state.clone();
		tokio::spawn(async move {
			while let Ok((tcp, _)) = listener.accept().await {
				tokio::spawn(serve(tcp, st.clone()));
			}
		});
		Self { state, url }
	}

	fn add(&self, target: ChatTarget, author: &str, text: &str, ts_ms: i64) -> HistoryEntry {
		self.state.lock().unwrap().add(target, author, text, ts_ms)
	}

	fn requests(&self) -> Vec<String> {
		self.state.lock().unwrap().requests.clone()
	}
}

fn caps() -> Vec<String> {
	[
		feature::PRESENCE,
		feature::RELAY,
		feature::HISTORY,
		feature::PINS,
		feature::REACTIONS,
		feature::TOPICS,
		feature::EVENTS,
	]
	.map(String::from)
	.to_vec()
}

async fn serve(tcp: tokio::net::TcpStream, state: Arc<Mutex<State>>) {
	if state.lock().unwrap().refuse {
		return;
	}
	#[allow(clippy::result_large_err)] // tungstenite's callback type
	let callback = |_: &Request, mut response: Response| {
		response
			.headers_mut()
			.insert("Sec-WebSocket-Protocol", voelin_gateway_proto::SUBPROTOCOL.parse().unwrap());
		Ok(response)
	};
	let ws = tokio_tungstenite::accept_hdr_async(tcp, callback).await.unwrap();
	let (mut sink, mut stream) = ws.split();
	let (tx, mut rx) = mpsc::unbounded_channel::<Envelope<ServerMsg>>();
	tokio::spawn(async move {
		while let Some(env) = rx.recv().await {
			let text = serde_json::to_string(&env).unwrap();
			if sink.send(Message::Text(text.into())).await.is_err() {
				break;
			}
		}
		let _ = sink.close().await;
	});
	let _ = tx.send(Envelope::new(ServerMsg::Hello {
		gateway_id: "fake-gw".into(),
		server_uid: "fake-server".into(),
		server_name: "Fake".into(),
		nonce: "n".into(),
		capabilities: caps(),
	}));
	// Pushes of this session.
	let (push_tx, mut push_rx) = mpsc::unbounded_channel();
	let open: Arc<Mutex<HashSet<ChatTarget>>> = Arc::default();
	let forward = tx.clone();
	tokio::spawn(async move {
		while let Some(msg) = push_rx.recv().await {
			if forward.send(Envelope::new(msg)).is_err() {
				break;
			}
		}
	});
	let mut logged_in = false;
	while let Some(Ok(frame)) = stream.next().await {
		let Message::Text(text) = frame else { continue };
		let env: Envelope<ClientMsg> = serde_json::from_str(text.as_str()).unwrap();
		let id = env.id.unwrap_or(0);
		let reply = {
			let mut st = state.lock().unwrap();
			let name = serde_json::to_value(&env.msg).unwrap()["type"].as_str().unwrap().to_owned();
			st.requests.push(name);
			answer(&mut st, env.msg, &open, &push_tx, &mut logged_in)
		};
		if tx.send(Envelope::with_id(id, reply)).is_err() {
			break;
		}
	}
	let mut st = state.lock().unwrap();
	st.sessions.retain(|(t, _)| !t.same_channel(&push_tx));
}

fn answer(
	st: &mut State,
	msg: ClientMsg,
	open: &Arc<Mutex<HashSet<ChatTarget>>>,
	push: &mpsc::UnboundedSender<ServerMsg>,
	logged_in: &mut bool,
) -> ServerMsg {
	match msg {
		ClientMsg::Auth { .. } => {
			*logged_in = true;
			st.sessions.push((push.clone(), open.clone()));
			ServerMsg::AuthOk {
				uid: ME.into(),
				token: "t".into(),
				token_expires: i64::MAX,
				capabilities: caps(),
			}
		}
		_ if !*logged_in => {
			ServerMsg::Error { code: ErrorCode::NotAuthenticated, message: "".into() }
		}
		ClientMsg::Enable { .. } => ServerMsg::Enabled { features: caps() },
		ClientMsg::SubscribePresence => {
			let snapshot = PresenceSnapshot {
				server_name: "Fake".into(),
				channels: Vec::new(),
				clients: Vec::new(),
			};
			let _ = push.send(ServerMsg::PresenceSnapshot { seq: 0, snapshot });
			ServerMsg::Ok
		}
		ClientMsg::OpenChat { target } => {
			open.lock().unwrap().insert(target);
			ServerMsg::Ok
		}
		ClientMsg::CloseChat { target } => {
			open.lock().unwrap().remove(&target);
			ServerMsg::Ok
		}
		ClientMsg::SendChat { target, text } => {
			// Posted as "me" (uid `me-uid`).
			st.add(target, "me", &text, now_ms());
			ServerMsg::Ok
		}
		ClientMsg::Post { target, text, topic } => {
			let rev = st.next_rev();
			let message = ChatMessage {
				target: target.clone(),
				author_name: "me".into(),
				author_uid: Some(ME.into()),
				author_id: None,
				text,
				ts_ms: now_ms(),
				via_relay: true,
			};
			let entry = HistoryEntry {
				rev,
				topic_id: topic,
				..HistoryEntry::new(st.messages.len() as i64 + 1, message)
			};
			st.messages.push(entry.clone());
			st.push_open(&target, ServerMsg::Message { entry: entry.clone() });
			ServerMsg::Posted { entry }
		}
		ClientMsg::QueryHistory(q) => ServerMsg::HistoryPage(st.page(&q)),
		ClientMsg::Sync { target, since_rev, limit } => {
			let mut changed: Vec<HistoryEntry> = st
				.messages
				.iter()
				.filter(|e| e.message.target == target && e.rev > since_rev)
				.map(|e| st.view(e, ME))
				.collect();
			changed.sort_by_key(|e| e.rev);
			let has_more = limit.is_some_and(|l| changed.len() > l as usize);
			if let Some(l) = limit {
				changed.truncate(l as usize);
			}
			let rev = changed.last().map_or(since_rev, |e| e.rev);
			ServerMsg::SyncPage { target, messages: changed, rev, has_more }
		}
		ClientMsg::Pin { message_id } => {
			let rev = st.next_rev();
			let Some(e) = st.messages.iter_mut().find(|e| e.id == message_id) else {
				return ServerMsg::Error { code: ErrorCode::NotFound, message: "".into() };
			};
			e.pinned = true;
			e.rev = rev;
			let entry = e.clone();
			let pin = PinInfo { entry: st.view(&entry, ME), by: me(), ts_ms: now_ms() };
			st.push_open(
				&entry.message.target,
				ServerMsg::Pinned { target: entry.message.target.clone(), pin },
			);
			ServerMsg::Ok
		}
		ClientMsg::ListPins { target } => ServerMsg::Pins {
			pins: st
				.messages
				.iter()
				.filter(|e| e.pinned && e.message.target == target)
				.map(|e| PinInfo { entry: st.view(e, ME), by: me(), ts_ms: 1 })
				.collect(),
			target,
		},
		ClientMsg::React { message_id, emoji } => {
			let rev = st.next_rev();
			let Some(e) = st.messages.iter_mut().find(|e| e.id == message_id) else {
				return ServerMsg::Error { code: ErrorCode::NotFound, message: "".into() };
			};
			e.rev = rev;
			let target = e.message.target.clone();
			let users = st.reactions.entry((message_id, emoji.clone())).or_default();
			users.insert(ME.into());
			let count = users.len() as u32;
			let push = ServerMsg::Reaction {
				target: target.clone(),
				message_id,
				emoji,
				user: me(),
				added: true,
				count,
			};
			st.push_open(&target, push);
			ServerMsg::Ok
		}
		ClientMsg::CreateTopic { target, title, message_id } => {
			let topic = TopicInfo {
				id: st.topics.len() as i64 + 1,
				target,
				title,
				creator: me(),
				created_ms: 1,
				root_message_id: message_id,
				last_activity_ms: 1,
				message_count: 0,
				archived: false,
			};
			st.topics.push(topic.clone());
			ServerMsg::Topic { topic }
		}
		ClientMsg::SubscribeEvents | ClientMsg::Ping => ServerMsg::Ok,
		_ => ServerMsg::Error { code: ErrorCode::UnknownType, message: "not in the fake".into() },
	}
}

fn me() -> UserRef {
	UserRef { uid: ME.into(), name: "me".into() }
}

fn now_ms() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

/// The next event matching `f`, within 10 s.
async fn wait<T>(rx: &mut Receiver<Event>, what: &str, mut f: impl FnMut(Event) -> Option<T>) -> T {
	timeout(Duration::from_secs(10), async {
		loop {
			match rx.recv().await {
				Ok(e) => {
					if let Some(t) = f(e) {
						return t;
					}
				}
				Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
				Err(e) => panic!("{e}"),
			}
		}
	})
	.await
	.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

type Batch = (Vec<HistoryMessage>, HistorySource, bool);

async fn batch(rx: &mut Receiver<Event>, source: HistorySource) -> Batch {
	wait(rx, &format!("{source:?} history"), |e| match e {
		Event::ChatHistory { messages, source: s, complete, target, .. }
			if s == source && target == LOBBY =>
		{
			Some((messages, s, complete))
		}
		_ => None,
	})
	.await
}

fn texts(messages: &[HistoryMessage]) -> Vec<&str> {
	messages.iter().map(|m| m.message.text.as_str()).collect()
}

async fn update(
	rx: &mut Receiver<Event>,
	what: &str,
	mut f: impl FnMut(&GatewayUpdate) -> bool,
) -> GatewayUpdate {
	wait(rx, what, |e| match e {
		Event::Gateway { update, .. } if f(&update) => Some(update),
		_ => None,
	})
	.await
}

fn observe(engine: &Engine, session: u64, gw: &FakeGateway) {
	let identity = tsclientlib::Identity::create();
	engine.send(Command::ObserveGateway {
		session,
		url: gw.url.clone(),
		identity: Box::new(identity),
	});
}

fn temp_db(tag: &str) -> std::path::PathBuf {
	let dir = std::env::temp_dir().join(format!("voelin-core-{tag}-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	dir.join("client.db")
}

/// Opening a chat shows the stored page, then the gateway's; older pages
/// come from the gateway; what was said while we were away arrives on the
/// next connect; the database keeps it all.
#[tokio::test(flavor = "multi_thread")]
async fn history_on_open_paging_and_offline_messages() {
	let gw = FakeGateway::start().await;
	for i in 1..=5 {
		gw.add(LOBBY, "alice", &format!("m{i}"), i * 1000);
	}
	gw.add(ChatTarget::Server, "alice", "elsewhere", 500);
	let path = temp_db("history");
	let settings = Settings::in_memory();
	settings.set_json("chat.history_page", json!(3)).unwrap();
	let engine = Engine::start_with(settings.clone(), History::open(&path).unwrap());
	let mut rx = engine.subscribe();

	engine.send(Command::OpenChat { session: 1, target: LOBBY });
	observe(&engine, 1, &gw);
	// Nothing stored yet; then the gateway's latest page; then in sync.
	let (local, _, complete) = batch(&mut rx, HistorySource::Local).await;
	assert!(local.is_empty() && !complete);
	let (page, _, complete) = batch(&mut rx, HistorySource::Gateway).await;
	assert_eq!(texts(&page), ["m3", "m4", "m5"]);
	assert!(!complete, "older messages exist");
	assert!(page.iter().all(|m| m.remote_id.is_some() && m.source == MessageSource::Gateway));
	let (synced, _, _) = batch(&mut rx, HistorySource::Gateway).await;
	assert!(synced.is_empty(), "nothing new: {synced:?}");

	// Older: the gateway's page before m3.
	engine.send(Command::LoadOlderHistory { session: 1, target: LOBBY, before: Some(page[0].id) });
	let (older, _, complete) = batch(&mut rx, HistorySource::Gateway).await;
	assert_eq!(texts(&older), ["m1", "m2"]);
	assert!(complete, "the beginning");

	// Away: two messages and a pin on m2.
	engine.send(Command::StopObserving { session: 1 });
	wait(&mut rx, "gateway stopped", |e| match e {
		Event::State { state, .. } if state.observe == ObserveState::Off => Some(()),
		_ => None,
	})
	.await;
	gw.add(LOBBY, "bob", "while away 1", 6000);
	gw.add(LOBBY, "bob", "while away 2", 7000);
	{
		let mut st = gw.state.lock().unwrap();
		let rev = st.next_rev();
		let m2 = st.messages.iter_mut().find(|e| e.message.text == "m2").unwrap();
		m2.pinned = true;
		m2.rev = rev;
	}
	// Back: stored page at once, then what we missed.
	observe(&engine, 1, &gw);
	let (local, _, _) = batch(&mut rx, HistorySource::Local).await;
	assert_eq!(texts(&local), ["m3", "m4", "m5"]);
	let mut missed = Vec::new();
	while missed.len() < 3 {
		missed.extend(batch(&mut rx, HistorySource::Gateway).await.0);
	}
	missed.sort_by_key(|m| (m.message.ts_ms, m.id));
	assert_eq!(texts(&missed), ["m2", "while away 1", "while away 2"]);
	assert!(missed[0].pinned);
	assert_eq!(missed[0].id, older[1].id, "the stored row was updated");

	// It is all in the database, oldest first.
	engine.send(Command::CloseSession { session: 1 });
	let history = engine.history();
	let stored = history.page("fake-server", &LOBBY, Default::default(), false).await.unwrap();
	assert_eq!(texts(&stored), ["m1", "m2", "m3", "m4", "m5", "while away 1", "while away 2"]);
	history.flush();
	drop((engine, history));

	// After a restart, with the gateway down: the stored page, found by the
	// gateway's URL, which is remembered with the server's id.
	gw.state.lock().unwrap().refuse = true;
	let engine = Engine::start_with(settings, History::open(&path).unwrap());
	let mut rx = engine.subscribe();
	engine.send(Command::OpenChat { session: 7, target: LOBBY });
	observe(&engine, 7, &gw);
	let (local, source) = wait(&mut rx, "history without the gateway", |e| match e {
		Event::ChatHistory { session: 7, messages, source, .. } => Some((messages, source)),
		_ => None,
	})
	.await;
	assert_eq!(source, HistorySource::Local);
	assert_eq!(texts(&local), ["m5", "while away 1", "while away 2"]);
	drop(engine);
	let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Live messages, our own included, are stored once; pins, reactions,
/// topics and other requests go through Command::Gateway.
#[tokio::test(flavor = "multi_thread")]
async fn live_messages_and_gateway_requests() {
	let gw = FakeGateway::start().await;
	let engine = Engine::start();
	let mut rx = engine.subscribe();
	engine.send(Command::OpenChat { session: 1, target: LOBBY });
	observe(&engine, 1, &gw);
	let connected =
		update(&mut rx, "connected", |u| matches!(u, GatewayUpdate::Connected { .. })).await;
	let GatewayUpdate::Connected { server_uid, uid, capabilities, .. } = connected else {
		unreachable!()
	};
	assert_eq!((server_uid.as_str(), uid.as_str()), ("fake-server", ME));
	assert!(capabilities.iter().any(|c| c == feature::PINS));
	// In sync (the chat is empty).
	batch(&mut rx, HistorySource::Gateway).await;

	// Our message: stored at once, then merged with the gateway's copy.
	engine.send(Command::SendChat { session: 1, target: LOBBY, text: "hello".into() });
	let (sent, _, _) = batch(&mut rx, HistorySource::Live).await;
	assert_eq!((sent[0].source, sent[0].remote_id), (MessageSource::Local, None));
	let (merged, _, _) = batch(&mut rx, HistorySource::Live).await;
	assert_eq!(merged[0].id, sent[0].id, "one message");
	let remote = merged[0].remote_id.expect("the gateway's id");

	// A message from someone else arrives live.
	gw.add(LOBBY, "bob", "hi me", now_ms());
	let (live, _, _) = batch(&mut rx, HistorySource::Live).await;
	assert_eq!(texts(&live), ["hi me"]);
	assert_ne!(live[0].id, sent[0].id);

	// Pin: done, pushed, the row is pinned.
	engine
		.send(Command::Gateway { session: 1, request: GatewayRequest::Pin { message_id: remote } });
	let (mut pinned_row, mut pinned_push, mut done) = (None, None, false);
	wait(&mut rx, "pin", |e| {
		match e {
			Event::ChatHistory { messages, source: HistorySource::Live, .. }
				if messages[0].pinned =>
			{
				pinned_row = Some(messages[0].clone());
			}
			Event::Gateway { update: GatewayUpdate::Pinned { pin, .. }, .. } => {
				pinned_push = Some(pin)
			}
			Event::Gateway { update: GatewayUpdate::Done { request }, .. } if request == "pin" => {
				done = true
			}
			_ => {}
		}
		(pinned_row.is_some() && pinned_push.is_some() && done).then_some(())
	})
	.await;
	assert_eq!(pinned_row.unwrap().id, sent[0].id);
	assert_eq!(pinned_push.unwrap().message.id, sent[0].id);

	// React: the row carries the count and that it was us.
	engine.send(Command::Gateway {
		session: 1,
		request: GatewayRequest::React { message_id: remote, emoji: "👍".into() },
	});
	let row = wait(&mut rx, "reaction", |e| match e {
		Event::ChatHistory { messages, source: HistorySource::Live, .. }
			if !messages[0].reactions.is_empty() =>
		{
			Some(messages[0].clone())
		}
		_ => None,
	})
	.await;
	assert_eq!(row.reactions, [ReactionCount { emoji: "👍".into(), count: 1, me: true }]);
	update(&mut rx, "reaction push", |u| {
		matches!(u, GatewayUpdate::Reaction { added: true, count: 1, .. })
	})
	.await;

	// A topic from the message, and a post into it.
	engine.send(Command::Gateway {
		session: 1,
		request: GatewayRequest::CreateTopic {
			target: LOBBY,
			title: "Plans".into(),
			message_id: Some(remote),
		},
	});
	let GatewayUpdate::Topic { topic } =
		update(&mut rx, "topic", |u| matches!(u, GatewayUpdate::Topic { .. })).await
	else {
		unreachable!()
	};
	assert_eq!((topic.title.as_str(), topic.root_message_id), ("Plans", Some(remote)));
	engine.send(Command::Gateway {
		session: 1,
		request: GatewayRequest::Post {
			target: LOBBY,
			text: "in topic".into(),
			topic: Some(topic.id),
		},
	});
	let GatewayUpdate::Posted { message } =
		update(&mut rx, "posted", |u| matches!(u, GatewayUpdate::Posted { .. })).await
	else {
		unreachable!()
	};
	assert_eq!((message.topic_id, message.message.text.as_str()), (Some(topic.id), "in topic"));
	assert_ne!(message.id, 0, "stored (in memory: negative ids)");

	// Pins list with local ids.
	engine.send(Command::Gateway { session: 1, request: GatewayRequest::Pins { target: LOBBY } });
	let GatewayUpdate::Pins { pins, .. } =
		update(&mut rx, "pins", |u| matches!(u, GatewayUpdate::Pins { .. })).await
	else {
		unreachable!()
	};
	assert_eq!(pins.iter().map(|p| p.message.id).collect::<Vec<_>>(), [sent[0].id]);

	// Refusals come as Failed, with the gateway's code.
	engine.send(Command::Gateway { session: 1, request: GatewayRequest::ConfigList });
	let failed = update(&mut rx, "failed", |u| matches!(u, GatewayUpdate::Failed { .. })).await;
	assert!(
		matches!(failed, GatewayUpdate::Failed { request, code: Some(ErrorCode::UnknownType), .. } if request == "config_list")
	);
	// Without a gateway, too.
	engine.send(Command::Gateway { session: 2, request: GatewayRequest::Streams });
	let failed = wait(&mut rx, "no gateway", |e| match e {
		Event::Gateway { session: 2, update } => Some(update),
		_ => None,
	})
	.await;
	assert!(matches!(failed, GatewayUpdate::Failed { code: None, .. }));

	// Everything is one row per message in the (in-memory) history.
	let stored = engine.history().page("fake-server", &LOBBY, Default::default(), false).await;
	assert_eq!(texts(&stored.unwrap()), ["hello", "hi me", "in topic"]);
	// The engine enabled the extensions and subscribed to events.
	let requests = gw.requests();
	assert!(requests.contains(&"enable".to_owned()), "{requests:?}");
	assert!(requests.contains(&"subscribe_events".to_owned()), "{requests:?}");
	// Requests serialize for tools.
	assert_eq!(
		serde_json::to_value(GatewayRequest::Pin { message_id: 7 }).unwrap(),
		json!({"pin": {"message_id": 7}})
	);
	let parsed: GatewayRequest =
		serde_json::from_value(json!({"topics": {"target": {"kind": "channel", "id": 1}}}))
			.unwrap();
	assert_eq!(parsed, GatewayRequest::Topics { target: LOBBY, include_archived: false });
}

/// With `chat.store_history` off nothing reaches the database; messages
/// are kept in memory with negative ids.
#[tokio::test(flavor = "multi_thread")]
async fn store_history_off_keeps_the_database_empty() {
	let gw = FakeGateway::start().await;
	gw.add(LOBBY, "alice", "secret", 1000);
	let path = temp_db("nostore");
	let settings = Settings::in_memory();
	settings.set_json("chat.store_history", json!(false)).unwrap();
	let engine = Engine::start_with(settings, History::open(&path).unwrap());
	let mut rx = engine.subscribe();
	engine.send(Command::OpenChat { session: 1, target: LOBBY });
	observe(&engine, 1, &gw);
	let (page, _, _) = batch(&mut rx, HistorySource::Gateway).await;
	assert_eq!(texts(&page), ["secret"]);
	assert!(page[0].id < 0, "{}", page[0].id);
	let history = engine.history();
	assert!(
		history.page("fake-server", &LOBBY, Default::default(), false).await.unwrap().is_empty()
	);
	assert_eq!(
		history.page("fake-server", &LOBBY, Default::default(), true).await.unwrap().len(),
		1
	);
	drop((engine, history));
	let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
