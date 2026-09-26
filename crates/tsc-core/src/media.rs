//! Stream media (feature `media`): capture and encoding for our stream,
//! decoding for the streams we watch.
//!
//! - [`Streamer`] captures a screen or window (and optionally system audio),
//!   encodes the video with the codec our offer carries ([`stream_codec`],
//!   VP8 by default) at the bitrate of the stream setup, and the audio with
//!   Opus (48 kHz stereo, 20 ms). It hands the frames to a [`MediaSink`]:
//!   the [`StreamSink`] of a live stream, or an [`EncodedSource`] for code
//!   that drives `tsc_stream::Streams` itself. Keyframe requests of viewers
//!   are honoured.
//! - [`VideoPipeline`] decodes the video of a watched stream on its own
//!   thread and hands every picture to a callback. After lost frames or
//!   decoder errors it skips to the next keyframe and asks for one.
//! - [`Viewer`] is a [`VideoPipeline`] fed from [`Engine::subscribe_frames`].
//!   The stream's audio needs nothing here: the session plays it
//!   ([`Command::SetStreamVolume`]).
//! - [`Latest`] hands the newest picture to a slower consumer such as a UI.
//! - [`LocalPreview`] runs capture → encoder → decoder without a server.
//!
//! [`tsc_media`] is re-exported for sources, codecs and pixel conversion.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use tsc_audio::{VoiceCodec, VoiceEncoder};
pub use tsc_media;
use tsc_media::capture::synthetic::{SineSource, SyntheticScreen};
use tsc_media::capture::{
	self, AudioCapture, CaptureOptions, CaptureSource, ScreenCapture, SourceId,
};
use tsc_media::{
	AudioBuffer, Codec, Codecs, ContentHint, EncoderConfig, FrameReceiver, VideoEncoder, VideoFrame,
};
use tsc_stream::{
	EncodedFrame, FrameSource, Frequency, MediaFrame, MediaKind, MediaTime, PeerConfig, VideoCodec,
};

use crate::stream::StreamSink;
use crate::{Command, Engine, SessionId};

/// How long media threads wait for input before checking whether to stop.
const POLL: Duration = Duration::from_millis(100);
/// Samples per channel in one 20 ms Opus frame at 48 kHz.
const OPUS_FRAME: usize = 960;
/// Opus bitrate of stream audio (stereo, music).
const OPUS_BITRATE: i32 = 128_000;
/// Video frames the decoder may fall behind before the queue is dropped.
pub const MAX_QUEUED: usize = 30;
/// Keyframe requests while waiting for one are at least this far apart.
const KEYFRAME_RETRY: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
	#[error(transparent)]
	Media(#[from] tsc_media::Error),
	#[error("Opus: {0}")]
	Opus(#[from] tsc_audio::Error),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, MediaError> {
	std::thread::Builder::new()
		.name(name.to_owned())
		.spawn(f)
		.map_err(|e| MediaError::Media(e.into()))
}

/// The `tsc_media` codec of a negotiated video codec.
pub fn media_codec(codec: VideoCodec) -> Codec {
	match codec {
		VideoCodec::Vp8 => Codec::Vp8,
		VideoCodec::Vp9 => Codec::Vp9,
		VideoCodec::H264 => Codec::H264,
		VideoCodec::Av1 => Codec::Av1,
	}
}

/// The negotiable video codec of a `tsc_media` codec.
pub fn video_codec(codec: Codec) -> VideoCodec {
	match codec {
		Codec::Vp8 => VideoCodec::Vp8,
		Codec::Vp9 => VideoCodec::Vp9,
		Codec::H264 => VideoCodec::H264,
		Codec::Av1 => VideoCodec::Av1,
	}
}

/// The codec our stream is encoded with: the first codec of the offer
/// (`config.video_codecs`) that we can encode.
pub fn stream_codec(codecs: &Codecs, config: &PeerConfig) -> Option<Codec> {
	let encodable = codecs.encoder_codecs();
	config.video_codecs.iter().map(|c| media_codec(*c)).find(|c| encodable.contains(c))
}

/// `config` adjusted to what `codecs` can do: viewers accept only codecs we
/// decode, and a streamer offers only the codec it encodes (every viewer
/// gets the same frames, and a viewer picks from the offer).
pub fn peer_config(codecs: &Codecs, mut config: PeerConfig) -> PeerConfig {
	config.accept_video_codecs = codecs.decoders().into_iter().map(video_codec).collect();
	if let Some(codec) = stream_codec(codecs, &config) {
		config.video_codecs = vec![video_codec(codec)];
	}
	config
}

/// Monitors and windows of this session's capture backend. The portal (on
/// Wayland) lists a single entry: its own dialog picks.
pub fn screen_sources() -> Result<Vec<CaptureSource>, MediaError> {
	Ok(capture::default_screen_capture()?.sources()?)
}

/// The synthetic test pattern as a source.
pub fn test_pattern_source() -> CaptureSource {
	let (width, height) = StreamerConfig::default().synthetic_size;
	CaptureSource {
		id: SourceId::Synthetic,
		name: "Test pattern".into(),
		width,
		height,
		primary: false,
	}
}

/// Where a [`Streamer`] puts encoded frames.
pub trait MediaSink: Send + Sync {
	/// Send one frame; `false` once nobody takes frames any more.
	fn send(&self, frame: EncodedFrame) -> bool;

	/// Whether a keyframe was asked for since the last call.
	fn take_keyframe_request(&self) -> bool;
}

impl MediaSink for StreamSink {
	fn send(&self, frame: EncodedFrame) -> bool {
		StreamSink::send(self, frame)
	}

	fn take_keyframe_request(&self) -> bool {
		StreamSink::take_keyframe_request(self)
	}
}

/// Which backend captures monitors and windows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CaptureBackend {
	/// The one for this desktop session (`capture::default_screen_capture`).
	#[default]
	Auto,
	/// X11, also under Wayland through XWayland (Linux).
	X11,
}

