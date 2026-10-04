//! The members panel: the card over a member row and what it can do
//! (private chat, poke, contacts, per-client volume). The rows themselves
//! are built in `vm::tree::members`.

use slint::ComponentHandle;
use voelin_core::{Command, Contact, Relation};
use voelin_model::ChatTarget;

use crate::app::{App, Bridge, MemberCard};
use crate::settings::ClientPlayback;
use crate::vm;

impl App {
	/// `VOELIN_OPEN=member`: the card of the first person that is not us.
	pub(crate) fn open_first_member(&mut self) {
		let Some(view) = self.view() else { return };
		let own = view.state.own_client;
		let mut others: Vec<u16> = view
			.presence
			.members(view.state.own_channel.unwrap_or_default())
			.filter(|c| Some(c.id) != own)
			.map(|c| c.id)
			.collect();
		others.sort_unstable();
		if let Some(id) = others.first().copied() {
			self.open_member(i32::from(id));
		}
	}

	/// Open the card of a client (`-1` closes it).
	pub(crate) fn open_member(&mut self, client: i32) {
		if let Some(view) = self.view_mut() {
			view.member_card = u16::try_from(client).ok();
		}
		self.refresh_member_card();
	}

	pub(crate) fn refresh_member_card(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let card = self
			.view()
			.and_then(|view| {
				let id = view.member_card?;
				let client = view.presence.clients.get(&id)?;
				let uid = client.uid.clone();
				let contact = uid.as_ref().and_then(|u| self.contacts.get(u));
				let playback =
					uid.as_ref().and_then(|u| self.playback.get(u)).copied().unwrap_or_default();
				let groups: Vec<slint::SharedString> = client
					.server_groups
					.iter()
					.filter_map(|g| view.server_groups.iter().find(|i| i.id == *g))
					.map(|g| g.name.clone().into())
					.collect();
				Some(MemberCard {
					open: true,
					id: i32::from(id),
					name: client.nickname.clone().into(),
					initials: vm::avatar::initials(&client.nickname).into(),
					tint: vm::avatar::tint(&client.nickname),
					avatar: vm::avatar::image(view.avatar(id)),
					status: vm::tree::status_of(client, view.talking.contains(&id)).into(),
					description: client.description.clone().unwrap_or_default().into(),
					groups: crate::app::model(groups),
					country: client.country.clone().unwrap_or_default().into(),
					talk_power: client.talk_power,
					friend: contact.is_some_and(|c| c.relation == Relation::Friend),
					blocked: contact.is_some_and(|c| c.relation == Relation::Blocked),
					volume: playback.volume * 100.0,
					muted: playback.muted,
					known: uid.is_some(),
					streaming: client.streaming == Some(true),
				})
			})
			.unwrap_or_default();
		bridge.set_member_card(card);
	}

	/// The card's buttons: "message", "poke", "friend", "blocked", "mute".
	pub(crate) fn member_action(&mut self, action: &str) {
		let Some(id) = self.current else { return };
		let Some(view) = self.sessions.get(&id) else { return };
		let Some(client) = view.member_card.and_then(|c| view.presence.clients.get(&c)) else {
			return;
		};
		let (client_id, nickname, uid) = (client.id, client.nickname.clone(), client.uid.clone());
		match action {
			"message" => {
				if let Some(uid) = uid {
					self.open_chat(ChatTarget::Private(uid), true);
					self.open_member(-1);
				}
			}
			"watch" => {
				self.open_member(-1);
				self.watch_client(client_id);
			}
			"poke" => {
				self.command(|session| Command::Poke {
					session,
					client: client_id,
					message: String::new(),
				});
				self.set_status(format!("Poked {nickname}"));
			}
			"friend" | "blocked" => {
				let Some(uid) = uid else { return };
				let wanted = if action == "friend" { Relation::Friend } else { Relation::Blocked };
				let current = self.contacts.get(&uid).map(|c| c.relation);
				let mut contact =
					self.contacts.get(&uid).cloned().unwrap_or_else(|| Contact::new(uid.clone()));
				contact.nickname = nickname;
				contact.relation = if current == Some(wanted) { Relation::Neutral } else { wanted };
				self.contacts.insert(uid, contact.clone());
				self.engine.send(Command::SetContact { contact: Box::new(contact) });
				self.refresh_member_card();
			}
			"mute" => {
				let Some(uid) = uid else { return };
				let mut playback = self.playback.get(&uid).copied().unwrap_or_default();
				playback.muted = !playback.muted;
				self.set_playback(&uid, client_id, playback);
			}
			_ => {}
		}
	}

	/// The card's volume slider, in percent.
	pub(crate) fn member_volume(&mut self, percent: f32) {
		let Some(view) = self.view() else { return };
		let Some(client) = view.member_card.and_then(|c| view.presence.clients.get(&c)) else {
			return;
		};
		let (client_id, Some(uid)) = (client.id, client.uid.clone()) else { return };
		let mut playback = self.playback.get(&uid).copied().unwrap_or_default();
		playback.volume = (percent / 100.0).clamp(0.0, 4.0);
		self.set_playback(&uid, client_id, playback);
	}

	/// Store one client's playback and apply it to the session.
	fn set_playback(&mut self, uid: &str, client: u16, playback: ClientPlayback) {
		if playback.is_default() {
			self.playback.remove(uid);
		} else {
			self.playback.insert(uid.to_owned(), playback);
		}
		if let Err(e) = self.prefs.set(&crate::settings::CLIENT_PLAYBACK, self.playback.clone()) {
			tracing::warn!(%e, "could not store client volumes");
		}
		self.command(|session| Command::SetClientMuted { session, client, muted: playback.muted });
		self.command(|session| Command::SetClientVolume {
			session,
			client,
			volume: playback.volume,
		});
		self.refresh_tree();
		self.refresh_member_card();
	}
}
