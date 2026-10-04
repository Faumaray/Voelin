//! Video codecs: common types, the encoder/decoder traits, codec preference
//! and backend selection.
//!
//! Backends:
//! - [`vpx`] (feature `vpx`): VP8 and VP9 encoding and decoding with the
//!   system libvpx
//! - [`h264`] (feature `openh264`): H.264 with Cisco's prebuilt OpenH264,
//!   loaded at runtime
//! - [`av1`] (feature `av1`): AV1 decoding with the system libdav1d
//! - [`hw`]: encoder and decoder factories found at runtime: FFmpeg's
//!   hardware and software encoders and decoders ([`crate::ffmpeg`], feature
//!   `ffmpeg`) and, on Android, MediaCodec encoders
//! - `mediacodec` (Android): the device's MediaCodec encoders and decoders
//!   (hardware, or Google's software VP8 / VP9 / H.264)
//!
//! A viewer decodes each codec with the best decoder that works here and
//! falls back to the next one ([`Codecs::decoders_for`]): hardware (FFmpeg's
//! hwaccels, MediaCodec), then FFmpeg's software decoders, then the built-in
//! libvpx, OpenH264 and dav1d.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use crate::frame::VideoFrame;
use crate::{Error, Result};

#[cfg(feature = "av1")]
pub mod av1;
#[cfg(feature = "openh264")]
pub mod h264;
pub mod hw;
pub mod image_layout;
#[cfg(target_os = "android")]
pub mod mediacodec;
#[cfg(feature = "vpx")]
pub mod vpx;

/// Video codecs of streams: the four TeamSpeak 6 uses, and HEVC for peers
/// that negotiate it (offered last, hardware encoders only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
	Vp8,
	Vp9,
	H264,
	Av1,
	H265,
}

impl Codec {
	pub const ALL: [Codec; 5] = [Codec::Vp8, Codec::Vp9, Codec::H264, Codec::Av1, Codec::H265];

	/// Name as in SDP (`a=rtpmap:<pt> <name>/90000`).
	pub fn name(self) -> &'static str {
		match self {
			Codec::Vp8 => "VP8",
			Codec::Vp9 => "VP9",
			Codec::H264 => "H264",
			Codec::Av1 => "AV1",
			Codec::H265 => "H265",
		}
	}
}

impl fmt::Display for Codec {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.name())
	}
}

impl FromStr for Codec {
	type Err = Error;

	/// Accepts SDP names and MIME types (`vp8`, `H264`, `video/AV1`, `h.264`).
	fn from_str(s: &str) -> Result<Self> {
		let name = s.trim();
		let name = name.strip_prefix("video/").unwrap_or(name);
		match name.to_ascii_lowercase().as_str() {
			"vp8" => Ok(Codec::Vp8),
			"vp9" => Ok(Codec::Vp9),
			"h264" | "h.264" | "avc" => Ok(Codec::H264),
			"av1" | "av1x" => Ok(Codec::Av1),
			"h265" | "h.265" | "hevc" => Ok(Codec::H265),
			_ => Err(Error::Convert(format!("unknown codec {s:?}"))),
		}
	}
}

#[cfg(feature = "str0m")]
impl From<Codec> for str0m::format::Codec {
	fn from(c: Codec) -> Self {
		match c {
			Codec::Vp8 => str0m::format::Codec::Vp8,
			Codec::Vp9 => str0m::format::Codec::Vp9,
			Codec::H264 => str0m::format::Codec::H264,
			Codec::Av1 => str0m::format::Codec::Av1,
			Codec::H265 => str0m::format::Codec::H265,
		}
	}
}

#[cfg(feature = "str0m")]
impl TryFrom<str0m::format::Codec> for Codec {
	type Error = Error;

	fn try_from(c: str0m::format::Codec) -> Result<Self> {
		match c {
			str0m::format::Codec::Vp8 => Ok(Codec::Vp8),
			str0m::format::Codec::Vp9 => Ok(Codec::Vp9),
			str0m::format::Codec::H264 => Ok(Codec::H264),
			str0m::format::Codec::Av1 => Ok(Codec::Av1),
			str0m::format::Codec::H265 => Ok(Codec::H265),
			other => Err(Error::Convert(format!("not a video codec: {other:?}"))),
		}
	}
}

/// Order in which a viewer accepts codecs, best first: AV1 > HEVC > VP9 >
/// H.264 > VP8 (the best compression first, the codec every client has
/// last). Answers follow the offer's order, so this ranks what a viewer
/// asks for when it chooses, and what [`Codecs::decoders`] lists.
pub const VIEWER_PREFERENCE: [Codec; 5] =
	[Codec::Av1, Codec::H265, Codec::Vp9, Codec::H264, Codec::Vp8];

/// What the encoder should optimise for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContentHint {
	/// Screen content: sharp text, mostly static.
	#[default]
	Screen,
	/// Camera-like content with motion.
	Motion,
}

/// H.264 profile of the encoder output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum H264Profile {
	/// Constrained High (what TeamSpeak clients decode): High without
	/// B-frames.
	#[default]
	ConstrainedHigh,
	ConstrainedBaseline,
}

