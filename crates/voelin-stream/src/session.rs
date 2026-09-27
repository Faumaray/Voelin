//! Stream sessions: the streamer's and the viewer's side of TeamSpeak 6
//! streams as state machines, and the list of streams in our channel.
//!
//! The sessions do no connection IO. They are fed the stream notifications of
//! one server connection and the app's decisions, and queue [`Output`]s: a
//! [`Request`] to send on the connection or a [`StreamEvent`] for the app.
//! They own the WebRTC [`Peer`]s (one per viewer on the streamer's side, one
//! per watched stream on the viewer's side). [`Streams::wait_peers`] handles
//! peer events and is cancel safe, so it can sit in a `select!` next to the
//! connection:
//!
//! ```ignore
//! loop {
//!     tokio::select! {
//!         msg = messages.next() => {
//!             for n in StreamNotification::from_message(&msg) {
//!                 streams.handle_notification(n).await;
//!             }
//!         }
//!         () = streams.wait_peers() => {}
//!     }
//!     while let Some(output) = streams.poll_output() {
//!         // Output::Request: send `request.to_command()`, report failures
//!         // with `streams.request_failed`. Output::Event: tell the app.
//!     }
//! }
//! ```

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use str0m::media::{MediaKind, MediaTime, Rid};
use tracing::{debug, warn};
use tsclientlib::ClientId;
use tsproto_packets::packets::OutCommand;

use crate::discovery::{ClientState, Discovery, StreamLookup};
use crate::dtls::SrtpProfile;
use crate::feedback::LayerFeedback;
use crate::layer::{LayerId, LayerSet, LayerSpec};
use crate::peer::{MediaFrame, OfferOptions, Peer, PeerConfig, PeerEvent};
use crate::proto::{self, LeaveReason, StreamInfo, StreamNotification, StreamSetup};
use crate::signal::Signal;
use crate::source::EncodedFrame;

/// Keyframe requests of a layer closer together than this are merged into
/// one event.
const KEYFRAME_INTERVAL: Duration = Duration::from_millis(250);
/// A viewer moves up to a layer once its estimate exceeds the layer's
/// `min_bitrate` by this many percent ...
const UP_MARGIN_PERCENT: u64 = 20;
/// ... for this long.
const UP_DELAY: Duration = Duration::from_secs(2);
/// Lowest bitrate target (bit/s) of a video layer: below it encoders cannot
/// produce usable video.
pub const MIN_VIDEO_BITRATE: u64 = 30_000;
/// Changed bandwidth estimates are reported in the viewer list at most this
/// often (layer and state changes at once).
const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// A layer's bitrate target is reported when it moved by more than
/// 1/`BITRATE_REPORT_STEP` (5%) since the last report.
const BITRATE_REPORT_STEP: u64 = 20;
/// How often a viewer asks for a new offer after its connection failed.
const MAX_RECONNECTS: u32 = 3;
/// Peer events handled per peer and poll, so one busy peer cannot starve the rest.
const EVENTS_PER_POLL: usize = 64;

/// A command for the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
	/// `setupstream`: start our stream.
	Setup(StreamSetup),
	/// `stopstream`: end our stream.
	Stop { id: String },
	/// `joinstreamrequest ... is_remove=0`: ask to watch.
	Join { id: String, streamer: ClientId, message: String },
	/// `joinstreamrequest ... is_remove=1`: withdraw the request or stop watching.
	Leave { id: String, streamer: ClientId },
	/// `respondjoinstreamrequest`: accept a viewer with our offer, or deny.
	Respond { id: String, viewer: ClientId, offer: Option<String>, accept: bool },
	/// `streamsignaling`
	Signal { id: String, peer: ClientId, signal: Signal },
	/// `removeclientfromstream`
	RemoveViewer { id: String, viewer: ClientId, reason: LeaveReason },
	/// `requeststreaminfo`: the streams of a client (see [`crate::discovery`]).
	StreamInfo { streamer: ClientId },
}

impl Request {
	pub fn to_command(&self) -> OutCommand {
		match self {
			Self::Setup(setup) => proto::setup(setup),
			Self::Stop { id } => proto::stop(id),
			Self::Join { id, streamer, message } => {
				proto::join_request(id, *streamer, message, false)
			}
			Self::Leave { id, streamer } => proto::join_request(id, *streamer, "", true),
			Self::Respond { id, viewer, offer, accept } => {
				proto::respond(id, *viewer, offer.as_deref(), *accept)
			}
			Self::Signal { id, peer, signal } => proto::signaling(id, *peer, &signal.to_json()),
			Self::RemoveViewer { id, viewer, reason } => proto::remove_viewer(id, *viewer, *reason),
			Self::StreamInfo { streamer } => proto::stream_info(*streamer),
		}
	}
}

/// Why a stream ended for us.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
	/// We stopped streaming or watching.
	Local,
	/// The streamer denied our join request.
	Denied,
	/// The stream was stopped (or its streamer disconnected).
	Stopped,
	/// The streamer or the server ended our viewing session.
	Removed(Option<LeaveReason>),
	/// A command failed or no connection could be established.
	Failed(String),
}

/// A viewer of our stream, as the streamer sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerState {
	/// Asked to watch, waiting for our decision.
	Requested,
	/// Offer sent; waiting for the answer and the connection.
	Connecting,
	Connected,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewerInfo {
	pub client: ClientId,
	pub state: ViewerState,
	/// The message of the join request.
	pub message: String,
	/// The simulcast layer the viewer receives (it moves at a keyframe of the
	/// new layer); `None` before we offered, and with RID simulcast (all
	/// layers).
	pub layer: Option<LayerId>,
	/// The latest bandwidth estimate of the connection (bit/s).
	pub estimate: Option<u64>,
	/// The SRTP profile of the connection, once DTLS is done.
	pub srtp_profile: Option<SrtpProfile>,
}

/// What happens to our own stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamerEvent {
	/// The server confirmed `setupstream`: the stream is live under `id`.
	Live {
		id: String,
	},
	/// A viewer asks to watch (without auto-accept); answer with [`Streams::respond`].
	Request {
		viewer: ClientId,
		message: String,
	},
	/// The viewers, their states or layers changed (full list; changed
	/// estimates are included about once a second).
	Viewers(Vec<ViewerInfo>),
	/// A viewer of `layer` connected, lost a frame or switches to it: the
	/// layer's encoder should send a keyframe. Also flagged in
	/// [`StreamerSession::layer_feedback`].
	KeyframeRequest {
		layer: LayerId,
	},
	/// The bitrate target of `layer` (bit/s) changed: the lowest bandwidth
	/// estimate of its viewers, see [`StreamerSession::layer_bitrate`].
	LayerBitrate {
		layer: LayerId,
		bitrate: u64,
	},
	Ended(EndReason),
}

/// What happens to a stream we watch.
#[derive(Clone, Debug)]
pub enum WatchEvent {
	/// The streamer accepted our join request; connecting.
	Accepted,
	Connected,
	/// An encoded video or audio frame.
	Frame(MediaFrame),
	Ended(EndReason),
}

#[derive(Clone, Debug)]
pub enum StreamEvent {
	/// The streams in our channel changed (full list).
	Streams(Vec<StreamInfo>),
	/// Our own stream.
	Streamer(StreamerEvent),
	/// A stream we watch.
	Watch { id: String, event: WatchEvent },
}

#[derive(Clone, Debug)]
pub enum Output {
	/// Send `request.to_command()` on the connection.
	Request(Request),
	Event(StreamEvent),
}

/// Queue of [`Output`]s the sessions append to.
#[derive(Debug, Default)]
pub struct Outbox(VecDeque<Output>);

impl Outbox {
	pub fn request(&mut self, request: Request) {
		self.0.push_back(Output::Request(request));
	}

	pub fn event(&mut self, event: StreamEvent) {
		self.0.push_back(Output::Event(event));
	}

	pub fn pop(&mut self) -> Option<Output> {
		self.0.pop_front()
	}

	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

/// A call the sessions cannot carry out.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
	#[error("already streaming")]
	AlreadyStreaming,
	#[error("not streaming")]
	NotStreaming,
	#[error("the stream is not live yet")]
	NotLive,
	#[error("client {0} has not asked to watch")]
	NoRequest(u16),
	#[error("client {0} is not a viewer")]
	UnknownViewer(u16),
	#[error("unknown stream {0}")]
	UnknownStream(String),
	#[error("already watching stream {0}")]
	AlreadyWatching(String),
	#[error("not watching stream {0}")]
	NotWatching(String),
	#[error("cannot watch our own stream")]
	OwnStream,
}

/// The streams in our channel, from `notifystreamstarted`/`notifystreaminfo`/
/// `notifystreamstopped` and the clients' `client_is_streaming`.
#[derive(Clone, Debug, Default)]
pub struct StreamDirectory {
	streams: BTreeMap<String, StreamInfo>,
	/// Clients with `client_is_streaming=1`.
	streaming: BTreeSet<u16>,
}

impl StreamDirectory {
	/// Apply a notification; `true` if the list of streams changed.
	pub fn apply(&mut self, n: &StreamNotification) -> bool {
		match n {
			StreamNotification::Started { info, .. } | StreamNotification::Info(info) => {
				self.streaming.insert(info.streamer.0);
				self.streams.insert(info.id.clone(), info.clone()).as_ref() != Some(info)
			}
			StreamNotification::Updated {
				id, name, kind, bitrate, viewer_limit, audio, ..
			} => {
				let Some(info) = self.streams.get_mut(id) else { return false };
				let before = info.clone();
				info.name = name.clone().unwrap_or(before.name.clone());
				info.kind = kind.unwrap_or(before.kind);
				info.bitrate = bitrate.unwrap_or(before.bitrate);
				info.viewer_limit = viewer_limit.unwrap_or(before.viewer_limit);
				info.audio = audio.unwrap_or(before.audio);
				*info != before
			}
			StreamNotification::Stopped { id, .. } => self.streams.remove(id).is_some(),
			_ => false,
		}
	}

	/// `client_is_streaming` of a client changed. When it stops, its streams
	/// are dropped; returns `true` if the list of streams changed.
	pub fn set_streaming(&mut self, client: ClientId, streaming: bool) -> bool {
		if streaming {
			self.streaming.insert(client.0);
			false
		} else {
			self.streaming.remove(&client.0);
			self.retain_streamers(|c| c != client)
		}
	}

	/// Drop the streams of streamers `keep` rejects (e.g. clients that left).
	pub fn retain_streamers(&mut self, mut keep: impl FnMut(ClientId) -> bool) -> bool {
		let before = self.streams.len();
		self.streams.retain(|_, s| keep(s.streamer));
		self.streaming.retain(|c| keep(ClientId(*c)));
		self.streams.len() != before
	}

	pub fn get(&self, id: &str) -> Option<&StreamInfo> {
		self.streams.get(id)
	}

	pub fn by_streamer(&self, client: ClientId) -> Option<&StreamInfo> {
		self.streams.values().find(|s| s.streamer == client)
	}

	pub fn iter(&self) -> impl Iterator<Item = &StreamInfo> {
		self.streams.values()
	}

	pub fn to_vec(&self) -> Vec<StreamInfo> {
		self.streams.values().cloned().collect()
	}

	/// Clients that stream, but whose stream id we have not been told.
	pub fn unannounced_streamers(&self) -> impl Iterator<Item = ClientId> + '_ {
		self.streaming
			.iter()
			.filter(|c| !self.streams.values().any(|s| s.streamer.0 == **c))
			.map(|c| ClientId(*c))
	}
}

/// Options for our own stream.
#[derive(Clone, Debug, Default)]
pub struct StreamerOptions {
	pub setup: StreamSetup,
	/// Accept every join request. Otherwise each one is reported as
	/// [`StreamerEvent::Request`] and answered with [`Streams::respond`].
	pub auto_accept: bool,
	/// The simulcast layers the source produces. Empty: one layer (0) at the
	/// setup's bitrate. Change it while live with [`Streams::set_layers`].
	pub layers: Vec<LayerSpec>,
	/// Bandwidth estimate (bit/s) a new viewer starts with: it gets the
	/// highest layer whose `min_bitrate` fits. `None`: the `bitrate` of the
	/// layer with the highest `min_bitrate`, i.e. new viewers start on the top
	/// layer, as a stream without simulcast does.
	pub start_bitrate: Option<u64>,
}

