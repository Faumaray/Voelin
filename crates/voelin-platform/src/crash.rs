//! Local crash reports (opt-in).
//!
//! [`install`] sets a panic hook that, while reports are enabled, writes one
//! plain-text file per panic into the reports directory: time, app version,
//! OS, thread, panic message and location, and a backtrace. Only the newest
//! [`DEFAULT_KEEP`] reports are kept. Nothing is ever uploaded: the UI lists
//! [`pending_reports`] after a crash and offers to open the folder (so the
//! user can attach a report to an issue) or [`clear`] it.
//!
//! The hook chains to the previous one, so panics still print to stderr and
//! abort or unwind as before. Reports may contain what a panic message
//! includes (e.g. a server address); they stay on the device.
//!
//! ```no_run
//! let dir = voelin_platform::crash::default_dir();
//! voelin_platform::crash::set_app_version(env!("CARGO_PKG_VERSION"));
//! voelin_platform::crash::install(&dir, true); // the user's opt-in setting
//! for report in voelin_platform::crash::pending_reports()? {
//!     println!("{}: {}", report.path.display(), report.summary);
//! }
//! # Ok::<(), std::io::Error>(())
//! ```

use std::backtrace::Backtrace;
use std::cell::Cell;
use std::fmt::Write as _;
use std::io;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, Once, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{APP_NAME, paths};

/// Directory name of the reports under the state directory.
pub const DIR_NAME: &str = "crash-reports";
/// Reports kept by default; older ones are deleted when a new one is written.
pub const DEFAULT_KEEP: usize = 10;

const PREFIX: &str = "crash-";
const SUFFIX: &str = ".txt";

/// `<state dir>/crash-reports` (`%LOCALAPPDATA%\voelin\crash-reports` on
/// Windows, `~/.local/state/voelin/crash-reports` on Linux).
pub fn default_dir() -> PathBuf {
	paths::state_dir().join(DIR_NAME)
}

struct Config {
	dir: Option<PathBuf>,
	version: String,
	keep: usize,
	/// Collected at install time, so the hook does no more IO than needed.
	system: String,
}

static CONFIG: Mutex<Config> = Mutex::new(Config {
	dir: None,
	version: String::new(),
	keep: DEFAULT_KEEP,
	system: String::new(),
});
static ENABLED: AtomicBool = AtomicBool::new(false);
static HOOK: Once = Once::new();
/// Orders reports written by this process within the same millisecond.
static SEQUENCE: AtomicU32 = AtomicU32::new(0);

thread_local! {
	/// Set while this thread writes a report: a panic inside the hook must not
	/// recurse.
	static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

fn config() -> MutexGuard<'static, Config> {
	CONFIG.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Set the panic hook (once per process) and where reports go. `enabled` is
/// the user's opt-in; while it is false panics are not recorded. Calling it
/// again changes the directory and the switch. Existing reports older than
/// the newest [`DEFAULT_KEEP`] (or [`set_keep`]) are deleted.
pub fn install(dir: impl Into<PathBuf>, enabled: bool) {
	let dir = dir.into();
	{
		let mut config = config();
		if config.version.is_empty() {
			config.version = env!("CARGO_PKG_VERSION").into();
		}
		config.system = system_info();
		prune(&dir, config.keep);
		config.dir = Some(dir);
	}
	ENABLED.store(enabled, Ordering::SeqCst);
	HOOK.call_once(|| {
		let previous = std::panic::take_hook();
		std::panic::set_hook(Box::new(move |info| {
			hook(info);
			previous(info);
		}));
	});
}

/// Panic if the `VOELIN_TEST_CRASH` environment variable is set, so testers can
/// check crash reports with a release build (docs/testing/manual-matrix.md).
/// Call right after [`install`].
pub fn test_crash_if_requested() {
	if std::env::var_os("VOELIN_TEST_CRASH").is_some_and(|v| !v.is_empty()) {
		panic!("test crash requested with VOELIN_TEST_CRASH");
	}
}

/// Turn recording on or off (the settings switch). No effect before
/// [`install`].
pub fn set_enabled(enabled: bool) {
	ENABLED.store(enabled, Ordering::SeqCst);
}

pub fn is_enabled() -> bool {
	ENABLED.load(Ordering::SeqCst)
}

/// The app's version for reports; defaults to this crate's version. Call
/// with the binary's `env!("CARGO_PKG_VERSION")`.
pub fn set_app_version(version: impl Into<String>) {
	config().version = version.into();
}

/// How many reports to keep (at least one).
pub fn set_keep(keep: usize) {
	config().keep = keep.max(1);
}

/// The directory given to [`install`].
pub fn dir() -> Option<PathBuf> {
	config().dir.clone()
}

/// A report on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
	pub path: PathBuf,
	/// When the panic happened, from the file name.
	pub time: SystemTime,
	/// The panic message's first line.
	pub summary: String,
}

