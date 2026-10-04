# schema-api snapshot

Copied on 2026-10-04 from the owner's local
`/home/faumaray/Project/Personal/schema-api` (version 0.1.0).
The source directory has no Git metadata and declares no license; its licensing
must be resolved before redistribution. This note does not grant a license.

Voelin consumes the message types, session helpers and wire codec via a path
dependency with default features disabled. Generated gRPC clients are not used
for myTeamSpeak's custom HTTP transport.

The only upstream patch is an empty `[workspace]` table in `Cargo.toml`, matching
Voelin's other vendored packages. This packaging change prevents Cargo from
assigning the vendor to an enclosing checkout's workspace when Voelin itself is
in a nested worktree. All Rust and protobuf sources remain unmodified.
