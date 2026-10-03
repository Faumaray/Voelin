//! Stream media (feature `media`): capture and encoding for our stream,
//! decoding for the streams we watch.
//!
//! - [`Streamer`] captures a screen or window (or the Stream Studio's
//!   composite, [`Streamer::start_studio`]), encodes the video with the
//!   codec our offer carries ([`stream_codec`], VP8 by default) in one or
//!   more simulcast layers ([`LayerSpec`]), and mixes any number of audio
//!   sources ([`AudioSourceSpec`]: desktop audio without Voelin's own
//!   playback, single applications, the shared window's application, the
//!   microphone) into Opus (48 kHz stereo, 20 ms). Capture, conversion,
//!   each layer's encoder and the audio mixer run on threads of their own.
//!   It hands the frames to a [`MediaSink`]: the [`StreamSink`] of a live
//!   stream, or an [`EncodedSource`] for code that drives
//!   `voelin_stream::Streams` itself. Keyframe requests of viewers are
//!   honoured per layer, and each layer's encoder follows the bitrate the
//!   sink allows. [`Streamer::reconfigure`] changes frame rate, bitrate,
//!   codec, layers and audio sources while streaming.
//! - [`VideoPipeline`] decodes the video of a watched stream on its own
//!   thread with the best decoder of its codec, falling back to the next
//!   one when a decoder fails, and hands every picture to a callback. After
//!   lost frames it asks for a keyframe; decoders that conceal what is
//!   missing (FFmpeg's H.264 and HEVC) go on decoding meanwhile, the others
//!   skip to it.
//! - [`Viewer`] is a [`VideoPipeline`] fed from [`Engine::subscribe_frames`].
//!   The stream's audio needs nothing here: the session plays it
//!   ([`Command::SetStreamVolume`]).
//! - [`Latest`] hands the newest picture to a slower consumer such as a UI.
//! - [`LocalPreview`] runs capture → encoder → decoder without a server.
//!
//! [`voelin_media`] is re-exported for sources, codecs and pixel conversion.
//! Codecs are chosen through `voelin_media::Codecs`, so whatever backends the
//! build has are used (libvpx and OpenH264 with feature `media-desktop`,
//! MediaCodec on Android).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{debug, warn};
use voelin_audio::tap::{self, TapGuard, TapSink};
use voelin_audio::{VoiceCodec, VoiceEncoder};
pub use voelin_media;
use voelin_media::capture::playback::{self, PlaybackFilter, SourceCapture};
pub use voelin_media::capture::playback::{AppMatch, AudioApp, AudioApps};
pub use voelin_media::capture::synthetic::Pattern;
use voelin_media::capture::synthetic::{SineSource, SyntheticScreen};
use voelin_media::capture::{
	self, CaptureOptions, CaptureSource, DmaBufRef, FramePacer, FrameSink, ScreenCapture, SourceId,
};
use voelin_media::handoff::Handoff;
use voelin_media::mix::{
	BlockClock, MIX_RATE, MixerConfig, MixerHandle, SourceHandle, SourceInput, StreamMixer,
};
pub use voelin_media::mix::{Level, SourceState};
use voelin_media::scale::Pyramid;
use voelin_media::studio::Studio;
use voelin_media::studio::output::{Packet, Track};
use voelin_media::{
	Codec, Codecs, ContentHint, EncodedChunk, EncoderConfig, FrameRef, GpuFrame, VideoEncoder,
	VideoFrame,
};
pub use voelin_media::{DecoderBackend, DecoderPreference, EncoderBackend, EncoderPreference};
use voelin_stream::{
	EncodedFrame, FrameSource, Frequency, LayerId, LayerSet, LayerSpec, MediaFrame, MediaKind,
	MediaTime, PeerConfig, VideoCodec,
};

use crate::settings::{AudioSourceKindSetting, AudioSourceSetting};
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
/// Failures in a row (errors, or [`FRAMES_WITHOUT_PICTURE`] frames that
/// gave none) after which a decoder is replaced by the next one of its
/// codec's ladder.
const DECODER_FAILURES: u32 = 3;
/// Frames a decoder may take without giving a picture before that counts
/// as a failure (and a keyframe is asked for): decoders of streams with
/// B-frames hold a few.
const FRAMES_WITHOUT_PICTURE: u32 = 10;
/// Decoded pictures a [`VideoPipeline`] recycles; while the consumer holds
/// them all, more are made.
const PICTURE_POOL: usize = 4;
/// The decoding rates of [`DecodeStats`] cover at least this long.
const RATE_WINDOW: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
	#[error(transparent)]
	Media(#[from] voelin_media::Error),
	#[error("Opus: {0}")]
	Opus(#[from] voelin_audio::Error),
	#[error("stream settings: {0}")]
	Config(String),
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

/// The `voelin_media` codec of a negotiated video codec.
pub fn media_codec(codec: VideoCodec) -> Codec {
	match codec {
		VideoCodec::Vp8 => Codec::Vp8,
		VideoCodec::Vp9 => Codec::Vp9,
		VideoCodec::H264 => Codec::H264,
		VideoCodec::Av1 => Codec::Av1,
		VideoCodec::H265 => Codec::H265,
	}
}

/// The negotiable video codec of a `voelin_media` codec.
pub fn video_codec(codec: Codec) -> VideoCodec {
	match codec {
		Codec::Vp8 => VideoCodec::Vp8,
		Codec::Vp9 => VideoCodec::Vp9,
		Codec::H264 => VideoCodec::H264,
		Codec::Av1 => VideoCodec::Av1,
		Codec::H265 => VideoCodec::H265,
	}
}

/// The codec our stream is encoded with: the first codec of the offer
/// (`config.video_codecs`) that we can encode.
pub fn stream_codec(codecs: &Codecs, config: &PeerConfig) -> Option<Codec> {
	let encodable = codecs.encoder_codecs();
	config.video_codecs.iter().map(|c| media_codec(*c)).find(|c| encodable.contains(c))
}

/// `config` adjusted to what `codecs` can do: viewers accept only codecs we
/// decode; a streamer offers its stream codec ([`stream_codec`]) first, then
/// [`offer_codecs`]. Viewers answer with the first they decode, and the
/// [`Streamer`] encodes each layer once per codec its viewers chose.
pub fn peer_config(codecs: &Codecs, mut config: PeerConfig) -> PeerConfig {
	config.accept_video_codecs = codecs.decoders().into_iter().map(video_codec).collect();
	if let Some(codec) = stream_codec(codecs, &config) {
		config.video_codecs = offer_codecs(codecs, codec).into_iter().map(video_codec).collect();
	}
	config
}

/// Whether every official TeamSpeak client decodes `codec`: their WebRTC
/// stack has VP8, VP9 and AV1 decoders built in. H.264 needs an OpenH264
/// library the official client downloads at start-up and may not get; it
/// still answers H.264 then, and decodes nothing (libwebrtc's
/// `NullVideoDecoder`). HEVC needs a hardware decoder.
pub fn decoded_everywhere(codec: Codec) -> bool {
	matches!(codec, Codec::Vp8 | Codec::Vp9 | Codec::Av1)
}

/// What a streamer with `primary` as its stream codec offers, in order:
/// `primary`; the codecs of hardware encoders and VP8 through libvpx that
/// every TeamSpeak client decodes ([`decoded_everywhere`]), so that a
/// viewer, which answers with the first codec it supports, never picks
/// H.264 while one of them is there; H.264 from hardware; HEVC last (for
/// peers that take nothing else). Other software encoders are only ever the
/// stream codec, so no viewer's answer starts one of them on top of it.
pub fn offer_codecs(codecs: &Codecs, primary: Codec) -> Vec<Codec> {
	let mut offer = vec![primary];
	for (codec, backend) in codecs.encoders() {
		let cheap = codecs.is_hardware(backend)
			|| (codec == Codec::Vp8 && backend == voelin_media::EncoderBackend::Libvpx);
		if cheap && !offer.contains(&codec) {
			offer.push(codec);
		}
	}
	// Stable: the encoder preference orders codecs of the same rank.
	offer[1..].sort_by_key(|&codec| match codec {
		_ if decoded_everywhere(codec) => 0,
		Codec::H265 => 2,
		_ => 1,
	});
	offer
}

/// The encoder preference of the settings `stream.hardware_acceleration`
/// and `stream.encoder_backend`.
pub fn encoder_preference(settings: &crate::settings::Settings) -> voelin_media::EncoderPreference {
	use crate::settings::{STREAM_ENCODER_BACKEND, STREAM_HARDWARE_ACCELERATION};
	voelin_media::EncoderPreference {
		hardware: settings.get(&STREAM_HARDWARE_ACCELERATION),
		backend: settings.get(&STREAM_ENCODER_BACKEND).parse().unwrap_or_default(),
	}
}

/// The decoder preference of the settings `stream.hardware_decoding` and
/// `stream.decoder_backend` (for `Codecs::with_decoder_preference`).
pub fn decoder_preference(settings: &crate::settings::Settings) -> DecoderPreference {
	use crate::settings::{STREAM_DECODER_BACKEND, STREAM_HARDWARE_DECODING};
	DecoderPreference {
		hardware: settings.get(&STREAM_HARDWARE_DECODING),
		backend: settings.get(&STREAM_DECODER_BACKEND).parse().unwrap_or_default(),
	}
}

/// The stream codec: `configured` if we can encode it, else the first codec
/// of the encoder preference (hardware first when enabled; VP8 through
/// libvpx without hardware) that every TeamSpeak client decodes
/// ([`decoded_everywhere`]), else any but HEVC (offered last only). Put it
/// first in `PeerConfig::video_codecs` before [`peer_config`].
pub fn preferred_codec(codecs: &Codecs, configured: Option<Codec>) -> Option<Codec> {
	let encodable = codecs.encoder_codecs();
	configured
		.filter(|c| encodable.contains(c))
		.or_else(|| encodable.iter().copied().find(|&c| decoded_everywhere(c)))
		.or_else(|| encodable.into_iter().find(|c| *c != Codec::H265))
}

/// The codec of the setting `stream.codec`; `None` for `auto` (see
/// [`preferred_codec`]).
pub fn configured_codec(settings: &crate::settings::Settings) -> Option<Codec> {
	use crate::settings::{CodecChoice, STREAM_CODEC};
	match settings.get(&STREAM_CODEC) {
		CodecChoice::Auto => None,
		CodecChoice::Vp8 => Some(Codec::Vp8),
		CodecChoice::Vp9 => Some(Codec::Vp9),
		CodecChoice::H264 => Some(Codec::H264),
		CodecChoice::Av1 => Some(Codec::Av1),
	}
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

	/// Adds to `layers` the simulcast layers a keyframe was asked for since
	/// the last call. Sinks without simulcast report a request as layer 0.
	fn take_layer_keyframes(&self, layers: &mut LayerSet) {
		if self.take_keyframe_request() {
			layers.insert(0);
		}
	}

	/// Bitrate (bit/s) the bandwidth estimates of `layer`'s viewers allow,
	/// once known; the encoder of the layer follows it.
	fn layer_bitrate(&self, layer: LayerId) -> Option<u64> {
		let _ = layer;
		None
	}

	/// Send one video frame encoded in `codec`: it goes to the viewers whose
	/// answer chose `codec`.
	fn send_video(&self, frame: EncodedFrame, codec: Codec) -> bool {
		let _ = codec;
		self.send(frame)
	}

	/// Adds to `out` the video codecs the viewers chose; none (the default)
	/// means only the stream codec is sent.
	fn video_codecs(&self, out: &mut Vec<Codec>) {
		let _ = out;
	}
}

impl MediaSink for StreamSink {
	fn send(&self, frame: EncodedFrame) -> bool {
		StreamSink::send(self, frame)
	}

	fn send_video(&self, frame: EncodedFrame, codec: Codec) -> bool {
		StreamSink::send_video(self, frame, video_codec(codec))
	}

	fn video_codecs(&self, out: &mut Vec<Codec>) {
		for codec in VideoCodec::ALL {
			if self.has_video_codec(codec) {
				out.push(media_codec(codec));
			}
		}
	}

	fn take_keyframe_request(&self) -> bool {
		StreamSink::take_keyframe_request(self)
	}

	fn take_layer_keyframes(&self, layers: &mut LayerSet) {
		StreamSink::take_layer_keyframes(self, layers);
	}

	fn layer_bitrate(&self, layer: LayerId) -> Option<u64> {
		StreamSink::layer_bitrate(self, layer)
	}
}

/// Which backend captures monitors and windows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CaptureBackend {
	/// The one for this desktop session (`capture::default_screen_capture`:
	/// the ScreenCast portal on Wayland, X11, Windows Graphics Capture).
	#[default]
	Auto,
	/// The xdg-desktop-portal ScreenCast dialog (Wayland; also X11 desktops
	/// that run the portal). Monitor and window ids are ignored: the
	/// dialog picks.
	Portal,
	/// X11, also under Wayland through XWayland (Linux).
	X11,
	/// wlroots compositors (Sway, Hyprland, river, ...) directly, without
	/// the portal: `ext-image-copy-capture-v1`, or `wlr-screencopy-unstable-v1`
	/// where that is missing (Linux).
	Wlroots,
}

impl CaptureBackend {
	/// Names as in settings: `auto`, `portal`, `x11`, `wlroots`.
	pub fn name(&self) -> &'static str {
		match self {
			CaptureBackend::Auto => "auto",
			CaptureBackend::Portal => "portal",
			CaptureBackend::X11 => "x11",
			CaptureBackend::Wlroots => "wlroots",
		}
	}
}

impl std::str::FromStr for CaptureBackend {
	type Err = MediaError;

	fn from_str(s: &str) -> Result<Self, MediaError> {
		match s.trim().to_ascii_lowercase().as_str() {
			"auto" | "" => Ok(CaptureBackend::Auto),
			"portal" => Ok(CaptureBackend::Portal),
			"x11" => Ok(CaptureBackend::X11),
			"wlroots" | "wlr" => Ok(CaptureBackend::Wlroots),
			other => Err(MediaError::Config(format!(
				"unknown capture backend {other:?}: auto, portal, x11 or wlroots"
			))),
		}
	}
}

/// What an audio source of our stream captures.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AudioSourceKind {
	/// Everything that plays except this process and its children (the
	/// voices and streams we play): PipeWire links from every other
	/// playback stream, WASAPI process loopback excluding our process tree,
	/// Android playback capture excluding our uid.
	DesktopWithoutSelf,
	/// One application (see [`AppMatch`]; [`audio_apps`] lists them).
	App(AppMatch),
	/// The application that owns the shared window ([`SourceId::Window`]),
	/// where the platform tells: X11 `_NET_WM_PID`, the owner of a Windows
	/// `HWND`. The ScreenCast portal (Wayland) does not say whose window it
	/// shares: pick the application from [`audio_apps`] instead.
	WindowAudio,
	/// Our microphone as the voice connection sends it (after noise
	/// suppression and gain control), while a voice connection runs.
	Microphone,
	/// A quiet sine tone of this many Hz (tests, the test pattern).
	Synthetic { hz: u32 },
}

impl AudioSourceKind {
	/// A name for the mixer and the UI.
	pub fn label(&self) -> String {
		match self {
			AudioSourceKind::DesktopWithoutSelf => "Desktop audio (without Voelin)".into(),
			AudioSourceKind::App(app) => format!("App: {app}"),
			AudioSourceKind::WindowAudio => "Audio of the shared window".into(),
			AudioSourceKind::Microphone => "Microphone".into(),
			AudioSourceKind::Synthetic { hz } => format!("Test tone {hz} Hz"),
		}
	}
}

/// One audio source of a stream, with its gain and mute. Compared bit for
/// bit (`gain` by its bits), so it is `Eq`.
#[derive(Clone, Debug)]
pub struct AudioSourceSpec {
	pub kind: AudioSourceKind,
	/// Linear gain (1: unchanged), any value >= 0.
	pub gain: f32,
	pub muted: bool,
}

impl PartialEq for AudioSourceSpec {
	fn eq(&self, other: &Self) -> bool {
		self.kind == other.kind
			&& self.gain.to_bits() == other.gain.to_bits()
			&& self.muted == other.muted
	}
}

impl Eq for AudioSourceSpec {}

