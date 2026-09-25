//! One WebRTC peer connection (str0m) driven by a tokio task.
//!
//! TeamSpeak 6 streams are peer to peer: the streamer creates one connection per
//! viewer and sends its offer through the server; the viewer answers. Host
//! candidates go into the SDP; server-reflexive candidates found through STUN
//! are reported later as [`PeerEvent::LocalCandidate`] for trickling.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, KeyframeRequestKind, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig, RtcError};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, trace, warn};

use crate::stun;

/// Video codecs a peer can negotiate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
	Vp8,
	Vp9,
	H264,
	Av1,
}

impl VideoCodec {
	pub const ALL: [Self; 4] = [Self::Vp8, Self::Vp9, Self::H264, Self::Av1];

	pub fn from_codec(c: Codec) -> Option<Self> {
		match c {
			Codec::Vp8 => Some(Self::Vp8),
			Codec::Vp9 => Some(Self::Vp9),
			Codec::H264 => Some(Self::H264),
			Codec::Av1 => Some(Self::Av1),
			_ => None,
		}
	}

	/// From an SDP encoding name (`a=rtpmap:<pt> VP8/90000`).
	pub fn from_sdp_name(name: &str) -> Option<Self> {
		Self::ALL.into_iter().find(|c| c.sdp_name().eq_ignore_ascii_case(name))
	}

	pub fn sdp_name(self) -> &'static str {
		match self {
			Self::Vp8 => "VP8",
			Self::Vp9 => "VP9",
			Self::H264 => "H264",
			Self::Av1 => "AV1",
		}
	}
}

#[derive(Clone, Debug)]
pub struct PeerConfig {
	/// Local addresses for host candidates. Empty: the primary IPv4 address.
	pub hosts: Vec<IpAddr>,
	/// STUN servers (`host:port`) for a server-reflexive candidate.
	pub stun_servers: Vec<String>,
	/// Video codecs a streamer offers, in order of preference. Frames are sent
	/// with the codec the viewer picks, so offer only what the encoder produces.
	pub video_codecs: Vec<VideoCodec>,
	/// Video codecs a viewer accepts (what it can decode). The answer keeps
	/// the order of the streamer's offer, so the streamer's preference wins.
	pub accept_video_codecs: Vec<VideoCodec>,
	pub video: bool,
	pub audio: bool,
}

impl Default for PeerConfig {
	fn default() -> Self {
		Self {
			hosts: Vec::new(),
			stun_servers: stun::TEAMSPEAK_STUN.iter().map(|s| (*s).to_owned()).collect(),
			video_codecs: vec![VideoCodec::Vp8],
			accept_video_codecs: VideoCodec::ALL.to_vec(),
			video: true,
			audio: true,
		}
	}
}

impl PeerConfig {
	/// Loopback only, no STUN: for tests on one machine.
	pub fn loopback() -> Self {
		Self {
			hosts: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
			stun_servers: Vec::new(),
			..Self::default()
		}
	}
}

