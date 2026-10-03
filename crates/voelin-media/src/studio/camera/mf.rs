//! Cameras through Media Foundation (Windows): listed with
//! `MFEnumDeviceSources` (the friendly name, every native format with its
//! sizes), captured with an `IMFSourceReader` on a thread of its own. The
//! reader sets the camera to the native format nearest the wanted size and
//! hands out NV12, decoding MJPEG and converting the rest itself (advanced
//! video processing), so every camera reaches the compositor the same way.
//!
//! Untested: type-checked for `x86_64-pc-windows-gnu` only.
#![allow(unsafe_code)]

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use tracing::{debug, warn};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::{
	IMF2DBuffer, IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSourceReader,
	MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
	MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
	MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
	MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
	MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
	MF_VERSION, MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromMediaSource,
	MFEnumDeviceSources, MFMediaType_Video, MFSTARTUP_NOSOCKET, MFShutdown, MFStartup,
	MFVideoFormat_I420, MFVideoFormat_IYUV, MFVideoFormat_MJPG, MFVideoFormat_NV12,
	MFVideoFormat_RGB24, MFVideoFormat_RGB32, MFVideoFormat_UYVY, MFVideoFormat_YUY2,
};
use windows::Win32::System::Com::{
	COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::{GUID, Interface, PWSTR};

use super::{BACKEND, Camera, Format, Pixel};
use crate::capture::FramePacer;
use crate::frame::{FrameRef, PixelsRef, PlaneRef};
use crate::pool::FramePool;
use crate::studio::compose::Feed;
use crate::studio::scene::Background;
use crate::studio::segment::BackgroundFilter;
use crate::studio::source::deliver;
use crate::{Error, Result};

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// COM and Media Foundation on the calling thread, until dropped there (it
/// is not `Send`). Every user makes it on a thread of its own.
struct Mf(PhantomData<*const ()>);

impl Mf {
	fn start() -> windows::core::Result<Self> {
		// SAFETY: initialisation for this thread, undone by `Drop` on it.
		unsafe {
			CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
			if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) {
				CoUninitialize();
				return Err(e);
			}
		}
		Ok(Self(PhantomData))
	}
}

impl Drop for Mf {
	fn drop(&mut self) {
		// SAFETY: paired with the calls in `start`, on the same thread; the
		// users release their COM objects before.
		unsafe {
			let _ = MFShutdown();
			CoUninitialize();
		}
	}
}

fn capture_error(e: impl std::fmt::Display) -> Error {
	Error::Capture { backend: BACKEND, message: e.to_string() }
}

/// Attributes with room for one item.
fn attributes() -> windows::core::Result<IMFAttributes> {
	let mut attributes = None;
	// SAFETY: the call fills `attributes`.
	unsafe { MFCreateAttributes(&mut attributes, 1)? };
	attributes.ok_or_else(windows::core::Error::empty)
}

/// The video capture devices.
fn devices() -> windows::core::Result<Vec<IMFActivate>> {
	let attributes = attributes()?;
	let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
	let mut count = 0u32;
	// SAFETY: `MFEnumDeviceSources` hands back an array of `count` interface
	// pointers allocated with CoTaskMemAlloc; they are moved out (`take`)
	// before the array is freed.
	unsafe {
		attributes.SetGUID(
			&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
			&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
		)?;
		MFEnumDeviceSources(&attributes, &mut list, &mut count)?;
		if list.is_null() {
			return Ok(Vec::new());
		}
		let devices = (0..count as usize).filter_map(|i| (*list.add(i)).take()).collect();
		CoTaskMemFree(Some(list.cast_const().cast()));
		Ok(devices)
	}
}

/// A string attribute of a device.
fn string(device: &IMFActivate, key: &GUID) -> windows::core::Result<String> {
	let mut value = PWSTR::null();
	let mut len = 0u32;
	// SAFETY: the string is allocated by the call, copied, then freed.
	unsafe {
		device.GetAllocatedString(key, &mut value, &mut len)?;
		let text = value.to_string().unwrap_or_default();
		CoTaskMemFree(Some(value.0.cast_const().cast()));
		Ok(text)
	}
}

