//! The Stream Studio: scenes of sources composited into one picture that the
//! streamer encodes, plus outputs that keep or forward the encoded packets.
//!
//! - [`scene`]: the scene graph (scenes, sources, transforms, crops), the
//!   persisted shape of a studio
//! - [`compose`]: the compositor, which draws the live scene into pooled
//!   frames at the output size
//! - [`source`]: the live input behind each of a scene's sources (colour,
//!   text, image, screen, window, camera)
//! - [`camera`]: listing and capturing cameras
//! - [`segment`]: person segmentation and background blur
//! - [`output`]: what the encoded packets go to besides the stream
//!   (recording, the replay buffer, WHIP)
//!
//! [`Studio`] ties them together and is what a UI drives: one
//! [`Command`] per thing a person can do, a stream of [`Event`]s back, and a
//! preview tap ([`Studio::preview`]) the UI reads at its own size and rate.
//!
//! # How it runs
//!
//! Each source runs on its own thread and publishes into a
//! [`compose::Feed`]. One compose thread ticks at the output frame rate,
//! draws the live scene into a pooled frame, offers it to the preview tap and
//! hands it to whatever [`crate::capture::FrameSink`] is attached — the
//! streamer's, through [`StudioCapture`], which is an ordinary
//! [`crate::ScreenCapture`] backend. So the studio needs nothing new of the
//! encoders, of simulcast or of the peers: it is just another screen source.
//!
//! Encoded packets come back the other way through [`Studio::write_packet`],
//! which tees them to every [`output::OutputSink`]. Recording and the replay
//! buffer therefore work with no stream live at all.
//!
//! Everything is changeable while running, and nothing is capped: any number
//! of scenes, sources and outputs, and a replay window limited only by the
//! setting.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::capture::{
	BoxFuture, CaptureOptions, CaptureSource, FrameSink, ScreenCapture, SourceId,
};
use crate::codec::Codec;
use crate::frame::VideoFrame;
use crate::handoff::Handoff;
use crate::studio::compose::{ComposeStats, Compositor, Feed, RgbaScaler};
use crate::studio::output::replay::{ReplayBuffer, ReplayStats};
use crate::studio::output::{OutputSink, Packet, Track};
use crate::studio::scene::{Background, Crop, Scene, Scenes, Source, SourceKind, Transform};
use crate::studio::source::Input;
use crate::{Error, Result};

pub mod camera;
pub mod compose;
pub mod output;
pub mod scene;
pub mod segment;
pub mod source;

/// Events buffered per subscriber before the oldest is dropped.
const EVENTS: usize = 256;
/// How long the compose thread waits before checking whether to stop.
const POLL: Duration = Duration::from_millis(100);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a source's transform, crop, opacity or background should become;
/// `None` leaves it alone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceChange {
	pub name: Option<String>,
	pub kind: Option<SourceKind>,
	pub transform: Option<Transform>,
	pub crop: Option<Crop>,
	pub opacity: Option<f32>,
	pub visible: Option<bool>,
	pub locked: Option<bool>,
	pub background: Option<Background>,
}

/// An output to add ([`Command::AddOutput`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputSpec {
	/// Record to a file; the name decides WebM or Matroska.
	Record { path: PathBuf },
	/// WHIP (`http(s)://`, `token`: the bearer token) or RTMP
	/// (`rtmp(s)://`, `token`: the stream key, unless it ends the URL).
	Url { url: String, token: Option<String> },
}

/// What a UI asks the studio to do. Everything takes effect from the next
/// composed frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
	/// Replace the whole scene graph (loading a scene file, or the
	/// `studio.scenes` setting).
	SetScenes(Box<Scenes>),
	/// A new, empty scene; the reply event carries its id.
	AddScene {
		name: String,
	},
	RemoveScene {
		scene: u64,
	},
	RenameScene {
		scene: u64,
		name: String,
	},
	/// Switch what is composited, live.
	SetActiveScene {
		scene: u64,
	},
	/// Output size and frame rate of the composite.
	SetOutput {
		width: u32,
		height: u32,
		fps: u32,
	},

	AddSource {
		scene: u64,
		name: String,
		kind: SourceKind,
	},
	RemoveSource {
		scene: u64,
		source: u64,
	},
	UpdateSource {
		scene: u64,
		source: u64,
		change: Box<SourceChange>,
	},
	/// Move a source to position `to` in the scene's order (0 is furthest
	/// back).
	ReorderSource {
		scene: u64,
		source: u64,
		to: usize,
	},

	/// Size and rate of the downscaled preview the UI reads.
	SetPreview {
		width: u32,
		height: u32,
		fps: u32,
	},

	StartRecording {
		path: PathBuf,
	},
	StopRecording,
	/// Window and memory limit of the replay buffer; 0 seconds turns it off.
	SetReplay {
		seconds: u32,
		memory_mb: u64,
	},
	/// Write what the replay buffer holds.
	SaveClip {
		path: PathBuf,
	},

	AddOutput(OutputSpec),
	RemoveOutput {
		id: u64,
	},

	/// Report [`State::Live`] (the UI's LIVE and timer). Outputs that push
	/// (WHIP) push from when they are added; the TeamSpeak stream itself is
	/// started by the engine around the studio.
	GoLive,
	/// Back to [`State::Idle`], closing every output that pushes. Recording
	/// and the replay buffer keep running.
	EndStream,
}

/// Where the studio is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
	/// Composing, nothing pushed out.
	#[default]
	Idle,
	Live,
}

