//! The server rail, the server dialog, and joining a channel.

use slint::{Color, SharedString};
use voelin_core::{ObserveState, SessionState, VoiceState};
use voelin_model::{ChannelId, ChannelInfo, Presence, ServerLink};
use voelin_store::Bookmark;

use crate::app::{BookmarkForm, ServerItem};
use crate::vm::avatar;

/// Restore a known server icon before any session exists. A missing/evicted
/// cache file falls back to initials; the bookmark stores no filesystem path.
pub fn cached_icon(bookmark: &Bookmark, cache: &voelin_core::Cache) -> slint::Image {
	avatar::image(bookmark.server_icon_id().and_then(|id| cache.icon(id)).as_ref())
}

/// Server colours: hues around the wheel, each far from the one before it
/// (where a server goes when its own is taken). White initials read on all.
const SERVER_TINTS: [u32; 10] = [
	0x2d6bff, // blue
	0xc2379a, // magenta
	0xc08400, // gold
	0x0c8fb0, // teal
	0x9c3fd1, // purple
	0xe0652a, // orange
	0x1f9d55, // green
	0x7c4dff, // violet
	0xd63a3a, // red
	0x6e8f1c, // olive
];

/// A colour for each server, in `bookmarks` order: its name picks one, and
/// when an earlier server has it, it takes the next free one. So no two
/// share a colour until there are more servers than colours (then they
/// start over), and adding a server leaves the others' colours.
pub fn tints(bookmarks: &[Bookmark]) -> Vec<Color> {
	let n = SERVER_TINTS.len();
	let mut taken = [false; SERVER_TINTS.len()];
	bookmarks
		.iter()
		.enumerate()
		.map(|(i, bookmark)| {
			if i % n == 0 {
				taken = [false; SERVER_TINTS.len()];
			}
			let start = avatar::name_hash(&bookmark.name) as usize % n;
			let slot = (0..n).map(|k| (start + k) % n).find(|&s| !taken[s]).unwrap_or(start);
			taken[slot] = true;
			avatar::rgb(SERVER_TINTS[slot])
		})
		.collect()
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
/// reached; `icon`: the server's icon (empty: its initials) on `tint`
/// ([`tints`]).
#[allow(clippy::too_many_arguments)]
pub fn item(
	bookmark: &Bookmark,
	state: &SessionState,
	unread: i32,
	live: bool,
	detail: String,
	flavor: String,
	icon: slint::Image,
	tint: Color,
) -> ServerItem {
	ServerItem {
		id: bookmark.id as i32,
		name: bookmark.name.clone().into(),
		address: bookmark.address.clone().into(),
		status: status(state).into(),
		initials: avatar::initials(&bookmark.name).into(),
		tint,
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
	let mut link = ServerLink::new(address);
	link.channel = (!path.is_empty()).then(|| voelin_model::join_channel_path(path));
	link.to_url()
}

/// TeamSpeak's voice port, of an address without one.
const DEFAULT_PORT: u16 = 9987;

/// Whether two addresses name the same server: the host in any case, an
/// IPv6 address with or without its brackets, no port as 9987 (a port a
/// DNS record gives is not looked up).
pub fn same_address(a: &str, b: &str) -> bool {
	let key = |address: &str| {
		let link = ServerLink::new(address);
		(link.host.trim_end_matches('.').to_lowercase(), link.port.unwrap_or(DEFAULT_PORT))
	};
	let a = key(a);
	!a.0.is_empty() && a == key(b)
}

/// The saved server a link to `address` opens, and whether it has voice
/// (`in_voice`): of those at that address ([`same_address`]; two can be,
/// with other identities), the one in voice, else the one shown
/// (`current`), else the first.
pub fn link_server(
	bookmarks: &[Bookmark],
	address: &str,
	current: Option<i64>,
	in_voice: impl Fn(i64) -> bool,
) -> Option<(i64, bool)> {
	let saved: Vec<i64> =
		bookmarks.iter().filter(|b| same_address(&b.address, address)).map(|b| b.id).collect();
	if let Some(id) = saved.iter().copied().find(|id| in_voice(*id)) {
		return Some((id, true));
	}
	let shown = saved.iter().copied().find(|id| Some(*id) == current);
	shown.or_else(|| saved.first().copied()).map(|id| (id, false))
}

/// The channel a link's path names (`/<id>`, or names from the top as the
/// server has them, [`voelin_model::split_channel_path`]). Of siblings with
/// the same name, the first in the tree's order that has the rest of the
/// path.
pub fn channel_by_path(presence: &Presence, path: &str) -> Option<ChannelId> {
	fn below(presence: &Presence, parent: ChannelId, names: &[String]) -> Option<ChannelId> {
		let (name, rest) = names.split_first()?;
		let siblings = presence.channels.values().filter(|c| c.parent == parent).collect();
		voelin_model::order_siblings(siblings)
			.into_iter()
			.filter(|c| c.name == *name)
			.find_map(|c| if rest.is_empty() { Some(c.id) } else { below(presence, c.id, rest) })
	}
	match voelin_model::channel_path_id(path) {
		Some(id) => presence.channels.contains_key(&id).then_some(id),
		None => below(presence, 0, &voelin_model::split_channel_path(path)),
	}
}

/// The server dialog's form for a link, over `form`: a saved server's
/// keeps its address, name and nickname (a link never changes it), and its
/// stored password unless the link has one; a new server's takes the
/// link's address, nickname (else `default_nickname`) and password. The
/// link's channel, its password and privilege key come along, for Connect.
pub fn link_form(
	link: &ServerLink,
	mut form: BookmarkForm,
	default_nickname: &str,
) -> BookmarkForm {
	let text = |value: &Option<String>| SharedString::from(value.as_deref().unwrap_or_default());
	if form.id < 0 {
		form.address = link.address().into();
	}
	if form.id < 0 || form.nickname.is_empty() {
		form.nickname = link.nickname.as_deref().unwrap_or(default_nickname).into();
	}
	if let Some(password) = &link.password {
		form.server_password = password.into();
	}
	form.channel = text(&link.channel);
	form.channel_password = text(&link.channel_password);
	form.token = text(&link.token);
	form
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
			Color::default(),
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

	fn named(names: &[&str]) -> Vec<Bookmark> {
		names.iter().map(|n| Bookmark { name: (*n).into(), ..Default::default() }).collect()
	}

	/// The colour a name picks.
	fn own_tint(name: &str, step: usize) -> Color {
		let n = SERVER_TINTS.len();
		avatar::rgb(SERVER_TINTS[(avatar::name_hash(name) as usize + step) % n])
	}

	#[test]
	fn tints_are_distinct_up_to_the_palette_size() {
		let names: Vec<String> = (0..SERVER_TINTS.len()).map(|i| format!("Server {i}")).collect();
		let mut bookmarks = named(&names.iter().map(String::as_str).collect::<Vec<_>>());
		let distinct: std::collections::HashSet<_> =
			tints(&bookmarks).iter().map(|t| t.as_argb_encoded()).collect();
		assert_eq!(distinct.len(), SERVER_TINTS.len());
		// One more starts over with its name's colour.
		bookmarks.extend(named(&["One more"]));
		assert_eq!(tints(&bookmarks)[SERVER_TINTS.len()], own_tint("One more", 0));
		// The sample servers, two of which shared a blue.
		let sample = tints(&named(&["Nightfall Guild", "Pixel Lounge", "Dev TS3"]));
		assert!(sample[0] != sample[1] && sample[1] != sample[2] && sample[0] != sample[2]);
	}

	#[test]
	fn tints_follow_the_name_and_stay_when_a_server_is_added() {
		let names = ["Nightfall Guild", "Pixel Lounge", "Dev TS3", "Home", "Raid"];
		let before = tints(&named(&names));
		let mut more = named(&names);
		more.extend(named(&["New one"]));
		assert_eq!(tints(&more)[..names.len()], before[..]);
		// Alone, a server has its name's colour, in any case.
		assert_eq!(tints(&named(&["pixel lounge"])), [own_tint("Pixel Lounge", 0)]);
		// A second server with the name takes the next colour.
		assert_eq!(tints(&named(&["Raid", "Raid"])), [own_tint("Raid", 0), own_tint("Raid", 1)]);
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
		// Read back, they name the same server and channel.
		let Some(voelin_model::Link::Server(link)) =
			voelin_model::Link::parse(&invite_link("[2001:db8::1]:9987", &["Games", "Ä/b"]))
		else {
			panic!("not a server link");
		};
		assert!(same_address(&link.address(), "[2001:db8::1]:9987"));
		assert_eq!(voelin_model::split_channel_path(&link.channel.unwrap()), ["Games", "Ä/b"]);
	}

	#[test]
	fn same_addresses() {
		for (a, b) in [
			("ts.example.org", "TS.Example.org"),
			(" ts.example.org ", "ts.example.org:9987"),
			("ts.example.org.", "ts.example.org"),
			("[2001:db8::1]:9987", "2001:db8::1"),
			("[2001:db8::1]", "[2001:DB8::1]:9987"),
			("127.0.0.1:9988", "127.0.0.1:9988"),
		] {
			assert!(same_address(a, b), "{a} = {b}");
		}
		for (a, b) in [
			("ts.example.org", "ts.example.org:9988"),
			("ts.example.org", "example.org"),
			("[2001:db8::1]:9988", "2001:db8::1"),
			("", ""),
		] {
			assert!(!same_address(a, b), "{a} != {b}");
		}
	}

	#[test]
	fn servers_of_links() {
		let saved =
			|id, address: &str| Bookmark { id, address: address.into(), ..Default::default() };
		let bookmarks = [
			saved(1, "other.example"),
			saved(2, "ts.example.org"),
			saved(3, "TS.example.org:9987"),
			saved(4, "ts.example.org:9988"),
		];
		let voice = |ids: &'static [i64]| move |id| ids.contains(&id);
		// Not in voice: the one shown, else the first.
		assert_eq!(link_server(&bookmarks, "ts.example.org", None, voice(&[])), Some((2, false)));
		assert_eq!(
			link_server(&bookmarks, "ts.example.org", Some(3), voice(&[])),
			Some((3, false))
		);
		// In voice on one of them, also when another is shown.
		assert_eq!(
			link_server(&bookmarks, "ts.example.org:9987", Some(2), voice(&[1, 3])),
			Some((3, true))
		);
		// Voice on the server at another port does not count.
		assert_eq!(
			link_server(&bookmarks, "ts.example.org:9988", None, voice(&[2])),
			Some((4, false))
		);
		assert_eq!(link_server(&bookmarks, "new.example", Some(1), voice(&[1])), None);
	}

	#[test]
	fn channels_by_path() {
		let channel = |id, parent, order, name: &str| ChannelInfo {
			id,
			parent,
			order,
			name: name.into(),
			..Default::default()
		};
		let mut p = Presence::default();
		for c in [
			channel(1, 0, 0, "Lobby"),
			// Two of the same name, the first in the tree with the higher id.
			channel(4, 0, 1, "Games"),
			channel(3, 4, 0, "Chess"),
			channel(7, 0, 4, "AC/DC"),
			channel(2, 0, 7, "Games"),
			channel(5, 2, 0, "Go"),
			channel(6, 2, 5, "Chess"),
		] {
			p.channels.insert(c.id, c);
		}
		assert_eq!(channel_by_path(&p, "Lobby"), Some(1), "at the top");
		assert_eq!(channel_by_path(&p, "Games/Chess"), Some(3), "nested");
		// The same name: the first, unless only another has the rest.
		assert_eq!(channel_by_path(&p, "Games"), Some(4));
		assert_eq!(channel_by_path(&p, "Games/Go"), Some(5));
		assert_eq!(channel_by_path(&p, "AC\\/DC"), Some(7), "a / in a name");
		assert_eq!(channel_by_path(&p, "/6"), Some(6), "by id");
		for missing in ["Chess", "Games/Poker", "Lobby/Games", "/99", "", "lobby"] {
			assert_eq!(channel_by_path(&p, missing), None, "{missing}");
		}
	}

	#[test]
	fn forms_from_links() {
		let link = ServerLink {
			host: "ts.example.org".into(),
			port: Some(9988),
			nickname: Some("Link".into()),
			password: Some("pw".into()),
			channel: Some("Gaming/Raid Night".into()),
			channel_password: Some("raid".into()),
			token: Some("key".into()),
		};
		let bare = ServerLink::new("ts.example.org");
		// A new server: the link's address, nickname and password.
		let new = BookmarkForm { id: -1, nickname: "Me".into(), ..Default::default() };
		let f = link_form(&link, new.clone(), "Me");
		assert_eq!((f.id, f.name.as_str(), f.address.as_str()), (-1, "", "ts.example.org:9988"));
		assert_eq!((f.nickname.as_str(), f.server_password.as_str()), ("Link", "pw"));
		assert_eq!(
			(f.channel.as_str(), f.channel_password.as_str(), f.token.as_str()),
			("Gaming/Raid Night", "raid", "key")
		);
		let f = link_form(&bare, new, "Me");
		assert_eq!((f.address.as_str(), f.nickname.as_str()), ("ts.example.org", "Me"));
		assert_eq!(
			(f.server_password.as_str(), f.channel.as_str(), f.token.as_str()),
			("", "", "")
		);
		// A saved server keeps its address, name, nickname and password...
		let saved = BookmarkForm {
			id: 3,
			name: "Nightfall".into(),
			address: "TS.example.org:9988".into(),
			nickname: "Nova".into(),
			server_password: "stored".into(),
			..Default::default()
		};
		let f = link_form(&bare, saved.clone(), "Me");
		assert_eq!(
			(f.id, f.name.as_str(), f.address.as_str()),
			(3, "Nightfall", "TS.example.org:9988")
		);
		assert_eq!((f.nickname.as_str(), f.server_password.as_str()), ("Nova", "stored"));
		// ...the link's password in place of the stored one.
		let f = link_form(&link, saved, "Me");
		assert_eq!((f.nickname.as_str(), f.server_password.as_str()), ("Nova", "pw"));
		assert_eq!((f.channel.as_str(), f.token.as_str()), ("Gaming/Raid Night", "key"));
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
