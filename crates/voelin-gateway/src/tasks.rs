//! Background tasks: chat pumps, relay teardown, stream tracking, retention,
//! event reminders, and applying setting changes as they happen.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::json;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};
use voelin_gateway_proto::activity_kind;
use voelin_model::{ChatTarget, PresenceDelta};
use voelin_observer::{ObserverEvent, RelayEvent};

use crate::db::NewActivity;
use crate::hub::{Feature, Hub, HubEvent, now_ms, now_secs};

pub fn spawn_all(hub: &Arc<Hub>) {
	tokio::spawn(pump_observer(hub.clone()));
	tokio::spawn(pump_relays(hub.clone(), hub.relays().subscribe()));
	tokio::spawn(teardown_idle_relays(hub.clone()));
	tokio::spawn(prune(hub.clone()));
	tokio::spawn(reminders(hub.clone()));
	tokio::spawn(apply_settings(hub.clone()));
	tokio::spawn(check_streams(hub.clone()));
	if let Some(path) = hub.settings.path() {
		tokio::spawn(watch_file(hub.clone(), path));
	}
}

/// Server chat and presence changes seen by the observer.
async fn pump_observer(hub: Arc<Hub>) {
	let mut events = hub.observer.subscribe();
	loop {
		match events.recv().await {
			Ok(ObserverEvent::Chat(msg)) if msg.target == ChatTarget::Server => {
				if !msg.author_id.is_some_and(|id| hub.is_own_client(id)) {
					hub.publish_incoming(msg);
				}
			}
			Ok(ObserverEvent::Delta(PresenceDelta::ClientLeft { id })) => hub.client_left(id),
			Ok(ObserverEvent::Delta(
				PresenceDelta::ClientJoined(_)
				| PresenceDelta::ClientChanged(_)
				| PresenceDelta::ClientMoved { .. },
			)) => {
				let presence = hub.observer.presence();
				let presence = presence.read().unwrap().clone();
				hub.reconcile_streams(&presence, false);
			}
			Ok(ObserverEvent::Snapshot(_)) | Err(RecvError::Lagged(_)) => {
				let presence = hub.observer.presence();
				let presence = presence.read().unwrap().clone();
				hub.reconcile_streams(&presence, true);
			}
			Ok(_) => {}
			Err(RecvError::Closed) => return,
		}
	}
}

/// The observer's poll does not produce events for changes it cannot see
/// as deltas; check the directory against presence now and then.
async fn check_streams(hub: Arc<Hub>) {
	let mut tick = tokio::time::interval(Duration::from_secs(15));
	loop {
		tick.tick().await;
		let presence = hub.observer.presence();
		let presence = presence.read().unwrap().clone();
		hub.reconcile_streams(&presence, true);
	}
}

/// Channel chat seen by one relay pool; subscribed before the pool opens
/// any relay, so nothing is missed when the pool is rebuilt. Ends with the pool.
pub async fn pump_relays(hub: Arc<Hub>, mut events: broadcast::Receiver<RelayEvent>) {
	loop {
		match events.recv().await {
			Ok(RelayEvent::Message(msg)) => {
				if !msg.author_id.is_some_and(|id| hub.is_own_client(id)) {
					hub.publish_incoming(msg);
				}
			}
			Ok(RelayEvent::Closed { channel, reason }) => {
				warn!(channel, %reason, "relay closed");
			}
			Err(RecvError::Lagged(_)) => {}
			Err(RecvError::Closed) => return,
		}
	}
}

async fn teardown_idle_relays(hub: Arc<Hub>) {
	loop {
		let rt = hub.runtime();
		let wait = Duration::from_secs(rt.relay.idle_teardown_secs.clamp(1, 15));
		tokio::time::sleep(wait).await;
		let rt = hub.runtime();
		for cid in hub.idle_relays(&rt) {
			hub.relays().close(cid).await;
		}
	}
}

