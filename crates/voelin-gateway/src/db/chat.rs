//! Messages, reactions, pins and topics.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, ToSql, params};
use voelin_gateway_proto::{HistoryEntry, PinInfo, ReactionCount, TopicInfo, UserRef};
use voelin_model::{ChatMessage, ChatTarget};

use super::{Db, parse_target, target_key, user};

/// Columns of a message row, see [`entry_from_row`].
const MESSAGE_COLUMNS: &str = "m.id, m.target, m.ts_ms, m.author_uid, m.author_name, m.text, \
	m.topic_id, m.rev, p.message_id IS NOT NULL";
const MESSAGE_FROM: &str = "messages m LEFT JOIN pins p ON p.message_id = m.id";

fn entry_from_row(r: &Row) -> rusqlite::Result<HistoryEntry> {
	let target: String = r.get(1)?;
	Ok(HistoryEntry {
		id: r.get(0)?,
		message: ChatMessage {
			target: parse_target(&target),
			ts_ms: r.get(2)?,
			author_uid: r.get(3)?,
			author_name: r.get(4)?,
			author_id: None,
			text: r.get(5)?,
			via_relay: true,
		},
		topic_id: r.get(6)?,
		rev: r.get(7)?,
		pinned: r.get(8)?,
		reactions: Vec::new(),
	})
}

/// A history request after cursors and quotas were applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryOptions {
	pub before: Option<i64>,
	pub after: Option<i64>,
	pub before_ms: Option<i64>,
	pub after_ms: Option<i64>,
	/// `None`: no limit.
	pub limit: Option<u32>,
	pub topic: Option<i64>,
	pub exclude_topics: bool,
}

impl HistoryOptions {
	/// SQL for this shape of request (bound with named parameters).
	pub fn sql(&self) -> String {
		let mut conds = vec!["m.target = :target"];
		if self.topic.is_some() {
			conds.push("m.topic_id = :topic");
		} else if self.exclude_topics {
			conds.push("m.topic_id IS NULL");
		}
		if self.before.is_some() {
			conds.push("m.id < :before");
		}
		if self.after.is_some() {
			conds.push("m.id > :after");
		}
		let order = if self.after.is_some() { "ASC" } else { "DESC" };
		format!(
			"SELECT {MESSAGE_COLUMNS} FROM {MESSAGE_FROM} WHERE {} ORDER BY m.id {order} LIMIT :limit",
			conds.join(" AND ")
		)
	}
}

/// Add the reaction summary to entries (one range query).
fn add_reactions(
	conn: &Connection,
	entries: &mut [HistoryEntry],
	me: Option<&str>,
) -> rusqlite::Result<()> {
	let (Some(lo), Some(hi)) =
		(entries.iter().map(|e| e.id).min(), entries.iter().map(|e| e.id).max())
	else {
		return Ok(());
	};
	let index: HashMap<i64, usize> = entries.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
	let mut stmt = conn.prepare_cached(
		"SELECT message_id, emoji, COUNT(*), MAX(uid = ?3), MIN(ts_ms) AS first FROM reactions
		 WHERE message_id BETWEEN ?1 AND ?2 GROUP BY message_id, emoji ORDER BY message_id, first",
	)?;
	let mut rows = stmt.query(params![lo, hi, me.unwrap_or("")])?;
	while let Some(r) = rows.next()? {
		if let Some(&i) = index.get(&r.get::<_, i64>(0)?) {
			entries[i].reactions.push(ReactionCount {
				emoji: r.get(1)?,
				count: r.get(2)?,
				me: r.get(3)?,
			});
		}
	}
	Ok(())
}

fn topic_from_row(r: &Row) -> rusqlite::Result<TopicInfo> {
	let target: String = r.get(1)?;
	Ok(TopicInfo {
		id: r.get(0)?,
		target: parse_target(&target),
		title: r.get(2)?,
		creator: user(r.get(3)?, r.get(4)?),
		created_ms: r.get(5)?,
		root_message_id: r.get(6)?,
		last_activity_ms: r.get(7)?,
		message_count: r.get::<_, i64>(8)? as u64,
		archived: r.get(9)?,
	})
}

const TOPIC_COLUMNS: &str = "id, target, title, creator_uid, creator_name, created_ms, \
	root_message_id, last_activity_ms, message_count, archived";