impl AudioSourceSpec {
	/// At gain 1, not muted.
	pub fn new(kind: AudioSourceKind) -> Self {
		Self { kind, gain: 1.0, muted: false }
	}
}

impl From<&AudioSourceSetting> for AudioSourceSpec {
	fn from(setting: &AudioSourceSetting) -> Self {
		let kind = match &setting.kind {
			AudioSourceKindSetting::Desktop => AudioSourceKind::DesktopWithoutSelf,
			// A pid wins: it names one process exactly.
			AudioSourceKindSetting::App { pid: Some(pid), .. } => {
				AudioSourceKind::App(AppMatch::Pid(*pid))
			}
			AudioSourceKindSetting::App { name, pid: None } => {
				AudioSourceKind::App(AppMatch::Name(name.clone().unwrap_or_default()))
			}
			AudioSourceKindSetting::Window => AudioSourceKind::WindowAudio,
			AudioSourceKindSetting::Microphone => AudioSourceKind::Microphone,
			AudioSourceKindSetting::Synthetic { frequency } => {
				AudioSourceKind::Synthetic { hz: *frequency }
			}
		};
		Self { kind, gain: setting.gain, muted: setting.muted }
	}
}

impl From<&AudioSourceSpec> for AudioSourceSetting {
	fn from(spec: &AudioSourceSpec) -> Self {
		let kind = match &spec.kind {
			AudioSourceKind::DesktopWithoutSelf => AudioSourceKindSetting::Desktop,
			AudioSourceKind::App(AppMatch::Pid(pid)) => {
				AudioSourceKindSetting::App { name: None, pid: Some(*pid) }
			}
			AudioSourceKind::App(AppMatch::Name(name)) => {
				AudioSourceKindSetting::App { name: Some(name.clone()), pid: None }
			}
			AudioSourceKind::WindowAudio => AudioSourceKindSetting::Window,
			AudioSourceKind::Microphone => AudioSourceKindSetting::Microphone,
			AudioSourceKind::Synthetic { hz } => {
				AudioSourceKindSetting::Synthetic { frequency: *hz }
			}
		};
		Self { kind, gain: spec.gain, muted: spec.muted }
	}
}

/// The sources of `settings::STREAM_AUDIO_SOURCES` as [`AudioSourceSpec`]s
/// (for [`StreamerConfig::audio_sources`]; empty: no audio).
pub fn audio_source_specs(settings: &[AudioSourceSetting]) -> Vec<AudioSourceSpec> {
	settings.iter().map(AudioSourceSpec::from).collect()
}

/// The audio of a stream when none is chosen: a tone with the test
/// pattern, else desktop audio without Voelin.
pub fn default_audio_sources(source: &SourceId) -> Vec<AudioSourceSpec> {
	let kind = match source {
		SourceId::Synthetic => AudioSourceKind::Synthetic { hz: 440 },
		_ => AudioSourceKind::DesktopWithoutSelf,
	};
	vec![AudioSourceSpec::new(kind)]
}

/// Applications that play audio now (Android: launchable apps), updated
/// live, for a picker of [`AudioSourceKind::App`].
pub fn audio_apps() -> Result<AudioApps, MediaError> {
	Ok(playback::audio_apps()?)
}

/// One audio source in [`StreamerStats`].
#[derive(Clone, Debug, PartialEq)]
pub struct AudioSourceStats {
	/// The mixer's id of the source.
	pub id: u64,
	pub spec: AudioSourceSpec,
	pub name: String,
	/// After its gain, before its mute.
	pub level: Level,
	pub state: SourceState,
	/// How far behind its capture it is mixed.
	pub latency: Duration,
	/// Times its capture delivered too late (and the audio broke up).
	pub underruns: u64,
	/// Why it captures nothing (no PipeWire, the window's process unknown,
	/// ...).
	pub error: Option<String>,
}

/// What and how to stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamerConfig {
	pub source: SourceId,
	/// For monitors and windows.
	pub backend: CaptureBackend,
	/// Frame-rate cap (at least 1). The capture delivers what the source
	/// gives up to this rate; layers may cap lower.
	pub fps: u32,
	/// Video bitrate in kbit/s, as in `StreamSetup::bitrate`, for the single
	/// layer when `layers` is empty.
	pub bitrate_kbps: u32,
	/// The codec of our offer, see [`stream_codec`].
	pub codec: Codec,
	/// Which encoders to use ([`encoder_preference`] of the settings).
	pub encoder: EncoderPreference,
	/// Send audio: [`audio_sources`](Self::audio_sources) mixed.
	pub audio: bool,
	/// Audio sources mixed into the stream (any number); empty: the
	/// [`default_audio_sources`] (a tone with the test pattern, else desktop
	/// audio without Voelin). [`Streamer::reconfigure`] changes them live.
	pub audio_sources: Vec<AudioSourceSpec>,
	pub cursor: bool,
	/// Size of the test pattern ([`SourceId::Synthetic`]).
	pub synthetic_size: (u32, u32),
	/// What the test pattern shows.
	pub synthetic_pattern: Pattern,
	/// Hand the test pattern over as DMA-BUFs of ordinary memory, as the
	/// ScreenCast portal hands over the screen
	/// ([`SyntheticScreen::with_dmabuf`]): drives the GPU path without a
	/// portal (Linux, `/dev/udmabuf`).
	pub synthetic_dmabuf: bool,
	/// Portal restore token from an earlier share: the desktop may skip its
	/// dialog. The new one is [`Streamer::restore_token`].
	pub restore_token: Option<String>,
	/// Simulcast layers to encode, each with its own encoder. Empty: one
	/// layer (id 0) at the source's size and `bitrate_kbps`.
	pub layers: Vec<LayerSpec>,
}

impl Default for StreamerConfig {
	fn default() -> Self {
		Self {
			source: SourceId::Monitor(0),
			backend: CaptureBackend::Auto,
			fps: 30,
			bitrate_kbps: 4608,
			codec: Codec::Vp8,
			encoder: EncoderPreference::default(),
			audio: true,
			audio_sources: Vec::new(),
			cursor: true,
			synthetic_size: (1280, 720),
			synthetic_pattern: Pattern::Simple,
			synthetic_dmabuf: false,
			restore_token: None,
			layers: Vec::new(),
		}
	}
}

impl StreamerConfig {
	/// The layers that are encoded: [`layers`](Self::layers), or one layer at
	/// `bitrate_kbps`.
	pub fn effective_layers(&self) -> Vec<LayerSpec> {
		if self.layers.is_empty() {
			vec![LayerSpec::single(u64::from(self.bitrate_kbps.max(1)) * 1000)]
		} else {
			self.layers.clone()
		}
	}
}

/// Changes for a running [`Streamer`] ([`Streamer::reconfigure`]); `None`
/// keeps the current value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamerConfigUpdate {
	/// Frame-rate cap; the capture follows without restarting.
	pub fps: Option<u32>,
	/// Bitrate of the single layer (used while `layers` is empty).
	pub bitrate_kbps: Option<u32>,
	/// Another codec: new encoders, starting with keyframes.
	pub codec: Option<Codec>,
	/// Other encoders (hardware on or off, another backend): new encoders
	/// for every codec, starting with keyframes.
	pub encoder: Option<EncoderPreference>,
	/// Another set of layers (`Some(vec![])`: back to a single layer). Layers
	/// keep their encoder when their id stays; new ids get new encoders.
	pub layers: Option<Vec<LayerSpec>>,
	/// Other audio sources. Sources whose kind stays keep their capture and
	/// just take the new gain and mute; new kinds start capturing, removed
	/// ones stop. `Some(vec![])` silences the audio (the track stays).
	pub audio_sources: Option<Vec<AudioSourceSpec>>,
}

/// One simulcast layer in [`StreamerStats`]. Rates are over the last second.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayerStats {
	pub id: LayerId,
	/// Size of the last encoded frame.
	pub width: u32,
	pub height: u32,
	/// Frames handed to the sink.
	pub frames: u64,
	pub keyframes: u64,
	/// Frames the layer's encoder was too busy for (replaced by a newer one
	/// before it got to them).
	pub dropped: u64,
	pub fps: f64,
	pub kbps: f64,
	/// Mean time to encode a frame.
	pub encode_time: Duration,
	/// The encoder's target bitrate (bit/s): the layer's, or what the
	/// viewers' bandwidth estimates allow.
	pub bitrate: u64,
	pub threads: u32,
	/// The encoder's speed setting (libvpx `cpu-used`), if it has one.
	pub speed: Option<i32>,
	/// The encoder of the stream codec (`libvpx`, `h264_vaapi`, ...).
	pub backend: Option<EncoderBackend>,
	/// The codecs this layer is encoded in: the stream codec, and those
	/// viewers chose instead (each with its own encoder).
	pub codecs: Vec<Codec>,
}

/// What a [`Streamer`] has done so far. Rates are over the last second.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamerStats {
	/// Frames handed to the sink (all layers).
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
	/// Frames the capture delivered (within the frame-rate cap).
	pub captured_frames: u64,
	pub capture_fps: f64,
	/// Mean time to convert a captured frame to I420 and scale it for every
	/// layer that is due, on the CPU.
	pub convert_time: Duration,
	/// Captured frames that never reached the CPU: DMA-BUFs converted to
	/// NV12 and scaled for every layer on the GPU, for VA-API encoders.
	pub gpu_frames: u64,
	/// Mean time the capture thread spends on such a frame (it waits for
	/// the GPU, so the buffer can go back to the compositor).
	pub gpu_convert_time: Duration,
	/// Why the GPU path stopped (it fell back to the CPU), if it did.
	pub gpu_error: Option<String>,
	/// Frames dropped between conversion and the encoders, all layers.
	pub dropped_frames: u64,
	/// Threads converting and scaling.
	pub convert_threads: usize,
	pub codec: Option<Codec>,
	pub layers: Vec<LayerStats>,
	/// Level of the stream's audio (after the limiter).
	pub audio_level: Level,
	/// The limiter's lowest gain in the last 20 ms (1: not limiting).
	pub audio_limiter: f32,
	pub audio_sources: Vec<AudioSourceStats>,
}

/// One simulcast layer while streaming: its encoder thread's inbox and
/// counters (written by the pipeline threads, read lock-free by stats).
struct Layer {
	id: LayerId,
	/// Size, scale and frame-rate cap, read by the capture thread when the
	/// layer set changes.
	spec: Mutex<LayerSpec>,
	/// Newest converted frame for the encoder.
	inbox: Handoff<VideoFrame>,
	/// Newest frame converted on the GPU, instead.
	gpu_inbox: Handoff<GpuFrame>,
	/// What every encoder of the layer takes GPU frames at, written by the
	/// encoder thread: `width << 32 | height` alignment, 0 if one of them
	/// needs frames in memory.
	gpu: AtomicU64,
	/// A keyframe was asked for.
	keyframe: AtomicBool,
	stop: AtomicBool,
	/// Configured bitrate and cap (0: none), bit/s.
	bitrate: AtomicU64,
	max_bitrate: AtomicU64,
	/// Frame rate the encoder plans for.
	fps: AtomicU32,
	/// An encoder to switch to (codec change).
	next_encoder: Mutex<Option<Box<dyn VideoEncoder>>>,
	threads: u32,
	// Counters.
	frames: AtomicU64,
	keyframes: AtomicU64,
	bytes: AtomicU64,
	encoded: AtomicU64,
	encode_ns: AtomicU64,
	/// `width << 32 | height` of the last encoded frame.
	size: AtomicU64,
	target: AtomicU64,
	/// `cpu-used`, or `i64::MIN`.
	speed: AtomicI64,
	/// The stream codec's encoder and every codec encoded (for stats).
	backend: Mutex<Option<EncoderBackend>>,
	codecs: Mutex<Vec<Codec>>,
}

impl Layer {
	fn new(spec: &LayerSpec, fps: u32, threads: u32) -> Self {
		Self {
			id: spec.id,
			spec: Mutex::new(spec.clone()),
			inbox: Handoff::new(),
			gpu_inbox: Handoff::new(),
			gpu: AtomicU64::new(0),
			keyframe: AtomicBool::new(false),
			stop: AtomicBool::new(false),
			bitrate: AtomicU64::new(spec.bitrate.max(1)),
			max_bitrate: AtomicU64::new(spec.max_bitrate.unwrap_or(0)),
			fps: AtomicU32::new(layer_fps(spec, fps)),
			next_encoder: Mutex::new(None),
			threads,
			frames: AtomicU64::new(0),
			keyframes: AtomicU64::new(0),
			bytes: AtomicU64::new(0),
			encoded: AtomicU64::new(0),
			encode_ns: AtomicU64::new(0),
			size: AtomicU64::new(0),
			target: AtomicU64::new(spec.bitrate),
			speed: AtomicI64::new(i64::MIN),
			backend: Mutex::new(None),
			codecs: Mutex::new(Vec::new()),
		}
	}

	fn update(&self, spec: &LayerSpec, fps: u32) {
		*lock(&self.spec) = spec.clone();
		self.bitrate.store(spec.bitrate.max(1), Ordering::Relaxed);
		self.max_bitrate.store(spec.max_bitrate.unwrap_or(0), Ordering::Relaxed);
		self.fps.store(layer_fps(spec, fps), Ordering::Relaxed);
	}

	fn stopped(&self) -> bool {
		self.stop.load(Ordering::Relaxed)
	}
}

/// The frame rate a layer encodes at: its cap, within the capture's.
fn layer_fps(spec: &LayerSpec, fps: u32) -> u32 {
	spec.max_fps.map_or(fps, |cap| cap.min(fps)).max(1)
}

/// What [`Streamer::reconfigure`] changes.
struct Settings {
	fps: u32,
	bitrate_kbps: u32,
	codec: Codec,
	encoder: EncoderPreference,
	layers: Vec<LayerSpec>,
}

impl Settings {
	fn effective_layers(&self) -> Vec<LayerSpec> {
		StreamerConfig {
			bitrate_kbps: self.bitrate_kbps,
			layers: self.layers.clone(),
			..StreamerConfig::default()
		}
		.effective_layers()
	}
}

/// Rates the stats thread computes once a second.
#[derive(Default)]
struct Rates {
	capture_fps: f64,
	convert_time: Duration,
	gpu_convert_time: Duration,
	/// Per layer: id, fps, kbit/s, encode time.
	layers: Vec<(LayerId, f64, f64, Duration)>,
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
	/// Frame-rate cap of the capture.
	fps: AtomicU32,
	captured: AtomicU64,
	converted: AtomicU64,
	convert_ns: AtomicU64,
	convert_threads: AtomicUsize,
	/// Frames converted on the GPU instead, and the time it took.
	gpu_converted: AtomicU64,
	gpu_convert_ns: AtomicU64,
	gpu_error: Mutex<Option<String>>,
	/// The layers being encoded, in configuration order.
	layers: Mutex<Vec<Arc<Layer>>>,
	/// Bumped whenever `layers` or a layer's spec changes.
	generation: AtomicU64,
	/// Scratch set for the sink's keyframe requests.
	keyframes: Mutex<LayerSet>,
	settings: Mutex<Option<Settings>>,
	rates: Mutex<Rates>,
	/// For encoders of the codecs viewers chose besides the stream codec,
	/// created by the encoder threads when a viewer needs one.
	encoders: Mutex<Option<(Codecs, EncoderPreference)>>,
	/// Bumped when `encoders` changes: those encoders are made again.
	encoders_generation: AtomicU64,
	/// The studio whose composite this is ([`Streamer::start_studio`]): its
	/// outputs get every packet of the stream codec, attached or not.
	studio: Option<Arc<Studio>>,
}

/// The sink of a studio's streamer while no stream is attached: the frames
/// are still encoded, for the studio's recording and replay buffer.
struct Unattached;

impl MediaSink for Unattached {
	fn send(&self, _frame: EncodedFrame) -> bool {
		true
	}

	fn take_keyframe_request(&self) -> bool {
		false
	}
}

static UNATTACHED: LazyLock<Arc<dyn MediaSink>> = LazyLock::new(|| Arc::new(Unattached));

impl Shared {
	fn sink(&self) -> Option<Arc<dyn MediaSink>> {
		lock(&self.sink).clone().or_else(|| self.studio.as_ref().map(|_| UNATTACHED.clone()))
	}

