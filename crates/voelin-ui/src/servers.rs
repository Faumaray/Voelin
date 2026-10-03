//! Servers: bookmarks, connecting and observing, the rail, the sidebar's
//! server card and the channel tree.

use slint::{ComponentHandle, SharedString};
use tracing::warn;
use voelin_core::{Command, ObserveState, Source, VoiceOptions, VoiceState};
use voelin_store::{Bookmark, QueryConfig, QueryTransport};

use crate::app::{App, BookmarkForm, Bridge};
use crate::vm;

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
				nickname: std::env::var("USER").unwrap_or_default().into(),
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
			name: b.name.clone().into(),
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
		let query = match form.query_transport.as_str() {
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
					server_port: None,
				})
			}
		};
		let mut bookmark = Bookmark {
			id: form.id as i64,
			name: if form.name.is_empty() {
				form.address.to_string()
			} else {
				form.name.to_string()
			},
			address: form.address.to_string(),
			nickname: form.nickname.to_string(),
			identity: self.bookmark(form.id as i64).and_then(|b| b.identity),
			default_channel: None,
			gateway_url: Some(form.gateway_url.to_string()).filter(|u| !u.is_empty()),
			query,
			client_version: None,
		};
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
				vm::servers::item(b, &state, unread, live, detail, flavor)
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
		let own_channel =
			view.and_then(|v| state.own_channel.and_then(|c| v.presence.channels.get(&c)));
		bridge.set_own_channel(own_channel.map(|c| c.name.clone()).unwrap_or_default().into());
		bridge.set_own_channel_topic(
			own_channel.and_then(|c| c.topic.clone()).unwrap_or_default().into(),
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
			groups: &view.server_groups,
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

	pub(crate) fn toggle_collapse(&mut self, channel: u64) {
		if let Some(view) = self.view_mut()
			&& !view.collapsed.remove(&channel)
		{
			view.collapsed.insert(channel);
		}
		self.refresh_tree();
	}
}
