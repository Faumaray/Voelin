//! TeamSpeak 6 streams (screen sharing): the signalling commands the server
//! relays, the JSON signal messages, and str0m WebRTC peer connections.
//!
//! Only TeamSpeak 6 servers support streams. Media flows peer to peer; the
//! server only forwards `respondjoinstreamrequest` (the streamer's offer) and
//! `streamsignaling` (answer and ICE candidates).

pub mod peer;
pub mod proto;
pub mod signal;
pub mod stun;

pub use peer::{MediaFrame, Peer, PeerConfig, PeerError, PeerEvent, VideoCodec};
pub use proto::{LeaveReason, StreamInfo, StreamKind, StreamNotification, StreamSetup};
pub use signal::{Signal, SignalError};
pub use str0m::format::Codec;
pub use str0m::media::{Frequency, MediaKind, MediaTime};
