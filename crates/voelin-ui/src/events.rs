//! What a server's gateway tells besides chat: its events (the events
//! page, RSVP, reminders), the stream directory and the activity feed (the
//! home page), our permissions there, its configuration (gateway
//! administration). The pins, reactions and topics are in `chat.rs`.

use slint::{ComponentHandle, SharedString};
use voelin_core::{Command, GatewayRequest, GatewayUpdate};
use voelin_gateway_proto::{Action, EventKind, EventQuery, EventSpec, RsvpStatus, feature};

use crate::app::{App, Bridge, EventForm, EventItem};
use crate::social::{NoticeKind, NoticeTarget, now_ms};
use crate::vm;
use crate::vm::social::{date_block, event_when, format_local, parse_local, soon};

impl App {
	/// A request to a session's gateway (not for the sample sessions).
	pub(crate) fn gateway_to(&self, session: i64, request: GatewayRequest) {
		if !self.demo_ui {
			self.engine.send(Command::Gateway { session: session as u64, request });
		}
	}

	/// Keep what a gateway update tells these screens (before the chat
	/// handles its part).
	pub(crate) fn gateway_extra(&mut self, id: i64, update: &GatewayUpdate) {
		use GatewayUpdate as U;
		let mut ask: Vec<GatewayRequest> = Vec::new();
		let mut remind = None;
		let extra = &mut self.sessions.entry(id).or_default().extra;
		match update {
			U::Connected { uid, capabilities, .. } => {
				extra.gateway_uid = Some(uid.clone());
				let has = |f: &str| capabilities.iter().any(|c| c == f);
				ask.push(GatewayRequest::Permissions { channel: None });
				if has(feature::EVENTS) {
					ask.push(GatewayRequest::Events { query: EventQuery::default() });
				}
				if has(feature::STREAMS) {
					ask.push(GatewayRequest::Streams);
				}
				if has(feature::ACTIVITY) {
					ask.push(GatewayRequest::Activity { before: None, limit: Some(30) });
				}
			}
			U::Disconnected { .. } => {
				extra.directory.clear();
				extra.events.clear();
				extra.events_loaded = false;
				extra.activity.clear();
				extra.actions.clear();
				extra.config.clear();
				extra.perms.clear();
			}
			U::Permissions { channel: None, actions } => extra.actions = actions.clone(),
			U::Events { events } => {
				extra.events = events.clone();
				extra.events.sort_by_key(|e| e.spec.start_ms);
				extra.events_loaded = true;
			}
			U::Event { event } => {
				match extra.events.iter().position(|e| e.id == event.id) {
					Some(i) => extra.events[i] = event.clone(),
					None => extra.events.push(event.clone()),
				}
				extra.events.sort_by_key(|e| e.spec.start_ms);
			}
			U::EventDeleted { id: event } => extra.events.retain(|e| e.id != *event),
			U::EventReminder { event, starts_in_ms } => {
				remind = Some((event.clone(), *starts_in_ms))
			}
			U::Streams { streams } => extra.directory = streams.clone(),
			U::StreamStarted { stream }
			| U::StreamUpdated { stream }
			| U::StreamRegistered { stream } => {
				extra.directory.retain(|e| e.id != stream.id);
				extra.directory.push(stream.clone());
			}
			U::StreamEnded { id: stream, .. } => extra.directory.retain(|e| e.id != *stream),
			U::Activity { entries, .. } => extra.activity = entries.clone(),
			U::ActivityAdded { entry } => {
				extra.activity.insert(0, entry.clone());
				extra.activity.truncate(100);
			}
			U::Config { entries } => extra.config = entries.clone(),
			U::ConfigValue { entry } => {
				match extra.config.iter().position(|e| e.key == entry.key) {
					Some(i) => extra.config[i] = entry.clone(),
					None => extra.config.push(entry.clone()),
				}
			}
			U::PermRules { rules } => extra.perms = rules.clone(),
			_ => return,
		}
		for request in ask {
			self.gateway_to(id, request);
		}
		if let Some((event, starts_in_ms)) = remind {
			let minutes = starts_in_ms / 60_000;
			let title = if minutes <= 0 {
				format!("{} starts now", event.spec.title)
			} else {
				format!("{} starts in {minutes} min", event.spec.title)
			};
			let body = format!("on {}", self.server_name(id));
			self.notify(
				NoticeKind::Event,
				title,
				body,
				NoticeTarget::Event(id, event.id),
				event.creator.name.clone(),
				Some(event.creator.uid.clone()),
			);
		}
		self.refresh_home();
		self.refresh_events();
		self.refresh_gateways();
	}

