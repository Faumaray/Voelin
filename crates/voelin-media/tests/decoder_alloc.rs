//! FFmpeg's decoders allocate nothing per picture once running: every
//! decoder that passed its self-test (hardware ones included: the GPU's
//! pictures are copied through a frame of our own) decodes into the same
//! `VideoFrame` with `decode_into`, counted with a global allocator that
//! counts only on the thread that asks. FFmpeg's own buffers are malloc'd
//! by FFmpeg and pooled by it; they are not Rust allocations.
#![cfg(feature = "ffmpeg")]
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::Duration;

use voelin_media::capture::synthetic::SyntheticScreen;
use voelin_media::ffmpeg::{FfmpegDecoder, decoder};
use voelin_media::{Codecs, EncoderConfig, VideoDecoder, VideoFrame};

thread_local! {
	static COUNTING: Cell<bool> = const { Cell::new(false) };
	static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn count() {
	// `try_with`: never panics, also while the thread is torn down; const
	// thread locals without destructors never allocate.
	let _ = COUNTING.try_with(|on| {
		if on.get() {
			let _ = COUNT.try_with(|n| n.set(n.get() + 1));
		}
	});
}

struct Counting;

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged, so `System`'s guarantees hold; counting touches only
// thread-local cells and never allocates.
unsafe impl GlobalAlloc for Counting {
	unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
		count();
		// SAFETY: forwarded unchanged (see above).
		unsafe { System.alloc(layout) }
	}

	unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
		count();
		// SAFETY: forwarded unchanged.
		unsafe { System.alloc_zeroed(layout) }
	}

	unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
		count();
		// SAFETY: forwarded unchanged.
		unsafe { System.realloc(ptr, layout, new_size) }
	}

	unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
		// SAFETY: forwarded unchanged.
		unsafe { System.dealloc(ptr, layout) }
	}
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations `f` makes on this thread.
fn allocations(f: impl FnOnce()) -> u64 {
	COUNT.with(|n| n.set(0));
	COUNTING.with(|on| on.set(true));
	f();
	COUNTING.with(|on| on.set(false));
	COUNT.with(Cell::get)
}

#[test]
fn decoders_allocate_nothing_per_picture() {
	assert_eq!(allocations(|| drop(std::hint::black_box(vec![0u8; 16]))), 1);
	let codecs = Codecs::new();
	let screen = SyntheticScreen::new(640, 360);
	let mut tested = Vec::new();
	for status in decoder::probe().iter().filter(|s| s.available.is_ok()) {
		let codec = status.spec.codec;
		let config = EncoderConfig { fps: 30, bitrate_bps: 1_000_000, ..EncoderConfig::default() };
		let Ok(mut encoder) = codecs.new_encoder(codec, config) else { continue };
		let mut stream = Vec::new();
		for n in 0..40u64 {
			let frame = screen.frame(n, 30).with_timestamp(Duration::from_millis(33 * n));
			stream.extend(encoder.encode(&frame, n == 0).unwrap().into_iter().map(|f| f.data));
		}
		let mut decoder = FfmpegDecoder::new(status.spec).unwrap();
		let mut picture = VideoFrame::black_i420(0, 0);
		// The first pictures size the buffers.
		for data in &stream[..10] {
			decoder.decode_into(data, &mut picture).unwrap();
		}
		let mut decoded = 0;
		let counted = allocations(|| {
			for data in &stream[10..] {
				decoded += usize::from(decoder.decode_into(data, &mut picture).unwrap());
			}
		});
		eprintln!("{}: {counted} allocations for {decoded} pictures", status.spec.name);
		assert!(decoded + 3 >= stream.len() - 10, "{}: {decoded} pictures", status.spec.name);
		assert_eq!(counted, 0, "{}", status.spec.name);
		tested.push(status.spec.name);
	}
	eprintln!("tested: {tested:?}");
}
