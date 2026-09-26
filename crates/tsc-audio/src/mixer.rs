//! Incoming voice: per-client jitter buffer, decoder and mixer, with a
//! volume and mute per client.
//!
//! The jitter buffer is the vendored `tsclientlib::audio::AudioHandler`. Per
//! sender it keeps a queue of Opus packets and adapts its length:
//!
//! - a new talker starts after a pre-buffer derived from the queue lengths of
//!   the other talkers (none for the first one);
//! - a queue that underruns plays Opus packet-loss concealment for the
//!   missing packet (one inserted frame) and drops the talker after three
//!   frames in a row without data, so a slow sender never lets latency grow;
//! - when the smallest queue length over the last 255 decoded packets exceeds
//!   its spread (a fast sender, or a jitter spike that passed), every 100th
//!   sample of the decoded frame is dropped (1 % faster playback) until the
//!   surplus is gone; above 0.5 s the queue is truncated outright.
//!
//! So sender clock drift is absorbed by occasional 1 % speed-ups (fast
//! sender) or concealment frames (slow sender) and latency stays bounded; the
//! `drift` integration test simulates an hour at ±200 ppm to check that.

use std::collections::{HashMap, HashSet};

use tsclientlib::ClientId;
use tsclientlib::audio::{AudioHandler, Error as QueueError};
use tsproto_packets::packets::InAudioBuf;

/// Loudest per-client volume: +12 dB.
pub const MAX_CLIENT_VOLUME: f32 = 4.0;

/// Jitter buffer, decoder and mixer for incoming voice, keyed by sender.
/// Output is interleaved stereo at 48 kHz.
#[derive(Default)]
pub struct Mixer {
	handler: AudioHandler<ClientId>,
	volumes: HashMap<ClientId, f32>,
	muted: HashSet<ClientId>,
}

impl Mixer {
	pub fn new() -> Self {
		Self::default()
	}

	/// Queue a voice packet from `id`. Returns `Some(id)` when this client
	/// just started talking. Late, duplicate and malformed packets are
	/// errors; on UDP they are expected and can be ignored.
	pub fn handle_packet(
		&mut self,
		id: ClientId,
		packet: InAudioBuf,
	) -> Result<Option<ClientId>, QueueError> {
		self.handler.handle_packet(id, packet)
	}

	/// Add the next `buf.len() / 2` stereo frames of all talkers to `buf`
	/// (not cleared first), each scaled by its volume. Returns the clients
	/// that stopped talking.
	pub fn fill_buffer(&mut self, buf: &mut [f32]) -> Vec<ClientId> {
		for (id, queue) in self.handler.get_mut_queues() {
			queue.volume = if self.muted.contains(id) {
				0.0
			} else {
				self.volumes.get(id).copied().unwrap_or(1.0)
			};
		}
		// Muted talkers are still decoded so their queues stay in sync.
		self.handler.fill_buffer(buf)
	}

	/// Linear gain for one client, clamped to `0..=`[`MAX_CLIENT_VOLUME`].
	/// Kept when the client stops and starts talking again.
	pub fn set_volume(&mut self, id: ClientId, volume: f32) {
		let volume = if volume.is_finite() { volume.clamp(0.0, MAX_CLIENT_VOLUME) } else { 1.0 };
		if volume == 1.0 {
			self.volumes.remove(&id);
		} else {
			self.volumes.insert(id, volume);
		}
	}

	/// Like [`set_volume`](Self::set_volume), in dB (0 dB = unchanged).
	pub fn set_volume_db(&mut self, id: ClientId, db: f32) {
		self.set_volume(id, 10f32.powf(db / 20.0));
	}

	pub fn volume(&self, id: ClientId) -> f32 {
		self.volumes.get(&id).copied().unwrap_or(1.0)
	}

	pub fn set_muted(&mut self, id: ClientId, muted: bool) {
		if muted {
			self.muted.insert(id);
		} else {
			self.muted.remove(&id);
		}
	}

	pub fn is_muted(&self, id: ClientId) -> bool {
		self.muted.contains(&id)
	}

	/// Forget volume and mute of a client (e.g. it left the server; client
	/// ids are reused).
	pub fn forget(&mut self, id: ClientId) {
		self.volumes.remove(&id);
		self.muted.remove(&id);
		self.handler.get_mut_queues().remove(&id);
	}

	/// Clients with a queue, i.e. currently talking or buffering.
	pub fn talkers(&self) -> impl Iterator<Item = ClientId> + '_ {
		self.handler.get_queues().keys().copied()
	}

	/// Drop all queues; volumes and mutes are kept.
	pub fn reset(&mut self) {
		self.handler.reset();
	}

	/// The underlying jitter buffer.
	pub fn handler(&self) -> &AudioHandler<ClientId> {
		&self.handler
	}
}

