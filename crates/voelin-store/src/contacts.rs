//! Contacts: people the user marked, by their unique id (the same on every
//! server).
//!
//! A contact is a friend, blocked, or neutral (only a note, a volume or a
//! mute). Its nickname is a hint (the last one seen); `last_seen_ms` and
//! `last_server` say when and where the engine saw the contact last.

use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::{Result, Store};

/// What the user thinks of a contact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Relation {
	Friend,
	Blocked,
	#[default]
	Neutral,
}

impl Relation {
	pub fn as_str(self) -> &'static str {
		match self {
			Relation::Friend => "friend",
			Relation::Blocked => "blocked",
			Relation::Neutral => "neutral",
		}
	}

	/// Parse [`Self::as_str`]; anything else is neutral.
	pub fn parse(s: &str) -> Self {
		match s {
			"friend" => Relation::Friend,
			"blocked" => Relation::Blocked,
			_ => Relation::Neutral,
		}
	}
}

fn default_volume() -> f32 {
	1.0
}

/// A contact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Contact {
	/// The client's unique id.
	pub uid: String,
	/// The nickname last seen (or given by the user).
	#[serde(default)]
	pub nickname: String,
	#[serde(default)]
	pub relation: Relation,
	#[serde(default)]
	pub note: String,
	/// Do not play the contact's voice.
	#[serde(default)]
	pub muted: bool,
	/// Playback volume, linear (1 = unchanged).
	#[serde(default = "default_volume")]
	pub volume: f32,
	/// Unix milliseconds when the contact was added.
	#[serde(default)]
	pub added_ms: i64,
	/// Unix milliseconds when the contact was last seen on a server (0: never).
	#[serde(default)]
	pub last_seen_ms: i64,
	/// The server (its name, else its unique id) the contact was last seen on.
	#[serde(default)]
	pub last_server: Option<String>,
}

impl Contact {
	/// A neutral contact with the default volume.
	pub fn new(uid: impl Into<String>) -> Self {
		Self {
			uid: uid.into(),
			nickname: String::new(),
			relation: Relation::Neutral,
			note: String::new(),
			muted: false,
			volume: 1.0,
			added_ms: 0,
			last_seen_ms: 0,
			last_server: None,
		}
	}
}

/// Version 3: contacts.
pub(crate) const SCHEMA_3: &str = r#"
	CREATE TABLE contacts (
		uid TEXT PRIMARY KEY,
		nickname TEXT NOT NULL DEFAULT '',
		relation TEXT NOT NULL DEFAULT 'neutral',
		note TEXT NOT NULL DEFAULT '',
		mute INTEGER NOT NULL DEFAULT 0,
		volume REAL NOT NULL DEFAULT 1.0,
		added_ms INTEGER NOT NULL DEFAULT 0,
		last_seen_ms INTEGER NOT NULL DEFAULT 0,
		last_server TEXT
	) WITHOUT ROWID;
"#;

const COLUMNS: &str =
	"uid, nickname, relation, note, mute, volume, added_ms, last_seen_ms, last_server";

fn contact_from_row(r: &Row) -> rusqlite::Result<Contact> {
	let relation: String = r.get(2)?;
	Ok(Contact {
		uid: r.get(0)?,
		nickname: r.get(1)?,
		relation: Relation::parse(&relation),
		note: r.get(3)?,
		muted: r.get(4)?,
		volume: r.get::<_, f64>(5)? as f32,
		added_ms: r.get(6)?,
		last_seen_ms: r.get(7)?,
		last_server: r.get(8)?,
	})
}

/// When and where a contact was seen, for [`Store::touch_contacts`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContactSeen {
	pub uid: String,
	pub nickname: String,
	pub ts_ms: i64,
	pub server: Option<String>,
}

impl Store {
	/// All contacts, by unique id.
	pub fn contacts(&self) -> Result<Vec<Contact>> {
		let mut stmt =
			self.db.prepare_cached(&format!("SELECT {COLUMNS} FROM contacts ORDER BY uid"))?;
		let rows = stmt.query_map([], contact_from_row)?;
		Ok(rows.collect::<rusqlite::Result<_>>()?)
	}

	pub fn contact(&self, uid: &str) -> Result<Option<Contact>> {
		Ok(self
			.db
			.prepare_cached(&format!("SELECT {COLUMNS} FROM contacts WHERE uid = ?1"))?
			.query_row([uid], contact_from_row)
			.optional()?)
	}

	/// Store a contact (insert or replace all of it).
	pub fn put_contact(&self, c: &Contact) -> Result<()> {
		self.db
			.prepare_cached(&format!(
				"INSERT OR REPLACE INTO contacts ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
			))?
			.execute(params![
				c.uid,
				c.nickname,
				c.relation.as_str(),
				c.note,
				c.muted,
				f64::from(c.volume),
				c.added_ms,
				c.last_seen_ms,
				c.last_server
			])?;
		Ok(())
	}

	/// Remove a contact; `true` if it existed.
	pub fn delete_contact(&self, uid: &str) -> Result<bool> {
		Ok(self.db.prepare_cached("DELETE FROM contacts WHERE uid = ?1")?.execute([uid])? > 0)
	}

