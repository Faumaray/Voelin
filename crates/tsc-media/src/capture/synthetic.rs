//! Synthetic sources for tests and `--synthetic` streaming: an animated test
//! pattern (a rectangle moving over a flat background, plus a frame counter)
//! and a sine tone.

use std::time::Duration;

use crate::capture::{
	AudioCapture, BoxFuture, CaptureOptions, CaptureSource, ScreenCapture, SourceId, Ticker, Worker,
};
use crate::frame::{AUDIO_SAMPLE_RATE, AudioBuffer, VideoFrame};
use crate::queue::{FrameReceiver, frame_channel};
use crate::{Error, Result};

/// Background colour of the pattern (RGB).
pub const BACKGROUND: [u8; 3] = [24, 40, 72];
/// Colour of the moving rectangle (RGB).
pub const RECT_COLOR: [u8; 3] = [240, 90, 30];
/// Colour of the frame counter (RGB).
pub const TEXT_COLOR: [u8; 3] = [255, 255, 255];

/// 3x5 bitmaps of the digits 0-9, one row per 3 bits.
const DIGITS: [[u8; 5]; 10] = [
	[7, 5, 5, 5, 7],
	[2, 6, 2, 2, 7],
	[7, 1, 7, 4, 7],
	[7, 1, 7, 1, 7],
	[5, 5, 7, 1, 1],
	[7, 4, 7, 1, 7],
	[7, 4, 7, 5, 7],
	[7, 1, 2, 2, 2],
	[7, 5, 7, 5, 7],
	[7, 5, 7, 1, 7],
];

/// The test pattern as a [`ScreenCapture`] backend.
pub struct SyntheticScreen {
	width: u32,
	height: u32,
	worker: Option<Worker>,
}

impl SyntheticScreen {
	pub fn new(width: u32, height: u32) -> Self {
		Self { width: width.max(16), height: height.max(16), worker: None }
	}

	/// The moving rectangle in frame `n`: `(x, y, width, height)`.
	pub fn rect(&self, n: u64) -> (u32, u32, u32, u32) {
		let (rw, rh) = ((self.width / 5).max(8), (self.height / 5).max(8));
		let range = u64::from(self.width - rw);
		let step = u64::from((self.width / 60).max(1));
		// Back and forth across the frame.
		let pos = (n * step) % (2 * range.max(1));
		let x = if pos > range { 2 * range - pos } else { pos };
		(x as u32, (self.height - rh) / 2, rw, rh)
	}

	/// Frame `n` as BGRA, stamped `n / fps` seconds.
	pub fn frame(&self, n: u64, fps: u32) -> VideoFrame {
		let (w, h) = (self.width as usize, self.height as usize);
		let mut data = vec![0; w * h * 4];
		let bgra = |c: [u8; 3]| [c[2], c[1], c[0], 255];
		for px in data.chunks_exact_mut(4) {
			px.copy_from_slice(&bgra(BACKGROUND));
		}
		let mut fill = |x0: usize, y0: usize, rw: usize, rh: usize, c: [u8; 3]| {
			for y in y0..(y0 + rh).min(h) {
				for x in x0..(x0 + rw).min(w) {
					data[(y * w + x) * 4..][..4].copy_from_slice(&bgra(c));
				}
			}
		};
		let (rx, ry, rw, rh) = self.rect(n);
		fill(rx as usize, ry as usize, rw as usize, rh as usize, RECT_COLOR);

		// Frame counter in the top-left corner.
		let scale = (h / 48).max(1);
		for (i, digit) in n.to_string().bytes().enumerate() {
			let bitmap = DIGITS[usize::from(digit - b'0')];
			let x0 = scale * 2 + i * 4 * scale;
			for (row, bits) in bitmap.iter().enumerate() {
				for col in 0..3 {
					if bits & (4 >> col) != 0 {
						let (x, y) = (x0 + col * scale, scale * 2 + row * scale);
						fill(x, y, scale, scale, TEXT_COLOR);
					}
				}
			}
		}
		let timestamp = Duration::from_secs(n) / fps.max(1);
		VideoFrame::from_bgra(self.width, self.height, w * 4, data)
			.expect("pattern buffer matches its size")
			.with_timestamp(timestamp)
	}
}

impl ScreenCapture for SyntheticScreen {
	fn backend(&self) -> &'static str {
		"synthetic"
	}

	fn sources(&mut self) -> Result<Vec<CaptureSource>> {
		Ok(vec![CaptureSource {
			id: SourceId::Synthetic,
			name: "Test pattern".into(),
			width: self.width,
			height: self.height,
			primary: true,
		}])
	}

	fn start(
		&mut self,
		source: &SourceId,
		options: &CaptureOptions,
	) -> BoxFuture<'_, Result<FrameReceiver<VideoFrame>>> {
		let source = source.clone();
		let options = options.clone();
		Box::pin(async move {
			if source != SourceId::Synthetic {
				return Err(Error::SourceNotFound(source));
			}
			self.stop();
			let (tx, rx) = frame_channel(options.queue);
			let pattern = SyntheticScreen::new(self.width, self.height);
			let fps = options.fps;
			self.worker = Some(Worker::spawn("tsc-synthetic-video", move |stop| {
				let mut ticker = Ticker::new(fps);
				let mut n = 0;
				while tx.send(pattern.frame(n, fps)) && ticker.wait(&stop) {
					n += 1;
				}
			})?);
			Ok(rx)
		})
	}

	fn stop(&mut self) {
		self.worker = None;
	}
}

