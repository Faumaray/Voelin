#!/usr/bin/env bash
# Integration smoke test: voelinctl against the TS3 and TS6 servers from
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
#   8. chat history through the engine (voelinctl gateway --engine): an admin
#      sets a runtime gateway setting; messages sent while a user is offline
#      reach the user's stored history when it connects again
#   9. pins, reactions and topics through the engine: post, pin (allowed by
#      a permission rule the admin sets), react, start a topic from the post,
#      post into it, read the topic and the pins back
#  10. stream directory (TeamSpeak 6 only): an engine session's stream
#      registers itself in the gateway's directory; a viewer that connects
#      later finds it there and watches it
#  11. stream (TeamSpeak 6 only): a viewer watches a synthetic VP8 + Opus stream
#  12. late stream (TeamSpeak 6 only): a viewer that connects after the stream
#      started finds it (requeststreaminfo) and watches it
#  13. the engine's voice features (voelinctl engine): upload a file and
#      post its link in chat, another client lists the channel's files and
#      downloads the linked file (compared byte for byte), the file is
#      deleted; an avatar set by one client is fetched by another (MD5
#      checked); poke, private message and offline message round trips; a
#      friend is seen online; a blocked contact's poke arrives flagged
#
# Usage: scripts/it-smoke.sh [ts3|ts6]...   (default: both)
# Env:   VOELINCTL=path/to/voelinctl, TSGW=path/to/tsgw (default: builds both
#        into $CARGO_TARGET_DIR or target)
#        TSGW_PORT_TS3, TSGW_PORT_TS6: gateway ports (default 7787, 7788)
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

BUILD_DIR="${CARGO_TARGET_DIR:-target}/debug"
if [[ -z "${VOELINCTL:-}" ]]; then
	cargo build --quiet -p voelinctl -p voelin-gateway
	VOELINCTL=$BUILD_DIR/voelinctl
fi
TSGW="${TSGW:-$BUILD_DIR/tsgw}"

declare -A PORTS=([ts3]=9987 [ts6]=9988)
# ServerQuery transport and address per server (dev/docker-compose.yml).
declare -A QUERY=([ts3]="raw 127.0.0.1:10011" [ts6]="ssh 127.0.0.1:10022")
declare -A GATEWAY=([ts3]=${TSGW_PORT_TS3:-7787} [ts6]=${TSGW_PORT_TS6:-7788})
QUERY_SECRET=voelin-dev-admin
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
		"$VOELINCTL" identity new --out "$id.tmp" --force >/dev/null
		if ! "$VOELINCTL" connect "$addr" --identity "$id.tmp" --nick admin \
			--privilege-key "$(privilege_key "$svc")" tree >/dev/null; then
			fail "could not redeem the $svc privilege key (already used?). Reset the servers:
  docker compose -f $COMPOSE_FILE down -v && docker compose -f $COMPOSE_FILE up -d && rm -rf $STATE_DIR"
		fi
		mv "$id.tmp" "$id"
		# The rapid test connections would trip the per-IP anti-flood ban.
		"$VOELINCTL" connect "$addr" --identity "$id" --nick admin raw \
			"serveredit virtualserver_antiflood_points_needed_ip_block=100000 virtualserver_antiflood_points_needed_command_block=100000" >/dev/null
	fi
	echo "--identity $id"
}

# Start a listener, send one message, and check the listener received it.
chat_roundtrip() {
	local addr=$1 kind=$2 sender_args=$3
	local token="smoke-$kind-$RANDOM$RANDOM" out="$STATE_DIR/listen.log"
	"$VOELINCTL" connect "$addr" --nick listener listen --expect "$token" --timeout 20 >"$out" 2>&1 &
	local listener=$!
	# Give the listener time to connect before sending.
	for _ in $(seq 1 40); do grep -q "listener" "$out" 2>/dev/null && break; sleep 0.25; done
	# shellcheck disable=SC2086
	"$VOELINCTL" connect "$addr" --nick sender $sender_args chat "$kind" "$token"
	if ! wait "$listener"; then
		cat "$out"
		fail "$kind chat message not received"
	fi
	grep -F "[$kind] sender" "$out"
}

