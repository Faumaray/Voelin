//! TeamSpeak 6 streams of a session: a task that runs the stream sessions
//! ([`voelin_stream::Streams`]) of the voice connection.
//!
//! The voice task forwards stream notifications here and sends the resulting
//! commands. Only started for TeamSpeak 6 servers.
//!
//! Streams that started before we joined are looked up on the server
//! (`requeststreaminfo`) and in the session's gateway directory: its
//! registered entries (stream id and streamer) are handed to the stream
//! sessions ([`voelin_stream::Streams::discovered`]) for streamers in our
//! channel, and a [`StreamLookup`] answers for the gateway when the server
//! cannot. Our own stream's life (live, viewers, ended) goes back to the
//! session ([`OwnStreamEvent`]), which keeps its directory entry.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{broadcast, mpsc};
use tracing::debug;
use tsclientlib::ClientId;
use voelin_stream::{
	ClientState, LayerFeedback, Output, Request, StreamEvent, StreamLookup, StreamNotification,
	StreamerEvent, StreamerOptions, Streams, WatchEvent,
};
pub use voelin_stream::{
	Codec, EncodedFrame, EndReason, FrameSource, Frequency, LayerId, LayerSet, LayerSpec,
	LeaveReason, MediaFrame, MediaKind, MediaTime, PeerConfig, SrtpProfile, StreamInfo, StreamKind,
	StreamSetup, SyntheticSource, VideoCodec, ViewerInfo, ViewerState,
};

use crate::audio::{AudioHandle, AudioIn};
use crate::settings::{STREAM_PERMISSIONS, SharedSettings, StreamPermissions};
use crate::voice::VoiceCmd;
use crate::{Event, SessionId};

/// Our own stream.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamState {
	/// `setupstream` sent.
	Starting,
	/// Live: frames given to `sink` (or [`crate::Command::SendStreamFrame`])
	/// reach every connected viewer.
	Live {
		id: String,
		sink: StreamSink,
	},
	Ended(EndReason),
}

/// A stream we watch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchState {
	/// Join request sent; waiting for the streamer.
	Requested,
	/// Accepted; connecting to the streamer.
	Connecting,
	/// Frames arrive through [`crate::Engine::subscribe_frames`].
	Connected,
	Ended(EndReason),
}

/// A received frame of a watched stream.
#[derive(Clone, Debug)]
pub struct StreamFrame {
	pub session: SessionId,
	pub stream_id: String,
	pub frame: MediaFrame,
}

/// Hands encoded frames of our stream to the engine, e.g. from an encoder
/// thread, and tells the encoders what the viewers need per simulcast layer:
/// keyframes and bitrates. Those are read lock-free from the stream session.
#[derive(Clone)]
pub struct StreamSink {
	tx: mpsc::UnboundedSender<StreamInput>,
	live: Arc<AtomicBool>,
	feedback: Arc<LayerFeedback>,
}

impl StreamSink {
	/// Send one frame; `false` once the stream has ended.
	pub fn send(&self, frame: EncodedFrame) -> bool {
		self.live.load(Ordering::Relaxed) && self.tx.send(StreamInput::Frame(frame)).is_ok()
	}

	/// Whether a viewer asked for a keyframe of any layer since the last
	/// call (or [`take_layer_keyframes`](Self::take_layer_keyframes)).
	pub fn take_keyframe_request(&self) -> bool {
		self.feedback.take_any_keyframe()
	}

	/// Adds to `layers` the simulcast layers viewers asked a keyframe for
	/// since the last call. Every layer is asked for when the stream goes live.
	pub fn take_layer_keyframes(&self, layers: &mut LayerSet) {
		self.feedback.take_keyframes(layers);
	}

	/// Bitrate (bit/s) the bandwidth estimates of `layer`'s viewers allow:
	/// the lowest estimate of its viewers, at least
	/// [`voelin_stream::session::MIN_VIDEO_BITRATE`], at most the layer's
	/// `max_bitrate`. `None` while no viewer of the layer has an estimate.
	pub fn layer_bitrate(&self, layer: LayerId) -> Option<u64> {
		self.feedback.bitrate(layer)
	}

	pub fn is_live(&self) -> bool {
		self.live.load(Ordering::Relaxed)
	}
}

impl fmt::Debug for StreamSink {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("StreamSink").field("live", &self.is_live()).finish()
	}
}

impl PartialEq for StreamSink {
	fn eq(&self, other: &Self) -> bool {
		Arc::ptr_eq(&self.live, &other.live)
	}
}

