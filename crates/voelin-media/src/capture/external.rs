//! Capture driven by platform code outside this crate.
//!
//! On Android, screen and system-audio capture need a MediaProjection, which
//! only the app's Java/Kotlin side can obtain (it asks the user and runs a
//! foreground service). The app registers a [`ScreenProvider`] and an
//! [`AudioProvider`] at start; [`super::default_screen_capture`] and
//! [`super::default_audio_capture`] then hand out capture backends that
//! forward to them, so stream code needs no platform knowledge.
//!
//! A provider receives the [`FrameSender`] of the capture and pushes frames
//! into it from any thread; dropping the sender ends the capture for the
//! consumer (e.g. when the user stops sharing from the system UI).

use std::sync::{Arc, Mutex, PoisonError};

use crate::capture::playback::{AudioApp, PlaybackFilter, SourceCapture, forward_audio};
use crate::capture::{
	AudioCapture, BoxFuture, CaptureOptions, CaptureSource, FrameSink, ScreenCapture, SourceId,
};
use crate::frame::{AUDIO_SAMPLE_RATE, AudioBuffer, VideoFrame};
use crate::mix::{SourceHandle, SourceInput};
use crate::queue::{FrameReceiver, FrameSender, frame_channel};
use crate::{Error, Result};

/// Platform code that captures the screen.
pub trait ScreenProvider: Send + Sync {
	/// Short name for logs (`"mediaprojection"`).
	fn name(&self) -> &'static str;

	/// What can be captured (Android: the whole screen, as one
	/// [`SourceId::Portal`] entry since the system dialog picks).
	fn sources(&self) -> Vec<CaptureSource>;

	/// Start delivering frames into `frames`. Resolves once frames flow, or
	/// with an error such as [`crate::Error::Cancelled`] when the user
	/// declined.
	fn start(
		&self,
		source: &SourceId,
		options: &CaptureOptions,
		frames: FrameSender<VideoFrame>,
	) -> BoxFuture<'static, Result<()>>;

	/// Like [`start`](Self::start), but frames go to `sink` from the
	/// provider's own thread, borrowed (no copy), and the provider may hand
	/// it [`FrameSink::gpu`] pictures while it accepts them. The default
	/// forwards the frames of `start`.
	fn start_sink(
		&self,
		source: &SourceId,
		options: &CaptureOptions,
		sink: Box<dyn FrameSink>,
	) -> BoxFuture<'static, Result<()>> {
		let (tx, rx) = frame_channel(options.queue);
		let started = self.start(source, options, tx);
		Box::pin(async move {
			started.await?;
			super::forward(rx, sink)
		})
	}

	fn stop(&self);
}

/// Platform code that captures what the device plays (48 kHz `f32`).
pub trait AudioProvider: Send + Sync {
	fn name(&self) -> &'static str;

	/// Start delivering audio into `buffers`.
	fn start(&self, buffers: FrameSender<AudioBuffer>) -> Result<()>;

	fn stop(&self);

	/// Start capturing the playback `filter` selects straight into a mixer
	/// input; several captures may run at once. Returns an id for
	/// [`stop_input`](Self::stop_input). The default supports none (see
	/// [`start_playback`] for the fallback).
	fn start_input(&self, filter: &PlaybackFilter, input: SourceInput) -> Result<u64> {
		let _ = (filter, input);
		Err(Error::CaptureUnavailable {
			backend: self.name(),
			reason: "capturing single applications is not supported".into(),
		})
	}

	fn stop_input(&self, id: u64) {
		let _ = id;
	}

	/// Applications whose playback can be captured (Android: the launchable
	/// apps, since it cannot tell which play).
	fn apps(&self) -> Vec<AudioApp> {
		Vec::new()
	}
}

/// A capture of [`AudioProvider::start_input`]; stops it when dropped.
struct ProviderCapture {
	provider: Arc<dyn AudioProvider>,
	id: u64,
}

impl SourceCapture for ProviderCapture {
	fn backend(&self) -> &'static str {
		self.provider.name()
	}
}

