#!/usr/bin/env bash
# Print `-p <crate>` for every workspace crate outside the vendored crates/proto
# (manifest paths with / or \ separators, for Windows),
# for commands that apply strict checks to our own code only:
#   cargo clippy $(scripts/own-crates.sh) --all-targets --no-deps -- -D warnings
set -euo pipefail
cd "$(dirname "$0")/.."
cargo metadata --no-deps --format-version 1 |
	jq -r '.packages[] | select(.manifest_path | gsub("\\\\"; "/") | contains("/crates/proto/") | not) | "-p \(.name)"' |
	tr '\n' ' '
