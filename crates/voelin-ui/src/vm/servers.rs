//! The server rail, the server dialog, and joining a channel.

use slint::SharedString;
use voelin_core::{ObserveState, SessionState, VoiceState};
use voelin_model::{ChannelId, ChannelInfo};
use voelin_store::Bookmark;

use crate::app::{BookmarkForm, ServerItem};
use crate::vm::avatar;

/// Restore a known server icon before any session exists. A missing/evicted
/// cache file falls back to initials; the bookmark stores no filesystem path.
pub fn cached_icon(bookmark: &Bookmark, cache: &voelin_core::Cache) -> slint::Image {
	avatar::image(bookmark.server_icon_id().and_then(|id| cache.icon(id)).as_ref())
}

/// "offline", "connecting", "observing" or "connected". "connecting" is
/// voice only: observing that is not there yet (a gateway that is away and
/// tried again) is "offline", as users never hear of the gateway.
pub fn status(state: &SessionState) -> &'static str {
	match (state.voice, state.observe) {
		(VoiceState::Connected, _) => "connected",
		(VoiceState::Connecting, _) => "connecting",
		(_, ObserveState::Observing) => "observing",
		_ => "offline",
	}
}

/// A server of the rail and the home page. `unread`: messages in its
/// chats; `live`: streams on it; `detail`: who is there ("9 online · 7
/// channels"), empty for its address; `flavor`: "TeamSpeak 6" once
/// reached; `icon`: the server's icon (empty: its initials).
pub fn item(
	bookmark: &Bookmark,
	state: &SessionState,
	unread: i32,
	live: bool,
	detail: String,
	flavor: String,
	icon: slint::Image,
) -> ServerItem {
	ServerItem {
		id: bookmark.id as i32,
		name: bookmark.name.clone().into(),
		address: bookmark.address.clone().into(),
		status: status(state).into(),
		initials: avatar::initials(&bookmark.name).into(),
		tint: avatar::tint(&bookmark.name),
		icon,
		unread,
		live,
		detail: if detail.is_empty() { bookmark.address.clone() } else { detail }.into(),
		flavor: flavor.into(),
		gateway: bookmark.gateway_url.is_some() || bookmark.query.is_some(),
	}
}

/// The server dialog's form for `bookmark`, with its stored password:
/// `connected` while `state` has voice, which hides Connect. A server still
/// named by its address shows no name, so a new address names it again.
pub fn form(
	bookmark: &Bookmark,
	state: Option<&SessionState>,
	server_password: &str,
) -> BookmarkForm {
	BookmarkForm {
		id: bookmark.id as i32,
		name: if bookmark.name == bookmark.address {
			SharedString::new()
		} else {
			bookmark.name.clone().into()
		},
		address: bookmark.address.clone().into(),
		nickname: bookmark.nickname.clone().into(),
		server_password: server_password.into(),
		connected: state.is_some_and(|s| s.voice == VoiceState::Connected),
		..Default::default()
	}
}

/// Where the server dialog's Connect goes ([`connect_to`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectTo {
	/// A channel path, or `/<id>`.
	pub channel: Option<String>,
	pub channel_password: Option<String>,
	/// A privilege key.
	pub token: Option<String>,
}

/// What the server dialog's Connect connects with, once the form was saved
/// as `saved` (none: saving failed, and nothing connects): the form's
/// channel (a link's) with its password, else the server's default
/// channel; and the link's privilege key.
pub fn connect_to(form: &BookmarkForm, saved: Option<&Bookmark>) -> Option<ConnectTo> {
	let saved = saved?;
	let given = |text: &SharedString| Some(text.to_string()).filter(|t| !t.is_empty());
	let channel = given(&form.channel);
	Some(ConnectTo {
		channel_password: channel.as_ref().and_then(|_| given(&form.channel_password)),
		channel: channel.or_else(|| saved.default_channel.clone()),
		token: given(&form.token),
	})
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

/// What joining a channel takes ([`join_password`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinStep {
	/// Move there, with this password.
	Send(Option<String>),
	/// Ask for the channel's password first.
	Ask,
}

/// How to join `channel`, given the password remembered for it this
/// connection: a locked channel asks for one unless it was given before,
/// as TeamSpeak does. A password given before goes along even when the
/// channel shows no lock: one locked since then still lets us in.
pub fn join_password(channel: &ChannelInfo, remembered: Option<&str>) -> JoinStep {
	match remembered.filter(|p| !p.is_empty()) {
		Some(password) => JoinStep::Send(Some(password.to_owned())),
		None if channel.has_password => JoinStep::Ask,
		None => JoinStep::Send(None),
	}
}

/// Where a voice connection asked into a channel stands ([`after_connect`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterConnect {
	/// Not known yet.
	Wait,
	/// In it.
	Joined,
	/// Put elsewhere without a word (a wrong password, for one): joining
	/// it again says why.
	JoinAgain,
}

