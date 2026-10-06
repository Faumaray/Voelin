//! Voelin patch: connection-bound myTeamSpeak proof and live account changes.

use base64::prelude::*;
use tsproto::myts::Identity;
use tsproto_packets::packets::{Direction, Flags, OutCommand, PacketType};

use crate::{ConnectOptions, Connection, ConnectionState, Error, MessageHandle, Result};

impl ConnectOptions {
	/// Snapshot the account credential for the initial connection and reconnects.
	/// This is separate from the server identity and account profile UUID.
	pub fn myts_identity(mut self, identity: Option<Identity>) -> Self {
		self.myts_identity = identity;
		self
	}
}

/// What a voice server is shown of the myTeamSpeak account (TeamSpeak 6)
/// besides its myTS ID, as myTeamSpeak signed it for that ID: its avatar,
/// its badges and its User Tag. The server checks the avatar and the badges
/// with the one certificate of each `updatemytsdata` and shows nothing of
/// what does not verify, so each comes with the certificate that verifies
/// it. Viewers check the User Tag's token themselves.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct MytsData {
	/// The raw myTS ID all of it is signed for: shown only on a connection
	/// that presents it.
	pub myts_id: Vec<u8>,
	/// An `AvatarData` as it came (`client_myteamspeak_avatar`).
	pub avatar: Signed,
	/// A `UserBadgesSignedList`, the badges chosen to show, in order
	/// (`client_signed_badges`).
	pub badges: Signed,
	/// The ids of those badges, as in the list: the server publishes the
	/// ones that verify.
	pub badge_ids: Vec<String>,
	pub user_tag: Option<UserTag>,
}

/// Something signed and the certificate that verifies it; empty: nothing.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Signed {
	pub certificate: Vec<u8>,
	pub data: Vec<u8>,
}

/// The User Tag (`client_user_tag`): the account's primary chat identifier
/// and the token that proves it (a `MatrixIdentifierToken` as it came).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct UserTag {
	pub tag: String,
	pub token: Vec<u8>,
}

impl std::fmt::Debug for MytsData {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("MytsData")
			.field("avatar", &self.avatar.data.len())
			.field("badges", &self.badge_ids)
			.field("user_tag", &self.user_tag.as_ref().map(|t| &t.tag))
			.finish()
	}
}

impl Signed {
	pub fn is_empty(&self) -> bool { self.data.is_empty() }
}

impl UserTag {
	/// `client_user_tag` as the official client sets it: compact JSON with
	/// the token (standard base64), the tag and when it was set (`updated`,
	/// milliseconds since 1970), in this order.
	pub fn value(&self, updated_ms: i64) -> String {
		let mut tag = String::with_capacity(self.tag.len());
		for c in self.tag.chars() {
			match c {
				'"' => tag.push_str("\\\""),
				'\\' => tag.push_str("\\\\"),
				'\n' => tag.push_str("\\n"),
				'\r' => tag.push_str("\\r"),
				'\t' => tag.push_str("\\t"),
				'\u{8}' => tag.push_str("\\b"),
				'\u{c}' => tag.push_str("\\f"),
				c if u32::from(c) < 0x20 => tag.push_str(&format!("\\u{:04x}", u32::from(c))),
				c => tag.push(c),
			}
		}
		format!(
			"{{\"myts_token\":\"{}\",\"tag\":\"{}\",\"updated\":{}}}",
			BASE64_STANDARD.encode(&self.token),
			tag,
			updated_ms
		)
	}
}

/// One `updatemytsdata`: a value is shown (checked with `certificate`), an
/// empty one clears what is shown, `None` leaves it as it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MytsUpdate {
	pub certificate: Vec<u8>,
	pub badges: Option<Vec<u8>>,
	pub avatar: Option<Vec<u8>>,
}

/// What to do about one part of what is shown.
enum Part<'a> {
	Keep,
	Show(&'a Signed),
	Clear,
}

