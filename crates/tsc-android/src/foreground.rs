//! When the app needs its voice foreground service.
//!
//! Android keeps the microphone and the process of a backgrounded app only
//! while a foreground service with a visible notification runs. The service
//! runs while any session has a voice connection (connecting or connected);
//! its notification names the servers and offers "Disconnect".

use std::collections::BTreeMap;

use tsc_core::{Event, SessionId, VoiceState};

#[derive(Default)]
pub struct Foreground {
	sessions: BTreeMap<SessionId, Session>,
}

#[derive(Default)]
struct Session {
	name: Option<String>,
	voice: VoiceState,
}

impl Foreground {
	/// Apply `event`. Returns the new notification text when it changed:
	/// `Some(None)` means the service should stop.
	pub fn update(&mut self, event: &Event) -> Option<Option<String>> {
		let before = self.notification();
		match event {
			Event::State { session, state } => {
				self.sessions.entry(*session).or_default().voice = state.voice;
			}
			Event::ServerInfo { session, name, .. } => {
				self.sessions.entry(*session).or_default().name = Some(name.clone());
			}
			_ => return None,
		}
		let after = self.notification();
		(after != before).then_some(after)
	}

	/// Sessions with a voice connection (for "Disconnect").
	pub fn voice_sessions(&self) -> Vec<SessionId> {
		self.sessions
			.iter()
			.filter(|(_, s)| s.voice != VoiceState::Disconnected)
			.map(|(id, _)| *id)
			.collect()
	}

	/// The notification text, `None` without voice.
	pub fn notification(&self) -> Option<String> {
		let active: Vec<&Session> =
			self.sessions.values().filter(|s| s.voice != VoiceState::Disconnected).collect();
		if active.is_empty() {
			return None;
		}
		let names: Vec<&str> =
			active.iter().map(|s| s.name.as_deref().unwrap_or("a server")).collect();
		let names = names.join(", ");
		Some(if active.iter().all(|s| s.voice == VoiceState::Connecting) {
			format!("Connecting to {names}")
		} else {
			format!("In voice on {names}")
		})
	}
}

#[cfg(test)]
mod tests {
	use tsc_core::SessionState;
	use tsc_model::{Capabilities, ServerFlavor};

	use super::*;

	fn voice(session: SessionId, voice: VoiceState) -> Event {
		Event::State { session, state: SessionState { voice, ..SessionState::default() } }
	}

	#[test]
	fn service_follows_voice_connections() {
		let mut fg = Foreground::default();
		assert_eq!(fg.update(&voice(1, VoiceState::Disconnected)), None);
		assert_eq!(
			fg.update(&voice(1, VoiceState::Connecting)),
			Some(Some("Connecting to a server".into()))
		);
		let info = Event::ServerInfo {
			session: 1,
			name: "Home".into(),
			flavor: ServerFlavor::Unknown(String::new()),
			capabilities: Capabilities::default(),
		};
		assert_eq!(fg.update(&info), Some(Some("Connecting to Home".into())));
		assert_eq!(
			fg.update(&voice(1, VoiceState::Connected)),
			Some(Some("In voice on Home".into()))
		);
		assert_eq!(fg.update(&voice(1, VoiceState::Connected)), None, "no change");
		assert_eq!(
			fg.update(&voice(2, VoiceState::Connecting)),
			Some(Some("In voice on Home, a server".into()))
		);
		assert_eq!(fg.voice_sessions(), [1, 2]);
		assert_eq!(fg.update(&Event::Error { session: 1, message: "x".into() }), None);
		fg.update(&voice(2, VoiceState::Disconnected));
		assert_eq!(fg.update(&voice(1, VoiceState::Disconnected)), Some(None));
		assert!(fg.voice_sessions().is_empty());
	}
}
