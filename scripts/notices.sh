#!/usr/bin/env bash
# Regenerate THIRD_PARTY_NOTICES.md from Cargo.lock with cargo-about
# (about.toml, about.hbs), followed by about-assets.md (the fonts, icons and
# emoji the UI bundles, which cargo-about does not see). The app shows the
# file in its About page
# (voelin_platform::notices::text()), so it is committed; run this after changing
# dependencies.
#
# Usage: scripts/notices.sh           regenerate THIRD_PARTY_NOTICES.md
#        scripts/notices.sh --check   fail if the committed file is stale (CI)
#
# Needs cargo-about $ABOUT_VERSION; the output depends on its version:
#   cargo install cargo-about --locked --version 0.9.2 --features cli
# Fails on any warning, e.g. a license whose text was not found.
set -euo pipefail

ABOUT_VERSION=0.9.2
OUT=THIRD_PARTY_NOTICES.md

cd "$(dirname "$0")/.."

check=false
case "${1:-}" in
	--check) check=true ;;
	"") ;;
	*) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

version=$(cargo about --version 2>/dev/null | awk '{print $2}' || true)
if [[ "$version" != "$ABOUT_VERSION" ]]; then
	echo "cargo-about $ABOUT_VERSION is required (found: ${version:-none}):" >&2
	echo "  cargo install cargo-about --locked --version $ABOUT_VERSION --features cli" >&2
	exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Workspace members with default features, filtered by the targets in
# about.toml, so the result does not depend on the host.
if ! cargo about generate --workspace --locked --fail about.hbs -o "$tmp/$OUT" 2>"$tmp/log"; then
	cat "$tmp/log" >&2
	exit 1
fi
if grep -E "WARN|ERROR" "$tmp/log" >&2; then
	echo "cargo-about reported problems (above); fix about.toml" >&2
	exit 1
fi
cat about-assets.md >>"$tmp/$OUT"
# Some upstream license texts contain trailing spaces. Keep the generated
# Markdown clean without changing the words of those notices.
sed 's/[[:blank:]]*$//' "$tmp/$OUT" >"$tmp/normalized"
mv "$tmp/normalized" "$tmp/$OUT"

if $check; then
	if ! diff -u "$OUT" "$tmp/$OUT" >"$tmp/diff"; then
		head -n 100 "$tmp/diff"
		echo "$OUT is stale: run scripts/notices.sh and commit the result" >&2
		exit 1
	fi
	echo "$OUT is up to date"
else
	mv "$tmp/$OUT" "$OUT"
	echo "wrote $OUT ($(wc -c <"$OUT") bytes)"
fi
