//! Screen capture on wlroots compositors (Sway, Hyprland, river, labwc, ...)
//! straight through Wayland, without the portal: `ext-image-copy-capture-v1`
//! with `ext-output-image-capture-source-v1` where the compositor has it,
//! else `wlr-screencopy-unstable-v1`.
//!
//! Frames are copied by the compositor into shared-memory buffers we
//! allocate once (a memfd per buffer size) and handed to the
//! [`FrameSink`] straight from that mapping. Both protocols wait for the
//! screen to change before they complete a frame (`copy_with_damage`), so a
//! still screen costs nothing; the frame rate is capped by
//! [`FrameSink::max_fps`]. Monitors only (window capture needs
//! `ext-foreign-toplevel-list`, not done). DMA-BUF capture is not done:
//! frames are always in shared memory.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use memmap2::MmapMut;
use tracing::{debug, warn};
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::image_capture_source::v1::client::{
	ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
	ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1,
	ext_image_copy_capture_session_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
	zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::capture::{
	BoxFuture, CaptureOptions, CaptureSource, FrameSink, QueueSink, ScreenCapture, SourceId, Worker,
};
use crate::frame::{FrameRef, PixelsRef, PlaneRef, VideoFrame};
use crate::queue::FrameReceiver;
use crate::{Error, Result};

const BACKEND: &str = "wlroots";
/// How long the compositor may take to describe buffers.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// How often waits check whether to stop.
const POLL: Duration = Duration::from_millis(100);
/// Failed frames in a row before the capture gives up.
const MAX_FAILURES: u32 = 20;

fn unavailable(e: impl std::fmt::Display) -> Error {
	Error::CaptureUnavailable { backend: BACKEND, reason: e.to_string() }
}

fn failed(e: impl std::fmt::Display) -> Error {
	Error::Capture { backend: BACKEND, message: e.to_string() }
}

/// A monitor as the compositor describes it.
#[derive(Clone, Debug, Default)]
struct Output {
	name: Option<String>,
	description: Option<String>,
	mode: (i32, i32),
}

/// What the compositor said about the frame being captured.
#[derive(Debug, Default)]
struct Frame {
	/// Offered shared-memory formats we can read.
	formats: Vec<wl_shm::Format>,
	size: (u32, u32),
	/// wlr-screencopy tells the stride; ext-image-copy-capture leaves it to
	/// us.
	stride: u32,
	/// The buffer description is complete.
	described: bool,
	y_invert: bool,
	ready: bool,
	failed: Option<String>,
	/// The capture session ended (ext): the output went away.
	stopped: bool,
}

#[derive(Default)]
struct State {
	outputs: Vec<(wl_output::WlOutput, Output)>,
	frame: Frame,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
	fn event(
		_: &mut Self,
		_: &wl_registry::WlRegistry,
		_: wl_registry::Event,
		_: &GlobalListContents,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<wl_output::WlOutput, usize> for State {
	fn event(
		state: &mut Self,
		_: &wl_output::WlOutput,
		event: wl_output::Event,
		index: &usize,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		let Some((_, output)) = state.outputs.get_mut(*index) else { return };
		match event {
			wl_output::Event::Mode { flags: WEnum::Value(flags), width, height, .. }
				if flags.contains(wl_output::Mode::Current) =>
			{
				output.mode = (width, height);
			}
			wl_output::Event::Name { name } => output.name = Some(name),
			wl_output::Event::Description { description } => {
				output.description = Some(description);
			}
			_ => {}
		}
	}
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ext_image_capture_source_v1::ExtImageCaptureSourceV1);

/// Shared-memory formats we read, best first, and whether they are BGRA in
/// memory (else RGBA).
const FORMATS: [(wl_shm::Format, bool); 4] = [
	(wl_shm::Format::Xrgb8888, true),
	(wl_shm::Format::Argb8888, true),
	(wl_shm::Format::Xbgr8888, false),
	(wl_shm::Format::Abgr8888, false),
];

fn supported(format: WEnum<wl_shm::Format>) -> Option<wl_shm::Format> {
	match format {
		WEnum::Value(f) if FORMATS.iter().any(|(s, _)| *s == f) => Some(f),
		_ => None,
	}
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for State {
	fn event(
		state: &mut Self,
		frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
		event: zwlr_screencopy_frame_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		use zwlr_screencopy_frame_v1::Event;
		let f = &mut state.frame;
		match event {
			Event::Buffer { format, width, height, stride } => {
				if let Some(format) = supported(format) {
					f.formats.push(format);
					f.size = (width, height);
					f.stride = stride;
				}
				// Before version 3 there is exactly one buffer event.
				if frame.version() < 3 {
					f.described = true;
				}
			}
			Event::BufferDone => f.described = true,
			Event::Flags { flags } => {
				f.y_invert = matches!(flags, WEnum::Value(v) if v.contains(zwlr_screencopy_frame_v1::Flags::YInvert));
			}
			Event::Ready { .. } => f.ready = true,
			Event::Failed => f.failed = Some("the compositor could not copy the frame".into()),
			_ => {}
		}
	}
}

impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, ()> for State {
	fn event(
		state: &mut Self,
		_: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
		event: ext_image_copy_capture_session_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		use ext_image_copy_capture_session_v1::Event;
		let f = &mut state.frame;
		match event {
			Event::BufferSize { width, height } => {
				// A new description starts (e.g. the output was resized).
				f.described = false;
				f.formats.clear();
				f.size = (width, height);
				f.stride = width * 4;
			}
			Event::ShmFormat { format } => {
				if let Some(format) = supported(format) {
					f.formats.push(format);
				}
			}
			Event::Done => f.described = true,
			Event::Stopped => f.stopped = true,
			_ => {}
		}
	}
}

impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, ()> for State {
	fn event(
		state: &mut Self,
		_: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
		event: ext_image_copy_capture_frame_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		use ext_image_copy_capture_frame_v1::{Event, FailureReason};
		let f = &mut state.frame;
		match event {
			Event::Ready => f.ready = true,
			Event::Failed { reason } => {
				f.failed = Some(match reason {
					WEnum::Value(FailureReason::BufferConstraints) => "buffer constraints".into(),
					WEnum::Value(FailureReason::Stopped) => {
						f.stopped = true;
						"the session stopped".into()
					}
					_ => "the compositor could not copy the frame".into(),
				});
			}
			_ => {}
		}
	}
}

/// A shared-memory buffer the compositor copies frames into.
struct ShmBuffer {
	map: MmapMut,
	pool: wl_shm_pool::WlShmPool,
	buffer: wl_buffer::WlBuffer,
	key: (wl_shm::Format, u32, u32, u32),
}

impl ShmBuffer {
	fn new(
		shm: &wl_shm::WlShm,
		qh: &QueueHandle<State>,
		format: wl_shm::Format,
		(width, height): (u32, u32),
		stride: u32,
	) -> Result<Self> {
		let size = stride as usize * height as usize;
		let fd =
			rustix::fs::memfd_create("voelin-wlroots-capture", rustix::fs::MemfdFlags::CLOEXEC)
				.map_err(|e| failed(format!("memfd_create: {e}")))?;
		let file = std::fs::File::from(fd);
		file.set_len(size as u64)?;
		let map = map_shared(&file)?;
		let len = i32::try_from(size).map_err(|_| failed("frame too large"))?;
		let pool = shm.create_pool(file.as_fd(), len, qh, ());
		let buffer =
			pool.create_buffer(0, width as i32, height as i32, stride as i32, format, qh, ());
		Ok(Self { map, pool, buffer, key: (format, width, height, stride) })
	}
}

impl Drop for ShmBuffer {
	fn drop(&mut self) {
		self.buffer.destroy();
		self.pool.destroy();
	}
}

/// Map our memfd.
#[allow(unsafe_code)]
fn map_shared(file: &std::fs::File) -> Result<MmapMut> {
	// SAFETY: memmap2 needs the file not to be truncated, and not to be
	// written by others while Rust reads the mapping. The file is an
	// anonymous memfd we created and never resize after mapping; the only
	// other party is the compositor, which writes into it only between our
	// copy request and its `ready` / `failed` event. The capture loop reads
	// the mapping only after `ready`, and requests the next copy only after
	// reading.
	Ok(unsafe { MmapMut::map_mut(file) }?)
}

/// The protocol the compositor offers.
enum Capturer {
	Ext {
		manager: ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
		sources: ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
	},
	Wlr(zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1),
}

/// A connection with the globals we need.
struct Display {
	conn: Connection,
	queue: EventQueue<State>,
	state: State,
	shm: wl_shm::WlShm,
	capturer: Capturer,
}

impl Display {
	fn connect(socket: Option<&PathBuf>) -> Result<Self> {
		let conn = match socket {
			Some(path) => {
				let stream = UnixStream::connect(path)
					.map_err(|e| unavailable(format!("{}: {e}", path.display())))?;
				Connection::from_socket(stream).map_err(unavailable)?
			}
			None => Connection::connect_to_env().map_err(unavailable)?,
		};
		let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(unavailable)?;
		let qh = queue.handle();
		let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(unavailable)?;
		let capturer = match (globals.bind(&qh, 1..=1, ()), globals.bind(&qh, 1..=1, ())) {
			(Ok(manager), Ok(sources)) => Capturer::Ext { manager, sources },
			_ => Capturer::Wlr(globals.bind(&qh, 1..=3, ()).map_err(|_| {
				unavailable(
					"the compositor has neither ext-image-copy-capture-v1 nor \
					 wlr-screencopy-unstable-v1",
				)
			})?),
		};
		let mut state = State::default();
		bind_outputs(&globals, &qh, &mut state);
		// Outputs describe themselves after binding.
		queue.roundtrip(&mut state).map_err(unavailable)?;
		Ok(Self { conn, queue, state, shm, capturer })
	}

	fn protocol(&self) -> &'static str {
		match self.capturer {
			Capturer::Ext { .. } => "ext-image-copy-capture-v1",
			Capturer::Wlr(_) => "wlr-screencopy-unstable-v1",
		}
	}

	/// Dispatch events for up to `timeout`.
	fn dispatch(&mut self, timeout: Duration) -> Result<()> {
		self.queue.dispatch_pending(&mut self.state).map_err(failed)?;
		self.queue.flush().map_err(failed)?;
		if let Some(guard) = self.queue.prepare_read() {
			let readable = {
				let fd = guard.connection_fd();
				let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
				let timeout = rustix::event::Timespec {
					tv_sec: timeout.as_secs() as _,
					tv_nsec: timeout.subsec_nanos() as _,
				};
				match rustix::event::poll(&mut fds, Some(&timeout)) {
					Ok(n) => n > 0,
					Err(rustix::io::Errno::INTR) => false,
					Err(e) => return Err(failed(format!("poll: {e}"))),
				}
			};
			if readable {
				guard.read().map_err(failed)?;
			}
		}
		self.queue.dispatch_pending(&mut self.state).map_err(failed)?;
		Ok(())
	}

	/// Dispatch until `done` holds; `false` if `stop` got set or `timeout`
	/// passed first.
	fn wait(
		&mut self,
		stop: &AtomicBool,
		timeout: Option<Duration>,
		done: impl Fn(&Frame) -> bool,
	) -> Result<bool> {
		let deadline = timeout.map(|t| Instant::now() + t);
		while !done(&self.state.frame) {
			if stop.load(Ordering::Relaxed) || deadline.is_some_and(|d| Instant::now() >= d) {
				return Ok(false);
			}
			self.dispatch(POLL)?;
		}
		Ok(true)
	}
}

fn bind_outputs(globals: &GlobalList, qh: &QueueHandle<State>, state: &mut State) {
	let outputs: Vec<(u32, u32)> = globals.contents().with_list(|list| {
		list.iter().filter(|g| g.interface == "wl_output").map(|g| (g.name, g.version)).collect()
	});
	for (name, version) in outputs {
		let index = state.outputs.len();
		let output: wl_output::WlOutput = globals.registry().bind(name, version.min(4), qh, index);
		state.outputs.push((output, Output::default()));
	}
}

/// wlroots capture backend. Sources are the compositor's outputs, as
/// [`SourceId::Monitor`] by their order.
#[derive(Default)]
pub struct WlrootsCapture {
	socket: Option<PathBuf>,
	worker: Option<Worker>,
}

impl WlrootsCapture {
	/// Capture from `$WAYLAND_DISPLAY`.
	pub fn new() -> Self {
		Self::default()
	}

	/// Capture from another Wayland display: a socket name in
	/// `$XDG_RUNTIME_DIR` (`"wayland-1"`) or a path.
	pub fn with_display(display: impl Into<PathBuf>) -> Self {
		let display = display.into();
		let socket = if display.is_absolute() {
			display
		} else {
			std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_default().join(display)
		};
		Self { socket: Some(socket), worker: None }
	}

	/// Whether the compositor offers one of the capture protocols.
	pub fn is_available(&self) -> bool {
		Display::connect(self.socket.as_ref()).is_ok()
	}
}

impl ScreenCapture for WlrootsCapture {
	fn backend(&self) -> &'static str {
		BACKEND
	}

	fn sources(&mut self) -> Result<Vec<CaptureSource>> {
		let display = Display::connect(self.socket.as_ref())?;
		Ok(display
			.state
			.outputs
			.iter()
			.enumerate()
			.map(|(i, (_, o))| CaptureSource {
				id: SourceId::Monitor(i as u32),
				name: o
					.description
					.clone()
					.or_else(|| o.name.clone())
					.unwrap_or_else(|| format!("Output {}", i + 1)),
				width: o.mode.0.max(0) as u32,
				height: o.mode.1.max(0) as u32,
				primary: i == 0,
			})
			.collect())
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
		let cursor = options.cursor;
		Box::pin(async move {
			self.stop();
			let SourceId::Monitor(index) = source else {
				return Err(Error::SourceNotFound(source));
			};
			let socket = self.socket.clone();
			let (init_tx, init_rx) = mpsc::channel();
			self.worker = Some(Worker::spawn("voelin-wlroots-capture", move |stop| {
				let display = Display::connect(socket.as_ref()).and_then(|d| {
					if (index as usize) < d.state.outputs.len() {
						Ok(d)
					} else {
						Err(Error::SourceNotFound(SourceId::Monitor(index)))
					}
				});
				let mut display = match display {
					Ok(d) => d,
					Err(e) => {
						let _ = init_tx.send(Err(e));
						return;
					}
				};
				let protocol = display.protocol();
				debug!(protocol, output = index, "wlroots capture");
				let _ = init_tx.send(Ok(()));
				if let Err(e) = capture_loop(&mut display, index as usize, cursor, sink, &stop) {
					warn!("wlroots capture stopped: {e}");
				}
			})?);
			match init_rx.recv() {
				Ok(result) => result,
				Err(_) => Err(failed("the capture thread ended during setup")),
			}
		})
	}

	fn stop(&mut self) {
		self.worker = None;
	}
}

/// Sleep until `until`, or until `stop` is set. `false` if stopped.
fn sleep_until(until: Instant, stop: &AtomicBool) -> bool {
	while !stop.load(Ordering::Relaxed) {
		let now = Instant::now();
		if now >= until {
			return true;
		}
		// Woken early by `Worker::stop`.
		std::thread::park_timeout(until - now);
	}
	false
}

fn capture_loop(
	display: &mut Display,
	output: usize,
	cursor: bool,
	mut sink: Box<dyn FrameSink>,
	stop: &AtomicBool,
) -> Result<()> {
	let qh = display.queue.handle();
	let wl_output = display.state.outputs[output].0.clone();
	let session = match &display.capturer {
		Capturer::Ext { manager, sources } => {
			let source = sources.create_source(&wl_output, &qh, ());
			let options = if cursor {
				ext_image_copy_capture_manager_v1::Options::PaintCursors
			} else {
				ext_image_copy_capture_manager_v1::Options::empty()
			};
			Some((manager.create_session(&source, options, &qh, ()), source))
		}
		Capturer::Wlr(_) => None,
	};
	let started = Instant::now();
	let mut buffer: Option<ShmBuffer> = None;
	// Rows of a y-inverted frame, flipped.
	let mut flipped: Vec<u8> = Vec::new();
	let mut next_due = Instant::now();
	let mut failures = 0;
	loop {
		if !sleep_until(next_due, stop) {
			break;
		}
		// Describe the buffer: per frame (wlr), or per session (ext).
		let frame = &mut display.state.frame;
		frame.ready = false;
		frame.failed = None;
		frame.y_invert = false;
		let wlr_frame = match &display.capturer {
			Capturer::Wlr(manager) => {
				frame.formats.clear();
				frame.described = false;
				Some(manager.capture_output(i32::from(cursor), &wl_output, &qh, ()))
			}
			Capturer::Ext { .. } => None,
		};
		let described = display.wait(stop, Some(SETUP_TIMEOUT), |f| f.described || f.stopped)?;
		if !described || display.state.frame.stopped {
			if let Some(f) = wlr_frame {
				f.destroy();
			}
			if stop.load(Ordering::Relaxed) || display.state.frame.stopped {
				break;
			}
			return Err(failed("the compositor did not describe the frame buffer"));
		}
		let frame = &display.state.frame;
		let Some(format) = FORMATS.iter().map(|(f, _)| *f).find(|f| frame.formats.contains(f))
		else {
			return Err(failed(format!("no readable shared-memory format in {:?}", frame.formats)));
		};
		let key = (format, frame.size.0, frame.size.1, frame.stride);
		if buffer.as_ref().is_none_or(|b| b.key != key) {
			// The old buffer goes before the new one is allocated.
			drop(buffer.take());
			buffer = Some(ShmBuffer::new(&display.shm, &qh, format, frame.size, frame.stride)?);
		}
		let shm = buffer.as_ref().expect("created above");
		let ext_frame = match (&wlr_frame, &session) {
			(Some(f), _) => {
				if f.version() >= 2 {
					f.copy_with_damage(&shm.buffer);
				} else {
					f.copy(&shm.buffer);
				}
				None
			}
			(None, Some((session, _))) => {
				let f = session.create_frame(&qh, ());
				f.attach_buffer(&shm.buffer);
				// Everything may have changed since the buffer was used.
				f.damage_buffer(0, 0, i32::MAX, i32::MAX);
				f.capture();
				Some(f)
			}
			(None, None) => unreachable!("one of the protocols is bound"),
		};
		let done = display.wait(stop, None, |f| f.ready || f.failed.is_some())?;
		if let Some(f) = wlr_frame {
			f.destroy();
		}
		if let Some(f) = ext_frame {
			f.destroy();
		}
		if !done {
			break;
		}
		if let Some(reason) = display.state.frame.failed.take() {
			if display.state.frame.stopped {
				break;
			}
			failures += 1;
			if failures > MAX_FAILURES {
				return Err(failed(reason));
			}
			// Buffer constraints changed (ext): the session describes the
			// new ones.
			buffer = None;
			continue;
		}
		failures = 0;
		let timestamp = started.elapsed();
		let fps = sink.max_fps().max(1);
		next_due = Instant::now() + Duration::from_secs(1) / fps * 7 / 8;
		if !sink.wants(timestamp) {
			continue;
		}
		let (width, height, stride) = (key.1, key.2, key.3 as usize);
		let bgra = FORMATS.iter().any(|(f, bgra)| *f == format && *bgra);
		let mut bytes: &[u8] = &shm.map;
		if display.state.frame.y_invert {
			// Rare (some renderers read back bottom-up): flip into a buffer.
			flipped.clear();
			for row in shm.map.chunks_exact(stride).take(height as usize).rev() {
				flipped.extend_from_slice(row);
			}
			bytes = &flipped;
		}
		let plane = PlaneRef::new(bytes, stride);
		let pixels = if bgra { PixelsRef::Bgra(plane) } else { PixelsRef::Rgba(plane) };
		if !sink.frame(FrameRef { width, height, timestamp, pixels }) {
			break;
		}
	}
	if let Some((session, source)) = session {
		session.destroy();
		source.destroy();
	}
	let _ = display.conn.flush();
	Ok(())
}
