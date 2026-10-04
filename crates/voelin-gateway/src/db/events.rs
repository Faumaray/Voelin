//! Scheduled events and RSVPs.

use rusqlite::{OptionalExtension, Row, ToSql, params};
use voelin_gateway_proto::{Attendee, EventInfo, EventKind, EventSpec, RsvpStatus, UserRef};

use super::{Db, user};

const EVENT_COLUMNS: &str = "e.id, e.title, e.description, e.start_ms, e.end_ms, e.channel, \
	e.kind, e.stream_title, e.stream_game, e.host_uid, e.creator_uid, e.creator_name, \
	e.created_ms, e.updated_ms, e.live_stream, \
	(SELECT COUNT(*) FROM rsvps r WHERE r.event_id = e.id AND r.status = 'going'), \
	(SELECT COUNT(*) FROM rsvps r WHERE r.event_id = e.id AND r.status = 'maybe'), \
	(SELECT COUNT(*) FROM rsvps r WHERE r.event_id = e.id AND r.status = 'not_going'), \
	(SELECT status FROM rsvps r WHERE r.event_id = e.id AND r.uid = :me)";

fn kind_str(kind: EventKind) -> &'static str {
	match kind {
		EventKind::Stream => "stream",
		EventKind::General | EventKind::Unknown => "general",
	}
}

fn event_from_row(r: &Row) -> rusqlite::Result<EventInfo> {
	let kind: String = r.get(6)?;
	let my: Option<String> = r.get(18)?;
	Ok(EventInfo {
		id: r.get(0)?,
		spec: EventSpec {
			title: r.get(1)?,
			description: r.get(2)?,
			start_ms: r.get(3)?,
			end_ms: r.get(4)?,
			channel: r.get::<_, Option<i64>>(5)?.map(|c| c as u64),
			kind: if kind == "stream" { EventKind::Stream } else { EventKind::General },
			stream_title: r.get(7)?,
			stream_game: r.get(8)?,
			host_uid: Some(r.get(9)?),
		},
		creator: user(r.get(10)?, r.get(11)?),
		created_ms: r.get(12)?,
		updated_ms: r.get(13)?,
		live_stream: r.get(14)?,
		going: r.get(15)?,
		maybe: r.get(16)?,
		not_going: r.get(17)?,
		my_rsvp: my.as_deref().map(RsvpStatus::parse),
		attendees: Vec::new(),
	})
}

/// Which events [`Db::events`] returns.
#[derive(Clone, Debug, Default)]
pub struct EventFilter {
	/// Events that end (or start, without an end) at or after this.
	pub from_ms: i64,
	pub to_ms: Option<i64>,
	pub channel: Option<u64>,
	pub limit: Option<u32>,
}

impl EventFilter {
	pub fn sql(&self) -> String {
		let mut conds = vec!["COALESCE(e.end_ms, e.start_ms) >= :from"];
		if self.to_ms.is_some() {
			conds.push("e.start_ms < :to");
		}
		if self.channel.is_some() {
			conds.push("e.channel = :channel");
		}
		format!(
			"SELECT {EVENT_COLUMNS} FROM events e WHERE {} ORDER BY +e.start_ms, e.id LIMIT :limit",
			conds.join(" AND ")
		)
	}
}