	/// Record sightings of contacts in one transaction (newer ones win;
	/// unknown unique ids are ignored). The nickname hint follows the
	/// sighting.
	pub fn touch_contacts(&mut self, seen: &[ContactSeen]) -> Result<()> {
		let tx = self.db.transaction()?;
		{
			let mut update = tx.prepare_cached(
				"UPDATE contacts SET last_seen_ms = ?2, last_server = COALESCE(?3, last_server),
					nickname = CASE WHEN ?4 = '' THEN nickname ELSE ?4 END
				 WHERE uid = ?1 AND last_seen_ms <= ?2",
			)?;
			for s in seen {
				update.execute(params![s.uid, s.ts_ms, s.server, s.nickname])?;
			}
		}
		tx.commit()?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn friend(uid: &str) -> Contact {
		Contact {
			nickname: "Alice".into(),
			relation: Relation::Friend,
			note: "from school".into(),
			volume: 1.5,
			added_ms: 10,
			..Contact::new(uid)
		}
	}

	#[test]
	fn put_get_delete() {
		let store = Store::open_in_memory().unwrap();
		assert!(store.contacts().unwrap().is_empty());
		let a = friend("a=");
		store.put_contact(&a).unwrap();
		let mut b = Contact::new("b=");
		b.relation = Relation::Blocked;
		b.muted = true;
		store.put_contact(&b).unwrap();
		assert_eq!(store.contacts().unwrap(), [a.clone(), b.clone()]);
		assert_eq!(store.contact("a=").unwrap(), Some(a.clone()));
		let changed = Contact { relation: Relation::Neutral, ..a.clone() };
		store.put_contact(&changed).unwrap();
		assert_eq!(store.contact("a=").unwrap().unwrap().relation, Relation::Neutral);
		assert!(store.delete_contact("a=").unwrap());
		assert!(!store.delete_contact("a=").unwrap());
		assert_eq!(store.contacts().unwrap(), [b]);
	}

	#[test]
	fn sightings_update_known_contacts_only() {
		let mut store = Store::open_in_memory().unwrap();
		store.put_contact(&friend("a=")).unwrap();
		let seen = |uid: &str, ts_ms, server: Option<&str>, nick: &str| ContactSeen {
			uid: uid.into(),
			nickname: nick.into(),
			ts_ms,
			server: server.map(str::to_owned),
		};
		store
			.touch_contacts(&[
				seen("a=", 100, Some("Home"), "Alice2"),
				seen("x=", 100, Some("Home"), "X"),
			])
			.unwrap();
		let a = store.contact("a=").unwrap().unwrap();
		assert_eq!((a.last_seen_ms, a.last_server.as_deref()), (100, Some("Home")));
		assert_eq!(a.nickname, "Alice2");
		assert!(store.contact("x=").unwrap().is_none());
		// An older sighting does not win; no server keeps the last one.
		store.touch_contacts(&[seen("a=", 50, Some("Other"), "")]).unwrap();
		store.touch_contacts(&[seen("a=", 200, None, "")]).unwrap();
		let a = store.contact("a=").unwrap().unwrap();
		assert_eq!((a.last_seen_ms, a.last_server.as_deref()), (200, Some("Home")));
		assert_eq!(a.nickname, "Alice2");
	}

	/// A database of version 2 (chat history, no contacts) gains the table
	/// and keeps its data.
	#[test]
	fn migrates_version_2() {
		let dir = std::env::temp_dir().join(format!("voelin-contacts-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("client.db");
		{
			let mut db = rusqlite::Connection::open(&path).unwrap();
			db.execute_batch(crate::store::SCHEMA_1).unwrap();
			let tx = db.transaction().unwrap();
			crate::chat::migrate_2(&tx).unwrap();
			tx.commit().unwrap();
			db.pragma_update(None, "user_version", 2).unwrap();
			db.execute_batch("INSERT INTO settings (key, value) VALUES ('k', '1');").unwrap();
		}
		let store = Store::open(&path).unwrap();
		assert_eq!(store.schema_version().unwrap(), 3);
		assert_eq!(Store::SCHEMA_VERSION, 3);
		assert_eq!(store.setting::<u32>("k").unwrap(), Some(1));
		store.put_contact(&friend("a=")).unwrap();
		drop(store);
		let store = Store::open(&path).unwrap();
		assert_eq!(store.contacts().unwrap(), [friend("a=")]);
		drop(store);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn relation_names() {
		for r in [Relation::Friend, Relation::Blocked, Relation::Neutral] {
			assert_eq!(Relation::parse(r.as_str()), r);
		}
		assert_eq!(Relation::parse("x"), Relation::Neutral);
		let json = serde_json::to_string(&friend("a=")).unwrap();
		assert!(json.contains(r#""relation":"friend""#));
		let back: Contact = serde_json::from_str(r#"{"uid":"u"}"#).unwrap();
		assert_eq!(back, Contact::new("u"));
	}
}
