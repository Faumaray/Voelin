//! PCM constants and helpers.

use std::f32::consts::TAU;

/// The sample rate TeamSpeak's Opus codecs run at.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per channel in one 20 ms frame.
pub const FRAME_SAMPLES: usize = SAMPLE_RATE as usize / 50;

/// A sine tone, mono, at [`SAMPLE_RATE`].
pub fn sine(freq: f32, seconds: f32, amplitude: f32) -> Vec<f32> {
	let n = (seconds * SAMPLE_RATE as f32) as usize;
	(0..n).map(|i| amplitude * (TAU * freq * i as f32 / SAMPLE_RATE as f32).sin()).collect()
}

/// Deterministic white noise, uniform in `[-amplitude, amplitude]`, for tests
/// and calibration.
pub fn white_noise(n: usize, amplitude: f32, seed: u64) -> Vec<f32> {
	// PCG-style LCG; the top 24 bits are uniform enough.
	let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
	(0..n)
		.map(|_| {
			state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
			let x = (state >> 40) as f32 / (1u64 << 24) as f32;
			amplitude * (2.0 * x - 1.0)
		})
		.collect()
}

/// Average interleaved channels into mono.
pub fn to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
	if channels <= 1 {
		return interleaved.to_vec();
	}
	interleaved.chunks_exact(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect()
}

/// Duplicate mono samples into `channels` interleaved channels.
pub fn from_mono(mono: &[f32], channels: usize) -> Vec<f32> {
	mono.iter().flat_map(|&s| std::iter::repeat_n(s, channels)).collect()
}

/// Power of one frequency in `samples` (Goertzel algorithm), normalised so a
/// full-scale sine at `freq` gives about 0.25 per sample squared.
pub fn goertzel_power(samples: &[f32], freq: f32, rate: u32) -> f32 {
	if samples.is_empty() {
		return 0.0;
	}
	let coeff = 2.0 * (TAU * freq / rate as f32).cos();
	let (mut s1, mut s2) = (0.0f32, 0.0f32);
	for &x in samples {
		let s = x + coeff * s1 - s2;
		s2 = s1;
		s1 = s;
	}
	let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
	power / (samples.len() as f32 * samples.len() as f32)
}

/// Mean square of the signal.
pub fn energy(samples: &[f32]) -> f32 {
	if samples.is_empty() {
		return 0.0;
	}
	samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32
}

/// Fraction of the signal energy at `freq`: close to 1 for a clean tone,
/// close to 0 for silence or noise.
pub fn tone_ratio(samples: &[f32], freq: f32, rate: u32) -> f32 {
	let e = energy(samples);
	if e < 1e-9 {
		return 0.0;
	}
	// A sine of amplitude a has energy a²/2 and Goertzel power a²/4.
	(2.0 * goertzel_power(samples, freq, rate) / e).min(1.0)
}

/// Like [`tone_ratio`], but only over the 100 ms windows that are not silent,
/// so leading and trailing silence in a recording do not dilute the result.
pub fn voiced_tone_ratio(samples: &[f32], freq: f32, rate: u32) -> f32 {
	let window = rate as usize / 10;
	let ratios: Vec<f32> = samples
		.chunks_exact(window)
		.filter(|w| energy(w) > 1e-4)
		.map(|w| tone_ratio(w, freq, rate))
		.collect();
	if ratios.is_empty() { 0.0 } else { ratios.iter().sum::<f32>() / ratios.len() as f32 }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn detects_tone() {
		let tone = sine(1000.0, 0.5, 0.5);
		assert!(tone_ratio(&tone, 1000.0, SAMPLE_RATE) > 0.95);
		assert!(tone_ratio(&tone, 440.0, SAMPLE_RATE) < 0.05);
		assert_eq!(tone_ratio(&[0.0; 100], 1000.0, SAMPLE_RATE), 0.0);

		let mut padded = vec![0.0; SAMPLE_RATE as usize];
		padded.extend(&tone);
		padded.extend(vec![0.0; SAMPLE_RATE as usize]);
		assert!(tone_ratio(&padded, 1000.0, SAMPLE_RATE) < 0.5);
		assert!(voiced_tone_ratio(&padded, 1000.0, SAMPLE_RATE) > 0.95);
		assert_eq!(voiced_tone_ratio(&[0.0; 48_000], 1000.0, SAMPLE_RATE), 0.0);
	}

	#[test]
	fn noise_is_white_and_deterministic() {
		let n = white_noise(48_000, 0.5, 1);
		assert_eq!(n, white_noise(48_000, 0.5, 1));
		assert_ne!(n, white_noise(48_000, 0.5, 2));
		assert!(n.iter().all(|s| s.abs() <= 0.5));
		// Uniform in ±a has energy a²/3.
		assert!((energy(&n) - 0.25 / 3.0).abs() < 0.005);
		assert!(tone_ratio(&n, 1000.0, SAMPLE_RATE) < 0.01);
	}

	#[test]
	fn channel_conversion() {
		assert_eq!(to_mono(&[1.0, 3.0, 2.0, 4.0], 2), vec![2.0, 3.0]);
		assert_eq!(from_mono(&[1.0, 2.0], 2), vec![1.0, 1.0, 2.0, 2.0]);
	}
}
