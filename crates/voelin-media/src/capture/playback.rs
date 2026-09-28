//! What applications play, captured into stream mixer sources: all of it
//! except our own process tree (so viewers do not hear the TeamSpeak voices
//! we play), or chosen applications only; and the list of applications
//! that play, for a picker.
//!
//! | Platform | All but ours | One application | Playing apps |
//! |---|---|---|---|
//! | Linux (PipeWire) | links from every other playback stream into a capture stream of ours ([`super::pipewire_links`]) | links from that application's streams | playback streams in the registry |
//! | Windows | WASAPI process loopback excluding our process tree | process loopback including the application's process tree | audio sessions of the default device |
//! | Android | `AudioPlaybackCapture` excluding our uid | matching the application's uid | launchable apps (Android cannot tell which play) |
//!
//! [`start_playback`] starts one; the capture feeds the given
//! [`SourceHandle`] (through one or more inputs) until it is dropped.
//! [`forward_audio`] feeds any [`AudioCapture`] into a source instead.

use std::time::Duration;

use tokio::sync::watch;

use crate::capture::{AudioCapture, Worker};
use crate::mix::SourceHandle;
use crate::{Error, Result};

/// Which playback a capture takes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PlaybackFilter {
	/// Everything that plays except this process and its children (our
	/// voices and watched streams).
	AllButSelf,
	/// One application's playback.
	App(AppMatch),
}

/// How an application is recognised.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AppMatch {
	/// A process and its children (Linux, Windows): the process that owns a
	/// window, or an entry of the playing-apps list. Ends with the process.
	Pid(u32),
	/// By name, ignoring case: the application name or executable (Linux,
	/// Windows; `.exe` optional) or the package name (Android). A restarted
	/// application is found again.
	Name(String),
}

impl AppMatch {
	/// Whether one of `names` is this name.
	pub fn matches_name<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> bool {
		let AppMatch::Name(wanted) = self else { return false };
		let wanted = wanted.trim();
		let wanted = wanted.strip_suffix(".exe").unwrap_or(wanted).to_lowercase();
		names.into_iter().any(|name| {
			let name = name.trim();
			let name = name.strip_suffix(".exe").unwrap_or(name);
			!name.is_empty() && name.to_lowercase() == wanted
		})
	}
}

impl std::fmt::Display for AppMatch {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			AppMatch::Pid(pid) => write!(f, "process {pid}"),
			AppMatch::Name(name) => f.write_str(name),
		}
	}
}

/// An application that plays audio (or could, on Android), for a picker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioApp {
	/// Name to show.
	pub name: String,
	/// Its process (Linux, Windows); pick with [`AppMatch::Pid`].
	pub pid: Option<u32>,
	/// Executable (Linux, Windows) or package name (Android): what
	/// [`AppMatch::Name`] matches besides `name`.
	pub binary: Option<String>,
	/// Icon name (freedesktop icon theme) or icon path, if the application
	/// gave one.
	pub icon: Option<String>,
	/// What it plays, if it says (e.g. a track or tab title).
	pub media: Option<String>,
	/// Its playback streams now.
	pub streams: u32,
	/// Audio is flowing now (not paused or idle).
	pub playing: bool,
}

/// A running capture that feeds a mixer source; stops when dropped.
pub trait SourceCapture: Send {
	/// Short name for logs (`"pipewire"`, `"wasapi"`, ...).
	fn backend(&self) -> &'static str;
}

/// Capture the playback `filter` selects into `source`, on this platform.
pub fn start_playback(
	filter: &PlaybackFilter,
	source: &SourceHandle,
) -> Result<Box<dyn SourceCapture>> {
	if let Some(provider) = super::external::audio_provider() {
		return super::external::start_playback(provider, filter, source);
	}
	#[cfg(all(target_os = "linux", feature = "pipewire"))]
	return super::pipewire_links::LinkManager::shared()?.capture(filter, source);
	#[cfg(windows)]
	return super::windows::start_playback(filter, source);
	#[allow(unreachable_code)]
	{
		let _ = (filter, source);
		Err(Error::CaptureUnavailable {
			backend: "audio",
			reason: "no application audio capture in this build".into(),
		})
	}
}

/// Applications that play audio now, updated live. Holds the backend's
/// connection (e.g. to PipeWire) while it lives.
pub struct AudioApps {
	rx: watch::Receiver<Vec<AudioApp>>,
	_keep: Box<dyn Send + Sync>,
}

impl AudioApps {
	pub fn new(rx: watch::Receiver<Vec<AudioApp>>, keep: Box<dyn Send + Sync>) -> Self {
		Self { rx, _keep: keep }
	}

	/// The list now (marks it seen).
	pub fn current(&mut self) -> Vec<AudioApp> {
		self.rx.borrow_and_update().clone()
	}

	/// Whether the list changed since [`current`](Self::current).
	pub fn has_changed(&self) -> bool {
		self.rx.has_changed().unwrap_or(false)
	}

	/// Wait for a change; `None` once the backend is gone.
	pub async fn changed(&mut self) -> Option<Vec<AudioApp>> {
		self.rx.changed().await.ok()?;
		Some(self.current())
	}

	/// Wait for a change from a plain thread, up to `timeout`.
	pub fn wait_changed(&mut self, timeout: Duration) -> Option<Vec<AudioApp>> {
		let deadline = std::time::Instant::now() + timeout;
		while std::time::Instant::now() < deadline {
			if self.has_changed() {
				return Some(self.current());
			}
			std::thread::sleep(Duration::from_millis(20));
		}
		None
	}
}

