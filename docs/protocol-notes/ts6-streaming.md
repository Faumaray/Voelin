# TeamSpeak 6: protocol notes

Everything here is reverse-engineered or collected from public sources. Mark each
item as **confirmed** (seen against our own server or in working code) or
**reported** (single secondary source). Update this file whenever a probe or
capture teaches something new.

## Legacy protocol on TS6 servers

- **Confirmed** (tsctl, 6.0.0-beta13.1, 2026-09-25): the TS3 client protocol
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
  (`tsc-gateway-proto::UniqueIds`).
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
  VP8, VP9, AV1.
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

Sent with `tsctl connect 127.0.0.1:9988 raw '<command>'` as a plain guest:

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

`crates/tsc-stream/tests/live_ts6.rs` runs a whole stream between two of our
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

## Open questions

- [x] Viewer side: `joinstreamrequest id clid msg is_remove` (see above).
- [x] Meaning of `setupstream` `type`, `accessibility` and `mode` values (enums above).
- [x] Parameters of `stopstream` and `removeclientfromstream` (`reason` is required).
- [ ] Which permission gates `setupstream` (error 2568).
- [x] Contents of `notifystreamstarted` (see probe results).
- [ ] Contents of `notifystreaminfo` (`requeststreaminfo`).
- [ ] Interop with the official TS6 client (its offer/answer details, codecs it
      actually picks, whether it trickles candidates).
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
