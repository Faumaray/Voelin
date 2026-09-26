//! One server session: merges its sources and routes commands.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tracing::debug;
use tsc_audio::AudioSettings;
use tsc_model::{ChatMessage, ChatTarget, Presence};
use tsclientlib::ClientId;

use crate::audio::{self, AudioEvent, AudioHandle, AudioIn};
use crate::gateway::{self, GatewayCmd, GatewayEvent};
use crate::query::{self, QueryCmd, QueryEvent};
use crate::route::{ChatRoute, Dedup, route_chat};
use crate::stream::{PeerConfig, StreamFrame, StreamHandle, StreamInput};
use crate::voice::{self, VoiceCmd, VoiceEvent};
use crate::{Command, Event, ObserveState, SessionId, SessionState, Source, VoiceState};

pub(crate) struct SessionHandle {
	tx: mpsc::UnboundedSender<Command>,
}

impl SessionHandle {
	pub fn spawn(
		id: SessionId,
		events: broadcast::Sender<Event>,
		frames: broadcast::Sender<StreamFrame>,
		audio_settings: AudioSettings,
	) -> Self {
		let (tx, rx) = mpsc::unbounded_channel();
		tokio::spawn(Session::new(id, events, frames, audio_settings).run(rx));
		Self { tx }
	}

	pub fn send(&self, command: Command) {
		let _ = self.tx.send(command);
	}
}

/// Events from sources, tagged with the generation of the source so events
/// of a replaced source are ignored.
enum SourceEvent {
	Voice(u64, VoiceEvent),
	Gateway(u64, GatewayEvent),
	Query(u64, QueryEvent),
	AudioOut(u64, tsproto_packets::packets::OutPacket),
	Audio(u64, AudioEvent),
}

struct Session {
	id: SessionId,
	events: broadcast::Sender<Event>,
	state: SessionState,
	generation: u64,
	voice: Option<(u64, mpsc::UnboundedSender<VoiceCmd>)>,
	voice_presence: Option<Presence>,
	nickname: String,
	gateway: Option<(u64, mpsc::UnboundedSender<GatewayCmd>)>,
	gateway_presence: Option<Presence>,
	query: Option<(u64, mpsc::UnboundedSender<QueryCmd>)>,
	query_presence: Option<Presence>,
	audio: Option<AudioHandle>,
	audio_settings: AudioSettings,
	/// Streams of the voice connection (TeamSpeak 6 only).
	streams: Option<StreamHandle>,
	stream_peer: PeerConfig,
	frames: broadcast::Sender<StreamFrame>,
	/// `client_is_streaming` of the voice presence, as last told to the streams.
	streaming_clients: BTreeMap<u16, Option<bool>>,
	open_chats: HashSet<ChatTarget>,
	dedup: Dedup,
	sources_tx: mpsc::UnboundedSender<SourceEvent>,
	sources_rx: Option<mpsc::UnboundedReceiver<SourceEvent>>,
}

impl Session {
	fn new(
		id: SessionId,
		events: broadcast::Sender<Event>,
		frames: broadcast::Sender<StreamFrame>,
		audio_settings: AudioSettings,
	) -> Self {
		let (sources_tx, sources_rx) = mpsc::unbounded_channel();
		Self {
			id,
			events,
			state: SessionState::default(),
			generation: 0,
			voice: None,
			voice_presence: None,
			nickname: String::new(),
			gateway: None,
			gateway_presence: None,
			query: None,
			query_presence: None,
			audio: None,
			audio_settings,
			streams: None,
			stream_peer: PeerConfig::default(),
			frames,
			streaming_clients: BTreeMap::new(),
			open_chats: HashSet::new(),
			dedup: Dedup::default(),
			sources_tx,
			sources_rx: Some(sources_rx),
		}
	}

	fn emit(&self, event: Event) {
		let _ = self.events.send(event);
	}

	fn error(&self, message: impl Into<String>) {
		self.emit(Event::Error { session: self.id, message: message.into() });
	}

	fn emit_state(&self) {
		self.emit(Event::State { session: self.id, state: self.state.clone() });
	}

