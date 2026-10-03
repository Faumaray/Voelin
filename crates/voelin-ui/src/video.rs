//! Video of streams for the UI: capture and encoding when we share, decoding
//! into Slint images when we watch, and the optional OpenH264 library
//! (desktop; Android decodes H.264 with the device's MediaCodec).

use std::path::Path;
#[cfg(not(target_os = "android"))]
use std::path::PathBuf;
use std::sync::Arc;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use tokio::runtime::Handle;
use tracing::warn;
use voelin_core::media::voelin_media::capture::{CaptureSource, SourceId};
use voelin_core::media::voelin_media::{Codec, Codecs, EncoderReport, VideoFrame, convert};
use voelin_core::media::{
	self, AudioSourceSpec, EncoderPreference, Latest, LocalPreview, Streamer, StreamerConfig,
	StreamerConfigUpdate, Viewer, peer_config, preferred_codec, stream_codec,
};
use voelin_core::stream::{LayerSpec, PeerConfig};
use voelin_core::studio::Studio;
use voelin_core::{Engine, StreamSink};

/// Whether this build can share and decode video.
pub const AVAILABLE: bool = true;

type Picture = SharedPixelBuffer<Rgba8Pixel>;

/// Human-readable text for a picture size.
fn size_text(width: u32, height: u32) -> String {
	if width == 0 || height == 0 { String::new() } else { format!("{width}×{height}") }
}

/// Cisco's OpenH264, downloaded on the user's request.
#[cfg(not(target_os = "android"))]
mod openh264 {
	pub use voelin_core::media::voelin_media::codec::h264::{
		OPENH264_VERSION, OpenH264, download_openh264,
	};

	pub enum State {
		Off,
		/// Enabled, but not downloaded yet (or the file is not a known build).
		Missing(String),
		Downloading,
		Loaded(std::path::PathBuf),
	}
}

/// The result of [`Video::download_h264`].
#[cfg(not(target_os = "android"))]
pub(crate) struct H264Download(Result<openh264::OpenH264, String>);
#[cfg(target_os = "android")]
pub(crate) struct H264Download;

pub(crate) struct Video {
	codecs: Arc<Codecs>,
	#[cfg(not(target_os = "android"))]
	openh264_dir: PathBuf,
	#[cfg(not(target_os = "android"))]
	h264: openh264::State,
	/// Which encoders to use and the configured stream codec (`None`:
	/// automatic), from the settings.
	encoder: EncoderPreference,
	codec: Option<Codec>,
	/// Sources of the share dialog, by index.
	sources: Vec<ShareEntry>,
}

impl Video {
	pub fn new(data_dir: &Path, openh264: bool) -> Self {
		#[cfg(target_os = "android")]
		let _ = (data_dir, openh264);
		#[allow(unused_mut, reason = "set_h264 does nothing on Android")]
		let mut video = Self {
			codecs: Arc::new(Codecs::new()),
			encoder: EncoderPreference::default(),
			codec: None,
			#[cfg(not(target_os = "android"))]
			openh264_dir: data_dir.join("openh264"),
			#[cfg(not(target_os = "android"))]
			h264: openh264::State::Off,
			sources: Vec::new(),
		};
		#[cfg(not(target_os = "android"))]
		video.set_h264(openh264);
		video
	}

	/// `base` with the codecs this machine can encode and decode: the stream
	/// codec (configured, or the encoder preference's first) offered first.
	pub fn peer_config(&self, mut base: PeerConfig) -> PeerConfig {
		if let Some(codec) = preferred_codec(&self.codecs, self.codec) {
			base.video_codecs = vec![media::video_codec(codec)];
		}
		peer_config(&self.codecs, base)
	}

	/// Use other encoders (`stream.hardware_acceleration`,
	/// `stream.encoder_backend`) and stream codec (`stream.codec`, `None`:
	/// automatic). A running share follows through
	/// `Streamer::reconfigure` with `StreamerConfigUpdate::encoder`.
	#[allow(dead_code, reason = "for the settings page")]
	pub fn set_encoder_preference(&mut self, encoder: EncoderPreference, codec: Option<Codec>) {
		let mut codecs = (*self.codecs).clone();
		codecs.set_preference(encoder.clone());
		self.codecs = Arc::new(codecs);
		self.encoder = encoder;
		self.codec = codec;
	}