fn part<'a>(wanted: &'a Signed, shown: &Signed, again: bool) -> Part<'a> {
	if wanted.is_empty() {
		if shown.is_empty() { Part::Keep } else { Part::Clear }
	} else if again || wanted != shown {
		Part::Show(wanted)
	} else {
		Part::Keep
	}
}

impl MytsData {
	/// The `updatemytsdata` commands that make a server that shows `shown`
	/// show `self`. `again`: everything once more, after a new account proof
	/// (`updatemytsid`), which the server does not check them for again.
	/// The avatar and the badges go together when one certificate verifies
	/// both (certificate, badges, avatar, as the official client sends
	/// them), apart otherwise; clearing needs no certificate.
	pub fn updates(&self, shown: &MytsData, again: bool) -> Vec<MytsUpdate> {
		let avatar = part(&self.avatar, &shown.avatar, again);
		let badges = part(&self.badges, &shown.badges, again);
		let mut updates = Vec::new();
		match (&badges, &avatar) {
			(Part::Show(badges), Part::Show(avatar)) if badges.certificate == avatar.certificate => {
				updates.push(MytsUpdate {
					certificate: avatar.certificate.clone(),
					badges: Some(badges.data.clone()),
					avatar: Some(avatar.data.clone()),
				});
			}
			_ => {
				if let Part::Show(avatar) = avatar {
					updates.push(MytsUpdate {
						certificate: avatar.certificate.clone(),
						avatar: Some(avatar.data.clone()),
						..Default::default()
					});
				}
				if let Part::Show(badges) = badges {
					updates.push(MytsUpdate {
						certificate: badges.certificate.clone(),
						badges: Some(badges.data.clone()),
						..Default::default()
					});
				}
			}
		}
		let clear_badges = matches!(badges, Part::Clear);
		let clear_avatar = matches!(avatar, Part::Clear);
		if clear_badges || clear_avatar {
			updates.push(MytsUpdate {
				certificate: Vec::new(),
				badges: if clear_badges { Some(Vec::new()) } else { None },
				avatar: if clear_avatar { Some(Vec::new()) } else { None },
			});
		}
		updates
	}
}

impl Connection {
	/// Show the server the account's avatar or badges, or clear them
	/// (`updatemytsdata`), as the official client does once connected and
	/// after a sign-in, after the account proof (`updatemytsid`). The server
	/// answers before the result: the own client's
	/// `client_myteamspeak_avatar` and `client_signed_badges`, empty for what
	/// did not verify. Await the returned handle's
	/// `StreamItem::MessageResult` for the answer.
	pub fn send_myts_update(&mut self, update: &MytsUpdate) -> Result<MessageHandle> {
		if !matches!(self.state, ConnectionState::Connected { .. }) {
			return Err(Error::NotConnected);
		}
		self.send_command_with_result(data_packet(update))
	}

	/// Set the User Tag (`clientupdate client_user_tag`), as built by
	/// [`UserTag::value`]; empty clears it. TeamSpeak 6 servers only.
	pub fn send_user_tag(&mut self, value: &str) -> Result<MessageHandle> {
		if !matches!(self.state, ConnectionState::Connected { .. }) {
			return Err(Error::NotConnected);
		}
		self.send_command_with_result(user_tag_packet(value))
	}

	/// Publish a connection-bound account proof, or clear the account identity.
	///
	/// Always updates the reconnect options, even if currently disconnected or
	/// sending fails. A running connection attempt owns its previous snapshot:
	/// callers must cancel and rebuild that attempt after `NotConnected`.
	/// Await the returned handle's `StreamItem::MessageResult` for server acceptance.
	pub fn update_myts_identity(&mut self, identity: Option<Identity>) -> Result<MessageHandle> {
		self.options.myts_identity = identity;
		let iv = match &self.state {
			ConnectionState::Connected { con, .. } => {
				con.client.params.as_ref().ok_or(Error::InitserverParamsMissing)?.shared_iv
			}
			_ => return Err(Error::NotConnected),
		};
		let packet = update_packet(self.options.myts_identity.as_ref(), &iv);
		self.send_command_with_result(packet)
	}
}

