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
- The voice channel view on desktop can copy an invite link to the channel,
  as the phone's Invite button does.
- Other people's myTeamSpeak badges: up to three pictures after their name
  in the channel tree and the members panel, and their names on the member
  card (what each is for on hover).
- Links in chat open in the browser; one whose text is not its address
  first shows where it goes. A right-click on a link opens or copies it.
- `ts3server://` and `teamspeak://` links in chat (and `tmspk.gg/s/…`)
  open the server dialog filled in from the link, its channel and key
  included, or move you into the link's channel when you are in voice on
  that server. Nothing connects before Connect, and a saved server keeps
  its nickname. `tmspk.gg` invite codes can't be opened yet.
- Every message can be quoted into the composer (`> ` lines) and its text
  copied, from the actions on hover or a right-click menu, also on servers
  without a gateway.
- A "New" line above the first unread message of a chat, and a bar that
  counts the new messages with a jump to the first; unread counts survive
  restarts. Messages count as read only while their chat is on screen in
  the focused window.

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
- Pop-ups, menus and tooltips have a clearer edge, and the member card, the
  emoji picker and the notifications panel dim the window behind them.
- Double-clicking a channel of a server without voice connects into it
  (it said "not connected with voice").
- Older chat messages load when scrolling up, above the messages on screen,
  which stay in place; the Older messages button remains for a chat that
  fits on the screen.
- One dialog adds a server, with Save and Connect (Enter connects).
  Connect connects only a server it saved (Join connected the server shown
  before when saving failed).
- A slimmer top bar on desktop: only the search and the bell. The bell's
  panel opens under the bell and is as tall as its notices; the members
  button is in the voice channel's header (as in the chat's), About in
  Settings. The user card has no settings gear (the rail's stays); its
  avatar and name open your account.
- Home shows where you left off (the last voice channel, joined in one
  click) and your unread mentions; the welcome banner shows only before the
  first server. Friends show once, the quick actions are gone, and the
  sidebar lists your recent private chats.
- Each server has a colour of its own (two often shared a blue), and small
  server icons are drawn sharp at twice their size instead of stretched.
  Small avatars, as in the channel tree, show one letter.
- Channels have one icon everywhere, the speaker, and are named without a
  `#` in tabs and elsewhere.
- The members panel shows a hand on those who need talk power to speak,
  instead of everyone's talk power as a bare number (the member card still
  has it).
- The chat header's pinned messages and topics are icon buttons with a
  count, so the channel's topic has room.
- Private chats live on the Direct Messages page, not in the server's chat
  tabs, and their unread messages count there (on the phone, on the Home
  tab) instead of on the server rail.

### Fixed

- The account's avatar is downloaded from its own link; it was asked for
  as an upload link and refused with HTTP 403.
- Hover help in the Stream Studio (a source's error, the status details)
  shows each time and closes when the pointer leaves; it showed only once,
  and stayed open until a click.
- Spacer channels: `[spacer]`, `[lspacer]`, `[rspacer]` and `[*spacer]` lines
  are drawn as TeamSpeak draws them, like `[cspacer]` was, and chats, search
  results, notifications and the other screens show a spacer's text instead
  of its raw name.
- Locked channels ask for their password (kept until voice disconnects, and
  asked again when it is wrong), and joining a full channel or one the
  server refuses says why; joining did nothing before.
- Pokes can carry a message (up to 100 characters); they were always sent
  empty.
- While a priority speaker talks, everyone else (watched streams too) is
  dimmed by the server's setting, as in TeamSpeak; nobody was dimmed before.
- BBCode from other clients shows as formatting (bold, colours, links,
  quotes, lists, code, pictures) instead of raw tags, and the server's
  welcome message opens the server chat. Messages with emoji keep their
  line breaks.
- Joining a friend in a subchannel of a server without voice connects into
  that subchannel; it connected into the default channel.
- Closing a chat tab left of the selected one keeps the selected chat; the
  one after it was selected.
- Connecting with voice from a private chat (Direct Messages) keeps that
  conversation open; it turned into the voice channel's chat, and the
  composer sent there.

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
