//! Two stream session managers talking through a fake server that relays the
//! commands like the TeamSpeak 6 server does (see
//! `docs/protocol-notes/ts6-streaming.md`), with real peers on loopback.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use tokio::time::timeout;
use tsclientlib::ClientId;
use voelin_stream::{
	ClientState, EndReason, FrameSource, LeaveReason, MediaKind, Output, PeerConfig, Request,
	StreamEvent, StreamInfo, StreamKind, StreamNotification, StreamSetup, StreamerEvent,
	StreamerOptions, Streams, SyntheticSource, ViewerState, WatchEvent,
};

const STREAMER: usize = 0;
const VIEWER: usize = 1;
const IDS: [ClientId; 2] = [ClientId(5), ClientId(6)];

/// Relays requests to notifications, as observed on a TS6 server.
#[derive(Default)]
struct FakeServer {
	streams: HashMap<String, StreamInfo>,
	next: u32,
	/// A client that is not in the channel yet: streams that start are not
	/// announced to it.
	absent: Option<usize>,
}

impl FakeServer {
	fn relay(&mut self, from: ClientId, request: Request) -> Vec<(usize, StreamNotification)> {
		let everyone = |n: StreamNotification| (0..2).map(move |i| (i, n.clone()));
		let to = |client: ClientId| IDS.iter().position(|c| *c == client).unwrap();
		match request {
			Request::Setup(setup) => {
				self.next += 1;
				let id = format!("stream-{}", self.next);
				let info = StreamInfo {
					id,
					streamer: from,
					name: setup.name,
					kind: setup.kind,
					bitrate: setup.bitrate,
					viewer_limit: setup.viewer_limit,
					audio: setup.audio,
				};
				self.streams.insert(info.id.clone(), info.clone());
				(0..2)
					.filter(|i| self.absent != Some(*i))
					.map(|i| {
						let own = IDS[i] == from;
						let return_code = own.then(|| "1".to_owned());
						(i, StreamNotification::Started { info: info.clone(), return_code })
					})
					.collect()
			}
			Request::Stop { id } => {
				self.streams.remove(&id);
				everyone(StreamNotification::Stopped { id, streamer: Some(from), reason: None })
					.collect()
			}
			Request::Join { id, streamer, message } => vec![(
				to(streamer),
				StreamNotification::JoinRequest { id, viewer: from, message, remove: false },
			)],
			Request::Leave { id, streamer } => vec![(
				to(streamer),
				StreamNotification::JoinRequest {
					id,
					viewer: from,
					message: String::new(),
					remove: true,
				},
			)],
			Request::Respond { id, viewer, offer, accept } => vec![(
				to(viewer),
				StreamNotification::JoinResponse {
					id,
					streamer: Some(from),
					accepted: accept,
					offer,
					message: String::new(),
				},
			)],
			Request::Signal { id, peer, signal } => vec![(
				to(peer),
				StreamNotification::Signaling { id, peer: from, json: signal.to_json() },
			)],
			Request::RemoveViewer { id, viewer, reason } => {
				everyone(StreamNotification::ViewerLeft { id, viewer, reason: Some(reason) })
					.collect()
			}
			Request::StreamInfo { streamer } => self
				.streams
				.values()
				.filter(|s| s.streamer == streamer)
				.map(|s| (to(from), StreamNotification::Info(s.clone())))
				.collect(),
		}
	}
}

struct Net {
	server: FakeServer,
	clients: [Streams; 2],
	/// Events not looked at yet, with the client index.
	events: Vec<(usize, StreamEvent)>,
	source: Option<SyntheticSource>,
	video: usize,
	audio: usize,
}

impl Net {
	fn new() -> Self {
		let config = PeerConfig::loopback();
		Self {
			server: FakeServer::default(),
			clients: IDS.map(|id| Streams::new(id, config.clone())),
			events: Vec::new(),
			source: None,
			video: 0,
			audio: 0,
		}
	}

