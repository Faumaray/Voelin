//! Taps: the processed microphone for consumers besides the voice
//! connection, such as the audio mixer of our screen share.
//!
//! The voice pipeline publishes every processed chunk (48 kHz mono, after
//! echo cancellation, noise suppression and gain control) to
//! [`microphone()`]. Unused, that is one relaxed atomic load per chunk.
//! With consumers attached ([`Tap::attach`]) the publisher hands the chunk
//! to each [`TapSink`] on its own thread; it never waits: the consumer list
//! is taken with a try-lock that only contends with attaching and
//! detaching, and a chunk that meets such a change is skipped.
//!
//! Several pipelines can run at once (one per server, the microphone
//! test); the tap follows one of them at a time: the first that publishes,
//! until it has been silent for [`HANDOVER`], then whichever publishes next.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// A publisher silent this long loses the tap to another one.
pub const HANDOVER: Duration = Duration::from_millis(200);

/// Receives tapped audio on the publisher's thread. Must not block.
pub trait TapSink: Send {
	/// 48 kHz mono samples.
	fn write(&mut self, samples: &[f32]);

	/// `frames` samples of silence (e.g. while the microphone is muted).
	fn silence(&mut self, frames: usize);
}

/// A publish point that any number of consumers can attach to.
pub struct Tap {
	/// Attached sinks: the only thing publishers look at when unused.
	active: AtomicUsize,
	sinks: Mutex<Vec<(u64, Box<dyn TapSink>)>>,
	next_sink: AtomicU64,
	/// The publisher followed now (0: none) and when it last published
	/// (milliseconds since `epoch`).
	owner: AtomicU64,
	owner_seen: AtomicU64,
}

static MICROPHONE: Tap = Tap::new();
static NEXT_PUBLISHER: AtomicU64 = AtomicU64::new(1);

/// Processed microphone audio, 48 kHz mono.
pub fn microphone() -> &'static Tap {
	&MICROPHONE
}

/// An id for a publisher (one per voice pipeline).
pub fn publisher_id() -> u64 {
	NEXT_PUBLISHER.fetch_add(1, Ordering::Relaxed)
}

fn now_ms() -> u64 {
	static EPOCH: OnceLock<Instant> = OnceLock::new();
	EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

impl Default for Tap {
	fn default() -> Self {
		Self::new()
	}
}

impl Tap {
	pub const fn new() -> Self {
		Self {
			active: AtomicUsize::new(0),
			sinks: Mutex::new(Vec::new()),
			next_sink: AtomicU64::new(1),
			owner: AtomicU64::new(0),
			owner_seen: AtomicU64::new(0),
		}
	}

	/// Whether anyone listens: publishers skip all work otherwise.
	#[inline]
	pub fn is_active(&self) -> bool {
		self.active.load(Ordering::Relaxed) != 0
	}

	/// Attach a consumer until the returned guard is dropped.
	pub fn attach(&self, sink: Box<dyn TapSink>) -> TapGuard<'_> {
		let id = self.next_sink.fetch_add(1, Ordering::Relaxed);
		let mut sinks = self.sinks.lock().unwrap_or_else(PoisonError::into_inner);
		sinks.push((id, sink));
		self.active.store(sinks.len(), Ordering::Relaxed);
		TapGuard { tap: self, id }
	}

	fn detach(&self, id: u64) {
		let mut sinks = self.sinks.lock().unwrap_or_else(PoisonError::into_inner);
		sinks.retain(|(i, _)| *i != id);
		self.active.store(sinks.len(), Ordering::Relaxed);
	}

	/// Whether `publisher` is the one followed now (taking over from one
	/// that went silent).
	fn follows(&self, publisher: u64) -> bool {
		let now = now_ms();
		let owner = self.owner.load(Ordering::Acquire);
		if owner != publisher {
			let stale = now.saturating_sub(self.owner_seen.load(Ordering::Relaxed))
				>= HANDOVER.as_millis() as u64;
			if owner != 0 && !stale {
				return false;
			}
			if self
				.owner
				.compare_exchange(owner, publisher, Ordering::AcqRel, Ordering::Relaxed)
				.is_err()
			{
				return false;
			}
		}
		self.owner_seen.store(now, Ordering::Relaxed);
		true
	}

	/// Hand `samples` to every consumer (no-op without consumers, or when
	/// another publisher is followed).
	pub fn publish(&self, publisher: u64, samples: &[f32]) {
		if !self.is_active() || samples.is_empty() || !self.follows(publisher) {
			return;
		}
		if let Ok(mut sinks) = self.sinks.try_lock() {
			for (_, sink) in sinks.iter_mut() {
				sink.write(samples);
			}
		}
	}

	/// Hand `frames` samples of silence to every consumer.
	pub fn publish_silence(&self, publisher: u64, frames: usize) {
		if !self.is_active() || frames == 0 || !self.follows(publisher) {
			return;
		}
		if let Ok(mut sinks) = self.sinks.try_lock() {
			for (_, sink) in sinks.iter_mut() {
				sink.silence(frames);
			}
		}
	}
}

/// Detaches its consumer when dropped.
pub struct TapGuard<'a> {
	tap: &'a Tap,
	id: u64,
}

impl Drop for TapGuard<'_> {
	fn drop(&mut self) {
		self.tap.detach(self.id);
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use super::*;

	#[derive(Clone, Default)]
	struct Collect(Arc<Mutex<Vec<f32>>>);

	impl TapSink for Collect {
		fn write(&mut self, samples: &[f32]) {
			self.0.lock().unwrap().extend_from_slice(samples);
		}

		fn silence(&mut self, frames: usize) {
			self.0.lock().unwrap().extend(std::iter::repeat_n(0.0, frames));
		}
	}

	#[test]
	fn consumers_attach_and_detach() {
		let tap = Tap::new();
		let (a, b) = (publisher_id(), publisher_id());
		assert!(!tap.is_active());
		tap.publish(a, &[1.0]);
		let first = Collect::default();
		let guard = tap.attach(Box::new(first.clone()));
		assert!(tap.is_active());
		tap.publish(a, &[0.5, 0.25]);
		tap.publish_silence(a, 2);
		// Another publisher is ignored while the first is live.
		tap.publish(b, &[9.0]);
		let second = Collect::default();
		let guard2 = tap.attach(Box::new(second.clone()));
		tap.publish(a, &[0.125]);
		drop(guard);
		tap.publish(a, &[0.0625]);
		assert_eq!(*first.0.lock().unwrap(), [0.5, 0.25, 0.0, 0.0, 0.125]);
		assert_eq!(*second.0.lock().unwrap(), [0.125, 0.0625]);
		drop(guard2);
		assert!(!tap.is_active());
	}

	#[test]
	fn a_silent_publisher_hands_over() {
		let tap = Tap::new();
		let (a, b) = (publisher_id(), publisher_id());
		let sink = Collect::default();
		let _guard = tap.attach(Box::new(sink.clone()));
		tap.publish(a, &[1.0]);
		tap.publish(b, &[2.0]);
		std::thread::sleep(HANDOVER + Duration::from_millis(20));
		tap.publish(b, &[3.0]);
		tap.publish(a, &[4.0]);
		assert_eq!(*sink.0.lock().unwrap(), [1.0, 3.0]);
	}
}
