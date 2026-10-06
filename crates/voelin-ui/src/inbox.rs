//! Requests from the platform around the window: on Android a tapped
//! notification or something shared to the app ("Share to Voelin"). They
//! can come from any thread and before the window runs; [`request`] keeps
//! them until the window takes them on its thread.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use slint::ComponentHandle;
use voelin_model::ChatTarget;

use crate::app::{App, MobileTab, Nav, Page, with_app};

/// What the platform asks the window to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
	/// Show the voice channel we are in (the phone's voice screen).
	ShowVoice,
	/// Shared to the app: the text goes into the current chat's composer
	/// (to send with one tap, or edit, as a mis-tap in the share sheet
	/// should not post into a chat), the files are uploaded to the current
	/// channel and linked there, as the composer's attach button does.
	Share { text: Option<String>, files: Vec<PathBuf> },
}

static PENDING: Mutex<Vec<Request>> = Mutex::new(Vec::new());

/// Hand `request` to the window: at once if it runs, else when it starts.
pub fn request(request: Request) {
	PENDING.lock().unwrap_or_else(PoisonError::into_inner).push(request);
	// Fails without an event loop yet; `run` takes them then.
	let _ = slint::invoke_from_event_loop(take);
}

/// Handle what is pending, on the window's thread; kept while there is no
/// window.
pub(crate) fn take() {
	let Some(ui) = with_app(|app| app.ui.upgrade()).flatten() else { return };
	let requests = std::mem::take(&mut *PENDING.lock().unwrap_or_else(PoisonError::into_inner));
	// Not inside `with_app`: navigating calls back into Rust, which borrows
	// the app again.
	let nav = ui.global::<Nav>();
	for request in requests {
		match request {
			Request::ShowVoice => {
				nav.invoke_show(Page::Server);
				nav.invoke_show_voice(true);
			}
			Request::Share { text, files } => {
				// The phone shows the chat it went to.
				nav.set_mobile_tab(MobileTab::Chat);
				nav.invoke_show(Page::Server);
				with_app(|app| app.share(&nav, text, files));
			}
		}
	}
}

impl App {
	fn share(&mut self, nav: &Nav<'_>, text: Option<String>, files: Vec<PathBuf>) {
		let current = self.current.and_then(|id| {
			let view = self.sessions.get(&id)?;
			let tab = &view.tabs[view.current_tab];
			// Files go to the channel of the chat, else to ours.
			let channel = match tab.target {
				ChatTarget::Channel(channel) => Some(channel),
				_ => view.state.own_channel,
			}
			.map(|c| {
				let name =
					view.presence.channels.get(&c).map(|c| crate::vm::tree::channel_title(c).0);
				(c, name.map_or_else(|| "the channel".to_owned(), |n| format!("#{n}")))
			});
			Some((id, tab.title.clone(), channel))
		});
		if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
			nav.set_draft(text.into());
			self.set_status(match &current {
				Some((_, title, _)) => format!("Shared text is ready to send to {title}"),
				None => "Shared text is in the composer".to_owned(),
			});
		}
		if files.is_empty() {
			return;
		}
		match current {
			Some((id, _, Some((channel, name)))) if !self.demo_ui => {
				let count = files.len();
				for file in files {
					self.upload(id, channel, file);
				}
				let what = if count == 1 {
					"the shared file".to_owned()
				} else {
					format!("{count} files")
				};
				self.set_status(format!("Uploading {what} to {name}"));
			}
			_ => self.set_status("Open a channel's chat (or join a channel) to share files there"),
		}
	}
}
