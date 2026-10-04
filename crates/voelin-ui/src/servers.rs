//! Servers: bookmarks, connecting and observing, the rail, the sidebar's
//! server card and the channel tree.

use slint::{ComponentHandle, SharedString};
use tracing::warn;
use voelin_core::identity::LaunchImport;
use voelin_core::{Command, ObserveState, Source, VoiceOptions, VoiceState};
use voelin_store::{Bookmark, QueryConfig, QueryTransport};

use crate::app::{App, BookmarkForm, Bridge, SessionView, later};
use crate::vm;

/// The name of the identity the app creates when it has none.
pub(crate) const CREATED_IDENTITY: &str = "Default";

/// What to tell the user about the identities imported from the official
/// clients at start ([`voelin_core::identity::import_new`]), if anything.
pub(crate) fn imported_status(report: &LaunchImport) -> Option<String> {
	let n = report.added.len();
	let names = report.added.iter().map(|f| f.nickname.as_str()).collect::<Vec<_>>().join(", ");
	let more = if n > 1 { format!(" ({n} imported: {names})") } else { String::new() };
	Some(match &report.default {
		Some(f) if report.replaced => format!(
			"You now use your TeamSpeak identity \u{201c}{}\u{201d}; your previous one is kept{more}",
			f.nickname
		),
		Some(f) => format!("Using your TeamSpeak identity \u{201c}{}\u{201d}{more}", f.nickname),
		None if n == 1 => format!("Imported the identity \u{201c}{names}\u{201d} from TeamSpeak"),
		None if n > 1 => format!("Imported {n} identities from TeamSpeak: {names}"),
		None => return None,
	})
}

/// The bookmark the form describes, over `old` (what the form does not show,
/// like the identity, the default channel or the client version, stays; a
/// new server has none of it). Only the address is needed: the name is the
/// address until the server tells its own (`adopt_server_name`), the
/// nickname `default_nickname`, the gateway is looked up later.
fn bookmark_from_form(
	old: Option<&Bookmark>,
	form: &BookmarkForm,
	default_nickname: impl FnOnce() -> String,
) -> Bookmark {
	let mut bookmark = old.cloned().unwrap_or_default();
	bookmark.id = form.id as i64;
	bookmark.address = form.address.trim().to_string();
	if old.is_some_and(|old| old.address != bookmark.address) {
		bookmark.cached_server_icon = None;
	}
	bookmark.name = match form.name.trim() {
		"" => bookmark.address.clone(),
		name => name.to_string(),
	};
	bookmark.nickname = match form.nickname.trim() {
		"" => default_nickname(),
		nickname => nickname.to_string(),
	};
	bookmark.gateway_url = Some(form.gateway_url.trim().to_string()).filter(|u| !u.is_empty());
	bookmark.query = match form.query_transport.as_str() {
		"none" | "" => None,
		t => {
			let (host, port) = form
				.query_addr
				.rsplit_once(':')
				.map_or((form.query_addr.to_string(), None), |(h, p)| {
					(h.to_string(), p.parse().ok())
				});
			let transport = match t {
				"raw" => QueryTransport::Raw,
				"http" => QueryTransport::Http,
				_ => QueryTransport::Ssh,
			};
			let default_port = match transport {
				QueryTransport::Raw => 10011,
				QueryTransport::Ssh => 10022,
				QueryTransport::Http => 10080,
			};
			Some(QueryConfig {
				transport,
				host,
				port: port.unwrap_or(default_port),
				user: form.query_user.to_string(),
				server_port: bookmark.query.as_ref().and_then(|q| q.server_port),
			})
		}
	};
	bookmark
}

/// Apply only details originating from the bookmark's current address.
/// Returns whether its persisted metadata changed.
fn apply_server_icon(
	bookmark: &mut Bookmark,
	view: &mut SessionView,
	address: &str,
	icon: u32,
) -> bool {
	if bookmark.address != address {
		return false;
	}
	view.icon_address = Some(address.to_owned());
	view.server_icon = icon;
	let before = bookmark.cached_server_icon.clone();
	bookmark.remember_server_icon(icon);
	bookmark.cached_server_icon != before
}

impl App {
	pub(crate) fn connect_voice(&mut self) {
		let channel =
			self.current.and_then(|id| self.bookmark(id)).and_then(|b| b.default_channel.clone());
		self.connect_voice_to(channel);
	}

