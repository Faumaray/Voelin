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
	Migration::Sql(SCHEMA_4),
	Migration::Sql(crate::chat::SCHEMA_5),
];

/// Version 4: where each identity came from ([`IdentityOrigin`]) and which
/// one is the default. Until now the app created an identity named
/// `Default` when it had none, and the only other way in was an import;
/// the default was the first identity.
const SCHEMA_4: &str = r#"
	ALTER TABLE identities ADD COLUMN origin TEXT NOT NULL DEFAULT 'user';
	ALTER TABLE identities ADD COLUMN is_default INTEGER NOT NULL DEFAULT 0;
	UPDATE identities SET origin = 'imported';
	UPDATE identities SET origin = 'created'
		WHERE id = (SELECT MIN(id) FROM identities) AND name = 'Default';
	UPDATE identities SET is_default = 1 WHERE id = (SELECT MIN(id) FROM identities);
"#;

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
	pub origin: IdentityOrigin,
	/// The identity the app connects with.
	pub is_default: bool,
}

/// Where an identity came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityOrigin {
	/// Made by the app on its own because it had none; nobody chose it, so
	/// the official client's identity may take its place as the default.
	Created,
	/// From another client (`voelin_core::identity`).
	Imported,
	/// Made or chosen as the default by the user.
	User,
}

impl IdentityOrigin {
	fn as_str(self) -> &'static str {
		match self {
			Self::Created => "created",
			Self::Imported => "imported",
			Self::User => "user",
		}
	}

	fn parse(text: &str) -> Self {
		match text {
			"created" => Self::Created,
			"imported" => Self::Imported,
			_ => Self::User,
		}
	}
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
	/// The companion gateway in use (`wss://` or `ws://`), for invisible
	/// presence and relay chat: the one that last logged in, else the best
	/// published one, else one typed into an older version.
	pub gateway_url: Option<String>,
	/// Every URL the server publishes for its gateway, best first: tried in
	/// this order.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub gateway_urls: Vec<String>,
	/// Own ServerQuery credentials (the password is kept in [`crate::Secrets`]).
	pub query: Option<QueryConfig>,
	/// Signed client version to present (`voelinctl versions` spec), `None` for the default.
	pub client_version: Option<String>,
	/// Last known icon, available before connecting. Bytes live in the image cache.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cached_server_icon: Option<CachedServerIcon>,
}

/// Bind cached metadata to the address that supplied it, so editing a bookmark
/// cannot display another server's icon. Do not persist machine-specific paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedServerIcon {
	pub address: String,
	pub id: u32,
}

impl Bookmark {
	/// The gateways to try, best first: those published, else the one kept.
	pub fn gateways(&self) -> Vec<String> {
		if self.gateway_urls.is_empty() {
			self.gateway_url.iter().cloned().collect()
		} else {
			self.gateway_urls.clone()
		}
	}

	/// Discovery found `urls` (best first, not empty); true if what is tried
	/// changed. The one in use stays if it is still published.
	pub fn set_gateways(&mut self, urls: Vec<String>) -> bool {
		if self.gateway_urls == urls {
			return false;
		}
		if !self.gateway_url.as_ref().is_some_and(|url| urls.contains(url)) {
			self.gateway_url = urls.first().cloned();
		}
		self.gateway_urls = urls;
		true
	}

	/// The gateway logged in at `url`; true if the one in use changed.
	pub fn gateway_in_use(&mut self, url: &str) -> bool {
		if self.gateway_url.as_deref() == Some(url) || !self.gateways().iter().any(|u| u == url) {
			return false;
		}
		self.gateway_url = Some(url.to_owned());
		true
	}

	pub fn server_icon_id(&self) -> Option<u32> {
		self.cached_server_icon
			.as_ref()
			.filter(|icon| icon.address == self.address.trim() && icon.id != 0)
			.map(|icon| icon.id)
	}

