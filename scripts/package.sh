#!/usr/bin/env bash
# Package release builds into the files CI uploads and the Docker builds
# produce (docs/building.md). Build first, then:
#
#   scripts/package.sh linux   [out-dir]  tsc-<version>-linux-<arch>.tar.gz and
#                                         tsc-desktop_<version>_<arch>.deb
#                                         from target/release
#   scripts/package.sh windows [out-dir]  tsc-<version>-windows-x86_64.zip and
#                                         tsc-desktop-<version>-setup.exe (NSIS)
#   scripts/package.sh android [out-dir]  the APKs Gradle built, renamed
#
# out-dir defaults to dist/. Each run also writes SHA256SUMS-<kind> there.
#
# Environment:
#   CARGO_TARGET_DIR  cargo's target directory (default: target)
#   WINDOWS_TARGET    the target triple the .exe files were built for, e.g.
#                     x86_64-pc-windows-gnu (default: none, target/release)
#   MAKENSIS          the NSIS compiler (default: makensis on PATH, then the
#                     default install location on Windows)
set -euo pipefail

cd "$(dirname "$0")/.."

kind=${1:-}
case "$kind" in
	linux | windows | android) ;;
	*) echo "usage: $0 linux|windows|android [out-dir]" >&2; exit 2 ;;
esac
mkdir -p "${2:-dist}"
out=$(cd "${2:-dist}" && pwd)
target_dir=${CARGO_TARGET_DIR:-target}

# The version of a crate, from its manifest.
crate_version() {
	sed -n 's/^version = "\(.*\)"$/\1/p' "$1/Cargo.toml" | head -n 1
}
version=$(crate_version crates/tsc-ui)
[[ -n $version ]] || { echo "no version in crates/tsc-ui/Cargo.toml" >&2; exit 1; }

app_id=io.github.faumaray.TsClient
bins=(tsc-desktop tsctl tsgw)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
files=()

package_linux() {
	local bin=$target_dir/release arch name stage pkg deb_arch depends
	arch=$(uname -m)
	name=tsc-$version-linux-$arch

	# Archive: a prefix layout, e.g. `tar -xzf … --strip-components=1 -C ~/.local`.
	stage=$tmp/$name
	for b in "${bins[@]}"; do
		install -Dm755 "$bin/$b" -t "$stage/bin"
	done
	install -Dm644 packaging/linux/$app_id.desktop -t "$stage/share/applications"
	install -Dm644 packaging/linux/$app_id.metainfo.xml -t "$stage/share/metainfo"
	install -Dm644 packaging/linux/$app_id.svg -t "$stage/share/icons/hicolor/scalable/apps"
	install -Dm644 THIRD_PARTY_NOTICES.md crates/tsc-gateway/tsgw.example.toml -t "$stage/share/doc/tsc"
	tar -C "$tmp" -czf "$out/$name.tar.gz" "$name"
	files+=("$name.tar.gz")

	# Debian package, with dependencies from the binaries' shared libraries.
	command -v dpkg-deb >/dev/null || { echo "dpkg-deb not found: skipping the .deb" >&2; return; }
	pkg=$tmp/debian/tsc-desktop
	for b in "${bins[@]}"; do
		install -Dm755 "$bin/$b" -t "$pkg/usr/bin"
	done
	install -Dm644 packaging/linux/$app_id.desktop -t "$pkg/usr/share/applications"
	install -Dm644 packaging/linux/$app_id.metainfo.xml -t "$pkg/usr/share/metainfo"
	install -Dm644 packaging/linux/$app_id.svg -t "$pkg/usr/share/icons/hicolor/scalable/apps"
	install -Dm644 THIRD_PARTY_NOTICES.md -t "$pkg/usr/share/doc/tsc-desktop"
	install -Dm644 crates/tsc-gateway/tsgw.example.toml -t "$pkg/usr/share/doc/tsc-desktop/examples"
	cat >"$pkg/usr/share/doc/tsc-desktop/copyright" <<-EOF
		Upstream: https://github.com/Faumaray/teamspeak_client_rs
		License: MIT OR Apache-2.0
		Third-party licenses: /usr/share/doc/tsc-desktop/THIRD_PARTY_NOTICES.md
	EOF
	deb_arch=$(dpkg --print-architecture)
	mkdir -p "$tmp/debian"
	printf 'Source: tsc-desktop\n\nPackage: tsc-desktop\nArchitecture: any\n' >"$tmp/debian/control"
	depends=$(cd "$tmp" && dpkg-shlibdeps -O "${bins[@]/#/debian/tsc-desktop/usr/bin/}" |
		sed -n 's/^shlibs:Depends=//p')
	# Loaded at run time (dlopen), so dpkg-shlibdeps cannot see them: the
	# keyboard, then Wayland or X11 and EGL for the window.
	depends=${depends:+$depends, }libxkbcommon0
	mkdir -p "$pkg/DEBIAN"
	cat >"$pkg/DEBIAN/control" <<-EOF
		Package: tsc-desktop
		Version: $version
		Architecture: $deb_arch
		Maintainer: Faumaray <23194470+Faumaray@users.noreply.github.com>
		Section: net
		Priority: optional
		Homepage: https://github.com/Faumaray/teamspeak_client_rs
		Depends: $depends
		Recommends: libwayland-client0, libwayland-cursor0, libxkbcommon-x11-0, libx11-xcb1, libxcursor1, libxi6, libxrandr2, libegl1
		Description: TeamSpeak 3 and 6 client
		 Desktop client for TeamSpeak 3 and TeamSpeak 6 servers (tsc-desktop), with
		 the tsctl command line client and the tsgw companion gateway.
	EOF
	dpkg-deb --root-owner-group --build "$pkg" "$out/tsc-desktop_${version}_$deb_arch.deb" >/dev/null
	files+=("tsc-desktop_${version}_$deb_arch.deb")
}