/// The applications that play audio, on this platform.
pub fn audio_apps() -> Result<AudioApps> {
	if let Some(provider) = super::external::audio_provider() {
		let (tx, rx) = watch::channel(provider.apps());
		return Ok(AudioApps::new(rx, Box::new(tx)));
	}
	#[cfg(all(target_os = "linux", feature = "pipewire"))]
	{
		let manager = super::pipewire_links::LinkManager::shared()?;
		return Ok(AudioApps::new(manager.apps(), Box::new(manager)));
	}
	#[cfg(windows)]
	return super::windows::audio_apps();
	#[allow(unreachable_code)]
	Err(Error::CaptureUnavailable {
		backend: "audio",
		reason: "no application audio listing in this build".into(),
	})
}

/// The process that owns a captured window, where the platform tells (X11
/// `_NET_WM_PID`, the owner of a Windows `HWND`). The ScreenCast portal
/// does not say which application a window belongs to.
pub fn window_pid(window: u64) -> Option<u32> {
	#[cfg(all(target_os = "linux", feature = "x11"))]
	return super::x11::window_pid(None, window);
	#[cfg(windows)]
	return super::windows::window_pid(window);
	#[allow(unreachable_code)]
	{
		let _ = window;
		None
	}
}

/// Whether `pid` is `ancestor` or one of its descendants (Linux: the
/// parent chain in `/proc`; elsewhere only `pid == ancestor`).
pub fn descends_from(pid: u32, ancestor: u32) -> bool {
	if pid == ancestor {
		return true;
	}
	#[cfg(target_os = "linux")]
	{
		let mut current = pid;
		// A bound on the chain, in case of a cycle while processes are reused.
		for _ in 0..64 {
			match parent_pid(current) {
				Some(parent) if parent == ancestor => return true,
				Some(parent) if parent > 1 && parent != current => current = parent,
				_ => return false,
			}
		}
	}
	false
}

/// The parent of `pid` from `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
pub fn parent_pid(pid: u32) -> Option<u32> {
	let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
	// `pid (comm) state ppid ...`; the command may contain spaces and ')'.
	let rest = &stat[stat.rfind(')')? + 1..];
	rest.split_whitespace().nth(1)?.parse().ok()
}

/// Feeds an [`AudioCapture`] (48 kHz buffers) into a mixer source from a
/// thread of its own.
struct Forwarded {
	backend: &'static str,
	worker: Option<Worker>,
	capture: Box<dyn AudioCapture>,
}

impl SourceCapture for Forwarded {
	fn backend(&self) -> &'static str {
		self.backend
	}
}

impl Drop for Forwarded {
	fn drop(&mut self) {
		self.worker = None;
		self.capture.stop();
	}
}

/// Start `capture` and feed its buffers into `source` until the returned
/// capture is dropped, the source is removed, or the capture ends.
pub fn forward_audio(
	mut capture: Box<dyn AudioCapture>,
	source: &SourceHandle,
) -> Result<Box<dyn SourceCapture>> {
	let mut buffers = capture.start()?;
	let mut input = source.input(crate::frame::AUDIO_SAMPLE_RATE);
	let worker = Worker::spawn("voelin-audio-forward", move |stop| {
		while !stop.load(std::sync::atomic::Ordering::Relaxed) && !input.is_closed() {
			match buffers.recv_timeout(Duration::from_millis(100)) {
				Some(buffer) => {
					input.push(&buffer.samples, buffer.channels);
				}
				None if buffers.is_closed() => break,
				None => {}
			}
		}
	})?;
	Ok(Box::new(Forwarded { backend: capture.backend(), worker: Some(worker), capture }))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::capture::synthetic::SineSource;
	use crate::mix::{MixerConfig, StreamMixer};

	#[test]
	fn names_match_loosely() {
		let firefox = AppMatch::Name("Firefox".into());
		assert!(firefox.matches_name(["firefox"]));
		assert!(firefox.matches_name(["", "FIREFOX.exe"]));
		assert!(!firefox.matches_name(["firefox-bin"]));
		assert!(AppMatch::Name("spotify.exe".into()).matches_name(["Spotify"]));
		assert!(!AppMatch::Pid(1).matches_name(["1"]));
		assert_eq!(AppMatch::Pid(7).to_string(), "process 7");
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn process_tree() {
		let me = std::process::id();
		assert!(descends_from(me, me));
		let parent = parent_pid(me).unwrap();
		assert!(descends_from(me, parent));
		assert!(!descends_from(parent, me));
		let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
		assert!(descends_from(child.id(), me));
		assert_eq!(parent_pid(child.id()), Some(me));
		child.kill().unwrap();
		child.wait().unwrap();
		assert!(!descends_from(u32::MAX - 1, me));
	}

	#[test]
	fn forwarded_capture_feeds_the_mixer() {
		let mut mixer = StreamMixer::new(MixerConfig::default());
		let source = mixer.handle().add_source("tone");
		let capture = forward_audio(Box::new(SineSource::new(1000.0, 0.5)), &source).unwrap();
		assert_eq!(capture.backend(), "synthetic");
		let mut out = vec![0.0; 1920];
		let deadline = std::time::Instant::now() + Duration::from_secs(5);
		while source.level().peak < 0.4 && std::time::Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(20));
			mixer.mix(&mut out);
		}
		assert!(source.level().peak > 0.4, "{:?}", source.level());
		drop(capture);
	}
}