	fn stopped(&self) -> bool {
		self.stop.load(Ordering::Relaxed)
	}

	fn set_error(&self, e: impl ToString) {
		*lock(&self.error) = Some(e.to_string());
	}

	/// Move the sink's keyframe requests to the layers they are for. Any
	/// encoder thread may call it; one at a time does the work.
	fn poll_keyframes(&self, sink: &dyn MediaSink) {
		let Ok(mut requested) = self.keyframes.try_lock() else { return };
		sink.take_layer_keyframes(&mut requested);
		if requested.is_empty() {
			return;
		}
		for layer in lock(&self.layers).iter() {
			if requested.contains(layer.id) {
				layer.keyframe.store(true, Ordering::Relaxed);
			}
		}
		requested.clear();
	}
}

/// Split `cores` encoder threads over layers by their share of the pixels
/// (source size unknown yet: layers with a fixed size count against
/// 1920x1080).
fn thread_split(layers: &[LayerSpec], cores: u32) -> Vec<u32> {
	let area = |l: &LayerSpec| {
		let (w, h) = l.output_size(1920, 1080);
		f64::from(w) * f64::from(h)
	};
	let total: f64 = layers.iter().map(area).sum::<f64>().max(1.0);
	layers.iter().map(|l| ((f64::from(cores) * area(l) / total).round() as u32).max(1)).collect()
}

/// CPUs split between the layers' encoders (all but one, see
/// `voelin_media::codec::encoder_cpus`).
fn cores() -> u32 {
	voelin_media::codec::encoder_cpus()
}

fn encoder_config(spec: &LayerSpec, fps: u32, threads: u32) -> EncoderConfig {
	EncoderConfig {
		fps: layer_fps(spec, fps),
		bitrate_bps: spec.bitrate.clamp(1, u64::from(u32::MAX)) as u32,
		content: ContentHint::Screen,
		threads,
		..EncoderConfig::default()
	}
}

fn new_encoder(
	codecs: &Codecs,
	codec: Codec,
	preference: &EncoderPreference,
	spec: &LayerSpec,
	fps: u32,
	threads: u32,
) -> Result<Box<dyn VideoEncoder>, MediaError> {
	let config = encoder_config(spec, fps, threads);
	let encoder = codecs.new_encoder_preferring(codec, config, preference)?;
	debug!(layer = spec.id, %codec, backend = %encoder.backend(), "video encoder");
	Ok(encoder)
}

/// Capture and encoding for our stream. Frames are captured from the start
/// (the portal asks the user then), but only converted and encoded once a
/// sink is [attached](Streamer::attach). Stops when dropped.
///
/// The capture backend hands each frame, still in its capture buffer, to
/// the conversion on its own thread: one pass converts it to I420 on all
/// cores and derives every layer's size from it (a pyramid, recycled
/// buffers). Each layer's newest frame goes through a one-slot handoff to
/// that layer's encoder thread; a frame an encoder was too busy for is
/// replaced, not queued.
///
/// Audio sources feed a [`StreamMixer`] from their capture threads; the
/// mixer runs every 20 ms on a thread of its own and its output is encoded
/// with Opus.
pub struct Streamer {
	shared: Arc<Shared>,
	screen: Option<Box<dyn ScreenCapture>>,
	/// The video source (for [`AudioSourceKind::WindowAudio`]).
	source: SourceId,
	// Boxed: a `Streamer` is held inline by previews.
	audio: Mutex<Option<Box<StreamAudio>>>,
	threads: Vec<JoinHandle<()>>,
	/// Encoder threads, by layer.
	encoders: Mutex<Vec<(LayerId, JoinHandle<()>)>>,
	backend: &'static str,
	restore_token: Option<String>,
	/// Why the audio did not start at all.
	audio_error: Option<String>,
}

impl Streamer {
	/// Start capturing. Must run on a Tokio runtime (the portal talks D-Bus
	/// on it); may wait for the user in the portal's dialog.
	pub async fn start(codecs: &Codecs, config: StreamerConfig) -> Result<Self, MediaError> {
		Self::launch(codecs, config, None).await
	}

	/// Stream `studio`'s composite ([`SourceId::Studio`]; `config.source` is
	/// ignored). Unlike other sources it is encoded from the start, attached
	/// or not, and every packet of the stream codec (video of the studio's
	/// layer, and the audio) also goes to the studio's outputs: recording
	/// and the replay buffer work without going live, and cost no second
	/// encode.
	pub async fn start_studio(
		codecs: &Codecs,
		config: StreamerConfig,
		studio: Arc<Studio>,
	) -> Result<Self, MediaError> {
		let config = StreamerConfig { source: SourceId::Studio, ..config };
		Self::launch(codecs, config, Some(studio)).await
	}

	async fn launch(
		codecs: &Codecs,
		config: StreamerConfig,
		studio: Option<Arc<Studio>>,
	) -> Result<Self, MediaError> {
		let fps = config.fps.max(1);
		let layers = config.effective_layers();
		check_layers(&layers)?;
		let shared = Arc::new(Shared { studio, ..Shared::default() });
		shared.fps.store(fps, Ordering::Relaxed);
		let mut streamer = Self {
			shared: shared.clone(),
			screen: None,
			source: config.source.clone(),
			audio: Mutex::new(None),
			threads: Vec::new(),
			encoders: Mutex::new(Vec::new()),
			backend: "",
			restore_token: None,
			audio_error: None,
		};
		// Encoders first: an unavailable codec fails before any dialog.
		let split = thread_split(&layers, cores());
		let mut ready = Vec::new();
		for (spec, threads) in layers.iter().zip(split) {
			let encoder = new_encoder(codecs, config.codec, &config.encoder, spec, fps, threads)?;
			ready.push((Arc::new(Layer::new(spec, fps, threads)), encoder));
		}
		*lock(&shared.encoders) = Some((codecs.clone(), config.encoder.clone()));
		for (layer, encoder) in ready {
			streamer.spawn_encoder(layer, encoder)?;
		}
		*lock(&shared.settings) = Some(Settings {
			fps,
			bitrate_kbps: config.bitrate_kbps,
			codec: config.codec,
			encoder: config.encoder.clone(),
			layers: config.layers.clone(),
		});
		streamer.threads.push(spawn("voelin-stream-stats", {
			let shared = shared.clone();
			move || stats_loop(&shared)
		})?);

		let ingest = Box::new(Ingest::new(shared.clone()));
		let options = CaptureOptions { fps, cursor: config.cursor, ..CaptureOptions::default() };
		// The portal's token for this choice comes with the capture.
		let (screen, restore_token): (Box<dyn ScreenCapture>, _) = match &config.source {
			SourceId::Synthetic => {
				let (w, h) = config.synthetic_size;
				let mut screen = SyntheticScreen::with_pattern(w, h, config.synthetic_pattern)
					.with_dmabuf(config.synthetic_dmabuf);
				screen.start_sink(&SourceId::Synthetic, &options, ingest).await?;
				(Box::new(screen), None)
			}
			SourceId::Studio => {
				let Some(studio) = &shared.studio else {
					return Err(MediaError::Config(
						"the studio's composite is streamed with Streamer::start_studio".into(),
					));
				};
				let mut capture = studio.capture();
				capture.start_sink(&SourceId::Studio, &options, ingest).await?;
				(Box::new(capture), None)
			}
			#[cfg(all(target_os = "linux", feature = "media-desktop"))]
			_ if config.source == SourceId::Portal || config.backend == CaptureBackend::Portal => {
				use voelin_media::capture::portal::PortalCapture;
				let mut portal = PortalCapture::with_restore_token(config.restore_token.clone());
				portal.start_sink(&SourceId::Portal, &options, ingest).await?;
				let token = portal.restore_token().map(str::to_owned);
				(Box::new(portal), token)
			}
			source => {
				let mut screen = screen_backend(&config.backend)?;
				screen.start_sink(source, &options, ingest).await?;
				(screen, None)
			}
		};
		streamer.backend = screen.backend();
		streamer.screen = Some(screen);
		streamer.restore_token = restore_token;
		if config.audio {
			let sources = if config.audio_sources.is_empty() {
				default_audio_sources(&config.source)
			} else {
				config.audio_sources.clone()
			};
			match StreamAudio::start(&shared, &config.source, sources) {
				Ok(audio) => *lock(&streamer.audio) = Some(Box::new(audio)),
				Err(e) => {
					warn!("streaming without audio: {e}");
					streamer.audio_error = Some(e.to_string());
				}
			}
		}
		Ok(streamer)
	}

	fn spawn_encoder(
		&self,
		layer: Arc<Layer>,
		encoder: Box<dyn VideoEncoder>,
	) -> Result<(), MediaError> {
		let id = layer.id;
		lock(&self.shared.layers).push(layer.clone());
		self.shared.generation.fetch_add(1, Ordering::Relaxed);
		let shared = self.shared.clone();
		let thread =
			spawn(&format!("voelin-encode-{id}"), move || encode_loop(&shared, &layer, encoder))?;
		lock(&self.encoders).push((id, thread));
		Ok(())
	}

	/// Change frame rate, bitrate, codec, encoders, layers or audio
	/// sources while streaming, without restarting the capture; applies
	/// from the next frame. A new codec or encoder preference gets new
	/// encoders (the stream codec's at once, those of codecs viewers chose
	/// on their next frame), which start with a keyframe; new layers get
	/// new encoders; removed layers stop. Fails without changing anything
	/// if an encoder cannot be created. Audio sources that cannot capture
	/// (e.g. an application that is not running by pid) do not fail the
	/// update: their [`AudioSourceStats::error`] says why. Audio sources on
	/// a streamer started without audio start the audio track.
	pub fn reconfigure(
		&self,
		codecs: &Codecs,
		update: StreamerConfigUpdate,
	) -> Result<(), MediaError> {
		let mut guard = lock(&self.shared.settings);
		let Some(settings) = guard.as_mut() else { return Ok(()) };
		let old_codec = settings.codec;
		let next = Settings {
			fps: update.fps.unwrap_or(settings.fps).max(1),
			bitrate_kbps: update.bitrate_kbps.unwrap_or(settings.bitrate_kbps),
			codec: update.codec.unwrap_or(settings.codec),
			encoder: update.encoder.unwrap_or_else(|| settings.encoder.clone()),
			layers: update.layers.unwrap_or_else(|| settings.layers.clone()),
		};
		let specs = next.effective_layers();
		check_layers(&specs)?;
		let codec_changed = next.codec != old_codec || next.encoder != settings.encoder;
		let current: Vec<Arc<Layer>> = lock(&self.shared.layers).clone();
		// Create every encoder before changing anything.
		let split = thread_split(&specs, cores());
		let mut created = Vec::new();
		for (spec, &threads) in specs.iter().zip(&split) {
			let existing = current.iter().any(|l| l.id == spec.id);
			if !existing || codec_changed {
				let encoder =
					new_encoder(codecs, next.codec, &next.encoder, spec, next.fps, threads)?;
				created.push((spec.id, encoder));
			}
		}
		if codec_changed {
			*lock(&self.shared.encoders) = Some((codecs.clone(), next.encoder.clone()));
			self.shared.encoders_generation.fetch_add(1, Ordering::Relaxed);
		}
		self.shared.fps.store(next.fps, Ordering::Relaxed);
		let mut layers = Vec::with_capacity(specs.len());
		for (spec, &threads) in specs.iter().zip(&split) {
			let encoder =
				created.iter().position(|(id, _)| *id == spec.id).map(|i| created.swap_remove(i).1);
			match current.iter().find(|l| l.id == spec.id) {
				Some(layer) => {
					layer.update(spec, next.fps);
					if let Some(encoder) = encoder {
						*lock(&layer.next_encoder) = Some(encoder);
					}
					layers.push(layer.clone());
				}
				None => {
					let layer = Arc::new(Layer::new(spec, next.fps, threads));
					let encoder = encoder.expect("created above");
					let shared = self.shared.clone();
					let thread = spawn(&format!("voelin-encode-{}", spec.id), {
						let layer = layer.clone();
						move || encode_loop(&shared, &layer, encoder)
					})?;
					lock(&self.encoders).push((spec.id, thread));
					layers.push(layer);
				}
			}
		}
		// Stop removed layers.
		let removed: Vec<LayerId> =
			current.iter().filter(|l| !specs.iter().any(|s| s.id == l.id)).map(|l| l.id).collect();
		for layer in current.iter().filter(|l| removed.contains(&l.id)) {
			layer.stop.store(true, Ordering::Relaxed);
			layer.inbox.close();
		}
		*lock(&self.shared.layers) = layers;
		self.shared.generation.fetch_add(1, Ordering::Relaxed);
		*settings = next;
		drop(guard);
		let finished: Vec<JoinHandle<()>> = {
			let mut encoders = lock(&self.encoders);
			let (done, keep) = encoders.drain(..).partition(|(id, _)| removed.contains(id));
			*encoders = keep;
			done.into_iter().map(|(_, t)| t).collect()
		};
		for thread in finished {
			let _ = thread.join();
		}
		if let Some(sources) = update.audio_sources {
			let mut audio = lock(&self.audio);
			match audio.as_mut() {
				Some(audio) => audio.apply(sources),
				None if !sources.is_empty() => {
					let started = StreamAudio::start(&self.shared, &self.source, sources)?;
					*audio = Some(Box::new(started));
				}
				None => {}
			}
		}
		debug!(?removed, codec_changed, "stream reconfigured");
		Ok(())
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

	/// Why there is no audio, or which audio sources capture nothing and why.
	pub fn audio_error(&self) -> Option<String> {
		self.audio_error.clone().or_else(|| lock(&self.audio).as_ref().and_then(|a| a.error()))
	}

	/// Whether the stream has audio (mixed from its audio sources).
	pub fn has_audio(&self) -> bool {
		lock(&self.audio).is_some()
	}

	/// The audio mixer, for lock-free level meters
	/// ([`MixerHandle::level`], each source's `level`) and direct control.
	pub fn audio_mixer(&self) -> Option<MixerHandle> {
		lock(&self.audio).as_ref().map(|a| a.mixer.clone())
	}

	/// The audio sources now.
	pub fn audio_sources(&self) -> Vec<AudioSourceSpec> {
		lock(&self.audio)
			.as_ref()
			.map(|a| a.active.iter().map(|s| s.spec.clone()).collect())
			.unwrap_or_default()
	}

	/// The ids of the layers being encoded.
	pub fn layer_ids(&self) -> Vec<LayerId> {
		lock(&self.shared.layers).iter().map(|l| l.id).collect()
	}

	pub fn stats(&self) -> StreamerStats {
		let shared = &self.shared;
		let audio = lock(&self.audio);
		let size = shared.size.load(Ordering::Relaxed);
		let codec = lock(&shared.settings).as_ref().map(|s| s.codec);
		let rates = lock(&shared.rates);
		let layers: Vec<LayerStats> = lock(&shared.layers)
			.iter()
			.map(|l| {
				let size = l.size.load(Ordering::Relaxed);
				let (fps, kbps, encode_time) = rates
					.layers
					.iter()
					.find(|r| r.0 == l.id)
					.map_or((0.0, 0.0, Duration::ZERO), |r| (r.1, r.2, r.3));
				let speed = l.speed.load(Ordering::Relaxed);
				LayerStats {
					id: l.id,
					width: (size >> 32) as u32,
					height: size as u32,
					frames: l.frames.load(Ordering::Relaxed),
					keyframes: l.keyframes.load(Ordering::Relaxed),
					dropped: l.inbox.replaced(),
					fps,
					kbps,
					encode_time,
					bitrate: l.target.load(Ordering::Relaxed),
					threads: l.threads,
					speed: (speed != i64::MIN).then_some(speed as i32),
					backend: *lock(&l.backend),
					codecs: lock(&l.codecs).clone(),
				}
			})
			.collect();
		StreamerStats {
			video_frames: shared.video_frames.load(Ordering::Relaxed),
			audio_frames: shared.audio_frames.load(Ordering::Relaxed),
			width: (size >> 32) as u32,
			height: size as u32,
			capture_ended: shared.capture_ended.load(Ordering::Relaxed),
			error: lock(&shared.error).clone(),
			captured_frames: shared.captured.load(Ordering::Relaxed),
			capture_fps: rates.capture_fps,
			convert_time: rates.convert_time,
			gpu_frames: shared.gpu_converted.load(Ordering::Relaxed),
			gpu_convert_time: rates.gpu_convert_time,
			gpu_error: lock(&shared.gpu_error).clone(),
			dropped_frames: layers.iter().map(|l| l.dropped).sum(),
			convert_threads: shared.convert_threads.load(Ordering::Relaxed),
			codec,
			layers,
			audio_level: audio.as_ref().map(|a| a.mixer.level()).unwrap_or_default(),
			audio_limiter: audio.as_ref().map_or(1.0, |a| a.mixer.limiter_gain()),
			audio_sources: audio.as_ref().map(|a| a.stats()).unwrap_or_default(),
		}
	}

	/// Stop capturing and encoding.
	pub fn stop(&mut self) {
		self.shared.stop.store(true, Ordering::Relaxed);
		self.detach();
		if let Some(mut screen) = self.screen.take() {
			screen.stop();
		}
		let audio = lock(&self.audio).take();
		if let Some(audio) = audio {
			audio.stop();
		}
		for layer in lock(&self.shared.layers).iter() {
			layer.inbox.close();
		}
		let encoders: Vec<_> = lock(&self.encoders).drain(..).collect();
		for (_, thread) in encoders {
			let _ = thread.join();
		}
		for thread in self.threads.drain(..) {
			thread.thread().unpark();
			let _ = thread.join();
		}
	}
}

impl Drop for Streamer {
	fn drop(&mut self) {
		self.stop();
	}
}

/// Layer ids must be unique.
fn check_layers(layers: &[LayerSpec]) -> Result<(), MediaError> {
	for (i, layer) in layers.iter().enumerate() {
		if layers[..i].iter().any(|l| l.id == layer.id) {
			return Err(MediaError::Config(format!("layer {} is listed twice", layer.id)));
		}
	}
	Ok(())
}

/// The capture backend for monitors and windows.
fn screen_backend(backend: &CaptureBackend) -> Result<Box<dyn ScreenCapture>, MediaError> {
	// Unused where every backend is built in.
	#[allow(unused_variables)]
	let unavailable = |backend: &'static str, reason: &str| {
		MediaError::Media(voelin_media::Error::CaptureUnavailable {
			backend,
			reason: reason.into(),
		})
	};
	Ok(match backend {
		CaptureBackend::Auto => capture::default_screen_capture()?,
		#[cfg(all(target_os = "linux", feature = "media-desktop"))]
		CaptureBackend::X11 => Box::new(voelin_media::capture::x11::X11Capture::new()),
		#[cfg(not(all(target_os = "linux", feature = "media-desktop")))]
		CaptureBackend::X11 => {
			return Err(unavailable("x11", "X11 capture is only in Linux desktop builds"));
		}
		#[cfg(all(target_os = "linux", feature = "media-desktop"))]
		CaptureBackend::Wlroots => Box::new(voelin_media::capture::wlroots::WlrootsCapture::new()),
		#[cfg(not(all(target_os = "linux", feature = "media-desktop")))]
		CaptureBackend::Wlroots => {
			return Err(unavailable("wlroots", "wlroots capture is only in Linux desktop builds"));
		}
		// Only reached where the portal is not built in.
		CaptureBackend::Portal => {
			return Err(unavailable(
				"portal",
				"the ScreenCast portal is only in Linux desktop builds",
			));
		}
	})
}

