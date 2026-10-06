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
mod logging;
mod lookup;
mod perms;
mod session;
mod settings;
mod streams;
mod tasks;
#[cfg(test)]
mod tests;

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use clap::Parser;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use voelin_query::Transport;

use crate::config::Bootstrap;
use crate::hub::Hub;
use crate::session::Peer;

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

/// How long a WebSocket connection made while the gateway starts waits for
/// the TeamSpeak server before it is refused.
const START_WAIT: Duration = Duration::from_secs(10);
/// How long open connections get to close at shutdown.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() -> Result<()> {
	let args = Args::parse();
	let layers = config::Layers::load(&args.config, &args.set)?;
	let boot = layers.bootstrap()?;
	let log_file = logging::init(&boot);
	info!(
		version = env!("CARGO_PKG_VERSION"),
		config = %args.config.display(),
		bind = %boot.bind,
		public_url = boot.public_url.as_deref(),
		gateway_id = %boot.gateway_id(),
		voice_port = boot.voice_port,
		query_transport = transport_name(boot.transport),
		query_addr = %boot.addr,
		query_user = %boot.user,
		query_password = if boot.password.is_some() { "set" } else { "none" },
		allowlisted = boot.allowlisted,
		db = %boot.db_path.display(),
		log_file = log_file.as_ref().map(|p| p.display().to_string()),
		"tsgw starting"
	);
	// Listen at once: apps that come while the TeamSpeak server is not
	// reachable yet wait for it a little instead of being turned away.
	let listener = tokio::net::TcpListener::bind(boot.bind)
		.await
		.with_context(|| format!("cannot listen on {}", boot.bind))?;
	info!(
		bind = %boot.bind,
		public_url = boot.public_url.as_deref(),
		well_known = boot.public_url.as_deref().unwrap_or("ws://<host the app asked for>/v1"),
		"listening"
	);
	let app = App::new(boot);
	let service = router(app.clone()).into_make_service_with_connect_info::<SocketAddr>();
	let server = axum::serve(listener, service).with_graceful_shutdown(shutdown(app.clone()));
	let mut server = std::pin::pin!(server.into_future());
	tokio::select! {
		// Stopped before the TeamSpeak server answered.
		served = &mut server => return served.context("serving failed"),
		hub = Hub::start(layers) => {
			let hub = hub?;
			app.hub.send_replace(Some(hub.clone()));
			tokio::spawn(reload_on_hangup(hub));
		}
	}
	server.await.context("serving failed")?;
	app.closed_within(CLOSE_WAIT).await;
	Ok(())
}

fn transport_name(transport: Transport) -> &'static str {
	match transport {
		Transport::Ssh => "ssh",
		Transport::Raw => "raw",
		Transport::Http => "http",
	}
}

/// What the HTTP handlers share.
pub struct App {
	boot: Bootstrap,
	/// The hub, once the gateway is logged in to the TeamSpeak server.
	hub: watch::Sender<Option<Arc<Hub>>>,
	/// Turns true at shutdown, which closes the WebSocket connections.
	stop: watch::Sender<bool>,
	/// WebSocket connections open now, and since the start.
	open: AtomicUsize,
	connections: AtomicU64,
}

impl App {
	fn new(boot: Bootstrap) -> Arc<Self> {
		Arc::new(Self {
			boot,
			hub: watch::channel(None).0,
			stop: watch::channel(false).0,
			open: AtomicUsize::new(0),
			connections: AtomicU64::new(0),
		})
	}

	/// An app for a hub that has started.
	#[cfg(test)]
	fn started(boot: Bootstrap, hub: Arc<Hub>) -> Arc<Self> {
		let app = Self::new(boot);
		app.hub.send_replace(Some(hub));
		app
	}

	fn hub(&self) -> Option<Arc<Hub>> {
		self.hub.borrow().clone()
	}

	/// The hub, waiting up to `wait` for it to start.
	async fn hub_within(&self, wait: Duration) -> Option<Arc<Hub>> {
		let mut hub = self.hub.subscribe();
		let started = tokio::time::timeout(wait, hub.wait_for(Option::is_some)).await;
		started.ok()?.ok()?.clone()
	}

