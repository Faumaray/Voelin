//! Requests beyond presence and plain chat: history paging, pins,
//! reactions, topics, events, the activity feed, permissions and
//! administration. Each checks the feature switch, the user's access to the
//! chat or channel, the action's permission rule and quotas.

use serde_json::{Value, json};
use tracing::warn;
use voelin_gateway_proto::{
	Action, ActivityEntry, ConfigEntry, ErrorCode, EventInfo, EventKind, EventQuery, EventSpec,
	HistoryEntry, HistoryPage, HistoryQuery, PermRule, PermRuleInfo, PinInfo, RsvpStatus,
	TopicInfo, UserRef, activity_kind,
};
use voelin_model::{ChannelId, ChatTarget};

use crate::config::perm_key;
use crate::db::{EventFilter, HistoryOptions, NewActivity};
use crate::hub::{Denied, Feature, Hub, HubEvent, User, now_ms};

fn channel_of(target: &ChatTarget) -> Option<ChannelId> {
	match target {
		ChatTarget::Channel(cid) => Some(*cid),
		_ => None,
	}
}

/// The admin's page quota applied to a requested limit (0: none).
fn page_limit(requested: Option<u32>, quota: u64) -> Option<u32> {
	let quota = u32::try_from(quota).unwrap_or(u32::MAX);
	match (requested, quota) {
		(r, 0) => r,
		(Some(r), q) => Some(r.min(q)),
		(None, q) => Some(q),
	}
}

impl Hub {
	/// Store an activity entry and push it (if the feed is on).
	pub fn add_activity(&self, activity: NewActivity<'_>) {
		if !self.feature_enabled(Feature::Activity) {
			return;
		}
		match self.db.add_activity(activity, now_ms()) {
			Ok(entry) => self.emit(HubEvent::Activity(entry)),
			Err(error) => warn!(%error, "could not store activity"),
		}
	}

	/// The actions the user may take, server-wide or in `channel`.
	pub async fn permissions(
		&self,
		user: &User,
		channel: Option<ChannelId>,
	) -> Result<Vec<Action>, Denied> {
		let mut actions = Vec::new();
		for action in Action::ALL {
			if self.allowed(user, action, channel).await? {
				actions.push(action);
			}
		}
		Ok(actions)
	}

	// History

	/// The original `history` request: before an id, oldest first.
	pub async fn history_v1(
		&self,
		user: &User,
		target: &ChatTarget,
		before: Option<i64>,
		limit: u32,
	) -> Result<Vec<HistoryEntry>, Denied> {
		self.require_read(user, target).await?;
		if !self.feature_enabled(Feature::History) {
			return Ok(Vec::new());
		}
		let opts = HistoryOptions {
			before,
			limit: page_limit(Some(limit), self.runtime().quota.history_page),
			..Default::default()
		};
		Ok(self.db.history(target, &opts, Some(&user.uid))?.0)
	}

	pub async fn history(&self, user: &User, q: HistoryQuery) -> Result<HistoryPage, Denied> {
		self.require_read(user, &q.target).await?;
		self.require(Feature::History)?;
		if let Some(topic) = q.topic {
			let t = self.db.topic(topic)?.ok_or_else(|| Denied::not_found("topic"))?;
			if t.target != q.target {
				return Err(Denied::bad("the topic belongs to another chat"));
			}
		}
		let opts = HistoryOptions {
			before: q.before,
			after: q.after,
			before_ms: q.before_ms,
			after_ms: q.after_ms,
			limit: page_limit(q.limit, self.runtime().quota.history_page),
			topic: q.topic,
			exclude_topics: q.exclude_topics,
		};
		let (messages, has_more) = self.db.history(&q.target, &opts, Some(&user.uid))?;
		Ok(HistoryPage { target: q.target, topic: q.topic, messages, has_more })
	}

	/// New and changed messages since a revision; returns them, the revision
	/// to continue from and whether more are waiting.
	pub async fn sync(
		&self,
		user: &User,
		target: &ChatTarget,
		since: i64,
		limit: Option<u32>,
	) -> Result<(Vec<HistoryEntry>, i64, bool), Denied> {
		self.require_read(user, target).await?;
		self.require(Feature::History)?;
		let limit = page_limit(limit, self.runtime().quota.history_page);
		let (messages, more) = self.db.sync(target, since, limit, Some(&user.uid))?;
		let rev = messages.last().map_or(since, |m| m.rev);
		Ok((messages, rev, more))
	}

