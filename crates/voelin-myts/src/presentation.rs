//! What voice servers are shown of the account besides its myTS ID
//! (TeamSpeak 6): its avatar and badges, as myTeamSpeak signed them for
//! the myTS ID. A server checks them with the one certificate of each
//! `updatemytsdata` and silently shows nothing of what does not verify, so
//! they are checked here first, the same way, to choose a certificate that
//! verifies them.
//!
//! All of it is public: certificates, signatures, the avatar's links,
//! badge ids.

use crate::{Error, profile};
use schema_api::api::{self, user};
use schema_api::{SessionToken, wire};
use tsproto::myts::Certificate;

/// `RequestContactsAvatarInfoResponse.data`, `AvatarInfoMap.info`.
const AVATAR_DATA: u32 = 1;
const AVATAR_INFO: u32 = 2;
/// `AvatarData.info`.
const INFO: u32 = 1;
/// `UserBadgesSignedResponse.list`, `UserBadgesSignedList.badges`.
const SIGNED_LIST: u32 = 1;
const SIGNED_BADGES: u32 = 1;

/// The account's avatar as myTeamSpeak's avatar service hands it out for
/// the account's own myTS ID (`requestContactsAvatar`), with the
/// certificate that came with it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct OwnAvatar {
	/// `OptionalAvatarDataContactInfo.user_cert`: a certificate like the
	/// one a password sign-in brings.
	pub certificate: Vec<u8>,
	/// The `AvatarData` exactly as it came.
	pub avatar: Vec<u8>,
}

impl std::fmt::Debug for OwnAvatar {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("OwnAvatar")
			.field("certificate", &self.certificate.len())
			.field("avatar", &self.avatar.len())
			.finish()
	}
}

/// One of the account's badges, signed for its myTS ID (`getSignedBadges`).
#[derive(Clone, PartialEq, Eq)]
pub struct SignedBadge {
	pub uuid: String,
	pub name: String,
	/// The badge's picture.
	pub url: String,
	/// The `SignedUserBadge` as it came.
	raw: Vec<u8>,
}

impl SignedBadge {
	/// A `SignedUserBadge` as it came (and as [`Self::raw`] keeps it).
	pub fn parse(raw: &[u8]) -> Option<Self> {
		let signed: user::SignedUserBadge = wire::decode(raw).ok()?;
		let badge = signed.badge?;
		Some(Self { uuid: badge.uuid, name: badge.name, url: badge.url, raw: raw.to_vec() })
	}

	pub fn raw(&self) -> &[u8] {
		&self.raw
	}
}

impl std::fmt::Debug for SignedBadge {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("SignedBadge").field("uuid", &self.uuid).field("name", &self.name).finish()
	}
}

/// `badges` as a `UserBadgesSignedList` (`myts_signed_badge`), each badge
/// exactly as it came.
pub fn badge_list<'a>(badges: impl IntoIterator<Item = &'a SignedBadge>) -> Vec<u8> {
	let mut list = Vec::new();
	for badge in badges {
		list.push(((SIGNED_BADGES << 3) | 2) as u8);
		let mut length = badge.raw.len();
		while length >= 0x80 {
			list.push(length as u8 | 0x80);
			length >>= 7;
		}
		list.push(length as u8);
		list.extend_from_slice(&badge.raw);
	}
	list
}

pub(crate) fn own_avatar_request(
	token: &SessionToken,
	myts_id: &[u8],
) -> user::RequestContactsAvatarInfoRequest {
	user::RequestContactsAvatarInfoRequest {
		session: token.as_str().into(),
		id: vec![user::AvatarRequestId {
			key: Some(wire::pack_any(&user::avatar_request_id::MytsKey { id: myts_id.to_vec() })),
		}],
	}
}

/// The reply to [`own_avatar_request`]: the entry for `myts_id`, if it
/// came with a certificate. The service answers an unknown session as it
/// answers "nothing to show" (with nothing), so `None` tells nothing.
pub(crate) fn own_avatar(bytes: &[u8], myts_id: &[u8]) -> Result<Option<OwnAvatar>, Error> {
	let response: user::RequestContactsAvatarInfoResponse = wire::decode(bytes)?;
	if response.error_code != 0 {
		return Err(Error::Refused(response.error_code));
	}
	for map in profile::raw_fields(bytes, AVATAR_DATA).unwrap_or_default() {
		let Some(raw) = profile::raw_field(map, AVATAR_INFO) else { continue };
		let Ok(avatar) = wire::decode::<api::AvatarData>(raw) else { continue };
		let Some(contact) = avatar
			.optional
			.as_ref()
			.and_then(|any| wire::unpack_any::<api::OptionalAvatarDataContactInfo>(any).ok())
		else {
			continue;
		};
		if contact.mytsid == myts_id && !contact.user_cert.is_empty() {
			return Ok(Some(OwnAvatar { certificate: contact.user_cert, avatar: raw.to_vec() }));
		}
	}
	Ok(None)
}

