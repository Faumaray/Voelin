//! The gateway source: presence and relayed chat through `tsgw`, and
//! everything else a gateway offers (history, pins, reactions, topics,
//! events, the stream directory, the activity feed, administration).
//!
//! [`GatewayClient`] (from `voelin-gateway-proto`, feature `client`) has one
//! async method per request and delivers pushes as [`Push`];
//! [`GatewayClient::capabilities`] says which features the gateway and user
//! have. The data types (`HistoryQuery`, `PinInfo`, `EventSpec`,
//! `StreamEntry`, …) are in `voelin_gateway_proto`. [`connect`] logs in
//! with the user's TeamSpeak identity.
//!
//! # In the engine
//!
//! [`crate::Command::ObserveGateway`] starts a session's gateway: invisible
//! presence, relayed chat, chat history ([`crate::history`]). The session
//! then takes [`crate::Command::Gateway`] with a [`GatewayRequest`] and
//! reports answers and pushes as [`crate::Event::Gateway`] with a
//! [`GatewayUpdate`]:
//!
//! - [`GatewayUpdate::Connected`] after login, with the capabilities
//!   ([`voelin_gateway_proto::feature`]; hide what is missing);
//!   [`GatewayUpdate::Capabilities`] when they change;
//!   [`GatewayUpdate::Disconnected`] when the gateway is gone.
//! - Each request is answered with its update (e.g. `Pins` for
//!   [`GatewayRequest::Pins`]), [`GatewayUpdate::Done`] for requests without
//!   data, or [`GatewayUpdate::Failed`] (gateway error code and message).
//! - Pushes: pins, reactions and topic changes of open chats, event
//!   changes and reminders, stream directory and activity changes. The
//!   engine subscribes to events, streams and activity when the gateway
//!   has them (the `Subscribe*` requests turn that off and on).
//! - Messages the gateway returns (posts, pins, topic history) and message
//!   changes (pins, reactions) are stored and also emitted as
//!   [`crate::Event::ChatHistory`], so the chat view updates by local id.
//!   Message ids in requests are the gateway's
//!   ([`crate::history::HistoryMessage::remote_id`]).
//! - Our own stream (TeamSpeak 6) registers itself in the directory when it
//!   goes live and the gateway has `streams` ([`GatewayUpdate::StreamRegistered`]),
//!   and is removed when it ends. The directory is also where the session's
//!   streams look for streams that started before we joined, next to the
//!   server's `requeststreaminfo`.
//!
//! A gateway that cannot be reached, or cannot log us in yet (the server
//! does not know the identity before voice connected once; its server is
//! restarting), is tried again by itself: after 1, 2, 5, 10, 20, then every
//! 30 s, give or take 30 %, and at once when voice connects. Only a refused
//! login (signature, security level, ban, not allowed) ends it. A connection
//! is pinged every 30 s and left after 60 s without an answer. All of it is
//! logged (target `voelin_core::gateway`), never shown.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::future::{Fuse, FusedFuture, FutureExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
pub use voelin_gateway_proto::client::{ClientError, GatewayClient, Push};
use voelin_gateway_proto::client::{Login, Progress};
use voelin_gateway_proto::{
	Action, ActivityEntry, ClientMsg, ConfigEntry, ErrorCode, EventInfo, EventQuery, EventSpec,
	HistoryEntry, HistoryQuery, PermRule, PermRuleInfo, PinInfo, RsvpStatus, StreamEntry,
	StreamSpec, TopicInfo, UserRef,
};
use voelin_model::{ChannelId, ChatTarget, Presence};
use voelin_store::WriteOutcome;

use crate::Event;
use crate::history::{ChatCtx, HistoryMessage, HistorySource, gateway_message, store_target};

pub(crate) enum GatewayCmd {
	OpenChat(ChatTarget),
	CloseChat(ChatTarget),
	SendChat(ChatTarget, String),
	/// Waiting for the next attempt: make it now (voice connected, which
	/// makes the identity known to the server; the server was selected
	/// again). Nothing while connected.
	RetryNow,
	Stop,
}

pub(crate) enum GatewayEvent {
	/// Logged in at the URL; requests go through the client.
	Connected(GatewayClient, String),
	Presence(Box<Presence>),
	/// A push other than presence.
	Push(Box<Push>),
	/// The gateway could not be reached, or the connection was lost; the
	/// next attempt follows by itself.
	Retrying(String),
	/// Stopped, or the gateway refused the login (the reason).
	Disconnected(Option<String>),
}

/// Connect to a gateway (`ws://…/v1` or `wss://…/v1`) and log in with a
/// TeamSpeak identity.
pub async fn connect(
	url: &str,
	identity: &tsclientlib::Identity,
) -> Result<(GatewayClient, mpsc::UnboundedReceiver<Push>), ClientError> {
	connect_watched(url, identity, &Progress::default()).await
}

/// [`connect`], telling `progress` each step (for the log).
async fn connect_watched(
	url: &str,
	identity: &tsclientlib::Identity,
	progress: &Progress,
) -> Result<(GatewayClient, mpsc::UnboundedReceiver<Push>), ClientError> {
	let login = Login::Identity { key: identity.key().clone(), key_offset: identity.counter() };
	voelin_gateway_proto::client::connect_watched(url, login, progress).await
}

