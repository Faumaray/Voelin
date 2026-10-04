# myTeamSpeak

## Desktop account sessions

myTeamSpeak is TeamSpeak's account service: it holds the user's identities,
bookmarks and settings server-side so the official clients can synchronise
them. Voelin opens a login page at desktop startup and makes the signed-in
myTeamSpeak account the main app profile. The same form lives in Settings →
My Account. Native login, OTP, saved-session validation and sign-out use
`voelin-myts`; registration, password recovery and account management open the
official website in the default browser.

This reverses the previous decision to leave account sign-in out of scope, at
the repository owner's request. The API remains non-public. This integration
does not establish permission to use or redistribute TeamSpeak's private API
or schemas.

Registration, activation-email resend, password recovery, username/email/password
changes, two-factor authentication, deletion and data export are available through
named browser actions. The website may be signed in to a different account;
Voelin does not transfer credentials to it. After completing a change, sign out
and back in to Voelin to refresh the profile.

Native cloud synchronization and recovery-key restoration are not implemented.
The app does not synchronize identities, bookmarks or settings, or create
account encryption and backup keys. Password login can decrypt an existing
version-2 account key to authenticate the account on voice servers (below).
To import cloud identities, sign in with the official client
so they are written to its local `settings.db`, then use the existing identity
import. See [../identity.md](../identity.md).

### Transport and verification boundary

The implementation consumes the message types, session status checks and wire
codec from the local `schema-api` snapshot in `third_party/schema-api`. Its
generated tonic gRPC clients are not used: the official clients use custom
protobuf-over-HTTP with `Content-Type: application/ts3cloud` at
`https://clientapi.myteamspeak.com`.

Requests contain a one-byte method-name length, the method name, and then the
protobuf payload. For login this is `05 6c 6f 67 69 6e` followed by `LoginData`.
Responses contain the protobuf alone. The live service labels these binary
responses `application/json`; decoding still uses protobuf and requires the
operation's explicit success status. Login uses `/authentication`; session
validation and deletion use `/session`.

`Client::login` accepts the user's password and derives the wire credential:
standard Base64 of PBKDF2-HMAC-SHA512 with 10,000 iterations, 48 output bytes,
and salt `ASCII_lowercase(email) + "ts3Login" + password`. The derivation runs
on Tokio's blocking pool. It reuses `base64` and `pbkdf2` already present in
the workspace dependency graph; no new cryptographic implementation is added.

These details replace the initial guessed header and raw-password handling.
On 2026-10-04, an owner-authorized live login, session validation, and logout
passed. Ordinary tests remain offline and use synthetic credentials. Live OTP
and expired-session renewal have not been verified.

Static evidence from the installed clients (ELF virtual addresses):

- TS3 request framing: `0x14a19dd`–`0x14a1a49`; password salt and iterations:
  `0x1152b60`–`0x1152c24`; 48-byte derivation: `0x1154920`; SHA-512 selection:
  `0x11e35af` and descriptor table `0x1b4a930`. Executable SHA-256:
  `87c9b6927949d28c64e9da21fdf62bdf7a9e5b204597e9f75497ff6ee45be8ac`.
- TS6 request framing: `0x1dfd4e1`–`0x1dfd54c`; response decoding:
  `0x1dfddcb`–`0x1dfde0b`. Executable SHA-256:
  `9a32215a578a14525ec43f5e6f60aae087c0229c1b60487da235e3b3782cd40a`.

The renewal request has separate `renewal_token` and `auth_token` fields.
The official TS3 client requires both cached values before renewing
(`0x1143d81`–`0x1143dba`). Password login supplies no auth token, so an expired
password-login session requires fresh sign-in. The client API rejects missing
renewal credentials before making a request. It does not silently create the
separate shareable authentication token used by the official client's
sharing/import flow.

