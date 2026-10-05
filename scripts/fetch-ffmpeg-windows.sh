#!/usr/bin/env bash
# FFmpeg's shared libraries for the Windows packages: BtbN's LGPL build
# (no GPL parts: no x264, x265), whose DLLs the app loads at run time from
# its own directory (crates/voelin-media/src/ffmpeg/sys.rs). Without them a
# Windows install has no FFmpeg, so no hardware encoders and decoders.
#
#   scripts/fetch-ffmpeg-windows.sh <out-dir>
#
# Puts avcodec, avformat, avutil, swresample and swscale DLLs, FFmpeg's
# LICENSE.txt as FFMPEG-LICENSE.txt and FFMPEG-README.txt (version, source)
# into <out-dir>; scripts/package.sh windows ships them with FFMPEG_DLL_DIR.
# The archive is checked against the release's checksums.
#
# Environment:
#   FFMPEG_BRANCH  FFmpeg release branch (default 8.1: libavcodec 62, which
#                  voelin-media's loader and struct layouts support)
#   FFMPEG_TAG     BtbN release tag (default latest, rebuilt daily; set an
#                  autobuild-YYYY-MM-DD-HH-MM tag to pin a build)
set -euo pipefail

out=${1:?usage: $0 <out-dir>}
branch=${FFMPEG_BRANCH:-8.1}
tag=${FFMPEG_TAG:-latest}
name=ffmpeg-n$branch-latest-win64-lgpl-shared-$branch
base=https://github.com/BtbN/FFmpeg-Builds/releases/download/$tag

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -sSfL --retry 4 -o "$tmp/$name.zip" "$base/$name.zip"
curl -sSfL --retry 4 -o "$tmp/checksums.sha256" "$base/checksums.sha256"
expected=$(awk -v file="$name.zip" '$2 == file { print $1 }' "$tmp/checksums.sha256")
[[ -n $expected ]] || { echo "$name.zip is not in the release's checksums" >&2; exit 1; }
actual=$(sha256sum "$tmp/$name.zip" | cut -d' ' -f1)
[[ $actual == "$expected" ]] || { echo "checksum mismatch for $name.zip" >&2; exit 1; }

mkdir -p "$out"
# unzip, else 7-Zip or Python (Windows runners' Git Bash may lack unzip).
if command -v unzip >/dev/null; then
	(cd "$tmp" && unzip -q "$name.zip")
elif command -v 7z >/dev/null; then
	(cd "$tmp" && 7z x -bso0 "$name.zip")
else
	(cd "$tmp" && "$(command -v python3 || command -v python)" -m zipfile -e "$name.zip" .)
fi
dlls=()
for lib in avcodec avformat avutil swresample swscale; do
	found=("$tmp/$name/bin/$lib"-*.dll)
	[[ -e ${found[0]} ]] || { echo "no $lib DLL in $name.zip" >&2; exit 1; }
	cp "${found[@]}" "$out/"
	dlls+=("$(basename "${found[0]}")")
done
cp "$tmp/$name/LICENSE.txt" "$out/FFMPEG-LICENSE.txt"
cat >"$out/FFMPEG-README.txt" <<EOF
FFmpeg $branch (LGPL-2.1-or-later), shared libraries: ${dlls[*]}

Built by https://github.com/BtbN/FFmpeg-Builds ($name.zip, release $tag,
SHA-256 $actual). FFmpeg's source: https://ffmpeg.org/download.html and
https://git.ffmpeg.org/ffmpeg.git (branch release/$branch); the build scripts
and the exact versions of every library in the build are in the
FFmpeg-Builds repository. Voelin loads these DLLs at run time; replace them
with your own build of the same major versions to use another FFmpeg.
EOF
printf '%s\n' "${dlls[@]/#/$out/}"
