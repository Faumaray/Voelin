//! The crash report panic hook, with real panics in threads and in a child
//! process. Its own test binary: the hook is process-wide.

use std::path::PathBuf;
use std::thread;

use voelin_platform::crash;

fn temp_dir(name: &str) -> PathBuf {
	let dir = std::env::temp_dir().join(format!("voelin-crash-{name}-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	dir
}

/// Panic in a named thread and wait for it.
fn panic_in_thread(name: &str, message: String) {
	let result =
		thread::Builder::new().name(name.into()).spawn(move || panic!("{message}")).unwrap().join();
	assert!(result.is_err(), "the thread should have panicked");
}

// The only test that installs the hook in this process: the hook and its
// settings are global. The child process test below only spawns.
#[test]
fn panic_hook_writes_reports() {
	let dir = temp_dir("hook");
	crash::set_app_version("9.8.7-test");
	crash::install(&dir, false);
	assert_eq!(crash::dir().as_deref(), Some(dir.as_path()));

	// Disabled: nothing is written.
	panic_in_thread("quiet", "not recorded".into());
	assert!(crash::pending_reports().unwrap().is_empty());

	// Enabled: one report with the details.
	crash::set_enabled(true);
	assert!(crash::is_enabled());
	panic_in_thread("crash-test", "boom 42\nsecond line".into());
	let reports = crash::pending_reports().unwrap();
	assert_eq!(reports.len(), 1, "{reports:?}");
	let report = &reports[0];
	assert_eq!(report.summary, "boom 42");
	let age = report.time.elapsed().unwrap_or_default();
	assert!(age.as_secs() < 60, "report time {:?} ago", age);
	let text = std::fs::read_to_string(&report.path).unwrap();
	eprintln!("{text}");
	assert!(text.contains("Version:  9.8.7-test"), "{text}");
	assert!(text.contains("Thread:   crash-test ("), "{text}");
	assert!(text.contains("Message:  boom 42\n          second line"), "{text}");
	assert!(text.contains(&format!("System:   {}", std::env::consts::OS)), "{text}");
	// The path separator is the platform's.
	let location = text.lines().find(|l| l.starts_with("Location: ")).unwrap_or_default();
	assert!(location.contains("tests") && location.contains("crash.rs:"), "{text}");
	assert!(text.contains("Backtrace:\n"), "{text}");
	// Symbols resolve on Linux (MSVC needs the PDB files next to the binary).
	if cfg!(target_os = "linux") {
		assert!(text.contains("panic_in_thread"), "{text}");
	}
	// No partial files are left behind.
	assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

	// Only the newest reports are kept.
	crash::set_keep(3);
	for i in 0..5 {
		panic_in_thread("crash-test", format!("panic {i}"));
	}
	let summaries: Vec<_> =
		crash::pending_reports().unwrap().into_iter().map(|r| r.summary).collect();
	assert_eq!(summaries, ["panic 2", "panic 3", "panic 4"]);

	// Reports survive a restart (a new install) and can be deleted.
	crash::install(&dir, true);
	assert_eq!(crash::pending_reports().unwrap().len(), 3);
	assert_eq!(crash::clear().unwrap(), 3);
	assert!(crash::pending_reports().unwrap().is_empty());

	// A directory that does not exist yet is created on the first report.
	let nested = dir.join("a").join("b");
	crash::install(&nested, true);
	panic_in_thread("crash-test", "into a new directory".into());
	assert_eq!(crash::reports_in(&nested).unwrap().len(), 1);

	crash::set_enabled(false);
	std::fs::remove_dir_all(&dir).unwrap();
}

/// Runs only as the child of `test_crash_in_child_process`: a start with
/// `VOELIN_TEST_CRASH` set, as a tester would do with the app.
#[test]
fn child_process_crash() {
	let Some(dir) = std::env::var_os("VOELIN_CRASH_CHILD_DIR") else { return };
	crash::set_app_version("child");
	crash::install(PathBuf::from(dir), true);
	crash::test_crash_if_requested();
	unreachable!("VOELIN_TEST_CRASH is set");
}

#[test]
fn test_crash_in_child_process() {
	let dir = temp_dir("child");
	// Captured: the child's "FAILED" would look like a failure of this run.
	let output = std::process::Command::new(std::env::current_exe().unwrap())
		.args(["--exact", "child_process_crash", "--test-threads=1"])
		.env("VOELIN_CRASH_CHILD_DIR", &dir)
		.env("VOELIN_TEST_CRASH", "1")
		.output()
		.unwrap();
	let log = String::from_utf8_lossy(&output.stderr);
	assert!(!output.status.success(), "the child should have panicked: {log}");
	// The next start finds the report.
	let reports = crash::reports_in(&dir).unwrap();
	assert_eq!(reports.len(), 1, "{reports:?}");
	assert_eq!(reports[0].summary, "test crash requested with VOELIN_TEST_CRASH");
	let text = std::fs::read_to_string(&reports[0].path).unwrap();
	assert!(text.contains("Version:  child"), "{text}");
	std::fs::remove_dir_all(&dir).unwrap();
}
