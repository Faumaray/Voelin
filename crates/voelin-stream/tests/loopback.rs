//! Two peers on loopback: offer/answer, then video and audio frames flow;
//! the SRTP profile DTLS negotiates; bandwidth estimates at the streamer;
//! losses repaired by retransmission; bursts of a high-bitrate stream.

use std::time::{Duration, Instant};

use tokio::time::timeout;
use voelin_stream::{MediaKind, MediaTime, Peer, PeerConfig, PeerEvent, SrtpProfile};

mod relay;
use relay::{Relay, Rule};

/// Bytes that look like a VP8 keyframe (frame tag + start code), then filler.
pub fn fake_vp8_keyframe(seq: u8, len: usize) -> Vec<u8> {
	let mut f = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x80, 0x02, 0xe0, 0x01];
	f.extend((0..len).map(|i| (i as u8) ^ seq));
	f
}

async fn wait_connected(peer: &mut Peer) {
	timeout(Duration::from_secs(10), async {
		loop {
			match peer.next_event().await {
				Some(PeerEvent::Connected) => return,
				Some(PeerEvent::Closed) | None => panic!("closed before connecting"),
				_ => {}
			}
		}
	})
	.await
	.expect("connect timeout");
}

#[tokio::test(flavor = "multi_thread")]
async fn media_flows_over_loopback() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let config = PeerConfig::loopback();
	let (mut streamer, offer) = Peer::offer(&config, "test-stream").await.unwrap();
	assert!(offer.contains("VP8"), "{offer}");
	let (mut viewer, answer) = Peer::answer(&config, &offer).await.unwrap();
	streamer.accept_answer(&answer).await.unwrap();
	assert!(streamer.accept_answer(&answer).await.is_err(), "second answer must fail");

	let (a, b) = tokio::join!(wait_connected(&mut streamer), wait_connected(&mut viewer));
	let ((), ()) = (a, b);

	// 1.5 s of 30 fps video (large frames need several packets) and 20 ms Opus frames.
	let sender = tokio::spawn(async move {
		for i in 0..45u64 {
			let frame = fake_vp8_keyframe(i as u8, 3000);
			streamer.write(MediaKind::Video, MediaTime::from_90khz(i * 3000), frame);
			for j in 0..2u64 {
				let n = i * 2 + j;
				streamer.write(
					MediaKind::Audio,
					MediaTime::new(n * 960, voelin_stream::Frequency::FORTY_EIGHT_KHZ),
					vec![0xfc, n as u8, 1, 2, 3],
				);
			}
			tokio::time::sleep(Duration::from_millis(33)).await;
		}
		streamer
	});

	let (mut video, mut audio) = (0, 0);
	let _ = timeout(Duration::from_secs(5), async {
		while let Some(event) = viewer.next_event().await {
			if let PeerEvent::Media(frame) = event {
				match frame.kind {
					MediaKind::Video => {
						assert_eq!(frame.codec, voelin_stream::Codec::Vp8);
						assert_eq!(frame.data.len(), 3010, "whole frame reassembled");
						video += 1;
					}
					MediaKind::Audio => {
						assert_eq!(frame.data[0], 0xfc);
						audio += 1;
					}
				}
				if video >= 40 && audio >= 80 {
					break;
				}
			}
		}
	})
	.await;
	let mut streamer = sender.await.unwrap();
	assert!(video >= 40 && audio >= 80, "received {video} video / {audio} audio frames");
	// The viewer's transport-cc feedback gives the streamer bandwidth estimates.
	let mut estimates = Vec::new();
	while let Some(event) = streamer.try_next_event() {
		if let PeerEvent::BitrateEstimate(bitrate) = event {
			estimates.push(bitrate);
		}
	}
	assert!(!estimates.is_empty(), "no bandwidth estimate");
	assert!(estimates.iter().all(|b| *b > 0), "{estimates:?}");
	drop(streamer);
}

/// Connect a streamer and a viewer; the SRTP profile each ended up with.
async fn negotiate(streamer: &PeerConfig, viewer: &PeerConfig) -> [Option<SrtpProfile>; 2] {
	let (mut s, offer) = Peer::offer(streamer, "srtp").await.unwrap();
	let (mut v, answer) = Peer::answer(viewer, &offer).await.unwrap();
	s.accept_answer(&answer).await.unwrap();
	tokio::join!(wait_connected(&mut s), wait_connected(&mut v));
	[s.srtp_profile(), v.srtp_profile()]
}

