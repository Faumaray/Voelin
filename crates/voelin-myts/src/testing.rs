//! Certificates and what they sign, from a synthetic root, for tests here
//! and in the crates using this one (feature `testing`): TeamSpeak's root
//! signs nothing a test can make, so tests pass the synthetic root to the
//! checks (`*_from`).

use crate::presentation::uuid_bytes;
use curve25519_dalek::{constants::ED25519_BASEPOINT_TABLE, scalar::Scalar};
use schema_api::api::{self, tschat, user};
use schema_api::wire;
use sha2::{Digest, Sha512};

/// When the chains of [`Chain::signing`] are valid, Unix seconds.
pub const NOW: i64 = 1_790_000_000;
/// `tsproto::license::TIMESTAMP_OFFSET`.
const OFFSET: i64 = 0x50e2_2700;

/// A synthetic root and a chain from it, and how to sign as its leaf.
pub struct Chain {
	pub root: [u8; 32],
	pub certificate: Vec<u8>,
	secret: Scalar,
}

impl Chain {
	/// A leaf of `kind` with key `seed`, valid from `from` to `to` (Unix
	/// seconds). Every chain has the same root.
	pub fn new(seed: u8, kind: u8, from: i64, to: i64) -> Self {
		let root = Scalar::from_bytes_mod_order([0x11; 32]);
		let leaf = Scalar::from_bytes_mod_order([seed; 32]);
		let mut block = vec![0];
		block.extend_from_slice((ED25519_BASEPOINT_TABLE * &leaf).compress().as_bytes());
		block.push(kind);
		for time in [from, to] {
			block.extend_from_slice(&u32::try_from(time - OFFSET).unwrap().to_be_bytes());
		}
		let mut hash: [u8; 64] = Sha512::digest(&block[1..]).into();
		hash[0] &= 248;
		hash[31] &= 63;
		hash[31] |= 64;
		let hash = Scalar::from_bytes_mod_order(hash[..32].try_into().unwrap());
		let mut certificate = vec![1];
		certificate.extend_from_slice(&block);
		Self {
			root: (ED25519_BASEPOINT_TABLE * &root).compress().to_bytes(),
			certificate,
			secret: leaf * hash + root,
		}
	}

	/// A `MYTSID_SIGN` leaf valid at [`NOW`].
	pub fn signing(seed: u8) -> Self {
		Self::new(seed, 6, NOW - 3600, NOW + 30 * 86_400)
	}

	pub fn sign(&self, message: &[u8]) -> Vec<u8> {
		let public = (ED25519_BASEPOINT_TABLE * &self.secret).compress().to_bytes();
		let nonce = Scalar::from_bytes_mod_order_wide(
			&Sha512::new()
				.chain_update(self.secret.as_bytes())
				.chain_update(message)
				.finalize()
				.into(),
		);
		let r = (ED25519_BASEPOINT_TABLE * &nonce).compress().to_bytes();
		let k = Scalar::from_bytes_mod_order_wide(
			&Sha512::new()
				.chain_update(r)
				.chain_update(public)
				.chain_update(message)
				.finalize()
				.into(),
		);
		let mut signature = r.to_vec();
		signature.extend_from_slice((k * self.secret + nonce).as_bytes());
		signature
	}

	/// An `AvatarData` with one picture, signed for `myts_id` at
	/// `timestamp`.
	pub fn avatar(&self, myts_id: &[u8], timestamp: u64) -> Vec<u8> {
		let info = api::AvatarInfo {
			map: vec![api::avatar_info::AvatarMap {
				state: api::AvatarState::Online as i32,
				name: format!("https://avatars.example.test/{timestamp}.png"),
			}],
		};
		let mut message = wire::encode(&info);
		message.extend_from_slice(myts_id);
		message.extend_from_slice(&timestamp.to_be_bytes());
		wire::encode(&api::AvatarData {
			info: Some(info),
			timestamp,
			sign: self.sign(&message),
			..Default::default()
		})
	}

	/// A `SignedUserBadge` for `myts_id`; `uuid` in the 36-character form.
	pub fn badge(&self, myts_id: &[u8], uuid: &str, name: &str) -> Vec<u8> {
		let mut message = uuid_bytes(uuid).expect("a badge id a server reads").to_vec();
		message.extend_from_slice(&7u64.to_be_bytes());
		message.extend_from_slice(myts_id);
		wire::encode(&user::SignedUserBadge {
			badge: Some(user::UserBadge {
				uuid: uuid.into(),
				name: name.into(),
				url: format!("https://badges.example.test/{name}.svg"),
				..Default::default()
			}),
			sign: self.sign(&message),
			sign_timestamp: 7,
		})
	}

	/// A User Tag's token (`MatrixIdentifierToken`) for `tags`, signed for
	/// `myts_id`.
	pub fn user_tag_token(&self, tags: &[&str], myts_id: &[u8]) -> Vec<u8> {
		let tags =
			tschat::TschatIdentifierTagList { tag: tags.iter().map(|t| t.to_string()).collect() };
		let mut message = wire::encode(&tags);
		message.extend_from_slice(&1_760_000_000_000u64.to_be_bytes());
		message.extend_from_slice(myts_id);
		wire::encode(&tschat::MatrixIdentifierToken {
			signature: self.sign(&message),
			sign_certificate: self.certificate.clone(),
			timestamp: 1_760_000_000_000,
			tags: Some(tags),
		})
	}
}
