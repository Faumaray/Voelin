//! Windows capture: Windows Graphics Capture (`windows-capture`) for monitors
//! and windows, WASAPI loopback (`wasapi`) for system audio.
//!
//! System audio uses process loopback in exclude mode with our own process
//! id, so our own playback (TeamSpeak voices) is not captured (Windows 10
//! 2004 or later). Older systems fall back to plain loopback of the default
//! output device, which includes our playback.
//!
//! Type-checked on Linux (`--target x86_64-pc-windows-gnu`), not run yet.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tracing::warn;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
	ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
	GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::capture::{
	AudioCapture, BoxFuture, CaptureOptions, CaptureSource, ScreenCapture, SourceId, Worker,
};
use crate::frame::{AUDIO_SAMPLE_RATE, AudioBuffer, VideoFrame};
use crate::queue::{FrameReceiver, FrameSender, frame_channel};
use crate::{Error, Result};

const BACKEND: &str = "wgc";

fn failed(e: impl std::fmt::Display) -> Error {
	Error::Capture { backend: BACKEND, message: e.to_string() }
}

/// Receives frames on the capture thread.
struct Handler {
	tx: FrameSender<VideoFrame>,
	started: Instant,
	scratch: Vec<u8>,
}

impl GraphicsCaptureApiHandler for Handler {
	type Flags = (FrameSender<VideoFrame>, Instant);
	type Error = String;

	fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
		let (tx, started) = ctx.flags;
		Ok(Self { tx, started, scratch: Vec::new() })
	}

	fn on_frame_arrived(
		&mut self,
		frame: &mut Frame,
		control: InternalCaptureControl,
	) -> std::result::Result<(), Self::Error> {
		let buffer = frame.buffer().map_err(|e| e.to_string())?;
		let (w, h) = (buffer.width(), buffer.height());
		let pixels = buffer.as_nopadding_buffer(&mut self.scratch).to_vec();
		let frame =
			VideoFrame::from_bgra(w, h, w as usize * 4, pixels).map_err(|e| e.to_string())?;
		if !self.tx.send(frame.with_timestamp(self.started.elapsed())) {
			control.stop();
		}
		Ok(())
	}
}

/// Windows Graphics Capture backend.
#[derive(Default)]
pub struct WindowsCapture {
	control: Option<CaptureControl<Handler, String>>,
}

impl WindowsCapture {
	pub fn new() -> Self {
		Self::default()
	}
}

fn start_item<T>(
	item: T,
	options: &CaptureOptions,
) -> Result<(CaptureControl<Handler, String>, FrameReceiver<VideoFrame>)>
where
	T: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
	let (tx, rx) = frame_channel(options.queue);
	let cursor = if options.cursor {
		CursorCaptureSettings::WithCursor
	} else {
		CursorCaptureSettings::WithoutCursor
	};
	let settings = Settings::new(
		item,
		cursor,
		DrawBorderSettings::WithoutBorder,
		SecondaryWindowSettings::Default,
		MinimumUpdateIntervalSettings::Custom(Duration::from_secs(1) / options.fps.max(1)),
		DirtyRegionSettings::Default,
		ColorFormat::Bgra8,
		(tx, Instant::now()),
	);
	let control = Handler::start_free_threaded(settings).map_err(failed)?;
	Ok((control, rx))
}

impl ScreenCapture for WindowsCapture {
	fn backend(&self) -> &'static str {
		BACKEND
	}

	fn sources(&mut self) -> Result<Vec<CaptureSource>> {
		let primary = Monitor::primary().ok();
		let mut sources = Vec::new();
		for monitor in Monitor::enumerate().map_err(failed)? {
			let Ok(index) = monitor.index() else { continue };
			sources.push(CaptureSource {
				id: SourceId::Monitor(index as u32),
				name: monitor.name().or_else(|_| monitor.device_name()).unwrap_or_default(),
				width: monitor.width().unwrap_or(0),
				height: monitor.height().unwrap_or(0),
				primary: primary == Some(monitor),
			});
		}
		for window in Window::enumerate().map_err(failed)? {
			let Ok(title) = window.title() else { continue };
			if title.trim().is_empty() {
				continue;
			}
			sources.push(CaptureSource {
				id: SourceId::Window(window.as_raw_hwnd() as usize as u64),
				name: title,
				width: window.width().unwrap_or(0).max(0) as u32,
				height: window.height().unwrap_or(0).max(0) as u32,
				primary: false,
			});
		}
		Ok(sources)
	}

	fn start(
		&mut self,
		source: &SourceId,
		options: &CaptureOptions,
	) -> BoxFuture<'_, Result<FrameReceiver<VideoFrame>>> {
		let source = source.clone();
		let options = options.clone();
		Box::pin(async move {
			self.stop();
			let not_found = || Error::SourceNotFound(source.clone());
			let (control, rx) = match source {
				SourceId::Monitor(index) => {
					let monitor = Monitor::from_index(index as usize).map_err(|_| not_found())?;
					start_item(monitor, &options)?
				}
				SourceId::Window(hwnd) => {
					let window = Window::from_raw_hwnd(hwnd as usize as *mut std::ffi::c_void);
					if !window.is_valid() {
						return Err(not_found());
					}
					start_item(window, &options)?
				}
				_ => return Err(not_found()),
			};
			self.control = Some(control);
			Ok(rx)
		})
	}

	fn stop(&mut self) {
		if let Some(control) = self.control.take()
			&& let Err(e) = control.stop()
		{
			warn!("stopping Windows Graphics Capture: {e}");
		}
	}
}

