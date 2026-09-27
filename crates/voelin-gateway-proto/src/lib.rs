//! Wire protocol of `tsgw`, the companion gateway that gives app users
//! invisible presence and channel chat without joining voice.
//!
//! Transport: WebSocket, subprotocol [`SUBPROTOCOL`], one JSON [`Envelope`]
//! per text frame. Within version 1 changes are additive only: new message
//! types and optional fields. Unknown fields must be ignored. The gateway
//! answers message types it does not know with [`ErrorCode::UnknownType`]
//! and keeps the connection; it only pushes newer message types to clients
//! that asked for them.
//!
//! Authentication proves ownership of a TeamSpeak identity: the gateway sends
//! a nonce in [`ServerMsg::Hello`], the client answers with
//! [`ClientMsg::Auth`], an ECDSA P-256 signature over [`challenge`] made with
//! the identity's private key. The unique id is derived from the public key,
//! exactly as the TeamSpeak server does.
//!
//! Besides presence and chat, gateways can offer pins, reactions, topics,
//! scheduled events, a directory of running streams, an activity feed and
//! runtime administration; [`ServerMsg::Hello`] lists what is enabled
//! ([`feature`]). The `client` feature adds a typed client ([`client`]).

mod auth;
#[cfg(feature = "client")]
pub mod client;
mod messages;
mod types;

pub use auth::{
	AuthError, MAX_CLOCK_SKEW_SECS, UniqueIds, challenge, identity_level, sign_challenge,
	verify_auth,
};
pub use messages::*;
pub use types::*;

/// WebSocket subprotocol.
pub const SUBPROTOCOL: &str = "tsgw.v1+json";
/// Protocol version in every envelope.
pub const VERSION: u32 = 1;
