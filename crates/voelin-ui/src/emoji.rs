//! Twemoji as images. The software renderer draws no colour glyphs, so the
//! UI shows emoji as pictures: this module reads the packed archive
//! (`assets/twemoji.bin`, made by `scripts/pack-twemoji.py`), finds the
//! picture of a grapheme cluster, splits text into runs of text and emoji,
//! and lists emoji for the picker. Decoding the SVGs into images is
//! `images.rs`' job.

use std::sync::{Arc, Mutex, OnceLock};

use unicode_segmentation::UnicodeSegmentation;

static DATA: &[u8] = include_bytes!("../assets/twemoji.bin");

const MAGIC: &[u8; 8] = b"VTWEMOJ1";
const ENTRY_SIZE: usize = 28;

#[derive(Clone, Copy, Debug)]
struct Block {
	offset: u32,
	len: u32,
	raw_len: u32,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
	key: (u32, u16),
	category: u16,
	block: u32,
	offset: u32,
	len: u32,
	name: (u32, u16),
	picker: bool,
}

/// An emoji of the picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerEmoji {
	/// The archive key (`1f600`).
	pub key: &'static str,
	/// Lower-case name for search and screen readers.
	pub name: &'static str,
	/// The characters picking it inserts.
	pub text: String,
}

/// The packed Twemoji: an index sorted by key and deflate blocks.
pub struct Archive {
	entries: Vec<Entry>,
	blocks: Vec<Block>,
	strings: &'static [u8],
	data: &'static [u8],
	/// Picker entries (indices into `entries`) in picker order.
	picker: Vec<u32>,
	/// The last decompressed block: neighbours are often wanted together.
	last: Mutex<Option<(u32, Arc<[u8]>)>>,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
	Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
	Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

impl Archive {
	/// Parse the index of an archive; `None` if it is malformed.
	pub fn parse(bytes: &'static [u8]) -> Option<Self> {
		if bytes.get(..8)? != MAGIC {
			return None;
		}
		let count = u32_at(bytes, 8)? as usize;
		let block_count = u32_at(bytes, 12)? as usize;
		let strings_len = u32_at(bytes, 16)? as usize;
		let mut at = 20;
		let mut blocks = Vec::with_capacity(block_count);
		for _ in 0..block_count {
			blocks.push(Block {
				offset: u32_at(bytes, at)?,
				len: u32_at(bytes, at + 4)?,
				raw_len: u32_at(bytes, at + 8)?,
			});
			at += 12;
		}
		let mut entries = Vec::with_capacity(count);
		for _ in 0..count {
			entries.push(Entry {
				key: (u32_at(bytes, at)?, u16_at(bytes, at + 4)?),
				category: u16_at(bytes, at + 6)?,
				block: u32_at(bytes, at + 8)?,
				offset: u32_at(bytes, at + 12)?,
				len: u32_at(bytes, at + 16)?,
				name: (u32_at(bytes, at + 20)?, u16_at(bytes, at + 24)?),
				picker: u16_at(bytes, at + 26)? & 1 == 1,
			});
			at += ENTRY_SIZE;
		}
		let strings = bytes.get(at..at + strings_len)?;
		let data = bytes.get(at + strings_len..)?;
		let mut picker: Vec<u32> =
			(0..entries.len() as u32).filter(|&i| entries[i as usize].picker).collect();
		picker.sort_by_key(|&i| {
			let e = &entries[i as usize];
			(e.category, e.block, e.offset)
		});
		Some(Self { entries, blocks, strings, data, picker, last: Mutex::new(None) })
	}

	fn string(&self, (offset, len): (u32, u16)) -> &'static str {
		let (offset, len) = (offset as usize, len as usize);
		let strings: &'static [u8] = self.strings;
		strings.get(offset..offset + len).and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("")
	}

	fn find(&self, key: &str) -> Option<&Entry> {
		self.entries
			.binary_search_by(|e| self.string(e.key).cmp(key))
			.ok()
			.map(|i| &self.entries[i])
	}

