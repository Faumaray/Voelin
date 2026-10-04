//! `voelinctl probe-stream`: how can a TeamSpeak 6 client that arrives after a
//! stream started learn about it?
//!
//! Client A creates a channel and starts a stream there (signalling only, no
//! media). Client B connects afterwards and tries candidate commands, first
//! from another channel, then from A's channel; A then tries to re-announce
//! the stream. Every command both clients send and receive is printed raw,
//! with the time since the probe started. Findings are kept in
//! `docs/research/ts6-late-join.md`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use futures::prelude::*;
use tokio::time::{Instant, sleep_until};
use tsclientlib::prelude::*;
use tsclientlib::{ChannelId, ClientId, Connection, DisconnectOptions, StreamItem};
use tsproto::connection::Event as RawEvent;
use tsproto_packets::packets::{Direction, Flags, OutCommand, PacketType};
use voelin_stream::{StreamNotification, StreamSetup, proto};

use crate::{identity, session};

#[derive(Args, Debug)]
pub struct ProbeStreamArgs {
	/// A TeamSpeak 6 server.
	#[arg(default_value = "127.0.0.1:9988")]
	address: String,
	/// Identity of the streamer A. A creates a channel, so on servers where
	/// guests may not, pass an admin identity.
	#[arg(long)]
	streamer_identity: Option<PathBuf>,
	/// Identity of the late client B.
	#[arg(long)]
	viewer_identity: Option<PathBuf>,
	/// How long to collect notifications after each answer.
	#[arg(long, default_value_t = 1200)]
	settle_ms: u64,
	/// More raw commands for B to try in A's channel. `{id}` is the stream
	/// id, `{a}`/`{b}` the clients' ids, `{cid}` A's channel.
	#[arg(long = "try")]
	extra: Vec<String>,
	/// Also print the noisy notifications (connection info, permissions).
	#[arg(long)]
	verbose: bool,
}

/// Which client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Who {
	A,
	B,
}

impl Who {
	fn label(self) -> &'static str {
		match self {
			Who::A => "A",
			Who::B => "B",
		}
	}
}

/// A raw command as sent or received.
struct Line {
	at: Instant,
	who: Who,
	outgoing: bool,
	text: String,
}

type Lines = Arc<Mutex<Vec<Line>>>;

/// Values the command templates refer to.
#[derive(Clone, Debug, Default)]
struct Vars {
	id: String,
	a: u16,
	b: u16,
	cid: u64,
}

impl Vars {
	fn fill(&self, template: &str) -> String {
		template
			.replace("{id}", &self.id)
			.replace("{a}", &self.a.to_string())
			.replace("{b}", &self.b.to_string())
			.replace("{cid}", &self.cid.to_string())
	}
}

struct Probe {
	args: ProbeStreamArgs,
	start: Instant,
	lines: Lines,
	/// Lines already printed.
	printed: usize,
	a: Connection,
	b: Option<Connection>,
	vars: Vars,
}

/// Notifications that say nothing about streams.
const NOISE: &[&str] = &[
	"notifyconnectioninfo",
	"setconnectioninfo",
	"notifyclientneededpermissions",
	"notifychannelgrouplist",
	"notifyservergrouplist",
	"notifyclientchannelgroupchanged",
	"clientinitiv",
	"initivexpand",
	"clientek",
	"clientinit ",
	"notifyclientpermlist",
	"channellist ",
	"notifychannelsubscribed",
	"notifychanneledited",
];

pub async fn run(args: ProbeStreamArgs) -> Result<()> {
	let start = Instant::now();
	let lines: Lines = Arc::default();
	let a = connect(&args.address, "probe-a", args.streamer_identity.as_ref(), None).await?;
	let mut probe = Probe { args, start, lines, printed: 0, a, b: None, vars: Vars::default() };
	probe.attach(Who::A)?;
	let result = probe.script().await;
	probe.print();
	probe.finish().await;
	result
}

