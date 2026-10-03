//! Interop with a browser WebRTC stack: headless Chromium (Playwright) is the
//! other peer, driven by `tests/interop/browser-peer.cjs` over stdin/stdout.
//! No TeamSpeak server is involved; SDP goes straight between the peers.
//!
//! Runs only with `VOELIN_INTEROP=1` (see `tests/interop/README.md`).

use std::time::{Duration, Instant};

use serde_json::json;
use tokio::time::timeout;
use voelin_stream::{
	Codec, FrameSource, LayerSpec, MediaKind, Peer, PeerConfig, PeerEvent, Signal, SrtpProfile,
	SyntheticSource,
};

#[path = "../../../tests/interop/browser.rs"]
mod browser;
use browser::{Browser, enabled};
mod relay;
use relay::{Relay, Rule};

/// The `srtpCipher` of `getStats()` for AES_CM_128_HMAC_SHA1_80, as Chromium
/// 141 and 152 name it.
fn aes_cm_128_sha1_80(cipher: &serde_json::Value) -> bool {
	["AES_CM_128_HMAC_SHA1_80", "SRTP_AES128_CM_HMAC_SHA1_80"].iter().any(|name| cipher == name)
}

async fn wait_connected(peer: &mut Peer) {
	timeout(Duration::from_secs(15), async {
		loop {
			match peer.next_event().await {
				Some(PeerEvent::Connected) => return,
				Some(PeerEvent::Closed) | None => panic!("closed before connecting"),
				_ => {}
			}
		}
	})
	.await
	.expect("no connection with the browser");
}

/// Our streamer peer (as in a TeamSpeak stream) sends VP8 + Opus to Chromium,
/// which answers as DTLS client: our SRTP order gives AES_CM_128_HMAC_SHA1_80,
/// and its transport-cc feedback drives our bandwidth estimation and pacer.
#[tokio::test(flavor = "multi_thread")]
async fn rust_streams_to_browser() {
	if !enabled() {
		eprintln!("skipped: set VOELIN_INTEROP=1 (needs node, Playwright and Chromium)");
		return;
	}
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let mut browser = Browser::start().await;
	let config = PeerConfig::loopback();
	let (mut peer, offer) = Peer::offer(&config, "interop").await.unwrap();
	// As a Voelin streamer with several layers offers: with the list of its
	// layers, which libwebrtc (the official client's stack) must skip.
	let layers = [
		LayerSpec::single(4_000_000),
		LayerSpec { id: 1, scale: 0.5, ..LayerSpec::single(1_000_000) },
	];
	let offer = voelin_stream::layer::add_to_sdp(&offer, &layers);
	assert!(offer.contains("\r\na=x-voelin-layers:0/1/4000000 1/0.5/1000000\r\n"), "{offer}");
	let answer = browser.call(json!({ "op": "answer", "sdp": offer })).await;
	let answer = answer["sdp"].as_str().unwrap();
	eprintln!("browser answer:\n{answer}");
	peer.accept_answer(answer).await.unwrap();
	wait_connected(&mut peer).await;

	// Three seconds of synthetic media.
	let mut source = SyntheticSource::new(30, 3000, true);
	let mut frames = Vec::new();
	let mut keyframe_requests = 0;
	let mut estimates = Vec::new();
	let end = Instant::now() + Duration::from_secs(3);
	while Instant::now() < end {
		source.poll_frames(Instant::now(), &mut frames);
		for f in frames.drain(..) {
			peer.write(f.kind, f.time, f.data);
		}
		while let Some(event) = peer.try_next_event() {
			match event {
				PeerEvent::KeyframeRequest => keyframe_requests += 1,
				PeerEvent::BitrateEstimate(bitrate) => estimates.push(bitrate),
				_ => {}
			}
		}
		tokio::time::sleep(Duration::from_millis(10)).await;
	}
	let stats = browser.call(json!({ "op": "stats" })).await;
	eprintln!("browser stats: {stats:#}\nkeyframe requests from the browser: {keyframe_requests}");
	eprintln!("bandwidth estimates (bit/s): {estimates:?}");
	assert_eq!(stats["connectionState"], "connected");
	assert!(aes_cm_128_sha1_80(&stats["transport"]["srtpCipher"]), "{stats:#}");
	assert_eq!(peer.srtp_profile(), Some(SrtpProfile::Aes128CmSha1_80));
	assert!(!estimates.is_empty(), "no bandwidth estimate from the browser's feedback");
	let video = &stats["inbound"]["video"];
	let audio = &stats["inbound"]["audio"];
	assert_eq!(video["codec"], "video/VP8");
	assert!(video["packetsReceived"].as_u64().unwrap() > 100, "video packets: {video}");
	assert!(video["bytesReceived"].as_u64().unwrap() > 100_000, "video bytes: {video}");
	assert_eq!(audio["codec"], "audio/opus");
	assert!(audio["packetsReceived"].as_u64().unwrap() > 50, "audio packets: {audio}");
	// The synthetic frames are real (tiny) VP8 keyframes.
	assert!(video["framesDecoded"].as_u64().unwrap() > 30, "decoded: {video}");
	browser.quit().await;
}

