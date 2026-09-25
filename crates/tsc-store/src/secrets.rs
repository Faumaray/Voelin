//! Storage for passwords and credentials.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::Result;

/// Key-value secret storage. `key` names what the secret is for, e.g.
/// `bookmark/3/server-password` or `bookmark/3/query-password`.
pub trait Secrets: Send + Sync {
	fn get(&self, key: &str) -> Result<Option<String>>;
	fn set(&self, key: &str, value: &str) -> Result<()>;
	fn delete(&self, key: &str) -> Result<()>;
}

/// In-memory secrets, for tests and platforms without a keyring.
#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<String, String>>);

impl Secrets for MemorySecrets {
	fn get(&self, key: &str) -> Result<Option<String>> {
		Ok(self.0.lock().unwrap().get(key).cloned())
	}

	fn set(&self, key: &str, value: &str) -> Result<()> {
		self.0.lock().unwrap().insert(key.to_string(), value.to_string());
		Ok(())
	}

	fn delete(&self, key: &str) -> Result<()> {
		self.0.lock().unwrap().remove(key);
		Ok(())
	}
}

/// The platform keyring: Secret Service on Linux, Credential Manager on
/// Windows, Keychain on macOS.
#[cfg(feature = "keyring")]
pub struct KeyringSecrets {
	service: String,
}

#[cfg(feature = "keyring")]
impl KeyringSecrets {
	/// `service` distinguishes this application's entries, e.g. the app id.
	pub fn new(service: impl Into<String>) -> Self {
		Self { service: service.into() }
	}

	fn entry(&self, key: &str) -> Result<keyring::Entry> {
		keyring::Entry::new(&self.service, key).map_err(|e| crate::Error::Secrets(e.to_string()))
	}
}

#[cfg(feature = "keyring")]
impl Secrets for KeyringSecrets {
	fn get(&self, key: &str) -> Result<Option<String>> {
		match self.entry(key)?.get_password() {
			Ok(v) => Ok(Some(v)),
			Err(keyring::Error::NoEntry) => Ok(None),
			Err(e) => Err(crate::Error::Secrets(e.to_string())),
		}
	}

	fn set(&self, key: &str, value: &str) -> Result<()> {
		self.entry(key)?.set_password(value).map_err(|e| crate::Error::Secrets(e.to_string()))
	}

	fn delete(&self, key: &str) -> Result<()> {
		match self.entry(key)?.delete_credential() {
			Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
			Err(e) => Err(crate::Error::Secrets(e.to_string())),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn memory_secrets() {
		let s = MemorySecrets::default();
		assert_eq!(s.get("a").unwrap(), None);
		s.set("a", "1").unwrap();
		assert_eq!(s.get("a").unwrap().as_deref(), Some("1"));
		s.delete("a").unwrap();
		s.delete("a").unwrap();
		assert_eq!(s.get("a").unwrap(), None);
	}
}
