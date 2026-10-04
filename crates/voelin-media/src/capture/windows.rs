//! Windows capture: Windows Graphics Capture (`windows-capture`) for monitors
//! and windows, WASAPI loopback (`wasapi`) for system audio.
//!
//! System audio uses process loopback in exclude mode with our own process
//! id, so our own playback (TeamSpeak voices) is not captured (Windows 10
//! 2004 or later). Older systems fall back to plain loopback of the default
//! output device, which includes our playback. Single applications use
//! process loopback in include mode on their process tree, one capture per
//! process ([`start_playback`]); [`audio_apps`] lists the processes with
//! audio sessions on the playback devices.
//!
//! Type-checked on Linux (`--target x86_64-pc-windows-gnu`), not run yet.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
	ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
	GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::capture::playback::{AppMatch, AudioApp, AudioApps, PlaybackFilter, SourceCapture};
use crate::capture::{
	AudioCapture, BoxFuture, CaptureOptions, CaptureSource, FrameSink, QueueSink, ScreenCapture,
	SourceId, Ticker, Worker,
};
use crate::frame::{AUDIO_SAMPLE_RATE, AudioBuffer, FrameRef, PixelsRef, PlaneRef, VideoFrame};
use crate::mix::SourceHandle;
use crate::queue::{FrameReceiver, frame_channel};
use crate::{Error, Result};

const BACKEND: &str = "wgc";

fn failed(e: impl std::fmt::Display) -> Error {
	Error::Capture { backend: BACKEND, message: e.to_string() }
}

/// Receives frames on the capture thread and hands the mapped staging
/// texture (with its row pitch) to the sink.
struct Handler {
	sink: Box<dyn FrameSink>,
	started: Instant,
}

impl GraphicsCaptureApiHandler for Handler {
	type Flags = (Box<dyn FrameSink>, Instant);
	type Error = String;

	fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
		let (sink, started) = ctx.flags;
		Ok(Self { sink, started })
	}

	fn on_frame_arrived(
		&mut self,
		frame: &mut Frame,
		control: InternalCaptureControl,
	) -> std::result::Result<(), Self::Error> {
		let timestamp = self.started.elapsed();
		// Frames come as the screen changes, up to its refresh rate; the
		// sink's cap decides before anything is copied off the GPU.
		if !self.sink.wants(timestamp) {
			return Ok(());
		}
		let mut buffer = frame.buffer().map_err(|e| e.to_string())?;
		let (width, height) = (buffer.width(), buffer.height());
		let pitch = buffer.row_pitch() as usize;
		let plane = PlaneRef::new(buffer.as_raw_buffer(), pitch);
		let frame = FrameRef { width, height, timestamp, pixels: PixelsRef::Bgra(plane) };
		if !self.sink.frame(frame) {
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
	sink: Box<dyn FrameSink>,
) -> Result<CaptureControl<Handler, String>>
where
	T: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
	let cursor = if options.cursor {
		CursorCaptureSettings::WithCursor
	} else {
		CursorCaptureSettings::WithoutCursor
	};
	// Left alone, Windows delivers about 60 frames a second (reported for
	// a default and for anything under 1 ms: robmikh/Win32CaptureSample#82).
	// 1 ms lets the sink's frame-rate cap decide, which can change while
	// capturing. Windows 10 has no such setting.
	let interval = match GraphicsCaptureApi::is_minimum_update_interval_supported() {
		Ok(true) => MinimumUpdateIntervalSettings::Custom(Duration::from_millis(1)),
		_ => MinimumUpdateIntervalSettings::Default,
	};
	let settings = Settings::new(
		item,
		cursor,
		DrawBorderSettings::WithoutBorder,
		SecondaryWindowSettings::Default,
		interval,
		DirtyRegionSettings::Default,
		ColorFormat::Bgra8,
		(sink, Instant::now()),
	);
	Handler::start_free_threaded(settings).map_err(failed)
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
		let (sink, rx) = QueueSink::new(options);
		let started = self.start_sink(source, options, Box::new(sink));
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
		let source = source.clone();
		let options = options.clone();
		Box::pin(async move {
			self.stop();
			let not_found = || Error::SourceNotFound(source.clone());
			let control = match source {
				SourceId::Monitor(index) => {
					let monitor = Monitor::from_index(index as usize).map_err(|_| not_found())?;
					start_item(monitor, &options, sink)?
				}
				SourceId::Window(hwnd) => {
					let window = Window::from_raw_hwnd(hwnd as usize as *mut std::ffi::c_void);
					if !window.is_valid() {
						return Err(not_found());
					}
					start_item(window, &options, sink)?
				}
				_ => return Err(not_found()),
			};
			self.control = Some(control);
			Ok(())
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
		AUDIO_BACKEND
	}

	fn start(&mut self) -> Result<FrameReceiver<AudioBuffer>> {
		self.stop();
		let (tx, rx) = frame_channel(50);
		let started = Instant::now();
		let worker =
			spawn_loopback("voelin-wasapi-loopback", Loopback::AllButSelf, move |samples| {
				let samples = samples.to_vec();
				tx.send(AudioBuffer { samples, channels: 2, timestamp: started.elapsed() })
			})?;
		self.worker = Some(worker);
		Ok(rx)
	}

	fn stop(&mut self) {
		self.worker = None;
	}
}

const AUDIO_BACKEND: &str = "wasapi";
/// Bytes of one 48 kHz stereo `f32` frame.
const FRAME_BYTES: usize = 2 * 4;

/// Which processes a loopback capture hears.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Loopback {
	/// Everything except our process tree (plain device loopback where
	/// process loopback is missing, which includes our playback).
	AllButSelf,
	/// One process and its children.
	Tree(u32),
}

