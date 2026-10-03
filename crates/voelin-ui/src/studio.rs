//! The Stream Studio: the controller behind its page in the main window, its
//! window of its own and the phone layout (ui/screens/studio*.slint,
//! ui/studio-window.slint).
//!
//! The engine's studio (`voelin_core::studio`) composites the scenes; a
//! `Streamer` started for it encodes the composite from the start, so the
//! recording and the replay buffer need no live stream, and going live
//! attaches it to a TeamSpeak stream of our voice channel like any share.
//! The studio runs while it is shown, live or recording.
//!
//! Both windows' StudioBridge globals show the same models (shared
//! VecModels) and get the same values; their callbacks come here
//! (bind/studio.rs). Commands go to the studio through one task, in order.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc};
use tracing::warn;
use voelin_core::media::voelin_media::capture::SourceId;
use voelin_core::media::voelin_media::handoff::Handoff;
use voelin_core::media::voelin_media::mix::SourceHandle;
use voelin_core::media::voelin_media::{VideoFrame, convert};
use voelin_core::media::{
	AudioApps, AudioSourceSpec, Latest, Streamer, StreamerConfigUpdate, StreamerStats, audio_apps,
	audio_source_specs, screen_sources,
};
use voelin_core::settings::{
	AudioSourceKindSetting, AudioSourceSetting, Key, Kind, STREAM_AUDIO_SOURCES,
	STREAM_BITRATE_KBPS, STREAM_LAYERS, STUDIO_RECORDING_DIR, STUDIO_REPLAY_MEMORY_MB,
	STUDIO_REPLAY_SECONDS, STUDIO_SCENES, Settings, layer_specs,
};
use voelin_core::stream::{EndReason, StreamSetup, ViewerInfo, ViewerState};
use voelin_core::studio::scene::{
	Align, Background, Colour, Crop, Fit, Scene, Scenes, Source, SourceKind, Transform,
};
use voelin_core::studio::{self, SourceChange, Stats, Status, Studio, camera};
use voelin_core::{Command, HistoryMessage, HistorySource, StreamState};
use voelin_model::{ChatMessage, ChatTarget};
use voelin_store::MessageSource;

use crate::app::{
	App, ChatLine, Nav, Page, StudioAudio, StudioBridge, StudioForm, StudioLayer, StudioNav,
	StudioPick, StudioScene, StudioSettingsForm, StudioSource, StudioSourceForm, StudioStatus,
	StudioViewer, StudioWindow, Tab, Theme, later, with_app,
};
use crate::settings::parse_positive;
use crate::video::CaptureRequest;
use crate::vm;
use crate::vm::studio::{OVERLAY_CHAT, OVERLAY_NOW_PLAYING, OVERLAY_VIEWERS, Overlay};

type Picture = SharedPixelBuffer<Rgba8Pixel>;

/// The stream settings panel and the studio's own choices.
pub static STUDIO_UI: Key<StudioUi> = Key::new(
	"studio.ui",
	Kind::Json,
	"Stream Studio: title, game, go-live message, overlays, stream audio, preview size.",
	StudioUi::default,
);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioUi {
	pub title: String,
	/// Game or category: goes into the stream's name after the title.
	pub game: String,
	/// Sent to the channel's chat when the stream goes live.
	pub message: String,
	/// Overlay sources kept in the live scene (`vm::studio::apply_overlays`).
	pub show_viewers: bool,
	pub show_chat: bool,
	pub show_now_playing: bool,
	/// The stream's audio (the mixer's sources); off: none.
	pub audio: bool,
	/// The preview's width in pixels (its height follows the output) and
	/// frame rate.
	pub preview_width: u32,
	pub preview_fps: u32,
}

impl Default for StudioUi {
	fn default() -> Self {
		Self {
			title: String::new(),
			game: String::new(),
			message: String::new(),
			show_viewers: false,
			show_chat: false,
			show_now_playing: false,
			audio: true,
			preview_width: 960,
			preview_fps: 15,
		}
	}
}

/// The models both windows show.
#[derive(Default)]
struct Models {
	scenes: Rc<VecModel<StudioScene>>,
	sources: Rc<VecModel<StudioSource>>,
	audio: Rc<VecModel<StudioAudio>>,
	layers: Rc<VecModel<StudioLayer>>,
	viewers: Rc<VecModel<StudioViewer>>,
	destinations: Rc<VecModel<StudioPick>>,
	picks: Rc<VecModel<StudioPick>>,
	/// The destination channel's chat (`studio_sync_chat`).
	chat: Rc<VecModel<ChatLine>>,
}

/// What a picker row adds.
enum Pick {
	Source { name: String, kind: SourceKind },
	Audio(AudioSourceSetting),
}

/// Our stream from the studio.
struct Outgoing {
	session: i64,
	live: bool,
	viewers: Vec<ViewerInfo>,
}

/// A change of the scene graph, applied in order by one task.
enum Op {
	Apply(studio::Command),
	/// A source for the live scene (a new scene if there is none).
	Add(Source),
	/// Keep the overlay sources as asked.
	Overlays(Vec<Overlay>),
}

/// A running studio.
struct Running {
	studio: Arc<Studio>,
	/// Encodes the composite; `None` until it started (or if it failed).
	streamer: Option<Streamer>,
	ops: mpsc::UnboundedSender<Op>,
	/// The newest preview picture, for the UI thread.
	pictures: Arc<Latest<Picture>>,
	/// Stops the preview thread.
	stop: Arc<AtomicBool>,
	tasks: Vec<tokio::task::JoinHandle<()>>,
	/// The mixer's meters, about 15 times a second.
	_meters: slint::Timer,
}

impl Drop for Running {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
		for task in &self.tasks {
			task.abort();
		}
	}
}

/// The studio's state on the UI thread (`App::studio`).
#[derive(Default)]
pub(crate) struct StudioState {
	models: Models,
	/// The main window's globals have the models.
	attached: bool,
	/// The studio in a window of its own.
	window: Option<StudioWindow>,
	run: Option<Running>,
	starting: bool,
	/// Why it does not run or cannot stream.
	error: String,
	/// Where it keeps its keys: the app's settings, or with VOELIN_DEMO_UI
	/// its own in memory (nothing stored).
	settings: Option<Settings>,
	scenes: Arc<Scenes>,
	status: Status,
	stats: Option<Box<Stats>>,
	encoder: Option<StreamerStats>,
	ui: StudioUi,
	destination: Option<i64>,
	stream: Option<Outgoing>,
	picks: Vec<Pick>,
	/// Applications that play audio, for the audio picker.
	apps: Option<AudioApps>,
	/// The mixer source of each audio row.
	meters: Vec<Option<SourceHandle>>,
	/// The overlays last asked for.
	overlays: Vec<Overlay>,
	has_preview: bool,
	/// Stats ticks it was not shown; it stops after a few.
	hidden: u32,
	/// Development switches waiting for the studio (dev.rs).
	dev: Vec<String>,
}

/// Forward the studio's events to the UI thread.
async fn forward_events(mut events: broadcast::Receiver<studio::Event>) {
	loop {
		match events.recv().await {
			Ok(event) => later(move |app| app.studio_event(event)),
			Err(RecvError::Lagged(_)) => {}
			Err(RecvError::Closed) => break,
		}
	}
}

/// Apply the ops one after the other.
async fn run_ops(studio: Weak<Studio>, mut ops: mpsc::UnboundedReceiver<Op>) {
	while let Some(op) = ops.recv().await {
		let Some(studio) = studio.upgrade() else { break };
		let result = match op {
			Op::Apply(command) => studio.apply(command).await,
			Op::Add(mut source) => {
				let mut scenes = studio.scenes();
				if scenes.active().is_none() {
					let id = scenes.next_scene_id();
					scenes.scenes.push(Scene::new(id, "Scene 1"));
					scenes.active = id;
				}
				if let Some(scene) = scenes.active_mut() {
					source.id = scene.next_source_id();
					scene.sources.push(source);
				}
				studio.apply(studio::Command::SetScenes(Box::new(scenes))).await
			}
			Op::Overlays(wanted) => {
				let mut scenes = studio.scenes();
				if vm::studio::apply_overlays(&mut scenes, &wanted) {
					studio.apply(studio::Command::SetScenes(Box::new(scenes))).await
				} else {
					Ok(())
				}
			}
		};
		if let Err(e) = result {
			later(move |app| app.set_status(format!("Stream Studio: {e}")));
		}
	}
}

/// Tell the UI thread when a key the studio follows changes elsewhere.
async fn watch_settings(settings: Settings) {
	let mut audio = settings.watch(&STREAM_AUDIO_SOURCES);
	let mut layers = settings.watch(&STREAM_LAYERS);
	let mut bitrate = settings.watch(&STREAM_BITRATE_KBPS);
	let mut ui = settings.watch(&STUDIO_UI);
	loop {
		tokio::select! {
			Some(_) = audio.changed() => later(|app| app.studio_audio_changed()),
			Some(_) = layers.changed() => later(|app| app.studio_encoding_changed()),
			Some(_) = bitrate.changed() => later(|app| app.studio_encoding_changed()),
			Some(_) = ui.changed() => later(|app| app.studio_ui_changed()),
			else => break,
		}
	}
}

