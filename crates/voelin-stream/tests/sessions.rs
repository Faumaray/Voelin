//! Two stream session managers talking through a fake server that relays the
//! commands like the TeamSpeak 6 server does (see
//! `docs/protocol-notes/ts6-streaming.md`), with real peers on loopback.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use tokio::time::timeout;
use tsclientlib::ClientId;
use voelin_stream::{
	ClientState, EndReason, FrameSource, LayerSpec, LeaveReason, MediaKind, Output, PeerConfig,
	Request, SessionError, Signal, SrtpProfile, StreamEvent, StreamInfo, StreamKind,
	StreamNotification, StreamSetup, StreamerEvent, StreamerOptions, Streams, SyntheticSource,
	VideoCodec, VideoFormat, ViewerState, WatchEvent,
};

mod relay;
use relay::{Relay, Rule};

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
					viewers: Some(0),
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
	/// The layer of the last video frame the viewer got (layered sources).
	layer: Option<u8>,
	/// Every signal relayed, with the sender's index.
	signals: Vec<(usize, Signal)>,
	/// Media goes through a relay with this rule, one per connection.
	relay: Option<Rule>,
	relays: Vec<Relay>,
	/// The format the source's video is in (`None`: every viewer gets it).
	video_format: Option<VideoFormat>,
}

impl Net {
	fn new() -> Self {
		let config = PeerConfig::loopback();
		Self::with(config.clone(), config, None)
	}

	/// The streamer's and the viewer's peer configurations, media through a
	/// relay with `relay`.
	fn with(streamer: PeerConfig, viewer: PeerConfig, relay: Option<Rule>) -> Self {
		Self {
			server: FakeServer::default(),
			clients: [Streams::new(IDS[STREAMER], streamer), Streams::new(IDS[VIEWER], viewer)],
			events: Vec::new(),
			source: None,
			video: 0,
			audio: 0,
			layer: None,
			signals: Vec::new(),
			relay,
			relays: Vec::new(),
			video_format: None,
		}
	}

	/// `request` of client `from` with its SDP pointing at a new relay (an
	/// offer) or the last one (an answer).
	async fn through_relay(&mut self, from: usize, mut request: Request) -> Request {
		let Some(rule) = self.relay else { return request };
		match &mut request {
			Request::Respond { offer: Some(sdp), .. }
			| Request::Signal { signal: Signal::Offer { sdp, .. }, .. }
				if from == STREAMER =>
			{
				let relay = Relay::new(rule).await;
				*sdp = relay.offer(sdp);
				self.relays.push(relay);
			}
			Request::Signal { signal: Signal::Answer { sdp }, .. } if from == VIEWER => {
				*sdp = self.relays.last().expect("an offer went first").answer(sdp);
			}
			_ => {}
		}
		request
	}

