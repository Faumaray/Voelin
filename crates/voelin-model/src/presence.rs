//! Who is where: channels and the clients in them.
//!
//! The same model is fed from a voice connection, a query session or a
//! gateway. Sources send a [`PresenceSnapshot`] followed by
//! [`PresenceDelta`]s; sources that can only poll use [`Presence::diff`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type ChannelId = u64;
pub type ClientId = u16;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
	pub id: ChannelId,
	/// `0` for top-level channels.
	pub parent: ChannelId,
	/// Id of the sibling directly above, `0` for the first.
	pub order: ChannelId,
	pub name: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub topic: Option<String>,
	#[serde(default)]
	pub has_password: bool,
	/// `None` means unlimited.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub max_clients: Option<i32>,
	#[serde(default)]
	pub needed_subscribe_power: i32,
	#[serde(default)]
	pub needed_talk_power: i32,
	#[serde(default)]
	pub is_default: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
	pub id: ClientId,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub uid: Option<String>,
	pub nickname: String,
	pub channel: ChannelId,
	/// ServerQuery clients (bots, our own relays). UIs normally hide them.
	#[serde(default)]
	pub is_query: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub away: Option<String>,
	#[serde(default)]
	pub input_muted: bool,
	#[serde(default)]
	pub output_muted: bool,
	/// `None` when the source cannot tell (query sessions see no voice).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub talking: Option<bool>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub streaming: Option<bool>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub server_groups: Vec<u64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub country: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceSnapshot {
	pub server_name: String,
	pub channels: Vec<ChannelInfo>,
	pub clients: Vec<ClientInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresenceDelta {
	ChannelAdded(ChannelInfo),
	ChannelChanged(ChannelInfo),
	ChannelRemoved { id: ChannelId },
	ClientJoined(ClientInfo),
	ClientChanged(ClientInfo),
	ClientMoved { id: ClientId, channel: ChannelId },
	ClientLeft { id: ClientId },
	ServerRenamed { name: String },
}

/// Current state; apply deltas as they arrive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Presence {
	pub server_name: String,
	pub channels: BTreeMap<ChannelId, ChannelInfo>,
	pub clients: BTreeMap<ClientId, ClientInfo>,
}

impl Presence {
	pub fn from_snapshot(s: PresenceSnapshot) -> Self {
		Self {
			server_name: s.server_name,
			channels: s.channels.into_iter().map(|c| (c.id, c)).collect(),
			clients: s.clients.into_iter().map(|c| (c.id, c)).collect(),
		}
	}

	pub fn snapshot(&self) -> PresenceSnapshot {
		PresenceSnapshot {
			server_name: self.server_name.clone(),
			channels: self.channels.values().cloned().collect(),
			clients: self.clients.values().cloned().collect(),
		}
	}

	/// Apply a delta. Returns `false` if it referred to something unknown
	/// (the caller may want to resynchronise).
	pub fn apply(&mut self, delta: &PresenceDelta) -> bool {
		match delta {
			PresenceDelta::ChannelAdded(c) | PresenceDelta::ChannelChanged(c) => {
				self.channels.insert(c.id, c.clone());
				true
			}
			PresenceDelta::ChannelRemoved { id } => self.channels.remove(id).is_some(),
			PresenceDelta::ClientJoined(c) | PresenceDelta::ClientChanged(c) => {
				self.clients.insert(c.id, c.clone());
				true
			}
			PresenceDelta::ClientMoved { id, channel } => match self.clients.get_mut(id) {
				Some(c) => {
					c.channel = *channel;
					true
				}
				None => false,
			},
			PresenceDelta::ClientLeft { id } => self.clients.remove(id).is_some(),
			PresenceDelta::ServerRenamed { name } => {
				self.server_name = name.clone();
				true
			}
		}
	}

