//! Keep a server's presence up to date from one query session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{broadcast, watch};
use tokio::time::{Instant, sleep, sleep_until};
use tracing::{debug, info, warn};
use voelin_model::{ChatMessage, Presence, PresenceDelta, PresenceSnapshot};
use voelin_query::{Command, Connect, Notification, QueryClient, attempt_worth_a_warning};

use crate::convert::{
	channel_from_row, client_from_row, delta_from_notification, is_text_message, update_channel,
};

#[derive(Clone, Debug)]
pub struct ObserverConfig {
	/// Reconnects wait [`Connect::retry_delay`].
	pub connect: Connect,
	/// Full resync interval. Query sessions receive no `notifyclientupdated`,
	/// so mute/away changes are only noticed by re-reading the client list.
	pub poll_interval: Duration,
	/// Also deliver server chat (`event=textserver`).
	pub server_chat: bool,
}

impl ObserverConfig {
	pub fn new(connect: Connect) -> Self {
		Self { connect, poll_interval: Duration::from_secs(10), server_chat: true }
	}
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObserverEvent {
	/// Full state; sent after every (re)connect.
	Snapshot(PresenceSnapshot),
	Delta(PresenceDelta),
	Chat(ChatMessage),
	/// Lost the query connection; presence may be stale until the next snapshot.
	Disconnected(String),
}

/// Handle to a running observer. Dropping it stops the task.
pub struct Observer {
	presence: Arc<RwLock<Presence>>,
	events: broadcast::Sender<ObserverEvent>,
	connected: Arc<AtomicBool>,
	_stop: watch::Sender<()>,
}

impl Observer {
	pub fn spawn(config: ObserverConfig) -> Self {
		let presence = Arc::new(RwLock::new(Presence::default()));
		let (events, _) = broadcast::channel(1024);
		let (stop, stopped) = watch::channel(());
		let connected = Arc::new(AtomicBool::new(false));
		let state = State {
			presence: presence.clone(),
			events: events.clone(),
			connected: connected.clone(),
		};
		tokio::spawn(run(config, state, stopped));
		Self { presence, events, connected, _stop: stop }
	}

	/// Connected and in sync with the server.
	pub fn is_connected(&self) -> bool {
		self.connected.load(Ordering::Relaxed)
	}

	/// Receive events. Call [`Observer::snapshot`] after subscribing to get
	/// the state the events apply to.
	pub fn subscribe(&self) -> broadcast::Receiver<ObserverEvent> {
		self.events.subscribe()
	}

	pub fn snapshot(&self) -> PresenceSnapshot {
		self.presence.read().unwrap().snapshot()
	}

	pub fn presence(&self) -> Arc<RwLock<Presence>> {
		self.presence.clone()
	}
}

/// What the observer task shares with its handle.
struct State {
	presence: Arc<RwLock<Presence>>,
	events: broadcast::Sender<ObserverEvent>,
	connected: Arc<AtomicBool>,
}

async fn run(config: ObserverConfig, state: State, mut stopped: watch::Receiver<()>) {
	let mut connect = config.connect.clone();
	connect.line.label = "observer".into();
	// Attempts since the last connection that worked.
	let mut attempt = 0u32;
	loop {
		attempt += 1;
		let started = Instant::now();
		let result = tokio::select! {
			r = session(&config, &connect, &state, attempt, started) => r,
			_ = stopped.changed() => return,
		};
		let reason = match result {
			Ok(()) => "connection closed".to_string(),
			Err(e) => e.to_string(),
		};
		let lost = state.connected.swap(false, Ordering::Relaxed);
		if lost {
			attempt = 0;
		}
		// Longer after each failure, or as long as the server asked.
		let retry_in = connect.retry_delay(attempt.max(1));
		let retry_in_s = retry_in.as_secs_f32();
		let elapsed_ms = started.elapsed().as_millis() as u64;
		if lost {
			let connected_for_s = started.elapsed().as_secs();
			warn!(%reason, addr = %connect.addr, connected_for_s, retry_in_s, "observer disconnected");
		} else if attempt_worth_a_warning(attempt) {
			warn!(%reason, addr = %connect.addr, attempt, elapsed_ms, retry_in_s, "observer could not connect");
		} else {
			debug!(%reason, addr = %connect.addr, attempt, elapsed_ms, retry_in_s, "observer could not connect");
		}
		let _ = state.events.send(ObserverEvent::Disconnected(reason));
		tokio::select! {
			_ = sleep(retry_in) => {}
			_ = stopped.changed() => return,
		}
	}
}

/// Read the full state.
async fn load(client: &QueryClient) -> voelin_query::Result<Presence> {
	let info = client.send(&Command::new("serverinfo")).await?;
	let channels = client
		.send(
			&Command::new("channellist")
				.flag("topic")
				.flag("flags")
				.flag("voice")
				.flag("limits")
				.flag("icon")
				.flag("banners"),
		)
		.await?;
	let clients = client
		.send(
			&Command::new("clientlist")
				.flag("uid")
				.flag("away")
				.flag("voice")
				.flag("groups")
				.flag("country"),
		)
		.await?;
	Ok(Presence::from_snapshot(PresenceSnapshot {
		server_name: info
			.first()
			.and_then(|r| r.get("virtualserver_name"))
			.unwrap_or_default()
			.to_string(),
		channels: channels.iter().filter_map(channel_from_row).collect(),
		clients: clients.iter().filter_map(client_from_row).collect(),
	}))
}

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as i64)
		.unwrap_or_default()
}

