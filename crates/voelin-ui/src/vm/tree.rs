//! The channel tree, the members of our channel and everyone on the
//! server for the members panel.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub use voelin_model::channel_title;
use voelin_model::{
	BannerMode, ChannelId, ChannelInfo, ClientInfo, GroupInfo, Presence, Spacer, TreeRow, badges,
	tree_rows,
};

use crate::app::{BadgeItem, MemberItem, TreeItem};
use crate::settings::ClientPlaybackMap;
use crate::vm::avatar;

/// A banner mode as the UI takes it (`TreeItem.banner-mode`).
pub fn banner_mode(mode: BannerMode) -> i32 {
	match mode {
		BannerMode::NoAdjust => 0,
		BannerMode::IgnoreAspect => 1,
		BannerMode::KeepAspect => 2,
	}
}

/// The picture of a banner address, if the engine fetched it.
pub fn banner(pictures: &HashMap<String, PathBuf>, url: Option<&str>) -> slint::Image {
	avatar::image(url.and_then(|u| pictures.get(u)))
}

/// A badge's picture, if the engine fetched it.
fn badge_icon(pictures: &HashMap<String, PathBuf>, guid: &str) -> slint::Image {
	avatar::image(badges::icon_url(guid).and_then(|url| pictures.get(&url)))
}

/// A client's badges for the member card: the first three in its order.
/// One the app does not know is "Badge", with its GUID as description.
pub fn badge_items(guids: &[String], pictures: &HashMap<String, PathBuf>) -> Vec<BadgeItem> {
	guids
		.iter()
		.take(badges::SHOWN)
		.map(|guid| {
			let (name, description) = match badges::info(guid) {
				Some(info) => (info.name.as_str(), info.description.as_str()),
				None => ("Badge", guid.as_str()),
			};
			BadgeItem {
				name: name.into(),
				description: description.into(),
				icon: badge_icon(pictures, guid),
			}
		})
		.collect()
}

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
	/// Icons in the engine's cache, by icon id (channel, group and client
	/// icons).
	pub icons: &'a HashMap<u32, PathBuf>,
	/// Banners and badges in the engine's cache, by address.
	pub pictures: &'a HashMap<String, PathBuf>,
	/// The server groups in display order; members are grouped by the
	/// first one each client is in.
	pub groups: &'a [GroupInfo],
	/// The channel groups (their icons).
	pub channel_groups: &'a [GroupInfo],
}

