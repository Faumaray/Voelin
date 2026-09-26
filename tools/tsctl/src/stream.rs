//! `tsctl connect ... stream start|list|watch`: TeamSpeak 6 streams.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use futures::prelude::*;
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};
use tokio::time::{Instant, Interval, sleep_until};
use tsc_model::ServerFlavor;
use tsc_stream::{
	EndReason, FrameSource, MediaKind, Output, PeerConfig, Request, StreamEvent, StreamInfo,
	StreamNotification, StreamSetup, StreamerEvent, StreamerOptions, Streams, SyntheticSource,
	WatchEvent,
};
use tsclientlib::events::{Event, PropertyId};
use tsclientlib::prelude::*;
use tsclientlib::{ClientId, Connection, MessageHandle, StreamItem};

#[derive(Args, Debug, Clone)]
pub struct StreamArgs {
	#[command(subcommand)]
	command: StreamCommand,
	/// Media over 127.0.0.1 only (both ends on this machine), no STUN.
	#[arg(long, global = true)]
	loopback: bool,
	/// STUN server (`host:port`) for a public candidate; repeatable [default: TeamSpeak's].
	#[arg(long, global = true)]
	stun: Vec<String>,
	/// Do not use STUN.
	#[arg(long, global = true, conflicts_with = "stun")]
	no_stun: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum StreamCommand {
	/// Stream in our channel. Without `--auto-accept`, answer join requests on
	/// stdin: `accept <clid>`, `deny <clid>`, `kick <clid>`, `stop`.
	Start {
		/// Stream name.
		#[arg(long, default_value = "tsctl")]
		name: String,
		/// Send synthetic frames (tiny VP8 keyframes and silent Opus); needed
		/// until capture and encoders exist.
		#[arg(long)]
		synthetic: bool,
		/// Accept every viewer.
		#[arg(long)]
		auto_accept: bool,
		/// Stop after this many seconds [default: until Ctrl-C].
		#[arg(long)]
		seconds: Option<u64>,
		/// Synthetic video frames per second.
		#[arg(long, default_value_t = 30)]
		fps: u32,
		/// Size of each synthetic video frame in bytes.
		#[arg(long, default_value_t = 4000)]
		frame_size: usize,
		/// No audio track.
		#[arg(long)]
		no_audio: bool,
	},
	/// List the streams in our channel.
	List {
		/// How long to collect stream announcements before printing.
		#[arg(long, default_value_t = 1500)]
		settle_ms: u64,
	},
	/// Watch a stream (the first one that shows up if neither `--id` nor
	/// `--streamer-nick` is given).
	Watch {
		/// Stream id.
		#[arg(long)]
		id: Option<String>,
		/// Nickname of the streamer.
		#[arg(long)]
		streamer_nick: Option<String>,
		/// Exit successfully once this many video frames arrived.
		#[arg(long)]
		expect_frames: Option<u64>,
		/// Give up after this many seconds; fails if `--expect-frames` was not reached.
		#[arg(long)]
		timeout: Option<u64>,
	},
}

pub async fn run(con: &mut Connection, args: &StreamArgs) -> Result<()> {
	let (own, version) = {
		let state = con.get_state()?;
		(state.own_client, state.server.version.clone())
	};
	if !ServerFlavor::from_version_string(&version).capabilities().streams {
		bail!("streams need a TeamSpeak 6 server (this one is {version})");
	}
	let mut config = if args.loopback { PeerConfig::loopback() } else { PeerConfig::default() };
	if !args.stun.is_empty() {
		config.stun_servers = args.stun.clone();
	} else if args.no_stun {
		config.stun_servers.clear();
	}
	let mut driver = Driver { streams: Streams::new(own, config), pending: HashMap::new() };
	let result = match &args.command {
		StreamCommand::Start {
			name,
			synthetic,
			auto_accept,
			seconds,
			fps,
			frame_size,
			no_audio,
		} => {
			let source = frame_source(*synthetic, *fps, *frame_size, !no_audio)?;
			let setup = StreamSetup { name: name.clone(), audio: !no_audio, ..Default::default() };
			let options = StreamerOptions { setup, auto_accept: *auto_accept };
			start(con, &mut driver, options, source, seconds.map(Duration::from_secs)).await
		}
		StreamCommand::List { settle_ms } => {
			list(con, &mut driver, Duration::from_millis(*settle_ms)).await
		}
		StreamCommand::Watch { id, streamer_nick, expect_frames, timeout } => {
			let target = Target { id: id.clone(), streamer_nick: streamer_nick.clone() };
			let timeout = timeout.map(Duration::from_secs);
			watch(con, &mut driver, &target, *expect_frames, timeout).await
		}
	};
	driver.finish(con).await;
	result
}

/// Where a streamer's frames come from. Capture and encoders (`tsc-media`)
/// plug in here.
fn frame_source(
	synthetic: bool,
	fps: u32,
	frame_size: usize,
	audio: bool,
) -> Result<Box<dyn FrameSource>> {
	if !synthetic {
		bail!("only synthetic frames are available yet: pass --synthetic");
	}
	Ok(Box::new(SyntheticSource::new(fps, frame_size, audio)))
}

/// What woke the driver.
enum Wake {
	/// Connection or peer activity; the new events are returned by `flush`.
	Stream,
	Tick,
	Line(Option<String>),
	Interrupted,
	Deadline,
}

/// Runs the stream sessions on the connection.
struct Driver {
	streams: Streams,
	/// Stream commands waiting for the server's answer.
	pending: HashMap<MessageHandle, Request>,
}

impl Driver {
	/// Wait for the next input and handle it.
	async fn next(
		&mut self,
		con: &mut Connection,
		deadline: Option<Instant>,
		tick: Option<&mut Interval>,
		lines: Option<&mut Lines<BufReader<Stdin>>>,
	) -> Result<Wake> {
		let item = {
			let mut events = con.events();
			let deadline = async {
				match deadline {
					Some(d) => sleep_until(d).await,
					None => future::pending().await,
				}
			};
			let tick = async {
				match tick {
					Some(t) => t.tick().await,
					None => future::pending().await,
				}
			};
			let line = async {
				match lines {
					Some(l) => l.next_line().await,
					None => future::pending().await,
				}
			};
			tokio::select! {
				item = events.next() => item,
				() = self.streams.wait_peers() => return Ok(Wake::Stream),
				_ = tick => return Ok(Wake::Tick),
				line = line => return Ok(Wake::Line(line?)),
				_ = tokio::signal::ctrl_c() => return Ok(Wake::Interrupted),
				() = deadline => return Ok(Wake::Deadline),
			}
		};
		match item {
			None => bail!("disconnected from server"),
			Some(item) => self.item(con, item?).await?,
		}
		Ok(Wake::Stream)
	}