/// The values are the raw bytes with TeamSpeak's escaping only (no
/// base64), as the official client sends them; an empty one is the bare
/// name, which clears.
fn data_packet(update: &MytsUpdate) -> OutCommand {
	let mut packet =
		OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, "updatemytsdata");
	packet.write_bin_arg("myts_certificate", &update.certificate);
	if let Some(badges) = &update.badges {
		packet.write_bin_arg("myts_signed_badge", badges);
	}
	if let Some(avatar) = &update.avatar {
		packet.write_bin_arg("myts_avatar", avatar);
	}
	packet
}

fn user_tag_packet(value: &str) -> OutCommand {
	let mut packet =
		OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, "clientupdate");
	packet.write_arg("client_user_tag", &value);
	packet
}

fn update_packet(identity: Option<&Identity>, iv: &[u8; 64]) -> OutCommand {
	let mut packet =
		OutCommand::new(Direction::C2S, Flags::empty(), PacketType::Command, "updatemytsid");
	if let Some(identity) = identity {
		identity.proof(iv).write_to(&mut packet);
	} else {
		// Empty arguments are serialized without '=' by the existing writer.
		packet.write_arg("myTeamspeakId", &"");
	}
	packet
}

#[cfg(test)]
mod tests {
	use super::*;

	fn signed(certificate: &[u8], data: &[u8]) -> Signed {
		Signed { certificate: certificate.to_vec(), data: data.to_vec() }
	}

	fn wire(update: &MytsUpdate) -> Vec<u8> { data_packet(update).into_packet().content().to_vec() }

	#[test]
	fn account_data_goes_raw_and_escaped() {
		let update = MytsUpdate {
			certificate: b"a b/c|d\\\x07\x08\x00\xff".to_vec(),
			badges: Some(vec![0x0a, 0x01, b' ']),
			avatar: Some(vec![0x0a, 0x02, b'h', b'i']),
		};
		assert_eq!(
			wire(&update),
			b"updatemytsdata myts_certificate=a\\sb\\/c\\pd\\\\\\a\\b\x00\xff \
			  myts_signed_badge=\\n\x01\\s myts_avatar=\\n\x02hi"
		);
		// Without badges: the certificate and the avatar.
		let update = MytsUpdate { badges: None, ..update };
		assert!(!wire(&update).windows(17).any(|w| w == b"myts_signed_badge"));
		// Clearing: bare names, no certificate needed.
		let clear = MytsUpdate {
			certificate: Vec::new(),
			badges: Some(Vec::new()),
			avatar: Some(Vec::new()),
		};
		assert_eq!(wire(&clear), b"updatemytsdata myts_certificate myts_signed_badge myts_avatar");
	}

	#[test]
	fn what_one_certificate_verifies_goes_together() {
		let data = MytsData {
			myts_id: vec![1; 33],
			avatar: signed(b"C", b"avatar"),
			badges: signed(b"C", b"badges"),
			badge_ids: vec!["b1".into()],
			user_tag: None,
		};
		let none = MytsData::default();
		assert_eq!(data.updates(&none, false), [MytsUpdate {
			certificate: b"C".to_vec(),
			badges: Some(b"badges".to_vec()),
			avatar: Some(b"avatar".to_vec()),
		}]);
		assert_eq!(
			wire(&data.updates(&none, false)[0]),
			b"updatemytsdata myts_certificate=C myts_signed_badge=badges myts_avatar=avatar"
		);
		// Unchanged: nothing, unless after a new account proof.
		assert!(data.updates(&data, false).is_empty());
		assert_eq!(data.updates(&data, true).len(), 1);
		// Two certificates: two commands, each with its own.
		let apart = MytsData { badges: signed(b"D", b"badges"), ..data.clone() };
		let updates: Vec<_> = apart.updates(&none, false).iter().map(wire).collect();
		assert_eq!(updates, [
			b"updatemytsdata myts_certificate=C myts_avatar=avatar".to_vec(),
			b"updatemytsdata myts_certificate=D myts_signed_badge=badges".to_vec(),
		]);
		// Only what changed.
		let newer = MytsData { avatar: signed(b"C", b"newer"), ..data.clone() };
		let updates: Vec<_> = newer.updates(&data, false).iter().map(wire).collect();
		assert_eq!(updates, [b"updatemytsdata myts_certificate=C myts_avatar=newer".to_vec()]);
		// What is no longer there is cleared; what never was is left alone.
		let fewer = MytsData { badges: Signed::default(), ..data.clone() };
		let updates: Vec<_> = fewer.updates(&data, true).iter().map(wire).collect();
		assert_eq!(updates, [
			b"updatemytsdata myts_certificate=C myts_avatar=avatar".to_vec(),
			b"updatemytsdata myts_certificate myts_signed_badge".to_vec(),
		]);
		let updates: Vec<_> = none.updates(&data, true).iter().map(wire).collect();
		assert_eq!(updates, [b"updatemytsdata myts_certificate myts_signed_badge myts_avatar".to_vec()]);
		assert!(none.updates(&none, true).is_empty());
	}

