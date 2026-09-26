//! Chat routing and duplicate suppression.

use std::collections::VecDeque;

use voelin_model::{ChannelId, ChatMessage, ChatTarget};

/// Which source carries a chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatRoute {
	Voice,
	Gateway,
	Query,
	Unavailable(&'static str),
}

/// Pick the source for reading/writing `target`.
///
/// The voice connection can only use the channel chat of its own channel;
/// other channels need a relay (gateway first, then own query credentials).
pub fn route_chat(
	target: &ChatTarget,
	voice_channel: Option<ChannelId>,
	gateway: bool,
	query: bool,
) -> ChatRoute {
	match target {
		ChatTarget::Server | ChatTarget::Private(_) if voice_channel.is_some() => ChatRoute::Voice,
		ChatTarget::Channel(cid) if voice_channel == Some(*cid) => ChatRoute::Voice,
		ChatTarget::Private(_) => ChatRoute::Unavailable("private chat needs a voice connection"),
		_ if gateway => ChatRoute::Gateway,
		_ if query => ChatRoute::Query,
		ChatTarget::Channel(_) => ChatRoute::Unavailable(
			"join the channel, or use a gateway or query credentials to chat without joining",
		),
		ChatTarget::Server => ChatRoute::Unavailable("not connected"),
	}
}

/// Drops the second copy of a message that arrives through two sources
/// (e.g. voice and a gateway relay during a switch-over).
#[derive(Default)]
pub struct Dedup {
	recent: VecDeque<(ChatTarget, String, String, i64)>,
}

impl Dedup {
	const WINDOW_MS: i64 = 2000;

	/// `true` if an equal message was seen within two seconds.
	pub fn is_duplicate(&mut self, msg: &ChatMessage) -> bool {
		while self.recent.front().is_some_and(|m| msg.ts_ms - m.3 > Self::WINDOW_MS * 2) {
			self.recent.pop_front();
		}
		// Relays post as "[nick] text"; compare the plain text.
		let text = strip_relay_prefix(&msg.text).to_string();
		let dup = self.recent.iter().any(|(t, a, x, ts)| {
			*t == msg.target
				&& *a == msg.author_name
				&& *x == text
				&& (msg.ts_ms - ts).abs() <= Self::WINDOW_MS
		});
		if !dup {
			self.recent.push_back((msg.target.clone(), msg.author_name.clone(), text, msg.ts_ms));
			if self.recent.len() > 256 {
				self.recent.pop_front();
			}
		}
		dup
	}
}

fn strip_relay_prefix(text: &str) -> &str {
	if let Some(rest) = text.strip_prefix('[')
		&& let Some((_, after)) = rest.split_once("] ")
	{
		return after;
	}
	text
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn routes() {
		let ch = ChatTarget::Channel;
		assert_eq!(route_chat(&ch(1), Some(1), true, true), ChatRoute::Voice);
		assert_eq!(route_chat(&ch(2), Some(1), true, true), ChatRoute::Gateway);
		assert_eq!(route_chat(&ch(2), Some(1), false, true), ChatRoute::Query);
		assert!(matches!(route_chat(&ch(2), Some(1), false, false), ChatRoute::Unavailable(_)));
		assert_eq!(route_chat(&ChatTarget::Server, Some(1), true, false), ChatRoute::Voice);
		assert_eq!(route_chat(&ChatTarget::Server, None, true, false), ChatRoute::Gateway);
		assert!(matches!(
			route_chat(&ChatTarget::Private("u".into()), None, true, true),
			ChatRoute::Unavailable(_)
		));
	}

	fn msg(author: &str, text: &str, ts_ms: i64) -> ChatMessage {
		ChatMessage {
			target: ChatTarget::Channel(1),
			author_name: author.into(),
			author_uid: None,
			author_id: None,
			text: text.into(),
			ts_ms,
			via_relay: false,
		}
	}

	#[test]
	fn dedup_window() {
		let mut d = Dedup::default();
		assert!(!d.is_duplicate(&msg("a", "hi", 1000)));
		assert!(d.is_duplicate(&msg("a", "hi", 1500)));
		assert!(d.is_duplicate(&msg("a", "[a] hi", 1600)));
		assert!(!d.is_duplicate(&msg("b", "hi", 1600)));
		assert!(!d.is_duplicate(&msg("a", "hi", 9000)));
	}
}
