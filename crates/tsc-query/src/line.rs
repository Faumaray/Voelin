//! Client for the line-based transports (raw TCP and SSH).
//!
//! A background task owns the connection. Commands are sent one at a time
//! (the protocol has no request ids); `notify*` lines are forwarded to an
//! event channel whenever they arrive.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};
use tracing::{debug, trace};

use crate::codec::{Command, Line, Notification, Row, parse_line};
use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct LineOptions {
	/// Send `whoami` after this long without traffic (servers drop idle
	/// query connections after 300 s by default).
	pub keepalive: Duration,
	/// Flood protection: at most `rate_commands` per `rate_window`. `None`
	/// when the client IP is on the server's query allowlist.
	pub rate_limit: Option<(u32, Duration)>,
	/// Give up on a command after this long.
	pub timeout: Duration,
}

impl Default for LineOptions {
	fn default() -> Self {
		Self {
			keepalive: Duration::from_secs(180),
			// TeamSpeak's default: 10 commands per 3 seconds.
			rate_limit: Some((10, Duration::from_secs(3))),
			timeout: Duration::from_secs(15),
		}
	}
}

type Reply = oneshot::Sender<Result<Vec<Row>>>;

struct Request {
	line: String,
	reply: Reply,
}

/// Handle to a line-based query connection. Cheap to clone.
#[derive(Clone)]
pub struct LineClient {
	requests: mpsc::Sender<Request>,
	timeout: Duration,
}

impl LineClient {
	/// Start the connection task. Returns the client and the event stream.
	pub fn spawn<R, W>(
		reader: R,
		writer: W,
		options: LineOptions,
	) -> (Self, mpsc::UnboundedReceiver<Notification>)
	where
		R: AsyncRead + Unpin + Send + 'static,
		W: AsyncWrite + Unpin + Send + 'static,
	{
		Self::spawn_with_guard(reader, writer, options, ())
	}

	/// Like [`LineClient::spawn`], keeping `guard` alive as long as the
	/// connection task runs (e.g. an SSH session handle).
	pub fn spawn_with_guard<R, W, G>(
		reader: R,
		writer: W,
		options: LineOptions,
		guard: G,
	) -> (Self, mpsc::UnboundedReceiver<Notification>)
	where
		R: AsyncRead + Unpin + Send + 'static,
		W: AsyncWrite + Unpin + Send + 'static,
		G: Send + 'static,
	{
		let (requests, rx) = mpsc::channel(64);
		let (events_tx, events) = mpsc::unbounded_channel();
		let timeout = options.timeout;
		tokio::spawn(async move {
			run(BufReader::new(reader), writer, rx, events_tx, options).await;
			drop(guard);
		});
		(Self { requests, timeout }, events)
	}

	/// Send a command and collect its data rows.
	pub async fn send(&self, cmd: &Command) -> Result<Vec<Row>> {
		self.send_line(cmd.to_line()).await
	}

	/// Send an already formatted command line.
	pub async fn send_line(&self, line: String) -> Result<Vec<Row>> {
		let (reply, rx) = oneshot::channel();
		self.requests.send(Request { line, reply }).await.map_err(|_| Error::Closed)?;
		match tokio::time::timeout(self.timeout, rx).await {
			Ok(Ok(result)) => result,
			Ok(Err(_)) => Err(Error::Closed),
			Err(_) => Err(Error::Timeout),
		}
	}

	pub fn is_closed(&self) -> bool {
		self.requests.is_closed()
	}
}

/// Token bucket for the query flood protection.
struct RateLimiter {
	limit: Option<(u32, Duration)>,
	sent: VecDeque<Instant>,
}

impl RateLimiter {
	/// When the next command may be sent.
	fn next_slot(&mut self, now: Instant) -> Instant {
		let Some((count, window)) = self.limit else { return now };
		while self.sent.front().is_some_and(|t| now.duration_since(*t) >= window) {
			self.sent.pop_front();
		}
		if self.sent.len() < count as usize {
			now
		} else {
			// Leave a little margin: the server's clock is not ours.
			self.sent[0] + window + Duration::from_millis(50)
		}
	}

	fn record(&mut self, t: Instant) {
		if self.limit.is_some() {
			self.sent.push_back(t);
		}
	}
}