	/// Connect the current server with voice, into `channel` (a name or
	/// path) if given.
	pub(crate) fn connect_voice_to(&mut self, channel: Option<String>) {
		let Some(b) = self.current.and_then(|id| self.bookmark(id)).cloned() else { return };
		if self.demo_ui {
			return;
		}
		let mut options = VoiceOptions::new(&b.address, &b.nickname);
		options.identity = Some(self.identity_for(Some(b.id)));
		options.server_password = self.secrets.get(&b.server_password_key()).ok().flatten();
		options.channel = channel;
		options.audio = true;
		options.stream_peer = self.video.peer_config(options.stream_peer);
		self.engine
			.send(Command::ConnectVoice { session: b.id as u64, options: Box::new(options) });
		self.set_status(format!("Connecting to {}…", b.address));
		// The admin may have published the gateway since it was added.
		self.discover_gateway(b.id);
	}

	/// The nickname a new server gets: the default identity's (one imported
	/// from TeamSpeak keeps the nickname used there), else the system user's.
	pub(crate) fn default_nickname(&self) -> String {
		self.store
			.default_identity()
			.ok()
			.flatten()
			.map(|identity| identity.name)
			.filter(|name| {
				![CREATED_IDENTITY, voelin_core::identity::DEFAULT_NICKNAME]
					.contains(&name.as_str())
			})
			.or_else(|| std::env::var("USER").or_else(|_| std::env::var("USERNAME")).ok())
			// Servers refuse an empty nickname (Android has no user name).
			.filter(|name| !name.trim().is_empty())
			.unwrap_or_else(|| "Voelin user".to_owned())
	}

	/// Look up the gateway of a server that has none in DNS
	/// ([`voelin_core::discover`]); one found is kept as if typed.
	pub(crate) fn discover_gateway(&self, id: i64) {
		let Some(b) = self.bookmark(id).filter(|b| b.gateway_url.is_none() && !self.demo_ui) else {
			return;
		};
		let address = b.address.clone();
		self.engine.runtime().spawn(async move {
			if let Some(url) = voelin_core::discover::gateway(&address).await {
				later(move |app| app.gateway_found(id, &address, url));
			}
		});
	}

	/// A gateway was found for the server at `address`; it is kept unless
	/// the server changed meanwhile.
	fn gateway_found(&mut self, id: i64, address: &str, url: String) {
		let Some(b) = self
			.bookmarks
			.iter_mut()
			.find(|b| b.id == id && b.address == address && b.gateway_url.is_none())
		else {
			return;
		};
		b.gateway_url = Some(url.clone());
		let name = b.name.clone();
		if let Err(e) = self.store.update_bookmark(b) {
			warn!(%e, "could not keep the gateway found");
			return;
		}
		self.set_status(format!("Found the gateway of {name}: {url}"));
		self.refresh_toolbar();
	}

	/// The server told its name: a server still named by its address takes
	/// it, so the address is all a user has to type.
	pub(crate) fn adopt_server_name(&mut self, id: i64, name: &str) {
		let Some(b) = self.bookmarks.iter_mut().find(|b| b.id == id && b.name == b.address) else {
			return;
		};
		if name.trim().is_empty() || self.demo_ui {
			return;
		}
		b.name = name.trim().to_owned();
		if let Err(e) = self.store.update_bookmark(b) {
			warn!(%e, "could not name the server");
		}
		self.refresh_servers();
		self.refresh_toolbar();
	}

	pub(crate) fn toggle_observe(&mut self) {
		let Some(b) = self.current.and_then(|id| self.bookmark(id)).cloned() else { return };
		let session = b.id as u64;
		let observing =
			self.sessions.get(&b.id).is_some_and(|v| v.state.observe != ObserveState::Off);
		if observing {
			self.engine.send(Command::StopObserving { session });
			return;
		}
		if let Some(url) = &b.gateway_url {
			self.engine.send(Command::ObserveGateway {
				session,
				url: url.clone(),
				identity: Box::new(self.identity_for(Some(b.id))),
			});
		} else if let Some(q) = &b.query {
			let secret = self.secrets.get(&b.query_password_key()).ok().flatten();
			let transport = match q.transport {
				QueryTransport::Raw => voelin_query::Transport::Raw,
				QueryTransport::Ssh => voelin_query::Transport::Ssh,
				QueryTransport::Http => voelin_query::Transport::Http,
			};
			let connect = voelin_query::Connect {
				transport,
				addr: format!("{}:{}", q.host, q.port),
				user: q.user.clone(),
				secret,
				server_port: q.server_port,
				server_id: None,
				line: Default::default(),
			};
			self.engine.send(Command::ObserveQuery { session, connect: Box::new(connect) });
		}
		self.set_status("Observing invisibly…");
	}

