//! Avatars without pictures: initials on a colour from the name.

use slint::Color;
use unicode_segmentation::UnicodeSegmentation;

/// Background colours for initials; white text reads on all of them.
const TINTS: [u32; 10] = [
	0x2d6bff, 0x7c4dff, 0xc2379a, 0xe0572e, 0xd08a00, 0x1f9d6b, 0x0c93b8, 0x5865f2, 0xa0439e,
	0x3a7d44,
];

/// Up to two letters: the first letters of the first two words, or the
/// first two of a single word ("Nova Star" → "NS", "dex" → "DE").
pub fn initials(name: &str) -> String {
	let words: Vec<&str> = name
		.split(|c: char| c.is_whitespace() || c == '_' || c == '-' || c == '.')
		.filter(|w| w.chars().any(char::is_alphanumeric))
		.collect();
	let first = |w: &str| w.chars().find(|c| c.is_alphanumeric());
	let letters: String = match words.as_slice() {
		[] => String::new(),
		[one] => one.chars().filter(|c| c.is_alphanumeric()).take(2).collect(),
		[a, b, ..] => first(a).into_iter().chain(first(b)).collect(),
	};
	letters.to_uppercase()
}

/// The first letter of `initials`, for avatars too small for two ("NS" →
/// "N").
pub fn first_letter(initials: &str) -> String {
	initials.graphemes(true).next().unwrap_or_default().to_owned()
}

/// A picture from the engine's cache (an avatar, a group icon), decoded
/// once; an empty image when there is none.
pub fn image(path: Option<&std::path::PathBuf>) -> slint::Image {
	path.map(|p| crate::images::file(p)).unwrap_or_default()
}

/// A stable number for a name, in any case (FNV-1a).
pub fn name_hash(name: &str) -> u32 {
	let mut hash: u32 = 0x811c_9dc5;
	for b in name.to_lowercase().bytes() {
		hash ^= u32::from(b);
		hash = hash.wrapping_mul(0x0100_0193);
	}
	hash
}

/// `0xrrggbb` as a colour.
pub fn rgb(rgb: u32) -> Color {
	Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// A stable colour for a name.
pub fn tint(name: &str) -> Color {
	rgb(TINTS[(name_hash(name) % TINTS.len() as u32) as usize])
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn initials_of_names() {
		assert_eq!(initials("Nova Star"), "NS");
		assert_eq!(initials("dex"), "DE");
		assert_eq!(initials("x"), "X");
		assert_eq!(initials("dark_knight"), "DK");
		assert_eq!(initials("  "), "");
		assert_eq!(initials("[Bot] Relay"), "BR");
		assert_eq!(initials("Ärger über"), "ÄÜ");
	}

	#[test]
	fn first_letter_of_initials() {
		assert_eq!(first_letter("NS"), "N");
		assert_eq!(first_letter("ÄÜ"), "Ä");
		assert_eq!(first_letter(""), "");
		assert_eq!(first_letter("X"), "X");
		// A letter with a combining mark stays whole.
		assert_eq!(first_letter("A\u{308}B"), "A\u{308}");
	}

	#[test]
	fn tints_are_stable() {
		assert_eq!(tint("Alice"), tint("alice"));
		let distinct: std::collections::HashSet<_> =
			["Alice", "Bob", "Carol", "Dave", "Eve", "Mallory"]
				.iter()
				.map(|n| tint(n).as_argb_encoded())
				.collect();
		assert!(distinct.len() > 2);
	}
}
