//! Wayland screen capture: the xdg-desktop-portal ScreenCast interface
//! (`ashpd`) picks the source and hands over a PipeWire remote; frames come
//! through a PipeWire video stream.
//!
//! The portal can remember the user's choice: pass the token from
//! [`PortalCapture::restore_token`] to [`PortalCapture::with_restore_token`]
//! next time (it persists until the user revokes it). Only shared-memory
//! buffers (`BGRx` / `RGBx` family) are negotiated; DMA-BUF import is a TODO.

use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use pipewire as pw;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::{Value, object, property};
use pw::spa::utils::{Fraction, Rectangle, SpaTypes};
use tracing::{debug, warn};

use crate::capture::pw::{PwThread, pod, serialize};
use crate::capture::{BoxFuture, CaptureOptions, CaptureSource, ScreenCapture, SourceId};
use crate::frame::VideoFrame;
use crate::queue::{FrameReceiver, FrameSender, frame_channel};
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
#[derive(Default)]
pub struct PortalCapture {
	restore_token: Option<String>,
	session: Option<Session>,
	thread: Option<PwThread>,
}

impl PortalCapture {
	pub fn new() -> Self {
		Self::default()
	}

	/// Reuse an earlier choice without asking (a token from
	/// [`PortalCapture::restore_token`]).
	pub fn with_restore_token(token: Option<String>) -> Self {
		Self { restore_token: token, session: None, thread: None }
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
		let source = source.clone();
		let options = options.clone();
		Box::pin(async move {
			if source != SourceId::Portal {
				return Err(Error::SourceNotFound(source));
			}
			self.stop();
			let (fd, node) = self.open(&options).await?;
			let (tx, rx) = frame_channel(options.queue);
			let fps = options.fps.max(1);
			let thread = PwThread::spawn("tsc-portal-capture", Some(fd), move |core, mainloop| {
				video_stream(core, mainloop, node, fps, tx)
			})
			.map_err(|e| Error::Capture { backend: BACKEND, message: e })?;
			self.thread = Some(thread);
			Ok(rx)
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

struct VideoState {
	tx: FrameSender<VideoFrame>,
	format: Option<(VideoFormat, u32, u32)>,
	started: Instant,
	interval: Duration,
	last: Option<Instant>,
	mainloop: pw::main_loop::MainLoopWeak,
}

fn video_stream(
	core: &pw::core::CoreRc,
	mainloop: &pw::main_loop::MainLoopRc,
	node: u32,
	fps: u32,
	tx: FrameSender<VideoFrame>,
) -> std::result::Result<(pw::stream::StreamRc, pw::stream::StreamListener<VideoState>), String> {
	let props = pw::properties::properties! {
		*pw::keys::MEDIA_TYPE => "Video",
		*pw::keys::MEDIA_CATEGORY => "Capture",
		*pw::keys::MEDIA_ROLE => "Screen",
	};
	let stream = pw::stream::StreamRc::new(core.clone(), "tsc-screen-capture", props)
		.map_err(|e| format!("PipeWire stream: {e}"))?;
	let state = VideoState {
		tx,
		format: None,
		started: Instant::now(),
		// Accept frames slightly early so rounding does not halve the rate.
		interval: Duration::from_secs(1) / fps * 9 / 10,
		last: None,
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
		.param_changed(|_, state, id, param| {
			let Some(param) = param else { return };
			if id != pw::spa::param::ParamType::Format.as_raw() {
				return;
			}
			let Ok((MediaType::Video, MediaSubtype::Raw)) =
				pw::spa::param::format_utils::parse_format(param)
			else {
				return;
			};
			let mut info = VideoInfoRaw::new();
			if info.parse(param).is_ok() {
				let size = info.size();
				debug!(format = ?info.format(), size.width, size.height, "screen capture format");
				state.format = Some((info.format(), size.width, size.height));
			}
		})
		.process(|stream, state| {
			let Some(mut buffer) = stream.dequeue_buffer() else { return };
			let Some((format, width, height)) = state.format else { return };
			let now = Instant::now();
			if state.last.is_some_and(|last| now - last < state.interval) {
				return;
			}
			let Some(data) = buffer.datas_mut().first_mut() else { return };
			let Some(frame) = copy_frame(data, format, width, height) else { return };
			state.last = Some(now);
			if !state.tx.send(frame.with_timestamp(now - state.started))
				&& let Some(mainloop) = state.mainloop.upgrade()
			{
				// Nobody receives frames anymore.
				mainloop.quit();
			}
		})
		.register()
		.map_err(|e| format!("PipeWire listener: {e}"))?;

	let format = serialize(Value::Object(object!(
		SpaTypes::ObjectParamFormat,
		pw::spa::param::ParamType::EnumFormat,
		property!(FormatProperties::MediaType, Id, MediaType::Video),
		property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
		property!(
			FormatProperties::VideoFormat,
			Choice,
			Enum,
			Id,
			VideoFormat::BGRx,
			VideoFormat::BGRx,
			VideoFormat::BGRA,
			VideoFormat::RGBx,
			VideoFormat::RGBA
		),
		property!(
			FormatProperties::VideoSize,
			Choice,
			Range,
			Rectangle,
			Rectangle { width: 1920, height: 1080 },
			Rectangle { width: 1, height: 1 },
			Rectangle { width: 8192, height: 8192 }
		),
		property!(
			FormatProperties::VideoFramerate,
			Choice,
			Range,
			Fraction,
			Fraction { num: fps, denom: 1 },
			Fraction { num: 0, denom: 1 },
			Fraction { num: fps, denom: 1 }
		),
	)))?;
	stream
		.connect(
			pw::spa::utils::Direction::Input,
			Some(node),
			pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
			&mut [pod(&format)?],
		)
		.map_err(|e| format!("cannot connect to the screen cast node {node}: {e}"))?;
	Ok((stream, listener))
}

/// Copy one shared-memory buffer into a frame. `None` for buffers without
/// mapped data (DMA-BUF), empty or corrupted chunks, and unknown formats.
fn copy_frame(
	data: &mut pw::spa::buffer::Data,
	format: VideoFormat,
	width: u32,
	height: u32,
) -> Option<VideoFrame> {
	let bgra = match format {
		VideoFormat::BGRx | VideoFormat::BGRA => true,
		VideoFormat::RGBx | VideoFormat::RGBA => false,
		_ => return None,
	};
	let chunk = data.chunk();
	if chunk.size() == 0 || chunk.flags().contains(pw::spa::buffer::ChunkFlags::CORRUPTED) {
		return None;
	}
	let offset = chunk.offset() as usize;
	let row = width as usize * 4;
	let stride = usize::try_from(chunk.stride()).ok().filter(|&s| s >= row).unwrap_or(row);
	let bytes = data.data()?;
	let bytes = bytes.get(offset..)?;
	let needed = stride * (height as usize).checked_sub(1)? + row;
	if bytes.len() < needed {
		return None;
	}
	let mut pixels = Vec::with_capacity(row * height as usize);
	for y in 0..height as usize {
		pixels.extend_from_slice(&bytes[y * stride..y * stride + row]);
	}
	let frame = if bgra {
		VideoFrame::from_bgra(width, height, row, pixels)
	} else {
		VideoFrame::from_rgba(width, height, row, pixels)
	};
	frame.ok()
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
}
