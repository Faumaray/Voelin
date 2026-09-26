//! `voelinctl observe` and `voelinctl relay`: invisible presence and channel chat.

use std::time::Duration;

use anyhow::{Result, bail};
use clap::Args;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, timeout_at};
use voelin_model::{ChatMessage, ChatTarget, Presence, PresenceDelta};
use voelin_observer::{
	Observer, ObserverConfig, ObserverEvent, RelayConfig, RelayEvent, RelayPool,
};

use crate::query::QueryConnArgs;

#[derive(Args, Debug, Clone)]
pub struct ObserveArgs {
	#[command(flatten)]
	pub conn: QueryConnArgs,
	/// Stop after this many seconds (default: until Ctrl-C).
	#[arg(long)]
	pub seconds: Option<u64>,
	/// Print events as JSON lines.
	#[arg(long)]
	pub json: bool,
	/// Resync interval in seconds.
	#[arg(long, default_value_t = 10)]
	pub poll: u64,
	/// Exit successfully once a client with this nickname is seen (for tests).
	#[arg(long)]
	pub expect_client: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct RelayArgs {
	#[command(flatten)]
	pub conn: QueryConnArgs,
	/// Channel id to relay.
	#[arg(long)]
	pub channel: u64,
	/// Post this message after opening the relay.
	#[arg(long)]
	pub send: Option<String>,
	/// Name shown in relayed posts.
	#[arg(long, default_value = "voelinctl")]
	pub nick: String,
	/// Stop after this many seconds (default: until Ctrl-C).
	#[arg(long)]
	pub seconds: Option<u64>,
	/// Exit successfully once a message containing this text arrives.
	#[arg(long)]
	pub expect: Option<String>,
}

fn print_chat(msg: &ChatMessage) {
	let target = match &msg.target {
		ChatTarget::Server => "server".to_string(),
		ChatTarget::Channel(cid) => format!("channel {cid}"),
		ChatTarget::Private(_) => "private".to_string(),
	};
	println!("[{target}] {}: {}", msg.author_name, msg.text);
}

fn print_tree(p: &Presence) {
	let channels: Vec<_> = p
		.channels
		.values()
		.map(|c| crate::tree::ChannelNode {
			id: tsclientlib::ChannelId(c.id),
			parent: tsclientlib::ChannelId(c.parent),
			order: tsclientlib::ChannelId(c.order),
			name: c.name.clone(),
		})
		.collect();
	let clients: Vec<_> = p
		.clients
		.values()
		.map(|c| crate::tree::ClientNode {
			channel: tsclientlib::ChannelId(c.channel),
			name: c.nickname.clone(),
			talk_power: 0,
			is_query: c.is_query,
			input_muted: c.input_muted,
			output_muted: c.output_muted,
			away: c.away.is_some(),
			streaming: c.streaming == Some(true),
		})
		.collect();
	print!(
		"{}",
		crate::tree::render(
			&format!("{} (via query, invisible)", p.server_name),
			&channels,
			&clients
		)
	);
}

fn describe(p: &Presence, d: &PresenceDelta) -> String {
	let name = |id: &u16| {
		p.clients.get(id).map(|c| c.nickname.clone()).unwrap_or_else(|| format!("clid {id}"))
	};
	let channel = |id: &u64| {
		p.channels.get(id).map(|c| c.name.clone()).unwrap_or_else(|| format!("cid {id}"))
	};
	match d {
		PresenceDelta::ClientJoined(c) => format!("+ {} in #{}", c.nickname, channel(&c.channel)),
		PresenceDelta::ClientLeft { id } => format!("- {} left", name(id)),
		PresenceDelta::ClientMoved { id, channel: to } => {
			format!("> {} moved to #{}", name(id), channel(to))
		}
		PresenceDelta::ClientChanged(c) => format!(
			"~ {} changed (muted: {}, away: {})",
			c.nickname,
			c.input_muted,
			c.away.is_some()
		),
		PresenceDelta::ChannelAdded(c) => format!("+ channel #{}", c.name),
		PresenceDelta::ChannelChanged(c) => format!("~ channel #{}", c.name),
		PresenceDelta::ChannelRemoved { id } => format!("- channel {}", channel(id)),
		PresenceDelta::ServerRenamed { name } => format!("~ server renamed to {name}"),
	}
}

pub async fn observe(args: ObserveArgs) -> Result<()> {
	let mut config = ObserverConfig::new(args.conn.connect_config());
	config.poll_interval = Duration::from_secs(args.poll);
	let observer = Observer::spawn(config);
	let mut events = observer.subscribe();
	let deadline = args.seconds.map(|s| Instant::now() + Duration::from_secs(s));
	loop {
		let next = async {
			match deadline {
				Some(d) => timeout_at(d, events.recv()).await.ok(),
				None => Some(events.recv().await),
			}
		};
		let event = tokio::select! {
			e = next => e,
			_ = tokio::signal::ctrl_c() => return Ok(()),
		};
		let event = match event {
			None => break,
			Some(Ok(e)) => e,
			Some(Err(RecvError::Lagged(n))) => {
				eprintln!("(missed {n} events)");
				continue;
			}
			Some(Err(RecvError::Closed)) => bail!("observer stopped"),
		};
		let presence = observer.presence();
		let presence = presence.read().unwrap().clone();
		if args.json {
			match &event {
				ObserverEvent::Snapshot(s) => println!("{}", serde_json::to_string(s)?),
				ObserverEvent::Delta(d) => println!("{}", serde_json::to_string(d)?),
				ObserverEvent::Chat(m) => println!("{}", serde_json::to_string(m)?),
				ObserverEvent::Disconnected(r) => {
					println!("{}", serde_json::json!({"disconnected": r}))
				}
			}
		} else {
			match &event {
				ObserverEvent::Snapshot(_) => print_tree(&presence),
				ObserverEvent::Delta(d) => println!("{}", describe(&presence, d)),
				ObserverEvent::Chat(m) => print_chat(m),
				ObserverEvent::Disconnected(r) => println!("! disconnected: {r}"),
			}
		}
		if let Some(nick) = &args.expect_client
			&& presence.clients.values().any(|c| &c.nickname == nick)
		{
			return Ok(());
		}
	}
	if let Some(nick) = &args.expect_client {
		bail!("client {nick:?} not seen");
	}
	Ok(())
}

pub async fn relay(args: RelayArgs) -> Result<()> {
	let pool = RelayPool::new(RelayConfig::new(args.conn.connect_config()));
	let mut events = pool.subscribe();
	pool.open(args.channel).await?;
	println!("relaying channel {}", args.channel);
	if let Some(text) = &args.send {
		pool.send(args.channel, &args.nick, text).await?;
		println!("sent");
	}
	let result = relay_loop(&args, &mut events).await;
	pool.close_all().await;
	result
}

async fn relay_loop(
	args: &RelayArgs,
	events: &mut tokio::sync::broadcast::Receiver<RelayEvent>,
) -> Result<()> {
	let deadline = args.seconds.map(|s| Instant::now() + Duration::from_secs(s));
	loop {
		let next = async {
			match deadline {
				Some(d) => timeout_at(d, events.recv()).await.ok(),
				None => Some(events.recv().await),
			}
		};
		let event = tokio::select! {
			e = next => e,
			_ = tokio::signal::ctrl_c() => return Ok(()),
		};
		match event {
			None => break,
			Some(Ok(RelayEvent::Message(m))) => {
				print_chat(&m);
				if args.expect.as_ref().is_some_and(|e| m.text.contains(e.as_str())) {
					return Ok(());
				}
			}
			Some(Ok(RelayEvent::Closed { reason, .. })) => bail!("relay closed: {reason}"),
			Some(Err(RecvError::Lagged(_))) => {}
			Some(Err(RecvError::Closed)) => break,
		}
	}
	if let Some(expect) = &args.expect {
		bail!("no message containing {expect:?}");
	}
	Ok(())
}
