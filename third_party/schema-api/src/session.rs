//! Session credentials and helpers for session-bearing request bodies.
//!
//! The schemas put sessions in protobuf fields. These helpers do not infer an
//! HTTP authorization header or copy sessions into unrelated nested messages.

use std::fmt;

use crate::api;

/// A nonempty session credential. Its [`Debug`](fmt::Debug) output is redacted.
///
/// This wrapper does not encrypt or zeroize its allocation. Generated protobuf
/// requests have ordinary `String` fields and their `Debug` output can include
/// credentials, so avoid logging complete request and response bodies.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionToken(String);

impl SessionToken {
    /// Wrap an existing credential without changing its contents.
    ///
    /// Only the empty string is rejected: the schema specifies no other token
    /// syntax, and whitespace or other characters are therefore not normalized.
    pub fn new(value: impl Into<String>) -> Result<Self, SessionError> {
        let value = value.into();
        if value.is_empty() {
            Err(SessionError::EmptyToken)
        } else {
            Ok(Self(value))
        }
    }

    /// Access the credential for an API that requires its string value.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Explicitly expose the credential. Equivalent to [`Self::as_str`].
    pub fn expose_secret(&self) -> &str {
        self.as_str()
    }

    /// Consume this wrapper and return the credential.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionToken([REDACTED])")
    }
}

impl TryFrom<&api::LoginSession> for SessionToken {
    type Error = SessionError;

    /// Require `ERROR_LOGIN_OK`, then require a nonempty session credential.
    ///
    /// A successful login with `skipSession` can legitimately omit a session;
    /// that response cannot be converted into a session credential.
    fn try_from(response: &api::LoginSession) -> Result<Self, Self::Error> {
        check_common_status("login", response.error, api::ErrorCommon::ErrorLoginOk)?;
        Self::new(response.session.clone())
    }
}

impl TryFrom<String> for SessionToken {
    type Error = SessionError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for SessionToken {
    type Error = SessionError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<&SessionToken> for api::Session {
    fn from(token: &SessionToken) -> Self {
        Self {
            session: token.as_str().to_owned(),
        }
    }
}

impl From<&SessionToken> for api::user::Session {
    fn from(token: &SessionToken) -> Self {
        Self {
            session: token.as_str().to_owned(),
        }
    }
}

/// An API-level error from the common login/session status enum.
///
/// The raw integer is retained, including values introduced by newer servers.
/// It is separate from a gRPC transport status, which may still be successful.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{operation} returned API status {code}")]
pub struct CommonStatusError {
    /// The operation whose expected success status was absent.
    pub operation: &'static str,
    /// The unmodified protobuf enum integer returned by the server.
    pub code: i32,
}

impl CommonStatusError {
    /// Resolve a known status without mapping unknown values to zero.
    pub fn known_code(&self) -> Option<api::ErrorCommon> {
        api::ErrorCommon::try_from(self.code).ok()
    }

    /// The original protobuf enum name, if the supplied schemas define it.
    pub fn code_name(&self) -> Option<&'static str> {
        self.known_code().map(|code| code.as_str_name())
    }
}

/// Failure to create a usable session credential.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// The credential was absent or empty.
    #[error("the session credential is empty")]
    EmptyToken,
    /// The server did not return the operation's success status.
    #[error(transparent)]
    Api(#[from] CommonStatusError),
}

fn check_common_status(
    operation: &'static str,
    code: i32,
    expected: api::ErrorCommon,
) -> Result<(), CommonStatusError> {
    if code == expected as i32 {
        Ok(())
    } else {
        Err(CommonStatusError { operation, code })
    }
}

/// Require `ERROR_SESSION_OK` (102) from `LoginService.session`.
pub fn validate_session_status(response: &api::LoginStatus) -> Result<(), CommonStatusError> {
    check_common_status("session", response.error, api::ErrorCommon::ErrorSessionOk)
}

/// Require `ERROR_SESSION_DELETED_OK` (204) from `LoginService.deleteSession`.
///
/// This is a separate check because successful session validation and deletion
/// have different status values in the supplied schemas.
pub fn validate_deleted_session_status(
    response: &api::LoginStatus,
) -> Result<(), CommonStatusError> {
    check_common_status(
        "deleteSession",
        response.error,
        api::ErrorCommon::ErrorSessionDeletedOk,
    )
}

/// Set a request's schema-defined session field while preserving other fields.
///
/// Implemented for all application request/body messages with a direct string
/// `session` or `session_id` field, and for the explicitly supported wrappers
/// whose nested request carries the session. This does not recursively modify
/// every session-looking field (for example a chat `shared_session_id`).
pub trait SessionRequest: Sized {
    /// Replace the request's session credential.
    fn set_session(&mut self, token: &SessionToken);