/// What and how to stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamerConfig {
	pub source: SourceId,
	/// For monitors and windows.
	pub backend: CaptureBackend,
	/// Frames per second (1 to 60).
	pub fps: u32,
	/// Video bitrate in kbit/s, as in `StreamSetup::bitrate`.
	pub bitrate_kbps: u32,
	/// The codec of our offer, see [`stream_codec`].
	pub codec: Codec,
	/// Capture and send system audio (a sine tone with the test pattern).
	pub audio: bool,
	pub cursor: bool,
	/// Size of the test pattern ([`SourceId::Synthetic`]).
	pub synthetic_size: (u32, u32),
	/// Portal restore token from an earlier share: the desktop may skip its
	/// dialog. The new one is [`Streamer::restore_token`].
	pub restore_token: Option<String>,
}

impl Default for StreamerConfig {
	fn default() -> Self {
		Self {
			source: SourceId::Monitor(0),
			backend: CaptureBackend::Auto,
			fps: 30,
			bitrate_kbps: 4608,
			codec: Codec::Vp8,
			audio: true,
			cursor: true,
			synthetic_size: (1280, 720),
			restore_token: None,
		}
	}
}

/// What a [`Streamer`] has done so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamerStats {
	/// Frames handed to the sink.
	pub video_frames: u64,
	pub audio_frames: u64,
	/// Size of the last captured frame.
	pub width: u32,
	pub height: u32,
	/// The capture stopped by itself (window closed, sharing ended in the
	/// desktop's UI, ...).
	pub capture_ended: bool,
	/// The last encoder error.
	pub error: Option<String>,
}

#[derive(Default)]
struct Shared {
	sink: Mutex<Option<Arc<dyn MediaSink>>>,
	stop: AtomicBool,
	video_frames: AtomicU64,
	audio_frames: AtomicU64,
	/// `width << 32 | height` of the last captured frame.
	size: AtomicU64,
	capture_ended: AtomicBool,
	error: Mutex<Option<String>>,
}

impl Shared {
	fn sink(&self) -> Option<Arc<dyn MediaSink>> {
		lock(&self.sink).clone()
	}

	fn stopped(&self) -> bool {
		self.stop.load(Ordering::Relaxed)
	}
}

/// Capture and encoding for our stream. Frames are captured from the start
/// (the portal asks the user then), but only encoded once a sink is
/// [attached](Streamer::attach). Stops when dropped.
pub struct Streamer {
	shared: Arc<Shared>,
	screen: Option<Box<dyn ScreenCapture>>,
	audio: Option<Box<dyn AudioCapture>>,
	threads: Vec<JoinHandle<()>>,
	backend: &'static str,
	restore_token: Option<String>,
	audio_error: Option<String>,
}