/// The studio's own idea of what it is doing, for the UI's header (LIVE, the
/// timer, REC).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
	pub state: State,
	/// How long it has been live.
	pub live_for: Duration,
	pub recording: Option<PathBuf>,
	pub recorded_for: Duration,
	pub active_scene: u64,
	pub output: (u32, u32),
	pub fps: u32,
	/// Outputs that are pushing (WHIP; not the recording).
	pub outputs: usize,
}

/// One source in [`Stats`]. Rates are over the last second.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceStats {
	pub id: u64,
	pub label: String,
	pub kind: &'static str,
	pub width: u32,
	pub height: u32,
	pub frames: u64,
	pub fps: f64,
	/// Frames the source produced that the composite never used.
	pub dropped: u64,
	pub error: Option<String>,
}

/// One output in [`Stats`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputStats {
	pub id: u64,
	pub name: String,
	pub bytes: u64,
	pub kbps: f64,
	pub error: Option<String>,
}

/// What the studio has done. Rates are over the last second.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
	pub status: Status,
	pub compose: ComposeStats,
	/// Composed frames a second.
	pub fps: f64,
	pub sources: Vec<SourceStats>,
	pub outputs: Vec<OutputStats>,
	pub replay: ReplayStats,
	/// Frames the preview tap produced.
	pub preview_frames: u64,
	/// Frames handed to the streamer.
	pub delivered: u64,
}

/// What the studio tells its UI.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
	/// Anything in [`Status`] changed.
	State(Status),
	/// Once a second.
	Stats(Box<Stats>),
	/// The scene graph changed and should be persisted (`studio.scenes`).
	Scenes(Arc<Scenes>),
	/// A scene or source was created; its id, so a UI can select it.
	SceneAdded {
		scene: u64,
	},
	SourceAdded {
		scene: u64,
		source: u64,
	},
	RecordingStarted {
		path: PathBuf,
	},
	RecordingStopped {
		path: PathBuf,
		duration: Duration,
		bytes: u64,
	},
	ClipSaved {
		path: PathBuf,
		duration: Duration,
	},
	OutputAdded {
		id: u64,
		name: String,
	},
	OutputRemoved {
		id: u64,
		name: String,
	},
	/// Something went wrong; the studio keeps going.
	Error {
		context: String,
		message: String,
	},
}

/// One output, with the id the UI removes it by.
struct Output {
	id: u64,
	sink: Box<dyn OutputSink>,
	/// A recording's file (a sink only knows its own short name).
	path: Option<PathBuf>,
	started: Instant,
	/// Rate over the last second.
	kbps: f64,
	last_bytes: u64,
	/// Pushes somewhere (counts towards [`Status::outputs`]).
	pushes: bool,
	error: Option<String>,
}

/// The scene the compose thread draws, published whenever anything changes.
struct Live {
	scene: Scene,
	/// One per `scene.sources`, in order.
	feeds: Vec<Option<Arc<Feed>>>,
	revision: u64,
	size: (u32, u32),
	fps: u32,
}

impl Default for Live {
	fn default() -> Self {
		Self {
			scene: Scene::new(0, ""),
			feeds: Vec::new(),
			revision: 0,
			size: (1920, 1080),
			fps: 30,
		}
	}
}

/// State the threads share.
struct Shared {
	live: Mutex<Arc<Live>>,
	/// The streamer's sink, while it is capturing us.
	sink: Mutex<Option<Box<dyn FrameSink>>>,
	preview: Arc<Handoff<VideoFrame>>,
	preview_size: Mutex<(u32, u32)>,
	preview_fps: AtomicU32,
	preview_frames: AtomicU64,
	delivered: AtomicU64,
	stop: AtomicBool,
	/// What the composite's timestamps count from.
	epoch: Instant,
	events: broadcast::Sender<Event>,
	/// The outputs, and the replay buffer which is always there (off when its
	/// window is zero).
	outputs: Mutex<Vec<Output>>,
	replay: Mutex<ReplayBuffer>,
	/// Which layer the file outputs take.
	layer: AtomicU32,
	/// The codec of the last packet, which a WHIP output must offer.
	codec: Mutex<Option<Codec>>,
	compose: Mutex<ComposeStats>,
	recording: Mutex<Option<u64>>,
	live_since: Mutex<Option<Instant>>,
	state: Mutex<State>,
}

impl Shared {
	fn stopped(&self) -> bool {
		self.stop.load(Ordering::Relaxed)
	}

	fn send(&self, event: Event) {
		let _ = self.events.send(event);
	}

	fn fail(&self, context: impl Into<String>, message: impl std::fmt::Display) {
		let (context, message) = (context.into(), message.to_string());
		warn!(context, "studio: {message}");
		self.send(Event::Error { context, message });
	}
}

/// The studio engine; see the [module docs](self). Stops its threads when
/// dropped.
pub struct Studio {
	shared: Arc<Shared>,
	/// The scene graph and the inputs behind it, which only the controller
	/// touches.
	graph: Mutex<Graph>,
	threads: Vec<JoinHandle<()>>,
}

/// The controller's own state.
struct Graph {
	scenes: Scenes,
	/// The input of every source of the active scene, by source id.
	inputs: Vec<(u64, Input)>,
	revision: u64,
	next_output: u64,
	preview: (u32, u32, u32),
}

