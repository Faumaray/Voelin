//! Connected actions: tree, chat, listen, repl.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::prelude::*;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::{Instant, timeout_at};
use tracing::{info, warn};
use tsclientlib::events::{Event, PropertyId, PropertyValue};
use tsclientlib::messages::c2s::{OutTokenUseMessage, OutTokenUsePart};
use tsclientlib::prelude::*;
use tsclientlib::{
	ChannelId, ClientId, Connection, DisconnectOptions, MessageHandle, MessageTarget, StreamItem,
	data,
};
use tsproto_packets::packets::{Direction, Flags, OutCommand, PacketType};

use crate::{Action, ChatTarget, ConnectArgs, identity, tree, versions};

pub async fn run(args: ConnectArgs) -> Result<()> {
	let mut options = Connection::build(args.address.clone())
		.name(args.nick.clone())
		.log_commands(args.log_commands);
	if let Some(version) = versions::resolve(&args.client_version)? {
		options = options.version(version);
	}
	if let Some(path) = &args.identity {
		options = options.identity(identity::load(path)?);
	}
	if let Some(password) = &args.server_password {
		options = options.password(password.clone());
	}
	if let Some(channel) = &args.channel {
		options = options.channel(channel.clone());
	}

	let mut con = options.connect().context("failed to start connection")?;
	wait_connected(&mut con, Instant::now() + args.connect_timeout()).await?;
	info!(address = %args.address, "connected");

	if let Some(token) = &args.privilege_key {
		let cmd = OutTokenUseMessage::new(&mut std::iter::once(OutTokenUsePart {
			token: token.as_str().into(),
		}));
		send_and_wait(&mut con, cmd, "tokenuse").await?;
		info!("privilege key redeemed");
	}

	let result = match &args.action {
		Action::Tree { settle_ms } => print_tree(&mut con, Duration::from_millis(*settle_ms)).await,
		Action::Chat { target } => send_chat(&mut con, target).await,
		Action::Listen { json, expect, timeout } => {
			listen(&mut con, *json, expect.as_deref(), timeout.map(Duration::from_secs)).await
		}
		Action::Raw { command } => {
			let cmd = OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, command);
			send_and_wait(&mut con, cmd, "command").await.map(|()| println!("ok"))
		}
		Action::Repl => repl(&mut con).await,
	};

	disconnect(con).await;
	result
}

/// What to do after handling one stream item.
enum Flow {
	Continue,
	Stop,
}

/// How [`pump`] ended.
#[derive(Debug, PartialEq, Eq)]
enum PumpEnd {
	Stopped,
	Deadline,
	Interrupted,
}

/// Drive the connection, handing every item to `handler` until it returns
/// [`Flow::Stop`], the deadline passes, or Ctrl-C is pressed.
async fn pump(
	con: &mut Connection,
	deadline: Option<Instant>,
	mut handler: impl FnMut(&mut Connection, StreamItem) -> Result<Flow>,
) -> Result<PumpEnd> {
	loop {
		let item = {
			let mut events = con.events();
			let next = async {
				match deadline {
					Some(deadline) => timeout_at(deadline, events.next()).await.ok(),
					None => Some(events.next().await),
				}
			};
			tokio::select! {
				item = next => item,
				_ = tokio::signal::ctrl_c() => return Ok(PumpEnd::Interrupted),
			}
		};
		match item {
			None => return Ok(PumpEnd::Deadline),
			Some(None) => bail!("disconnected from server"),
			Some(Some(item)) => {
				if let Flow::Stop = handler(con, item?)? {
					return Ok(PumpEnd::Stopped);
				}
			}
		}
	}
}

async fn wait_connected(con: &mut Connection, deadline: Instant) -> Result<()> {
	let end = pump(con, Some(deadline), |_, item| {
		Ok(match item {
			StreamItem::BookEvents(_) => Flow::Stop,
			StreamItem::IdentityLevelIncreasing(level) => {
				eprintln!("server requires identity level {level}, improving identity...");
				Flow::Continue
			}
			_ => Flow::Continue,
		})
	})
	.await?;
	match end {
		PumpEnd::Stopped => Ok(()),
		PumpEnd::Deadline => bail!("timed out while connecting"),
		PumpEnd::Interrupted => bail!("interrupted"),
	}
}

