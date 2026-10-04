#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

/// Generated protobuf packages. Original package names and RPC routes are preserved.
#[allow(clippy::all, rustdoc::broken_intra_doc_links, non_camel_case_types)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/packages.rs"));
}

/// Local account serialization messages (proto2 presence is preserved).
pub use proto::com::teamspeak::account::proto as account;
/// Main application namespace (login, user, management, chat, sync, integrations).
pub use proto::com::teamspeak::myteamspeak::proto as api;
/// Streaming push transport messages and client.
pub use proto::com::teamspeak::push::proto as push;
/// Local synchronization serialization messages.
pub use proto::com::teamspeak::sync::proto as sync;

pub mod client;
#[cfg(feature = "reflection")]
pub mod reflection;
pub mod session;
pub mod wire;

pub use client::{ApiClient, ClientError};
pub use prost;
pub use prost_types;
pub use session::{SessionRequest, SessionToken};
pub use tonic;

/// Includes imports and all schemas enabled for this build; suitable for reflection.
pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/api_descriptor.bin"));