pub(crate) enum StreamInput {
	Start {
		setup: StreamSetup,
		auto_accept: bool,
	},
	Stop,
	Respond {
		viewer: u16,
		accept: bool,
	},
	Kick {
		viewer: u16,
	},
	Watch {
		stream_id: String,
	},
	Leave {
		stream_id: String,
	},
	RequestKeyframe {
		stream_id: String,
	},
	/// Simulcast layers of our stream (now and for streams started later).
	Layers(Vec<LayerSpec>),
	/// SRTP profile order of new connections.
	SrtpProfiles(Vec<SrtpProfile>),
	Frame(EncodedFrame),
	Notification(StreamNotification),
	RequestFailed(Request, String),
	/// Clients on the server: channel and `client_is_streaming`.
	Clients(BTreeMap<u16, ClientState>),
	/// The registered streams of the gateway's directory (full list).
	Directory(Vec<StreamInfo>),
	/// The voice connection is gone.
	Shutdown(String),
}

/// Our own stream, for the session (the gateway directory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OwnStreamEvent {
	Live {
		id: String,
		title: String,
		kind: StreamKind,
	},
	/// Connected viewers.
	Viewers(u32),
	Ended {
		id: String,
	},
}

/// The kind of a stream as the gateway directory names it.
pub(crate) fn kind_name(kind: StreamKind) -> String {
	match kind {
		StreamKind::Camera => "camera".into(),
		StreamKind::Screen => "screen".into(),
		StreamKind::Window => "window".into(),
		StreamKind::Other(v) => format!("other:{v}"),
	}
}

/// [`kind_name`] back; unknown names are screens.
pub(crate) fn parse_kind(name: &str) -> StreamKind {
	match name {
		"camera" => StreamKind::Camera,
		"window" => StreamKind::Window,
		other => match other.strip_prefix("other:").and_then(|v| v.parse().ok()) {
			Some(v) => StreamKind::from_u8(v),
			None => StreamKind::Screen,
		},
	}
}

/// The gateway directory as the stream task sees it.
#[derive(Default)]
struct GatewayStreams {
	entries: Vec<StreamInfo>,
	/// Streamers the [`GatewayLookup`] was asked about.
	wanted: Vec<ClientId>,
}

/// Looks unannounced streams up in the gateway's directory (after the
/// server, see [`voelin_stream::discovery`]).
struct GatewayLookup(Arc<Mutex<GatewayStreams>>);

impl StreamLookup for GatewayLookup {
	fn lookup(&mut self, streamer: ClientId, _out: &mut voelin_stream::session::Outbox) -> bool {
		let mut dir = self.0.lock().unwrap_or_else(PoisonError::into_inner);
		let known = dir.entries.iter().any(|e| e.streamer == streamer);
		if known {
			dir.wanted.push(streamer);
		}
		known
	}
}

pub(crate) struct StreamHandle {
	tx: mpsc::UnboundedSender<StreamInput>,
}

impl StreamHandle {
	#[allow(clippy::too_many_arguments)] // the parts of the session the task uses
	pub fn spawn(
		session: SessionId,
		own_client: u16,
		config: PeerConfig,
		voice: mpsc::UnboundedSender<VoiceCmd>,
		events: broadcast::Sender<Event>,
		frames: broadcast::Sender<StreamFrame>,
		audio: Option<AudioHandle>,
		settings: SharedSettings,
		own: mpsc::UnboundedSender<OwnStreamEvent>,
	) -> Self {
		let (tx, rx) = mpsc::unbounded_channel();
		let mut streams = Streams::new(ClientId(own_client), config);
		let gateway = Arc::new(Mutex::new(GatewayStreams::default()));
		streams.add_lookup(Box::new(GatewayLookup(gateway.clone())));
		let task = StreamTask {
			session,
			streams,
			voice,
			events,
			frames,
			audio,
			tx: tx.clone(),
			sink: None,
			layers: Vec::new(),
			settings,
			own,
			setup: None,
			live: None,
			viewers: 0,
			clients: BTreeMap::new(),
			gateway,
		};
		tokio::spawn(task.run(rx));
		Self { tx }
	}

	pub fn send(&self, input: StreamInput) {
		let _ = self.tx.send(input);
	}
}

