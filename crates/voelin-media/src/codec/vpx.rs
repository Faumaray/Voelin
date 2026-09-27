//! VP8 and VP9 through the system libvpx.
//!
//! Encoder settings follow libwebrtc's realtime configuration: one pass CBR,
//! no lag (`VPX_DL_REALTIME`), error resilient, keyframes only on request,
//! and screen-content tuning for [`ContentHint::Screen`]
//! (`VP8E_SET_SCREEN_CONTENT_MODE`, `VP9E_SET_TUNE_CONTENT`, static
//! threshold). Threads default to the CPUs the frame size can use; VP8 gets
//! as many token partitions, VP9 row multithreading and tile columns by
//! width. `cpu-used` adapts to the measured encode time (see
//! [`SpeedControl`]) unless [`EncoderConfig::speed`] fixes it. Bitrate
//! changes apply to the running encoder, without a keyframe.

use std::time::{Duration, Instant};

use crate::codec::{
	Codec, ContentHint, EncodedChunk, EncodedFrame, EncoderBackend, EncoderConfig, VideoDecoder,
	VideoEncoder,
};
use crate::convert;
use crate::frame::{FrameData, Plane, VIDEO_CLOCK_RATE, VideoFrame};
use crate::{Error, Result};

mod raw;

fn is_vp9(codec: Codec) -> Result<bool> {
	match codec {
		Codec::Vp8 => Ok(false),
		Codec::Vp9 => Ok(true),
		_ => Err(Error::CodecUnavailable { codec, reason: "not a libvpx codec".into() }),
	}
}

/// Whether the linked libvpx can encode (or decode) `codec`.
pub fn check(codec: Codec, encoder: bool) -> Result<()> {
	if raw::available(is_vp9(codec)?, encoder) {
		Ok(())
	} else {
		let what = if encoder { "encoder" } else { "decoder" };
		Err(Error::CodecUnavailable { codec, reason: format!("libvpx has no {codec} {what}") })
	}
}

/// Version of the linked libvpx (`v1.14.0`).
pub fn libvpx_version() -> String {
	raw::version()
}

/// Speed levels (bigger is faster): VP8 `cpu-used` is minus the level (a
/// fixed speed), VP9's is the level (realtime mode).
struct Levels {
	slowest: i32,
	fastest: i32,
	/// Where adapting starts: fast, so the first seconds do not fall behind.
	start: i32,
}

// VP8 levels above 10 are no faster on screen content, only worse.
const VP8_LEVELS: Levels = Levels { slowest: 4, fastest: 10, start: 10 };
const VP9_LEVELS: Levels = Levels { slowest: 5, fastest: 9, start: 8 };

/// Adapts the speed level to how long frames take to encode compared to
/// the frame interval: faster above 75 % of it, slower (better quality)
/// below 35 %, deciding every 15 frames on a running mean of the encode
/// time (keyframes excluded: they are always slow).
pub struct SpeedControl {
	level: i32,
	slowest: i32,
	fastest: i32,
	/// Mean encode time in seconds.
	mean: f64,
	frames: u32,
}

impl SpeedControl {
	const WINDOW: u32 = 15;
	const TOO_SLOW: f64 = 0.75;
	const TOO_FAST: f64 = 0.35;

	fn new(levels: &Levels) -> Self {
		Self {
			level: levels.start,
			slowest: levels.slowest,
			fastest: levels.fastest,
			mean: 0.0,
			frames: 0,
		}
	}

	pub fn level(&self) -> i32 {
		self.level
	}

	/// Account one encode; returns the new level if it changed.
	pub fn update(&mut self, took: Duration, interval: Duration) -> Option<i32> {
		let took = took.as_secs_f64();
		self.mean = if self.frames == 0 { took } else { self.mean * 0.8 + took * 0.2 };
		self.frames += 1;
		if self.frames < Self::WINDOW || interval.is_zero() {
			return None;
		}
		let load = self.mean / interval.as_secs_f64();
		let level = if load > Self::TOO_SLOW {
			(self.level + 1).min(self.fastest)
		} else if load < Self::TOO_FAST {
			(self.level - 1).max(self.slowest)
		} else {
			self.level
		};
		if level == self.level {
			return None;
		}
		self.level = level;
		self.frames = 0;
		Some(level)
	}
}

