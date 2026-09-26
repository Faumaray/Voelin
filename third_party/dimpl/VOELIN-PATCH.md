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
`scripts/own-crates.sh` leaves it out of the strict fmt/clippy checks.

The first commit that added this directory is the unmodified crate as
published (the `.crate` contents, `Cargo.toml` normalized by crates.io,
`Cargo.toml.orig` the original manifest). `git diff` against that commit
shows our changes.

## Running its tests

From this directory (its own workspace; it has its own `Cargo.lock`):

```sh
cargo test --lib
```

The upstream dev-dependencies (OpenSSL vendored, wolfSSL) are built for
that, so the first run takes a few minutes.
