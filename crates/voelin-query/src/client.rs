//! Transport-independent query client.

use std::time::Duration;

use tokio::io::split;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout_at};
use tracing::debug;

use crate::codec::{Command, Notification, Row};
use crate::http::HttpClient;
use crate::line::{LineClient, LineOptions};
use crate::tcp::QuickAck;
use crate::{Error, Result};

/// How long to send nothing after the server dropped an SSH login: its flood
/// protection counts per 3 seconds.
const SSH_DROP_WAIT: Duration = Duration::from_secs(3);

/// Seconds between attempts to open a query connection, the last repeating.
const RETRY_SECS: [u64; 6] = [1, 2, 3, 5, 10, 30];

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
	Raw,
	Ssh,
	Http,
}

/// Everything needed to open a query connection.
#[derive(Clone, Debug)]
pub struct Connect {
	pub transport: Transport,
	/// host:port of the query interface.
	pub addr: String,
	/// Login name (raw/SSH). Ignored for HTTP.
	pub user: String,
	/// Password (raw/SSH) or API key (HTTP, optional for guest access).
	pub secret: Option<String>,
	/// Select the virtual server by voice port (`use port=`)...
	pub server_port: Option<u16>,
	/// ...or by id (defaults to 1).
	pub server_id: Option<u32>,
	pub line: LineOptions,
}

impl Connect {
	/// How long to wait after failed attempt `attempt` (counted from 1) to
	/// open a connection: the next step of [`RETRY_SECS`], or longer if the
	/// server's flood protection asked for it.
	pub fn retry_delay(&self, attempt: u32) -> Duration {
		let step = RETRY_SECS[(attempt.max(1) as usize - 1).min(RETRY_SECS.len() - 1)];
		Duration::from_secs(step).max(self.line.flood.held_for().unwrap_or_default())
	}
}

/// Whether failed attempt `attempt` to open a connection is worth a
/// warning: the first few are, then every tenth (about every 5 minutes at
/// the longest wait). Log the others at debug level.
pub fn attempt_worth_a_warning(attempt: u32) -> bool {
	attempt <= 3 || attempt.is_multiple_of(10)
}

/// Fails at once when the server's flood protection asked to wait past
/// `deadline`: the connection could not log in in time anyway.
fn flood_wait_fits(options: &LineOptions, addr: &str, deadline: Instant) -> Result<()> {
	match options.flood.held_for() {
		Some(held) if Instant::now() + held > deadline => Err(io_timeout(format!(
			"the server's flood protection asks to wait {} s; not connecting to {addr} now",
			held.as_secs()
		))),
		_ => Ok(()),
	}
}

fn io_timeout(message: String) -> Error {
	Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, message))
}

/// A logged-in query connection with a selected virtual server.
#[derive(Clone)]
pub enum QueryClient {
	Line(LineClient),
	Http(HttpClient),
}

impl QueryClient {
	/// Connect, log in and select the virtual server. The event receiver is
	/// `None` for HTTP, which has no events.
	pub async fn connect(
		cfg: &Connect,
	) -> Result<(Self, Option<mpsc::UnboundedReceiver<Notification>>)> {
		let options = &cfg.line;
		// Connecting, logging in and selecting the server, all of it.
		let deadline = Instant::now() + options.connect_timeout;
		let timed_out = |what: &str| {
			let wait_s = options.connect_timeout.as_secs_f32();
			io_timeout(format!("{what} at {} within {wait_s} s", cfg.addr))
		};
		match cfg.transport {
			Transport::Raw => {
				flood_wait_fits(options, &cfg.addr, deadline)?;
				let stream = QuickAck::connect(&cfg.addr, options.connect_timeout).await?;
				let (r, w) = split(stream);
				let (client, events) = LineClient::spawn(r, w, options.clone());
				let client = Self::Line(client);
				timeout_at(deadline, client.login_and_use(cfg))
					.await
					.map_err(|_| timed_out("no query login"))??;
				Ok((client, Some(events)))
			}
			Transport::Ssh => {
				// The server counts the SSH login as a command.
				flood_wait_fits(options, &cfg.addr, deadline)?;
				timeout_at(deadline, options.flood.acquire(options.rate_limit))
					.await
					.map_err(|_| timed_out("the flood protection allowed no login"))?;
				let password = cfg.secret.as_deref().unwrap_or_default();
				let wait = deadline.saturating_duration_since(Instant::now());
				let login = crate::ssh::connect(&cfg.addr, &cfg.user, password, wait);
				let shell = match timeout_at(deadline, login).await {
					Ok(shell) => shell,
					Err(_) => Err(timed_out("no SSH login")),
				};
				// A server at its flood limit drops new connections during the
				// login, without saying why.
				if options.flood.limit(options.rate_limit).is_some()
					&& matches!(shell, Err(Error::Ssh(russh::Error::Disconnect)))
				{
					debug!(addr = %cfg.addr, "SSH login dropped, maybe by the flood protection; waiting");
					options.flood.hold(SSH_DROP_WAIT);
				}
				let shell = shell?;
				debug!(
					fingerprint = ?shell.fingerprint,
					addr = %cfg.addr,
					conn = %options.label,
					"ssh query connected"
				);
				let (r, w) = split(shell.stream);
				let (client, events) =
					LineClient::spawn_with_guard(r, w, options.clone(), shell.session);
				let client = Self::Line(client);
				// SSH already authenticated; only select the server.
				timeout_at(deadline, client.use_server(cfg))
					.await
					.map_err(|_| timed_out("no answer to selecting the server"))??;
				Ok((client, Some(events)))
			}
			Transport::Http => {
				let base = if cfg.addr.contains("://") {
					cfg.addr.clone()
				} else {
					format!("http://{}", cfg.addr)
				};
				let mut client =
					HttpClient::new(&base, cfg.secret.clone(), cfg.server_id.unwrap_or(1))?;
				if let Some(port) = cfg.server_port {
					// Resolve the port to a server id once.
					let servers = client.send(&Command::new("serverlist")).await?;
					let sid = servers
						.iter()
						.find(|r| r.parse::<u16>("virtualserver_port") == Some(port))
						.and_then(|r| r.parse("virtualserver_id"))
						.ok_or_else(|| {
							Error::Protocol(format!("no virtual server on port {port}"))
						})?;
					client.set_sid(sid);
				}
				Ok((Self::Http(client), None))
			}
		}
	}

