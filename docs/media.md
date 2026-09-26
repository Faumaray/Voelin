# Media: frames, codecs and capture (`tsc-media`)

`crates/tsc-media` turns screens and system audio into frames, encodes and
decodes video, and converts pixels for rendering. It does no networking:
`tsc-stream` carries the encoded frames (`Peer::write(MediaKind::Video,
MediaTime::new(frame.pts_90khz, Frequency::NINETY_KHZ), data)`), and received
`MediaFrame`s go to `VideoDecoder::decode`.

## API overview

| Item | What it is |
|---|---|
| `VideoFrame { width, height, timestamp, data: FrameData }` | `FrameData::I420 { y, u, v }`, `Nv12 { y, uv }`, `Bgra(Plane)`, `Rgba(Plane)`; `Plane { data, stride }` |
| `AudioBuffer { samples, channels, timestamp }` | interleaved `f32`, 48 kHz |
| `convert::to_rgba(&frame, out, stride)`, `to_rgba_vec`, `to_i420`, `psnr` | BT.601 limited range (the WebRTC default). `to_rgba` writes straight into a Slint `SharedPixelBuffer<Rgba8Pixel>` (`make_mut_bytes()`, stride `width * 4`) |
| `Codec` | `Vp8`, `Vp9`, `H264`, `Av1`; `FromStr` for SDP/MIME names; `From`/`TryFrom` `str0m::format::Codec` with feature `str0m` |
| `VideoEncoder` | `encode(&frame, force_keyframe) -> Vec<EncodedFrame>`, `set_bitrate(bps)`, `codec()`, `backend()` |
| `EncodedFrame { data, keyframe, pts_90khz }` | one frame for `Peer::write` |
| `VideoDecoder` | `decode(&[u8]) -> Option<VideoFrame>` (timestamp zero; the caller knows the RTP time) |
| `EncoderConfig { fps, bitrate_bps, keyframe_interval, content, threads }` | resolution follows the frames; a size change restarts with a keyframe |
| `Codecs` | `new()`, `with_openh264(lib)`, `decoders()` (viewer order), `encoders()` / `encoder_codecs()` (streamer order), `new_decoder(codec)`, `new_encoder(codec, config)`, `pick_encoder(&accepted)` |
| `ScreenCapture` | `sources()`, `start(&SourceId, &CaptureOptions)` (async: the portal asks the user), `stop()` |
| `AudioCapture` | `start() -> FrameReceiver<AudioBuffer>`, `stop()` |
| `FrameReceiver<T>` | bounded queue that drops the oldest item; `recv().await`, `try_recv()`, `recv_timeout()` |
| `capture::default_screen_capture()`, `default_audio_capture()` | the backend for this session |
| `capture::synthetic::{SyntheticScreen, SineSource}` | test pattern (moving rectangle, frame counter) and sine tone |

```rust
let codecs = Codecs::new().with_openh264(OpenH264::find_in(&data_dir)?); // H.264 optional
let mut capture = tsc_media::capture::default_screen_capture()?;
let mut frames = capture.start(&SourceId::Monitor(0), &CaptureOptions::default()).await?;
let codec = codecs.pick_encoder(&viewer_codecs).unwrap();
let mut encoder = codecs.new_encoder(codec, EncoderConfig::default())?;
while let Some(frame) = frames.recv().await {
    for f in encoder.encode(&frame, keyframe_requested)? {
        peer.write(MediaKind::Video, MediaTime::new(f.pts_90khz, Frequency::NINETY_KHZ), f.data);
    }
}
```

## The pipeline in `tsc-core` (`media` module)

```
streamer: ScreenCapture ─ VideoEncoder (codec of our offer) ─┐
          AudioCapture ─ 20 ms stereo Opus (128 kbit/s) ─────┴─ MediaSink: StreamSink / EncodedSource
viewer:   Engine::subscribe_frames ─ VideoPipeline (thread) ─ VideoDecoder ─ picture callback ─ Latest
          stream audio ─ session audio thread (mixer, own volume) ─ speakers
```

