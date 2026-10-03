//! Wayland screen capture: the xdg-desktop-portal ScreenCast interface
//! (`ashpd`) picks the source and hands over a PipeWire remote; frames come
//! through a PipeWire video stream.
//!
//! The portal can remember the user's choice: pass the token from
//! [`PortalCapture::restore_token`] to [`PortalCapture::with_restore_token`]
//! next time (it persists until the user revokes it).
//!
//! Buffers: LINEAR DMA-BUFs (mapped and read by the CPU, with
//! `DMA_BUF_IOCTL_SYNC` around each read) are offered first, shared memory
//! (`BGRx` / `BGRA` / `RGBx` / `RGBA`) second, so compositors without
//! DMA-BUF support use shared memory. When reading DMA-BUFs turns out slow
//! (buffers in memory the CPU reads uncached, e.g. a discrete GPU's VRAM),
//! the stream switches to shared memory. A sink that
//! [accepts DMA-BUFs](FrameSink::accepts_dmabuf) (a VA-API encoder, which
//! converts and encodes them on the GPU) gets them before anything is
//! mapped, and then the CPU never reads them. Tiled DMA-BUFs are not
//! offered: the modifiers the encoder's driver imports are not known here.
//! Frames go to the [`FrameSink`] while the buffer is dequeued; nothing is
//! copied before conversion. The frame rate
//! follows the sink's cap: the compositor is asked for up to that rate and
//! sends frames when the screen changes.

use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use pipewire as pw;
use pw::spa::buffer::DataType;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::{
	ChoiceValue, Object, Pod, PodPropFlags, Property, PropertyFlags, Value, property,
};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};
use tracing::{debug, warn};

use crate::capture::dmabuf::{self, DmaBufMap};
use crate::capture::pw::{PwThread, pod, serialize};
use crate::capture::{
	BoxFuture, CaptureOptions, CaptureSource, DRM_MOD_LINEAR, DmaBufRef, FrameSink, QueueSink,
	ScreenCapture, SourceId, drm_fourcc,
};
use crate::frame::{FrameRef, PixelsRef, PlaneRef, VideoFrame};
use crate::queue::FrameReceiver;
use crate::{Error, Result};

const BACKEND: &str = "portal";

fn portal_error(e: ashpd::Error) -> Error {
	match e {
		ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)
		| ashpd::Error::Portal(ashpd::PortalError::Cancelled(_)) => Error::Cancelled,
		// No bus, no portal service, or no ScreenCast interface.
		ashpd::Error::Zbus(_)
		| ashpd::Error::Portal(ashpd::PortalError::ZBus(_))
		| ashpd::Error::PortalNotFound(_) => Error::CaptureUnavailable {
			backend: BACKEND,
			reason: format!("the ScreenCast portal is not available: {e}"),
		},
		other => Error::Capture { backend: BACKEND, message: other.to_string() },
	}
}

/// A portal session kept open while capturing.
struct Session {
	proxy: Screencast,
	session: ashpd::desktop::Session<Screencast>,
}

/// ScreenCast portal + PipeWire backend (Wayland, also works on X11 desktops
/// that run xdg-desktop-portal).
pub struct PortalCapture {
	restore_token: Option<String>,
	dmabuf: bool,
	session: Option<Session>,
	thread: Option<PwThread>,
}

impl Default for PortalCapture {
	fn default() -> Self {
		// VOELIN_PORTAL_DMABUF=0 offers shared memory only.
		let dmabuf = std::env::var("VOELIN_PORTAL_DMABUF").map_or(true, |v| v != "0");
		Self { restore_token: None, dmabuf, session: None, thread: None }
	}
}

impl PortalCapture {
	pub fn new() -> Self {
		Self::default()
	}

	/// Reuse an earlier choice without asking (a token from
	/// [`PortalCapture::restore_token`]).
	pub fn with_restore_token(token: Option<String>) -> Self {
		let mut capture = Self::default();
		capture.restore_token = token;
		capture
	}

