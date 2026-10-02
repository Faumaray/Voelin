//! `voelinctl studio`: the Stream Studio without a UI. `run` plays a scene
//! file (the JSON of the `studio.scenes` setting) through the studio and the
//! streamer's encoder, and can save a preview picture, record, keep a replay
//! buffer and save it as a clip, and push to a WHIP service, all at once.
//! `bench` times the compositor alone; `cameras` lists what a scene's camera
//! source can use.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use voelin_core::media::voelin_media::frame::{FrameData, Plane, VideoFrame};
use voelin_core::media::voelin_media::{Codec, Codecs, convert};
use voelin_core::media::{
	AudioSourceKind, AudioSourceSpec, EncoderPreference, Streamer, StreamerConfig, preferred_codec,
};
use voelin_core::settings::{STUDIO_REPLAY_SECONDS, STUDIO_SCENES, Settings};
use voelin_core::studio::compose::{Compositor, Feed, RgbaScaler};
use voelin_core::studio::scene::{Colour, Crop, Fit, Scene, Scenes, Source, SourceKind, Transform};
use voelin_core::studio::{self, Command, Event, OutputSpec, camera};

use crate::alloc;

#[derive(Args, Debug)]
pub struct StudioArgs {
	#[command(subcommand)]
	command: StudioCommand,
}

#[derive(Subcommand, Debug)]
enum StudioCommand {
	/// Run a scene file for a while: composite it, encode it as a stream
	/// would, and save a preview, record, save a replay clip or push to WHIP.
	Run(RunArgs),
	/// Composite N sources (a screen, a camera, an image, text, ...) into the
	/// output size at its frame rate, without encoding; print the compose
	/// time and heap allocations per frame.
	Bench(BenchArgs),
	/// List the cameras a scene's camera source can name, with their
	/// formats and sizes.
	Cameras,
}

#[derive(Args, Debug)]
struct RunArgs {
	/// The scenes: JSON as in the `studio.scenes` setting.
	scenes: PathBuf,
	/// How long to run.
	#[arg(long, default_value_t = 5)]
	seconds: u64,
	/// Switch to this scene halfway through (live, also while recording).
	#[arg(long)]
	switch_to: Option<u64>,
	/// Save the last preview picture here (PNG).
	#[arg(long)]
	preview: Option<PathBuf>,
	/// Size of the preview.
	#[arg(long, default_value = "480x270")]
	preview_size: String,
	/// Record the whole run to this file (.webm, or .mkv).
	#[arg(long)]
	record: Option<PathBuf>,
	/// Keep a replay buffer and save it to this file at the end (.webm or
	/// .mkv).
	#[arg(long)]
	clip: Option<PathBuf>,
	/// Seconds the replay buffer keeps.
	#[arg(long, default_value_t = 30)]
	replay: u32,
	/// Push to this WHIP endpoint.
	#[arg(long)]
	whip: Option<String>,
	/// Bearer token for --whip.
	#[arg(long, env = "VOELIN_WHIP_TOKEN", hide_env_values = true)]
	token: Option<String>,
	/// Video codec: vp8, vp9, h264, av1 [default: the first the encoders give].
	#[arg(long)]
	codec: Option<String>,
	/// Encoder: `auto`, `software` or a backend name (`voelinctl stream encoders`).
	#[arg(long, default_value = "auto")]
	encoder: String,
	/// Video bitrate in kbit/s.
	#[arg(long, default_value_t = 4608)]
	bitrate: u32,
	/// Mix a test tone of this many Hz into the audio (otherwise the audio
	/// track stays empty).
	#[arg(long)]
	tone: Option<u32>,
}

#[derive(Args, Debug)]
struct BenchArgs {
	/// Sources in the scene; they cycle through a 4K screen scaled down, a
	/// mirrored and cropped camera, an image with alpha and a line of text.
	#[arg(long, default_value_t = 4)]
	sources: usize,
	/// Output size.
	#[arg(long, default_value = "1920x1080")]
	res: String,
	/// Output frame rate; every source delivers a new picture every frame.
	#[arg(long, default_value_t = 60)]
	fps: u32,
	/// Measured time in seconds (after a short warm-up).
	#[arg(long, default_value_t = 5)]
	seconds: u64,
	/// Compositor threads (0: one per CPU).
	#[arg(long, default_value_t = 0)]
	threads: usize,
}

