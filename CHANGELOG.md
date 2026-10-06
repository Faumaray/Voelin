# Changelog

User-visible changes of the apps and the `tsgw` gateway. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[docs/release.md](docs/release.md#versioning).

## [Unreleased]

### Added

- myTeamSpeak profile: Settings → My Account shows the account's avatar,
  description, registration date, previous sign-in, badges and signed-in
  devices from the account service; the avatar is also the sidebar's
  profile picture where a server has none for us.
- Log files: each run logs to `<state>/voelin/logs/voelin.log`, the previous
  four runs are kept; `VOELIN_LOG` sets what is logged. Settings → Advanced
  opens the folder.
- Linux `.tar.gz`: libvpx and libdav1d in `lib/`, and `bin/voelin-install-deps`
  for FFmpeg, VA-API drivers, PipeWire, the desktop portal and a keyring.
- Windows packages ship FFmpeg's LGPL libraries, so hardware encoders and
  decoders work without installing FFmpeg.
- AV1 decoding through dav1d in every desktop build.
- 122 more signed client versions (`Versions.csv`).
- Other users' myTeamSpeak avatars (TeamSpeak 6), as the official client
  shows them: where the server has no avatar of them, its avatar could not
  be downloaded, or the server is only observed.
- Voice connections show servers the account's avatar, the badges chosen
  in Settings → My Account (up to three) and its User Tag, as the official
  client does, so other TeamSpeak 6 users see them; again after a
  reconnect or an account change, and cleared on sign-out. A saved session
  is enough: the avatar, its certificate, the signed badges and the User
  Tag's token come from myTeamSpeak's services, and Voelin checks each
  signature as the server does before sending it. The log says what the
  server published.
- Settings → My Account shows the account's User Tag and lets you choose the
  badges servers show.
- Pictures (banners, avatars) go through the desktop's proxy where the
  desktop portal names one (Linux), and through the system proxy on
  Windows and macOS; SOCKS proxies work too.
- tsgw logs what it does: a startup summary, the address the TeamSpeak
  server sees for its queries (with a hint when that address is not
  allowlisted), every app connection with its logins and how long they
  took, refused logins, slow requests, ServerQuery failures, relays and the
  observer. `log.level` (`TSGW_LOG_LEVEL`) sets what is logged, `log.file`
  (`TSGW_LOG_FILE`) also writes to a file that is rotated at 20 MB (five
  kept); colour only on a terminal.
- Every gateway a server publishes is tried in turn (TLS first, 20 seconds
  each) before waiting to try again, so a TLS proxy that fails falls back to
  the plain gateway published next to it.

### Changed

- New login page; it no longer opens when a session is saved (only when one
  has expired), and "Continue without an account" is remembered.
- Selecting a server observes it through its gateway at once; the Observe
  button is gone. The server dialog asks only for the address, password,
  name and nickname: the gateway is found from the address.
- Relayed messages appear in TeamSpeak under the author's own nickname
  (`Nick1` while the name is taken), not as `[Nick] text` from the relay.
- The client reports itself as TeamSpeak 6 (`6.0.0-beta4.1` on Linux,
  `6.0.0-beta2` on Windows) instead of `3.?.?`; `--client-version generic`
  claims the old version.
- Watching streams: VP8 decodes on one thread (FFmpeg's slice threads made
  multi-partition streams 40-80 % slower), pictures are converted for the
  window on their own thread into reused buffers, and libvpx and dav1d use
  fewer threads. A 1440p60 VP8 stream from a 6-8 core sender showed 35-49
  fps; FFmpeg decodes it at 95-101 fps on one thread against 54-60 on four.
- Release builds no longer limit Cargo to one job.
- The gateway is invisible: relayed messages look like everyone else's (no
  "via relay"), and a gateway that is away is tried again quietly instead
  of showing a WebSocket error; only what the user did and failed is
  told, in plain words. A server whose gateway is away no longer shows as
  connecting, so it can always be joined.
- A server's published gateway replaces one stored before (found or typed
  into an older version), so a gateway that moved is followed.
- zbus's warnings about desktop portal requests are left out of the logs.
- Banners: up to 128 MiB, more time on slow hosts, BMP and ICO too, and
  SVG after a comment or DOCTYPE; pictures larger than 4096 pixels on a
  side are scaled down. A banner that fails is tried again for as long as
  it is shown (every 15 minutes at most), and the log says why it is
  missing (HTTP status, an HTML page, a video, too large). The picture
  cache defaults to 1 GiB, the decoded images to 256 MB.
- Banners that fail: the log names the URL (without its query) the first
  time, says when a connection stalled and after how much data, and when it
  could not connect at all. A banner that is not there (HTTP 400, 404, 410)
  is tried again after 15 minutes and then hourly; a host that asks to wait
  (429, 503) is waited for. A banner shown again is only checked for
  changes (ETag, Last-Modified), not downloaded again. At most 6 downloads
  run per host, so a slow host cannot hold up the others.
