//! Chat text as TeamSpeak clients write it, with BBCode (`[b]`, `[url=…]`,
//! `[quote]`, `[list]` …), read into a [`Doc`]: blocks (lines, quotes, code,
//! list items, rules) of styled spans and pictures. The chat draws a Doc
//! (`vm::chat`) and reads out its [`Doc::plain`] text; lists of chats,
//! notifications and the home's news show its [`Doc::one_line`] text.
//!
//! The Doc knows nothing of BBCode: a Markdown front end (TeamSpeak 6
//! writes `**bold**` and `> quotes`) can be a second parser making the same
//! Doc.
//!
//! - Only known tags are read, in any case; other text in brackets stays
//!   (`[Nova] hi`, `[1] item`).
//! - A tag left open ends with the message; a closing tag that closes
//!   nothing stays text. Tags may close out of order.
//! - `[left]`, `[center]`, `[right]`, `[size]`, `[table]`, `[th]` and `[td]`
//!   are dropped and their text kept; `[tr]` starts a line.
//! - Nothing inside `[code]` is a tag: one line of code is a span, more are
//!   a block.
//! - A line break starts a block; one right after a block tag (`[quote]`,
//!   `[list]`, `[*]` …) belongs to the tag, and so do the spaces after it.
//! - Bare addresses (`https://…`, `www.…`) are links, without the
//!   punctuation after them.
//! - Links only go to the web, to TeamSpeak servers and to channel files
//!   ([`link_target`]): `[url=javascript:…]` is no link.
//! - Past [`MAX_DEPTH`] tags open at once or [`MAX_RUNS`] spans, a message
//!   is plain text.

use std::borrow::Cow;

/// Tags open at once, at most; deeper, the message is plain text.
pub const MAX_DEPTH: usize = 16;
/// Spans of a message, at most (and in the chat, its words and emoji);
/// more, and it is plain text.
pub const MAX_RUNS: usize = 1500;

/// How a span of text looks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Style {
	pub bold: bool,
	pub italic: bool,
	pub underline: bool,
	pub strike: bool,
	/// Code: on a background.
	pub code: bool,
	/// The author's colour (shown through [`readable`]).
	pub color: Option<[u8; 3]>,
	/// Where it links to ([`link_target`]).
	pub link: Option<String>,
}

/// A piece of a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Span {
	Text {
		text: String,
		style: Style,
	},
	/// A picture at an address (`[img]…[/img]`).
	Image {
		url: String,
	},
}

/// What a block is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockKind {
	/// A line of text.
	Text,
	/// A line of a quote; the first names who said it (`[quote=Name]`), when
	/// the quote does.
	Quote { author: String },
	/// Code: all its lines in one span.
	Code,
	/// An item of a list.
	Bullet,
	/// A horizontal line (`[hr]`), without spans.
	Rule,
}

/// A line, a quote's line, code, a list item or a rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
	pub kind: BlockKind,
	pub spans: Vec<Span>,
}

/// A message's text: its blocks, top to bottom.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doc {
	pub blocks: Vec<Block>,
}

impl Block {
	fn new(kind: BlockKind) -> Self {
		Self { kind, spans: Vec::new() }
	}

	/// Nothing but spaces.
	fn is_blank(&self) -> bool {
		self.spans.iter().all(|s| matches!(s, Span::Text { text, .. } if text.trim().is_empty()))
	}

	/// An empty line (of text or of a quote).
	fn is_empty_line(&self) -> bool {
		self.spans.is_empty() && matches!(self.kind, BlockKind::Text | BlockKind::Quote { .. })
	}

	/// The text of the spans; a picture is its address.
	fn text_into(&self, out: &mut String) {
		for span in &self.spans {
			match span {
				Span::Text { text, .. } => out.push_str(text),
				Span::Image { url } => out.push_str(url),
			}
		}
	}

	/// No spaces at the start and the end.
	fn trim(&mut self) {
		if let Some(Span::Text { text, .. }) = self.spans.first_mut() {
			*text = text.trim_start().to_owned();
		}
		if let Some(Span::Text { text, .. }) = self.spans.last_mut() {
			text.truncate(text.trim_end().len());
		}
		self.spans.retain(|s| !matches!(s, Span::Text { text, .. } if text.is_empty()));
	}
}

impl Doc {
	/// Every line of `text` as it is: what a message past the caps shows.
	pub fn plain_text(text: &str) -> Self {
		let mut blocks: Vec<Block> = text
			.lines()
			.map(|line| Block {
				kind: BlockKind::Text,
				spans: if line.trim().is_empty() {
					Vec::new()
				} else {
					vec![Span::Text { text: line.to_owned(), style: Style::default() }]
				},
			})
			.collect();
		trim_lines(&mut blocks);
		Self { blocks }
	}

	/// The text, a line per block: a quote's lines start with `> ` (the
	/// first with its author), list items with `• `, a rule is `---` and a
	/// picture its address. For copying and screen readers.
	pub fn plain(&self) -> String {
		let mut out = String::new();
		for (i, block) in self.blocks.iter().enumerate() {
			if i > 0 {
				out.push('\n');
			}
			match &block.kind {
				BlockKind::Quote { author } if author.is_empty() => out.push_str("> "),
				BlockKind::Quote { author } => out.push_str(&format!("> {author}: ")),
				BlockKind::Bullet => out.push_str("• "),
				BlockKind::Rule => out.push_str("---"),
				BlockKind::Text | BlockKind::Code => {}
			}
			block.text_into(&mut out);
		}
		out
	}

