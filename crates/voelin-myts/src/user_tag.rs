//! The account's User Tag (TeamSpeak 6): its primary TeamSpeak chat
//! identifier ("name@myteamspeak.com"), and the token that proves it is
//! the account's, from the chat service (`/tschat`). Viewers check the
//! token against the myTS ID the account shows the server; nothing else
//! of the account is needed (no chat sign-in).

use crate::{Error, profile};
use schema_api::api::{tschat, user};
use schema_api::{SessionToken, wire};
use tsproto::myts::Certificate;

/// `ChatRequestReturnCode`: SUCCESS and SESSION_EXPIRED.
const SUCCESS: i32 = 1;
const SESSION_EXPIRED: i32 = 5;
/// `SignedAllowedIdentifier.token`, `MatrixIdentifierToken.tags`.
const TOKEN: u32 = 1;
const TAGS: u32 = 4;

/// Why a User Tag's token is not shown: viewers would not accept it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UserTagError {
	#[error("the User Tag's token does not parse")]
	Format,
	#[error("the User Tag's certificate is not valid")]
	Certificate,
	#[error("the User Tag's token is not signed for the account's myTS ID")]
	Signature,
	#[error("the User Tag's token does not name the tag")]
	Tag,
}

/// The chat service's requests carry the session only (no chat account).
pub(crate) fn request(token: &SessionToken) -> tschat::AuthenticatedUser {
	tschat::AuthenticatedUser { session: token.as_str().into(), ..Default::default() }
}

/// A chat service answer: SUCCESS, or why not. An expired session is the
/// same refusal as the account service's.
fn check(error: Option<&tschat::ErrorHandling>, required: bool) -> Result<(), Error> {
	match error.map(|e| e.return_code) {
		Some(SUCCESS) => Ok(()),
		None if !required => Ok(()),
		Some(SESSION_EXPIRED) => Err(Error::Refused(profile::SESSION_EXPIRED)),
		code => Err(Error::ChatRefused(code.unwrap_or_default())),
	}
}

/// The primary identifier of `getActiveIdentifierList`'s answer; `None`:
/// the account has none.
pub(crate) fn primary(list: tschat::TschatIdentifierList) -> Result<Option<String>, Error> {
	check(list.error_handling.as_ref(), false)?;
	Ok(primary_of(&list))
}

fn primary_of(list: &tschat::TschatIdentifierList) -> Option<String> {
	list.ts_chat_identifier_mapping
		.iter()
		.find(|m| m.primary && !m.ts_chat_identifier.is_empty())
		.map(|m| m.ts_chat_identifier.clone())
}

/// The account data's active identifiers (`getAccountData`), for when the
/// chat service names no primary one.
pub(crate) fn account_request(token: &SessionToken) -> user::AccountDataRequest {
	user::AccountDataRequest {
		session: token.as_str().into(),
		selector: vec![user::UserAccountDataSelector::TsChat as i32],
		..Default::default()
	}
}

pub(crate) fn account_primary(data: &user::UserAccountData) -> Option<String> {
	data.ts_chat_identifier_list_active.as_ref().and_then(primary_of)
}

/// The token of `requestSignedAllowedIdentifierList`'s answer, exactly as
/// it came: viewers check its signature over parts of it.
pub(crate) fn token(bytes: &[u8]) -> Result<Vec<u8>, Error> {
	let answer: tschat::SignedAllowedIdentifier = wire::decode(bytes)?;
	check(answer.error.as_ref(), true)?;
	match profile::raw_field(bytes, TOKEN) {
		Some(token) if !token.is_empty() => Ok(token.to_vec()),
		_ => Err(Error::NoToken),
	}
}

/// Check a User Tag's token as viewers check it (the official client's
/// `verifyIdentifierToken`): its certificate's chain and leaf valid at
/// `at` (Unix seconds), its signature over the tags, its timestamp (8
/// bytes, big-endian) and the myTS ID, and the tag among the tags. Until
/// when its certificate is valid, Unix seconds.
pub fn check_user_tag(
	token: &[u8],
	tag: &str,
	myts_id: &[u8],
	at: i64,
) -> Result<i64, UserTagError> {
	check_user_tag_from(&tsproto::ROOT_KEY, token, tag, myts_id, at)
}

