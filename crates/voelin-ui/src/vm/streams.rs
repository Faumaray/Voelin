//! Streams on the voice view, the phone's Activity tab and Home: what is
//! shared, and who shares it.

use std::path::PathBuf;

use voelin_core::stream::StreamKind;

use crate::app::StreamItem;
use crate::vm::avatar;

/// "Screen", "Window", "Camera"; "" for a kind we do not know.
pub fn kind_label(kind: &StreamKind) -> &'static str {
	match kind {
		StreamKind::Screen => "Screen",
		StreamKind::Window => "Window",
		StreamKind::Camera => "Camera",
		StreamKind::Other(_) => "",
	}
}

/// A gateway directory's kind as a label: "screen" → "Screen".
pub fn directory_kind_label(kind: &str) -> String {
	let mut chars = kind.chars();
	chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// A stream with its streamer: the name, and the avatar as the members
/// show it (the picture from the engine's cache, else the initials on the
/// name's colour). The caller fills in the rest.
pub fn streamer(name: &str, picture: Option<&PathBuf>) -> StreamItem {
	StreamItem {
		streamer: name.into(),
		initials: avatar::initials(name).into(),
		tint: avatar::tint(name),
		avatar: avatar::image(picture),
		..StreamItem::default()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn kinds_have_labels() {
		assert_eq!(kind_label(&StreamKind::Screen), "Screen");
		assert_eq!(kind_label(&StreamKind::Window), "Window");
		assert_eq!(kind_label(&StreamKind::Camera), "Camera");
		assert_eq!(kind_label(&StreamKind::Other(9)), "");
		assert_eq!(kind_label(&StreamKind::from_u8(2)), "Camera");
	}

	#[test]
	fn directory_kinds_are_capitalised() {
		assert_eq!(directory_kind_label("screen"), "Screen");
		assert_eq!(directory_kind_label("Camera"), "Camera");
		assert_eq!(directory_kind_label(""), "");
	}

	#[test]
	fn streamers_look_as_in_the_members() {
		let item = streamer("Nova Star", None);
		assert_eq!(item.streamer, "Nova Star");
		assert_eq!(item.initials, "NS");
		assert_eq!(item.tint, avatar::tint("Nova Star"));
		assert_eq!(item.avatar.size(), slint::Image::default().size());
		assert!(item.id.is_empty() && !item.own && !item.studio);
	}
}