	#[test]
	fn the_user_tag_is_set_as_the_official_client_sets_it() {
		let tag = UserTag { tag: "alex@myteamspeak.com".into(), token: vec![0xfb, 0xff, 0x01, 0x02] };
		assert_eq!(
			tag.value(1_759_740_000_123),
			r#"{"myts_token":"+/8BAg==","tag":"alex@myteamspeak.com","updated":1759740000123}"#
		);
		let odd = UserTag { tag: "a\"b\\c\n\u{1}é".into(), token: Vec::new() };
		assert_eq!(odd.value(1), r#"{"myts_token":"","tag":"a\"b\\c\n\u0001é","updated":1}"#);
		// TeamSpeak's escaping on the wire; empty clears.
		let packet = user_tag_packet(&tag.value(5)).into_packet();
		assert_eq!(
			packet.content(),
			&br#"clientupdate client_user_tag={"myts_token":"+\/8BAg==","tag":"alex@myteamspeak.com","updated":5}"#[..]
		);
		assert_eq!(user_tag_packet("").into_packet().content(), b"clientupdate client_user_tag");
	}

	#[test]
	fn clear_contains_no_previous_account_proof() {
		let packet = update_packet(None, &[0; 64]).into_packet();
		assert_eq!(packet.content(), b"updatemytsid myTeamspeakId");
	}

	#[test]
	fn update_uses_current_connection_challenge() {
		// Scalar one and the standard compressed Edwards base point.
		let mut private = [0; 32];
		private[0] = 1;
		let mut public = [0x66; 32];
		public[0] = 0x58;
		let identity = Identity::new(vec![1; 33], 42, public, private, vec![2; 65]).unwrap();
		let first = update_packet(Some(&identity), &[0; 64]).into_packet();
		let second = update_packet(Some(&identity), &[1; 64]).into_packet();
		assert_ne!(first.content(), second.content());
		let wire = std::str::from_utf8(first.content()).unwrap();
		assert!(wire.starts_with("updatemytsid myTeamspeakId="));
		assert!(wire.contains(" acTime=42 "));
		assert!(!wire.contains("client_myteamspeak_id"));
		assert_eq!(wire.split(' ').count(), 7);
	}

	#[test]
	fn failed_update_still_replaces_reconnect_snapshot() {
		// Building opens no socket until the connection future is polled.
		let mut private = [0; 32];
		private[0] = 1;
		let mut public = [0x66; 32];
		public[0] = 0x58;
		let identity = Identity::new(vec![1; 33], 42, public, private, vec![2; 65]).unwrap();
		let mut connection =
			Connection::build("localhost").myts_identity(Some(identity)).connect().unwrap();
		assert!(connection.options.myts_identity.is_some());
		assert!(matches!(connection.update_myts_identity(None), Err(Error::NotConnected)));
		assert!(connection.options.myts_identity.is_none());
	}
}
