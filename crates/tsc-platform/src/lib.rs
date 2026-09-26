//! Desktop integration.
//!
//! - [`hotkey`]: global push-to-talk hotkeys with press and release events
//!   (xdg-desktop-portal on Wayland, XInput2 on X11, a keyboard hook on
//!   Windows)
//! - [`notify`]: desktop notifications
//! - [`paths`]: data, config, cache and state directories
//! - [`notices`]: third-party notices for the About page

pub mod hotkey;
pub mod notices;
pub mod notify;
pub mod paths;

pub use hotkey::{BackendKind, Hotkey, HotkeyEvent, HotkeyEvents, HotkeyManager, Key, Modifiers};
pub use notify::{Notification, notify};

/// Application id: desktop file, icon, portal registrations. A placeholder
/// until the product has a name.
pub const APP_ID: &str = "io.github.faumaray.TsClient";
/// Name shown in notifications. A placeholder until the product has a name.
pub const APP_NAME: &str = "TS Client";

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
