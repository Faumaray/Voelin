//! The lookup query connection: logins, permission checks and server chat
//! posts go through it. When the TeamSpeak server drops it (a restart, a
//! network problem) it is opened again, and commands sent meanwhile wait
//! for it a little.

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{debug, error, info, warn};
use voelin_query::{Command, Connect, QueryClient, Row, Transport, attempt_worth_a_warning};

/// How long a command waits for a lost connection to come back.
const RECONNECT_WAIT: Duration = Duration::from_secs(5);

pub struct Lookup {
	connect: Connect,
	client: watch::Sender<QueryClient>,
	/// The session's nickname, set again on every new connection.
	nickname: Mutex<String>,
	/// The connection's client id while it is up: server chat from it is the
	/// gateway's own. Not kept across reconnects: after a server restart the
	/// old id may be a user's.
	own_id: Mutex<Option<u16>>,
}

impl Lookup {
	/// Log in, trying until it works, and keep the connection open from
	/// then on.
	pub async fn open(mut connect: Connect, nickname: String) -> Arc<Self> {
		connect.line.label = "lookup".into();
		let (client, own_id) = open_until_it_works(&connect, &nickname).await;
		let lookup = Arc::new(Self {
			connect,
			client: watch::channel(client).0,
			nickname: Mutex::new(nickname),
			own_id: Mutex::new(Some(own_id)),
		});
		tokio::spawn(keep_open(Arc::downgrade(&lookup)));
		lookup
	}

	/// The connection is up.
	pub fn is_up(&self) -> bool {
		!self.client.borrow().is_closed()
	}

	/// The connection's client id, while it is up.
	pub fn own_id(&self) -> Option<u16> {
		*self.own_id.lock().unwrap()
	}

	/// The id is the connection's own.
	pub fn is_own(&self, id: u16) -> bool {
		self.own_id() == Some(id)
	}

	/// Send a command; when the connection is lost on the way, once more
	/// on the new one. A failure is logged with the command's name.
	pub async fn send(&self, cmd: &Command) -> voelin_query::Result<Vec<Row>> {
		let result = self.send_quiet(cmd).await;
		if let Err(error) = &result {
			warn!(command = %cmd.name, %error, "ServerQuery command failed");
		}
		result
	}

	/// [`Lookup::send`] for callers that log a failure themselves.
	pub async fn send_quiet(&self, cmd: &Command) -> voelin_query::Result<Vec<Row>> {
		let mut current = self.client.subscribe();
		let client = current.borrow_and_update().clone();
		match client.send(cmd).await {
			Err(error) if lost(&error, &client) => {
				match tokio::time::timeout(RECONNECT_WAIT, current.changed()).await {
					Ok(Ok(())) => {
						let client = current.borrow_and_update().clone();
						client.send(cmd).await
					}
					_ => Err(error),
				}
			}
			result => result,
		}
	}

	/// The connection, after waiting a little for it if it is lost.
	pub async fn client(&self) -> QueryClient {
		let mut current = self.client.subscribe();
		let client = current.borrow_and_update().clone();
		if !client.is_closed() {
			return client;
		}
		match tokio::time::timeout(RECONNECT_WAIT, current.changed()).await {
			Ok(Ok(())) => current.borrow_and_update().clone(),
			_ => client,
		}
	}

	/// Rename the session, now and after reconnects.
	pub async fn set_nickname(&self, nickname: &str) {
		*self.nickname.lock().unwrap() = nickname.to_string();
		set_nickname(&self.client().await, nickname).await;
	}