/// Observe through the first of `urls` (best first, as published) that
/// logs in.
pub(crate) async fn run(
	mut urls: Vec<String>,
	identity: tsclientlib::Identity,
	mut commands: mpsc::UnboundedReceiver<GatewayCmd>,
	events: mpsc::UnboundedSender<GatewayEvent>,
) {
	// A gateway that is away (down, restarting, a network change, a proxy
	// in front of it that fails) or cannot log us in yet (it does not know
	// the identity before voice connected once, its server is restarting)
	// is tried again until the session stops observing; only a refused
	// login ends it. A round tries every URL in turn without waiting; only
	// a round that fails waits.
	debug!(?urls, "gateway: starting");
	let mut chats = Chats::default();
	let mut failures = 0;
	let mut told = Told::default();
	// Why the last failed round failed.
	let mut last_round: Option<String> = None;
	// Attempts since observing started or the connection was lost.
	let (mut attempts, mut since) = (0, Instant::now());
	let reason = 'observe: loop {
		let mut lost = None;
		let mut index = 0;
		while index < urls.len() {
			attempts += 1;
			let url = urls[index].clone();
			let next = urls.get(index + 1).map(String::as_str);
			debug!(%url, attempt = attempts, "gateway: trying");
			let at = Try { url: &url, number: attempts, since, next };
			match attempt(&at, &identity, &mut commands, &events, &mut chats, &mut told).await {
				Ended::Stopped => break 'observe None,
				Ended::Refused(reason) => break 'observe Some(reason),
				Ended::Lost { reason, logged_in } => {
					lost = Some(reason);
					if logged_in {
						// It worked: it is tried first again, after a wait.
						urls[..=index].rotate_right(1);
						failures = 0;
						(attempts, since) = (0, Instant::now());
						break;
					}
					index += 1;
				}
			}
		}
		let Some(reason) = lost else { break None };
		failures += 1;
		let delay = retry_delay(failures, jitter());
		// Past the longest wait, the same cause again is not news (a server
		// only observed, never joined, cannot log us in at all).
		let wait = delay.as_secs_f64();
		if failures as usize > RETRY_SECONDS.len() && last_round.as_ref() == Some(&reason) {
			debug!(failures, error = %reason, "gateway: trying again in {wait:.1} s");
		} else {
			info!(failures, error = %reason, "gateway: trying again in {wait:.1} s");
		}
		last_round = Some(reason.clone());
		let _ = events.send(GatewayEvent::Retrying(reason));
		if !chats.wait(delay, &mut commands).await {
			break None;
		}
	};
	debug!(?reason, "gateway: stopped");
	let _ = events.send(GatewayEvent::Disconnected(reason));
}

/// How one connection to the gateway ended.
enum Ended {
	/// The session stopped observing.
	Stopped,
	/// The gateway refused the login: trying again would not help.
	Refused(String),
	/// Not reached, not logged in yet, or lost (after logging in): try
	/// again.
	Lost { reason: String, logged_in: bool },
}

/// Login errors that trying again does not change: the identity or the
/// user is not allowed. Anything else (the gateway's server away, an
/// identity voice has not made known yet, too many logins) passes.
fn refused(code: ErrorCode) -> bool {
	matches!(
		code,
		ErrorCode::AuthFailed | ErrorCode::LevelTooLow | ErrorCode::Banned | ErrorCode::Forbidden
	)
}

/// One attempt of [`run`], for the log.
struct Try<'a> {
	url: &'a str,
	/// Since observing started, or the connection was lost; 1 first.
	number: u32,
	/// When observing started, or the connection was lost.
	since: Instant,
	/// The URL tried next in this round if this one fails.
	next: Option<&'a str>,
}

/// What was logged per URL: each cause once while it lasts (until a login
/// works), not at every attempt of a long absence.
#[derive(Default)]
struct Told(Vec<(String, String)>);

impl Told {
	/// Whether `url` failing with `cause` is news.
	fn news(&mut self, url: &str, cause: &str) -> bool {
		if self.0.iter().any(|(u, c)| u == url && c == cause) {
			return false;
		}
		self.0.retain(|(u, _)| u != url);
		self.0.push((url.to_owned(), cause.to_owned()));
		true
	}
}

/// One attempt to reach a gateway and log in (TCP, TLS, the WebSocket
/// upgrade, the login's own queries at the gateway) gives up after this
/// long, so a gateway that never answers does not hold up the next one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// The waits after failed rounds in a row, the last one repeated.
const RETRY_SECONDS: [u64; 6] = [1, 2, 5, 10, 20, 30];

/// The wait before the next attempt after `failures` failed rounds in a
/// row, give or take 30 % (`jitter`, -1 to 1): clients that lost the same
/// gateway do not all come back at the same moment.
fn retry_delay(failures: u32, jitter: f64) -> Duration {
	let index = (failures.max(1) as usize - 1).min(RETRY_SECONDS.len() - 1);
	Duration::from_secs(RETRY_SECONDS[index]).mul_f64(1.0 + 0.3 * jitter.clamp(-1.0, 1.0))
}

