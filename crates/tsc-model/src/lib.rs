//! Domain model shared by the client engine, the gateway and the UI.
//!
//! Nothing in here does IO; the types describe what a server offers and what
//! the user sees.

mod server;

pub use server::{Capabilities, ServerFlavor, ServerVersion};
