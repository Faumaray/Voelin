//! Texts of the home, friends, messages, bell and events screens: times
//! ("12m ago", "Yesterday"), message previews without BBCode (read by
//! `vm::bbcode`), mentions, event dates, a poke's message, the private
//! chats of the home sidebar; pure, so they are tested here.

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};

fn local(ts_ms: i64) -> Option<NaiveDateTime> {
	DateTime::from_timestamp_millis(ts_ms).map(|t| t.with_timezone(&Local).naive_local())
}

fn now_local() -> NaiveDateTime {
	Local::now().naive_local()
}

/// "now", "5m ago", "2h ago", "3d ago", else the date ("12 Mar").
pub fn ago(ts_ms: i64) -> String {
	ago_at(ts_ms, chrono::Utc::now().timestamp_millis())
}

pub fn ago_at(ts_ms: i64, now_ms: i64) -> String {
	if ts_ms <= 0 {
		return String::new();
	}
	let minutes = (now_ms - ts_ms) / 60_000;
	match minutes {
		i64::MIN..=0 => "now".into(),
		1..=59 => format!("{minutes}m ago"),
		60..=1439 => format!("{}h ago", minutes / 60),
		1440..=10079 => format!("{}d ago", minutes / 1440),
		_ => local(ts_ms).map(|t| t.format("%-d %b").to_string()).unwrap_or_default(),
	}
}

/// The time of a conversation in a list: "11:42" today, "Yesterday", the
/// weekday within a week ("Mon"), else the date ("12 Mar", with the year
/// when it is another).
pub fn list_time(ts_ms: i64) -> String {
	local(ts_ms).map(|t| list_time_at(t, now_local())).unwrap_or_default()
}

pub fn list_time_at(then: NaiveDateTime, now: NaiveDateTime) -> String {
	use chrono::Datelike;
	let days = (now.date() - then.date()).num_days();
	match days {
		i64::MIN..=0 => then.format("%H:%M").to_string(),
		1 => "Yesterday".into(),
		2..=6 => then.format("%a").to_string(),
		_ if then.year() == now.year() => then.format("%-d %b").to_string(),
		_ => then.format("%-d %b %Y").to_string(),
	}
}

/// Chat text without BBCode (`[b]`, `[URL=…]`, `[/URL]`), on one line.
pub fn plain(text: &str) -> String {
	without_emoji(&crate::vm::bbcode::parse(text).one_line())
}

/// Text without emoji, runs of spaces as one: one line of text draws no
/// emoji (they are pictures in the chat).
pub fn without_emoji(text: &str) -> String {
	let mut words = String::with_capacity(text.len());
	for run in crate::emoji::split(text).into_iter().filter(|r| r.emoji.is_none()) {
		words.extend(run.text.chars().filter(|c| !is_emoji(*c)));
	}
	words.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Pictographs, symbols and their joiners and variation selectors.
fn is_emoji(c: char) -> bool {
	matches!(u32::from(c), 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0xFE0F | 0x200D)
}

/// A one-line preview of a message: "You: …" for our own, "Sent a
/// picture" / "Sent a file" for links without text.
pub fn preview(text: &str, mine: bool, files: &[voelin_model::FileRef]) -> String {
	let body = if files.is_empty() { plain(text) } else { String::new() };
	let body = if body.is_empty() && !files.is_empty() {
		if files.iter().any(|f| crate::vm::chat::is_picture(&f.name)) {
			"Sent a picture".to_owned()
		} else {
			"Sent a file".to_owned()
		}
	} else {
		body
	};
	if mine { format!("You: {body}") } else { body }
}

/// Whether `text` names `nick` as a word (case does not matter): a mention.
pub fn mentions(text: &str, nick: &str) -> bool {
	let nick = nick.trim().to_lowercase();
	if nick.is_empty() {
		return false;
	}
	let text = text.to_lowercase();
	text.match_indices(&nick).any(|(at, _)| {
		let before = text[..at].chars().next_back();
		let after = text[at + nick.len()..].chars().next();
		!before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
	})
}

/// Case-insensitive search in any of `fields`.
pub fn matches(query: &str, fields: &[&str]) -> bool {
	let q = query.trim().to_lowercase();
	q.is_empty() || fields.iter().any(|f| f.to_lowercase().contains(&q))
}

/// A poke's message as it is sent: trimmed, and cut to the
/// [`voelin_core::POKE_MESSAGE_MAX`] characters servers take.
pub fn poke_message(text: &str) -> String {
	let cut: String = text.trim().chars().take(voelin_core::POKE_MESSAGE_MAX).collect();
	cut.trim_end().to_owned()
}

/// The private chats of the home sidebar, by their index in `chats`
/// (newest first): at most `n`, those with unread messages first.
pub fn sidebar_dms(chats: &[crate::messages::ChatRef], n: usize) -> Vec<usize> {
	let mut private: Vec<usize> = (0..chats.len())
		.filter(|&i| matches!(chats[i].target, voelin_model::ChatTarget::Private(_)))
		.collect();
	private.sort_by_key(|&i| chats[i].unread == 0);
	private.truncate(n);
	private
}

/// The channels the search (Ctrl+K) finds for `query`, with their titles:
/// a spacer by its text, never a line or an empty spacer.
pub fn found_channels<'a>(
	channels: impl IntoIterator<Item = &'a voelin_model::ChannelInfo>,
	query: &str,
) -> Vec<(u64, &'a str)> {
	channels
		.into_iter()
		.filter_map(|c| Some((c.id, crate::vm::tree::listed_title(c)?)))
		.filter(|(_, title)| matches(query, &[*title]))
		.collect()
}

