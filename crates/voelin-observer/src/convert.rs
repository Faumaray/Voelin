//! Query rows and events to model types.

use voelin_model::{ChannelInfo, ChatMessage, ChatTarget, ClientInfo, PresenceDelta};
use voelin_query::{Notification, Row};

/// From a `channellist -topic -flags -voice -limits` row, or the fields of
/// `notifychannelcreated`.
pub fn channel_from_row(row: &Row) -> Option<ChannelInfo> {
	Some(ChannelInfo {
		id: row.parse("cid")?,
		parent: row.parse("pid").or_else(|| row.parse("cpid")).unwrap_or(0),
		order: row.parse("channel_order").unwrap_or(0),
		name: row.get("channel_name").unwrap_or_default().to_string(),
		topic: row.get("channel_topic").filter(|t| !t.is_empty()).map(str::to_string),
		has_password: row.flag("channel_flag_password").unwrap_or(false),
		max_clients: row.parse::<i32>("channel_maxclients").filter(|m| *m >= 0),
		needed_subscribe_power: row.parse("channel_needed_subscribe_power").unwrap_or(0),
		needed_talk_power: row.parse("channel_needed_talk_power").unwrap_or(0),
		is_default: row.flag("channel_flag_default").unwrap_or(false),
		icon: row.parse::<i64>("channel_icon_id").map_or(0, |i| i as u32),
	})
}

/// Update `channel` with the properties present in `row` (for
/// `notifychanneledited`, which only carries what changed).
pub fn update_channel(channel: &mut ChannelInfo, row: &Row) {
	if let Some(name) = row.get("channel_name") {
		channel.name = name.to_string();
	}
	if let Some(topic) = row.get("channel_topic") {
		channel.topic = Some(topic.to_string()).filter(|t| !t.is_empty());
	}
	if let Some(order) = row.parse("channel_order") {
		channel.order = order;
	}
	if let Some(p) = row.flag("channel_flag_password") {
		channel.has_password = p;
	}
	if let Some(m) = row.parse::<i32>("channel_maxclients") {
		channel.max_clients = Some(m).filter(|m| *m >= 0);
	}
	if let Some(p) = row.parse("channel_needed_subscribe_power") {
		channel.needed_subscribe_power = p;
	}
	if let Some(p) = row.parse("channel_needed_talk_power") {
		channel.needed_talk_power = p;
	}
	if let Some(d) = row.flag("channel_flag_default") {
		channel.is_default = d;
	}
}

/// From a `clientlist -uid -away -voice -groups -country` row or a
/// `notifycliententerview` row (which names the channel `ctid`).
pub fn client_from_row(row: &Row) -> Option<ClientInfo> {
	let away = row.flag("client_away").unwrap_or(false);
	Some(ClientInfo {
		id: row.parse("clid")?,
		uid: row.get("client_unique_identifier").map(str::to_string),
		nickname: row.get("client_nickname").unwrap_or_default().to_string(),
		channel: row.parse("cid").or_else(|| row.parse("ctid"))?,
		is_query: row.parse::<u8>("client_type") == Some(1),
		away: away.then(|| row.get("client_away_message").unwrap_or_default().to_string()),
		input_muted: row.flag("client_input_muted").unwrap_or(false),
		output_muted: row.flag("client_output_muted").unwrap_or(false),
		talking: row.flag("client_flag_talking"),
		streaming: row.flag("client_is_streaming"),
		server_groups: row
			.get("client_servergroups")
			.map(|g| g.split(',').filter_map(|g| g.parse().ok()).collect())
			.unwrap_or_default(),
		country: row.get("client_country").filter(|c| !c.is_empty()).map(str::to_string),
		// What the row carries (`notifycliententerview` all of it,
		// `clientlist -voice -groups -icon` some).
		avatar: row.get("client_flag_avatar").filter(|a| !a.is_empty()).map(str::to_string),
		description: row.get("client_description").filter(|d| !d.is_empty()).map(str::to_string),
		talk_power: row.parse("client_talk_power").unwrap_or(0),
		talker: row.flag("client_is_talker").unwrap_or(false),
		channel_group: row.parse("client_channel_group_id"),
		badges: row.get("client_badges").map(voelin_model::parse_badges).unwrap_or_default(),
		icon: row.parse::<i64>("client_icon_id").map_or(0, |i| i as u32),
		recording: row.flag("client_is_recording").unwrap_or(false),
		priority_speaker: row.flag("client_is_priority_speaker").unwrap_or(false),
		channel_commander: row.flag("client_is_channel_commander").unwrap_or(false),
		database_id: row.parse("client_database_id"),
	})
}

