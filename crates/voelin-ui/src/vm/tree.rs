//! The channel tree and the members of our channel.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use voelin_model::{ChannelId, ClientInfo, GroupInfo, Presence, TreeRow, tree_rows};

use crate::app::{MemberItem, TreeItem};
use crate::settings::ClientPlaybackMap;
use crate::vm::avatar;

/// What the tree shows besides the presence.
pub struct TreeInput<'a> {
	pub presence: &'a Presence,
	pub collapsed: &'a HashSet<ChannelId>,
	pub talking: &'a HashSet<u16>,
	pub playback: &'a ClientPlaybackMap,
	pub own_channel: Option<ChannelId>,
	pub own_client: Option<u16>,
	/// Search text: only matching channels and clients (and the channels
	/// above them) are shown. Empty: everything.
	pub filter: &'a str,
	/// Avatar pictures in the engine's cache, by unique id.
	pub avatars: &'a HashMap<String, PathBuf>,
	/// Icons in the engine's cache, by icon id (group icons).
	pub icons: &'a HashMap<u32, PathBuf>,
	/// The server groups in display order; members are grouped by the
	/// first one each client is in.
	pub groups: &'a [GroupInfo],
}

impl TreeInput<'_> {
	fn avatar(&self, client: &ClientInfo) -> slint::Image {
		avatar::image(client.uid.as_ref().and_then(|uid| self.avatars.get(uid)))
	}

	/// The group a client is sorted under: the first of its server groups
	/// in display order.
	fn group_of(&self, client: &ClientInfo) -> Option<&GroupInfo> {
		self.groups.iter().find(|g| client.server_groups.contains(&g.id))
	}
}

/// "Speaking", "Away: brb", "Muted", …
pub fn status_of(client: &ClientInfo, talking: bool) -> String {
	if talking {
		"Speaking".to_owned()
	} else if client.streaming == Some(true) {
		"Streaming".to_owned()
	} else if let Some(message) = &client.away {
		if message.is_empty() { "Away".to_owned() } else { format!("Away: {message}") }
	} else if client.output_muted {
		"Sound off".to_owned()
	} else if client.input_muted {
		"Muted".to_owned()
	} else {
		"Listening".to_owned()
	}
}

fn talking(input: &TreeInput, client: &ClientInfo) -> bool {
	input.talking.contains(&client.id) || client.talking == Some(true)
}

/// The rows of the tree, channels with their clients below.
pub fn rows(input: &TreeInput) -> Vec<TreeItem> {
	let p = input.presence;
	let rows: Vec<TreeItem> = tree_rows(p, &|cid| input.collapsed.contains(&cid))
		.into_iter()
		.map(|row| match row {
			TreeRow::Channel { depth, channel } => TreeItem {
				is_channel: true,
				depth: depth as i32,
				id: channel.id as i32,
				name: channel.name.clone().into(),
				own: input.own_channel == Some(channel.id),
				locked: channel.has_password,
				collapsed: input.collapsed.contains(&channel.id),
				local_volume: 100,
				members: p.members(channel.id).count() as i32,
				max_members: channel.max_clients.filter(|m| *m >= 0).unwrap_or(-1),
				..Default::default()
			},
			TreeRow::Client { depth, client } => {
				let playback = client
					.uid
					.as_ref()
					.and_then(|uid| input.playback.get(uid))
					.copied()
					.unwrap_or_default();
				TreeItem {
					is_channel: false,
					depth: depth as i32,
					id: client.id as i32,
					name: client.nickname.clone().into(),
					talking: talking(input, client),
					muted: client.input_muted,
					sound_off: client.output_muted,
					away: client.away.is_some(),
					streaming: client.streaming == Some(true),
					local_muted: playback.muted,
					local_volume: (playback.volume * 100.0).round() as i32,
					myself: input.own_client == Some(client.id),
					initials: avatar::initials(&client.nickname).into(),
					tint: avatar::tint(&client.nickname),
					avatar: input.avatar(client),
					..Default::default()
				}
			}
		})
		.collect();
	filter(rows, input.filter)
}