/// Start a loopback capture on a thread of its own that hands each packet
/// (48 kHz stereo) to `deliver` until it returns `false` or the worker is
/// dropped. Returns once the capture runs (or failed to start).
fn spawn_loopback(
	name: &str,
	mode: Loopback,
	mut deliver: impl FnMut(&[f32]) -> bool + Send + 'static,
) -> Result<Worker> {
	let (init_tx, init_rx) = std::sync::mpsc::channel();
	let mut worker = Worker::spawn(name, move |stop| {
		if let Err(e) = loopback(mode, &stop, &init_tx, &mut deliver) {
			// After a successful start the consumer just sees the end.
			let _ = init_tx.send(Err(e.clone()));
			warn!("audio capture ({mode:?}) ended: {e}");
		}
	})?;
	match init_rx.recv() {
		Ok(Ok(())) => Ok(worker),
		Ok(Err(reason)) => {
			worker.stop();
			Err(Error::CaptureUnavailable { backend: AUDIO_BACKEND, reason })
		}
		Err(_) => {
			worker.stop();
			Err(Error::CaptureUnavailable {
				backend: AUDIO_BACKEND,
				reason: "capture thread ended".into(),
			})
		}
	}
}

fn open_client(mode: Loopback) -> std::result::Result<wasapi::AudioClient, String> {
	let format = wasapi::WaveFormat::new(
		32,
		32,
		&wasapi::SampleType::Float,
		AUDIO_SAMPLE_RATE as usize,
		2,
		None,
	);
	let stream =
		wasapi::StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 200_000 };
	let (pid, include) = match mode {
		// Everything but our own process tree.
		Loopback::AllButSelf => (std::process::id(), false),
		Loopback::Tree(pid) => (pid, true),
	};
	let opened = wasapi::AudioClient::new_application_loopback_client(pid, include).and_then(
		|mut client| {
			client.initialize_client(&format, &wasapi::Direction::Capture, &stream)?;
			Ok(client)
		},
	);
	match opened {
		Ok(client) => return Ok(client),
		// No fallback that would capture other applications too.
		Err(e) if include => return Err(format!("process loopback of {pid}: {e}")),
		Err(e) => warn!("process loopback unavailable ({e}), capturing all system audio"),
	}
	let enumerator = wasapi::DeviceEnumerator::new().map_err(|e| e.to_string())?;
	let device =
		enumerator.get_default_device(&wasapi::Direction::Render).map_err(|e| e.to_string())?;
	let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
	// Capture on a render device is loopback.
	client
		.initialize_client(&format, &wasapi::Direction::Capture, &stream)
		.map_err(|e| e.to_string())?;
	Ok(client)
}

