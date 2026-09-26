//! TeamSpeak 6 streams of a session: a task that runs the stream sessions
//! ([`tsc_stream::Streams`]) of the voice connection.
//!
//! The voice task forwards stream notifications here and sends the resulting
//! commands. Only started for TeamSpeak 6 servers.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{broadcast, mpsc};
use tracing::debug;
pub use tsc_stream::{
	Codec, EncodedFrame, EndReason, FrameSource, Frequency, LeaveReason, MediaFrame, MediaKind,
	MediaTime, PeerConfig, StreamInfo, StreamKind, StreamSetup, SyntheticSource, VideoCodec,
	ViewerInfo, ViewerState,
};
use tsc_stream::{
	Output, Request, StreamEvent, StreamNotification, StreamerEvent, StreamerOptions, Streams,
	WatchEvent,
};
use tsclientlib::ClientId;

use crate::audio::{AudioHandle, AudioIn};
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

/// Hands encoded frames of our stream to the engine, e.g. from an encoder thread.
#[derive(Clone)]
pub struct StreamSink {
	tx: mpsc::UnboundedSender<StreamInput>,
	live: Arc<AtomicBool>,
	keyframe: Arc<AtomicBool>,
}

impl StreamSink {
	/// Send one frame; `false` once the stream has ended.
	pub fn send(&self, frame: EncodedFrame) -> bool {
		self.live.load(Ordering::Relaxed) && self.tx.send(StreamInput::Frame(frame)).is_ok()
	}

	/// Whether a viewer asked for a keyframe since the last call.
	pub fn take_keyframe_request(&self) -> bool {
		self.keyframe.swap(false, Ordering::Relaxed)
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
	Frame(EncodedFrame),
	Notification(StreamNotification),
	RequestFailed(Request, String),
	/// Clients on the server with their `client_is_streaming`.
	Clients(BTreeMap<u16, Option<bool>>),
	/// The voice connection is gone.
	Shutdown(String),
}

pub(crate) struct StreamHandle {
	tx: mpsc::UnboundedSender<StreamInput>,
}

impl StreamHandle {
	pub fn spawn(
		session: SessionId,
		own_client: u16,
		config: PeerConfig,
		voice: mpsc::UnboundedSender<VoiceCmd>,
		events: broadcast::Sender<Event>,
		frames: broadcast::Sender<StreamFrame>,
		audio: Option<AudioHandle>,
	) -> Self {
		let (tx, rx) = mpsc::unbounded_channel();
		let task = StreamTask {
			session,
			streams: Streams::new(ClientId(own_client), config),
			voice,
			events,
			frames,
			audio,
			tx: tx.clone(),
			sink: None,
			clients: BTreeMap::new(),
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
	/// Clients on the server and their `client_is_streaming`.
	clients: BTreeMap<u16, Option<bool>>,
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
				self.streams.start(StreamerOptions { setup, auto_accept }).map(|()| {
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
				// Streams of clients that left or stopped streaming are gone.
				// Only changes count: a snapshot may predate the streaming flag
				// of a stream that was just announced.
				for (client, streaming) in &clients {
					let was = self.clients.get(client).copied().flatten();
					if let (Some(was), Some(now)) = (was, *streaming)
						&& was != now
					{
						self.streams.set_client_streaming(ClientId(*client), now);
					}
				}
				self.streams.retain_streamers(|c| clients.contains_key(&c.0));
				self.clients = clients;
				Ok(())
			}
			StreamInput::Shutdown(_) => Ok(()),
		};
		if let Err(e) = result {
			self.emit(Event::Error { session, message: format!("stream: {e}") });
		}
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
				let sink = StreamSink {
					tx: self.tx.clone(),
					live: Arc::new(AtomicBool::new(true)),
					keyframe: Arc::new(AtomicBool::new(true)),
				};
				self.sink = Some(sink.clone());
				self.emit(Event::StreamState { session, state: StreamState::Live { id, sink } });
			}
			StreamEvent::Streamer(StreamerEvent::Request { viewer, message }) => {
				self.emit(Event::StreamViewerRequest { session, viewer: viewer.0, message });
			}
			StreamEvent::Streamer(StreamerEvent::Viewers(viewers)) => {
				self.emit(Event::StreamViewers { session, viewers });
			}
			StreamEvent::Streamer(StreamerEvent::KeyframeRequest) => {
				if let Some(sink) = &self.sink {
					sink.keyframe.store(true, Ordering::Relaxed);
				}
				self.emit(Event::StreamKeyframeRequest { session });
			}
			StreamEvent::Streamer(StreamerEvent::Ended(reason)) => {
				if let Some(sink) = self.sink.take() {
					sink.live.store(false, Ordering::Relaxed);
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
	use tsc_stream::{FrameSource, SyntheticSource};

	use super::*;

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

	/// The whole path: test pattern → VP8 → stream task → str0m peers on
	/// loopback → the viewer's frame bus → decoder. The pictures show the
	/// moving rectangle.
	#[cfg(feature = "media")]
	#[tokio::test(flavor = "multi_thread")]
	async fn test_pattern_through_stream_tasks() {
		use std::sync::mpsc as std_mpsc;

		use tsc_media::capture::SourceId;
		use tsc_media::{Codec, Codecs};

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