/// A number from -1 to 1, another each time: the standard library's
/// randomly keyed hasher over a counter and the clock.
fn jitter() -> f64 {
	use std::hash::{BuildHasher, Hasher};
	static COUNT: AtomicU64 = AtomicU64::new(0);
	let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
	hasher.write_u64(COUNT.fetch_add(1, Ordering::Relaxed));
	let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
	hasher.write_u128(now.unwrap_or_default().as_nanos());
	(hasher.finish() >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
}

/// Asking a gateway that is still connecting to try again at once, at most
/// this often ([`Observed::nudge`]): selecting servers does not hammer a
/// struggling gateway.
const NUDGE_EVERY: Duration = Duration::from_secs(5);

/// What a session's gateway observes with, so that asking again for the
/// same ([`crate::Command::ObserveGateway`], e.g. the server selected again)
/// does not start it over, at most makes it try again sooner.
pub(crate) struct Observed {
	urls: Vec<String>,
	/// The identity's public key.
	key: String,
	/// The identity's counter, which the login sends: a raised security
	/// level keeps the key and changes only this.
	counter: u64,
	nudged: Option<Instant>,
}

impl Observed {
	pub(crate) fn new(urls: &[String], identity: &tsclientlib::Identity) -> Self {
		Self {
			urls: urls.to_vec(),
			key: identity.key().to_pub().to_ts(),
			counter: identity.counter(),
			nudged: None,
		}
	}

	/// Whether this is what is observed with.
	pub(crate) fn same(&self, urls: &[String], identity: &tsclientlib::Identity) -> bool {
		self.urls == urls
			&& self.counter == identity.counter()
			&& self.key == identity.key().to_pub().to_ts()
	}

	/// Whether to try again now ([`GatewayCmd::RetryNow`]): not twice
	/// within [`NUDGE_EVERY`].
	pub(crate) fn nudge(&mut self) -> bool {
		self.nudge_at(Instant::now())
	}

	fn nudge_at(&mut self, now: Instant) -> bool {
		if self.nudged.is_some_and(|at| now.saturating_duration_since(at) < NUDGE_EVERY) {
			return false;
		}
		self.nudged = Some(now);
		true
	}
}

/// Messages written while the gateway is away, at most this many, go out
/// when it is back.
const MAX_UNSENT: usize = 64;

/// What a new connection restores: the chats open at the gateway, and
/// messages written while it was away.
#[derive(Default)]
struct Chats {
	open: Vec<ChatTarget>,
	unsent: Vec<(ChatTarget, String)>,
}

impl Chats {
	/// Note a chat command; it goes out when connected.
	fn note(&mut self, cmd: GatewayCmd) {
		match cmd {
			GatewayCmd::OpenChat(target) => {
				if !self.open.contains(&target) {
					self.open.push(target);
				}
			}
			GatewayCmd::CloseChat(target) => self.open.retain(|open| *open != target),
			GatewayCmd::SendChat(target, text) => {
				if self.unsent.len() < MAX_UNSENT {
					self.unsent.push((target, text));
				}
			}
			GatewayCmd::RetryNow | GatewayCmd::Stop => {}
		}
	}

	/// Carry out a chat command on a connected gateway.
	fn apply(&mut self, client: &GatewayClient, cmd: GatewayCmd) -> Result<(), ClientError> {
		match cmd {
			GatewayCmd::OpenChat(target) => {
				if !self.open.contains(&target) {
					self.open.push(target.clone());
				}
				client.send(ClientMsg::OpenChat { target })
			}
			GatewayCmd::CloseChat(target) => {
				self.open.retain(|open| *open != target);
				client.send(ClientMsg::CloseChat { target })
			}
			GatewayCmd::SendChat(target, text) => client.send(ClientMsg::SendChat { target, text }),
			GatewayCmd::RetryNow | GatewayCmd::Stop => Ok(()),
		}
	}

	/// Open the chats again on a new connection and send what waited.
	fn restore(&mut self, client: &GatewayClient) -> Result<(), ClientError> {
		for target in &self.open {
			client.send(ClientMsg::OpenChat { target: target.clone() })?;
		}
		for (target, text) in std::mem::take(&mut self.unsent) {
			client.send(ClientMsg::SendChat { target, text })?;
		}
		Ok(())
	}

	/// Wait `delay` before the next attempt, noting the commands that come
	/// meanwhile, or until told to try now; false when the session stops
	/// observing.
	async fn wait(
		&mut self,
		delay: Duration,
		commands: &mut mpsc::UnboundedReceiver<GatewayCmd>,
	) -> bool {
		let sleep = tokio::time::sleep(delay);
		tokio::pin!(sleep);
		loop {
			tokio::select! {
				() = &mut sleep => return true,
				cmd = commands.recv() => match cmd {
					None | Some(GatewayCmd::Stop) => return false,
					Some(GatewayCmd::RetryNow) => {
						debug!("gateway: trying again now");
						return true;
					}
					Some(cmd) => self.note(cmd),
				},
			}
		}
	}
}

/// Features whose pushes for open chats need [`ClientMsg::Enable`].
const CHAT_EXTENSIONS: [&str; 4] = [
	voelin_gateway_proto::feature::HISTORY,
	voelin_gateway_proto::feature::PINS,
	voelin_gateway_proto::feature::REACTIONS,
	voelin_gateway_proto::feature::TOPICS,
];

/// One connection: log in, then relay until it ends.
async fn attempt(
	at: &Try<'_>,
	identity: &tsclientlib::Identity,
	commands: &mut mpsc::UnboundedReceiver<GatewayCmd>,
	events: &mpsc::UnboundedSender<GatewayEvent>,
	chats: &mut Chats,
	told: &mut Told,
) -> Ended {
	let url = at.url;
	let lost = |reason: String| Ended::Lost { reason, logged_in: false };
	let progress = Progress::default();
	let started = Instant::now();
	let result =
		tokio::time::timeout(CONNECT_TIMEOUT, connect_watched(url, identity, &progress)).await;
	let elapsed_ms = started.elapsed().as_millis() as u64;
	let (stage, peer) = (progress.stage(), progress.peer_addr());
	let (client, mut pushes) = match result {
		Ok(Ok(connected)) => connected,
		Ok(Err(ClientError::Gateway { code, message })) if refused(code) => {
			warn!(%url, ?code, %message, "gateway refused the login");
			return Ended::Refused(format!("gateway refused login: {code:?}: {message}"));
		}
		// Observing starts before voice: on a server we never joined, the
		// first login cannot work. Voice connecting makes it try again.
		Ok(Err(ClientError::Gateway { code: ErrorCode::UnknownIdentity, message })) => {
			if told.news(url, "unknown identity") {
				info!(%url, elapsed_ms, "the server does not know this identity yet (voice connects it); trying again");
			} else {
				debug!(%url, elapsed_ms, "the server does not know this identity yet");
			}
			return lost(format!("UnknownIdentity: {message}"));
		}
		Ok(Err(ClientError::Gateway { code, message })) => {
			if told.news(url, &format!("{code:?}")) {
				warn!(%url, ?code, %message, elapsed_ms, next = ?at.next, "gateway did not log us in; trying again");
			} else {
				debug!(%url, ?code, %message, elapsed_ms, "gateway still does not log us in");
			}
			return lost(format!("{code:?}: {message}"));
		}
		Ok(Err(e)) => {
			let reason = e.to_string();
			if told.news(url, &reason) {
				warn!(%url, %stage, ?peer, steps = %progress, elapsed_ms, error = %reason, next = ?at.next, "gateway unreachable");
			} else {
				debug!(%url, %stage, steps = %progress, elapsed_ms, error = %reason, "gateway still unreachable");
			}
			return lost(reason);
		}
		Err(_) => {
			let reason = format!("no answer within {} s ({stage})", CONNECT_TIMEOUT.as_secs());
			if told.news(url, &reason) {
				warn!(%url, %stage, ?peer, steps = %progress, elapsed_ms, next = ?at.next, "gateway unreachable: {reason}");
			} else {
				debug!(%url, %stage, steps = %progress, elapsed_ms, "gateway still unreachable: {reason}");
			}
			return lost(reason);
		}
	};
	*told = Told::default();
	info!(
		%url,
		attempt = at.number,
		connect_ms = elapsed_ms,
		since_start_ms = at.since.elapsed().as_millis() as u64,
		server = %client.info().server_name,
		?peer,
		steps = %progress,
		capabilities = ?client.capabilities(),
		"gateway connected"
	);
	let connected = Instant::now();
	let reason = match relay(url, &client, &mut pushes, commands, events, chats, connected).await {
		Ok(Ended::Lost { reason, .. }) => reason,
		Ok(ended) => {
			debug!(%url, connected_for_s = connected.elapsed().as_secs(), "gateway: left");
			return ended;
		}
		Err(e) => e.to_string(),
	};
	warn!(%url, connected_for_s = connected.elapsed().as_secs(), error = %reason, "gateway connection lost");
	Ended::Lost { reason, logged_in: true }
}

/// A connection is checked this often while nothing else asks the gateway
/// anything ([`ClientMsg::Ping`])…
const PING_EVERY: Duration = Duration::from_secs(30);
/// …and taken as lost without an answer within this long (the gateway
/// answers in turn, after requests that can take a while).
const PING_TIMEOUT: Duration = Duration::from_secs(60);

/// Whether the gateway still answers: a ping, answered in time.
async fn alive(client: &GatewayClient) -> Result<(), String> {
	match tokio::time::timeout(PING_TIMEOUT, client.request(ClientMsg::Ping)).await {
		// Any answer will do, also an error from an older gateway.
		Ok(Ok(_) | Err(ClientError::Gateway { .. })) => Ok(()),
		Ok(Err(e)) => Err(e.to_string()),
		Err(_) => Err(format!("no answer to a ping within {} s", PING_TIMEOUT.as_secs())),
	}
}

async fn relay(
	url: &str,
	client: &GatewayClient,
	pushes: &mut mpsc::UnboundedReceiver<Push>,
	commands: &mut mpsc::UnboundedReceiver<GatewayCmd>,
	events: &mpsc::UnboundedSender<GatewayEvent>,
	chats: &mut Chats,
	connected: Instant,
) -> Result<Ended, ClientError> {
	let lost = |reason: String| Ended::Lost { reason, logged_in: true };
	// Messages with their gateway ids, pins, reactions and topics of open
	// chats (older gateways answer `unknown_type` as a push; ignored).
	if CHAT_EXTENSIONS.iter().any(|f| client.has(f)) {
		client.send(ClientMsg::Enable { features: Vec::new() })?;
	}
	let _ = events.send(GatewayEvent::Connected(client.clone(), url.to_owned()));
	client.subscribe_presence()?;
	chats.restore(client)?;
	let mut presence = Presence::default();
	let mut synced = false;
	// A connection that died without closing (a network change, sleep, a
	// NAT that forgot it) shows only when the gateway stops answering.
	let start = tokio::time::Instant::now() + PING_EVERY;
	let mut keepalive = tokio::time::interval_at(start, PING_EVERY);
	keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	let ping = Fuse::terminated();
	tokio::pin!(ping);

	loop {
		tokio::select! {
			push = pushes.recv() => match push {
				None | Some(Push::Disconnected(None)) => {
					return Ok(lost("the gateway closed the connection".into()));
				}
				Some(Push::Disconnected(Some(reason))) => return Ok(lost(reason)),
				Some(Push::PresenceSnapshot(snapshot)) => {
					presence = Presence::from_snapshot(snapshot);
					if !synced {
						synced = true;
						info!(
							%url,
							channels = presence.channels.len(),
							clients = presence.clients.len(),
							since_connect_ms = connected.elapsed().as_millis() as u64,
							"gateway presence"
						);
					}
					let _ = events.send(GatewayEvent::Presence(Box::new(presence.clone())));
				}
				Some(Push::PresenceDelta(delta)) => {
					presence.apply(&delta);
					let _ = events.send(GatewayEvent::Presence(Box::new(presence.clone())));
				}
				// Answers to requests sent without waiting.
				Some(Push::Other(_) | Push::Error { code: ErrorCode::UnknownType, .. }) => {}
				// The gateway forgot the login (restarted): log in again.
				Some(Push::Error { code: ErrorCode::NotAuthenticated, message }) => {
					return Ok(lost(format!("gateway session lost: {message}")));
				}
				Some(push) => {
					let _ = events.send(GatewayEvent::Push(Box::new(push)));
				}
			},
			cmd = commands.recv() => match cmd {
				None | Some(GatewayCmd::Stop) => return Ok(Ended::Stopped),
				Some(cmd) => chats.apply(client, cmd)?,
			},
			_ = keepalive.tick(), if ping.is_terminated() => ping.set(alive(client).fuse()),
			answer = &mut ping => {
				if let Err(reason) = answer {
					return Ok(lost(reason));
				}
			}
		}
	}
}

/// Something to ask a session's gateway ([`crate::Command::Gateway`]).
/// Message ids are the gateway's. Serialized in snake case, e.g.
/// `{"pin":{"message_id":7}}`, `"config_list"`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayRequest {
	/// Report the capabilities again ([`GatewayUpdate::Capabilities`]).
	Capabilities,
	/// What the user may do, server-wide or in `channel`
	/// ([`GatewayUpdate::Permissions`]).
	Permissions {
		#[serde(default)]
		channel: Option<ChannelId>,
	},
	/// Post into a chat, optionally into a topic ([`GatewayUpdate::Posted`]).
	Post {
		target: ChatTarget,
		text: String,
		#[serde(default)]
		topic: Option<i64>,
	},
	Pin {
		message_id: i64,
	},
	Unpin {
		message_id: i64,
	},
	/// [`GatewayUpdate::Pins`].
	Pins {
		target: ChatTarget,
	},
	React {
		message_id: i64,
		emoji: String,
	},
	Unreact {
		message_id: i64,
		emoji: String,
	},
	/// Who reacted with `emoji` ([`GatewayUpdate::Reactors`]).
	Reactors {
		message_id: i64,
		emoji: String,
	},
	/// Start a topic, from a message or standalone ([`GatewayUpdate::Topic`]).
	CreateTopic {
		target: ChatTarget,
		title: String,
		#[serde(default)]
		message_id: Option<i64>,
	},
	/// Rename or (un)archive a topic ([`GatewayUpdate::Topic`]).
	UpdateTopic {
		topic_id: i64,
		#[serde(default)]
		title: Option<String>,
		#[serde(default)]
		archived: Option<bool>,
	},
	/// [`GatewayUpdate::Topics`], most recently active first.
	Topics {
		target: ChatTarget,
		#[serde(default)]
		include_archived: bool,
	},
	/// A topic's messages before gateway message `before` (the latest
	/// without), `limit` of them (`chat.history_page` without)
	/// ([`GatewayUpdate::TopicHistory`]).
	TopicHistory {
		target: ChatTarget,
		topic: i64,
		#[serde(default)]
		before: Option<i64>,
		#[serde(default)]
		limit: Option<u32>,
	},
	/// [`GatewayUpdate::Event`].
	CreateEvent {
		event: EventSpec,
	},
	/// [`GatewayUpdate::Event`].
	UpdateEvent {
		id: i64,
		event: EventSpec,
	},
	DeleteEvent {
		id: i64,
	},
	/// One event with its attendees ([`GatewayUpdate::Event`]).
	GetEvent {
		id: i64,
	},
	/// [`GatewayUpdate::Events`], by start time.
	Events {
		#[serde(default)]
		query: EventQuery,
	},
	/// Answer an invitation; `None` withdraws the answer ([`GatewayUpdate::Event`]).
	Rsvp {
		event_id: i64,
		#[serde(default)]
		status: Option<RsvpStatus>,
	},
	/// Event changes and reminders as pushes (on by default).
	SubscribeEvents {
		on: bool,
	},
	/// The stream directory ([`GatewayUpdate::Streams`]).
	Streams,
	/// Directory changes as pushes (on by default).
	SubscribeStreams {
		on: bool,
	},
	/// Add a stream to the directory (our own stream registers itself;
	/// [`GatewayUpdate::StreamRegistered`]).
	RegisterStream {
		stream: StreamSpec,
	},
	UpdateStream {
		stream_id: String,
		#[serde(default)]
		title: Option<String>,
		#[serde(default)]
		viewers: Option<u32>,
	},
	UnregisterStream {
		stream_id: String,
	},
	/// Newest first, entries with an id below `before` ([`GatewayUpdate::Activity`]).
	Activity {
		#[serde(default)]
		before: Option<i64>,
		#[serde(default)]
		limit: Option<u32>,
	},
	/// New activity as pushes (on by default).
	SubscribeActivity {
		on: bool,
	},
	/// Administration (capability `admin`): [`GatewayUpdate::Config`].
	ConfigList,
	/// [`GatewayUpdate::ConfigValue`].
	ConfigGet {
		key: String,
	},
	/// Store a runtime value at the gateway ([`GatewayUpdate::ConfigValue`]).
	ConfigSet {
		key: String,
		value: serde_json::Value,
	},
	/// Drop the runtime value ([`GatewayUpdate::ConfigValue`]).
	ConfigReset {
		key: String,
	},
	/// Read the gateway's file again ([`GatewayUpdate::Config`]).
	ConfigReload,
	/// [`GatewayUpdate::PermRules`].
	PermList,
	PermSet {
		action: Action,
		rule: PermRule,
	},
	PermReset {
		action: Action,
	},
}

