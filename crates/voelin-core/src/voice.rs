//! The voice source: a normal client connection.

use std::collections::HashMap;
use std::time::Duration;

use futures::prelude::*;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};
use tracing::{info, warn};
use tsclientlib::data;
use tsclientlib::events::Event as BookEvent;
use tsclientlib::prelude::*;
use tsclientlib::{
	ClientType, Connection, DisconnectOptions, Identity, MaxClients, MessageHandle, MessageTarget,
	StreamItem, Version,
};
use tsproto_packets::packets::{AudioData, InAudioBuf, OutPacket};
use voelin_model::{
	ChannelId, ChannelInfo, ChatMessage, ChatTarget, ClientInfo, GroupInfo, GroupNamingMode,
	GroupType, HostBannerMode, HostMessageMode, Presence, ServerDetails, ServerFlavor,
	parse_badges,
};
use voelin_stream::{PeerConfig, Request, StreamNotification};

#[derive(Clone, Debug)]
pub struct VoiceOptions {
	pub address: String,
	pub nickname: String,
	pub identity: Option<Identity>,
	/// Signed client version; `None` for the library default.
	pub client_version: Option<Version>,
	pub server_password: Option<String>,
	/// Channel path to join, e.g. `Lobby/Sub`.
	pub channel: Option<String>,
	/// Open the audio devices (capture and playback).
	pub audio: bool,
	/// Network settings for stream peer connections (TeamSpeak 6).
	pub stream_peer: PeerConfig,
}

impl VoiceOptions {
	pub fn new(address: impl Into<String>, nickname: impl Into<String>) -> Self {
		Self {
			address: address.into(),
			nickname: nickname.into(),
			identity: None,
			client_version: None,
			server_password: None,
			channel: None,
			audio: false,
			stream_peer: PeerConfig::default(),
		}
	}
}

pub(crate) enum VoiceCmd {
	SendChat(ChatTarget, String),
	Move(ChannelId, Option<String>),
	SetInputMuted(bool),
	SetOutputMuted(bool),
	/// An encoded voice packet from the audio thread.
	Audio(OutPacket),
	/// A stream command; failures come back as [`VoiceEvent::StreamRequestFailed`].
	Stream(Request),
	Disconnect,
}

pub(crate) enum VoiceEvent {
	Connected {
		name: String,
		flavor: ServerFlavor,
		own_client: u16,
		/// The server's unique id (from its public key, as the server
		/// generation computes it).
		server_uid: String,
		/// Our unique id as the server knows it.
		own_uid: Option<String>,
	},
	Presence(Presence),
	OwnChannel(ChannelId),
	Chat(ChatMessage),
	/// Incoming voice for the audio thread, and who is talking.
	Audio(InAudioBuf),
	Talking {
		client: u16,
		talking: bool,
	},
	Stream(StreamNotification),
	StreamRequestFailed(Request, String),
	Disconnected(Option<String>),
}

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
/// [`VoiceEvent::Connected`] told it, `talking` the clients talking now.
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

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or_default()
}

enum Input {
	Item(Option<Result<StreamItem, tsclientlib::Error>>),
	Cmd(Option<VoiceCmd>),
	Tick,
}

/// Run a voice connection until it ends or is told to disconnect.
pub(crate) async fn run(
	options: VoiceOptions,
	mut commands: mpsc::UnboundedReceiver<VoiceCmd>,
	events: mpsc::UnboundedSender<VoiceEvent>,
) {
	let reason = match run_inner(&options, &mut commands, &events).await {
		Ok(()) => None,
		Err(e) => Some(e.to_string()),
	};
	let _ = events.send(VoiceEvent::Disconnected(reason));
}