	pub(crate) fn select_server(&mut self, id: i64) {
		self.current = Some(id);
		self.refresh_all();
	}

	pub(crate) fn bookmark_form(&self, id: i64) -> BookmarkForm {
		let Some(b) = self.bookmark(id) else {
			return BookmarkForm {
				id: -1,
				nickname: self.default_nickname().into(),
				query_transport: "none".into(),
				query_user: "serveradmin".into(),
				..Default::default()
			};
		};
		let secret = |key: String| -> SharedString {
			self.secrets.get(&key).ok().flatten().unwrap_or_default().into()
		};
		let (transport, addr, user) = match &b.query {
			Some(q) => (
				match q.transport {
					QueryTransport::Raw => "raw",
					QueryTransport::Ssh => "ssh",
					QueryTransport::Http => "http",
				},
				format!("{}:{}", q.host, q.port),
				q.user.clone(),
			),
			None => ("none", String::new(), "serveradmin".to_string()),
		};
		BookmarkForm {
			id: b.id as i32,
			// Named by its address: empty, so a new address names it again.
			name: if b.name == b.address { SharedString::new() } else { b.name.clone().into() },
			address: b.address.clone().into(),
			nickname: b.nickname.clone().into(),
			server_password: secret(b.server_password_key()),
			gateway_url: b.gateway_url.clone().unwrap_or_default().into(),
			query_transport: transport.into(),
			query_addr: addr.into(),
			query_user: user.into(),
			query_password: secret(b.query_password_key()),
		}
	}

	pub(crate) fn save_bookmark(&mut self, form: BookmarkForm) {
		let mut bookmark =
			bookmark_from_form(self.bookmark(form.id as i64), &form, || self.default_nickname());
		let result = if bookmark.id < 0 {
			self.store.add_bookmark(&bookmark).map(|id| bookmark.id = id)
		} else {
			self.store.update_bookmark(&bookmark)
		};
		if let Err(e) = result {
			self.set_status(format!("Could not save: {e}"));
			return;
		}
		for (key, value) in [
			(bookmark.server_password_key(), &form.server_password),
			(bookmark.query_password_key(), &form.query_password),
		] {
			let result = if value.is_empty() {
				self.secrets.delete(&key)
			} else {
				self.secrets.set(&key, value)
			};
			if let Err(e) = result {
				warn!(%e, "could not store secret");
			}
		}
		self.current = Some(bookmark.id);
		self.bookmarks = self.store.bookmarks().unwrap_or_default();
		self.refresh_all();
		self.discover_gateway(bookmark.id);
	}

	pub(crate) fn delete_bookmark(&mut self, id: i64) {
		self.engine.send(Command::CloseSession { session: id as u64 });
		if let Some(b) = self.bookmark(id).cloned() {
			let _ = self.secrets.delete(&b.server_password_key());
			let _ = self.secrets.delete(&b.query_password_key());
		}
		let _ = self.store.delete_bookmark(id);
		self.sessions.remove(&id);
		self.bookmarks = self.store.bookmarks().unwrap_or_default();
		self.current = self.bookmarks.first().map(|b| b.id);
		self.refresh_all();
	}

	/// Keep only metadata supplied by the bookmark's current server.
	pub(crate) fn remember_server_icon(&mut self, id: i64, address: &str, icon: u32) {
		let Some(bookmark) = self.bookmarks.iter_mut().find(|b| b.id == id) else {
			return;
		};
		let changed =
			apply_server_icon(bookmark, self.sessions.entry(id).or_default(), address, icon);
		if changed
			&& !self.demo_ui
			&& let Err(error) = self.store.update_bookmark(bookmark)
		{
			warn!(%error, "could not keep the server icon");
		}
	}

	pub(crate) fn server_list_icon(&self, bookmark: &Bookmark) -> slint::Image {
		let current = self
			.sessions
			.get(&bookmark.id)
			.map(|view| view.server_icon_for(&bookmark.address))
			.unwrap_or_default();
		if current.size().width > 0 {
			current
		} else {
			vm::servers::cached_icon(bookmark, &self.engine.cache())
		}
	}

