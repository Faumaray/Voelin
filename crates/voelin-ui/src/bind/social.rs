//! Home, friends, direct messages, the bell, the search, a server's events
//! and pictures in chat.

use slint::ComponentHandle;

use crate::app::{Bridge, MainWindow, with_app};

pub(super) fn wire(ui: &MainWindow) {
	let bridge = ui.global::<Bridge>();
	// Home.
	bridge.on_open_conversation(|key| {
		with_app(|app| app.open_conversation(key));
	});
	bridge.on_watch_live(|session, id| {
		with_app(|app| app.watch_live(i64::from(session), &id));
	});
	bridge.on_open_happening(|index| {
		with_app(|app| app.open_happening(index));
	});
	bridge.on_open_recording(|path| {
		with_app(|app| app.open_recording(&path));
	});
	bridge.on_open_recordings_folder(|| {
		with_app(|app| app.open_recordings_folder());
	});
	bridge.on_connect_server(|id| {
		with_app(|app| app.connect_server(i64::from(id)));
	});
	bridge.on_resume(|| {
		with_app(|app| app.resume());
	});

	// Friends.
	bridge.on_friends_tab(|tab| {
		with_app(|app| app.friends_tab(tab));
	});
	bridge.on_search_contacts(|text| {
		with_app(|app| app.search_contacts(text.to_string()));
	});
	bridge.on_select_contact(|uid| {
		with_app(|app| app.select_contact(uid.to_string()));
	});
	bridge.on_contact_action(|uid, action| {
		with_app(|app| app.contact_action(&uid, &action));
	});
	bridge.on_contact_note(|uid, note| {
		with_app(|app| app.contact_note(&uid, note.to_string()));
	});
	bridge.on_contact_volume(|uid, percent| {
		with_app(|app| app.contact_volume(&uid, percent));
	});

	// Direct messages.
	bridge.on_dm_tab(|tab| {
		with_app(|app| app.dm_tab(tab));
	});
	bridge.on_search_conversations(|text| {
		with_app(|app| app.search_conversations(text.to_string()));
	});
	bridge.on_dm_action(|action| {
		with_app(|app| app.dm_action(&action));
	});
	bridge.on_dm_volume(|percent| {
		with_app(|app| app.dm_volume(percent));
	});
	bridge.on_send_offline(|form| {
		with_app(|app| app.send_offline(form));
	});

	// The bell.
	bridge.on_open_notice(|key| {
		with_app(|app| app.open_notice(key));
	});
	bridge.on_mark_notices_read(|| {
		with_app(|app| app.mark_notices_read());
	});
	bridge.on_clear_notices(|| {
		with_app(|app| app.clear_notices());
	});

	// The search: the top bar's field used to filter the channel tree;
	// it now opens the search (Ctrl+K), whose results are everywhere.
	bridge.on_search(|text| {
		with_app(|app| app.search(text.to_string()));
	});
	bridge.on_search_activate(|index| {
		with_app(|app| app.search_activate(index));
	});

	// Events.
	bridge.on_open_events(|| {
		with_app(|app| app.open_events());
	});
	bridge.on_rsvp(|id, status| {
		with_app(|app| app.rsvp(id, &status));
	});
	bridge.on_toggle_event(|id| {
		with_app(|app| app.toggle_event(id));
	});
	bridge.on_edit_event(|id| with_app(|app| app.edit_event(id)).unwrap_or_default());
	bridge.on_save_event(|form| with_app(|app| app.save_event(&form)).unwrap_or_default().into());
	bridge.on_delete_event(|id| {
		with_app(|app| app.delete_event(id));
	});
	bridge.on_watch_event(|id| {
		with_app(|app| app.watch_event(id));
	});

	// Pictures in chat.
	bridge.on_open_preview(|key, index| {
		with_app(|app| app.open_preview(key, index)).unwrap_or_default()
	});

	// Opening the messages or the Library loads what they list.
	bridge.on_open_messages(|| {
		with_app(|app| app.load_recent_chats());
	});
	bridge.on_open_library(|| {
		with_app(|app| app.load_library());
	});
}