/// Encoder settings. The resolution follows the frames: the first frame and
/// every size change start a new keyframe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderConfig {
	/// Expected frame rate (rate control; timestamps come from the frames).
	pub fps: u32,
	/// Target bitrate in bits per second (no limit; TeamSpeak 6 servers
	/// enforce none, see `voelin_stream::proto::MAX_ANNOUNCED_BITRATE`).
	pub bitrate_bps: u32,
	/// Maximum frames between keyframes; `None` sends keyframes only at the
	/// start and on request (PLI), like WebRTC.
	pub keyframe_interval: Option<u32>,
	pub content: ContentHint,
	/// Most encoder threads (still no more than the frame size can use); 0:
	/// all CPUs but one ([`encoder_cpus`]).
	pub threads: u32,
	/// A fixed speed / quality trade-off in the backend's own terms (libvpx
	/// `cpu-used`); `None` lets the encoder adapt it to how long frames take
	/// to encode compared to the frame interval. FFmpeg backends: the x264
	/// preset index (0 ultrafast .. 9 placebo), the NVENC preset `p1`..`p7`,
	/// the SVT-AV1 preset, rav1e's speed, libaom's `cpu-used`.
	pub speed: Option<i32>,
	/// H.264 only.
	pub h264_profile: H264Profile,
}

impl Default for EncoderConfig {
	fn default() -> Self {
		Self {
			fps: 30,
			bitrate_bps: 2_500_000,
			keyframe_interval: None,
			content: ContentHint::Screen,
			threads: 0,
			speed: None,
			h264_profile: H264Profile::default(),
		}
	}
}

/// CPUs encoders use by default: all but one (at least one). libvpx's
/// threads wait on each other by spinning, so an encode that wants every
/// core slows down many times over as soon as anything else runs (the
/// capture and conversion of the next frame, another layer, the desktop);
/// measured on a 4-core machine: 130-290 ms per 1080p frame with 4 threads
/// under load, against 9-17 ms with 3.
pub fn encoder_cpus() -> u32 {
	let cpus = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
	cpus.saturating_sub(1).max(1)
}

impl EncoderConfig {
	/// Threads for a `width` x `height` encode: [`threads`](Self::threads)
	/// (or [`encoder_cpus`]), but no more than one per 320x240 pixels: more
	/// would idle on small frames and cost quality (VP8 token partitions).
	pub fn threads_for(&self, width: u32, height: u32) -> u32 {
		let cpus = match self.threads {
			0 => encoder_cpus(),
			n => n,
		};
		let useful = (u64::from(width) * u64::from(height) / (320 * 240)).clamp(1, 64) as u32;
		cpus.min(useful)
	}
}

/// An encoded frame borrowed from the encoder (see
/// [`VideoEncoder::encode_with`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodedChunk<'a> {
	pub data: &'a [u8],
	pub keyframe: bool,
	/// Presentation time on the 90 kHz RTP clock.
	pub pts_90khz: u64,
}

/// One encoded frame, ready for `voelin_stream::Peer::write`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFrame {
	pub data: Vec<u8>,
	pub keyframe: bool,
	/// Presentation time on the 90 kHz RTP clock.
	pub pts_90khz: u64,
}

/// A video encoder. Encoders are created per stream and used from one thread
/// at a time.
pub trait VideoEncoder: Send {
	fn codec(&self) -> Codec;

	/// Which implementation encodes (for logs and settings).
	fn backend(&self) -> EncoderBackend;

	/// Encode one frame of any pixel format. Returns no frame when the
	/// encoder skipped it (rate control), usually one.
	fn encode(&mut self, frame: &VideoFrame, force_keyframe: bool) -> Result<Vec<EncodedFrame>>;

	/// Like [`encode`](Self::encode), but hands each encoded frame to `out`
	/// borrowed from the encoder's own buffer, so nothing is allocated or
	/// copied here (libvpx does this; the default goes through `encode`).
	fn encode_with(
		&mut self,
		frame: &VideoFrame,
		force_keyframe: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		for f in self.encode(frame, force_keyframe)? {
			out(EncodedChunk { data: &f.data, keyframe: f.keyframe, pts_90khz: f.pts_90khz });
		}
		Ok(())
	}

	/// Change the target bitrate (bits per second).
	fn set_bitrate(&mut self, bps: u32) -> Result<()>;

	/// Change the expected frame rate (rate control and speed decisions);
	/// ignored by encoders that follow the frame timestamps anyway.
	fn set_fps(&mut self, fps: u32) -> Result<()> {
		let _ = fps;
		Ok(())
	}

	/// The current speed setting, for statistics (libvpx `cpu-used`).
	fn speed(&self) -> Option<i32> {
		None
	}

	/// Whether this encoder takes [`GpuFrame`](crate::GpuFrame)s
	/// ([`encode_gpu`](Self::encode_gpu)), and then the multiple their
	/// width and height must be of (a size it encodes exactly; GPU frames
	/// are made at that size instead of being cropped here). `None` for
	/// encoders that need the pixels in memory: all but VA-API.
	fn gpu_alignment(&self) -> Option<(u32, u32)> {
		None
	}

	/// Encode a frame that is in GPU memory, like
	/// [`encode_with`](Self::encode_with). Only for encoders with a
	/// [`gpu_alignment`](Self::gpu_alignment), and frames of a size that is a
	/// multiple of it; the rest fail with `Error::CodecUnavailable`.
	fn encode_gpu(
		&mut self,
		frame: &crate::GpuFrame,
		force_keyframe: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		let _ = (frame, force_keyframe, out);
		Err(Error::CodecUnavailable {
			codec: self.codec(),
			reason: format!("{} takes no frames in GPU memory", self.backend()),
		})
	}
}

/// A video decoder.
pub trait VideoDecoder: Send {
	fn codec(&self) -> Codec;

	/// Decode one complete frame (a depacketized RTP frame). Returns the
	/// picture if one is ready; its timestamp is zero.
	fn decode(&mut self, data: &[u8]) -> Result<Option<VideoFrame>>;

