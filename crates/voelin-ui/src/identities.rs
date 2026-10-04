//! Settings → Identities: the stored identities, which one servers see us
//! as (the default), and the import of the official clients' identities on
//! start (`voelin_core::identity::import_new`, app.rs).

use slint::ComponentHandle;
use tracing::warn;
use voelin_core::settings::IDENTITY_IMPORT;
use voelin_store::{IdentityEntry, IdentityOrigin};

use crate::app::{App, Bridge, IdentityItem, model};
use crate::vm::avatar;

/// A row of the identities card.
fn item(entry: &IdentityEntry) -> IdentityItem {
	let origin = match entry.origin {
		IdentityOrigin::Imported => "From TeamSpeak",
		IdentityOrigin::Created | IdentityOrigin::User => "Made in Voelin",
	};
	IdentityItem {
		id: entry.id as i32,
		name: entry.name.clone().into(),
		detail: format!("{origin} · level {}", entry.level).into(),
		uid: entry.uid.clone().into(),
		initials: avatar::initials(&entry.name).into(),
		tint: avatar::tint(&entry.name),
		is_default: entry.is_default,
	}
}

impl App {
	pub(crate) fn refresh_identities(&self) {
		let Some(ui) = self.ui.upgrade() else { return };
		let bridge = ui.global::<Bridge>();
		let mut entries = self.store.identities().unwrap_or_default();
		// A store from before defaults were marked: the first one.
		let default = self.store.default_identity().ok().flatten().map(|e| e.id);
		for entry in &mut entries {
			entry.is_default = Some(entry.id) == default;
		}
		bridge.set_identities(model(entries.iter().map(item).collect()));
		bridge.set_identity_import(self.prefs.get(&IDENTITY_IMPORT));
	}

	/// The user's choice of the identity servers see us as, from the next
	/// connection on; no import replaces it.
	pub(crate) fn use_identity(&mut self, id: i64) {
		let result =
			self.store.set_default_identity(id, true).and_then(|()| self.store.identity(id));
		match result {
			Ok(identity) => {
				self.identity = identity;
				let name = self
					.store
					.identities()
					.ok()
					.and_then(|all| all.into_iter().find(|e| e.id == id))
					.map(|e| e.name)
					.unwrap_or_default();
				self.set_status(format!(
					"Servers see you as \u{201c}{name}\u{201d} from your next connection"
				));
			}
			Err(e) => self.set_status(format!("Could not change the identity: {e}")),
		}
		self.refresh_identities();
	}

	pub(crate) fn set_identity_import(&mut self, on: bool) {
		if let Err(e) = self.prefs.set(&IDENTITY_IMPORT, on) {
			warn!(%e, "could not store the setting");
		}
		self.refresh_identities();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rows_say_where_an_identity_came_from() {
		let entry = |origin, is_default| IdentityEntry {
			id: 3,
			name: "Main Nick".into(),
			uid: "uid=".into(),
			level: 8,
			origin,
			is_default,
		};
		let row = item(&entry(IdentityOrigin::Imported, true));
		assert_eq!((row.id, row.name.as_str(), row.uid.as_str()), (3, "Main Nick", "uid="));
		assert_eq!(row.detail, "From TeamSpeak · level 8");
		assert!(row.is_default);
		assert_eq!(row.initials, avatar::initials("Main Nick"));
		for origin in [IdentityOrigin::Created, IdentityOrigin::User] {
			let row = item(&entry(origin, false));
			assert_eq!(row.detail, "Made in Voelin · level 8");
			assert!(!row.is_default);
		}
	}
}
