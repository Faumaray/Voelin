//! Shared transport configuration and access to every application service.
//!
//! The schema does not specify a transport authentication scheme. Metadata and
//! bearer authentication are therefore opt-in. Sessions embedded in protobuf
//! messages remain explicit fields controlled by the caller.

use std::{
    collections::HashSet,
    fmt,
    sync::{Arc, RwLock},
    time::Duration,
};

use tonic::{
    metadata::{Ascii, KeyAndValueRef, MetadataMap, MetadataValue},
    service::{interceptor::InterceptedService, Interceptor},
    transport::{Channel, ClientTlsConfig, Endpoint},
    Request, Status,
};

use crate::api::{
    addon::user_addon_service_client::UserAddonServiceClient,
    integration::integration_user_service_client::IntegrationUserServiceClient,
    login::login_service_client::LoginServiceClient,
    management::user::user_management_service_client::UserManagementServiceClient,
    messengerconnector::messenger_connector_client_service_client::MessengerConnectorClientServiceClient,
    synchronization::synchronization_service_client::SynchronizationServiceClient,
    tschat::chat_requests_client::ChatRequestsClient,
    user::user_account_service_client::UserAccountServiceClient,
};
use crate::push::push_service_client::PushServiceClient;

/// Errors raised while configuring or connecting a client.
///
/// RPC failures themselves are returned as [`tonic::Status`] by generated clients.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The endpoint or TLS configuration was invalid, or the connection failed.
    #[error("gRPC transport error: {0}")]
    Transport(#[from] tonic::transport::Error),
    /// A URL must identify an HTTP(S) origin rather than a service path.
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(&'static str),
    /// Bearer tokens must be nonempty and contain only visible ASCII without spaces.
    #[error("bearer token must be nonempty visible ASCII without spaces")]
    InvalidBearerToken,
    /// A panic occurred while shared metadata was being changed.
    #[error("shared request metadata lock is poisoned")]
    MetadataLockPoisoned,
}

/// Metadata shared by all service handles derived from an [`ApiClient`].
///
/// Changes affect subsequent calls, including calls made through already-created
/// service handles. They do not change an active push stream. Debug output never
/// includes metadata values. Explicit per-request metadata overrides defaults.
#[derive(Clone, Default)]
pub struct SharedMetadata(Arc<RwLock<MetadataMap>>);

impl SharedMetadata {
    /// Create a shared metadata store, preserving duplicate and binary entries.
    pub fn new(metadata: MetadataMap) -> Self {
        Self(Arc::new(RwLock::new(metadata)))
    }

    /// Read a snapshot. The returned map may contain credentials.
    pub fn snapshot(&self) -> Result<MetadataMap, ClientError> {
        self.0
            .read()
            .map(|map| map.clone())
            .map_err(|_| ClientError::MetadataLockPoisoned)
    }

    /// Atomically replace all default metadata.
    pub fn replace(&self, metadata: MetadataMap) -> Result<(), ClientError> {
        *self
            .0
            .write()
            .map_err(|_| ClientError::MetadataLockPoisoned)? = metadata;
        Ok(())
    }

    /// Set or rotate an optional bearer token while keeping other metadata.
    ///
    /// The value is marked sensitive. Pass the token without the `Bearer ` prefix.
    pub fn set_bearer_token(&self, token: impl AsRef<str>) -> Result<(), ClientError> {
        let value = bearer_value(token.as_ref())?;
        self.0
            .write()
            .map_err(|_| ClientError::MetadataLockPoisoned)?
            .insert("authorization", value);
        Ok(())
    }

    /// Remove the default authorization header.
    pub fn clear_bearer_token(&self) -> Result<(), ClientError> {
        self.0
            .write()
            .map_err(|_| ClientError::MetadataLockPoisoned)?
            .remove("authorization");
        Ok(())
    }
}

impl fmt::Debug for SharedMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedMetadata").finish_non_exhaustive()
    }
}

fn bearer_value(token: &str) -> Result<MetadataValue<Ascii>, ClientError> {
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(ClientError::InvalidBearerToken);
    }
    let mut value: MetadataValue<Ascii> = format!("Bearer {token}")
        .parse()
        .map_err(|_| ClientError::InvalidBearerToken)?;
    value.set_sensitive(true);
    Ok(value)
}

/// The interceptor used by the configured generated service clients.
#[derive(Clone, Debug)]
pub struct RequestInterceptor {
    metadata: SharedMetadata,
    timeout: Option<Duration>,
}

