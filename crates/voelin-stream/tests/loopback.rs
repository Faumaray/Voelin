//! Two peers on loopback: offer/answer, then video and audio frames flow.

use std::time::Duration;

use tokio::time::timeout;
use voelin_stream::{MediaKind, MediaTime, Peer, PeerConfig, PeerEvent};

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
	let streamer = sender.await.unwrap();
	assert!(video >= 40 && audio >= 80, "received {video} video / {audio} audio frames");
	drop(streamer);
}