/// Turn preview frames into pictures on a thread of its own; the UI thread
/// takes the newest when it gets to it.
fn spawn_preview(
	handoff: Arc<Handoff<VideoFrame>>,
	pictures: Arc<Latest<Picture>>,
	stop: Arc<AtomicBool>,
) {
	let spawned =
		std::thread::Builder::new().name("voelin-studio-preview".into()).spawn(move || {
			while !stop.load(Ordering::Relaxed) && !handoff.is_closed() {
				let Some(frame) = handoff.wait_timeout(Duration::from_millis(250)) else {
					continue;
				};
				let mut picture = Picture::new(frame.width, frame.height);
				let stride = frame.width as usize * 4;
				if let Err(e) = convert::to_rgba(&frame, picture.make_mut_bytes(), stride) {
					warn!("studio preview: {e}");
					continue;
				}
				if pictures.put(picture) {
					later(|app| app.studio_picture());
				}
			}
		});
	if let Err(e) = spawned {
		warn!("cannot show the studio's preview: {e}");
	}
}

/// The demo studio of VOELIN_DEMO_UI: sample scenes from synthetic sources,
/// test tones, recordings in a temporary folder; in memory only.
fn demo_settings() -> Settings {
	let settings = Settings::in_memory();
	let text = |text: &str, size_px: f32| SourceKind::Text {
		text: text.into(),
		font: None,
		size_px,
		colour: Colour::WHITE,
		backdrop: Colour::CLEAR,
		align: Align::Center,
		padding: 0,
	};
	let source = |id: u64, name: &str, kind: SourceKind, transform: Transform| Source {
		name: name.into(),
		transform,
		..Source::new(id, kind)
	};
	let slate = |id: u64, name: &str, words: &str| {
		let mut scene = Scene::new(id, name);
		scene.sources = vec![
			source(
				1,
				"Backdrop",
				SourceKind::Colour { colour: Colour::rgb(16, 26, 60), size: (1920, 1080) },
				Transform::full(1920, 1080),
			),
			source(2, "Title", text(words, 120.0), Transform::box_at(160.0, 440.0, 1600.0, 200.0)),
		];
		scene
	};
	let camera = SourceKind::Camera {
		device: camera::SYNTHETIC.into(),
		size: None,
		fps: None,
		mirror: false,
	};
	let mut main = Scene::new(1, "Main Stream");
	let mut cam = source(2, "Camera", camera.clone(), Transform::default());
	cam.transform = vm::studio::placement(&camera, (1920, 1080));
	cam.background = Background::Blur { strength: 0.04 };
	let mut game = source(
		1,
		"Game Capture",
		SourceKind::Pattern { size: (1280, 720) },
		Transform { fit: Fit::Cover, ..Transform::full(1920, 1080) },
	);
	// The pattern's frame counter sits in its top-left corner, under the
	// preview's LIVE badge and timer: cropped away.
	game.crop = Crop { top: 120, ..Crop::default() };
	main.sources = vec![game, cam];
	let mut chatting = Scene::new(2, "Just Chatting");
	chatting.sources = vec![source(
		1,
		"Camera",
		camera,
		Transform { fit: Fit::Cover, ..Transform::full(1920, 1080) },
	)];
	let scenes = Scenes {
		scenes: vec![
			main,
			chatting,
			slate(3, "Be Right Back", "Be right back"),
			slate(4, "Starting Soon", "Starting soon"),
			slate(5, "Ending", "Thanks for watching"),
		],
		active: 1,
		width: 1920,
		height: 1080,
		fps: 60,
	};
	let tone = |frequency, gain| AudioSourceSetting {
		gain,
		..AudioSourceSetting::new(AudioSourceKindSetting::Synthetic { frequency })
	};
	let recordings = std::env::temp_dir().join("voelin-demo-recordings");
	let results = [
		settings.set(&STUDIO_SCENES, scenes),
		settings.set(
			&STREAM_AUDIO_SOURCES,
			vec![
				AudioSourceSetting {
					gain: 0.49,
					..AudioSourceSetting::new(AudioSourceKindSetting::Microphone)
				},
				tone(440, 0.25),
				tone(660, 0.39),
			],
		),
		settings.set(&STUDIO_RECORDING_DIR, recordings.display().to_string()),
		settings.set(
			&STUDIO_UI,
			StudioUi {
				title: "Exploring the Lands Between".into(),
				game: "Adventure".into(),
				message: "Going live in Chill Zone — exploring the Lands Between. Come hang!"
					.into(),
				show_viewers: true,
				show_now_playing: true,
				..StudioUi::default()
			},
		),
	];
	for result in results {
		if let Err(e) = result {
			warn!("demo studio: {e}");
		}
	}
	settings
}

/// The overlay text of what is on: the title and the game.
fn now_playing(ui: &StudioUi) -> Option<String> {
	let lines: Vec<&str> =
		[ui.title.trim(), ui.game.trim()].into_iter().filter(|s| !s.is_empty()).collect();
	(!lines.is_empty()).then(|| lines.join("\n"))
}

/// The stream's name: the title, and the game after it. TeamSpeak streams
/// have a name only, and the gateway's directory takes it from there.
pub fn stream_name(ui: &StudioUi) -> String {
	match (ui.title.trim(), ui.game.trim()) {
		(title, "") => title.to_owned(),
		("", game) => game.to_owned(),
		(title, game) => format!("{title} — {game}"),
	}
}

fn file_name(what: &str) -> String {
	format!("{what} {}.mkv", chrono::Local::now().format("%Y-%m-%d %H-%M-%S"))
}

impl App {
	/// The studio's settings (made once).
	fn studio_settings(&mut self) -> Settings {
		let demo = self.demo_ui;
		let prefs = &self.prefs;
		self.studio
			.settings
			.get_or_insert_with(|| if demo { demo_settings() } else { prefs.clone() })
			.clone()
	}

	fn studio_streamer(&self) -> Option<&Streamer> {
		self.studio.run.as_ref()?.streamer.as_ref()
	}

	fn studio_op(&self, op: Op) {
		if let Some(run) = &self.studio.run {
			let _ = run.ops.send(op);
		}
	}

	/// `f` on the StudioBridge of the main window and of the studio window.
	fn studio_each(&self, f: impl Fn(&StudioBridge)) {
		if let Some(ui) = self.ui.upgrade() {
			f(&ui.global::<StudioBridge>());
		}
		if let Some(window) = &self.studio.window {
			f(&window.global::<StudioBridge>());
		}
	}

	/// The StudioNav of the window the studio is in.
	fn studio_nav(&self, f: impl FnOnce(&StudioNav)) {
		match (&self.studio.window, self.ui.upgrade()) {
			(Some(window), _) => f(&window.global::<StudioNav>()),
			(None, Some(ui)) => f(&ui.global::<StudioNav>()),
			_ => {}
		}
	}

	fn studio_attach(&self, bridge: &StudioBridge) {
		let m = &self.studio.models;
		bridge.set_scenes(ModelRc::from(m.scenes.clone()));
		bridge.set_sources(ModelRc::from(m.sources.clone()));
		bridge.set_audio(ModelRc::from(m.audio.clone()));
		bridge.set_layers(ModelRc::from(m.layers.clone()));
		bridge.set_viewers(ModelRc::from(m.viewers.clone()));
		bridge.set_destinations(ModelRc::from(m.destinations.clone()));
		bridge.set_picks(ModelRc::from(m.picks.clone()));
		bridge.set_chat(ModelRc::from(m.chat.clone()));
	}

	/// Whether a studio page or window is on screen.
	fn studio_shown(&self) -> bool {
		self.studio.window.is_some()
			|| self.ui.upgrade().is_some_and(|ui| ui.global::<Nav>().get_page() == Page::Studio)
	}

	/// The studio page or window appeared: show it and start the studio.
	pub(crate) fn studio_open(&mut self) {
		if !self.studio.attached
			&& let Some(ui) = self.ui.upgrade()
		{
			self.studio_attach(&ui.global::<StudioBridge>());
			self.studio.attached = true;
		}
		if let Some(window) = &self.studio.window
			&& let Err(e) = window.show()
		{
			warn!("cannot show the studio window: {e}");
		}
		self.studio.hidden = 0;
		if self.studio.run.is_none() && !self.studio.starting {
			self.studio_start();
		}
		self.studio_refresh();
	}

	fn studio_start(&mut self) {
		let settings = self.studio_settings();
		settings.register(&STUDIO_UI);
		self.studio.ui = (*settings.get_arc(&STUDIO_UI)).clone();
		self.studio.scenes = settings.get_arc(&STUDIO_SCENES);
		self.studio.starting = true;
		self.studio.error.clear();
		self.engine.runtime().spawn(async move {
			let result = studio::start(&settings).await.map_err(|e| e.to_string());
			later(move |app| app.studio_started(result));
		});
	}

