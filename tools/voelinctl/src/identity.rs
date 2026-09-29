//! Load, create and store client identities as JSON files, and move them in
//! and out of the client database (`voelin_core::identity`).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tsclientlib::Identity;
use voelin_core::identity::{self as ident, Found, Imported};
use voelin_store::Store;

pub fn default_path() -> PathBuf {
	dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("voelinctl").join("identity.json")
}

/// The client database the desktop app uses.
fn default_store() -> PathBuf {
	voelin_platform::paths::data_dir().join("client.db")
}

fn open_store(path: Option<PathBuf>) -> Result<Store> {
	let path = path.unwrap_or_else(default_store);
	Store::open(&path).with_context(|| format!("failed to open {}", path.display()))
}

/// Identities in the client database, with the id `export` takes.
pub fn list(store: Option<PathBuf>) -> Result<()> {
	for entry in open_store(store)?.identities()? {
		println!("{:>3}  {:<20} {}  level {}", entry.id, entry.name, entry.uid, entry.level);
	}
	Ok(())
}

/// Import from the official clients. Only public facts are printed: the
/// nickname, the unique id and the security level.
pub fn import(from: Option<PathBuf>, dry_run: bool, store: Option<PathBuf>) -> Result<()> {
	let paths = match from {
		Some(path) => vec![path],
		None => ident::discover(),
	};
	if paths.is_empty() {
		anyhow::bail!("no TeamSpeak client data in the usual locations; pass --from <path>");
	}
	let mut found: Vec<Found> = Vec::new();
	for path in &paths {
		match ident::read(path) {
			Ok(identities) => found.extend(identities),
			// Searching the usual locations turns up files without
			// identities; that is not a reason to stop.
			Err(e) => eprintln!("{e}"),
		}
	}
	if found.is_empty() {
		anyhow::bail!("no identity found in {} file(s)", paths.len());
	}
	// `Some(outcome)` once something was stored.
	let outcomes: Vec<Option<Imported>> = if dry_run {
		vec![None; found.len()]
	} else {
		ident::import(&open_store(store)?, &found)?.into_iter().map(Some).collect()
	};
	let mut source: Option<&Path> = None;
	for (identity, outcome) in found.iter().zip(&outcomes) {
		if source != Some(&identity.source) {
			println!("{}", identity.source.display());
			source = Some(&identity.source);
		}
		let status = match outcome {
			None => "would import".to_owned(),
			Some(Imported::Added(id)) => format!("imported as {id}"),
			Some(Imported::Duplicate) => "already in the store".to_owned(),
		};
		println!(
			"  {:<20} {}  level {:<3} {status}",
			identity.nickname,
			identity.uid(),
			identity.level()
		);
	}
	Ok(())
}

/// Write a stored identity out in the TeamSpeak 3 `.ini` form.
pub fn export(id: i64, path: &Path, store: Option<PathBuf>) -> Result<()> {
	let store = open_store(store)?;
	let identity = store.identity(id)?;
	let name = store
		.identities()?
		.into_iter()
		.find(|entry| entry.id == id)
		.map_or_else(|| "voelin".to_owned(), |entry| entry.name);
	ident::write_export(path, &name, &identity)?;
	println!("wrote {} ({})", path.display(), uid(&identity));
	Ok(())
}

pub fn load(path: &Path) -> Result<Identity> {
	let data = fs::read_to_string(path)
		.with_context(|| format!("failed to read identity {}", path.display()))?;
	serde_json::from_str(&data).with_context(|| format!("invalid identity {}", path.display()))
}

pub fn save(path: &Path, identity: &Identity) -> Result<()> {
	if let Some(dir) = path.parent() {
		fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
	}
	let data = serde_json::to_string_pretty(identity)?;
	fs::write(path, data).with_context(|| format!("failed to write {}", path.display()))
}

/// Create a new identity with at least the given security level.
///
/// Levels above ~24 take minutes; the work runs on the calling thread.
pub fn create(level: u8) -> Identity {
	let mut identity = Identity::create();
	if identity.level() < level {
		identity.upgrade_level(level);
	}
	identity
}

pub fn uid(identity: &Identity) -> String {
	identity.key().to_pub().get_uid()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn roundtrip() {
		let dir = std::env::temp_dir().join(format!("voelinctl-id-test-{}", std::process::id()));
		let path = dir.join("id.json");
		let identity = create(8);
		assert!(identity.level() >= 8);
		save(&path, &identity).unwrap();
		let loaded = load(&path).unwrap();
		assert_eq!(uid(&loaded), uid(&identity));
		assert_eq!(loaded.counter(), identity.counter());
		fs::remove_dir_all(dir).unwrap();
	}
}
