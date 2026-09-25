//! TeamSpeak 6 stream commands and notifications on the client command channel.
//!
//! See `docs/protocol-notes/ts6-streaming.md` for how these were found.

use std::borrow::Cow;
use std::iter;

use tsclientlib::messages::c2s;
use tsclientlib::{ClientId, InMessage};
use tsproto_packets::packets::OutCommand;

/// `type` of a stream (`StreamType` in the server's protobuf schema).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
	Camera,
	Screen,
	Window,
	Other(u8),
}

impl StreamKind {
	pub fn from_u8(v: u8) -> Self {
		match v {
			2 => Self::Camera,
			3 => Self::Screen,
			4 => Self::Window,
			v => Self::Other(v),
		}
	}

	pub fn to_u8(self) -> u8 {
		match self {
			Self::Camera => 2,
			Self::Screen => 3,
			Self::Window => 4,
			Self::Other(v) => v,
		}
	}
}

/// Why a viewer left a stream (`StreamLeaveReason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaveReason {
	None,
	Left,
	Denied,
	Failed,
	Kicked,
	Banned,
	Other(u8),
}

impl LeaveReason {
	pub fn to_u8(self) -> u8 {
		match self {
			Self::None => 1,
			Self::Left => 2,
			Self::Denied => 3,
			Self::Failed => 4,
			Self::Kicked => 5,
			Self::Banned => 6,
			Self::Other(v) => v,
		}
	}

	pub fn from_u8(v: u8) -> Self {
		match v {
			1 => Self::None,
			2 => Self::Left,
			3 => Self::Denied,
			4 => Self::Failed,
			5 => Self::Kicked,
			6 => Self::Banned,
			v => Self::Other(v),
		}
	}
}

/// Parameters of `setupstream`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSetup {
	pub name: String,
	pub kind: StreamKind,
	/// kbit/s; the server caps streams at 10 Mbit/s.
	pub bitrate: u32,
	/// `accessibility`: the official client and ts6-manager send 1.
	pub accessibility: u8,
	/// `mode`: 1 (none) is what clients send today; 2 is P2P, 3 SFU.
	pub mode: u8,
	/// 0 = unlimited.
	pub viewer_limit: u32,
	pub audio: bool,
}

impl Default for StreamSetup {
	fn default() -> Self {
		Self {
			name: String::new(),
			kind: StreamKind::Screen,
			bitrate: 4608,
			accessibility: 1,
			mode: 1,
			viewer_limit: 0,
			audio: true,
		}
	}
}

/// A stream as announced by `notifystreamstarted` / `notifystreaminfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInfo {
	pub id: String,
	pub streamer: ClientId,
	pub name: String,
	pub kind: StreamKind,
	pub bitrate: u32,
	pub viewer_limit: u32,
	pub audio: bool,
}

/// Stream notifications, decoded from [`InMessage`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamNotification {
	/// A stream started in our channel. `return_code` is set on the answer to our own `setupstream`.
	Started {
		info: StreamInfo,
		return_code: Option<String>,
	},
	Info(StreamInfo),
	Stopped {
		id: String,
		streamer: Option<ClientId>,
		reason: Option<LeaveReason>,
	},
	/// (Streamer side) a viewer asks to watch, or withdraws (`remove`).
	JoinRequest {
		id: String,
		viewer: ClientId,
		message: String,
		remove: bool,
	},
	/// (Viewer side) the streamer answered our join request.
	JoinResponse {
		id: String,
		streamer: Option<ClientId>,
		accepted: bool,
		offer: Option<String>,
		message: String,
	},
	/// A `json` signalling message from `peer`.
	Signaling {
		id: String,
		peer: ClientId,
		json: String,
	},
	ViewerJoined {
		id: String,
		viewer: ClientId,
	},
	ViewerLeft {
		id: String,
		viewer: ClientId,
		reason: Option<LeaveReason>,
	},
}

