# Gateway protocol (`tsgw.v1+json`)

How Voelin talks to `tsgw` ([gateway-admin.md](gateway-admin.md)). The types
are in `crates/voelin-gateway-proto` (`messages.rs`, `types.rs`); its
`client` feature is a typed client (one async method per request, pushes as
`Push`), which the app uses through `voelin_core::gateway`.

## Transport

WebSocket to `…/v1`, subprotocol `tsgw.v1+json`, one JSON envelope per text
frame:

```json
{"v":1,"id":7,"type":"send_chat","data":{"target":{"kind":"channel","id":5},"text":"hi"}}
```

The client picks `id` for requests; the answer carries the same `id`.
Pushes have no `id`. Version 1 only grows: new message types and optional
fields. Both sides ignore unknown fields. The gateway answers a type it does
not know with `error` / `unknown_type` (and a malformed message with
`bad_request`), with the request's `id`, and keeps the connection.

Older clients never receive a type they do not know: the newer pushes only go
to clients that asked for them (`enable`, `subscribe_*`), and newer error
codes only answer newer requests. Enums that may grow (`ErrorCode`,
`EventKind`, `RsvpStatus`, `Action`, `StreamSource`, `ConfigSource`) have an
`unknown` fallback in the Rust types; activity kinds are plain strings.

## Login and capabilities

1. Gateway: `hello {gateway_id, server_uid, server_name, nonce, capabilities}`.
2. Client: `auth {omega, key_offset, ts, signature, nickname}`, an ECDSA
   signature with the TeamSpeak identity over
   `tsgw-auth-v1\n{gateway_id}\n{server_uid}\n{nonce}\n{ts}`, or
   `resume {token, nickname}`.
3. Gateway: `auth_ok {uid, token, token_expires, capabilities}` or `error`.

`capabilities` in `hello` lists the features enabled on the gateway;
`auth_ok` repeats them for this user and adds `admin` when the user may
administer the gateway. A `capabilities {capabilities}` push follows when an
admin turns a feature on or off (to clients that sent `enable` or subscribed
to something). Clients hide what is missing.

| Capability | Meaning |
|---|---|
| `presence` | `subscribe_presence` |
| `relay` | channel and server chat |
| `history` | stored messages: `history`, `query_history`, `sync` |
| `pins`, `reactions`, `topics` | on stored messages |
| `events` | scheduled events |
| `streams` | stream directory |
| `activity` | activity feed |
| `admin` | configuration and permission rules (only in `auth_ok`) |

## Original messages

| Client → gateway | Answer |
|---|---|
| `subscribe_presence` / `unsubscribe_presence` | `presence_snapshot {seq, snapshot}`, then `presence_delta {seq, delta}` pushes / `ok` |
| `open_chat {target}` / `close_chat {target}` | `ok`; chat arrives as `chat_event {id, message}` (or `message`, below) |
| `send_chat {target, text}` | `ok` |
| `history {target, before?, limit}` | `history {messages}`, oldest first |
| `ping` | `pong` |

`history` entries now also carry `topic_id`, `reactions`, `pinned` and
`rev` (all omitted when empty); `limit` is no longer capped at 200, only by
`quota.history_page` if an admin sets it.

## Chat extensions

**`enable {features}`** → `enabled {features}`. After it, messages in open
chats arrive as `message {entry}` instead of `chat_event`, with the pushes
of the listed features (`pins`, `reactions`, `topics`; empty: all). Clients
that do not send it see topic posts in `chat_event` with the topic prefix.

A stored message (`HistoryEntry`):

```json
{"id":42,"message":{"target":{"kind":"channel","id":5},"author_name":"Bob","author_uid":"…",
 "text":"hi","ts_ms":1700000000123,"via_relay":true},
 "topic_id":3,"reactions":[{"emoji":"🦀","count":2,"me":true}],"pinned":true,"rev":1700000000200001}
```

`id` is stable and never reused; `ts_ms` is when the gateway saw it; `rev`
increases with every change to the message (post, pin, reaction), across
restarts.

| Request | Answer | Notes |
|---|---|---|
| `query_history {target, before?, after?, before_ms?, after_ms?, limit?, topic?, exclude_topics?}` | `history_page {target, topic?, messages, has_more}` | Oldest first. No cursor: the latest `limit`. `before`: the `limit` right before it. `after`: the `limit` right after it. Both: the range from `after`. `*_ms` are times (Unix ms). No `limit`: everything in range. `has_more`: more beyond the page in the paging direction |
| `sync {target, since_rev, limit?}` | `sync_page {target, messages, rev, has_more}` | New and changed messages by revision; pass `rev` next time |
| `post {target, text, topic?}` | `posted {entry}` | Like `send_chat`, returns the stored message; into a topic, TeamSpeak sees `[#Title] text` |
| `pin {message_id}` / `unpin {message_id}` | `ok` | Pushes `pinned {target, pin}` / `unpinned {target, message_id, by}` |
| `list_pins {target}` | `pins {target, pins}` | Newest first; `pin` is `{entry, by, ts_ms}` |
| `react {message_id, emoji}` / `unreact {…}` | `ok` | Any string without spaces. Push `reaction {target, message_id, emoji, user, added, count}` |
| `reactors {message_id, emoji}` | `reactors {message_id, emoji, users}` | |
| `create_topic {target, title, message_id?}` | `topic {topic}` | Push `topic_updated {topic}` (also on renames, archiving and new posts) |
| `update_topic {topic_id, title?, archived?}` | `topic {topic}` | Creator or moderator; archived topics take no posts |
| `list_topics {target, include_archived}` | `topics {target, topics}` | Most recently active first; `topic` has `message_count`, `last_activity_ms`, `root_message_id` |
| `permissions {channel?}` | `permissions {channel, actions}` | Actions the user may take (to hide buttons) |

