//! Chat lines: one [`ChatLine`] per message, with its reactions and the
//! files it links.

use std::rc::Rc;

use slint::{Image, ModelRc, VecModel};
use voelin_core::HistoryMessage;
use voelin_model::{ChatMessage, FileRef};

use crate::app::{ChatLine, FileItem, ReactionItem, TextRun};
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

/// What a line needs besides the message itself.
#[derive(Default)]
pub struct LineCtx {
	/// The message's handle in its chat.
	pub key: i32,
	/// The author's avatar picture.
	pub avatar: Image,
	/// The gateway takes reactions and pins for this chat.
	pub gateway: bool,
	/// Highlighted (jumped to from the pins).
	pub marked: bool,
	/// The topic the message belongs to, when it should be shown.
	pub topic: String,
	/// The state of the downloads started from this message, by link index.
	pub downloads: Vec<(usize, FileItem)>,
	/// Pictures of the links shown inline, by link index.
	pub previews: Vec<(usize, Image)>,
}

/// When a message was sent, for its header (see [`stamp`]).
pub fn time_of(ts_ms: i64) -> String {
	chrono::DateTime::from_timestamp_millis(ts_ms)
		.map(|t| {
			stamp(t.with_timezone(&chrono::Local).naive_local(), chrono::Local::now().naive_local())
		})
		.unwrap_or_default()
}

/// "Today at 10:14", "Yesterday at 10:14", "12 Mar at 10:14", and the year
/// when it is not this one.
pub fn stamp(then: chrono::NaiveDateTime, now: chrono::NaiveDateTime) -> String {
	use chrono::Datelike;
	let time = then.format("%H:%M");
	let days = (now.date() - then.date()).num_days();
	match days {
		0 => format!("Today at {time}"),
		1 => format!("Yesterday at {time}"),
		_ if then.year() == now.year() => format!("{} at {time}", then.format("%-d %b")),
		_ => format!("{} at {time}", then.format("%-d %b %Y")),
	}
}

/// "2.4 MB", "912 kB", "48 B".
pub fn size_text(bytes: u64) -> String {
	const UNITS: [(u64, &str); 3] = [(1 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "kB")];
	for (unit, name) in UNITS {
		if bytes >= unit {
			return format!("{:.1} {name}", bytes as f64 / unit as f64);
		}
	}
	format!("{bytes} B")
}

/// The text without the file links that became cards.
fn without_links(text: &str, files: &[FileRef]) -> String {
	let mut out = text.to_owned();
	for file in files {
		for pattern in [
			format!("[URL={}]{}[/URL]", file.url, file.name),
			format!("[URL={}]", file.url),
			format!("[url={}]", file.url),
			file.url.clone(),
		] {
			out = out.replace(&pattern, "");
		}
	}
	out.replace("[/URL]", "").replace("[/url]", "").trim().to_owned()
}

/// A chat's last message for a list of chats, on one line ("Mira: the
/// banner I promised: guild-banner.png"), and when it came.
pub fn preview(message: &ChatMessage) -> (String, String) {
	let text = if message.blocked {
		"Message from a blocked contact.".to_owned()
	} else {
		let files = message.file_refs();
		let names = files.iter().map(|f| f.name.as_str());
		let text = without_links(&message.text, &files);
		std::iter::once(text.as_str()).chain(names).collect::<Vec<_>>().join(" ")
	};
	// A one-line text cannot show emoji (they are pictures): left out.
	let plain = match emoji::runs(&text) {
		Some(runs) => runs.into_iter().filter(|r| r.emoji.is_none()).map(|r| r.text).collect(),
		None => text,
	};
	let mut text = plain.split_whitespace().collect::<Vec<_>>().join(" ");
	if text.is_empty() {
		text = "sent an emoji".into();
	}
	(format!("{}: {text}", message.author_name), time_of(message.ts_ms))
}

fn runs_model(runs: &[emoji::Run]) -> ModelRc<TextRun> {
	ModelRc::from(Rc::new(VecModel::from(
		runs.iter()
			.map(|r| TextRun {
				text: r.text.clone().into(),
				emoji: r.emoji.clone().unwrap_or_default().into(),
			})
			.collect::<Vec<_>>(),
	)))
}

/// The line of a stored message; `previous` is the line before it.
pub fn history_line(
	message: &HistoryMessage,
	previous: Option<&Previous>,
	ctx: &LineCtx,
) -> ChatLine {
	let files = message.message.file_refs();
	let mut line = line_of(&message.message, previous, ctx, &files);
	line.pinned = message.pinned;
	line.remote = ctx.gateway && message.remote_id.is_some();
	line.reactions = ModelRc::from(Rc::new(VecModel::from(
		message
			.reactions
			.iter()
			.map(|r| ReactionItem {
				key: emoji::first_key(&r.emoji).unwrap_or_default().into(),
				text: r.emoji.clone().into(),
				count: r.count as i32,
				me: r.me,
			})
			.collect::<Vec<_>>(),
	)));
	line
}

/// The line of a message the engine keeps no history for (no ids, no
/// reactions).
pub fn line(message: &ChatMessage, previous: Option<&Previous>) -> ChatLine {
	let files = message.file_refs();
	line_of(message, previous, &LineCtx::default(), &files)
}

