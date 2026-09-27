# TeamSpeak 6: streams that started before we arrived

**Question.** The server sends `notifystreamstarted` only to the clients that
are in the streamer's channel when a stream starts. A client that connects
later, or enters the channel later, sees only the streamer's
`client_is_streaming=1`. How can it learn the stream id, so that it can send
`joinstreamrequest`?

**Answer.** `requeststreaminfo clid=<streamer>` works. The server answers with
`notifystreaminfo`: one part per stream of that client, each with the stream
id and the stream's properties. This works from any channel and needs no
permission beyond a guest's. Voelin uses it (see "What Voelin does" below).

Server: `teamspeaksystems/teamspeak6-server` 6.0.0-beta13.1 [Build: 1790080330],
default permissions, dev/docker-compose.yml. Probed on 2026-09-26 with
`voelinctl probe-stream` (`tools/voelinctl/src/probe.rs`).

## How the probe works

```
voelinctl probe-stream 127.0.0.1:9988 --streamer-identity dev/.state/ts6-admin.json
```

1. Client A creates a channel (`channelcreate`), which moves it there, and
   starts a stream with `setupstream` (signalling only, no media).
2. Client B connects to the default channel and tries candidate commands.
3. B moves into A's channel and tries them again. Extra commands can be given
   with `--try '<raw command>'`; `{id}`, `{a}`, `{b}` and `{cid}` are replaced
   by the stream id, the two client ids and A's channel.
4. B sends `joinstreamrequest` with the id it learned.
5. A sends `updatestream` in several spellings, `requeststreaminfo` for
   itself, and a second `setupstream`.
6. A puts the stream id into its `client_meta_data`. B reconnects straight
   into A's channel and tries again.
7. A clears `client_meta_data` and stops the stream.

