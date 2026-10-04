//! The viewer's pipeline on an H.264 stream with B-frames at 2560x1440 and
//! 60 fps, like the official client sends (AMF with B-frames, keyframes
//! rarely): frames pushed in real time into a `VideoPipeline`, every
//! picture converted to RGBA as the app shows it, with and without a lost
//! frame every second. The stream is the one `voelin-media`'s
//! `h264_with_b_frames_at_1440p60` measurement makes (in the temporary
//! directory); `VOELIN_OPENH264_LIB` adds Cisco's OpenH264 alone, the
//! decoder the app used before FFmpeg's. Run with `--release --ignored`.

#![cfg(feature = "media-desktop")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use voelin_core::media::voelin_media::{Codec, Codecs, convert};
use voelin_core::media::{DecodeStats, VideoPipeline};
use voelin_stream::{Frequency, MediaFrame, MediaKind, MediaTime};

/// The access units of an Annex B stream with access unit delimiters.
fn access_units(stream: &[u8]) -> Vec<&[u8]> {
	let mut starts = Vec::new();
	let mut i = 0;
	while i + 3 < stream.len() {
		if stream[i..i + 3] == [0, 0, 1] {
			if stream[i + 3] & 0x1f == 9 {
				starts.push(if i > 0 && stream[i - 1] == 0 { i - 1 } else { i });
			}
			i += 3;
		} else {
			i += 1;
		}
	}
	starts.push(stream.len());
	starts.windows(2).map(|w| &stream[w[0]..w[1]]).collect()
}

/// `units` at 60 fps into a pipeline of `codecs`, every `loss`-th frame
/// lost; the pictures shown and the pipeline's statistics.
fn play(codecs: Codecs, units: &[&[u8]], loss: Option<usize>) -> (u64, DecodeStats) {
	let shown = Arc::new(AtomicU64::new(0));
	let rgba = Mutex::new(vec![0u8; 2560 * 1440 * 4]);
	let pipeline = VideoPipeline::new(
		Arc::new(codecs),
		{
			let shown = shown.clone();
			move |picture| {
				let stride = picture.width as usize * 4;
				let mut rgba = rgba.lock().unwrap();
				rgba.resize(stride * picture.height as usize, 0);
				convert::to_rgba(&picture, &mut rgba, stride).unwrap();
				shown.fetch_add(1, Ordering::Relaxed);
			}
		},
		|| {},
	);
	let started = Instant::now();
	let mut contiguous = true;
	for (n, unit) in units.iter().enumerate() {
		let due = started + Duration::from_micros(16_667 * n as u64);
		std::thread::sleep(due.saturating_duration_since(Instant::now()));
		if loss.is_some_and(|every| n % every == every - 1) {
			contiguous = false;
			continue;
		}
		pipeline.push(MediaFrame {
			kind: MediaKind::Video,
			codec: voelin_stream::Codec::H264,
			time: MediaTime::new(1500 * n as u64, Frequency::NINETY_KHZ),
			network_time: Instant::now(),
			contiguous,
			data: Arc::from(*unit),
		});
		contiguous = true;
	}
	std::thread::sleep(Duration::from_millis(200));
	(shown.load(Ordering::Relaxed), pipeline.stats())
}

#[test]
#[ignore = "a measurement: needs the stream voelin-media's measurement makes"]
fn h264_with_b_frames_plays_at_full_rate() {
	let dir = std::env::temp_dir().join("voelin-h264-bframes");
	let mut played = 0;
	for stream in ["h264_vaapi", "x264"] {
		let Ok(data) = std::fs::read(dir.join(format!("{stream}-bframes-1440p60.h264"))) else {
			eprintln!("{stream}: no stream in {}, skipped", dir.display());
			continue;
		};
		let units = access_units(&data);
		let mut setups = vec![("ladder", Codecs::new())];
		if let Some(path) = std::env::var_os("VOELIN_OPENH264_LIB") {
			let library =
				voelin_core::media::voelin_media::codec::h264::OpenH264::load(path).unwrap();
			setups.push(("openh264", Codecs::builtin().with_openh264(library)));
		}
		for (name, codecs) in setups {
			for loss in [None, Some(60)] {
				let ladder = codecs.decoders_for(Codec::H264);
				let (shown, stats) = play(codecs.clone(), &units, loss);
				let seconds = units.len() as f64 / 60.0;
				eprintln!(
					"{stream} 2560x1440@60 with B-frames, {name} {ladder:?}, {}: {shown} of {} \
					 pictures shown ({:.1} fps), decoder {:?}, skipped {}, keyframe requests {}, \
					 last error {:?}",
					if loss.is_some() { "a frame lost every second" } else { "no loss" },
					units.len(),
					shown as f64 / seconds,
					stats.decoder,
					stats.skipped,
					stats.keyframe_requests,
					stats.error,
				);
				if name == "ladder" {
					// Everything but the frames lost and the two the decoder
					// still holds at the end.
					let lost = loss.map_or(0, |every| units.len() / every);
					assert!(shown + 3 + lost as u64 >= units.len() as u64, "{shown}");
				}
			}
		}
		played += 1;
	}
	assert!(played > 0, "no stream: run voelin-media's h264_with_b_frames_at_1440p60 first");
}
