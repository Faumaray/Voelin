# TeamSpeak 6: protocol notes

Everything here is reverse-engineered or collected from public sources. Mark each
item as **confirmed** (seen against our own server or in working code) or
**reported** (single secondary source). Update this file whenever a probe or
capture teaches something new.

## Legacy protocol on TS6 servers

- **Confirmed** (voelinctl, 6.0.0-beta13.1, 2026-09-25): the TS3 client protocol
  connects (`initivexpand2` license chain with the TS5-server block type),
  channel tree, server chat and channel chat work. Client version: the vendored
  default `Windows_3_X_X__1`.
- **Confirmed**: new fields the vendored declarations do not know yet (logged as
  "Unknown argument"):
  - `initserver`: `virtualserver_address`, `virtualserver_version_sign`
  - `initserver` / `notifycliententerview`: `client_is_streaming`
- **Confirmed**: unique ids differ. TeamSpeak 3 uses `base64(SHA1(omega))`
  (28 characters), TeamSpeak 6 uses `base64(SHA256(omega))` (44 characters),
  where omega is the base64 public key string. The same identity therefore has
  a different `client_unique_identifier` on each server generation
  (`voelin-gateway-proto::UniqueIds`).
- **Confirmed**: TS6 has no raw ServerQuery port; SSH query on 10022 and HTTP
  WebQuery on 10080 (HTTPS 10443).
- **Reported**: beta13 made the Init1 puzzle difficulty adaptive to the server's
  protection level.

## Screen sharing / streams

Transport (**reported**, TeamSpeak staff on community.teamspeak.com):

- WebRTC peer-to-peer between clients; the server only relays signalling. No SFU yet.
- ICE with STUN only (`turn.teamspeak.com`, `turn2.teamspeak.com`; no TURN relay).
- Media ports: UDP 49152–65535.
- Streams are channel-scoped: "public" means everyone in the streamer's channel.
- Bitrate capped at 10 Mbit/s.