	/// The events page of the current server was opened.
	pub(crate) fn open_events(&mut self) {
		let Some(id) = self.current else { return };
		let loaded = self.sessions.get(&id).is_some_and(|v| v.extra.events_loaded);
		if !loaded {
			self.gateway_to(id, GatewayRequest::Events { query: EventQuery::default() });
		}
		self.refresh_events();
	}

	/// Channels an event can be in: server-wide first.
	fn event_channels(&self) -> Vec<(Option<u64>, String)> {
		let mut out = vec![(None, "Server-wide".to_owned())];
		if let Some(view) = self.view() {
			out.extend(
				view.presence.channels.values().map(|c| (Some(c.id), format!("#{}", c.name))),
			);
		}
		out
	}

	pub(crate) fn refresh_events(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let view = self.view();
		let has = view.is_some_and(|v| v.gateway_has(feature::EVENTS));
		bridge.set_has_events(has);
		let extra = view.map(|v| &v.extra);
		let moderate = extra.is_some_and(|e| e.actions.contains(&Action::Moderate));
		bridge.set_can_create_events(
			has && extra
				.is_some_and(|e| e.actions.is_empty() || e.actions.contains(&Action::CreateEvent)),
		);
		bridge.set_events_loading(has && !extra.is_some_and(|e| e.events_loaded));
		let channels = self.event_channels();
		bridge.set_event_channels(crate::app::model(
			channels.iter().map(|(_, n)| SharedString::from(n.as_str())).collect(),
		));
		let now = now_ms();
		let items: Vec<EventItem> = extra
			.map(|extra| {
				extra
					.events
					.iter()
					.filter(|e| e.spec.end_ms.unwrap_or(e.spec.start_ms + 3 * 3_600_000) >= now)
					.map(|e| {
						let (weekday, day, month) = date_block(e.spec.start_ms);
						let place = channels
							.iter()
							.find(|(c, _)| *c == e.spec.channel)
							.map_or_else(|| "A channel".to_owned(), |(_, n)| n.clone());
						let stream = if e.spec.kind == EventKind::Stream {
							[e.spec.stream_title.clone(), e.spec.stream_game.clone()]
								.into_iter()
								.flatten()
								.filter(|s| !s.is_empty())
								.collect::<Vec<_>>()
								.join(" · ")
						} else {
							String::new()
						};
						let stream = if e.spec.kind == EventKind::Stream && stream.is_empty() {
							"Scheduled stream".to_owned()
						} else {
							stream
						};
						EventItem {
							id: e.id as i32,
							title: e.spec.title.clone().into(),
							description: e.spec.description.clone().into(),
							weekday: weekday.into(),
							day: day.into(),
							month: month.into(),
							when: event_when(e.spec.start_ms, e.spec.end_ms).into(),
							place: place.into(),
							stream: stream.into(),
							live: e.live_stream.is_some(),
							going: e.going as i32,
							maybe: e.maybe as i32,
							not_going: e.not_going as i32,
							rsvp: e.my_rsvp.map(RsvpStatus::as_str).unwrap_or_default().into(),
							creator: e.creator.name.clone().into(),
							editable: moderate
								|| extra.gateway_uid.as_deref() == Some(e.creator.uid.as_str()),
							soon: soon(e.spec.start_ms, e.spec.end_ms, now).into(),
							attendees: crate::app::model(
								e.attendees
									.iter()
									.map(|a| {
										let status = match a.status {
											RsvpStatus::Going => "going",
											RsvpStatus::Maybe => "maybe",
											RsvpStatus::NotGoing => "not going",
											RsvpStatus::Unknown => "?",
										};
										SharedString::from(format!("{} ({status})", a.user.name))
									})
									.collect(),
							),
							expanded: extra.expanded.contains(&e.id),
						}
					})
					.collect()
			})
			.unwrap_or_default();
		vm::list::sync(&self.models.social.events, &items);
	}