/// The stream's audio: the mixer (running on its own thread) and the
/// captures feeding its sources.
struct StreamAudio {
	mixer: MixerHandle,
	/// The video source, for [`AudioSourceKind::WindowAudio`].
	video: SourceId,
	active: Vec<ActiveSource>,
	thread: Option<JoinHandle<()>>,
}

/// An audio source in the mixer and what feeds it.
struct ActiveSource {
	spec: AudioSourceSpec,
	handle: SourceHandle,
	_feed: Option<Feed>,
	error: Option<String>,
}

impl Drop for ActiveSource {
	fn drop(&mut self) {
		// Its captures see the input close and stop.
		self.handle.remove();
	}
}

/// What feeds a source; stops when dropped.
enum Feed {
	Capture(#[allow(dead_code)] Box<dyn SourceCapture>),
	Microphone(#[allow(dead_code)] TapGuard<'static>),
}

/// Processed microphone audio (48 kHz mono) into a mixer input.
struct MicrophoneInput(SourceInput);

impl TapSink for MicrophoneInput {
	fn write(&mut self, samples: &[f32]) {
		self.0.push(samples, 1);
	}

	fn silence(&mut self, frames: usize) {
		self.0.push_silence(frames);
	}
}

impl StreamAudio {
	/// Start the mixer thread and `sources`.
	fn start(
		shared: &Arc<Shared>,
		video: &SourceId,
		sources: Vec<AudioSourceSpec>,
	) -> Result<Self, MediaError> {
		let mut encoder = VoiceEncoder::new(VoiceCodec::Music)?;
		encoder.set_bitrate(OPUS_BITRATE)?;
		let mixer =
			StreamMixer::new(MixerConfig { max_block: OPUS_FRAME, ..MixerConfig::default() });
		let handle = mixer.handle();
		let thread = spawn("voelin-stream-audio", {
			let shared = shared.clone();
			move || audio_loop(&shared, mixer, encoder)
		})?;
		let mut audio =
			Self { mixer: handle, video: video.clone(), active: Vec::new(), thread: Some(thread) };
		audio.apply(sources);
		Ok(audio)
	}

	/// Make the sources `specs`: kinds that stay keep their capture and
	/// take the new gain and mute, new ones start, the rest stop.
	fn apply(&mut self, specs: Vec<AudioSourceSpec>) {
		let mut old: Vec<Option<ActiveSource>> = self.active.drain(..).map(Some).collect();
		let mut next = Vec::with_capacity(specs.len());
		for spec in specs {
			let same =
				old.iter().position(|o| o.as_ref().is_some_and(|a| a.spec.kind == spec.kind));
			match same.and_then(|i| old[i].take()) {
				Some(mut source) => {
					source.handle.set_gain(spec.gain);
					source.handle.set_muted(spec.muted);
					source.spec = spec;
					next.push(source);
				}
				None => next.push(self.start_source(spec)),
			}
		}
		drop(old);
		self.active = next;
	}

	fn start_source(&self, spec: AudioSourceSpec) -> ActiveSource {
		let handle = self.mixer.add_source(spec.kind.label());
		handle.set_gain(spec.gain);
		handle.set_muted(spec.muted);
		let (feed, error) = match self.feed(&spec.kind, &handle) {
			Ok(feed) => (Some(feed), None),
			Err(e) => {
				warn!("stream audio source {}: {e}", spec.kind.label());
				(None, Some(e))
			}
		};
		ActiveSource { spec, handle, _feed: feed, error }
	}

	fn feed(&self, kind: &AudioSourceKind, handle: &SourceHandle) -> Result<Feed, String> {
		let playback = |filter: PlaybackFilter| {
			playback::start_playback(&filter, handle).map(Feed::Capture).map_err(|e| e.to_string())
		};
		match kind {
			AudioSourceKind::DesktopWithoutSelf => playback(PlaybackFilter::AllButSelf),
			AudioSourceKind::App(app) => playback(PlaybackFilter::App(app.clone())),
			AudioSourceKind::WindowAudio => {
				let SourceId::Window(window) = self.video else {
					return Err("no window is shared; pick the application instead".into());
				};
				let pid = playback::window_pid(window).ok_or(
					"the shared window's process is unknown here; pick the application instead",
				)?;
				playback(PlaybackFilter::App(AppMatch::Pid(pid)))
			}
			AudioSourceKind::Microphone => {
				let input = MicrophoneInput(handle.input(MIX_RATE));
				Ok(Feed::Microphone(tap::microphone().attach(Box::new(input))))
			}
			AudioSourceKind::Synthetic { hz } => {
				let tone = SineSource::new(*hz as f32, 0.05);
				playback::forward_audio(Box::new(tone), handle)
					.map(Feed::Capture)
					.map_err(|e| e.to_string())
			}
		}
	}

	/// The sources that capture nothing, and why.
	fn error(&self) -> Option<String> {
		let errors: Vec<String> = self
			.active
			.iter()
			.filter_map(|s| s.error.as_ref().map(|e| format!("{}: {e}", s.spec.kind.label())))
			.collect();
		(!errors.is_empty()).then(|| errors.join("; "))
	}

	fn stats(&self) -> Vec<AudioSourceStats> {
		self.active
			.iter()
			.map(|s| {
				let stats = s.handle.stats();
				AudioSourceStats {
					id: s.handle.id(),
					spec: s.spec.clone(),
					name: s.handle.name().to_owned(),
					level: s.handle.level(),
					state: stats.state,
					latency: stats.latency,
					underruns: stats.underruns,
					error: s.error.clone(),
				}
			})
			.collect()
	}

	/// Stop the captures and the mixer thread (the streamer's stop flag
	/// must be set).
	fn stop(mut self) {
		self.active.clear();
		if let Some(thread) = self.thread.take() {
			thread.thread().unpark();
			let _ = thread.join();
		}
	}
}

/// A layer as the capture thread sees it.
struct IngestLayer {
	layer: Arc<Layer>,
	spec: LayerSpec,
	pacer: FramePacer,
}

/// The conversion stage, on the capture backend's thread: paces frames,
/// converts and scales them for every layer that is due, and hands each
/// layer's frame to its encoder. Allocates nothing per frame once the
/// frame pools are warm.
///
/// Frames still in a DMA-BUF (the portal's) are converted and scaled on
/// the GPU instead when every encoder of every layer takes GPU frames
/// (VA-API): the CPU never reads them, and the buffer is free again as
/// soon as the GPU is done with it.
struct Ingest {
	shared: Arc<Shared>,
	pyramid: Pyramid,
	pacer: FramePacer,
	fps: u32,
	generation: u64,
	layers: Vec<IngestLayer>,
	sizes: Vec<(u32, u32)>,
	due: Vec<bool>,
	out: Vec<Option<Arc<VideoFrame>>>,
	#[cfg(all(target_os = "linux", feature = "media-desktop"))]
	gpu: GpuStage,
}

/// The GPU stage of [`Ingest`].
#[cfg(all(target_os = "linux", feature = "media-desktop"))]
#[derive(Default)]
struct GpuStage {
	/// Made for the first DMA-BUF; `Err` once it failed (the CPU converts
	/// from then on).
	converter: Option<Result<voelin_media::ffmpeg::GpuConverter, ()>>,
	layers: Vec<voelin_media::ffmpeg::GpuLayer>,
	out: Vec<Option<Arc<GpuFrame>>>,
}

impl Ingest {
	fn new(shared: Arc<Shared>) -> Self {
		let pyramid = Pyramid::new(0);
		shared.convert_threads.store(pyramid.threads(), Ordering::Relaxed);
		let fps = shared.fps.load(Ordering::Relaxed);
		Self {
			shared,
			pyramid,
			pacer: FramePacer::new(Some(fps)),
			fps,
			generation: u64::MAX,
			layers: Vec::new(),
			sizes: Vec::new(),
			due: Vec::new(),
			out: Vec::new(),
			#[cfg(all(target_os = "linux", feature = "media-desktop"))]
			gpu: GpuStage::default(),
		}
	}

	/// Whether frames go to the GPU: every layer's encoders take GPU frames
	/// and the GPU stage has not failed. On Android: the screen can render
	/// straight into the encoder (`mediacodec`'s surface path).
	fn gpu_path(&self) -> bool {
		#[cfg(all(target_os = "linux", feature = "media-desktop"))]
		{
			!matches!(self.gpu.converter, Some(Err(())))
				&& !self.layers.is_empty()
				&& self
					.layers
					.iter()
					.all(|l| l.layer.stopped() || l.layer.gpu.load(Ordering::Relaxed) != 0)
		}
		#[cfg(target_os = "android")]
		{
			!self.layers.is_empty()
				&& self
					.layers
					.iter()
					.all(|l| l.layer.stopped() || l.layer.gpu.load(Ordering::Relaxed) != 0)
		}
		#[cfg(not(any(
			all(target_os = "linux", feature = "media-desktop"),
			target_os = "android"
		)))]
		false
	}

	/// Convert DMA-BUF `frame` on the GPU for every layer that is due; `None`
	/// if it is not taken (the backend maps it for [`FrameSink::frame`]).
	#[cfg(all(target_os = "linux", feature = "media-desktop"))]
	fn gpu_frame(&mut self, frame: &DmaBufRef) -> Option<bool> {
		use voelin_media::ffmpeg::{GpuConverter, GpuLayer};

		if self.shared.stopped() {
			return Some(false);
		}
		if !self.gpu_path() {
			return None;
		}
		let shared = self.shared.clone();
		let converter = match &mut self.gpu.converter {
			Some(Ok(converter)) => converter,
			Some(Err(())) => return None,
			None => match GpuConverter::new() {
				Ok(converter) => self.gpu.converter.insert(Ok(converter)).as_mut().ok()?,
				Err(e) => {
					warn!("no GPU conversion of captured frames, the CPU converts them: {e}");
					*lock(&shared.gpu_error) = Some(e.to_string());
					self.gpu.converter = Some(Err(()));
					return None;
				}
			},
		};
		self.pacer.keep(frame.timestamp);
		shared.captured.fetch_add(1, Ordering::Relaxed);
		shared
			.size
			.store(u64::from(frame.width) << 32 | u64::from(frame.height), Ordering::Relaxed);
		if shared.sink().is_none() {
			return Some(true);
		}
		let mut any = false;
		self.gpu.layers.clear();
		for l in &mut self.layers {
			let due = !l.layer.stopped() && l.pacer.take(frame.timestamp);
			any |= due;
			let alignment = l.layer.gpu.load(Ordering::Relaxed);
			self.gpu.layers.push(GpuLayer {
				size: l.spec.output_size(frame.width, frame.height),
				alignment: ((alignment >> 32) as u32, alignment as u32),
				due,
			});
		}
		if !any {
			return Some(true);
		}
		self.gpu.out.clear();
		self.gpu.out.resize(self.layers.len(), None);
		let started = Instant::now();
		if let Err(e) = converter.convert(frame, &self.gpu.layers, &mut self.gpu.out) {
			warn!("GPU conversion failed, the CPU converts captured frames from now on: {e}");
			*lock(&shared.gpu_error) = Some(e.to_string());
			self.gpu.converter = Some(Err(()));
			self.gpu.out.clear();
			// This frame is counted and its layers' pacing taken: it is
			// dropped, and the next one takes the CPU path.
			return Some(true);
		}
		shared.gpu_convert_ns.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
		shared.gpu_converted.fetch_add(1, Ordering::Relaxed);
		for (l, out) in self.layers.iter().zip(&mut self.gpu.out) {
			if let Some(frame) = out.take() {
				l.layer.gpu_inbox.put(frame);
			}
		}
		Some(true)
	}

	/// Pick up layer and frame-rate changes.
	fn refresh(&mut self) {
		let fps = self.shared.fps.load(Ordering::Relaxed);
		if fps != self.fps {
			self.fps = fps;
			self.pacer.set_fps(Some(fps));
		}
		let generation = self.shared.generation.load(Ordering::Relaxed);
		if generation == self.generation {
			return;
		}
		self.generation = generation;
		let layers = lock(&self.shared.layers).clone();
		let old = std::mem::take(&mut self.layers);
		for layer in layers {
			let spec = lock(&layer.spec).clone();
			let fps = spec.max_fps.map(|cap| cap.min(self.fps));
			// Keep a layer's pacing across changes.
			let pacer = match old.iter().find(|l| l.layer.id == layer.id) {
				Some(l) => {
					let mut pacer = l.pacer.clone();
					pacer.set_fps(fps);
					pacer
				}
				None => FramePacer::new(fps),
			};
			self.layers.push(IngestLayer { layer, spec, pacer });
		}
		self.sizes.resize(self.layers.len(), (0, 0));
		self.due.resize(self.layers.len(), false);
		self.out.resize(self.layers.len(), None);
	}
}

