//! Transport-independent query client.

use tokio::io::split;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::codec::{Command, Notification, Row};
use crate::http::HttpClient;
use crate::line::{LineClient, LineOptions};
use crate::{Error, Result};

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
		match cfg.transport {
			Transport::Raw => {
				let stream = TcpStream::connect(&cfg.addr).await?;
				stream.set_nodelay(true)?;
				let (r, w) = stream.into_split();
				let (client, events) = LineClient::spawn(r, w, cfg.line.clone());
				let client = Self::Line(client);
				client.login_and_use(cfg).await?;
				Ok((client, Some(events)))
			}
			Transport::Ssh => {
				let password = cfg.secret.as_deref().unwrap_or_default();
				let shell = crate::ssh::connect(&cfg.addr, &cfg.user, password).await?;
				tracing::debug!(fingerprint = ?shell.fingerprint, addr = %cfg.addr, "ssh query connected");
				let (r, w) = split(shell.stream);
				let (client, events) =
					LineClient::spawn_with_guard(r, w, cfg.line.clone(), shell.session);
				let client = Self::Line(client);
				// SSH already authenticated; only select the server.
				client.use_server(cfg).await?;
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
