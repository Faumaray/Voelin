//! Domain model shared by the client engine, the gateway and the UI.
//!
//! Nothing in here does IO; the types describe what a server offers and what
//! the user sees.

mod chat;
mod presence;
mod server;
mod tree;

pub use chat::{ChatMessage, ChatTarget, FileRef, parse_file_links, relay_text, split_message};
pub use presence::{
	ChannelId, ChannelInfo, ClientId, ClientInfo, GroupId, GroupInfo, GroupNamingMode, GroupType,
	Presence, PresenceDelta, PresenceSnapshot, parse_badges,
};
pub use server::{
	BannerMode, Capabilities, HostMessageMode, ServerDetails, ServerFlavor, ServerVersion,
};
pub use tree::{Spacer, TreeRow, channel_title, order_siblings, parse_spacer, tree_rows};