/// Answer STUN binding requests with a made-up public address, so a peer
/// reports a server-reflexive candidate without internet access.
async fn fake_stun_server() -> std::net::SocketAddr {
	let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
	let addr = socket.local_addr().unwrap();
	tokio::spawn(async move {
		let mut buf = [0; 1500];
		while let Ok((n, from)) = socket.recv_from(&mut buf).await {
			if n < 20 || buf[..2] != [0, 1] {
				continue;
			}
			// Binding success with XOR-MAPPED-ADDRESS 203.0.113.9:40000.
			let mut resp = vec![0x01, 0x01, 0, 12];
			resp.extend_from_slice(&buf[4..20]);
			resp.extend_from_slice(&[0, 0x20, 0, 8, 0, 1]);
			resp.extend_from_slice(&(40000u16 ^ 0x2112).to_be_bytes());
			let ip = u32::from(std::net::Ipv4Addr::new(203, 0, 113, 9)) ^ 0x2112_A442;
			resp.extend_from_slice(&ip.to_be_bytes());
			let _ = socket.send_to(&resp, from).await;
		}
	});
	addr
}

/// A trickled candidate as our sessions send it (`iceCandidate` signal with
/// the peer's mid) must be accepted by the browser.
#[tokio::test(flavor = "multi_thread")]
async fn trickled_candidates_reach_browser() {
	if !enabled() {
		eprintln!("skipped: set VOELIN_INTEROP=1 (needs node, Playwright and Chromium)");
		return;
	}
	let mut browser = Browser::start().await;
	// Primary address (STUN is not sent from loopback) and the fake STUN server.
	let stun = fake_stun_server().await;
	let config = PeerConfig {
		hosts: Vec::new(),
		stun_servers: vec![stun.to_string()],
		..PeerConfig::loopback()
	};
	let (mut peer, offer) = Peer::offer(&config, "interop").await.unwrap();
	let answer = browser.call(json!({ "op": "answer", "sdp": offer })).await;
	peer.accept_answer(answer["sdp"].as_str().unwrap()).await.unwrap();
	let (candidate, mid) = timeout(Duration::from_secs(10), async {
		loop {
			match peer.next_event().await {
				Some(PeerEvent::LocalCandidate { candidate, mid }) => return (candidate, mid),
				Some(PeerEvent::Closed) | None => panic!("closed"),
				_ => {}
			}
		}
	})
	.await
	.expect("no server-reflexive candidate");
	assert!(candidate.contains("203.0.113.9 40000 typ srflx"), "{candidate}");
	let first_mid = offer.lines().find_map(|l| l.strip_prefix("a=mid:")).unwrap().trim();
	assert_eq!(mid.as_deref(), Some(first_mid));
	// Through the signal JSON, as the server relays it.
	let json = Signal::IceCandidate { candidate, mid, mline_index: Some(0) }.to_json();
	let Signal::IceCandidate { candidate, mid, mline_index } = Signal::parse(&json).unwrap() else {
		unreachable!()
	};
	let add = json!({
		"op": "candidate", "candidate": candidate, "sdpMid": mid, "sdpMLineIndex": mline_index
	});
	browser.call(add).await;
	wait_connected(&mut peer).await;
	browser.quit().await;
}

/// Frames per codec.
#[derive(Debug, Default)]
struct Counts(Vec<(Codec, usize)>);