	fn studio_started(&mut self, result: Result<Arc<Studio>, String>) {
		self.studio.starting = false;
		let studio = match result {
			Ok(studio) => studio,
			Err(e) => {
				self.studio.error = format!("The studio did not start: {e}");
				self.set_status(self.studio.error.clone());
				self.studio_refresh_status();
				return;
			}
		};
		let settings = self.studio_settings();
		let runtime = self.engine.runtime().clone();
		let (ops, rx) = mpsc::unbounded_channel();
		let stop = Arc::new(AtomicBool::new(false));
		let pictures = Arc::new(Latest::new());
		let tasks = vec![
			runtime.spawn(run_ops(Arc::downgrade(&studio), rx)),
			runtime.spawn(forward_events(studio.events())),
			runtime.spawn(watch_settings(settings.clone())),
		];
		spawn_preview(studio.preview(), pictures.clone(), stop.clone());
		let meters = slint::Timer::default();
		meters.start(slint::TimerMode::Repeated, Duration::from_millis(66), || {
			with_app(|app| app.studio_meters());
		});
		self.studio.scenes = Arc::new(studio.scenes());
		self.studio.status = studio.status();
		self.studio.run = Some(Running {
			studio: studio.clone(),
			streamer: None,
			ops,
			pictures,
			stop,
			tasks,
			_meters: meters,
		});
		self.studio_preview_size();
		// The encoder: the stream's codec, layers and audio.
		let mut sources = audio_source_specs(&settings.get(&STREAM_AUDIO_SOURCES));
		if !self.studio.ui.audio {
			sources.clear();
		}
		let request = CaptureRequest {
			fps: self.studio.scenes.fps(),
			bitrate_kbps: settings.get(&STREAM_BITRATE_KBPS),
			audio: !sources.is_empty(),
			audio_sources: sources,
			restore_token: None,
		};
		let layers = layer_specs(&settings.get(&STREAM_LAYERS));
		self.video.start_studio(&runtime, studio, request, layers, |result| {
			later(move |app| app.studio_encoder(result));
		});
		self.studio_overlays(true);
		self.studio_refresh();
	}

	fn studio_encoder(&mut self, result: Result<Streamer, String>) {
		let Some(run) = &mut self.studio.run else {
			// Stopped meanwhile.
			if let Ok(streamer) = result {
				std::thread::spawn(move || drop(streamer));
			}
			return;
		};
		let problem = match result {
			Ok(streamer) => {
				let warning = streamer.audio_error().map(|e| format!("Stream audio: {e}"));
				run.streamer = Some(streamer);
				warning
			}
			Err(e) => {
				self.studio.error = format!("The stream encoder did not start: {e}");
				Some(self.studio.error.clone())
			}
		};
		if let Some(problem) = problem {
			self.set_status(problem);
		}
		self.studio_map_meters();
		self.studio_refresh_audio();
		self.studio_refresh_status();
		self.studio_dev_apply();
	}

	/// Stop the studio (it is not shown, live or recording).
	fn studio_stop(&mut self) {
		let Some(mut run) = self.studio.run.take() else { return };
		let streamer = run.streamer.take();
		let studio = run.studio.clone();
		drop(run);
		// Stopping joins threads: not on the UI thread.
		std::thread::spawn(move || drop((streamer, studio)));
		self.studio.apps = None;
		self.studio.meters.clear();
		self.studio.stats = None;
		self.studio.encoder = None;
		self.studio.has_preview = false;
		self.studio_each(|b| {
			b.set_has_preview(false);
			b.set_preview(slint::Image::default());
		});
		self.studio_refresh_status();
	}

	/// The size and rate of the preview: its width, the output's shape.
	fn studio_preview_size(&self) {
		let (w, h) = self.studio.scenes.size();
		let width = self.studio.ui.preview_width.max(16);
		let height = ((u64::from(width) * u64::from(h) / u64::from(w.max(1))) as u32).max(2) & !1;
		let fps = self.studio.ui.preview_fps.max(1);
		self.studio_op(Op::Apply(studio::Command::SetPreview { width, height, fps }));
	}

	/// A preview picture is waiting.
	fn studio_picture(&mut self) {
		let Some(picture) = self.studio.run.as_ref().and_then(|r| r.pictures.take()) else {
			return;
		};
		let image = slint::Image::from_rgba8(picture);
		let first = !self.studio.has_preview;
		self.studio.has_preview = true;
		self.studio_each(|b| {
			b.set_preview(image.clone());
			if first {
				b.set_has_preview(true);
			}
		});
	}

	/// The studio's events.
	pub(crate) fn studio_event(&mut self, event: studio::Event) {
		match event {
			studio::Event::State(status) => {
				self.studio.status = status;
				self.studio_refresh_status();
			}
			studio::Event::Stats(stats) => {
				self.studio.status = stats.status.clone();
				self.studio.stats = Some(stats);
				self.studio_tick();
			}
			studio::Event::Scenes(scenes) => {
				let switched =
					self.studio.scenes.active().map(|s| s.id) != scenes.active().map(|s| s.id);
				let resized = self.studio.scenes.size() != scenes.size();
				self.studio.scenes = scenes;
				self.studio_refresh_scenes();
				if switched {
					self.studio_overlays(true);
				}
				if resized {
					self.studio_preview_size();
				}
				self.studio_refresh_status();
			}
			studio::Event::RecordingStarted { path } => {
				self.set_status(format!("Recording to {}", path.display()));
			}
			studio::Event::RecordingStopped { path, duration, bytes } => self.set_status(format!(
				"Saved a recording of {} ({:.1} MB) to {}",
				vm::studio::clock(duration),
				bytes as f64 / 1e6,
				path.display()
			)),
			studio::Event::ClipSaved { path, duration } => self.set_status(format!(
				"Saved a clip of {} seconds to {}",
				duration.as_secs(),
				path.display()
			)),
			studio::Event::Error { context, message } => {
				self.set_status(format!("Stream Studio ({context}): {message}"));
			}
			_ => {}
		}
	}

	/// Once a second, with the studio's statistics.
	fn studio_tick(&mut self) {
		let busy = self.studio.stream.is_some() || self.studio.status.recording.is_some();
		if self.studio_shown() || busy {
			self.studio.hidden = 0;
		} else {
			self.studio.hidden += 1;
			if self.studio.hidden >= 3 {
				self.studio_stop();
				return;
			}
		}
		self.studio.encoder = self.studio_streamer().map(Streamer::stats);
		self.studio_map_meters();
		self.studio_refresh_audio();
		self.studio_refresh_destinations();
		self.studio_refresh_scenes();
		self.studio_overlays(false);
		self.studio_refresh_status();
	}

	/// Everything the studio shows.
	fn studio_refresh(&mut self) {
		self.studio_refresh_scenes();
		self.studio_refresh_audio();
		self.studio_refresh_layers();
		self.studio_refresh_destinations();
		self.studio_refresh_status();
	}

	fn studio_refresh_scenes(&self) {
		let m = &self.studio.models;
		vm::list::sync(&m.scenes, &vm::studio::scenes(&self.studio.scenes));
		let stats = self.studio.stats.as_ref().map_or(&[][..], |s| &s.sources[..]);
		vm::list::sync(&m.sources, &vm::studio::sources(self.studio.scenes.active(), stats));
	}

	/// The mixer source of each audio row, and why one captures nothing.
	fn studio_map_meters(&mut self) {
		let sources = self.studio.settings.as_ref().map(|s| s.get(&STREAM_AUDIO_SOURCES));
		let streamer = self.studio_streamer();
		let mixer = streamer.and_then(Streamer::audio_mixer);
		let stats = self.studio.encoder.as_ref().map(|s| &s.audio_sources[..]).unwrap_or_default();
		self.studio.meters = sources
			.unwrap_or_default()
			.iter()
			.map(|setting| {
				let kind = AudioSourceSpec::from(setting).kind;
				let stat = stats.iter().find(|s| s.spec.kind == kind)?;
				mixer.as_ref()?.source(stat.id)
			})
			.collect();
	}

	fn studio_refresh_audio(&self) {
		let Some(settings) = &self.studio.settings else { return };
		let sources = settings.get(&STREAM_AUDIO_SOURCES);
		let stats = self.studio.encoder.as_ref().map(|s| &s.audio_sources[..]).unwrap_or_default();
		let live: Vec<Option<vm::studio::AudioLive>> = sources
			.iter()
			.enumerate()
			.map(|(i, setting)| {
				let kind = AudioSourceSpec::from(setting).kind;
				let error =
					stats.iter().find(|s| s.spec.kind == kind).and_then(|s| s.error.clone());
				let level = self.studio.meters.get(i).cloned().flatten()?.level().peak_db();
				Some(vm::studio::AudioLive { level: level.max(-100.0), error })
			})
			.collect();
		vm::list::sync(&self.studio.models.audio, &vm::studio::audio(&sources, &live));
	}

	/// The meters, about 15 times a second (only rows that moved redraw).
	fn studio_meters(&mut self) {
		let model = &self.studio.models.audio;
		for (i, meter) in self.studio.meters.iter().enumerate() {
			let level = meter.as_ref().map_or(-100.0, |m| m.level().peak_db().max(-100.0));
			if let Some(mut row) = model.row_data(i)
				&& (row.level - level).abs() >= 0.5
			{
				row.level = level;
				model.set_row_data(i, row);
			}
		}
	}

	fn studio_refresh_layers(&mut self) {
		let settings = self.studio_settings();
		vm::list::sync(
			&self.studio.models.layers,
			&vm::studio::layers(&settings.get(&STREAM_LAYERS)),
		);
	}

	/// Sessions the stream can go to: TeamSpeak 6 with voice, the current
	/// one first.
	fn studio_destinations(&self) -> Vec<(i64, String)> {
		let mut ids: Vec<i64> = self
			.sessions
			.iter()
			.filter(|(_, v)| v.streams_available() && v.state.own_channel.is_some())
			.map(|(id, _)| *id)
			.collect();
		ids.sort_by_key(|id| (Some(*id) != self.current, *id));
		ids.into_iter().map(|id| (id, self.studio_destination_name(id))).collect()
	}