# A path makensis.exe understands when running on Windows (Git Bash).
native_path() {
	if command -v cygpath >/dev/null; then cygpath -m "$1"; else printf '%s\n' "$1"; fi
}

package_windows() {
	local bin=$target_dir/${WINDOWS_TARGET:+$WINDOWS_TARGET/}release name stage makensis
	name=tsc-$version-windows-x86_64
	stage=$tmp/$name
	mkdir -p "$stage"
	for b in "${bins[@]}"; do
		cp "$bin/$b.exe" "$stage/"
	done
	cp THIRD_PARTY_NOTICES.md crates/tsc-gateway/tsgw.example.toml "$stage/"
	if command -v zip >/dev/null; then
		(cd "$tmp" && zip -qr "$out/$name.zip" "$name")
	elif command -v 7z >/dev/null; then
		(cd "$tmp" && 7z a -tzip -bso0 "$(native_path "$out/$name.zip")" "$name")
	else
		powershell -NoProfile -Command \
			"Compress-Archive -Path '$(native_path "$stage")' -DestinationPath '$(native_path "$out/$name.zip")'"
	fi
	files+=("$name.zip")

	makensis=${MAKENSIS:-$(command -v makensis || true)}
	if [[ -z $makensis && -x "/c/Program Files (x86)/NSIS/makensis.exe" ]]; then
		makensis="/c/Program Files (x86)/NSIS/makensis.exe"
	fi
	if [[ -z $makensis ]]; then
		echo "makensis not found: skipping the installer" >&2
		return
	fi
	# Absolute paths: makensis changes into the script's directory.
	"$makensis" -V2 -DVERSION="$version" -DOUTDIR="$(native_path "$out")" \
		-DBINARY="$(native_path "$(cd "$bin" && pwd)/tsc-desktop.exe")" packaging/windows/installer.nsi
	files+=("tsc-desktop-$version-setup.exe")
}

package_android() {
	local apk android_version found=0
	android_version=$(crate_version crates/tsc-android)
	for apk in android/app/build/outputs/apk/*/*.apk; do
		[[ -e $apk ]] || continue
		# app-debug.apk, app-release.apk, app-release-unsigned.apk
		local variant
		variant=$(basename "$apk" .apk)
		variant=${variant#app-}
		cp "$apk" "$out/tsc-$android_version-android-$variant.apk"
		files+=("tsc-$android_version-android-$variant.apk")
		found=1
	done
	((found)) || { echo "no APKs in android/app/build/outputs/apk: build with Gradle first" >&2; exit 1; }
}

"package_$kind"

(cd "$out" && sha256sum "${files[@]}" >"SHA256SUMS-$kind")
printf '%s\n' "${files[@]/#/$out/}"