impl GatewayRequest {
	/// The request's name, as in [`GatewayUpdate::Done`] and [`GatewayUpdate::Failed`].
	pub fn name(&self) -> &'static str {
		match self {
			GatewayRequest::Capabilities => "capabilities",
			GatewayRequest::Permissions { .. } => "permissions",
			GatewayRequest::Post { .. } => "post",
			GatewayRequest::Pin { .. } => "pin",
			GatewayRequest::Unpin { .. } => "unpin",
			GatewayRequest::Pins { .. } => "pins",
			GatewayRequest::React { .. } => "react",
			GatewayRequest::Unreact { .. } => "unreact",
			GatewayRequest::Reactors { .. } => "reactors",
			GatewayRequest::CreateTopic { .. } => "create_topic",
			GatewayRequest::UpdateTopic { .. } => "update_topic",
			GatewayRequest::Topics { .. } => "topics",
			GatewayRequest::TopicHistory { .. } => "topic_history",
			GatewayRequest::CreateEvent { .. } => "create_event",
			GatewayRequest::UpdateEvent { .. } => "update_event",
			GatewayRequest::DeleteEvent { .. } => "delete_event",
			GatewayRequest::GetEvent { .. } => "get_event",
			GatewayRequest::Events { .. } => "events",
			GatewayRequest::Rsvp { .. } => "rsvp",
			GatewayRequest::SubscribeEvents { .. } => "subscribe_events",
			GatewayRequest::Streams => "streams",
			GatewayRequest::SubscribeStreams { .. } => "subscribe_streams",
			GatewayRequest::RegisterStream { .. } => "register_stream",
			GatewayRequest::UpdateStream { .. } => "update_stream",
			GatewayRequest::UnregisterStream { .. } => "unregister_stream",
			GatewayRequest::Activity { .. } => "activity",
			GatewayRequest::SubscribeActivity { .. } => "subscribe_activity",
			GatewayRequest::ConfigList => "config_list",
			GatewayRequest::ConfigGet { .. } => "config_get",
			GatewayRequest::ConfigSet { .. } => "config_set",
			GatewayRequest::ConfigReset { .. } => "config_reset",
			GatewayRequest::ConfigReload => "config_reload",
			GatewayRequest::PermList => "perm_list",
			GatewayRequest::PermSet { .. } => "perm_set",
			GatewayRequest::PermReset { .. } => "perm_reset",
		}
	}
}

