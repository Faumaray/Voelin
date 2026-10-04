use std::time::Instant;

use serde_json::json;
use voelin_gateway_proto::{EventKind, EventSpec, RsvpStatus, StreamEntry, StreamSource, UserRef};
use voelin_model::{ChatMessage, ChatTarget};

use super::*;

fn msg(target: ChatTarget, text: &str, ts_ms: i64) -> ChatMessage {
	ChatMessage {
		target,
		author_name: "a".into(),
		author_uid: Some("u".into()),
		author_id: None,
		text: text.into(),
		ts_ms,
		via_relay: true,
		blocked: false,
	}
}

fn alice() -> UserRef {
	UserRef { uid: "alice".into(), name: "Alice".into() }
}

fn bob() -> UserRef {
	UserRef { uid: "bob".into(), name: "Bob".into() }
}

fn texts(entries: &[voelin_gateway_proto::HistoryEntry]) -> Vec<&str> {
	entries.iter().map(|e| e.message.text.as_str()).collect()
}

const CH1: ChatTarget = ChatTarget::Channel(1);

#[test]
fn tokens_expire() {
	let db = Db::in_memory().unwrap();
	let info = TokenInfo { uid: "u".into(), cldbid: 5, expires: 100 };
	db.add_token("h", &info).unwrap();
	assert_eq!(db.token("h", 50).unwrap(), Some(info));
	assert_eq!(db.token("h", 150).unwrap(), None);
	assert_eq!(db.token("x", 50).unwrap(), None);
	assert_eq!(db.prune_tokens(150).unwrap(), 1);
}

#[test]
fn migrates_the_original_schema() {
	// A database as the first gateway version left it.
	let conn = Connection::open_in_memory().unwrap();
	conn.execute_batch(
		"CREATE TABLE tokens (hash TEXT PRIMARY KEY, uid TEXT NOT NULL, cldbid INTEGER NOT NULL,
			expires INTEGER NOT NULL);
		CREATE TABLE messages (id INTEGER PRIMARY KEY, target TEXT NOT NULL, ts_ms INTEGER NOT NULL,
			author_uid TEXT, author_name TEXT NOT NULL, text TEXT NOT NULL);
		CREATE INDEX messages_by_target ON messages (target, id);
		CREATE TABLE audit (ts INTEGER NOT NULL, uid TEXT, action TEXT NOT NULL, detail TEXT);
		INSERT INTO tokens VALUES ('h', 'u', 5, 9999999999);
		INSERT INTO messages VALUES (7, 'channel/1', 1000, 'u', 'Alice', 'old one');
		INSERT INTO messages VALUES (9, 'server', 2000, NULL, 'Bob', 'old two');
		INSERT INTO audit VALUES (1, 'u', 'login', 'signature');",
	)
	.unwrap();
	let db = Db::init(conn).unwrap();
	assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
	assert_eq!(db.token("h", 0).unwrap().unwrap().cldbid, 5);
	let (page, _) = db.history(&CH1, &HistoryOptions::default(), None).unwrap();
	assert_eq!(page.len(), 1);
	assert_eq!((page[0].id, page[0].message.text.as_str(), page[0].rev), (7, "old one", 7));
	assert_eq!(page[0].message.author_name, "Alice");
	let server = db.message(9, None).unwrap().unwrap();
	assert_eq!((server.message.target, server.message.author_uid), (ChatTarget::Server, None));
	assert_eq!(
		db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM audit", [], |r| r.get::<_, i64>(0)))
			.unwrap(),
		1
	);
	// New messages continue after the old ids, with larger revisions.
	let new = db.add_message(msg(CH1, "new", 3000), None).unwrap();
	assert!(new.id > 9 && new.rev > 9);
	// Opening again is a no-op.
	let conn = db.0.into_inner().unwrap().conn;
	let db = Db::init(conn).unwrap();
	assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
	assert_eq!(db.history(&CH1, &HistoryOptions::default(), None).unwrap().0.len(), 2);
}

#[test]
fn ids_are_never_reused() {
	let db = Db::in_memory().unwrap();
	let first = db.add_message(msg(CH1, "a", 1), None).unwrap();
	assert_eq!(db.prune_messages(10).unwrap(), 1);
	let second = db.add_message(msg(CH1, "b", 20), None).unwrap();
	assert!(second.id > first.id);
	assert!(second.rev > first.rev);
}

