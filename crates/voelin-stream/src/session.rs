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

use str0m::media::{MediaKind, MediaTime};
use tracing::{debug, warn};
use tsclientlib::ClientId;
use tsproto_packets::packets::OutCommand;

use crate::discovery::{ClientState, Discovery, StreamLookup};
use crate::peer::{MediaFrame, Peer, PeerConfig, PeerEvent};
use crate::proto::{self, LeaveReason, StreamInfo, StreamNotification, StreamSetup};
use crate::signal::Signal;
use crate::source::EncodedFrame;

/// Keyframe requests closer together than this are merged into one event.
const KEYFRAME_INTERVAL: Duration = Duration::from_millis(250);
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
	/// The viewers or their states changed (full list).
	Viewers(Vec<ViewerInfo>),
	/// A viewer connected or lost a frame: the encoder should send a keyframe.
	KeyframeRequest,
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
	/// The simulcast layers the source produces. Empty: one layer (0).
	pub layers: Vec<crate::LayerSpec>,
}

struct ViewerSlot {
	state: ViewerState,
	message: String,
	/// `None` while the viewer waits for our decision or after its connection closed.
	peer: Option<Peer>,
}

/// Our own stream: `setupstream`, one peer connection per accepted viewer.
pub struct StreamerSession {
	own: ClientId,
	options: StreamerOptions,
	config: PeerConfig,
	id: Option<String>,
	ended: bool,
	/// Stopped before the server confirmed the setup: stop once it does.
	stop_when_live: bool,
	viewers: BTreeMap<u16, ViewerSlot>,
	last_keyframe_request: Option<Instant>,
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
		Self {
			own,
			options,
			config,
			id: None,
			ended: false,
			stop_when_live: false,
			viewers: BTreeMap::new(),
			last_keyframe_request: None,
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

	pub fn viewers(&self) -> Vec<ViewerInfo> {
		self.viewers
			.iter()
			.map(|(clid, v)| ViewerInfo {
				client: ClientId(*clid),
				state: v.state,
				message: v.message.clone(),
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
					out.event(StreamEvent::Streamer(StreamerEvent::Live { id: info.id.clone() }));
				}
			}
			_ if self.ended => {}
			StreamNotification::Stopped { .. } => self.end(EndReason::Stopped, out),
			StreamNotification::JoinRequest { viewer, message, remove: false, .. } => {
				let slot = ViewerSlot {
					state: ViewerState::Requested,
					message: message.clone(),
					peer: None,
				};
				self.viewers.insert(viewer.0, slot);
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

	/// Create a peer for `viewer` and send our offer: in `respondjoinstreamrequest`,
	/// or as `reconnectOffer` for a viewer that lost its connection.
	async fn offer(&mut self, viewer: ClientId, reconnect: bool, out: &mut Outbox) {
		let Some(id) = self.id.clone() else { return };
		match Peer::offer(&self.config, &id).await {
			Ok((peer, sdp)) => {
				if let Some(slot) = self.viewers.get_mut(&viewer.0) {
					slot.peer = Some(peer);
					slot.state = ViewerState::Connecting;
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

	/// Send an encoded frame to every connected viewer.
	pub fn write(&self, kind: MediaKind, time: MediaTime, data: Arc<[u8]>) {
		for slot in self.viewers.values() {
			if let (ViewerState::Connected, Some(peer)) = (slot.state, &slot.peer) {
				peer.write(kind, time, data.clone());
			}
		}
	}

	/// Whether any viewer is connected (frames are dropped otherwise).
	pub fn has_connected_viewers(&self) -> bool {
		self.viewers.values().any(|v| v.state == ViewerState::Connected)
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
				self.emit_viewers(out);
				self.keyframe_request(out);
			}
			PeerEvent::LocalCandidate { candidate, mid } => out.request(Request::Signal {
				id,
				peer: viewer,
				signal: Signal::IceCandidate { candidate, mid, mline_index: Some(0) },
			}),
			PeerEvent::KeyframeRequest => self.keyframe_request(out),
			PeerEvent::Media(_) => {}
			PeerEvent::Closed => {
				// The viewer may ask for a new offer (`reconnect`); it stays
				// until it leaves.
				if slot.peer.take().is_some() {
					debug!(viewer = viewer.0, "viewer connection closed");
					slot.state = ViewerState::Connecting;
					self.emit_viewers(out);
				}
			}
		}
	}

	fn keyframe_request(&mut self, out: &mut Outbox) {
		let now = Instant::now();
		if self.last_keyframe_request.is_some_and(|t| now.duration_since(t) < KEYFRAME_INTERVAL) {
			return;
		}
		self.last_keyframe_request = Some(now);
		out.event(StreamEvent::Streamer(StreamerEvent::KeyframeRequest));
	}

	fn emit_viewers(&self, out: &mut Outbox) {
		out.event(StreamEvent::Streamer(StreamerEvent::Viewers(self.viewers())));
	}
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
			PeerEvent::KeyframeRequest => {}
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

	/// Send an encoded frame of our stream to all connected viewers.
	pub fn write(&self, kind: MediaKind, time: MediaTime, data: impl Into<Arc<[u8]>>) {
		if let Some(s) = self.streamer() {
			s.write(kind, time, data.into());
		}
	}

	pub fn write_frame(&self, frame: &EncodedFrame) {
		self.write(frame.kind, frame.time, frame.data.clone());
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
			events
				.iter()
				.any(|e| matches!(e, StreamEvent::Streamer(StreamerEvent::KeyframeRequest))),
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
}
