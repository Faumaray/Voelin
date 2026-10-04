//! Validated myTeamSpeak signing credentials and connection-bound public proofs.
//!
//! Serializing an [`Identity`] exports its private key. Persist it only in a
//! credential store, never application settings, diagnostics or packet logs.

use std::fmt;

use base64::prelude::*;
use curve25519_dalek::{constants::ED25519_BASEPOINT_TABLE, scalar::Scalar};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256, Sha512};
use thiserror::Error;
use tsproto_packets::packets::OutCommand;
use zeroize::Zeroize;

/// An account identity; distinct from the account UUID and server identity.
#[derive(Clone, Serialize)]
pub struct Identity {
	myts_id: Vec<u8>,
	creation_time: u64,
	public_key: [u8; 32],
	private_key: [u8; 32],
	public_signature: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum IdentityError {
	#[error("myTeamSpeak identity must contain 33 bytes")]
	InvalidId,
	#[error("myTeamSpeak public signature must contain 65 to 4096 bytes")]
	InvalidSignature,
	#[error("myTeamSpeak private key is zero or does not match its public key")]
	InvalidKeyPair,
}

impl fmt::Debug for Identity {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("Identity([REDACTED])") }
}

impl Drop for Identity {
	fn drop(&mut self) { self.private_key.zeroize(); }
}

impl<'de> Deserialize<'de> for Identity {
	fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
		#[derive(Deserialize)]
		#[serde(deny_unknown_fields)]
		struct Stored {
			myts_id: Vec<u8>,
			creation_time: u64,
			public_key: [u8; 32],
			private_key: [u8; 32],
			public_signature: Vec<u8>,
		}
		let mut stored = Stored::deserialize(deserializer)?;
		let result = Self::new(
			stored.myts_id,
			stored.creation_time,
			stored.public_key,
			stored.private_key,
			stored.public_signature,
		);
		stored.private_key.zeroize();
		result.map_err(serde::de::Error::custom)
	}
}

impl Identity {
	/// `creation_time` is the voice protocol's decimal `acTime` integer,
	/// byte-swapped from the account service's numeric creation timestamp.
	pub fn new(
		myts_id: Vec<u8>, creation_time: u64, public_key: [u8; 32], mut private_key: [u8; 32],
		public_signature: Vec<u8>,
	) -> Result<Self, IdentityError> {
		let result = (|| {
			if myts_id.len() != 33 {
				return Err(IdentityError::InvalidId);
			}
			if !(65..=4096).contains(&public_signature.len()) {
				return Err(IdentityError::InvalidSignature);
			}
			let mut scalar = Scalar::from_bytes_mod_order(private_key);
			let valid = scalar != Scalar::ZERO
				&& (ED25519_BASEPOINT_TABLE * &scalar).compress().to_bytes() == public_key;
			scalar.zeroize();
			if !valid {
				return Err(IdentityError::InvalidKeyPair);
			}
			Ok(Self { myts_id, creation_time, public_key, private_key, public_signature })
		})();
		private_key.zeroize();
		result
	}

	pub fn id(&self) -> String { BASE64_STANDARD.encode(&self.myts_id) }

	/// Sign this connection's challenge with the original raw private scalar.
	/// This is not Ed25519's seed expansion: the account already stores a scalar.
	pub fn proof(&self, shared_iv: &[u8; 64]) -> Proof {
		let mut challenge = Sha256::new();
		challenge.update([0x30, 0x08, 0, 0, 0, 0]);
		challenge.update(shared_iv);
		let mut message = challenge.finalize().to_vec();
		message.extend_from_slice(b"MyTeamSpeakID");
		let mut nonce_hash = Sha512::new();
		nonce_hash.update(self.private_key);
		nonce_hash.update(&message);
		let mut nonce = Scalar::from_bytes_mod_order_wide(&nonce_hash.finalize().into());
		let public_nonce = (ED25519_BASEPOINT_TABLE * &nonce).compress().to_bytes();
		let mut hash = Sha512::new();
		hash.update(public_nonce);
		hash.update(self.public_key);
		hash.update(&message);
		let challenge = Scalar::from_bytes_mod_order_wide(&hash.finalize().into());
		let mut scalar = Scalar::from_bytes_mod_order(self.private_key);
		let mut response = challenge * scalar + nonce;
		let mut signature = [0; 64];
		signature[..32].copy_from_slice(&public_nonce);
		signature[32..].copy_from_slice(response.as_bytes());
		scalar.zeroize();
		nonce.zeroize();
		response.zeroize();
		Proof {
			myteamspeak_id: self.id(),
			creation_time: self.creation_time,
			public_key: BASE64_STANDARD.encode(self.public_key),
			auth_signature: BASE64_STANDARD.encode(signature),
			public_signature: BASE64_STANDARD.encode(&self.public_signature[..64]),
			public_signature_certificate: BASE64_STANDARD.encode(&self.public_signature[64..]),
		}
	}
}