	async fn item(&mut self, con: &mut Connection, item: StreamItem) -> Result<()> {
		match item {
			StreamItem::MessageEvent(msg) => {
				for n in StreamNotification::from_message(&msg) {
					self.streams.handle_notification(n).await;
				}
			}
			StreamItem::MessageResult(handle, result) => {
				if let (Some(request), Err(e)) = (self.pending.remove(&handle), result) {
					self.streams.request_failed(&request, &e.to_string());
				}
			}
			StreamItem::BookEvents(events) => {
				let state = con.get_state()?;
				for e in &events {
					if let Event::PropertyChanged {
						id: PropertyId::ClientIsStreaming(clid), ..
					} = e && let Some(streaming) =
						state.clients.get(clid).and_then(|c| c.is_streaming)
					{
						self.streams.set_client_streaming(*clid, streaming);
					}
				}
				self.streams.retain_streamers(|c| state.clients.contains_key(&c));
			}
			_ => {}
		}
		Ok(())
	}

	/// Send the sessions' requests; returns their events.
	fn flush(&mut self, con: &mut Connection) -> Result<Vec<StreamEvent>> {
		let mut events = Vec::new();
		while let Some(output) = self.streams.poll_output() {
			match output {
				Output::Request(request) => {
					let handle = request.to_command().send_with_result(con)?;
					self.pending.insert(handle, request);
				}
				Output::Event(e) => events.push(e),
			}
		}
		Ok(events)
	}

