//! One WebRTC peer connection (str0m) driven by a tokio task.
//!
//! TeamSpeak 6 streams are peer to peer: the streamer creates one connection per
//! viewer and sends its offer through the server; the viewer answers. Host
//! candidates go into the SDP; server-reflexive candidates found through STUN
//! are reported later as [`PeerEvent::LocalCandidate`] for trickling.
//!
//! DTLS negotiates the SRTP profile in the order of
//! [`PeerConfig::srtp_profiles`] (see [`crate::dtls`]). A streamer's peer
//! estimates the bandwidth to its viewer and paces its packets
//! ([`PeerConfig::bandwidth_estimation`], [`PeerEvent::BitrateEstimate`]),
//! and can offer RID simulcast ([`PeerConfig::simulcast`]).

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{
	Direction, KeyframeRequestKind, MediaKind, MediaTime, Mid, Pt, Rid, Rids, Simulcast,
	SimulcastLayer,
};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig, RtcError};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, trace, warn};

use crate::dtls::{self, NegotiatedProfile, SrtpProfile};
use crate::h264::{self, H264Profile};
use crate::layer::LayerSpec;
use crate::{mdns, stun};

/// Initial bandwidth estimate of a streamer's peer without
/// [`OfferOptions::start_bitrate`].
pub const DEFAULT_START_BITRATE: u64 = 1_000_000;

/// Socket buffers a peer asks for by default ([`PeerConfig::udp_buffer`]).
pub const DEFAULT_UDP_BUFFER: usize = 4 << 20;

/// How long an mDNS candidate's name is asked for.
const MDNS_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a connected viewer waits for video by default
/// ([`PeerConfig::stall_timeout`]): a streamer sends a keyframe as soon as
/// a viewer connects.
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Video codecs a peer can negotiate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
	Vp8,
	Vp9,
	H264,
	Av1,
	/// HEVC: offered last, for peers that take nothing else.
	H265,
}

impl VideoCodec {
	pub const ALL: [Self; 5] = [Self::Vp8, Self::Vp9, Self::H264, Self::Av1, Self::H265];

	pub fn from_codec(c: Codec) -> Option<Self> {
		match c {
			Codec::Vp8 => Some(Self::Vp8),
			Codec::Vp9 => Some(Self::Vp9),
			Codec::H264 => Some(Self::H264),
			Codec::Av1 => Some(Self::Av1),
			Codec::H265 => Some(Self::H265),
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
			Self::H265 => "H265",
		}
	}

	/// Bit of this codec in a set of codecs ([`crate::LayerFeedback`]).
	pub fn bit(self) -> u8 {
		1 << Self::ALL.iter().position(|c| *c == self).unwrap_or(0)
	}
}

impl std::fmt::Display for VideoCodec {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.sdp_name())
	}
}

/// A video codec as an answer chose it, with the H.264 profile: the
/// streamer encodes each format with an encoder of its own and sends a
/// viewer only frames of its format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoFormat {
	Vp8,
	Vp9,
	H264(H264Profile),
	Av1,
	H265,
}

impl VideoFormat {
	pub const ALL: [Self; 6] = [
		Self::Vp8,
		Self::Vp9,
		Self::H264(H264Profile::ConstrainedHigh),
		Self::H264(H264Profile::ConstrainedBaseline),
		Self::Av1,
		Self::H265,
	];

	pub fn codec(self) -> VideoCodec {
		match self {
			Self::Vp8 => VideoCodec::Vp8,
			Self::Vp9 => VideoCodec::Vp9,
			Self::H264(_) => VideoCodec::H264,
			Self::Av1 => VideoCodec::Av1,
			Self::H265 => VideoCodec::H265,
		}
	}

	/// Bit of this format in a set of formats ([`crate::LayerFeedback`]):
	/// the codec's, and one more for H.264 Constrained Baseline.
	pub fn bit(self) -> u8 {
		match self {
			Self::H264(H264Profile::ConstrainedBaseline) => 1 << VideoCodec::ALL.len(),
			other => other.codec().bit(),
		}
	}
}

/// H.264 in Constrained High, what our encoders make unless told otherwise.
impl From<VideoCodec> for VideoFormat {
	fn from(codec: VideoCodec) -> Self {
		match codec {
			VideoCodec::Vp8 => Self::Vp8,
			VideoCodec::Vp9 => Self::Vp9,
			VideoCodec::H264 => Self::H264(H264Profile::ConstrainedHigh),
			VideoCodec::Av1 => Self::Av1,
			VideoCodec::H265 => Self::H265,
		}
	}
}

