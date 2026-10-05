//! The account's profile: what `getAccountData` and the login response tell
//! about the signed-in user, and the avatar's download link.

use crate::Error;
use schema_api::SessionToken;
use schema_api::api::{self, user};

/// `LoginSession.user_avatar`, the account's avatar as myTeamSpeak signed
/// it.
pub(crate) const USER_AVATAR: u32 = 11;

/// What voice servers show of the account (TeamSpeak 6): sent to each
/// server once connected (`updatemytsdata`), as the official client does.
/// Public data, as myTeamSpeak signed it; empty without it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Presentation {
	/// The account's certificate (`LoginSession.mytsid_user_cert.cert`).
	pub certificate: Vec<u8>,
	/// The avatar (`LoginSession.user_avatar`, an `AvatarData`: its
	/// pictures' links, a timestamp and myTeamSpeak's signature), the bytes
	/// exactly as they came: the signature covers them, and fields this
	/// version does not know would be lost by encoding them again.
	pub avatar: Vec<u8>,
}

impl Presentation {
	/// Whether there is anything to show (a certificate, without which a
	/// server takes nothing).
	pub fn is_empty(&self) -> bool {
		self.certificate.is_empty()
	}
}

impl std::fmt::Debug for Presentation {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Presentation")
			.field("certificate", &self.certificate.len())
			.field("avatar", &self.avatar.len())
			.finish()
	}
}

/// The bytes of the last length-delimited field `number` at the top level
/// of a protobuf message, as they are; `None` without one or for a message
/// that does not parse.
pub(crate) fn raw_field(message: &[u8], number: u32) -> Option<&[u8]> {
	fn varint(data: &[u8], at: &mut usize) -> Option<u64> {
		let mut value = 0u64;
		for shift in (0..64).step_by(7) {
			let byte = *data.get(*at)?;
			*at += 1;
			value |= u64::from(byte & 0x7f) << shift;
			if byte & 0x80 == 0 {
				return Some(value);
			}
		}
		None
	}
	let mut at = 0;
	let mut found = None;
	while at < message.len() {
		let key = varint(message, &mut at)?;
		let skip = match key & 7 {
			0 => {
				varint(message, &mut at)?;
				0
			}
			1 => 8,
			2 => usize::try_from(varint(message, &mut at)?).ok()?,
			5 => 4,
			// Groups are not used by myTeamSpeak.
			_ => return None,
		};
		let end = at.checked_add(skip).filter(|end| *end <= message.len())?;
		if key & 7 == 2 && key >> 3 == u64::from(number) {
			found = Some(&message[at..end]);
		}
		at = end;
	}
	found
}

/// `ERROR_SESSION_EXPIRED` of the `/user` service.
pub(crate) const SESSION_EXPIRED: i32 = user::ErrorReturnCode::ErrorSessionExpired as i32;
/// Avatars are small pictures; anything larger is not one.
pub(crate) const MAX_AVATAR_BYTES: usize = 4 << 20;

