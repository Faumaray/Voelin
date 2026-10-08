# Running the tsgw gateway

`tsgw` lets users of Voelin see who is in which channel and read and
write channel chat **without joining voice**, on a server you administer. It
holds ServerQuery sessions (invisible to normal users of the official clients)
and serves users over a WebSocket (`tsgw.v1+json`, see
[gateway-protocol.md](gateway-protocol.md)). It also adds what TeamSpeak does
not have: pinned messages, reactions, topics, scheduled events, a directory
of running streams and an activity feed. Voelin shows these only on servers
with a gateway; official TeamSpeak clients do not see them (topic posts
appear in the channel as `[#topic] text`).

## What users get

| Feature | How |
|---|---|
| Presence | One query session watches the server; each user sees the channels their `i_channel_subscribe_power` allows, without query clients |
| Channel chat | A relay query session is moved into each channel someone opened; it forwards what is said and posts users' messages under their own nickname, as if they wrote them |
| Server chat | Read by the watcher, posted by the gateway's lookup session |
| History | Relayed messages are stored (SQLite) with stable ids; pages by id or time in both directions, without a size limit unless you set one; `sync` returns what changed since a revision |
| Pins | Pinned messages per chat, kept even when older messages are pruned |
| Reactions | Any emoji on any stored message, with counts and who reacted |
| Topics | Threads in a chat, started from a message or standalone; posts are relayed as `[#Title] text`, and TeamSpeak posts in that form join the topic |
| Events | Scheduled events per server or channel, with RSVP (going / maybe / not going) and reminders; stream events show when the host goes live |
| Stream directory | Voelin streamers register running TeamSpeak 6 streams, so viewers who join later can find them; entries go when the streamer leaves or stops, and clients that stream without registering are listed as detected (without a stream id) |
| Activity feed | Streams started and stopped, events scheduled, starting or cancelled, topics started, messages pinned |

The nickname in relayed posts is the one the server has on record for the
user's identity (from `clientdbinfo`), not something the user can choose. For
each post the relay (or, for server chat, the lookup session) takes that
nickname, sends the message and takes its own name back; while someone on the
server has the nickname it uses `Nick1` to `Nick3`, as TeamSpeak names a second
client of the same name. Only when it can take none of them (a nickname under
three characters, the server refusing the change) does the post go out under
the relay's own name, as `relay.format` (`[Nick] text`).
Each feature can be turned off (`features.*`); pins, reactions and topics
need `history.enabled`.

## Authentication and authorization

Users log in by signing a challenge with their TeamSpeak identity. The
gateway derives the unique id (SHA-1 of the public key on TeamSpeak 3, SHA-256
on TeamSpeak 6) and looks it up with `clientdbfind`, so **an identity must have
connected to the server with voice once** before it can use the gateway.

Checks, mirroring what the user could do with a real connection
(`permoverview`, cached 60 s):

- identity security level ≥ `virtualserver_needed_identity_security_level`
  (and `auth.min_security_level`); not in `banlist`; not in `auth.deny_uids`;
  member of one of `auth.require_server_groups` if set
- read a channel: `i_channel_join_power` ≥ the channel's
  `i_channel_needed_join_power`, and no channel password unless the user has
  `b_channel_join_ignore_password`
- post in a channel: the above plus `b_client_channel_textmessage_send`
- post in server chat: `b_client_server_textmessage_send`
- see a channel in presence (and its events, streams and activity):
  `i_channel_subscribe_power` (server level) ≥ the channel's
  `channel_needed_subscribe_power`