impl Db {
	/// Store a message; returns it with its id and revision.
	pub fn add_message(
		&self,
		msg: ChatMessage,
		topic_id: Option<i64>,
	) -> rusqlite::Result<HistoryEntry> {
		Ok(self.add_messages(vec![(msg, topic_id)])?.pop().expect("one message"))
	}

	/// Store several messages in one transaction.
	pub fn add_messages(
		&self,
		msgs: Vec<(ChatMessage, Option<i64>)>,
	) -> rusqlite::Result<Vec<HistoryEntry>> {
		let mut g = self.lock();
		let now = super::now_ms();
		let revs: Vec<i64> = msgs.iter().map(|_| g.next_rev(now)).collect();
		let tx = g.conn.transaction()?;
		let mut out = Vec::with_capacity(msgs.len());
		{
			let mut insert = tx.prepare_cached(
				"INSERT INTO messages (target, ts_ms, author_uid, author_name, text, topic_id, rev)
				 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
			)?;
			let mut topic = tx.prepare_cached(
				"UPDATE topics SET message_count = message_count + 1,
					last_activity_ms = MAX(last_activity_ms, ?2) WHERE id = ?1",
			)?;
			for ((msg, topic_id), rev) in msgs.into_iter().zip(revs) {
				insert.execute(params![
					target_key(&msg.target),
					msg.ts_ms,
					msg.author_uid,
					msg.author_name,
					msg.text,
					topic_id,
					rev
				])?;
				if let Some(t) = topic_id {
					topic.execute(params![t, msg.ts_ms])?;
				}
				out.push(HistoryEntry {
					id: tx.last_insert_rowid(),
					message: msg,
					topic_id,
					reactions: Vec::new(),
					pinned: false,
					rev,
				});
			}
		}
		tx.commit()?;
		Ok(out)
	}

	/// One message with its reactions as `me` sees them.
	pub fn message(&self, id: i64, me: Option<&str>) -> rusqlite::Result<Option<HistoryEntry>> {
		let g = self.lock();
		let entry = g
			.conn
			.prepare_cached(&format!(
				"SELECT {MESSAGE_COLUMNS} FROM {MESSAGE_FROM} WHERE m.id = ?1"
			))?
			.query_row([id], entry_from_row)
			.optional()?;
		let Some(entry) = entry else { return Ok(None) };
		let mut entries = [entry];
		add_reactions(&g.conn, &mut entries, me)?;
		let [entry] = entries;
		Ok(Some(entry))
	}

	/// A page of history, oldest first, and whether there is more in the
	/// paging direction.
	pub fn history(
		&self,
		target: &ChatTarget,
		opts: &HistoryOptions,
		me: Option<&str>,
	) -> rusqlite::Result<(Vec<HistoryEntry>, bool)> {
		let g = self.lock();
		let key = target_key(target);
		let mut opts = opts.clone();
		// Time cursors become id cursors (ids follow arrival order).
		if let Some(after_ms) = opts.after_ms {
			let first: Option<i64> = g
				.conn
				.prepare_cached(
					"SELECT id FROM messages WHERE target = ?1 AND ts_ms > ?2
					 ORDER BY ts_ms, id LIMIT 1",
				)?
				.query_row(params![key, after_ms], |r| r.get(0))
				.optional()?;
			let Some(first) = first else { return Ok((Vec::new(), false)) };
			opts.after = Some(opts.after.map_or(first - 1, |a| a.max(first - 1)));
		}
		if let Some(before_ms) = opts.before_ms {
			let last: Option<i64> = g
				.conn
				.prepare_cached(
					"SELECT id FROM messages WHERE target = ?1 AND ts_ms < ?2
					 ORDER BY ts_ms DESC, id DESC LIMIT 1",
				)?
				.query_row(params![key, before_ms], |r| r.get(0))
				.optional()?;
			let Some(last) = last else { return Ok((Vec::new(), false)) };
			opts.before = Some(opts.before.map_or(last + 1, |b| b.min(last + 1)));
		}
		let limit = opts.limit.map_or(-1, |l| i64::from(l) + 1);
		let mut named: Vec<(&str, &dyn ToSql)> = vec![(":target", &key), (":limit", &limit)];
		if let Some(t) = &opts.topic {
			named.push((":topic", t));
		}
		if let Some(b) = &opts.before {
			named.push((":before", b));
		}
		if let Some(a) = &opts.after {
			named.push((":after", a));
		}
		let mut stmt = g.conn.prepare_cached(&opts.sql())?;
		let mut entries = stmt
			.query_map(named.as_slice(), entry_from_row)?
			.collect::<rusqlite::Result<Vec<_>>>()?;
		let has_more = opts.limit.is_some_and(|l| entries.len() > l as usize);
		if has_more {
			entries.pop();
		}
		if opts.after.is_none() {
			entries.reverse();
		}
		add_reactions(&g.conn, &mut entries, me)?;
		Ok((entries, has_more))
	}

