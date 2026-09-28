//! Android MediaCodec (NDK `AMediaCodec`) encoders and decoders.
//!
//! Byte-buffer mode on both sides, so frames stay [`VideoFrame`]s like with
//! the software codecs: encoders take I420 / NV12 (whatever the codec asks
//! for, see [`ImageLayout`]), decoders return I420 or NV12. The device's
//! default codec for a MIME type is used; most phones have hardware H.264,
//! many hardware VP8 / VP9, and every Android has Google's software VP8,
//! VP9 and H.264 (`c2.android.*`).
//!
//! H.264 is encoded as High profile without B-frames (the Constrained High
//! stream TeamSpeak negotiates, see `voelin-stream`), SPS/PPS in front of every
//! keyframe.

// AMediaCodec handles are used from one thread at a time (`&mut self`), which
// the NDK allows from any thread; the wrapper only asserts that.
#![allow(unsafe_code)]

use std::sync::OnceLock;
use std::time::Duration;

use ndk::media::media_codec::{
	DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use tracing::{debug, info};

use super::hw::EncoderFactory;
use super::image_layout::{
	COLOR_FORMAT_FLEXIBLE, COLOR_FORMAT_I420, COLOR_FORMAT_NV12, ImageLayout,
};
use super::{Codec, EncodedFrame, EncoderBackend, EncoderConfig, VideoDecoder, VideoEncoder};
use crate::frame::{FrameData, VideoFrame};
use crate::{Error, Result, convert};

/// `BUFFER_FLAG_KEY_FRAME`.
const FLAG_KEY_FRAME: u32 = 1;
/// `BUFFER_FLAG_CODEC_CONFIG`: SPS/PPS (H.264), no picture.
const FLAG_CODEC_CONFIG: u32 = 2;
/// `AVCProfileHigh`, `AVCProfileConstrainedHigh`.
const AVC_PROFILE_HIGH: i32 = 0x08;
const AVC_PROFILE_CONSTRAINED_HIGH: i32 = 0x80000;
/// `BITRATE_MODE_VBR`.
const BITRATE_MODE_VBR: i32 = 1;
/// Keyframe interval for "only on request" (seconds; what WebRTC uses).
const NO_PERIODIC_KEYFRAMES: i32 = 3600;
/// How long `decode` / `encode` wait for a free input buffer.
const INPUT_WAIT: Duration = Duration::from_millis(20);
/// How long they wait for the first output of a call.
const OUTPUT_WAIT: Duration = Duration::from_millis(5);

pub const NAME: &str = "mediacodec";

fn mime(codec: Codec) -> &'static str {
	match codec {
		Codec::Vp8 => "video/x-vnd.on2.vp8",
		Codec::Vp9 => "video/x-vnd.on2.vp9",
		Codec::H264 => "video/avc",
		Codec::Av1 => "video/av01",
		Codec::H265 => "video/hevc",
	}
}

/// Google's software codecs (as opposed to the vendor's hardware ones).
fn is_software(name: &str) -> bool {
	name.starts_with("c2.android.") || name.starts_with("OMX.google.") || name.contains(".sw.")
}

/// An `AMediaCodec` that may move between threads.
struct Handle(MediaCodec);

// SAFETY: see the module attribute; `Handle` is only reached through
// `&mut self` of its owner, never shared.
unsafe impl Send for Handle {}

impl Drop for Handle {
	fn drop(&mut self) {
		let _ = self.0.stop();
	}
}

fn encoder_error(codec: Codec, context: &str, e: impl std::fmt::Display) -> Error {
	Error::Encoder { codec, message: format!("{context}: {e}") }
}

fn decoder_error(codec: Codec, context: &str, e: impl std::fmt::Display) -> Error {
	Error::Decoder { codec, message: format!("{context}: {e}") }
}

/// The default codec's name per MIME type and direction, probed once.
struct Available {
	encoders: Vec<(Codec, String)>,
	decoders: Vec<(Codec, String)>,
}