/// libvpx VP8 / VP9 encoder.
pub struct VpxEncoder {
	codec: Codec,
	config: EncoderConfig,
	encoder: Option<raw::Encoder>,
	last_pts: Option<i64>,
	speed: SpeedControl,
}

impl VpxEncoder {
	pub fn new(codec: Codec, config: EncoderConfig) -> Result<Self> {
		check(codec, true)?;
		let levels = if codec == Codec::Vp9 { &VP9_LEVELS } else { &VP8_LEVELS };
		Ok(Self { codec, config, encoder: None, last_pts: None, speed: SpeedControl::new(levels) })
	}

	/// The libvpx `cpu-used` value in use.
	pub fn cpu_used(&self) -> i32 {
		match (self.config.speed, self.codec) {
			(Some(speed), _) => speed,
			(None, Codec::Vp9) => self.speed.level(),
			(None, _) => -self.speed.level(),
		}
	}

	fn error(&self, message: String) -> Error {
		Error::Encoder { codec: self.codec, message }
	}

	/// (Re)create the libvpx context for this frame size.
	fn ensure_encoder(&mut self, width: u32, height: u32) -> Result<&mut raw::Encoder> {
		if self.encoder.as_ref().is_none_or(|e| e.size() != (width, height)) {
			// Drop the old context first: only one lives at a time.
			self.encoder = None;
			let params = raw::EncoderParams {
				vp9: self.codec == Codec::Vp9,
				width,
				height,
				bitrate_kbps: self.config.bitrate_bps / 1000,
				keyframe_interval: self.config.keyframe_interval,
				screen: self.config.content == ContentHint::Screen,
				threads: self.config.threads_for(width, height),
				speed: self.cpu_used(),
			};
			let encoder = raw::Encoder::new(&params).map_err(|e| self.error(e))?;
			self.encoder = Some(encoder);
		}
		Ok(self.encoder.as_mut().expect("encoder was just created"))
	}
}

impl VideoEncoder for VpxEncoder {
	fn codec(&self) -> Codec {
		self.codec
	}

	fn backend(&self) -> EncoderBackend {
		EncoderBackend::Libvpx
	}

	fn encode(&mut self, frame: &VideoFrame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
		let mut frames = Vec::new();
		self.encode_with(frame, force_keyframe, &mut |f| {
			frames.push(EncodedFrame {
				data: f.data.to_vec(),
				keyframe: f.keyframe,
				pts_90khz: f.pts_90khz,
			});
		})?;
		Ok(frames)
	}

