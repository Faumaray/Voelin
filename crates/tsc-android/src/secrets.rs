//! Passwords in the Android Keystore.
//!
//! The Kotlin `SecretStore` encrypts each value with an AES-GCM key that
//! lives in the Android Keystore (hardware-backed where the device has it)
//! and keeps the ciphertext in the app's private preferences.

use tsc_store::{Error, Result, Secrets};

use crate::bridge;

pub struct KeystoreSecrets;

fn error(e: jni::errors::Error) -> Error {
	Error::Secrets(format!("Android Keystore: {e}"))
}

impl Secrets for KeystoreSecrets {
	fn get(&self, key: &str) -> Result<Option<String>> {
		bridge::secret_get(key).map_err(error)
	}

	fn set(&self, key: &str, value: &str) -> Result<()> {
		bridge::secret_set(key, value).map_err(error)
	}

	fn delete(&self, key: &str) -> Result<()> {
		bridge::secret_delete(key).map_err(error)
	}
}
