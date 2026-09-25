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

Milestone **M0 (foundation)**:

- `crates/proto/`: vendored tsclientlib (see [UPSTREAM.md](crates/proto/UPSTREAM.md) for the patch log)
- `tools/tsctl`: headless CLI that connects, prints the channel tree, sends and receives chat
- `dev/`: TeamSpeak 3.13 and TeamSpeak 6 (6.0.0-beta13.1) servers for local testing
- CI: build and test on Linux and Windows, `cargo-deny`, smoke test against both servers

No GUI, voice or screen sharing yet.

## Try it

```sh
# Local servers (use REGISTRY=mirror.gcr.io if Docker Hub rate-limits you)
docker compose -f dev/docker-compose.yml up -d

cargo run -p tsctl -- connect 127.0.0.1:9987 tree            # TeamSpeak 3
cargo run -p tsctl -- connect 127.0.0.1:9988 tree            # TeamSpeak 6
cargo run -p tsctl -- connect 127.0.0.1:9988 --nick me repl  # interactive chat

scripts/it-smoke.sh   # tree + server/channel chat round-trips on both servers
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
