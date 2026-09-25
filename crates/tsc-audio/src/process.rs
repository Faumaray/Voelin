//! Microphone processing: echo cancellation, noise suppression and automatic
//! gain control.
//!
//! Backed by [sonora](https://crates.io/crates/sonora) (BSD-3-Clause), a pure
//! Rust port of the WebRTC audio processing module (AEC3, NS, AGC2): no C++
//! toolchain or system library needed, so it builds the same on Linux,
//! Windows and Android.
//!
//! The [`Processor`] runs on 10 ms mono frames at 48 kHz. The far end, i.e.
//! exactly what goes to the speakers (the mixer output after per-client volume
//! and output mute), is fed through [`Processor::render`]; the microphone
//! through [`Processor::capture`]. Both accept any length and frame internally.

use serde::{Deserialize, Serialize};
use sonora::config::{
	AdaptiveDigital, CaptureLevelAdjustment, EchoCanceller, GainController2, HighPassFilter,
	MaxProcessingRate, NoiseSuppression, NoiseSuppressionLevel, Pipeline,
};
use sonora::{AudioProcessing, Config, StreamConfig};

use crate::Framer;
use crate::pcm::SAMPLE_RATE;

/// Samples in one 10 ms processing frame (mono, 48 kHz).
pub const PROCESS_FRAME: usize = SAMPLE_RATE as usize / 100;

/// How hard noise suppression works.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseLevel {
	/// About 6 dB.
	Low,
	/// About 12 dB.
	#[default]
	Moderate,
	/// About 18 dB.
	High,
	/// About 21 dB.
	VeryHigh,
}

impl From<NoiseLevel> for NoiseSuppressionLevel {
	fn from(level: NoiseLevel) -> Self {
		match level {
			NoiseLevel::Low => Self::Low,
			NoiseLevel::Moderate => Self::Moderate,
			NoiseLevel::High => Self::High,
			NoiseLevel::VeryHigh => Self::VeryHigh,
		}
	}
}

/// User settings for microphone processing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessingSettings {
	/// Remove what the speakers play from the microphone signal.
	pub echo_cancellation: bool,
	/// Remove steady background noise (fans, hum, hiss).
	pub noise_suppression: bool,
	pub noise_level: NoiseLevel,
	/// Bring speech to a steady level (adaptive digital gain and limiter).
	pub auto_gain: bool,
	/// Manual microphone gain in dB, applied before everything else.
	/// Clamped to ±[`MAX_INPUT_GAIN_DB`].
	pub input_gain_db: f32,
	/// Remove rumble below ~80 Hz. Always on while echo cancellation is.
	pub high_pass: bool,
}

/// Limit of [`ProcessingSettings::input_gain_db`].
pub const MAX_INPUT_GAIN_DB: f32 = 30.0;

impl Default for ProcessingSettings {
	fn default() -> Self {
		Self {
			echo_cancellation: true,
			noise_suppression: true,
			noise_level: NoiseLevel::Moderate,
			auto_gain: true,
			input_gain_db: 0.0,
			high_pass: true,
		}
	}
}

impl ProcessingSettings {
	/// Everything off: the processor passes audio through untouched.
	pub fn off() -> Self {
		Self {
			echo_cancellation: false,
			noise_suppression: false,
			noise_level: NoiseLevel::Moderate,
			auto_gain: false,
			input_gain_db: 0.0,
			high_pass: false,
		}
	}

	fn input_gain(&self) -> f32 {
		let db = if self.input_gain_db.is_finite() { self.input_gain_db } else { 0.0 };
		10f32.powf(db.clamp(-MAX_INPUT_GAIN_DB, MAX_INPUT_GAIN_DB) / 20.0)
	}

	fn is_passthrough(&self) -> bool {
		!self.echo_cancellation
			&& !self.noise_suppression
			&& !self.auto_gain
			&& !self.high_pass
			&& self.input_gain() == 1.0
	}

	fn to_config(&self) -> Config {
		let gain = self.input_gain();
		Config {
			pipeline: Pipeline {
				// Full band: voice is Opus at 48 kHz.
				maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz,
				..Default::default()
			},
			capture_level_adjustment: (gain != 1.0)
				.then(|| CaptureLevelAdjustment { pre_gain_factor: gain, ..Default::default() }),
			high_pass_filter: self.high_pass.then(HighPassFilter::default),
			echo_canceller: self.echo_cancellation.then(EchoCanceller::default),
			noise_suppression: self
				.noise_suppression
				.then(|| NoiseSuppression { level: self.noise_level.into(), ..Default::default() }),
			gain_controller2: self.auto_gain.then(|| GainController2 {
				adaptive_digital: Some(AdaptiveDigital::default()),
				..Default::default()
			}),
			..Default::default()
		}
	}
}