	/// The channel and the server of a destination.
	fn studio_destination_parts(&self, id: i64) -> (String, String) {
		let Some(view) = self.sessions.get(&id) else { return Default::default() };
		let channel = view
			.state
			.own_channel
			.and_then(|c| view.presence.channels.get(&c))
			.map(|c| c.name.clone())
			.unwrap_or_default();
		let server = if view.presence.server_name.is_empty() {
			self.bookmark(id).map(|b| b.name.clone()).unwrap_or_default()
		} else {
			view.presence.server_name.clone()
		};
		(channel, server)
	}

	/// "Chill Zone • Nightfall Guild".
	fn studio_destination_name(&self, id: i64) -> String {
		let (channel, server) = self.studio_destination_parts(id);
		format!("{channel} • {server}")
	}

	fn studio_refresh_destinations(&mut self) {
		let list = self.studio_destinations();
		// Keep the destination while it works (and while live).
		let keep = self.studio.stream.as_ref().map(|s| s.session).or(self.studio.destination);
		self.studio.destination =
			keep.filter(|id| list.iter().any(|(d, _)| d == id)).or_else(|| {
				self.studio.stream.as_ref().map(|s| s.session).or(list.first().map(|(id, _)| *id))
			});
		let rows: Vec<StudioPick> = list
			.iter()
			.map(|(id, _)| {
				let (channel, server) = self.studio_destination_parts(*id);
				StudioPick { name: channel.into(), detail: server.into(), kind: "server".into() }
			})
			.collect();
		vm::list::sync(&self.studio.models.destinations, &rows);
		self.studio_bind_chat();
		self.studio_refresh_form();
	}

	/// The session and channel the stream (and its chat) goes to.
	fn studio_chat_target(&self) -> Option<(i64, u64)> {
		let id = self.studio.destination?;
		Some((id, self.sessions.get(&id)?.state.own_channel?))
	}

	/// Show the destination channel's chat (its tab opened, with its
	/// history, if need be).
	fn studio_bind_chat(&mut self) {
		let name = match self.studio_chat_target() {
			Some((id, channel)) => {
				let target = ChatTarget::Channel(channel);
				let demo = self.demo_ui;
				let view = self.sessions.entry(id).or_default();
				let name = view.presence.channels.get(&channel).map(|c| c.name.clone());
				let name = name.unwrap_or_default();
				if !view.tabs.iter().any(|t| t.target == target) {
					view.tabs.push(Tab::new(target.clone(), format!("#{name}")));
					if !demo {
						self.engine.send(Command::OpenChat { session: id as u64, target });
					}
				}
				name
			}
			None => String::new(),
		};
		self.studio_each(|b| b.set_chat_name(name.clone().into()));
		self.studio_sync_chat();
	}

	/// The stream chat's lines: the destination tab's, made as the chat
	/// view makes them. The main window keeps only its shown tab's lines up
	/// to date, and the stream's channel need not be that tab.
	fn studio_sync_chat(&self) {
		let tab = self.studio_chat_target().and_then(|(id, channel)| {
			let view = self.sessions.get(&id)?;
			let tab = view.tabs.iter().find(|t| t.target == ChatTarget::Channel(channel))?;
			Some((view, tab))
		});
		let lines: Vec<ChatLine> = match tab {
			Some((view, tab)) if view.has_history() => self.lines_of(view, tab, &tab.messages),
			// Without stored history the tab's lines are pushed as they come.
			Some((_, tab)) => tab.lines.iter().collect(),
			None => Vec::new(),
		};
		vm::list::sync(&self.studio.models.chat, &lines);
	}

	/// A chat of session `id` changed (chat.rs): follow it if it is the
	/// stream's.
	pub(crate) fn studio_chat_changed(&mut self, id: i64, target: &ChatTarget) {
		if self.studio.run.is_some()
			&& let Some((session, channel)) = self.studio_chat_target()
			&& session == id
			&& *target == ChatTarget::Channel(channel)
		{
			self.studio_sync_chat();
			self.studio_overlays(false);
		}
	}

	fn studio_refresh_form(&self) {
		let ui = &self.studio.ui;
		let list = self.studio_destinations();
		let destination = self
			.studio
			.destination
			.and_then(|id| list.iter().position(|(d, _)| *d == id))
			.map_or(-1, |i| i as i32);
		let bitrate = self.studio.settings.as_ref().map_or(0, |s| s.get(&STREAM_BITRATE_KBPS));
		let form = StudioForm {
			destination,
			title: ui.title.clone().into(),
			game: ui.game.clone().into(),
			message: ui.message.clone().into(),
			show_viewers: ui.show_viewers,
			show_chat: ui.show_chat,
			show_now_playing: ui.show_now_playing,
			audio: ui.audio,
			bitrate: bitrate.to_string().into(),
		};
		self.studio_each(|b| {
			if b.get_form() != form {
				b.set_form(form.clone());
			}
		});
	}

	/// The header, the preview's badges and the bottom bar.
	fn studio_refresh_status(&self) {
		let st = &self.studio;
		let live = st.stream.as_ref().is_some_and(|s| s.live);
		let starting = st.stream.as_ref().is_some_and(|s| !s.live);
		let running = st.run.is_some();
		let encoder = st.encoder.as_ref();
		let (w, h) = st.scenes.size();
		let fps = st.scenes.fps();
		let settings = st.settings.as_ref();
		// What is sent: measured, else as set.
		let measured: f64 = encoder.map_or(0.0, |e| e.layers.iter().map(|l| l.kbps).sum());
		let layers = settings.map(|s| s.get(&STREAM_LAYERS)).unwrap_or_default();
		let configured_bps: u64 = if layers.is_empty() {
			settings.map_or(0, |s| u64::from(s.get(&STREAM_BITRATE_KBPS)) * 1000)
		} else {
			layers.iter().map(|l| l.bitrate).max().unwrap_or(0)
		};
		let kbps = if measured > 0.0 { measured.round() as u64 } else { configured_bps / 1000 };
		// The viewers' bandwidth against the top layer's bitrate.
		let bandwidth = st
			.stream
			.as_ref()
			.and_then(|s| {
				s.viewers
					.iter()
					.filter(|v| v.state == ViewerState::Connected)
					.filter_map(|v| v.estimate)
					.min()
			})
			.filter(|_| configured_bps > 0)
			.map(|e| e as f64 / configured_bps as f64);
		let composed = st.stats.as_ref().map_or(f64::from(fps), |s| s.fps);
		let (quality, quality_text) = vm::studio::quality(live, fps, composed, bandwidth);
		let viewers = st.stream.as_ref().map_or(&[][..], |s| &s.viewers[..]);
		let watching = viewers.iter().filter(|v| v.state == ViewerState::Connected).count();
		let destination = st.destination.map(|id| self.studio_destination_name(id));
		let sharing =
			st.destination.is_some_and(|d| self.share.as_ref().is_some_and(|s| s.session == d));
		let has_encoder = st.run.as_ref().is_some_and(|r| r.streamer.is_some());
		let hint = if !st.error.is_empty() {
			st.error.clone()
		} else if !running || !has_encoder {
			"Starting the studio…".to_owned()
		} else if destination.is_none() {
			"Join a voice channel on a TeamSpeak 6 server to go live.".to_owned()
		} else if sharing {
			"Stop sharing your screen first.".to_owned()
		} else if starting {
			"Waiting for the server…".to_owned()
		} else {
			String::new()
		};
		let replay = st.stats.as_ref().map(|s| &s.replay);
		let replay_seconds = settings.map_or(0, |s| s.get(&STUDIO_REPLAY_SECONDS));
		let mut details = vec![format!(
			"Composite {w}×{h}: {composed:.1} of {fps} fps, {:.1} ms a frame",
			st.stats.as_ref().map_or(0.0, |s| s.compose.compose_time.as_secs_f64() * 1000.0)
		)];
		if let Some(e) = encoder {
			let codec = e.codec.map(|c| c.to_string()).unwrap_or_default();
			let backend = e.layers.first().and_then(|l| l.backend).map(|b| format!(" ({b})"));
			details.push(format!(
				"Encoder {codec}{}: {} layer(s), {} kbit/s",
				backend.unwrap_or_default(),
				e.layers.len(),
				vm::studio::thousands(kbps)
			));
		}
		if let Some(r) = replay.filter(|_| replay_seconds > 0) {
			details.push(format!(
				"Replay buffer: {} s held, {:.1} MB",
				r.duration.as_secs(),
				(r.memory_bytes + r.spilled_bytes) as f64 / 1e6
			));
		}
		let scene = st.scenes.active();
		let shows = |f: fn(&SourceKind) -> bool| {
			scene.is_some_and(|s| s.sources.iter().any(|src| src.visible && f(&src.kind)))
		};
		let status = StudioStatus {
			running,
			destination: destination.clone().unwrap_or_default().into(),
			quality,
			quality_text: quality_text.into(),
			format: vm::studio::format_line(h, fps, kbps).into(),
			details: details.join("\n").into(),
			live,
			starting,
			elapsed: vm::studio::clock(st.status.live_for).into(),
			viewers: watching as i32,
			participants: viewers.len() as i32,
			can_go_live: hint.is_empty() && !live && !starting,
			hint: hint.into(),
			recording: st.status.recording.is_some(),
			recorded: vm::studio::clock(st.status.recorded_for).into(),
			replay_seconds: replay_seconds as i32,
			replay_held: replay
				.map_or(String::new(), |r| format!("{} s", r.duration.as_secs()))
				.into(),
			output_width: w as i32,
			output_height: h as i32,
			fps: fps as i32,
			detached: st.window.is_some(),
			can_detach: cfg!(not(target_os = "android")),
			screen_shown: shows(|k| {
				matches!(
					k,
					SourceKind::Screen { .. }
						| SourceKind::Portal { .. }
						| SourceKind::Pattern { .. }
				)
			}),
			camera_shown: shows(|k| matches!(k, SourceKind::Camera { .. })),
			window_shown: shows(|k| matches!(k, SourceKind::Window { .. })),
		};
		let requests: Vec<StudioViewer> = viewers
			.iter()
			.filter(|v| v.state == ViewerState::Requested)
			.map(|v| StudioViewer {
				client: i32::from(v.client.0),
				name: self.studio_viewer_name(v).into(),
				state: "requested".into(),
			})
			.collect();
		vm::list::sync(&self.studio.models.viewers, &requests);
		self.studio_each(|b| {
			if b.get_status() != status {
				b.set_status(status.clone());
			}
		});
	}

