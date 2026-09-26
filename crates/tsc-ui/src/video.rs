//! Video of streams for the UI: capture and encoding when we share, decoding
//! into Slint images when we watch, and the optional OpenH264 library.
//!
//! Needs the `media` feature (tsc-core's media pipeline). Without it the
//! same API exists and reports that video is not available; stream audio
//! still plays (the engine does that).

pub(crate) use imp::*;

/// Human-readable text for a picture size.
#[cfg(feature = "media")]
fn size_text(width: u32, height: u32) -> String {
	if width == 0 || height == 0 { String::new() } else { format!("{width}×{height}") }
}

#[cfg(feature = "media")]
mod imp {
	use std::path::{Path, PathBuf};
	use std::sync::Arc;

	use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
	use tokio::runtime::Handle;
	use tracing::warn;
	use tsc_core::media::tsc_media::capture::SourceId;
	use tsc_core::media::tsc_media::codec::h264::{self, OpenH264};
	use tsc_core::media::tsc_media::{Codecs, VideoFrame, convert};
	use tsc_core::media::{
		self, Latest, LocalPreview, Streamer, StreamerConfig, Viewer, peer_config, stream_codec,
	};
	use tsc_core::stream::PeerConfig;
	use tsc_core::{Engine, StreamSink};

	use super::size_text;

	/// Whether this build can share and decode video.
	pub const AVAILABLE: bool = true;

	type Picture = SharedPixelBuffer<Rgba8Pixel>;

	/// OpenH264 state.
	enum H264 {
		Off,
		/// Enabled, but not downloaded yet (or the file is not a known build).
		Missing(String),
		Downloading,
		Loaded(PathBuf),
	}

	pub(crate) struct Video {
		codecs: Arc<Codecs>,
		openh264_dir: PathBuf,
		h264: H264,
		/// Sources of the share dialog, by index.
		sources: Vec<(SourceId, String)>,
	}

	impl Video {
		pub fn new(data_dir: &Path, openh264: bool) -> Self {
			let mut video = Self {
				codecs: Arc::new(Codecs::new()),
				openh264_dir: data_dir.join("openh264"),
				h264: H264::Off,
				sources: Vec::new(),
			};
			video.set_h264(openh264);
			video
		}

		/// `base` with the codecs this machine can encode and decode.
		pub fn peer_config(&self, base: PeerConfig) -> PeerConfig {
			peer_config(&self.codecs, base)
		}

		/// Whether H.264 works, and a line for the settings page.
		pub fn h264_status(&self) -> (bool, String) {
			match &self.h264 {
				H264::Off => (false, String::new()),
				H264::Missing(reason) => (false, reason.clone()),
				H264::Downloading => (false, "Downloading OpenH264 from Cisco…".into()),
				H264::Loaded(path) => (
					true,
					format!("OpenH264 {} is loaded ({}).", h264::OPENH264_VERSION, path.display()),
				),
			}
		}

		/// Enable or disable H.264; enabling loads a library downloaded
		/// earlier. Takes effect for new connections.
		pub fn set_h264(&mut self, enabled: bool) {
			if !enabled {
				self.h264 = H264::Off;
				self.codecs = Arc::new(Codecs::new());
				return;
			}
			if matches!(self.h264, H264::Loaded(_) | H264::Downloading) {
				return;
			}
			match OpenH264::find_in(&self.openh264_dir) {
				Ok(library) => self.loaded(library),
				Err(_) => {
					self.h264 = H264::Missing("OpenH264 is not downloaded yet.".into());
				}
			}
		}

		fn loaded(&mut self, library: OpenH264) {
			self.h264 = H264::Loaded(library.path().to_owned());
			self.codecs = Arc::new(Codecs::new().with_openh264(library));
		}

		/// Download Cisco's OpenH264 (only on the user's request). `done`
		/// runs on the runtime; pass its argument to [`Video::downloaded`].
		pub fn download_h264(
			&mut self,
			runtime: &Handle,
			done: impl FnOnce(H264Download) + Send + 'static,
		) {
			if matches!(self.h264, H264::Downloading | H264::Loaded(_)) {
				return;
			}
			self.h264 = H264::Downloading;
			let dir = self.openh264_dir.clone();
			runtime.spawn(async move {
				let result = h264::download_openh264(&dir).await.map_err(|e| e.to_string());
				done(H264Download(result));
			});
		}

		pub fn downloaded(&mut self, download: H264Download, enabled: bool) {
			match download.0 {
				Ok(library) if enabled => self.loaded(library),
				Ok(_) => self.h264 = H264::Off,
				Err(e) => {
					warn!("OpenH264 download failed: {e}");
					self.h264 = H264::Missing(format!("Download failed: {e}"));
				}
			}
		}

