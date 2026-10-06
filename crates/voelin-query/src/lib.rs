//! TeamSpeak ServerQuery client.
//!
//! ServerQuery is the admin/bot interface of TeamSpeak servers. Query clients
//! are invisible to normal users, which is what makes invisible presence and
//! channel-chat relays possible.
//!
//! Transports:
//! - raw TCP (TeamSpeak 3, port 10011) and SSH (both, port 10022): line based,
//!   with asynchronous `notify*` events ([`LineClient`])
//! - HTTP WebQuery (TeamSpeak 6 and 3.12+, port 10080): request/response only,
//!   authenticated with an API key ([`HttpClient`])
//!
//! [`QueryClient`] hides the difference for code that only sends commands.

mod client;
mod codec;
mod http;
mod line;
mod ssh;
mod tcp;

pub use client::{Connect, QueryClient, Transport, attempt_worth_a_warning};
pub use codec::{
	Command, Line, Notification, QueryError, Row, escape, parse_line, parse_rows, unescape,
};
pub use http::HttpClient;
pub use line::{FloodGuard, LineClient, LineOptions};

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("io: {0}")]
	Io(#[from] std::io::Error),
	#[error("ssh: {0}")]
	Ssh(#[from] russh::Error),
	#[error("ssh authentication failed")]
	SshAuth,
	#[error("http: {0}")]
	Http(#[from] reqwest::Error),
	#[error("server error {0}")]
	Query(#[from] QueryError),
	#[error("protocol: {0}")]
	Protocol(String),
	#[error("connection closed")]
	Closed,
	#[error("timed out")]
	Timeout,
	#[error("not supported by this transport: {0}")]
	Unsupported(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;
