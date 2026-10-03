//! Decoders through the system's FFmpeg (loaded at runtime): the probe, the
//! ladder of each codec, every encoder of this machine against every decoder
//! of its codec, VA-API decoding on the GPU, and the codecs a viewer accepts
//! with and without FFmpeg.
//!
//! Skipped without FFmpeg. Hardware decoders need their GPU and driver; on a
//! machine without them the self-test reports why and they are skipped.
#![cfg(feature = "ffmpeg")]

use std::time::{Duration, Instant};

use voelin_media::capture::synthetic::SyntheticScreen;
use voelin_media::ffmpeg::{DECODERS, DecoderSpec, Ffmpeg, FfmpegDecoder, decoder};
use voelin_media::{
	Codec, Codecs, DecoderBackend, EncodedFrame, EncoderConfig, EncoderPreference, VideoDecoder,
	VideoFrame, convert,
};

fn ffmpeg() -> Option<&'static Ffmpeg> {
	match Ffmpeg::get() {
		Ok(f) => Some(f),
		Err(e) => {
			eprintln!("FFmpeg not available ({e}), skipped");
			None
		}
	}
}

fn available(name: &str) -> bool {
	decoder::probe().iter().any(|s| s.spec.name == name && s.available.is_ok())
}

#[test]
fn probe_reports_every_decoder() {
	if ffmpeg().is_none() {
		return;
	}
	let statuses = decoder::probe();
	assert_eq!(statuses.len(), DECODERS.len());
	for status in statuses {
		eprintln!(
			"{:>18}: {:?} ({} ms)",
			status.spec.name,
			status.available,
			status.took.as_millis()
		);
		if let Err(reason) = &status.available {
			assert!(!reason.is_empty());
		}
	}
	// The report lists them with the built-in ones, ranked by the ladder.
	let codecs = Codecs::new();
	let report = codecs.report();
	assert!(report.decoders.iter().any(|d| d.name == "libvpx"));
	for status in statuses {
		let info = report.decoders.iter().find(|d| d.name == status.spec.name).unwrap();
		assert_eq!(info.status, status.available);
		assert_eq!(info.rank.is_some(), status.available.is_ok(), "{}", status.spec.name);
	}
	for codec in Codec::ALL {
		let ladder: Vec<String> =
			codecs.decoders_for(codec).iter().map(|b| b.to_string()).collect();
		eprintln!("{codec}: {}", ladder.join(" > "));
	}
	eprintln!("viewer accepts: {:?}", codecs.decoders());
	// FFmpeg's software decoders are in every build: a viewer takes every
	// codec but AV1 (which needs dav1d or libaom in FFmpeg) with them.
	for (name, codec) in [("h264", Codec::H264), ("hevc", Codec::H265), ("vp9", Codec::Vp9)] {
		if available(name) {
			assert!(codecs.decoders().contains(&codec), "{codec}");
		}
	}
}

/// `frames` of the moving test pattern at `w` x `h` through `encoder`'s
/// best backend (`backend` named first), with a keyframe at the start.
fn encode(codec: Codec, backend: &str, w: u32, h: u32, frames: u64) -> Vec<(EncodedFrame, u64)> {
	let codecs = Codecs::new();
	let screen = SyntheticScreen::new(w, h);
	let config = EncoderConfig { fps: 30, bitrate_bps: 2_000_000, ..EncoderConfig::default() };
	let preference = EncoderPreference { hardware: true, backend: backend.parse().unwrap() };
	let mut encoder = codecs.new_encoder_preferring(codec, config, &preference).unwrap();
	let mut out = Vec::new();
	let mut sources = Vec::new();
	for n in 0..frames + 8 {
		let frame = screen.frame(n.min(frames - 1), 30);
		let frame = frame.with_timestamp(Duration::from_millis(33 * n));
		sources.push(frame.pts_90khz());
		for f in encoder.encode(&frame, n == 0).unwrap() {
			let index = sources.iter().position(|&p| p == f.pts_90khz).unwrap() as u64;
			if out.len() < frames as usize {
				out.push((f, index.min(frames - 1)));
			}
		}
	}
	out
}

