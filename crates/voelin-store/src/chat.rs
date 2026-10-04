//! Chat history: the messages this device saw or fetched from a gateway,
//! per server and chat.
//!
//! Every message is one row, whatever delivered it: the voice connection,
//! a gateway (with its stable message id, [`StoredMessage::remote_id`]), a
//! query relay, or this device when it sent the message. The same message
//! often arrives twice (live over voice and again from the gateway);
//! [`Store::write_messages`] merges such copies (see [`WriteOutcome`]):
//!
//! - a gateway copy matches the row with its gateway id, else a row without
//!   one of the same message;
//! - otherwise two copies are the same message when they come from different
//!   sources, their text is equal once a relay prefix (`[nick] `) is removed,
//!   they were stamped at most `tolerance_ms` apart, and they have the same
//!   author: the same unique id, or the relay prefix of one names the author
//!   of the other, or (without both unique ids) the same nickname.
//!
//! Pages are by time: `(ts_ms, id)` orders a chat ([`PageQuery`]). A
//! [`ChatCursor`] remembers how far a chat is synced with a gateway.

use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::{Result, Store};

/// Where a chat message was posted.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChatTarget {
	Server,
	Channel(u64),
	/// Private chat with the client of this unique id.
	Private(String),
}

impl ChatTarget {
	/// The form stored in the database: `server`, `channel/<cid>`, `private/<uid>`.
	pub fn key(&self) -> String {
		match self {
			ChatTarget::Server => "server".into(),
			ChatTarget::Channel(cid) => format!("channel/{cid}"),
			ChatTarget::Private(uid) => format!("private/{uid}"),
		}
	}

	/// Parse [`Self::key`]; unknown forms are the server chat.
	pub fn from_key(key: &str) -> Self {
		match key.split_once('/') {
			Some(("channel", cid)) => {
				cid.parse().map(ChatTarget::Channel).unwrap_or(ChatTarget::Server)
			}
			Some(("private", uid)) => ChatTarget::Private(uid.to_owned()),
			_ => ChatTarget::Server,
		}
	}
}

/// What delivered a message first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageSource {
	/// Our own voice connection.
	Voice,
	/// A `tsgw` gateway (live or from its history).
	Gateway,
	/// A query relay with the user's own credentials.
	Query,
	/// This device: sent by the user, or stored before sources were recorded.
	#[default]
	Local,
}

impl MessageSource {
	pub fn as_str(self) -> &'static str {
		match self {
			MessageSource::Voice => "voice",
			MessageSource::Gateway => "gateway",
			MessageSource::Query => "query",
			MessageSource::Local => "local",
		}
	}

	pub fn parse(s: &str) -> Self {
		match s {
			"voice" => MessageSource::Voice,
			"gateway" => MessageSource::Gateway,
			"query" => MessageSource::Query,
			_ => MessageSource::Local,
		}
	}

	/// Bit in the `seen_by` column.
	fn bit(self) -> i64 {
		match self {
			MessageSource::Voice => 1,
			MessageSource::Gateway => 2,
			MessageSource::Query => 4,
			MessageSource::Local => 8,
		}
	}
}

/// Whether a row is known to the gateway.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SyncState {
	/// Only seen by this device (no gateway copy yet).
	#[default]
	Local,
	/// The gateway has it: [`StoredMessage::remote_id`] is set.
	Synced,
}

fn is_false(b: &bool) -> bool {
	!*b
}

/// One emoji on a message, as the gateway counts it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
	pub emoji: String,
	pub count: u32,
	/// The user reacted with this emoji.
	#[serde(default, skip_serializing_if = "is_false")]
	pub me: bool,
}

/// What a gateway says about a message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteInfo {
	/// The gateway's message id.
	pub id: i64,
	/// Revision of the last change at the gateway.
	pub rev: i64,
	pub topic_id: Option<i64>,
	pub reactions: Vec<Reaction>,
	pub pinned: bool,
}

/// A message to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMessage {
	pub server_uid: String,
	pub target: ChatTarget,
	/// Unix milliseconds.
	pub ts_ms: i64,
	pub author_uid: Option<String>,
	pub author_name: String,
	/// The author's client id on the server, if known.
	pub author_id: Option<u16>,
	/// Carried by a relay rather than our own connection.
	pub via_relay: bool,
	pub text: String,
	pub source: MessageSource,
	/// Set for messages from a gateway.
	pub remote: Option<RemoteInfo>,
}

/// A stored message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMessage {
	/// Assigned by the store; stable on this device.
	pub id: i64,
	pub server_uid: String,
	pub target: ChatTarget,
	/// Unix milliseconds.
	pub ts_ms: i64,
	pub author_uid: Option<String>,
	pub author_name: String,
	pub author_id: Option<u16>,
	pub via_relay: bool,
	pub text: String,
	/// What delivered it first.
	pub source: MessageSource,
	/// The gateway's message id, once a gateway copy was stored.
	pub remote_id: Option<i64>,
	pub topic_id: Option<i64>,
	/// Gateway revision of the last change (0 without a gateway copy).
	pub rev: i64,
	pub reactions: Vec<Reaction>,
	pub pinned: bool,
	pub sync_state: SyncState,
}