impl std::fmt::Display for VideoFormat {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::H264(H264Profile::ConstrainedHigh) => f.write_str("H264 Constrained High"),
			Self::H264(H264Profile::ConstrainedBaseline) => {
				f.write_str("H264 Constrained Baseline")
			}
			other => other.codec().fmt(f),
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
	/// SRTP protection profiles in order of preference. As DTLS server (our
	/// role with official TeamSpeak clients and browsers, both as streamer
	/// and as viewer) we pick the first of them the other side offers; as
	/// DTLS client we offer them in this order. Empty: the default,
	/// [`SrtpProfile::DEFAULT_ORDER`]. Applies to connections created after
	/// a change.
	pub srtp_profiles: Vec<SrtpProfile>,
	/// A streamer's peers estimate the bandwidth to their viewer (transport-cc
	/// feedback), report it ([`PeerEvent::BitrateEstimate`]) and pace their
	/// packets to it.
	pub bandwidth_estimation: bool,
	/// A streamer offers RID simulcast (`a=simulcast`, one `a=rid` per layer
	/// with a [`LayerSpec::rid`]) when it has at least two such layers. Only
	/// for peers that support it (SFUs, WHIP servers): an answer without
	/// simulcast leaves the peer unable to send video. Official TeamSpeak
	/// viewers get a plain offer and one layer at a time.
	pub simulcast: bool,
	/// H.264 `profile-level-id`s of the offer, best first: Constrained High,
	/// then Constrained Baseline for peers that take only that (headless
	/// Chromium and other libwebrtc builds without a High decoder), each at
	/// the level the stream needs ([`set_h264_format`](Self::set_h264_format));
	/// level 3.1 by default. A viewer gets frames of the profile its answer
	/// chose ([`PeerEvent::VideoCodec`]). At most two are offered.
	pub h264_profile_level_ids: Vec<u32>,
	/// Receive and send buffer (bytes) each peer socket asks the system for
	/// (`SO_RCVBUF`, `SO_SNDBUF`; Linux caps them at `net.core.rmem_max` and
	/// `wmem_max`); 0 keeps the system's default. Linux's default, 208 KB,
	/// holds about 90 full-size packets: a 1440p keyframe arriving while
	/// the peer task is busy overflows it, and every lost packet costs the
	/// viewer a retransmission or a keyframe.
	pub udp_buffer: usize,
	/// How long a connected viewer waits for video before its peer reports
	/// [`PeerEvent::NoVideo`]; a streamer's peer waits twice as long for
	/// the viewer to report any of our video ([`PeerEvent::NoFeedback`]),
	/// so a Voelin viewer steps down first. `Duration::MAX` turns both off.
	pub stall_timeout: Duration,
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
			srtp_profiles: SrtpProfile::DEFAULT_ORDER.to_vec(),
			bandwidth_estimation: true,
			simulcast: false,
			h264_profile_level_ids: H264Profile::LADDER
				.iter()
				.map(|p| h264::profile_level_id(*p, h264::MIN_OFFER_LEVEL))
				.collect(),
			udp_buffer: DEFAULT_UDP_BUFFER,
			stall_timeout: DEFAULT_STALL_TIMEOUT,
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

	/// Offer H.264 in both profiles ([`H264Profile::LADDER`]) at the levels
	/// a `width` x `height` stream at `fps` and `bitrate` bit/s needs (at
	/// least 3.1, see [`crate::h264::offer_profile_level_id`]).
	pub fn set_h264_format(&mut self, width: u32, height: u32, fps: u32, bitrate: u64) {
		self.h264_profile_level_ids = H264Profile::LADDER
			.iter()
			.map(|p| h264::offer_profile_level_id(*p, width, height, fps, bitrate))
			.collect();
	}
}

#[derive(Debug, thiserror::Error)]
pub enum PeerError {
	#[error("network: {0}")]
	Io(#[from] std::io::Error),
	#[error("invalid SDP: {0}")]
	Sdp(String),
	/// The offer's video uses none of the codecs we decode.
	#[error(
		"no video codec in common: the stream offers {offered}, this client decodes {accepted}"
	)]
	NoCommonCodec { offered: String, accepted: String },
	#[error("WebRTC: {0}")]
	Rtc(#[from] RtcError),
	#[error("no usable local address")]
	NoAddress,
	#[error("no answer is expected")]
	NotOffering,
	#[error("peer connection closed")]
	Closed,
}

/// How a streamer's peer starts ([`Peer::offer_with`]).
#[derive(Clone, Debug, Default)]
pub struct OfferOptions<'a> {
	/// Initial bandwidth estimate in bit/s (with
	/// [`PeerConfig::bandwidth_estimation`]); the pacer starts at twice it.
	/// `None`: [`DEFAULT_START_BITRATE`].
	pub start_bitrate: Option<u64>,
	/// Bitrate the estimation probes up to, see
	/// [`Peer::set_desired_bitrate`]. `None`: the start bitrate.
	pub desired_bitrate: Option<u64>,
	/// The stream's layers, for RID simulcast ([`PeerConfig::simulcast`]).
	pub layers: &'a [LayerSpec],
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
	/// A received frame of a RID simulcast layer.
	LayerMedia { rid: Rid, frame: MediaFrame },
	/// The viewer asks for a keyframe.
	KeyframeRequest,
	/// The viewer asks for a keyframe of a RID simulcast layer.
	LayerKeyframeRequest(Rid),
	/// The bandwidth estimate to the viewer changed (bit/s; streamer side,
	/// with [`PeerConfig::bandwidth_estimation`]; from transport-cc
	/// feedback, or REMB from a viewer that sends no transport-cc).
	BitrateEstimate(u64),
	/// The answer accepted RID simulcast: video goes out per layer with
	/// these RIDs ([`Peer::write_rid`]).
	Simulcast(Vec<Rid>),
	/// The video codec the answer chose (streamer side), with the H.264
	/// profile: the first of our offer the viewer took. Video written to
	/// this peer must be in it.
	VideoCodec(VideoFormat),
	/// Viewer side: connected, but no video arrived within
	/// [`PeerConfig::stall_timeout`]. `audio`: audio did, so SRTP works
	/// and the video codec is the suspect. Once per connection.
	NoVideo { audio: bool },
	/// Streamer side: connected and sending video for twice
	/// [`PeerConfig::stall_timeout`], yet the viewer reported none of it
	/// (no receiver report of our video, no REMB): its SRTP fails or
	/// nothing reaches it. Keyframe requests do not count: a viewer that
	/// gets nothing keeps asking. Once per connection.
	NoFeedback,
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
	Offer(String, oneshot::Sender<Result<String, PeerError>>),
	RemoteCandidate(String),
	Write { kind: MediaKind, time: MediaTime, data: Arc<[u8]>, rid: Option<Rid> },
	RequestKeyframe,
	DesiredBitrate(u64),
	Close,
}

/// Handle to a peer connection task. Dropping it closes the connection.
pub struct Peer {
	cmd: mpsc::UnboundedSender<Cmd>,
	events: mpsc::UnboundedReceiver<PeerEvent>,
	srtp_profile: NegotiatedProfile,
}

impl Peer {
	/// Streamer side: create a connection that sends our media; returns the SDP
	/// offer for `respondjoinstreamrequest`.
	pub async fn offer(config: &PeerConfig, stream_id: &str) -> Result<(Self, String), PeerError> {
		Self::offer_with(config, stream_id, &OfferOptions::default()).await
	}