	/// Offer LINEAR DMA-BUFs (default, unless `VOELIN_PORTAL_DMABUF=0`), or
	/// shared memory only.
	pub fn with_dmabuf(mut self, enabled: bool) -> Self {
		self.dmabuf = enabled;
		self
	}

	/// Token to persist after a successful start; `None` before that or if
	/// the portal did not grant persistence.
	pub fn restore_token(&self) -> Option<&str> {
		self.restore_token.as_deref()
	}

	/// Whether the ScreenCast portal answers on the session bus.
	pub async fn is_available() -> bool {
		match Screencast::new().await {
			// Reading a property needs the service to exist.
			Ok(proxy) => proxy.available_source_types().await.is_ok(),
			Err(_) => false,
		}
	}

	async fn open(&mut self, options: &CaptureOptions) -> Result<(std::os::fd::OwnedFd, u32)> {
		let proxy = Screencast::new().await.map_err(portal_error)?;
		let session = proxy.create_session(Default::default()).await.map_err(portal_error)?;
		let modes = proxy.available_cursor_modes().await.unwrap_or_default();
		let cursor = if !options.cursor {
			CursorMode::Hidden
		} else if modes.contains(CursorMode::Embedded) {
			CursorMode::Embedded
		} else {
			// Metadata cursors would need drawing; fall back to none.
			CursorMode::Hidden
		};
		let select = SelectSourcesOptions::default()
			.set_cursor_mode(cursor)
			.set_sources(SourceType::Monitor | SourceType::Window)
			.set_multiple(false)
			.set_restore_token(self.restore_token.as_deref())
			.set_persist_mode(PersistMode::ExplicitlyRevoked);
		proxy
			.select_sources(&session, select)
			.await
			.map_err(portal_error)?
			.response()
			.map_err(portal_error)?;
		let streams = proxy
			.start(&session, None, Default::default())
			.await
			.map_err(portal_error)?
			.response()
			.map_err(portal_error)?;
		if let Some(token) = streams.restore_token() {
			self.restore_token = Some(token.to_owned());
		}
		let stream = streams.streams().first().ok_or(Error::Cancelled)?;
		let node = stream.pipe_wire_node_id();
		debug!(node, size = ?stream.size(), "portal stream started");
		let fd = proxy
			.open_pipe_wire_remote(&session, Default::default())
			.await
			.map_err(portal_error)?;
		self.session = Some(Session { proxy, session });
		Ok((fd, node))
	}
}

impl ScreenCapture for PortalCapture {
	fn backend(&self) -> &'static str {
		BACKEND
	}

	fn sources(&mut self) -> Result<Vec<CaptureSource>> {
		Ok(vec![CaptureSource {
			id: SourceId::Portal,
			name: "Choose a screen or window".into(),
			width: 0,
			height: 0,
			primary: true,
		}])
	}

	/// Asks the user through the portal dialog (unless a restore token
	/// applies). Needs a tokio runtime (the D-Bus connection runs on it).
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

	/// The sink reads the PipeWire buffer itself (shared memory, or a
	/// LINEAR DMA-BUF mapped for reading) while the buffer is dequeued.
	fn start_sink(
		&mut self,
		source: &SourceId,
		options: &CaptureOptions,
		sink: Box<dyn FrameSink>,
	) -> BoxFuture<'_, Result<()>> {
		let source = source.clone();
		let options = options.clone();
		Box::pin(async move {
			if source != SourceId::Portal {
				return Err(Error::SourceNotFound(source));
			}
			self.stop();
			let (fd, node) = self.open(&options).await?;
			let dmabuf = self.dmabuf;
			let thread =
				PwThread::spawn("voelin-portal-capture", Some(fd), move |core, mainloop| {
					video_stream(core, mainloop, node, dmabuf, sink)
				})
				.map_err(|e| Error::Capture { backend: BACKEND, message: e })?;
			self.thread = Some(thread);
			Ok(())
		})
	}

	fn stop(&mut self) {
		self.thread = None;
		if let Some(s) = self.session.take() {
			// Closing needs the async runtime; do it in the background if there
			// is one, else the portal closes the session with the connection.
			if let Ok(handle) = tokio::runtime::Handle::try_current() {
				handle.spawn(async move {
					let _ = s.session.close().await;
					drop(s.proxy);
				});
			}
		}
	}
}

