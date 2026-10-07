//! Server links: `ts3server://` (TeamSpeak 3 and 5), `teamspeak://`
//! (TeamSpeak 6) and TeamSpeak's short links on `tmspk.gg`, whose page sends
//! the browser on to a `teamspeak://` link.

use crate::percent;
use crate::presence::ChannelId;

/// What a link opens ([`Link::parse`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
	/// A server, and maybe a channel on it.
	Server(ServerLink),
	/// A TeamSpeak 6 invite code (`tmspk.gg/<code>`,
	/// `teamspeak://invite=<code>`): only a TeamSpeak service knows the
	/// server it stands for.
	Invite(String),
}

/// A server link: the server, and what to join it with (none of it empty).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerLink {
	/// A host name, an IP address (IPv6 without brackets) or a server
	/// nickname.
	pub host: String,
	pub port: Option<u16>,
	pub nickname: Option<String>,
	/// The server's password.
	pub password: Option<String>,
	/// The channel to join: its names from the top, separated by `/` (a `/`
	/// in a name as `\/`, [`split_channel_path`]), or `/<id>` (`cid=`).
	pub channel: Option<String>,
	pub channel_password: Option<String>,
	/// A privilege key: it puts its user in a server or channel group.
	pub token: Option<String>,
}

const SCHEMES: [&str; 2] = ["ts3server://", "teamspeak://"];
const WEB: [&str; 2] = ["https://", "http://"];
const SHORT_HOSTS: [&str; 2] = ["tmspk.gg", "www.tmspk.gg"];

impl Link {
	/// Read a link: `ts3server://<host>?<query>` with the keys `port`,
	/// `nickname`, `password`, `channel` (`A/B`), `cid`, `channelpassword`
	/// and `token` (`teamspeak://` alike; the scheme and the keys in any
	/// case, other keys ignored, a `+` kept as it is),
	/// `https://tmspk.gg/s/<host>?<query>` (also `/s=<host>`), and the
	/// invite codes `tmspk.gg/<code>` and `teamspeak://invite=<code>`. None
	/// for anything else.
	pub fn parse(text: &str) -> Option<Link> {
		let text = text.trim();
		// The same length in bytes: an index of one is one of the other.
		let lower = text.to_ascii_lowercase();
		match SCHEMES.iter().find(|s| lower.starts_with(**s)) {
			Some(scheme) => parse_server(&text[scheme.len()..]),
			None => parse_short(text, &lower),
		}
	}
}

impl ServerLink {
	/// A link to the server at `address` (`host`, `host:port`, `[v6]`,
	/// `[v6]:port` or a bare IPv6 address), joining nothing.
	pub fn new(address: &str) -> Self {
		let (host, port) = split_address(address.trim());
		ServerLink { host: host.to_owned(), port, ..Default::default() }
	}

	/// The server's address as a bookmark keeps it: `host`, `host:port` or
	/// `[v6]:port`.
	pub fn address(&self) -> String {
		match self.port {
			Some(port) if self.host.contains(':') => format!("[{}]:{port}", self.host),
			Some(port) => format!("{}:{port}", self.host),
			None => self.host.clone(),
		}
	}

	/// The link as TeamSpeak clients open it: `ts3server://<host>?port=…`,
	/// then what is set of nickname, password, channel (or `cid` for
	/// `/<id>`), channel password and privilege key.
	pub fn to_url(&self) -> String {
		let mut url = match self.port {
			Some(_) if self.host.contains(':') => format!("ts3server://[{}]", self.host),
			_ => format!("ts3server://{}", self.host),
		};
		let pair = |key: &str, value: &Option<String>| {
			let value = value.as_deref().filter(|v| !v.is_empty())?;
			Some(format!("{key}={}", percent::encode(value)))
		};
		let channel = self.channel.as_deref().filter(|c| !c.is_empty()).map(|path| {
			match channel_path_id(path) {
				Some(id) => format!("cid={id}"),
				// A `/` between names, `%2F` in one.
				None => {
					let names: Vec<String> =
						split_channel_path(path).iter().map(|n| percent::encode(n)).collect();
					format!("channel={}", names.join("/"))
				}
			}
		});
		let query: Vec<String> = [
			self.port.map(|port| format!("port={port}")),
			pair("nickname", &self.nickname),
			pair("password", &self.password),
			channel,
			pair("channelpassword", &self.channel_password),
			pair("token", &self.token),
		]
		.into_iter()
		.flatten()
		.collect();
		if !query.is_empty() {
			url.push('?');
			url.push_str(&query.join("&"));
		}
		url
	}
}

