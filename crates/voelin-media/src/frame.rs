//! Raw video frames and audio buffers.

use std::time::Duration;

use crate::{Error, Result};

/// Sample rate of all audio in this crate (and of Opus in streams).
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;

/// RTP clock rate of video.
pub const VIDEO_CLOCK_RATE: u64 = 90_000;

/// Pixel layout of a [`VideoFrame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
	/// 8-bit YUV 4:2:0 in three planes.
	I420,
	/// 8-bit YUV 4:2:0: a Y plane and an interleaved UV plane.
	Nv12,
	/// 4 bytes per pixel in B, G, R, A byte order (X11, Windows, PipeWire
	/// `BGRx`); alpha may be undefined.
	Bgra,
	/// 4 bytes per pixel in R, G, B, A byte order.
	Rgba,
}

/// One image plane: `stride` bytes per row, at least `stride * rows` bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plane {
	pub data: Vec<u8>,
	pub stride: usize,
}

impl Plane {
	pub fn new(data: Vec<u8>, stride: usize) -> Self {
		Self { data, stride }
	}

	/// A plane filled with `value`, without padding.
	pub fn filled(width: usize, rows: usize, value: u8) -> Self {
		Self { data: vec![value; width * rows], stride: width }
	}

	/// Row `y`, `width` bytes long.
	pub fn row(&self, y: usize, width: usize) -> &[u8] {
		&self.data[y * self.stride..y * self.stride + width]
	}

	/// The plane borrowed.
	pub fn view(&self) -> PlaneRef<'_> {
		PlaneRef { data: &self.data, stride: self.stride }
	}
}

/// A borrowed image plane: `stride` bytes per row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneRef<'a> {
	pub data: &'a [u8],
	pub stride: usize,
}

impl<'a> PlaneRef<'a> {
	pub fn new(data: &'a [u8], stride: usize) -> Self {
		Self { data, stride }
	}

	/// Row `y`, `width` bytes long.
	pub fn row(&self, y: usize, width: usize) -> &'a [u8] {
		&self.data[y * self.stride..y * self.stride + width]
	}

	fn check(&self, name: &str, width: usize, rows: usize) -> Result<()> {
		if self.stride < width {
			return Err(Error::InvalidFrame(format!(
				"{name} stride {} is smaller than its width {width}",
				self.stride
			)));
		}
		// The last row need not be padded.
		let needed = if rows == 0 { 0 } else { self.stride * (rows - 1) + width };
		if self.data.len() < needed {
			return Err(Error::InvalidFrame(format!(
				"{name} plane has {} bytes, needs {needed}",
				self.data.len()
			)));
		}
		Ok(())
	}

	/// A tightly packed copy of `width` x `rows` bytes.
	fn to_plane(self, width: usize, rows: usize) -> Plane {
		let mut data = Vec::with_capacity(width * rows);
		for y in 0..rows {
			data.extend_from_slice(self.row(y, width));
		}
		Plane::new(data, width)
	}
}

/// The pixels of a [`FrameRef`], borrowed (e.g. from a mapped capture
/// buffer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelsRef<'a> {
	I420 {
		y: PlaneRef<'a>,
		u: PlaneRef<'a>,
		v: PlaneRef<'a>,
	},
	Nv12 {
		y: PlaneRef<'a>,
		uv: PlaneRef<'a>,
	},
	/// BGRA or BGRx.
	Bgra(PlaneRef<'a>),
	/// RGBA or RGBx.
	Rgba(PlaneRef<'a>),
}

/// A video frame whose pixels are borrowed, e.g. a capture buffer that is
/// mapped only while a callback runs. Capture backends hand these to a
/// [`FrameSink`](crate::capture::FrameSink), which converts straight from
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameRef<'a> {
	pub width: u32,
	pub height: u32,
	/// As [`VideoFrame::timestamp`].
	pub timestamp: Duration,
	pub pixels: PixelsRef<'a>,
}