	/// A stored message the user may read.
	async fn readable_message(&self, user: &User, id: i64) -> Result<HistoryEntry, Denied> {
		let entry =
			self.db.message(id, Some(&user.uid))?.ok_or_else(|| Denied::not_found("message"))?;
		self.require_read(user, &entry.message.target).await?;
		Ok(entry)
	}

	// Pins

	pub async fn pin(&self, user: &User, message_id: i64) -> Result<(), Denied> {
		self.require(Feature::Pins)?;
		let entry = self.readable_message(user, message_id).await?;
		let target = entry.message.target.clone();
		self.require_allowed(user, Action::Pin, channel_of(&target)).await?;
		if entry.pinned {
			return Ok(());
		}
		let quota = self.runtime().quota.pins_per_channel;
		if quota > 0 && self.db.pin_count(&target)? >= quota {
			return Err(Denied::quota(format!("at most {quota} pins per chat")));
		}
		if self.db.pin(message_id, &target, &user.user_ref(), now_ms())?
			&& let Some(pin) = self.db.pin_info(message_id, None)?
		{
			self.emit(HubEvent::Pinned(pin));
			self.add_activity(NewActivity {
				kind: activity_kind::PINNED,
				actor: Some(&user.user_ref()),
				channel: channel_of(&target),
				ref_id: Some(message_id.to_string()),
				text: format!("{} pinned a message", user.nickname),
				data: json!({ "message_id": message_id, "target": target }),
			});
		}
		Ok(())
	}

	pub async fn unpin(&self, user: &User, message_id: i64) -> Result<(), Denied> {
		self.require(Feature::Pins)?;
		let entry = self.readable_message(user, message_id).await?;
		let target = entry.message.target.clone();
		self.require_allowed(user, Action::Pin, channel_of(&target)).await?;
		if self.db.unpin(message_id, now_ms())?.is_some() {
			self.emit(HubEvent::Unpinned { target, message_id, by: user.user_ref() });
		}
		Ok(())
	}

	pub async fn pins(&self, user: &User, target: &ChatTarget) -> Result<Vec<PinInfo>, Denied> {
		self.require(Feature::Pins)?;
		self.require_read(user, target).await?;
		Ok(self.db.pins(target, Some(&user.uid))?)
	}

	// Reactions

	pub async fn react(
		&self,
		user: &User,
		message_id: i64,
		emoji: &str,
		add: bool,
	) -> Result<(), Denied> {
		self.require(Feature::Reactions)?;
		let rt = self.runtime();
		if emoji.is_empty() || emoji.chars().any(|c| c.is_whitespace() || c.is_control()) {
			return Err(Denied::bad("a reaction is a non-empty string without spaces"));
		}
		if rt.quota.reaction_bytes > 0 && emoji.len() as u64 > rt.quota.reaction_bytes {
			return Err(Denied::quota(format!(
				"reactions are limited to {} bytes",
				rt.quota.reaction_bytes
			)));
		}
		let entry = self.readable_message(user, message_id).await?;
		let target = entry.message.target;
		self.require_allowed(user, Action::React, channel_of(&target)).await?;
		let me = user.user_ref();
		let count = if add {
			let quota = rt.quota.reactions_per_message;
			if quota > 0
				&& !entry.reactions.iter().any(|r| r.emoji == emoji)
				&& self.db.reaction_kinds(message_id)? >= quota
			{
				return Err(Denied::quota(format!(
					"at most {quota} different reactions per message"
				)));
			}
			self.db.add_reaction(message_id, emoji, &me, now_ms())?
		} else {
			self.db.remove_reaction(message_id, emoji, &me.uid, now_ms())?
		};
		if let Some(count) = count {
			self.emit(HubEvent::Reaction {
				target,
				message_id,
				emoji: emoji.to_string(),
				user: me,
				added: add,
				count,
			});
		}
		Ok(())
	}

