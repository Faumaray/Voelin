//! The account's profile: what `getAccountData` and the login response tell
//! about the signed-in user, and the avatar's download link.

use crate::Error;
use schema_api::SessionToken;
use schema_api::api::{self, user};

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
	/// The avatar's file names, the one for "online" first.
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

/// The signed link for `file_name`; only HTTPS (and loopback HTTP in tests).
pub(crate) fn signed_url(
	response: user::AvatarSignedUrlResponse,
	file_name: &str,
) -> Result<String, Error> {
	check(response.return_code.as_ref())?;
	let url = response
		.signed_urls
		.into_iter()
		.find(|u| u.file_name == file_name || u.file_name.is_empty())
		.map(|u| u.signed_url)
		.ok_or(Error::AvatarUrl)?;
	let allowed =
		url.starts_with("https://") || (cfg!(test) && url.starts_with("http://127.0.0.1:"));
	if !allowed || url.len() > 4096 {
		return Err(Error::AvatarUrl);
	}
	Ok(url)
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
		let response = |url: &str| user::AvatarSignedUrlResponse {
			return_code: Some(user::ReturnCode { success: true, ..Default::default() }),
			signed_urls: vec![user::avatar_signed_url_response::SignedUrl {
				file_name: "a.png".into(),
				signed_url: url.into(),
			}],
		};
		assert_eq!(
			signed_url(response("https://cdn.example.test/a.png?sig=1"), "a.png").unwrap(),
			"https://cdn.example.test/a.png?sig=1"
		);
		assert!(matches!(
			signed_url(response("http://cdn.example.test/a"), "a.png"),
			Err(Error::AvatarUrl)
		));
		assert!(matches!(
			signed_url(response("file:///etc/passwd"), "a.png"),
			Err(Error::AvatarUrl)
		));
		assert!(matches!(signed_url(response("https://x"), "b.png"), Err(Error::AvatarUrl)));
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
