//! One server session: merges its sources and routes commands.
//!
//! Chat messages from every source are stored under the server's unique id
//! ([`crate::history`]); the id comes from the voice connection or the
//! gateway, or, before they connect, from what the database remembers for
//! the address (`voice:<address>`, `gateway:<url>`). The gateway's features
//! are driven through its client ([`crate::gateway`]).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tracing::debug;
use tsclientlib::ClientId;
use voelin_audio::AudioSettings;
use voelin_gateway_proto::{HistoryEntry, StreamEntry, StreamSpec, feature};
use voelin_model::{ChatMessage, ChatTarget, Presence};
use voelin_stream::ClientState;

use crate::audio::{self, AudioEvent, AudioHandle, AudioIn};
use crate::gateway::{
	self, ClientError, GatewayClient, GatewayCmd, GatewayEvent, GatewayRequest, GatewayUpdate,
	Push, ReactionPush,
};
use crate::history::{self, ChatCtx, HistoryMessage, MessageSource, SharedHistory};
use crate::query::{self, QueryCmd, QueryEvent};
use crate::route::{ChatRoute, Dedup, route_chat};
use crate::settings::SharedSettings;
use crate::stream::{
	LayerSpec, OwnStreamEvent, PeerConfig, SrtpProfile, StreamFrame, StreamHandle, StreamInfo,
	StreamInput, StreamKind, kind_name, parse_kind,
};
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
		settings: SharedSettings,
		history: SharedHistory,
	) -> Self {
		let (tx, rx) = mpsc::unbounded_channel();
		tokio::spawn(Session::new(id, events, frames, audio_settings, settings, history).run(rx));
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
	/// Our own stream (from the stream task of that voice connection).
	OwnStream(u64, OwnStreamEvent),
	/// The gateway's stream directory after subscribing.
	Directory(u64, Result<Vec<StreamEntry>, ClientError>),
	/// The server's unique id the database remembers for an address.
	KnownServer(String),
}

/// Our live stream, for the gateway's directory.
struct OwnStream {
	id: String,
	title: String,
	kind: StreamKind,
	/// Registered in the directory of the connected gateway.
	registered: bool,
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
	/// The logged-in gateway, for requests.
	gateway_client: Option<GatewayClient>,
	query: Option<(u64, mpsc::UnboundedSender<QueryCmd>)>,
	query_presence: Option<Presence>,
	audio: Option<AudioHandle>,
	audio_settings: AudioSettings,
	settings: SharedSettings,
	history: SharedHistory,
	/// Our unique id on the server, for the messages we send.
	own_uid: Option<String>,
	/// Other names of this server (`voice:<address>`, `gateway:<url>`),
	/// remembered with its unique id once a source tells it.
	aliases: Vec<String>,
	/// Streams of the voice connection (TeamSpeak 6 only).
	streams: Option<StreamHandle>,
	stream_peer: PeerConfig,
	/// Simulcast layers of our stream, for the stream task.
	stream_layers: Vec<LayerSpec>,
	/// The SRTP profile setting (`Command::SetSrtpProfiles`), over the voice
	/// options' `stream_peer`.
	srtp_profiles: Option<Vec<SrtpProfile>>,
	frames: broadcast::Sender<StreamFrame>,
	/// Channel and `client_is_streaming` of the voice presence, as last told
	/// to the streams.
	streaming_clients: BTreeMap<u16, ClientState>,
	/// The gateway's stream directory, by id.
	directory: BTreeMap<String, StreamEntry>,
	own_stream: Option<OwnStream>,
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
		settings: SharedSettings,
		history: SharedHistory,
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
			gateway_client: None,
			query: None,
			query_presence: None,
			audio: None,
			audio_settings,
			settings,
			history,
			own_uid: None,
			aliases: Vec::new(),
			streams: None,
			stream_peer: PeerConfig::default(),
			stream_layers: Vec::new(),
			srtp_profiles: None,
			frames,
			streaming_clients: BTreeMap::new(),
			directory: BTreeMap::new(),
			own_stream: None,
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

