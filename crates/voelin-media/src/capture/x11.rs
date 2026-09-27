//! X11 screen and window capture through x11rb.
//!
//! Pixels come through MIT-SHM (a memfd passed to the server, MIT-SHM 1.2)
//! or, for remote displays and old servers, plain GetImage. XFixes provides
//! the cursor image, which is blended into the frame. Supports 24/32-bit
//! TrueColor visuals (BGRx in memory).
//!
//! Window capture reads the window's own contents, clipped to the screen.
//! Without a compositor, parts covered by other windows may be stale.

mod shm;

use std::fmt::Display;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use memmap2::MmapMut;
use tracing::{debug, warn};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
	AtomEnum, ConnectionExt as _, ImageFormat, ImageOrder, MapState, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use crate::capture::{
	BoxFuture, CaptureOptions, CaptureSource, FrameSink, QueueSink, ScreenCapture, SourceId,
	Ticker, Worker,
};
use crate::frame::{FrameRef, PixelsRef, PlaneRef, VideoFrame};
use crate::queue::FrameReceiver;
use crate::{Error, Result};

const BACKEND: &str = "x11";
/// How often the monitor or window position is looked up again.
const REGION_REFRESH: Duration = Duration::from_secs(1);

fn unavailable(e: impl Display) -> Error {
	Error::CaptureUnavailable { backend: BACKEND, reason: e.to_string() }
}

fn failed(e: impl Display) -> Error {
	Error::Capture { backend: BACKEND, message: e.to_string() }
}

/// X11 capture backend.
pub struct X11Capture {
	display: Option<String>,
	use_shm: bool,
	worker: Option<Worker>,
}

impl Default for X11Capture {
	fn default() -> Self {
		Self::new()
	}
}

impl X11Capture {
	/// Capture from `$DISPLAY`.
	pub fn new() -> Self {
		Self { display: None, use_shm: true, worker: None }
	}

	/// Capture from another display (e.g. `":101"`).
	pub fn with_display(display: impl Into<String>) -> Self {
		Self { display: Some(display.into()), ..Self::new() }
	}

	/// Use MIT-SHM when the server supports it (default). Off forces
	/// GetImage.
	pub fn use_shm(mut self, enabled: bool) -> Self {
		self.use_shm = enabled;
		self
	}
}

impl ScreenCapture for X11Capture {
	fn backend(&self) -> &'static str {
		BACKEND
	}

	fn sources(&mut self) -> Result<Vec<CaptureSource>> {
		let x = Display11::connect(self.display.as_deref())?;
		let mut sources: Vec<CaptureSource> = x
			.monitors()?
			.into_iter()
			.enumerate()
			.map(|(i, m)| CaptureSource {
				id: SourceId::Monitor(i as u32),
				name: m.name,
				width: m.rect.width.into(),
				height: m.rect.height.into(),
				primary: m.primary,
			})
			.collect();
		sources.extend(x.windows()?);
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

	/// The sink reads the MIT-SHM segment itself (or the GetImage reply);
	/// the cursor is blended into it first.
	fn start_sink(
		&mut self,
		source: &SourceId,
		options: &CaptureOptions,
		mut sink: Box<dyn FrameSink>,
	) -> BoxFuture<'_, Result<()>> {
		let source = source.clone();
		let options = options.clone();
		Box::pin(async move {
			self.stop();
			let x = Display11::connect(self.display.as_deref())?;
			// Fail here, not in the thread, if the source does not exist.
			let mut region = x.region(&source)?;
			let use_shm = self.use_shm;
			self.worker = Some(Worker::spawn("voelin-x11-capture", move |stop| {
				let mut grabber = Grabber::new(&x, use_shm);
				let cursor = options.cursor && x.xfixes;
				let started = Instant::now();
				let mut fps = sink.max_fps();
				let mut ticker = Ticker::new(fps);
				let mut region_at = Instant::now();
				loop {
					let timestamp = started.elapsed();
					if sink.wants(timestamp) {
						// Monitors and windows move rarely: look them up once a
						// second, and again when a grab fails (e.g. a window
						// shrank).
						if region_at.elapsed() >= REGION_REFRESH {
							region_at = Instant::now();
							match x.region(&source) {
								Ok(r) => region = r,
								Err(e) => {
									warn!("X11 capture stopped: {e}");
									break;
								}
							}
						}
						let mut result = capture(&x, &region, &mut grabber, cursor);
						if result.is_err() {
							region_at = Instant::now();
							if let Ok(r) = x.region(&source) {
								region = r;
								result = capture(&x, &region, &mut grabber, cursor);
							}
						}
						if let Err(e) = result {
							// E.g. the window was closed: end the stream.
							warn!("X11 capture stopped: {e}");
							break;
						}
						let (width, height) = (region.rect.width, region.rect.height);
						let plane = PlaneRef::new(grabber.pixels(), usize::from(width) * 4);
						let frame = FrameRef {
							width: width.into(),
							height: height.into(),
							timestamp,
							pixels: PixelsRef::Bgra(plane),
						};
						if !sink.frame(frame) {
							break;
						}
					}
					if sink.max_fps() != fps {
						fps = sink.max_fps();
						ticker.set_fps(fps);
					}
					if !ticker.wait(&stop) {
						break;
					}
				}
			})?);
			Ok(())
		})
	}

	fn stop(&mut self) {
		self.worker = None;
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
	x: i16,
	y: i16,
	width: u16,
	height: u16,
}