impl Studio {
	/// Start a studio for `scenes`, composing at once so a preview has
	/// something to show before anything is live.
	///
	/// Must run on a Tokio runtime: the portal and camera sources talk D-Bus.
	pub async fn start(scenes: Scenes) -> Result<Self> {
		scenes.check().map_err(Error::InvalidFrame)?;
		let (events, _) = broadcast::channel(EVENTS);
		let shared = Arc::new(Shared {
			live: Mutex::new(Arc::new(Live {
				size: scenes.size(),
				fps: scenes.fps(),
				..Live::default()
			})),
			sink: Mutex::new(None),
			preview: Arc::new(Handoff::new()),
			preview_size: Mutex::new((480, 270)),
			preview_fps: AtomicU32::new(15),
			preview_frames: AtomicU64::new(0),
			delivered: AtomicU64::new(0),
			stop: AtomicBool::new(false),
			epoch: Instant::now(),
			events,
			outputs: Mutex::new(Vec::new()),
			replay: Mutex::new(ReplayBuffer::new(Duration::ZERO, 512, 0, 2)),
			layer: AtomicU32::new(0),
			codec: Mutex::new(None),
			compose: Mutex::new(ComposeStats::default()),
			recording: Mutex::new(None),
			live_since: Mutex::new(None),
			state: Mutex::new(State::Idle),
		});
		let studio = Self {
			shared: shared.clone(),
			graph: Mutex::new(Graph {
				scenes,
				inputs: Vec::new(),
				revision: 0,
				next_output: 1,
				preview: (480, 270, 15),
			}),
			threads: Vec::new(),
		};
		studio.rebuild().await;
		let mut studio = studio;
		let compose = shared.clone();
		studio.threads.push(
			std::thread::Builder::new()
				.name("voelin-studio-compose".into())
				.spawn(move || compose_loop(&compose))
				.map_err(Error::Io)?,
		);
		let stats = shared.clone();
		studio.threads.push(
			std::thread::Builder::new()
				.name("voelin-studio-stats".into())
				.spawn(move || stats_loop(&stats))
				.map_err(Error::Io)?,
		);
		Ok(studio)
	}

	/// Events for a UI. Late subscribers miss what happened before.
	pub fn events(&self) -> broadcast::Receiver<Event> {
		self.shared.events.subscribe()
	}

	/// The preview tap: the newest downscaled RGBA frame, latest-wins. The
	/// UI takes what it can and never blocks the composite.
	pub fn preview(&self) -> Arc<Handoff<VideoFrame>> {
		self.shared.preview.clone()
	}

	/// The instant the composite's timestamps (and so the encoded video's)
	/// count from. Audio meant for the same outputs must use this clock too.
	pub fn epoch(&self) -> Instant {
		self.shared.epoch
	}

	/// The scene graph as it stands, to persist or to show.
	pub fn scenes(&self) -> Scenes {
		lock(&self.graph).scenes.clone()
	}

	pub fn status(&self) -> Status {
		self.shared.status()
	}

	/// The cameras that can be used as sources.
	pub fn cameras(&self) -> Vec<camera::Camera> {
		camera::list()
	}

	/// Which simulcast layer the outputs and the replay buffer take (the
	/// streamer's layer ids), from the next output started. Default 0.
	pub fn set_layer(&self, layer: u32) {
		self.shared.layer.store(layer, Ordering::Relaxed);
		lock(&self.shared.replay).set_layer(layer);
	}

	/// A [`ScreenCapture`] backend that hands the composite to the streamer.
	pub fn capture(self: &Arc<Self>) -> StudioCapture {
		StudioCapture { shared: self.shared.clone() }
	}

