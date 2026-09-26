//! The media pipeline through the engine and the TeamSpeak 6 development
//! server (dev/docker-compose.yml): one session streams the test pattern
//! (VP8 + Opus), another decodes it. Runs only with `VOELIN_LIVE=1`.

#![cfg(feature = "media-desktop")]

use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use tokio::sync::broadcast::Receiver;
use tokio::time::timeout;
use voelin_core::media::voelin_media::capture::SourceId;
use voelin_core::media::voelin_media::capture::synthetic::RECT_COLOR;
use voelin_core::media::voelin_media::{Codec, Codecs, convert};
use voelin_core::media::{Streamer, StreamerConfig, Viewer, peer_config, stream_codec};
use voelin_core::stream::{PeerConfig, StreamSetup};
use voelin_core::{Command, Engine, Event, StreamState, VoiceOptions, VoiceState, WatchState};

fn live() -> bool {
	std::env::var("VOELIN_LIVE").is_ok_and(|v| v == "1")
}

/// Wait for an event matching `f`, failing after 20 s.
async fn wait_for<T>(
	rx: &mut Receiver<Event>,
	what: &str,
	mut f: impl FnMut(&Event) -> Option<T>,
) -> T {
	timeout(Duration::from_secs(20), async {
		loop {
			match rx.recv().await {
				Ok(e) => {
					if let Some(t) = f(&e) {
						return t;
					}
					if let Event::Error { session, message } = e {
						eprintln!("engine error (session {session}): {message}");
					}
				}
				Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
				Err(e) => panic!("event bus closed: {e}"),
			}
		}
	})
	.await
	.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn connect(
	engine: &Engine,
	events: &mut Receiver<Event>,
	session: u64,
	nick: &str,
	peer: PeerConfig,
) {
	let mut options = VoiceOptions::new("127.0.0.1:9988", nick);
	options.stream_peer = peer;
	engine.send(Command::ConnectVoice { session, options: Box::new(options) });
	wait_for(events, "voice connected", |e| match e {
		Event::State { session: s, state }
			if *s == session
				&& state.voice == VoiceState::Connected
				&& state.own_channel.is_some() =>
		{
			Some(())
		}
		_ => None,
	})
	.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ts6_test_pattern_decoded() {
	if !live() {
		eprintln!("skipped: set VOELIN_LIVE=1 with the dev servers running");
		return;
	}
	let engine = Engine::start();
	let mut events = engine.subscribe();
	let codecs = Arc::new(Codecs::new());
	let peer = peer_config(&codecs, PeerConfig::loopback());
	let codec = stream_codec(&codecs, &peer).expect("an encoder");
	assert_eq!(codec, Codec::Vp8);
	let tag = std::process::id() % 10_000;
	connect(&engine, &mut events, 1, &format!("m-streamer-{tag}"), peer.clone()).await;
	connect(&engine, &mut events, 2, &format!("m-viewer-{tag}"), peer).await;

	// Capture first (as the app does: the portal may ask), then go live.
	let config = StreamerConfig {
		source: SourceId::Synthetic,
		synthetic_size: (640, 360),
		bitrate_kbps: 2000,
		codec,
		..StreamerConfig::default()
	};
	let streamer = Streamer::start(&codecs, config).await.unwrap();
	let setup = StreamSetup { name: format!("media {tag}"), ..Default::default() };
	engine.send(Command::StartStream { session: 1, setup, auto_accept: true });
	// The viewer's list may update before the streamer goes live.
	let mut live = None;
	let mut listed = Vec::new();
	let (id, sink) = wait_for(&mut events, "stream live and in the viewer's list", |e| {
		match e {
			Event::StreamState { session: 1, state: StreamState::Live { id, sink } } => {
				live = Some((id.clone(), sink.clone()));
			}
			Event::StreamState { session: 1, state: StreamState::Ended(reason) } => {
				panic!("stream ended: {reason:?}")
			}
			Event::StreamsChanged { session: 2, streams } => {
				listed = streams.iter().map(|s| s.id.clone()).collect();
			}
			_ => {}
		}
		live.clone().filter(|(id, _)| listed.contains(id))
	})
	.await;
	streamer.attach(Arc::new(sink));

	let (tx, pictures) = std_mpsc::channel();
	let viewer = Viewer::start(&engine, 2, &id, codecs, move |picture| {
		let _ = tx.send(picture);
	});
	engine.send(Command::WatchStream { session: 2, stream_id: id.clone() });
	wait_for(&mut events, "watching", |e| match e {
		Event::WatchState { session: 2, stream_id, state: WatchState::Connected }
			if *stream_id == id =>
		{
			Some(())
		}
		Event::WatchState { session: 2, state: WatchState::Ended(reason), .. } => {
			panic!("watching ended: {reason:?}")
		}
		_ => None,
	})
	.await;

	// 60 decoded pictures (two seconds), each with the rectangle in the
	// middle row, about a fifth of the width.
	let widths = tokio::task::spawn_blocking(move || {
		let mut widths = Vec::new();
		while widths.len() < 60 {
			let picture = pictures.recv_timeout(Duration::from_secs(10)).expect("no picture");
			assert_eq!((picture.width, picture.height), (640, 360));
			let rgba = convert::to_rgba_vec(&picture).unwrap();
			let row = &rgba[(picture.height / 2 * picture.width * 4) as usize..]
				[..(picture.width * 4) as usize];
			let rect = row
				.chunks_exact(4)
				.filter(|p| p.iter().zip(RECT_COLOR).all(|(a, b)| a.abs_diff(b) <= 24))
				.count();
			widths.push(rect);
		}
		widths
	})
	.await
	.unwrap();
	assert!(widths.iter().all(|w| (120..=136).contains(w)), "{widths:?}");
	let stats = viewer.stats();
	assert!(stats.error.is_none(), "{stats:?}");
	eprintln!("decoded {stats:?}, sent {:?}", streamer.stats());

	engine.send(Command::StopStream { session: 1 });
	wait_for(&mut events, "stream ended", |e| match e {
		Event::StreamState { session: 1, state: StreamState::Ended(_) } => Some(()),
		_ => None,
	})
	.await;
	drop(viewer);
	drop(streamer);
	engine.send(Command::CloseSession { session: 1 });
	engine.send(Command::CloseSession { session: 2 });
	tokio::time::sleep(Duration::from_millis(500)).await;
}
