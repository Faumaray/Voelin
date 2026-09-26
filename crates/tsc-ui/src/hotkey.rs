//! Global push-to-talk: a task owns the `tsc_platform::HotkeyManager`
//! (portal on Wayland, XInput2 on X11, a hook on Windows) and reports
//! presses and releases.

use tokio::runtime::Runtime;
use tokio::sync::mpsc;

/// Id of the push-to-talk registration (the portal remembers it).
#[cfg(not(target_os = "android"))]
const PTT_ID: &str = "push-to-talk";

/// Handle to the hotkey task.
pub(crate) struct GlobalPtt {
	tx: mpsc::UnboundedSender<Option<String>>,
}

impl GlobalPtt {
	/// Start the task. `status` gets a line for the settings page,
	/// `transmit` the key's state; both are called on the runtime.
	pub fn start(
		runtime: &Runtime,
		status: impl Fn(String) + Send + 'static,
		transmit: impl Fn(bool) + Send + 'static,
	) -> Self {
		let (tx, rx) = mpsc::unbounded_channel();
		runtime.spawn(run(rx, status, transmit));
		Self { tx }
	}

	/// Register `key` (e.g. `Ctrl+Shift+T`) instead of the current one, or
	/// nothing.
	pub fn set(&self, key: Option<String>) {
		let _ = self.tx.send(key);
	}
}

#[cfg(target_os = "android")]
async fn run(
	mut rx: mpsc::UnboundedReceiver<Option<String>>,
	status: impl Fn(String),
	_transmit: impl Fn(bool),
) {
	while let Some(key) = rx.recv().await {
		if key.is_some() {
			status("Global hotkeys are not available on this platform.".into());
		}
	}
}

#[cfg(not(target_os = "android"))]
async fn run(
	mut rx: mpsc::UnboundedReceiver<Option<String>>,
	status: impl Fn(String),
	transmit: impl Fn(bool),
) {
	use tsc_platform::{BackendKind, Hotkey, HotkeyEvent, HotkeyEvents, HotkeyManager};

	async fn next(events: &mut Option<HotkeyEvents>) -> Option<HotkeyEvent> {
		match events {
			Some(events) => events.recv().await,
			None => std::future::pending().await,
		}
	}

	let mut manager: Option<HotkeyManager> = None;
	let mut events: Option<HotkeyEvents> = None;
	loop {
		tokio::select! {
			key = rx.recv() => {
				let Some(key) = key else { break };
				events = None;
				// Released: whatever was held no longer counts.
				transmit(false);
				if let Some(m) = &mut manager {
					let _ = m.unregister(PTT_ID).await;
				}
				let Some(key) = key else {
					status("Off.".into());
					continue;
				};
				let hotkey: Hotkey = match key.parse() {
					Ok(h) => h,
					Err(e) => {
						status(format!("{e}"));
						continue;
					}
				};
				if manager.is_none() {
					match HotkeyManager::new().await {
						Ok(m) => manager = Some(m),
						Err(e) => {
							status(format!("Global hotkeys are not available: {e}"));
							continue;
						}
					}
				}
				let Some(m) = &mut manager else { continue };
				match m.register(PTT_ID, "Push to talk", hotkey).await {
					Ok(registered) => {
						events = Some(registered);
						status(match m.backend() {
							BackendKind::Portal => format!(
								"Registered with the desktop: {}",
								m.trigger_description(PTT_ID).unwrap_or(key)
							),
							BackendKind::X11 => format!("Active: {hotkey} (X11)"),
							BackendKind::WindowsHook => format!("Active: {hotkey}"),
						});
					}
					Err(e) => status(format!("Could not register {key}: {e}")),
				}
			}
			event = next(&mut events) => match event {
				Some(HotkeyEvent::Pressed) => transmit(true),
				Some(HotkeyEvent::Released) => transmit(false),
				None => {
					events = None;
					status("The hotkey was removed by the desktop.".into());
				}
			},
		}
	}
}