# One client records while another sends a tone; Goertzel checks the result.
voice_roundtrip() {
	local addr=$1 out="$STATE_DIR/voice.log"
	"$VOELINCTL" connect "$addr" --nick recorder voice record "$STATE_DIR/voice.wav" \
		--seconds 6 --expect-tone 1000 >"$out" 2>&1 &
	local recorder=$!
	sleep 2
	"$VOELINCTL" connect "$addr" --nick talker voice send --tone 1000 --seconds 2 >/dev/null
	if ! wait "$recorder"; then
		cat "$out"
		fail "voice tone not received"
	fi
	grep "tone ratio" "$out"
}

# TeamSpeak 6: a viewer waits for a stream, a streamer sends synthetic frames.
stream_check() {
	local addr=$1 out="$STATE_DIR/stream-watch.log" streamer_out="$STATE_DIR/stream-start.log"
	# Unique nicknames: other clients may stream on the same server.
	local streamer="streamer-$$-$RANDOM"
	"$VOELINCTL" connect "$addr" --nick "viewer-$$" stream --loopback watch --streamer-nick "$streamer" \
		--expect-frames 60 --timeout 30 >"$out" 2>&1 &
	local viewer=$!
	sleep 2
	if ! "$VOELINCTL" connect "$addr" --nick "$streamer" stream --loopback start --synthetic \
		--auto-accept --seconds 10 >"$streamer_out" 2>&1; then
		cat "$streamer_out" "$out"
		fail "streamer failed"
	fi
	if ! wait "$viewer"; then
		cat "$out" "$streamer_out"
		fail "stream frames not received"
	fi
	grep -E "first video frame|received" "$out"
}

# TeamSpeak 6: the stream is live before the viewer connects. The server does
# not announce running streams to newcomers; the viewer looks it up.
stream_late_check() {
	local addr=$1 out="$STATE_DIR/stream-late-watch.log" streamer_out="$STATE_DIR/stream-late-start.log"
	local streamer="early-$$-$RANDOM"
	"$VOELINCTL" connect "$addr" --nick "$streamer" stream --loopback start --synthetic \
		--auto-accept --seconds 30 >"$streamer_out" 2>&1 &
	local streamer_pid=$!
	for _ in $(seq 1 60); do grep -q "is live" "$streamer_out" 2>/dev/null && break; sleep 0.25; done
	grep -q "is live" "$streamer_out" || { cat "$streamer_out"; fail "late-join streamer did not go live"; }
	if ! "$VOELINCTL" connect "$addr" --nick "late-$$" stream --loopback watch --streamer-nick "$streamer" \
		--expect-frames 60 --timeout 25 >"$out" 2>&1; then
		cat "$out" "$streamer_out"
		fail "late viewer did not receive the running stream"
	fi
	# SIGINT: the streamer stops its stream properly.
	kill -INT "$streamer_pid" 2>/dev/null || true
	wait "$streamer_pid" 2>/dev/null || true
	grep -E "watching|received" "$out"
}

