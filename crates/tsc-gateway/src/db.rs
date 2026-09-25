//! SQLite: login tokens, chat history, audit log.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use tsc_model::{ChatMessage, ChatTarget};

pub struct Db(Mutex<Connection>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenInfo {
	pub uid: String,
	pub cldbid: u64,
	pub expires: i64,
}

fn target_key(target: &ChatTarget) -> String {
	match target {
		ChatTarget::Server => "server".into(),
		ChatTarget::Channel(cid) => format!("channel/{cid}"),
		ChatTarget::Private(uid) => format!("private/{uid}"),
	}
}

impl Db {
	pub fn open(path: &Path) -> rusqlite::Result<Self> {
		Self::init(Connection::open(path)?)
	}

	#[cfg(test)]
	pub fn in_memory() -> rusqlite::Result<Self> {
		Self::init(Connection::open_in_memory()?)
	}

	fn init(conn: Connection) -> rusqlite::Result<Self> {
		conn.pragma_update(None, "journal_mode", "WAL")?;
		conn.execute_batch(
			"CREATE TABLE IF NOT EXISTS tokens (
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
			);",
		)?;
		Ok(Self(Mutex::new(conn)))
	}

	pub fn add_token(&self, hash: &str, info: &TokenInfo) -> rusqlite::Result<()> {
		self.0.lock().unwrap().execute(
			"INSERT OR REPLACE INTO tokens (hash, uid, cldbid, expires) VALUES (?1, ?2, ?3, ?4)",
			params![hash, info.uid, info.cldbid as i64, info.expires],
		)?;
		Ok(())
	}

	/// A token that has not expired at `now`.
	pub fn token(&self, hash: &str, now: i64) -> rusqlite::Result<Option<TokenInfo>> {
		self.0
			.lock()
			.unwrap()
			.query_row(
				"SELECT uid, cldbid, expires FROM tokens WHERE hash = ?1 AND expires > ?2",
				params![hash, now],
				|r| {
					Ok(TokenInfo {
						uid: r.get(0)?,
						cldbid: r.get::<_, i64>(1)? as u64,
						expires: r.get(2)?,
					})
				},
			)
			.optional()
	}

	/// Store a message; returns its id.
	pub fn add_message(&self, msg: &ChatMessage) -> rusqlite::Result<i64> {
		let conn = self.0.lock().unwrap();
		conn.execute(
			"INSERT INTO messages (target, ts_ms, author_uid, author_name, text) VALUES (?1, ?2, ?3, ?4, ?5)",
			params![target_key(&msg.target), msg.ts_ms, msg.author_uid, msg.author_name, msg.text],
		)?;
		Ok(conn.last_insert_rowid())
	}

	/// Up to `limit` messages before id `before`, oldest first.
	pub fn history(
		&self,
		target: &ChatTarget,
		before: Option<i64>,
		limit: u32,
	) -> rusqlite::Result<Vec<(i64, ChatMessage)>> {
		let conn = self.0.lock().unwrap();
		let mut stmt = conn.prepare(
			"SELECT id, ts_ms, author_uid, author_name, text FROM messages
			 WHERE target = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3",
		)?;
		let rows =
			stmt.query_map(params![target_key(target), before.unwrap_or(i64::MAX), limit], |r| {
				Ok((
					r.get::<_, i64>(0)?,
					ChatMessage {
						target: target.clone(),
						ts_ms: r.get(1)?,
						author_uid: r.get(2)?,
						author_name: r.get(3)?,
						author_id: None,
						text: r.get(4)?,
						via_relay: true,
					},
				))
			})?;
		let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
		out.reverse();
		Ok(out)
	}

	pub fn prune(&self, now: i64, retention_days: u64) -> rusqlite::Result<()> {
		let cutoff_ms = (now - retention_days as i64 * 86_400) * 1000;
		let conn = self.0.lock().unwrap();
		conn.execute("DELETE FROM messages WHERE ts_ms < ?1", [cutoff_ms])?;
		conn.execute("DELETE FROM tokens WHERE expires < ?1", [now])?;
		Ok(())
	}

	pub fn audit(&self, now: i64, uid: Option<&str>, action: &str, detail: &str) {
		let result = self.0.lock().unwrap().execute(
			"INSERT INTO audit (ts, uid, action, detail) VALUES (?1, ?2, ?3, ?4)",
			params![now, uid, action, detail],
		);
		if let Err(error) = result {
			tracing::warn!(%error, "failed to write audit log");
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn msg(target: ChatTarget, text: &str, ts_ms: i64) -> ChatMessage {
		ChatMessage {
			target,
			author_name: "a".into(),
			author_uid: Some("u".into()),
			author_id: None,
			text: text.into(),
			ts_ms,
			via_relay: true,
		}
	}

	#[test]
	fn tokens_expire() {
		let db = Db::in_memory().unwrap();
		let info = TokenInfo { uid: "u".into(), cldbid: 5, expires: 100 };
		db.add_token("h", &info).unwrap();
		assert_eq!(db.token("h", 50).unwrap(), Some(info));
		assert_eq!(db.token("h", 150).unwrap(), None);
		assert_eq!(db.token("x", 50).unwrap(), None);
	}

	#[test]
	fn history_pages_per_target() {
		let db = Db::in_memory().unwrap();
		for i in 0..5 {
			db.add_message(&msg(ChatTarget::Channel(1), &format!("c{i}"), i)).unwrap();
			db.add_message(&msg(ChatTarget::Server, &format!("s{i}"), i)).unwrap();
		}
		let last = db.history(&ChatTarget::Channel(1), None, 2).unwrap();
		assert_eq!(last.iter().map(|(_, m)| m.text.as_str()).collect::<Vec<_>>(), ["c3", "c4"]);
		let before = db.history(&ChatTarget::Channel(1), Some(last[0].0), 10).unwrap();
		assert_eq!(before.len(), 3);
		assert_eq!(db.history(&ChatTarget::Server, None, 10).unwrap().len(), 5);
		db.prune(10, 0).unwrap();
		assert!(db.history(&ChatTarget::Server, None, 10).unwrap().is_empty());
	}
}