	fn studio_viewer_name(&self, viewer: &ViewerInfo) -> String {
		let session = self.studio.stream.as_ref().map(|s| s.session);
		session
			.and_then(|id| self.sessions.get(&id))
			.map_or_else(|| format!("client {}", viewer.client.0), |v| v.nickname(viewer.client.0))
	}

	/// Keep the overlay sources as the switches say (`force`: also when
	/// nothing changed, e.g. after a scene switch).
	fn studio_overlays(&mut self, force: bool) {
		let ui = &self.studio.ui;
		let watching =
			self.studio.stream.as_ref().map_or(0, |s| {
				s.viewers.iter().filter(|v| v.state == ViewerState::Connected).count()
			});
		let chat = ui.show_chat.then(|| self.studio_chat_tail(5)).flatten();
		let wanted = vec![
			Overlay {
				name: OVERLAY_VIEWERS,
				text: ui.show_viewers.then(|| format!("{watching} watching")),
			},
			Overlay { name: OVERLAY_CHAT, text: chat },
			Overlay {
				name: OVERLAY_NOW_PLAYING,
				text: ui.show_now_playing.then(|| now_playing(ui)).flatten(),
			},
		];
		if force || wanted != self.studio.overlays {
			self.studio.overlays = wanted.clone();
			self.studio_op(Op::Overlays(wanted));
		}
	}

	/// The last lines of the stream's chat, for the chat overlay.
	fn studio_chat_tail(&self, lines: usize) -> Option<String> {
		let chat = &self.studio.models.chat;
		let count = chat.row_count();
		let text: Vec<String> = (count.saturating_sub(lines)..count)
			.filter_map(|i| chat.row_data(i))
			.map(|l| {
				let line = format!("{}: {}", l.author, l.text);
				match line.char_indices().nth(64) {
					Some((at, _)) => format!("{}…", &line[..at]),
					None => line,
				}
			})
			.collect();
		(!text.is_empty()).then(|| text.join("\n"))
	}

	// Following the settings.

	/// `stream.audio_sources` (or the stream audio switch) changed.
	pub(crate) fn studio_audio_changed(&mut self) {
		let settings = self.studio_settings();
		let mut specs = audio_source_specs(&settings.get(&STREAM_AUDIO_SOURCES));
		if !self.studio.ui.audio {
			specs.clear();
		}
		let codecs = self.video.codecs();
		let result =
			self.studio_streamer().filter(|s| s.audio_sources() != specs).map(|streamer| {
				let update =
					StreamerConfigUpdate { audio_sources: Some(specs), ..Default::default() };
				streamer.reconfigure(&codecs, update)
			});
		if result.is_some() {
			self.studio.encoder = self.studio_streamer().map(Streamer::stats);
		}
		if let Some(Err(e)) = result {
			self.set_status(format!("Stream audio: {e}"));
		}
		self.studio_map_meters();
		self.studio_refresh_audio();
	}

	/// `stream.layers` or `stream.bitrate_kbps` changed.
	pub(crate) fn studio_encoding_changed(&mut self) {
		let settings = self.studio_settings();
		let update = StreamerConfigUpdate {
			bitrate_kbps: Some(settings.get(&STREAM_BITRATE_KBPS)),
			layers: Some(layer_specs(&settings.get(&STREAM_LAYERS))),
			..Default::default()
		};
		let codecs = self.video.codecs();
		let result = self.studio_streamer().map(|streamer| streamer.reconfigure(&codecs, update));
		if let Some(Err(e)) = result {
			self.set_status(format!("Stream quality: {e}"));
		}
		self.studio_refresh_layers();
		self.studio_refresh_form();
		self.studio_refresh_status();
	}

	/// `studio.ui` changed elsewhere (another window, `--set`).
	pub(crate) fn studio_ui_changed(&mut self) {
		let settings = self.studio_settings();
		let ui = (*settings.get_arc(&STUDIO_UI)).clone();
		if ui == self.studio.ui {
			return;
		}
		let (audio, preview) = (
			ui.audio != self.studio.ui.audio,
			(ui.preview_width, ui.preview_fps)
				!= (self.studio.ui.preview_width, self.studio.ui.preview_fps),
		);
		self.studio.ui = ui;
		if audio {
			self.studio_audio_changed();
		}
		if preview {
			self.studio_preview_size();
		}
		self.studio_overlays(false);
		self.studio_refresh_form();
		self.studio_refresh_status();
	}

	fn studio_store_ui(&mut self, ui: StudioUi) {
		let settings = self.studio_settings();
		self.studio.ui = ui.clone();
		if let Err(e) = settings.set(&STUDIO_UI, ui) {
			self.set_status(e.to_string());
		}
	}

	// The stream settings panel.

	/// A field of the stream settings.
	pub(crate) fn studio_edit(&mut self, key: &str, value: &str) {
		let mut ui = self.studio.ui.clone();
		let on = value == "true";
		match key {
			"title" => ui.title = value.into(),
			"game" => ui.game = value.into(),
			"message" => ui.message = value.into(),
			"show-viewers" => ui.show_viewers = on,
			"show-chat" => ui.show_chat = on,
			"show-now-playing" => ui.show_now_playing = on,
			"audio" => ui.audio = on,
			"bitrate" => {
				let settings = self.studio_settings();
				match parse_positive(value) {
					Some(kbps) => {
						if let Err(e) = settings.set(&STREAM_BITRATE_KBPS, kbps) {
							self.set_status(e.to_string());
						}
					}
					None => self.set_status("The bitrate is a whole number of kbit/s above 0."),
				}
				return;
			}
			"destination" => {
				let list = self.studio_destinations();
				let index = value.parse::<usize>().ok();
				if self.studio.stream.is_none()
					&& let Some((id, _)) = index.and_then(|i| list.get(i))
				{
					self.studio.destination = Some(*id);
					self.studio_bind_chat();
					self.studio_refresh_form();
					self.studio_overlays(false);
					self.studio_refresh_status();
				}
				return;
			}
			_ => return,
		}
		let audio = ui.audio != self.studio.ui.audio;
		self.studio_store_ui(ui);
		if audio {
			self.studio_audio_changed();
		}
		self.studio_overlays(false);
		self.studio_refresh_form();
		self.studio_refresh_status();
	}

	/// The output's size and rate: presets or anything typed.
	pub(crate) fn studio_set_output(&mut self, width: &str, height: &str, fps: &str) {
		let (Some(width), Some(height), Some(fps)) =
			(parse_positive(width), parse_positive(height), parse_positive(fps))
		else {
			self.set_status("The output is a width, a height and a frame rate above 0.");
			return;
		};
		self.studio_op(Op::Apply(studio::Command::SetOutput { width, height, fps }));
		let codecs = self.video.codecs();
		let update = StreamerConfigUpdate { fps: Some(fps), ..Default::default() };
		let result = self.studio_streamer().map(|streamer| streamer.reconfigure(&codecs, update));
		if let Some(Err(e)) = result {
			self.set_status(format!("Stream quality: {e}"));
		}
	}

	pub(crate) fn studio_set_layer(&mut self, index: usize, row: &StudioLayer) {
		let settings = self.studio_settings();
		let mut layers = settings.get(&STREAM_LAYERS);
		let Some(old) = layers.get(index) else { return };
		match vm::studio::parse_layer(row, old) {
			Ok(layer) => layers[index] = layer,
			Err(e) => {
				self.set_status(format!("Layer {}: {e}", index + 1));
				self.studio_refresh_layers();
				return;
			}
		}
		if let Err(e) = settings.set(&STREAM_LAYERS, layers) {
			self.set_status(e.to_string());
		}
		self.studio_refresh_layers();
	}

	pub(crate) fn studio_add_layer(&mut self) {
		let settings = self.studio_settings();
		let mut layers = settings.get(&STREAM_LAYERS);
		let bitrate = settings.get(&STREAM_BITRATE_KBPS);
		if layers.is_empty() {
			layers.push(vm::studio::next_layer(&[], bitrate));
		}
		layers.push(vm::studio::next_layer(&layers, bitrate));
		if let Err(e) = settings.set(&STREAM_LAYERS, layers) {
			self.set_status(e.to_string());
		}
		self.studio_refresh_layers();
	}