/// Reports in the [`install`]ed directory, oldest first; empty before
/// [`install`] or when the directory does not exist.
pub fn pending_reports() -> io::Result<Vec<Report>> {
	match dir() {
		Some(dir) => reports_in(&dir),
		None => Ok(Vec::new()),
	}
}

/// Delete all reports in the [`install`]ed directory; returns how many.
pub fn clear() -> io::Result<usize> {
	match dir() {
		Some(dir) => clear_in(&dir),
		None => Ok(0),
	}
}

/// Reports in `dir`, oldest first.
pub fn reports_in(dir: &Path) -> io::Result<Vec<Report>> {
	let mut reports = Vec::new();
	for path in report_files(dir)? {
		let Some(time) = path.file_name().and_then(|n| n.to_str()).and_then(parse_file_time) else {
			continue;
		};
		let text = std::fs::read_to_string(&path).unwrap_or_default();
		let summary = text
			.lines()
			.find_map(|line| line.strip_prefix("Message:"))
			.map(|m| m.trim().to_string())
			.unwrap_or_default();
		reports.push(Report { path, time, summary });
	}
	Ok(reports)
}

/// Delete all reports in `dir`; returns how many.
pub fn clear_in(dir: &Path) -> io::Result<usize> {
	let files = report_files(dir)?;
	for path in &files {
		std::fs::remove_file(path)?;
	}
	Ok(files.len())
}

/// Report files in `dir`, sorted by name, which is by time.
fn report_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
	let entries = match std::fs::read_dir(dir) {
		Ok(entries) => entries,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(e) => return Err(e),
	};
	let mut files: Vec<PathBuf> = entries
		.filter_map(|e| e.ok())
		.map(|e| e.path())
		.filter(|p| {
			p.file_name()
				.and_then(|n| n.to_str())
				.is_some_and(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX))
		})
		.collect();
	files.sort();
	Ok(files)
}

/// Keep the newest `keep` reports.
fn prune(dir: &Path, keep: usize) {
	if let Ok(files) = report_files(dir) {
		for path in files.iter().take(files.len().saturating_sub(keep)) {
			let _ = std::fs::remove_file(path);
		}
	}
}

fn hook(info: &PanicHookInfo<'_>) {
	if !is_enabled() || IN_HOOK.with(|h| h.replace(true)) {
		return;
	}
	match write_report(info) {
		Ok(Some(path)) => eprintln!("crash report written to {}", path.display()),
		Ok(None) => {}
		Err(e) => eprintln!("could not write a crash report: {e}"),
	}
	IN_HOOK.with(|h| h.set(false));
}