/// The layers of our stream with their state, allocated when the list is set.
#[derive(Debug)]
struct LayerTable {
	specs: Vec<LayerSpec>,
	/// The RID of each layer, for peers with RID simulcast.
	rids: Vec<Option<Rid>>,
	/// Indices of `specs` by descending `min_bitrate` (then `bitrate`): the
	/// order viewers step down through.
	ladder: Vec<usize>,
	/// When a keyframe of each layer was last asked for.
	last_keyframe_request: Vec<Option<Instant>>,
	/// The bitrate target last reported per layer.
	reported: Vec<Option<u64>>,
}

impl LayerTable {
	/// `specs`, or one layer 0 at `bitrate` if empty. Later layers with an id
	/// already listed are dropped.
	fn new(specs: &[LayerSpec], bitrate: u64) -> Self {
		let mut unique: Vec<LayerSpec> = Vec::with_capacity(specs.len().max(1));
		for spec in specs {
			if unique.iter().any(|s| s.id == spec.id) {
				warn!(layer = spec.id, "duplicate layer id, ignored");
			} else {
				unique.push(spec.clone());
			}
		}
		if unique.is_empty() {
			unique.push(LayerSpec::single(bitrate));
		}
		let mut ladder: Vec<usize> = (0..unique.len()).collect();
		ladder.sort_by(|a, b| {
			let (a, b) = (&unique[*a], &unique[*b]);
			b.min_bitrate.cmp(&a.min_bitrate).then(b.bitrate.cmp(&a.bitrate))
		});
		Self {
			rids: unique.iter().map(|s| s.rid.as_deref().map(Rid::from)).collect(),
			last_keyframe_request: vec![None; unique.len()],
			reported: vec![None; unique.len()],
			ladder,
			specs: unique,
		}
	}

	fn len(&self) -> usize {
		self.specs.len()
	}

	fn index(&self, layer: LayerId) -> Option<usize> {
		self.specs.iter().position(|s| s.id == layer)
	}

	fn rid_index(&self, rid: Rid) -> Option<usize> {
		self.rids.iter().position(|r| *r == Some(rid))
	}

	/// Position of layer `index` in the ladder (0: top).
	fn rank(&self, index: usize) -> usize {
		self.ladder.iter().position(|i| *i == index).unwrap_or(usize::MAX)
	}

	fn top(&self) -> usize {
		self.ladder[0]
	}

	/// The highest layer whose `min_bitrate` `estimate` reaches, else the
	/// lowest.
	fn fitting(&self, estimate: u64) -> usize {
		let fits = self.ladder.iter().find(|i| self.specs[**i].min_bitrate <= estimate);
		fits.or(self.ladder.last()).copied().unwrap_or(0)
	}

	/// What a layer is sent at when bandwidth allows: `max_bitrate`, else
	/// `bitrate`.
	fn cap(&self, index: usize) -> u64 {
		let spec = &self.specs[index];
		spec.max_bitrate.unwrap_or(spec.bitrate)
	}

	/// Layers with a RID, if at least two: what an offer with RID simulcast
	/// carries.
	fn rid_layers(&self) -> impl Iterator<Item = usize> + '_ {
		let n = self.rids.iter().filter(|r| r.is_some()).count();
		(0..self.len()).filter(move |i| n >= 2 && self.rids[*i].is_some())
	}

	/// The share of `estimate` a RID simulcast peer that takes `rids` has for
	/// layer `index`: from the lowest layer up, each gets up to its
	/// [`cap`](Self::cap), the highest the rest.
	fn rid_share(&self, rids: &[Rid], estimate: u64, index: usize) -> Option<u64> {
		let taken = |i: usize| self.rids[i].is_some_and(|r| rids.contains(&r));
		if !taken(index) {
			return None;
		}
		let top = self.ladder.iter().copied().find(|i| taken(*i))?;
		let mut remaining = estimate;
		for &i in self.ladder.iter().rev().filter(|i| taken(**i)) {
			let share = if i == top { remaining } else { remaining.min(self.cap(i)) };
			if i == index {
				return Some(share);
			}
			remaining -= share;
		}
		None
	}
}

/// Which layer a viewer without RID simulcast receives, and switching
/// between layers: down as soon as the estimate falls below the layer's
/// `min_bitrate`, up once the estimate has exceeded the higher layer's
/// `min_bitrate` by [`UP_MARGIN_PERCENT`] for [`UP_DELAY`]. A switch waits
/// for a keyframe of the new layer; until then the old layer is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LayerChoice {
	/// The layer whose frames the viewer gets.
	layer: LayerId,
	/// The layer to switch to at its next keyframe.
	pending: Option<LayerId>,
	/// Since when the estimate allows a higher layer.
	up_since: Option<Instant>,
}

impl LayerChoice {
	fn new(layer: LayerId) -> Self {
		Self { layer, pending: None, up_since: None }
	}

	/// The layer the viewer is on or heading to.
	fn target(&self) -> LayerId {
		self.pending.unwrap_or(self.layer)
	}

	/// A new estimate. Returns the layer a keyframe is needed of when the
	/// viewer is to switch.
	fn estimate(&mut self, table: &LayerTable, estimate: u64, now: Instant) -> Option<LayerId> {
		let Some(current) = table.index(self.target()) else {
			// The layer is gone (new layer list).
			self.up_since = None;
			return self.switch_to(table.specs[table.fitting(estimate)].id);
		};
		if estimate < table.specs[current].min_bitrate {
			self.up_since = None;
			return self.switch_to(table.specs[table.fitting(estimate)].id);
		}
		let with_margin = u128::from(estimate) * 100 / (100 + u128::from(UP_MARGIN_PERCENT));
		let up = table.fitting(u64::try_from(with_margin).unwrap_or(u64::MAX));
		if table.rank(up) >= table.rank(current) {
			self.up_since = None;
			return None;
		}
		let since = *self.up_since.get_or_insert(now);
		if now.saturating_duration_since(since) < UP_DELAY {
			return None;
		}
		self.up_since = None;
		self.switch_to(table.specs[up].id)
	}

	/// Head for `layer`; the layer to request a keyframe of, if any.
	fn switch_to(&mut self, layer: LayerId) -> Option<LayerId> {
		if layer == self.layer {
			self.pending = None;
			None
		} else if self.pending == Some(layer) {
			None
		} else {
			self.pending = Some(layer);
			Some(layer)
		}
	}

	/// A video frame of `layer`: whether it goes to the viewer, and whether
	/// the viewer switched to it.
	fn frame(&mut self, layer: LayerId, keyframe: bool) -> (bool, bool) {
		if keyframe && self.pending == Some(layer) {
			self.layer = layer;
			self.pending = None;
			return (true, true);
		}
		(self.layer == layer, false)
	}
}

struct ViewerSlot {
	state: ViewerState,
	message: String,
	/// `None` while the viewer waits for our decision or after its connection closed.
	peer: Option<Peer>,
	/// The viewer's layer (without RID simulcast).
	choice: LayerChoice,
	/// The RIDs the viewer's answer took (RID simulcast): it gets all these layers.
	rids: Option<Vec<Rid>>,
	/// The latest bandwidth estimate of the connection (bit/s).
	estimate: Option<u64>,
	srtp_profile: Option<SrtpProfile>,
}

impl ViewerSlot {
	fn new(message: String) -> Self {
		Self {
			state: ViewerState::Requested,
			message,
			peer: None,
			choice: LayerChoice::new(0),
			rids: None,
			estimate: None,
			srtp_profile: None,
		}
	}

	fn connected(&self) -> bool {
		self.state == ViewerState::Connected
	}
}

/// Our own stream: `setupstream`, one peer connection per accepted viewer.
///
/// With several layers, each viewer gets the layer its bandwidth estimate
/// allows ([`LayerChoice`]), or every layer with its RID if its connection
/// negotiated RID simulcast. Keyframe requests and bitrate targets go to the
/// encoders per layer, through events and [`layer_feedback`](Self::layer_feedback).
pub struct StreamerSession {
	own: ClientId,
	options: StreamerOptions,
	config: PeerConfig,
	id: Option<String>,
	ended: bool,
	/// Stopped before the server confirmed the setup: stop once it does.
	stop_when_live: bool,
	viewers: BTreeMap<u16, ViewerSlot>,
	layers: LayerTable,
	feedback: Arc<LayerFeedback>,
	/// When the viewer list was last reported.
	last_viewers_event: Option<Instant>,
}

impl StreamerSession {
	/// Start streaming: queues `setupstream`.
	pub fn start(
		own: ClientId,
		options: StreamerOptions,
		config: PeerConfig,
		out: &mut Outbox,
	) -> Self {
		out.request(Request::Setup(options.setup.clone()));
		let layers = LayerTable::new(&options.layers, setup_bitrate(&options.setup));
		let feedback = Arc::new(LayerFeedback::new());
		feedback.prepare(layers.specs.iter().map(|s| s.id));
		Self {
			own,
			options,
			config,
			id: None,
			ended: false,
			stop_when_live: false,
			viewers: BTreeMap::new(),
			layers,
			feedback,
			last_viewers_event: None,
		}
	}

	/// The stream id, once the server confirmed the setup.
	pub fn id(&self) -> Option<&str> {
		self.id.as_deref()
	}

	pub fn is_ended(&self) -> bool {
		self.ended
	}

	/// Ended and nothing left to send.
	fn is_finished(&self) -> bool {
		self.ended && !self.stop_when_live
	}

	/// The layers of the stream (one layer 0 without simulcast).
	pub fn layers(&self) -> &[LayerSpec] {
		&self.layers.specs
	}

	/// Keyframe requests and bitrate targets per layer, for the encoders.
	pub fn layer_feedback(&self) -> &Arc<LayerFeedback> {
		&self.feedback
	}

	/// The bitrate (bit/s) the estimates of `layer`'s viewers allow.
	pub fn layer_bitrate(&self, layer: LayerId) -> Option<u64> {
		self.feedback.bitrate(layer)
	}

	pub fn viewers(&self) -> Vec<ViewerInfo> {
		self.viewers
			.iter()
			.map(|(clid, v)| ViewerInfo {
				client: ClientId(*clid),
				state: v.state,
				message: v.message.clone(),
				layer: (v.peer.is_some() && v.rids.is_none()).then_some(v.choice.layer),
				estimate: v.estimate,
				srtp_profile: v.srtp_profile,
			})
			.collect()
	}

	/// Whether `n` is about this stream.
	pub fn wants(&self, n: &StreamNotification) -> bool {
		match (&self.id, n) {
			(None, StreamNotification::Started { info, .. }) => info.streamer == self.own,
			// Responses to join requests are for viewers.
			(Some(_), StreamNotification::JoinResponse { .. }) => false,
			(Some(id), n) => n.stream_id() == id,
			(None, _) => false,
		}
	}

	pub async fn handle(&mut self, n: &StreamNotification, out: &mut Outbox) {
		match n {
			StreamNotification::Started { info, .. } if self.id.is_none() => {
				self.id = Some(info.id.clone());
				if self.stop_when_live {
					self.stop_when_live = false;
					out.request(Request::Stop { id: info.id.clone() });
				} else if !self.ended {
					// Encoders start with a keyframe of every layer.
					for spec in &self.layers.specs {
						self.feedback.request_keyframe(spec.id);
					}
					out.event(StreamEvent::Streamer(StreamerEvent::Live { id: info.id.clone() }));
				}
			}
			_ if self.ended => {}
			StreamNotification::Stopped { .. } => self.end(EndReason::Stopped, out),
			StreamNotification::JoinRequest { viewer, message, remove: false, .. } => {
				self.viewers.insert(viewer.0, ViewerSlot::new(message.clone()));
				if self.options.auto_accept {
					self.offer(*viewer, false, out).await;
				} else {
					out.event(StreamEvent::Streamer(StreamerEvent::Request {
						viewer: *viewer,
						message: message.clone(),
					}));
					self.emit_viewers(out);
				}
			}
			StreamNotification::JoinRequest { viewer, remove: true, .. }
			| StreamNotification::ViewerLeft { viewer, .. } => {
				if self.viewers.remove(&viewer.0).is_some() {
					self.emit_viewers(out);
					self.update_targets(out);
				}
			}
			StreamNotification::Signaling { peer, json, .. } => {
				self.signal(*peer, json, out).await;
			}
			StreamNotification::ViewerJoined { viewer, .. } => {
				debug!(viewer = viewer.0, "viewer joined the stream");
			}
			_ => {}
		}
	}

