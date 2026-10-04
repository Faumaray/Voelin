//! Desktop account state. Only the UI thread mutates state or persists credentials.
use serde::{Deserialize, Serialize};
use slint::ComponentHandle;
use std::sync::Arc;
use voelin_myts::{Client, Login, ServerIdentity, SessionToken};
use voelin_store::Secrets;

use crate::app::{App, Bridge, MytsForm, later};

// One keyring item makes session replacement atomic: a failed write cannot
// combine a new session with another account's renewal material.
const KEY: &str = "myts/session";

#[derive(Default, Serialize, Deserialize)]
struct Saved {
	session: String,
	email: String,
	username: String,
	#[serde(default)]
	uuid: String,
	renewal_token: String,
	device_id: String,
	#[serde(default)]
	otp_token: String,
	// Private account identity belongs in the same secret-store transaction.
	#[serde(default)]
	identity: Option<ServerIdentity>,
}

#[derive(Default)]
pub(crate) struct Account {
	saved: Option<Saved>,
	signed_in: bool,
	busy: bool,
	otp_required: bool,
	status: i32,
	persistence_status: i32,
	generation: u64,
	started: bool,
	prompt: bool,
	// Also set after a failed read/delete so the user can retry clearing.
	can_forget: bool,
	identity_failed: bool,
}

impl Account {
	fn load(&mut self, secrets: &dyn Secrets) {
		match read_saved(secrets) {
			Ok(saved) => {
				self.saved = saved;
				self.can_forget = false;
				self.status = 0;
				self.persistence_status = 0;
				self.identity_failed = false;
			}
			Err(()) => {
				self.can_forget = true;
				self.persistence_status = 1;
			}
		}
	}

	fn restored(&mut self) {
		self.signed_in = true;
		self.prompt = false;
		self.otp_required = false;
	}

	fn device_for(&self, email: &str) -> (String, String) {
		self.saved
			.as_ref()
			.filter(|s| s.email.eq_ignore_ascii_case(email.trim()))
			.map(|s| (s.device_id.clone(), s.otp_token.clone()))
			.unwrap_or_default()
	}

	fn accept(&mut self, secrets: &dyn Secrets, login: Login, email: String) {
		self.identity_failed = login.identity.is_err();
		let (renewal_token, device_id, otp_token) = login
			.renewal
			.map(|renewal| (renewal.token, renewal.device_id, renewal.otp_token))
			.unwrap_or_default();
		let saved = Saved {
			session: login.token.into_inner(),
			email,
			username: login.username,
			uuid: login.uuid,
			renewal_token,
			device_id,
			otp_token,
			identity: login.identity.ok(),
		};
		self.persistence_status = if save(secrets, &saved) {
			0
		} else {
			// Remove an older credential after a replacement failed. Do not
			// silently sign back into the previous account on next startup.
			let cleared = secrets.delete(KEY).is_ok();
			if cleared { 5 } else { 6 }
		};
		self.status = 0;
		self.saved = Some(saved);
		self.restored();
	}

	fn revocation_failed(&mut self, generation: u64) {
		if self.generation == generation && !self.signed_in && !self.busy && !self.can_forget {
			self.status = 9;
		}
	}
	fn form(&self) -> MytsForm {
		let identity = self.identity();
		MytsForm {
			available: true,
			prompt: self.prompt,
			signed_in: self.signed_in,
			email: self.saved.as_ref().map(|s| s.email.as_str()).unwrap_or_default().into(),
			username: self.saved.as_ref().map(|s| s.username.as_str()).unwrap_or_default().into(),
			uuid: self.saved.as_ref().map(|s| s.uuid.as_str()).unwrap_or_default().into(),
			myts_id: identity.map(ServerIdentity::id).unwrap_or_default().into(),
			identity_status: if !self.signed_in {
				0
			} else if identity.is_some() {
				1
			} else if self.identity_failed {
				3
			} else {
				2
			},
			status: self.status,
			persistence_status: self.persistence_status,
			busy: self.busy,
			otp_required: self.otp_required,
			can_forget: self.can_forget || self.saved.is_some(),
		}
	}
	fn identity(&self) -> Option<&ServerIdentity> {
		self.saved.as_ref().filter(|_| self.signed_in).and_then(|s| s.identity.as_ref())
	}
	fn begin(&mut self) -> Option<u64> {
		if self.busy {
			return None;
		}
		self.generation += 1;
		self.busy = true;
		self.status = 0;
		Some(self.generation)
	}

