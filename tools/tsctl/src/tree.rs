//! Render the channel tree of a connection as text.

use std::collections::HashMap;
use std::fmt::Write;

use tsclientlib::data;
use tsclientlib::{ChannelId, ClientType};

/// Minimal channel view, so the ordering logic can be tested without a connection.
#[derive(Clone, Debug)]
pub struct ChannelNode {
	pub id: ChannelId,
	pub parent: ChannelId,
	/// Id of the sibling this channel is sorted below, `0` for the first one.
	pub order: ChannelId,
	pub name: String,
}

#[derive(Clone, Debug)]
pub struct ClientNode {
	pub channel: ChannelId,
	pub name: String,
	pub talk_power: i32,
	pub is_query: bool,
	pub input_muted: bool,
	pub output_muted: bool,
	pub away: bool,
}

/// Sort siblings by following the `order` links (each channel names the
/// sibling directly above it). Channels whose predecessor is missing are
/// appended at the end, sorted by id, so nothing is ever dropped.
pub fn order_siblings(mut siblings: Vec<&ChannelNode>) -> Vec<&ChannelNode> {
	siblings.sort_by_key(|c| c.id.0);
	let mut by_prev: HashMap<ChannelId, &ChannelNode> = HashMap::new();
	for c in &siblings {
		by_prev.entry(c.order).or_insert(c);
	}
	let mut sorted = Vec::with_capacity(siblings.len());
	let mut prev = ChannelId(0);
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

pub fn render(server_name: &str, channels: &[ChannelNode], clients: &[ClientNode]) -> String {
	let mut out = String::new();
	let _ = writeln!(out, "{server_name}");
	render_level(&mut out, channels, clients, ChannelId(0), 0);
	out
}

fn render_level(
	out: &mut String,
	channels: &[ChannelNode],
	clients: &[ClientNode],
	parent: ChannelId,
	depth: usize,
) {
	let siblings = order_siblings(channels.iter().filter(|c| c.parent == parent).collect());
	let indent = "  ".repeat(depth);
	for channel in siblings {
		let _ = writeln!(out, "{indent}# {} (cid {})", channel.name, channel.id.0);
		let mut members: Vec<_> =
			clients.iter().filter(|c| c.channel == channel.id && !c.is_query).collect();
		members.sort_by(|a, b| b.talk_power.cmp(&a.talk_power).then_with(|| a.name.cmp(&b.name)));
		for client in members {
			let mut flags = Vec::new();
			if client.input_muted {
				flags.push("mic off");
			}
			if client.output_muted {
				flags.push("sound off");
			}
			if client.away {
				flags.push("away");
			}
			let flags =
				if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join(", ")) };
			let _ = writeln!(out, "{indent}  - {}{flags}", client.name);
		}
		render_level(out, channels, clients, channel.id, depth + 1);
	}
}

pub fn from_state(state: &data::Connection) -> String {
	let channels: Vec<_> = state
		.channels
		.values()
		.map(|c| ChannelNode { id: c.id, parent: c.parent, order: c.order, name: c.name.clone() })
		.collect();
	let clients: Vec<_> = state
		.clients
		.values()
		.map(|c| ClientNode {
			channel: c.channel,
			name: c.name.clone(),
			talk_power: c.talk_power,
			is_query: matches!(c.client_type, ClientType::Query { .. }),
			input_muted: c.input_muted,
			output_muted: c.output_muted,
			away: c.away_message.is_some(),
		})
		.collect();
	render(&state.server.name, &channels, &clients)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ch(id: u64, parent: u64, order: u64, name: &str) -> ChannelNode {
		ChannelNode {
			id: ChannelId(id),
			parent: ChannelId(parent),
			order: ChannelId(order),
			name: name.into(),
		}
	}

	fn cl(channel: u64, name: &str, talk_power: i32) -> ClientNode {
		ClientNode {
			channel: ChannelId(channel),
			name: name.into(),
			talk_power,
			is_query: false,
			input_muted: false,
			output_muted: false,
			away: false,
		}
	}

	#[test]
	fn follows_order_links() {
		// Display order: 7, 3, 5 (ids deliberately not sorted).
		let channels = [ch(5, 0, 3, "c"), ch(3, 0, 7, "b"), ch(7, 0, 0, "a")];
		let sorted = order_siblings(channels.iter().collect());
		let names: Vec<_> = sorted.iter().map(|c| c.name.as_str()).collect();
		assert_eq!(names, ["a", "b", "c"]);
	}

	#[test]
	fn keeps_channels_with_broken_links() {
		let channels = [ch(1, 0, 0, "a"), ch(2, 0, 99, "orphan")];
		let sorted = order_siblings(channels.iter().collect());
		assert_eq!(sorted.len(), 2);
		assert_eq!(sorted[1].name, "orphan");
	}

	#[test]
	fn renders_nested_tree() {
		let channels = [ch(1, 0, 0, "Lobby"), ch(2, 0, 1, "Games"), ch(3, 2, 0, "Squad")];
		let mut query = cl(1, "serveradmin", 0);
		query.is_query = true;
		let mut muted = cl(3, "Bob", 0);
		muted.input_muted = true;
		let clients = [cl(1, "Alice", 10), cl(1, "Zed", 50), query, muted];
		let text = render("Test Server", &channels, &clients);
		assert_eq!(
			text,
			"Test Server\n\
			 # Lobby (cid 1)\n  \
			 - Zed\n  \
			 - Alice\n\
			 # Games (cid 2)\n  \
			 # Squad (cid 3)\n    \
			 - Bob [mic off]\n"
		);
	}
}
