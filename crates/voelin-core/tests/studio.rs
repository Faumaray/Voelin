//! The Stream Studio as the video of a stream: the streamer encodes its
//! composite (VP8 and an Opus test tone) without any stream attached, and the
//! studio records it and keeps a replay buffer while the scene switches.
//! The files are read back with ffprobe and decoded with ffmpeg where those
//! are installed.

#![cfg(feature = "media-desktop")]

use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::{Duration, Instant};

use voelin_core::media::voelin_media::{Codec, Codecs};
use voelin_core::media::{AudioSourceKind, AudioSourceSpec, Streamer, StreamerConfig};
use voelin_core::settings::{STUDIO_REPLAY_SECONDS, STUDIO_SCENES, Settings};
use voelin_core::studio::scene::{Colour, Scene, Scenes, Source, SourceKind, Transform};
use voelin_core::studio::{self, Command, Event};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const RED: [u8; 3] = [220, 30, 30];
const BLUE: [u8; 3] = [30, 30, 220];

fn colour_scene(id: u64, name: &str, rgb: [u8; 3]) -> Scene {
	let mut scene = Scene::new(id, name);
	scene.sources.push(Source {
		transform: Transform::full(WIDTH, HEIGHT),
		..Source::new(
			1,
			SourceKind::Colour { colour: Colour::rgb(rgb[0], rgb[1], rgb[2]), size: (16, 16) },
		)
	});
	scene
}

fn has(tool: &str) -> bool {
	Process::new(tool).arg("-version").output().is_ok_and(|o| o.status.success())
}

/// What ffprobe says of `path`: each stream's codec and start time, and the
/// duration, in seconds.
fn probe(path: &Path) -> (Vec<(String, f64)>, f64) {
	let out = Process::new("ffprobe")
		.args(["-v", "error", "-show_entries", "stream=codec_name,start_time:format=duration"])
		.args(["-of", "csv=p=0"])
		.arg(path)
		.output()
		.unwrap();
	assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
	let text = String::from_utf8_lossy(&out.stdout).into_owned();
	let mut lines: Vec<&str> = text.lines().collect();
	let duration = lines.pop().and_then(|d| d.parse().ok()).unwrap_or(0.0);
	let streams = lines
		.iter()
		.map(|l| {
			let (codec, start) = l.split_once(',').unwrap_or((l, "0"));
			(codec.to_owned(), start.parse().unwrap_or(f64::NAN))
		})
		.collect();
	(streams, duration)
}

/// Codecs VP8 and Opus, starting together (the audio's clock is the
/// studio's): what a recording of the studio holds.
fn check_streams(streams: &[(String, f64)]) {
	let codecs: Vec<&str> = streams.iter().map(|(c, _)| c.as_str()).collect();
	assert_eq!(codecs, ["vp8", "opus"], "{streams:?}");
	assert!((streams[0].1 - streams[1].1).abs() < 0.1, "out of sync: {streams:?}");
}

/// Every picture of `path`, decoded by ffmpeg as RGB.
fn decode(path: &Path) -> Vec<Vec<u8>> {
	let out = Process::new("ffmpeg")
		.args(["-v", "error", "-i"])
		.arg(path)
		.args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
		.output()
		.unwrap();
	assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
	assert!(out.stderr.is_empty(), "decoder complaints: {}", String::from_utf8_lossy(&out.stderr));
	out.stdout.chunks_exact((WIDTH * HEIGHT * 3) as usize).map(<[u8]>::to_vec).collect()
}

fn centre(picture: &[u8]) -> [u8; 3] {
	let at = ((HEIGHT / 2 * WIDTH + WIDTH / 2) * 3) as usize;
	[picture[at], picture[at + 1], picture[at + 2]]
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
	a.iter().zip(b).all(|(a, b)| a.abs_diff(b) <= 24)
}

