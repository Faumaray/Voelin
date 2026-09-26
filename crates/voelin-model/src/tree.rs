//! Flatten presence into display rows in TeamSpeak's channel order.

use std::collections::HashMap;

use crate::presence::{ChannelId, ChannelInfo, ClientInfo, Presence};

/// One line of a channel tree.
#[derive(Clone, Debug, PartialEq)]
pub enum TreeRow<'a> {
	Channel { depth: usize, channel: &'a ChannelInfo },
	Client { depth: usize, client: &'a ClientInfo },
}

/// Sort siblings by following the `order` links (each channel names the
/// sibling directly above it). Channels whose predecessor is missing are
/// appended at the end, sorted by id, so nothing is ever dropped.
pub fn order_siblings(mut siblings: Vec<&ChannelInfo>) -> Vec<&ChannelInfo> {
	siblings.sort_by_key(|c| c.id);
	let mut by_prev: HashMap<ChannelId, &ChannelInfo> = HashMap::new();
	for c in &siblings {
		by_prev.entry(c.order).or_insert(c);
	}
	let mut sorted = Vec::with_capacity(siblings.len());
	let mut prev = 0;
	while let Some(c) = by_prev.remove(&prev) {
		sorted.push(c);
		prev = c.id;
	}
	for c in siblings {
		if !sorted.iter().any(|s| s.id == c.id) {
			sorted.push(c);
		}
	}
	sorted
}

/// Channels and their (non-query) clients, depth-first. Clients are sorted by
/// name; `collapsed` channels hide their sub-channels and clients.
pub fn tree_rows<'a>(p: &'a Presence, collapsed: &dyn Fn(ChannelId) -> bool) -> Vec<TreeRow<'a>> {
	let mut rows = Vec::new();
	add_level(p, 0, 0, collapsed, &mut rows);
	rows
}

fn add_level<'a>(
	p: &'a Presence,
	parent: ChannelId,
	depth: usize,
	collapsed: &dyn Fn(ChannelId) -> bool,
	rows: &mut Vec<TreeRow<'a>>,
) {
	let siblings = order_siblings(p.channels.values().filter(|c| c.parent == parent).collect());
	for channel in siblings {
		rows.push(TreeRow::Channel { depth, channel });
		if collapsed(channel.id) {
			continue;
		}
		let mut members: Vec<_> = p.members(channel.id).collect();
		members.sort_by(|a, b| a.nickname.to_lowercase().cmp(&b.nickname.to_lowercase()));
		for client in members {
			rows.push(TreeRow::Client { depth: depth + 1, client });
		}
		add_level(p, channel.id, depth + 1, collapsed, rows);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::presence::PresenceSnapshot;

	fn ch(id: u64, parent: u64, order: u64, name: &str) -> ChannelInfo {
		ChannelInfo { id, parent, order, name: name.into(), ..Default::default() }
	}

	fn cl(id: u16, name: &str, channel: u64) -> ClientInfo {
		ClientInfo { id, nickname: name.into(), channel, ..Default::default() }
	}

	#[test]
	fn follows_order_links() {
		let channels = [ch(5, 0, 3, "c"), ch(3, 0, 7, "b"), ch(7, 0, 0, "a")];
		let names: Vec<_> =
			order_siblings(channels.iter().collect()).iter().map(|c| c.name.as_str()).collect();
		assert_eq!(names, ["a", "b", "c"]);
		let broken = [ch(1, 0, 0, "a"), ch(2, 0, 99, "orphan")];
		assert_eq!(order_siblings(broken.iter().collect())[1].name, "orphan");
	}

	#[test]
	fn rows_depth_first_with_collapse() {
		let mut bot = cl(9, "bot", 1);
		bot.is_query = true;
		let p = Presence::from_snapshot(PresenceSnapshot {
			server_name: "s".into(),
			channels: vec![ch(1, 0, 0, "Lobby"), ch(2, 0, 1, "Games"), ch(3, 2, 0, "Squad")],
			clients: vec![cl(1, "zed", 1), cl(2, "Alice", 1), bot, cl(3, "Bob", 3)],
		});
		let describe = |rows: Vec<TreeRow>| -> Vec<String> {
			rows.iter()
				.map(|r| match r {
					TreeRow::Channel { depth, channel } => format!("{depth}#{}", channel.name),
					TreeRow::Client { depth, client } => format!("{depth}-{}", client.nickname),
				})
				.collect()
		};
		assert_eq!(
			describe(tree_rows(&p, &|_| false)),
			["0#Lobby", "1-Alice", "1-zed", "0#Games", "1#Squad", "2-Bob"]
		);
		assert_eq!(
			describe(tree_rows(&p, &|c| c == 2)),
			["0#Lobby", "1-Alice", "1-zed", "0#Games"]
		);
	}
}