# An observer over ServerQuery must see a voice client join.
presence_check() {
	local addr=$1 query=$2 out="$STATE_DIR/observe.log"
	# shellcheck disable=SC2086
	"$VOELINCTL" observe $query --secret "$QUERY_SECRET" --allowlisted --poll 2 \
		--seconds 20 --expect-client presence-probe >"$out" 2>&1 &
	local observer=$!
	sleep 2
	"$VOELINCTL" connect "$addr" --nick presence-probe listen --timeout 4 >/dev/null 2>&1 || true
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
	"$VOELINCTL" relay $query --secret "$QUERY_SECRET" --allowlisted --channel 1 \
		--expect "to-relay-$token" --seconds 20 >"$out" 2>&1 &
	local relay=$!
	"$VOELINCTL" connect "$addr" --nick relay-listener listen --expect "from-relay-$token" \
		--timeout 20 >"$heard" 2>&1 &
	local listener=$!
	sleep 4
	"$VOELINCTL" connect "$addr" --nick relay-talker chat channel "to-relay-$token"
	# shellcheck disable=SC2086
	"$VOELINCTL" relay $query --secret "$QUERY_SECRET" --allowlisted --channel 1 --nick Alice \
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
	"$TSGW" --config "dev/tsgw-$svc.toml" --set "listen.bind=127.0.0.1:$port" \
		--set "history.path=$STATE_DIR/tsgw-$svc.db" >"$STATE_DIR/tsgw-$svc.log" 2>&1 &
	local gateway=$!
	for _ in $(seq 1 40); do
		grep -q "listening" "$STATE_DIR/tsgw-$svc.log" 2>/dev/null && break
		sleep 0.25
	done

	# The gateway only knows identities that connected with voice once.
	local user="$STATE_DIR/$svc-gateway-user.json" stranger="$STATE_DIR/stranger.json"
	[[ -f "$user" ]] || "$VOELINCTL" identity new --out "$user" >/dev/null
	"$VOELINCTL" connect "$addr" --identity "$user" --nick gateway-user tree >/dev/null
	"$VOELINCTL" identity new --out "$stranger" --force >/dev/null
	if "$VOELINCTL" gateway "$url" --identity "$stranger" --seconds 3 >/dev/null 2>&1; then
		fail "gateway accepted an identity the server does not know"
	fi

	"$VOELINCTL" gateway "$url" --identity "$user" --presence --open channel:1 \
		--expect "to-gateway-$token" --seconds 20 >"$out" 2>&1 &
	local reader=$!
	"$VOELINCTL" connect "$addr" --nick gateway-listener listen --expect "from-gateway-$token" \
		--timeout 20 >"$STATE_DIR/gateway-heard.log" 2>&1 &
	local listener=$!
	sleep 4
	"$VOELINCTL" connect "$addr" --nick gateway-talker chat channel "to-gateway-$token"
	"$VOELINCTL" gateway "$url" --identity "$user" --send "channel:1=from-gateway-$token" --seconds 3 >/dev/null
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

	history_check "$svc" "$addr" "$url" "$user"
	features_check "$svc" "$url" "$user"
	if [[ $svc == ts6 ]]; then
		directory_check "$svc" "$addr" "$url" "$user"
	fi
	kill "$gateway"
	wait "$gateway" 2>/dev/null || true
}

# An engine session through the gateway: voelinctl gateway --engine.
engine() {
	local url=$1 identity=$2
	shift 2
	"$VOELINCTL" gateway "$url" --identity "$identity" --engine "$@"
}

# A gateway request as the server's admin; the answer must contain $expect.
admin_request() {
	local svc=$1 url=$2 request=$3 expect=$4 out="$STATE_DIR/admin-request.log"
	if ! engine "$url" "$STATE_DIR/$svc-admin.json" --request "$request" --expect "$expect" \
		--seconds 15 >"$out" 2>&1; then
		cat "$out"
		fail "gateway request $request was not answered with $expect"
	fi
	grep -F "$expect" "$out" | cut -c1-200
}