/// Echo canceller statistics, for diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProcessingStats {
	/// Echo return loss enhancement in dB: how much echo the canceller removes.
	pub erle_db: Option<f64>,
	/// Render-to-capture delay the canceller estimated.
	pub delay_ms: Option<i32>,
}

/// Microphone processing pipeline stage. See the [module docs](self).
pub struct Processor {
	apm: AudioProcessing,
	settings: ProcessingSettings,
	passthrough: bool,
	delay_ms: u32,
	render_framer: Framer,
	capture_framer: Framer,
	frame: Vec<f32>,
}

impl Processor {
	pub fn new(settings: &ProcessingSettings) -> Self {
		let stream = StreamConfig::new(SAMPLE_RATE, 1);
		let apm = AudioProcessing::builder()
			.config(settings.to_config())
			.capture_config(stream)
			.render_config(stream)
			.build();
		Self {
			apm,
			settings: settings.clone(),
			passthrough: settings.is_passthrough(),
			delay_ms: 0,
			render_framer: Framer::new(PROCESS_FRAME),
			capture_framer: Framer::new(PROCESS_FRAME),
			frame: vec![0.0; PROCESS_FRAME],
		}
	}

	pub fn settings(&self) -> &ProcessingSettings {
		&self.settings
	}

	/// Change settings. Only reconfigures when something changed, since that
	/// resets the adaptive filters.
	pub fn apply(&mut self, settings: &ProcessingSettings) {
		if *settings == self.settings {
			return;
		}
		self.apm.apply_config(settings.to_config());
		self.settings = settings.clone();
		self.passthrough = settings.is_passthrough();
	}

	/// Hint for the echo canceller: time from handing a frame to
	/// [`render`](Self::render) until it is heard, plus time from the
	/// microphone picking a sample up until it reaches
	/// [`capture`](Self::capture). The canceller estimates the rest itself.
	pub fn set_delay_ms(&mut self, delay_ms: u32) {
		if delay_ms != self.delay_ms {
			self.delay_ms = delay_ms;
			// Values above 500 ms are clamped, which is fine for a hint.
			let _ = self.apm.set_stream_delay_ms(delay_ms.min(500) as i32);
		}
	}

	/// Feed far-end audio: mono 48 kHz, exactly as played.
	pub fn render(&mut self, samples: &[f32]) {
		if !self.settings.echo_cancellation {
			return;
		}
		let Self { apm, render_framer, frame, .. } = self;
		render_framer.push(samples, |f| {
			// Render output is not used: no render-side processing is enabled.
			let _ = apm.process_render_f32(&[f], &mut [frame.as_mut_slice()]);
		});
	}

	/// Process microphone audio (mono 48 kHz), appending the result to `out`.
	/// Output comes in whole 10 ms frames, so up to 10 ms stay buffered.
	pub fn capture(&mut self, samples: &[f32], out: &mut Vec<f32>) {
		if self.passthrough && self.capture_framer.buffered() == 0 {
			out.extend_from_slice(samples);
			return;
		}
		let Self { apm, capture_framer, frame, passthrough, .. } = self;
		capture_framer.push(samples, |f| {
			if *passthrough {
				out.extend_from_slice(f);
			} else if apm.process_capture_f32(&[f], &mut [frame.as_mut_slice()]).is_ok() {
				out.extend_from_slice(frame);
			} else {
				out.extend_from_slice(f);
			}
		});
	}

	pub fn stats(&self) -> ProcessingStats {
		let stats = self.apm.statistics();
		ProcessingStats { erle_db: stats.echo_return_loss_enhancement, delay_ms: stats.delay_ms }
	}
}

impl std::fmt::Debug for Processor {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Processor").field("settings", &self.settings).finish_non_exhaustive()
	}
}

#[cfg(test)]
mod tests {
	use std::f32::consts::TAU;

	use super::*;
	use crate::pcm::{self, energy, white_noise as noise};

	fn db(ratio: f32) -> f32 {
		10.0 * ratio.max(1e-20).log10()
	}

	#[test]
	fn passthrough_when_off() {
		let mut p = Processor::new(&ProcessingSettings::off());
		let input = pcm::sine(440.0, 0.05, 0.3);
		let mut out = Vec::new();
		p.capture(&input[..100], &mut out);
		p.capture(&input[100..], &mut out);
		assert_eq!(out, input);
	}