	async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
		let mut sources = self.sources_rx.take().expect("receiver");
		loop {
			tokio::select! {
				cmd = commands.recv() => match cmd {
					None | Some(Command::CloseSession { .. }) => break,
					Some(cmd) => self.command(cmd),
				},
				Some(event) = sources.recv() => self.source_event(event),
			}
		}
		self.stop_all();
	}

	fn next_generation(&mut self) -> u64 {
		self.generation += 1;
		self.generation
	}

	/// Spawn a task that forwards a source's events, tagged.
	fn forward<E: Send + 'static>(
		&self,
		mut rx: mpsc::UnboundedReceiver<E>,
		wrap: impl Fn(E) -> SourceEvent + Send + 'static,
	) {
		let tx = self.sources_tx.clone();
		tokio::spawn(async move {
			while let Some(e) = rx.recv().await {
				if tx.send(wrap(e)).is_err() {
					break;
				}
			}
		});
	}

	fn command(&mut self, cmd: Command) {
		match cmd {
			Command::ConnectVoice { options, .. } => {
				if let Some((_, tx)) = self.voice.take() {
					let _ = tx.send(VoiceCmd::Disconnect);
				}
				self.stop_streams("voice reconnecting");
				let generation = self.next_generation();
				self.nickname = options.nickname.clone();
				self.stream_peer = options.stream_peer.clone();
				let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
				let (ev_tx, ev_rx) = mpsc::unbounded_channel();
				self.audio = None;
				if options.audio {
					let (out_tx, out_rx) = mpsc::unbounded_channel();
					let (audio_tx, audio_rx) = mpsc::unbounded_channel();
					self.audio = Some(audio::spawn(out_tx, audio_tx, self.audio_settings.clone()));
					self.forward(out_rx, move |p| SourceEvent::AudioOut(generation, p));
					self.forward(audio_rx, move |e| SourceEvent::Audio(generation, e));
				}
				tokio::spawn(voice::run(*options, cmd_rx, ev_tx));
				self.forward(ev_rx, move |e| SourceEvent::Voice(generation, e));
				self.voice = Some((generation, cmd_tx));
				self.state.voice = VoiceState::Connecting;
				self.emit_state();
			}
			Command::DisconnectVoice { .. } => {
				if let Some((_, tx)) = &self.voice {
					let _ = tx.send(VoiceCmd::Disconnect);
				}
			}
			Command::ObserveGateway { url, identity, .. } => {
				self.stop_observing();
				let generation = self.next_generation();
				let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
				let (ev_tx, ev_rx) = mpsc::unbounded_channel();
				tokio::spawn(gateway::run(url, *identity, cmd_rx, ev_tx));
				self.forward(ev_rx, move |e| SourceEvent::Gateway(generation, e));
				self.gateway = Some((generation, cmd_tx));
				self.state.observe = ObserveState::Connecting;
				self.emit_state();
				self.reopen_relayed_chats();
			}
			Command::ObserveQuery { connect, .. } => {
				self.stop_observing();
				let generation = self.next_generation();
				let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
				let (ev_tx, ev_rx) = mpsc::unbounded_channel();
				tokio::spawn(query::run(*connect, cmd_rx, ev_tx));
				self.forward(ev_rx, move |e| SourceEvent::Query(generation, e));
				self.query = Some((generation, cmd_tx));
				self.state.observe = ObserveState::Connecting;
				self.emit_state();
				self.reopen_relayed_chats();
			}
			Command::StopObserving { .. } => {
				self.stop_observing();
				self.publish_presence();
			}
			Command::OpenChat { target, .. } => {
				self.open_chats.insert(target.clone());
				self.open_relay(&target);
			}
			Command::CloseChat { target, .. } => {
				self.open_chats.remove(&target);
				if let Some((_, tx)) = &self.gateway {
					let _ = tx.send(GatewayCmd::CloseChat(target.clone()));
				}
				if let Some((_, tx)) = &self.query {
					let _ = tx.send(QueryCmd::CloseChat(target));
				}
			}
			Command::SendChat { target, text, .. } => match self.route(&target) {
				ChatRoute::Voice => self.voice_cmd(VoiceCmd::SendChat(target, text)),
				ChatRoute::Gateway => {
					if let Some((_, tx)) = &self.gateway {
						let _ = tx.send(GatewayCmd::SendChat(target, text));
					}
				}
				ChatRoute::Query => {
					if let Some((_, tx)) = &self.query {
						let nick = if self.nickname.is_empty() {
							"user".into()
						} else {
							self.nickname.clone()
						};
						let _ = tx.send(QueryCmd::SendChat { target, nick, text });
					}
				}
				ChatRoute::Unavailable(reason) => self.error(reason),
			},
			Command::MoveToChannel { channel, password, .. } => {
				self.voice_cmd(VoiceCmd::Move(channel, password));
			}
			Command::SetInputMuted { muted, .. } => {
				self.state.input_muted = muted;
				self.voice_cmd(VoiceCmd::SetInputMuted(muted));
				if let Some(a) = &self.audio {
					a.send(AudioIn::InputMuted(muted));
				}
				self.emit_state();
			}
			Command::SetOutputMuted { muted, .. } => {
				self.state.output_muted = muted;
				self.voice_cmd(VoiceCmd::SetOutputMuted(muted));
				if let Some(a) = &self.audio {
					a.send(AudioIn::OutputMuted(muted));
				}
				self.emit_state();
			}
			Command::SetTransmitting { on, .. } => {
				self.state.transmitting = on;
				if let Some(a) = &self.audio {
					a.send(AudioIn::Transmit(on));
				}
				self.emit_state();
			}
			Command::SetAudioSettings(settings) => {
				self.audio_settings = (*settings).clone();
				if let Some(a) = &self.audio {
					a.send(AudioIn::Settings(settings));
				}
			}
			// Without audio there is nothing to adjust; the UI sends volumes
			// again after connecting.
			Command::SetClientVolume { client, volume, .. } => {
				if let Some(a) = &self.audio {
					a.send(AudioIn::ClientVolume { client: ClientId(client), volume });
				}
			}
			Command::SetClientMuted { client, muted, .. } => {
				if let Some(a) = &self.audio {
					a.send(AudioIn::ClientMuted { client: ClientId(client), muted });
				}
			}
			Command::SetStreamVolume { stream_id, volume, .. } => {
				if let Some(a) = &self.audio {
					a.send(AudioIn::StreamVolume { stream: stream_id, volume });
				}
			}
			Command::StartStream { setup, auto_accept, .. } => {
				self.stream_input(StreamInput::Start { setup, auto_accept });
			}
			Command::StopStream { .. } => self.stream_input(StreamInput::Stop),
			Command::AcceptViewer { viewer, accept, .. } => {
				self.stream_input(StreamInput::Respond { viewer, accept });
			}
			Command::KickViewer { viewer, .. } => self.stream_input(StreamInput::Kick { viewer }),
			Command::SendStreamFrame { frame, .. } => {
				// Frames without a stream are dropped silently.
				if let Some(s) = &self.streams {
					s.send(StreamInput::Frame(frame));
				}
			}
			Command::WatchStream { stream_id, .. } => {
				self.stream_input(StreamInput::Watch { stream_id });
			}
			Command::LeaveStream { stream_id, .. } => {
				self.stream_input(StreamInput::Leave { stream_id });
			}
			Command::RequestStreamKeyframe { stream_id, .. } => {
				self.stream_input(StreamInput::RequestKeyframe { stream_id });
			}
			Command::CloseSession { .. } | Command::TestMicrophone { .. } => {}
		}
	}

	fn stream_input(&self, input: StreamInput) {
		match &self.streams {
			Some(s) => s.send(input),
			None if self.state.voice == VoiceState::Connected => {
				self.error("streams need a TeamSpeak 6 server");
			}
			None => self.error("not connected with voice"),
		}
	}

	fn stop_streams(&mut self, reason: &str) {
		if let Some(s) = self.streams.take() {
			s.send(StreamInput::Shutdown(reason.into()));
		}
		self.streaming_clients.clear();
	}

	fn voice_cmd(&self, cmd: VoiceCmd) {
		match &self.voice {
			Some((_, tx)) => {
				let _ = tx.send(cmd);
			}
			None => self.error("not connected with voice"),
		}
	}

	fn route(&self, target: &ChatTarget) -> ChatRoute {
		let voice_channel =
			if self.state.voice == VoiceState::Connected { self.state.own_channel } else { None };
		route_chat(target, voice_channel, self.gateway.is_some(), self.query.is_some())
	}

	/// Chats that are not covered by the voice connection need a relay.
	fn open_relay(&self, target: &ChatTarget) {
		match self.route(target) {
			ChatRoute::Gateway => {
				if let Some((_, tx)) = &self.gateway {
					let _ = tx.send(GatewayCmd::OpenChat(target.clone()));
				}
			}
			ChatRoute::Query => {
				if let Some((_, tx)) = &self.query {
					let _ = tx.send(QueryCmd::OpenChat(target.clone()));
				}
			}
			_ => {}
		}
	}

	fn reopen_relayed_chats(&self) {
		for target in &self.open_chats {
			self.open_relay(target);
		}
	}

	fn stop_observing(&mut self) {
		if let Some((_, tx)) = self.gateway.take() {
			let _ = tx.send(GatewayCmd::Stop);
		}
		if let Some((_, tx)) = self.query.take() {
			let _ = tx.send(QueryCmd::Stop);
		}
		self.gateway_presence = None;
		self.query_presence = None;
		self.state.observe = ObserveState::Off;
		self.emit_state();
	}

	fn stop_all(&mut self) {
		self.stop_streams("session closed");
		if let Some((_, tx)) = self.voice.take() {
			let _ = tx.send(VoiceCmd::Disconnect);
		}
		self.audio = None;
		self.stop_observing();
	}

	fn is_current(&self, source: Source, generation: u64) -> bool {
		let current = match source {
			Source::Voice => self.voice.as_ref().map(|(g, _)| *g),
			Source::Gateway => self.gateway.as_ref().map(|(g, _)| *g),
			Source::Query => self.query.as_ref().map(|(g, _)| *g),
		};
		current == Some(generation)
	}

	fn source_event(&mut self, event: SourceEvent) {
		match event {
			SourceEvent::Voice(g, e) if self.is_current(Source::Voice, g) => self.voice_event(e),
			SourceEvent::Gateway(g, e) if self.is_current(Source::Gateway, g) => {
				self.gateway_event(e)
			}
			SourceEvent::Query(g, e) if self.is_current(Source::Query, g) => self.query_event(e),
			SourceEvent::AudioOut(g, packet) if self.is_current(Source::Voice, g) => {
				if let Some((_, tx)) = &self.voice {
					let _ = tx.send(VoiceCmd::Audio(packet));
				}
			}
			SourceEvent::Audio(g, event) if self.is_current(Source::Voice, g) => match event {
				AudioEvent::Error(message) => self.error(message),
				AudioEvent::Level { db, sending } => {
					self.emit(Event::InputLevel { session: Some(self.id), level_db: db, sending })
				}
			},
			_ => debug!("event from a replaced source ignored"),
		}
	}

	fn voice_event(&mut self, e: VoiceEvent) {
		match e {
			VoiceEvent::Connected { name, flavor, own_client } => {
				self.state.voice = VoiceState::Connected;
				self.emit_state();
				let capabilities = flavor.capabilities();
				if capabilities.streams
					&& let Some((_, voice)) = &self.voice
				{
					self.streams = Some(StreamHandle::spawn(
						self.id,
						own_client,
						self.stream_peer.clone(),
						voice.clone(),
						self.events.clone(),
						self.frames.clone(),
						self.audio.clone(),
					));
				}
				self.emit(Event::ServerInfo { session: self.id, name, flavor, capabilities });
				self.reopen_relayed_chats();
			}
			VoiceEvent::Presence(p) => {
				// Client ids are reused: forget the volumes of those who left.
				if let (Some(a), Some(old)) = (&self.audio, &self.voice_presence) {
					for id in old.clients.keys().filter(|id| !p.clients.contains_key(id)) {
						a.send(AudioIn::ClientLeft(ClientId(*id)));
					}
				}
				if let Some(s) = &self.streams {
					let clients: BTreeMap<_, _> =
						p.clients.values().map(|c| (c.id, c.streaming)).collect();
					if clients != self.streaming_clients {
						self.streaming_clients = clients.clone();
						s.send(StreamInput::Clients(clients));
					}
				}
				self.voice_presence = Some(p);
				self.publish_presence();
			}
			VoiceEvent::Stream(n) => {
				if let Some(s) = &self.streams {
					s.send(StreamInput::Notification(n));
				}
			}
			VoiceEvent::StreamRequestFailed(request, error) => {
				if let Some(s) = &self.streams {
					s.send(StreamInput::RequestFailed(request, error));
				}
			}
			VoiceEvent::OwnChannel(cid) => {
				if self.state.own_channel != Some(cid) {
					self.state.own_channel = Some(cid);
					self.emit_state();
				}
			}
			VoiceEvent::Chat(msg) => self.chat(msg),
			VoiceEvent::Audio(packet) => {
				if let Some(a) = &self.audio {
					a.send(AudioIn::Packet(packet));
				}
			}
			VoiceEvent::Talking { client, talking } => {
				self.emit(Event::Talking { session: self.id, client, talking });
			}
			VoiceEvent::Disconnected(reason) => {
				self.stop_streams("voice disconnected");
				self.voice = None;
				self.voice_presence = None;
				self.audio = None;
				self.state.voice = VoiceState::Disconnected;
				self.state.own_channel = None;
				self.emit_state();
				if let Some(reason) = reason {
					self.error(format!("voice: {reason}"));
				}
				self.publish_presence();
			}
		}
	}

	fn gateway_event(&mut self, e: GatewayEvent) {
		match e {
			GatewayEvent::Connected => {
				self.state.observe = ObserveState::Observing;
				self.emit_state();
			}
			GatewayEvent::Presence(p) => {
				self.gateway_presence = Some(p);
				self.publish_presence();
			}
			GatewayEvent::Chat(msg) => self.chat(msg),
			GatewayEvent::Error(message) => self.error(format!("gateway: {message}")),
			GatewayEvent::Disconnected(reason) => {
				self.gateway = None;
				self.gateway_presence = None;
				self.state.observe = ObserveState::Off;
				self.emit_state();
				if let Some(reason) = reason {
					self.error(format!("gateway: {reason}"));
				}
				self.publish_presence();
			}
		}
	}

	fn query_event(&mut self, e: QueryEvent) {
		match e {
			QueryEvent::Presence(p) => {
				if self.state.observe != ObserveState::Observing {
					self.state.observe = ObserveState::Observing;
					self.emit_state();
				}
				self.query_presence = Some(p);
				self.publish_presence();
			}
			QueryEvent::Chat(msg) => self.chat(msg),
			QueryEvent::Error(message) => self.error(format!("query: {message}")),
			QueryEvent::Disconnected => {
				self.query = None;
				self.query_presence = None;
				self.state.observe = ObserveState::Off;
				self.emit_state();
				self.publish_presence();
			}
		}
	}

	fn chat(&mut self, msg: ChatMessage) {
		let wanted = match &msg.target {
			ChatTarget::Server | ChatTarget::Private(_) => true,
			target => self.open_chats.contains(target) || !msg.via_relay,
		};
		if wanted && !self.dedup.is_duplicate(&msg) {
			self.emit(Event::Chat { session: self.id, message: msg });
		}
	}

	/// Publish the presence of the most authoritative source.
	fn publish_presence(&mut self) {
		let (source, presence) = if let Some(p) = &self.voice_presence {
			(Some(Source::Voice), p.clone())
		} else if let Some(p) = &self.gateway_presence {
			(Some(Source::Gateway), p.clone())
		} else if let Some(p) = &self.query_presence {
			(Some(Source::Query), p.clone())
		} else {
			(None, Presence::default())
		};
		if self.state.presence_source != source {
			self.state.presence_source = source;
			self.emit_state();
		}
		self.emit(Event::Presence { session: self.id, presence: Arc::new(presence) });
	}
}
