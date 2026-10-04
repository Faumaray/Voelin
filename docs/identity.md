# Identities

A TeamSpeak identity is an ECC P-256 key pair. The public key gives the client
its unique id, which is how every server recognises it; the private key proves
ownership. Losing it means losing every server group, every permission and
every friend entry tied to that id — and anyone who copies it can impersonate
its owner on every server. **An identity file is a secret. Treat it like an SSH
private key.**

Voelin can import identities from the official clients so that switching does
not mean starting over, and export them back so they are never trapped here.

The myTeamSpeak account has a separate signing identity. After password sign-in,
Voelin can authenticate its `myTS ID` on voice servers without replacing the
P-256 identity that owns server permissions. The primary account applies to all
voice connections and is cleared on sign-out. See
[the account protocol notes](research/myteamspeak.md#authenticated-account-identity-on-voice-servers).

## The unique ids

Both generations derive the unique id from `omega`, the base64 of the DER
("tomcrypt") encoding of the *public* key:

| Server | Unique id |
| --- | --- |
| TeamSpeak 3 | `base64(SHA1(omega))`, 28 characters |
| TeamSpeak 6 | `base64(SHA256(omega))`, 44 characters |

The same key therefore has two ids. `voelinctl identity show` prints both
(`uid` and `uid6`); the store keys identities by the TeamSpeak 3 id, which is
the shorter and older of the two.

The *security level* is hash cash over `omega`: the number of trailing zero
bits of `SHA1(omega || decimal counter)`, counted from the first byte. The
counter is stored next to the key; raising it raises the level, which is why
the counter travels with every export. Servers commonly require level 8.

## The export string

Every format below ultimately holds the same string:

```
<counter>V<obfuscated base64>
```

- `<counter>` is the hash cash counter in decimal.
- `V` separates it (the obfuscated part is base64, which never contains `V`
  before the first `V` here because the counter is digits only).
- The obfuscated part is `base64(obfuscate(base64(DER key pair)))`, where the
  DER is tomcrypt's `SEQUENCE { BIT STRING, INTEGER 32, INTEGER x, INTEGER y,
  INTEGER private }` (109–117 bytes in practice, so 152 base64 characters, so
  204 characters after the second base64).
- `obfuscate` XORs the first 100 bytes with a fixed 128-byte table and the
  first 20 bytes with `SHA1` of the bytes from offset 20 up to the first zero
  byte. It is not encryption; it stops nothing. The algorithm lives in
  `tsproto_types::crypto::EccKeyPrivP256::{from,to}_ts_obfuscated`.

`tsproto`'s `Identity::new_from_ts_str` parses this string, so every importer
below only has to find it.

## Where the clients keep it

### TeamSpeak 3 and TeamSpeak 6: the `ProtobufItems` table

Both clients store identities in a table of the same shape, in their
`settings.db` (SQLite):

```sql
CREATE TABLE ProtobufItems (
    timestamp integer unsigned NOT NULL,
    key       varchar NOT NULL UNIQUE,
    value     varchar        -- protobuf bytes, despite the column type
)
```

`key` is `"1"`, `"2"`, … for the items plus one row `"Checksum"`. Each item
`value` is a protobuf message with a common header and one payload field:

| Field | Wire type | Meaning |
| --- | --- | --- |
| 2 | bytes | the item's own UUID (36 characters, with dashes) |
| 3 | bytes | a second UUID; present in TeamSpeak 6 rows, absent in TeamSpeak 3 rows |
| 4 | varint | small enum, 1 or 3 observed |
| 5 | bytes | optional UUID |
| 6 | varint | **item type**, 0–5 |
| 7, 8 | bytes | optional UUIDs, usually empty |
| 9 | varint | Unix timestamp in seconds |
| 16 + type | bytes | the payload, one field per item type |

The payload field number is `16 + type`, so the type in field 6 says which
field carries the body. **Type 1, payload field 17, is an identity:**

| Field | Wire type | Meaning |
| --- | --- | --- |
| 1 | bytes | the export string `<counter>V<obfuscated base64>` (207–213 characters) |
| 2 | bytes | the nickname, UTF-8 |
| 3 | bytes | an 8-character alphanumeric token, identical for every identity in one install — an account or device identifier, not part of the key |
| 4 | bytes | empty in every row seen |
| 5 | varint | 1 on exactly one identity: the selected one |

Voelin reads fields 1, 2 and 5 and ignores the rest. The other item types
(0, 2, 3, 4, 5 — connection profiles, bookmarks, server lists and such) are
**not** decoded; they are skipped by their type, not by guessing at their
contents.

The `Checksum` row is `SHA1` over the concatenation of the other rows' `value`
blobs in ascending numeric `key` order — 20 bytes, no key, no length prefixes.
It is an integrity check the client writes. Voelin does not verify it: it never
writes these files, so it cannot invalidate one, and refusing to import from a
database whose checksum a third-party tool left stale would help nobody. A key
that does not parse is rejected on its own merits.

`AccountData/Account` in the TeamSpeak 6 database is the myTeamSpeak account
record, not an identity — see [research/myteamspeak.md](research/myteamspeak.md).
It contains no ASN.1 key pair.

### TeamSpeak 3: `.ini` export

"Export identity" in the TeamSpeak 3 client writes:

```ini
[Identity]
id=friendly name
identity="<counter>V<obfuscated base64>"
nickname=Nickname
```

The quotes around `identity` are optional in files seen in the wild, so both
forms are accepted. Keys are matched case-insensitively and the section header
is not required.

### TeamSpeak 3: older `Profiles` rows

Client versions before the protobuf tables kept identities in the `Profiles`
table of `settings.db` as `Identities/<n>/identity` and
`Identities/<n>/nickname` rows. Voelin reads those too when they are present.
A current 3.6 install has only `Capture/…` and `Playback/…` rows there.

### `ts3clientui_qt.secrets.conf`

Some TeamSpeak 3 builds keep the identity list in this Qt config file next to
`settings.db`. It is read by the same line scanner as the `.ini` export: every
`identity=` line becomes an identity and everything else is ignored. No such
file existed on the installation this was developed against, so that path is
best-effort — if a build wraps the lines in a container of its own, Voelin
finds nothing in it rather than guessing.

## Default locations

Voelin looks in these places with `--auto`; each is opened read-only and none
is ever written.

| Client | Linux | Windows | macOS |
| --- | --- | --- | --- |
| TeamSpeak 3 | `~/.ts3client/settings.db` | `%APPDATA%\TS3Client\settings.db` | `~/Library/Application Support/TeamSpeak 3/settings.db` |
| TeamSpeak 6 | `~/.config/TeamSpeak/Default/settings.db` | `%APPDATA%\TeamSpeak\Default\settings.db` | `~/Library/Application Support/TeamSpeak/Default/settings.db` |
| TeamSpeak 6, Flatpak | `~/.var/app/com.teamspeak.TeamSpeak/config/TeamSpeak/Default/settings.db` | — | — |
| TeamSpeak 3, Flatpak | `~/.var/app/com.teamspeak.TeamSpeak3/.ts3client/settings.db` | — | — |

`$XDG_CONFIG_HOME` is honoured on Linux where the client honours it. Portable
installs keep `settings.db` next to the executable; point `--from` at it.

## What Voelin does

`voelin_core::identity` reads and writes these forms; `voelinctl` drives it:

```
voelinctl identity import --auto [--dry-run] [--store <db>]
voelinctl identity import --from <path> [--dry-run] [--store <db>]
voelinctl identity list [--store <db>]
voelinctl identity export <id> <path> [--store <db>]
```

`--store` defaults to the desktop app's client database,
`<data dir>/voelin/client.db`, so the CLI and the app share identities.

**On every start** the app does the same as `import --auto`
(`voelin_core::identity::import_new`): identities found in the default
locations that the client database does not have yet are added, the files
are only read. The user should appear as on the official client, so the
identity the official client uses by default (field 5 of its payload, below;
TeamSpeak 6's before TeamSpeak 3's) becomes Voelin's default when Voelin has
none, or only the one it created on its own the first time it started,
which nobody chose. That happens once: the created identity is kept, and
choosing it again in Settings → Profiles makes it the user's own, which
no import replaces; identities that were imported or chosen are never
replaced. The store keeps where each identity came from and which one is the
default (`identities.origin`: `created`, `imported`, `user`;
`identities.is_default`; schema version 4, which marks the first identity
named `Default` of an older database as created). What happened is shown
once in the status line (the toast on desktop), by nickname, e.g. "You now
use your TeamSpeak identity "X"; your previous one is kept". The runtime
setting `identity.import_from_teamspeak` (default on, also in Settings →
Profiles) turns it off, e.g. `--set identity.import_from_teamspeak=false`.
The sample data of `VOELIN_DEMO_UI` never imports.

**Settings → Profiles** lists the stored identities with where they came
from ("Made in Voelin", "Imported", "Chosen by you"), their security level
and unique ids; "Use as default" picks the one servers see from the next
connection on (`Store::set_default_identity` as the user's choice). The
store's default is the only default: a server connects with the identity its
bookmark names, if any (Profiles → Identity per server), else with the
store's default, and My Account shows the one the current server uses. The
page also renames, exports, deletes and raises the security level of
identities, and finds and imports the official clients' identities by
hand. `voelinctl identity list` shows the same (origin and default).
`VOELIN_OPEN=settings:profiles` (or `settings:identities`) opens it.

- **Import** prints what it found as nickname, unique id and security level —
  all three are public, everything else in an identity is the private key —
  and stores each one through `voelin-store` under its nickname. An identity
  whose TeamSpeak 3 unique id is already in the store is reported as a
  duplicate instead of being added twice, including repeats within one run.
  `--dry-run` reads and stores nothing. With `--auto`, a location that holds
  no identity is reported and skipped rather than ending the run.
- Source files are opened read-only (`SQLITE_OPEN_READ_ONLY`) **and
  `immutable`**, so SQLite takes no lock, creates no `-shm` file and does not
  replay a write-ahead log the running client left behind. Nothing of the
  user's is written, moved or copied. The cost is that an identity the client
  has written but not yet checkpointed into the main database file is not
  seen; closing the official client first makes sure everything is there.
- **Export** writes the TeamSpeak 3 `.ini` form, which the official client and
  every other tool read. A new file is created with mode `0600` on Unix,
  before anything is written into it.

An import is only trusted when the key pair is whole: the public key derived
from the private scalar has to verify what that scalar signs. The tests check
this on synthetic identities, and on a real installation when
`VOELIN_IDENTITY_IMPORT_CHECK` points `cargo test -p voelin-core` at a
database — which is how this was verified without any client's data entering
the repository. Every fixture in the tests is generated by
`tsclientlib::Identity::create`.

## Not supported

- **Encrypted identity exports.** The TeamSpeak 3 client can password-protect
  an `.ini` export; that container is not decoded. Export without a password
  and import that.
- **The other `ProtobufItems` item types.** Bookmarks, connection profiles and
  server lists are not imported. Only identities are.
- **myTeamSpeak identity synchronisation.** Desktop My Account supports an
  experimental account session, but does not fetch cloud identities or decrypt
  account backups. Sign in with the official client once so it writes the
  identities to `settings.db`, then import from there. See
  [research/myteamspeak.md](research/myteamspeak.md) for the account client's
  verification scope, session renewal, and cloud-sync limitations. The signed-in
  account is the main app profile; server identities and bookmark nicknames are
  independent.
- **Writing back to the official clients.** Voelin only reads them. Use the
  `.ini` export and the client's own import.
- **Bringing the nickname's history along.** Only the key pair and the
  nickname are imported. Which servers an identity was used on, its badges and
  its myTeamSpeak links stay behind; servers re-associate by unique id on the
  next connection, which is the part that matters.