	/// The rail: servers with their state, unread count and streams.
	pub(crate) fn refresh_servers(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let items: Vec<_> = self
			.bookmarks
			.iter()
			.map(|b| {
				let view = self.sessions.get(&b.id);
				let state = view.map(|v| v.state.clone()).unwrap_or_default();
				let unread = view.map_or(0, |v| v.unread());
				let live = view.is_some_and(|v| v.streams_available() && !v.streams.is_empty());
				let detail = view
					.filter(|v| !v.presence.channels.is_empty())
					.map(|v| vm::tree::stats(&v.presence))
					.unwrap_or_default();
				let flavor = view.map(|v| v.extra.flavor.clone()).unwrap_or_default();
				let icon = self.server_list_icon(b);
				vm::servers::item(b, &state, unread, live, detail, flavor, icon)
			})
			.collect();
		vm::list::sync(&self.models.servers, &items);
		bridge.set_current_server(self.current.map_or(-1, |id| id as i32));
		bridge.set_unread_total(self.view().map_or(0, |v| v.unread()));
	}

	/// The server card, the voice controls and who we are.
	pub(crate) fn refresh_toolbar(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let bookmark = self.current.and_then(|id| self.bookmark(id));
		let view = self.view();
		let state = view.map(|v| v.state.clone()).unwrap_or_default();
		bridge.set_server_icon(bookmark.map(|b| self.server_list_icon(b)).unwrap_or_default());
		bridge.set_server_title(
			bookmark.map_or("No server selected".into(), |b| b.name.clone()).into(),
		);
		let has_presence = view.is_some_and(|v| !v.presence.channels.is_empty())
			&& (state.voice == VoiceState::Connected || state.observe == ObserveState::Observing);
		bridge.set_server_stats(
			view.filter(|_| has_presence)
				.map(|v| vm::tree::stats(&v.presence))
				.unwrap_or_default()
				.into(),
		);
		bridge.set_nickname(bookmark.map(|b| b.nickname.clone()).unwrap_or_default().into());
		let own = view.zip(state.own_client).and_then(|(v, c)| v.avatar(c));
		bridge.set_own_avatar(vm::avatar::image(own));
		let banner = |url: Option<&str>| {
			view.map(|v| vm::tree::banner(&v.pictures, url)).unwrap_or_default()
		};
		let details = view.map(|v| &v.presence.server);
		bridge.set_server_banner(banner(details.map(|d| d.banner_gfx_url.as_str())));
		bridge.set_server_banner_mode(details.map_or(0, |d| vm::tree::banner_mode(d.banner_mode)));
		let own_channel =
			view.and_then(|v| state.own_channel.and_then(|c| v.presence.channels.get(&c)));
		bridge.set_own_channel(
			own_channel.map(|c| vm::tree::channel_title(c).0).unwrap_or_default().into(),
		);
		bridge.set_own_channel_topic(
			own_channel.and_then(|c| c.topic.clone()).unwrap_or_default().into(),
		);
		bridge
			.set_own_channel_banner(banner(own_channel.and_then(|c| c.banner_gfx_url.as_deref())));
		bridge.set_own_channel_banner_mode(
			own_channel.map_or(0, |c| vm::tree::banner_mode(c.banner_mode)),
		);
		bridge.set_own_channel_limit(
			own_channel.and_then(|c| c.max_clients).filter(|m| *m >= 0).unwrap_or(-1),
		);
		bridge.set_voice_connected(state.voice == VoiceState::Connected);
		bridge.set_voice_connecting(state.voice == VoiceState::Connecting);
		bridge.set_observing(state.observe != ObserveState::Off);
		bridge.set_can_observe(
			bookmark.is_some_and(|b| b.gateway_url.is_some() || b.query.is_some()),
		);
		bridge.set_input_muted(state.input_muted);
		bridge.set_output_muted(state.output_muted);
		bridge.set_transmitting(state.transmitting);
		bridge.set_presence_source(
			match state.presence_source {
				Some(Source::Voice) => "live (voice)",
				Some(Source::Gateway) => "invisible (gateway)",
				Some(Source::Query) => "invisible (query)",
				None => "",
			}
			.into(),
		);
		if state.voice != VoiceState::Connected {
			bridge.set_input_level(-100.0);
			bridge.set_input_sending(false);
		}
	}