/// Retention: at start, hourly, and whenever a retention setting changes.
async fn prune(hub: Arc<Hub>) {
	let mut changes = hub.settings.subscribe();
	loop {
		let rt = hub.runtime();
		let now = now_ms();
		let day = 86_400_000;
		if rt.history.retention_days > 0 {
			let cutoff = now - rt.history.retention_days as i64 * day;
			match hub.db.prune_messages(cutoff) {
				Ok(n) if n > 0 => info!(messages = n, "pruned history"),
				Ok(_) => {}
				Err(error) => warn!(%error, "pruning history failed"),
			}
		}
		if rt.activity.retention_days > 0
			&& let Err(error) = hub.db.prune_activity(now - rt.activity.retention_days as i64 * day)
		{
			warn!(%error, "pruning activity failed");
		}
		if let Err(error) = hub.db.prune_tokens(now_secs()) {
			warn!(%error, "pruning tokens failed");
		}
		let (history, activity) = (rt.history.retention_days, rt.activity.retention_days);
		tokio::select! {
			_ = tokio::time::sleep(Duration::from_secs(3600)) => {}
			_ = async {
				while changes.changed().await.is_ok() {
					let rt = changes.borrow_and_update().clone();
					if rt.history.retention_days != history || rt.activity.retention_days != activity {
						break;
					}
				}
			} => {}
		}
	}
}

/// Reminders before events start, and an activity entry when they do.
async fn reminders(hub: Arc<Hub>) {
	let mut tick = tokio::time::interval(Duration::from_secs(5));
	let mut changes = hub.settings.subscribe();
	// Catch reminders due shortly before a restart.
	let mut last = now_ms() - 60_000;
	loop {
		tokio::select! {
			_ = tick.tick() => {}
			// New reminder times apply at once.
			r = changes.changed() => if r.is_err() { return },
		}
		if !hub.feature_enabled(Feature::Events) {
			last = now_ms();
			continue;
		}
		let now = now_ms();
		let offsets = hub.runtime().events.reminder_minutes.clone();
		for offset in offsets {
			let offset_ms = offset as i64 * 60_000;
			let due = hub.db.events_starting(last + offset_ms, now + offset_ms).unwrap_or_default();
			for event in due {
				if hub.db.mark_notice(event.id, offset as i64).unwrap_or(false) {
					let starts_in_ms = event.spec.start_ms - now;
					hub.emit(HubEvent::Reminder { event, starts_in_ms });
				}
			}
		}
		// Offset -1: the event started.
		for event in hub.db.events_starting(last, now).unwrap_or_default() {
			if hub.db.mark_notice(event.id, -1).unwrap_or(false) {
				hub.add_activity(NewActivity {
					kind: activity_kind::EVENT_STARTING,
					actor: Some(&event.creator),
					channel: event.spec.channel,
					ref_id: Some(event.id.to_string()),
					text: format!("{} is starting", event.spec.title),
					data: json!({ "title": event.spec.title, "kind": event.spec.kind,
						"live_stream": event.live_stream }),
				});
			}
		}
		last = now;
	}
}

/// Apply setting changes that need more than reading the new value.
async fn apply_settings(hub: Arc<Hub>) {
	let mut changes = hub.settings.subscribe();
	let mut old = changes.borrow_and_update().clone();
	while changes.changed().await.is_ok() {
		let new = changes.borrow_and_update().clone();
		if new.relay.nickname != old.relay.nickname {
			hub.rebuild_relays(&new.relay.nickname).await;
		}
		if new.relay.pinned_channels != old.relay.pinned_channels {
			hub.apply_pinned_channels(&old.relay.pinned_channels, &new.relay.pinned_channels).await;
		}
		let features_changed = Feature::ALL.iter().any(|f| f.enabled(&old) != f.enabled(&new));
		if features_changed || new.perm.admin != old.perm.admin {
			hub.emit(HubEvent::Capabilities);
		}
		if new.features.streams && !old.features.streams {
			let presence = hub.observer.presence();
			let presence = presence.read().unwrap().clone();
			hub.reconcile_streams(&presence, true);
		}
		old = new;
	}
}

/// Reload the file when it changes (checked every few seconds).
async fn watch_file(hub: Arc<Hub>, path: std::path::PathBuf) {
	let modified = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
	let mut last: Option<SystemTime> = modified(&path);
	loop {
		tokio::time::sleep(Duration::from_secs(5)).await;
		let now = modified(&path);
		if now != last {
			last = now;
			if let Err(error) = hub.settings.reload() {
				warn!(error = format!("{error:#}"), "not reloading the changed configuration");
			}
		}
	}
}