Pins, reactions and topics also need read (reactions, pins) or post (topics)
access to the chat. On top of that, each action has a permission rule, see
[Permission rules](#permission-rules).

Posts are limited to `limits.posts_per_window` per `limits.post_window_secs`
per connection (5 per 5 s by default), other changes to
`limits.actions_per_window` per `limits.action_window_secs` (30 per 10 s).
Every login, post and settings change is written to the audit table.

## Setup

1. Enable ServerQuery. TeamSpeak 6 has it off by default:
   `TSSERVER_QUERY_SSH_ENABLED=1` (the gateway needs events, so SSH, not HTTP).
   TeamSpeak 3: raw (10011) or SSH (10022).
2. Use a query login with enough rights for `clientdbfind`, `clientdbinfo`,
   `permoverview`, `banlist`, `clientmove`, `sendtextmessage`,
   `servergroupsbyclientid` and `channelgroupclientlist`, e.g. a dedicated
   login in the Server Admin group.
3. Put the address the server sees for the gateway into the server's
   `query_ip_allowlist.txt` and set `query.allowlisted = true`. The server's
   flood protection counts the query commands of each address, all of
   tsgw's connections together and the SSH login included: it refuses the
   10th command within 3 seconds and asks to wait, and two more commands
   during the wait get the address banned for 600 seconds. tsgw logs the
   address when it starts ("the TeamSpeak server sees the gateway's queries
   coming from this address"):
   - tsgw on the server's machine: `127.0.0.1`, which the file lists by
     default.
   - tsgw in a Docker container with bridge networking: the container's
     or the bridge's address (e.g. `172.17.0.2`), not `127.0.0.1`. Add
     that address, or run the container with `--network host`.

   The server reads the file again within seconds, without a restart; that
   also lifts a 600 s ban. Do not set `allowlisted = true` without the
   entry in the file: tsgw then sends at full speed until the server
   refuses a command, and an older tsgw got the address banned that way.
   Now it stops at the first refusal and logs "the server limits this
   address although it was taken to be on its query allowlist". Without
   the allowlist tsgw stays under the limit itself (9 commands per 3 s
   over all its connections) and, after a refusal (other query tools on
   the same address count too), sends nothing on any connection for at
   least 3 s, so logins are slower when many happen at once.
4. Each relayed channel is one more query connection. Keep
   `relay.max_channel_relays` below the server's per-IP connection limit.
   Relays count toward a channel's max clients unless their group has
   `b_channel_join_ignore_maxclients`.
5. Install tsgw ([below](#install)), copy the example config to
   `/etc/tsgw/tsgw.toml` and set the query address and credentials
   (`TSGW_QUERY_PASSWORD` or `password_file`).
6. Run it behind TLS (Caddy, nginx) and publish it in DNS, or let it
   answer on its default port next to the server
   ([below](#letting-voelin-find-the-gateway)): users only add the
   server's address and never see the gateway.

## Letting Voelin find the gateway

When a user adds a server by its address (`ts.example.org`), Voelin looks
for its gateway the way TeamSpeak looks for the voice server (SRV
`_ts3._udp`, then TSDNS), and observes the server through it as soon as the
user selects the server (users do not type gateway URLs or query logins).
It asks at the server's host name and then at each parent domain down to the
registered domain, the most specific name first; for `ts.example.org` that
is `ts.example.org`, then `example.org`. It looks once per run when the
server is selected or connects with voice, and again when its address
changes: what is published takes the place of what was found before (a
lookup that finds nothing keeps it). Every gateway found is kept and
tried in turn, best first, each for up to 20 seconds; the first that logs
in is used, and the log file names those it passed over (URL and error).
Only when all of them fail does it wait (1, 2, 5, 10, 20, then every 30
seconds, give or take 30 %; at once when voice connects) and start over;
users see nothing of it. Only a refused login ends it (bad signature,
security level too low, banned, not allowed); a gateway that cannot do the
login yet (for example an identity the server has not seen) is tried again.
Once connected, the client pings the gateway every 30 seconds and connects
again after 60 seconds without an answer.

**DNS records** (preferred). An SRV record names the gateway's host and
port, the service name says whether it speaks TLS:

| Record | Gives |
|---|---|
| `_tsgws._tcp.<name>  SRV <prio> <weight> <port> <host>` | `wss://<host>:<port>/v1` (TLS, e.g. behind Caddy or nginx) |
| `_tsgw._tcp.<name>  SRV <prio> <weight> <port> <host>` | `ws://<host>:<port>/v1` (plain, e.g. tsgw itself on 7788) |
| `_tsgws._tcp.<name>  TXT "path=/<path>"` (optional, same for `_tsgw`) | another path than `/v1`, e.g. behind a reverse proxy |

Which gateways: those of the most specific name that publishes a record,
and only those (a parent domain's records are another server's gateway
when the host publishes its own). At that name `_tsgws` before `_tsgw`,
among several records of one kind the lowest priority first, then tsgw's
own answer at that name (below), but only when a plain record is published
there, so a server that publishes only TLS is never reached without it. A
TLS proxy that fails (no rule for the host, no certificate) thus falls
back to the plain gateway published next to it. With no record at any
name, tsgw's own answer at the most specific name that gives one. A target
of `.` means "no gateway here" and the search goes on at the parent
domain. Examples (zone of `example.org`):

```dns
; Behind a TLS proxy at gw.example.org that forwards /tsgw/v1 to tsgw:
_tsgws._tcp.example.org.  3600 IN SRV 0 0 443 gw.example.org.
_tsgws._tcp.example.org.  3600 IN TXT "path=/tsgw/v1"
;   -> wss://gw.example.org:443/tsgw/v1 for ts.example.org, voice.example.org, …

; tsgw itself, without TLS, next to the server ts.example.org:
_tsgw._tcp.ts.example.org.  3600 IN SRV 0 0 7788 ts.example.org.
;   -> ws://ts.example.org:7788/v1, only for ts.example.org
```

The record's service name must match what listens on the port: tsgw
itself speaks plain WebSocket, so its own port (7788) is published as
`_tsgw`; a `_tsgws` record there makes Voelin start TLS with it, which
fails. Publish `_tsgws` only for a port with TLS in front.

Behind [Zoraxy](https://zoraxy.aroz.org/): add an HTTP proxy rule for the
gateway's host (`gw.example.org`) with one upstream, tsgw's plain address
(`<tsgw host>:7788`), "Proxy Target require TLS Connection" off,
WebSockets on, and a certificate for the host (ACME). If the rule also
has an uptime monitor, point it at `/health` (tsgw's `/` answers 404).
Check from outside: `curl https://gw.example.org/health` prints `ok` (or
what is missing while tsgw is not logged in to the TeamSpeak server, see
[Logs](#logs)); Zoraxy's own `404 page not found` there means no rule
matched the host or the request went to another upstream. Then set `listen.public_url =
"wss://gw.example.org/v1"` in tsgw.toml, so its own answer points at the
proxy.

One domain with several servers and several gateways: publish one record
per server host (`_tsgws._tcp.ts1.example.org`, `_tsgws._tcp.ts2.example.org`);
a record at the domain itself applies to every host below it that has none
of its own.

**Without DNS records** (like TSDNS on port 41144): Voelin asks
`http://<name>:7788/.well-known/tsgw` at the same names (and at an IP
address the server was added by). tsgw answers there with its URL,
`{"url": "…"}`: `listen.public_url` if set, else the address the request
came in on (`ws://<host>:7788/v1`). So a tsgw that listens on the default
port on the server's host is found with no setup at all; behind a proxy,
set `listen.public_url` so the answer points at the proxy.

Discovery is as trustworthy as DNS and the network, like TSDNS: prefer a
`_tsgws` record, so TLS proves the gateway is yours. Voelin checks a
`wss://` gateway's certificate against the system's certificate store, so
use one from a public CA (Caddy's automatic Let's Encrypt certificates
work); the Android app cannot reach `wss://` gateways yet (it has no
system store to read). A user logs in with
their own identity, when they select the server in Voelin.

## Install

tsgw is packaged for Linux servers, separately from the app
([building.md](building.md)): the release workflow attaches the packages and
the image (`tsgw-image.tar.gz`) to each release and uploads them as artifacts
when run by hand, and
`scripts/docker-build.sh linux` builds them locally.

**Debian/Ubuntu** (`tsgw_<version>_amd64.deb`, Ubuntu 24.04+ / Debian 13+).
It installs `/usr/bin/tsgw` and a systemd unit that is not started until
there is a config:

```sh
sudo apt install ./tsgw_<version>_amd64.deb
sudo install -d -m 755 /etc/tsgw
sudo cp /usr/share/doc/tsgw/examples/tsgw.example.toml /etc/tsgw/tsgw.toml
sudoedit /etc/tsgw/tsgw.toml
# The query password, readable by root only (systemd reads it before
# starting tsgw as an unprivileged dynamic user).
echo 'TSGW_QUERY_PASSWORD=…' | sudo install -m 600 /dev/stdin /etc/tsgw/tsgw.env
sudo systemctl enable --now tsgw
journalctl -u tsgw -f
```

The database (`history.path = "tsgw.db"`) ends up in `/var/lib/tsgw`. The unit
(`packaging/linux/tsgw.service`) runs tsgw sandboxed: no write access outside
that directory, network only.

**Other distributions** (`tsgw-<version>-linux-x86_64.tar.gz`): `bin/tsgw`,
the same unit in `lib/systemd/system/` and the example config in
`share/doc/tsgw/`. Copy the binary to `/usr/bin` (or change `ExecStart`) and
the unit to `/etc/systemd/system/`.

**Container** (`tsgw-image.tar.gz`, or build it):

```sh
docker load -i tsgw-image.tar.gz        # or: docker build -f crates/voelin-gateway/Dockerfile -t tsgw .
docker run -d --name tsgw --init --restart unless-stopped \
  -v $PWD/tsgw.toml:/etc/tsgw/tsgw.toml:ro -v tsgw-data:/var/lib/tsgw \
  -e TSGW_QUERY_PASSWORD=… -p 7788:7788 tsgw
docker logs -f tsgw
```

The volume keeps the database (`/var/lib/tsgw/tsgw.db`: login tokens,
history, settings changed at runtime) when the container is replaced.
`--init` passes `docker stop` on to tsgw and cleans up after it;
`--restart unless-stopped` brings it back after a crash or a reboot. With
bridge networking (the default) see step 3 of [Setup](#setup) about the
address the TeamSpeak server sees.

**From source:** `cargo run --release -p voelin-gateway -- --config tsgw.toml`.

## Logs

tsgw logs to standard output: `journalctl -u tsgw -f` under systemd,
`docker logs -f tsgw` in a container (colours only on a terminal; `NO_COLOR`
turns them off there too). At the default level it logs:

- at start: the version and settings in use (whether a query password is
  set, never the password itself), the query login and how long it took,
  the address the TeamSpeak server sees for the gateway (with a hint when
  `query.allowlisted` is off), "listening" with the URL apps are given;
- each app connection: its address, `X-Forwarded-For` and `User-Agent`,
  then logins (unique id, nickname, signature or token, how long it took),
  refused logins with the reason, requests slower than 1 s and the close
  (reason, how long it was open). Every line of a connection carries its
  number and address (`ws{id=12 peer=…}`);
- ServerQuery: failed commands (by name), each wait the server's flood
  protection asks for, relays opened and closed, the watcher connecting
  and losing its connection (with the attempt and the next wait), the
  lookup connection lost and restored.

More detail with `log.level` (`TSGW_LOG_LEVEL`; without it `RUST_LOG`):

| Setting | Shows |
|---|---|
| `TSGW_LOG_LEVEL=debug` | also every request and its time, ServerQuery connections opening and closing |
| `TSGW_LOG_LEVEL=info,voelin_query=trace` | every query command sent and answered, with its time and the connection it went over (`lookup`, `observer`, `relay <channel>`) |
| `TSGW_LOG_LEVEL=warn` | only problems |

Libraries that log every packet (`russh`, `hyper`, `h2`, `tower`,
`tungstenite`) stay at warnings unless the setting names them
(`debug,russh=debug`). Under systemd put the variable into
`/etc/tsgw/tsgw.env`, in a container pass `-e TSGW_LOG_LEVEL=…`.

`log.file` (`TSGW_LOG_FILE`) writes the log to a file as well. tsgw appends
to it, also across restarts; at 20 MB it moves to `tsgw.1.log` (older ones
one further along, 5 files in all) and a new one starts. A relative path is
relative to the working directory, `/var/lib/tsgw` under systemd and in the
container (`TSGW_LOG_FILE=logs/tsgw.log` with the volume above). The systemd
unit can also write `/var/log/tsgw`. If the file cannot be opened, tsgw
says so and logs to standard output only.

While the TeamSpeak server cannot be reached (not started yet, restarting),
tsgw listens anyway and logs in as soon as it can, waiting 1, 2, 3, 5, 10,
then 30 seconds between attempts (longer if the server's flood protection
asks for it). Apps that connect meanwhile wait up to 10 seconds, then are
told to try again. A query connection the server drops is opened again the
same way, and relays that someone still reads or that are pinned come back.
Opening a query connection (connecting, logging in, selecting the server)
gives up after 10 seconds, so a server that does not answer holds up
nothing for long. `/health` answers
`ok` while the gateway is logged in and watching the server, otherwise 503
and what is missing (`starting: …`, `query: down, observer: up`).

## Visibility

Query clients are not hidden by the server; official clients hide them from
users whose `i_client_serverquery_view_power` is below the query client's
needed view power (100 by default). Server Admins, and third-party clients
that ignore the rule, see the gateway's sessions. Relayed posts show the
user's nickname, like a message the user wrote in TeamSpeak; whoever can see
query clients sees that it came from a query session.

## Configuration

`tsgw.toml` holds two kinds of keys (the example marks them):

- **Bootstrap** keys are needed before the database opens: `gateway_id`,
  `server.voice_port`, `query.*` (transport, address, login, password,
  `allowlisted`), `listen.bind`, `listen.public_url`, `history.path` and
  `log.level`, `log.file` ([Logs](#logs)).
  They come from the file, the environment or the command line and apply
  at start; changing them needs a restart.
- Everything else (history, relays, features, permission rules, quotas,
  limits, reminders) is a **seed**: the value in use unless an admin changes
  it at runtime. Runtime changes are stored in the gateway's database and
  apply at once, without a restart.

Where a value comes from, highest first:

| Source | How |
|---|---|
| `db` | set at runtime (`config_set`, `perm_set`), stored in the database |
| `cli` | `tsgw --set history.retention_days=90` (repeatable) |
| `env` | `TSGW_<KEY>`: upper case, dots become `_`, e.g. `TSGW_HISTORY_RETENTION_DAYS=90`, `TSGW_QUERY_PASSWORD=…`; lists as `1,2,3` or JSON, rules as JSON |
| `file` | `tsgw.toml` |
| `default` | built in |

**Reload.** `SIGHUP` (`systemctl reload tsgw`), a changed file (checked every
5 s) or the `config_reload` admin command reads the file again. Values set at
runtime keep winning; the log names every key where the file now says
something else ("file differs from the value set at runtime"). Bootstrap keys
changed in the file are logged as needing a restart. A file that does not
parse changes nothing. `config_reset` drops a runtime value, so the key falls
back to the command line, environment, file or default.

Changes apply live: relays pick up a new `relay.nickname` (the relay
sessions are replaced; channel chat keeps flowing), newly pinned channels
open, retention prunes at once, reminder times and quotas apply to the next
request, and connected apps get the new list of features.

### Keys

Numbers are non-negative; quotas and limits of 0 mean none. There are no
built-in caps on history pages, pins, topics, events or reactions: only what
you set here.

| Key | Default | |
|---|---|---|
| `auth.require_server_groups` | `[]` | If not empty, users need one of these server groups |
| `auth.deny_uids` | `[]` | Unique ids that may never use the gateway |
| `auth.min_security_level` | `0` | On top of the server's own requirement |
| `auth.token_ttl_hours` | `720` | Lifetime of login tokens |
| `relay.nickname` | `"Chat Relay"` | Relay sessions are named `<nickname> <cid>`, the lookup session `<nickname> Gateway` |
| `relay.format` | `"[{nick}] {text}"` | How posts appear when the relay cannot take the user's nickname |
| `relay.idle_teardown_secs` | `120` | Close a relay this long after its last reader left |
| `relay.max_channel_relays` | `6` | Concurrent relays (0: no limit) |
| `relay.pinned_channels` | `[]` | Always relayed, so their history is complete |
| `history.enabled` | `true` | Store messages; pins, reactions and topics need it |
| `history.retention_days` | `0` | Delete older messages (0: keep forever; pinned messages are kept) |
| `features.pins`, `.reactions`, `.topics`, `.events`, `.streams`, `.activity` | `true` | Turn features off; apps hide them |
| `topics.relay_format` | `"[#{topic}] {text}"` | How topic posts appear in TeamSpeak; TeamSpeak posts in this form join the topic |
| `events.reminder_minutes` | `[15, 0]` | Remind event subscribers this many minutes before the start (0: when it starts) |
| `events.stream_link_minutes` | `60` | A stream by the host links to a stream event from this long before its start until this long after its end |
| `activity.retention_days` | `0` | Delete older activity (0: keep forever) |
| `limits.posts_per_window` / `limits.post_window_secs` | `5` / `5` | Chat posts per connection |
| `limits.actions_per_window` / `limits.action_window_secs` | `30` / `10` | Pins, reactions, topics, events, streams per connection |
| `quota.history_page` | `0` | Most messages per history page |
| `quota.pins_per_channel` | `0` | Pins per chat |
| `quota.topics_per_channel` | `0` | Open topics per chat |
| `quota.events_per_user` | `0` | Upcoming events per creator |
| `quota.reactions_per_message` | `0` | Different emoji per message |
| `quota.reaction_bytes` | `64` | Longest reaction |
| `quota.message_bytes` | `0` | Longest post (TeamSpeak splits long ones anyway) |
| `quota.streams_per_user` | `0` | Directory entries per user |
| `perm.<action>` | unset | Permission rules, below |

Before this version history was pruned after 30 days by default; now the
default keeps it. Set `history.retention_days = 30` to keep the old behaviour.

### Admin commands

Users with the `admin` permission see the `admin` capability and can send
(see [gateway-protocol.md](gateway-protocol.md#administration)):

| Command | Does |
|---|---|
| `config_list` | Every key with value, source (`db`, `cli`, `env`, `file`, `default`), default, type, description and whether it is bootstrap; the query password is shown as `***` |
| `config_get {key}` | One key |
| `config_set {key, value}` | Store a value at runtime (JSON of the key's type); applies at once |
| `config_reset {key}` | Drop the runtime value |
| `config_reload` | Read the file again, like `SIGHUP` |
| `perm_list` | The rule of every action, its source and what the default allows |
| `perm_set {action, rule}` | Set a rule (same as `config_set perm.<action>`) |
| `perm_reset {action}` | Back to the file's rule or the default |

## Permission rules

Each action has a rule: who may do it, by TeamSpeak server group ids and/or
channel group ids (in the channel concerned), or everyone:

```toml
[perm]
pin = { server_groups = [6], channel_groups = [5] }   # Server Admin, Channel Admin
create_event = { server_groups = [6, 7] }
react = { everyone = true }
```

A rule replaces the action's default; `{}` allows nobody. Users with `admin`
may do everything, and whoever may `moderate` a channel may also pin there.
Access to the chat itself (read, or post for topics) is always required.
Groups come from presence for users who are online and from
`servergroupsbyclientid` / `channelgroupclientlist` (cached 60 s) otherwise.

| Action | Default without a rule |
|---|---|
| `react` | everyone who can read the chat |
| `pin` | moderators and admins |
| `create_topic` | everyone who can post in the chat |
| `create_event` | everyone who can post in the event's channel (server chat for server-wide events) |
| `rsvp` | everyone |
| `stream` | everyone (their own streams) |
| `moderate` | `b_channel_modify_name` in the channel (Channel Admin): edit or delete others' topics, events, streams and pins |
| `admin` | `b_virtualserver_modify_name` (Server Admin): settings and rules |

Creators may always edit and delete their own topics, events and stream
entries.

## Data

The database (`history.path`, SQLite in WAL mode) holds login tokens, chat
messages with pins, reactions and topics, events and RSVPs, the stream
directory (so it survives a restart; entries whose client is gone are dropped
once the gateway sees the server), the activity feed, settings changed at
runtime and the audit log. The schema is versioned (`PRAGMA user_version`)
and migrated in place at start, keeping existing messages and their ids.
Message ids are never reused, even after pruning, so apps can merge history
with what they store locally.
