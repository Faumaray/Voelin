//! Where the log goes: standard output (journald, `docker logs`) and, with
//! `log.file`, a file as well.
//!
//! What is logged comes from `log.level` (`TSGW_LOG_LEVEL`), else
//! `RUST_LOG`, else [`DEFAULT_FILTER`]. The libraries that log every packet
//! stay at warnings unless the filter names them, so `debug` stays readable.

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use tracing::warn;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use crate::config::Bootstrap;

/// What is logged when nothing says otherwise.
pub const DEFAULT_FILTER: &str = "info,russh=warn,hyper=warn,h2=warn,tower=warn,tungstenite=warn";

/// Libraries kept at warnings unless a filter names them.
const NOISY: &[&str] = &["russh", "hyper", "h2", "tower", "tungstenite", "tokio_tungstenite"];

/// One log file's limit before it moves aside.
const MAX_BYTES: u64 = 20 << 20;
/// Files kept: the one written and the ones moved aside.
const KEEP: usize = 5;

/// Start logging. Problems with the filter or the file are logged, not
/// fatal. Returns the log file in use.
pub fn init(boot: &Bootstrap) -> Option<PathBuf> {
	let rust_log = std::env::var("RUST_LOG").ok();
	let (directives, from) = match (&boot.log_level, &rust_log) {
		(Some(level), _) => (level.as_str(), "log.level"),
		(None, Some(rust_log)) if !rust_log.trim().is_empty() => (rust_log.as_str(), "RUST_LOG"),
		_ => (DEFAULT_FILTER, "default"),
	};
	let (filter, bad_filter) = match EnvFilter::try_new(quiet_noisy(directives)) {
		Ok(filter) => (filter, None),
		Err(error) => (EnvFilter::new(DEFAULT_FILTER), Some(error)),
	};
	let ansi = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
	let console = fmt::layer().with_ansi(ansi);
	let file = boot.log_file.as_deref().map(|path| (path, LogFile::open(path, KEEP, MAX_BYTES)));
	let to_file = match &file {
		Some((_, Ok(log))) => {
			let log = log.clone();
			Some(fmt::layer().with_ansi(false).with_writer(move || log.clone()))
		}
		_ => None,
	};
	tracing_subscriber::registry().with(filter).with(console).with(to_file).init();
	if let Some(error) = bad_filter {
		warn!(filter = directives, from, %error, "not a log filter; using the default");
	}
	match file {
		Some((path, Ok(_))) => Some(path.to_path_buf()),
		Some((path, Err(error))) => {
			warn!(
				file = %path.display(),
				%error,
				"cannot write the log file; logging to standard output only"
			);
			None
		}
		None => None,
	}
}

/// `directives` with the libraries in [`NOISY`] it does not name at
/// warnings.
fn quiet_noisy(directives: &str) -> String {
	let named = |lib: &str| {
		directives.split(',').any(|d| {
			let target = d.trim().split(['=', '[']).next().unwrap_or_default();
			target == lib || target.starts_with(&format!("{lib}::"))
		})
	};
	let mut filter = directives.trim().trim_end_matches(',').to_string();
	for lib in NOISY.iter().filter(|lib| !named(lib)) {
		filter.push_str(&format!(",{lib}=warn"));
	}
	filter
}

/// A log file written by appending, also across restarts. When it would
/// grow past its limit it moves to `<name>.1.<ext>` (the older ones one
/// further along, the oldest dropped) and a new one starts. Clones write to
/// the same file.
#[derive(Clone)]
pub struct LogFile(Arc<Mutex<Inner>>);

struct Inner {
	path: PathBuf,
	file: File,
	written: u64,
	keep: usize,
	max: u64,
}

impl LogFile {
	pub fn open(path: &Path, keep: usize, max: u64) -> io::Result<Self> {
		if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
			std::fs::create_dir_all(dir)?;
		}
		let file = append(path)?;
		let written = file.metadata()?.len();
		let inner = Inner { path: path.to_path_buf(), file, written, keep: keep.max(1), max };
		Ok(Self(Arc::new(Mutex::new(inner))))
	}
}