	/// The text on one line, runs of spaces as one (lists and previews).
	pub fn one_line(&self) -> String {
		let mut all = String::new();
		for block in &self.blocks {
			block.text_into(&mut all);
			all.push(' ');
		}
		all.split_whitespace().collect::<Vec<_>>().join(" ")
	}

	/// Where the text links to, in order, each once.
	pub fn links(&self) -> Vec<&str> {
		let mut links: Vec<&str> = Vec::new();
		for span in self.blocks.iter().flat_map(|b| &b.spans) {
			if let Span::Text { style: Style { link: Some(link), .. }, .. } = span
				&& !links.contains(&link.as_str())
			{
				links.push(link);
			}
		}
		links
	}

	/// The addresses of the pictures, in order.
	pub fn images(&self) -> Vec<&str> {
		self.blocks
			.iter()
			.flat_map(|b| &b.spans)
			.filter_map(|s| match s {
				Span::Image { url } => Some(url.as_str()),
				Span::Text { .. } => None,
			})
			.collect()
	}

	/// Lines of text without style, links or pictures: shown as they are.
	pub fn is_plain(&self) -> bool {
		self.blocks.iter().all(|b| {
			b.kind == BlockKind::Text
				&& b.spans
					.iter()
					.all(|s| matches!(s, Span::Text { style, .. } if *style == Style::default()))
		})
	}

	/// Leave out the spans `keep` refuses (a file link shown as a card),
	/// one space where they were, and the blocks left without text.
	pub fn retain(&mut self, mut keep: impl FnMut(&Span) -> bool) {
		self.blocks.retain_mut(|block| {
			let kept: Vec<bool> = block.spans.iter().map(&mut keep).collect();
			if !kept.contains(&false) {
				return true;
			}
			let mut spans: Vec<Span> = Vec::with_capacity(block.spans.len());
			let mut gap = false;
			for (mut span, kept) in block.spans.drain(..).zip(kept) {
				if !kept {
					gap = true;
					continue;
				}
				if let (true, Some(Span::Text { text: before, .. }), Span::Text { text, .. }) =
					(gap, spans.last(), &mut span)
					&& before.ends_with(char::is_whitespace)
				{
					*text = text.trim_start().to_owned();
				}
				gap = false;
				let merged = match (spans.last_mut(), &span) {
					(
						Some(Span::Text { text: before, style: before_style }),
						Span::Text { text, style },
					) if before_style == style => {
						before.push_str(text);
						true
					}
					_ => false,
				};
				if !merged {
					spans.push(span);
				}
			}
			block.spans = spans;
			block.trim();
			!block.spans.is_empty()
		});
		trim_lines(&mut self.blocks);
	}
}

/// No empty lines at the start and the end.
fn trim_lines(blocks: &mut Vec<Block>) {
	while blocks.last().is_some_and(Block::is_empty_line) {
		blocks.pop();
	}
	let start = blocks.iter().take_while(|b| b.is_empty_line()).count();
	blocks.drain(..start);
}

/// Schemes a link may have: the web, TeamSpeak servers, channel files.
const SCHEMES: [&str; 8] = [
	"https://",
	"http://",
	"ts3server://",
	"teamspeak://",
	"ts3file://",
	"tsfile://",
	"ts5file://",
	"ts6file://",
];

/// Where a link may go: the web (`www.…` as `https://www.…`), TeamSpeak
/// servers (`ts3server`, `teamspeak`) and channel files (`ts3file` and the
/// like, which the chat shows as cards). Anything else (`javascript:`,
/// `file:`, `data:`, an address with spaces) is no link.
pub fn link_target(address: &str) -> Option<String> {
	let address = address.trim();
	if address.chars().any(|c| c.is_whitespace() || c.is_control()) {
		return None;
	}
	let lower = address.to_ascii_lowercase();
	if lower.starts_with("www.") && address.len() > 4 {
		return Some(format!("https://{address}"));
	}
	SCHEMES
		.iter()
		.any(|s| lower.starts_with(s) && address.len() > s.len())
		.then(|| address.to_owned())
}

/// An address on the web (`http`, `https`).
pub fn is_web(address: &str) -> bool {
	let lower = address.get(..8).unwrap_or(address).to_ascii_lowercase();
	lower.starts_with("https://") || lower.starts_with("http://")
}

/// Where the first bare address in `text` is (its start and end), if any:
/// `https://…`, `http://…`, `www.…` or a TeamSpeak link at the start of a
/// word, up to a space, without the punctuation after it.
fn find_address(text: &str) -> Option<(usize, usize)> {
	const STARTS: [&str; 9] = [
		"https://",
		"http://",
		"www.",
		"ts3server://",
		"teamspeak://",
		"ts3file://",
		"tsfile://",
		"ts5file://",
		"ts6file://",
	];
	let lower = text.to_ascii_lowercase();
	let mut from = 0;
	loop {
		let (start, prefix) = STARTS
			.iter()
			.filter_map(|p| lower[from..].find(p).map(|i| (from + i, *p)))
			.min_by_key(|(i, _)| *i)?;
		let word_start = text[..start]
			.chars()
			.next_back()
			.is_none_or(|c| !c.is_alphanumeric() && !matches!(c, '.' | '/' | '@' | '-' | '_'));
		let end = text[start..]
			.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"'))
			.map_or(text.len(), |i| start + i);
		let end = start + trim_address(&text[start..end]).len();
		let after = &text[start + prefix.len()..end.max(start + prefix.len())];
		if word_start && after.chars().next().is_some_and(char::is_alphanumeric) {
			return Some((start, end));
		}
		from = start + prefix.len();
	}
}

