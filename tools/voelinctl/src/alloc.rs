//! The process's global allocator: the system allocator, counting
//! allocations (for `voelinctl stream bench`, which reports heap
//! allocations per frame). The count is one relaxed atomic add per
//! allocation.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

/// Allocations (including reallocations) and bytes requested so far.
pub fn counts() -> (u64, u64) {
	(ALLOCATIONS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed))
}

pub struct Counting;

fn count(size: usize) {
	ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
	BYTES.fetch_add(size as u64, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged, so `System`'s guarantees hold; counting touches only atomics
// and never allocates.
unsafe impl GlobalAlloc for Counting {
	unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
		count(layout.size());
		// SAFETY: forwarded unchanged (see above).
		unsafe { System.alloc(layout) }
	}

	unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
		count(layout.size());
		// SAFETY: forwarded unchanged.
		unsafe { System.alloc_zeroed(layout) }
	}

	unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
		count(new_size);
		// SAFETY: forwarded unchanged.
		unsafe { System.realloc(ptr, layout, new_size) }
	}

	unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
		// SAFETY: forwarded unchanged.
		unsafe { System.dealloc(ptr, layout) }
	}
}