A saved OTP renewal token is used only with the same account's device ID in a
password login; an explicit one-time code takes precedence. Both are replaced
with the next successful response and cleared on sign-out. The installed TS6
serializer at `0x1cbed20`–`0x1cbefc1` independently confirms the email/password/OTP/
device/OTP-renewal fields. Live OTP and token rotation remain unverified.

Passwords and one-time codes are not persisted. Session and renewal material
are stored together as a single `myts/session` item through the app's secret
store, never in plaintext settings. Replacing one item prevents a partial write
from mixing credentials from different accounts. The bundle also stores the
account UUID, username and email, with backward-compatible defaults for older
bundles. With a saved session the login page does not open at all: startup
validates the session in the background (Settings → My Account shows the saved
profile as "Checking session…" meanwhile) and only an expired session brings
the page back. Errors leave retry and manual sign-in available. Transport
failures retain saved credentials. A locked keyring can be retried without
restarting. Continue without an account dismisses the page and is remembered
(`ui.skip_account_prompt`), so it does not ask at the next start; signing in
clears that.

### Profile and avatar

After a sign-in or a validated saved session the app asks the user service,
`https://clientapi.myteamspeak.com/user` (the route string is in the TS6
client next to `/authentication` and `/session`), for `getAccountData` with the
basic info, avatars, description, badges and authenticated devices, and shows
them in Settings → My Account (and the avatar in the sidebar's profile when
the server has none for us). `requestAvatarSignedUrl` turns the avatar's file
name (the "online" one first) into a signed HTTPS link, which is fetched
(at most 4 MiB) and kept in `<data>/account/avatar` for the next start; it is
fetched again only when the file name changes. The login response's own
avatar file names and description are used until then. What is kept goes into
the same secret-store bundle as the session (`profile`), never the token into
a URL. Neither call has been run against the live service from this
environment.

Sign-out returns to login, clears local account material and attempts remote
session deletion; storage or remote errors are reported. A late completion
cannot sign a canceled operation back in. Server identities and bookmark
nicknames remain independent of the primary app profile.

### Authenticated account identity on voice servers

The account UUID is a profile identifier, not the authenticated `myTS ID`.
Password login unwraps the account's encrypted root key, then the account's
private signing scalar. The key pair is checked before it becomes usable.
The validated identity is stored with the session in the secret store; it is
not published to voice sessions until saved-session validation succeeds.
Older saved sessions need a fresh password sign-in to obtain this identity.
Missing, unsupported or invalid identity data leaves account login usable and
shows an explicit warning that voice servers will not receive a myTS ID.

The established version-2 format uses:

- PBKDF2-HMAC-SHA512, 10,000 iterations, 32 bytes, with salt
  `ASCII_lowercase(email) + "ts3Encryption" + password`.
- Root wrapping: `02 || tag16 || nonce12 || ciphertext32`, AES-256-GCM
  with empty associated data.
- Private-key wrapping: `iv16 || ciphertext32 || SHA512(plaintext32)`,
  AES-256-CTR with a little-endian 128-bit counter. The digest and matching
  public key must verify. Version-1 root wrapping is explicitly unsupported.

The existing transport's connection challenge is
`SHA256(30 08 00 00 00 00 || shared_iv64) || "MyTeamSpeakID"`.
Signing uses the raw account scalar, not Ed25519 seed expansion. The public
proof sends `myTeamspeakId`, `acTime`, `userPubKey`, `authSign`, `pubSign` and
`pubSignCert` on `clientinit`; the last two split `public_signature` at byte 64.
The HTTP response's numeric creation timestamp is byte-swapped once for the
voice protocol's decimal `acTime`. The provider public signature covers
`public_key32 || wire_acTime.to_le_bytes() || myTS_ID33`.
Neither the account UUID nor `mytsid_user_cert.cert` substitutes for these fields.
Private key material never belongs in a packet.

The main account is shared by all voice connections. Each new connection signs
its own challenge. Account changes cancel an unfinished handshake; connected
clients send `updatemytsid`, and logout sends an empty myTS ID. Rejected or
unanswered account updates disconnect rather than retaining a previous account
association.