impl Drop for WindowsCapture {
	fn drop(&mut self) {
		self.stop();
	}
}

/// WASAPI loopback of everything except this process (see the module docs).
#[derive(Default)]
pub struct WasapiLoopback {
	worker: Option<Worker>,
}

impl WasapiLoopback {
	pub fn new() -> Self {
		Self::default()
	}
}

impl AudioCapture for WasapiLoopback {
	fn backend(&self) -> &'static str {
		"wasapi"
	}

	fn start(&mut self) -> Result<FrameReceiver<AudioBuffer>> {
		self.stop();
		let (tx, rx) = frame_channel(50);
		let (init_tx, init_rx) = std::sync::mpsc::channel();
		self.worker = Some(Worker::spawn("tsc-wasapi-loopback", move |stop| {
			if let Err(e) = loopback(&tx, &stop, &init_tx) {
				// After a successful start the receiver just sees the end.
				let _ = init_tx.send(Err(e.clone()));
				warn!("system audio capture ended: {e}");
			}
		})?);
		match init_rx.recv() {
			Ok(Ok(())) => Ok(rx),
			Ok(Err(reason)) => {
				self.stop();
				Err(Error::CaptureUnavailable { backend: "wasapi", reason })
			}
			Err(_) => {
				self.stop();
				Err(Error::CaptureUnavailable {
					backend: "wasapi",
					reason: "capture thread ended".into(),
				})
			}
		}
	}

	fn stop(&mut self) {
		self.worker = None;
	}
}

fn open_client() -> std::result::Result<wasapi::AudioClient, String> {
	let format = wasapi::WaveFormat::new(
		32,
		32,
		&wasapi::SampleType::Float,
		AUDIO_SAMPLE_RATE as usize,
		2,
		None,
	);
	let mode = wasapi::StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 200_000 };
	// Everything but our own process tree.
	match wasapi::AudioClient::new_application_loopback_client(std::process::id(), false) {
		Ok(mut client) => {
			match client.initialize_client(&format, &wasapi::Direction::Capture, &mode) {
				Ok(()) => return Ok(client),
				Err(e) => warn!("process loopback unavailable ({e}), capturing all system audio"),
			}
		}
		Err(e) => warn!("process loopback unavailable ({e}), capturing all system audio"),
	}
	let enumerator = wasapi::DeviceEnumerator::new().map_err(|e| e.to_string())?;
	let device =
		enumerator.get_default_device(&wasapi::Direction::Render).map_err(|e| e.to_string())?;
	let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
	// Capture on a render device is loopback.
	client
		.initialize_client(&format, &wasapi::Direction::Capture, &mode)
		.map_err(|e| e.to_string())?;
	Ok(client)
}

fn loopback(
	tx: &FrameSender<AudioBuffer>,
	stop: &std::sync::atomic::AtomicBool,
	init: &std::sync::mpsc::Sender<std::result::Result<(), String>>,
) -> std::result::Result<(), String> {
	// COM for this thread; "already initialised" is fine.
	let _ = wasapi::initialize_mta();
	let client = open_client()?;
	let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
	let capture = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
	client.start_stream().map_err(|e| e.to_string())?;
	let _ = init.send(Ok(()));

	const FRAME_BYTES: usize = 2 * 4;
	let chunk = AUDIO_SAMPLE_RATE as usize / 100 * FRAME_BYTES;
	let started = Instant::now();
	let mut queue = VecDeque::new();
	while !stop.load(Ordering::Relaxed) {
		// No events arrive while nothing plays.
		if event.wait_for_event(100).is_err() {
			continue;
		}
		capture.read_from_device_to_deque(&mut queue).map_err(|e| e.to_string())?;
		while queue.len() >= chunk {
			let bytes: Vec<u8> = queue.drain(..chunk).collect();
			let samples = bytes
				.chunks_exact(4)
				.map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
				.collect();
			let buffer = AudioBuffer { samples, channels: 2, timestamp: started.elapsed() };
			if !tx.send(buffer) {
				stop.store(true, Ordering::Relaxed);
				break;
			}
		}
	}
	client.stop_stream().map_err(|e| e.to_string())
}