/// A channel path's names from the top: `Gaming/Raid Night` →
/// `["Gaming", "Raid Night"]`, `AC\/DC` → `["AC/DC"]`.
pub fn split_channel_path(path: &str) -> Vec<String> {
	let mut names = Vec::new();
	let mut name = String::new();
	let mut chars = path.chars().peekable();
	while let Some(c) = chars.next() {
		match c {
			'\\' if chars.peek() == Some(&'/') => {
				name.push('/');
				chars.next();
			}
			'/' => names.push(std::mem::take(&mut name)),
			c => name.push(c),
		}
	}
	names.push(name);
	names
}

/// A channel path from its names from the top, a `/` in a name as `\/`:
/// the reverse of [`split_channel_path`].
pub fn join_channel_path(names: &[impl AsRef<str>]) -> String {
	names.iter().map(|n| n.as_ref().replace('/', "\\/")).collect::<Vec<_>>().join("/")
}

/// The channel id of a path `/<id>` (what `cid=` gives).
pub fn channel_path_id(path: &str) -> Option<ChannelId> {
	let id = path.strip_prefix('/')?;
	if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
		return None;
	}
	id.parse().ok()
}

/// After `ts3server://` or `teamspeak://`: `<host>[:port][/]?<query>`, or
/// `invite=<code>`.
fn parse_server(rest: &str) -> Option<Link> {
	let (head, query) = rest.split_once('?').unwrap_or((rest, ""));
	let authority = head.split('/').next().unwrap_or_default();
	if let Some(key) = authority.get(..7).filter(|k| k.eq_ignore_ascii_case("invite=")) {
		let code = percent::decode(&authority[key.len()..]);
		return (!code.is_empty()).then_some(Link::Invite(code));
	}
	let authority = percent::decode(authority);
	let (host, port) = split_address(&authority);
	if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
		return None;
	}
	let mut link = ServerLink { host: host.to_owned(), port, ..Default::default() };
	let mut cid = None;
	for pair in query.split('&') {
		let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
		let value = Some(percent::decode(raw)).filter(|v| !v.is_empty());
		match key.to_ascii_lowercase().as_str() {
			// One that is no port is ignored.
			"port" => link.port = value.as_deref().and_then(parse_port).or(link.port),
			"nickname" => link.nickname = value,
			"password" => link.password = value,
			// A `/` between names, `%2F` in one.
			"channel" => {
				let names: Vec<String> = raw.split('/').map(percent::decode).collect();
				link.channel = Some(join_channel_path(&names)).filter(|_| names.concat() != "");
			}
			"cid" => cid = value.and_then(|v| v.parse::<ChannelId>().ok()).filter(|c| *c != 0),
			"channelpassword" => link.channel_password = value,
			"token" => link.token = value,
			_ => {}
		}
	}
	// The id names one channel for sure.
	if let Some(cid) = cid {
		link.channel = Some(format!("/{cid}"));
	}
	Some(Link::Server(link))
}

/// `tmspk.gg/s/<host>?…` (or `/s=`), with or without `https://`, goes where
/// its page sends the browser: `teamspeak://<host>?…`; any other path is an
/// invite code.
fn parse_short(text: &str, lower: &str) -> Option<Link> {
	let start = WEB.iter().find(|s| lower.starts_with(**s)).map_or(0, |s| s.len());
	let (host, path) = text[start..].split_once('/')?;
	if !SHORT_HOSTS.contains(&host.to_ascii_lowercase().as_str()) {
		return None;
	}
	if path.get(..2).is_some_and(|s| s.eq_ignore_ascii_case("s/") || s.eq_ignore_ascii_case("s=")) {
		return parse_server(&path[2..]);
	}
	let code = path.split(['?', '#', '/']).next().unwrap_or_default();
	let valid = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_');
	(!code.is_empty() && code.bytes().all(valid)).then(|| Link::Invite(code.to_owned()))
}

