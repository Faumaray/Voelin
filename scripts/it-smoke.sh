#!/usr/bin/env bash
# Integration smoke test: tsctl against the TS3 and TS6 servers from
# dev/docker-compose.yml.
#
# For each server it checks:
#   1. connect + channel tree (channelsubscribeall)
#   2. server chat between two clients
#   3. channel chat between two clients
#   4. voice: a 1 kHz tone sent by one client is recorded by another
#   5. invisible presence: a ServerQuery observer sees a voice client join
#   6. relay chat: a query relay reads channel chat and posts into the channel
#   7. gateway (tsgw): login with a TeamSpeak identity, presence, channel chat
#      both ways, refusal of an identity the server does not know
#
# Usage: scripts/it-smoke.sh [ts3|ts6]...   (default: both)
# Env:   TSCTL=path/to/tsctl (default: builds target/debug/tsctl)
#        COMPOSE_FILE=dev/docker-compose.yml
set -euo pipefail

cd "$(dirname "$0")/.."
export RUST_BACKTRACE=0 RUST_LOG="${RUST_LOG:-error}"
COMPOSE_FILE="${COMPOSE_FILE:-dev/docker-compose.yml}"
# Survives `cargo clean`: holds the admin identity that redeemed the one-time key.
STATE_DIR="${SMOKE_STATE_DIR:-dev/.state}"
mkdir -p "$STATE_DIR"
# Never leave background clients behind.
trap 'kill $(jobs -p) 2>/dev/null || true' EXIT

if [[ -z "${TSCTL:-}" ]]; then
	cargo build --quiet -p tsctl -p tsc-gateway
	TSCTL=target/debug/tsctl
fi
TSGW="${TSGW:-target/debug/tsgw}"

declare -A PORTS=([ts3]=9987 [ts6]=9988)
# ServerQuery transport and address per server (dev/docker-compose.yml).
declare -A QUERY=([ts3]="raw 127.0.0.1:10011" [ts6]="ssh 127.0.0.1:10022")
declare -A GATEWAY=([ts3]=7787 [ts6]=7788)
QUERY_SECRET=tsc-dev-admin
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
		if ! "$TSCTL" connect "$addr" --identity "$id.tmp" --nick admin \
			--privilege-key "$(privilege_key "$svc")" tree >/dev/null; then
			fail "could not redeem the $svc privilege key (already used?). Reset the servers:
  docker compose -f $COMPOSE_FILE down -v && docker compose -f $COMPOSE_FILE up -d && rm -rf $STATE_DIR"
		fi
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

# One client records while another sends a tone; Goertzel checks the result.
voice_roundtrip() {
	local addr=$1 out="$STATE_DIR/voice.log"
	"$TSCTL" connect "$addr" --nick recorder voice record "$STATE_DIR/voice.wav" \
		--seconds 6 --expect-tone 1000 >"$out" 2>&1 &
	local recorder=$!
	sleep 2
	"$TSCTL" connect "$addr" --nick talker voice send --tone 1000 --seconds 2 >/dev/null
	if ! wait "$recorder"; then
		cat "$out"
		fail "voice tone not received"
	fi
	grep "tone ratio" "$out"
}

# An observer over ServerQuery must see a voice client join.
presence_check() {
	local addr=$1 query=$2 out="$STATE_DIR/observe.log"
	# shellcheck disable=SC2086
	"$TSCTL" observe $query --secret "$QUERY_SECRET" --allowlisted --poll 2 \
		--seconds 20 --expect-client presence-probe >"$out" 2>&1 &
	local observer=$!
	sleep 2
	"$TSCTL" connect "$addr" --nick presence-probe listen --timeout 4 >/dev/null 2>&1 || true
	if ! wait "$observer"; then
		cat "$out"
		fail "observer did not see the voice client"
	fi
	grep "presence-probe" "$out"
}

