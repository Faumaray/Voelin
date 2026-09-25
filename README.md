# teamspeak_client_rs

A TeamSpeak 3 / TeamSpeak 6 compatible voice client written in Rust, for desktop
(Linux Wayland/X11, Windows, later macOS) and mobile (Android, later iOS).

It speaks the TeamSpeak client protocol itself (no official SDK). The protocol
layer is a vendored fork of [ReSpeak/tsclientlib](https://github.com/ReSpeak/tsclientlib).

> Not affiliated with or endorsed by TeamSpeak Systems GmbH. "TeamSpeak" is a
> trademark of its owner; the product name of this client is still to be decided.

## Goals

| Feature | How |
|---|---|
| Global server chat | Native `sendtextmessage targetmode=3` |
| Chat in channels without joining their voice channel | Relay through invisible ServerQuery sessions (companion gateway `tsgw`, or own query credentials). The protocol only delivers channel chat for the channel you are in. |
| Voice | Opus over the TS3 UDP protocol, echo cancellation, jitter buffer |
| See who is in voice channels without joining | Channel subscriptions when connected; invisible presence through the gateway or own query credentials |
| Watch and share screen with sound | TeamSpeak 6 servers only: WebRTC peer-to-peer streams signalled through the server |

See [docs/architecture.md](docs/architecture.md) for the design and milestones.

## Status

Done so far (milestones M0–M3 of [the plan](docs/architecture.md#milestones)):

- `crates/proto/`: vendored tsclientlib, modernised (TS6 fields and stream
  messages, faster off-thread Init1 puzzle, `curve25519-dalek` 4, fuzzing fixes;
  see [UPSTREAM.md](crates/proto/UPSTREAM.md))
- `crates/tsc-model`: server flavor detection (TS3 / TS6) and capabilities
- `crates/tsc-audio`: Opus voice encoding, framing, resampling, WAV, jitter buffer/mixer, cpal devices
- `crates/tsc-store`: SQLite store for identities, bookmarks, settings and chat history; keyring secrets
- `crates/tsc-query`: ServerQuery client over raw TCP, SSH and HTTP WebQuery (events, rate limiting, keepalive)
- `crates/tsc-observer`: invisible presence (events or polling) and channel-chat relays over ServerQuery
- `tools/tsctl`: headless CLI: channel tree, chat, voice send/record, raw commands, stream events,
  `query`, `observe` (invisible presence) and `relay` (channel chat without joining)
- `dev/`: TeamSpeak 3.13 and TeamSpeak 6 (6.0.0-beta13.1) servers; `scripts/it-smoke.sh`
  checks tree, server chat, channel chat, a voice tone round-trip, invisible presence
  and relay chat on both
- `fuzz/`: cargo-fuzz targets for packets, commands and the license chain

No GUI and no screen sharing yet.

## Try it

```sh
# Local servers (use REGISTRY=mirror.gcr.io if Docker Hub rate-limits you)
docker compose -f dev/docker-compose.yml up -d

cargo run -p tsctl -- connect 127.0.0.1:9987 tree            # TeamSpeak 3
cargo run -p tsctl -- connect 127.0.0.1:9988 tree            # TeamSpeak 6
cargo run -p tsctl -- connect 127.0.0.1:9988 --nick me repl  # interactive chat
cargo run -p tsctl -- connect 127.0.0.1:9988 --nick me voice send --tone 440
cargo run -p tsctl -- connect 127.0.0.1:9988 --nick ear voice record out.wav

# ServerQuery (dev password tsc-dev-admin): invisible presence and relay chat
cargo run -p tsctl -- observe ssh 127.0.0.1:10022 --secret tsc-dev-admin --allowlisted
cargo run -p tsctl -- relay ssh 127.0.0.1:10022 --secret tsc-dev-admin --allowlisted --channel 1

scripts/it-smoke.sh   # all of the above, on both servers
```

Other commands: `tsctl identity new`, `tsctl versions`, `tsctl connect <addr> listen|chat|raw`.
`tsctl --help` lists all options.

Building on Linux needs the ALSA headers (`libasound2-dev`) for the audio examples,
plus CMake and a C compiler for the bundled libopus.

## License

The vendored crates in `crates/proto/` are `MIT OR Apache-2.0` (upstream
`LICENSE-MIT` / `LICENSE-APACHE` are kept there). New crates are declared under
the same terms in `Cargo.toml`.

Third-party notices: the planned UI toolkit, Slint, is used under its
royalty-free license, which requires an attribution in the app's About page.