/// What [`Store::write_messages`] did with one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WriteOutcome {
	/// Stored as a new row.
	Inserted,
	/// Another copy of a stored message (see the [module docs](self)); the
	/// row gained what this copy adds, e.g. the gateway id.
	Merged,
	/// A gateway message stored before, with changes (pin, reactions, topic).
	Updated,
	/// Nothing new.
	Unchanged,
}

/// A stored message and what the write did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
	pub message: StoredMessage,
	pub outcome: WriteOutcome,
}

/// A window of a chat, by `(ts_ms, id)`. Pages are always oldest first.
///
/// Without `after`: the newest `limit` messages before `before` (the newest
/// of the chat without it). With `after`: the oldest `limit` messages after
/// it, up to `before` if given. `limit: None` is everything in range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageQuery {
	pub before: Option<(i64, i64)>,
	pub after: Option<(i64, i64)>,
	pub limit: Option<usize>,
}

/// How far a chat is synced with a gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatCursor {
	/// The gateway the revision belongs to; another gateway starts over.
	pub gateway_id: String,
	/// The last gateway revision stored (`sync since_rev`).
	pub rev: i64,
	/// The gateway has nothing older than the oldest message stored from it.
	pub complete: bool,
	/// Unix milliseconds of the last sync.
	pub updated_ms: i64,
}

/// Version 2: times in milliseconds, sources, gateway ids and metadata,
/// dedupe keys, sync cursors and server aliases. Old messages keep their
/// data (source `local`, their second times in milliseconds).
pub(crate) fn migrate_2(tx: &Transaction) -> rusqlite::Result<()> {
	tx.execute_batch(
		"ALTER TABLE messages ADD COLUMN ts_ms INTEGER NOT NULL DEFAULT 0;
		UPDATE messages SET ts_ms = ts * 1000;
		ALTER TABLE messages ADD COLUMN author_id INTEGER;
		ALTER TABLE messages ADD COLUMN source TEXT NOT NULL DEFAULT 'local';
		ALTER TABLE messages ADD COLUMN remote_id INTEGER;
		ALTER TABLE messages ADD COLUMN topic_id INTEGER;
		ALTER TABLE messages ADD COLUMN rev INTEGER NOT NULL DEFAULT 0;
		ALTER TABLE messages ADD COLUMN sync_state INTEGER NOT NULL DEFAULT 0;
		ALTER TABLE messages ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
		ALTER TABLE messages ADD COLUMN reactions TEXT;
		-- Sources that delivered the row (MessageSource bits), for dedupe.
		ALTER TABLE messages ADD COLUMN seen_by INTEGER NOT NULL DEFAULT 8;
		-- FNV-1a of the text without a relay prefix, for dedupe.
		ALTER TABLE messages ADD COLUMN text_hash INTEGER NOT NULL DEFAULT 0;
		DROP INDEX messages_by_target;
		CREATE INDEX messages_by_time ON messages (server_uid, target, ts_ms, id);
		CREATE UNIQUE INDEX messages_by_remote ON messages (server_uid, target, remote_id)
			WHERE remote_id IS NOT NULL;
		CREATE TABLE chat_cursors (
			server_uid TEXT NOT NULL,
			target TEXT NOT NULL,
			gateway_id TEXT NOT NULL,
			rev INTEGER NOT NULL DEFAULT 0,
			complete INTEGER NOT NULL DEFAULT 0,
			updated_ms INTEGER NOT NULL DEFAULT 0,
			PRIMARY KEY (server_uid, target)
		) WITHOUT ROWID;
		CREATE TABLE server_aliases (
			alias TEXT PRIMARY KEY,
			server_uid TEXT NOT NULL,
			updated_ms INTEGER NOT NULL DEFAULT 0
		) WITHOUT ROWID;",
	)?;
	let rows: Vec<(i64, String)> = tx
		.prepare("SELECT id, text FROM messages")?
		.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
		.collect::<rusqlite::Result<_>>()?;
	let mut update = tx.prepare("UPDATE messages SET text_hash = ?2 WHERE id = ?1")?;
	for (id, text) in rows {
		update.execute(params![id, text_hash(&text)])?;
	}
	Ok(())
}

/// A relay posts as `[nick] text`: the nick and the text.
fn split_relay(text: &str) -> (Option<&str>, &str) {
	if let Some(rest) = text.strip_prefix('[')
		&& let Some((nick, after)) = rest.split_once("] ")
		&& !nick.is_empty()
	{
		return (Some(nick), after);
	}
	(None, text)
}

/// FNV-1a (64 bit) of the text without a relay prefix.
fn text_hash(text: &str) -> i64 {
	let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
	for byte in split_relay(text).1.bytes() {
		hash ^= u64::from(byte);
		hash = hash.wrapping_mul(0x0100_0000_01b3);
	}
	hash as i64
}

