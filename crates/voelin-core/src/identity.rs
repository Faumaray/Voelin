//! Importing identities from the official TeamSpeak clients, and exporting
//! them back.
//!
//! An identity is the key pair that gives a client its unique id on every
//! server; losing it loses every server group tied to that id, so switching
//! clients has to bring it along. The formats and locations are described in
//! `docs/identity.md`; this module reads them:
//!
//! - the TeamSpeak 3 `.ini` export (`identity="<counter>V<obfuscated>"`),
//!   which is also the shape of the `identity=` lines in
//!   `ts3clientui_qt.secrets.conf`,
//! - the `ProtobufItems` table of both clients' `settings.db`, where each
//!   item's type (field 6) says which payload field carries the body and
//!   type 1 (field 17) is an identity,
//! - the `Identities/<n>/…` rows older TeamSpeak 3 clients kept in
//!   `Profiles`.
//!
//! [`locations`] lists where those files live per platform and [`discover`]
//! which of them exist here. Every source is opened **read-only and
//! `immutable`**: the official client may be running, and none of its files
//! may be written, locked or recovered. [`import`] stores what the user
//! picked through [`Store`], skipping unique ids the store already has, and
//! [`export`] writes an identity back out in the `.ini` form so nothing is
//! trapped here.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::{fs, str};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use tsclientlib::Identity;
use voelin_gateway_proto::UniqueIds;
use voelin_store::Store;

/// Name given to an identity whose source kept none.
const DEFAULT_NICKNAME: &str = "imported";

/// Item field holding the item's type; the payload is in field
/// [`PAYLOAD_FIELD_BASE`] `+ type`.
const TYPE_FIELD: u32 = 6;
/// Item type of an identity.
const IDENTITY_TYPE: u64 = 1;
/// Field number of the payload of item type 0.
const PAYLOAD_FIELD_BASE: u32 = 16;
/// Field number of the identity payload: [`PAYLOAD_FIELD_BASE`] `+ 1`.
const IDENTITY_PAYLOAD_FIELD: u32 = PAYLOAD_FIELD_BASE + IDENTITY_TYPE as u32;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
	#[error("{path}: {message}")]
	Read { path: String, message: String },
	#[error("{path}: no TeamSpeak identity in this file")]
	NoIdentity { path: String },
	#[error("client database: {0}")]
	Store(String),
}

impl IdentityError {
	fn read(path: &Path, message: impl std::fmt::Display) -> Self {
		Self::Read { path: path.display().to_string(), message: message.to_string() }
	}
}

impl From<voelin_store::Error> for IdentityError {
	fn from(e: voelin_store::Error) -> Self {
		Self::Store(e.to_string())
	}
}

/// An identity read from another client, before it is stored.
#[derive(Clone, Debug)]
pub struct Found {
	/// The nickname the other client kept with it.
	pub nickname: String,
	pub identity: Identity,
	/// The file it came from.
	pub source: PathBuf,
}

impl Found {
	/// The unique id TeamSpeak 3 servers know this identity by, which is also
	/// how the store keys it.
	pub fn uid(&self) -> String {
		self.identity.key().to_pub().get_uid()
	}

	/// The unique ids of both server generations.
	pub fn uids(&self) -> UniqueIds {
		UniqueIds::from_omega(&self.identity.key().to_pub().to_ts())
	}

	/// Hash cash security level; servers commonly require 8.
	pub fn level(&self) -> u8 {
		self.identity.level()
	}
}

/// What [`import`] did with one found identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Imported {
	/// Added to the store under this id.
	Added(i64),
	/// The store already had this unique id.
	Duplicate,
}

/// Read every identity in `path`, whichever of the known forms it has.
///
/// The file is only read, never written, moved or locked.
pub fn read(path: &Path) -> Result<Vec<Found>, IdentityError> {
	let found = if is_sqlite(path)? { read_database(path)? } else { read_text(path)? };
	if found.is_empty() {
		return Err(IdentityError::NoIdentity { path: path.display().to_string() });
	}
	Ok(found)
}

