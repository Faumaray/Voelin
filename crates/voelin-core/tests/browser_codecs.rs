//! The stream's real encoders against a browser's WebRTC stack: the test
//! pattern, encoded by each encoder this machine has as a share encodes the
//! screen, sent by our streamer peer offering just that codec, must decode
//! at its size in headless Chromium (libwebrtc, the stack the official
//! TeamSpeak 6 client is built on). And the other way: what Chromium sends
//! (AV1, VP9, H.264) our viewer accepts and the engine's pipeline decodes.
//!
//! Runs only with `VOELIN_INTEROP=1` (see `tests/interop/README.md`). Codecs
//! the browser does not decode (H.264 in Playwright's Chromium, HEVC) are
//! skipped; `VOELIN_CHROMIUM=/usr/bin/chromium` tests a system Chromium that
//! decodes H.264.

#![cfg(feature = "media-desktop")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::time::timeout;
use voelin_core::media::voelin_media::capture::SourceId;
use voelin_core::media::voelin_media::{Codec, Codecs};
use voelin_core::media::{
	EncodedSource, EncoderPreference, Streamer, StreamerConfig, VideoPipeline, decoded_everywhere,
	media_codec, peer_config, preferred_codec, video_codec,
};
use voelin_core::stream::{FrameSource, PeerConfig, VideoCodec};
use voelin_stream::{Peer, PeerEvent};

#[path = "../../../tests/interop/browser.rs"]
mod browser;
use browser::{Browser, enabled};

const SIZE: (u32, u32) = (1280, 720);

/// The test pattern encoded with `codec` by the encoders `preference` picks
/// (from DMA-BUFs with `dmabuf`, as the ScreenCast portal hands over the
/// screen).
async fn test_pattern(preference: EncoderPreference, codec: Codec, dmabuf: bool) -> EncodedSource {
	let codecs = Codecs::new().with_preference(preference.clone());
	let config = StreamerConfig {
		source: SourceId::Synthetic,
		synthetic_size: SIZE,
		synthetic_dmabuf: dmabuf,
		fps: 30,
		bitrate_kbps: 3000,
		audio: false,
		codec,
		encoder: preference,
		..StreamerConfig::default()
	};
	EncodedSource::new(Streamer::start(&codecs, config).await.unwrap())
}

/// Our streamer peer with `config`'s offer, answered by the browser and
/// connected; the codec the answer chose, `None` if it took no video.
async fn connect(browser: &mut Browser, config: &PeerConfig) -> Option<(Peer, VideoCodec)> {
	let (mut peer, offer) = Peer::offer(config, "interop").await.unwrap();
	let answer = browser.call(json!({ "op": "answer", "sdp": offer })).await;
	peer.accept_answer(answer["sdp"].as_str().unwrap()).await.ok()?;
	let mut chosen = None;
	timeout(Duration::from_secs(15), async {
		loop {
			match peer.next_event().await {
				Some(PeerEvent::VideoCodec(codec)) => chosen = Some(codec),
				Some(PeerEvent::Connected) => return,
				Some(PeerEvent::Closed) | None => panic!("closed before connecting"),
				_ => {}
			}
		}
	})
	.await
	.expect("no connection with the browser");
	Some((peer, chosen?))
}

/// Three seconds of `source` to `peer`; the browser's `inbound-rtp` video
/// stats.
async fn send(browser: &mut Browser, peer: &mut Peer, source: &mut EncodedSource) -> Value {
	source.request_keyframe();
	let mut frames = Vec::new();
	let end = Instant::now() + Duration::from_secs(3);
	while Instant::now() < end {
		source.poll_frames(Instant::now(), &mut frames);
		for f in frames.drain(..) {
			peer.write(f.kind, f.time, f.data);
		}
		while let Some(event) = peer.try_next_event() {
			if matches!(event, PeerEvent::KeyframeRequest) {
				source.request_keyframe();
			}
		}
		tokio::time::sleep(Duration::from_millis(5)).await;
	}
	let stats = browser.call(json!({ "op": "stats" })).await;
	peer.close();
	stats["inbound"]["video"].clone()
}

/// Whether the browser decoded most of three seconds at the stream's size.
fn decoded(video: &Value) -> Result<(), String> {
	let frames = video["framesDecoded"].as_u64().unwrap_or(0);
	let size = (video["frameWidth"].as_u64(), video["frameHeight"].as_u64());
	let expected = (Some(u64::from(SIZE.0)), Some(u64::from(SIZE.1)));
	if frames < 60 || size != expected {
		return Err(format!("{frames} frames decoded at {size:?}"));
	}
	Ok(())
}