impl Streamer {
	/// Start capturing. Must run on a Tokio runtime (the portal talks D-Bus
	/// on it); may wait for the user in the portal's dialog.
	pub async fn start(codecs: &Codecs, config: StreamerConfig) -> Result<Self, MediaError> {
		let fps = config.fps.clamp(1, 60);
		let encoder = codecs.new_encoder(
			config.codec,
			EncoderConfig {
				fps,
				bitrate_bps: config.bitrate_kbps.clamp(100, 10_000) * 1000,
				content: ContentHint::Screen,
				..EncoderConfig::default()
			},
		)?;
		let options = CaptureOptions { fps, cursor: config.cursor, ..CaptureOptions::default() };
		let mut restore_token = None;
		let (screen, frames): (Box<dyn ScreenCapture>, _) = match &config.source {
			SourceId::Synthetic => {
				let (w, h) = config.synthetic_size;
				let mut screen = SyntheticScreen::new(w, h);
				let frames = screen.start(&SourceId::Synthetic, &options).await?;
				(Box::new(screen), frames)
			}
			#[cfg(target_os = "linux")]
			SourceId::Portal => {
				use tsc_media::capture::portal::PortalCapture;
				let mut portal = PortalCapture::with_restore_token(config.restore_token.clone());
				let frames = portal.start(&SourceId::Portal, &options).await?;
				restore_token = portal.restore_token().map(str::to_owned);
				(Box::new(portal), frames)
			}
			source => {
				let mut screen = match config.backend {
					CaptureBackend::Auto => capture::default_screen_capture()?,
					#[cfg(target_os = "linux")]
					CaptureBackend::X11 => Box::new(tsc_media::capture::x11::X11Capture::new()),
					#[cfg(not(target_os = "linux"))]
					CaptureBackend::X11 => {
						return Err(tsc_media::Error::CaptureUnavailable {
							backend: "x11",
							reason: "X11 capture is only built on Linux".into(),
						}
						.into());
					}
				};
				let frames = screen.start(source, &options).await?;
				(screen, frames)
			}
		};
		let backend = screen.backend();

		let shared = Arc::new(Shared::default());
		let mut streamer = Self {
			shared: shared.clone(),
			screen: Some(screen),
			audio: None,
			threads: Vec::new(),
			backend,
			restore_token,
			audio_error: None,
		};
		streamer.threads.push(spawn("tsc-stream-video", {
			let shared = shared.clone();
			move || video_loop(&shared, frames, encoder)
		})?);
		if config.audio {
			match start_audio(&config.source) {
				Ok((capture, buffers, encoder)) => {
					streamer.audio = Some(capture);
					streamer.threads.push(spawn("tsc-stream-audio", move || {
						audio_loop(&shared, buffers, encoder)
					})?);
				}
				Err(e) => {
					warn!("streaming without audio: {e}");
					streamer.audio_error = Some(e.to_string());
				}
			}
		}
		Ok(streamer)
	}

	/// Start encoding into `sink` (replacing an earlier one).
	pub fn attach(&self, sink: Arc<dyn MediaSink>) {
		*lock(&self.shared.sink) = Some(sink);
	}

	/// Stop encoding; capture goes on.
	pub fn detach(&self) {
		*lock(&self.shared.sink) = None;
	}

	/// The capture backend (`"x11"`, `"portal"`, `"synthetic"`, ...).
	pub fn backend(&self) -> &'static str {
		self.backend
	}

	/// The portal's token for this choice of screen, to store for next time.
	pub fn restore_token(&self) -> Option<&str> {
		self.restore_token.as_deref()
	}

	/// Why system audio is not captured, if it was asked for.
	pub fn audio_error(&self) -> Option<&str> {
		self.audio_error.as_deref()
	}

	/// Whether system audio is captured.
	pub fn has_audio(&self) -> bool {
		self.audio.is_some()
	}

	pub fn stats(&self) -> StreamerStats {
		let size = self.shared.size.load(Ordering::Relaxed);
		StreamerStats {
			video_frames: self.shared.video_frames.load(Ordering::Relaxed),
			audio_frames: self.shared.audio_frames.load(Ordering::Relaxed),
			width: (size >> 32) as u32,
			height: size as u32,
			capture_ended: self.shared.capture_ended.load(Ordering::Relaxed),
			error: lock(&self.shared.error).clone(),
		}
	}

	/// Stop capturing and encoding.
	pub fn stop(&mut self) {
		self.shared.stop.store(true, Ordering::Relaxed);
		self.detach();
		if let Some(mut screen) = self.screen.take() {
			screen.stop();
		}
		if let Some(mut audio) = self.audio.take() {
			audio.stop();
		}
		for thread in self.threads.drain(..) {
			let _ = thread.join();
		}
	}
}

impl Drop for Streamer {
	fn drop(&mut self) {
		self.stop();
	}
}

type AudioStart = (Box<dyn AudioCapture>, FrameReceiver<AudioBuffer>, VoiceEncoder);

fn start_audio(source: &SourceId) -> Result<AudioStart, MediaError> {
	let mut encoder = VoiceEncoder::new(VoiceCodec::Music)?;
	encoder.set_bitrate(OPUS_BITRATE)?;
	let mut capture: Box<dyn AudioCapture> = match source {
		SourceId::Synthetic => Box::new(SineSource::new(440.0, 0.05)),
		_ => capture::default_audio_capture()?,
	};
	let buffers = capture.start()?;
	Ok((capture, buffers, encoder))
}