/// The reply to `getSignedBadges`: the badges as they came. `None`: the
/// reply has no list (nothing learned).
pub(crate) fn signed_badges(bytes: &[u8]) -> Result<Option<Vec<SignedBadge>>, Error> {
	let response: user::UserBadgesSignedResponse = wire::decode(bytes)?;
	if response.error_code != 0 {
		return Err(Error::Refused(response.error_code));
	}
	let Some(list) = profile::raw_field(bytes, SIGNED_LIST) else { return Ok(None) };
	let badges = profile::raw_fields(list, SIGNED_BADGES).unwrap_or_default();
	Ok(Some(badges.into_iter().filter_map(SignedBadge::parse).collect()))
}

/// A certificate a TeamSpeak 6 server takes for an avatar or badges at
/// `now`: its chain from `root`, a leaf that signs for myTS IDs and is
/// valid then.
fn signing_certificate(bytes: &[u8], root: &[u8; 32], now: i64) -> Option<Certificate> {
	Certificate::parse(bytes, root).ok().filter(|c| c.signs_myts_data() && c.valid_at(now))
}

/// The message signed, as a server builds it: a part serialized again
/// (C++ `SerializeAsString`), then what follows. Serialized again by this
/// version, a part can differ from the bytes that came (fields this version
/// does not know), so those count too.
fn messages(raw: &[u8], encoded: Vec<u8>, tail: &[u8]) -> Vec<Vec<u8>> {
	let mut parts = vec![encoded];
	if parts[0] != raw {
		parts.push(raw.to_vec());
	}
	parts
		.into_iter()
		.map(|mut message| {
			message.extend_from_slice(tail);
			message
		})
		.collect()
}

/// Whether `certificate` verifies `avatar` (an `AvatarData`) for `myts_id`:
/// signed over the `AvatarInfo`, the myTS ID and the timestamp (8 bytes,
/// big-endian).
fn avatar_verifies(certificate: &Certificate, avatar: &[u8], myts_id: &[u8]) -> bool {
	let Ok(data) = wire::decode::<api::AvatarData>(avatar) else { return false };
	let mut tail = myts_id.to_vec();
	tail.extend_from_slice(&data.timestamp.to_be_bytes());
	let raw = profile::raw_field(avatar, INFO).unwrap_or_default();
	let encoded = wire::encode(&data.info.unwrap_or_default());
	messages(raw, encoded, &tail).iter().any(|message| certificate.verifies(message, &data.sign))
}

/// A badge id as a server reads it (16 bytes): only the 36-character form
/// with hyphens (Boost's `uuid` `operator>>`; anything else throws there).
/// `None` for one it would not read, which is never sent.
pub(crate) fn uuid_bytes(uuid: &str) -> Option<[u8; 16]> {
	let text = uuid.as_bytes();
	let hyphens = [8, 13, 18, 23];
	let well_formed = text.len() == 36
		&& text
			.iter()
			.enumerate()
			.all(|(at, c)| if hyphens.contains(&at) { *c == b'-' } else { c.is_ascii_hexdigit() });
	if !well_formed {
		return None;
	}
	let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
	let hex: Vec<u8> = text.iter().copied().filter(|c| *c != b'-').collect();
	let mut bytes = [0; 16];
	for (byte, pair) in bytes.iter_mut().zip(hex.chunks_exact(2)) {
		*byte = (digit(pair[0])? << 4) | digit(pair[1])?;
	}
	Some(bytes)
}

/// Whether `certificate` verifies `badge` for `myts_id`: signed over the
/// badge's id (16 bytes), its timestamp (8 bytes, big-endian) and the myTS
/// ID.
fn badge_verifies(certificate: &Certificate, badge: &SignedBadge, myts_id: &[u8]) -> bool {
	let Ok(signed) = wire::decode::<user::SignedUserBadge>(badge.raw.as_slice()) else {
		return false;
	};
	let Some(uuid) = signed.badge.as_ref().and_then(|b| uuid_bytes(&b.uuid)) else {
		return false;
	};
	let mut message = uuid.to_vec();
	message.extend_from_slice(&signed.sign_timestamp.to_be_bytes());
	message.extend_from_slice(myts_id);
	certificate.verifies(&message, &signed.sign)
}

/// Whether `avatar` (an `AvatarData`) has pictures: a server shows one
/// without any as no avatar.
pub fn avatar_has_pictures(avatar: &[u8]) -> bool {
	wire::decode::<api::AvatarData>(avatar)
		.is_ok_and(|data| data.info.is_some_and(|info| !info.map.is_empty()))
}