	pub(crate) fn studio_remove_layer(&mut self, index: usize) {
		let settings = self.studio_settings();
		let mut layers = settings.get(&STREAM_LAYERS);
		if index < layers.len() {
			layers.remove(index);
			// One layer left is the single layer.
			if layers.len() == 1 {
				layers.clear();
			}
			if let Err(e) = settings.set(&STREAM_LAYERS, layers) {
				self.set_status(e.to_string());
			}
		}
		self.studio_refresh_layers();
	}

	/// Settings → Streaming in the main window.
	pub(crate) fn studio_advanced(&mut self) {
		let weak = self.ui.clone();
		// After this callback: opening the settings calls back into the app.
		let _ = slint::invoke_from_event_loop(move || {
			if let Some(ui) = weak.upgrade() {
				ui.global::<Nav>().invoke_open_settings(crate::app::SettingsSection::Streaming);
				let _ = ui.show();
			}
		});
	}

	// Scenes and sources.

	pub(crate) fn studio_scene_command(&mut self, command: studio::Command) {
		self.studio_op(Op::Apply(command));
	}

	pub(crate) fn studio_add_scene(&mut self) {
		let n = self.studio.scenes.scenes.len() + 1;
		self.studio_op(Op::Apply(studio::Command::AddScene { name: format!("Scene {n}") }));
	}

	fn studio_live_source(&self, id: u64) -> Option<&Source> {
		self.studio.scenes.active()?.source(id)
	}

	fn studio_live_scene(&self) -> Option<u64> {
		self.studio.scenes.active().map(|s| s.id)
	}

	fn studio_update(&self, source: u64, change: SourceChange) {
		let Some(scene) = self.studio_live_scene() else { return };
		let command = studio::Command::UpdateSource { scene, source, change: Box::new(change) };
		self.studio_op(Op::Apply(command));
	}

	pub(crate) fn studio_toggle_source(&mut self, id: u64) {
		if let Some(source) = self.studio_live_source(id) {
			let visible = Some(!source.visible);
			self.studio_update(id, SourceChange { visible, ..Default::default() });
		}
	}

	/// up, down, lock, remove, bg-keep, bg-blur.
	pub(crate) fn studio_source_action(&mut self, id: u64, action: &str) {
		let Some(scene) = self.studio.scenes.active() else { return };
		let Some(index) = scene.sources.iter().position(|s| s.id == id) else { return };
		let source = &scene.sources[index];
		let (scene, locked) = (scene.id, source.locked);
		match action {
			"up" | "down" => {
				let to = if action == "up" { index + 1 } else { index.saturating_sub(1) };
				self.studio_op(Op::Apply(studio::Command::ReorderSource { scene, source: id, to }));
			}
			"lock" => {
				self.studio_update(id, SourceChange { locked: Some(!locked), ..Default::default() })
			}
			"remove" => {
				self.studio_op(Op::Apply(studio::Command::RemoveSource { scene, source: id }));
			}
			"bg-keep" | "bg-blur" => {
				let background = if action == "bg-blur" {
					Background::Blur { strength: 0.04 }
				} else {
					Background::Keep
				};
				self.studio_update(
					id,
					SourceChange { background: Some(background), ..Default::default() },
				);
			}
			_ => {}
		}
	}

	pub(crate) fn studio_source_form(&self, id: u64) -> StudioSourceForm {
		self.studio_live_source(id).map(vm::studio::form).unwrap_or_default()
	}

	pub(crate) fn studio_apply_source(&mut self, form: &StudioSourceForm) {
		let Ok(id) = u64::try_from(form.id) else { return };
		let Some(old) = self.studio_live_source(id) else { return };
		match vm::studio::change(form, old) {
			Ok(mut change) => {
				// A locked source keeps its place unless it is unlocked.
				if old.locked
					&& form.locked && (change.transform.is_some() || change.crop.is_some())
				{
					change.transform = None;
					change.crop = None;
					self.set_status("Unlock the source to move, resize or crop it.");
				}
				self.studio_update(id, change);
			}
			Err(e) => self.set_status(e),
		}
	}

	/// A text, colour or image source from the dialog.
	pub(crate) fn studio_add_source(&mut self, form: &StudioSourceForm) {
		match vm::studio::new_source(form, self.studio.scenes.size()) {
			Ok(source) => self.studio_op(Op::Add(source)),
			Err(e) => self.set_status(e),
		}
	}

	fn studio_set_picks(&mut self, picks: Vec<(Pick, StudioPick)>) {
		let rows: Vec<StudioPick> = picks.iter().map(|(_, row)| row.clone()).collect();
		self.studio.picks = picks.into_iter().map(|(pick, _)| pick).collect();
		vm::list::sync(&self.studio.models.picks, &rows);
	}

	/// What a screen, window or camera source can show. False when there is
	/// nothing to pick from: the one choice (the desktop's own dialog) was
	/// added at once.
	pub(crate) fn studio_list_sources(&mut self, kind: &str) -> bool {
		let row = |name: &str, detail: String, icon: &str| StudioPick {
			name: name.into(),
			detail: detail.into(),
			kind: icon.into(),
		};
		let mut picks: Vec<(Pick, StudioPick)> = Vec::new();
		let source = |name: &str, kind: SourceKind| Pick::Source { name: name.into(), kind };
		match kind {
			"camera" => {
				// The demo never opens a device, not even to list it.
				let cameras = if self.demo_ui {
					vec![(
						camera::SYNTHETIC.to_owned(),
						"Test pattern".to_owned(),
						"1280×720".to_owned(),
					)]
				} else {
					camera::list()
						.into_iter()
						.map(|c| {
							let biggest =
								c.formats.iter().flat_map(|f| &f.sizes).max_by_key(|(w, h)| w * h);
							let detail =
								biggest.map_or(String::new(), |(w, h)| format!("up to {w}×{h}"));
							(c.id, c.name, detail)
						})
						.collect()
				};
				for (device, name, detail) in cameras {
					let kind = SourceKind::Camera { device, size: None, fps: None, mirror: true };
					picks.push((source(&name, kind), row(&name, detail, "camera")));
				}
			}
			"screen" | "window" => {
				let listed = if self.demo_ui { Ok(Vec::new()) } else { screen_sources() };
				match listed {
					Ok(list) => {
						for s in list {
							let size = if s.width > 0 {
								format!("{}×{}", s.width, s.height)
							} else {
								String::new()
							};
							match s.id {
								SourceId::Monitor(monitor) if kind == "screen" => {
									let detail =
										if s.primary { format!("{size} · primary") } else { size };
									let k =
										SourceKind::Screen { monitor, backend: None, cursor: true };
									picks.push((
										source(&s.name, k),
										row(&s.name, detail, "monitor"),
									));
								}
								SourceId::Window(handle) if kind == "window" => {
									let k =
										SourceKind::Window { handle, backend: None, cursor: true };
									picks.push((source(&s.name, k), row(&s.name, size, "window")));
								}
								SourceId::Portal => {
									let k =
										SourceKind::Portal { restore_token: None, cursor: true };
									let name = if kind == "screen" { "Screen" } else { "Window" };
									let detail = "Your desktop asks what to share".to_owned();
									picks.push((
										source(name, k),
										row("Choose with the system dialog", detail, "portal"),
									));
								}
								_ => {}
							}
						}
					}
					Err(e) => self.set_status(format!("Cannot list what to share: {e}")),
				}
				if self.demo_ui || cfg!(debug_assertions) {
					let k = SourceKind::Pattern { size: (1280, 720) };
					picks.push((
						source("Test pattern", k),
						row("Test pattern", "1280×720".into(), "pattern"),
					));
				}
			}
			_ => {}
		}
		if picks.len() == 1
			&& matches!(&picks[0].0, Pick::Source { kind: SourceKind::Portal { .. }, .. })
		{
			self.studio_set_picks(picks);
			self.studio_add_picked(0);
			return false;
		}
		self.studio_set_picks(picks);
		true
	}

	pub(crate) fn studio_add_picked(&mut self, index: usize) {
		let Some(Pick::Source { name, kind }) = self.studio.picks.get(index) else { return };
		let mut source = Source::new(0, kind.clone());
		source.name.clone_from(name);
		source.transform = vm::studio::placement(kind, self.studio.scenes.size());
		self.studio_op(Op::Add(source));
	}

	/// The phone's source cards: show a screen, a camera or a window (and
	/// hide the other two kinds); add one if the live scene has none.
	pub(crate) fn studio_main_source(&mut self, kind: &str) {
		let wanted = |k: &SourceKind| match kind {
			"screen" => matches!(
				k,
				SourceKind::Screen { .. } | SourceKind::Portal { .. } | SourceKind::Pattern { .. }
			),
			"camera" => matches!(k, SourceKind::Camera { .. }),
			_ => matches!(k, SourceKind::Window { .. }),
		};
		let main = |k: &SourceKind| {
			matches!(
				k,
				SourceKind::Screen { .. }
					| SourceKind::Portal { .. }
					| SourceKind::Pattern { .. }
					| SourceKind::Camera { .. }
					| SourceKind::Window { .. }
			)
		};
		let sources: Vec<(u64, bool, bool, bool)> = self
			.studio
			.scenes
			.active()
			.map(|s| {
				s.sources
					.iter()
					.map(|s| (s.id, wanted(&s.kind), main(&s.kind), s.visible))
					.collect()
			})
			.unwrap_or_default();
		for (id, is_wanted, is_main, visible) in &sources {
			let show = *is_wanted;
			if *is_main && show != *visible {
				self.studio_update(*id, SourceChange { visible: Some(show), ..Default::default() });
			}
		}
		if !sources.iter().any(|s| s.1) && self.studio_list_sources(kind) {
			if self.studio.picks.len() == 1 {
				self.studio_add_picked(0);
			} else {
				self.studio_nav(|nav| {
					nav.set_pick_kind(kind.into());
					nav.set_dialog("pick".into());
				});
			}
		}
	}