	/// Encodes straight from the frame's planes (I420 frames are not
	/// copied) and hands out libvpx's own output buffer.
	fn encode_with(
		&mut self,
		frame: &VideoFrame,
		force_keyframe: bool,
		out: &mut dyn FnMut(EncodedChunk<'_>),
	) -> Result<()> {
		let i420 = convert::to_i420(frame)?;
		let FrameData::I420 { y, u, v } = &i420.data else {
			unreachable!("to_i420 returns I420");
		};
		// libvpx needs strictly increasing timestamps; the duration of a
		// frame is the time since the previous one (rate control follows a
		// variable frame rate).
		let interval = VIDEO_CLOCK_RATE / u64::from(self.config.fps.max(1));
		let mut pts = i420.pts_90khz() as i64;
		let duration = match self.last_pts {
			Some(last) if pts <= last => {
				pts = last + 1;
				1
			}
			Some(last) => ((pts - last) as u64).min(VIDEO_CLOCK_RATE),
			None => interval,
		};
		self.last_pts = Some(pts);
		let image = raw::I420 {
			width: i420.width,
			height: i420.height,
			y: &y.data,
			u: &u.data,
			v: &v.data,
			y_stride: y.stride,
			uv_stride: u.stride,
		};
		let codec = self.codec;
		self.ensure_encoder(i420.width, i420.height)?;
		let encoder = self.encoder.as_mut().expect("created above");
		let started = Instant::now();
		let mut keyframe = force_keyframe;
		encoder
			.encode(&image, pts, duration, force_keyframe, &mut |p| {
				keyframe |= p.keyframe;
				if !p.data.is_empty() {
					out(EncodedChunk {
						data: p.data,
						keyframe: p.keyframe,
						pts_90khz: p.pts as u64,
					});
				}
			})
			.map_err(|message| Error::Encoder { codec, message })?;
		if self.config.speed.is_none() && !keyframe {
			let budget = Duration::from_secs(1) / self.config.fps.max(1);
			if let Some(level) = self.speed.update(started.elapsed(), budget) {
				let cpu_used = if codec == Codec::Vp9 { level } else { -level };
				tracing::debug!(%codec, cpu_used, "encoder speed");
				encoder.set_speed(cpu_used).map_err(|message| Error::Encoder { codec, message })?;
			}
		}
		Ok(())
	}

	fn set_fps(&mut self, fps: u32) -> Result<()> {
		self.config.fps = fps.max(1);
		Ok(())
	}

	fn speed(&self) -> Option<i32> {
		Some(self.cpu_used())
	}

	fn set_bitrate(&mut self, bps: u32) -> Result<()> {
		self.config.bitrate_bps = bps;
		if let Some(encoder) = &mut self.encoder {
			encoder
				.set_bitrate(bps / 1000)
				.map_err(|message| Error::Encoder { codec: self.codec, message })?;
		}
		Ok(())
	}
}

/// libvpx VP8 / VP9 decoder.
pub struct VpxDecoder {
	codec: Codec,
	decoder: raw::Decoder,
}

impl VpxDecoder {
	pub fn new(codec: Codec) -> Result<Self> {
		let vp9 = is_vp9(codec)?;
		check(codec, false)?;
		let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4) as u32);
		let decoder =
			raw::Decoder::new(vp9, threads).map_err(|message| Error::Decoder { codec, message })?;
		Ok(Self { codec, decoder })
	}
}

impl VideoDecoder for VpxDecoder {
	fn codec(&self) -> Codec {
		self.codec
	}

