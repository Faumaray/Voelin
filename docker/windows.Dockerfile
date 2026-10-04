# syntax=docker/dockerfile:1
#
# Windows packages, cross-compiled on Linux (docs/building.md):
# voelin-<version>-windows-x86_64.zip with voelin.exe, and the NSIS installer
# voelin-<version>-setup.exe.
#
#   docker build -f docker/windows.Dockerfile -o dist/windows .
#
# or scripts/docker-build.sh windows. This builds for x86_64-pc-windows-gnu
# with mingw-w64; CI's Windows jobs build the MSVC target on Windows. Both run
# on Windows 10 and 11. Unsigned, like CI's (signing: docs/release.md).

FROM ubuntu:24.04 AS toolchain
ARG DEBIAN_FRONTEND=noninteractive
# mingw-w64 (C compiler and linker), NASM (aws-lc, libvpx and dav1d
# assembly), CMake (the bundled libopus, aws-lc), Meson and Ninja (dav1d),
# NSIS (the installer), zip and unzip.
RUN apt-get update && apt-get install -y --no-install-recommends \
		build-essential ca-certificates cmake curl git meson mingw-w64 nasm ninja-build nsis perl \
		pkg-config unzip zip \
	&& rm -rf /var/lib/apt/lists/* \
	&& update-alternatives --set x86_64-w64-mingw32-gcc /usr/bin/x86_64-w64-mingw32-gcc-posix \
	&& update-alternatives --set x86_64-w64-mingw32-g++ /usr/bin/x86_64-w64-mingw32-g++-posix
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
COPY rust-toolchain.toml /tmp/toolchain/
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
		| sh -s -- -y --no-modify-path --profile minimal --default-toolchain none \
	&& cd /tmp/toolchain && rustup toolchain install \
	&& rustup target add --toolchain "$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)" \
		x86_64-pc-windows-gnu

# libvpx as a static mingw library (CI's Windows jobs take it from vcpkg),
# from upstream's git at a pinned commit.
FROM toolchain AS libvpx
ARG LIBVPX_VERSION=1.15.2
ARG LIBVPX_COMMIT=d168454ecd099805c675d4a98c66f4891373302a
RUN git clone --quiet --depth 1 --branch "v$LIBVPX_VERSION" \
		https://chromium.googlesource.com/webm/libvpx /tmp/libvpx \
	&& test "$(git -C /tmp/libvpx rev-parse HEAD)" = "$LIBVPX_COMMIT" \
	&& cd /tmp/libvpx \
	&& CROSS=x86_64-w64-mingw32- ./configure --target=x86_64-win64-gcc --prefix=/opt/libvpx \
		--as=nasm --enable-static --disable-shared --enable-pic \
		--disable-examples --disable-tools --disable-docs --disable-unit-tests \
	&& make -j"$(nproc)" && make install \
	&& rm -rf /tmp/libvpx

# dav1d (AV1 decoding) as a static mingw library (CI's Windows jobs take it
# from vcpkg), from upstream's git at a pinned commit.
FROM toolchain AS dav1d
ARG DAV1D_VERSION=1.5.4
ARG DAV1D_COMMIT=54706fc6bc0cdecab7e9593974a4039cc038fca7
RUN git clone --quiet --depth 1 --branch "$DAV1D_VERSION" \
		https://code.videolan.org/videolan/dav1d.git /tmp/dav1d \
	&& test "$(git -C /tmp/dav1d rev-parse HEAD)" = "$DAV1D_COMMIT" \
	&& printf '%s\n' \
		"[binaries]" \
		"c = 'x86_64-w64-mingw32-gcc'" \
		"ar = 'x86_64-w64-mingw32-ar'" \
		"strip = 'x86_64-w64-mingw32-strip'" \
		"windres = 'x86_64-w64-mingw32-windres'" \
		"[host_machine]" \
		"system = 'windows'" \
		"cpu_family = 'x86_64'" \
		"cpu = 'x86_64'" \
		"endian = 'little'" >/tmp/mingw.ini \
	&& meson setup /tmp/dav1d/build /tmp/dav1d --cross-file /tmp/mingw.ini \
		--prefix=/opt/dav1d --libdir=lib --buildtype=release -Ddefault_library=static \
		-Denable_tools=false -Denable_tests=false -Denable_examples=false \
	&& ninja -C /tmp/dav1d/build install \
	&& rm -rf /tmp/dav1d

FROM toolchain AS build
COPY --from=libvpx /opt/libvpx /opt/libvpx
COPY --from=dav1d /opt/dav1d /opt/dav1d
ARG LIBVPX_VERSION=1.15.2
# libvpx-native-sys: the library above, linked statically; dav1d-sys
# through system-deps, without pkg-config.
ENV VPX_LIB_DIR=/opt/libvpx/lib VPX_INCLUDE_DIR=/opt/libvpx/include \
	VPX_VERSION=$LIBVPX_VERSION VPX_STATIC=1 \
	SYSTEM_DEPS_DAV1D_NO_PKG_CONFIG=1 SYSTEM_DEPS_DAV1D_SEARCH_NATIVE=/opt/dav1d/lib \
	SYSTEM_DEPS_DAV1D_LIB=dav1d SYSTEM_DEPS_DAV1D_LINK=static
WORKDIR /src
COPY . .
# FFmpeg's LGPL DLLs go into the packages (scripts/fetch-ffmpeg-windows.sh).
RUN --mount=type=cache,id=voelin-cargo-registry,target=/usr/local/cargo/registry \
	--mount=type=cache,id=voelin-target-windows,target=/src/target \
	cargo build --release --locked --target x86_64-pc-windows-gnu -p voelin-ui \
	&& scripts/fetch-ffmpeg-windows.sh /tmp/ffmpeg \
	&& FFMPEG_DLL_DIR=/tmp/ffmpeg WINDOWS_TARGET=x86_64-pc-windows-gnu scripts/package.sh windows /out

FROM scratch AS artifacts
COPY --from=build /out /
