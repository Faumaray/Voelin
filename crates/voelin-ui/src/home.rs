//! The home page (design mockup 01): live streams on our servers, what is
//! happening there (events, scheduled streams, server news), and the
//! Library of recordings and clips. Friends are in `social.rs`, the recent
//! chats in `messages.rs`.

use std::path::{Path, PathBuf};

use slint::ComponentHandle;
use voelin_core::VoiceState;

use crate::app::{App, Bridge, HappeningItem, LiveItem, Page, RecordingItem};
use crate::vm;
use crate::vm::social::{event_when, plain};

/// What a "What's Happening" entry opens.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Happening {
	Event(i64, i64),
	News(i64),
}

/// "screen" → "Screen".
fn kind_label(kind: &str) -> String {
	let mut chars = kind.chars();
	chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

impl App {
	/// Streams on our servers: those the server lists (any channel; we can
	/// watch them at once), the gateways' directories, and clients flagged
	/// streaming.
	fn live_rooms(&self) -> Vec<LiveItem> {
		let mut rooms = Vec::new();
		for b in &self.bookmarks {
			let Some(view) = self.sessions.get(&b.id) else { continue };
			let channel_of = |cid: Option<u64>| {
				cid.and_then(|c| view.presence.channels.get(&c))
					.map(|c| vm::tree::channel_title(c).0.to_owned())
					.unwrap_or_default()
			};
			let backdrop = vm::avatar::tint(&b.name);
			let item = |id: String,
			            title: String,
			            client: Option<u16>,
			            uid: Option<&str>,
			            name: String| {
				let avatar = match (client, uid) {
					(Some(c), _) if view.avatar(c).is_some() => vm::avatar::image(view.avatar(c)),
					(_, Some(uid)) => self.avatar_of(uid),
					_ => slint::Image::default(),
				};
				LiveItem {
					session: b.id as i32,
					id: id.into(),
					title: title.into(),
					initials: vm::avatar::initials(&name).into(),
					tint: vm::avatar::tint(&name),
					avatar,
					streamer: name.into(),
					server: b.name.clone().into(),
					backdrop,
					..Default::default()
				}
			};
			let mut covered: Vec<u16> = Vec::new();
			if view.streams_available() {
				for s in &view.streams {
					let name = view.nickname(s.streamer.0);
					let title =
						if s.name.is_empty() { format!("{name}'s stream") } else { s.name.clone() };
					let mut room = item(s.id.clone(), title, Some(s.streamer.0), None, name);
					room.channel = channel_of(view.channel_of(s.streamer.0)).into();
					room.viewers = view.stream_viewers.get(&s.id).map_or(0, |v| *v as i32);
					room.kind = match s.kind {
						voelin_core::stream::StreamKind::Screen => "Screen",
						voelin_core::stream::StreamKind::Window => "Window",
						voelin_core::stream::StreamKind::Camera => "Camera",
						voelin_core::stream::StreamKind::Other(_) => "",
					}
					.into();
					room.watchable = view.state.own_client != Some(s.streamer.0);
					covered.push(s.streamer.0);
					rooms.push(room);
				}
			}
			for e in &view.extra.directory {
				if e.client_id.is_some_and(|c| covered.contains(&c)) {
					continue;
				}
				let title = if e.title.is_empty() {
					format!("{}'s stream", e.streamer.name)
				} else {
					e.title.clone()
				};
				let mut room = item(
					e.id.clone(),
					title,
					e.client_id,
					Some(&e.streamer.uid),
					e.streamer.name.clone(),
				);
				room.channel = channel_of(e.channel).into();
				room.viewers = e.viewers.map_or(0, |v| v as i32);
				room.kind = kind_label(&e.kind).into();
				covered.extend(e.client_id);
				rooms.push(room);
			}
			for c in view.presence.clients.values() {
				if c.streaming != Some(true)
					|| covered.contains(&c.id)
					|| Some(c.id) == view.state.own_client
				{
					continue;
				}
				let mut room = item(
					format!("client/{}", c.id),
					format!("{} is live", c.nickname),
					Some(c.id),
					c.uid.as_deref(),
					c.nickname.clone(),
				);
				room.channel = channel_of(Some(c.channel)).into();
				rooms.push(room);
			}
		}
		rooms
	}

	/// Events, scheduled streams and server news of our servers, what is
	/// soonest first.
	pub(crate) fn happenings(&self) -> Vec<(HappeningItem, Happening)> {
		let now = crate::social::now_ms();
		let mut events: Vec<(i64, HappeningItem, Happening)> = Vec::new();
		let mut news = Vec::new();
		for b in &self.bookmarks {
			let Some(view) = self.sessions.get(&b.id) else { continue };
			for e in &view.extra.events {
				let ends = e.spec.end_ms.unwrap_or(e.spec.start_ms + 3 * 3_600_000);
				if ends < now {
					continue;
				}
				let stream = e.spec.kind == voelin_gateway_proto::EventKind::Stream;
				events.push((
					e.spec.start_ms,
					HappeningItem {
						kind: if stream { "stream" } else { "event" }.into(),
						title: e.spec.title.clone().into(),
						subtitle: if stream {
							format!("Scheduled stream · {}", b.name)
						} else {
							format!("Event · {}", b.name)
						}
						.into(),
						when: event_when(e.spec.start_ms, e.spec.end_ms).into(),
						session: b.id as i32,
						id: e.id as i32,
						live: e.live_stream.is_some(),
					},
					Happening::Event(b.id, e.id),
				));
			}
			let details = &view.presence.server;
			for (title, text) in [
				(format!("Welcome to {}", b.name), &details.welcome_message),
				(format!("News of {}", b.name), &details.host_message),
			] {
				let text = plain(text);
				if !text.is_empty() {
					news.push((
						HappeningItem {
							kind: "news".into(),
							title: title.into(),
							subtitle: text.into(),
							session: b.id as i32,
							..Default::default()
						},
						Happening::News(b.id),
					));
				}
			}
		}
		events.sort_by_key(|(start, ..)| *start);
		events
			.into_iter()
			.map(|(_, item, h)| (item, h))
			.take(6)
			.chain(news.into_iter().take(3))
			.collect()
	}

	/// Live rooms and what is happening (friends: `refresh_people`, chats:
	/// `refresh_chats`).
	pub(crate) fn refresh_home(&self) {
		let m = &self.models.social;
		vm::list::sync(&m.live, &self.live_rooms());
		let items: Vec<HappeningItem> = self.happenings().into_iter().map(|(i, _)| i).collect();
		vm::list::sync(&m.happenings, &items);
	}

	/// A live room's card: watch it at once in our channel, else go to its
	/// server (and the streamer's channel, watching once there).
	pub(crate) fn watch_live(&mut self, session: i64, id: &str) {
		self.show_server(session);
		let Some(view) = self.sessions.get(&session) else { return };
		if view.streams_available() && view.streams.iter().any(|s| s.id == id) {
			self.watch_stream(id.to_owned());
			return;
		}
		let client = view
			.extra
			.directory
			.iter()
			.find(|e| e.id == id)
			.and_then(|e| e.client_id)
			.or_else(|| id.strip_prefix("client/").and_then(|c| c.parse().ok()));
		match client {
			Some(client) if view.state.voice == VoiceState::Connected => self.watch_client(client),
			_ => self.set_status("Connect with voice to watch."),
		}
	}

	pub(crate) fn open_happening(&mut self, index: i32) {
		let Some((_, target)) =
			usize::try_from(index).ok().and_then(|i| self.happenings().into_iter().nth(i))
		else {
			return;
		};
		match target {
			Happening::Event(session, _) => {
				self.select_server(session);
				self.navigate(|nav| nav.invoke_show(Page::Events));
			}
			Happening::News(session) => self.show_server(session),
		}
	}

	/// Connect a bookmark with voice (the Join buttons).
	pub(crate) fn connect_server(&mut self, id: i64) {
		self.select_server(id);
		self.connect_voice();
	}

	/// The folder of recordings and clips.
	fn recordings_dir(&self) -> PathBuf {
		voelin_core::studio::recording_dir(&self.prefs)
	}

	/// List the recordings and clips (when the Library opens).
	pub(crate) fn load_library(&mut self) {
		// Sample data shows sample recordings, not the user's.
		if self.demo_ui {
			if let Some(ui) = self.ui.upgrade() {
				ui.global::<Bridge>().set_recordings_dir("~/Videos/Voelin".into());
			}
			return;
		}
		let dir = self.recordings_dir();
		let items = recordings_in(&dir);
		vm::list::sync(&self.models.social.recordings, &items);
		if let Some(ui) = self.ui.upgrade() {
			ui.global::<Bridge>().set_recordings_dir(dir.display().to_string().into());
		}
	}

	pub(crate) fn open_recording(&mut self, path: &str) {
		if let Err(e) = crate::app::open_path(Path::new(path)) {
			self.set_status(format!("Cannot open {path}: {e}"));
		}
	}

	pub(crate) fn open_recordings_folder(&mut self) {
		let dir = self.recordings_dir();
		if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| crate::app::open_path(&dir)) {
			self.set_status(format!("Cannot open {}: {e}", dir.display()));
		}
	}
}