/// A pinned message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Pin {
	/// The message as stored ([`HistoryMessage::id`] is the local id).
	pub message: HistoryMessage,
	pub by: UserRef,
	pub ts_ms: i64,
}

/// What a session's gateway reports ([`crate::Event::Gateway`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayUpdate {
	/// Logged in.
	Connected {
		/// Where: the first of the published URLs that logged in.
		url: String,
		gateway_id: String,
		server_uid: String,
		server_name: String,
		/// Our unique id on the server.
		uid: String,
		/// See [`voelin_gateway_proto::feature`].
		capabilities: Vec<String>,
	},
	/// The features available to us changed.
	Capabilities {
		capabilities: Vec<String>,
	},
	/// The gateway connection ended (nothing of it is available).
	Disconnected {
		reason: Option<String>,
	},
	/// A request without data succeeded (e.g. `pin`, `react`, `subscribe_*`).
	Done {
		request: String,
	},
	/// A request failed; `code` is the gateway's (`None`: not connected,
	/// connection lost, unexpected answer).
	Failed {
		request: String,
		code: Option<ErrorCode>,
		message: String,
	},
	Permissions {
		channel: Option<ChannelId>,
		actions: Vec<Action>,
	},
	/// Our post, as stored.
	Posted {
		message: HistoryMessage,
	},
	Pins {
		target: ChatTarget,
		pins: Vec<Pin>,
	},
	/// Push (chat open) or our pin.
	Pinned {
		target: ChatTarget,
		pin: Pin,
	},
	/// Push (chat open).
	Unpinned {
		target: ChatTarget,
		message_id: i64,
		by: UserRef,
	},
	/// Push (chat open): `user` added or removed `emoji`; `count` is the new total.
	Reaction {
		target: ChatTarget,
		message_id: i64,
		emoji: String,
		user: UserRef,
		added: bool,
		count: u32,
	},
	Reactors {
		message_id: i64,
		emoji: String,
		users: Vec<UserRef>,
	},
	/// A topic: created, changed (also as a push), or new activity.
	Topic {
		topic: TopicInfo,
	},
	Topics {
		target: ChatTarget,
		topics: Vec<TopicInfo>,
	},
	/// A page of a topic's messages, oldest first, stored.
	TopicHistory {
		target: ChatTarget,
		topic: i64,
		messages: Vec<HistoryMessage>,
		has_more: bool,
	},
	/// An event: the answer to create, update, get and rsvp, or a push
	/// (created or changed, RSVP counts, going live).
	Event {
		event: EventInfo,
	},
	Events {
		events: Vec<EventInfo>,
	},
	/// Push.
	EventDeleted {
		id: i64,
	},
	/// Push at the gateway's reminder times before an event starts (0: it starts).
	EventReminder {
		event: EventInfo,
		starts_in_ms: i64,
	},
	/// The whole directory (answer to `streams`, and after subscribing).
	Streams {
		streams: Vec<StreamEntry>,
	},
	/// Push.
	StreamStarted {
		stream: StreamEntry,
	},
	/// Push.
	StreamUpdated {
		stream: StreamEntry,
	},
	/// Push.
	StreamEnded {
		id: String,
		reason: String,
	},
	/// A stream we registered (our own stream when it goes live).
	StreamRegistered {
		stream: StreamEntry,
	},
	/// Newest first.
	Activity {
		entries: Vec<ActivityEntry>,
		has_more: bool,
	},
	/// Push.
	ActivityAdded {
		entry: ActivityEntry,
	},
	Config {
		entries: Vec<ConfigEntry>,
	},
	ConfigValue {
		entry: ConfigEntry,
	},
	PermRules {
		rules: Vec<PermRuleInfo>,
	},
}

