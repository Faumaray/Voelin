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