/// An address without the punctuation after it (`https://x.org).` →
/// `https://x.org`); a bracket it opened itself stays (`…/Foo_(bar)`).
fn trim_address(address: &str) -> &str {
	let mut address = address;
	loop {
		let count = |c: char| address.matches(c).count();
		let trimmed = match address.chars().next_back() {
			Some('.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '*') => true,
			Some(')') => count(')') > count('('),
			Some(']') => count(']') > count('['),
			Some('}') => count('}') > count('{'),
			_ => false,
		};
		if !trimmed {
			return address;
		}
		address = &address[..address.len() - 1];
	}
}

/// A colour as BBCode gives it: `#rgb`, `#rrggbb` or a name (`red`).
fn color(value: &str) -> Option<[u8; 3]> {
	let value = unquote(value);
	if let Some(hex) = value.strip_prefix('#') {
		let digits: Vec<u8> =
			hex.chars().map(|c| c.to_digit(16).map(|d| d as u8)).collect::<Option<_>>()?;
		return match digits[..] {
			[r, g, b] => Some([r * 17, g * 17, b * 17]),
			[r1, r2, g1, g2, b1, b2] => Some([r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2]),
			_ => None,
		};
	}
	let name = value.to_ascii_lowercase();
	COLORS.iter().find(|(n, _)| *n == name).map(|(_, rgb)| *rgb)
}

/// Colour names (as in CSS) that chats use.
const COLORS: [(&str, [u8; 3]); 48] = [
	("black", [0x00, 0x00, 0x00]),
	("white", [0xff, 0xff, 0xff]),
	("red", [0xff, 0x00, 0x00]),
	("green", [0x00, 0x80, 0x00]),
	("blue", [0x00, 0x00, 0xff]),
	("yellow", [0xff, 0xff, 0x00]),
	("orange", [0xff, 0xa5, 0x00]),
	("purple", [0x80, 0x00, 0x80]),
	("pink", [0xff, 0xc0, 0xcb]),
	("brown", [0xa5, 0x2a, 0x2a]),
	("gray", [0x80, 0x80, 0x80]),
	("grey", [0x80, 0x80, 0x80]),
	("silver", [0xc0, 0xc0, 0xc0]),
	("maroon", [0x80, 0x00, 0x00]),
	("olive", [0x80, 0x80, 0x00]),
	("lime", [0x00, 0xff, 0x00]),
	("aqua", [0x00, 0xff, 0xff]),
	("cyan", [0x00, 0xff, 0xff]),
	("teal", [0x00, 0x80, 0x80]),
	("navy", [0x00, 0x00, 0x80]),
	("fuchsia", [0xff, 0x00, 0xff]),
	("magenta", [0xff, 0x00, 0xff]),
	("gold", [0xff, 0xd7, 0x00]),
	("violet", [0xee, 0x82, 0xee]),
	("indigo", [0x4b, 0x00, 0x82]),
	("crimson", [0xdc, 0x14, 0x3c]),
	("coral", [0xff, 0x7f, 0x50]),
	("salmon", [0xfa, 0x80, 0x72]),
	("tomato", [0xff, 0x63, 0x47]),
	("orangered", [0xff, 0x45, 0x00]),
	("darkorange", [0xff, 0x8c, 0x00]),
	("turquoise", [0x40, 0xe0, 0xd0]),
	("khaki", [0xf0, 0xe6, 0x8c]),
	("chocolate", [0xd2, 0x69, 0x1e]),
	("firebrick", [0xb2, 0x22, 0x22]),
	("darkred", [0x8b, 0x00, 0x00]),
	("darkgreen", [0x00, 0x64, 0x00]),
	("darkblue", [0x00, 0x00, 0x8b]),
	("darkviolet", [0x94, 0x00, 0xd3]),
	("lightblue", [0xad, 0xd8, 0xe6]),
	("lightgreen", [0x90, 0xee, 0x90]),
	("skyblue", [0x87, 0xce, 0xeb]),
	("royalblue", [0x41, 0x69, 0xe1]),
	("dodgerblue", [0x1e, 0x90, 0xff]),
	("limegreen", [0x32, 0xcd, 0x32]),
	("forestgreen", [0x22, 0x8b, 0x22]),
	("deeppink", [0xff, 0x14, 0x93]),
	("hotpink", [0xff, 0x69, 0xb4]),
];

/// A tag's value without the quotes around it (`[url="…"]`).
fn unquote(value: &str) -> &str {
	let value = value.trim();
	for quote in ['"', '\''] {
		if let Some(inner) = value.strip_prefix(quote).and_then(|v| v.strip_suffix(quote)) {
			return inner.trim();
		}
	}
	value
}

/// A colour the author chose, made readable on the chat in the dark or the
/// light theme: lightened or darkened, keeping its hue, until it has a
/// contrast of 4.5:1 to the background (WCAG AA).
pub fn readable(rgb: [u8; 3], dark: bool) -> [u8; 3] {
	// The chat's background (Theme.surface).
	let background = if dark { [0x0b, 0x13, 0x30] } else { [0xf7, 0xf9, 0xfe] };
	let toward = if dark { [0xff; 3] } else { [0x00; 3] };
	(0..=20)
		.map(|step| mix(rgb, toward, step as f32 / 20.0))
		.find(|c| contrast(*c, background) >= 4.5)
		.unwrap_or(toward)
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
	std::array::from_fn(|i| {
		(f32::from(a[i]) + (f32::from(b[i]) - f32::from(a[i])) * t).round() as u8
	})
}

/// Relative luminance (WCAG).
fn luminance(rgb: [u8; 3]) -> f32 {
	let channel = |c: u8| {
		let c = f32::from(c) / 255.0;
		if c <= 0.039_28 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
	};
	0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
}

/// The contrast ratio of two colours, 1 to 21.
pub fn contrast(a: [u8; 3], b: [u8; 3]) -> f32 {
	let (la, lb) = (luminance(a), luminance(b));
	(la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Read a message's text.
pub fn parse(text: &str) -> Doc {
	let text: Cow<str> =
		if text.contains('\r') { text.replace("\r\n", "\n").into() } else { text.into() };
	let mut parser = Parser::default();
	match parser.run(&text) {
		Ok(()) => parser.finish(),
		Err(TooMuch) => Doc::plain_text(&text),
	}
}

/// A tag as written: `[b]`, `[/URL]`, `[color=#f00]`.
struct Tag<'a> {
	/// In lower case.
	name: String,
	value: Option<&'a str>,
	closing: bool,
	/// Its length, brackets included.
	len: usize,
}

/// The tag at the start of `text` (which starts with `[`), if it looks like
/// one: a name of letters (or `*`), maybe `=` and a value, on one line.
fn tag_at(text: &str) -> Option<Tag<'_>> {
	// Not past the next bracket or line break.
	let end = 1 + text[1..].find([']', '[', '\n'])?;
	if text.as_bytes()[end] != b']' {
		return None;
	}
	let inside = &text[1..end];
	let (closing, inside) = match inside.strip_prefix('/') {
		Some(rest) => (true, rest),
		None => (false, inside),
	};
	let (name, value) = match inside.split_once('=') {
		Some((name, value)) => (name, Some(value)),
		None => (inside, None),
	};
	let letters = name.chars().all(|c| c.is_ascii_alphabetic());
	if name.is_empty()
		|| name.len() > 8
		|| !(letters || name == "*")
		|| (closing && value.is_some())
	{
		return None;
	}
	Some(Tag { name: name.to_ascii_lowercase(), value, closing, len: end + 1 })
}

/// The text up to `[/name]` (any case) on this line (in all of `text` with
/// `lines`), or to the end of the line when it is not there; and how much
/// of `text` that used. It reads no further than that.
fn raw<'a>(text: &'a str, name: &str, lines: bool) -> (&'a str, usize) {
	let close = format!("[/{name}]");
	let bytes = text.as_bytes();
	let mut from = 0;
	while let Some(i) = text[from..].find(['[', '\n']) {
		let at = from + i;
		if bytes[at] == b'\n' {
			if !lines {
				return (&text[..at], at);
			}
		} else if bytes
			.get(at..at + close.len())
			.is_some_and(|w| w.eq_ignore_ascii_case(close.as_bytes()))
		{
			return (&text[..at], at + close.len());
		}
		from = at + 1;
	}
	(text, text.len())
}