/// The date block of an event: "SAT", "26", "APR".
pub fn date_block(ts_ms: i64) -> (String, String, String) {
	local(ts_ms)
		.map(|t| {
			(
				t.format("%a").to_string().to_uppercase(),
				t.format("%-d").to_string(),
				t.format("%b").to_string().to_uppercase(),
			)
		})
		.unwrap_or_default()
}

/// "Sat 26 Apr, 19:00 – 21:00" (the end's date too when it is another day).
pub fn event_when(start_ms: i64, end_ms: Option<i64>) -> String {
	let Some(start) = local(start_ms) else { return String::new() };
	let mut out = start.format("%a %-d %b, %H:%M").to_string();
	if let Some(end) = end_ms.and_then(local) {
		let end = if end.date() == start.date() {
			end.format("%H:%M")
		} else {
			end.format("%a %-d %b, %H:%M")
		};
		out.push_str(&format!(" – {end}"));
	}
	out
}

/// "Starts in 15 min", "Starts in 3 h", "Happening now", or nothing (more
/// than a day away, or over).
pub fn soon(start_ms: i64, end_ms: Option<i64>, now_ms: i64) -> String {
	let minutes = (start_ms - now_ms) / 60_000;
	let over = end_ms.map_or(now_ms - start_ms > 3 * 3_600_000, |end| now_ms > end);
	match minutes {
		_ if over => String::new(),
		i64::MIN..=0 => "Happening now".into(),
		1..=59 => format!("Starts in {minutes} min"),
		60..=1439 => format!("Starts in {} h", minutes / 60),
		_ => String::new(),
	}
}

/// `2026-10-04 19:00` in local time → Unix milliseconds.
pub fn parse_local(text: &str) -> Option<i64> {
	let t = NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M").ok()?;
	Local.from_local_datetime(&t).earliest().map(|t| t.timestamp_millis())
}