/// `host`, `host:port`, `[v6]`, `[v6]:port` or a bare IPv6 address: the
/// host (an IPv6 address without its brackets) and the port, if one is
/// given that is one.
fn split_address(address: &str) -> (&str, Option<u16>) {
	if let Some((host, after)) = address.strip_prefix('[').and_then(|a| a.split_once(']')) {
		return (host, after.strip_prefix(':').and_then(parse_port));
	}
	match address.rsplit_once(':') {
		// A port follows the only colon; more colons: an IPv6 address.
		Some((host, port))
			if !host.contains(':')
				&& !port.is_empty()
				&& port.bytes().all(|b| b.is_ascii_digit()) =>
		{
			(host, parse_port(port))
		}
		_ => (address, None),
	}
}

fn parse_port(text: &str) -> Option<u16> {
	text.parse().ok().filter(|p| *p != 0)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn server(text: &str) -> ServerLink {
		match Link::parse(text) {
			Some(Link::Server(link)) => link,
			other => panic!("{text}: {other:?}"),
		}
	}

	fn host(host: &str, port: Option<u16>) -> ServerLink {
		ServerLink { host: host.into(), port, ..Default::default() }
	}

	#[test]
	fn every_parameter() {
		let link = server(
			"ts3server://ts.example.org?port=9988&nickname=Nova%20X&password=s3cr%3Ft&channel=Gaming/Raid%20Night&channelpassword=raid+1&token=AbC%2Bd&addbookmark=Mine",
		);
		assert_eq!(
			link,
			ServerLink {
				host: "ts.example.org".into(),
				port: Some(9988),
				nickname: Some("Nova X".into()),
				password: Some("s3cr?t".into()),
				channel: Some("Gaming/Raid Night".into()),
				// A `+` stays.
				channel_password: Some("raid+1".into()),
				token: Some("AbC+d".into()),
			}
		);
		assert_eq!(link.address(), "ts.example.org:9988");
		assert_eq!(
			link.to_url(),
			"ts3server://ts.example.org?port=9988&nickname=Nova%20X&password=s3cr%3Ft&channel=Gaming/Raid%20Night&channelpassword=raid%2B1&token=AbC%2Bd"
		);
		assert_eq!(Link::parse(&link.to_url()), Some(Link::Server(link)));
		// A channel id names the channel, also next to a path.
		assert_eq!(server("ts3server://h?cid=12").channel.as_deref(), Some("/12"));
		assert_eq!(server("ts3server://h?channel=Lobby&cid=12").channel.as_deref(), Some("/12"));
		assert_eq!(server("ts3server://h?cid=x&channel=Lobby").channel.as_deref(), Some("Lobby"));
		assert_eq!(server("ts3server://h?cid=12").to_url(), "ts3server://h?cid=12");
		// Empty values are none.
		assert_eq!(server("ts3server://h?nickname=&channel=&password"), host("h", None));
	}

	#[test]
	fn teamspeak_scheme_is_the_same() {
		let query = "ts.example.org?port=9987&channel=Lobby&nickname=Nova";
		assert_eq!(
			Link::parse(&format!("teamspeak://{query}")),
			Link::parse(&format!("ts3server://{query}"))
		);
		// Any case, of the scheme and the keys; the host as written.
		let link = server("TeamSpeak://TS.Example.org/?PORT=9988&Channel=Lobby&CID=");
		assert_eq!(
			link,
			ServerLink { channel: Some("Lobby".into()), ..host("TS.Example.org", Some(9988)) }
		);
		assert_eq!(server("TS3SERVER://h:9988"), host("h", Some(9988)));
	}

	#[test]
	fn short_links() {
		for text in [
			"https://tmspk.gg/s/ts.example.org?port=9988&channel=Lobby",
			"https://tmspk.gg/s=ts.example.org?port=9988&channel=Lobby",
			"HTTPS://www.TMSPK.gg/S/ts.example.org?port=9988&channel=Lobby",
			"tmspk.gg/s/ts.example.org/?port=9988&channel=Lobby",
			"http://tmspk.gg/s/ts.example.org:9988?channel=Lobby",
		] {
			assert_eq!(
				server(text),
				ServerLink { channel: Some("Lobby".into()), ..host("ts.example.org", Some(9988)) },
				"{text}"
			);
		}
		let invite = |code: &str| Some(Link::Invite(code.into()));
		assert_eq!(Link::parse("https://tmspk.gg/AbC12x"), invite("AbC12x"));
		assert_eq!(Link::parse("tmspk.gg/AbC12x/?ref=1"), invite("AbC12x"));
		assert_eq!(Link::parse("teamspeak://invite=AbC12x"), invite("AbC12x"));
		assert_eq!(Link::parse("TEAMSPEAK://Invite=AbC12x/"), invite("AbC12x"));
		for text in ["https://tmspk.gg/", "https://tmspk.gg", "tmspk.gg/a.b", "teamspeak://invite="]
		{
			assert_eq!(Link::parse(text), None, "{text}");
		}
	}

	#[test]
	fn ipv6() {
		let v6 = host("2001:db8::1", Some(9987));
		assert_eq!(server("ts3server://[2001:db8::1]?port=9987"), v6);
		assert_eq!(server("ts3server://[2001:db8::1]:9987"), v6);
		assert_eq!(v6.address(), "[2001:db8::1]:9987");
		assert_eq!(v6.to_url(), "ts3server://[2001:db8::1]?port=9987");
		// Without a port, with or without brackets.
		let bare = host("2001:db8::1", None);
		assert_eq!(server("ts3server://2001:db8::1"), bare);
		assert_eq!(server("ts3server://[2001:db8::1]"), bare);
		assert_eq!(bare.address(), "2001:db8::1");
		assert_eq!(ServerLink::new(" [2001:db8::1]:9988 "), host("2001:db8::1", Some(9988)));
	}

	#[test]
	fn escapes() {
		let link = server("ts3server://h?channel=%C3%84rger/Caf%C3%A9&nickname=J%C3%BCrgen");
		assert_eq!(link.channel.as_deref(), Some("Ärger/Café"));
		assert_eq!(link.nickname.as_deref(), Some("Jürgen"));
		// `%2F` is a `/` in a name.
		let link = server("ts3server://h?channel=AC%2FDC/Live");
		assert_eq!(link.channel.as_deref(), Some("AC\\/DC/Live"));
		assert_eq!(split_channel_path("AC\\/DC/Live"), ["AC/DC", "Live"]);
		assert_eq!(join_channel_path(&["AC/DC", "Live"]), "AC\\/DC/Live");
		assert_eq!(link.to_url(), "ts3server://h?channel=AC%2FDC/Live");
		assert_eq!(split_channel_path("Lobby"), ["Lobby"]);
		assert_eq!(channel_path_id("/12"), Some(12));
		assert_eq!(channel_path_id("12"), None);
		assert_eq!(channel_path_id("/1a"), None);
	}

	#[test]
	fn invalid_ports_are_ignored() {
		for text in [
			"ts3server://h?port=abc",
			"ts3server://h?port=70000",
			"ts3server://h?port=0",
			"ts3server://h?port=",
			"ts3server://h:70000",
		] {
			assert_eq!(server(text), host("h", None), "{text}");
		}
		// One in the address stays.
		assert_eq!(server("ts3server://h:9988?port=x"), host("h", Some(9988)));
	}

	#[test]
	fn other_links_are_none() {
		for text in [
			"",
			"http://example.com",
			"https://example.com/s/ts.example.org",
			"ts3file://a.png?serverUID=x&channel=1",
			"ts3server://",
			"ts3server://?port=9987",
			"ts3server://a b",
			"ts.example.org",
			"mailto:a@example.org",
		] {
			assert_eq!(Link::parse(text), None, "{text:?}");
		}
	}

	#[test]
	fn round_trip() {
		// The invite links of voelin-ui (`vm::servers::invite_link`).
		for (address, names, url) in [
			(
				"ts.example.com:9988",
				&["Gaming", "Raid Night"][..],
				"ts3server://ts.example.com?port=9988&channel=Gaming/Raid%20Night",
			),
			(" ts.example.com ", &[], "ts3server://ts.example.com"),
			(
				"[2001:db8::1]:9987",
				&["Chill & Co"],
				"ts3server://[2001:db8::1]?port=9987&channel=Chill%20%26%20Co",
			),
			("2001:db8::1", &[], "ts3server://2001:db8::1"),
			("h:x", &["Ä/b"], "ts3server://h:x?channel=%C3%84%2Fb"),
		] {
			let mut link = ServerLink::new(address);
			link.channel = (!names.is_empty()).then(|| join_channel_path(names));
			assert_eq!(link.to_url(), url);
			assert_eq!(Link::parse(url), Some(Link::Server(link.clone())), "{url}");
			assert_eq!(link.address(), address.trim());
			if let Some(path) = &link.channel {
				assert_eq!(split_channel_path(path), names);
			}
		}
	}
}