/// Which avatar a TeamSpeak 6 server shows for `myts_id` at `now` (Unix
/// seconds), and with which certificate: of `avatars` (`AvatarData`s as
/// they came), the newest (by its timestamp) that one of `certificates`
/// verifies. Indices into both.
pub fn choose_avatar(
	avatars: &[&[u8]],
	certificates: &[&[u8]],
	myts_id: &[u8],
	now: i64,
) -> Option<(usize, usize)> {
	choose_avatar_from(&tsproto::ROOT_KEY, avatars, certificates, myts_id, now)
}

/// [`choose_avatar`] with chains from `root` instead of TeamSpeak's (tests).
pub fn choose_avatar_from(
	root: &[u8; 32],
	avatars: &[&[u8]],
	certificates: &[&[u8]],
	myts_id: &[u8],
	now: i64,
) -> Option<(usize, usize)> {
	let certificates: Vec<_> =
		certificates.iter().map(|bytes| signing_certificate(bytes, root, now)).collect();
	let mut chosen = None;
	let mut newest = 0;
	for (index, avatar) in avatars.iter().enumerate() {
		let Ok(data) = wire::decode::<api::AvatarData>(*avatar) else { continue };
		if chosen.is_some() && data.timestamp <= newest {
			continue;
		}
		let verifying = certificates
			.iter()
			.position(|c| c.as_ref().is_some_and(|c| avatar_verifies(c, avatar, myts_id)));
		if let Some(certificate) = verifying {
			chosen = Some((index, certificate));
			newest = data.timestamp;
		}
	}
	chosen
}

/// Which of `certificates` a TeamSpeak 6 server verifies every one of
/// `badges` with for `myts_id` at `now` (Unix seconds); an index.
pub fn choose_badges_certificate(
	badges: &[&SignedBadge],
	certificates: &[&[u8]],
	myts_id: &[u8],
	now: i64,
) -> Option<usize> {
	choose_badges_certificate_from(&tsproto::ROOT_KEY, badges, certificates, myts_id, now)
}

/// [`choose_badges_certificate`] with chains from `root` instead of
/// TeamSpeak's (tests).
pub fn choose_badges_certificate_from(
	root: &[u8; 32],
	badges: &[&SignedBadge],
	certificates: &[&[u8]],
	myts_id: &[u8],
	now: i64,
) -> Option<usize> {
	certificates.iter().position(|bytes| {
		signing_certificate(bytes, root, now)
			.is_some_and(|c| badges.iter().all(|badge| badge_verifies(&c, badge, myts_id)))
	})
}

#[cfg(test)]
pub(crate) mod tests {
	use super::*;
	use crate::testing::{Chain, NOW};

	pub(crate) const ID: [u8; 33] = [4; 33];
	const BADGE_A: &str = "a2a2a2a2-0000-4000-8000-000000000001";
	const BADGE_B: &str = "B3B3B3B3-0000-4000-8000-0000000000B2";

	fn badge(raw: &[u8]) -> SignedBadge {
		SignedBadge::parse(raw).unwrap()
	}

	#[test]
	fn the_avatar_goes_with_a_certificate_that_verifies_it() {
		let (login, service) = (Chain::signing(1), Chain::signing(2));
		let old = login.avatar(&ID, 100);
		let new = service.avatar(&ID, 200);
		let certificates = [login.certificate.as_slice(), service.certificate.as_slice()];
		let choose = |avatars: &[&[u8]], certificates: &[&[u8]], id: &[u8], now| {
			choose_avatar_from(&login.root, avatars, certificates, id, now)
		};
		// The newest that verifies, each with its own certificate.
		assert_eq!(choose(&[&old, &new], &certificates, &ID, NOW), Some((1, 1)));
		assert_eq!(choose(&[&new, &old], &certificates, &ID, NOW), Some((0, 1)));
		assert_eq!(choose(&[&old], &certificates, &ID, NOW), Some((0, 0)));
		assert!(avatar_has_pictures(&old));
		assert!(!avatar_has_pictures(&wire::encode(&api::AvatarData::default())));
		// Without the newer one's certificate: the older one.
		assert_eq!(choose(&[&old, &new], &certificates[..1], &ID, NOW), Some((0, 0)));
		// Another myTS ID, another time, another root: nothing.
		assert_eq!(choose(&[&old, &new], &certificates, &[5; 33], NOW), None);
		assert_eq!(choose(&[&old], &certificates, &ID, NOW + 31 * 86_400), None);
		assert_eq!(choose(&[&old], &certificates, &ID, NOW - 7200), None);
		assert_eq!(choose_avatar(&[&old], &certificates, &ID, NOW), None);
		// A field this version does not know, after the signed part: still
		// the same avatar.
		let mut unknown = old.clone();
		unknown.extend_from_slice(&[0x78, 0x01]);
		assert_eq!(choose(&[&unknown], &certificates, &ID, NOW), Some((0, 0)));
		// Changed after signing.
		let mut changed: api::AvatarData = wire::decode(old.as_slice()).unwrap();
		changed.timestamp = 101;
		assert_eq!(choose(&[&wire::encode(&changed)], &certificates, &ID, NOW), None);
		// Only a key that signs for myTS IDs.
		let other = Chain::new(1, 5, NOW - 3600, NOW + 3600);
		assert_eq!(choose(&[&old], &[&other.certificate], &ID, NOW), None);
		assert_eq!(choose(&[b"\xff"], &certificates, &ID, NOW), None);
	}

