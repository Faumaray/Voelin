# syntax=docker/dockerfile:1
#
# Android APKs (docs/building.md, docs/android.md): a debug APK, installable
# as is, for arm64-v8a and x86_64 (emulators).
#
#   docker build -f docker/android.Dockerfile -o dist/android .
#   docker build -f docker/android.Dockerfile --build-arg ABIS=arm64-v8a -o dist/android .
#
# or scripts/docker-build.sh android. A signed release APK needs your keystore
# (docs/release.md), passed as build secrets:
#
#   docker build -f docker/android.Dockerfile --build-arg APK=release \
#     --secret id=keystore,src=release.jks --secret id=signing,src=signing.env \
#     -o dist/android .
#
# signing.env holds VOELIN_SIGNING_STORE_PASSWORD, VOELIN_SIGNING_KEY_ALIAS and
# VOELIN_SIGNING_KEY_PASSWORD lines (NAME=value). Without them a release APK is
# unsigned.

FROM ubuntu:24.04 AS toolchain
ARG DEBIAN_FRONTEND=noninteractive
# JDK 17 (Gradle, the Android Gradle plugin), CMake + Ninja (the bundled libopus).
RUN apt-get update && apt-get install -y --no-install-recommends \
		build-essential ca-certificates cmake curl ninja-build openjdk-17-jdk-headless \
		pkg-config python3 unzip \
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
# Java tools do not read HTTPS_PROXY: turn it into JVM properties for
# sdkmanager and Gradle when it is set (GRADLE_OPTS for the wrapper's JVM,
# -D arguments for the build JVM).
COPY <<-'EOF' /usr/local/bin/java-proxy-opts
	#!/bin/sh
	p=${HTTPS_PROXY:-${https_proxy:-}}
	[ -n "$p" ] || exit 0
	hp=${p#*://}; hp=${hp%%/*}; hp=${hp##*@}
	echo "-Dhttps.proxyHost=${hp%:*} -Dhttps.proxyPort=${hp##*:} -Dhttp.proxyHost=${hp%:*} -Dhttp.proxyPort=${hp##*:}"
EOF
RUN chmod +x /usr/local/bin/java-proxy-opts

# The SDK packages the release workflow installs (.github/workflows/release.yml,
# package-android job).
ENV ANDROID_HOME=/opt/android-sdk
ARG CMDLINE_TOOLS=commandlinetools-linux-16111833_latest.zip
ARG CMDLINE_TOOLS_SHA1=e025545c62a8e64c7559119566a569fb1dec5f60
ARG NDK_VERSION=27.3.13750724
RUN curl -sSfL -o /tmp/tools.zip "https://dl.google.com/android/repository/$CMDLINE_TOOLS" \
	&& echo "$CMDLINE_TOOLS_SHA1  /tmp/tools.zip" | sha1sum -c - \
	&& mkdir -p "$ANDROID_HOME/cmdline-tools" \
	&& unzip -q /tmp/tools.zip -d "$ANDROID_HOME/cmdline-tools" \
	&& mv "$ANDROID_HOME/cmdline-tools/cmdline-tools" "$ANDROID_HOME/cmdline-tools/latest" \
	&& rm /tmp/tools.zip \
	&& export JAVA_OPTS="$(java-proxy-opts)" \
	&& yes | "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" --licenses >/dev/null \
	&& "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" \
		"ndk;$NDK_VERSION" "platforms;android-36" "build-tools;36.0.0" >/dev/null
ENV ANDROID_NDK_HOME=$ANDROID_HOME/ndk/$NDK_VERSION ANDROID_NDK_ROOT=$ANDROID_HOME/ndk/$NDK_VERSION

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
COPY rust-toolchain.toml /tmp/toolchain/
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
		| sh -s -- -y --no-modify-path --profile minimal --default-toolchain none \
	&& cd /tmp/toolchain && rustup toolchain install \
	&& rustup target add --toolchain "$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)" \
		aarch64-linux-android x86_64-linux-android armv7-linux-androideabi \
	&& cargo install --locked cargo-ndk --version 4.1.2 \
	&& rm -rf "$CARGO_HOME/registry"

FROM toolchain AS build
# Comma-separated Android ABIs: arm64-v8a, x86_64, armeabi-v7a.
ARG ABIS=arm64-v8a,x86_64
# debug (installable as is) or release (signed with the build secrets).
ARG APK=debug
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=voelin-cargo-registry,target=/usr/local/cargo/registry \
	--mount=type=cache,id=voelin-target-android,target=/src/target \
	--mount=type=cache,id=voelin-gradle,target=/root/.gradle \
	--mount=type=secret,id=keystore,required=false \
	--mount=type=secret,id=signing,required=false \
	set -e; \
	if [ "$APK" = release ] && [ -f /run/secrets/keystore ]; then \
		export VOELIN_SIGNING_STORE_FILE=/run/secrets/keystore; \
		set -a; . /run/secrets/signing; set +a; \
	fi; \
	proxy="$(java-proxy-opts)"; \
	export GRADLE_OPTS="$proxy"; \
	task="assemble$(echo "$APK" | sed 's/./\U&/')"; \
	(cd android && ./gradlew --no-daemon $proxy "$task" -Pvoelin.abis="$ABIS") \
	&& scripts/package.sh android /out

FROM scratch AS artifacts
COPY --from=build /out /
