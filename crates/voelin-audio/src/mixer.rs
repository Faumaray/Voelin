//! Incoming voice: per-client jitter buffer, decoder and mixer, with a
//! volume and mute per client, and everyone else dimmed while a priority
//! speaker talks.
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

use crate::pcm::FRAME_SAMPLES;

/// Loudest per-client volume: +12 dB.
pub const MAX_CLIENT_VOLUME: f32 = 4.0;
/// Most a priority speaker dims the others (dB).
const MAX_DIMM_DB: f32 = -60.0;
/// The dimming for a priority speaker fades in and out over this many
/// samples (5 frames, 100 ms) instead of jumping.
const DIMM_RAMP_SAMPLES: usize = 5 * FRAME_SAMPLES;

/// Jitter buffer, decoder and mixer for incoming voice, keyed by sender.
/// Output is interleaved stereo at 48 kHz.
pub struct Mixer {
	handler: AudioHandler<ClientId>,
	volumes: HashMap<ClientId, f32>,
	muted: HashSet<ClientId>,
	/// While one of them talks, everyone else is dimmed.
	priority: HashSet<ClientId>,
	/// Linear gain of the others while a priority speaker talks (1: none).
	dimm: f32,
	/// How far the dimming has faded in (0..=1).
	fade: f32,
	/// The others' mix while the dimming fades (reused).
	others: Vec<f32>,
}

