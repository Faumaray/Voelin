//! Slint user interface shared by the desktop binary and the mobile apps.

mod app;
mod appearance;
mod bind;
mod chat;
mod dev;
mod emoji;
mod hotkey;
mod images;
mod inbox;
mod members;
mod servers;
mod settings;
mod settings_page;
mod streams;
mod studio;
mod video;
mod vm;

pub use app::{HostedEngine, RunOptions, run};
pub use inbox::{Request, request};