	/// Every encoder backend, what works and why the rest does not.
	#[allow(dead_code, reason = "for the integrations page")]
	pub fn encoder_report(&self) -> EncoderReport {
		self.codecs.report()
	}

	/// Whether H.264 works, and a line for the settings page.
	pub fn h264_status(&self) -> (bool, String) {
		#[cfg(target_os = "android")]
		return (true, "H.264 uses the device's codecs.".into());
		#[cfg(not(target_os = "android"))]
		match &self.h264 {
			openh264::State::Off => (false, String::new()),
			openh264::State::Missing(reason) => (false, reason.clone()),
			openh264::State::Downloading => (false, "Downloading OpenH264 from Cisco…".into()),
			openh264::State::Loaded(path) => (
				true,
				format!("OpenH264 {} is loaded ({}).", openh264::OPENH264_VERSION, path.display()),
			),
		}
	}

	/// Enable or disable H.264; enabling loads a library downloaded earlier.
	/// Takes effect for new connections.
	pub fn set_h264(&mut self, enabled: bool) {
		#[cfg(target_os = "android")]
		let _ = enabled;
		#[cfg(not(target_os = "android"))]
		{
			use openh264::State;
			if !enabled {
				self.h264 = State::Off;
				self.codecs = Arc::new(Codecs::new().with_preference(self.encoder.clone()));
				return;
			}
			if matches!(self.h264, State::Loaded(_) | State::Downloading) {
				return;
			}
			match openh264::OpenH264::find_in(&self.openh264_dir) {
				Ok(library) => self.loaded(library),
				Err(_) => self.h264 = State::Missing("OpenH264 is not downloaded yet.".into()),
			}
		}
	}

	#[cfg(not(target_os = "android"))]
	fn loaded(&mut self, library: openh264::OpenH264) {
		self.h264 = openh264::State::Loaded(library.path().to_owned());
		self.codecs =
			Arc::new(Codecs::new().with_openh264(library).with_preference(self.encoder.clone()));
	}

	/// Download Cisco's OpenH264 (only on the user's request). `done` runs
	/// on the runtime; pass its argument to [`Video::downloaded`].
	pub fn download_h264(
		&mut self,
		runtime: &Handle,
		done: impl FnOnce(H264Download) + Send + 'static,
	) {
		#[cfg(target_os = "android")]
		let _ = (runtime, done);
		#[cfg(not(target_os = "android"))]
		{
			use openh264::State;
			if matches!(self.h264, State::Downloading | State::Loaded(_)) {
				return;
			}
			self.h264 = State::Downloading;
			let dir = self.openh264_dir.clone();
			runtime.spawn(async move {
				let result = openh264::download_openh264(&dir).await.map_err(|e| e.to_string());
				done(H264Download(result));
			});
		}
	}

	pub fn downloaded(&mut self, download: H264Download, enabled: bool) {
		#[cfg(target_os = "android")]
		let _ = (download, enabled);
		#[cfg(not(target_os = "android"))]
		match download.0 {
			Ok(library) if enabled => self.loaded(library),
			Ok(_) => self.h264 = openh264::State::Off,
			Err(e) => {
				warn!("OpenH264 download failed: {e}");
				self.h264 = openh264::State::Missing(format!("Download failed: {e}"));
			}
		}
	}

	/// Screens and windows to share, as (name, detail); the test pattern too
	/// when asked for. With `restorable` (a portal restore token from an
	/// earlier share) the portal comes twice: its dialog, which always asks,
	/// and the last choice again without the dialog.
	pub fn sources(
		&mut self,
		test_pattern: bool,
		restorable: bool,
	) -> Result<Vec<(String, String)>, String> {
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
		let (rows, entries) = share_rows(&sources, restorable);
		self.sources = entries;
		Ok(rows)
	}