fn video_loop(
	shared: &Shared,
	mut frames: FrameReceiver<VideoFrame>,
	mut encoder: Box<dyn VideoEncoder>,
) {
	// A requested keyframe the encoder has not produced yet (rate control
	// may skip a frame).
	let mut keyframe_due = false;
	while !shared.stopped() {
		let Some(frame) = frames.recv_timeout(POLL) else {
			if frames.is_closed() {
				debug!("screen capture ended");
				shared.capture_ended.store(true, Ordering::Relaxed);
				break;
			}
			continue;
		};
		let size = u64::from(frame.width) << 32 | u64::from(frame.height);
		shared.size.store(size, Ordering::Relaxed);
		let Some(sink) = shared.sink() else { continue };
		let keyframe = sink.take_keyframe_request() || keyframe_due;
		match encoder.encode(&frame, keyframe) {
			Ok(encoded) => {
				keyframe_due = keyframe && !encoded.iter().any(|f| f.keyframe);
				for f in encoded {
					let frame = EncodedFrame {
						kind: MediaKind::Video,
						time: MediaTime::from_90khz(f.pts_90khz),
						data: f.data.into(),
					};
					if sink.send(frame) {
						shared.video_frames.fetch_add(1, Ordering::Relaxed);
					}
				}
			}
			Err(e) => {
				warn!("video encoding failed: {e}");
				keyframe_due = keyframe;
				*lock(&shared.error) = Some(e.to_string());
			}
		}
	}
}

/// Interleaved stereo from any channel count (extra channels are dropped).
fn append_stereo(buffer: &AudioBuffer, out: &mut Vec<f32>) {
	match buffer.channels {
		0 => {}
		1 => out.extend(buffer.samples.iter().flat_map(|s| [*s, *s])),
		2 => out.extend_from_slice(&buffer.samples),
		n => {
			for frame in buffer.samples.chunks_exact(usize::from(n)) {
				out.extend_from_slice(&frame[..2]);
			}
		}
	}
}

fn audio_loop(shared: &Shared, mut buffers: FrameReceiver<AudioBuffer>, mut encoder: VoiceEncoder) {
	// Interleaved stereo waiting for a whole Opus frame.
	let mut pending: Vec<f32> = Vec::new();
	// Opus frames since the capture started: the RTP time in 20 ms steps.
	let mut frames: u64 = 0;
	while !shared.stopped() {
		let Some(buffer) = buffers.recv_timeout(POLL) else {
			if buffers.is_closed() {
				debug!("system audio capture ended");
				break;
			}
			continue;
		};
		// Follow the capture clock across gaps (nothing played, sink not
		// attached yet), so audio stays in step with video.
		let at = buffer.timestamp.as_micros() as u64 * 48 / 1000;
		let position = frames * OPUS_FRAME as u64 + (pending.len() / 2) as u64;
		if at > position + 10 * OPUS_FRAME as u64 {
			pending.clear();
			frames = at / OPUS_FRAME as u64;
		}
		let Some(sink) = shared.sink() else { continue };
		append_stereo(&buffer, &mut pending);
		while pending.len() >= OPUS_FRAME * 2 {
			let data: Option<Arc<[u8]>> = match encoder.encode_to_bytes(&pending[..OPUS_FRAME * 2])
			{
				Ok(data) => Some(data.into()),
				Err(e) => {
					warn!("audio encoding failed: {e}");
					None
				}
			};
			pending.drain(..OPUS_FRAME * 2);
			if let Some(data) = data {
				let time = MediaTime::new(frames * OPUS_FRAME as u64, Frequency::FORTY_EIGHT_KHZ);
				if sink.send(EncodedFrame { kind: MediaKind::Audio, time, data }) {
					shared.audio_frames.fetch_add(1, Ordering::Relaxed);
				}
			}
			frames += 1;
		}
	}
}

/// Collects a [`Streamer`]'s frames for [`EncodedSource`].
struct ChannelSink {
	tx: std_mpsc::Sender<EncodedFrame>,
	keyframe: AtomicBool,
}

impl MediaSink for ChannelSink {
	fn send(&self, frame: EncodedFrame) -> bool {
		self.tx.send(frame).is_ok()
	}

	fn take_keyframe_request(&self) -> bool {
		self.keyframe.swap(false, Ordering::Relaxed)
	}
}

/// A [`Streamer`] as a [`FrameSource`], for code that runs
/// `tsc_stream::Streams` itself (e.g. tsctl).
pub struct EncodedSource {
	streamer: Streamer,
	frames: std_mpsc::Receiver<EncodedFrame>,
	sink: Arc<ChannelSink>,
}

impl EncodedSource {
	pub fn new(streamer: Streamer) -> Self {
		let (tx, frames) = std_mpsc::channel();
		let sink = Arc::new(ChannelSink { tx, keyframe: AtomicBool::new(true) });
		streamer.attach(sink.clone());
		Self { streamer, frames, sink }
	}

	pub fn streamer(&self) -> &Streamer {
		&self.streamer
	}
}

impl FrameSource for EncodedSource {
	fn poll_frames(&mut self, _now: Instant, out: &mut Vec<EncodedFrame>) {
		out.extend(self.frames.try_iter());
	}

	fn request_keyframe(&mut self) {
		self.sink.keyframe.store(true, Ordering::Relaxed);
	}
}