	/// The channel tree, the members of our channel and everyone on the
	/// server.
	pub(crate) fn refresh_tree(&self) {
		let Some(view) = self.view() else {
			vm::list::sync(&self.models.tree, &[]);
			vm::list::sync(&self.models.members, &[]);
			vm::list::sync(&self.models.server_members, &[]);
			return;
		};
		let input = vm::tree::TreeInput {
			presence: &view.presence,
			collapsed: &view.collapsed,
			talking: &view.talking,
			playback: &self.playback,
			own_channel: view.state.own_channel,
			own_client: view.state.own_client,
			filter: "",
			avatars: &view.avatars,
			icons: &view.icons,
			pictures: &view.pictures,
			groups: &view.server_groups,
			channel_groups: &view.channel_groups,
		};
		vm::list::sync(&self.models.tree, &vm::tree::rows(&input));
		let connected = view.state.voice == VoiceState::Connected;
		let members = if connected { vm::tree::members(&input) } else { Vec::new() };
		vm::list::sync(&self.models.members, &members);
		// The people in our channel first while their view is shown.
		let watching = self.watch.as_ref().is_some_and(|w| w.shown);
		let voice_first = connected && (self.voice_view || watching);
		let everyone = vm::tree::server_members(&input, voice_first);
		let filter = self.member_filter.trim().to_lowercase();
		let shown =
			if filter.is_empty() { everyone } else { vm::tree::matching(everyone, &filter) };
		vm::list::sync(&self.models.server_members, &shown);
		if let Some(ui) = self.ui.upgrade() {
			let total = view.presence.clients.values().filter(|c| !c.is_query).count();
			ui.global::<crate::app::Bridge>().set_member_total(total as i32);
		}
	}

	/// The members panel's search.
	pub(crate) fn search_members(&mut self, text: String) {
		if self.member_filter != text {
			self.member_filter = text;
			self.refresh_tree();
		}
	}

	/// The voice channel view was opened or closed: its chat is our
	/// channel's, and its people lead the members panel.
	pub(crate) fn show_voice_view(&mut self, on: bool) {
		self.voice_view = on;
		if on && let Some(channel) = self.view().and_then(|v| v.state.own_channel) {
			self.open_chat(voelin_model::ChatTarget::Channel(channel), true);
		}
		self.refresh_tree();
	}

	/// A link to the voice channel we are in, for the Invite button (which
	/// copies it); empty, with a note, without one.
	pub(crate) fn invite_link(&mut self) -> String {
		let bookmark = self.current.and_then(|id| self.bookmark(id)).map(|b| b.address.clone());
		let path = self.view().and_then(|v| {
			let mut names = Vec::new();
			let mut channel = v.presence.channels.get(&v.state.own_channel?)?;
			loop {
				names.push(channel.name.clone());
				match v.presence.channels.get(&channel.parent) {
					Some(parent) if channel.parent != 0 && names.len() < 64 => channel = parent,
					_ => break,
				}
			}
			names.reverse();
			Some(names)
		});
		let (Some(address), Some(path)) = (bookmark, path) else {
			self.set_status("Join a voice channel to invite others to it");
			return String::new();
		};
		let path: Vec<&str> = path.iter().map(String::as_str).collect();
		let link = vm::servers::invite_link(&address, &path);
		self.set_status(format!("Copied an invite to {}: {link}", path.last().unwrap_or(&"")));
		link
	}

	pub(crate) fn toggle_collapse(&mut self, channel: u64) {
		if let Some(view) = self.view_mut()
			&& !view.collapsed.remove(&channel)
		{
			view.collapsed.insert(channel);
		}
		self.refresh_tree();
	}
}

#[cfg(test)]
mod tests {
	use voelin_core::identity::Found;

	use super::*;

