//! The rail, the server card, the channel tree and the voice controls.

use slint::ComponentHandle;
use voelin_core::Command;
use voelin_model::ChannelId;

use crate::app::{Bridge, MainWindow, with_app};

pub(super) fn wire(ui: &MainWindow) {
	let bridge = ui.global::<Bridge>();
	bridge.on_select_server(|id| {
		with_app(|app| app.select_server(id as i64));
	});
	bridge.on_search(|text| {
		with_app(|app| app.search(text.to_string()));
	});
	bridge.on_connect_voice(|| {
		with_app(|app| app.connect_voice());
	});
	bridge.on_disconnect_voice(|| {
		with_app(|app| app.command(|session| Command::DisconnectVoice { session }));
	});
	bridge.on_toggle_observe(|| {
		with_app(|app| app.toggle_observe());
	});
	bridge.on_join_channel(|cid| {
		with_app(|app| {
			app.command(|session| Command::MoveToChannel {
				session,
				channel: cid as ChannelId,
				password: None,
			})
		});
	});
	bridge.on_toggle_collapse(|cid| {
		with_app(|app| app.toggle_collapse(cid as ChannelId));
	});
	bridge.on_set_input_muted(|muted| {
		with_app(|app| app.command(|session| Command::SetInputMuted { session, muted }));
	});
	bridge.on_set_output_muted(|muted| {
		with_app(|app| app.command(|session| Command::SetOutputMuted { session, muted }));
	});
	bridge.on_push_to_talk(|on| {
		with_app(|app| app.command(|session| Command::SetTransmitting { session, on }));
	});
	bridge.on_edit_bookmark(|id| with_app(|app| app.bookmark_form(id as i64)).unwrap_or_default());
	bridge.on_save_bookmark(|form| {
		with_app(|app| app.save_bookmark(form));
	});
	bridge.on_delete_bookmark(|id| {
		with_app(|app| app.delete_bookmark(id as i64));
	});
	bridge.on_open_client(|id| with_app(|app| app.open_client(id as u16)).unwrap_or(false));
	bridge.on_client_audio_changed(|form| {
		with_app(|app| app.client_playback_changed(&form));
	});
}