impl RequestInterceptor {
    /// Create an interceptor for a generated client or another compatible service.
    pub fn new(metadata: SharedMetadata, timeout: Option<Duration>) -> Self {
        Self { metadata, timeout }
    }
}

impl Interceptor for RequestInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let defaults = self
            .metadata
            .0
            .read()
            .map_err(|_| Status::internal("shared request metadata is unavailable"))?;
        // Capture the original keys before appending, so duplicate default values
        // survive while any explicit per-request value overrides the entire key.
        let supplied: HashSet<String> = request
            .metadata()
            .iter()
            .map(|entry| match entry {
                KeyAndValueRef::Ascii(key, _) => key.as_str().to_owned(),
                KeyAndValueRef::Binary(key, _) => key.as_str().to_owned(),
            })
            .collect();
        for entry in defaults.iter() {
            match entry {
                KeyAndValueRef::Ascii(key, value) if !supplied.contains(key.as_str()) => {
                    request.metadata_mut().append(key.clone(), value.clone());
                }
                KeyAndValueRef::Binary(key, value) if !supplied.contains(key.as_str()) => {
                    request
                        .metadata_mut()
                        .append_bin(key.clone(), value.clone());
                }
                _ => {}
            }
        }
        if !request.metadata().contains_key("grpc-timeout") {
            if let Some(timeout) = self.timeout {
                request.set_timeout(timeout);
            }
        }
        Ok(request)
    }
}

/// Channel type used by every service getter on [`ApiClient`].
pub type InterceptedChannel = InterceptedService<Channel, RequestInterceptor>;

#[derive(Clone, Debug, Default)]
struct CallOptions {
    request_timeout: Option<Duration>,
    stream_timeout: Option<Duration>,
    max_decoding_message_size: Option<usize>,
    max_encoding_message_size: Option<usize>,
    #[cfg(feature = "gzip")]
    send_gzip: bool,
    #[cfg(feature = "gzip")]
    accept_gzip: bool,
}

/// Builder for shared transport, request metadata, and message limits.
///
/// An HTTPS URL enables certificate validation using native trust roots. A
/// custom [`ClientTlsConfig`] can supply a private CA and an mTLS identity.
/// No application credentials or request deadlines are set by default.
#[derive(Debug)]
#[must_use = "call connect, connect_lazy, or build_with_channel to create the client"]
pub struct ClientBuilder {
    endpoint: Endpoint,
    metadata: SharedMetadata,
    options: CallOptions,
}

impl ClientBuilder {
    /// Build from an absolute `http://` or `https://` origin URL.
    ///
    /// The default connection timeout is ten seconds. Protobuf service paths
    /// are generated from the schema, so URL path prefixes are rejected.
    pub fn new(endpoint: impl Into<String>) -> Result<Self, ClientError> {
        let mut endpoint = Endpoint::from_shared(endpoint.into())?;
        validate_endpoint(&endpoint)?;
        if endpoint.uri().scheme_str() == Some("https") {
            endpoint = endpoint.tls_config(ClientTlsConfig::new().with_native_roots())?;
        }
        endpoint = endpoint.connect_timeout(Duration::from_secs(10));
        Ok(Self::from_endpoint(endpoint))
    }