	/// Deliver queued requests until nothing is left.
	async fn flush(&mut self) {
		loop {
			let mut notes = Vec::new();
			for (i, client) in self.clients.iter_mut().enumerate() {
				while let Some(output) = client.poll_output() {
					match output {
						Output::Request(r) => notes.extend(self.server.relay(IDS[i], r)),
						Output::Event(StreamEvent::Watch {
							event: WatchEvent::Frame(f), ..
						}) => match f.kind {
							MediaKind::Video => self.video += 1,
							MediaKind::Audio => self.audio += 1,
						},
						Output::Event(e) => self.events.push((i, e)),
					}
				}
			}
			if notes.is_empty() {
				return;
			}
			for (to, n) in notes {
				self.clients[to].handle_notification(n).await;
			}
		}
	}

	/// Handle queued requests, then wait for peer events or the next frame tick.
	async fn step(&mut self) {
		self.flush().await;
		let [a, b] = &mut self.clients;
		tokio::select! {
			() = a.wait_peers() => {}
			() = b.wait_peers() => {}
			() = tokio::time::sleep(Duration::from_millis(10)) => {}
		}
		if let Some(source) = &mut self.source {
			let mut frames = Vec::new();
			source.poll_frames(Instant::now(), &mut frames);
			for frame in &frames {
				self.clients[STREAMER].write_frame(frame);
			}
		}
	}

	/// Run the network until `f` accepts an event; earlier events are dropped.
	async fn until(&mut self, what: &str, mut f: impl FnMut(usize, &StreamEvent) -> bool) {
		timeout(Duration::from_secs(10), async {
			loop {
				self.flush().await;
				if let Some(pos) = self.events.iter().position(|(i, e)| f(*i, e)) {
					self.events.drain(..=pos);
					return;
				}
				self.step().await;
			}
		})
		.await
		.unwrap_or_else(|_| panic!("timed out waiting for {what}; events: {:?}", self.events));
	}

	/// Run until the viewer received `n` more video and audio frames.
	async fn frames(&mut self, n: usize) {
		let (video, audio) = (self.video + n, self.audio + n);
		let deadline = Instant::now() + Duration::from_secs(10);
		while self.video < video || self.audio < audio {
			assert!(
				Instant::now() < deadline,
				"got {} video / {} audio frames",
				self.video,
				self.audio
			);
			self.step().await;
		}
	}
}

