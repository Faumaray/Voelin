//! Voice activity detection for voice-activated transmission.
//!
//! A level gate with hangover, optionally combined with the RNN speech
//! detector of WebRTC's AGC2 (through sonora), which ignores noise such as
//! typing, fans or breathing that is loud enough to pass the level gate.
//! Works on 10 ms mono frames at 48 kHz, after [`Processor`](crate::process::Processor).

use serde::{Deserialize, Serialize};
use sonora_agc2::vad_wrapper::VoiceActivityDetectorWrapper;

use crate::pcm::SAMPLE_RATE;
use crate::process::PROCESS_FRAME;

/// What opens the gate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VadMode {
	/// The level is above the threshold.
	#[default]
	Level,
	/// The level is above the threshold and the frame sounds like speech.
	Speech,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VadSettings {
	pub mode: VadMode,
	/// Level in dBFS (RMS of a 10 ms frame; a full-scale sine is -3 dBFS)
	/// that opens the gate.
	pub threshold_db: f32,
	/// Keep transmitting this long after the last active frame, so word
	/// endings and short pauses are not cut.
	pub hangover_ms: u32,
	/// Speech probability (0 to 1) needed in [`VadMode::Speech`].
	pub speech_probability: f32,
}

impl Default for VadSettings {
	fn default() -> Self {
		// Speech after automatic gain control sits around -20 dBFS and the
		// controller keeps noise below -50 dBFS.
		Self {
			mode: VadMode::Level,
			threshold_db: -40.0,
			hangover_ms: 300,
			speech_probability: 0.6,
		}
	}
}

/// Level of silence, so the meter has a floor.
pub const SILENCE_DB: f32 = -100.0;

/// RMS level in dBFS.
pub fn level_db(samples: &[f32]) -> f32 {
	let e = crate::pcm::energy(samples);
	if e > 0.0 { (10.0 * e.log10()).max(SILENCE_DB) } else { SILENCE_DB }
}

/// Voice activity detector. See the [module docs](self).
pub struct Vad {
	settings: VadSettings,
	rnn: Option<VoiceActivityDetectorWrapper>,
	scaled: Vec<f32>,
	hang_remaining_ms: u32,
	level_db: f32,
	probability: f32,
}

impl Vad {
	pub fn new(settings: &VadSettings) -> Self {
		let mut vad = Self {
			settings: settings.clone(),
			rnn: None,
			scaled: vec![0.0; PROCESS_FRAME],
			hang_remaining_ms: 0,
			level_db: SILENCE_DB,
			probability: 0.0,
		};
		vad.apply(settings);
		vad
	}

	pub fn settings(&self) -> &VadSettings {
		&self.settings
	}

	pub fn apply(&mut self, settings: &VadSettings) {
		if settings.mode == VadMode::Speech && self.rnn.is_none() {
			self.rnn = Some(VoiceActivityDetectorWrapper::new(
				sonora_simd::detect_backend(),
				SAMPLE_RATE as i32,
			));
		} else if settings.mode != VadMode::Speech {
			self.rnn = None;
			self.probability = 0.0;
		}
		self.settings = settings.clone();
	}

	/// Analyse mono 48 kHz audio, a whole number of 10 ms frames (a
	/// trailing partial frame is ignored). Returns whether to transmit.
	pub fn process(&mut self, samples: &[f32]) -> bool {
		for frame in samples.chunks_exact(PROCESS_FRAME) {
			self.level_db = level_db(frame);
			let mut active = self.level_db >= self.settings.threshold_db;
			if let Some(rnn) = &mut self.rnn {
				// The detector works on 16-bit scale floats.
				for (dst, src) in self.scaled.iter_mut().zip(frame) {
					*dst = src * 32768.0;
				}
				self.probability = rnn.analyze(&self.scaled);
				active &= self.probability >= self.settings.speech_probability;
			}
			if active {
				// Plus this frame itself.
				self.hang_remaining_ms = self.settings.hangover_ms + 10;
			} else {
				self.hang_remaining_ms = self.hang_remaining_ms.saturating_sub(10);
			}
		}
		self.is_active()
	}

	/// Whether the gate is open after the last frame.
	pub fn is_active(&self) -> bool {
		self.hang_remaining_ms > 0
	}

	/// Level of the last frame in dBFS, for a microphone meter.
	pub fn level_db(&self) -> f32 {
		self.level_db
	}

	/// Speech probability of the last frame, 0 unless in [`VadMode::Speech`].
	pub fn speech_probability(&self) -> f32 {
		self.probability
	}

	/// Close the gate immediately (e.g. when switching to push-to-talk).
	pub fn reset(&mut self) {
		self.hang_remaining_ms = 0;
	}
}