	/// Like [`decode`](Self::decode), into `out`: `true` if a picture is
	/// there, else `out` is unchanged. Decoders that can reuse `out`'s
	/// buffers do (FFmpeg's), so a caller that keeps passing the same frame
	/// gets its pictures without allocations.
	fn decode_into(&mut self, data: &[u8], out: &mut VideoFrame) -> Result<bool> {
		let picture = self.decode(data)?;
		Ok(picture.map(|p| *out = p).is_some())
	}

	/// Whether frames after a lost one still decode to a picture (what is
	/// missing concealed until the next keyframe), so that a viewer keeps
	/// decoding instead of waiting for a keyframe: FFmpeg's H.264 and HEVC
	/// decoders. The others fail, or show garbage, on a missing reference.
	fn conceals_errors(&self) -> bool {
		false
	}
}

/// Implementations behind [`VideoEncoder`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EncoderBackend {
	/// An OS / GPU encoder of its own API (Android's `mediacodec`, see
	/// [`hw`]).
	Hardware(&'static str),
	/// An FFmpeg encoder by its FFmpeg name (`h264_vaapi`, `libx264`, see
	/// [`crate::ffmpeg`]), hardware or software.
	Ffmpeg(&'static str),
	Libvpx,
	OpenH264,
}

impl EncoderBackend {
	/// The name in settings (`stream.encoder_backend`) and logs.
	pub fn name(self) -> &'static str {
		match self {
			EncoderBackend::Hardware(name) | EncoderBackend::Ffmpeg(name) => name,
			EncoderBackend::Libvpx => "libvpx",
			EncoderBackend::OpenH264 => "openh264",
		}
	}
}

impl fmt::Display for EncoderBackend {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.name())
	}
}

/// Implementations behind [`VideoDecoder`], as [`Codecs::decoders_for`]
/// orders them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DecoderBackend {
	/// An OS / GPU decoder of its own API (Android's `mediacodec`), or one
	/// an application adds ([`Codecs::with_decoder`]).
	Hardware(&'static str),
	/// An FFmpeg decoder by its name in `ffmpeg::DECODERS`: in hardware
	/// (`h264_vaapi`, FFmpeg's names of its hwaccels) or in software
	/// (`h264`, `libdav1d`).
	Ffmpeg(&'static str),
	Libvpx,
	OpenH264,
	Dav1d,
}

impl DecoderBackend {
	/// The name in settings (`stream.decoder_backend`) and logs.
	pub fn name(self) -> &'static str {
		match self {
			DecoderBackend::Hardware(name) | DecoderBackend::Ffmpeg(name) => name,
			DecoderBackend::Libvpx => "libvpx",
			DecoderBackend::OpenH264 => "openh264",
			DecoderBackend::Dav1d => "dav1d",
		}
	}
}

impl fmt::Display for DecoderBackend {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.name())
	}
}

/// Which decoders to use (settings `stream.hardware_decoding` and
/// `stream.decoder_backend`): the choices of the encoders, `hardware` for
/// GPU decoders and `backend` naming a [`DecoderBackend::name`].
pub type DecoderPreference = EncoderPreference;

/// Which encoders to use (settings `stream.hardware_acceleration` and
/// `stream.encoder_backend`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderPreference {
	/// Use hardware encoders when [`backend`](Self::backend) is `Auto`.
	pub hardware: bool,
	pub backend: BackendChoice,
}

impl Default for EncoderPreference {
	fn default() -> Self {
		Self { hardware: true, backend: BackendChoice::Auto }
	}
}

/// `stream.encoder_backend`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BackendChoice {
	/// Hardware first (if enabled), then software.
	#[default]
	Auto,
	/// Software encoders only.
	Software,
	/// This backend ([`EncoderBackend::name`]) for the codecs it encodes;
	/// the automatic order for the others and as fallback.
	Named(String),
}

impl FromStr for BackendChoice {
	type Err = std::convert::Infallible;

	fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
		Ok(match s.trim() {
			"" | "auto" => BackendChoice::Auto,
			"software" => BackendChoice::Software,
			name => BackendChoice::Named(name.to_owned()),
		})
	}
}

impl fmt::Display for BackendChoice {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			BackendChoice::Auto => f.write_str("auto"),
			BackendChoice::Software => f.write_str("software"),
			BackendChoice::Named(name) => f.write_str(name),
		}
	}
}

/// One encoder backend in [`Codecs::report`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderInfo {
	/// [`EncoderBackend::name`].
	pub name: String,
	/// The API or library (`VA-API`, `NVENC`, `x264`, `libvpx`, ...).
	pub api: String,
	pub codec: Codec,
	pub hardware: bool,
	/// Usable (passed its self-test), or why not.
	pub status: std::result::Result<(), String>,
	/// Position among the encoders of its codec under the current
	/// preference (0: used first); `None` if not used.
	pub rank: Option<usize>,
}

/// One decoder backend in [`Codecs::report`]: as [`EncoderInfo`], its
/// `rank` the position in its codec's ladder ([`Codecs::decoders_for`]).
pub type DecoderInfo = EncoderInfo;

/// Every encoder and decoder backend this build knows, what works here and
/// why the rest does not (for the UI and `voelinctl stream encoders`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderReport {
	/// The FFmpeg libraries in use (release, version, path), or why none.
	pub ffmpeg: std::result::Result<String, String>,
	/// Whether captured DMA-BUFs can go to a VA-API encoder without a copy,
	/// or why not.
	pub zero_copy: std::result::Result<(), String>,
	pub encoders: Vec<EncoderInfo>,
	pub decoders: Vec<DecoderInfo>,
}

