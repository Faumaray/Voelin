//! tsgw: companion gateway for TeamSpeak 3/6 servers.
//!
//! Gives app users invisible presence (who is in which channel) and channel
//! chat without joining voice, through ServerQuery, plus pins, reactions,
//! topics, events, a stream directory and an activity feed. See
//! docs/gateway-admin.md and the protocol in `voelin-gateway-proto`.

mod config;
mod db;
mod features;
mod hub;
mod perms;
mod session;
mod settings;
mod streams;
mod tasks;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::hub::Hub;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
	/// Configuration file.
	#[arg(long, short, default_value = "tsgw.toml", env = "TSGW_CONFIG")]
	config: PathBuf,
	/// Set a key, e.g. `--set history.retention_days=90`; wins over the
	/// environment and the file, but not over values set at runtime.
	#[arg(long = "set", value_name = "KEY=VALUE")]
	set: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(
			EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
		)
		.init();
	let args = Args::parse();
	let layers = config::Layers::load(&args.config, &args.set)?;
	let bind = layers.bootstrap()?.bind;
	let hub = Hub::start(layers).await?;
	tokio::spawn(reload_on_hangup(hub.clone()));
	let listener = tokio::net::TcpListener::bind(bind).await?;
	info!(%bind, "listening");
	axum::serve(listener, router(hub)).with_graceful_shutdown(shutdown()).await?;
	Ok(())
}

pub fn router(hub: Arc<Hub>) -> Router {
	Router::new().route("/v1", get(ws)).route("/health", get(|| async { "ok" })).with_state(hub)
}

async fn ws(State(hub): State<Arc<Hub>>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
	upgrade
		.protocols([voelin_gateway_proto::SUBPROTOCOL])
		.on_upgrade(move |socket| session::run(hub, socket))
}

/// `SIGHUP` reads the configuration file again.
#[cfg(unix)]
async fn reload_on_hangup(hub: Arc<Hub>) {
	use tokio::signal::unix::{SignalKind, signal};
	let Ok(mut hangup) = signal(SignalKind::hangup()) else { return };
	while hangup.recv().await.is_some() {
		if let Err(error) = hub.settings.reload() {
			warn!(error = format!("{error:#}"), "reload failed; keeping the current configuration");
		}
	}
}

#[cfg(not(unix))]
async fn reload_on_hangup(_hub: Arc<Hub>) {}

async fn shutdown() {
	let _ = tokio::signal::ctrl_c().await;
	info!("shutting down");
}