	/// Deliver queued requests until nothing is left.
	async fn flush(&mut self) {
		loop {
			let mut outputs = Vec::new();
			for (i, client) in self.clients.iter_mut().enumerate() {
				while let Some(output) = client.poll_output() {
					outputs.push((i, output));
				}
			}
			let mut notes = Vec::new();
			for (i, output) in outputs {
				match output {
					Output::Request(r) => {
						let r = self.through_relay(i, r).await;
						if let Request::Signal { signal, .. } = &r {
							self.signals.push((i, signal.clone()));
						}
						notes.extend(self.server.relay(IDS[i], r));
					}
					Output::Event(StreamEvent::Watch { event: WatchEvent::Frame(f), .. }) => {
						match f.kind {
							MediaKind::Video => {
								self.video += 1;
								self.layer = SyntheticSource::frame_layer(&f.data);
							}
							MediaKind::Audio => self.audio += 1,
						}
					}
					Output::Event(e) => self.events.push((i, e)),
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
				let format = self.video_format.filter(|_| frame.kind == MediaKind::Video);
				self.clients[STREAMER].write_frame_in(frame, format);
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

	/// Run until the viewer's video comes from `layer` (and the streamer
	/// reports it on that layer).
	async fn on_layer(&mut self, layer: u8) {
		let deadline = Instant::now() + Duration::from_secs(10);
		loop {
			let streamer = self.clients[STREAMER].streamer().unwrap().viewers();
			let reported = streamer.first().and_then(|v| v.layer) == Some(u16::from(layer));
			let video = self.video;
			// Frames of the new layer, after the switch.
			if self.layer == Some(layer) && reported {
				while self.video < video + 5 {
					self.step().await;
				}
				if self.layer == Some(layer) {
					return;
				}
			}
			assert!(Instant::now() < deadline, "video stays on layer {:?}", self.layer);
			self.step().await;
		}
	}

	/// Start a stream that accepts everyone, watch it until connected; its id.
	async fn watching(&mut self) -> String {
		let options = StreamerOptions { auto_accept: true, ..Default::default() };
		self.clients[STREAMER].start(options).unwrap();
		self.until("stream in the viewer's list", |i, e| {
			i == VIEWER && matches!(e, StreamEvent::Streams(l) if l.len() == 1)
		})
		.await;
		let id = self.clients[VIEWER].directory().by_streamer(IDS[STREAMER]).unwrap().id.clone();
		self.clients[VIEWER].watch(&id, "").unwrap();
		self.until("viewer connected", |i, e| {
			i == VIEWER && watch_event(e, |e| matches!(e, WatchEvent::Connected))
		})
		.await;
		id
	}

	/// The signals `client` sent that `f` accepts.
	fn sent(&self, client: usize, f: impl Fn(&Signal) -> bool) -> usize {
		self.signals.iter().filter(|(i, s)| *i == client && f(s)).count()
	}

	/// The first viewer of our stream, as the streamer sees it.
	fn viewer(&self) -> voelin_stream::ViewerInfo {
		self.clients[STREAMER].streamer().unwrap().viewers().remove(0)
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

/// A Voelin streamer with layers lists them in its offer; the viewer picks
/// one, and "Auto" hands the choice back to its bandwidth estimate.
#[tokio::test(flavor = "multi_thread")]
async fn viewer_picks_a_layer() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let mut net = Net::new();
	// Without `min_bitrate`s every estimate fits the top layer.
	let layers = vec![
		LayerSpec { id: 0, ..LayerSpec::single(1_500_000) },
		LayerSpec { id: 1, scale: 0.5, max_fps: Some(15), ..LayerSpec::single(400_000) },
	];
	let options =
		StreamerOptions { auto_accept: true, layers: layers.clone(), ..Default::default() };
	net.clients[STREAMER].start(options).unwrap();
	net.until("stream in the viewer's list", |i, e| {
		i == VIEWER && matches!(e, StreamEvent::Streams(l) if l.len() == 1)
	})
	.await;
	let id = net.clients[VIEWER].directory().by_streamer(IDS[STREAMER]).unwrap().id.clone();
	net.clients[VIEWER].watch(&id, "").unwrap();
	net.until("the offer's layers", |i, e| {
		i == VIEWER
			&& watch_event(e, |e| {
				matches!(e, WatchEvent::Layers(l)
					if l.iter().map(|l| (l.id, l.scale, l.max_fps)).eq([(0, 1.0, None), (1, 0.5, Some(15))]))
			})
	})
	.await;
	net.until("viewer connected", |i, e| {
		i == VIEWER && watch_event(e, |e| matches!(e, WatchEvent::Connected))
	})
	.await;
	net.source = Some(SyntheticSource::with_layers(30, 0, true, &layers));
	net.on_layer(0).await;

	net.clients[VIEWER].set_watch_layer(&id, Some(1)).unwrap();
	assert_eq!(net.clients[VIEWER].watching().next().unwrap().layer(), Some(1));
	net.on_layer(1).await;
	// The estimate does not move it back while it is picked.
	net.frames(20).await;
	assert_eq!(net.layer, Some(1));

	net.clients[VIEWER].set_watch_layer(&id, None).unwrap();
	net.on_layer(0).await;
	let asked: Vec<_> = net
		.signals
		.iter()
		.filter_map(|(i, s)| match s {
			Signal::Layer { layer } => Some((*i, *layer)),
			_ => None,
		})
		.collect();
	assert_eq!(asked, [(VIEWER, Some(1)), (VIEWER, None)]);
	assert_eq!(
		net.clients[VIEWER].set_watch_layer(&id, Some(7)),
		Err(SessionError::UnknownLayer(id.clone(), 7))
	);
}

/// A streamer whose offer lists no layers (one layer here; an official
/// client always) is never asked for one.
#[tokio::test(flavor = "multi_thread")]
async fn no_layer_request_without_the_offer_listing_layers() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let mut net = Net::new();
	let options = StreamerOptions { auto_accept: true, ..Default::default() };
	net.clients[STREAMER].start(options).unwrap();
	net.until("stream in the viewer's list", |i, e| {
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
	assert!(
		!net.events.iter().any(|(_, e)| watch_event(e, |e| matches!(e, WatchEvent::Layers(_)))),
		"{:?}",
		net.events
	);
	let viewer = &mut net.clients[VIEWER];
	assert!(viewer.watching().next().unwrap().layers().is_empty());
	assert_eq!(viewer.set_watch_layer(&id, Some(0)), Err(SessionError::NoLayers(id.clone())));
	assert_eq!(viewer.set_watch_layer(&id, None), Err(SessionError::NoLayers(id.clone())));
	assert!(viewer.poll_output().is_none(), "nothing sent");
	net.flush().await;
	assert!(!net.signals.iter().any(|(_, s)| matches!(s, Signal::Layer { .. })));
}

const AEAD_FIRST: [SrtpProfile; 3] =
	[SrtpProfile::AeadAes128Gcm, SrtpProfile::AeadAes256Gcm, SrtpProfile::Aes128CmSha1_80];
/// SRTP that fails after the handshake with these profiles: AES-GCM.
const AEAD: Rule = Rule::FailSrtp(&[7, 8]);
const STALL: Duration = Duration::from_secs(1);

fn config(srtp: &[SrtpProfile], stall_timeout: Duration) -> PeerConfig {
	PeerConfig { srtp_profiles: srtp.to_vec(), stall_timeout, ..PeerConfig::loopback() }
}

/// The viewer's connection carries nothing (its SRTP fails after the
/// handshake, with AES-GCM here): the viewer asks for a new one without the
/// AEAD profiles and gets the stream over AES_CM_128_HMAC_SHA1_80; the new
/// connection does not fall back again.
#[tokio::test(flavor = "multi_thread")]
async fn viewer_falls_back_when_srtp_fails() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	// The answering viewer is the DTLS server: its order picks AES-GCM. The
	// streamer does not step down by itself.
	let streamer = config(&SrtpProfile::DEFAULT_ORDER, Duration::MAX);
	let mut net = Net::with(streamer, config(&AEAD_FIRST, STALL), Some(AEAD));
	net.source = Some(SyntheticSource::new(30, 4000, true));
	net.watching().await;
	net.frames(10).await;
	assert_eq!(net.relays.iter().map(Relay::profile).collect::<Vec<_>>(), [Some(7), Some(1)]);
	assert_eq!(net.sent(VIEWER, |s| *s == Signal::Reconnect), 1);
	assert_eq!(net.viewer().srtp_profile, Some(SrtpProfile::Aes128CmSha1_80));
	// Longer than the stall timeout: the working connection stays.
	net.frames(60).await;
	assert_eq!(net.sent(VIEWER, |s| *s == Signal::Reconnect), 1);
	assert_eq!(net.relays.len(), 2);
}

/// A viewer that gets nothing and does not step down by itself (as the
/// official client may not): its receiver reports never mention our video,
/// so the streamer offers a new connection (`reconnectOffer`) without the
/// profile that failed, and the viewer gets the stream.
#[tokio::test(flavor = "multi_thread")]
async fn streamer_falls_back_when_its_viewer_gets_nothing() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let streamer = config(&AEAD_FIRST, STALL);
	let mut net = Net::with(streamer, config(&AEAD_FIRST, Duration::MAX), Some(AEAD));
	net.source = Some(SyntheticSource::new(30, 4000, true));
	net.watching().await;
	net.frames(10).await;
	assert_eq!(net.relays.iter().map(Relay::profile).collect::<Vec<_>>(), [Some(7), Some(1)]);
	assert_eq!(net.sent(VIEWER, |s| *s == Signal::Reconnect), 0);
	assert_eq!(net.sent(STREAMER, |s| matches!(s, Signal::Offer { reconnect: true, .. })), 1);
	assert_eq!(net.viewer().srtp_profile, Some(SrtpProfile::Aes128CmSha1_80));
	net.frames(90).await;
	assert_eq!(net.relays.len(), 2, "the working connection stays");
}

/// Audio comes, video does not (the streamer has no encoder for the codec
/// our answer took): the viewer asks for a new connection without that
/// codec and gets video in the next one.
#[tokio::test(flavor = "multi_thread")]
async fn viewer_falls_back_to_another_codec() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let streamer = PeerConfig {
		video_codecs: vec![VideoCodec::Vp9, VideoCodec::Vp8],
		..config(&SrtpProfile::DEFAULT_ORDER, Duration::MAX)
	};
	let mut net = Net::with(streamer, config(&SrtpProfile::DEFAULT_ORDER, STALL), None);
	// The source's video is VP8 only.
	net.video_format = Some(VideoFormat::Vp8);
	net.source = Some(SyntheticSource::new(30, 4000, true));
	net.watching().await;
	net.frames(10).await;
	assert_eq!(net.sent(VIEWER, |s| *s == Signal::Reconnect), 1);
	assert_eq!(net.viewer().codec, Some(VideoCodec::Vp8));
}

/// Video arrives in the codec our answer took, but our decoders make
/// nothing of it (the app's pipeline says so: `video_undecodable`): the
/// viewer asks for a new connection without that codec and gets the next
/// one the streamer offered. With no other codec left it stays.
#[tokio::test(flavor = "multi_thread")]
async fn undecodable_video_falls_back_to_another_codec() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let streamer = PeerConfig {
		video_codecs: vec![VideoCodec::Vp9, VideoCodec::Vp8],
		..config(&SrtpProfile::DEFAULT_ORDER, Duration::MAX)
	};
	let mut net = Net::with(streamer, config(&SrtpProfile::DEFAULT_ORDER, Duration::MAX), None);
	net.source = Some(SyntheticSource::new(30, 4000, true));
	let id = net.watching().await;
	net.frames(10).await;
	assert_eq!(net.viewer().codec, Some(VideoCodec::Vp9));
	net.clients[VIEWER].video_undecodable(&id).unwrap();
	net.until("connected again", |i, e| {
		i == VIEWER && watch_event(e, |e| matches!(e, WatchEvent::Connected))
	})
	.await;
	net.frames(10).await;
	assert_eq!(net.sent(VIEWER, |s| *s == Signal::Reconnect), 1);
	assert_eq!(net.viewer().codec, Some(VideoCodec::Vp8));
	// VP8 does not decode either: nothing is left, the viewer stays.
	net.clients[VIEWER].video_undecodable(&id).unwrap();
	net.frames(10).await;
	assert_eq!(net.sent(VIEWER, |s| *s == Signal::Reconnect), 1);
	assert_eq!(
		net.clients[VIEWER].video_undecodable("nope"),
		Err(SessionError::NotWatching("nope".into()))
	);
}

/// Nothing left to fall back to: SRTP fails with every profile both sides
/// have. The viewer says so and stays; nothing loops.
#[tokio::test(flavor = "multi_thread")]
async fn no_fallback_left() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let only_aead = [SrtpProfile::AeadAes128Gcm];
	let streamer = config(&only_aead, Duration::MAX);
	let mut net = Net::with(streamer, config(&only_aead, STALL), Some(AEAD));
	net.source = Some(SyntheticSource::new(30, 4000, true));
	net.watching().await;
	let end = Instant::now() + STALL * 3;
	while Instant::now() < end {
		net.step().await;
	}
	assert_eq!(net.sent(VIEWER, |s| *s == Signal::Reconnect), 0);
	assert_eq!((net.video, net.relays.len()), (0, 1));
}
