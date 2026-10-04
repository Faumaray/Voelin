//! A small fork-join thread pool for per-frame pixel work.
//!
//! [`Workers::run`] splits a job into numbered tasks that the pool's threads
//! and the calling thread claim until none are left, and returns once all of
//! them finished. Unlike `std::thread::scope` it spawns nothing per job, and
//! unlike a general task queue it allocates nothing per job: the job is a
//! borrowed closure, published through a mutex and condition variable
//! (futexes on Linux and Android, SRW locks on Windows).
#![allow(unsafe_code)]

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

/// Most tasks one job is split into (see [`Workers::tasks`]).
pub const MAX_TASKS: usize = 64;

/// The job of one [`Workers::run`] call.
#[derive(Clone, Copy)]
struct Job {
	/// The caller's closure with its lifetime erased; see [`Workers::run`]
	/// for why it stays valid while a worker uses it.
	f: *const (dyn Fn(usize) + Sync + 'static),
	tasks: usize,
}

// SAFETY: the closure is `Sync`, so calling it from several threads through
// a shared pointer is allowed; the pointer is only dereferenced while the
// `run` call that owns the closure waits (see `Workers::run`).
unsafe impl Send for Job {}

struct State {
	/// Bumped for every job.
	generation: u64,
	job: Option<Job>,
	/// Workers that took the current job and have not given it back.
	active: usize,
	/// Tasks of the current job that are done.
	finished: usize,
	panic: Option<Box<dyn Any + Send>>,
	stop: bool,
}

struct Shared {
	state: Mutex<State>,
	/// Workers wait here for a job.
	wake: Condvar,
	/// The caller waits here for the last task.
	done: Condvar,
	/// The next task to claim.
	next: AtomicUsize,
}

impl Shared {
	fn lock(&self) -> MutexGuard<'_, State> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

/// Run tasks of `job` until none are left; returns how many this thread
/// ran.
fn claim(shared: &Shared, job: Job, panic: &mut Option<Box<dyn Any + Send>>) -> usize {
	let mut count = 0;
	loop {
		let task = shared.next.fetch_add(1, Ordering::Relaxed);
		if task >= job.tasks {
			return count;
		}
		// SAFETY: `job.f` points to the closure of the `run` call that
		// published `job`; that call does not return before every thread
		// that took the job gave it back (`active`), so the closure lives.
		let f = unsafe { &*job.f };
		if let Err(e) = catch_unwind(AssertUnwindSafe(|| f(task))) {
			panic.get_or_insert(e);
		}
		count += 1;
	}
}

/// A pool of threads for splitting per-frame work. Stops its threads when
/// dropped.
pub struct Workers {
	shared: Arc<Shared>,
	threads: Vec<JoinHandle<()>>,
}

impl Workers {
	/// A pool that uses `threads` threads in total, the caller of
	/// [`Workers::run`] included (so `threads - 1` are spawned). 0 means one
	/// per available CPU.
	pub fn new(name: &str, threads: usize) -> Self {
		let threads = match threads {
			0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
			n => n,
		};
		let shared = Arc::new(Shared {
			state: Mutex::new(State {
				generation: 0,
				job: None,
				active: 0,
				finished: 0,
				panic: None,
				stop: false,
			}),
			wake: Condvar::new(),
			done: Condvar::new(),
			next: AtomicUsize::new(0),
		});
		let mut workers = Self { shared, threads: Vec::new() };
		for i in 1..threads {
			let shared = workers.shared.clone();
			let spawned = std::thread::Builder::new()
				.name(format!("{name}-{i}"))
				.spawn(move || worker(&shared));
			match spawned {
				Ok(thread) => workers.threads.push(thread),
				Err(e) => {
					tracing::warn!("cannot start a pixel worker thread: {e}");
					break;
				}
			}
		}
		workers
	}

	/// Threads that work on a job, the caller included.
	pub fn threads(&self) -> usize {
		self.threads.len() + 1
	}

	/// A good number of tasks to split a job of `units` independent parts
	/// (e.g. pairs of rows) into: a few per thread, so a thread that gets
	/// descheduled does not hold everyone up.
	pub fn tasks(&self, units: usize) -> usize {
		(self.threads() * 3).min(MAX_TASKS).min(units).max(1)
	}

