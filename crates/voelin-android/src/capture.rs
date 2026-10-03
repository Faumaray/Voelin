//! Screen and system-audio capture through MediaProjection.
//!
//! Registered as `voelin_media` capture providers, so stream code captures with
//! `voelin_media::capture::default_screen_capture()` as on desktop. Starting
//! asks the Kotlin side for the user's consent; on consent it runs
//! `ScreenCaptureService` (a `mediaProjection` foreground service) whose
//! virtual display renders into an `ImageReader`, whose RGBA frames come to
//! [`on_frame`]. A stream's sink takes them borrowed, converting straight
//! from the reader's buffer.
//!
//! The zero-copy path: while every encoder the sink feeds takes pictures
//! straight from the screen ([`FrameSink::accepts_gpu`]: one MediaCodec
//! encoder, see `voelin_media::codec::mediacodec`), a ticker at the
//! stream's frame rate tells the sink a picture is due; the first one makes
//! the encoder publish its input surface, and the virtual display is
//! pointed at it, so the screen goes from the compositor into the encoder
//! without a copy. When that stops (a second simulcast layer or codec, the
//! encoder gone or failed) the display goes back to the reader.
//!
//! System audio (`AudioPlaybackCapture`, 48 kHz float) needs that running
//! projection: everything but our own uid, or one app's uid, each into a
//! stream mixer input ([`on_input`]); several captures may run at once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tracing::{debug, warn};
use voelin_media::capture::external::{self, AudioProvider, ScreenProvider};
use voelin_media::capture::playback::{AppMatch, AudioApp, PlaybackFilter};
use voelin_media::capture::{BoxFuture, CaptureOptions, CaptureSource, FrameSink, SourceId};
use voelin_media::codec::mediacodec::{self, InputSurface};
use voelin_media::mix::SourceInput;
use voelin_media::{
	AudioBuffer, Error, FrameRef, FrameSender, GpuFrame, PixelsRef, PlaneRef, Result, VideoFrame,
};

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
	/// Mixer inputs of [`AudioProvider::start_input`] captures, by id.
	inputs: HashMap<u64, SourceInput>,
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

/// The screen's consumer of [`ScreenProvider::start_sink`], apart from the
/// audio state so that converting a frame does not hold up audio.
#[derive(Default)]
struct Video {
	sink: Option<Box<dyn FrameSink>>,
	/// The ticker that serves this sink.
	ticker: u64,
	/// Size of the captured screen (the reader's), once a frame came.
	size: (u32, u32),
	/// The last frame's time and when it came: the ticker's pictures
	/// continue that clock.
	last: Option<(Duration, Instant)>,
	/// The encoder surface the display renders into (its generation).
	rendering: Option<u64>,
}

static VIDEO: Mutex<Option<Video>> = Mutex::new(None);