pub fn run(args: StudioArgs) -> Result<()> {
	match args.command {
		StudioCommand::Run(args) => tokio::runtime::Runtime::new()?.block_on(run_scenes(args)),
		StudioCommand::Bench(args) => bench(&args),
		StudioCommand::Cameras => {
			for camera in camera::list() {
				println!("{}  {} ({})", camera.id, camera.name, camera.backend);
				for format in &camera.formats {
					let sizes: Vec<String> =
						format.sizes.iter().map(|(w, h)| format!("{w}x{h}")).collect();
					let fps = if format.max_fps > 0 {
						format!(", up to {} fps", format.max_fps)
					} else {
						String::new()
					};
					println!("    {}: {}{fps}", format.pixel.label(), sizes.join(" "));
				}
			}
			Ok(())
		}
	}
}

/// `<width>x<height>`.
fn parse_size(size: &str) -> Result<(u32, u32)> {
	let parsed = size.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
	parsed.with_context(|| format!("bad size {size:?}, expected e.g. 1280x720"))
}

async fn run_scenes(args: RunArgs) -> Result<()> {
	let text = std::fs::read_to_string(&args.scenes)
		.with_context(|| format!("reading {}", args.scenes.display()))?;
	let scenes: Scenes = serde_json::from_str(&text).context("the scene file")?;
	scenes.check().map_err(|e| anyhow!("the scene file: {e}"))?;
	let fps = scenes.fps();
	let settings = Settings::in_memory();
	settings.set(&STUDIO_SCENES, scenes)?;
	settings.set(&STUDIO_REPLAY_SECONDS, if args.clip.is_some() { args.replay } else { 0 })?;
	let studio = studio::start(&settings).await?;
	let (width, height) = parse_size(&args.preview_size)?;
	studio.apply(Command::SetPreview { width, height, fps: 5 }).await?;

	let preference =
		EncoderPreference { hardware: true, backend: args.encoder.parse().unwrap_or_default() };
	let codecs = Codecs::new().with_preference(preference);
	let codec: Codec = match &args.codec {
		Some(codec) => codec.parse().map_err(|e| anyhow!("{e}"))?,
		None => preferred_codec(&codecs, None).context("no video encoder")?,
	};
	let config = StreamerConfig {
		fps,
		bitrate_kbps: args.bitrate,
		codec,
		encoder: codecs.preference().clone(),
		audio: args.tone.is_some(),
		audio_sources: args
			.tone
			.map(|hz| AudioSourceSpec::new(AudioSourceKind::Synthetic { hz }))
			.into_iter()
			.collect(),
		..StreamerConfig::default()
	};
	let streamer = Streamer::start_studio(&codecs, config, studio.clone()).await?;
	println!("studio: {} at {fps} fps, {codec}", streamer.backend());

	let mut events = studio.events();
	let printer = tokio::spawn(async move {
		while let Ok(event) = events.recv().await {
			match event {
				Event::Stats(stats) => {
					let sources: Vec<String> = stats
						.sources
						.iter()
						.map(|s| match &s.error {
							Some(e) => format!("{} error: {e}", s.label),
							None => {
								format!("{} {}x{} {:.0} fps", s.label, s.width, s.height, s.fps)
							}
						})
						.collect();
					let outputs: Vec<String> = stats
						.outputs
						.iter()
						.map(|o| format!("{} {:.0} kbit/s", o.name, o.kbps))
						.collect();
					println!(
						"composed {:.0} fps, {:.2} ms; sources: {}; outputs: {}; replay {:.1} s",
						stats.fps,
						stats.compose.compose_time.as_secs_f64() * 1e3,
						sources.join(", "),
						if outputs.is_empty() { "-".into() } else { outputs.join(", ") },
						stats.replay.duration.as_secs_f64(),
					);
				}
				Event::RecordingStopped { path, duration, bytes } => println!(
					"recorded {} ({:.1} s, {bytes} bytes)",
					path.display(),
					duration.as_secs_f64()
				),
				Event::ClipSaved { path, duration } => {
					println!("clip saved: {} ({:.1} s)", path.display(), duration.as_secs_f64());
				}
				Event::OutputAdded { name, .. } => println!("output: {name}"),
				Event::OutputRemoved { name, .. } => println!("output gone: {name}"),
				Event::Error { context, message } => println!("error: {context}: {message}"),
				_ => {}
			}
		}
	});

	if let Some(path) = &args.record {
		studio.apply(Command::StartRecording { path: path.clone() }).await?;
	}
	if let Some(url) = &args.whip {
		let spec = OutputSpec::Url { url: url.clone(), token: args.token.clone() };
		studio.apply(Command::AddOutput(spec)).await?;
		studio.apply(Command::GoLive).await?;
	}
	let run = Duration::from_secs(args.seconds);
	tokio::time::sleep(run / 2).await;
	if let Some(scene) = args.switch_to {
		studio.apply(Command::SetActiveScene { scene }).await?;
		println!("switched to scene {scene}");
	}
	tokio::time::sleep(run - run / 2).await;

	if args.record.is_some() {
		studio.apply(Command::StopRecording).await?;
	}
	if let Some(path) = &args.clip {
		studio.apply(Command::SaveClip { path: path.clone() }).await?;
	}
	if let Some(path) = &args.preview {
		let picture =
			studio.preview().wait_timeout(Duration::from_secs(2)).context("no preview")?;
		save_png(&picture, path)?;
		println!("preview saved: {} ({}x{})", path.display(), picture.width, picture.height);
	}
	if args.whip.is_some() {
		studio.apply(Command::EndStream).await?;
	}
	let stats = streamer.stats();
	println!(
		"encoded {} video / {} audio frames{}",
		stats.video_frames,
		stats.audio_frames,
		stats.error.map(|e| format!(" (last error: {e})")).unwrap_or_default()
	);
	drop(streamer);
	// Let the last events print.
	tokio::time::sleep(Duration::from_millis(100)).await;
	printer.abort();
	Ok(())
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

/// A `width` x `height` picture of a cheap pattern, BGRA (as screens and
/// cameras deliver) or RGBA with alpha (as images and text are).
fn picture(width: u32, height: u32, bgra: bool, seed: u8) -> Arc<VideoFrame> {
	let mut data = Vec::with_capacity(width as usize * height as usize * 4);
	for y in 0..height {
		for x in 0..width {
			let v = (x ^ y) as u8 ^ seed;
			let alpha = if bgra { 255 } else { 128 | v };
			data.extend_from_slice(&[v, v.wrapping_mul(3), seed, alpha]);
		}
	}
	let plane = Plane::new(data, width as usize * 4);
	Arc::new(VideoFrame {
		width,
		height,
		timestamp: Duration::ZERO,
		data: if bgra { FrameData::Bgra(plane) } else { FrameData::Rgba(plane) },
	})
}

/// Source `i` of the bench scene and the size of its pictures.
fn bench_source(i: usize, width: u32, height: u32) -> (Source, (u32, u32), bool) {
	let id = i as u64 + 1;
	// Later rounds of the four kinds are shifted so they do not cover each
	// other exactly.
	let shift = (i / 4) as f32 * 24.0;
	let (w, h) = (width as f32, height as f32);
	match i % 4 {
		0 => (
			Source {
				transform: Transform { fit: Fit::Cover, ..Transform::full(width, height) },
				..Source::new(id, SourceKind::Screen { monitor: 0, backend: None, cursor: true })
			},
			(3840, 2160),
			true,
		),
		1 => (
			Source {
				transform: Transform::box_at(w * 0.75 - shift, h * 0.7 - shift, w * 0.2, h * 0.2),
				crop: Crop { left: 40, top: 0, right: 40, bottom: 0 },
				..Source::new(
					id,
					SourceKind::Camera {
						device: String::new(),
						size: None,
						fps: None,
						mirror: true,
					},
				)
			},
			(1280, 720),
			true,
		),
		2 => (
			Source {
				transform: Transform::box_at(32.0 + shift, 32.0 + shift, 256.0, 256.0),
				opacity: 0.75,
				..Source::new(id, SourceKind::Image { path: "logo.png".into() })
			},
			(256, 256),
			false,
		),
		_ => (
			Source {
				transform: Transform::box_at(32.0 + shift, h - 120.0 - shift, w * 0.4, 80.0),
				..Source::new(
					id,
					SourceKind::Text {
						text: "LIVE".into(),
						font: None,
						size_px: 48.0,
						colour: Colour::WHITE,
						backdrop: Colour::CLEAR,
						align: Default::default(),
						padding: 0,
					},
				)
			},
			(600, 80),
			false,
		),
	}
}

fn bench(args: &BenchArgs) -> Result<()> {
	let (width, height) = parse_size(&args.res)?;
	let fps = args.fps.max(1);
	let mut scene = Scene::new(1, "bench");
	let mut pictures = Vec::new();
	for i in 0..args.sources {
		let (source, (w, h), bgra) = bench_source(i, width, height);
		scene.sources.push(source);
		pictures.push(picture(w, h, bgra, (i * 40) as u8));
	}
	let feeds: Vec<Option<Arc<Feed>>> =
		(0..scene.sources.len()).map(|_| Some(Arc::new(Feed::new()))).collect();
	let mut compositor = Compositor::new(args.threads);
	compositor.set_size(width, height);
	let mut preview = RgbaScaler::new(2);
	let interval = Duration::from_secs(1) / fps;
	let mut times = Vec::with_capacity((args.seconds * u64::from(fps)) as usize + 1);
	let mut previews = Duration::ZERO;
	let mut step = |n: u64, times: Option<&mut Vec<Duration>>| -> Result<()> {
		for (feed, picture) in feeds.iter().zip(&pictures) {
			feed.as_ref().expect("every source has a feed").put(picture.clone());
		}
		let started = Instant::now();
		let frame = compositor.compose(&scene, 1, &feeds, interval * n as u32)?;
		let composed = started.elapsed();
		drop(preview.scale(&frame, 480, 270)?);
		if let Some(times) = times {
			times.push(composed);
			previews += started.elapsed() - composed;
		}
		Ok(())
	};
	let warmup = u64::from(fps).max(10);
	for n in 0..warmup {
		step(n, None)?;
	}
	let frames = args.seconds * u64::from(fps);
	let (allocations, bytes) = alloc::counts();
	let mut next = Instant::now();
	for n in warmup..warmup + frames {
		step(n, Some(&mut times))?;
		next += interval;
		std::thread::sleep(next.saturating_duration_since(Instant::now()));
	}
	let (allocations, bytes) = {
		let (a, b) = alloc::counts();
		(a - allocations, b - bytes)
	};
	times.sort();
	let ms = |d: Duration| d.as_secs_f64() * 1e3;
	let mean = times.iter().sum::<Duration>() / times.len().max(1) as u32;
	let at = |q: f64| times[((times.len() as f64 * q) as usize).min(times.len() - 1)];
	let stats = compositor.stats();
	println!(
		"studio bench: {width}x{height} at {fps} fps, {} sources, {} compositor threads, {frames} frames",
		args.sources, stats.threads
	);
	println!(
		"compose: mean {:.2} ms, p50 {:.2}, p95 {:.2}, max {:.2} (a frame at {fps} fps: {:.2} ms)",
		ms(mean),
		ms(at(0.5)),
		ms(at(0.95)),
		ms(*times.last().unwrap_or(&Duration::ZERO)),
		ms(interval)
	);
	println!("preview 480x270: mean {:.2} ms", ms(previews / frames.max(1) as u32));
	println!(
		"heap: {:.2} allocations ({:.0} bytes) per frame; plans made {}, canvases allocated {}",
		allocations as f64 / frames.max(1) as f64,
		bytes as f64 / frames.max(1) as f64,
		stats.replans,
		stats.allocated
	);
	Ok(())
}