/// Decodes `stream` with `decoder`: the PSNR of every picture against the
/// pattern frame it came from, or why it failed.
fn decode(
	decoder: &mut dyn VideoDecoder,
	stream: &[(EncodedFrame, u64)],
	screen: &SyntheticScreen,
) -> Result<(usize, f64), String> {
	let mut picture = VideoFrame::black_i420(0, 0);
	let mut worst = f64::INFINITY;
	let mut decoded = 0;
	for (frame, index) in stream {
		if decoder.decode_into(&frame.data, &mut picture).map_err(|e| e.to_string())? {
			let source = screen.frame(*index, 30);
			let source = if (picture.width, picture.height) == (source.width, source.height) {
				source
			} else {
				// Cropped to the encoder's alignment (AV1 in hardware).
				let mut view = source.view();
				(view.width, view.height) = (picture.width, picture.height);
				view.to_frame()
			};
			worst = worst.min(convert::psnr(&source, &picture).map_err(|e| e.to_string())?);
			decoded += 1;
		}
	}
	Ok((decoded, worst))
}

/// Every encoder this machine has, through every decoder of its codec
/// (hardware, FFmpeg's software ones, the built-in ones): every frame
/// decodes, PSNR above 28 dB against the pattern, as `tests/ffmpeg.rs`
/// checks the encoders.
#[test]
fn every_encoder_decodes_with_every_decoder() {
	if ffmpeg().is_none() {
		return;
	}
	let codecs = Codecs::new();
	let (w, h) = (320, 240);
	let screen = SyntheticScreen::new(w, h);
	let mut table = Vec::new();
	let mut failed = Vec::new();
	for (codec, encoder) in codecs.encoders() {
		let stream = encode(codec, encoder.name(), w, h, 12);
		for backend in codecs.decoders_for(codec) {
			let mut decoder = codecs.new_decoder_with(codec, backend).unwrap();
			match decode(&mut *decoder, &stream, &screen) {
				Ok((decoded, psnr)) if decoded == stream.len() && psnr > 28.0 => {
					table.push(format!("{codec:<4} {encoder:<12} -> {backend:<12} {psnr:5.1} dB"));
				}
				Ok((decoded, psnr)) => failed.push(format!(
					"{encoder} -> {backend}: {decoded} of {} pictures, PSNR {psnr:.1} dB",
					stream.len()
				)),
				Err(e) => failed.push(format!("{encoder} -> {backend}: {e}")),
			}
		}
	}
	eprintln!("{}", table.join("\n"));
	assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// The VA-API decoders that passed their self-test decode on the GPU: every
/// picture of a stream comes out of a VA-API surface, none from FFmpeg's
/// software path; at 1920x1080 too (1088 coded rows, cropped). Skipped
/// where no VA-API decoder works.
#[cfg(target_os = "linux")]
#[test]
fn vaapi_decodes_on_the_gpu() {
	if ffmpeg().is_none() {
		return;
	}
	let names: Vec<&str> =
		DECODERS.iter().map(|d| d.name).filter(|n| n.ends_with("_vaapi") && available(n)).collect();
	if names.is_empty() {
		eprintln!("no usable VA-API decoder, skipped");
		return;
	}
	let codecs = Codecs::new();
	for name in &names {
		let spec = DecoderSpec::by_name(name).unwrap();
		if codecs.encoders().iter().all(|(c, _)| *c != spec.codec) {
			eprintln!("{name}: nothing here encodes {}, skipped", spec.codec);
			continue;
		}
		for (w, h) in [(320, 240), (1920, 1080)] {
			let stream = encode(spec.codec, "auto", w, h, 10);
			let mut decoder = FfmpegDecoder::new(spec).unwrap();
			let screen = SyntheticScreen::new(w, h);
			let started = Instant::now();
			let (decoded, psnr) = decode(&mut decoder, &stream, &screen).unwrap();
			eprintln!(
				"{name} {w}x{h}: {decoded} pictures, {} on the GPU, PSNR {psnr:.1} dB, {:.1} ms each",
				decoder.gpu_pictures(),
				started.elapsed().as_secs_f64() * 1000.0 / decoded.max(1) as f64
			);
			assert_eq!(decoded, stream.len(), "{name}");
			assert_eq!(decoder.gpu_pictures(), decoded as u64, "{name}: not on the GPU");
			assert_eq!(decoder.cpu_pictures(), 0, "{name}");
			assert!(psnr > 28.0, "{name}: {psnr:.1} dB");
		}
	}
}

/// What a viewer accepts with FFmpeg and without (`VOELIN_FFMPEG=0`, in a
/// child process: FFmpeg is loaded once per process).
#[test]
fn decoders_with_and_without_ffmpeg() {
	let without = std::env::var("VOELIN_FFMPEG").is_ok_and(|v| v == "0");
	let codecs = Codecs::new();
	let decoders = codecs.decoders();
	eprintln!("VOELIN_FFMPEG={without}: {decoders:?}");
	if without {
		// Only the built-in decoders: libvpx (and dav1d with `av1`).
		assert!(Ffmpeg::get().is_err());
		assert!(decoder::probe().is_empty());
		assert!(!decoders.contains(&Codec::H265) && !decoders.contains(&Codec::H264));
		assert_eq!(decoders.contains(&Codec::Av1), cfg!(feature = "av1"));
		#[cfg(feature = "vpx")]
		assert_eq!(decoders[decoders.len() - 2..], [Codec::Vp9, Codec::Vp8]);
		let ladder = codecs.decoders_for(Codec::Vp9);
		assert!(ladder.iter().all(|b| !matches!(b, DecoderBackend::Ffmpeg(_))), "{ladder:?}");
		return;
	}
	if ffmpeg().is_some() {
		// The software decoders of every FFmpeg build.
		for codec in [Codec::H265, Codec::Vp9, Codec::H264, Codec::Vp8] {
			assert!(decoders.contains(&codec), "{codec}: {decoders:?}");
		}
		// The viewer's order: best first.
		let order: Vec<usize> = decoders
			.iter()
			.map(|c| voelin_media::codec::VIEWER_PREFERENCE.iter().position(|p| p == c).unwrap())
			.collect();
		assert!(order.windows(2).all(|w| w[0] < w[1]), "{decoders:?}");
	}
	let child = std::process::Command::new(std::env::current_exe().unwrap())
		.args(["--exact", "decoders_with_and_without_ffmpeg", "--nocapture"])
		.env("VOELIN_FFMPEG", "0")
		.status()
		.unwrap();
	assert!(child.success(), "without FFmpeg: {child}");
}

/// The access units of an Annex B H.264 stream that has access unit
/// delimiters (`aud=1` in x264, `-aud 1` in VA-API), as a stream's frames.
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

/// A 32x18 luma thumbnail (block averages), to compare pictures cheaply.
fn thumbnail(picture: &VideoFrame) -> Vec<u8> {
	let y = match &picture.data {
		voelin_media::FrameData::I420 { y, .. } | voelin_media::FrameData::Nv12 { y, .. } => y,
		_ => panic!("not YUV"),
	};
	let (w, bw, bh) =
		(picture.width as usize, picture.width as usize / 32, picture.height as usize / 18);
	let mut out = Vec::with_capacity(32 * 18);
	for by in 0..18 {
		for bx in 0..32 {
			let sum: u64 = (by * bh..(by + 1) * bh)
				.map(|row| {
					y.row(row, w)[bx * bw..(bx + 1) * bw].iter().map(|&p| u64::from(p)).sum::<u64>()
				})
				.sum();
			out.push((sum / (bw * bh) as u64) as u8);
		}
	}
	out
}

/// Make `path` with the `ffmpeg` command from `args` (output options
/// last), unless it is there; whether it is.
fn ffmpeg_command(path: &std::path::Path, args: &[&str]) -> bool {
	if path.exists() {
		return true;
	}
	let made = std::process::Command::new("ffmpeg")
		.args(["-hide_banner", "-loglevel", "error", "-y"])
		.args(args)
		.arg(path)
		.status();
	if !made.is_ok_and(|s| s.success()) {
		let _ = std::fs::remove_file(path);
		return false;
	}
	true
}

/// H.264 with B-frames at 2560x1440 and 60 fps, like the official client's
/// stream (AMF with B-frames and rare keyframes): every H.264 decoder of
/// the ladder, and Cisco's OpenH264 with `VOELIN_OPENH264_LIB`, decodes it;
/// how fast, the slowest frame, and whether every picture comes out, in
/// order (against FFmpeg's software decoder). The streams come from the
/// `ffmpeg` command: x264 with 3 B-frames, `h264_vaapi` with 2, ten
/// seconds of `testsrc2` at 8 Mbit/s, keyframes every ten seconds. Run with
/// `--release`.
#[test]
#[ignore = "a measurement: needs the ffmpeg command; run with --release"]
fn h264_with_b_frames_at_1440p60() {
	if ffmpeg().is_none() {
		return;
	}
	let dir = std::env::temp_dir().join("voelin-h264-bframes");
	std::fs::create_dir_all(&dir).unwrap();
	let source = "-f lavfi -i testsrc2=size=2560x1440:rate=60 -t 10";
	let rate = "-g 600 -b:v 8M -maxrate 9M -bufsize 9M -f h264";
	let x264 = "-c:v libx264 -preset veryfast -profile:v high -bf 3 -x264-params aud=1";
	let vaapi = "-vf format=nv12,hwupload -c:v h264_vaapi -profile:v high -bf 2 -aud 1";
	let mut streams = Vec::new();
	for (name, args) in [
		("x264", format!("{source} {x264} -pix_fmt yuv420p {rate}")),
		("h264_vaapi", format!("-vaapi_device /dev/dri/renderD128 {source} {vaapi} {rate}")),
	] {
		let path = dir.join(format!("{name}-bframes-1440p60.h264"));
		let args: Vec<&str> = args.split(' ').collect();
		if ffmpeg_command(&path, &args) {
			streams.push((name, std::fs::read(&path).unwrap()));
		} else {
			eprintln!("{name}: the ffmpeg command did not make the stream, skipped");
		}
	}
	let codecs = Codecs::new();
	let openh264 = std::env::var_os("VOELIN_OPENH264_LIB")
		.map(|path| voelin_media::codec::h264::OpenH264::load(path).unwrap());
	let mut names: Vec<String> =
		codecs.decoders_for(Codec::H264).iter().map(|b| b.to_string()).collect();
	names.extend(openh264.as_ref().map(|_| "openh264".to_owned()));
	for (stream, data) in &streams {
		let units = access_units(data);
		let mut reference: Option<Vec<Vec<u8>>> = None;
		for name in &names {
			let mut decoder: Box<dyn VideoDecoder> = match (name.as_str(), &openh264) {
				("openh264", Some(library)) => Box::new(library.decoder().unwrap()),
				_ => {
					let backend =
						codecs.decoders_for(Codec::H264).into_iter().find(|b| b.name() == name);
					codecs.new_decoder_with(Codec::H264, backend.unwrap()).unwrap()
				}
			};
			let mut picture = VideoFrame::black_i420(0, 0);
			let mut thumbnails = Vec::with_capacity(units.len());
			let (mut errors, mut slowest, mut busy) = (0, Duration::ZERO, Duration::ZERO);
			for unit in &units {
				let started = Instant::now();
				let result = decoder.decode_into(unit, &mut picture);
				let took = started.elapsed();
				(slowest, busy) = (slowest.max(took), busy + took);
				match result {
					Ok(true) => {
						assert_eq!((picture.width, picture.height), (2560, 1440), "{name}");
						thumbnails.push(thumbnail(&picture));
					}
					Ok(false) => {}
					Err(_) => errors += 1,
				}
			}
			let fps = thumbnails.len() as f64 / busy.as_secs_f64();
			let differing = reference.as_ref().map_or(0, |reference| {
				let differs = |(a, b): &(&Vec<u8>, &Vec<u8>)| {
					a.iter().zip(*b).any(|(x, y)| x.abs_diff(*y) > 3)
				};
				thumbnails.iter().zip(reference).filter(differs).count()
			});
			eprintln!(
				"{stream} 2560x1440@60 with B-frames, {name}: {} of {} pictures, {errors} errors, \
				 {fps:.0} fps, slowest frame {:.1} ms, {differing} differ from FFmpeg's software decoder",
				thumbnails.len(),
				units.len(),
				slowest.as_secs_f64() * 1000.0,
			);
			if name == "h264" {
				reference = Some(thumbnails);
			} else if name.ends_with("_vaapi") {
				assert_eq!(errors, 0, "{name}");
				assert!(thumbnails.len() + 3 >= units.len(), "{name}: pictures missing");
			}
		}
	}
	// What a viewer adds per picture: the NV12 or I420 picture to RGBA.
	let mut rgba = vec![0; 2560 * 1440 * 4];
	let i420 = VideoFrame::black_i420(2560, 1440);
	let started = Instant::now();
	for _ in 0..30 {
		convert::to_rgba(&i420, &mut rgba, 2560 * 4).unwrap();
	}
	eprintln!("to RGBA at 2560x1440: {:.1} ms", started.elapsed().as_secs_f64() * 1000.0 / 30.0);
}
