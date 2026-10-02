//! WHIP: pushing the studio to a broadcast service over WebRTC.
//!
//! WHIP (WebRTC-HTTP ingestion, RFC 9725) is one HTTP request: `POST` the SDP
//! offer, get the answer and a `Location` back, and `DELETE` that location to
//! stop. Everything after that is an ordinary WebRTC session, which `str0m`
//! drives on a thread of its own; the studio's encoded packets go in through
//! a channel, so an output that is slow or gone never holds up an encoder.
//!
//! The session offers exactly one video codec — the one the studio already
//! encodes — and Opus, both send-only, so the service never asks for a codec
//! that would mean a second encode.
//!
//! Candidates: one host candidate on the interface that reaches the service.
//! That is enough for a service with a public address, which is what WHIP is
//! for, and for a server on this machine (the tests). There is no STUN or
//! TURN here yet, so a studio behind a NAT that the service cannot reach back
//! through will not connect.

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use str0m::change::SdpAnswer;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};
use tracing::{debug, trace, warn};

use crate::codec::Codec;
use crate::studio::output::{OutputSink, Packet, Track};
use crate::{Error, Result};

/// Packets waiting for the session thread. A full queue drops the packet
/// (a stalled upload must not stall an encoder) and asks for a keyframe, so
/// the service's decoder recovers once the upload does.
const QUEUE: usize = 256;

fn error(message: impl Into<String>) -> Error {
	Error::Capture { backend: "whip", message: message.into() }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One packet on its way to the session thread.
struct Queued {
	video: bool,
	pts_90khz: u64,
	data: Vec<u8>,
}

/// What a WHIP output is doing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WhipStats {
	pub connected: bool,
	pub packets: u64,
	pub bytes: u64,
	/// Packets dropped because the session could not keep up.
	pub dropped: u64,
	pub error: Option<String>,
}

/// A WHIP session; stops and deletes its resource when dropped.
pub struct Whip {
	name: String,
	/// The `Location` the service gave us, to `DELETE` on stop.
	resource: Option<String>,
	token: Option<String>,
	layer: u32,
	codec: Codec,
	queue: Option<SyncSender<Queued>>,
	thread: Option<JoinHandle<()>>,
	stop: Arc<AtomicBool>,
	shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
	connected: AtomicBool,
	/// A keyframe is wanted: at the start, when the session comes up, when
	/// the service asks (PLI/FIR), and after a packet was dropped. Taken by
	/// [`OutputSink::needs_keyframe`].
	keyframe: AtomicBool,
	packets: AtomicU64,
	bytes: AtomicU64,
	dropped: AtomicU64,
	error: Mutex<Option<String>>,
}

impl Whip {
	/// Whether `url` is one this output takes.
	pub fn handles(url: &str) -> bool {
		url.starts_with("http://") || url.starts_with("https://")
	}

