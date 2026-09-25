//! Sample-rate conversion.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

use crate::{Error, Result};

/// Resample a whole interleaved clip from `from` Hz to `to` Hz.
pub fn resample_all(samples: &[f32], channels: usize, from: u32, to: u32) -> Result<Vec<f32>> {
	if from == to || samples.is_empty() {
		return Ok(samples.to_vec());
	}
	if channels == 0 || !samples.len().is_multiple_of(channels) {
		return Err(Error::Invalid(format!("{} samples for {channels} channels", samples.len())));
	}
	let frames = samples.len() / channels;
	let mut resampler =
		Fft::<f32>::new(from as usize, to as usize, 1024, channels, FixedSync::Input)
			.map_err(|e| Error::Resample(e.to_string()))?;
	let input = InterleavedSlice::new(samples, channels, frames)
		.map_err(|e| Error::Resample(e.to_string()))?;
	let output =
		resampler.process_all(&input, frames, None).map_err(|e| Error::Resample(e.to_string()))?;
	Ok(output.take_data())
}

/// Streaming mono resampler with linear interpolation, for device audio whose
/// rate differs from 48 kHz. Cheap and latency-free; good enough for voice.
#[derive(Clone, Debug)]
pub struct Linear {
	/// Input samples per output sample.
	step: f64,
	/// Position of the next output sample, relative to `prev`.
	pos: f64,
	prev: f32,
}

impl Linear {
	pub fn new(from: u32, to: u32) -> Self {
		Self { step: from as f64 / to as f64, pos: 0.0, prev: 0.0 }
	}

	pub fn is_passthrough(&self) -> bool {
		self.step == 1.0
	}

	/// Resample a chunk, appending to `out`. State carries over between calls.
	pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
		if self.is_passthrough() {
			out.extend_from_slice(input);
			return;
		}
		for &next in input {
			while self.pos < 1.0 {
				out.push(self.prev + (next - self.prev) * self.pos as f32);
				self.pos += self.step;
			}
			self.pos -= 1.0;
			self.prev = next;
		}
	}
}

#[cfg(test)]
mod tests {
	#[test]
	fn linear_streaming_rate_and_tone() {
		let tone: Vec<f32> = (0..44_100)
			.map(|i| 0.5 * (std::f32::consts::TAU * 1000.0 * i as f32 / 44_100.0).sin())
			.collect();
		let mut r = Linear::new(44_100, 48_000);
		let mut out = Vec::new();
		// Arbitrary chunk sizes must give the same stream.
		for chunk in tone.chunks(333) {
			r.process(chunk, &mut out);
		}
		assert!((out.len() as i64 - 48_000).abs() <= 2, "len {}", out.len());
		assert!(crate::pcm::tone_ratio(&out[100..], 1000.0, 48_000) > 0.95);
		let mut same = Linear::new(48_000, 48_000);
		let mut o = Vec::new();
		same.process(&[1.0, 2.0], &mut o);
		assert_eq!(o, vec![1.0, 2.0]);
	}

	use super::*;
	use crate::pcm;

	#[test]
	fn keeps_tone_across_rates() {
		// 1 kHz at 44.1 kHz, resampled to 48 kHz.
		let n = 44_100;
		let tone: Vec<f32> = (0..n)
			.map(|i| 0.5 * (std::f32::consts::TAU * 1000.0 * i as f32 / 44_100.0).sin())
			.collect();
		let out = resample_all(&tone, 1, 44_100, 48_000).unwrap();
		assert!((out.len() as i64 - 48_000).abs() < 100, "len {}", out.len());
		assert!(pcm::tone_ratio(&out, 1000.0, 48_000) > 0.95);
	}

	#[test]
	fn passthrough_and_errors() {
		assert_eq!(resample_all(&[1.0, 2.0], 1, 48_000, 48_000).unwrap(), vec![1.0, 2.0]);
		assert!(resample_all(&[1.0, 2.0, 3.0], 2, 44_100, 48_000).is_err());
	}
}