fn available() -> &'static Available {
	static AVAILABLE: OnceLock<Available> = OnceLock::new();
	AVAILABLE.get_or_init(|| {
		let probe = |create: fn(&str) -> Option<MediaCodec>| {
			Codec::ALL
				.into_iter()
				.filter_map(|codec| {
					let handle = Handle(create(mime(codec))?);
					let name = handle.0.name().unwrap_or_else(|_| "?".into());
					Some((codec, name))
				})
				.collect::<Vec<_>>()
		};
		let found = Available {
			encoders: probe(MediaCodec::from_encoder_type),
			decoders: probe(MediaCodec::from_decoder_type),
		};
		info!(encoders = ?found.encoders, decoders = ?found.decoders, "MediaCodec");
		found
	})
}

/// Whether the device decodes `codec`.
pub fn check_decoder(codec: Codec) -> Result<()> {
	if available().decoders.iter().any(|(c, _)| *c == codec) {
		Ok(())
	} else {
		Err(Error::CodecUnavailable { codec, reason: "no MediaCodec decoder".into() })
	}
}

/// Encoders of this device: hardware ones first (H.264, VP9, VP8), then
/// Google's software VP8 and H.264. Software VP9 and AV1 are too slow.
pub fn probe() -> Option<MediaCodecFactory> {
	let encoders = &available().encoders;
	let name = |codec| encoders.iter().find(|(c, _)| *c == codec).map(|(_, n)| n.as_str());
	let mut codecs = Vec::new();
	for codec in [Codec::H264, Codec::Vp9, Codec::Vp8, Codec::Av1] {
		if name(codec).is_some_and(|n| !is_software(n)) {
			codecs.push(codec);
		}
	}
	for codec in [Codec::Vp8, Codec::H264] {
		if name(codec).is_some_and(is_software) {
			codecs.push(codec);
		}
	}
	(!codecs.is_empty()).then_some(MediaCodecFactory { codecs })
}

pub struct MediaCodecFactory {
	codecs: Vec<Codec>,
}

impl EncoderFactory for MediaCodecFactory {
	fn name(&self) -> &'static str {
		NAME
	}

	fn codecs(&self) -> Vec<Codec> {
		self.codecs.clone()
	}

	fn create(&self, codec: Codec, config: &EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
		if !self.codecs.contains(&codec) {
			return Err(Error::CodecUnavailable { codec, reason: "no MediaCodec encoder".into() });
		}
		Ok(Box::new(MediaCodecEncoder { codec, config: config.clone(), session: None }))
	}
}

/// A MediaCodec encoder. The codec is (re)created for the frame size.
pub struct MediaCodecEncoder {
	codec: Codec,
	config: EncoderConfig,
	session: Option<EncoderSession>,
}

struct EncoderSession {
	handle: Handle,
	layout: ImageLayout,
	/// H.264 SPS/PPS, put in front of keyframes that lack them.
	parameter_sets: Vec<u8>,
	/// Staging buffer for the codec's input layout.
	staging: Vec<u8>,
	last_pts_us: Option<u64>,
}

impl MediaCodecEncoder {
	fn format(
		&self,
		width: u32,
		height: u32,
		color_format: i32,
		profile: Option<i32>,
	) -> MediaFormat {
		let mut format = MediaFormat::new();
		format.set_str("mime", mime(self.codec));
		format.set_i32("width", width as i32);
		format.set_i32("height", height as i32);
		format.set_i32("color-format", color_format);
		format.set_i32("bitrate", self.config.bitrate_bps.min(i32::MAX as u32) as i32);
		format.set_i32("bitrate-mode", BITRATE_MODE_VBR);
		format.set_i32("frame-rate", self.config.fps.max(1) as i32);
		let interval = match self.config.keyframe_interval {
			Some(frames) => frames.div_ceil(self.config.fps.max(1)).max(1) as i32,
			None => NO_PERIODIC_KEYFRAMES,
		};
		format.set_i32("i-frame-interval", interval);
		if self.codec == Codec::H264 {
			format.set_i32("max-bframes", 0);
			format.set_i32("prepend-sps-pps-to-idr-frames", 1);
			if let Some(profile) = profile {
				format.set_i32("profile", profile);
			}
		}
		format
	}