Static TS3 evidence in the binary identified above: proof helper `0x1018580`,
`clientinit` invocation `0x1020c3b`, live update `0x1049659`, scalar signing
`0x1196800`, password KDF `0x1152cf0`, version-2 unwrap `0x11542d0`, GCM
layout `0x1155610`, and private-key integrity check `0x1153280`.
The login-to-storage timestamp swap is at `0x110b57c`; the server's public
signature payload is built at `0x1222da`–`0x12260d` in the pinned TS3 server.
Independent synthetic crypto vectors and malformed/tampered key tests cover
the local implementation. The ignored `voelin-core/tests/myts_live.rs` test
requires explicit account opt-in and caller-owned loopback servers; it checks
server-side identity association, clearing, reattachment, and fresh handshakes.

On 2026-10-04, owner-authorized live password login, authenticated key unwrap,
session validation and remote session deletion passed. Isolated official TS3
3.13.8 and TS6 6.0.0-beta13.1 servers each accepted two fresh signed handshakes,
reported the expected `client_myteamspeak_id`, and passed live clear/reattach
checks. Both reported the native Linux compatibility tuple and Voelin metadata.
The test servers needed a container-local DNS override to a working address
for `ts3services.teamspeak.com` before their official revocation-list downloads
succeeded; TLS and signature/revocation checks remained enabled. No server
override is part of the application.

Official-client visual inspection remains unverified: isolated TS3 3.6.2
crashed before showing its UI; isolated TS6 started but required account
confirmation before reaching the server view. No existing client profile was
changed. GUI checks of Voelin's account fields passed at desktop and narrow
sizes using synthetic account data.

### Client reporting

The voice engine chooses an unchanged signed compatibility tuple for the
actual operating system: the newest TeamSpeak 6 client build signed for it
(`6.0.0-beta4.1 [Build: 1779880475]` on Linux, captured from the official
Linux client's `clientinit`; `6.0.0-beta2` on Windows, from upstream
tsdeclarations), else the generic `3.?.? [Build: 5680278000]`. Every row of
`Versions.csv` verifies against TeamSpeak's version-signing key. A bookmark's explicit compatibility-version override
is honored and validated. `client_meta_data` identifies `Voelin`, its package
version and native OS. The CLI uses the same catalogue and native default.
TeamSpeak signs compatibility versions: replacing the signed version with an
arbitrary `Voelin` string is not supported. Metadata does not establish that
stock clients display Voelin in their standard version label.

An account change during an automatic transport reconnect cancels that voice
session to prevent stale credentials from being sent. Reconnect manually in
that case; ordinary connected account changes use the live update command.

The vendored crate has no declared license. Its provenance is recorded in
`third_party/schema-api/VOELIN-PATCH.md`; no license grant is inferred from its
presence. Runtime dependency notices are generated normally.

### Official account-management routes

