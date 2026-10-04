//! A one-slot, latest-wins handoff between two threads.
//!
//! The producer [`put`](Handoff::put)s shared items (`Arc<T>`), replacing one
//! the consumer has not taken yet (a stale frame is worth nothing to an
//! encoder); the consumer [`take`](Handoff::take)s the newest. The slot is a
//! single atomic pointer, so neither side ever blocks the other or
//! allocates; a waiting consumer is parked and unparked.
#![allow(unsafe_code)]

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::Thread;
use std::time::{Duration, Instant};

/// See the [module docs](self).
pub struct Handoff<T> {
	/// Null, or an `Arc<T>` turned into a raw pointer.
	slot: AtomicPtr<T>,
	consumer: OnceLock<Thread>,
	closed: AtomicBool,
	replaced: AtomicU64,
}

// SAFETY: the slot owns an `Arc<T>`, which moves between the threads; that
// is what `Arc<T>: Send + Sync` allows.
unsafe impl<T: Send + Sync> Send for Handoff<T> {}
// SAFETY: as above; all shared access goes through atomics.
unsafe impl<T: Send + Sync> Sync for Handoff<T> {}

impl<T> Default for Handoff<T> {
	fn default() -> Self {
		Self {
			slot: AtomicPtr::new(ptr::null_mut()),
			consumer: OnceLock::new(),
			closed: AtomicBool::new(false),
			replaced: AtomicU64::new(0),
		}
	}
}

impl<T> Handoff<T> {
	pub fn new() -> Self {
		Self::default()
	}

	/// Offer `item`. Returns `true` if it replaced an item the consumer had
	/// not taken (counted in [`Handoff::replaced`]).
	pub fn put(&self, item: Arc<T>) -> bool {
		let new = Arc::into_raw(item).cast_mut();
		let old = self.slot.swap(new, Ordering::AcqRel);
		if let Some(consumer) = self.consumer.get() {
			consumer.unpark();
		}
		if old.is_null() {
			return false;
		}
		// SAFETY: a non-null slot value always comes from `Arc::into_raw`
		// in `put`, and the swap moved its ownership to us.
		drop(unsafe { Arc::from_raw(old) });
		self.replaced.fetch_add(1, Ordering::Relaxed);
		true
	}

	/// The newest item, if one was put since the last take.
	pub fn take(&self) -> Option<Arc<T>> {
		let item = self.slot.swap(ptr::null_mut(), Ordering::AcqRel);
		// SAFETY: as in `put`: the swap moved the `Arc` out of the slot.
		(!item.is_null()).then(|| unsafe { Arc::from_raw(item) })
	}

	/// Wait up to `timeout` for an item. The first thread that waits becomes
	/// the consumer that `put` wakes; there should be only one.
	pub fn wait_timeout(&self, timeout: Duration) -> Option<Arc<T>> {
		if let Some(item) = self.take() {
			return Some(item);
		}
		let current = std::thread::current();
		let parks = self.consumer.get_or_init(|| current.clone()).id() == current.id();
		let deadline = Instant::now() + timeout;
		loop {
			if let Some(item) = self.take() {
				return Some(item);
			}
			let now = Instant::now();
			if now >= deadline || self.is_closed() {
				return None;
			}
			if parks {
				std::thread::park_timeout(deadline - now);
			} else {
				// Not the registered consumer: poll.
				std::thread::sleep((deadline - now).min(Duration::from_millis(1)));
			}
		}
	}

	/// Make the calling thread the consumer that [`put`](Self::put) wakes
	/// (what the first [`wait_timeout`](Self::wait_timeout) does), for a
	/// thread that waits on several handoffs itself
	/// (`std::thread::park_timeout`, then [`take`](Self::take) from each).
	pub fn register(&self) {
		self.consumer.get_or_init(std::thread::current);
	}

	/// Wake the consumer and make waits return at once (items can still be
	/// taken).
	pub fn close(&self) {
		self.closed.store(true, Ordering::Release);
		if let Some(consumer) = self.consumer.get() {
			consumer.unpark();
		}
	}

	pub fn is_closed(&self) -> bool {
		self.closed.load(Ordering::Acquire)
	}

	/// Items dropped because a newer one replaced them before the consumer
	/// took them.
	pub fn replaced(&self) -> u64 {
		self.replaced.load(Ordering::Relaxed)
	}
}

impl<T> Drop for Handoff<T> {
	fn drop(&mut self) {
		drop(self.take());
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn newest_wins() {
		let handoff = Handoff::new();
		assert!(handoff.take().is_none());
		assert!(!handoff.put(Arc::new(1)));
		assert!(handoff.put(Arc::new(2)));
		assert_eq!(handoff.replaced(), 1);
		assert_eq!(handoff.take().as_deref(), Some(&2));
		assert!(handoff.take().is_none());
		// Items left in the slot are dropped with it.
		let item = Arc::new(3);
		handoff.put(item.clone());
		drop(handoff);
		assert_eq!(Arc::strong_count(&item), 1);
	}

	#[test]
	fn wakes_the_consumer() {
		let handoff = Arc::new(Handoff::new());
		let consumer = std::thread::spawn({
			let handoff = handoff.clone();
			move || {
				let mut got = Vec::new();
				while let Some(item) = handoff.wait_timeout(Duration::from_secs(5)) {
					got.push(*item);
					if *item == 99 {
						break;
					}
				}
				got
			}
		});
		for i in 0..100 {
			handoff.put(Arc::new(i));
			if i % 10 == 0 {
				std::thread::sleep(Duration::from_millis(1));
			}
		}
		let got = consumer.join().unwrap();
		assert_eq!(got.last(), Some(&99));
		assert!(got.windows(2).all(|w| w[0] < w[1]), "in order: {got:?}");
		assert_eq!(got.len() as u64 + handoff.replaced(), 100);
	}

	#[test]
	fn close_ends_waits() {
		let handoff = Arc::new(Handoff::<u8>::new());
		let waiter = std::thread::spawn({
			let handoff = handoff.clone();
			move || handoff.wait_timeout(Duration::from_secs(10))
		});
		std::thread::sleep(Duration::from_millis(20));
		let started = Instant::now();
		handoff.close();
		assert!(waiter.join().unwrap().is_none());
		assert!(started.elapsed() < Duration::from_secs(5));
	}
}