async fn run_inner(
	options: &VoiceOptions,
	commands: &mut mpsc::UnboundedReceiver<VoiceCmd>,
	events: &mpsc::UnboundedSender<VoiceEvent>,
) -> anyhow::Result<()> {
	let mut builder = Connection::build(options.address.clone()).name(options.nickname.clone());
	if let Some(identity) = &options.identity {
		builder = builder.identity(identity.clone());
	}
	if let Some(version) = &options.client_version {
		builder = builder.version(version.clone());
	}
	if let Some(pw) = &options.server_password {
		builder = builder.password(pw.clone());
	}
	if let Some(channel) = &options.channel {
		builder = builder.channel(channel.clone());
	}
	let mut con = builder.connect()?;

	// Wait for the initial state.
	let connected = timeout(Duration::from_secs(30), async {
		con.events()
			.try_filter(|e| future::ready(matches!(e, StreamItem::BookEvents(_))))
			.next()
			.await
	})
	.await
	.map_err(|_| anyhow::anyhow!("timed out while connecting"))?;
	match connected {
		Some(Ok(_)) => {}
		Some(Err(e)) => return Err(e.into()),
		None => anyhow::bail!("connection closed"),
	}
	let server_uid = {
		let state = con.get_state()?;
		let flavor = ServerFlavor::from_version_string(&state.server.version);
		let ids = voelin_gateway_proto::UniqueIds::from_omega(&state.server.public_key.to_ts());
		let server_uid = ids.for_server(matches!(flavor, ServerFlavor::Ts6(_))).to_owned();
		info!(server = %state.server.name, ?flavor, %server_uid, "voice connected");
		let own_uid = state
			.clients
			.get(&state.own_client)
			.and_then(|c| c.uid.as_ref())
			.map(|u| u.as_ref().to_string());
		let _ = events.send(VoiceEvent::Connected {
			name: state.server.name.clone(),
			flavor,
			own_client: state.own_client.0,
			server_uid: server_uid.clone(),
			own_uid,
		});
		server_uid
	};
	// Subscribe to all channels to see everyone.
	let cmd = con.get_state()?.server.set_subscribed(true);
	cmd.send(&mut con)?;
	// Who is talking: last voice packet per client.
	let mut talking: HashMap<u16, Instant> = HashMap::new();
	publish_state(&con, &server_uid, &talking, events)?;

	let mut tick = tokio::time::interval(Duration::from_millis(250));
	// Stream commands waiting for the server's answer.
	let mut stream_requests: HashMap<MessageHandle, Request> = HashMap::new();

	loop {
		let input = {
			let mut stream = con.events();
			tokio::select! {
				item = stream.next() => Input::Item(item),
				cmd = commands.recv() => Input::Cmd(cmd),
				_ = tick.tick() => Input::Tick,
			}
		};
		match input {
			Input::Item(None) => anyhow::bail!("connection closed"),
			Input::Item(Some(Err(e))) => return Err(e.into()),
			Input::Item(Some(Ok(item))) => match item {
				StreamItem::BookEvents(book_events) => {
					for e in &book_events {
						if let BookEvent::Message { target, invoker, message } = e {
							let own_channel = own_channel(&con);
							let target = match target {
								MessageTarget::Server => ChatTarget::Server,
								MessageTarget::Channel => {
									ChatTarget::Channel(own_channel.unwrap_or(0))
								}
								// Our own private message comes back to us: it
								// belongs to the chat with its receiver.
								MessageTarget::Client(to)
									if Some(invoker.id) == own_client(&con) =>
								{
									ChatTarget::Private(client_uid(&con, to).unwrap_or_default())
								}
								MessageTarget::Client(_) | MessageTarget::Poke(_) => {
									ChatTarget::Private(
										invoker
											.uid
											.as_ref()
											.map(|u| u.as_ref().to_string())
											.unwrap_or_default(),
									)
								}
							};
							let _ = events.send(VoiceEvent::Chat(ChatMessage {
								target,
								author_name: invoker.name.clone(),
								author_uid: invoker.uid.as_ref().map(|u| u.as_ref().to_string()),
								author_id: Some(invoker.id.0),
								text: message.clone(),
								ts_ms: now_ms(),
								via_relay: false,
								blocked: false,
							}));
						}
					}
					publish_state(&con, &server_uid, &talking, events)?;
				}
				StreamItem::Audio(packet) => {
					let from = match packet.data().data() {
						AudioData::S2C { from, .. } | AudioData::S2CWhisper { from, .. } => *from,
						_ => continue,
					};
					if talking.insert(from, Instant::now()).is_none() {
						let _ = events.send(VoiceEvent::Talking { client: from, talking: true });
					}
					let _ = events.send(VoiceEvent::Audio(packet));
				}
				StreamItem::MessageEvent(msg) => {
					for n in StreamNotification::from_message(&msg) {
						let _ = events.send(VoiceEvent::Stream(n));
					}
				}
				StreamItem::MessageResult(handle, result) => {
					if let (Some(request), Err(e)) = (stream_requests.remove(&handle), result) {
						let _ =
							events.send(VoiceEvent::StreamRequestFailed(request, e.to_string()));
					}
				}
				StreamItem::DisconnectedTemporarily(reason) => {
					warn!(?reason, "voice connection interrupted, reconnecting");
				}
				_ => {}
			},
			Input::Cmd(None) | Input::Cmd(Some(VoiceCmd::Disconnect)) => break,
			Input::Cmd(Some(VoiceCmd::Stream(request))) => {
				let handle = request.to_command().send_with_result(&mut con)?;
				stream_requests.insert(handle, request);
			}
			Input::Cmd(Some(cmd)) => handle_command(&mut con, cmd)?,
			Input::Tick => {
				talking.retain(|client, last| {
					let active = last.elapsed() < Duration::from_millis(400);
					if !active {
						let _ =
							events.send(VoiceEvent::Talking { client: *client, talking: false });
					}
					active
				});
			}
		}
	}

	con.disconnect(DisconnectOptions::new())?;
	let _ = timeout(Duration::from_secs(3), con.events().for_each(|_| future::ready(()))).await;
	Ok(())
}