`user`, `by` and `creator` are `{uid, name}`.

## Events

| Request | Answer |
|---|---|
| `create_event {event: EventSpec}` | `event {event}` |
| `update_event {id, event: EventSpec}` | `event {event}` (creator or moderator) |
| `delete_event {id}` | `ok` |
| `get_event {id}` | `event {event}` with `attendees` |
| `list_events {from_ms?, to_ms?, channel?, limit?}` | `events {events}` by start; default from now, including running events |
| `rsvp {event_id, status}` | `event {event}`; `status` `going`, `maybe`, `not_going` or `null` to withdraw |
| `subscribe_events` / `unsubscribe_events` | `ok` |

`EventSpec`: `{title, description, start_ms, end_ms?, channel?, kind:
"general"|"stream", stream_title?, stream_game?, host_uid?}`. An event adds
`id, creator, created_ms, updated_ms, going, maybe, not_going, my_rsvp?,
attendees?, live_stream?`. When the host of a stream event registers a stream
around its time, `live_stream` is the directory id until the stream ends.

Pushes to subscribers (only for events in channels they can see):
`event_updated {event}` (created, changed, RSVP counts, live),
`event_deleted {id}`, `event_reminder {event, starts_in_ms}` at the times in
`events.reminder_minutes`.

## Stream directory

| Request | Answer |
|---|---|
| `register_stream {stream_id, client_id?, channel?, title, kind, viewers?}` | `stream {stream}` |
| `update_stream {stream_id, title?, viewers?}` | `stream {stream}` |
| `unregister_stream {stream_id}` | `ok` |
| `list_streams` | `streams {streams}` |
| `subscribe_streams` / `unsubscribe_streams` | `streams {streams}` / `ok` |

An entry: `{id, stream_id?, streamer, client_id?, channel?, title, kind,
started_ms, viewers?, source: "registered"|"detected", event_id?}`. `id` is
the stream id, or `client/<clid>` for detected streams (a client with
`client_is_streaming` but no registration; its stream id is unknown).
`client_id` must be the registering user's own voice client; with it the
gateway drops the entry when that client leaves or stops streaming and
follows it to other channels. Entries without a `client_id` end with the
registering connection. Pushes to subscribers: `stream_started {stream}`,
`stream_updated {stream}`, `stream_ended {id, reason}`.

## Activity feed

`list_activity {before?, limit?}` → `activity {entries, has_more}`, newest
first; `subscribe_activity` / `unsubscribe_activity` → `ok`, then
`activity_added {entry}` pushes. An entry: `{id, ts_ms, kind, actor?,
channel?, ref_id?, text, data}`; `text` is a readable summary for kinds the
client does not know. Kinds so far: `stream_started`, `stream_ended`,
`event_created`, `event_cancelled`, `event_starting`, `topic_created`,
`pinned`.

## Administration

Needs the `admin` capability; see [gateway-admin.md](gateway-admin.md#admin-commands).

| Request | Answer |
|---|---|
| `config_list` | `config {entries}` |
| `config_get {key}` | `config_value {entry}` |
| `config_set {key, value}` | `config_value {entry}` |
| `config_reset {key}` | `config_value {entry}` |
| `config_reload` | `config {entries}` |
| `perm_list` | `perm_rules {rules}` |
| `perm_set {action, rule: {everyone?, server_groups?, channel_groups?}}` | `perm_rules {rules}` |
| `perm_reset {action}` | `perm_rules {rules}` |

`entry`: `{key, value, source: "db"|"cli"|"env"|"file"|"default", default,
type, description, bootstrap?}`. `rules`: `{action, rule?, source, default}`
for `react`, `pin`, `create_topic`, `create_event`, `rsvp`, `stream`,
`moderate`, `admin`.

## Errors

`error {code, message}` with `code`: `bad_request`, `auth_failed`,
`unknown_identity`, `level_too_low`, `banned`, `forbidden`,
`not_authenticated`, `unavailable`, `internal`, and for the extensions
`unknown_type`, `not_found`, `quota_exceeded`, `rate_limited`,
`feature_disabled`.
