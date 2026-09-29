//! Encoders through the system's FFmpeg (loaded at runtime), decoded with
//! this crate's own decoders: H.264 with Cisco's OpenH264
//! (`VOELIN_OPENH264_LIB`), AV1 with dav1d (`--features av1`).
//!
//! Skipped without FFmpeg. Hardware backends need their GPU and driver; on a
//! machine without them the self-test reports why and they are skipped.
#![cfg(feature = "ffmpeg")]

use std::time::Duration;

use voelin_media::capture::synthetic::SyntheticScreen;
use voelin_media::ffmpeg::{BACKENDS, Ffmpeg, FfmpegEncoder, probe};
use voelin_media::{
	Codec, Codecs, EncoderBackend, EncoderConfig, VideoDecoder, VideoEncoder, convert,
};

const W: u32 = 320;
const H: u32 = 240;

fn ffmpeg() -> Option<&'static Ffmpeg> {
	match Ffmpeg::get() {
		Ok(f) => Some(f),
		Err(e) => {
			eprintln!("FFmpeg not available ({e}), skipped");
			None
		}
	}
}

fn decoder(codec: Codec) -> Option<Box<dyn VideoDecoder>> {
	match codec {
		#[cfg(feature = "openh264")]
		Codec::H264 => {
			let path = std::env::var_os("VOELIN_OPENH264_LIB")?;
			let library = voelin_media::codec::h264::OpenH264::load(path).ok()?;
			Some(Box::new(library.decoder().unwrap()))
		}
		#[cfg(feature = "av1")]
		Codec::Av1 => Some(Box::new(voelin_media::codec::av1::Dav1dDecoder::new().unwrap())),
		_ => None,
	}
}

/// Twelve frames of the moving test pattern, a keyframe forced at 8, a
/// bitrate change at 5: keyframes where asked, timestamps kept, and (with a
/// decoder) every picture decodable at PSNR > 28 dB.
fn roundtrip(name: &'static str) {
	let screen = SyntheticScreen::new(W, H);
	let config = EncoderConfig { fps: 30, bitrate_bps: 1_500_000, ..EncoderConfig::default() };
	let mut encoder = FfmpegEncoder::new(name, config).unwrap();
	assert_eq!(encoder.backend(), EncoderBackend::Ffmpeg(name));
	let codec = encoder.codec();
	let mut decoder = decoder(codec);
	let mut keyframes = Vec::new();
	let mut sent = Vec::new();
	let mut decoded = 0;
	let mut sources = Vec::new();
	// Frames in until the first packet came out (the encoder's delay).
	let mut delay = None;
	// Until frame 11 came out (encoders with a delay need more input).
	for n in 0..60u32 {
		if sources.len() > 11 && sent.len() >= 12 {
			break;
		}
		let frame = screen.frame(u64::from(n), 30);
		if n == 5 {
			encoder.set_bitrate(800_000).unwrap();
		}
		let out = encoder.encode(&frame, n == 8).unwrap();
		sources.push(frame);
		if delay.is_none() && !out.is_empty() {
			delay = Some(n);
		}
		for f in &out {
			sent.push(f.pts_90khz);
			// Encoders with a delay hand out older frames: match by time.
			let index = sources.iter().position(|s| s.pts_90khz() == f.pts_90khz).unwrap();
			if f.keyframe {
				keyframes.push(index);
			}
			if let Some(decoder) = &mut decoder
				&& let Some(picture) = decoder.decode(&f.data).unwrap()
			{
				assert_eq!((picture.width, picture.height), (W, H));
				let psnr = convert::psnr(&sources[index], &picture).unwrap();
				assert!(psnr > 28.0, "{name} frame {index}: PSNR {psnr:.1} dB");
				decoded += 1;
			}
		}
	}
	eprintln!(
		"{name}: {} packets from {} frames (first after {delay:?}), keyframes at {keyframes:?}, decoded {decoded}",
		sent.len(),
		sources.len()
	);
	assert!(sent.len() >= 8, "{name}: only {} packets", sent.len());
	assert_eq!(keyframes.first(), Some(&0), "{name}");
	assert!(keyframes.contains(&8), "{name}: forced keyframe missing: {keyframes:?}");
	assert!(sent.windows(2).all(|w| w[0] < w[1]), "{name}: timestamps {sent:?}");
	assert_eq!(sent[0], 0);
	if decoder.is_some() {
		assert!(decoded >= 8, "{name}: decoded {decoded}");
	}
}

