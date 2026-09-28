//! The SQLite database.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tsclientlib::Identity;

use crate::{Error, Result};

/// A schema migration: SQL, or code for what SQL alone cannot do.
enum Migration {
	Sql(&'static str),
	Code(fn(&rusqlite::Transaction) -> rusqlite::Result<()>),
}

/// Schema migrations; entry `i` upgrades from version `i` to `i + 1`
/// (`PRAGMA user_version`). Add new ones at the end; never change one that
/// shipped.
const MIGRATIONS: &[Migration] = &[
	Migration::Sql(SCHEMA_1),
	Migration::Code(crate::chat::migrate_2),
	Migration::Sql(crate::contacts::SCHEMA_3),
];

/// Version 1: identities, bookmarks, settings and a first chat cache.
pub(crate) const SCHEMA_1: &str = r#"
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
"#;

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

pub struct Store {
	pub(crate) db: Connection,
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
		// WAL: readers do not block the writer (the settings service writes
		// from its own connection). NORMAL is durable with WAL except for
		// the last transactions on a power loss, and avoids an fsync per write.
		db.pragma_update(None, "journal_mode", "WAL")?;
		db.pragma_update(None, "synchronous", "NORMAL")?;
		Self::init(db)
	}

	pub fn open_in_memory() -> Result<Self> {
		Self::init(Connection::open_in_memory()?)
	}

	fn init(mut db: Connection) -> Result<Self> {
		// Statements run through `prepare_cached` are parsed once per connection.
		db.set_prepared_statement_cache_capacity(64);
		db.pragma_update(None, "foreign_keys", true)?;
		// Each migration with its version bump in one write transaction: a
		// second connection that opens the file at the same time (the
		// settings writer, the chat history) waits, then sees the new version.
		loop {
			let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
			let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
			let Some(migration) = MIGRATIONS.get(version as usize) else { break };
			match migration {
				Migration::Sql(sql) => tx.execute_batch(sql)?,
				Migration::Code(migrate) => migrate(&tx)?,
			}
			tx.pragma_update(None, "user_version", version + 1)?;
			tx.commit()?;
		}
		Ok(Self { db })
	}

	/// The schema version (number of migrations applied).
	pub fn schema_version(&self) -> Result<i64> {
		Ok(self.db.pragma_query_value(None, "user_version", |r| r.get(0))?)
	}

	/// The number of schema migrations this version knows.
	pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

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
			.prepare_cached("SELECT value FROM settings WHERE key = ?1")?
			.query_row([key], |r| r.get(0))
			.optional()?;
		value.map(|v| serde_json::from_str(&v).map_err(Error::from)).transpose()
	}

	pub fn set_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
		self.set_setting_json(key, &serde_json::to_string(value)?)
	}

	/// Store a setting that is already JSON text.
	pub fn set_setting_json(&self, key: &str, json: &str) -> Result<()> {
		self.db
			.prepare_cached(
				"INSERT INTO settings (key, value) VALUES (?1, ?2)
				 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
			)?
			.execute(params![key, json])?;
		Ok(())
	}

	/// Remove a setting; `true` if it was there.
	pub fn delete_setting(&self, key: &str) -> Result<bool> {
		Ok(self.db.prepare_cached("DELETE FROM settings WHERE key = ?1")?.execute([key])? > 0)
	}

	/// All settings as `(key, JSON text)`, ordered by key.
	pub fn settings_json(&self) -> Result<Vec<(String, String)>> {
		let mut stmt = self.db.prepare_cached("SELECT key, value FROM settings ORDER BY key")?;
		let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
		Ok(rows.collect::<rusqlite::Result<_>>()?)
	}

	/// Store (`Some(JSON text)`) or remove (`None`) several settings in one
	/// transaction.
	pub fn write_settings<'a>(
		&mut self,
		changes: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
	) -> Result<()> {
		let tx = self.db.transaction()?;
		{
			let mut put = tx.prepare_cached(
				"INSERT INTO settings (key, value) VALUES (?1, ?2)
				 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
			)?;
			let mut delete = tx.prepare_cached("DELETE FROM settings WHERE key = ?1")?;
			for (key, json) in changes {
				match json {
					Some(json) => put.execute(params![key, json])?,
					None => delete.execute([key])?,
				};
			}
		}
		tx.commit()?;
		Ok(())
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
		let mut store = Store::open_in_memory().unwrap();
		assert_eq!(store.setting::<u32>("volume").unwrap(), None);
		store.set_setting("volume", &80u32).unwrap();
		store.set_setting("volume", &90u32).unwrap();
		assert_eq!(store.setting::<u32>("volume").unwrap(), Some(90));
		store.write_settings([("a", Some("1")), ("volume", None), ("b", Some("\"x\""))]).unwrap();
		let all = store.settings_json().unwrap();
		assert_eq!(all, [("a".to_owned(), "1".to_owned()), ("b".to_owned(), "\"x\"".to_owned())]);
		assert!(store.delete_setting("a").unwrap());
		assert!(!store.delete_setting("a").unwrap());
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
