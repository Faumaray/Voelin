//! The stream mixer: `cargo bench -p voelin-media --bench mix`.
//!
//! One iteration pushes 20 ms into every source and mixes one 20 ms block
//! (960 frames), as the streamer does 50 times a second.
//!
//! - `mix/48k/<n>`: n stereo sources at 48 kHz (copied through)
//! - `mix/44k1/<n>`: n stereo sources at 44.1 kHz (cubic resampling)
//! - `mix/limited/8`: eight loud sources, the limiter working on every frame

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use voelin_media::mix::{MixerConfig, SourceInput, StreamMixer};

const BLOCK: usize = 960;

/// A mixer with `sources` sources at `rate`, each fed a 20 ms stereo block
/// per iteration.
struct Setup {
	mixer: StreamMixer,
	inputs: Vec<SourceInput>,
	/// 20 ms of input at the sources' rate.
	block: Vec<f32>,
	out: Vec<f32>,
}

impl Setup {
	fn new(sources: usize, rate: u32, amplitude: f32) -> Self {
		let mixer = StreamMixer::new(MixerConfig::default());
		let handle = mixer.handle();
		let inputs = (0..sources).map(|i| handle.add_source(format!("s{i}")).input(rate)).collect();
		let frames = rate as usize / 50;
		let block = (0..frames)
			.flat_map(|i| {
				let s = amplitude * (i as f32 * 0.05).sin();
				[s, -s]
			})
			.collect();
		let mut setup = Self { mixer, inputs, block, out: vec![0.0; BLOCK * 2] };
		// Every source past its latency and settled.
		for _ in 0..100 {
			setup.step();
		}
		setup
	}

	fn step(&mut self) {
		for input in &mut self.inputs {
			input.push(&self.block, 2);
		}
		self.mixer.mix(&mut self.out);
	}
}

fn mixing(c: &mut Criterion) {
	let mut group = c.benchmark_group("mix");
	group.sample_size(50).measurement_time(Duration::from_secs(3));
	for (name, rate) in [("48k", 48_000), ("44k1", 44_100)] {
		for sources in [1, 4, 16, 64] {
			let mut setup = Setup::new(sources, rate, 0.5 / sources as f32);
			group.throughput(Throughput::Elements((BLOCK * sources) as u64));
			group.bench_function(BenchmarkId::new(name, sources), |b| {
				b.iter(|| {
					setup.step();
					black_box(&setup.out);
				});
			});
		}
	}
	let mut setup = Setup::new(8, 48_000, 0.6);
	group.throughput(Throughput::Elements((BLOCK * 8) as u64));
	group.bench_function(BenchmarkId::new("limited", 8), |b| {
		b.iter(|| {
			setup.step();
			black_box(&setup.out);
		});
	});
	group.finish();
}

criterion_group!(benches, mixing);
criterion_main!(benches);