    /// Set the credential and return the request for fluent construction.
    fn with_session(mut self, token: &SessionToken) -> Self {
        self.set_session(token);
        self
    }
}

macro_rules! session_field {
    ($($message:path => $field:ident),+ $(,)?) => {
        $(
            impl SessionRequest for $message {
                fn set_session(&mut self, token: &SessionToken) {
                    self.$field = token.as_str().to_owned();
                }
            }
        )+
    };
}

session_field! {
    api::Session => session,
    api::integration::RequestIntegrationUserStatus => session,
    api::integration::RequestBindData => session,
    api::integration::RequestUnbindData => session,
    api::integration::UserIntegrationSubscriptionInfoRequest => session_id,
    api::messengerconnector::RequestCreateMessengerAccountRequest => session,
    api::namedserver::User => session,
    api::namedserver::NamedServer => session,
    api::tschat::AuthenticatedUser => session,
    api::user::UserData => session,
    api::user::Session => session,
    api::user::BadgeCode => session,
    api::user::RequestContactsAvatarInfoRequest => session,
    api::user::SpawnVoiceServerLocationRequest => session,
    api::user::VoiceServerRequest => session,
    api::user::RequestUploadAvatarRequest => session,
    api::user::RequestDeleteAvatarInfoRequest => session,
    api::user::AvatarSignedUrlRequest => session,
    api::user::EmailChange => session,
    api::user::ResetAccountRequest => session,
    api::user::UpdateUserDescriptionRequest => session,
    api::user::UploadFileRequest => session,
    api::user::DownloadFileRequest => session,
    api::user::UpdateTwoFactorAuthTypeRequest => session,
    api::user::TwoFactorAuthTypeRequest => session,
    api::user::TwoFactorAuthTypeOtpConfirmationRequest => session,
    api::user::RegisterFirebasePush => session,
    api::user::AccountDataRequest => session,
    api::management::user::Request => session,
    api::management::user::BadgeCodesRequest => session,
    api::management::user::EmailChange => session,
    api::management::user::AssignBadgeRequest => session,
    api::management::user::UserRenameRequest => session,
    api::management::user::AddBadgeRequest => session,
    api::management::user::OAuthClientRequest => session,
    api::management::user::SearchTerm => session,
    api::management::user::PermissionRequest => session,
    api::management::user::TsCommunityRequest => session,
    api::management::user::RenameAllowanceRequest => session,
    api::management::user::BadgeStatisticsRequest => session,
    api::management::user::UserStatisticsRequest => session,
    api::synchronization::SyncRequestItemClasses => session,
}

macro_rules! nested_session {
    ($($message:path => $field:ident),+ $(,)?) => {
        $(
            impl SessionRequest for $message {
                fn set_session(&mut self, token: &SessionToken) {
                    self.$field.get_or_insert_with(Default::default).set_session(token);
                }
            }
        )+
    };
}

nested_session! {
    api::user::NewUserPublicKeyData => session,
    api::tschat::GroupSessionRequest => user,
    api::tschat::MoveRequest => user,
    api::tschat::ContactRequest => user,
    api::tschat::ContactRequestList => user,
    api::tschat::IdentifierRequest => user,
    api::management::user::UpdateBadgeRequest => add_badge_request,
    api::management::user::TsCommunityListRequest => request,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_requires_explicit_success_and_nonempty_token() {
        let mut response = api::LoginSession {
            session: "credential".into(),
            ..Default::default()
        };
        assert!(matches!(
            SessionToken::try_from(&response),
            Err(SessionError::Api(CommonStatusError { code: 0, .. }))
        ));
        response.error = api::ErrorCommon::ErrorLoginOk as i32;
        assert_eq!(
            SessionToken::try_from(&response).unwrap().as_str(),
            "credential"
        );
        response.session.clear();
        assert_eq!(
            SessionToken::try_from(&response),
            Err(SessionError::EmptyToken)
        );
    }

    #[test]
    fn redaction_does_not_normalize_credentials() {
        let token = SessionToken::new(" secret with spaces ").unwrap();
        assert_eq!(token.as_str(), " secret with spaces ");
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains("REDACTED"));
    }

    #[test]
    fn status_checks_preserve_unknown_codes_and_distinguish_deletion() {
        let unknown = api::LoginStatus {
            error: 98_765,
            ..Default::default()
        };
        let error = validate_session_status(&unknown).unwrap_err();
        assert_eq!(error.code, 98_765);
        assert_eq!(error.known_code(), None);
        assert_eq!(error.code_name(), None);

        let deleted = api::LoginStatus {
            error: api::ErrorCommon::ErrorSessionDeletedOk as i32,
            ..Default::default()
        };
        assert!(validate_deleted_session_status(&deleted).is_ok());
        let error = validate_session_status(&deleted).unwrap_err();
        assert_eq!(error.code_name(), Some("ERROR_SESSION_DELETED_OK"));
    }

    #[test]
    fn nested_authentication_preserves_existing_request_data() {
        let token = SessionToken::new("new-session").unwrap();
        let request = api::tschat::GroupSessionRequest {
            user: Some(api::tschat::AuthenticatedUser {
                session: "old-session".into(),
                matrix_id: "matrix-user".into(),
            }),
            group_sessions: vec![api::tschat::GroupSession {
                shared_session_id: "unrelated-group-session".into(),
                ..Default::default()
            }],
        }
        .with_session(&token);
        let user = request.user.unwrap();
        assert_eq!(user.session, "new-session");
        assert_eq!(user.matrix_id, "matrix-user");
        assert_eq!(
            request.group_sessions[0].shared_session_id,
            "unrelated-group-session"
        );

        let request = api::user::NewUserPublicKeyData::default().with_session(&token);
        assert_eq!(request.session.unwrap().session, "new-session");
    }

    #[test]
    fn session_id_and_oneof_fields_are_handled_without_replacement() {
        let token = SessionToken::new("token").unwrap();
        let request = api::integration::UserIntegrationSubscriptionInfoRequest::default()
            .with_session(&token);
        assert_eq!(request.session_id, "token");

        let identifier =
            api::management::user::request::RequestOneof::Email("owner@example.test".into());
        let request = api::management::user::Request {
            request_oneof: Some(identifier.clone()),
            ..Default::default()
        }
        .with_session(&token);
        assert_eq!(request.request_oneof, Some(identifier));
    }
}
