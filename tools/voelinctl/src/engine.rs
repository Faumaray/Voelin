//! `voelinctl gateway --engine`: one engine session (voelin-core) with the
//! gateway (and optionally voice), for chat history, the gateway's features
//! and the stream directory as the app uses them.
//!
//! Prints one line per event: `history <source> <target> ...` per message
//! of a history batch (then `history <source> <target> batch n=.. complete=..`),
//! `update <json>` per gateway update, `chat ...` for live messages,
//! `state ...`, `streams ...`, `stream ...`, `watch ...`, `error: ...`.
//! `--expect TEXT` succeeds on the first line containing TEXT.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant as TokioInstant, sleep_until, timeout_at};
use voelin_core::gateway::{GatewayRequest, GatewayUpdate};
use voelin_core::history::HistoryMessage;
use voelin_core::settings::Settings;
use voelin_core::stream::{FrameSource, MediaKind, PeerConfig, StreamSetup, SyntheticSource};
use voelin_core::{
	Command, Engine, Event, History, HistorySource, ObserveState, StreamState, VoiceOptions,
	VoiceState, WatchState,
};
use voelin_model::ChatTarget;

use crate::gateway::{GatewayArgs, parse_target, target_name};

const SESSION: u64 = 1;

fn message_line(source: HistorySource, target: &ChatTarget, m: &HistoryMessage) -> String {
	let reactions: Vec<String> = m
		.reactions
		.iter()
		.map(|r| format!("{}{}{}", r.emoji, r.count, if r.me { "*" } else { "" }))
		.collect();
	format!(
		"history {} {} id={} remote={} topic={} pinned={} reactions=[{}] {}: {}",
		source_name(source),
		target_name(target),
		m.id,
		m.remote_id.map_or("-".into(), |r| r.to_string()),
		m.topic_id.map_or("-".into(), |t| t.to_string()),
		m.pinned,
		reactions.join(","),
		m.message.author_name,
		m.message.text
	)
}

fn source_name(source: HistorySource) -> &'static str {
	match source {
		HistorySource::Local => "local",
		HistorySource::Gateway => "gateway",
		HistorySource::Live => "live",
	}
}

/// What the run is waiting for.
struct Run {
	expect: Option<String>,
	/// Lines printed so far that matched.
	done: bool,
}

impl Run {
	fn print(&mut self, line: String) {
		if self.expect.as_ref().is_some_and(|e| line.contains(e.as_str())) {
			self.done = true;
		}
		println!("{line}");
	}
}