impl Counts {
	fn add(&mut self, codec: Codec) {
		match self.0.iter_mut().find(|(c, _)| *c == codec) {
			Some((_, n)) => *n += 1,
			None => self.0.push((codec, 1)),
		}
	}

	fn get(&self, codec: Codec) -> usize {
		self.0.iter().find(|(c, _)| *c == codec).map_or(0, |(_, n)| *n)
	}

	fn total(&self) -> usize {
		self.0.iter().map(|(_, n)| n).sum()
	}
}

struct Received {
	video: Counts,
	audio: Counts,
	/// VP8 keyframes (frame tag bit 0 clear).
	vp8_keyframes: usize,
}

/// Collect frames from `peer` for up to `limit` or until `enough`.
async fn receive(peer: &mut Peer, limit: Duration, enough: usize) -> Received {
	let mut r = Received { video: Counts::default(), audio: Counts::default(), vp8_keyframes: 0 };
	let _ = timeout(limit, async {
		while let Some(event) = peer.next_event().await {
			if let PeerEvent::Media(frame) = event {
				let map = match frame.kind {
					MediaKind::Video => &mut r.video,
					MediaKind::Audio => &mut r.audio,
				};
				map.add(frame.codec);
				if frame.codec == Codec::Vp8 && frame.data.first().is_some_and(|b| b & 1 == 0) {
					r.vp8_keyframes += 1;
				}
				if r.video.total() >= enough && r.audio.total() >= enough {
					return;
				}
			}
		}
	})
	.await;
	r
}

/// Chromium sends a canvas stream and an oscillator; our viewer peer answers.
#[tokio::test(flavor = "multi_thread")]
async fn browser_streams_to_rust() {
	if !enabled() {
		eprintln!("skipped: set VOELIN_INTEROP=1 (needs node, Playwright and Chromium)");
		return;
	}
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let mut browser = Browser::start().await;
	let config = PeerConfig::loopback();
	let mut tested = Vec::new();
	for (name, codec) in
		[("VP8", Codec::Vp8), ("VP9", Codec::Vp9), ("H264", Codec::H264), ("AV1", Codec::Av1)]
	{
		// VP8 with trickled browser candidates, the others with candidates in the SDP.
		let trickle = codec == Codec::Vp8;
		let offer = browser.call(json!({ "op": "offer", "codec": name, "trickle": trickle })).await;
		if offer.get("unsupported").is_some() {
			eprintln!("{name}: the browser cannot send it, skipped");
			continue;
		}
		let offer = offer["sdp"].as_str().unwrap();
		let (mut peer, answer) = Peer::answer(&config, offer).await.unwrap();
		browser.call(json!({ "op": "accept", "sdp": answer })).await;
		if trickle {
			let candidates = browser.call(json!({ "op": "candidates" })).await;
			let candidates = candidates["candidates"].as_array().unwrap();
			assert!(!candidates.is_empty(), "no browser candidates");
			for c in candidates {
				peer.add_remote_candidate(c["candidate"].as_str().unwrap());
			}
		}
		wait_connected(&mut peer).await;
		// We are the DTLS server here too: our order picks the profile.
		assert_eq!(peer.srtp_profile(), Some(SrtpProfile::Aes128CmSha1_80), "{name}");
		let received = receive(&mut peer, Duration::from_secs(8), 60).await;
		eprintln!(
			"{name}: video {:?}, audio {:?}, VP8 keyframes {}",
			received.video, received.audio, received.vp8_keyframes
		);
		assert!(received.video.get(codec) >= 30, "{name} video frames");
		assert!(received.audio.get(Codec::Opus) >= 30, "{name} Opus frames");
		if codec == Codec::Vp8 {
			// A keyframe request (PLI) makes the browser's encoder send one.
			let before = received.vp8_keyframes;
			peer.request_keyframe();
			let after = receive(&mut peer, Duration::from_secs(3), 90).await;
			assert!(after.vp8_keyframes > 0, "no keyframe after PLI (had {before} before)");
		}
		tested.push(name);
	}
	assert!(tested.contains(&"VP8") && tested.contains(&"VP9"), "tested {tested:?}");
	let stats = browser.call(json!({ "op": "stats" })).await;
	assert!(aes_cm_128_sha1_80(&stats["transport"]["srtpCipher"]), "{stats:#}");
	browser.quit().await;
}

