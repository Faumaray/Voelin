//! The SQLite database.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tsclientlib::Identity;

use crate::{Error, Result};

/// Schema migrations; entry `i` upgrades from version `i` to `i + 1`.
const MIGRATIONS: &[&str] = &[r#"
	CREATE TABLE identities (
		id INTEGER PRIMARY KEY,
		name TEXT NOT NULL,
		uid TEXT NOT NULL UNIQUE,
		data TEXT NOT NULL,
		created INTEGER NOT NULL DEFAULT (unixepoch())
	);
	CREATE TABLE bookmarks (
		id INTEGER PRIMARY KEY,
		data TEXT NOT NULL,
		position INTEGER NOT NULL DEFAULT 0
	);
	CREATE TABLE settings (
		key TEXT PRIMARY KEY,
		value TEXT NOT NULL
	);
	CREATE TABLE messages (
		id INTEGER PRIMARY KEY,
		server_uid TEXT NOT NULL,
		target TEXT NOT NULL,
		ts INTEGER NOT NULL,
		author_uid TEXT,
		author_name TEXT NOT NULL,
		via_relay INTEGER NOT NULL DEFAULT 0,
		text TEXT NOT NULL
	);
	CREATE INDEX messages_by_target ON messages (server_uid, target, id);
"#];

/// A stored identity, without its private key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityEntry {
	pub id: i64,
	pub name: String,
	pub uid: String,
	pub level: u8,
}

/// How to reach a server's ServerQuery for invisible presence and relay chat.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryConfig {
	pub transport: QueryTransport,
	pub host: String,
	pub port: u16,
	pub user: String,
	/// Virtual server port to `use` (defaults to the voice port).
	pub server_port: Option<u16>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryTransport {
	/// Raw TCP (TeamSpeak 3 only, port 10011).
	Raw,
	/// SSH (port 10022).
	#[default]
	Ssh,
	/// HTTP WebQuery with an API key (TeamSpeak 6, port 10080/10443). No events.
	Http,
}

/// A saved server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
	/// Assigned by the store.
	#[serde(skip)]
	pub id: i64,
	pub name: String,
	/// host, host:port, or a server nickname.
	pub address: String,
	pub nickname: String,
	/// Identity to connect with (`IdentityEntry::id`).
	pub identity: Option<i64>,
	pub default_channel: Option<String>,
	/// `wss://` URL of a companion gateway, for invisible presence and relay chat.
	pub gateway_url: Option<String>,
	/// Own ServerQuery credentials (the password is kept in [`crate::Secrets`]).
	pub query: Option<QueryConfig>,
	/// Signed client version to present (`voelinctl versions` spec), `None` for the default.
	pub client_version: Option<String>,
}

impl Bookmark {
	/// Secret key of the server password.
	pub fn server_password_key(&self) -> String {
		format!("bookmark/{}/server-password", self.id)
	}
	/// Secret key of the query password or API key.
	pub fn query_password_key(&self) -> String {
		format!("bookmark/{}/query-password", self.id)
	}
}

/// Where a chat message was posted.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChatTarget {
	Server,
	Channel(u64),
	/// Private chat with the client of this unique id.
	Private(String),
}

impl ChatTarget {
	fn key(&self) -> String {
		match self {
			ChatTarget::Server => "server".into(),
			ChatTarget::Channel(cid) => format!("channel/{cid}"),
			ChatTarget::Private(uid) => format!("private/{uid}"),
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMessage {
	/// Assigned by the store; used for paging.
	pub id: i64,
	pub server_uid: String,
	pub target: ChatTarget,
	/// Unix timestamp in seconds.
	pub ts: i64,
	pub author_uid: Option<String>,
	pub author_name: String,
	/// Received through a gateway/query relay instead of our own connection.
	pub via_relay: bool,
	pub text: String,
}

pub struct Store {
	db: Connection,
}

impl Store {
	/// Open or create the database at `path`.
	pub fn open(path: &Path) -> Result<Self> {
		if let Some(dir) = path.parent() {
			std::fs::create_dir_all(dir)?;
		}
		let db = Connection::open(path)?;
		// Identities contain private keys: keep the file private.
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
		}
		db.pragma_update(None, "journal_mode", "WAL")?;
		Self::init(db)
	}

	pub fn open_in_memory() -> Result<Self> {
		Self::init(Connection::open_in_memory()?)
	}

	fn init(db: Connection) -> Result<Self> {
		db.pragma_update(None, "foreign_keys", true)?;
		let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
		for (i, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
			db.execute_batch(migration)?;
			db.pragma_update(None, "user_version", i as i64 + 1)?;
		}
		Ok(Self { db })
	}

	// Identities

	pub fn add_identity(&self, name: &str, identity: &Identity) -> Result<i64> {
		let uid = identity.key().to_pub().get_uid();
		self.db.execute(
			"INSERT INTO identities (name, uid, data) VALUES (?1, ?2, ?3)",
			params![name, uid, serde_json::to_string(identity)?],
		)?;
		Ok(self.db.last_insert_rowid())
	}

	pub fn identities(&self) -> Result<Vec<IdentityEntry>> {
		let mut stmt = self.db.prepare("SELECT id, name, uid, data FROM identities ORDER BY id")?;
		let rows = stmt.query_map([], |r| {
			Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?))
		})?;
		rows.map(|row| {
			let (id, name, uid, data) = row?;
			let identity: Identity = serde_json::from_str(&data)?;
			Ok(IdentityEntry { id, name, uid, level: identity.level() })
		})
		.collect()
	}