	async fn signal(&mut self, viewer: ClientId, json: &str, out: &mut Outbox) {
		let signal = match Signal::parse(json) {
			Ok(s) => s,
			Err(e) => {
				warn!(viewer = viewer.0, "bad signalling message: {e}");
				return;
			}
		};
		let Some(slot) = self.viewers.get(&viewer.0) else {
			debug!(viewer = viewer.0, "signal from a client that is not a viewer");
			return;
		};
		match signal {
			Signal::Answer { sdp } => {
				let Some(peer) = &slot.peer else { return };
				if let Err(e) = peer.accept_answer(&sdp).await {
					warn!(viewer = viewer.0, "cannot apply the viewer's answer: {e}");
					self.remove(viewer, LeaveReason::Failed, out);
				}
			}
			Signal::IceCandidate { candidate, .. } => {
				if let Some(peer) = &slot.peer {
					peer.add_remote_candidate(&candidate);
				}
			}
			Signal::Reconnect => {
				if slot.state != ViewerState::Requested {
					self.offer(viewer, true, out).await;
				}
			}
			other => debug!(viewer = viewer.0, "ignoring signal {other:?}"),
		}
	}

	/// The estimate a new viewer starts with.
	fn start_bitrate(&self) -> u64 {
		self.options.start_bitrate.unwrap_or(self.layers.specs[self.layers.top()].bitrate)
	}

	/// What a viewer's connection probes for: enough for the top layer (with
	/// the margin to switch up), or for all its layers with RID simulcast.
	fn desired_bitrate(&self, rids: Option<&[Rid]>) -> u64 {
		match rids {
			Some(rids) => {
				let taken = (0..self.layers.len())
					.filter(|i| self.layers.rids[*i].is_some_and(|r| rids.contains(&r)));
				taken.map(|i| self.layers.cap(i)).fold(0, u64::saturating_add)
			}
			None => {
				let top = u128::from(self.layers.cap(self.layers.top()));
				let with_margin = top * (100 + u128::from(UP_MARGIN_PERCENT)) / 100;
				u64::try_from(with_margin).unwrap_or(u64::MAX)
			}
		}
	}

	/// Create a peer for `viewer` and send our offer: in `respondjoinstreamrequest`,
	/// or as `reconnectOffer` for a viewer that lost its connection.
	async fn offer(&mut self, viewer: ClientId, reconnect: bool, out: &mut Outbox) {
		let Some(id) = self.id.clone() else { return };
		let start = self.start_bitrate();
		let layer = self.layers.specs[self.layers.fitting(start)].id;
		// Offering RID simulcast: the start estimate is for all its layers.
		let rid_offer = self.config.simulcast && self.layers.rid_layers().next().is_some();
		let (start_bitrate, desired_bitrate) = if rid_offer {
			let all: Vec<Rid> =
				self.layers.rid_layers().filter_map(|i| self.layers.rids[i]).collect();
			let desired = self.desired_bitrate(Some(&all));
			(desired, desired)
		} else {
			(start, self.desired_bitrate(None))
		};
		let options = OfferOptions {
			start_bitrate: Some(start_bitrate),
			desired_bitrate: Some(desired_bitrate),
			layers: &self.layers.specs,
		};
		match Peer::offer_with(&self.config, &id, &options).await {
			Ok((peer, sdp)) => {
				if let Some(slot) = self.viewers.get_mut(&viewer.0) {
					slot.peer = Some(peer);
					slot.state = ViewerState::Connecting;
					slot.choice = LayerChoice::new(layer);
					slot.rids = None;
					slot.estimate = None;
					slot.srtp_profile = None;
				}
				out.request(if reconnect {
					Request::Signal { id, peer: viewer, signal: Signal::Offer { sdp, reconnect } }
				} else {
					Request::Respond { id, viewer, offer: Some(sdp), accept: true }
				});
			}
			Err(e) => {
				warn!(viewer = viewer.0, "cannot create a peer connection: {e}");
				self.viewers.remove(&viewer.0);
				out.request(if reconnect {
					Request::RemoveViewer { id, viewer, reason: LeaveReason::Failed }
				} else {
					Request::Respond { id, viewer, offer: None, accept: false }
				});
			}
		}
		self.emit_viewers(out);
		self.update_targets(out);
	}

	/// Accept or deny a pending join request.
	pub async fn respond(
		&mut self,
		viewer: ClientId,
		accept: bool,
		out: &mut Outbox,
	) -> Result<(), SessionError> {
		let id = self.id.clone().ok_or(SessionError::NotLive)?;
		match self.viewers.get(&viewer.0) {
			Some(slot) if slot.state == ViewerState::Requested => {}
			_ => return Err(SessionError::NoRequest(viewer.0)),
		}
		if accept {
			self.offer(viewer, false, out).await;
		} else {
			self.viewers.remove(&viewer.0);
			out.request(Request::Respond { id, viewer, offer: None, accept: false });
			self.emit_viewers(out);
		}
		Ok(())
	}

	/// End a viewer's session (`removeclientfromstream` with reason kicked).
	pub fn kick(&mut self, viewer: ClientId, out: &mut Outbox) -> Result<(), SessionError> {
		if self.id.is_none() {
			return Err(SessionError::NotLive);
		}
		if !self.viewers.contains_key(&viewer.0) {
			return Err(SessionError::UnknownViewer(viewer.0));
		}
		self.remove(viewer, LeaveReason::Kicked, out);
		Ok(())
	}

	fn remove(&mut self, viewer: ClientId, reason: LeaveReason, out: &mut Outbox) {
		self.viewers.remove(&viewer.0);
		if let Some(id) = &self.id {
			out.request(Request::RemoveViewer { id: id.clone(), viewer, reason });
		}
		self.emit_viewers(out);
		self.update_targets(out);
	}

	/// End the stream (`stopstream`).
	pub fn stop(&mut self, out: &mut Outbox) {
		if self.ended {
			return;
		}
		match &self.id {
			Some(id) => out.request(Request::Stop { id: id.clone() }),
			None => self.stop_when_live = true,
		}
		self.end(EndReason::Local, out);
	}

	fn end(&mut self, reason: EndReason, out: &mut Outbox) {
		self.ended = true;
		self.viewers.clear();
		out.event(StreamEvent::Streamer(StreamerEvent::Ended(reason)));
	}

	/// Use a new layer list: viewers move to the layers their estimates
	/// allow (at the next keyframe of their new layer, which is requested),
	/// connections probe for the new bitrates. Peers with RID simulcast keep
	/// the RIDs their answer took.
	pub fn set_layers(&mut self, layers: Vec<LayerSpec>, out: &mut Outbox) {
		let old: Vec<LayerId> = self.layers.specs.iter().map(|s| s.id).collect();
		self.layers = LayerTable::new(&layers, setup_bitrate(&self.options.setup));
		self.options.layers = layers;
		self.feedback.prepare(self.layers.specs.iter().map(|s| s.id));
		for id in old {
			if self.layers.index(id).is_none() {
				self.feedback.set_bitrate(id, None);
			}
		}
		let start = self.start_bitrate();
		let mut keyframes = LayerSet::new();
		for slot in self.viewers.values_mut() {
			if slot.peer.is_none() {
				slot.choice = LayerChoice::new(self.layers.specs[self.layers.fitting(start)].id);
				continue;
			}
			if slot.rids.is_none() {
				let fitting = self.layers.fitting(slot.estimate.unwrap_or(start));
				slot.choice.up_since = None;
				if let Some(layer) = slot.choice.switch_to(self.layers.specs[fitting].id) {
					keyframes.insert(layer);
				}
				if slot.connected() {
					keyframes.insert(slot.choice.target());
				}
			}
		}
		let desired: Vec<(u16, u64)> = self
			.viewers
			.iter()
			.filter(|(_, v)| v.peer.is_some())
			.map(|(c, v)| (*c, self.desired_bitrate(v.rids.as_deref())))
			.collect();
		for (client, bitrate) in desired {
			if let Some(peer) = self.viewers.get(&client).and_then(|v| v.peer.as_ref()) {
				peer.set_desired_bitrate(bitrate);
			}
		}
		for layer in keyframes.iter() {
			self.keyframe_request(layer, out);
		}
		self.emit_viewers(out);
		self.update_targets(out);
	}

	/// Send an encoded frame: audio to every connected viewer, video to the
	/// viewers of its layer (with its RID to peers with RID simulcast).
	pub fn write_frame(&mut self, frame: &EncodedFrame, out: &mut Outbox) {
		let (kind, time, layer) = (frame.kind, frame.time, frame.layer);
		let rid = match kind {
			MediaKind::Video => self.layers.index(layer).and_then(|i| self.layers.rids[i]),
			MediaKind::Audio => None,
		};
		let mut switched = false;
		for slot in self.viewers.values_mut() {
			let (ViewerState::Connected, Some(peer)) = (slot.state, &slot.peer) else {
				continue;
			};
			if kind == MediaKind::Audio {
				peer.write(kind, time, frame.data.clone());
				continue;
			}
			match &slot.rids {
				Some(rids) => {
					if let Some(rid) = rid
						&& rids.contains(&rid)
					{
						peer.write_rid(kind, time, frame.data.clone(), Some(rid));
					}
				}
				None => {
					let (send, now_on_layer) = slot.choice.frame(layer, frame.keyframe);
					switched |= now_on_layer;
					if send {
						peer.write(kind, time, frame.data.clone());
					}
				}
			}
		}
		if switched {
			self.emit_viewers(out);
			self.update_targets(out);
		}
	}

	/// Whether any viewer is connected (frames are dropped otherwise).
	pub fn has_connected_viewers(&self) -> bool {
		self.viewers.values().any(ViewerSlot::connected)
	}

	/// A command of this session failed on the server.
	pub fn request_failed(&mut self, request: &Request, error: &str, out: &mut Outbox) {
		match request {
			Request::Setup(_) if self.id.is_none() => {
				self.stop_when_live = false;
				if !self.ended {
					self.end(EndReason::Failed(format!("setupstream: {error}")), out);
				}
			}
			Request::Respond { viewer, accept: true, .. } => {
				if self.viewers.remove(&viewer.0).is_some() {
					self.emit_viewers(out);
					self.update_targets(out);
				}
			}
			other => debug!(?other, "stream command failed: {error}"),
		}
	}

	/// Handle ready peer events; `Pending` if there were none.
	pub fn poll_peers(&mut self, cx: &mut Context<'_>, out: &mut Outbox) -> Poll<()> {
		let mut events = Vec::new();
		for (clid, slot) in &mut self.viewers {
			let Some(peer) = &mut slot.peer else { continue };
			for _ in 0..EVENTS_PER_POLL {
				match peer.poll_event(cx) {
					Poll::Ready(Some(e)) => events.push((ClientId(*clid), e)),
					Poll::Ready(None) => {
						events.push((ClientId(*clid), PeerEvent::Closed));
						break;
					}
					Poll::Pending => break,
				}
			}
		}
		if events.is_empty() {
			return Poll::Pending;
		}
		for (viewer, event) in events {
			self.peer_event(viewer, event, out);
		}
		Poll::Ready(())
	}

