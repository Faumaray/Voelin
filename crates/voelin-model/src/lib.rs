//! Domain model shared by the client engine, the gateway and the UI.
//!
//! Nothing in here does IO; the types describe what a server offers and what
//! the user sees.

pub mod badges;
mod chat;
mod link;
mod percent;
mod presence;
mod server;
mod tree;

pub use chat::{ChatMessage, ChatTarget, FileRef, parse_file_links, relay_text, split_message};
pub use link::{Link, ServerLink, channel_path_id, join_channel_path, split_channel_path};
pub use presence::{
	ChannelId, ChannelInfo, ClientId, ClientInfo, GroupId, GroupInfo, GroupNamingMode, GroupType,
	Presence, PresenceDelta, PresenceSnapshot, myts_avatar_url, parse_badges, shown_badges,
};
pub use server::{
	BannerMode, Capabilities, HostMessageMode, ServerDetails, ServerFlavor, ServerVersion,
};
pub use tree::{Spacer, TreeRow, channel_title, order_siblings, parse_spacer, tree_rows};
