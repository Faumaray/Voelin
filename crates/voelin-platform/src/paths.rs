//! Where the app keeps its files.
//!
//! `<base>/voelin` for the platform's data, config, cache and state bases (XDG
//! on Linux, where Flatpak points them into `~/.var/app/<app id>/`; Known
//! Folders on Windows: `%APPDATA%` for data and config, `%LOCALAPPDATA%` for
//! cache and state). Each can be overridden with an environment variable,
//! e.g. for portable installs or tests.

use std::path::{Path, PathBuf};

/// Directory name under each base; the desktop app's store already lives in
/// `<data>/voelin`.
pub const APP_DIR: &str = "voelin";

fn resolve(var: &str, base: Option<PathBuf>) -> PathBuf {
	match std::env::var_os(var).filter(|v| !v.is_empty()) {
		Some(dir) => dir.into(),
		None => base.unwrap_or_else(|| PathBuf::from(".")).join(APP_DIR),
	}
}

/// Identities, bookmarks, chat history. `VOELIN_DATA_DIR` overrides.
pub fn data_dir() -> PathBuf {
	resolve("VOELIN_DATA_DIR", dirs::data_dir())
}

/// Settings files. `VOELIN_CONFIG_DIR` overrides.
pub fn config_dir() -> PathBuf {
	resolve("VOELIN_CONFIG_DIR", dirs::config_dir())
}

/// Disposable files (avatars, downloaded codecs). `VOELIN_CACHE_DIR` overrides.
pub fn cache_dir() -> PathBuf {
	resolve("VOELIN_CACHE_DIR", dirs::cache_dir())
}

/// Logs and crash reports. `VOELIN_STATE_DIR` overrides.
pub fn state_dir() -> PathBuf {
	resolve("VOELIN_STATE_DIR", dirs::state_dir().or_else(dirs::data_local_dir))
}

/// Create `dir` and its parents if needed.
pub fn ensure(dir: &Path) -> std::io::Result<&Path> {
	std::fs::create_dir_all(dir)?;
	Ok(dir)
}

/// Running inside a Flatpak sandbox.
pub fn is_flatpak() -> bool {
	cfg!(target_os = "linux") && Path::new("/.flatpak-info").exists()
}

/// The display server of this session, as far as the environment tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayServer {
	Wayland,
	X11,
	Windows,
	MacOs,
	Unknown,
}

pub fn display_server() -> DisplayServer {
	if cfg!(windows) {
		DisplayServer::Windows
	} else if cfg!(target_os = "macos") {
		DisplayServer::MacOs
	} else if std::env::var_os("WAYLAND_DISPLAY").is_some()
		|| std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
	{
		DisplayServer::Wayland
	} else if std::env::var_os("DISPLAY").is_some() {
		DisplayServer::X11
	} else {
		DisplayServer::Unknown
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn dirs_end_in_app_dir_or_override() {
		for dir in [data_dir(), config_dir(), cache_dir(), state_dir()] {
			assert!(dir.is_absolute() || dir.starts_with("."), "{}", dir.display());
		}
		assert_eq!(
			resolve("VOELIN_TEST_UNSET_VARIABLE", Some("/base".into())),
			Path::new("/base/voelin")
		);
		assert_eq!(resolve("VOELIN_TEST_UNSET_VARIABLE", None), Path::new("./voelin"));
		// PATH is always set: stands in for an override.
		let path = std::env::var_os("PATH").unwrap();
		assert_eq!(resolve("PATH", Some("/base".into())), PathBuf::from(path));
	}

	#[test]
	fn ensure_creates() {
		let dir =
			std::env::temp_dir().join(format!("voelin-platform-{}", std::process::id())).join("a");
		assert_eq!(ensure(&dir).unwrap(), dir);
		assert!(dir.is_dir());
		std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
	}
}