#[test]
fn history_pages_per_target() {
	let db = Db::in_memory().unwrap();
	for i in 0..5 {
		db.add_message(msg(CH1, &format!("c{i}"), i), None).unwrap();
		db.add_message(msg(ChatTarget::Server, &format!("s{i}"), i), None).unwrap();
	}
	let opts = |limit| HistoryOptions { limit: Some(limit), ..Default::default() };
	let (last, more) = db.history(&CH1, &opts(2), None).unwrap();
	assert_eq!(texts(&last), ["c3", "c4"]);
	assert!(more);
	let before = HistoryOptions { before: Some(last[0].id), ..opts(10) };
	let (older, more) = db.history(&CH1, &before, None).unwrap();
	assert_eq!(texts(&older), ["c0", "c1", "c2"]);
	assert!(!more);
	let after = HistoryOptions { after: Some(older[0].id), ..opts(2) };
	let (newer, more) = db.history(&CH1, &after, None).unwrap();
	assert_eq!(texts(&newer), ["c1", "c2"]);
	assert!(more);
	let both = HistoryOptions {
		after: Some(older[0].id),
		before: Some(last[1].id),
		limit: None,
		..Default::default()
	};
	assert_eq!(texts(&db.history(&CH1, &both, None).unwrap().0), ["c1", "c2", "c3"]);
	// Time cursors.
	let since = HistoryOptions { after_ms: Some(2), ..Default::default() };
	assert_eq!(texts(&db.history(&CH1, &since, None).unwrap().0), ["c3", "c4"]);
	let until = HistoryOptions { before_ms: Some(2), ..Default::default() };
	assert_eq!(texts(&db.history(&CH1, &until, None).unwrap().0), ["c0", "c1"]);
	let none = HistoryOptions { after_ms: Some(99), ..Default::default() };
	assert!(db.history(&CH1, &none, None).unwrap().0.is_empty());
	assert_eq!(db.history(&ChatTarget::Server, &opts(10), None).unwrap().0.len(), 5);
	db.prune_messages(10).unwrap();
	assert!(db.history(&ChatTarget::Server, &opts(10), None).unwrap().0.is_empty());
}