/// Every encoder of this machine (hardware ones from memory and from
/// DMA-BUFs) decodes in the browser: most frames decoded, at the stream's
/// size.
#[tokio::test(flavor = "multi_thread")]
async fn real_encoders_decode_in_browser() {
	if !enabled() {
		eprintln!("skipped: set VOELIN_INTEROP=1 (needs node, Playwright and Chromium)");
		return;
	}
	let mut browser = Browser::start().await;
	let codecs = Codecs::new();
	let udmabuf = std::path::Path::new("/dev/udmabuf").exists();
	let mut failed = Vec::new();
	for (codec, backend) in codecs.encoders() {
		let gpu = codecs.is_hardware(backend) && backend.name().ends_with("_vaapi") && udmabuf;
		for dmabuf in [false, true].into_iter().filter(|d| !d || gpu) {
			let what =
				format!("{codec} {}{}", backend.name(), if dmabuf { " (DMA-BUF)" } else { "" });
			let config = PeerConfig {
				video_codecs: vec![video_codec(codec)],
				audio: false,
				// Headless Chromium lists only the H.264 profiles of
				// libwebrtc's software decoder (Baseline, Constrained
				// Baseline, Main), not the Constrained High we offer; its
				// FFmpeg decoder takes our High profile streams all the same.
				h264_profile_level_id: 0x42e01f,
				..PeerConfig::loopback()
			};
			let Some((mut peer, _)) = connect(&mut browser, &config).await else {
				eprintln!("{what}: the browser does not decode it, skipped");
				continue;
			};
			let preference =
				EncoderPreference { hardware: true, backend: backend.name().parse().unwrap() };
			let mut source = test_pattern(preference, codec, dmabuf).await;
			let video = send(&mut browser, &mut peer, &mut source).await;
			eprintln!("{what}: {video}");
			if let Err(e) = decoded(&video) {
				failed.push(format!("{what}: {e}"));
			}
		}
	}
	browser.quit().await;
	assert!(failed.is_empty(), "not decoded in the browser:\n{}", failed.join("\n"));
}

/// The offer a share makes on this machine (`preferred_codec`,
/// `peer_config`): the browser, which answers like the official client with
/// the first codec of the offer it lists, picks one every TeamSpeak client
/// decodes, never H.264 ahead of them (the official client may list H.264
/// without being able to decode it), and decodes it.
#[tokio::test(flavor = "multi_thread")]
async fn share_offer_picks_a_codec_every_client_decodes() {
	if !enabled() {
		eprintln!("skipped: set VOELIN_INTEROP=1 (needs node, Playwright and Chromium)");
		return;
	}
	let mut browser = Browser::start().await;
	let codecs = Codecs::new();
	let primary = preferred_codec(&codecs, None).expect("an encoder");
	let config = PeerConfig {
		video_codecs: vec![video_codec(primary)],
		audio: false,
		..PeerConfig::loopback()
	};
	let config = peer_config(&codecs, config);
	eprintln!("offer: {:?}", config.video_codecs);
	let (mut peer, chosen) = connect(&mut browser, &config).await.expect("video answered");
	let chosen = media_codec(chosen);
	assert!(
		decoded_everywhere(chosen),
		"the browser chose {chosen} from {:?}",
		config.video_codecs
	);
	let mut source = test_pattern(codecs.preference().clone(), chosen, false).await;
	let video = send(&mut browser, &mut peer, &mut source).await;
	eprintln!("{chosen}: {video}");
	browser.quit().await;
	decoded(&video).unwrap();
}

/// Headless Chromium sends its canvas (320x240) in AV1, VP9 and H.264 in
/// turn (what it cannot send is skipped; a system Chromium sends H.264) to
/// our viewer peer, which accepts what this machine decodes
/// (`peer_config`); the engine's pipeline decodes 30 pictures at that size
/// with the first decoder of the codec's ladder (VA-API where it decodes
/// the codec), asking the browser for keyframes as a watch does.
#[tokio::test(flavor = "multi_thread")]
async fn browser_streams_decode_in_the_pipeline() {
	if !enabled() {
		eprintln!("skipped: set VOELIN_INTEROP=1 (needs node, Playwright and Chromium)");
		return;
	}
	let mut browser = Browser::start().await;
	let codecs = Arc::new(Codecs::new());
	let config = peer_config(&codecs, PeerConfig::loopback());
	let mut decoded = Vec::new();
	for codec in [Codec::Av1, Codec::Vp9, Codec::H264] {
		let offer = browser.call(json!({ "op": "offer", "codec": codec.name() })).await;
		if offer.get("unsupported").is_some() {
			eprintln!("{codec}: the browser cannot send it, skipped");
			continue;
		}
		assert!(config.accept_video_codecs.contains(&video_codec(codec)), "{codec} not accepted");
		let (mut peer, answer) =
			Peer::answer(&config, offer["sdp"].as_str().unwrap()).await.unwrap();
		browser.call(json!({ "op": "accept", "sdp": answer })).await;
		let keyframe = Arc::new(AtomicBool::new(false));
		let (tx, pictures) = std::sync::mpsc::channel();
		let pipeline = VideoPipeline::new(
			codecs.clone(),
			move |picture| {
				let _ = tx.send((picture.width, picture.height));
			},
			{
				let keyframe = keyframe.clone();
				move || keyframe.store(true, Ordering::Relaxed)
			},
		);
		let mut sizes = Vec::new();
		let _ = timeout(Duration::from_secs(15), async {
			while let Some(event) = peer.next_event().await {
				match event {
					PeerEvent::Media(frame) => pipeline.push(frame),
					PeerEvent::Closed => break,
					_ => {}
				}
				if keyframe.swap(false, Ordering::Relaxed) {
					peer.request_keyframe();
				}
				sizes.extend(pictures.try_iter());
				if sizes.len() >= 30 {
					break;
				}
			}
		})
		.await;
		let stats = pipeline.stats();
		peer.close();
		eprintln!("{codec}: {} pictures, {stats:?}", sizes.len());
		assert!(sizes.len() >= 30, "{codec}: {} pictures decoded, {stats:?}", sizes.len());
		assert!(sizes.iter().all(|&s| s == (320, 240)), "{codec}: {sizes:?}");
		assert_eq!(stats.codec, Some(codec));
		assert_eq!(stats.decoder, codecs.decoders_for(codec).first().copied(), "{stats:?}");
		decoded.push(codec);
	}
	browser.quit().await;
	assert!(decoded.contains(&Codec::Av1), "the browser sent no AV1: {decoded:?}");
}