#[tokio::test(flavor = "multi_thread")]
async fn srtp_profile_order() {
	use SrtpProfile::{AeadAes128Gcm, AeadAes256Gcm, Aes128CmSha1_80};
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let default = PeerConfig::loopback();
	// By default: SRTP_AES128_CM_SHA1_80, as between official TeamSpeak clients.
	let profiles = negotiate(&default, &default).await;
	assert_eq!(profiles, [Some(Aes128CmSha1_80); 2]);
	assert_eq!(profiles[0].unwrap().name(), "AES_CM_128_HMAC_SHA1_80");
	// The answering peer is the DTLS server (a=setup:passive); its order wins
	// over a client that offers GCM first, as browsers do.
	let gcm_first = PeerConfig {
		srtp_profiles: vec![AeadAes256Gcm, AeadAes128Gcm, Aes128CmSha1_80],
		..PeerConfig::loopback()
	};
	assert_eq!(negotiate(&gcm_first, &default).await, [Some(Aes128CmSha1_80); 2]);
	assert_eq!(negotiate(&default, &gcm_first).await, [Some(AeadAes256Gcm); 2]);
	// A peer without AES_CM_128_HMAC_SHA1_80 still connects with GCM.
	let gcm_only = PeerConfig { srtp_profiles: vec![AeadAes128Gcm], ..PeerConfig::loopback() };
	assert_eq!(negotiate(&gcm_only, &default).await, [Some(AeadAes128Gcm); 2]);
}

/// The payload types of the first video section of `sdp` with `name`.
fn video_pts<'a>(sdp: &'a str, name: &str) -> Vec<&'a str> {
	let video = sdp.split("m=").find(|s| s.starts_with("video")).unwrap();
	video
		.lines()
		.filter_map(|l| l.strip_prefix("a=rtpmap:"))
		.filter(|l| l.contains(&format!(" {name}/")))
		.filter_map(|l| l.split(' ').next())
		.collect()
}

/// Our answer to a libwebrtc offer (headless Chromium 152, H.264 first, as
/// the official client streams) keeps what repairs losses and paces the
/// sender: NACK with retransmissions (RTX), PLI, transport-cc.
#[tokio::test]
async fn answers_keep_loss_repair_and_feedback() {
	let offer = include_str!("data/chromium-152-h264-offer.sdp");
	let (_viewer, answer) = Peer::answer(&PeerConfig::loopback(), offer).await.unwrap();
	let h264 = video_pts(&answer, "H264");
	assert!(!h264.is_empty(), "{answer}");
	for pt in h264 {
		for fb in ["nack", "nack pli", "transport-cc"] {
			let line = format!("a=rtcp-fb:{pt} {fb}\r\n");
			assert!(answer.contains(&line), "no {line:?} in\n{answer}");
		}
		assert!(answer.contains(&format!(" apt={pt}")), "no retransmissions for {pt}:\n{answer}");
	}
	assert!(answer.contains("transport-wide-cc"), "{answer}");
}

/// Video frames of `size` bytes that look like VP8 to the depacketizer
/// (keyframe tag on `keyframe`), the rest pseudo-random: nothing on the
/// path compresses or repeats.
fn noise_frame(seq: u64, size: usize, keyframe: bool) -> Vec<u8> {
	let mut state = seq.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
	let mut frame = fake_vp8_keyframe(seq as u8, 0);
	frame[0] |= u8::from(!keyframe);
	frame.extend((0..size).map(|_| {
		state ^= state << 13;
		state ^= state >> 7;
		state ^= state << 17;
		state as u8
	}));
	frame
}

/// Connect `streamer` and `viewer` configurations, through `relay` if given.
async fn connect(
	streamer: &PeerConfig,
	viewer: &PeerConfig,
	relay: Option<&Relay>,
) -> (Peer, Peer) {
	let (mut s, offer) = Peer::offer(streamer, "loss").await.unwrap();
	let offer = relay.map_or(offer.clone(), |r| r.offer(&offer));
	let (mut v, answer) = Peer::answer(viewer, &offer).await.unwrap();
	let answer = relay.map_or(answer.clone(), |r| r.answer(&answer));
	s.accept_answer(&answer).await.unwrap();
	tokio::join!(wait_connected(&mut s), wait_connected(&mut v));
	(s, v)
}

