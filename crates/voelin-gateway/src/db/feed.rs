//! The stream directory and the activity feed.

use rusqlite::params;
use serde_json::Value;
use voelin_gateway_proto::{ActivityEntry, StreamEntry, UserRef};
use voelin_model::ChannelId;

use super::{Db, user};

/// An activity entry before it is stored.
#[derive(Clone, Debug, Default)]
pub struct NewActivity<'a> {
	pub kind: &'a str,
	pub actor: Option<&'a UserRef>,
	pub channel: Option<ChannelId>,
	pub ref_id: Option<String>,
	pub text: String,
	pub data: Value,
}

impl Db {
	pub fn save_stream(&self, entry: &StreamEntry) -> rusqlite::Result<()> {
		let data = serde_json::to_string(entry).expect("serializable");
		self.lock()
			.conn
			.prepare_cached("INSERT OR REPLACE INTO streams (id, data) VALUES (?1, ?2)")?
			.execute(params![entry.id, data])?;
		Ok(())
	}

	pub fn delete_stream(&self, id: &str) -> rusqlite::Result<()> {
		self.lock().conn.prepare_cached("DELETE FROM streams WHERE id = ?1")?.execute([id])?;
		Ok(())
	}

	/// The stored directory (entries that no longer parse are skipped).
	pub fn streams(&self) -> rusqlite::Result<Vec<StreamEntry>> {
		let g = self.lock();
		let mut stmt = g.conn.prepare_cached("SELECT data FROM streams")?;
		let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
		Ok(rows.filter_map(|r| r.ok()).filter_map(|d| serde_json::from_str(&d).ok()).collect())
	}

	pub fn add_activity(&self, a: NewActivity<'_>, ts_ms: i64) -> rusqlite::Result<ActivityEntry> {
		let g = self.lock();
		let data = (!a.data.is_null()).then(|| a.data.to_string());
		g.conn
			.prepare_cached(
				"INSERT INTO activity (ts_ms, kind, actor_uid, actor_name, channel, ref_id, text, data)
				 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
			)?
			.execute(params![
				ts_ms,
				a.kind,
				a.actor.map(|u| &u.uid),
				a.actor.map(|u| &u.name),
				a.channel.map(|c| c as i64),
				a.ref_id,
				a.text,
				data
			])?;
		Ok(ActivityEntry {
			id: g.conn.last_insert_rowid(),
			ts_ms,
			kind: a.kind.to_string(),
			actor: a.actor.cloned(),
			channel: a.channel,
			ref_id: a.ref_id,
			text: a.text,
			data: a.data,
		})
	}

	/// Newest first, below id `before`; and whether older entries exist.
	pub fn activity(
		&self,
		before: Option<i64>,
		limit: Option<u32>,
	) -> rusqlite::Result<(Vec<ActivityEntry>, bool)> {
		let g = self.lock();
		let mut stmt = g.conn.prepare_cached(
			"SELECT id, ts_ms, kind, actor_uid, actor_name, channel, ref_id, text, data
			 FROM activity WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
		)?;
		let limit_sql = limit.map_or(-1, |l| i64::from(l) + 1);
		let mut entries = stmt
			.query_map(params![before.unwrap_or(i64::MAX), limit_sql], |r| {
				let actor_uid: Option<String> = r.get(3)?;
				let actor_name: Option<String> = r.get(4)?;
				let data: Option<String> = r.get(8)?;
				Ok(ActivityEntry {
					id: r.get(0)?,
					ts_ms: r.get(1)?,
					kind: r.get(2)?,
					actor: actor_uid.map(|uid| user(uid, actor_name.unwrap_or_default())),
					channel: r.get::<_, Option<i64>>(5)?.map(|c| c as u64),
					ref_id: r.get(6)?,
					text: r.get(7)?,
					data: data.and_then(|d| serde_json::from_str(&d).ok()).unwrap_or(Value::Null),
				})
			})?
			.collect::<rusqlite::Result<Vec<_>>>()?;
		let has_more = limit.is_some_and(|l| entries.len() > l as usize);
		if has_more {
			entries.pop();
		}
		Ok((entries, has_more))
	}

	pub fn prune_activity(&self, cutoff_ms: i64) -> rusqlite::Result<usize> {
		self.lock()
			.conn
			.prepare_cached("DELETE FROM activity WHERE ts_ms < ?1")?
			.execute([cutoff_ms])
	}
}