/// A sine tone as an [`AudioCapture`] backend: 10 ms stereo buffers.
pub struct SineSource {
	frequency: f32,
	amplitude: f32,
	worker: Option<Worker>,
}

impl SineSource {
	/// Samples per channel in one buffer (10 ms).
	pub const BUFFER_FRAMES: usize = AUDIO_SAMPLE_RATE as usize / 100;

	pub fn new(frequency: f32, amplitude: f32) -> Self {
		Self { frequency, amplitude, worker: None }
	}

	/// Buffer `n` of the tone (continuous across buffers).
	pub fn buffer(&self, n: u64) -> AudioBuffer {
		let start = n * Self::BUFFER_FRAMES as u64;
		let mut samples = Vec::with_capacity(Self::BUFFER_FRAMES * 2);
		for i in 0..Self::BUFFER_FRAMES as u64 {
			let t = (start + i) as f64 / f64::from(AUDIO_SAMPLE_RATE);
			let s = f64::from(self.amplitude)
				* (2.0 * std::f64::consts::PI * f64::from(self.frequency) * t).sin();
			samples.extend([s as f32; 2]);
		}
		AudioBuffer { samples, channels: 2, timestamp: Duration::from_millis(n * 10) }
	}
}

impl AudioCapture for SineSource {
	fn backend(&self) -> &'static str {
		"synthetic"
	}

	fn start(&mut self) -> Result<FrameReceiver<AudioBuffer>> {
		self.stop();
		// Half a second of audio before the oldest is dropped.
		let (tx, rx) = frame_channel(50);
		let tone = SineSource::new(self.frequency, self.amplitude);
		self.worker = Some(Worker::spawn("tsc-synthetic-audio", move |stop| {
			let mut ticker = Ticker::new(100);
			let mut n = 0;
			while tx.send(tone.buffer(n)) && ticker.wait(&stop) {
				n += 1;
			}
		})?);
		Ok(rx)
	}

	fn stop(&mut self) {
		self.worker = None;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::convert;

	fn rgb_at(frame: &VideoFrame, x: u32, y: u32) -> [u8; 3] {
		let rgba = convert::to_rgba_vec(frame).unwrap();
		let i = ((y * frame.width + x) * 4) as usize;
		[rgba[i], rgba[i + 1], rgba[i + 2]]
	}

	#[test]
	fn pattern_moves_and_counts() {
		let screen = SyntheticScreen::new(320, 240);
		let (x0, y0, w, h) = screen.rect(0);
		let (x1, ..) = screen.rect(10);
		assert_ne!(x0, x1);
		let frame = screen.frame(10, 30);
		assert_eq!(frame.timestamp, Duration::from_secs(10) / 30);
		assert_eq!(rgb_at(&frame, x1 + w / 2, y0 + h / 2), RECT_COLOR);
		assert_eq!(rgb_at(&frame, 319, 239), BACKGROUND);
		// The "1" of "10" lights its middle column at the top.
		let scale = 240 / 48;
		assert_eq!(rgb_at(&frame, (scale * 2 + scale) as u32, (scale * 2) as u32), TEXT_COLOR);
		// The rectangle bounces within the frame.
		for n in 0..500 {
			let (x, _, w, _) = screen.rect(n);
			assert!(x + w <= 320);
		}
	}

	#[test]
	fn sine_is_continuous() {
		let tone = SineSource::new(1000.0, 0.5);
		let a = tone.buffer(0);
		let b = tone.buffer(1);
		assert_eq!(a.frames(), 480);
		let last = a.samples[a.samples.len() - 2];
		let next = b.samples[0];
		// Adjacent samples of a 1 kHz tone differ by at most 2*pi*f/rate*amp.
		assert!((last - next).abs() < 0.07, "{last} -> {next}");
		assert!(a.samples.iter().all(|s| s.abs() <= 0.5));
	}

	#[tokio::test]
	async fn capture_runs_until_stopped() {
		let mut screen = SyntheticScreen::new(64, 48);
		let options = CaptureOptions { fps: 50, ..CaptureOptions::default() };
		assert!(screen.start(&SourceId::Monitor(0), &options).await.is_err());
		let mut frames = screen.start(&SourceId::Synthetic, &options).await.unwrap();
		let first = frames.recv().await.unwrap();
		let second = frames.recv().await.unwrap();
		assert_eq!((first.width, first.height), (64, 48));
		assert!(second.timestamp > first.timestamp);
		screen.stop();
		// The worker is gone: the channel closes after the queued frames.
		while frames.recv().await.is_some() {}

		let mut tone = SineSource::new(440.0, 0.25);
		let mut audio = tone.start().unwrap();
		let buffer = audio.recv_timeout(Duration::from_secs(2)).unwrap();
		assert_eq!((buffer.channels, buffer.frames()), (2, 480));
		tone.stop();
	}
}
