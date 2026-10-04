#!/usr/bin/env bash
# Build the release Flatpak in the same privileged image as GitHub Actions.
# Requires rootful Docker on a trusted machine. No CI cache/artifact API used.
# Usage: scripts/flatpak-build.sh [out-dir, default dist/flatpak]
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p "${1:-dist/flatpak}"
out=$(cd "${1:-dist/flatpak}" && pwd)
tmp=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/voelin-flatpak.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/source"
# Copy tracked working-tree files only: target/, secrets, and runner state
# must not become a Flatpak source or enter the privileged build container.
git ls-files -z | tar --null -T - -cf - | tar -C "$tmp/source" -xf -

docker run --rm --privileged \
	--mount "type=bind,src=$tmp/source,dst=/src" \
	--mount "type=bind,src=$out,dst=/out" \
	--workdir /src \
	ghcr.io/flathub-infra/flatpak-github-actions:freedesktop-25.08 \
	bash -euo pipefail -c '
		curl -sSfL -o /tmp/flatpak-cargo-generator.py \
			https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/41c20aa10819cdb2a4f3ca171758a96d1955c018/cargo/flatpak-cargo-generator.py
		python3 -m venv /tmp/venv
		/tmp/venv/bin/pip install --quiet "aiohttp>=3.9.5,<4" "tomlkit>=0.13.3,<1" "PyYAML>=6.0.2,<7"
		/tmp/venv/bin/python /tmp/flatpak-cargo-generator.py Cargo.lock -o packaging/flatpak/cargo-sources.json
		flatpak remote-add --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo
		xvfb-run -a flatpak-builder --repo=/tmp/repo --state-dir=/tmp/state \
			--disable-rofiles-fuse --install-deps-from=flathub --force-clean \
			--default-branch=master --arch=x86_64 \
			/tmp/build packaging/flatpak/io.github.faumaray.Voelin.yml
		flatpak build-bundle --runtime-repo=https://flathub.org/repo/flathub.flatpakrepo \
			--arch=x86_64 /tmp/repo /out/voelin.flatpak io.github.faumaray.Voelin master
	'
test -s "$out/voelin.flatpak"
printf '%s\n' "$out/voelin.flatpak"