/// A tag open at a point of the text.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Open {
	Bold,
	Italic,
	Underline,
	Strike,
	Color(Option<[u8; 3]>),
	Link(Option<String>),
	Quote,
	List,
}

impl Open {
	fn name(&self) -> &'static str {
		match self {
			Open::Bold => "b",
			Open::Italic => "i",
			Open::Underline => "u",
			Open::Strike => "s",
			Open::Color(_) => "color",
			Open::Link(_) => "url",
			Open::Quote => "quote",
			Open::List => "list",
		}
	}
}

/// Past a cap: the message is shown as plain text.
struct TooMuch;

#[derive(Default)]
struct Parser {
	blocks: Vec<Block>,
	block: Option<Block>,
	/// The tags open, outermost first.
	open: Vec<Open>,
	/// A line break here belongs to the block tag just before it.
	after_tag: bool,
	/// The author of a quote whose first line is still to come.
	author: Option<String>,
	spans: usize,
}

impl Parser {
	fn run(&mut self, text: &str) -> Result<(), TooMuch> {
		let mut rest = text;
		while let Some(at) = rest.find(['[', '\n']) {
			self.text(&rest[..at])?;
			rest = &rest[at..];
			if let Some(after) = rest.strip_prefix('\n') {
				self.line_break();
				rest = after;
				continue;
			}
			let used = match tag_at(rest) {
				Some(tag) => self.tag(&tag, &rest[tag.len..])?.map(|used| tag.len + used),
				None => None,
			};
			match used {
				Some(used) => rest = &rest[used..],
				None => {
					self.text("[")?;
					rest = &rest[1..];
				}
			}
		}
		self.text(rest)
	}

	fn finish(mut self) -> Doc {
		self.end_block(false);
		trim_lines(&mut self.blocks);
		Doc { blocks: self.blocks }
	}

	/// The style of text here.
	fn style(&self) -> Style {
		let mut style = Style::default();
		for open in &self.open {
			match open {
				Open::Bold => style.bold = true,
				Open::Italic => style.italic = true,
				Open::Underline => style.underline = true,
				Open::Strike => style.strike = true,
				Open::Color(Some(rgb)) => style.color = Some(*rgb),
				Open::Link(Some(link)) => style.link = Some(link.clone()),
				Open::Color(None) | Open::Link(None) | Open::Quote | Open::List => {}
			}
		}
		style
	}

