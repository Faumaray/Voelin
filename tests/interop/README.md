# Browser interop test

`crates/voelin-stream/tests/browser_interop.rs` checks our str0m peers, and
`crates/voelin-core/tests/browser_codecs.rs` our real encoders, against the
WebRTC stack of Chromium (libwebrtc, the same stack as the official
TeamSpeak 6 client). `browser-peer.cjs` runs headless Chromium through
Playwright and exchanges SDP and candidates with the Rust tests as JSON lines
on stdin/stdout (`browser.rs` is the Rust side, shared by both tests). No
TeamSpeak server is involved.

| Test | What it checks |
|---|---|
| `rust_streams_to_browser` | Our streamer offer (`PeerConfig::loopback()`, VP8 + Opus) answered by an `RTCPeerConnection`; `SyntheticSource` media for 3 s; `inbound-rtp` stats: VP8 packets and bytes, decoded frames (the synthetic frames are real 1x1 VP8 keyframes), Opus packets; the `transport` stats' `srtpCipher` is `AES_CM_128_HMAC_SHA1_80` (our SRTP order, we are the DTLS server); our bandwidth estimation gets estimates from Chromium's transport-cc feedback |
| `browser_streams_to_rust` | Chromium offers a canvas `captureStream` and an oscillator with VP8, VP9, H264 and AV1 preferred in turn (codecs the browser cannot send are skipped); our viewer `Peer::answer` must receive frames of that codec and Opus, over `AES_CM_128_HMAC_SHA1_80`. VP8 also uses trickled browser candidates and checks that a PLI brings a keyframe |
| `real_encoders_decode_in_browser` (voelin-core) | Every encoder this machine has (`Codecs::encoders()`), as a share uses it: the 1280x720 test pattern through the `Streamer`, VA-API encoders also from DMA-BUFs as the ScreenCast portal hands over the screen; our streamer peer offers that codec alone (H.264 as Constrained Baseline, see below); the browser must decode at least 60 frames at 1280x720 in 3 s. Codecs the browser does not answer (HEVC, H.264 in Playwright's Chromium) are skipped |
| `share_offer_picks_a_codec_every_client_decodes` (voelin-core) | The offer a share makes on this machine (`preferred_codec`, `peer_config`): the browser, answering with the first codec of the offer it lists as the official client does, must pick one every TeamSpeak client decodes (`decoded_everywhere`), and decode it |
| `browser_renegotiates_with_rust` | Chromium streams VP8, then sends a new offer on the same connection with VP9 first (as the official client re-offers when it does not encode the codec we chose); `Peer::renegotiate` answers on the same peer, in the new offer's codec order; VP9 frames must arrive |
| `trickled_candidates_reach_browser` | A server-reflexive candidate (from a fake STUN server) as our sessions trickle it (`iceCandidate` signal with the peer's mid) is accepted by `addIceCandidate` |

## Running

Needs Node.js with Playwright and its Chromium (`npx playwright install
chromium` on a new machine; `PLAYWRIGHT_BROWSERS_PATH` if the browsers live
elsewhere).

```sh
VOELIN_INTEROP=1 cargo test -p voelin-stream --test browser_interop -- --nocapture
VOELIN_INTEROP=1 cargo test -p voelin-core --features media-desktop --test browser_codecs -- --nocapture
```

Without Playwright's own browsers, `playwright-core` drives an installed
Chromium: `npm install playwright-core` somewhere, then
`NODE_PATH=<that>/node_modules VOELIN_CHROMIUM=/usr/bin/chromium`. A system
Chromium usually decodes H.264, which Playwright's build does not. Run it
headless and away from the desktop session, e.g. with `WAYLAND_DISPLAY` and
`DISPLAY` unset under `dbus-run-session`.

Without `VOELIN_INTEROP=1` the tests return immediately, so `cargo test` passes on
machines without a browser. `VOELIN_NODE` selects the node binary; `NODE_PATH`
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
- Chromium 141 offers AEAD_AES_256_GCM, AEAD_AES_128_GCM and
  AES_CM_128_HMAC_SHA1_80 as DTLS client; with our default order
  (`PeerConfig::srtp_profiles`) the connection uses AES_CM_128_HMAC_SHA1_80
  (`getStats()` transport: `srtpCipher`, DTLS 1.2 `FEFD`,
  `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`). Before, dimpl picked AES-256-GCM.
  Chromium 152 names the same cipher `SRTP_AES128_CM_HMAC_SHA1_80`.
- Chromium 152 (Arch Linux build) decodes every encoder's stream on this
  machine (AMD, VA-API): H.264 and AV1 from VA-API (from memory and from
  DMA-BUFs), VP8 and VP9 from libvpx, x264, SVT-AV1 and libaom, all at
  1280x720, about 100 frames in 3.4 s, no freezes.
- Headless Chromium 152 lists H.264 only in the profiles of libwebrtc's
  software decoder (Baseline, Constrained Baseline, Main, packetization modes
  0 and 1): it does not answer an offer of Constrained High alone
  (`640c1f`, what our streamers offer), yet decodes High profile streams
  when it negotiated another profile (its decoder is FFmpeg's).
- With str0m's bandwidth estimation and pacer on our streamer peer, Chromium's
  transport-cc feedback yields estimates within the first second (about
  640 kbit/s rising towards the desired bitrate with the 720 kbit/s synthetic
  stream).
