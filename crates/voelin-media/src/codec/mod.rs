//! Video codecs: common types, the encoder/decoder traits, codec preference
//! and backend selection.
//!
//! Backends:
//! - [`vpx`] (feature `vpx`): VP8 and VP9 encoding and decoding with the
//!   system libvpx
//! - [`h264`] (feature `openh264`): H.264 with Cisco's prebuilt OpenH264,
//!   loaded at runtime
//! - [`av1`] (feature `av1`): AV1 decoding with the system libdav1d
//! - [`hw`]: hardware encoders (VA-API, Media Foundation); not implemented yet
//! - `mediacodec` (Android): the device's MediaCodec encoders and decoders
//!   (hardware, or Google's software VP8 / VP9 / H.264)

use std::fmt;
use std::str::FromStr;

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

/// Video codecs TeamSpeak 6 streams use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
	Vp8,
	Vp9,
	H264,
	Av1,
}

impl Codec {
	pub const ALL: [Codec; 4] = [Codec::Vp8, Codec::Vp9, Codec::H264, Codec::Av1];

	/// Name as in SDP (`a=rtpmap:<pt> <name>/90000`).
	pub fn name(self) -> &'static str {
		match self {
			Codec::Vp8 => "VP8",
			Codec::Vp9 => "VP9",
			Codec::H264 => "H264",
			Codec::Av1 => "AV1",
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
			other => Err(Error::Convert(format!("not a video codec: {other:?}"))),
		}
	}
}

/// Order in which a viewer accepts codecs: VP9 > VP8 > AV1 > H.264.
pub const VIEWER_PREFERENCE: [Codec; 4] = [Codec::Vp9, Codec::Vp8, Codec::Av1, Codec::H264];

/// What the encoder should optimise for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContentHint {
	/// Screen content: sharp text, mostly static.
	#[default]
	Screen,
	/// Camera-like content with motion.
	Motion,
}

/// Encoder settings. The resolution follows the frames: the first frame and
/// every size change start a new keyframe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderConfig {
	/// Expected frame rate (rate control; timestamps come from the frames).
	pub fps: u32,
	/// Target bitrate in bits per second. TeamSpeak caps streams at 10 Mbit/s.
	pub bitrate_bps: u32,
	/// Maximum frames between keyframes; `None` sends keyframes only at the
	/// start and on request (PLI), like WebRTC.
	pub keyframe_interval: Option<u32>,
	pub content: ContentHint,
	/// Encoder threads; 0 picks a number from the resolution and CPU count.
	pub threads: u32,
	/// A fixed speed / quality trade-off in the backend's own terms (libvpx
	/// `cpu-used`); `None` lets the encoder adapt it to how long frames take
	/// to encode compared to the frame interval.
	pub speed: Option<i32>,
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
		}
	}
}

impl EncoderConfig {
	/// Threads for a `width` x `height` encode: all CPUs, but no more than
	/// one per 320x240 pixels (more would idle on small frames).
	pub fn threads_for(&self, width: u32, height: u32) -> u32 {
		if self.threads > 0 {
			return self.threads;
		}
		let cpus = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
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
}

/// A video decoder.
pub trait VideoDecoder: Send {
	fn codec(&self) -> Codec;

	/// Decode one complete frame (a depacketized RTP frame). Returns the
	/// picture if one is ready; its timestamp is zero.
	fn decode(&mut self, data: &[u8]) -> Result<Option<VideoFrame>>;
}

/// Implementations behind [`VideoEncoder`], in the streamer's order of
/// preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EncoderBackend {
	/// An OS / GPU encoder (see [`hw`]).
	Hardware(&'static str),
	Libvpx,
	OpenH264,
}

fn unavailable(codec: Codec, reason: impl Into<String>) -> Error {
	Error::CodecUnavailable { codec, reason: reason.into() }
}

/// The codecs this build and machine can use, and factories for them.
///
/// Viewer order ([`Codecs::decoders`]): VP9 > VP8 > AV1 > H.264. Streamer
/// order ([`Codecs::encoders`]): hardware > VP8 (libvpx) > H.264 (OpenH264)
/// > VP9 (libvpx, costly in software).
pub struct Codecs {
	hardware: Vec<Box<dyn hw::HardwareEncoderFactory>>,
	#[cfg(feature = "openh264")]
	openh264: Option<h264::OpenH264>,
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
			.finish()
	}
}