	/// Start a codec for `width` x `height`: NV12 input, else I420; for
	/// H.264 Constrained High, else High, else the codec's default profile.
	fn start(&self, width: u32, height: u32) -> Result<EncoderSession> {
		let codec = self.codec;
		let profiles: &[Option<i32>] = if codec == Codec::H264 {
			&[Some(AVC_PROFILE_CONSTRAINED_HIGH), Some(AVC_PROFILE_HIGH), None]
		} else {
			&[None]
		};
		let mut last_error = String::from("no configuration tried");
		for color_format in [COLOR_FORMAT_NV12, COLOR_FORMAT_I420] {
			for &profile in profiles {
				let handle = Handle(
					MediaCodec::from_encoder_type(mime(codec))
						.ok_or_else(|| encoder_error(codec, "create", "no encoder"))?,
				);
				let format = self.format(width, height, color_format, profile);
				if let Err(e) = handle.0.configure(&format, None, MediaCodecDirection::Encoder) {
					last_error = format!("configure {color_format}/{profile:?}: {e}");
					continue;
				}
				let layout = input_layout(&handle.0.input_format(), width, height, color_format);
				handle.0.start().map_err(|e| encoder_error(codec, "start", e))?;
				debug!(%codec, width, height, color_format, ?profile, ?layout, "MediaCodec encoder");
				return Ok(EncoderSession {
					handle,
					layout,
					parameter_sets: Vec::new(),
					staging: Vec::new(),
					last_pts_us: None,
				});
			}
		}
		Err(encoder_error(codec, "no working configuration", last_error))
	}
}

/// Where the encoder wants its input: `image-data` if it says, else the
/// color format with its `stride` / `slice-height`.
fn input_layout(format: &MediaFormat, width: u32, height: u32, requested: i32) -> ImageLayout {
	if let Some(layout) = format
		.buffer("image-data")
		.and_then(ImageLayout::from_media_image2)
		.filter(|l| l.width >= width && l.height >= height)
	{
		return layout.cropped(width, height);
	}
	let stride = format.i32("stride").and_then(|s| usize::try_from(s).ok()).unwrap_or(0);
	let slice = format.i32("slice-height").and_then(|s| usize::try_from(s).ok()).unwrap_or(0);
	let color = match format.i32("color-format") {
		Some(c @ (COLOR_FORMAT_I420 | COLOR_FORMAT_NV12)) => c,
		// Flexible or unset: what we asked for.
		_ => requested,
	};
	ImageLayout::from_color_format(color, width, height, stride, slice)
		.unwrap_or_else(|| ImageLayout::nv12(width, height, stride, slice))
}

/// The top-left `width` x `height` of an I420 frame (planes keep their
/// stride, so nothing is copied).
fn crop_i420(mut frame: VideoFrame, width: u32, height: u32) -> VideoFrame {
	debug_assert!(matches!(frame.data, FrameData::I420 { .. }));
	frame.width = width;
	frame.height = height;
	frame
}

/// H.264 NAL unit type of the first NAL after a start code.
fn first_nal_type(data: &[u8]) -> Option<u8> {
	let start = if data.starts_with(&[0, 0, 0, 1]) {
		4
	} else if data.starts_with(&[0, 0, 1]) {
		3
	} else {
		return None;
	};
	data.get(start).map(|b| b & 0x1f)
}

impl VideoEncoder for MediaCodecEncoder {
	fn codec(&self) -> Codec {
		self.codec
	}

	fn backend(&self) -> EncoderBackend {
		EncoderBackend::Hardware(NAME)
	}

