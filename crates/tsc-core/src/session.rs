//! One server session: merges its sources and routes commands.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tracing::debug;
use tsc_model::{ChatMessage, ChatTarget, Presence};

use crate::audio::{self, AudioHandle, AudioIn};
use crate::gateway::{self, GatewayCmd, GatewayEvent};
use crate::query::{self, QueryCmd, QueryEvent};
use crate::route::{ChatRoute, Dedup, route_chat};
use crate::voice::{self, VoiceCmd, VoiceEvent};
use crate::{Command, Event, ObserveState, SessionId, SessionState, Source, VoiceState};

pub(crate) struct SessionHandle {
	tx: mpsc::UnboundedSender<Command>,
}

impl SessionHandle {
	pub fn spawn(id: SessionId, events: broadcast::Sender<Event>) -> Self {
		let (tx, rx) = mpsc::unbounded_channel();
		tokio::spawn(Session::new(id, events).run(rx));
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
	AudioError(String),
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
	open_chats: HashSet<ChatTarget>,
	dedup: Dedup,
	sources_tx: mpsc::UnboundedSender<SourceEvent>,
	sources_rx: Option<mpsc::UnboundedReceiver<SourceEvent>>,
}

impl Session {
	fn new(id: SessionId, events: broadcast::Sender<Event>) -> Self {
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
				let generation = self.next_generation();
				self.nickname = options.nickname.clone();
				let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
				let (ev_tx, ev_rx) = mpsc::unbounded_channel();
				if options.audio {
					let (out_tx, out_rx) = mpsc::unbounded_channel();
					let (err_tx, err_rx) = mpsc::unbounded_channel();
					self.audio = Some(audio::spawn(out_tx, err_tx, false));
					self.forward(out_rx, move |p| SourceEvent::AudioOut(generation, p));
					self.forward(err_rx, SourceEvent::AudioError);
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
			Command::CloseSession { .. } => {}
		}
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
			SourceEvent::AudioError(message) => self.error(message),
			_ => debug!("event from a replaced source ignored"),
		}
	}

	fn voice_event(&mut self, e: VoiceEvent) {
		match e {
			VoiceEvent::Connected { name, flavor } => {
				self.state.voice = VoiceState::Connected;
				self.emit_state();
				let capabilities = flavor.capabilities();
				self.emit(Event::ServerInfo { session: self.id, name, flavor, capabilities });
				self.reopen_relayed_chats();
			}
			VoiceEvent::Presence(p) => {
				self.voice_presence = Some(p);
				self.publish_presence();
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