/// Where the official clients keep their databases on this platform (see
/// `docs/identity.md`). Paths that do not exist are listed too;
/// [`discover`] filters them.
pub fn locations() -> Vec<PathBuf> {
	let mut out = Vec::new();
	if let Some(config) = dirs::config_dir() {
		// TeamSpeak 6 on every platform.
		out.push(config.join("TeamSpeak").join("Default").join("settings.db"));
		// TeamSpeak 3; on Linux it keeps a hidden directory in $HOME instead.
		if cfg!(target_os = "windows") {
			out.push(config.join("TS3Client").join("settings.db"));
		} else if cfg!(target_os = "macos") {
			out.push(config.join("TeamSpeak 3").join("settings.db"));
		}
	}
	if !cfg!(any(target_os = "windows", target_os = "macos"))
		&& let Some(home) = dirs::home_dir()
	{
		out.push(home.join(".ts3client").join("settings.db"));
		// Flatpak redirects each app's config into its own sandbox, so
		// `dirs::config_dir` of this process never points at it.
		let app = home.join(".var").join("app");
		out.push(
			app.join("com.teamspeak.TeamSpeak")
				.join("config")
				.join("TeamSpeak")
				.join("Default")
				.join("settings.db"),
		);
		out.push(app.join("com.teamspeak.TeamSpeak3").join(".ts3client").join("settings.db"));
	}
	out
}

/// The [`locations`] that exist here.
pub fn discover() -> Vec<PathBuf> {
	locations().into_iter().filter(|p| p.is_file()).collect()
}

/// Store `found`, skipping identities whose unique id the store already has
/// and repeats within `found` itself. The source files are not touched.
pub fn import(store: &Store, found: &[Found]) -> Result<Vec<Imported>, IdentityError> {
	let mut known: HashSet<String> =
		store.identities()?.into_iter().map(|entry| entry.uid).collect();
	found
		.iter()
		.map(|f| {
			if !known.insert(f.uid()) {
				return Ok(Imported::Duplicate);
			}
			Ok(Imported::Added(store.add_identity(&f.nickname, &f.identity)?))
		})
		.collect()
}

/// The TeamSpeak 3 `.ini` export of an identity, which the official client
/// and every other tool reads.
pub fn export(nickname: &str, identity: &Identity) -> String {
	format!(
		"[Identity]\nid={nickname}\nidentity=\"{}V{}\"\nnickname={nickname}\n",
		identity.counter(),
		identity.key().to_ts_obfuscated(),
	)
}

/// Write [`export`] to `path`. A new file is created readable only by its
/// owner on Unix, because it holds a private key.
pub fn write_export(path: &Path, nickname: &str, identity: &Identity) -> Result<(), IdentityError> {
	let mut options = fs::OpenOptions::new();
	options.write(true).create(true).truncate(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600);
	}
	let mut file = options.open(path).map_err(|e| IdentityError::read(path, e))?;
	file.write_all(export(nickname, identity).as_bytes()).map_err(|e| IdentityError::read(path, e))
}

/// Does `path` start with the SQLite file header?
fn is_sqlite(path: &Path) -> Result<bool, IdentityError> {
	let mut head = [0u8; 16];
	let mut file = fs::File::open(path).map_err(|e| IdentityError::read(path, e))?;
	match file.read_exact(&mut head) {
		Ok(()) => Ok(&head == b"SQLite format 3\0"),
		// Shorter than a header: not a database, maybe a small `.ini`.
		Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
		Err(e) => Err(IdentityError::read(path, e)),
	}
}

/// Open a client's database without disturbing it: read-only, and
/// `immutable` so SQLite takes no lock, creates no shared-memory file and
/// does not replay a write-ahead log the running client left behind. The
/// cost is that identities the client has written but not yet checkpointed
/// are not seen; nothing of the user's is changed to see them.
fn open_read_only(path: &Path) -> Result<Connection, IdentityError> {
	let uri = format!("file:{}?mode=ro&immutable=1", uri_escape(path));
	Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
		.map_err(|e| IdentityError::read(path, e))
}