/// Chromium streams VP8, then renegotiates to VP9 on the same connection, as
/// the official client re-offers when it does not encode the codec our
/// answer chose: our viewer answers on the same peer and keeps receiving,
/// now VP9.
#[tokio::test(flavor = "multi_thread")]
async fn browser_renegotiates_with_rust() {
	if !enabled() {
		eprintln!("skipped: set VOELIN_INTEROP=1 (needs node, Playwright and Chromium)");
		return;
	}
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let mut browser = Browser::start().await;
	let offer = browser.call(json!({ "op": "offer", "codec": "VP8", "trickle": false })).await;
	let (mut peer, answer) =
		Peer::answer(&PeerConfig::loopback(), offer["sdp"].as_str().unwrap()).await.unwrap();
	browser.call(json!({ "op": "accept", "sdp": answer })).await;
	wait_connected(&mut peer).await;
	let before = receive(&mut peer, Duration::from_secs(5), 30).await;
	assert!(before.video.get(Codec::Vp8) >= 30, "before: {:?}", before.video);

	let offer = browser.call(json!({ "op": "reoffer", "codec": "VP9" })).await;
	let again = peer.renegotiate(offer["sdp"].as_str().unwrap()).await.unwrap();
	browser.call(json!({ "op": "accept", "sdp": again })).await;
	let after = receive(&mut peer, Duration::from_secs(8), 30).await;
	eprintln!("after the new offer: video {:?}, audio {:?}", after.video, after.audio);
	assert!(after.video.get(Codec::Vp9) >= 30, "after: {:?}", after.video);
	let stats = browser.call(json!({ "op": "stats" })).await;
	assert_eq!(stats["connectionState"], "connected", "{stats:#}");
	browser.quit().await;
}

/// Chromium with its host addresses behind mDNS names (`<uuid>.local`, as
/// without camera or microphone permission) and no STUN: the names are
/// its only candidates. Our candidates are kept from it, so it cannot
/// start the checks itself (and learn our address from them): the
/// connection comes up only if our peers resolve its names by a multicast
/// DNS query and reach it, as streamer (the names in the browser's answer)
/// and as viewer (in its offer). Then the media flows.
#[tokio::test(flavor = "multi_thread")]
async fn browser_mdns_candidates_connect() {
	if !enabled() {
		eprintln!("skipped: set VOELIN_INTEROP=1 (needs node, Playwright and Chromium)");
		return;
	}
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let mut browser = Browser::start_with_mdns().await;
	let only_mdns = |sdp: &str| {
		let candidates: Vec<&str> = sdp.lines().filter(|l| l.starts_with("a=candidate:")).collect();
		!candidates.is_empty() && candidates.iter().all(|l| l.contains(".local "))
	};
	let without_candidates = |sdp: &str| -> String {
		sdp.split_inclusive('\n').filter(|l| !l.starts_with("a=candidate:")).collect()
	};
	let config = PeerConfig::loopback();

	// Our streamer, the browser's answer with mDNS names.
	let (mut peer, offer) = Peer::offer(&config, "mdns").await.unwrap();
	let offer = without_candidates(&offer);
	let answer = browser.call(json!({ "op": "answer", "sdp": offer })).await;
	let answer = answer["sdp"].as_str().unwrap();
	assert!(only_mdns(answer), "the browser's answer:\n{answer}");
	peer.accept_answer(answer).await.unwrap();
	wait_connected(&mut peer).await;
	let mut source = SyntheticSource::new(30, 3000, false);
	let mut frames = Vec::new();
	let end = Instant::now() + Duration::from_secs(2);
	while Instant::now() < end {
		source.poll_frames(Instant::now(), &mut frames);
		for f in frames.drain(..) {
			peer.write(f.kind, f.time, f.data);
		}
		tokio::time::sleep(Duration::from_millis(10)).await;
	}
	let stats = browser.call(json!({ "op": "stats" })).await;
	assert!(stats["inbound"]["video"]["framesDecoded"].as_u64().unwrap() > 20, "{stats:#}");

	// The browser streams, its offer with mDNS names; our viewer answers.
	let offer = browser.call(json!({ "op": "offer", "codec": "VP8", "trickle": false })).await;
	let offer = offer["sdp"].as_str().unwrap();
	assert!(only_mdns(offer), "the browser's offer:\n{offer}");
	let (mut peer, answer) = Peer::answer(&config, offer).await.unwrap();
	browser.call(json!({ "op": "accept", "sdp": without_candidates(&answer) })).await;
	wait_connected(&mut peer).await;
	let received = receive(&mut peer, Duration::from_secs(5), 20).await;
	assert!(received.video.get(Codec::Vp8) >= 20, "{:?}", received.video);
	browser.quit().await;
}