	/// Number of emoji in the archive.
	#[cfg(test)]
	pub fn len(&self) -> usize {
		self.entries.len()
	}

	pub fn contains(&self, key: &str) -> bool {
		self.find(key).is_some()
	}

	/// The name of an emoji (lower case), if the archive has it.
	#[allow(dead_code, reason = "for screens that label emoji (reactions)")]
	pub fn name(&self, key: &str) -> Option<&'static str> {
		self.find(key).map(|e| self.string(e.name))
	}

	/// The SVG of an emoji.
	pub fn svg(&self, key: &str) -> Option<Vec<u8>> {
		let entry = *self.find(key)?;
		let block = self.block(entry.block)?;
		let (start, len) = (entry.offset as usize, entry.len as usize);
		block.get(start..start + len).map(<[u8]>::to_vec)
	}

	fn block(&self, index: u32) -> Option<Arc<[u8]>> {
		let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
		if let Some((i, block)) = last.as_ref()
			&& *i == index
		{
			return Some(block.clone());
		}
		let b = self.blocks.get(index as usize)?;
		let compressed = self.data.get(b.offset as usize..(b.offset + b.len) as usize)?;
		let raw =
			miniz_oxide::inflate::decompress_to_vec_with_limit(compressed, b.raw_len as usize)
				.ok()?;
		let raw: Arc<[u8]> = raw.into();
		*last = Some((index, raw.clone()));
		Some(raw)
	}

	fn picker_emoji(&self, entry: &Entry) -> PickerEmoji {
		let key = self.string(entry.key);
		PickerEmoji { key, name: self.string(entry.name), text: text_of(key) }
	}

	/// The picker's emoji of a category: smileys, people, nature, food,
	/// activities, travel, objects, symbols, flags (0-8).
	pub fn category(&self, category: usize) -> Vec<PickerEmoji> {
		self.picker
			.iter()
			.map(|&i| &self.entries[i as usize])
			.filter(|e| e.category as usize == category)
			.map(|e| self.picker_emoji(e))
			.collect()
	}

	/// Picker emoji whose name has every word of `query` (case-insensitive).
	pub fn search(&self, query: &str) -> Vec<PickerEmoji> {
		let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
		if words.is_empty() {
			return Vec::new();
		}
		self.picker
			.iter()
			.map(|&i| &self.entries[i as usize])
			.filter(|e| {
				let name = self.string(e.name);
				words.iter().all(|w| name.contains(w.as_str()))
			})
			.map(|e| self.picker_emoji(e))
			.collect()
	}
}

/// The archive in the binary.
pub fn archive() -> &'static Archive {
	static ARCHIVE: OnceLock<Archive> = OnceLock::new();
	ARCHIVE.get_or_init(|| Archive::parse(DATA).expect("the bundled emoji archive is valid"))
}

/// Characters shown as emoji even without U+FE0F: Emoji_Presentation=Yes
/// below U+1F000 (above it nearly everything is).
const PRESENTATION_BMP: &[(u32, u32)] = &[
	(0x231A, 0x231B),
	(0x23E9, 0x23EC),
	(0x23F0, 0x23F0),
	(0x23F3, 0x23F3),
	(0x25FD, 0x25FE),
	(0x2614, 0x2615),
	(0x2648, 0x2653),
	(0x267F, 0x267F),
	(0x2693, 0x2693),
	(0x26A1, 0x26A1),
	(0x26AA, 0x26AB),
	(0x26BD, 0x26BE),
	(0x26C4, 0x26C5),
	(0x26CE, 0x26CE),
	(0x26D4, 0x26D4),
	(0x26EA, 0x26EA),
	(0x26F2, 0x26F3),
	(0x26F5, 0x26F5),
	(0x26FA, 0x26FA),
	(0x26FD, 0x26FD),
	(0x2705, 0x2705),
	(0x270A, 0x270B),
	(0x2728, 0x2728),
	(0x274C, 0x274C),
	(0x274E, 0x274E),
	(0x2753, 0x2755),
	(0x2757, 0x2757),
	(0x2795, 0x2797),
	(0x27B0, 0x27B0),
	(0x27BF, 0x27BF),
	(0x2B1B, 0x2B1C),
	(0x2B50, 0x2B50),
	(0x2B55, 0x2B55),
];