/// Carry out a request and report the answer.
pub(crate) async fn execute(ctx: ChatCtx, client: GatewayClient, request: GatewayRequest) {
	let name = request.name();
	let update = match answer(&ctx, &client, request).await {
		Ok(Some(update)) => update,
		Ok(None) => GatewayUpdate::Done { request: name.into() },
		Err(e) => {
			GatewayUpdate::Failed { request: name.into(), code: e.code(), message: e.to_string() }
		}
	};
	ctx.emit(Event::Gateway { session: ctx.session, update });
}

/// Store gateway messages; as stored (local id 0 if storing failed).
async fn stored(ctx: &ChatCtx, entries: &[HistoryEntry]) -> Vec<HistoryMessage> {
	match ctx.store_entries(entries).await {
		Ok(written) => written.into_iter().map(|w| w.message.into()).collect(),
		Err(e) => {
			tracing::warn!("cannot store gateway messages: {e}");
			entries.iter().map(HistoryMessage::unstored).collect()
		}
	}
}

async fn answer(
	ctx: &ChatCtx,
	client: &GatewayClient,
	request: GatewayRequest,
) -> Result<Option<GatewayUpdate>, ClientError> {
	use GatewayRequest as R;
	use GatewayUpdate as U;
	Ok(Some(match request {
		R::Capabilities => U::Capabilities { capabilities: client.capabilities() },
		R::Permissions { channel } => {
			U::Permissions { channel, actions: client.permissions(channel).await? }
		}
		R::Post { target, text, topic } => {
			let entry = client.post(target.clone(), text, topic).await?;
			let message = stored(ctx, std::slice::from_ref(&entry)).await.remove(0);
			ctx.emit_batch(&target, vec![message.clone()], HistorySource::Live, false);
			U::Posted { message }
		}
		R::Pin { message_id } => return client.pin(message_id).await.map(|()| None),
		R::Unpin { message_id } => return client.unpin(message_id).await.map(|()| None),
		R::Pins { target } => {
			let pins = client.pins(target.clone()).await?;
			let entries: Vec<HistoryEntry> = pins.iter().map(|p| p.entry.clone()).collect();
			let messages = stored(ctx, &entries).await;
			let pins = pins
				.into_iter()
				.zip(messages)
				.map(|(p, message)| Pin { message, by: p.by, ts_ms: p.ts_ms })
				.collect();
			U::Pins { target, pins }
		}
		R::React { message_id, emoji } => {
			return client.react(message_id, emoji).await.map(|()| None);
		}
		R::Unreact { message_id, emoji } => {
			return client.unreact(message_id, emoji).await.map(|()| None);
		}
		R::Reactors { message_id, emoji } => {
			let users = client.reactors(message_id, emoji.clone()).await?;
			U::Reactors { message_id, emoji, users }
		}
		R::CreateTopic { target, title, message_id } => {
			U::Topic { topic: client.create_topic(target, title, message_id).await? }
		}
		R::UpdateTopic { topic_id, title, archived } => {
			U::Topic { topic: client.update_topic(topic_id, title, archived).await? }
		}
		R::Topics { target, include_archived } => {
			let topics = client.topics(target.clone(), include_archived).await?;
			U::Topics { target, topics }
		}
		R::TopicHistory { target, topic, before, limit } => {
			let mut query = HistoryQuery::latest(target.clone(), limit.or(ctx.page_limit()));
			query.before = before;
			query.topic = Some(topic);
			let page = client.history(query).await?;
			let messages = stored(ctx, &page.messages).await;
			U::TopicHistory { target, topic, messages, has_more: page.has_more }
		}
		R::CreateEvent { event } => U::Event { event: client.create_event(event).await? },
		R::UpdateEvent { id, event } => U::Event { event: client.update_event(id, event).await? },
		R::DeleteEvent { id } => return client.delete_event(id).await.map(|()| None),
		R::GetEvent { id } => U::Event { event: client.event(id).await? },
		R::Events { query } => U::Events { events: client.events(query).await? },
		R::Rsvp { event_id, status } => U::Event { event: client.rsvp(event_id, status).await? },
		R::SubscribeEvents { on: true } => return client.subscribe_events().await.map(|()| None),
		R::SubscribeEvents { on: false } => {
			return client.unsubscribe_events().await.map(|()| None);
		}
		R::Streams => U::Streams { streams: client.streams().await? },
		R::SubscribeStreams { on: true } => {
			U::Streams { streams: client.subscribe_streams().await? }
		}
		R::SubscribeStreams { on: false } => {
			return client.unsubscribe_streams().await.map(|()| None);
		}
		R::RegisterStream { stream } => {
			U::StreamRegistered { stream: client.register_stream(stream).await? }
		}
		R::UpdateStream { stream_id, title, viewers } => {
			U::StreamUpdated { stream: client.update_stream(stream_id, title, viewers).await? }
		}
		R::UnregisterStream { stream_id } => {
			return client.unregister_stream(stream_id).await.map(|()| None);
		}
		R::Activity { before, limit } => {
			let (entries, has_more) = client.activity(before, limit).await?;
			U::Activity { entries, has_more }
		}
		R::SubscribeActivity { on: true } => {
			return client.subscribe_activity().await.map(|()| None);
		}
		R::SubscribeActivity { on: false } => {
			return client.unsubscribe_activity().await.map(|()| None);
		}
		R::ConfigList => U::Config { entries: client.config_list().await? },
		R::ConfigGet { key } => U::ConfigValue { entry: client.config_get(key).await? },
		R::ConfigSet { key, value } => {
			U::ConfigValue { entry: client.config_set(key, value).await? }
		}
		R::ConfigReset { key } => U::ConfigValue { entry: client.config_reset(key).await? },
		R::ConfigReload => U::Config { entries: client.config_reload().await? },
		R::PermList => U::PermRules { rules: client.perm_list().await? },
		R::PermSet { action, rule } => U::PermRules { rules: client.perm_set(action, rule).await? },
		R::PermReset { action } => U::PermRules { rules: client.perm_reset(action).await? },
	}))
}