/// What the viewer got of a stream: frames, and those after a gap.
#[derive(Debug, Default)]
struct Received {
	frames: u64,
	gaps: u64,
}

/// Send `frames` video frames at `fps` (a keyframe of `keyframe_size`
/// every `keyframe_every`, else `size`); count what the viewer receives
/// until a second of silence.
async fn stream_noise(
	streamer: Peer,
	viewer: &mut Peer,
	(frames, fps, size): (u64, u64, usize),
	(keyframe_every, keyframe_size): (u64, usize),
) -> Received {
	let sender = tokio::spawn(async move {
		let start = Instant::now();
		for i in 0..frames {
			let keyframe = i % keyframe_every == 0;
			let data = noise_frame(i, if keyframe { keyframe_size } else { size }, keyframe);
			streamer.write(MediaKind::Video, MediaTime::from_90khz(i * 90_000 / fps), data);
			let next = start + Duration::from_micros((i + 1) * 1_000_000 / fps);
			tokio::time::sleep_until(next.into()).await;
		}
		streamer
	});
	let mut received = Received::default();
	while let Ok(Some(event)) = timeout(Duration::from_secs(1), viewer.next_event()).await {
		if let PeerEvent::Media(f) = event
			&& f.kind == MediaKind::Video
		{
			received.frames += 1;
			received.gaps += u64::from(!f.contiguous);
		}
	}
	drop(sender.await.unwrap());
	received
}

/// One media packet in 25 lost on the way to the viewer: the viewer asks
/// again (NACK) and the streamer retransmits (RTX) before the depacketizer
/// gives up, so every frame arrives whole and in order.
#[tokio::test(flavor = "multi_thread")]
async fn losses_are_repaired_by_retransmission() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let relay = Relay::new(Rule::LoseEvery(25)).await;
	let config = PeerConfig { bandwidth_estimation: false, ..PeerConfig::loopback() };
	let (streamer, mut viewer) = connect(&config, &config, Some(&relay)).await;
	// 3 s at 30 fps, 12 KB frames (about 10 packets each), keyframes of 60 KB.
	let received = stream_noise(streamer, &mut viewer, (90, 30, 12_000), (30, 60_000)).await;
	let (media, lost) = relay.media();
	eprintln!("{received:?}; relay: {lost} of {media} media packets lost");
	assert!(lost >= 30, "the relay lost too little: {lost} of {media}");
	assert_eq!(received.frames, 90, "{received:?}");
	assert_eq!(received.gaps, 0, "a loss reached the viewer: {received:?}");
}

/// UDP receive errors of this host so far (`RcvbufErrors` in
/// `/proc/net/snmp`: datagrams dropped because a socket's buffer was full).
fn receive_buffer_errors() -> Option<u64> {
	let snmp = std::fs::read_to_string("/proc/net/snmp").ok()?;
	let mut udp = snmp.lines().filter(|l| l.starts_with("Udp: "));
	let (names, values) = (udp.next()?, udp.next()?);
	let column = names.split_whitespace().position(|n| n == "RcvbufErrors")?;
	values.split_whitespace().nth(column)?.parse().ok()
}

/// A 9 Mbit/s stream at 60 fps with a 400 KB keyframe every second, sent
/// without pacing (the worst case of a 1440p stream), over loopback with
/// the system's socket buffers, with what Linux's default `rmem_max`
/// (212992) lets us ask for, and with ours: datagrams the kernel dropped
/// (host-wide counter), frames that arrived, and those after a gap. A
/// measurement, not a check: `-- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement (about 20 s)"]
async fn high_bitrate_bursts() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	for buffer in [0, 212_992, voelin_stream::peer::DEFAULT_UDP_BUFFER] {
		let config = PeerConfig {
			bandwidth_estimation: false,
			udp_buffer: buffer,
			..PeerConfig::loopback()
		};
		let (streamer, mut viewer) = connect(&config, &config, None).await;
		let before = receive_buffer_errors();
		let received = stream_noise(streamer, &mut viewer, (300, 60, 18_750), (60, 400_000)).await;
		let dropped = before.zip(receive_buffer_errors()).map(|(a, b)| b - a);
		eprintln!(
			"socket buffers asked: {buffer} bytes; datagrams dropped by the kernel: \
			 {dropped:?}; frames received: {} of 300, after a gap: {}",
			received.frames, received.gaps
		);
	}
}