	/// Offer `codec` (the layer the studio encodes) and Opus to the WHIP
	/// endpoint at `url`, with `token` as the bearer token if the service
	/// wants one.
	///
	/// Must run on a Tokio runtime: the HTTP request does.
	pub async fn start(
		url: &str,
		token: Option<&str>,
		codec: Codec,
		layer: u32,
		audio: bool,
	) -> Result<Self> {
		let endpoint = reqwest::Url::parse(url).map_err(|e| error(format!("bad WHIP url: {e}")))?;
		let host = endpoint
			.socket_addrs(|| match endpoint.scheme() {
				"https" => Some(443),
				_ => Some(80),
			})
			.map_err(|e| error(format!("cannot resolve {url}: {e}")))?
			.into_iter()
			.next()
			.ok_or_else(|| error(format!("{url} resolves to nothing")))?;

		let (socket, local) = bind_towards(host)?;
		let mut rtc = build_rtc(codec, audio);
		let candidate = Candidate::host(local, Protocol::Udp)
			.map_err(|e| error(format!("cannot offer {local}: {e}")))?;
		rtc.add_local_candidate(candidate);
		let mut api = rtc.sdp_api();
		let msid = Some("voelin-studio".to_owned());
		let video = api.add_media(MediaKind::Video, Direction::SendOnly, msid.clone(), None, None);
		let audio_mid =
			audio.then(|| api.add_media(MediaKind::Audio, Direction::SendOnly, msid, None, None));
		let (offer, pending) = api.apply().ok_or_else(|| error("nothing to offer"))?;

		let mut request = reqwest::Client::new()
			.post(endpoint.clone())
			.header("content-type", "application/sdp")
			.body(offer.to_sdp_string());
		if let Some(token) = token {
			request = request.bearer_auth(token);
		}
		let response =
			request.send().await.map_err(|e| error(format!("the WHIP request failed: {e}")))?;
		let status = response.status();
		let resource = response
			.headers()
			.get(reqwest::header::LOCATION)
			.and_then(|l| l.to_str().ok())
			.and_then(|l| endpoint.join(l).ok())
			.map(|u| u.to_string());
		let body = response.text().await.unwrap_or_default();
		if !status.is_success() {
			return Err(error(format!("the service answered {status}: {}", body.trim())));
		}
		let answer = SdpAnswer::from_sdp_string(&body)
			.map_err(|e| error(format!("the service's answer is not SDP: {e}")))?;
		rtc.sdp_api()
			.accept_answer(pending, answer)
			.map_err(|e| error(format!("the service's answer was refused: {e}")))?;
		debug!(url, resource = resource.as_deref().unwrap_or("-"), %codec, "WHIP session");

		let (queue, packets) = sync_channel(QUEUE);
		let stop = Arc::new(AtomicBool::new(false));
		// The first frames should stand alone too.
		let shared = Arc::new(Shared { keyframe: AtomicBool::new(true), ..Shared::default() });
		let session = Session {
			rtc,
			socket,
			packets,
			stop: stop.clone(),
			shared: shared.clone(),
			video,
			audio: audio_mid,
			writers: [None, None],
		};
		let thread = std::thread::Builder::new()
			.name("voelin-whip".into())
			.spawn(move || session.run())
			.map_err(|e| error(e.to_string()))?;
		Ok(Self {
			name: url.to_owned(),
			resource,
			token: token.map(str::to_owned),
			layer,
			codec,
			queue: Some(queue),
			thread: Some(thread),
			stop,
			shared,
		})
	}

	/// The resource the service gave us, which stops the stream when deleted.
	pub fn resource(&self) -> Option<&str> {
		self.resource.as_deref()
	}

	pub fn stats(&self) -> WhipStats {
		WhipStats {
			connected: self.shared.connected.load(Ordering::Relaxed),
			packets: self.shared.packets.load(Ordering::Relaxed),
			bytes: self.shared.bytes.load(Ordering::Relaxed),
			dropped: self.shared.dropped.load(Ordering::Relaxed),
			error: lock(&self.shared.error).clone(),
		}
	}

	/// Stop the session and tell the service, if there is a runtime to do it
	/// on (the request needs one).
	fn teardown(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
		self.queue = None;
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
		let Some(resource) = self.resource.take() else { return };
		let token = self.token.clone();
		let logged = resource.clone();
		let delete = async move {
			let mut request = reqwest::Client::new().delete(&resource);
			if let Some(token) = token {
				request = request.bearer_auth(token);
			}
			match request.send().await {
				Ok(response) => debug!(%resource, status = %response.status(), "WHIP deleted"),
				Err(e) => debug!(%resource, "cannot delete the WHIP resource: {e}"),
			}
		};
		match tokio::runtime::Handle::try_current() {
			Ok(handle) => {
				let _ = logged;
				handle.spawn(delete);
			}
			// No runtime: the service times the session out instead.
			Err(_) => debug!(resource = %logged, "no runtime to delete the WHIP resource on"),
		}
	}
}

impl Drop for Whip {
	fn drop(&mut self) {
		self.teardown();
	}
}

impl OutputSink for Whip {
	fn name(&self) -> &str {
		&self.name
	}

	fn wants(&self, track: Track) -> bool {
		match track {
			Track::Video { codec, layer } => layer == self.layer && codec == self.codec,
			Track::Audio { .. } => true,
		}
	}

	fn needs_keyframe(&mut self) -> bool {
		// Once per request, not on every frame until one arrives.
		self.shared.keyframe.swap(false, Ordering::Relaxed)
	}