/// More than 10k messages: no cap on page size, cursors in both directions,
/// topics, and every query path on an index.
#[test]
fn large_history_without_caps() {
	let db = Db::in_memory().unwrap();
	let topic = db.create_topic(&CH1, "big", &alice(), None, 0).unwrap();
	let n = 12_000;
	let start = Instant::now();
	let batch: Vec<_> = (0..n)
		.map(|i| {
			let target = if i % 3 == 2 { ChatTarget::Server } else { CH1 };
			let topic_id = (i % 5 == 0 && target == CH1).then_some(topic.id);
			(msg(target, &format!("m{i}"), i as i64), topic_id)
		})
		.collect();
	db.add_messages(batch).unwrap();
	let inserted = start.elapsed();
	let ch1 = (0..n).filter(|i| i % 3 != 2).count();
	let in_topic = (0..n).filter(|i| i % 3 != 2 && i % 5 == 0).count();

	let start = Instant::now();
	let (all, more) = db.history(&CH1, &HistoryOptions::default(), None).unwrap();
	assert_eq!(all.len(), ch1);
	assert!(!more);
	assert!(all.windows(2).all(|w| w[0].id < w[1].id));

	// Walk backwards and forwards in pages of 1000.
	let mut seen = 0;
	let mut before = None;
	loop {
		let opts = HistoryOptions { before, limit: Some(1000), ..Default::default() };
		let (page, more) = db.history(&CH1, &opts, None).unwrap();
		seen += page.len();
		before = page.first().map(|e| e.id);
		if !more {
			break;
		}
	}
	assert_eq!(seen, ch1);
	let mut seen = 0;
	let mut after = Some(0);
	loop {
		let opts = HistoryOptions { after, limit: Some(1000), ..Default::default() };
		let (page, more) = db.history(&CH1, &opts, None).unwrap();
		seen += page.len();
		after = page.last().map(|e| e.id);
		if !more {
			break;
		}
	}
	assert_eq!(seen, ch1);

	let topic_opts = HistoryOptions { topic: Some(topic.id), ..Default::default() };
	let (topic_page, _) = db.history(&CH1, &topic_opts, None).unwrap();
	assert_eq!(topic_page.len(), in_topic);
	assert!(topic_page.iter().all(|e| e.topic_id == Some(topic.id)));
	let main = HistoryOptions { exclude_topics: true, ..Default::default() };
	assert_eq!(db.history(&CH1, &main, None).unwrap().0.len(), ch1 - in_topic);
	let t = db.topic(topic.id).unwrap().unwrap();
	assert_eq!(t.message_count as usize, in_topic);
	let (since, _) = db.sync(&CH1, all[ch1 - 10].rev, None, None).unwrap();
	assert_eq!(since.len(), 9);
	let queried = start.elapsed();
	eprintln!("{n} messages: insert {inserted:?}, queries {queried:?}");

	// Every shape of history query runs on an index.
	let shapes = [
		HistoryOptions::default(),
		HistoryOptions { before: Some(1), ..Default::default() },
		HistoryOptions { after: Some(1), ..Default::default() },
		HistoryOptions { before: Some(1), after: Some(1), ..Default::default() },
		HistoryOptions { topic: Some(1), ..Default::default() },
		HistoryOptions { topic: Some(1), after: Some(1), ..Default::default() },
		HistoryOptions { exclude_topics: true, before: Some(1), ..Default::default() },
	];
	for shape in shapes {
		assert_indexed(&db, &shape.sql());
	}
	for sql in [
		"SELECT id FROM messages WHERE target = ?1 AND ts_ms > ?2 ORDER BY ts_ms, id LIMIT 1",
		"SELECT id FROM messages WHERE target = ?1 AND ts_ms < ?2 ORDER BY ts_ms DESC, id DESC LIMIT 1",
		"SELECT m.id FROM messages m WHERE m.target = ?1 AND m.rev > ?2 ORDER BY m.rev",
		"SELECT message_id, emoji, COUNT(*), MIN(ts_ms) AS first FROM reactions
		 WHERE message_id BETWEEN ?1 AND ?2 GROUP BY message_id, emoji",
		"SELECT message_id FROM pins WHERE target = ?1 ORDER BY ts_ms DESC",
		"DELETE FROM messages WHERE ts_ms < ?1",
		"SELECT id FROM topics WHERE target = ?1 AND archived = 0 ORDER BY last_activity_ms DESC",
		"SELECT id FROM topics WHERE target = ?1 AND title = ?2",
		"SELECT id FROM activity WHERE id < ?1 ORDER BY id DESC",
		"DELETE FROM activity WHERE ts_ms < ?1",
		"SELECT id FROM events WHERE start_ms > ?1 AND start_ms <= ?2",
		"SELECT id FROM events WHERE host_uid = ?1 AND start_ms <= ?2",
		"SELECT id FROM events WHERE live_stream = ?1",
		"SELECT COUNT(*) FROM events WHERE creator_uid = ?1 AND start_ms >= ?2",
		"SELECT uid FROM rsvps WHERE event_id = ?1",
	] {
		assert_indexed(&db, sql);
	}
	for filter in [
		EventFilter { from_ms: 0, ..Default::default() },
		EventFilter { from_ms: 0, channel: Some(1), to_ms: Some(5), ..Default::default() },
	] {
		assert_indexed(&db, &filter.sql());
	}
}

fn assert_indexed(db: &Db, sql: &str) {
	let plan = db.query_plan(sql);
	for line in &plan {
		let full_scan = line.starts_with("SCAN");
		assert!(!full_scan, "{sql}\n{plan:?}");
	}
}

#[test]
fn reactions_and_revisions() {
	let db = Db::in_memory().unwrap();
	let m = db.add_message(msg(CH1, "hi", 1), None).unwrap();
	let other = db.add_message(msg(CH1, "other", 2), None).unwrap();
	assert_eq!(db.add_reaction(m.id, "👍", &alice(), 10).unwrap(), Some(1));
	assert_eq!(db.add_reaction(m.id, "👍", &alice(), 11).unwrap(), None);
	assert_eq!(db.add_reaction(m.id, "👍", &bob(), 12).unwrap(), Some(2));
	assert_eq!(db.add_reaction(m.id, "🏳️‍🌈", &bob(), 13).unwrap(), Some(1));
	assert_eq!(db.reaction_kinds(m.id).unwrap(), 2);
	let entry = db.message(m.id, Some("alice")).unwrap().unwrap();
	assert_eq!(entry.reactions.len(), 2);
	assert_eq!(
		(entry.reactions[0].emoji.as_str(), entry.reactions[0].count, entry.reactions[0].me),
		("👍", 2, true)
	);
	assert!(!entry.reactions[1].me);
	assert!(entry.rev > other.rev, "reacting bumps the revision");
	let (changed, _) = db.sync(&CH1, other.rev, None, None).unwrap();
	assert_eq!(texts(&changed), ["hi"]);
	assert_eq!(db.reactors(m.id, "👍").unwrap(), [alice(), bob()]);
	assert!(db.has_reaction(m.id, "👍", "bob").unwrap());
	assert_eq!(db.remove_reaction(m.id, "👍", "alice", 20).unwrap(), Some(1));
	assert_eq!(db.remove_reaction(m.id, "👍", "alice", 21).unwrap(), None);
	// A page carries the reactions of its messages only.
	let (page, _) = db.history(&CH1, &HistoryOptions::default(), Some("bob")).unwrap();
	assert_eq!(page[0].reactions.len(), 2);
	assert!(page[0].reactions.iter().all(|r| r.me));
	assert!(page[1].reactions.is_empty());
}