		/// Screens and windows to share, as (name, detail); the test pattern
		/// too when asked for.
		pub fn sources(&mut self, test_pattern: bool) -> Result<Vec<(String, String)>, String> {
			let mut error = None;
			let mut sources = match media::screen_sources() {
				Ok(list) => list,
				Err(e) => {
					error = Some(e.to_string());
					Vec::new()
				}
			};
			if test_pattern {
				sources.push(media::test_pattern_source());
			}
			if sources.is_empty() {
				return Err(error.unwrap_or_else(|| "Nothing to share was found.".into()));
			}
			self.sources = sources.iter().map(|s| (s.id.clone(), s.name.clone())).collect();
			Ok(sources
				.iter()
				.map(|s| {
					let name = match s.id {
						SourceId::Portal => "Choose via system dialog".to_owned(),
						_ => s.name.clone(),
					};
					let detail = match (&s.id, s.primary) {
						(SourceId::Monitor(_), true) => {
							format!("{} · primary", size_text(s.width, s.height))
						}
						(SourceId::Window(_), _) => {
							format!("window {}", size_text(s.width, s.height)).trim().to_owned()
						}
						_ => size_text(s.width, s.height),
					};
					(name, detail)
				})
				.collect())
		}

		/// Start capturing source `index` of [`Video::sources`]; `done` runs
		/// on the runtime (the portal may wait for the user first).
		pub fn start_capture(
			&self,
			runtime: &Handle,
			index: usize,
			options: CaptureRequest,
			done: impl FnOnce(Result<Capture, String>) + Send + 'static,
		) {
			let Some((source, name)) = self.sources.get(index).cloned() else {
				done(Err("Pick something to share.".into()));
				return;
			};
			let peer = self.peer_config(PeerConfig::default());
			let Some(codec) = stream_codec(&self.codecs, &peer) else {
				done(Err("No video encoder is available.".into()));
				return;
			};
			let codecs = self.codecs.clone();
			let config = StreamerConfig {
				source,
				fps: options.fps,
				bitrate_kbps: options.bitrate_kbps,
				codec,
				audio: options.audio,
				restore_token: options.restore_token,
				..StreamerConfig::default()
			};
			runtime.spawn(async move {
				let result = Streamer::start(&codecs, config).await;
				done(result.map(|streamer| Capture { streamer, name }).map_err(|e| e.to_string()));
			});
		}

		/// Decode stream `stream_id` of `session`; `wake` is called (on the
		/// decoder thread) when a picture is ready and none is waiting.
		pub fn watch(
			&self,
			engine: &Engine,
			session: u64,
			stream_id: &str,
			wake: impl Fn() + Send + Sync + 'static,
		) -> Decoder {
			let pictures = Arc::new(Latest::new());
			let on_frame = deliver(pictures.clone(), wake);
			let viewer = Viewer::start(engine, session, stream_id, self.codecs.clone(), on_frame);
			Decoder { source: Source::Viewer(viewer), pictures }
		}

		/// The test pattern through the encoder and decoder, without a
		/// server (`TSC_DEMO_STREAM`).
		pub fn demo(
			&self,
			runtime: &Handle,
			wake: impl Fn() + Send + Sync + 'static,
			done: impl FnOnce(Result<Decoder, String>) + Send + 'static,
		) {
			let pictures = Arc::new(Latest::new());
			let on_frame = deliver(pictures.clone(), wake);
			let codecs = self.codecs.clone();
			let config =
				StreamerConfig { source: SourceId::Synthetic, ..StreamerConfig::default() };
			runtime.spawn(async move {
				let result = LocalPreview::start(codecs, config, on_frame).await;
				done(
					result
						.map(|preview| Decoder { source: Source::Preview(preview), pictures })
						.map_err(|e| e.to_string()),
				);
			});
		}
	}

