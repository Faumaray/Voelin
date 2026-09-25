//! Audio thread: capture → encode → voice connection, and incoming voice →
//! jitter buffer/mixer → playback.
//!
//! Devices live on their own OS thread (cpal streams are not `Send` on every
//! platform); the connection task talks to it through channels.

use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, warn};
use tsc_audio::pcm::{self, FRAME_SAMPLES, SAMPLE_RATE};
use tsc_audio::resample::Linear;
use tsc_audio::{Framer, Mixer, VoiceCodec, VoiceEncoder};
use tsclientlib::ClientId;
use tsproto_packets::packets::{AudioData, InAudioBuf, OutPacket};

pub(crate) enum AudioIn {
	Packet(InAudioBuf),
	Transmit(bool),
	InputMuted(bool),
	OutputMuted(bool),
}

pub(crate) struct AudioHandle {
	tx: std_mpsc::Sender<AudioIn>,
}

impl AudioHandle {
	pub fn send(&self, msg: AudioIn) {
		let _ = self.tx.send(msg);
	}
}

/// Voice activity: transmit while the level is above the threshold, with a
/// short hang time so word endings are not cut.
struct Vad {
	threshold: f32,
	hang_frames: u32,
	remaining: u32,
}

impl Vad {
	fn active(&mut self, frame: &[f32]) -> bool {
		if pcm::energy(frame) > self.threshold {
			self.remaining = self.hang_frames;
		} else if self.remaining > 0 {
			self.remaining -= 1;
		}
		self.remaining > 0
	}
}

/// Start the audio thread. Encoded packets go to `outgoing`; problems (e.g.
/// no device) are reported through `errors`.
pub(crate) fn spawn(
	outgoing: mpsc::UnboundedSender<OutPacket>,
	errors: mpsc::UnboundedSender<String>,
	voice_activation: bool,
) -> AudioHandle {
	let (tx, rx) = std_mpsc::channel();
	thread::Builder::new()
		.name("tsc-audio".into())
		.spawn(move || run(rx, outgoing, errors, voice_activation))
		.expect("spawn audio thread");
	AudioHandle { tx }
}

fn run(
	rx: std_mpsc::Receiver<AudioIn>,
	outgoing: mpsc::UnboundedSender<OutPacket>,
	errors: mpsc::UnboundedSender<String>,
	voice_activation: bool,
) {
	#[cfg(feature = "audio-device")]
	let mut capture = match tsc_audio::device::Capture::open_default(500) {
		Ok(c) => Some(c),
		Err(e) => {
			let _ = errors.send(format!("no microphone: {e}"));
			None
		}
	};
	#[cfg(feature = "audio-device")]
	let mut playback = match tsc_audio::device::Playback::open_default(200) {
		Ok(p) => Some(p),
		Err(e) => {
			let _ = errors.send(format!("no speakers: {e}"));
			None
		}
	};
	#[cfg(not(feature = "audio-device"))]
	let _ = errors.send("built without audio device support".to_string());

	let mut encoder = match VoiceEncoder::new(VoiceCodec::Voice) {
		Ok(e) => e,
		Err(e) => {
			let _ = errors.send(e.to_string());
			return;
		}
	};
	let mut mixer = Mixer::new();
	let mut framer = Framer::new(FRAME_SAMPLES);
	let mut vad = Vad { threshold: 1e-4, hang_frames: 15, remaining: 0 };
	let (mut ptt, mut input_muted, mut output_muted) = (false, false, false);
	let mut was_sending = false;

	#[cfg(feature = "audio-device")]
	let mut capture_resampler = capture.as_ref().map(|c| Linear::new(c.rate, SAMPLE_RATE));
	#[cfg(feature = "audio-device")]
	let mut playback_resampler = playback.as_ref().map(|p| Linear::new(SAMPLE_RATE, p.rate));
	let mut captured = Vec::new();
	let mut resampled = Vec::new();

	loop {
		// Control messages and incoming voice.
		loop {
			match rx.try_recv() {
				Ok(AudioIn::Packet(packet)) => {
					let from = match packet.data().data() {
						AudioData::S2C { from, .. } | AudioData::S2CWhisper { from, .. } => *from,
						_ => continue,
					};
					if let Err(error) = mixer.handle_packet(ClientId(from), packet) {
						debug!(%error, "dropped voice packet");
					}
				}
				Ok(AudioIn::Transmit(on)) => ptt = on,
				Ok(AudioIn::InputMuted(m)) => input_muted = m,
				Ok(AudioIn::OutputMuted(m)) => output_muted = m,
				Err(std_mpsc::TryRecvError::Empty) => break,
				Err(std_mpsc::TryRecvError::Disconnected) => return,
			}
		}

		// Capture → 48 kHz mono → 20 ms frames → Opus.
		captured.clear();
		resampled.clear();
		#[cfg(feature = "audio-device")]
		if let (Some(c), Some(r)) = (&mut capture, &mut capture_resampler) {
			c.read_available(&mut captured);
			let mono = pcm::to_mono(&captured, c.channels);
			r.process(&mono, &mut resampled);
		}
		let mut frames = Vec::new();
		framer.push(&resampled, |f| frames.push(f.to_vec()));
		for frame in frames {
			let send = !input_muted && if voice_activation { vad.active(&frame) } else { ptt };
			if send {
				match encoder.encode(&frame) {
					Ok(packet) => {
						let _ = outgoing.send(packet);
					}
					Err(e) => warn!(%e, "encoding failed"),
				}
			} else if was_sending {
				let _ = outgoing.send(encoder.end_of_stream());
			}
			was_sending = send;
		}

		// Mixer → device rate/channels → playback.
		#[cfg(feature = "audio-device")]
		if let (Some(p), Some(r)) = (&mut playback, &mut playback_resampler) {
			let needed_48k = (p.free() / p.channels) as u64 * SAMPLE_RATE as u64 / p.rate as u64;
			if needed_48k as usize >= FRAME_SAMPLES {
				let mut stereo = vec![0.0; FRAME_SAMPLES * 2];
				mixer.fill_buffer(&mut stereo);
				if output_muted {
					stereo.fill(0.0);
				}
				let mono = pcm::to_mono(&stereo, 2);
				let mut out = Vec::with_capacity(mono.len());
				r.process(&mono, &mut out);
				p.write(&pcm::from_mono(&out, p.channels));
			}
		}
		#[cfg(not(feature = "audio-device"))]
		let _ = (output_muted, &mut mixer);

		thread::sleep(Duration::from_millis(5));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn vad_hangs_after_speech() {
		let mut vad = Vad { threshold: 1e-3, hang_frames: 2, remaining: 0 };
		let loud = vec![0.5; 960];
		let quiet = vec![0.0; 960];
		assert!(!vad.active(&quiet));
		assert!(vad.active(&loud));
		assert!(vad.active(&quiet));
		assert!(!vad.active(&quiet));
	}
}
