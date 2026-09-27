//! TeamSpeak 6 streams (screen sharing): the signalling commands the server
//! relays, the JSON signal messages, and str0m WebRTC peer connections.
//!
//! Only TeamSpeak 6 servers support streams. Media flows peer to peer; the
//! server only forwards `respondjoinstreamrequest` (the streamer's offer) and
//! `streamsignaling` (answer and ICE candidates).
//!
//! [`Streams`] (module [`session`]) ties it together for one connection: the
//! streams in our channel, our own stream with a peer per viewer, and the
//! streams we watch. [`FrameSource`] is where a streamer's encoded frames come from.

pub mod dtls;
pub mod feedback;
pub mod layer;
pub mod peer;
pub mod proto;
pub mod session;
pub mod signal;
pub mod source;
pub mod stun;

pub use dtls::SrtpProfile;
pub use feedback::LayerFeedback;
pub use layer::{LayerId, LayerSet, LayerSpec};
pub use peer::{MediaFrame, OfferOptions, Peer, PeerConfig, PeerError, PeerEvent, VideoCodec};
pub use proto::{LeaveReason, StreamInfo, StreamKind, StreamNotification, StreamSetup};
pub use session::{
	EndReason, Output, Request, SessionError, StreamDirectory, StreamEvent, StreamerEvent,
	StreamerOptions, Streams, ViewerInfo, ViewerState, WatchEvent, WatchState,
};
pub use signal::{Signal, SignalError};
pub use source::{EncodedFrame, FrameSource, SyntheticSource};
pub use str0m::format::Codec;
pub use str0m::media::{Frequency, MediaKind, MediaTime, Rid};
