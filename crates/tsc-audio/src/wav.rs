//! WAV file input and output (float samples in memory).

use std::path::Path;

use hound::{SampleFormat, WavReader, WavSpec, WavWriter};

use crate::Result;

/// Interleaved samples plus their format.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
	pub samples: Vec<f32>,
	pub channels: usize,
	pub rate: u32,
}

pub fn read(path: &Path) -> Result<Clip> {
	let mut reader = WavReader::open(path)?;
	let spec = reader.spec();
	let samples = match spec.sample_format {
		SampleFormat::Float => reader.samples::<f32>().collect::<std::result::Result<_, _>>()?,
		SampleFormat::Int => {
			let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
			reader
				.samples::<i32>()
				.map(|s| s.map(|s| s as f32 * scale))
				.collect::<std::result::Result<_, _>>()?
		}
	};
	Ok(Clip { samples, channels: spec.channels as usize, rate: spec.sample_rate })
}

/// Write 16-bit PCM.
pub fn write(path: &Path, clip: &Clip) -> Result<()> {
	let spec = WavSpec {
		channels: clip.channels as u16,
		sample_rate: clip.rate,
		bits_per_sample: 16,
		sample_format: SampleFormat::Int,
	};
	let mut writer = WavWriter::create(path, spec)?;
	for &s in &clip.samples {
		writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
	}
	writer.finalize()?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn roundtrip() {
		let path = std::env::temp_dir().join(format!("tsc-audio-{}.wav", std::process::id()));
		let clip = Clip { samples: vec![0.0, 0.5, -0.5, 0.25], channels: 2, rate: 48_000 };
		write(&path, &clip).unwrap();
		let back = read(&path).unwrap();
		std::fs::remove_file(&path).unwrap();
		assert_eq!((back.channels, back.rate), (2, 48_000));
		for (a, b) in clip.samples.iter().zip(&back.samples) {
			assert!((a - b).abs() < 1e-3);
		}
	}
}