impl Drop for ProviderCapture {
	fn drop(&mut self) {
		self.provider.stop_input(self.id);
	}
}

/// Capture through `provider` into `source`: [`AudioProvider::start_input`],
/// or for everything but our own playback, providers without it run their
/// plain [`AudioProvider::start`] into the source.
pub fn start_playback(
	provider: Arc<dyn AudioProvider>,
	filter: &PlaybackFilter,
	source: &SourceHandle,
) -> Result<Box<dyn SourceCapture>> {
	match provider.start_input(filter, source.input(AUDIO_SAMPLE_RATE)) {
		Ok(id) => Ok(Box::new(ProviderCapture { provider, id })),
		Err(_) if *filter == PlaybackFilter::AllButSelf => {
			forward_audio(Box::new(ExternalAudioCapture::new(provider)), source)
		}
		Err(e) => Err(e),
	}
}

type Slot<T> = Mutex<Option<Arc<T>>>;

static SCREEN: Slot<dyn ScreenProvider> = Mutex::new(None);
static AUDIO: Slot<dyn AudioProvider> = Mutex::new(None);

fn get<T: ?Sized>(slot: &Slot<T>) -> Option<Arc<T>> {
	slot.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Make [`super::default_screen_capture`] use `provider` (`None` restores
/// the built-in backends).
pub fn set_screen_provider(provider: Option<Arc<dyn ScreenProvider>>) {
	*SCREEN.lock().unwrap_or_else(PoisonError::into_inner) = provider;
}

/// Make [`super::default_audio_capture`] use `provider`.
pub fn set_audio_provider(provider: Option<Arc<dyn AudioProvider>>) {
	*AUDIO.lock().unwrap_or_else(PoisonError::into_inner) = provider;
}

pub fn screen_provider() -> Option<Arc<dyn ScreenProvider>> {
	get(&SCREEN)
}

pub fn audio_provider() -> Option<Arc<dyn AudioProvider>> {
	get(&AUDIO)
}

/// A [`ScreenCapture`] backed by a [`ScreenProvider`].
pub struct ExternalScreenCapture {
	provider: Arc<dyn ScreenProvider>,
	running: bool,
}

impl ExternalScreenCapture {
	pub fn new(provider: Arc<dyn ScreenProvider>) -> Self {
		Self { provider, running: false }
	}
}

impl ScreenCapture for ExternalScreenCapture {
	fn backend(&self) -> &'static str {
		self.provider.name()
	}

	fn sources(&mut self) -> Result<Vec<CaptureSource>> {
		Ok(self.provider.sources())
	}

	fn start(
		&mut self,
		source: &SourceId,
		options: &CaptureOptions,
	) -> BoxFuture<'_, Result<FrameReceiver<VideoFrame>>> {
		self.stop();
		let (tx, rx) = frame_channel(options.queue);
		let started = self.provider.start(source, options, tx);
		self.running = true;
		Box::pin(async move {
			started.await?;
			Ok(rx)
		})
	}

	fn start_sink(
		&mut self,
		source: &SourceId,
		options: &CaptureOptions,
		sink: Box<dyn FrameSink>,
	) -> BoxFuture<'_, Result<()>> {
		self.stop();
		let options = CaptureOptions { fps: sink.max_fps(), ..options.clone() };
		let started = self.provider.start_sink(source, &options, sink);
		self.running = true;
		started
	}

	fn stop(&mut self) {
		if std::mem::take(&mut self.running) {
			self.provider.stop();
		}
	}
}

impl Drop for ExternalScreenCapture {
	fn drop(&mut self) {
		self.stop();
	}
}

/// An [`AudioCapture`] backed by an [`AudioProvider`].
pub struct ExternalAudioCapture {
	provider: Arc<dyn AudioProvider>,
	running: bool,
}

impl ExternalAudioCapture {
	pub fn new(provider: Arc<dyn AudioProvider>) -> Self {
		Self { provider, running: false }
	}
}