	fn encode(&mut self, frame: &VideoFrame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
		let codec = self.codec;
		// Encoders want even sizes.
		let (width, height) = (frame.width & !1, frame.height & !1);
		if width == 0 || height == 0 {
			return Err(Error::InvalidFrame(format!("{}x{} frame", frame.width, frame.height)));
		}
		let resized = self
			.session
			.as_ref()
			.is_some_and(|s| (s.layout.width, s.layout.height) != (width, height));
		if resized || self.session.is_none() {
			self.session = None;
			self.session = Some(self.start(width, height)?);
		}
		let session = self.session.as_mut().expect("started above");
		let mut i420 = convert::to_i420(frame)?.into_owned();
		if (i420.width, i420.height) != (width, height) {
			i420 = crop_i420(i420, width, height);
		}

		// Strictly increasing presentation times.
		let mut pts_us = i420.timestamp.as_micros() as u64;
		if let Some(last) = session.last_pts_us
			&& pts_us <= last
		{
			pts_us = last + 1;
		}

		if force_keyframe && !resized {
			let mut params = MediaFormat::new();
			params.set_i32("request-sync", 0);
			session
				.handle
				.0
				.set_parameters(params)
				.map_err(|e| encoder_error(codec, "request keyframe", e))?;
		}

		let len = session.layout.buffer_len();
		session.staging.resize(len, 0);
		session.layout.write(&i420, &mut session.staging)?;
		match session
			.handle
			.0
			.dequeue_input_buffer(INPUT_WAIT)
			.map_err(|e| encoder_error(codec, "dequeue input", e))?
		{
			DequeuedInputBufferResult::Buffer(mut buffer) => {
				let dst = buffer.buffer_mut();
				if dst.len() < len {
					return Err(encoder_error(
						codec,
						"input buffer",
						format!("{} bytes, need {len}", dst.len()),
					));
				}
				dst[..len].write_copy_of_slice(&session.staging);
				session
					.handle
					.0
					.queue_input_buffer(buffer, 0, len, pts_us, 0)
					.map_err(|e| encoder_error(codec, "queue input", e))?;
				session.last_pts_us = Some(pts_us);
			}
			// The codec is still busy: skip this frame (rate control).
			DequeuedInputBufferResult::TryAgainLater => {}
		}
		drain_encoder(codec, session)
	}

	fn set_bitrate(&mut self, bps: u32) -> Result<()> {
		self.config.bitrate_bps = bps;
		if let Some(session) = &self.session {
			let mut params = MediaFormat::new();
			params.set_i32("video-bitrate", bps.min(i32::MAX as u32) as i32);
			session
				.handle
				.0
				.set_parameters(params)
				.map_err(|e| encoder_error(self.codec, "set bitrate", e))?;
		}
		Ok(())
	}
}

/// Collect what the encoder has finished.
fn drain_encoder(codec: Codec, session: &mut EncoderSession) -> Result<Vec<EncodedFrame>> {
	let mut out = Vec::new();
	let mut wait = OUTPUT_WAIT;
	loop {
		let result = session
			.handle
			.0
			.dequeue_output_buffer(wait)
			.map_err(|e| encoder_error(codec, "dequeue output", e))?;
		wait = Duration::ZERO;
		match result {
			DequeuedOutputBufferInfoResult::Buffer(buffer) => {
				let info = *buffer.info();
				let offset = usize::try_from(info.offset()).unwrap_or(0);
				let size = usize::try_from(info.size()).unwrap_or(0);
				let data = buffer.buffer().get(offset..offset + size).map(<[u8]>::to_vec);
				session
					.handle
					.0
					.release_output_buffer(buffer, false)
					.map_err(|e| encoder_error(codec, "release output", e))?;
				let Some(mut data) = data.filter(|d| !d.is_empty()) else {
					continue;
				};
				if info.flags() & FLAG_CODEC_CONFIG != 0 {
					session.parameter_sets = data;
					continue;
				}
				let keyframe = info.flags() & FLAG_KEY_FRAME != 0;
				if keyframe
					&& codec == Codec::H264
					&& !session.parameter_sets.is_empty()
					&& first_nal_type(&data) != Some(7)
				{
					let mut with_sps = session.parameter_sets.clone();
					with_sps.append(&mut data);
					data = with_sps;
				}
				let pts_us = u64::try_from(info.presentation_time_us()).unwrap_or(0);
				out.push(EncodedFrame { data, keyframe, pts_90khz: pts_us * 9 / 100 });
			}
			DequeuedOutputBufferInfoResult::OutputFormatChanged
			| DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
			DequeuedOutputBufferInfoResult::TryAgainLater => return Ok(out),
		}
	}
}

/// A MediaCodec decoder.
pub struct MediaCodecDecoder {
	codec: Codec,
	handle: Handle,
	/// Output layout, known after the first format change.
	layout: Option<ImageLayout>,
	pts_us: u64,
}