	/// The block text goes into: a line of what is open, made when the
	/// first text comes.
	fn block(&mut self) -> &mut Block {
		let quote = self.open.contains(&Open::Quote);
		let author = &mut self.author;
		self.block.get_or_insert_with(|| {
			Block::new(if quote {
				BlockKind::Quote { author: author.take().unwrap_or_default() }
			} else {
				BlockKind::Text
			})
		})
	}

	/// End the block; a line break leaves an empty line where it was blank.
	fn end_block(&mut self, by_break: bool) {
		let Some(block) = self.block.take() else { return };
		if !block.is_blank() {
			self.blocks.push(block);
			return;
		}
		match block.kind {
			// The author goes with the quote's first words.
			BlockKind::Quote { author } if !author.is_empty() => self.author = Some(author),
			kind @ (BlockKind::Text | BlockKind::Quote { .. }) if by_break => {
				self.blocks.push(Block::new(kind));
			}
			_ => {}
		}
	}

	/// End the block for a block tag; a line break right after it is its own.
	fn block_tag(&mut self) {
		self.end_block(false);
		self.after_tag = true;
	}

	fn line_break(&mut self) {
		if std::mem::take(&mut self.after_tag) {
			return;
		}
		// An empty line, if nothing came since the last break.
		self.block();
		self.end_block(true);
	}

	fn open(&mut self, open: Open) -> Result<(), TooMuch> {
		self.open.push(open);
		if self.open.len() > MAX_DEPTH { Err(TooMuch) } else { Ok(()) }
	}

	/// A tag: `Some(n)` when it was one (and `n` bytes after it were part of
	/// it), `None` when it is text.
	fn tag(&mut self, tag: &Tag, after: &str) -> Result<Option<usize>, TooMuch> {
		if tag.closing {
			return Ok(self.close(&tag.name).then_some(0));
		}
		match tag.name.as_str() {
			"b" => self.open(Open::Bold)?,
			"i" => self.open(Open::Italic)?,
			"u" => self.open(Open::Underline)?,
			"s" => self.open(Open::Strike)?,
			"color" => self.open(Open::Color(tag.value.and_then(color)))?,
			"url" => match tag.value {
				Some(target) => self.open(Open::Link(link_target(unquote(target))))?,
				// `[url]address[/url]`
				None => {
					let (address, used) = raw(after, "url", false);
					match link_target(address) {
						Some(link) => {
							self.push(address, Style { link: Some(link), ..self.style() })?;
							return Ok(Some(used));
						}
						// Not an address (`[url][img]…[/img][/url]`): what
						// is inside is read, its bare addresses as links.
						None => self.open(Open::Link(None))?,
					}
				}
			},
			"img" => {
				let (address, used) = raw(after, "img", false);
				match link_target(address).filter(|a| is_web(a) || a.contains("file://")) {
					Some(url) => self.image(url)?,
					None => self.text(address)?,
				}
				return Ok(Some(used));
			}
			"code" => {
				let (code, used) = raw(after, "code", true);
				self.code(code)?;
				return Ok(Some(used));
			}
			"quote" => {
				self.block_tag();
				self.open(Open::Quote)?;
				self.author = tag.value.map(|v| unquote(v).to_owned()).filter(|a| !a.is_empty());
			}
			"list" => {
				self.block_tag();
				self.open(Open::List)?;
			}
			"*" if self.open.contains(&Open::List) => {
				self.block_tag();
				self.block = Some(Block::new(BlockKind::Bullet));
			}
			"hr" => {
				self.block_tag();
				self.blocks.push(Block::new(BlockKind::Rule));
			}
			"tr" => self.block_tag(),
			// Table cells, apart.
			"td" | "th" => {
				if self.block.as_ref().is_some_and(|b| !b.is_blank()) {
					self.text(" ")?;
				}
			}
			"left" | "center" | "right" | "size" | "table" => {}
			_ => return Ok(None),
		}
		Ok(Some(0))
	}

	/// A closing tag: whether it closed something.
	fn close(&mut self, name: &str) -> bool {
		match name {
			"left" | "center" | "right" | "size" | "table" | "td" | "th" => return true,
			"tr" => {
				self.block_tag();
				return true;
			}
			"*" => return self.open.contains(&Open::List),
			_ => {}
		}
		let Some(at) = self.open.iter().rposition(|o| o.name() == name) else { return false };
		let open = self.open.remove(at);
		if matches!(open, Open::Quote | Open::List) {
			self.block_tag();
			if open == Open::Quote {
				// A quote without text has no author to give.
				self.author = None;
			}
		}
		true
	}

	/// Text in the style here, with its bare addresses as links.
	fn text(&mut self, text: &str) -> Result<(), TooMuch> {
		let style = self.style();
		if style.link.is_some() || style.code {
			return self.push(text, style);
		}
		let mut rest = text;
		while let Some((start, end)) = find_address(rest) {
			self.push(&rest[..start], style.clone())?;
			let address = &rest[start..end];
			self.push(address, Style { link: link_target(address), ..style.clone() })?;
			rest = &rest[end..];
		}
		self.push(rest, style)
	}