impl std::fmt::Debug for Vad {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Vad")
			.field("settings", &self.settings)
			.field("level_db", &self.level_db)
			.field("active", &self.is_active())
			.finish_non_exhaustive()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::pcm::{self, white_noise as noise};

	/// A voiced sound: a 120 Hz pulse train through three vowel formants,
	/// scaled to `amplitude` peak.
	pub(crate) fn vowel(seconds: f32, amplitude: f32) -> Vec<f32> {
		let n = (seconds * SAMPLE_RATE as f32) as usize;
		let pulses: Vec<f32> =
			(0..n).map(|i| if i % (SAMPLE_RATE as usize / 120) == 0 { 1.0 } else { 0.0 }).collect();
		let resonate = |freq: f32, bandwidth: f32| -> Vec<f32> {
			let r = (-std::f32::consts::PI * bandwidth / SAMPLE_RATE as f32).exp();
			let c = 2.0 * r * (std::f32::consts::TAU * freq / SAMPLE_RATE as f32).cos();
			let (mut y1, mut y2) = (0.0f32, 0.0f32);
			pulses
				.iter()
				.map(|&x| {
					let y = x * (1.0 - r) + c * y1 - r * r * y2;
					(y2, y1) = (y1, y);
					y
				})
				.collect()
		};
		let (f1, f2, f3) = (resonate(700.0, 80.0), resonate(1200.0, 90.0), resonate(2600.0, 120.0));
		let mix: Vec<f32> = (0..n).map(|i| f1[i] + 0.6 * f2[i] + 0.3 * f3[i]).collect();
		let peak = mix.iter().fold(0.0f32, |m, s| m.max(s.abs()));
		mix.iter().map(|s| s / peak * amplitude).collect()
	}

	fn active_fraction(vad: &mut Vad, signal: &[f32]) -> f32 {
		let frames: Vec<bool> =
			signal.chunks_exact(PROCESS_FRAME).map(|f| vad.process(f)).collect();
		frames.iter().filter(|&&a| a).count() as f32 / frames.len() as f32
	}

	#[test]
	fn level() {
		assert_eq!(level_db(&[0.0; 480]), SILENCE_DB);
		assert!((level_db(&pcm::sine(1000.0, 0.01, 1.0)) + 3.01).abs() < 0.1);
		assert!((level_db(&[0.1; 480]) + 20.0).abs() < 0.01);
	}

	#[test]
	fn gate_and_hangover() {
		let settings = VadSettings { hangover_ms: 50, ..Default::default() };
		let mut vad = Vad::new(&settings);
		let loud = pcm::sine(300.0, 0.01, 0.1);
		let quiet = vec![0.001; PROCESS_FRAME];
		assert!(!vad.process(&quiet));
		assert!(vad.process(&loud));
		// 50 ms of hangover after the last loud frame, then closed.
		for _ in 0..5 {
			assert!(vad.process(&quiet));
		}
		assert!(!vad.process(&quiet));
		// A 20 ms frame is analysed as two 10 ms frames.
		assert!(vad.process(&[loud.clone(), loud].concat()));
		vad.reset();
		assert!(!vad.is_active());
	}

	#[test]
	fn threshold_is_respected() {
		let mut vad =
			Vad::new(&VadSettings { threshold_db: -30.0, hangover_ms: 0, ..Default::default() });
		// -33 dBFS stays closed, -23 dBFS opens.
		assert!(!vad.process(&pcm::sine(300.0, 0.01, 0.0316)));
		assert!(vad.process(&pcm::sine(300.0, 0.01, 0.1)));
	}

	/// In speech mode, noise loud enough for the level gate stays out, a
	/// voiced sound gets through.
	#[test]
	fn speech_mode_rejects_noise() {
		let settings = VadSettings { mode: VadMode::Speech, hangover_ms: 0, ..Default::default() };
		let hiss = noise(SAMPLE_RATE as usize * 2, 0.05, 3);
		assert!(level_db(&hiss) > settings.threshold_db);
		let mut level_only = Vad::new(&VadSettings { mode: VadMode::Level, ..settings.clone() });
		assert!(active_fraction(&mut level_only, &hiss) > 0.99);
		let mut vad = Vad::new(&settings);
		let noise_active = active_fraction(&mut vad, &hiss);
		assert!(noise_active < 0.1, "noise active {noise_active}");

		let mut vad = Vad::new(&settings);
		let voice_active = active_fraction(&mut vad, &vowel(2.0, 0.3));
		assert!(voice_active > 0.8, "voice active {voice_active}");
		assert!(vad.speech_probability() > 0.5);

		// Switching back to level mode drops the detector.
		vad.apply(&VadSettings::default());
		assert_eq!(vad.speech_probability(), 0.0);
	}
}
