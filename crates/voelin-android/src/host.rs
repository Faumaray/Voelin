//! The engine for the whole process.
//!
//! Android destroys and recreates the activity (and so the window and its
//! thread) while a voice call goes on in a foreground service. The engine
//! therefore lives here, on its own runtime, for as long as the process; a
//! new window [attaches](EngineHost::attach) and first receives events that
//! rebuild the current state ([`Replay`]), then the live ones.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::runtime::{Handle, Runtime};
use tokio::sync::{broadcast, mpsc};
use voelin_core::{Engine, Event, SessionId, StreamState, WatchState};

/// Chat messages per session a new window gets back.
pub const CHAT_BACKLOG: usize = 300;

/// The latest state-bearing events per session.
#[derive(Default)]
pub struct Replay {
	sessions: BTreeMap<SessionId, SessionReplay>,
}

#[derive(Default)]
struct SessionReplay {
	server_info: Option<Event>,
	state: Option<Event>,
	presence: Option<Event>,
	streams: Option<Event>,
	own_stream: Option<Event>,
	viewers: Option<Event>,
	watching: BTreeMap<String, Event>,
	chat: VecDeque<Event>,
}

impl Replay {
	/// Remember what `event` changes.
	pub fn record(&mut self, event: &Event) {
		let session = match event {
			Event::State { session, .. }
			| Event::ServerInfo { session, .. }
			| Event::Presence { session, .. }
			| Event::Chat { session, .. }
			| Event::StreamsChanged { session, .. }
			| Event::StreamState { session, .. }
			| Event::StreamViewers { session, .. }
			| Event::WatchState { session, .. } => *session,
			// Transient: talking flags, errors, requests, keyframe requests.
			_ => return,
		};
		let s = self.sessions.entry(session).or_default();
		let slot = Some(event.clone());
		match event {
			Event::State { .. } => s.state = slot,
			Event::ServerInfo { .. } => s.server_info = slot,
			Event::Presence { .. } => s.presence = slot,
			Event::StreamsChanged { .. } => s.streams = slot,
			Event::StreamViewers { .. } => s.viewers = slot,
			Event::StreamState { state, .. } => {
				if matches!(state, StreamState::Ended(_)) {
					s.own_stream = None;
					s.viewers = None;
				} else {
					s.own_stream = slot;
				}
			}
			Event::WatchState { stream_id, state, .. } => {
				if matches!(state, WatchState::Ended(_)) {
					s.watching.remove(stream_id);
				} else {
					s.watching.insert(stream_id.clone(), event.clone());
				}
			}
			Event::Chat { .. } => {
				if s.chat.len() == CHAT_BACKLOG {
					s.chat.pop_front();
				}
				s.chat.push_back(event.clone());
			}
			_ => {}
		}
	}

	/// Events that rebuild the recorded state, per session: server info,
	/// state, presence, chat, then streams.
	pub fn events(&self) -> Vec<Event> {
		let mut out = Vec::new();
		for s in self.sessions.values() {
			out.extend(s.server_info.iter().cloned());
			out.extend(s.state.iter().cloned());
			out.extend(s.presence.iter().cloned());
			out.extend(s.chat.iter().cloned());
			out.extend(s.streams.iter().cloned());
			out.extend(s.own_stream.iter().cloned());
			out.extend(s.viewers.iter().cloned());
			out.extend(s.watching.values().cloned());
		}
		out
	}
}

/// Events of one source, recorded and passed on to the attached window.
#[derive(Clone, Default)]
pub struct Fanout {
	shared: Arc<Mutex<FanoutState>>,
}

#[derive(Default)]
struct FanoutState {
	replay: Replay,
	window: Option<mpsc::UnboundedSender<Event>>,
}

impl Fanout {
	fn lock(&self) -> MutexGuard<'_, FanoutState> {
		self.shared.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Record `event` and pass it to the window, if one is attached.
	pub fn publish(&self, event: Event) {
		let mut state = self.lock();
		state.replay.record(&event);
		if let Some(window) = &state.window
			&& window.send(event).is_err()
		{
			state.window = None;
		}
	}

	/// Receive events from now on, starting with the replay. A previously
	/// attached window stops receiving (its channel closes).
	pub fn attach(&self) -> mpsc::UnboundedReceiver<Event> {
		let (tx, rx) = mpsc::unbounded_channel();
		let mut state = self.lock();
		for event in state.replay.events() {
			let _ = tx.send(event);
		}
		state.window = Some(tx);
		rx
	}

	/// Forward `source` until it closes; then the window's channel closes.
	pub async fn pump(self, mut source: broadcast::Receiver<Event>) {
		loop {
			match source.recv().await {
				Ok(event) => self.publish(event),
				Err(broadcast::error::RecvError::Lagged(n)) => {
					tracing::warn!(missed = n, "engine events lagged");
				}
				Err(broadcast::error::RecvError::Closed) => break,
			}
		}
		self.lock().window = None;
	}
}

/// The process's engine, runtime and event fan-out.
pub struct EngineHost {
	runtime: Runtime,
	engine: Engine,
	fanout: Fanout,
}

