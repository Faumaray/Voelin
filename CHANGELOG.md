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