fn write_report(info: &PanicHookInfo<'_>) -> io::Result<Option<PathBuf>> {
	// Captured before taking the lock: other threads may panic meanwhile.
	let backtrace = Backtrace::force_capture();
	let (dir, version, keep, system) = {
		let config = config();
		let Some(dir) = config.dir.clone() else { return Ok(None) };
		(dir, config.version.clone(), config.keep, config.system.clone())
	};
	let now = SystemTime::now();
	let thread = std::thread::current();
	let report = format_report(&ReportFields {
		time: now,
		version: &version,
		system: &system,
		thread: thread.name().unwrap_or("<unnamed>"),
		thread_id: &format!("{:?}", thread.id()),
		location: &info.location().map(|l| l.to_string()).unwrap_or_default(),
		message: info.payload_as_str().unwrap_or("<non-string panic payload>"),
		backtrace: &backtrace.to_string(),
	});

	std::fs::create_dir_all(&dir)?;
	let name = file_name(now, SEQUENCE.fetch_add(1, Ordering::SeqCst), std::process::id());
	let path = dir.join(&name);
	// Written under another name first, so a half-written file is never listed.
	let partial = dir.join(format!("{name}.partial"));
	std::fs::write(&partial, report)?;
	std::fs::rename(&partial, &path)?;
	prune(&dir, keep);
	Ok(Some(path))
}

struct ReportFields<'a> {
	time: SystemTime,
	version: &'a str,
	system: &'a str,
	thread: &'a str,
	thread_id: &'a str,
	location: &'a str,
	message: &'a str,
	backtrace: &'a str,
}

fn format_report(r: &ReportFields<'_>) -> String {
	let mut out = String::new();
	let _ = writeln!(out, "{APP_NAME} crash report");
	let _ = writeln!(out);
	let _ = writeln!(out, "Time:     {}", rfc3339(r.time));
	let _ = writeln!(out, "Version:  {}", r.version);
	let _ = writeln!(out, "System:   {}", r.system);
	let _ = writeln!(out, "Thread:   {} ({})", r.thread, r.thread_id);
	let _ = writeln!(out, "Location: {}", r.location);
	// The message may span lines; the first stays on the "Message:" line.
	let mut lines = r.message.lines();
	let _ = writeln!(out, "Message:  {}", lines.next().unwrap_or(""));
	for line in lines {
		let _ = writeln!(out, "          {line}");
	}
	let _ = writeln!(out);
	let _ = writeln!(out, "Backtrace:");
	let _ = writeln!(out, "{}", r.backtrace);
	out
}

/// OS, architecture, distribution and session: what helps to reproduce.
fn system_info() -> String {
	let mut info = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
	#[cfg(target_os = "linux")]
	{
		let os_release = std::fs::read_to_string("/etc/os-release")
			.or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
			.unwrap_or_default();
		if let Some(name) = os_release
			.lines()
			.find_map(|l| l.strip_prefix("PRETTY_NAME="))
			.map(|v| v.trim_matches('"'))
		{
			let _ = write!(info, ", {name}");
		}
		if let Ok(kernel) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
			let _ = write!(info, ", kernel {}", kernel.trim());
		}
	}
	let _ = write!(info, ", {:?}", paths::display_server());
	if paths::is_flatpak() {
		info.push_str(", Flatpak");
	}
	info
}

/// `crash-20260926T101500.123Z-0001-4242.txt`: time, sequence, process id.
/// Sorting the names sorts the reports by time.
fn file_name(time: SystemTime, sequence: u32, pid: u32) -> String {
	let (date, clock, millis) = civil(time);
	let (y, mo, d) = date;
	let (h, mi, s) = clock;
	format!(
		"{PREFIX}{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}.{millis:03}Z-{:04}-{pid}{SUFFIX}",
		sequence % 10_000
	)
}

/// The time from a name made by [`file_name`].
fn parse_file_time(name: &str) -> Option<SystemTime> {
	let stamp = name.strip_prefix(PREFIX)?.get(..19)?;
	let num = |range: std::ops::Range<usize>| stamp.get(range)?.parse::<u64>().ok();
	let (y, mo, d) = (num(0..4)?, num(4..6)?, num(6..8)?);
	let (h, mi, s, ms) = (num(9..11)?, num(11..13)?, num(13..15)?, num(16..19)?);
	if stamp.as_bytes()[8] != b'T' || stamp.as_bytes()[15] != b'.' || !(1..=12).contains(&mo) {
		return None;
	}
	let days = days_from_civil(y as i64, mo as u32, d as u32);
	let secs = u64::try_from(days).ok()? * 86_400 + h * 3600 + mi * 60 + s;
	Some(UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(ms))
}