/// What a window needs: see `voelin_ui::HostedEngine`.
pub struct Attached {
	pub engine: Engine,
	pub runtime: Handle,
	pub events: mpsc::UnboundedReceiver<Event>,
}

impl EngineHost {
	pub fn start() -> std::io::Result<Self> {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.thread_name("voelin-engine")
			.build()?;
		let engine = runtime.block_on(async { Engine::start() });
		let fanout = Fanout::default();
		runtime.spawn(fanout.clone().pump(engine.subscribe()));
		Ok(Self { runtime, engine, fanout })
	}

	pub fn engine(&self) -> &Engine {
		&self.engine
	}

	pub fn runtime(&self) -> &Handle {
		self.runtime.handle()
	}

	/// Hand the engine to a new window.
	pub fn attach(&self) -> Attached {
		Attached {
			engine: self.engine.clone(),
			runtime: self.runtime.handle().clone(),
			events: self.fanout.attach(),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc as StdArc;

	use voelin_core::{SessionState, VoiceState};
	use voelin_model::{ChatMessage, ChatTarget, Presence};

	use super::*;

	fn state(session: SessionId, voice: VoiceState) -> Event {
		Event::State { session, state: SessionState { voice, ..SessionState::default() } }
	}

	fn chat(session: SessionId, text: &str) -> Event {
		Event::Chat {
			session,
			message: ChatMessage {
				target: ChatTarget::Server,
				author_name: "a".into(),
				author_uid: None,
				author_id: None,
				text: text.into(),
				ts_ms: 0,
				via_relay: false,
				blocked: false,
			},
		}
	}

	fn describe(events: &[Event]) -> Vec<String> {
		events
			.iter()
			.map(|e| match e {
				Event::State { session, state } => format!("{session} state {:?}", state.voice),
				Event::Presence { session, .. } => format!("{session} presence"),
				Event::Chat { session, message } => format!("{session} chat {}", message.text),
				Event::WatchState { session, stream_id, .. } => {
					format!("{session} watch {stream_id}")
				}
				other => format!("{other:?}"),
			})
			.collect()
	}

	#[test]
	fn replay_keeps_the_latest_state_and_the_chat_backlog() {
		let mut replay = Replay::default();
		replay.record(&state(2, VoiceState::Connecting));
		replay.record(&Event::Presence { session: 2, presence: StdArc::new(Presence::default()) });
		replay.record(&state(2, VoiceState::Connected));
		replay.record(&Event::Talking { session: 2, client: 5, talking: true });
		replay.record(&Event::Error { session: 2, message: "x".into() });
		for i in 0..CHAT_BACKLOG + 2 {
			replay.record(&chat(1, &i.to_string()));
		}
		replay.record(&Event::WatchState {
			session: 2,
			stream_id: "s1".into(),
			state: WatchState::Connected,
		});
		replay.record(&Event::WatchState {
			session: 2,
			stream_id: "s2".into(),
			state: WatchState::Requested,
		});
		replay.record(&Event::WatchState {
			session: 2,
			stream_id: "s2".into(),
			state: WatchState::Ended(voelin_core::stream::EndReason::Local),
		});

		let events = describe(&replay.events());
		assert_eq!(events.len(), CHAT_BACKLOG + 3);
		assert_eq!(events[0], "1 chat 2", "the oldest messages are dropped");
		assert_eq!(events[CHAT_BACKLOG - 1], format!("1 chat {}", CHAT_BACKLOG + 1));
		assert_eq!(
			events[CHAT_BACKLOG..],
			["2 state Connected", "2 presence", "2 watch s1"].map(String::from)
		);
	}

	#[tokio::test]
	async fn a_new_window_gets_the_replay_then_live_events() {
		let (tx, rx) = broadcast::channel(16);
		let fanout = Fanout::default();
		let pump = tokio::spawn(fanout.clone().pump(rx));

		tx.send(state(1, VoiceState::Connected)).unwrap();
		tx.send(chat(1, "before")).unwrap();
		tokio::task::yield_now().await;
		while fanout.lock().replay.events().len() < 2 {
			tokio::task::yield_now().await;
		}

		let mut first = fanout.attach();
		assert_eq!(describe(&[first.recv().await.unwrap()]), ["1 state Connected"]);
		assert_eq!(describe(&[first.recv().await.unwrap()]), ["1 chat before"]);
		tx.send(chat(1, "live")).unwrap();
		assert_eq!(describe(&[first.recv().await.unwrap()]), ["1 chat live"]);

		// The activity is recreated: the old window's channel ends.
		let mut second = fanout.attach();
		assert!(first.recv().await.is_none());
		let mut replayed = Vec::new();
		for _ in 0..3 {
			replayed.push(second.recv().await.unwrap());
		}
		assert_eq!(describe(&replayed), ["1 state Connected", "1 chat before", "1 chat live"]);

		drop(tx);
		pump.await.unwrap();
		assert!(second.recv().await.is_none(), "the source closed");
	}
}
