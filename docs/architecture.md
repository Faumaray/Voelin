# Architecture

This document condenses the project plan: what the client must do, which protocol
facts constrain it, how the code is organised, and the milestone order.

## Protocol facts that shape the design

- **Channel chat** (`sendtextmessage targetmode=2`) always goes to the sender's
  *current* channel, on both TeamSpeak 3 and TeamSpeak 6. There is no protocol
  way to read or post in another channel.
- **Presence**: a connected client always sits in some channel and is visible
  there. Only ServerQuery clients are invisible. TeamSpeak 6 has no raw query:
  SSH on 10022 and HTTP WebQuery (API key) on 10080/10443.
- **Screen sharing** exists only on TeamSpeak 6: peer-to-peer WebRTC (STUN only),
  with the server relaying signalling commands (`setupstream`,
  `respondjoinstreamrequest`, `streamsignaling`, `notifystream*`, ...). See
  [protocol-notes/ts6-streaming.md](protocol-notes/ts6-streaming.md).
- `clientinit` must carry a `client_version_sign` signed by TeamSpeak, so the
  client reuses known version/signature pairs (`voelinctl versions`).
- TeamSpeak 6 servers still accept the TeamSpeak 3 client protocol; verified with
  `voelinctl` against 6.0.0-beta13.1.

### What "invisible" means for query clients (verified on 3.13.8 and 6.0.0-beta13.1)

The server does not hide ServerQuery clients: every client subscribed to a
channel receives `notifycliententerview` for query clients in it too
(`client_type=1`). Official clients hide a query client when its
`client_needed_serverquery_view_power` (100 for `serveradmin` by default) is
higher than the viewer's `i_client_serverquery_view_power` (only Server Admins
have it by default). So observers and relays are invisible to normal users of
official clients, visible to admins, and visible to third-party clients that
ignore the rule. This client applies the same rule.

TeamSpeak 6 ships with SSH and HTTP query disabled
(`TSSERVER_QUERY_SSH_ENABLED`, `TSSERVER_QUERY_HTTP_ENABLED`). HTTP guest
access (`query-http-allow-guest`, on by default) lacks list permissions unless
the admin grants them to the guest query group.

## How each feature is delivered

| Feature | Mechanism |
|---|---|
| Server chat | Voice connection: `targetmode=3`. Without one: gateway or own query session (`servernotifyregister event=textserver`) |
| Channel chat without joining | Relay pool (`voelin-observer`): invisible query sessions sit in channels and relay both ways; posts appear as `[Nick] text` |
| Chat history | Every message a session sees is stored on the device (`voelin-store`); a gateway's stored history fills in what the device missed (sync by revision, paging by time) |
| Pins, reactions, topics, events, stream directory, activity | Gateway features (`tsgw`), driven through `Command::Gateway` / `Event::Gateway` |
| Voice | tsproto UDP transport, Opus (`opus2`), echo cancellation / noise suppression (sonora), own jitter buffer, cpal / AAudio |
| Invisible presence | Gateway presence stream (snapshot + deltas from query events) or direct query; `channelsubscribeall` when voice-connected |
| Screen share with sound | TS6 stream signalling (`voelin-stream`), str0m WebRTC, per-platform capture and codecs (`voelin-media`) |
| Channel files, avatars, icons | Voice connection: `ftgetfilelist`, `ftinitdownload`/`ftinitupload` + TCP transfer, `ftdeletefile`/`ftrenamefile`/`ftcreatedir`; images cached on disk by content |
| Pokes, private messages, offline messages | Voice connection: `clientpoke`, `sendtextmessage targetmode=1`, `messagelist`/`messageget`/`messageadd`/`messagedel`/`messageupdateflag` |
| Friends, blocking | Contacts by unique id in the client database; presence of every session |

## Workspace layout

```
crates/proto/        vendored tsclientlib (tsproto, tsproto-packets, tsproto-types,
                     tsproto-structs + declarations, ts-bookkeeping, tsclientlib)
crates/<voelin-*>       our crates (table below)
tools/voelinctl          headless CLI
dev/                 docker-compose TS3 + TS6 servers, gateway configs
scripts/             it-smoke.sh, own-crates.sh
fuzz/                cargo-fuzz targets
```

