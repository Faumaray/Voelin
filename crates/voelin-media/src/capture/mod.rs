//! Screen and system-audio capture.
//!
//! Backends deliver frames on a [`FrameReceiver`] from their own thread; the
//! queue drops the oldest frame when the consumer falls behind.
//!
//! | Backend | Screen | System audio |
//! |---|---|---|
//! | [`synthetic`] | test pattern | sine tone |
//! | `x11` (Linux, feature `x11`) | MIT-SHM / GetImage, XFixes cursor | |
//! | `portal` / `pipewire_audio` (Linux, feature `pipewire`) | ScreenCast portal + PipeWire | default sink monitor |
//! | `windows` | Windows Graphics Capture | WASAPI process loopback |
//! | [`external`] | platform code (Android MediaProjection) | same |

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::frame::{AudioBuffer, VideoFrame};
use crate::queue::FrameReceiver;
use crate::{Error, Result};

pub mod external;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod pipewire_audio;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod portal;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod pw;
pub mod synthetic;
#[cfg(windows)]
pub mod windows;
#[cfg(all(target_os = "linux", feature = "x11"))]
pub mod x11;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What to capture.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SourceId {
	/// A monitor, by the backend's index (see [`ScreenCapture::sources`]).
	Monitor(u32),
	/// A top-level window by native handle (X11 window id, Windows `HWND`).
	Window(u64),
	/// Whatever the user picks in the desktop portal's dialog.
	Portal,
	/// The synthetic test pattern.
	Synthetic,
}

/// A source as shown in a picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureSource {
	pub id: SourceId,
	pub name: String,
	/// Current size; 0 if unknown (portal).
	pub width: u32,
	pub height: u32,
	/// The primary monitor.
	pub primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureOptions {
	/// Maximum frame rate.
	pub fps: u32,
	/// Draw the mouse cursor into the frames.
	pub cursor: bool,
	/// Frames queued before the oldest is dropped.
	pub queue: usize,
}

impl Default for CaptureOptions {
	fn default() -> Self {
		Self { fps: 30, cursor: true, queue: 2 }
	}
}

/// A screen / window capture backend.
pub trait ScreenCapture: Send {
	/// Short name for logs (`"x11"`, `"portal"`, ...).
	fn backend(&self) -> &'static str;

	/// Monitors and windows that can be captured. The portal backend
	/// returns a single [`SourceId::Portal`] entry: its dialog picks.
	fn sources(&mut self) -> Result<Vec<CaptureSource>>;

	/// Start capturing (stopping a previous capture). Async because the
	/// portal asks the user. Frames stop when [`ScreenCapture::stop`] is
	/// called, the backend is dropped, or the receiver is dropped.
	fn start(
		&mut self,
		source: &SourceId,
		options: &CaptureOptions,
	) -> BoxFuture<'_, Result<FrameReceiver<VideoFrame>>>;

	fn stop(&mut self);
}

/// A system-audio capture backend: 48 kHz stereo `f32` buffers.
pub trait AudioCapture: Send {
	fn backend(&self) -> &'static str;

	fn start(&mut self) -> Result<FrameReceiver<AudioBuffer>>;

	fn stop(&mut self);
}

/// The screen capture backend for this session: a registered
/// [`external::ScreenProvider`] (Android), else the ScreenCast portal on
/// Wayland, X11 otherwise on Linux, Windows Graphics Capture on Windows.
pub fn default_screen_capture() -> Result<Box<dyn ScreenCapture>> {
	if let Some(provider) = external::screen_provider() {
		return Ok(Box::new(external::ExternalScreenCapture::new(provider)));
	}
	#[cfg(target_os = "linux")]
	{
		let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
			|| std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland");
		#[cfg(feature = "pipewire")]
		if wayland {
			return Ok(Box::new(portal::PortalCapture::new()));
		}
		#[cfg(feature = "x11")]
		if std::env::var_os("DISPLAY").is_some() {
			return Ok(Box::new(x11::X11Capture::new()));
		}
		let _ = wayland;
	}
	#[cfg(windows)]
	return Ok(Box::new(windows::WindowsCapture::new()));
	#[allow(unreachable_code)]
	Err(Error::CaptureUnavailable {
		backend: "screen",
		reason: "no screen capture backend for this session in this build".into(),
	})
}

/// The system-audio backend: a registered [`external::AudioProvider`]
/// (Android), else PipeWire on Linux, WASAPI loopback on Windows.
pub fn default_audio_capture() -> Result<Box<dyn AudioCapture>> {
	if let Some(provider) = external::audio_provider() {
		return Ok(Box::new(external::ExternalAudioCapture::new(provider)));
	}
	#[cfg(all(target_os = "linux", feature = "pipewire"))]
	return Ok(Box::new(pipewire_audio::PipeWireAudioCapture::new()));
	#[cfg(windows)]
	return Ok(Box::new(windows::WasapiLoopback::new()));
	#[allow(unreachable_code)]
	Err(Error::CaptureUnavailable {
		backend: "audio",
		reason: "no system audio capture backend in this build".into(),
	})
}

/// A capture thread that is stopped and joined on drop.
pub(crate) struct Worker {
	stop: Arc<AtomicBool>,
	thread: Option<JoinHandle<()>>,
}

impl Worker {
	/// Run `f` on a named thread; it should return once the flag is set.
	pub fn spawn(name: &str, f: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Result<Self> {
		let stop = Arc::new(AtomicBool::new(false));
		let flag = stop.clone();
		let thread = std::thread::Builder::new().name(name.to_owned()).spawn(move || f(flag))?;
		Ok(Self { stop, thread: Some(thread) })
	}

	pub fn stop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
		if let Some(thread) = self.thread.take() {
			thread.thread().unpark();
			let _ = thread.join();
		}
	}
}

impl Drop for Worker {
	fn drop(&mut self) {
		self.stop();
	}
}

/// Paces a capture loop at a fixed rate.
pub(crate) struct Ticker {
	interval: Duration,
	next: Instant,
}

impl Ticker {
	pub fn new(fps: u32) -> Self {
		Self { interval: Duration::from_secs(1) / fps.max(1), next: Instant::now() }
	}

	/// Sleep until the next tick. Returns `false` if `stop` got set.
	pub fn wait(&mut self, stop: &AtomicBool) -> bool {
		self.next += self.interval;
		let now = Instant::now();
		if self.next < now {
			// Fell behind: skip ticks instead of bursting.
			self.next = now;
		}
		while !stop.load(Ordering::Relaxed) {
			let now = Instant::now();
			if now >= self.next {
				return true;
			}
			// Woken early by `Worker::stop`.
			std::thread::park_timeout(self.next - now);
		}
		false
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn worker_stops_promptly() {
		let started = Instant::now();
		let mut worker = Worker::spawn("test", |stop| {
			let mut ticker = Ticker::new(1);
			while ticker.wait(&stop) {}
		})
		.unwrap();
		std::thread::sleep(Duration::from_millis(20));
		worker.stop();
		assert!(started.elapsed() < Duration::from_millis(900));
	}
}
