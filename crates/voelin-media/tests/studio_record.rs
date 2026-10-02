//! The studio's recording and replay outputs against a real encoder and a
//! real demuxer: frames the test pattern produces are encoded, written to
//! WebM and read back with `ffprobe` (skipped where it is not installed).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use voelin_media::codec::{Codec, Codecs, EncoderConfig};
use voelin_media::frame::VideoFrame;
use voelin_media::studio::output::record::Recorder;
use voelin_media::studio::output::replay::ReplayBuffer;
use voelin_media::studio::output::{OutputSink, Packet, Track};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const FPS: u64 = 30;

fn dir(name: &str) -> PathBuf {
	let dir = std::env::temp_dir().join(format!("voelin-studio-{name}-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// A moving picture, so the encoder has something to encode.
fn frame(n: u64) -> VideoFrame {
	let mut frame = VideoFrame::black_i420(WIDTH, HEIGHT);
	let voelin_media::frame::FrameData::I420 { y, .. } = &mut frame.data else { unreachable!() };
	let x0 = (n * 4 % u64::from(WIDTH - 40)) as usize;
	for row in 40..120 {
		y.data[row * y.stride + x0..][..40].fill(200);
	}
	frame.timestamp = Duration::from_micros(n * 1_000_000 / FPS);
	frame
}

/// Encode `frames` pictures with `codec` and hand every packet to `sink`.
/// Returns how many keyframes came out.
fn encode_into(codec: Codec, frames: u64, sink: &mut dyn OutputSink) -> u64 {
	let codecs = Codecs::new();
	let mut encoder = codecs
		.new_encoder(
			codec,
			EncoderConfig { fps: FPS as u32, bitrate_bps: 800_000, ..EncoderConfig::default() },
		)
		.expect("an encoder for the test codec");
	let mut keyframes = 0;
	for n in 0..frames {
		let picture = frame(n);
		// A keyframe every second, as a stream would have.
		let force = n.is_multiple_of(FPS);
		encoder
			.encode_with(&picture, force, &mut |chunk| {
				keyframes += u64::from(chunk.keyframe);
				sink.write(&Packet {
					track: Track::Video { codec, layer: 0 },
					pts_90khz: chunk.pts_90khz,
					keyframe: chunk.keyframe,
					width: WIDTH,
					height: HEIGHT,
					data: chunk.data,
				})
				.expect("the sink took the packet");
			})
			.expect("encoded");
		// 20 ms of "Opus" per two pictures, so the audio track is used too.
		if n.is_multiple_of(2) {
			sink.write(&Packet {
				track: Track::Audio { channels: 2 },
				pts_90khz: n * 90_000 / FPS,
				keyframe: true,
				width: 0,
				height: 0,
				data: &[0xFC, 0xFF, 0xFE],
			})
			.expect("the sink took the audio packet");
		}
	}
	keyframes
}

/// `ffprobe -show_streams` of `path`, or `None` if ffprobe is not installed.
fn ffprobe(path: &Path) -> Option<String> {
	let out = Command::new("ffprobe")
		.args([
			"-v",
			"error",
			"-show_entries",
			"stream=codec_name,codec_type,width,height:format=format_name,duration",
			"-of",
			"default=noprint_wrappers=1",
		])
		.arg(path)
		.output()
		.ok()?;
	assert!(
		out.status.success(),
		"ffprobe rejected {}: {}",
		path.display(),
		String::from_utf8_lossy(&out.stderr)
	);
	assert!(
		out.stderr.is_empty(),
		"ffprobe complained about {}: {}",
		path.display(),
		String::from_utf8_lossy(&out.stderr)
	);
	Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn a_recording_is_a_webm_ffprobe_reads() {
	let codec = Codec::Vp8;
	if Codecs::new().new_encoder(codec, EncoderConfig::default()).is_err() {
		eprintln!("skipped: no VP8 encoder in this build");
		return;
	}
	let dir = dir("record");
	let path = dir.join("recording.webm");
	let mut recorder = Recorder::start(&path, 0, 2).unwrap();
	let keyframes = encode_into(codec, 90, &mut recorder);
	assert!(keyframes >= 3, "only {keyframes} keyframes");
	assert!(recorder.duration() >= Duration::from_millis(2800), "{:?}", recorder.duration());
	assert!(recorder.bytes() > 1000);
	recorder.finish().unwrap();

	let Some(probe) = ffprobe(&path) else {
		eprintln!("skipped the ffprobe check: ffprobe is not installed");
		std::fs::remove_dir_all(&dir).ok();
		return;
	};
	println!("{probe}");
	assert!(probe.contains("codec_name=vp8"), "{probe}");
	assert!(probe.contains("codec_type=video"), "{probe}");
	assert!(probe.contains("codec_name=opus"), "{probe}");
	assert!(probe.contains(&format!("width={WIDTH}")), "{probe}");
	assert!(probe.contains(&format!("height={HEIGHT}")), "{probe}");
	assert!(probe.contains("format_name=matroska,webm"), "{probe}");
	let duration: f64 = probe
		.lines()
		.find_map(|l| l.strip_prefix("duration="))
		.and_then(|d| d.parse().ok())
		.unwrap_or(0.0);
	assert!(duration > 2.5, "the file says it is {duration} s long");
	std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_replay_clip_starts_at_a_keyframe_and_plays() {
	let codec = Codec::Vp8;
	if Codecs::new().new_encoder(codec, EncoderConfig::default()).is_err() {
		eprintln!("skipped: no VP8 encoder in this build");
		return;
	}
	let dir = dir("replay");
	// Two seconds kept out of five encoded, spilling at once (0 MB memory).
	let mut buffer = ReplayBuffer::new(Duration::from_secs(2), 0, 0, 2).with_spill_dir(&dir);
	encode_into(codec, 150, &mut buffer);
	let stats = buffer.stats();
	assert!(stats.spilled_bytes > 0, "{stats:?}");
	assert!(stats.duration >= Duration::from_secs(2), "{stats:?}");
	// Five seconds went in, at most three come out (two plus the keyframe
	// interval the window starts in).
	assert!(stats.duration <= Duration::from_secs(3), "{stats:?}");

	let clip = dir.join("clip.webm");
	let length = buffer.save_clip(&clip).unwrap();
	assert!(length >= Duration::from_secs(2), "{length:?}");
	// Saving does not empty the buffer.
	assert_eq!(buffer.stats().packets, stats.packets);

	let Some(probe) = ffprobe(&clip) else {
		eprintln!("skipped the ffprobe check: ffprobe is not installed");
		std::fs::remove_dir_all(&dir).ok();
		return;
	};
	println!("{probe}");
	assert!(probe.contains("codec_name=vp8"), "{probe}");
	// The clip decodes from its first frame: ffmpeg would report errors on
	// stderr (which `ffprobe` asserts is empty) if it started mid-picture.
	let frames = Command::new("ffprobe")
		.args([
			"-v",
			"error",
			"-count_frames",
			"-select_streams",
			"v:0",
			"-show_entries",
			"stream=nb_read_frames",
			"-of",
			"default=noprint_wrappers=1:nokey=1",
		])
		.arg(&clip)
		.output()
		.expect("ffprobe ran");
	assert!(frames.stderr.is_empty(), "{}", String::from_utf8_lossy(&frames.stderr));
	let decoded: u64 = String::from_utf8_lossy(&frames.stdout).trim().parse().unwrap_or_default();
	assert!(decoded >= 60, "only {decoded} frames decoded from the clip");
	std::fs::remove_dir_all(&dir).ok();
}