impl TreeInput<'_> {
	fn avatar(&self, client: &ClientInfo) -> slint::Image {
		avatar::image(client.uid.as_ref().and_then(|uid| self.avatars.get(uid)))
	}

	/// An icon by id, if the engine fetched it (0: none).
	fn icon(&self, id: u32) -> Option<slint::Image> {
		let path = self.icons.get(&id).filter(|_| id != 0)?;
		Some(avatar::image(Some(path))).filter(|i| i.size().width > 0)
	}

	/// The icons beside a client, as TeamSpeak shows them: its server
	/// groups' in display order, its channel group's, its own.
	fn client_icons(&self, client: &ClientInfo) -> Vec<slint::Image> {
		let groups = self.groups.iter().filter(|g| client.server_groups.contains(&g.id));
		let channel_group =
			self.channel_groups.iter().filter(|g| client.channel_group == Some(g.id));
		groups
			.chain(channel_group)
			.map(|g| g.icon)
			.chain([client.icon])
			.filter_map(|id| self.icon(id))
			.collect()
	}

	/// The pictures of the badges a client shows that arrived, in its
	/// order; empty slots last.
	fn badges(&self, client: &ClientInfo) -> [slint::Image; badges::SHOWN] {
		let mut icons = client
			.badges
			.iter()
			.take(badges::SHOWN)
			.map(|guid| badge_icon(self.pictures, guid))
			.filter(|i| i.size().width > 0);
		std::array::from_fn(|_| icons.next().unwrap_or_default())
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

/// A spacer kind as the UI takes it (`TreeItem.spacer`, 0: no spacer).
fn spacer_index(kind: Spacer) -> i32 {
	match kind {
		Spacer::Left => 1,
		Spacer::Center => 2,
		Spacer::Right => 3,
		Spacer::Fill => 4,
	}
}

/// A channel's title for lists and pickers (the search, the event form);
/// none for a spacer that only separates.
pub fn listed_title(channel: &ChannelInfo) -> Option<&str> {
	match channel_title(channel) {
		(text, Some(kind)) if kind.separates(text) => None,
		(text, _) => Some(text),
	}
}

/// Channels deeper than this are not followed up (a broken presence could
/// loop).
const PATH_DEPTH: usize = 64;

/// The path of a channel, its names from the top as the server has them
/// (what an invite link names, what connecting into it takes); empty for a
/// channel the presence does not know.
pub fn channel_path(presence: &Presence, channel: ChannelId) -> Vec<String> {
	let mut names = Vec::new();
	let mut next = presence.channels.get(&channel);
	while let Some(c) = next.filter(|_| names.len() < PATH_DEPTH) {
		names.push(c.name.clone());
		next = presence.channels.get(&c.parent).filter(|_| c.parent != 0);
	}
	names.reverse();
	names
}

/// About as many characters as a fill spacer's row holds: more than the
/// widest tree shows.
const FILL_CHARS: usize = 200;

/// A spacer's text as its row shows it: a fill pattern repeated across the
/// row, a blank text empty.
fn spacer_text(kind: Spacer, text: &str) -> String {
	match kind {
		Spacer::Fill => match text.chars().count() {
			0 => String::new(),
			n => text.repeat((FILL_CHARS / n).max(1)),
		},
		_ if text.trim().is_empty() => String::new(),
		_ => text.to_owned(),
	}
}

/// The rows of the tree, channels with their clients below.
pub fn rows(input: &TreeInput) -> Vec<TreeItem> {
	let p = input.presence;
	let rows: Vec<TreeItem> = tree_rows(p, &|cid| input.collapsed.contains(&cid))
		.into_iter()
		.map(|row| match row {
			TreeRow::Channel { depth, channel } => {
				let (name, spacer) = match channel_title(channel) {
					(text, Some(kind)) => (spacer_text(kind, text), spacer_index(kind)),
					(name, None) => (name.to_owned(), 0),
				};
				TreeItem {
					spacer,
					is_channel: true,
					depth: depth as i32,
					id: channel.id as i32,
					name: name.into(),
					own: input.own_channel == Some(channel.id),
					locked: channel.has_password,
					collapsed: input.collapsed.contains(&channel.id),
					local_volume: 100,
					members: p.members(channel.id).count() as i32,
					max_members: channel.max_clients.filter(|m| *m >= 0).unwrap_or(-1),
					icon: input.icon(channel.icon).unwrap_or_default(),
					banner: banner(input.pictures, channel.banner_gfx_url.as_deref()),
					banner_mode: banner_mode(channel.banner_mode),
					..Default::default()
				}
			}
			TreeRow::Client { depth, client } => {
				let playback = client
					.uid
					.as_ref()
					.and_then(|uid| input.playback.get(uid))
					.copied()
					.unwrap_or_default();
				let mut icons = input.client_icons(client).into_iter();
				let [badge, badge_2, badge_3] = input.badges(client);
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
					icon: icons.next().unwrap_or_default(),
					icon_2: icons.next().unwrap_or_default(),
					icon_3: icons.next().unwrap_or_default(),
					badge,
					badge_2,
					badge_3,
					..Default::default()
				}
			}
		})
		.collect();
	filter(rows, input.filter)
}

/// A spacer row that only separates: a line, or no text.
fn separator(row: &TreeItem) -> bool {
	row.is_channel
		&& (row.spacer == spacer_index(Spacer::Fill) || (row.spacer != 0 && row.name.is_empty()))
}

