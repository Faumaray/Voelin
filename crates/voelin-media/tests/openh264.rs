//! H.264 through Cisco's OpenH264, loaded at runtime.
//!
//! Without the library only the "unavailable" paths run. To test encoding
//! and decoding, point `VOELIN_OPENH264_LIB` at Cisco's library (for example the
//! file the ignored `download_and_roundtrip` test fetches:
//! `cargo test -p voelin-media --test openh264 -- --ignored`).
#![cfg(feature = "openh264")]

use std::path::PathBuf;

use voelin_media::capture::synthetic::{RECT_COLOR, SyntheticScreen};
use voelin_media::codec::h264::{H264Profile, OpenH264};
use voelin_media::{
	Codec, Codecs, EncoderBackend, EncoderConfig, Error, VideoEncoder, VideoFrame, convert,
};

#[test]
fn missing_library_is_a_clear_error() {
	let err = OpenH264::load("/nonexistent/dir/libopenh264.so").unwrap_err();
	assert!(matches!(err, Error::CodecUnavailable { codec: Codec::H264, .. }), "{err}");
	assert!(err.to_string().contains("OpenH264 library not found"), "{err}");

	// Without a library, H.264 is neither decodable nor encodable.
	let codecs = Codecs::builtin();
	assert!(!codecs.decoders().contains(&Codec::H264));
	assert!(!codecs.encoder_codecs().contains(&Codec::H264));
	let err = codecs.new_decoder(Codec::H264).err().unwrap();
	assert!(err.to_string().contains("OpenH264 library is not loaded"), "{err}");
	assert!(codecs.new_encoder(Codec::H264, EncoderConfig::default()).is_err());
}

fn rgb_at(frame: &VideoFrame, x: u32, y: u32) -> [u8; 3] {
	let rgba = convert::to_rgba_vec(frame).unwrap();
	let i = ((y * frame.width + x) * 4) as usize;
	[rgba[i], rgba[i + 1], rgba[i + 2]]
}

/// `profile_idc` of the first SPS in an Annex B stream.
fn sps_profile(data: &[u8]) -> Option<u8> {
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

fn roundtrip(library: OpenH264) {
	let codecs = Codecs::builtin().with_openh264(library.clone());
	assert_eq!(codecs.decoders().last(), Some(&Codec::H264));
	assert!(codecs.encoders().contains(&(Codec::H264, EncoderBackend::OpenH264)));

	let screen = SyntheticScreen::new(320, 240);
	let config = EncoderConfig { fps: 30, bitrate_bps: 1_500_000, ..EncoderConfig::default() };
	let mut encoder = codecs.new_encoder(Codec::H264, config.clone()).unwrap();
	let mut decoder = codecs.new_decoder(Codec::H264).unwrap();
	let mut decoded_frames = 0;
	let mut worst = f64::INFINITY;
	for n in 0..20 {
		if n == 12 {
			encoder.set_bitrate(800_000).unwrap();
		}
		let source = screen.frame(n, 30);
		let encoded = encoder.encode(&source, n == 6).unwrap();
		for e in &encoded {
			if n == 0 {
				assert!(e.keyframe);
				// Constrained High: profile_idc 100.
				assert_eq!(sps_profile(&e.data), Some(100));
			}
			if n == 6 {
				assert!(e.keyframe);
			}
			if let Some(picture) = decoder.decode(&e.data).unwrap() {
				decoded_frames += 1;
				worst = worst.min(convert::psnr(&source, &picture).unwrap());
				let (x, y, w, h) = screen.rect(n);
				let rect = rgb_at(&picture, x + w / 2, y + h / 2);
				let close =
					(0..3).all(|c| (i32::from(rect[c]) - i32::from(RECT_COLOR[c])).abs() <= 16);
				assert!(close, "frame {n}: rectangle {rect:?}");
			}
		}
	}
	assert!(decoded_frames >= 18, "only {decoded_frames} frames decoded");
	assert!(worst > 30.0, "worst PSNR {worst:.1} dB");

	// Constrained Baseline on request: profile_idc 66.
	let mut baseline = library.encoder(config).unwrap();
	baseline.set_profile(H264Profile::ConstrainedBaseline).unwrap();
	let encoded = baseline.encode(&screen.frame(0, 30), true).unwrap();
	assert_eq!(sps_profile(&encoded[0].data), Some(66));
}

#[test]
fn roundtrip_with_library_from_env() {
	let Some(path) = std::env::var_os("VOELIN_OPENH264_LIB") else {
		eprintln!("skipped: VOELIN_OPENH264_LIB is not set");
		return;
	};
	roundtrip(OpenH264::load(PathBuf::from(path)).unwrap());
}

/// Downloads Cisco's binary (network access needed).
#[cfg(feature = "openh264-download")]
#[tokio::test]
#[ignore = "downloads from ciscobinary.openh264.org"]
async fn download_and_roundtrip() {
	if voelin_media::codec::h264::platform_library_name().is_none() {
		return;
	}
	let dir = std::env::temp_dir().join(format!("voelin-media-openh264-{}", std::process::id()));
	let library = voelin_media::codec::h264::download_openh264(&dir).await.unwrap();
	assert!(library.path().starts_with(&dir));
	// A second call reuses the verified file.
	let again = voelin_media::codec::h264::download_openh264(&dir).await.unwrap();
	assert_eq!(again, library);
	tokio::task::spawn_blocking(move || roundtrip(library)).await.unwrap();
	std::fs::remove_dir_all(&dir).unwrap();
}