struct Monitor {
	name: String,
	rect: Rect,
	primary: bool,
}

/// Part of a drawable to read, and where it is on the root window.
struct Region {
	drawable: Window,
	rect: Rect,
	root_x: i32,
	root_y: i32,
}

struct Atoms {
	net_client_list: u32,
	net_wm_name: u32,
	utf8_string: u32,
}

struct Display11 {
	conn: RustConnection,
	root: Window,
	screen: Rect,
	shm: bool,
	xfixes: bool,
	atoms: Atoms,
}

impl Display11 {
	fn connect(display: Option<&str>) -> Result<Self> {
		let (conn, screen_num) = RustConnection::connect(display).map_err(unavailable)?;
		let setup = conn.setup();
		if setup.image_byte_order != ImageOrder::LSB_FIRST {
			return Err(unavailable("MSB-first X servers are not supported"));
		}
		let screen = &setup.roots[screen_num];
		let (root, root_depth) = (screen.root, screen.root_depth);
		let screen_rect =
			Rect { x: 0, y: 0, width: screen.width_in_pixels, height: screen.height_in_pixels };
		let bpp =
			setup.pixmap_formats.iter().find(|f| f.depth == root_depth).map(|f| f.bits_per_pixel);
		if bpp != Some(32) {
			return Err(unavailable(format!(
				"unsupported screen depth {root_depth} ({bpp:?} bits per pixel), need 24/32-bit TrueColor"
			)));
		}

		// MIT-SHM 1.2 is needed to pass a memfd.
		let shm = conn
			.shm_query_version()
			.ok()
			.and_then(|c| c.reply().ok())
			.is_some_and(|v| (v.major_version, v.minor_version) >= (1, 2));
		let xfixes = conn
			.xfixes_query_version(4, 0)
			.ok()
			.and_then(|c| c.reply().ok())
			.is_some_and(|v| v.major_version >= 1);
		let intern = |name: &[u8]| -> Result<u32> {
			Ok(conn.intern_atom(false, name).map_err(failed)?.reply().map_err(failed)?.atom)
		};
		let atoms = Atoms {
			net_client_list: intern(b"_NET_CLIENT_LIST")?,
			net_wm_name: intern(b"_NET_WM_NAME")?,
			utf8_string: intern(b"UTF8_STRING")?,
		};
		debug!(shm, xfixes, "connected to X11 display");
		Ok(Self { conn, root, screen: screen_rect, shm, xfixes, atoms })
	}

