//! RID simulcast over loopback: our stream session offers its layers with
//! RIDs (`PeerConfig::simulcast`), a str0m peer (as an SFU or WHIP server
//! would) takes them, and each layer's frames arrive with their RID.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tokio::time::timeout;
use tsclientlib::ClientId;
use voelin_stream::{
	FrameSource, LayerSpec, MediaKind, Output, Peer, PeerConfig, PeerEvent, Request, Signal,
	StreamEvent, StreamInfo, StreamKind, StreamNotification, StreamerEvent, StreamerOptions,
	Streams, SyntheticSource, ViewerState,
};

const OWN: ClientId = ClientId(10);
const VIEWER: ClientId = ClientId(20);

fn drain(s: &mut Streams) -> (Vec<Request>, Vec<StreamEvent>) {
	let (mut requests, mut events) = (Vec::new(), Vec::new());
	while let Some(o) = s.poll_output() {
		match o {
			Output::Request(r) => requests.push(r),
			Output::Event(e) => events.push(e),
		}
	}
	(requests, events)
}

#[tokio::test(flavor = "multi_thread")]
async fn rid_simulcast_over_loopback() {
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let layers = vec![
		LayerSpec { id: 0, rid: Some("h".into()), ..LayerSpec::single(1_500_000) },
		LayerSpec { id: 1, scale: 0.5, rid: Some("l".into()), ..LayerSpec::single(400_000) },
	];
	let config = PeerConfig { simulcast: true, ..PeerConfig::loopback() };
	let mut s = Streams::new(OWN, config);
	let options =
		StreamerOptions { auto_accept: true, layers: layers.clone(), ..Default::default() };
	s.start(options).unwrap();
	let info = StreamInfo {
		id: "s-1".into(),
		streamer: OWN,
		name: "simulcast".into(),
		kind: StreamKind::Screen,
		bitrate: 1900,
		viewer_limit: 0,
		audio: true,
		viewers: Some(0),
	};
	s.handle_notification(StreamNotification::Started { info, return_code: Some("1".into()) })
		.await;
	s.handle_notification(StreamNotification::JoinRequest {
		id: "s-1".into(),
		viewer: VIEWER,
		message: String::new(),
		remove: false,
	})
	.await;
	let (requests, _) = drain(&mut s);
	let offer = requests
		.iter()
		.find_map(|r| match r {
			Request::Respond { offer: Some(sdp), .. } => Some(sdp.clone()),
			_ => None,
		})
		.expect("offer");
	assert!(offer.contains("a=simulcast:send h;l"), "{offer}");

	// The other side takes both layers.
	let (mut viewer, answer) = Peer::answer(&PeerConfig::loopback(), &offer).await.unwrap();
	assert!(answer.contains("a=simulcast:recv h;l"), "{answer}");
	let json = Signal::Answer { sdp: answer }.to_json();
	s.handle_notification(StreamNotification::Signaling { id: "s-1".into(), peer: VIEWER, json })
		.await;
	timeout(Duration::from_secs(10), async {
		loop {
			let (_, events) = drain(&mut s);
			let connected = events.iter().any(|e| {
				matches!(e, StreamEvent::Streamer(StreamerEvent::Viewers(v))
					if v.iter().any(|v| v.state == ViewerState::Connected))
			});
			if connected {
				return;
			}
			s.wait_peers().await;
		}
	})
	.await
	.expect("viewer did not connect");
	let viewers = s.streamer().unwrap().viewers();
	assert_eq!(viewers[0].layer, None, "RID simulcast viewers get every layer");

	// Every layer's frames arrive with the layer's RID.
	let mut source = SyntheticSource::with_layers(30, 0, true, &layers);
	let mut frames = Vec::new();
	let mut by_rid: BTreeMap<String, Vec<u8>> = BTreeMap::new();
	let mut audio = 0;
	timeout(Duration::from_secs(10), async {
		while by_rid.len() < 2 || by_rid.values().any(|l| l.len() < 20) || audio < 20 {
			source.poll_frames(Instant::now(), &mut frames);
			for f in frames.drain(..) {
				s.write_frame(&f);
			}
			let _ = drain(&mut s);
			while let Some(event) = viewer.try_next_event() {
				match event {
					PeerEvent::LayerMedia { rid, frame } => {
						assert_eq!(frame.kind, MediaKind::Video);
						let layer = SyntheticSource::frame_layer(&frame.data).unwrap();
						by_rid.entry(rid.to_string()).or_default().push(layer);
					}
					PeerEvent::Media(frame) => {
						assert_eq!(frame.kind, MediaKind::Audio, "video without a RID");
						audio += 1;
					}
					_ => {}
				}
			}
			tokio::select! {
				() = s.wait_peers() => {}
				() = tokio::time::sleep(Duration::from_millis(10)) => {}
			}
		}
	})
	.await
	.unwrap_or_else(|_| panic!("frames per RID: {by_rid:?}, audio {audio}"));
	assert!(by_rid["h"].iter().all(|l| *l == 0), "{by_rid:?}");
	assert!(by_rid["l"].iter().all(|l| *l == 1), "{by_rid:?}");
}