/// Public, connection-bound authentication values. Never contains private keys.
#[derive(Clone)]
pub struct Proof {
	pub myteamspeak_id: String,
	pub creation_time: u64,
	pub public_key: String,
	pub auth_signature: String,
	pub public_signature: String,
	pub public_signature_certificate: String,
}

impl fmt::Debug for Proof {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("Proof([REDACTED])") }
}

impl Proof {
	/// Append the official case-sensitive wire fields to an outgoing command.
	pub fn write_to(&self, packet: &mut OutCommand) {
		packet.write_arg("myTeamspeakId", &self.myteamspeak_id);
		packet.write_arg("acTime", &self.creation_time);
		packet.write_arg("userPubKey", &self.public_key);
		packet.write_arg("authSign", &self.auth_signature);
		packet.write_arg("pubSign", &self.public_signature);
		packet.write_arg("pubSignCert", &self.public_signature_certificate);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tsproto_packets::packets::{Direction, Flags, PacketType};

	fn hex<const N: usize>(value: &str) -> [u8; N] {
		let mut bytes = [0; N];
		for (i, byte) in bytes.iter_mut().enumerate() {
			*byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).unwrap();
		}
		bytes
	}

	fn identity() -> Identity {
		Identity::new(
			vec![7; 33],
			123456,
			hex("ce92350b547b6cf028df0618bf9aba55f949930059308d83ebd727e13472ed99"),
			std::array::from_fn(|i| i as u8 + 1),
			vec![9; 65],
		)
		.unwrap()
	}

	#[test]
	fn raw_scalar_signature_matches_independent_vector() {
		// Python integer Edwards arithmetic + hashlib, independent of dalek.
		let proof = identity().proof(&std::array::from_fn(|i| i as u8));
		let expected = hex::<64>(
			"d56e1808e1ded53e20fc2849f016603c88eefe0494d75e9b869653e2bacaa495c562b571289d88a561c0f4b568cf90cc515b557b6d4321962e94ccf85f105a08",
		);
		assert_eq!(BASE64_STANDARD.decode(proof.auth_signature).unwrap(), expected);
		assert_ne!(
			identity().proof(&[0; 64]).auth_signature,
			identity().proof(&[1; 64]).auth_signature
		);
	}

	#[test]
	fn credentials_roundtrip_validate_and_redact() {
		let identity = identity();
		let stored = serde_json::to_value(&identity).unwrap();
		let restored: Identity = serde_json::from_value(stored.clone()).unwrap();
		assert_eq!(serde_json::to_value(&restored).unwrap(), stored);
		assert_eq!(
			identity.proof(&[0; 64]).auth_signature,
			restored.proof(&[0; 64]).auth_signature
		);
		assert_eq!(format!("{:?}", restored), "Identity([REDACTED])");
		assert_eq!(format!("{:?}", restored.proof(&[0; 64])), "Proof([REDACTED])");
		for field in ["myts_id", "public_signature", "public_key", "private_key"] {
			let mut invalid = stored.clone();
			invalid[field] = serde_json::json!([]);
			assert!(serde_json::from_value::<Identity>(invalid).is_err(), "{}", field);
		}
		for private in [vec![0; 32], vec![1; 32]] {
			let mut invalid = stored.clone();
			invalid["private_key"] = serde_json::json!(private);
			assert!(serde_json::from_value::<Identity>(invalid).is_err());
		}
		for length in [64, 4097] {
			let mut invalid = stored.clone();
			invalid["public_signature"] = serde_json::json!(vec![9; length]);
			assert!(serde_json::from_value::<Identity>(invalid).is_err());
		}
	}

	#[test]
	fn proof_writes_exact_public_wire_fields() {
		let mut command =
			OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, "clientinit");
		identity().proof(&[0; 64]).write_to(&mut command);
		let packet = command.into_packet();
		let wire = std::str::from_utf8(packet.content()).unwrap();
		let fields: Vec<_> =
			wire.split(' ').skip(1).map(|field| field.split('=').next().unwrap()).collect();
		assert_eq!(fields, [
			"myTeamspeakId",
			"acTime",
			"userPubKey",
			"authSign",
			"pubSign",
			"pubSignCert"
		]);
		assert!(!wire.contains("private"));
	}
}
