//! The stream mixer allocates nothing per block: pushing into inputs and
//! mixing, with resampling, downmixing, drift correction, gain and mute
//! changes, stalls and restarts, counted with a global allocator that
//! counts only on the thread that asks.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use voelin_media::mix::{MixerConfig, StreamMixer};

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
fn mixing_allocates_nothing_per_block() {
	const BLOCK: usize = 960;
	// The counter works.
	assert_eq!(allocations(|| drop(std::hint::black_box(vec![0u8; 16]))), 1);
	let mut mixer = StreamMixer::new(MixerConfig::default());
	let handle = mixer.handle();
	// 48 kHz stereo, 44.1 kHz mono, 6 channels, and one that stalls.
	let stereo = handle.add_source("stereo");
	let mono = handle.add_source("mono");
	let surround = handle.add_source("5.1");
	let flaky = handle.add_source("flaky");
	let mut inputs = [stereo.input(48_000), mono.input(44_100), surround.input(48_000)];
	let mut flaky_input = flaky.input(48_000);
	let stereo_block: Vec<f32> = (0..BLOCK * 2).map(|i| (i as f32 * 0.01).sin() * 0.3).collect();
	let mono_block: Vec<f32> = (0..882).map(|i| (i as f32 * 0.02).sin() * 0.3).collect();
	let six_block: Vec<f32> = (0..BLOCK * 6).map(|i| (i as f32 * 0.03).sin() * 0.3).collect();
	let mut out = vec![0.0f32; BLOCK * 2];
	// Slightly fast clock on one input so the drift correction runs.
	let mut owed = 0.0f64;
	let mut step = |block: usize, out: &mut Vec<f32>| {
		inputs[0].push(&stereo_block, 2);
		inputs[1].push(&mono_block, 1);
		owed += BLOCK as f64 * 1.004;
		let frames = owed as usize;
		owed -= frames as f64;
		inputs[2].push(&six_block[..frames.min(BLOCK) * 6], 6);
		// On for a second, off for a second.
		if (block / 50).is_multiple_of(2) {
			flaky_input.push(&stereo_block, 2);
		}
		match block % 7 {
			0 => stereo.set_gain(0.5 + (block % 3) as f32 * 0.25),
			3 => mono.set_muted(block.is_multiple_of(2)),
			5 => handle.set_gain(0.9),
			_ => {}
		}
		mixer.mix(out);
		let _ = (stereo.level(), handle.level(), surround.state(), handle.limiter_gain());
	};
	// Warm up: sources adopted, inputs playing, latencies settled.
	for block in 0..300 {
		step(block, &mut out);
	}
	let count = allocations(|| {
		for block in 300..1300 {
			step(block, &mut out);
		}
	});
	assert_eq!(count, 0, "allocations in 1000 blocks");
	// The flaky source went idle and came back in the measured part.
	assert!(flaky.stats().underruns > 0);
	assert!(out.iter().any(|s| *s != 0.0));
}