pub async fn run(args: GatewayArgs) -> Result<()> {
	let identity = crate::identity::load(&args.identity)?;
	let settings = Settings::in_memory();
	if let Some(e) = settings.apply_overrides(args.set.iter().map(String::as_str)).first() {
		bail!("--set: {e}");
	}
	let history = match &args.db {
		Some(path) => History::open(path).with_context(|| format!("history {}", path.display()))?,
		None => History::in_memory(),
	};
	let engine = Engine::start_with(settings, history);
	let mut events = engine.subscribe();
	let mut frames = engine.subscribe_frames();
	let deadline = TokioInstant::now() + Duration::from_secs(args.seconds);
	let mut out = Run { expect: args.expect.clone(), done: false };

	let chats: Vec<ChatTarget> =
		args.chat.iter().map(|t| parse_target(t)).collect::<Result<_>>()?;
	for target in &chats {
		engine.send(Command::OpenChat { session: SESSION, target: target.clone() });
	}
	let roundtrip = args.roundtrip.as_deref().map(parse_target).transpose()?;
	if let Some(target) = &roundtrip {
		engine.send(Command::OpenChat { session: SESSION, target: target.clone() });
	}
	engine.send(Command::ObserveGateway {
		session: SESSION,
		urls: vec![args.url.clone()],
		identity: Box::new(identity.clone()),
	});
	if let Some(addr) = &args.voice {
		let mut options = VoiceOptions::new(addr, &args.nick);
		options.identity = Some(identity);
		if args.loopback {
			options.stream_peer = PeerConfig::loopback();
		}
		engine.send(Command::ConnectVoice { session: SESSION, options: Box::new(options) });
	}
	let requests: Vec<GatewayRequest> = args
		.request
		.iter()
		.map(|r| serde_json::from_str(r).with_context(|| format!("--request {r}")))
		.collect::<Result<_>>()?;

	let mut gateway_up = false;
	let mut voice_up = false;
	let mut older_left: HashMap<ChatTarget, u32> =
		chats.iter().map(|t| (t.clone(), args.older)).collect();
	// The oldest message shown per chat: (ts_ms, id).
	let mut oldest: HashMap<ChatTarget, (i64, i64)> = HashMap::new();
	let mut rt = roundtrip.map(Roundtrip::new);
	let mut streaming: Option<(Instant, tokio::task::JoinHandle<()>)> = None;
	let mut stream_started = false;
	let mut watching = false;
	let mut nicknames: HashMap<u16, String> = HashMap::new();
	let mut video_frames = 0u64;

	loop {
		if out.done && rt.as_ref().is_none_or(|r| r.finished) {
			return Ok(());
		}
		if let Some(r) = &rt
			&& let Some(e) = &r.failed
		{
			bail!("roundtrip failed: {e}");
		}
		// Our stream's time is up.
		if let Some((until, _)) = &streaming
			&& Instant::now() >= *until
		{
			let (_, feeder) = streaming.take().unwrap();
			feeder.abort();
			engine.send(Command::StopStream { session: SESSION });
		}
		let wake = streaming
			.as_ref()
			.map(|(until, _)| TokioInstant::from_std(*until))
			.unwrap_or(deadline)
			.min(deadline);
		let event = tokio::select! {
			e = timeout_at(wake, events.recv()) => match e {
				Ok(Ok(e)) => e,
				Ok(Err(RecvError::Lagged(_))) => continue,
				Ok(Err(RecvError::Closed)) => bail!("engine stopped"),
				Err(_) if TokioInstant::now() >= deadline => break,
				Err(_) => continue,
			},
			f = frames.recv() => {
				if let Ok(f) = f && f.frame.kind == MediaKind::Video {
					video_frames += 1;
					if video_frames == 1 {
						out.print(format!("watch {} first video frame", f.stream_id));
					}
					if args.expect_frames.is_some_and(|n| video_frames >= n) {
						out.print(format!("received {video_frames} video frames"));
						return Ok(());
					}
				}
				continue;
			}
		};
		match event {
			Event::State { session: SESSION, state } => {
				out.print(format!(
					"state voice={:?} observe={:?} server_uid={}",
					state.voice,
					state.observe,
					state.server_uid.as_deref().unwrap_or("-")
				));
				voice_up = state.voice == VoiceState::Connected && state.own_channel.is_some();
				if state.observe == ObserveState::Off && gateway_up {
					gateway_up = false;
				}
			}
			Event::Gateway { session: SESSION, update } => {
				out.print(format!("update {}", serde_json::to_string(&update)?));
				if let GatewayUpdate::Connected { .. } = &update {
					gateway_up = true;
					for request in &requests {
						engine
							.send(Command::Gateway { session: SESSION, request: request.clone() });
					}
				}
				if let Some(r) = &mut rt {
					r.update(&engine, &update);
				}
			}
			Event::ChatHistory { session: SESSION, target, messages, source, complete } => {
				for m in &messages {
					out.print(message_line(source, &target, m));
				}
				out.print(format!(
					"history {} {} batch n={} complete={complete}",
					source_name(source),
					target_name(&target),
					messages.len()
				));
				if source != HistorySource::Live {
					for m in &messages {
						let at = (m.message.ts_ms, m.id);
						let o = oldest.entry(target.clone()).or_insert(at);
						*o = (*o).min(at);
					}
				}
				if let Some(r) = &mut rt {
					r.history(&engine, &messages);
				}
				// Page back after each batch from the gateway.
				let left = older_left.get(&target).copied().unwrap_or(0);
				if left > 0 && source == HistorySource::Gateway && !complete {
					older_left.insert(target.clone(), left - 1);
					let before = oldest.get(&target).map(|(_, id)| *id);
					engine.send(Command::LoadOlderHistory { session: SESSION, target, before });
				}
			}
			Event::Chat { session: SESSION, message } => out.print(format!(
				"chat {} {}: {}",
				target_name(&message.target),
				message.author_name,
				message.text
			)),
			Event::Presence { session: SESSION, presence } => {
				nicknames = presence.clients.values().map(|c| (c.id, c.nickname.clone())).collect();
			}
			Event::ServerInfo { session: SESSION, name, flavor, .. } => {
				out.print(format!("server {name:?} {flavor:?}"));
			}
			Event::StreamsChanged { session: SESSION, streams } => {
				for s in &streams {
					let nick = nicknames.get(&s.streamer.0).map_or("?", String::as_str);
					out.print(format!(
						"streams: {} by {nick} ({}) {:?}",
						s.id, s.streamer.0, s.name
					));
				}
				if let Some(wanted) = &args.watch_streamer
					&& !watching && let Some(s) = streams
					.iter()
					.find(|s| nicknames.get(&s.streamer.0).is_some_and(|n| n == wanted))
				{
					watching = true;
					out.print(format!("watching {}", s.id));
					engine.send(Command::WatchStream { session: SESSION, stream_id: s.id.clone() });
				}
			}
			Event::StreamState { session: SESSION, state } => match state {
				StreamState::Starting => out.print("stream starting".into()),
				StreamState::Live { id, sink } => {
					out.print(format!("stream live {id}"));
					let seconds = args.stream_seconds.unwrap_or(0);
					let feeder = tokio::spawn(async move {
						let mut source = SyntheticSource::new(30, 2000, true);
						let mut buf = Vec::new();
						while sink.is_live() {
							source.poll_frames(Instant::now(), &mut buf);
							for f in buf.drain(..) {
								sink.send(f);
							}
							tokio::time::sleep(Duration::from_millis(10)).await;
						}
					});
					streaming = Some((Instant::now() + Duration::from_secs(seconds), feeder));
				}
				StreamState::Ended(reason) => {
					out.print(format!("stream ended {reason:?}"));
					if args.stream_seconds.is_some() && streaming.is_none() {
						// Give the directory update a moment to go out.
						sleep_until(TokioInstant::now() + Duration::from_millis(500)).await;
						return Ok(());
					}
				}
			},
			Event::WatchState { session: SESSION, stream_id, state } => {
				out.print(format!("watch {stream_id} {state:?}"));
				if let WatchState::Ended(reason) = state
					&& args.expect_frames.is_some()
				{
					bail!("the stream ended before enough frames arrived: {reason:?}");
				}
			}
			Event::Error { session: SESSION, message } => out.print(format!("error: {message}")),
			_ => {}
		}
		// Stream once voice (and the gateway, if any) are up.
		if args.stream_seconds.is_some() && !stream_started && voice_up && gateway_up {
			stream_started = true;
			let setup = StreamSetup { name: args.stream_title.clone(), ..Default::default() };
			engine.send(Command::StartStream { session: SESSION, setup, auto_accept: true });
		}
	}
	if let Some(r) = &rt
		&& !r.finished
	{
		bail!("roundtrip did not finish (step {:?})", r.step);
	}
	if args.expect.is_some() && !out.done {
		bail!("expected output did not appear");
	}
	if args.expect_frames.is_some() {
		bail!("received only {video_frames} video frames");
	}
	Ok(())
}