- `Streamer::start(&codecs, StreamerConfig)` starts capturing right away (the
  portal asks the user then), so a cancelled dialog never starts a stream;
  frames are encoded only after `attach(sink)`, i.e. once the stream is live.
  The encoder runs at the setup's bitrate; a viewer's keyframe request (on
  the sink) forces a keyframe, and is kept until the encoder produced one.
  Audio follows the capture clock across gaps. With the test pattern
  (`SourceId::Synthetic`) the audio is a quiet sine tone. The portal's restore
  token is available afterwards (`restore_token()`) to store.
- `peer_config(&codecs, config)` makes a viewer accept only what we decode
  and a streamer offer only the codec it encodes (VP8 unless changed): all
  viewers get the same frames, and a viewer picks from the offer.
- `VideoPipeline` decodes on its own thread. It starts at a keyframe, and
  after a lost frame (`contiguous == false`, a lagging frame bus, a queue
  longer than 30 frames) or a decoder error it skips to the next keyframe,
  asking for one at most every 500 ms. Keyframes are recognised from the
  bitstream (VP8 frame tag, VP9 header, H.264 IDR/SPS NAL units, AV1
  sequence header OBU).
- `Viewer` feeds a `VideoPipeline` from the engine and sends
  `RequestStreamKeyframe`; `LocalPreview` runs capture → encoder → decoder
  without a server (the desktop app's `TSC_DEMO_STREAM`).
- The audio of watched streams does not go through this module: the session
  hands the Opus frames to its audio thread, which plays them through the
  jitter buffer and mixer under a made-up client id, with its own volume.
  Playback is mono like the rest of the audio path.

## Codecs

Preference: a viewer accepts VP9 > VP8 > AV1 > H.264; a streamer encodes with
hardware > VP8 (libvpx) > H.264 (OpenH264) > VP9 (libvpx, expensive in
software). `Codecs` filters both lists by what is compiled in and loaded.

| Backend | Feature | Encode | Decode | Library |
|---|---|---|---|---|
| libvpx | `vpx` (default) | VP8, VP9 | VP8, VP9 | system libvpx (1.14 tested), dynamically linked; bindings from `libvpx-native-sys` (pre-generated, no bindgen) |
| OpenH264 | `openh264` (default) | H.264 Constrained High / Baseline | H.264 | Cisco's prebuilt binary, loaded at runtime |
| dav1d | `av1` | – | AV1 | system libdav1d >= 1.3 (1.4.1 tested) |
| hardware | – | not yet | – | `codec::hw::HardwareEncoderFactory`; VA-API and Media Foundation stubs report nothing (TODO) |

libvpx settings follow libwebrtc's realtime setup: one pass CBR, no lag,
`VPX_DL_REALTIME`, error resilient, `cpu-used` 8 (VP8: fixed speed −8), keyframes
only at start and on request unless `keyframe_interval` is set, screen content
tuning (`VP8E_SET_SCREEN_CONTENT_MODE`, `VP9E_SET_TUNE_CONTENT`, static
threshold), VP9 row-mt, tiles and cyclic-refresh AQ. `set_bitrate` reconfigures
the running encoder (`vpx_codec_enc_config_set`). The C API is wrapped in
`codec/vpx/raw.rs`, one of the two modules with `unsafe` code; existing safe
wrappers either encode only or cannot change the bitrate at runtime.

The OpenH264 encoder has no safe runtime bitrate change in the `openh264`
crate, so `set_bitrate` recreates it (the next frame is an IDR). It enables
frame skipping so rate control can hold the bitrate (`encode` may then return
no frame).

### OpenH264 and Cisco's license

H.264 is patent-encumbered. Cisco pays the MPEG LA royalties for its own
OpenH264 binaries, but only when every user downloads the binary from Cisco
themselves. Therefore:

- The OpenH264 source is never compiled (the `openh264` crate is used with
  `default-features = false, features = ["libloading"]`; its bundled sources
  stay unused in the registry).
- `codec::h264::download_openh264(dir)` (feature `openh264-download`) fetches
  `https://ciscobinary.openh264.org/<file>.bz2` for this platform (OpenH264
  2.6.0), unpacks it, checks the SHA-256 against a pinned table, and stores it
  in `dir`. Call it only after the user opted in.
- `OpenH264::load(path)` / `find_in(dir)` load only files whose SHA-256 matches
  a known Cisco build, so no other code gets executed.
- The app must show `codec::h264::OPENH264_ATTRIBUTION` ("OpenH264 Video Codec
  provided by Cisco Systems, Inc.") next to the switch that enables it and in
  its About page / notices, must let the user disable it, and must not ship
  the binary in its packages. Cisco's binary is BSD-2-Clause licensed; its
  license text belongs in the notices shipped with the app.

Without the library, `Codecs` neither offers nor accepts H.264, and
`OpenH264::load` fails with `Error::CodecUnavailable` ("OpenH264 library not
found at ...").

## Capture

| Platform | Screen | System audio |
|---|---|---|
| any | `SyntheticScreen` | `SineSource` |
| Linux X11 (`x11`, default) | `X11Capture`: MIT-SHM 1.2 (memfd passed to the server) or GetImage; RandR 1.5 monitors; windows from `_NET_CLIENT_LIST` (root children without a WM); XFixes cursor blended in | |
| Linux Wayland (`pipewire`, default) | `PortalCapture`: xdg-desktop-portal ScreenCast via `ashpd`, frames from a PipeWire video stream | `PipeWireAudioCapture`: default sink monitor (`stream.capture.sink = true`) |
| Windows | `WindowsCapture`: Windows Graphics Capture (`windows-capture`), monitors and windows | `WasapiLoopback`: process loopback excluding our own process tree, falling back to plain loopback |

`default_screen_capture()` picks the portal when `WAYLAND_DISPLAY` is set (or
`XDG_SESSION_TYPE=wayland`), X11 when `DISPLAY` is set, WGC on Windows.

Notes:

- X11 needs a 24/32-bit TrueColor visual (BGRx in memory). Window capture
  reads the window itself, clipped to the screen; without a compositor, parts
  covered by other windows may be stale. MIT-SHM needs a local server with
  MIT-SHM 1.2; otherwise GetImage is used. The only `unsafe` here is mapping
  our memfd (`capture/x11/shm.rs`).
- Portal: `start` needs a tokio runtime (zbus runs on it) and may show the
  desktop's picker. Choices persist with `PersistMode::ExplicitlyRevoked`: store
  `PortalCapture::restore_token()` (e.g. in `tsc-store` settings) and pass it to
  `PortalCapture::with_restore_token` next time. The cursor is embedded when
  the portal supports it. Only shared-memory buffers (BGRx/BGRA/RGBx/RGBA) are
  negotiated; DMA-BUF import is a TODO. Without a session bus or portal,
  `start` returns `Error::CaptureUnavailable`; a cancelled dialog returns
  `Error::Cancelled`.
- PipeWire system audio captures everything the default sink plays, including
  our own playback (TeamSpeak voices): PipeWire has no "all but this process"
  monitor. Excluding our nodes needs a private null sink with links from every
  other application's output (via the registry); TODO. Until then, play voices
  on another sink or accept that viewers hear them. Without a PipeWire daemon,
  `start` returns `Error::CaptureUnavailable`.
- Windows process loopback needs Windows 10 2004 or later.

## Build requirements

Linux (Debian/Ubuntu packages): `libvpx-dev` (feature `vpx`),
`libpipewire-0.3-dev libspa-0.2-dev libclang-dev` (feature `pipewire`; the
PipeWire bindings run bindgen), `pkg-config`, and `libdav1d-dev` for
`--features av1`. X11 capture is pure Rust (x11rb) and needs no packages.
Tests of X11 capture need an X server: `xvfb`, run with `xvfb-run -a cargo
test -p tsc-media` (skipped without `DISPLAY`).

Windows: libvpx is not found through pkg-config there. Install it (e.g.
`vcpkg install libvpx:x64-windows`) and set `VPX_LIB_DIR`, `VPX_INCLUDE_DIR`
and `VPX_VERSION`, or build without the `vpx` feature. The Windows capture
code is type-checked on Linux (`cargo clippy -p tsc-media --target
x86_64-pc-windows-gnu --no-default-features --features openh264`) but has not
run on Windows yet.

## Licenses

| Component | License | How it is used |
|---|---|---|
| libvpx | BSD-3-Clause | system library, dynamic |
| `libvpx-native-sys` | MPL-2.0 | unmodified bindings |
| `yuv` (yuvutils-rs) | BSD-3-Clause OR Apache-2.0 | |
| `openh264` / `openh264-sys2` | BSD-2-Clause | bindings, `libloading` only |
| Cisco OpenH264 binary | BSD-2-Clause + Cisco's royalty coverage | downloaded by the user, see above |
| `dav1d` crate / libdav1d | MIT / BSD-2-Clause | optional, system library |
| `x11rb`, `memmap2`, `rustix` | MIT OR Apache-2.0 (rustix also Apache-2.0 WITH LLVM-exception) | |
| `ashpd`, `pipewire`, `libspa` / libpipewire | MIT | libpipewire is dynamic |
| `windows-capture`, `wasapi`, `windows` | MIT (`windows`: MIT OR Apache-2.0) | Windows only |
| `bzip2` / `libbz2-rs-sys` | MIT OR Apache-2.0 / bzip2-1.0.6 | unpacks the OpenH264 download; `bzip2-1.0.6` is allowed in `deny.toml` |

## Verification status

| What | How | Status |
|---|---|---|
| Frame validation, YUV ↔ RGB (values, NV12, odd sizes, strides) | unit tests | tested |
| VP8 / VP9 encode → decode (PSNR > 30 dB, rectangle colour, forced keyframes, bitrate change, resolution change) | unit tests, `tests/codec_roundtrip.rs` | tested |
| H.264 with Cisco's 2.6.0 library (PSNR, Constrained High `profile_idc` 100, Baseline 66) | `tests/openh264.rs` with `TSC_OPENH264_LIB` | tested locally, skipped without the library |
| OpenH264 download, SHA-256 check, reuse | `tests/openh264.rs -- --ignored` | tested locally (network) |
| H.264 library missing / unknown file | unit + integration tests | tested |
| AV1 decoder construction, garbage input | unit test (`--features av1`) | tested; no AV1 stream decoded (no encoder available) |
| X11 capture (MIT-SHM and GetImage), sources, window capture → VP8 → decode, cursor | `tests/x11_capture.rs` under Xvfb | tested |
| Portal / PipeWire error paths (no bus, bus without portal, no daemon) | unit tests, manual probe | tested |
| Portal capture, PipeWire video and audio streams | – | compiles only (no portal or PipeWire daemon here) |
| Windows Graphics Capture, WASAPI | – | type-checked for `x86_64-pc-windows-gnu` only |
| Hardware encoders | – | stubs |
| Test pattern → VP8 → decoder, rectangle position and colour | `tsc-core` `media::tests::local_preview_decodes_the_pattern` | tested |
| Test pattern → two engine stream tasks → str0m peers on loopback → decoder | `tsc-core` `stream::tests::test_pattern_through_stream_tasks` | tested |
| Stream audio (RTP time → jitter buffer ids, volume, end) | `tsc-core` `audio::tests::stream_audio_with_volume`, stream task test | tested |
| The same through the TeamSpeak 6 server | `tsc-core/tests/media_live.rs` (`TSC_LIVE=1`), `tsctl stream start --synthetic` / `watch --expect-frames` | tested against 6.0.0-beta13.1 |
| X11 monitor capture → VP8 → server → decoder | `tsctl stream start --source x11` under Xvfb | tested manually (debug build: about 8 fps at 1400×900) |
| Desktop app watching and sharing | Xvfb, `TSC_AUTOWATCH` / `TSC_AUTOSHARE` against tsctl, screenshots | tested manually |
