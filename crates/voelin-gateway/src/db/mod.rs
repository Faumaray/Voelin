//! SQLite: login tokens, chat history with pins, reactions and topics,
//! events, the stream directory, the activity feed, runtime settings and
//! the audit log.
//!
//! The schema is versioned (`PRAGMA user_version`) and migrated in place on
//! open, keeping existing data. WAL journal, statement cache, one
//! transaction per logical change.

mod chat;
mod events;
mod feed;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use voelin_gateway_proto::UserRef;
use voelin_model::ChatTarget;

pub use chat::HistoryOptions;
pub use events::EventFilter;
pub use feed::NewActivity;

pub struct Db(Mutex<Inner>);

struct Inner {
	conn: Connection,
	/// Last revision handed out; see [`Inner::next_rev`].
	rev: i64,
}

impl Inner {
	/// Revisions order changes to messages across restarts: at least the
	/// current time in microseconds, and always increasing.
	fn next_rev(&mut self, now_ms: i64) -> i64 {
		self.rev = (self.rev + 1).max(now_ms.saturating_mul(1000));
		self.rev
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenInfo {
	pub uid: String,
	pub cldbid: u64,
	pub expires: i64,
}

pub fn target_key(target: &ChatTarget) -> String {
	match target {
		ChatTarget::Server => "server".into(),
		ChatTarget::Channel(cid) => format!("channel/{cid}"),
		ChatTarget::Private(uid) => format!("private/{uid}"),
	}
}

pub fn parse_target(key: &str) -> ChatTarget {
	match key.split_once('/') {
		Some(("channel", cid)) => ChatTarget::Channel(cid.parse().unwrap_or_default()),
		Some(("private", uid)) => ChatTarget::Private(uid.to_string()),
		_ => ChatTarget::Server,
	}
}

fn user(uid: String, name: String) -> UserRef {
	UserRef { uid, name }
}

/// The original schema (before versioning), kept so a new database and an
/// old one migrate the same way.
const SCHEMA_0: &str = "
CREATE TABLE IF NOT EXISTS tokens (
	hash TEXT PRIMARY KEY,
	uid TEXT NOT NULL,
	cldbid INTEGER NOT NULL,
	expires INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS messages (
	id INTEGER PRIMARY KEY,
	target TEXT NOT NULL,
	ts_ms INTEGER NOT NULL,
	author_uid TEXT,
	author_name TEXT NOT NULL,
	text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_by_target ON messages (target, id);
CREATE TABLE IF NOT EXISTS audit (
	ts INTEGER NOT NULL,
	uid TEXT,
	action TEXT NOT NULL,
	detail TEXT
);";

/// `MIGRATIONS[n]` takes the schema from version `n` to `n + 1`.
const MIGRATIONS: &[&str] = &["
-- Messages: ids that are never reused (AUTOINCREMENT), topics, revisions.
CREATE TABLE messages_v1 (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	target TEXT NOT NULL,
	ts_ms INTEGER NOT NULL,
	author_uid TEXT,
	author_name TEXT NOT NULL,
	text TEXT NOT NULL,
	topic_id INTEGER,
	rev INTEGER NOT NULL
);
INSERT INTO messages_v1 (id, target, ts_ms, author_uid, author_name, text, rev)
	SELECT id, target, ts_ms, author_uid, author_name, text, id FROM messages;
DROP TABLE messages;
ALTER TABLE messages_v1 RENAME TO messages;
CREATE INDEX messages_by_target ON messages (target, id);
CREATE INDEX messages_main ON messages (target, id) WHERE topic_id IS NULL;
CREATE INDEX messages_by_topic ON messages (topic_id, id) WHERE topic_id IS NOT NULL;
CREATE INDEX messages_by_time ON messages (target, ts_ms);
CREATE INDEX messages_by_rev ON messages (target, rev);
CREATE INDEX messages_ts ON messages (ts_ms);

CREATE TABLE reactions (
	message_id INTEGER NOT NULL,
	emoji TEXT NOT NULL,
	uid TEXT NOT NULL,
	name TEXT NOT NULL,
	ts_ms INTEGER NOT NULL,
	PRIMARY KEY (message_id, emoji, uid)
) WITHOUT ROWID;

CREATE TABLE pins (
	message_id INTEGER PRIMARY KEY,
	target TEXT NOT NULL,
	by_uid TEXT NOT NULL,
	by_name TEXT NOT NULL,
	ts_ms INTEGER NOT NULL
);
CREATE INDEX pins_by_target ON pins (target, ts_ms);

CREATE TABLE topics (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	target TEXT NOT NULL,
	title TEXT NOT NULL,
	creator_uid TEXT NOT NULL,
	creator_name TEXT NOT NULL,
	created_ms INTEGER NOT NULL,
	root_message_id INTEGER,
	last_activity_ms INTEGER NOT NULL,
	message_count INTEGER NOT NULL DEFAULT 0,
	archived INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX topics_by_target ON topics (target, last_activity_ms);
CREATE INDEX topics_by_title ON topics (target, title);
CREATE INDEX topics_by_activity ON topics (last_activity_ms);

CREATE TABLE events (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	title TEXT NOT NULL,
	description TEXT NOT NULL,
	start_ms INTEGER NOT NULL,
	end_ms INTEGER,
	channel INTEGER,
	kind TEXT NOT NULL,
	stream_title TEXT,
	stream_game TEXT,
	host_uid TEXT NOT NULL,
	creator_uid TEXT NOT NULL,
	creator_name TEXT NOT NULL,
	created_ms INTEGER NOT NULL,
	updated_ms INTEGER NOT NULL,
	live_stream TEXT
);
CREATE INDEX events_by_start ON events (start_ms);
CREATE INDEX events_by_end ON events (COALESCE(end_ms, start_ms));
CREATE INDEX events_by_channel ON events (channel, start_ms);
CREATE INDEX events_by_creator ON events (creator_uid, start_ms);
CREATE INDEX events_by_host ON events (host_uid, start_ms);
CREATE INDEX events_by_stream ON events (live_stream) WHERE live_stream IS NOT NULL;

CREATE TABLE rsvps (
	event_id INTEGER NOT NULL,
	uid TEXT NOT NULL,
	name TEXT NOT NULL,
	status TEXT NOT NULL,
	ts_ms INTEGER NOT NULL,
	PRIMARY KEY (event_id, uid)
) WITHOUT ROWID;

-- Reminders already sent: (event, minutes before the start).
CREATE TABLE event_notices (
	event_id INTEGER NOT NULL,
	offset_min INTEGER NOT NULL,
	PRIMARY KEY (event_id, offset_min)
) WITHOUT ROWID;

-- The stream directory, so it survives a restart (JSON of StreamEntry).
CREATE TABLE streams (
	id TEXT PRIMARY KEY,
	data TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE activity (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	ts_ms INTEGER NOT NULL,
	kind TEXT NOT NULL,
	actor_uid TEXT,
	actor_name TEXT,
	channel INTEGER,
	ref_id TEXT,
	text TEXT NOT NULL,
	data TEXT
);
CREATE INDEX activity_by_time ON activity (ts_ms);

-- Settings changed at runtime (JSON values); they win over the file.
CREATE TABLE config (
	key TEXT PRIMARY KEY,
	value TEXT NOT NULL,
	updated_ms INTEGER NOT NULL,
	updated_by TEXT
) WITHOUT ROWID;

CREATE INDEX tokens_by_expiry ON tokens (expires);
CREATE INDEX audit_by_time ON audit (ts);
"];

/// Current schema version.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

fn now_ms() -> i64 {
	crate::hub::now_ms()
}

impl Db {
	pub fn open(path: &Path) -> rusqlite::Result<Self> {
		Self::init(Connection::open(path)?)
	}

	#[cfg(test)]
	pub fn in_memory() -> rusqlite::Result<Self> {
		Self::init(Connection::open_in_memory()?)
	}

	fn init(mut conn: Connection) -> rusqlite::Result<Self> {
		conn.pragma_update(None, "journal_mode", "WAL")?;
		// Safe with WAL: a crash loses at most the last transactions, never
		// consistency.
		conn.pragma_update(None, "synchronous", "NORMAL")?;
		conn.set_prepared_statement_cache_capacity(128);
		migrate(&mut conn)?;
		let rev: i64 =
			conn.query_row("SELECT COALESCE(MAX(rev), 0) FROM messages", [], |r| r.get(0))?;
		Ok(Self(Mutex::new(Inner { conn, rev })))
	}

	fn lock(&self) -> MutexGuard<'_, Inner> {
		self.0.lock().unwrap_or_else(|e| e.into_inner())
	}

	pub fn schema_version(&self) -> rusqlite::Result<i64> {
		self.lock().conn.pragma_query_value(None, "user_version", |r| r.get(0))
	}

	pub fn add_token(&self, hash: &str, info: &TokenInfo) -> rusqlite::Result<()> {
		self.lock()
			.conn
			.prepare_cached(
				"INSERT OR REPLACE INTO tokens (hash, uid, cldbid, expires) VALUES (?1, ?2, ?3, ?4)",
			)?
			.execute(params![hash, info.uid, info.cldbid as i64, info.expires])?;
		Ok(())
	}

	/// A token that has not expired at `now`.
	pub fn token(&self, hash: &str, now: i64) -> rusqlite::Result<Option<TokenInfo>> {
		self.lock()
			.conn
			.prepare_cached(
				"SELECT uid, cldbid, expires FROM tokens WHERE hash = ?1 AND expires > ?2",
			)?
			.query_row(params![hash, now], |r| {
				Ok(TokenInfo {
					uid: r.get(0)?,
					cldbid: r.get::<_, i64>(1)? as u64,
					expires: r.get(2)?,
				})
			})
			.optional()
	}

	/// Drop expired tokens.
	pub fn prune_tokens(&self, now: i64) -> rusqlite::Result<usize> {
		self.lock().conn.prepare_cached("DELETE FROM tokens WHERE expires < ?1")?.execute([now])
	}

	pub fn audit(&self, now: i64, uid: Option<&str>, action: &str, detail: &str) {
		let result = self
			.lock()
			.conn
			.prepare_cached("INSERT INTO audit (ts, uid, action, detail) VALUES (?1, ?2, ?3, ?4)")
			.and_then(|mut s| s.execute(params![now, uid, action, detail]));
		if let Err(error) = result {
			tracing::warn!(%error, "failed to write audit log");
		}
	}

	/// Settings stored at runtime.
	pub fn config_values(&self) -> rusqlite::Result<Vec<(String, Value)>> {
		let g = self.lock();
		let mut stmt = g.conn.prepare_cached("SELECT key, value FROM config ORDER BY key")?;
		let rows = stmt.query_map([], |r| {
			let text: String = r.get(1)?;
			Ok((r.get(0)?, serde_json::from_str(&text).unwrap_or(Value::Null)))
		})?;
		rows.collect()
	}

	pub fn set_config(&self, key: &str, value: &Value, by: Option<&str>) -> rusqlite::Result<()> {
		self.lock()
			.conn
			.prepare_cached(
				"INSERT INTO config (key, value, updated_ms, updated_by) VALUES (?1, ?2, ?3, ?4)
				 ON CONFLICT (key) DO UPDATE SET value = excluded.value,
					updated_ms = excluded.updated_ms, updated_by = excluded.updated_by",
			)?
			.execute(params![key, value.to_string(), now_ms(), by])?;
		Ok(())
	}

	pub fn delete_config(&self, key: &str) -> rusqlite::Result<()> {
		self.lock().conn.prepare_cached("DELETE FROM config WHERE key = ?1")?.execute([key])?;
		Ok(())
	}

	/// `EXPLAIN QUERY PLAN` details, for tests of index use.
	#[cfg(test)]
	pub fn query_plan(&self, sql: &str) -> Vec<String> {
		let g = self.lock();
		let mut stmt = g.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
		// Unbound parameters are fine for a plan.
		let mut rows = stmt.raw_query();
		let mut out = Vec::new();
		while let Some(r) = rows.next().unwrap() {
			out.push(r.get::<_, String>(3).unwrap());
		}
		out
	}

	#[cfg(test)]
	fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
		f(&self.lock().conn)
	}
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
	let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
	if version >= SCHEMA_VERSION {
		return Ok(());
	}
	let tx = conn.transaction()?;
	if version == 0 {
		tx.execute_batch(SCHEMA_0)?;
	}
	for (i, step) in MIGRATIONS.iter().enumerate().skip(version as usize) {
		tx.execute_batch(step)?;
		tx.pragma_update(None, "user_version", i as i64 + 1)?;
		tracing::info!(from = i, to = i + 1, "migrated database");
	}
	tx.commit()
}

#[cfg(test)]
mod tests;