	/// [`offer`](Self::offer) with a start estimate for the bandwidth
	/// estimation and the stream's layers (for RID simulcast).
	pub async fn offer_with(
		config: &PeerConfig,
		stream_id: &str,
		options: &OfferOptions<'_>,
	) -> Result<(Self, String), PeerError> {
		Self::offer_now(config, stream_id, options)
	}

	/// [`offer_with`](Self::offer_with), which never waits: for the stream
	/// sessions, which offer again from peer events.
	pub(crate) fn offer_now(
		config: &PeerConfig,
		stream_id: &str,
		options: &OfferOptions<'_>,
	) -> Result<(Self, String), PeerError> {
		let start = options.start_bitrate.unwrap_or(DEFAULT_START_BITRATE).max(1);
		let bwe = config.bandwidth_estimation.then_some(start);
		let (mut rtc, srtp_profile) = build_rtc(config, &config.video_codecs, true, bwe);
		if bwe.is_some() {
			let desired = options.desired_bitrate.unwrap_or(start);
			rtc.bwe().set_desired_bitrate(Bitrate::bps(desired));
		}
		let net = Net::bind(config, &mut rtc)?;
		let mut api = rtc.sdp_api();
		let msid = Some(stream_id.to_owned());
		let mut mids = Vec::new();
		if config.video {
			let simulcast = config.simulcast.then(|| simulcast_offer(options.layers)).flatten();
			let mid =
				api.add_media(MediaKind::Video, Direction::SendOnly, msid.clone(), None, simulcast);
			mids.push((mid, MediaKind::Video));
		}
		if config.audio {
			let mid = api.add_media(MediaKind::Audio, Direction::SendOnly, msid, None, None);
			mids.push((mid, MediaKind::Audio));
		}
		let (offer, pending) = api.apply().ok_or(PeerError::Sdp("nothing to offer".into()))?;
		let sdp = offer.to_sdp_string();
		Ok((Self::spawn(rtc, net, config, Some(pending), mids, srtp_profile), sdp))
	}

	/// Viewer side: accept the streamer's offer; returns our SDP answer.
	pub async fn answer(config: &PeerConfig, offer: &str) -> Result<(Self, String), PeerError> {
		// Our codecs in the order of the offer, then the rest.
		let offered = offered_video_codecs(offer);
		let mut codecs: Vec<_> =
			offered.iter().copied().filter(|c| config.accept_video_codecs.contains(c)).collect();
		// Without one, the answer's video section would list no payload type,
		// which no peer (str0m included) can parse.
		if config.video
			&& codecs.is_empty()
			&& offer.lines().any(|l| l.trim_start().starts_with("m=video "))
		{
			let names = |codecs: &[VideoCodec]| match codecs {
				[] => "none we know".to_owned(),
				_ => codecs.iter().map(|c| c.sdp_name()).collect::<Vec<_>>().join(", "),
			};
			return Err(PeerError::NoCommonCodec {
				offered: names(&offered),
				accepted: names(&config.accept_video_codecs),
			});
		}
		for c in &config.accept_video_codecs {
			if !codecs.contains(c) {
				codecs.push(*c);
			}
		}
		let parsed = SdpOffer::from_sdp_string(offer).map_err(|e| PeerError::Sdp(e.to_string()))?;
		let (mut rtc, srtp_profile) = build_rtc(config, &codecs, false, None);
		let net = Net::bind(config, &mut rtc)?;
		let answer = rtc.sdp_api().accept_offer(parsed)?;
		let peer = Self::spawn(rtc, net, config, None, Vec::new(), srtp_profile);
		peer.resolve_mdns(offer);
		Ok((peer, answer.to_sdp_string()))
	}

	/// Resolve the mDNS names of the candidates in `sdp` (a whole SDP or one
	/// candidate line) in the background and add them as plain candidates:
	/// peers that hide their host addresses (browsers, other libwebrtc
	/// builds) can then still connect on a LAN. ICE takes whichever pair
	/// works, these or the others.
	fn resolve_mdns(&self, sdp: &str) {
		for line in sdp.lines() {
			let Some(name) = mdns::candidate_name(line) else { continue };
			let (name, line, cmd) = (name.to_owned(), line.to_owned(), self.cmd.clone());
			tokio::spawn(async move {
				match mdns::resolve(&name, MDNS_TIMEOUT).await {
					Some(ip) => {
						debug!(name, %ip, "mDNS candidate resolved");
						let _ =
							cmd.send(Cmd::RemoteCandidate(line.replace(&name, &ip.to_string())));
					}
					None => debug!(name, "mDNS candidate not resolved"),
				}
			});
		}
	}

	fn spawn(
		rtc: Rtc,
		net: Net,
		config: &PeerConfig,
		pending: Option<SdpPendingOffer>,
		mids: Vec<(Mid, MediaKind)>,
		srtp_profile: NegotiatedProfile,
	) -> Self {
		let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
		let (event_tx, event_rx) = mpsc::unbounded_channel();
		let task = Task {
			rtc,
			net,
			offerer: pending.is_some(),
			pending,
			cmd: cmd_rx,
			events: event_tx,
			mids,
			writers: HashMap::new(),
			stun: Vec::new(),
			twcc: false,
			stall: Stall { timeout: config.stall_timeout, ..Stall::default() },
		};
		tokio::spawn(task.run(config.stun_servers.clone()));
		Self { cmd: cmd_tx, events: event_rx, srtp_profile }
	}

	/// Streamer side: apply the viewer's answer.
	pub async fn accept_answer(&self, sdp: &str) -> Result<(), PeerError> {
		let (tx, rx) = oneshot::channel();
		self.cmd.send(Cmd::Answer(sdp.to_owned(), tx)).map_err(|_| PeerError::Closed)?;
		rx.await.map_err(|_| PeerError::Closed)??;
		self.resolve_mdns(sdp);
		Ok(())
	}