impl FrameRef<'_> {
	pub fn format(&self) -> PixelFormat {
		match self.pixels {
			PixelsRef::I420 { .. } => PixelFormat::I420,
			PixelsRef::Nv12 { .. } => PixelFormat::Nv12,
			PixelsRef::Bgra(_) => PixelFormat::Bgra,
			PixelsRef::Rgba(_) => PixelFormat::Rgba,
		}
	}

	/// Check that the planes are large enough for the frame size.
	pub fn validate(&self) -> Result<()> {
		if self.width == 0 || self.height == 0 {
			return Err(Error::InvalidFrame(format!("empty frame {}x{}", self.width, self.height)));
		}
		let (w, h) = (self.width as usize, self.height as usize);
		let (cw, ch) = chroma_size(self.width, self.height);
		match &self.pixels {
			PixelsRef::I420 { y, u, v } => {
				y.check("Y", w, h)?;
				u.check("U", cw, ch)?;
				v.check("V", cw, ch)
			}
			PixelsRef::Nv12 { y, uv } => {
				y.check("Y", w, h)?;
				uv.check("UV", cw * 2, ch)
			}
			PixelsRef::Bgra(p) => p.check("BGRA", w * 4, h),
			PixelsRef::Rgba(p) => p.check("RGBA", w * 4, h),
		}
	}

	/// An owned copy with tightly packed planes.
	pub fn to_frame(&self) -> VideoFrame {
		let (w, h) = (self.width as usize, self.height as usize);
		let (cw, ch) = chroma_size(self.width, self.height);
		let data = match self.pixels {
			PixelsRef::I420 { y, u, v } => FrameData::I420 {
				y: y.to_plane(w, h),
				u: u.to_plane(cw, ch),
				v: v.to_plane(cw, ch),
			},
			PixelsRef::Nv12 { y, uv } => {
				FrameData::Nv12 { y: y.to_plane(w, h), uv: uv.to_plane(cw * 2, ch) }
			}
			PixelsRef::Bgra(p) => FrameData::Bgra(p.to_plane(w * 4, h)),
			PixelsRef::Rgba(p) => FrameData::Rgba(p.to_plane(w * 4, h)),
		};
		VideoFrame { width: self.width, height: self.height, timestamp: self.timestamp, data }
	}
}

/// The pixels of a [`VideoFrame`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameData {
	I420 { y: Plane, u: Plane, v: Plane },
	Nv12 { y: Plane, uv: Plane },
	Bgra(Plane),
	Rgba(Plane),
}

/// An uncompressed video frame.
///
/// YUV frames use BT.601 limited range (what WebRTC endpoints assume unless
/// told otherwise); see [`crate::convert`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
	pub width: u32,
	pub height: u32,
	/// Capture or presentation time on a monotonic clock with an arbitrary
	/// origin (capture backends use the start of the capture). Decoders
	/// return frames with a zero timestamp; the caller knows the RTP time.
	pub timestamp: Duration,
	pub data: FrameData,
}

/// Size of the chroma planes of a 4:2:0 image.
pub fn chroma_size(width: u32, height: u32) -> (usize, usize) {
	(width.div_ceil(2) as usize, height.div_ceil(2) as usize)
}

impl VideoFrame {
	/// A black I420 frame.
	pub fn black_i420(width: u32, height: u32) -> Self {
		let (cw, ch) = chroma_size(width, height);
		let data = FrameData::I420 {
			y: Plane::filled(width as usize, height as usize, 16),
			u: Plane::filled(cw, ch, 128),
			v: Plane::filled(cw, ch, 128),
		};
		Self { width, height, timestamp: Duration::ZERO, data }
	}

	/// Wrap packed BGRA / BGRx pixels.
	pub fn from_bgra(width: u32, height: u32, stride: usize, data: Vec<u8>) -> Result<Self> {
		let frame = Self {
			width,
			height,
			timestamp: Duration::ZERO,
			data: FrameData::Bgra(Plane::new(data, stride)),
		};
		frame.validate()?;
		Ok(frame)
	}

	/// Wrap packed RGBA pixels.
	pub fn from_rgba(width: u32, height: u32, stride: usize, data: Vec<u8>) -> Result<Self> {
		let frame = Self {
			width,
			height,
			timestamp: Duration::ZERO,
			data: FrameData::Rgba(Plane::new(data, stride)),
		};
		frame.validate()?;
		Ok(frame)
	}

	pub fn with_timestamp(mut self, timestamp: Duration) -> Self {
		self.timestamp = timestamp;
		self
	}

	pub fn format(&self) -> PixelFormat {
		match self.data {
			FrameData::I420 { .. } => PixelFormat::I420,
			FrameData::Nv12 { .. } => PixelFormat::Nv12,
			FrameData::Bgra(_) => PixelFormat::Bgra,
			FrameData::Rgba(_) => PixelFormat::Rgba,
		}
	}

	/// The frame with its pixels borrowed.
	pub fn view(&self) -> FrameRef<'_> {
		let pixels = match &self.data {
			FrameData::I420 { y, u, v } => {
				PixelsRef::I420 { y: y.view(), u: u.view(), v: v.view() }
			}
			FrameData::Nv12 { y, uv } => PixelsRef::Nv12 { y: y.view(), uv: uv.view() },
			FrameData::Bgra(p) => PixelsRef::Bgra(p.view()),
			FrameData::Rgba(p) => PixelsRef::Rgba(p.view()),
		};
		FrameRef { width: self.width, height: self.height, timestamp: self.timestamp, pixels }
	}

	/// The timestamp on the 90 kHz RTP video clock.
	pub fn pts_90khz(&self) -> u64 {
		(self.timestamp.as_micros() * u128::from(VIDEO_CLOCK_RATE) / 1_000_000) as u64
	}

	/// Check that the planes are large enough for the frame size.
	pub fn validate(&self) -> Result<()> {
		self.view().validate()
	}
}