fn emoji_presentation(cp: u32) -> bool {
	cp >= 0x1F000 || PRESENTATION_BMP.iter().any(|&(lo, hi)| (lo..=hi).contains(&cp))
}

/// Whether a character can start an emoji; text without any is plain.
fn may_be_emoji(c: char) -> bool {
	c as u32 >= 0x2000 || c == '\u{FE0F}' || c == '\u{20E3}' || c == '\u{a9}' || c == '\u{ae}'
}

fn key_of(cps: &[u32]) -> String {
	cps.iter().map(|cp| format!("{cp:x}")).collect::<Vec<_>>().join("-")
}

/// The characters of an archive key, with U+FE0F after a lone character
/// that is text by default, so other clients show an emoji too.
fn text_of(key: &str) -> String {
	let cps: Vec<u32> = key.split('-').filter_map(|p| u32::from_str_radix(p, 16).ok()).collect();
	let mut text: String = cps.iter().filter_map(|&cp| char::from_u32(cp)).collect();
	if cps.len() == 1 && !emoji_presentation(cps[0]) {
		text.push('\u{FE0F}');
	}
	text
}

/// The archive key of a grapheme cluster that is an emoji: code points in
/// hex joined by `-`, without U+FE0F unless the sequence has a ZWJ (as
/// Twemoji names its files).
pub fn key_for(grapheme: &str) -> Option<String> {
	let cps: Vec<u32> = grapheme.chars().map(|c| c as u32).collect();
	let first = *cps.first()?;
	let has_vs = cps.contains(&0xFE0F);
	let has_keycap = cps.contains(&0x20E3);
	// Text characters (digits, ©, ↔) are emoji only with U+FE0F or a keycap.
	if cps.len() == 1 && !emoji_presentation(first) {
		return None;
	}
	if first < 0x2000 && !has_vs && !has_keycap && first != 0xA9 && first != 0xAE {
		return None;
	}
	if (first == 0xA9 || first == 0xAE) && !has_vs {
		return None;
	}
	let archive = archive();
	let key = if cps.contains(&0x200D) {
		key_of(&cps)
	} else {
		key_of(&cps.iter().copied().filter(|&cp| cp != 0xFE0F).collect::<Vec<_>>())
	};
	if archive.contains(&key) {
		return Some(key);
	}
	// Some ZWJ sequences are named without their U+FE0F.
	let bare = key_of(&cps.iter().copied().filter(|&cp| cp != 0xFE0F).collect::<Vec<_>>());
	archive.contains(&bare).then_some(bare)
}

/// The archive key of the first emoji in `text` (a reaction's emoji, which
/// may be any string).
pub fn first_key(text: &str) -> Option<String> {
	text.graphemes(true).find_map(key_for)
}

/// A piece of text: plain, or one emoji (`emoji` is its key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
	pub text: String,
	pub emoji: Option<String>,
}

