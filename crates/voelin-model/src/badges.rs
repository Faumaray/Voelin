//! myTeamSpeak badges: what the GUIDs a client shows (`client_badges`)
//! stand for, and where TeamSpeak keeps their pictures.
//!
//! The table is `data/badges.csv`: tsclientlib's list (2023, also in
//! `proto/tsproto-structs/declarations`) and the newer badges of
//! TeamSpeak's own list (`https://badges-content.teamspeak.com/list`, a
//! protobuf) as of October 2026. Badges added since are unknown here.

use std::collections::HashMap;
use std::sync::LazyLock;

/// How many badges a client shows at most (TeamSpeak lets one pick three).
pub const SHOWN: usize = 3;

/// TeamSpeak's server for the badges' pictures.
const CONTENT: &str = "https://badges-content.teamspeak.com";

/// One badge of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadgeInfo {
	pub name: String,
	pub description: String,
	/// Its picture's name on TeamSpeak's server, without `.svg`.
	pub filename: String,
}

/// The table by GUID (lower case), read once.
static BADGES: LazyLock<HashMap<String, BadgeInfo>> = LazyLock::new(|| {
	// Columns: uid, name, description, filename, codes.
	records(include_str!("../data/badges.csv"))
		.into_iter()
		.skip(1)
		.filter_map(|mut fields| {
			fields.truncate(4);
			let [uid, name, description, filename] = <[String; 4]>::try_from(fields).ok()?;
			Some((uid.to_ascii_lowercase(), BadgeInfo { name, description, filename }))
		})
		.collect()
});

/// What a badge GUID stands for; none for one the table does not know.
pub fn info(guid: &str) -> Option<&'static BadgeInfo> {
	BADGES.get(&guid.to_ascii_lowercase())
}

/// The address of a badge's picture (an SVG); none for an unknown badge.
pub fn icon_url(guid: &str) -> Option<String> {
	let info = info(guid)?;
	Some(format!("{CONTENT}/{}/{}.svg", guid.to_ascii_lowercase(), info.filename))
}

/// The records of a CSV text (RFC 4180): fields split at commas; a quoted
/// field may hold commas and line breaks, and `""` for a quote.
fn records(text: &str) -> Vec<Vec<String>> {
	let mut records = Vec::new();
	let mut record = Vec::new();
	let mut field = String::new();
	let mut quoted = false;
	let mut chars = text.chars().peekable();
	while let Some(c) = chars.next() {
		match c {
			'"' if quoted => {
				if chars.next_if_eq(&'"').is_some() {
					field.push('"');
				} else {
					quoted = false;
				}
			}
			'"' if field.is_empty() => quoted = true,
			',' if !quoted => record.push(std::mem::take(&mut field)),
			'\r' if !quoted => {}
			'\n' if !quoted => {
				record.push(std::mem::take(&mut field));
				records.push(std::mem::take(&mut record));
			}
			c => field.push(c),
		}
	}
	if !field.is_empty() || !record.is_empty() {
		record.push(field);
		records.push(record);
	}
	records
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn known_badges() {
		let info = info("4b27be5a-b92a-4b30-8b2d-14b59653f427").unwrap();
		assert_eq!(info.name, "20th Anniversary");
		assert_eq!(info.description, "Celebrating 20 Years of TeamSpeak");
		assert_eq!(info.filename, "20_years");
		// Upper case on the wire finds it too.
		assert_eq!(super::info("4B27BE5A-B92A-4B30-8B2D-14B59653F427"), Some(info));
		assert_eq!(
			icon_url("4b27be5a-b92a-4b30-8b2d-14b59653f427").as_deref(),
			Some(
				"https://badges-content.teamspeak.com/4b27be5a-b92a-4b30-8b2d-14b59653f427/20_years.svg"
			)
		);
	}

	#[test]
	fn quoted_fields_keep_their_commas() {
		let fools = info("05114019-6b46-4b13-b5a1-e5179ef69fb5").unwrap();
		assert_eq!(fools.name, "April Fools!");
		assert_eq!(
			fools.description,
			"Roses are red, gaming is fun, you are carrying too much to be able to run :("
		);
		assert_eq!(fools.filename, "rpg");
		let stay = info("7a627d47-5496-4d68-83b5-2c4eafff9b30").unwrap();
		assert_eq!(stay.name, "Stay Home, Stay Safe");
		assert_eq!(stay.description, "Playing Apart, Staying Connected");
		let up = info("0cd924ed-c5ea-459e-b60a-4f1bc0b65f07").unwrap();
		assert_eq!(up.filename, "up,_up_and_away!");
	}

	#[test]
	fn unknown_badges() {
		assert_eq!(info("00000000-0000-0000-0000-000000000000"), None);
		assert_eq!(info(""), None);
		assert_eq!(icon_url("00000000-0000-0000-0000-000000000000"), None);
	}

	#[test]
	fn every_row_is_read() {
		let rows = include_str!("../data/badges.csv").lines().count() - 1;
		assert_eq!(BADGES.len(), rows);
		assert!(BADGES.values().all(|b| !b.name.is_empty() && !b.filename.is_empty()));
	}

	#[test]
	fn csv_records() {
		assert_eq!(
			records("a,\"b, \"\"c\"\"\",\r\n\"multi\nline\",d"),
			[vec!["a", "b, \"c\"", ""], vec!["multi\nline", "d"]]
		);
		assert!(records("").is_empty());
	}
}