	/// Start capturing source `index` of [`Video::sources`]; `done` runs on
	/// the runtime (the portal may wait for the user first).
	pub fn start_capture(
		&self,
		runtime: &Handle,
		index: usize,
		options: CaptureRequest,
		done: impl FnOnce(Result<Capture, String>) + Send + 'static,
	) {
		let Some((source, name, restore)) = self.sources.get(index).cloned() else {
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
			encoder: self.encoder.clone(),
			audio: options.audio,
			audio_sources: options.audio_sources,
			// The portal skips its dialog with a token: only when asked to.
			restore_token: options.restore_token.filter(|_| restore),
			..StreamerConfig::default()
		};
		runtime.spawn(async move {
			let result = Streamer::start(&codecs, config).await;
			done(result.map(|streamer| Capture { streamer, name }).map_err(|e| e.to_string()));
		});
	}

	/// The codecs, for reconfiguring a running streamer.
	pub fn codecs(&self) -> Arc<Codecs> {
		self.codecs.clone()
	}

	/// Encode the Stream Studio's composite ([`Streamer::start_studio`]):
	/// from the start, so its recording and replay buffer work before it
	/// goes live, and a stream attaches to it like to a capture. `done` runs
	/// on the runtime.
	pub fn start_studio(
		&self,
		runtime: &Handle,
		studio: Arc<Studio>,
		options: CaptureRequest,
		layers: Vec<LayerSpec>,
		done: impl FnOnce(Result<Streamer, String>) + Send + 'static,
	) {
		let peer = self.peer_config(PeerConfig::default());
		let Some(codec) = stream_codec(&self.codecs, &peer) else {
			done(Err("No video encoder is available.".into()));
			return;
		};
		let codecs = self.codecs.clone();
		let config = StreamerConfig {
			fps: options.fps,
			bitrate_kbps: options.bitrate_kbps,
			codec,
			encoder: self.encoder.clone(),
			audio: options.audio,
			audio_sources: options.audio_sources,
			layers,
			..StreamerConfig::default()
		};
		runtime.spawn(async move {
			let result = Streamer::start_studio(&codecs, config, studio).await;
			done(result.map_err(|e| e.to_string()));
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

	/// The test pattern through the encoder and decoder, without a server
	/// (`VOELIN_DEMO_STREAM`).
	pub fn demo(
		&self,
		runtime: &Handle,
		wake: impl Fn() + Send + Sync + 'static,
		done: impl FnOnce(Result<Decoder, String>) + Send + 'static,
	) {
		let pictures = Arc::new(Latest::new());
		let on_frame = deliver(pictures.clone(), wake);
		let codecs = self.codecs.clone();
		let config = StreamerConfig { source: SourceId::Synthetic, ..StreamerConfig::default() };
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

/// What a row of the share dialog starts: the source, its name, and whether
/// to restore the portal's last choice instead of asking.
type ShareEntry = (SourceId, String, bool);

/// The share dialog's rows (name, detail) for `sources`, and what each row
/// starts (restoring only in the extra portal row with `restorable`).
fn share_rows(
	sources: &[CaptureSource],
	restorable: bool,
) -> (Vec<(String, String)>, Vec<ShareEntry>) {
	let mut rows = Vec::with_capacity(sources.len() + 1);
	let mut entries = Vec::with_capacity(sources.len() + 1);
	for s in sources {
		let name = match s.id {
			SourceId::Portal => "Choose via system dialog".to_owned(),
			_ => s.name.clone(),
		};
		let detail = match (&s.id, s.primary) {
			(SourceId::Monitor(_), true) => format!("{} · primary", size_text(s.width, s.height)),
			(SourceId::Window(_), _) => {
				format!("window {}", size_text(s.width, s.height)).trim().to_owned()
			}
			_ => size_text(s.width, s.height),
		};
		rows.push((name, detail));
		entries.push((s.id.clone(), s.name.clone(), false));
		if s.id == SourceId::Portal && restorable {
			rows.push(("Same as last time".to_owned(), "without the dialog".to_owned()));
			entries.push((s.id.clone(), s.name.clone(), true));
		}
	}
	(rows, entries)
}

/// Share settings from the dialog.
pub(crate) struct CaptureRequest {
	pub fps: u32,
	pub bitrate_kbps: u32,
	pub audio: bool,
	/// Mixed into the stream's audio (`stream.audio_sources`).
	pub audio_sources: Vec<AudioSourceSpec>,
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
		self.streamer.audio_error()
	}

	/// For the audio mixer's rows: its levels and errors.
	pub fn streamer(&self) -> &Streamer {
		&self.streamer
	}

	/// Mix `sources` from now on (`stream.audio_sources` changed while
	/// sharing): a source whose kind stays keeps capturing with the new
	/// gain and mute, new ones start, removed ones stop. True if anything
	/// changed. A capture without sound stays so: its stream has no audio
	/// track to carry them.
	pub fn follow_audio(
		&self,
		codecs: &Codecs,
		sources: Vec<AudioSourceSpec>,
	) -> Result<bool, String> {
		if !self.has_audio() || self.streamer.audio_sources() == sources {
			return Ok(false);
		}
		let update = StreamerConfigUpdate { audio_sources: Some(sources), ..Default::default() };
		self.streamer.reconfigure(codecs, update).map(|()| true).map_err(|e| e.to_string())
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

	fn stats(&self) -> media::DecodeStats {
		match &self.source {
			Source::Viewer(v) => v.stats(),
			Source::Preview(p) => p.stats(),
		}
	}

	/// One line about the video: codec, size, problems.
	pub fn info(&self) -> String {
		let stats = self.stats();
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
		let stats = self.stats();
		stats.error.filter(|_| stats.decoded == 0)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// "Choose via system dialog" never passes the restore token, so the
	/// portal always asks; with a token the last choice gets its own row.
	#[test]
	fn the_portal_dialog_always_asks() {
		let portal = CaptureSource {
			id: SourceId::Portal,
			name: "Screen".into(),
			width: 0,
			height: 0,
			primary: false,
		};
		let (rows, entries) = share_rows(std::slice::from_ref(&portal), false);
		assert_eq!(rows.len(), 1);
		assert_eq!(rows[0].0, "Choose via system dialog");
		assert!(!entries[0].2);
		let (rows, entries) = share_rows(&[portal], true);
		let names: Vec<_> = rows.iter().map(|(name, _)| name.as_str()).collect();
		assert_eq!(names, ["Choose via system dialog", "Same as last time"]);
		let restore: Vec<_> = entries.iter().map(|e| e.2).collect();
		assert_eq!(restore, [false, true]);
	}

	/// A running share mixes what `stream.audio_sources` says now; one
	/// started without sound has no audio track to change.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_share_follows_its_audio_sources() {
		use voelin_core::media::AudioSourceKind;
		let codecs = Codecs::new();
		let tone = |hz| AudioSourceSpec::new(AudioSourceKind::Synthetic { hz });
		let start = async |audio| {
			let config = StreamerConfig {
				source: SourceId::Synthetic,
				synthetic_size: (64, 48),
				audio,
				audio_sources: vec![tone(440)],
				..StreamerConfig::default()
			};
			let streamer = Streamer::start(&codecs, config).await.unwrap();
			Capture { streamer, name: "Test pattern".into() }
		};
		let share = start(true).await;
		let mixed = vec![
			AudioSourceSpec { muted: true, ..tone(440) },
			AudioSourceSpec { gain: 0.5, ..tone(660) },
		];
		assert_eq!(share.follow_audio(&codecs, mixed.clone()), Ok(true));
		assert_eq!(share.streamer().audio_sources(), mixed);
		assert_eq!(share.follow_audio(&codecs, mixed), Ok(false), "nothing changed");
		// Every source taken out: silence, the track stays for new ones.
		assert_eq!(share.follow_audio(&codecs, Vec::new()), Ok(true));
		assert!(share.has_audio() && share.streamer().audio_sources().is_empty());
		assert_eq!(share.follow_audio(&codecs, vec![tone(330)]), Ok(true));
		assert_eq!(share.streamer().audio_sources(), [tone(330)]);

		let silent = start(false).await;
		assert_eq!(silent.follow_audio(&codecs, vec![tone(660)]), Ok(false));
		assert!(!silent.has_audio());
	}
}
