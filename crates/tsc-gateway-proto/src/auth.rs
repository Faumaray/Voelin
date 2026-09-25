//! Identity-challenge authentication.

use base64::prelude::*;
use sha1::{Digest, Sha1};
use sha2::Sha256;
use tsproto_types::crypto::{EccKeyPrivP256, EccKeyPubP256};

/// Maximum allowed difference between the client's and our clock.
pub const MAX_CLOCK_SKEW_SECS: i64 = 60;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
	#[error("invalid public key")]
	BadKey,
	#[error("invalid signature")]
	BadSignature,
	#[error("timestamp too far from the gateway's clock")]
	ClockSkew,
}

/// The bytes a client signs.
pub fn challenge(gateway_id: &str, server_uid: &str, nonce: &str, ts: i64) -> Vec<u8> {
	format!("tsgw-auth-v1\n{gateway_id}\n{server_uid}\n{nonce}\n{ts}").into_bytes()
}

/// Client side: sign the challenge with the identity key (base64 DER).
pub fn sign_challenge(
	key: &EccKeyPrivP256,
	gateway_id: &str,
	server_uid: &str,
	nonce: &str,
	ts: i64,
) -> String {
	BASE64_STANDARD.encode(key.clone().sign(&challenge(gateway_id, server_uid, nonce, ts)))
}

/// The unique ids a TeamSpeak identity has on the two server generations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniqueIds {
	/// TeamSpeak 3: `base64(SHA1(omega))`.
	pub ts3: String,
	/// TeamSpeak 6: `base64(SHA256(omega))` (verified on 6.0.0-beta13.1).
	pub ts6: String,
}

impl UniqueIds {
	pub fn from_omega(omega: &str) -> Self {
		Self {
			ts3: BASE64_STANDARD.encode(Sha1::digest(omega.as_bytes())),
			ts6: BASE64_STANDARD.encode(Sha256::digest(omega.as_bytes())),
		}
	}

	/// The id the given server knows the identity by.
	pub fn for_server(&self, is_ts6: bool) -> &str {
		if is_ts6 { &self.ts6 } else { &self.ts3 }
	}
}

/// Gateway side: check the signature and clock, return the identity's ids.
pub fn verify_auth(
	omega: &str,
	signature: &str,
	gateway_id: &str,
	server_uid: &str,
	nonce: &str,
	ts: i64,
	now: i64,
) -> Result<UniqueIds, AuthError> {
	if (now - ts).abs() > MAX_CLOCK_SKEW_SECS {
		return Err(AuthError::ClockSkew);
	}
	let key = EccKeyPubP256::from_ts(omega).map_err(|_| AuthError::BadKey)?;
	let signature = BASE64_STANDARD.decode(signature).map_err(|_| AuthError::BadSignature)?;
	key.verify(&challenge(gateway_id, server_uid, nonce, ts), &signature)
		.map_err(|_| AuthError::BadSignature)?;
	Ok(UniqueIds::from_omega(omega))
}

/// Hash-cash security level of an identity, as TeamSpeak computes it:
/// leading zero bits of `SHA1(omega + decimal offset)`, bytes from the start,
/// bits from least significant.
pub fn identity_level(omega: &str, key_offset: u64) -> u8 {
	let hash = Sha1::digest(format!("{omega}{key_offset}").as_bytes());
	let mut level = 0;
	for &byte in hash.as_slice() {
		if byte == 0 {
			level += 8;
		} else {
			level += byte.trailing_zeros() as u8;
			break;
		}
	}
	level
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sign_and_verify() {
		let key = EccKeyPrivP256::create();
		let omega = key.to_pub().to_ts();
		let sig = sign_challenge(&key, "gw", "srv", "nonce", 1000);
		let ids = verify_auth(&omega, &sig, "gw", "srv", "nonce", 1000, 1010).unwrap();
		assert_eq!(ids.ts3, key.to_pub().get_uid());
		assert_eq!(ids.ts6.len(), 44);

		// Any change to the challenge breaks the signature.
		assert_eq!(
			verify_auth(&omega, &sig, "gw", "srv", "other", 1000, 1000),
			Err(AuthError::BadSignature)
		);
		assert_eq!(
			verify_auth(&omega, &sig, "gw2", "srv", "nonce", 1000, 1000),
			Err(AuthError::BadSignature)
		);
		// Someone else's key.
		let other = EccKeyPrivP256::create().to_pub().to_ts();
		assert_eq!(
			verify_auth(&other, &sig, "gw", "srv", "nonce", 1000, 1000),
			Err(AuthError::BadSignature)
		);
		// Stale timestamp.
		assert_eq!(
			verify_auth(&omega, &sig, "gw", "srv", "nonce", 1000, 2000),
			Err(AuthError::ClockSkew)
		);
		assert_eq!(
			verify_auth("garbage", &sig, "gw", "srv", "nonce", 1000, 1000),
			Err(AuthError::BadKey)
		);
	}

	#[test]
	fn ts6_uid_is_sha256_of_omega() {
		// One identity as seen in clientdblist of TeamSpeak 3.13.8 and 6.0.0-beta13.1.
		let omega = "MEsDAgcAAgEgAiBCmlKdgFw427AndT7fm2Wx4bgENOi6eQRUzwbT7Dnv9wIgcJETAwMtVjWdsCqZejblVJruSKWe4l845nWNAMYMkpk=";
		let ids = UniqueIds::from_omega(omega);
		assert_eq!(ids.ts3, "uXDkxSZbv0IoA0z/toW3guGpUbI=");
		assert_eq!(ids.ts6, "xEZDkyeGdZ4bOSBCKqzTw8jVz2jsZCrJDRl9F8WWd3M=");
		assert_eq!(ids.for_server(true), ids.ts6);
	}

	#[test]
	fn level_matches_tsproto() {
		// Same algorithm as the vendored client (and the TeamSpeak server).
		let identity = tsproto::Identity::create();
		let omega = identity.key().to_pub().to_ts();
		assert!(identity.level() >= 8);
		assert_eq!(identity_level(&omega, identity.counter()), identity.level());
		for offset in 0..200 {
			assert_eq!(
				identity_level(&omega, offset),
				tsproto::algorithms::get_hash_cash_level(&omega, offset)
			);
		}
	}
}