	/// Do what `command` says.
	pub async fn apply(&self, command: Command) -> Result<()> {
		match command {
			Command::SetScenes(scenes) => {
				scenes.check().map_err(Error::InvalidFrame)?;
				lock(&self.graph).scenes = *scenes;
			}
			Command::AddScene { name } => {
				let mut graph = lock(&self.graph);
				let id = graph.scenes.next_scene_id();
				graph.scenes.scenes.push(Scene::new(id, name));
				if graph.scenes.scenes.len() == 1 {
					graph.scenes.active = id;
				}
				drop(graph);
				self.shared.send(Event::SceneAdded { scene: id });
			}
			Command::RemoveScene { scene } => {
				let mut graph = lock(&self.graph);
				graph.scenes.scenes.retain(|s| s.id != scene);
				if graph.scenes.active == scene {
					graph.scenes.active = graph.scenes.scenes.first().map_or(0, |s| s.id);
				}
			}
			Command::RenameScene { scene, name } => {
				let mut graph = lock(&self.graph);
				graph.scenes.scene_mut(scene).ok_or_else(|| no_scene(scene))?.name = name;
			}
			Command::SetActiveScene { scene } => {
				let mut graph = lock(&self.graph);
				if graph.scenes.scene(scene).is_none() {
					return Err(no_scene(scene));
				}
				graph.scenes.active = scene;
			}
			Command::SetOutput { width, height, fps } => {
				let mut graph = lock(&self.graph);
				graph.scenes.width = width;
				graph.scenes.height = height;
				graph.scenes.fps = fps;
				graph.scenes.check().map_err(Error::InvalidFrame)?;
			}
			Command::AddSource { scene, name, kind } => {
				let mut graph = lock(&self.graph);
				let target = graph.scenes.scene_mut(scene).ok_or_else(|| no_scene(scene))?;
				let id = target.next_source_id();
				target.sources.push(Source { name, ..Source::new(id, kind) });
				drop(graph);
				self.shared.send(Event::SourceAdded { scene, source: id });
			}
			Command::RemoveSource { scene, source } => {
				let mut graph = lock(&self.graph);
				let target = graph.scenes.scene_mut(scene).ok_or_else(|| no_scene(scene))?;
				target.sources.retain(|s| s.id != source);
			}
			Command::UpdateSource { scene, source, change } => {
				let mut graph = lock(&self.graph);
				let Some(target) = graph
					.scenes
					.scene_mut(scene)
					.ok_or_else(|| no_scene(scene))?
					.source_mut(source)
				else {
					return Err(Error::SourceNotFound(SourceId::Window(source)));
				};
				let change = *change;
				if let Some(name) = change.name {
					target.name = name;
				}
				if let Some(kind) = change.kind {
					target.kind = kind;
				}
				if let Some(transform) = change.transform {
					target.transform = transform;
				}
				if let Some(crop) = change.crop {
					target.crop = crop;
				}
				if let Some(opacity) = change.opacity {
					target.opacity = opacity.clamp(0.0, 1.0);
				}
				if let Some(visible) = change.visible {
					target.visible = visible;
				}
				if let Some(locked) = change.locked {
					target.locked = locked;
				}
				if let Some(background) = change.background {
					target.background = background;
				}
				graph.scenes.check().map_err(Error::InvalidFrame)?;
			}
			Command::ReorderSource { scene, source, to } => {
				let mut graph = lock(&self.graph);
				let target = graph.scenes.scene_mut(scene).ok_or_else(|| no_scene(scene))?;
				let Some(from) = target.sources.iter().position(|s| s.id == source) else {
					return Err(Error::SourceNotFound(SourceId::Window(source)));
				};
				let moved = target.sources.remove(from);
				let to = to.min(target.sources.len());
				target.sources.insert(to, moved);
			}
			Command::SetPreview { width, height, fps } => {
				let mut graph = lock(&self.graph);
				graph.preview = (width.max(2), height.max(2), fps.max(1));
				let preview = graph.preview;
				drop(graph);
				*lock(&self.shared.preview_size) = (preview.0, preview.1);
				self.shared.preview_fps.store(preview.2, Ordering::Relaxed);
				return Ok(());
			}
			Command::StartRecording { path } => return self.start_recording(&path),
			Command::StopRecording => return self.stop_recording(),
			Command::SetReplay { seconds, memory_mb } => {
				let mut replay = lock(&self.shared.replay);
				replay.set_length(Duration::from_secs(u64::from(seconds)));
				replay.set_memory_mb(memory_mb);
				drop(replay);
				self.shared.state_changed();
				return Ok(());
			}
			Command::SaveClip { path } => {
				let length = lock(&self.shared.replay).save_clip(&path)?;
				self.shared.send(Event::ClipSaved { path, duration: length });
				return Ok(());
			}
			Command::AddOutput(spec) => return self.add_output(spec).await,
			Command::RemoveOutput { id } => {
				let mut outputs = lock(&self.shared.outputs);
				let Some(at) = outputs.iter().position(|o| o.id == id) else {
					return Err(Error::InvalidFrame(format!("no studio output {id}")));
				};
				let mut output = outputs.remove(at);
				drop(outputs);
				let name = output.sink.name().to_owned();
				if let Err(e) = output.sink.finish() {
					self.shared.fail(name.clone(), e);
				}
				self.shared.send(Event::OutputRemoved { id, name });
				self.shared.state_changed();
				return Ok(());
			}
			Command::GoLive => {
				*lock(&self.shared.state) = State::Live;
				*lock(&self.shared.live_since) = Some(Instant::now());
				self.shared.state_changed();
				return Ok(());
			}
			Command::EndStream => {
				let pushing: Vec<Output> = {
					let mut outputs = lock(&self.shared.outputs);
					let (pushing, rest) = outputs.drain(..).partition(|o| o.pushes);
					*outputs = rest;
					pushing
				};
				for mut output in pushing {
					let name = output.sink.name().to_owned();
					if let Err(e) = output.sink.finish() {
						self.shared.fail(name.clone(), e);
					}
					self.shared.send(Event::OutputRemoved { id: output.id, name });
				}
				*lock(&self.shared.state) = State::Idle;
				*lock(&self.shared.live_since) = None;
				self.shared.state_changed();
				return Ok(());
			}
		}
		// Every scene-graph command lands here: start and stop the inputs the
		// new graph needs and publish it.
		self.rebuild().await;
		Ok(())
	}

	fn start_recording(&self, path: &Path) -> Result<()> {
		if lock(&self.shared.recording).is_some() {
			return Err(Error::InvalidFrame("already recording".into()));
		}
		let layer = self.shared.layer.load(Ordering::Relaxed);
		let recorder = output::record::Recorder::start(path, layer, 2)?;
		let id = {
			let mut graph = lock(&self.graph);
			let id = graph.next_output;
			graph.next_output += 1;
			id
		};
		lock(&self.shared.outputs).push(Output {
			id,
			sink: Box::new(recorder),
			path: Some(path.to_owned()),
			started: Instant::now(),
			kbps: 0.0,
			last_bytes: 0,
			pushes: false,
			error: None,
		});
		*lock(&self.shared.recording) = Some(id);
		self.shared.send(Event::RecordingStarted { path: path.to_owned() });
		self.shared.state_changed();
		Ok(())
	}

	fn stop_recording(&self) -> Result<()> {
		let Some(id) = lock(&self.shared.recording).take() else {
			return Err(Error::InvalidFrame("not recording".into()));
		};
		let mut outputs = lock(&self.shared.outputs);
		let Some(at) = outputs.iter().position(|o| o.id == id) else { return Ok(()) };
		let mut output = outputs.remove(at);
		drop(outputs);
		let bytes = output.sink.bytes();
		let path = output.path.clone().unwrap_or_else(|| PathBuf::from(output.sink.name()));
		let duration = self.shared.recorded_for(&output);
		if let Err(e) = output.sink.finish() {
			self.shared.fail("recording", e);
		}
		self.shared.send(Event::RecordingStopped { path, duration, bytes });
		self.shared.state_changed();
		Ok(())
	}