/// The steps of `--roundtrip`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
	Post,
	Pin,
	React,
	Topic,
	TopicPost,
	TopicHistory,
	Pins,
	Done,
}

/// Post, pin, react, start a topic from the message, post into it, read
/// the topic and the pins; each answer is checked.
struct Roundtrip {
	target: ChatTarget,
	tag: String,
	step: Step,
	/// Gateway and local id of our post.
	remote: i64,
	local: i64,
	topic: i64,
	pinned_row: bool,
	pinned_push: bool,
	reacted_row: bool,
	started: bool,
	finished: bool,
	failed: Option<String>,
}

impl Roundtrip {
	fn new(target: ChatTarget) -> Self {
		let tag = format!("{}", std::process::id() % 100_000);
		Self {
			target,
			tag,
			step: Step::Post,
			remote: 0,
			local: 0,
			topic: 0,
			pinned_row: false,
			pinned_push: false,
			reacted_row: false,
			started: false,
			finished: false,
			failed: None,
		}
	}

	fn send(&self, engine: &Engine, request: GatewayRequest) {
		engine.send(Command::Gateway { session: SESSION, request });
	}

	fn next(&mut self, engine: &Engine, step: Step) {
		self.step = step;
		println!("roundtrip: {step:?}");
		let target = self.target.clone();
		match step {
			Step::Post => self.send(
				engine,
				GatewayRequest::Post { target, text: format!("rt-{}", self.tag), topic: None },
			),
			Step::Pin => self.send(engine, GatewayRequest::Pin { message_id: self.remote }),
			Step::React => self.send(
				engine,
				GatewayRequest::React { message_id: self.remote, emoji: "👍".into() },
			),
			Step::Topic => self.send(
				engine,
				GatewayRequest::CreateTopic {
					target,
					title: format!("rt topic {}", self.tag),
					message_id: Some(self.remote),
				},
			),
			Step::TopicPost => self.send(
				engine,
				GatewayRequest::Post {
					target,
					text: format!("rt-in-topic-{}", self.tag),
					topic: Some(self.topic),
				},
			),
			Step::TopicHistory => self.send(
				engine,
				GatewayRequest::TopicHistory {
					target,
					topic: self.topic,
					before: None,
					limit: None,
				},
			),
			Step::Pins => self.send(engine, GatewayRequest::Pins { target }),
			Step::Done => {
				self.finished = true;
				println!("roundtrip ok");
			}
		}
	}