/// Pushes about messages of open chats: stored, then reported, in the
/// order they arrived (the writer thread runs jobs in order).
impl ChatCtx {
	pub(crate) fn pinned(&self, target: ChatTarget, pin: PinInfo) {
		let mut entry = pin.entry.clone();
		entry.pinned = true;
		let ctx = self.clone();
		self.history.write(
			self.memory(),
			vec![gateway_message(&self.server_uid, &entry)],
			self.tolerance_ms(),
			move |result| {
				let message = match result.ok().and_then(|mut w| w.pop()) {
					Some(w) => {
						let changed = w.outcome != WriteOutcome::Unchanged;
						let message = HistoryMessage::from(w.message);
						if changed {
							ctx.emit_batch(
								&target,
								vec![message.clone()],
								HistorySource::Live,
								false,
							);
						}
						message
					}
					None => HistoryMessage::unstored(&entry),
				};
				let pin = Pin { message, by: pin.by, ts_ms: pin.ts_ms };
				let update = GatewayUpdate::Pinned { target, pin };
				ctx.emit(Event::Gateway { session: ctx.session, update });
			},
		);
	}

	pub(crate) fn unpinned(&self, target: ChatTarget, message_id: i64, by: UserRef) {
		let (uid, key, ctx) = (self.server_uid.clone(), store_target(&target), self.clone());
		self.history.run_then(
			self.memory(),
			move |s| s.set_pinned(&uid, &key, message_id, false),
			move |result| {
				ctx.emit_changed(result.ok().flatten());
				let update = GatewayUpdate::Unpinned { target, message_id, by };
				ctx.emit(Event::Gateway { session: ctx.session, update });
			},
		);
	}

	/// A reaction push; `own_uid` tells whether it was ours.
	pub(crate) fn reaction(&self, push: ReactionPush, own_uid: Option<&str>) {
		let ReactionPush { target, message_id, emoji, user, added, count } = push;
		let me = (own_uid == Some(user.uid.as_str())).then_some(added);
		let (uid, key, ctx) = (self.server_uid.clone(), store_target(&target), self.clone());
		let e = emoji.clone();
		self.history.run_then(
			self.memory(),
			move |s| s.set_reaction(&uid, &key, message_id, &e, count, me),
			move |result| {
				ctx.emit_changed(result.ok().flatten());
				let update =
					GatewayUpdate::Reaction { target, message_id, emoji, user, added, count };
				ctx.emit(Event::Gateway { session: ctx.session, update });
			},
		);
	}
}

/// The fields of [`Push::Reaction`].
pub(crate) struct ReactionPush {
	pub target: ChatTarget,
	pub message_id: i64,
	pub emoji: String,
	pub user: UserRef,
	pub added: bool,
	pub count: u32,
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use std::sync::atomic::AtomicUsize;

	use futures::{SinkExt, StreamExt};
	use tokio_tungstenite::tungstenite::Message;
	use voelin_gateway_proto::{Envelope, ServerMsg};

	use super::*;

	fn millis(d: Duration) -> u64 {
		(d.as_secs_f64() * 1000.0).round() as u64
	}

	#[test]
	fn retries_come_soon_then_every_30_s_spread() {
		let seconds: Vec<u64> = (0..=8).map(|n| retry_delay(n, 0.0).as_secs()).collect();
		assert_eq!(seconds, [1, 1, 2, 5, 10, 20, 30, 30, 30]);
		// Give or take 30 %.
		assert_eq!(millis(retry_delay(1, -1.0)), 700);
		assert_eq!(millis(retry_delay(1, 1.0)), 1300);
		assert_eq!(millis(retry_delay(6, 1.0)), 39_000);
		assert_eq!(millis(retry_delay(3, 7.0)), millis(retry_delay(3, 1.0)));
		let samples: Vec<f64> = (0..1000).map(|_| jitter()).collect();
		assert!(samples.iter().all(|j| (-1.0..1.0).contains(j)), "{samples:?}");
		assert!(samples.iter().any(|j| *j < -0.5) && samples.iter().any(|j| *j > 0.5));
	}

