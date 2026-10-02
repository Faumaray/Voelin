//! The channel tree, the members of our channel and everyone on the
//! server for the members panel.

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
	["admin", "owner", "operator", "leiter"].iter().any(|w| lower.contains(w))
}

/// Moderator groups, shown as a role next to the name.
fn is_moderator_group(name: &str) -> bool {
	name.to_lowercase().contains("mod")
}

impl TreeInput<'_> {
	/// The position of a client's group in display order (no group last).
	fn group_rank(&self, client: &ClientInfo) -> usize {
		self.groups.iter().position(|g| client.server_groups.contains(&g.id)).unwrap_or(usize::MAX)
	}

	/// One row of the members panel or the voice channel, under `section`.
	fn member(&self, c: &ClientInfo, section: &Section) -> MemberItem {
		let talking = talking(self, c);
		let group = self.group_of(c).map_or("", |g| g.name.as_str());
		let admin = is_admin_group(group);
		// Someone elsewhere on the server: where they are says more than
		// "Listening".
		let elsewhere = self.own_channel != Some(c.channel)
			&& !talking
			&& c.streaming != Some(true)
			&& c.away.is_none();
		let status = match self.presence.channels.get(&c.channel) {
			Some(channel) if elsewhere => format!("In {}", channel.name),
			_ => status_of(c, talking),
		};
		MemberItem {
			id: c.id as i32,
			name: c.nickname.clone().into(),
			initials: avatar::initials(&c.nickname).into(),
			tint: avatar::tint(&c.nickname),
			avatar: self.avatar(c),
			talking,
			muted: c.input_muted,
			sound_off: c.output_muted,
			away: c.away.is_some(),
			streaming: c.streaming == Some(true),
			myself: self.own_client == Some(c.id),
			status: status.into(),
			away_message: c.away.clone().unwrap_or_default().into(),
			group: section.title.clone().into(),
			group_icon: section.icon.clone(),
			first_in_group: false,
			group_count: 0,
			admin,
			role: if !admin && is_moderator_group(group) { group.into() } else { "".into() },
			priority: c.priority_speaker,
			commander: c.channel_commander,
			recording: c.recording,
			talk_power: c.talk_power,
		}
	}
}

/// A heading of the members panel.
#[derive(Clone, Default)]
struct Section {
	title: String,
	icon: slint::Image,
}

/// Rows in sections: each section's first row carries the heading and how
/// many rows follow.
fn sectioned(
	input: &TreeInput,
	mut clients: Vec<(Section, usize, &ClientInfo)>,
) -> Vec<MemberItem> {
	clients.sort_by_key(|(_, rank, c)| (*rank, c.nickname.to_lowercase(), c.id));
	let mut rows: Vec<MemberItem> = clients.iter().map(|(s, _, c)| input.member(c, s)).collect();
	let mut start = 0;
	while start < rows.len() {
		let rank = clients[start].1;
		let count = clients[start..].iter().take_while(|(_, r, _)| *r == rank).count();
		rows[start].first_in_group = true;
		rows[start].group_count = count as i32;
		start += count;
	}
	rows
}

/// The section of a client's server group.
fn group_section(input: &TreeInput, client: &ClientInfo) -> (Section, usize) {
	match input.group_of(client) {
		Some(g) => (
			Section {
				title: g.name.clone(),
				icon: avatar::image(
					Some(g.icon).filter(|i| *i != 0).and_then(|i| input.icons.get(&i)),
				),
			},
			1 + input.group_rank(client),
		),
		None => (Section { title: "Online".into(), ..Default::default() }, usize::MAX),
	}
}

/// The clients in our channel, grouped by server group (in the server's
/// display order) and by name inside a group.
pub fn members(input: &TreeInput) -> Vec<MemberItem> {
	let Some(channel) = input.own_channel else { return Vec::new() };
	let clients = input
		.presence
		.members(channel)
		.map(|c| {
			let (section, rank) = group_section(input, c);
			(section, rank, c)
		})
		.collect();
	sectioned(input, clients)
}