| Crate | Responsibility | State |
|---|---|---|
| `voelin-model` | UI-facing domain model (servers and their details, channels, clients, server and channel groups, chat and its file links, capabilities), no IO | done |
| `voelin-query` | ServerQuery codec and transports: raw TCP, SSH (russh), HTTP WebQuery | done |
| `voelin-observer` | Presence tracker + chat relay pool over `voelin-query` | done |
| `voelin-gateway-proto`, `voelin-gateway` (`tsgw`) | Companion service for server admins: identity-challenge auth, permission-mirroring authorization, presence stream, chat relay, SQLite history, WebSocket + JSON (`tsgw.v1+json`) | done |
| `voelin-store` | Identities, bookmarks, settings, chat history (dedupe, paging, sync cursors), contacts, secrets (keyring / Android Keystore); versioned schema | done |
| `voelin-audio` | Capture/playback, Opus, echo cancellation, resampling, jitter buffer, mixer, VAD/push-to-talk | v0 (no AEC yet) |
| `voelin-stream` | TS6 stream commands, JSON signalling, str0m peer connections, host/STUN candidates, streamer/viewer sessions, frame-source seam | sessions + transport |
| `voelin-core` | Engine: runtime, per-server sessions, merge of voice/gateway/query sources, event bus, command API, chat history, the gateway's features, audio settings and volumes, streams on TS6, files, avatar and icon cache, pokes, offline messages, contacts; `media` module (capture → encoder → stream, stream → decoder) | done |
| `voelin-ui` | Slint UI and the desktop binary: streams panel, viewer, share dialog, audio / hotkey / codec settings | done (desktop) |
| `voelin-media` | Screen and system-audio capture backends (PipeWire portal, X11, Windows Graphics Capture, Android MediaProjection), video codecs | done (desktop hardware encoders through FFmpeg loaded at runtime, not run on a GPU yet; MediaCodec not run on a device yet) |
| `voelin-platform` | Global hotkeys, notifications, paths, crash reports, notices | done |
| `voelin-android` + `android/` | Android library + Gradle/Kotlin app (foreground services for voice and screen capture); see [android.md](android.md) | builds, not yet run on a device |

## Merging sources

Each server session has up to three optional sources: **voice** (normal client
connection), **gateway**, and **query** (own credentials). Presence authority is
voice (after subscribing to all channels) > gateway > query. Channel chat uses
the native command only when voice-connected *and* in that channel, otherwise the
gateway, otherwise a local query relay. UI states: `Offline`,
`Observing (Gateway|Query)` (invisible), `Connecting`, `Connected`.

## Chat history

Every chat message a session sees is stored in the client database
(`voelin-store::chat`, table `messages`) under the server's unique id: live
over voice, pushed by the gateway, read by a query relay, or sent by us
(stored at once, as `local`). The unique id comes from the voice connection
(the server's key: base64 SHA-1 on TeamSpeak 3, SHA-256 on 6, the same value
as `virtualserver_unique_identifier`) or the gateway's `hello`; the database
remembers it for the bookmark's address and gateway URL (`server_aliases`),
so a chat shows its stored messages before any source has connected.
Query-only sessions, which learn no id, keep history under the query address.