/// The videos in `dir`, newest first.
pub(crate) fn recordings_in(dir: &Path) -> Vec<RecordingItem> {
	let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
	let mut files: Vec<(std::time::SystemTime, RecordingItem)> = entries
		.flatten()
		.filter_map(|e| {
			let path = e.path();
			let ext = path.extension()?.to_str()?.to_ascii_lowercase();
			if !["webm", "mkv", "mp4", "mov"].contains(&ext.as_str()) {
				return None;
			}
			let meta = e.metadata().ok()?;
			let modified = meta.modified().ok()?;
			let name = path.file_name()?.to_string_lossy().to_string();
			let when = chrono::DateTime::<chrono::Local>::from(modified).format("%-d %b %H:%M");
			Some((
				modified,
				RecordingItem {
					clip: name.to_lowercase().contains("clip"),
					detail: format!("{} · {when}", vm::chat::size_text(meta.len())).into(),
					name: name.into(),
					path: path.display().to_string().into(),
				},
			))
		})
		.collect();
	files.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
	files.into_iter().map(|(_, item)| item).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn library_lists_videos() {
		let dir = std::env::temp_dir().join(format!("voelin-library-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join("raid.webm"), b"1234").unwrap();
		std::fs::write(dir.join("clip-boss.mkv"), b"12").unwrap();
		std::fs::write(dir.join("notes.txt"), b"x").unwrap();
		let items = recordings_in(&dir);
		let mut names: Vec<String> = items.iter().map(|i| i.name.to_string()).collect();
		names.sort();
		assert_eq!(names, ["clip-boss.mkv", "raid.webm"]);
		assert!(items.iter().any(|i| i.clip && i.detail.starts_with("2 B")));
		assert!(recordings_in(&dir.join("missing")).is_empty());
		std::fs::remove_dir_all(&dir).unwrap();
		assert_eq!(kind_label("screen"), "Screen");
		assert_eq!(kind_label(""), "");
	}
}
