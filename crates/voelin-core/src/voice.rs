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
	ChannelId, ChannelInfo, ChatMessage, ChatTarget, ClientInfo, Presence, ServerFlavor,
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

/// The presence visible through a voice connection.
pub(crate) fn presence_from_book(book: &data::Connection) -> Presence {
	let limit = |m: &Option<MaxClients>| match m {
		Some(MaxClients::Limited(n)) => Some(*n as i32),
		_ => None,
	};
	Presence {
		server_name: book.server.name.clone(),
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
					},
				)
			})
			.collect(),
		clients: book
			.clients
			.values()
			.map(|c| {
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
						talking: None,
						streaming: c.is_streaming,
						server_groups: c.server_groups.iter().map(|g| g.0).collect(),
						country: Some(c.country_code.clone()).filter(|c| !c.is_empty()),
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
	{
		let state = con.get_state()?;
		let flavor = ServerFlavor::from_version_string(&state.server.version);
		info!(server = %state.server.name, ?flavor, "voice connected");
		let _ = events.send(VoiceEvent::Connected {
			name: state.server.name.clone(),
			flavor,
			own_client: state.own_client.0,
		});
	}
	// Subscribe to all channels to see everyone.
	let cmd = con.get_state()?.server.set_subscribed(true);
	cmd.send(&mut con)?;
	publish_state(&con, events)?;

	// Who is talking: last voice packet per client.
	let mut talking: std::collections::HashMap<u16, Instant> = Default::default();
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
							}));
						}
					}
					publish_state(&con, events)?;
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

fn own_channel(con: &Connection) -> Option<ChannelId> {
	let state = con.get_state().ok()?;
	state.clients.get(&state.own_client).map(|c| c.channel.0)
}

fn publish_state(
	con: &Connection,
	events: &mpsc::UnboundedSender<VoiceEvent>,
) -> anyhow::Result<()> {
	let state = con.get_state()?;
	let _ = events.send(VoiceEvent::Presence(presence_from_book(state)));
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