	async fn add_output(&self, spec: OutputSpec) -> Result<()> {
		let layer = self.shared.layer.load(Ordering::Relaxed);
		let (sink, pushes): (Box<dyn OutputSink>, bool) = match &spec {
			OutputSpec::Record { path } => {
				(Box::new(output::record::Recorder::start(path, layer, 2)?), false)
			}
			OutputSpec::Url { url, token } if output::rtmp::Rtmp::handles(url) => {
				let codec = *lock(&self.shared.codec);
				let rtmp = output::rtmp::Rtmp::start(url, token.as_deref(), layer, codec).await?;
				(Box::new(rtmp), true)
			}
			OutputSpec::Url { url, token } => {
				#[cfg(feature = "whip")]
				{
					let codec = self.stream_codec();
					let whip = output::whip::Whip::start(url, token.as_deref(), codec, layer, true)
						.await?;
					(Box::new(whip), true)
				}
				#[cfg(not(feature = "whip"))]
				{
					let _ = (url, token);
					return Err(Error::CaptureUnavailable {
						backend: "whip",
						reason: "this build has no WHIP output (feature `whip`)".into(),
					});
				}
			}
		};
		let id = {
			let mut graph = lock(&self.graph);
			let id = graph.next_output;
			graph.next_output += 1;
			id
		};
		let name = sink.name().to_owned();
		let path = match &spec {
			OutputSpec::Record { path } => Some(path.clone()),
			OutputSpec::Url { .. } => None,
		};
		lock(&self.shared.outputs).push(Output {
			id,
			sink,
			path,
			started: Instant::now(),
			kbps: 0.0,
			last_bytes: 0,
			pushes,
			error: None,
		});
		self.shared.send(Event::OutputAdded { id, name });
		self.shared.state_changed();
		Ok(())
	}

	/// The codec a WHIP output offers: whatever the streamer encodes, which
	/// [`Studio::write_packet`] learns. VP8 until the first packet.
	#[cfg(feature = "whip")]
	fn stream_codec(&self) -> Codec {
		lock(&self.shared.codec).unwrap_or(Codec::Vp8)
	}

	/// Start and stop source inputs so they match the active scene, then
	/// publish it to the compose thread.
	async fn rebuild(&self) {
		let (scene, size, fps, wanted) = {
			let graph = lock(&self.graph);
			let scene = graph.scenes.active().cloned().unwrap_or_else(|| Scene::new(0, ""));
			let wanted: Vec<(u64, SourceKind, Background)> = scene
				.sources
				.iter()
				.map(|s| (s.id, s.kind.clone(), s.background.clone()))
				.collect();
			(scene, graph.scenes.size(), graph.scenes.fps(), wanted)
		};
		// Drop the inputs of sources that are gone or that changed input.
		{
			let mut graph = lock(&self.graph);
			graph.inputs.retain(|(id, input)| {
				wanted.iter().any(|(w, kind, background)| {
					w == id
						&& input.kind().same_input(kind)
						// A background filter is set up when the input starts,
						// with the effect it applies: another effect (or
						// strength, image, colour) needs a new input.
						&& input.background() == background
				})
			});
		}
		for (id, kind, background) in &wanted {
			let known = lock(&self.graph).inputs.iter().any(|(i, _)| i == id);
			if known {
				continue;
			}
			let input = Input::start(kind.clone(), fps, background.clone()).await;
			if let Some(error) = input.feed.error() {
				self.shared.fail(kind.label(), error);
			}
			lock(&self.graph).inputs.push((*id, input));
		}
		let mut graph = lock(&self.graph);
		graph.revision += 1;
		let revision = graph.revision;
		let feeds: Vec<Option<Arc<Feed>>> = scene
			.sources
			.iter()
			.map(|s| graph.inputs.iter().find(|(id, _)| *id == s.id).map(|(_, i)| i.feed.clone()))
			.collect();
		for (_, input) in &graph.inputs {
			input.set_fps(fps);
		}
		let scenes = Arc::new(graph.scenes.clone());
		drop(graph);
		*lock(&self.shared.live) = Arc::new(Live { scene, feeds, revision, size, fps });
		self.shared.send(Event::Scenes(scenes));
		self.shared.state_changed();
	}

	/// Hand one encoded packet to every output. Called on the streamer's
	/// encoder threads.
	pub fn write_packet(&self, packet: &Packet<'_>) {
		self.shared.write_packet(packet);
	}

	/// Whether an output needs a keyframe of `layer` now (a recording that
	/// just started, a WHIP session that just connected, the replay buffer's
	/// next clip start). Only the layer the outputs take
	/// ([`Studio::set_layer`]) ever does.
	pub fn needs_keyframe(&self, layer: u32) -> bool {
		layer == self.shared.layer.load(Ordering::Relaxed) && self.shared.needs_keyframe()
	}

	/// Stop composing and close every output.
	pub fn stop(&mut self) {
		self.shared.stop.store(true, Ordering::Relaxed);
		self.shared.preview.close();
		for thread in self.threads.drain(..) {
			let _ = thread.join();
		}
		for mut output in lock(&self.shared.outputs).drain(..) {
			if let Err(e) = output.sink.finish() {
				debug!("closing a studio output: {e}");
			}
		}
		lock(&self.graph).inputs.clear();
	}
}

impl Drop for Studio {
	fn drop(&mut self) {
		self.stop();
	}
}

fn no_scene(scene: u64) -> Error {
	Error::InvalidFrame(format!("no studio scene {scene}"))
}