impl FrameSink for Ingest {
	fn max_fps(&self) -> u32 {
		self.shared.fps.load(Ordering::Relaxed).max(1)
	}

	fn wants(&mut self, timestamp: Duration) -> bool {
		if self.shared.stopped() {
			return false;
		}
		// Also the layers, which `accepts_dmabuf` (asked next) looks at.
		self.refresh();
		self.pacer.due(timestamp)
	}

	fn accepts_dmabuf(&self) -> bool {
		cfg!(all(target_os = "linux", feature = "media-desktop")) && self.gpu_path()
	}

	fn accepts_gpu(&self) -> bool {
		cfg!(target_os = "android") && self.gpu_path()
	}

	/// Android: the screen goes straight into each due layer's encoder,
	/// which draws it at the layer's size; only its size and time come here.
	#[cfg(target_os = "android")]
	fn gpu(&mut self, frame: GpuFrame) -> bool {
		if self.shared.stopped() {
			return false;
		}
		self.pacer.keep(frame.timestamp);
		self.refresh();
		let shared = &self.shared;
		shared.captured.fetch_add(1, Ordering::Relaxed);
		shared
			.size
			.store(u64::from(frame.width) << 32 | u64::from(frame.height), Ordering::Relaxed);
		if shared.sink().is_none() {
			return true;
		}
		for l in &mut self.layers {
			if !l.layer.stopped() && l.pacer.take(frame.timestamp) {
				let (width, height) = l.spec.output_size(frame.width, frame.height);
				l.layer.gpu_inbox.put(Arc::new(frame.sized(width, height)));
			}
		}
		shared.gpu_converted.fetch_add(1, Ordering::Relaxed);
		true
	}

	/// The tiled layout the GPU's own RGB surfaces have, once the GPU stage
	/// runs (its first DMA-BUF is LINEAR); none on the CPU path, which
	/// cannot read tiled buffers.
	fn dmabuf_modifiers(&self) -> &[u64] {
		#[cfg(all(target_os = "linux", feature = "media-desktop"))]
		if self.gpu_path()
			&& let Some(Ok(converter)) = &self.gpu.converter
		{
			return converter.modifiers();
		}
		&[]
	}

	fn dmabuf(&mut self, frame: &DmaBufRef) -> Option<bool> {
		#[cfg(all(target_os = "linux", feature = "media-desktop"))]
		return self.gpu_frame(frame);
		#[cfg(not(all(target_os = "linux", feature = "media-desktop")))]
		{
			let _ = frame;
			None
		}
	}

	fn frame(&mut self, frame: FrameRef<'_>) -> bool {
		if self.shared.stopped() {
			return false;
		}
		self.pacer.keep(frame.timestamp);
		self.refresh();
		let shared = &self.shared;
		shared.captured.fetch_add(1, Ordering::Relaxed);
		shared
			.size
			.store(u64::from(frame.width) << 32 | u64::from(frame.height), Ordering::Relaxed);
		if shared.sink().is_none() {
			return true;
		}
		let mut any = false;
		for (i, l) in self.layers.iter_mut().enumerate() {
			self.sizes[i] = l.spec.output_size(frame.width, frame.height);
			self.due[i] = !l.layer.stopped() && l.pacer.take(frame.timestamp);
			any |= self.due[i];
		}
		if !any {
			return true;
		}
		let started = Instant::now();
		if let Err(e) = self.pyramid.process(&frame, &self.sizes, &self.due, &mut self.out) {
			warn!("cannot convert a captured frame: {e}");
			shared.set_error(e);
			return true;
		}
		shared.convert_ns.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
		shared.converted.fetch_add(1, Ordering::Relaxed);
		for (l, out) in self.layers.iter().zip(&mut self.out) {
			if let Some(frame) = out.take() {
				l.layer.inbox.put(frame);
			}
		}
		true
	}
}

impl Drop for Ingest {
	fn drop(&mut self) {
		if !self.shared.stopped() {
			debug!("screen capture ended");
			self.shared.capture_ended.store(true, Ordering::Relaxed);
		}
	}
}

/// A layer's picture: in memory, or on the GPU (converted from a captured
/// DMA-BUF there).
#[derive(Clone)]
enum Picture {
	Cpu(Arc<VideoFrame>),
	Gpu(Arc<GpuFrame>),
}

impl Picture {
	fn size(&self) -> (u32, u32) {
		match self {
			Self::Cpu(frame) => (frame.width, frame.height),
			Self::Gpu(frame) => (frame.width, frame.height),
		}
	}

	/// The same picture `later` after it was captured (a still screen sent
	/// again; copies a frame in memory).
	fn later(&self, later: Duration) -> Self {
		match self {
			Self::Cpu(frame) => {
				let timestamp = frame.timestamp + later;
				Self::Cpu(Arc::new((**frame).clone().with_timestamp(timestamp)))
			}
			Self::Gpu(frame) => Self::Gpu(Arc::new(frame.at(frame.timestamp + later))),
		}
	}
}

/// A layer's next picture, from either inbox, or `None` after [`POLL`] or
/// once the layer is closed. The encoder thread is registered with both
/// ([`Handoff::register`]), so a `put` to either wakes it.
fn next_picture(layer: &Layer) -> Option<Picture> {
	let deadline = Instant::now() + POLL;
	loop {
		if let Some(frame) = layer.gpu_inbox.take() {
			return Some(Picture::Gpu(frame));
		}
		if let Some(frame) = layer.inbox.take() {
			return Some(Picture::Cpu(frame));
		}
		let now = Instant::now();
		if now >= deadline || layer.inbox.is_closed() {
			return None;
		}
		std::thread::park_timeout(deadline - now);
	}
}

/// What every encoder of a layer takes GPU frames at, for [`Layer::gpu`]:
/// the largest alignment of them all, or 0 if one of them takes none.
fn gpu_alignment<'a>(encoders: impl IntoIterator<Item = &'a dyn VideoEncoder>) -> u64 {
	let mut alignment = (2, 2);
	for encoder in encoders {
		let Some((w, h)) = encoder.gpu_alignment() else { return 0 };
		alignment = (alignment.0.max(w), alignment.1.max(h));
	}
	u64::from(alignment.0) << 32 | u64::from(alignment.1)
}

/// Which of a studio's outputs get an encoder's packets.
#[derive(Clone, Copy)]
enum ToStudio<'a> {
	None,
	/// The stream codec's: all of them, recordings and replay buffer too.
	All(&'a Studio),
	/// Another codec's: the outputs that asked for it
	/// ([`Studio::output_codecs`]).
	Asked(&'a Studio),
}

/// One encoder of a layer: its keyframe and bitrate state.
struct LayerEncoder {
	codec: Codec,
	/// `None`: creating it failed (not retried until it is needed anew).
	encoder: Option<Box<dyn VideoEncoder>>,
	/// A requested keyframe the encoder has not produced yet (rate control
	/// may skip a frame).
	keyframe_due: bool,
	/// The bitrate and frame rate it was last set to.
	bitrate: u64,
	fps: u32,
}

impl LayerEncoder {
	fn new(encoder: Box<dyn VideoEncoder>, fps: u32) -> Self {
		Self { codec: encoder.codec(), encoder: Some(encoder), keyframe_due: true, bitrate: 0, fps }
	}

	/// Encode `picture` and hand the frames to `sink` (and `studio`'s
	/// outputs); follows the layer's target bitrate and frame rate first.
	#[allow(clippy::too_many_arguments)]
	fn encode(
		&mut self,
		shared: &Shared,
		layer: &Layer,
		sink: &dyn MediaSink,
		studio: ToStudio<'_>,
		picture: &Picture,
		requested: bool,
		target: u64,
	) {
		let codec = self.codec;
		let Some(encoder) = &mut self.encoder else { return };
		if matches!(picture, Picture::Gpu(_)) && encoder.gpu_alignment().is_none() {
			// The capture switches to frames in memory once it sees this
			// encoder (made for a viewer just now); until then it waits, and
			// its first frame then is a keyframe.
			self.keyframe_due = true;
			return;
		}
		let (width, height) = picture.size();
		if target.abs_diff(self.bitrate) * 100 > self.bitrate {
			match encoder.set_bitrate(target.min(u64::from(u32::MAX)) as u32) {
				Ok(()) => self.bitrate = target,
				Err(e) => warn!(layer = layer.id, %codec, "cannot change the bitrate: {e}"),
			}
		}
		let want_fps = layer.fps.load(Ordering::Relaxed);
		if want_fps != self.fps && encoder.set_fps(want_fps).is_ok() {
			self.fps = want_fps;
		}
		let force = requested || self.keyframe_due;
		let mut produced_keyframe = false;
		let mut out = |chunk: EncodedChunk<'_>| {
			produced_keyframe |= chunk.keyframe;
			let packet = Packet {
				track: Track::Video { codec, layer: u32::from(layer.id) },
				pts_90khz: chunk.pts_90khz,
				keyframe: chunk.keyframe,
				width,
				height,
				data: chunk.data,
			};
			match studio {
				ToStudio::All(studio) => studio.write_packet(&packet),
				ToStudio::Asked(studio) => studio.write_output_packet(&packet),
				ToStudio::None => {}
			}
			let encoded = EncodedFrame {
				kind: MediaKind::Video,
				time: MediaTime::from_90khz(chunk.pts_90khz),
				// The one allocation per encoded frame.
				data: Arc::from(chunk.data),
				layer: layer.id,
				keyframe: chunk.keyframe,
			};
			if sink.send_video(encoded, codec) {
				layer.frames.fetch_add(1, Ordering::Relaxed);
				layer.keyframes.fetch_add(u64::from(chunk.keyframe), Ordering::Relaxed);
				layer.bytes.fetch_add(chunk.data.len() as u64, Ordering::Relaxed);
				shared.video_frames.fetch_add(1, Ordering::Relaxed);
			}
		};
		let result = match picture {
			Picture::Cpu(frame) => encoder.encode_with(frame, force, &mut out),
			Picture::Gpu(frame) => encoder.encode_gpu(frame, force, &mut out),
		};
		match result {
			Ok(()) => self.keyframe_due = force && !produced_keyframe,
			Err(e) => {
				warn!(layer = layer.id, %codec, "video encoding failed: {e}");
				self.keyframe_due = force;
				shared.set_error(e);
			}
		}
	}
}

/// An encoder of `codec` for `layer` (a codec a viewer chose), with the
/// streamer's current codecs and preference.
fn extra_encoder(shared: &Shared, layer: &Layer, codec: Codec) -> Option<Box<dyn VideoEncoder>> {
	let guard = lock(&shared.encoders);
	let (codecs, preference) = guard.as_ref()?;
	let spec = lock(&layer.spec).clone();
	let config = encoder_config(&spec, layer.fps.load(Ordering::Relaxed), layer.threads);
	match codecs.new_encoder_preferring(codec, config, preference) {
		Ok(encoder) => {
			debug!(layer = layer.id, %codec, backend = %encoder.backend(), "encoder for viewers");
			Some(encoder)
		}
		Err(e) => {
			warn!(layer = layer.id, %codec, "no encoder for viewers of this codec: {e}");
			None
		}
	}
}

/// One layer's encoder thread: the stream codec's encoder, plus one per
/// other codec the sink's viewers chose (created when a viewer needs it,
/// dropped when none does).
fn encode_loop(shared: &Shared, layer: &Layer, encoder: Box<dyn VideoEncoder>) {
	let fps = layer.fps.load(Ordering::Relaxed);
	let mut primary = LayerEncoder::new(encoder, fps);
	// The first frame is a keyframe anyway; no need to insist.
	primary.keyframe_due = false;
	primary.bitrate = layer.bitrate.load(Ordering::Relaxed);
	*lock(&layer.backend) = primary.encoder.as_ref().map(|e| e.backend());
	let mut extra: Vec<LayerEncoder> = Vec::new();
	let mut wanted: Vec<Codec> = Vec::new();
	let mut generation = shared.encoders_generation.load(Ordering::Relaxed);
	// Whether the stream codec was encoded for the last frame.
	let mut primary_on = true;
	// The last picture, and when it came: a static screen sends no frames,
	// so keyframe requests are answered by encoding it again.
	let mut last: Option<(Picture, Instant)> = None;
	layer.speed.store(
		primary.encoder.as_ref().and_then(|e| e.speed()).map_or(i64::MIN, i64::from),
		Ordering::Relaxed,
	);
	// The capture may hand over GPU frames once every encoder takes them.
	layer.inbox.register();
	layer.gpu_inbox.register();
	layer.gpu.store(gpu_alignment(primary.encoder.as_deref()), Ordering::Relaxed);
	while !shared.stopped() && !layer.stopped() {
		let picture = next_picture(layer);
		if let Some(next) = lock(&layer.next_encoder).take() {
			// Another codec: its first frame is a keyframe anyway.
			*lock(&layer.backend) = Some(next.backend());
			primary = LayerEncoder::new(next, primary.fps);
		}
		let Some(sink) = shared.sink() else { continue };
		shared.poll_keyframes(&*sink);
		if shared.studio.as_ref().is_some_and(|s| s.needs_keyframe(u32::from(layer.id))) {
			layer.keyframe.store(true, Ordering::Relaxed);
		}
		// The codecs viewers chose; none known: the stream codec.
		wanted.clear();
		sink.video_codecs(&mut wanted);
		// With a studio, the stream codec for its recordings and replay
		// buffer, and the codecs its outputs of this layer need (RTMP: H.264).
		if let Some(studio) = &shared.studio {
			if !wanted.contains(&primary.codec) {
				wanted.insert(0, primary.codec);
			}
			studio.output_codecs(u32::from(layer.id), &mut wanted);
		}
		let now = shared.encoders_generation.load(Ordering::Relaxed);
		if now != generation {
			generation = now;
			extra.clear();
		}
		extra.retain(|e| wanted.contains(&e.codec));
		for &codec in &wanted {
			if codec != primary.codec && !extra.iter().any(|e| e.codec == codec) {
				let fps = layer.fps.load(Ordering::Relaxed);
				extra.push(match extra_encoder(shared, layer, codec) {
					Some(encoder) => LayerEncoder::new(encoder, fps),
					None => {
						LayerEncoder { codec, encoder: None, keyframe_due: false, bitrate: 0, fps }
					}
				});
			}
		}
		let primary_wanted = wanted.is_empty() || wanted.contains(&primary.codec);
		if primary_wanted && !primary_on {
			// Frames were skipped: the next one must stand alone.
			primary.keyframe_due = true;
		}
		primary_on = primary_wanted;
		{
			let mut codecs = lock(&layer.codecs);
			codecs.clear();
			codecs.extend(primary_on.then_some(primary.codec));
			codecs.extend(extra.iter().filter(|e| e.encoder.is_some()).map(|e| e.codec));
		}
		// The stream codec's encoder counts even while nobody takes it, so
		// the capture does not switch paths as viewers come and go.
		let encoders = primary.encoder.as_deref().into_iter();
		let gpu = gpu_alignment(encoders.chain(extra.iter().filter_map(|e| e.encoder.as_deref())));
		layer.gpu.store(gpu, Ordering::Relaxed);
		let requested = layer.keyframe.swap(false, Ordering::Relaxed);
		let due = primary_on && primary.keyframe_due
			|| extra.iter().any(|e| e.keyframe_due && e.encoder.is_some());
		let picture = match picture {
			Some(picture) => {
				last = Some((picture.clone(), Instant::now()));
				picture
			}
			None if requested || due => {
				let Some((picture, at)) = &last else {
					layer.keyframe.store(requested, Ordering::Relaxed);
					continue;
				};
				// Same picture, later timestamp (rare).
				picture.later(at.elapsed())
			}
			None => continue,
		};
		// Follow the layer's bitrate, or what the viewers' estimates allow.
		let configured = layer.bitrate.load(Ordering::Relaxed);
		let cap = match layer.max_bitrate.load(Ordering::Relaxed) {
			0 => u64::MAX,
			max => max,
		};
		let target = sink.layer_bitrate(layer.id).unwrap_or(configured).clamp(1, cap);
		let started = Instant::now();
		if primary_on {
			let studio = shared.studio.as_deref().map_or(ToStudio::None, ToStudio::All);
			primary.encode(shared, layer, &*sink, studio, &picture, requested, target);
			layer.target.store(primary.bitrate, Ordering::Relaxed);
		}
		for encoder in &mut extra {
			let studio = shared.studio.as_deref().map_or(ToStudio::None, ToStudio::Asked);
			encoder.encode(shared, layer, &*sink, studio, &picture, requested, target);
		}
		layer.encode_ns.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
		layer.encoded.fetch_add(1, Ordering::Relaxed);
		let (width, height) = picture.size();
		layer.size.store(u64::from(width) << 32 | u64::from(height), Ordering::Relaxed);
		layer.speed.store(
			primary.encoder.as_ref().and_then(|e| e.speed()).map_or(i64::MIN, i64::from),
			Ordering::Relaxed,
		);
	}
}