	fn push(&mut self, text: &str, style: Style) -> Result<(), TooMuch> {
		// Spaces between a block tag and its line break are the tag's.
		if text.is_empty() || self.after_tag && text.trim().is_empty() {
			return Ok(());
		}
		// Text after a block tag (`[/quote] ok`) starts at its first word.
		let text = if std::mem::take(&mut self.after_tag) && !style.code {
			text.trim_start()
		} else {
			text
		};
		let block = self.block();
		// A list item starts at its first word.
		let text = if block.kind == BlockKind::Bullet && block.spans.is_empty() {
			text.trim_start()
		} else {
			text
		};
		if text.is_empty() {
			return Ok(());
		}
		if let Some(Span::Text { text: last, style: last_style }) = block.spans.last_mut()
			&& *last_style == style
		{
			last.push_str(text);
			return Ok(());
		}
		block.spans.push(Span::Text { text: text.to_owned(), style });
		self.counted()
	}

	fn image(&mut self, url: String) -> Result<(), TooMuch> {
		self.after_tag = false;
		self.block().spans.push(Span::Image { url });
		self.counted()
	}

	/// One line of code is a span; more are a block.
	fn code(&mut self, code: &str) -> Result<(), TooMuch> {
		if !code.contains('\n') {
			let style = Style { code: true, ..self.style() };
			return self.push(code, style);
		}
		let code = code.strip_prefix('\n').unwrap_or(code);
		let code = code.strip_suffix('\n').unwrap_or(code);
		self.block_tag();
		self.blocks.push(Block {
			kind: BlockKind::Code,
			spans: vec![Span::Text { text: code.to_owned(), style: Style::default() }],
		});
		self.counted()
	}

