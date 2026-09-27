//! The client engine.
//!
//! The UI talks to an [`Engine`] with [`Command`]s and receives [`Event`]s.
//! Each server the user works with is a session with up to three sources:
//!
//! - **voice**: a normal client connection (visible, can talk)
//! - **gateway**: a `tsgw` gateway (invisible presence, relayed chat)
//! - **query**: the user's own ServerQuery credentials (same, without a gateway)
//!
//! Presence comes from the most authoritative connected source
//! (voice > gateway > query). Channel chat goes through the voice connection
//! when the user is in that channel, otherwise through a relay.
//!
//! On TeamSpeak 6 servers the voice connection also carries streams (screen
//! sharing): see the `*Stream*` commands and events, and
//! [`Engine::subscribe_frames`] for the frames of watched streams. The audio
//! of watched streams plays through the session's speakers by itself
//! ([`Command::SetStreamVolume`]); capture, encoding and decoding of video
//! are in [`media`] (feature `media`).
//!
//! Settings are a [`settings::Settings`] service: typed keys, runtime
//! values stored in the client database ahead of command line, config file
//! and defaults. The engine reads its own keys from it (e.g. who may watch
//! our stream), takes [`Command::SetSetting`] / [`Command::ResetSetting`],
//! and reports every change as [`Event::SettingChanged`].

mod audio;
mod gateway;
#[cfg(feature = "media")]
pub mod media;
mod query;
mod route;
mod session;
pub mod settings;
pub mod stream;
mod voice;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
pub use voelin_audio::settings::TransmitMode;
pub use voelin_audio::{AudioSettings, ProcessingSettings, VadSettings};
use voelin_model::{Capabilities, ChannelId, ChatMessage, ChatTarget, Presence, ServerFlavor};

use crate::audio::{AudioEvent, AudioHandle, AudioIn};
pub use route::{ChatRoute, Dedup, route_chat};
use settings::{SettingChange, Settings, SharedSettings};
pub use stream::{StreamFrame, StreamSink, StreamState, WatchState};
use voelin_stream::{
	EncodedFrame, LayerId, LayerSpec, SrtpProfile, StreamInfo, StreamSetup, ViewerInfo,
};
pub use voice::VoiceOptions;

pub type SessionId = u64;

