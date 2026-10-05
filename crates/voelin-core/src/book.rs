//! The voice connection's book (tsclientlib) as model types.

use tsclientlib::{ClientType, MaxClients, data};
use voelin_model::{
	BannerMode, ChannelInfo, ClientInfo, GroupInfo, GroupNamingMode, GroupType, HostMessageMode,
	Presence, ServerDetails, myts_avatar_url, parse_badges,
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
			tsclientlib::HostBannerMode::NoAdjust => BannerMode::NoAdjust,
			tsclientlib::HostBannerMode::AdjustIgnoreAspect => BannerMode::IgnoreAspect,
			tsclientlib::HostBannerMode::AdjustKeepAspect => BannerMode::KeepAspect,
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
						banner_gfx_url: c.banner_gfx_url.clone().filter(|u| !u.is_empty()),
						// A number the book keeps as text.
						banner_mode: c
							.banner_mode
							.as_deref()
							.map_or(BannerMode::NoAdjust, BannerMode::from_wire),
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
						myts_avatar: c.my_team_speak_avatar.as_deref().and_then(myts_avatar_url),
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

#[cfg(test)]
mod tests {
	use tsclientlib::InMessage;
	use tsproto_packets::packets::{Direction, Flags, OutPacket, PacketType};
	use tsproto_types::crypto::EccKeyPrivP256;

	use super::*;

	fn message(text: &str) -> InMessage {
		let packet = OutPacket::new_with_dir(Direction::S2C, Flags::empty(), PacketType::Command);
		InMessage::new(&packet.header(), text.as_bytes()).unwrap()
	}

	/// The book of a TeamSpeak 6 server (6.0.0-beta13.1) after `initserver`.
	fn ts6_book() -> data::Connection {
		let InMessage::InitServer(init) = message(concat!(
			"initserver virtualserver_name=Test virtualserver_welcomemessage ",
			"virtualserver_platform=Linux virtualserver_version=6.0.0-beta13.1 ",
			"virtualserver_maxclients=32 virtualserver_created=0 ",
			"virtualserver_codec_encryption_mode=0 virtualserver_hostmessage ",
			"virtualserver_hostmessage_mode=0 virtualserver_default_server_group=8 ",
			"virtualserver_default_channel_group=8 virtualserver_hostbanner_url ",
			r"virtualserver_hostbanner_gfx_url=https:\/\/example.com\/banner.png ",
			"virtualserver_hostbanner_gfx_interval=60 ",
			"virtualserver_priority_speaker_dimm_modificator=-18.0000 virtualserver_id=1 ",
			"virtualserver_ask_for_privilegekey=0 ",
			"virtualserver_hostbutton_tooltip virtualserver_hostbutton_url ",
			"virtualserver_hostbutton_gfx_url virtualserver_name_phonetic ",
			"virtualserver_icon_id=0 virtualserver_hostbanner_mode=2 ",
			"virtualserver_channel_temp_delete_delay_default=0 acn=t aclid=2 pv=7 ",
			"client_talk_power=75 client_needed_serverquery_view_power=75",
		)) else {
			panic!("not an initserver");
		};
		data::Connection::new(EccKeyPrivP256::create().to_pub(), &init)
	}

	/// A `channellist` entry as TeamSpeak 6 sends it.
	fn channel(cid: u64, order: u64, name: &str, banner: &str) -> String {
		format!(
			"cid={cid} cpid=0 channel_name={name} channel_topic channel_codec=4 \
			 channel_codec_quality=6 channel_maxclients=-1 channel_maxfamilyclients=-1 \
			 channel_order={order} channel_flag_permanent=1 channel_flag_semi_permanent=0 \
			 channel_flag_default=0 channel_flag_password=0 channel_codec_latency_factor=1 \
			 channel_codec_is_unencrypted=1 channel_delete_delay=0 \
			 channel_flag_maxclients_unlimited=1 channel_flag_maxfamilyclients_unlimited=1 \
			 channel_flag_maxfamilyclients_inherited=0 channel_needed_talk_power=0 \
			 channel_forced_silence=0 channel_name_phonetic channel_icon_id=0 {banner} \
			 channel_storage_quota=4294967295"
		)
	}

	/// Channel banners as a TeamSpeak 6 server (6.0.0-beta13.1) sends them:
	/// in the channel list, when a channel is created and when it is edited.
	#[test]
	fn channel_banners() {
		let mut book = ts6_book();
		let list = format!(
			"channellist {}|{}",
			channel(1, 0, "Lobby", "channel_banner_gfx_url channel_banner_mode=0"),
			channel(
				12,
				1,
				"Raid",
				r"channel_banner_gfx_url=http:\/\/127.0.0.1:1\/d.png channel_banner_mode=2"
			),
		);
		for text in [
			list.as_str(),
			concat!(
				"notifychannelcreated cid=13 cpid=0 invokerid=1 invokername=serveradmin ",
				"invokeruid=serveradmin channel_name=New channel_order=12 ",
				r"channel_flag_permanent=1 channel_banner_gfx_url=https:\/\/example.com\/new.jpg ",
				"channel_banner_mode=1",
			),
			concat!(
				"notifychanneledited cid=12 reasonid=10 invokerid=1 invokername=serveradmin ",
				"invokeruid=serveradmin channel_banner_mode=1",
			),
		] {
			book.handle_command(&message(text)).unwrap();
		}
		let p = presence_from_book(&book, None, |_| false);
		let banner = |p: &Presence, cid| {
			let c = &p.channels[&cid];
			(c.banner_gfx_url.clone(), c.banner_mode)
		};
		assert_eq!(banner(&p, 1), (None, BannerMode::NoAdjust));
		assert_eq!(
			banner(&p, 12),
			(Some("http://127.0.0.1:1/d.png".into()), BannerMode::IgnoreAspect)
		);
		assert_eq!(
			banner(&p, 13),
			(Some("https://example.com/new.jpg".into()), BannerMode::IgnoreAspect)
		);
		// Removing the banner sends an empty address.
		book.handle_command(&message(
			"notifychanneledited cid=12 reasonid=10 invokerid=1 invokername=x channel_banner_gfx_url",
		))
		.unwrap();
		let p = presence_from_book(&book, None, |_| false);
		assert_eq!(p.channels[&12].banner_gfx_url, None);
		// The host banner, for comparison.
		assert_eq!(p.server.banner_gfx_url, "https://example.com/banner.png");
		assert_eq!(p.server.banner_gfx_interval_s, 60);
		assert_eq!(p.server.banner_mode, BannerMode::KeepAspect);
		assert_eq!(BannerMode::from_wire("7"), BannerMode::NoAdjust);
	}

	/// The myTeamSpeak avatar of a client as a TeamSpeak 6 server sends it:
	/// when the client enters and when it changes.
	#[test]
	fn myts_avatars() {
		let mut book = ts6_book();
		book.handle_command(&message(&format!("channellist {}", channel(1, 0, "Lobby", ""))))
			.unwrap();
		let enter = |clid: u16, avatar: &str| {
			format!(
				"notifycliententerview reasonid=0 ctid=1 clid={clid} client_database_id={clid} \
				 client_nickname=c{clid} client_type=0 cfid=0 client_unique_identifier=u{clid}= \
				 client_flag_avatar client_description client_icon_id=0 client_input_muted=0 \
				 client_output_muted=0 client_outputonly_muted=0 client_input_hardware=1 \
				 client_output_hardware=1 client_meta_data client_is_recording=0 \
				 client_channel_group_id=8 client_channel_group_inherited_channel_id=1 \
				 client_servergroups=8 client_away=0 client_away_message client_talk_power=75 \
				 client_talk_request=0 client_talk_request_msg client_is_talker=0 \
				 client_is_priority_speaker=0 client_unread_messages=0 client_nickname_phonetic \
				 client_needed_serverquery_view_power=75 client_is_channel_commander=0 \
				 client_country=RU client_badges client_myteamspeak_id=m{clid} \
				 client_integrations{avatar}"
			)
		};
		for text in [
			enter(
				5,
				r" client_myteamspeak_avatar=3,https:\/\/a.example\/away.png;2,https:\/\/a.example\/on.png",
			),
			// A TeamSpeak 3 server, or a client without one.
			enter(6, ""),
			enter(7, " client_myteamspeak_avatar"),
		] {
			book.handle_command(&message(&text)).unwrap();
		}
		let p = presence_from_book(&book, None, |_| false);
		let avatar = |p: &Presence, clid: u16| p.clients[&clid].myts_avatar.clone();
		assert_eq!(avatar(&p, 5).as_deref(), Some("https://a.example/on.png"));
		assert_eq!((avatar(&p, 6), avatar(&p, 7)), (None, None));
		book.handle_command(&message(
			r"notifyclientupdated clid=6 client_myteamspeak_avatar=4,https:\/\/a.example\/off.png",
		))
		.unwrap();
		let p = presence_from_book(&book, None, |_| false);
		assert_eq!(avatar(&p, 6).as_deref(), Some("https://a.example/off.png"));
	}
}