async fn connect(
	address: &str,
	nick: &str,
	identity: Option<&PathBuf>,
	channel: Option<ChannelId>,
) -> Result<Connection> {
	let nick = format!("{nick}-{}", std::process::id() % 10_000);
	let mut options = Connection::build(address.to_owned()).name(nick);
	if let Some(path) = identity {
		options = options.identity(identity::load(path)?);
	}
	if let Some(channel) = channel {
		options = options.channel_id(channel);
	}
	let mut con = options.connect().context("failed to start connection")?;
	session::wait_connected(&mut con, Instant::now() + Duration::from_secs(30)).await?;
	Ok(con)
}

fn raw(text: &str) -> OutCommand {
	OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, text)
}

impl Probe {
	fn con(&mut self, who: Who) -> Result<&mut Connection> {
		match who {
			Who::A => Ok(&mut self.a),
			Who::B => self.b.as_mut().context("B is not connected"),
		}
	}

	/// Record every command `who` sends and receives from now on.
	fn attach(&mut self, who: Who) -> Result<()> {
		let lines = self.lines.clone();
		let client = self.con(who)?.get_tsproto_client_mut()?;
		client.event_listeners.push(Box::new(move |event: &RawEvent| {
			let (outgoing, packet_type, content) = match event {
				RawEvent::ReceivePacket(p) => (false, p.header().packet_type(), p.content()),
				RawEvent::SendPacket(p) => (true, p.header().packet_type(), p.content()),
				_ => return,
			};
			if !packet_type.is_command() {
				return;
			}
			let text = String::from_utf8_lossy(content).into_owned();
			let line = Line { at: Instant::now(), who, outgoing, text };
			lines.lock().unwrap_or_else(PoisonError::into_inner).push(line);
		}));
		Ok(())
	}

	fn note(&mut self, text: &str) {
		self.print();
		println!("\n## {text}");
	}

	/// Print the lines recorded since the last call.
	fn print(&mut self) {
		let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
		for line in &lines[self.printed..] {
			if !self.args.verbose && NOISE.iter().any(|n| line.text.starts_with(n)) {
				continue;
			}
			let mut text = line.text.clone();
			if text.len() > 600 {
				let mut end = 600;
				while !text.is_char_boundary(end) {
					end -= 1;
				}
				text.truncate(end);
				text.push_str(" …");
			}
			let t = line.at.duration_since(self.start).as_secs_f32();
			let arrow = if line.outgoing { "->" } else { "<-" };
			println!("[{t:7.3}] {} {arrow} {text}", line.who.label());
		}
		self.printed = lines.len();
	}

	/// Drive both connections until `deadline` or until `done` returns true
	/// for an item; `true` if it did.
	async fn pump(
		&mut self,
		deadline: Instant,
		mut done: impl FnMut(Who, &StreamItem) -> bool,
	) -> Result<bool> {
		loop {
			let (who, item) = {
				let mut a_events = self.a.events();
				let a = a_events.next();
				let b = async {
					match &mut self.b {
						Some(b) => b.events().next().await,
						None => future::pending().await,
					}
				};
				tokio::select! {
					item = a => (Who::A, item),
					item = b => (Who::B, item),
					() = sleep_until(deadline) => return Ok(false),
				}
			};
			match item {
				None => bail!("{} was disconnected", who.label()),
				Some(Err(e)) => bail!("{}: {e}", who.label()),
				Some(Ok(item)) => {
					if done(who, &item) {
						return Ok(true);
					}
				}
			}
		}
	}

	async fn settle(&mut self) -> Result<()> {
		let deadline = Instant::now() + Duration::from_millis(self.args.settle_ms);
		self.pump(deadline, |_, _| false).await?;
		Ok(())
	}

	/// Send a raw command (a template, see [`Vars`]), print the answer and
	/// what arrives shortly after; the server's error, if any.
	async fn send(&mut self, who: Who, template: &str) -> Result<Option<String>> {
		let text = self.vars.fill(template);
		let handle = raw(&text).send_with_result(self.con(who)?)?;
		let mut result = None;
		let deadline = Instant::now() + Duration::from_secs(10);
		self.pump(deadline, |w, item| match item {
			StreamItem::MessageResult(h, r) if w == who && *h == handle => {
				result = Some(r.as_ref().err().map(ToString::to_string));
				true
			}
			_ => false,
		})
		.await?;
		self.settle().await?;
		self.print();
		let error = match result {
			None => Some("no answer".to_owned()),
			Some(error) => error,
		};
		println!(
			"          {} => {}",
			who.label(),
			error.as_deref().map_or_else(|| "ok".to_owned(), |e| format!("error: {e}"))
		);
		Ok(error)
	}