Rows carry `ts_ms`, the author, the text, the source that delivered them
first, and, once the gateway's copy is known, its stable id (`remote_id`,
unique per server and chat), revision, topic, pin and reactions. A message
often arrives twice (over voice and from the gateway); the store merges the
copies into one row: by gateway id, else by the text without a relay's
`[nick] ` prefix, the author (unique id, the relay prefix naming the other
copy's author, or the nickname without unique ids) and a time difference of
at most `chat.dedupe_tolerance_ms` (default 5 s), between different sources
only (a person saying the same thing twice stays two messages).

`voelin-core::history` runs the database on a writer thread: live messages
are queued without waiting, writes that queue up go into one transaction,
statements are cached. When a chat opens (and whenever a source of the
session connects), the engine emits the newest `chat.history_page` stored
messages (`Event::ChatHistory`, source `Local`), then, with a gateway that
keeps history, syncs the chat: the first time the gateway's latest page,
afterwards `sync` from the chat's revision cursor (`chat_cursors`), which
returns new messages and messages whose pin, reactions or topic changed,
page after page until done (`Gateway` batches; the last one may be empty).
`Command::LoadOlderHistory` pages back: the gateway's page before that time
is fetched and merged first, then the stored page is emitted, so gaps (times
this device was offline) fill in as the user scrolls. Live messages and
changes come as `Live` batches. The UI keeps each chat as a list keyed by
the local id and ordered by `(ts_ms, id)`, and upserts every batch. Without
a gateway only `Local` batches come: the UI labels the chat "only messages
seen by this device".

Settings: `chat.store_history` (off: memory only, negative ids),
`chat.history_page` (0: everything), `chat.dedupe_tolerance_ms`,
`chat.retention_days` (0: keep everything; pinned messages stay). No limit
is built in.

## Gateway features in the engine

The session's gateway source runs on the typed client
(`voelin-gateway-proto`, feature `client`): presence and relayed chat as
before, plus the extensions. After login the engine enables the chat
extension pushes and subscribes to events, the stream directory and the
activity feed when the gateway offers them. `Command::Gateway` carries a
`GatewayRequest` (pins, reactions, topics and topic history, events with
RSVP, the stream directory, activity, permissions, runtime configuration and
permission rules); answers and pushes come as `Event::Gateway` with a
`GatewayUpdate` (`Done`/`Failed` for requests without data or refused ones,
`Connected`/`Capabilities`/`Disconnected` for what the gateway offers).
Messages in answers and pushes are stored first, so the UI gets them with
local ids. The contract is in the module docs of `voelin_core::gateway` and
`voelin_core::history`; `voelinctl gateway --engine` drives it from the
command line.

## Server details, groups and client details

The voice connection's book has everything the server tells its clients;
`voelin-model` carries it: per client the avatar hash (`client_flag_avatar`),
description, talk power, channel group, server groups, badges, icon, away
message, mute, talk, recording and stream flags; per server
(`Presence::server`, `ServerDetails`) the welcome and host message, host
banner (link, image, reload interval, scaling), host button, icon, unique id,
platform and version; the server and channel groups (name, icon, sort id,
naming mode, type). `Event::Presence` carries all of it; `Event::ServerDetails`
and `Event::Groups` come when those parts change. Gateway and query presence
fill what their rows have. The new fields are optional in JSON, so gateways
and clients of different versions still understand each other.

## Files, avatars and icons

Channel file browsers, avatars and icons go over the voice connection (TS3
and TS6 alike, verified against 3.13.8 and 6.0.0-beta13.1). `ftgetfilelist`
lists a directory; `ftinitdownload`/`ftinitupload` open a transfer, which
then runs over its own TCP connection to the port the server announces (the
dev TS6 server announces 30034, its published port). The engine
(`voelin_core::files`) streams transfers in 64 KiB pieces: downloads into
`<path>.part`, renamed when complete (resumable), or into memory; uploads
read the file as they send it. Progress is reported every
`files.progress_interval_ms`; transfers can be cancelled. Nothing limits
sizes besides the server's quotas. Requests and transfers carry ids the
caller picks (`RequestId`, `TransferId`).

Chat messages link files as `[URL=ts3file://name?serverUID=…&channel=…&path=…&filename=…&size=…]`;
`ChatMessage::file_refs` finds them (`FileRef`) for file cards and
`Command::DownloadChatFile` fetches one (refused if the link names another
server). `FileRef::to_bbcode` writes a link.

Avatars live in channel 0 as `/avatar_<unique id bytes as a–p>` (one letter
per nibble; the same on TS6 with its longer unique ids); `client_flag_avatar`
is the file's MD5. Icons are `/icon_<id>`, the id being the CRC32 of the
image (below 1000: built into clients). The engine fetches every avatar and
icon of the servers it is on (`cache.fetch_images`) into a content-addressed
disk cache (`voelin_core::cache`, `<cache>/voelin/images/{avatars,icons}`):
a lookup is a map access, a download of the same file for several sessions
runs once, and the least recently used files go when the cache exceeds
`cache.max_mb` (0: no limit). `Event::AvatarReady` / `Event::IconReady`
report the files. `Command::SetAvatar` uploads ours to `/avatar` and
announces its hash (`clientupdate client_flag_avatar`), or removes it.

## Pokes, private and offline messages

`Command::Poke` sends `clientpoke`; incoming pokes are `Event::Poke` (they
used to be stored as private messages). Private messages to any client on
the server are a `ChatTarget::Private(unique id)` chat over the voice
connection, stored in the chat history under the peer's unique id, so the
conversation survives reconnects and new client ids. Gateways and query
relays cannot send private messages as the user; without voice the engine
says so. A voice command that fails (e.g. a private message to a client who
left) is an error event; the connection stays.

Offline messages (`voelin_core::offline`, capability `offline_messages`,
TS3 and TS6) are listed, read, sent to a unique id (on TS6 the one that
server generation derives, see `voelinctl identity show`), deleted and
marked read. Guests may not send them by default.

## Contacts

Contacts (`voelin_store::contacts`, table `contacts` since schema version 3)
are people by unique id: friend, blocked or neutral, with a note, a mute and
a volume, when they were added, and when and on which server they were last
seen. The engine (`voelin_core::contacts`) keeps them in memory for lookups,
takes `Command::SetContact` / `RemoveContact`, and reports
`Event::ContactsChanged`. From the presence of every session (voice,
gateway or query) it reports where each friend is (`Event::FriendPresence`)
for Home's friends list, and records sightings in one write per change. A
contact's mute and volume apply to their voice in every session. Messages of
blocked contacts are flagged (`ChatMessage::blocked`, live and in history);
their private messages and pokes are dropped or flagged per
`privacy.block_mode`. `stream.permissions = friends` lets friends watch our
stream without asking and asks for everyone else.

## Streams

`voelin-stream::Streams` holds the stream state of one TS6 connection without
doing connection IO: it is fed stream notifications (and failed commands) and
the app's decisions, owns the str0m peers, and queues requests to send plus
events. Inside it, `StreamerSession` runs our stream (one peer per viewer, our
offer in `respondjoinstreamrequest`), `ViewerSession` one watched stream
(answer through `streamsignaling`), and `StreamDirectory` the streams of our
channel. Its tests drive two instances through a fake relay server.

Transport details of our stream (`voelin-stream`):

- **SRTP profile**: DTLS negotiates the profile in the order of
  `PeerConfig::srtp_profiles`, by default AES_CM_128_HMAC_SHA1_80 (what
  official TS6 streams use), then AEAD_AES_128_GCM and AEAD_AES_256_GCM for
  peers without it. Our peer is the DTLS server with official clients and
  browsers, so our order decides. `dtls::VoelinDtlsProvider` wraps str0m's
  dimpl DTLS with that order; dimpl is vendored in `third_party/dimpl` with a
  patch for it (`VOELIN-PATCH.md`). The order is a setting
  (`Streams::set_srtp_profiles`, `Command::SetSrtpProfiles`) and applies to
  new connections.
- **Losses**: each peer socket asks for 4 MB of receive and send buffer
  (`PeerConfig::udp_buffer`; Linux caps it at `net.core.rmem_max`, and
  what was granted is logged, with a hint when it is less): the default
  208 KB overflowed under a 1440p keyframe on loopback (118-209 datagrams
  per 5 s of a 9 Mbit/s stream), the larger buffer dropped none
  (`tests/loopback.rs` `high_bitrate_bursts`). What is still lost is
  repaired by NACK and retransmission (RTX), which our answers keep for
  every video codec; str0m waits up to 30 complete frames for a
  retransmission before it hands a frame over as non-contiguous
  (`losses_are_repaired_by_retransmission`: one packet in 25 lost, every
  frame whole).
- **Bandwidth estimation and pacing**: each viewer connection of our stream
  runs str0m's send-side estimation (transport-cc feedback; REMB if a viewer
  sends only that) and paces packets to it. Estimates are per viewer
  (`ViewerInfo::estimate`). The offer is unchanged by it: str0m always offers
  the transport-cc and abs-send-time extensions.