	/// Record a server's latest icon; zero removes the previous one.
	pub fn remember_server_icon(&mut self, id: u32) {
		self.cached_server_icon =
			(id != 0).then(|| CachedServerIcon { address: self.address.trim().to_owned(), id });
	}

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

	/// Store an identity; it is the default when no other one is.
	pub fn add_identity(
		&self,
		name: &str,
		identity: &Identity,
		origin: IdentityOrigin,
	) -> Result<i64> {
		let uid = identity.key().to_pub().get_uid();
		self.db.execute(
			"INSERT INTO identities (name, uid, data, origin, is_default)
			 VALUES (?1, ?2, ?3, ?4, NOT EXISTS (SELECT 1 FROM identities WHERE is_default))",
			params![name, uid, serde_json::to_string(identity)?, origin.as_str()],
		)?;
		Ok(self.db.last_insert_rowid())
	}

	pub fn identities(&self) -> Result<Vec<IdentityEntry>> {
		let mut stmt = self.db.prepare(
			"SELECT id, name, uid, data, origin, is_default FROM identities ORDER BY id",
		)?;
		let rows = stmt.query_map([], |r| {
			Ok((
				r.get::<_, i64>(0)?,
				r.get(1)?,
				r.get(2)?,
				r.get::<_, String>(3)?,
				r.get::<_, String>(4)?,
				r.get(5)?,
			))
		})?;
		rows.map(|row| {
			let (id, name, uid, data, origin, is_default) = row?;
			let identity: Identity = serde_json::from_str(&data)?;
			let origin = IdentityOrigin::parse(&origin);
			Ok(IdentityEntry { id, name, uid, level: identity.level(), origin, is_default })
		})
		.collect()
	}

	/// The identity the app connects with: the one marked as the default,
	/// else the first.
	pub fn default_identity(&self) -> Result<Option<IdentityEntry>> {
		let identities = self.identities()?;
		Ok(identities.iter().find(|i| i.is_default).or(identities.first()).cloned())
	}

	/// Make `id` the default identity. `by_user`: the user chose it, so one
	/// the app created on its own becomes theirs ([`IdentityOrigin::User`])
	/// and is never replaced by an import again.
	pub fn set_default_identity(&self, id: i64, by_user: bool) -> Result<()> {
		let tx = self.db.unchecked_transaction()?;
		if tx.execute("UPDATE identities SET is_default = 1 WHERE id = ?1", [id])? == 0 {
			return Err(Error::NotFound("identity", id));
		}
		tx.execute("UPDATE identities SET is_default = 0 WHERE id <> ?1", [id])?;
		if by_user {
			tx.execute(
				"UPDATE identities SET origin = 'user' WHERE id = ?1 AND origin = 'created'",
				[id],
			)?;
		}
		tx.commit()?;
		Ok(())
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
		let id = store.add_identity("main", &identity, IdentityOrigin::Imported).unwrap();
		let list = store.identities().unwrap();
		assert_eq!(list.len(), 1);
		assert_eq!(list[0].name, "main");
		assert_eq!(list[0].uid, identity.key().to_pub().get_uid());
		assert!(list[0].level >= 8);
		assert_eq!(list[0].origin, IdentityOrigin::Imported);
		let loaded = store.identity(id).unwrap();
		assert_eq!(loaded.counter(), identity.counter());
		// The same key twice is rejected (uid is unique).
		assert!(store.add_identity("dup", &identity, IdentityOrigin::User).is_err());
		store.rename_identity(id, "renamed").unwrap();
		assert_eq!(store.identities().unwrap()[0].name, "renamed");
		store.delete_identity(id).unwrap();
		assert!(matches!(store.identity(id), Err(Error::NotFound(..))));
	}

