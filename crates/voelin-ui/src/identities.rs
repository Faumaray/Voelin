//! The identity servers see us as: the store's default (schema 4,
//! `Store::default_identity`), chosen under Settings → Profiles, and the
//! import of the official clients' identities on start
//! (`voelin_core::identity::import_new`, app.rs). The rest of the Profiles
//! page is in `settings_pages.rs`.

use tracing::warn;
use voelin_core::settings::IDENTITY_IMPORT;
use voelin_store::IdentityOrigin;

use crate::app::App;

/// Where an identity came from, as Profiles says it.
pub(crate) fn origin_label(origin: IdentityOrigin) -> &'static str {
	match origin {
		IdentityOrigin::Created => "Made in Voelin",
		IdentityOrigin::Imported => "Imported",
		IdentityOrigin::User => "Chosen by you",
	}
}

impl App {
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
		assert_eq!(origin_label(IdentityOrigin::Created), "Made in Voelin");
		assert_eq!(origin_label(IdentityOrigin::Imported), "Imported");
		// Made by the app, then chosen: an import never replaces it.
		assert_eq!(origin_label(IdentityOrigin::User), "Chosen by you");
	}
}