	/// Viewer side: answer a new offer of the streamer on this connection
	/// (a renegotiation: the same ICE credentials and DTLS fingerprint).
	/// The official client re-offers when the codec our answer chose is one
	/// it does not encode, and expects the answer from the same peer.
	pub async fn renegotiate(&self, offer: &str) -> Result<String, PeerError> {
		let (tx, rx) = oneshot::channel();
		self.cmd.send(Cmd::Offer(offer.to_owned(), tx)).map_err(|_| PeerError::Closed)?;
		let answer = rx.await.map_err(|_| PeerError::Closed)??;
		self.resolve_mdns(offer);
		Ok(answer)
	}

	/// Add a trickled remote candidate (`candidate:...`, `a=` prefix
	/// optional); one with an mDNS name once it is resolved.
	pub fn add_remote_candidate(&self, candidate: &str) {
		if mdns::candidate_name(candidate).is_some() {
			self.resolve_mdns(candidate);
		} else {
			let _ = self.cmd.send(Cmd::RemoteCandidate(candidate.to_owned()));
		}
	}

	/// Send one encoded frame (streamer). Dropped until connected.
	pub fn write(&self, kind: MediaKind, time: MediaTime, data: impl Into<Arc<[u8]>>) {
		let _ = self.cmd.send(Cmd::Write { kind, time, data: data.into(), rid: None });
	}

	/// Send one video frame of the RID simulcast layer `rid`
	/// ([`PeerEvent::Simulcast`]); `None` as [`write`](Self::write).
	pub fn write_rid(&self, kind: MediaKind, time: MediaTime, data: Arc<[u8]>, rid: Option<Rid>) {
		let _ = self.cmd.send(Cmd::Write { kind, time, data, rid });
	}

	/// Ask the streamer for a keyframe (viewer).
	pub fn request_keyframe(&self) {
		let _ = self.cmd.send(Cmd::RequestKeyframe);
	}

	/// The bitrate (bit/s) the bandwidth estimation probes up to: what we
	/// would send if the path allowed it.
	pub fn set_desired_bitrate(&self, bitrate: u64) {
		let _ = self.cmd.send(Cmd::DesiredBitrate(bitrate));
	}

