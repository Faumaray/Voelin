//! What the app's foreground services show.
//!
//! Android keeps the microphone and the process of a backgrounded app only
//! while a foreground service with a visible notification runs. The voice
//! service runs while any session has a voice connection (connecting or
//! connected); its notification names the channel and the server, counts
//! the people there and offers Mute, Deafen and Leave. While our stream is
//! live, the screen-sharing service's notification says where it goes and
//! how many watch.

use std::collections::BTreeMap;
use std::sync::Arc;

use voelin_core::stream::{StreamState, ViewerInfo, ViewerState};
use voelin_core::{Event, SessionId, VoiceState};
use voelin_model::{Presence, channel_title};

/// What the voice notification shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceNotice {
	/// The channel we are in ("Chill Zone"), or what is going on.
	pub title: String,
	/// The server and how many are in the channel.
	pub text: String,
	/// The microphone is muted in every voice session (the action unmutes).
	pub muted: bool,
	/// The sound is off in every voice session (the action turns it on).
	pub deafened: bool,
	/// When the first voice session connected (Unix ms), for the elapsed
	/// time; 0 while connecting.
	pub since_ms: u64,
}

/// What the screen-sharing notification shows while our stream is live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenNotice {
	pub title: String,
	pub text: String,
}

/// The notifications that changed with an event: `Some(None)` stops the
/// voice service, or puts the screen notification back to its default.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
	pub voice: Option<Option<VoiceNotice>>,
	pub screen: Option<Option<ScreenNotice>>,
}

#[derive(Default)]
pub struct Foreground {
	sessions: BTreeMap<SessionId, Session>,
}

#[derive(Default)]
struct Session {
	name: Option<String>,
	voice: VoiceState,
	muted: bool,
	deafened: bool,
	own_channel: Option<u64>,
	presence: Arc<Presence>,
	/// When voice connected (Unix ms).
	since_ms: Option<u64>,
	/// Our stream is live.
	live: bool,
	/// Viewers connected to our stream.
	watching: usize,
}

impl Session {
	fn name(&self) -> &str {
		self.name.as_deref().unwrap_or("a server")
	}

	/// Our channel's name (a spacer's text) and how many are in it.
	fn channel(&self) -> Option<(&str, usize)> {
		let id = self.own_channel?;
		let channel = self.presence.channels.get(&id)?;
		let people =
			self.presence.clients.values().filter(|c| c.channel == id && !c.is_query).count();
		Some((channel_title(channel).0, people))
	}
}

fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |d| d.as_millis() as u64)
}

impl Foreground {
	/// Apply `event`; the notifications it changed.
	pub fn update(&mut self, event: &Event) -> Changes {
		self.update_at(event, now_ms())
	}

	fn update_at(&mut self, event: &Event, now_ms: u64) -> Changes {
		let (voice, screen) = (self.notice(), self.screen());
		match event {
			Event::State { session, state } => {
				let s = self.sessions.entry(*session).or_default();
				s.since_ms = match state.voice {
					VoiceState::Connected => s.since_ms.or(Some(now_ms)),
					_ => None,
				};
				s.voice = state.voice;
				s.muted = state.input_muted;
				s.deafened = state.output_muted;
				s.own_channel = state.own_channel;
			}
			Event::ServerInfo { session, name, .. } => {
				self.sessions.entry(*session).or_default().name = Some(name.clone());
			}
			Event::Presence { session, presence } => {
				self.sessions.entry(*session).or_default().presence = presence.clone();
			}
			Event::StreamState { session, state } => {
				let s = self.sessions.entry(*session).or_default();
				s.live = matches!(state, StreamState::Live { .. });
				if !s.live {
					s.watching = 0;
				}
			}
			Event::StreamViewers { session, viewers } => {
				self.sessions.entry(*session).or_default().watching = connected(viewers);
			}
			_ => return Changes::default(),
		}
		let (voice_after, screen_after) = (self.notice(), self.screen());
		Changes {
			voice: (voice_after != voice).then_some(voice_after),
			screen: (screen_after != screen).then_some(screen_after),
		}
	}

	fn active(&self) -> impl Iterator<Item = (&SessionId, &Session)> {
		self.sessions.iter().filter(|(_, s)| s.voice != VoiceState::Disconnected)
	}

	/// Sessions with a voice connection (for the notification's actions).
	pub fn voice_sessions(&self) -> Vec<SessionId> {
		self.active().map(|(id, _)| *id).collect()
	}

	/// The voice notification, `None` without voice.
	pub fn notice(&self) -> Option<VoiceNotice> {
		let active: Vec<&Session> = self.active().map(|(_, s)| s).collect();
		let connecting = active.iter().all(|s| s.voice == VoiceState::Connecting);
		let (title, text) = match active.as_slice() {
			[] => return None,
			[one] if connecting => ("Connecting…".to_owned(), one.name().to_owned()),
			[one] => match one.channel() {
				Some((channel, 1)) => (channel.to_owned(), format!("{} · just you", one.name())),
				Some((channel, n)) => {
					(channel.to_owned(), format!("{} · {n} in voice", one.name()))
				}
				None => ("In voice".to_owned(), one.name().to_owned()),
			},
			several => (
				format!("In voice on {} servers", several.len()),
				several.iter().map(|s| s.name()).collect::<Vec<_>>().join(", "),
			),
		};
		Some(VoiceNotice {
			title,
			text,
			muted: active.iter().all(|s| s.muted),
			deafened: active.iter().all(|s| s.deafened),
			since_ms: active.iter().filter_map(|s| s.since_ms).min().unwrap_or(0),
		})
	}

