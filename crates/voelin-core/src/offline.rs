//! Offline messages: mail the server keeps for a unique id until its owner
//! reads it (voice connection; TeamSpeak 3 and 6, capability
//! `offline_messages`).
//!
//! # Contract for the UI
//!
//! - [`crate::Command::ListOfflineMessages`] → [`crate::Event::OfflineMessages`]
//!   (our inbox, without texts; empty is an empty list).
//! - [`crate::Command::GetOfflineMessage`] → [`crate::Event::OfflineMessage`]
//!   (with the text; the server marks it read).
//! - [`crate::Command::SendOfflineMessage`], [`crate::Command::DeleteOfflineMessage`],
//!   [`crate::Command::SetOfflineMessageRead`] → [`crate::Event::RequestDone`].
//!
//! Servers do not announce new offline messages while we are connected;
//! list them after connecting and when the user opens the inbox.

use serde::Serialize;

/// An offline message in the inbox list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OfflineMessageInfo {
	pub id: u32,
	/// The sender's unique id.
	pub from_uid: String,
	pub subject: String,
	/// Unix seconds when it was sent.
	pub ts_s: i64,
	pub read: bool,
}

/// An offline message with its text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OfflineMessage {
	pub id: u32,
	pub from_uid: String,
	pub subject: String,
	pub text: String,
	pub ts_s: i64,
}
