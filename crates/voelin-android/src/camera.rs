//! Cameras for the Stream Studio through Camera2.
//!
//! Registered as the studio's [`CameraProvider`]: the studio's camera
//! picker lists the device's cameras (front ones first; a back camera is
//! not mirrored by default), and a camera source opens one.
//! `CameraCapture.kt` asks for the CAMERA permission if needed, opens the
//! camera into an `ImageReader` (YUV_420_888) and hands each frame's planes
//! to [`on_frame`] as direct buffers valid during the call, with the turn
//! that makes it upright for the screen's rotation. The studio's sink
//! converts straight from the camera's memory when the picture is upright
//! and planar or NV12; otherwise it is gathered (and turned) into a buffer
//! each camera keeps, so a running camera allocates nothing per frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tracing::warn;
use voelin_media::capture::FrameSink;
use voelin_media::studio::camera::{Camera, CameraProvider, Format, Pixel, YuvPlanes};
use voelin_media::studio::compose::Feed;
use voelin_media::{Error, Result};

use crate::bridge;

const BACKEND: &str = "camera2";

struct Running {
	sink: Box<dyn FrameSink>,
	feed: Arc<Feed>,
	/// Clock origin of the frames' timestamps (`Image.timestamp`).
	origin_ns: Option<i64>,
	/// Gathered or turned pictures (I420), reused.
	scratch: Vec<u8>,
}

static RUNNING: Mutex<Option<HashMap<u64, Running>>> = Mutex::new(None);

fn running() -> MutexGuard<'static, Option<HashMap<u64, Running>>> {
	RUNNING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Make Camera2 the studio's cameras.
pub fn register() {
	voelin_media::studio::camera::set_provider(Some(Arc::new(Camera2)));
}

struct Camera2;

/// Stops its camera when dropped.
struct Guard(u64);

impl Drop for Guard {
	fn drop(&mut self) {
		running().as_mut().map(|r| r.remove(&self.0));
		if let Err(e) = bridge::stop_camera(self.0) {
			warn!("stopping camera {}: {e}", self.0);
		}
	}
}

impl CameraProvider for Camera2 {
	fn name(&self) -> &'static str {
		BACKEND
	}

	fn list(&self) -> Vec<Camera> {
		match bridge::cameras() {
			Ok(lines) => parse(&lines),
			Err(e) => {
				warn!("listing cameras: {e}");
				Vec::new()
			}
		}
	}

	fn start(
		&self,
		device: &str,
		size: Option<(u32, u32)>,
		fps: u32,
		sink: Box<dyn FrameSink>,
		feed: Arc<Feed>,
	) -> Result<Box<dyn Send>> {
		static NEXT: AtomicU64 = AtomicU64::new(1);
		let id = NEXT.fetch_add(1, Ordering::Relaxed);
		let entry = Running { sink, feed, origin_ns: None, scratch: Vec::new() };
		running().get_or_insert_with(HashMap::new).insert(id, entry);
		let (width, height) = size.unwrap_or((0, 0));
		match bridge::start_camera(id, device, width, height, fps) {
			Ok(true) => Ok(Box::new(Guard(id))),
			started => {
				running().as_mut().map(|r| r.remove(&id));
				let reason = match started {
					Err(e) => e.to_string(),
					_ => format!("camera {device:?} cannot be opened"),
				};
				Err(Error::CaptureUnavailable { backend: BACKEND, reason })
			}
		}
	}
}

/// `id<TAB>facing<TAB>sizes<TAB>maxFps` lines (`CameraCapture.list`).
fn parse(lines: &str) -> Vec<Camera> {
	let mut seen: HashMap<String, usize> = HashMap::new();
	lines
		.lines()
		.filter_map(|line| {
			let mut parts = line.split('\t');
			let id = parts.next()?.trim();
			let facing = parts.next()?.trim();
			let sizes = parts
				.next()
				.unwrap_or_default()
				.split(',')
				.filter_map(|s| {
					let (w, h) = s.trim().split_once('x')?;
					Some((w.parse().ok()?, h.parse().ok()?))
				})
				.collect::<Vec<(u32, u32)>>();
			let max_fps = parts.next().and_then(|f| f.trim().parse().ok()).unwrap_or(0);
			if id.is_empty() {
				return None;
			}
			let label = match facing {
				"front" => "Front camera",
				"back" => "Back camera",
				_ => "External camera",
			};
			let n = seen.entry(label.to_owned()).or_insert(0);
			*n += 1;
			let name = if *n == 1 { label.to_owned() } else { format!("{label} {n}") };
			Some(Camera {
				id: id.to_owned(),
				name,
				backend: BACKEND,
				formats: vec![Format { pixel: Pixel::I420, sizes, max_fps }],
				// What faces the user is mirrored, as a mirror would be.
				mirrored: facing != "back",
			})
		})
		.collect()
}

/// A frame of camera `id`. Returns `false` once it is no longer wanted.
pub fn on_frame(id: u64, planes: &YuvPlanes<'_>, timestamp_ns: i64) -> bool {
	let mut guard = running();
	let Some(run) = guard.as_mut().and_then(|r| r.get_mut(&id)) else { return false };
	let Running { sink, feed, origin_ns, scratch } = run;
	if feed.is_closed() {
		return false;
	}
	let origin = *origin_ns.get_or_insert(timestamp_ns);
	let timestamp = Duration::from_nanos(u64::try_from(timestamp_ns - origin).unwrap_or(0));
	if !sink.wants(timestamp) {
		return true;
	}
	match planes.frame(timestamp, scratch) {
		Ok(frame) => sink.frame(frame),
		Err(e) => {
			warn!("dropping a camera frame: {e}");
			true
		}
	}
}

/// Camera `id` failed (or could not open): the studio shows why.
pub fn on_error(id: u64, message: String) {
	if let Some(run) = running().as_ref().and_then(|r| r.get(&id)) {
		run.feed.set_error(Error::Capture { backend: BACKEND, message });
	}
}