The probe hooks into the raw connection (tsclientlib's `unstable` API) and
prints every command both clients send and receive. It prints them unparsed,
so notifications the vendored declarations did not know yet
(`notifystreamupdated`) show up too.

## What the server answered

Stream id below: `6b5158dd-22ba-4118-b20d-9196fe975ac7`. A is clid 6, B is
clid 7 (8 after reconnecting). `return_code` values are left out.

### (a) Entering the streamer's channel

```
B -> clientmove cid=3 clid=7
B <- notifycliententerview cfid=0 ctid=3 reasonid=2 clid=6 ... client_meta_data client_is_recording=0 client_is_streaming=1 ...
B <- notifyclientmoved cfid=1 ctid=3 reasonid=0 clid=7
B <- error id=0 msg=ok
```

No `notifystreamstarted` and no `notifystreaminfo`. B only gets the
streaming flag. The same happens when B connects straight into A's channel
(step 6): its `notifycliententerview` for A carries `client_is_streaming=1`
and nothing else about the stream.

### (b) `requeststreaminfo`

| Command sent by B | Answer |
|---|---|
| `requeststreaminfo id=<id>` | `error id=1542 msg=missing required parameter` |
| `requeststreaminfo id=<id> clid=6` | `notifystreaminfo clid=6 id=<id> name=late\sjoin\sprobe type=3 accessibility=1 mode=1 viewer=0 bitrate=4608 viewer_limit=0 audio=1`, then ok |
| `requeststreaminfo clid=6` | the same `notifystreaminfo` |
| `requeststreaminfo stream_id=<id>` | 1542 |
| `requeststreaminfo stream_id=<id> clid=6`, `streamid=<id> clid=6`, `id=<id> clid=6 msg` | the same `notifystreaminfo` (only `clid` matters) |
| `requeststreaminfo clid=6 id=00000000-0000-0000-0000-000000000000` | the same `notifystreaminfo` with the real id: `id` is ignored |
| `requeststreaminfo cid=3`, `requeststreaminfo` | 1542 |
| `requeststreaminfo clid=<B itself, not streaming>` | `notifystreaminfo return_code=...` with no stream fields, then ok |
| `requeststreaminfo clid=9999` (no such client) | `notifystreaminfo return_code=...` with no stream fields, then ok (no error) |
| `requeststreaminfo clid=9\|clid=10` | answered for the first part only |

- `clid` is the only required parameter. The notification carries the
  command's `return_code`.
- It works the same from the default channel (A's stream in another channel)
  and from A's channel.
- A client with two streams (A sent a second `setupstream` in step 5) gets
  both streams in one answer, as two parts:

  ```
  notifystreaminfo return_code=0 clid=6 id=6b5158dd-... name=renamed\sagain type=3 accessibility=1 mode=1 viewer=0 bitrate=4000 viewer_limit=0 audio=1|clid=6 id=069ba23c-b71a-4002-9c97-a91d5ab4428b name=second type=3 accessibility=1 mode=1 viewer=0 bitrate=4608 viewer_limit=0 audio=1
  ```

- The field names differ from `notifystreamstarted`: `accessibility` (not
  `access`), and `viewer`, the current number of viewers. Both match the
  `StreamInfoEvent` protobuf message embedded in the server.
- A second `setupstream` while a stream runs is accepted. It creates a
  second stream with a new id, and the channel gets `notifystreamstarted`
  for it.

### (c) `clientgetvariables clid=6`

- From the default channel, without `channelsubscribeall`, the answer is
  `error id=512 msg=invalid clientID`: A is not visible to B.
- In A's channel the answer is `notifyclientupdated clid=6 client_version=...
  client_platform=... client_created=... client_totalconnections=... ...`.
  It has nothing about streams.

### (d) `clientinfo clid=6`

The answer has `client_is_streaming=1` among the usual properties, and no
stream id.

### (e) Other command names

`requeststreamattendees id=<id>`, `streamattendees id=<id>`, `streamlist`,
`liststreams`, `requeststreams` and `requeststreamlist` all fail with
`error id=256 msg=command not found`. The server binary names no other
stream command in text form: `setupstream`, `stopstream`, `updatestream`,
`requeststreaminfo`, `joinstreamrequest`, `respondjoinstreamrequest`,
`streamsignaling` and `removeclientfromstream` are all of them.
`notifystreamattendees` (protobuf `StreamAttendeesEvent`: `return_code`,
`client_id`) never appeared.

### (f) `updatestream` by the streamer

| Command sent by A | Answer |
|---|---|
| `updatestream id=<id> name=renamed\sprobe` | ok; **B** (in the channel) and A get `notifystreamupdated clid=6 id=<id> name=renamed\sprobe` |
| `updatestream id=<id>` | ok, no notification |
| `updatestream stream_id=<id> name=...` | 1542 (`id` is required) |
| `updatestream id=<id> name=renamed\sprobe type=3 bitrate=4608 accessibility=1 mode=1 viewer_limit=0 audio=1` | ok, no notification (nothing changed) |
| `updatestream id=<id> name=renamed\sagain type=3 access=1 mode=1 bitrate=4000 viewer_limit=0 audio=1` | ok; `notifystreamupdated clid=6 id=<id> name=renamed\sagain bitrate=4000` |

`notifystreamupdated` carries only the fields that changed, and goes only to
the clients in the channel. It re-announces nothing to clients that do not
know the stream: with no change it sends nothing at all. So it is no way
to tell latecomers about a stream.

### Joining with a learned id

`joinstreamrequest id=<id> clid=6 msg=probe is_remove=0` from B, with the id
learned through `requeststreaminfo`, reaches A as
`notifyjoinstreamrequest clid=7 id=<id> msg=probe is_remove=0`. `is_remove=1`
is relayed too. B's reconnect in step 6 gave the same result. The full
join with media works (see "Verification" below).

The server does not limit join requests to the streamer's channel. In a
later run (2026-09-27, same server), B stayed in the default channel, sent
`channelsubscribeall`, looked the stream up and asked to join:

```
B -> requeststreaminfo clid=2
B <- notifystreaminfo clid=2 id=e977003a-... name=late\sjoin\sprobe type=3 accessibility=1 mode=1 viewer=0 bitrate=4608 viewer_limit=0 audio=1
B -> joinstreamrequest id=e977003a-... clid=2 msg=probe is_remove=0
A <- notifyjoinstreamrequest clid=3 id=e977003a-... msg=probe is_remove=0
```

So a stream is "channel scoped" only in whom the server announces it to. A
streamer that wants to admit only its channel has to check where the viewer
is. Voelin's `stream.permissions = channel` does that.

### `client_meta_data` as a side channel

`clientupdate client_meta_data=voelin-stream=<id>` is accepted from a guest.
The channel gets `notifyclientupdated clid=6 client_meta_data=voelin-stream=<id>`,
and a client that arrives later finds the value in its `notifycliententerview`
for A. So metadata would work as a fallback between Voelin clients, but
`requeststreaminfo` makes it unnecessary.

### Stopping

`stopstream id=<id> reason=1`: B, in the channel, gets
`notifystreamstopped clid=6 id=<id> reason=1`.

## What Voelin does

- `voelin_stream::proto::stream_info` sends `requeststreaminfo clid=<streamer>`.
  The declarations (`crates/proto/tsproto-structs/declarations/Messages.toml`)
  now know the answer's fields (`accessibility`, `viewer`, `return_code`, an
  optional `id` for the empty answer) and `notifystreamupdated`.
- `voelin_stream::discovery::Discovery` follows the clients on the server:
  their channel and `client_is_streaming`. It keeps the stream directory to
  the streams in our own channel. It looks up a client that streams in our
  channel and whose stream we were not told:
  - after connecting;
  - after we or the streamer change channels;
  - when a streamer in our channel starts and its `notifystreamstarted`
    has not arrived by the next update of the client list.

  Each streamer is looked up once per change of its channel or flag. The
  answer arrives as `notifystreaminfo` and fills the directory like an
  announcement, so `Event::StreamsChanged` lists the stream and it can be
  watched like any other.
- Where to look is a `StreamLookup` trait. `ServerLookup` (`requeststreaminfo`)
  is the only lookup now. It stops asking on a connection whose server answers
  "command not found". A later source, such as a gateway's stream directory,
  plugs in with `Streams::add_lookup`, and hands what it finds to
  `Streams::discovered`.
- `notifystreamupdated` updates the directory (name, bitrate, …).
- The engine (`voelin-core`) and `voelinctl stream list/watch` feed the
  client list into `Streams::update_clients`.

Fallbacks (i) and (ii) from the plan were not built, because the native
command works. (i) would have been a re-announcement by the streamer; (ii)
would have put the stream id into `client_meta_data`. The probe shows what
they would do: (i) does not work through `updatestream`, and (ii) would work
between Voelin clients.

### Limits

- Streams of official clients are found the same way, since this is a
  server command: nothing on the streamer's side is needed.
- Only clients we can see are looked up. That is always true of our own
  channel, which is the only place streams are listed.
- Not probed: streams with `accessibility` other than 1 (public), and
  whether a server permission can hide streams from `requeststreaminfo`.

## Verification

- `voelinctl connect 127.0.0.1:9988 --nick early stream --loopback start --synthetic --auto-accept`,
  then, once it is live, `voelinctl connect 127.0.0.1:9988 stream --loopback watch --streamer-nick early --expect-frames 30`:
  the late viewer printed `watching <id> of early`, connected and decoded 30
  pictures.
- `scripts/it-smoke.sh`, step "stream (late viewer)", does the same.
- `crates/voelin-core/tests/stream_live.rs` `ts6_late_viewer_finds_running_stream`
  (`VOELIN_LIVE=1`): an engine session connects after the stream is live,
  gets it in `Event::StreamsChanged`, watches it and receives frames.
- `crates/voelin-stream/tests/sessions.rs` `late_viewer_finds_the_stream`
  runs the same flow through a fake server, and `discovery.rs` has unit tests
  for when lookups happen.
