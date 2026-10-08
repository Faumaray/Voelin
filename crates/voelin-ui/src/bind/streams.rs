//! The streams panel, the share dialog and the viewer, for the main window
//! and for the viewer's own window (each has its own Bridge global).

use crate::app::{Bridge, with_app};

pub(crate) fn wire(bridge: &Bridge) {
	bridge.on_open_share(|| with_app(|app| app.open_share()).unwrap_or_default());
	bridge.on_start_share(|form| {
		with_app(|app| app.start_share(form));
	});
	bridge.on_stop_share(|| {
		with_app(|app| app.stop_share());
	});
	bridge.on_respond_viewer(|viewer, accept| {
		with_app(|app| app.respond_viewer(viewer as u16, accept));
	});
	bridge.on_kick_viewer(|viewer| {
		with_app(|app| app.kick_viewer(viewer as u16));
	});
	bridge.on_watch_stream(|id| {
		with_app(|app| app.watch_stream(id.to_string()));
	});
	bridge.on_watch_client(|client| {
		if let Ok(client) = u16::try_from(client) {
			with_app(|app| app.watch_client(client));
		}
	});
	bridge.on_show_viewer(|| {
		with_app(|app| app.show_viewer(true));
	});
	bridge.on_hide_viewer(|| {
		with_app(|app| app.show_viewer(false));
	});
	bridge.on_leave_stream(|| {
		with_app(|app| app.leave_stream());
	});
	bridge.on_set_stream_volume(|volume| {
		with_app(|app| app.set_stream_volume(volume));
	});
	bridge.on_toggle_fullscreen(|| {
		with_app(|app| app.toggle_fullscreen());
	});
	bridge.on_pop_out_viewer(|out| {
		with_app(|app| if out { app.viewer_pop_out() } else { app.viewer_dock() });
	});
	bridge.on_show_cursor(|shown| {
		with_app(|app| app.viewer_cursor(shown));
	});
	bridge.on_set_stream_quality(|index| {
		with_app(|app| app.set_stream_quality(index));
	});
}