/// Whether `data` (one depacketized frame) starts a picture that decodes on
/// its own. Unknown formats count as keyframes, so the decoder gets to try.
pub fn is_keyframe(codec: Codec, data: &[u8]) -> bool {
	match codec {
		// Frame tag: bit 0 is 0 for key frames.
		Codec::Vp8 => data.first().is_some_and(|b| b & 1 == 0),
		Codec::Vp9 => vp9_is_keyframe(data),
		Codec::H264 => h264_nal_types(data).any(|t| t == 5 || t == 7),
		Codec::Av1 => av1_has_sequence_header(data).unwrap_or(true),
	}
}

/// VP9 uncompressed header: frame marker, profile, show_existing_frame,
/// frame_type (0 = key frame).
fn vp9_is_keyframe(data: &[u8]) -> bool {
	let Some(&b) = data.first() else { return false };
	if b >> 6 != 2 {
		return false;
	}
	let profile = ((b >> 5) & 1) | (((b >> 4) & 1) << 1);
	// Profile 3 has a reserved bit after the profile.
	let mut bit = if profile == 3 { 3 } else { 4 };
	let read = |bit: u32| (u32::from(b) >> (7 - bit)) & 1;
	let show_existing = read(bit);
	if show_existing == 1 {
		return false;
	}
	bit += 1;
	read(bit) == 0
}

/// NAL unit types of an Annex B H.264 frame.
fn h264_nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
	data.windows(4).filter_map(|w| (w[..3] == [0, 0, 1]).then_some(w[3] & 0x1f))
}

/// Whether an AV1 temporal unit carries a sequence header OBU (sent with
/// key frames); `None` if the OBUs cannot be parsed.
fn av1_has_sequence_header(mut data: &[u8]) -> Option<bool> {
	while let Some(&header) = data.first() {
		let obu_type = (header >> 3) & 0x0f;
		if obu_type == 1 {
			return Some(true);
		}
		let extension = header & 0x04 != 0;
		let has_size = header & 0x02 != 0;
		let mut pos = 1 + usize::from(extension);
		if !has_size {
			return Some(false);
		}
		// LEB128 size.
		let mut size = 0usize;
		for i in 0..8 {
			let byte = *data.get(pos)?;
			pos += 1;
			size |= usize::from(byte & 0x7f) << (7 * i);
			if byte & 0x80 == 0 {
				break;
			}
		}
		data = data.get(pos + size..)?;
	}
	Some(false)
}

/// Decoding statistics of a [`VideoPipeline`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DecodeStats {
	/// Pictures handed to the callback.
	pub decoded: u64,
	/// Frames skipped while waiting for a keyframe.
	pub skipped: u64,
	pub keyframe_requests: u64,
	pub codec: Option<Codec>,
	pub width: u32,
	pub height: u32,
	/// The last error (no decoder for the codec, decoding failed).
	pub error: Option<String>,
}

#[derive(Default)]
struct QueueState {
	frames: VecDeque<MediaFrame>,
	/// Frames were dropped: wait for a keyframe.
	lost: bool,
	closed: bool,
}

#[derive(Default)]
struct DecodeQueue {
	state: Mutex<QueueState>,
	cond: Condvar,
	stats: Mutex<DecodeStats>,
}

/// Feeds frames into a [`VideoPipeline`]; cheap to clone.
#[derive(Clone)]
pub struct FrameInput {
	queue: Arc<DecodeQueue>,
}

impl FrameInput {
	/// Queue a video frame (audio frames are ignored). When the decoder
	/// falls more than [`MAX_QUEUED`] frames behind, the queue is dropped
	/// and decoding resumes at the next keyframe.
	pub fn push(&self, frame: MediaFrame) {
		if frame.kind != MediaKind::Video {
			return;
		}
		let mut state = lock(&self.queue.state);
		if state.frames.len() >= MAX_QUEUED {
			state.frames.clear();
			state.lost = true;
		}
		state.frames.push_back(frame);
		drop(state);
		self.queue.cond.notify_one();
	}

	/// Frames were lost before they reached us (e.g. a lagging receiver).
	pub fn lost(&self) {
		lock(&self.queue.state).lost = true;
	}
}

/// Decodes the video frames of one stream on its own thread. See the
/// [module docs](self).
pub struct VideoPipeline {
	input: FrameInput,
	thread: Option<JoinHandle<()>>,
}

impl VideoPipeline {
	/// `on_frame` gets every decoded picture on the decoding thread;
	/// `request_keyframe` is called when the decoder needs one (at most
	/// every half second).
	pub fn new(
		codecs: Arc<Codecs>,
		on_frame: impl FnMut(VideoFrame) + Send + 'static,
		request_keyframe: impl Fn() + Send + 'static,
	) -> Self {
		let queue = Arc::new(DecodeQueue::default());
		let thread = std::thread::Builder::new()
			.name("tsc-stream-decode".into())
			.spawn({
				let queue = queue.clone();
				move || decode_loop(&queue, &codecs, on_frame, request_keyframe)
			})
			.map_err(|e| warn!("cannot start the video decoder: {e}"))
			.ok();
		Self { input: FrameInput { queue }, thread }
	}

