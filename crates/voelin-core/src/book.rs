//! The voice connection's book (tsclientlib) as model types.

use tsclientlib::{ClientType, MaxClients, data};
use voelin_model::{
	ChannelInfo, ClientInfo, GroupInfo, GroupNamingMode, GroupType, HostBannerMode,
	HostMessageMode, Presence, ServerDetails, parse_badges,
};

fn group_info(
	id: u64,
	name: &str,
	icon: tsclientlib::IconId,
	sort_id: i32,
	naming_mode: tsclientlib::GroupNamingMode,
	group_type: tsclientlib::GroupType,
) -> GroupInfo {
	GroupInfo {
		id,
		name: name.to_owned(),
		icon: icon.0,
		sort_id,
		naming_mode: match naming_mode {
			tsclientlib::GroupNamingMode::None => GroupNamingMode::None,
			tsclientlib::GroupNamingMode::Before => GroupNamingMode::Before,
			tsclientlib::GroupNamingMode::After => GroupNamingMode::After,
		},
		group_type: match group_type {
			tsclientlib::GroupType::Template => GroupType::Template,
			tsclientlib::GroupType::Regular => GroupType::Regular,
			tsclientlib::GroupType::Query => GroupType::Query,
		},
	}
}

/// What the server tells about itself; `uid` as the voice source computed it.
fn server_details(server: &data::Server, uid: Option<&str>) -> ServerDetails {
	ServerDetails {
		name: server.name.clone(),
		uid: uid.map(str::to_owned),
		welcome_message: server.welcome_message.clone(),
		host_message: server.hostmessage.clone(),
		host_message_mode: match server.hostmessage_mode {
			tsclientlib::HostMessageMode::None => HostMessageMode::None,
			tsclientlib::HostMessageMode::Log => HostMessageMode::Log,
			tsclientlib::HostMessageMode::Modal => HostMessageMode::Modal,
			tsclientlib::HostMessageMode::Modalquit => HostMessageMode::ModalQuit,
		},
		banner_url: server.hostbanner_url.clone(),
		banner_gfx_url: server.hostbanner_gfx_url.clone(),
		banner_gfx_interval_s: server.hostbanner_gfx_interval.whole_seconds().max(0) as u64,
		banner_mode: match server.hostbanner_mode {
			tsclientlib::HostBannerMode::NoAdjust => HostBannerMode::NoAdjust,
			tsclientlib::HostBannerMode::AdjustIgnoreAspect => HostBannerMode::IgnoreAspect,
			tsclientlib::HostBannerMode::AdjustKeepAspect => HostBannerMode::KeepAspect,
		},
		host_button_tooltip: server.hostbutton_tooltip.clone(),
		host_button_url: server.hostbutton_url.clone(),
		host_button_gfx_url: server.hostbutton_gfx_url.clone(),
		icon: server.icon.0,
		platform: server.platform.clone(),
		version: server.version.clone(),
		max_clients: server.max_clients,
		default_server_group: Some(server.default_server_group.0),
		default_channel_group: Some(server.default_channel_group.0),
	}
}

/// The presence visible through a voice connection; `server_uid` as
/// the voice source computed it, `talking` the clients talking now.
pub(crate) fn presence_from_book(
	book: &data::Connection,
	server_uid: Option<&str>,
	talking: impl Fn(u16) -> bool,
) -> Presence {
	let limit = |m: &Option<MaxClients>| match m {
		Some(MaxClients::Limited(n)) => Some(*n as i32),
		_ => None,
	};
	Presence {
		server_name: book.server.name.clone(),
		server: server_details(&book.server, server_uid),
		server_groups: book
			.server_groups
			.values()
			.map(|g| {
				let info =
					group_info(g.id.0, &g.name, g.icon, g.sort_id, g.naming_mode, g.group_type);
				(g.id.0, info)
			})
			.collect(),
		channel_groups: book
			.channel_groups
			.values()
			.map(|g| {
				let info =
					group_info(g.id.0, &g.name, g.icon, g.sort_id, g.naming_mode, g.group_type);
				(g.id.0, info)
			})
			.collect(),
		channels: book
			.channels
			.values()
			.map(|c| {
				(
					c.id.0,
					ChannelInfo {
						id: c.id.0,
						parent: c.parent.0,
						order: c.order.0,
						name: c.name.clone(),
						topic: c.topic.clone().filter(|t| !t.is_empty()),
						has_password: c.has_password.unwrap_or(false),
						max_clients: limit(&c.max_clients),
						needed_subscribe_power: 0,
						needed_talk_power: c.needed_talk_power.unwrap_or(0),
						is_default: c.is_default.unwrap_or(false),
						icon: c.icon.map_or(0, |i| i.0),
					},
				)
			})
			.collect(),
		clients: book
			.clients
			.values()
			.map(|c| {
				let mut server_groups: Vec<u64> = c.server_groups.iter().map(|g| g.0).collect();
				server_groups.sort_unstable();
				(
					c.id.0,
					ClientInfo {
						id: c.id.0,
						uid: c.uid.as_ref().map(|u| u.as_ref().to_string()),
						nickname: c.name.clone(),
						channel: c.channel.0,
						is_query: matches!(c.client_type, ClientType::Query { .. }),
						away: c.away_message.clone(),
						input_muted: c.input_muted,
						output_muted: c.output_muted,
						talking: Some(talking(c.id.0)),
						streaming: c.is_streaming,
						server_groups,
						country: Some(c.country_code.clone()).filter(|c| !c.is_empty()),
						avatar: Some(c.avatar_hash.clone()).filter(|h| !h.is_empty()),
						description: Some(c.description.clone()).filter(|d| !d.is_empty()),
						talk_power: c.talk_power,
						talker: c.talk_power_granted,
						channel_group: Some(c.channel_group.0),
						badges: parse_badges(&c.badges),
						icon: c.icon.0,
						recording: c.is_recording,
						priority_speaker: c.is_priority_speaker,
						channel_commander: c.is_channel_commander,
						database_id: Some(c.database_id.0),
					},
				)
			})
			.collect(),
	}
}
