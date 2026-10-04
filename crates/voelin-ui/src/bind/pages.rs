//! The settings pages of the new design (settings_pages.rs).

use slint::ComponentHandle;

use crate::app::{Bridge, MainWindow, with_app};

pub(super) fn wire(ui: &MainWindow) {
	let bridge = ui.global::<Bridge>();
	bridge.on_settings_section(|section| {
		with_app(|app| app.settings_section(section));
	});
	// Profiles.
	bridge.on_identity_action(|id, action| {
		with_app(|app| app.identity_action(id, &action));
	});
	bridge.on_rename_identity(|id, name| {
		with_app(|app| app.rename_identity(id, &name));
	});
	bridge.on_improve_target(|id, target| {
		with_app(|app| app.improve_identity(id, target));
	});
	bridge.on_find_identities(|| {
		with_app(|app| app.find_identities());
	});
	bridge.on_pick_found(|index, on| {
		with_app(|app| app.pick_found(index, on));
	});
	bridge.on_import_identities(|| {
		with_app(|app| app.import_identities());
	});
	bridge.on_bookmark_identity(|bookmark, index| {
		with_app(|app| app.bookmark_identity(bookmark, index));
	});
	// Voice & Video.
	bridge.on_video_changed(|form| {
		with_app(|app| app.video_changed(&form));
	});
	bridge.on_toggle_camera_preview(|| {
		with_app(|app| app.toggle_camera_preview());
	});
	bridge.on_test_sound(|| {
		with_app(|app| app.test_sound());
	});
	// Streaming.
	bridge.on_streaming_changed(|form| {
		with_app(|app| app.streaming_changed(&form));
	});
	bridge.on_layer_changed(|index, item| {
		with_app(|app| app.layer_changed(index, &item));
	});
	bridge.on_add_layer(|| {
		with_app(|app| app.add_layer());
	});
	bridge.on_remove_layer(|index| {
		with_app(|app| app.remove_layer(index));
	});
	bridge.on_audio_source_changed(|index, item| {
		with_app(|app| app.audio_source_changed(index, &item));
	});
	bridge.on_add_audio_source(|kind, name| {
		with_app(|app| app.add_audio_source(&kind, &name));
	});
	bridge.on_remove_audio_source(|index| {
		with_app(|app| app.remove_audio_source(index));
	});
	// Notifications, privacy.
	bridge.on_notify_changed(|form| {
		with_app(|app| app.notify_changed(&form));
	});
	bridge.on_test_notification(|| {
		with_app(|app| app.test_notification());
	});
	bridge.on_privacy_changed(|form| {
		with_app(|app| app.privacy_changed(&form));
	});
	// Integrations.
	bridge.on_gateway_admin(|session| {
		with_app(|app| app.gateway_admin(session));
	});
	bridge.on_config_set(|key, value| {
		with_app(|app| app.config_set(&key, &value));
	});
	bridge.on_config_reset(|key| {
		with_app(|app| app.config_reset(&key));
	});
	bridge.on_config_reload(|| {
		with_app(|app| app.config_reload());
	});
	bridge.on_perm_set(|action, everyone, server, channel| {
		with_app(|app| app.perm_set(&action, everyone, &server, &channel));
	});
	bridge.on_perm_reset(|action| {
		with_app(|app| app.perm_reset(&action));
	});
	bridge.on_refresh_encoders(|| {
		with_app(|app| app.refresh_encoders());
	});
	// Advanced.
	bridge.on_advanced_changed(|form| {
		with_app(|app| app.advanced_changed(&form));
	});
	bridge.on_srtp_move(|index, by| {
		with_app(|app| app.srtp_move(index, by));
	});
	bridge.on_srtp_toggle(|name| {
		with_app(|app| app.srtp_toggle(&name));
	});
	bridge.on_setting_set(|key, value| {
		with_app(|app| app.setting_set(&key, &value)).unwrap_or_default().into()
	});
	bridge.on_setting_reset(|key| {
		with_app(|app| app.setting_reset(&key));
	});
	bridge.on_search_settings(|text| {
		with_app(|app| app.search_settings(text.to_string()));
	});
	bridge.on_open_logs(|| {
		with_app(|app| app.open_logs());
	});
}
