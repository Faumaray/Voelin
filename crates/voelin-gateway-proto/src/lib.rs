//! Wire protocol of `tsgw`, the companion gateway that gives app users
//! invisible presence and channel chat without joining voice.
//!
//! Transport: WebSocket, subprotocol [`SUBPROTOCOL`], one JSON [`Envelope`]
//! per text frame. Within version 1 changes are additive only: new message
//! types and optional fields. Unknown fields must be ignored.
//!
//! Authentication proves ownership of a TeamSpeak identity: the gateway sends
//! a nonce in [`ServerMsg::Hello`], the client answers with
//! [`ClientMsg::Auth`], an ECDSA P-256 signature over [`challenge`] made with
//! the identity's private key. The unique id is derived from the public key,
//! exactly as the TeamSpeak server does.

mod auth;
mod messages;

pub use auth::{
	AuthError, MAX_CLOCK_SKEW_SECS, UniqueIds, challenge, identity_level, sign_challenge,
	verify_auth,
};
pub use messages::*;

/// WebSocket subprotocol.
pub const SUBPROTOCOL: &str = "tsgw.v1+json";
/// Protocol version in every envelope.
pub const VERSION: u32 = 1;