	fn peer_event(&mut self, viewer: ClientId, event: PeerEvent, out: &mut Outbox) {
		let Some(id) = self.id.clone() else { return };
		let Some(slot) = self.viewers.get_mut(&viewer.0) else { return };
		match event {
			PeerEvent::Connected => {
				slot.state = ViewerState::Connected;
				slot.srtp_profile = slot.peer.as_ref().and_then(Peer::srtp_profile);
				if let Some(profile) = slot.srtp_profile {
					debug!(viewer = viewer.0, %profile, "viewer connected");
				}
				let layers = self.viewer_layers(viewer.0);
				for layer in layers.iter() {
					self.keyframe_request(layer, out);
				}
				self.emit_viewers(out);
				self.update_targets(out);
			}
			PeerEvent::LocalCandidate { candidate, mid } => out.request(Request::Signal {
				id,
				peer: viewer,
				signal: Signal::IceCandidate { candidate, mid, mline_index: Some(0) },
			}),
			PeerEvent::KeyframeRequest => {
				let layers = self.viewer_layers(viewer.0);
				for layer in layers.iter() {
					self.keyframe_request(layer, out);
				}
			}
			PeerEvent::LayerKeyframeRequest(rid) => {
				if let Some(i) = self.layers.rid_index(rid) {
					self.keyframe_request(self.layers.specs[i].id, out);
				}
			}
			PeerEvent::BitrateEstimate(bitrate) => {
				self.viewer_estimate(viewer, bitrate, Instant::now(), out);
			}
			PeerEvent::Simulcast(rids) => {
				debug!(viewer = viewer.0, ?rids, "viewer takes RID simulcast");
				let desired = self.desired_bitrate(Some(&rids));
				if let Some(slot) = self.viewers.get_mut(&viewer.0) {
					if let Some(peer) = &slot.peer {
						peer.set_desired_bitrate(desired);
					}
					slot.rids = Some(rids);
				}
			}
			PeerEvent::Media(_) | PeerEvent::LayerMedia { .. } => {}
			PeerEvent::Closed => {
				// The viewer may ask for a new offer (`reconnect`); it stays
				// until it leaves.
				if slot.peer.take().is_some() {
					debug!(viewer = viewer.0, "viewer connection closed");
					slot.state = ViewerState::Connecting;
					self.emit_viewers(out);
					self.update_targets(out);
				}
			}
		}
	}

	/// The layers `viewer` receives.
	fn viewer_layers(&self, viewer: u16) -> LayerSet {
		let Some(slot) = self.viewers.get(&viewer) else { return LayerSet::new() };
		match &slot.rids {
			Some(rids) => (0..self.layers.len())
				.filter(|i| self.layers.rids[*i].is_some_and(|r| rids.contains(&r)))
				.map(|i| self.layers.specs[i].id)
				.collect(),
			None => [slot.choice.layer].into_iter().collect(),
		}
	}

	/// A new bandwidth estimate of `viewer`'s connection.
	fn viewer_estimate(&mut self, viewer: ClientId, bitrate: u64, now: Instant, out: &mut Outbox) {
		let Some(slot) = self.viewers.get_mut(&viewer.0) else { return };
		slot.estimate = Some(bitrate);
		let (keyframe, affected) = match &slot.rids {
			Some(_) => (None, None),
			None => {
				let keyframe = if self.layers.len() > 1 {
					slot.choice.estimate(&self.layers, bitrate, now)
				} else {
					None
				};
				(keyframe, self.layers.index(slot.choice.layer))
			}
		};
		if let Some(layer) = keyframe {
			debug!(viewer = viewer.0, bitrate, layer, "viewer switches layers");
			self.keyframe_request(layer, out);
			self.emit_viewers(out);
		} else if self.last_viewers_event.is_none_or(|t| now.duration_since(t) >= STATS_INTERVAL) {
			self.emit_viewers(out);
		}
		match affected {
			Some(i) => self.update_target(i, out),
			None => self.update_targets(out),
		}
	}

	/// A viewer of `layer` needs a keyframe; requests of a layer closer
	/// together than [`KEYFRAME_INTERVAL`] are merged.
	fn keyframe_request(&mut self, layer: LayerId, out: &mut Outbox) {
		let Some(i) = self.layers.index(layer) else { return };
		let now = Instant::now();
		let last = &mut self.layers.last_keyframe_request[i];
		if last.is_some_and(|t| now.duration_since(t) < KEYFRAME_INTERVAL) {
			return;
		}
		*last = Some(now);
		self.feedback.request_keyframe(layer);
		out.event(StreamEvent::Streamer(StreamerEvent::KeyframeRequest { layer }));
	}

	/// The bitrate target of layer `index`: the lowest estimate (or RID
	/// share) of its connected viewers, at least [`MIN_VIDEO_BITRATE`], at
	/// most the layer's `max_bitrate`.
	fn layer_target(&self, index: usize) -> Option<u64> {
		let id = self.layers.specs[index].id;
		let lowest = self
			.viewers
			.values()
			.filter(|v| v.connected())
			.filter_map(|v| {
				let estimate = v.estimate?;
				match &v.rids {
					Some(rids) => self.layers.rid_share(rids, estimate, index),
					None => (v.choice.layer == id).then_some(estimate),
				}
			})
			.min()?;
		let max = self.layers.specs[index].max_bitrate.unwrap_or(u64::MAX);
		Some(lowest.max(MIN_VIDEO_BITRATE).min(max))
	}

	fn update_target(&mut self, index: usize, out: &mut Outbox) {
		let target = self.layer_target(index);
		let layer = self.layers.specs[index].id;
		self.feedback.set_bitrate(layer, target);
		let reported = &mut self.layers.reported[index];
		match (target, *reported) {
			(None, _) => *reported = None,
			(Some(new), Some(old)) if new.abs_diff(old) <= old / BITRATE_REPORT_STEP => {}
			(Some(bitrate), _) => {
				*reported = Some(bitrate);
				out.event(StreamEvent::Streamer(StreamerEvent::LayerBitrate { layer, bitrate }));
			}
		}
	}

	fn update_targets(&mut self, out: &mut Outbox) {
		for i in 0..self.layers.len() {
			self.update_target(i, out);
		}
	}

	fn emit_viewers(&mut self, out: &mut Outbox) {
		self.last_viewers_event = Some(Instant::now());
		out.event(StreamEvent::Streamer(StreamerEvent::Viewers(self.viewers())));
	}
}

/// The video bitrate of a stream setup in bit/s.
fn setup_bitrate(setup: &StreamSetup) -> u64 {
	u64::from(setup.bitrate) * 1000
}

/// State of a stream we watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchState {
	/// Join request sent.
	Requested,
	/// Accepted; connecting to the streamer.
	Connecting,
	Connected,
	Ended,
}

/// Watching one stream: `joinstreamrequest`, answer the streamer's offer, receive media.
pub struct ViewerSession {
	id: String,
	streamer: ClientId,
	own: ClientId,
	config: PeerConfig,
	state: WatchState,
	peer: Option<Peer>,
	reconnects: u32,
	/// The RID simulcast layer we play, if the streamer sends several.
	layer_rid: Option<Rid>,
}

impl ViewerSession {
	/// Ask `streamer` to let us watch stream `id`: queues `joinstreamrequest`.
	pub fn join(
		own: ClientId,
		id: &str,
		streamer: ClientId,
		message: &str,
		config: PeerConfig,
		out: &mut Outbox,
	) -> Self {
		out.request(Request::Join { id: id.to_owned(), streamer, message: message.to_owned() });
		Self {
			id: id.to_owned(),
			streamer,
			own,
			config,
			state: WatchState::Requested,
			peer: None,
			reconnects: 0,
			layer_rid: None,
		}
	}

	pub fn id(&self) -> &str {
		&self.id
	}

	pub fn streamer(&self) -> ClientId {
		self.streamer
	}

	pub fn state(&self) -> WatchState {
		self.state
	}

	/// Whether `n` is for this viewing session.
	pub fn wants(&self, n: &StreamNotification) -> bool {
		if n.stream_id() != self.id {
			return false;
		}
		match n {
			StreamNotification::JoinRequest { .. } => false,
			StreamNotification::Signaling { peer, .. } => *peer == self.streamer,
			StreamNotification::ViewerJoined { viewer, .. }
			| StreamNotification::ViewerLeft { viewer, .. } => *viewer == self.own,
			_ => true,
		}
	}

	pub async fn handle(&mut self, n: &StreamNotification, out: &mut Outbox) {
		if self.state == WatchState::Ended {
			return;
		}
		match n {
			StreamNotification::JoinResponse { accepted: false, .. } => {
				self.end(EndReason::Denied, out);
			}
			StreamNotification::JoinResponse { accepted: true, offer, .. } => {
				self.state = WatchState::Connecting;
				self.event(WatchEvent::Accepted, out);
				// Without an offer in the response, it follows as an `offer` signal.
				if let Some(offer) = offer {
					self.answer(offer, out).await;
				}
			}
			StreamNotification::Signaling { json, .. } => match Signal::parse(json) {
				Ok(Signal::Offer { sdp, .. }) => self.answer(&sdp, out).await,
				Ok(Signal::IceCandidate { candidate, .. }) => {
					if let Some(peer) = &self.peer {
						peer.add_remote_candidate(&candidate);
					}
				}
				Ok(other) => debug!(stream = self.id, "ignoring signal {other:?}"),
				Err(e) => warn!(stream = self.id, "bad signalling message: {e}"),
			},
			StreamNotification::Stopped { .. } => self.end(EndReason::Stopped, out),
			StreamNotification::ViewerLeft { reason, .. } => {
				self.end(EndReason::Removed(*reason), out);
			}
			_ => {}
		}
	}

	/// Answer an offer (the first one or a `reconnectOffer`) with a new peer.
	async fn answer(&mut self, offer: &str, out: &mut Outbox) {
		match Peer::answer(&self.config, offer).await {
			Ok((peer, sdp)) => {
				self.peer = Some(peer);
				self.state = WatchState::Connecting;
				self.signal(Signal::Answer { sdp }, out);
			}
			Err(e) => self.fail(format!("cannot answer the streamer's offer: {e}"), out),
		}
	}

	fn signal(&self, signal: Signal, out: &mut Outbox) {
		out.request(Request::Signal { id: self.id.clone(), peer: self.streamer, signal });
	}

	/// The peer configuration for connections made from now on.
	pub fn set_config(&mut self, config: PeerConfig) {
		self.config = config;
	}

	/// Stop watching (or withdraw the request).
	pub fn leave(&mut self, out: &mut Outbox) {
		if self.state == WatchState::Ended {
			return;
		}
		out.request(Request::Leave { id: self.id.clone(), streamer: self.streamer });
		self.end(EndReason::Local, out);
	}

	/// Ask the streamer for a keyframe (RTCP PLI).
	pub fn request_keyframe(&self) {
		if let Some(peer) = &self.peer {
			peer.request_keyframe();
		}
	}

	/// Drop the connection and ask the streamer for a new offer (`reconnect`).
	pub fn reconnect(&mut self, out: &mut Outbox) {
		if matches!(self.state, WatchState::Connecting | WatchState::Connected) {
			self.peer = None;
			self.state = WatchState::Connecting;
			self.signal(Signal::Reconnect, out);
		}
	}

	fn fail(&mut self, error: String, out: &mut Outbox) {
		warn!(stream = self.id, "{error}");
		out.request(Request::Leave { id: self.id.clone(), streamer: self.streamer });
		self.end(EndReason::Failed(error), out);
	}

	fn end(&mut self, reason: EndReason, out: &mut Outbox) {
		self.state = WatchState::Ended;
		self.peer = None;
		self.event(WatchEvent::Ended(reason), out);
	}

	fn event(&self, event: WatchEvent, out: &mut Outbox) {
		out.event(StreamEvent::Watch { id: self.id.clone(), event });
	}

	/// A command of this session failed on the server.
	pub fn request_failed(&mut self, request: &Request, error: &str, out: &mut Outbox) {
		match request {
			Request::Join { .. } if self.state != WatchState::Ended => {
				self.end(EndReason::Failed(format!("joinstreamrequest: {error}")), out);
			}
			other => debug!(?other, "stream command failed: {error}"),
		}
	}