impl Codecs {
	/// Software codecs compiled in, plus whatever hardware encoders
	/// [`hw::probe`] finds. H.264 needs [`Codecs::with_openh264`].
	pub fn new() -> Self {
		Self {
			hardware: hw::probe(),
			#[cfg(feature = "openh264")]
			openh264: None,
		}
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

	fn has_openh264(&self) -> bool {
		#[cfg(feature = "openh264")]
		return self.openh264.is_some();
		#[cfg(not(feature = "openh264"))]
		return false;
	}

	/// Whether `codec` can be decoded, or why not.
	pub fn check_decoder(&self, codec: Codec) -> Result<()> {
		#[cfg(target_os = "android")]
		if mediacodec::check_decoder(codec).is_ok() {
			return Ok(());
		}
		match codec {
			Codec::Vp8 | Codec::Vp9 => {
				#[cfg(feature = "vpx")]
				return vpx::check(codec, false);
				#[cfg(not(feature = "vpx"))]
				return Err(unavailable(codec, "built without the `vpx` feature"));
			}
			Codec::H264 if self.has_openh264() => Ok(()),
			Codec::H264 => Err(unavailable(codec, "the OpenH264 library is not loaded")),
			Codec::Av1 => {
				#[cfg(feature = "av1")]
				return Ok(());
				#[cfg(not(feature = "av1"))]
				return Err(unavailable(codec, "built without the `av1` feature"));
			}
		}
	}

	/// Decodable codecs in the viewer's order of preference.
	pub fn decoders(&self) -> Vec<Codec> {
		VIEWER_PREFERENCE.into_iter().filter(|&c| self.check_decoder(c).is_ok()).collect()
	}

	/// Encoders in the streamer's order of preference.
	pub fn encoders(&self) -> Vec<(Codec, EncoderBackend)> {
		let mut list = Vec::new();
		for factory in &self.hardware {
			for codec in factory.codecs() {
				list.push((codec, EncoderBackend::Hardware(factory.name())));
			}
		}
		#[cfg(feature = "vpx")]
		if vpx::check(Codec::Vp8, true).is_ok() {
			list.push((Codec::Vp8, EncoderBackend::Libvpx));
		}
		if self.has_openh264() {
			list.push((Codec::H264, EncoderBackend::OpenH264));
		}
		#[cfg(feature = "vpx")]
		if vpx::check(Codec::Vp9, true).is_ok() {
			list.push((Codec::Vp9, EncoderBackend::Libvpx));
		}
		list
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

	pub fn new_decoder(&self, codec: Codec) -> Result<Box<dyn VideoDecoder>> {
		#[cfg(target_os = "android")]
		if mediacodec::check_decoder(codec).is_ok() {
			return Ok(Box::new(mediacodec::MediaCodecDecoder::new(codec)?));
		}
		self.check_decoder(codec)?;
		match codec {
			#[cfg(feature = "vpx")]
			Codec::Vp8 | Codec::Vp9 => Ok(Box::new(vpx::VpxDecoder::new(codec)?)),
			#[cfg(feature = "openh264")]
			Codec::H264 => match &self.openh264 {
				Some(lib) => Ok(Box::new(lib.decoder()?)),
				None => Err(unavailable(codec, "the OpenH264 library is not loaded")),
			},
			#[cfg(feature = "av1")]
			Codec::Av1 => Ok(Box::new(av1::Dav1dDecoder::new()?)),
			#[allow(unreachable_patterns)]
			_ => Err(unavailable(codec, "no decoder in this build")),
		}
	}

	/// An encoder for `codec` from the most preferred backend that works.
	pub fn new_encoder(
		&self,
		codec: Codec,
		config: EncoderConfig,
	) -> Result<Box<dyn VideoEncoder>> {
		let mut last_error = None;
		for (c, backend) in self.encoders() {
			if c != codec {
				continue;
			}
			match self.new_encoder_with(codec, backend, config.clone()) {
				Ok(encoder) => return Ok(encoder),
				Err(e) => {
					tracing::warn!(%codec, ?backend, "encoder backend failed: {e}");
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
			EncoderBackend::Hardware(name) => {
				let factory = self
					.hardware
					.iter()
					.find(|f| f.name() == name)
					.ok_or_else(|| unavailable(codec, format!("no hardware encoder {name}")))?;
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
			_ => Err(unavailable(codec, format!("{backend:?} cannot encode it in this build"))),
		}
	}

	/// The first codec in our encoder preference that the viewer accepts.
	pub fn pick_encoder(&self, accepted: &[Codec]) -> Option<Codec> {
		self.encoder_codecs().into_iter().find(|c| accepted.contains(c))
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
		let codecs = Codecs::new();
		// Without OpenH264, H.264 is neither offered nor accepted.
		assert!(!codecs.decoders().contains(&Codec::H264));
		assert!(codecs.new_decoder(Codec::H264).is_err());
		let err = codecs.new_encoder(Codec::H264, EncoderConfig::default()).err().unwrap();
		assert!(matches!(err, Error::CodecUnavailable { codec: Codec::H264, .. }), "{err}");
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

	#[test]
	fn thread_heuristic() {
		let config = EncoderConfig { threads: 3, ..EncoderConfig::default() };
		assert_eq!(config.threads_for(1920, 1080), 3);
		assert_eq!(EncoderConfig::default().threads_for(320, 240), 1);
	}
}
