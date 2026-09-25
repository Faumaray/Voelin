//! Invisible presence and channel-chat relays over ServerQuery.
//!
//! Query clients do not show up in normal clients' channel trees. The
//! [`Observer`] keeps a live [`Presence`](tsc_model::Presence) of a server
//! from one query session. A [`RelayPool`] puts one query session into each
//! channel whose chat someone wants to read or write without joining it.
//!
//! Used by the gateway (`tsgw`) for all of its users and by the client for
//! users who have their own query credentials.

mod convert;
mod observer;
mod relay;

pub use convert::{channel_from_row, client_from_row, delta_from_notification, is_text_message};
pub use observer::{Observer, ObserverConfig, ObserverEvent};
pub use relay::{RelayConfig, RelayEvent, RelayPool};