	/// Handle ready peer events; `Pending` if there were none.
	pub fn poll_peer(&mut self, cx: &mut Context<'_>, out: &mut Outbox) -> Poll<()> {
		let mut events = Vec::new();
		if let Some(peer) = &mut self.peer {
			for _ in 0..EVENTS_PER_POLL {
				match peer.poll_event(cx) {
					Poll::Ready(Some(e)) => events.push(e),
					Poll::Ready(None) => {
						events.push(PeerEvent::Closed);
						break;
					}
					Poll::Pending => break,
				}
			}
		}
		if events.is_empty() {
			return Poll::Pending;
		}
		for event in events {
			self.peer_event(event, out);
		}
		Poll::Ready(())
	}

	fn peer_event(&mut self, event: PeerEvent, out: &mut Outbox) {
		if self.state == WatchState::Ended {
			return;
		}
		match event {
			PeerEvent::Connected => {
				self.state = WatchState::Connected;
				self.reconnects = 0;
				self.event(WatchEvent::Connected, out);
			}
			PeerEvent::LocalCandidate { candidate, mid } => {
				self.signal(Signal::IceCandidate { candidate, mid, mline_index: Some(0) }, out);
			}
			PeerEvent::Media(frame) => self.event(WatchEvent::Frame(frame), out),
			// A streamer offered RID simulcast: play one layer, the first that arrives.
			PeerEvent::LayerMedia { rid, frame } => {
				if *self.layer_rid.get_or_insert(rid) == rid {
					self.event(WatchEvent::Frame(frame), out);
				}
			}
			PeerEvent::KeyframeRequest
			| PeerEvent::LayerKeyframeRequest(_)
			| PeerEvent::BitrateEstimate(_)
			| PeerEvent::Simulcast(_) => {}
			PeerEvent::Closed => {
				if self.peer.take().is_none() {
					return;
				}
				if self.reconnects < MAX_RECONNECTS {
					self.reconnects += 1;
					debug!(
						stream = self.id,
						attempt = self.reconnects,
						"connection lost, reconnecting"
					);
					self.state = WatchState::Connecting;
					self.signal(Signal::Reconnect, out);
				} else {
					self.fail("connection to the streamer lost".into(), out);
				}
			}
		}
	}
}

/// All stream sessions of one server connection: the directory of streams in
/// our channel, our own stream, and the streams we watch.
pub struct Streams {
	own: ClientId,
	config: PeerConfig,
	directory: StreamDirectory,
	discovery: Discovery,
	streamer: Option<StreamerSession>,
	viewers: BTreeMap<String, ViewerSession>,
	out: Outbox,
}

impl Streams {
	/// `own` is our client id on the connection; `config` is used for every peer.
	pub fn new(own: ClientId, config: PeerConfig) -> Self {
		Self {
			own,
			config,
			directory: StreamDirectory::default(),
			discovery: Discovery::new(own),
			streamer: None,
			viewers: BTreeMap::new(),
			out: Outbox::default(),
		}
	}

	pub fn own_client(&self) -> ClientId {
		self.own
	}

	pub fn directory(&self) -> &StreamDirectory {
		&self.directory
	}

	/// The clients' channels as of [`Self::update_clients`].
	pub fn discovery(&self) -> &Discovery {
		&self.discovery
	}

	/// Our stream, while it has not ended.
	pub fn streamer(&self) -> Option<&StreamerSession> {
		self.streamer.as_ref().filter(|s| !s.is_ended())
	}

	pub fn watching(&self) -> impl Iterator<Item = &ViewerSession> {
		self.viewers.values()
	}

	/// The next request or event; call until `None` after every input.
	pub fn poll_output(&mut self) -> Option<Output> {
		self.out.pop()
	}

	/// Start our stream (`setupstream`).
	pub fn start(&mut self, options: StreamerOptions) -> Result<(), SessionError> {
		// Also while a stream stopped before it went live waits for its id.
		if self.streamer.is_some() {
			return Err(SessionError::AlreadyStreaming);
		}
		let session = StreamerSession::start(self.own, options, self.config.clone(), &mut self.out);
		self.streamer = Some(session);
		Ok(())
	}

	/// Stop our stream.
	pub fn stop(&mut self) -> Result<(), SessionError> {
		let streamer = self.streamer.as_mut().filter(|s| !s.is_ended());
		streamer.ok_or(SessionError::NotStreaming)?.stop(&mut self.out);
		self.cleanup();
		Ok(())
	}

	/// Accept or deny a viewer's join request.
	pub async fn respond(&mut self, viewer: ClientId, accept: bool) -> Result<(), SessionError> {
		let streamer = self.streamer.as_mut().filter(|s| !s.is_ended());
		streamer.ok_or(SessionError::NotStreaming)?.respond(viewer, accept, &mut self.out).await
	}

	/// Remove a viewer from our stream.
	pub fn kick(&mut self, viewer: ClientId) -> Result<(), SessionError> {
		let streamer = self.streamer.as_mut().filter(|s| !s.is_ended());
		streamer.ok_or(SessionError::NotStreaming)?.kick(viewer, &mut self.out)
	}

	/// Send an encoded frame of our stream (layer 0) to all connected viewers.
	pub fn write(&mut self, kind: MediaKind, time: MediaTime, data: impl Into<Arc<[u8]>>) {
		let frame = EncodedFrame { kind, time, data: data.into(), layer: 0, keyframe: false };
		self.write_frame(&frame);
	}

	/// Send an encoded frame of our stream: audio to every connected viewer,
	/// video to the viewers of its layer.
	pub fn write_frame(&mut self, frame: &EncodedFrame) {
		if let Some(s) = self.streamer.as_mut().filter(|s| !s.is_ended()) {
			s.write_frame(frame, &mut self.out);
		}
	}

	/// Change the simulcast layers of our stream while it runs (see
	/// [`StreamerSession::set_layers`]).
	pub fn set_layers(&mut self, layers: Vec<crate::LayerSpec>) -> Result<(), SessionError> {
		let streamer = self.streamer.as_mut().filter(|s| !s.is_ended());
		streamer.ok_or(SessionError::NotStreaming)?.set_layers(layers, &mut self.out);
		Ok(())
	}

	/// The peer configuration of new connections.
	pub fn peer_config(&self) -> &PeerConfig {
		&self.config
	}

	/// Use `config` for connections made from now on (our stream's viewers,
	/// streams we watch); existing connections keep theirs.
	pub fn set_peer_config(&mut self, config: PeerConfig) {
		if let Some(s) = &mut self.streamer {
			s.config = config.clone();
		}
		for v in self.viewers.values_mut() {
			v.set_config(config.clone());
		}
		self.config = config;
	}

	/// The SRTP profile order of new connections (a user setting); see
	/// [`PeerConfig::srtp_profiles`].
	pub fn set_srtp_profiles(&mut self, profiles: Vec<crate::SrtpProfile>) {
		let config = PeerConfig { srtp_profiles: profiles, ..self.config.clone() };
		self.set_peer_config(config);
	}

	/// Watch a stream from the directory.
	pub fn watch(&mut self, id: &str, message: &str) -> Result<(), SessionError> {
		let info = self.directory.get(id).ok_or_else(|| SessionError::UnknownStream(id.into()))?;
		let streamer = info.streamer;
		self.watch_from(id, streamer, message)
	}

	/// Watch stream `id` of `streamer`, also if it is not in the directory.
	pub fn watch_from(
		&mut self,
		id: &str,
		streamer: ClientId,
		message: &str,
	) -> Result<(), SessionError> {
		if streamer == self.own || self.streamer().is_some_and(|s| s.id() == Some(id)) {
			return Err(SessionError::OwnStream);
		}
		if self.viewers.get(id).is_some_and(|v| v.state() != WatchState::Ended) {
			return Err(SessionError::AlreadyWatching(id.into()));
		}
		let session = ViewerSession::join(
			self.own,
			id,
			streamer,
			message,
			self.config.clone(),
			&mut self.out,
		);
		self.viewers.insert(id.to_owned(), session);
		Ok(())
	}

	/// Stop watching a stream.
	pub fn leave(&mut self, id: &str) -> Result<(), SessionError> {
		let viewer =
			self.viewers.get_mut(id).ok_or_else(|| SessionError::NotWatching(id.into()))?;
		viewer.leave(&mut self.out);
		self.cleanup();
		Ok(())
	}

	/// Ask the streamer of a watched stream for a keyframe.
	pub fn request_keyframe(&self, id: &str) {
		if let Some(v) = self.viewers.get(id) {
			v.request_keyframe();
		}
	}

	/// Reconnect to a watched stream: the streamer sends a new offer.
	pub fn reconnect(&mut self, id: &str) -> Result<(), SessionError> {
		let viewer =
			self.viewers.get_mut(id).ok_or_else(|| SessionError::NotWatching(id.into()))?;
		viewer.reconnect(&mut self.out);
		Ok(())
	}

	/// Stop streaming and watching, e.g. before disconnecting.
	pub fn close(&mut self) {
		if let Some(s) = &mut self.streamer {
			s.stop(&mut self.out);
		}
		for v in self.viewers.values_mut() {
			v.leave(&mut self.out);
		}
		self.cleanup();
	}

	/// Feed a stream notification from the connection.
	pub async fn handle_notification(&mut self, n: StreamNotification) {
		// Looked up streams of clients that have left our channel since.
		let elsewhere = matches!(&n, StreamNotification::Info(info)
			if !self.discovery.in_our_channel(info.streamer));
		if !elsewhere && self.directory.apply(&n) {
			self.emit_streams();
		}
		if let Some(s) = &mut self.streamer
			&& s.wants(&n)
		{
			s.handle(&n, &mut self.out).await;
		}
		if let Some(v) = self.viewers.get_mut(n.stream_id())
			&& v.wants(&n)
		{
			v.handle(&n, &mut self.out).await;
		}
		self.cleanup();
	}

	/// `client_is_streaming` of a client changed.
	pub fn set_client_streaming(&mut self, client: ClientId, streaming: bool) {
		if self.directory.set_streaming(client, streaming) {
			self.emit_streams();
		}
	}

	/// Drop the streams of streamers `keep` rejects (clients that left or stopped).
	pub fn retain_streamers(&mut self, keep: impl FnMut(ClientId) -> bool) {
		if self.directory.retain_streamers(keep) {
			self.emit_streams();
		}
	}

	/// The clients on the server changed (full list, with ourselves): keeps
	/// the directory to the streams in our channel and looks up the streams
	/// the server did not announce to us (see [`crate::discovery`]). Covers
	/// [`Self::set_client_streaming`] and [`Self::retain_streamers`].
	pub fn update_clients(&mut self, clients: BTreeMap<u16, ClientState>) {
		if self.discovery.update(clients, &mut self.directory, &mut self.out) {
			self.emit_streams();
		}
	}

	/// A stream found by another [`StreamLookup`] (e.g. a gateway's directory).
	pub fn discovered(&mut self, info: StreamInfo) {
		if self.discovery.in_our_channel(info.streamer)
			&& self.directory.apply(&StreamNotification::Info(info))
		{
			self.emit_streams();
		}
	}

	/// Another place to look up unannounced streams, after the server.
	pub fn add_lookup(&mut self, lookup: Box<dyn StreamLookup>) {
		self.discovery.add_lookup(lookup);
	}

	/// A request from [`Output::Request`] failed on the server.
	pub fn request_failed(&mut self, request: &Request, error: &str) {
		let watched = match request {
			Request::Join { id, .. } | Request::Leave { id, .. } | Request::Signal { id, .. } => {
				self.viewers.get_mut(id)
			}
			Request::StreamInfo { .. } => {
				self.discovery.failed(request, error);
				return;
			}
			_ => None,
		};
		if let Some(v) = watched {
			v.request_failed(request, error, &mut self.out);
		} else if let Some(s) = &mut self.streamer {
			s.request_failed(request, error, &mut self.out);
		}
		self.cleanup();
	}

	/// Wait for peer events and handle them. Cancel safe; never returns while
	/// there are no peers.
	pub async fn wait_peers(&mut self) {
		std::future::poll_fn(|cx| self.poll_peers(cx)).await
	}

