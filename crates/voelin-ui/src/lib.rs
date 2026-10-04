//! Slint user interface shared by the desktop binary and the mobile apps.

mod app;
mod appearance;
mod bind;
mod camera;
mod chat;
mod dev;
mod emoji;
mod events;
mod home;
mod hotkey;
mod identities;
mod images;
mod inbox;
mod members;
mod messages;
mod previews;
mod servers;
mod settings;
mod settings_page;
mod settings_pages;
mod social;
mod streams;
mod studio;
mod video;
mod vm;

pub use app::{HostedEngine, RunOptions, run};
pub use inbox::{Request, request};
