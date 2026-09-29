//! Asking the user for a file.
//!
//! Linux uses xdg-desktop-portal's file chooser (feature `portal`), which
//! works on Wayland and X11 and inside a Flatpak. Other platforms have no
//! backend yet; [`available`] says so, so a window can hide what it cannot
//! offer.

use std::path::PathBuf;

use crate::{Error, Result};

/// Whether [`pick_file`] can ask the user.
pub const fn available() -> bool {
	cfg!(all(unix, not(target_os = "macos"), feature = "portal"))
}

/// Ask the user for one existing file; `None` when they cancelled.
pub async fn pick_file(title: &str) -> Result<Option<PathBuf>> {
	#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
	return portal_pick(title).await;
	#[cfg(not(all(unix, not(target_os = "macos"), feature = "portal")))]
	{
		let _ = title;
		Err(Error::Unsupported("no file chooser on this platform".into()))
	}
}

#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
async fn portal_pick(title: &str) -> Result<Option<PathBuf>> {
	use ashpd::desktop::file_chooser::SelectedFiles;

	let request = SelectedFiles::open_file()
		.title(title)
		.multiple(false)
		.modal(true)
		.send()
		.await
		.map_err(|e| Error::Backend(e.to_string()))?;
	let files = match request.response() {
		Ok(files) => files,
		// Cancelling is not a failure.
		Err(ashpd::Error::Response(_)) => return Ok(None),
		Err(e) => return Err(Error::Backend(e.to_string())),
	};
	Ok(files.uris().first().and_then(|uri| file_uri_path(uri.as_str())))
}

/// The path of a `file://` URI, with percent escapes resolved.
#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
fn file_uri_path(uri: &str) -> Option<PathBuf> {
	let rest = uri.strip_prefix("file://")?;
	// file:///path and file://localhost/path both mean a local file.
	let path = rest.strip_prefix("localhost").unwrap_or(rest);
	let bytes = path.as_bytes();
	let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%' && i + 2 < bytes.len() {
			let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
			if let Ok(byte) = u8::from_str_radix(hex, 16) {
				out.push(byte);
				i += 3;
				continue;
			}
		}
		out.push(bytes[i]);
		i += 1;
	}
	let path = PathBuf::from(String::from_utf8(out).ok()?);
	path.is_absolute().then_some(path)
}

#[cfg(all(test, unix, not(target_os = "macos"), feature = "portal"))]
mod tests {
	use super::*;

	#[test]
	fn paths_of_file_uris() {
		assert_eq!(file_uri_path("file:///tmp/a%20b.png"), Some("/tmp/a b.png".into()));
		assert_eq!(file_uri_path("file://localhost/tmp/x"), Some("/tmp/x".into()));
		assert_eq!(file_uri_path("https://example.com/x"), None);
		assert_eq!(file_uri_path("file://relative"), None);
	}
}