# Messages sent while a user is offline reach the user's stored history
# through the gateway when the user connects again.
history_check() {
	local svc=$1 addr=$2 url=$3 user=$4 token="offline-$RANDOM$RANDOM"
	local db="$STATE_DIR/$svc-history-$$.db" out="$STATE_DIR/history.log"
	rm -f "$db"*
	# The gateway reads channel 1 even while nobody has it open (runtime setting).
	admin_request "$svc" "$url" '{"config_set":{"key":"relay.pinned_channels","value":[1]}}' \
		'"config_value":{"entry":{"key":"relay.pinned_channels","value":[1]'
	# First visit: the chat is synced and stored.
	if ! engine "$url" "$user" --db "$db" --chat channel:1 --expect "history gateway channel:1 batch" \
		--seconds 15 >"$out" 2>&1; then
		cat "$out"
		fail "first chat history sync failed"
	fi
	# Away: someone talks in the channel.
	sleep 2
	"$VOELINCTL" connect "$addr" --nick history-talker chat channel "$token-1"
	"$VOELINCTL" connect "$addr" --nick history-talker chat channel "$token-2"
	sleep 1
	# Back: the stored history at once, then what was missed, from the gateway.
	if ! engine "$url" "$user" --db "$db" --chat channel:1 --expect "$token-2" \
		--seconds 15 >"$out" 2>&1; then
		cat "$out" "$STATE_DIR/tsgw-$svc.log"
		fail "messages sent while offline did not arrive"
	fi
	grep -E "^history gateway channel:1 .*$token-1" "$out" || { cat "$out"; fail "$token-1 not from the gateway"; }
	grep -E "^history gateway channel:1 .*$token-2" "$out" || { cat "$out"; fail "$token-2 not from the gateway"; }
	# And stored: the next visit shows them from the database first.
	if ! engine "$url" "$user" --db "$db" --chat channel:1 --expect "history gateway channel:1 batch" \
		--seconds 15 >"$out" 2>&1; then
		cat "$out"
		fail "third chat history sync failed"
	fi
	grep -E "^history local channel:1 .*$token-2" "$out" || { cat "$out"; fail "$token-2 was not stored"; }
	admin_request "$svc" "$url" '{"config_reset":{"key":"relay.pinned_channels"}}' '"config_value"' >/dev/null
	rm -f "$db"*
}

# Post, pin, react, topic and topic post through the engine as a normal user;
# pinning is allowed by a permission rule the admin sets for the test.
features_check() {
	local svc=$1 url=$2 user=$3 out="$STATE_DIR/roundtrip.log"
	admin_request "$svc" "$url" '{"perm_set":{"action":"pin","rule":{"everyone":true}}}' '"perm_rules"' >/dev/null
	if ! engine "$url" "$user" --roundtrip channel:1 --seconds 20 >"$out" 2>&1; then
		cat "$out"
		admin_request "$svc" "$url" '{"perm_reset":{"action":"pin"}}' '"perm_rules"' >/dev/null || true
		fail "pin/react/topic round trip failed"
	fi
	admin_request "$svc" "$url" '{"perm_reset":{"action":"pin"}}' '"perm_rules"' >/dev/null
	grep -E "^roundtrip|\"pinned\":\{|\"reaction\":\{" "$out" | cut -c1-160
}

# TeamSpeak 6: an engine session streams; its stream registers in the
# gateway's directory; a viewer that connects later finds it and watches it.
directory_check() {
	local svc=$1 addr=$2 url=$3 user=$4 streamer="dir-$$-$RANDOM" title="directory-$RANDOM"
	local out="$STATE_DIR/directory-stream.log" viewer_out="$STATE_DIR/directory-watch.log"
	engine "$url" "$STATE_DIR/$svc-admin.json" --voice "$addr" --nick "$streamer" --loopback \
		--stream-seconds 30 --stream-title "$title" --seconds 45 >"$out" 2>&1 &
	local streamer_pid=$!
	for _ in $(seq 1 80); do grep -q '"stream_registered"' "$out" 2>/dev/null && break; sleep 0.25; done
	grep -q '"stream_registered"' "$out" || { cat "$out"; fail "the stream did not register in the directory"; }
	if ! engine "$url" "$user" --voice "$addr" --nick "late-dir-$$" --loopback \
		--watch-streamer "$streamer" --expect-frames 30 --seconds 25 >"$viewer_out" 2>&1; then
		cat "$viewer_out" "$out"
		fail "late viewer did not watch the stream from the directory"
	fi
	grep -qF "\"title\":\"$title\"" "$viewer_out" || { cat "$viewer_out"; fail "the directory did not list the stream"; }
	kill -INT "$streamer_pid" 2>/dev/null || true
	wait "$streamer_pid" 2>/dev/null || true
	grep -E '"stream_registered"' "$out" | cut -c1-160
	grep -E "^watching|^received" "$viewer_out"
}

