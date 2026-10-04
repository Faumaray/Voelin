//! The app's log file: `<state>/logs/voelin.log` for this run, the previous
//! runs' as `voelin.1.log` (the last one) to `voelin.4.log`. A run that writes
//! more than [`MAX_BYTES`] starts a fresh file (the full one moves to
//! `voelin.1.log`), so the logs never fill the disk.
//!
//! ```no_run
//! let log = voelin_platform::logs::LogFile::open(voelin_platform::logs::default_dir())?;
//! // tracing_subscriber::fmt::layer().with_ansi(false).with_writer(move || log.clone())
//! # Ok::<(), std::io::Error>(())
//! ```

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::paths;

/// The log directory's name under the state directory.
pub const DIR_NAME: &str = "logs";
/// Files kept: this run's and the previous runs'.
pub const KEEP: usize = 5;
/// One file's limit.
pub const MAX_BYTES: u64 = 20 << 20;

const NAME: &str = "voelin";

/// The file this process writes, once [`LogFile::open`] succeeded.
static CURRENT: OnceLock<PathBuf> = OnceLock::new();

/// `<state>/logs`.
pub fn default_dir() -> PathBuf {
	paths::state_dir().join(DIR_NAME)
}

/// The log file of this run, if there is one.
pub fn current() -> Option<&'static Path> {
	CURRENT.get().map(PathBuf::as_path)
}

/// A log file to write to from any thread; clones write to the same file.
#[derive(Clone)]
pub struct LogFile(Arc<Mutex<Inner>>);

struct Inner {
	dir: PathBuf,
	file: File,
	written: u64,
	keep: usize,
	max: u64,
}

impl LogFile {
	/// Start this run's file in `dir`, moving the previous runs' along.
	pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
		Self::open_with(dir.into(), KEEP, MAX_BYTES)
	}

	fn open_with(dir: PathBuf, keep: usize, max: u64) -> io::Result<Self> {
		std::fs::create_dir_all(&dir)?;
		let file = start(&dir, keep)?;
		let _ = CURRENT.set(dir.join(file_name(0)));
		Ok(Self(Arc::new(Mutex::new(Inner { dir, file, written: 0, keep, max }))))
	}

	/// The file being written.
	pub fn path(&self) -> PathBuf {
		self.lock().dir.join(file_name(0))
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
		self.0.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

impl Write for LogFile {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		let mut inner = self.lock();
		if inner.written > 0 && inner.written + buf.len() as u64 > inner.max {
			inner.file = start(&inner.dir, inner.keep)?;
			inner.written = 0;
		}
		let n = inner.file.write(buf)?;
		inner.written += n as u64;
		Ok(n)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.lock().file.flush()
	}
}

/// `voelin.log`, `voelin.1.log`, …
fn file_name(n: usize) -> String {
	if n == 0 { format!("{NAME}.log") } else { format!("{NAME}.{n}.log") }
}

/// Move the files one along (the oldest goes) and open a new current one.
fn start(dir: &Path, keep: usize) -> io::Result<File> {
	let keep = keep.max(1);
	let _ = std::fs::remove_file(dir.join(file_name(keep - 1)));
	for n in (0..keep - 1).rev() {
		let from = dir.join(file_name(n));
		if from.exists() {
			let _ = std::fs::rename(&from, dir.join(file_name(n + 1)));
		}
	}
	OpenOptions::new().create(true).write(true).truncate(true).open(dir.join(file_name(0)))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn temp_dir(name: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"voelin-logs-{name}-{}-{:?}",
			std::process::id(),
			std::thread::current().id()
		));
		let _ = std::fs::remove_dir_all(&dir);
		dir
	}

	fn read(dir: &Path, n: usize) -> Option<String> {
		std::fs::read_to_string(dir.join(file_name(n))).ok()
	}

	#[test]
	fn each_run_gets_a_file_and_the_oldest_go() {
		let dir = temp_dir("runs");
		for run in 0..4 {
			let mut log = LogFile::open_with(dir.clone(), 3, MAX_BYTES).unwrap();
			writeln!(log, "run {run}").unwrap();
		}
		assert_eq!(read(&dir, 0).as_deref(), Some("run 3\n"));
		assert_eq!(read(&dir, 1).as_deref(), Some("run 2\n"));
		assert_eq!(read(&dir, 2).as_deref(), Some("run 1\n"));
		assert_eq!(read(&dir, 3), None, "only three files are kept");
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn a_full_file_starts_over() {
		let dir = temp_dir("full");
		let log = LogFile::open_with(dir.clone(), 3, 10).unwrap();
		let mut writer = log.clone();
		writer.write_all(b"12345678\n").unwrap();
		// Clones share the file and its size.
		log.clone().write_all(b"abcdef\n").unwrap();
		assert_eq!(read(&dir, 0).as_deref(), Some("abcdef\n"));
		assert_eq!(read(&dir, 1).as_deref(), Some("12345678\n"));
		assert_eq!(log.path(), dir.join("voelin.log"));
		// A line longer than the limit still goes out whole.
		writer.write_all(b"a line longer than ten bytes\n").unwrap();
		assert_eq!(read(&dir, 0).as_deref(), Some("a line longer than ten bytes\n"));
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