impl StreamNotification {
	/// Decode the stream notifications in `msg`; empty for other messages.
	pub fn from_message(msg: &InMessage) -> Vec<Self> {
		match msg {
			InMessage::StreamStarted(m) => m
				.iter()
				.map(|p| Self::Started {
					info: StreamInfo {
						id: p.stream_id.clone(),
						streamer: p.client_id,
						name: p.stream_name.clone().unwrap_or_default(),
						kind: StreamKind::from_u8(p.stream_type.unwrap_or(3)),
						bitrate: p.bitrate.unwrap_or(0),
						viewer_limit: p.viewer_limit.unwrap_or(0),
						audio: p.audio.unwrap_or(false),
					},
					return_code: p.return_code.clone(),
				})
				.collect(),
			InMessage::StreamInfo(m) => m
				.iter()
				.filter_map(|p| {
					Some(Self::Info(StreamInfo {
						id: p.stream_id.clone(),
						streamer: p.client_id?,
						name: p.stream_name.clone().unwrap_or_default(),
						kind: StreamKind::from_u8(p.stream_type.unwrap_or(3)),
						bitrate: p.bitrate.unwrap_or(0),
						viewer_limit: p.viewer_limit.unwrap_or(0),
						audio: p.audio.unwrap_or(false),
					}))
				})
				.collect(),
			InMessage::StreamStopped(m) => m
				.iter()
				.map(|p| Self::Stopped {
					id: p.stream_id.clone(),
					streamer: p.client_id,
					reason: p.leave_reason.map(LeaveReason::from_u8),
				})
				.collect(),
			InMessage::JoinStreamRequest(m) => m
				.iter()
				.map(|p| Self::JoinRequest {
					id: p.stream_id.clone(),
					viewer: p.client_id,
					message: p.message.clone().unwrap_or_default(),
					remove: p.is_remove.unwrap_or(false),
				})
				.collect(),
			InMessage::RespondJoinStreamRequest(m) => m
				.iter()
				.map(|p| Self::JoinResponse {
					id: p.stream_id.clone(),
					streamer: p.client_id,
					accepted: p.accepted.unwrap_or(false),
					offer: p.offer.clone().filter(|o| !o.is_empty()),
					message: p.message.clone().unwrap_or_default(),
				})
				.collect(),
			InMessage::StreamSignaling(m) => m
				.iter()
				.map(|p| Self::Signaling {
					id: p.stream_id.clone(),
					peer: p.client_id,
					json: p.json.clone(),
				})
				.collect(),
			InMessage::StreamClientJoined(m) => m
				.iter()
				.map(|p| Self::ViewerJoined { id: p.stream_id.clone(), viewer: p.client_id })
				.collect(),
			InMessage::StreamClientLeft(m) => m
				.iter()
				.map(|p| Self::ViewerLeft {
					id: p.stream_id.clone(),
					viewer: p.client_id,
					reason: p.leave_reason.map(LeaveReason::from_u8),
				})
				.collect(),
			_ => Vec::new(),
		}
	}

	/// The stream id this notification is about.
	pub fn stream_id(&self) -> &str {
		match self {
			Self::Started { info, .. } | Self::Info(info) => &info.id,
			Self::Stopped { id, .. }
			| Self::JoinRequest { id, .. }
			| Self::JoinResponse { id, .. }
			| Self::Signaling { id, .. }
			| Self::ViewerJoined { id, .. }
			| Self::ViewerLeft { id, .. } => id,
		}
	}
}

/// `setupstream`: start streaming in our channel. The server answers with
/// `notifystreamstarted` carrying the stream id.
pub fn setup(setup: &StreamSetup) -> OutCommand {
	c2s::OutSetupStreamMessage::new(&mut iter::once(c2s::OutSetupStreamPart {
		stream_name: Cow::Borrowed(&setup.name),
		stream_type: setup.kind.to_u8(),
		bitrate: setup.bitrate,
		access: setup.accessibility,
		stream_mode: setup.mode,
		viewer_limit: setup.viewer_limit,
		audio: setup.audio,
	}))
}

