//! A full stream between two clients through the TeamSpeak 6 dev server
//! (dev/docker-compose.yml): `setupstream`, `joinstreamrequest`,
//! `respondjoinstreamrequest` with our offer, `streamsignaling` with the answer,
//! then media over loopback. Runs only with `TSC_LIVE=1`.

use std::time::Duration;

use futures::prelude::*;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tsc_stream::{
	Frequency, LeaveReason, MediaKind, MediaTime, Peer, PeerConfig, PeerEvent, Signal,
	StreamNotification, StreamSetup, proto,
};
use tsclientlib::prelude::*;
use tsclientlib::{ClientId, Connection, DisconnectOptions, MessageHandle, StreamItem};
use tsproto_packets::packets::OutCommand;

fn live() -> bool {
	std::env::var("TSC_LIVE").is_ok_and(|v| v == "1")
}

fn ts6_addr() -> String {
	std::env::var("TSC_TS6_ADDR").unwrap_or_else(|_| "127.0.0.1:9988".into())
}

type Reply = oneshot::Sender<Result<(), String>>;

/// A connected client running in its own task.
struct Client {
	id: ClientId,
	commands: mpsc::UnboundedSender<Option<(OutCommand, Option<Reply>)>>,
	notifications: mpsc::UnboundedReceiver<StreamNotification>,
}

impl Client {
	async fn connect(nick: &str) -> Client {
		let mut con = Connection::build(ts6_addr()).name(nick.to_owned()).connect().unwrap();
		timeout(Duration::from_secs(30), async {
			con.events()
				.try_filter(|e| future::ready(matches!(e, StreamItem::BookEvents(_))))
				.next()
				.await
				.unwrap()
				.unwrap();
		})
		.await
		.expect("connect timeout");
		let id = con.get_state().unwrap().own_client;
		let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Option<(OutCommand, Option<Reply>)>>();
		let mut waiting: Vec<(MessageHandle, Reply)> = Vec::new();
		let (note_tx, note_rx) = mpsc::unbounded_channel();
		tokio::spawn(async move {
			loop {
				let mut events = con.events();
				tokio::select! {
					item = events.next() => match item {
						Some(Ok(StreamItem::MessageEvent(msg))) => {
							for n in StreamNotification::from_message(&msg) {
								let _ = note_tx.send(n);
							}
						}
						Some(Ok(StreamItem::MessageResult(handle, result))) => {
							if let Some(i) = waiting.iter().position(|(h, _)| *h == handle) {
								let _ = waiting.remove(i).1.send(result.map_err(|e| e.to_string()));
							}
						}
						Some(Ok(_)) => {}
						_ => return,
					},
					cmd = cmd_rx.recv() => {
						drop(events);
						match cmd {
							Some(Some((cmd, None))) => cmd.send(&mut con).unwrap(),
							Some(Some((cmd, Some(reply)))) => {
								waiting.push((cmd.send_with_result(&mut con).unwrap(), reply));
							}
							_ => break,
						}
					}
				}
			}
			con.disconnect(DisconnectOptions::new()).unwrap();
			let _ =
				timeout(Duration::from_secs(3), con.events().for_each(|_| future::ready(()))).await;
		});
		Client { id, commands: cmd_tx, notifications: note_rx }
	}

	fn send(&self, cmd: OutCommand) {
		self.commands.send(Some((cmd, None))).unwrap();
	}

	/// Send and wait for the server's answer.
	async fn call(&self, cmd: OutCommand) -> Result<(), String> {
		let (tx, rx) = oneshot::channel();
		self.commands.send(Some((cmd, Some(tx)))).unwrap();
		timeout(Duration::from_secs(10), rx).await.expect("no answer").unwrap()
	}

	async fn next(&mut self, what: &str) -> StreamNotification {
		timeout(Duration::from_secs(15), self.notifications.recv())
			.await
			.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
			.expect("client task ended")
	}

