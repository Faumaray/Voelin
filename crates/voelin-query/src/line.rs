//! Client for the line-based transports (raw TCP and SSH).
//!
//! A background task owns the connection. Commands are sent one at a time
//! (the protocol has no request ids); `notify*` lines are forwarded to an
//! event channel whenever they arrive.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until};
use tracing::{Instrument, debug, info_span, trace, warn};

use crate::codec::{Command, Line, Notification, QueryError, Row, parse_line};
use crate::{Error, Result};

/// Waited on top of what the server asks for: its clock is not ours.
const FLOOD_MARGIN: Duration = Duration::from_millis(100);

/// TeamSpeak refuses the 10th command within 3 seconds from one address,
/// the SSH login included.
const SERVER_LIMIT: (u32, Duration) = (9, Duration::from_secs(3));

#[derive(Clone, Debug)]
pub struct LineOptions {
	/// Send `whoami` after this long without traffic (servers drop idle
	/// query connections after 300 s by default).
	pub keepalive: Duration,
	/// Flood protection: at most this many commands per window, counted in
	/// `flood`. `None` when the client IP is on the server's query
	/// allowlist (until the server refuses a command for flooding: then
	/// the default applies).
	pub rate_limit: Option<(u32, Duration)>,
	/// Give up on a command after this long, time in the queue included.
	/// A connection whose server leaves a command unanswered for twice as
	/// long is closed.
	pub timeout: Duration,
	/// Give up opening a connection after this long: connecting, logging in
	/// and selecting the server, waits for the flood protection included.
	pub connect_timeout: Duration,
	/// What the connection is for (`lookup`, `observer`, `relay 5`), in
	/// its log lines.
	pub label: String,
	/// Commands sent and waits the server asked for. The server counts
	/// commands per source address, so clones of these options (and the
	/// connections opened with them) share one count.
	pub flood: FloodGuard,
}

impl Default for LineOptions {
	fn default() -> Self {
		Self {
			keepalive: Duration::from_secs(180),
			rate_limit: Some(SERVER_LIMIT),
			timeout: Duration::from_secs(15),
			connect_timeout: Duration::from_secs(10),
			label: "query".into(),
			flood: FloodGuard::default(),
		}
	}
}

/// The flood protection's state, shared by every connection to one server:
/// when recent commands went out, and until when the server asked to wait.
#[derive(Clone, Debug, Default)]
pub struct FloodGuard(Arc<Mutex<FloodState>>);

#[derive(Debug, Default)]
struct FloodState {
	sent: VecDeque<Instant>,
	hold_until: Option<Instant>,
	/// The server refused a command although no limit was set: the address
	/// is not on its allowlist after all.
	limited: bool,
}

impl FloodState {
	/// `limit`, or the server's own when it turned out to apply.
	fn limit(&self, limit: Option<(u32, Duration)>) -> Option<(u32, Duration)> {
		limit.or(self.limited.then_some(SERVER_LIMIT))
	}

	/// When the next command may be sent.
	fn next_slot(&mut self, limit: Option<(u32, Duration)>, now: Instant) -> Instant {
		let held = self.hold_until.filter(|t| *t > now);
		self.hold_until = held;
		let Some((count, window)) = limit else { return held.unwrap_or(now) };
		while self.sent.front().is_some_and(|t| now.duration_since(*t) >= window) {
			self.sent.pop_front();
		}
		let slot = match self.sent.len().checked_sub(count as usize) {
			None => now,
			// Leave a little margin: the server's clock is not ours.
			Some(oldest) => self.sent[oldest] + window + Duration::from_millis(50),
		};
		held.map_or(slot, |held| held.max(slot))
	}
}

impl FloodGuard {
	fn lock(&self) -> std::sync::MutexGuard<'_, FloodState> {
		self.0.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Wait until a command may be sent under `limit`, and count it. Safe to
	/// cancel: the command is only counted when this returns.
	pub async fn acquire(&self, limit: Option<(u32, Duration)>) {
		loop {
			let slot = {
				let mut state = self.lock();
				let now = Instant::now();
				let limit = state.limit(limit);
				let slot = state.next_slot(limit, now);
				if slot <= now {
					if limit.is_some() {
						state.sent.push_back(now);
					}
					return;
				}
				slot
			};
			sleep_until(slot).await;
		}
	}

	/// The limit that applies: `limit`, or the server's own once it refused
	/// a command although `limit` was `None`.
	pub fn limit(&self, limit: Option<(u32, Duration)>) -> Option<(u32, Duration)> {
		self.lock().limit(limit)
	}