- **Simulcast**: `StreamerOptions::layers` lists the encodings (`LayerSpec`).
  A viewer gets the highest layer whose `min_bitrate` its estimate reaches:
  down at once, up after the estimate exceeded the next layer's minimum by 20%
  for 2 s. A switch asks for a keyframe of the new layer and waits for it
  before the viewer's frames change layer; the RTP timestamps come from one
  capture clock, so the viewer's single video track continues. Keyframe
  requests (PLI/FIR) go to the viewer's layer only. Each layer's bitrate
  target is the lowest estimate of its viewers, at least 30 kbit/s, at most
  its `max_bitrate`; the encoders read targets and keyframe requests through
  `LayerFeedback` (lock-free, via `StreamSink::layer_bitrate` and
  `take_layer_keyframes`), and events report them. Peers that negotiate RID
  simulcast (`PeerConfig::simulcast`, off for TeamSpeak; for SFUs or WHIP) get
  every layer with its RID. The layer list can change while live
  (`Streams::set_layers`, `Command::SetStreamLayers`). Without layers the
  stream has one layer 0 at the setup's bitrate and every viewer gets it.

In `voelin-core` a stream task per voice connection runs `Streams`; the voice task
forwards `MessageEvent`s and sends the requests. Encoders push frames through
the `StreamSink` of `StreamState::Live`; received frames of watched streams go
to `Engine::subscribe_frames`, not the event bus. The audio of watched streams
goes straight to the session's audio thread, which mixes it like a talker
with its own volume (`Command::SetStreamVolume`).