# A voice session through the engine: voelinctl engine <addr> <args>.
eng() {
	local addr=$1
	shift
	"$VOELINCTL" engine "$addr" "$@"
}

# The unique id the server knows an identity by (TeamSpeak 6: another hash).
server_uid() {
	local svc=$1 id=$2 field=uid
	[[ $svc == ts6 ]] && field=uid6
	"$VOELINCTL" identity show --identity "$id" | awk -v f="$field:" '$1 == f { print $2 }'
}

# Run a background listener, then the action; both must succeed.
roundtrip() {
	local what=$1 listen_log=$2 listener_cmd=$3 action_cmd=$4
	eval "$listener_cmd" >"$listen_log" 2>&1 &
	local listener=$!
	sleep 3
	if ! eval "$action_cmd" >"$STATE_DIR/action.log" 2>&1; then
		cat "$STATE_DIR/action.log" "$listen_log"
		fail "$what: the action failed"
	fi
	if ! wait "$listener"; then
		cat "$listen_log" "$STATE_DIR/action.log"
		fail "$what: not received"
	fi
}

# Files, avatars, pokes, private and offline messages, contacts through the
# engine (voelinctl engine).
engine_voice_check() {
	local svc=$1 addr=$2 admin_id="$STATE_DIR/$svc-admin.json"
	local user="$STATE_DIR/voice-user.json" other="$STATE_DIR/voice-other.json"
	[[ -f "$user" ]] || "$VOELINCTL" identity new --out "$user" >/dev/null
	[[ -f "$other" ]] || "$VOELINCTL" identity new --out "$other" >/dev/null
	local t="$$-$RANDOM" dir="$STATE_DIR/files-$svc"
	rm -rf "$dir" && mkdir -p "$dir/in"

	# Files: the admin uploads (guests may not) and links the file in chat;
	# a reader downloads it from the link; a guest lists it; it is deleted.
	head -c 1500000 /dev/urandom >"$dir/smoke-$t.bin"
	roundtrip "file link" "$dir/reader.log" \
		"eng $addr --nick file-reader --seconds 25 listen --fetch-links $dir/in" \
		"eng $addr --identity $admin_id --nick file-sharer --seconds 20 files upload $dir/smoke-$t.bin --share --overwrite"
	cmp "$dir/smoke-$t.bin" "$dir/in/smoke-$t.bin" || fail "the downloaded file differs"
	grep -E "^file link|^transfer 7 done" "$dir/reader.log"
	if ! eng "$addr" --nick file-lister --seconds 20 files ls --channel 1 --path / \
		--expect "smoke-$t.bin" >"$dir/ls.log" 2>&1; then
		cat "$dir/ls.log"
		fail "the file is not listed"
	fi
	grep "smoke-$t.bin" "$dir/ls.log"
	eng "$addr" --identity "$admin_id" --nick file-sharer --seconds 20 files rm --channel 1 \
		"/smoke-$t.bin" >/dev/null || fail "could not delete the file"

	# Avatar: set by one client (kept by the server), fetched by another.
	printf '%s' 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==' |
		base64 -d >"$dir/avatar.png"
	local md5
	md5=$(md5sum "$dir/avatar.png" | cut -d' ' -f1)
	eng "$addr" --identity "$user" --nick avatar-owner-$t --seconds 20 avatar set "$dir/avatar.png" \
		>"$dir/avatar-set.log" 2>&1 || { cat "$dir/avatar-set.log"; fail "could not set the avatar"; }
	roundtrip "avatar" "$dir/avatar-owner.log" \
		"eng $addr --identity $user --nick avatar-owner-$t --seconds 10 listen --expect never-$t || true" \
		"eng $addr --nick avatar-fetcher --seconds 12 avatar wait --nick avatar-owner-$t --md5 $md5"
	grep "^avatar avatar-owner-$t" "$STATE_DIR/action.log"

	# Poke and private message from one guest to another.
	roundtrip "poke" "$dir/poke.log" \
		"eng $addr --identity $user --nick poke-target-$t --seconds 15 listen --expect poke-$t" \
		"eng $addr --nick poker --seconds 10 poke --to poke-target-$t --message poke-$t"
	grep "^poke from" "$dir/poke.log"
	roundtrip "private message" "$dir/dm.log" \
		"eng $addr --identity $user --nick dm-target-$t --seconds 15 listen --expect dm-$t" \
		"eng $addr --nick dm-sender --seconds 10 dm --to dm-target-$t --message dm-$t"
	grep "^chat private" "$dir/dm.log"

	# Offline message: the admin writes (guests may not), the user reads
	# and deletes it.
	local uid
	uid=$(server_uid "$svc" "$user")
	eng "$addr" --identity "$admin_id" --nick mailer --seconds 15 offline send --to-uid "$uid" \
		--subject "smoke $t" --message "offline-$t" >"$dir/offline-send.log" 2>&1 ||
		{ cat "$dir/offline-send.log"; fail "could not send the offline message"; }
	if ! eng "$addr" --identity "$user" --nick mail-reader --seconds 15 offline read \
		--expect "offline-$t" --delete >"$dir/offline-read.log" 2>&1; then
		cat "$dir/offline-read.log"
		fail "the offline message did not arrive"
	fi
	grep "offline-$t" "$dir/offline-read.log"

	# Contacts: a friend seen online; a blocked contact's poke flagged.
	local db="$dir/contacts.db"
	eng "$addr" --db "$db" contacts set "$uid" --relation friend --nickname friend >/dev/null
	eng "$addr" --db "$db" contacts set "$(server_uid "$svc" "$other")" --relation blocked >/dev/null
	roundtrip "friend presence" "$dir/friend.log" \
		"eng $addr --identity $user --nick friend-$t --seconds 10 listen --expect never-$t || true" \
		"eng $addr --db $db --nick friend-watcher --seconds 10 contacts ls --expect-online $uid"
	grep "^friend" "$STATE_DIR/action.log" | tail -1
	roundtrip "blocked poke" "$dir/blocked.log" \
		"eng $addr --db $db --set privacy.block_mode=flag --nick blocker-$t --seconds 15 listen --expect '(blocked)'" \
		"eng $addr --identity $other --nick blocked-poker --seconds 10 poke --to blocker-$t --message blocked-$t"
	grep "^poke from" "$dir/blocked.log"
	rm -rf "$dir"
}