	/// Stop streaming and watching; wait briefly for the server's answers.
	async fn finish(&mut self, con: &mut Connection) {
		self.streams.close();
		if self.flush(con).is_err() {
			return;
		}
		let deadline = Instant::now() + Duration::from_secs(3);
		while !self.pending.is_empty() {
			match self.next(con, Some(deadline), None, None).await {
				Ok(Wake::Stream) => {
					if self.flush(con).is_err() {
						return;
					}
				}
				_ => return,
			}
		}
	}
}

fn nick(con: &Connection, client: ClientId) -> String {
	con.get_state()
		.ok()
		.and_then(|s| s.clients.get(&client).map(|c| c.name.clone()))
		.unwrap_or_else(|| "?".into())
}

fn end_text(reason: &EndReason) -> String {
	match reason {
		EndReason::Local => "stopped".into(),
		EndReason::Denied => "denied by the streamer".into(),
		EndReason::Stopped => "the stream was stopped".into(),
		EndReason::Removed(reason) => format!("removed by the streamer ({reason:?})"),
		EndReason::Failed(e) => format!("failed: {e}"),
	}
}

async fn start(
	con: &mut Connection,
	driver: &mut Driver,
	options: StreamerOptions,
	mut source: Box<dyn FrameSource>,
	length: Option<Duration>,
) -> Result<()> {
	let auto_accept = options.auto_accept;
	driver.streams.start(options)?;
	let deadline = length.map(|l| Instant::now() + l);
	let mut tick = tokio::time::interval(source.interval());
	tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	let mut lines = (!auto_accept).then(|| BufReader::new(tokio::io::stdin()).lines());
	let (mut live, mut video, mut audio) = (false, 0u64, 0u64);
	let mut frames = Vec::new();
	loop {
		let wake = driver.next(con, deadline, live.then_some(&mut tick), lines.as_mut()).await?;
		match wake {
			Wake::Stream => {}
			Wake::Tick => {
				source.poll_frames(std::time::Instant::now(), &mut frames);
				let connected =
					driver.streams.streamer().is_some_and(|s| s.has_connected_viewers());
				for frame in frames.drain(..) {
					if connected {
						match frame.kind {
							MediaKind::Video => video += 1,
							MediaKind::Audio => audio += 1,
						}
					}
					driver.streams.write_frame(&frame);
				}
			}
			Wake::Line(None) => lines = None,
			Wake::Line(Some(line)) => {
				if let Err(e) = command_line(driver, &line).await {
					println!("{e}");
				}
			}
			Wake::Interrupted | Wake::Deadline => {
				driver.streams.stop()?;
			}
		}
		for event in driver.flush(con)? {
			let StreamEvent::Streamer(event) = event else { continue };
			match event {
				StreamerEvent::Live { id } => {
					live = true;
					println!("stream {id} is live");
				}
				StreamerEvent::Request { viewer, message } => {
					println!(
						"{} (clid {}) asks to watch: {message:?}; type `accept {}` or `deny {}`",
						nick(con, viewer),
						viewer.0,
						viewer.0,
						viewer.0
					);
				}
				StreamerEvent::Viewers(viewers) => {
					let list: Vec<String> = viewers
						.iter()
						.map(|v| {
							format!("{} (clid {}, {:?})", nick(con, v.client), v.client.0, v.state)
						})
						.collect();
					println!("viewers: [{}]", list.join(", "));
				}
				StreamerEvent::KeyframeRequest => source.request_keyframe(),
				StreamerEvent::Ended(reason) => {
					println!("sent {video} video / {audio} audio frames to connected viewers");
					return match reason {
						EndReason::Local => Ok(()),
						other => bail!("stream ended: {}", end_text(&other)),
					};
				}
			}
		}
	}
}

/// `accept <clid>`, `deny <clid>`, `kick <clid>`, `stop` while streaming.
async fn command_line(driver: &mut Driver, line: &str) -> Result<()> {
	let mut words = line.split_whitespace();
	let command = words.next().unwrap_or_default();
	let client = words.next().and_then(|w| w.parse().ok()).map(ClientId);
	match (command, client) {
		("accept", Some(c)) => driver.streams.respond(c, true).await?,
		("deny", Some(c)) => driver.streams.respond(c, false).await?,
		("kick", Some(c)) => driver.streams.kick(c)?,
		("stop" | "quit", _) => driver.streams.stop()?,
		("", _) => {}
		_ => bail!("commands: accept <clid>, deny <clid>, kick <clid>, stop"),
	}
	Ok(())
}

async fn list(con: &mut Connection, driver: &mut Driver, settle: Duration) -> Result<()> {
	let deadline = Instant::now() + settle;
	loop {
		match driver.next(con, Some(deadline), None, None).await? {
			Wake::Deadline | Wake::Interrupted => break,
			_ => {
				driver.flush(con)?;
			}
		}
	}
	let streams: Vec<&StreamInfo> = driver.streams.directory().iter().collect();
	for s in &streams {
		println!(
			"{}  {:?} by {} (clid {}), {} kbit/s{}",
			s.id,
			s.name,
			nick(con, s.streamer),
			s.streamer.0,
			s.bitrate,
			if s.audio { ", audio" } else { "" }
		);
	}
	// Streams that started before we connected are only known by the flag.
	let state = con.get_state()?;
	let mut unannounced = 0;
	for c in state.clients.values() {
		if c.is_streaming == Some(true) && !streams.iter().any(|s| s.streamer == c.id) {
			println!("?  {} (clid {}) is streaming; id not announced to us", c.name, c.id.0);
			unannounced += 1;
		}
	}
	if streams.is_empty() && unannounced == 0 {
		println!("no streams");
	}
	Ok(())
}

struct Target {
	id: Option<String>,
	streamer_nick: Option<String>,
}

impl Target {
	/// The stream to watch and its streamer, once known.
	fn find(&self, con: &Connection, streams: &Streams) -> Option<(String, ClientId)> {
		let state = con.get_state().ok()?;
		let streamer = match &self.streamer_nick {
			Some(nick) => Some(state.clients.values().find(|c| c.name == *nick)?.id),
			None => None,
		};
		let own = streams.own_client();
		let found = streams.directory().iter().find(|s| {
			s.streamer != own
				&& self.id.as_ref().is_none_or(|id| *id == s.id)
				&& streamer.is_none_or(|c| c == s.streamer)
		});
		if let Some(s) = found {
			return Some((s.id.clone(), s.streamer));
		}
		// A known id of a stream that was not announced to us: its streamer
		// is the named client, or the only other client that streams.
		let id = self.id.clone()?;
		let streamer = streamer.or_else(|| {
			let mut streaming = state
				.clients
				.values()
				.filter(|c| c.id != own && c.is_streaming == Some(true))
				.map(|c| c.id);
			let first = streaming.next()?;
			streaming.next().is_none().then_some(first)
		})?;
		Some((id, streamer))
	}
}

async fn watch(
	con: &mut Connection,
	driver: &mut Driver,
	target: &Target,
	expect: Option<u64>,
	timeout: Option<Duration>,
) -> Result<()> {
	let deadline = timeout.map(|t| Instant::now() + t);
	let mut watching: Option<String> = None;
	let (mut video, mut audio) = (0u64, 0u64);
	let mut codec = None;
	let mut waiting_told = false;
	loop {
		if watching.is_none() {
			if let Some((id, streamer)) = target.find(con, &driver.streams) {
				println!("watching {id} of {} (clid {})", nick(con, streamer), streamer.0);
				driver.streams.watch_from(&id, streamer, "")?;
				watching = Some(id);
			} else if !waiting_told {
				println!("waiting for a stream");
				waiting_told = true;
			}
		}
		for event in driver.flush(con)? {
			let StreamEvent::Watch { id, event } = event else { continue };
			if Some(&id) != watching.as_ref() {
				continue;
			}
			match event {
				WatchEvent::Accepted => println!("accepted, connecting"),
				WatchEvent::Connected => println!("connected"),
				WatchEvent::Frame(f) => {
					match f.kind {
						MediaKind::Video => video += 1,
						MediaKind::Audio => audio += 1,
					}
					if f.kind == MediaKind::Video && codec.replace(f.codec).is_none() {
						println!("first video frame ({:?}, {} bytes)", f.codec, f.data.len());
					}
					if expect.is_some_and(|n| video >= n) {
						println!("received {video} video / {audio} audio frames");
						return Ok(());
					}
				}
				WatchEvent::Ended(reason) => {
					println!("received {video} video / {audio} audio frames");
					match expect {
						Some(n) => bail!("{} before {n} video frames arrived", end_text(&reason)),
						None => {
							println!("{}", end_text(&reason));
							return Ok(());
						}
					}
				}
			}
		}
		match driver.next(con, deadline, None, None).await? {
			Wake::Deadline | Wake::Interrupted => {
				println!("received {video} video / {audio} audio frames");
				return match expect {
					Some(n) => bail!("timed out waiting for {n} video frames"),
					None => Ok(()),
				};
			}
			_ => {}
		}
	}
}