	/// Handle ready peer events; `Pending` if there were none.
	pub fn poll_peers(&mut self, cx: &mut Context<'_>) -> Poll<()> {
		let mut ready = false;
		if let Some(s) = &mut self.streamer {
			ready |= s.poll_peers(cx, &mut self.out).is_ready();
		}
		for v in self.viewers.values_mut() {
			ready |= v.poll_peer(cx, &mut self.out).is_ready();
		}
		if ready {
			self.cleanup();
			Poll::Ready(())
		} else {
			Poll::Pending
		}
	}

	fn emit_streams(&mut self) {
		self.out.event(StreamEvent::Streams(self.directory.to_vec()));
	}

	/// Forget sessions that ended.
	fn cleanup(&mut self) {
		if self.streamer.as_ref().is_some_and(StreamerSession::is_finished) {
			self.streamer = None;
		}
		self.viewers.retain(|_, v| v.state() != WatchState::Ended);
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use tokio::time::timeout;

	use super::*;
	use crate::proto::StreamKind;
	use crate::{Frequency, SyntheticSource};

	const OWN: ClientId = ClientId(10);
	const OTHER: ClientId = ClientId(20);

	fn text(request: &Request) -> String {
		String::from_utf8(request.to_command().0.content().to_vec()).unwrap()
	}

	fn texts(requests: &[Request]) -> Vec<String> {
		requests.iter().map(text).collect()
	}

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

	fn info(id: &str, streamer: ClientId) -> StreamInfo {
		StreamInfo {
			id: id.into(),
			streamer,
			name: "screen".into(),
			kind: StreamKind::Screen,
			bitrate: 4608,
			viewer_limit: 0,
			audio: true,
		}
	}

	fn started(id: &str, streamer: ClientId, own: bool) -> StreamNotification {
		StreamNotification::Started {
			info: info(id, streamer),
			return_code: own.then(|| "1".to_owned()),
		}
	}

	fn stopped(id: &str) -> StreamNotification {
		StreamNotification::Stopped { id: id.into(), streamer: None, reason: None }
	}

	fn join(viewer: ClientId, remove: bool) -> StreamNotification {
		StreamNotification::JoinRequest { id: "s-1".into(), viewer, message: "hi".into(), remove }
	}

	fn signaling(peer: ClientId, signal: &Signal) -> StreamNotification {
		StreamNotification::Signaling { id: "s-1".into(), peer, json: signal.to_json() }
	}

	fn response(accepted: bool, offer: Option<String>) -> StreamNotification {
		StreamNotification::JoinResponse {
			id: "s-1".into(),
			streamer: Some(OTHER),
			accepted,
			offer,
			message: String::new(),
		}
	}

	/// The last viewer list in `events`.
	fn viewers(events: &[StreamEvent]) -> Option<Vec<(u16, ViewerState)>> {
		events.iter().rev().find_map(|e| match e {
			StreamEvent::Streamer(StreamerEvent::Viewers(v)) => {
				Some(v.iter().map(|v| (v.client.0, v.state)).collect())
			}
			_ => None,
		})
	}

	/// Handle peer events until `f` accepts the collected events.
	async fn until(s: &mut Streams, mut f: impl FnMut(&[StreamEvent]) -> bool) -> Vec<StreamEvent> {
		let mut events = Vec::new();
		timeout(Duration::from_secs(10), async {
			loop {
				events.extend(drain(s).1);
				if f(&events) {
					return;
				}
				s.wait_peers().await;
			}
		})
		.await
		.expect("timed out");
		events
	}

	#[tokio::test]
	async fn streamer_transcript() {
		let config = PeerConfig::loopback();
		let mut s = Streams::new(OWN, config.clone());
		let setup = StreamSetup { name: "t".into(), ..Default::default() };
		s.start(StreamerOptions { setup, auto_accept: false, ..Default::default() }).unwrap();
		assert_eq!(s.start(StreamerOptions::default()), Err(SessionError::AlreadyStreaming));
		let (requests, _) = drain(&mut s);
		assert_eq!(
			texts(&requests),
			[
				"setupstream name=t type=3 bitrate=4608 accessibility=1 mode=1 viewer_limit=0 audio=1"
			]
		);

		// Someone else's stream only goes into the directory.
		s.handle_notification(started("x-1", OTHER, false)).await;
		let (requests, events) = drain(&mut s);
		assert!(requests.is_empty());
		assert!(matches!(&events[..], [StreamEvent::Streams(l)] if l.len() == 1));
		assert_eq!(s.streamer().unwrap().id(), None);

		s.handle_notification(started("s-1", OWN, true)).await;
		let (_, events) = drain(&mut s);
		assert!(matches!(
			&events[..],
			[StreamEvent::Streams(l), StreamEvent::Streamer(StreamerEvent::Live { id })]
				if l.len() == 2 && id == "s-1"
		));

		// A join request waits for our decision.
		s.handle_notification(join(OTHER, false)).await;
		let (requests, events) = drain(&mut s);
		assert!(requests.is_empty());
		assert!(matches!(
			&events[0],
			StreamEvent::Streamer(StreamerEvent::Request { viewer: OTHER, message }) if message == "hi"
		));
		assert_eq!(viewers(&events), Some(vec![(20, ViewerState::Requested)]));
		assert_eq!(s.respond(ClientId(99), true).await, Err(SessionError::NoRequest(99)));

		// Accept: the offer goes into respondjoinstreamrequest.
		s.respond(OTHER, true).await.unwrap();
		let (requests, events) = drain(&mut s);
		let [Request::Respond { id, viewer: OTHER, offer: Some(offer), accept: true }] =
			&requests[..]
		else {
			panic!("{requests:?}")
		};
		assert_eq!(id, "s-1");
		assert!(offer.contains("m=video") && offer.contains("VP8"), "{offer}");
		let command = text(&requests[0]);
		assert!(
			command.starts_with("respondjoinstreamrequest id=s-1 clid=20 msg offer="),
			"{command}"
		);
		assert!(command.ends_with("decision=1"), "{command}");
		assert_eq!(viewers(&events), Some(vec![(20, ViewerState::Connecting)]));

		// The viewer's answer arrives through notifystreamsignaling.
		let (mut viewer_peer, answer) = Peer::answer(&config, offer).await.unwrap();
		s.handle_notification(signaling(OTHER, &Signal::Answer { sdp: answer })).await;
		let events =
			until(&mut s, |e| viewers(e) == Some(vec![(20, ViewerState::Connected)])).await;
		assert!(
			events.iter().any(|e| matches!(
				e,
				StreamEvent::Streamer(StreamerEvent::KeyframeRequest { layer: 0 })
			)),
			"a new viewer needs a keyframe"
		);
		// Frames reach the viewer.
		timeout(Duration::from_secs(5), async {
			loop {
				let time = MediaTime::new(0, Frequency::FORTY_EIGHT_KHZ);
				s.write(MediaKind::Audio, time, &crate::source::OPUS_SILENCE[..]);
				tokio::select! {
					e = viewer_peer.next_event() => if let Some(PeerEvent::Media(_)) = e { break },
					() = tokio::time::sleep(Duration::from_millis(20)) => {}
				}
			}
		})
		.await
		.expect("no media at the viewer");

		// Withdrawal.
		s.handle_notification(join(OTHER, true)).await;
		let (requests, events) = drain(&mut s);
		assert!(requests.is_empty());
		assert_eq!(viewers(&events), Some(vec![]));

		// Deny.
		s.handle_notification(join(ClientId(21), false)).await;
		s.respond(ClientId(21), false).await.unwrap();
		let (requests, _) = drain(&mut s);
		assert_eq!(texts(&requests), ["respondjoinstreamrequest id=s-1 clid=21 msg decision=0"]);

		// Kick.
		s.handle_notification(join(ClientId(22), false)).await;
		s.respond(ClientId(22), true).await.unwrap();
		let _ = drain(&mut s);
		s.kick(ClientId(22)).unwrap();
		assert_eq!(s.kick(ClientId(22)), Err(SessionError::UnknownViewer(22)));
		let (requests, events) = drain(&mut s);
		assert_eq!(texts(&requests), ["removeclientfromstream id=s-1 clid=22 reason=5"]);
		assert_eq!(viewers(&events), Some(vec![]));

		// A viewer that asks to reconnect gets a new offer.
		s.handle_notification(join(ClientId(23), false)).await;
		s.respond(ClientId(23), true).await.unwrap();
		let _ = drain(&mut s);
		s.handle_notification(signaling(ClientId(23), &Signal::Reconnect)).await;
		let (requests, _) = drain(&mut s);
		assert!(matches!(
			&requests[..],
			[Request::Signal {
				peer: ClientId(23),
				signal: Signal::Offer { reconnect: true, .. },
				..
			}]
		));
		assert!(text(&requests[0]).contains("reconnectOffer"));

		// Stop.
		s.stop().unwrap();
		let (requests, events) = drain(&mut s);
		assert_eq!(texts(&requests), ["stopstream id=s-1 reason=1"]);
		assert!(matches!(
			&events[..],
			[StreamEvent::Streamer(StreamerEvent::Ended(EndReason::Local))]
		));
		assert!(s.streamer().is_none());
		assert_eq!(s.stop(), Err(SessionError::NotStreaming));
		// The server tells everyone.
		s.handle_notification(stopped("s-1")).await;
		let (requests, events) = drain(&mut s);
		assert!(requests.is_empty());
		assert!(matches!(&events[..], [StreamEvent::Streams(l)] if l.len() == 1));
	}

	#[tokio::test]
	async fn auto_accept_and_failures() {
		let mut s = Streams::new(OWN, PeerConfig::loopback());
		s.start(StreamerOptions { auto_accept: true, ..Default::default() }).unwrap();
		let (requests, _) = drain(&mut s);
		s.request_failed(&requests[0], "insufficient client permissions (2568)");
		let (_, events) = drain(&mut s);
		assert!(matches!(
			&events[..],
			[StreamEvent::Streamer(StreamerEvent::Ended(EndReason::Failed(m)))] if m.contains("2568")
		));
		assert!(s.streamer().is_none());

		s.start(StreamerOptions { auto_accept: true, ..Default::default() }).unwrap();
		s.handle_notification(started("s-1", OWN, true)).await;
		s.handle_notification(join(OTHER, false)).await;
		let (requests, events) = drain(&mut s);
		assert!(matches!(
			&requests[..],
			[Request::Setup(_), Request::Respond { offer: Some(_), accept: true, .. }]
		));
		assert!(
			!events
				.iter()
				.any(|e| matches!(e, StreamEvent::Streamer(StreamerEvent::Request { .. })))
		);
		// The server refuses the response: the viewer is dropped.
		s.request_failed(&requests[1], "invalid client id");
		let (_, events) = drain(&mut s);
		assert_eq!(viewers(&events), Some(vec![]));
		// The server ends the stream.
		s.handle_notification(stopped("s-1")).await;
		let (_, events) = drain(&mut s);
		assert!(
			events.iter().any(|e| matches!(
				e,
				StreamEvent::Streamer(StreamerEvent::Ended(EndReason::Stopped))
			))
		);
		assert!(s.streamer().is_none());
	}

	#[tokio::test]
	async fn stop_before_live() {
		let mut s = Streams::new(OWN, PeerConfig::loopback());
		s.start(StreamerOptions::default()).unwrap();
		s.stop().unwrap();
		let _ = drain(&mut s);
		// The stream still needs its id to be stopped.
		assert_eq!(s.start(StreamerOptions::default()), Err(SessionError::AlreadyStreaming));
		s.handle_notification(started("s-1", OWN, true)).await;
		let (requests, events) = drain(&mut s);
		assert_eq!(texts(&requests), ["stopstream id=s-1 reason=1"]);
		assert!(!events.iter().any(|e| matches!(e, StreamEvent::Streamer(_))));
		s.start(StreamerOptions::default()).unwrap();
	}

	#[tokio::test]
	async fn viewer_transcript() {
		let config = PeerConfig::loopback();
		let mut s = Streams::new(OWN, config.clone());
		assert_eq!(s.watch("s-1", ""), Err(SessionError::UnknownStream("s-1".into())));
		s.handle_notification(started("s-1", OTHER, false)).await;
		let _ = drain(&mut s);

		// Denied.
		s.watch("s-1", "please").unwrap();
		assert_eq!(s.watch("s-1", ""), Err(SessionError::AlreadyWatching("s-1".into())));
		let (requests, _) = drain(&mut s);
		assert_eq!(texts(&requests), ["joinstreamrequest id=s-1 clid=20 msg=please is_remove=0"]);
		s.handle_notification(response(false, None)).await;
		let (_, events) = drain(&mut s);
		assert!(matches!(
			&events[..],
			[StreamEvent::Watch { event: WatchEvent::Ended(EndReason::Denied), .. }]
		));
		assert_eq!(s.watching().count(), 0);

		// Accepted: our answer goes back through streamsignaling.
		s.watch("s-1", "").unwrap();
		let (mut streamer_peer, offer) = Peer::offer(&config, "s-1").await.unwrap();
		s.handle_notification(response(true, Some(offer))).await;
		let (requests, events) = drain(&mut s);
		assert_eq!(text(&requests[0]), "joinstreamrequest id=s-1 clid=20 msg is_remove=0");
		let Request::Signal { peer: OTHER, signal: Signal::Answer { sdp }, .. } = &requests[1]
		else {
			panic!("{requests:?}")
		};
		assert!(text(&requests[1]).starts_with("streamsignaling id=s-1 clid=20 json={"));
		assert!(matches!(&events[..], [StreamEvent::Watch { event: WatchEvent::Accepted, .. }]));
		streamer_peer.accept_answer(sdp).await.unwrap();

		// Media arrives as frames.
		let events = until(&mut s, |e| {
			e.iter().any(|e| matches!(e, StreamEvent::Watch { event: WatchEvent::Connected, .. }))
		})
		.await;
		assert!(events.iter().all(|e| matches!(e, StreamEvent::Watch { .. })));
		timeout(Duration::from_secs(5), async {
			while !matches!(streamer_peer.next_event().await, Some(PeerEvent::Connected)) {}
		})
		.await
		.unwrap();
		let frame: Arc<[u8]> = SyntheticSource::video_frame(0, 2000).into();
		let video = until(&mut s, |e| {
			streamer_peer.write(MediaKind::Video, MediaTime::from_90khz(0), frame.clone());
			e.iter().any(|e| matches!(e, StreamEvent::Watch { event: WatchEvent::Frame(_), .. }))
		})
		.await;
		assert!(video.iter().any(|e| matches!(
			e,
			StreamEvent::Watch { event: WatchEvent::Frame(f), .. } if f.data.len() == 2000
		)));

		// The streamer's ICE candidates are added; unknown signals are ignored.
		let candidate = Signal::IceCandidate {
			candidate: "candidate:1 1 udp 2130706431 127.0.0.1 9 typ host".into(),
			mid: Some("0".into()),
			mline_index: Some(0),
		};
		let unknown = Signal::Unknown { cmd: "future".into(), args: serde_json::json!({}) };
		for signal in [candidate, unknown] {
			s.handle_notification(signaling(OTHER, &signal)).await;
		}
		assert!(drain(&mut s).0.is_empty());

		// Leave.
		s.leave("s-1").unwrap();
		let (requests, events) = drain(&mut s);
		assert_eq!(texts(&requests), ["joinstreamrequest id=s-1 clid=20 msg is_remove=1"]);
		assert!(matches!(
			&events[..],
			[StreamEvent::Watch { event: WatchEvent::Ended(EndReason::Local), .. }]
		));
		assert_eq!(s.leave("s-1"), Err(SessionError::NotWatching("s-1".into())));

		// Kicked.
		s.watch("s-1", "").unwrap();
		s.handle_notification(StreamNotification::ViewerLeft {
			id: "s-1".into(),
			viewer: OWN,
			reason: Some(LeaveReason::Kicked),
		})
		.await;
		let (_, events) = drain(&mut s);
		assert!(matches!(
			&events[..],
			[StreamEvent::Watch {
				event: WatchEvent::Ended(EndReason::Removed(Some(LeaveReason::Kicked))),
				..
			}]
		));

		// The stream stops.
		s.watch("s-1", "").unwrap();
		s.handle_notification(stopped("s-1")).await;
		let (_, events) = drain(&mut s);
		assert!(events.iter().any(|e| matches!(
			e,
			StreamEvent::Watch { event: WatchEvent::Ended(EndReason::Stopped), .. }
		)));
		assert!(events.iter().any(|e| matches!(e, StreamEvent::Streams(l) if l.is_empty())));
		assert_eq!(s.watch("s-1", ""), Err(SessionError::UnknownStream("s-1".into())));
		assert_eq!(s.watch_from("x", OWN, ""), Err(SessionError::OwnStream));

		// A failed join request ends the session.
		s.watch_from("s-2", OTHER, "").unwrap();
		let (requests, _) = drain(&mut s);
		s.request_failed(&requests[0], "stream not found");
		let (_, events) = drain(&mut s);
		assert!(matches!(
			&events[..],
			[StreamEvent::Watch { event: WatchEvent::Ended(EndReason::Failed(_)), .. }]
		));
	}

	#[test]
	fn directory() {
		let mut d = StreamDirectory::default();
		assert!(d.apply(&started("a", ClientId(1), false)));
		assert!(!d.apply(&started("a", ClientId(1), false)), "unchanged");
		let renamed = StreamInfo { name: "renamed".into(), ..info("a", ClientId(1)) };
		assert!(d.apply(&StreamNotification::Info(renamed)));
		assert_eq!(d.get("a").unwrap().name, "renamed");
		assert!(!d.set_streaming(ClientId(2), true));
		assert_eq!(d.unannounced_streamers().collect::<Vec<_>>(), [ClientId(2)]);
		assert!(d.apply(&started("b", ClientId(2), false)));
		assert_eq!(d.unannounced_streamers().count(), 0);
		assert_eq!(d.by_streamer(ClientId(2)).unwrap().id, "b");
		assert!(d.set_streaming(ClientId(2), false));
		assert!(d.retain_streamers(|c| c != ClientId(1)));
		assert_eq!(d.iter().count(), 0);
		assert!(d.apply(&started("c", ClientId(3), false)));
		assert!(d.apply(&stopped("c")));
		assert!(!d.apply(&join(ClientId(3), false)));
	}

	/// Layers for the simulcast tests: 0 needs 2 Mbit/s, 1 needs 600 kbit/s
	/// (at most 1.2 Mbit/s), 2 is the fallback.
	fn three_layers() -> Vec<LayerSpec> {
		vec![
			LayerSpec { id: 0, min_bitrate: 2_000_000, ..LayerSpec::single(4_000_000) },
			LayerSpec {
				id: 1,
				scale: 0.5,
				min_bitrate: 600_000,
				max_bitrate: Some(1_200_000),
				..LayerSpec::single(1_000_000)
			},
			LayerSpec { id: 2, scale: 0.25, ..LayerSpec::single(300_000) },
		]
	}

	fn streamer_events(out: &mut Outbox) -> Vec<StreamerEvent> {
		let mut events = Vec::new();
		while let Some(o) = out.pop() {
			if let Output::Event(StreamEvent::Streamer(e)) = o {
				events.push(e);
			}
		}
		events
	}

	fn keyframe_layers(events: &[StreamerEvent]) -> Vec<LayerId> {
		let layers = events.iter().filter_map(|e| match e {
			StreamerEvent::KeyframeRequest { layer } => Some(*layer),
			_ => None,
		});
		layers.collect()
	}

	fn bitrates(events: &[StreamerEvent]) -> Vec<(LayerId, u64)> {
		let bitrates = events.iter().filter_map(|e| match e {
			StreamerEvent::LayerBitrate { layer, bitrate } => Some((*layer, *bitrate)),
			_ => None,
		});
		bitrates.collect()
	}

	#[test]
	fn layer_table() {
		// The list order does not matter; the ladder goes by min_bitrate.
		let mut layers = three_layers();
		layers.rotate_left(1);
		layers.push(LayerSpec { id: 1, ..LayerSpec::single(1) });
		let table = LayerTable::new(&layers, 0);
		assert_eq!(table.len(), 3, "duplicate id dropped");
		let ids = |i: &usize| table.specs[*i].id;
		assert_eq!(table.ladder.iter().map(ids).collect::<Vec<_>>(), [0, 1, 2]);
		assert_eq!(ids(&table.fitting(10_000_000)), 0);
		assert_eq!(ids(&table.fitting(2_000_000)), 0);
		assert_eq!(ids(&table.fitting(1_999_999)), 1);
		assert_eq!(ids(&table.fitting(100)), 2);
		assert_eq!(table.cap(table.index(1).unwrap()), 1_200_000);
		// Without layers: one layer 0 at the setup's bitrate.
		let single = LayerTable::new(&[], 4_608_000);
		assert_eq!((single.len(), single.specs[0].id, single.specs[0].bitrate), (1, 0, 4_608_000));
		assert_eq!(single.fitting(0), 0);
	}

	#[test]
	fn rid_shares() {
		let mut layers = three_layers();
		for (l, rid) in layers.iter_mut().zip(["f", "h", "q"]) {
			l.rid = Some(rid.into());
		}
		let table = LayerTable::new(&layers, 0);
		let rids: Vec<Rid> = ["f", "h", "q"].map(Rid::from).to_vec();
		let share = |estimate, id| table.rid_share(&rids, estimate, table.index(id).unwrap());
		// From the bottom up: q gets its 300k, h up to its 1.2M, f the rest.
		assert_eq!(share(5_000_000, 2), Some(300_000));
		assert_eq!(share(5_000_000, 1), Some(1_200_000));
		assert_eq!(share(5_000_000, 0), Some(3_500_000));
		assert_eq!(share(1_000_000, 1), Some(700_000));
		assert_eq!(share(1_000_000, 0), Some(0));
		// A peer that took only f and q.
		let two = [Rid::from("f"), Rid::from("q")];
		assert_eq!(table.rid_share(&two, 1_000_000, table.index(1).unwrap()), None);
		assert_eq!(table.rid_share(&two, 1_000_000, table.index(0).unwrap()), Some(700_000));
	}

	#[test]
	fn layer_choice_hysteresis() {
		let table = LayerTable::new(&three_layers(), 0);
		let t0 = Instant::now();
		let mut c = LayerChoice::new(0);
		// Enough for layer 0: stay.
		assert_eq!(c.estimate(&table, 3_000_000, t0), None);
		// Below its minimum: switch down at once, to the layer that fits.
		assert_eq!(c.estimate(&table, 1_000_000, t0), Some(1));
		assert_eq!((c.layer, c.pending, c.target()), (0, Some(1), 1));
		// Asked again: no new keyframe request.
		assert_eq!(c.estimate(&table, 900_000, t0), None);
		// Until a keyframe of layer 1, layer 0 is sent.
		assert_eq!(c.frame(1, false), (false, false));
		assert_eq!(c.frame(0, false), (true, false));
		assert_eq!(c.frame(0, true), (true, false));
		assert_eq!(c.frame(1, true), (true, true));
		assert_eq!((c.layer, c.pending), (1, None));
		assert_eq!(c.frame(0, true), (false, false));
		assert_eq!(c.frame(1, false), (true, false));
		// Just above layer 0's minimum is not enough to go up (margin) ...
		assert_eq!(c.estimate(&table, 2_300_000, t0), None);
		assert_eq!(c.up_since, None);
		// ... 2.5 Mbit/s is, but only after UP_DELAY.
		assert_eq!(c.estimate(&table, 2_500_000, t0), None);
		assert_eq!(c.estimate(&table, 2_500_000, t0 + UP_DELAY / 2), None);
		// A dip restarts the wait.
		assert_eq!(c.estimate(&table, 2_000_000, t0 + UP_DELAY / 2), None);
		let t1 = t0 + UP_DELAY;
		assert_eq!(c.estimate(&table, 2_500_000, t1), None);
		assert_eq!(c.estimate(&table, 2_500_000, t1 + UP_DELAY), Some(0));
		assert_eq!(c.target(), 0);
		// Falling back before the keyframe cancels the switch without a request.
		assert_eq!(c.estimate(&table, 1_000_000, t1 + UP_DELAY), None);
		assert_eq!((c.layer, c.pending), (1, None));
		// Down to the last layer, and no lower.
		assert_eq!(c.estimate(&table, 100_000, t1), Some(2));
		assert_eq!(c.frame(2, true), (true, true));
		assert_eq!(c.estimate(&table, 1, t1), None);
		// The layer went away (new list): move to what fits.
		let table = LayerTable::new(&three_layers()[..2], 0);
		assert_eq!(c.estimate(&table, 100_000, t1), Some(1));
	}

	/// A streamer with `layers` and connected viewers (without peers) on
	/// the given layers.
	fn streamer_with(layers: Vec<LayerSpec>, viewers: &[(u16, LayerId)]) -> StreamerSession {
		let mut out = Outbox::default();
		let options = StreamerOptions { layers, ..Default::default() };
		let mut s = StreamerSession::start(OWN, options, PeerConfig::loopback(), &mut out);
		s.id = Some("s-1".into());
		for (client, layer) in viewers {
			let mut slot = ViewerSlot::new(String::new());
			slot.state = ViewerState::Connected;
			slot.choice = LayerChoice::new(*layer);
			s.viewers.insert(*client, slot);
		}
		s
	}

	#[test]
	fn layer_bitrates_and_keyframes() {
		let mut s = streamer_with(three_layers(), &[(20, 0), (21, 0), (22, 1)]);
		let mut out = Outbox::default();
		let now = Instant::now();
		let feedback = s.layer_feedback().clone();
		assert_eq!(feedback.bitrate(0), None);

		// The lowest estimate of a layer's viewers, no cap without max_bitrate.
		s.viewer_estimate(ClientId(20), 9_000_000, now, &mut out);
		assert_eq!(bitrates(&streamer_events(&mut out)), [(0, 9_000_000)]);
		s.viewer_estimate(ClientId(21), 3_000_000, now, &mut out);
		assert_eq!(bitrates(&streamer_events(&mut out)), [(0, 3_000_000)]);
		// Small changes update the target, but are not reported.
		s.viewer_estimate(ClientId(21), 3_100_000, now, &mut out);
		assert!(bitrates(&streamer_events(&mut out)).is_empty());
		assert_eq!(feedback.bitrate(0), Some(3_100_000));
		assert_eq!(s.layer_bitrate(0), Some(3_100_000));
		// Layer 1 is capped at its max_bitrate.
		s.viewer_estimate(ClientId(22), 1_500_000, now, &mut out);
		assert_eq!(bitrates(&streamer_events(&mut out)), [(1, 1_200_000)]);
		// A viewer that moves to another layer counts there once it switched.
		s.viewer_estimate(ClientId(21), 1_000_000, now, &mut out);
		let events = streamer_events(&mut out);
		assert_eq!(keyframe_layers(&events), [1], "keyframe of the new layer");
		assert_eq!(bitrates(&events), [(0, 1_000_000)], "still on layer 0");
		assert_eq!(s.viewers()[1].layer, None, "no peer in this test");
		assert_eq!(s.viewers[&21].choice.pending, Some(1));
		s.viewers.get_mut(&21).unwrap().choice.frame(1, true);
		s.update_targets(&mut out);
		assert_eq!(bitrates(&streamer_events(&mut out)), [(0, 9_000_000), (1, 1_000_000)]);
		// Never below MIN_VIDEO_BITRATE.
		s.viewer_estimate(ClientId(22), 10_000, now, &mut out);
		s.viewer_estimate(ClientId(21), 10_000, now, &mut out);
		assert_eq!(feedback.bitrate(1), Some(MIN_VIDEO_BITRATE));
		let _ = streamer_events(&mut out);

		// Keyframe requests go to the viewer's layer, merged per layer.
		let _ = feedback.take_any_keyframe();
		s.peer_event(ClientId(20), PeerEvent::KeyframeRequest, &mut out);
		s.peer_event(ClientId(20), PeerEvent::KeyframeRequest, &mut out);
		s.peer_event(ClientId(22), PeerEvent::KeyframeRequest, &mut out);
		assert_eq!(keyframe_layers(&streamer_events(&mut out)), [0]);
		let mut layers = LayerSet::new();
		feedback.take_keyframes(&mut layers);
		assert_eq!(layers.iter().collect::<Vec<_>>(), [0]);
		// Viewer 22 is still on layer 1 (switching to 2); layer 1 was
		// requested for viewer 21's switch just before.
		s.layers.last_keyframe_request.fill(None);
		s.peer_event(ClientId(22), PeerEvent::KeyframeRequest, &mut out);
		assert_eq!(keyframe_layers(&streamer_events(&mut out)), [1]);

		// Viewers leave: their layers lose their targets.
		s.viewers.clear();
		s.update_targets(&mut out);
		assert_eq!((feedback.bitrate(0), feedback.bitrate(1)), (None, None));
	}

	#[test]
	fn single_layer_never_switches() {
		let mut s = streamer_with(Vec::new(), &[(20, 0)]);
		let mut out = Outbox::default();
		s.viewer_estimate(ClientId(20), 50_000, Instant::now(), &mut out);
		let events = streamer_events(&mut out);
		assert!(keyframe_layers(&events).is_empty());
		assert_eq!(bitrates(&events), [(0, 50_000)]);
		assert_eq!(s.viewers[&20].choice, LayerChoice::new(0));
		assert_eq!(s.layers(), [LayerSpec::single(4_608_000)]);
	}

	#[tokio::test]
	async fn new_layer_list_reassigns_viewers() {
		let mut s = streamer_with(three_layers(), &[(20, 0), (21, 2), (22, 0)]);
		// 20 and 21 have connections, 22 none yet.
		for client in [20, 21] {
			let (peer, _) = Peer::offer(&PeerConfig::loopback(), "s-1").await.unwrap();
			s.viewers.get_mut(&client).unwrap().peer = Some(peer);
		}
		s.viewers.get_mut(&22).unwrap().state = ViewerState::Requested;
		let mut out = Outbox::default();
		let now = Instant::now();
		s.viewer_estimate(ClientId(20), 2_500_000, now, &mut out);
		s.viewer_estimate(ClientId(21), 250_000, now, &mut out);
		let _ = streamer_events(&mut out);
		assert_eq!(s.layer_feedback().bitrate(2), Some(250_000));

		// Layer 2 is gone, layer 1 needs less now, layer 5 is new.
		let mut layers = three_layers();
		layers.truncate(2);
		layers[1].min_bitrate = 200_000;
		layers.push(LayerSpec { id: 5, ..LayerSpec::single(100_000) });
		s.set_layers(layers, &mut out);
		let events = streamer_events(&mut out);
		assert_eq!(s.layers().len(), 3);
		assert_eq!(s.viewers[&20].choice, LayerChoice::new(0), "still fits");
		assert_eq!((s.viewers[&21].choice.layer, s.viewers[&21].choice.pending), (2, Some(1)));
		assert_eq!(keyframe_layers(&events), [0, 1], "the connected viewers' layers");
		assert_eq!(s.viewers[&22].choice, LayerChoice::new(0), "starts on the top layer");
		assert_eq!(s.layer_feedback().bitrate(2), None, "removed layer");
		assert_eq!(bitrates(&events), [(0, 2_500_000)]);
	}

	/// Two viewers on real loopback connections: one moves to layer 1 and
	/// gets its frames from the next keyframe on, the other stays on layer 0.
	#[tokio::test(flavor = "multi_thread")]
	async fn layer_switching_over_loopback() {
		// Estimates are injected; the connections' own would interfere.
		let config = PeerConfig { bandwidth_estimation: false, ..PeerConfig::loopback() };
		let mut s = Streams::new(OWN, config);
		let layers = three_layers()[..2].to_vec();
		s.start(StreamerOptions { auto_accept: true, layers, ..Default::default() }).unwrap();
		s.handle_notification(started("s-1", OWN, true)).await;
		let mut peers = Vec::new();
		for client in [20, 21] {
			s.handle_notification(join(ClientId(client), false)).await;
			let (requests, _) = drain(&mut s);
			let offer = requests
				.iter()
				.find_map(|r| match r {
					Request::Respond { offer: Some(sdp), .. } => Some(sdp.clone()),
					_ => None,
				})
				.unwrap();
			let (peer, answer) = Peer::answer(&PeerConfig::loopback(), &offer).await.unwrap();
			s.handle_notification(signaling(ClientId(client), &Signal::Answer { sdp: answer }))
				.await;
			peers.push(peer);
		}
		let connected = vec![(20, ViewerState::Connected), (21, ViewerState::Connected)];
		until(&mut s, |e| viewers(e) == Some(connected.clone())).await;
		let info = s.streamer().unwrap().viewers();
		assert_eq!(info.iter().map(|v| v.layer).collect::<Vec<_>>(), [Some(0), Some(0)]);
		assert!(
			info.iter().all(|v| v.srtp_profile == Some(SrtpProfile::Aes128CmSha1_80)),
			"{info:?}"
		);

		let frame = |seq: u64, layer: LayerId, keyframe: bool| EncodedFrame {
			kind: MediaKind::Video,
			time: MediaTime::from_90khz(seq * 3000),
			data: SyntheticSource::layer_frame(seq, layer, 1500).into(),
			layer,
			keyframe,
		};
		let mut seq = 0;
		// Both get layer 0.
		let received = |peer: &mut Peer| {
			let mut frames = Vec::new();
			while let Some(e) = peer.try_next_event() {
				if let PeerEvent::Media(f) = e
					&& f.kind == MediaKind::Video
				{
					frames.push((f.data[22], SyntheticSource::frame_layer(&f.data).unwrap()));
				}
			}
			frames
		};
		timeout(Duration::from_secs(10), async {
			let mut got = [false, false];
			while got != [true, true] {
				seq += 1;
				s.write_frame(&frame(seq, 0, true));
				tokio::time::sleep(Duration::from_millis(50)).await;
				for (i, peer) in peers.iter_mut().enumerate() {
					got[i] |= !received(peer).is_empty();
				}
			}
		})
		.await
		.expect("no video at the viewers");
		tokio::time::sleep(Duration::from_millis(200)).await;
		for peer in &mut peers {
			let _ = received(peer);
		}

		// Viewer 21's estimate drops below layer 0's minimum.
		let streamer = s.streamer.as_mut().unwrap();
		streamer.viewer_estimate(ClientId(21), 1_000_000, Instant::now(), &mut s.out);
		let (_, events) = drain(&mut s);
		assert!(events.iter().any(|e| matches!(
			e,
			StreamEvent::Streamer(StreamerEvent::KeyframeRequest { layer: 1 })
		)));
		let first = seq + 1;
		for (layer, keyframe) in [(1, false), (0, false), (1, true), (0, false), (1, false)] {
			seq += 1;
			s.write_frame(&frame(seq, layer, keyframe));
			tokio::time::sleep(Duration::from_millis(30)).await;
		}
		let seq = |n: u64| (first + n) as u8;
		let mut got = [Vec::new(), Vec::new()];
		timeout(Duration::from_secs(5), async {
			while got[0].len() < 2 || got[1].len() < 3 {
				for (i, peer) in peers.iter_mut().enumerate() {
					got[i].extend(received(peer));
				}
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
		})
		.await
		.unwrap_or_else(|_| panic!("frames: {got:?}"));
		assert_eq!(got[0], [(seq(1), 0), (seq(3), 0)]);
		assert_eq!(got[1], [(seq(1), 0), (seq(2), 1), (seq(4), 1)]);
		let (_, events) = drain(&mut s);
		let info = s.streamer().unwrap().viewers();
		assert_eq!(info.iter().map(|v| v.layer).collect::<Vec<_>>(), [Some(0), Some(1)]);
		assert!(
			events.iter().any(|e| matches!(e, StreamEvent::Streamer(StreamerEvent::Viewers(_))))
		);
		assert_eq!(s.streamer().unwrap().layer_bitrate(1), Some(1_000_000));
	}
}