	#[test]
	fn the_default_identity() {
		let store = Store::open_in_memory().unwrap();
		assert_eq!(store.default_identity().unwrap(), None);
		let made = store.add_identity("Default", &Identity::create(), IdentityOrigin::Created);
		let made = made.unwrap();
		let other = store.add_identity("Other", &Identity::create(), IdentityOrigin::Imported);
		let other = other.unwrap();
		// The first one is the default.
		let default = store.default_identity().unwrap().unwrap();
		assert_eq!((default.id, default.origin), (made, IdentityOrigin::Created));
		// An import moves the default without making the old one the user's.
		store.set_default_identity(other, false).unwrap();
		assert_eq!(store.default_identity().unwrap().unwrap().id, other);
		assert_eq!(store.identities().unwrap()[0].origin, IdentityOrigin::Created);
		// The user's choice makes the created one theirs.
		store.set_default_identity(made, true).unwrap();
		let list = store.identities().unwrap();
		assert_eq!((list[0].is_default, list[0].origin), (true, IdentityOrigin::User));
		assert_eq!((list[1].is_default, list[1].origin), (false, IdentityOrigin::Imported));
		assert!(matches!(store.set_default_identity(99, true), Err(Error::NotFound(..))));
		assert_eq!(store.default_identity().unwrap().unwrap().id, made);
	}