/// Counter values at the last stats sample.
#[derive(Clone, Copy, Default)]
struct Sample {
	captured: u64,
	converted: u64,
	convert_ns: u64,
	gpu_converted: u64,
	gpu_convert_ns: u64,
}

/// Once a second: rates for [`Streamer::stats`]; every five seconds a
/// summary at debug level.
fn stats_loop(shared: &Shared) {
	const PERIOD: Duration = Duration::from_secs(1);
	let mut last = Sample::default();
	// Per layer: id, frames, bytes, encoded, encode_ns.
	let mut last_layers: Vec<(LayerId, u64, u64, u64, u64)> = Vec::new();
	let mut at = Instant::now();
	let mut ticks = 0u32;
	while !shared.stopped() {
		std::thread::park_timeout(PERIOD.saturating_sub(at.elapsed()));
		if shared.stopped() {
			break;
		}
		let elapsed = at.elapsed();
		if elapsed < PERIOD {
			continue;
		}
		at = Instant::now();
		let secs = elapsed.as_secs_f64();
		let now = Sample {
			captured: shared.captured.load(Ordering::Relaxed),
			converted: shared.converted.load(Ordering::Relaxed),
			convert_ns: shared.convert_ns.load(Ordering::Relaxed),
			gpu_converted: shared.gpu_converted.load(Ordering::Relaxed),
			gpu_convert_ns: shared.gpu_convert_ns.load(Ordering::Relaxed),
		};
		let mean = |ns: u64, n: u64| Duration::from_nanos(ns.checked_div(n).unwrap_or(0));
		let mut rates = lock(&shared.rates);
		rates.capture_fps = (now.captured - last.captured) as f64 / secs;
		rates.convert_time = mean(now.convert_ns - last.convert_ns, now.converted - last.converted);
		rates.gpu_convert_time =
			mean(now.gpu_convert_ns - last.gpu_convert_ns, now.gpu_converted - last.gpu_converted);
		rates.layers.clear();
		let layers = lock(&shared.layers);
		for layer in layers.iter() {
			let current = (
				layer.id,
				layer.frames.load(Ordering::Relaxed),
				layer.bytes.load(Ordering::Relaxed),
				layer.encoded.load(Ordering::Relaxed),
				layer.encode_ns.load(Ordering::Relaxed),
			);
			let before = last_layers
				.iter()
				.find(|l| l.0 == layer.id)
				.copied()
				.unwrap_or((layer.id, 0, 0, 0, 0));
			rates.layers.push((
				layer.id,
				(current.1 - before.1) as f64 / secs,
				(current.2 - before.2) as f64 * 8.0 / secs / 1000.0,
				mean(current.4 - before.4, current.3 - before.3),
			));
		}
		last_layers.clear();
		last_layers.extend(layers.iter().map(|l| {
			(
				l.id,
				l.frames.load(Ordering::Relaxed),
				l.bytes.load(Ordering::Relaxed),
				l.encoded.load(Ordering::Relaxed),
				l.encode_ns.load(Ordering::Relaxed),
			)
		}));
		last = now;
		ticks += 1;
		if ticks.is_multiple_of(5) && tracing::enabled!(tracing::Level::DEBUG) {
			let summary: Vec<String> = rates
				.layers
				.iter()
				.map(|(id, fps, kbps, encode)| {
					format!(
						"L{id} {fps:.1} fps {kbps:.0} kbit/s enc {:.1} ms",
						encode.as_secs_f64() * 1e3
					)
				})
				.collect();
			debug!(
				"stream: capture {:.1} fps, convert {:.1} ms (GPU {:.1} ms), {}",
				rates.capture_fps,
				rates.convert_time.as_secs_f64() * 1e3,
				rates.gpu_convert_time.as_secs_f64() * 1e3,
				summary.join(", ")
			);
		}
	}
}

/// The mixer thread: every 20 ms (on the monotonic clock) one Opus frame
/// of the mix, encoded while a sink is attached. The RTP time is the
/// frame's number since the start, so it follows real time across
/// silences and stalls. A studio's streamer counts from the studio's epoch
/// instead, the clock of its video, so its recordings stay in sync.
fn audio_loop(shared: &Shared, mut mixer: StreamMixer, mut encoder: VoiceEncoder) {
	let start = shared.studio.as_ref().map_or_else(Instant::now, |s| s.epoch());
	let mut clock = BlockClock::starting_at(start, OPUS_FRAME);
	let mut block = vec![0.0f32; OPUS_FRAME * 2];
	while !shared.stopped() {
		let now = Instant::now();
		let due = clock.due(now);
		if due.is_empty() {
			std::thread::park_timeout(clock.next().saturating_duration_since(now).min(POLL));
			continue;
		}
		for frame in due {
			// Mixed even without a sink, so the sources' buffers keep moving.
			mixer.mix(&mut block);
			let Some(sink) = shared.sink() else { continue };
			let data: Arc<[u8]> = match encoder.encode_to_bytes(&block) {
				Ok(data) => data.into(),
				Err(e) => {
					warn!("audio encoding failed: {e}");
					continue;
				}
			};
			if let Some(studio) = &shared.studio {
				studio.write_packet(&Packet {
					track: Track::Audio { channels: 2 },
					pts_90khz: frame * OPUS_FRAME as u64 * 90_000 / u64::from(MIX_RATE),
					keyframe: true,
					width: 0,
					height: 0,
					data: &data,
				});
			}
			let time = MediaTime::new(frame * OPUS_FRAME as u64, Frequency::FORTY_EIGHT_KHZ);
			let frame =
				EncodedFrame { kind: MediaKind::Audio, time, data, layer: 0, keyframe: false };
			if sink.send(frame) {
				shared.audio_frames.fetch_add(1, Ordering::Relaxed);
			}
		}
	}
}

/// Collects a [`Streamer`]'s frames for [`EncodedSource`].
struct ChannelSink {
	tx: std_mpsc::Sender<EncodedFrame>,
	/// A keyframe on every layer was asked for.
	keyframe: AtomicBool,
	/// Layers a keyframe was asked for.
	layers: Mutex<LayerSet>,
	/// What the viewers' bandwidth estimates allow, per layer.
	bitrates: Mutex<Vec<(LayerId, u64)>>,
	shared: Arc<Shared>,
}

impl MediaSink for ChannelSink {
	fn send(&self, frame: EncodedFrame) -> bool {
		self.tx.send(frame).is_ok()
	}

	fn take_keyframe_request(&self) -> bool {
		self.keyframe.swap(false, Ordering::Relaxed)
	}

	fn take_layer_keyframes(&self, layers: &mut LayerSet) {
		if self.keyframe.swap(false, Ordering::Relaxed) {
			for layer in lock(&self.shared.layers).iter() {
				layers.insert(layer.id);
			}
		}
		let mut requested = lock(&self.layers);
		layers.union_with(&requested);
		requested.clear();
	}

	fn layer_bitrate(&self, layer: LayerId) -> Option<u64> {
		lock(&self.bitrates).iter().find(|(id, _)| *id == layer).map(|(_, bps)| *bps)
	}
}

/// A [`Streamer`] as a [`FrameSource`], for code that runs
/// `voelin_stream::Streams` itself (e.g. voelinctl).
pub struct EncodedSource {
	streamer: Streamer,
	frames: std_mpsc::Receiver<EncodedFrame>,
	sink: Arc<ChannelSink>,
}

