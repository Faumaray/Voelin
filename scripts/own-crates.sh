#!/usr/bin/env bash
# Print `-p <crate>` for every workspace crate outside the vendored crates/proto
# and third_party (manifest paths with / or \ separators, for Windows),
# on one line, for commands that apply strict checks to our own code only
# (`jq -j`: no line breaks at all, as jq on Windows ends lines with \r\n):
#   cargo clippy $(scripts/own-crates.sh) --all-targets --no-deps -- -D warnings
set -euo pipefail
cd "$(dirname "$0")/.."
cargo metadata --no-deps --format-version 1 |
	jq -j '.packages[] | select(.manifest_path | gsub("\\\\"; "/") | (contains("/crates/proto/") or contains("/third_party/")) | not) | "-p \(.name) "'