	fn own_client(&mut self, who: Who) -> Result<ClientId> {
		Ok(self.con(who)?.get_state()?.own_client)
	}

	async fn connect_b(&mut self, channel: Option<ChannelId>) -> Result<()> {
		let identity = self.args.viewer_identity.clone();
		let connecting = connect(&self.args.address, "probe-b", identity.as_ref(), channel);
		// Keep A's connection going while B connects.
		let b = {
			let mut a = self.a.events();
			let drain_a = async {
				loop {
					if a.next().await.is_none() {
						future::pending::<()>().await;
					}
				}
			};
			tokio::select! {
				b = connecting => b?,
				() = drain_a => unreachable!(),
			}
		};
		self.b = Some(b);
		self.attach(Who::B)?;
		self.vars.b = self.own_client(Who::B)?.0;
		self.settle().await?;
		self.print();
		let a_id = ClientId(self.vars.a);
		let state = self.con(Who::B)?.get_state()?;
		let own = state.own_client;
		let a = state.clients.get(&a_id);
		println!(
			"          B is clid {} in channel {:?}; B's book: A {}",
			own.0,
			state.clients.get(&own).map(|c| c.channel.0),
			a.map_or_else(
				|| "not visible".to_owned(),
				|a| format!(
					"in channel {}, is_streaming {:?}, metadata {:?}",
					a.channel.0, a.is_streaming, a.metadata
				)
			)
		);
		Ok(())
	}

	async fn disconnect_b(&mut self) -> Result<()> {
		if let Some(mut b) = self.b.take() {
			b.disconnect(DisconnectOptions::new())?;
			let drain = b.events().for_each(|_| future::ready(()));
			let _ = tokio::time::timeout(Duration::from_secs(3), drain).await;
		}
		Ok(())
	}

	async fn finish(&mut self) {
		let _ = self.disconnect_b().await;
		if self.a.disconnect(DisconnectOptions::new()).is_ok() {
			let drain = self.a.events().for_each(|_| future::ready(()));
			let _ = tokio::time::timeout(Duration::from_secs(3), drain).await;
		}
	}

	/// Candidate commands that could tell B about A's stream.
	const QUERIES: &[&str] = &[
		"requeststreaminfo id={id}",
		"requeststreaminfo id={id} clid={a}",
		"requeststreaminfo clid={a}",
		"requeststreaminfo stream_id={id}",
		"requeststreaminfo stream_id={id} clid={a}",
		"requeststreaminfo streamid={id} clid={a}",
		"requeststreaminfo id={id} clid={a} msg",
		"requeststreaminfo id={id} clid={a} return_code=probe",
		"requeststreaminfo cid={cid}",
		"requeststreaminfo",
		"clientgetvariables clid={a}",
		"clientinfo clid={a}",
		"requeststreamattendees id={id}",
		"streamattendees id={id}",
		"streamlist",
		"liststreams",
		"requeststreams",
		"requeststreamlist",
	];

	async fn queries(&mut self) -> Result<()> {
		for q in Self::QUERIES {
			self.send(Who::B, q).await?;
		}
		Ok(())
	}