	/// Log the address the server sees the gateway's queries come from: it
	/// counts the commands of each address for its flood protection, unless
	/// the address is in its `query_ip_allowlist.txt`.
	pub async fn log_address(&self, allowlisted: bool) {
		let Some(own) = self.own_id() else { return };
		let Ok(rows) = self.send(&Command::new("clientlist").flag("ip")).await else { return };
		let address = rows
			.iter()
			.find(|r| r.parse::<u16>("clid") == Some(own))
			.and_then(|r| r.get("connection_client_ip"));
		let Some(address) = address.filter(|a| !a.is_empty()) else {
			debug!("the server did not say where the gateway's queries come from");
			return;
		};
		let loopback = address.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
		match (allowlisted, loopback) {
			(true, _) => info!(
				address,
				"the TeamSpeak server sees the gateway's queries coming from this address"
			),
			(false, true) => info!(
				address,
				hint = "the server's query_ip_allowlist.txt has this address by default; if it still \
				        does, set query.allowlisted = true so the gateway stops slowing itself down",
				"the TeamSpeak server sees the gateway's queries coming from this address"
			),
			(false, false) => info!(
				address,
				hint = "add this address to the server's query_ip_allowlist.txt (read again within \
				        seconds) and set query.allowlisted = true; until then the server's flood \
				        protection limits the gateway",
				"the TeamSpeak server sees the gateway's queries coming from this address"
			),
		}
	}
}

/// The error means the connection is gone, not that the command failed.
fn lost(error: &voelin_query::Error, client: &QueryClient) -> bool {
	use voelin_query::Error;
	matches!(error, Error::Closed | Error::Io(_) | Error::Ssh(_)) || client.is_closed()
}

/// Wait for the connection to close, open it again, repeat.
async fn keep_open(lookup: Weak<Lookup>) {
	loop {
		let Some(client) = lookup.upgrade().map(|l| l.client.borrow().clone()) else { return };
		client.closed().await;
		let Some(lookup) = lookup.upgrade() else { return };
		let lost_at = Instant::now();
		*lookup.own_id.lock().unwrap() = None;
		warn!(addr = %lookup.connect.addr, "lookup query connection lost; reconnecting");
		let nickname = lookup.nickname.lock().unwrap().clone();
		let (client, own_id) = open_until_it_works(&lookup.connect, &nickname).await;
		*lookup.own_id.lock().unwrap() = Some(own_id);
		lookup.client.send_replace(client);
		info!(down_ms = lost_at.elapsed().as_millis() as u64, "lookup query connection restored");
	}
}

/// Open a connection, waiting longer after each failure.
async fn open_until_it_works(connect: &Connect, nickname: &str) -> (QueryClient, u16) {
	let started = Instant::now();
	let transport = match connect.transport {
		Transport::Ssh => "ssh",
		Transport::Raw => "raw",
		Transport::Http => "http",
	};
	let mut attempt = 0;
	loop {
		attempt += 1;
		match open_once(connect, nickname).await {
			Ok(opened) => {
				let elapsed_ms = started.elapsed().as_millis() as u64;
				info!(addr = %connect.addr, transport, attempt, elapsed_ms, "query login ok");
				return opened;
			}
			Err(e) => {
				let retry_in = connect.retry_delay(attempt);
				let retry_in_s = retry_in.as_secs_f32();
				let refused = matches!(&e, voelin_query::Error::SshAuth)
					|| matches!(&e, voelin_query::Error::Query(q) if q.id == ERR_INVALID_LOGIN);
				if refused {
					error!(
						addr = %connect.addr,
						user = %connect.user,
						attempt,
						retry_in_s,
						"the TeamSpeak server refused the query login; check query.user and the password"
					);
				} else if attempt_worth_a_warning(attempt) {
					warn!(
						addr = %connect.addr,
						error = %e,
						attempt,
						retry_in_s,
						"query login failed; trying again"
					);
				} else {
					debug!(
						addr = %connect.addr,
						error = %e,
						attempt,
						retry_in_s,
						"query login failed; trying again"
					);
				}
				tokio::time::sleep(retry_in).await;
			}
		}
	}
}

/// `login` with a wrong name or password.
const ERR_INVALID_LOGIN: u32 = 520;

async fn open_once(connect: &Connect, nickname: &str) -> voelin_query::Result<(QueryClient, u16)> {
	let (client, _) = QueryClient::connect(connect).await?;
	let own_id = client.own_client_id().await?;
	set_nickname(&client, nickname).await;
	Ok((client, own_id))
}

/// The name is cosmetic: failing keeps the server-assigned one.
async fn set_nickname(client: &QueryClient, nickname: &str) {
	let _ = client.send(&Command::new("clientupdate").arg("client_nickname", nickname)).await;
}
