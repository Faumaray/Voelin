//! Requests from the platform around the window: on Android a tapped
//! notification. They can come from any thread and before the window runs;
//! [`request`] keeps them until the window takes them on its thread.

use std::sync::{Mutex, PoisonError};

use slint::ComponentHandle;

use crate::app::{Nav, Page, with_app};

/// What the platform asks the window to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
	/// Show the voice channel we are in (the phone's voice screen).
	ShowVoice,
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
		}
	}
}
