//! The settings page, crash reports and About.

use slint::{ComponentHandle, SharedString};

use crate::app::{Bridge, MainWindow, with_app};

pub(super) fn wire(ui: &MainWindow) {
	let bridge = ui.global::<Bridge>();
	bridge.on_open_settings(|| {
		with_app(|app| app.open_settings());
	});
	bridge.on_close_settings(|| {
		with_app(|app| app.close_settings());
	});
	bridge.on_audio_changed(|form| {
		with_app(|app| app.audio_changed(&form));
	});
	bridge.on_toggle_mic_test(|| {
		with_app(|app| app.set_mic_test(!app.mic_test));
	});
	bridge.on_apply_ptt(|enabled, key| {
		with_app(|app| app.apply_ptt(enabled, key.to_string()));
	});
	bridge.on_set_h264(|enabled| {
		with_app(|app| app.set_h264(enabled));
	});
	bridge.on_download_h264(|| {
		with_app(|app| app.download_h264());
	});
	bridge.on_panel_resized(|width| {
		with_app(|app| app.panel_resized(width));
	});
	bridge.on_appearance_changed(|form| {
		with_app(|app| app.appearance_changed(&form));
	});
	bridge.on_set_crash_reports(|enabled| {
		with_app(|app| app.set_crash_reports(enabled));
	});
	bridge.on_open_crash_folder(|| {
		with_app(|app| app.open_crash_folder());
	});
	bridge.on_delete_crash_reports(|| {
		with_app(|app| app.delete_crash_reports());
	});
	bridge.on_dismiss_crash_notice(|| {
		with_app(|app| {
			if let Some(ui) = app.ui.upgrade() {
				ui.global::<Bridge>().set_crash_notice(SharedString::new());
			}
		});
	});
	bridge.on_open_about(|| {
		with_app(|app| app.load_notices());
	});
	bridge.on_use_identity(|id| {
		with_app(|app| app.use_identity(id as i64));
	});
	bridge.on_set_identity_import(|on| {
		with_app(|app| app.set_identity_import(on));
	});
}