impl AudioCapture for ExternalAudioCapture {
	fn backend(&self) -> &'static str {
		self.provider.name()
	}

	fn start(&mut self) -> Result<FrameReceiver<AudioBuffer>> {
		self.stop();
		// About a second of 10 ms buffers.
		let (tx, rx) = frame_channel(100);
		self.provider.start(tx)?;
		self.running = true;
		Ok(rx)
	}

	fn stop(&mut self) {
		if std::mem::take(&mut self.running) {
			self.provider.stop();
		}
	}
}

impl Drop for ExternalAudioCapture {
	fn drop(&mut self) {
		self.stop();
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::time::Duration;

	use super::*;

	/// Sends one frame per start, or refuses.
	#[derive(Default)]
	struct Fake {
		refuse: bool,
		stops: AtomicUsize,
		sender: Mutex<Option<FrameSender<VideoFrame>>>,
	}

	impl ScreenProvider for Fake {
		fn name(&self) -> &'static str {
			"fake"
		}

		fn sources(&self) -> Vec<CaptureSource> {
			vec![CaptureSource {
				id: SourceId::Portal,
				name: "Screen".into(),
				width: 0,
				height: 0,
				primary: true,
			}]
		}

		fn start(
			&self,
			_: &SourceId,
			_: &CaptureOptions,
			frames: FrameSender<VideoFrame>,
		) -> BoxFuture<'static, Result<()>> {
			if self.refuse {
				return Box::pin(async { Err(Error::Cancelled) });
			}
			frames.send(VideoFrame::black_i420(4, 4));
			*self.sender.lock().unwrap() = Some(frames);
			Box::pin(async { Ok(()) })
		}

		fn stop(&self) {
			self.stops.fetch_add(1, Ordering::Relaxed);
			self.sender.lock().unwrap().take();
		}
	}

	#[tokio::test]
	async fn frames_flow_until_the_provider_drops_the_sender() {
		let fake = Arc::new(Fake::default());
		let mut capture = ExternalScreenCapture::new(fake.clone());
		assert_eq!(capture.backend(), "fake");
		assert_eq!(capture.sources().unwrap().len(), 1);
		let mut rx = capture.start(&SourceId::Portal, &CaptureOptions::default()).await.unwrap();
		assert_eq!(rx.recv().await.unwrap().width, 4);
		// The system stopped the projection.
		fake.sender.lock().unwrap().take();
		assert!(rx.recv_timeout(Duration::from_millis(10)).is_none());
		assert!(rx.is_closed());
		drop(capture);
		assert_eq!(fake.stops.load(Ordering::Relaxed), 1);
	}

	#[tokio::test]
	async fn refusal_is_an_error_and_stop_is_not_repeated() {
		let fake = Arc::new(Fake { refuse: true, ..Fake::default() });
		let mut capture = ExternalScreenCapture::new(fake.clone());
		let result = capture.start(&SourceId::Portal, &CaptureOptions::default()).await;
		assert!(matches!(result, Err(Error::Cancelled)));
		capture.stop();
		capture.stop();
		assert_eq!(fake.stops.load(Ordering::Relaxed), 1);
	}

	#[test]
	fn registered_provider_is_the_default() {
		struct Silent;
		impl AudioProvider for Silent {
			fn name(&self) -> &'static str {
				"silent"
			}
			fn start(&self, buffers: FrameSender<AudioBuffer>) -> Result<()> {
				buffers.send(AudioBuffer {
					samples: vec![0.0; 960],
					channels: 2,
					timestamp: Duration::ZERO,
				});
				Ok(())
			}
			fn stop(&self) {}
		}
		set_audio_provider(Some(Arc::new(Silent)));
		let mut capture = crate::capture::default_audio_capture().unwrap();
		assert_eq!(capture.backend(), "silent");
		let mut rx = capture.start().unwrap();
		assert_eq!(rx.try_recv().unwrap().frames(), 480);
		set_audio_provider(None);
		assert!(audio_provider().is_none());
	}
}