	pub async fn reactors(
		&self,
		user: &User,
		message_id: i64,
		emoji: &str,
	) -> Result<Vec<UserRef>, Denied> {
		self.require(Feature::Reactions)?;
		self.readable_message(user, message_id).await?;
		Ok(self.db.reactors(message_id, emoji)?)
	}

	// Topics

	pub async fn create_topic(
		&self,
		user: &User,
		target: &ChatTarget,
		title: &str,
		message_id: Option<i64>,
	) -> Result<TopicInfo, Denied> {
		self.require(Feature::Topics)?;
		let title = title.trim();
		if title.is_empty() {
			return Err(Denied::bad("a topic needs a title"));
		}
		self.require_post(user, target).await?;
		self.require_allowed(user, Action::CreateTopic, channel_of(target)).await?;
		if let Some(id) = message_id {
			let root = self.db.message(id, None)?.ok_or_else(|| Denied::not_found("message"))?;
			if root.message.target != *target {
				return Err(Denied::bad("the message belongs to another chat"));
			}
		}
		let quota = self.runtime().quota.topics_per_channel;
		if quota > 0 && self.db.open_topic_count(target)? >= quota {
			return Err(Denied::quota(format!("at most {quota} open topics per chat")));
		}
		let topic = self.db.create_topic(target, title, &user.user_ref(), message_id, now_ms())?;
		self.emit(HubEvent::Topic(topic.clone()));
		self.add_activity(NewActivity {
			kind: activity_kind::TOPIC_CREATED,
			actor: Some(&user.user_ref()),
			channel: channel_of(target),
			ref_id: Some(topic.id.to_string()),
			text: format!("{} started the topic {}", user.nickname, topic.title),
			data: json!({ "title": topic.title, "target": target }),
		});
		Ok(topic)
	}

	pub async fn update_topic(
		&self,
		user: &User,
		id: i64,
		title: Option<&str>,
		archived: Option<bool>,
	) -> Result<TopicInfo, Denied> {
		self.require(Feature::Topics)?;
		let topic = self.db.topic(id)?.ok_or_else(|| Denied::not_found("topic"))?;
		self.require_read(user, &topic.target).await?;
		if topic.creator.uid != user.uid {
			self.require_allowed(user, Action::Moderate, channel_of(&topic.target)).await?;
		}
		let title = title.map(str::trim);
		if title == Some("") {
			return Err(Denied::bad("a topic needs a title"));
		}
		let topic =
			self.db.update_topic(id, title, archived)?.ok_or_else(|| Denied::not_found("topic"))?;
		self.emit(HubEvent::Topic(topic.clone()));
		Ok(topic)
	}

	pub async fn topics(
		&self,
		user: &User,
		target: &ChatTarget,
		include_archived: bool,
	) -> Result<Vec<TopicInfo>, Denied> {
		self.require(Feature::Topics)?;
		self.require_read(user, target).await?;
		Ok(self.db.topics(target, include_archived)?)
	}

	// Events

	/// The user may see an event (server-wide, or in a visible channel).
	pub fn event_visible(&self, user: &User, event: &EventInfo) -> bool {
		event.spec.channel.is_none_or(|c| self.channel_visible(user, c))
	}

	fn check_spec(spec: &EventSpec) -> Result<(), Denied> {
		if spec.title.trim().is_empty() {
			return Err(Denied::bad("an event needs a title"));
		}
		if spec.end_ms.is_some_and(|end| end < spec.start_ms) {
			return Err(Denied::bad("the event ends before it starts"));
		}
		if spec.kind == EventKind::Unknown {
			return Err(Denied::bad("unknown event kind"));
		}
		Ok(())
	}

	async fn check_event_channel(&self, user: &User, spec: &EventSpec) -> Result<(), Denied> {
		if let Some(cid) = spec.channel
			&& !self.channel_visible(user, cid)
		{
			return Err(Denied::forbidden("no access to this channel"));
		}
		self.require_allowed(user, Action::CreateEvent, spec.channel).await
	}