	/// Messages of `target` new or changed after revision `since`, by revision.
	pub fn sync(
		&self,
		target: &ChatTarget,
		since: i64,
		limit: Option<u32>,
		me: Option<&str>,
	) -> rusqlite::Result<(Vec<HistoryEntry>, bool)> {
		let g = self.lock();
		let limit_sql = limit.map_or(-1, |l| i64::from(l) + 1);
		let mut entries = g
			.conn
			.prepare_cached(&format!(
				"SELECT {MESSAGE_COLUMNS} FROM {MESSAGE_FROM}
				 WHERE m.target = ?1 AND m.rev > ?2 ORDER BY m.rev LIMIT ?3"
			))?
			.query_map(params![target_key(target), since, limit_sql], entry_from_row)?
			.collect::<rusqlite::Result<Vec<_>>>()?;
		let has_more = limit.is_some_and(|l| entries.len() > l as usize);
		if has_more {
			entries.pop();
		}
		add_reactions(&g.conn, &mut entries, me)?;
		Ok((entries, has_more))
	}

	/// Bump a message's revision (something about it changed).
	fn touch(conn: &Connection, id: i64, rev: i64) -> rusqlite::Result<()> {
		conn.prepare_cached("UPDATE messages SET rev = ?2 WHERE id = ?1")?
			.execute(params![id, rev])?;
		Ok(())
	}

