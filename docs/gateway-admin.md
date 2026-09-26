# Running the tsgw gateway

`tsgw` lets users of Voelin see who is in which channel and read and
write channel chat **without joining voice**, on a server you administer. It
holds ServerQuery sessions (invisible to normal users of the official clients)
and serves users over a WebSocket (`tsgw.v1+json`, see `crates/voelin-gateway-proto`).

## What users get

| Feature | How |
|---|---|
| Presence | One query session watches the server; each user sees the channels their `i_channel_subscribe_power` allows, without query clients |
| Channel chat | A relay query session is moved into each channel someone opened; it forwards what is said and posts users' messages as `[Nick] text` |
| Server chat | Read by the watcher, posted by the gateway's lookup session |
| History | Relayed messages are stored (SQLite) and can be paged |

The nickname in relayed posts is the one the server has on record for the
user's identity (from `clientdbinfo`), not something the user can choose.

## Authentication and authorization

Users log in by signing a challenge with their TeamSpeak identity key. The
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
- see a channel in presence: `i_channel_subscribe_power` (server level) ≥ the
  channel's `channel_needed_subscribe_power`

Posts are limited to 5 per 5 seconds per connection, and every login and post
is written to the audit table.

## Setup

1. Enable ServerQuery. TeamSpeak 6 has it off by default:
   `TSSERVER_QUERY_SSH_ENABLED=1` (the gateway needs events, so SSH, not HTTP).
   TeamSpeak 3: raw (10011) or SSH (10022).
2. Use a query login with enough rights for `clientdbfind`, `clientdbinfo`,
   `permoverview`, `banlist`, `clientmove` and `sendtextmessage`, e.g. a
   dedicated login in the Server Admin group.
3. Put the gateway's IP into `query_ip_allowlist.txt` and set
   `query.allowlisted = true`. Otherwise the server's flood protection
   (10 commands / 3 s) applies and bans the gateway under load.
4. Each relayed channel is one more query connection. Keep
   `relay.max_channel_relays` below the server's per-IP connection limit.
   Relays count toward a channel's max clients unless their group has
   `b_channel_join_ignore_maxclients`.
5. Install tsgw ([below](#install)), copy the example config to
   `/etc/tsgw/tsgw.toml` and set the query address and credentials
   (`TSGW_QUERY_PASSWORD` or `password_file`).
6. Run it behind TLS (Caddy, nginx) and give users the `wss://…/v1` URL.

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

The audit database (`path = "tsgw.db"`) ends up in `/var/lib/tsgw`. The unit
(`packaging/linux/tsgw.service`) runs tsgw sandboxed: no write access outside
that directory, network only.

**Other distributions** (`tsgw-<version>-linux-x86_64.tar.gz`): `bin/tsgw`,
the same unit in `lib/systemd/system/` and the example config in
`share/doc/tsgw/`. Copy the binary to `/usr/bin` (or change `ExecStart`) and
the unit to `/etc/systemd/system/`.

**Container** (`tsgw-image.tar.gz`, or build it):

```sh
docker load -i tsgw-image.tar.gz        # or: docker build -f crates/voelin-gateway/Dockerfile -t tsgw .
docker run -v $PWD/tsgw.toml:/etc/tsgw/tsgw.toml -e TSGW_QUERY_PASSWORD=… -p 7788:7788 tsgw
```

**From source:** `cargo run --release -p voelin-gateway -- --config tsgw.toml`.

## Visibility

Query clients are not hidden by the server; official clients hide them from
users whose `i_client_serverquery_view_power` is below the query client's
needed view power (100 by default). Server Admins, and third-party clients
that ignore the rule, see the gateway's sessions. Relayed channels show the
relay's posts under its own nickname (`Chat Relay <cid>`), so people in the
channel can tell the messages come through the gateway.
