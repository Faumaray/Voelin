//! Chat lines: one [`ChatLine`] per message, with its text in blocks of
//! styled runs (`vm::bbcode`), its reactions and the files and pictures it
//! links.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use slint::{Color, Image, ModelRc, VecModel};
use unicode_segmentation::UnicodeSegmentation;
use voelin_core::HistoryMessage;
use voelin_model::{ChatMessage, ChatTarget, FileRef};

use crate::app::{ChatLine, FileItem, ReactionItem, TextBlock, TextRun};
use crate::emoji;
use crate::vm::avatar;
use crate::vm::bbcode::{self, BlockKind, Doc, Span, Style};

/// Lines of the same author within this time are grouped (no header).
const GROUP_MS: i64 = 5 * 60 * 1000;

/// The author and time of the line before, for grouping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Previous {
	pub author: String,
	pub ts_ms: i64,
}

impl Previous {
	pub fn of(message: &ChatMessage) -> Self {
		Self { author: message.author_name.clone(), ts_ms: message.ts_ms }
	}
}

/// What a line needs besides the message itself.
#[derive(Default)]
pub struct LineCtx<'a> {
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
	/// Pictures on the web in the engine's cache, by address: the `[img]`
	/// pictures here show as picture cards, the others as links (`None`:
	/// all as links).
	pub pictures: Option<&'a HashMap<String, PathBuf>>,
	/// What earlier lines of the chat were built from, to reuse.
	pub cache: Option<&'a LineCache>,
	/// The first new message: the "New" divider is above it, so it starts
	/// a group of its own.
	pub unread_start: bool,
}

/// The first message after `read` (by `(ts_ms, id)`, as a chat is ordered)
/// that is not ours: where the "New" divider goes. `messages` are
/// `(ts_ms, id, ours)` in order; `None` read: nothing was.
pub fn first_unread(messages: &[(i64, i64, bool)], read: Option<(i64, i64)>) -> Option<usize> {
	messages.iter().position(|&(ts, id, own)| !own && read.is_none_or(|r| (ts, id) > r))
}

/// How many of `messages` (as for [`first_unread`]) came after `read` and
/// are not ours.
pub fn unread_count(messages: &[(i64, i64, bool)], read: Option<(i64, i64)>) -> i32 {
	let new = |&&(ts, id, own): &&(i64, i64, bool)| !own && read.is_none_or(|r| (ts, id) > r);
	messages.iter().filter(new).count() as i32
}

/// Since when messages are new, for "5 new messages since 10:14" (see
/// [`since`]).
pub fn since_time(ts_ms: i64) -> String {
	chrono::DateTime::from_timestamp_millis(ts_ms)
		.map(|t| {
			since(t.with_timezone(&chrono::Local).naive_local(), chrono::Local::now().naive_local())
		})
		.unwrap_or_default()
}

/// "10:14" today, "12 Mar 10:14" before, with the year when it is not this
/// one.
pub fn since(then: chrono::NaiveDateTime, now: chrono::NaiveDateTime) -> String {
	use chrono::Datelike;
	if then.date() == now.date() {
		then.format("%H:%M").to_string()
	} else if then.year() == now.year() {
		then.format("%-d %b %H:%M").to_string()
	} else {
		then.format("%-d %b %Y %H:%M").to_string()
	}
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

/// What a line shows of its message's text.
#[derive(Clone, Default)]
struct Body {
	text: String,
	blocks: ModelRc<TextBlock>,
	rich: bool,
	jumbo: bool,
	link: String,
}

/// A message's text read: without the file links (cards), the files it
/// links, and the addresses of its pictures on the web.
struct Read {
	doc: Doc,
	files: Vec<FileRef>,
	images: Vec<String>,
}

/// What lines of a chat were built from, by message id, kept between
/// refreshes: a message whose text, reactions and cards did not change is
/// not read again and gets the same models (they compare by pointer), so
/// its line compares equal and `list::sync` leaves its row. (Not with an
/// empty picture: Slint's empty `Image` equals nothing, not even itself.)
#[derive(Default)]
pub struct LineCache(RefCell<HashMap<i64, Parts>>);

#[derive(Default)]
struct Parts {
	rev: i64,
	text: String,
	read: Option<Read>,
	/// The body last built, and which pictures were cards then.
	body: Option<(Vec<bool>, Body)>,
	reactions: (Vec<ReactionItem>, ModelRc<ReactionItem>),
	files: (Vec<FileItem>, ModelRc<FileItem>),
}

impl LineCache {
	/// Forget the messages not among `ids` once there are many more.
	pub fn keep(&self, ids: impl IntoIterator<Item = i64>, count: usize) {
		let mut parts = self.0.borrow_mut();
		if parts.len() > count + 64 {
			let ids: std::collections::HashSet<i64> = ids.into_iter().collect();
			parts.retain(|id, _| ids.contains(id));
		}
	}
}

/// The parts of message `id`, started again when it changed.
fn current<'a>(all: &'a mut HashMap<i64, Parts>, id: i64, rev: i64, text: &str) -> &'a mut Parts {
	let parts = all.entry(id).or_default();
	if parts.rev != rev || parts.text != text {
		*parts = Parts { rev, text: text.to_owned(), ..Parts::default() };
	}
	parts
}

