#!/usr/bin/env bash
# Build the packages in Docker, the same ones CI uploads (docs/building.md).
#
#   scripts/docker-build.sh [linux|windows|android|all] [docker build args…]
#
# Output goes to dist/<platform>/. Needs Docker with BuildKit (Docker 23+, or
# docker buildx). Extra arguments go to `docker build`, e.g.
#   scripts/docker-build.sh android --build-arg ABIS=arm64-v8a
#
# Behind an HTTPS proxy, HTTPS_PROXY/HTTP_PROXY/NO_PROXY are passed on to the
# build; a proxy on this machine's loopback is reached with host networking.
# If the proxy inspects TLS, point BUILD_CA_CERT at its CA certificate (PEM):
# the builds then trust it.
set -euo pipefail

cd "$(dirname "$0")/.."

platform=${1:-all}
shift || true
case "$platform" in
	linux | windows | android) platforms=("$platform") ;;
	all) platforms=(linux windows android) ;;
	*) echo "usage: $0 [linux|windows|android|all] [docker build args…]" >&2; exit 2 ;;
esac

args=()
for var in HTTPS_PROXY https_proxy HTTP_PROXY http_proxy NO_PROXY no_proxy; do
	if [[ -n ${!var:-} ]]; then
		args+=(--build-arg "$var=${!var}")
		case "${!var}" in
			*://127.* | *://localhost*) args+=(--network host) ;;
		esac
	fi
done

if [[ -n ${BUILD_CA_CERT:-} ]]; then
	args+=(--secret "id=ca,src=$BUILD_CA_CERT")
fi

for p in "${platforms[@]}"; do
	echo "== $p → dist/$p"
	DOCKER_BUILDKIT=1 docker build -f "docker/$p.Dockerfile" -o "dist/$p" "${args[@]}" "$@" .
	ls -l "dist/$p"
done
