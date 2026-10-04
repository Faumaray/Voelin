# Verification

Verified on 2026-10-04 on Linux x86_64 with Rust/Cargo 1.99.0, using the included `Cargo.lock` (tonic 0.14.6, prost 0.14.4, prost-reflect 0.16.5). Dependencies are fetched by Cargo; protobuf compilation uses `protoc-bin-vendored` 3.2.0.

## Checks

| Check | Result |
| --- | --- |
| All features, all targets | 25 library tests + 4 schema tests + 5 local transport tests passed; all four examples compile |
| No default features | 15 library tests + 3 schema tests passed; 4 documentation examples compile/run |
| Default-feature documentation tests | 4 passed |
| Clippy, all features and targets, warnings denied | Passed |
| Rustfmt check | Passed |
| Rustdoc, all features, no dependencies | Generated successfully |
| Original protobuf source integrity | All 123 files match the recorded SHA-256 hashes |

## Reproduce

```sh
cargo fmt --all --check
cargo test --locked --all-features --all-targets
cargo test --locked --no-default-features
cargo test --locked --doc
cargo clippy --locked --all-features --all-targets -- -D warnings
cargo doc --locked --all-features --no-deps
```

The GitHub Actions workflow runs these checks using stable Rust. Test counts across feature configurations overlap; they are not separate suites to sum together.

## What was exercised

Local tonic servers test the exact original HTTP/2 method paths for unary add-on download lookup and push streaming, typed request/response payloads, response metadata, default ASCII/binary metadata, per-call metadata overrides, preserved gRPC error codes/details/metadata, streamed frames, normal EOF, terminal stream errors, and cancellation of a pending unary RPC by its configured deadline. These tests use ephemeral localhost ports and deterministic server shutdown.

Unit/schema tests cover metadata rotation and sensitivity, endpoint validation, timeout precedence, expected login/session/deletion success codes, unknown codes, nested body session assignment, checked protobuf `Any` URLs, malformed payload rejection, opt-in push envelopes, proto2 presence, a fixed protobuf wire vector, complete application service/RPC counts, cached descriptors, protobuf JSON naming/enums/base64/int64/Any, and unknown JSON/wire behavior.

## Scope and remaining integration work

No deployment endpoint, credentials, or server implementation was supplied, so no live service call was made. TLS/mTLS examples compile; they were not exercised against a certificate-bearing test or production server. Gzip support compiles in the all-feature build; compressed traffic was not separately exercised. Linux is the only tested build host.

Business rules absent from the schemas remain caller/server responsibilities: credential encoding, cryptography, signed-URL HTTP transfer details, opaque blob interpretation, renewal policy, authorization requirements, and synchronization conflict resolution. Every declared request/response field and application RPC remains accessible.

The supplied Google `descriptor.proto` and `cpp_features.proto` contain malformed option syntax. Original files remain unchanged; builds use compiler-matched standard Google imports, and the compiler-only `pb.CppFeatures` type is excluded. The application and optional infrastructure bindings both compile with this documented strategy.