	pub async fn create_event(&self, user: &User, spec: EventSpec) -> Result<EventInfo, Denied> {
		self.require(Feature::Events)?;
		Self::check_spec(&spec)?;
		self.check_event_channel(user, &spec).await?;
		if spec.host_uid.as_ref().is_some_and(|h| *h != user.uid) {
			self.require_allowed(user, Action::Moderate, spec.channel).await?;
		}
		let quota = self.runtime().quota.events_per_user;
		if quota > 0 && self.db.upcoming_events_by(&user.uid, now_ms())? >= quota {
			return Err(Denied::quota(format!("at most {quota} upcoming events per user")));
		}
		let event = self.db.create_event(&spec, &user.user_ref(), now_ms())?;
		self.emit(HubEvent::Event(EventInfo { my_rsvp: None, ..event.clone() }));
		self.add_activity(NewActivity {
			kind: activity_kind::EVENT_CREATED,
			actor: Some(&user.user_ref()),
			channel: spec.channel,
			ref_id: Some(event.id.to_string()),
			text: format!("{} scheduled {}", user.nickname, event.spec.title),
			data: json!({ "title": event.spec.title, "start_ms": event.spec.start_ms,
				"kind": event.spec.kind }),
		});
		Ok(event)
	}

	/// An event the user created, or may moderate.
	async fn own_event(&self, user: &User, id: i64) -> Result<EventInfo, Denied> {
		let event =
			self.db.event(id, Some(&user.uid), false)?.ok_or_else(|| Denied::not_found("event"))?;
		if !self.event_visible(user, &event) {
			return Err(Denied::not_found("event"));
		}
		if event.creator.uid != user.uid {
			self.require_allowed(user, Action::Moderate, event.spec.channel).await?;
		}
		Ok(event)
	}

	pub async fn update_event(
		&self,
		user: &User,
		id: i64,
		spec: EventSpec,
	) -> Result<EventInfo, Denied> {
		self.require(Feature::Events)?;
		Self::check_spec(&spec)?;
		let old = self.own_event(user, id).await?;
		if spec.channel != old.spec.channel {
			self.check_event_channel(user, &spec).await?;
		}
		self.db.update_event(id, &spec, now_ms())?.ok_or_else(|| Denied::not_found("event"))?;
		self.emit(HubEvent::Event(
			self.db.event(id, None, false)?.ok_or_else(|| Denied::not_found("event"))?,
		));
		self.db.event(id, Some(&user.uid), false)?.ok_or_else(|| Denied::not_found("event"))
	}

	pub async fn delete_event(&self, user: &User, id: i64) -> Result<(), Denied> {
		self.require(Feature::Events)?;
		let event = self.own_event(user, id).await?;
		if self.db.delete_event(id)? {
			self.emit(HubEvent::EventDeleted { id, channel: event.spec.channel });
			self.add_activity(NewActivity {
				kind: activity_kind::EVENT_CANCELLED,
				actor: Some(&user.user_ref()),
				channel: event.spec.channel,
				ref_id: Some(id.to_string()),
				text: format!("{} cancelled {}", user.nickname, event.spec.title),
				data: json!({ "title": event.spec.title, "start_ms": event.spec.start_ms }),
			});
		}
		Ok(())
	}

	pub async fn get_event(&self, user: &User, id: i64) -> Result<EventInfo, Denied> {
		self.require(Feature::Events)?;
		let event =
			self.db.event(id, Some(&user.uid), true)?.ok_or_else(|| Denied::not_found("event"))?;
		if !self.event_visible(user, &event) {
			return Err(Denied::not_found("event"));
		}
		Ok(event)
	}

	pub async fn events(&self, user: &User, q: EventQuery) -> Result<Vec<EventInfo>, Denied> {
		self.require(Feature::Events)?;
		let filter = EventFilter {
			from_ms: q.from_ms.unwrap_or_else(now_ms),
			to_ms: q.to_ms,
			channel: q.channel,
			limit: q.limit,
		};
		let events = self.db.events(&filter, Some(&user.uid))?;
		Ok(events.into_iter().filter(|e| self.event_visible(user, e)).collect())
	}

