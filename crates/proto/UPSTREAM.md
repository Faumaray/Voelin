# Vendored upstream: ReSpeak/tsclientlib

The crates in this directory are a vendored fork of
[ReSpeak/tsclientlib](https://github.com/ReSpeak/tsclientlib), licensed
`MIT OR Apache-2.0` (see `LICENSE-MIT` and `LICENSE-APACHE` in this directory).

| Item | Source |
|---|---|
| tsclientlib | `ee3bc6f45a7137db7793ba5593a321df400d53e5` (master) |
| `tsproto-structs/declarations` (was a git submodule) | [ReSpeak/tsdeclarations](https://github.com/ReSpeak/tsdeclarations) `83cb8a94b65021bb8ec4beb339e5bfbdd93df60f` (MIT OR Apache-2.0, own `LICENSE-*` kept inside the directory) |

## Layout mapping

| Upstream path | Here |
|---|---|
| `tsclientlib/` | `crates/proto/tsclientlib/` |
| `tsproto/` | `crates/proto/tsproto/` |
| `utils/ts-bookkeeping/` | `crates/proto/ts-bookkeeping/` |
| `utils/tsproto-packets/` | `crates/proto/tsproto-packets/` |
| `utils/tsproto-structs/` | `crates/proto/tsproto-structs/` (submodule flattened into plain files) |
| `utils/tsproto-types/` | `crates/proto/tsproto-types/` |
| `README.md`, `CHANGELOG.md`, `.rustfmt.toml`, `clippy.toml` | `crates/proto/README.upstream.md`, `CHANGELOG.md`, `.rustfmt.toml`, `clippy.toml` |

Not vendored: `.appveyor/`, `tools/measure-latency.sh`, `tslientlib.code-workspace`,
the upstream root `Cargo.toml` (replaced by this repository's workspace).

## Local patches

The import commit contains the upstream files unmodified. Every later change to
files in this directory is listed here, newest last, so it can be diffed
against or offered back to upstream.

<!-- patches -->
1. **Workspace integration.** Path dependencies point at the flat `crates/proto/<crate>`
   layout; every crate gets `publish = false`.
2. **Replace `audiopus` with `opus2` 0.4** (`tsclientlib/src/audio.rs`,
   `tsclientlib/examples/audio_utils/audio_to_ts.rs`, `tsclientlib/Cargo.toml`).
   `audiopus` is unmaintained (RUSTSEC-2026-0150) and breaks with CMake 4. The
   `audiopus-unstable` feature (decoder complexity/DRED via a fork) is dropped.