/// `rows` as a model: the one in `kept` when they are the same rows.
fn model_of<T: Clone + PartialEq + 'static>(
	rows: Vec<T>,
	kept: Option<&mut (Vec<T>, ModelRc<T>)>,
) -> ModelRc<T> {
	match kept {
		Some((old, model)) if *old == rows => model.clone(),
		Some(kept) => {
			let model = ModelRc::from(Rc::new(VecModel::from(rows.clone())));
			*kept = (rows, model.clone());
			model
		}
		None => ModelRc::from(Rc::new(VecModel::from(rows))),
	}
}

/// A link to a channel's file, which the chat shows as a card.
fn is_file(address: &str) -> bool {
	FileRef::parse(address).is_some()
}

fn read(message: &ChatMessage) -> Read {
	let mut doc = bbcode::parse(&message.text);
	doc.retain(|span| match span {
		Span::Text { style, .. } => !style.link.as_deref().is_some_and(is_file),
		Span::Image { url } => !is_file(url),
	});
	let images = web_images(&doc);
	Read { doc, files: message.file_refs(), images }
}

fn web_images(doc: &Doc) -> Vec<String> {
	doc.images().into_iter().filter(|u| bbcode::is_web(u)).map(str::to_owned).collect()
}

/// The addresses of the pictures on the web a message's text shows
/// (`[img]`).
pub fn web_pictures(text: &str) -> Vec<String> {
	if !text.as_bytes().windows(5).any(|w| w.eq_ignore_ascii_case(b"[img]")) {
		return Vec::new();
	}
	web_images(&bbcode::parse(text))
}

/// The colours of a span: the author's made readable on the dark and the
/// light theme, or transparent for the text's own.
fn inks(color: Option<[u8; 3]>) -> (Color, Color) {
	let rgb = |[r, g, b]: [u8; 3]| Color::from_rgb_u8(r, g, b);
	match color {
		Some(c) => (rgb(bbcode::readable(c, true)), rgb(bbcode::readable(c, false))),
		None => (Color::default(), Color::default()),
	}
}

/// A run of `text` (or the emoji `emoji`) in `style`; `masked`: its link's
/// text shows something else ([`bbcode::is_masked`]).
fn run(
	text: &str,
	emoji: Option<String>,
	style: &Style,
	masked: bool,
	inks: (Color, Color),
) -> TextRun {
	TextRun {
		text: text.into(),
		emoji: emoji.unwrap_or_default().into(),
		bold: style.bold,
		italic: style.italic,
		underline: style.underline,
		strike: style.strike,
		code: style.code,
		ink_dark: inks.0,
		ink_light: inks.1,
		link: style.link.clone().unwrap_or_default().into(),
		masked,
	}
}

/// Characters of a run at most: a longer word (an address) is cut into
/// runs, so a line can break inside it (a run does not wrap).
const LONG_WORD: usize = 24;

