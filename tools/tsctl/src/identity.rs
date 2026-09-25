//! Load, create and store client identities as JSON files.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tsclientlib::Identity;

pub fn default_path() -> PathBuf {
	dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("tsctl").join("identity.json")
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
		let dir = std::env::temp_dir().join(format!("tsctl-id-test-{}", std::process::id()));
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
