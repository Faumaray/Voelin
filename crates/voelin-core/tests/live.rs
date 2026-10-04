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

/// A 1×1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// An HTTP server on 127.0.0.1 that answers every request with `body`;
/// returns its address. The engine fetches banners itself (the server only
/// passes their addresses on), so it reaches this one.
async fn serve(body: Vec<u8>) -> String {
	use tokio::io::{AsyncReadExt, AsyncWriteExt};
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	tokio::spawn(async move {
		while let Ok((mut socket, _)) = listener.accept().await {
			let body = body.clone();
			tokio::spawn(async move {
				let mut request = Vec::new();
				let mut buf = [0u8; 1024];
				while !request.windows(4).any(|w| w == b"\r\n\r\n") {
					match socket.read(&mut buf).await {
						Ok(0) | Err(_) => return,
						Ok(n) => request.extend_from_slice(&buf[..n]),
					}
				}
				let head = format!(
					"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
					body.len()
				);
				let _ = socket.write_all(head.as_bytes()).await;
				let _ = socket.write_all(&body).await;
			});
		}
	});
	format!("http://{addr}")
}

/// The host banner and a channel banner set through ServerQuery reach the
/// engine's cache (`Event::PictureReady`); the server is put back after.
#[tokio::test(flavor = "multi_thread")]
async fn ts6_banners() {
	use base64::Engine as _;
	use voelin_model::BannerMode;
	use voelin_query::{Command as Query, QueryClient};
	if !live() {
		return;
	}
	let png = base64::prelude::BASE64_STANDARD.decode(PNG).unwrap();
	let base = serve(png.clone()).await;
	let (host_url, channel_url) = (format!("{base}/host.png"), format!("{base}/channel.png"));
	let (admin, _) = QueryClient::connect(&query(voelin_query::Transport::Ssh, "127.0.0.1:10022"))
		.await
		.unwrap();
	let info = admin.send(&Query::new("serverinfo")).await.unwrap().remove(0);
	let before = |key: &str| info.get(key).unwrap_or_default().to_owned();
	let (old_url, old_mode) =
		(before("virtualserver_hostbanner_gfx_url"), before("virtualserver_hostbanner_mode"));
	let edit = Query::new("serveredit")
		.arg("virtualserver_hostbanner_gfx_url", &host_url)
		.arg("virtualserver_hostbanner_mode", 2);
	admin.send(&edit).await.unwrap();
	let created = admin
		.send(
			&Query::new("channelcreate")
				.arg("channel_name", format!("banner-{}", std::process::id()))
				.arg("channel_banner_gfx_url", &channel_url)
				.arg("channel_banner_mode", 1),
		)
		.await;

	let check = {
		let (host_url, channel_url) = (host_url.clone(), channel_url.clone());
		tokio::spawn(async move {
			let engine = Engine::start();
			let dir =
				std::env::temp_dir().join(format!("voelin-live-banners-{}", std::process::id()));
			engine.send(Command::AttachCache(dir.clone()));
			let mut events = engine.subscribe();
			engine.send(Command::ConnectVoice {
				session: 1,
				options: Box::new(VoiceOptions::new("127.0.0.1:9988", "banner-check")),
			});
			let mut seen = std::collections::HashMap::new();
			let mut channel_mode = None;
			wait_for(&mut events, "both banners", |e| {
				match e {
					Event::PictureReady { url, path, .. } => {
						seen.insert(url.clone(), std::fs::read(path).unwrap());
					}
					Event::Presence { presence, .. } => {
						channel_mode = presence
							.channels
							.values()
							.find(|c| c.banner_gfx_url.as_deref() == Some(channel_url.as_str()))
							.map(|c| c.banner_mode);
						assert_eq!(presence.server.banner_mode, BannerMode::KeepAspect);
					}
					_ => {}
				}
				seen.contains_key(&host_url) && seen.contains_key(&channel_url)
			})
			.await;
			assert_eq!(channel_mode, Some(BannerMode::IgnoreAspect));
			assert!(seen.values().all(|bytes| *bytes == png));
			engine.send(Command::CloseSession { session: 1 });
			tokio::time::sleep(Duration::from_millis(500)).await;
			let _ = std::fs::remove_dir_all(dir);
		})
	};
	let result = check.await;

	if let Ok(rows) = &created
		&& let Some(cid) = rows.first().and_then(|r| r.get("cid"))
	{
		let delete = Query::new("channeldelete").arg("cid", cid).arg("force", 1);
		admin.send(&delete).await.unwrap();
	}
	let restore = Query::new("serveredit")
		.arg("virtualserver_hostbanner_gfx_url", old_url)
		.arg("virtualserver_hostbanner_mode", old_mode);
	admin.send(&restore).await.unwrap();
	created.unwrap();
	if let Err(e) = result {
		std::panic::resume_unwind(e.into_panic());
	}
}