/// `word` in pieces of at most [`LONG_WORD`] characters.
fn pieces(word: &str) -> Vec<&str> {
	let mut out = Vec::new();
	let mut start = 0;
	for (count, (at, _)) in word.grapheme_indices(true).enumerate() {
		if count > 0 && count % LONG_WORD == 0 {
			out.push(&word[start..at]);
			start = at;
		}
	}
	out.push(&word[start..]);
	out
}

/// The runs of spans: a run per word and per emoji (short inline code in
/// one piece, on one background), and whether a run is an emoji.
fn runs_of(spans: &[Span]) -> (Vec<TextRun>, bool) {
	let mut runs: Vec<TextRun> = Vec::new();
	let mut any_emoji = false;
	for span in spans {
		match span {
			Span::Text { text, style } => {
				let inks = inks(style.color);
				let masked = style.link.as_deref().is_some_and(|l| bbcode::is_masked(text, l));
				let words = if style.code && text.graphemes(true).count() <= LONG_WORD {
					vec![emoji::Run { text: text.clone(), emoji: None }]
				} else {
					emoji::split(text)
				};
				for word in words {
					any_emoji |= word.emoji.is_some();
					match word.emoji {
						Some(key) => runs.push(run(&word.text, Some(key), style, masked, inks)),
						None => runs.extend(
							pieces(&word.text)
								.into_iter()
								.map(|p| run(p, None, style, masked, inks)),
						),
					}
				}
			}
			// A picture that is not shown: its address, as a link.
			Span::Image { url } => {
				let style = Style { link: Some(url.clone()), ..Style::default() };
				runs.extend(
					pieces(url).into_iter().map(|p| run(p, None, &style, false, inks(None))),
				);
			}
		}
	}
	(runs, any_emoji)
}

/// The blocks of a doc for `RichText` (code a block per line), and whether
/// a run is an emoji; `None` past [`bbcode::MAX_RUNS`] runs.
fn blocks_of(doc: &Doc) -> Option<(Vec<TextBlock>, bool)> {
	let mut count = 0;
	let mut any_emoji = false;
	let mut blocks = Vec::with_capacity(doc.blocks.len());
	for block in &doc.blocks {
		let (kind, caption) = match &block.kind {
			BlockKind::Text => (0, ""),
			BlockKind::Quote { author } => (1, author.as_str()),
			BlockKind::Code => (2, ""),
			BlockKind::Bullet => (3, ""),
			BlockKind::Rule => (4, ""),
		};
		let lines: Vec<(Vec<TextRun>, bool)> = match &block.spans[..] {
			[Span::Text { text, .. }] if block.kind == BlockKind::Code => text
				.split('\n')
				.map(|line| runs_of(&[Span::Text { text: line.into(), style: Style::default() }]))
				.collect(),
			spans => vec![runs_of(spans)],
		};
		for (mut runs, emoji) in lines {
			any_emoji |= emoji;
			// An empty line keeps its height.
			if runs.is_empty() && block.kind != BlockKind::Rule {
				runs.push(TextRun { text: " ".into(), ..Default::default() });
			}
			count += runs.len();
			if count > bbcode::MAX_RUNS {
				return None;
			}
			blocks.push(TextBlock {
				runs: ModelRc::from(Rc::new(VecModel::from(runs))),
				kind,
				caption: caption.into(),
			});
		}
	}
	Some((blocks, any_emoji))
}

/// What a line shows of `doc`, without the pictures that are cards
/// (`cards[i]`: the doc's `i`th picture on the web).
fn body_of(doc: &Doc, cards: &[bool]) -> Body {
	let mut doc = Cow::Borrowed(doc);
	if cards.contains(&true) {
		let mut i = 0;
		doc.to_mut().retain(|span| {
			let Span::Image { url } = span else { return true };
			if !bbcode::is_web(url) {
				return true;
			}
			i += 1;
			!cards.get(i - 1).copied().unwrap_or(false)
		});
	}
	let text = doc.plain();
	let words = emoji::split(&text);
	let jumbo = emoji::is_jumbo(&words);
	let link = doc.links().into_iter().find(|l| bbcode::is_web(l)).unwrap_or_default().to_owned();
	let rich = !doc.is_plain() || words.iter().any(|w| w.emoji.is_some());
	// Past the runs a line can have: its plain text.
	match rich.then(|| blocks_of(&doc)).flatten() {
		Some((blocks, _)) => Body {
			text,
			blocks: ModelRc::from(Rc::new(VecModel::from(blocks))),
			rich: true,
			jumbo,
			link,
		},
		None => Body { text, blocks: ModelRc::default(), rich: false, jumbo, link },
	}
}

