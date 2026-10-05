#!/usr/bin/env bash
# Package release builds into the files CI uploads and the Docker builds
# produce (docs/building.md). Build first, then:
#
#   scripts/package.sh linux   [out-dir]  from target/release:
#                                         voelin-<version>-linux-<arch>.tar.gz,
#                                         voelin_<version>_<arch>.deb (the app),
#                                         tsgw-<version>-linux-<arch>.tar.gz,
#                                         tsgw_<version>_<arch>.deb (the gateway,
#                                         for servers)
#   scripts/package.sh windows [out-dir]  voelin-<version>-windows-x86_64.zip and
#                                         voelin-<version>-setup.exe (NSIS)
#   scripts/package.sh android [out-dir]  the APKs Gradle built, renamed
#
# out-dir defaults to dist/. Each run also writes SHA256SUMS-<kind> there.
# voelinctl (a development tool) is in no package.
#
# Environment:
#   CARGO_TARGET_DIR  cargo's target directory (default: target)
#   WINDOWS_TARGET    the target triple the .exe files were built for, e.g.
#                     x86_64-pc-windows-gnu (default: none, target/release)
#   MAKENSIS          the NSIS compiler (default: makensis on PATH, then the
#                     default install location on Windows)
#   FFMPEG_DLL_DIR    FFmpeg's DLLs for the Windows packages, from
#                     scripts/fetch-ffmpeg-windows.sh (default: none; the
#                     packages then have no FFmpeg)
#
# The Linux .tar.gz carries libvpx and libdav1d (lib/, found through the
# binary's RUNPATH, set with patchelf) and bin/voelin-install-deps for what
# the system provides; the .deb depends on and recommends the system's.
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
	local v
	v=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$1/Cargo.toml" | head -n 1)
	[[ -n $v ]] || { echo "no version in $1/Cargo.toml" >&2; exit 1; }
	printf '%s\n' "$v"
}
version=$(crate_version crates/voelin-ui)

app_id=io.github.faumaray.Voelin
homepage=https://github.com/Faumaray/Voelin
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
files=()