#[test]
fn pins_survive_retention() {
	let db = Db::in_memory().unwrap();
	let old = db.add_message(msg(CH1, "old pinned", 1), None).unwrap();
	let gone = db.add_message(msg(CH1, "old", 2), None).unwrap();
	db.add_reaction(gone.id, "x", &alice(), 3).unwrap();
	assert!(db.pin(old.id, &CH1, &alice(), 5).unwrap());
	assert!(!db.pin(old.id, &CH1, &bob(), 6).unwrap());
	assert_eq!(db.pin_count(&CH1).unwrap(), 1);
	let pins = db.pins(&CH1, None).unwrap();
	assert_eq!(
		(pins[0].entry.id, pins[0].by.clone(), pins[0].entry.pinned),
		(old.id, alice(), true)
	);
	assert_eq!(db.prune_messages(100).unwrap(), 1);
	assert!(db.message(gone.id, None).unwrap().is_none());
	assert!(db.reactors(gone.id, "x").unwrap().is_empty());
	assert!(db.message(old.id, None).unwrap().unwrap().pinned);
	assert_eq!(db.unpin(old.id, 7).unwrap(), Some(alice()));
	assert_eq!(db.unpin(old.id, 8).unwrap(), None);
	assert!(db.pins(&CH1, None).unwrap().is_empty());
}

#[test]
fn topics() {
	let db = Db::in_memory().unwrap();
	let root = db.add_message(msg(CH1, "let's raid", 1), None).unwrap();
	let t = db.create_topic(&CH1, "Raid", &alice(), Some(root.id), 10).unwrap();
	let other = db.create_topic(&CH1, "Other", &bob(), None, 20).unwrap();
	db.add_message(msg(CH1, "in topic", 30), Some(t.id)).unwrap();
	let list = db.topics(&CH1, false).unwrap();
	assert_eq!(list.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["Raid", "Other"]);
	assert_eq!(
		(list[0].message_count, list[0].last_activity_ms, list[0].root_message_id),
		(1, 30, Some(root.id))
	);
	assert_eq!(db.topic_by_title(&CH1, "Raid").unwrap().unwrap().id, t.id);
	let archived = db.update_topic(other.id, Some("Renamed"), Some(true)).unwrap().unwrap();
	assert!(archived.archived && archived.title == "Renamed");
	assert_eq!(db.topics(&CH1, false).unwrap().len(), 1);
	assert_eq!(db.topics(&CH1, true).unwrap().len(), 2);
	assert_eq!(db.open_topic_count(&CH1).unwrap(), 1);
	assert!(db.topic_by_title(&CH1, "Renamed").unwrap().is_none());
	assert!(db.update_topic(999, Some("x"), None).unwrap().is_none());
}