impl Shared {
	fn status(&self) -> Status {
		let live = lock(&self.live);
		let outputs = lock(&self.outputs);
		let recording = lock(&self.recording)
			.and_then(|id| outputs.iter().find(|o| o.id == id))
			.and_then(|o| o.path.clone());
		let recorded_for = lock(&self.recording)
			.and_then(|id| outputs.iter().find(|o| o.id == id))
			.map_or(Duration::ZERO, |o| self.recorded_for(o));
		Status {
			state: *lock(&self.state),
			live_for: lock(&self.live_since).map_or(Duration::ZERO, |at| at.elapsed()),
			recording,
			recorded_for,
			active_scene: live.scene.id,
			output: live.size,
			fps: live.fps,
			outputs: outputs.iter().filter(|o| o.pushes).count(),
		}
	}

	/// How long an output has been going.
	fn recorded_for(&self, output: &Output) -> Duration {
		output.started.elapsed()
	}

	fn state_changed(&self) {
		self.send(Event::State(self.status()));
	}

	fn needs_keyframe(&self) -> bool {
		let mut outputs = lock(&self.outputs);
		let any = outputs.iter_mut().any(|o| o.sink.needs_keyframe());
		any || lock(&self.replay).needs_keyframe()
	}

	fn write_packet(&self, packet: &Packet<'_>) {
		if let Track::Video { codec, .. } = packet.track {
			let mut known = lock(&self.codec);
			if *known != Some(codec) {
				*known = Some(codec);
			}
		}
		if let Err(e) = lock(&self.replay).write(packet) {
			self.fail("replay", e);
		}
		let mut failed = Vec::new();
		{
			let mut outputs = lock(&self.outputs);
			for output in outputs.iter_mut() {
				if !output.sink.wants(packet.track) {
					continue;
				}
				if let Err(e) = output.sink.write(packet) {
					output.error = Some(e.to_string());
					failed.push((output.id, output.sink.name().to_owned(), e.to_string()));
				}
			}
			outputs.retain(|o| !failed.iter().any(|(id, _, _)| *id == o.id));
		}
		for (id, name, message) in failed {
			self.fail(name.clone(), message);
			self.send(Event::OutputRemoved { id, name });
		}
	}
}

/// The compose thread: draw the scene at the output rate, offer it to the
/// preview and hand it to the streamer.
fn compose_loop(shared: &Shared) {
	let mut compositor = Compositor::new(0);
	let mut preview = RgbaScaler::new(2);
	let mut pacer = crate::capture::FramePacer::new(Some(15));
	let mut preview_fps = 0;
	let mut next = Instant::now();
	while !shared.stopped() {
		let live = lock(&shared.live).clone();
		let interval = Duration::from_secs(1) / live.fps.max(1);
		let want = shared.preview_fps.load(Ordering::Relaxed);
		if want != preview_fps {
			preview_fps = want;
			pacer.set_fps(Some(want));
		}
		compositor.set_size(live.size.0, live.size.1);
		let timestamp = shared.epoch.elapsed();
		let frame = match compositor.compose(&live.scene, live.revision, &live.feeds, timestamp) {
			Ok(frame) => frame,
			Err(e) => {
				shared.fail("compose", e);
				std::thread::sleep(POLL);
				continue;
			}
		};
		// The preview, at its own size and rate.
		if pacer.take(timestamp) {
			let (pw, ph) = *lock(&shared.preview_size);
			match preview.scale(&frame, pw, ph) {
				Ok(small) => {
					shared.preview.put(small);
					shared.preview_frames.fetch_add(1, Ordering::Relaxed);
				}
				Err(e) => shared.fail("preview", e),
			}
		}
		// The streamer, if it is capturing us.
		{
			let mut sink = lock(&shared.sink);
			if let Some(sink) = sink.as_mut()
				&& sink.wants(timestamp)
			{
				if sink.frame(frame.view()) {
					shared.delivered.fetch_add(1, Ordering::Relaxed);
				} else {
					debug!("the studio's frame sink went away");
					*lock(&shared.sink) = None;
				}
			}
		}
		drop(frame);
		*lock(&shared.compose) = compositor.stats();
		next += interval;
		let now = Instant::now();
		if next < now {
			next = now;
		}
		std::thread::sleep((next - now).min(POLL));
	}
}

/// Once a second: rates for [`Event::Stats`].
fn stats_loop(shared: &Shared) {
	let mut last = Instant::now();
	let mut frames = 0u64;
	let mut per_source: Vec<(u64, u64)> = Vec::new();
	while !shared.stopped() {
		std::thread::sleep(POLL);
		let elapsed = last.elapsed();
		if elapsed < Duration::from_secs(1) {
			continue;
		}
		last = Instant::now();
		let seconds = elapsed.as_secs_f64();
		let live = lock(&shared.live).clone();
		let mut sources = Vec::with_capacity(live.scene.sources.len());
		for (source, feed) in live.scene.sources.iter().zip(&live.feeds) {
			let (delivered, dropped, size, error) = match feed {
				Some(feed) => (feed.delivered(), feed.dropped(), feed.size(), feed.error()),
				None => (0, 0, (0, 0), None),
			};
			let previous =
				per_source.iter().find(|(id, _)| *id == source.id).map_or(0, |(_, n)| *n);
			sources.push(SourceStats {
				id: source.id,
				label: source.label().to_owned(),
				kind: source.kind.label(),
				width: size.0,
				height: size.1,
				frames: delivered,
				fps: delivered.saturating_sub(previous) as f64 / seconds,
				dropped,
				error,
			});
		}
		per_source = sources.iter().map(|s| (s.id, s.frames)).collect();
		let outputs = {
			let mut outputs = lock(&shared.outputs);
			let mut stats = Vec::with_capacity(outputs.len());
			for output in outputs.iter_mut() {
				let bytes = output.sink.bytes();
				output.kbps =
					(bytes.saturating_sub(output.last_bytes) * 8) as f64 / seconds / 1000.0;
				output.last_bytes = bytes;
				stats.push(OutputStats {
					id: output.id,
					name: output.sink.name().to_owned(),
					bytes,
					kbps: output.kbps,
					error: output.error.clone().or_else(|| output.sink.error()),
				});
			}
			stats
		};
		let composed = lock(&shared.compose).frames;
		let fps = composed.saturating_sub(frames) as f64 / seconds;
		frames = composed;
		shared.send(Event::Stats(Box::new(Stats {
			status: shared.status(),
			compose: *lock(&shared.compose),
			fps,
			sources,
			outputs,
			replay: lock(&shared.replay).stats(),
			preview_frames: shared.preview_frames.load(Ordering::Relaxed),
			delivered: shared.delivered.load(Ordering::Relaxed),
		})));
	}
}