# make_deb <package> <version> <depends suffix> <description…>: builds
# <package>_<version>_<arch>.deb from the tree in $tmp/debian/<package>, with
# Depends from its /usr/bin binaries (dpkg-shlibdeps) plus <depends suffix>.
# Extra control fields can be passed in $deb_extra.
make_deb() {
	local pkg=$1 ver=$2 extra_depends=$3 root=$tmp/debian/$1 deb_arch depends bins
	shift 3
	deb_arch=$(dpkg --print-architecture)
	mkdir -p "$tmp/debian"
	printf 'Source: %s\n\nPackage: %s\nArchitecture: any\n' "$pkg" "$pkg" >"$tmp/debian/control"
	bins=("$root"/usr/bin/*)
	depends=$(cd "$tmp" && dpkg-shlibdeps -O "${bins[@]#"$tmp/"}" | sed -n 's/^shlibs:Depends=//p')
	depends=${depends:+$depends${extra_depends:+, }}$extra_depends
	cat >"$root/usr/share/doc/$pkg/copyright" <<-EOF
		Upstream: $homepage
		License: MIT OR Apache-2.0
		Third-party licenses: /usr/share/doc/$pkg/THIRD_PARTY_NOTICES.md
	EOF
	mkdir -p "$root/DEBIAN"
	{
		# A pre-release sorts before its release in Debian with ~ (0.0.1~alpha).
		printf 'Package: %s\nVersion: %s\nArchitecture: %s\n' "$pkg" "${ver/-/\~}" "$deb_arch"
		printf 'Maintainer: Faumaray <23194470+Faumaray@users.noreply.github.com>\n'
		printf 'Section: net\nPriority: optional\nHomepage: %s\n' "$homepage"
		printf 'Depends: %s\n' "$depends"
		[[ -z ${deb_extra:-} ]] || printf '%s\n' "$deb_extra"
		printf 'Description: %s\n' "$1"
		shift
		printf ' %s\n' "$@"
	} >"$root/DEBIAN/control"
	dpkg-deb --root-owner-group --build "$root" "$out/${pkg}_${ver}_$deb_arch.deb" >/dev/null
	files+=("${pkg}_${ver}_$deb_arch.deb")
}

# Libraries of <exe> whose sonames differ between distributions (libvpx 7
# to 11, libdav1d 6 and 7), copied into <dir>; <exe> then finds them there
# (RUNPATH $ORIGIN/../lib) and the system's other libraries as usual.
bundle_libs() {
	local exe=$1 dir=$2 lib path
	if ! command -v patchelf >/dev/null; then
		echo "patchelf not found: the .tar.gz uses the system's libvpx and libdav1d" >&2
		return
	fi
	mkdir -p "$dir"
	while read -r lib path; do
		case $lib in
			libvpx.so.* | libdav1d.so.*) install -m644 "$path" "$dir/$lib" ;;
		esac
	done < <(ldd "$exe" | awk '$2 == "=>" && $3 ~ /^\// { print $1, $3 }')
	# shellcheck disable=SC2016 # $ORIGIN is for the dynamic linker.
	patchelf --set-rpath '$ORIGIN/../lib' "$exe"
}

package_linux() {
	local bin=$target_dir/release arch name stage root gw_version
	arch=$(uname -m)
	gw_version=$(crate_version crates/voelin-gateway)

	# The app. Archives use a prefix layout, e.g.
	# `tar -xzf … --strip-components=1 -C ~/.local`.
	name=voelin-$version-linux-$arch
	stage=$tmp/$name
	install -Dm755 "$bin/voelin" packaging/linux/voelin-install-deps -t "$stage/bin"
	install -Dm644 packaging/linux/$app_id.desktop -t "$stage/share/applications"
	install -Dm644 packaging/linux/$app_id.metainfo.xml -t "$stage/share/metainfo"
	install -Dm644 packaging/linux/$app_id.svg -t "$stage/share/icons/hicolor/scalable/apps"
	install -Dm644 THIRD_PARTY_NOTICES.md -t "$stage/share/doc/voelin"
	bundle_libs "$stage/bin/voelin" "$stage/lib"
	tar -C "$tmp" -czf "$out/$name.tar.gz" "$name"
	files+=("$name.tar.gz")

	# The gateway, for servers: nothing of the app, no GUI libraries.
	name=tsgw-$gw_version-linux-$arch
	stage=$tmp/$name
	install -Dm755 "$bin/tsgw" -t "$stage/bin"
	install -Dm644 packaging/linux/tsgw.service -t "$stage/lib/systemd/system"
	install -Dm644 THIRD_PARTY_NOTICES.md crates/voelin-gateway/tsgw.example.toml -t "$stage/share/doc/tsgw"
	tar -C "$tmp" -czf "$out/$name.tar.gz" "$name"
	files+=("$name.tar.gz")

	command -v dpkg-deb >/dev/null || { echo "dpkg-deb not found: skipping the .deb files" >&2; return; }

	root=$tmp/debian/voelin
	install -Dm755 "$bin/voelin" -t "$root/usr/bin"
	install -Dm644 packaging/linux/$app_id.desktop -t "$root/usr/share/applications"
	install -Dm644 packaging/linux/$app_id.metainfo.xml -t "$root/usr/share/metainfo"
	install -Dm644 packaging/linux/$app_id.svg -t "$root/usr/share/icons/hicolor/scalable/apps"
	install -Dm644 THIRD_PARTY_NOTICES.md -t "$root/usr/share/doc/voelin"
	# libxkbcommon, Wayland/X11 and EGL are loaded at run time (dlopen), so
	# dpkg-shlibdeps cannot see them; neither FFmpeg (hardware and extra
	# encoders and decoders) and libva (GPU colour conversion), nor what
	# screen sharing, Wayland hotkeys and saved passwords talk to.
	deb_extra="Recommends: libwayland-client0, libwayland-cursor0, libxkbcommon-x11-0, libx11-xcb1, libxcursor1, libxi6, libxrandr2, libegl1, libavcodec61 | libavcodec60 | libavcodec59 | libavcodec-extra, libavformat61 | libavformat60 | libavformat59, libva2, libva-drm2, mesa-va-drivers | va-driver-all | intel-media-va-driver, pipewire, xdg-desktop-portal, gnome-keyring | kwalletmanager | keepassxc" \
		make_deb voelin "$version" libxkbcommon0 \
		"Voelin, a TeamSpeak 3 and 6 client" \
		"Desktop client for TeamSpeak 3 and TeamSpeak 6 servers: voice, chat," \
		"screen sharing and invisible presence through a tsgw gateway."

	root=$tmp/debian/tsgw
	install -Dm755 "$bin/tsgw" -t "$root/usr/bin"
	install -Dm644 packaging/linux/tsgw.service -t "$root/usr/lib/systemd/system"
	install -Dm644 THIRD_PARTY_NOTICES.md -t "$root/usr/share/doc/tsgw"
	install -Dm644 crates/voelin-gateway/tsgw.example.toml -t "$root/usr/share/doc/tsgw/examples"
	make_deb tsgw "$gw_version" "" \
		"Voelin gateway for TeamSpeak servers" \
		"Runs next to a TeamSpeak 3 or 6 server and gives Voelin users invisible" \
		"presence and channel chat over ServerQuery. Configure /etc/tsgw/tsgw.toml" \
		"(example in /usr/share/doc/tsgw/examples), then systemctl enable --now tsgw."
}

# A path Windows programs understand when running on Windows (Git Bash):
# backslashes, since makensis's File finds nothing through "D:/a/…" paths.
native_path() {
	if command -v cygpath >/dev/null; then cygpath -w "$1"; else printf '%s\n' "$1"; fi
}

package_windows() {
	local bin=$target_dir/${WINDOWS_TARGET:+$WINDOWS_TARGET/}release name stage makensis
	name=voelin-$version-windows-x86_64
	stage=$tmp/$name
	mkdir -p "$stage"
	cp "$bin/voelin.exe" THIRD_PARTY_NOTICES.md "$stage/"
	# FFmpeg's DLLs next to voelin.exe, where the app looks for them first.
	local ffmpeg=()
	if [[ -n ${FFMPEG_DLL_DIR:-} ]]; then
		cp "$FFMPEG_DLL_DIR"/*.dll "$FFMPEG_DLL_DIR"/FFMPEG-*.txt "$stage/"
		ffmpeg=(-DFFMPEG="$(native_path "$(cd "$FFMPEG_DLL_DIR" && pwd)")")
	else
		echo "FFMPEG_DLL_DIR not set: the Windows packages have no FFmpeg" >&2
	fi
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
	"$makensis" -V2 -DVERSION="$version" -DVERSION_NUMERIC="${version%%-*}" \
		-DOUTDIR="$(native_path "$out")" "${ffmpeg[@]}" \
		-DBINARY="$(native_path "$(cd "$bin" && pwd)/voelin.exe")" packaging/windows/installer.nsi
	files+=("voelin-$version-setup.exe")
}

package_android() {
	local apk android_version variant found=0
	android_version=$(crate_version crates/voelin-android)
	for apk in android/app/build/outputs/apk/*/*.apk; do
		[[ -e $apk ]] || continue
		# app-debug.apk, app-release.apk, app-release-unsigned.apk
		variant=$(basename "$apk" .apk)
		variant=${variant#app-}
		cp "$apk" "$out/voelin-$android_version-android-$variant.apk"
		files+=("voelin-$android_version-android-$variant.apk")
		found=1
	done
	((found)) || { echo "no APKs in android/app/build/outputs/apk: build with Gradle first" >&2; exit 1; }
}

"package_$kind"

(cd "$out" && sha256sum "${files[@]}" >"SHA256SUMS-$kind")
printf '%s\n' "${files[@]/#/$out/}"
