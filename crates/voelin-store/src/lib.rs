//! Local persistence for the client.
//!
//! One SQLite database holds identities, bookmarks, settings and a chat
//! cache. Passwords and query credentials never go into the database; they
//! live in a [`Secrets`] store (the OS keyring with the `keyring` feature).

mod secrets;
mod store;

#[cfg(feature = "keyring")]
pub use secrets::KeyringSecrets;
pub use secrets::{MemorySecrets, Secrets};
pub use store::{
	Bookmark, ChatTarget, IdentityEntry, QueryConfig, QueryTransport, Store, StoredMessage,
};

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