impl Default for Mixer {
	fn default() -> Self {
		Self {
			handler: AudioHandler::new(),
			volumes: HashMap::new(),
			muted: HashSet::new(),
			priority: HashSet::new(),
			dimm: 1.0,
			fade: 0.0,
			others: Vec::new(),
		}
	}
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
	/// (not cleared first), each scaled by its volume, and dimmed while a
	/// priority speaker talks. Returns the clients that stopped talking.
	pub fn fill_buffer(&mut self, buf: &mut [f32]) -> Vec<ClientId> {
		let queues = self.handler.get_mut_queues();
		// A priority speaker with a queue (talking or buffering) dims all
		// other queues, streams too, unless we muted it.
		let dim = queues.keys().any(|id| self.priority.contains(id) && !self.muted.contains(id));
		let frames = buf.len() / 2;
		let step = frames as f32 / DIMM_RAMP_SAMPLES as f32;
		let from = self.fade;
		self.fade = if dim { (from + step).min(1.0) } else { (from - step).max(0.0) };
		let to = self.fade;
		let dimm = self.dimm;
		let gain = |fade: f32| 1.0 + (dimm - 1.0) * fade;
		let fading = from != to;
		// While fading, the others are mixed into a buffer of their own and
		// ramped sample by sample; the priority speakers go straight to `buf`.
		let mut priority = Vec::new();
		for (id, queue) in queues {
			// Muted talkers are still decoded so their queues stay in sync.
			let volume = if self.muted.contains(id) {
				0.0
			} else {
				self.volumes.get(id).copied().unwrap_or(1.0)
			};
			queue.volume = if self.priority.contains(id) {
				if fading {
					priority.push((*id, volume));
					0.0
				} else {
					volume
				}
			} else if fading {
				volume
			} else {
				volume * gain(to)
			};
		}
		if !fading {
			return self.handler.fill_buffer(buf);
		}
		self.others.clear();
		self.others.resize(buf.len(), 0.0);
		let stopped = self.handler.fill_buffer_with_proc(&mut self.others, |id, samples| {
			if let Some(&(_, volume)) = priority.iter().find(|(p, _)| p == id) {
				for (out, s) in buf.iter_mut().zip(samples) {
					*out += s * volume;
				}
			}
		});
		// From the last frame's gain to this one's.
		for (i, (out, s)) in buf.chunks_exact_mut(2).zip(self.others.chunks_exact(2)).enumerate() {
			let g = gain(from + (to - from) * (i + 1) as f32 / frames as f32);
			out[0] += s[0] * g;
			out[1] += s[1] * g;
		}
		stopped
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

	/// The priority speakers, and how much everyone else is dimmed while
	/// one of them talks: dB, clamped to -60..=0 (NaN: not at all).
	/// Replaces the previous set; [`forget`](Self::forget) leaves it alone,
	/// the caller sends a new set when the clients change.
	pub fn set_priority(&mut self, clients: impl IntoIterator<Item = ClientId>, dimm_db: f32) {
		self.priority = clients.into_iter().collect();
		let db = if dimm_db.is_nan() { 0.0 } else { dimm_db.clamp(MAX_DIMM_DB, 0.0) };
		self.dimm = 10f32.powf(db / 20.0);
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
			.field("priority", &self.priority)
			.field("dimm", &self.dimm)
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

	/// A priority speaker dims the other talker by the server's -18 dB, fading
	/// in and out, but not itself, and not while we muted it.
	#[test]
	fn priority_speaker_dims_others() {
		let (a, p) = (ClientId(3), ClientId(5));
		let mut enc_a = VoiceEncoder::new(VoiceCodec::Voice).unwrap();
		let mut enc_p = VoiceEncoder::new(VoiceCodec::Voice).unwrap();
		let tone_a = pcm::sine(500.0, 4.0, 0.2);
		let tone_p = pcm::sine(1300.0, 4.0, 0.2);
		let frame = |tone: &[f32], i: u16| {
			let at = i as usize * FRAME_SAMPLES;
			tone[at..at + FRAME_SAMPLES].to_vec()
		};

		let mut mixer = Mixer::new();
		mixer.set_priority([p], -18.0);
		let (mut id_a, mut id_p) = (0u16, 0u16);
		// `frames` frames with a talking and p too if `with_p`; returns the
		// second half of the mix.
		let mut play = |mixer: &mut Mixer, frames: u16, with_p: bool| {
			let mut out = Vec::new();
			for _ in 0..frames {
				let fa = frame(&tone_a, id_a);
				mixer
					.handle_packet(a, packet(3, id_a, enc_a.encode_to_bytes(&fa).unwrap()))
					.unwrap();
				id_a += 1;
				if with_p {
					let fp = frame(&tone_p, id_p);
					let data = enc_p.encode_to_bytes(&fp).unwrap();
					mixer.handle_packet(p, packet(5, id_p, data)).unwrap();
					id_p += 1;
				}
				let mut buf = vec![0.0; FRAME_SAMPLES * 2];
				mixer.fill_buffer(&mut buf);
				out.extend(pcm::to_mono(&buf, 2));
			}
			out.split_off(out.len() / 2)
		};
		let power = |out: &[f32], f| pcm::goertzel_power(out, f, SAMPLE_RATE);
		let db = |level: f32, full: f32| 10.0 * (level / full).log10();

		let alone = play(&mut mixer, 30, false);
		let full = power(&alone, 500.0);
		assert!(full > 1e-4);
		// The dimming fades in over 5 frames: 1/5 of the way after the first.
		play(&mut mixer, 1, true);
		assert!((mixer.fade - 0.2).abs() < 1e-6, "fade {}", mixer.fade);
		let both = play(&mut mixer, 50, true);
		let dimmed = db(power(&both, 500.0), full);
		assert!((dimmed + 18.0).abs() < 1.0, "dimmed by {dimmed} dB");
		let own = db(power(&both, 1300.0), full);
		assert!(own.abs() < 1.0, "priority speaker at {own} dB");

		// p stops: its queue ends after three lost frames, and a is back.
		let after = play(&mut mixer, 40, false);
		assert_eq!(mixer.talkers().collect::<Vec<_>>(), vec![a]);
		assert_eq!(mixer.fade, 0.0);
		let back = db(power(&after, 500.0), full);
		assert!(back.abs() < 1.0, "after the priority speaker: {back} dB");

		// A priority speaker we muted dims nobody.
		mixer.set_muted(p, true);
		let muted = play(&mut mixer, 40, true);
		assert_eq!(mixer.talkers().count(), 2);
		let level = db(power(&muted, 500.0), full);
		assert!(level.abs() < 1.0, "muted priority speaker: {level} dB");
		assert!(power(&muted, 1300.0) < 1e-6);
	}

	/// The dimming fades in and out sample by sample, not in a step per
	/// frame: the other talker's level (per period of its tone) never jumps.
	#[test]
	fn priority_dimming_ramps_per_sample() {
		let (a, p) = (ClientId(3), ClientId(5));
		let mut enc_a = VoiceEncoder::new(VoiceCodec::Voice).unwrap();
		let mut enc_p = VoiceEncoder::new(VoiceCodec::Voice).unwrap();
		let tone = pcm::sine(500.0, 2.0, 0.2);
		let mut mixer = Mixer::new();
		mixer.set_priority([p], -18.0);
		// p is not heard (volume 0) but dims, so the mix is a's tone alone.
		mixer.set_volume(p, 0.0);
		let mut out = Vec::new();
		// p talks from frame 30 to 59.
		for (i, frame) in tone.chunks_exact(FRAME_SAMPLES).enumerate() {
			let id = i as u16;
			mixer.handle_packet(a, packet(3, id, enc_a.encode_to_bytes(frame).unwrap())).unwrap();
			if let Some(j) = i.checked_sub(30).filter(|j| *j < 30) {
				let data = enc_p.encode_to_bytes(frame).unwrap();
				mixer.handle_packet(p, packet(5, j as u16, data)).unwrap();
			}
			let mut buf = vec![0.0; FRAME_SAMPLES * 2];
			mixer.fill_buffer(&mut buf);
			out.extend(pcm::to_mono(&buf, 2));
		}
		assert_eq!(mixer.fade, 0.0);
		// The level per period of the 500 Hz tone (96 samples, 10 a frame).
		let level: Vec<f32> = out.chunks_exact(96).map(|c| pcm::energy(c).sqrt()).collect();
		let full = level[200..300].iter().sum::<f32>() / 100.0;
		let mean = |range: std::ops::Range<usize>| {
			level[range.clone()].iter().sum::<f32>() / range.len() as f32 / full
		};
		let jump = level[200..].windows(2).map(|w| (w[1] - w[0]).abs() / full).fold(0.0, f32::max);
		assert!(jump < 0.05, "the level jumps by {jump} of full");
		// Fully dimmed after 5 frames, and back after p's queue ended.
		let dimmed = mean(350..600);
		assert!((dimmed - 0.126).abs() < 0.02, "dimmed to {dimmed}");
		let back = mean(800..1000);
		assert!((back - 1.0).abs() < 0.05, "back to {back}");
	}

	#[test]
	fn priority_dimm_bounds() {
		let mut mixer = Mixer::new();
		assert_eq!(mixer.dimm, 1.0);
		mixer.set_priority([ClientId(1)], -18.0);
		assert!((mixer.dimm - 0.1259).abs() < 1e-3);
		mixer.set_priority([ClientId(1)], 6.0);
		assert_eq!(mixer.dimm, 1.0);
		mixer.set_priority([ClientId(1)], f32::NEG_INFINITY);
		assert!((mixer.dimm - 0.001).abs() < 1e-6);
		mixer.set_priority([ClientId(1)], f32::NAN);
		assert_eq!(mixer.dimm, 1.0);
		// Forgetting a client keeps the set: the caller replaces it.
		mixer.forget(ClientId(1));
		assert!(mixer.priority.contains(&ClientId(1)));
		mixer.set_priority([], -18.0);
		assert!(mixer.priority.is_empty());
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
