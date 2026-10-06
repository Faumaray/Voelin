//! Validated myTeamSpeak signing credentials and connection-bound public proofs.
//!
//! Serializing an [`Identity`] exports its private key. Persist it only in a
//! credential store, never application settings, diagnostics or packet logs.

use std::fmt;

use base64::prelude::*;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::{constants::ED25519_BASEPOINT_TABLE, scalar::Scalar};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256, Sha512};
use thiserror::Error;
use tsproto_packets::packets::OutCommand;
use tsproto_types::crypto::EccKeyPubEd25519;
use zeroize::Zeroize;

use crate::license::{self, LicenseBlockType, Licenses};

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

	/// The myTS ID's 33 bytes: what myTeamSpeak signs the account's avatar,
	/// badges and identifier token for.
	pub fn id_bytes(&self) -> &[u8] { &self.myts_id }

	/// The certificate of the myTS ID's signature (`pubSignCert`): public.
	pub fn public_signature_certificate(&self) -> &[u8] { &self.public_signature[64..] }

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

/// A myTeamSpeak certificate: a TeamSpeak license chain whose last block's
/// key signs what myTeamSpeak hands out for an account (its avatar, badges
/// and identifier token). Checked as a TeamSpeak 6 server and the official
/// client check it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Certificate {
	block_type: Option<LicenseBlockType>,
	/// Unix seconds.
	not_valid_before: i64,
	not_valid_after: i64,
	/// The leaf's key, derived along the chain from the root.
	key: [u8; 32],
}

#[derive(Debug, Error)]
pub enum CertificateError {
	#[error("myTeamSpeak certificate must start with version 1")]
	Version,
	#[error("myTeamSpeak certificate holds no key")]
	Empty,
	#[error("myTeamSpeak certificate does not parse: {0}")]
	License(#[from] license::Error),
}

impl Certificate {
	/// Parse a certificate whose chain starts at `root` (the TeamSpeak root
	/// key, [`crate::ROOT_KEY`]). Nested validity windows are checked here,
	/// the leaf's against a time by [`Self::valid_at`].
	pub fn parse(data: &[u8], root: &[u8; 32]) -> Result<Self, CertificateError> {
		if data.first() != Some(&1) {
			return Err(CertificateError::Version);
		}
		let licenses = Licenses::parse_ignore_expired(data.to_vec())?;
		let leaf = licenses.blocks.last().ok_or(CertificateError::Empty)?;
		let leaf_data = &data[data.len() - leaf.len..];
		let key = licenses.derive_public_key(EccKeyPubEd25519::from_bytes(*root))?;
		Ok(Self {
			block_type: leaf.get_type(leaf_data).ok(),
			not_valid_before: leaf.get_not_valid_before(leaf_data)?.unix_timestamp(),
			not_valid_after: leaf.get_not_valid_after(leaf_data)?.unix_timestamp(),
			key: key.compress().to_bytes(),
		})
	}

	/// Whether the leaf is a key that signs for myTS IDs (`MYTSID_SIGN`):
	/// a TeamSpeak 6 server takes no other for an avatar or badges.
	pub fn signs_myts_data(&self) -> bool { self.block_type == Some(LicenseBlockType::MytsIdSign) }

	/// Whether the leaf is valid at `unix` seconds.
	pub fn valid_at(&self, unix: i64) -> bool {
		self.not_valid_before <= unix && unix < self.not_valid_after
	}

	/// Until when the leaf is valid, Unix seconds.
	pub fn not_valid_after(&self) -> i64 { self.not_valid_after }

