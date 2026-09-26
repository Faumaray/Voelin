//! Screen and system-audio capture through MediaProjection.
//!
//! Registered as `tsc_media` capture providers, so stream code captures with
//! `tsc_media::capture::default_screen_capture()` as on desktop. Starting
//! asks the Kotlin side for the user's consent; on consent it runs
//! `ScreenCaptureService` (a `mediaProjection` foreground service) whose
//! `ImageReader` hands RGBA frames to [`on_frame`]. System audio
//! (`AudioPlaybackCapture`, 48 kHz float) needs that running projection.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::oneshot;
use tracing::warn;
use tsc_media::capture::external::{self, AudioProvider, ScreenProvider};
use tsc_media::capture::{BoxFuture, CaptureOptions, CaptureSource, SourceId};
use tsc_media::{AudioBuffer, Error, FrameSender, Result, VideoFrame};

use crate::bridge;

const SCREEN: &str = "mediaprojection";
const AUDIO: &str = "playbackcapture";
/// Longest side of captured frames (the service scales the display down).
const MAX_SIZE: u32 = 1920;

#[derive(Default)]
struct State {
	frames: Option<FrameSender<VideoFrame>>,
	/// Answer for the pending `start`.
	pending: Option<oneshot::Sender<Result<()>>>,
	audio: Option<FrameSender<AudioBuffer>>,
	/// Clock origin of frame and audio timestamps (both `CLOCK_MONOTONIC`
	/// nanoseconds: `Image.timestamp`, `System.nanoTime`), so they stay in sync.
	origin_ns: Option<i64>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
	let mut guard: MutexGuard<'_, Option<State>> =
		STATE.lock().unwrap_or_else(PoisonError::into_inner);
	f(guard.get_or_insert_with(State::default))
}

/// Make MediaProjection the default screen and audio capture.
pub fn register() {
	external::set_screen_provider(Some(Arc::new(Projection)));
	external::set_audio_provider(Some(Arc::new(PlaybackAudio)));
}

fn since(first: &mut Option<i64>, now_ns: i64) -> Duration {
	let first = *first.get_or_insert(now_ns);
	Duration::from_nanos(u64::try_from(now_ns - first).unwrap_or(0))
}

struct Projection;

impl ScreenProvider for Projection {
	fn name(&self) -> &'static str {
		SCREEN
	}

	fn sources(&self) -> Vec<CaptureSource> {
		vec![CaptureSource {
			// The system dialog picks (whole screen, or one app on Android 14+).
			id: SourceId::Portal,
			name: "Screen".into(),
			width: 0,
			height: 0,
			primary: true,
		}]
	}

	fn start(
		&self,
		_source: &SourceId,
		options: &CaptureOptions,
		frames: FrameSender<VideoFrame>,
	) -> BoxFuture<'static, Result<()>> {
		let (tx, rx) = oneshot::channel();
		with_state(|s| {
			if let Some(previous) = s.pending.replace(tx) {
				let _ = previous.send(Err(Error::Cancelled));
			}
			s.frames = Some(frames);
			s.origin_ns = None;
		});
		if let Err(e) = bridge::request_screen_capture(options.fps.clamp(1, 60), MAX_SIZE) {
			with_state(|s| {
				s.pending = None;
				s.frames = None;
			});
			let error = Error::CaptureUnavailable { backend: SCREEN, reason: e.to_string() };
			return Box::pin(async move { Err(error) });
		}
		Box::pin(async move { rx.await.unwrap_or(Err(Error::Cancelled)) })
	}

	fn stop(&self) {
		on_stopped();
		if let Err(e) = bridge::stop_screen_capture() {
			warn!("stopping screen capture: {e}");
		}
	}
}

struct PlaybackAudio;

impl AudioProvider for PlaybackAudio {
	fn name(&self) -> &'static str {
		AUDIO
	}

	fn start(&self, buffers: FrameSender<AudioBuffer>) -> Result<()> {
		with_state(|s| {
			s.audio = Some(buffers);
		});
		let started = bridge::start_system_audio();
		match started {
			Ok(true) => Ok(()),
			Ok(false) | Err(_) => {
				with_state(|s| s.audio = None);
				let reason = match started {
					Err(e) => e.to_string(),
					_ => "needs a running screen capture (and the microphone permission)".into(),
				};
				Err(Error::CaptureUnavailable { backend: AUDIO, reason })
			}
		}
	}

	fn stop(&self) {
		with_state(|s| s.audio = None);
		if let Err(e) = bridge::stop_system_audio() {
			warn!("stopping system audio: {e}");
		}
	}
}

/// The user answered the consent dialog (or the service failed to start).
pub fn on_result(granted: bool, error: Option<String>) {
	with_state(|s| {
		let result = match (granted, error) {
			(true, _) => Ok(()),
			(false, None) => Err(Error::Cancelled),
			(false, Some(message)) => Err(Error::Capture { backend: SCREEN, message }),
		};
		if result.is_err() {
			s.frames = None;
		}
		if let Some(pending) = s.pending.take() {
			let _ = pending.send(result);
		}
	});
}

pub fn wants_frames() -> bool {
	with_state(|s| s.frames.as_ref().is_some_and(|f| !f.is_closed()))
}

/// A captured RGBA frame. Returns `false` once nobody wants frames.
pub fn on_frame(
	pixels: Vec<u8>,
	width: u32,
	height: u32,
	stride: usize,
	timestamp_ns: i64,
) -> bool {
	with_state(|s| {
		let timestamp = since(&mut s.origin_ns, timestamp_ns);
		let Some(frames) = &s.frames else {
			return false;
		};
		match VideoFrame::from_rgba(width, height, stride, pixels) {
			Ok(frame) => {
				if !frames.send(frame.with_timestamp(timestamp)) {
					s.frames = None;
					return false;
				}
			}
			Err(e) => warn!("dropping a screen frame: {e}"),
		}
		true
	})
}

/// The projection ended (stopped by us, the user, or the system): the
/// consumers see their channels close.
pub fn on_stopped() {
	with_state(|s| {
		s.frames = None;
		s.audio = None;
		if let Some(pending) = s.pending.take() {
			let _ = pending.send(Err(Error::Cancelled));
		}
	});
}

pub fn wants_audio() -> bool {
	with_state(|s| s.audio.as_ref().is_some_and(|a| !a.is_closed()))
}

/// Captured playback audio. Returns `false` once nobody wants it.
pub fn on_audio(samples: Vec<f32>, channels: u16, timestamp_ns: i64) -> bool {
	with_state(|s| {
		let timestamp = since(&mut s.origin_ns, timestamp_ns);
		let Some(audio) = &s.audio else {
			return false;
		};
		if !audio.send(AudioBuffer { samples, channels, timestamp }) {
			s.audio = None;
			return false;
		}
		true
	})
}
