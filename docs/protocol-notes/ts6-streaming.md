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
| C→S | `stopstream` | `id`? |
| C→S | `removeclientfromstream` | `id clid`? |
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

Stream ids are UUIDs. The notification carries the command's `return_code`, and
the field is named `access` there (not `accessibility`).

Errors: `setupstream` refused with error 2568 ("insufficient client
permissions") has been seen; no stream permission names are documented.

## Open questions

- [ ] Viewer side: the command is `joinstreamrequest`, but `id`, `clid` and `msg`
      are not enough. Find the missing parameter(s) with
      `tsctl connect ... --log-commands raw '<candidate>'` (or the REPL's `/raw`)
      while another client streams.
- [ ] Meaning of `setupstream` `type`, `accessibility` and `mode` values.
- [ ] Parameters of `stopstream` (more than `id`) and `removeclientfromstream`.
- [ ] Which permission gates `setupstream` (error 2568).
- [ ] Contents of `notifystreaminfo` and `notifystreamstarted`.
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
