//! `voelinctl connect ... stream start|list|watch`: TeamSpeak 6 streams.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use futures::prelude::*;
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};
use tokio::time::{Instant, Interval, sleep_until};
use tsclientlib::prelude::*;
use tsclientlib::{ClientId, Connection, MessageHandle, StreamItem};
use voelin_core::media::voelin_media::capture::SourceId;
use voelin_core::media::voelin_media::{Codec, Codecs, VideoFrame, convert};
use voelin_core::media::{
	CaptureBackend, EncodedSource, EncoderPreference, Latest, Streamer, StreamerConfig,
	VideoPipeline, peer_config, preferred_codec, stream_codec, video_codec,
};
use voelin_model::ServerFlavor;
use voelin_stream::{
	ClientState, EndReason, FrameSource, H264Profile, LayerId, LayerSpec, MediaKind, Output,
	PeerConfig, Request, SrtpProfile, StreamEvent, StreamInfo, StreamNotification, StreamSetup,
	StreamerEvent, StreamerOptions, Streams, SyntheticSource, VideoCodec, ViewerInfo, WatchEvent,
};

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
	/// SRTP profiles in order of preference, comma separated
	/// (`AES_CM_128_HMAC_SHA1_80`, `AEAD_AES_128_GCM`, `AEAD_AES_256_GCM`)
	/// [default: in this order].
	#[arg(long, global = true, value_delimiter = ',')]
	srtp: Vec<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum StreamCommand {
	/// Stream in our channel: the screen (VP8) and system audio (Opus).
	/// Without `--auto-accept`, answer join requests on stdin: `accept
	/// <clid>`, `deny <clid>`, `kick <clid>`, `stop`.
	Start {
		/// Stream name.
		#[arg(long, default_value = "voelinctl")]
		name: String,
		/// What to capture: `synthetic` (a moving test pattern and a tone),
		/// `x11[:<monitor>]`, `screen[:<monitor>]` (this session's backend),
		/// `window:<id>` or `portal` (the desktop's dialog).
		#[arg(long, default_value = "screen")]
		source: String,
		/// Same as `--source synthetic`.
		#[arg(long, conflicts_with = "source")]
		synthetic: bool,
		/// Size of the test pattern.
		#[arg(long, default_value = "1280x720")]
		size: String,
		/// Accept every viewer.
		#[arg(long)]
		auto_accept: bool,
		/// Stop after this many seconds [default: until Ctrl-C].
		#[arg(long)]
		seconds: Option<u64>,
		/// Video frames per second.
		#[arg(long, default_value_t = 30)]
		fps: u32,
		/// Video bitrate in kbit/s (announced in `setupstream` too).
		#[arg(long, default_value_t = 4608)]
		bitrate: u32,
		/// No audio track.
		#[arg(long)]
		no_audio: bool,
		/// A simulcast layer, repeatable: `<scale or WxH>:<bitrate>[:<option>...]`
		/// with options `min=<bitrate>` (estimate a viewer needs; default half
		/// the bitrate), `max=<bitrate>`, `fps=<n>`, `rid=<id>`, `id=<n>`
		/// (default: position). Bitrates in bit/s with optional k or M, e.g.
		/// `--layer 1.0:6000k --layer 0.5:1500k:min=800k`.
		#[arg(long = "layer", value_parser = parse_layer)]
		layers: Vec<LayerSpec>,
		/// Offer RID simulcast (the layers' `rid`s) to viewers. Only for peers
		/// that support it; official TeamSpeak clients do not.
		#[arg(long)]
		simulcast: bool,
		/// Send placeholder frames (tiny VP8 keyframes padded to each layer's
		/// bitrate, no encoder) instead of capturing.
		#[arg(long, conflicts_with_all = ["source", "synthetic"])]
		placeholder: bool,
		/// Video codec: vp8, vp9, h264, av1 [default: VP8, which every client
		/// decodes; with --encoder, that encoder's codec]. One codec is offered.
		#[arg(long)]
		codec: Option<String>,
		/// Encoder: `auto`, `software` or a backend name from `voelinctl
		/// stream encoders`.
		#[arg(long, default_value = "auto")]
		encoder: String,
		/// No hardware encoders unless named with --encoder.
		#[arg(long)]
		no_hardware: bool,
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
		/// Exit successfully once this many video frames were decoded.
		#[arg(long)]
		expect_frames: Option<u64>,
		/// Give up after this many seconds; fails if `--expect-frames` was not reached.
		#[arg(long)]
		timeout: Option<u64>,
		/// Save the last decoded picture as PNG.
		#[arg(long)]
		save_frame: Option<PathBuf>,
		/// Ask for this simulcast layer (by id) once the streamer's offer
		/// lists its layers (Voelin streamers with several do).
		#[arg(long)]
		layer: Option<u16>,
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
	if !args.srtp.is_empty() {
		config.srtp_profiles = parse_srtp(&args.srtp)?;
	}
	let mut codecs = Codecs::new();
	if let StreamCommand::Start {
		simulcast,
		placeholder,
		codec,
		encoder,
		no_hardware,
		synthetic,
		size,
		fps,
		bitrate,
		..
	} = &args.command
	{
		config.simulcast = *simulcast;
		codecs.set_preference(EncoderPreference {
			hardware: !no_hardware,
			backend: encoder.parse().unwrap_or_default(),
		});
		if *placeholder {
			// The placeholder frames are VP8.
			config.video_codecs = vec![VideoCodec::Vp8];
		} else {
			let configured = match codec {
				Some(codec) => Some(codec.parse().map_err(|e| anyhow::anyhow!("{e}"))?),
				// One codec is offered (below): by default the one every
				// client decodes. A named encoder brings its own codec.
				None if encoder == "auto" => Some(Codec::Vp8),
				None => None,
			};
			if let Some(codec) = preferred_codec(&codecs, configured) {
				config.video_codecs = vec![video_codec(codec)];
			}
			if *synthetic {
				let (w, h) = parse_size(size)?;
				let bitrate = u64::from(*bitrate) * 1000;
				config.set_h264_format(H264Profile::ConstrainedHigh, w, h, *fps, bitrate);
			}
		}
	}
	// Offer the codec we encode, accept what we decode.
	let codecs = Arc::new(codecs);
	let mut config = match &args.command {
		StreamCommand::Start { placeholder: true, .. } => config,
		_ => peer_config(&codecs, config),
	};
	if matches!(args.command, StreamCommand::Start { .. }) {
		// Frames go out through a `FrameSource`, which does not tell codecs
		// apart: offer the stream codec alone.
		config.video_codecs.truncate(1);
	}
	let codec = stream_codec(&codecs, &config);
	let mut driver = Driver { streams: Streams::new(own, config), pending: HashMap::new() };
	driver.sync_clients(con)?;
	let result = match &args.command {
		StreamCommand::Start {
			name,
			source,
			synthetic,
			size,
			auto_accept,
			seconds,
			fps,
			bitrate,
			no_audio,
			layers,
			placeholder,
			..
		} => {
			let mut layers = layers.clone();
			number_layers(&mut layers);
			let layers = &layers;
			for l in layers {
				println!(
					"layer {}: {}, {}, needs {}{}",
					l.id,
					l.size.map_or(format!("scale {}", l.scale), |(w, h)| format!("{w}x{h}")),
					rate(l.bitrate),
					rate(l.min_bitrate),
					l.rid.as_ref().map(|r| format!(", rid {r}")).unwrap_or_default()
				);
			}
			let (source, audio) = if *placeholder {
				let source = SyntheticSource::with_layers(
					*fps,
					(u64::from(*bitrate) * 1000 / 8 / u64::from((*fps).max(1))) as usize,
					!no_audio,
					layers,
				);
				(Source::Placeholder(source), !no_audio)
			} else {
				let source = if *synthetic { "synthetic" } else { source.as_str() };
				let (source, backend) = parse_source(source)?;
				let Some(codec) = codec else { bail!("no video encoder in this build") };
				let config = StreamerConfig {
					source,
					backend,
					fps: *fps,
					bitrate_kbps: *bitrate,
					codec,
					encoder: codecs.preference().clone(),
					audio: !no_audio,
					synthetic_size: parse_size(size)?,
					layers: layers.clone(),
					..StreamerConfig::default()
				};
				let streamer = Streamer::start(&codecs, config).await.context("capture")?;
				let backend = streamer.stats().layers.first().and_then(|l| l.backend);
				let backend = backend.map_or("?".to_owned(), |b| b.to_string());
				println!("capturing with {} ({codec} through {backend})", streamer.backend());
				if let Some(e) = streamer.audio_error() {
					println!("audio: {e}");
				}
				let audio = streamer.has_audio();
				(Source::Encoded(EncodedSource::new(streamer)), audio)
			};
			let setup =
				StreamSetup { name: name.clone(), bitrate: *bitrate, audio, ..Default::default() };
			let options = StreamerOptions {
				setup,
				auto_accept: *auto_accept,
				layers: layers.clone(),
				..Default::default()
			};
			start(con, &mut driver, options, source, seconds.map(Duration::from_secs)).await
		}
		StreamCommand::List { settle_ms } => {
			list(con, &mut driver, Duration::from_millis(*settle_ms)).await
		}
		StreamCommand::Watch { id, streamer_nick, expect_frames, timeout, save_frame, layer } => {
			let target =
				Target { id: id.clone(), streamer_nick: streamer_nick.clone(), layer: *layer };
			let timeout = timeout.map(Duration::from_secs);
			let expect = *expect_frames;
			let save = save_frame.as_deref();
			watch(con, &mut driver, codecs, &target, expect, timeout, save).await
		}
	};
	driver.finish(con).await;
	result
}

/// `synthetic`, `x11[:<monitor>]`, `screen[:<monitor>]`, `window:<id>`, `portal`.
fn parse_source(spec: &str) -> Result<(SourceId, CaptureBackend)> {
	let (kind, arg) = spec.split_once(':').map_or((spec, None), |(k, a)| (k, Some(a)));
	let number = |default: u64| -> Result<u64> {
		arg.map_or(Ok(default), |a| {
			let a = a.trim_start_matches("0x");
			let radix = if a.len() < arg.unwrap_or_default().len() { 16 } else { 10 };
			u64::from_str_radix(a, radix).with_context(|| format!("bad number in {spec:?}"))
		})
	};
	Ok(match kind {
		"synthetic" => (SourceId::Synthetic, CaptureBackend::Auto),
		"portal" => (SourceId::Portal, CaptureBackend::Auto),
		"x11" => (SourceId::Monitor(number(0)? as u32), CaptureBackend::X11),
		"screen" | "monitor" => (SourceId::Monitor(number(0)? as u32), CaptureBackend::Auto),
		"window" if arg.is_some() => (SourceId::Window(number(0)?), CaptureBackend::Auto),
		_ => bail!("unknown source {spec:?}: synthetic, x11[:N], screen[:N], window:<id>, portal"),
	})
}

/// `<width>x<height>`.
fn parse_size(size: &str) -> Result<(u32, u32)> {
	let parsed = size.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
	parsed.with_context(|| format!("bad size {size:?}, expected e.g. 1280x720"))
}

/// A bitrate in bit/s: `1500000`, `1500k`, `1.5M`.
fn parse_rate(rate: &str) -> Result<u64, String> {
	let (number, factor) = match rate.trim().char_indices().last() {
		Some((i, 'k' | 'K')) => (&rate[..i], 1e3),
		Some((i, 'm' | 'M')) => (&rate[..i], 1e6),
		_ => (rate, 1.0),
	};
	match number.trim().parse::<f64>() {
		Ok(n) if n.is_finite() && n >= 0.0 => Ok((n * factor).round() as u64),
		_ => Err(format!("bad bitrate {rate:?}, expected e.g. 1500k")),
	}
}

/// `--layer <scale or WxH>:<bitrate>[:min=..][:max=..][:fps=..][:rid=..][:id=..]`;
/// the id defaults to the position in the list (fixed up by the caller).
fn parse_layer(spec: &str) -> Result<LayerSpec, String> {
	let mut parts = spec.split(':');
	let (Some(size), Some(bitrate)) = (parts.next(), parts.next()) else {
		return Err(format!("bad layer {spec:?}, expected e.g. 0.5:1500k:min=800k"));
	};
	let bitrate = parse_rate(bitrate)?;
	let mut layer =
		LayerSpec { min_bitrate: bitrate / 2, id: LayerId::MAX, ..LayerSpec::single(bitrate) };
	match parse_size(size) {
		Ok(size) => layer.size = Some(size),
		Err(_) => {
			layer.scale = size.parse().map_err(|_| format!("bad scale or size {size:?}"))?;
			if !(layer.scale > 0.0 && layer.scale.is_finite()) {
				return Err(format!("bad scale {size:?}"));
			}
		}
	}
	for option in parts {
		let (key, value) = option.split_once('=').ok_or(format!("bad layer option {option:?}"))?;
		let number = || value.parse().map_err(|_| format!("bad number in {option:?}"));
		match key {
			"min" => layer.min_bitrate = parse_rate(value)?,
			"max" => layer.max_bitrate = Some(parse_rate(value)?),
			"fps" => layer.max_fps = Some(number()?),
			"rid" => layer.rid = Some(value.to_owned()),
			"id" => layer.id = number()? as LayerId,
			_ => return Err(format!("unknown layer option {key:?} (min, max, fps, rid, id)")),
		}
	}
	Ok(layer)
}

/// Layer ids not given are the layers' positions.
fn number_layers(layers: &mut [LayerSpec]) {
	for (i, layer) in layers.iter_mut().enumerate() {
		if layer.id == LayerId::MAX {
			layer.id = i as LayerId;
		}
	}
}

fn parse_srtp(names: &[String]) -> Result<Vec<SrtpProfile>> {
	names
		.iter()
		.map(|n| {
			SrtpProfile::from_name(n).with_context(|| {
				format!(
					"unknown SRTP profile {n:?}: {}",
					SrtpProfile::ALL.map(SrtpProfile::name).join(", ")
				)
			})
		})
		.collect()
}

/// A bitrate for people.
fn rate(bitrate: u64) -> String {
	if bitrate >= 1_000_000 {
		format!("{:.2} Mbit/s", bitrate as f64 / 1e6)
	} else {
		format!("{} kbit/s", bitrate / 1000)
	}
}

/// What the streamer sends.
enum Source {
	/// Captured and encoded.
	Encoded(EncodedSource),
	/// Placeholder frames, no encoder.
	Placeholder(SyntheticSource),
}

impl Source {
	fn frames(&mut self) -> &mut dyn FrameSource {
		match self {
			Self::Encoded(s) => s,
			Self::Placeholder(s) => s,
		}
	}
}

/// A viewer as the streamer's output lists it.
fn viewer_text(con: &Connection, v: &ViewerInfo) -> String {
	let mut text = format!("{} (clid {}, {:?}", nick(con, v.client), v.client.0, v.state);
	match v.layer {
		Some(layer) => text += &format!(", layer {layer}"),
		None if v.state == voelin_stream::ViewerState::Connected => text += ", all layers (RID)",
		None => {}
	}
	if let Some(estimate) = v.estimate {
		text += &format!(", estimate {}", rate(estimate));
	}
	if let Some(profile) = v.srtp_profile {
		text += &format!(", {profile}");
	}
	text + ")"
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
			StreamItem::BookEvents(_) => self.sync_clients(con)?,
			_ => {}
		}
		Ok(())
	}

	/// Tell the sessions the clients on the server: streams of clients that
	/// left, stopped or are not in our channel are dropped; streams that
	/// started before we came are looked up.
	fn sync_clients(&mut self, con: &Connection) -> Result<()> {
		let state = con.get_state()?;
		let clients = state
			.clients
			.values()
			.map(|c| (c.id.0, ClientState { channel: c.channel.0, streaming: c.is_streaming }))
			.collect();
		self.streams.update_clients(clients);
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
	mut source: Source,
	length: Option<Duration>,
) -> Result<()> {
	let auto_accept = options.auto_accept;
	driver.streams.start(options)?;
	let deadline = length.map(|l| Instant::now() + l);
	let mut tick = tokio::time::interval(source.frames().interval());
	tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	let mut lines = (!auto_accept).then(|| BufReader::new(tokio::io::stdin()).lines());
	let (mut live, mut video, mut audio) = (false, 0u64, 0u64);
	let mut frames = Vec::new();
	loop {
		let wake = driver.next(con, deadline, live.then_some(&mut tick), lines.as_mut()).await?;
		match wake {
			Wake::Stream => {}
			Wake::Tick => {
				source.frames().poll_frames(std::time::Instant::now(), &mut frames);
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
					let list: Vec<String> = viewers.iter().map(|v| viewer_text(con, v)).collect();
					println!("viewers: [{}]", list.join(", "));
				}
				StreamerEvent::KeyframeRequest { layer } => {
					source.frames().request_layer_keyframe(layer);
				}
				StreamerEvent::LayerBitrate { layer, bitrate } => {
					println!("layer {layer}: target {}", rate(bitrate));
					source.frames().set_layer_bitrate(layer, bitrate);
				}
				StreamerEvent::Ended(reason) => {
					println!("sent {video} video / {audio} audio frames to connected viewers");
					if let Source::Encoded(source) = &source
						&& let Some(e) = &source.streamer().stats().error
					{
						println!("last encoder error: {e}");
					}
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
			"{}  {:?} by {} (clid {}), {} kbit/s{}{}",
			s.id,
			s.name,
			nick(con, s.streamer),
			s.streamer.0,
			s.bitrate,
			if s.audio { ", audio" } else { "" },
			s.viewers.map(|n| format!(", {n} watching")).unwrap_or_default()
		);
	}
	// Streamers in our channel known only by the flag: the lookup failed or
	// did not answer in time.
	let state = con.get_state()?;
	let channel = state.clients.get(&state.own_client).map(|c| c.channel);
	let mut unannounced = 0;
	for c in state.clients.values() {
		if c.is_streaming == Some(true)
			&& Some(c.channel) == channel
			&& !streams.iter().any(|s| s.streamer == c.id)
		{
			println!("?  {} (clid {}) is streaming; id not known", c.name, c.id.0);
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
	/// The simulcast layer to ask for.
	layer: Option<u16>,
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

/// Save a picture as an RGBA PNG.
fn save_png(picture: &VideoFrame, path: &Path) -> Result<()> {
	let rgba = convert::to_rgba_vec(picture)?;
	let file = std::io::BufWriter::new(std::fs::File::create(path)?);
	let mut encoder = png::Encoder::new(file, picture.width, picture.height);
	encoder.set_color(png::ColorType::Rgba);
	encoder.set_depth(png::BitDepth::Eight);
	encoder.write_header()?.write_image_data(&rgba)?;
	Ok(())
}

async fn watch(
	con: &mut Connection,
	driver: &mut Driver,
	codecs: Arc<Codecs>,
	target: &Target,
	expect: Option<u64>,
	timeout: Option<Duration>,
	save: Option<&Path>,
) -> Result<()> {
	let deadline = timeout.map(|t| Instant::now() + t);
	let mut watching: Option<String> = None;
	let (mut video, mut audio) = (0u64, 0u64);
	let mut codec = None;
	let mut waiting_told = false;
	// Decoding runs on its own thread; the loop checks its progress.
	let latest = Arc::new(Latest::new());
	let keyframe_wanted = Arc::new(AtomicBool::new(false));
	let decoder = VideoPipeline::new(
		codecs,
		{
			let latest = latest.clone();
			move |picture| {
				latest.put(picture);
			}
		},
		{
			let wanted = keyframe_wanted.clone();
			move || wanted.store(true, Ordering::Relaxed)
		},
	);
	let mut tick = tokio::time::interval(Duration::from_millis(100));
	let mut first_picture = true;
	let mut last_picture: Option<Arc<VideoFrame>> = None;
	let summary = |video: u64, audio: u64, decoder: &VideoPipeline| {
		let stats = decoder.stats();
		println!(
			"received {video} video / {audio} audio frames, decoded {} pictures{}",
			stats.decoded,
			stats.error.map(|e| format!(" (last error: {e})")).unwrap_or_default()
		);
	};
	let save_last = |last: &Option<Arc<VideoFrame>>| -> Result<()> {
		match (save, last) {
			(Some(path), Some(picture)) => {
				save_png(picture, path)?;
				println!("saved {}", path.display());
				Ok(())
			}
			(Some(_), None) => bail!("no picture to save"),
			(None, _) => Ok(()),
		}
	};
	loop {
		if let Some(picture) = latest.take() {
			if first_picture {
				println!("first picture decoded ({}x{})", picture.width, picture.height);
				first_picture = false;
			}
			last_picture = Some(picture);
		}
		if let Some(id) = &watching
			&& keyframe_wanted.swap(false, Ordering::Relaxed)
		{
			driver.streams.request_keyframe(id);
		}
		if expect.is_some_and(|n| decoder.stats().decoded >= n) {
			summary(video, audio, &decoder);
			return save_last(&last_picture);
		}
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
				WatchEvent::Layers(layers) => {
					println!(
						"layers offered: {:?}",
						layers.iter().map(|l| l.id).collect::<Vec<_>>()
					);
					if let Some(layer) = target.layer {
						driver.streams.set_watch_layer(&id, Some(layer))?;
						println!("asked for layer {layer}");
					}
				}
				WatchEvent::Connected => println!("connected"),
				WatchEvent::Frame(f) => {
					match f.kind {
						MediaKind::Video => video += 1,
						MediaKind::Audio => audio += 1,
					}
					if f.kind == MediaKind::Video && codec.replace(f.codec).is_none() {
						println!("first video frame ({:?}, {} bytes)", f.codec, f.data.len());
					}
					decoder.push(f);
				}
				WatchEvent::Ended(reason) => {
					summary(video, audio, &decoder);
					match expect {
						Some(n) => bail!("{} before {n} pictures were decoded", end_text(&reason)),
						None => {
							println!("{}", end_text(&reason));
							return save_last(&last_picture.or_else(|| latest.take()));
						}
					}
				}
			}
		}
		match driver.next(con, deadline, Some(&mut tick), None).await? {
			Wake::Deadline | Wake::Interrupted => {
				summary(video, audio, &decoder);
				return match expect {
					Some(n) => bail!("timed out waiting for {n} decoded pictures"),
					None => save_last(&last_picture.or_else(|| latest.take())),
				};
			}
			_ => {}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn layers() {
		let mut layers = vec![
			parse_layer("1.0:6000k").unwrap(),
			parse_layer("0.5:1.5M:min=800k:max=2M:fps=15:rid=h:id=7").unwrap(),
			parse_layer("640x360:300000:min=0").unwrap(),
		];
		number_layers(&mut layers);
		let [full, half, small] = &layers[..] else { unreachable!() };
		assert_eq!(
			(full.id, full.scale, full.bitrate, full.min_bitrate),
			(0, 1.0, 6_000_000, 3_000_000)
		);
		assert_eq!(
			(half.id, half.scale, half.bitrate, half.min_bitrate),
			(7, 0.5, 1_500_000, 800_000)
		);
		assert_eq!(
			(half.max_bitrate, half.max_fps, half.rid.as_deref()),
			(Some(2_000_000), Some(15), Some("h"))
		);
		assert_eq!((small.id, small.size, small.min_bitrate), (2, Some((640, 360)), 0));
		for bad in ["1.0", "x:1k", "0:1k", "1.0:fast", "1.0:1k:min", "1.0:1k:color=red"] {
			assert!(parse_layer(bad).is_err(), "{bad}");
		}
	}

	#[test]
	fn srtp_names() {
		let names = ["aes_cm_128_hmac_sha1_80".to_owned(), "AEAD_AES_256_GCM".to_owned()];
		assert_eq!(
			parse_srtp(&names).unwrap(),
			[SrtpProfile::Aes128CmSha1_80, SrtpProfile::AeadAes256Gcm]
		);
		assert!(parse_srtp(&["rot13".to_owned()]).is_err());
		assert_eq!(rate(1_500_000), "1.50 Mbit/s");
		assert_eq!(rate(300_000), "300 kbit/s");
	}
}