	fn update(&mut self, engine: &Engine, update: &GatewayUpdate) {
		match update {
			GatewayUpdate::Connected { .. } if !self.started => {
				self.started = true;
				self.next(engine, Step::Post);
			}
			GatewayUpdate::Failed { request, message, .. } => {
				self.failed = Some(format!("{request}: {message}"));
			}
			GatewayUpdate::Posted { message } if self.step == Step::Post => {
				self.remote = message.remote_id.unwrap_or_default();
				self.local = message.id;
				self.next(engine, Step::Pin);
			}
			GatewayUpdate::Pinned { pin, .. } if self.step == Step::Pin => {
				self.pinned_push = pin.message.id == self.local;
				self.after_pin(engine);
			}
			GatewayUpdate::Reaction { message_id, added: true, .. }
				if self.step == Step::React && *message_id == self.remote =>
			{
				if self.reacted_row {
					self.next(engine, Step::Topic);
				}
			}
			GatewayUpdate::Topic { topic } if self.step == Step::Topic => {
				if topic.root_message_id != Some(self.remote) {
					self.failed = Some(format!("topic from another message: {topic:?}"));
					return;
				}
				self.topic = topic.id;
				self.next(engine, Step::TopicPost);
			}
			GatewayUpdate::Posted { message } if self.step == Step::TopicPost => {
				if message.topic_id != Some(self.topic) {
					self.failed = Some(format!("posted outside the topic: {message:?}"));
					return;
				}
				self.next(engine, Step::TopicHistory);
			}
			GatewayUpdate::TopicHistory { messages, .. } if self.step == Step::TopicHistory => {
				let text = format!("rt-in-topic-{}", self.tag);
				if !messages.iter().any(|m| m.message.text == text) {
					self.failed = Some(format!("topic history without {text}"));
					return;
				}
				self.next(engine, Step::Pins);
			}
			GatewayUpdate::Pins { pins, .. } if self.step == Step::Pins => {
				if !pins.iter().any(|p| p.message.id == self.local) {
					self.failed = Some("our pin is not listed".into());
					return;
				}
				self.next(engine, Step::Done);
			}
			_ => {}
		}
	}

	fn after_pin(&mut self, engine: &Engine) {
		if self.pinned_row && self.pinned_push {
			self.next(engine, Step::React);
		}
	}

	fn history(&mut self, engine: &Engine, messages: &[HistoryMessage]) {
		let Some(ours) = messages.iter().find(|m| m.id == self.local && self.local != 0) else {
			return;
		};
		match self.step {
			Step::Pin if ours.pinned => {
				self.pinned_row = true;
				self.after_pin(engine);
			}
			Step::React if ours.reactions.iter().any(|r| r.emoji == "👍" && r.me) => {
				self.reacted_row = true;
			}
			_ => {}
		}
	}
}
