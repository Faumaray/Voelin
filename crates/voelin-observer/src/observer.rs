//! Keep a server's presence up to date from one query session.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{broadcast, watch};
use tokio::time::{Instant, sleep, sleep_until};
use tracing::{debug, info, warn};
use voelin_model::{ChatMessage, Presence, PresenceDelta, PresenceSnapshot};
use voelin_query::{Command, Connect, Notification, QueryClient};

use crate::convert::{
	channel_from_row, client_from_row, delta_from_notification, is_text_message, update_channel,
};

#[derive(Clone, Debug)]
pub struct ObserverConfig {
	pub connect: Connect,
	/// Full resync interval. Query sessions receive no `notifyclientupdated`,
	/// so mute/away changes are only noticed by re-reading the client list.
	pub poll_interval: Duration,
	/// Also deliver server chat (`event=textserver`).
	pub server_chat: bool,
	/// Wait between reconnect attempts.
	pub reconnect_delay: Duration,
}

impl ObserverConfig {
	pub fn new(connect: Connect) -> Self {
		Self {
			connect,
			poll_interval: Duration::from_secs(10),
			server_chat: true,
			reconnect_delay: Duration::from_secs(5),
		}
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
	_stop: watch::Sender<()>,
}

impl Observer {
	pub fn spawn(config: ObserverConfig) -> Self {
		let presence = Arc::new(RwLock::new(Presence::default()));
		let (events, _) = broadcast::channel(1024);
		let (stop, stopped) = watch::channel(());
		tokio::spawn(run(config, presence.clone(), events.clone(), stopped));
		Self { presence, events, _stop: stop }
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

async fn run(
	config: ObserverConfig,
	presence: Arc<RwLock<Presence>>,
	events: broadcast::Sender<ObserverEvent>,
	mut stopped: watch::Receiver<()>,
) {
	loop {
		let result = tokio::select! {
			r = session(&config, &presence, &events) => r,
			_ = stopped.changed() => return,
		};
		let reason = match result {
			Ok(()) => "connection closed".to_string(),
			Err(e) => e.to_string(),
		};
		warn!(%reason, addr = %config.connect.addr, "observer disconnected");
		let _ = events.send(ObserverEvent::Disconnected(reason));
		tokio::select! {
			_ = sleep(config.reconnect_delay) => {}
			_ = stopped.changed() => return,
		}
	}
}

/// Read the full state.
async fn load(client: &QueryClient) -> voelin_query::Result<Presence> {
	let info = client.send(&Command::new("serverinfo")).await?;
	let channels = client
		.send(&Command::new("channellist").flag("topic").flag("flags").flag("voice").flag("limits"))
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
	presence: &RwLock<Presence>,
	events: &broadcast::Sender<ObserverEvent>,
) -> voelin_query::Result<()> {
	let (client, mut notifications) = QueryClient::connect(&config.connect).await?;
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
	let state = load(&client).await?;
	info!(
		server = %state.server_name,
		channels = state.channels.len(),
		clients = state.clients.len(),
		events = notifications.is_some(),
		"observer connected"
	);
	*presence.write().unwrap() = state.clone();
	let _ = events.send(ObserverEvent::Snapshot(state.snapshot()));

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
				let fresh = load(&client).await?;
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
