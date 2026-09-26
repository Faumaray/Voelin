//! Chat messages, from any source.

use serde::{Deserialize, Serialize};

use crate::presence::{ChannelId, ClientId};

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
	fn target_json() {
		assert_eq!(
			serde_json::to_string(&ChatTarget::Channel(5)).unwrap(),
			r#"{"kind":"channel","id":5}"#
		);
		assert_eq!(serde_json::to_string(&ChatTarget::Server).unwrap(), r#"{"kind":"server"}"#);
	}
}