	#[test]
	fn the_same_observation_is_nudged_at_most_every_5_s() {
		let identity = tsclientlib::Identity::create();
		let urls = vec!["ws://gw.example.test:7788/v1".to_owned()];
		let mut observed = Observed::new(&urls, &identity);
		assert!(observed.same(&urls, &identity));
		assert!(!observed.same(&["wss://gw.example.test/v1".to_owned()], &identity));
		assert!(!observed.same(&urls, &tsclientlib::Identity::create()));
		// Its security level raised: logged in with the new counter.
		let raised = tsclientlib::Identity::new(identity.key().clone(), identity.counter() + 1);
		assert!(!observed.same(&urls, &raised));
		let now = Instant::now();
		assert!(observed.nudge_at(now));
		assert!(!observed.nudge_at(now + Duration::from_secs(4)));
		assert!(observed.nudge_at(now + NUDGE_EVERY));
		assert!(!observed.nudge_at(now + NUDGE_EVERY + Duration::from_secs(1)));
	}

	#[test]
	fn only_some_login_errors_are_final() {
		use ErrorCode as E;
		for code in [E::AuthFailed, E::LevelTooLow, E::Banned, E::Forbidden] {
			assert!(refused(code), "{code:?}");
		}
		for code in [
			E::Unavailable,
			E::Internal,
			E::RateLimited,
			E::UnknownIdentity,
			E::NotAuthenticated,
			E::BadRequest,
			E::Unknown,
		] {
			assert!(!refused(code), "{code:?}");
		}
	}

	#[tokio::test(start_paused = true)]
	async fn retry_now_ends_the_wait() {
		let (tx, mut rx) = mpsc::unbounded_channel();
		let mut chats = Chats::default();
		let start = tokio::time::Instant::now();
		// What comes meanwhile is noted; RetryNow ends the wait at once.
		tx.send(GatewayCmd::OpenChat(ChatTarget::Channel(1))).unwrap();
		tx.send(GatewayCmd::RetryNow).unwrap();
		assert!(chats.wait(Duration::from_secs(30), &mut rx).await);
		assert_eq!(start.elapsed(), Duration::ZERO);
		assert_eq!(chats.open, [ChatTarget::Channel(1)]);
		// Told 3 s into a wait of 30.
		let told = tokio::spawn(async move {
			tokio::time::sleep(Duration::from_secs(3)).await;
			tx.send(GatewayCmd::RetryNow).unwrap();
			tx
		});
		assert!(chats.wait(Duration::from_secs(30), &mut rx).await);
		assert_eq!(millis(start.elapsed()), 3000);
		let tx = told.await.unwrap();
		// Not told: the whole wait.
		assert!(chats.wait(Duration::from_secs(30), &mut rx).await);
		assert_eq!(millis(start.elapsed()), 33_000);
		tx.send(GatewayCmd::Stop).unwrap();
		assert!(!chats.wait(Duration::from_secs(30), &mut rx).await);
	}

	/// A logged-in client of a gateway on an in-memory connection that
	/// answers pings when `pings`; counts the pings.
	async fn gateway(
		pings: bool,
	) -> (GatewayClient, mpsc::UnboundedReceiver<Push>, Arc<AtomicUsize>) {
		let (near, far) = tokio::io::duplex(64 * 1024);
		let count = Arc::new(AtomicUsize::new(0));
		let counted = count.clone();
		tokio::spawn(async move {
			let mut ws = tokio_tungstenite::accept_async(far).await.unwrap();
			let hello = ServerMsg::Hello {
				gateway_id: "gw".into(),
				server_uid: "server".into(),
				server_name: "Server".into(),
				nonce: "n".into(),
				capabilities: Vec::new(),
			};
			let text = serde_json::to_string(&Envelope::new(hello)).unwrap();
			ws.send(Message::Text(text.into())).await.unwrap();
			while let Some(Ok(frame)) = ws.next().await {
				let Message::Text(text) = frame else { continue };
				let env: Envelope<ClientMsg> = serde_json::from_str(text.as_str()).unwrap();
				let reply = match env.msg {
					ClientMsg::Resume { .. } => ServerMsg::AuthOk {
						uid: "me".into(),
						token: "t".into(),
						token_expires: 0,
						capabilities: Vec::new(),
					},
					ClientMsg::Ping => {
						counted.fetch_add(1, Ordering::SeqCst);
						if !pings {
							continue;
						}
						ServerMsg::Pong
					}
					_ => ServerMsg::Ok,
				};
				let text = serde_json::to_string(&Envelope::with_id(env.id.unwrap_or(0), reply));
				if ws.send(Message::Text(text.unwrap().into())).await.is_err() {
					break;
				}
			}
		});
		let (ws, _) = tokio_tungstenite::client_async("ws://gw.test/v1", near).await.unwrap();
		let (client, pushes) = GatewayClient::start(ws, Login::Token("t".into())).await.unwrap();
		(client, pushes, count)
	}

	#[tokio::test(start_paused = true)]
	async fn a_gateway_that_stops_answering_is_left() {
		let (client, mut pushes, pings) = gateway(false).await;
		let (_tx, mut commands) = mpsc::unbounded_channel();
		let (events, _events) = mpsc::unbounded_channel();
		let start = tokio::time::Instant::now();
		let mut chats = Chats::default();
		let ended = relay(
			"ws://gw.test/v1",
			&client,
			&mut pushes,
			&mut commands,
			&events,
			&mut chats,
			Instant::now(),
		)
		.await
		.unwrap();
		let Ended::Lost { reason, logged_in: true } = ended else { panic!("not lost") };
		assert!(reason.contains("ping"), "{reason}");
		assert_eq!(millis(start.elapsed()), millis(PING_EVERY + PING_TIMEOUT));
		assert_eq!(pings.load(Ordering::SeqCst), 1);
	}

	#[tokio::test(start_paused = true)]
	async fn a_gateway_that_answers_pings_is_kept() {
		let (client, mut pushes, pings) = gateway(true).await;
		let (_tx, mut commands) = mpsc::unbounded_channel();
		let (events, _events) = mpsc::unbounded_channel();
		let mut chats = Chats::default();
		let relayed = tokio::time::timeout(
			// The 20th ping at 600 s, answered.
			Duration::from_secs(615),
			relay(
				"ws://gw.test/v1",
				&client,
				&mut pushes,
				&mut commands,
				&events,
				&mut chats,
				Instant::now(),
			),
		)
		.await;
		assert!(relayed.is_err(), "the connection was left");
		assert_eq!(pings.load(Ordering::SeqCst), 20);
	}
}
