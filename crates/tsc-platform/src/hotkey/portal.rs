//! Wayland backend: xdg-desktop-portal `GlobalShortcuts`.
//!
//! The portal binds all shortcuts of a session at once, so every change
//! closes the session and binds the full list in a new one. Shortcuts the
//! user already confirmed are remembered by id and do not ask again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use ashpd::desktop::global_shortcuts::{
	Activated, BindShortcutsOptions, Deactivated, GlobalShortcuts, NewShortcut,
};
use ashpd::desktop::{CreateSessionOptions, Session};
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;
use tracing::debug;

use super::{Hotkey, HotkeyEvent, HotkeyEvents};
use crate::{APP_ID, Error, Result};

type Senders = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<HotkeyEvent>>>>;

fn portal_error(e: ashpd::Error) -> Error {
	Error::Backend(format!("portal: {e}"))
}

struct Shortcut {
	id: String,
	description: String,
	hotkey: Hotkey,
}

pub(crate) struct PortalBackend {
	portal: GlobalShortcuts,
	session: Option<Session<GlobalShortcuts>>,
	shortcuts: Vec<Shortcut>,
	senders: Senders,
	triggers: HashMap<String, String>,
	listener: tokio::task::JoinHandle<()>,
}

impl PortalBackend {
	pub async fn new() -> Result<Self> {
		// Outside a sandbox the portal learns our app id this way (newer
		// portals need it for global shortcuts); older ones lack the call.
		if let Ok(app_id) = APP_ID.parse()
			&& let Err(e) = ashpd::register_host_app(app_id).await
		{
			debug!(%e, "portal host app registration");
		}
		let portal = GlobalShortcuts::new().await.map_err(portal_error)?;
		let activated = portal.receive_activated().await.map_err(portal_error)?;
		let deactivated = portal.receive_deactivated().await.map_err(portal_error)?;
		let senders = Senders::default();
		let listener = tokio::spawn(listen(activated, deactivated, senders.clone()));
		Ok(Self {
			portal,
			session: None,
			shortcuts: Vec::new(),
			senders,
			triggers: HashMap::new(),
			listener,
		})
	}

	fn senders(
		&self,
	) -> std::sync::MutexGuard<'_, HashMap<String, mpsc::UnboundedSender<HotkeyEvent>>> {
		self.senders.lock().unwrap_or_else(PoisonError::into_inner)
	}

	pub async fn register(
		&mut self,
		id: &str,
		description: &str,
		hotkey: Hotkey,
	) -> Result<HotkeyEvents> {
		if hotkey.key.is_mouse() {
			return Err(Error::Unsupported("the portal does not bind mouse buttons".into()));
		}
		self.shortcuts.retain(|s| s.id != id);
		self.shortcuts.push(Shortcut { id: id.into(), description: description.into(), hotkey });
		if let Err(e) = self.rebind().await {
			self.shortcuts.retain(|s| s.id != id);
			return Err(e);
		}
		let (tx, rx) = mpsc::unbounded_channel();
		self.senders().insert(id.into(), tx);
		Ok(rx)
	}

	pub async fn unregister(&mut self, id: &str) -> Result<()> {
		self.senders().remove(id);
		self.shortcuts.retain(|s| s.id != id);
		self.rebind().await
	}

	pub fn trigger_description(&self, id: &str) -> Option<String> {
		self.triggers.get(id).cloned()
	}

	async fn rebind(&mut self) -> Result<()> {
		if let Some(old) = self.session.take() {
			let _ = old.close().await;
		}
		self.triggers.clear();
		if self.shortcuts.is_empty() {
			return Ok(());
		}
		let session = self
			.portal
			.create_session(CreateSessionOptions::default())
			.await
			.map_err(portal_error)?;
		let new: Vec<NewShortcut> = self
			.shortcuts
			.iter()
			.map(|s| {
				let trigger = s.hotkey.xdg_trigger();
				NewShortcut::new(&s.id, &s.description).preferred_trigger(trigger.as_deref())
			})
			.collect();
		let request = self
			.portal
			.bind_shortcuts(&session, &new, None, BindShortcutsOptions::default())
			.await
			.map_err(portal_error)?;
		let bound = request.response().map_err(portal_error)?;
		self.triggers = bound
			.shortcuts()
			.iter()
			.map(|s| (s.id().to_string(), s.trigger_description().to_string()))
			.collect();
		self.session = Some(session);
		Ok(())
	}
}

impl Drop for PortalBackend {
	fn drop(&mut self) {
		// Closing the session needs an await; the portal closes it when our
		// D-Bus connection goes away.
		self.listener.abort();
	}
}

/// Forward `Activated` / `Deactivated` to the registration with that id.
async fn listen(
	activated: impl Stream<Item = Activated>,
	deactivated: impl Stream<Item = Deactivated>,
	senders: Senders,
) {
	let activated = activated.map(|a| (a.shortcut_id().to_string(), HotkeyEvent::Pressed));
	let deactivated = deactivated.map(|d| (d.shortcut_id().to_string(), HotkeyEvent::Released));
	let mut events = std::pin::pin!(futures_util::stream::select(activated, deactivated));
	while let Some((id, event)) = events.next().await {
		let senders = senders.lock().unwrap_or_else(PoisonError::into_inner);
		if let Some(tx) = senders.get(&id) {
			let _ = tx.send(event);
		}
	}
}
