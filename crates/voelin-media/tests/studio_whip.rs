//! The studio's WHIP output against a WHIP server built on str0m here in the
//! test: the offer is posted, the answer applied, ICE and DTLS come up, real
//! VP8 frames arrive at the server, and stopping deletes the resource.
//!
//! The server is the smallest thing that is still WHIP: a TCP listener that
//! answers one `POST` with `201 Created`, a `Location` and the SDP answer, and
//! one `DELETE` with `200`.
#![cfg(feature = "whip")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::media::MediaKind;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};
use voelin_media::codec::{Codec, Codecs, EncoderConfig};
use voelin_media::frame::{FrameData, VideoFrame};
use voelin_media::studio::output::whip::Whip;
use voelin_media::studio::output::{OutputSink, Packet, Track};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;

/// What the test server saw.
#[derive(Default)]
struct Seen {
	video: AtomicU64,
	audio: AtomicU64,
	keyframes: AtomicU64,
	connected: AtomicBool,
	deleted: AtomicBool,
	resource: Mutex<Option<String>>,
}

/// Read one HTTP request: its start line and body.
fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
	let mut reader = BufReader::new(stream.try_clone().ok()?);
	let mut start = String::new();
	reader.read_line(&mut start).ok()?;
	let mut length = 0usize;
	loop {
		let mut line = String::new();
		if reader.read_line(&mut line).ok()? == 0 {
			break;
		}
		if line.trim().is_empty() {
			break;
		}
		if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
			length = value.trim().parse().unwrap_or(0);
		}
	}
	let mut body = vec![0u8; length];
	if length > 0 {
		reader.read_exact(&mut body).ok()?;
	}
	Some((start.trim().to_owned(), String::from_utf8_lossy(&body).into_owned()))
}

/// A WHIP server on a loopback port; returns its URL and what it saw.
fn spawn_server() -> (String, Arc<Seen>) {
	let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
	let port = listener.local_addr().unwrap().port();
	let url = format!("http://127.0.0.1:{port}/whip");
	let seen = Arc::new(Seen::default());
	let shared = seen.clone();
	std::thread::Builder::new()
		.name("whip-test-server".into())
		.spawn(move || {
			for stream in listener.incoming().take(2) {
				let Ok(mut stream) = stream else { continue };
				let Some((start, body)) = read_request(&mut stream) else { continue };
				if start.starts_with("DELETE") {
					shared.deleted.store(true, Ordering::Relaxed);
					let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
					continue;
				}
				assert!(start.starts_with("POST"), "{start}");
				let answer = match accept(&body, &shared) {
					Ok(answer) => answer,
					Err(e) => {
						let _ = stream.write_all(
							format!(
								"HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{e}",
								e.len()
							)
							.as_bytes(),
						);
						continue;
					}
				};
				*shared.resource.lock().unwrap() = Some(format!("/whip/{port}"));
				let response = format!(
					"HTTP/1.1 201 Created\r\nContent-Type: application/sdp\r\n\
					 Location: /whip/{port}\r\nContent-Length: {}\r\n\r\n{answer}",
					answer.len()
				);
				let _ = stream.write_all(response.as_bytes());
				let _ = stream.flush();
			}
		})
		.expect("the server thread");
	(url, seen)
}

/// Accept an offer, start the session's thread and return the SDP answer.
fn accept(offer: &str, seen: &Arc<Seen>) -> Result<String, String> {
	let offer = SdpOffer::from_sdp_string(offer).map_err(|e| e.to_string())?;
	let mut rtc =
		RtcConfig::new().clear_codecs().enable_opus(true).enable_vp8(true).build(Instant::now());
	let socket = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
	let local = socket.local_addr().map_err(|e| e.to_string())?;
	rtc.add_local_candidate(Candidate::host(local, Protocol::Udp).map_err(|e| e.to_string())?);
	let answer = rtc.sdp_api().accept_offer(offer).map_err(|e| e.to_string())?;
	let seen = seen.clone();
	std::thread::Builder::new()
		.name("whip-test-session".into())
		.spawn(move || run_session(rtc, socket, local, seen))
		.map_err(|e| e.to_string())?;
	Ok(answer.to_sdp_string())
}

fn run_session(mut rtc: Rtc, socket: UdpSocket, local: SocketAddr, seen: Arc<Seen>) {
	let mut buf = vec![0u8; 2000];
	let deadline = Instant::now() + Duration::from_secs(30);
	while rtc.is_alive() && Instant::now() < deadline {
		if rtc.handle_input(Input::Timeout(Instant::now())).is_err() {
			return;
		}
		let timeout = loop {
			match rtc.poll_output() {
				Ok(Output::Timeout(t)) => break t,
				Ok(Output::Transmit(t)) => {
					let _ = socket.send_to(&t.contents, t.destination);
				}
				Ok(Output::Event(Event::IceConnectionStateChange(state))) => {
					if matches!(
						state,
						str0m::IceConnectionState::Connected | str0m::IceConnectionState::Completed
					) {
						seen.connected.store(true, Ordering::Relaxed);
					}
				}
				Ok(Output::Event(Event::MediaData(data))) => {
					if data.params.spec().codec.kind() == MediaKind::Video {
						seen.video.fetch_add(1, Ordering::Relaxed);
						seen.keyframes.fetch_add(
							u64::from(data.contiguous && is_vp8_key(&data.data)),
							Ordering::Relaxed,
						);
					} else {
						seen.audio.fetch_add(1, Ordering::Relaxed);
					}
				}
				Ok(Output::Event(_)) => {}
				Err(_) => return,
			}
		};
		let wait = timeout
			.saturating_duration_since(Instant::now())
			.clamp(Duration::from_millis(1), Duration::from_millis(20));
		if socket.set_read_timeout(Some(wait)).is_err() {
			return;
		}
		match socket.recv_from(&mut buf) {
			Ok((n, source)) => {
				let Ok(contents) = buf[..n].try_into() else { continue };
				let input = Input::Receive(
					Instant::now(),
					Receive { proto: Protocol::Udp, source, destination: local, contents },
				);
				if rtc.accepts(&input) {
					let _ = rtc.handle_input(input);
				}
			}
			Err(_) => continue,
		}
	}
}

