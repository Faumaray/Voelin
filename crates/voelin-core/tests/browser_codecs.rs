//! The stream's real encoders against a browser's WebRTC stack: the test
//! pattern, encoded by each encoder this machine has as a share encodes the
//! screen, sent by our streamer peer offering just that codec (H.264 in both
//! profiles of the ladder, encoded in the one the browser chose), must
//! decode at its size in headless Chromium (libwebrtc, the stack the
//! official TeamSpeak 6 client is built on).
//!
//! Runs only with `VOELIN_INTEROP=1` (see `tests/interop/README.md`). Codecs
//! the browser does not decode (H.264 in Playwright's Chromium, HEVC) are
//! skipped; `VOELIN_CHROMIUM=/usr/bin/chromium` tests a system Chromium that
//! decodes H.264.

#![cfg(feature = "media-desktop")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::time::timeout;
use voelin_core::media::voelin_media::capture::SourceId;
use voelin_core::media::voelin_media::{Codec, Codecs};
use voelin_core::media::{
	EncoderPreference, MediaSink, Streamer, StreamerConfig, decoded_everywhere, media_codec,
	peer_config, preferred_codec, video_codec,
};
use voelin_core::stream::{EncodedFrame, PeerConfig, VideoFormat};
use voelin_stream::{H264Profile, Peer, PeerEvent};

#[path = "../../../tests/interop/browser.rs"]
mod browser;
use browser::{Browser, enabled};

const SIZE: (u32, u32) = (1280, 720);

/// A viewer as the stream task serves it: the frames of the format its
/// answer chose (the streamer makes an encoder of that format), keyframe
/// requests back.
struct ViewerSink {
	format: VideoFormat,
	frames: mpsc::Sender<EncodedFrame>,
	keyframe: AtomicBool,
}

impl MediaSink for ViewerSink {
	fn send(&self, _: EncodedFrame) -> bool {
		true
	}

	fn send_video(&self, frame: EncodedFrame, format: VideoFormat) -> bool {
		format != self.format || self.frames.send(frame).is_ok()
	}

	fn video_codecs(&self, out: &mut Vec<VideoFormat>) {
		out.push(self.format);
	}

	fn take_keyframe_request(&self) -> bool {
		self.keyframe.swap(false, Ordering::Relaxed)
	}
}

/// The streamer, its viewer and the viewer's frames.
type Source = (Streamer, Arc<ViewerSink>, mpsc::Receiver<EncodedFrame>);

/// The test pattern encoded with `codec` by the encoders `preference` picks
/// (from DMA-BUFs with `dmabuf`, as the ScreenCast portal hands over the
/// screen), for a viewer of `format`.
async fn test_pattern(
	preference: EncoderPreference,
	codec: Codec,
	dmabuf: bool,
	format: VideoFormat,
) -> Source {
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
	let streamer = Streamer::start(&codecs, config).await.unwrap();
	let (tx, frames) = mpsc::channel();
	let sink = Arc::new(ViewerSink { format, frames: tx, keyframe: AtomicBool::new(true) });
	streamer.attach(sink.clone());
	(streamer, sink, frames)
}

/// `profile_idc` of the first SPS in an H.264 access unit (Annex B).
fn sps_profile(data: &[u8]) -> Option<u8> {
	let at = data.windows(4).position(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7)?;
	data.get(at + 4).copied()
}

/// Our streamer peer with `config`'s offer, answered by the browser and
/// connected; the format the answer chose, `None` if it took no video.
async fn connect(browser: &mut Browser, config: &PeerConfig) -> Option<(Peer, VideoFormat)> {
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

/// Three seconds of the streamer's frames for its viewer to `peer`; the
/// browser's `inbound-rtp` video stats, and the first frame sent.
async fn send(
	browser: &mut Browser,
	peer: &mut Peer,
	(_streamer, sink, frames): Source,
) -> (Value, Option<EncodedFrame>) {
	let mut first = None;
	let end = Instant::now() + Duration::from_secs(3);
	while Instant::now() < end {
		while let Ok(f) = frames.try_recv() {
			peer.write(f.kind, f.time, f.data.clone());
			first.get_or_insert(f);
		}
		while let Some(event) = peer.try_next_event() {
			if matches!(event, PeerEvent::KeyframeRequest) {
				sink.keyframe.store(true, Ordering::Relaxed);
			}
		}
		tokio::time::sleep(Duration::from_millis(5)).await;
	}
	let stats = browser.call(json!({ "op": "stats" })).await;
	peer.close();
	(stats["inbound"]["video"].clone(), first)
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
/// size. H.264 is offered in both profiles of the ladder; the browser takes
/// the one it lists (headless Chromium: Constrained Baseline only) and gets
/// it from an encoder of that profile.
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
				..PeerConfig::loopback()
			};
			let Some((mut peer, format)) = connect(&mut browser, &config).await else {
				eprintln!("{what}: the browser does not decode it, skipped");
				continue;
			};
			let preference =
				EncoderPreference { hardware: true, backend: backend.name().parse().unwrap() };
			let source = test_pattern(preference, codec, dmabuf, format).await;
			let (video, first) = send(&mut browser, &mut peer, source).await;
			eprintln!("{what}, the browser chose {format}: {video}");
			if let Err(e) = decoded(&video) {
				failed.push(format!("{what}: {e}"));
			}
			// H.264 in the profile the answer chose.
			let expected = match format {
				VideoFormat::H264(H264Profile::ConstrainedHigh) => Some(100),
				VideoFormat::H264(H264Profile::ConstrainedBaseline) => Some(66),
				_ => None,
			};
			let sps = first.as_ref().and_then(|f| sps_profile(&f.data));
			if expected.is_some() && sps != expected {
				failed.push(format!("{what}: SPS profile {sps:?}, {format} answered"));
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
	let (mut peer, format) = connect(&mut browser, &config).await.expect("video answered");
	let chosen = media_codec(format.codec());
	assert!(
		decoded_everywhere(chosen),
		"the browser chose {chosen} from {:?}",
		config.video_codecs
	);
	let source = test_pattern(codecs.preference().clone(), chosen, false, format).await;
	let (video, _) = send(&mut browser, &mut peer, source).await;
	eprintln!("{chosen}: {video}");
	browser.quit().await;
	decoded(&video).unwrap();
}