/// Rows that match `query`, and the channels above them. A separator
/// never matches (`---` is no channel to look for), but stays above what
/// does.
fn filter(rows: Vec<TreeItem>, query: &str) -> Vec<TreeItem> {
	let query = query.trim().to_lowercase();
	if query.is_empty() {
		return rows;
	}
	let matches: Vec<bool> = rows
		.iter()
		.map(|r| !separator(r) && r.name.to_lowercase().contains(query.as_str()))
		.collect();
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

/// Whether a client cannot speak in its channel: the channel needs more
/// talk power than it has, and it was not made a talker.
pub fn cannot_talk(channel: Option<&ChannelInfo>, client: &ClientInfo) -> bool {
	channel.is_some_and(|c| {
		c.needed_talk_power > 0 && client.talk_power < c.needed_talk_power && !client.talker
	})
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
		let channel = self.presence.channels.get(&c.channel);
		let status = match channel {
			Some(channel) if elsewhere => format!("In {}", channel_title(channel).0),
			_ => status_of(c, talking),
		};
		let [badge, badge_2, badge_3] = self.badges(c);
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
			cannot_talk: cannot_talk(channel, c),
			badge,
			badge_2,
			badge_3,
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
			Section { title: g.name.clone(), icon: input.icon(g.icon).unwrap_or_default() },
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
		pictures: HashMap<String, PathBuf>,
		groups: Vec<GroupInfo>,
		channel_groups: Vec<GroupInfo>,
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
			pictures: &extras.pictures,
			groups: &extras.groups,
			channel_groups: &extras.channel_groups,
		}
	}

	/// A PNG of this width in a file (the engine's cache names them without
	/// an extension).
	fn picture_file(dir: &std::path::Path, name: &str, width: u32) -> PathBuf {
		let mut png = Vec::new();
		let mut encoder = png::Encoder::new(&mut png, width, 2);
		encoder.set_color(png::ColorType::Rgba);
		encoder.set_depth(png::BitDepth::Eight);
		encoder.write_header().unwrap().write_image_data(&vec![120; width as usize * 8]).unwrap();
		let path = dir.join(name);
		std::fs::write(&path, png).unwrap();
		path
	}

	/// Avatars, channel icons and banners, group and client icons (in
	/// TeamSpeak's order, those that arrived) reach the rows.
	#[test]
	fn pictures_in_rows() {
		let dir = std::env::temp_dir().join(format!("voelin-tree-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let mut p = presence();
		let lobby = p.channels.get_mut(&1).unwrap();
		lobby.icon = 2001;
		lobby.banner_gfx_url = Some("https://e.com/lobby.png".into());
		lobby.banner_mode = BannerMode::KeepAspect;
		let alice = p.clients.get_mut(&10).unwrap();
		alice.uid = Some("alice=".into());
		alice.server_groups = vec![7, 6];
		alice.channel_group = Some(5);
		alice.icon = 4004;
		let group = |id, sort_id, icon| GroupInfo { id, icon, sort_id, ..Default::default() };
		let e = Extras {
			avatars: HashMap::from([("alice=".into(), picture_file(&dir, "avatar", 5))]),
			icons: HashMap::from([
				(2001, picture_file(&dir, "2001", 7)),
				(3003, picture_file(&dir, "3003", 3)),
				(4004, picture_file(&dir, "4004", 4)),
				(5005, picture_file(&dir, "5005", 6)),
			]),
			pictures: HashMap::from([(
				"https://e.com/lobby.png".into(),
				picture_file(&dir, "banner", 9),
			)]),
			// Group 7 sorts first; group 6's icon has not arrived.
			groups: vec![group(7, 10, 3003), group(6, 20, 9999)],
			channel_groups: vec![group(5, 0, 5005)],
		};
		let (t, c, pb) = (HashSet::new(), HashSet::new(), ClientPlaybackMap::new());
		let rows = rows(&input(&p, &t, &c, &pb, "", &e));
		let width = |i: &slint::Image| i.size().width;
		let lobby = &rows[0];
		assert_eq!((width(&lobby.icon), width(&lobby.banner), lobby.banner_mode), (7, 9, 2));
		let alice = &rows[1];
		assert_eq!(width(&alice.avatar), 5);
		assert_eq!([&alice.icon, &alice.icon_2, &alice.icon_3].map(width), [3, 6, 4]);
		// Nothing for the others.
		let games = &rows[2];
		assert_eq!((width(&games.icon), width(&games.banner), games.banner_mode), (0, 0, 0));
		assert_eq!(width(&rows[3].avatar), 0);
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// A client's badges: the first three in its order, on the card by name
	/// ("Badge" for one the app does not know), in the rows the pictures
	/// that arrived.
	#[test]
	fn badges_on_the_card_and_in_rows() {
		let dir = std::env::temp_dir().join(format!("voelin-tree-badges-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let guids: Vec<String> = [
			"4b27be5a-b92a-4b30-8b2d-14b59653f427",
			"00000000-0000-0000-0000-000000000000",
			"05114019-6b46-4b13-b5a1-e5179ef69fb5",
			"2bf80270-8efe-46dc-a472-3280a0479145",
		]
		.map(String::from)
		.into();
		let picture = |i: usize, width| {
			(badges::icon_url(&guids[i]).unwrap(), picture_file(&dir, &format!("badge-{i}"), width))
		};
		let e = Extras {
			pictures: HashMap::from([picture(0, 3), picture(2, 5), picture(3, 7)]),
			..Default::default()
		};
		let width = |i: &slint::Image| i.size().width;
		let card = badge_items(&guids, &e.pictures);
		let names: Vec<_> = card.iter().map(|b| b.name.as_str()).collect();
		assert_eq!(names, ["20th Anniversary", "Badge", "April Fools!"]);
		assert_eq!(card[1].description.as_str(), guids[1]);
		assert_eq!(card.iter().map(|b| width(&b.icon)).collect::<Vec<_>>(), [3, 0, 5]);

		let mut p = presence();
		p.clients.get_mut(&11).unwrap().badges = guids;
		let (t, c, pb) = (HashSet::new(), HashSet::new(), ClientPlaybackMap::new());
		let tree = input(&p, &t, &c, &pb, "", &e);
		let bob = rows(&tree).into_iter().find(|r| !r.is_channel && r.id == 11).unwrap();
		assert_eq!([&bob.badge, &bob.badge_2, &bob.badge_3].map(width), [3, 5, 0]);
		let bob = members(&tree).into_iter().find(|m| m.id == 11).unwrap();
		assert_eq!([&bob.badge, &bob.badge_2, &bob.badge_3].map(width), [3, 5, 0]);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn spacer_rows_keep_tree_identity() {
		let mut p = presence();
		p.channels.get_mut(&2).unwrap().name = "[cspacer12]Гамесы".into();
		let more = |id, order, name: &str| ChannelInfo {
			id,
			order,
			name: name.into(),
			has_password: id == 5,
			..Default::default()
		};
		for c in
			[more(4, 2, "[*spacer1]-="), more(5, 4, "[rspacer2]Staff"), more(6, 5, "[spacer3]")]
		{
			p.channels.insert(c.id, c);
		}
		let (t, c, pb) = (HashSet::new(), HashSet::from([2]), ClientPlaybackMap::new());
		let e = Extras::default();
		let rows = rows(&input(&p, &t, &c, &pb, "", &e));
		let channels: Vec<_> = rows
			.iter()
			.filter(|r| r.is_channel)
			.map(|r| (r.id, r.spacer, r.name.chars().take(6).collect::<String>()))
			.collect();
		let row = |id, spacer, name: &str| (id, spacer, name.to_owned());
		assert_eq!(
			channels,
			[
				row(1, 0, "Lobby"),
				row(2, 2, "Гамесы"),
				row(4, 4, "-=-=-="),
				row(5, 3, "Staff"),
				row(6, 1, ""),
			]
		);
		// A fill line repeats its pattern across the row.
		let fill = &rows.iter().find(|r| r.id == 4 && r.is_channel).unwrap().name;
		assert_eq!(fill.chars().count(), FILL_CHARS);
		assert_eq!(spacer_text(Spacer::Fill, ""), "");
		assert_eq!(spacer_text(Spacer::Fill, "x".repeat(300).as_str()).len(), 300);
		let games = rows.iter().find(|r| r.id == 2 && r.is_channel).unwrap();
		assert!(games.collapsed && games.members == 1);
		assert!(rows.iter().any(|r| r.id == 5 && r.is_channel && r.locked));
		// Only top-level channels are spacers: a sub-channel keeps its name.
		p.channels.get_mut(&3).unwrap().name = "[cspacer1]Chess".into();
		let c = HashSet::new();
		let rows = super::rows(&input(&p, &t, &c, &pb, "", &e));
		let chess = rows.iter().find(|r| r.is_channel && r.id == 3).unwrap();
		assert_eq!((chess.name.as_str(), chess.spacer), ("[cspacer1]Chess", 0));
		assert_eq!(channel_title(&p.channels[&2]), ("Гамесы", Some(Spacer::Center)));
		assert_eq!(channel_title(&p.channels[&3]), ("[cspacer1]Chess", None));
		// Lists and pickers leave out what only separates.
		let listed: Vec<_> = p.channels.values().filter_map(listed_title).collect();
		assert_eq!(listed, ["Lobby", "Гамесы", "[cspacer1]Chess", "Staff"]);
	}

	/// The search finds a spacer by its text, never a separator, but keeps
	/// separators above what it finds.
	#[test]
	fn search_skips_separators() {
		let mut p = presence();
		p.channels.get_mut(&2).unwrap().name = "[*spacer1]---".into();
		p.channels.get_mut(&1).unwrap().name = "[cspacer]Lobby".into();
		p.channels.insert(
			4,
			ChannelInfo { id: 4, order: 2, name: "[spacer4]".into(), ..Default::default() },
		);
		let (t, c, pb) = (HashSet::new(), HashSet::new(), ClientPlaybackMap::new());
		let e = Extras::default();
		let names = |filter| {
			super::rows(&input(&p, &t, &c, &pb, filter, &e))
				.iter()
				.map(|r| r.name.chars().take(3).collect::<String>())
				.collect::<Vec<_>>()
		};
		assert!(names("-").is_empty());
		assert!(names("spacer").is_empty());
		assert_eq!(names("lob"), ["Lob"]);
		// The line stays as the parent of Chess.
		assert_eq!(names("chess"), ["---", "Che"]);
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
	fn members_who_cannot_talk() {
		let channel = |needed| ChannelInfo { needed_talk_power: needed, ..Default::default() };
		let client = |talk_power, talker| ClientInfo { talk_power, talker, ..Default::default() };
		for (needed, power, talker, cannot) in [
			(50, 75, false, false),
			(50, 50, false, false),
			(50, 0, false, true),
			(50, 49, false, true),
			// A talker speaks without talk power.
			(50, 0, true, false),
			// The channel needs none.
			(0, 0, false, false),
		] {
			let case = format!("needed {needed}, power {power}, talker {talker}");
			assert_eq!(
				cannot_talk(Some(&channel(needed)), &client(power, talker)),
				cannot,
				"{case}"
			);
		}
		assert!(!cannot_talk(None, &client(0, false)));
		// In the members panel: bob (talk power 75) speaks in Games, which
		// needs 50; Carol (none) cannot.
		let mut p = presence();
		p.channels.get_mut(&2).unwrap().needed_talk_power = 50;
		p.clients.get_mut(&11).unwrap().talk_power = 75;
		p.clients.get_mut(&12).unwrap().channel = 2;
		let (t, c, pb) = (HashSet::new(), HashSet::new(), ClientPlaybackMap::new());
		let e = Extras::default();
		let rows: Vec<_> = members(&input(&p, &t, &c, &pb, "", &e))
			.iter()
			.map(|m| (m.name.to_string(), m.cannot_talk))
			.collect();
		assert_eq!(rows, [("bob".to_owned(), false), ("Carol".to_owned(), true)]);
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

	#[test]
	fn channel_paths() {
		let mut p = presence();
		assert_eq!(channel_path(&p, 1), ["Lobby"], "a channel at the top");
		assert_eq!(channel_path(&p, 3), ["Games", "Chess"], "the names from the top");
		assert!(channel_path(&p, 99).is_empty(), "an unknown channel");
		// Two channels each other's parent: the path stops.
		p.channels.get_mut(&2).unwrap().parent = 3;
		assert_eq!(channel_path(&p, 3).len(), PATH_DEPTH);
	}
}