/// The studio as a [`ScreenCapture`] backend: what makes the composite a
/// video source of the existing streamer, with its encoders, simulcast and
/// peers unchanged.
pub struct StudioCapture {
	shared: Arc<Shared>,
}

impl ScreenCapture for StudioCapture {
	fn backend(&self) -> &'static str {
		"studio"
	}

	fn sources(&mut self) -> Result<Vec<CaptureSource>> {
		let live = lock(&self.shared.live);
		Ok(vec![CaptureSource {
			id: SourceId::Studio,
			name: "Stream Studio".into(),
			width: live.size.0,
			height: live.size.1,
			primary: true,
		}])
	}

	fn start(
		&mut self,
		source: &SourceId,
		options: &CaptureOptions,
	) -> BoxFuture<'_, Result<crate::queue::FrameReceiver<VideoFrame>>> {
		let (source, options) = (source.clone(), options.clone());
		Box::pin(async move {
			if source != SourceId::Studio {
				return Err(Error::SourceNotFound(source));
			}
			let (sink, rx) = crate::capture::QueueSink::new(&options);
			*lock(&self.shared.sink) = Some(Box::new(sink));
			Ok(rx)
		})
	}

	fn start_sink(
		&mut self,
		source: &SourceId,
		_options: &CaptureOptions,
		sink: Box<dyn FrameSink>,
	) -> BoxFuture<'_, Result<()>> {
		let source = source.clone();
		Box::pin(async move {
			if source != SourceId::Studio {
				return Err(Error::SourceNotFound(source));
			}
			*lock(&self.shared.sink) = Some(sink);
			Ok(())
		})
	}

	fn stop(&mut self) {
		*lock(&self.shared.sink) = None;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::studio::scene::Colour;

	fn one_colour_scene() -> Scenes {
		let mut scenes = Scenes { width: 64, height: 48, fps: 60, ..Scenes::default() };
		let mut scene = Scene::new(1, "Main");
		scene.sources.push(Source {
			transform: Transform::full(64, 48),
			..Source::new(1, SourceKind::Colour { colour: Colour::rgb(9, 8, 7), size: (64, 48) })
		});
		scenes.scenes.push(scene);
		scenes.active = 1;
		scenes
	}

	#[tokio::test]
	async fn it_composes_into_the_preview_and_follows_commands() {
		let studio = Studio::start(one_colour_scene()).await.unwrap();
		let preview = studio.preview();
		studio.apply(Command::SetPreview { width: 32, height: 24, fps: 60 }).await.unwrap();
		let started = Instant::now();
		let frame = loop {
			if let Some(frame) = preview.wait_timeout(Duration::from_millis(200)) {
				break frame;
			}
			assert!(started.elapsed() < Duration::from_secs(5), "no preview frame");
		};
		assert_eq!((frame.width, frame.height), (32, 24));
		let crate::frame::FrameData::Rgba(p) = &frame.data else { panic!("not RGBA") };
		assert_eq!(&p.data[..4], &[9, 8, 7, 255]);

		// Scenes and sources can be added, moved and removed while running.
		studio.apply(Command::AddScene { name: "Break".into() }).await.unwrap();
		let scenes = studio.scenes();
		assert_eq!(scenes.scenes.len(), 2);
		let break_id = scenes.scenes[1].id;
		studio
			.apply(Command::AddSource {
				scene: break_id,
				name: "Slate".into(),
				kind: SourceKind::Colour { colour: Colour::rgb(1, 2, 3), size: (64, 48) },
			})
			.await
			.unwrap();
		studio.apply(Command::SetActiveScene { scene: break_id }).await.unwrap();
		assert_eq!(studio.status().active_scene, break_id);
		// The new scene's source draws.
		let frame = loop {
			let frame = preview.wait_timeout(Duration::from_secs(2)).expect("a preview frame");
			let crate::frame::FrameData::Rgba(p) = &frame.data else { panic!() };
			if p.data[..3] == [1, 2, 3] {
				break frame;
			}
			assert!(started.elapsed() < Duration::from_secs(10), "the scene never switched");
		};
		drop(frame);

		studio.apply(Command::RemoveScene { scene: break_id }).await.unwrap();
		assert_eq!(studio.scenes().scenes.len(), 1);
		assert!(studio.apply(Command::SetActiveScene { scene: 99 }).await.is_err());
		assert!(studio.apply(Command::RenameScene { scene: 99, name: "x".into() }).await.is_err());
	}

	#[tokio::test]
	async fn a_new_background_effect_reaches_the_input() {
		let studio = Studio::start(one_colour_scene()).await.unwrap();
		for background in [
			Background::Blur { strength: 0.02 },
			Background::Blur { strength: 0.08 },
			Background::Colour { colour: Colour::rgb(0, 200, 0) },
			Background::Keep,
		] {
			let change =
				SourceChange { background: Some(background.clone()), ..Default::default() };
			let command = Command::UpdateSource { scene: 1, source: 1, change: Box::new(change) };
			studio.apply(command).await.unwrap();
			let graph = lock(&studio.graph);
			assert_eq!(graph.inputs.len(), 1);
			assert_eq!(graph.inputs[0].1.background(), &background);
		}
	}

	#[tokio::test]
	async fn go_live_and_recording_change_the_status() {
		let studio = Studio::start(one_colour_scene()).await.unwrap();
		let mut events = studio.events();
		assert_eq!(studio.status().state, State::Idle);
		studio.apply(Command::GoLive).await.unwrap();
		assert_eq!(studio.status().state, State::Live);
		// Ending the stream closes what pushes somewhere.
		struct Pushing;
		impl OutputSink for Pushing {
			fn name(&self) -> &str {
				"push"
			}

			fn write(&mut self, _packet: &Packet<'_>) -> Result<()> {
				Ok(())
			}
		}
		lock(&studio.shared.outputs).push(Output {
			id: 99,
			sink: Box::new(Pushing),
			path: None,
			started: Instant::now(),
			kbps: 0.0,
			last_bytes: 0,
			pushes: true,
			error: None,
		});
		assert_eq!(studio.status().outputs, 1);
		studio.apply(Command::EndStream).await.unwrap();
		assert_eq!(studio.status().state, State::Idle);
		assert_eq!(studio.status().outputs, 0);
		assert!(lock(&studio.shared.outputs).is_empty());

		// A recording is an output; stopping one that is not running fails.
		assert!(studio.apply(Command::StopRecording).await.is_err());
		let dir = std::env::temp_dir().join(format!("voelin-studio-ctl-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("out.webm");
		studio.apply(Command::StartRecording { path: path.clone() }).await.unwrap();
		assert_eq!(studio.status().recording.as_deref(), Some(path.as_path()));
		assert!(studio.needs_keyframe(0), "a fresh recording wants a keyframe");
		assert!(!studio.needs_keyframe(1), "but only of the layer it records");
		assert!(studio.apply(Command::StartRecording { path: path.clone() }).await.is_err());
		studio.apply(Command::StopRecording).await.unwrap();
		assert!(studio.status().recording.is_none());

		// The replay buffer is off until it is given a window.
		assert!(studio.apply(Command::SaveClip { path: dir.join("clip.webm") }).await.is_err());
		studio.apply(Command::SetReplay { seconds: 5, memory_mb: 8 }).await.unwrap();
		// Still no keyframe in it, so still no clip.
		assert!(studio.apply(Command::SaveClip { path: dir.join("clip.webm") }).await.is_err());

		// Events arrived for all of it.
		let mut seen = Vec::new();
		while let Ok(event) = events.try_recv() {
			seen.push(event);
		}
		assert!(seen.iter().any(|e| matches!(e, Event::RecordingStarted { .. })), "{seen:?}");
		assert!(seen.iter().any(|e| matches!(e, Event::RecordingStopped { .. })), "{seen:?}");
		assert!(seen.iter().any(|e| matches!(e, Event::OutputRemoved { id: 99, .. })), "{seen:?}");
		assert!(seen.iter().any(|e| matches!(e, Event::State(_))), "{seen:?}");
		std::fs::remove_dir_all(&dir).ok();
	}

	#[tokio::test]
	async fn packets_reach_the_outputs_and_the_studio_is_a_capture_backend() {
		let studio = Arc::new(Studio::start(one_colour_scene()).await.unwrap());
		let dir = std::env::temp_dir().join(format!("voelin-studio-tee-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		studio
			.apply(Command::AddOutput(OutputSpec::Record { path: dir.join("tee.webm") }))
			.await
			.unwrap();
		studio.apply(Command::SetReplay { seconds: 2, memory_mb: 4 }).await.unwrap();
		for n in 0..30u64 {
			studio.write_packet(&Packet {
				track: Track::Video { codec: Codec::Vp8, layer: 0 },
				pts_90khz: n * 3000,
				keyframe: n.is_multiple_of(15),
				width: 64,
				height: 48,
				data: &[1, 2, 3, 4],
			});
		}
		{
			let outputs = lock(&studio.shared.outputs);
			assert_eq!(outputs.len(), 1);
			assert!(outputs[0].sink.bytes() > 0);
		}
		assert!(lock(&studio.shared.replay).stats().packets > 0);
		// RTMP carries H.264: with VP8 as the stream codec it is refused
		// with that reason, before anything connects.
		let e = studio
			.apply(Command::AddOutput(OutputSpec::Url {
				url: "rtmp://live.example/app".into(),
				token: None,
			}))
			.await
			.unwrap_err()
			.to_string();
		assert!(e.contains("H.264") && e.contains("VP8"), "{e}");

		// The studio is a capture backend the streamer can use unchanged.
		let mut capture = studio.capture();
		assert_eq!(capture.backend(), "studio");
		let listed = capture.sources().unwrap();
		assert_eq!(listed[0].id, SourceId::Studio);
		assert_eq!((listed[0].width, listed[0].height), (64, 48));
		let frames = capture.start(&SourceId::Studio, &CaptureOptions::default()).await.unwrap();
		let mut frames = frames;
		let frame = frames.recv_timeout(Duration::from_secs(5)).expect("a composed frame");
		assert_eq!((frame.width, frame.height), (64, 48));
		assert!(studio.shared.delivered.load(Ordering::Relaxed) > 0);
		capture.stop();
		std::fs::remove_dir_all(&dir).ok();
	}
}
