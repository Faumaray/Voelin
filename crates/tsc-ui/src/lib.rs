//! Slint user interface shared by the desktop binary and the mobile apps.

mod app;
mod hotkey;
mod settings;
mod streams;
mod video;

pub use app::{RunOptions, run};