	/// Whether the leaf's key signed `message` (Ed25519, as TeamSpeak checks
	/// it: SHA-512 over R, the key and the message, without the cofactor;
	/// a signature whose S is not reduced is refused).
	pub fn verifies(&self, message: &[u8], signature: &[u8]) -> bool {
		verify_ed25519(&self.key, message, signature)
	}
}

fn verify_ed25519(key: &[u8; 32], message: &[u8], signature: &[u8]) -> bool {
	if signature.len() != 64 {
		return false;
	}
	let Some(public) = CompressedEdwardsY(*key).decompress() else {
		return false;
	};
	let mut s = [0; 32];
	s.copy_from_slice(&signature[32..]);
	let Some(s) = Option::<Scalar>::from(Scalar::from_canonical_bytes(s)) else {
		return false;
	};
	let mut hash = Sha512::new();
	hash.update(&signature[..32]);
	hash.update(key);
	hash.update(message);
	let k = Scalar::from_bytes_mod_order_wide(&hash.finalize().into());
	let r = EdwardsPoint::vartime_double_scalar_mul_basepoint(&k, &-public, &s);
	r.compress().as_bytes() == &signature[..32]
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

	/// A chain from a synthetic root (secret `root`): an intermediate block
	/// when `outer` is set, then a leaf of `kind` with the key `leaf`·B, valid
	/// from `from` to `to` (Unix seconds). Also the leaf's derived secret.
	fn chain(
		root: Scalar, outer: Option<(i64, i64)>, leaf: Scalar, kind: u8, from: i64, to: i64,
	) -> (Vec<u8>, Scalar) {
		fn block(key: &Scalar, kind: u8, from: i64, to: i64, extra: &[u8]) -> (Vec<u8>, Scalar) {
			let mut block = vec![0];
			block.extend_from_slice((ED25519_BASEPOINT_TABLE * key).compress().as_bytes());
			block.push(kind);
			for time in [from, to] {
				block.extend_from_slice(&((time - license::TIMESTAMP_OFFSET) as u32).to_be_bytes());
			}
			block.extend_from_slice(extra);
			let mut hash: [u8; 64] = Sha512::digest(&block[1..]).into();
			hash[0] &= 248;
			hash[31] &= 63;
			hash[31] |= 64;
			let mut low = [0; 32];
			low.copy_from_slice(&hash[..32]);
			(block, Scalar::from_bytes_mod_order(low))
		}
		let mut data = vec![1];
		let mut secret = root;
		if let Some((from, to)) = outer {
			let key = Scalar::from_bytes_mod_order([3; 32]);
			let (outer, hash) = block(&key, 0, from, to, b"\0\0\0\0Synthetic\0");
			data.extend_from_slice(&outer);
			secret += key * hash;
		}
		let (leaf_block, hash) = block(&leaf, kind, from, to, &[]);
		data.extend_from_slice(&leaf_block);
		(data, secret + leaf * hash)
	}

	fn sign(secret: &Scalar, message: &[u8]) -> Vec<u8> {
		let public = (ED25519_BASEPOINT_TABLE * secret).compress().to_bytes();
		let nonce = Scalar::from_bytes_mod_order_wide(
			&Sha512::new().chain_update(secret.as_bytes()).chain_update(message).finalize().into(),
		);
		let r = (ED25519_BASEPOINT_TABLE * &nonce).compress().to_bytes();
		let k = Scalar::from_bytes_mod_order_wide(
			&Sha512::new().chain_update(r).chain_update(public).chain_update(message).finalize().into(),
		);
		let mut signature = r.to_vec();
		signature.extend_from_slice((k * secret + nonce).as_bytes());
		signature
	}

	const NOW: i64 = 1_790_000_000;

	#[test]
	fn certificate_key_signs_along_the_chain() {
		let root = Scalar::from_bytes_mod_order([5; 32]);
		let root_key = (ED25519_BASEPOINT_TABLE * &root).compress().to_bytes();
		let leaf = Scalar::from_bytes_mod_order([7; 32]);
		for outer in [None, Some((NOW - 1000, NOW + 1000))] {
			let (data, secret) = chain(root, outer, leaf, 6, NOW - 10, NOW + 10);
			let certificate = Certificate::parse(&data, &root_key).unwrap();
			assert!(certificate.signs_myts_data());
			assert!(certificate.valid_at(NOW - 10) && certificate.valid_at(NOW + 9));
			assert!(!certificate.valid_at(NOW - 11) && !certificate.valid_at(NOW + 10));
			assert_eq!(certificate.not_valid_after(), NOW + 10);
			let signature = sign(&secret, b"message");
			assert!(certificate.verifies(b"message", &signature));
			assert!(!certificate.verifies(b"messagE", &signature));
			assert!(!certificate.verifies(b"message", &signature[..63]));
			let mut flipped = signature.clone();
			flipped[5] ^= 1;
			assert!(!certificate.verifies(b"message", &flipped));
			// The same S plus the group order: refused, though it is the same
			// point (S must be reduced).
			let mut s = [0u8; 32];
			s.copy_from_slice(&signature[32..]);
			let order: [u8; 32] = [
				0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
				0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
			];
			let mut carry = 0u16;
			for (s, l) in s.iter_mut().zip(order) {
				let sum = u16::from(*s) + u16::from(l) + carry;
				*s = sum as u8;
				carry = sum >> 8;
			}
			let mut unreduced = signature.clone();
			unreduced[32..].copy_from_slice(&s);
			assert!(!certificate.verifies(b"message", &unreduced));
			// Another root: another key.
			let other = Certificate::parse(&data, &crate::ROOT_KEY).unwrap();
			assert!(!other.verifies(b"message", &signature));
		}
	}

	#[test]
	fn ed25519_matches_rfc_8032() {
		// RFC 8032, 7.1, tests 1 and 2.
		let key = hex::<32>("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
		let signature = hex::<64>(
			"e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
		);
		assert!(verify_ed25519(&key, b"", &signature));
		assert!(!verify_ed25519(&key, b"\x72", &signature));
		let key = hex::<32>("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c");
		let signature = hex::<64>(
			"92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
		);
		assert!(verify_ed25519(&key, b"\x72", &signature));
		assert!(!verify_ed25519(&key, b"\x73", &signature));
	}

	#[test]
	fn certificate_kinds_windows_and_format() {
		let root = Scalar::from_bytes_mod_order([5; 32]);
		let root_key = (ED25519_BASEPOINT_TABLE * &root).compress().to_bytes();
		let leaf = Scalar::from_bytes_mod_order([7; 32]);
		for kind in [4, 5, 7, 32] {
			let (data, _) = chain(root, None, leaf, kind, NOW - 10, NOW + 10);
			assert!(!Certificate::parse(&data, &root_key).unwrap().signs_myts_data(), "{}", kind);
		}
		// A leaf valid longer than the block above it.
		let (data, _) = chain(root, Some((NOW - 5, NOW + 5)), leaf, 6, NOW - 10, NOW + 10);
		assert!(Certificate::parse(&data, &root_key).is_err());
		let (mut data, _) = chain(root, None, leaf, 6, NOW - 10, NOW + 10);
		data[0] = 0;
		assert!(matches!(Certificate::parse(&data, &root_key), Err(CertificateError::Version)));
		assert!(matches!(Certificate::parse(&[1], &root_key), Err(CertificateError::Empty)));
		assert!(Certificate::parse(&[], &root_key).is_err());
		data[0] = 1;
		data[34] = 9;
		assert!(Certificate::parse(&data, &root_key).is_err());
	}
}
