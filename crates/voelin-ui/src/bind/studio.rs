//! The Stream Studio's callbacks, for the main window and for the studio's
//! own window (each has its own StudioBridge global).

use voelin_core::studio::Command;

use crate::app::{StudioBridge, with_app};

/// Slint ids are ints; the studio's are u64.
fn id(value: i32) -> u64 {
	u64::try_from(value).unwrap_or(u64::MAX)
}

fn index(value: i32) -> usize {
	usize::try_from(value).unwrap_or(usize::MAX)
}

pub(crate) fn wire(bridge: &StudioBridge) {
	// From a page's init: run it after the page is built, outside of Slint's
	// update.
	bridge.on_open(|| {
		let _ = slint::invoke_from_event_loop(|| {
			with_app(|app| app.studio_open());
		});
	});
	bridge.on_detach(|| {
		with_app(|app| app.studio_detach());
	});
	bridge.on_dock(|| {
		with_app(|app| app.studio_dock());
	});

	bridge.on_select_scene(|scene| {
		with_app(|app| app.studio_scene_command(Command::SetActiveScene { scene: id(scene) }));
	});
	bridge.on_add_scene(|| {
		with_app(|app| app.studio_add_scene());
	});
	bridge.on_rename_scene(|scene, name| {
		let name = name.trim().to_owned();
		if !name.is_empty() {
			with_app(|app| {
				app.studio_scene_command(Command::RenameScene { scene: id(scene), name })
			});
		}
	});
	bridge.on_remove_scene(|scene| {
		with_app(|app| app.studio_scene_command(Command::RemoveScene { scene: id(scene) }));
	});

	bridge.on_list_sources(|kind| with_app(|app| app.studio_list_sources(&kind)).unwrap_or(false));
	bridge.on_add_picked(|_, i| {
		with_app(|app| app.studio_add_picked(index(i)));
	});
	bridge.on_add_source(|form| {
		with_app(|app| app.studio_add_source(&form));
	});
	bridge.on_source_form(|source| {
		with_app(|app| app.studio_source_form(id(source))).unwrap_or_default()
	});
	bridge.on_apply_source(|form| {
		with_app(|app| app.studio_apply_source(&form));
	});
	bridge.on_toggle_source(|source| {
		with_app(|app| app.studio_toggle_source(id(source)));
	});
	bridge.on_source_action(|source, action| {
		with_app(|app| app.studio_source_action(id(source), &action));
	});
	bridge.on_main_source(|kind| {
		with_app(|app| app.studio_main_source(&kind));
	});

	bridge.on_list_audio(|| {
		with_app(|app| app.studio_list_audio());
	});
	bridge.on_add_audio(|i| {
		with_app(|app| app.studio_add_audio(index(i)));
	});
	bridge.on_set_gain(|i, db, done| {
		with_app(|app| app.studio_set_gain(index(i), db, done));
	});
	bridge.on_toggle_mute(|i| {
		with_app(|app| app.studio_toggle_mute(index(i)));
	});
	bridge.on_remove_audio(|i| {
		with_app(|app| app.studio_remove_audio(index(i)));
	});

	bridge.on_edit_form(|key, value| {
		with_app(|app| app.studio_edit(&key, &value));
	});
	bridge.on_set_output(|w, h, fps| {
		with_app(|app| app.studio_set_output(&w, &h, &fps));
	});
	bridge.on_set_layer(|i, layer| {
		with_app(|app| app.studio_set_layer(index(i), &layer));
	});
	bridge.on_add_layer(|| {
		with_app(|app| app.studio_add_layer());
	});
	bridge.on_remove_layer(|i| {
		with_app(|app| app.studio_remove_layer(index(i)));
	});
	bridge.on_advanced_settings(|| {
		with_app(|app| app.studio_advanced());
	});

	bridge.on_save_clip(|| {
		with_app(|app| app.studio_save_clip());
	});
	bridge.on_toggle_recording(|| {
		with_app(|app| app.studio_toggle_recording());
	});
	bridge.on_open_recordings(|| {
		with_app(|app| app.studio_open_recordings());
	});
	bridge.on_go_live(|| {
		with_app(|app| app.studio_go_live());
	});
	bridge.on_end_stream(|| {
		with_app(|app| app.studio_end());
	});
	bridge.on_settings_form(|| with_app(|app| app.studio_settings_form()).unwrap_or_default());
	bridge.on_apply_settings(|form| {
		with_app(|app| app.studio_apply_settings(&form));
	});

	bridge.on_respond_viewer(|client, accept| {
		with_app(|app| app.studio_respond_viewer(u16::try_from(client).unwrap_or(0), accept));
	});
	bridge.on_send_chat(|text| {
		with_app(|app| app.studio_send_chat(text.to_string()));
	});
	bridge.on_open_chat(|| {
		with_app(|app| app.studio_open_chat());
	});
}