	/// RandR 1.5 monitors, or the whole screen.
	fn monitors(&self) -> Result<Vec<Monitor>> {
		let randr = self
			.conn
			.randr_query_version(1, 5)
			.ok()
			.and_then(|c| c.reply().ok())
			.is_some_and(|v| (v.major_version, v.minor_version) >= (1, 5));
		let mut monitors = Vec::new();
		if randr
			&& let Ok(reply) =
				self.conn.randr_get_monitors(self.root, true).map_err(failed)?.reply()
		{
			for m in reply.monitors {
				let name = self
					.conn
					.get_atom_name(m.name)
					.ok()
					.and_then(|c| c.reply().ok())
					.map(|r| String::from_utf8_lossy(&r.name).into_owned())
					.unwrap_or_else(|| "Monitor".into());
				let rect = Rect { x: m.x, y: m.y, width: m.width, height: m.height };
				if let Some(rect) = clip(rect, self.screen) {
					monitors.push(Monitor { name, rect, primary: m.primary });
				}
			}
		}
		if monitors.is_empty() {
			monitors.push(Monitor { name: "Screen".into(), rect: self.screen, primary: true });
		}
		Ok(monitors)
	}

	fn property(&self, window: Window, property: u32, type_: u32) -> Option<Vec<u8>> {
		let reply = self
			.conn
			.get_property(false, window, property, type_, 0, 1 << 16)
			.ok()?
			.reply()
			.ok()?;
		(reply.type_ != u32::from(AtomEnum::NONE)).then_some(reply.value)
	}

	fn window_name(&self, window: Window) -> Option<String> {
		let name = self
			.property(window, self.atoms.net_wm_name, self.atoms.utf8_string)
			.or_else(|| self.property(window, AtomEnum::WM_NAME.into(), AtomEnum::STRING.into()))?;
		let name = String::from_utf8_lossy(&name).trim().to_owned();
		(!name.is_empty()).then_some(name)
	}

	/// Named top-level windows: the window manager's client list, or mapped
	/// children of the root when there is no window manager.
	fn windows(&self) -> Result<Vec<CaptureSource>> {
		let clients: Vec<Window> =
			match self.property(self.root, self.atoms.net_client_list, AtomEnum::WINDOW.into()) {
				Some(v) if !v.is_empty() => {
					v.chunks_exact(4).map(|c| u32::from_ne_bytes(c.try_into().unwrap())).collect()
				}
				_ => {
					let tree =
						self.conn.query_tree(self.root).map_err(failed)?.reply().map_err(failed)?;
					tree.children
						.into_iter()
						.filter(|&w| {
							self.conn
								.get_window_attributes(w)
								.ok()
								.and_then(|c| c.reply().ok())
								.is_some_and(|a| {
									a.map_state == MapState::VIEWABLE
										&& a.class == WindowClass::INPUT_OUTPUT
										&& !a.override_redirect
								})
						})
						.collect()
				}
			};
		let mut windows = Vec::new();
		for window in clients {
			let Some(name) = self.window_name(window) else { continue };
			let Ok(geometry) = self.conn.get_geometry(window).map_err(failed)?.reply() else {
				continue;
			};
			windows.push(CaptureSource {
				id: SourceId::Window(window.into()),
				name,
				width: geometry.width.into(),
				height: geometry.height.into(),
				primary: false,
			});
		}
		Ok(windows)
	}

	/// What to read for `source` right now.
	fn region(&self, source: &SourceId) -> Result<Region> {
		match *source {
			SourceId::Monitor(i) => {
				let monitors = self.monitors()?;
				let m = monitors.get(i as usize).ok_or(Error::SourceNotFound(source.clone()))?;
				Ok(Region {
					drawable: self.root,
					rect: m.rect,
					root_x: m.rect.x.into(),
					root_y: m.rect.y.into(),
				})
			}
			SourceId::Window(id) => {
				let window =
					Window::try_from(id).map_err(|_| Error::SourceNotFound(source.clone()))?;
				let not_found = || Error::SourceNotFound(source.clone());
				let geometry = self
					.conn
					.get_geometry(window)
					.map_err(failed)?
					.reply()
					.map_err(|_| not_found())?;
				let bpp = self
					.conn
					.setup()
					.pixmap_formats
					.iter()
					.find(|f| f.depth == geometry.depth)
					.map(|f| f.bits_per_pixel);
				if bpp != Some(32) {
					return Err(failed(format!(
						"window depth {} is not supported",
						geometry.depth
					)));
				}
				let origin = self
					.conn
					.translate_coordinates(window, self.root, 0, 0)
					.map_err(failed)?
					.reply()
					.map_err(|_| not_found())?;
				let on_root = Rect {
					x: origin.dst_x,
					y: origin.dst_y,
					width: geometry.width,
					height: geometry.height,
				};
				// GetImage fails for parts of a window outside the screen.
				let visible =
					clip(on_root, self.screen).ok_or_else(|| failed("the window is off screen"))?;
				Ok(Region {
					drawable: window,
					rect: Rect {
						x: visible.x - on_root.x,
						y: visible.y - on_root.y,
						width: visible.width,
						height: visible.height,
					},
					root_x: visible.x.into(),
					root_y: visible.y.into(),
				})
			}
			_ => Err(Error::SourceNotFound(source.clone())),
		}
	}
}