/// The rows whose name contains `filter` (lower case), in sections
/// counted again.
pub fn matching(rows: Vec<MemberItem>, filter: &str) -> Vec<MemberItem> {
	let mut rows: Vec<MemberItem> =
		rows.into_iter().filter(|m| m.name.to_lowercase().contains(filter)).collect();
	let mut start = 0;
	while start < rows.len() {
		let group = rows[start].group.clone();
		let count = rows[start..].iter().take_while(|m| m.group == group).count();
		for (i, row) in rows[start..start + count].iter_mut().enumerate() {
			row.first_in_group = i == 0;
			row.group_count = if i == 0 { count as i32 } else { 0 };
		}
		start += count;
	}
	rows
}

/// Everyone on the server for the members panel: first the people in our
/// voice channel (`voice_first`, in the voice channel and stream views) or
/// those streaming, then each server group in the server's order.
pub fn server_members(input: &TreeInput, voice_first: bool) -> Vec<MemberItem> {
	let first = Section {
		title: if voice_first { "In Voice" } else { "Streaming" }.into(),
		icon: slint::Image::default(),
	};
	let clients = input
		.presence
		.clients
		.values()
		.filter(|c| !c.is_query)
		.map(|c| {
			let lead = if voice_first {
				input.own_channel == Some(c.channel)
			} else {
				c.streaming == Some(true)
			};
			if lead {
				(first.clone(), 0, c)
			} else {
				let (section, rank) = group_section(input, c);
				(section, rank, c)
			}
		})
		.collect();
	sectioned(input, clients)
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

	#[test]
	fn everyone_in_sections() {
		let mut p = presence();
		p.clients.get_mut(&10).unwrap().server_groups = vec![7];
		p.clients.get_mut(&12).unwrap().server_groups = vec![7];
		p.clients.get_mut(&12).unwrap().streaming = Some(true);
		p.clients.get_mut(&11).unwrap().server_groups = vec![6];
		let group = |id, sort_id, name: &str| GroupInfo {
			id,
			name: name.into(),
			icon: 0,
			sort_id,
			..Default::default()
		};
		let e = Extras {
			groups: vec![group(6, 10, "Moderator"), group(7, 20, "Guest")],
			..Default::default()
		};
		let (t, c, pb) = (HashSet::new(), HashSet::new(), ClientPlaybackMap::new());
		let rows = |voice_first| {
			server_members(&input(&p, &t, &c, &pb, "", &e), voice_first)
				.iter()
				.map(|m| {
					(
						m.name.to_string(),
						m.first_in_group.then(|| format!("{} — {}", m.group, m.group_count)),
						m.status.to_string(),
					)
				})
				.collect::<Vec<_>>()
		};
		let row = |name: &str, head: Option<&str>, status: &str| {
			(name.to_owned(), head.map(str::to_owned), status.to_owned())
		};
		// The query client is left out; people elsewhere show their channel.
		assert_eq!(
			rows(false),
			[
				row("Carol", Some("Streaming — 1"), "Streaming"),
				row("bob", Some("Moderator — 1"), "Listening"),
				row("Alice", Some("Guest — 1"), "In Lobby"),
			]
		);
		assert_eq!(
			rows(true),
			[
				row("bob", Some("In Voice — 1"), "Listening"),
				row("Alice", Some("Guest — 2"), "In Lobby"),
				row("Carol", None, "Streaming"),
			]
		);
		let all = server_members(&input(&p, &t, &c, &pb, "", &e), false);
		assert_eq!(all[1].role, "Moderator");
		assert!(!all[1].admin);
		// The search keeps the sections of what it finds.
		let found = matching(all, "al");
		assert_eq!(found.len(), 1);
		assert!(found[0].first_in_group && found[0].group == "Guest" && found[0].group_count == 1);
	}
}
