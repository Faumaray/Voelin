//! Two peers on loopback: offer/answer, then video and audio frames flow;
//! the SRTP profile DTLS negotiates; bandwidth estimates at the streamer.

use std::time::Duration;

use tokio::time::timeout;
use voelin_stream::{MediaKind, MediaTime, Peer, PeerConfig, PeerEvent, SrtpProfile};

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