    /// Use a preconfigured tonic endpoint without changing its transport options.
    ///
    /// The caller is responsible for configuring TLS and validating this endpoint.
    /// This is an escape hatch for advanced flow control, origin, and executor
    /// settings that are not exposed directly by this builder.
    pub fn from_endpoint(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            metadata: SharedMetadata::default(),
            options: CallOptions::default(),
        }
    }

    /// Replace the TLS settings, including trust roots, server name, or mTLS identity.
    pub fn tls_config(mut self, config: ClientTlsConfig) -> Result<Self, ClientError> {
        self.endpoint = self.endpoint.tls_config(config)?;
        Ok(self)
    }

    /// Set the connection-establishment timeout.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.endpoint = self.endpoint.connect_timeout(timeout);
        self
    }

    /// Set a default `grpc-timeout` for unary calls.
    ///
    /// Per-request deadlines override this value. This uses
    /// [`Request::set_timeout`]: the channel limits response setup and sends the
    /// deadline to the server. Push calls are unaffected; use
    /// [`Self::stream_timeout`] to opt into a push deadline.
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.options.request_timeout = Some(timeout);
        self
    }

    /// Set the push RPC's `grpc-timeout`. No stream deadline is set by default.
    ///
    /// The channel limits response setup; enforcement after stream establishment
    /// depends on the server. This is not an inactivity timeout between messages.
    /// For a client-side receive timeout, wrap `stream.message()` in
    /// `tokio::time::timeout`.
    pub fn stream_timeout(mut self, timeout: Duration) -> Self {
        self.options.stream_timeout = Some(timeout);
        self
    }

    /// Replace all default request metadata. Per-request values take precedence.
    pub fn metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = SharedMetadata::new(metadata);
        self
    }

    /// Reuse an existing metadata store for coordinated credential rotation.
    pub fn shared_metadata(mut self, metadata: SharedMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Set optional bearer authentication, without changing other metadata.
    ///
    /// This also updates the shared store if [`Self::shared_metadata`] was used.
    pub fn bearer_token(self, token: impl AsRef<str>) -> Result<Self, ClientError> {
        self.metadata.set_bearer_token(token)?;
        Ok(self)
    }

    /// Set the maximum decoded response size in bytes for all service clients.
    ///
    /// If unset, tonic's default limit is retained (4 MiB).
    pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
        self.options.max_decoding_message_size = Some(limit);
        self
    }

    /// Set the maximum encoded request size in bytes for all service clients.
    pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
        self.options.max_encoding_message_size = Some(limit);
        self
    }

    /// Enable or disable compression of outgoing requests. Disabled by default.
    #[cfg(feature = "gzip")]
    pub fn send_gzip(mut self, enabled: bool) -> Self {
        self.options.send_gzip = enabled;
        self
    }

    /// Advertise support for gzip responses. Disabled by default.
    #[cfg(feature = "gzip")]
    pub fn accept_gzip(mut self, enabled: bool) -> Self {
        self.options.accept_gzip = enabled;
        self
    }

    /// Set TCP keepalive probe idle time, or disable it with `None`.
    pub fn tcp_keepalive(mut self, interval: Option<Duration>) -> Self {
        self.endpoint = self.endpoint.tcp_keepalive(interval);
        self
    }

    /// Enable or disable TCP_NODELAY.
    pub fn tcp_nodelay(mut self, enabled: bool) -> Self {
        self.endpoint = self.endpoint.tcp_nodelay(enabled);
        self
    }

    /// Set the HTTP/2 ping interval. Coordinate this with the server's policy.
    pub fn http2_keep_alive_interval(mut self, interval: Duration) -> Self {
        self.endpoint = self.endpoint.http2_keep_alive_interval(interval);
        self
    }

    /// Set the maximum wait for a keepalive ping acknowledgement.
    pub fn keep_alive_timeout(mut self, timeout: Duration) -> Self {
        self.endpoint = self.endpoint.keep_alive_timeout(timeout);
        self
    }

    /// Send HTTP/2 keepalive pings even when no RPC is active.
    pub fn keep_alive_while_idle(mut self, enabled: bool) -> Self {
        self.endpoint = self.endpoint.keep_alive_while_idle(enabled);
        self
    }

    /// Override the user-agent prefix used by tonic.
    pub fn user_agent(mut self, user_agent: impl AsRef<str>) -> Result<Self, ClientError> {
        self.endpoint = self.endpoint.user_agent(user_agent.as_ref())?;
        Ok(self)
    }

    /// Establish the shared channel now. Requires a Tokio runtime.
    pub async fn connect(self) -> Result<ApiClient, ClientError> {
        let channel = self.endpoint.connect().await?;
        Ok(self.build_with_channel(channel))
    }

    /// Defer connection until the first RPC. Requires a Tokio runtime.
    pub fn connect_lazy(self) -> ApiClient {
        let channel = self.endpoint.connect_lazy();
        self.build_with_channel(channel)
    }

    /// Apply request settings to a caller-provided channel.
    ///
    /// The builder's endpoint, TLS, connection, and keepalive options are ignored;
    /// the supplied channel owns those settings. Useful for custom connectors.
    pub fn build_with_channel(self, channel: Channel) -> ApiClient {
        ApiClient {
            channel,
            metadata: self.metadata,
            options: self.options,
        }
    }
}

fn validate_endpoint(endpoint: &Endpoint) -> Result<(), ClientError> {
    let uri = endpoint.uri();
    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
        return Err(ClientError::InvalidEndpoint(
            "expected an absolute http:// or https:// origin",
        ));
    }
    if uri
        .authority()
        .is_some_and(|authority| authority.as_str().contains('@'))
    {
        return Err(ClientError::InvalidEndpoint(
            "use request metadata for credentials instead of URL user information",
        ));
    }
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
        return Err(ClientError::InvalidEndpoint(
            "service URL paths and query strings are not supported",
        ));
    }
    Ok(())
}