	/// The SRTP profile DTLS negotiated, once it has.
	pub fn srtp_profile(&self) -> Option<SrtpProfile> {
		self.srtp_profile.get()
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

/// Payload types (and their RTX) of the H.264 profiles an offer lists, in
/// the order of [`PeerConfig::h264_profile_level_ids`]: 112 as before for
/// Constrained High, which str0m's defaults lack, and 108, str0m's own for
/// Constrained Baseline.
const H264_PTS: [(u8, u8); 2] = [(112, 113), (108, 109)];

/// The send simulcast of an offer: the layers with a RID, if at least two.
fn simulcast_offer(layers: &[LayerSpec]) -> Option<Simulcast> {
	let mut simulcast = Simulcast::new();
	for layer in layers {
		let Some(rid) = layer.rid.as_deref() else { continue };
		let id = Rid::from(rid);
		if *id != *rid {
			warn!(rid, sent = &*id, "RIDs are up to 8 letters, digits or _; the RID was changed");
		}
		if simulcast.send.iter().any(|l| l.rid == id) {
			warn!(rid, "duplicate RID, layer not offered");
			continue;
		}
		let mut builder = SimulcastLayer::new_with_attributes(&id);
		if let Some((w, h)) = layer.size {
			builder = builder.max_width(w).max_height(h);
		}
		if let Some(fps) = layer.max_fps {
			builder = builder.max_fps(fps);
		}
		if let Some(max) = layer.max_bitrate {
			builder = builder.max_br(u32::try_from(max).unwrap_or(u32::MAX));
		}
		simulcast.add_send_layer(builder.build());
	}
	(simulcast.send.len() >= 2).then_some(simulcast)
}

/// An RTC with Opus and `video` codecs, in this order of preference, our
/// DTLS, and bandwidth estimation starting at `bwe` bit/s if given. An
/// offer lists H.264 only as our encoders produce it (the
/// [`PeerConfig::h264_profile_level_ids`], packetization mode 1, best
/// first): with str0m's other variants offered too, a viewer may answer a
/// profile we never send. An answer takes str0m's variants and Constrained
/// High, which they lack.
fn build_rtc(
	config: &PeerConfig,
	video: &[VideoCodec],
	offer: bool,
	bwe: Option<u64>,
) -> (Rtc, NegotiatedProfile) {
	let mut rtc_config =
		RtcConfig::new().clear_codecs().enable_opus(config.audio).enable_bwe(bwe.map(Bitrate::bps));
	if offer {
		// The viewer's receiver reports ([`PeerEvent::NoFeedback`]).
		rtc_config = rtc_config.set_stats_interval(Some(Duration::from_secs(1)));
	}
	for codec in video {
		rtc_config = match codec {
			VideoCodec::Vp8 => rtc_config.enable_vp8(true),
			VideoCodec::Vp9 => rtc_config.enable_vp9(true),
			VideoCodec::H264 => {
				let mut c = if offer { rtc_config } else { rtc_config.enable_h264(true) };
				let ids = config.h264_profile_level_ids.iter().copied().filter(|id| {
					offer
						|| H264Profile::from_profile_level_id(*id)
							== Some(H264Profile::ConstrainedHigh)
				});
				for (id, (pt, rtx)) in ids.zip(H264_PTS) {
					c.codec_config().add_h264(pt.into(), Some(rtx.into()), true, id);
				}
				c
			}
			VideoCodec::Av1 => rtc_config.enable_av1(true),
			VideoCodec::H265 => rtc_config.enable_h265(true),
		};
	}
	dtls::build_rtc(rtc_config, &config.srtp_profiles)
}

/// `answer` with the payload types of each media line in the order of the
/// same media line of `offer` (those the offer lacks last). The offerer sends
/// the answer's first codec; str0m answers a new offer on a running
/// connection in the codec order the connection was built with, so without
/// this a streamer that re-offers with another codec first keeps sending the
/// old one. Only the `m=` line order changes, which str0m's own state does
/// not depend on.
fn order_like_offer(answer: &str, offer: &str) -> String {
	let offered: Vec<Vec<&str>> = offer
		.lines()
		.filter_map(|l| l.trim_end().strip_prefix("m="))
		.map(|m| m.split_whitespace().skip(3).collect())
		.collect();
	let mut section = 0;
	let mut out = String::with_capacity(answer.len());
	for line in answer.split_inclusive('\n') {
		let Some(media) = line.trim_end().strip_prefix("m=") else {
			out.push_str(line);
			continue;
		};
		let mut parts: Vec<&str> = media.split_whitespace().collect();
		if let Some(order) = offered.get(section)
			&& parts.len() > 3
		{
			parts[3..].sort_by_key(|pt| order.iter().position(|o| o == pt).unwrap_or(usize::MAX));
		}
		section += 1;
		out.push_str("m=");
		out.push_str(&parts.join(" "));
		out.push_str(&line[line.trim_end().len()..]);
	}
	out
}

/// The video codecs of the first video media line of `sdp`, in offered order.
pub(crate) fn offered_video_codecs(sdp: &str) -> Vec<VideoCodec> {
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
	fn bind(config: &PeerConfig, rtc: &mut Rtc) -> Result<Self, PeerError> {
		let hosts = if config.hosts.is_empty() {
			primary_ipv4().into_iter().collect()
		} else {
			config.hosts.clone()
		};
		let (tx, incoming) = mpsc::channel(256);
		let mut sockets = Vec::new();
		for ip in hosts {
			let socket = Arc::new(udp_socket(SocketAddr::new(ip, 0), config.udp_buffer)?);
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

/// A UDP socket on `addr` asking for `buffer` bytes of receive and send
/// buffer (0: the system's default), see [`PeerConfig::udp_buffer`]. What
/// the system granted is logged; once per process with a hint when it is
/// less than asked.
fn udp_socket(addr: SocketAddr, buffer: usize) -> std::io::Result<UdpSocket> {
	use socket2::{Domain, Protocol, Socket, Type};
	static CAPPED: std::sync::Once = std::sync::Once::new();
	let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
	if buffer > 0 {
		// Best effort: the system caps what it grants, and a smaller
		// buffer only costs packets under load.
		let _ = socket.set_recv_buffer_size(buffer);
		let _ = socket.set_send_buffer_size(buffer);
	}
	socket.set_nonblocking(true)?;
	socket.bind(&addr.into())?;
	let (receive, send) =
		(socket.recv_buffer_size().unwrap_or(0), socket.send_buffer_size().unwrap_or(0));
	debug!(%addr, asked = buffer, receive, send, "UDP socket buffers");
	if receive < buffer {
		CAPPED.call_once(|| {
			warn!(
				asked = buffer,
				granted = receive,
				"the system grants less UDP receive buffer than asked; high-bitrate streams \
				 may lose packets in bursts (Linux: sysctl net.core.rmem_max)"
			);
		});
	}
	UdpSocket::from_std(socket.into())
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
	/// A transport-cc estimate arrived: REMB is ignored from then on.
	twcc: bool,
	/// Made the offer: the streamer's side.
	offerer: bool,
	stall: Stall,
}

/// Whether media gets through once connected ([`PeerEvent::NoVideo`],
/// [`PeerEvent::NoFeedback`]).
#[derive(Debug, Default)]
struct Stall {
	timeout: Duration,
	connected: Option<Instant>,
	/// Checked already (once per connection).
	checked: bool,
	audio_in: bool,
	video_in: bool,
	video_out: bool,
	/// The viewer reported our video (receiver report, REMB).
	acknowledged: bool,
}

impl Stall {
	/// When the check is due: after the timeout for a viewer, twice it for
	/// a streamer.
	fn due(&self, offerer: bool) -> Option<Instant> {
		let wait = if offerer { self.timeout.saturating_mul(2) } else { self.timeout };
		self.connected.filter(|_| !self.checked)?.checked_add(wait)
	}

	/// A renegotiated connection waits from `now` again, if it is connected.
	fn restart(&mut self, now: Instant) {
		if self.connected.is_some() {
			*self = Self { timeout: self.timeout, connected: Some(now), ..Self::default() };
		}
	}

	/// The event to report at `now`, once, if media does not get through.
	fn check(&mut self, now: Instant, offerer: bool, video: bool) -> Option<PeerEvent> {
		if self.due(offerer).is_none_or(|due| now < due) {
			return None;
		}
		self.checked = true;
		if offerer {
			(self.video_out && !self.acknowledged).then_some(PeerEvent::NoFeedback)
		} else {
			(video && !self.video_in).then_some(PeerEvent::NoVideo { audio: self.audio_in })
		}
	}
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
		// Turns in a row str0m wanted again at once.
		let mut immediate = 0u32;
		loop {
			let now = Instant::now();
			self.rtc.handle_input(Input::Timeout(now))?;
			let video = self.mids.iter().any(|(_, kind)| *kind == MediaKind::Video);
			if let Some(event) = self.stall.check(now, self.offerer, video) {
				debug!(?event, "connected, but the media does not get through");
				let _ = self.events.send(event);
			}
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
			let deadline = self.stall.due(self.offerer).map_or(deadline, |due| due.min(deadline));
			// The pacer asks for a timeout at once after every packet it lets
			// out. Tokio's timer rounds that up to its next millisecond, so a
			// paced stream sent one packet a millisecond: 9.5 Mbit/s at 1188
			// bytes (str0m's probes measured exactly that, and the estimate
			// of a 60 Mbit/s stream on loopback fell to 9 Mbit/s). A due
			// timeout is handled at once instead, a bounded number of turns
			// in a row so packets and commands are still read in between.
			if deadline <= Instant::now() && immediate < 256 {
				immediate += 1;
				continue;
			}
			immediate = 0;
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
				self.stall.connected = Some(Instant::now());
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
					self.stall.audio_in = true;
					MediaKind::Audio
				} else {
					self.stall.video_in = true;
					MediaKind::Video
				};
				let frame = MediaFrame {
					kind,
					codec: d.params.spec().codec,
					time: d.time,
					network_time: d.network_time,
					contiguous: d.contiguous,
					data: d.data,
				};
				let _ = self.events.send(match d.rid {
					Some(rid) => PeerEvent::LayerMedia { rid, frame },
					None => PeerEvent::Media(frame),
				});
			}
			Event::KeyframeRequest(r) => {
				let _ = self.events.send(match r.rid {
					Some(rid) => PeerEvent::LayerKeyframeRequest(rid),
					None => PeerEvent::KeyframeRequest,
				});
			}
			Event::EgressBitrateEstimate(estimate) => {
				// Transport-cc estimates also come from the estimator's own
				// timer; a REMB only from the viewer.
				let bitrate = match estimate {
					BweKind::Twcc(b) => {
						self.twcc = true;
						Some(b)
					}
					BweKind::Remb(_, b) => {
						self.stall.acknowledged = true;
						(!self.twcc).then_some(b)
					}
					_ => None,
				};
				if let Some(b) = bitrate {
					let _ = self.events.send(PeerEvent::BitrateEstimate(b.as_u64()));
				}
			}
			// A receiver report of our video: the viewer decrypts it.
			Event::MediaEgressStats(stats)
				if stats.remote.is_some() && self.mids.contains(&(stats.mid, MediaKind::Video)) =>
			{
				self.stall.acknowledged = true;
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
			Cmd::Offer(sdp, reply) => {
				let result = SdpOffer::from_sdp_string(&sdp)
					.map_err(|e| PeerError::Sdp(e.to_string()))
					.and_then(|offer| Ok(self.rtc.sdp_api().accept_offer(offer)?.to_sdp_string()))
					.map(|answer| order_like_offer(&answer, &sdp));
				if result.is_ok() {
					// Another codec, maybe: it gets the whole wait anew.
					self.stall.restart(Instant::now());
				}
				let _ = reply.send(result);
			}
			Cmd::RemoteCandidate(line) => {
				let line = line.trim().trim_start_matches("a=");
				match Candidate::from_sdp_string(line) {
					Ok(c) => self.rtc.add_remote_candidate(c),
					Err(e) => debug!("ignoring remote candidate {line:?}: {e}"),
				}
			}
			Cmd::Write { kind, time, data, rid } => self.write(kind, time, data, rid),
			Cmd::RequestKeyframe => {
				for &(mid, _) in &self.mids {
					if let Some(mut w) = self.rtc.writer(mid) {
						let _ = w.request_keyframe(None, KeyframeRequestKind::Pli);
					}
				}
			}
			Cmd::DesiredBitrate(bitrate) => {
				self.rtc.bwe().set_desired_bitrate(Bitrate::bps(bitrate));
			}
			Cmd::Close => return Ok(false),
		}
		Ok(true)
	}

	fn accept_answer(&mut self, sdp: &str) -> Result<(), PeerError> {
		let pending = self.pending.take().ok_or(PeerError::NotOffering)?;
		let answer = SdpAnswer::from_sdp_string(sdp).map_err(|e| PeerError::Sdp(e.to_string()))?;
		self.rtc.sdp_api().accept_answer(pending, answer)?;
		// RID simulcast: the answer lists the layers it takes.
		for &(mid, kind) in &self.mids {
			if kind == MediaKind::Video
				&& let Some(Rids::Specific(rids)) = self.rtc.media(mid).map(|m| m.rids_tx())
				&& !rids.is_empty()
			{
				let _ = self.events.send(PeerEvent::Simulcast(rids.clone()));
			}
		}
		// The codec video is written in (see `find_writer`).
		if let Some((mid, pt)) = self.find_writer(MediaKind::Video) {
			self.writers.insert(MediaKind::Video, (mid, pt));
			let format = self.rtc.writer(mid).and_then(|w| {
				let spec = w.payload_params().find(|p| p.pt() == pt)?.spec();
				Some(match VideoCodec::from_codec(spec.codec)? {
					VideoCodec::H264 => VideoFormat::H264(
						spec.format
							.profile_level_id
							.and_then(H264Profile::from_profile_level_id)
							.unwrap_or_default(),
					),
					codec => codec.into(),
				})
			});
			if let Some(format) = format {
				debug!(%format, "the answer chose");
				let _ = self.events.send(PeerEvent::VideoCodec(format));
			}
		}
		Ok(())
	}

	/// A frame that cannot be sent (e.g. a RID the answer did not take) is
	/// dropped; the connection stays.
	fn write(&mut self, kind: MediaKind, time: MediaTime, data: Arc<[u8]>, rid: Option<Rid>) {
		if !self.writers.contains_key(&kind) {
			let Some((mid, pt)) = self.find_writer(kind) else {
				trace!(?kind, "no negotiated media to write to");
				return;
			};
			self.writers.insert(kind, (mid, pt));
		}
		let (mid, pt) = self.writers[&kind];
		if let Some(mut writer) = self.rtc.writer(mid) {
			if let Some(rid) = rid {
				writer = writer.rid(rid);
			}
			match writer.write(pt, Instant::now(), time, data) {
				Ok(()) => self.stall.video_out |= kind == MediaKind::Video,
				Err(e) => debug!(?kind, ?rid, "frame not sent: {e}"),
			}
		}
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
		// Only the profiles we encode, best first: a viewer must not answer
		// another.
		let h264: Vec<&str> = offer
			.lines()
			.filter(|l| l.starts_with("a=fmtp:") && l.contains("profile-level-id"))
			.collect();
		assert_eq!(h264.len(), 2, "{offer}");
		assert!(h264[0].contains("profile-level-id=640c1f"), "{offer}");
		assert!(h264[1].contains("profile-level-id=42e01f"), "{offer}");
		let pts: Vec<&str> = offer
			.lines()
			.find_map(|l| l.strip_prefix("m=video "))
			.unwrap()
			.split(' ')
			.skip(2)
			.collect();
		let at = |pt: &str| pts.iter().position(|p| *p == pt).unwrap();
		assert!(at("112") < at("108") && at("108") < at("96"), "{pts:?}");
		let (_peer, answer) = Peer::answer(&PeerConfig::loopback(), &offer).await.unwrap();
		assert_eq!(offered_video_codecs(&answer)[0], VideoCodec::H264, "{answer}");
		let vp8_only =
			PeerConfig { accept_video_codecs: vec![VideoCodec::Vp8], ..PeerConfig::loopback() };
		let (_peer, answer) = Peer::answer(&vp8_only, &offer).await.unwrap();
		assert_eq!(offered_video_codecs(&answer), [VideoCodec::Vp8], "{answer}");
	}

	/// The ICE username fragment and DTLS fingerprint of an SDP.
	fn transport(sdp: &str) -> (String, String) {
		let line = |prefix: &str| {
			sdp.lines().find_map(|l| l.strip_prefix(prefix)).unwrap_or_default().trim().to_owned()
		};
		(line("a=ice-ufrag:"), line("a=fingerprint:"))
	}

	/// A streamer's new offer (the official client re-offers when it does not
	/// encode the codec our answer chose) is answered on the same connection:
	/// same ICE credentials and DTLS fingerprint, the new media answered.
	#[tokio::test]
	async fn a_new_offer_is_answered_on_the_same_connection() {
		let mut streamer = RtcConfig::new()
			.clear_codecs()
			.enable_vp8(true)
			.enable_opus(true)
			.build(Instant::now());
		let mut api = streamer.sdp_api();
		api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
		let (offer, pending) = api.apply().unwrap();
		let (peer, answer) =
			Peer::answer(&PeerConfig::loopback(), &offer.to_sdp_string()).await.unwrap();
		let answer_sdp = SdpAnswer::from_sdp_string(&answer).unwrap();
		streamer.sdp_api().accept_answer(pending, answer_sdp).unwrap();

		let mut api = streamer.sdp_api();
		api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
		let (offer, _pending) = api.apply().unwrap();
		let again = peer.renegotiate(&offer.to_sdp_string()).await.unwrap();
		assert_eq!(transport(&again), transport(&answer), "{again}");
		assert!(again.lines().all(|l| !l.is_empty() || l.ends_with('\r')), "line ends kept");
		assert!(!transport(&again).0.is_empty());
		assert!(again.contains("m=audio") && again.contains("opus/48000"), "{again}");
	}

	#[test]
	fn answers_follow_the_offers_codec_order() {
		let offer = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nm=video 9 UDP/TLS/RTP/SAVPF 98 99 96 97 45\r\n";
		let answer = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96 97 7 98 99\r\na=rtpmap:96 VP8/90000\r\n";
		assert_eq!(
			order_like_offer(answer, offer),
			"v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\nm=video 9 UDP/TLS/RTP/SAVPF 98 99 96 97 7\r\na=rtpmap:96 VP8/90000\r\n"
		);
		assert_eq!(order_like_offer("v=0\r\n", offer), "v=0\r\n");
	}

	/// An offer of codecs we cannot decode is refused with the reason, not
	/// answered with a video section the streamer cannot parse.
	#[tokio::test]
	async fn no_common_codec_is_refused() {
		let streamer =
			PeerConfig { video_codecs: vec![VideoCodec::H264], ..PeerConfig::loopback() };
		let (_peer, offer) = Peer::offer(&streamer, "s").await.unwrap();
		let vp8_only =
			PeerConfig { accept_video_codecs: vec![VideoCodec::Vp8], ..PeerConfig::loopback() };
		let error = Peer::answer(&vp8_only, &offer).await.err().expect("refused");
		assert!(matches!(error, PeerError::NoCommonCodec { .. }), "{error}");
		assert_eq!(
			error.to_string(),
			"no video codec in common: the stream offers H264, this client decodes VP8"
		);
	}

	/// The format the answer chose, as the streamer's peer reports it.
	async fn answered_format(peer: &mut Peer) -> VideoFormat {
		loop {
			match peer.next_event().await.unwrap() {
				PeerEvent::VideoCodec(format) => return format,
				PeerEvent::Closed => panic!("closed"),
				_ => {}
			}
		}
	}

	/// Several codecs offered: the streamer learns which one each viewer's
	/// answer chose; H.264 carries the level the stream needs, in both
	/// profiles.
	#[tokio::test]
	async fn streamer_learns_the_answered_codec() {
		let mut streamer = PeerConfig {
			video_codecs: vec![VideoCodec::Vp8, VideoCodec::H264, VideoCodec::H265],
			..PeerConfig::loopback()
		};
		streamer.set_h264_format(1920, 1080, 60, 8_000_000);
		let (mut peer, offer) = Peer::offer(&streamer, "s").await.unwrap();
		assert!(offer.contains("profile-level-id=640c2a"), "{offer}");
		assert!(offer.contains("profile-level-id=42e02a"), "{offer}");
		assert!(offer.contains("H265/90000"), "{offer}");
		let high = VideoFormat::H264(H264Profile::ConstrainedHigh);
		let viewers = [
			(vec![VideoCodec::Vp8, VideoCodec::H264], VideoFormat::Vp8),
			// Our viewers take both H.264 profiles: the better one.
			(vec![VideoCodec::H264], high),
			(vec![VideoCodec::H265], VideoFormat::H265),
		];
		for (accept, expected) in viewers {
			let (mut streamer_peer, offer) = Peer::offer(&streamer, "s").await.unwrap();
			let config = PeerConfig { accept_video_codecs: accept, ..PeerConfig::loopback() };
			let (_viewer, answer) = Peer::answer(&config, &offer).await.unwrap();
			streamer_peer.accept_answer(&answer).await.unwrap();
			assert_eq!(answered_format(&mut streamer_peer).await, expected);
		}
		peer.close();
		while peer.next_event().await.is_some() {}
	}

	/// A viewer whose H.264 is Constrained Baseline only (headless
	/// Chromium's libwebrtc lists no High profile) answers the fallback
	/// payload type of our H.264 ladder, and the streamer learns the profile.
	#[tokio::test]
	async fn a_baseline_only_viewer_gets_the_fallback_profile() {
		let streamer =
			PeerConfig { video_codecs: vec![VideoCodec::H264], ..PeerConfig::loopback() };
		let (mut streamer_peer, offer) = Peer::offer(&streamer, "s").await.unwrap();
		let mut viewer = RtcConfig::new().clear_codecs().enable_opus(true);
		viewer.codec_config().add_h264(102.into(), Some(103.into()), true, 0x42e01f);
		let mut viewer = viewer.build(Instant::now());
		let offer = SdpOffer::from_sdp_string(&offer).unwrap();
		let answer = viewer.sdp_api().accept_offer(offer).unwrap().to_sdp_string();
		assert!(answer.contains("profile-level-id=42e01f"), "{answer}");
		assert!(!answer.contains("profile-level-id=640c"), "{answer}");
		streamer_peer.accept_answer(&answer).await.unwrap();
		assert_eq!(
			answered_format(&mut streamer_peer).await,
			VideoFormat::H264(H264Profile::ConstrainedBaseline)
		);
	}

	#[test]
	fn format_bits_are_distinct() {
		let bits = VideoFormat::ALL.iter().fold(0u8, |bits, f| {
			assert_eq!(bits & f.bit(), 0, "{f}");
			bits | f.bit()
		});
		assert_eq!(bits.count_ones(), 6);
		assert_eq!(VideoFormat::from(VideoCodec::H264).bit(), VideoCodec::H264.bit());
	}

	/// Once per connection, after the timeout (twice it for a streamer),
	/// anew after a renegotiation; never with `Duration::MAX`.
	#[test]
	fn stall_checks() {
		let t0 = Instant::now();
		let second = Duration::from_secs(1);
		let mut viewer = Stall { timeout: second, ..Stall::default() };
		assert!(viewer.check(t0 + 10 * second, false, true).is_none(), "not connected");
		viewer.connected = Some(t0);
		viewer.audio_in = true;
		assert!(viewer.check(t0 + second / 2, false, true).is_none());
		let event = viewer.check(t0 + second, false, true);
		assert!(matches!(event, Some(PeerEvent::NoVideo { audio: true })), "{event:?}");
		assert!(viewer.check(t0 + 2 * second, false, true).is_none(), "once");
		// A new offer on the connection: the new codec gets its own wait.
		viewer.restart(t0 + 2 * second);
		viewer.video_in = true;
		assert!(viewer.check(t0 + 4 * second, false, true).is_none(), "video came");
		assert_eq!(viewer.due(false), None);

		let mut streamer = Stall { timeout: second, connected: Some(t0), ..Stall::default() };
		streamer.video_out = true;
		assert!(streamer.check(t0 + second, true, true).is_none(), "a streamer waits longer");
		assert!(matches!(streamer.check(t0 + 2 * second, true, true), Some(PeerEvent::NoFeedback)));
		let mut working = Stall {
			timeout: second,
			connected: Some(t0),
			video_out: true,
			acknowledged: true,
			..Stall::default()
		};
		assert!(working.check(t0 + 2 * second, true, true).is_none(), "the viewer reported");

		let off = Stall { timeout: Duration::MAX, connected: Some(t0), ..Stall::default() };
		assert_eq!(off.due(false), None);
	}

	/// The SDP lines that make up the media description (not the random ids,
	/// ports, credentials and fingerprints).
	fn media_lines(sdp: &str) -> Vec<&str> {
		const KEPT: [&str; 8] = [
			"m=",
			"a=setup",
			"a=extmap",
			"a=rtcp-fb",
			"a=rtpmap",
			"a=fmtp",
			"a=simulcast",
			"a=rid",
		];
		sdp.lines().filter(|l| KEPT.iter().any(|k| l.starts_with(k))).collect()
	}

	/// Bandwidth estimation changes nothing in the offer official viewers get
	/// (str0m always offers transport-cc and abs-send-time).
	#[tokio::test]
	async fn offer_with_bandwidth_estimation_is_unchanged() {
		let without = PeerConfig { bandwidth_estimation: false, ..PeerConfig::loopback() };
		let (_a, plain) = Peer::offer(&without, "s").await.unwrap();
		let (_b, bwe) = Peer::offer(&PeerConfig::loopback(), "s").await.unwrap();
		assert_eq!(media_lines(&plain), media_lines(&bwe));
		assert!(bwe.contains("transport-wide-cc") && bwe.contains("transport-cc"), "{bwe}");
		assert!(!bwe.contains("a=simulcast"), "{bwe}");
		// Layers without the simulcast flag: still the plain offer.
		let layers = [
			LayerSpec { rid: Some("h".into()), ..LayerSpec::single(2_000_000) },
			LayerSpec {
				id: 1,
				size: Some((640, 360)),
				max_fps: Some(15),
				max_bitrate: Some(500_000),
				rid: Some("l".into()),
				..LayerSpec::single(400_000)
			},
		];
		let options = OfferOptions { layers: &layers, ..OfferOptions::default() };
		let (_c, layered) = Peer::offer_with(&PeerConfig::loopback(), "s", &options).await.unwrap();
		assert_eq!(media_lines(&plain), media_lines(&layered));
		// With it: RID simulcast with the layers' limits.
		let simulcast = PeerConfig { simulcast: true, ..PeerConfig::loopback() };
		let (_d, sdp) = Peer::offer_with(&simulcast, "s", &options).await.unwrap();
		assert!(sdp.contains("a=simulcast:send h;l"), "{sdp}");
		assert!(sdp.contains("a=rid:h send\r\n"), "{sdp}");
		assert!(
			sdp.contains("a=rid:l send max-width=640;max-height=360;max-fps=15;max-br=500000"),
			"{sdp}"
		);
		// One layer with a RID is not simulcast.
		let options = OfferOptions { layers: &layers[..1], ..OfferOptions::default() };
		let (_e, sdp) = Peer::offer_with(&simulcast, "s", &options).await.unwrap();
		assert!(!sdp.contains("a=simulcast"), "{sdp}");
	}
}