/// The picture at `url`, if the engine has it and it can be shown.
fn picture(pictures: Option<&HashMap<String, PathBuf>>, url: &str) -> Option<Image> {
	let image = avatar::image(pictures?.get(url));
	(image.size().width > 0).then_some(image)
}

/// A picture's name: the last part of its address's path.
fn picture_name(url: &str) -> &str {
	let path = url.split(['?', '#']).next().unwrap_or(url);
	path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(url)
}

/// A chat's last message for a list of chats, on one line ("Mira: the
/// banner I promised: guild-banner.png"), and when it came.
pub fn preview(message: &ChatMessage) -> (String, String) {
	let text = if message.blocked {
		"Message from a blocked contact.".to_owned()
	} else {
		let read = read(message);
		let names = read.files.into_iter().map(|f| f.name);
		std::iter::once(read.doc.one_line()).chain(names).collect::<Vec<_>>().join(" ")
	};
	// A one-line text cannot show emoji (they are pictures): left out.
	let mut text = crate::vm::social::without_emoji(&text);
	if text.is_empty() {
		text = "sent an emoji".into();
	}
	(format!("{}: {text}", message.author_name), time_of(message.ts_ms))
}

/// `text` by `author` as a quote to answer below: each line after `> `,
/// the first with who said it ("> Nova: see you at 8\n> bring elixirs\n");
/// "" for no text. TeamSpeak clients show `[quote]` as it is, so a quote
/// is plain lines.
pub fn quote(author: &str, text: &str) -> String {
	let mut out = String::new();
	for (i, line) in text.trim().lines().enumerate() {
		match i {
			0 if !author.is_empty() => out.push_str(&format!("> {author}: {line}\n")),
			_ => out.push_str(&format!("> {line}\n")),
		}
	}
	out
}

/// The line of a stored message; `previous` is the line before it.
pub fn history_line(
	message: &HistoryMessage,
	previous: Option<&Previous>,
	ctx: &LineCtx,
) -> ChatLine {
	let reactions = message
		.reactions
		.iter()
		.map(|r| ReactionItem {
			key: emoji::first_key(&r.emoji).unwrap_or_default().into(),
			text: r.emoji.clone().into(),
			count: r.count as i32,
			me: r.me,
		})
		.collect();
	let mut cache = ctx.cache.map(|c| c.0.borrow_mut());
	let parts =
		cache.as_mut().map(|all| current(all, message.id, message.rev, &message.message.text));
	let mut line = line_of(&message.message, previous, ctx, parts, reactions);
	line.pinned = message.pinned;
	line.remote = ctx.gateway && message.remote_id.is_some();
	line
}

/// The line of a message the engine keeps no history for (no ids, no
/// reactions).
pub fn line(message: &ChatMessage, previous: Option<&Previous>) -> ChatLine {
	line_of(message, previous, &LineCtx::default(), None, Vec::new())
}

/// A message of the server (its welcome or host message), by `server`.
pub fn server_message(server: &str, text: &str) -> ChatMessage {
	ChatMessage {
		target: ChatTarget::Server,
		author_name: server.to_owned(),
		author_uid: None,
		author_id: None,
		text: text.to_owned(),
		ts_ms: 0,
		via_relay: false,
		blocked: false,
	}
}

