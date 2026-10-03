//! Pictures linked in chat, shown inline: a link to a PNG, JPEG, GIF or
//! WebP file (by name) up to `ui.image_preview_kb` is downloaded into
//! memory once (`Command::DownloadChatFile` with `DownloadTo::Memory`,
//! voice connections only) and decoded once into the image cache
//! (`images.rs`); the chat shows it as a picture card, and a click opens
//! it larger.

use voelin_core::{Bytes, Command, DownloadTo, TransferState, VoiceState};
use voelin_model::{ChatTarget, FileRef};

use crate::app::{App, SessionView};
use crate::settings::UI_IMAGE_PREVIEW_KB;

/// A linked picture.
#[derive(Clone, Debug)]
pub(crate) enum Preview {
	/// Downloading (transfer id).
	Loading(u64),
	Ready(Bytes),
	/// Too large, not a picture, or the download failed.
	Failed,
}

/// Whether a file name is a picture the chat shows inline.
pub(crate) fn is_previewable(name: &str) -> bool {
	let lower = name.to_ascii_lowercase();
	[".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| lower.ends_with(e))
}

/// The key of a link: where the file is.
pub(crate) fn link_key(file: &FileRef) -> String {
	format!(
		"{}/{}{}{}",
		file.server_uid.as_deref().unwrap_or(""),
		file.channel,
		file.path,
		file.name
	)
}

/// The decoded picture of a link, if it is there (decoded once, kept by
/// the image cache).
pub(crate) fn image_of(view: &SessionView, file: &FileRef) -> Option<slint::Image> {
	let key = link_key(file);
	match view.extra.previews.get(&key)? {
		Preview::Ready(bytes) => {
			let image = crate::images::picture(&key, &bytes.0);
			(image.size().width > 0).then_some(image)
		}
		_ => None,
	}
}

impl App {
	/// Download the pictures linked in the current chat of `session` that
	/// are not there yet.
	pub(crate) fn fetch_previews(&mut self, session: i64) {
		if self.demo_ui {
			return;
		}
		let limit = u64::from(self.prefs.get(&UI_IMAGE_PREVIEW_KB)) * 1024;
		let Some(view) = self.sessions.get_mut(&session) else { return };
		if limit == 0
			|| view.state.voice != VoiceState::Connected
			|| !view.capabilities.file_transfer
		{
			return;
		}
		let Some(tab) = view.tabs.get(view.current_tab) else { return };
		// Channel chats only: a link names its channel's file browser.
		if matches!(tab.target, ChatTarget::Private(_)) && tab.messages.is_empty() {
			return;
		}
		let wanted: Vec<FileRef> = tab
			.messages
			.iter()
			.rev()
			.take(200)
			.flat_map(|m| m.message.message.file_refs())
			.filter(|f| is_previewable(&f.name) && f.size.is_some_and(|s| s <= limit))
			.filter(|f| !view.extra.previews.contains_key(&link_key(f)))
			.collect();
		for file in wanted {
			let transfer = view.next_transfer;
			view.next_transfer += 1;
			view.extra.previews.insert(link_key(&file), Preview::Loading(transfer));
			self.engine.send(Command::DownloadChatFile {
				session: session as u64,
				transfer,
				file,
				password: None,
				to: DownloadTo::Memory,
			});
		}
	}

	/// A transfer of a picture: `true` if it was one.
	pub(crate) fn preview_progress(
		&mut self,
		session: i64,
		transfer: u64,
		state: &TransferState,
	) -> bool {
		let limit = u64::from(self.prefs.get(&UI_IMAGE_PREVIEW_KB)) * 1024;
		let Some(view) = self.sessions.get_mut(&session) else { return false };
		let Some(key) = view
			.extra
			.previews
			.iter()
			.find(|(_, p)| matches!(p, Preview::Loading(t) if *t == transfer))
			.map(|(k, _)| k.clone())
		else {
			return false;
		};
		match state {
			TransferState::Started { size, .. } if *size > limit => {
				view.extra.previews.insert(key, Preview::Failed);
				if !self.demo_ui {
					self.engine.send(Command::CancelTransfer { session: session as u64, transfer });
				}
			}
			TransferState::Done { data: Some(data), .. } => {
				view.extra.previews.insert(key, Preview::Ready(data.clone()));
				if self.current == Some(session) {
					self.refresh_chat();
				}
			}
			TransferState::Done { .. } | TransferState::Failed(_) | TransferState::Cancelled => {
				view.extra.previews.insert(key, Preview::Failed);
			}
			_ => {}
		}
		true
	}

	/// The picture of link `index` of a message of the current chat, for
	/// the large view.
	pub(crate) fn open_preview(&self, key: i32, index: i32) -> slint::Image {
		let Some(view) = self.view() else { return slint::Image::default() };
		let Some(tab) = view.tabs.get(view.current_tab) else { return slint::Image::default() };
		tab.message(key)
			.and_then(|m| m.message.file_refs().into_iter().nth(index.max(0) as usize))
			.and_then(|f| image_of(view, &f))
			.unwrap_or_default()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn previewable_names() {
		assert!(is_previewable("Banner.PNG"));
		assert!(is_previewable("a.webp"));
		assert!(!is_previewable("notes.svg"));
		assert!(!is_previewable("route.pdf"));
		let file =
			FileRef { channel: 2, path: "/".into(), name: "a.png".into(), ..Default::default() };
		assert_eq!(link_key(&file), "/2/a.png");
	}
}