/// A picture in GPU memory: an NV12 VA-API surface, converted and scaled
/// from a captured DMA-BUF on the GPU (`ffmpeg::GpuConverter`, Linux) so
/// that no CPU reads its pixels, for the encoders that take one
/// ([`VideoEncoder::gpu_alignment`](crate::VideoEncoder::gpu_alignment)).
/// Dropping it gives the surface back to its pool. Only this crate makes
/// them.
#[non_exhaustive]
pub struct GpuFrame {
	pub width: u32,
	pub height: u32,
	/// As [`VideoFrame::timestamp`].
	pub timestamp: Duration,
	#[cfg(feature = "ffmpeg")]
	pub(crate) surface: crate::ffmpeg::Surface,
}

impl GpuFrame {
	/// The timestamp on the 90 kHz RTP video clock.
	pub fn pts_90khz(&self) -> u64 {
		(self.timestamp.as_micros() * u128::from(VIDEO_CLOCK_RATE) / 1_000_000) as u64
	}

	/// The same picture at another time (another reference to the same
	/// surface), e.g. to send a still screen again.
	pub fn at(&self, timestamp: Duration) -> Self {
		Self {
			width: self.width,
			height: self.height,
			timestamp,
			#[cfg(feature = "ffmpeg")]
			surface: self.surface.new_ref(),
		}
	}
}

impl std::fmt::Debug for GpuFrame {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "GpuFrame({}x{} at {:?})", self.width, self.height, self.timestamp)
	}
}

/// Interleaved 32-bit float audio at [`AUDIO_SAMPLE_RATE`].
#[derive(Clone, Debug, PartialEq)]
pub struct AudioBuffer {
	pub samples: Vec<f32>,
	pub channels: u16,
	/// Capture time on a monotonic clock with an arbitrary origin.
	pub timestamp: Duration,
}

impl AudioBuffer {
	/// Samples per channel.
	pub fn frames(&self) -> usize {
		self.samples.len() / usize::from(self.channels.max(1))
	}

	pub fn duration(&self) -> Duration {
		Duration::from_secs_f64(self.frames() as f64 / f64::from(AUDIO_SAMPLE_RATE))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn validate_checks_planes() {
		let frame = VideoFrame::black_i420(33, 17);
		frame.validate().unwrap();
		assert_eq!(chroma_size(33, 17), (17, 9));

		let short = VideoFrame::from_bgra(4, 4, 16, vec![0; 16 * 3 + 15]);
		assert!(matches!(short, Err(Error::InvalidFrame(_))));
		// The last row needs no padding.
		VideoFrame::from_bgra(4, 4, 20, vec![0; 20 * 3 + 16]).unwrap();
		assert!(VideoFrame::from_rgba(4, 4, 8, vec![0; 64]).is_err());
		assert!(VideoFrame::black_i420(0, 4).validate().is_err());
	}

	#[test]
	fn rtp_time() {
		let frame = VideoFrame::black_i420(2, 2).with_timestamp(Duration::from_millis(1500));
		assert_eq!(frame.pts_90khz(), 135_000);
		let audio = AudioBuffer { samples: vec![0.0; 960], channels: 2, timestamp: Duration::ZERO };
		assert_eq!(audio.frames(), 480);
		assert_eq!(audio.duration(), Duration::from_millis(10));
	}

	#[test]
	fn borrowed_frames() {
		// 3x2 BGRA with 4 bytes of padding per row, last row unpadded.
		let data: Vec<u8> = (0..16 + 12).collect();
		let frame = FrameRef {
			width: 3,
			height: 2,
			timestamp: Duration::from_millis(5),
			pixels: PixelsRef::Bgra(PlaneRef::new(&data, 16)),
		};
		frame.validate().unwrap();
		assert_eq!(frame.format(), PixelFormat::Bgra);
		let owned = frame.to_frame();
		let FrameData::Bgra(p) = &owned.data else { panic!("not BGRA") };
		assert_eq!(p.stride, 12);
		assert_eq!(p.data[..12], data[..12]);
		assert_eq!(p.data[12..], data[16..]);
		assert_eq!(owned.timestamp, Duration::from_millis(5));
		assert_eq!(owned.view().to_frame(), owned);
		let short = FrameRef { pixels: PixelsRef::Bgra(PlaneRef::new(&data[..27], 16)), ..frame };
		assert!(short.validate().is_err());
		assert_eq!(VideoFrame::black_i420(5, 3).view().format(), PixelFormat::I420);
	}
}