/// Percent-encode the characters SQLite's URI parser would take as syntax.
fn uri_escape(path: &Path) -> String {
	let mut out = String::new();
	for c in path.to_string_lossy().chars() {
		match c {
			'?' | '#' | '%' => out.push_str(&format!("%{:02X}", u32::from(c))),
			_ => out.push(c),
		}
	}
	out
}

fn read_database(path: &Path) -> Result<Vec<Found>, IdentityError> {
	let db = open_read_only(path)?;
	let mut found = protobuf_identities(&db, path).map_err(|e| IdentityError::read(path, e))?;
	found.extend(profile_identities(&db, path).map_err(|e| IdentityError::read(path, e))?);
	Ok(found)
}

fn has_table(db: &Connection, name: &str) -> rusqlite::Result<bool> {
	Ok(db
		.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [name], |_| {
			Ok(())
		})
		.optional()?
		.is_some())
}

/// Identities in the `ProtobufItems` table both clients use.
fn protobuf_identities(db: &Connection, source: &Path) -> rusqlite::Result<Vec<Found>> {
	if !has_table(db, "ProtobufItems")? {
		return Ok(Vec::new());
	}
	// The column is declared `varchar` but holds protobuf bytes.
	let mut stmt =
		db.prepare("SELECT CAST(value AS BLOB) FROM ProtobufItems WHERE key <> 'Checksum'")?;
	let items = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
	let mut out = Vec::new();
	for item in items {
		if let Some(found) = identity_payload(&item?).and_then(|p| payload_identity(p, source)) {
			out.push(found);
		}
	}
	Ok(out)
}

/// Identities in the `Identities/<n>/…` rows older TeamSpeak 3 clients kept.
fn profile_identities(db: &Connection, source: &Path) -> rusqlite::Result<Vec<Found>> {
	if !has_table(db, "Profiles")? {
		return Ok(Vec::new());
	}
	let mut stmt = db.prepare(
		"SELECT key, CAST(value AS TEXT) FROM Profiles WHERE key LIKE 'Identities/%' ORDER BY key",
	)?;
	let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
	// `Identities/<n>/identity` and `Identities/<n>/nickname` belong together.
	let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
	for row in rows {
		let (key, value) = row?;
		let mut parts = key.splitn(3, '/');
		if let (Some(_), Some(index), Some(field)) = (parts.next(), parts.next(), parts.next()) {
			groups.entry(index.to_owned()).or_default().push((field.to_ascii_lowercase(), value));
		}
	}
	Ok(groups.values().filter_map(|group| record_identity(group, source)).collect())
}

/// Identities in the TeamSpeak 3 `.ini` export form, which is also what
/// `ts3clientui_qt.secrets.conf` uses for its `identity=` lines.
fn read_text(path: &Path) -> Result<Vec<Found>, IdentityError> {
	// A foreign binary file lands here and is not UTF-8; that is not an
	// unexpected failure, it simply holds no identity.
	let Ok(text) = fs::read_to_string(path) else {
		return Ok(Vec::new());
	};
	Ok(text_identities(&text, path))
}

/// `key=value` lines grouped into one record per `identity=` line: the keys
/// before the first one and after each one belong to it, because a
/// TeamSpeak 3 export writes `id=` before and `nickname=` after.
fn text_identities(text: &str, source: &Path) -> Vec<Found> {
	let mut records: Vec<Vec<(String, String)>> = vec![Vec::new()];
	for line in text.lines() {
		let line = line.trim();
		if line.starts_with('[') || line.starts_with(['#', ';']) {
			continue;
		}
		let Some((key, value)) = line.split_once('=') else { continue };
		let key = key.trim().to_ascii_lowercase();
		let record = records.last().expect("at least one record");
		if key == "identity" && record.iter().any(|(k, _)| k == "identity") {
			records.push(Vec::new());
		}
		let value = value.trim().trim_matches('"').to_owned();
		records.last_mut().expect("at least one record").push((key, value));
	}
	records.iter().filter_map(|record| record_identity(record, source)).collect()
}