struct StreamTask {
	session: SessionId,
	streams: Streams,
	voice: mpsc::UnboundedSender<VoiceCmd>,
	events: broadcast::Sender<Event>,
	frames: broadcast::Sender<StreamFrame>,
	/// Where the audio of watched streams plays, if the session has audio.
	audio: Option<AudioHandle>,
	/// For the sinks handed out.
	tx: mpsc::UnboundedSender<StreamInput>,
	/// The sink of our live stream.
	sink: Option<StreamSink>,
	/// Simulcast layers of our stream (empty: one layer).
	layers: Vec<LayerSpec>,
	/// `stream.permissions` decides join requests.
	settings: SharedSettings,
	/// Our stream's life, for the session.
	own: mpsc::UnboundedSender<OwnStreamEvent>,
	/// The setup of our stream since `Start`.
	setup: Option<StreamSetup>,
	/// Our live stream's id.
	live: Option<String>,
	/// Connected viewers of our stream, as last told.
	viewers: u32,
	/// The clients as of the last update.
	clients: BTreeMap<u16, ClientState>,
	gateway: Arc<Mutex<GatewayStreams>>,
}

impl StreamTask {
	async fn run(mut self, mut rx: mpsc::UnboundedReceiver<StreamInput>) {
		loop {
			tokio::select! {
				input = rx.recv() => match input {
					None => break,
					Some(StreamInput::Shutdown(reason)) => {
						self.shutdown(reason);
						break;
					}
					Some(input) => self.input(input).await,
				},
				() = self.streams.wait_peers() => {}
			}
			self.flush();
		}
		debug!(session = self.session, "stream task ended");
	}

	fn emit(&self, event: Event) {
		let _ = self.events.send(event);
	}

	async fn input(&mut self, input: StreamInput) {
		let session = self.session;
		let result = match input {
			StreamInput::Start { setup, auto_accept } => {
				self.setup = Some(setup.clone());
				let layers = self.layers.clone();
				let options = StreamerOptions { setup, auto_accept, layers, ..Default::default() };
				self.streams.start(options).map(|()| {
					self.emit(Event::StreamState { session, state: StreamState::Starting });
				})
			}
			StreamInput::Stop => self.streams.stop(),
			StreamInput::Respond { viewer, accept } => {
				self.streams.respond(ClientId(viewer), accept).await
			}
			StreamInput::Kick { viewer } => self.streams.kick(ClientId(viewer)),
			StreamInput::Watch { stream_id } => self.streams.watch(&stream_id, "").map(|()| {
				self.emit(Event::WatchState { session, stream_id, state: WatchState::Requested });
			}),
			StreamInput::Leave { stream_id } => self.streams.leave(&stream_id),
			StreamInput::RequestKeyframe { stream_id } => {
				self.streams.request_keyframe(&stream_id);
				Ok(())
			}
			StreamInput::Layers(layers) => {
				self.layers = layers.clone();
				// Without a stream they are used for the next one.
				if self.streams.streamer().is_some() {
					self.streams.set_layers(layers)
				} else {
					Ok(())
				}
			}
			StreamInput::SrtpProfiles(profiles) => {
				self.streams.set_srtp_profiles(profiles);
				Ok(())
			}
			StreamInput::Frame(frame) => {
				self.streams.write_frame(&frame);
				Ok(())
			}
			StreamInput::Notification(n) => {
				self.streams.handle_notification(n).await;
				Ok(())
			}
			StreamInput::RequestFailed(request, error) => {
				self.streams.request_failed(&request, &error);
				Ok(())
			}
			StreamInput::Clients(clients) => {
				// Streams of clients that left, stopped streaming or are not
				// in our channel are gone; unannounced ones are looked up.
				self.clients = clients.clone();
				self.streams.update_clients(clients);
				self.add_directory_streams();
				Ok(())
			}
			StreamInput::Directory(entries) => {
				self.gateway.lock().unwrap_or_else(PoisonError::into_inner).entries = entries;
				self.add_directory_streams();
				Ok(())
			}
			StreamInput::Shutdown(_) => Ok(()),
		};
		if let Err(e) = result {
			self.emit(Event::Error { session, message: format!("stream: {e}") });
		}
	}

	/// Streams of the gateway directory we were not told about: streamers in
	/// our channel that still stream (the sessions check the channel).
	fn add_directory_streams(&mut self) {
		let entries = {
			let mut dir = self.gateway.lock().unwrap_or_else(PoisonError::into_inner);
			dir.wanted.clear();
			dir.entries.clone()
		};
		let own = self.streams.own_client();
		for info in entries {
			let stopped =
				self.clients.get(&info.streamer.0).is_some_and(|c| c.streaming == Some(false));
			if info.streamer != own && !stopped && self.streams.directory().get(&info.id).is_none()
			{
				debug!(stream = %info.id, streamer = info.streamer.0, "stream from the gateway directory");
				self.streams.discovered(info);
			}
		}
	}

