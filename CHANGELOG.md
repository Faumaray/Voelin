# Changelog

User-visible changes of the apps and the `tsgw` gateway. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[docs/release.md](docs/release.md#versioning).

## [Unreleased]

The first release is in preparation.

### Added

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
- `tsgw`: companion gateway for server admins (presence, channel chat
  relay, history).
- Chat history: messages are kept on the device and, with a gateway, what
  was said while away appears when a chat opens; scrolling back loads older
  messages from the gateway. The same message seen twice (over voice and
  through the gateway) is shown once. Settings: `chat.store_history`,
  `chat.history_page`, `chat.dedupe_tolerance_ms`, `chat.retention_days`.
- Opt-in local crash reports; third-party notices in the About page.