/// The capture loop: packets as they come, converted into one reused
/// buffer (nothing allocated per packet).
fn loopback(
	mode: Loopback,
	stop: &AtomicBool,
	init: &std::sync::mpsc::Sender<std::result::Result<(), String>>,
	deliver: &mut dyn FnMut(&[f32]) -> bool,
) -> std::result::Result<(), String> {
	// COM for this thread; "already initialised" is fine.
	let _ = wasapi::initialize_mta();
	let client = open_client(mode)?;
	let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
	let capture = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
	client.start_stream().map_err(|e| e.to_string())?;
	let _ = init.send(Ok(()));

	// A tenth of a second per packet; grown if a packet is larger.
	let mut bytes = vec![0u8; AUDIO_SAMPLE_RATE as usize / 10 * FRAME_BYTES];
	let mut samples: Vec<f32> = Vec::with_capacity(bytes.len() / 4);
	while !stop.load(Ordering::Relaxed) {
		// No events arrive while nothing plays.
		if event.wait_for_event(100).is_err() {
			continue;
		}
		loop {
			let (frames, info) = match capture.read_from_device(&mut bytes) {
				Ok(read) => read,
				Err(wasapi::WasapiError::DataLengthTooShort { expected, .. }) => {
					// That packet is lost; the next one fits.
					bytes.resize(expected * FRAME_BYTES, 0);
					samples.reserve(expected * 2);
					continue;
				}
				Err(e) => return Err(e.to_string()),
			};
			if frames == 0 {
				break;
			}
			let len = frames as usize * FRAME_BYTES;
			samples.clear();
			if info.flags.silent {
				samples.resize(len / 4, 0.0);
			} else {
				samples.extend(
					bytes[..len]
						.chunks_exact(4)
						.map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
				);
			}
			if !deliver(&samples) {
				stop.store(true, Ordering::Relaxed);
				break;
			}
		}
	}
	client.stop_stream().map_err(|e| e.to_string())
}

/// Loopback captures feeding a mixer source.
struct LoopbackCapture {
	_worker: Worker,
}

impl SourceCapture for LoopbackCapture {
	fn backend(&self) -> &'static str {
		AUDIO_BACKEND
	}
}

fn capture_into(mode: Loopback, source: &SourceHandle) -> Result<Worker> {
	let mut input = source.input(AUDIO_SAMPLE_RATE);
	spawn_loopback("voelin-wasapi-source", mode, move |samples| {
		input.push(samples, 2);
		!input.is_closed()
	})
}

/// Capture the playback `filter` selects into `source`: process loopback
/// excluding our process tree, or including the chosen process tree (for a
/// name, one capture per matching process, matched again every two
/// seconds).
pub fn start_playback(
	filter: &PlaybackFilter,
	source: &SourceHandle,
) -> Result<Box<dyn SourceCapture>> {
	let worker = match filter {
		PlaybackFilter::AllButSelf => capture_into(Loopback::AllButSelf, source)?,
		PlaybackFilter::App(AppMatch::Pid(pid)) => capture_into(Loopback::Tree(*pid), source)?,
		PlaybackFilter::App(app) => {
			let (app, source) = (app.clone(), source.clone());
			Worker::spawn("voelin-wasapi-apps", move |stop| follow_app(&app, &source, &stop))?
		}
	};
	Ok(Box::new(LoopbackCapture { _worker: worker }))
}

/// Keep one include-mode capture per process whose audio sessions match
/// `app`, until stopped or the source is removed.
fn follow_app(app: &AppMatch, source: &SourceHandle, stop: &AtomicBool) {
	let _ = wasapi::initialize_mta();
	// Per process: its capture, and the polls it was missing from.
	let mut captures: Vec<(u32, Worker, u32)> = Vec::new();
	let mut ticker = Ticker::new(1);
	loop {
		let pids: Vec<u32> = match sessions() {
			Ok(apps) => apps
				.into_iter()
				.filter(|a| app.matches_name([a.name.as_str(), a.binary.as_deref().unwrap_or("")]))
				.filter_map(|a| a.pid)
				.collect(),
			Err(e) => {
				debug!("listing audio sessions: {e}");
				Vec::new()
			}
		};
		for entry in &mut captures {
			entry.2 = if pids.contains(&entry.0) { 0 } else { entry.2 + 1 };
		}
		// Gone for two polls: the process ended or closed its sessions.
		captures.retain(|(_, _, missing)| *missing < 2);
		for pid in pids {
			if captures.iter().any(|(p, ..)| *p == pid) {
				continue;
			}
			match capture_into(Loopback::Tree(pid), source) {
				Ok(worker) => captures.push((pid, worker, 0)),
				Err(e) => warn!("capturing process {pid} ({app}): {e}"),
			}
		}
		if source.is_removed() || !ticker.wait(stop) || !ticker.wait(stop) {
			break;
		}
	}
}