impl EncodedSource {
	pub fn new(streamer: Streamer) -> Self {
		let (tx, frames) = std_mpsc::channel();
		let sink = Arc::new(ChannelSink {
			tx,
			keyframe: AtomicBool::new(true),
			layers: Mutex::new(LayerSet::new()),
			bitrates: Mutex::new(Vec::new()),
			shared: streamer.shared.clone(),
		});
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

	fn request_layer_keyframe(&mut self, layer: LayerId) {
		lock(&self.sink.layers).insert(layer);
	}

	fn set_layer_bitrate(&mut self, layer: LayerId, bitrate: u64) {
		let mut bitrates = lock(&self.sink.bitrates);
		match bitrates.iter_mut().find(|(id, _)| *id == layer) {
			Some(entry) => entry.1 = bitrate,
			None => bitrates.push((layer, bitrate)),
		}
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
		// IRAP pictures (16-23) or a VPS / SPS in front of one.
		Codec::H265 => data
			.windows(4)
			.filter_map(|w| (w[..3] == [0, 0, 1]).then_some((w[3] >> 1) & 0x3f))
			.any(|t| (16..=23).contains(&t) || t == 32 || t == 33),
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
	/// The decoder in use ([`Codecs::decoders_for`]: the first of the
	/// codec's that works).
	pub decoder: Option<DecoderBackend>,
	pub width: u32,
	pub height: u32,
	/// The last error (no decoder for the codec, decoding failed).
	pub error: Option<String>,
	/// Bytes of video received.
	pub bytes: u64,
	/// Pictures decoded per second and video received in bit/s, over the
	/// time since the previous [`VideoPipeline::stats`] (at least
	/// [`RATE_WINDOW`]; the watch screen asks once a second).
	pub fps: u32,
	pub bitrate: u64,
}

/// Where the rates of [`DecodeStats`] were last measured from.
#[derive(Default)]
struct DecodeRates {
	since: Option<Instant>,
	decoded: u64,
	bytes: u64,
	fps: u32,
	bitrate: u64,
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
	rates: Mutex<DecodeRates>,
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
		lock(&self.queue.stats).bytes += frame.data.len() as u64;
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
	/// `on_frame` gets every decoded picture on the decoding thread, from a
	/// pool: a picture comes back to it once the consumer drops it, so a
	/// consumer that converts and drops each one costs no allocations.
	/// `request_keyframe` is called when the decoder needs one (every half
	/// second until one comes).
	pub fn new(
		codecs: Arc<Codecs>,
		on_frame: impl FnMut(Arc<VideoFrame>) + Send + 'static,
		request_keyframe: impl Fn() + Send + 'static,
	) -> Self {
		let queue = Arc::new(DecodeQueue::default());
		let thread = std::thread::Builder::new()
			.name("voelin-stream-decode".into())
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
		let mut stats = lock(&self.input.queue.stats).clone();
		let mut rates = lock(&self.input.queue.rates);
		let now = Instant::now();
		let elapsed = rates.since.map(|since| now.saturating_duration_since(since));
		if elapsed.is_none_or(|e| e >= RATE_WINDOW) {
			if let Some(seconds) = elapsed.map(|e| e.as_secs_f64()) {
				rates.fps = ((stats.decoded - rates.decoded) as f64 / seconds).round() as u32;
				rates.bitrate = ((stats.bytes - rates.bytes) as f64 * 8.0 / seconds) as u64;
			}
			(rates.since, rates.decoded, rates.bytes) = (Some(now), stats.decoded, stats.bytes);
		}
		(stats.fps, stats.bitrate) = (rates.fps, rates.bitrate);
		stats
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

/// The decoder of a [`VideoPipeline`] and the rest of its codec's ladder.
struct ActiveDecoder {
	codec: Codec,
	backend: DecoderBackend,
	decoder: Box<dyn voelin_media::VideoDecoder>,
	/// The codec's other decoders, best first, still to try.
	rest: VecDeque<DecoderBackend>,
	/// Failures since the last picture.
	failures: u32,
	/// Frames taken since the last picture or failure.
	without_picture: u32,
}

impl ActiveDecoder {
	/// The first decoder of `codec`'s ladder that opens.
	fn start(codecs: &Codecs, codec: Codec) -> Result<Self, String> {
		let mut rest: VecDeque<DecoderBackend> = codecs.decoders_for(codec).into();
		let (backend, decoder) = Self::open_next(codecs, codec, &mut rest).map_err(|e| {
			e.unwrap_or_else(|| {
				codecs
					.check_decoder(codec)
					.err()
					.map_or_else(|| format!("no decoder for {codec}"), |e| e.to_string())
			})
		})?;
		Ok(Self { codec, backend, decoder, rest, failures: 0, without_picture: 0 })
	}

	/// The next decoder of `rest` that opens; the last error if none did.
	fn open_next(
		codecs: &Codecs,
		codec: Codec,
		rest: &mut VecDeque<DecoderBackend>,
	) -> Result<(DecoderBackend, Box<dyn voelin_media::VideoDecoder>), Option<String>> {
		let mut error = None;
		while let Some(backend) = rest.pop_front() {
			match codecs.new_decoder_with(codec, backend) {
				Ok(decoder) => return Ok((backend, decoder)),
				Err(e) => {
					warn!(%codec, %backend, "the decoder does not open: {e}");
					error = Some(e.to_string());
				}
			}
		}
		Err(error)
	}

	/// Replace the decoder by the next one of the ladder, after `reason`;
	/// `false` (keeping this one) if there is none.
	fn fall_back(&mut self, codecs: &Codecs, reason: &str) -> bool {
		(self.failures, self.without_picture) = (0, 0);
		let (codec, failed) = (self.codec, self.backend);
		match Self::open_next(codecs, codec, &mut self.rest) {
			Ok((backend, decoder)) => {
				warn!(%codec, %failed, %backend, "the decoder failed ({reason}); trying the next");
				(self.backend, self.decoder) = (backend, decoder);
				true
			}
			Err(_) => {
				warn!(%codec, %failed, "the decoder fails ({reason}), and no other is left");
				false
			}
		}
	}
}

/// A pooled picture nobody else holds, to decode into: one the consumer
/// gave back, else a new one (when the pool is full, the oldest is left to
/// whoever holds it).
fn free_picture(pictures: &mut Vec<Arc<VideoFrame>>) -> usize {
	if let Some(i) = pictures.iter_mut().position(|p| Arc::get_mut(p).is_some()) {
		return i;
	}
	if pictures.len() >= PICTURE_POOL {
		pictures.remove(0);
	}
	pictures.push(Arc::new(VideoFrame::black_i420(0, 0)));
	pictures.len() - 1
}

fn decode_loop(
	queue: &DecodeQueue,
	codecs: &Codecs,
	mut on_frame: impl FnMut(Arc<VideoFrame>),
	request_keyframe: impl Fn(),
) {
	let mut active: Option<ActiveDecoder> = None;
	// Start at a keyframe; the streamer sends one when a viewer connects.
	let mut waiting = true;
	// A keyframe is wanted (after a loss, or an error of a decoder that goes
	// on), asked for until one decodes.
	let mut wanted = false;
	let mut last_request: Option<Instant> = None;
	let mut pictures: Vec<Arc<VideoFrame>> = Vec::with_capacity(PICTURE_POOL);
	let mut request = |stats: &Mutex<DecodeStats>| {
		if last_request.is_none_or(|t| t.elapsed() >= KEYFRAME_RETRY) {
			last_request = Some(Instant::now());
			lock(stats).keyframe_requests += 1;
			request_keyframe();
		}
	};
	let set_error = |message: String| lock(&queue.stats).error = Some(message);
	loop {
		let next = {
			let mut state = lock(&queue.state);
			if state.frames.is_empty() && !state.closed {
				state =
					queue.cond.wait_timeout(state, POLL).unwrap_or_else(PoisonError::into_inner).0;
			}
			if state.closed {
				return;
			}
			state.frames.pop_front().map(|frame| (frame, std::mem::take(&mut state.lost)))
		};
		// Asked for again while none comes, even while no frame does.
		if (waiting || wanted) && active.is_some() {
			request(&queue.stats);
		}
		let Some((frame, lost)) = next else { continue };
		let codec = match Codec::try_from(frame.codec) {
			Ok(codec) => codec,
			Err(e) => {
				set_error(e.to_string());
				continue;
			}
		};
		if active.as_ref().is_none_or(|a| a.codec != codec) {
			match ActiveDecoder::start(codecs, codec) {
				Ok(started) => {
					let mut stats = lock(&queue.stats);
					(stats.codec, stats.decoder) = (Some(codec), Some(started.backend));
					drop(stats);
					active = Some(started);
					waiting = true;
				}
				Err(e) => {
					if active.is_some() || lock(&queue.stats).error.is_none() {
						warn!("no decoder for the stream: {e}");
					}
					active = None;
					set_error(e);
					continue;
				}
			}
		}
		let Some(dec) = &mut active else { continue };
		if lost || !frame.contiguous {
			// A decoder that conceals the loss goes on; the others would
			// fail, or show garbage, until a keyframe.
			if dec.decoder.conceals_errors() {
				wanted = true;
			} else {
				waiting = true;
			}
		}
		let keyframe = is_keyframe(codec, &frame.data);
		if waiting {
			if !keyframe {
				lock(&queue.stats).skipped += 1;
				request(&queue.stats);
				continue;
			}
			waiting = false;
		}
		loop {
			let slot = free_picture(&mut pictures);
			let picture = Arc::get_mut(&mut pictures[slot]).expect("a free picture");
			let failure = match dec.decoder.decode_into(&frame.data, picture) {
				Ok(true) => {
					(dec.failures, dec.without_picture) = (0, 0);
					wanted &= !keyframe;
					{
						let mut stats = lock(&queue.stats);
						stats.decoded += 1;
						(stats.width, stats.height) = (picture.width, picture.height);
					}
					on_frame(pictures[slot].clone());
					None
				}
				Ok(false) => {
					// Decoders with a delay (B-frames) give it a few frames
					// later; one that gives nothing gets keyframes to try.
					dec.without_picture += 1;
					(dec.without_picture >= FRAMES_WITHOUT_PICTURE).then(|| {
						dec.without_picture = 0;
						wanted = true;
						request(&queue.stats);
						format!("no picture from {FRAMES_WITHOUT_PICTURE} frames")
					})
				}
				Err(e) => {
					debug!("decoding failed: {e}");
					set_error(e.to_string());
					if dec.decoder.conceals_errors() {
						wanted = true;
					} else {
						waiting = true;
					}
					request(&queue.stats);
					Some(e.to_string())
				}
			};
			let Some(reason) = failure else { break };
			dec.failures += 1;
			if dec.failures < DECODER_FAILURES || !dec.fall_back(codecs, &reason) {
				break;
			}
			lock(&queue.stats).decoder = Some(dec.backend);
			if !keyframe {
				waiting = true;
				request(&queue.stats);
				break;
			}
			// The keyframe that failed starts the next decoder at once.
			waiting = false;
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
		on_frame: impl FnMut(Arc<VideoFrame>) + Send + 'static,
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

/// Feeds one layer of a [`Streamer`]'s video straight into a
/// [`VideoPipeline`].
struct PipelineSink {
	input: FrameInput,
	codec: voelin_stream::Codec,
	layer: LayerId,
	keyframe: Arc<AtomicBool>,
}

impl MediaSink for PipelineSink {
	fn send(&self, frame: EncodedFrame) -> bool {
		if frame.kind != MediaKind::Video || frame.layer != self.layer {
			return true;
		}
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

	fn take_layer_keyframes(&self, layers: &mut LayerSet) {
		if self.take_keyframe_request() {
			layers.insert(self.layer);
		}
	}
}

/// Capture → encoder → decoder without a server, e.g. to preview a share or
/// to develop the viewer (`VOELIN_DEMO_STREAM` in the desktop app).
pub struct LocalPreview {
	// Dropped first: no more frames into the pipeline.
	streamer: Streamer,
	pipeline: VideoPipeline,
}

impl LocalPreview {
	pub async fn start(
		codecs: Arc<Codecs>,
		mut config: StreamerConfig,
		on_frame: impl FnMut(Arc<VideoFrame>) + Send + 'static,
	) -> Result<Self, MediaError> {
		// The audio would go nowhere.
		config.audio = false;
		let codec = config.codec;
		// The first (usually the largest) layer is shown.
		let layer = config.effective_layers()[0].id;
		let streamer = Streamer::start(&codecs, config).await?;
		let keyframe = Arc::new(AtomicBool::new(true));
		let pipeline = VideoPipeline::new(codecs, on_frame, {
			let keyframe = keyframe.clone();
			move || keyframe.store(true, Ordering::Relaxed)
		});
		let input = pipeline.input();
		streamer.attach(Arc::new(PipelineSink { input, codec: codec.into(), layer, keyframe }));
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
#[cfg(all(test, feature = "media-desktop"))]
pub(crate) fn rectangle_span(picture: &VideoFrame) -> (u32, u32) {
	use voelin_media::capture::synthetic::{BACKGROUND, RECT_COLOR};

	let rgba = voelin_media::convert::to_rgba_vec(picture).unwrap();
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
		// VP8: the 1x1 keyframe of voelin-stream's synthetic source.
		assert!(is_keyframe(Codec::Vp8, &voelin_stream::source::VP8_KEYFRAME_1X1));
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
		// HEVC: a VPS (type 32) or an IDR (19) starts one, a trailing picture not.
		assert!(is_keyframe(Codec::H265, &[0, 0, 1, 0x40, 0x01]));
		assert!(is_keyframe(Codec::H265, &[0, 0, 0, 1, 0x26, 0x01]));
		assert!(!is_keyframe(Codec::H265, &[0, 0, 1, 0x02, 0x01]));
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

	#[cfg(feature = "media-desktop")]
	#[test]
	fn peer_config_follows_codecs() {
		let codecs = Codecs::builtin();
		let config = peer_config(&codecs, PeerConfig::default());
		assert_eq!(config.video_codecs, [VideoCodec::Vp8]);
		assert!(config.accept_video_codecs.contains(&VideoCodec::Vp8));
		assert!(!config.accept_video_codecs.contains(&VideoCodec::H264), "no OpenH264 loaded");
		assert_eq!(stream_codec(&codecs, &config), Some(Codec::Vp8));
		let h264_only = PeerConfig { video_codecs: vec![VideoCodec::H264], ..config.clone() };
		assert_eq!(stream_codec(&codecs, &h264_only), None);
		// VP9 as the stream codec: VP8 (cheap) is offered after it, nothing
		// else in software.
		let vp9 = PeerConfig { video_codecs: vec![VideoCodec::Vp9], ..config };
		let vp9 = peer_config(&codecs, vp9);
		assert_eq!(vp9.video_codecs, [VideoCodec::Vp9, VideoCodec::Vp8]);
		assert_eq!(stream_codec(&codecs, &vp9), Some(Codec::Vp9));
		// With FFmpeg's software encoders, still only VP8 besides the stream
		// codec.
		let all = Codecs::new();
		let offer = offer_codecs(&all, Codec::Vp8);
		for codec in &offer[1..] {
			let backend = all.encoders().into_iter().find(|(c, _)| c == codec).unwrap().1;
			assert!(all.is_hardware(backend), "{codec} offered in software");
		}
		if all.encoder_codecs().contains(&Codec::H264) && !offer.contains(&Codec::H264) {
			assert_eq!(offer_codecs(&all, Codec::H264)[..2], [Codec::H264, Codec::Vp8]);
		}
		// Whatever this machine has: codecs every TeamSpeak client decodes
		// come before H.264, HEVC last, and the automatic stream codec is
		// one of the first.
		let rank = |c: &Codec| match c {
			_ if decoded_everywhere(*c) => 0,
			Codec::H265 => 2,
			_ => 1,
		};
		let ranks: Vec<_> = offer[1..].iter().map(rank).collect();
		assert!(ranks.is_sorted(), "{offer:?}");
		assert!(preferred_codec(&all, None).is_some_and(decoded_everywhere), "{all:?}");
		assert_eq!(
			preferred_codec(&all, Some(Codec::H264)).is_some(),
			all.encoder_codecs().contains(&Codec::H264)
		);
	}

	/// A sink whose viewers chose `codecs`; keeps every video frame with its
	/// codec.
	#[derive(Default)]
	struct CodecSink {
		codecs: Mutex<Vec<Codec>>,
		frames: Mutex<Vec<(Codec, EncodedFrame)>>,
		keyframe: AtomicBool,
	}

	impl MediaSink for CodecSink {
		fn send(&self, _: EncodedFrame) -> bool {
			true
		}

		fn send_video(&self, frame: EncodedFrame, codec: Codec) -> bool {
			lock(&self.frames).push((codec, frame));
			true
		}

		fn take_keyframe_request(&self) -> bool {
			self.keyframe.swap(false, Ordering::Relaxed)
		}

		fn video_codecs(&self, out: &mut Vec<Codec>) {
			out.extend(lock(&self.codecs).iter());
		}
	}

	/// Viewers that chose another codec get their own encoder per layer,
	/// made when they come and dropped when they go; the stream codec is
	/// skipped while nobody takes it.
	#[cfg(feature = "media-desktop")]
	#[tokio::test(flavor = "multi_thread")]
	async fn an_encoder_per_codec_viewers_chose() {
		let codecs = Codecs::builtin();
		let config = StreamerConfig {
			source: SourceId::Synthetic,
			synthetic_size: (160, 120),
			audio: false,
			..StreamerConfig::default()
		};
		let streamer = Streamer::start(&codecs, config).await.unwrap();
		let sink = Arc::new(CodecSink::default());
		streamer.attach(sink.clone());
		let wait_for = |codec: Codec, count: usize| {
			let sink = sink.clone();
			async move {
				let deadline = Instant::now() + Duration::from_secs(10);
				loop {
					let n = lock(&sink.frames).iter().filter(|(c, _)| *c == codec).count();
					if n >= count {
						return;
					}
					assert!(Instant::now() < deadline, "no {codec} frames");
					tokio::time::sleep(Duration::from_millis(20)).await;
				}
			}
		};
		// Nobody chose yet: the stream codec.
		wait_for(Codec::Vp8, 3).await;
		// A VP9 viewer as well.
		*lock(&sink.codecs) = vec![Codec::Vp8, Codec::Vp9];
		wait_for(Codec::Vp9, 5).await;
		assert_eq!(streamer.stats().layers[0].codecs, [Codec::Vp8, Codec::Vp9]);
		let vp9: Vec<EncodedFrame> = lock(&sink.frames)
			.iter()
			.filter(|(c, _)| *c == Codec::Vp9)
			.map(|(_, f)| f.clone())
			.collect();
		assert!(is_keyframe(Codec::Vp9, &vp9[0].data), "the first VP9 frame is a keyframe");
		let mut decoder = codecs.new_decoder(Codec::Vp9).unwrap();
		for frame in &vp9 {
			let picture = decoder.decode(&frame.data).unwrap().expect("a picture");
			assert_eq!((picture.width, picture.height), (160, 120));
		}
		// Only VP9 viewers left: VP8 stops.
		*lock(&sink.codecs) = vec![Codec::Vp9];
		tokio::time::sleep(Duration::from_millis(300)).await;
		let before = lock(&sink.frames).iter().filter(|(c, _)| *c == Codec::Vp8).count();
		wait_for(Codec::Vp9, vp9.len() + 10).await;
		let after = lock(&sink.frames).iter().filter(|(c, _)| *c == Codec::Vp8).count();
		assert_eq!(before, after, "VP8 is not encoded for nobody");
		assert_eq!(streamer.stats().layers[0].codecs, [Codec::Vp9]);
		assert_eq!(streamer.stats().layers[0].backend, Some(EncoderBackend::Libvpx));
		// Another encoder preference: new encoders, the stream goes on.
		let update = StreamerConfigUpdate {
			encoder: Some(EncoderPreference { hardware: false, ..EncoderPreference::default() }),
			..StreamerConfigUpdate::default()
		};
		streamer.reconfigure(&codecs, update).unwrap();
		let count = lock(&sink.frames).iter().filter(|(c, _)| *c == Codec::Vp9).count();
		wait_for(Codec::Vp9, count + 5).await;
	}

	/// The test pattern through VP8 and back, without a network: pictures
	/// show the moving rectangle.
	#[cfg(feature = "media-desktop")]
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
		// Rates over the second since: the pattern's rate, its bitrate.
		tokio::time::sleep(Duration::from_secs(1)).await;
		let stats = preview.stats();
		assert!((20..=40).contains(&stats.fps), "{stats:?}");
		assert!((100_000..5_000_000).contains(&stats.bitrate), "{stats:?}");
	}

	/// A VP8 decoder for the pipeline tests: fails (with errors, or giving
	/// no picture), or gives a 2x2 picture per frame; conceals losses or not.
	#[derive(Clone, Copy)]
	struct FakeDecoder {
		errors: bool,
		pictures: bool,
		conceals: bool,
	}

	impl voelin_media::VideoDecoder for FakeDecoder {
		fn codec(&self) -> Codec {
			Codec::Vp8
		}

		fn decode(&mut self, _: &[u8]) -> voelin_media::Result<Option<VideoFrame>> {
			if self.errors {
				let message = "made to fail".into();
				return Err(voelin_media::Error::Decoder { codec: Codec::Vp8, message });
			}
			Ok(self.pictures.then(|| VideoFrame::black_i420(2, 2)))
		}

		fn conceals_errors(&self) -> bool {
			self.conceals
		}
	}

	impl voelin_media::codec::hw::DecoderFactory for FakeDecoder {
		fn backend(&self) -> DecoderBackend {
			DecoderBackend::Hardware("fake")
		}

		fn codec(&self) -> Codec {
			Codec::Vp8
		}

		fn api(&self) -> &'static str {
			"test"
		}

		fn is_hardware(&self) -> bool {
			true
		}

		fn create(&self) -> voelin_media::Result<Box<dyn voelin_media::VideoDecoder>> {
			Ok(Box::new(*self))
		}
	}

	/// A decoder that fails, with errors or by giving no picture from
	/// several keyframes, is replaced by the next of its codec's ladder
	/// (libvpx here) without ending the stream: pictures go on.
	#[cfg(feature = "media-desktop")]
	#[tokio::test(flavor = "multi_thread")]
	async fn a_failing_decoder_is_replaced_by_the_next() {
		for errors in [true, false] {
			let fake = FakeDecoder { errors, pictures: false, conceals: false };
			let codecs = Arc::new(Codecs::builtin().with_decoder(Arc::new(fake)));
			assert_eq!(codecs.decoders_for(Codec::Vp8)[0], DecoderBackend::Hardware("fake"));
			assert_eq!(codecs.decoders_for(Codec::Vp8).last(), Some(&DecoderBackend::Libvpx));
			let config = StreamerConfig {
				source: SourceId::Synthetic,
				synthetic_size: (320, 240),
				..StreamerConfig::default()
			};
			let (tx, rx) = std_mpsc::channel();
			let preview = LocalPreview::start(codecs, config, move |picture| {
				let _ = tx.send(picture);
			})
			.await
			.unwrap();
			for _ in 0..10 {
				let picture = rx.recv_timeout(Duration::from_secs(10)).expect("no picture");
				assert_eq!((picture.width, picture.height), (320, 240));
			}
			let stats = preview.stats();
			assert_eq!(stats.decoder, Some(DecoderBackend::Libvpx), "errors {errors}: {stats:?}");
			assert!(stats.keyframe_requests > 0, "{stats:?}");
		}
	}

	/// After a lost frame a decoder that conceals the loss goes on decoding
	/// while a keyframe is asked for, again every half second until one
	/// comes; one that does not skips to the keyframe.
	#[test]
	fn losses_ask_for_keyframes_until_one_comes() {
		for conceals in [true, false] {
			let fake = FakeDecoder { errors: false, pictures: true, conceals };
			let codecs = Arc::new(Codecs::builtin().with_decoder(Arc::new(fake)));
			let (tx, pictures) = std_mpsc::channel();
			let requests = Arc::new(AtomicU32::new(0));
			let pipeline = VideoPipeline::new(
				codecs,
				move |_| {
					let _ = tx.send(());
				},
				{
					let requests = requests.clone();
					move || {
						requests.fetch_add(1, Ordering::Relaxed);
					}
				},
			);
			let frame = |keyframe: bool, contiguous: bool| MediaFrame {
				kind: MediaKind::Video,
				codec: voelin_stream::Codec::Vp8,
				time: MediaTime::new(0, Frequency::NINETY_KHZ),
				network_time: Instant::now(),
				contiguous,
				// VP8's frame tag: bit 0 clear on keyframes.
				data: Arc::from(if keyframe { &[0x10_u8, 0][..] } else { &[0x11, 0][..] }),
			};
			let got =
				|n: usize| (0..n).all(|_| pictures.recv_timeout(Duration::from_secs(2)).is_ok());
			pipeline.push(frame(true, true));
			pipeline.push(frame(false, true));
			assert!(got(2), "conceals {conceals}");
			pipeline.push(frame(false, false));
			pipeline.push(frame(false, true));
			if conceals {
				assert!(got(2), "decoding goes on");
			} else {
				assert!(pictures.recv_timeout(Duration::from_millis(300)).is_err(), "skipped");
			}
			std::thread::sleep(Duration::from_millis(1200));
			let asked = requests.load(Ordering::Relaxed);
			assert!(asked >= 2, "conceals {conceals}: {asked} requests");
			pipeline.push(frame(true, true));
			assert!(got(1), "the keyframe decodes");
			let asked = requests.load(Ordering::Relaxed);
			std::thread::sleep(Duration::from_millis(700));
			assert_eq!(requests.load(Ordering::Relaxed), asked, "no more requests");
			let stats = pipeline.stats();
			assert_eq!(stats.skipped, if conceals { 0 } else { 2 }, "{stats:?}");
			assert_eq!(stats.decoder, Some(DecoderBackend::Hardware("fake")));
		}
	}

	fn layer(id: LayerId, scale: f32, max_fps: Option<u32>, bitrate: u64) -> LayerSpec {
		LayerSpec { id, scale, max_fps, ..LayerSpec::single(bitrate) }
	}

	/// Poll `source` until `done` holds for the frames collected since the
	/// last call (at most 10 s).
	async fn collect(
		source: &mut EncodedSource,
		done: impl Fn(&[EncodedFrame]) -> bool,
	) -> Vec<EncodedFrame> {
		let mut frames = Vec::new();
		let deadline = Instant::now() + Duration::from_secs(10);
		while !done(&frames) {
			assert!(Instant::now() < deadline, "timed out with {} frames", frames.len());
			tokio::time::sleep(Duration::from_millis(20)).await;
			source.poll_frames(Instant::now(), &mut frames);
		}
		frames
	}

	/// Size of the last picture of `layer`, and whether the first frame that
	/// decoded was a keyframe. Frames before it (another codec) are skipped.
	fn decoded_size(codec: Codec, frames: &[EncodedFrame], layer: LayerId) -> ((u32, u32), bool) {
		let mut decoder = Codecs::new().new_decoder(codec).unwrap();
		let (mut size, mut first_keyframe) = ((0, 0), None);
		for f in frames.iter().filter(|f| f.layer == layer) {
			match decoder.decode(&f.data) {
				Ok(Some(p)) => {
					size = (p.width, p.height);
					first_keyframe.get_or_insert(f.keyframe);
				}
				Ok(None) => {}
				Err(e) => assert!(first_keyframe.is_none(), "layer {layer}: {e}"),
			}
		}
		(size, first_keyframe == Some(true))
	}

	/// The test pattern as DMA-BUFs, as the portal hands over the screen:
	/// with VA-API encoders on every layer, each frame is converted and
	/// scaled on the GPU and none on the CPU; after a switch to libvpx,
	/// which needs frames in memory, the CPU converts them again and the
	/// stream goes on. Skipped without `h264_vaapi` or `/dev/udmabuf`.
	#[cfg(target_os = "linux")]
	#[tokio::test(flavor = "multi_thread")]
	async fn dmabufs_take_the_gpu_path_while_every_encoder_does() {
		let vaapi = voelin_media::ffmpeg::probe()
			.iter()
			.any(|s| s.spec.name == "h264_vaapi" && s.available.is_ok());
		if !vaapi || !std::path::Path::new("/dev/udmabuf").exists() {
			eprintln!("no h264_vaapi or /dev/udmabuf, skipped");
			return;
		}
		let codecs = Codecs::new();
		let vaapi = EncoderPreference { hardware: true, backend: "h264_vaapi".parse().unwrap() };
		let config = StreamerConfig {
			source: SourceId::Synthetic,
			synthetic_size: (640, 360),
			synthetic_dmabuf: true,
			fps: 30,
			audio: false,
			codec: Codec::H264,
			encoder: vaapi,
			layers: vec![layer(0, 1.0, None, 1_000_000), layer(5, 0.5, Some(15), 300_000)],
			..StreamerConfig::default()
		};
		let streamer = Streamer::start(&codecs, config).await.unwrap();
		let mut source = EncodedSource::new(streamer);
		let count = |frames: &[EncodedFrame], id| frames.iter().filter(|f| f.layer == id).count();
		let frames = collect(&mut source, |f| count(f, 0) >= 20 && count(f, 5) >= 5).await;
		for id in [0, 5] {
			assert!(frames.iter().find(|f| f.layer == id).unwrap().keyframe, "layer {id}");
		}
		let stats = source.streamer().stats();
		assert_eq!(stats.gpu_error, None);
		assert!(stats.gpu_frames >= 20, "{} frames on the GPU", stats.gpu_frames);
		let on_cpu = stats.captured_frames - stats.gpu_frames;
		assert!(on_cpu <= 1, "{on_cpu} of {} frames on the CPU", stats.captured_frames);
		let sizes: Vec<_> = stats.layers.iter().map(|l| (l.width, l.height)).collect();
		assert_eq!(sizes, [(640, 360), (320, 180)]);

		let libvpx = EncoderPreference { hardware: false, backend: "libvpx".parse().unwrap() };
		let update = StreamerConfigUpdate {
			codec: Some(Codec::Vp8),
			encoder: Some(libvpx),
			..StreamerConfigUpdate::default()
		};
		source.streamer().reconfigure(&codecs, update).unwrap();
		let gpu = source.streamer().stats().gpu_frames;
		let frames = collect(&mut source, |f| count(f, 0) >= 40).await;
		assert_eq!(decoded_size(Codec::Vp8, &frames, 0), ((640, 360), true));
		let stats = source.streamer().stats();
		assert!(
			stats.gpu_frames <= gpu + 2,
			"{} frames on the GPU after the switch",
			stats.gpu_frames - gpu
		);
		assert_eq!(stats.gpu_error, None, "a planned switch, not a failure");
	}

	/// Two layers at their own sizes and frame rates, keyframes per layer,
	/// then a new codec and another layer set while streaming.
	#[cfg(feature = "media-desktop")]
	#[tokio::test(flavor = "multi_thread")]
	async fn simulcast_layers_and_reconfigure() {
		let codecs = Codecs::new();
		let config = StreamerConfig {
			source: SourceId::Synthetic,
			synthetic_size: (320, 240),
			fps: 30,
			audio: false,
			layers: vec![layer(0, 1.0, None, 800_000), layer(5, 0.5, Some(10), 300_000)],
			..StreamerConfig::default()
		};
		let streamer = Streamer::start(&codecs, config).await.unwrap();
		assert_eq!(streamer.layer_ids(), [0, 5]);
		let mut source = EncodedSource::new(streamer);
		let count = |frames: &[EncodedFrame], id| frames.iter().filter(|f| f.layer == id).count();
		let started = Instant::now();
		let frames = collect(&mut source, |f| count(f, 0) >= 30).await;
		let seconds = started.elapsed().as_secs_f64();
		// Both start with a keyframe (the source asks for one on every layer).
		for id in [0, 5] {
			assert!(frames.iter().find(|f| f.layer == id).unwrap().keyframe, "layer {id}");
		}
		assert_eq!(decoded_size(Codec::Vp8, &frames, 0), ((320, 240), true));
		assert_eq!(decoded_size(Codec::Vp8, &frames, 5), ((160, 120), true));
		// Layer 5 is capped at 10 fps (layer 0 may fall behind its 30 under
		// load, dropping stale frames).
		let small = count(&frames, 5) as f64;
		assert!(small >= 3.0 && small <= seconds * 10.0 * 1.2 + 2.0, "{small} in {seconds:.2} s");

		// A keyframe on layer 5 only.
		source.request_layer_keyframe(5);
		let frames = collect(&mut source, |f| f.iter().any(|f| f.layer == 5 && f.keyframe)).await;
		assert!(!frames.iter().any(|f| f.layer == 0 && f.keyframe), "layer 0 got a keyframe");

		// A viewer's estimate lowers layer 5's encoder bitrate only.
		source.set_layer_bitrate(5, 150_000);
		let target = |id| {
			let stats = source.streamer().stats();
			stats.layers.iter().find(|l| l.id == id).map(|l| l.bitrate)
		};
		let deadline = Instant::now() + Duration::from_secs(5);
		while target(5) != Some(150_000) {
			assert!(Instant::now() < deadline, "layer 5 at {:?}", target(5));
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
		assert_eq!(target(0), Some(800_000));

		// VP9, layer 5 gone, a new layer 7 at a fixed size.
		let fixed = LayerSpec { size: Some((96, 64)), ..layer(7, 1.0, None, 200_000) };
		let update = StreamerConfigUpdate {
			codec: Some(Codec::Vp9),
			layers: Some(vec![layer(0, 1.0, None, 600_000), fixed]),
			fps: Some(20),
			..StreamerConfigUpdate::default()
		};
		source.streamer().reconfigure(&codecs, update).unwrap();
		assert_eq!(source.streamer().layer_ids(), [0, 7]);
		// VP8 frames encoded before the switch may still come first; the
		// first VP9 picture of each layer is a keyframe.
		let frames = collect(&mut source, |f| count(f, 7) >= 10 && count(f, 0) >= 10).await;
		assert_eq!(decoded_size(Codec::Vp9, &frames, 0), ((320, 240), true));
		assert_eq!(decoded_size(Codec::Vp9, &frames, 7), ((96, 64), true));
		let stats = source.streamer().stats();
		assert_eq!(stats.codec, Some(Codec::Vp9));
		assert_eq!(stats.layers.iter().map(|l| l.id).collect::<Vec<_>>(), [0, 7]);
		assert!(stats.error.is_none(), "{stats:?}");
		assert!(stats.captured_frames > 0 && stats.convert_threads >= 1, "{stats:?}");

		// Duplicate ids are refused and change nothing.
		let bad = StreamerConfigUpdate {
			layers: Some(vec![layer(1, 1.0, None, 1), layer(1, 0.5, None, 1)]),
			..StreamerConfigUpdate::default()
		};
		assert!(source.streamer().reconfigure(&codecs, bad).is_err());
		assert_eq!(source.streamer().layer_ids(), [0, 7]);
	}

	#[test]
	fn audio_sources_from_settings() {
		use crate::settings::{AudioSourceKindSetting as K, AudioSourceSetting as S};
		let settings = [
			S::new(K::Desktop),
			S { gain: 0.5, ..S::new(K::App { name: Some("firefox".into()), pid: None }) },
			S { muted: true, ..S::new(K::App { name: None, pid: Some(42) }) },
			S::new(K::Window),
			S::new(K::Microphone),
			S::new(K::Synthetic { frequency: 1000 }),
		];
		let specs = audio_source_specs(&settings);
		let kinds: Vec<AudioSourceKind> = specs.iter().map(|s| s.kind.clone()).collect();
		assert_eq!(
			kinds,
			[
				AudioSourceKind::DesktopWithoutSelf,
				AudioSourceKind::App(AppMatch::Name("firefox".into())),
				AudioSourceKind::App(AppMatch::Pid(42)),
				AudioSourceKind::WindowAudio,
				AudioSourceKind::Microphone,
				AudioSourceKind::Synthetic { hz: 1000 },
			]
		);
		assert_eq!((specs[1].gain, specs[2].muted), (0.5, true));
		let back: Vec<S> = specs.iter().map(S::from).collect();
		assert_eq!(back, settings);
		assert_eq!(
			default_audio_sources(&SourceId::Monitor(0)),
			[AudioSourceSpec::new(AudioSourceKind::DesktopWithoutSelf)]
		);
	}

	/// Audio sources mixed, then changed while streaming: gain, mute, one
	/// removed, the microphone added; levels in the stats follow.
	#[cfg(feature = "media-desktop")]
	#[tokio::test(flavor = "multi_thread")]
	async fn audio_sources_change_live() {
		let codecs = Codecs::new();
		let tone = |hz| AudioSourceSpec::new(AudioSourceKind::Synthetic { hz });
		let config = StreamerConfig {
			source: SourceId::Synthetic,
			synthetic_size: (64, 48),
			audio_sources: vec![tone(440), AudioSourceSpec { gain: 0.5, ..tone(1000) }],
			..StreamerConfig::default()
		};
		let streamer = Streamer::start(&codecs, config).await.unwrap();
		let wait = |what: &str, done: &dyn Fn(&StreamerStats) -> bool| {
			let deadline = Instant::now() + Duration::from_secs(5);
			loop {
				let stats = streamer.stats();
				if done(&stats) {
					return stats;
				}
				assert!(Instant::now() < deadline, "{what}: {:#?}", stats.audio_sources);
				std::thread::sleep(Duration::from_millis(20));
			}
		};
		// The SineSource tone is 0.05; the second at half gain.
		let near = |level: f32, want: f32| (level - want).abs() < want * 0.1;
		let stats = wait("both tones", &|s| {
			s.audio_sources.len() == 2
				&& near(s.audio_sources[0].level.peak, 0.05)
				&& near(s.audio_sources[1].level.peak, 0.025)
		});
		assert!(stats.audio_sources.iter().all(|s| s.state == SourceState::Playing));
		assert!(stats.audio_level.peak > 0.05 && stats.audio_limiter == 1.0, "{stats:?}");
		let first = stats.audio_sources[0].id;

		// Mute the first (kept: same id), drop the second, add the
		// microphone and a window source (the pattern is no window).
		let update = StreamerConfigUpdate {
			audio_sources: Some(vec![
				AudioSourceSpec { muted: true, ..tone(440) },
				AudioSourceSpec::new(AudioSourceKind::Microphone),
				AudioSourceSpec::new(AudioSourceKind::WindowAudio),
			]),
			..StreamerConfigUpdate::default()
		};
		streamer.reconfigure(&codecs, update).unwrap();
		assert_eq!(streamer.audio_sources().len(), 3);
		let publisher = tap::publisher_id();
		let speak = std::thread::spawn(move || {
			// 20 ms of a 0.2 tone every 20 ms, for a second.
			let chunk: Vec<f32> = (0..960).map(|i| 0.2 * (i as f32 * 0.1).sin()).collect();
			for _ in 0..50 {
				tap::microphone().publish(publisher, &chunk);
				std::thread::sleep(Duration::from_millis(20));
			}
		});
		let stats = wait("microphone in, tone muted", &|s| {
			s.audio_sources.len() == 3
				&& s.audio_sources[1].level.peak > 0.15
				&& s.audio_level.peak > 0.15
		});
		speak.join().unwrap();
		assert_eq!(stats.audio_sources[0].id, first, "the kept tone keeps its source");
		assert!(stats.audio_sources[0].spec.muted);
		// Muted sources are still metered.
		assert!(near(stats.audio_sources[0].level.peak, 0.05), "{stats:?}");
		let window = &stats.audio_sources[2];
		assert!(window.error.as_deref().is_some_and(|e| e.contains("no window")), "{window:?}");
		assert!(streamer.audio_error().is_some_and(|e| e.contains("shared window")));
		assert!(streamer.audio_mixer().is_some());

		// All sources gone: silence, the track stays.
		let update = StreamerConfigUpdate { audio_sources: Some(Vec::new()), ..Default::default() };
		streamer.reconfigure(&codecs, update).unwrap();
		wait("silence", &|s| s.audio_sources.is_empty() && s.audio_level.peak < 0.01);
		assert!(streamer.has_audio());
	}

	/// Audio of the test pattern: 20 ms Opus frames on a 48 kHz clock.
	#[cfg(feature = "media-desktop")]
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