impl Db {
	pub fn create_event(
		&self,
		spec: &EventSpec,
		creator: &UserRef,
		now_ms: i64,
	) -> rusqlite::Result<EventInfo> {
		let id = {
			let g = self.lock();
			g.conn
				.prepare_cached(
					"INSERT INTO events (title, description, start_ms, end_ms, channel, kind,
						stream_title, stream_game, host_uid, creator_uid, creator_name,
						created_ms, updated_ms)
					 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
				)?
				.execute(params![
					spec.title,
					spec.description,
					spec.start_ms,
					spec.end_ms,
					spec.channel.map(|c| c as i64),
					kind_str(spec.kind),
					spec.stream_title,
					spec.stream_game,
					spec.host_uid.as_deref().unwrap_or(&creator.uid),
					creator.uid,
					creator.name,
					now_ms
				])?;
			g.conn.last_insert_rowid()
		};
		Ok(self.event(id, Some(&creator.uid), false)?.expect("just inserted"))
	}

	/// Replace what the creator set; `None` if there is no such event.
	pub fn update_event(
		&self,
		id: i64,
		spec: &EventSpec,
		now_ms: i64,
	) -> rusqlite::Result<Option<EventInfo>> {
		let changed = self
			.lock()
			.conn
			.prepare_cached(
				"UPDATE events SET title = ?2, description = ?3, start_ms = ?4, end_ms = ?5,
					channel = ?6, kind = ?7, stream_title = ?8, stream_game = ?9,
					host_uid = COALESCE(?10, host_uid), updated_ms = ?11
				 WHERE id = ?1",
			)?
			.execute(params![
				id,
				spec.title,
				spec.description,
				spec.start_ms,
				spec.end_ms,
				spec.channel.map(|c| c as i64),
				kind_str(spec.kind),
				spec.stream_title,
				spec.stream_game,
				spec.host_uid,
				now_ms
			])?;
		if changed == 0 {
			return Ok(None);
		}
		// Reminders follow the new start time.
		self.lock()
			.conn
			.prepare_cached("DELETE FROM event_notices WHERE event_id = ?1")?
			.execute([id])?;
		self.event(id, None, false)
	}

	pub fn delete_event(&self, id: i64) -> rusqlite::Result<bool> {
		let mut g = self.lock();
		let tx = g.conn.transaction()?;
		let n = tx.prepare_cached("DELETE FROM events WHERE id = ?1")?.execute([id])?;
		tx.prepare_cached("DELETE FROM rsvps WHERE event_id = ?1")?.execute([id])?;
		tx.prepare_cached("DELETE FROM event_notices WHERE event_id = ?1")?.execute([id])?;
		tx.commit()?;
		Ok(n > 0)
	}

	/// One event with RSVP counts, `me`'s answer and optionally all attendees.
	pub fn event(
		&self,
		id: i64,
		me: Option<&str>,
		attendees: bool,
	) -> rusqlite::Result<Option<EventInfo>> {
		let g = self.lock();
		let event = g
			.conn
			.prepare_cached(&format!("SELECT {EVENT_COLUMNS} FROM events e WHERE e.id = :id"))?
			.query_row(&[(":id", &id as &dyn ToSql), (":me", &me)], event_from_row)
			.optional()?;
		let Some(mut event) = event else { return Ok(None) };
		if attendees {
			let mut stmt = g.conn.prepare_cached(
				"SELECT uid, name, status, ts_ms FROM rsvps WHERE event_id = ?1 ORDER BY ts_ms",
			)?;
			event.attendees = stmt
				.query_map([id], |r| {
					Ok(Attendee {
						user: user(r.get(0)?, r.get(1)?),
						status: RsvpStatus::parse(&r.get::<_, String>(2)?),
						ts_ms: r.get(3)?,
					})
				})?
				.collect::<rusqlite::Result<_>>()?;
		}
		Ok(Some(event))
	}

	/// Events by start time.
	pub fn events(
		&self,
		filter: &EventFilter,
		me: Option<&str>,
	) -> rusqlite::Result<Vec<EventInfo>> {
		let g = self.lock();
		let limit = filter.limit.map_or(-1, i64::from);
		let channel = filter.channel.map(|c| c as i64);
		let mut named: Vec<(&str, &dyn ToSql)> =
			vec![(":from", &filter.from_ms), (":limit", &limit), (":me", &me)];
		if let Some(to) = &filter.to_ms {
			named.push((":to", to));
		}
		if let Some(c) = &channel {
			named.push((":channel", c));
		}
		let mut stmt = g.conn.prepare_cached(&filter.sql())?;
		stmt.query_map(named.as_slice(), event_from_row)?.collect()
	}

	/// Events starting in `(after_ms, until_ms]`.
	pub fn events_starting(
		&self,
		after_ms: i64,
		until_ms: i64,
	) -> rusqlite::Result<Vec<EventInfo>> {
		let g = self.lock();
		let me: Option<&str> = None;
		let mut stmt = g.conn.prepare_cached(&format!(
			"SELECT {EVENT_COLUMNS} FROM events e WHERE e.start_ms > :after AND e.start_ms <= :until
			 ORDER BY e.start_ms"
		))?;
		stmt.query_map(
			&[(":after", &after_ms as &dyn ToSql), (":until", &until_ms), (":me", &me)],
			event_from_row,
		)?
		.collect()
	}

	/// Upcoming events created by `uid`.
	pub fn upcoming_events_by(&self, uid: &str, now_ms: i64) -> rusqlite::Result<u64> {
		self.lock()
			.conn
			.prepare_cached(
				"SELECT COUNT(*) FROM events WHERE creator_uid = ?1 AND start_ms >= ?2",
			)?
			.query_row(params![uid, now_ms], |r| r.get::<_, i64>(0))
			.map(|n| n as u64)
	}

	/// Set (`Some`) or withdraw an answer.
	pub fn rsvp(
		&self,
		event_id: i64,
		by: &UserRef,
		status: Option<RsvpStatus>,
		now_ms: i64,
	) -> rusqlite::Result<()> {
		let g = self.lock();
		match status {
			Some(status) => g
				.conn
				.prepare_cached(
					"INSERT INTO rsvps (event_id, uid, name, status, ts_ms) VALUES (?1, ?2, ?3, ?4, ?5)
					 ON CONFLICT (event_id, uid) DO UPDATE SET name = excluded.name,
						status = excluded.status, ts_ms = excluded.ts_ms",
				)?
				.execute(params![event_id, by.uid, by.name, status.as_str(), now_ms])?,
			None => g
				.conn
				.prepare_cached("DELETE FROM rsvps WHERE event_id = ?1 AND uid = ?2")?
				.execute(params![event_id, by.uid])?,
		};
		Ok(())
	}

	/// Record that a reminder was sent; `false` if it already was.
	pub fn mark_notice(&self, event_id: i64, offset_min: i64) -> rusqlite::Result<bool> {
		self.lock()
			.conn
			.prepare_cached(
				"INSERT OR IGNORE INTO event_notices (event_id, offset_min) VALUES (?1, ?2)",
			)?
			.execute(params![event_id, offset_min])
			.map(|n| n > 0)
	}

	/// Stream events of `host_uid` that are not live and whose window
	/// (`start - window` to `end + window`) contains `now_ms`.
	pub fn linkable_events(
		&self,
		host_uid: &str,
		now_ms: i64,
		window_ms: i64,
	) -> rusqlite::Result<Vec<i64>> {
		let g = self.lock();
		let mut stmt = g.conn.prepare_cached(
			"SELECT id FROM events WHERE host_uid = ?1 AND start_ms <= ?2 + ?3
				AND COALESCE(end_ms, start_ms) + ?3 >= ?2 AND kind = 'stream'
				AND live_stream IS NULL ORDER BY start_ms",
		)?;
		stmt.query_map(params![host_uid, now_ms, window_ms], |r| r.get(0))?.collect()
	}

	/// Events currently linked to a stream.
	pub fn events_with_stream(&self, stream: &str) -> rusqlite::Result<Vec<i64>> {
		let g = self.lock();
		let mut stmt = g.conn.prepare_cached("SELECT id FROM events WHERE live_stream = ?1")?;
		stmt.query_map([stream], |r| r.get(0))?.collect()
	}

	pub fn set_live_stream(
		&self,
		event_id: i64,
		stream: Option<&str>,
		now_ms: i64,
	) -> rusqlite::Result<()> {
		self.lock()
			.conn
			.prepare_cached("UPDATE events SET live_stream = ?2, updated_ms = ?3 WHERE id = ?1")?
			.execute(params![event_id, stream, now_ms])?;
		Ok(())
	}
}