/// The pixel format of a Media Foundation subtype.
fn pixel_of(subtype: GUID) -> Option<Pixel> {
	Some(match subtype {
		s if s == MFVideoFormat_NV12 => Pixel::Nv12,
		s if s == MFVideoFormat_YUY2 => Pixel::Yuyv,
		s if s == MFVideoFormat_UYVY => Pixel::Uyvy,
		s if s == MFVideoFormat_I420 || s == MFVideoFormat_IYUV => Pixel::I420,
		s if s == MFVideoFormat_RGB32 => Pixel::Bgrx,
		s if s == MFVideoFormat_RGB24 => Pixel::Bgr24,
		s if s == MFVideoFormat_MJPG => Pixel::Mjpeg,
		_ => return None,
	})
}

/// A `UINT64` attribute holding two `u32`s (frame size, frame rate).
fn pair(media: &IMFMediaType, key: &GUID) -> Option<(u32, u32)> {
	// SAFETY: an attribute read.
	let value = unsafe { media.GetUINT64(key) }.ok()?;
	Some(((value >> 32) as u32, value as u32))
}

/// Frames per second of a media type, 0 if it does not say.
fn rate(media: &IMFMediaType) -> u32 {
	pair(media, &MF_MT_FRAME_RATE).map_or(0, |(n, d)| n / d.max(1))
}

/// The native media types of a camera.
fn native_types(reader: &IMFSourceReader) -> Vec<IMFMediaType> {
	// SAFETY: `GetNativeMediaType` fails past the last type.
	(0..).map_while(|i| unsafe { reader.GetNativeMediaType(STREAM, i) }.ok()).collect()
}

/// Open a camera: its media source and a source reader for it.
fn open(
	device: &IMFActivate,
	attributes: Option<&IMFAttributes>,
) -> windows::core::Result<(IMFMediaSource, IMFSourceReader)> {
	// SAFETY: COM calls on interfaces we hold.
	unsafe {
		let source: IMFMediaSource = device.ActivateObject()?;
		let reader = MFCreateSourceReaderFromMediaSource(&source, attributes)?;
		Ok((source, reader))
	}
}

/// The formats of a camera: per pixel format its sizes, largest first, and
/// the highest rate of the largest.
fn formats(device: &IMFActivate) -> windows::core::Result<Vec<Format>> {
	let (source, reader) = open(device, None)?;
	// With the area of the largest size so far.
	let mut formats: Vec<(Format, u32)> = Vec::new();
	for media in native_types(&reader) {
		// SAFETY: an attribute read.
		let subtype = unsafe { media.GetGUID(&MF_MT_SUBTYPE) };
		let Some(pixel) = subtype.ok().and_then(pixel_of) else { continue };
		let Some(size) = pair(&media, &MF_MT_FRAME_SIZE) else { continue };
		let at = match formats.iter().position(|(f, _)| f.pixel == pixel) {
			Some(at) => at,
			None => {
				formats.push((Format { pixel, sizes: Vec::new(), max_fps: 0 }, 0));
				formats.len() - 1
			}
		};
		let (format, largest) = &mut formats[at];
		if !format.sizes.contains(&size) {
			format.sizes.push(size);
		}
		let area = size.0 * size.1;
		if area > *largest {
			(*largest, format.max_fps) = (area, rate(&media));
		} else if area == *largest {
			format.max_fps = format.max_fps.max(rate(&media));
		}
	}
	drop(reader);
	// SAFETY: the source is ours.
	unsafe {
		let _ = source.Shutdown();
	}
	Ok(formats
		.into_iter()
		.map(|(mut f, _)| {
			f.sizes.sort_unstable_by_key(|&(w, h)| std::cmp::Reverse(u64::from(w) * u64::from(h)));
			f
		})
		.collect())
}

