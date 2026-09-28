//! Synthetic capture -> encoder -> decoder, through the public API.
#![cfg(feature = "vpx")]

use std::time::Duration;

use voelin_media::capture::synthetic::{BACKGROUND, RECT_COLOR, SyntheticScreen, TEXT_COLOR};
use voelin_media::{
	CaptureOptions, Codec, Codecs, EncoderBackend, EncoderConfig, ScreenCapture, SourceId,
	VideoFrame, convert,
};

const W: u32 = 320;
const H: u32 = 240;
const FPS: u32 = 30;

fn rgb_at(frame: &VideoFrame, x: u32, y: u32) -> [u8; 3] {
	let rgba = convert::to_rgba_vec(frame).unwrap();
	let i = ((y * frame.width + x) * 4) as usize;
	[rgba[i], rgba[i + 1], rgba[i + 2]]
}

fn assert_close(actual: [u8; 3], expected: [u8; 3], tolerance: i32, what: &str) {
	let ok = (0..3).all(|c| (i32::from(actual[c]) - i32::from(expected[c])).abs() <= tolerance);
	assert!(ok, "{what}: got {actual:?}, expected {expected:?} ±{tolerance}");
}

/// Encode 30 frames of the pattern, force a keyframe midway, lower the
/// bitrate, and check every decoded frame.
fn roundtrip(codec: Codec) {
	let codecs = Codecs::builtin();
	let screen = SyntheticScreen::new(W, H);
	let config = EncoderConfig { fps: FPS, bitrate_bps: 1_500_000, ..EncoderConfig::default() };
	let mut encoder = codecs.new_encoder(codec, config).unwrap();
	assert_eq!(encoder.backend(), EncoderBackend::Libvpx);
	let mut decoder = codecs.new_decoder(codec).unwrap();
	let mut worst = f64::INFINITY;
	for n in 0..30 {
		if n == 20 {
			encoder.set_bitrate(600_000).unwrap();
		}
		let source = screen.frame(n, FPS);
		let encoded = encoder.encode(&source, n == 15).unwrap();
		assert_eq!(encoded.len(), 1, "{codec} frame {n}");
		let encoded = &encoded[0];
		assert_eq!(encoded.keyframe, n == 0 || n == 15, "{codec} frame {n}");
		assert_eq!(encoded.pts_90khz, source.pts_90khz());
		let decoded = decoder.decode(&encoded.data).unwrap().expect("a picture per frame");
		assert_eq!((decoded.width, decoded.height), (W, H));
		worst = worst.min(convert::psnr(&source, &decoded).unwrap());

		// The rectangle is where the pattern put it, in its colour.
		let (x, y, w, h) = screen.rect(n);
		assert_close(rgb_at(&decoded, x + w / 2, y + h / 2), RECT_COLOR, 16, "rectangle");
		assert_close(rgb_at(&decoded, W - 10, H - 10), BACKGROUND, 16, "background");
	}
	assert!(worst > 30.0, "{codec}: worst PSNR {worst:.1} dB");
}

#[test]
fn vp8_synthetic_roundtrip() {
	roundtrip(Codec::Vp8);
}

#[test]
fn vp9_synthetic_roundtrip() {
	roundtrip(Codec::Vp9);
}

/// Frames from the capture thread go through encode / decode in order.
#[tokio::test]
async fn capture_thread_to_decoder() {
	let codecs = Codecs::builtin();
	let mut screen = SyntheticScreen::new(W, H);
	let options = CaptureOptions { fps: 60, cursor: false, queue: 8 };
	let mut frames = screen.start(&SourceId::Synthetic, &options).await.unwrap();
	let config = EncoderConfig { fps: 60, bitrate_bps: 1_000_000, ..EncoderConfig::default() };
	let mut encoder = codecs.new_encoder(Codec::Vp8, config).unwrap();
	let mut decoder = codecs.new_decoder(Codec::Vp8).unwrap();
	let mut last_pts = None;
	for _ in 0..10 {
		let frame =
			tokio::time::timeout(Duration::from_secs(5), frames.recv()).await.unwrap().unwrap();
		let encoded = encoder.encode(&frame, false).unwrap();
		for e in &encoded {
			assert!(last_pts.is_none_or(|p| e.pts_90khz > p));
			last_pts = Some(e.pts_90khz);
			let decoded = decoder.decode(&e.data).unwrap().unwrap();
			// The counter's first digit (3x5 cells of 5 px) sits at (10, 10).
			let rgba = convert::to_rgba_vec(&decoded).unwrap();
			let lit = (10..25u32)
				.flat_map(|x| (10..35u32).map(move |y| (x, y)))
				.filter(|&(x, y)| {
					let p = &rgba[((y * W + x) * 4) as usize..][..3];
					(0..3).all(|c| (i32::from(p[c]) - i32::from(TEXT_COLOR[c])).abs() < 60)
				})
				.count();
			assert!(lit >= 25, "counter digit missing ({lit} lit pixels)");
		}
	}
	screen.stop();
}