	/// Delete messages sent before `cutoff_ms`, with their reactions. Pinned
	/// messages are kept. Topics without activity since then go too.
	pub fn prune_messages(&self, cutoff_ms: i64) -> rusqlite::Result<usize> {
		let mut g = self.lock();
		let tx = g.conn.transaction()?;
		tx.execute(
			"DELETE FROM reactions WHERE message_id IN (SELECT id FROM messages
				WHERE ts_ms < ?1 AND id NOT IN (SELECT message_id FROM pins))",
			[cutoff_ms],
		)?;
		let n = tx.execute(
			"DELETE FROM messages WHERE ts_ms < ?1 AND id NOT IN (SELECT message_id FROM pins)",
			[cutoff_ms],
		)?;
		tx.execute("DELETE FROM topics WHERE last_activity_ms < ?1", [cutoff_ms])?;
		tx.commit()?;
		Ok(n)
	}

	// Reactions

	/// Add a reaction; returns the new count, or `None` if it was there.
	pub fn add_reaction(
		&self,
		message_id: i64,
		emoji: &str,
		by: &UserRef,
		now_ms: i64,
	) -> rusqlite::Result<Option<u32>> {
		let mut g = self.lock();
		let rev = g.next_rev(now_ms);
		let tx = g.conn.transaction()?;
		let added = tx
			.prepare_cached(
				"INSERT OR IGNORE INTO reactions (message_id, emoji, uid, name, ts_ms)
				 VALUES (?1, ?2, ?3, ?4, ?5)",
			)?
			.execute(params![message_id, emoji, by.uid, by.name, now_ms])?;
		if added == 0 {
			return Ok(None);
		}
		Self::touch(&tx, message_id, rev)?;
		let count = reaction_count(&tx, message_id, emoji)?;
		tx.commit()?;
		Ok(Some(count))
	}

	/// Remove a reaction; returns the new count, or `None` if there was none.
	pub fn remove_reaction(
		&self,
		message_id: i64,
		emoji: &str,
		uid: &str,
		now_ms: i64,
	) -> rusqlite::Result<Option<u32>> {
		let mut g = self.lock();
		let rev = g.next_rev(now_ms);
		let tx = g.conn.transaction()?;
		let removed = tx
			.prepare_cached(
				"DELETE FROM reactions WHERE message_id = ?1 AND emoji = ?2 AND uid = ?3",
			)?
			.execute(params![message_id, emoji, uid])?;
		if removed == 0 {
			return Ok(None);
		}
		Self::touch(&tx, message_id, rev)?;
		let count = reaction_count(&tx, message_id, emoji)?;
		tx.commit()?;
		Ok(Some(count))
	}

	/// Different emoji on a message.
	pub fn reaction_kinds(&self, message_id: i64) -> rusqlite::Result<u64> {
		self.lock()
			.conn
			.prepare_cached("SELECT COUNT(DISTINCT emoji) FROM reactions WHERE message_id = ?1")?
			.query_row([message_id], |r| r.get::<_, i64>(0))
			.map(|n| n as u64)
	}

	/// Whether `uid` reacted with `emoji`.
	pub fn has_reaction(&self, message_id: i64, emoji: &str, uid: &str) -> rusqlite::Result<bool> {
		self.lock()
			.conn
			.prepare_cached(
				"SELECT 1 FROM reactions WHERE message_id = ?1 AND emoji = ?2 AND uid = ?3",
			)?
			.exists(params![message_id, emoji, uid])
	}

	/// Who reacted with `emoji`, first first.
	pub fn reactors(&self, message_id: i64, emoji: &str) -> rusqlite::Result<Vec<UserRef>> {
		let g = self.lock();
		let mut stmt = g.conn.prepare_cached(
			"SELECT uid, name FROM reactions WHERE message_id = ?1 AND emoji = ?2 ORDER BY ts_ms",
		)?;
		stmt.query_map(params![message_id, emoji], |r| Ok(user(r.get(0)?, r.get(1)?)))?.collect()
	}

	// Pins

	/// Pin a message; `false` if it already was.
	pub fn pin(
		&self,
		message_id: i64,
		target: &ChatTarget,
		by: &UserRef,
		now_ms: i64,
	) -> rusqlite::Result<bool> {
		let mut g = self.lock();
		let rev = g.next_rev(now_ms);
		let tx = g.conn.transaction()?;
		let added = tx
			.prepare_cached(
				"INSERT OR IGNORE INTO pins (message_id, target, by_uid, by_name, ts_ms)
				 VALUES (?1, ?2, ?3, ?4, ?5)",
			)?
			.execute(params![message_id, target_key(target), by.uid, by.name, now_ms])?;
		if added > 0 {
			Self::touch(&tx, message_id, rev)?;
		}
		tx.commit()?;
		Ok(added > 0)
	}

	/// Unpin a message; returns who had pinned it.
	pub fn unpin(&self, message_id: i64, now_ms: i64) -> rusqlite::Result<Option<UserRef>> {
		let mut g = self.lock();
		let rev = g.next_rev(now_ms);
		let tx = g.conn.transaction()?;
		let by = tx
			.prepare_cached("DELETE FROM pins WHERE message_id = ?1 RETURNING by_uid, by_name")?
			.query_row([message_id], |r| Ok(user(r.get(0)?, r.get(1)?)))
			.optional()?;
		if by.is_some() {
			Self::touch(&tx, message_id, rev)?;
		}
		tx.commit()?;
		Ok(by)
	}

	pub fn pin_info(&self, message_id: i64, me: Option<&str>) -> rusqlite::Result<Option<PinInfo>> {
		let pin = self
			.lock()
			.conn
			.prepare_cached("SELECT by_uid, by_name, ts_ms FROM pins WHERE message_id = ?1")?
			.query_row([message_id], |r| Ok((user(r.get(0)?, r.get(1)?), r.get::<_, i64>(2)?)))
			.optional()?;
		let Some((by, ts_ms)) = pin else { return Ok(None) };
		Ok(self.message(message_id, me)?.map(|entry| PinInfo { entry, by, ts_ms }))
	}

	/// Pins of a chat, newest first.
	pub fn pins(&self, target: &ChatTarget, me: Option<&str>) -> rusqlite::Result<Vec<PinInfo>> {
		let g = self.lock();
		let mut stmt = g.conn.prepare_cached(&format!(
			"SELECT {MESSAGE_COLUMNS}, p.by_uid, p.by_name, p.ts_ms
			 FROM pins p JOIN messages m ON m.id = p.message_id
			 WHERE p.target = ?1 ORDER BY p.ts_ms DESC"
		))?;
		let rows = stmt.query_map([target_key(target)], |r| {
			Ok((entry_from_row(r)?, user(r.get(9)?, r.get(10)?), r.get::<_, i64>(11)?))
		})?;
		let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
		let mut entries: Vec<HistoryEntry> = rows.iter().map(|(e, ..)| e.clone()).collect();
		add_reactions(&g.conn, &mut entries, me)?;
		Ok(entries
			.into_iter()
			.zip(rows)
			.map(|(entry, (_, by, ts_ms))| PinInfo { entry, by, ts_ms })
			.collect())
	}

	pub fn pin_count(&self, target: &ChatTarget) -> rusqlite::Result<u64> {
		self.lock()
			.conn
			.prepare_cached("SELECT COUNT(*) FROM pins WHERE target = ?1")?
			.query_row([target_key(target)], |r| r.get::<_, i64>(0))
			.map(|n| n as u64)
	}

	// Topics

	pub fn create_topic(
		&self,
		target: &ChatTarget,
		title: &str,
		creator: &UserRef,
		root_message_id: Option<i64>,
		now_ms: i64,
	) -> rusqlite::Result<TopicInfo> {
		let g = self.lock();
		g.conn
			.prepare_cached(
				"INSERT INTO topics (target, title, creator_uid, creator_name, created_ms,
					root_message_id, last_activity_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?5)",
			)?
			.execute(params![
				target_key(target),
				title,
				creator.uid,
				creator.name,
				now_ms,
				root_message_id
			])?;
		Ok(TopicInfo {
			id: g.conn.last_insert_rowid(),
			target: target.clone(),
			title: title.to_string(),
			creator: creator.clone(),
			created_ms: now_ms,
			root_message_id,
			last_activity_ms: now_ms,
			message_count: 0,
			archived: false,
		})
	}

	pub fn topic(&self, id: i64) -> rusqlite::Result<Option<TopicInfo>> {
		self.lock()
			.conn
			.prepare_cached(&format!("SELECT {TOPIC_COLUMNS} FROM topics WHERE id = ?1"))?
			.query_row([id], topic_from_row)
			.optional()
	}

	pub fn update_topic(
		&self,
		id: i64,
		title: Option<&str>,
		archived: Option<bool>,
	) -> rusqlite::Result<Option<TopicInfo>> {
		self.lock()
			.conn
			.prepare_cached(
				"UPDATE topics SET title = COALESCE(?2, title), archived = COALESCE(?3, archived)
				 WHERE id = ?1",
			)?
			.execute(params![id, title, archived])?;
		self.topic(id)
	}

	/// Most recently active first.
	pub fn topics(
		&self,
		target: &ChatTarget,
		include_archived: bool,
	) -> rusqlite::Result<Vec<TopicInfo>> {
		let g = self.lock();
		let mut stmt = g.conn.prepare_cached(&format!(
			"SELECT {TOPIC_COLUMNS} FROM topics WHERE target = ?1 AND (?2 OR archived = 0)
			 ORDER BY last_activity_ms DESC"
		))?;
		stmt.query_map(params![target_key(target), include_archived], topic_from_row)?.collect()
	}

	/// Open topics in a chat.
	pub fn open_topic_count(&self, target: &ChatTarget) -> rusqlite::Result<u64> {
		self.lock()
			.conn
			.prepare_cached("SELECT COUNT(*) FROM topics WHERE target = ?1 AND archived = 0")?
			.query_row([target_key(target)], |r| r.get::<_, i64>(0))
			.map(|n| n as u64)
	}

	/// The most recently active open topic with this title.
	pub fn topic_by_title(
		&self,
		target: &ChatTarget,
		title: &str,
	) -> rusqlite::Result<Option<TopicInfo>> {
		self.lock()
			.conn
			.prepare_cached(&format!(
				"SELECT {TOPIC_COLUMNS} FROM topics WHERE target = ?1 AND title = ?2 AND archived = 0
				 ORDER BY last_activity_ms DESC LIMIT 1"
			))?
			.query_row(params![target_key(target), title], topic_from_row)
			.optional()
	}
}

fn reaction_count(conn: &Connection, message_id: i64, emoji: &str) -> rusqlite::Result<u32> {
	conn.prepare_cached("SELECT COUNT(*) FROM reactions WHERE message_id = ?1 AND emoji = ?2")?
		.query_row(params![message_id, emoji], |r| r.get(0))
}