	fn counted(&mut self) -> Result<(), TooMuch> {
		self.spans += 1;
		if self.spans > MAX_RUNS { Err(TooMuch) } else { Ok(()) }
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn text(text: &str) -> Span {
		Span::Text { text: text.into(), style: Style::default() }
	}

	fn styled(text: &str, style: Style) -> Span {
		Span::Text { text: text.into(), style }
	}

	fn line(spans: Vec<Span>) -> Block {
		Block { kind: BlockKind::Text, spans }
	}

	fn bold() -> Style {
		Style { bold: true, ..Default::default() }
	}

	/// The spans of a one-block doc.
	fn spans(source: &str) -> Vec<Span> {
		let doc = parse(source);
		assert_eq!(doc.blocks.len(), 1, "{source}: {doc:?}");
		doc.blocks.into_iter().next().unwrap().spans
	}

	#[test]
	fn inline_tags() {
		assert_eq!(spans("a [b]bold[/b] z"), [text("a "), styled("bold", bold()), text(" z")]);
		let style = |f: fn(&mut Style)| {
			let mut style = Style::default();
			f(&mut style);
			style
		};
		assert_eq!(spans("[i]x[/i]"), [styled("x", style(|s| s.italic = true))]);
		assert_eq!(spans("[u]x[/u]"), [styled("x", style(|s| s.underline = true))]);
		assert_eq!(spans("[s]x[/s]"), [styled("x", style(|s| s.strike = true))]);
		assert_eq!(
			spans("[color=#ff8000]x[/color]"),
			[styled("x", style(|s| s.color = Some([0xff, 0x80, 0x00])))]
		);
		// Layout tags go, their text stays.
		assert_eq!(spans("[center][size=14]Hi[/size][/center]"), [text("Hi")]);
		assert_eq!(spans("[left]a[/left] [right]b[/right]"), [text("a b")]);
	}

	#[test]
	fn links() {
		let link = |l: &str| Style { link: Some(l.into()), ..Default::default() };
		assert_eq!(
			spans("see [URL=https://x.org/a]the page[/URL]!"),
			[text("see "), styled("the page", link("https://x.org/a")), text("!")]
		);
		assert_eq!(
			spans("[url]https://x.org[/url]"),
			[styled("https://x.org", link("https://x.org"))]
		);
		// Quotes around the address go.
		assert_eq!(spans("[url=\"www.x.org\"]x[/url]"), [styled("x", link("https://www.x.org"))]);
		// Styles inside a link.
		assert_eq!(
			spans("[url=https://x.org][b]x[/b][/url]"),
			[styled("x", Style { bold: true, ..link("https://x.org") })]
		);
		// No link to anything but the web, servers and files.
		assert_eq!(spans("[url=javascript:alert(1)]x[/url]"), [text("x")]);
		assert_eq!(spans("[url=file:///etc/passwd]x[/url]"), [text("x")]);
		assert_eq!(spans("[url]javascript:alert(1)[/url]"), [text("javascript:alert(1)")]);
		// Not an address inside: its tags are read, its addresses linked.
		assert_eq!(
			spans("[url][img]https://x.org/a.png[/img][/url] ok"),
			[Span::Image { url: "https://x.org/a.png".into() }, text(" ok")]
		);
		assert_eq!(
			spans("[url][b]https://x.org[/b][/url]"),
			[styled("https://x.org", Style { bold: true, ..link("https://x.org") })]
		);
		assert_eq!(
			spans("[URL=ts3server://ts.example?port=9987]join[/URL]"),
			[styled("join", link("ts3server://ts.example?port=9987"))]
		);
		let doc = parse("[url=https://a.org]a[/url] https://b.org [url=https://a.org]again[/url]");
		assert_eq!(doc.links(), ["https://a.org", "https://b.org"]);
		assert!(is_web("HTTPS://x") && !is_web("ts3server://x") && !is_web("www.x"));
	}

	#[test]
	fn bare_addresses() {
		let link = |l: &str| Style { link: Some(l.into()), ..Default::default() };
		assert_eq!(
			spans("(see https://x.org)."),
			[text("(see "), styled("https://x.org", link("https://x.org")), text(").")]
		);
		assert_eq!(
			spans("www.example.com/a?b=1, then"),
			[
				styled("www.example.com/a?b=1", link("https://www.example.com/a?b=1")),
				text(", then")
			]
		);
		let wiki = "https://en.wikipedia.org/wiki/Foo_(bar)";
		assert_eq!(spans(wiki), [styled(wiki, link(wiki))]);
		// Not inside a word, and not without an address.
		assert_eq!(spans("xhttps://x.org"), [text("xhttps://x.org")]);
		assert_eq!(spans("go to www. or http:// now"), [text("go to www. or http:// now")]);
		// Not inside a link or code.
		assert_eq!(
			spans("[code]https://x.org[/code]")[0],
			styled("https://x.org", Style { code: true, ..Default::default() })
		);
	}

	#[test]
	fn case_and_nesting() {
		assert_eq!(spans("[B]x[/b]"), [styled("x", bold())]);
		let both = Style { bold: true, italic: true, ..Default::default() };
		let italic = Style { italic: true, ..Default::default() };
		assert_eq!(spans("[b]a[i]b[/i][/b]"), [styled("a", bold()), styled("b", both.clone())]);
		// Closed out of order: each closes its own.
		assert_eq!(
			spans("[b]a [i]b[/b] c[/i]"),
			[styled("a ", bold()), styled("b", both), styled(" c", italic)]
		);
		// Colours nest; the inner one counts.
		let red = Style { color: Some([0xff, 0, 0]), ..Default::default() };
		assert_eq!(
			spans("[color=red]a[color=#00f]b[/color]c[/color]"),
			[
				styled("a", red.clone()),
				styled("b", Style { color: Some([0, 0, 0xff]), ..Default::default() }),
				styled("c", red)
			]
		);
	}

	#[test]
	fn unknown_unclosed_and_stray() {
		assert_eq!(spans("[Nova] hi"), [text("[Nova] hi")]);
		assert_eq!(spans("[1] item"), [text("[1] item")]);
		assert_eq!(spans("a [not a tag here] b"), [text("a [not a tag here] b")]);
		assert_eq!(spans("[b"), [text("[b")]);
		assert_eq!(spans("[]"), [text("[]")]);
		// Open to the end.
		assert_eq!(spans("a [b]bold"), [text("a "), styled("bold", bold())]);
		// Closing nothing: text.
		assert_eq!(spans("a[/b] [/url] b"), [text("a[/b] [/url] b")]);
		// [*] outside a list is text.
		assert_eq!(spans("rate [*] it"), [text("rate [*] it")]);
		assert!(parse("[Nova] hi\n[1] item").is_plain());
	}

	#[test]
	fn code_is_literal() {
		let code = Style { code: true, ..Default::default() };
		assert_eq!(spans("run [code][b]x[/b][/code]"), [text("run "), styled("[b]x[/b]", code)]);
		let doc = parse("look:\n[code]\nfn main() {\n\t[b]\n}\n[/code]\nok");
		assert_eq!(
			doc.blocks,
			[
				line(vec![text("look:")]),
				Block { kind: BlockKind::Code, spans: vec![text("fn main() {\n\t[b]\n}")] },
				line(vec![text("ok")]),
			]
		);
		// Unclosed: to the end.
		assert_eq!(parse("[code]a\n[i]b").blocks[0].spans, [text("a\n[i]b")]);
	}

	#[test]
	fn lines_quotes_lists_rules() {
		let doc = parse("one\n\ntwo\n");
		assert_eq!(
			doc.blocks,
			[line(vec![text("one")]), line(Vec::new()), line(vec![text("two")])]
		);
		assert_eq!(doc.plain(), "one\n\ntwo");
		assert_eq!(doc.one_line(), "one two");

		let quote =
			|author: &str, spans| Block { kind: BlockKind::Quote { author: author.into() }, spans };
		let doc = parse("[quote=\"Mira\"]\nraid at 8\nbring elixirs[/quote]\nok!");
		assert_eq!(
			doc.blocks,
			[
				quote("Mira", vec![text("raid at 8")]),
				quote("", vec![text("bring elixirs")]),
				line(vec![text("ok!")]),
			]
		);
		assert_eq!(doc.plain(), "> Mira: raid at 8\n> bring elixirs\nok!");
		assert_eq!(parse("[QUOTE]x[/QUOTE]").blocks, [quote("", vec![text("x")])]);

		let bullet = |spans| Block { kind: BlockKind::Bullet, spans };
		let doc = parse("Bring:\n[list]\n[*] elixirs\n[*][b]food[/b]\n[/list]\nThanks");
		assert_eq!(
			doc.blocks,
			[
				line(vec![text("Bring:")]),
				bullet(vec![text("elixirs")]),
				bullet(vec![styled("food", bold())]),
				line(vec![text("Thanks")]),
			]
		);
		assert_eq!(doc.plain(), "Bring:\n• elixirs\n• food\nThanks");

		let doc = parse("a[hr]b");
		assert_eq!(
			doc.blocks,
			[line(vec![text("a")]), Block::new(BlockKind::Rule), line(vec![text("b")])]
		);
		assert_eq!(doc.plain(), "a\n---\nb");
		assert_eq!(doc.one_line(), "a b");
		// The text after a block tag starts at its first word.
		for source in ["[quote]x[/quote] ok", "[list][*]x[/list]  ok", "[hr] ok"] {
			let doc = parse(source);
			assert_eq!(doc.blocks.last().unwrap(), &line(vec![text("ok")]), "{source}");
		}
		// Not code's.
		let code = Style { code: true, ..Default::default() };
		assert_eq!(parse("[hr][code]  x[/code]").blocks[1].spans, [styled("  x", code)]);

		// A table: a line per row, cells apart.
		let doc = parse(
			"[table][tr][th]Name[/th][th]Level[/th][/tr]\n[tr][td]Nova[/td][td]42[/td][/tr][/table]",
		);
		assert_eq!(doc.blocks, [line(vec![text("Name Level")]), line(vec![text("Nova 42")])]);
		assert!(doc.is_plain());
	}

	#[test]
	fn pictures() {
		let doc = parse("look [img]https://x.org/a.png[/img] here");
		assert_eq!(
			doc.blocks[0].spans,
			[text("look "), Span::Image { url: "https://x.org/a.png".into() }, text(" here")]
		);
		assert_eq!(doc.images(), ["https://x.org/a.png"]);
		assert_eq!(doc.one_line(), "look https://x.org/a.png here");
		assert!(!doc.is_plain());
		// Only from the web or a channel's files.
		assert_eq!(spans("[img]javascript:x[/img]"), [text("javascript:x")]);
		assert_eq!(
			parse("[IMG]ts3file://a.png?channel=2[/IMG]").images(),
			["ts3file://a.png?channel=2"]
		);
	}

	#[test]
	fn colours() {
		assert_eq!(color("#f00"), Some([0xff, 0, 0]));
		assert_eq!(color("#00FF7f"), Some([0, 0xff, 0x7f]));
		assert_eq!(color("\"Red\""), Some([0xff, 0, 0]));
		assert_eq!(color("darkorange"), Some([0xff, 0x8c, 0]));
		for garbage in ["#zzz", "#12345", "", "#", "rgb(1,2,3)", "nocolour"] {
			assert_eq!(color(garbage), None, "{garbage}");
		}
		// A colour that is not one: the text stays, without one.
		assert_eq!(spans("[color=#zz]x[/color] y"), [text("x y")]);
	}

	#[test]
	fn readable_on_both_themes() {
		let dark_bg = [0x0b, 0x13, 0x30];
		let light_bg = [0xf7, 0xf9, 0xfe];
		for rgb in [[0, 0, 0], [0, 0, 0x80], [0xff, 0, 0], [0xff, 0xff, 0], [0xff; 3], [0x80; 3]] {
			let dark = readable(rgb, true);
			let light = readable(rgb, false);
			assert!(contrast(dark, dark_bg) >= 4.5, "{rgb:?} → {dark:?}");
			assert!(contrast(light, light_bg) >= 4.5, "{rgb:?} → {light:?}");
		}
		// Colours that read stay as they are.
		assert_eq!(readable([0xff, 0xd7, 0x00], true), [0xff, 0xd7, 0x00]);
		assert_eq!(readable([0x00, 0x00, 0x80], false), [0x00, 0x00, 0x80]);
		// Red stays reddish.
		let red = readable([0xff, 0, 0], false);
		assert!(red[0] > red[1] && red[0] > red[2], "{red:?}");
	}

	#[test]
	fn caps_fall_back_to_plain_text() {
		let deep = format!("{}x", "[b]".repeat(MAX_DEPTH + 1));
		let doc = parse(&deep);
		assert!(doc.is_plain());
		assert_eq!(doc.plain(), deep);
		assert!(!parse(&format!("{}x", "[b]".repeat(MAX_DEPTH))).is_plain());
		let many = "[b]a[/b] ".repeat(MAX_RUNS);
		assert!(parse(&many).is_plain());
		assert!(!parse(&"[b]a[/b] ".repeat(MAX_RUNS / 2 - 1)).is_plain());
	}

	/// Brackets that open no tag and tags read as text are read once: a
	/// long message of them is quick, and reads as before.
	#[test]
	fn long_texts_of_brackets() {
		let brackets = "[ ".repeat(20_000);
		assert_eq!(parse(&brackets).plain(), brackets);
		let links = "[url]a[/url]".repeat(5_000);
		assert_eq!(parse(&links).plain(), "a".repeat(5_000));
		let code = "[code]a[/code] ".repeat(MAX_RUNS);
		assert!(parse(&code).is_plain(), "past the cap");
		// `[url]address[/url]` is on one line; `[code]` may have more.
		assert_eq!(parse("[url]https://x.org\n[/url]").links(), ["https://x.org"]);
		assert_eq!(parse("[code]x\n[/CODE]").blocks[0].kind, BlockKind::Code);
	}

	#[test]
	fn retain_leaves_out_spans_and_their_lines() {
		let mut doc = parse(
			"here you go [URL=ts3file://x]plan.pdf[/URL] !\n[url=ts3file://y]y.png[/url]\nbye",
		);
		doc.retain(|s| !matches!(s, Span::Text { style: Style { link: Some(l), .. }, .. } if l.starts_with("ts3file")));
		assert_eq!(doc.blocks, [line(vec![text("here you go !")]), line(vec![text("bye")])]);
		assert_eq!(doc.plain(), "here you go !\nbye");
	}
}
