//! A one-hour call with a sender whose clock runs ±200 ppm off ours: the
//! jitter buffer must keep the latency bounded instead of drifting by the
//! 720 ms the clocks disagree over an hour.
//!
//! The sender transmits continuously (the worst case: talk spurts would reset
//! the queue) over a network with 20–60 ms of jitter, reordering and 0.5 %
//! loss. Every 30 s it sends a loud marker frame instead of the quiet one;
//! the latency is the time from generating a marker to it leaving the mixer.

use tsclientlib::ClientId;
use tsproto_packets::packets::{AudioData, CodecType, Direction, InAudioBuf, OutAudio};
use voelin_audio::pcm::{FRAME_SAMPLES, SAMPLE_RATE, energy, sine, white_noise};
use voelin_audio::{Mixer, VoiceCodec, VoiceEncoder};

/// The receiver pulls 10 ms at a time, like a playback loop.
const PULL: usize = SAMPLE_RATE as usize / 100;
const MARKER_EVERY: u64 = 1500;

struct Rng(u64);

impl Rng {
	fn next(&mut self) -> f64 {
		self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
		(self.0 >> 11) as f64 / (1u64 << 53) as f64
	}
}

/// Returns the latency of each marker in ms, in order.
fn simulate(ppm: f64, hours: f64) -> Vec<f64> {
	let mut encoder = VoiceEncoder::new(VoiceCodec::Voice).unwrap();
	// A quiet frame (-65 dBFS noise) and a loud marker, encoded once.
	let quiet = white_noise(FRAME_SAMPLES, 0.001, 1);
	for _ in 0..5 {
		encoder.encode_to_bytes(&quiet).unwrap();
	}
	let quiet = encoder.encode_to_bytes(&quiet).unwrap().to_vec();
	let marker = encoder.encode_to_bytes(&sine(1000.0, 0.02, 0.5)).unwrap().to_vec();

	let mut rng = Rng(ppm.to_bits());
	let mut mixer = Mixer::new();
	let client = ClientId(5);
	let total = (hours * 3600.0 * SAMPLE_RATE as f64) as u64;
	// Sender frame n is made at n * 20 ms of the sender's clock.
	let frame_time = |n: u64| n as f64 * FRAME_SAMPLES as f64 / (1.0 + ppm * 1e-6);

	// (arrival time, packet id), kept sorted by arrival.
	let mut in_flight: Vec<(f64, u64)> = Vec::new();
	let mut next_frame = 0u64;
	let mut markers_sent = Vec::new();
	let mut latencies = Vec::new();
	let mut out = vec![0.0f32; PULL * 2];
	let mut now = 0u64;
	while now < total {
		// Everything the sender produced up to now goes on the wire.
		while frame_time(next_frame) <= now as f64 {
			let is_marker = next_frame % MARKER_EVERY == MARKER_EVERY / 2;
			if is_marker {
				markers_sent.push(frame_time(next_frame));
			}
			// Markers are never lost, so each one can be matched.
			if is_marker || rng.next() >= 0.005 {
				let jitter = (0.02 + 0.04 * rng.next()) * SAMPLE_RATE as f64;
				in_flight.push((frame_time(next_frame) + jitter, next_frame));
			}
			next_frame += 1;
		}
		in_flight.sort_by(|a, b| a.0.total_cmp(&b.0));
		let arrived = in_flight.partition_point(|&(t, _)| t <= now as f64);
		for (_, n) in in_flight.drain(..arrived) {
			let data = if n % MARKER_EVERY == MARKER_EVERY / 2 { &marker } else { &quiet };
			let packet = OutAudio::new(&AudioData::S2C {
				id: n as u16,
				codec: CodecType::OpusVoice,
				from: 5,
				data,
			});
			let packet = InAudioBuf::try_new(Direction::S2C, packet.into_vec()).unwrap();
			// Late packets are dropped by the jitter buffer, as on a real network.
			let _ = mixer.handle_packet(client, packet);
		}

		out.fill(0.0);
		mixer.fill_buffer(&mut out);
		let left: Vec<f32> = out.iter().step_by(2).copied().collect();
		if energy(&left) > 1e-3 {
			// The latest marker made before now; they are 30 s apart.
			let sent = markers_sent.iter().rev().find(|&&t| t <= now as f64).copied();
			if let Some(sent) = sent {
				let latency = (now as f64 - sent) * 1000.0 / SAMPLE_RATE as f64;
				// One detection per marker (it spans two 10 ms pulls).
				if latencies.len() < markers_sent.len() && latency < 5000.0 {
					let index = markers_sent.iter().position(|&t| t == sent).unwrap();
					if index == latencies.len() {
						latencies.push(latency);
					}
				}
			}
		}
		now += PULL as u64;
	}
	assert_eq!(latencies.len(), markers_sent.len(), "every marker must come out (ppm {ppm})");
	latencies
}

fn check(ppm: f64) {
	let latencies = simulate(ppm, 1.0);
	assert_eq!(latencies.len(), 120);
	let max = latencies.iter().copied().fold(0.0, f64::max);
	let min = latencies.iter().copied().fold(f64::MAX, f64::min);
	let first: f64 = latencies[..20].iter().sum::<f64>() / 20.0;
	let last: f64 = latencies[latencies.len() - 20..].iter().sum::<f64>() / 20.0;
	println!(
		"{ppm:+} ppm: latency min {min:.0} ms, max {max:.0} ms, first 10 min avg {first:.0} ms, last 10 min avg {last:.0} ms"
	);
	// The network alone adds 20-60 ms; unchecked drift would add 720 ms.
	assert!(min >= 20.0, "latency {min} ms is below the network delay");
	assert!(max < 250.0, "latency grew to {max} ms");
	assert!((last - first).abs() < 60.0, "latency drifted from {first} ms to {last} ms");
}

#[test]
fn fast_sender_one_hour() {
	check(200.0);
}

#[test]
fn slow_sender_one_hour() {
	check(-200.0);
}
