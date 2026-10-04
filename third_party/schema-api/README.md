# schema-api

A Rust crate for the API in the supplied protobuf archive. It exposes **all 106 application RPCs across nine services**, plus all 228 application message declarations and 33 enums. Original namespaces, field numbers, enum numbers, oneofs, proto2 presence, and RPC route spelling are preserved. The namespace is treated solely as your API's wire contract.

Features include configurable async gRPC clients, native/custom TLS and mTLS, rotating request metadata, request deadlines, message limits, optional gzip, session-body helpers, binary codecs, checked `Any` conversion, runtime descriptors, protobuf JSON, and optional generated servers. All original 123 `.proto` files are included.

## Use locally

Extract this directory next to your application and add:

```toml
[dependencies]
schema-api = { path = "../schema-api" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Use a current stable Rust toolchain. Cargo downloads dependencies on the first build. A bundled `protoc` is selected automatically; installing a protobuf compiler separately is unnecessary. `PROTOC` can override the executable and `PROTOC_INCLUDE` can override its standard include directory (containing `google/protobuf`). Set both on hosts without a vendored compiler. `Cargo.lock` records the versions used for verification. This local crate is not published; `publish = false` avoids accidental registry publication. No new license is imposed on your supplied schemas.

## Login and a session-authenticated call

```no_run
use schema_api::{api, ApiClient, SessionRequest, SessionToken};
use std::time::Duration;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let client = ApiClient::builder("https://api.example.invalid")?
    .connect_timeout(Duration::from_secs(10))
    .request_timeout(Duration::from_secs(30))
    .connect()
    .await?;

let response = client.login().login(api::LoginData {
    email: "user@example.invalid".into(),
    password: "your-server-defined-credential".into(),
    device_name: "Rust client".into(),
    ..Default::default()
}).await?.into_inner();

// Require the explicit LOGIN_OK code (200), then a nonempty session.
// Keep the full response: it also carries keys, permissions and renewal data.
let session = SessionToken::try_from(&response)?;
let request = api::user::Session::default().with_session(&session);
let badges = client.user().get_badges(request).await?.into_inner();
// Inspect the response's own application status fields before using its data.
# let _ = badges;
# Ok(())
# }
```

The archive does not define a hostname, credential encoding, or deployment authentication policy. Supply your own endpoint and server-defined credential representation. Session helpers set explicit protobuf body fields; they do not add a guessed authorization header. `skipSession` logins can succeed without yielding a usable `SessionToken`. Unknown numeric application error values remain accessible.

## Service coverage

Every getter returns a generated tonic client over a cheap clone of the shared channel. Client methods accept either a message or `tonic::Request<Message>` and retain response metadata and `tonic::Status` errors. No RPC is hidden behind a smaller handwritten facade.

| Getter | Service | RPCs | Coverage |
| --- | --- | ---: | --- |
| `login()` | `LoginService` | 12 | Sessions, login, auth/renewal tokens, account status |
| `user()` | `UserAccountService` | 30 | Accounts, badges, voice servers, avatars, files, 2FA |
| `management()` | `UserManagementService` | 39 | Users, activation, resets, badges, communities |
| `chat()` | `ChatRequests` | 16 | Contacts, identifiers, home migration, group sessions |
| `synchronization()` | `SynchronizationService` | 2 | Check and update item classes |
| `integration()` | `IntegrationUserService` | 4 | Bindings, status, subscriptions |
| `messenger()` | `MessengerConnectorClientService` | 1 | Messenger account creation |
| `addon()` | `UserAddonService` | 1 | Download/update lookup |
| `push()` | `PushService` | 1 | Server-streaming `longPull` |

Rust method names are snake case, such as `login_with_auth_token`, `get_account_data`, and `long_pull`. Their on-wire method names retain the original spelling. The complete method/type/auth-field catalog is in `docs/SCHEMA.md`; `docs/schema-inventory.json` includes resolved paths, symbols, and original source hashes.

Use separate `ApiClient`s when services live at different endpoints. For custom connectors or middleware, use `ApiClient::from_channel`, `client::ClientBuilder::from_endpoint`, or the public generated client types under `proto`.

## Namespaces and features

| Namespace | Contents |
| --- | --- |
| `api` | Shared application types and service submodules |
| `push` | Push transport `Message` and `PushServiceClient` |
| `api::push` | Notification envelopes and payload types |
| `account` | Local account serialization messages |
| `sync` | Local synchronization serialization messages |
| `proto::com::teamspeak` | Complete original application package hierarchy, including caches and revocation |
| `proto::envoy`, `proto::xds`, etc. | Optional infrastructure packages |

| Cargo feature | Default | Effect |
| --- | --- | --- |
| `reflection` | Yes | Runtime inspection, dynamic messages, protobuf JSON |
| `server` | No | Generated tonic server traits/adapters for every enabled service |
| `infrastructure` | No | Also generate supporting Envoy/xDS/UDPA/CEL/gRPC/validation bindings and Envoy load-reporting client |
| `gzip` | No | Opt-in request/response gzip support |

`--no-default-features` retains all application types, gRPC clients, session helpers, codecs, TLS, and the raw `FILE_DESCRIPTOR_SET`, while dropping runtime JSON/reflection dependencies. Google well-known messages map to `prost_types`; `google.protobuf.Empty` maps to `()`.

The archive's Google `descriptor.proto` and `cpp_features.proto` contain malformed options. The build keeps these originals untouched and resolves standard Google protobuf imports from the compiler's bundled includes first. Compiler-only `pb.CppFeatures` is not exposed. The optional infrastructure definitions are data bindings; generating validation annotations does not execute business validation or implement an xDS controller.

## Metadata, TLS, deadlines, and limits

```no_run
use schema_api::{ApiClient, tonic::{Request, metadata::MetadataMap}};
use std::time::Duration;
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let mut headers = MetadataMap::new();
headers.insert("x-tenant", "example".parse()?);
let client = ApiClient::builder("https://api.example.invalid")?
    .metadata(headers)
    .max_decoding_message_size(16 * 1024 * 1024)
    .max_encoding_message_size(16 * 1024 * 1024)
    .request_timeout(Duration::from_secs(20))
    .connect().await?;