impl MediaCodecDecoder {
	pub fn new(codec: Codec) -> Result<Self> {
		check_decoder(codec)?;
		let handle = Handle(
			MediaCodec::from_decoder_type(mime(codec))
				.ok_or_else(|| decoder_error(codec, "create", "no decoder"))?,
		);
		let mut format = MediaFormat::new();
		format.set_str("mime", mime(codec));
		// A hint; the stream's own size wins.
		format.set_i32("width", 1920);
		format.set_i32("height", 1080);
		format.set_i32("color-format", COLOR_FORMAT_FLEXIBLE);
		handle
			.0
			.configure(&format, None, MediaCodecDirection::Decoder)
			.map_err(|e| decoder_error(codec, "configure", e))?;
		handle.0.start().map_err(|e| decoder_error(codec, "start", e))?;
		Ok(Self { codec, handle, layout: None, pts_us: 0 })
	}

	fn update_layout(&mut self) {
		let format = self.handle.0.output_format();
		let width = format.i32("width").unwrap_or(0).max(0) as u32;
		let height = format.i32("height").unwrap_or(0).max(0) as u32;
		let (visible_w, visible_h) = match format.rect("crop") {
			Some((left, top, right, bottom)) if left == 0 && top == 0 => {
				((right + 1).max(0) as u32, (bottom + 1).max(0) as u32)
			}
			_ => (width, height),
		};
		let stride = format.i32("stride").and_then(|s| usize::try_from(s).ok()).unwrap_or(0);
		let slice = format.i32("slice-height").and_then(|s| usize::try_from(s).ok()).unwrap_or(0);
		let layout = format
			.buffer("image-data")
			.and_then(ImageLayout::from_media_image2)
			.or_else(|| {
				ImageLayout::from_color_format(
					format.i32("color-format").unwrap_or(0),
					width,
					height,
					stride,
					slice,
				)
			})
			.map(|l| l.cropped(visible_w, visible_h));
		debug!(codec = %self.codec, ?layout, "MediaCodec decoder output");
		self.layout = layout;
	}
}

impl VideoDecoder for MediaCodecDecoder {
	fn codec(&self) -> Codec {
		self.codec
	}

	fn decode(&mut self, data: &[u8]) -> Result<Option<VideoFrame>> {
		let codec = self.codec;
		match self
			.handle
			.0
			.dequeue_input_buffer(INPUT_WAIT)
			.map_err(|e| decoder_error(codec, "dequeue input", e))?
		{
			DequeuedInputBufferResult::Buffer(mut buffer) => {
				let dst = buffer.buffer_mut();
				if dst.len() < data.len() {
					return Err(decoder_error(
						codec,
						"input buffer",
						format!("{} bytes for a {} byte frame", dst.len(), data.len()),
					));
				}
				dst[..data.len()].write_copy_of_slice(data);
				// Only the order matters; the caller knows the RTP time.
				self.pts_us += 33_333;
				self.handle
					.0
					.queue_input_buffer(buffer, 0, data.len(), self.pts_us, 0)
					.map_err(|e| decoder_error(codec, "queue input", e))?;
			}
			DequeuedInputBufferResult::TryAgainLater => {
				return Err(decoder_error(codec, "dequeue input", "the decoder is stalled"));
			}
		}

		// The newest finished picture; older ones are dropped.
		let mut picture = None;
		let mut wait = OUTPUT_WAIT;
		loop {
			let result = self
				.handle
				.0
				.dequeue_output_buffer(wait)
				.map_err(|e| decoder_error(codec, "dequeue output", e))?;
			wait = Duration::ZERO;
			match result {
				DequeuedOutputBufferInfoResult::Buffer(buffer) => {
					let info = *buffer.info();
					let offset = usize::try_from(info.offset()).unwrap_or(0);
					let size = usize::try_from(info.size()).unwrap_or(0);
					let read = match (&self.layout, buffer.buffer().get(offset..offset + size)) {
						(Some(layout), Some(bytes)) if size > 0 => Some(layout.read(bytes)),
						_ => None,
					};
					self.handle
						.0
						.release_output_buffer(buffer, false)
						.map_err(|e| decoder_error(codec, "release output", e))?;
					match read {
						Some(Ok(frame)) => picture = Some(frame),
						Some(Err(e)) => return Err(decoder_error(codec, "output", e)),
						None => {}
					}
				}
				DequeuedOutputBufferInfoResult::OutputFormatChanged => self.update_layout(),
				DequeuedOutputBufferInfoResult::OutputBuffersChanged => {}
				DequeuedOutputBufferInfoResult::TryAgainLater => return Ok(picture),
			}
		}
	}
}