/// Text split into runs for a wrapping layout: a run per word (with the
/// space after it) and per emoji. `None` if the text has no emoji (show it
/// as one text). Line breaks become spaces.
pub fn runs(text: &str) -> Option<Vec<Run>> {
	if !text.chars().any(may_be_emoji) {
		return None;
	}
	let mut out: Vec<Run> = Vec::new();
	let mut word = String::new();
	let mut any = false;
	for g in text.graphemes(true) {
		if let Some(key) = key_for(g) {
			if !word.is_empty() {
				out.push(Run { text: std::mem::take(&mut word), emoji: None });
			}
			out.push(Run { text: g.to_owned(), emoji: Some(key) });
			any = true;
			continue;
		}
		let space = g.chars().all(char::is_whitespace);
		if space {
			// Spaces stay with the word before them (or stand alone after
			// an emoji).
			word.push(' ');
			out.push(Run { text: std::mem::take(&mut word), emoji: None });
		} else {
			word.push_str(g);
		}
	}
	if !word.is_empty() {
		out.push(Run { text: word, emoji: None });
	}
	any.then_some(out)
}

/// One to three emoji and nothing else but spaces: shown large.
pub fn is_jumbo(runs: &[Run]) -> bool {
	let emoji = runs.iter().filter(|r| r.emoji.is_some()).count();
	(1..=3).contains(&emoji) && runs.iter().all(|r| r.emoji.is_some() || r.text.trim().is_empty())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn archive_opens() {
		let a = archive();
		assert!(a.len() > 3000);
		assert!(a.contains("1f600"));
		assert!(!a.contains("zzz"));
		assert_eq!(a.name("1f600"), Some("grinning face"));
		let svg = a.svg("1f600").unwrap();
		assert!(svg.starts_with(b"<svg"), "{:?}", String::from_utf8_lossy(&svg[..20]));
		assert!(svg.ends_with(b"</svg>"));
		// Twice (the cached block).
		assert_eq!(a.svg("1f600").unwrap(), svg);
		let zwj = a.svg("1f468-200d-1f4bb").unwrap();
		assert!(zwj.starts_with(b"<svg"));
	}

	#[test]
	fn keys_of_graphemes() {
		assert_eq!(key_for("😀").as_deref(), Some("1f600"));
		// U+FE0F dropped outside ZWJ sequences.
		assert_eq!(key_for("❤\u{FE0F}").as_deref(), Some("2764"));
		// Keycap.
		assert_eq!(key_for("1\u{FE0F}\u{20E3}").as_deref(), Some("31-20e3"));
		// Skin tone.
		assert_eq!(key_for("👍🏽").as_deref(), Some("1f44d-1f3fd"));
		// ZWJ sequence.
		assert_eq!(key_for("👨\u{200D}💻").as_deref(), Some("1f468-200d-1f4bb"));
		// Flag.
		assert_eq!(key_for("🇩🇪").as_deref(), Some("1f1e9-1f1ea"));
		// Text stays text.
		assert_eq!(key_for("a"), None);
		assert_eq!(key_for("1"), None);
		assert_eq!(key_for("©"), None);
		assert_eq!(key_for("❤"), None);
		assert_eq!(key_for("—"), None);
	}

	#[test]
	fn text_runs() {
		assert_eq!(runs("plain text, no emoji"), None);
		assert_eq!(runs("em — dash"), None);
		let r = runs("hi 👋 there").unwrap();
		let texts: Vec<_> = r.iter().map(|r| (r.text.as_str(), r.emoji.as_deref())).collect();
		assert_eq!(texts, [("hi ", None), ("👋", Some("1f44b")), (" ", None), ("there", None)]);
		assert!(!is_jumbo(&r));
		let r = runs("🔥🔥 ").unwrap();
		assert!(is_jumbo(&r));
		assert!(!is_jumbo(&runs("🔥🔥🔥🔥").unwrap()));
	}

	#[test]
	fn picker() {
		let a = archive();
		let smileys = a.category(0);
		assert!(smileys.len() > 50);
		assert_eq!(smileys[0].key, "1f600");
		assert_eq!(smileys[0].text, "😀");
		assert!(a.category(1).iter().all(|e| !e.key.contains("1f3fb")), "no skin tone variants");
		let hearts = a.search("red heart");
		assert!(hearts.iter().any(|e| e.key == "2764" && e.text == "❤\u{FE0F}"));
		assert!(a.search("  ").is_empty());
	}
}
