# syntax=docker/dockerfile:1
#
# Linux packages (docs/building.md), for Ubuntu 24.04 and newer: the app as
# voelin-<version>-linux-x86_64.tar.gz and voelin_<version>_amd64.deb, the
# gateway (servers) as tsgw-<version>-linux-x86_64.tar.gz and
# tsgw_<version>_amd64.deb.
#
#   docker build -f docker/linux.Dockerfile -o dist/linux .
#
# or scripts/docker-build.sh linux. The cargo registry and target directory
# live in cache mounts, so rebuilds are incremental.

FROM ubuntu:24.04 AS toolchain
ARG DEBIAN_FRONTEND=noninteractive
# The same libraries as CI (.github/actions/system-deps), plus dpkg-dev for
# the .deb's dependencies and patchelf for the .tar.gz's bundled libraries.
RUN apt-get update && apt-get install -y --no-install-recommends \
		build-essential ca-certificates cmake curl dpkg-dev patchelf pkg-config \
		libasound2-dev libfontconfig1-dev libxkbcommon-dev \
		libvpx-dev libdav1d-dev libpipewire-0.3-dev libspa-0.2-dev libclang-dev \
	&& rm -rf /var/lib/apt/lists/*
# Extra CA certificates (PEM, one or more), for building behind a
# TLS-inspecting proxy; scripts/docker-build.sh passes $BUILD_CA_CERT as this
# secret. One file per certificate, so the Java truststore gets each one too.
RUN --mount=type=secret,id=ca,required=false \
	if [ -s /run/secrets/ca ]; then \
		awk '/-----BEGIN CERTIFICATE-----/ { n++ } n { print > ("/usr/local/share/ca-certificates/build-ca-" n ".crt") }' \
			/run/secrets/ca \
		&& update-ca-certificates; \
	fi
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
# The toolchain rust-toolchain.toml asks for.
COPY rust-toolchain.toml /tmp/toolchain/
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
		| sh -s -- -y --no-modify-path --profile minimal --default-toolchain none \
	&& cd /tmp/toolchain && rustup toolchain install

FROM toolchain AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=voelin-cargo-registry,target=/usr/local/cargo/registry \
	--mount=type=cache,id=voelin-target-linux,target=/src/target \
	cargo build --release --locked -p voelin-ui -p voelin-gateway \
	&& scripts/package.sh linux /out

FROM scratch AS artifacts
COPY --from=build /out /