fn temp_dir() -> PathBuf {
	let dir = std::env::temp_dir().join(format!("voelin-studio-core-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_studio_stream_records_and_replays_while_the_scene_switches() {
	let codecs = Codecs::new();
	if !codecs.encoder_codecs().contains(&Codec::Vp8) {
		eprintln!("skipped: no VP8 encoder in this build");
		return;
	}
	let settings = Settings::in_memory();
	let mut scenes =
		Scenes { width: WIDTH, height: HEIGHT, fps: 30, active: 1, ..Scenes::default() };
	scenes.scenes.push(colour_scene(1, "Red", RED));
	scenes.scenes.push(colour_scene(2, "Blue", BLUE));
	settings.set(&STUDIO_SCENES, scenes).unwrap();
	settings.set(&STUDIO_REPLAY_SECONDS, 10).unwrap();
	let studio = studio::start(&settings).await.unwrap();
	let mut events = studio.events();

	// No stream is attached: the composite is encoded anyway.
	let config = StreamerConfig {
		fps: 30,
		bitrate_kbps: 800,
		codec: Codec::Vp8,
		audio_sources: vec![AudioSourceSpec::new(AudioSourceKind::Synthetic { hz: 440 })],
		..StreamerConfig::default()
	};
	let mut streamer = Streamer::start_studio(&codecs, config, studio.clone()).await.unwrap();
	assert_eq!(streamer.backend(), "studio");

	let dir = temp_dir();
	let recording = dir.join("recording.webm");
	studio.apply(Command::StartRecording { path: recording.clone() }).await.unwrap();
	tokio::time::sleep(Duration::from_millis(1500)).await;
	// Switch the scene live, while recording.
	studio.apply(Command::SetActiveScene { scene: 2 }).await.unwrap();
	tokio::time::sleep(Duration::from_millis(1500)).await;
	studio.apply(Command::StopRecording).await.unwrap();
	let clip = dir.join("clip.mkv");
	studio.apply(Command::SaveClip { path: clip.clone() }).await.unwrap();

	let stats = streamer.stats();
	assert!(stats.video_frames >= 60, "{stats:?}");
	assert!(stats.audio_frames >= 100, "{stats:?}");
	assert_eq!(stats.width, WIDTH, "{stats:?}");
	streamer.stop();

	let mut seen = Vec::new();
	while let Ok(event) = events.try_recv() {
		seen.push(event);
	}
	let stopped = seen.iter().find_map(|e| match e {
		Event::RecordingStopped { bytes, duration, .. } => Some((*bytes, *duration)),
		_ => None,
	});
	let (bytes, duration) = stopped.expect("a RecordingStopped event");
	assert!(bytes > 1000 && duration >= Duration::from_secs(2), "{bytes} bytes in {duration:?}");
	assert!(seen.iter().any(|e| matches!(e, Event::ClipSaved { .. })), "{seen:?}");

	if !has("ffprobe") || !has("ffmpeg") {
		eprintln!("ffprobe or ffmpeg is not installed: the files were not read back");
		std::fs::remove_dir_all(&dir).ok();
		return;
	}
	// The recording: VP8 and Opus, about three seconds, every picture
	// decodes, red before the switch and blue after it.
	let (streams, length) = probe(&recording);
	check_streams(&streams);
	assert!((2.5..4.5).contains(&length), "{length} s");
	let pictures = decode(&recording);
	assert!(pictures.len() >= 60, "{} pictures", pictures.len());
	assert!(near(centre(&pictures[0]), RED), "{:?}", centre(&pictures[0]));
	let last = pictures.last().unwrap();
	assert!(near(centre(last), BLUE), "{:?}", centre(last));

	// The clip: Matroska, from the replay buffer, also both scenes (it
	// started with the streamer, before the recording).
	let (streams, length) = probe(&clip);
	check_streams(&streams);
	assert!(length >= 2.5, "{length} s");
	let pictures = decode(&clip);
	assert!(near(centre(&pictures[0]), RED), "{:?}", centre(&pictures[0]));
	assert!(near(centre(pictures.last().unwrap()), BLUE));
	std::fs::remove_dir_all(&dir).ok();
}

/// FFmpeg's own RTMP server (`ffmpeg -listen 1`) on `port`, keeping what
/// it receives in `out`; its log goes to `out` with `.log`.
fn rtmp_server(port: u16, out: &Path) -> std::process::Child {
	let log = std::fs::File::create(out.with_extension("log")).unwrap();
	Process::new("ffmpeg")
		.args(["-hide_banner", "-nostdin", "-loglevel", "warning", "-listen", "1", "-i"])
		.arg(format!("rtmp://127.0.0.1:{port}/live/test-key"))
		.args(["-c", "copy", "-y"])
		.arg(out)
		.stderr(log)
		.spawn()
		.unwrap()
}

/// Wait up to `limit` for `child` to exit by itself.
fn wait_exit(child: &mut std::process::Child, limit: Duration) -> Option<std::process::ExitStatus> {
	let deadline = Instant::now() + limit;
	while Instant::now() < deadline {
		if let Some(status) = child.try_wait().unwrap() {
			return Some(status);
		}
		std::thread::sleep(Duration::from_millis(50));
	}
	None
}

/// ffprobe's view of the streams of `path`: codec, width, height, sample
/// rate and channels per stream.
fn probe_streams(path: &Path) -> Vec<String> {
	let out = Process::new("ffprobe")
		.args(["-v", "error", "-show_entries"])
		.args(["stream=codec_name,width,height,sample_rate,channels", "-of", "csv=p=0"])
		.arg(path)
		.output()
		.unwrap();
	assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
	String::from_utf8_lossy(&out.stdout)
		.lines()
		.map(|l| l.trim_end_matches(',').to_owned())
		.collect()
}

/// The audio of `path`, mono, decoded by ffmpeg: its length in seconds and
/// its frequency (from the zero crossings), ignoring the first 100 ms.
fn tone(path: &Path) -> (f64, f64) {
	let out = Process::new("ffmpeg")
		.args(["-v", "error", "-i"])
		.arg(path)
		.args(["-map", "0:a", "-ac", "1", "-ar", "48000", "-f", "f32le", "-"])
		.output()
		.unwrap();
	assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
	let samples: Vec<f32> =
		out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
	let tail = &samples[samples.len().min(4800)..];
	let crossings = tail.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
	let seconds = tail.len() as f64 / 48_000.0;
	(samples.len() as f64 / 48_000.0, crossings as f64 / 2.0 / seconds.max(1e-9))
}

/// Whether the first video frame of `path` is a keyframe.
fn starts_with_keyframe(path: &Path) -> bool {
	let out = Process::new("ffprobe")
		.args(["-v", "error", "-select_streams", "v", "-read_intervals", "%+#1"])
		.args(["-show_entries", "frame=key_frame", "-of", "csv=p=0"])
		.arg(path)
		.output()
		.unwrap();
	String::from_utf8_lossy(&out.stdout).lines().next() == Some("1")
}

/// The studio pushed over RTMP to FFmpeg's own RTMP server: H.264 as
/// encoded and the tone as AAC arrive, with the stream key; the server goes
/// away mid-stream and a new one takes its place, which the output finds
/// again (starting at a keyframe); End Stream unpublishes cleanly, so the
/// server finishes its file and exits by itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_studio_stream_goes_out_over_rtmp_and_comes_back_after_the_server_did() {
	use voelin_core::media::voelin_media::ffmpeg::avio::Connection;
	use voelin_core::studio::OutputSpec;

	if !has("ffmpeg") || !has("ffprobe") {
		eprintln!("skipped: ffmpeg and ffprobe are the RTMP server and the check");
		return;
	}
	if let Err(e) = Connection::available() {
		eprintln!("skipped: no FFmpeg libraries with RTMP: {e}");
		return;
	}
	let codecs = Codecs::new();
	if !codecs.encoder_codecs().contains(&Codec::H264) {
		eprintln!("skipped: no H.264 encoder");
		return;
	}
	let settings = Settings::in_memory();
	let mut scenes =
		Scenes { width: WIDTH, height: HEIGHT, fps: 30, active: 1, ..Scenes::default() };
	scenes.scenes.push(colour_scene(1, "Red", RED));
	settings.set(&STUDIO_SCENES, scenes).unwrap();
	let studio = studio::start(&settings).await.unwrap();
	let config = StreamerConfig {
		fps: 30,
		bitrate_kbps: 800,
		codec: Codec::H264,
		audio_sources: vec![AudioSourceSpec::new(AudioSourceKind::Synthetic { hz: 440 })],
		..StreamerConfig::default()
	};
	let mut streamer = Streamer::start_studio(&codecs, config, studio.clone()).await.unwrap();

	let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
	let dir = std::env::temp_dir().join(format!("voelin-studio-rtmp-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	let (first, second) = (dir.join("first.flv"), dir.join("second.flv"));
	let mut server = rtmp_server(port, &first);
	// The key apart from the URL, as a service gives them.
	let spec = OutputSpec::Url {
		url: format!("rtmp://127.0.0.1:{port}/live"),
		token: Some("test-key".into()),
	};
	let deadline = Instant::now() + Duration::from_secs(10);
	loop {
		match studio.apply(Command::AddOutput(spec.clone())).await {
			Ok(()) => break,
			// The server is still starting.
			Err(e) if Instant::now() < deadline => {
				eprintln!("not yet: {e}");
				tokio::time::sleep(Duration::from_millis(200)).await;
			}
			Err(e) => panic!("the RTMP output did not connect: {e}"),
		}
	}
	studio.apply(Command::GoLive).await.unwrap();
	tokio::time::sleep(Duration::from_secs(3)).await;

	// The server goes away (as a service restarting would); another one
	// takes the address.
	Process::new("kill").args(["-TERM", &server.id().to_string()]).status().unwrap();
	assert!(wait_exit(&mut server, Duration::from_secs(10)).is_some(), "the first server hangs");
	let mut server = rtmp_server(port, &second);
	let deadline = Instant::now() + Duration::from_secs(20);
	while std::fs::metadata(&second).map_or(0, |m| m.len()) < 20_000 {
		assert!(Instant::now() < deadline, "the output did not come back");
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	tokio::time::sleep(Duration::from_secs(2)).await;
	studio.apply(Command::EndStream).await.unwrap();
	let status = wait_exit(&mut server, Duration::from_secs(10));
	if status.is_none() {
		let _ = server.kill();
	}
	streamer.stop();
	assert!(status.is_some_and(|s| s.success()), "the server did not finish: {status:?}");

	for (file, seconds) in [(&first, 2.0), (&second, 1.5)] {
		let log = std::fs::read_to_string(file.with_extension("log")).unwrap_or_default();
		// The key arrived as the stream's name.
		assert!(!log.contains("Unexpected stream"), "{log}");
		let streams = probe_streams(file);
		assert_eq!(streams, [format!("h264,{WIDTH},{HEIGHT}"), "aac,48000,2".to_owned()], "{log}");
		assert!(starts_with_keyframe(file), "{}", file.display());
		let pictures = decode(file);
		assert!(pictures.len() as f64 >= seconds * 25.0, "{} pictures", pictures.len());
		assert!(near(centre(&pictures[0]), RED), "{:?}", centre(&pictures[0]));
		let (length, hz) = tone(file);
		assert!(length >= seconds, "{length} s of audio in {}", file.display());
		assert!((430.0..450.0).contains(&hz), "{hz} Hz in {}", file.display());
	}
	std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_studio_source_needs_a_studio() {
	let codecs = Codecs::new();
	let config = StreamerConfig {
		source: voelin_core::media::voelin_media::SourceId::Studio,
		audio: false,
		..StreamerConfig::default()
	};
	let started = Instant::now();
	let error = Streamer::start(&codecs, config).await.err().expect("refused");
	assert!(error.to_string().contains("start_studio"), "{error}");
	assert!(started.elapsed() < Duration::from_secs(5));
}