	#[test]
	fn edited_server_rejects_queued_icons_and_failed_reconnect_keeps_them_hidden() {
		let path = std::env::temp_dir()
			.join(format!("voelin-server-icon-provenance-{}.svg", std::process::id()));
		std::fs::write(&path, r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="red"/></svg>"#).unwrap();
		let mut bookmark = Bookmark { address: "a.test".into(), ..Default::default() };
		let mut view = SessionView::default();
		view.icons.insert(1234, path.clone());
		assert!(apply_server_icon(&mut bookmark, &mut view, "a.test", 1234));
		assert_eq!(view.server_icon_for(&bookmark.address).size().width, 4);

		bookmark = bookmark_from_form(
			Some(&bookmark),
			&BookmarkForm { address: "b.test".into(), ..Default::default() },
			|| "Me".into(),
		);
		// Connecting does not retag received imagery. A failed B connection
		// must not make A's image appear under B's name.
		for state in [VoiceState::Connecting, VoiceState::Disconnected] {
			view.state.voice = state;
			assert_eq!(view.server_icon_for(&bookmark.address).size().width, 0);
		}
		assert!(!apply_server_icon(&mut bookmark, &mut view, "a.test", 4321));
		assert_eq!(view.server_icon, 1234);
		assert_eq!(view.icon_address.as_deref(), Some("a.test"));
		assert!(bookmark.server_icon_id().is_none());

		// Equal icon IDs on the next server still need new provenance.
		assert!(apply_server_icon(&mut bookmark, &mut view, "b.test", 1234));
		assert_eq!(bookmark.server_icon_id(), Some(1234));
		assert_eq!(view.server_icon_for(&bookmark.address).size().width, 4);
		assert!(!apply_server_icon(&mut bookmark, &mut view, "a.test", 0));
		assert_eq!(bookmark.server_icon_id(), Some(1234));
		assert!(apply_server_icon(&mut bookmark, &mut view, "b.test", 0));
		assert!(bookmark.server_icon_id().is_none());
		assert_eq!(view.server_icon_for(&bookmark.address).size().width, 0);
		std::fs::remove_file(path).unwrap();
	}

	#[test]
	fn the_address_alone_makes_a_server() {
		let form = BookmarkForm {
			id: -1,
			address: " ts.example.test ".into(),
			query_transport: "none".into(),
			..Default::default()
		};
		let b = bookmark_from_form(None, &form, || "Nick".into());
		assert_eq!((b.address.as_str(), b.name.as_str()), ("ts.example.test", "ts.example.test"));
		assert_eq!(b.nickname, "Nick");
		assert_eq!(
			(b.gateway_url, b.query, b.identity, b.default_channel),
			(None, None, None, None)
		);
	}

	#[test]
	fn editing_keeps_what_the_form_does_not_show() {
		let old = Bookmark {
			id: 7,
			name: "Old".into(),
			address: "old.example.test".into(),
			nickname: "Me".into(),
			identity: Some(2),
			default_channel: Some("Lobby/Sub".into()),
			gateway_url: Some("ws://gw.example.test:7788/v1".into()),
			query: Some(QueryConfig { server_port: Some(9988), ..Default::default() }),
			client_version: Some("linux".into()),
			cached_server_icon: Some(voelin_store::CachedServerIcon {
				address: "old.example.test".into(),
				id: 1234,
			}),
		};
		let form = BookmarkForm {
			id: 7,
			name: "Mine".into(),
			address: "new.example.test:9988".into(),
			nickname: "Other".into(),
			// Cleared: looked up again.
			gateway_url: "".into(),
			query_transport: "raw".into(),
			query_addr: "q.example.test".into(),
			query_user: "serveradmin".into(),
			..Default::default()
		};
		let b = bookmark_from_form(Some(&old), &form, || unreachable!("a nickname was given"));
		assert_eq!((b.id, b.name.as_str(), b.nickname.as_str()), (7, "Mine", "Other"));
		assert_eq!(b.address, "new.example.test:9988");
		assert!(b.cached_server_icon.is_none());
		assert_eq!((b.identity, b.default_channel), (Some(2), Some("Lobby/Sub".into())));
		assert_eq!(b.client_version.as_deref(), Some("linux"));
		assert_eq!(b.gateway_url, None);
		let q = b.query.unwrap();
		assert_eq!(
			(q.transport, q.host.as_str(), q.port),
			(QueryTransport::Raw, "q.example.test", 10011)
		);
		assert_eq!(q.server_port, Some(9988));
	}

	#[test]
	fn the_import_is_told_by_nickname() {
		let found = |nickname: &str| Found {
			nickname: nickname.into(),
			identity: tsclientlib::Identity::create(),
			source: "settings.db".into(),
			selected: false,
		};
		let report = |added: &[&str], default: Option<&str>, replaced| LaunchImport {
			added: added.iter().map(|n| found(n)).collect(),
			default: default.map(found),
			replaced,
		};
		assert_eq!(imported_status(&report(&[], None, false)), None);
		assert_eq!(
			imported_status(&report(&["Main"], Some("Main"), false)).unwrap(),
			"Using your TeamSpeak identity \u{201c}Main\u{201d}"
		);
		assert_eq!(
			imported_status(&report(&["Main", "Alt"], None, false)).unwrap(),
			"Imported 2 identities from TeamSpeak: Main, Alt"
		);
		// Imported earlier, now ours instead of the one the app made.
		assert_eq!(
			imported_status(&report(&[], Some("Main"), true)).unwrap(),
			"You now use your TeamSpeak identity \u{201c}Main\u{201d}; your previous one is kept"
		);
	}
}
