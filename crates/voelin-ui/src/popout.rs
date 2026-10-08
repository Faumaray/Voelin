//! The stream we watch in a window of its own (desktop): the player alone,
//! optionally over the other windows, while the main window goes on. Its
//! Bridge global is filled like the main window's (`viewer_each`); the
//! pictures go only to the window that shows the player, as each window
//! scales them again. Closing it brings the stream back into the main
//! window.

use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};
use tracing::warn;

use crate::app::{App, Bridge, Page, ViewerWindow, later};

/// The window, while there is one.
#[derive(Default)]
pub(crate) struct Popout {
	pub window: Option<ViewerWindow>,
	/// The last window stayed over the others: the next does too.
	on_top: bool,
}

impl App {
	/// `f` on the Bridge of the main window and of the viewer's window.
	pub(crate) fn viewer_each(&self, f: impl Fn(&Bridge)) {
		if let Some(ui) = self.ui.upgrade() {
			f(&ui.global::<Bridge>());
		}
		if let Some(window) = &self.popout.window {
			f(&window.global::<Bridge>());
		}
	}

	/// `f` on the Bridge and the window that show the player: the viewer's
	/// own while it has one, else the main window.
	pub(crate) fn with_player(&self, f: impl FnOnce(&Bridge, &slint::Window)) {
		match (&self.popout.window, self.ui.upgrade()) {
			(Some(window), _) => f(&window.global::<Bridge>(), window.window()),
			(None, Some(ui)) => f(&ui.global::<Bridge>(), ui.window()),
			_ => {}
		}
	}

	/// The player into a window of its own.
	pub(crate) fn viewer_pop_out(&mut self) {
		// One window on Android.
		if cfg!(target_os = "android") || self.popout.window.is_some() || self.watch.is_none() {
			return;
		}
		// The main window's full screen was the player's.
		self.set_fullscreen(false);
		let window = match ViewerWindow::new() {
			Ok(window) => window,
			Err(e) => {
				self.set_status(format!("Cannot open the stream's window: {e}"));
				return;
			}
		};
		let bridge = window.global::<Bridge>();
		crate::bind::streams::wire(&bridge);
		self.models.attach(&bridge);
		bridge.set_desktop(true);
		bridge.set_viewer_on_top(self.popout.on_top);
		window.window().on_close_requested(|| {
			// After this event: the window goes away then.
			later(|app| app.viewer_dock());
			slint::CloseRequestResponse::HideWindow
		});
		// Without its window the stream stays in the main window.
		if let Err(e) = window.show() {
			warn!("cannot show the stream's window: {e}");
			self.set_status(format!("Cannot open the stream's window: {e}"));
			return;
		}
		// The picture moves with the player.
		if let Some(ui) = self.ui.upgrade() {
			let main = ui.global::<Bridge>();
			bridge.set_viewer_frame(main.get_viewer_frame());
			main.set_viewer_frame(slint::Image::default());
		}
		self.popout.window = Some(window);
		self.theme_windows();
		self.refresh_viewer();
		// Our channel's people no longer lead the members panel.
		self.refresh_tree();
	}

	/// The player back into the main window, on the stream's server page.
	pub(crate) fn viewer_dock(&mut self) {
		let Some(window) = self.close_popout() else { return };
		if let Some(watch) = &mut self.watch {
			watch.shown = true;
		}
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_viewer_frame(window.global::<Bridge>().get_viewer_frame());
		}
		// The main window may have gone to another server meanwhile: else
		// the stream would play on, unseen.
		let session =
			self.watch.as_ref().and_then(|w| w.session).filter(|id| self.sessions.contains_key(id));
		if let Some(id) = session
			&& self.current != Some(id)
		{
			self.select_server(id);
		}
		self.refresh_viewer();
		self.refresh_streams();
		self.refresh_tree();
		if self.watch_here() {
			// Showing the page runs Nav code that calls back into the app.
			self.navigate(|nav| nav.invoke_show(Page::Server));
		}
	}

	/// Hide the viewer's window and forget it (leaving the stream, docking).
	pub(crate) fn close_popout(&mut self) -> Option<ViewerWindow> {
		let window = self.popout.window.take()?;
		self.popout.on_top = window.global::<Bridge>().get_viewer_on_top();
		let _ = window.hide();
		Some(window)
	}

	/// The pointer over the player's window shows or hides (it hides with
	/// the controls in full screen).
	pub(crate) fn viewer_cursor(&self, shown: bool) {
		self.with_player(|_, window| crate::streams::show_cursor(window, shown));
	}

	/// A picture of the viewer's window (screenshots, dev.rs).
	pub(crate) fn viewer_snapshot(&self) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
		self.popout.window.as_ref()?.window().take_snapshot().ok()
	}

	/// The main window closes: the studio's and the stream's windows go
	/// with it, so the app ends.
	pub(crate) fn close_windows(&mut self) {
		if let Some(window) = self.studio.window.take() {
			let _ = window.hide();
		}
		self.close_popout();
	}
}
