//! Shared memory mapping for MIT-SHM captures (the only `unsafe` in X11
//! capture).
#![allow(unsafe_code)]

use std::fs::File;

use memmap2::MmapMut;

/// Map our memfd read-write.
pub(super) fn map(file: &File) -> std::io::Result<MmapMut> {
	// SAFETY: memmap2 needs the file not to be truncated, and not to be
	// written by others while Rust reads the mapping. The file is an
	// anonymous memfd we created and never resize after mapping; the only
	// other party is the X server, which writes into it while it executes a
	// ShmGetImage request. The capture loop waits for that request's reply
	// before reading, and issues no other request on the segment meanwhile.
	unsafe { MmapMut::map_mut(file) }
}