	async fn quit(self) {
		let _ = self.commands.send(None);
		tokio::time::sleep(Duration::from_millis(300)).await;
	}
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_through_ts6_server() {
	if !live() {
		eprintln!("skipped: set TSC_LIVE=1 with the dev servers running");
		return;
	}
	let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
	let tag = std::process::id() % 10_000;
	let mut streamer = Client::connect(&format!("streamer-{tag}")).await;
	let mut viewer = Client::connect(&format!("viewer-{tag}")).await;

	// 1. Start the stream.
	streamer.send(proto::setup(&StreamSetup { name: "live test".into(), ..Default::default() }));
	let stream_id = loop {
		if let StreamNotification::Started { info, return_code: Some(_) } =
			streamer.next("own notifystreamstarted").await
		{
			assert_eq!(info.streamer, streamer.id);
			break info.id;
		}
	};
	// The viewer, in the same channel, sees it.
	loop {
		if let StreamNotification::Started { info, .. } = viewer.next("notifystreamstarted").await
			&& info.id == stream_id
		{
			assert_eq!(info.name, "live test");
			break;
		}
	}

	// 2. Viewer asks to join.
	viewer.send(proto::join_request(&stream_id, streamer.id, "let me watch", false));
	let viewer_id = loop {
		if let StreamNotification::JoinRequest { id, viewer: v, message, remove } =
			streamer.next("notifyjoinstreamrequest").await
		{
			assert_eq!(
				(id.as_str(), message.as_str(), remove),
				(stream_id.as_str(), "let me watch", false)
			);
			break v;
		}
	};
	assert_eq!(viewer_id, viewer.id);

	// 3. Streamer accepts with an offer.
	let config = PeerConfig::loopback();
	let (mut send_peer, offer) = Peer::offer(&config, &stream_id).await.unwrap();
	streamer.send(proto::respond(&stream_id, viewer_id, Some(&offer), true));

	// 4. Viewer answers through streamsignaling.
	let offer_in = loop {
		match viewer.next("notifyrespondjoinstreamrequest").await {
			StreamNotification::JoinResponse { accepted, offer: Some(offer), .. } => {
				assert!(accepted);
				break offer;
			}
			StreamNotification::JoinResponse { accepted, offer: None, message, .. } => {
				panic!("response without offer (accepted {accepted}, {message:?})")
			}
			_ => {}
		}
	};
	assert_eq!(offer_in.trim(), offer.trim(), "offer must arrive unchanged");
	let (mut recv_peer, answer) = Peer::answer(&config, &offer_in).await.unwrap();
	viewer.send(proto::signaling(
		&stream_id,
		streamer.id,
		&Signal::Answer { sdp: answer }.to_json(),
	));

	// 5. Streamer applies the answer.
	loop {
		if let StreamNotification::Signaling { peer, json, .. } =
			streamer.next("notifystreamsignaling").await
		{
			assert_eq!(peer, viewer_id);
			match Signal::parse(&json).unwrap() {
				Signal::Answer { sdp } => {
					send_peer.accept_answer(&sdp).await.unwrap();
					break;
				}
				other => panic!("unexpected signal {other:?}"),
			}
		}
	}

	// 6. Media.
	timeout(Duration::from_secs(10), async {
		while !matches!(send_peer.next_event().await, Some(PeerEvent::Connected)) {}
	})
	.await
	.expect("streamer peer did not connect");
	let sender = tokio::spawn(async move {
		for i in 0..60u64 {
			let mut frame = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x80, 0x02, 0xe0, 0x01];
			frame.extend(std::iter::repeat_n(i as u8, 1500));
			send_peer.write(MediaKind::Video, MediaTime::from_90khz(i * 3000), frame);
			send_peer.write(
				MediaKind::Audio,
				MediaTime::new(i * 960, Frequency::FORTY_EIGHT_KHZ),
				vec![0xfc, i as u8],
			);
			tokio::time::sleep(Duration::from_millis(33)).await;
		}
		send_peer
	});
	let (mut video, mut audio) = (0, 0);
	let _ = timeout(Duration::from_secs(6), async {
		while let Some(event) = recv_peer.next_event().await {
			match event {
				PeerEvent::Media(f) if f.kind == MediaKind::Video => video += 1,
				PeerEvent::Media(_) => audio += 1,
				_ => {}
			}
			if video >= 50 && audio >= 50 {
				break;
			}
		}
	})
	.await;
	let send_peer = sender.await.unwrap();
	assert!(video >= 50 && audio >= 50, "viewer got {video} video / {audio} audio frames");

	drop((send_peer, recv_peer));
	// 7. The streamer ends the viewer's session, then the stream.
	streamer
		.call(proto::remove_viewer(&stream_id, viewer_id, LeaveReason::Kicked))
		.await
		.expect("removeclientfromstream");
	loop {
		if let StreamNotification::ViewerLeft { viewer: v, reason, .. } =
			viewer.next("notifystreamclientleft").await
		{
			assert_eq!((v, reason), (viewer_id, Some(LeaveReason::Kicked)));
			break;
		}
	}
	streamer.call(proto::stop(&stream_id)).await.expect("stopstream");
	loop {
		if let StreamNotification::Stopped { id, .. } = viewer.next("notifystreamstopped").await {
			assert_eq!(id, stream_id);
			break;
		}
	}
	viewer.quit().await;
	streamer.quit().await;
}
