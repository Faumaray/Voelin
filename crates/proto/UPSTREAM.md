# Vendored upstream: ReSpeak/tsclientlib

The crates in this directory are a vendored fork of
[ReSpeak/tsclientlib](https://github.com/ReSpeak/tsclientlib), licensed
`MIT OR Apache-2.0` (see `LICENSE-MIT` and `LICENSE-APACHE` in this directory).

| Item | Source |
|---|---|
| tsclientlib | `ee3bc6f45a7137db7793ba5593a321df400d53e5` (master) |
| `tsproto-structs/declarations` (was a git submodule) | [ReSpeak/tsdeclarations](https://github.com/ReSpeak/tsdeclarations) `83cb8a94b65021bb8ec4beb339e5bfbdd93df60f` (MIT OR Apache-2.0, own `LICENSE-*` kept inside the directory) |

## Layout mapping

| Upstream path | Here |
|---|---|
| `tsclientlib/` | `crates/proto/tsclientlib/` |
| `tsproto/` | `crates/proto/tsproto/` |
| `utils/ts-bookkeeping/` | `crates/proto/ts-bookkeeping/` |
| `utils/tsproto-packets/` | `crates/proto/tsproto-packets/` |
| `utils/tsproto-structs/` | `crates/proto/tsproto-structs/` (submodule flattened into plain files) |
| `utils/tsproto-types/` | `crates/proto/tsproto-types/` |
| `README.md`, `CHANGELOG.md`, `.rustfmt.toml`, `clippy.toml` | `crates/proto/README.upstream.md`, `CHANGELOG.md`, `.rustfmt.toml`, `clippy.toml` |

Not vendored: `.appveyor/`, `tools/measure-latency.sh`, `tslientlib.code-workspace`,
the upstream root `Cargo.toml` (replaced by this repository's workspace).

## Local patches

The import commit contains the upstream files unmodified. Every later change to
files in this directory is listed here, newest last, so it can be diffed
against or offered back to upstream.

<!-- patches -->
1. **Workspace integration.** Path dependencies point at the flat `crates/proto/<crate>`
   layout; every crate gets `publish = false`.
2. **Replace `audiopus` with `opus2` 0.4** (`tsclientlib/src/audio.rs`,
   `tsclientlib/examples/audio_utils/audio_to_ts.rs`, `tsclientlib/Cargo.toml`).
   `audiopus` is unmaintained (RUSTSEC-2026-0150) and breaks with CMake 4. The
   `audiopus-unstable` feature (decoder complexity/DRED via a fork) is dropped.
3. **Init1 RSA puzzle off-thread** (`tsproto/src/algorithms.rs`, `tsproto/src/client.rs`,
   `tsproto/benches/modpow.rs`). `solve_rsa_puzzle` does Montgomery squarings with
   `crypto-bigint` (about 25% faster than `num-bigint` `modpow`) on a
   `spawn_blocking` thread that stops when the connect future is dropped. The level
   cap is `max_puzzle_level()` (default 100 million, was a fixed 10 million),
   adjustable with `set_max_puzzle_level`, because TeamSpeak 6 adapts the level.
4. **TeamSpeak 6 declarations** (`tsproto-structs/declarations/Messages.toml`, `Book.toml`).
   New fields `client_is_streaming` (book: `Client::is_streaming: Option<bool>`),
   `virtualserver_address`, `virtualserver_version_sign`; stream notifications
   (`notifystreamstarted`, `notifystreamstopped`, `notifystreaminfo`,
   `notifyjoinstreamrequest`, `notifyrespondjoinstreamrequest`,
   `notifystreamsignaling`, `notifystreamclientjoined`, `notifystreamclientleft`) and
   commands (`setupstream`, `stopstream`, `streamsignaling`, `respondjoinstreamrequest`,
   `removeclientfromstream`). See `docs/protocol-notes/ts6-streaming.md`.
5. **`curve25519-dalek-ng` → `curve25519-dalek` 4** (`tsproto-types`, `tsproto`). The `-ng`
   fork is unmaintained; the API is the same apart from the basepoint table being a
   reference. Verified by the license-chain unit tests and a live TeamSpeak 6 handshake.
6. **Panics on malformed input found by fuzzing** (`fuzz/`):
   - `tsproto/src/license.rs`: an empty license (`initivexpand2 l=`) indexed `data[0]`;
     license properties indexed past the end or with start > end. All property reads
     are bounds-checked now.
   - `tsproto-packets/src/packets.rs`: the invalid-codec error for short C2S audio
     packets read `content[4]` instead of the codec byte `content[2]`.
7. **TeamSpeak 6 stream commands, verified against 6.0.0-beta13.1**
   (`tsproto-structs/declarations/Messages.toml`). New fields `is_remove` and
   `reason` (stream leave reason). New command `joinstreamrequest id clid msg is_remove`
   (`JoinStreamRequestRequest`); `stopstream` and `removeclientfromstream` take the
   required `reason`; `notifyjoinstreamrequest` carries `is_remove`,
   `notifystreamstopped` and `notifystreamclientleft` carry `reason`, the latter also
   `return_code`.
8. **File transfer fixes** (`tsclientlib/src/lib.rs`, marked "Voelin patch").
   A server that refuses `ftinitdownload`/`ftinitupload` (file not found, no
   permission, quota) answers with an `error` for the command's return code, which
   came out as a `MessageResult` the caller cannot match to its
   `FiletransferHandle`; it now fails the transfer (`FiletransferFailed`). A
   transfer address of `0.0.0.0`/`::` means the server's own address. The return
   code and transfer id counters wrap instead of overflowing.
9. **TeamSpeak 6 times in milliseconds** (`ts-bookkeeping/build/message_parser.rs`,
   "Voelin patch"). TeamSpeak 6 sends some times, e.g. `datetime` of `notifyfilelist`,
   in milliseconds, which failed to parse and dropped the whole message (empty file
   lists). Values from 10^11 on (year 5138 in seconds) are taken as milliseconds.
10. **Test logger** (`tsclientlib/src/tests.rs`, "Voelin patch"). The tests' shared
   tracing setup used `init`, which panics when quickcheck (`use_logging`) has
   already installed a `log` logger; the panic poisoned the `Lazy` and failed every
   later test of the binary, depending on test order. It uses `try_init` now.
11. **TSDNS without an SRV record, as the official client resolves**
   (`tsclientlib/src/resolver.rs`). Without an SRV record at `_tsdns._tcp.<domain>`
   the official client asks a TSDNS server directly on TCP 41144 at the address's
   parent domains (`A/AAAA DNS resolve for possible TSDNS successful, "example.com"`,
   `TSDNS found at <ip>:41144 and queried successfully` in its log); upstream went
   straight to the address on port 9987, so servers published only through TSDNS
   (on another port) could not be reached without typing the port. Now: SRV
   `_ts3._udp.<address>`, then TSDNS (the servers `_tsdns._tcp` names, else the
   ones at each parent domain down to the last two labels), then A/AAAA on 9987.
   The steps start at once and come out in that order; each is given up after
   10 s, a TSDNS server after 3 s (connect and answer), so a port a firewall drops
   does not stall connecting. A port typed with the address now overrides the port
   of every result (the `_ts3._udp` one too). The TSDNS query sends the host
   without the port, and the answer is trimmed. SRV records of weight 0, the
   common weight, were dropped by the weighted ordering; they are kept now
   (RFC 2782 order). A CNAME among the SRV answers no longer panics. The lookups
   are injectable inside the module, and the tests use a fake TSDNS server and
   fake names; the upstream tests that need DNS and the internet are `#[ignore]`d.
12. **Private keys with leading zero bytes** (`tsproto-types/src/crypto.rs`,
   "Voelin patch"). The TeamSpeak export stores the private scalar as a DER
   integer, which drops leading zero bytes, and `EccKeyPrivP256::from_tomcrypt`
   handed those fewer than 32 bytes to `from_short`, which refused them: one
   identity in 256 (the official client's ones included) could not be imported.
   The scalar is padded to 32 bytes now; a test round-trips such keys.
13. **Channel banners** (`tsproto-structs/declarations/Messages.toml`, `Book.toml`).
   TeamSpeak 6 channels have a banner picture, `channel_banner_gfx_url` and
   `channel_banner_mode`, which its server sends in `channellist`,
   `notifychannelcreated` and `notifychanneledited` (verified against
   6.0.0-beta13.1; upstream declared them for `channellist` only, so creating
   or editing a channel logged "Unknown argument" and lost them). The book's
   `Channel` keeps them as `banner_gfx_url` and `banner_mode`. The mode stays
   a string, as upstream declared it, so a value the parser does not know
   cannot drop the whole channel list; its numbers are those of
   `HostBannerMode` (the server's `channeledit` documentation: 0 as it is,
   1 stretched, 2 scaled keeping the aspect ratio).
14. **myTeamSpeak connection identity** (`tsproto/src/myts.rs`,
    `tsclientlib/src/myts.rs`, `tsclientlib/src/lib.rs`). Validated, redacted account
    credentials retain the original raw signing scalar and validate again when
    deserialized from a credential store. Connection-bound account proofs use the
    negotiated shared IV and the official raw-scalar signing construction, not
    Ed25519 seed expansion. `clientinit` and `updatemytsid` carry the six official
    case-sensitive proof fields; clearing an account sends only an empty
    `myTeamspeakId`. Connection options retain the latest account for reconnects;
    callers cancel and rebuild an in-flight attempt when its snapshot changes.
    Tests cover an independent signature vector, persistence validation/redaction,
    wire field names, clearing and reconnect snapshot replacement. The account
    UUID and the ordinary server identity remain separate.
15. **myTeamSpeak presentation, TeamSpeak 6** (`tsproto/src/license.rs`,
    `tsproto/src/myts.rs`, `tsclientlib/src/myts.rs`, `tsclientlib/src/lib.rs`,
    `tsproto-packets/src/packets.rs`, `tsproto-structs/declarations/Book.toml`).
    License block types 4–7 (Token, License_Sign, MyTsId_Sign, Updater), which
    carry nothing after the header. `myts::Certificate` checks a myTeamSpeak
    certificate chain as a TeamSpeak 6 server does: leaf type, validity window,
    key derived from a given root, cofactorless Ed25519 with a reduced S;
    `Identity::id_bytes` and `public_signature_certificate`. `MytsData` holds the
    avatar and the badges, each with the certificate that verifies it, and the
    User Tag; `MytsData::updates` builds `updatemytsdata` (split by certificate,
    bare parameters to clear) and `clientupdate client_user_tag`, which
    `Connection::send_myts_update` and `send_user_tag` send. The escaped writer
    also escapes `\a` and `\b`, as TeamSpeak does for binary values. Book:
    `Client::my_team_speak_avatar`, `my_team_speak_id` and `signed_badges`.