	// The audio mixer.

	/// What can be added to the mixer.
	pub(crate) fn studio_list_audio(&mut self) {
		let settings = self.studio_settings();
		let current = settings.get(&STREAM_AUDIO_SOURCES);
		let mut picks: Vec<(Pick, StudioPick)> = Vec::new();
		let mut add = |setting: AudioSourceSetting, name: String, detail: String| {
			let (_, _, kind) = vm::studio::audio_label(&setting.kind);
			let row = StudioPick { name: name.into(), detail: detail.into(), kind: kind.into() };
			picks.push((Pick::Audio(setting), row));
		};
		let fixed = [
			(AudioSourceKindSetting::Microphone, "Microphone", "What you say in voice, cleaned up"),
			(
				AudioSourceKindSetting::Desktop,
				"Desktop audio",
				"Everything that plays, without Voelin",
			),
			(
				AudioSourceKindSetting::Window,
				"Audio of the shared window",
				"Its application (X11 and Windows)",
			),
		];
		for (kind, name, detail) in fixed {
			if !current.iter().any(|s| s.kind == kind) {
				add(AudioSourceSetting::new(kind), name.into(), detail.into());
			}
		}
		if self.demo_ui {
			for frequency in [330, 550] {
				let kind = AudioSourceKindSetting::Synthetic { frequency };
				add(AudioSourceSetting::new(kind), "Test tone".into(), format!("{frequency} Hz"));
			}
		} else {
			if self.studio.apps.is_none() {
				match audio_apps() {
					Ok(apps) => self.studio.apps = Some(apps),
					Err(e) => warn!("no application list: {e}"),
				}
			}
			let apps = self.studio.apps.as_mut().map(AudioApps::current).unwrap_or_default();
			for app in apps {
				let name = app.binary.clone().unwrap_or_else(|| app.name.clone());
				let kind = AudioSourceKindSetting::App { name: Some(name), pid: None };
				let detail = app
					.media
					.clone()
					.unwrap_or_else(|| if app.playing { "playing".into() } else { String::new() });
				add(AudioSourceSetting::new(kind), app.name, detail);
			}
		}
		self.studio_set_picks(picks);
	}

	fn studio_audio_sources(&mut self, f: impl FnOnce(&mut Vec<AudioSourceSetting>)) {
		let settings = self.studio_settings();
		let mut sources = settings.get(&STREAM_AUDIO_SOURCES);
		f(&mut sources);
		if let Err(e) = settings.set(&STREAM_AUDIO_SOURCES, sources) {
			self.set_status(e.to_string());
		}
		self.studio_refresh_audio();
	}

	pub(crate) fn studio_add_audio(&mut self, index: usize) {
		let Some(Pick::Audio(setting)) = self.studio.picks.get(index) else { return };
		let setting = setting.clone();
		self.studio_audio_sources(|sources| sources.push(setting));
	}

	/// While dragging the gain goes to the mixer directly; at the end it is
	/// stored (and the streamer follows).
	pub(crate) fn studio_set_gain(&mut self, index: usize, db: f32, done: bool) {
		let gain = vm::studio::gain_linear(db);
		if let Some(Some(meter)) = self.studio.meters.get(index) {
			meter.set_gain(gain);
		}
		let model = &self.studio.models.audio;
		if let Some(mut row) = model.row_data(index) {
			row.gain_db = db;
			model.set_row_data(index, row);
		}
		if done {
			self.studio_audio_sources(|sources| {
				if let Some(s) = sources.get_mut(index) {
					s.gain = gain;
				}
			});
		}
	}

	pub(crate) fn studio_toggle_mute(&mut self, index: usize) {
		self.studio_audio_sources(|sources| {
			if let Some(s) = sources.get_mut(index) {
				s.muted = !s.muted;
			}
		});
	}

	pub(crate) fn studio_remove_audio(&mut self, index: usize) {
		self.studio_audio_sources(|sources| {
			if index < sources.len() {
				sources.remove(index);
			}
		});
	}

	// Recording, clips, the studio settings.

	fn studio_recordings(&mut self) -> Option<PathBuf> {
		let dir = studio::recording_dir(&self.studio_settings());
		match std::fs::create_dir_all(&dir) {
			Ok(()) => Some(dir),
			Err(e) => {
				self.set_status(format!("Cannot use {}: {e}", dir.display()));
				None
			}
		}
	}

	pub(crate) fn studio_save_clip(&mut self) {
		if self.studio_settings().get(&STUDIO_REPLAY_SECONDS) == 0 {
			self.set_status("The replay buffer is off: turn it on in the Studio Settings.");
			return;
		}
		if let Some(dir) = self.studio_recordings() {
			let path = dir.join(file_name("Clip"));
			self.studio_op(Op::Apply(studio::Command::SaveClip { path }));
		}
	}

	pub(crate) fn studio_toggle_recording(&mut self) {
		if self.studio.status.recording.is_some() {
			self.studio_op(Op::Apply(studio::Command::StopRecording));
		} else if let Some(dir) = self.studio_recordings() {
			let path = dir.join(file_name("Recording"));
			self.studio_op(Op::Apply(studio::Command::StartRecording { path }));
		}
	}

	pub(crate) fn studio_open_recordings(&mut self) {
		if let Some(dir) = self.studio_recordings()
			&& let Err(e) = crate::app::open_path(&dir)
		{
			self.set_status(format!("Cannot open {}: {e}", dir.display()));
		}
	}

	pub(crate) fn studio_settings_form(&mut self) -> StudioSettingsForm {
		let settings = self.studio_settings();
		StudioSettingsForm {
			replay_seconds: settings.get(&STUDIO_REPLAY_SECONDS).to_string().into(),
			replay_memory: settings.get(&STUDIO_REPLAY_MEMORY_MB).to_string().into(),
			recording_dir: settings.get(&STUDIO_RECORDING_DIR).into(),
			preview_width: self.studio.ui.preview_width.to_string().into(),
			preview_fps: self.studio.ui.preview_fps.to_string().into(),
		}
	}

	pub(crate) fn studio_apply_settings(&mut self, form: &StudioSettingsForm) {
		let settings = self.studio_settings();
		let number = |text: &str| text.trim().parse::<u64>().ok();
		let mut errors = Vec::new();
		match number(&form.replay_seconds).and_then(|v| u32::try_from(v).ok()) {
			Some(v) => errors.extend(settings.set(&STUDIO_REPLAY_SECONDS, v).err()),
			None => self.set_status("The replay buffer is a whole number of seconds."),
		}
		match number(&form.replay_memory) {
			Some(v) => errors.extend(settings.set(&STUDIO_REPLAY_MEMORY_MB, v).err()),
			None => self.set_status("The replay memory is a whole number of MB."),
		}
		errors.extend(
			settings.set(&STUDIO_RECORDING_DIR, form.recording_dir.trim().to_owned()).err(),
		);
		let mut ui = self.studio.ui.clone();
		if let (Some(width), Some(fps)) =
			(parse_positive(&form.preview_width), parse_positive(&form.preview_fps))
		{
			ui.preview_width = width;
			ui.preview_fps = fps;
		}
		if ui != self.studio.ui {
			self.studio_store_ui(ui);
			self.studio_preview_size();
		}
		if let Some(e) = errors.first() {
			self.set_status(e.to_string());
		}
		self.studio_refresh_status();
	}

	// Going live.

	pub(crate) fn studio_go_live(&mut self) {
		let Some(session) = self.studio.destination else { return };
		if self.studio.stream.is_some() || self.studio_streamer().is_none() {
			return;
		}
		if self.share.as_ref().is_some_and(|s| s.session == session) {
			self.set_status("Stop sharing your screen first.");
			return;
		}
		self.studio.stream = Some(Outgoing { session, live: false, viewers: Vec::new() });
		if self.demo_ui {
			self.studio_demo_live();
			return;
		}
		let settings = self.studio_settings();
		let layers = settings.get(&STREAM_LAYERS);
		let bitrate = match layers.iter().map(|l| l.bitrate).max() {
			Some(bps) => u32::try_from(bps / 1000).unwrap_or(u32::MAX),
			None => settings.get(&STREAM_BITRATE_KBPS),
		};
		let setup = StreamSetup {
			name: stream_name(&self.studio.ui),
			bitrate,
			audio: self.studio.ui.audio,
			..StreamSetup::default()
		};
		let auto_accept = self.settings.share.auto_accept;
		self.engine.send(Command::StartStream { session: session as u64, setup, auto_accept });
		self.studio_refresh_status();
	}

	/// Whether `session` is where the studio streams to (app.rs routes its
	/// stream events here).
	pub(crate) fn studio_streams_to(&self, session: i64) -> bool {
		self.studio.stream.as_ref().is_some_and(|s| s.session == session)
	}