/// What the UI asks for.
#[derive(Clone, Debug)]
pub enum Command {
	ConnectVoice {
		session: SessionId,
		options: Box<VoiceOptions>,
	},
	DisconnectVoice {
		session: SessionId,
	},
	/// Invisible presence and relay chat through a `tsgw` gateway.
	ObserveGateway {
		session: SessionId,
		url: String,
		identity: Box<tsclientlib::Identity>,
	},
	/// Invisible presence and relay chat with own ServerQuery credentials.
	ObserveQuery {
		session: SessionId,
		connect: Box<voelin_query::Connect>,
	},
	StopObserving {
		session: SessionId,
	},
	OpenChat {
		session: SessionId,
		target: ChatTarget,
	},
	CloseChat {
		session: SessionId,
		target: ChatTarget,
	},
	SendChat {
		session: SessionId,
		target: ChatTarget,
		text: String,
	},
	MoveToChannel {
		session: SessionId,
		channel: ChannelId,
		password: Option<String>,
	},
	SetInputMuted {
		session: SessionId,
		muted: bool,
	},
	SetOutputMuted {
		session: SessionId,
		muted: bool,
	},
	/// Push-to-talk key or button held (only counts with
	/// [`TransmitMode::PushToTalk`]).
	SetTransmitting {
		session: SessionId,
		on: bool,
	},
	/// Audio settings of all sessions (devices, processing, transmit mode),
	/// also used by sessions started later.
	SetAudioSettings(Box<AudioSettings>),
	/// Playback volume of one client, linear (1 = unchanged, up to 4).
	/// Local only; forgotten when the client leaves.
	SetClientVolume {
		session: SessionId,
		client: u16,
		volume: f32,
	},
	/// Do not play one client (local only).
	SetClientMuted {
		session: SessionId,
		client: u16,
		muted: bool,
	},
	/// Open the microphone without a session to show its level
	/// ([`Event::InputLevel`] with `session: None`), e.g. in the settings.
	TestMicrophone {
		on: bool,
	},
	/// Start streaming in our channel (TeamSpeak 6 only). Progress comes as
	/// [`Event::StreamState`]; frames go in through the sink of
	/// [`StreamState::Live`] or [`Command::SendStreamFrame`].
	StartStream {
		session: SessionId,
		setup: StreamSetup,
		/// Accept every viewer instead of asking with [`Event::StreamViewerRequest`].
		auto_accept: bool,
	},
	StopStream {
		session: SessionId,
	},
	/// Answer an [`Event::StreamViewerRequest`].
	AcceptViewer {
		session: SessionId,
		viewer: u16,
		accept: bool,
	},
	/// Remove a viewer from our stream.
	KickViewer {
		session: SessionId,
		viewer: u16,
	},
	/// An encoded frame for our stream.
	SendStreamFrame {
		session: SessionId,
		frame: EncodedFrame,
	},
	/// The simulcast layers of our stream (empty: one layer at the setup's
	/// bitrate). Applies to the live stream at once (viewers move to the
	/// layers their bandwidth allows, keyframes are requested) and to streams
	/// started later; the encoder must produce the same layers
	/// (`media::StreamerConfig::layers`).
	SetStreamLayers {
		session: SessionId,
		layers: Vec<LayerSpec>,
	},
	/// SRTP protection profiles for stream connections, in order of
	/// preference (a user setting; empty: the default,
	/// `SrtpProfile::DEFAULT_ORDER`). Applies to connections made from now
	/// on, in all sessions and those started later.
	SetSrtpProfiles(Vec<SrtpProfile>),
	/// Watch a stream from [`Event::StreamsChanged`]. Frames arrive through
	/// [`Engine::subscribe_frames`].
	WatchStream {
		session: SessionId,
		stream_id: String,
	},
	LeaveStream {
		session: SessionId,
		stream_id: String,
	},
	/// Ask the streamer of a watched stream for a keyframe.
	RequestStreamKeyframe {
		session: SessionId,
		stream_id: String,
	},
	/// Playback volume of a watched stream's audio, linear (1 = unchanged,
	/// up to 4).
	SetStreamVolume {
		session: SessionId,
		stream_id: String,
		volume: f32,
	},
	/// Close everything of a session.
	CloseSession {
		session: SessionId,
	},
	/// Set a setting's runtime value (see [`settings`]); invalid values are
	/// reported as [`Event::SettingRejected`].
	SetSetting {
		key: String,
		value: serde_json::Value,
	},
	/// Remove a setting's runtime value: the command line, config file or
	/// default applies again.
	ResetSetting {
		key: String,
	},
	/// Use these settings from now on (e.g. the ones of the client database,
	/// for an engine started without).
	AttachSettings(Settings),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VoiceState {
	#[default]
	Disconnected,
	Connecting,
	Connected,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ObserveState {
	#[default]
	Off,
	Connecting,
	/// Invisible presence is live.
	Observing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
	Voice,
	Gateway,
	Query,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionState {
	pub voice: VoiceState,
	pub observe: ObserveState,
	/// Where presence currently comes from.
	pub presence_source: Option<Source>,
	/// Our own channel while voice-connected.
	pub own_channel: Option<ChannelId>,
	/// Our own client id while voice-connected.
	pub own_client: Option<u16>,
	pub input_muted: bool,
	pub output_muted: bool,
	pub transmitting: bool,
}

/// What the engine reports.
#[derive(Clone, Debug)]
pub enum Event {
	State {
		session: SessionId,
		state: SessionState,
	},
	ServerInfo {
		session: SessionId,
		name: String,
		flavor: ServerFlavor,
		capabilities: Capabilities,
	},
	/// Full presence of the session (the UI rebuilds its view from it).
	Presence {
		session: SessionId,
		presence: Arc<Presence>,
	},
	Chat {
		session: SessionId,
		message: ChatMessage,
	},
	/// A client in the session started or stopped talking (voice only).
	Talking {
		session: SessionId,
		client: u16,
		talking: bool,
	},
	/// Microphone level for a meter, about ten times a second while the
	/// microphone is open: of a voice session, or of
	/// [`Command::TestMicrophone`] (`session: None`). `level_db` is the
	/// loudest 10 ms (dBFS, after processing) since the last report;
	/// `sending` whether voice is transmitted.
	InputLevel {
		session: Option<SessionId>,
		level_db: f32,
		sending: bool,
	},
	/// A problem with the microphone test (e.g. no device).
	MicrophoneTestError {
		message: String,
	},
	Error {
		session: SessionId,
		message: String,
	},
	/// The streams in our channel (TeamSpeak 6), full list.
	StreamsChanged {
		session: SessionId,
		streams: Vec<StreamInfo>,
	},
	/// Our own stream.
	StreamState {
		session: SessionId,
		state: StreamState,
	},
	/// Someone asks to watch our stream; answer with [`Command::AcceptViewer`].
	StreamViewerRequest {
		session: SessionId,
		viewer: u16,
		message: String,
	},
	/// The viewers of our stream and their state, full list.
	StreamViewers {
		session: SessionId,
		viewers: Vec<ViewerInfo>,
	},
	/// A viewer of simulcast layer `layer` (0 without simulcast) needs a
	/// keyframe (also flagged on the [`StreamSink`]).
	StreamKeyframeRequest {
		session: SessionId,
		layer: LayerId,
	},
	/// The bitrate target of a layer of our stream changed: the lowest
	/// bandwidth estimate of its viewers (bit/s; also on the [`StreamSink`]).
	StreamLayerBitrate {
		session: SessionId,
		layer: LayerId,
		bitrate: u64,
	},
	/// A stream we watch.
	WatchState {
		session: SessionId,
		stream_id: String,
		state: WatchState,
	},
	/// A setting was set or reset (by anyone: a command, the UI through
	/// [`Engine::settings`], …). Read the new value from the settings.
	SettingChanged {
		key: String,
	},
	/// A [`Command::SetSetting`] / [`Command::ResetSetting`] was refused.
	SettingRejected {
		key: String,
		message: String,
	},
}

/// Handle to the engine. Cheap to clone; all methods are non-blocking.
#[derive(Clone)]
pub struct Engine {
	commands: mpsc::UnboundedSender<Command>,
	events: broadcast::Sender<Event>,
	frames: broadcast::Sender<StreamFrame>,
	runtime: tokio::runtime::Handle,
	settings: SharedSettings,
}

impl Engine {
	/// Start the engine on the current tokio runtime, with settings that are
	/// not stored (see [`Command::AttachSettings`]).
	pub fn start() -> Self {
		Self::start_with_settings(Settings::in_memory())
	}

	/// Start the engine on the current tokio runtime with `settings`.
	pub fn start_with_settings(settings: Settings) -> Self {
		let (commands, rx) = mpsc::unbounded_channel();
		let (events, _) = broadcast::channel(4096);
		// About a minute of one watched stream.
		let (frames, _) = broadcast::channel(4096);
		let settings = SharedSettings::new(settings);
		tokio::spawn(run(rx, events.clone(), frames.clone(), settings.clone()));
		Self { commands, events, frames, runtime: tokio::runtime::Handle::current(), settings }
	}

	/// The settings the engine uses (read, watch or change them directly;
	/// changes are reported as [`Event::SettingChanged`] all the same).
	pub fn settings(&self) -> Settings {
		self.settings.current()
	}

	/// The runtime the engine runs on, for helpers started from other
	/// threads (e.g. `media::Viewer` from a UI thread).
	pub fn runtime(&self) -> &tokio::runtime::Handle {
		&self.runtime
	}

	pub fn send(&self, command: Command) {
		let _ = self.commands.send(command);
	}

	pub fn subscribe(&self) -> broadcast::Receiver<Event> {
		self.events.subscribe()
	}

	/// Encoded frames of the streams we watch (kept off the event bus).
	pub fn subscribe_frames(&self) -> broadcast::Receiver<StreamFrame> {
		self.frames.subscribe()
	}
}

/// What woke the engine's loop.
enum Input {
	Command(Command),
	Setting(SettingChange),
	/// Changes were missed: the engine re-reads what it uses.
	SettingsLagged,
}

/// A command that applies the stored audio settings if they differ from
/// what runs (e.g. set through [`Command::SetSetting`]).
fn stored_audio(settings: &Settings, running: &AudioSettings) -> Option<Command> {
	let stored = settings.get_arc(&settings::AUDIO);
	(*stored != *running).then(|| Command::SetAudioSettings(Box::new((*stored).clone())))
}

async fn run(
	mut commands: mpsc::UnboundedReceiver<Command>,
	events: broadcast::Sender<Event>,
	frames: broadcast::Sender<StreamFrame>,
	shared: SharedSettings,
) {
	let mut sessions: HashMap<SessionId, session::SessionHandle> = HashMap::new();
	let mut current = shared.current();
	let mut changes = current.subscribe();
	let mut settings = AudioSettings::default();
	let mut srtp_profiles: Option<Vec<SrtpProfile>> = None;
	let mut mic_test: Option<AudioHandle> = None;
	loop {
		let input = tokio::select! {
			command = commands.recv() => match command {
				Some(command) => Input::Command(command),
				None => break,
			},
			change = changes.recv() => match change {
				Ok(change) => Input::Setting(change),
				Err(broadcast::error::RecvError::Lagged(_)) => Input::SettingsLagged,
				// Not while `current` holds the settings.
				Err(broadcast::error::RecvError::Closed) => continue,
			},
		};
		let command = match input {
			Input::Command(command) => command,
			Input::Setting(change) => {
				let audio = change.key == settings::AUDIO.name();
				let _ = events.send(Event::SettingChanged { key: change.key });
				match audio.then(|| stored_audio(&current, &settings)).flatten() {
					Some(command) => command,
					None => continue,
				}
			}
			Input::SettingsLagged => match stored_audio(&current, &settings) {
				Some(command) => command,
				None => continue,
			},
		};
		let id = match command {
			Command::SetAudioSettings(new) => {
				settings = (*new).clone();
				for s in sessions.values() {
					s.send(Command::SetAudioSettings(new.clone()));
				}
				if let Some(test) = &mic_test {
					test.send(AudioIn::Settings(new));
				}
				continue;
			}
			Command::TestMicrophone { on } => {
				mic_test = on.then(|| test_microphone(&settings, &events));
				continue;
			}
			Command::SetSrtpProfiles(profiles) => {
				for s in sessions.values() {
					s.send(Command::SetSrtpProfiles(profiles.clone()));
				}
				srtp_profiles = Some(profiles);
				continue;
			}
			Command::SetSetting { key, value } => {
				if let Err(e) = current.set_json(&key, value) {
					let _ = events.send(Event::SettingRejected { key, message: e.to_string() });
				}
				continue;
			}
			Command::ResetSetting { key } => {
				if let Err(e) = current.reset(&key) {
					let _ = events.send(Event::SettingRejected { key, message: e.to_string() });
				}
				continue;
			}
			Command::AttachSettings(new) => {
				shared.replace(new.clone());
				changes = new.subscribe();
				current = new;
				continue;
			}
			ref command => command_session(command),
		};
		if matches!(command, Command::CloseSession { .. }) {
			if let Some(s) = sessions.remove(&id) {
				s.send(command);
			}
			continue;
		}
		sessions
			.entry(id)
			.or_insert_with(|| {
				let s = session::SessionHandle::spawn(
					id,
					events.clone(),
					frames.clone(),
					settings.clone(),
					shared.clone(),
				);
				if let Some(profiles) = &srtp_profiles {
					s.send(Command::SetSrtpProfiles(profiles.clone()));
				}
				s
			})
			.send(command);
	}
}

/// An audio thread whose packets go nowhere, for its level.
fn test_microphone(settings: &AudioSettings, events: &broadcast::Sender<Event>) -> AudioHandle {
	let (packets, _) = mpsc::unbounded_channel();
	let (audio_tx, mut audio_rx) = mpsc::unbounded_channel();
	let handle = audio::spawn(packets, audio_tx, settings.clone());
	let events = events.clone();
	tokio::spawn(async move {
		while let Some(event) = audio_rx.recv().await {
			let _ = events.send(match event {
				AudioEvent::Level { db, sending } => {
					Event::InputLevel { session: None, level_db: db, sending }
				}
				AudioEvent::Error(message) => Event::MicrophoneTestError { message },
			});
		}
	});
	handle
}

/// The session a command is for (engine-wide commands are handled before).
fn command_session(command: &Command) -> SessionId {
	match command {
		Command::ConnectVoice { session, .. }
		| Command::DisconnectVoice { session }
		| Command::ObserveGateway { session, .. }
		| Command::ObserveQuery { session, .. }
		| Command::StopObserving { session }
		| Command::OpenChat { session, .. }
		| Command::CloseChat { session, .. }
		| Command::SendChat { session, .. }
		| Command::MoveToChannel { session, .. }
		| Command::SetInputMuted { session, .. }
		| Command::SetOutputMuted { session, .. }
		| Command::SetTransmitting { session, .. }
		| Command::StartStream { session, .. }
		| Command::StopStream { session }
		| Command::AcceptViewer { session, .. }
		| Command::KickViewer { session, .. }
		| Command::SendStreamFrame { session, .. }
		| Command::SetStreamLayers { session, .. }
		| Command::WatchStream { session, .. }
		| Command::LeaveStream { session, .. }
		| Command::RequestStreamKeyframe { session, .. }
		| Command::SetStreamVolume { session, .. }
		| Command::SetClientVolume { session, .. }
		| Command::SetClientMuted { session, .. }
		| Command::CloseSession { session } => *session,
		Command::SetAudioSettings(_)
		| Command::SetSrtpProfiles(_)
		| Command::TestMicrophone { .. }
		| Command::SetSetting { .. }
		| Command::ResetSetting { .. }
		| Command::AttachSettings(_) => {
			unreachable!("engine-wide commands have no session")
		}
	}
}