impl Drop for PortalCapture {
	fn drop(&mut self) {
		self.stop();
	}
}

/// Pixel formats we take, in order of preference (all 4 bytes per pixel;
/// `x` and `A` are ignored).
const FORMATS: [VideoFormat; 4] =
	[VideoFormat::BGRx, VideoFormat::BGRA, VideoFormat::RGBx, VideoFormat::RGBA];
/// DRM_FORMAT_MOD_LINEAR: rows of pixels, readable by the CPU once mapped.
const MODIFIER_LINEAR: i64 = 0;
/// Reading DMA-BUFs slower than this (per pixel, conversion included) means
/// the buffers live in memory the CPU reads uncached (e.g. a discrete GPU's
/// VRAM); shared memory is faster then.
///
/// Measured on a Radeon RX 7900 GRE capturing a 2560x1440 desktop at 60 fps,
/// the same run through both paths: a LINEAR DMA-BUF took 11.95 ms per frame
/// (3.24 ns per pixel) and 19.5 CPU cores, shared memory 0.27 ms (0.07 ns
/// per pixel) and 0.41 cores — 44 times the time and 48 times the CPU. The
/// threshold was 6.0, which that does not reach, so the switch never
/// happened and a discrete GPU burned twenty cores on screen sharing. It
/// sits between the two figures, an order of magnitude above the fast path,
/// so a GPU whose buffers the CPU can read (anything with unified memory)
/// keeps them.
const SLOW_DMABUF_NS_PER_PIXEL: f64 = 1.0;
/// DMA-BUF frames timed before deciding.
const DMABUF_PROBE_FRAMES: u32 = 30;
/// DMA-BUFs that failed to map before shared memory is offered instead. A
/// few failures can be a buffer being replaced; more means this compositor's
/// buffers cannot be read this way at all.
const DMABUF_FAILURES_BEFORE_SHM: u32 = 5;

/// What was negotiated.
#[derive(Clone, Copy, Debug)]
struct Negotiated {
	format: VideoFormat,
	width: u32,
	height: u32,
	/// DMA-BUF modifier; `None`: shared memory.
	modifier: Option<i64>,
}

struct VideoState {
	sink: Box<dyn FrameSink>,
	format: Option<Negotiated>,
	started: Instant,
	/// Frame rate the offered formats ask for.
	fps: u32,
	/// Offer DMA-BUFs.
	dmabuf: bool,
	/// The sink's tiled modifiers the offer was made with (ahead of LINEAR).
	modifiers: Vec<u64>,
	/// Mappings of the DMA-BUFs PipeWire cycles through.
	maps: Vec<DmaBufMap>,
	/// Time spent handing DMA-BUF frames to the sink, and how many.
	dmabuf_time: Duration,
	dmabuf_frames: u32,
	/// DMA-BUFs that could not be mapped (see
	/// [`DMABUF_FAILURES_BEFORE_SHM`]).
	dmabuf_failures: u32,
	logged: Option<bool>,
	mainloop: pw::main_loop::MainLoopWeak,
}

