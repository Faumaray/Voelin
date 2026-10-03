//! Cameras for the studio.
//!
//! Linux: cameras are listed straight from V4L2 (`/dev/video*`: the card
//! name, the pixel formats and every resolution the driver reports, no
//! hard-coded list), and captured through PipeWire, which is what already
//! owns the devices on a modern desktop. The PipeWire connection comes from
//! the user's daemon, or — in a sandbox, where that is not allowed — from
//! the XDG Camera portal ([`ashpd`]), which asks the user once; both go
//! through the same stream code and the same [`crate::capture::pw::PwThread`]
//! as screen capture.
//!
//! Every camera pixel format PipeWire can give us (YUY2, UYVY, NV12, I420,
//! and the packed 24- and 32-bit ones) is converted to RGBA or taken as BGRA
//! for the compositor, in one SIMD pass into a pooled frame, so a running
//! camera allocates nothing per frame. MJPEG, which many USB cameras offer
//! for their large sizes only (1080p and up at full rate), is decoded with
//! `zune-jpeg` straight into pooled RGBA frames on a thread of its own:
//! the newest picture wins, so a decode that falls behind drops pictures
//! instead of holding up the camera. A raw format is preferred where it
//! gives the wanted size at the wanted rate (no decoding at all).
//!
//! [`SYNTHETIC`] is a camera that is always there: the test pattern. Tests
//! and machines without a camera use it.
//!
//! Windows and Android have no camera backend yet ([`list`] returns only the
//! synthetic one).

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use crate::Result;
use crate::capture::synthetic::SyntheticScreen;
use crate::capture::{CaptureOptions, ScreenCapture, SourceId};
use crate::studio::compose::Feed;
use crate::studio::scene::Background;
use crate::studio::segment::BackgroundFilter;
use crate::studio::source::FeedSink;

/// The id of the camera that is always available: the test pattern.
pub const SYNTHETIC: &str = "synthetic";

const BACKEND: &str = "camera";

/// A pixel format a camera delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pixel {
	/// 4:2:2, two pixels in `Y0 U Y1 V`.
	Yuyv,
	/// 4:2:2, two pixels in `U Y0 V Y1`.
	Uyvy,
	Nv12,
	I420,
	/// 4 bytes per pixel, B G R x.
	Bgrx,
	/// 4 bytes per pixel, R G B x.
	Rgbx,
	/// 3 bytes per pixel.
	Rgb24,
	Bgr24,
	/// Motion JPEG, decoded on a thread of its own (see the
	/// [module docs](self)).
	Mjpeg,
}

impl Pixel {
	/// The format of a V4L2 / DRM four-character code.
	pub fn from_fourcc(code: u32) -> Option<Self> {
		Some(match &code.to_le_bytes() {
			b"YUYV" | b"YUY2" => Self::Yuyv,
			b"UYVY" => Self::Uyvy,
			b"NV12" => Self::Nv12,
			b"YU12" | b"I420" => Self::I420,
			b"XR24" | b"AR24" | b"BGR4" => Self::Bgrx,
			b"XB24" | b"AB24" | b"RGB4" => Self::Rgbx,
			b"RGB3" => Self::Rgb24,
			b"BGR3" => Self::Bgr24,
			b"MJPG" | b"JPEG" => Self::Mjpeg,
			_ => return None,
		})
	}

	/// The name shown in a picker.
	pub fn label(self) -> &'static str {
		match self {
			Self::Yuyv => "YUYV",
			Self::Uyvy => "UYVY",
			Self::Nv12 => "NV12",
			Self::I420 => "I420",
			Self::Bgrx => "BGRx",
			Self::Rgbx => "RGBx",
			Self::Rgb24 => "RGB",
			Self::Bgr24 => "BGR",
			Self::Mjpeg => "MJPEG",
		}
	}
}

/// One pixel format of a camera, with the sizes the driver reports for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Format {
	pub pixel: Pixel,
	/// Every resolution the driver lists, largest first; empty for a driver
	/// that only reports a range.
	pub sizes: Vec<(u32, u32)>,
	/// Highest frame rate of the largest size, or 0 if unknown.
	pub max_fps: u32,
}

/// A camera as shown in a picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Camera {
	/// What a scene's `SourceKind::Camera { device }` names: the device path
	/// on Linux, or [`SYNTHETIC`].
	pub id: String,
	pub name: String,
	/// Where the list came from (`"v4l2"`, `"synthetic"`).
	pub backend: &'static str,
	pub formats: Vec<Format>,
}

/// Every camera this session can use. Never fails: a device that cannot be
/// asked is left out, and [`SYNTHETIC`] is always last.
pub fn list() -> Vec<Camera> {
	// Devices are listed where they can be captured: through PipeWire.
	#[cfg(all(target_os = "linux", feature = "pipewire"))]
	let devices = v4l2::list();
	#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
	let devices = Vec::new();
	let synthetic = Camera {
		id: SYNTHETIC.to_owned(),
		name: "Test pattern".to_owned(),
		backend: "synthetic",
		formats: vec![Format { pixel: Pixel::Bgrx, sizes: vec![(1280, 720)], max_fps: 60 }],
	};
	devices.into_iter().chain([synthetic]).collect()
}

/// A running camera; stops when dropped.
pub struct Capture {
	backend: &'static str,
	device: String,
	screen: Option<Box<dyn ScreenCapture>>,
	#[cfg(all(target_os = "linux", feature = "pipewire"))]
	stream: Option<pipewire_camera::Stream>,
}