fn with_video<R>(f: impl FnOnce(&mut Video) -> R) -> R {
	let mut guard = VIDEO.lock().unwrap_or_else(PoisonError::into_inner);
	f(guard.get_or_insert_with(Video::default))
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

impl Projection {
	/// Ask for consent; `Ok` once the capture runs.
	fn request(options: &CaptureOptions, tx: oneshot::Sender<Result<()>>) -> Result<()> {
		with_state(|s| {
			if let Some(previous) = s.pending.replace(tx) {
				let _ = previous.send(Err(Error::Cancelled));
			}
			s.origin_ns = None;
		});
		bridge::request_screen_capture(options.fps.clamp(1, 60), MAX_SIZE).map_err(|e| {
			with_state(|s| s.pending = None);
			Error::CaptureUnavailable { backend: SCREEN, reason: e.to_string() }
		})
	}
}

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
		with_video(|v| v.sink = None);
		with_state(|s| s.frames = Some(frames));
		if let Err(e) = Self::request(options, tx) {
			with_state(|s| s.frames = None);
			return Box::pin(async move { Err(e) });
		}
		Box::pin(async move { rx.await.unwrap_or(Err(Error::Cancelled)) })
	}

	fn start_sink(
		&self,
		_source: &SourceId,
		options: &CaptureOptions,
		sink: Box<dyn FrameSink>,
	) -> BoxFuture<'static, Result<()>> {
		static TICKERS: AtomicU64 = AtomicU64::new(1);
		let (tx, rx) = oneshot::channel();
		with_state(|s| s.frames = None);
		let ticker = TICKERS.fetch_add(1, Ordering::Relaxed);
		with_video(|v| *v = Video { sink: Some(sink), ticker, ..Video::default() });
		if let Err(e) = Self::request(options, tx) {
			with_video(|v| v.sink = None);
			return Box::pin(async move { Err(e) });
		}
		if let Err(e) = std::thread::Builder::new()
			.name("voelin-screen-ticker".into())
			.spawn(move || run_ticker(ticker))
		{
			warn!("no zero-copy screen path: {e}");
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

/// The zero-copy path's ticker (see the [module docs](self)): runs until
/// its sink is gone or replaced.
fn run_ticker(id: u64) {
	loop {
		let mut redirect: Option<Option<InputSurface>> = None;
		let interval = with_video(|v| {
			if v.ticker != id {
				return None;
			}
			let Some(sink) = v.sink.as_mut() else {
				// Back to the reader, whose next frame stops the service.
				if v.rendering.take().is_some() {
					redirect = Some(None);
				}
				return None;
			};
			let interval = Duration::from_secs(1) / sink.max_fps().max(1);
			let (width, height) = v.size;
			if width == 0 {
				// No frame yet.
				return Some(interval);
			}
			if !sink.accepts_gpu() {
				if v.rendering.take().is_some() {
					debug!("screen back to frames in memory");
					redirect = Some(None);
				}
				return Some(interval);
			}
			// The encoder makes its surface for the first picture.
			match (mediacodec::input_surface(), v.rendering) {
				(Some(surface), rendering) if rendering != Some(surface.generation) => {
					debug!(surface.width, surface.height, "screen straight into the encoder");
					v.rendering = Some(surface.generation);
					redirect = Some(Some(surface));
				}
				(None, Some(_)) => {
					v.rendering = None;
					redirect = Some(None);
				}
				_ => {}
			}
			let timestamp = v.last.map_or(Duration::ZERO, |(at, when)| at + when.elapsed());
			if sink.wants(timestamp) && !sink.gpu(GpuFrame::rendered(width, height, timestamp)) {
				v.sink = None;
			}
			Some(interval)
		});
		if let Some(target) = redirect
			&& let Err(e) = bridge::set_screen_surface(target.as_ref())
		{
			warn!("pointing the screen at the encoder: {e}");
		}
		let Some(interval) = interval else { return };
		std::thread::sleep(interval);
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

	/// Everything but our uid, or the uid of one app's package.
	fn start_input(&self, filter: &PlaybackFilter, input: SourceInput) -> Result<u64> {
		let package = match filter {
			PlaybackFilter::AllButSelf => None,
			PlaybackFilter::App(app @ AppMatch::Name(name)) => Some(
				self.apps()
					.into_iter()
					.find(|a| {
						app.matches_name([a.name.as_str(), a.binary.as_deref().unwrap_or("")])
					})
					.and_then(|a| a.binary)
					// Not launchable (a service, say): maybe the package itself.
					.unwrap_or_else(|| name.trim().to_owned()),
			),
			PlaybackFilter::App(AppMatch::Pid(_)) => {
				return Err(Error::CaptureUnavailable {
					backend: AUDIO,
					reason: "Android captures apps by package, not by process".into(),
				});
			}
		};
		static NEXT: AtomicU64 = AtomicU64::new(1);
		let id = NEXT.fetch_add(1, Ordering::Relaxed);
		with_state(|s| s.inputs.insert(id, input));
		let started = bridge::start_audio_input(id, package.as_deref());
		if let Ok(true) = started {
			return Ok(id);
		}
		with_state(|s| s.inputs.remove(&id));
		let reason = match (started, package) {
			(Err(e), _) => e.to_string(),
			(_, Some(package)) => format!(
				"cannot capture {package}: not installed, or no running screen capture (and \
				 the microphone permission)"
			),
			(_, None) => "needs a running screen capture (and the microphone permission)".into(),
		};
		Err(Error::CaptureUnavailable { backend: AUDIO, reason })
	}

	fn stop_input(&self, id: u64) {
		with_state(|s| s.inputs.remove(&id));
		if let Err(e) = bridge::stop_audio_input(id) {
			warn!("stopping audio capture {id}: {e}");
		}
	}

	/// Android cannot tell which apps play: the launchable ones.
	fn apps(&self) -> Vec<AudioApp> {
		match bridge::launchable_apps() {
			Ok(apps) => apps
				.into_iter()
				.map(|(name, package, icon)| AudioApp {
					name,
					binary: Some(package),
					icon: (!icon.is_empty()).then_some(icon),
					..AudioApp::default()
				})
				.collect(),
			Err(e) => {
				warn!("listing apps: {e}");
				Vec::new()
			}
		}
	}
}

pub fn wants_input(id: u64) -> bool {
	with_state(|s| s.inputs.get(&id).is_some_and(|i| !i.is_closed()))
}

/// Captured playback for mixer input `id`. Returns `false` once the input
/// is gone (the capture then stops).
pub fn on_input(id: u64, samples: &[f32], channels: u16) -> bool {
	with_state(|s| {
		let Some(input) = s.inputs.get_mut(&id) else { return false };
		input.push(samples, channels);
		if input.is_closed() {
			s.inputs.remove(&id);
			return false;
		}
		true
	})
}

/// The user answered the consent dialog (or the service failed to start).
pub fn on_result(granted: bool, error: Option<String>) {
	if !granted {
		with_video(|v| v.sink = None);
	}
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
	with_video(|v| v.sink.is_some())
		|| with_state(|s| s.frames.as_ref().is_some_and(|f| !f.is_closed()))
}

/// A captured RGBA frame, valid during the call. Returns `false` once
/// nobody wants frames.
pub fn on_frame(pixels: &[u8], width: u32, height: u32, stride: usize, timestamp_ns: i64) -> bool {
	let timestamp = with_state(|s| {
		let timestamp = since(&mut s.origin_ns, timestamp_ns);
		if let Some(origin) = s.origin_ns {
			mediacodec::set_screen_origin(origin);
		}
		timestamp
	});
	let taken = with_video(|v| {
		let sink = v.sink.as_mut()?;
		v.size = (width, height);
		v.last = Some((timestamp, Instant::now()));
		// While the screen renders into the encoder, what still reaches
		// the reader is not used.
		if sink.accepts_gpu() || !sink.wants(timestamp) {
			return Some(true);
		}
		let pixels = PixelsRef::Rgba(PlaneRef::new(pixels, stride));
		let frame = FrameRef { width, height, timestamp, pixels };
		if let Err(e) = frame.validate() {
			warn!("dropping a screen frame: {e}");
			return Some(true);
		}
		if !sink.frame(frame) {
			v.sink = None;
			return Some(false);
		}
		Some(true)
	});
	if let Some(more) = taken {
		return more;
	}
	with_state(|s| {
		let Some(frames) = &s.frames else {
			return false;
		};
		match VideoFrame::from_rgba(width, height, stride, pixels.to_vec()) {
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
	with_video(|v| v.sink = None);
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