	/// Where frames go in.
	pub fn input(&self) -> FrameInput {
		self.input.clone()
	}

	pub fn push(&self, frame: MediaFrame) {
		self.input.push(frame);
	}

	pub fn stats(&self) -> DecodeStats {
		lock(&self.input.queue.stats).clone()
	}
}

impl Drop for VideoPipeline {
	fn drop(&mut self) {
		lock(&self.input.queue.state).closed = true;
		self.input.queue.cond.notify_all();
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

fn decode_loop(
	queue: &DecodeQueue,
	codecs: &Codecs,
	mut on_frame: impl FnMut(VideoFrame),
	request_keyframe: impl Fn(),
) {
	let mut decoder: Option<(Codec, Box<dyn tsc_media::VideoDecoder>)> = None;
	// Start at a keyframe; the streamer sends one when a viewer connects.
	let mut waiting = true;
	let mut last_request: Option<Instant> = None;
	let mut request = |stats: &Mutex<DecodeStats>| {
		if last_request.is_none_or(|t| t.elapsed() >= KEYFRAME_RETRY) {
			last_request = Some(Instant::now());
			lock(stats).keyframe_requests += 1;
			request_keyframe();
		}
	};
	let set_error = |message: String| lock(&queue.stats).error = Some(message);
	loop {
		let (frame, lost) = {
			let mut state = lock(&queue.state);
			loop {
				if state.closed {
					return;
				}
				if let Some(frame) = state.frames.pop_front() {
					break (frame, std::mem::take(&mut state.lost));
				}
				state =
					queue.cond.wait_timeout(state, POLL).unwrap_or_else(PoisonError::into_inner).0;
			}
		};
		let codec = match Codec::try_from(frame.codec) {
			Ok(codec) => codec,
			Err(e) => {
				set_error(e.to_string());
				continue;
			}
		};
		if decoder.as_ref().is_none_or(|(c, _)| *c != codec) {
			match codecs.new_decoder(codec) {
				Ok(d) => {
					decoder = Some((codec, d));
					waiting = true;
					lock(&queue.stats).codec = Some(codec);
				}
				Err(e) => {
					if decoder.is_some() || lock(&queue.stats).error.is_none() {
						warn!("no decoder for the stream: {e}");
					}
					decoder = None;
					set_error(e.to_string());
					continue;
				}
			}
		}
		if lost || !frame.contiguous {
			waiting = true;
		}
		if waiting {
			if !is_keyframe(codec, &frame.data) {
				lock(&queue.stats).skipped += 1;
				request(&queue.stats);
				continue;
			}
			waiting = false;
		}
		let Some((_, dec)) = &mut decoder else { continue };
		match dec.decode(&frame.data) {
			Ok(Some(picture)) => {
				{
					let mut stats = lock(&queue.stats);
					stats.decoded += 1;
					(stats.width, stats.height) = (picture.width, picture.height);
				}
				on_frame(picture);
			}
			Ok(None) => {}
			Err(e) => {
				debug!("decoding failed: {e}");
				set_error(e.to_string());
				waiting = true;
				request(&queue.stats);
			}
		}
	}
}

/// Decodes a stream watched through the engine: a [`VideoPipeline`] fed from
/// [`Engine::subscribe_frames`], asking the streamer for keyframes through
/// the engine. Stops when dropped.
pub struct Viewer {
	pipeline: VideoPipeline,
	forward: tokio::task::JoinHandle<()>,
}

impl Viewer {
	pub fn start(
		engine: &Engine,
		session: SessionId,
		stream_id: &str,
		codecs: Arc<Codecs>,
		on_frame: impl FnMut(VideoFrame) + Send + 'static,
	) -> Self {
		let request = {
			let engine = engine.clone();
			let stream_id = stream_id.to_owned();
			move || {
				let stream_id = stream_id.clone();
				engine.send(Command::RequestStreamKeyframe { session, stream_id });
			}
		};
		let pipeline = VideoPipeline::new(codecs, on_frame, request);
		let input = pipeline.input();
		let mut frames = engine.subscribe_frames();
		let stream_id = stream_id.to_owned();
		let forward = engine.runtime().spawn(async move {
			use tokio::sync::broadcast::error::RecvError;
			loop {
				match frames.recv().await {
					Ok(f) if f.session == session && f.stream_id == stream_id => {
						input.push(f.frame)
					}
					Ok(_) => {}
					Err(RecvError::Lagged(_)) => input.lost(),
					Err(RecvError::Closed) => break,
				}
			}
		});
		Self { pipeline, forward }
	}

	pub fn stats(&self) -> DecodeStats {
		self.pipeline.stats()
	}
}

impl Drop for Viewer {
	fn drop(&mut self) {
		self.forward.abort();
	}
}

/// Hands the newest item from a producer thread to a consumer that may be
/// slower (e.g. a UI): an item that was not taken yet is replaced.
pub struct Latest<T> {
	slot: Mutex<Option<T>>,
}

impl<T> Default for Latest<T> {
	fn default() -> Self {
		Self { slot: Mutex::new(None) }
	}
}

impl<T> Latest<T> {
	pub fn new() -> Self {
		Self::default()
	}

	/// Store `item`. Returns `true` if the slot was empty, i.e. the consumer
	/// needs a wake-up; otherwise a wake-up is already pending.
	pub fn put(&self, item: T) -> bool {
		lock(&self.slot).replace(item).is_none()
	}

	pub fn take(&self) -> Option<T> {
		lock(&self.slot).take()
	}
}

/// Feeds a [`Streamer`]'s video straight into a [`VideoPipeline`].
struct PipelineSink {
	input: FrameInput,
	codec: tsc_stream::Codec,
	keyframe: Arc<AtomicBool>,
}

impl MediaSink for PipelineSink {
	fn send(&self, frame: EncodedFrame) -> bool {
		self.input.push(MediaFrame {
			kind: frame.kind,
			codec: self.codec,
			time: frame.time,
			network_time: Instant::now(),
			contiguous: true,
			data: frame.data,
		});
		true
	}

	fn take_keyframe_request(&self) -> bool {
		self.keyframe.swap(false, Ordering::Relaxed)
	}
}

/// Capture → encoder → decoder without a server, e.g. to preview a share or
/// to develop the viewer (`TSC_DEMO_STREAM` in the desktop app).
pub struct LocalPreview {
	// Dropped first: no more frames into the pipeline.
	streamer: Streamer,
	pipeline: VideoPipeline,
}

impl LocalPreview {
	pub async fn start(
		codecs: Arc<Codecs>,
		mut config: StreamerConfig,
		on_frame: impl FnMut(VideoFrame) + Send + 'static,
	) -> Result<Self, MediaError> {
		// The audio would go nowhere.
		config.audio = false;
		let codec = config.codec;
		let streamer = Streamer::start(&codecs, config).await?;
		let keyframe = Arc::new(AtomicBool::new(true));
		let pipeline = VideoPipeline::new(codecs, on_frame, {
			let keyframe = keyframe.clone();
			move || keyframe.store(true, Ordering::Relaxed)
		});
		let input = pipeline.input();
		streamer.attach(Arc::new(PipelineSink { input, codec: codec.into(), keyframe }));
		Ok(Self { streamer, pipeline })
	}

	pub fn streamer(&self) -> &Streamer {
		&self.streamer
	}

	pub fn stats(&self) -> DecodeStats {
		self.pipeline.stats()
	}
}

/// The first and last column of the rectangle in the middle row of a
/// decoded test pattern. Checks that pixels further than two columns from
/// its edges (which the codec blurs) have the colour of the rectangle or
/// the background.
#[cfg(test)]
pub(crate) fn rectangle_span(picture: &VideoFrame) -> (u32, u32) {
	use tsc_media::capture::synthetic::{BACKGROUND, RECT_COLOR};

	let rgba = tsc_media::convert::to_rgba_vec(picture).unwrap();
	let y = picture.height / 2;
	let pixel = |x: u32| {
		let i = ((y * picture.width + x) * 4) as usize;
		[rgba[i], rgba[i + 1], rgba[i + 2]]
	};
	let close = |p: [u8; 3], c: [u8; 3]| p.iter().zip(c).all(|(a, b)| a.abs_diff(b) <= 24);
	let inside: Vec<u32> = (0..picture.width).filter(|&x| close(pixel(x), RECT_COLOR)).collect();
	let (&a, &b) = inside.first().zip(inside.last()).expect("no rectangle in the middle row");
	for x in 0..picture.width {
		if x + 2 < a || x > b + 2 {
			assert!(close(pixel(x), BACKGROUND), "pixel {x},{y} is {:?}", pixel(x));
		} else if (a + 2..=b.saturating_sub(2)).contains(&x) {
			assert!(close(pixel(x), RECT_COLOR), "pixel {x},{y} is {:?}", pixel(x));
		}
	}
	(a, b)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keyframe_detection() {
		// VP8: the 1x1 keyframe of tsc-stream's synthetic source.
		assert!(is_keyframe(Codec::Vp8, &tsc_stream::source::VP8_KEYFRAME_1X1));
		assert!(!is_keyframe(Codec::Vp8, &[0x31, 0x00]));
		// VP9 profile 0: marker 10, profile 00, show_existing 0, frame_type 0/1.
		assert!(is_keyframe(Codec::Vp9, &[0b1000_0000]));
		assert!(!is_keyframe(Codec::Vp9, &[0b1000_0100]));
		assert!(!is_keyframe(Codec::Vp9, &[0b1000_1000]), "show existing frame");
		// H.264: SPS + IDR vs. a non-IDR slice.
		assert!(is_keyframe(Codec::H264, &[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2]));
		assert!(!is_keyframe(Codec::H264, &[0, 0, 0, 1, 0x41, 1, 2]));
		// AV1: temporal delimiter (type 2, size 0) then a sequence header.
		assert!(is_keyframe(Codec::Av1, &[0x12, 0x00, 0x0a, 0x01, 0xff]));
		assert!(!is_keyframe(Codec::Av1, &[0x12, 0x00, 0x32, 0x01, 0xff]));
		assert!(is_keyframe(Codec::Av1, &[0x12, 0x05]), "truncated: let the decoder try");
	}

	#[test]
	fn latest_keeps_the_newest() {
		let latest = Latest::new();
		assert!(latest.put(1));
		assert!(!latest.put(2), "a wake-up is pending");
		assert_eq!(latest.take(), Some(2));
		assert_eq!(latest.take(), None);
		assert!(latest.put(3));
	}

	#[test]
	fn peer_config_follows_codecs() {
		let codecs = Codecs::new();
		let config = peer_config(&codecs, PeerConfig::default());
		assert_eq!(config.video_codecs, [VideoCodec::Vp8]);
		assert!(config.accept_video_codecs.contains(&VideoCodec::Vp8));
		assert!(!config.accept_video_codecs.contains(&VideoCodec::H264), "no OpenH264 loaded");
		assert_eq!(stream_codec(&codecs, &config), Some(Codec::Vp8));
		let h264_only = PeerConfig { video_codecs: vec![VideoCodec::H264], ..config };
		assert_eq!(stream_codec(&codecs, &h264_only), None);
	}

	/// The test pattern through VP8 and back, without a network: pictures
	/// show the moving rectangle.
	#[tokio::test(flavor = "multi_thread")]
	async fn local_preview_decodes_the_pattern() {
		let codecs = Arc::new(Codecs::new());
		let config = StreamerConfig {
			source: SourceId::Synthetic,
			synthetic_size: (320, 240),
			bitrate_kbps: 1500,
			..StreamerConfig::default()
		};
		let (tx, rx) = std_mpsc::channel();
		let preview = LocalPreview::start(codecs, config, move |picture| {
			let _ = tx.send(picture);
		})
		.await
		.unwrap();
		let mut spans = Vec::new();
		while spans.len() < 10 {
			let picture = rx.recv_timeout(Duration::from_secs(5)).expect("no picture");
			assert_eq!((picture.width, picture.height), (320, 240));
			spans.push(rectangle_span(&picture));
		}
		// 320 / 5 = 64 pixels wide, give or take the codec's blur.
		for (a, b) in &spans {
			assert!((60..=68).contains(&(b - a + 1)), "{spans:?}");
		}
		assert_ne!(spans.first(), spans.last(), "the rectangle moves");
		let stats = preview.stats();
		assert!(stats.decoded >= 10 && stats.error.is_none(), "{stats:?}");
		assert_eq!(stats.codec, Some(Codec::Vp8));
		assert_eq!(preview.streamer().backend(), "synthetic");
	}

	/// Audio of the test pattern: 20 ms Opus frames on a 48 kHz clock.
	#[tokio::test(flavor = "multi_thread")]
	async fn streamer_sends_opus() {
		let codecs = Codecs::new();
		let config = StreamerConfig {
			source: SourceId::Synthetic,
			synthetic_size: (64, 48),
			..StreamerConfig::default()
		};
		let streamer = Streamer::start(&codecs, config).await.unwrap();
		assert!(streamer.has_audio(), "{:?}", streamer.audio_error());
		let mut source = EncodedSource::new(streamer);
		let mut frames = Vec::new();
		let deadline = Instant::now() + Duration::from_secs(5);
		while frames.iter().filter(|f: &&EncodedFrame| f.kind == MediaKind::Audio).count() < 10 {
			assert!(Instant::now() < deadline, "no audio");
			tokio::time::sleep(Duration::from_millis(20)).await;
			source.poll_frames(Instant::now(), &mut frames);
		}
		let times: Vec<u64> = frames
			.iter()
			.filter(|f| f.kind == MediaKind::Audio)
			.map(|f| f.time.rebase(Frequency::FORTY_EIGHT_KHZ).numer())
			.collect();
		assert!(times.windows(2).all(|w| w[1] - w[0] == OPUS_FRAME as u64), "{times:?}");
		let video = frames.iter().find(|f| f.kind == MediaKind::Video).expect("video too");
		assert!(is_keyframe(Codec::Vp8, &video.data), "the first frame is a keyframe");
		assert!(source.streamer().stats().video_frames > 0);
	}
}