/// Whether a stored row and a new copy are the same message (the hash and
/// time already matched).
fn same_message(new: &NewMessage, old: &StoredMessage) -> bool {
	let (new_prefix, new_text) = split_relay(&new.text);
	let (old_prefix, old_text) = split_relay(&old.text);
	if new_text != old_text {
		return false;
	}
	if let (Some(a), Some(b)) = (&new.author_uid, &old.author_uid)
		&& a == b
	{
		return true;
	}
	if new_prefix.is_some_and(|n| n == old.author_name)
		|| old_prefix.is_some_and(|n| n == new.author_name)
	{
		return true;
	}
	(new.author_uid.is_none() || old.author_uid.is_none()) && new.author_name == old.author_name
}

const COLUMNS: &str = "id, server_uid, target, ts_ms, author_uid, author_name, author_id, \
	via_relay, text, source, remote_id, topic_id, rev, reactions, pinned, sync_state";

fn reactions_json(reactions: &[Reaction]) -> Option<String> {
	(!reactions.is_empty()).then(|| serde_json::to_string(reactions).unwrap_or_default())
}

fn message_from_row(r: &Row) -> rusqlite::Result<StoredMessage> {
	let target: String = r.get(2)?;
	let source: String = r.get(9)?;
	let reactions: Option<String> = r.get(13)?;
	Ok(StoredMessage {
		id: r.get(0)?,
		server_uid: r.get(1)?,
		target: ChatTarget::from_key(&target),
		ts_ms: r.get(3)?,
		author_uid: r.get(4)?,
		author_name: r.get(5)?,
		author_id: r.get(6)?,
		via_relay: r.get(7)?,
		text: r.get(8)?,
		source: MessageSource::parse(&source),
		remote_id: r.get(10)?,
		topic_id: r.get(11)?,
		rev: r.get(12)?,
		reactions: reactions.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
		pinned: r.get(14)?,
		sync_state: if r.get::<_, i64>(15)? == 1 { SyncState::Synced } else { SyncState::Local },
	})
}

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or_default()
}

/// Upsert one message inside `tx`.
fn write_one(tx: &Transaction, m: &NewMessage, tolerance_ms: i64) -> rusqlite::Result<Written> {
	let target = m.target.key();
	let by_id = |id: i64| -> rusqlite::Result<StoredMessage> {
		tx.prepare_cached(&format!("SELECT {COLUMNS} FROM messages WHERE id = ?1"))?
			.query_row([id], message_from_row)
	};
	if let Some(remote) = &m.remote {
		// The same gateway message again.
		let existing = tx
			.prepare_cached(&format!(
				"SELECT {COLUMNS} FROM messages
				 WHERE server_uid = ?1 AND target = ?2 AND remote_id = ?3"
			))?
			.query_row(params![m.server_uid, target, remote.id], message_from_row)
			.optional()?;
		if let Some(old) = existing {
			let changed = old.rev != remote.rev
				|| old.topic_id != remote.topic_id
				|| old.pinned != remote.pinned
				|| old.reactions != remote.reactions;
			if !changed {
				return Ok(Written { message: old, outcome: WriteOutcome::Unchanged });
			}
			tx.prepare_cached(
				"UPDATE messages SET rev = ?2, topic_id = ?3, pinned = ?4, reactions = ?5 WHERE id = ?1",
			)?
			.execute(params![
				old.id,
				remote.rev,
				remote.topic_id,
				remote.pinned,
				reactions_json(&remote.reactions)
			])?;
			return Ok(Written { message: by_id(old.id)?, outcome: WriteOutcome::Updated });
		}
	}
	// Another copy of a message stored from a different source.
	let hash = text_hash(&m.text);
	let duplicate = {
		let mut stmt = tx.prepare_cached(&format!(
			"SELECT {COLUMNS} FROM messages
			 WHERE server_uid = ?1 AND target = ?2 AND ts_ms BETWEEN ?3 AND ?4
			   AND text_hash = ?5 AND (seen_by & ?6) = 0 AND (?7 = 0 OR remote_id IS NULL)
			 ORDER BY ABS(ts_ms - ?8), id"
		))?;
		let rows = stmt.query_map(
			params![
				m.server_uid,
				target,
				m.ts_ms.saturating_sub(tolerance_ms),
				m.ts_ms.saturating_add(tolerance_ms),
				hash,
				m.source.bit(),
				m.remote.is_some(),
				m.ts_ms
			],
			message_from_row,
		)?;
		let mut found = None;
		for row in rows {
			let row = row?;
			if same_message(m, &row) {
				found = Some(row);
				break;
			}
		}
		found
	};
	if let Some(old) = duplicate {
		let remote = m.remote.clone().unwrap_or_default();
		tx.prepare_cached(
			"UPDATE messages SET seen_by = seen_by | ?2,
				author_uid = COALESCE(author_uid, ?3), author_id = COALESCE(author_id, ?4),
				remote_id = COALESCE(?5, remote_id), rev = MAX(rev, ?6),
				topic_id = COALESCE(?7, topic_id), pinned = CASE WHEN ?5 IS NULL THEN pinned ELSE ?8 END,
				reactions = CASE WHEN ?5 IS NULL THEN reactions ELSE ?9 END,
				sync_state = CASE WHEN ?5 IS NULL THEN sync_state ELSE 1 END
			 WHERE id = ?1",
		)?
		.execute(params![
			old.id,
			m.source.bit(),
			m.author_uid,
			m.author_id,
			m.remote.as_ref().map(|r| r.id),
			remote.rev,
			remote.topic_id,
			remote.pinned,
			reactions_json(&remote.reactions)
		])?;
		let message = by_id(old.id)?;
		let outcome = if message == old { WriteOutcome::Unchanged } else { WriteOutcome::Merged };
		return Ok(Written { message, outcome });
	}
	let remote = m.remote.clone().unwrap_or_default();
	tx.prepare_cached(
		"INSERT INTO messages (server_uid, target, ts, ts_ms, author_uid, author_name, author_id,
			via_relay, text, source, remote_id, topic_id, rev, reactions, pinned, sync_state,
			seen_by, text_hash)
		 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
	)?
	.execute(params![
		m.server_uid,
		target,
		m.ts_ms.div_euclid(1000),
		m.ts_ms,
		m.author_uid,
		m.author_name,
		m.author_id,
		m.via_relay,
		m.text,
		m.source.as_str(),
		m.remote.as_ref().map(|r| r.id),
		remote.topic_id,
		remote.rev,
		reactions_json(&remote.reactions),
		remote.pinned,
		i64::from(m.remote.is_some()),
		m.source.bit(),
		hash
	])?;
	let id = tx.last_insert_rowid();
	Ok(Written { message: by_id(id)?, outcome: WriteOutcome::Inserted })
}