Read-only inspection on 2026-10-04 verified these routes in the deployed
[main bundle](https://www.myteamspeak.com/main.a0429db5055b28c1.js) and
[account-management bundle](https://www.myteamspeak.com/933.0799f055321e36e9.js):

| Action | Official route |
| --- | --- |
| Registration | `https://www.myteamspeak.com/register` |
| Password recovery | `https://www.myteamspeak.com/forgot-password` |
| Activation email | `https://www.myteamspeak.com/resend-activation` |
| Account overview | `https://www.myteamspeak.com/my-account` |
| Username / email / password | `/my-account/change-username`, `/my-account/change-email`, `/my-account/change-password` |
| Two-factor authentication | `/my-account/change-2fa` |
| Deletion / data export | `/my-account/manage-account`, `/my-account/manage-data` |

The site's authentication guard preserves the requested destination. Its
[registration](https://www.myteamspeak.com/5.dcb40731983490af.js) and
[password recovery](https://www.myteamspeak.com/252.5c94d1e115160e7d.js) flows
use reCAPTCHA. Password changes rewrap encryption-key material, and deletion
requires a separate confirmation. Browser handoff retains those provider-owned
steps. Only fixed HTTPS URLs are passed as desktop-opener arguments; no shell,
email, password or token is included. Opening a page does not submit a change.
These are website-source findings, not a published native API guarantee.

### Local verification

```sh
cargo test -p voelin-myts
cargo test -p voelin-ui --lib myts::tests
cargo build -p voelin-myts -p voelin-ui
cargo clippy $(scripts/own-crates.sh) --all-targets --no-deps -- -D warnings
cargo fmt $(scripts/own-crates.sh) --check
scripts/notices.sh --check
```

The HTTP tests use loopback mock servers and synthetic credentials. UI tests
cover the secret-store bundle, failed reads/writes/deletes, form states,
concurrent operations, primary-account replacement, legacy-bundle migration,
remembered-device ownership and late completions after sign-out. Screenshot
fixtures use `VOELIN_DEMO_UI=1` and `VOELIN_OPEN=login`, `login:otp`,
`login:saved` or `login:account`; they never read the keyring. The formatting
command excludes vendored sources; `cargo fmt --all` also traverses local path
dependencies and reports upstream formatting differences. Clippy's `--no-deps`
also keeps diagnostics out of the existing vendored protocol crates.

The ignored `owner_live_login_validate_logout` test requires the owner to set
`VOELIN_MYTS_LIVE=1`, `VOELIN_MYTS_EMAIL`, `VOELIN_MYTS_PASSWORD`,
`VOELIN_MYTS_DEVICE_ID`, and optionally `VOELIN_MYTS_OTP`, then explicitly run
`cargo test -p voelin-myts owner_live_login_validate_logout -- --ignored`.
This test is not part of the offline verification commands above.

## The local record, structurally

The TeamSpeak 6 client keeps its account state in its `settings.db` in the
table `AccountData`, shape `(timestamp integer unsigned, key varchar unique,
value varchar)`, with two rows: `Account` and `Checksum`. `Checksum` is 20
bytes — `SHA1` of the other rows' values, the same rule as `ProtobufItems`
(see [../identity.md](../identity.md)).

`Account` is a protobuf message, about 1.7 kB in the install examined. Its
shape, field numbers and lengths only:

| Field | Wire type | Length | Looks like |
| --- | --- | --- | --- |
| 1 | bytes | 18 | printable ASCII |
| 2 | bytes | 64 | printable ASCII, base64 alphabet |
| 3 | bytes | 36 | a UUID |
| 4 | bytes | 8 | printable ASCII, base64 alphabet |
| 5 | bytes | 32 | binary |
| 10 | bytes | 36 | a UUID |
| 50 | message | 38 | one nested field: a UUID |
| 51 | message | 2–5 | repeated, nine times; one or two small fields each |
| 52 | bytes | 61 | binary |
| 54 | message | 375 | 32-byte and 33-byte binary fields, a varint, a nested 112-byte blob, a 176-byte blob |
| 56 | message | 114 | one nested field: a 112-byte blob |
| 57 | message | 178 | a nested 98-character ASCII string, a varint, a 64-byte blob |
| 58 | bytes | 386 | printable ASCII |
| 59 | message | 6 | one small field |
| 61 | message | 241 | a 64-byte and a 112-byte blob |

The 386-character printable field and the 64-byte base64-alphabet field are the
shape of a bearer token and an identifier; **they were not decoded, parsed or
used, and no request was made to any TeamSpeak service.**

No field in `Account` parses as an ASN.1 `SEQUENCE`, so it carries **no
identity key pair** in the tomcrypt DER form the identities use. Importing
identities therefore never needs to touch this row, and Voelin's importer does
not read it. The 112-byte blobs are the right size for a raw key or signature
but are not DER; what they are was not investigated further, because nothing
Voelin does needs them.

`avatar_cache_mytsid` (one row, `contacts`) and `RevocationList` (one row,
`Cache`) are caches the account service fills. Neither is read either.
