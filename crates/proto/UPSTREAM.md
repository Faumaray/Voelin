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
3. **Init1 RSA puzzle off-thread** (`tsproto/src/algorithms.rs`, `tsproto/src/client.rs`,
   `tsproto/benches/modpow.rs`). `solve_rsa_puzzle` does Montgomery squarings with
   `crypto-bigint` (about 25% faster than `num-bigint` `modpow`) on a
   `spawn_blocking` thread that stops when the connect future is dropped. The level
   cap is `max_puzzle_level()` (default 100 million, was a fixed 10 million),
   adjustable with `set_max_puzzle_level`, because TeamSpeak 6 adapts the level.
4. **TeamSpeak 6 declarations** (`tsproto-structs/declarations/Messages.toml`, `Book.toml`).
   New fields `client_is_streaming` (book: `Client::is_streaming: Option<bool>`),
   `virtualserver_address`, `virtualserver_version_sign`; stream notifications
   (`notifystreamstarted`, `notifystreamstopped`, `notifystreaminfo`,
   `notifyjoinstreamrequest`, `notifyrespondjoinstreamrequest`,
   `notifystreamsignaling`, `notifystreamclientjoined`, `notifystreamclientleft`) and
   commands (`setupstream`, `stopstream`, `streamsignaling`, `respondjoinstreamrequest`,
   `removeclientfromstream`). See `docs/protocol-notes/ts6-streaming.md`.
5. **`curve25519-dalek-ng` → `curve25519-dalek` 4** (`tsproto-types`, `tsproto`). The `-ng`
   fork is unmaintained; the API is the same apart from the basepoint table being a
   reference. Verified by the license-chain unit tests and a live TeamSpeak 6 handshake.
6. **Panics on malformed input found by fuzzing** (`fuzz/`):
   - `tsproto/src/license.rs`: an empty license (`initivexpand2 l=`) indexed `data[0]`;
     license properties indexed past the end or with start > end. All property reads
     are bounds-checked now.
   - `tsproto-packets/src/packets.rs`: the invalid-codec error for short C2S audio
     packets read `content[4]` instead of the codec byte `content[2]`.
7. **TeamSpeak 6 stream commands, verified against 6.0.0-beta13.1**
   (`tsproto-structs/declarations/Messages.toml`). New fields `is_remove` and
   `reason` (stream leave reason). New command `joinstreamrequest id clid msg is_remove`
   (`JoinStreamRequestRequest`); `stopstream` and `removeclientfromstream` take the
   required `reason`; `notifyjoinstreamrequest` carries `is_remove`,
   `notifystreamstopped` and `notifystreamclientleft` carry `reason`, the latter also
   `return_code`.
8. **File transfer fixes** (`tsclientlib/src/lib.rs`, marked "Voelin patch").
   A server that refuses `ftinitdownload`/`ftinitupload` (file not found, no
   permission, quota) answers with an `error` for the command's return code, which
   came out as a `MessageResult` the caller cannot match to its
   `FiletransferHandle`; it now fails the transfer (`FiletransferFailed`). A
   transfer address of `0.0.0.0`/`::` means the server's own address. The return
   code and transfer id counters wrap instead of overflowing.