	async fn login_and_use(&self, cfg: &Connect) -> Result<()> {
		if let Some(password) = &cfg.secret {
			self.send(
				&Command::new("login")
					.arg("client_login_name", &cfg.user)
					.arg("client_login_password", password),
			)
			.await?;
		}
		self.use_server(cfg).await
	}

	async fn use_server(&self, cfg: &Connect) -> Result<()> {
		let cmd = match (cfg.server_port, cfg.server_id) {
			(Some(port), _) => Command::new("use").arg("port", port),
			(None, sid) => Command::new("use").arg("sid", sid.unwrap_or(1)),
		};
		self.send(&cmd).await.map(|_| ())
	}

	pub async fn send(&self, cmd: &Command) -> Result<Vec<Row>> {
		match self {
			QueryClient::Line(c) => c.send(cmd).await,
			QueryClient::Http(c) => c.send(cmd).await,
		}
	}

	/// End the session politely (servers otherwise keep SSH query clients
	/// around until they time out). HTTP has no session to end.
	pub async fn quit(&self) {
		if let QueryClient::Line(c) = self {
			// The server closes the connection instead of answering.
			let _ = tokio::time::timeout(
				std::time::Duration::from_secs(2),
				c.send(&Command::new("quit")),
			)
			.await;
		}
	}

	/// The connection is gone (HTTP has none).
	pub fn is_closed(&self) -> bool {
		match self {
			QueryClient::Line(c) => c.is_closed(),
			QueryClient::Http(_) => false,
		}
	}

	/// Resolves when the connection is gone (never for HTTP).
	pub async fn closed(&self) {
		match self {
			QueryClient::Line(c) => c.closed().await,
			QueryClient::Http(_) => std::future::pending().await,
		}
	}

	/// Whether the transport delivers `notify*` events.
	pub fn has_events(&self) -> bool {
		matches!(self, QueryClient::Line(_))
	}

	/// Our own client id (`whoami`).
	pub async fn own_client_id(&self) -> Result<u16> {
		let rows = self.send(&Command::new("whoami")).await?;
		rows.first()
			.and_then(|r| r.parse("client_id"))
			.ok_or_else(|| Error::Protocol("whoami without client_id".into()))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A server that accepts connections and never says anything.
	async fn silent_server() -> String {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap().to_string();
		tokio::spawn(async move {
			let mut open = Vec::new();
			while let Ok((stream, _)) = listener.accept().await {
				open.push(stream);
			}
		});
		addr
	}

	fn connect(transport: Transport, addr: &str) -> Connect {
		Connect {
			transport,
			addr: addr.into(),
			user: "serveradmin".into(),
			secret: Some("pw".into()),
			server_port: None,
			server_id: None,
			line: LineOptions { connect_timeout: Duration::from_millis(300), ..Default::default() },
		}
	}

	async fn assert_times_out(cfg: &Connect) {
		let started = std::time::Instant::now();
		match QueryClient::connect(cfg).await {
			Err(Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::TimedOut),
			Err(e) => panic!("{e}"),
			Ok(_) => panic!("connected to a silent server"),
		}
		assert!(started.elapsed() < Duration::from_secs(5));
	}

	#[tokio::test]
	async fn a_silent_server_does_not_hold_up_the_login() {
		let addr = silent_server().await;
		// In the SSH handshake.
		assert_times_out(&connect(Transport::Ssh, &addr)).await;
		// At `login` and `use`, which have a longer timeout of their own.
		assert_times_out(&connect(Transport::Raw, &addr)).await;
	}

	#[tokio::test]
	async fn a_long_flood_wait_fails_the_connect_at_once() {
		let addr = silent_server().await;
		let cfg = connect(Transport::Ssh, &addr);
		cfg.line.flood.hold(Duration::from_secs(600));
		let started = std::time::Instant::now();
		assert!(matches!(QueryClient::connect(&cfg).await, Err(Error::Io(_))));
		assert!(started.elapsed() < Duration::from_millis(100));
	}

	#[test]
	fn waits_grow_and_follow_the_server() {
		let cfg = connect(Transport::Raw, "h:1");
		let secs = |attempt| cfg.retry_delay(attempt).as_secs();
		assert_eq!([secs(1), secs(3), secs(5), secs(6), secs(60)], [1, 3, 10, 30, 30]);
		cfg.line.flood.hold(Duration::from_secs(600));
		assert!(cfg.retry_delay(1) > Duration::from_secs(590));
		let warned: Vec<u32> = (1..=30).filter(|a| attempt_worth_a_warning(*a)).collect();
		assert_eq!(warned, [1, 2, 3, 10, 20, 30]);
	}
}
