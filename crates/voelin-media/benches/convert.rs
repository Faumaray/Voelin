//! Pixel conversion and scaling of screen-sized frames:
//! `cargo bench -p voelin-media --bench convert`.
//!
//! - `to_i420`: the allocating one-thread conversion streams used before
//! - `converter/<n>`: [`Converter`] into a pooled frame on n threads (0: all)
//! - `pyramid`: conversion plus a half and a quarter size layer
//! - `scale/half`, `scale/2-3`: one luma plane, 2x2 box and area averaging

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use voelin_media::capture::synthetic::{Pattern, SyntheticScreen};
use voelin_media::convert::{self, Converter};
use voelin_media::pool::FramePool;
use voelin_media::scale::{PlaneScaler, Pyramid};
use voelin_media::workers::Workers;
use voelin_media::{FrameData, VideoFrame};

const SIZES: [(u32, u32); 3] = [(1280, 720), (1920, 1080), (2560, 1440)];

fn source(w: u32, h: u32) -> VideoFrame {
	SyntheticScreen::with_pattern(w, h, Pattern::Desktop).frame(1, 30)
}

fn conversion(c: &mut Criterion) {
	let mut group = c.benchmark_group("convert");
	group.sample_size(30).measurement_time(Duration::from_secs(3));
	for (w, h) in SIZES {
		let src = source(w, h);
		let size = format!("{w}x{h}");
		group.throughput(Throughput::Elements(u64::from(w) * u64::from(h)));
		group.bench_with_input(BenchmarkId::new("to_i420", &size), &src, |b, src| {
			b.iter(|| black_box(convert::to_i420(src).unwrap()));
		});
		for threads in [1, 0] {
			let mut converter = Converter::new(threads);
			let mut pool = FramePool::new();
			let id = BenchmarkId::new(format!("converter/{}", converter.threads()), &size);
			group.bench_with_input(id, &src, |b, src| {
				b.iter(|| {
					let target = Arc::get_mut(pool.get(w, h)).unwrap();
					converter.to_i420_into(&src.view(), target).unwrap();
				});
			});
		}
		let mut pyramid = Pyramid::new(0);
		let sizes = [(w, h), (w / 2, h / 2), (w / 4, h / 4)];
		let mut out = vec![None; 3];
		group.bench_with_input(BenchmarkId::new("pyramid/1+1/2+1/4", &size), &src, |b, src| {
			b.iter(|| {
				pyramid.process(&src.view(), &sizes, &[true; 3], &mut out).unwrap();
				out.iter_mut().for_each(|f| drop(f.take()));
			});
		});
	}
	group.finish();
}

fn scaling(c: &mut Criterion) {
	let mut group = c.benchmark_group("scale");
	group.sample_size(30).measurement_time(Duration::from_secs(3));
	let mut workers = Workers::new("bench-scale", 0);
	let i420 = convert::to_i420(&source(1920, 1080)).unwrap().into_owned();
	let FrameData::I420 { y, .. } = &i420.data else { unreachable!() };
	for (name, (dw, dh)) in [("half", (960, 540)), ("2-3", (1280, 720))] {
		let mut scaler = PlaneScaler::new((1920, 1080), (dw, dh));
		let mut out = vec![0; dw * dh];
		group.throughput(Throughput::Elements((dw * dh) as u64));
		group.bench_function(format!("{name}/1920x1080-luma"), |b| {
			b.iter(|| scaler.scale(&mut workers, y.view(), &mut out, dw).unwrap());
		});
	}
	group.finish();
}

criterion_group!(benches, conversion, scaling);
criterion_main!(benches);