/// `stopstream`: end our stream.
pub fn stop(id: &str) -> OutCommand {
	c2s::OutStopStreamMessage::new(&mut iter::once(c2s::OutStopStreamPart {
		stream_id: Cow::Borrowed(id),
		leave_reason: LeaveReason::None.to_u8(),
	}))
}

/// `joinstreamrequest`: ask `streamer` to let us watch (or withdraw with `remove`).
pub fn join_request(id: &str, streamer: ClientId, message: &str, remove: bool) -> OutCommand {
	c2s::OutJoinStreamRequestRequestMessage::new(&mut iter::once(
		c2s::OutJoinStreamRequestRequestPart {
			stream_id: Cow::Borrowed(id),
			client_id: streamer,
			message: Cow::Borrowed(message),
			is_remove: remove,
		},
	))
}

/// `respondjoinstreamrequest`: accept a viewer with our SDP offer, or deny.
pub fn respond(id: &str, viewer: ClientId, offer: Option<&str>, accept: bool) -> OutCommand {
	c2s::OutRespondJoinStreamRequestRequestMessage::new(&mut iter::once(
		c2s::OutRespondJoinStreamRequestRequestPart {
			stream_id: Cow::Borrowed(id),
			client_id: viewer,
			message: Some(Cow::Borrowed("")),
			offer: offer.map(Cow::Borrowed),
			accepted: accept,
		},
	))
}

/// `streamsignaling`: send a [`Signal`](crate::Signal) JSON to `peer`.
pub fn signaling(id: &str, peer: ClientId, json: &str) -> OutCommand {
	c2s::OutStreamSignalingRequestMessage::new(&mut iter::once(
		c2s::OutStreamSignalingRequestPart {
			stream_id: Cow::Borrowed(id),
			client_id: peer,
			json: Cow::Borrowed(json),
		},
	))
}

/// `removeclientfromstream`: end a viewer's session (`reason` is usually
/// [`LeaveReason::Kicked`]; the viewer gets `notifystreamclientleft` with it).
pub fn remove_viewer(id: &str, viewer: ClientId, reason: LeaveReason) -> OutCommand {
	c2s::OutRemoveClientFromStreamMessage::new(&mut iter::once(
		c2s::OutRemoveClientFromStreamPart {
			stream_id: Cow::Borrowed(id),
			client_id: viewer,
			leave_reason: reason.to_u8(),
		},
	))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn text(cmd: OutCommand) -> String {
		String::from_utf8(cmd.0.content().to_vec()).unwrap()
	}

	#[test]
	fn commands() {
		let s = StreamSetup { name: "my screen".into(), ..Default::default() };
		assert_eq!(
			text(setup(&s)),
			"setupstream name=my\\sscreen type=3 bitrate=4608 accessibility=1 mode=1 viewer_limit=0 \
			 audio=1"
		);
		assert_eq!(
			text(join_request("u-1", ClientId(5), "", false)),
			"joinstreamrequest id=u-1 clid=5 msg is_remove=0"
		);
		assert_eq!(
			text(signaling("u-1", ClientId(5), r#"{"a":1}"#)),
			"streamsignaling id=u-1 clid=5 json={\"a\":1}"
		);
		assert_eq!(text(stop("u-1")), "stopstream id=u-1 reason=1");
		assert_eq!(
			text(remove_viewer("u-1", ClientId(7), LeaveReason::Kicked)),
			"removeclientfromstream id=u-1 clid=7 reason=5"
		);
		let r = text(respond("u-1", ClientId(7), Some("v=0"), true));
		assert!(r.starts_with("respondjoinstreamrequest id=u-1 clid=7"), "{r}");
		assert!(r.contains("offer=v=0") && r.contains("decision=1"), "{r}");
	}
}
