# Security

## Reporting a vulnerability

Please report security problems privately through GitHub: **Security >
Report a vulnerability** on
https://github.com/Faumaray/teamspeak_client_rs (private vulnerability
reporting). Do not open a public issue for them. If private reporting is not
available, open an issue asking for a contact, without details.

Include what is affected (desktop app, Android app, `tsgw` gateway, `voelinctl`),
the version or commit, and how to reproduce. You should get an answer within
a week. Fixes are released as soon as they are ready and credited in the
changelog unless you prefer otherwise.

Supported versions: until 1.0, only the latest release gets fixes.

In scope: this repository's code and its packages. Problems in TeamSpeak
servers or the official clients belong to TeamSpeak Systems GmbH; problems in
dependencies to their projects (tell us too if this client is affected).

## What the client stores

Everything stays on the device; there is no telemetry and no account with
us.

| What | Where | Protection |
|---|---|---|
| Identities (TeamSpeak private keys) | SQLite database `client.db` in the data directory (`~/.local/share/voelin`, Flatpak `~/.var/app/<app id>/data/voelin`, `%APPDATA%\voelin`, Android app storage) | File permissions of the user account only; not encrypted. An identity is the account on every server that knows it: back it up, do not share the file |
| Server and ServerQuery passwords | OS keyring (Secret Service, Windows Credential Manager, Android Keystore), service `voelin` | Keyring; kept in memory for the session only when there is no keyring |
| Bookmarks, settings, chat history | `client.db` | File permissions; history stays until deleted |
| Crash reports (opt-in) | `crash-reports` in the state directory (`~/.local/state/voelin`, `%LOCALAPPDATA%\voelin`) | Local only, never uploaded. They contain the panic message and a backtrace, which may include server addresses or names; review before attaching one to an issue |
| OpenH264 (opt-in) | Downloaded from Cisco into the app's directories | Loaded only if its SHA-256 matches a known Cisco build |

## What others can see

- **Voice connections** use the TeamSpeak protocol's encryption. The server
  and its admins see what any client sees: your nickname, identity, IP
  address, channel and messages.
- **Streams (TeamSpeak 6)** are peer-to-peer WebRTC (DTLS-SRTP). Viewers and
  streamers exchange ICE candidates, so **they learn each other's IP
  addresses**, including local network addresses. Only watch and share
  streams with people you are fine sharing your address with.
- **Relayed channel chat** (gateway or own ServerQuery login) is posted by a
  query client as `[Nick] text`; people in that channel see it came through
  a relay.

## What the gateway (`tsgw`) stores

The gateway is run by a server admin (see
[docs/gateway-admin.md](docs/gateway-admin.md)). In its SQLite database
(`history.path`, default `tsgw.db`):

| Table | Contents | Retention |
|---|---|---|
| `tokens` | SHA-256 hashes of login tokens (never the tokens), the user's unique id and database id, expiry | Removed after expiry (`auth.token_ttl_hours`, default 30 days) |
| `messages` | Relayed server and channel chat: target, time, author unique id and name, text | `history.retention_days` (default 30); disable with `history.enabled = false` |
| `audit` | Time, unique id and action of every login and post | Not pruned automatically; the admin should rotate it |

It holds no voice and no passwords of users: users log in by signing a
challenge with their identity key. The gateway's own ServerQuery credentials
(usually with admin rights) are in its config file, a password file or
`TSGW_QUERY_PASSWORD`: keep them readable only by the gateway's user. Serve
the gateway over TLS (`wss://`) only; tokens travel in its messages.
