//! CPU access to LINEAR DMA-BUFs (screen capture buffers a compositor
//! shares): a read-only mapping, and `DMA_BUF_IOCTL_SYNC` around each read
//! so caches are coherent with what the GPU wrote. Tiled buffers cannot be
//! read like this; they need a GPU import (not done here).
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{BorrowedFd, RawFd};

use memmap2::{Mmap, MmapOptions};

/// `struct dma_buf_sync` of `<linux/dma-buf.h>`.
#[repr(C)]
struct DmaBufSync {
	flags: u64,
}

const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_START: u64 = 0 << 2;
const DMA_BUF_SYNC_END: u64 = 1 << 2;
/// `DMA_BUF_IOCTL_SYNC`: `_IOW('b', 0, struct dma_buf_sync)`.
const SYNC: rustix::ioctl::Opcode = rustix::ioctl::opcode::write::<DmaBufSync>(b'b', 0);

/// The size of DMA-BUF `fd` in bytes (`lseek(fd, 0, SEEK_END)`, which a
/// DMA-BUF answers with its buffer object's size without moving anything).
pub(crate) fn size_of(fd: RawFd) -> Option<usize> {
	if fd < 0 {
		return None;
	}
	// SAFETY: borrowed for the call from a caller that holds it open.
	let fd = unsafe { BorrowedFd::borrow_raw(fd) };
	let end = rustix::fs::seek(fd, rustix::fs::SeekFrom::End(0)).ok()?;
	usize::try_from(end).ok().filter(|&size| size > 0)
}

/// A read-only mapping of a DMA-BUF.
pub(crate) struct DmaBufMap {
	fd: RawFd,
	map: Mmap,
}

impl DmaBufMap {
	/// Map the first `len` bytes of the DMA-BUF `fd`. The caller keeps `fd`
	/// open while the mapping is used (the mapping itself keeps the buffer
	/// alive, but [`DmaBufMap::read`] syncs through the descriptor).
	pub fn new(fd: RawFd, len: usize) -> io::Result<Self> {
		if fd < 0 || len == 0 {
			return Err(io::Error::new(io::ErrorKind::InvalidInput, "no DMA-BUF to map"));
		}
		// SAFETY: memmap2 requires that the mapped object is not truncated
		// or changed by others while Rust reads it. A DMA-BUF cannot be
		// truncated; the compositor writes into a buffer only while it is
		// not handed to us (PipeWire queues it back after our callback), and
		// `read` brackets every access with DMA_BUF_IOCTL_SYNC so the CPU
		// sees the GPU's writes. We only read.
		let map = unsafe { MmapOptions::new().len(len).map(fd) }?;
		Ok(Self { fd, map })
	}

	pub fn fd(&self) -> RawFd {
		self.fd
	}

	pub fn len(&self) -> usize {
		self.map.len()
	}

	/// Run `f` on the mapped bytes between the start and end of a read
	/// access.
	pub fn read<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
		self.sync(DMA_BUF_SYNC_START);
		let result = f(&self.map);
		self.sync(DMA_BUF_SYNC_END);
		result
	}

	fn sync(&self, when: u64) {
		// SAFETY: `fd` is the open DMA-BUF this mapping was made from (see
		// `new`); borrowing it for the call does not close it.
		let fd = unsafe { BorrowedFd::borrow_raw(self.fd) };
		let sync = DmaBufSync { flags: DMA_BUF_SYNC_READ | when };
		// SAFETY: DMA_BUF_IOCTL_SYNC takes a pointer to a `dma_buf_sync`,
		// which is what `Setter` passes; it only reads it. A failure (e.g. a
		// kernel without the ioctl) leaves the data as coherent as the
		// mapping is anyway, so it is ignored.
		let _ = unsafe {
			rustix::ioctl::ioctl(fd, rustix::ioctl::Setter::<SYNC, DmaBufSync>::new(sync))
		};
	}
}

#[cfg(test)]
mod tests {
	use std::io::Write;
	use std::os::fd::AsRawFd;

	use super::*;

	/// A memfd maps like a DMA-BUF; the sync ioctl fails on it and is
	/// ignored.
	#[test]
	fn maps_and_reads() {
		let fd = rustix::fs::memfd_create("voelin-dmabuf-test", rustix::fs::MemfdFlags::CLOEXEC)
			.unwrap();
		let mut file = std::fs::File::from(fd);
		file.write_all(&[7; 8192]).unwrap();
		let map = DmaBufMap::new(file.as_raw_fd(), 8192).unwrap();
		assert_eq!(map.len(), 8192);
		assert_eq!(map.fd(), file.as_raw_fd());
		assert_eq!(map.read(|b| b.iter().map(|&x| u64::from(x)).sum::<u64>()), 7 * 8192);
		assert!(DmaBufMap::new(-1, 10).is_err());
		assert_eq!(size_of(file.as_raw_fd()), Some(8192));
		assert_eq!(size_of(-1), None);
	}
}