/// The signed-in user's account, as the account service has it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
	pub username: String,
	pub email: String,
	pub description: String,
	/// When the account was registered, Unix seconds (0: not told).
	pub registered: i64,
	/// The last sign-in before this one, Unix seconds (0: not told).
	pub last_login: i64,
	pub badges: Vec<Badge>,
	/// The avatar's file names, the one for "online" first: each is the
	/// picture's download link.
	pub avatars: Vec<String>,
	/// Devices signed in to the account.
	pub devices: Vec<Device>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Badge {
	pub name: String,
	pub description: String,
	/// The badge's picture.
	pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Device {
	pub name: String,
	/// Unix seconds (0: not told).
	pub last_login: i64,
	pub region: String,
}

impl Profile {
	/// What a login response carries: the avatar and the description.
	pub(crate) fn from_login(response: &api::LoginSession) -> Self {
		Self {
			username: response.username.clone(),
			description: response
				.user_data
				.as_ref()
				.map(|d| d.description.clone())
				.unwrap_or_default(),
			avatars: avatar_files(response.user_avatar.as_ref().and_then(|a| a.info.as_ref())),
			..Default::default()
		}
	}
}

/// `getAccountData` for what the profile shows.
pub(crate) fn request(token: &SessionToken) -> user::AccountDataRequest {
	use user::UserAccountDataSelector as S;
	user::AccountDataRequest {
		session: token.as_str().into(),
		selector: [S::BasicInfo, S::Avatars, S::Description, S::Badges, S::AuthenticatedDevices]
			.map(|s| s as i32)
			.into(),
		..Default::default()
	}
}

pub(crate) fn from_account_data(data: user::UserAccountData) -> Result<Profile, Error> {
	check(data.return_code.as_ref())?;
	Ok(Profile {
		username: data.username,
		email: data.email_address,
		description: data.description,
		registered: unix_seconds(data.registration_date),
		last_login: unix_seconds(data.last_login),
		badges: data
			.badges
			.into_iter()
			.map(|b| Badge { name: b.name, description: b.description, url: b.url })
			.collect(),
		avatars: avatar_files(data.avatar_info.as_ref()),
		devices: data
			.authenticated_devices
			.into_iter()
			.map(|d| Device {
				name: d.device_name,
				last_login: unix_seconds(d.last_login),
				region: d.region,
			})
			.collect(),
	})
}

/// An avatar file name as a download link: the official client
/// (`Avatar_Cache::request_avatar_from_urls`) GETs the names of the avatar
/// map as they are. Only HTTPS (and loopback HTTP in tests).
pub(crate) fn avatar_link(name: &str) -> Result<&str, Error> {
	let allowed =
		name.starts_with("https://") || (cfg!(test) && name.starts_with("http://127.0.0.1:"));
	if !allowed || name.len() > 4096 {
		return Err(Error::AvatarUrl);
	}
	Ok(name)
}

/// A refusal: `success` unset with an error code. (A reply without a return
/// code is taken as it is: the service sets one when it refuses.)
fn check(code: Option<&user::ReturnCode>) -> Result<(), Error> {
	match code {
		Some(code) if !code.success && code.error_code != 0 => Err(Error::Refused(code.error_code)),
		_ => Ok(()),
	}
}

/// The avatar's file names, the "online" picture first, without duplicates.
fn avatar_files(info: Option<&api::AvatarInfo>) -> Vec<String> {
	let mut maps: Vec<_> = info.map(|i| i.map.iter().collect()).unwrap_or_default();
	maps.sort_by_key(|m| m.state != api::AvatarState::Online as i32);
	let mut names: Vec<String> = Vec::new();
	for map in maps {
		if !map.name.is_empty() && !names.contains(&map.name) {
			names.push(map.name.clone());
		}
	}
	names
}

/// Dates come in seconds or milliseconds; seconds either way.
fn unix_seconds(value: i64) -> i64 {
	if value > 100_000_000_000 { value / 1000 } else { value.max(0) }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_avatar_is_kept_as_it_came() {
		let avatar = api::AvatarData {
			info: Some(api::AvatarInfo { map: vec![map(api::AvatarState::Online, "https://a/o")] }),
			timestamp: 1_760_000_000,
			sign: vec![7; 64],
			..Default::default()
		};
		// A field this version does not know, inside the avatar.
		let mut raw = schema_api::wire::encode(&avatar);
		raw.extend_from_slice(&[0x78, 0x01]);
		let mut message = schema_api::wire::encode(&api::LoginSession {
			session: "s".into(),
			username: "u".into(),
			..Default::default()
		});
		message.push(((USER_AVATAR << 3) | 2) as u8);
		message.push(raw.len() as u8);
		message.extend_from_slice(&raw);
		message.extend_from_slice(&schema_api::wire::encode(&api::LoginSession {
			push_token: "after".into(),
			..Default::default()
		}));
		assert_eq!(raw_field(&message, USER_AVATAR), Some(raw.as_slice()));
		assert_eq!(raw_field(&message, 13), None);
		// Cut short: nothing rather than a part.
		assert_eq!(raw_field(&message[..message.len() - 3], USER_AVATAR), None);
		assert!(Presentation::default().is_empty());
	}

	fn map(state: api::AvatarState, name: &str) -> api::avatar_info::AvatarMap {
		api::avatar_info::AvatarMap { state: state as i32, name: name.into() }
	}

	#[test]
	fn account_data_becomes_a_profile() {
		let data = user::UserAccountData {
			return_code: Some(user::ReturnCode { success: true, ..Default::default() }),
			username: "Alex".into(),
			email_address: "alex@example.test".into(),
			description: "Hi".into(),
			registration_date: 1_700_000_000_000,
			last_login: 1_750_000_000,
			avatar_info: Some(api::AvatarInfo {
				map: vec![
					map(api::AvatarState::Away, "away.png"),
					map(api::AvatarState::Online, "online.png"),
					map(api::AvatarState::Dnd, "online.png"),
					map(api::AvatarState::Offline, ""),
				],
			}),
			badges: vec![user::UserBadge {
				name: "Early".into(),
				description: "Was there".into(),
				url: "https://badges.example.test/early.svg".into(),
				..Default::default()
			}],
			authenticated_devices: vec![user::AuthenticatedDevice {
				device_name: "Voelin".into(),
				last_login: 1_760_000_000_000,
				region: "EU".into(),
				..Default::default()
			}],
			..Default::default()
		};
		let profile = from_account_data(data).unwrap();
		assert_eq!(profile.username, "Alex");
		assert_eq!(profile.email, "alex@example.test");
		assert_eq!(profile.registered, 1_700_000_000);
		assert_eq!(profile.last_login, 1_750_000_000);
		assert_eq!(profile.avatars, ["online.png", "away.png"]);
		assert_eq!(profile.badges[0].name, "Early");
		assert_eq!(profile.devices[0].last_login, 1_760_000_000);
	}

	#[test]
	fn refusals_and_bad_links_are_errors() {
		let refused = user::UserAccountData {
			return_code: Some(user::ReturnCode {
				error_code: SESSION_EXPIRED,
				..Default::default()
			}),
			..Default::default()
		};
		let error = from_account_data(refused).unwrap_err();
		assert!(error.is_invalid_session());
		assert_eq!(
			avatar_link("https://cdn.example.test/a.png?sig=1").unwrap(),
			"https://cdn.example.test/a.png?sig=1"
		);
		for bad in ["http://cdn.example.test/a", "file:///etc/passwd", "a.png", ""] {
			assert!(matches!(avatar_link(bad), Err(Error::AvatarUrl)), "{bad}");
		}
		let long = format!("https://cdn.example.test/{}", "a".repeat(4096));
		assert!(matches!(avatar_link(&long), Err(Error::AvatarUrl)));
	}

	#[test]
	fn the_login_response_carries_avatar_and_description() {
		let response = api::LoginSession {
			username: "Alex".into(),
			user_avatar: Some(api::AvatarData {
				info: Some(api::AvatarInfo { map: vec![map(api::AvatarState::Online, "o.png")] }),
				..Default::default()
			}),
			user_data: Some(api::UserData { description: "Hi".into() }),
			..Default::default()
		};
		let profile = Profile::from_login(&response);
		assert_eq!(
			(profile.avatars, profile.description.as_str()),
			(vec!["o.png".to_owned()], "Hi")
		);
	}
}