	pub(crate) fn studio_stream_state(&mut self, state: StreamState) {
		match state {
			StreamState::Starting => {}
			StreamState::Live { sink, .. } => {
				if let Some(streamer) = self.studio_streamer() {
					streamer.attach(Arc::new(sink));
				}
				self.studio_now_live();
			}
			StreamState::Ended(reason) => self.studio_ended(Some(&reason)),
		}
	}

	/// The stream is up: the studio's LIVE, the go-live message.
	fn studio_now_live(&mut self) {
		let Some(out) = &mut self.studio.stream else { return };
		out.live = true;
		self.studio_op(Op::Apply(studio::Command::GoLive));
		let message = self.studio.ui.message.trim().to_owned();
		if !message.is_empty() {
			self.studio_send_chat(message);
		}
		self.set_status("You are live");
		self.studio_overlays(false);
		self.studio_refresh_status();
	}

	pub(crate) fn studio_viewers(&mut self, viewers: Vec<ViewerInfo>) {
		if let Some(out) = &mut self.studio.stream {
			out.viewers = viewers;
		}
		self.studio_overlays(false);
		self.studio_refresh_status();
	}

	pub(crate) fn studio_end(&mut self) {
		let Some(out) = &self.studio.stream else { return };
		if self.demo_ui {
			self.studio_ended(None);
		} else {
			self.engine.send(Command::StopStream { session: out.session as u64 });
		}
	}

	fn studio_ended(&mut self, reason: Option<&EndReason>) {
		if let Some(streamer) = self.studio_streamer() {
			streamer.detach();
		}
		self.studio_op(Op::Apply(studio::Command::EndStream));
		self.studio.stream = None;
		self.set_status(match reason {
			Some(EndReason::Failed(e)) => format!("The stream failed: {e}"),
			_ => "The stream has ended".into(),
		});
		self.studio_overlays(false);
		self.studio_refresh_status();
	}

	pub(crate) fn studio_respond_viewer(&mut self, client: u16, accept: bool) {
		let Some(out) = &mut self.studio.stream else { return };
		if self.demo_ui {
			for v in out.viewers.iter_mut().filter(|v| v.client.0 == client) {
				v.state = ViewerState::Connected;
			}
			out.viewers.retain(|v| accept || v.client.0 != client);
			self.studio_refresh_status();
			return;
		}
		let session = out.session as u64;
		self.engine.send(Command::AcceptViewer { session, viewer: client, accept });
	}

	pub(crate) fn studio_send_chat(&mut self, text: String) {
		let Some((id, channel)) = self.studio_chat_target() else { return };
		let target = ChatTarget::Channel(channel);
		// Sample data has no engine to echo the message: shown as it would be.
		if self.demo_ui {
			let view = self.sessions.entry(id).or_default();
			let (client, history) = (view.state.own_client, view.has_history());
			let author = self.bookmark(id).map(|b| b.nickname.clone()).unwrap_or_default();
			let ts_ms = chrono::Utc::now().timestamp_millis();
			let message = ChatMessage {
				target: target.clone(),
				author_name: author,
				author_uid: client.map(|c| format!("demo-{c}")),
				author_id: client,
				text,
				ts_ms,
				via_relay: false,
				blocked: false,
			};
			if history {
				let stored = HistoryMessage {
					// Negative: in memory only.
					id: -ts_ms,
					message,
					source: MessageSource::Voice,
					remote_id: None,
					topic_id: None,
					reactions: Vec::new(),
					pinned: false,
					rev: 0,
				};
				self.history_batch(id, &target, vec![stored], HistorySource::Live, false);
			} else {
				self.add_message(id, message);
			}
		} else {
			self.engine.send(Command::SendChat { session: id as u64, target, text });
		}
	}

	/// The destination's chat in the main window.
	pub(crate) fn studio_open_chat(&mut self) {
		let Some((id, channel)) = self.studio_chat_target() else { return };
		if self.current != Some(id) {
			self.select_server(id);
		}
		self.open_chat(ChatTarget::Channel(channel), true);
		let weak = self.ui.clone();
		let _ = slint::invoke_from_event_loop(move || {
			if let Some(ui) = weak.upgrade() {
				ui.global::<Nav>().invoke_show(Page::Server);
				let _ = ui.show();
			}
		});
	}

	// The window of its own.

	pub(crate) fn studio_detach(&mut self) {
		// One window on Android.
		if cfg!(target_os = "android") {
			return;
		}
		if self.studio.window.is_none() {
			let window = match StudioWindow::new() {
				Ok(window) => window,
				Err(e) => {
					self.set_status(format!("Cannot open the studio window: {e}"));
					return;
				}
			};
			let bridge = window.global::<StudioBridge>();
			crate::bind::studio::wire(&bridge);
			self.studio_attach(&bridge);
			window.window().on_close_requested(|| {
				// After this event: the window goes away then.
				later(|app| app.studio_dock());
				slint::CloseRequestResponse::HideWindow
			});
			self.studio.window = Some(window);
			self.studio_theme();
			// The main window closes the studio's with it.
			if let Some(ui) = self.ui.upgrade() {
				ui.window().on_close_requested(|| {
					with_app(|app| {
						if let Some(window) = app.studio.window.take() {
							let _ = window.hide();
						}
					});
					slint::CloseRequestResponse::HideWindow
				});
				if ui.global::<Nav>().get_page() == Page::Studio {
					ui.global::<Nav>().set_page(Page::Server);
				}
			}
		}
		// The next picture goes to both windows with the flag.
		self.studio.has_preview = false;
		self.studio_open();
		self.studio_bind_chat();
	}

	/// The studio back into the main window.
	pub(crate) fn studio_dock(&mut self) {
		let Some(window) = self.studio.window.take() else { return };
		let _ = window.hide();
		self.studio_refresh_status();
		let weak = self.ui.clone();
		// Showing the page runs Nav code that calls back into the app.
		let _ = slint::invoke_from_event_loop(move || {
			if let Some(ui) = weak.upgrade() {
				ui.global::<Nav>().invoke_show(Page::Studio);
			}
		});
	}

	/// The studio window follows the main window's theme.
	pub(crate) fn studio_theme(&self) {
		let (Some(ui), Some(window)) = (self.ui.upgrade(), &self.studio.window) else { return };
		let (from, to) = (ui.global::<Theme>(), window.global::<Theme>());
		to.set_mode(from.get_mode());
		to.set_font_scale(from.get_font_scale());
		to.set_system_dark(from.get_system_dark());
	}

	/// A picture of the studio window (screenshots, dev.rs).
	pub(crate) fn studio_snapshot(&self) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
		self.studio.window.as_ref()?.window().take_snapshot().ok()
	}

	// Development switches and sample data (dev.rs).

	/// `VOELIN_OPEN=studio:<what>`: `window` (the window of its own), `live`,
	/// `record`, or a dialog: `source`, `audio`, `camera`, `scene`, `settings`.
	pub(crate) fn studio_dev(&mut self, what: &str) {
		match what {
			"" => {}
			"window" => self.studio_detach(),
			other => self.studio.dev.push(other.into()),
		}
	}

	/// The development switches that wait for the studio and its encoder.
	fn studio_dev_apply(&mut self) {
		for what in std::mem::take(&mut self.studio.dev) {
			match what.as_str() {
				"live" => {
					self.studio_refresh_destinations();
					self.studio_go_live();
				}
				"record" => self.studio_toggle_recording(),
				"source" => {
					let front =
						self.studio.scenes.active().and_then(|s| s.sources.first()).map(|s| s.id);
					let form = front.map(|id| self.studio_source_form(id)).unwrap_or_default();
					self.studio_nav(|nav| {
						nav.set_source_form(form);
						nav.set_dialog("source".into());
					});
				}
				"audio" | "camera" => {
					if what == "audio" {
						self.studio_list_audio();
					} else {
						self.studio_list_sources("camera");
					}
					self.studio_nav(|nav| {
						nav.set_pick_kind(what.as_str().into());
						nav.set_dialog("pick".into());
					});
				}
				"scene" => {
					let scene = self.studio.scenes.active().map(|s| (s.id as i32, s.name.clone()));
					let (id, name) = scene.unwrap_or_default();
					self.studio_nav(|nav| {
						nav.set_scene_id(id);
						nav.set_scene_name(name.into());
						nav.set_dialog("scene".into());
					});
				}
				"settings" => {
					let form = self.studio_settings_form();
					self.studio_nav(|nav| {
						nav.set_settings_form(form);
						nav.set_dialog("settings".into());
					});
				}
				other => eprintln!("VOELIN_OPEN: unknown studio switch {other:?}"),
			}
		}
	}

	/// VOELIN_DEMO_UI going live: no server, so the studio goes live at once
	/// with sample viewers.
	fn studio_demo_live(&mut self) {
		let viewer = |id: u16, state: ViewerState, estimate: u64| ViewerInfo {
			client: tsclientlib::ClientId(id),
			state,
			message: String::new(),
			layer: Some(0),
			estimate: Some(estimate),
			srtp_profile: None,
			codec: None,
		};
		if let Some(out) = &mut self.studio.stream {
			out.viewers = vec![
				viewer(3, ViewerState::Connected, 12_000_000),
				viewer(4, ViewerState::Connected, 9_500_000),
				viewer(9, ViewerState::Connected, 20_000_000),
				viewer(5, ViewerState::Connecting, 0),
				viewer(6, ViewerState::Requested, 0),
			];
		}
		self.studio_now_live();
	}
}
