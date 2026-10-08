//! TeamSpeak links opened in the app (`ts3server://`, `teamspeak://` and
//! `tmspk.gg`), from a message or the platform.

use slint::ComponentHandle;
use voelin_core::VoiceState;
use voelin_model::{Link, ServerLink};

use crate::app::{App, Nav};
use crate::vm;

impl App {
	/// Open a TeamSpeak link. In voice on its server: show it and move into
	/// the link's channel (a locked one asks for its password if the link
	/// has none or a wrong one). Otherwise the server dialog, filled in from
	/// the link (the saved server's when there is one): nothing connects
	/// before its Connect.
	pub(crate) fn open_link(&mut self, url: &str) {
		let link = match Link::parse(url) {
			Some(Link::Server(link)) => link,
			Some(Link::Invite(_)) => {
				self.set_status(
					"TeamSpeak invite codes (tmspk.gg/…) can't be opened yet; ask for the server address",
				);
				return;
			}
			None => {
				self.set_status("Not a TeamSpeak server link");
				return;
			}
		};
		let in_voice = |id: i64| {
			self.sessions.get(&id).is_some_and(|v| v.state.voice == VoiceState::Connected)
		};
		match vm::servers::link_server(&self.bookmarks, &link.address(), self.current, in_voice) {
			Some((id, true)) => self.move_by_link(id, &link),
			saved => self.open_link_form(saved.map(|(id, _)| id), &link),
		}
	}

	/// In voice on the link's server: show it, and move into its channel.
	fn move_by_link(&mut self, session: i64, link: &ServerLink) {
		self.show_server(session);
		let Some(path) = &link.channel else { return };
		let Some(view) = self.sessions.get_mut(&session) else { return };
		let Some(channel) = vm::servers::channel_by_path(&view.presence, path) else {
			let server = self.bookmark(session).map(|b| b.name.clone()).unwrap_or_default();
			self.set_status(format!("{server} has no channel \u{201c}{path}\u{201d}"));
			return;
		};
		if view.state.own_channel == Some(channel) {
			return;
		}
		// As if typed into the password dialog: a wrong one asks again.
		if let Some(password) = &link.channel_password {
			view.channel_passwords.insert(channel, password.clone());
		}
		self.join_channel(session, channel);
	}

	/// The server dialog, filled in from the link over the saved server's
	/// form, or a new server's.
	fn open_link_form(&mut self, saved: Option<i64>, link: &ServerLink) {
		let form = self.bookmark_form(saved.unwrap_or(-1));
		let form = vm::servers::link_form(link, form, &self.default_nickname());
		let Some(ui) = self.ui.upgrade() else { return };
		let nav = ui.global::<Nav>();
		nav.set_bookmark_form(form);
		nav.set_bookmark_open(true);
	}
}
