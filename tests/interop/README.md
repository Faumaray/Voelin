# Browser interop test

`crates/tsc-stream/tests/browser_interop.rs` checks our str0m peers against
the WebRTC stack of Chromium (libwebrtc, the same stack as the official
TeamSpeak 6 client). `browser-peer.cjs` runs headless Chromium through
Playwright and exchanges SDP and candidates with the Rust test as JSON lines
on stdin/stdout. No TeamSpeak server is involved.

| Test | What it checks |
|---|---|
| `rust_streams_to_browser` | Our streamer offer (`PeerConfig::loopback()`, VP8 + Opus) answered by an `RTCPeerConnection`; `SyntheticSource` media for 3 s; `inbound-rtp` stats: VP8 packets and bytes, decoded frames (the synthetic frames are real 1x1 VP8 keyframes), Opus packets |
| `browser_streams_to_rust` | Chromium offers a canvas `captureStream` and an oscillator with VP8, VP9, H264 and AV1 preferred in turn (codecs the browser cannot send are skipped); our viewer `Peer::answer` must receive frames of that codec and Opus. VP8 also uses trickled browser candidates and checks that a PLI brings a keyframe |
| `trickled_candidates_reach_browser` | A server-reflexive candidate (from a fake STUN server) as our sessions trickle it (`iceCandidate` signal with the peer's mid) is accepted by `addIceCandidate` |

## Running

Needs Node.js with Playwright and its Chromium (`npx playwright install
chromium` on a new machine; `PLAYWRIGHT_BROWSERS_PATH` if the browsers live
elsewhere).

```sh
TSC_INTEROP=1 cargo test -p tsc-stream --test browser_interop -- --nocapture
```

Without `TSC_INTEROP=1` the tests return immediately, so `cargo test` passes on
machines without a browser. `TSC_NODE` selects the node binary; `NODE_PATH`
defaults to `npm root -g` so a global Playwright is found.

Chromium is started with `--disable-features=WebRtcHideLocalIpsWithMdns`:
without camera/microphone permission it would hide host candidates behind
mDNS names (`<uuid>.local`), which str0m cannot resolve. The official client
may do the same; see `docs/protocol-notes/ts6-streaming.md`.

## Findings

- Chromium 141 (Playwright 1.56) connects to a str0m peer on 127.0.0.1 from its
  own host candidate on the primary interface; it gathers no loopback candidates.
- str0m's answer lists codecs in its own configured order, and the offerer
  sends the first one. `Peer::answer` therefore configures the viewer's codecs
  in the order of the offer (`accept_video_codecs` filters them).
- `Peer::write` sends on the first negotiated payload type of a kind, so a
  streamer offers only the codecs its encoder produces (`video_codecs`,
  default VP8 only).
- str0m mids are random strings (`a=mid:fhs`), not `0`/`1`; trickled
  candidates must carry the real mid, or browsers reject them.
- Playwright's Chromium build sends VP8, VP9 and AV1, not H.264.
