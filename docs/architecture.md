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
| Voice | tsproto UDP transport, Opus (`opus2`), echo cancellation / noise suppression (sonora), own jitter buffer, cpal / AAudio |
| Invisible presence | Gateway presence stream (snapshot + deltas from query events) or direct query; `channelsubscribeall` when voice-connected |
| Screen share with sound | TS6 stream signalling (`voelin-stream`), str0m WebRTC, per-platform capture and codecs (`voelin-media`) |

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
| `voelin-model` | UI-facing domain model (servers, channels, clients, chat, capabilities), no IO | done |
| `voelin-query` | ServerQuery codec and transports: raw TCP, SSH (russh), HTTP WebQuery | done |
| `voelin-observer` | Presence tracker + chat relay pool over `voelin-query` | done |
| `voelin-gateway-proto`, `voelin-gateway` (`tsgw`) | Companion service for server admins: identity-challenge auth, permission-mirroring authorization, presence stream, chat relay, SQLite history, WebSocket + JSON (`tsgw.v1+json`) | done |
| `voelin-store` | Identities, bookmarks, settings, chat cache, secrets (keyring / Android Keystore) | done |
| `voelin-audio` | Capture/playback, Opus, echo cancellation, resampling, jitter buffer, mixer, VAD/push-to-talk | v0 (no AEC yet) |
| `voelin-stream` | TS6 stream commands, JSON signalling, str0m peer connections, host/STUN candidates, streamer/viewer sessions, frame-source seam | sessions + transport |
| `voelin-core` | Engine: runtime, per-server sessions, merge of voice/gateway/query sources, event bus, command API, audio settings and volumes, streams on TS6; `media` module (capture → encoder → stream, stream → decoder) | done |
| `voelin-ui` | Slint UI and the desktop binary: streams panel, viewer, share dialog, audio / hotkey / codec settings | done (desktop) |
| `voelin-media` | Screen and system-audio capture backends (PipeWire portal, X11, Windows Graphics Capture, Android MediaProjection), video codecs | done (desktop hardware encoders planned; MediaCodec not run on a device yet) |
| `voelin-platform` | Global hotkeys, notifications, paths, crash reports, notices | done |
| `voelin-android` + `android/` | Android library + Gradle/Kotlin app (foreground services for voice and screen capture); see [android.md](android.md) | builds, not yet run on a device |

## Merging sources

Each server session has up to three optional sources: **voice** (normal client
connection), **gateway**, and **query** (own credentials). Presence authority is
voice (after subscribing to all channels) > gateway > query. Channel chat uses
the native command only when voice-connected *and* in that channel, otherwise the
gateway, otherwise a local query relay. UI states: `Offline`,
`Observing (Gateway|Query)` (invisible), `Connecting`, `Connected`.

## Streams

`voelin-stream::Streams` holds the stream state of one TS6 connection without
doing connection IO: it is fed stream notifications (and failed commands) and
the app's decisions, owns the str0m peers, and queues requests to send plus
events. Inside it, `StreamerSession` runs our stream (one peer per viewer, our
offer in `respondjoinstreamrequest`), `ViewerSession` one watched stream
(answer through `streamsignaling`), and `StreamDirectory` the streams of our
channel. Its tests drive two instances through a fake relay server.

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
for `voelinctl stream start`; `SyntheticSource` (fake VP8 bytes) remains for
voelin-stream's own tests.

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