fn video_stream(
	core: &pw::core::CoreRc,
	mainloop: &pw::main_loop::MainLoopRc,
	node: u32,
	dmabuf: bool,
	sink: Box<dyn FrameSink>,
) -> std::result::Result<(pw::stream::StreamRc, pw::stream::StreamListener<VideoState>), String> {
	let props = pw::properties::properties! {
		*pw::keys::MEDIA_TYPE => "Video",
		*pw::keys::MEDIA_CATEGORY => "Capture",
		*pw::keys::MEDIA_ROLE => "Screen",
	};
	let stream = pw::stream::StreamRc::new(core.clone(), "voelin-screen-capture", props)
		.map_err(|e| format!("PipeWire stream: {e}"))?;
	let fps = sink.max_fps().max(1);
	let modifiers = sink.dmabuf_modifiers().to_vec();
	let formats = enum_formats(fps, dmabuf, &modifiers)?;
	let state = VideoState {
		sink,
		format: None,
		started: Instant::now(),
		fps,
		dmabuf,
		modifiers,
		maps: Vec::new(),
		dmabuf_time: Duration::ZERO,
		dmabuf_frames: 0,
		dmabuf_failures: 0,
		logged: None,
		mainloop: mainloop.downgrade(),
	};
	let listener = stream
		.add_local_listener_with_user_data(state)
		.state_changed(|_, state, _, new| {
			if let pw::stream::StreamState::Error(e) = &new {
				warn!("screen capture stream failed: {e}");
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
			if let Err(e) = format_changed(stream, state, param) {
				warn!("screen capture format: {e}");
			}
		})
		.remove_buffer(|_, state, _| {
			// PipeWire is replacing its buffers: map the new ones.
			state.maps.clear();
		})
		.process(process)
		.register()
		.map_err(|e| format!("PipeWire listener: {e}"))?;

	let mut pods = formats.iter().map(|f| pod(f)).collect::<std::result::Result<Vec<_>, _>>()?;
	stream
		.connect(
			pw::spa::utils::Direction::Input,
			Some(node),
			pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
			&mut pods,
		)
		.map_err(|e| format!("cannot connect to the screen cast node {node}: {e}"))?;
	Ok((stream, listener))
}

fn size_range() -> Property {
	property!(
		FormatProperties::VideoSize,
		Choice,
		Range,
		Rectangle,
		Rectangle { width: 1920, height: 1080 },
		Rectangle { width: 1, height: 1 },
		Rectangle { width: 16384, height: 16384 }
	)
}

/// Up to `fps`, as the compositor can (it sends frames on damage only).
fn framerate_range(fps: u32) -> Property {
	property!(
		FormatProperties::VideoFramerate,
		Choice,
		Range,
		Fraction,
		Fraction { num: fps, denom: 1 },
		Fraction { num: 0, denom: 1 },
		Fraction { num: fps, denom: 1 }
	)
}

/// The modifiers we take, best first: the sink's tiled ones, then LINEAR,
/// which the CPU can always read.
fn our_modifiers(tiled: &[u64]) -> Vec<i64> {
	tiled.iter().map(|&m| m as i64).chain([MODIFIER_LINEAR]).collect()
}

/// The modifier property of a DMA-BUF format: a choice of `modifiers` the
/// producer must not fixate (we pick, see [`format_changed`]), or one fixed
/// modifier.
fn modifier_property(modifiers: &[i64], fixed: bool) -> Property {
	let flags = pw::spa::sys::SPA_POD_PROP_FLAG_MANDATORY
		| if fixed { 0 } else { pw::spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE };
	let value = if fixed {
		Value::Long(modifiers[0])
	} else {
		Value::Choice(ChoiceValue::Long(Choice(
			ChoiceFlags::empty(),
			ChoiceEnum::Enum { default: modifiers[0], alternatives: modifiers.to_vec() },
		)))
	};
	Property {
		key: FormatProperties::VideoModifier.as_raw(),
		flags: PropertyFlags::from_bits_retain(flags),
		value,
	}
}

/// The modifiers a producer's modifier property leaves to choose from.
fn offered_modifiers(prop: &pw::spa::pod::PodProp) -> Vec<i64> {
	use pw::spa::pod::deserialize::PodDeserializer;
	match PodDeserializer::deserialize_any_from(prop.value().as_bytes()).map(|(_, v)| v) {
		Ok(Value::Long(m)) => vec![m],
		Ok(Value::Choice(ChoiceValue::Long(Choice(
			_,
			ChoiceEnum::Enum { default, alternatives },
		)))) => std::iter::once(default).chain(alternatives).collect(),
		_ => Vec::new(),
	}
}

fn format_object(properties: Vec<Property>) -> Value {
	Value::Object(Object {
		type_: SpaTypes::ObjectParamFormat.as_raw(),
		id: pw::spa::param::ParamType::EnumFormat.as_raw(),
		properties,
	})
}

/// The formats we offer: each pixel format as a DMA-BUF (if `dmabuf`) with
/// the sink's tiled modifiers ahead of LINEAR, then all of them in shared
/// memory.
fn enum_formats(
	fps: u32,
	dmabuf: bool,
	tiled: &[u64],
) -> std::result::Result<Vec<Vec<u8>>, String> {
	let modifiers = our_modifiers(tiled);
	let mut formats = Vec::new();
	let base = || {
		vec![
			property!(FormatProperties::MediaType, Id, MediaType::Video),
			property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
		]
	};
	if dmabuf {
		for format in FORMATS {
			let mut properties = base();
			properties.push(property!(FormatProperties::VideoFormat, Id, format));
			properties.push(modifier_property(&modifiers, false));
			properties.push(size_range());
			properties.push(framerate_range(fps));
			formats.push(serialize(format_object(properties))?);
		}
	}
	let mut properties = base();
	properties.push(property!(
		FormatProperties::VideoFormat,
		Choice,
		Enum,
		Id,
		FORMATS[0],
		FORMATS[0],
		FORMATS[1],
		FORMATS[2],
		FORMATS[3]
	));
	properties.push(size_range());
	properties.push(framerate_range(fps));
	formats.push(serialize(format_object(properties))?);
	Ok(formats)
}

/// Offer formats again (another frame rate, other modifiers, or no
/// DMA-BUFs); PipeWire renegotiates.
fn renegotiate(stream: &pw::stream::Stream, state: &VideoState) {
	let result = enum_formats(state.fps, state.dmabuf, &state.modifiers).and_then(|formats| {
		let mut pods =
			formats.iter().map(|f| pod(f)).collect::<std::result::Result<Vec<_>, _>>()?;
		stream.update_params(&mut pods).map_err(|e| e.to_string())
	});
	if let Err(e) = result {
		warn!("cannot renegotiate the screen capture format: {e}");
	}
}

fn format_changed(
	stream: &pw::stream::Stream,
	state: &mut VideoState,
	param: &Pod,
) -> std::result::Result<(), String> {
	let Ok((MediaType::Video, MediaSubtype::Raw)) =
		pw::spa::param::format_utils::parse_format(param)
	else {
		return Ok(());
	};
	let mut info = VideoInfoRaw::new();
	info.parse(param).map_err(|e| format!("cannot parse the video format: {e}"))?;
	let size = info.size();
	state.maps.clear();
	let modifier = param
		.as_object()
		.ok()
		.and_then(|o| o.find_prop(Id(FormatProperties::VideoModifier.as_raw())));
	let modifier = match modifier {
		Some(prop) if prop.flags().contains(PodPropFlags::DONT_FIXATE) => {
			// The producer left the choice to us: the best of ours that it
			// can make, else LINEAR.
			let theirs = offered_modifiers(prop);
			let ours = our_modifiers(&state.modifiers);
			let chosen = ours.into_iter().find(|m| theirs.contains(m)).unwrap_or(MODIFIER_LINEAR);
			debug!(offered = ?theirs, chosen, "screen capture modifier");
			let mut properties = vec![
				property!(FormatProperties::MediaType, Id, MediaType::Video),
				property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
				property!(FormatProperties::VideoFormat, Id, info.format()),
				modifier_property(&[chosen], true),
				property!(
					FormatProperties::VideoSize,
					Rectangle,
					Rectangle { width: size.width, height: size.height }
				),
			];
			properties.push(framerate_range(state.fps));
			let fixed = serialize(format_object(properties))?;
			stream.update_params(&mut [pod(&fixed)?]).map_err(|e| e.to_string())?;
			return Ok(());
		}
		Some(prop) => Some(prop.value().get_long().unwrap_or(MODIFIER_LINEAR)),
		None => None,
	};
	debug!(format = ?info.format(), size.width, size.height, ?modifier, "screen capture format");
	state.format = Some(Negotiated {
		format: info.format(),
		width: size.width,
		height: size.height,
		modifier,
	});
	let types = if modifier.is_some() {
		1 << pw::spa::sys::SPA_DATA_DmaBuf
	} else {
		(1 << pw::spa::sys::SPA_DATA_MemPtr) | (1 << pw::spa::sys::SPA_DATA_MemFd)
	};
	let buffers = serialize(Value::Object(Object {
		type_: SpaTypes::ObjectParamBuffers.as_raw(),
		id: pw::spa::param::ParamType::Buffers.as_raw(),
		properties: vec![Property {
			key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
			flags: PropertyFlags::empty(),
			value: Value::Int(types as i32),
		}],
	}))?;
	stream.update_params(&mut [pod(&buffers)?]).map_err(|e| e.to_string())
}

fn process(stream: &pw::stream::Stream, state: &mut VideoState) {
	// Follow the sink: ask the compositor for another rate, or buffers with
	// the modifiers the sink takes now.
	let fps = state.sink.max_fps().max(1);
	let modifiers = state.sink.dmabuf_modifiers();
	if fps != state.fps || (state.dmabuf && modifiers != state.modifiers.as_slice()) {
		state.fps = fps;
		state.modifiers = modifiers.to_vec();
		renegotiate(stream, state);
	}
	let Some(mut buffer) = stream.dequeue_buffer() else { return };
	let Some(format) = state.format else { return };
	if format.modifier.is_some_and(|m| m != MODIFIER_LINEAR) && !state.sink.accepts_dmabuf() {
		// Tiled, and the sink no longer imports it: the CPU cannot read it.
		// The new offer (above) brings LINEAR buffers.
		return;
	}
	let timestamp = state.started.elapsed();
	if !state.sink.wants(timestamp) {
		// Dropping the buffer queues it back.
		return;
	}
	let Some(data) = buffer.datas_mut().first_mut() else { return };
	let bgra = match format.format {
		VideoFormat::BGRx | VideoFormat::BGRA => true,
		VideoFormat::RGBx | VideoFormat::RGBA => false,
		_ => return,
	};
	let chunk = data.chunk();
	if chunk.size() == 0 || chunk.flags().contains(pw::spa::buffer::ChunkFlags::CORRUPTED) {
		return;
	}
	let offset = chunk.offset() as usize;
	let row = format.width as usize * 4;
	let stride = usize::try_from(chunk.stride()).ok().filter(|&s| s >= row).unwrap_or(row);
	let needed = offset + stride * (format.height as usize).saturating_sub(1) + row;
	fn view(
		bytes: &[u8],
		at: (usize, usize, bool),
		format: Negotiated,
		t: Duration,
	) -> Option<FrameRef<'_>> {
		let (offset, stride, bgra) = at;
		let plane = PlaneRef::new(bytes.get(offset..)?, stride);
		let pixels = if bgra { PixelsRef::Bgra(plane) } else { PixelsRef::Rgba(plane) };
		Some(FrameRef { width: format.width, height: format.height, timestamp: t, pixels })
	}
	let at = (offset, stride, bgra);
	let is_dmabuf = data.type_() == DataType::DmaBuf;
	if state.logged != Some(is_dmabuf) {
		state.logged = Some(is_dmabuf);
		debug!(dmabuf = is_dmabuf, "screen capture buffers");
	}
	// A sink that imports DMA-BUFs (a VA-API encoder) gets the buffer as it
	// is, without a mapping or copy.
	if is_dmabuf && state.sink.accepts_dmabuf() {
		let raw = data.as_raw();
		let fourcc = match format.format {
			VideoFormat::BGRx => drm_fourcc(b"XR24"),
			VideoFormat::BGRA => drm_fourcc(b"AR24"),
			VideoFormat::RGBx => drm_fourcc(b"XB24"),
			_ => drm_fourcc(b"AB24"),
		};
		let frame = DmaBufRef {
			width: format.width,
			height: format.height,
			timestamp,
			fourcc,
			modifier: format.modifier.map_or(DRM_MOD_LINEAR, |m| m as u64),
			fd: data.fd(),
			// As for the mapping below: compositors leave `maxsize` 0 for
			// a DMA-BUF, whose size is its buffer object's.
			size: match (raw.mapoffset + raw.maxsize) as usize {
				0 => dmabuf::size_of(data.fd()).unwrap_or(raw.mapoffset as usize + needed),
				size => size,
			},
			planes: [(raw.mapoffset as usize + offset, stride), (0, 0), (0, 0), (0, 0)],
			plane_count: 1,
		};
		if let Some(more) = state.sink.dmabuf(&frame) {
			if !more && let Some(mainloop) = state.mainloop.upgrade() {
				mainloop.quit();
			}
			return;
		}
	}
	if format.modifier.is_some_and(|m| m != MODIFIER_LINEAR) {
		// Tiled and not taken as it is: nothing the CPU can read.
		return;
	}
	let started = Instant::now();
	let more = if is_dmabuf {
		let raw = data.as_raw();
		// A DMA-BUF's size lives in its buffer object, not in `maxsize`:
		// `maxsize` describes a mapping, which only shared memory has, and
		// compositors leave it 0 (measured on a 2560x1440 Wayland desktop:
		// fd valid, stride 10240, `mapoffset` and `maxsize` both 0). Mapping
		// `mapoffset + maxsize` bytes then asks for a zero-length mapping
		// and every frame is dropped, so fall back to the rows the
		// negotiated format describes, which is exactly what is read below.
		let len = match (raw.mapoffset + raw.maxsize) as usize {
			0 => raw.mapoffset as usize + needed,
			size => size,
		};
		let fd = data.fd();
		if !state.maps.iter().any(|m| m.fd() == fd && m.len() >= len) {
			state.maps.retain(|m| m.fd() != fd);
			match DmaBufMap::new(fd, len) {
				Ok(map) => state.maps.push(map),
				Err(e) => {
					warn!(
						fd,
						len,
						mapoffset = raw.mapoffset,
						maxsize = raw.maxsize,
						stride,
						height = format.height,
						"cannot map a screen capture DMA-BUF: {e}"
					);
					// Dropping every frame would leave the viewer a black
					// screen; shared memory always works.
					state.dmabuf_failures += 1;
					if state.dmabuf_failures >= DMABUF_FAILURES_BEFORE_SHM && state.dmabuf {
						tracing::info!(
							"screen capture DMA-BUFs cannot be mapped; switching to shared memory"
						);
						state.dmabuf = false;
						renegotiate(stream, state);
					}
					return;
				}
			}
		}
		let map = state.maps.iter().find(|m| m.fd() == fd).expect("mapped above");
		let base = raw.mapoffset as usize;
		map.read(|bytes| {
			match bytes
				.get(base..)
				.filter(|b| b.len() >= needed)
				.and_then(|b| view(b, at, format, timestamp))
			{
				Some(frame) => state.sink.frame(frame),
				None => true,
			}
		})
	} else {
		match data.data().filter(|b| b.len() >= needed).and_then(|b| view(b, at, format, timestamp))
		{
			Some(frame) => state.sink.frame(frame),
			None => true,
		}
	};
	if is_dmabuf && state.dmabuf {
		probe_dmabuf(stream, state, started.elapsed(), format);
	}
	if !more && let Some(mainloop) = state.mainloop.upgrade() {
		// Nobody takes frames any more.
		mainloop.quit();
	}
}