/// A cloneable collection of the nine application service clients.
///
/// All service handles share one multiplexed channel. Cloning the facade or a
/// generated handle is inexpensive; separate handles support concurrent calls.
/// No RPC is retried automatically and no body/session fields are injected.
#[derive(Clone, Debug)]
pub struct ApiClient {
    channel: Channel,
    metadata: SharedMetadata,
    options: CallOptions,
}

macro_rules! service_getter {
    ($method:ident, $client:ident, $stream:literal, $description:literal) => {
        #[doc = $description]
        pub fn $method(&self) -> $client<InterceptedChannel> {
            let timeout = if $stream {
                self.options.stream_timeout
            } else {
                self.options.request_timeout
            };
            let mut client = $client::with_interceptor(
                self.channel.clone(),
                RequestInterceptor::new(self.metadata.clone(), timeout),
            );
            if let Some(limit) = self.options.max_decoding_message_size {
                client = client.max_decoding_message_size(limit);
            }
            if let Some(limit) = self.options.max_encoding_message_size {
                client = client.max_encoding_message_size(limit);
            }
            #[cfg(feature = "gzip")]
            {
                if self.options.send_gzip {
                    client = client.send_compressed(tonic::codec::CompressionEncoding::Gzip);
                }
                if self.options.accept_gzip {
                    client = client.accept_compressed(tonic::codec::CompressionEncoding::Gzip);
                }
            }
            client
        }
    };
}

impl ApiClient {
    /// Create a configuration builder for the supplied HTTP(S) origin.
    pub fn builder(endpoint: impl Into<String>) -> Result<ClientBuilder, ClientError> {
        ClientBuilder::new(endpoint)
    }

    /// Connect with the default settings and no transport credentials.
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self, ClientError> {
        Self::builder(endpoint)?.connect().await
    }

    /// Wrap an existing channel with empty metadata and default message limits.
    ///
    /// For custom request settings, use [`ClientBuilder::build_with_channel`].
    pub fn from_channel(channel: Channel) -> Self {
        Self {
            channel,
            metadata: SharedMetadata::default(),
            options: CallOptions::default(),
        }
    }

    /// Access the shared metadata store for credential rotation or replacement.
    pub fn metadata(&self) -> SharedMetadata {
        self.metadata.clone()
    }

    /// Clone the underlying channel for advanced generated-client usage.
    ///
    /// A raw channel does not apply this facade's metadata or message options.
    pub fn channel(&self) -> Channel {
        self.channel.clone()
    }

