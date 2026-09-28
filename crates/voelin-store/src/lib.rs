//! Local persistence for the client.
//!
//! One SQLite database holds identities, bookmarks, settings and the chat
//! history ([`chat`]). Passwords and query credentials never go into the
//! database; they live in a [`Secrets`] store (the OS keyring with the
//! `keyring` feature).
//!
//! The schema is versioned (`PRAGMA user_version`): opening a database runs
//! the migrations it has not seen, each in one transaction, keeping the data.

pub mod chat;
mod secrets;
mod store;

pub use chat::{
	ChatCursor, ChatTarget, MessageSource, NewMessage, PageQuery, Reaction, RemoteInfo,
	StoredMessage, SyncState, WriteOutcome, Written,
};
#[cfg(feature = "keyring")]
pub use secrets::KeyringSecrets;
pub use secrets::{MemorySecrets, Secrets};
pub use store::{Bookmark, IdentityEntry, QueryConfig, QueryTransport, Store};

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("database: {0}")]
	Db(#[from] rusqlite::Error),
	#[error("serialization: {0}")]
	Json(#[from] serde_json::Error),
	#[error("{0} {1} not found")]
	NotFound(&'static str, i64),
	#[error("secret store: {0}")]
	Secrets(String),
	#[error("io: {0}")]
	Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
