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

/// An HTTP server that fails each URL once with 503, then returns `body`;
/// returns its address. The engine fetches banners itself (the server only
/// passes their addresses on), so it reaches this one.
async fn serve(body: Vec<u8>) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
	use tokio::io::{AsyncReadExt, AsyncWriteExt};
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
	let count = requests.clone();
	let seen = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
	tokio::spawn(async move {
		while let Ok((mut socket, _)) = listener.accept().await {
			let body = body.clone();
			let count = count.clone();
			let seen = seen.clone();
			tokio::spawn(async move {
				let mut request = Vec::new();
				let mut buf = [0u8; 1024];
				while !request.windows(4).any(|w| w == b"\r\n\r\n") {
					match socket.read(&mut buf).await {
						Ok(0) | Err(_) => return,
						Ok(n) => request.extend_from_slice(&buf[..n]),
					}
				}
				count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
				let target =
					String::from_utf8_lossy(&request).split_whitespace().nth(1).unwrap().to_owned();
				let first = seen.lock().unwrap().insert(target);
				if first {
					let _ = socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
					return;
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
	(format!("http://{addr}"), requests)
}

/// Banner policy, channel edits/removal and the host refresh against a real TS6.
/// Override only this test's endpoints to use a private fixture. Server settings
/// and the temporary channel are restored even when the assertions panic.
#[tokio::test(flavor = "multi_thread")]
async fn ts6_banners() {
	use base64::Engine as _;
	use std::sync::atomic::Ordering;
	use voelin_core::settings::CACHE_FETCH_IMAGES;
	use voelin_model::BannerMode;
	use voelin_query::{Command as Query, QueryClient};
	if !live() {
		return;
	}
	let voice_addr =
		std::env::var("VOELIN_BANNER_VOICE_ADDR").unwrap_or_else(|_| "127.0.0.1:9988".into());
	let query_addr =
		std::env::var("VOELIN_BANNER_QUERY_ADDR").unwrap_or_else(|_| "127.0.0.1:10022".into());
	let png = base64::prelude::BASE64_STANDARD.decode(PNG).unwrap();
	let (base, requests) = serve(png.clone()).await;
	let (host_url, channel_url) = (format!("{base}/host.png"), format!("{base}/channel.png"));
	let (admin, _) =
		QueryClient::connect(&query(voelin_query::Transport::Ssh, &query_addr)).await.unwrap();
	let info = admin.send(&Query::new("serverinfo")).await.unwrap().remove(0);
	let before = |key: &str| info.get(key).unwrap_or_default().to_owned();
	let restore = Query::new("serveredit")
		.arg("virtualserver_hostbanner_gfx_url", before("virtualserver_hostbanner_gfx_url"))
		.arg("virtualserver_hostbanner_mode", before("virtualserver_hostbanner_mode"))
		.arg(
			"virtualserver_hostbanner_gfx_interval",
			before("virtualserver_hostbanner_gfx_interval"),
		);
	let created = admin
		.send(
			&Query::new("channelcreate")
				.arg("channel_name", format!("banner-{}", std::process::id()))
				.arg("channel_flag_permanent", 1)
				.arg("channel_banner_gfx_url", &channel_url)
				.arg("channel_banner_mode", 1),
		)
		.await
		.unwrap();
	let cid = created[0].get("cid").unwrap().to_owned();
	let channel_id: u64 = cid.parse().unwrap();
	let engine = Engine::start();
	engine.settings().set(&CACHE_FETCH_IMAGES, false).unwrap();
	let dir = std::env::temp_dir().join(format!("voelin-live-banners-{}", std::process::id()));
	engine.send(Command::AttachCache(dir.clone()));
	let check = {
		let (admin, engine, cid) = (admin.clone(), engine.clone(), cid.clone());
		tokio::spawn(async move {
			admin
				.send(
					&Query::new("serveredit")
						.arg("virtualserver_hostbanner_gfx_url", &host_url)
						.arg("virtualserver_hostbanner_mode", 2)
						.arg("virtualserver_hostbanner_gfx_interval", 60),
				)
				.await
				.unwrap();
			let mut events = engine.subscribe();
			engine.send(Command::ConnectVoice {
				session: 1,
				options: Box::new(VoiceOptions::new(&voice_addr, "banner-check")),
			});
			wait_for(&mut events, "banner metadata with fetching disabled", |e| {
				assert!(
					!matches!(e, Event::PictureReady { .. }),
					"disabled image fetching emitted a picture"
				);
				matches!(e, Event::Presence { presence, .. }
					if presence.server.banner_gfx_url == host_url
					&& presence.server.banner_mode == BannerMode::KeepAspect
					&& presence.channels.get(&channel_id).is_some_and(|c|
						c.banner_gfx_url.as_deref() == Some(channel_url.as_str())
						&& c.banner_mode == BannerMode::IgnoreAspect))
			})
			.await;
			assert_eq!(
				requests.load(Ordering::SeqCst),
				0,
				"disabled policy must prevent HTTP requests"
			);

			// A new presence after opting in must fetch both existing banners.
			engine.settings().set(&CACHE_FETCH_IMAGES, true).unwrap();
			admin
				.send(&Query::new("channeledit").arg("cid", &cid).arg("channel_banner_mode", 2))
				.await
				.unwrap();
			let mut seen = std::collections::HashSet::new();
			let mut changed_mode = false;
			wait_for(&mut events, "both images and changed sizing mode", |e| {
				match e {
					Event::PictureReady { url, path, .. } => {
						assert_eq!(std::fs::read(path).unwrap(), png);
						seen.insert(url.clone());
					}
					Event::Presence { presence, .. } => {
						changed_mode = presence
							.channels
							.get(&channel_id)
							.is_some_and(|c| c.banner_mode == BannerMode::KeepAspect);
					}
					_ => {}
				}
				changed_mode && seen.contains(&host_url) && seen.contains(&channel_url)
			})
			.await;

			assert_eq!(
				requests.load(Ordering::SeqCst),
				4,
				"both initial URLs must retry once after their 503 response"
			);
			let replacement = format!("{base}/replacement.png");
			admin
				.send(
					&Query::new("channeledit")
						.arg("cid", &cid)
						.arg("channel_banner_gfx_url", &replacement)
						.arg("channel_banner_mode", 0),
				)
				.await
				.unwrap();
			let (mut metadata, mut image) = (false, false);
			wait_for(&mut events, "replacement channel banner", |e| {
				match e {
					Event::Presence { presence, .. } => {
						metadata = presence.channels.get(&channel_id).is_some_and(|c| {
							c.banner_gfx_url.as_deref() == Some(replacement.as_str())
								&& c.banner_mode == BannerMode::NoAdjust
						});
					}
					Event::PictureReady { url, path, .. } if *url == replacement => {
						assert_eq!(std::fs::read(path).unwrap(), png);
						image = true;
					}
					_ => {}
				}
				metadata && image
			})
			.await;
			admin
				.send(&Query::new("channeledit").arg("cid", &cid).arg("channel_banner_gfx_url", ""))
				.await
				.unwrap();
			wait_for(&mut events, "channel banner removed", |e| {
				matches!(e, Event::Presence { presence, .. } if
					presence.channels.get(&channel_id).is_some_and(|c|
						c.banner_gfx_url.as_deref().is_none_or(str::is_empty)))
			})
			.await;

			// The same host URL must be downloaded again, bypassing its cached file.
			let before_reload = requests.load(Ordering::SeqCst);
			let deadline = Instant::now() + Duration::from_secs(75);
			loop {
				let event = timeout_at(deadline, events.recv())
					.await
					.expect("host banner did not reload after its 60-second interval")
					.unwrap();
				if let Event::PictureReady { url, path, .. } = event
					&& url == host_url
				{
					assert_eq!(std::fs::read(path).unwrap(), png);
					assert!(
						requests.load(Ordering::SeqCst) > before_reload,
						"reload must make a fresh HTTP request"
					);
					break;
				}
			}
		})
	};
	let result = check.await;
	engine.send(Command::CloseSession { session: 1 });
	// Attempt both cleanup operations before reporting either failure.
	let deleted = admin.send(&Query::new("channeldelete").arg("cid", &cid).arg("force", 1)).await;
	let restored = admin.send(&restore).await;
	let _ = std::fs::remove_dir_all(dir);
	deleted.unwrap();
	restored.unwrap();
	if let Err(error) = result {
		std::panic::resume_unwind(error.into_panic());
	}
}

/// Upload through one independent identity/cache and download through another.
/// This proves the server file path and initial avatar metadata, rather than
/// accepting the uploader's local cache insertion as a successful download.
#[tokio::test(flavor = "multi_thread")]
async fn ts6_avatars() {
	use base64::Engine as _;
	if !live() {
		return;
	}
	let voice_addr =
		std::env::var("VOELIN_BANNER_VOICE_ADDR").unwrap_or_else(|_| "127.0.0.1:9988".into());
	let dir = std::env::temp_dir().join(format!("voelin-live-avatars-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	let image = dir.join("upload.png");
	let png = base64::prelude::BASE64_STANDARD.decode(PNG).unwrap();
	std::fs::write(&image, &png).unwrap();
	let uploader = Engine::start();
	let reader = Engine::start();
	uploader.send(Command::AttachCache(dir.join("uploader")));
	reader.send(Command::AttachCache(dir.join("reader")));
	let check = {
		let (uploader, reader, dir) = (uploader.clone(), reader.clone(), dir.clone());
		tokio::spawn(async move {
			let nickname = format!("avatar-owner-{}", std::process::id());
			let mut owner_events = uploader.subscribe();
			uploader.send(Command::ConnectVoice {
				session: 1,
				options: Box::new(VoiceOptions::new(&voice_addr, &nickname)),
			});
			let presence = wait_for(&mut owner_events, "avatar owner presence", |event| {
				matches!(event, Event::Presence { presence, .. } if presence.clients.values().any(|client| client.nickname == nickname && client.uid.is_some()))
			}).await;
			let Event::Presence { presence, .. } = presence else { unreachable!() };
			let uid = presence
				.clients
				.values()
				.find(|client| client.nickname == nickname)
				.unwrap()
				.uid
				.clone()
				.unwrap();
			uploader.send(Command::SetAvatar { session: 1, request: 1, image: Some(image) });
			let done = wait_for(&mut owner_events, "avatar upload", |event| {
				matches!(event, Event::RequestDone { request: 1, .. })
			})
			.await;
			assert!(matches!(done, Event::RequestDone { result: Ok(()), .. }), "{done:?}");
			let mut reader_events = reader.subscribe();
			reader.send(Command::ConnectVoice {
				session: 1,
				options: Box::new(VoiceOptions::new(
					&voice_addr,
					format!("avatar-reader-{}", std::process::id()),
				)),
			});
			let downloaded = wait_for(
				&mut reader_events,
				"avatar downloaded by independent client",
				|event| matches!(event, Event::AvatarReady { client_uid, .. } if *client_uid == uid),
			)
			.await;
			let Event::AvatarReady { path, hash, .. } = downloaded else { unreachable!() };
			assert!(path.starts_with(dir.join("reader")));
			assert_eq!(std::fs::read(path).unwrap(), png);
			assert_eq!(hash, format!("{:x}", md5::compute(&png)));
		})
	};
	let result = check.await;
	// The identity was generated for this test; remove its server-side file too.
	let mut cleanup = uploader.subscribe();
	uploader.send(Command::SetAvatar { session: 1, request: 2, image: None });
	let _ = tokio::time::timeout(Duration::from_secs(5), async {
		while let Ok(event) = cleanup.recv().await {
			if matches!(event, Event::RequestDone { request: 2, .. }) {
				break;
			}
		}
	})
	.await;
	uploader.send(Command::CloseSession { session: 1 });
	reader.send(Command::CloseSession { session: 1 });
	let _ = std::fs::remove_dir_all(dir);
	if let Err(error) = result {
		std::panic::resume_unwind(error.into_panic());
	}
}