async fn disconnect(mut con: Connection) {
	if let Err(error) = con.disconnect(DisconnectOptions::new()) {
		warn!(%error, "failed to disconnect cleanly");
		return;
	}
	let drain = con.events().for_each(|_| future::ready(()));
	if tokio::time::timeout(Duration::from_secs(5), drain).await.is_err() {
		warn!("timed out waiting for disconnect");
	}
}

/// Send a command and wait until the server acknowledges it.
async fn send_and_wait(con: &mut Connection, cmd: OutCommand, what: &str) -> Result<()> {
	let handle = cmd.send_with_result(con)?;
	let mut result = None;
	let end = pump(con, Some(Instant::now() + Duration::from_secs(10)), |_, item| {
		Ok(match item {
			StreamItem::MessageResult(h, r) if h == handle => {
				result = Some(r);
				Flow::Stop
			}
			_ => Flow::Continue,
		})
	})
	.await?;
	match (end, result) {
		(_, Some(Ok(()))) => Ok(()),
		(_, Some(Err(error))) => bail!("{what} failed: {error}"),
		(PumpEnd::Interrupted, None) => bail!("interrupted"),
		_ => bail!("{what}: no answer from server"),
	}
}

async fn print_tree(con: &mut Connection, settle: Duration) -> Result<()> {
	let cmd = con.get_state()?.server.set_subscribed(true);
	send_and_wait(con, cmd, "channelsubscribeall").await?;
	// Clients of newly subscribed channels arrive right after the answer.
	pump(con, Some(Instant::now() + settle), |_, _| Ok(Flow::Continue)).await?;
	print!("{}", tree::from_state(con.get_state()?));
	Ok(())
}

fn chat_command(state: &data::Connection, target: &ChatTarget) -> OutCommand {
	match target {
		ChatTarget::Server { message } => state.server.send_textmessage(message),
		ChatTarget::Channel { message } => state.send_message(MessageTarget::Channel, message),
		ChatTarget::Client { clid, message } => {
			state.send_message(MessageTarget::Client(ClientId(*clid)), message)
		}
	}
}

async fn send_chat(con: &mut Connection, target: &ChatTarget) -> Result<()> {
	let cmd = chat_command(con.get_state()?, target);
	send_and_wait(con, cmd, "sendtextmessage").await
}

fn target_label(target: &MessageTarget) -> &'static str {
	match target {
		MessageTarget::Server => "server",
		MessageTarget::Channel => "channel",
		MessageTarget::Client(_) => "private",
		MessageTarget::Poke(_) => "poke",
	}
}

fn channel_name(state: &data::Connection, id: ChannelId) -> String {
	state.channels.get(&id).map(|c| c.name.clone()).unwrap_or_else(|| format!("cid {}", id.0))
}

/// One line describing an event, or `None` for events not worth printing.
fn describe(state: &data::Connection, event: &Event) -> Option<String> {
	match event {
		Event::Message { target, invoker, message } => Some(format!(
			"[{}] {} (clid {}): {message}",
			target_label(target),
			invoker.name,
			invoker.id.0
		)),
		Event::PropertyAdded { id: PropertyId::Client(clid), extra, .. } => {
			let client = state.clients.get(clid)?;
			let reason = extra.reason.map(|r| format!(" ({r:?})")).unwrap_or_default();
			Some(format!(
				"+ {} (clid {}) in #{}{reason}",
				client.name,
				clid.0,
				channel_name(state, client.channel)
			))
		}
		Event::PropertyRemoved {
			id: PropertyId::Client(clid),
			old: PropertyValue::Client(client),
			..
		} => Some(format!("- {} (clid {}) left", client.name, clid.0)),
		Event::PropertyChanged {
			id: PropertyId::ClientChannel(clid),
			old: PropertyValue::ChannelId(from),
			..
		} => {
			let client = state.clients.get(clid)?;
			Some(format!(
				"> {} (clid {}) moved #{} -> #{}",
				client.name,
				clid.0,
				channel_name(state, *from),
				channel_name(state, client.channel)
			))
		}
		_ => None,
	}
}

/// Print book events; returns `true` if a text message contained `expect`.
fn print_events(
	state: &data::Connection,
	events: &[Event],
	json: bool,
	expect: Option<&str>,
) -> Result<bool> {
	let mut matched = false;
	for event in events {
		if let (Event::Message { message, .. }, Some(expect)) = (event, expect) {
			matched |= message.contains(expect);
		}
		if json {
			println!("{}", serde_json::to_string(event)?);
		} else if let Some(line) = describe(state, event) {
			println!("{line}");
		}
	}
	Ok(matched)
}