	pub fn identity(&self, id: i64) -> Result<Identity> {
		let data: Option<String> = self
			.db
			.query_row("SELECT data FROM identities WHERE id = ?1", [id], |r| r.get(0))
			.optional()?;
		let data = data.ok_or(Error::NotFound("identity", id))?;
		Ok(serde_json::from_str(&data)?)
	}

	/// Store an identity again, e.g. after its security level was improved.
	pub fn update_identity(&self, id: i64, identity: &Identity) -> Result<()> {
		let n = self.db.execute(
			"UPDATE identities SET data = ?2 WHERE id = ?1",
			params![id, serde_json::to_string(identity)?],
		)?;
		if n == 0 { Err(Error::NotFound("identity", id)) } else { Ok(()) }
	}

	pub fn rename_identity(&self, id: i64, name: &str) -> Result<()> {
		let n =
			self.db.execute("UPDATE identities SET name = ?2 WHERE id = ?1", params![id, name])?;
		if n == 0 { Err(Error::NotFound("identity", id)) } else { Ok(()) }
	}

	pub fn delete_identity(&self, id: i64) -> Result<()> {
		self.db.execute("DELETE FROM identities WHERE id = ?1", [id])?;
		Ok(())
	}

	// Bookmarks

	pub fn add_bookmark(&self, bookmark: &Bookmark) -> Result<i64> {
		self.db.execute(
			"INSERT INTO bookmarks (data, position)
			 VALUES (?1, (SELECT COALESCE(MAX(position), -1) + 1 FROM bookmarks))",
			[serde_json::to_string(bookmark)?],
		)?;
		Ok(self.db.last_insert_rowid())
	}

	pub fn update_bookmark(&self, bookmark: &Bookmark) -> Result<()> {
		let n = self.db.execute(
			"UPDATE bookmarks SET data = ?2 WHERE id = ?1",
			params![bookmark.id, serde_json::to_string(bookmark)?],
		)?;
		if n == 0 { Err(Error::NotFound("bookmark", bookmark.id)) } else { Ok(()) }
	}

	pub fn bookmarks(&self) -> Result<Vec<Bookmark>> {
		let mut stmt = self.db.prepare("SELECT id, data FROM bookmarks ORDER BY position, id")?;
		let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
		rows.map(|row| {
			let (id, data) = row?;
			let mut bookmark: Bookmark = serde_json::from_str(&data)?;
			bookmark.id = id;
			Ok(bookmark)
		})
		.collect()
	}

	pub fn delete_bookmark(&self, id: i64) -> Result<()> {
		self.db.execute("DELETE FROM bookmarks WHERE id = ?1", [id])?;
		Ok(())
	}

	// Settings