impl Store {
	/// Store messages in one transaction, merging copies of the same
	/// message (see the [module docs](self)); results in input order.
	pub fn write_messages(
		&mut self,
		messages: &[NewMessage],
		tolerance_ms: i64,
	) -> Result<Vec<Written>> {
		let tx = self.db.transaction()?;
		let mut out = Vec::with_capacity(messages.len());
		for m in messages {
			out.push(write_one(&tx, m, tolerance_ms.max(0))?);
		}
		tx.commit()?;
		Ok(out)
	}

	/// A page of a chat, oldest first (see [`PageQuery`]).
	pub fn messages(
		&self,
		server_uid: &str,
		target: &ChatTarget,
		query: PageQuery,
	) -> Result<Vec<StoredMessage>> {
		let (before_ts, before_id) = query.before.unwrap_or((i64::MAX, i64::MAX));
		let (after_ts, after_id) = query.after.unwrap_or((i64::MIN, i64::MIN));
		let ascending = query.after.is_some();
		let sql = format!(
			"SELECT {COLUMNS} FROM messages
			 WHERE server_uid = ?1 AND target = ?2 AND (ts_ms, id) < (?3, ?4) AND (ts_ms, id) > (?5, ?6)
			 ORDER BY ts_ms {order}, id {order} LIMIT ?7",
			order = if ascending { "ASC" } else { "DESC" }
		);
		let limit = query.limit.map_or(-1, |l| l as i64);
		let mut rows = self
			.db
			.prepare_cached(&sql)?
			.query_map(
				params![server_uid, target.key(), before_ts, before_id, after_ts, after_id, limit],
				message_from_row,
			)?
			.collect::<rusqlite::Result<Vec<_>>>()?;
		if !ascending {
			rows.reverse();
		}
		Ok(rows)
	}

	/// The newest message of every chat (of every server), the most
	/// recently active chat first, at most `limit` chats (`None`: all).
	pub fn recent_chats(&self, limit: Option<usize>) -> Result<Vec<StoredMessage>> {
		// SQLite takes the bare columns from the row with the MAX.
		// ponytail: one pass over the time index; keep a table of each
		// chat's last message if histories grow into the millions.
		let sql = format!(
			"SELECT {COLUMNS}, MAX(ts_ms) FROM messages WHERE server_uid <> ''
			 GROUP BY server_uid, target ORDER BY ts_ms DESC, id DESC LIMIT ?1"
		);
		let limit = limit.map_or(-1, |l| l as i64);
		Ok(self
			.db
			.prepare_cached(&sql)?
			.query_map([limit], message_from_row)?
			.collect::<rusqlite::Result<Vec<_>>>()?)
	}

	/// One message by its local id.
	pub fn message(&self, id: i64) -> Result<Option<StoredMessage>> {
		Ok(self
			.db
			.prepare_cached(&format!("SELECT {COLUMNS} FROM messages WHERE id = ?1"))?
			.query_row([id], message_from_row)
			.optional()?)
	}

	/// One message by its gateway id.
	pub fn message_by_remote(
		&self,
		server_uid: &str,
		target: &ChatTarget,
		remote_id: i64,
	) -> Result<Option<StoredMessage>> {
		Ok(self
			.db
			.prepare_cached(&format!(
				"SELECT {COLUMNS} FROM messages
				 WHERE server_uid = ?1 AND target = ?2 AND remote_id = ?3"
			))?
			.query_row(params![server_uid, target.key(), remote_id], message_from_row)
			.optional()?)
	}

