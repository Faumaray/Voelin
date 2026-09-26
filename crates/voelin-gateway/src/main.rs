//! tsgw: companion gateway for TeamSpeak 3/6 servers.
//!
//! Gives app users invisible presence (who is in which channel) and channel
//! chat without joining voice, through ServerQuery. See
//! docs/gateway-admin.md and the protocol in `voelin-gateway-proto`.

mod config;
mod db;
mod hub;
mod perms;
mod session;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::hub::Hub;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
	/// Configuration file.
	#[arg(long, short, default_value = "tsgw.toml", env = "TSGW_CONFIG")]
	config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(
			EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
		)
		.init();
	let args = Args::parse();
	let config = config::Config::load(&args.config)?;
	let bind = config.listen.bind;
	let hub = Hub::start(config).await?;
	let app = Router::new()
		.route("/v1", get(ws))
		.route("/health", get(|| async { "ok" }))
		.with_state(hub);
	let listener = tokio::net::TcpListener::bind(bind).await?;
	info!(%bind, "listening");
	axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
	Ok(())
}

async fn ws(State(hub): State<Arc<Hub>>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
	upgrade
		.protocols([voelin_gateway_proto::SUBPROTOCOL])
		.on_upgrade(move |socket| session::run(hub, socket))
}

async fn shutdown() {
	let _ = tokio::signal::ctrl_c().await;
	info!("shutting down");
}