	/// Deltas that turn `self` into `new`, for sources that poll.
	pub fn diff(&self, new: &Presence) -> Vec<PresenceDelta> {
		let mut out = Vec::new();
		if self.server_name != new.server_name {
			out.push(PresenceDelta::ServerRenamed { name: new.server_name.clone() });
		}
		for (id, c) in &new.channels {
			match self.channels.get(id) {
				None => out.push(PresenceDelta::ChannelAdded(c.clone())),
				Some(old) if old != c => out.push(PresenceDelta::ChannelChanged(c.clone())),
				_ => {}
			}
		}
		for (id, c) in &new.clients {
			match self.clients.get(id) {
				None => out.push(PresenceDelta::ClientJoined(c.clone())),
				Some(old) if old == c => {}
				Some(old) if ClientInfo { channel: c.channel, ..old.clone() } == *c => {
					out.push(PresenceDelta::ClientMoved { id: *id, channel: c.channel });
				}
				Some(_) => out.push(PresenceDelta::ClientChanged(c.clone())),
			}
		}
		for id in self.clients.keys().filter(|id| !new.clients.contains_key(id)) {
			out.push(PresenceDelta::ClientLeft { id: *id });
		}
		// Channels last, so clients leave a removed channel first.
		for id in self.channels.keys().filter(|id| !new.channels.contains_key(id)) {
			out.push(PresenceDelta::ChannelRemoved { id: *id });
		}
		out
	}

	/// Clients in a channel, excluding query clients.
	pub fn members(&self, channel: ChannelId) -> impl Iterator<Item = &ClientInfo> {
		self.clients.values().filter(move |c| c.channel == channel && !c.is_query)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ch(id: u64, name: &str) -> ChannelInfo {
		ChannelInfo { id, name: name.into(), ..Default::default() }
	}

	fn cl(id: u16, name: &str, channel: u64) -> ClientInfo {
		ClientInfo { id, nickname: name.into(), channel, ..Default::default() }
	}

	fn presence(channels: Vec<ChannelInfo>, clients: Vec<ClientInfo>) -> Presence {
		Presence::from_snapshot(PresenceSnapshot { server_name: "s".into(), channels, clients })
	}

	#[test]
	fn diff_then_apply_reaches_target() {
		let old = presence(
			vec![ch(1, "Lobby"), ch(2, "Games")],
			vec![cl(1, "Alice", 1), cl(2, "Bob", 1), cl(3, "Carol", 2)],
		);
		let mut muted = cl(2, "Bob", 1);
		muted.input_muted = true;
		let new = presence(
			vec![ch(1, "Lobby"), ch(3, "New")],
			vec![cl(1, "Alice", 3), muted, cl(4, "Dave", 1)],
		);
		let deltas = old.diff(&new);
		assert!(deltas.contains(&PresenceDelta::ClientMoved { id: 1, channel: 3 }));
		assert!(deltas.contains(&PresenceDelta::ClientLeft { id: 3 }));
		assert!(deltas.contains(&PresenceDelta::ChannelRemoved { id: 2 }));
		assert!(deltas.iter().any(|d| matches!(d, PresenceDelta::ClientChanged(c) if c.id == 2)));
		let mut state = old.clone();
		for d in &deltas {
			assert!(state.apply(d));
		}
		assert_eq!(state, new);
		assert!(new.diff(&new).is_empty());
	}

	#[test]
	fn members_skip_query_clients() {
		let mut bot = cl(9, "bot", 1);
		bot.is_query = true;
		let p = presence(vec![ch(1, "Lobby")], vec![cl(1, "Alice", 1), bot]);
		assert_eq!(p.members(1).count(), 1);
	}

	#[test]
	fn delta_json_is_tagged() {
		let json =
			serde_json::to_string(&PresenceDelta::ClientMoved { id: 1, channel: 3 }).unwrap();
		assert_eq!(json, r#"{"kind":"client_moved","id":1,"channel":3}"#);
		let back: PresenceDelta = serde_json::from_str(&json).unwrap();
		assert_eq!(back, PresenceDelta::ClientMoved { id: 1, channel: 3 });
	}
}