	/// Converts decoded frames into Slint pixel buffers, keeping the newest.
	fn deliver(
		pictures: Arc<Latest<Picture>>,
		wake: impl Fn() + Send + Sync + 'static,
	) -> impl FnMut(VideoFrame) + Send + 'static {
		move |frame: VideoFrame| {
			let mut buffer = Picture::new(frame.width, frame.height);
			let stride = frame.width as usize * 4;
			if let Err(e) = convert::to_rgba(&frame, buffer.make_mut_bytes(), stride) {
				warn!("cannot show a picture: {e}");
				return;
			}
			if pictures.put(buffer) {
				wake();
			}
		}
	}

	/// The result of [`Video::download_h264`].
	pub(crate) struct H264Download(Result<OpenH264, String>);

	/// Share settings from the dialog.
	pub(crate) struct CaptureRequest {
		pub fps: u32,
		pub bitrate_kbps: u32,
		pub audio: bool,
		pub restore_token: Option<String>,
	}

	/// A running capture for our stream. Stops when dropped.
	pub(crate) struct Capture {
		streamer: Streamer,
		name: String,
	}

	impl Capture {
		/// Encode into the live stream.
		pub fn attach(&self, sink: StreamSink) {
			self.streamer.attach(Arc::new(sink));
		}

		pub fn source_name(&self) -> &str {
			&self.name
		}

		pub fn has_audio(&self) -> bool {
			self.streamer.has_audio()
		}

		pub fn audio_error(&self) -> Option<String> {
			self.streamer.audio_error().map(str::to_owned)
		}

		/// The portal's token for this choice, to keep.
		pub fn restore_token(&self) -> Option<String> {
			self.streamer.restore_token().map(str::to_owned)
		}

		/// The capture stopped by itself (e.g. sharing ended in the desktop).
		pub fn ended(&self) -> bool {
			self.streamer.stats().capture_ended
		}

		/// One line about what is sent.
		pub fn status(&self) -> String {
			let stats = self.streamer.stats();
			let mut text = format!("{} · {}", self.name, size_text(stats.width, stats.height));
			text.push_str(&format!(" · {} frames sent", stats.video_frames));
			if self.streamer.has_audio() {
				text.push_str(" · with sound");
			}
			if let Some(e) = stats.error {
				text.push_str(&format!(" · encoder: {e}"));
			}
			text
		}
	}

	enum Source {
		Viewer(Viewer),
		Preview(LocalPreview),
	}

	/// Decodes a watched stream into pictures. Stops when dropped.
	pub(crate) struct Decoder {
		source: Source,
		pictures: Arc<Latest<Picture>>,
	}

	impl Decoder {
		/// The newest picture since the last call.
		pub fn take_picture(&self) -> Option<Image> {
			self.pictures.take().map(Image::from_rgba8)
		}

		/// One line about the video: codec, size, problems.
		pub fn info(&self) -> String {
			let stats = match &self.source {
				Source::Viewer(v) => v.stats(),
				Source::Preview(p) => p.stats(),
			};
			let mut parts = Vec::new();
			if let Some(codec) = stats.codec {
				parts.push(codec.to_string());
			}
			if stats.width > 0 {
				parts.push(size_text(stats.width, stats.height));
			}
			if let Some(e) = stats.error {
				parts.push(e);
			}
			parts.join(" · ")
		}

		/// Whether pictures could never come (no decoder for the codec).
		pub fn error(&self) -> Option<String> {
			let stats = match &self.source {
				Source::Viewer(v) => v.stats(),
				Source::Preview(p) => p.stats(),
			};
			stats.error.filter(|_| stats.decoded == 0)
		}
	}
}

#[cfg(not(feature = "media"))]
mod imp {
	use std::path::Path;

	use slint::Image;
	use tokio::runtime::Handle;
	use tsc_core::stream::PeerConfig;
	use tsc_core::{Engine, StreamSink};

	pub const AVAILABLE: bool = false;
	const UNAVAILABLE: &str = "Video is not available in this build.";

	pub(crate) struct Video;

	impl Video {
		pub fn new(_data_dir: &Path, _openh264: bool) -> Self {
			Self
		}

		pub fn peer_config(&self, base: PeerConfig) -> PeerConfig {
			base
		}

		pub fn h264_status(&self) -> (bool, String) {
			(false, UNAVAILABLE.into())
		}

		pub fn set_h264(&mut self, _enabled: bool) {}

		pub fn download_h264(&mut self, _runtime: &Handle, _done: impl FnOnce(H264Download)) {}

		pub fn downloaded(&mut self, _download: H264Download, _enabled: bool) {}

		pub fn sources(&mut self, _test_pattern: bool) -> Result<Vec<(String, String)>, String> {
			Err(UNAVAILABLE.into())
		}

		pub fn start_capture(
			&self,
			_runtime: &Handle,
			_index: usize,
			_options: CaptureRequest,
			done: impl FnOnce(Result<Capture, String>),
		) {
			done(Err(UNAVAILABLE.into()));
		}

		pub fn watch(
			&self,
			_engine: &Engine,
			_session: u64,
			_stream_id: &str,
			_wake: impl Fn() + Send + Sync + 'static,
		) -> Decoder {
			Decoder
		}

		pub fn demo(
			&self,
			_runtime: &Handle,
			_wake: impl Fn() + Send + Sync + 'static,
			done: impl FnOnce(Result<Decoder, String>),
		) {
			done(Err(UNAVAILABLE.into()));
		}
	}

	pub(crate) struct H264Download;

	#[allow(dead_code, reason = "the same fields as with video")]
	pub(crate) struct CaptureRequest {
		pub fps: u32,
		pub bitrate_kbps: u32,
		pub audio: bool,
		pub restore_token: Option<String>,
	}

	pub(crate) enum Capture {}

	impl Capture {
		pub fn attach(&self, _sink: StreamSink) {
			match *self {}
		}

		pub fn source_name(&self) -> &str {
			match *self {}
		}

		pub fn has_audio(&self) -> bool {
			match *self {}
		}

		pub fn audio_error(&self) -> Option<String> {
			match *self {}
		}

		pub fn restore_token(&self) -> Option<String> {
			match *self {}
		}

		pub fn ended(&self) -> bool {
			match *self {}
		}

		pub fn status(&self) -> String {
			match *self {}
		}
	}

	pub(crate) struct Decoder;

	impl Decoder {
		pub fn take_picture(&self) -> Option<Image> {
			None
		}

		pub fn info(&self) -> String {
			String::new()
		}

		pub fn error(&self) -> Option<String> {
			Some(UNAVAILABLE.into())
		}
	}
}