/// Intersection of two rectangles.
fn clip(a: Rect, b: Rect) -> Option<Rect> {
	let x0 = i32::from(a.x).max(b.x.into());
	let y0 = i32::from(a.y).max(b.y.into());
	let x1 = (i32::from(a.x) + i32::from(a.width)).min(i32::from(b.x) + i32::from(b.width));
	let y1 = (i32::from(a.y) + i32::from(a.height)).min(i32::from(b.y) + i32::from(b.height));
	(x1 > x0 && y1 > y0).then(|| Rect {
		x: x0 as i16,
		y: y0 as i16,
		width: (x1 - x0) as u16,
		height: (y1 - y0) as u16,
	})
}

struct Segment {
	id: u32,
	map: MmapMut,
}

/// Reads regions through MIT-SHM, falling back to GetImage.
struct Grabber {
	shm: bool,
	segment: Option<Segment>,
	/// The last GetImage reply.
	image: Vec<u8>,
	/// Where the last grab is: the segment (`true`) or `image`, and its
	/// length.
	last: (bool, usize),
}

impl Grabber {
	fn new(x: &Display11, want_shm: bool) -> Self {
		Self { shm: want_shm && x.shm, segment: None, image: Vec::new(), last: (false, 0) }
	}

	/// Read the region's pixels (tightly packed BGRx) into the shared memory
	/// segment, or with GetImage; [`Grabber::pixels`] has them.
	fn grab(&mut self, x: &Display11, r: &Region) -> Result<()> {
		let size = usize::from(r.rect.width) * usize::from(r.rect.height) * 4;
		if self.shm {
			match self.grab_shm(x, r, size) {
				Ok(()) => {
					self.last = (true, size);
					return Ok(());
				}
				Err(e) => {
					debug!("MIT-SHM capture failed, using GetImage: {e}");
					self.shm = false;
					self.release(x);
				}
			}
		}
		let reply = x
			.conn
			.get_image(
				ImageFormat::Z_PIXMAP,
				r.drawable,
				r.rect.x,
				r.rect.y,
				r.rect.width,
				r.rect.height,
				!0,
			)
			.map_err(failed)?
			.reply()
			.map_err(failed)?;
		self.image = reply.data;
		if self.image.len() < size {
			return Err(failed(format!(
				"GetImage returned {} bytes, expected {size}",
				self.image.len()
			)));
		}
		self.last = (false, size);
		Ok(())
	}

	/// The pixels of the last grab.
	fn pixels(&mut self) -> &mut [u8] {
		match (self.last, &mut self.segment) {
			((true, size), Some(segment)) => &mut segment.map[..size],
			((_, size), _) => {
				let len = size.min(self.image.len());
				&mut self.image[..len]
			}
		}
	}

	fn grab_shm(&mut self, x: &Display11, r: &Region, size: usize) -> Result<()> {
		if self.segment.as_ref().is_none_or(|s| s.map.len() < size) {
			self.release(x);
			self.segment = Some(Self::attach(x, size)?);
		}
		let segment = self.segment.as_ref().expect("attached above");
		x.conn
			.shm_get_image(
				r.drawable,
				r.rect.x,
				r.rect.y,
				r.rect.width,
				r.rect.height,
				!0,
				ImageFormat::Z_PIXMAP.into(),
				segment.id,
				0,
			)
			.map_err(failed)?
			.reply()
			.map_err(failed)?;
		Ok(())
	}

	fn attach(x: &Display11, size: usize) -> Result<Segment> {
		let fd = rustix::fs::memfd_create("voelin-x11-capture", rustix::fs::MemfdFlags::CLOEXEC)
			.map_err(|e| failed(format!("memfd_create: {e}")))?;
		let file = std::fs::File::from(fd);
		file.set_len(size as u64)?;
		let map = shm::map(&file)?;
		let id = x.conn.generate_id().map_err(failed)?;
		x.conn
			.shm_attach_fd(id, OwnedFd::from(file), false)
			.map_err(failed)?
			.check()
			.map_err(failed)?;
		Ok(Segment { id, map })
	}

