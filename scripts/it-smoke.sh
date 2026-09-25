#!/usr/bin/env bash
# Integration smoke test: tsctl against the TS3 and TS6 servers from
# dev/docker-compose.yml.
#
# For each server it checks:
#   1. connect + channel tree (channelsubscribeall)
#   2. server chat between two clients
#   3. channel chat between two clients
#
# Usage: scripts/it-smoke.sh [ts3|ts6]...   (default: both)
# Env:   TSCTL=path/to/tsctl (default: builds target/debug/tsctl)
#        COMPOSE_FILE=dev/docker-compose.yml
set -euo pipefail

cd "$(dirname "$0")/.."
export RUST_BACKTRACE=0 RUST_LOG="${RUST_LOG:-error}"
COMPOSE_FILE="${COMPOSE_FILE:-dev/docker-compose.yml}"
STATE_DIR="target/it-smoke"
mkdir -p "$STATE_DIR"

if [[ -z "${TSCTL:-}" ]]; then
	cargo build --quiet -p tsctl
	TSCTL=target/debug/tsctl
fi

declare -A PORTS=([ts3]=9987 [ts6]=9988)
SERVERS=("$@")
[[ ${#SERVERS[@]} -eq 0 ]] && SERVERS=(ts3 ts6)

log() { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

compose_logs() { docker compose -f "$COMPOSE_FILE" logs --no-color "$1" 2>&1; }

# Wait until the server printed its ServerAdmin privilege key, then echo it.
privilege_key() {
	local svc=$1 key=""
	for _ in $(seq 1 60); do
		key=$(compose_logs "$svc" | grep -o 'token=[^ ]*' | head -1 | cut -d= -f2 || true)
		[[ -n "$key" ]] && break
		sleep 1
	done
	[[ -n "$key" ]] || fail "$svc did not print a privilege key"
	echo "$key"
}

# Prepare an identity in the server's admin group. The privilege key only works
# once, so the identity is kept in $STATE_DIR for later runs (`down -v` resets).
admin_args() {
	local svc=$1 addr=$2 id="$STATE_DIR/$svc-admin.json"
	if [[ ! -f "$id" ]]; then
		"$TSCTL" identity new --out "$id.tmp" --force >/dev/null
		"$TSCTL" connect "$addr" --identity "$id.tmp" --nick admin \
			--privilege-key "$(privilege_key "$svc")" tree >/dev/null
		mv "$id.tmp" "$id"
		# The rapid test connections would trip the per-IP anti-flood ban.
		"$TSCTL" connect "$addr" --identity "$id" --nick admin raw \
			"serveredit virtualserver_antiflood_points_needed_ip_block=100000 virtualserver_antiflood_points_needed_command_block=100000" >/dev/null
	fi
	echo "--identity $id"
}

# Start a listener, send one message, and check the listener received it.
chat_roundtrip() {
	local addr=$1 kind=$2 sender_args=$3
	local token="smoke-$kind-$RANDOM$RANDOM" out="$STATE_DIR/listen.log"
	"$TSCTL" connect "$addr" --nick listener listen --expect "$token" --timeout 20 >"$out" 2>&1 &
	local listener=$!
	# Give the listener time to connect before sending.
	for _ in $(seq 1 40); do grep -q "listener" "$out" 2>/dev/null && break; sleep 0.25; done
	# shellcheck disable=SC2086
	"$TSCTL" connect "$addr" --nick sender $sender_args chat "$kind" "$token"
	if ! wait "$listener"; then
		cat "$out"
		fail "$kind chat message not received"
	fi
	grep -F "[$kind] sender" "$out"
}

for svc in "${SERVERS[@]}"; do
	port=${PORTS[$svc]:?unknown server $svc}
	addr="127.0.0.1:$port"
	log "$svc ($addr)"

	admin=$(admin_args "$svc" "$addr")

	tree=$("$TSCTL" connect "$addr" --nick tree-check tree)
	echo "$tree"
	grep -q "tree-check" <<<"$tree" || fail "own client missing from tree"

	# Guests may not use the server chat by default, so the sender is the admin.
	chat_roundtrip "$addr" server "$admin"
	chat_roundtrip "$addr" channel ""
done

log "all smoke tests passed"
