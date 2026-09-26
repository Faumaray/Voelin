//! When the app needs its voice foreground service.
//!
//! Android keeps the microphone and the process of a backgrounded app only
//! while a foreground service with a visible notification runs. The service
//! runs while any session has a voice connection (connecting or connected);
//! its notification names the servers and offers Mute and Disconnect.

use std::collections::BTreeMap;

use voelin_core::{Event, SessionId, VoiceState};

/// What the voice notification shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceNotice {
	pub text: String,
	/// The microphone is muted in every voice session (the action unmutes).
	pub muted: bool,
}

#[derive(Default)]
pub struct Foreground {
	sessions: BTreeMap<SessionId, Session>,
}

#[derive(Default)]
struct Session {
	name: Option<String>,
	voice: VoiceState,
	muted: bool,
}

impl Foreground {
	/// Apply `event`. Returns the new notification when it changed:
	/// `Some(None)` means the service should stop.
	pub fn update(&mut self, event: &Event) -> Option<Option<VoiceNotice>> {
		let before = self.notice();
		match event {
			Event::State { session, state } => {
				let s = self.sessions.entry(*session).or_default();
				s.voice = state.voice;
				s.muted = state.input_muted;
			}
			Event::ServerInfo { session, name, .. } => {
				self.sessions.entry(*session).or_default().name = Some(name.clone());
			}
			_ => return None,
		}
		let after = self.notice();
		(after != before).then_some(after)
	}

	fn active(&self) -> impl Iterator<Item = (&SessionId, &Session)> {
		self.sessions.iter().filter(|(_, s)| s.voice != VoiceState::Disconnected)
	}

	/// Sessions with a voice connection (for the notification's actions).
	pub fn voice_sessions(&self) -> Vec<SessionId> {
		self.active().map(|(id, _)| *id).collect()
	}

	/// The notification, `None` without voice.
	pub fn notice(&self) -> Option<VoiceNotice> {
		let active: Vec<&Session> = self.active().map(|(_, s)| s).collect();
		if active.is_empty() {
			return None;
		}
		let names: Vec<&str> =
			active.iter().map(|s| s.name.as_deref().unwrap_or("a server")).collect();
		let names = names.join(", ");
		let text = if active.iter().all(|s| s.voice == VoiceState::Connecting) {
			format!("Connecting to {names}")
		} else {
			format!("In voice on {names}")
		};
		Some(VoiceNotice { text, muted: active.iter().all(|s| s.muted) })
	}
}

#[cfg(test)]
mod tests {
	use voelin_core::SessionState;
	use voelin_model::{Capabilities, ServerFlavor};

	use super::*;

	fn voice(session: SessionId, voice: VoiceState, input_muted: bool) -> Event {
		Event::State {
			session,
			state: SessionState { voice, input_muted, ..SessionState::default() },
		}
	}

	fn notice(text: &str, muted: bool) -> Option<Option<VoiceNotice>> {
		Some(Some(VoiceNotice { text: text.into(), muted }))
	}

	#[test]
	fn service_follows_voice_connections() {
		let mut fg = Foreground::default();
		assert_eq!(fg.update(&voice(1, VoiceState::Disconnected, false)), None);
		assert_eq!(
			fg.update(&voice(1, VoiceState::Connecting, false)),
			notice("Connecting to a server", false)
		);
		let info = Event::ServerInfo {
			session: 1,
			name: "Home".into(),
			flavor: ServerFlavor::Unknown(String::new()),
			capabilities: Capabilities::default(),
		};
		assert_eq!(fg.update(&info), notice("Connecting to Home", false));
		assert_eq!(
			fg.update(&voice(1, VoiceState::Connected, false)),
			notice("In voice on Home", false)
		);
		assert_eq!(fg.update(&voice(1, VoiceState::Connected, false)), None, "no change");
		assert_eq!(
			fg.update(&voice(1, VoiceState::Connected, true)),
			notice("In voice on Home", true)
		);
		// A second session that is not muted: the action mutes both.
		assert_eq!(
			fg.update(&voice(2, VoiceState::Connecting, false)),
			notice("In voice on Home, a server", false)
		);
		assert_eq!(fg.voice_sessions(), [1, 2]);
		assert_eq!(fg.update(&Event::Error { session: 1, message: "x".into() }), None);
		fg.update(&voice(2, VoiceState::Disconnected, false));
		assert_eq!(fg.update(&voice(1, VoiceState::Disconnected, true)), Some(None));
		assert!(fg.voice_sessions().is_empty());
	}
}
