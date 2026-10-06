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
- Icon buttons show their name on hover (desktop), also while they are
  disabled.

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

### Fixed

- The account's avatar is downloaded from its own link; it was asked for
  as an upload link and refused with HTTP 403.
- Hover help in the Stream Studio (a source's error, the status details)
  shows each time and closes when the pointer leaves; it showed only once,
  and stayed open until a click.

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