/// Time the first DMA-BUF frames; if reading them is slow, offer shared
/// memory only.
fn probe_dmabuf(
	stream: &pw::stream::Stream,
	state: &mut VideoState,
	took: Duration,
	format: Negotiated,
) {
	if state.dmabuf_frames >= DMABUF_PROBE_FRAMES {
		return;
	}
	state.dmabuf_frames += 1;
	state.dmabuf_time += took;
	if state.dmabuf_frames < DMABUF_PROBE_FRAMES {
		return;
	}
	let pixels = f64::from(format.width) * f64::from(format.height);
	let per_pixel = state.dmabuf_time.as_secs_f64() * 1e9 / f64::from(DMABUF_PROBE_FRAMES) / pixels;
	if per_pixel > SLOW_DMABUF_NS_PER_PIXEL {
		tracing::info!(
			"reading DMA-BUFs takes {per_pixel:.1} ns per pixel; switching screen capture to shared memory"
		);
		state.dmabuf = false;
		renegotiate(stream, state);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Without a session bus the portal must fail cleanly, not hang or
	/// panic. Skipped where a bus exists (it could open a real dialog).
	#[tokio::test]
	async fn unavailable_without_session_bus() {
		let runtime_bus = std::env::var_os("XDG_RUNTIME_DIR")
			.is_some_and(|d| std::path::Path::new(&d).join("bus").exists());
		if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() || runtime_bus {
			eprintln!("skipped: a D-Bus session bus is configured");
			return;
		}
		let mut capture = PortalCapture::new();
		assert_eq!(capture.sources().unwrap()[0].id, SourceId::Portal);
		let err = capture.start(&SourceId::Portal, &CaptureOptions::default()).await.err().unwrap();
		assert!(matches!(err, Error::CaptureUnavailable { backend: "portal", .. }), "{err}");
		assert!(!PortalCapture::is_available().await);
		assert!(capture.restore_token().is_none());
	}

	/// The offered formats parse back: DMA-BUF (mandatory LINEAR modifier,
	/// left to us to fixate) per pixel format, then shared memory.
	#[test]
	fn offered_formats() {
		// A sink that imports one tiled modifier (an AMD one here).
		const TILED: u64 = 0x0200_0000_28a0_1f04;
		let formats = enum_formats(60, true, &[TILED]).unwrap();
		assert_eq!(formats.len(), FORMATS.len() + 1);
		for (i, bytes) in formats.iter().enumerate() {
			let object = pod(bytes).unwrap().as_object().unwrap();
			let modifier = object.find_prop(Id(FormatProperties::VideoModifier.as_raw()));
			if i < FORMATS.len() {
				let modifier = modifier.expect("a modifier");
				let flags = modifier.flags();
				assert!(flags.contains(PodPropFlags::MANDATORY | PodPropFlags::DONT_FIXATE));
				// Read back as a producer's choice is: ours first, LINEAR last.
				let offered = offered_modifiers(modifier);
				assert_eq!(offered.first(), Some(&(TILED as i64)));
				assert_eq!(offered.last(), Some(&MODIFIER_LINEAR));
			} else {
				assert!(modifier.is_none(), "shared memory has no modifier");
			}
		}
		let linear_only = enum_formats(60, true, &[]).unwrap();
		let object = pod(&linear_only[0]).unwrap().as_object().unwrap();
		let prop = object.find_prop(Id(FormatProperties::VideoModifier.as_raw())).unwrap();
		assert_eq!(offered_modifiers(prop), [MODIFIER_LINEAR, MODIFIER_LINEAR]);
		assert_eq!(enum_formats(30, false, &[TILED]).unwrap().len(), 1);
		let fixed =
			serialize(format_object(vec![modifier_property(&[MODIFIER_LINEAR], true)])).unwrap();
		let object = pod(&fixed).unwrap().as_object().unwrap();
		let prop = object.find_prop(Id(FormatProperties::VideoModifier.as_raw())).unwrap();
		assert_eq!(prop.value().get_long().unwrap(), MODIFIER_LINEAR);
		assert!(!prop.flags().contains(PodPropFlags::DONT_FIXATE));
	}
}
