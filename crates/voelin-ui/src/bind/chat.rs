//! The chat view: tabs, messages, reactions, pins, topics and files.

use slint::ComponentHandle;
use voelin_model::{ChannelId, ChatTarget};

use crate::app::{App, Bridge, MainWindow, with_app};

pub(super) fn wire(ui: &MainWindow) {
	let bridge = ui.global::<Bridge>();
	bridge.on_search_topics(|text| {
		with_app(|app| app.search_topics(text.to_string()));
	});
	bridge.on_open_channel_chat(|cid| {
		with_app(|app| app.open_chat(ChatTarget::Channel(cid as ChannelId), true));
	});
	// A tab of the chat strip, which leaves out private chats.
	bridge.on_select_tab(|i| {
		with_app(|app| {
			if let Some(index) = app.strip_tab(i) {
				app.select_tab(index);
			}
		});
	});
	bridge.on_close_tab(|i| {
		with_app(|app| {
			if let Some(index) = app.strip_tab(i) {
				app.close_tab(index);
			}
		});
	});
	bridge.on_send_message(|text| {
		with_app(|app| app.send_message(text.to_string()));
	});
	bridge.on_load_older(|| {
		with_app(App::load_older);
	});
	bridge.on_mark_read(|| {
		with_app(App::mark_read);
	});
	bridge.on_chat_shown_changed(|| {
		with_app(App::chat_shown_changed);
	});
	bridge.on_react(|key, emoji| {
		with_app(|app| app.react(key, emoji.to_string()));
	});
	bridge.on_pin(|key, on| {
		with_app(|app| app.pin(key, on));
	});
	bridge.on_download_file(|key, index| {
		with_app(|app| app.download_file(key, index));
	});
	bridge.on_attach_file(|| {
		with_app(App::attach_file);
	});
	bridge.on_open_pins(|| {
		with_app(App::open_pins);
	});
	bridge.on_jump_to_pin(|key| {
		with_app(|app| app.jump_to_pin(key));
	});
	bridge.on_open_topics(|| {
		with_app(App::open_topics);
	});
	bridge.on_open_topic(|id| {
		with_app(|app| app.open_topic(id));
	});
	bridge.on_send_topic_message(|text| {
		with_app(|app| app.send_topic_message(text.to_string()));
	});
	bridge.on_create_topic(|key, title| {
		with_app(|app| app.create_topic(key, title.to_string()));
	});
	bridge.on_open_link(|link, masked| {
		with_app(|app| app.open_link_text(&link, masked));
	});
	bridge.on_quote_text(|author, text| crate::vm::chat::quote(&author, &text).into());
	bridge.on_open_member(|id| {
		with_app(|app| app.open_member(id));
	});
	bridge.on_member_action(|action| {
		with_app(|app| app.member_action(&action));
	});
	bridge.on_send_poke(|text| {
		with_app(|app| app.send_poke(&text));
	});
	bridge.on_member_volume(|percent| {
		with_app(|app| app.member_volume(percent));
	});
}
