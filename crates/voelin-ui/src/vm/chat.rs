//! Chat lines.

use std::rc::Rc;

use slint::{ModelRc, VecModel};
use voelin_model::ChatMessage;

use crate::app::{ChatLine, TextRun};
use crate::emoji;
use crate::vm::avatar;

/// Lines of the same author within this time are grouped (no header).
const GROUP_MS: i64 = 5 * 60 * 1000;

/// The author and time of the line before, for grouping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Previous {
	pub author: String,
	pub relayed: bool,
	pub ts_ms: i64,
}

impl Previous {
	pub fn of(message: &ChatMessage) -> Self {
		Self {
			author: message.author_name.clone(),
			relayed: message.via_relay,
			ts_ms: message.ts_ms,
		}
	}
}

fn time_of(ts_ms: i64) -> String {
	chrono::DateTime::from_timestamp_millis(ts_ms)
		.map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
		.unwrap_or_default()
}

/// The line of a message; `previous` is the line before it in the chat.
pub fn line(message: &ChatMessage, previous: Option<&Previous>) -> ChatLine {
	let continued = previous.is_some_and(|p| {
		p.author == message.author_name
			&& p.relayed == message.via_relay
			&& (0..GROUP_MS).contains(&(message.ts_ms - p.ts_ms))
	});
	let runs = emoji::runs(&message.text);
	let jumbo = runs.as_deref().is_some_and(emoji::is_jumbo);
	let runs_model = match &runs {
		Some(runs) => ModelRc::from(Rc::new(VecModel::from(
			runs.iter()
				.map(|r| TextRun {
					text: r.text.clone().into(),
					emoji: r.emoji.clone().unwrap_or_default().into(),
				})
				.collect::<Vec<_>>(),
		))),
		None => ModelRc::default(),
	};
	ChatLine {
		author: message.author_name.clone().into(),
		text: message.text.clone().into(),
		time: time_of(message.ts_ms).into(),
		relayed: message.via_relay,
		runs: runs_model,
		rich: runs.is_some(),
		jumbo,
		continued,
		initials: avatar::initials(&message.author_name).into(),
		tint: avatar::tint(&message.author_name),
	}
}

#[cfg(test)]
mod tests {
	use slint::Model;
	use voelin_model::ChatTarget;

	use super::*;

	fn message(author: &str, text: &str, ts_ms: i64) -> ChatMessage {
		ChatMessage {
			target: ChatTarget::Server,
			author_name: author.into(),
			author_uid: None,
			author_id: None,
			text: text.into(),
			ts_ms,
			via_relay: false,
		}
	}

	#[test]
	fn grouping() {
		let first = message("Alice", "hi", 1_000_000);
		let a = line(&first, None);
		assert!(!a.continued);
		assert_eq!(a.initials, "AL");
		let prev = Previous::of(&first);
		assert!(line(&message("Alice", "again", 1_060_000), Some(&prev)).continued);
		assert!(!line(&message("Bob", "me", 1_060_000), Some(&prev)).continued);
		assert!(!line(&message("Alice", "later", 1_000_000 + GROUP_MS), Some(&prev)).continued);
	}

	#[test]
	fn emoji_runs() {
		let plain = line(&message("A", "no emoji here", 0), None);
		assert!(!plain.rich && plain.runs.row_count() == 0);
		let rich = line(&message("A", "gg 🎉", 0), None);
		assert!(rich.rich && !rich.jumbo);
		assert_eq!(rich.runs.row_count(), 2);
		assert_eq!(rich.runs.row_data(1).unwrap().emoji, "1f389");
		assert!(line(&message("A", "🎉", 0), None).jumbo);
	}
}
