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

mod audio;
mod gateway;
mod query;
mod route;
mod session;
mod voice;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tsc_model::{Capabilities, ChannelId, ChatMessage, ChatTarget, Presence, ServerFlavor};

pub use route::{ChatRoute, Dedup, route_chat};
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
		connect: Box<tsc_query::Connect>,
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
	/// Push-to-talk state (or "always transmit" when voice activation is off).
	SetTransmitting {
		session: SessionId,
		on: bool,
	},
	/// Close everything of a session.
	CloseSession {
		session: SessionId,
	},
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
	Error {
		session: SessionId,
		message: String,
	},
}

/// Handle to the engine. Cheap to clone; all methods are non-blocking.
#[derive(Clone)]
pub struct Engine {
	commands: mpsc::UnboundedSender<Command>,
	events: broadcast::Sender<Event>,
}

impl Engine {
	/// Start the engine on the current tokio runtime.
	pub fn start() -> Self {
		let (commands, rx) = mpsc::unbounded_channel();
		let (events, _) = broadcast::channel(4096);
		tokio::spawn(run(rx, events.clone()));
		Self { commands, events }
	}

	pub fn send(&self, command: Command) {
		let _ = self.commands.send(command);
	}

	pub fn subscribe(&self) -> broadcast::Receiver<Event> {
		self.events.subscribe()
	}
}

async fn run(mut commands: mpsc::UnboundedReceiver<Command>, events: broadcast::Sender<Event>) {
	let mut sessions: HashMap<SessionId, session::SessionHandle> = HashMap::new();
	while let Some(command) = commands.recv().await {
		let id = command_session(&command);
		if matches!(command, Command::CloseSession { .. }) {
			if let Some(s) = sessions.remove(&id) {
				s.send(command);
			}
			continue;
		}
		sessions
			.entry(id)
			.or_insert_with(|| session::SessionHandle::spawn(id, events.clone()))
			.send(command);
	}
}

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
		| Command::CloseSession { session } => *session,
	}
}
