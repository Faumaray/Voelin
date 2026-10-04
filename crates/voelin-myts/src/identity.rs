//! Authenticated account-key unwrap; plaintext keys never leave this module
//! except inside the validated, redacted protocol identity.
use aes_gcm::{Aes256Gcm, KeyInit, aead::AeadInOut};
use ctr::cipher::{KeyIvInit, StreamCipher};
use pbkdf2::sha2::{Digest, Sha512};
use schema_api::api;
use subtle::ConstantTimeEq;
use tsproto::myts::Identity;
use zeroize::Zeroizing;

/// Static, non-secret explanation for a usable login without a server identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
	#[error("sign in with your password to unlock the server identity")]
	PasswordRequired,
	#[error("the account did not provide a server identity")]
	Missing,
	#[error("the account uses an unsupported encryption-key version")]
	UnsupportedKeyVersion,
	#[error("the account encryption key has an invalid format")]
	InvalidRootKey,
	#[error("the account encryption key could not be authenticated")]
	RootAuthentication,
	#[error("the account server identity has an invalid format")]
	InvalidIdentity,
	#[error("the account private key could not be verified")]
	PrivateKeyIntegrity,
}

pub(crate) fn encryption_key(email: &str, password: &str) -> Zeroizing<[u8; 32]> {
	let salt = Zeroizing::new(format!("{}ts3Encryption{password}", email.to_ascii_lowercase()));
	Zeroizing::new(pbkdf2::pbkdf2_hmac_array::<Sha512, 32>(
		password.as_bytes(),
		salt.as_bytes(),
		10_000,
	))
}

