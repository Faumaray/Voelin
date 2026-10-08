//! Desktop integration.
//!
//! - [`hotkey`]: global push-to-talk hotkeys with press and release events
//!   (xdg-desktop-portal on Wayland, XInput2 on X11, a keyboard hook on
//!   Windows)
//! - [`files`]: asking the user for a file (xdg-desktop-portal)
//! - [`notify`]: desktop notifications
//! - [`paths`]: data, config, cache and state directories
//! - [`crash`]: opt-in local crash reports (panic hook)
//! - [`logs`]: the log file, with the previous runs' kept
//! - [`notices`]: third-party notices for the About page
//! - [`proxy`]: the proxy the desktop's network settings name for an address
//!   (xdg-desktop-portal)

pub mod crash;
pub mod files;
pub mod hotkey;
pub mod logs;
pub mod notices;
pub mod notify;
pub mod paths;
pub mod proxy;

pub use hotkey::{BackendKind, Hotkey, HotkeyEvent, HotkeyEvents, HotkeyManager, Key, Modifiers};
pub use notify::{Notification, notify};

/// Application id: desktop file, icon, portal registrations. It cannot change
/// once published on Flathub or Google Play.
pub const APP_ID: &str = "io.github.faumaray.Voelin";
/// Name shown in notifications and window titles.
pub const APP_NAME: &str = "Voelin";

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("invalid hotkey: {0}")]
	InvalidHotkey(String),
	#[error("not supported: {0}")]
	Unsupported(String),
	#[error("{0}")]
	Backend(String),
	#[error("notification: {0}")]
	Notification(String),
}

pub type Result<T> = std::result::Result<T, Error>;
