//! Engine against the development servers (dev/docker-compose.yml).
//! Runs only with `VOELIN_LIVE=1`; `scripts/it-smoke.sh` sets it.

use std::time::Duration;

use tokio::sync::broadcast::Receiver;
use tokio::time::{Instant, timeout_at};
use voelin_core::{Command, Engine, Event, ObserveState, Source, VoiceOptions, VoiceState};
use voelin_model::ChatTarget;

fn live() -> bool {
	std::env::var("VOELIN_LIVE").is_ok_and(|v| v == "1")
}

/// Wait for an event matching `f`, failing after 20 s.
async fn wait_for(
	rx: &mut Receiver<Event>,
	what: &str,
	mut f: impl FnMut(&Event) -> bool,
) -> Event {
	let deadline = Instant::now() + Duration::from_secs(20);
	loop {
		match timeout_at(deadline, rx.recv()).await {
			Ok(Ok(e)) if f(&e) => return e,
			Ok(Ok(Event::Error { message, .. })) => eprintln!("engine error: {message}"),
			Ok(_) => {}
			Err(_) => panic!("timed out waiting for {what}"),
		}
	}
}

async fn engine_roundtrip(voice_addr: &str, query: voelin_query::Connect) {
	let engine = Engine::start();
	let mut events = engine.subscribe();
	let nick = format!("engine-{}", std::process::id() % 10_000);

	// Session 1: voice.
	engine.send(Command::ConnectVoice {
		session: 1,
		options: Box::new(VoiceOptions::new(voice_addr, &nick)),
	});
	wait_for(&mut events, "voice connected", |e| {
		matches!(e, Event::State { session: 1, state } if state.voice == VoiceState::Connected && state.own_channel.is_some())
	})
	.await;

	// Session 2: invisible, through own query credentials.
	engine.send(Command::ObserveQuery { session: 2, connect: Box::new(query) });
	let (mut observing, mut sees_voice_client) = (false, false);
	wait_for(&mut events, "query presence with the voice client", |e| {
		match e {
			Event::State { session: 2, state } => {
				observing = state.observe == ObserveState::Observing
					&& state.presence_source == Some(Source::Query);
			}
			Event::Presence { session: 2, presence } => {
				sees_voice_client = presence.clients.values().any(|c| c.nickname == nick);
			}
			_ => {}
		}
		observing && sees_voice_client
	})
	.await;

	// Channel chat: voice -> relay.
	engine.send(Command::OpenChat { session: 2, target: ChatTarget::Channel(1) });
	tokio::time::sleep(Duration::from_secs(2)).await;
	engine.send(Command::SendChat {
		session: 1,
		target: ChatTarget::Channel(1),
		text: "engine-hello".into(),
	});
	wait_for(&mut events, "relayed channel message", |e| {
		matches!(e, Event::Chat { session: 2, message } if message.text == "engine-hello" && message.target == ChatTarget::Channel(1))
	})
	.await;

	// And back: relay -> voice.
	engine.send(Command::SendChat {
		session: 2,
		target: ChatTarget::Channel(1),
		text: "from-query".into(),
	});
	wait_for(
		&mut events,
		"message posted by the relay",
		|e| matches!(e, Event::Chat { session: 1, message } if message.text.ends_with("from-query")),
	)
	.await;

	engine.send(Command::CloseSession { session: 1 });
	engine.send(Command::CloseSession { session: 2 });
	tokio::time::sleep(Duration::from_millis(500)).await;
}

fn query(transport: voelin_query::Transport, addr: &str) -> voelin_query::Connect {
	voelin_query::Connect {
		transport,
		addr: addr.into(),
		user: "serveradmin".into(),
		secret: Some("voelin-dev-admin".into()),
		server_port: Some(9987),
		server_id: None,
		line: voelin_query::LineOptions { rate_limit: None, ..Default::default() },
	}
}

#[tokio::test(flavor = "multi_thread")]
async fn ts6_engine() {
	if !live() {
		return;
	}
	engine_roundtrip("127.0.0.1:9988", query(voelin_query::Transport::Ssh, "127.0.0.1:10022"))
		.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ts3_engine() {
	if !live() {
		return;
	}
	engine_roundtrip("127.0.0.1:9987", query(voelin_query::Transport::Raw, "127.0.0.1:10011"))
		.await;
}
