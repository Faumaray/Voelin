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
  client reuses known version/signature pairs (`tsctl versions`).
- TeamSpeak 6 servers still accept the TeamSpeak 3 client protocol; verified with
  `tsctl` against 6.0.0-beta13.1.

## How each feature is delivered

| Feature | Mechanism |
|---|---|
| Server chat | Voice connection: `targetmode=3`. Without one: gateway or own query session (`servernotifyregister event=textserver`) |
| Channel chat without joining | Relay pool (`tsc-observer`): invisible query sessions sit in channels and relay both ways; posts appear as `[Nick] text` |
| Voice | tsproto UDP transport, Opus (`opus2`), echo cancellation / noise suppression (sonora), own jitter buffer, cpal / AAudio |
| Invisible presence | Gateway presence stream (snapshot + deltas from query events) or direct query; `channelsubscribeall` when voice-connected |
| Screen share with sound | TS6 stream signalling (`tsc-stream`), str0m WebRTC, per-platform capture and codecs (`tsc-media`) |

## Workspace layout

Present today:

```
crates/proto/     vendored tsclientlib (tsproto, tsproto-packets, tsproto-types,
                  tsproto-structs + declarations, ts-bookkeeping, tsclientlib)
tools/tsctl       headless CLI
dev/              docker-compose TS3 + TS6 servers
scripts/          it-smoke.sh
```

Planned crates:

| Crate | Responsibility |
|---|---|
| `tsc-model` | UI-facing domain model (servers, channels, clients, chat, capabilities), no IO |
| `tsc-query` | ServerQuery codec and transports: raw TCP, SSH (russh), HTTP WebQuery |
| `tsc-observer` | Presence tracker + chat relay pool over `tsc-query` |
| `tsc-gateway-proto`, `tsc-gateway` (`tsgw`) | Companion service for server admins: identity-challenge auth, permission-mirroring authorization, presence stream, chat relay, SQLite history, WebSocket + JSON (`tsgw.v1+json`) |
| `tsc-store` | Identities, bookmarks, settings, chat cache, secrets (keyring / Android Keystore) |
| `tsc-audio` | Capture/playback, Opus, echo cancellation, resampling, jitter buffer, mixer, VAD/push-to-talk |
| `tsc-media` | Screen and system-audio capture backends (PipeWire portal, X11, Windows Graphics Capture, Android MediaProjection), video codecs |
| `tsc-stream` | TS6 stream signalling state machine, str0m peer connections |
| `tsc-core` | Engine: runtime, per-server sessions, merge of voice/gateway/query sources, event bus, command API |
| `tsc-platform` | Global hotkeys, notifications, paths, Android JNI glue |
| `tsc-ui` | Slint UI and the desktop binary |
| `tsc-android` + `android/` | Android library + Gradle/Kotlin app (foreground services for voice and screen capture) |

## Merging sources

Each server session has up to three optional sources: **voice** (normal client
connection), **gateway**, and **query** (own credentials). Presence authority is
voice (after subscribing to all channels) > gateway > query. Channel chat uses
the native command only when voice-connected *and* in that channel, otherwise the
gateway, otherwise a local query relay. UI states: `Offline`,
`Observing (Gateway|Query)` (invisible), `Connecting`, `Connected`.

## Milestones

| | Scope | Exit criteria |
|---|---|---|
| **M0** | Vendoring, workspace, `tsctl`, dev servers, CI | Builds on Linux + Windows; smoke test passes on TS3 and TS6 |
| M1 | Protocol modernisation: edition 2024, dependency updates, puzzle rewrite (off-thread, progress, cancel), TS6 declarations (`client_is_streaming`, stream commands), fuzzing | Tests green on both servers, clippy clean |
| M2 | Identities/bookmarks store, audio v0 (Opus, jitter buffer), `tsctl voice` | Tone round-trips between two clients on both servers |
| M3 | `tsc-query` (raw/SSH/HTTP) + observer | Invisible presence and relay chat verified |
| M4 | Gateway v1 | Presence + channel chat without appearing in the client list, permission denials tested |
| M5 | `tsc-core` + Slint desktop UI (Linux) | Daily use on GNOME, KDE, X11 |
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