fn unavailable(codec: Codec, reason: impl Into<String>) -> Error {
	Error::CodecUnavailable { codec, reason: reason.into() }
}

/// Order of codecs among hardware encoders: the ones every TeamSpeak
/// client decodes first (AV1, VP9, VP8: the official client's WebRTC stack
/// has their decoders built in), then H.264 (the official client decodes it
/// only with an OpenH264 library it downloads at start-up and may not get),
/// and HEVC last (only for peers that take nothing else).
const HARDWARE_ORDER: [Codec; 5] = [Codec::Av1, Codec::Vp9, Codec::Vp8, Codec::H264, Codec::H265];

/// An encoder backend in the base order (before the preference applies).
struct Candidate {
	codec: Codec,
	backend: EncoderBackend,
	hardware: bool,
	/// See [`hw::EncoderFactory::is_automatic`].
	automatic: bool,
}

impl Candidate {
	fn builtin(codec: Codec, backend: EncoderBackend) -> Self {
		Self { codec, backend, hardware: false, automatic: true }
	}
}

/// The codecs this build and machine can use, and factories for them.
///
/// Viewer order ([`Codecs::decoders`]): AV1 > HEVC > VP9 > H.264 > VP8, each
/// codec with its ladder of decoders ([`Codecs::decoders_for`]) under the
/// [`DecoderPreference`]. Streamer
/// order ([`Codecs::encoders`]) under the default [`EncoderPreference`]:
/// hardware (H.264, AV1, VP9, VP8, HEVC; FFmpeg's or MediaCodec) > VP8
/// (libvpx) > H.264 (x264 and OpenH264 through FFmpeg, then Cisco's
/// OpenH264) > VP9 (libvpx, costly in software) > AV1 (SVT-AV1, rav1e,
/// libaom through FFmpeg). Cheap to clone: the factories are shared.
#[derive(Clone)]
pub struct Codecs {
	factories: Vec<Arc<dyn hw::EncoderFactory>>,
	decoder_factories: Vec<Arc<dyn hw::DecoderFactory>>,
	#[cfg(feature = "openh264")]
	openh264: Option<h264::OpenH264>,
	preference: EncoderPreference,
	decoder_preference: DecoderPreference,
}

impl Default for Codecs {
	fn default() -> Self {
		Self::new()
	}
}

impl fmt::Debug for Codecs {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Codecs")
			.field("decoders", &self.decoders())
			.field("encoders", &self.encoders())
			.field("preference", &self.preference)
			.field("decoder_preference", &self.decoder_preference)
			.finish()
	}
}

impl Codecs {
	/// Software codecs compiled in, plus the encoders [`hw::probe`] finds
	/// (FFmpeg's that passed their self-test, MediaCodec) and the decoders
	/// [`hw::probe_decoders`] finds (FFmpeg's), both probed at once. H.264
	/// through Cisco's library needs [`Codecs::with_openh264`].
	pub fn new() -> Self {
		let (factories, decoder_factories) = std::thread::scope(|scope| {
			let decoders = scope.spawn(hw::probe_decoders);
			(hw::probe(), decoders.join().unwrap_or_default())
		});
		Self {
			factories: factories.into_iter().map(Arc::from).collect(),
			decoder_factories: decoder_factories.into_iter().map(Arc::from).collect(),
			#[cfg(feature = "openh264")]
			openh264: None,
			preference: EncoderPreference::default(),
			decoder_preference: DecoderPreference::default(),
		}
	}

	/// Only the codecs compiled into this crate (libvpx, OpenH264 once
	/// loaded, dav1d): no FFmpeg or MediaCodec encoders, no FFmpeg decoders,
	/// nothing probed.
	pub fn builtin() -> Self {
		Self {
			factories: Vec::new(),
			decoder_factories: Vec::new(),
			#[cfg(feature = "openh264")]
			openh264: None,
			preference: EncoderPreference::default(),
			decoder_preference: DecoderPreference::default(),
		}
	}

	/// Decode `factory`'s codec with it first (a decoder of the embedding
	/// application, or a test's).
	pub fn with_decoder(mut self, factory: Arc<dyn hw::DecoderFactory>) -> Self {
		self.decoder_factories.insert(0, factory);
		self
	}

	/// Use `preference` for [`decoders_for`](Self::decoders_for) and
	/// [`new_decoder`](Self::new_decoder).
	pub fn with_decoder_preference(mut self, preference: DecoderPreference) -> Self {
		self.decoder_preference = preference;
		self
	}

	pub fn set_decoder_preference(&mut self, preference: DecoderPreference) {
		self.decoder_preference = preference;
	}

	pub fn decoder_preference(&self) -> &DecoderPreference {
		&self.decoder_preference
	}

	/// Enable H.264 through a loaded OpenH264 library.
	#[cfg(feature = "openh264")]
	pub fn with_openh264(mut self, library: h264::OpenH264) -> Self {
		self.openh264 = Some(library);
		self
	}

	#[cfg(feature = "openh264")]
	pub fn openh264(&self) -> Option<&h264::OpenH264> {
		self.openh264.as_ref()
	}

	/// Use `preference` for [`encoders`](Self::encoders) and
	/// [`new_encoder`](Self::new_encoder).
	pub fn with_preference(mut self, preference: EncoderPreference) -> Self {
		self.preference = preference;
		self
	}

	pub fn set_preference(&mut self, preference: EncoderPreference) {
		self.preference = preference;
	}

	pub fn preference(&self) -> &EncoderPreference {
		&self.preference
	}

	/// Whether `backend` is a hardware encoder here.
	pub fn is_hardware(&self, backend: EncoderBackend) -> bool {
		self.factories.iter().any(|f| f.backend() == backend && f.is_hardware())
	}