	/// Answer an event ("" withdraws the answer).
	pub(crate) fn rsvp(&mut self, event_id: i32, status: &str) {
		let Some(id) = self.current else { return };
		let status = (!status.is_empty()).then(|| RsvpStatus::parse(status));
		self.gateway_to(id, GatewayRequest::Rsvp { event_id: i64::from(event_id), status });
	}

	/// Show or hide who answered (fetching the event with its attendees).
	pub(crate) fn toggle_event(&mut self, event_id: i32) {
		let Some(id) = self.current else { return };
		let event_id = i64::from(event_id);
		let view = self.sessions.entry(id).or_default();
		if !view.extra.expanded.remove(&event_id) {
			view.extra.expanded.insert(event_id);
			self.gateway_to(id, GatewayRequest::GetEvent { id: event_id });
		}
		self.refresh_events();
	}

	/// The form for a new event (`-1`) or an existing one.
	pub(crate) fn edit_event(&self, event_id: i32) -> EventForm {
		let channels = self.event_channels();
		let event =
			self.view().and_then(|v| v.extra.events.iter().find(|e| e.id == i64::from(event_id)));
		match event {
			Some(e) => EventForm {
				id: event_id,
				title: e.spec.title.clone().into(),
				description: e.spec.description.clone().into(),
				start: format_local(e.spec.start_ms).into(),
				end: e.spec.end_ms.map(format_local).unwrap_or_default().into(),
				channel: channels.iter().position(|(c, _)| *c == e.spec.channel).unwrap_or(0)
					as i32,
				stream: e.spec.kind == EventKind::Stream,
				stream_title: e.spec.stream_title.clone().unwrap_or_default().into(),
				game: e.spec.stream_game.clone().unwrap_or_default().into(),
			},
			None => {
				// The next full hour.
				let start = (now_ms() / 3_600_000 + 1) * 3_600_000;
				EventForm { id: -1, start: format_local(start).into(), ..Default::default() }
			}
		}
	}

	/// Create or change an event; the reason when the form is not right.
	pub(crate) fn save_event(&mut self, form: &EventForm) -> String {
		let Some(id) = self.current else { return "No server selected.".into() };
		let Some(start_ms) = parse_local(&form.start) else {
			return "Write the start as YYYY-MM-DD HH:MM.".into();
		};
		let end_ms = if form.end.trim().is_empty() {
			None
		} else {
			match parse_local(&form.end) {
				Some(end) if end > start_ms => Some(end),
				Some(_) => return "The end must be after the start.".into(),
				None => return "Write the end as YYYY-MM-DD HH:MM, or leave it empty.".into(),
			}
		};
		let channels = self.event_channels();
		let text = |s: &slint::SharedString| Some(s.to_string()).filter(|s| !s.trim().is_empty());
		let spec = EventSpec {
			title: form.title.trim().to_owned(),
			description: form.description.to_string(),
			start_ms,
			end_ms,
			channel: channels.get(form.channel.max(0) as usize).and_then(|(c, _)| *c),
			kind: if form.stream { EventKind::Stream } else { EventKind::General },
			stream_title: text(&form.stream_title).filter(|_| form.stream),
			stream_game: text(&form.game).filter(|_| form.stream),
			host_uid: None,
		};
		self.gateway_to(
			id,
			if form.id < 0 {
				GatewayRequest::CreateEvent { event: spec }
			} else {
				GatewayRequest::UpdateEvent { id: i64::from(form.id), event: spec }
			},
		);
		String::new()
	}

	pub(crate) fn delete_event(&mut self, event_id: i32) {
		if let Some(id) = self.current {
			self.gateway_to(id, GatewayRequest::DeleteEvent { id: i64::from(event_id) });
		}
	}

	/// Watch the stream of a live scheduled stream.
	pub(crate) fn watch_event(&mut self, event_id: i32) {
		let Some(id) = self.current else { return };
		let stream = self
			.view()
			.and_then(|v| v.extra.events.iter().find(|e| e.id == i64::from(event_id)))
			.and_then(|e| e.live_stream.clone());
		if let Some(stream) = stream {
			self.watch_live(id, &stream);
		}
	}
}
