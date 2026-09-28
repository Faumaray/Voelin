//! Chat: tabs per session (server, channels, private chats) and their
//! messages. Each tab keeps its lines as a model; the chat view shows the
//! selected tab's model, so a new message appends one row.

use slint::{ComponentHandle, ModelRc};
use voelin_core::Command;
use voelin_model::{ChatMessage, ChatTarget};

use crate::app::{App, Bridge, ChatTab, Tab};
use crate::vm;

impl App {
	pub(crate) fn open_chat(&mut self, target: ChatTarget, focus: bool) {
		let Some(id) = self.current else { return };
		let title = match &target {
			ChatTarget::Channel(cid) => {
				let name = self
					.sessions
					.get(&id)
					.and_then(|v| v.presence.channels.get(cid).map(|c| c.name.clone()));
				format!("#{}", name.unwrap_or_else(|| cid.to_string()))
			}
			ChatTarget::Server => "Server".into(),
			ChatTarget::Private(uid) => format!("@{uid}"),
		};
		let view = self.sessions.entry(id).or_default();
		let index = match view.tabs.iter().position(|t| t.target == target) {
			Some(i) => i,
			None => {
				view.tabs.push(Tab::new(target.clone(), title));
				if !self.demo_ui {
					self.engine.send(Command::OpenChat { session: id as u64, target });
				}
				view.tabs.len() - 1
			}
		};
		if focus {
			view.current_tab = index;
			view.tabs[index].unread = 0;
		}
		self.refresh_chat();
		self.refresh_servers();
	}

	pub(crate) fn select_tab(&mut self, index: usize) {
		if let Some(view) = self.view_mut()
			&& index < view.tabs.len()
		{
			view.current_tab = index;
			view.tabs[index].unread = 0;
		}
		self.refresh_chat();
		self.refresh_servers();
	}

	pub(crate) fn close_tab(&mut self, index: usize) {
		let Some(id) = self.current else { return };
		let view = self.sessions.entry(id).or_default();
		if index == 0 || index >= view.tabs.len() {
			return;
		}
		let tab = view.tabs.remove(index);
		view.current_tab = view.current_tab.min(view.tabs.len() - 1);
		self.engine.send(Command::CloseChat { session: id as u64, target: tab.target });
		self.refresh_chat();
	}

	pub(crate) fn send_message(&mut self, text: String) {
		let Some(id) = self.current else { return };
		let Some(view) = self.sessions.get(&id) else { return };
		let target = view.tabs[view.current_tab].target.clone();
		self.engine.send(Command::SendChat { session: id as u64, target, text });
	}

	pub(crate) fn add_message(&mut self, id: i64, message: ChatMessage) {
		let current = self.current == Some(id);
		let view = self.sessions.entry(id).or_default();
		let index = match view.tabs.iter().position(|t| t.target == message.target) {
			Some(i) => i,
			None => {
				let title = match &message.target {
					ChatTarget::Channel(cid) => format!(
						"#{}",
						view.presence
							.channels
							.get(cid)
							.map(|c| c.name.clone())
							.unwrap_or_else(|| cid.to_string())
					),
					ChatTarget::Private(_) => format!("@{}", message.author_name),
					ChatTarget::Server => "Server".into(),
				};
				view.tabs.push(Tab::new(message.target.clone(), title));
				view.tabs.len() - 1
			}
		};
		let shown = current && view.current_tab == index;
		let tab = &mut view.tabs[index];
		let line = vm::chat::line(&message, tab.last.as_ref());
		tab.last = Some(vm::chat::Previous::of(&message));
		tab.lines.push(line);
		if !shown {
			tab.unread += 1;
		}
		if current {
			self.refresh_chat();
		}
		if !shown {
			self.refresh_servers();
		}
	}

	/// Tabs and the shown tab's messages.
	pub(crate) fn refresh_chat(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let (tabs, lines) = match self.view() {
			None => (
				vec![ChatTab { title: "Server".into(), ..Default::default() }],
				ModelRc::from(self.models.no_chat.clone()),
			),
			Some(view) => {
				let tabs: Vec<ChatTab> = view
					.tabs
					.iter()
					.map(|t| ChatTab {
						title: t.title.clone().into(),
						name: t.title.trim_start_matches(['#', '@']).into(),
						kind: match t.target {
							ChatTarget::Server => 0,
							ChatTarget::Channel(_) => 1,
							ChatTarget::Private(_) => 2,
						},
						id: match t.target {
							ChatTarget::Channel(cid) => cid as i32,
							_ => 0,
						},
						unread: t.unread,
						topic: match t.target {
							ChatTarget::Channel(cid) => view
								.presence
								.channels
								.get(&cid)
								.and_then(|c| c.topic.clone())
								.unwrap_or_default()
								.into(),
							_ => Default::default(),
						},
					})
					.collect();
				(tabs, ModelRc::from(view.tabs[view.current_tab].lines.clone()))
			}
		};
		vm::list::sync(&self.models.tabs, &tabs);
		let current = self.view().map_or(0, |v| v.current_tab as i32);
		if bridge.get_current_tab() != current {
			bridge.set_current_tab(current);
		}
		// Swap the model only when another tab is shown.
		if bridge.get_messages() != lines {
			bridge.set_messages(lines);
		}
	}
}