	fn has_openh264(&self) -> bool {
		#[cfg(feature = "openh264")]
		return self.openh264.is_some();
		#[cfg(not(feature = "openh264"))]
		return false;
	}

	/// Whether `codec` can be decoded, or why not.
	pub fn check_decoder(&self, codec: Codec) -> Result<()> {
		if !self.decoders_for(codec).is_empty() {
			return Ok(());
		}
		if !self.decoder_candidates(codec).is_empty() {
			let choice = &self.decoder_preference;
			return Err(unavailable(codec, format!("its decoders are not used ({choice:?})")));
		}
		let builtin = match codec {
			Codec::Vp8 | Codec::Vp9 if cfg!(feature = "vpx") => "libvpx has no decoder for it",
			Codec::Vp8 | Codec::Vp9 => "built without the `vpx` feature",
			Codec::H264 => "the OpenH264 library is not loaded",
			Codec::Av1 => "built without the `av1` feature (dav1d)",
			Codec::H265 => "nothing built in decodes it",
		};
		Err(unavailable(codec, format!("no FFmpeg decoder works here, and {builtin}")))
	}

	/// Decodable codecs in the viewer's order of preference
	/// ([`VIEWER_PREFERENCE`]): those with at least one decoder.
	pub fn decoders(&self) -> Vec<Codec> {
		VIEWER_PREFERENCE.into_iter().filter(|&c| !self.decoders_for(c).is_empty()).collect()
	}

	/// Every decoder of `codec` here and whether it is a GPU one: the
	/// platform's (MediaCodec), the factories (an application's, then
	/// FFmpeg's hardware and software decoders), then the built-in ones.
	fn decoder_candidates(&self, codec: Codec) -> Vec<(DecoderBackend, bool)> {
		let mut list = Vec::new();
		// The device's codecs (hardware, or Google's software ones): Android
		// has no others, so hardware decoding off does not leave them out.
		#[cfg(target_os = "android")]
		if mediacodec::check_decoder(codec).is_ok() {
			list.push((DecoderBackend::Hardware("mediacodec"), false));
		}
		for f in self.decoder_factories.iter().filter(|f| f.codec() == codec) {
			list.push((f.backend(), f.is_hardware()));
		}
		match codec {
			#[cfg(feature = "vpx")]
			Codec::Vp8 | Codec::Vp9 if vpx::check(codec, false).is_ok() => {
				list.push((DecoderBackend::Libvpx, false));
			}
			Codec::H264 if self.has_openh264() => list.push((DecoderBackend::OpenH264, false)),
			#[cfg(feature = "av1")]
			Codec::Av1 => list.push((DecoderBackend::Dav1d, false)),
			_ => {}
		}
		list
	}

	/// The decoders of `codec` in the order a viewer tries them, its ladder:
	/// hardware first (unless the [`DecoderPreference`] turns it off), then
	/// FFmpeg's software decoders, then the built-in libvpx, OpenH264 and
	/// dav1d; a backend named in the preference before all of them.
	pub fn decoders_for(&self, codec: Codec) -> Vec<DecoderBackend> {
		let all = self.decoder_candidates(codec);
		let preference = &self.decoder_preference;
		let automatic = |c: &&(DecoderBackend, bool)| preference.hardware || !c.1;
		let chosen: Vec<&(DecoderBackend, bool)> = match &preference.backend {
			BackendChoice::Auto => all.iter().filter(automatic).collect(),
			BackendChoice::Software => all.iter().filter(|c| !c.1).collect(),
			BackendChoice::Named(name) => {
				let named = all.iter().filter(|c| c.0.name() == name);
				let rest = all.iter().filter(|c| c.0.name() != name).filter(automatic);
				named.chain(rest).collect()
			}
		};
		chosen.into_iter().map(|c| c.0).collect()
	}

	/// Every usable encoder in the base order, before the preference.
	fn candidates(&self) -> Vec<Candidate> {
		let mut list = Vec::new();
		let factories = |codec: Codec, hardware: bool, list: &mut Vec<Candidate>| {
			for f in self.factories.iter().filter(|f| f.is_hardware() == hardware) {
				if f.codecs().contains(&codec) {
					let automatic = f.is_automatic();
					list.push(Candidate { codec, backend: f.backend(), hardware, automatic });
				}
			}
		};
		for codec in HARDWARE_ORDER {
			factories(codec, true, &mut list);
		}
		#[cfg(feature = "vpx")]
		if vpx::check(Codec::Vp8, true).is_ok() {
			list.push(Candidate::builtin(Codec::Vp8, EncoderBackend::Libvpx));
		}
		factories(Codec::H264, false, &mut list);
		if self.has_openh264() {
			list.push(Candidate::builtin(Codec::H264, EncoderBackend::OpenH264));
		}
		#[cfg(feature = "vpx")]
		if vpx::check(Codec::Vp9, true).is_ok() {
			list.push(Candidate::builtin(Codec::Vp9, EncoderBackend::Libvpx));
		}
		for codec in [Codec::Av1, Codec::H265, Codec::Vp8, Codec::Vp9] {
			factories(codec, false, &mut list);
		}
		list
	}

	/// Encoders in the streamer's order of preference.
	pub fn encoders(&self) -> Vec<(Codec, EncoderBackend)> {
		self.encoders_for(&self.preference)
	}