	#[test]
	fn input_gain_is_clamped() {
		let s = ProcessingSettings { input_gain_db: 1000.0, ..ProcessingSettings::off() };
		assert!((s.input_gain() - 10f32.powf(1.5)).abs() < 1e-3);
		let s = ProcessingSettings { input_gain_db: f32::NAN, ..ProcessingSettings::off() };
		assert_eq!(s.input_gain(), 1.0);
		let s = ProcessingSettings { input_gain_db: 6.0, ..ProcessingSettings::off() };
		assert!(!s.is_passthrough());
	}

	#[test]
	fn settings_serde_defaults() {
		let s: ProcessingSettings = serde_json::from_str(r#"{"noise_level":"very_high"}"#).unwrap();
		assert_eq!(s.noise_level, NoiseLevel::VeryHigh);
		assert!(s.echo_cancellation);
	}

	/// The far end leaks into the microphone 40 ms later at -6 dB; after the
	/// canceller converged the echo must be at least 10 dB quieter.
	#[test]
	fn cancels_synthetic_echo() {
		let settings = ProcessingSettings {
			echo_cancellation: true,
			high_pass: true,
			..ProcessingSettings::off()
		};
		let mut p = Processor::new(&settings);
		p.set_delay_ms(40);
		let seconds = 6;
		let delay = 40 * PROCESS_FRAME / 10;
		// Speech-like far end: noise with a syllable-rate envelope.
		let far: Vec<f32> = noise(SAMPLE_RATE as usize * seconds, 0.4, 1)
			.into_iter()
			.enumerate()
			.map(|(i, s)| s * (0.6 + 0.4 * (i as f32 * 4.0 / SAMPLE_RATE as f32 * TAU).sin()))
			.collect();
		let mut near = vec![0.0; delay];
		near.extend(far.iter().map(|s| s * 0.5));
		near.truncate(far.len());

		let mut out = Vec::new();
		for (f, n) in far.chunks(PROCESS_FRAME).zip(near.chunks(PROCESS_FRAME)) {
			p.render(f);
			p.capture(n, &mut out);
		}
		assert_eq!(out.len(), near.len());
		let tail = SAMPLE_RATE as usize * 2;
		let before = energy(&near[near.len() - tail..]);
		let after = energy(&out[out.len() - tail..]);
		let attenuation = db(before / after);
		assert!(attenuation >= 10.0, "echo attenuated by {attenuation:.1} dB");
	}

	/// White noise gets quieter while a tone on top of it survives. The tone
	/// comes in 200 ms bursts like syllables: a steady tone is stationary and
	/// a noise suppressor rightly learns it as noise.
	#[test]
	fn suppresses_noise_keeps_tone() {
		let settings = ProcessingSettings {
			noise_suppression: true,
			noise_level: NoiseLevel::High,
			..ProcessingSettings::off()
		};
		let seconds = 4;
		let n = SAMPLE_RATE as usize * seconds;
		let hiss = noise(n, 0.05, 7);

		let mut p = Processor::new(&settings);
		let mut out = Vec::new();
		p.capture(&hiss, &mut out);
		let tail = SAMPLE_RATE as usize;
		let reduction = db(energy(&hiss[n - tail..]) / energy(&out[n - tail..]));
		assert!(reduction >= 10.0, "noise reduced by {reduction:.1} dB");

		let ms = |ms: usize| ms * SAMPLE_RATE as usize / 1000;
		let tone: Vec<f32> = pcm::sine(600.0, seconds as f32, 0.3)
			.into_iter()
			.enumerate()
			.map(|(i, s)| if i % ms(400) < ms(200) { s } else { 0.0 })
			.collect();
		let mixed: Vec<f32> = tone.iter().zip(&hiss).map(|(a, b)| a + b).collect();
		let mut p = Processor::new(&settings);
		let mut out = Vec::new();
		p.capture(&mixed, &mut out);
		// The last two seconds, away from the burst edges.
		for start in (n - ms(2000)..n).step_by(ms(400)) {
			let on = start + ms(40)..start + ms(160);
			let tone_in = pcm::goertzel_power(&tone[on.clone()], 600.0, SAMPLE_RATE);
			let tone_out = pcm::goertzel_power(&out[on], 600.0, SAMPLE_RATE);
			let loss = db(tone_in / tone_out);
			assert!(loss.abs() < 3.0, "tone changed by {loss:.1} dB");
			let off = start + ms(240)..start + ms(360);
			let reduction = db(energy(&hiss[off.clone()]) / energy(&out[off]));
			assert!(reduction >= 6.0, "noise between bursts reduced by {reduction:.1} dB");
		}
	}
}
