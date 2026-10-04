//! The streams panel, the share dialog and the viewer.

use slint::ComponentHandle;

use crate::app::{Bridge, MainWindow, with_app};

pub(super) fn wire(ui: &MainWindow) {
	let bridge = ui.global::<Bridge>();
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
	bridge.on_set_stream_quality(|index| {
		with_app(|app| app.set_stream_quality(index));
	});
}