	/// Encoders in the order `preference` gives.
	pub fn encoders_for(&self, preference: &EncoderPreference) -> Vec<(Codec, EncoderBackend)> {
		let all = self.candidates();
		let automatic = |c: &Candidate| c.automatic && (preference.hardware || !c.hardware);
		let chosen: Vec<&Candidate> = match &preference.backend {
			BackendChoice::Auto => all.iter().filter(|c| automatic(c)).collect(),
			BackendChoice::Software => all.iter().filter(|c| c.automatic && !c.hardware).collect(),
			BackendChoice::Named(name) => {
				let named = all.iter().filter(|c| c.backend.name() == name);
				let rest = all.iter().filter(|c| c.backend.name() != name && automatic(c));
				named.chain(rest).collect()
			}
		};
		chosen.into_iter().map(|c| (c.codec, c.backend)).collect()
	}

	/// Encodable codecs in the streamer's order of preference, without
	/// duplicates (for the offer).
	pub fn encoder_codecs(&self) -> Vec<Codec> {
		let mut codecs = Vec::new();
		for (codec, _) in self.encoders() {
			if !codecs.contains(&codec) {
				codecs.push(codec);
			}
		}
		codecs
	}

	/// A decoder for `codec`: the first of its ladder
	/// ([`decoders_for`](Self::decoders_for)) that opens.
	pub fn new_decoder(&self, codec: Codec) -> Result<Box<dyn VideoDecoder>> {
		self.check_decoder(codec)?;
		let mut last_error = None;
		for backend in self.decoders_for(codec) {
			match self.new_decoder_with(codec, backend) {
				Ok(decoder) => return Ok(decoder),
				Err(e) => {
					tracing::warn!(%codec, %backend, "decoder failed to open: {e}");
					last_error = Some(e);
				}
			}
		}
		Err(last_error.unwrap_or_else(|| unavailable(codec, "no decoder in this build")))
	}

	/// A decoder from a specific backend.
	pub fn new_decoder_with(
		&self,
		codec: Codec,
		backend: DecoderBackend,
	) -> Result<Box<dyn VideoDecoder>> {
		#[cfg(target_os = "android")]
		if backend == DecoderBackend::Hardware("mediacodec") {
			return Ok(Box::new(mediacodec::MediaCodecDecoder::new(codec)?));
		}
		if let Some(factory) =
			self.decoder_factories.iter().find(|f| f.backend() == backend && f.codec() == codec)
		{
			return factory.create();
		}
		match backend {
			#[cfg(feature = "vpx")]
			DecoderBackend::Libvpx if matches!(codec, Codec::Vp8 | Codec::Vp9) => {
				Ok(Box::new(vpx::VpxDecoder::new(codec)?))
			}
			#[cfg(feature = "openh264")]
			DecoderBackend::OpenH264 if codec == Codec::H264 => match &self.openh264 {
				Some(lib) => Ok(Box::new(lib.decoder()?)),
				None => Err(unavailable(codec, "the OpenH264 library is not loaded")),
			},
			#[cfg(feature = "av1")]
			DecoderBackend::Dav1d if codec == Codec::Av1 => Ok(Box::new(av1::Dav1dDecoder::new()?)),
			_ => Err(unavailable(codec, format!("no decoder {backend} for it here"))),
		}
	}

	/// An encoder for `codec` from the most preferred backend that works.
	pub fn new_encoder(
		&self,
		codec: Codec,
		config: EncoderConfig,
	) -> Result<Box<dyn VideoEncoder>> {
		self.new_encoder_preferring(codec, config, &self.preference)
	}

	/// As [`new_encoder`](Self::new_encoder) with another preference.
	pub fn new_encoder_preferring(
		&self,
		codec: Codec,
		config: EncoderConfig,
		preference: &EncoderPreference,
	) -> Result<Box<dyn VideoEncoder>> {
		let mut last_error = None;
		for (c, backend) in self.encoders_for(preference) {
			if c != codec {
				continue;
			}
			match self.new_encoder_with(codec, backend, config.clone()) {
				Ok(encoder) => return Ok(encoder),
				Err(e) => {
					tracing::warn!(%codec, %backend, "encoder backend failed: {e}");
					last_error = Some(e);
				}
			}
		}
		Err(last_error.unwrap_or_else(|| unavailable(codec, "no encoder in this build")))
	}

	/// An encoder from a specific backend.
	pub fn new_encoder_with(
		&self,
		codec: Codec,
		backend: EncoderBackend,
		config: EncoderConfig,
	) -> Result<Box<dyn VideoEncoder>> {
		match backend {
			EncoderBackend::Hardware(_) | EncoderBackend::Ffmpeg(_) => {
				let factory = self
					.factories
					.iter()
					.find(|f| f.backend() == backend)
					.ok_or_else(|| unavailable(codec, format!("no encoder {backend} here")))?;
				factory.create(codec, &config)
			}
			#[cfg(feature = "vpx")]
			EncoderBackend::Libvpx => Ok(Box::new(vpx::VpxEncoder::new(codec, config)?)),
			#[cfg(feature = "openh264")]
			EncoderBackend::OpenH264 if codec == Codec::H264 => match &self.openh264 {
				Some(lib) => Ok(Box::new(lib.encoder(config)?)),
				None => Err(unavailable(codec, "the OpenH264 library is not loaded")),
			},
			#[allow(unreachable_patterns)]
			_ => Err(unavailable(codec, format!("{backend} cannot encode it in this build"))),
		}
	}

	/// The first codec in our encoder preference that the viewer accepts.
	pub fn pick_encoder(&self, accepted: &[Codec]) -> Option<Codec> {
		self.encoder_codecs().into_iter().find(|c| accepted.contains(c))
	}