    service_getter!(
        login,
        LoginServiceClient,
        false,
        "Get the login, session, token, and account lifecycle client."
    );
    service_getter!(
        user,
        UserAccountServiceClient,
        false,
        "Get the user account, avatar, file, and voice-server client."
    );
    service_getter!(
        management,
        UserManagementServiceClient,
        false,
        "Get the user-management, badge, OAuth, and community client."
    );
    service_getter!(
        chat,
        ChatRequestsClient,
        false,
        "Get the contacts, identifiers, and chat-session client."
    );
    service_getter!(
        synchronization,
        SynchronizationServiceClient,
        false,
        "Get the item synchronization client."
    );
    service_getter!(
        integration,
        IntegrationUserServiceClient,
        false,
        "Get the account integration and subscription client."
    );
    service_getter!(
        messenger,
        MessengerConnectorClientServiceClient,
        false,
        "Get the messenger-account connector client."
    );
    service_getter!(
        addon,
        UserAddonServiceClient,
        false,
        "Get the add-on download client."
    );
    service_getter!(
        push,
        PushServiceClient,
        true,
        "Get the server-streaming push client. Unary deadlines do not apply."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_ascii_binary_and_duplicate_metadata() {
        let mut defaults = MetadataMap::new();
        defaults.append("x-tag", "one".parse().unwrap());
        defaults.append("x-tag", "two".parse().unwrap());
        defaults.append_bin("x-payload-bin", MetadataValue::from_bytes(&[0, 255]));
        defaults.append_bin("x-payload-bin", MetadataValue::from_bytes(&[1, 254]));
        let mut interceptor = RequestInterceptor::new(SharedMetadata::new(defaults), None);
        let result = interceptor.call(Request::new(())).unwrap();
        assert_eq!(result.metadata().get_all("x-tag").iter().count(), 2);
        let binary: Vec<_> = result
            .metadata()
            .get_all_bin("x-payload-bin")
            .iter()
            .map(|value| value.to_bytes().unwrap())
            .collect();
        assert_eq!(binary.len(), 2);
        assert_eq!(binary[0].as_ref(), &[0, 255]);
        assert_eq!(binary[1].as_ref(), &[1, 254]);
    }

    #[test]
    fn per_request_metadata_overrides_entire_default_key() {
        let mut defaults = MetadataMap::new();
        defaults.append("x-tag", "default-one".parse().unwrap());
        defaults.append("x-tag", "default-two".parse().unwrap());
        defaults.insert_bin("x-data-bin", MetadataValue::from_bytes(b"default"));
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert("x-tag", "explicit".parse().unwrap());
        request
            .metadata_mut()
            .insert_bin("x-data-bin", MetadataValue::from_bytes(b"explicit"));
        let mut interceptor = RequestInterceptor::new(SharedMetadata::new(defaults), None);
        let result = interceptor.call(request).unwrap();
        assert_eq!(result.metadata().get_all("x-tag").iter().count(), 1);
        assert_eq!(result.metadata().get("x-tag").unwrap(), "explicit");
        assert_eq!(
            result
                .metadata()
                .get_bin("x-data-bin")
                .unwrap()
                .to_bytes()
                .unwrap()
                .as_ref(),
            b"explicit"
        );
    }

    #[test]
    fn bearer_rotation_reaches_existing_interceptor_and_preserves_other_headers() {
        let mut defaults = MetadataMap::new();
        defaults.insert("x-project", "demo".parse().unwrap());
        let shared = SharedMetadata::new(defaults);
        let mut interceptor = RequestInterceptor::new(shared.clone(), None);
        shared.set_bearer_token("first").unwrap();
        let first = interceptor.call(Request::new(())).unwrap();
        assert_eq!(
            first.metadata().get("authorization").unwrap(),
            "Bearer first"
        );
        assert!(first
            .metadata()
            .get("authorization")
            .unwrap()
            .is_sensitive());
        shared.set_bearer_token("second").unwrap();
        let second = interceptor.call(Request::new(())).unwrap();
        assert_eq!(
            second.metadata().get("authorization").unwrap(),
            "Bearer second"
        );
        assert_eq!(second.metadata().get("x-project").unwrap(), "demo");
        assert!(!format!("{shared:?}").contains("second"));
        shared.clear_bearer_token().unwrap();
        assert!(!interceptor
            .call(Request::new(()))
            .unwrap()
            .metadata()
            .contains_key("authorization"));
    }

    #[test]
    fn invalid_bearer_does_not_replace_working_credentials() {
        let shared = SharedMetadata::default();
        shared.set_bearer_token("valid").unwrap();
        for invalid in ["", "has space", "injected\r\nheader", "non-ascii-é"] {
            assert!(matches!(
                shared.set_bearer_token(invalid),
                Err(ClientError::InvalidBearerToken)
            ));
        }
        assert_eq!(
            shared.snapshot().unwrap().get("authorization").unwrap(),
            "Bearer valid"
        );
    }

    #[test]
    fn default_deadline_is_added_and_explicit_deadline_wins() {
        let mut interceptor =
            RequestInterceptor::new(SharedMetadata::default(), Some(Duration::from_secs(30)));
        let result = interceptor.call(Request::new(())).unwrap();
        let mut expected = Request::new(());
        expected.set_timeout(Duration::from_secs(30));
        assert_eq!(
            result.metadata().get("grpc-timeout").unwrap(),
            expected.metadata().get("grpc-timeout").unwrap()
        );
        let mut explicit = Request::new(());
        explicit.set_timeout(Duration::from_secs(2));
        let deadline = explicit.metadata().get("grpc-timeout").unwrap().clone();
        let result = interceptor.call(explicit).unwrap();
        assert_eq!(result.metadata().get("grpc-timeout").unwrap(), deadline);
    }

    #[test]
    fn no_timeout_or_auth_is_added_by_default() {
        let mut interceptor = RequestInterceptor::new(SharedMetadata::default(), None);
        assert!(interceptor
            .call(Request::new(()))
            .unwrap()
            .metadata()
            .is_empty());
        let builder = ClientBuilder::new("http://localhost:50051")
            .unwrap()
            .request_timeout(Duration::from_secs(5));
        assert_eq!(
            builder.options.request_timeout,
            Some(Duration::from_secs(5))
        );
        assert_eq!(builder.options.stream_timeout, None);
    }

    #[test]
    fn endpoint_validation_rejects_non_origins() {
        for invalid in [
            "ftp://example.com",
            "example.com",
            "http://example.com/api",
            "https://example.com?token=x",
        ] {
            assert!(
                ClientBuilder::new(invalid).is_err(),
                "unexpected valid URL: {invalid}"
            );
        }
        assert!(ClientBuilder::new("http://localhost:50051/").is_ok());
    }
}
