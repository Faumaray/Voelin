//! VP8 and VP9 through the system libvpx.
//!
//! Encoder settings follow libwebrtc's realtime configuration: one pass CBR,
//! no lag, error resilient, fixed high speed (`cpu-used` 8), keyframes on
//! request, and screen-content tuning for [`ContentHint::Screen`].

use std::time::Duration;

use crate::codec::{
	Codec, ContentHint, EncodedFrame, EncoderBackend, EncoderConfig, VideoDecoder, VideoEncoder,
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

/// libvpx VP8 / VP9 encoder.
pub struct VpxEncoder {
	codec: Codec,
	config: EncoderConfig,
	encoder: Option<raw::Encoder>,
	last_pts: Option<i64>,
}

impl VpxEncoder {
	pub fn new(codec: Codec, config: EncoderConfig) -> Result<Self> {
		check(codec, true)?;
		Ok(Self { codec, config, encoder: None, last_pts: None })
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
		let i420 = convert::to_i420(frame)?;
		let FrameData::I420 { y, u, v } = &i420.data else {
			unreachable!("to_i420 returns I420");
		};
		// libvpx needs strictly increasing timestamps.
		let mut pts = i420.pts_90khz() as i64;
		if let Some(last) = self.last_pts
			&& pts <= last
		{
			pts = last + 1;
		}
		self.last_pts = Some(pts);
		let duration = VIDEO_CLOCK_RATE / u64::from(self.config.fps.max(1));
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
		let encoder = self.ensure_encoder(i420.width, i420.height)?;
		let packets = encoder
			.encode(&image, pts, duration, force_keyframe)
			.map_err(|message| Error::Encoder { codec, message })?;
		Ok(packets
			.into_iter()
			.filter(|p| !p.data.is_empty())
			.map(|p| EncodedFrame { data: p.data, keyframe: p.keyframe, pts_90khz: p.pts as u64 })
			.collect())
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
	fn garbage_is_an_error() {
		let mut decoder = VpxDecoder::new(Codec::Vp8).unwrap();
		assert!(decoder.decode(&[0x42; 64]).is_err());
		assert!(decoder.decode(&[]).unwrap().is_none());
		assert!(VpxEncoder::new(Codec::H264, EncoderConfig::default()).is_err());
		assert!(libvpx_version().starts_with('v'));
	}
}