	/// A database of version 3 learns where its identities came from: the
	/// first one, named `Default`, is the one the app made; the others were
	/// imported; the first stays the default.
	#[test]
	fn migrates_version_3_identities() {
		let dir = std::env::temp_dir().join(format!("voelin-store-v3-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("client.db");
		let (made, imported) = (Identity::create(), Identity::create());
		{
			let mut db = rusqlite::Connection::open(&path).unwrap();
			db.execute_batch(SCHEMA_1).unwrap();
			let tx = db.transaction().unwrap();
			crate::chat::migrate_2(&tx).unwrap();
			tx.commit().unwrap();
			db.execute_batch(crate::contacts::SCHEMA_3).unwrap();
			db.pragma_update(None, "user_version", 3).unwrap();
			for (name, identity) in [("Default", &made), ("Main", &imported)] {
				let uid = identity.key().to_pub().get_uid();
				let data = serde_json::to_string(identity).unwrap();
				db.execute(
					"INSERT INTO identities (name, uid, data) VALUES (?1, ?2, ?3)",
					params![name, uid, data],
				)
				.unwrap();
			}
		}
		let store = Store::open(&path).unwrap();
		assert_eq!(store.schema_version().unwrap(), Store::SCHEMA_VERSION);
		let list = store.identities().unwrap();
		assert_eq!((list[0].origin, list[0].is_default), (IdentityOrigin::Created, true));
		assert_eq!((list[1].origin, list[1].is_default), (IdentityOrigin::Imported, false));
		// The keys are untouched.
		assert_eq!(store.identity(list[1].id).unwrap().counter(), imported.counter());
		drop(store);
		std::fs::remove_dir_all(dir).unwrap();
	}

	/// A database of version 4 learns where its chats were read, keeping
	/// its messages; nothing is read yet.
	#[test]
	fn migrates_version_4_read_markers() {
		use crate::{ChatRead, ChatTarget, PageQuery};
		let dir = std::env::temp_dir().join(format!("voelin-store-v4-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("client.db");
		{
			let mut db = rusqlite::Connection::open(&path).unwrap();
			db.execute_batch(SCHEMA_1).unwrap();
			let tx = db.transaction().unwrap();
			crate::chat::migrate_2(&tx).unwrap();
			tx.commit().unwrap();
			db.execute_batch(crate::contacts::SCHEMA_3).unwrap();
			db.execute_batch(SCHEMA_4).unwrap();
			db.pragma_update(None, "user_version", 4).unwrap();
			db.execute(
				"INSERT INTO messages (server_uid, target, ts, ts_ms, author_name, text)
				 VALUES ('srv', 'channel/1', 1, 1000, 'a', 'kept')",
				[],
			)
			.unwrap();
		}
		let store = Store::open(&path).unwrap();
		assert_eq!(Store::SCHEMA_VERSION, 5);
		assert_eq!(store.schema_version().unwrap(), Store::SCHEMA_VERSION);
		let channel = ChatTarget::Channel(1);
		let rows = store.messages("srv", &channel, PageQuery::default()).unwrap();
		assert_eq!(rows.len(), 1);
		assert!(store.chat_reads("srv").unwrap().is_empty());
		let read = ChatRead { ts_ms: rows[0].ts_ms, id: rows[0].id, updated_ms: 1 };
		store.set_chat_read("srv", &channel, &read).unwrap();
		drop(store);
		// Opened again: the marker is there and nothing runs twice.
		let store = Store::open(&path).unwrap();
		assert_eq!(store.chat_read("srv", &channel).unwrap(), Some(read));
		drop(store);
		std::fs::remove_dir_all(dir).unwrap();
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
	fn published_gateways_are_kept_with_the_one_in_use() {
		let store = Store::open_in_memory().unwrap();
		// From an older version: one URL, no list.
		let mut b = Bookmark {
			address: "ts.example.org".into(),
			gateway_url: Some("ws://127.0.0.1:7788/v1".into()),
			..Default::default()
		};
		b.id = store.add_bookmark(&b).unwrap();
		assert_eq!(b.gateways(), ["ws://127.0.0.1:7788/v1"]);
		let published =
			vec!["wss://gw.example.org/v1".to_owned(), "ws://ts.example.org:7788/v1".into()];
		assert!(b.set_gateways(published.clone()));
		assert!(!b.set_gateways(published.clone()), "unchanged");
		// The old one is not published: the best published one is in use.
		assert_eq!(b.gateway_url.as_deref(), Some("wss://gw.example.org/v1"));
		assert_eq!(b.gateways(), published);
		// The plain one logged in; one that is not published never counts.
		assert!(b.gateway_in_use("ws://ts.example.org:7788/v1"));
		assert!(!b.gateway_in_use("ws://ts.example.org:7788/v1"));
		assert!(!b.gateway_in_use("ws://elsewhere.test/v1"));
		store.update_bookmark(&b).unwrap();
		let reloaded = store.bookmarks().unwrap().remove(0);
		assert_eq!(reloaded, b);
		// Still published: the one in use stays.
		b.set_gateways(vec!["ws://ts.example.org:7788/v1".into()]);
		assert_eq!(b.gateway_url.as_deref(), Some("ws://ts.example.org:7788/v1"));
	}

	#[test]
	fn bookmark_icons_survive_reload_and_are_bound_to_the_server_address() {
		let store = Store::open_in_memory().unwrap();
		let mut bookmark = Bookmark { address: "example.test:9987".into(), ..Default::default() };
		bookmark.id = store.add_bookmark(&bookmark).unwrap();
		assert_eq!(bookmark.server_icon_id(), None);
		bookmark.remember_server_icon(4_000_000_000);
		store.update_bookmark(&bookmark).unwrap();
		let mut reloaded = store.bookmarks().unwrap().remove(0);
		assert_eq!(reloaded.server_icon_id(), Some(4_000_000_000));
		reloaded.address = "other.test:9987".into();
		assert_eq!(reloaded.server_icon_id(), None);
		reloaded.remember_server_icon(42);
		assert_eq!(reloaded.server_icon_id(), Some(42));
		reloaded.remember_server_icon(0);
		assert!(reloaded.cached_server_icon.is_none());
		store.update_bookmark(&reloaded).unwrap();
		assert_eq!(store.bookmarks().unwrap()[0].server_icon_id(), None);
		let old = serde_json::to_value(Bookmark::default()).unwrap();
		assert!(old.get("cached_server_icon").is_none());
		assert_eq!(serde_json::from_value::<Bookmark>(old).unwrap().server_icon_id(), None);
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