for svc in "${SERVERS[@]}"; do
	port=${PORTS[$svc]:?unknown server $svc}
	addr="127.0.0.1:$port"
	log "$svc ($addr)"

	admin=$(admin_args "$svc" "$addr")

	tree=$("$VOELINCTL" connect "$addr" --nick tree-check tree)
	echo "$tree"
	grep -q "tree-check" <<<"$tree" || fail "own client missing from tree"

	# Guests may not use the server chat by default, so the sender is the admin.
	chat_roundtrip "$addr" server "$admin"
	chat_roundtrip "$addr" channel ""
	voice_roundtrip "$addr"
	presence_check "$addr" "${QUERY[$svc]}"
	relay_check "$addr" "${QUERY[$svc]}"
	gateway_check "$svc" "$addr"
	engine_voice_check "$svc" "$addr"
	if [[ $svc == ts6 ]]; then
		stream_check "$addr"
		stream_late_check "$addr"
	fi
done

log "engine (voelin-core) against both servers, media pipeline through TS6"
VOELIN_LIVE=1 cargo test --quiet -p voelin-core --features media-desktop --test live --test stream_live --test media_live 2>&1 | grep -E "test result|panicked|timed out" || fail "engine tests failed"

log "stream (voelin-stream) through the TeamSpeak 6 server"
VOELIN_LIVE=1 cargo test --quiet -p voelin-stream --test live_ts6 2>&1 | grep -E "test result|panicked|timed out" || fail "stream test failed"

log "all smoke tests passed"
