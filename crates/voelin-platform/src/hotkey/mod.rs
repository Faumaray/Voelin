//! Global hotkeys with press and release events, for push-to-talk.
//!
//! Backends, picked by [`HotkeyManager::new`]:
//!
//! - **Portal** (Wayland): the xdg-desktop-portal `GlobalShortcuts`
//!   interface. The compositor owns the binding: our [`Hotkey`] is only the
//!   preferred trigger, the user confirms or changes it in a system dialog,
//!   and the portal reports `Activated` / `Deactivated`. Needs KDE Plasma
//!   5.27+, GNOME 48+ or another desktop implementing it.
//! - **X11**: XInput2 raw key and button events on the root window. They are
//!   delivered whatever window has focus and nothing is grabbed, so the key
//!   still reaches the focused application. Under Wayland this only sees keys
//!   while an X11 (XWayland) window has focus.
//! - **Windows**: a low-level keyboard and mouse hook (`WH_KEYBOARD_LL`,
//!   `WH_MOUSE_LL`) on its own thread. `RegisterHotKey` would only report
//!   presses and swallow the key.

// Builds without a backend (other platforms, or Linux without both
// features) compile, but have nothing to register with.
#![cfg_attr(
	not(any(
		windows,
		all(unix, not(target_os = "macos"), any(feature = "portal", feature = "x11"))
	)),
	allow(unused_variables, unreachable_code)
)]

mod key;
#[cfg(any(test, windows, all(unix, not(target_os = "macos"), feature = "x11")))]
mod tracker;

#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
mod portal;
#[cfg(windows)]
mod windows;
#[cfg(all(unix, not(target_os = "macos"), feature = "x11"))]
mod x11;

pub use key::{Hotkey, Key, Modifiers, NamedKey};
use tokio::sync::mpsc;

use crate::{Error, Result};

/// A registered hotkey went down or up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyEvent {
	Pressed,
	Released,
}

/// Events of one registration; dropping it unregisters the hotkey on the
/// next key event (X11, Windows) or on [`HotkeyManager::unregister`].
pub type HotkeyEvents = mpsc::UnboundedReceiver<HotkeyEvent>;

/// How hotkeys are captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
	Portal,
	X11,
	WindowsHook,
}

enum Backend {
	#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
	Portal(portal::PortalBackend),
	#[cfg(all(unix, not(target_os = "macos"), feature = "x11"))]
	X11(x11::X11Backend),
	#[cfg(windows)]
	Windows(windows::HookBackend),
	/// Keeps matches exhaustive on platforms without any backend.
	#[allow(dead_code)]
	None(std::convert::Infallible),
}

/// Registers global hotkeys. See the [module docs](self) for the backends.
pub struct HotkeyManager {
	backend: Backend,
}

impl HotkeyManager {
	/// The best backend for this desktop session: the portal on Wayland
	/// (falling back to X11 when the desktop does not offer it), X11 on X11,
	/// the hook on Windows. Must be called within a Tokio runtime (the portal
	/// listens for its signals on a task).
	pub async fn new() -> Result<Self> {
		let mut errors = Vec::new();
		for kind in preferred_backends() {
			match Self::with_backend(kind).await {
				Ok(manager) => return Ok(manager),
				Err(e) => {
					tracing::debug!(?kind, %e, "hotkey backend unavailable");
					errors.push(format!("{kind:?}: {e}"));
				}
			}
		}
		Err(Error::Unsupported(if errors.is_empty() {
			"no hotkey backend for this platform".into()
		} else {
			errors.join("; ")
		}))
	}

	/// Use a specific backend.
	pub async fn with_backend(kind: BackendKind) -> Result<Self> {
		let backend = match kind {
			#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
			BackendKind::Portal => Backend::Portal(portal::PortalBackend::new().await?),
			#[cfg(all(unix, not(target_os = "macos"), feature = "x11"))]
			BackendKind::X11 => Backend::X11(x11::X11Backend::connect(None)?),
			#[cfg(windows)]
			BackendKind::WindowsHook => Backend::Windows(windows::HookBackend::start()?),
			#[allow(unreachable_patterns)]
			_ => return Err(Error::Unsupported(format!("{kind:?} is not available in this build"))),
		};
		Ok(Self { backend })
	}

	/// X11 on a specific display (e.g. `":1"`), mainly for tests.
	#[cfg(all(unix, not(target_os = "macos"), feature = "x11"))]
	pub fn x11(display: &str) -> Result<Self> {
		Ok(Self { backend: Backend::X11(x11::X11Backend::connect(Some(display))?) })
	}

	pub fn backend(&self) -> BackendKind {
		match &self.backend {
			#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
			Backend::Portal(_) => BackendKind::Portal,
			#[cfg(all(unix, not(target_os = "macos"), feature = "x11"))]
			Backend::X11(_) => BackendKind::X11,
			#[cfg(windows)]
			Backend::Windows(_) => BackendKind::WindowsHook,
			Backend::None(never) => match *never {},
		}
	}

	/// Register `hotkey` under a stable `id` (the portal remembers the
	/// user's choice by it); `description` is shown in the portal's dialog.
	/// Registering an id again replaces it.
	///
	/// With the portal this may show a dialog and waits for the user.
	pub async fn register(
		&mut self,
		id: &str,
		description: &str,
		hotkey: Hotkey,
	) -> Result<HotkeyEvents> {
		let _ = description;
		match &mut self.backend {
			#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
			Backend::Portal(p) => p.register(id, description, hotkey).await,
			#[cfg(all(unix, not(target_os = "macos"), feature = "x11"))]
			Backend::X11(x) => Ok(x.register(id, hotkey)),
			#[cfg(windows)]
			Backend::Windows(w) => Ok(w.register(id, hotkey)),
			Backend::None(never) => match *never {},
		}
	}

	pub async fn unregister(&mut self, id: &str) -> Result<()> {
		match &mut self.backend {
			#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
			Backend::Portal(p) => p.unregister(id).await,
			#[cfg(all(unix, not(target_os = "macos"), feature = "x11"))]
			Backend::X11(x) => {
				x.unregister(id);
				Ok(())
			}
			#[cfg(windows)]
			Backend::Windows(w) => {
				w.unregister(id);
				Ok(())
			}
			Backend::None(never) => match *never {},
		}
	}

	/// What actually triggers `id`, as the desktop describes it (portal
	/// only: the user may have picked another key). Other backends return
	/// `None`; the registered [`Hotkey`] is the trigger.
	pub fn trigger_description(&self, id: &str) -> Option<String> {
		match &self.backend {
			#[cfg(all(unix, not(target_os = "macos"), feature = "portal"))]
			Backend::Portal(p) => p.trigger_description(id),
			#[allow(unreachable_patterns)]
			_ => {
				let _ = id;
				None
			}
		}
	}
}

/// Backends to try, best first.
fn preferred_backends() -> Vec<BackendKind> {
	if cfg!(windows) {
		return vec![BackendKind::WindowsHook];
	}
	if !cfg!(all(unix, not(target_os = "macos"))) {
		return Vec::new();
	}
	let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
		|| std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland");
	let x11 = std::env::var_os("DISPLAY").is_some();
	let mut kinds = Vec::new();
	if wayland {
		kinds.push(BackendKind::Portal);
	}
	if x11 {
		kinds.push(BackendKind::X11);
	}
	if !wayland {
		// E.g. a portal without a Wayland session, or no display variables.
		kinds.push(BackendKind::Portal);
	}
	kinds
}