Codecs (**reported**, from the ts6-manager bot's working code):

- Video: H.264 Constrained High (the only H.264 profile the TS client decodes),
  VP8, VP9, AV1. H.264 only when the client could download OpenH264 (see
  "Codecs the official client decodes" below).
- Audio: Opus, 48 kHz stereo, payload type 111.

Commands over the normal client command channel (**reported**, ts6-manager,
which uses a TS3-signed client version over the legacy UDP transport):

| Direction | Command | Parameters |
|---|---|---|
| C→S | `setupstream` | `name type bitrate accessibility mode viewer_limit audio` (bot defaults: `type=3 bitrate=4608 accessibility=1 mode=1 viewer_limit=0 audio=1`) |
| C→S | `respondjoinstreamrequest` | `id clid msg offer=<SDP> decision=1\|0` |
| C→S | `streamsignaling` | `id clid json={"cmd":…,"args":{…}}` (answer, ICE candidates `candidate`/`sdpMid`/`sdpMlineIndex`, reconnect) |
| C→S | `joinstreamrequest` | `id clid msg is_remove` (viewer; confirmed, see below) |
| C→S | `stopstream` | `id reason` (confirmed) |
| C→S | `removeclientfromstream` | `id clid reason` (confirmed) |
| S→C | `notifystreamstarted`, `notifystreamstopped`, `notifystreaminfo` | |
| S→C | `notifyjoinstreamrequest`, `notifyrespondjoinstreamrequest` | |
| S→C | `notifystreamsignaling` | |
| S→C | `notifystreamclientjoined`, `notifystreamclientleft` | |

Flow: a viewer's join request arrives at the streamer as
`notifyjoinstreamrequest`; the streamer creates one peer connection per viewer and
sends its SDP offer in `respondjoinstreamrequest`; the viewer's answer and ICE
candidates come back through `streamsignaling` / `notifystreamsignaling`.

### Probe results (confirmed, 6.0.0-beta13.1, default permissions, 2026-09-25)

Sent with `voelinctl connect 127.0.0.1:9988 raw '<command>'` as a plain guest:

| Command | Result |
|---|---|
| `setupstream name=probe type=3 bitrate=4608 accessibility=1 mode=1 viewer_limit=0 audio=1` | ok; a guest may start a stream by default |
| `joinstreamrequest id=<uuid>` (also with `clid=`, `msg=`) | exists, `error id=1542` missing required parameter |
| `streamsignaling id=1 clid=1 json={}` | exists (`ClientInvalidId`) |
| `stopstream id=<x>` | exists, missing parameter |
| `requestjoinstream`, `joinstream` | `CommandNotFound` |

After `setupstream` the server sends, to the streamer:

```
notifyclientupdated clid=20 client_is_streaming=1
notifystreamstarted clid=20 id=e7da957e-7ce0-46cf-bf6d-754d3446b4d4 name=probe type=3 access=1 mode=1 bitrate=4608 viewer_limit=0 audio=1 return_code=0
```

Other clients in the channel receive the same `notifystreamstarted` (without
`return_code`) and `notifyclientupdated clid=<streamer> client_is_streaming=1`;
`client_is_streaming` is also part of `notifycliententerview` and `initserver`.
When the streamer disconnects, everyone gets `notifystreamstopped id=<uuid>`.

Stream ids are UUIDs. The notification carries the command's `return_code`, and
the field is named `access` there (not `accessibility`).

### Join handshake (confirmed, 6.0.0-beta13.1)

```
viewer   -> server: joinstreamrequest id=<uuid> clid=<streamer clid> msg=<text> is_remove=0
streamer <- server: notifyjoinstreamrequest clid=<viewer clid> id=<uuid> msg=<text> is_remove=0
```

All four parameters are required. `is_remove=1` withdraws the request; the
server also sends `notifyjoinstreamrequest ... is_remove=1` to the streamer when
the viewer disconnects. Found by testing candidate parameter names from the
server binary against a live stream (`is_remove` matches the
`JoinStreamRequestEvent.is_remove` protobuf field).

### End-to-end stream (confirmed, 6.0.0-beta13.1, 2026-09-25)

`crates/voelin-stream/tests/live_ts6.rs` runs a whole stream between two of our
clients through the server, with str0m on both ends:

```
streamer -> setupstream name=live\stest type=3 bitrate=4608 accessibility=1 mode=1 viewer_limit=0 audio=1
streamer <- notifystreamstarted clid=<s> id=<uuid> ... return_code=...   (viewer gets it without return_code)
viewer   -> joinstreamrequest id=<uuid> clid=<s> msg=... is_remove=0
streamer <- notifyjoinstreamrequest clid=<v> id=<uuid> msg=... is_remove=0
streamer -> respondjoinstreamrequest id=<uuid> clid=<v> msg offer=<SDP offer> decision=1
viewer   <- notifyrespondjoinstreamrequest id=<uuid> ... offer=<SDP, unchanged> decision=1
viewer   -> streamsignaling id=<uuid> clid=<s> json={"cmd":"answer","args":{"answer":<SDP>}}
streamer <- notifystreamsignaling id=<uuid> clid=<v> json=<unchanged>
           ICE + DTLS directly between the peers; VP8 video and Opus audio flow
streamer -> removeclientfromstream id=<uuid> clid=<v> reason=5
viewer   <- notifystreamclientleft id=<uuid> clid=<v> reason=5   (streamer: + return_code)
streamer -> stopstream id=<uuid> reason=1
viewer   <- notifystreamstopped id=<uuid>
```

- `stopstream id=<uuid>` alone and `stopstream id clid` fail with
  `ParameterMissing`; `stopstream id reason` works. `removeclientfromstream id clid`
  fails the same way; it needs `reason` too. Reasons are `StreamLeaveReason` values.
- The server passes SDP and JSON through unchanged (escaped as usual).
- Host candidates embedded in the SDP are enough on one machine; trickled
  candidates use `{"cmd":"iceCandidate","args":{"candidate","sdpMid","sdpMLineIndex"}}`
  (the parser also accepts `sdp`/`mid`/`mLine`).

### Schema embedded in the server (confirmed)

The server binary embeds the protobuf descriptors of the new binary protocol
(`client/streaming.proto`, `events/stream_events.proto`). The text commands
are separate handlers with their own parameter names, but the events and the
enums line up:

| Enum | Values |
|---|---|
| `StreamType` (`type`) | 1 none, 2 camera, **3 screen**, 4 window, 5 existing session, 6 voice |
| `StreamAccessibility` (`accessibility`/`access`) | 1 none, 2 public, 3 contacts only, 4 private |
| `StreamMode` (`mode`) | 1 none, 2 P2P, 3 SFU |
| `StreamLeaveReason` (`reason`) | 1 none, 2 left, 3 denied, 4 failed, 5 kicked, 6 banned |

Events (text notification names in brackets):

| Event | Fields |
|---|---|
| StreamStarted (`notifystreamstarted`) | client_id, session_id, name, type, access, mode, bitrate, viewer_limit, audio |
| StreamStopped (`notifystreamstopped`) | client_id, session_id, reason |
| StreamUpdated | client_id, session_id, name, type, access, mode, bitrate, viewer_limit, audio |
| StreamInfo (`notifystreaminfo`) | return_code, client_id, session_id, name, type, accessibility, mode, viewer, bitrate, viewer_limit, audio |
| JoinStreamRequest (`notifyjoinstreamrequest`) | client_id, session_id, message, is_remove |
| RespondJoinStreamRequest (`notifyrespondjoinstreamrequest`) | client_id, session_id, message, decision, offer |
| StreamSignaling (`notifystreamsignaling`) | client_id, session_id, json |
| StreamClientJoined / StreamClientLeft | client_id, session_id (, reason) |

In text form `client_id` is `clid`, `session_id` is `id` and `message` is `msg`.
The binary protocol's requests (`SetupStreamRequest` with `max_width`,
`max_height`, `max_framerate`, `codec`, `properties`, and uint64 stream ids)
differ from the text commands. There is also a `virtualserver_sfu_endpoint`
property for the planned SFU mode.