	fn release(&mut self, x: &Display11) {
		if let Some(segment) = self.segment.take() {
			let _ = x.conn.shm_detach(segment.id);
			let _ = x.conn.flush();
		}
	}
}

/// A cursor image: premultiplied ARGB, top-left corner in frame coordinates.
struct Cursor<'a> {
	left: i32,
	top: i32,
	width: usize,
	height: usize,
	argb: &'a [u32],
}

/// Blend a premultiplied ARGB cursor onto packed BGRx pixels.
fn blend_cursor(bgrx: &mut [u8], width: usize, height: usize, cursor: &Cursor<'_>) {
	for cy in 0..cursor.height {
		let py = cursor.top + cy as i32;
		if py < 0 || py >= height as i32 {
			continue;
		}
		for cx in 0..cursor.width {
			let px = cursor.left + cx as i32;
			if px < 0 || px >= width as i32 {
				continue;
			}
			let argb = cursor.argb[cy * cursor.width + cx];
			let alpha = argb >> 24;
			if alpha == 0 {
				continue;
			}
			let i = (py as usize * width + px as usize) * 4;
			for (c, shift) in [(0, 0), (1, 8), (2, 16)] {
				let src = (argb >> shift) & 0xff;
				let dst = u32::from(bgrx[i + c]);
				bgrx[i + c] = (src + dst * (255 - alpha) / 255).min(255) as u8;
			}
		}
	}
}

/// Grab the region and blend the cursor into it; the pixels are then in
/// [`Grabber::pixels`].
fn capture(x: &Display11, region: &Region, grabber: &mut Grabber, cursor: bool) -> Result<()> {
	grabber.grab(x, region)?;
	let (w, h) = (usize::from(region.rect.width), usize::from(region.rect.height));
	if cursor && let Ok(image) = x.conn.xfixes_get_cursor_image().map_err(failed)?.reply() {
		let cursor = Cursor {
			left: i32::from(image.x) - i32::from(image.xhot) - region.root_x,
			top: i32::from(image.y) - i32::from(image.yhot) - region.root_y,
			width: image.width.into(),
			height: image.height.into(),
			argb: &image.cursor_image,
		};
		if cursor.argb.len() >= cursor.width * cursor.height {
			blend_cursor(grabber.pixels(), w, h, &cursor);
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn clipping() {
		let screen = Rect { x: 0, y: 0, width: 100, height: 50 };
		let inside = Rect { x: 10, y: 10, width: 20, height: 20 };
		assert_eq!(clip(inside, screen), Some(inside));
		let partly = Rect { x: -5, y: 40, width: 20, height: 20 };
		assert_eq!(clip(partly, screen), Some(Rect { x: 0, y: 40, width: 15, height: 10 }));
		assert_eq!(clip(Rect { x: 100, y: 0, width: 5, height: 5 }, screen), None);
	}

	#[test]
	fn cursor_blending() {
		let (w, h) = (4, 3);
		let mut pixels = vec![100; w * h * 4];
		// Opaque red, half-transparent white (premultiplied), transparent.
		let argb = [0xffff0000, 0x80808080, 0x00000000, 0xff00ff00];
		let cursor = Cursor { left: 3, top: 2, width: 2, height: 2, argb: &argb };
		blend_cursor(&mut pixels, w, h, &cursor);
		// Only the top-left cursor pixel lands inside the frame, at (3, 2).
		let at = |x: usize, y: usize| pixels[(y * w + x) * 4..][..3].to_vec();
		assert_eq!(at(3, 2), [0, 0, 255]);
		assert_eq!(at(2, 2), [100, 100, 100]);

		let mut pixels = vec![100; 4];
		let cursor = Cursor { left: 0, top: 0, width: 1, height: 1, argb: &argb[1..2] };
		blend_cursor(&mut pixels, 1, 1, &cursor);
		// 128 + 100 * 127 / 255 = 177
		assert_eq!(pixels[..3], [177, 177, 177]);
	}
}