impl Write for LogFile {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		let mut inner = self.0.lock().unwrap_or_else(PoisonError::into_inner);
		if inner.written > 0 && inner.written + buf.len() as u64 > inner.max {
			rotate(&inner.path, inner.keep);
			// If no new file can be made, the moved one goes on: moving the
			// files along on every line would soon drop them all.
			if let Ok(file) = append(&inner.path) {
				inner.file = file;
			}
			inner.written = 0;
		}
		let n = inner.file.write(buf)?;
		inner.written += n as u64;
		Ok(n)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.0.lock().unwrap_or_else(PoisonError::into_inner).file.flush()
	}
}

fn append(path: &Path) -> io::Result<File> {
	OpenOptions::new().create(true).append(true).open(path)
}

/// `tsgw.log` as the `n`th one moved aside: `tsgw.1.log`.
fn moved_aside(path: &Path, n: usize) -> PathBuf {
	let stem = path.file_stem().unwrap_or_default().to_string_lossy();
	let name = match path.extension() {
		Some(ext) => format!("{stem}.{n}.{}", ext.to_string_lossy()),
		None => format!("{stem}.{n}"),
	};
	path.with_file_name(name)
}

/// Move the files one along; the oldest goes.
fn rotate(path: &Path, keep: usize) {
	if keep <= 1 {
		let _ = std::fs::remove_file(path);
		return;
	}
	let _ = std::fs::remove_file(moved_aside(path, keep - 1));
	for n in (1..keep - 1).rev() {
		let _ = std::fs::rename(moved_aside(path, n), moved_aside(path, n + 1));
	}
	let _ = std::fs::rename(path, moved_aside(path, 1));
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn noisy_libraries_stay_quiet_unless_named() {
		assert_eq!(
			quiet_noisy("debug"),
			"debug,russh=warn,hyper=warn,h2=warn,tower=warn,tungstenite=warn,tokio_tungstenite=warn"
		);
		let named = quiet_noisy("info,voelin_query=trace,russh=debug,hyper::proto=info,");
		assert!(named.starts_with("info,voelin_query=trace,russh=debug,hyper::proto=info,h2=warn"));
		assert!(!named.contains("russh=warn") && !named.contains("hyper=warn"));
		assert!(EnvFilter::try_new(quiet_noisy(DEFAULT_FILTER)).is_ok());
		assert_eq!(quiet_noisy(DEFAULT_FILTER).matches("russh").count(), 1);
	}

	#[test]
	fn the_file_is_appended_to_and_moved_aside_when_full() {
		let dir = std::env::temp_dir().join(format!("tsgw-log-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let path = dir.join("logs").join("tsgw.log");
		let read = |p: PathBuf| std::fs::read_to_string(p).ok();
		LogFile::open(&path, 3, 12).unwrap().write_all(b"start 1\n").unwrap();
		// A restart goes on in the same file.
		let log = LogFile::open(&path, 3, 12).unwrap();
		log.clone().write_all(b"two\n").unwrap();
		assert_eq!(read(path.clone()).as_deref(), Some("start 1\ntwo\n"));
		// Full: moved aside, and the oldest goes.
		log.clone().write_all(b"three\n").unwrap();
		log.clone().write_all(b"four 4\n").unwrap();
		log.clone().write_all(b"five 5\n").unwrap();
		assert_eq!(read(path.clone()).as_deref(), Some("five 5\n"));
		assert_eq!(read(dir.join("logs/tsgw.1.log")).as_deref(), Some("four 4\n"));
		assert_eq!(read(dir.join("logs/tsgw.2.log")).as_deref(), Some("three\n"));
		assert_eq!(read(dir.join("logs/tsgw.3.log")), None, "three files in all");
		assert_eq!(moved_aside(Path::new("/var/log/tsgw"), 2), Path::new("/var/log/tsgw.2"));
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