impl Capture {
	/// Open `device` (a [`Camera::id`]; empty: the first camera, else the
	/// test pattern) and feed `feed` with up to `fps` frames a second at
	/// `size` if the camera has it.
	///
	/// Must run on a Tokio runtime: the camera portal talks D-Bus.
	pub async fn start(
		device: &str,
		size: Option<(u32, u32)>,
		fps: u32,
		feed: Arc<Feed>,
		background: Background,
	) -> Result<Self> {
		let fps = fps.max(1);
		let wanted = match device.trim() {
			"" => list()
				.into_iter()
				.find(|c| c.backend != "synthetic")
				.map_or_else(|| SYNTHETIC.to_owned(), |c| c.id),
			name => name.to_owned(),
		};
		if wanted == SYNTHETIC {
			let (w, h) = size.unwrap_or((1280, 720));
			let mut screen = SyntheticScreen::new(w.max(2), h.max(2));
			let options = CaptureOptions { fps, cursor: false, ..CaptureOptions::default() };
			let mut sink = FeedSink::new(feed, Arc::new(AtomicU32::new(fps)));
			if background.needs_mask() {
				sink = sink
					.with_background(BackgroundFilter::with_default_segmenter(background.clone()));
			}
			screen.start_sink(&SourceId::Synthetic, &options, Box::new(sink)).await?;
			return Ok(Self {
				backend: "synthetic",
				device: wanted,
				screen: Some(Box::new(screen)),
				#[cfg(all(target_os = "linux", feature = "pipewire"))]
				stream: None,
			});
		}
		#[cfg(all(target_os = "linux", feature = "pipewire"))]
		{
			let formats = v4l2::list()
				.into_iter()
				.find(|c| c.id == wanted)
				.map(|c| c.formats)
				.unwrap_or_default();
			let stream =
				pipewire_camera::Stream::start(&wanted, size, fps, formats, feed, background)
					.await?;
			tracing::debug!(device = %wanted, "camera through PipeWire");
			Ok(Self { backend: "pipewire", device: wanted, screen: None, stream: Some(stream) })
		}
		#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
		{
			let _ = (size, feed, background);
			Err(crate::Error::CaptureUnavailable {
				backend: BACKEND,
				reason: format!("no camera backend for {wanted:?} in this build"),
			})
		}
	}

	/// Where the frames come from (`"pipewire"`, `"portal"`, `"synthetic"`).
	pub fn backend(&self) -> &'static str {
		#[cfg(all(target_os = "linux", feature = "pipewire"))]
		if let Some(stream) = &self.stream {
			return stream.backend();
		}
		self.backend
	}

	/// The camera that was opened.
	pub fn device(&self) -> &str {
		&self.device
	}
}

impl Drop for Capture {
	fn drop(&mut self) {
		if let Some(screen) = &mut self.screen {
			screen.stop();
		}
	}
}

/// Listing cameras through V4L2: the card name and every pixel format and
/// resolution the driver reports.
///
/// Only the three read-only enumeration ioctls are used; frames come through
/// PipeWire. The structures below are the kernel's V4L2 ABI (`videodev2.h`),
/// which is stable and the same on 32- and 64-bit: plain `u8` and `u32`
/// fields, no pointers.
#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod v4l2 {
	#![allow(unsafe_code)]

	use std::os::fd::OwnedFd;

	use rustix::ioctl::{Getter, Opcode, Updater, ioctl, opcode};

	use super::{Camera, Format, Pixel};

	/// `V4L2_BUF_TYPE_VIDEO_CAPTURE`.
	const CAPTURE: u32 = 1;
	/// `V4L2_CAP_VIDEO_CAPTURE`.
	const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
	/// `V4L2_CAP_DEVICE_CAPS`.
	const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
	/// `V4L2_FRMSIZE_TYPE_DISCRETE`.
	const FRMSIZE_DISCRETE: u32 = 1;
	/// `V4L2_FRMIVAL_TYPE_DISCRETE`.
	const FRMIVAL_DISCRETE: u32 = 1;
	/// Enumeration stops here even if the driver never says "no more".
	const MAX_ENTRIES: u32 = 128;

	/// `struct v4l2_capability`.
	#[repr(C)]
	#[derive(Clone, Copy)]
	struct Capability {
		driver: [u8; 16],
		card: [u8; 32],
		bus_info: [u8; 32],
		version: u32,
		capabilities: u32,
		device_caps: u32,
		reserved: [u32; 3],
	}

	/// `struct v4l2_fmtdesc`.
	#[repr(C)]
	#[derive(Clone, Copy)]
	struct FmtDesc {
		index: u32,
		type_: u32,
		flags: u32,
		description: [u8; 32],
		pixelformat: u32,
		mbus_code: u32,
		reserved: [u32; 3],
	}

	/// `struct v4l2_frmsizeenum` (the union as the largest member,
	/// `v4l2_frmsize_stepwise`).
	#[repr(C)]
	#[derive(Clone, Copy)]
	struct FrmSizeEnum {
		index: u32,
		pixel_format: u32,
		type_: u32,
		/// Discrete: `[width, height]`; stepwise: min, max and step of both.
		size: [u32; 6],
		reserved: [u32; 2],
	}

	/// `struct v4l2_frmivalenum` (the union as `v4l2_frmival_stepwise`).
	#[repr(C)]
	#[derive(Clone, Copy)]
	struct FrmIvalEnum {
		index: u32,
		pixel_format: u32,
		width: u32,
		height: u32,
		type_: u32,
		/// Discrete: `[numerator, denominator]` seconds per frame.
		interval: [u32; 6],
		reserved: [u32; 2],
	}

	const QUERYCAP: Opcode = opcode::read::<Capability>(b'V', 0);
	const ENUM_FMT: Opcode = opcode::read_write::<FmtDesc>(b'V', 2);
	const ENUM_FRAMESIZES: Opcode = opcode::read_write::<FrmSizeEnum>(b'V', 74);
	const ENUM_FRAMEINTERVALS: Opcode = opcode::read_write::<FrmIvalEnum>(b'V', 75);

	/// The NUL-terminated ASCII of a fixed-size kernel string field.
	fn text(bytes: &[u8]) -> String {
		let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
		String::from_utf8_lossy(&bytes[..end]).trim().to_owned()
	}

	fn open(path: &std::path::Path) -> Option<OwnedFd> {
		use rustix::fs::{Mode, OFlags};
		rustix::fs::open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK, Mode::empty())
			.ok()
	}

	/// The card name of a capture device, or `None` if it cannot capture
	/// video.
	fn card(fd: &OwnedFd) -> Option<String> {
		// SAFETY: `QUERYCAP` is `_IOR('V', 0, struct v4l2_capability)`, and
		// `Capability` is that structure; the kernel only writes into it.
		let caps = unsafe { ioctl(fd, Getter::<QUERYCAP, Capability>::new()) }.ok()?;
		let usable = if caps.capabilities & CAP_DEVICE_CAPS != 0 {
			caps.device_caps
		} else {
			caps.capabilities
		};
		(usable & CAP_VIDEO_CAPTURE != 0).then(|| text(&caps.card))
	}

	/// Every resolution the driver lists for `pixelformat`, largest first,
	/// and the highest frame rate of the largest one.
	fn sizes(fd: &OwnedFd, pixelformat: u32) -> (Vec<(u32, u32)>, u32) {
		let mut sizes = Vec::new();
		for index in 0..MAX_ENTRIES {
			let mut entry = FrmSizeEnum {
				index,
				pixel_format: pixelformat,
				type_: 0,
				size: [0; 6],
				reserved: [0; 2],
			};
			// SAFETY: `ENUM_FRAMESIZES` is
			// `_IOWR('V', 74, struct v4l2_frmsizeenum)` and `entry` is that
			// structure, which the kernel reads and writes in place.
			if unsafe { ioctl(fd, Updater::<ENUM_FRAMESIZES, FrmSizeEnum>::new(&mut entry)) }
				.is_err()
			{
				break;
			}
			if entry.type_ == FRMSIZE_DISCRETE {
				sizes.push((entry.size[0], entry.size[1]));
			} else {
				// A range: its largest size is all we offer.
				sizes.push((entry.size[1], entry.size[4]));
				break;
			}
		}
		sizes.sort_unstable_by_key(|&(w, h)| std::cmp::Reverse(u64::from(w) * u64::from(h)));
		sizes.dedup();
		let fps = sizes.first().map_or(0, |&(w, h)| max_fps(fd, pixelformat, w, h));
		(sizes, fps)
	}

	/// The highest frame rate the driver lists for one size.
	fn max_fps(fd: &OwnedFd, pixel_format: u32, width: u32, height: u32) -> u32 {
		let mut best = 0;
		for index in 0..MAX_ENTRIES {
			let mut entry = FrmIvalEnum {
				index,
				pixel_format,
				width,
				height,
				type_: 0,
				interval: [0; 6],
				reserved: [0; 2],
			};
			// SAFETY: `ENUM_FRAMEINTERVALS` is
			// `_IOWR('V', 75, struct v4l2_frmivalenum)` and `entry` is that
			// structure, which the kernel reads and writes in place.
			if unsafe { ioctl(fd, Updater::<ENUM_FRAMEINTERVALS, FrmIvalEnum>::new(&mut entry)) }
				.is_err()
			{
				break;
			}
			// Seconds per frame; a stepwise range starts with its smallest
			// interval, which is the highest rate.
			let (num, den) = (entry.interval[0], entry.interval[1]);
			if num > 0 {
				best = best.max(den / num);
			}
			if entry.type_ != FRMIVAL_DISCRETE {
				break;
			}
		}
		best
	}

	/// The pixel formats of a capture device.
	fn formats(fd: &OwnedFd) -> Vec<Format> {
		let mut formats = Vec::new();
		for index in 0..MAX_ENTRIES {
			let mut entry = FmtDesc {
				index,
				type_: CAPTURE,
				flags: 0,
				description: [0; 32],
				pixelformat: 0,
				mbus_code: 0,
				reserved: [0; 3],
			};
			// SAFETY: `ENUM_FMT` is `_IOWR('V', 2, struct v4l2_fmtdesc)` and
			// `entry` is that structure, which the kernel reads and writes
			// in place.
			if unsafe { ioctl(fd, Updater::<ENUM_FMT, FmtDesc>::new(&mut entry)) }.is_err() {
				break;
			}
			let Some(pixel) = Pixel::from_fourcc(entry.pixelformat) else { continue };
			if formats.iter().any(|f: &Format| f.pixel == pixel) {
				continue;
			}
			let (sizes, max_fps) = sizes(fd, entry.pixelformat);
			formats.push(Format { pixel, sizes, max_fps });
		}
		formats
	}

	/// Every `/dev/video*` that can capture video, by path.
	pub fn list() -> Vec<Camera> {
		let Ok(entries) = std::fs::read_dir("/dev") else { return Vec::new() };
		let mut paths: Vec<std::path::PathBuf> = entries
			.flatten()
			.map(|e| e.path())
			.filter(|p| {
				p.file_name()
					.and_then(|n| n.to_str())
					.is_some_and(|n| n.starts_with("video") && n[5..].parse::<u32>().is_ok())
			})
			.collect();
		paths.sort();
		paths
			.into_iter()
			.filter_map(|path| {
				let fd = open(&path)?;
				let name = card(&fd)?;
				let formats = formats(&fd);
				// A device node without a usable format is a metadata or
				// output node of the same camera.
				(!formats.is_empty()).then(|| Camera {
					id: path.to_string_lossy().into_owned(),
					name: if name.is_empty() { path.to_string_lossy().into_owned() } else { name },
					backend: "v4l2",
					formats,
				})
			})
			.collect()
	}
}