	/// Call `f(0)`, ..., `f(tasks - 1)` on the pool's threads and this one,
	/// each exactly once, and return when all returned. A panic in `f` is
	/// resumed here after the other tasks finished.
	pub fn run(&mut self, tasks: usize, f: &(dyn Fn(usize) + Sync)) {
		if tasks <= 1 || self.threads.is_empty() {
			(0..tasks).for_each(f);
			return;
		}
		// SAFETY: this only erases the lifetime. The pointer is published in
		// `State::job` below and removed before this function returns. A
		// worker copies it only while holding the lock and counts itself in
		// `active` in the same critical section; it gives it back
		// (`active -= 1`) after its last use. This function waits until
		// `active` is zero, and takes `&mut self`, so no second job can
		// overlap. Hence `f` outlives every dereference of the pointer.
		let f_static: &'static (dyn Fn(usize) + Sync) = unsafe { std::mem::transmute(f) };
		let job = Job { f: f_static, tasks };
		{
			let mut state = self.shared.lock();
			self.shared.next.store(0, Ordering::Relaxed);
			state.generation = state.generation.wrapping_add(1);
			state.job = Some(job);
			state.finished = 0;
		}
		self.shared.wake.notify_all();
		let mut panic = None;
		let count = claim(&self.shared, job, &mut panic);
		let mut state = self.shared.lock();
		state.finished += count;
		while state.finished < tasks || state.active > 0 {
			state = self.shared.done.wait(state).unwrap_or_else(PoisonError::into_inner);
		}
		state.job = None;
		let panic = panic.or_else(|| state.panic.take());
		drop(state);
		if let Some(panic) = panic {
			resume_unwind(panic);
		}
	}
}

fn worker(shared: &Shared) {
	let mut seen = 0;
	loop {
		let job = {
			let mut state = shared.lock();
			loop {
				if state.stop {
					return;
				}
				if let Some(job) = state.job
					&& state.generation != seen
				{
					seen = state.generation;
					state.active += 1;
					break job;
				}
				state = shared.wake.wait(state).unwrap_or_else(PoisonError::into_inner);
			}
		};
		let mut panic = None;
		let count = claim(shared, job, &mut panic);
		let mut state = shared.lock();
		state.finished += count;
		state.active -= 1;
		if panic.is_some() && state.panic.is_none() {
			state.panic = panic;
		}
		if state.active == 0 && state.finished >= job.tasks {
			shared.done.notify_all();
		}
	}
}

impl Drop for Workers {
	fn drop(&mut self) {
		self.shared.lock().stop = true;
		self.shared.wake.notify_all();
		for thread in self.threads.drain(..) {
			let _ = thread.join();
		}
	}
}

/// Hands out the items of a slice to the tasks of a [`Workers::run`] job,
/// each item to exactly one task, without `unsafe` or allocation.
pub struct Slots<T> {
	slots: [Mutex<Option<T>>; MAX_TASKS],
	len: usize,
}

impl<T> Slots<T> {
	/// Up to [`MAX_TASKS`] items; more panic.
	pub fn new(items: impl IntoIterator<Item = T>) -> Self {
		let slots = std::array::from_fn(|_| Mutex::new(None));
		let mut len = 0;
		for (slot, item) in slots.iter().zip(items) {
			*slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(item);
			len += 1;
		}
		Self { slots, len }
	}

	pub fn len(&self) -> usize {
		self.len
	}

	pub fn is_empty(&self) -> bool {
		self.len == 0
	}

	/// Item `i`, once.
	pub fn take(&self, i: usize) -> Option<T> {
		self.slots.get(i)?.lock().unwrap_or_else(PoisonError::into_inner).take()
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::AtomicU64;

	use super::*;

	#[test]
	fn runs_every_task_once() {
		let mut workers = Workers::new("test-workers", 4);
		assert_eq!(workers.threads(), 4);
		for round in 0..200 {
			let tasks = 1 + round % 40;
			let hits: Vec<AtomicU64> = (0..tasks).map(|_| AtomicU64::new(0)).collect();
			workers.run(tasks, &|i| {
				hits[i].fetch_add(1, Ordering::Relaxed);
			});
			assert!(hits.iter().all(|h| h.load(Ordering::Relaxed) == 1), "round {round}");
		}
	}

	#[test]
	fn borrows_and_mutates_through_slots() {
		let mut workers = Workers::new("test-workers", 3);
		let mut data = vec![0u32; 1000];
		let slots = Slots::new(data.chunks_mut(100));
		assert_eq!(slots.len(), 10);
		workers.run(slots.len(), &|i| {
			for x in slots.take(i).unwrap() {
				*x = i as u32 + 1;
			}
		});
		assert!(data.chunks(100).enumerate().all(|(i, c)| c.iter().all(|&x| x == i as u32 + 1)));
	}

	#[test]
	fn panics_reach_the_caller_and_the_pool_survives() {
		let mut workers = Workers::new("test-workers", 2);
		let result = catch_unwind(AssertUnwindSafe(|| {
			workers.run(8, &|i| assert_ne!(i, 5, "task five"));
		}));
		assert!(result.is_err());
		let count = AtomicU64::new(0);
		workers.run(8, &|_| {
			count.fetch_add(1, Ordering::Relaxed);
		});
		assert_eq!(count.load(Ordering::Relaxed), 8);
	}

	#[test]
	fn single_thread_runs_inline() {
		let mut workers = Workers::new("test-workers", 1);
		assert_eq!(workers.threads(), 1);
		assert_eq!(workers.tasks(100), 3);
		let count = AtomicU64::new(0);
		workers.run(5, &|_| {
			count.fetch_add(1, Ordering::Relaxed);
		});
		assert_eq!(count.load(Ordering::Relaxed), 5);
	}
}
