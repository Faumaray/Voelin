//! Screen and system-audio capture.
//!
//! Screen backends hand each frame to a [`FrameSink`] on their own thread,
//! with the pixels still in the capture buffer (a PipeWire buffer, the X11
//! shared-memory segment, a Windows staging texture), so the sink converts
//! straight from it ([`ScreenCapture::start_sink`]). [`ScreenCapture::start`]
//! instead copies frames into a [`FrameReceiver`]; the queue drops the oldest
//! frame when the consumer falls behind. Audio backends use a
//! [`FrameReceiver`].
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

use crate::frame::{AudioBuffer, FrameRef, VideoFrame};
use crate::queue::{FrameReceiver, FrameSender, frame_channel};
use crate::{Error, Result};

#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub(crate) mod dmabuf;
pub mod external;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod pipewire_audio;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod pipewire_links;
pub mod playback;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod portal;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub(crate) mod pw;
pub mod synthetic;
#[cfg(windows)]
pub mod windows;
#[cfg(all(target_os = "linux", feature = "wlroots"))]
pub mod wlroots;
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
	/// The Stream Studio's composite ([`crate::studio::StudioCapture`]).
	Studio,
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
	/// Maximum frame rate for [`ScreenCapture::start`] (with
	/// [`ScreenCapture::start_sink`], [`FrameSink::max_fps`] decides).
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

/// Receives captured frames on the capture backend's thread; see
/// [`ScreenCapture::start_sink`].
pub trait FrameSink: Send {
	/// The highest frame rate wanted now. Backends that pace themselves
	/// (X11, the test pattern) capture at this rate; it may change while
	/// capturing.
	fn max_fps(&self) -> u32;

	/// Whether a frame captured at `timestamp` would be used. Backends that
	/// get frames pushed (PipeWire, Windows, wlroots) ask before they map or
	/// copy anything, and give unwanted buffers straight back.
	fn wants(&mut self, timestamp: Duration) -> bool {
		let _ = timestamp;
		true
	}