fn unwrap_root(wrapped: &[u8], key: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, IdentityError> {
	match wrapped.first() {
		None => return Err(IdentityError::InvalidRootKey),
		Some(2) => {}
		Some(_) => return Err(IdentityError::UnsupportedKeyVersion),
	}
	if wrapped.len() != 61 {
		return Err(IdentityError::InvalidRootKey);
	}
	let mut plaintext = Zeroizing::new([0; 32]);
	plaintext.copy_from_slice(&wrapped[29..]);
	let cipher = Aes256Gcm::new(key.into());
	cipher
		.decrypt_inout_detached(
			wrapped[17..29].try_into().map_err(|_| IdentityError::InvalidRootKey)?,
			b"",
			plaintext.as_mut_slice().into(),
			wrapped[1..17].try_into().map_err(|_| IdentityError::InvalidRootKey)?,
		)
		.map_err(|_| IdentityError::RootAuthentication)?;
	Ok(plaintext)
}

fn unwrap_private(wrapped: &[u8], root: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, IdentityError> {
	if wrapped.len() != 112 {
		return Err(IdentityError::InvalidIdentity);
	}
	let mut plaintext = Zeroizing::new([0; 32]);
	plaintext.copy_from_slice(&wrapped[16..48]);
	let mut cipher = ctr::Ctr128LE::<aes::Aes256>::new(
		root.into(),
		wrapped[..16].try_into().map_err(|_| IdentityError::InvalidIdentity)?,
	);
	cipher.apply_keystream(plaintext.as_mut_slice());
	let digest = Zeroizing::new(<[u8; 64]>::from(Sha512::digest(plaintext.as_slice())));
	if !bool::from(digest.as_slice().ct_eq(&wrapped[48..])) {
		return Err(IdentityError::PrivateKeyIntegrity);
	}
	Ok(plaintext)
}

pub(crate) fn decrypt(
	response: &api::LoginSession,
	key: &[u8; 32],
) -> Result<Identity, IdentityError> {
	let data = response.myts_id_data.as_ref().ok_or(IdentityError::Missing)?;
	let encrypted = data.user_private_key.as_ref().ok_or(IdentityError::Missing)?;
	let public_key =
		data.user_public_key.as_slice().try_into().map_err(|_| IdentityError::InvalidIdentity)?;
	let root = unwrap_root(&response.key, key)?;
	let private = unwrap_private(&encrypted.encrypted_user_private_key, &root)?;
	Identity::new(
		data.my_teamspeak_id.clone(),
		// The HTTP schema uses the numeric timestamp; the voice protocol's
		// decimal acTime represents its byte-swapped integer. The server
		// verifies pubKey || acTime.to_le_bytes() || myTS ID. The official
		// login-to-storage boundary performs this same conversion.
		data.account_creation_time.swap_bytes(),
		public_key,
		*private,
		data.public_signature.clone(),
	)
	.map_err(|_| IdentityError::InvalidIdentity)
}

#[cfg(test)]
mod tests {
	use super::*;
	// Independent Python hashlib + cryptography AESGCM/AES-ECB fixtures.
	// CTR is assembled by incrementing the counter as a little-endian integer.
	const ROOT: &str = "0201123b3434c3b04a2b6713e5a14c5f10000102030405060708090a0be7012a4987a157ece9103814f297d77cae6a2084730d7e049895cb0ba2334178";
	const PRIVATE: &str = "000102030405060708090a0b0c0d0e0f7a4f26742cde57b1d8077f162eee88bd3afe99c66443d961cd28a368ac98cc2f887af58a36202e05c4c1cfec5bf6c61fad66bca851536004074b31f1b56e4ac93d9c9fc20dc59e01fecab23063ef341b2d2d75c4e8e4fa1e9ba958658260e336";
	fn hex(s: &str) -> Vec<u8> {
		s.as_bytes()
			.chunks_exact(2)
			.map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
			.collect()
	}
	#[test]
	fn independent_password_root_and_private_vectors() {
		let key = encryption_key("User@Example.TEST", "password");
		assert_eq!(
			key.as_slice(),
			hex("81ccdf8e98030891e11700e5d76dfa6c52ff01121f668ab6a5525742ed18c5ca")
		);
		let root = unwrap_root(&hex(ROOT), &key).unwrap();
		assert_eq!(root.as_slice(), (0..32).collect::<Vec<u8>>());
		let private = unwrap_private(&hex(PRIVATE), &root).unwrap();
		assert_eq!(private.as_slice(), (32..64).collect::<Vec<u8>>());
	}
	#[test]
	fn decrypts_response_and_rejects_a_mismatched_public_key() {
		let key = encryption_key("user@example.test", "password");
		let mut response = api::LoginSession {
			key: hex(ROOT),
			myts_id_data: Some(api::MyTeamSpeakIdData {
				// Independently computed scalar multiplication using affine Edwards arithmetic.
				user_public_key: hex(
					"e67b030457ea1be59391ea3f28748d12abe417fef0f7bb6ad68e54e836eda6c4",
				),
				user_private_key: Some(api::my_team_speak_id_data::UserPrivateKey {
					encrypted_user_private_key: hex(PRIVATE),
					// Never trust this alternate server plaintext field.
					decrypted_user_private_key: vec![0; 32],
				}),
				account_creation_time: 123,
				my_teamspeak_id: vec![1; 33],
				public_signature: vec![2; 65],
			}),
			..Default::default()
		};
		let identity = decrypt(&response, &key).unwrap();
		assert_eq!(identity.id(), "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB");
		assert_eq!(identity.proof(&[0; 64]).creation_time, 123u64.swap_bytes());
		response.myts_id_data.as_mut().unwrap().user_public_key[0] ^= 1;
		assert_eq!(decrypt(&response, &key).unwrap_err(), IdentityError::InvalidIdentity);
	}

	#[test]
	fn every_authenticated_root_byte_is_checked() {
		let key = encryption_key("user@example.test", "password");
		for index in 1..61 {
			let mut wrapped = hex(ROOT);
			wrapped[index] ^= 1;
			assert_eq!(unwrap_root(&wrapped, &key).unwrap_err(), IdentityError::RootAuthentication);
		}
		assert_eq!(
			unwrap_root(&hex(ROOT), &[0; 32]).unwrap_err(),
			IdentityError::RootAuthentication
		);
	}
	#[test]
	fn every_private_item_byte_is_checked() {
		let root = std::array::from_fn(|i| i as u8);
		for index in 0..112 {
			let mut wrapped = hex(PRIVATE);
			wrapped[index] ^= 1;
			assert_eq!(
				unwrap_private(&wrapped, &root).unwrap_err(),
				IdentityError::PrivateKeyIntegrity
			);
		}
	}
	#[test]
	fn missing_unsupported_and_malformed_are_explicit() {
		assert_eq!(
			decrypt(&api::LoginSession::default(), &[0; 32]).unwrap_err(),
			IdentityError::Missing
		);
		assert_eq!(unwrap_root(&[], &[0; 32]).unwrap_err(), IdentityError::InvalidRootKey);
		assert_eq!(unwrap_root(&[1], &[0; 32]).unwrap_err(), IdentityError::UnsupportedKeyVersion);
		for size in [1, 60, 62] {
			let mut wrapped = vec![0; size];
			wrapped[0] = 2;
			assert_eq!(unwrap_root(&wrapped, &[0; 32]).unwrap_err(), IdentityError::InvalidRootKey);
		}
		for size in [0, 111, 113] {
			assert_eq!(
				unwrap_private(&vec![0; size], &[0; 32]).unwrap_err(),
				IdentityError::InvalidIdentity
			);
		}
	}
}