/// Unix milliseconds → `2026-10-04 19:00` in local time.
pub fn format_local(ts_ms: i64) -> String {
	local(ts_ms).map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn at(text: &str) -> NaiveDateTime {
		NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
	}

	#[test]
	fn times() {
		let now = 1_800_000_000_000;
		assert_eq!(ago_at(now - 30_000, now), "now");
		assert_eq!(ago_at(now - 12 * 60_000, now), "12m ago");
		assert_eq!(ago_at(now - 2 * 3_600_000, now), "2h ago");
		assert_eq!(ago_at(now - 3 * 86_400_000, now), "3d ago");
		assert_eq!(ago_at(0, now), "");
		let now = at("2026-10-03 14:00");
		assert_eq!(list_time_at(at("2026-10-03 11:42"), now), "11:42");
		assert_eq!(list_time_at(at("2026-10-02 23:00"), now), "Yesterday");
		assert_eq!(list_time_at(at("2026-09-28 10:00"), now), "Mon");
		assert_eq!(list_time_at(at("2026-03-12 10:00"), now), "12 Mar");
		assert_eq!(list_time_at(at("2025-03-12 10:00"), now), "12 Mar 2025");
	}

	#[test]
	fn previews() {
		assert_eq!(plain("[b]Hi[/b]  there"), "Hi there");
		assert_eq!(plain("see [URL=https://x.org]this[/URL]"), "see this");
		assert_eq!(plain("a [not a tag here] b"), "a [not a tag here] b");
		assert_eq!(plain("[1] item"), "[1] item");
		assert_eq!(plain("later? 👀 Posting ❤️"), "later? Posting");
		// Only known tags go.
		assert_eq!(plain("[nick] hi"), "[nick] hi");
		assert_eq!(plain("[quote=Mira]raid?[/quote]\nyes [i]now[/i]"), "raid? yes now");
		assert_eq!(preview("Hello", true, &[]), "You: Hello");
		let file = |name: &str| voelin_model::FileRef { name: name.into(), ..Default::default() };
		assert_eq!(
			preview("[URL=ts3file://x]a.png[/URL]", false, &[file("a.png")]),
			"Sent a picture"
		);
		assert_eq!(preview("x", true, &[file("notes.txt")]), "You: Sent a file");
	}

	#[test]
	fn mention_words() {
		assert!(mentions("hey Nova, raid?", "nova"));
		assert!(mentions("@Nova", "Nova"));
		assert!(!mentions("supernova", "Nova"));
		assert!(!mentions("anything", ""));
		assert!(matches("kai", &["Kairo", "x"]));
		assert!(matches("", &[]));
		assert!(!matches("zz", &["Kairo"]));
	}

	#[test]
	fn poke_messages() {
		assert_eq!(poke_message("  Raid in 5?  "), "Raid in 5?");
		assert_eq!(poke_message(""), "");
		assert_eq!(poke_message(" \n "), "");
		let exact = "x".repeat(100);
		assert_eq!(poke_message(&exact), exact);
		// Characters, not bytes: two bytes each.
		assert_eq!(poke_message(&"ä".repeat(150)), "ä".repeat(100));
		assert_eq!(poke_message(&format!("{}€!", "ü".repeat(99))), format!("{}€", "ü".repeat(99)));
		// A cut that ends in a space leaves it out.
		assert_eq!(poke_message(&format!("{} more", "a".repeat(99))), "a".repeat(99));
	}

	/// The search shows spacers by their text and leaves out lines.
	#[test]
	fn search_channels() {
		let channel = |id, parent, name: &str| voelin_model::ChannelInfo {
			id,
			parent,
			name: name.into(),
			..Default::default()
		};
		let channels = [
			channel(1, 0, "[cspacer0]Gaming"),
			channel(2, 0, "[*spacer]---"),
			channel(3, 0, "[spacer1]"),
			// Only top-level channels are spacers.
			channel(4, 1, "[cspacer]Game Night"),
			channel(5, 0, "Lobby"),
		];
		assert_eq!(found_channels(&channels, "gam"), [(1, "Gaming"), (4, "[cspacer]Game Night")]);
		assert!(found_channels(&channels, "-").is_empty());
		// A spacer is not found by its prefix; a sub-channel's name is all
		// its own.
		assert_eq!(found_channels(&channels, "spacer"), [(4, "[cspacer]Game Night")]);
		assert_eq!(found_channels(&channels, "").len(), 3);
	}

	#[test]
	fn sidebar_chats() {
		use voelin_model::ChatTarget;
		let chat = |target: ChatTarget, unread| crate::messages::ChatRef {
			session: Some(1),
			server_uid: None,
			target,
			last: None,
			unread,
		};
		let dm = |uid: &str, unread| chat(ChatTarget::Private(uid.into()), unread);
		let chats = [
			dm("a", 0),
			chat(ChatTarget::Server, 4),
			dm("b", 2),
			chat(ChatTarget::Channel(3), 1),
			dm("c", 0),
			dm("d", 1),
			dm("e", 0),
			dm("f", 0),
		];
		// Private chats only, unread first, otherwise newest first; capped.
		assert_eq!(sidebar_dms(&chats, 5), [2, 5, 0, 4, 6]);
		assert_eq!(sidebar_dms(&chats, 1), [2]);
		assert!(sidebar_dms(&chats[1..2], 5).is_empty());
	}

	#[test]
	fn event_dates() {
		let start = parse_local("2026-04-25 19:00").unwrap();
		assert_eq!(format_local(start), "2026-04-25 19:00");
		assert_eq!(date_block(start), ("SAT".into(), "25".into(), "APR".into()));
		let end = parse_local("2026-04-25 21:30").unwrap();
		assert_eq!(event_when(start, Some(end)), "Sat 25 Apr, 19:00 – 21:30");
		assert_eq!(event_when(start, None), "Sat 25 Apr, 19:00");
		assert!(parse_local("tomorrow").is_none());
		assert_eq!(soon(start, Some(end), start - 15 * 60_000), "Starts in 15 min");
		assert_eq!(soon(start, Some(end), start - 3 * 3_600_000), "Starts in 3 h");
		assert_eq!(soon(start, Some(end), start + 60_000), "Happening now");
		assert_eq!(soon(start, Some(end), end + 60_000), "");
		assert_eq!(soon(start, None, start - 2 * 86_400_000), "");
	}
}