/// One `.ini` record or one `Identities/<n>/…` group: its `identity` key
/// holds the export string, its `nickname` or `id` the name.
fn record_identity(record: &[(String, String)], source: &Path) -> Option<Found> {
	let find =
		|wanted: &str| record.iter().find(|(key, _)| key == wanted).map(|(_, value)| value.trim());
	let identity = Identity::new_from_ts_str(find("identity")?).ok()?;
	let nickname = find("nickname").or_else(|| find("id")).unwrap_or_default();
	let nickname = if nickname.is_empty() { DEFAULT_NICKNAME } else { nickname }.to_owned();
	Some(Found { nickname, identity, source: source.to_owned() })
}

/// The identity payload of a `ProtobufItems` item, or `None` if the item is
/// something else (a bookmark, a connection profile, a server list).
fn identity_payload(item: &[u8]) -> Option<&[u8]> {
	let mut kind = None;
	let mut payload = None;
	let mut wire = Wire { rest: item };
	while let Some((number, field)) = wire.next_field() {
		match (number, field) {
			(TYPE_FIELD, Field::Var(v)) => kind = Some(v),
			(IDENTITY_PAYLOAD_FIELD, Field::Bytes(b)) => payload = Some(b),
			_ => {}
		}
	}
	// Match on the declared type, never on a payload field alone: another
	// item type may reuse the number.
	payload.filter(|_| kind == Some(IDENTITY_TYPE))
}

/// The export string is field 1 of the identity payload, the nickname field 2.
fn payload_identity(payload: &[u8], source: &Path) -> Option<Found> {
	let mut export = None;
	let mut nickname = None;
	let mut wire = Wire { rest: payload };
	while let Some((number, Field::Bytes(bytes))) = wire.next_field() {
		match number {
			1 => export = str::from_utf8(bytes).ok(),
			2 => nickname = Some(String::from_utf8_lossy(bytes).into_owned()),
			_ => {}
		}
	}
	let identity = Identity::new_from_ts_str(export?).ok()?;
	let nickname =
		nickname.filter(|n| !n.is_empty()).unwrap_or_else(|| DEFAULT_NICKNAME.to_owned());
	Some(Found { nickname, identity, source: source.to_owned() })
}

/// A protobuf field's value. Fixed-width fields carry nothing we read; they
/// only have to be skipped to reach the next field.
enum Field<'a> {
	Var(u64),
	Bytes(&'a [u8]),
	Fixed,
}

/// Just enough of the protobuf wire format to walk the items (nothing else
/// in the tree speaks protobuf, and three field numbers do not earn a
/// dependency).
struct Wire<'a> {
	rest: &'a [u8],
}

impl<'a> Wire<'a> {
	fn varint(&mut self) -> Option<u64> {
		let mut value = 0;
		let mut shift = 0;
		loop {
			let (&byte, rest) = self.rest.split_first()?;
			self.rest = rest;
			// `checked_shl` ends a varint longer than ten bytes instead of
			// overflowing: such a value cannot be one of ours anyway.
			value |= u64::from(byte & 0x7f).checked_shl(shift)?;
			if byte & 0x80 == 0 {
				return Some(value);
			}
			shift += 7;
		}
	}