fn available(name: &str) -> bool {
	probe().iter().any(|s| s.spec.name == name && s.available.is_ok())
}

#[test]
fn probe_reports_every_backend() {
	let Some(ffmpeg) = ffmpeg() else { return };
	eprintln!("{:?}", ffmpeg.info());
	let statuses = probe();
	assert_eq!(statuses.len(), BACKENDS.len());
	for status in statuses {
		eprintln!("{:>18}: {:?}", status.spec.name, status.available);
		if let Err(reason) = &status.available {
			assert!(!reason.is_empty());
		}
	}
	// The report lists them with the built-in ones.
	let report = Codecs::new().report();
	assert!(report.ffmpeg.is_ok(), "{report:?}");
	assert!(report.encoders.iter().any(|e| e.name == "libvpx"));
	for status in statuses {
		let info = report.encoders.iter().find(|e| e.name == status.spec.name).unwrap();
		assert_eq!(info.status, status.available);
		let automatic = status.available.is_ok() && status.spec.is_automatic();
		assert_eq!(info.rank.is_some(), automatic, "{}", status.spec.name);
	}
}

#[test]
fn software_encoders_roundtrip() {
	if ffmpeg().is_none() {
		return;
	}
	let mut tested = Vec::new();
	for name in ["libx264", "libopenh264", "libsvtav1", "librav1e", "libaom-av1"] {
		if available(name) {
			roundtrip(name);
			tested.push(name);
		} else {
			eprintln!("{name} skipped: not available");
		}
	}
	eprintln!("tested: {tested:?}");
}

#[test]
fn x264_is_constrained_high_and_changes_size() {
	if ffmpeg().is_none() || !available("libx264") {
		return;
	}
	let config = EncoderConfig { fps: 30, bitrate_bps: 1_000_000, ..EncoderConfig::default() };
	let mut encoder = FfmpegEncoder::new("libx264", config).unwrap();
	let screen = SyntheticScreen::new(W, H);
	let first = encoder.encode(&screen.frame(0, 30), false).unwrap();
	// profile_idc 100 (High) without B-frames: what TeamSpeak decodes.
	let sps =
		first[0].data.windows(5).find(|w| w[..4] == [0, 0, 1, 0x67] || w[..4] == [0, 0, 0, 1]);
	assert!(sps.is_some());
	let profile = h264_profile(&first[0].data);
	assert_eq!(profile, Some(100), "High profile");
	// A new size: a new session, starting with a keyframe.
	let small = SyntheticScreen::new(160, 120);
	let out = encoder.encode(&small.frame(1, 30), false);
	assert!(out.unwrap().iter().any(|f| f.keyframe));
	// Odd sizes are cropped to even ones instead of failing.
	let odd =
		voelin_media::VideoFrame::black_i420(201, 151).with_timestamp(Duration::from_millis(66));
	assert!(encoder.encode(&odd, false).unwrap().iter().any(|f| f.keyframe));
	// Baseline on request.
	let baseline = EncoderConfig {
		h264_profile: voelin_media::H264Profile::ConstrainedBaseline,
		..EncoderConfig::default()
	};
	let mut encoder = FfmpegEncoder::new("libx264", baseline).unwrap();
	let out = encoder.encode(&screen.frame(0, 30), false).unwrap();
	assert_eq!(h264_profile(&out[0].data), Some(66));
}

/// `profile_idc` of the first SPS in an Annex B stream.
fn h264_profile(data: &[u8]) -> Option<u8> {
	let mut i = 0;
	while i + 4 < data.len() {
		if data[i..i + 3] == [0, 0, 1] {
			if data[i + 3] & 0x1f == 7 {
				return data.get(i + 4).copied();
			}
			i += 3;
		} else {
			i += 1;
		}
	}
	None
}