- The gateway: only a refused login stops observing; anything else is tried
  again after 1, 2, 5, 10, 20, then every 30 seconds, and at once when voice
  connects. A connection that stops answering is noticed within 90 seconds.
  Finding a server's gateway asks DNS and the gateway at once instead of one
  after the other. The log says what was found, which gateway was tried,
  how long each step took and why one failed.
- tsgw answers faster: each ServerQuery command takes under 2 ms instead of
  about 45 ms (TCP_QUICKACK), so logins and startup take milliseconds.
  TeamSpeak's flood protection is shared by all of tsgw's query
  connections, so tsgw no longer earns a 600 second ban. It starts while
  the TeamSpeak server is still down and connects once it is up, its
  `/health` says what is missing, and it stops cleanly on SIGTERM.

### Fixed

- The account's avatar is downloaded from its own link; it was asked for
  as an upload link and refused with HTTP 403.
- Observing no longer stops for the whole run the first time a server is
  joined with a new identity (the gateway did not know it yet).
- tsgw reconnects its query connection for logins when the TeamSpeak server
  restarts; before, every login was refused until tsgw was restarted.

## [0.0.1-alpha] - 2026-10-04

The first alpha.

### Added

- myTeamSpeak account sign-in (desktop): sign in with the account's email,
  password and one-time code; the session is kept in the system keyring and
  checked at start. Voice connections present the account to servers (its
  id with a proof signed by the account) and follow sign-in and sign-out
  live. The account screen links to the account website for creating an
  account, resetting the password and managing it.
- Banners: the host banner and TeamSpeak 6 channel banners, behind the
  server card, the channel rows and the chat and voice headers. Banners on
  the web (http, https) and in the server's own files (`ts3image://`) are
  downloaded, up to 64 MiB, retried after failures and reloaded as often as
  the server asks; server and channel icons and avatars are retried too.

- Desktop app (Linux Wayland/X11, Windows): servers and bookmarks, channel
  tree with talking indicators, server and channel chat, voice with
  push-to-talk (global hotkeys), voice activation, echo cancellation, noise
  suppression and gain control, per-user volume.
- Observing a server invisibly and chatting in channels without joining
  them, through the `tsgw` gateway or own ServerQuery credentials.
- TeamSpeak 6 streams: watching and sharing screens with sound (VP8, VP9,
  optional H.264 through Cisco's OpenH264, AV1 decoding).
- Hardware video encoders (VA-API, NVENC, Quick Sync, AMF, Media
  Foundation, VideoToolbox) and more software ones (x264, SVT-AV1, libaom)
  through an installed FFmpeg, loaded at runtime; each is tested at start
  and skipped with a reason if it does not work. Viewers that answer with
  another offered codec get their own encoder. Settings:
  `stream.hardware_acceleration`, `stream.encoder_backend`, `stream.codec`;
  `voelinctl stream encoders` lists what works.
- Video decoding through the installed FFmpeg as well: hardware first
  (VA-API, NVDEC, D3D11VA, DXVA2, VideoToolbox), then FFmpeg's software
  decoders, then the built-in ones; each decoder is tested at start, and
  one that fails while watching is replaced by the next without ending the
  stream. Viewers now take AV1, HEVC and H.264 without OpenH264, and
  H.264 with B-frames as the official client sends it (OpenH264 showed
  under 1 fps of such a stream; 60 fps now). After a lost frame H.264 and
  HEVC go on decoding while a keyframe is asked for. Settings:
  `stream.hardware_decoding`, `stream.decoder_backend`.
- Streams connect and play in more cases: H.264 is offered in Constrained
  High and Constrained Baseline, each viewer getting the profile it takes;
  a connection that comes up but carries nothing is replaced by one
  without the SRTP profile or the codec it used; host addresses hidden
  behind mDNS names (as browsers send them) are resolved; larger socket
  buffers keep high-bitrate streams from losing packets in bursts.
- Streams up to 60 Mbit/s, 8K and 320 fps where the hardware can: an
  automatic bitrate by default (from the size and frame rate, up to 60
  Mbit/s; any bitrate can be typed), share and studio presets up to 8K,
  320 fps and 60 Mbit/s, and screens in memory converted on the GPU for
  VA-API encoders. A stream's bandwidth estimate no longer stops near 10
  Mbit/s (packets were let out one a millisecond), and H.264 offers
  declare the level the stream needs (up to 5.2) instead of 3.1. Setting:
  `stream.bitrate_kbps` (0: automatic).
- `tsgw`: companion gateway for server admins (presence, channel chat
  relay, history).
- Chat history: messages are kept on the device and, with a gateway, what
  was said while away appears when a chat opens; scrolling back loads older
  messages from the gateway. The same message seen twice (over voice and
  through the gateway) is shown once. Settings: `chat.store_history`,
  `chat.history_page`, `chat.dedupe_tolerance_ms`, `chat.retention_days`.
- Opt-in local crash reports; third-party notices in the About page.

### Known issues

- Account keyring access runs on the UI thread: a locked keyring's unlock
  prompt holds the window until it is answered.