	fn finish(&mut self, generation: u64) -> bool {
		if self.generation != generation {
			return false;
		}
		self.busy = false;
		true
	}

	fn forget(&mut self, secrets: &dyn Secrets) -> bool {
		self.generation += 1;
		self.busy = false;
		self.signed_in = false;
		self.prompt = true;
		self.saved = None;
		self.identity_failed = false;
		self.otp_required = false;
		let cleared = secrets.delete(KEY).is_ok();
		self.persistence_status = if cleared { 0 } else { 8 };
		self.can_forget = !cleared;
		cleared
	}
}

fn login_error_status(error: &voelin_myts::Error) -> i32 {
	match error.status_code() {
		Some(208) => 3,
		Some(209) => 14,
		Some(203) => 13,
		Some(202) => 16,
		Some(103 | 206 | 207) => 15,
		_ if matches!(error, voelin_myts::Error::Http(429)) => 17,
		_ => 4,
	}
}

// Fixed official routes only. Neither the session nor an email goes in a URL.
fn portal_url(action: i32) -> Option<&'static str> {
	match action {
		0 => Some("https://www.myteamspeak.com/register"),
		1 => Some("https://www.myteamspeak.com/forgot-password"),
		2 => Some("https://www.myteamspeak.com/my-account"),
		3 => Some("https://www.myteamspeak.com/resend-activation"),
		4 => Some("https://www.myteamspeak.com/my-account/change-username"),
		5 => Some("https://www.myteamspeak.com/my-account/change-email"),
		6 => Some("https://www.myteamspeak.com/my-account/change-password"),
		7 => Some("https://www.myteamspeak.com/my-account/change-2fa"),
		8 => Some("https://www.myteamspeak.com/my-account/manage-account"),
		9 => Some("https://www.myteamspeak.com/my-account/manage-data"),
		_ => None,
	}
}

fn read_saved(secrets: &dyn Secrets) -> Result<Option<Saved>, ()> {
	secrets
		.get(KEY)
		.map_err(|_| ())?
		.map(|value| {
			let saved: Saved = serde_json::from_str(&value).map_err(|_| ())?;
			SessionToken::new(saved.session.clone()).map_err(|_| ())?;
			Ok(saved)
		})
		.transpose()
}

fn save(secrets: &dyn Secrets, saved: &Saved) -> bool {
	serde_json::to_string(saved).ok().is_some_and(|value| secrets.set(KEY, &value).is_ok())
}

impl App {
	fn publish_myts_identity(&self) {
		self.engine.send(voelin_core::Command::SetMytsIdentity(
			self.myts.identity().cloned().map(Arc::new),
		));
	}

	pub(crate) fn refresh_myts(&self) {
		let Some(ui) = self.ui.upgrade() else {
			return;
		};
		ui.global::<Bridge>().set_myts(self.myts.form());
	}

	pub(crate) fn start_myts(&mut self) {
		if self.myts.started {
			return;
		}
		self.myts.started = true;
		if self.demo_ui {
			self.refresh_myts();
			return;
		}
		self.myts.prompt = true;
		self.myts_retry();
	}

	pub(crate) fn myts_dismiss(&mut self) {
		self.myts.prompt = false;
		self.refresh_myts();
	}

