//! View models: engine state turned into the Slint structs the screens
//! show. Pure functions (no window, no engine), unit-tested here; the
//! models live in `crate::models` and are updated row by row with
//! [`list::sync`], so a small change redraws only what changed.

pub mod avatar;
pub mod chat;
pub mod list;
pub mod servers;
pub mod social;
pub mod studio;
pub mod tree;