	/// The oldest time of a message stored from the gateway, if any.
	pub fn oldest_remote_ts(&self, server_uid: &str, target: &ChatTarget) -> Result<Option<i64>> {
		Ok(self
			.db
			.prepare_cached(
				"SELECT MIN(ts_ms) FROM messages
				 WHERE server_uid = ?1 AND target = ?2 AND remote_id IS NOT NULL",
			)?
			.query_row(params![server_uid, target.key()], |r| r.get(0))?)
	}

	/// Mark a gateway message (un)pinned; the updated row, `None` if it is
	/// not stored.
	pub fn set_pinned(
		&mut self,
		server_uid: &str,
		target: &ChatTarget,
		remote_id: i64,
		pinned: bool,
	) -> Result<Option<StoredMessage>> {
		self.db
			.prepare_cached(
				"UPDATE messages SET pinned = ?4
				 WHERE server_uid = ?1 AND target = ?2 AND remote_id = ?3",
			)?
			.execute(params![server_uid, target.key(), remote_id, pinned])?;
		self.message_by_remote(server_uid, target, remote_id)
	}

	/// Set the count of one emoji on a gateway message (0 removes it) and,
	/// if given, whether the user reacted with it. The updated row, `None`
	/// if the message is not stored.
	pub fn set_reaction(
		&mut self,
		server_uid: &str,
		target: &ChatTarget,
		remote_id: i64,
		emoji: &str,
		count: u32,
		me: Option<bool>,
	) -> Result<Option<StoredMessage>> {
		let tx = self.db.transaction()?;
		let current: Option<(i64, Option<String>)> = tx
			.prepare_cached(
				"SELECT id, reactions FROM messages
				 WHERE server_uid = ?1 AND target = ?2 AND remote_id = ?3",
			)?
			.query_row(params![server_uid, target.key(), remote_id], |r| Ok((r.get(0)?, r.get(1)?)))
			.optional()?;
		let Some((id, json)) = current else { return Ok(None) };
		let mut reactions: Vec<Reaction> =
			json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
		match reactions.iter().position(|r| r.emoji == emoji) {
			_ if count == 0 => reactions.retain(|r| r.emoji != emoji),
			Some(i) => {
				reactions[i].count = count;
				if let Some(me) = me {
					reactions[i].me = me;
				}
			}
			None => {
				reactions.push(Reaction { emoji: emoji.to_owned(), count, me: me.unwrap_or(false) })
			}
		}
		tx.prepare_cached("UPDATE messages SET reactions = ?2 WHERE id = ?1")?
			.execute(params![id, reactions_json(&reactions)])?;
		tx.commit()?;
		self.message(id)
	}

	/// How far a chat is synced with a gateway.
	pub fn chat_cursor(&self, server_uid: &str, target: &ChatTarget) -> Result<Option<ChatCursor>> {
		Ok(self
			.db
			.prepare_cached(
				"SELECT gateway_id, rev, complete, updated_ms FROM chat_cursors
				 WHERE server_uid = ?1 AND target = ?2",
			)?
			.query_row(params![server_uid, target.key()], |r| {
				Ok(ChatCursor {
					gateway_id: r.get(0)?,
					rev: r.get(1)?,
					complete: r.get(2)?,
					updated_ms: r.get(3)?,
				})
			})
			.optional()?)
	}

	pub fn set_chat_cursor(
		&self,
		server_uid: &str,
		target: &ChatTarget,
		cursor: &ChatCursor,
	) -> Result<()> {
		self.db
			.prepare_cached(
				"INSERT INTO chat_cursors (server_uid, target, gateway_id, rev, complete, updated_ms)
				 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
				 ON CONFLICT (server_uid, target) DO UPDATE SET gateway_id = excluded.gateway_id,
					rev = excluded.rev, complete = excluded.complete, updated_ms = excluded.updated_ms",
			)?
			.execute(params![
				server_uid,
				target.key(),
				cursor.gateway_id,
				cursor.rev,
				cursor.complete,
				cursor.updated_ms
			])?;
		Ok(())
	}

	/// Forget the gateway ids of a chat (another gateway numbers its
	/// messages differently) and its cursor; the rows stay, as seen by this
	/// device, and are matched again with the new gateway's copies.
	pub fn forget_remote_ids(&mut self, server_uid: &str, target: &ChatTarget) -> Result<usize> {
		let tx = self.db.transaction()?;
		let n = tx
			.prepare_cached(
				"UPDATE messages SET remote_id = NULL, rev = 0, sync_state = 0,
					seen_by = CASE WHEN seen_by = 2 THEN 8 ELSE seen_by & ~2 END
				 WHERE server_uid = ?1 AND target = ?2 AND remote_id IS NOT NULL",
			)?
			.execute(params![server_uid, target.key()])?;
		tx.prepare_cached("DELETE FROM chat_cursors WHERE server_uid = ?1 AND target = ?2")?
			.execute(params![server_uid, target.key()])?;
		tx.commit()?;
		Ok(n)
	}

	/// The server unique id last seen under another name of the server
	/// (e.g. `voice:<address>`, `gateway:<url>`).
	pub fn server_alias(&self, alias: &str) -> Result<Option<String>> {
		Ok(self
			.db
			.prepare_cached("SELECT server_uid FROM server_aliases WHERE alias = ?1")?
			.query_row([alias], |r| r.get(0))
			.optional()?)
	}