/// A message of the server ([`server_message`]) as a line of the server
/// chat: by the server, without a time, reactions or pins. `id` keeps what
/// it was built from in the cache, apart from the messages'.
pub fn server_line(server: &str, text: &str, id: i64, continued: bool, ctx: &LineCtx) -> ChatLine {
	let message = server_message(server, text);
	let previous = continued.then(|| Previous::of(&message));
	let mut cache = ctx.cache.map(|c| c.0.borrow_mut());
	let parts = cache.as_mut().map(|all| current(all, id, 0, text));
	let mut line = line_of(&message, previous.as_ref(), ctx, parts, Vec::new());
	line.time = Default::default();
	line
}

fn line_of(
	message: &ChatMessage,
	previous: Option<&Previous>,
	ctx: &LineCtx,
	parts: Option<&mut Parts>,
	reactions: Vec<ReactionItem>,
) -> ChatLine {
	let continued = !ctx.unread_start
		&& previous.is_some_and(|p| {
			p.author == message.author_name && (0..GROUP_MS).contains(&(message.ts_ms - p.ts_ms))
		});
	let cached = parts.is_some();
	let mut own = Parts::default();
	let parts = parts.unwrap_or(&mut own);
	let read = parts.read.get_or_insert_with(|| read(message));
	// Pictures on the web that are here are cards, after the files'.
	let pictures: Vec<Option<Image>> =
		read.images.iter().map(|url| picture(ctx.pictures, url)).collect();
	let shown: Vec<bool> = pictures.iter().map(Option::is_some).collect();
	let body = match &parts.body {
		Some((was, body)) if *was == shown => body.clone(),
		_ => {
			let body = body_of(&read.doc, &shown);
			parts.body = Some((shown, body.clone()));
			body
		}
	};
	let mut cards: Vec<FileItem> = read
		.files
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
	let first = cards.len();
	for (i, (url, picture)) in read.images.iter().zip(pictures).enumerate() {
		if let Some(picture) = picture {
			cards.push(FileItem {
				name: picture_name(url).into(),
				index: (first + i) as i32,
				picture: true,
				preview: picture,
				..Default::default()
			});
		}
	}
	let files = model_of(cards, cached.then_some(&mut parts.files));
	let reactions = model_of(reactions, cached.then_some(&mut parts.reactions));
	ChatLine {
		key: ctx.key,
		author: message.author_name.clone().into(),
		text: body.text.into(),
		time: time_of(message.ts_ms).into(),
		blocks: body.blocks,
		rich: body.rich,
		jumbo: body.jumbo,
		continued,
		initials: avatar::initials(&message.author_name).into(),
		tint: avatar::tint(&message.author_name),
		avatar: ctx.avatar.clone(),
		blocked: message.blocked,
		pinned: false,
		reactions,
		files,
		remote: false,
		topic: ctx.topic.clone().into(),
		marked: ctx.marked,
		link: body.link.into(),
		unread_start: ctx.unread_start,
	}
}

