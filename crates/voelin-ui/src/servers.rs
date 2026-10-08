//! Servers: bookmarks, connecting and observing, the rail, the sidebar's
//! server card and the channel tree.

use std::time::Instant;

use slint::ComponentHandle;
use tracing::{debug, info, warn};
use voelin_core::identity::LaunchImport;
use voelin_core::{Command, JoinFailure, ObserveState, Source, VoiceOptions, VoiceState};
use voelin_model::ChannelId;
use voelin_store::{Bookmark, QueryTransport};

use crate::app::{App, BookmarkForm, Bridge, Nav, SessionView, later};
use crate::vm;
use crate::vm::servers::{AfterConnect, JoinStep};

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
/// nickname `default_nickname`, the gateway is looked up in DNS
/// (`discover_gateway`). A new address drops the gateway and the query
/// login found or set for the old one.
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
		bookmark.gateway_url = None;
		bookmark.gateway_urls.clear();
		bookmark.query = None;
	}
	bookmark.name = match form.name.trim() {
		"" => bookmark.address.clone(),
		name => name.to_string(),
	};
	bookmark.nickname = match form.nickname.trim() {
		"" => default_nickname(),
		nickname => nickname.to_string(),
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

/// Whether observing goes on as it is when discovery changed the gateways
/// kept from `previous` to `found`: the same ones (an older version kept
/// only the one in use), or logged in through `in_use`, still published.
/// What was found is kept for the next start, without a new login now.
fn observing_stays(
	previous: &[String],
	found: &[String],
	in_use: Option<&str>,
	observe: ObserveState,
) -> bool {
	found == previous
		|| (observe == ObserveState::Observing
			&& in_use.is_some_and(|url| found.iter().any(|u| u == url)))
}

impl App {
	pub(crate) fn connect_voice(&mut self) {
		let channel =
			self.current.and_then(|id| self.bookmark(id)).and_then(|b| b.default_channel.clone());
		self.connect_voice_to(channel, None, None);
	}

	/// Connect the current server with voice, into `channel` (a path, or
	/// `/<id>`, with its password) if given, with a privilege key if given;
	/// whether it was asked for.
	pub(crate) fn connect_voice_to(
		&mut self,
		channel: Option<String>,
		channel_password: Option<String>,
		token: Option<String>,
	) -> bool {
		let Some(b) = self.current.and_then(|id| self.bookmark(id)).cloned() else { return false };
		if self.demo_ui {
			return false;
		}
		let mut options = VoiceOptions::new(&b.address, &b.nickname);
		if let Some(spec) = &b.client_version {
			match voelin_core::versions::resolve(spec) {
				Ok(version) => options.client_version = version,
				Err(error) => {
					self.set_status(format!("Invalid client compatibility version: {error}"));
					return false;
				}
			}
		}
		options.identity = Some(self.identity_for(Some(b.id)));
		options.server_password = self.secrets.get(&b.server_password_key()).ok().flatten();
		options.channel = channel;
		options.channel_password = channel_password;
		options.token = token;
		options.audio = true;
		options.stream_peer = self.video.peer_config(options.stream_peer);
		self.engine
			.send(Command::ConnectVoice { session: b.id as u64, options: Box::new(options) });
		self.set_status(format!("Connecting to {}…", b.address));
		// The admin may have published the gateway since it was added.
		self.discover_gateway(b.id);
		true
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

	/// Look up the gateway of a server ([`voelin_core::discover`]), once a
	/// run: every one the server publishes is kept, best first, and tried in
	/// turn; they take the place of a stored one that is no longer published
	/// (a gateway that moved, or one typed into an older version, which
	/// showed the field). Nothing found keeps what is stored.
	pub(crate) fn discover_gateway(&mut self, id: i64) {
		if self.demo_ui || !self.gateways_looked_up.insert(id) {
			return;
		}
		let Some(b) = self.bookmark(id) else { return };
		let address = b.address.clone();
		self.engine.runtime().spawn(async move {
			let started = Instant::now();
			let urls = voelin_core::discover::gateways(&address).await;
			let elapsed_ms = started.elapsed().as_millis() as u64;
			if urls.is_empty() {
				info!(server = %address, elapsed_ms, "no gateway published");
			} else {
				later(move |app| app.gateway_found(id, &address, urls, elapsed_ms));
			}
		});
	}

	/// Gateways were found for the server at `address` (best first); they
	/// are kept unless the server changed meanwhile, and the server is
	/// observed through them if it is the one shown. Users never hear of
	/// them (logged only).
	fn gateway_found(&mut self, id: i64, address: &str, urls: Vec<String>, elapsed_ms: u64) {
		let Some(b) = self.bookmarks.iter_mut().find(|b| b.id == id && b.address == address) else {
			return;
		};
		let previous = b.gateways();
		let in_use = b.gateway_url.clone();
		if !b.set_gateways(urls) {
			debug!(server = %b.address, urls = ?b.gateway_urls, elapsed_ms, "gateway found, as kept");
			return;
		}
		if let Err(e) = self.store.update_bookmark(b) {
			warn!(%e, "could not keep the gateway found");
			return;
		}
		let observe = self.sessions.get(&id).map(|v| v.state.observe).unwrap_or_default();
		let kept = observing_stays(&previous, &b.gateways(), in_use.as_deref(), observe);
		info!(server = %b.address, urls = ?b.gateway_urls, ?previous, kept, elapsed_ms, "gateway found");
		self.refresh_toolbar();
		if kept {
			if self.current == Some(id) && observe == ObserveState::Off {
				self.observe(id);
			}
			return;
		}
		// Observed through the old ones: through these now.
		if !previous.is_empty() {
			self.engine.send(Command::StopObserving { session: id as u64 });
			if let Some(view) = self.sessions.get_mut(&id) {
				view.state.observe = ObserveState::Off;
			}
		}
		if self.current == Some(id) {
			self.observe(id);
		}
	}

	/// The gateway logged in at `url`: it is the one in use from now on.
	pub(crate) fn gateway_in_use(&mut self, id: i64, url: &str) {
		let Some(b) = self.bookmarks.iter_mut().find(|b| b.id == id) else { return };
		if !b.gateway_in_use(url) {
			return;
		}
		if let Err(e) = self.store.update_bookmark(b) {
			warn!(%e, "could not keep the gateway in use");
			return;
		}
		info!(server = %b.address, %url, "gateway in use");
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

	/// Observe the server invisibly, unless it is already: through its
	/// gateway, else its own query login. Without either the gateway is
	/// looked up first, and observing starts when it is found
	/// (`gateway_found`). Asked while still connecting, the engine tries
	/// the gateway again sooner.
	pub(crate) fn observe(&mut self, id: i64) {
		let Some(b) = self.bookmark(id).cloned() else { return };
		if self.demo_ui {
			return;
		}
		let session = b.id as u64;
		let state = self.sessions.get(&b.id).map(|v| v.state.observe).unwrap_or_default();
		let urls = b.gateways();
		match state {
			ObserveState::Observing => return,
			// The engine decides: the same gateways are not started over,
			// at most tried again now.
			ObserveState::Connecting => {
				if !urls.is_empty() {
					debug!(server = %b.address, "still connecting to the gateway: try now");
					self.engine.send(Command::ObserveGateway {
						session,
						urls,
						identity: Box::new(self.identity_for(Some(b.id))),
					});
				}
				return;
			}
			ObserveState::Off => {}
		}
		if !urls.is_empty() {
			info!(server = %b.address, ?urls, "observing through the gateway");
			self.engine.send(Command::ObserveGateway {
				session,
				urls,
				identity: Box::new(self.identity_for(Some(b.id))),
			});
			// Still the one the server publishes?
			self.discover_gateway(b.id);
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
		} else {
			self.discover_gateway(b.id);
			return;
		}
		self.set_status("Observing invisibly…");
	}

	/// Show a server, and observe it.
	pub(crate) fn select_server(&mut self, id: i64) {
		self.current = Some(id);
		self.prefetch_counts();
		self.track_reading();
		self.refresh_all();
		self.observe(id);
	}

	pub(crate) fn bookmark_form(&self, id: i64) -> BookmarkForm {
		let Some(b) = self.bookmark(id) else {
			return BookmarkForm {
				id: -1,
				nickname: self.default_nickname().into(),
				..Default::default()
			};
		};
		let password = self.secrets.get(&b.server_password_key()).ok().flatten();
		let state = self.sessions.get(&b.id).map(|view| &view.state);
		vm::servers::form(b, state, password.as_deref().unwrap_or_default())
	}

	/// Save the server dialog's server, and show it; its id, none when it
	/// could not be saved (the toast says why).
	pub(crate) fn save_bookmark(&mut self, form: &BookmarkForm) -> Option<i64> {
		let old = self.bookmark(form.id as i64).cloned();
		let mut bookmark = bookmark_from_form(old.as_ref(), form, || self.default_nickname());
		let result = if bookmark.id < 0 {
			self.store.add_bookmark(&bookmark).map(|id| bookmark.id = id)
		} else {
			self.store.update_bookmark(&bookmark)
		};
		if let Err(e) = result {
			self.set_status(format!("Could not save: {e}"));
			return None;
		}
		let key = bookmark.server_password_key();
		let result = if form.server_password.is_empty() {
			self.secrets.delete(&key)
		} else {
			self.secrets.set(&key, &form.server_password)
		};
		if let Err(e) = result {
			warn!(%e, "could not store secret");
		}
		// Another server: what was observed is the old one's, and its
		// gateway is looked up anew.
		if let Some(old) = old.filter(|old| old.address != bookmark.address) {
			if old.query.is_some() {
				let _ = self.secrets.delete(&old.query_password_key());
			}
			self.gateways_looked_up.remove(&bookmark.id);
			self.engine.send(Command::StopObserving { session: bookmark.id as u64 });
			if let Some(view) = self.sessions.get_mut(&bookmark.id) {
				view.state.observe = ObserveState::Off;
			}
		}
		self.bookmarks = self.store.bookmarks().unwrap_or_default();
		self.select_server(bookmark.id);
		Some(bookmark.id)
	}

	/// The server dialog's Connect: save the server, then connect it with
	/// voice, into the channel of the link that filled the form if one did
	/// ([`vm::servers::connect_to`]). Only once saved: connecting what is
	/// shown after a failed save would connect the server shown before.
	/// Whether it was saved.
	pub(crate) fn save_and_connect(&mut self, form: &BookmarkForm) -> bool {
		let saved = self.save_bookmark(form);
		let Some(to) = vm::servers::connect_to(form, saved.and_then(|id| self.bookmark(id))) else {
			return false;
		};
		self.connect_voice_to(to.channel, to.channel_password, to.token);
		true
	}

	pub(crate) fn delete_bookmark(&mut self, id: i64) {
		self.engine.send(Command::CloseSession { session: id as u64 });
		if let Some(b) = self.bookmark(id).cloned() {
			let _ = self.secrets.delete(&b.server_password_key());
			let _ = self.secrets.delete(&b.query_password_key());
		}
		let _ = self.store.delete_bookmark(id);
		self.save_reads(id);
		self.sessions.remove(&id);
		self.gateways_looked_up.remove(&id);
		self.bookmarks = self.store.bookmarks().unwrap_or_default();
		self.current = self.bookmarks.first().map(|b| b.id);
		self.prefetch_counts();
		self.track_reading();
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

	/// A server's colour, as the rail shows it ([`vm::servers::tints`]).
	pub(crate) fn server_tint(&self, id: i64) -> slint::Color {
		let tints = vm::servers::tints(&self.bookmarks);
		self.bookmarks.iter().position(|b| b.id == id).map(|i| tints[i]).unwrap_or_default()
	}

	/// The rail: servers with their state, unread count (without private
	/// chats) and streams; what the chat strip and the private chats have
	/// unread.
	pub(crate) fn refresh_servers(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let tints = vm::servers::tints(&self.bookmarks);
		let items: Vec<_> = self
			.bookmarks
			.iter()
			.zip(tints)
			.map(|(b, tint)| {
				let view = self.sessions.get(&b.id);
				let state = view.map(|v| v.state.clone()).unwrap_or_default();
				let unread = view.map_or(0, SessionView::channel_unread);
				let live = view.is_some_and(|v| v.streams_available() && !v.streams.is_empty());
				let detail = view
					.filter(|v| !v.presence.channels.is_empty())
					.map(|v| vm::tree::stats(&v.presence))
					.unwrap_or_default();
				let flavor = view.map(|v| v.extra.flavor.clone()).unwrap_or_default();
				let icon = self.server_list_icon(b);
				vm::servers::item(b, &state, unread, live, detail, flavor, icon, tint)
			})
			.collect();
		vm::list::sync(&self.models.servers, &items);
		bridge.set_last_place(self.last_place(&items));
		bridge.set_current_server(self.current.map_or(-1, |id| id as i32));
		// Private chats count on the messages page (and the phone's Home).
		bridge.set_unread_total(self.view().map_or(0, SessionView::channel_unread));
		bridge.set_dm_unread(self.dm_unread());
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
	/// copies it, and the copy's toast names the channel); empty, with a
	/// note, without one.
	pub(crate) fn invite_link(&mut self) -> String {
		let bookmark = self.current.and_then(|id| self.bookmark(id)).map(|b| b.address.clone());
		let path = self.view().and_then(|v| {
			let cid = v.state.own_channel?;
			let own = v.presence.channels.get(&cid)?;
			// The link names the channels as the server has them; the
			// toast as the tree shows them.
			Some((
				vm::tree::channel_path(&v.presence, cid),
				vm::tree::channel_title(own).0.to_owned(),
			))
		});
		let (Some(address), Some((path, title))) = (bookmark, path) else {
			self.set_status("Join a voice channel to invite others to it");
			return String::new();
		};
		let path: Vec<&str> = path.iter().map(String::as_str).collect();
		let link = vm::servers::invite_link(&address, &path);
		self.copy_note = Some(format!("Copied an invite to {title}: {link}"));
		link
	}

	/// Join a channel of a session (a double-click in the tree, joining a
	/// friend): move there with voice, or connect into it. A locked channel
	/// asks for its password first, unless it was given this connection.
	pub(crate) fn join_channel(&mut self, session: i64, channel: ChannelId) {
		let Some(view) = self.sessions.get(&session) else { return };
		let Some(info) = view.presence.channels.get(&channel) else { return };
		let remembered = view.channel_passwords.get(&channel).map(String::as_str);
		match vm::servers::join_password(info, remembered) {
			JoinStep::Send(password) => self.enter_channel(session, channel, password),
			JoinStep::Ask => self.ask_channel_password(session, channel, false),
		}
	}

	/// The password dialog's Join.
	pub(crate) fn join_with_password(&mut self, password: String) {
		let Some((session, channel)) = self.join_target.take() else { return };
		if password.is_empty() {
			return;
		}
		if let Some(view) = self.sessions.get_mut(&session) {
			view.channel_passwords.insert(channel, password.clone());
		}
		self.enter_channel(session, channel, Some(password));
	}

	/// Move into a channel; without voice on its server, connect into it.
	fn enter_channel(&mut self, session: i64, channel: ChannelId, password: Option<String>) {
		let Some(view) = self.sessions.get_mut(&session) else { return };
		if view.state.voice != VoiceState::Disconnected {
			// Chosen while connecting: no longer the one to join after.
			view.join_after_connect = None;
			if !self.demo_ui {
				self.engine.send(Command::MoveToChannel {
					session: session as u64,
					channel,
					password,
				});
			}
			return;
		}
		if self.current != Some(session) {
			self.select_server(session);
		}
		// By id: by its name the server finds only a top-level channel (a
		// subchannel's name connects into the default channel).
		if self.connect_voice_to(Some(format!("/{channel}")), password, None)
			&& let Some(view) = self.sessions.get_mut(&session)
		{
			view.join_after_connect = Some(channel);
		}
	}

	/// Connected into another channel than the one asked for
	/// (`SessionView::join_after_connect`): the server says nothing when,
	/// for one, the password was wrong, so it is joined again, and that
	/// move says why. Once the channel shows in the voice
	/// connection's presence, which can come after the own channel.
	pub(crate) fn join_after_connect(&mut self, session: i64) {
		let Some(view) = self.sessions.get_mut(&session) else { return };
		let (VoiceState::Connected, Some(asked)) = (view.state.voice, view.join_after_connect)
		else {
			return;
		};
		let known = view.presence.channels.contains_key(&asked);
		match vm::servers::after_connect(view.state.own_channel, asked, known) {
			AfterConnect::Wait => {}
			AfterConnect::Joined => view.join_after_connect = None,
			AfterConnect::JoinAgain => {
				view.join_after_connect = None;
				self.join_channel(session, asked);
			}
		}
	}

	/// Open the password dialog for a locked channel; `wrong`: the one
	/// given was refused.
	fn ask_channel_password(&mut self, session: i64, channel: ChannelId, wrong: bool) {
		let Some(info) =
			self.sessions.get(&session).and_then(|v| v.presence.channels.get(&channel))
		else {
			return;
		};
		let name = vm::tree::channel_title(info).0.to_owned();
		self.join_target = Some((session, channel));
		let Some(ui) = self.ui.upgrade() else { return };
		let nav = ui.global::<Nav>();
		nav.set_channel_password_name(name.into());
		nav.set_channel_password_error(wrong);
		nav.set_channel_password_open(true);
	}

	/// The server refused a move ([`voelin_core::Event::JoinFailed`]): a
	/// password asks again (the one given is forgotten), the rest is a
	/// toast.
	pub(crate) fn join_failed(&mut self, session: i64, channel: ChannelId, reason: JoinFailure) {
		match reason {
			JoinFailure::Password => {
				let given = self
					.sessions
					.get_mut(&session)
					.and_then(|v| v.channel_passwords.remove(&channel))
					.is_some();
				self.ask_channel_password(session, channel, given);
			}
			JoinFailure::Full => self.set_status("The channel is full"),
			JoinFailure::Other(text) => self.set_status(text),
		}
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
	use voelin_store::QueryConfig;

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
		let form =
			BookmarkForm { id: -1, address: " ts.example.test ".into(), ..Default::default() };
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
			gateway_urls: vec!["ws://gw.example.test:7788/v1".into()],
			query: Some(QueryConfig { server_port: Some(9988), ..Default::default() }),
			client_version: Some("linux".into()),
			cached_server_icon: Some(voelin_store::CachedServerIcon {
				address: "old.example.test".into(),
				id: 1234,
			}),
		};
		// Same address: the gateway found and the query login stay.
		let same = BookmarkForm {
			id: 7,
			name: "Mine".into(),
			address: "old.example.test".into(),
			nickname: "Other".into(),
			..Default::default()
		};
		let b = bookmark_from_form(Some(&old), &same, || unreachable!("a nickname was given"));
		assert_eq!((b.id, b.name.as_str(), b.nickname.as_str()), (7, "Mine", "Other"));
		assert_eq!(b.gateway_url, old.gateway_url);
		assert_eq!(b.query, old.query);
		assert_eq!(b.cached_server_icon, old.cached_server_icon);
		// Another address: looked up again, for the new server.
		let form = BookmarkForm { address: "new.example.test:9988".into(), ..same };
		let b = bookmark_from_form(Some(&old), &form, || unreachable!("a nickname was given"));
		assert_eq!(b.address, "new.example.test:9988");
		assert!(b.cached_server_icon.is_none());
		assert_eq!((b.identity, b.default_channel), (Some(2), Some("Lobby/Sub".into())));
		assert_eq!(b.client_version.as_deref(), Some("linux"));
		assert_eq!((b.gateway_url, b.query), (None, None));
		assert!(b.gateway_urls.is_empty());
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

	#[test]
	fn a_gateway_found_again_is_not_logged_in_again() {
		let (tls, plain) = ("wss://gw.example.test/v1", "ws://ts.example.test:7788/v1");
		let urls = |list: &[&str]| list.iter().map(|u| u.to_string()).collect::<Vec<_>>();
		let stays = |previous: &[&str], found: &[&str], observe| {
			observing_stays(&urls(previous), &urls(found), Some(plain), observe)
		};
		// The same ones (an older version kept only the one in use).
		assert!(stays(&[plain], &[plain], ObserveState::Connecting));
		assert!(stays(&[plain], &[plain], ObserveState::Observing));
		// Logged in through one still published.
		assert!(stays(&[plain], &[tls, plain], ObserveState::Observing));
		// Not logged in yet: through the new ones.
		assert!(!stays(&[plain], &[tls, plain], ObserveState::Connecting));
		// The one in use is no longer published.
		assert!(!stays(&[plain], &[tls], ObserveState::Observing));
		assert!(!observing_stays(&[], &urls(&[tls]), None, ObserveState::Off));
	}
}