# A relay reads a voice client's channel message and posts one back.
relay_check() {
	local addr=$1 query=$2 out="$STATE_DIR/relay.log" heard="$STATE_DIR/relay-heard.log"
	local token="relay-$RANDOM$RANDOM"
	# shellcheck disable=SC2086
	"$TSCTL" relay $query --secret "$QUERY_SECRET" --allowlisted --channel 1 \
		--expect "to-relay-$token" --seconds 20 >"$out" 2>&1 &
	local relay=$!
	"$TSCTL" connect "$addr" --nick relay-listener listen --expect "from-relay-$token" \
		--timeout 20 >"$heard" 2>&1 &
	local listener=$!
	sleep 4
	"$TSCTL" connect "$addr" --nick relay-talker chat channel "to-relay-$token"
	# shellcheck disable=SC2086
	"$TSCTL" relay $query --secret "$QUERY_SECRET" --allowlisted --channel 1 --nick Alice \
		--send "from-relay-$token" --seconds 1 >/dev/null
	if ! wait "$relay"; then
		cat "$out"
		fail "relay did not receive the channel message"
	fi
	if ! wait "$listener"; then
		cat "$heard"
		fail "voice client did not receive the relayed post"
	fi
	grep "to-relay-$token" "$out"
	grep "from-relay-$token" "$heard"
}

# Users of the gateway: presence, chat both ways, refusal of unknown identities.
gateway_check() {
	local svc=$1 addr=$2 port=${GATEWAY[$svc]} out="$STATE_DIR/gateway.log"
	local url="ws://127.0.0.1:$port/v1" token="gw-$RANDOM$RANDOM"
	"$TSGW" --config "dev/tsgw-$svc.toml" >"$STATE_DIR/tsgw-$svc.log" 2>&1 &
	local gateway=$!
	for _ in $(seq 1 40); do
		grep -q "listening" "$STATE_DIR/tsgw-$svc.log" 2>/dev/null && break
		sleep 0.25
	done

	# The gateway only knows identities that connected with voice once.
	local user="$STATE_DIR/$svc-gateway-user.json" stranger="$STATE_DIR/stranger.json"
	[[ -f "$user" ]] || "$TSCTL" identity new --out "$user" >/dev/null
	"$TSCTL" connect "$addr" --identity "$user" --nick gateway-user tree >/dev/null
	"$TSCTL" identity new --out "$stranger" --force >/dev/null
	if "$TSCTL" gateway "$url" --identity "$stranger" --seconds 3 >/dev/null 2>&1; then
		fail "gateway accepted an identity the server does not know"
	fi

	"$TSCTL" gateway "$url" --identity "$user" --presence --open channel:1 \
		--expect "to-gateway-$token" --seconds 20 >"$out" 2>&1 &
	local reader=$!
	"$TSCTL" connect "$addr" --nick gateway-listener listen --expect "from-gateway-$token" \
		--timeout 20 >"$STATE_DIR/gateway-heard.log" 2>&1 &
	local listener=$!
	sleep 4
	"$TSCTL" connect "$addr" --nick gateway-talker chat channel "to-gateway-$token"
	"$TSCTL" gateway "$url" --identity "$user" --send "channel:1=from-gateway-$token" --seconds 3 >/dev/null
	if ! wait "$reader"; then
		cat "$out" "$STATE_DIR/tsgw-$svc.log"
		fail "gateway user did not receive the channel message"
	fi
	if ! wait "$listener"; then
		cat "$STATE_DIR/gateway-heard.log"
		fail "voice client did not receive the gateway user's post"
	fi
	grep -E "presence:|to-gateway-$token" "$out"
	grep "from-gateway-$token" "$STATE_DIR/gateway-heard.log"
	kill "$gateway"
	wait "$gateway" 2>/dev/null || true
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
	voice_roundtrip "$addr"
	presence_check "$addr" "${QUERY[$svc]}"
	relay_check "$addr" "${QUERY[$svc]}"
	gateway_check "$svc" "$addr"
done

log "engine (tsc-core) against both servers"
TSC_LIVE=1 cargo test --quiet -p tsc-core --test live 2>&1 | grep -E "test result|panicked|timed out" || fail "engine tests failed"

log "stream (tsc-stream) through the TeamSpeak 6 server"
TSC_LIVE=1 cargo test --quiet -p tsc-stream --test live_ts6 2>&1 | grep -E "test result|panicked|timed out" || fail "stream test failed"

log "all smoke tests passed"