	fn gateway_update(&self, update: GatewayUpdate) {
		self.emit(Event::Gateway { session: self.id, update });
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
				if let Some(profiles) = &self.srtp_profiles {
					self.stream_peer.srtp_profiles = profiles.clone();
				}
				self.known_server(format!("voice:{}", options.address));
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
				self.known_server(format!("gateway:{url}"));
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
				// Queries do not tell the server's unique id: without another
				// source, the history is kept under the query address.
				if self.state.server_uid.is_none() {
					let port = connect.server_port.map(|p| format!("/{p}")).unwrap_or_default();
					self.learn_server_uid(format!("query:{}{port}", connect.addr), Source::Query);
					self.open_histories();
				}
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
				self.open_history(&target);
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
			Command::SendChat { target, text, .. } => {
				let route = self.route(&target);
				if !matches!(route, ChatRoute::Unavailable(_)) {
					self.store_own(&target, &text, &route);
				}
				match route {
					ChatRoute::Voice => self.voice_cmd(VoiceCmd::SendChat(target, text)),
					ChatRoute::Gateway => {
						if let Some((_, tx)) = &self.gateway {
							let _ = tx.send(GatewayCmd::SendChat(target, text));
						}
					}
					ChatRoute::Query => {
						if let Some((_, tx)) = &self.query {
							let nick = self.relay_nick();
							let _ = tx.send(QueryCmd::SendChat { target, nick, text });
						}
					}
					ChatRoute::Unavailable(reason) => self.error(reason),
				}
			}
			Command::LoadOlderHistory { target, before, .. } => match self.chat_ctx() {
				Some(ctx) => {
					let gateway = self.gateway_client.clone();
					tokio::spawn(history::load_older(ctx, target, before, gateway));
				}
				// Nothing is known about the server yet: nothing stored.
				None => self.emit(Event::ChatHistory {
					session: self.id,
					target,
					messages: Vec::new(),
					source: history::HistorySource::Local,
					complete: true,
				}),
			},
			Command::Gateway { request, .. } => self.gateway_request(request),
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
			// Kept for streams started later, also without a connection.
			Command::SetStreamLayers { layers, .. } => {
				self.stream_layers = layers.clone();
				if let Some(s) = &self.streams {
					s.send(StreamInput::Layers(layers));
				}
			}
			Command::SetSrtpProfiles(profiles) => {
				self.stream_peer.srtp_profiles = profiles.clone();
				self.srtp_profiles = Some(profiles.clone());
				if let Some(s) = &self.streams {
					s.send(StreamInput::SrtpProfiles(profiles));
				}
			}
			// Engine-wide.
			Command::CloseSession { .. }
			| Command::TestMicrophone { .. }
			| Command::SetSetting { .. }
			| Command::ResetSetting { .. }
			| Command::AttachSettings(_)
			| Command::AttachHistory(_) => {}
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
		// The stream task's report comes from a replaced source.
		self.own_stream_ended();
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

	/// The nickname relays post our messages under.
	fn relay_nick(&self) -> String {
		if self.nickname.is_empty() { "user".into() } else { self.nickname.clone() }
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

	// Chat history

	/// What the history flows need; `None` until the server's unique id is known.
	fn chat_ctx(&self) -> Option<ChatCtx> {
		Some(ChatCtx {
			session: self.id,
			events: self.events.clone(),
			history: self.history.current(),
			settings: self.settings.current(),
			server_uid: self.state.server_uid.clone()?,
		})
	}

	/// Show a chat's stored messages, then sync it with the gateway.
	fn open_history(&self, target: &ChatTarget) {
		if let Some(ctx) = self.chat_ctx() {
			tokio::spawn(history::open_chat(ctx, target.clone(), self.gateway_client.clone()));
		}
	}

	fn open_histories(&self) {
		for target in &self.open_chats {
			self.open_history(target);
		}
	}

	/// Look up the server's unique id remembered for `alias`, and remember
	/// it under `alias` once a source tells it.
	fn known_server(&mut self, alias: String) {
		if !self.aliases.contains(&alias) {
			self.aliases.push(alias.clone());
		}
		if self.state.server_uid.is_some() {
			return;
		}
		let tx = self.sources_tx.clone();
		self.history.current().run_then(
			false,
			move |s| s.server_alias(&alias),
			move |uid| {
				if let Ok(Some(uid)) = uid {
					let _ = tx.send(SourceEvent::KnownServer(uid));
				}
			},
		);
	}

	/// A source told the server's unique id. The gateway's wins: its
	/// history is the one synced. `true` if the history key changed.
	fn learn_server_uid(&mut self, uid: String, from: Source) -> bool {
		if from != Source::Gateway && self.gateway_client.is_some() {
			return false;
		}
		if from != Source::Query {
			let aliases = self.aliases.clone();
			let key = uid.clone();
			self.history.current().run_then(
				false,
				move |s| aliases.iter().try_for_each(|a| s.set_server_alias(a, &key)),
				|r| {
					if let Err(e) = r {
						debug!("cannot remember the server's aliases: {e}");
					}
				},
			);
		}
		if self.state.server_uid.as_deref() == Some(uid.as_str()) {
			return false;
		}
		self.state.server_uid = Some(uid);
		self.emit_state();
		true
	}

	/// Store a message we send, at once (the copy that comes back is merged).
	fn store_own(&self, target: &ChatTarget, text: &str, route: &ChatRoute) {
		let Some(ctx) = self.chat_ctx() else { return };
		let msg = ChatMessage {
			target: target.clone(),
			author_name: self.relay_nick(),
			author_uid: self.own_uid.clone(),
			author_id: self.state.own_client.filter(|_| *route == ChatRoute::Voice),
			text: text.to_owned(),
			ts_ms: history::now_ms(),
			via_relay: *route != ChatRoute::Voice,
		};
		ctx.store_live(vec![history::new_message(&ctx.server_uid, &msg, MessageSource::Local)]);
	}

	// Gateway

	fn gateway_request(&mut self, request: GatewayRequest) {
		let (Some(client), Some(ctx)) = (self.gateway_client.clone(), self.chat_ctx()) else {
			self.gateway_update(GatewayUpdate::Failed {
				request: request.name().into(),
				code: None,
				message: "not connected to a gateway".into(),
			});
			return;
		};
		match request {
			// The session keeps the directory.
			GatewayRequest::SubscribeStreams { on: true } => self.subscribe_streams(&client),
			request => {
				tokio::spawn(gateway::execute(ctx, client, request));
			}
		}
	}

	fn subscribe_streams(&self, client: &GatewayClient) {
		let Some((generation, _)) = self.gateway else { return };
		let (client, tx) = (client.clone(), self.sources_tx.clone());
		tokio::spawn(async move {
			let result = client.subscribe_streams().await;
			let _ = tx.send(SourceEvent::Directory(generation, result));
		});
	}

	/// Subscribe to what the gateway pushes (errors are reported).
	fn subscribe_gateway(&self, client: &GatewayClient) {
		if client.has(feature::STREAMS) {
			self.subscribe_streams(client);
		}
		let ctx = self.chat_ctx();
		for (feature, request) in [
			(feature::EVENTS, GatewayRequest::SubscribeEvents { on: true }),
			(feature::ACTIVITY, GatewayRequest::SubscribeActivity { on: true }),
		] {
			if let Some(ctx) = ctx.clone().filter(|_| client.has(feature)) {
				tokio::spawn(gateway::execute(ctx, client.clone(), request));
			}
		}
	}

	/// The registered streams of the directory, for the stream task.
	fn feed_directory(&self) {
		let Some(streams) = &self.streams else { return };
		let entries = self
			.directory
			.values()
			.filter_map(|e| {
				Some(StreamInfo {
					id: e.stream_id.clone()?,
					streamer: ClientId(e.client_id?),
					name: e.title.clone(),
					kind: parse_kind(&e.kind),
					// Not in the directory; the stream's offer tells.
					bitrate: 0,
					viewer_limit: 0,
					audio: true,
				})
			})
			.collect();
		streams.send(StreamInput::Directory(entries));
	}

	fn register_own_stream(&mut self) {
		let Some(client) = self.gateway_client.clone().filter(|c| c.has(feature::STREAMS)) else {
			return;
		};
		let Some(own) = self.own_stream.as_mut().filter(|s| !s.registered) else { return };
		own.registered = true;
		let spec = StreamSpec {
			stream_id: own.id.clone(),
			client_id: self.state.own_client,
			channel: self.state.own_channel,
			title: own.title.clone(),
			kind: kind_name(own.kind),
			viewers: None,
		};
		let (session, events) = (self.id, self.events.clone());
		tokio::spawn(async move {
			let update = match client.register_stream(spec).await {
				Ok(stream) => GatewayUpdate::StreamRegistered { stream },
				Err(e) => GatewayUpdate::Failed {
					request: "register_stream".into(),
					code: e.code(),
					message: e.to_string(),
				},
			};
			let _ = events.send(Event::Gateway { session, update });
		});
	}

	fn own_stream_ended(&mut self) {
		let Some(own) = self.own_stream.take() else { return };
		if let Some(client) = self.gateway_client.clone().filter(|_| own.registered) {
			tokio::spawn(async move {
				if let Err(e) = client.unregister_stream(own.id).await {
					debug!("cannot unregister our stream: {e}");
				}
			});
		}
	}

	fn own_stream_event(&mut self, e: OwnStreamEvent) {
		match e {
			OwnStreamEvent::Live { id, title, kind } => {
				self.own_stream_ended();
				self.own_stream = Some(OwnStream { id, title, kind, registered: false });
				self.register_own_stream();
			}
			OwnStreamEvent::Viewers(n) => {
				let registered = self.own_stream.as_ref().filter(|s| s.registered);
				if let (Some(own), Some(client)) = (registered, self.gateway_client.clone()) {
					let id = own.id.clone();
					tokio::spawn(async move {
						if let Err(e) = client.update_stream(id, None, Some(n)).await {
							debug!("cannot update our stream's viewers: {e}");
						}
					});
				}
			}
			OwnStreamEvent::Ended { id } => {
				if self.own_stream.as_ref().is_some_and(|s| s.id == id) {
					self.own_stream_ended();
				}
			}
		}
	}

	fn stop_observing(&mut self) {
		if let Some((_, tx)) = self.gateway.take() {
			let _ = tx.send(GatewayCmd::Stop);
		}
		if let Some((_, tx)) = self.query.take() {
			let _ = tx.send(QueryCmd::Stop);
		}
		self.gateway_gone(None);
		self.gateway_presence = None;
		self.query_presence = None;
		self.state.observe = ObserveState::Off;
		self.emit_state();
	}

	/// Forget the logged-in gateway and what came from it.
	fn gateway_gone(&mut self, reason: Option<String>) {
		if self.gateway_client.take().is_none() {
			return;
		}
		if let Some(own) = &mut self.own_stream {
			own.registered = false;
		}
		if !self.directory.is_empty() {
			self.directory.clear();
			self.feed_directory();
		}
		self.gateway_update(GatewayUpdate::Disconnected { reason });
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
			SourceEvent::OwnStream(g, e) if self.is_current(Source::Voice, g) => {
				self.own_stream_event(e)
			}
			SourceEvent::Directory(g, result) if self.is_current(Source::Gateway, g) => {
				match result {
					Ok(streams) => {
						self.directory =
							streams.iter().map(|e| (e.id.clone(), e.clone())).collect();
						self.feed_directory();
						self.gateway_update(GatewayUpdate::Streams { streams });
					}
					Err(e) => self.gateway_update(GatewayUpdate::Failed {
						request: "subscribe_streams".into(),
						code: e.code(),
						message: e.to_string(),
					}),
				}
			}
			SourceEvent::KnownServer(uid) => {
				// A source may have told the real one meanwhile.
				if self.state.server_uid.is_none() {
					self.state.server_uid = Some(uid);
					self.emit_state();
					self.open_histories();
				}
			}
			_ => debug!("event from a replaced source ignored"),
		}
	}

	fn voice_event(&mut self, e: VoiceEvent) {
		match e {
			VoiceEvent::Connected { name, flavor, own_client, server_uid, own_uid } => {
				self.state.voice = VoiceState::Connected;
				self.state.own_client = Some(own_client);
				if own_uid.is_some() {
					self.own_uid = own_uid;
				}
				self.learn_server_uid(server_uid, Source::Voice);
				self.emit_state();
				let capabilities = flavor.capabilities();
				if capabilities.streams
					&& let Some((generation, voice)) = &self.voice
				{
					let generation = *generation;
					let (own_tx, own_rx) = mpsc::unbounded_channel();
					self.forward(own_rx, move |e| SourceEvent::OwnStream(generation, e));
					let streams = StreamHandle::spawn(
						self.id,
						own_client,
						self.stream_peer.clone(),
						voice.clone(),
						self.events.clone(),
						self.frames.clone(),
						self.audio.clone(),
						self.settings.clone(),
						own_tx,
					);
					if !self.stream_layers.is_empty() {
						streams.send(StreamInput::Layers(self.stream_layers.clone()));
					}
					self.streams = Some(streams);
					self.feed_directory();
				}
				self.emit(Event::ServerInfo { session: self.id, name, flavor, capabilities });
				self.reopen_relayed_chats();
				self.open_histories();
			}
			VoiceEvent::Presence(p) => {
				// Client ids are reused: forget the volumes of those who left.
				if let (Some(a), Some(old)) = (&self.audio, &self.voice_presence) {
					for id in old.clients.keys().filter(|id| !p.clients.contains_key(id)) {
						a.send(AudioIn::ClientLeft(ClientId(*id)));
					}
				}
				if let Some(s) = &self.streams {
					let clients: BTreeMap<_, _> = p
						.clients
						.values()
						.map(|c| (c.id, ClientState { channel: c.channel, streaming: c.streaming }))
						.collect();
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
			VoiceEvent::Chat(msg) => self.chat(msg, MessageSource::Voice, None),
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
				self.state.own_client = None;
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
			GatewayEvent::Connected(client) => {
				self.state.observe = ObserveState::Observing;
				let info = client.info().clone();
				self.gateway_client = Some(client.clone());
				if self.own_uid.is_none() {
					self.own_uid = Some(info.uid.clone());
				}
				self.learn_server_uid(info.server_uid.clone(), Source::Gateway);
				self.emit_state();
				self.gateway_update(GatewayUpdate::Connected {
					gateway_id: info.gateway_id,
					server_uid: info.server_uid,
					server_name: info.server_name,
					uid: info.uid,
					capabilities: client.capabilities(),
				});
				self.subscribe_gateway(&client);
				self.register_own_stream();
				self.open_histories();
			}
			GatewayEvent::Presence(p) => {
				self.gateway_presence = Some(p);
				self.publish_presence();
			}
			GatewayEvent::Push(push) => self.gateway_push(*push),
			GatewayEvent::Disconnected(reason) => {
				self.gateway = None;
				self.gateway_gone(reason.clone());
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

	fn gateway_push(&mut self, push: Push) {
		let ctx = self.chat_ctx();
		match push {
			Push::Chat { id, message } => {
				let entry = HistoryEntry::new(id, message.clone());
				self.chat(message, MessageSource::Gateway, Some(entry));
			}
			Push::Message(entry) => {
				self.chat(entry.message.clone(), MessageSource::Gateway, Some(entry));
			}
			Push::Pinned { target, pin } => match ctx {
				Some(ctx) => ctx.pinned(target, pin),
				None => {
					let message = HistoryMessage::unstored(&pin.entry);
					let pin = gateway::Pin { message, by: pin.by, ts_ms: pin.ts_ms };
					self.gateway_update(GatewayUpdate::Pinned { target, pin });
				}
			},
			Push::Unpinned { target, message_id, by } => match ctx {
				Some(ctx) => ctx.unpinned(target, message_id, by),
				None => self.gateway_update(GatewayUpdate::Unpinned { target, message_id, by }),
			},
			Push::Reaction { target, message_id, emoji, user, added, count } => {
				let push = ReactionPush { target, message_id, emoji, user, added, count };
				match ctx {
					Some(ctx) => ctx.reaction(push, self.own_uid.as_deref()),
					None => self.gateway_update(GatewayUpdate::Reaction {
						target: push.target,
						message_id: push.message_id,
						emoji: push.emoji,
						user: push.user,
						added: push.added,
						count: push.count,
					}),
				}
			}
			Push::TopicUpdated(topic) => self.gateway_update(GatewayUpdate::Topic { topic }),
			Push::EventUpdated(event) => self.gateway_update(GatewayUpdate::Event { event }),
			Push::EventDeleted(id) => self.gateway_update(GatewayUpdate::EventDeleted { id }),
			Push::EventReminder { event, starts_in_ms } => {
				self.gateway_update(GatewayUpdate::EventReminder { event, starts_in_ms });
			}
			Push::StreamStarted(stream) => {
				self.directory.insert(stream.id.clone(), stream.clone());
				self.feed_directory();
				self.gateway_update(GatewayUpdate::StreamStarted { stream });
			}
			Push::StreamUpdated(stream) => {
				self.directory.insert(stream.id.clone(), stream.clone());
				self.feed_directory();
				self.gateway_update(GatewayUpdate::StreamUpdated { stream });
			}
			Push::StreamEnded { id, reason } => {
				if self.directory.remove(&id).is_some() {
					self.feed_directory();
				}
				self.gateway_update(GatewayUpdate::StreamEnded { id, reason });
			}
			Push::ActivityAdded(entry) => {
				self.gateway_update(GatewayUpdate::ActivityAdded { entry })
			}
			Push::Capabilities(capabilities) => {
				self.gateway_update(GatewayUpdate::Capabilities { capabilities });
			}
			Push::Error { code, message } => self.error(format!("gateway: {code:?}: {message}")),
			Push::PresenceSnapshot(_)
			| Push::PresenceDelta(_)
			| Push::Other(_)
			| Push::Disconnected(_) => {}
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
			QueryEvent::Chat(msg) => self.chat(msg, MessageSource::Query, None),
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

	/// A live message: reported, and stored (merged with other copies).
	fn chat(&mut self, msg: ChatMessage, source: MessageSource, entry: Option<HistoryEntry>) {
		let wanted = match &msg.target {
			ChatTarget::Server | ChatTarget::Private(_) => true,
			target => self.open_chats.contains(target) || !msg.via_relay,
		};
		if !wanted {
			return;
		}
		if let Some(ctx) = self.chat_ctx() {
			let stored = match &entry {
				Some(entry) => history::gateway_message(&ctx.server_uid, entry),
				None => history::new_message(&ctx.server_uid, &msg, source),
			};
			ctx.store_live(vec![stored]);
		}
		if !self.dedup.is_duplicate(&msg) {
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
