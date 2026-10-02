#!/bin/sh
# Take a screenshot of the app with sample data, headless.
#
#   scripts/shots.sh <out.png> <VOELIN_OPEN> [<width>x<height>]
#
# The pictures in docs/screenshots/ are made this way (and reduced to 256
# colours with `convert -colors 256 PNG8:`).
#
# WAYLAND_DISPLAY is removed so the window opens on the Xvfb server and not
# on the desktop of a Wayland session (winit prefers Wayland when it can).
set -eu
out=$1
open=$2
size=${3:-1440x960}
bin=${VOELIN_BIN:-$CARGO_TARGET_DIR/debug/voelin}
env -u WAYLAND_DISPLAY \
	SLINT_BACKEND=winit-software \
	VOELIN_DATA_DIR="$(mktemp -d)" \
	VOELIN_DEMO_UI=1 \
	VOELIN_OPEN="$open" \
	VOELIN_WINDOW_SIZE="$size" \
	VOELIN_SCREENSHOT="$out" \
	VOELIN_SCREENSHOT_DELAY="${DELAY:-4}" \
	xvfb-run -a -s "-screen 0 ${size}x24" "$bin" >/dev/null 2>&1