	fn own_stream(&self, event: OwnStreamEvent) {
		let _ = self.own.send(event);
	}

	/// Send the requests on the connection and report the events.
	fn flush(&mut self) {
		while let Some(output) = self.streams.poll_output() {
			match output {
				Output::Request(request) => {
					let _ = self.voice.send(VoiceCmd::Stream(request));
				}
				Output::Event(event) => self.event(event),
			}
		}
	}

	fn event(&mut self, event: StreamEvent) {
		let session = self.session;
		match event {
			StreamEvent::Streams(streams) => self.emit(Event::StreamsChanged { session, streams }),
			StreamEvent::Streamer(StreamerEvent::Live { id }) => {
				let feedback = match self.streams.streamer() {
					Some(s) => s.layer_feedback().clone(),
					None => Arc::new(LayerFeedback::new()),
				};
				let sink = StreamSink {
					tx: self.tx.clone(),
					live: Arc::new(AtomicBool::new(true)),
					feedback,
				};
				self.sink = Some(sink.clone());
				let setup = self.setup.clone().unwrap_or_default();
				self.live = Some(id.clone());
				self.viewers = 0;
				self.own_stream(OwnStreamEvent::Live {
					id: id.clone(),
					title: setup.name,
					kind: setup.kind,
				});
				self.emit(Event::StreamState { session, state: StreamState::Live { id, sink } });
			}
			StreamEvent::Streamer(StreamerEvent::Request { viewer, message }) => {
				match self.join_decision(viewer) {
					// Answered through the input queue, as the user would.
					Some(accept) => {
						debug!(
							viewer = viewer.0,
							accept, "join request answered by stream.permissions"
						);
						let _ = self.tx.send(StreamInput::Respond { viewer: viewer.0, accept });
					}
					None => {
						self.emit(Event::StreamViewerRequest {
							session,
							viewer: viewer.0,
							message,
						});
					}
				}
			}
			StreamEvent::Streamer(StreamerEvent::Viewers(viewers)) => {
				let connected =
					viewers.iter().filter(|v| v.state == ViewerState::Connected).count() as u32;
				if connected != self.viewers && self.live.is_some() {
					self.viewers = connected;
					self.own_stream(OwnStreamEvent::Viewers(connected));
				}
				self.emit(Event::StreamViewers { session, viewers });
			}
			// The sink already has it (layer feedback).
			StreamEvent::Streamer(StreamerEvent::KeyframeRequest { layer }) => {
				self.emit(Event::StreamKeyframeRequest { session, layer });
			}
			StreamEvent::Streamer(StreamerEvent::LayerBitrate { layer, bitrate }) => {
				self.emit(Event::StreamLayerBitrate { session, layer, bitrate });
			}
			StreamEvent::Streamer(StreamerEvent::Ended(reason)) => {
				if let Some(sink) = self.sink.take() {
					sink.live.store(false, Ordering::Relaxed);
				}
				if let Some(id) = self.live.take() {
					self.own_stream(OwnStreamEvent::Ended { id });
				}
				self.emit(Event::StreamState { session, state: StreamState::Ended(reason) });
			}
			StreamEvent::Watch { id, event } => {
				let state = match event {
					WatchEvent::Accepted => WatchState::Connecting,
					WatchEvent::Connected => WatchState::Connected,
					WatchEvent::Ended(reason) => {
						if let Some(audio) = &self.audio {
							audio.send(AudioIn::StreamEnded(id.clone()));
						}
						WatchState::Ended(reason)
					}
					WatchEvent::Frame(frame) => {
						if let (MediaKind::Audio, Some(audio)) = (frame.kind, &self.audio) {
							audio.send(AudioIn::StreamAudio {
								stream: id.clone(),
								time: frame.time.rebase(Frequency::FORTY_EIGHT_KHZ).numer(),
								data: frame.data.clone(),
							});
						}
						let _ = self.frames.send(StreamFrame { session, stream_id: id, frame });
						return;
					}
				};
				self.emit(Event::WatchState { session, stream_id: id, state });
			}
		}
	}