/// Our streamer peer to Chromium through a relay that passes the handshake
/// and drops SRTP once AES-GCM was selected, as an official viewer whose
/// SRTP fails after the handshake: synthetic media for `seconds`; whether
/// the peer reported `NoFeedback`, the profile selected, Chromium's stats.
async fn stream_through_failing_srtp(
	browser: &mut Browser,
	srtp_profiles: Vec<SrtpProfile>,
	seconds: u64,
) -> (bool, Option<u16>, serde_json::Value) {
	let relay = Relay::new(Rule::FailSrtp(&[7, 8])).await;
	let config = PeerConfig {
		srtp_profiles,
		stall_timeout: Duration::from_secs(1),
		..PeerConfig::loopback()
	};
	let (mut peer, offer) = Peer::offer(&config, "interop").await.unwrap();
	let answer = browser.call(json!({ "op": "answer", "sdp": relay.offer(&offer) })).await;
	peer.accept_answer(&relay.answer(answer["sdp"].as_str().unwrap())).await.unwrap();
	wait_connected(&mut peer).await;
	let mut source = SyntheticSource::new(30, 3000, true);
	let (mut frames, mut no_feedback) = (Vec::new(), false);
	let end = Instant::now() + Duration::from_secs(seconds);
	while Instant::now() < end {
		source.poll_frames(Instant::now(), &mut frames);
		for f in frames.drain(..) {
			peer.write(f.kind, f.time, f.data);
		}
		while let Some(event) = peer.try_next_event() {
			no_feedback |= matches!(event, PeerEvent::NoFeedback);
		}
		tokio::time::sleep(Duration::from_millis(10)).await;
	}
	(no_feedback, relay.profile(), browser.call(json!({ "op": "stats" })).await)
}

/// The streamer's fallback against libwebrtc: with AES-GCM first, our peer
/// (the DTLS server) selects it; the relay lets no SRTP through, Chromium's
/// receiver reports never mention our video, and the peer reports
/// `NoFeedback` (the streamer session then offers again without the AEAD
/// profiles). The next connection, AES_CM_128_HMAC_SHA1_80 only, works:
/// no `NoFeedback`, Chromium decodes.
#[tokio::test(flavor = "multi_thread")]
async fn browser_srtp_failure_is_noticed() {
	use SrtpProfile::{AeadAes128Gcm, AeadAes256Gcm, Aes128CmSha1_80};
	if !enabled() {
		eprintln!("skipped: set VOELIN_INTEROP=1 (needs node, Playwright and Chromium)");
		return;
	}
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let mut browser = Browser::start().await;
	let aead_first = vec![AeadAes128Gcm, AeadAes256Gcm, Aes128CmSha1_80];
	let (no_feedback, profile, stats) =
		stream_through_failing_srtp(&mut browser, aead_first, 3).await;
	eprintln!("AES-GCM failing: NoFeedback {no_feedback}, profile {profile:?}, {stats:#}");
	assert_eq!(profile, Some(7), "AEAD_AES_128_GCM selected");
	assert!(no_feedback, "a viewer that gets nothing must be noticed");
	assert_eq!(stats["inbound"]["video"]["framesDecoded"].as_u64().unwrap_or(0), 0);

	let (no_feedback, profile, stats) =
		stream_through_failing_srtp(&mut browser, vec![Aes128CmSha1_80], 3).await;
	eprintln!("AES_CM: NoFeedback {no_feedback}, profile {profile:?}, {stats:#}");
	assert_eq!(profile, Some(1));
	assert!(!no_feedback, "a working connection must not be taken for a failing one");
	assert!(aes_cm_128_sha1_80(&stats["transport"]["srtpCipher"]), "{stats:#}");
	assert!(stats["inbound"]["video"]["framesDecoded"].as_u64().unwrap() > 30, "{stats:#}");
	browser.quit().await;
}