	/// Send nothing for `wait`, on any connection sharing this guard.
	pub fn hold(&self, wait: Duration) {
		let until = Instant::now() + wait;
		let mut state = self.lock();
		state.hold_until = Some(state.hold_until.map_or(until, |held| held.max(until)));
	}

	/// Keep to the server's limit from now on, also where none was set.
	/// Whether that is new.
	fn limit_from_now_on(&self) -> bool {
		!std::mem::replace(&mut self.lock().limited, true)
	}

	/// How long sending is still held, if it is.
	pub fn held_for(&self) -> Option<Duration> {
		let until = self.lock().hold_until?;
		until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())
	}
}

/// How long the server's flood protection asks to wait
/// (`extra_msg=please\swait\s1\sseconds`).
fn flood_wait(error: &QueryError) -> Duration {
	let seconds = error
		.extra_msg
		.as_deref()
		.and_then(|m| m.split(' ').find_map(|word| word.parse::<u64>().ok()))
		.unwrap_or(3);
	Duration::from_secs(seconds)
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
	/// [`LineOptions::label`].
	label: Arc<str>,
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
		let label = Arc::from(options.label.as_str());
		// Not a child of whatever opened the connection: it outlives that.
		let span = info_span!(parent: None, "query", conn = %options.label);
		tokio::spawn(
			async move {
				run(BufReader::new(reader), writer, rx, events_tx, options).await;
				drop(guard);
			}
			.instrument(span),
		);
		(Self { requests, timeout, label }, events)
	}

	/// Send a command and collect its data rows.
	pub async fn send(&self, cmd: &Command) -> Result<Vec<Row>> {
		self.send_line(cmd.to_line()).await
	}

	/// Send an already formatted command line.
	pub async fn send_line(&self, line: String) -> Result<Vec<Row>> {
		let (reply, rx) = oneshot::channel();
		let name = command_name(&line).to_owned();
		self.requests.send(Request { line, reply }).await.map_err(|_| Error::Closed)?;
		match tokio::time::timeout(self.timeout, rx).await {
			Ok(Ok(result)) => result,
			Ok(Err(_)) => Err(Error::Closed),
			Err(_) => {
				let timeout_s = self.timeout.as_secs();
				debug!(conn = %self.label, command = name, timeout_s, "no answer in time");
				Err(Error::Timeout)
			}
		}
	}

	pub fn is_closed(&self) -> bool {
		self.requests.is_closed()
	}

	/// Resolves when the connection is gone.
	pub async fn closed(&self) {
		self.requests.closed().await;
	}
}

/// The command of a line (`clientlist` of `clientlist -uid`).
fn command_name(line: &str) -> &str {
	line.split(' ').next().unwrap_or_default()
}

/// A line as it may appear in the log: never a login's password.
fn loggable(line: &str) -> &str {
	if command_name(line) == "login" { "login (credentials not logged)" } else { line }
}

/// A command on its way out.
struct Outgoing {
	line: String,
	/// `None` for the keepalive.
	reply: Option<Reply>,
	/// Already sent once and refused by the flood protection.
	retried: bool,
}

impl Outgoing {
	fn name(&self) -> &str {
		command_name(&self.line)
	}

	/// The caller gave up waiting.
	fn abandoned(&self) -> bool {
		self.reply.as_ref().is_some_and(oneshot::Sender::is_closed)
	}
}

/// A command sent and not answered yet.
struct Sent {
	command: Outgoing,
	at: Instant,
	rows: Vec<Row>,
}

/// Resolves when the caller of the queued command gives up.
async fn given_up(queued: &mut Option<Outgoing>) {
	match queued.as_mut().and_then(|c| c.reply.as_mut()) {
		Some(reply) => reply.closed().await,
		None => std::future::pending().await,
	}
}