	/// One frame. Its pixels are only valid during the call. Returns
	/// `false` when no more frames are wanted; the backend then stops.
	fn frame(&mut self, frame: FrameRef<'_>) -> bool;

	/// Whether the sink takes frames that are still in a DMA-BUF (GPU
	/// memory) through [`dmabuf`](Self::dmabuf), e.g. for a VA-API encoder
	/// that imports them without a copy
	/// (`ffmpeg::FfmpegEncoder::encode_dmabuf`). Backends that capture into
	/// DMA-BUFs (the portal) then offer the buffer before mapping it.
	fn accepts_dmabuf(&self) -> bool {
		false
	}

	/// A frame in a DMA-BUF, valid during the call. `None`: not taken, the
	/// backend maps it and calls [`frame`](Self::frame); `Some(more)` as the
	/// result of `frame`.
	fn dmabuf(&mut self, frame: &DmaBufRef) -> Option<bool> {
		let _ = frame;
		None
	}

	/// Tiled DRM format modifiers of RGB buffers the sink takes now
	/// ([`accepts_dmabuf`](Self::accepts_dmabuf)), best first; backends that
	/// negotiate buffers (the portal) offer them ahead of LINEAR, and offer
	/// again when this changes. The CPU cannot read tiled buffers, so a sink
	/// lists only what it imports.
	fn dmabuf_modifiers(&self) -> &[u64] {
		&[]
	}
}

/// `fourcc_code(a, b, c, d)` of `drm_fourcc.h`.
pub const fn drm_fourcc(code: &[u8; 4]) -> u32 {
	code[0] as u32 | (code[1] as u32) << 8 | (code[2] as u32) << 16 | (code[3] as u32) << 24
}

/// `DRM_FORMAT_MOD_LINEAR`.
pub const DRM_MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`: the driver's implicit layout.
pub const DRM_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// A captured frame still in a DMA-BUF: one buffer object (a file
/// descriptor borrowed for the call) with one plane per `planes` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaBufRef {
	pub width: u32,
	pub height: u32,
	/// As [`VideoFrame::timestamp`].
	pub timestamp: Duration,
	/// DRM format (`drm_fourcc(b"XR24")` for BGRx, `b"NV12"`, ...).
	pub fourcc: u32,
	/// DRM format modifier (tiling); [`DRM_MOD_LINEAR`] for rows in memory
	/// order.
	pub modifier: u64,
	pub fd: i32,
	/// Size of the buffer object in bytes.
	pub size: usize,
	/// `(offset, stride)` of each plane in the buffer object.
	pub planes: [(usize, usize); 4],
	pub plane_count: usize,
}

/// Keeps frames of a source for a frame-rate cap, by their timestamps, so
/// the kept frames are evenly spaced at the cap (or at the source's rate if
/// that is lower).
#[derive(Clone, Debug)]
pub struct FramePacer {
	interval: Duration,
	next: Option<Duration>,
}

impl FramePacer {
	/// At most `fps` frames per second; `None` (or 0) keeps every frame.
	pub fn new(fps: Option<u32>) -> Self {
		let mut pacer = Self { interval: Duration::ZERO, next: None };
		pacer.set_fps(fps);
		pacer
	}

	pub fn set_fps(&mut self, fps: Option<u32>) {
		self.interval = match fps {
			Some(fps) if fps > 0 => Duration::from_secs(1) / fps,
			_ => Duration::ZERO,
		};
	}

	/// Whether a frame at `timestamp` is due. A frame up to an eighth of
	/// the interval early still counts, so jitter does not halve the rate;
	/// a timestamp far before the expected one means the clock started
	/// over.
	pub fn due(&self, timestamp: Duration) -> bool {
		self.next.is_none_or(|next| {
			timestamp + self.interval / 8 >= next || timestamp + 2 * self.interval <= next
		})
	}

	/// Record that the frame at `timestamp` was kept.
	pub fn keep(&mut self, timestamp: Duration) {
		let iv = self.interval;
		self.next = Some(match self.next {
			// On time: keep the average rate exact.
			Some(next) if timestamp < next + iv && timestamp + 2 * iv > next => next + iv,
			// Late (or the clock jumped): start over from this frame.
			_ => timestamp + iv,
		});
	}

	/// [`due`](Self::due), and [`keep`](Self::keep) if so.
	pub fn take(&mut self, timestamp: Duration) -> bool {
		let due = self.due(timestamp);
		if due {
			self.keep(timestamp);
		}
		due
	}
}

/// Copies frames into a queue: [`ScreenCapture::start`] of backends that
/// deliver into a [`FrameSink`].
pub(crate) struct QueueSink {
	tx: FrameSender<VideoFrame>,
	fps: u32,
	pacer: FramePacer,
}

impl QueueSink {
	pub fn new(options: &CaptureOptions) -> (Self, FrameReceiver<VideoFrame>) {
		let (tx, rx) = frame_channel(options.queue);
		let fps = options.fps.max(1);
		(Self { tx, fps, pacer: FramePacer::new(Some(fps)) }, rx)
	}
}

impl FrameSink for QueueSink {
	fn max_fps(&self) -> u32 {
		self.fps
	}

	fn wants(&mut self, timestamp: Duration) -> bool {
		!self.tx.is_closed() && self.pacer.due(timestamp)
	}

	fn frame(&mut self, frame: FrameRef<'_>) -> bool {
		self.pacer.keep(frame.timestamp);
		self.tx.send(frame.to_frame())
	}
}

/// Hand frames of `frames` to `sink` on a thread of their own, until either
/// side stops ([`ScreenCapture::start_sink`] of backends that only have
/// [`ScreenCapture::start`]).
pub fn forward(mut frames: FrameReceiver<VideoFrame>, mut sink: Box<dyn FrameSink>) -> Result<()> {
	std::thread::Builder::new().name("voelin-capture-forward".into()).spawn(move || {
		loop {
			match frames.recv_timeout(Duration::from_millis(100)) {
				Some(frame) => {
					if sink.wants(frame.timestamp) && !sink.frame(frame.view()) {
						break;
					}
				}
				None if frames.is_closed() => break,
				None => {}
			}
		}
	})?;
	Ok(())
}

/// A screen / window capture backend.
pub trait ScreenCapture: Send {
	/// Short name for logs (`"x11"`, `"portal"`, ...).
	fn backend(&self) -> &'static str;

	/// Monitors and windows that can be captured. The portal backend
	/// returns a single [`SourceId::Portal`] entry: its dialog picks.
	fn sources(&mut self) -> Result<Vec<CaptureSource>>;

	/// Start capturing (stopping a previous capture), copying every frame
	/// into a queue. Async because the portal asks the user. Frames stop
	/// when [`ScreenCapture::stop`] is called, the backend is dropped, or the
	/// receiver is dropped.
	fn start(
		&mut self,
		source: &SourceId,
		options: &CaptureOptions,
	) -> BoxFuture<'_, Result<FrameReceiver<VideoFrame>>>;

	/// Start capturing into `sink` (stopping a previous capture): the
	/// backend calls it on its own thread with the pixels still in the
	/// capture buffer, and drops it when the capture ends (stopped, the
	/// source went away, or the sink returned `false`). `options.fps` is
	/// ignored in favour of [`FrameSink::max_fps`].
	///
	/// The default forwards the frames of [`ScreenCapture::start`] from a
	/// thread of its own.
	fn start_sink(
		&mut self,
		source: &SourceId,
		options: &CaptureOptions,
		sink: Box<dyn FrameSink>,
	) -> BoxFuture<'_, Result<()>> {
		let source = source.clone();
		let options = CaptureOptions { fps: sink.max_fps(), ..options.clone() };
		Box::pin(async move {
			let frames = self.start(&source, &options).await?;
			forward(frames, sink)
		})
	}

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

	/// Change the rate from the next tick on.
	pub fn set_fps(&mut self, fps: u32) {
		self.interval = Duration::from_secs(1) / fps.max(1);
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

	fn kept(pacer: &mut FramePacer, source_fps: f64, jitter_ms: f64, seconds: u32) -> usize {
		let frames = (source_fps * f64::from(seconds)) as usize;
		(0..frames)
			.filter(|&n| {
				let jitter = if n % 2 == 0 { jitter_ms } else { -jitter_ms };
				let t = (n as f64 * 1000.0 / source_fps + jitter).max(0.0);
				pacer.take(Duration::from_secs_f64(t / 1000.0))
			})
			.count()
	}

	#[test]
	fn pacer_caps_the_rate() {
		// 60 Hz source, 30 fps cap: every other frame, even with jitter.
		assert_eq!(kept(&mut FramePacer::new(Some(30)), 60.0, 1.5, 10), 300);
		// 144 Hz to 60: close to 60.
		let n = kept(&mut FramePacer::new(Some(60)), 144.0, 0.5, 10);
		assert!((580..=610).contains(&n), "{n}");
		// A cap above the source rate keeps everything.
		assert_eq!(kept(&mut FramePacer::new(Some(120)), 30.0, 2.0, 10), 300);
		assert_eq!(kept(&mut FramePacer::new(None), 60.0, 0.0, 1), 60);
		// A clock that jumps back starts over.
		let mut pacer = FramePacer::new(Some(10));
		assert!(pacer.take(Duration::from_secs(100)));
		assert!(!pacer.take(Duration::from_millis(100_050)));
		assert!(pacer.take(Duration::from_secs(1)));
	}
}