/// The executable file name of `pid`, if we may look.
fn process_name(pid: u32) -> Option<String> {
	use windows::Win32::System::Threading::{
		OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
		QueryFullProcessImageNameW,
	};
	let mut path = [0u16; 1024];
	let mut len = path.len() as u32;
	// SAFETY: the handle is checked by `OpenProcess` and closed by `Owned`;
	// the pointer and length describe `path`, which outlives the call.
	#[allow(unsafe_code)]
	unsafe {
		let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
		let process = windows::core::Owned::new(process);
		QueryFullProcessImageNameW(
			*process,
			PROCESS_NAME_WIN32,
			windows::core::PWSTR(path.as_mut_ptr()),
			&mut len,
		)
		.ok()?;
	}
	let path = String::from_utf16_lossy(&path[..(len as usize).min(path.len())]);
	path.rsplit(['\\', '/']).next().map(str::to_owned).filter(|n| !n.is_empty())
}

/// Audio sessions of every active playback device, one entry per process
/// (not ours).
fn sessions() -> std::result::Result<Vec<AudioApp>, String> {
	let own = std::process::id();
	let enumerator = wasapi::DeviceEnumerator::new().map_err(|e| e.to_string())?;
	let devices =
		enumerator.get_device_collection(&wasapi::Direction::Render).map_err(|e| e.to_string())?;
	let mut apps: Vec<AudioApp> = Vec::new();
	for device in &devices {
		let Ok(device) = device else { continue };
		let Ok(manager) = device.get_iaudiosessionmanager() else { continue };
		let Ok(sessions) = manager.get_audiosessionenumerator() else { continue };
		for i in 0..sessions.get_count().unwrap_or(0) {
			let Ok(session) = sessions.get_session(i) else { continue };
			// Process 0 is the system sounds session.
			let Ok(pid) = session.get_process_id() else { continue };
			if pid == 0 || pid == own {
				continue;
			}
			let state = session.get_state().ok();
			if matches!(state, Some(wasapi::SessionState::Expired)) {
				continue;
			}
			let playing = matches!(state, Some(wasapi::SessionState::Active));
			if let Some(app) = apps.iter_mut().find(|a| a.pid == Some(pid)) {
				app.streams += 1;
				app.playing |= playing;
				continue;
			}
			let binary = process_name(pid);
			// Names like "@%SystemRoot%\..." are resource references.
			let display = session
				.get_display_name()
				.ok()
				.filter(|n| !n.trim().is_empty() && !n.starts_with('@'));
			let stem = binary.as_deref().map(|b| b.strip_suffix(".exe").unwrap_or(b).to_owned());
			apps.push(AudioApp {
				name: display.or(stem).unwrap_or_else(|| format!("Process {pid}")),
				pid: Some(pid),
				binary,
				icon: session.get_icon_path().ok().filter(|p| !p.trim().is_empty()),
				media: None,
				streams: 1,
				playing,
			});
		}
	}
	apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.pid.cmp(&b.pid)));
	Ok(apps)
}

/// Applications with audio sessions, polled every two seconds.
pub fn audio_apps() -> Result<AudioApps> {
	let _ = wasapi::initialize_mta();
	let first = sessions()
		.map_err(|reason| Error::CaptureUnavailable { backend: AUDIO_BACKEND, reason })?;
	let (tx, rx) = tokio::sync::watch::channel(first);
	let worker = Worker::spawn("voelin-wasapi-sessions", move |stop| {
		let _ = wasapi::initialize_mta();
		let mut ticker = Ticker::new(1);
		while ticker.wait(&stop) && ticker.wait(&stop) && !tx.is_closed() {
			if let Ok(apps) = sessions() {
				tx.send_if_modified(|current| {
					let changed = *current != apps;
					if changed {
						*current = apps;
					}
					changed
				});
			}
		}
	})?;
	Ok(AudioApps::new(rx, Box::new(worker)))
}

/// The process that owns window `hwnd`.
pub fn window_pid(hwnd: u64) -> Option<u32> {
	let window = Window::from_raw_hwnd(hwnd as usize as *mut std::ffi::c_void);
	if !window.is_valid() {
		return None;
	}
	window.process_id().ok()
}
