//! Streams through the engine against the development servers
//! (dev/docker-compose.yml): one session streams synthetic frames, another
//! watches. Runs only with `VOELIN_LIVE=1`.

use std::time::{Duration, Instant};

use tokio::sync::broadcast::Receiver;
use tokio::time::timeout;
use voelin_core::stream::{
	EndReason, FrameSource, LeaveReason, MediaKind, PeerConfig, StreamSetup, SyntheticSource,
	ViewerState,
};
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

/// Wait until both `f` and `g` matched an event, in either order. Events of
/// the two sessions race, so waiting for them one after the other can drop one.
async fn wait_both<A, B>(
	rx: &mut Receiver<Event>,
	what: &str,
	mut f: impl FnMut(&Event) -> Option<A>,
	mut g: impl FnMut(&Event) -> Option<B>,
) -> (A, B) {
	let (mut a, mut b) = (None, None);
	wait_for(rx, what, |e| {
		if a.is_none() {
			a = f(e);
		}
		if b.is_none() {
			b = g(e);
		}
		(a.is_some() && b.is_some()).then_some(())
	})
	.await;
	(a.unwrap(), b.unwrap())
}

async fn connect(
	engine: &Engine,
	events: &mut Receiver<Event>,
	session: u64,
	addr: &str,
	nick: &str,
) {
	let mut options = VoiceOptions::new(addr, nick);
	options.stream_peer = PeerConfig::loopback();
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
async fn ts6_stream_between_sessions() {
	if !live() {
		eprintln!("skipped: set VOELIN_LIVE=1 with the dev servers running");
		return;
	}
	let engine = Engine::start();
	let mut events = engine.subscribe();
	let mut frames = engine.subscribe_frames();
	let tag = std::process::id() % 10_000;
	connect(&engine, &mut events, 1, "127.0.0.1:9988", &format!("e-streamer-{tag}")).await;
	connect(&engine, &mut events, 2, "127.0.0.1:9988", &format!("e-viewer-{tag}")).await;

	// Stream, asking for each viewer.
	let setup = StreamSetup { name: format!("engine {tag}"), ..Default::default() };
	engine.send(Command::StartStream { session: 1, setup, auto_accept: false });
	// The viewer's list often updates before the streamer goes live, so wait
	// for both at once and remember the latest list.
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

	// Synthetic frames through the sink, as an encoder would send them.
	let feeder = tokio::spawn({
		let sink = sink.clone();
		async move {
			let mut source = SyntheticSource::new(30, 3000, true);
			let mut out = Vec::new();
			while sink.is_live() {
				source.poll_frames(Instant::now(), &mut out);
				for frame in out.drain(..) {
					sink.send(frame);
				}
				tokio::time::sleep(Duration::from_millis(10)).await;
			}
		}
	});

	engine.send(Command::WatchStream { session: 2, stream_id: id.clone() });
	let viewer = wait_for(&mut events, "viewer request", |e| match e {
		Event::StreamViewerRequest { session: 1, viewer, .. } => Some(*viewer),
		_ => None,
	})
	.await;
	engine.send(Command::AcceptViewer { session: 1, viewer, accept: true });
	wait_both(
		&mut events,
		"watching and viewer connected",
		|e| match e {
			Event::WatchState { session: 2, stream_id, state: WatchState::Connected }
				if *stream_id == id =>
			{
				Some(())
			}
			Event::WatchState { session: 2, state: WatchState::Ended(reason), .. } => {
				panic!("watching ended: {reason:?}")
			}
			_ => None,
		},
		|e| match e {
			Event::StreamViewers { session: 1, viewers }
				if viewers
					.iter()
					.any(|v| v.client.0 == viewer && v.state == ViewerState::Connected) =>
			{
				Some(())
			}
			_ => None,
		},
	)
	.await;
	assert!(sink.take_keyframe_request(), "a new viewer asks for a keyframe");

	let (mut video, mut audio) = (0, 0);
	let _ = timeout(Duration::from_secs(10), async {
		while video < 60 || audio < 60 {
			match frames.recv().await {
				Ok(f) if f.session == 2 && f.stream_id == id => match f.frame.kind {
					MediaKind::Video => video += 1,
					MediaKind::Audio => audio += 1,
				},
				Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
				Err(_) => break,
			}
		}
	})
	.await;
	assert!(video >= 60 && audio >= 60, "viewer got {video} video / {audio} audio frames");

	// The streamer removes the viewer, then ends the stream.
	engine.send(Command::KickViewer { session: 1, viewer });
	wait_for(&mut events, "kicked", |e| match e {
		Event::WatchState {
			session: 2,
			state: WatchState::Ended(EndReason::Removed(Some(LeaveReason::Kicked))),
			..
		} => Some(()),
		_ => None,
	})
	.await;
	engine.send(Command::StopStream { session: 1 });
	wait_both(
		&mut events,
		"stream ended and gone from the viewer's list",
		|e| match e {
			Event::StreamState { session: 1, state: StreamState::Ended(EndReason::Local) } => {
				Some(())
			}
			_ => None,
		},
		|e| match e {
			Event::StreamsChanged { session: 2, streams } if streams.iter().all(|s| s.id != id) => {
				Some(())
			}
			_ => None,
		},
	)
	.await;
	assert!(!sink.send(voelin_core::stream::EncodedFrame {
		kind: MediaKind::Audio,
		time: voelin_core::stream::MediaTime::from_90khz(0),
		data: vec![0].into(),
		layer: 0,
		keyframe: false,
	}));
	feeder.await.unwrap();

	engine.send(Command::CloseSession { session: 1 });
	engine.send(Command::CloseSession { session: 2 });
	tokio::time::sleep(Duration::from_millis(500)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ts3_has_no_streams() {
	if !live() {
		return;
	}
	let engine = Engine::start();
	let mut events = engine.subscribe();
	let nick = format!("e-nostream-{}", std::process::id() % 10_000);
	connect(&engine, &mut events, 1, "127.0.0.1:9987", &nick).await;
	engine.send(Command::StartStream {
		session: 1,
		setup: StreamSetup::default(),
		auto_accept: true,
	});
	let message = wait_for(&mut events, "error", |e| match e {
		Event::Error { session: 1, message } => Some(message.clone()),
		_ => None,
	})
	.await;
	assert!(message.contains("TeamSpeak 6"), "{message}");
	engine.send(Command::CloseSession { session: 1 });
	tokio::time::sleep(Duration::from_millis(500)).await;
}