	pub(crate) fn myts_portal(&mut self, action: i32) {
		let Some(url) = portal_url(action).filter(|_| !self.myts.busy) else {
			return;
		};
		let generation = self.myts.generation;
		self.runtime.spawn(async move {
			let opened = tokio::task::spawn_blocking(move || {
				crate::app::desktop_opener(std::ffi::OsStr::new(url))
					.status()
					.is_ok_and(|status| status.success())
			})
			.await
			.unwrap_or(false);
			later(move |app| {
				if app.myts.generation == generation && !app.myts.busy {
					app.myts.status = if opened { 11 } else { 10 };
					app.refresh_myts();
				}
			});
		});
	}

	pub(crate) fn myts_retry(&mut self) {
		if self.demo_ui || self.myts.busy {
			return;
		}
		if self.myts.saved.is_none() {
			self.myts.load(self.secrets.as_ref());
		}
		let Some(saved) = self.myts.saved.as_ref() else {
			self.refresh_myts();
			return;
		};
		let token = SessionToken::new(saved.session.clone()).expect("validated stored token");
		let generation = self.myts.begin().expect("startup is idle");
		self.refresh_myts();
		self.runtime.spawn(async move {
			// Password login does not provide the separate auth token required
			// by loginWithRenewalToken. Expiry requires a fresh password login.
			let result = async { Client::new()?.validate_session(&token).await }.await;
			later(move |app| {
				if !app.myts.finish(generation) {
					return;
				}
				let was_signed_in = app.myts.signed_in;
				match result {
					Ok(()) => app.myts.restored(),
					Err(error) => {
						app.myts.otp_required =
							error.is_otp_required() || error.status_code() == Some(209);
						app.myts.status = 2;
						if error.is_invalid_session() {
							app.myts.signed_in = false;
							app.myts.status = 15;
						}
					}
				}
				if was_signed_in != app.myts.signed_in {
					app.publish_myts_identity();
				}
				app.refresh_myts();
			});
		});
	}

	pub(crate) fn myts_login(&mut self, email: String, password: String, otp: String) {
		if email.trim().is_empty() || password.is_empty() || self.myts.signed_in {
			return;
		}
		let (device, otp_renewal) = self.myts.device_for(&email);
		let Some(generation) = self.myts.begin() else {
			return;
		};
		self.refresh_myts();
		self.runtime.spawn(async move {
			let result = async {
				Client::new()?
					.login_with_otp_renewal(
						email.trim(),
						&password,
						(!otp.is_empty()).then_some(otp.as_str()),
						&device,
						&otp_renewal,
					)
					.await
			}
			.await;
			later(move |app| {
				if !app.myts.finish(generation) {
					app.discard_myts_login(result.ok());
					return;
				}
				match result {
					Ok(login) => {
						app.myts.accept(app.secrets.as_ref(), login, email.trim().to_owned());
						app.publish_myts_identity();
					}
					Err(error) => {
						app.myts.otp_required =
							error.is_otp_required() || error.status_code() == Some(209);
						app.myts.status = login_error_status(&error);
					}
				}
				app.refresh_myts();
			});
		});
	}

	// A canceled login can still have created a remote session. Revoke it,
	// never publish or save it after a sign-out/replacement.
	fn discard_myts_login(&self, login: Option<Login>) {
		if let Some(login) = login {
			let generation =
				(!self.myts.busy && !self.myts.signed_in).then_some(self.myts.generation);
			self.runtime.spawn(async move {
				let result = async { Client::new()?.logout(&login.token).await }.await;
				if result.is_err() {
					// Static diagnostic: no credential, account, response body or URL.
					tracing::warn!("canceled account sign-in session could not be revoked");
					later(move |app| {
						if let Some(generation) = generation {
							app.myts.revocation_failed(generation);
						}
						app.refresh_myts();
					});
				}
			});
		}
	}