	pub fn set_server_alias(&self, alias: &str, server_uid: &str) -> Result<()> {
		self.db
			.prepare_cached(
				"INSERT INTO server_aliases (alias, server_uid, updated_ms) VALUES (?1, ?2, ?3)
				 ON CONFLICT (alias) DO UPDATE SET server_uid = excluded.server_uid,
					updated_ms = excluded.updated_ms",
			)?
			.execute(params![alias, server_uid, now_ms()])?;
		Ok(())
	}

	/// Delete messages sent before `before_ms` (Unix milliseconds); pinned
	/// ones stay.
	pub fn prune_messages(&self, before_ms: i64) -> Result<usize> {
		Ok(self
			.db
			.prepare_cached(
				"DELETE FROM messages WHERE ts_ms < ?1 AND pinned = 0 AND server_uid <> ''",
			)?
			.execute([before_ms])?)
	}

	/// Give new messages ids from `first` on (e.g. negative ones, for a
	/// database whose ids must not be confused with another's). SQLite
	/// numbers a new row one above the largest id, so this keeps a hidden
	/// row (no server) just below `first`. For an empty chat table.
	pub fn start_message_ids_at(&self, first: i64) -> Result<()> {
		self.db.execute(
			"INSERT INTO messages (id, server_uid, target, ts, author_name, text)
			 VALUES (?1, '', '', 0, '', '')",
			[first.saturating_sub(1)],
		)?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const SRV: &str = "srv";

	fn msg(source: MessageSource, author: &str, text: &str, ts_ms: i64) -> NewMessage {
		NewMessage {
			server_uid: SRV.into(),
			target: ChatTarget::Channel(1),
			ts_ms,
			author_uid: Some(format!("uid-{author}")),
			author_name: author.into(),
			author_id: None,
			via_relay: source != MessageSource::Voice,
			text: text.into(),
			source,
			remote: None,
		}
	}

	fn remote(mut m: NewMessage, id: i64, rev: i64) -> NewMessage {
		m.source = MessageSource::Gateway;
		m.remote = Some(RemoteInfo { id, rev, ..Default::default() });
		m
	}

	fn outcomes(written: &[Written]) -> Vec<WriteOutcome> {
		written.iter().map(|w| w.outcome).collect()
	}

	fn texts(rows: &[StoredMessage]) -> Vec<&str> {
		rows.iter().map(|m| m.text.as_str()).collect()
	}

	#[test]
	fn recent_chats_newest_first() {
		let mut store = Store::open_in_memory().unwrap();
		let mut dm = msg(MessageSource::Voice, "b", "hi", 3000);
		dm.target = ChatTarget::Private("uid-b".into());
		let mut other = msg(MessageSource::Voice, "c", "elsewhere", 2500);
		other.server_uid = "srv2".into();
		store
			.write_messages(
				&[
					msg(MessageSource::Voice, "a", "old", 1000),
					msg(MessageSource::Voice, "a", "newer", 2000),
					dm,
					other,
				],
				0,
			)
			.unwrap();
		let recent = store.recent_chats(None).unwrap();
		assert_eq!(texts(&recent), ["hi", "elsewhere", "newer"]);
		assert_eq!(recent[0].target, ChatTarget::Private("uid-b".into()));
		assert_eq!(texts(&store.recent_chats(Some(1)).unwrap()), ["hi"]);
	}

	#[test]
	fn batch_insert_and_pages() {
		let mut store = Store::open_in_memory().unwrap();
		let target = ChatTarget::Channel(1);
		let batch: Vec<_> = (0..10)
			.map(|i| msg(MessageSource::Voice, "a", &format!("m{i}"), 1000 + i * 10))
			.collect();
		let written = store.write_messages(&batch, 5000).unwrap();
		assert!(written.iter().all(|w| w.outcome == WriteOutcome::Inserted));
		// Same time, higher id: after.
		store.write_messages(&[msg(MessageSource::Voice, "a", "m9b", 1090)], 0).unwrap();

		let page = |q| store.messages(SRV, &target, q).unwrap();
		let newest = page(PageQuery { limit: Some(3), ..Default::default() });
		assert_eq!(texts(&newest), ["m8", "m9", "m9b"]);
		let pos = |m: &StoredMessage| (m.ts_ms, m.id);
		let older =
			page(PageQuery { before: Some(pos(&newest[0])), limit: Some(3), ..Default::default() });
		assert_eq!(texts(&older), ["m5", "m6", "m7"]);
		let after =
			page(PageQuery { after: Some(pos(&older[0])), limit: Some(2), ..Default::default() });
		assert_eq!(texts(&after), ["m6", "m7"]);
		let range = page(PageQuery {
			after: Some(pos(&older[0])),
			before: Some(pos(&newest[0])),
			limit: None,
		});
		assert_eq!(texts(&range), ["m6", "m7"]);
		assert_eq!(page(PageQuery::default()).len(), 11, "no limit");
		assert!(store.messages(SRV, &ChatTarget::Server, PageQuery::default()).unwrap().is_empty());
		assert!(store.messages("other", &target, PageQuery::default()).unwrap().is_empty());
		assert_eq!(store.prune_messages(1050).unwrap(), 5);
	}

	#[test]
	fn copies_from_two_sources_are_one_row() {
		let mut store = Store::open_in_memory().unwrap();
		// Live over voice, then from the gateway 1.2 s later (clock skew).
		let live =
			store.write_messages(&[msg(MessageSource::Voice, "a", "hi", 10_000)], 5000).unwrap();
		let gw = store
			.write_messages(&[remote(msg(MessageSource::Gateway, "a", "hi", 11_200), 77, 5)], 5000)
			.unwrap();
		assert_eq!(outcomes(&gw), [WriteOutcome::Merged]);
		let row = &gw[0].message;
		assert_eq!((row.id, row.remote_id, row.rev), (live[0].message.id, Some(77), 5));
		assert_eq!((row.source, row.sync_state), (MessageSource::Voice, SyncState::Synced));
		// The gateway copy again: unchanged; with a pin: updated.
		let again = store
			.write_messages(&[remote(msg(MessageSource::Gateway, "a", "hi", 11_200), 77, 5)], 5000);
		assert_eq!(outcomes(&again.unwrap()), [WriteOutcome::Unchanged]);
		let mut pinned = remote(msg(MessageSource::Gateway, "a", "hi", 11_200), 77, 6);
		pinned.remote.as_mut().unwrap().pinned = true;
		let updated = store.write_messages(&[pinned], 5000).unwrap();
		assert_eq!(outcomes(&updated), [WriteOutcome::Updated]);
		assert!(updated[0].message.pinned);

		// A second live copy from the same source is a new message (the
		// author said it twice)...
		let twice =
			store.write_messages(&[msg(MessageSource::Voice, "a", "hi", 10_500)], 5000).unwrap();
		assert_eq!(outcomes(&twice), [WriteOutcome::Inserted]);
		// ...and the query relay's copy of it matches it, not the first one.
		let relayed = store.write_messages(&[msg(MessageSource::Query, "a", "hi", 10_400)], 5000);
		let relayed = relayed.unwrap();
		assert_eq!(relayed[0].message.id, twice[0].message.id);
		assert_eq!(relayed[0].outcome, WriteOutcome::Unchanged, "nothing visible changed");
		// Too far apart in time, another author or another text: not the same.
		for other in [
			msg(MessageSource::Query, "a", "hi", 30_000),
			msg(MessageSource::Query, "b", "hi", 10_000),
			msg(MessageSource::Query, "a", "hi!", 10_000),
		] {
			assert_eq!(
				outcomes(&store.write_messages(&[other], 5000).unwrap()),
				[WriteOutcome::Inserted]
			);
		}
	}

	#[test]
	fn relay_posts_match_the_gateway_copy() {
		let mut store = Store::open_in_memory().unwrap();
		// We sent "hey" through the gateway: stored at once as our own...
		let mut own = msg(MessageSource::Local, "me", "hey", 5_000);
		own.via_relay = false;
		let own = store.write_messages(&[own], 5000).unwrap();
		// ...the voice connection hears the relay post it as "[me] hey"...
		let mut heard = msg(MessageSource::Voice, "tsgw relay", "[me] hey", 5_300);
		heard.author_uid = Some("uid-relay".into());
		let heard = store.write_messages(&[heard], 5000).unwrap();
		assert_eq!(
			(heard[0].message.id, heard[0].outcome),
			(own[0].message.id, WriteOutcome::Unchanged)
		);
		// ...and the gateway pushes its copy with our uid.
		let gw = store
			.write_messages(&[remote(msg(MessageSource::Gateway, "me", "hey", 5_100), 9, 1)], 5000)
			.unwrap();
		assert_eq!((gw[0].message.id, gw[0].outcome), (own[0].message.id, WriteOutcome::Merged));
		assert_eq!(gw[0].message.text, "hey");
		// Without uids the nickname decides.
		let mut a = msg(MessageSource::Voice, "x", "yo", 1);
		a.author_uid = None;
		let a = store.write_messages(&[a], 5000).unwrap();
		let b =
			store.write_messages(&[remote(msg(MessageSource::Gateway, "x", "yo", 2), 10, 2)], 5000);
		assert_eq!(b.unwrap()[0].message.id, a[0].message.id);
	}

	#[test]
	fn pins_reactions_and_cursors() {
		let mut store = Store::open_in_memory().unwrap();
		let target = ChatTarget::Channel(1);
		store.write_messages(&[remote(msg(MessageSource::Gateway, "a", "x", 1), 5, 1)], 0).unwrap();
		assert!(store.set_pinned(SRV, &target, 5, true).unwrap().unwrap().pinned);
		assert!(store.set_pinned(SRV, &target, 6, true).unwrap().is_none());
		let r = store.set_reaction(SRV, &target, 5, "👍", 2, Some(true)).unwrap().unwrap();
		assert_eq!(r.reactions, [Reaction { emoji: "👍".into(), count: 2, me: true }]);
		let r = store.set_reaction(SRV, &target, 5, "👍", 1, Some(false)).unwrap().unwrap();
		assert_eq!(r.reactions, [Reaction { emoji: "👍".into(), count: 1, me: false }]);
		assert!(
			store
				.set_reaction(SRV, &target, 5, "👍", 0, None)
				.unwrap()
				.unwrap()
				.reactions
				.is_empty()
		);
		// Pinned messages survive pruning.
		assert_eq!(store.prune_messages(i64::MAX).unwrap(), 0);

		assert_eq!(store.chat_cursor(SRV, &target).unwrap(), None);
		let cursor = ChatCursor { gateway_id: "gw".into(), rev: 42, complete: true, updated_ms: 7 };
		store.set_chat_cursor(SRV, &target, &cursor).unwrap();
		assert_eq!(store.chat_cursor(SRV, &target).unwrap(), Some(cursor));
		assert_eq!(store.oldest_remote_ts(SRV, &target).unwrap(), Some(1));
		// Another gateway: ids and cursor are forgotten, the row stays.
		assert_eq!(store.forget_remote_ids(SRV, &target).unwrap(), 1);
		assert_eq!(store.chat_cursor(SRV, &target).unwrap(), None);
		let row = &store.messages(SRV, &target, PageQuery::default()).unwrap()[0];
		assert_eq!((row.remote_id, row.sync_state), (None, SyncState::Local));
		// The new gateway's copy links to it again.
		let again =
			store.write_messages(&[remote(msg(MessageSource::Gateway, "a", "x", 1), 900, 1)], 0);
		assert_eq!(again.unwrap()[0].message.id, row.id);

		assert_eq!(store.server_alias("voice:host").unwrap(), None);
		store.set_server_alias("voice:host", "uid1").unwrap();
		store.set_server_alias("voice:host", "uid2").unwrap();
		assert_eq!(store.server_alias("voice:host").unwrap().as_deref(), Some("uid2"));
	}

	/// A database of version 1 with messages is upgraded in place.
	#[test]
	fn migrates_version_1_with_data() {
		let dir = std::env::temp_dir().join(format!("voelin-store-migrate-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("client.db");
		{
			let db = rusqlite::Connection::open(&path).unwrap();
			db.execute_batch(crate::store::SCHEMA_1).unwrap();
			db.pragma_update(None, "user_version", 1).unwrap();
			db.execute_batch(
				"INSERT INTO settings (key, value) VALUES ('k', '\"v\"');
				 INSERT INTO messages (server_uid, target, ts, author_uid, author_name, via_relay, text)
				 VALUES ('srv', 'channel/1', 100, 'uid-a', 'a', 0, 'old one'),
				        ('srv', 'channel/1', 200, NULL, 'b', 1, '[c] old two');",
			)
			.unwrap();
		}
		let mut store = Store::open(&path).unwrap();
		assert_eq!(store.schema_version().unwrap(), Store::SCHEMA_VERSION);
		assert_eq!(store.setting::<String>("k").unwrap().as_deref(), Some("v"));
		let rows = store.messages(SRV, &ChatTarget::Channel(1), PageQuery::default()).unwrap();
		assert_eq!(texts(&rows), ["old one", "[c] old two"]);
		assert_eq!((rows[0].ts_ms, rows[0].source), (100_000, MessageSource::Local));
		assert!(rows[1].via_relay);
		// The old rows take part in dedupe: the gateway copy of the relay post.
		let mut gw = remote(msg(MessageSource::Gateway, "c", "old two", 201_000), 3, 3);
		gw.author_uid = Some("uid-c".into());
		let gw = store.write_messages(&[gw], 5000).unwrap();
		assert_eq!((gw[0].message.id, gw[0].outcome), (rows[1].id, WriteOutcome::Merged));
		drop(store);
		// Opening again runs nothing.
		let store = Store::open(&path).unwrap();
		assert_eq!(
			store.messages(SRV, &ChatTarget::Channel(1), PageQuery::default()).unwrap().len(),
			2
		);
		drop(store);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn ids_can_start_elsewhere() {
		let mut store = Store::open_in_memory().unwrap();
		store.start_message_ids_at(-1000).unwrap();
		let w = store.write_messages(&[msg(MessageSource::Voice, "a", "x", 1)], 0).unwrap();
		assert_eq!(w[0].message.id, -1000);
		let w = store.write_messages(&[msg(MessageSource::Voice, "a", "y", 2)], 0).unwrap();
		assert_eq!(w[0].message.id, -999);
		assert_eq!(store.prune_messages(i64::MAX).unwrap(), 2, "the hidden row stays");
		let w = store.write_messages(&[msg(MessageSource::Voice, "a", "z", 3)], 0).unwrap();
		assert_eq!(w[0].message.id, -1000);
	}

	#[test]
	fn keys_round_trip() {
		for t in [ChatTarget::Server, ChatTarget::Channel(12), ChatTarget::Private("a/b=".into())] {
			assert_eq!(ChatTarget::from_key(&t.key()), t);
		}
		assert_eq!(split_relay("[nick] text"), (Some("nick"), "text"));
		assert_eq!(split_relay("[] text"), (None, "[] text"));
		assert_eq!(text_hash("[nick] text"), text_hash("text"));
	}
}
