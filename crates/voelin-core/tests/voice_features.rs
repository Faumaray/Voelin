//! The engine's contacts and voice feature commands through its public API
//! (without a server: answers without a voice connection, contacts kept
//! in the client database). The round trips against real servers are in
//! `scripts/it-smoke.sh` (`voelinctl engine`).

use std::time::Duration;

use tokio::sync::broadcast;
use voelin_core::settings::Settings;
use voelin_core::{Command, Contact, DownloadTo, Engine, Event, History, Relation, TransferState};
use voelin_model::FileRef;

async fn next<T>(rx: &mut broadcast::Receiver<Event>, mut f: impl FnMut(Event) -> Option<T>) -> T {
	tokio::time::timeout(Duration::from_secs(10), async {
		loop {
			if let Some(t) = f(rx.recv().await.expect("engine events")) {
				return t;
			}
		}
	})
	.await
	.expect("event in time")
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_without_voice_fail_with_their_ids() {
	let engine = Engine::start();
	let mut rx = engine.subscribe();
	engine.send(Command::ListFiles {
		session: 1,
		request: 11,
		channel: 1,
		password: None,
		path: "/docs/".into(),
	});
	let (path, result) = next(&mut rx, |e| match e {
		Event::FileList { request: 11, path, result, .. } => Some((path, result)),
		_ => None,
	})
	.await;
	assert_eq!(path, "/docs");
	assert!(result.unwrap_err().contains("voice"));

	engine.send(Command::DownloadFile {
		session: 1,
		transfer: 12,
		channel: 1,
		password: None,
		path: "/a".into(),
		to: DownloadTo::Memory,
	});
	let state = next(&mut rx, |e| match e {
		Event::Transfer { transfer: 12, state, .. } => Some(state),
		_ => None,
	})
	.await;
	assert!(matches!(state, TransferState::Failed(_)));

	for (request, command) in [
		(
			13,
			Command::SendOfflineMessage {
				session: 1,
				request: 13,
				to_uid: "u=".into(),
				subject: "s".into(),
				text: "t".into(),
			},
		),
		(
			14,
			Command::DeleteFiles {
				session: 1,
				request: 14,
				channel: 1,
				password: None,
				paths: vec!["/a".into()],
			},
		),
		(15, Command::SetAvatar { session: 1, request: 15, image: None }),
	] {
		engine.send(command);
		let result = next(&mut rx, |e| match e {
			Event::RequestDone { request: r, result, .. } if r == request => Some(result),
			_ => None,
		})
		.await;
		assert!(result.is_err(), "request {request}");
	}
	engine.send(Command::ListOfflineMessages { session: 1, request: 16 });
	let result = next(&mut rx, |e| match e {
		Event::OfflineMessages { request: 16, result, .. } => Some(result),
		_ => None,
	})
	.await;
	assert!(result.is_err());
}

/// A file linked in chat is fetched through the voice connection.
#[tokio::test(flavor = "multi_thread")]
async fn chat_file_needs_voice() {
	let engine = Engine::start();
	let mut rx = engine.subscribe();
	let file = FileRef {
		server_uid: Some("other=".into()),
		channel: 1,
		path: "/".into(),
		name: "a".into(),
		..Default::default()
	};
	// Without a server id of our own the link cannot be checked: it is
	// tried (and fails without voice).
	engine.send(Command::DownloadChatFile {
		session: 1,
		transfer: 1,
		file,
		password: None,
		to: DownloadTo::Memory,
	});
	let state = next(&mut rx, |e| match e {
		Event::Transfer { transfer: 1, state, .. } => Some(state),
		_ => None,
	})
	.await;
	assert!(matches!(state, TransferState::Failed(e) if e.contains("voice")));
}

#[tokio::test(flavor = "multi_thread")]
async fn contacts_are_kept_in_the_database() {
	let dir = std::env::temp_dir().join(format!("voelin-contacts-engine-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	let path = dir.join("client.db");
	{
		let engine = Engine::start_with(Settings::in_memory(), History::open(&path).unwrap());
		let mut rx = engine.subscribe();
		let friend = Contact {
			nickname: "Alice".into(),
			relation: Relation::Friend,
			volume: 0.5,
			..Contact::new("alice=")
		};
		engine.send(Command::SetContact { contact: Box::new(friend) });
		engine.send(Command::SetContact {
			contact: Box::new(Contact { relation: Relation::Blocked, ..Contact::new("troll=") }),
		});
		engine.send(Command::RemoveContact { uid: "troll=".into() });
		let contacts = next(&mut rx, |e| match e {
			Event::ContactsChanged { contacts }
				if contacts.len() == 1 && contacts[0].uid == "alice=" =>
			{
				Some(contacts)
			}
			_ => None,
		})
		.await;
		assert_eq!(contacts[0].volume, 0.5);
		assert!(contacts[0].added_ms > 0);
		// Friends offline everywhere are reported so.
		let (uid, sessions) = next(&mut rx, |e| match e {
			Event::FriendPresence { uid, sessions } => Some((uid, sessions)),
			_ => None,
		})
		.await;
		assert_eq!((uid.as_str(), sessions.len()), ("alice=", 0));
		engine.history().flush();
	}
	// A new engine attaches the database later: the contacts come back.
	let engine = Engine::start();
	let mut rx = engine.subscribe();
	engine.send(Command::AttachHistory(History::open(&path).unwrap()));
	next(&mut rx, |e| match e {
		Event::ContactsChanged { contacts } if contacts.len() == 1 => Some(()),
		_ => None,
	})
	.await;
	assert_eq!(engine.contacts()[0].nickname, "Alice");
	drop(engine);
	let _ = std::fs::remove_dir_all(dir);
}
