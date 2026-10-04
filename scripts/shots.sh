#!/bin/sh
# Take a screenshot of the app with sample data, headless.
#
#   scripts/shots.sh <out.png> <VOELIN_OPEN> [<width>x<height>]
#
# The pictures in docs/screenshots/ are made this way (and reduced to 256
# colours with `convert -colors 256 PNG8:`).
#
# WAYLAND_DISPLAY is removed so the window opens on the Xvfb server and not
# on the desktop of a Wayland session (winit prefers Wayland when it can),
# and XDG_SESSION_TYPE so nothing picks the desktop portal for a Wayland
# session (the push-to-talk hotkey would ask the desktop to bind it).
# The screen is twice the window's size, so the pointer (in its middle) is
# not over the window and nothing shows as hovered. FFmpeg stays off: the
# pictures need no hardware encoders, and probing them can crash in some
# drivers. Extra arguments (`--set ui.theme=light`) come from $ARGS.
set -eu
out=$1
open=$2
size=${3:-1440x960}
w=${size%x*}
h=${size#*x}
bin=${VOELIN_BIN:-$CARGO_TARGET_DIR/debug/voelin}
env -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE \
	SLINT_BACKEND=winit-software \
	VOELIN_FFMPEG=0 \
	VOELIN_DATA_DIR="$(mktemp -d)" \
	VOELIN_DEMO_UI=1 \
	VOELIN_OPEN="$open" \
	VOELIN_WINDOW_SIZE="$size" \
	VOELIN_SCREENSHOT="$out" \
	VOELIN_SCREENSHOT_DELAY="${DELAY:-4}" \
	xvfb-run -a -s "-screen 0 $((w * 2))x$((h * 2))x24" "$bin" ${ARGS:-} >/dev/null 2>&1