/// Presence changes carried by a query event. `notifychanneledited` needs the
/// current channel state and is handled by the observer.
pub fn delta_from_notification(n: &Notification) -> Vec<PresenceDelta> {
	let rows = n.rows.iter();
	match n.name.as_str() {
		"notifycliententerview" => {
			rows.filter_map(client_from_row).map(PresenceDelta::ClientJoined).collect()
		}
		"notifyclientleftview" => rows
			.filter_map(|r| r.parse("clid"))
			.map(|id| PresenceDelta::ClientLeft { id })
			.collect(),
		"notifyclientmoved" => {
			// Several clients can move at once; ctid is only in the first row.
			let ctid = n.rows.first().and_then(|r| r.parse("ctid"));
			rows.filter_map(|r| {
				Some(PresenceDelta::ClientMoved {
					id: r.parse("clid")?,
					channel: r.parse("ctid").or(ctid)?,
				})
			})
			.collect()
		}
		"notifychannelcreated" => {
			rows.filter_map(channel_from_row).map(PresenceDelta::ChannelAdded).collect()
		}
		"notifychanneldeleted" => rows
			.filter_map(|r| r.parse("cid"))
			.map(|id| PresenceDelta::ChannelRemoved { id })
			.collect(),
		"notifyserveredited" => rows
			.filter_map(|r| r.get("virtualserver_name"))
			.map(|name| PresenceDelta::ServerRenamed { name: name.to_string() })
			.collect(),
		_ => Vec::new(),
	}
}

/// A `notifytextmessage` as a chat message.
pub fn is_text_message(n: &Notification, now_ms: i64) -> Option<ChatMessage> {
	if n.name != "notifytextmessage" {
		return None;
	}
	let row = n.rows.first()?;
	let target = match row.parse::<u8>("targetmode")? {
		3 => ChatTarget::Server,
		2 => ChatTarget::Channel(0), // filled in by the receiver, who knows its channel
		1 => ChatTarget::Private(row.get("invokeruid").unwrap_or_default().to_string()),
		_ => return None,
	};
	Some(ChatMessage {
		target,
		author_name: row.get("invokername").unwrap_or_default().to_string(),
		author_uid: row.get("invokeruid").map(str::to_string),
		author_id: row.parse("invokerid"),
		text: row.get("msg").unwrap_or_default().to_string(),
		ts_ms: now_ms,
		via_relay: true,
		blocked: false,
	})
}

#[cfg(test)]
mod tests {
	use voelin_query::{Line, parse_line};

	use super::*;

	fn notify(line: &str) -> Notification {
		match parse_line(line) {
			Line::Notify(n) => n,
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn clientlist_row() {
		// Captured from TeamSpeak 6.0.0-beta13.1 (clientlist -uid -away -voice -groups -country).
		let row = &voelin_query::parse_rows(
			"clid=7 cid=1 client_database_id=4 client_nickname=sample client_type=0 client_away=0 \
			 client_away_message client_flag_talking=0 client_input_muted=1 client_output_muted=0 \
			 client_unique_identifier=hSC9DusMo3MJwDuA8MuvZBaHfb\\/h77N9WZtYskAFlG0= \
			 client_servergroups=8,9 client_country",
		)[0];
		let c = client_from_row(row).unwrap();
		assert_eq!((c.id, c.channel, c.nickname.as_str()), (7, 1, "sample"));
		assert!(!c.is_query && c.input_muted && c.away.is_none());
		assert_eq!(c.talking, Some(false));
		assert_eq!(c.server_groups, vec![8, 9]);
		assert_eq!(c.uid.as_deref(), Some("hSC9DusMo3MJwDuA8MuvZBaHfb/h77N9WZtYskAFlG0="));
		assert_eq!(c.country, None);
	}

	#[test]
	fn channellist_row() {
		let row = &voelin_query::parse_rows(
			"cid=1 pid=0 channel_order=0 channel_name=Default\\sChannel channel_topic=t \
			 channel_flag_default=1 channel_flag_password=0 channel_maxclients=-1 \
			 channel_needed_subscribe_power=0 channel_needed_talk_power=5",
		)[0];
		let c = channel_from_row(row).unwrap();
		assert_eq!(c.name, "Default Channel");
		assert!(c.is_default && !c.has_password);
		assert_eq!(c.max_clients, None);
		assert_eq!(c.needed_talk_power, 5);
	}

	#[test]
	fn events_to_deltas() {
		let n = notify(
			"notifycliententerview cfid=0 ctid=1 reasonid=0 clid=6 \
			 client_unique_identifier=abc client_nickname=evtest client_type=0",
		);
		match &delta_from_notification(&n)[..] {
			[PresenceDelta::ClientJoined(c)] => assert_eq!((c.id, c.channel), (6, 1)),
			other => panic!("{other:?}"),
		}
		let n = notify("notifyclientmoved ctid=5 reasonid=0 clid=6|clid=7");
		assert_eq!(
			delta_from_notification(&n),
			vec![
				PresenceDelta::ClientMoved { id: 6, channel: 5 },
				PresenceDelta::ClientMoved { id: 7, channel: 5 },
			]
		);
		let n = notify("notifyclientleftview cfid=1 ctid=0 reasonid=8 clid=6");
		assert_eq!(delta_from_notification(&n), vec![PresenceDelta::ClientLeft { id: 6 }]);
		let n = notify("notifychanneldeleted cid=3 invokerid=0");
		assert_eq!(delta_from_notification(&n), vec![PresenceDelta::ChannelRemoved { id: 3 }]);
	}

	#[test]
	fn text_messages() {
		let n = notify(
			"notifytextmessage targetmode=3 msg=hi\\sall invokerid=5 invokername=Bob invokeruid=u",
		);
		let m = is_text_message(&n, 42).unwrap();
		assert_eq!(m.target, ChatTarget::Server);
		assert_eq!(
			(m.text.as_str(), m.author_name.as_str(), m.author_id),
			("hi all", "Bob", Some(5))
		);
		assert!(is_text_message(&notify("notifyclientleftview clid=1"), 0).is_none());
	}
}