async fn session(
	config: &ObserverConfig,
	connect: &Connect,
	state: &State,
	attempt: u32,
	started: Instant,
) -> voelin_query::Result<()> {
	let (presence, events) = (&*state.presence, &state.events);
	let (client, mut notifications) = QueryClient::connect(connect).await?;
	if notifications.is_some() {
		for event in ["server", "channel"] {
			let mut cmd = Command::new("servernotifyregister").arg("event", event);
			if event == "channel" {
				cmd = cmd.arg("id", 0);
			}
			client.send(&cmd).await?;
		}
		if config.server_chat {
			client.send(&Command::new("servernotifyregister").arg("event", "textserver")).await?;
		}
	}
	let loaded = load(&client).await?;
	info!(
		server = %loaded.server_name,
		channels = loaded.channels.len(),
		clients = loaded.clients.len(),
		events = notifications.is_some(),
		attempt,
		elapsed_ms = started.elapsed().as_millis() as u64,
		"observer connected"
	);
	*presence.write().unwrap() = loaded.clone();
	state.connected.store(true, Ordering::Relaxed);
	let _ = events.send(ObserverEvent::Snapshot(loaded.snapshot()));

	let mut next_poll = Instant::now() + config.poll_interval;
	loop {
		let notification: Option<Notification> = tokio::select! {
			n = async {
				match &mut notifications {
					Some(rx) => rx.recv().await,
					None => std::future::pending().await,
				}
			} => match n {
				Some(n) => Some(n),
				None => return Ok(()),
			},
			_ = sleep_until(next_poll) => None,
		};

		match notification {
			Some(n) => handle_notification(&n, presence, events),
			None => {
				// Resync and publish only what changed.
				let listed = load(&client).await?;
				let fresh = keep_unlisted(&presence.read().unwrap(), listed);
				let deltas = presence.read().unwrap().diff(&fresh);
				if !deltas.is_empty() {
					debug!(changes = deltas.len(), "observer resync");
				}
				*presence.write().unwrap() = fresh;
				for d in deltas {
					let _ = events.send(ObserverEvent::Delta(d));
				}
				next_poll = Instant::now() + config.poll_interval;
			}
		}
	}
}

/// What `clientlist` does not tell (the avatars, which come with
/// `notifycliententerview` and `notifyclientupdated`), kept from what was
/// known of the same client: a resync must not take them away.
fn keep_unlisted(old: &Presence, mut fresh: Presence) -> Presence {
	for (id, client) in &mut fresh.clients {
		let Some(known) = old.clients.get(id).filter(|known| known.uid == client.uid) else {
			continue;
		};
		if client.avatar.is_none() {
			client.avatar.clone_from(&known.avatar);
		}
		if client.myts_avatar.is_none() {
			client.myts_avatar.clone_from(&known.myts_avatar);
		}
	}
	fresh
}

fn handle_notification(
	n: &Notification,
	presence: &RwLock<Presence>,
	events: &broadcast::Sender<ObserverEvent>,
) {
	if let Some(msg) = is_text_message(n, now_ms()) {
		let _ = events.send(ObserverEvent::Chat(msg));
		return;
	}
	let deltas = if n.name == "notifychanneledited" || n.name == "notifychannelmoved" {
		let state = presence.read().unwrap();
		n.rows
			.iter()
			.filter_map(|row| {
				let mut channel = state.channels.get(&row.parse("cid")?)?.clone();
				update_channel(&mut channel, row);
				if let Some(parent) = row.parse("cpid") {
					channel.parent = parent;
				}
				if let Some(order) = row.parse("order") {
					channel.order = order;
				}
				Some(PresenceDelta::ChannelChanged(channel))
			})
			.collect()
	} else {
		delta_from_notification(n)
	};
	let mut state = presence.write().unwrap();
	for d in deltas {
		state.apply(&d);
		let _ = events.send(ObserverEvent::Delta(d));
	}
}

#[cfg(test)]
mod resync_tests {
	use super::*;
	use voelin_model::ClientInfo;

	#[test]
	fn a_resync_keeps_the_avatars_the_list_does_not_tell() {
		let client = |uid: &str, myts: Option<&str>| ClientInfo {
			id: 5,
			uid: Some(uid.into()),
			nickname: "a".into(),
			avatar: myts.map(|_| "0123456789abcdef0123456789abcdef".into()),
			myts_avatar: myts.map(Into::into),
			..Default::default()
		};
		let mut old = Presence::default();
		old.clients.insert(5, client("u=", Some("https://a.example.test/on.png")));
		let mut listed = Presence::default();
		listed.clients.insert(5, client("u=", None));
		let fresh = keep_unlisted(&old, listed.clone());
		assert_eq!(fresh.clients[&5], old.clients[&5]);
		assert!(old.diff(&fresh).is_empty());
		// Another client in the same slot starts without them.
		let mut other = Presence::default();
		other.clients.insert(5, client("v=", None));
		let fresh = keep_unlisted(&old, other);
		assert_eq!(
			(fresh.clients[&5].avatar.clone(), fresh.clients[&5].myts_avatar.clone()),
			(None, None)
		);
	}
}