	/// The screen-sharing notification while our stream is live.
	pub fn screen(&self) -> Option<ScreenNotice> {
		let s = self.sessions.values().find(|s| s.live)?;
		let title = match s.channel() {
			Some((channel, _)) => format!("Live in {channel}"),
			None => "Live".to_owned(),
		};
		let text = match s.watching {
			0 => s.name().to_owned(),
			1 => format!("{} · 1 watching", s.name()),
			n => format!("{} · {n} watching", s.name()),
		};
		Some(ScreenNotice { title, text })
	}
}

fn connected(viewers: &[ViewerInfo]) -> usize {
	viewers.iter().filter(|v| v.state == ViewerState::Connected).count()
}

#[cfg(test)]
mod tests {
	use voelin_core::SessionState;
	use voelin_core::stream::EndReason;
	use voelin_model::{Capabilities, ChannelInfo, ClientInfo, ServerFlavor};

	use super::*;

	fn voice(session: SessionId, voice: VoiceState, input_muted: bool) -> Event {
		Event::State {
			session,
			state: SessionState {
				voice,
				input_muted,
				own_channel: Some(2),
				..SessionState::default()
			},
		}
	}

	fn notice(title: &str, text: &str, muted: bool, since_ms: u64) -> Option<Option<VoiceNotice>> {
		Some(Some(VoiceNotice {
			title: title.into(),
			text: text.into(),
			muted,
			deafened: false,
			since_ms,
		}))
	}

	fn presence(people: u16) -> Event {
		let mut p = Presence::default();
		// A spacer shows its text.
		p.channels.insert(
			2,
			ChannelInfo { id: 2, name: "[cspacer1]Chill Zone".into(), ..Default::default() },
		);
		for id in 1..=people {
			p.clients.insert(id, ClientInfo { id, channel: 2, ..Default::default() });
		}
		Event::Presence { session: 1, presence: Arc::new(p) }
	}

	#[test]
	fn the_voice_service_follows_voice_connections() {
		let mut fg = Foreground::default();
		assert_eq!(fg.update_at(&voice(1, VoiceState::Disconnected, false), 5).voice, None);
		assert_eq!(
			fg.update_at(&voice(1, VoiceState::Connecting, false), 5).voice,
			notice("Connecting…", "a server", false, 0)
		);
		let info = Event::ServerInfo {
			session: 1,
			name: "Home".into(),
			flavor: ServerFlavor::Unknown(String::new()),
			capabilities: Capabilities::default(),
		};
		assert_eq!(fg.update_at(&info, 6).voice, notice("Connecting…", "Home", false, 0));
		assert_eq!(
			fg.update_at(&voice(1, VoiceState::Connected, false), 7).voice,
			notice("In voice", "Home", false, 7)
		);
		// The channel and its people, once presence knows them.
		assert_eq!(
			fg.update_at(&presence(3), 8).voice,
			notice("Chill Zone", "Home · 3 in voice", false, 7)
		);
		assert_eq!(
			fg.update_at(&presence(1), 8).voice,
			notice("Chill Zone", "Home · just you", false, 7)
		);
		assert_eq!(
			fg.update_at(&voice(1, VoiceState::Connected, false), 9).voice,
			None,
			"no change"
		);
		assert_eq!(
			fg.update_at(&voice(1, VoiceState::Connected, true), 9).voice,
			notice("Chill Zone", "Home · just you", true, 7)
		);
		// A second session that is not muted: the action mutes both, and
		// the time counts from the first connection.
		assert_eq!(
			fg.update_at(&voice(2, VoiceState::Connected, false), 20).voice,
			notice("In voice on 2 servers", "Home, a server", false, 7)
		);
		assert_eq!(fg.voice_sessions(), [1, 2]);
		assert_eq!(
			fg.update(&Event::Error { session: 1, message: "x".into() }),
			Changes::default()
		);
		fg.update_at(&voice(2, VoiceState::Disconnected, false), 30);
		assert_eq!(fg.update_at(&voice(1, VoiceState::Disconnected, true), 30).voice, Some(None));
		assert!(fg.voice_sessions().is_empty());
	}

	#[test]
	fn deafen_needs_every_session_deafened() {
		let mut fg = Foreground::default();
		let deaf = |session, output_muted| Event::State {
			session,
			state: SessionState {
				voice: VoiceState::Connected,
				output_muted,
				..Default::default()
			},
		};
		fg.update_at(&deaf(1, true), 1);
		assert!(fg.notice().unwrap().deafened);
		fg.update_at(&deaf(2, false), 1);
		assert!(!fg.notice().unwrap().deafened);
	}

	#[test]
	fn the_screen_notice_says_where_we_are_live() {
		let mut fg = Foreground::default();
		fg.update_at(&voice(1, VoiceState::Connected, false), 1);
		fg.update_at(&presence(2), 1);
		assert_eq!(fg.screen(), None);
		// `StreamState::Live` carries a sink only the engine makes.
		fg.sessions.get_mut(&1).unwrap().live = true;
		let screen = |title: &str, text: &str| {
			Some(Some(ScreenNotice { title: title.into(), text: text.into() }))
		};
		assert_eq!(fg.screen(), screen("Live in Chill Zone", "a server").flatten());
		let viewer = |client, state| ViewerInfo {
			client: tsclientlib::ClientId(client),
			state,
			message: String::new(),
			layer: None,
			estimate: None,
			srtp_profile: None,
			codec: None,
		};
		let viewers = Event::StreamViewers {
			session: 1,
			viewers: vec![viewer(2, ViewerState::Connected), viewer(3, ViewerState::Requested)],
		};
		assert_eq!(
			fg.update_at(&viewers, 3).screen,
			screen("Live in Chill Zone", "a server · 1 watching")
		);
		let ended =
			Event::StreamState { session: 1, state: StreamState::Ended(EndReason::Stopped) };
		assert_eq!(fg.update_at(&ended, 4).screen, Some(None));
	}
}
