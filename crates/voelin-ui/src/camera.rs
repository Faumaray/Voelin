//! The camera preview of the Voice & Video settings (the Stream Studio's
//! camera capture, with its background blur), and the speakers' test tone.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use tokio::sync::oneshot;
use voelin_core::media::voelin_media::convert;
use voelin_core::studio::camera::{self, Capture};
use voelin_core::studio::compose::Feed;
use voelin_core::studio::scene::Background;

use crate::app::with_app;

/// The cameras as (id, name): the devices, then the test pattern.
pub(crate) fn list() -> Vec<(String, String)> {
	camera::list().into_iter().map(|c| (c.id, c.name)).collect()
}

/// A running preview; the camera stops when it is dropped.
pub(crate) struct Preview {
	feed: Arc<Feed>,
	status: Arc<Mutex<String>>,
	mirror: bool,
	_stop: oneshot::Sender<()>,
	_timer: slint::Timer,
}

impl Preview {
	/// Open `device` (empty: the first camera) at `size` if it has it.
	pub fn start(
		runtime: &tokio::runtime::Handle,
		device: &str,
		size: Option<(u32, u32)>,
		blur: bool,
		mirror: bool,
	) -> Self {
		let feed = Arc::new(Feed::new());
		let status = Arc::new(Mutex::new(String::new()));
		let (stop, stopped) = oneshot::channel::<()>();
		let background = if blur { Background::Blur { strength: 0.04 } } else { Background::Keep };
		let (device, task_feed, task_status) = (device.to_owned(), feed.clone(), status.clone());
		runtime.spawn(async move {
			match Capture::start(&device, size, 30, task_feed, background).await {
				Ok(capture) => {
					// Runs until the preview is dropped.
					let _ = stopped.await;
					drop(capture);
				}
				Err(e) => {
					*task_status.lock().unwrap_or_else(PoisonError::into_inner) =
						format!("The camera does not start: {e}");
				}
			}
		});
		let timer = slint::Timer::default();
		timer.start(slint::TimerMode::Repeated, Duration::from_millis(66), || {
			with_app(|app| app.camera_frame());
		});
		Self { feed, status, mirror, _stop: stop, _timer: timer }
	}

	/// The newest picture since the last call.
	pub fn take_picture(&self) -> Option<Image> {
		let frame = self.feed.take()?;
		let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(frame.width, frame.height);
		let stride = frame.width as usize * 4;
		convert::to_rgba(&frame, buffer.make_mut_bytes(), stride).ok()?;
		if self.mirror {
			for row in buffer.make_mut_slice().chunks_mut(frame.width as usize) {
				row.reverse();
			}
		}
		Some(Image::from_rgba8(buffer))
	}

	/// What to show while there is no picture.
	pub fn status(&self) -> String {
		let status = self.status.lock().unwrap_or_else(PoisonError::into_inner).clone();
		if !status.is_empty() {
			return status;
		}
		match self.feed.error() {
			Some(e) => e,
			None if self.feed.delivered() == 0 => "Starting the camera…".into(),
			None => String::new(),
		}
	}
}

/// Play a short two-tone chime on `device` (the system default for
/// `None`) at `volume` (linear). Blocks while it plays.
pub(crate) fn play_tone(device: Option<&str>, volume: f32) -> Result<(), String> {
	let mut out = voelin_audio::device::Playback::open(device, 300).map_err(|e| e.to_string())?;
	let rate = out.rate as f32;
	let channels = out.channels.max(1);
	let samples: Vec<f32> = (0..(rate * 0.7) as usize)
		.flat_map(|i| {
			let t = i as f32 / rate;
			let freq = if t < 0.35 { 660.0 } else { 880.0 };
			// Fade in and out of each note.
			let local = t % 0.35;
			let envelope = (local / 0.02).min(1.0) * ((0.35 - local) / 0.05).min(1.0);
			let v = (t * freq * std::f32::consts::TAU).sin() * 0.3 * envelope * volume.min(2.0);
			std::iter::repeat_n(v, channels)
		})
		.collect();
	let mut written = 0;
	while written < samples.len() {
		written += out.write(&samples[written..]);
		std::thread::sleep(Duration::from_millis(20));
	}
	while out.queued() > 0 {
		std::thread::sleep(Duration::from_millis(20));
	}
	Ok(())
}