/// A connection asked into `asked`, connected with voice: `own` is our
/// channel once known, `known` whether `asked` shows in the presence yet
/// (it can come after our own channel).
pub fn after_connect(own: Option<ChannelId>, asked: ChannelId, known: bool) -> AfterConnect {
	match own {
		Some(own) if own == asked => AfterConnect::Joined,
		Some(_) if known => AfterConnect::JoinAgain,
		_ => AfterConnect::Wait,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn server_icon_is_visible_before_connection_and_missing_files_fall_back() {
		let dir =
			std::env::temp_dir().join(format!("voelin-server-icon-model-{}", std::process::id()));
		std::fs::create_dir_all(dir.join("icons")).unwrap();
		let path = dir.join("icons/1234");
		let mut bytes = Vec::new();
		let mut encoder = png::Encoder::new(&mut bytes, 4, 4);
		encoder.set_color(png::ColorType::Rgba);
		encoder.set_depth(png::BitDepth::Eight);
		encoder.write_header().unwrap().write_image_data(&[200; 64]).unwrap();
		std::fs::write(&path, bytes).unwrap();
		let cache = voelin_core::Cache::new(&dir);
		let mut bookmark = Bookmark { address: "example.test".into(), ..Default::default() };
		bookmark.remember_server_icon(1234);
		let server = item(
			&bookmark,
			&SessionState::default(),
			0,
			false,
			String::new(),
			String::new(),
			cached_icon(&bookmark, &cache),
		);
		assert_eq!(server.status.as_str(), "offline");
		assert_eq!(server.icon.size().width, 4);
		bookmark.address = "different.test".into();
		assert_eq!(cached_icon(&bookmark, &cache).size().width, 0);
		bookmark.address = "example.test".into();
		std::fs::remove_file(path).unwrap();
		assert_eq!(cached_icon(&bookmark, &cache).size().width, 0);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn connect_shows_only_without_voice() {
		let b = Bookmark {
			id: 3,
			name: "ts.example.test".into(),
			address: "ts.example.test".into(),
			nickname: "Nova".into(),
			..Default::default()
		};
		let mut state = SessionState::default();
		let f = form(&b, None, "pw");
		assert_eq!((f.id, f.name.as_str(), f.address.as_str()), (3, "", "ts.example.test"));
		assert_eq!((f.nickname.as_str(), f.server_password.as_str()), ("Nova", "pw"));
		assert!(!f.connected);
		for (voice, connected) in [
			(VoiceState::Disconnected, false),
			// Connect again with another address.
			(VoiceState::Connecting, false),
			(VoiceState::Connected, true),
		] {
			state.voice = voice;
			assert_eq!(form(&b, Some(&state), "").connected, connected, "{voice:?}");
		}
		// Observing is not voice.
		state.voice = VoiceState::Disconnected;
		state.observe = ObserveState::Observing;
		assert!(!form(&b, Some(&state), "").connected);
		let named = Bookmark { name: "Nightfall".into(), ..b };
		assert_eq!(form(&named, None, "").name.as_str(), "Nightfall");
	}

	#[test]
	fn connect_only_what_was_saved() {
		let saved = Bookmark { id: 3, default_channel: Some("Lobby".into()), ..Default::default() };
		let plain = BookmarkForm { address: "ts.example.test".into(), ..Default::default() };
		// Saving failed: nothing connects (not the server shown before).
		assert_eq!(connect_to(&plain, None), None);
		assert_eq!(
			connect_to(&plain, Some(&saved)),
			Some(ConnectTo { channel: Some("Lobby".into()), ..Default::default() })
		);
		// A link's channel, its password and privilege key.
		let link = BookmarkForm {
			channel: "Gaming/Raid Night".into(),
			channel_password: "raid".into(),
			token: "key".into(),
			..plain.clone()
		};
		assert_eq!(connect_to(&link, None), None);
		assert_eq!(
			connect_to(&link, Some(&saved)),
			Some(ConnectTo {
				channel: Some("Gaming/Raid Night".into()),
				channel_password: Some("raid".into()),
				token: Some("key".into()),
			})
		);
		// A password goes only with the link's channel.
		let token = BookmarkForm { channel_password: "raid".into(), token: "key".into(), ..plain };
		assert_eq!(
			connect_to(&token, Some(&saved)),
			Some(ConnectTo {
				channel: Some("Lobby".into()),
				channel_password: None,
				token: Some("key".into()),
			})
		);
	}

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
	fn locked_channels_ask_for_their_password_once() {
		let open = ChannelInfo { id: 1, name: "Lobby".into(), ..Default::default() };
		let locked = ChannelInfo { has_password: true, ..open.clone() };
		assert_eq!(join_password(&open, None), JoinStep::Send(None));
		assert_eq!(join_password(&locked, None), JoinStep::Ask);
		assert_eq!(join_password(&locked, Some("")), JoinStep::Ask);
		assert_eq!(join_password(&locked, Some("pw")), JoinStep::Send(Some("pw".into())));
		assert_eq!(join_password(&open, Some("pw")), JoinStep::Send(Some("pw".into())));
	}

	#[test]
	fn connecting_into_a_channel() {
		assert_eq!(after_connect(None, 4, true), AfterConnect::Wait);
		assert_eq!(after_connect(Some(4), 4, false), AfterConnect::Joined);
		// Put into the default channel: once the channel is known, again.
		assert_eq!(after_connect(Some(1), 4, false), AfterConnect::Wait);
		assert_eq!(after_connect(Some(1), 4, true), AfterConnect::JoinAgain);
	}

	#[test]
	fn states() {
		let mut s = SessionState::default();
		assert_eq!(status(&s), "offline");
		s.observe = ObserveState::Connecting;
		assert_eq!(status(&s), "offline");
		s.observe = ObserveState::Observing;
		assert_eq!(status(&s), "observing");
		s.voice = VoiceState::Connecting;
		assert_eq!(status(&s), "connecting");
		s.voice = VoiceState::Connected;
		assert_eq!(status(&s), "connected");
	}
}