/// Every camera, by symbolic link. Never fails: problems leave cameras out.
pub fn list() -> Vec<Camera> {
	// A thread of its own: the caller's COM apartment does not matter.
	let listed = std::thread::spawn(|| -> windows::core::Result<Vec<Camera>> {
		let _mf = Mf::start()?;
		let mut cameras = Vec::new();
		for device in devices()? {
			let Ok(id) = string(&device, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
			else {
				continue;
			};
			let name = string(&device, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
				.ok()
				.filter(|n| !n.is_empty())
				.unwrap_or_else(|| id.clone());
			let formats = formats(&device).unwrap_or_else(|e| {
				debug!(camera = %name, "cannot read the camera's formats: {e}");
				Vec::new()
			});
			// Webcams face the user.
			cameras.push(Camera { id, name, backend: "mediafoundation", formats, mirrored: true });
		}
		Ok(cameras)
	})
	.join();
	match listed {
		Ok(Ok(cameras)) => cameras,
		Ok(Err(e)) => {
			debug!("Media Foundation lists no cameras: {e}");
			Vec::new()
		}
		Err(_) => Vec::new(),
	}
}

/// A running camera; stops when dropped.
pub struct Stream {
	stop: Arc<AtomicBool>,
	thread: Option<JoinHandle<()>>,
}

impl Stream {
	/// Open the camera with symbolic link `device` and feed `feed` from a
	/// thread of its own; returns once the camera is set up.
	pub fn start(
		device: &str,
		size: Option<(u32, u32)>,
		fps: u32,
		feed: Arc<Feed>,
		background: Background,
	) -> Result<Self> {
		let stop = Arc::new(AtomicBool::new(false));
		let (ready_tx, ready_rx) = std::sync::mpsc::channel();
		let device = device.to_owned();
		let thread = std::thread::Builder::new()
			.name("voelin-camera".into())
			.spawn({
				let stop = stop.clone();
				move || {
					let mf = match Mf::start() {
						Ok(mf) => mf,
						Err(e) => {
							let _ = ready_tx.send(Err(capture_error(e)));
							return;
						}
					};
					match Reader::open(&device, size, fps) {
						Ok(reader) => {
							let _ = ready_tx.send(Ok(()));
							if let Err(e) = reader.run(&stop, &feed, background) {
								warn!("camera {device}: {e}");
								feed.set_error(e);
							}
							// Before Media Foundation shuts down.
							drop(reader);
						}
						Err(e) => {
							let _ = ready_tx.send(Err(capture_error(e)));
						}
					}
					drop(mf);
				}
			})
			.map_err(Error::Io)?;
		let stream = Self { stop, thread: Some(thread) };
		ready_rx.recv().unwrap_or_else(|_| Err(capture_error("the camera thread ended")))?;
		Ok(stream)
	}
}

impl Drop for Stream {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
		// The reader looks at `stop` after every frame.
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

/// An open camera with NV12 output at `width` x `height`.
struct Reader {
	source: IMFMediaSource,
	reader: IMFSourceReader,
	width: u32,
	height: u32,
	fps: u32,
}

impl Reader {
	fn open(device: &str, size: Option<(u32, u32)>, fps: u32) -> windows::core::Result<Self> {
		let not_found = || windows::core::Error::new(E_FAIL, "no such camera");
		let camera = devices()?
			.into_iter()
			.find(|d| {
				string(d, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
					.is_ok_and(|id| id == device)
			})
			.ok_or_else(not_found)?;
		let attributes = attributes()?;
		// SAFETY: an attribute write.
		unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)? };
		let (source, reader) = open(&camera, Some(&attributes))?;
		// The native type nearest the wanted size (default 720p), the
		// fastest of those up to `fps`, and NV12 made of it.
		let (w, h) = size.unwrap_or((1280, 720));
		let wanted = i64::from(w) * i64::from(h);
		let fps = fps.max(1);
		let (media, (width, height)) = native_types(&reader)
			.into_iter()
			.filter_map(|media| {
				let size = pair(&media, &MF_MT_FRAME_SIZE)?;
				Some((media, size))
			})
			.min_by_key(|(media, (width, height))| {
				let distance = (i64::from(*width) * i64::from(*height) - wanted).abs();
				(distance, std::cmp::Reverse(rate(media).min(fps)))
			})
			.ok_or_else(not_found)?;
		// SAFETY: COM calls on interfaces we hold.
		unsafe {
			reader.SetCurrentMediaType(STREAM, None, &media)?;
			let out = MFCreateMediaType()?;
			out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
			out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
			out.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(width) << 32) | u64::from(height))?;
			if let Some((n, d)) = pair(&media, &MF_MT_FRAME_RATE) {
				out.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(n) << 32) | u64::from(d))?;
			}
			reader.SetCurrentMediaType(STREAM, None, &out)?;
		}
		debug!(device, width, height, rate = rate(&media), "camera through Media Foundation");
		Ok(Self { source, reader, width, height, fps })
	}

	/// Read frames until `stop` or the feed closes.
	fn run(
		&self,
		stop: &AtomicBool,
		feed: &Feed,
		background: Background,
	) -> std::result::Result<(), String> {
		let mut filter =
			background.needs_mask().then(|| BackgroundFilter::with_default_segmenter(background));
		let mut pool = FramePool::new();
		let mut pacer = FramePacer::new(Some(self.fps));
		let started = Instant::now();
		let (width, height) = (self.width, self.height);
		let rows = height as usize + height.div_ceil(2) as usize;
		while !stop.load(Ordering::Relaxed) && !feed.is_closed() {
			let (mut flags, mut sample) = (0u32, None);
			// SAFETY: a synchronous read into locals.
			unsafe {
				self.reader.ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample))
			}
			.map_err(|e| format!("reading the camera failed: {e}"))?;
			if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 | MF_SOURCE_READERF_ERROR.0) as u32 != 0 {
				return Err("the camera stopped".into());
			}
			let Some(sample) = sample else { continue };
			let timestamp = started.elapsed();
			if !pacer.take(timestamp) {
				continue;
			}
			// SAFETY: the buffer is locked while `bytes` is used, and
			// unlocked after any successful lock.
			unsafe {
				let Ok(buffer) = sample.ConvertToContiguousBuffer() else { continue };
				let two_d = buffer.cast::<IMF2DBuffer>().ok();
				let (mut data, mut pitch, mut length) = (std::ptr::null_mut(), width as i32, 0u32);
				let locked = match &two_d {
					Some(b) => b.Lock2D(&mut data, &mut pitch),
					None => buffer.Lock(&mut data, None, Some(&mut length)),
				};
				if locked.is_err() {
					continue;
				}
				// A bottom-up buffer (negative pitch) is not NV12 as asked.
				if !data.is_null() && pitch > 0 {
					let stride = pitch as usize;
					let length = if two_d.is_some() { stride * rows } else { length as usize };
					let bytes = std::slice::from_raw_parts(data, length);
					let (y, uv) = bytes.split_at((stride * height as usize).min(bytes.len()));
					let pixels = PixelsRef::Nv12 {
						y: PlaneRef::new(y, stride),
						uv: PlaneRef::new(uv, stride),
					};
					let frame = FrameRef { width, height, timestamp, pixels };
					if let Err(e) = deliver(&mut pool, feed, &frame, filter.as_mut()) {
						debug!("a camera frame was not taken: {e}");
					}
				}
				let _ = match &two_d {
					Some(b) => b.Unlock2D(),
					None => buffer.Unlock(),
				};
			}
		}
		Ok(())
	}
}

impl Drop for Reader {
	fn drop(&mut self) {
		// SAFETY: the source is ours.
		unsafe {
			let _ = self.source.Shutdown();
		}
	}
}