	/// Every encoder backend this build knows, with what works here and why
	/// the rest does not, and each one's rank under the current preference.
	pub fn report(&self) -> EncoderReport {
		let order = self.encoders();
		let rank = |codec: Codec, backend: EncoderBackend| {
			order.iter().filter(|(c, _)| *c == codec).position(|(_, b)| *b == backend)
		};
		let mut encoders = Vec::new();
		let mut add = |backend: EncoderBackend, api: &str, codec, hardware, status| {
			encoders.push(EncoderInfo {
				name: backend.name().to_owned(),
				api: api.to_owned(),
				codec,
				hardware,
				rank: rank(codec, backend),
				status,
			});
		};
		for f in &self.factories {
			if matches!(f.backend(), EncoderBackend::Hardware(_)) {
				for codec in f.codecs() {
					add(f.backend(), f.name(), codec, f.is_hardware(), Ok(()));
				}
			}
		}
		#[cfg(feature = "ffmpeg")]
		for status in crate::ffmpeg::probe() {
			let spec = status.spec;
			let backend = EncoderBackend::Ffmpeg(spec.name);
			add(backend, spec.api, spec.codec, spec.is_hardware(), status.available.clone());
		}
		for codec in [Codec::Vp8, Codec::Vp9] {
			#[cfg(feature = "vpx")]
			let status = vpx::check(codec, true).map_err(|e| e.to_string());
			#[cfg(not(feature = "vpx"))]
			let status = Err("built without the `vpx` feature".to_owned());
			add(EncoderBackend::Libvpx, "libvpx", codec, false, status);
		}
		let status = if self.has_openh264() {
			Ok(())
		} else if cfg!(feature = "openh264") {
			Err("Cisco's OpenH264 library is not loaded".to_owned())
		} else {
			Err("built without the `openh264` feature".to_owned())
		};
		add(EncoderBackend::OpenH264, "OpenH264 (Cisco)", Codec::H264, false, status);
		#[cfg(feature = "ffmpeg")]
		let (ffmpeg, zero_copy) = match crate::ffmpeg::Ffmpeg::get() {
			Ok(ffmpeg) => {
				let info = ffmpeg.info();
				let text = format!(
					"FFmpeg {} (libavcodec {}, libavutil {}) from {}",
					info.release,
					info.avcodec,
					info.avutil,
					info.path.display()
				);
				let vaapi = encoders.iter().any(|e| e.name.ends_with("_vaapi") && e.status.is_ok());
				let zero_copy = match (&info.dmabuf_import, vaapi) {
					(Err(e), _) => Err(e.clone()),
					(Ok(()), false) => Err("no working VA-API encoder".to_owned()),
					(Ok(()), true) => Ok(()),
				};
				(Ok(text), zero_copy)
			}
			Err(e) => (Err(e.to_owned()), Err("FFmpeg is not loaded".to_owned())),
		};
		#[cfg(not(feature = "ffmpeg"))]
		let (ffmpeg, zero_copy) = (
			Err("built without the `ffmpeg` feature".to_owned()),
			Err("built without the `ffmpeg` feature".to_owned()),
		);
		EncoderReport { ffmpeg, zero_copy, encoders, decoders: self.decoder_report() }
	}