/// VP8's keyframe bit (the inverse of `P` in the payload header).
fn is_vp8_key(data: &[u8]) -> bool {
	data.first().is_some_and(|b| b & 1 == 0)
}

fn frame(n: u64) -> VideoFrame {
	let mut frame = VideoFrame::black_i420(WIDTH, HEIGHT);
	let FrameData::I420 { y, .. } = &mut frame.data else { unreachable!() };
	let x0 = (n * 3 % u64::from(WIDTH - 20)) as usize;
	for row in 20..60 {
		y.data[row * y.stride + x0..][..20].fill(220);
	}
	frame.timestamp = Duration::from_micros(n * 1_000_000 / 30);
	frame
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_whip_session_carries_the_studios_packets() {
	let codecs = Codecs::new();
	let Ok(mut encoder) = codecs.new_encoder(
		Codec::Vp8,
		EncoderConfig { fps: 30, bitrate_bps: 400_000, ..EncoderConfig::default() },
	) else {
		eprintln!("skipped: no VP8 encoder in this build");
		return;
	};
	let (url, seen) = spawn_server();
	let mut whip = Whip::start(&url, Some("test-token"), Codec::Vp8, 0, true)
		.await
		.expect("the WHIP session started");
	assert!(whip.resource().is_some_and(|r| r.contains("/whip/")), "{:?}", whip.resource());
	// Until ICE is up there is nobody to send a keyframe to.
	assert!(whip.needs_keyframe());
	assert!(whip.wants(Track::Video { codec: Codec::Vp8, layer: 0 }));
	assert!(!whip.wants(Track::Video { codec: Codec::Vp9, layer: 0 }));
	assert!(!whip.wants(Track::Video { codec: Codec::Vp8, layer: 1 }));

	// Push frames until the server has seen enough, or give up.
	let started = Instant::now();
	let mut n = 0u64;
	while started.elapsed() < Duration::from_secs(20)
		&& (seen.video.load(Ordering::Relaxed) < 20 || seen.audio.load(Ordering::Relaxed) < 10)
	{
		let picture = frame(n);
		encoder
			.encode_with(&picture, n == 0, &mut |chunk| {
				whip.write(&Packet {
					track: Track::Video { codec: Codec::Vp8, layer: 0 },
					pts_90khz: chunk.pts_90khz,
					keyframe: chunk.keyframe,
					width: WIDTH,
					height: HEIGHT,
					data: chunk.data,
				})
				.expect("the session took the packet");
			})
			.expect("encoded");
		if n.is_multiple_of(2) {
			// Opus silence: a valid frame the server can count.
			whip.write(&Packet {
				track: Track::Audio { channels: 2 },
				pts_90khz: n * 90_000 / 30,
				keyframe: true,
				width: 0,
				height: 0,
				data: &[0xF8, 0xFF, 0xFE],
			})
			.expect("the session took the audio packet");
		}
		n += 1;
		tokio::time::sleep(Duration::from_millis(33)).await;
	}

	let stats = whip.stats();
	assert!(seen.connected.load(Ordering::Relaxed), "ICE never came up: {stats:?}");
	assert!(stats.connected, "the output does not think it is connected: {stats:?}");
	assert!(whip.bytes() > 0, "{stats:?}");
	assert_eq!(stats.error, None);
	let (video, audio) = (seen.video.load(Ordering::Relaxed), seen.audio.load(Ordering::Relaxed));
	assert!(video >= 20, "only {video} video frames reached the server ({stats:?})");
	assert!(audio >= 10, "only {audio} audio frames reached the server ({stats:?})");
	assert!(seen.keyframes.load(Ordering::Relaxed) >= 1, "no keyframe arrived");

	// Stopping deletes the resource the service handed out.
	whip.finish().unwrap();
	let waited = Instant::now();
	while !seen.deleted.load(Ordering::Relaxed) && waited.elapsed() < Duration::from_secs(5) {
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	assert!(seen.deleted.load(Ordering::Relaxed), "the resource was not deleted");
}

#[tokio::test]
async fn a_service_that_refuses_says_why() {
	let (url, _seen) = spawn_server();
	// An offer the server cannot parse: it answers 400 with a reason.
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let port = listener.local_addr().unwrap().port();
	std::thread::spawn(move || {
		for stream in listener.incoming().take(1) {
			let Ok(mut stream) = stream else { continue };
			let _ = read_request(&mut stream);
			let body = "no capacity";
			let _ = stream.write_all(
				format!(
					"HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\n\r\n{body}",
					body.len()
				)
				.as_bytes(),
			);
		}
	});
	let Err(e) =
		Whip::start(&format!("http://127.0.0.1:{port}/whip"), None, Codec::Vp8, 0, true).await
	else {
		panic!("a service that refuses 503 still started a session");
	};
	let e = e.to_string();
	assert!(e.contains("503") && e.contains("no capacity"), "{e}");
	let _ = url;
}