	pub fn setting<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
		let value: Option<String> = self
			.db
			.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
			.optional()?;
		value.map(|v| serde_json::from_str(&v).map_err(Error::from)).transpose()
	}

	pub fn set_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
		self.db.execute(
			"INSERT INTO settings (key, value) VALUES (?1, ?2)
			 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
			params![key, serde_json::to_string(value)?],
		)?;
		Ok(())
	}

	// Chat cache

	pub fn add_message(&self, msg: &StoredMessage) -> Result<i64> {
		self.db.execute(
			"INSERT INTO messages (server_uid, target, ts, author_uid, author_name, via_relay, text)
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
			params![
				msg.server_uid,
				msg.target.key(),
				msg.ts,
				msg.author_uid,
				msg.author_name,
				msg.via_relay,
				msg.text
			],
		)?;
		Ok(self.db.last_insert_rowid())
	}

	/// Up to `limit` messages older than `before` (message id), oldest first.
	pub fn history(
		&self,
		server_uid: &str,
		target: &ChatTarget,
		before: Option<i64>,
		limit: usize,
	) -> Result<Vec<StoredMessage>> {
		let mut stmt = self.db.prepare(
			"SELECT id, ts, author_uid, author_name, via_relay, text FROM messages
			 WHERE server_uid = ?1 AND target = ?2 AND id < ?3
			 ORDER BY id DESC LIMIT ?4",
		)?;
		let rows = stmt.query_map(
			params![server_uid, target.key(), before.unwrap_or(i64::MAX), limit as i64],
			|r| {
				Ok(StoredMessage {
					id: r.get(0)?,
					server_uid: server_uid.to_string(),
					target: target.clone(),
					ts: r.get(1)?,
					author_uid: r.get(2)?,
					author_name: r.get(3)?,
					via_relay: r.get(4)?,
					text: r.get(5)?,
				})
			},
		)?;
		let mut messages = rows.collect::<rusqlite::Result<Vec<_>>>()?;
		messages.reverse();
		Ok(messages)
	}

	/// Delete cached messages older than `ts` (Unix seconds).
	pub fn prune_messages(&self, ts: i64) -> Result<usize> {
		Ok(self.db.execute("DELETE FROM messages WHERE ts < ?1", [ts])?)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn identities() {
		let store = Store::open_in_memory().unwrap();
		let identity = Identity::create();
		let id = store.add_identity("main", &identity).unwrap();
		let list = store.identities().unwrap();
		assert_eq!(list.len(), 1);
		assert_eq!(list[0].name, "main");
		assert_eq!(list[0].uid, identity.key().to_pub().get_uid());
		assert!(list[0].level >= 8);
		let loaded = store.identity(id).unwrap();
		assert_eq!(loaded.counter(), identity.counter());
		// The same key twice is rejected (uid is unique).
		assert!(store.add_identity("dup", &identity).is_err());
		store.rename_identity(id, "renamed").unwrap();
		assert_eq!(store.identities().unwrap()[0].name, "renamed");
		store.delete_identity(id).unwrap();
		assert!(matches!(store.identity(id), Err(Error::NotFound(..))));
	}

	#[test]
	fn bookmarks_keep_order_and_fields() {
		let store = Store::open_in_memory().unwrap();
		let mut b = Bookmark {
			name: "Home".into(),
			address: "ts.example.org".into(),
			nickname: "me".into(),
			gateway_url: Some("wss://gw.example.org/v1".into()),
			query: Some(QueryConfig {
				transport: QueryTransport::Ssh,
				host: "ts.example.org".into(),
				port: 10022,
				user: "me".into(),
				server_port: None,
			}),
			..Default::default()
		};
		b.id = store.add_bookmark(&b).unwrap();
		let second = Bookmark { name: "Other".into(), ..Default::default() };
		store.add_bookmark(&second).unwrap();
		let list = store.bookmarks().unwrap();
		assert_eq!(list.len(), 2);
		assert_eq!(list[0], b);
		assert_eq!(list[1].name, "Other");

		b.nickname = "renamed".into();
		store.update_bookmark(&b).unwrap();
		assert_eq!(store.bookmarks().unwrap()[0].nickname, "renamed");
		assert_eq!(b.server_password_key(), format!("bookmark/{}/server-password", b.id));
	}

	#[test]
	fn settings() {
		let store = Store::open_in_memory().unwrap();
		assert_eq!(store.setting::<u32>("volume").unwrap(), None);
		store.set_setting("volume", &80u32).unwrap();
		store.set_setting("volume", &90u32).unwrap();
		assert_eq!(store.setting::<u32>("volume").unwrap(), Some(90));
	}

	#[test]
	fn chat_history_pages() {
		let store = Store::open_in_memory().unwrap();
		let target = ChatTarget::Channel(5);
		for i in 0..10 {
			store
				.add_message(&StoredMessage {
					id: 0,
					server_uid: "srv".into(),
					target: target.clone(),
					ts: 1000 + i,
					author_uid: None,
					author_name: "a".into(),
					via_relay: i % 2 == 0,
					text: format!("m{i}"),
				})
				.unwrap();
		}
		let last = store.history("srv", &target, None, 3).unwrap();
		let texts: Vec<_> = last.iter().map(|m| m.text.as_str()).collect();
		assert_eq!(texts, ["m7", "m8", "m9"]);
		let older = store.history("srv", &target, Some(last[0].id), 3).unwrap();
		assert_eq!(older.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["m4", "m5", "m6"]);
		assert!(store.history("srv", &ChatTarget::Server, None, 3).unwrap().is_empty());
		assert_eq!(store.prune_messages(1005).unwrap(), 5);
	}

	#[test]
	fn reopen_keeps_data_and_schema() {
		let dir = std::env::temp_dir().join(format!("voelin-store-{}", std::process::id()));
		let path = dir.join("client.db");
		{
			let store = Store::open(&path).unwrap();
			store.set_setting("k", &"v").unwrap();
		}
		let store = Store::open(&path).unwrap();
		assert_eq!(store.setting::<String>("k").unwrap().as_deref(), Some("v"));
		drop(store);
		std::fs::remove_dir_all(dir).unwrap();
	}
}