	fn take(&mut self, n: usize) -> Option<&'a [u8]> {
		let (head, rest) = self.rest.split_at_checked(n)?;
		self.rest = rest;
		Some(head)
	}

	/// The next field, or `None` at the end and on bytes that are not a
	/// message. The caller does not need to tell those apart: a truncated
	/// item simply yields fewer fields and then does not look like an
	/// identity.
	fn next_field(&mut self) -> Option<(u32, Field<'a>)> {
		let key = self.varint()?;
		let number = u32::try_from(key >> 3).ok()?;
		let value = match key & 7 {
			0 => Field::Var(self.varint()?),
			1 => {
				self.take(8)?;
				Field::Fixed
			}
			2 => {
				let len = usize::try_from(self.varint()?).ok()?;
				Field::Bytes(self.take(len)?)
			}
			5 => {
				self.take(4)?;
				Field::Fixed
			}
			_ => return None,
		};
		Some((number, value))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A synthetic identity; never a real one from any client.
	fn synthetic() -> Identity {
		Identity::create()
	}

	fn temp_dir(name: &str) -> PathBuf {
		let dir =
			std::env::temp_dir().join(format!("voelin-identity-{name}-{}", std::process::id()));
		fs::create_dir_all(&dir).unwrap();
		dir
	}

	/// Length-delimited protobuf field, for building the synthetic databases.
	fn bytes_field(number: u32, value: &[u8]) -> Vec<u8> {
		let mut out = varint(u64::from(number) << 3 | 2);
		out.extend(varint(value.len() as u64));
		out.extend(value);
		out
	}

	fn varint_field(number: u32, value: u64) -> Vec<u8> {
		let mut out = varint(u64::from(number) << 3);
		out.extend(varint(value));
		out
	}

	fn varint(mut value: u64) -> Vec<u8> {
		let mut out = Vec::new();
		loop {
			let byte = (value & 0x7f) as u8;
			value >>= 7;
			out.push(if value == 0 { byte } else { byte | 0x80 });
			if value == 0 {
				return out;
			}
		}
	}

	/// An item of the shape both clients write, holding `identity`.
	fn protobuf_item(identity: &Identity, nickname: &str) -> Vec<u8> {
		let export = format!("{}V{}", identity.counter(), identity.key().to_ts_obfuscated());
		let mut payload = bytes_field(1, export.as_bytes());
		payload.extend(bytes_field(2, nickname.as_bytes()));
		payload.extend(bytes_field(3, b"a1b2c3d4"));
		payload.extend(varint_field(5, 1));
		let mut item = bytes_field(2, b"4a7b4f46-0000-4000-8000-000000000001");
		item.extend(varint_field(TYPE_FIELD, IDENTITY_TYPE));
		item.extend(varint_field(9, 1_700_000_000));
		item.extend(bytes_field(IDENTITY_PAYLOAD_FIELD, &payload));
		item
	}

	#[test]
	fn ts3_export_round_trips_through_both_unique_ids() {
		let dir = temp_dir("export");
		let path = dir.join("identity.ini");
		let identity = synthetic();
		write_export(&path, "tester", &identity).unwrap();
		let found = read(&path).unwrap();
		assert_eq!(found.len(), 1);
		assert_eq!(found[0].nickname, "tester");
		// Both server generations must see the same identity again.
		let before = UniqueIds::from_omega(&identity.key().to_pub().to_ts());
		assert_eq!(found[0].uids().ts3, before.ts3);
		assert_eq!(found[0].uids().ts6, before.ts6);
		assert_eq!(found[0].uid(), before.ts3);
		assert_eq!(found[0].identity.counter(), identity.counter());
		assert_eq!(found[0].level(), identity.level());
		fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn reads_an_unquoted_export_and_a_secrets_file_with_several() {
		let one = synthetic();
		let two = synthetic();
		let source = Path::new("test.conf");
		let text = format!(
			"[Identity]\nid=first\nidentity={}V{}\nnickname=First\n\
			 identity=\"{}V{}\"\nnickname=Second\n",
			one.counter(),
			one.key().to_ts_obfuscated(),
			two.counter(),
			two.key().to_ts_obfuscated(),
		);
		let found = text_identities(&text, source);
		assert_eq!(found.len(), 2);
		assert_eq!(found[0].nickname, "First");
		assert_eq!(found[1].nickname, "Second");
		assert_eq!(found[0].uid(), one.key().to_pub().get_uid());
		assert_eq!(found[1].uid(), two.key().to_pub().get_uid());
	}

	#[test]
	fn reads_a_teamspeak_3_profiles_database() {
		let dir = temp_dir("profiles");
		let path = dir.join("settings.db");
		let identity = synthetic();
		{
			let db = Connection::open(&path).unwrap();
			db.execute_batch(
				"CREATE TABLE Profiles (timestamp INTEGER UNSIGNED NOT NULL,
				 key VARCHAR NOT NULL UNIQUE, value VARCHAR)",
			)
			.unwrap();
			let export = format!("{}V{}", identity.counter(), identity.key().to_ts_obfuscated());
			for (key, value) in [
				("Identities/1/identity", export.as_str()),
				("Identities/1/nickname", "Legacy"),
				// A setting that is not an identity must be ignored.
				("Capture/Default", "whatever"),
			] {
				db.execute("INSERT INTO Profiles VALUES (0, ?1, ?2)", (key, value)).unwrap();
			}
		}
		let found = read(&path).unwrap();
		assert_eq!(found.len(), 1);
		assert_eq!(found[0].nickname, "Legacy");
		assert_eq!(found[0].uid(), identity.key().to_pub().get_uid());
		fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn reads_a_protobuf_items_database_and_skips_other_item_types() {
		let dir = temp_dir("protobuf");
		let path = dir.join("settings.db");
		let identity = synthetic();
		{
			let db = Connection::open(&path).unwrap();
			db.execute_batch(
				"CREATE TABLE ProtobufItems (timestamp integer unsigned NOT NULL,
				 key varchar NOT NULL UNIQUE, value varchar)",
			)
			.unwrap();
			// A bookmark-shaped item: type 0, so its payload is field 16.
			let mut other = varint_field(TYPE_FIELD, 0);
			other.extend(bytes_field(PAYLOAD_FIELD_BASE, b"not an identity"));
			// An item whose type says identity but whose payload field is a
			// bookmark's: neither half alone may be trusted.
			let mut mismatched = varint_field(TYPE_FIELD, IDENTITY_TYPE);
			mismatched.extend(bytes_field(PAYLOAD_FIELD_BASE, b"not an identity"));
			for (key, value) in [
				("1", other),
				("2", protobuf_item(&identity, "Имя")),
				("3", mismatched),
				("Checksum", vec![0; 20]),
			] {
				db.execute("INSERT INTO ProtobufItems VALUES (0, ?1, ?2)", (key, value)).unwrap();
			}
		}
		let found = read(&path).unwrap();
		assert_eq!(found.len(), 1);
		// A non-ASCII nickname survives.
		assert_eq!(found[0].nickname, "Имя");
		assert_eq!(found[0].uid(), identity.key().to_pub().get_uid());
		fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn import_stores_once_and_reports_repeats() {
		let store = Store::open_in_memory().unwrap();
		let identity = synthetic();
		let found = |nickname: &str| Found {
			nickname: nickname.to_owned(),
			identity: identity.clone(),
			source: PathBuf::from("test.ini"),
		};
		// The same unique id twice in one batch, then again in a second run.
		let outcome = import(&store, &[found("first"), found("again")]).unwrap();
		assert!(matches!(outcome[0], Imported::Added(_)));
		assert_eq!(outcome[1], Imported::Duplicate);
		assert_eq!(import(&store, &[found("third")]).unwrap(), [Imported::Duplicate]);
		let stored = store.identities().unwrap();
		assert_eq!(stored.len(), 1);
		assert_eq!(stored[0].name, "first");
		assert_eq!(stored[0].uid, identity.key().to_pub().get_uid());
	}

	#[test]
	fn rejects_foreign_and_corrupt_files() {
		let dir = temp_dir("foreign");
		// Text that is not an export.
		let text = dir.join("notes.txt");
		fs::write(&text, "hello = world\n[Identity]\nnickname=nobody\n").unwrap();
		assert!(matches!(read(&text), Err(IdentityError::NoIdentity { .. })));

		// A database whose identity string is damaged.
		let broken = dir.join("broken.db");
		{
			let db = Connection::open(&broken).unwrap();
			db.execute_batch(
				"CREATE TABLE ProtobufItems (timestamp integer, key varchar UNIQUE, value varchar)",
			)
			.unwrap();
			let mut payload = bytes_field(1, b"12345Vnot-a-key");
			payload.extend(bytes_field(2, b"Broken"));
			let mut item = varint_field(TYPE_FIELD, IDENTITY_TYPE);
			item.extend(bytes_field(IDENTITY_PAYLOAD_FIELD, &payload));
			db.execute("INSERT INTO ProtobufItems VALUES (0, '1', ?1)", (item,)).unwrap();
		}
		assert!(matches!(read(&broken), Err(IdentityError::NoIdentity { .. })));

		// Bytes that only claim to be a database.
		let fake = dir.join("fake.db");
		fs::write(&fake, b"SQLite format 3\0 and then nonsense").unwrap();
		assert!(matches!(read(&fake), Err(IdentityError::Read { .. })));

		// Nothing there at all.
		assert!(matches!(read(&dir.join("missing.ini")), Err(IdentityError::Read { .. })));
		fs::remove_dir_all(dir).unwrap();
	}

	/// An imported key pair must be whole: the public key derived from the
	/// private scalar has to verify what that scalar signs. The clients'
	/// format carries the public point alongside the private one and
	/// [`Identity`] keeps only the scalar, so this also proves the two
	/// halves of the parsed key belong together.
	fn signs_and_verifies(identity: &Identity) {
		let nonce = b"voelin identity import self-check";
		let signature = identity.key().clone().sign(nonce);
		identity.key().to_pub().verify(nonce, &signature).expect("own signature verifies");
		// A different message must not verify under the same signature.
		assert!(identity.key().to_pub().verify(b"something else", &signature).is_err());
	}

	#[test]
	fn an_imported_key_pair_is_whole() {
		let dir = temp_dir("whole");
		let path = dir.join("identity.ini");
		write_export(&path, "tester", &synthetic()).unwrap();
		let found = read(&path).unwrap();
		assert_eq!(found.len(), 1);
		signs_and_verifies(&found[0].identity);
		fs::remove_dir_all(dir).unwrap();
	}

	/// Checks a real client database when `VOELIN_IDENTITY_IMPORT_CHECK`
	/// points at one, printing only public facts: how many identities were
	/// found, their nicknames, their unique ids and their levels. Without
	/// the variable there is nothing to check and the test passes.
	#[test]
	fn checks_the_database_the_environment_points_at() {
		let Some(path) = std::env::var_os("VOELIN_IDENTITY_IMPORT_CHECK") else { return };
		let path = PathBuf::from(path);
		let found = read(&path).expect("read the database");
		println!("{}: {} identities", path.display(), found.len());
		for identity in &found {
			let uids = identity.uids();
			// A TeamSpeak unique id is base64 of a SHA-1 or SHA-256 digest.
			assert_eq!(uids.ts3.len(), 28, "{}", uids.ts3);
			assert_eq!(uids.ts6.len(), 44, "{}", uids.ts6);
			for id in [&uids.ts3, &uids.ts6] {
				assert!(
					id.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)),
					"{id} is not base64"
				);
			}
			assert_eq!(uids.ts3, identity.uid());
			// Servers reject anything below 8.
			assert!(identity.level() >= 8, "level {}", identity.level());
			signs_and_verifies(&identity.identity);
			// The export must come back as the same identity.
			let text = export(&identity.nickname, &identity.identity);
			let again = text_identities(&text, &path);
			assert_eq!(again.len(), 1);
			assert_eq!(again[0].uids().ts3, uids.ts3);
			assert_eq!(again[0].uids().ts6, uids.ts6);
			assert_eq!(again[0].nickname, identity.nickname);
			println!(
				"  {} {} {} level {}",
				identity.nickname,
				uids.ts3,
				uids.ts6,
				identity.level()
			);
		}
		assert!(!found.is_empty());
	}

	#[test]
	fn locations_are_absolute_and_named_settings_db() {
		for path in locations() {
			assert!(path.is_absolute(), "{}", path.display());
			assert_eq!(path.file_name().unwrap(), "settings.db");
		}
	}

	#[test]
	fn uri_escape_keeps_sqlite_from_reading_a_path_as_options() {
		assert_eq!(uri_escape(Path::new("/tmp/a b/settings.db")), "/tmp/a b/settings.db");
		assert_eq!(uri_escape(Path::new("/tmp/w?t#f%/x.db")), "/tmp/w%3Ft%23f%25/x.db");
	}
}