impl std::fmt::Debug for Mixer {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Mixer")
			.field("talkers", &self.handler.get_queues().len())
			.field("volumes", &self.volumes)
			.field("muted", &self.muted)
			.finish()
	}
}

#[cfg(test)]
mod tests {
	use tsproto_packets::packets::{AudioData, CodecType, Direction, OutAudio};

	use super::*;
	use crate::VoiceEncoder;
	use crate::encode::VoiceCodec;
	use crate::pcm::{self, FRAME_SAMPLES, SAMPLE_RATE};

	fn packet(from: u16, id: u16, data: &[u8]) -> InAudioBuf {
		let packet = OutAudio::new(&AudioData::S2C { id, codec: CodecType::OpusVoice, from, data });
		InAudioBuf::try_new(Direction::S2C, packet.into_vec()).unwrap()
	}

	/// Two clients send tones at the same level; after setting volumes the
	/// mix carries each at its own gain, and a muted one not at all.
	#[test]
	fn volume_per_client() {
		let (a, b) = (ClientId(3), ClientId(4));
		let mut enc_a = VoiceEncoder::new(VoiceCodec::Voice).unwrap();
		let mut enc_b = VoiceEncoder::new(VoiceCodec::Voice).unwrap();
		let tone_a = pcm::sine(500.0, 2.0, 0.2);
		let tone_b = pcm::sine(1300.0, 2.0, 0.2);

		let mut mixer = Mixer::new();
		mixer.set_volume(a, 0.25);
		mixer.set_volume_db(b, 6.0);
		assert!((mixer.volume(b) - 1.995).abs() < 0.01);
		let mut out = Vec::new();
		let frames = tone_a.chunks_exact(FRAME_SAMPLES).zip(tone_b.chunks_exact(FRAME_SAMPLES));
		for (i, (fa, fb)) in frames.enumerate() {
			let id = i as u16;
			let started = mixer.handle_packet(a, packet(3, id, enc_a.encode_to_bytes(fa).unwrap()));
			if i == 0 {
				assert_eq!(started.unwrap(), Some(a));
			}
			mixer.handle_packet(b, packet(4, id, enc_b.encode_to_bytes(fb).unwrap())).unwrap();
			let mut buf = vec![0.0; FRAME_SAMPLES * 2];
			mixer.fill_buffer(&mut buf);
			out.extend(pcm::to_mono(&buf, 2));
		}
		assert_eq!(mixer.talkers().count(), 2);
		let tail = &out[out.len() / 2..];
		let power = |f| pcm::goertzel_power(tail, f, SAMPLE_RATE);
		// Amplitude ratio 0.25 / 2 = 1/8, power ratio 1/64 (-18 dB).
		let ratio_db = 10.0 * (power(500.0) / power(1300.0)).log10();
		assert!((ratio_db + 18.0).abs() < 1.5, "ratio {ratio_db} dB");

		// Mute b: only a remains.
		mixer.set_muted(b, true);
		let mut out = Vec::new();
		for i in 0..20u16 {
			let id = 100 + i;
			let fa = &tone_a[..FRAME_SAMPLES];
			let fb = &tone_b[..FRAME_SAMPLES];
			let _ = mixer.handle_packet(a, packet(3, id, enc_a.encode_to_bytes(fa).unwrap()));
			let _ = mixer.handle_packet(b, packet(4, id, enc_b.encode_to_bytes(fb).unwrap()));
			let mut buf = vec![0.0; FRAME_SAMPLES * 2];
			mixer.fill_buffer(&mut buf);
			out.extend(pcm::to_mono(&buf, 2));
		}
		let tail = &out[out.len() / 2..];
		assert!(pcm::goertzel_power(tail, 1300.0, SAMPLE_RATE) < 1e-6);
		assert!(pcm::goertzel_power(tail, 500.0, SAMPLE_RATE) > 1e-4);
	}

	#[test]
	fn volume_bounds_and_forget() {
		let mut mixer = Mixer::new();
		let id = ClientId(9);
		mixer.set_volume(id, 100.0);
		assert_eq!(mixer.volume(id), MAX_CLIENT_VOLUME);
		mixer.set_volume(id, -1.0);
		assert_eq!(mixer.volume(id), 0.0);
		mixer.set_volume(id, f32::NAN);
		assert_eq!(mixer.volume(id), 1.0);
		mixer.set_volume(id, 0.5);
		mixer.set_muted(id, true);
		assert!(mixer.is_muted(id));
		mixer.forget(id);
		assert!(!mixer.is_muted(id));
		assert_eq!(mixer.volume(id), 1.0);
	}
}