	/// Every decoder this build knows, with what works here and why the
	/// rest does not, and each one's place in its codec's ladder.
	fn decoder_report(&self) -> Vec<DecoderInfo> {
		let ladders: Vec<(Codec, Vec<DecoderBackend>)> =
			Codec::ALL.into_iter().map(|c| (c, self.decoders_for(c))).collect();
		let mut decoders = Vec::new();
		let mut add = |backend: DecoderBackend, api: &str, codec, hardware, status| {
			let ladder = ladders.iter().find(|(c, _)| *c == codec).map(|(_, l)| l);
			decoders.push(DecoderInfo {
				name: backend.name().to_owned(),
				api: api.to_owned(),
				codec,
				hardware,
				rank: ladder.and_then(|l| l.iter().position(|b| *b == backend)),
				status,
			});
		};
		#[cfg(target_os = "android")]
		for codec in Codec::ALL {
			if mediacodec::check_decoder(codec).is_ok() {
				add(DecoderBackend::Hardware("mediacodec"), "MediaCodec", codec, true, Ok(()));
			}
		}
		for f in &self.decoder_factories {
			if !matches!(f.backend(), DecoderBackend::Ffmpeg(_)) {
				add(f.backend(), f.api(), f.codec(), f.is_hardware(), Ok(()));
			}
		}
		#[cfg(feature = "ffmpeg")]
		for status in crate::ffmpeg::decoder::probe() {
			let spec = status.spec;
			let backend = DecoderBackend::Ffmpeg(spec.name);
			add(backend, spec.api, spec.codec, spec.is_hardware(), status.available.clone());
		}
		for codec in [Codec::Vp8, Codec::Vp9] {
			#[cfg(feature = "vpx")]
			let status = vpx::check(codec, false).map_err(|e| e.to_string());
			#[cfg(not(feature = "vpx"))]
			let status = Err("built without the `vpx` feature".to_owned());
			add(DecoderBackend::Libvpx, "libvpx", codec, false, status);
		}
		let status = if self.has_openh264() {
			Ok(())
		} else if cfg!(feature = "openh264") {
			Err("Cisco's OpenH264 library is not loaded".to_owned())
		} else {
			Err("built without the `openh264` feature".to_owned())
		};
		add(DecoderBackend::OpenH264, "OpenH264 (Cisco)", Codec::H264, false, status);
		let status = if cfg!(feature = "av1") {
			Ok(())
		} else {
			Err("built without the `av1` feature".to_owned())
		};
		add(DecoderBackend::Dav1d, "dav1d", Codec::Av1, false, status);
		decoders
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn codec_names() {
		for c in Codec::ALL {
			assert_eq!(c.name().parse::<Codec>().unwrap(), c);
		}
		assert_eq!("video/h264".parse::<Codec>().unwrap(), Codec::H264);
		assert_eq!(" vp9 ".parse::<Codec>().unwrap(), Codec::Vp9);
		assert!("opus".parse::<Codec>().is_err());
		assert_eq!(Codec::Av1.to_string(), "AV1");
	}

	#[cfg(feature = "str0m")]
	#[test]
	fn str0m_mapping() {
		for c in Codec::ALL {
			let s: str0m::format::Codec = c.into();
			assert_eq!(Codec::try_from(s).unwrap(), c);
		}
		assert!(Codec::try_from(str0m::format::Codec::Opus).is_err());
	}

	#[test]
	fn preference_order() {
		let codecs = Codecs::builtin();
		// Without OpenH264, H.264 is neither offered nor accepted.
		assert!(!codecs.decoders().contains(&Codec::H264));
		assert!(codecs.new_decoder(Codec::H264).is_err());
		let err = codecs.new_encoder(Codec::H264, EncoderConfig::default()).err().unwrap();
		assert!(matches!(err, Error::CodecUnavailable { codec: Codec::H264, .. }), "{err}");
		assert!(codecs.new_decoder(Codec::H265).is_err());
		#[cfg(feature = "vpx")]
		{
			assert_eq!(codecs.decoders()[..2], [Codec::Vp9, Codec::Vp8]);
			assert_eq!(codecs.encoder_codecs()[0], Codec::Vp8);
			assert_eq!(codecs.pick_encoder(&[Codec::H264, Codec::Vp9]), Some(Codec::Vp9));
			assert_eq!(codecs.pick_encoder(&[Codec::Vp8, Codec::Vp9]), Some(Codec::Vp8));
		}
		#[cfg(feature = "av1")]
		assert!(codecs.decoders().contains(&Codec::Av1));
		// Decoders keep the viewer order.
		let order: Vec<usize> = codecs
			.decoders()
			.iter()
			.map(|c| VIEWER_PREFERENCE.iter().position(|p| p == c).unwrap())
			.collect();
		assert!(order.windows(2).all(|w| w[0] < w[1]));
	}

	struct Fake(&'static str, Codec, bool);

	impl hw::EncoderFactory for Fake {
		fn name(&self) -> &'static str {
			self.0
		}

		fn codecs(&self) -> Vec<Codec> {
			vec![self.1]
		}

		fn create(&self, codec: Codec, _: &EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
			Err(unavailable(codec, "fake"))
		}

		fn is_hardware(&self) -> bool {
			self.2
		}

		fn backend(&self) -> EncoderBackend {
			EncoderBackend::Ffmpeg(self.0)
		}
	}

	#[test]
	fn encoder_preference() {
		let mut codecs = Codecs::builtin();
		codecs.factories = vec![
			Arc::new(Fake("x264", Codec::H264, false)),
			Arc::new(Fake("vp8_gpu", Codec::Vp8, true)),
			Arc::new(Fake("h264_gpu", Codec::H264, true)),
			Arc::new(Fake("hevc_gpu", Codec::H265, true)),
			Arc::new(Fake("av1_sw", Codec::Av1, false)),
		];
		let names = |preference: EncoderPreference| -> Vec<&'static str> {
			codecs.encoders_for(&preference).iter().map(|(_, b)| b.name()).collect()
		};
		let auto = names(EncoderPreference::default());
		// Hardware in codec order (H.264 after VP8, HEVC last), then software.
		assert_eq!(auto[..3], ["vp8_gpu", "h264_gpu", "hevc_gpu"]);
		let position = |list: &[&str], name| list.iter().position(|n| *n == name).unwrap();
		assert!(position(&auto, "x264") < position(&auto, "av1_sw"));
		#[cfg(feature = "vpx")]
		assert!(position(&auto, "libvpx") < position(&auto, "x264"), "VP8 before software H.264");
		let no_hardware = names(EncoderPreference { hardware: false, ..Default::default() });
		assert!(!no_hardware.iter().any(|n| n.ends_with("_gpu")));
		let software =
			names(EncoderPreference { hardware: true, backend: "software".parse().unwrap() });
		assert_eq!(software, no_hardware);
		// A named backend comes first, even hardware with acceleration off.
		let named =
			names(EncoderPreference { hardware: false, backend: "hevc_gpu".parse().unwrap() });
		assert_eq!(named[0], "hevc_gpu");
		assert!(!named[1..].iter().any(|n| n.ends_with("_gpu")));
		// The codecs follow the preference.
		codecs.set_preference(EncoderPreference::default());
		assert_eq!(codecs.encoder_codecs()[..3], [Codec::Vp8, Codec::H264, Codec::H265]);
		// The report ranks what the preference uses.
		let report = codecs.report();
		let openh264 = report.encoders.iter().find(|e| e.name == "openh264").unwrap();
		assert!(openh264.status.is_err() && openh264.rank.is_none());
		assert_eq!("auto".parse::<BackendChoice>().unwrap(), BackendChoice::Auto);
		assert_eq!(BackendChoice::Named("h264_vaapi".into()).to_string(), "h264_vaapi");
	}

	#[test]
	fn thread_heuristic() {
		let config = EncoderConfig { threads: 3, ..EncoderConfig::default() };
		assert_eq!(config.threads_for(1920, 1080), 3);
		assert_eq!(config.threads_for(320, 240), 1, "capped by the frame size");
		assert_eq!(EncoderConfig::default().threads_for(320, 240), 1);
	}
}