fn watch_event(event: &StreamEvent, f: impl Fn(&WatchEvent) -> bool) -> bool {
	matches!(event, StreamEvent::Watch { event, .. } if f(event))
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_between_sessions() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let mut net = Net::new();
	let setup = StreamSetup { name: "fake".into(), kind: StreamKind::Screen, ..Default::default() };
	net.clients[STREAMER]
		.start(StreamerOptions { setup, auto_accept: false, ..Default::default() })
		.unwrap();
	net.until("live", |i, e| {
		i == STREAMER && matches!(e, StreamEvent::Streamer(StreamerEvent::Live { .. }))
	})
	.await;
	net.until("stream in the viewer's list", |i, e| {
		i == VIEWER && matches!(e, StreamEvent::Streams(l) if l.len() == 1)
	})
	.await;
	let id = net.clients[VIEWER].directory().by_streamer(IDS[STREAMER]).unwrap().id.clone();

	// Join, accepted by the streamer.
	net.clients[VIEWER].watch(&id, "let me see").unwrap();
	net.until("join request", |i, e| {
		i == STREAMER && matches!(e, StreamEvent::Streamer(StreamerEvent::Request { .. }))
	})
	.await;
	net.clients[STREAMER].respond(IDS[VIEWER], true).await.unwrap();
	net.until("viewer connected", |i, e| {
		i == VIEWER && watch_event(e, |e| matches!(e, WatchEvent::Connected))
	})
	.await;
	net.until("streamer sees the viewer", |i, e| {
		i == STREAMER
			&& matches!(e, StreamEvent::Streamer(StreamerEvent::Viewers(v))
				if v.len() == 1 && v[0].state == ViewerState::Connected)
	})
	.await;

	// Media.
	net.source = Some(SyntheticSource::new(30, 4000, true));
	net.frames(20).await;

	// The viewer reconnects: new offer, new answer, media again.
	net.clients[VIEWER].reconnect(&id).unwrap();
	net.until("reconnected", |i, e| {
		i == VIEWER && watch_event(e, |e| matches!(e, WatchEvent::Connected))
	})
	.await;
	net.frames(20).await;

	// The streamer kicks the viewer.
	net.clients[STREAMER].kick(IDS[VIEWER]).unwrap();
	net.until("kicked", |i, e| {
		i == VIEWER
			&& watch_event(e, |e| {
				matches!(e, WatchEvent::Ended(EndReason::Removed(Some(LeaveReason::Kicked))))
			})
	})
	.await;
	assert_eq!(net.clients[VIEWER].watching().count(), 0);

	// Watch again, then the viewer leaves by itself.
	net.clients[VIEWER].watch(&id, "").unwrap();
	net.until("second join request", |i, e| {
		i == STREAMER && matches!(e, StreamEvent::Streamer(StreamerEvent::Request { .. }))
	})
	.await;
	net.clients[STREAMER].respond(IDS[VIEWER], true).await.unwrap();
	net.until("viewer connected again", |i, e| {
		i == VIEWER && watch_event(e, |e| matches!(e, WatchEvent::Connected))
	})
	.await;
	net.frames(5).await;
	net.clients[VIEWER].leave(&id).unwrap();
	net.until("viewer gone", |i, e| {
		i == STREAMER
			&& matches!(e, StreamEvent::Streamer(StreamerEvent::Viewers(v)) if v.is_empty())
	})
	.await;

	// The streamer stops; the viewer's list empties.
	net.clients[STREAMER].stop().unwrap();
	net.until("stream gone", |i, e| {
		i == VIEWER && matches!(e, StreamEvent::Streams(l) if l.is_empty())
	})
	.await;
	assert!(net.clients[STREAMER].streamer().is_none());
}

/// The viewer arrives after the stream started: the server did not announce
/// it, the viewer looks it up (`requeststreaminfo`) and watches.
#[tokio::test(flavor = "multi_thread")]
async fn late_viewer_finds_the_stream() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let mut net = Net::new();
	net.server.absent = Some(VIEWER);
	let setup = StreamSetup { name: "early".into(), ..Default::default() };
	net.clients[STREAMER]
		.start(StreamerOptions { setup, auto_accept: true, ..Default::default() })
		.unwrap();
	net.until("live", |i, e| {
		i == STREAMER && matches!(e, StreamEvent::Streamer(StreamerEvent::Live { .. }))
	})
	.await;
	net.flush().await;
	assert!(net.clients[VIEWER].directory().iter().next().is_none(), "not announced");

	// The viewer's first client list: the streamer streams in its channel.
	net.server.absent = None;
	let clients: BTreeMap<u16, ClientState> = IDS
		.iter()
		.map(|c| (c.0, ClientState { channel: 1, streaming: Some(c == &IDS[STREAMER]) }))
		.collect();
	net.clients[VIEWER].update_clients(clients);
	net.until("looked up stream in the viewer's list", |i, e| {
		i == VIEWER && matches!(e, StreamEvent::Streams(l) if l.len() == 1)
	})
	.await;
	let id = net.clients[VIEWER].directory().by_streamer(IDS[STREAMER]).unwrap().id.clone();
	net.clients[VIEWER].watch(&id, "").unwrap();
	net.until("viewer connected", |i, e| {
		i == VIEWER && watch_event(e, |e| matches!(e, WatchEvent::Connected))
	})
	.await;
	net.source = Some(SyntheticSource::new(30, 4000, true));
	net.frames(10).await;
	net.clients[STREAMER].stop().unwrap();
	net.until("stream gone", |i, e| {
		i == VIEWER && matches!(e, StreamEvent::Streams(l) if l.is_empty())
	})
	.await;
}