	/// Wait for the WebSocket connections to close, at most `wait`.
	async fn closed_within(&self, wait: Duration) {
		let deadline = Instant::now() + wait;
		while self.open.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
	}
}

pub fn router(app: Arc<App>) -> Router {
	Router::new()
		.route("/v1", get(ws))
		.route("/health", get(health))
		.route("/.well-known/tsgw", get(well_known))
		.with_state(app)
}

/// `ok` while the gateway is logged in to the TeamSpeak server and watching
/// it; otherwise 503 and what is missing.
async fn health(State(app): State<Arc<App>>) -> (StatusCode, String) {
	let Some(hub) = app.hub() else {
		let starting = "starting: not logged in to the TeamSpeak server yet";
		return (StatusCode::SERVICE_UNAVAILABLE, starting.into());
	};
	let (query, observer) = (hub.lookup.is_up(), hub.observer.is_connected());
	if query && observer {
		return (StatusCode::OK, "ok".into());
	}
	let state = |up| if up { "up" } else { "down" };
	let text = format!("query: {}, observer: {}", state(query), state(observer));
	(StatusCode::SERVICE_UNAVAILABLE, text)
}

/// Where apps reach this gateway, for those that find it by asking the
/// server's host on the default port (docs/gateway-admin.md): the
/// configured public URL, else this listener as the request reached it.
async fn well_known(State(app): State<Arc<App>>, headers: HeaderMap) -> Json<serde_json::Value> {
	let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
	let url = app
		.boot
		.public_url
		.clone()
		.unwrap_or_else(|| format!("ws://{}/v1", host.unwrap_or("localhost")));
	debug!(host, answer = %url, "asked where the gateway is");
	Json(serde_json::json!({ "url": url }))
}

async fn ws(
	State(app): State<Arc<App>>,
	connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
	headers: HeaderMap,
	upgrade: WebSocketUpgrade,
) -> Response {
	let text = |name: header::HeaderName| {
		headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
	};
	let peer = Peer {
		id: app.connections.fetch_add(1, Ordering::Relaxed) + 1,
		addr: connect_info.map(|Extension(ConnectInfo(addr))| addr),
		forwarded_for: text(header::HeaderName::from_static("x-forwarded-for")),
		user_agent: text(header::USER_AGENT),
	};
	let mut stop = app.stop.subscribe();
	// Not held up by the wait at shutdown, which waits for open requests.
	let hub = tokio::select! {
		hub = app.hub_within(START_WAIT) => hub,
		_ = stop.wait_for(|stopping| *stopping) => None,
	};
	if *stop.borrow_and_update() {
		debug!(id = peer.id, peer = %peer.address(), "websocket refused: shutting down");
		return (StatusCode::SERVICE_UNAVAILABLE, "the gateway is stopping").into_response();
	}
	let Some(hub) = hub else {
		warn!(
			id = peer.id,
			peer = %peer.address(),
			"websocket refused: not logged in to the TeamSpeak server yet"
		);
		return (StatusCode::SERVICE_UNAVAILABLE, "the gateway is starting").into_response();
	};
	upgrade
		.protocols([voelin_gateway_proto::SUBPROTOCOL])
		.on_upgrade(move |socket| async move {
			app.open.fetch_add(1, Ordering::Relaxed);
			session::run(hub, socket, peer, stop).await;
			app.open.fetch_sub(1, Ordering::Relaxed);
		})
		.into_response()
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

async fn shutdown(app: Arc<App>) {
	terminated().await;
	info!(connections = app.open.load(Ordering::Relaxed), "shutting down");
	app.stop.send_replace(true);
}

/// Ctrl-C, or `SIGTERM` (`systemctl stop`, `docker stop`: as the
/// container's first process tsgw gets no default handling of it).
#[cfg(unix)]
async fn terminated() {
	use tokio::signal::unix::{SignalKind, signal};
	match signal(SignalKind::terminate()) {
		Ok(mut term) => {
			tokio::select! {
				_ = tokio::signal::ctrl_c() => {}
				_ = term.recv() => {}
			}
		}
		Err(_) => {
			let _ = tokio::signal::ctrl_c().await;
		}
	}
}

#[cfg(not(unix))]
async fn terminated() {
	let _ = tokio::signal::ctrl_c().await;
}