	pub async fn rsvp(
		&self,
		user: &User,
		id: i64,
		status: Option<RsvpStatus>,
	) -> Result<EventInfo, Denied> {
		self.require(Feature::Events)?;
		if status == Some(RsvpStatus::Unknown) {
			return Err(Denied::bad("unknown answer"));
		}
		let event = self.db.event(id, None, false)?.ok_or_else(|| Denied::not_found("event"))?;
		if !self.event_visible(user, &event) {
			return Err(Denied::not_found("event"));
		}
		self.require_allowed(user, Action::Rsvp, event.spec.channel).await?;
		self.db.rsvp(id, &user.user_ref(), status, now_ms())?;
		if let Some(event) = self.db.event(id, None, false)? {
			self.emit(HubEvent::Event(event));
		}
		self.db.event(id, Some(&user.uid), false)?.ok_or_else(|| Denied::not_found("event"))
	}

	// Activity

	pub async fn activity(
		&self,
		user: &User,
		before: Option<i64>,
		limit: Option<u32>,
	) -> Result<(Vec<ActivityEntry>, bool), Denied> {
		self.require(Feature::Activity)?;
		let (entries, more) = self.db.activity(before, limit)?;
		let visible =
			entries.into_iter().filter(|e| e.channel.is_none_or(|c| self.channel_visible(user, c)));
		Ok((visible.collect(), more))
	}

	// Administration

	pub async fn require_admin(&self, user: &User) -> Result<(), Denied> {
		if self.is_admin(user).await? {
			Ok(())
		} else {
			Err(Denied::forbidden("only gateway admins may do this"))
		}
	}

	pub async fn config_list(&self, user: &User) -> Result<Vec<ConfigEntry>, Denied> {
		self.require_admin(user).await?;
		Ok(self.settings.entries())
	}

	pub async fn config_get(&self, user: &User, key: &str) -> Result<ConfigEntry, Denied> {
		self.require_admin(user).await?;
		self.settings.entry(key).ok_or_else(|| Denied::not_found("setting"))
	}

	pub async fn config_set(
		&self,
		user: &User,
		key: &str,
		value: Value,
	) -> Result<ConfigEntry, Denied> {
		self.require_admin(user).await?;
		if self.settings.entry(key).is_none() {
			return Err(Denied::not_found("setting"));
		}
		let entry = self
			.settings
			.set(&self.db, key, value, Some(&user.uid))
			.map_err(|e| Denied::new(ErrorCode::BadRequest, format!("{e:#}")))?;
		self.audit(Some(&user.uid), "config_set", &format!("{key}={}", entry.value));
		Ok(entry)
	}

	pub async fn config_reset(&self, user: &User, key: &str) -> Result<ConfigEntry, Denied> {
		self.require_admin(user).await?;
		let entry = self
			.settings
			.reset(&self.db, key, Some(&user.uid))
			.map_err(|e| Denied::new(ErrorCode::NotFound, format!("{e:#}")))?;
		self.audit(Some(&user.uid), "config_reset", key);
		Ok(entry)
	}

	pub async fn config_reload(&self, user: &User) -> Result<Vec<ConfigEntry>, Denied> {
		self.require_admin(user).await?;
		self.settings.reload().map_err(|e| Denied::new(ErrorCode::BadRequest, format!("{e:#}")))?;
		self.audit(Some(&user.uid), "config_reload", "");
		Ok(self.settings.entries())
	}

	pub async fn perm_list(&self, user: &User) -> Result<Vec<PermRuleInfo>, Denied> {
		self.require_admin(user).await?;
		Ok(self.settings.perm_rules())
	}

	pub async fn perm_set(
		&self,
		user: &User,
		action: Action,
		rule: Option<PermRule>,
	) -> Result<Vec<PermRuleInfo>, Denied> {
		if action == Action::Unknown {
			return Err(Denied::bad("unknown action"));
		}
		let key = perm_key(action);
		match rule {
			Some(rule) => {
				let value = serde_json::to_value(rule).expect("serializable");
				self.config_set(user, &key, value).await?;
			}
			None => {
				self.config_reset(user, &key).await?;
			}
		}
		Ok(self.settings.perm_rules())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn page_quota() {
		assert_eq!(page_limit(None, 0), None);
		assert_eq!(page_limit(Some(50_000), 0), Some(50_000));
		assert_eq!(page_limit(Some(50), 100), Some(50));
		assert_eq!(page_limit(Some(500), 100), Some(100));
		assert_eq!(page_limit(None, 100), Some(100));
	}
}
