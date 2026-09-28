//! The server rail.

use voelin_core::{ObserveState, SessionState, VoiceState};
use voelin_store::Bookmark;

use crate::app::ServerItem;
use crate::vm::avatar;

/// "offline", "connecting", "observing" or "connected".
pub fn status(state: &SessionState) -> &'static str {
	match (state.voice, state.observe) {
		(VoiceState::Connected, _) => "connected",
		(VoiceState::Connecting, _) | (_, ObserveState::Connecting) => "connecting",
		(_, ObserveState::Observing) => "observing",
		_ => "offline",
	}
}

/// A server of the rail. `unread`: messages in its chats; `live`:
/// streams in our channel there.
pub fn item(bookmark: &Bookmark, state: &SessionState, unread: i32, live: bool) -> ServerItem {
	ServerItem {
		id: bookmark.id as i32,
		name: bookmark.name.clone().into(),
		address: bookmark.address.clone().into(),
		status: status(state).into(),
		initials: avatar::initials(&bookmark.name).into(),
		tint: avatar::tint(&bookmark.name),
		unread,
		live,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn states() {
		let mut s = SessionState::default();
		assert_eq!(status(&s), "offline");
		s.observe = ObserveState::Observing;
		assert_eq!(status(&s), "observing");
		s.voice = VoiceState::Connecting;
		assert_eq!(status(&s), "connecting");
		s.voice = VoiceState::Connected;
		assert_eq!(status(&s), "connected");
	}
}
