//! Chat messages, from any source.
//!
//! Messages can link files in a channel's file browser: TeamSpeak clients
//! post them as `[URL=ts3file://…]name[/URL]`; [`ChatMessage::file_refs`]
//! finds them as [`FileRef`]s (for file cards) and [`FileRef::to_bbcode`]
//! writes one.

use serde::{Deserialize, Serialize};

use crate::percent;
use crate::presence::{ChannelId, ClientId};

fn is_false(b: &bool) -> bool {
	!*b
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ChatTarget {
	/// Everyone on the server.
	Server,
	Channel(ChannelId),
	/// Private chat, by the other side's unique id when known.
	Private(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
	pub target: ChatTarget,
	pub author_name: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub author_uid: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub author_id: Option<ClientId>,
	pub text: String,
	/// Unix time in milliseconds.
	pub ts_ms: i64,
	/// Carried by a gateway or query relay rather than our own connection.
	#[serde(default)]
	pub via_relay: bool,
	/// The author is blocked in our contacts (set by the engine when it
	/// reports the message; never sent over the wire).
	#[serde(default, skip_serializing_if = "is_false")]
	pub blocked: bool,
}

impl ChatMessage {
	/// The file links in the text, in order of appearance, each once.
	pub fn file_refs(&self) -> Vec<FileRef> {
		parse_file_links(&self.text)
	}
}

/// A file in a channel's file browser, as a chat message links it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileRef {
	/// The server's unique id (`serverUID`), when the link tells it: a link
	/// from another server cannot be downloaded here.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub server_uid: Option<String>,
	pub channel: ChannelId,
	/// The directory, `/` for the channel's root.
	pub path: String,
	pub name: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub size: Option<u64>,
	#[serde(default, skip_serializing_if = "is_false")]
	pub is_dir: bool,
	/// Unix seconds of the last change, when the link tells it.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub modified_s: Option<i64>,
	/// The link as it appeared.
	pub url: String,
}

/// URL schemes of file links (`ts3file`; later clients may use others of
/// the same form).
const FILE_SCHEMES: &[&str] = &["ts3file://", "tsfile://", "ts5file://", "ts6file://"];

impl FileRef {
	/// The file's path in the channel: [`Self::path`] joined with [`Self::name`].
	pub fn full_path(&self) -> String {
		let dir = self.path.trim_end_matches('/');
		format!("{dir}/{}", self.name)
	}

	/// Parse one link (`ts3file://name?serverUID=…&channel=…&path=…&filename=…&isDir=…&size=…&fileDateTime=…`).
	pub fn parse(url: &str) -> Option<Self> {
		let lower = url.get(..12).unwrap_or(url).to_ascii_lowercase();
		let scheme = FILE_SCHEMES.iter().find(|s| lower.starts_with(**s))?;
		let rest = &url[scheme.len()..];
		let (host, query) = rest.split_once('?').unwrap_or((rest, ""));
		let mut file = FileRef { url: url.to_owned(), path: "/".into(), ..Default::default() };
		let mut channel = None;
		for pair in query.split('&') {
			let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
			let value = percent::decode(value);
			match key.to_ascii_lowercase().as_str() {
				"serveruid" => file.server_uid = Some(value).filter(|v| !v.is_empty()),
				"channel" | "cid" => channel = value.parse().ok(),
				"path" => file.path = if value.is_empty() { "/".into() } else { value },
				"filename" | "name" => file.name = value,
				"isdir" => file.is_dir = matches!(value.as_str(), "1" | "true"),
				"size" => file.size = value.parse().ok(),
				"filedatetime" => file.modified_s = value.parse().ok(),
				_ => {}
			}
		}
		if file.name.is_empty() {
			file.name = percent::decode(host.trim_end_matches('/'));
		}
		file.channel = channel?;
		(!file.name.is_empty()).then_some(file)
	}

	/// The link as TeamSpeak clients post it: `[URL=ts3file://…]name[/URL]`.
	pub fn to_bbcode(&self) -> String {
		let mut url = format!("ts3file://{}?", percent::encode(&self.name));
		if let Some(uid) = &self.server_uid {
			url.push_str(&format!("serverUID={}&", percent::encode(uid)));
		}
		url.push_str(&format!(
			"channel={}&path={}&filename={}&isDir={}",
			self.channel,
			percent::encode(&self.path),
			percent::encode(&self.name),
			u8::from(self.is_dir)
		));
		if let Some(size) = self.size {
			url.push_str(&format!("&size={size}"));
		}
		if let Some(t) = self.modified_s {
			url.push_str(&format!("&fileDateTime={t}"));
		}
		format!("[URL={url}]{}[/URL]", self.name)
	}
}

/// The file links in `text` (BBCode `[URL=…]`, `[URL]…[/URL]`, Markdown
/// `[name](…)` or bare), in order, each once.
pub fn parse_file_links(text: &str) -> Vec<FileRef> {
	let lower = text.to_ascii_lowercase();
	let mut found: Vec<FileRef> = Vec::new();
	let mut at = 0;
	while at < text.len() {
		let next = FILE_SCHEMES.iter().filter_map(|s| lower[at..].find(s).map(|i| at + i)).min();
		let Some(start) = next else { break };
		let end = text[start..]
			.find(|c: char| c.is_whitespace() || matches!(c, ']' | '[' | ')' | '"' | '<' | '>'))
			.map_or(text.len(), |i| start + i);
		if let Some(file) = FileRef::parse(&text[start..end])
			&& !found.iter().any(|f| f.url == file.url)
		{
			found.push(file);
		}
		at = end.max(start + 1);
	}
	found
}

/// Format of messages a relay posts on behalf of a user.
pub fn relay_text(format: &str, nick: &str, text: &str) -> String {
	format.replace("{nick}", nick).replace("{text}", text)
}

/// Split text so each part fits TeamSpeak's message limit (1024 bytes of
/// text), breaking at character boundaries.
pub fn split_message(text: &str, max_bytes: usize) -> Vec<String> {
	let mut parts = Vec::new();
	let mut current = String::new();
	for c in text.chars() {
		if current.len() + c.len_utf8() > max_bytes {
			parts.push(std::mem::take(&mut current));
		}
		current.push(c);
	}
	if !current.is_empty() || parts.is_empty() {
		parts.push(current);
	}
	parts
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn relay_format() {
		assert_eq!(relay_text("[{nick}] {text}", "Alice", "hi"), "[Alice] hi");
	}

	#[test]
	fn splits_on_char_boundaries() {
		assert_eq!(split_message("abcdef", 4), vec!["abcd", "ef"]);
		assert_eq!(split_message("", 4), vec![""]);
		let parts = split_message("ääää", 3);
		assert!(parts.iter().all(|p| p.len() <= 3));
		assert_eq!(parts.concat(), "ääää");
	}

	#[test]
	fn parses_ts3_file_links() {
		let text = "look: [URL=ts3file://Report%20Q3.pdf?serverUID=l%2FHvD5zXg5r4%2F%2FC6Ht7J9Ts3Wmc%3D&channel=5&path=%2Fdocs&filename=Report%20Q3.pdf&isDir=0&size=12345&fileDateTime=1454598011]Report Q3.pdf[/URL] and [URL]ts3file://a.txt?channel=1&path=%2F&filename=a+b.txt[/URL]";
		let files = parse_file_links(text);
		assert_eq!(files.len(), 2);
		let f = &files[0];
		assert_eq!(f.server_uid.as_deref(), Some("l/HvD5zXg5r4//C6Ht7J9Ts3Wmc="));
		assert_eq!((f.channel, f.path.as_str(), f.name.as_str()), (5, "/docs", "Report Q3.pdf"));
		assert_eq!((f.size, f.modified_s, f.is_dir), (Some(12345), Some(1_454_598_011), false));
		assert_eq!(f.full_path(), "/docs/Report Q3.pdf");
		assert_eq!(files[1].name, "a+b.txt");
		assert_eq!(files[1].full_path(), "/a+b.txt");
	}

	#[test]
	fn file_links_in_other_forms() {
		// Markdown, bare, a repeated link, a link without a channel.
		let text = "[x](ts3file://x?channel=2&filename=x&isDir=true) ts3file://y?channel=3&path=/d/ \
			ts3file://y?channel=3&path=/d/ ts3file://z?filename=z";
		let files = parse_file_links(text);
		assert_eq!(files.len(), 2);
		assert!(files[0].is_dir);
		assert_eq!((files[1].name.as_str(), files[1].full_path().as_str()), ("y", "/d/y"));
		let msg = ChatMessage {
			target: ChatTarget::Channel(1),
			author_name: "a".into(),
			author_uid: None,
			author_id: None,
			text: text.into(),
			ts_ms: 0,
			via_relay: false,
			blocked: false,
		};
		assert_eq!(msg.file_refs(), files);
		assert!(parse_file_links("no links, ts3file:// alone").is_empty());
	}

	#[test]
	fn file_link_round_trip() {
		let file = FileRef {
			server_uid: Some("uid+/=".into()),
			channel: 7,
			path: "/a dir".into(),
			name: "ä b&c.png".into(),
			size: Some(10),
			is_dir: false,
			modified_s: Some(99),
			url: String::new(),
		};
		let text = file.to_bbcode();
		let parsed = parse_file_links(&text);
		assert_eq!(parsed.len(), 1);
		assert_eq!(FileRef { url: String::new(), ..parsed[0].clone() }, file);
		assert!(text.ends_with("]ä b&c.png[/URL]"));
	}

	#[test]
	fn blocked_flag_stays_off_the_wire() {
		let msg = ChatMessage {
			target: ChatTarget::Server,
			author_name: "a".into(),
			author_uid: None,
			author_id: None,
			text: "t".into(),
			ts_ms: 1,
			via_relay: false,
			blocked: false,
		};
		let json = serde_json::to_string(&msg).unwrap();
		assert!(!json.contains("blocked"));
		let back: ChatMessage = serde_json::from_str(&json).unwrap();
		assert_eq!(back, msg);
	}

	#[test]
	fn target_json() {
		assert_eq!(
			serde_json::to_string(&ChatTarget::Channel(5)).unwrap(),
			r#"{"kind":"channel","id":5}"#
		);
		assert_eq!(serde_json::to_string(&ChatTarget::Server).unwrap(), r#"{"kind":"server"}"#);
	}
}