/// [`check_user_tag`] with chains from `root` instead of TeamSpeak's
/// (tests).
pub fn check_user_tag_from(
	root: &[u8; 32],
	token: &[u8],
	tag: &str,
	myts_id: &[u8],
	at: i64,
) -> Result<i64, UserTagError> {
	let decoded: tschat::MatrixIdentifierToken =
		wire::decode(token).map_err(|_| UserTagError::Format)?;
	let certificate = Certificate::parse(&decoded.sign_certificate, root)
		.ok()
		.filter(|c| c.valid_at(at))
		.ok_or(UserTagError::Certificate)?;
	let tags = decoded.tags.unwrap_or_default();
	let raw = profile::raw_field(token, TAGS).unwrap_or_default();
	let mut tail = decoded.timestamp.to_be_bytes().to_vec();
	tail.extend_from_slice(myts_id);
	let mut parts = vec![wire::encode(&tags)];
	if parts[0] != raw {
		parts.push(raw.to_vec());
	}
	let signed = parts.into_iter().any(|mut message| {
		message.extend_from_slice(&tail);
		certificate.verifies(&message, &decoded.signature)
	});
	if !signed {
		return Err(UserTagError::Signature);
	}
	if !tags.tag.iter().any(|t| t == tag) {
		return Err(UserTagError::Tag);
	}
	Ok(certificate.not_valid_after())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::presentation::tests::ID;
	use crate::testing::{Chain, NOW};

	#[test]
	fn the_token_is_checked_as_viewers_check_it() {
		let chain = Chain::new(3, 4, NOW - 3600, NOW + 86_400);
		let tag = "alex@myteamspeak.com";
		let good = chain.user_tag_token(&["alex@tschat-1.teamspeak.com", tag], &ID);
		let check = |token: &[u8], tag: &str, id: &[u8], at| {
			check_user_tag_from(&chain.root, token, tag, id, at)
		};
		// Any leaf: viewers do not ask which kind.
		assert_eq!(check(&good, tag, &ID, NOW), Ok(NOW + 86_400));
		assert_eq!(check(&good, "alex@tschat-1.teamspeak.com", &ID, NOW), Ok(NOW + 86_400));
		assert_eq!(check(&good, "bob@myteamspeak.com", &ID, NOW), Err(UserTagError::Tag));
		assert_eq!(check(&good, tag, &[5; 33], NOW), Err(UserTagError::Signature));
		assert_eq!(check(&good, tag, &ID, NOW + 86_400), Err(UserTagError::Certificate));
		assert_eq!(check(&good, tag, &ID, NOW - 7200), Err(UserTagError::Certificate));
		// TeamSpeak's root: another key.
		assert_eq!(check_user_tag(&good, tag, &ID, NOW), Err(UserTagError::Signature));
		assert_eq!(check(b"\xff", tag, &ID, NOW), Err(UserTagError::Format));
		let mut decoded: tschat::MatrixIdentifierToken = wire::decode(good.as_slice()).unwrap();
		decoded.timestamp += 1;
		assert_eq!(check(&wire::encode(&decoded), tag, &ID, NOW), Err(UserTagError::Signature));
	}

	#[test]
	fn answers_are_required_to_succeed() {
		let ok = |code| Some(tschat::ErrorHandling { return_code: code, message: "m".into() });
		let list = |error| tschat::TschatIdentifierList {
			ts_chat_identifier_mapping: vec![
				tschat::TschatIdentifierMapping {
					ts_chat_identifier: "old@tschat-1.teamspeak.com".into(),
					..Default::default()
				},
				tschat::TschatIdentifierMapping {
					ts_chat_identifier: "alex@myteamspeak.com".into(),
					primary: true,
					..Default::default()
				},
			],
			error_handling: error,
		};
		assert_eq!(primary(list(ok(1))).unwrap().as_deref(), Some("alex@myteamspeak.com"));
		assert_eq!(primary(list(None)).unwrap().as_deref(), Some("alex@myteamspeak.com"));
		assert!(primary(list(ok(5))).unwrap_err().is_invalid_session());
		assert!(matches!(primary(list(ok(3))), Err(Error::ChatRefused(3))));
		assert_eq!(primary(tschat::TschatIdentifierList::default()).unwrap(), None);
		// The token's answer must say SUCCESS.
		let answer = |error| {
			wire::encode(&tschat::SignedAllowedIdentifier {
				token: Some(tschat::MatrixIdentifierToken {
					signature: vec![1; 64],
					..Default::default()
				}),
				error,
			})
		};
		assert!(super::token(&answer(ok(1))).is_ok());
		assert!(matches!(super::token(&answer(None)), Err(Error::ChatRefused(0))));
		assert!(matches!(super::token(&answer(ok(4))), Err(Error::ChatRefused(4))));
		// What the live service answers an unknown session.
		let mut live = vec![0x12, 0x44, 0x08, 0x05, 0x12, 0x40];
		live.extend_from_slice(&[b'x'; 0x40]);
		assert!(super::token(&live).unwrap_err().is_invalid_session());
		let none = wire::encode(&tschat::SignedAllowedIdentifier { token: None, error: ok(1) });
		assert!(matches!(super::token(&none), Err(Error::NoToken)));
	}
}