	pub(crate) fn myts_logout(&mut self) {
		let token =
			self.myts.saved.as_ref().and_then(|s| SessionToken::new(s.session.clone()).ok());
		let cleared = self.myts.forget(self.secrets.as_ref());
		self.publish_myts_identity();
		self.myts.status = if cleared { 7 } else { 0 };
		if let Some(token) = token {
			let generation = self.myts.generation;
			self.myts.busy = true;
			self.runtime.spawn(async move {
				let result = async { Client::new()?.logout(&token).await }.await;
				later(move |app| {
					if !app.myts.finish(generation) {
						return;
					}
					if result.is_err() && cleared {
						app.myts.status = 9;
					}
					app.refresh_myts();
				});
			});
		}
		self.refresh_myts();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use voelin_store::MemorySecrets;

	fn login(name: &str) -> Login {
		Login {
			token: SessionToken::new(format!("{name}-session")).unwrap(),
			uuid: format!("{name}-uuid"),
			username: name.into(),
			identity: Ok(server_identity()),
			renewal: Some(voelin_myts::Renewal {
				token: format!("{name}-renewal"),
				otp_token: format!("{name}-otp"),
				device_id: format!("{name}-device"),
			}),
		}
	}

	fn server_identity() -> ServerIdentity {
		let mut private = [0; 32];
		private[0] = 1;
		let mut public = [0x66; 32];
		public[0] = 0x58; // Compressed Ed25519 basepoint for scalar one.
		ServerIdentity::new(vec![1; 33], 1, public, private, vec![2; 65]).unwrap()
	}

	#[test]
	fn primary_account_persists_restores_after_validation_and_signout_returns_to_login() {
		let secrets = MemorySecrets::default();
		let mut account = Account { prompt: true, ..Default::default() };
		account.load(&secrets);
		assert!(account.form().prompt);
		assert!(!account.form().signed_in);
		account.accept(&secrets, login("Alice"), "alice@example.test".into());
		assert!(!account.form().prompt);
		assert!(account.form().signed_in);
		assert_eq!(account.form().identity_status, 1);
		assert!(account.identity().is_some());
		assert_eq!(account.form().uuid, "Alice-uuid");
		let mut restarted = Account { prompt: true, ..Default::default() };
		restarted.load(&secrets);
		assert!(restarted.identity().is_none(), "unvalidated cache cannot authenticate servers");
		assert!(!restarted.form().signed_in, "cached profile is not validated yet");
		let generation = restarted.begin().unwrap();
		assert!(restarted.finish(generation));
		restarted.restored();
		assert!(restarted.identity().is_some());
		assert!(!restarted.form().prompt);
		assert_eq!(restarted.form().username, "Alice");
		assert!(restarted.forget(&secrets));
		assert!(restarted.form().prompt);
		assert_eq!(restarted.form().username, "");
		assert_eq!(restarted.form().uuid, "");
		assert!(!restarted.form().signed_in);
		assert!(restarted.identity().is_none());
		assert_eq!(restarted.form().myts_id, "");
	}

	#[test]
	fn switching_primary_account_never_reuses_another_accounts_device_material() {
		let secrets = MemorySecrets::default();
		let mut account = Account::default();
		account.accept(&secrets, login("Alice"), "alice@example.test".into());
		assert_eq!(
			account.device_for(" ALICE@example.test "),
			("Alice-device".into(), "Alice-otp".into())
		);
		assert_eq!(account.device_for("bob@example.test"), Default::default());
		account.forget(&secrets);
		assert_eq!(account.device_for("alice@example.test"), Default::default());
		let mut bob = login("Bob");
		bob.renewal = None;
		account.accept(&secrets, bob, "bob@example.test".into());
		let saved = read_saved(&secrets).unwrap().unwrap();
		assert_eq!(saved.uuid, "Bob-uuid");
		assert_eq!(saved.email, "bob@example.test");
		assert!(saved.renewal_token.is_empty());
		assert!(saved.otp_token.is_empty());
		assert!(saved.device_id.is_empty());
	}

	#[test]
	fn missing_server_key_keeps_login_but_never_reuses_previous_identity() {
		let secrets = MemorySecrets::default();
		let mut account = Account::default();
		account.accept(&secrets, login("Alice"), "alice@example.test".into());
		assert!(account.identity().is_some());
		account.forget(&secrets);
		let mut bob = login("Bob");
		bob.identity = Err(voelin_myts::IdentityError::Missing);
		account.accept(&secrets, bob, "bob@example.test".into());
		assert!(account.signed_in);
		assert!(account.identity().is_none());
		assert_eq!(account.form().identity_status, 3);
		assert!(account.form().myts_id.is_empty());
		assert!(read_saved(&secrets).unwrap().unwrap().identity.is_none());
	}

	#[test]
	fn corrupted_persisted_private_identity_cannot_be_restored() {
		let secrets = MemorySecrets::default();
		let mut account = Account::default();
		account.accept(&secrets, login("Alice"), "alice@example.test".into());
		let mut value: serde_json::Value =
			serde_json::from_str(&secrets.get(KEY).unwrap().unwrap()).unwrap();
		value["identity"]["private_key"] = serde_json::json!(vec![0u8; 32]);
		secrets.set(KEY, &value.to_string()).unwrap();
		assert!(read_saved(&secrets).is_err());
		let mut restored = Account::default();
		restored.load(&secrets);
		assert!(restored.identity().is_none());
		assert_eq!(restored.form().persistence_status, 1);
	}

	#[test]
	fn keyring_retry_and_legacy_bundle_do_not_claim_a_validated_account() {
		let secrets = MemorySecrets::default();
		secrets.set(KEY, r#"{"session":"legacy","email":"a@example.test","username":"A","renewal_token":"r","device_id":"d"}"#).unwrap();
		let mut account = Account { prompt: true, ..Default::default() };
		account.load(&Unavailable);
		assert_eq!(account.form().persistence_status, 1);
		account.load(&secrets);
		assert_eq!(account.status, 0);
		assert_eq!(account.form().persistence_status, 0);
		assert!(!account.can_forget);
		assert!(!account.signed_in);
		assert!(account.prompt);
		assert_eq!(account.form().uuid, "");
		account.restored();
		assert_eq!(account.form().identity_status, 2);
		assert!(account.identity().is_none());
		assert_eq!(account.saved.unwrap().otp_token, "");
	}

	#[test]
	fn login_survives_keyring_failure_with_visible_session_only_status() {
		let mut account = Account { prompt: true, ..Default::default() };
		account.accept(&Unavailable, login("Alice"), "alice@example.test".into());
		assert!(account.signed_in);
		assert!(!account.prompt);
		assert_eq!(account.form().persistence_status, 6);
		assert!(account.form().can_forget);
		assert_eq!(account.form().username, "Alice");
		let generation = account.begin().unwrap();
		assert!(account.finish(generation));
		account.restored();
		assert_eq!(
			account.form().persistence_status,
			6,
			"validation must not hide failed persistence"
		);
		account.status = 11;
		assert_eq!(
			account.form().persistence_status,
			6,
			"opening management must retain the warning"
		);
		account.forget(&Unavailable);
		account.status = 11;
		assert_eq!(
			account.form().persistence_status,
			8,
			"failed deletion must remain visible after browser launch"
		);
		account.forget(&MemorySecrets::default());
		assert_eq!(account.form().persistence_status, 0);
	}

	#[test]
	fn portal_actions_are_fixed_official_https_destinations() {
		for action in 0..10 {
			let url = portal_url(action).unwrap();
			assert!(url.starts_with("https://www.myteamspeak.com/"));
			assert!(!url.contains(['?', '#', '@']));
		}
		assert!(portal_url(-1).is_none());
		assert!(portal_url(10).is_none());
		assert_eq!(portal_url(1), Some("https://www.myteamspeak.com/forgot-password"));
		assert_eq!(login_error_status(&voelin_myts::Error::Http(429)), 17);
		assert_eq!(login_error_status(&voelin_myts::Error::Timeout), 4);
	}

	#[test]
	fn failed_late_session_revocation_reports_signed_out_without_overwriting_new_login() {
		let mut account = Account::default();
		let old = account.begin().unwrap();
		account.forget(&MemorySecrets::default());
		assert!(!account.finish(old));
		let signed_out = account.generation;
		account.revocation_failed(signed_out);
		assert_eq!(account.status, 9);
		let new = account.begin().unwrap();
		account.revocation_failed(signed_out);
		account.revocation_failed(new);
		assert_eq!(account.status, 0);
		assert!(account.busy);
		account.finish(new);
		account.signed_in = true;
		account.revocation_failed(new);
		assert_eq!(account.status, 0);
	}

	#[test]
	fn form_exposes_signed_out_busy_otp_and_signed_in_states_without_tokens() {
		let mut account = Account::default();
		assert!(account.form().available);
		assert!(!account.form().signed_in);
		assert!(!account.form().can_forget);
		let generation = account.begin().unwrap();
		assert!(account.form().busy);
		account.finish(generation);
		account.otp_required = true;
		account.status = 3;
		assert!(account.form().otp_required);
		assert_eq!(account.form().status, 3);
		account.saved = Some(Saved {
			session: "secret session".into(),
			email: "a@example.test".into(),
			username: "Name".into(),
			..Default::default()
		});
		account.signed_in = true;
		account.otp_required = false;
		let form = account.form();
		assert!(form.signed_in);
		assert!(form.can_forget);
		assert_eq!(form.username, "Name");
		assert_eq!(form.email, "a@example.test");
		assert!(!form.otp_required);
	}

	#[test]
	fn persistence_roundtrip_and_logout_delete_all_material() {
		let secrets = MemorySecrets::default();
		let saved = Saved {
			session: "session".into(),
			email: "email".into(),
			renewal_token: "renewal".into(),
			device_id: "device".into(),
			username: "name".into(),
			otp_token: "otp renewal".into(),
			uuid: "account-uuid".into(),
			identity: Some(server_identity()),
		};
		assert!(save(&secrets, &saved));
		let loaded = read_saved(&secrets).unwrap().unwrap();
		assert_eq!(loaded.session, "session");
		assert_eq!(loaded.renewal_token, "renewal");
		assert_eq!(loaded.email, "email");
		assert_eq!(loaded.identity.unwrap().id(), server_identity().id());
		let loaded = read_saved(&secrets).unwrap().unwrap();
		let mut account = Account { saved: Some(loaded), signed_in: true, ..Default::default() };
		assert!(account.forget(&secrets));
		assert!(read_saved(&secrets).unwrap().is_none());
		assert!(!account.signed_in);
	}

	#[test]
	fn single_operation_and_logout_prevent_stale_resurrection() {
		let mut account = Account::default();
		let generation = account.begin().unwrap();
		assert!(account.begin().is_none());
		account.forget(&MemorySecrets::default());
		assert!(!account.finish(generation));
		let replacement = account.begin().unwrap();
		assert!(!account.finish(generation));
		assert!(account.busy);
		assert!(account.finish(replacement));
	}

	struct Unavailable;
	impl Secrets for Unavailable {
		fn get(&self, _: &str) -> voelin_store::Result<Option<String>> {
			Err(voelin_store::Error::Secrets("locked".into()))
		}
		fn set(&self, _: &str, _: &str) -> voelin_store::Result<()> {
			Err(voelin_store::Error::Secrets("locked".into()))
		}
		fn delete(&self, _: &str) -> voelin_store::Result<()> {
			Err(voelin_store::Error::Secrets("locked".into()))
		}
	}

	#[test]
	fn keyring_failure_is_visible_and_logout_still_clears_memory() {
		assert!(read_saved(&Unavailable).is_err());
		assert!(!save(&Unavailable, &Saved::default()));
		let mut account =
			Account { saved: Some(Saved::default()), signed_in: true, ..Default::default() };
		assert!(!account.forget(&Unavailable));
		assert!(account.saved.is_none());
		assert!(!account.signed_in);
		assert!(account.can_forget);
	}

	#[test]
	fn malformed_or_empty_session_is_not_used() {
		let secrets = MemorySecrets::default();
		secrets.set(KEY, "broken").unwrap();
		assert!(read_saved(&secrets).is_err());
		assert!(save(&secrets, &Saved::default()));
		assert!(read_saved(&secrets).is_err());
	}
}