	fn decode(&mut self, data: &[u8]) -> Result<Option<VideoFrame>> {
		let picture = self
			.decoder
			.decode(data)
			.map_err(|message| Error::Decoder { codec: self.codec, message })?;
		Ok(picture.map(|p| {
			let chroma = p.width.div_ceil(2) as usize;
			VideoFrame {
				width: p.width,
				height: p.height,
				timestamp: Duration::ZERO,
				data: FrameData::I420 {
					y: Plane::new(p.y, p.width as usize),
					u: Plane::new(p.u, chroma),
					v: Plane::new(p.v, chroma),
				},
			}
		}))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A gradient with a bright square that moves per frame.
	fn frame(n: u32, w: u32, h: u32) -> VideoFrame {
		let mut data = vec![0; (w * h * 4) as usize];
		for y in 0..h {
			for x in 0..w {
				let inside = (x + 64 - n * 4 % 64) % w < 48 && y > h / 4 && y < h / 2;
				let px = if inside {
					[40, 220, 250, 255]
				} else {
					[(x % 256) as u8, (y % 256) as u8, 90, 255]
				};
				let i = ((y * w + x) * 4) as usize;
				data[i..i + 4].copy_from_slice(&px);
			}
		}
		VideoFrame::from_bgra(w, h, (w * 4) as usize, data)
			.unwrap()
			.with_timestamp(Duration::from_millis(u64::from(n) * 33))
	}

	fn roundtrip(codec: Codec) {
		let config = EncoderConfig { fps: 30, bitrate_bps: 1_500_000, ..EncoderConfig::default() };
		let mut encoder = VpxEncoder::new(codec, config).unwrap();
		let mut decoder = VpxDecoder::new(codec).unwrap();
		let mut decoded = 0;
		for n in 0..12 {
			let src = frame(n, 160, 120);
			let out = encoder.encode(&src, n == 8).unwrap();
			assert_eq!(out.len(), 1, "frame {n}");
			assert_eq!(out[0].keyframe, n == 0 || n == 8, "frame {n}");
			assert_eq!(out[0].pts_90khz, src.pts_90khz());
			let picture = decoder.decode(&out[0].data).unwrap().expect("a picture");
			assert_eq!((picture.width, picture.height), (160, 120));
			let psnr = convert::psnr(&src, &picture).unwrap();
			assert!(psnr > 28.0, "{codec} frame {n}: PSNR {psnr:.1} dB");
			decoded += 1;
		}
		assert_eq!(decoded, 12);
	}

	#[test]
	fn vp8_roundtrip() {
		roundtrip(Codec::Vp8);
	}

	#[test]
	fn vp9_roundtrip() {
		roundtrip(Codec::Vp9);
	}

	#[test]
	fn resolution_change_restarts_with_keyframe() {
		let mut encoder = VpxEncoder::new(Codec::Vp8, EncoderConfig::default()).unwrap();
		let mut decoder = VpxDecoder::new(Codec::Vp8).unwrap();
		for (n, (w, h)) in [(64, 48), (64, 48), (97, 55)].into_iter().enumerate() {
			let out = encoder.encode(&frame(n as u32, w, h), false).unwrap();
			assert_eq!(out[0].keyframe, n != 1);
			let picture = decoder.decode(&out[0].data).unwrap().unwrap();
			assert_eq!((picture.width, picture.height), (w, h));
		}
		encoder.set_bitrate(300_000).unwrap();
		assert!(!encoder.encode(&frame(3, 97, 55), false).unwrap()[0].keyframe);
	}

	#[test]
	fn speed_follows_the_encode_time() {
		let mut control = SpeedControl::new(&VP8_LEVELS);
		let interval = Duration::from_millis(33);
		// Fast encodes: slower levels (better quality) down to the slowest.
		let mut changes = Vec::new();
		for _ in 0..500 {
			changes.extend(control.update(Duration::from_millis(2), interval));
		}
		assert_eq!(control.level(), VP8_LEVELS.slowest);
		assert!(changes.windows(2).all(|w| w[1] == w[0] - 1), "{changes:?}");
		// Slow encodes: faster again.
		for _ in 0..500 {
			control.update(Duration::from_millis(30), interval);
		}
		assert_eq!(control.level(), VP8_LEVELS.fastest);
		// In between: stays.
		let level = control.level();
		for _ in 0..100 {
			assert!(
				control.update(Duration::from_millis(18), interval).is_none()
					|| level != control.level()
			);
		}
	}

	#[test]
	fn borrowed_output_and_fixed_speed() {
		let config = EncoderConfig { speed: Some(-6), ..EncoderConfig::default() };
		let mut encoder = VpxEncoder::new(Codec::Vp8, config).unwrap();
		assert_eq!(encoder.speed(), Some(-6));
		let mut sizes = Vec::new();
		for n in 0..3 {
			encoder
				.encode_with(&frame(n, 64, 48), false, &mut |f| {
					sizes.push((f.data.len(), f.keyframe))
				})
				.unwrap();
		}
		assert_eq!(sizes.len(), 3);
		assert!(sizes[0].1 && !sizes[1].1);
		assert_eq!(VpxEncoder::new(Codec::Vp9, EncoderConfig::default()).unwrap().speed(), Some(8));
	}

	#[test]
	fn garbage_is_an_error() {
		let mut decoder = VpxDecoder::new(Codec::Vp8).unwrap();
		assert!(decoder.decode(&[0x42; 64]).is_err());
		assert!(decoder.decode(&[]).unwrap().is_none());
		assert!(VpxEncoder::new(Codec::H264, EncoderConfig::default()).is_err());
		assert!(libvpx_version().starts_with('v'));
	}
}
