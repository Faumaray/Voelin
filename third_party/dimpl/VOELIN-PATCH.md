# dimpl in Voelin

This directory is dimpl 0.7.4 from crates.io (`MIT OR Apache-2.0`, see
`LICENSE-MIT.txt` and `LICENSE-APACHE.txt`), vendored so that it can be
patched. The root `Cargo.toml` replaces the crates.io crate with it:

```toml
[patch.crates-io]
dimpl = { path = "third_party/dimpl" }
```

Everything that depends on dimpl (str0m through `str0m-proto` and
`str0m-aws-lc-rs`, and `voelin-stream`) builds against this copy. The
directory is excluded from the workspace (`exclude = ["fuzz", "third_party"]`),
so its own tests and dev-dependencies are not part of our builds, and
`scripts/own-crates.sh` leaves it out of the strict fmt/clippy checks. The
manifest keeps upstream's `license`, so cargo-deny and cargo-about treat it
like the crates.io crate.

The commit that added this directory is the unmodified crate as published
(the `.crate` contents: `Cargo.toml` normalized by crates.io,
`Cargo.toml.orig` the original manifest). `git diff` against that commit
shows our changes.

## The patch: SRTP profile order

Upstream, a DTLS 1.2 server picks the SRTP protection profile (RFC 5764) in a
fixed order, AEAD_AES_256_GCM, AEAD_AES_128_GCM, then AES128_CM_SHA1_80, and
a client always offers all three; there is no setting. In TeamSpeak 6
streams our str0m peer is the DTLS server, and the official clients
(libwebrtc) offer all three, so our streams ended up with AES-256-GCM, while
streams between official clients use AES_CM_128_HMAC_SHA1_80. The patch:

- `ConfigBuilder::srtp_profiles(&[SrtpProfile])`: the profiles in order of
  preference (duplicates dropped; an empty list is `ConfigError::NoSrtpProfiles`).
  `Config::srtp_profiles()` returns it, or `SrtpProfile::ALL` when unset.
- `Config::select_srtp_profile(offered)`: the first configured profile the
  client offered. The DTLS 1.2 server uses it; with the default list that is
  exactly upstream's fixed order. The DTLS 1.3 server uses it when a list was
  configured, otherwise it keeps upstream's choice (the client's order).
- Clients (DTLS 1.2, 1.3 and the auto/hybrid ClientHello) offer the configured
  profiles in their order (`UseSrtpExtension::from_profiles`); by default
  that is upstream's offer. A client rejects a server that selects a profile
  it did not offer (`SecurityError::ServerSelectedUnofferedSrtpProfile`, as
  RFC 5764 requires; with the default offer any profile dimpl parses was
  offered, so nothing changes there).
- `[workspace]` at the end of `Cargo.toml`, so the crate's tests run from this
  directory even inside another workspace's tree (e.g. a git worktree under
  the repository).
- Unit tests: `config::tests::srtp_profiles_*`,
  `use_srtp::tests::from_profiles_keeps_order`, and `srtp_profile_order` in
  `src/lib.rs` (full DTLS 1.2/1.3 handshakes: the server's order wins over a
  browser-like GCM-first offer; unset, the result is upstream's).

`voelin-stream` (`crates/voelin-stream/src/dtls.rs`) builds its dimpl
configurations with the order from `PeerConfig::srtp_profiles`.

## Dropping the patch

Once upstream dimpl can configure the SRTP profiles (and str0m either passes
such a setting through or lets `voelin-stream` keep building its own dimpl
`Config` as it does now): switch `crates/voelin-stream/src/dtls.rs` to the
upstream API, remove the `[patch.crates-io]` entry and `"third_party"` from
`exclude` in the root `Cargo.toml` if nothing else is vendored, delete this
directory, and run `cargo update -p dimpl` so `Cargo.lock` points at
crates.io again. `scripts/notices.sh --check` and `cargo deny check` should
pass unchanged.

To update the vendored version instead: copy the new release from
`~/.cargo/registry/src/*/dimpl-<version>` in a commit of its own (plus the
`[workspace]` table), re-apply the changes above, and `cargo update -p dimpl`.

## Running its tests

From this directory (its own workspace with its own `Cargo.lock`):

```sh
cargo test --locked --lib          # unit tests, including the patch's
cargo test --locked                # plus upstream's OpenSSL/wolfSSL interop tests
```

The upstream dev-dependencies (OpenSSL vendored, wolfSSL) are built for
that, so the first run takes a few minutes. Do not run `cargo fmt` here: our
`rustfmt.toml` would reformat the crate; format changed files with an empty
rustfmt config (`rustfmt --edition 2024 --config-path <empty dir> <files>`).
