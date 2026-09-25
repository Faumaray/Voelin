//! Domain model shared by the client engine, the gateway and the UI.
//!
//! Nothing in here does IO; the types describe what a server offers and what
//! the user sees.

mod chat;
mod presence;
mod server;

pub use chat::{ChatMessage, ChatTarget, relay_text, split_message};
pub use presence::{
	ChannelId, ChannelInfo, ClientId, ClientInfo, Presence, PresenceDelta, PresenceSnapshot,
};
pub use server::{Capabilities, ServerFlavor, ServerVersion};
