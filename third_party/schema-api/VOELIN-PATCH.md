# schema-api snapshot

Copied on 2026-10-04 from the owner's local
`/home/faumaray/Project/Personal/schema-api` (version 0.1.0).
The source directory has no Git metadata and declares no license; its licensing
must be resolved before redistribution. This note does not grant a license.

Voelin consumes the message types, session helpers and wire codec via a path
dependency with default features disabled. Generated gRPC clients are not used
for myTeamSpeak's custom HTTP transport.

Two packaging patches in `Cargo.toml`; all Rust and protobuf sources remain
unmodified:

1. An empty `[workspace]` table, matching Voelin's other vendored packages. It
   prevents Cargo from assigning the vendor to an enclosing checkout's
   workspace when Voelin itself is in a nested worktree.
2. tonic's TLS uses `tls-aws-lc` instead of `tls-ring`. Voelin's rustls already
   uses aws-lc-rs (reqwest, str0m); with `ring` too, rustls cannot choose a
   process-wide crypto provider and every TLS client built without an explicit
   one panics (`wss://` gateway connections did). `voelin-core`'s
   `one_rustls_crypto_provider` test fails if a second provider comes back.