/// Rows that match `query`, and the channels above them.
fn filter(rows: Vec<TreeItem>, query: &str) -> Vec<TreeItem> {
	let query = query.trim().to_lowercase();
	if query.is_empty() {
		return rows;
	}
	let matches: Vec<bool> =
		rows.iter().map(|r| r.name.to_lowercase().contains(query.as_str())).collect();
	let mut keep = matches.clone();
	// A channel stays if anything below it matches.
	for i in 0..rows.len() {
		if !rows[i].is_channel || keep[i] {
			continue;
		}
		let depth = rows[i].depth;
		keep[i] = rows[i + 1..]
			.iter()
			.zip(&matches[i + 1..])
			.take_while(|(r, _)| r.depth > depth)
			.any(|(_, m)| *m);
	}
	rows.into_iter().zip(keep).filter_map(|(r, k)| k.then_some(r)).collect()
}

/// "12 online · 8 channels"
pub fn stats(p: &Presence) -> String {
	let online = p.clients.values().filter(|c| !c.is_query).count();
	let channels = p.channels.len();
	format!("{online} online · {channels} {}", if channels == 1 { "channel" } else { "channels" })
}

/// Names of server groups whose members wear the crown (the server's
/// admins). TeamSpeak has no flag for it, so the usual names are matched.
fn is_admin_group(name: &str) -> bool {
	let lower = name.to_lowercase();
	["admin", "owner", "operator", "moderator", "leiter"].iter().any(|w| lower.contains(w))
}

