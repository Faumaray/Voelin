//! Account sessions over the custom myTeamSpeak HTTP transport.
//!
//! Requests use the official client's method envelope and password derivation.
//! No requests or credentials are logged.
mod identity;
mod transport;
pub use identity::IdentityError;
pub use tsproto::myts::Identity as ServerIdentity;
use zeroize::{Zeroize, Zeroizing};

use base64::{Engine as _, engine::general_purpose::STANDARD};
pub use schema_api::SessionToken;
use schema_api::{api, session, wire};
use std::fmt;

/// Owned account login result. Credential fields are redacted in Debug output.
#[derive(Clone)]
pub struct Login {
	pub token: SessionToken,
	pub uuid: String,
	pub username: String,
	pub renewal: Option<Renewal>,
	/// Validated server identity, or why this session cannot associate with servers.
	pub identity: Result<ServerIdentity, IdentityError>,
}

/// Server-provided alternative login material, stored only in a secret store.
#[derive(Clone)]
pub struct Renewal {
	pub token: String,
	pub otp_token: String,
	pub device_id: String,
}

impl fmt::Debug for Login {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Login([REDACTED])")
	}
}
impl fmt::Debug for Renewal {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Renewal([REDACTED])")
	}
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("account transport failed")]
	Transport(#[from] reqwest::Error),
	#[error("invalid account response")]
	Decode(#[from] schema_api::prost::DecodeError),
	#[error(transparent)]
	Session(#[from] session::SessionError),
	#[error(transparent)]
	Status(#[from] session::CommonStatusError),
	#[error("account request exceeds size limit")]
	RequestTooLarge,
	#[error("account response exceeds size limit")]
	ResponseTooLarge,
	#[error("account request timed out")]
	Timeout,
	#[error("account endpoint returned HTTP {0}")]
	Http(u16),
	#[error("account endpoint returned an unsupported content type")]
	ContentType,
	#[error("account credential preparation failed")]
	CredentialPreparation,
	#[error("renewal requires both a renewal token and an auth token")]
	MissingRenewalCredentials,
}
impl Error {
	pub fn status_code(&self) -> Option<i32> {
		match self {
			Self::Status(status) | Self::Session(session::SessionError::Api(status)) => {
				Some(status.code)
			}
			_ => None,
		}
	}
	pub fn is_otp_required(&self) -> bool {
		self.status_code() == Some(api::ErrorCommon::ErrorAuthRequiresOtp as i32)
	}
	/// Only the explicit expired-session status permits an automatic renewal.
	pub fn is_invalid_session(&self) -> bool {
		self.status_code() == Some(api::ErrorCommon::ErrorSessionExpired as i32)
	}
}

#[derive(Clone)]
pub struct Client {
	transport: transport::Transport,
}
impl Client {
	pub fn new() -> Result<Self, Error> {
		Ok(Self { transport: transport::Transport::new()? })
	}

	/// Sign in with the user's password; the protocol credential is derived
	/// internally, off the async runtime's worker threads.
	pub async fn login(
		&self,
		email: &str,
		password: &str,
		otp: Option<&str>,
		device: &str,
	) -> Result<Login, Error> {
		self.login_with_otp_renewal(email, password, otp, device, "").await
	}

	/// Password login on a remembered device. The OTP renewal token must belong
	/// to this account and device; an explicit one-time code takes precedence.
	pub async fn login_with_otp_renewal(
		&self,
		email: &str,
		password: &str,
		otp: Option<&str>,
		device: &str,
		otp_renewal: &str,
	) -> Result<Login, Error> {
		// Check before copying caller-controlled strings into protobuf allocations.
		transport::check_input_size(&[
			email,
			password,
			otp.unwrap_or_default(),
			device,
			otp_renewal,
		])?;
		let mut request = api::LoginData {
			email: email.into(),
			password: password.into(),
			otp: otp.unwrap_or_default().into(),
			device_id: device.into(),
			otp_renewal_token: if device.is_empty() || otp.is_some_and(|code| !code.is_empty()) {
				String::new()
			} else {
				otp_renewal.into()
			},
			device_name: "Voelin".into(),
			..Default::default()
		};
		let (request, encryption_key) = tokio::task::spawn_blocking(move || {
			let password = Zeroizing::new(std::mem::take(&mut request.password));
			let encryption_key = identity::encryption_key(&request.email, &password);
			request.password = login_password(&request.email, &password);
			(request, encryption_key)
		})
		.await
		.map_err(|_| Error::CredentialPreparation)?;
		self.login_call("login", &wire::encode(&request), Some(encryption_key)).await
	}

	/// `auth` is the schema's auth_token, not an OTP or device identifier.
	/// The official client requires both credentials. Password login does not
	/// supply an auth token; callers must not invent one or pass an empty value.
	pub async fn login_with_renewal(&self, renewal: &str, auth: &str) -> Result<Login, Error> {
		if renewal.is_empty() || auth.is_empty() {
			return Err(Error::MissingRenewalCredentials);
		}
		transport::check_input_size(&[renewal, auth])?;
		let request =
			api::RenewalTokenLogin { renewal_token: renewal.into(), auth_token: auth.into() };
		self.login_call("loginWithRenewalToken", &wire::encode(&request), None).await
	}

	async fn login_call(
		&self,
		method: &str,
		body: &[u8],
		encryption_key: Option<Zeroizing<[u8; 32]>>,
	) -> Result<Login, Error> {
		let bytes = self.transport.call("authentication", method, body).await?;
		let response: api::LoginSession = wire::decode(bytes)?;
		let token = SessionToken::try_from(&response)?;
		let (response, identity) = tokio::task::spawn_blocking(move || {
			let identity = match encryption_key {
				Some(key) => identity::decrypt(&response, &key),
				None => Err(IdentityError::PasswordRequired),
			};
			(response, identity)
		})
		.await
		.map_err(|_| Error::CredentialPreparation)?;
		Ok(Login {
			token,
			identity,
			uuid: response.uuid,
			username: response.username,
			renewal: response.alternative_login_info.map(|info| Renewal {
				token: info.renewal_token,
				otp_token: info.otp_renewal_token,
				device_id: info.device_id,
			}),
		})
	}

	pub async fn validate_session(&self, token: &SessionToken) -> Result<(), Error> {
		let response = self.session_call("session", token).await?;
		Ok(session::validate_session_status(&response)?)
	}
	pub async fn logout(&self, token: &SessionToken) -> Result<(), Error> {
		let response = self.session_call("deleteSession", token).await?;
		Ok(session::validate_deleted_session_status(&response)?)
	}
	async fn session_call(
		&self,
		method: &str,
		token: &SessionToken,
	) -> Result<api::LoginStatus, Error> {
		transport::check_input_size(&[token.as_str()])?;
		let bytes = self
			.transport
			.call("session", method, &wire::encode(&api::Session::from(token)))
			.await?;
		Ok(wire::decode(bytes)?)
	}
}

fn login_password(email: &str, password: &str) -> String {
	// Official client's classic-locale bytewise lowercase is ASCII-only.
	// These are wire-protocol constants, not configurable password policy.
	let salt = Zeroizing::new(format!("{}ts3Login{password}", email.to_ascii_lowercase()));
	let mut derived = pbkdf2::pbkdf2_hmac_array::<pbkdf2::sha2::Sha512, 48>(
		password.as_bytes(),
		salt.as_bytes(),
		10_000,
	);
	let encoded = STANDARD.encode(derived);
	derived.zeroize();
	encoded
}

#[cfg(test)]
mod tests;