	async fn script(&mut self) -> Result<()> {
		let version = self.a.get_state()?.server.version.clone();
		self.vars.a = self.own_client(Who::A)?.0;
		println!("server {version}; A is clid {}", self.vars.a);

		self.note("A creates a channel, moves there and starts a stream");
		let name = format!("probe-{}", std::process::id() % 10_000);
		self.send(Who::A, &format!("channelcreate channel_name={name}")).await?;
		let cid = {
			let state = self.a.get_state()?;
			let channel = state.channels.values().find(|c| c.name == name);
			channel.context("the new channel is not in A's book")?.id
		};
		self.vars.cid = cid.0;
		// The creator is usually moved already ("already member of channel").
		self.send(Who::A, "clientmove cid={cid} clid={a}").await?;
		let setup = StreamSetup { name: "late join probe".into(), ..Default::default() };
		let handle = proto::setup(&setup).send_with_result(&mut self.a)?;
		let own = ClientId(self.vars.a);
		let mut id = None;
		let deadline = Instant::now() + Duration::from_secs(10);
		self.pump(deadline, |who, item| {
			if let (Who::A, StreamItem::MessageEvent(msg)) = (who, item) {
				for n in StreamNotification::from_message(msg) {
					if let StreamNotification::Started { info, .. } = n
						&& info.streamer == own
					{
						id = Some(info.id);
					}
				}
			}
			matches!(item, StreamItem::MessageResult(h, Err(_)) if *h == handle) || id.is_some()
		})
		.await?;
		self.vars.id = id.context("setupstream was not confirmed")?;
		self.settle().await?;
		self.print();
		println!("          stream id {}", self.vars.id);

		self.note("B connects to the default channel, A's stream runs in another channel");
		self.connect_b(None).await?;
		self.queries().await?;
		self.send(Who::B, "channelsubscribeall").await?;
		self.send(Who::B, "requeststreaminfo clid={a}").await?;
		self.send(Who::B, "joinstreamrequest id={id} clid={a} msg=probe is_remove=0").await?;
		self.send(Who::B, "joinstreamrequest id={id} clid={a} msg is_remove=1").await?;

		self.note("B switches into A's channel");
		self.send(Who::B, "clientmove cid={cid} clid={b}").await?;
		self.queries().await?;
		for extra in self.args.extra.clone() {
			self.send(Who::B, &extra).await?;
		}

		self.note("B asks to join with the id it was not told (does the server relay it?)");
		self.send(Who::B, "joinstreamrequest id={id} clid={a} msg=probe is_remove=0").await?;
		self.send(Who::B, "joinstreamrequest id={id} clid={a} msg is_remove=1").await?;

		self.note("A tries to re-announce the stream while B is in the channel");
		self.send(Who::A, "updatestream id={id} name=renamed\\sprobe").await?;
		self.send(Who::A, "updatestream id={id}").await?;
		self.send(Who::A, "updatestream stream_id={id} name=renamed\\sprobe").await?;
		self.send(
			Who::A,
			"updatestream id={id} name=renamed\\sprobe type=3 bitrate=4608 accessibility=1 \
			 mode=1 viewer_limit=0 audio=1",
		)
		.await?;
		self.send(
			Who::A,
			"updatestream id={id} name=renamed\\sagain type=3 access=1 mode=1 bitrate=4000 \
			 viewer_limit=0 audio=1",
		)
		.await?;
		self.send(Who::A, "requeststreaminfo id={id} clid={a}").await?;
		self.send(
			Who::A,
			"setupstream name=second type=3 bitrate=4608 accessibility=1 mode=1 viewer_limit=0 \
			 audio=1",
		)
		.await?;

		self.note("A publishes the stream id in its client_meta_data");
		self.send(Who::A, "clientupdate client_meta_data=voelin-stream={id}").await?;

		self.note("B reconnects straight into A's channel");
		self.disconnect_b().await?;
		self.settle().await?;
		self.connect_b(Some(cid)).await?;
		self.send(Who::B, "requeststreaminfo id={id} clid={a}").await?;
		self.send(Who::B, "clientgetvariables clid={a}").await?;
		self.send(Who::B, "joinstreamrequest id={id} clid={a} msg=probe is_remove=0").await?;
		self.send(Who::B, "joinstreamrequest id={id} clid={a} msg is_remove=1").await?;

		self.note("A clears client_meta_data and stops the stream");
		self.send(Who::A, "clientupdate client_meta_data").await?;
		self.send(Who::A, "stopstream id={id} reason=1").await?;
		Ok(())
	}
}
