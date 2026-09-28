//! The chat view.

use slint::ComponentHandle;
use voelin_model::{ChannelId, ChatTarget};

use crate::app::{Bridge, MainWindow, with_app};

pub(super) fn wire(ui: &MainWindow) {
	let bridge = ui.global::<Bridge>();
	bridge.on_open_channel_chat(|cid| {
		with_app(|app| app.open_chat(ChatTarget::Channel(cid as ChannelId), true));
	});
	bridge.on_select_tab(|i| {
		with_app(|app| app.select_tab(i as usize));
	});
	bridge.on_close_tab(|i| {
		with_app(|app| app.close_tab(i as usize));
	});
	bridge.on_send_message(|text| {
		with_app(|app| app.send_message(text.to_string()));
	});
}