async fn listen(
	con: &mut Connection,
	json: bool,
	expect: Option<&str>,
	timeout: Option<Duration>,
) -> Result<()> {
	let deadline = timeout.map(|t| Instant::now() + t);
	let end = pump(con, deadline, |con, item| {
		if let StreamItem::BookEvents(events) = item
			&& print_events(con.get_state()?, &events, json, expect)?
		{
			return Ok(Flow::Stop);
		}
		Ok(Flow::Continue)
	})
	.await?;
	match (end, expect) {
		(PumpEnd::Deadline, Some(expect)) => bail!("timed out waiting for message {expect:?}"),
		_ => Ok(()),
	}
}

const REPL_HELP: &str = "\
commands:
  <text>                send to the current channel
  /server <text>        send to the server chat
  /channel <text>       send to the current channel
  /msg <clid> <text>    send a private message
  /raw <command>        send a raw, already escaped command
  /tree                 print the channel tree
  /whoami               print own client and channel
  /quit                 disconnect";

enum ReplInput {
	Item(Option<Result<StreamItem, tsclientlib::Error>>),
	Line(Option<String>),
	Interrupted,
}

async fn repl(con: &mut Connection) -> Result<()> {
	let cmd = con.get_state()?.server.set_subscribed(true);
	cmd.send(con)?;
	println!("connected, type /help for commands");
	let mut lines = BufReader::new(tokio::io::stdin()).lines();
	let mut pending: Option<MessageHandle> = None;
	loop {
		let input = {
			let mut events = con.events();
			tokio::select! {
				item = events.next() => ReplInput::Item(item),
				line = lines.next_line() => ReplInput::Line(line?),
				_ = tokio::signal::ctrl_c() => ReplInput::Interrupted,
			}
		};
		match input {
			ReplInput::Interrupted | ReplInput::Line(None) => return Ok(()),
			ReplInput::Item(None) => bail!("disconnected from server"),
			ReplInput::Item(Some(item)) => match item? {
				StreamItem::BookEvents(events) => {
					print_events(con.get_state()?, &events, false, None)?;
				}
				StreamItem::MessageResult(handle, Err(error)) => {
					println!("error (command {}): {error}", handle.0);
				}
				StreamItem::MessageResult(handle, Ok(())) if Some(handle) == pending => {
					println!("ok (command {})", handle.0);
					pending = None;
				}
				_ => {}
			},
			ReplInput::Line(Some(line)) => {
				let line = line.trim();
				if line.is_empty() {
					continue;
				}
				let (command, rest) = line.split_once(' ').unwrap_or((line, ""));
				let target = match command {
					"/help" => {
						println!("{REPL_HELP}");
						continue;
					}
					"/quit" | "/exit" => return Ok(()),
					"/tree" => {
						print!("{}", tree::from_state(con.get_state()?));
						continue;
					}
					"/whoami" => {
						let state = con.get_state()?;
						let own = &state.clients[&state.own_client];
						println!(
							"{} (clid {}) in #{}",
							own.name,
							own.id.0,
							channel_name(state, own.channel)
						);
						continue;
					}
					"/raw" => {
						let cmd = OutCommand::new(
							Direction::C2S,
							Flags::empty(),
							PacketType::Command,
							rest,
						);
						pending = Some(cmd.send_with_result(con)?);
						continue;
					}
					"/server" => ChatTarget::Server { message: rest.into() },
					"/channel" => ChatTarget::Channel { message: rest.into() },
					"/msg" => {
						let Some((clid, message)) = rest.split_once(' ') else {
							println!("usage: /msg <clid> <text>");
							continue;
						};
						let Ok(clid) = clid.parse() else {
							println!("invalid client id {clid:?}");
							continue;
						};
						ChatTarget::Client { clid, message: message.into() }
					}
					c if c.starts_with('/') => {
						println!("unknown command {c}, try /help");
						continue;
					}
					_ => ChatTarget::Channel { message: line.into() },
				};
				let cmd = chat_command(con.get_state()?, &target);
				pending = Some(cmd.send_with_result(con)?);
			}
		}
	}
}