	/// The answer `stream.permissions` gives to a join request; `None`: ask.
	fn join_decision(&self, viewer: ClientId) -> Option<bool> {
		match *self.settings.current().get_arc(&STREAM_PERMISSIONS) {
			StreamPermissions::Everyone => Some(true),
			StreamPermissions::Nobody => Some(false),
			// No contacts yet: ask for everyone.
			StreamPermissions::Friends => None,
			StreamPermissions::Channel => {
				let discovery = self.streams.discovery();
				let own = discovery.own_channel();
				Some(own.is_some() && discovery.channel_of(viewer) == own)
			}
		}
	}

	/// The connection is gone: report our stream and watched streams as ended.
	fn shutdown(&mut self, reason: String) {
		let session = self.session;
		let ended = || EndReason::Failed(reason.clone());
		if self.streams.streamer().is_some() {
			self.emit(Event::StreamState { session, state: StreamState::Ended(ended()) });
		}
		if let Some(sink) = self.sink.take() {
			sink.live.store(false, Ordering::Relaxed);
		}
		if let Some(id) = self.live.take() {
			self.own_stream(OwnStreamEvent::Ended { id });
		}
		for watched in self.streams.watching() {
			let stream_id = watched.id().to_owned();
			if let Some(audio) = &self.audio {
				audio.send(AudioIn::StreamEnded(stream_id.clone()));
			}
			self.emit(Event::WatchState { session, stream_id, state: WatchState::Ended(ended()) });
		}
		if self.streams.directory().iter().next().is_some() {
			self.emit(Event::StreamsChanged { session, streams: Vec::new() });
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, Instant};

	use tokio::time::timeout;
	use voelin_stream::{FrameSource, SyntheticSource};

	use super::*;
	use crate::settings::Settings;

	const CLIENTS: [u16; 2] = [5, 6];

	/// The notifications the server sends for `request` from `from`, per receiver.
	fn relay(from: u16, request: Request) -> Vec<(u16, StreamNotification)> {
		let both = |n: StreamNotification| CLIENTS.map(|c| (c, n.clone())).to_vec();
		match request {
			Request::Setup(setup) => CLIENTS
				.map(|c| {
					let info = StreamInfo {
						id: "s-1".into(),
						streamer: ClientId(from),
						name: setup.name.clone(),
						kind: setup.kind,
						bitrate: setup.bitrate,
						viewer_limit: setup.viewer_limit,
						audio: setup.audio,
					};
					let return_code = (c == from).then(|| "1".to_owned());
					(c, StreamNotification::Started { info, return_code })
				})
				.to_vec(),
			Request::Stop { id } => {
				both(StreamNotification::Stopped { id, streamer: None, reason: None })
			}
			Request::Join { id, streamer, message } => vec![(
				streamer.0,
				StreamNotification::JoinRequest {
					id,
					viewer: ClientId(from),
					message,
					remove: false,
				},
			)],
			Request::Leave { id, streamer } => vec![(
				streamer.0,
				StreamNotification::JoinRequest {
					id,
					viewer: ClientId(from),
					message: String::new(),
					remove: true,
				},
			)],
			Request::Respond { id, viewer, offer, accept } => vec![(
				viewer.0,
				StreamNotification::JoinResponse {
					id,
					streamer: Some(ClientId(from)),
					accepted: accept,
					offer,
					message: String::new(),
				},
			)],
			Request::Signal { id, peer, signal } => vec![(
				peer.0,
				StreamNotification::Signaling { id, peer: ClientId(from), json: signal.to_json() },
			)],
			Request::RemoveViewer { id, viewer, reason } => {
				both(StreamNotification::ViewerLeft { id, viewer, reason: Some(reason) })
			}
			Request::StreamInfo { .. } => Vec::new(),
		}
	}

	async fn wait<T>(
		rx: &mut broadcast::Receiver<Event>,
		mut f: impl FnMut(Event) -> Option<T>,
	) -> T {
		timeout(Duration::from_secs(10), async {
			loop {
				if let Some(t) = f(rx.recv().await.unwrap()) {
					return t;
				}
			}
		})
		.await
		.expect("timed out")
	}

	type Handles = Arc<Vec<StreamHandle>>;

	/// Two stream tasks (sessions 1 and 2) whose commands go through a fake
	/// server; `audio` is the viewer's audio thread.
	fn pair(
		audio: Option<AudioHandle>,
	) -> (Handles, broadcast::Receiver<Event>, broadcast::Receiver<StreamFrame>) {
		pair_with(audio, &Settings::in_memory())
	}

	/// [`pair`] with `settings` for both tasks.
	fn pair_with(
		audio: Option<AudioHandle>,
		settings: &Settings,
	) -> (Handles, broadcast::Receiver<Event>, broadcast::Receiver<StreamFrame>) {
		let (events, rx) = broadcast::channel(4096);
		let (frames, frames_rx) = broadcast::channel(4096);
		let mut voices = Vec::new();
		let mut handles = Vec::new();
		let mut audio = audio;
		for (i, clid) in CLIENTS.into_iter().enumerate() {
			let (voice, voice_rx) = mpsc::unbounded_channel();
			let config = PeerConfig::loopback();
			let session = i as u64 + 1;
			handles.push(StreamHandle::spawn(
				session,
				clid,
				config,
				voice,
				events.clone(),
				frames.clone(),
				if session == 2 { audio.take() } else { None },
				SharedSettings::new(settings.clone()),
				mpsc::unbounded_channel().0,
			));
			voices.push(voice_rx);
		}
		let handles = Arc::new(handles);
		// The server: relays requests from one task as notifications to the others.
		for (i, mut voice_rx) in voices.into_iter().enumerate() {
			let handles = handles.clone();
			tokio::spawn(async move {
				while let Some(cmd) = voice_rx.recv().await {
					if let VoiceCmd::Stream(request) = cmd {
						for (to, n) in relay(CLIENTS[i], request) {
							let index = CLIENTS.iter().position(|c| *c == to).unwrap();
							handles[index].send(StreamInput::Notification(n));
						}
					}
				}
			});
		}
		(handles, rx, frames_rx)
	}

	/// Session 1 streams (auto-accepting), session 2 watches; returns the sink
	/// once the viewer is connected.
	async fn live_and_watching(
		handles: &Handles,
		rx: &mut broadcast::Receiver<Event>,
	) -> StreamSink {
		let setup = StreamSetup { name: "t".into(), ..Default::default() };
		handles[0].send(StreamInput::Start { setup, auto_accept: true });
		// Live for the streamer and listed for the viewer, in either order.
		let (mut sink, mut listed) = (None, false);
		wait(rx, |e| {
			match e {
				Event::StreamState { session: 1, state: StreamState::Live { sink: s, .. } } => {
					sink = Some(s);
				}
				Event::StreamsChanged { session: 2, streams } => listed = streams.len() == 1,
				_ => {}
			}
			(sink.is_some() && listed).then_some(())
		})
		.await;
		handles[1].send(StreamInput::Watch { stream_id: "s-1".into() });
		wait(rx, |e| match e {
			Event::WatchState { session: 2, state: WatchState::Connected, .. } => Some(()),
			_ => None,
		})
		.await;
		sink.unwrap()
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn stream_tasks_through_fake_server() {
		// The viewer's audio thread.
		let (audio, audio_rx) = AudioHandle::channel();
		let (handles, mut rx, mut frames_rx) = pair(Some(audio));
		let sink = live_and_watching(&handles, &mut rx).await;

		// Frames from the sink reach the viewer's frame bus.
		let feeder = tokio::spawn({
			let sink = sink.clone();
			async move {
				let mut source = SyntheticSource::new(30, 2000, true);
				let mut out = Vec::new();
				while sink.is_live() {
					source.poll_frames(Instant::now(), &mut out);
					for f in out.drain(..) {
						sink.send(f);
					}
					tokio::time::sleep(Duration::from_millis(10)).await;
				}
			}
		});
		let (mut video, mut audio) = (0, 0);
		timeout(Duration::from_secs(10), async {
			while video < 10 || audio < 10 {
				let f = frames_rx.recv().await.unwrap();
				assert_eq!((f.session, f.stream_id.as_str()), (2, "s-1"));
				match f.frame.kind {
					MediaKind::Video => video += 1,
					MediaKind::Audio => audio += 1,
				}
			}
		})
		.await
		.expect("no frames");
		assert!(sink.take_keyframe_request());

		// Stopping ends the sink and tells the viewer.
		handles[0].send(StreamInput::Stop);
		wait(&mut rx, |e| match e {
			Event::WatchState {
				session: 2, state: WatchState::Ended(EndReason::Stopped), ..
			} => Some(()),
			_ => None,
		})
		.await;
		assert!(!sink.is_live());
		feeder.await.unwrap();

		// Stream audio went to the viewer's audio thread, in 20 ms steps,
		// and the ended stream was forgotten there.
		let received: Vec<AudioIn> = audio_rx.try_iter().collect();
		let times: Vec<u64> = received
			.iter()
			.filter_map(|m| match m {
				AudioIn::StreamAudio { stream, time, .. } if stream == "s-1" => Some(*time),
				_ => None,
			})
			.collect();
		assert!(times.len() >= 10, "{} audio frames", times.len());
		assert!(times.windows(2).all(|w| w[1] > w[0] && (w[1] - w[0]) % 960 == 0), "{times:?}");
		assert!(matches!(received.last(), Some(AudioIn::StreamEnded(id)) if id == "s-1"));

		// Errors surface as events.
		handles[1].send(StreamInput::Leave { stream_id: "s-1".into() });
		wait(&mut rx, |e| match e {
			Event::Error { session: 2, message } if message.contains("not watching") => Some(()),
			_ => None,
		})
		.await;
		handles[1].send(StreamInput::Shutdown("bye".into()));
	}

	/// Simulcast layers and the SRTP setting through the stream tasks: the
	/// viewer gets the top layer only, the sink carries keyframe requests and
	/// the bitrate target per layer.
	#[tokio::test(flavor = "multi_thread")]
	async fn layers_through_stream_tasks() {
		let (handles, mut rx, mut frames_rx) = pair(None);
		let layers = vec![
			LayerSpec { id: 0, min_bitrate: 800_000, ..LayerSpec::single(2_000_000) },
			LayerSpec { id: 1, scale: 0.5, ..LayerSpec::single(500_000) },
		];
		// Set before the stream starts: kept for it.
		handles[0].send(StreamInput::Layers(layers.clone()));
		// The viewer is the DTLS server: its order decides.
		handles[1].send(StreamInput::SrtpProfiles(vec![SrtpProfile::AeadAes128Gcm]));
		let sink = live_and_watching(&handles, &mut rx).await;
		let mut keyframes = LayerSet::new();
		sink.take_layer_keyframes(&mut keyframes);
		assert!(keyframes.contains(0), "{keyframes:?}");

		let feeder = tokio::spawn({
			let sink = sink.clone();
			async move {
				let mut source = SyntheticSource::with_layers(30, 0, true, &layers);
				let mut out = Vec::new();
				while sink.is_live() {
					source.poll_frames(Instant::now(), &mut out);
					for f in out.drain(..) {
						sink.send(f);
					}
					tokio::time::sleep(Duration::from_millis(10)).await;
				}
			}
		});
		let mut video = 0;
		timeout(Duration::from_secs(10), async {
			while video < 20 {
				let f = frames_rx.recv().await.unwrap();
				if f.frame.kind == MediaKind::Video {
					assert_eq!(SyntheticSource::frame_layer(&f.frame.data), Some(0));
					video += 1;
				}
			}
		})
		.await
		.expect("no frames");
		// Bandwidth estimates reach the viewer list and the layer's target.
		let (mut info, mut bitrate) = (None, None);
		wait(&mut rx, |e| {
			match e {
				Event::StreamViewers { session: 1, viewers } if viewers[0].estimate.is_some() => {
					info = Some(viewers[0].clone());
				}
				Event::StreamLayerBitrate { session: 1, layer: 0, bitrate: b } => bitrate = Some(b),
				_ => {}
			}
			(info.is_some() && bitrate.is_some()).then_some(())
		})
		.await;
		let info = info.unwrap();
		assert_eq!(info.layer, Some(0));
		assert_eq!(info.srtp_profile, Some(SrtpProfile::AeadAes128Gcm));
		assert!(sink.layer_bitrate(0).is_some_and(|b| b > 0));
		assert_eq!(sink.layer_bitrate(1), None, "no viewer on layer 1");

		handles[0].send(StreamInput::Stop);
		wait(&mut rx, |e| match e {
			Event::WatchState { session: 2, state: WatchState::Ended(_), .. } => Some(()),
			_ => None,
		})
		.await;
		feeder.await.unwrap();
	}

	/// `stream.permissions` answers join requests unless it is `friends`.
	#[tokio::test(flavor = "multi_thread")]
	async fn permissions_answer_join_requests() {
		let in_channels = |streamer: u64, viewer: u64| {
			StreamInput::Clients(BTreeMap::from([
				(CLIENTS[0], ClientState { channel: streamer, streaming: Some(false) }),
				(CLIENTS[1], ClientState { channel: viewer, streaming: Some(false) }),
			]))
		};
		for (permissions, viewer_channel, expected) in [
			(StreamPermissions::Everyone, 2, Some(true)),
			(StreamPermissions::Channel, 1, Some(true)),
			(StreamPermissions::Channel, 2, Some(false)),
			(StreamPermissions::Nobody, 1, Some(false)),
			(StreamPermissions::Friends, 1, None),
		] {
			let settings = Settings::in_memory();
			settings.set(&STREAM_PERMISSIONS, permissions).unwrap();
			let (handles, mut rx, _frames) = pair_with(None, &settings);
			for h in handles.iter() {
				h.send(in_channels(1, viewer_channel));
			}
			let setup = StreamSetup { name: "p".into(), ..Default::default() };
			handles[0].send(StreamInput::Start { setup, auto_accept: false });
			let (mut live, mut listed) = (false, false);
			wait(&mut rx, |e| {
				match e {
					Event::StreamState { session: 1, state: StreamState::Live { .. } } => {
						live = true
					}
					Event::StreamsChanged { session: 2, streams } => listed = !streams.is_empty(),
					_ => {}
				}
				(live && listed).then_some(())
			})
			.await;
			handles[1].send(StreamInput::Watch { stream_id: "s-1".into() });
			let answer = wait(&mut rx, |e| match e {
				Event::WatchState { session: 2, state: WatchState::Connected, .. } => {
					Some(Some(true))
				}
				Event::WatchState {
					session: 2,
					state: WatchState::Ended(EndReason::Denied),
					..
				} => Some(Some(false)),
				Event::StreamViewerRequest { session: 1, .. } => Some(None),
				_ => None,
			})
			.await;
			assert_eq!(answer, expected, "{permissions:?}, viewer in channel {viewer_channel}");
			for h in handles.iter() {
				h.send(StreamInput::Shutdown("done".into()));
			}
		}
	}

	/// The whole path: test pattern → VP8 → stream task → str0m peers on
	/// loopback → the viewer's frame bus → decoder. The pictures show the
	/// moving rectangle.
	#[cfg(feature = "media-desktop")]
	#[tokio::test(flavor = "multi_thread")]
	async fn test_pattern_through_stream_tasks() {
		use std::sync::mpsc as std_mpsc;

		use voelin_media::capture::SourceId;
		use voelin_media::{Codec, Codecs};

		use crate::media::{Streamer, StreamerConfig, VideoPipeline, rectangle_span};

		let (handles, mut rx, mut frames_rx) = pair(None);
		let sink = live_and_watching(&handles, &mut rx).await;
		let codecs = Arc::new(Codecs::new());
		let config = StreamerConfig {
			source: SourceId::Synthetic,
			synthetic_size: (320, 240),
			bitrate_kbps: 1500,
			..StreamerConfig::default()
		};
		let streamer = Streamer::start(&codecs, config).await.unwrap();
		streamer.attach(Arc::new(sink.clone()));

		let (tx, pictures) = std_mpsc::channel();
		let pipeline = VideoPipeline::new(
			codecs,
			move |picture| {
				let _ = tx.send(picture);
			},
			{
				let handles = handles.clone();
				move || handles[1].send(StreamInput::RequestKeyframe { stream_id: "s-1".into() })
			},
		);
		let input = pipeline.input();
		let forward = tokio::spawn(async move {
			while let Ok(f) = frames_rx.recv().await {
				input.push(f.frame);
			}
		});
		let spans = tokio::task::spawn_blocking(move || {
			let mut spans = Vec::new();
			while spans.len() < 15 {
				let picture = pictures.recv_timeout(Duration::from_secs(10)).expect("no picture");
				assert_eq!((picture.width, picture.height), (320, 240));
				spans.push(rectangle_span(&picture));
			}
			spans
		})
		.await
		.unwrap();
		for (a, b) in &spans {
			assert!((60..=68).contains(&(b - a + 1)), "{spans:?}");
		}
		assert_ne!(spans.first(), spans.last(), "the rectangle moves");
		let stats = pipeline.stats();
		assert_eq!(stats.codec, Some(Codec::Vp8));
		assert!(stats.error.is_none(), "{stats:?}");
		let sent = streamer.stats();
		assert!(sent.video_frames >= 15 && sent.audio_frames > 0, "{sent:?}");

		handles[0].send(StreamInput::Stop);
		wait(&mut rx, |e| match e {
			Event::WatchState { session: 2, state: WatchState::Ended(_), .. } => Some(()),
			_ => None,
		})
		.await;
		forward.abort();
		drop(streamer);
	}
}
