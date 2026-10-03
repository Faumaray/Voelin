//! Callbacks of the screens, wired to the app. One module per screen or
//! area; each has a `wire` that the window calls once.

mod chat;
mod emoji;
mod pages;
mod servers;
mod settings;
mod social;
mod streams;

use crate::app::MainWindow;

/// Connect every callback of the window.
pub(crate) fn wire(ui: &MainWindow) {
	servers::wire(ui);
	chat::wire(ui);
	streams::wire(ui);
	settings::wire(ui);
	emoji::wire(ui);
	social::wire(ui);
	pages::wire(ui);
}
