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

#[cfg(test)]
mod tests {
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
