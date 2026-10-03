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

/// A `ts3server://` link that joins the channel at `path` (its names from
/// the top) on the server at `address` (`host`, `host:port` or
/// `[v6]:port`), as TeamSpeak clients open them.
pub fn invite_link(address: &str, path: &[&str]) -> String {
	fn encode(text: &str) -> String {
		text.bytes()
			.map(|b| match b {
				b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
					char::from(b).to_string()
				}
				_ => format!("%{b:02X}"),
			})
			.collect()
	}
	let address = address.trim();
	// A port follows the last colon, unless that colon is inside an IPv6
	// address without brackets.
	let (host, port) = match address.rsplit_once(':') {
		Some((host, port))
			if !port.is_empty()
				&& port.bytes().all(|b| b.is_ascii_digit())
				&& (!host.contains(':') || host.ends_with(']')) =>
		{
			(host, Some(port))
		}
		_ => (address, None),
	};
	let mut link = format!("ts3server://{host}");
	let mut query = Vec::new();
	if let Some(port) = port {
		query.push(format!("port={port}"));
	}
	if !path.is_empty() {
		let names: Vec<String> = path.iter().map(|n| encode(n)).collect();
		query.push(format!("channel={}", names.join("/")));
	}
	if !query.is_empty() {
		link.push('?');
		link.push_str(&query.join("&"));
	}
	link
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn invite_links() {
		assert_eq!(
			invite_link("ts.example.com:9988", &["Gaming", "Raid Night"]),
			"ts3server://ts.example.com?port=9988&channel=Gaming/Raid%20Night"
		);
		assert_eq!(invite_link(" ts.example.com ", &[]), "ts3server://ts.example.com");
		assert_eq!(
			invite_link("[2001:db8::1]:9987", &["Chill & Co"]),
			"ts3server://[2001:db8::1]?port=9987&channel=Chill%20%26%20Co"
		);
		// A bare IPv6 address has no port.
		assert_eq!(invite_link("2001:db8::1", &[]), "ts3server://2001:db8::1");
		assert_eq!(invite_link("h:x", &["Ä/b"]), "ts3server://h:x?channel=%C3%84%2Fb");
	}

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
