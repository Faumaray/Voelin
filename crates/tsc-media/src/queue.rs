//! Bounded channel from capture threads to consumers.
//!
//! When the consumer falls behind, the oldest item is dropped: for screen
//! capture the newest frame is the one worth encoding, and memory stays
//! bounded. Receiving works from async code ([`FrameReceiver::recv`]) and
//! from plain threads ([`FrameReceiver::recv_timeout`]).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

struct State<T> {
	queue: VecDeque<T>,
	capacity: usize,
	senders: usize,
	receiver: bool,
	dropped: u64,
}

struct Shared<T> {
	state: Mutex<State<T>>,
	cond: Condvar,
	notify: Notify,
}

impl<T> Shared<T> {
	fn lock(&self) -> MutexGuard<'_, State<T>> {
		// A panic while holding the lock cannot leave the queue inconsistent.
		self.state.lock().unwrap_or_else(|e| e.into_inner())
	}

	fn wake(&self) {
		self.cond.notify_all();
		self.notify.notify_one();
	}
}

/// A channel keeping at most `capacity` (at least 1) items.
pub fn frame_channel<T>(capacity: usize) -> (FrameSender<T>, FrameReceiver<T>) {
	let shared = Arc::new(Shared {
		state: Mutex::new(State {
			queue: VecDeque::new(),
			capacity: capacity.max(1),
			senders: 1,
			receiver: true,
			dropped: 0,
		}),
		cond: Condvar::new(),
		notify: Notify::new(),
	});
	(FrameSender { shared: shared.clone() }, FrameReceiver { shared })
}

/// Sending half; cloneable. The channel closes when all senders are gone.
pub struct FrameSender<T> {
	shared: Arc<Shared<T>>,
}

impl<T> FrameSender<T> {
	/// Queue an item, dropping the oldest one if full. Returns `false` if the
	/// receiver is gone (the producer should stop).
	pub fn send(&self, item: T) -> bool {
		let mut state = self.shared.lock();
		if !state.receiver {
			return false;
		}
		if state.queue.len() >= state.capacity {
			state.queue.pop_front();
			state.dropped += 1;
		}
		state.queue.push_back(item);
		drop(state);
		self.shared.wake();
		true
	}

	pub fn is_closed(&self) -> bool {
		!self.shared.lock().receiver
	}
}

impl<T> Clone for FrameSender<T> {
	fn clone(&self) -> Self {
		self.shared.lock().senders += 1;
		Self { shared: self.shared.clone() }
	}
}

impl<T> Drop for FrameSender<T> {
	fn drop(&mut self) {
		self.shared.lock().senders -= 1;
		self.shared.wake();
	}
}

/// Receiving half.
pub struct FrameReceiver<T> {
	shared: Arc<Shared<T>>,
}

impl<T> FrameReceiver<T> {
	/// The next item; `None` once the channel is empty and closed.
	pub async fn recv(&mut self) -> Option<T> {
		loop {
			{
				let mut state = self.shared.lock();
				if let Some(item) = state.queue.pop_front() {
					return Some(item);
				}
				if state.senders == 0 {
					return None;
				}
			}
			// `notify_one` stores a permit when nobody waits, so a send
			// between the check above and this await is not lost.
			self.shared.notify.notified().await;
		}
	}

	/// The next item if one is queued.
	pub fn try_recv(&mut self) -> Option<T> {
		self.shared.lock().queue.pop_front()
	}

	/// Block the thread until an item arrives, the channel closes or the
	/// timeout passes. Do not call from async code.
	pub fn recv_timeout(&mut self, timeout: Duration) -> Option<T> {
		let deadline = Instant::now() + timeout;
		let mut state = self.shared.lock();
		loop {
			if let Some(item) = state.queue.pop_front() {
				return Some(item);
			}
			let now = Instant::now();
			if state.senders == 0 || now >= deadline {
				return None;
			}
			state = self
				.shared
				.cond
				.wait_timeout(state, deadline - now)
				.unwrap_or_else(|e| e.into_inner())
				.0;
		}
	}

	/// `true` once all senders are gone (queued items may remain).
	pub fn is_closed(&self) -> bool {
		self.shared.lock().senders == 0
	}

	/// Items dropped because the queue was full.
	pub fn dropped(&self) -> u64 {
		self.shared.lock().dropped
	}
}

impl<T> Drop for FrameReceiver<T> {
	fn drop(&mut self) {
		let mut state = self.shared.lock();
		state.receiver = false;
		state.queue.clear();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn drops_oldest_when_full() {
		let (tx, mut rx) = frame_channel(2);
		for i in 0..5 {
			assert!(tx.send(i));
		}
		assert_eq!(rx.dropped(), 3);
		assert_eq!(rx.try_recv(), Some(3));
		assert_eq!(rx.recv_timeout(Duration::from_millis(1)), Some(4));
		assert_eq!(rx.recv_timeout(Duration::from_millis(1)), None);
		assert!(!rx.is_closed());
		drop(tx);
		assert!(rx.is_closed());
	}

	#[test]
	fn sender_sees_closed_receiver() {
		let (tx, rx) = frame_channel(1);
		let tx2 = tx.clone();
		drop(rx);
		assert!(tx.is_closed());
		assert!(!tx2.send(1));
	}

	#[test]
	fn blocking_receive_across_threads() {
		let (tx, mut rx) = frame_channel(4);
		let t = std::thread::spawn(move || {
			std::thread::sleep(Duration::from_millis(20));
			tx.send(7);
		});
		assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Some(7));
		t.join().unwrap();
		// The sender is gone: no waiting.
		assert_eq!(rx.recv_timeout(Duration::from_secs(5)), None);
	}

	#[tokio::test]
	async fn async_receive() {
		let (tx, mut rx) = frame_channel(4);
		let t = std::thread::spawn(move || {
			for i in 0..3 {
				std::thread::sleep(Duration::from_millis(5));
				tx.send(i);
			}
		});
		let mut got = Vec::new();
		while let Some(i) = rx.recv().await {
			got.push(i);
		}
		t.join().unwrap();
		assert_eq!(got, [0, 1, 2]);
	}
}