	fn write(&mut self, packet: &Packet<'_>) -> Result<()> {
		let Some(queue) = &self.queue else { return Ok(()) };
		if !self.wants(packet.track) {
			return Ok(());
		}
		let queued = Queued {
			video: packet.track.is_video(),
			pts_90khz: packet.pts_90khz,
			data: packet.data.to_vec(),
		};
		match queue.try_send(queued) {
			Ok(()) => Ok(()),
			Err(TrySendError::Full(_)) => {
				self.shared.dropped.fetch_add(1, Ordering::Relaxed);
				self.shared.keyframe.store(true, Ordering::Relaxed);
				Ok(())
			}
			Err(TrySendError::Disconnected(_)) => Err(error(
				lock(&self.shared.error).clone().unwrap_or_else(|| "session ended".into()),
			)),
		}
	}

	fn finish(&mut self) -> Result<()> {
		self.teardown();
		Ok(())
	}

	fn bytes(&self) -> u64 {
		self.shared.bytes.load(Ordering::Relaxed)
	}
}

/// An RTC that offers exactly `codec` and, if `audio`, Opus.
fn build_rtc(codec: Codec, audio: bool) -> Rtc {
	let config = RtcConfig::new().clear_codecs().enable_opus(audio);
	let config = match codec {
		Codec::Vp8 => config.enable_vp8(true),
		Codec::Vp9 => config.enable_vp9(true),
		Codec::Av1 => config.enable_av1(true),
		Codec::H264 => config.enable_h264(true),
		Codec::H265 => config.enable_h265(true),
	};
	config.build(Instant::now())
}

/// A UDP socket on the interface that reaches `peer`, and its address.
///
/// Connecting a socket makes the kernel pick the route, so its local address
/// is the one the peer can send back to; the socket that is kept is then
/// bound to that address but left unconnected, so it takes the service's
/// candidates as well.
fn bind_towards(peer: SocketAddr) -> Result<(UdpSocket, SocketAddr)> {
	let any = if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
	let probe = UdpSocket::bind(any)?;
	probe.connect(peer)?;
	let local = probe.local_addr()?;
	drop(probe);
	let socket = UdpSocket::bind(SocketAddr::new(local.ip(), 0))?;
	let local = socket.local_addr()?;
	Ok((socket, local))
}

/// The session's thread: str0m's I/O and the studio's packets.
struct Session {
	rtc: Rtc,
	socket: UdpSocket,
	packets: Receiver<Queued>,
	stop: Arc<AtomicBool>,
	shared: Arc<Shared>,
	video: Mid,
	audio: Option<Mid>,
	/// The payload type for video and for audio, once negotiated.
	writers: [Option<(Mid, Pt)>; 2],
}