#[test]
fn codecs_prefer_ffmpeg_h264_over_nothing() {
	if ffmpeg().is_none() || !available("libx264") {
		return;
	}
	let codecs = Codecs::new();
	assert!(codecs.encoder_codecs().contains(&Codec::H264));
	let encoder = codecs.new_encoder(Codec::H264, EncoderConfig::default()).unwrap();
	assert!(matches!(encoder.backend(), EncoderBackend::Ffmpeg(_)));
	let named =
		voelin_media::EncoderPreference { hardware: false, backend: "libx264".parse().unwrap() };
	let encoder = codecs.new_encoder_preferring(Codec::H264, EncoderConfig::default(), &named);
	assert_eq!(encoder.unwrap().backend(), EncoderBackend::Ffmpeg("libx264"));
}

/// Unknown names and hardware without its device fail with a reason, not
/// a panic.
#[test]
fn unusable_backends_fail_cleanly() {
	if ffmpeg().is_none() {
		return;
	}
	assert!(FfmpegEncoder::new("no_such_encoder", EncoderConfig::default()).is_err());
	for status in probe().iter().filter(|s| s.available.is_err()) {
		let Ok(mut encoder) = FfmpegEncoder::new(status.spec.name, EncoderConfig::default()) else {
			continue;
		};
		let frame = voelin_media::VideoFrame::black_i420(W, H);
		assert!(encoder.encode(&frame, true).is_err(), "{} passed now", status.spec.name);
	}
}

/// A backend whose wrapper cannot change the bitrate running (libaom here,
/// VA-API alike) waits for the next keyframe, but reopens at once when the
/// target falls below half.
#[test]
fn bitrate_changes_without_live_reconfiguration() {
	if ffmpeg().is_none() || !available("libaom-av1") {
		return;
	}
	let screen = SyntheticScreen::new(W, H);
	let config = EncoderConfig { fps: 30, bitrate_bps: 1_000_000, ..EncoderConfig::default() };
	let mut encoder = FfmpegEncoder::new("libaom-av1", config).unwrap();
	let key = |encoder: &mut FfmpegEncoder, n: u64, force: bool| {
		encoder.encode(&screen.frame(n, 30), force).unwrap().iter().any(|f| f.keyframe)
	};
	assert!(key(&mut encoder, 0, false));
	assert!(!key(&mut encoder, 1, false));
	encoder.set_bitrate(800_000).unwrap();
	assert!(!key(&mut encoder, 2, false), "a small change waits for a keyframe");
	encoder.set_bitrate(300_000).unwrap();
	assert!(key(&mut encoder, 3, false), "congestion: at once");
	encoder.set_bitrate(400_000).unwrap();
	assert!(!key(&mut encoder, 4, false));
	assert!(key(&mut encoder, 5, true));
	assert!(!key(&mut encoder, 6, false));
}

/// DMA-BUF import is for VA-API encoders and NV12 buffers; anything else is
/// refused as unavailable, so the caller maps the buffer instead.
#[cfg(target_os = "linux")]
#[test]
fn dmabuf_import_is_refused_cleanly() {
	use voelin_media::capture::{DRM_MOD_LINEAR, DmaBufRef, drm_fourcc};
	if ffmpeg().is_none() {
		return;
	}
	let frame = DmaBufRef {
		width: W,
		height: H,
		timestamp: Duration::ZERO,
		fourcc: drm_fourcc(b"XR24"),
		modifier: DRM_MOD_LINEAR,
		fd: -1,
		size: (W * H * 4) as usize,
		planes: [(0, (W * 4) as usize), (0, 0), (0, 0), (0, 0)],
		plane_count: 1,
	};
	for name in ["libx264", "h264_vaapi"] {
		let mut encoder = FfmpegEncoder::new(name, EncoderConfig::default()).unwrap();
		let err = encoder.encode_dmabuf(&frame, true, &mut |_| {}).unwrap_err();
		assert!(matches!(err, voelin_media::Error::CodecUnavailable { .. }), "{name}: {err}");
	}
}