/// UTC, `2026-09-26T10:15:00.123Z`.
fn rfc3339(time: SystemTime) -> String {
	let ((y, mo, d), (h, mi, s), ms) = civil(time);
	format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

type Date = (i64, u32, u32);
type Clock = (u32, u32, u32);

/// UTC calendar date, time of day and milliseconds (clamped to the epoch).
fn civil(time: SystemTime) -> (Date, Clock, u32) {
	let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
	let secs = since.as_secs();
	let days = (secs / 86_400) as i64;
	let rem = (secs % 86_400) as u32;
	(civil_from_days(days), (rem / 3600, rem / 60 % 60, rem % 60), since.subsec_millis())
}

// Howard Hinnant's algorithms (http://howardhinnant.github.io/date_algorithms.html).
fn civil_from_days(days: i64) -> Date {
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z.rem_euclid(146_097);
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
	let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
	let y = yoe + era * 400 + i64::from(m <= 2);
	(y, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
	let y = if m <= 2 { y - 1 } else { y };
	let era = y.div_euclid(400);
	let yoe = y.rem_euclid(400);
	let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
	let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
	let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
	era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
	use super::*;

	fn at(secs: u64, millis: u64) -> SystemTime {
		UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis)
	}

	#[test]
	fn dates() {
		assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
		// 2026-09-26T10:15:00Z
		assert_eq!(rfc3339(at(1_790_417_700, 123)), "2026-09-26T10:15:00.123Z");
		// Leap day.
		assert_eq!(rfc3339(at(951_825_600, 0)), "2000-02-29T12:00:00.000Z");
		for days in [-1, 0, 59, 60, 365, 11_016, 20_722, 40_000] {
			let (y, m, d) = civil_from_days(days);
			assert_eq!(days_from_civil(y, m, d), days);
		}
	}

	#[test]
	fn file_names_sort_and_parse() {
		let time = at(1_790_417_700, 7);
		let name = file_name(time, 3, 4242);
		assert_eq!(name, "crash-20260926T101500.007Z-0003-4242.txt");
		assert_eq!(parse_file_time(&name), Some(time));
		assert!(file_name(time, 3, 1) < file_name(time, 4, 1));
		assert!(file_name(time, 9, 1) < file_name(at(1_790_417_700, 8), 0, 1));
		assert_eq!(parse_file_time("crash-garbage.txt"), None);
		assert_eq!(parse_file_time("crash-20261326T101500.007Z-0-1.txt"), None);
	}

	#[test]
	fn report_format() {
		let report = format_report(&ReportFields {
			time: at(1_790_417_700, 0),
			version: "1.2.3",
			system: "linux x86_64",
			thread: "audio",
			thread_id: "ThreadId(7)",
			location: "src/audio.rs:10:5",
			message: "first\nsecond",
			backtrace: "   0: frame",
		});
		assert!(report.starts_with(&format!("{APP_NAME} crash report\n")));
		assert!(report.contains("Time:     2026-09-26T10:15:00.000Z\n"));
		assert!(report.contains("Version:  1.2.3\n"));
		assert!(report.contains("Thread:   audio (ThreadId(7))\n"));
		assert!(report.contains("Message:  first\n          second\n"));
		assert!(report.contains("Backtrace:\n   0: frame\n"));
	}

	#[test]
	fn system_info_names_os() {
		assert!(system_info().starts_with(std::env::consts::OS));
	}

	#[test]
	fn missing_dir_has_no_reports() {
		let dir = std::env::temp_dir().join(format!("voelin-crash-missing-{}", std::process::id()));
		assert!(reports_in(&dir).unwrap().is_empty());
		assert_eq!(clear_in(&dir).unwrap(), 0);
	}
}