impl Session {
	fn run(mut self) {
		let mut buf = vec![0u8; 2000];
		let local = match self.socket.local_addr() {
			Ok(addr) => addr,
			Err(e) => {
				self.fail(format!("the WHIP socket has no address: {e}"));
				return;
			}
		};
		while !self.stop.load(Ordering::Relaxed) {
			if let Err(e) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
				self.fail(format!("WHIP session failed: {e}"));
				return;
			}
			let deadline = loop {
				if !self.rtc.is_alive() {
					self.fail("the WHIP session closed");
					return;
				}
				match self.rtc.poll_output() {
					Ok(Output::Timeout(t)) => break t,
					Ok(Output::Transmit(t)) => {
						if let Err(e) = self.socket.send_to(&t.contents, t.destination) {
							trace!("WHIP send to {} failed: {e}", t.destination);
						}
					}
					Ok(Output::Event(Event::IceConnectionStateChange(state))) => {
						let up = state == str0m::IceConnectionState::Connected
							|| state == str0m::IceConnectionState::Completed;
						if up && !self.shared.connected.swap(up, Ordering::Relaxed) {
							self.shared.keyframe.store(true, Ordering::Relaxed);
						}
						self.shared.connected.store(up, Ordering::Relaxed);
						debug!(?state, "WHIP ICE");
					}
					Ok(Output::Event(Event::KeyframeRequest(_))) => {
						self.shared.keyframe.store(true, Ordering::Relaxed);
					}
					Ok(Output::Event(_)) => {}
					Err(e) => {
						self.fail(format!("WHIP session failed: {e}"));
						return;
					}
				}
			};
			// Feed what the studio queued, then wait for the socket until
			// str0m's deadline (never longer than a tick, so the queue is
			// drained promptly).
			while let Ok(queued) = self.packets.try_recv() {
				self.send(queued);
			}
			let now = Instant::now();
			let wait = deadline.saturating_duration_since(now).min(Duration::from_millis(5));
			if self.socket.set_read_timeout(Some(wait.max(Duration::from_millis(1)))).is_err() {
				return;
			}
			match self.socket.recv_from(&mut buf) {
				Ok((n, source)) => {
					let Ok(contents) = buf[..n].try_into() else { continue };
					let input = Input::Receive(
						Instant::now(),
						Receive { proto: Protocol::Udp, source, destination: local, contents },
					);
					if self.rtc.accepts(&input)
						&& let Err(e) = self.rtc.handle_input(input)
					{
						self.fail(format!("WHIP session failed: {e}"));
						return;
					}
				}
				Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
				Err(e) => trace!("WHIP receive failed: {e}"),
			}
		}
	}

	/// One studio packet into the session.
	fn send(&mut self, queued: Queued) {
		let slot = usize::from(!queued.video);
		let kind = if queued.video { MediaKind::Video } else { MediaKind::Audio };
		if self.writers[slot].is_none() {
			let Some(mid) = (if queued.video { Some(self.video) } else { self.audio }) else {
				return;
			};
			let Some(writer) = self.rtc.writer(mid) else { return };
			let Some(pt) =
				writer.payload_params().find(|p| p.spec().codec.kind() == kind).map(|p| p.pt())
			else {
				return;
			};
			self.writers[slot] = Some((mid, pt));
		}
		let Some((mid, pt)) = self.writers[slot] else { return };
		// Video keeps the 90 kHz clock; Opus counts samples at 48 kHz.
		let time = if queued.video {
			MediaTime::from_90khz(queued.pts_90khz)
		} else {
			MediaTime::new(queued.pts_90khz * 48_000 / 90_000, Frequency::FORTY_EIGHT_KHZ)
		};
		let bytes = queued.data.len() as u64;
		let Some(writer) = self.rtc.writer(mid) else { return };
		match writer.write(pt, Instant::now(), time, queued.data) {
			Ok(()) => {
				self.shared.packets.fetch_add(1, Ordering::Relaxed);
				self.shared.bytes.fetch_add(bytes, Ordering::Relaxed);
			}
			Err(e) => trace!(?kind, "WHIP packet not sent: {e}"),
		}
	}

	fn fail(&self, message: impl Into<String>) {
		let message = message.into();
		warn!("{message}");
		*lock(&self.shared.error) = Some(message);
		self.shared.connected.store(false, Ordering::Relaxed);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn urls_and_codecs() {
		assert!(Whip::handles("https://live.example/whip"));
		assert!(Whip::handles("http://127.0.0.1:8080/whip"));
		assert!(!Whip::handles("rtmp://live.example/app"));
		// Every codec the studio can encode has an str0m codec to offer.
		for codec in [Codec::Vp8, Codec::Vp9, Codec::Av1, Codec::H264, Codec::H265] {
			assert!(build_rtc(codec, true).is_alive(), "{codec}");
		}
	}

	#[test]
	fn a_socket_binds_towards_the_peer() {
		let (socket, local) = bind_towards("127.0.0.1:9".parse().unwrap()).unwrap();
		assert!(local.ip().is_loopback(), "{local}");
		assert_ne!(local.port(), 0);
		assert_eq!(socket.local_addr().unwrap(), local);
	}

	#[tokio::test]
	async fn an_endpoint_that_is_not_there_fails() {
		// Port 1 on loopback: nothing listens.
		let Err(e) = Whip::start("http://127.0.0.1:1/whip", Some("t"), Codec::Vp8, 0, true).await
		else {
			panic!("a session started against nothing");
		};
		assert!(e.to_string().contains("WHIP request failed"), "{e}");
		assert!(Whip::start("not a url", None, Codec::Vp8, 0, true).await.is_err());
	}
}