`voelin-core::media` (feature `media`) connects this to `voelin-media`:
`Streamer` captures (screen, window, portal or the test pattern, plus system
audio), encodes with the codec of our offer (VP8 by default; the offer carries
only the codec we encode, since every viewer gets the same frames) and Opus,
and feeds the `StreamSink`, honouring keyframe requests. `Viewer` decodes a
watched stream on its own thread (skipping to the next keyframe after losses
and asking for one) and hands pictures to the UI, which keeps only the newest
(`Latest`). `EncodedSource` wraps a `Streamer` as a `voelin-stream::FrameSource`
for `voelinctl stream start`; `SyntheticSource` (fake VP8 bytes, one frame per
layer) remains for voelin-stream's own tests and `voelinctl stream start
--placeholder`.

The server announces a stream only to the clients in the streamer's channel
at its start. `voelin-stream::discovery` follows the clients' channels and
streaming flags (`Streams::update_clients`), and looks up streams nobody
announced to us with `requeststreaminfo clid=<streamer>`
([research/ts6-late-join.md](research/ts6-late-join.md)). The lookup is a
`StreamLookup` trait. The session's gateway directory is the second one:
its registered entries (stream id, streamer) go to the stream task, which
adds those of streamers in our channel that still stream
(`Streams::discovered`), so a stream is found even when the server's lookup
does not answer. Our own stream registers itself in the directory when it
goes live (title, kind, channel, client), updates its viewer count, and is
removed when it ends.

## Settings

`voelin-core::settings::Settings` is the settings service. Keys are typed
statics (`Key<T>`: name, default, validation, a `Kind` for a settings page);
the effective value is the runtime value stored in the client database, else
a command line or environment override, else a config file, else the
default. Reads come from memory (`get`, `get_arc`, or a `SettingWatch` per
key); writes notify subscribers and go to SQLite on a writer thread. The
engine holds one (`Engine::settings`, `Command::AttachSettings`), takes
`Command::SetSetting` / `ResetSetting` and reports `Event::SettingChanged`.
The UI opens it on its database and registers its own keys (`ui`,
`client_playback`); the stream (`stream.*`), chat (`chat.*`), cache (`cache.*`),
file transfer (`files.*`) and privacy (`privacy.*`) keys
are defined in the core, which reads them on every use.

## Milestones

| | Scope | Exit criteria |
|---|---|---|
| **M0** | Vendoring, workspace, `voelinctl`, dev servers, CI | Builds on Linux + Windows; smoke test passes on TS3 and TS6 |
| M1 | Protocol modernisation: edition 2024, dependency updates, puzzle rewrite (off-thread, progress, cancel), TS6 declarations (`client_is_streaming`, stream commands), fuzzing | Tests green on both servers, clippy clean |
| M2 | Identities/bookmarks store, audio v0 (Opus, jitter buffer), `voelinctl voice` | Tone round-trips between two clients on both servers |
| M3 | `voelin-query` (raw/SSH/HTTP) + observer | Invisible presence and relay chat verified |
| M4 | Gateway v1 | Presence + channel chat without appearing in the client list, permission denials tested |
| M5 | `voelin-core` + Slint desktop UI (Linux) | Daily use on GNOME, KDE, X11 |
| M6 | Audio quality, global push-to-talk, Windows parity and packaging | One-hour call without drift or echo |
| M7 | TS6 stream viewer | Watch an official TS6 client's stream |
| M8 | Streamer: capture, system audio, encoders | Official TS6 client watches our 1080p30 stream with sound |
| M9 | Android app | Background voice, watch and share screen |
| M10 | Release hardening | Signing, crash reports, i18n, notices audit |
| M11+ | macOS / iOS | |

## Risks

- TeamSpeak 6 streaming is undocumented and still in beta: probe against our own
  server, record findings in `docs/protocol-notes/`, pin server versions, gate
  features on the server version. AGPL projects (astrum-ts6) are reference only.
- Signed client versions could be blocked: configurable, with fallbacks.
- ServerQuery flood limits: token buckets, capped relay pool, allowlist guidance.
- H.264 patents: no bundled encoder; runtime-downloaded Cisco OpenH264, VP8/VP9
  preferred, OS hardware codecs.