/// The clients in our channel, grouped by server group (in the server's
/// display order) and by name inside a group.
pub fn members(input: &TreeInput) -> Vec<MemberItem> {
	let Some(channel) = input.own_channel else { return Vec::new() };
	let mut clients: Vec<&ClientInfo> = input.presence.members(channel).collect();
	clients.sort_by_key(|c| {
		let group = input.group_of(c);
		(
			group.map_or(i32::MAX, |g| g.sort_id),
			group.map_or(String::new(), |g| g.name.clone()),
			c.nickname.to_lowercase(),
		)
	});
	let mut previous: Option<String> = None;
	clients
		.into_iter()
		.map(|c| {
			let talking = talking(input, c);
			let group = input.group_of(c);
			let name = group.map(|g| g.name.clone()).unwrap_or_default();
			let first = previous.as_deref() != Some(name.as_str());
			previous = Some(name.clone());
			MemberItem {
				id: c.id as i32,
				name: c.nickname.clone().into(),
				initials: avatar::initials(&c.nickname).into(),
				tint: avatar::tint(&c.nickname),
				avatar: input.avatar(c),
				talking,
				muted: c.input_muted,
				sound_off: c.output_muted,
				away: c.away.is_some(),
				streaming: c.streaming == Some(true),
				myself: input.own_client == Some(c.id),
				status: status_of(c, talking).into(),
				away_message: c.away.clone().unwrap_or_default().into(),
				group: name.into(),
				group_icon: avatar::image(
					group.map(|g| g.icon).filter(|i| *i != 0).and_then(|i| input.icons.get(&i)),
				),
				first_in_group: first,
				admin: group.is_some_and(|g| is_admin_group(&g.name)),
				priority: c.priority_speaker,
				commander: c.channel_commander,
				recording: c.recording,
				talk_power: c.talk_power,
			}
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use voelin_model::ChannelInfo;

	use super::*;

	fn presence() -> Presence {
		let channel = |id, parent, order, name: &str| ChannelInfo {
			id,
			parent,
			order,
			name: name.into(),
			max_clients: if id == 2 { Some(5) } else { None },
			..Default::default()
		};
		let client = |id, channel, name: &str| ClientInfo {
			id,
			channel,
			nickname: name.into(),
			..Default::default()
		};
		let mut p = Presence::default();
		for c in [channel(1, 0, 0, "Lobby"), channel(2, 0, 1, "Games"), channel(3, 2, 0, "Chess")] {
			p.channels.insert(c.id, c);
		}
		for c in [client(10, 1, "Alice"), client(11, 2, "bob"), client(12, 3, "Carol")] {
			p.clients.insert(c.id, c);
		}
		let mut query = client(13, 1, "serveradmin");
		query.is_query = true;
		p.clients.insert(13, query);
		p
	}

	/// Empty caches and no groups, for the tests below.
	#[derive(Default)]
	struct Extras {
		avatars: HashMap<String, PathBuf>,
		icons: HashMap<u32, PathBuf>,
		groups: Vec<GroupInfo>,
	}

	fn input<'a>(
		p: &'a Presence,
		talking: &'a HashSet<u16>,
		collapsed: &'a HashSet<ChannelId>,
		playback: &'a ClientPlaybackMap,
		filter: &'a str,
		extras: &'a Extras,
	) -> TreeInput<'a> {
		TreeInput {
			presence: p,
			collapsed,
			talking,
			playback,
			own_channel: Some(2),
			own_client: Some(11),
			filter,
			avatars: &extras.avatars,
			icons: &extras.icons,
			groups: &extras.groups,
		}
	}

	#[test]
	fn tree_rows_and_counts() {
		let p = presence();
		let (t, c, pb) = (HashSet::from([11]), HashSet::new(), ClientPlaybackMap::new());
		let e = Extras::default();
		let rows = rows(&input(&p, &t, &c, &pb, "", &e));
		let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
		assert_eq!(names, ["Lobby", "Alice", "Games", "bob", "Chess", "Carol"]);
		assert_eq!((rows[0].members, rows[0].max_members), (1, -1));
		assert_eq!((rows[2].members, rows[2].max_members, rows[2].own), (1, 5, true));
		assert!(rows[3].talking && rows[3].myself);
		assert_eq!(rows[3].initials, "BO");
		assert_eq!(stats(&p), "3 online · 3 channels");
	}

	#[test]
	fn search_keeps_parents() {
		let p = presence();
		let (t, c, pb) = (HashSet::new(), HashSet::new(), ClientPlaybackMap::new());
		let e = Extras::default();
		let rows = rows(&input(&p, &t, &c, &pb, "car", &e));
		let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
		assert_eq!(names, ["Games", "Chess", "Carol"]);
		let rows = super::rows(&input(&p, &t, &c, &pb, "LOBBY", &e));
		assert_eq!(rows.len(), 1);
	}

	#[test]
	fn members_of_our_channel() {
		let mut p = presence();
		p.clients.get_mut(&12).unwrap().channel = 2;
		p.clients.get_mut(&12).unwrap().away = Some(String::new());
		let (t, c, pb) = (HashSet::new(), HashSet::new(), ClientPlaybackMap::new());
		let e = Extras::default();
		let members = members(&input(&p, &t, &c, &pb, "", &e));
		let names: Vec<_> = members.iter().map(|m| (m.name.as_str(), m.status.as_str())).collect();
		assert_eq!(names, [("bob", "Listening"), ("Carol", "Away")]);
	}

	#[test]
	fn members_are_grouped_by_server_group() {
		let mut p = presence();
		p.clients.get_mut(&12).unwrap().channel = 2;
		p.clients.get_mut(&12).unwrap().server_groups = vec![6];
		p.clients.get_mut(&11).unwrap().server_groups = vec![7];
		let group = |id, sort_id, name: &str| GroupInfo {
			id,
			name: name.into(),
			icon: 0,
			sort_id,
			..Default::default()
		};
		let e = Extras {
			groups: vec![group(6, 10, "Server Admin"), group(7, 20, "Guest")],
			..Default::default()
		};
		let (t, c, pb) = (HashSet::new(), HashSet::new(), ClientPlaybackMap::new());
		let members = members(&input(&p, &t, &c, &pb, "", &e));
		let rows: Vec<_> =
			members.iter().map(|m| (m.name.as_str(), m.group.as_str(), m.first_in_group)).collect();
		assert_eq!(rows, [("Carol", "Server Admin", true), ("bob", "Guest", true)]);
		assert!(members[0].admin && !members[1].admin);
	}
}