// Only enable bearer authentication if it is part of YOUR deployment contract.
client.metadata().set_bearer_token("opaque-token")?;
client.metadata().clear_bearer_token()?;

let mut request = Request::new(schema_api::api::Session { session: "session".into() });
request.metadata_mut().insert("x-tenant", "override".parse()?);
request.set_timeout(Duration::from_secs(5));
let _ = client.login().session(request).await?;
# Ok(())
# }
```

Per-request metadata overrides defaults by key; duplicate default values and binary metadata are preserved. Shared metadata changes affect future calls through existing handles. Debug output for the shared metadata store and `SessionToken` is redacted; generated protobuf messages can contain secrets and should not be logged wholesale.

HTTPS enables native-root certificate validation. `client::ClientBuilder::tls_config` accepts a tonic `ClientTlsConfig` for a private CA, explicit TLS name, or client identity; see `examples/custom_tls.rs`. HTTP is supported for local or appropriately protected deployments. Endpoint paths/queries are rejected because service routes come from the protobuf descriptors.

The default connection timeout is 10 seconds. No RPC deadline is imposed by default. `request_timeout` sets `grpc-timeout` for unary calls; `stream_timeout` opts into a push deadline. The channel limits response setup; enforcement after a stream is established depends on the server. For a receive inactivity timeout, wrap `stream.message()` in `tokio::time::timeout`. Per-request timeouts override defaults. Message-size limits are per protobuf message. With `gzip`, use `send_gzip(true)` and `accept_gzip(true)` only when supported by the server.

## Push streams and binary payloads

```no_run
# async fn example(client: schema_api::ApiClient) -> Result<(), Box<dyn std::error::Error>> {
let mut stream = client.push().long_pull(()).await?.into_inner();
while let Some(message) = stream.message().await? {
    // The contract only guarantees opaque bytes here.
    println!("{} bytes", message.payload.len());
}
# Ok(())
# }
```

Dropping the stream cancels it. Stream errors remain `tonic::Status`. There is no automatic replay/reconnection policy because no resume cursor or delivery guarantee is specified. `wire::decode_push_payload::<T>` and `decode_push_notification` are explicit, fallible decoders for deployments that define the corresponding encoding. Typed `wire::pack_any`/`unpack_any` use full protobuf names and reject mismatched type URLs. Plain `wire::encode`/`decode` work with any prost message.

## Reflection and protobuf JSON

```rust
# #[cfg(feature = "reflection")]
# {
use schema_api::{api, reflection};
let message = api::LoginStatus { error: 200, uuid: "id".into() };
let json = reflection::to_json(&message).unwrap();
let roundtrip: api::LoginStatus = reflection::from_json(&json).unwrap();
assert_eq!(roundtrip, message);
# }
```

JSON follows protobuf rules: base64 bytes, named enums, string-encoded 64-bit integers, lower-camel field names, and descriptor-resolved `Any`. Unknown JSON fields and trailing content are rejected by default; configurable options are available. Dynamic messages support schema inspection and retain unknown wire fields. Decoding into generated types discards unknown wire fields. `FILE_DESCRIPTOR_SET` is available in every build for tooling or an optional server reflection implementation.

## Build and run examples

```sh
cargo test --locked --all-features --all-targets
cargo test --locked --no-default-features
cargo test --locked --doc
cargo run --locked --example inspect_schema
cargo doc --locked --all-features --no-deps --open
```

`examples/login.rs` reads `API_ENDPOINT`, `API_EMAIL`, `API_PASSWORD`, and optional `API_OTP`/`API_DEVICE_ID`. `examples/push.rs` reads `API_ENDPOINT` and optional `API_PUSH_HEADER`/`API_PUSH_VALUE`. No real hostname or credential is embedded. `examples/custom_tls.rs` documents CA and mTLS environment variables.

Tests cover actual in-process unary/streaming gRPC exchanges, metadata and status handling, deadline separation, schema coverage, binary compatibility, proto2 presence, unknown enums, checked session status, nested session updates, `Any`, and protobuf JSON. Verification details are in `docs/VERIFICATION.md`.

The schemas do not define encryption/signature algorithms, password derivation, blob envelope encoding, signed-URL HTTP upload details, token refresh rules, or safe retry/idempotency behavior. The crate exposes all related fields and RPCs without inventing those protocols or automatically retrying mutations. Live server compatibility requires your endpoint and its deployment contract.
