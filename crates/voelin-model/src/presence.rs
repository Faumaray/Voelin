//! Who is where: channels and the clients in them.
//!
//! The same model is fed from a voice connection, a query session or a
//! gateway. Sources send a [`PresenceSnapshot`] followed by
//! [`PresenceDelta`]s; sources that can only poll use [`Presence::diff`].
//!
//! A voice connection also knows the server's details (welcome message,
//! host banner, …) and its server and channel groups ([`Presence::server`],
//! [`Presence::server_groups`], [`Presence::channel_groups`]); snapshots and
//! deltas do not carry them (other sources leave them empty).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::server::ServerDetails;

pub type ChannelId = u64;
pub type ClientId = u16;
/// A server or channel group id.
pub type GroupId = u64;

fn is_zero_i32(n: &i32) -> bool {
	*n == 0
}

fn is_zero_u32(n: &u32) -> bool {
	*n == 0
}

fn is_false(b: &bool) -> bool {
	!*b
}

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
	/// Icon id (`channel_icon_id`); 0: none.
	#[serde(default, skip_serializing_if = "is_zero_u32")]
	pub icon: u32,
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
	/// Away, with the away message (empty without one); `None`: not away.
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
	pub server_groups: Vec<GroupId>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub country: Option<String>,
	/// MD5 hash of the client's avatar (`client_flag_avatar`, hex); `None`:
	/// no avatar. A new hash means a new avatar.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub avatar: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub description: Option<String>,
	#[serde(default, skip_serializing_if = "is_zero_i32")]
	pub talk_power: i32,
	/// Allowed to talk regardless of talk power (`client_is_talker`).
	#[serde(default, skip_serializing_if = "is_false")]
	pub talker: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub channel_group: Option<GroupId>,
	/// Badge GUIDs (`client_badges`), in display order.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub badges: Vec<String>,
	/// Icon id (`client_icon_id`); 0: none.
	#[serde(default, skip_serializing_if = "is_zero_u32")]
	pub icon: u32,
	#[serde(default, skip_serializing_if = "is_false")]
	pub recording: bool,
	#[serde(default, skip_serializing_if = "is_false")]
	pub priority_speaker: bool,
	#[serde(default, skip_serializing_if = "is_false")]
	pub channel_commander: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub database_id: Option<u64>,
}

/// Badge GUIDs of a `client_badges` value
/// (`Overwolf=0:badges=<guid>,<guid>`).
pub fn parse_badges(value: &str) -> Vec<String> {
	value
		.split(':')
		.filter_map(|part| part.strip_prefix("badges="))
		.flat_map(|list| list.split(','))
		.map(str::trim)
		.filter(|g| !g.is_empty())
		.map(str::to_owned)
		.collect()
}

/// How a group's name is shown next to its members.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupNamingMode {
	#[default]
	None,
	/// Before the nickname: `[Admin] Alice`.
	Before,
	/// After the nickname: `Alice [Admin]`.
	After,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupType {
	/// For new virtual servers; nobody is a member.
	Template,
	#[default]
	Regular,
	/// ServerQuery clients.
	Query,
}

/// A server or channel group.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
	pub id: GroupId,
	pub name: String,
	/// Icon id; 0: none.
	#[serde(default, skip_serializing_if = "is_zero_u32")]
	pub icon: u32,
	/// Display order: lower first, then by id.
	#[serde(default)]
	pub sort_id: i32,
	#[serde(default)]
	pub naming_mode: GroupNamingMode,
	#[serde(default)]
	pub group_type: GroupType,
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
	/// What the source knows about the server (a voice connection: all of
	/// it; other sources: nothing).
	pub server: ServerDetails,
	pub server_groups: BTreeMap<GroupId, GroupInfo>,
	pub channel_groups: BTreeMap<GroupId, GroupInfo>,
}

impl Presence {
	pub fn from_snapshot(s: PresenceSnapshot) -> Self {
		Self {
			server_name: s.server_name,
			channels: s.channels.into_iter().map(|c| (c.id, c)).collect(),
			clients: s.clients.into_iter().map(|c| (c.id, c)).collect(),
			..Self::default()
		}
	}

	/// The client with this unique id, if present (the first of several
	/// connections with the same identity).
	pub fn client_by_uid(&self, uid: &str) -> Option<&ClientInfo> {
		self.clients.values().find(|c| c.uid.as_deref() == Some(uid))
	}

	/// Groups in display order (sort id, then id).
	pub fn sorted_groups(groups: &BTreeMap<GroupId, GroupInfo>) -> Vec<&GroupInfo> {
		let mut sorted: Vec<_> = groups.values().collect();
		sorted.sort_by_key(|g| (g.sort_id, g.id));
		sorted
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
	fn badges_and_groups() {
		assert_eq!(
			parse_badges(
				"Overwolf=0:badges=c9e97536-5a2d-4c8e-a135-af404587a472,450f81c1-ab41-4211-a338-222fa94ed157"
			),
			["c9e97536-5a2d-4c8e-a135-af404587a472", "450f81c1-ab41-4211-a338-222fa94ed157"]
		);
		assert!(parse_badges("").is_empty());
		assert!(parse_badges("Overwolf=1").is_empty());
		let group = |id, sort_id| GroupInfo { id, sort_id, ..Default::default() };
		let groups = BTreeMap::from([(1, group(1, 20)), (2, group(2, 10)), (3, group(3, 10))]);
		let order: Vec<_> = Presence::sorted_groups(&groups).iter().map(|g| g.id).collect();
		assert_eq!(order, [2, 3, 1]);
		let mut alice = cl(1, "Alice", 1);
		alice.uid = Some("A=".into());
		let p = presence(vec![ch(1, "Lobby")], vec![alice, cl(2, "Bob", 1)]);
		assert_eq!(p.client_by_uid("A=").map(|c| c.id), Some(1));
		assert!(p.client_by_uid("B=").is_none());
	}

	#[test]
	fn old_json_without_new_fields() {
		let c: ClientInfo =
			serde_json::from_str(r#"{"id":1,"nickname":"a","channel":2,"away":""}"#).unwrap();
		assert_eq!(c.away.as_deref(), Some(""));
		assert_eq!((c.talk_power, c.avatar.as_ref(), c.badges.len()), (0, None, 0));
		// Defaults stay off the wire.
		assert_eq!(
			serde_json::to_string(&cl(1, "a", 2)).unwrap(),
			r#"{"id":1,"nickname":"a","channel":2,"is_query":false,"input_muted":false,"output_muted":false}"#
		);
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