async fn run<R, W>(
	mut reader: BufReader<R>,
	mut writer: W,
	mut requests: mpsc::Receiver<Request>,
	events: mpsc::UnboundedSender<Notification>,
	options: LineOptions,
) where
	R: AsyncRead + Unpin,
	W: AsyncWrite + Unpin,
{
	let mut limiter = RateLimiter { limit: options.rate_limit, sent: VecDeque::new() };
	// The request being answered and the rows collected for it so far.
	let mut pending: Option<(Reply, Vec<Row>)> = None;
	let mut keepalive_pending = false;
	let mut last_activity = Instant::now();
	let mut buf = Vec::new();

	loop {
		let can_send = pending.is_none() && !keepalive_pending;
		let keepalive_at = last_activity + options.keepalive;
		tokio::select! {
			read = reader.read_until(b'\n', &mut buf) => {
				match read {
					Ok(0) | Err(_) => {
						debug!("query connection closed");
						break;
					}
					Ok(_) => {}
				}
				let text = String::from_utf8_lossy(&buf).into_owned();
				buf.clear();
				trace!(line = %text.trim(), "query <");
				match parse_line(&text) {
					Line::Empty => {}
					Line::Notify(n) => {
						let _ = events.send(n);
					}
					// Response data always has key=value pairs; lines without
					// any `=` are the welcome banner, which may arrive after the
					// first command was sent.
					Line::Data(rows) if text.contains('=') => {
						if let Some((_, collected)) = &mut pending {
							collected.extend(rows);
						}
					}
					Line::Data(_) => {}
					Line::Error(e) => {
						if keepalive_pending && pending.is_none() {
							keepalive_pending = false;
						} else if let Some((reply, rows)) = pending.take() {
							let result = if e.is_ok() || e.is_empty_result() {
								Ok(rows)
							} else {
								Err(e.into())
							};
							let _ = reply.send(result);
						}
					}
				}
			}
			req = requests.recv(), if can_send => {
				let Some(req) = req else { break };
				let slot = limiter.next_slot(Instant::now());
				if slot > Instant::now() {
					sleep_until(slot).await;
				}
				limiter.record(Instant::now());
				trace!(line = %req.line, "query >");
				if let Err(e) = write_line(&mut writer, &req.line).await {
					let _ = req.reply.send(Err(e.into()));
					break;
				}
				last_activity = Instant::now();
				pending = Some((req.reply, Vec::new()));
			}
			_ = sleep_until(keepalive_at), if can_send => {
				limiter.record(Instant::now());
				if write_line(&mut writer, "whoami").await.is_err() {
					break;
				}
				last_activity = Instant::now();
				keepalive_pending = true;
			}
		}
	}
	if let Some((reply, _)) = pending {
		let _ = reply.send(Err(Error::Closed));
	}
	debug!("query connection task ended");
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
	writer.write_all(line.as_bytes()).await?;
	writer.write_all(b"\n").await?;
	writer.flush().await
}

#[cfg(test)]
mod tests {
	use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

	use super::*;

	/// A scripted fake server: answers each command from `script` in order.
	async fn fake_server(
		stream: tokio::io::DuplexStream,
		script: Vec<(&'static str, &'static str)>,
	) {
		let (r, mut w) = tokio::io::split(stream);
		let mut r = BufReader::new(r);
		w.write_all(b"TS3\n\rWelcome to the TeamSpeak 3 ServerQuery interface.\n\r").await.unwrap();
		for (expect, answer) in script {
			let mut line = String::new();
			r.read_line(&mut line).await.unwrap();
			assert_eq!(line.trim_end(), expect);
			w.write_all(answer.as_bytes()).await.unwrap();
		}
	}

	#[tokio::test]
	async fn request_response_and_events() {
		let (client_io, server_io) = duplex(4096);
		let server = tokio::spawn(fake_server(
			server_io,
			vec![
				(
					"login client_login_name=serveradmin client_login_password=pw",
					"error id=0 msg=ok\n\r",
				),
				(
					"clientlist -uid",
					"clid=1 client_nickname=A|clid=2 client_nickname=B\\sC\n\r\
				 notifycliententerview clid=3 client_nickname=D\n\r\
				 error id=0 msg=ok\n\r",
				),
				("use sid=9", "error id=1024 msg=invalid\\sserverID\n\r"),
				("channellist", "error id=1281 msg=database\\sempty\\sresult\\sset\n\r"),
			],
		));
		let (r, w) = tokio::io::split(client_io);
		let (client, mut events) =
			LineClient::spawn(r, w, LineOptions { rate_limit: None, ..Default::default() });

		let login = Command::new("login")
			.arg("client_login_name", "serveradmin")
			.arg("client_login_password", "pw");
		assert!(client.send(&login).await.unwrap().is_empty());
		let rows = client.send(&Command::new("clientlist").flag("uid")).await.unwrap();
		assert_eq!(rows.len(), 2);
		assert_eq!(rows[1].get("client_nickname"), Some("B C"));
		let event = events.recv().await.unwrap();
		assert_eq!(event.name, "notifycliententerview");
		match client.send(&Command::new("use").arg("sid", 9)).await {
			Err(Error::Query(e)) => assert_eq!(e.id, 1024),
			other => panic!("{other:?}"),
		}
		// "empty result set" is an empty list, not an error.
		assert!(client.send(&Command::new("channellist")).await.unwrap().is_empty());
		server.await.unwrap();
	}

	#[tokio::test(start_paused = true)]
	async fn rate_limiter_spaces_commands() {
		let mut limiter =
			RateLimiter { limit: Some((2, Duration::from_secs(3))), sent: VecDeque::new() };
		let t0 = Instant::now();
		assert_eq!(limiter.next_slot(t0), t0);
		limiter.record(t0);
		limiter.record(t0);
		assert!(limiter.next_slot(t0) >= t0 + Duration::from_secs(3));
		let later = t0 + Duration::from_secs(4);
		assert_eq!(limiter.next_slot(later), later);
	}
}