Errors: `setupstream` refused with error 2568 ("insufficient client
permissions") has been seen; no stream permission names are documented.

More text commands and notifications named in the server binary:
`requeststreaminfo`, `updatestream` and `notifystreamupdated` (probed, see
below), and `notifystreamattendees` (`return_code`, `client_id`; never seen).
The protobuf `RequestStreamInfoRequest` only has `stream_id`, but the text
command takes `clid` instead.

### Streams that started before we joined (confirmed, 6.0.0-beta13.1, 2026-09-26)

`initserver` and `notifycliententerview` carry `client_is_streaming`, but no
stream id. The server sends `notifystreamstarted` only to the clients in the
streamer's channel when the stream starts. A client that connects later, or
enters the channel later, gets nothing about running streams.

`requeststreaminfo clid=<streamer>` fills the gap. It answers with
`notifystreaminfo` (the command's `return_code`, then one part per stream of
the client: `clid id name type accessibility mode viewer bitrate viewer_limit
audio`), from any channel. For a client without streams, or an unknown
client, the answer has no stream parts.

`updatestream id=<id> [name type access mode bitrate viewer_limit audio]`
notifies the channel with `notifystreamupdated clid id <changed fields>`. It
sends nothing when nothing changed.

Full transcript, all the variants tried, and what Voelin does with them:
[docs/research/ts6-late-join.md](../research/ts6-late-join.md). Voelin
looks up unannounced streams in its channel with `requeststreaminfo`
(`voelin_stream::discovery`).

### Viewer counts (confirmed, 6.0.0-beta13.1, 2026-10-03)

Streamer A, viewers B and B2, bystander C in one channel, every command
logged (`--log-commands`):

- `viewer` of `notifystreaminfo` is the number of viewers the streamer
  accepted: `requeststreaminfo clid=<A>` answered `viewer=1` while B2
  watched and `viewer=0` after it left.
- `notifystreamclientjoined clid=<viewer> id=<stream>` goes to everyone in
  the channel (C got one for B and one for B2), but not to the viewer who
  joined.
- `notifystreamclientleft clid=<viewer> id=<stream> reason=1` goes only to
  the streamer and to the viewer who left (`joinstreamrequest ...
  is_remove=1`); C got nothing, also not when B left the server.
- No `notifystreamupdated` when viewers come or go (`StreamUpdated` has no
  viewer field).

So a client counts joins itself, and asks `requeststreaminfo` again now and
then for the leaves it is not told about: `StreamInfo::viewers`
(`voelin_stream`), kept by `StreamDirectory` and refreshed one stream at a
time every 5 s by the engine (`Streams::refresh_viewer_counts`).

### WebRTC interop with Chromium (confirmed, Chromium 141, 2026-09-26)

`crates/voelin-stream/tests/browser_interop.rs` (`VOELIN_INTEROP=1`, see
`tests/interop/README.md`) runs our str0m peers against headless Chromium's
libwebrtc, the stack the official client is built on:

- Our streamer offer (VP8 + Opus, BUNDLE, `setup:actpass`, host candidates in
  the SDP) is accepted; Chromium answers `setup:active` and decodes the
  synthetic 1x1 VP8 keyframes. Opus packets arrive.
- Chromium's offers (canvas video + oscillator audio) with VP8, VP9 or AV1
  preferred are answered by our viewer, which receives frames of that codec
  and Opus; a PLI from us yields a VP8 keyframe. Playwright's Chromium cannot
  send H.264.
- Trickled candidates must carry the real `sdpMid`: str0m's mids are random
  strings (e.g. `fhs`), not `0`. Our `iceCandidate` signals use the peer's
  first mid and `sdpMLineIndex` 0; Chromium's `addIceCandidate` accepts them.
- Without camera/microphone permission Chromium hides host candidates behind
  mDNS names (`<uuid>.local`), which str0m cannot resolve. The test disables
  that; the official client may send mDNS candidates, which would then need a
  resolver (or its server-reflexive candidates) on our side.
- str0m answers in its own codec order and the offerer sends the answer's
  first codec, so `Peer::answer` orders the viewer's codecs like the offer.
- SRTP profile: libwebrtc answers our `setup:actpass` offer as DTLS client
  (`setup:active`) and offers AEAD_AES_256_GCM, AEAD_AES_128_GCM and
  AES_CM_128_HMAC_SHA1_80; the server picks. dimpl, str0m's DTLS, always
  picked AES-256-GCM, while streams between official clients are reported to
  use AES_CM_128_HMAC_SHA1_80 (and official clients failed with ours). Our
  peers now pick in the order of `PeerConfig::srtp_profiles`, by default
  AES_CM_128_HMAC_SHA1_80 first; Chromium's `getStats()` confirms
  `srtpCipher: AES_CM_128_HMAC_SHA1_80` (DTLS 1.2,
  `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`). Whether this was what broke the
  official client is still to be confirmed with it.
- With bandwidth estimation on our streamer peer, Chromium's transport-cc
  feedback gives estimates; the offer is the same as without (str0m always
  offers transport-cc and abs-send-time).

### Codecs the official client decodes (confirmed, client 6.0.0-beta4.1, 2026-10-03)

From the official Linux client's log (`~/.cache/TeamSpeak/Default/logs/`)
while it watched a Voelin stream, and the symbols of its binary:

- At start-up it downloads Cisco's OpenH264
  (`http://ciscobinary.openh264.org/libopenh264-2.6.0-linux64.8.so.bz2`); here
  the download failed (`Failed to download openh264 binary http code: 403`).
- Voelin offered H.264 first (the hardware encoder's codec), with every H.264
  variant str0m knows. The client answered H.264 `profile-level-id=42e01f`
  (packetization mode 1), then could not make a decoder for it: `Trying to
  create decoder for unsupported format. Codec name: H264` — libwebrtc's
  `NullVideoDecoder`, nothing shown. Its SDP capabilities list H.264 whether
  or not OpenH264 loaded.
- Its decoders: `FFmpegDecoderVP8`, `FFmpegDecoderVP9`, `FFmpegDecoderAV1`
  (FFmpeg bundled in `/opt/teamspeak`, dav1d and libaom linked in), AMF
  hardware decoders for H.264 and AV1, and OpenH264 (`H264DecoderImpl`) once
  downloaded. VP8, VP9 and AV1 therefore always decode; H.264 depends on the
  download.
- It answers with the first codec of the offer it lists, like Chromium.

Voelin now offers codecs every client decodes first (`decoded_everywhere`)
and H.264 only in the profile it encodes. The real encoders' streams (VA-API
H.264 and AV1, from memory and from DMA-BUFs; libvpx VP8 and VP9; x264,
SVT-AV1, libaom) decode in Chromium 152's libwebrtc at full size
(`crates/voelin-core/tests/browser_codecs.rs`). Watching such a stream with
the official client itself is still to be confirmed: the beta needs a
myTeamSpeak sign-in on first start, so it cannot run from a throwaway
profile.

## Open questions

- [x] Viewer side: `joinstreamrequest id clid msg is_remove` (see above).
- [x] Meaning of `setupstream` `type`, `accessibility` and `mode` values (enums above).
- [x] Parameters of `stopstream` and `removeclientfromstream` (`reason` is required).
- [ ] Which permission gates `setupstream` (error 2568).
- [x] Contents of `notifystreamstarted` (see probe results).
- [x] Contents of `notifystreaminfo` (`requeststreaminfo clid=<streamer>`, see above).
- [x] Whether a client that connects later is told about running streams: no (see above).
- [x] How a client finds running streams after connecting: `requeststreaminfo`
      (presumably what the official client does too; not captured from it).
- [ ] Interop with the official TS6 client (its offer/answer details, whether
      it trickles candidates, mDNS host candidates). The codecs it picks and
      decodes are known (see above); Chromium's WebRTC stack interoperates.
- [ ] Whether stream audio can be sent without video.
- [ ] The WebRTC/protobuf transport some TS6 clients use on UDP 9987
      (`client_protocol_format=proto`, reported in community reverse engineering).
      Not needed while the legacy transport works.

## Sources

- clusterzx/ts6-manager: `packages/backend/src/voice/streaming/stream-signaling.ts`,
  `packages/backend/src/voice/tslib/client.ts`
- community.teamspeak.com threads 56850 (how screen share works), 58890 (TS3
  servers have no screen share), 62456 (port range), 62512 (bitrate limit)
- github.com/teamspeak/teamspeak6-server `CONFIG.md`, releases