#[test]
fn events_and_rsvps() {
	let db = Db::in_memory().unwrap();
	let spec = EventSpec {
		title: "Stream night".into(),
		start_ms: 10_000,
		end_ms: Some(20_000),
		channel: Some(1),
		kind: EventKind::Stream,
		..Default::default()
	};
	let e = db.create_event(&spec, &alice(), 1).unwrap();
	assert_eq!(e.spec.host_uid.as_deref(), Some("alice"));
	let general = db
		.create_event(
			&EventSpec { title: "Later".into(), start_ms: 50_000, ..Default::default() },
			&bob(),
			1,
		)
		.unwrap();
	db.rsvp(e.id, &bob(), Some(RsvpStatus::Going), 2).unwrap();
	db.rsvp(e.id, &alice(), Some(RsvpStatus::Maybe), 3).unwrap();
	db.rsvp(e.id, &alice(), Some(RsvpStatus::Going), 4).unwrap();
	let got = db.event(e.id, Some("bob"), true).unwrap().unwrap();
	assert_eq!((got.going, got.maybe, got.my_rsvp), (2, 0, Some(RsvpStatus::Going)));
	assert_eq!(got.attendees.len(), 2);
	db.rsvp(e.id, &bob(), None, 5).unwrap();
	assert_eq!(db.event(e.id, Some("bob"), false).unwrap().unwrap().my_rsvp, None);

	let all = db.events(&EventFilter { from_ms: 0, ..Default::default() }, None).unwrap();
	assert_eq!(all.iter().map(|e| e.id).collect::<Vec<_>>(), [e.id, general.id]);
	// Ended events are not listed from later on; running ones are.
	let from = |from_ms| EventFilter { from_ms, ..Default::default() };
	assert_eq!(db.events(&from(15_000), None).unwrap().len(), 2);
	assert_eq!(db.events(&from(25_000), None).unwrap().len(), 1);
	let ch = EventFilter { from_ms: 0, channel: Some(1), ..Default::default() };
	assert_eq!(db.events(&ch, None).unwrap().len(), 1);
	assert_eq!(db.upcoming_events_by("bob", 5).unwrap(), 1);
	assert_eq!(db.events_starting(9_000, 10_000).unwrap().len(), 1);
	assert!(db.events_starting(10_000, 11_000).unwrap().is_empty());
	assert!(db.mark_notice(e.id, 15).unwrap());
	assert!(!db.mark_notice(e.id, 15).unwrap());

	// The host's stream links within the window.
	assert_eq!(db.linkable_events("alice", 9_000, 5_000).unwrap(), [e.id]);
	assert!(db.linkable_events("alice", 30_000, 5_000).unwrap().is_empty());
	assert!(db.linkable_events("bob", 9_000, 5_000).unwrap().is_empty());
	db.set_live_stream(e.id, Some("s1"), 6).unwrap();
	assert_eq!(db.events_with_stream("s1").unwrap(), [e.id]);
	assert!(db.linkable_events("alice", 9_000, 5_000).unwrap().is_empty());

	let moved = db
		.update_event(e.id, &EventSpec { start_ms: 12_000, host_uid: None, ..spec.clone() }, 7)
		.unwrap()
		.unwrap();
	assert_eq!((moved.spec.start_ms, moved.spec.host_uid.as_deref()), (12_000, Some("alice")));
	assert!(db.mark_notice(e.id, 15).unwrap(), "a moved event is reminded again");
	assert!(db.update_event(999, &spec, 8).unwrap().is_none());
	assert!(db.delete_event(e.id).unwrap());
	assert!(!db.delete_event(e.id).unwrap());
	assert!(db.event(e.id, None, true).unwrap().is_none());
}

#[test]
fn streams_and_activity() {
	let db = Db::in_memory().unwrap();
	let entry = StreamEntry {
		id: "s1".into(),
		stream_id: Some("s1".into()),
		streamer: alice(),
		client_id: Some(3),
		channel: Some(1),
		title: "t".into(),
		kind: "screen".into(),
		started_ms: 1,
		viewers: None,
		source: StreamSource::Registered,
		event_id: None,
	};
	db.save_stream(&entry).unwrap();
	db.save_stream(&StreamEntry { title: "u".into(), ..entry.clone() }).unwrap();
	assert_eq!(db.streams().unwrap()[0].title, "u");
	db.delete_stream("s1").unwrap();
	assert!(db.streams().unwrap().is_empty());

	for i in 0..5 {
		db.add_activity(
			NewActivity {
				kind: "pinned",
				actor: Some(&alice()),
				channel: Some(1),
				ref_id: Some(i.to_string()),
				text: format!("a{i}"),
				data: if i == 0 { Value::Null } else { json!({ "i": i }) },
			},
			i,
		)
		.unwrap();
	}
	let (page, more) = db.activity(None, Some(2)).unwrap();
	assert_eq!(page.iter().map(|a| a.text.as_str()).collect::<Vec<_>>(), ["a4", "a3"]);
	assert!(more);
	assert_eq!(page[0].data["i"], 4);
	let (rest, more) = db.activity(Some(page[1].id), None).unwrap();
	assert_eq!(rest.len(), 3);
	assert!(!more);
	assert_eq!(rest[2].data, Value::Null);
	assert_eq!(db.prune_activity(3).unwrap(), 3);
	assert_eq!(db.activity(None, None).unwrap().0.len(), 2);
}

#[test]
fn config_table() {
	let db = Db::in_memory().unwrap();
	db.set_config("a.b", &json!([1, 2]), Some("alice")).unwrap();
	db.set_config("a.b", &json!([3]), None).unwrap();
	db.set_config("c", &json!(null), None).unwrap();
	assert_eq!(
		db.config_values().unwrap(),
		[("a.b".to_string(), json!([3])), ("c".to_string(), json!(null))]
	);
	db.delete_config("a.b").unwrap();
	assert_eq!(db.config_values().unwrap().len(), 1);
}