fn line_of(
	message: &ChatMessage,
	previous: Option<&Previous>,
	ctx: &LineCtx,
	files: &[FileRef],
) -> ChatLine {
	let continued = previous.is_some_and(|p| {
		p.author == message.author_name
			&& p.relayed == message.via_relay
			&& (0..GROUP_MS).contains(&(message.ts_ms - p.ts_ms))
	});
	let text =
		if files.is_empty() { message.text.clone() } else { without_links(&message.text, files) };
	let runs = emoji::runs(&text);
	let jumbo = runs.as_deref().is_some_and(emoji::is_jumbo);
	let cards: Vec<FileItem> = files
		.iter()
		.enumerate()
		.map(|(i, f)| {
			let mut item = FileItem {
				name: f.name.clone().into(),
				detail: f.size.map(size_text).unwrap_or_default().into(),
				index: i as i32,
				picture: is_picture(&f.name),
				..Default::default()
			};
			// A download of this link shows its progress instead of the size.
			if let Some((_, running)) = ctx.downloads.iter().find(|(index, _)| *index == i) {
				item.detail = running.detail.clone();
				item.state = running.state.clone();
				item.progress = running.progress;
			}
			if let Some((_, picture)) = ctx.previews.iter().find(|(index, _)| *index == i) {
				item.preview = picture.clone();
			}
			item
		})
		.collect();
	ChatLine {
		key: ctx.key,
		author: message.author_name.clone().into(),
		text: text.clone().into(),
		time: time_of(message.ts_ms).into(),
		relayed: message.via_relay,
		runs: runs.as_deref().map(runs_model).unwrap_or_default(),
		rich: runs.is_some(),
		jumbo,
		continued,
		initials: avatar::initials(&message.author_name).into(),
		tint: avatar::tint(&message.author_name),
		avatar: ctx.avatar.clone(),
		blocked: message.blocked,
		pinned: false,
		reactions: ModelRc::default(),
		files: ModelRc::from(Rc::new(VecModel::from(cards))),
		remote: false,
		topic: ctx.topic.clone().into(),
		marked: ctx.marked,
	}
}

pub fn is_picture(name: &str) -> bool {
	let lower = name.to_ascii_lowercase();
	[".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".svg"].iter().any(|e| lower.ends_with(e))
}

#[cfg(test)]
mod tests {
	use slint::Model;
	use voelin_core::HistoryMessage;
	use voelin_gateway_proto::ReactionCount;
	use voelin_model::ChatTarget;
	use voelin_store::MessageSource;

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
			blocked: false,
		}
	}

	fn stored(message: ChatMessage) -> HistoryMessage {
		HistoryMessage {
			id: 1,
			message,
			source: MessageSource::Voice,
			remote_id: Some(7),
			topic_id: None,
			reactions: vec![ReactionCount { emoji: "🎉".into(), count: 2, me: true }],
			pinned: true,
			rev: 1,
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

	#[test]
	fn reactions_and_pins() {
		let ctx = LineCtx { key: 4, gateway: true, ..Default::default() };
		let line = history_line(&stored(message("A", "hi", 0)), None, &ctx);
		assert!(line.pinned && line.remote && line.key == 4);
		assert_eq!(line.reactions.row_count(), 1);
		let r = line.reactions.row_data(0).unwrap();
		assert_eq!((r.key.as_str(), r.count, r.me), ("1f389", 2, true));
		// Without a gateway nothing can be pinned or reacted to.
		let plain = history_line(&stored(message("A", "hi", 0)), None, &LineCtx::default());
		assert!(!plain.remote);
	}

	#[test]
	fn file_cards_leave_the_text() {
		let url = "ts3file://plan.pdf?channel=2&path=/&filename=plan.pdf&isDir=0&size=2048";
		let text = format!("here you go [URL={url}]plan.pdf[/URL] 🎉");
		let line = line(&message("A", &text, 0), None);
		assert_eq!(line.files.row_count(), 1);
		let file = line.files.row_data(0).unwrap();
		assert_eq!((file.name.as_str(), file.detail.as_str()), ("plan.pdf", "2.0 kB"));
		assert!(!line.text.contains("ts3file"), "{}", line.text);
		assert!(line.text.starts_with("here you go"));
	}

	#[test]
	fn previews_are_one_line() {
		let url = "ts3file://plan.pdf?channel=2&path=/&filename=plan.pdf&isDir=0&size=2048";
		let text = format!("route\nfor  tonight: [URL={url}]plan.pdf[/URL]");
		let (last, time) = preview(&message("Mira", &text, 0));
		assert_eq!(last, "Mira: route for tonight: plan.pdf");
		assert!(!time.is_empty());
		assert_eq!(
			preview(&message("Ari", "co-op later? 👀 Posting", 0)).0,
			"Ari: co-op later? Posting"
		);
		assert_eq!(preview(&message("dex", "🎉🎉", 0)).0, "dex: sent an emoji");
		let mut blocked = message("X", "secret", 0);
		blocked.blocked = true;
		assert_eq!(preview(&blocked).0, "X: Message from a blocked contact.");
	}

	#[test]
	fn stamps() {
		let at = |d: &str| chrono::NaiveDateTime::parse_from_str(d, "%Y-%m-%d %H:%M").unwrap();
		let now = at("2026-10-03 09:00");
		assert_eq!(stamp(at("2026-10-03 08:05"), now), "Today at 08:05");
		assert_eq!(stamp(at("2026-10-02 23:59"), now), "Yesterday at 23:59");
		assert_eq!(stamp(at("2026-03-12 10:14"), now), "12 Mar at 10:14");
		assert_eq!(stamp(at("2025-12-31 10:14"), now), "31 Dec 2025 at 10:14");
	}

	#[test]
	fn sizes() {
		assert_eq!(size_text(48), "48 B");
		assert_eq!(size_text(2 << 20), "2.0 MB");
	}
}