/// The address of picture `index` of a message's links, counted as its
/// line's cards are: its files first, then its pictures on the web.
pub fn picture_link(message: &ChatMessage, index: usize) -> Option<String> {
	let read = read(message);
	index.checked_sub(read.files.len()).and_then(|i| read.images.into_iter().nth(i))
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

	/// The "New" divider is above its line, so the line has its header even
	/// right after the same author.
	#[test]
	fn the_first_new_line_is_not_continued() {
		let prev = Previous::of(&message("Alice", "hi", 1_000_000));
		let again = stored(message("Alice", "again", 1_060_000));
		let line = history_line(&again, Some(&prev), &LineCtx::default());
		assert!(line.continued && !line.unread_start);
		let ctx = LineCtx { unread_start: true, ..Default::default() };
		let line = history_line(&again, Some(&prev), &ctx);
		assert!(line.unread_start && !line.continued);
	}

	#[test]
	fn first_unread_messages() {
		// (ts_ms, id, ours), in order.
		let chat = [(10, 1, false), (20, 2, true), (20, 3, false), (30, 4, false)];
		// Nothing read: everything not ours is new.
		assert_eq!((first_unread(&chat, None), unread_count(&chat, None)), (Some(0), 3));
		// Our own messages are skipped.
		assert_eq!(first_unread(&chat, Some((10, 1))), Some(2));
		assert_eq!(unread_count(&chat, Some((10, 1))), 2);
		// At the same time the id decides.
		assert_eq!(first_unread(&chat, Some((20, 2))), Some(2));
		assert_eq!(first_unread(&chat, Some((20, 3))), Some(3));
		assert_eq!(first_unread(&[(20, 5, false), (20, 6, false)], Some((20, 5))), Some(1));
		// Everything read, or read past the end.
		for read in [(30, 4), (40, 0)] {
			assert_eq!(
				(first_unread(&chat, Some(read)), unread_count(&chat, Some(read))),
				(None, 0)
			);
		}
		assert_eq!(first_unread(&[], None), None);
	}

	#[test]
	fn new_since() {
		let at = |d: &str| chrono::NaiveDateTime::parse_from_str(d, "%Y-%m-%d %H:%M").unwrap();
		let now = at("2026-10-03 09:00");
		assert_eq!(since(at("2026-10-03 08:05"), now), "08:05");
		assert_eq!(since(at("2026-03-12 10:14"), now), "12 Mar 10:14");
		assert_eq!(since(at("2025-12-31 10:14"), now), "31 Dec 2025 10:14");
	}

	/// The runs of block `i` of a line, as (text, emoji).
	fn runs(line: &ChatLine, i: usize) -> Vec<TextRun> {
		line.blocks.row_data(i).unwrap().runs.iter().collect()
	}

	#[test]
	fn emoji_runs() {
		let plain = line(&message("A", "no emoji here\nnor here", 0), None);
		assert!(!plain.rich && plain.blocks.row_count() == 0);
		assert_eq!(plain.text, "no emoji here\nnor here");
		let rich = line(&message("A", "gg 🎉", 0), None);
		assert!(rich.rich && !rich.jumbo);
		assert_eq!(rich.blocks.row_count(), 1);
		let r = runs(&rich, 0);
		assert_eq!((r.len(), r[0].text.as_str(), r[1].emoji.as_str()), (2, "gg ", "1f389"));
		assert!(line(&message("A", "🎉", 0), None).jumbo);
		// Formatting around it: still large.
		let jumbo = line(&message("A", "[b]🎉[/b]", 0), None);
		assert!(jumbo.rich && jumbo.jumbo);
		// Lines stay lines with emoji (they were run together).
		let lines = line(&message("A", "first 🎉\nsecond", 0), None);
		assert_eq!(lines.blocks.row_count(), 2);
		assert_eq!(lines.text, "first 🎉\nsecond");
	}

	#[test]
	fn bbcode_lines() {
		let text = "[b]Raid[/b] at [color=#f00]8[/color], see https://x.org/raid\n\
		            [quote=Nova]bring elixirs[/quote]\n[list][*]one[*][u]two[/u][/list][hr]\
		            [code]a\n\nb[/code]";
		let line = line(&message("A", text, 0), None);
		assert!(line.rich && !line.jumbo);
		assert_eq!(
			line.text,
			"Raid at 8, see https://x.org/raid\n> Nova: bring elixirs\n• one\n• two\n---\na\n\nb"
		);
		assert_eq!(line.link, "https://x.org/raid");
		let kinds: Vec<(i32, String)> =
			line.blocks.iter().map(|b| (b.kind, b.caption.to_string())).collect();
		let kind = |k: i32| (k, String::new());
		assert_eq!(
			kinds,
			[kind(0), (1, "Nova".into()), kind(3), kind(3), kind(4), kind(2), kind(2), kind(2)]
		);
		let first = runs(&line, 0);
		assert!(first[0].bold && first[0].text == "Raid");
		let eight = first.iter().find(|r| r.text.starts_with('8')).unwrap();
		// The author's red, readable on both themes.
		assert!(eight.ink_dark.alpha() == 255 && eight.ink_light.alpha() == 255);
		assert!(eight.ink_dark.red() > eight.ink_dark.green());
		assert_eq!(first[0].ink_dark.alpha(), 0, "the text's own colour");
		let address = first.iter().find(|r| !r.link.is_empty()).unwrap();
		assert_eq!(
			(address.text.as_str(), address.link.as_str()),
			("https://x.org/raid", "https://x.org/raid")
		);
		assert!(!address.masked, "its own address");
		assert!(runs(&line, 3)[0].underline);
		assert!(runs(&line, 4).is_empty(), "a rule has no runs");
		// Code: a block per line, empty ones kept.
		let code: Vec<String> = (5..8).map(|i| runs(&line, i)[0].text.to_string()).collect();
		assert_eq!(code, ["a", " ", "b"]);
		// Inline code: one run.
		let inline = super::line(&message("A", "run [code]/pull 10[/code]", 0), None);
		let r = runs(&inline, 0);
		assert!(r[1].code && r[1].text == "/pull 10");
		// Not formatting: plain.
		let relayed = super::line(&message("A", "[Nova] hi [1]", 0), None);
		assert!(!relayed.rich && relayed.text == "[Nova] hi [1]" && relayed.link.is_empty());
	}

	/// A link whose text is not its address asks before it opens: each of
	/// its runs says so.
	#[test]
	fn masked_link_runs() {
		let text = "on the [url=https://x.org/raids]raid board[/url], or www.x.org";
		let line = line(&message("A", text, 0), None);
		let links: Vec<(String, bool)> = runs(&line, 0)
			.iter()
			.filter(|r| !r.link.is_empty())
			.map(|r| (r.text.trim().to_owned(), r.masked))
			.collect();
		let masked = |t: &str| (t.to_owned(), true);
		assert_eq!(links, [masked("raid"), masked("board"), ("www.x.org".into(), false)]);
	}

	#[test]
	fn quotes() {
		assert_eq!(quote("Nova", "see you at 8"), "> Nova: see you at 8\n");
		assert_eq!(
			quote("Nova", "see you at 8\r\nbring elixirs\n\nok"),
			"> Nova: see you at 8\n> bring elixirs\n> \n> ok\n"
		);
		assert_eq!(quote("Nova", "  \n "), "");
		assert_eq!(quote("", "hi"), "> hi\n");
		// A line's text is plain (BBCode read), a quote in it quoted again.
		let line = line(&message("Kairo", "[quote=Nova]bring [b]elixirs[/b][/quote]\nok", 0), None);
		assert_eq!(quote(&line.author, &line.text), "> Kairo: > Nova: bring elixirs\n> ok\n");
	}

	/// A run does not wrap: long words (addresses) and long inline code
	/// are cut so the line can break inside them.
	#[test]
	fn long_words_are_cut() {
		let url = format!("https://x.org/{}", "a".repeat(40));
		let line = line(&message("A", &format!("see {url}"), 0), None);
		let r = runs(&line, 0);
		let pieces: Vec<&str> = r[1..].iter().map(|r| r.text.as_str()).collect();
		assert_eq!(pieces.concat(), url);
		assert!(pieces.len() == 3 && pieces.iter().all(|p| p.chars().count() <= LONG_WORD));
		assert!(r[1..].iter().all(|r| r.link == url.as_str()));
		let code = "cargo build --release --locked -j 1";
		let line = super::line(&message("A", &format!("[code]{code}[/code]"), 0), None);
		let r = runs(&line, 0);
		assert!(r.len() > 1 && r.iter().all(|r| r.code));
		assert_eq!(r.iter().map(|r| r.text.as_str()).collect::<String>(), code);
	}

	#[test]
	fn pictures_on_the_web() {
		let url = "https://x.org/loot.png";
		let text = format!("loot [img]{url}[/img]");
		// Not here (or not fetched): the address, as a link.
		let link = line(&message("A", &text, 0), None);
		assert_eq!(link.files.row_count(), 0);
		let r = runs(&link, 0);
		assert_eq!((r[1].text.as_str(), r[1].link.as_str()), (url, url));
		// Here: a picture card after the files, and no address.
		let dir = std::env::temp_dir().join(format!("voelin-chat-picture-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("loot");
		let mut bytes = Vec::new();
		let mut encoder = png::Encoder::new(&mut bytes, 4, 2);
		encoder.set_color(png::ColorType::Rgba);
		encoder.set_depth(png::BitDepth::Eight);
		encoder.write_header().unwrap().write_image_data(&[200; 32]).unwrap();
		std::fs::write(&path, bytes).unwrap();
		let pictures = HashMap::from([(url.to_owned(), path)]);
		let file = "ts3file://plan.pdf?channel=2&path=/&filename=plan.pdf&isDir=0&size=2048";
		let stored = stored(message("A", &format!("{text} [URL={file}]plan.pdf[/URL]"), 0));
		let ctx = LineCtx { pictures: Some(&pictures), ..Default::default() };
		let shown = history_line(&stored, None, &ctx);
		assert_eq!(shown.text, "loot");
		assert_eq!(shown.files.row_count(), 2);
		let card = shown.files.row_data(1).unwrap();
		assert_eq!((card.name.as_str(), card.index, card.preview.size().width), ("loot.png", 1, 4));
		assert_eq!(picture_link(&stored.message, 1).as_deref(), Some(url));
		assert_eq!(picture_link(&stored.message, 0), None);
		assert_eq!(web_pictures(&text), [url]);
		assert!(web_pictures("[url]https://x.org/a.png[/url]").is_empty());
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// A line built again from an unchanged message is equal (its models
	/// are the same), so `list::sync` leaves its row; a change builds anew.
	#[test]
	fn unchanged_lines_compare_equal() {
		let cache = LineCache::default();
		// An avatar: an empty picture is never equal.
		let avatar = Image::from_rgba8(slint::SharedPixelBuffer::new(1, 1));
		let ctx = LineCtx { key: 3, avatar, cache: Some(&cache), ..Default::default() };
		let mut stored = stored(message("A", "[b]hi[/b] 🎉", 0));
		let first = history_line(&stored, None, &ctx);
		assert!(first == history_line(&stored, None, &ctx));
		// Without the cache, models are new each time.
		let plain = LineCtx { key: 3, avatar: ctx.avatar.clone(), ..Default::default() };
		assert!(history_line(&stored, None, &plain) != history_line(&stored, None, &plain));
		stored.reactions[0].count = 3;
		let reacted = history_line(&stored, None, &ctx);
		assert!(reacted != first && reacted.blocks == first.blocks);
		stored.message.text = "[i]edited[/i]".into();
		stored.rev = 2;
		let edited = history_line(&stored, None, &ctx);
		assert!(edited.blocks != first.blocks);
		assert_eq!(edited.text, "edited");
		cache.keep([], 0);
		assert_eq!(cache.0.borrow().len(), 1, "few: kept");
	}

	#[test]
	fn server_lines() {
		let cache = LineCache::default();
		let ctx = LineCtx { key: -1, cache: Some(&cache), ..Default::default() };
		let welcome =
			server_line("Nightfall", "Welcome to [b]Nightfall[/b]!", i64::MIN, false, &ctx);
		assert_eq!((welcome.author.as_str(), welcome.time.as_str()), ("Nightfall", ""));
		assert!(welcome.rich && !welcome.continued && !welcome.remote && welcome.key == -1);
		assert_eq!(welcome.text, "Welcome to Nightfall!");
		assert!(server_line("Nightfall", "News", i64::MIN + 1, true, &ctx).continued);
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
		for text in [
			format!("here you go [URL={url}]plan.pdf[/URL] 🎉"),
			format!("here you go [b][URL={url}]plan.pdf[/URL][/b] 🎉"),
			format!("here you go {url} 🎉"),
		] {
			let line = line(&message("A", &text, 0), None);
			assert_eq!(line.files.row_count(), 1);
			let file = line.files.row_data(0).unwrap();
			assert_eq!((file.name.as_str(), file.detail.as_str()), ("plan.pdf", "2.0 kB"));
			assert_eq!(line.text, "here you go 🎉");
			assert!(line.rich);
		}
		// Only the link: no text.
		let only = line(&message("A", &format!("[URL={url}]plan.pdf[/URL]"), 0), None);
		assert!(only.text.is_empty() && !only.rich);
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