/// Camera frames through PipeWire: the user's daemon, or the XDG Camera
/// portal's connection in a sandbox.
#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod pipewire_camera {
	use std::os::fd::OwnedFd;
	use std::sync::Arc;
	use std::time::{Duration, Instant};

	use pipewire as pw;
	use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
	use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
	use pw::spa::pod::{ChoiceValue, Object, Property, PropertyFlags, Value, property};
	use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle, SpaTypes};
	use tracing::{debug, warn};

	use crate::capture::FramePacer;
	use crate::capture::pw::{PwThread, pod, serialize};
	use crate::frame::{FrameRef, PixelsRef, PlaneRef};
	use crate::pool::FramePool;
	use crate::studio::compose::Feed;
	use crate::studio::scene::Background;
	use crate::studio::segment::BackgroundFilter;
	use crate::studio::source::deliver;
	use crate::{Error, Result};

	use super::{BACKEND, Format, Pixel, mjpeg};

	/// The pixel formats we take, in order of preference; all of them the
	/// compositor can use after one conversion.
	const FORMATS: [VideoFormat; 8] = [
		VideoFormat::NV12,
		VideoFormat::I420,
		VideoFormat::YUY2,
		VideoFormat::UYVY,
		VideoFormat::BGRx,
		VideoFormat::RGBx,
		VideoFormat::BGR,
		VideoFormat::RGB,
	];

	/// A running camera stream; stops when dropped.
	pub struct Stream {
		backend: &'static str,
		_thread: PwThread,
	}

	impl Stream {
		/// Connect to `device` (a `/dev/video*` path) and feed `feed`.
		/// `formats` are what V4L2 lists for it (empty if unknown).
		pub async fn start(
			device: &str,
			size: Option<(u32, u32)>,
			fps: u32,
			formats: Vec<Format>,
			feed: Arc<Feed>,
			background: Background,
		) -> Result<Self> {
			// The daemon first: it needs no dialog. A sandbox refuses it, and
			// then the portal's connection is the only way in.
			let offers = enum_formats(size, fps, &formats)
				.map_err(|e| Error::Capture { backend: BACKEND, message: e })?;
			let direct =
				connect(None, device, offers.clone(), fps, feed.clone(), background.clone());
			match direct {
				Ok(thread) => Ok(Self { backend: "pipewire", _thread: thread }),
				Err(direct) => {
					debug!("PipeWire cameras: {direct}; asking the camera portal");
					let fd = portal_fd().await?;
					let thread = connect(Some(fd), device, offers, fps, feed, background)?;
					Ok(Self { backend: "portal", _thread: thread })
				}
			}
		}

		pub fn backend(&self) -> &'static str {
			self.backend
		}
	}

	/// A PipeWire connection with camera access from the XDG Camera portal.
	async fn portal_fd() -> Result<OwnedFd> {
		use ashpd::desktop::camera::Camera;
		let unavailable = |e: String| Error::CaptureUnavailable { backend: BACKEND, reason: e };
		let proxy = Camera::new()
			.await
			.map_err(|e| unavailable(format!("the camera portal is not available: {e}")))?;
		if !proxy
			.is_present()
			.await
			.map_err(|e| unavailable(format!("the camera portal did not answer: {e}")))?
		{
			return Err(unavailable("the desktop reports no camera".into()));
		}
		proxy
			.request_access(Default::default())
			.await
			.map_err(|e| unavailable(format!("camera access was refused: {e}")))?
			.response()
			.map_err(|_| Error::Cancelled)?;
		proxy
			.open_pipe_wire_remote(Default::default())
			.await
			.map_err(|e| unavailable(format!("the camera portal gave no connection: {e}")))
	}

	fn connect(
		fd: Option<OwnedFd>,
		device: &str,
		offers: Vec<Vec<u8>>,
		fps: u32,
		feed: Arc<Feed>,
		background: Background,
	) -> Result<PwThread> {
		let device = device.to_owned();
		PwThread::spawn("voelin-camera", fd, move |core, mainloop| {
			// No target node: the session manager connects the stream to the
			// default camera. Picking one of several by `api.v4l2.path`
			// needs a registry round trip and is not done yet.
			camera_stream(core, mainloop, None, &device, &offers, fps, feed, background)
		})
		.map_err(|e| Error::Capture { backend: BACKEND, message: e })
	}

	struct State {
		feed: Arc<Feed>,
		filter: Option<BackgroundFilter>,
		pool: FramePool,
		pacer: FramePacer,
		started: Instant,
		format: Option<Negotiated>,
		/// The MJPEG decoder, once MJPEG was negotiated.
		mjpeg: Option<mjpeg::Decoder>,
		mainloop: pw::main_loop::MainLoopWeak,
	}

	/// What the camera and we agreed on.
	#[derive(Clone, Copy, Debug)]
	enum Negotiated {
		Raw(VideoFormat, u32, u32),
		/// The size comes with each picture.
		Mjpeg,
	}

	type Parts = (pw::stream::StreamRc, pw::stream::StreamListener<State>);

	#[allow(clippy::too_many_arguments)]
	fn camera_stream(
		core: &pw::core::CoreRc,
		mainloop: &pw::main_loop::MainLoopRc,
		target: Option<u32>,
		device: &str,
		offers: &[Vec<u8>],
		fps: u32,
		feed: Arc<Feed>,
		background: Background,
	) -> std::result::Result<Parts, String> {
		let props = pw::properties::properties! {
			*pw::keys::MEDIA_TYPE => "Video",
			*pw::keys::MEDIA_CATEGORY => "Capture",
			*pw::keys::MEDIA_ROLE => "Camera",
		};
		let stream = pw::stream::StreamRc::new(core.clone(), "voelin-camera", props)
			.map_err(|e| format!("PipeWire stream: {e}"))?;
		let state = State {
			feed,
			filter: background
				.needs_mask()
				.then(|| BackgroundFilter::with_default_segmenter(background)),
			pool: FramePool::new(),
			pacer: FramePacer::new(Some(fps)),
			started: Instant::now(),
			format: None,
			mjpeg: None,
			mainloop: mainloop.downgrade(),
		};
		let listener = stream
			.add_local_listener_with_user_data(state)
			.state_changed(|_, state, _, new| {
				if let pw::stream::StreamState::Error(e) = &new {
					warn!("camera stream failed: {e}");
					state.feed.set_error(format!("camera stream failed: {e}"));
				}
				if matches!(
					new,
					pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected
				) && let Some(mainloop) = state.mainloop.upgrade()
				{
					mainloop.quit();
				}
			})
			.param_changed(|stream, state, id, param| {
				let Some(param) = param else { return };
				if id != pw::spa::param::ParamType::Format.as_raw() {
					return;
				}
				match pw::spa::param::format_utils::parse_format(param) {
					Ok((MediaType::Video, MediaSubtype::Raw)) => {
						let mut info = VideoInfoRaw::new();
						if let Err(e) = info.parse(param) {
							warn!("cannot parse the camera format: {e}");
							return;
						}
						let size = info.size();
						debug!(format = ?info.format(), size.width, size.height, "camera format");
						state.format =
							Some(Negotiated::Raw(info.format(), size.width, size.height));
					}
					Ok((MediaType::Video, MediaSubtype::Mjpg)) => {
						debug!("camera format MJPEG");
						state.format = Some(Negotiated::Mjpeg);
					}
					_ => return,
				}
				// Only buffers the CPU can read: a camera node may otherwise
				// hand out DMA-BUFs, which this path cannot map.
				if let Err(e) = use_memory_buffers(stream) {
					warn!("camera buffers: {e}");
				}
			})
			.process(process)
			.register()
			.map_err(|e| format!("PipeWire listener: {e}"))?;
		let mut pods = offers.iter().map(|f| pod(f)).collect::<std::result::Result<Vec<_>, _>>()?;
		stream
			.connect(
				pw::spa::utils::Direction::Input,
				target,
				pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
				&mut pods,
			)
			.map_err(|e| format!("cannot connect to a camera ({device}): {e}"))?;
		Ok((stream, listener))
	}

	/// Ask for buffers in memory the CPU can read.
	fn use_memory_buffers(stream: &pw::stream::Stream) -> std::result::Result<(), String> {
		let types = (1 << pw::spa::sys::SPA_DATA_MemPtr) | (1 << pw::spa::sys::SPA_DATA_MemFd);
		let buffers = serialize(Value::Object(Object {
			type_: SpaTypes::ObjectParamBuffers.as_raw(),
			id: pw::spa::param::ParamType::Buffers.as_raw(),
			properties: vec![pw::spa::pod::Property {
				key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
				flags: pw::spa::pod::PropertyFlags::empty(),
				value: Value::Int(types),
			}],
		}))?;
		stream.update_params(&mut [pod(&buffers)?]).map_err(|e| e.to_string())
	}

	type Sizes = Vec<(u32, u32)>;

	/// The PipeWire format of a camera pixel format: `None` for MJPEG.
	fn spa_format(pixel: Pixel) -> Option<VideoFormat> {
		Some(match pixel {
			Pixel::Nv12 => VideoFormat::NV12,
			Pixel::I420 => VideoFormat::I420,
			Pixel::Yuyv => VideoFormat::YUY2,
			Pixel::Uyvy => VideoFormat::UYVY,
			Pixel::Bgrx => VideoFormat::BGRx,
			Pixel::Rgbx => VideoFormat::RGBx,
			Pixel::Bgr24 => VideoFormat::BGR,
			Pixel::Rgb24 => VideoFormat::RGB,
			Pixel::Mjpeg => return None,
		})
	}

	/// The offers, best first, from what the driver lists for the camera
	/// (`formats`, from V4L2):
	///
	/// 1. the wanted size at least at the wanted rate, raw formats first
	///    (nothing to decode);
	/// 2. the wanted size at any rate, MJPEG first: cameras give their large
	///    sizes at full rate only compressed (a raw 1080p over USB 2 is a few
	///    frames a second);
	/// 3. every format at the sizes it lists (the one nearest the wanted
	///    size first), raw formats first.
	///
	/// Every offer names sizes the driver lists for its format: PipeWire's
	/// V4L2 source does not always check (it took a fixed size on trust, and
	/// a range too, and then failed to set NV12 at 2560x1440 on a camera
	/// that has that size only as MJPEG). PipeWire takes the first offer
	/// that fits. Without a list (a camera only the portal can see), every
	/// format at the wanted size, then at any.
	fn enum_formats(
		size: Option<(u32, u32)>,
		fps: u32,
		formats: &[Format],
	) -> std::result::Result<Vec<Vec<u8>>, String> {
		let (w, h) = size.unwrap_or((1280, 720));
		let exact = (w.max(2), h.max(2));
		let rect = |(width, height): (u32, u32)| Rectangle { width, height };
		// Our order of preference among raw formats, then MJPEG.
		let rank = |f: Option<VideoFormat>| match f {
			Some(f) => FORMATS.iter().position(|x| *x == f).unwrap_or(FORMATS.len()),
			None => FORMATS.len() + 1,
		};
		let mut known: Vec<(Option<VideoFormat>, Sizes)> = formats
			.iter()
			.map(|f| (spa_format(f.pixel), f.sizes.clone()))
			.filter(|(_, sizes)| !sizes.is_empty())
			.collect();
		known.sort_by_key(|(f, _)| rank(*f));
		// Format (`None`: MJPEG), sizes (empty: any), lowest rate.
		let mut offers: Vec<(Option<VideoFormat>, Sizes, u32)> = Vec::new();
		if known.is_empty() {
			let all: Vec<Option<VideoFormat>> =
				FORMATS.iter().copied().map(Some).chain([None]).collect();
			offers.extend(all.iter().map(|f| (*f, vec![exact], fps)));
			offers.extend(all.iter().map(|f| (*f, Vec::new(), 0)));
		} else {
			let at_size: Vec<Option<VideoFormat>> =
				known.iter().filter(|(_, sizes)| sizes.contains(&exact)).map(|(f, _)| *f).collect();
			offers.extend(at_size.iter().map(|f| (*f, vec![exact], fps)));
			let mjpeg_first = at_size
				.iter()
				.filter(|f| f.is_none())
				.chain(at_size.iter().filter(|f| f.is_some()));
			offers.extend(mjpeg_first.map(|f| (*f, vec![exact], 0)));
			let area = |(w, h): (u32, u32)| i64::from(w) * i64::from(h);
			for (format, sizes) in &known {
				let mut sizes = sizes.clone();
				sizes.sort_by_key(|s| (area(*s) - area(exact)).abs());
				offers.push((*format, sizes, 0));
			}
		}
		let mut params = Vec::with_capacity(offers.len());
		for (format, sizes, min_fps) in offers {
			let mut properties = vec![
				property!(FormatProperties::MediaType, Id, MediaType::Video),
				property!(
					FormatProperties::MediaSubtype,
					Id,
					if format.is_some() { MediaSubtype::Raw } else { MediaSubtype::Mjpg }
				),
			];
			if let Some(format) = format {
				properties.push(property!(FormatProperties::VideoFormat, Id, format));
			}
			// A list (also of one size): the source checks each against
			// what the driver lists. No list: any size, the wanted first.
			let choice = match sizes.split_first() {
				Some((first, _)) => ChoiceEnum::Enum {
					default: rect(*first),
					alternatives: sizes.iter().copied().map(rect).collect(),
				},
				None => ChoiceEnum::Range {
					default: rect(exact),
					min: Rectangle { width: 1, height: 1 },
					max: Rectangle { width: 16384, height: 16384 },
				},
			};
			properties.push(Property {
				key: FormatProperties::VideoSize.as_raw(),
				flags: PropertyFlags::empty(),
				value: Value::Choice(ChoiceValue::Rectangle(Choice(ChoiceFlags::empty(), choice))),
			});
			// The wanted rate or any above it (any at all in the later
			// passes): a camera offers fixed rates (often only 30), and
			// `FramePacer` brings whatever we get down to `fps`.
			properties.push(property!(
				FormatProperties::VideoFramerate,
				Choice,
				Range,
				Fraction,
				Fraction { num: fps, denom: 1 },
				Fraction { num: min_fps, denom: 1 },
				Fraction { num: 1000, denom: 1 }
			));
			params.push(serialize(Value::Object(Object {
				type_: SpaTypes::ObjectParamFormat.as_raw(),
				id: pw::spa::param::ParamType::EnumFormat.as_raw(),
				properties,
			}))?);
		}
		Ok(params)
	}

	fn process(stream: &pw::stream::Stream, state: &mut State) {
		let Some(mut buffer) = stream.dequeue_buffer() else { return };
		let Some(negotiated) = state.format else { return };
		let timestamp = state.started.elapsed();
		if state.feed.is_closed() {
			if let Some(mainloop) = state.mainloop.upgrade() {
				mainloop.quit();
			}
			return;
		}
		if !state.pacer.take(timestamp) {
			return;
		}
		let Some(data) = buffer.datas_mut().first_mut() else { return };
		let chunk = data.chunk();
		if chunk.size() == 0 || chunk.flags().contains(pw::spa::buffer::ChunkFlags::CORRUPTED) {
			return;
		}
		let (offset, stride) = (chunk.offset() as usize, chunk.stride() as usize);
		let size = chunk.size() as usize;
		let Some(bytes) = data.data() else { return };
		let Some(bytes) = bytes.get(offset..) else { return };
		let result = match negotiated {
			Negotiated::Raw(format, width, height) => {
				take(state, format, width, height, stride, bytes, timestamp)
			}
			Negotiated::Mjpeg => {
				let jpeg = &bytes[..size.min(bytes.len())];
				// The decoder starts with the first picture; the background
				// filter moves to its thread.
				let decoder = match &mut state.mjpeg {
					Some(decoder) => decoder,
					slot @ None => {
						match mjpeg::Decoder::start(state.feed.clone(), state.filter.take()) {
							Ok(decoder) => slot.insert(decoder),
							Err(e) => {
								state.feed.set_error(e.to_string());
								return;
							}
						}
					}
				};
				decoder.put(jpeg, timestamp);
				Ok(())
			}
		};
		if let Err(e) = result {
			warn!("cannot take a camera frame: {e}");
			state.feed.set_error(e);
		}
	}

	/// Hand one camera buffer to the feed, converted to something the
	/// compositor draws.
	fn take(
		state: &mut State,
		format: VideoFormat,
		width: u32,
		height: u32,
		stride: usize,
		bytes: &[u8],
		timestamp: Duration,
	) -> Result<()> {
		let (w, h) = (width as usize, height as usize);
		/// The buffer as a plane whose stride is at least one row.
		fn plane(bytes: &[u8], stride: usize, row: usize) -> PlaneRef<'_> {
			PlaneRef::new(bytes, if stride >= row { stride } else { row })
		}
		let pixels = match format {
			VideoFormat::BGRx | VideoFormat::BGRA => PixelsRef::Bgra(plane(bytes, stride, w * 4)),
			VideoFormat::RGBx | VideoFormat::RGBA => PixelsRef::Rgba(plane(bytes, stride, w * 4)),
			VideoFormat::NV12 => {
				let y = plane(bytes, stride, w);
				let split = y.stride * h;
				PixelsRef::Nv12 {
					y,
					uv: plane(bytes.get(split..).unwrap_or_default(), stride, w.div_ceil(2) * 2),
				}
			}
			VideoFormat::I420 => {
				let y = plane(bytes, stride, w);
				let (cw, ch) = crate::frame::chroma_size(width, height);
				let split = y.stride * h;
				let rest = bytes.get(split..).unwrap_or_default();
				PixelsRef::I420 {
					y,
					u: PlaneRef::new(rest, cw),
					v: PlaneRef::new(rest.get(cw * ch..).unwrap_or_default(), cw),
				}
			}
			// 4:2:2 and 24-bit formats: unpacked into the pool's RGBA frame.
			other => return packed(state, other, width, height, stride, bytes, timestamp),
		};
		let frame = FrameRef { width, height, timestamp, pixels };
		deliver(&mut state.pool, &state.feed, &frame, state.filter.as_mut())
	}

	/// YUY2 / UYVY / RGB / BGR straight into a pooled RGBA frame.
	fn packed(
		state: &mut State,
		format: VideoFormat,
		width: u32,
		height: u32,
		stride: usize,
		bytes: &[u8],
		timestamp: Duration,
	) -> Result<()> {
		use crate::frame::{FrameData, PixelFormat};
		let (w, h) = (width as usize, height as usize);
		let row = match format {
			VideoFormat::YUY2 | VideoFormat::UYVY => w * 2,
			VideoFormat::RGB | VideoFormat::BGR => w * 3,
			other => {
				return Err(Error::Convert(format!("camera format {other:?} is not supported")));
			}
		};
		let stride = if stride >= row { stride } else { row };
		if bytes.len() < stride * (h - 1) + row {
			return Err(Error::InvalidFrame(format!(
				"camera buffer of {} bytes is too small for {width}x{height}",
				bytes.len()
			)));
		}
		let slot = state.pool.get_format(width, height, PixelFormat::Rgba);
		let target = Arc::get_mut(slot).expect("the pool hands out unshared frames");
		target.timestamp = timestamp;
		let FrameData::Rgba(out) = &mut target.data else {
			return Err(Error::InvalidFrame("the camera pool is not RGBA".into()));
		};
		match format {
			VideoFormat::YUY2 | VideoFormat::UYVY => {
				let image = yuv::YuvPackedImage {
					yuy: bytes,
					yuy_stride: u32::try_from(stride)
						.map_err(|_| Error::InvalidFrame("camera stride too large".into()))?,
					width,
					height,
				};
				let convert = if format == VideoFormat::YUY2 {
					yuv::yuyv422_to_rgba
				} else {
					yuv::uyvy422_to_rgba
				};
				convert(
					&image,
					&mut out.data,
					out.stride as u32,
					yuv::YuvRange::Limited,
					yuv::YuvStandardMatrix::Bt601,
				)
				.map_err(|e| Error::Convert(e.to_string()))?;
			}
			_ => {
				let bgr = format == VideoFormat::BGR;
				for y in 0..h {
					let src = &bytes[y * stride..][..row];
					let dst = &mut out.data[y * out.stride..][..w * 4];
					for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(3)) {
						d.copy_from_slice(&if bgr {
							[s[2], s[1], s[0], 255]
						} else {
							[s[0], s[1], s[2], 255]
						});
					}
				}
			}
		}
		if let Some(filter) = state.filter.as_mut() {
			filter.apply(target)?;
		}
		state.feed.put(slot.clone());
		Ok(())
	}
}

