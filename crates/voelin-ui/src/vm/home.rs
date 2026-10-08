//! Home's "Continue where you left off": the voice channel we were in last,
//! if it can still be reached.

use voelin_core::VoiceState;
use voelin_store::Bookmark;

use crate::settings::LastVoice;

/// What the card's button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeAction {
	/// Voice is connected there: show the server.
	Open,
	/// Connect into the channel.
	Join,
}

/// The card: the server, the channel and the button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resume {
	pub bookmark: i64,
	/// The channel's path, its names from the top as the server has them.
	pub path: Vec<String>,
	pub action: ResumeAction,
}

impl Resume {
	/// The channel's name as the tree shows it (a spacer's text).
	pub fn title(&self) -> &str {
		match self.path.as_slice() {
			[top] => voelin_model::parse_spacer(top).map_or(top.as_str(), |(_, text)| text),
			[.., last] => last,
			[] => "",
		}
	}
}

/// Where to continue: the last voice channel, unless its bookmark is gone
/// or points at another address now. Open while `voice` says a bookmark is
/// connected, else Join.
pub fn resume(
	last: Option<&LastVoice>,
	bookmarks: &[Bookmark],
	voice: impl Fn(i64) -> VoiceState,
) -> Option<Resume> {
	let last = last.filter(|l| !l.channel.is_empty())?;
	bookmarks.iter().find(|b| b.id == last.bookmark && b.address == last.address)?;
	let action = match voice(last.bookmark) {
		VoiceState::Connected => ResumeAction::Open,
		_ => ResumeAction::Join,
	};
	Some(Resume { bookmark: last.bookmark, path: last.channel.clone(), action })
}

#[cfg(test)]
mod tests {
	use super::*;

	fn bookmark(id: i64, address: &str) -> Bookmark {
		Bookmark { id, address: address.into(), ..Default::default() }
	}

	fn last(bookmark: i64, address: &str, channel: &[&str]) -> LastVoice {
		LastVoice {
			bookmark,
			address: address.into(),
			channel: channel.iter().map(|n| n.to_string()).collect(),
		}
	}

	#[test]
	fn resume_cases() {
		let bookmarks = [bookmark(1, "ts.example"), bookmark(2, "pixel.example:9988")];
		let offline = |_| VoiceState::Disconnected;
		assert_eq!(resume(None, &bookmarks, offline), None, "nothing stored");
		assert_eq!(
			resume(Some(&last(3, "gone.example", &["Lobby"])), &bookmarks, offline),
			None,
			"the bookmark was deleted"
		);
		assert_eq!(
			resume(Some(&last(1, "old.example", &["Lobby"])), &bookmarks, offline),
			None,
			"the bookmark has another address now"
		);
		assert_eq!(resume(Some(&last(1, "ts.example", &[])), &bookmarks, offline), None);

		let place = last(2, "pixel.example:9988", &["Games", "Chess"]);
		let joined = resume(Some(&place), &bookmarks, offline).unwrap();
		assert_eq!(joined.bookmark, 2);
		assert_eq!(joined.path, ["Games", "Chess"]);
		assert_eq!(joined.action, ResumeAction::Join);
		assert_eq!(joined.title(), "Chess");
		let connecting = resume(Some(&place), &bookmarks, |_| VoiceState::Connecting);
		assert_eq!(connecting.unwrap().action, ResumeAction::Join);
		// Connected elsewhere is not connected there.
		let connected = |id| if id == 2 { VoiceState::Connected } else { VoiceState::Disconnected };
		assert_eq!(resume(Some(&place), &bookmarks, connected).unwrap().action, ResumeAction::Open);
		let elsewhere = |id| if id == 1 { VoiceState::Connected } else { VoiceState::Disconnected };
		assert_eq!(resume(Some(&place), &bookmarks, elsewhere).unwrap().action, ResumeAction::Join);
	}

	#[test]
	fn resume_titles() {
		let title = |path: &[&str]| {
			let path = path.iter().map(|n| n.to_string()).collect();
			Resume { bookmark: 1, path, action: ResumeAction::Join }.title().to_owned()
		};
		assert_eq!(title(&["Lobby"]), "Lobby");
		assert_eq!(title(&["[cspacer0]Gaming"]), "Gaming", "a spacer by its text");
		// Only channels at the top are spacers.
		assert_eq!(title(&["Games", "[cspacer]Chess"]), "[cspacer]Chess");
	}
}