#[derive(Debug, thiserror::Error)]
pub enum PeerError {
	#[error("network: {0}")]
	Io(#[from] std::io::Error),
	#[error("invalid SDP: {0}")]
	Sdp(String),
	#[error("WebRTC: {0}")]
	Rtc(#[from] RtcError),
	#[error("no usable local address")]
	NoAddress,
	#[error("no answer is expected")]
	NotOffering,
	#[error("peer connection closed")]
	Closed,
}

/// Something that happened on the connection.
#[derive(Clone, Debug)]
pub enum PeerEvent {
	/// ICE and DTLS are up; media can flow.
	Connected,
	/// A candidate found after the SDP was sent (server reflexive), as an SDP
	/// `candidate:` line for [`crate::Signal::IceCandidate`], with the mid of
	/// the first (bundled) media line.
	LocalCandidate { candidate: String, mid: Option<String> },
	/// A received frame (depacketized).
	Media(MediaFrame),
	/// The viewer asks for a keyframe.
	KeyframeRequest,
	/// The connection is gone.
	Closed,
}

#[derive(Clone, Debug)]
pub struct MediaFrame {
	pub kind: MediaKind,
	pub codec: Codec,
	/// RTP time (90 kHz for video, 48 kHz for Opus).
	pub time: MediaTime,
	pub network_time: Instant,
	/// `false` if frames were lost before this one.
	pub contiguous: bool,
	pub data: Arc<[u8]>,
}

enum Cmd {
	Answer(String, oneshot::Sender<Result<(), PeerError>>),
	RemoteCandidate(String),
	Write { kind: MediaKind, time: MediaTime, data: Arc<[u8]> },
	RequestKeyframe,
	Close,
}

/// Handle to a peer connection task. Dropping it closes the connection.
pub struct Peer {
	cmd: mpsc::UnboundedSender<Cmd>,
	events: mpsc::UnboundedReceiver<PeerEvent>,
}

impl Peer {
	/// Streamer side: create a connection that sends our media; returns the SDP
	/// offer for `respondjoinstreamrequest`.
	pub async fn offer(config: &PeerConfig, stream_id: &str) -> Result<(Self, String), PeerError> {
		let mut rtc = build_rtc(config, &config.video_codecs);
		let net = Net::bind(config, &mut rtc).await?;
		let mut api = rtc.sdp_api();
		let msid = Some(stream_id.to_owned());
		let mut mids = Vec::new();
		if config.video {
			let mid =
				api.add_media(MediaKind::Video, Direction::SendOnly, msid.clone(), None, None);
			mids.push((mid, MediaKind::Video));
		}
		if config.audio {
			let mid = api.add_media(MediaKind::Audio, Direction::SendOnly, msid, None, None);
			mids.push((mid, MediaKind::Audio));
		}
		let (offer, pending) = api.apply().ok_or(PeerError::Sdp("nothing to offer".into()))?;
		let sdp = offer.to_sdp_string();
		Ok((Self::spawn(rtc, net, config, Some(pending), mids), sdp))
	}

	/// Viewer side: accept the streamer's offer; returns our SDP answer.
	pub async fn answer(config: &PeerConfig, offer: &str) -> Result<(Self, String), PeerError> {
		// Our codecs in the order of the offer, then the rest.
		let mut codecs: Vec<_> = offered_video_codecs(offer)
			.into_iter()
			.filter(|c| config.accept_video_codecs.contains(c))
			.collect();
		for c in &config.accept_video_codecs {
			if !codecs.contains(c) {
				codecs.push(*c);
			}
		}
		let offer = SdpOffer::from_sdp_string(offer).map_err(|e| PeerError::Sdp(e.to_string()))?;
		let mut rtc = build_rtc(config, &codecs);
		let net = Net::bind(config, &mut rtc).await?;
		let answer = rtc.sdp_api().accept_offer(offer)?;
		Ok((Self::spawn(rtc, net, config, None, Vec::new()), answer.to_sdp_string()))
	}

	fn spawn(
		rtc: Rtc,
		net: Net,
		config: &PeerConfig,
		pending: Option<SdpPendingOffer>,
		mids: Vec<(Mid, MediaKind)>,
	) -> Self {
		let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
		let (event_tx, event_rx) = mpsc::unbounded_channel();
		let task = Task {
			rtc,
			net,
			pending,
			cmd: cmd_rx,
			events: event_tx,
			mids,
			writers: HashMap::new(),
			stun: Vec::new(),
		};
		tokio::spawn(task.run(config.stun_servers.clone()));
		Self { cmd: cmd_tx, events: event_rx }
	}

	/// Streamer side: apply the viewer's answer.
	pub async fn accept_answer(&self, sdp: &str) -> Result<(), PeerError> {
		let (tx, rx) = oneshot::channel();
		self.cmd.send(Cmd::Answer(sdp.to_owned(), tx)).map_err(|_| PeerError::Closed)?;
		rx.await.map_err(|_| PeerError::Closed)?
	}

	/// Add a trickled remote candidate (`candidate:...`, `a=` prefix optional).
	pub fn add_remote_candidate(&self, candidate: &str) {
		let _ = self.cmd.send(Cmd::RemoteCandidate(candidate.to_owned()));
	}

	/// Send one encoded frame (streamer). Dropped until connected.
	pub fn write(&self, kind: MediaKind, time: MediaTime, data: impl Into<Arc<[u8]>>) {
		let _ = self.cmd.send(Cmd::Write { kind, time, data: data.into() });
	}

	/// Ask the streamer for a keyframe (viewer).
	pub fn request_keyframe(&self) {
		let _ = self.cmd.send(Cmd::RequestKeyframe);
	}

	pub fn close(&self) {
		let _ = self.cmd.send(Cmd::Close);
	}

	/// The next event; `None` once the task ended.
	pub async fn next_event(&mut self) -> Option<PeerEvent> {
		self.events.recv().await
	}

	pub fn try_next_event(&mut self) -> Option<PeerEvent> {
		self.events.try_recv().ok()
	}

	/// Poll for the next event, for driving many peers from one task.
	pub fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<PeerEvent>> {
		self.events.poll_recv(cx)
	}
}

impl Drop for Peer {
	fn drop(&mut self) {
		self.close();
	}
}

/// H.264 Constrained High, level 3.1: reportedly the only H.264 profile the
/// TeamSpeak client decodes; str0m's defaults lack it.
const H264_CONSTRAINED_HIGH: u32 = 0x640c1f;

/// An RTC with Opus and `video` codecs, in this order of preference.
fn build_rtc(config: &PeerConfig, video: &[VideoCodec]) -> Rtc {
	let mut rtc_config = RtcConfig::new()
		.set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
		.clear_codecs()
		.enable_opus(config.audio);
	for codec in video {
		rtc_config = match codec {
			VideoCodec::Vp8 => rtc_config.enable_vp8(true),
			VideoCodec::Vp9 => rtc_config.enable_vp9(true),
			VideoCodec::H264 => {
				let mut c = rtc_config.enable_h264(true);
				c.codec_config().add_h264(
					112.into(),
					Some(113.into()),
					true,
					H264_CONSTRAINED_HIGH,
				);
				c
			}
			VideoCodec::Av1 => rtc_config.enable_av1(true),
		};
	}
	rtc_config.build(Instant::now())
}

/// The video codecs of the first video media line of `sdp`, in offered order.
fn offered_video_codecs(sdp: &str) -> Vec<VideoCodec> {
	let mut pts: Vec<&str> = Vec::new();
	let mut names: Vec<(&str, VideoCodec)> = Vec::new();
	let mut in_video = false;
	for line in sdp.lines().map(str::trim) {
		if let Some(media) = line.strip_prefix("m=") {
			in_video = pts.is_empty() && media.starts_with("video ");
			if in_video {
				pts = media.split_whitespace().skip(3).collect();
			}
		} else if in_video
			&& let Some((pt, encoding)) =
				line.strip_prefix("a=rtpmap:").and_then(|r| r.split_once(' '))
			&& let Some(codec) = encoding.split('/').next().and_then(VideoCodec::from_sdp_name)
		{
			names.push((pt, codec));
		}
	}
	let mut codecs = Vec::new();
	for pt in pts {
		if let Some((_, codec)) = names.iter().find(|(p, _)| *p == pt)
			&& !codecs.contains(codec)
		{
			codecs.push(*codec);
		}
	}
	codecs
}

/// The primary local IPv4 address (the one used for the default route).
fn primary_ipv4() -> Option<IpAddr> {
	let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
	// Connecting a UDP socket sends nothing; it only picks the route.
	socket.connect("192.0.2.1:9").ok()?;
	let ip = socket.local_addr().ok()?.ip();
	(!ip.is_unspecified()).then_some(ip)
}

struct Packet {
	data: Vec<u8>,
	source: SocketAddr,
	destination: SocketAddr,
}

/// One UDP socket per local address; readers forward into one channel.
struct Net {
	sockets: Vec<Arc<UdpSocket>>,
	incoming: mpsc::Receiver<Packet>,
}

impl Net {
	async fn bind(config: &PeerConfig, rtc: &mut Rtc) -> Result<Self, PeerError> {
		let hosts = if config.hosts.is_empty() {
			primary_ipv4().into_iter().collect()
		} else {
			config.hosts.clone()
		};
		let (tx, incoming) = mpsc::channel(256);
		let mut sockets = Vec::new();
		for ip in hosts {
			let socket = Arc::new(UdpSocket::bind(SocketAddr::new(ip, 0)).await?);
			let local = socket.local_addr()?;
			match Candidate::host(local, "udp") {
				Ok(c) => {
					rtc.add_local_candidate(c);
				}
				Err(e) => {
					warn!(%local, "skipping host candidate: {e}");
					continue;
				}
			}
			let reader = socket.clone();
			let tx = tx.clone();
			tokio::spawn(async move {
				let mut buf = vec![0; 2000];
				loop {
					let (n, source) = match reader.recv_from(&mut buf).await {
						Ok(r) => r,
						// ICMP port unreachable surfaces as an error on some systems.
						Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
						Err(e) => {
							debug!("UDP read failed: {e}");
							break;
						}
					};
					let packet = Packet { data: buf[..n].to_vec(), source, destination: local };
					if tx.send(packet).await.is_err() {
						break;
					}
				}
			});
			sockets.push(socket);
		}
		if sockets.is_empty() {
			return Err(PeerError::NoAddress);
		}
		Ok(Self { sockets, incoming })
	}

	fn socket_for(&self, local: SocketAddr) -> Option<&Arc<UdpSocket>> {
		self.sockets.iter().find(|s| s.local_addr().is_ok_and(|a| a == local))
	}
}

struct PendingStun {
	id: stun::TransactionId,
	server: SocketAddr,
	base: SocketAddr,
}

struct Task {
	rtc: Rtc,
	net: Net,
	pending: Option<SdpPendingOffer>,
	cmd: mpsc::UnboundedReceiver<Cmd>,
	events: mpsc::UnboundedSender<PeerEvent>,
	/// Media lines of the session.
	mids: Vec<(Mid, MediaKind)>,
	/// Negotiated (mid, pt) for sending each media kind.
	writers: HashMap<MediaKind, (Mid, Pt)>,
	stun: Vec<PendingStun>,
}

fn transaction_id() -> stun::TransactionId {
	let state = RandomState::new();
	let a = state.hash_one(Instant::now()).to_le_bytes();
	let b = state.hash_one(std::process::id()).to_le_bytes();
	let mut id = [0; 12];
	id[..8].copy_from_slice(&a);
	id[8..].copy_from_slice(&b[..4]);
	id
}

impl Task {
	async fn run(mut self, stun_servers: Vec<String>) {
		let stun_servers = Self::resolve_stun(stun_servers);
		if let Err(e) = self.run_loop(stun_servers).await {
			debug!("peer connection ended: {e}");
		}
		self.rtc.disconnect();
		let _ = self.events.send(PeerEvent::Closed);
	}

	/// Resolve the STUN servers in the background; each first IPv4 address
	/// arrives on the returned channel.
	fn resolve_stun(servers: Vec<String>) -> mpsc::UnboundedReceiver<SocketAddr> {
		let (tx, rx) = mpsc::unbounded_channel();
		for server in servers {
			let tx = tx.clone();
			tokio::spawn(async move {
				let lookup = tokio::net::lookup_host(server.as_str());
				match tokio::time::timeout(Duration::from_secs(3), lookup).await {
					Ok(Ok(mut addrs)) => {
						if let Some(addr) = addrs.find(SocketAddr::is_ipv4) {
							let _ = tx.send(addr);
						}
					}
					_ => debug!(server, "STUN server lookup failed"),
				}
			});
		}
		rx
	}

	/// Send a binding request to `server` from every non-loopback socket.
	async fn send_stun(&mut self, server: SocketAddr) {
		for socket in &self.net.sockets {
			let Ok(base) = socket.local_addr() else { continue };
			if base.ip().is_loopback() {
				continue;
			}
			let id = transaction_id();
			if socket.send_to(&stun::binding_request(&id), server).await.is_ok() {
				self.stun.push(PendingStun { id, server, base });
			}
		}
	}

	/// Returns `true` if the packet was a STUN answer for us.
	fn handle_stun(&mut self, packet: &Packet) -> bool {
		let Some(i) = self.stun.iter().position(|s| {
			s.server == packet.source
				&& s.base == packet.destination
				&& stun::parse_binding_response(&packet.data, &s.id).is_some()
		}) else {
			return false;
		};
		let pending = self.stun.remove(i);
		let Some(mapped) = stun::parse_binding_response(&packet.data, &pending.id) else {
			return true;
		};
		// Several servers may report the same mapping.
		self.stun.retain(|s| s.base != pending.base);
		if mapped == pending.base {
			return true;
		}
		match Candidate::server_reflexive(mapped, pending.base, "udp") {
			Ok(c) => {
				let candidate = c.to_sdp_string();
				let mid = self.mids.first().map(|(mid, _)| mid.to_string());
				if self.rtc.add_local_candidate(c).is_some() {
					let _ = self.events.send(PeerEvent::LocalCandidate { candidate, mid });
				}
			}
			Err(e) => debug!("bad reflexive candidate {mapped}: {e}"),
		}
		true
	}

	async fn run_loop(
		&mut self,
		mut stun_servers: mpsc::UnboundedReceiver<SocketAddr>,
	) -> Result<(), PeerError> {
		loop {
			self.rtc.handle_input(Input::Timeout(Instant::now()))?;
			let deadline = loop {
				if !self.rtc.is_alive() {
					return Ok(());
				}
				match self.rtc.poll_output()? {
					Output::Timeout(t) => break t,
					Output::Transmit(t) => {
						if let Some(socket) = self.net.socket_for(t.source)
							&& let Err(e) = socket.send_to(&t.contents, t.destination).await
						{
							trace!("UDP send to {} failed: {e}", t.destination);
						}
					}
					Output::Event(e) => {
						if !self.handle_event(e) {
							return Ok(());
						}
					}
				}
			};
			let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
			tokio::select! {
				packet = self.net.incoming.recv() => {
					let Some(packet) = packet else { return Ok(()) };
					if self.handle_stun(&packet) {
						continue;
					}
					let Ok(contents) = packet.data.as_slice().try_into() else { continue };
					let input = Input::Receive(Instant::now(), Receive {
						proto: Protocol::Udp,
						source: packet.source,
						destination: packet.destination,
						contents,
					});
					if self.rtc.accepts(&input) {
						self.rtc.handle_input(input)?;
					}
				}
				cmd = self.cmd.recv() => {
					let Some(cmd) = cmd else { return Ok(()) };
					if !self.handle_cmd(cmd)? {
						return Ok(());
					}
				}
				Some(server) = stun_servers.recv() => self.send_stun(server).await,
				() = sleep => {}
			}
		}
	}

	/// Returns `false` when the connection ended.
	fn handle_event(&mut self, event: Event) -> bool {
		match event {
			Event::Connected => {
				let _ = self.events.send(PeerEvent::Connected);
			}
			Event::IceConnectionStateChange(IceConnectionState::Disconnected) => return false,
			Event::MediaAdded(m) => {
				if !self.mids.iter().any(|(mid, _)| *mid == m.mid) {
					self.mids.push((m.mid, m.kind));
				}
			}
			Event::MediaData(d) => {
				let kind = if d.params.spec().codec.is_audio() {
					MediaKind::Audio
				} else {
					MediaKind::Video
				};
				let _ = self.events.send(PeerEvent::Media(MediaFrame {
					kind,
					codec: d.params.spec().codec,
					time: d.time,
					network_time: d.network_time,
					contiguous: d.contiguous,
					data: d.data,
				}));
			}
			Event::KeyframeRequest(_) => {
				let _ = self.events.send(PeerEvent::KeyframeRequest);
			}
			_ => {}
		}
		true
	}

	/// Returns `false` to close.
	fn handle_cmd(&mut self, cmd: Cmd) -> Result<bool, PeerError> {
		match cmd {
			Cmd::Answer(sdp, reply) => {
				let result = self.accept_answer(&sdp);
				let _ = reply.send(result);
			}
			Cmd::RemoteCandidate(line) => {
				let line = line.trim().trim_start_matches("a=");
				match Candidate::from_sdp_string(line) {
					Ok(c) => self.rtc.add_remote_candidate(c),
					Err(e) => debug!("ignoring remote candidate {line:?}: {e}"),
				}
			}
			Cmd::Write { kind, time, data } => self.write(kind, time, data)?,
			Cmd::RequestKeyframe => {
				for &(mid, _) in &self.mids {
					if let Some(mut w) = self.rtc.writer(mid) {
						let _ = w.request_keyframe(None, KeyframeRequestKind::Pli);
					}
				}
			}
			Cmd::Close => return Ok(false),
		}
		Ok(true)
	}

	fn accept_answer(&mut self, sdp: &str) -> Result<(), PeerError> {
		let pending = self.pending.take().ok_or(PeerError::NotOffering)?;
		let answer = SdpAnswer::from_sdp_string(sdp).map_err(|e| PeerError::Sdp(e.to_string()))?;
		self.rtc.sdp_api().accept_answer(pending, answer)?;
		Ok(())
	}

	fn write(
		&mut self,
		kind: MediaKind,
		time: MediaTime,
		data: Arc<[u8]>,
	) -> Result<(), PeerError> {
		if !self.writers.contains_key(&kind) {
			let Some((mid, pt)) = self.find_writer(kind) else {
				trace!(?kind, "no negotiated media to write to");
				return Ok(());
			};
			self.writers.insert(kind, (mid, pt));
		}
		let (mid, pt) = self.writers[&kind];
		if let Some(writer) = self.rtc.writer(mid) {
			writer.write(pt, Instant::now(), time, data)?;
		}
		Ok(())
	}

	fn find_writer(&mut self, kind: MediaKind) -> Option<(Mid, Pt)> {
		let mid = self.mids.iter().find(|(_, k)| *k == kind)?.0;
		let writer = self.rtc.writer(mid)?;
		// The first remote payload type in the answer is the peer's preference.
		let pt = writer.payload_params().find(|p| p.spec().codec.kind() == kind)?.pt();
		Some((mid, pt))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn offered_codec_order() {
		let sdp = "v=0\r\n\
			m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
			a=rtpmap:111 opus/48000/2\r\n\
			m=video 9 UDP/TLS/RTP/SAVPF 98 99 96 97 45 102\r\n\
			a=rtpmap:96 VP8/90000\r\n\
			a=rtpmap:97 rtx/90000\r\n\
			a=rtpmap:98 VP9/90000\r\n\
			a=rtpmap:99 rtx/90000\r\n\
			a=rtpmap:45 AV1/90000\r\n\
			a=rtpmap:102 H264/90000\r\n\
			m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
			a=rtpmap:96 H264/90000\r\n";
		assert_eq!(
			offered_video_codecs(sdp),
			[VideoCodec::Vp9, VideoCodec::Vp8, VideoCodec::Av1, VideoCodec::H264]
		);
		assert!(offered_video_codecs("v=0\r\n").is_empty());
	}

	#[tokio::test]
	async fn answer_follows_offer_order() {
		let streamer = PeerConfig {
			video_codecs: vec![VideoCodec::H264, VideoCodec::Vp8],
			..PeerConfig::loopback()
		};
		let (_peer, offer) = Peer::offer(&streamer, "s").await.unwrap();
		assert!(offer.contains("profile-level-id=640c1f"), "{offer}");
		let (_peer, answer) = Peer::answer(&PeerConfig::loopback(), &offer).await.unwrap();
		assert_eq!(offered_video_codecs(&answer)[0], VideoCodec::H264, "{answer}");
		let vp8_only =
			PeerConfig { accept_video_codecs: vec![VideoCodec::Vp8], ..PeerConfig::loopback() };
		let (_peer, answer) = Peer::answer(&vp8_only, &offer).await.unwrap();
		assert_eq!(offered_video_codecs(&answer), [VideoCodec::Vp8], "{answer}");
	}
}