async fn sleep_until_some(at: Option<Instant>) {
	match at {
		Some(at) => sleep_until(at).await,
		None => std::future::pending().await,
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
	let flood = options.flood.clone();
	let limit = options.rate_limit;
	// Taken from the queue, waiting for the flood protection.
	let mut queued: Option<Outgoing> = None;
	// Sent, waiting for the answer.
	let mut sent: Option<Sent> = None;
	let mut last_activity = Instant::now();
	let mut buf = Vec::new();

	loop {
		let idle = queued.is_none() && sent.is_none();
		let keepalive_at = last_activity + options.keepalive;
		let stalled_at = sent.as_ref().map(|s| s.at + options.timeout * 2);
		tokio::select! {
			read = reader.read_until(b'\n', &mut buf) => {
				match read {
					Ok(0) => {
						debug!("query connection closed by the server");
						break;
					}
					Err(error) => {
						debug!(%error, "query connection failed");
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
						if let Some(sent) = &mut sent {
							sent.rows.extend(rows);
						}
					}
					Line::Data(_) => {}
					Line::Error(e) => {
						let Some(Sent { command, at, rows }) = sent.take() else { continue };
						let elapsed_ms = (at.elapsed().as_secs_f64() * 10_000.0).round() / 10.0;
						trace!(command = command.name(), id = e.id, rows = rows.len(), elapsed_ms, "query answered");
						if e.is_flood() {
							// Every connection waits: the server counts them together,
							// and two more commands now would get the address banned.
							// What the server asks for is too short: it still counts
							// the commands of the last 3 s, the refused ones too.
							let wait = flood_wait(&e).max(SERVER_LIMIT.1);
							flood.hold(wait + FLOOD_MARGIN);
							if limit.is_none() && flood.limit_from_now_on() {
								warn!(
									"the server limits this address although it was taken to be on its query allowlist; keeping to 9 commands per 3 s from now on"
								);
							}
							// The keepalive did its job: the server answered.
							let resend = !command.retried && command.reply.is_some();
							warn!(
								command = command.name(),
								wait_ms = wait.as_millis() as u64,
								resend,
								"the server's flood protection refused a command; sending nothing until the wait is over"
							);
							if resend {
								queued = Some(Outgoing { retried: true, ..command });
								continue;
							}
						}
						let name = command.name().to_owned();
						let Some(reply) = command.reply else { continue };
						let result = if e.is_ok() || e.is_empty_result() {
							Ok(rows)
						} else {
							debug!(
								command = name,
								error = %e,
								elapsed_ms = at.elapsed().as_millis() as u64,
								"query command failed"
							);
							Err(e.into())
						};
						let _ = reply.send(result);
					}
				}
			}
			req = requests.recv(), if idle => {
				let Some(req) = req else { break };
				let command = Outgoing { line: req.line, reply: Some(req.reply), retried: false };
				if command.abandoned() {
					debug!(command = command.name(), "dropping a command its caller gave up on");
				} else {
					queued = Some(command);
				}
			}
			() = given_up(&mut queued) => {
				if let Some(command) = queued.take() {
					debug!(command = command.name(), "dropping a command its caller gave up on");
				}
			}
			() = flood.acquire(limit), if queued.is_some() && sent.is_none() => {
				let Some(command) = queued.take() else { continue };
				if command.abandoned() {
					debug!(command = command.name(), "dropping a command its caller gave up on");
					continue;
				}
				trace!(line = %loggable(&command.line), "query >");
				if let Err(e) = write_line(&mut writer, &command.line).await {
					if let Some(reply) = command.reply {
						let _ = reply.send(Err(e.into()));
					}
					break;
				}
				last_activity = Instant::now();
				sent = Some(Sent { command, at: last_activity, rows: Vec::new() });
			}
			_ = sleep_until(keepalive_at), if idle => {
				queued = Some(Outgoing { line: "whoami".into(), reply: None, retried: false });
			}
			() = sleep_until_some(stalled_at) => {
				let command = sent.as_ref().map(|s| s.command.name().to_owned()).unwrap_or_default();
				warn!(
					command,
					waited_s = (options.timeout * 2).as_secs(),
					"the server did not answer; closing the query connection"
				);
				break;
			}
		}
	}
	if let Some(Sent { command: Outgoing { reply: Some(reply), .. }, .. }) = sent {
		let _ = reply.send(Err(Error::Closed));
	}
	debug!("query connection task ended");
}

/// Write one command in one write: over SSH each write is a packet.
async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
	let mut bytes = Vec::with_capacity(line.len() + 1);
	bytes.extend_from_slice(line.as_bytes());
	bytes.push(b'\n');
	writer.write_all(&bytes).await?;
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
	async fn the_flood_guard_spaces_commands() {
		let limit = Some((2, Duration::from_secs(3)));
		let mut state = FloodState::default();
		let t0 = Instant::now();
		assert_eq!(state.next_slot(limit, t0), t0);
		state.sent.extend([t0, t0]);
		assert!(state.next_slot(limit, t0) >= t0 + Duration::from_secs(3));
		let later = t0 + Duration::from_secs(4);
		assert_eq!(state.next_slot(limit, later), later);
		// A wait the server asked for applies with and without a limit.
		state.hold_until = Some(later + Duration::from_secs(2));
		assert_eq!(state.next_slot(limit, later), later + Duration::from_secs(2));
		assert_eq!(state.next_slot(None, later), later + Duration::from_secs(2));
	}

	/// The commands a fake server got, and when.
	type Seen = Arc<Mutex<Vec<(String, Instant)>>>;

	/// A fake server that answers every command with `error id=0`, and
	/// with `refusals` (in order) for the first ones; records when each
	/// command arrived.
	fn answering_server(stream: tokio::io::DuplexStream, refusals: Vec<&'static str>) -> Seen {
		let seen = Arc::new(Mutex::new(Vec::new()));
		let log = seen.clone();
		tokio::spawn(async move {
			let (r, mut w) = tokio::io::split(stream);
			let mut lines = BufReader::new(r).lines();
			let mut refusals = refusals.into_iter();
			while let Ok(Some(line)) = lines.next_line().await {
				log.lock().unwrap().push((line, Instant::now()));
				let answer = refusals.next().unwrap_or("error id=0 msg=ok");
				if w.write_all(format!("{answer}\n\r").as_bytes()).await.is_err() {
					return;
				}
			}
		});
		seen
	}

	fn client(options: &LineOptions) -> (LineClient, Seen) {
		client_refused(options, Vec::new())
	}

	fn client_refused(options: &LineOptions, refusals: Vec<&'static str>) -> (LineClient, Seen) {
		let (client_io, server_io) = duplex(4096);
		let seen = answering_server(server_io, refusals);
		let (r, w) = tokio::io::split(client_io);
		let (client, _events) = LineClient::spawn(r, w, options.clone());
		(client, seen)
	}

	const FLOOD: &str =
		"error id=524 msg=client\\sis\\sflooding extra_msg=please\\swait\\s2\\sseconds";

	#[tokio::test(start_paused = true)]
	async fn connections_share_the_flood_limit() {
		// TeamSpeak counts the commands of all connections from one address.
		let options =
			LineOptions { rate_limit: Some((2, Duration::from_secs(3))), ..Default::default() };
		let (a, seen_a) = client(&options);
		let (b, seen_b) = client(&options.clone());
		let t0 = Instant::now();
		a.send(&Command::new("whoami")).await.unwrap();
		a.send(&Command::new("whoami")).await.unwrap();
		b.send(&Command::new("version")).await.unwrap();
		let at = |seen: &Seen, i: usize| seen.lock().unwrap()[i].1;
		assert_eq!(at(&seen_a, 1), t0);
		assert!(at(&seen_b, 0) >= t0 + Duration::from_secs(3), "the third command waited");
		// Options built separately count separately.
		let (c, seen_c) = client(&LineOptions {
			rate_limit: Some((2, Duration::from_secs(3))),
			..Default::default()
		});
		let t1 = Instant::now();
		c.send(&Command::new("whoami")).await.unwrap();
		assert_eq!(at(&seen_c, 0), t1);
	}

	#[tokio::test(start_paused = true)]
	async fn a_flood_refusal_holds_every_connection_then_resends_once() {
		let options = LineOptions { rate_limit: None, ..Default::default() };
		let (a, seen_a) = client_refused(&options, vec![FLOOD]);
		let (b, seen_b) = client(&options);
		let t0 = Instant::now();
		let first = tokio::spawn(async move { a.send(&Command::new("clientlist")).await });
		// While the server's wait lasts, the other connection sends nothing.
		tokio::time::sleep(Duration::from_millis(100)).await;
		b.send(&Command::new("whoami")).await.unwrap();
		assert!(first.await.unwrap().is_ok(), "resent after the wait");
		// At least the server's 3 s window, longer than it asked for.
		let wait = Duration::from_millis(3100);
		let seen_a = seen_a.lock().unwrap().clone();
		assert_eq!(seen_a.len(), 2);
		assert_eq!(seen_a[0].0, "clientlist");
		assert!(seen_a[1].1 >= t0 + wait);
		assert!(seen_b.lock().unwrap()[0].1 >= t0 + wait);

		// Refused again: the caller gets the error, and sending still waits.
		let (c, seen_c) = client_refused(&options, vec![FLOOD, FLOOD]);
		match c.send(&Command::new("clientlist")).await {
			Err(Error::Query(e)) => assert!(e.is_flood()),
			other => panic!("{other:?}"),
		}
		assert_eq!(seen_c.lock().unwrap().len(), 2);
		assert!(options.flood.held_for().is_some_and(|d| d > Duration::from_secs(2)));
	}

	#[tokio::test(start_paused = true)]
	async fn a_refusal_without_a_limit_brings_the_servers_limit() {
		// Taken to be on the allowlist, and it is not.
		let options = LineOptions { rate_limit: None, ..Default::default() };
		let (a, seen) = client_refused(&options, vec![FLOOD]);
		for _ in 0..10 {
			a.send(&Command::new("whoami")).await.unwrap();
		}
		let seen = seen.lock().unwrap().clone();
		// Refused, resent, then 8 more at once and the 9th after 3 s.
		assert_eq!(seen.len(), 11);
		let resent = seen[1].1;
		assert!(seen[9].1 - resent < Duration::from_secs(1));
		assert!(seen[10].1 - resent >= Duration::from_secs(3));
	}

	#[tokio::test(start_paused = true)]
	async fn commands_whose_caller_gave_up_are_not_sent() {
		let options =
			LineOptions { rate_limit: None, timeout: Duration::from_secs(1), ..Default::default() };
		let (a, seen) = client(&options);
		options.flood.hold(Duration::from_secs(5));
		assert!(matches!(a.send(&Command::new("clientlist")).await, Err(Error::Timeout)));
		tokio::time::sleep(Duration::from_secs(5)).await;
		a.send(&Command::new("whoami")).await.unwrap();
		let sent: Vec<String> = seen.lock().unwrap().iter().map(|(l, _)| l.clone()).collect();
		assert_eq!(sent, ["whoami"]);
	}

	#[tokio::test(start_paused = true)]
	async fn a_server_that_stops_answering_is_left() {
		let (client_io, server_io) = duplex(4096);
		// Reads commands and never answers.
		tokio::spawn(async move {
			let mut lines = BufReader::new(server_io).lines();
			while let Ok(Some(_)) = lines.next_line().await {}
		});
		let (r, w) = tokio::io::split(client_io);
		let options =
			LineOptions { rate_limit: None, timeout: Duration::from_secs(1), ..Default::default() };
		let (client, _events) = LineClient::spawn(r, w, options);
		assert!(matches!(client.send(&Command::new("whoami")).await, Err(Error::Timeout)));
		assert!(!client.is_closed());
		tokio::time::sleep(Duration::from_secs(2)).await;
		assert!(client.is_closed());
		assert!(matches!(client.send(&Command::new("whoami")).await, Err(Error::Closed)));
	}

	#[test]
	fn the_wait_comes_from_the_refusal() {
		let refusal = |extra: Option<&str>| QueryError {
			id: 524,
			msg: "client is flooding".into(),
			extra_msg: extra.map(Into::into),
			failed_permid: None,
		};
		assert_eq!(flood_wait(&refusal(Some("please wait 600 seconds"))).as_secs(), 600);
		assert_eq!(flood_wait(&refusal(None)).as_secs(), 3);
		assert_eq!(
			loggable("login client_login_name=a client_login_password=secret").find("secret"),
			None
		);
		assert_eq!(loggable("whoami"), "whoami");
	}

	/// Records each write it gets.
	struct Writes(Vec<Vec<u8>>);

	impl AsyncWrite for Writes {
		fn poll_write(
			mut self: std::pin::Pin<&mut Self>,
			_: &mut std::task::Context<'_>,
			buf: &[u8],
		) -> std::task::Poll<std::io::Result<usize>> {
			self.0.push(buf.to_vec());
			std::task::Poll::Ready(Ok(buf.len()))
		}

		fn poll_flush(
			self: std::pin::Pin<&mut Self>,
			_: &mut std::task::Context<'_>,
		) -> std::task::Poll<std::io::Result<()>> {
			std::task::Poll::Ready(Ok(()))
		}

		fn poll_shutdown(
			self: std::pin::Pin<&mut Self>,
			_: &mut std::task::Context<'_>,
		) -> std::task::Poll<std::io::Result<()>> {
			std::task::Poll::Ready(Ok(()))
		}
	}

	#[tokio::test]
	async fn a_command_goes_out_in_one_write() {
		let mut writes = Writes(Vec::new());
		write_line(&mut writes, "clientlist -uid").await.unwrap();
		assert_eq!(writes.0, [b"clientlist -uid\n".to_vec()]);
	}
}