fn own_client(con: &Connection) -> Option<tsclientlib::ClientId> {
	con.get_state().ok().map(|state| state.own_client)
}

fn client_uid(con: &Connection, client: &tsclientlib::ClientId) -> Option<String> {
	let state = con.get_state().ok()?;
	state.clients.get(client)?.uid.as_ref().map(|u| u.as_ref().to_string())
}

fn own_channel(con: &Connection) -> Option<ChannelId> {
	let state = con.get_state().ok()?;
	state.clients.get(&state.own_client).map(|c| c.channel.0)
}

fn publish_state(
	con: &Connection,
	server_uid: &str,
	talking: &HashMap<u16, Instant>,
	events: &mpsc::UnboundedSender<VoiceEvent>,
) -> anyhow::Result<()> {
	let state = con.get_state()?;
	let presence = presence_from_book(state, Some(server_uid), |id| talking.contains_key(&id));
	let _ = events.send(VoiceEvent::Presence(presence));
	if let Some(own) = state.clients.get(&state.own_client) {
		let _ = events.send(VoiceEvent::OwnChannel(own.channel.0));
	}
	Ok(())
}

fn handle_command(con: &mut Connection, cmd: VoiceCmd) -> anyhow::Result<()> {
	match cmd {
		VoiceCmd::SendChat(target, text) => {
			let state = con.get_state()?;
			let out = match target {
				ChatTarget::Server => state.server.send_textmessage(&text),
				ChatTarget::Channel(_) => state.send_message(MessageTarget::Channel, &text),
				ChatTarget::Private(uid) => {
					let client = state
						.clients
						.values()
						.find(|c| c.uid.as_ref().is_some_and(|u| u.as_ref().to_string() == uid))
						.ok_or_else(|| anyhow::anyhow!("client is not on the server"))?;
					state.send_message(MessageTarget::Client(client.id), &text)
				}
			};
			out.send(con)?;
		}
		VoiceCmd::Move(channel, password) => {
			let state = con.get_state()?;
			let own = &state.clients[&state.own_client];
			let mut part = own.client_move(tsclientlib::ChannelId(channel));
			if let Some(pw) = &password {
				part = part.set_password(pw);
			}
			part.send(con)?;
		}
		VoiceCmd::SetInputMuted(muted) => {
			con.get_state()?.client_update().set_input_muted(muted).send(con)?;
		}
		VoiceCmd::SetOutputMuted(muted) => {
			con.get_state()?.client_update().set_output_muted(muted).send(con)?;
		}
		VoiceCmd::Audio(packet) => {
			con.send_audio(packet)?;
		}
		VoiceCmd::Stream(request) => {
			request.to_command().send(con)?;
		}
		VoiceCmd::Disconnect => {}
	}
	Ok(())
}