/// MJPEG pictures decoded on a thread of their own, the newest first: the
/// camera's thread copies each picture into a recycled buffer and hands it
/// over latest-wins, so a decode that falls behind skips pictures instead
/// of holding up the camera. Each picture is decoded by `zune-jpeg`
/// straight into a pooled RGBA frame (its SIMD paths do the colour
/// conversion on the way), and the background filter, if any, runs there
/// too.
///
/// ponytail: one decoder thread. A camera-like 1440p picture (400 KB) takes
/// about 30 ms here, so 1440p at 30 fps is about all one thread does;
/// decode alternate pictures on a second thread (dropping any that come out
/// late) if larger MJPEG cameras need it.
#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod mjpeg {
	use std::sync::Arc;
	use std::thread::JoinHandle;
	use std::time::Duration;

	use tracing::debug;
	use zune_core::bytestream::ZCursor;
	use zune_core::colorspace::ColorSpace;
	use zune_core::options::DecoderOptions;
	use zune_jpeg::JpegDecoder;

	use crate::frame::{FrameData, PixelFormat};
	use crate::handoff::Handoff;
	use crate::pool::FramePool;
	use crate::studio::compose::Feed;
	use crate::studio::segment::BackgroundFilter;
	use crate::{Error, Result};

	/// Pictures in a row that do not decode before the source reports an
	/// error: a camera sends a broken picture now and then.
	const FAILURES: u32 = 30;
	/// How often the decoder checks whether to stop.
	const POLL: Duration = Duration::from_millis(100);

	/// One compressed picture.
	pub struct Jpeg {
		pub data: Vec<u8>,
		pub timestamp: Duration,
	}

	/// The decoder thread; stops when dropped.
	pub struct Decoder {
		input: Arc<Handoff<Jpeg>>,
		/// Buffers handed over, reused once the decoder let go of them.
		spare: Vec<Arc<Jpeg>>,
		thread: Option<JoinHandle<()>>,
	}

	impl Decoder {
		/// Decode into `feed`, through `filter` if given.
		pub fn start(feed: Arc<Feed>, filter: Option<BackgroundFilter>) -> Result<Self> {
			let input = Arc::new(Handoff::new());
			let thread = std::thread::Builder::new()
				.name("voelin-mjpeg".into())
				.spawn({
					let input = input.clone();
					move || run(&input, &feed, filter)
				})
				.map_err(Error::Io)?;
			Ok(Self { input, spare: Vec::new(), thread: Some(thread) })
		}

		/// Hand over a picture (copied); one the decoder has not taken yet
		/// is replaced.
		pub fn put(&mut self, data: &[u8], timestamp: Duration) {
			let free = self.spare.iter().position(|j| Arc::strong_count(j) == 1);
			let slot = match free {
				Some(i) => &mut self.spare[i],
				None => {
					self.spare.push(Arc::new(Jpeg { data: Vec::new(), timestamp }));
					self.spare.last_mut().expect("just pushed")
				}
			};
			let jpeg = Arc::get_mut(slot).expect("nobody else holds a spare buffer");
			jpeg.data.clear();
			jpeg.data.extend_from_slice(data);
			jpeg.timestamp = timestamp;
			self.input.put(slot.clone());
		}
	}

	impl Drop for Decoder {
		fn drop(&mut self) {
			self.input.close();
			if let Some(thread) = self.thread.take() {
				let _ = thread.join();
			}
		}
	}

	fn run(input: &Handoff<Jpeg>, feed: &Feed, mut filter: Option<BackgroundFilter>) {
		let mut pool = FramePool::new();
		let mut failures = 0;
		while !input.is_closed() && !feed.is_closed() {
			let Some(jpeg) = input.wait_timeout(POLL) else { continue };
			match decode(&jpeg, &mut pool, feed, filter.as_mut()) {
				Ok(()) => failures = 0,
				Err(e) => {
					failures += 1;
					debug!("an MJPEG picture did not decode: {e}");
					if failures == FAILURES {
						feed.set_error(format!("the camera's MJPEG does not decode: {e}"));
					}
				}
			}
		}
	}

	/// Decode `jpeg` into a pooled RGBA frame and hand it to `feed`.
	pub fn decode(
		jpeg: &Jpeg,
		pool: &mut FramePool,
		feed: &Feed,
		filter: Option<&mut BackgroundFilter>,
	) -> Result<()> {
		let convert = |e: zune_jpeg::errors::DecodeErrors| Error::Convert(format!("MJPEG: {e}"));
		let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
		let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&jpeg.data[..]), options);
		decoder.decode_headers().map_err(convert)?;
		let (w, h) = decoder
			.dimensions()
			.ok_or_else(|| Error::Convert("MJPEG: a picture without a size".into()))?;
		let (width, height) = (
			u32::try_from(w).map_err(|_| Error::Convert("MJPEG: too wide".into()))?,
			u32::try_from(h).map_err(|_| Error::Convert("MJPEG: too high".into()))?,
		);
		let slot = pool.get_format(width, height, PixelFormat::Rgba);
		let target = Arc::get_mut(slot).expect("the pool hands out unshared frames");
		target.timestamp = jpeg.timestamp;
		let FrameData::Rgba(plane) = &mut target.data else {
			return Err(Error::InvalidFrame("the camera pool is not RGBA".into()));
		};
		// Pooled planes are tightly packed: exactly what the decoder writes.
		decoder.decode_into(&mut plane.data).map_err(convert)?;
		if let Some(filter) = filter {
			filter.apply(target)?;
		}
		feed.put(slot.clone());
		Ok(())
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		/// A JPEG of `width` x `height`: red on the left half, blue on the
		/// right (made with the `image` crate's encoder).
		fn picture(width: u32, height: u32) -> Vec<u8> {
			let image = image::RgbImage::from_fn(width, height, |x, _| {
				if x < width / 2 { image::Rgb([220, 30, 30]) } else { image::Rgb([30, 30, 220]) }
			});
			let mut out = std::io::Cursor::new(Vec::new());
			image.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
			out.into_inner()
		}

		#[test]
		fn pictures_decode_into_pooled_frames() {
			let feed = Feed::new();
			let mut pool = FramePool::new();
			for n in 0..3u64 {
				let jpeg = Jpeg { data: picture(64, 36), timestamp: Duration::from_millis(n * 33) };
				decode(&jpeg, &mut pool, &feed, None).unwrap();
				let frame = feed.take().expect("a frame");
				assert_eq!((frame.width, frame.height), (64, 36));
				assert_eq!(frame.timestamp, Duration::from_millis(n * 33));
				let FrameData::Rgba(plane) = &frame.data else { panic!("not RGBA") };
				let pixel = |x: usize| &plane.data[(18 * 64 + x) * 4..][..4];
				let near = |p: &[u8], c: [u8; 3]| p.iter().zip(c).all(|(a, b)| a.abs_diff(b) < 24);
				assert!(near(pixel(8), [220, 30, 30]), "{:?}", pixel(8));
				assert!(near(pixel(56), [30, 30, 220]), "{:?}", pixel(56));
				assert_eq!(pixel(8)[3], 255);
			}
			// The same frame came back each time.
			assert_eq!(pool.allocated(), 1);
			let broken = Jpeg { data: vec![0xFF, 0xD8, 1, 2, 3], timestamp: Duration::ZERO };
			assert!(decode(&broken, &mut pool, &feed, None).is_err());
		}

		#[test]
		fn the_thread_takes_the_newest_picture() {
			let feed = Arc::new(Feed::new());
			let mut decoder = Decoder::start(feed.clone(), None).unwrap();
			let jpeg = picture(32, 18);
			for n in 0..5u64 {
				decoder.put(&jpeg, Duration::from_millis(n));
			}
			let started = std::time::Instant::now();
			while feed.delivered() == 0 && started.elapsed() < Duration::from_secs(5) {
				std::thread::sleep(Duration::from_millis(5));
			}
			assert!(feed.delivered() >= 1);
			assert_eq!(feed.size(), (32, 18));
			// Buffers are reused, not one per picture.
			assert!(decoder.spare.len() <= 3, "{}", decoder.spare.len());
			drop(decoder);
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	#[test]
	fn the_test_pattern_camera_is_always_listed() {
		let cameras = list();
		let synthetic = cameras.last().expect("at least the test pattern");
		assert_eq!(synthetic.id, SYNTHETIC);
		assert_eq!(synthetic.backend, "synthetic");
		assert!(!synthetic.formats.is_empty());
		// Real cameras, if any, have a name and at least one usable format.
		for camera in cameras.iter().filter(|c| c.backend != "synthetic") {
			assert!(!camera.id.is_empty() && !camera.name.is_empty(), "{camera:?}");
			assert!(!camera.formats.is_empty(), "{camera:?}");
			for format in &camera.formats {
				assert!(format.sizes.iter().all(|&(w, h)| w > 0 && h > 0), "{format:?}");
			}
			println!(
				"camera {} ({}): {}",
				camera.id,
				camera.name,
				camera
					.formats
					.iter()
					.map(|f| format!(
						"{} {:?} up to {} fps",
						f.pixel.label(),
						f.sizes.first(),
						f.max_fps
					))
					.collect::<Vec<_>>()
					.join(", ")
			);
		}
	}

	#[test]
	fn four_character_codes() {
		assert_eq!(Pixel::from_fourcc(u32::from_le_bytes(*b"YUYV")), Some(Pixel::Yuyv));
		assert_eq!(Pixel::from_fourcc(u32::from_le_bytes(*b"MJPG")), Some(Pixel::Mjpeg));
		assert_eq!(Pixel::from_fourcc(u32::from_le_bytes(*b"XR24")), Some(Pixel::Bgrx));
		assert_eq!(Pixel::from_fourcc(u32::from_le_bytes(*b"ZZZZ")), None);
		assert_eq!(Pixel::Nv12.label(), "NV12");
	}

	/// The first real camera, through PipeWire. Needs a camera and a
	/// PipeWire daemon, so it is not part of the normal run:
	/// `cargo test -p voelin-media --lib real_camera -- --ignored --nocapture`.
	#[tokio::test]
	#[ignore = "needs a camera and PipeWire"]
	async fn a_real_camera_delivers() {
		let Some(camera) = list().into_iter().find(|c| c.backend != "synthetic") else {
			panic!("no camera on this machine");
		};
		let feed = Arc::new(Feed::new());
		let capture =
			Capture::start(&camera.id, None, 15, feed.clone(), Background::Keep).await.unwrap();
		let started = std::time::Instant::now();
		while feed.delivered() < 3 && started.elapsed() < Duration::from_secs(20) {
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
		println!(
			"{} through {}: {} frames at {:?}, error {:?}",
			camera.name,
			capture.backend(),
			feed.delivered(),
			feed.size(),
			feed.error()
		);
		assert!(feed.delivered() >= 3, "only {} frames", feed.delivered());
		let frame = feed.take().expect("a frame");
		assert!(frame.width > 0 && frame.height > 0);
		frame.validate().unwrap();
	}

	/// The first real camera at its largest MJPEG size (decoded on our
	/// thread), for three seconds: prints the size and the rate that
	/// arrived. Needs a camera with MJPEG and PipeWire:
	/// `cargo test -p voelin-media --lib largest_mjpeg -- --ignored --nocapture`.
	#[tokio::test]
	#[ignore = "needs a camera with MJPEG and PipeWire"]
	async fn a_real_camera_delivers_its_largest_mjpeg_size() {
		let Some((camera, (w, h), fps)) = list().into_iter().find_map(|c| {
			let mjpeg = c.formats.iter().find(|f| f.pixel == Pixel::Mjpeg)?;
			// `VOELIN_CAMERA_SIZE=1920x1080` picks another of its sizes.
			let wanted = std::env::var("VOELIN_CAMERA_SIZE").ok().and_then(|s| {
				let (w, h) = s.split_once('x')?;
				Some((w.parse().ok()?, h.parse().ok()?))
			});
			let size = wanted.unwrap_or(*mjpeg.sizes.first()?);
			Some((c.clone(), size, mjpeg.max_fps.max(1)))
		}) else {
			panic!("no camera with MJPEG on this machine");
		};
		let feed = Arc::new(Feed::new());
		let capture = Capture::start(&camera.id, Some((w, h)), fps, feed.clone(), Background::Keep)
			.await
			.unwrap();
		let started = std::time::Instant::now();
		while feed.delivered() < 2 && started.elapsed() < Duration::from_secs(20) {
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
		let (first, at) = (feed.delivered(), std::time::Instant::now());
		tokio::time::sleep(Duration::from_secs(3)).await;
		let rate = (feed.delivered() - first) as f64 / at.elapsed().as_secs_f64();
		println!(
			"{} through {}: asked {w}x{h} at {fps} fps, got {:?} at {rate:.1} fps, error {:?}",
			camera.name,
			capture.backend(),
			feed.size(),
			feed.error()
		);
		drop(capture);
		assert_eq!(feed.size(), (w, h));
		// Not `fps`: in low light a camera's auto exposure lowers its rate
		// (the Fifine K420 gives 10 fps at 1440p then, also to `v4l2-ctl`).
		assert!(rate >= 1.0, "{rate:.1} fps");
	}

	#[tokio::test]
	async fn the_synthetic_camera_delivers() {
		let feed = Arc::new(Feed::new());
		let capture = Capture::start(SYNTHETIC, Some((64, 48)), 30, feed.clone(), Background::Keep)
			.await
			.unwrap();
		assert_eq!(capture.backend(), "synthetic");
		assert_eq!(capture.device(), SYNTHETIC);
		let started = std::time::Instant::now();
		while feed.delivered() == 0 && started.elapsed() < Duration::from_secs(5) {
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
		assert!(feed.delivered() > 0);
		assert_eq!(feed.size(), (64, 48));
		assert_eq!(feed.error(), None);
	}
}