	#[test]
	fn badges_go_with_a_certificate_that_verifies_all_of_them() {
		let (one, two) = (Chain::signing(1), Chain::signing(2));
		let a = badge(&one.badge(&ID, BADGE_A, "A"));
		let b = badge(&two.badge(&ID, BADGE_B, "B"));
		let certificates = [one.certificate.as_slice(), two.certificate.as_slice()];
		let choose = |badges: &[&SignedBadge], id: &[u8]| {
			choose_badges_certificate_from(&one.root, badges, &certificates, id, NOW)
		};
		assert_eq!(choose(&[&a], &ID), Some(0));
		assert_eq!(choose(&[&b], &ID), Some(1));
		assert_eq!(choose(&[&a, &b], &ID), None, "no one certificate verifies both");
		assert_eq!(choose(&[&a], &[5; 33]), None);
		let both = badge(&two.badge(&ID, BADGE_A, "A"));
		assert_eq!(choose(&[&both, &b], &ID), Some(1));
		// An id a server would not read is never sent, though the signature
		// is over the same 16 bytes.
		for odd in ["{a2a2a2a2-0000-4000-8000-000000000001}", "a2a2a2a2000040008000000000000001"] {
			let mut signed: user::SignedUserBadge = wire::decode(a.raw()).unwrap();
			signed.badge.as_mut().unwrap().uuid = odd.into();
			assert_eq!(choose(&[&badge(&wire::encode(&signed))], &ID), None, "{odd}");
		}
	}

	#[test]
	fn badge_ids_are_read_as_a_server_reads_them() {
		assert_eq!(uuid_bytes(BADGE_A).unwrap()[..4], [0xa2; 4]);
		assert_eq!(uuid_bytes(BADGE_B).unwrap()[15], 0xb2);
		for bad in [
			"",
			"a2a2a2a2",
			"a2a2a2a2-0000-4000-8000-00000000000g",
			"a2a2a2a2+0000-4000-8000-000000000001",
			// Without hyphens, braced, or a sign where a digit belongs: the
			// server throws on them.
			"a2a2a2a2000040008000000000000001",
			"{a2a2a2a2-0000-4000-8000-000000000001}",
			"a2a2a2a2-0000-4000-8000-0000000000+1",
			"+2a2a2a2-0000-4000-8000-000000000001",
			"a2a2a2a2-0000-4000-8000-0000000000\u{e9}",
			" 2a2a2a2-0000-4000-8000-000000000001",
		] {
			assert_eq!(uuid_bytes(bad), None, "{bad}");
		}
	}

	#[test]
	fn the_badge_list_keeps_each_badge_as_it_came() {
		let chain = Chain::signing(1);
		let a = chain.badge(&ID, BADGE_A, "A");
		let b = chain.badge(&ID, BADGE_B, "B");
		let (a_badge, b_badge) = (badge(&a), badge(&b));
		let decoded = |raw: &[u8]| wire::decode::<user::SignedUserBadge>(raw).unwrap();
		assert_eq!(
			badge_list([&b_badge, &a_badge]),
			wire::encode(&user::UserBadgesSignedList { badges: vec![decoded(&b), decoded(&a)] })
		);
		// A field this version does not know stays.
		let mut raw = a.clone();
		raw.extend_from_slice(&[0x78, 0x01]);
		let odd = SignedBadge::parse(&raw).unwrap();
		assert_eq!(odd.raw(), raw);
		let list = badge_list([&odd]);
		assert_eq!(profile::raw_fields(&list, 1).unwrap(), [raw.as_slice()]);
		// Long ones get a longer length.
		let long = SignedBadge { raw: vec![0; 300], ..odd };
		assert_eq!(&badge_list([&long])[..3], [0x0a, 0xac, 0x02]);
		assert!(badge_list([]).is_empty());
	}
}
