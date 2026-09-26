//! AV1 decoding through the system libdav1d (`dav1d` crate).

use std::time::Duration;

use dav1d::{PixelLayout, PlanarImageComponent, Settings};

use crate::codec::{Codec, VideoDecoder};
use crate::frame::{FrameData, Plane, VideoFrame};
use crate::{Error, Result};

fn error(e: impl std::fmt::Display) -> Error {
	Error::Decoder { codec: Codec::Av1, message: e.to_string() }
}

/// dav1d decoder tuned for low latency (one frame of delay).
pub struct Dav1dDecoder {
	decoder: dav1d::Decoder,
}

impl Dav1dDecoder {
	pub fn new() -> Result<Self> {
		let mut settings = Settings::new();
		settings.set_max_frame_delay(1);
		settings.set_n_threads(
			std::thread::available_parallelism().map_or(1, |n| n.get().min(4) as u32),
		);
		let decoder = dav1d::Decoder::with_settings(&settings).map_err(|e| {
			Error::CodecUnavailable { codec: Codec::Av1, reason: format!("dav1d: {e}") }
		})?;
		Ok(Self { decoder })
	}

	fn drain(&mut self, last: &mut Option<dav1d::Picture>) -> Result<()> {
		loop {
			match self.decoder.get_picture() {
				Ok(p) => *last = Some(p),
				Err(e) if e.is_again() => return Ok(()),
				Err(e) => return Err(error(e)),
			}
		}
	}
}

impl VideoDecoder for Dav1dDecoder {
	fn codec(&self) -> Codec {
		Codec::Av1
	}

	fn decode(&mut self, data: &[u8]) -> Result<Option<VideoFrame>> {
		if data.is_empty() {
			return Ok(None);
		}
		let mut last = None;
		// `Again` from send: pictures must be taken out before the rest of the
		// data is accepted.
		let mut result = self.decoder.send_data(data.to_vec(), None, None, None);
		loop {
			match result {
				Ok(()) => break,
				Err(e) if e.is_again() => {
					self.drain(&mut last)?;
					result = self.decoder.send_pending_data();
				}
				Err(e) => return Err(error(e)),
			}
		}
		self.drain(&mut last)?;
		last.map(|p| to_frame(&p)).transpose()
	}
}

fn to_frame(picture: &dav1d::Picture) -> Result<VideoFrame> {
	if picture.bit_depth() != 8 || picture.pixel_layout() != PixelLayout::I420 {
		return Err(error(format!(
			"unsupported picture format {:?} {} bit",
			picture.pixel_layout(),
			picture.bit_depth()
		)));
	}
	let (w, h) = (picture.width() as usize, picture.height() as usize);
	let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
	let copy = |component: PlanarImageComponent, width: usize, rows: usize| {
		let stride = picture.stride(component) as usize;
		let plane = picture.plane(component);
		let mut out = Vec::with_capacity(width * rows);
		for row in 0..rows {
			out.extend_from_slice(&plane[row * stride..row * stride + width]);
		}
		Plane::new(out, width)
	};
	Ok(VideoFrame {
		width: w as u32,
		height: h as u32,
		timestamp: Duration::ZERO,
		data: FrameData::I420 {
			y: copy(PlanarImageComponent::Y, w, h),
			u: copy(PlanarImageComponent::U, cw, ch),
			v: copy(PlanarImageComponent::V, cw, ch),
		},
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn decoder_constructs_and_rejects_garbage() {
		let mut decoder = Dav1dDecoder::new().unwrap();
		assert_eq!(decoder.codec(), Codec::Av1);
		assert!(decoder.decode(&[]).unwrap().is_none());
		// Not an OBU stream: an error or no picture, never a panic.
		if let Ok(p) = decoder.decode(&[0xff; 32]) {
			assert!(p.is_none());
		}
	}
}
