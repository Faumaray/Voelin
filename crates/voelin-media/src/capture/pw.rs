//! Shared PipeWire plumbing: a thread running a main loop.

use std::os::fd::OwnedFd;
use std::thread::JoinHandle;

use pipewire as pw;
use pw::spa::pod::{Pod, Value, serialize::PodSerializer};

/// A thread with a PipeWire main loop; quits and joins on drop.
pub(crate) struct PwThread {
	quit: pw::channel::Sender<()>,
	thread: Option<JoinHandle<()>>,
}

impl PwThread {
	/// Connect to PipeWire on a new thread (through `fd`, e.g. a portal
	/// remote, or to the user's daemon), let `setup` create the streams, and
	/// run the loop until stopped. Setup errors are returned here.
	///
	/// `setup` returns what must stay alive while the loop runs (streams,
	/// listeners); it can stop the loop itself through the main loop.
	pub fn spawn<S, K>(name: &str, fd: Option<OwnedFd>, setup: S) -> Result<Self, String>
	where
		S: FnOnce(&pw::core::CoreRc, &pw::main_loop::MainLoopRc) -> Result<K, String>
			+ Send
			+ 'static,
		K: 'static,
	{
		let (quit, quit_rx) = pw::channel::channel::<()>();
		let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), String>>();
		let thread = std::thread::Builder::new()
			.name(name.to_owned())
			.spawn(move || {
				pw::init();
				let started = (|| {
					let mainloop = pw::main_loop::MainLoopRc::new(None)
						.map_err(|e| format!("PipeWire main loop: {e}"))?;
					let context = pw::context::ContextRc::new(&mainloop, None)
						.map_err(|e| format!("PipeWire context: {e}"))?;
					let core = match fd {
						Some(fd) => context.connect_fd_rc(fd, None),
						None => context.connect_rc(None),
					}
					.map_err(|e| format!("cannot connect to PipeWire: {e}"))?;
					let keep = setup(&core, &mainloop)?;
					Ok::<_, String>((mainloop, context, core, keep))
				})();
				let (mainloop, _context, _core, keep) = match started {
					Ok(parts) => parts,
					Err(e) => {
						let _ = init_tx.send(Err(e));
						return;
					}
				};
				let weak = mainloop.downgrade();
				let _quit = quit_rx.attach(mainloop.loop_(), move |()| {
					if let Some(mainloop) = weak.upgrade() {
						mainloop.quit();
					}
				});
				let _ = init_tx.send(Ok(()));
				mainloop.run();
				drop(keep);
			})
			.map_err(|e| e.to_string())?;
		let mut this = Self { quit, thread: Some(thread) };
		match init_rx.recv() {
			Ok(Ok(())) => Ok(this),
			Ok(Err(e)) => {
				this.stop();
				Err(e)
			}
			Err(_) => {
				this.stop();
				Err("the PipeWire thread ended during setup".into())
			}
		}
	}

	pub fn stop(&mut self) {
		let _ = self.quit.send(());
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

impl Drop for PwThread {
	fn drop(&mut self) {
		self.stop();
	}
}

/// Serialize a format object for `Stream::connect`.
pub(crate) fn serialize(value: Value) -> Result<Vec<u8>, String> {
	PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
		.map(|(cursor, _)| cursor.into_inner())
		.map_err(|e| format!("cannot build the PipeWire format: {e:?}"))
}

pub(crate) fn pod(bytes: &[u8]) -> Result<&Pod, String> {
	Pod::from_bytes(bytes).ok_or_else(|| "invalid PipeWire format pod".to_owned())
}
