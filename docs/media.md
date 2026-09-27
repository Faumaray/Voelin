# Media: frames, codecs and capture (`voelin-media`)

`crates/voelin-media` turns screens and system audio into frames, encodes and
decodes video, and converts pixels for rendering. It does no networking:
`voelin-stream` carries the encoded frames (`Peer::write(MediaKind::Video,
MediaTime::new(frame.pts_90khz, Frequency::NINETY_KHZ), data)`), and received
`MediaFrame`s go to `VideoDecoder::decode`.

## API overview

| Item | What it is |
|---|---|
| `VideoFrame { width, height, timestamp, data: FrameData }` | `FrameData::I420 { y, u, v }`, `Nv12 { y, uv }`, `Bgra(Plane)`, `Rgba(Plane)`; `Plane { data, stride }` |
| `FrameRef { width, height, timestamp, pixels: PixelsRef }` | the same with borrowed planes (`PlaneRef { data: &[u8], stride }`), e.g. a mapped capture buffer; `VideoFrame::view()`, `FrameRef::to_frame()` |
| `AudioBuffer { samples, channels, timestamp }` | interleaved `f32`, 48 kHz |
| `convert::to_rgba(&frame, out, stride)`, `to_rgba_vec`, `to_i420`, `psnr` | BT.601 limited range (the WebRTC default). `to_rgba` writes straight into a Slint `SharedPixelBuffer<Rgba8Pixel>` (`make_mut_bytes()`, stride `width * 4`) |
| `Codec` | `Vp8`, `Vp9`, `H264`, `Av1`; `FromStr` for SDP/MIME names; `From`/`TryFrom` `str0m::format::Codec` with feature `str0m` |
| `VideoEncoder` | `encode(&frame, force_keyframe) -> Vec<EncodedFrame>`; `encode_with(&frame, force_keyframe, &mut |EncodedChunk| ..)` hands out the encoder's own buffer (no copy); `set_bitrate(bps)` (libvpx: in place, no keyframe), `set_fps`, `speed()`, `codec()`, `backend()` |
| `EncodedFrame { data, keyframe, pts_90khz }` | one frame for `Peer::write` |
| `VideoDecoder` | `decode(&[u8]) -> Option<VideoFrame>` (timestamp zero; the caller knows the RTP time) |
| `EncoderConfig { fps, bitrate_bps, keyframe_interval, content, threads, speed }` | resolution follows the frames; a size change restarts with a keyframe; `threads` is a maximum (0: all CPUs but one), still capped at one per 320x240 pixels; `speed: None` adapts libvpx `cpu-used` to the encode time |
| `Codecs` | `new()`, `with_openh264(lib)`, `decoders()` (viewer order), `encoders()` / `encoder_codecs()` (streamer order), `new_decoder(codec)`, `new_encoder(codec, config)`, `pick_encoder(&accepted)` |
| `ScreenCapture` | `sources()`, `start_sink(&SourceId, &CaptureOptions, Box<dyn FrameSink>)` (frames borrowed from the capture buffer, on the backend's thread), `start(..)` (copies into a queue), `stop()`; async: the portal asks the user |
| `FrameSink` | `max_fps()` (may change while capturing), `wants(timestamp)` (asked before anything is mapped or copied), `frame(FrameRef) -> bool` |
| `FramePacer` | frame-rate cap by timestamps, evenly spaced |
| `convert::Converter` | `to_i420_into(&FrameRef, &mut VideoFrame)`: any stride, BGRx/BGRA/RGBx/RGBA/I420/NV12, row bands on all cores, no allocation |
| `scale::{PlaneScaler, Pyramid, scale_i420}` | area-average downscaling (bilinear up), 2x2 box fast path; `Pyramid::process(&FrameRef, sizes, due, out)` converts once and derives every size from the nearest larger one |
| `pool::FramePool`, `handoff::Handoff`, `workers::Workers` | recycled `Arc<VideoFrame>`s; one-slot latest-wins handoff between threads (counts replaced items); fork-join pool without per-job allocation |
| `AudioCapture` | `start() -> FrameReceiver<AudioBuffer>`, `stop()` |
| `FrameReceiver<T>` | bounded queue that drops the oldest item; `recv().await`, `try_recv()`, `recv_timeout()` |
| `capture::default_screen_capture()`, `default_audio_capture()` | the backend for this session |
| `capture::synthetic::{SyntheticScreen, SineSource}` | test pattern (moving rectangle, frame counter) and sine tone |

```rust
let codecs = Codecs::new().with_openh264(OpenH264::find_in(&data_dir)?); // H.264 optional
let mut capture = voelin_media::capture::default_screen_capture()?;
let mut frames = capture.start(&SourceId::Monitor(0), &CaptureOptions::default()).await?;
let codec = codecs.pick_encoder(&viewer_codecs).unwrap();
let mut encoder = codecs.new_encoder(codec, EncoderConfig::default())?;
while let Some(frame) = frames.recv().await {
    for f in encoder.encode(&frame, keyframe_requested)? {
        peer.write(MediaKind::Video, MediaTime::new(f.pts_90khz, Frequency::NINETY_KHZ), f.data);
    }
}
```

## The pipeline in `voelin-core` (`media` module)

Feature `media` of voelin-core builds the module with whatever voelin-media backends
are enabled (on Android: MediaCodec and the app's capture source);
`media-desktop` adds voelin-media's default backends (libvpx, OpenH264, X11,
PipeWire). The desktop app and voelinctl use `media-desktop`, the Android app
`media`. Codecs are always chosen through `Codecs`, so each build uses its own.

```
streamer: capture thread ── FrameSink: pace, convert + scale (Pyramid on all cores)
            │ one Handoff per layer (newest frame wins)
            ├─ encoder thread, layer 0 ─┐
            ├─ encoder thread, layer 1 ─┤
            └─ ...                      ├─ MediaSink: StreamSink / EncodedSource
          AudioCapture ─ 20 ms stereo Opus (128 kbit/s) ─┘
viewer:   Engine::subscribe_frames ─ VideoPipeline (thread) ─ VideoDecoder ─ picture callback ─ Latest
          stream audio ─ session audio thread (mixer, own volume) ─ speakers
```

- `Streamer::start(&codecs, StreamerConfig)` starts capturing right away (the
  portal asks the user then), so a cancelled dialog never starts a stream;
  frames are converted and encoded only after `attach(sink)`, i.e. once the
  stream is live. Audio follows the capture clock across gaps. With the test
  pattern (`SourceId::Synthetic`, `synthetic_pattern`) the audio is a quiet
  sine tone. The portal's restore token is available afterwards
  (`restore_token()`) to store.
- Threads: the capture backend calls the streamer's `FrameSink` on its own
  thread with the frame still in its capture buffer. Frames over the
  frame-rate cap (`StreamerConfig::fps`, any value >= 1) are skipped before
  anything is read. The rest is converted to I420 once, on all cores, and
  scaled for every layer that is due (`Pyramid`, recycled frames). Each
  layer's newest frame goes through a one-slot handoff to that layer's
  encoder thread; a frame the encoder was too busy for is replaced and
  counted, never queued, so latency stays one frame.
- Simulcast: `StreamerConfig::layers` (`voelin_stream::LayerSpec`; empty:
  one layer 0 at `bitrate_kbps`) are all encoded, each with its own encoder
  at `output_size()` of the source, its own frame-rate cap (`max_fps`,
  paced by timestamps) and its share of the CPUs. Frames carry `layer` and
  `keyframe`. `MediaSink::take_layer_keyframes` is polled by the encoder
  threads; exactly the requested layers encode a keyframe, kept due until
  one is produced; a still screen sends no frames, so a request is answered
  by encoding the last picture again. Each layer follows
  `MediaSink::layer_bitrate(id)` when the sink knows it (capped by
  `max_bitrate`), else its configured bitrate; libvpx changes it in place
  without a keyframe.
- `Streamer::reconfigure(&codecs, StreamerConfigUpdate { fps, bitrate_kbps,
  codec, layers })` applies from the next frame without restarting the
  capture: encoders are created first (an error changes nothing), layers
  keep their encoder when their id stays, new ids get threads, removed ones
  stop, a new codec swaps every encoder (first frame a keyframe), and the
  frame-rate cap reaches the capture backend (X11 and the test pattern
  retime, the portal renegotiates the rate with the compositor).
- `stats()`: `StreamerStats` with capture fps, convert time and threads,
  dropped frames, codec, and per layer (`LayerStats`) size, frames,
  keyframes, dropped, fps, kbit/s, encode time, target bitrate, threads
  and encoder speed. Counters are atomics; rates are computed once a
  second, and a one-line summary is logged every 5 s at debug level
  (`RUST_LOG=voelin_core::media=debug`).
- `CaptureBackend`: `Auto` (the session default: portal on Wayland, X11,
  WGC), `Portal`, `X11`, `Wlroots`; `name()` / `FromStr` use the settings
  spellings `auto`, `portal`, `x11`, `wlroots`.
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
  without a server (the desktop app's `VOELIN_DEMO_STREAM`).
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
| MediaCodec (Android) | – | H.264, VP8, VP9 | VP8, VP9, H.264, AV1 | the device's default codec per type through the NDK (`ndk` crate); a hardware encoder factory named `mediacodec` |

On Android, `codec::mediacodec` uses byte-buffer mode on both sides, so it
takes and returns `VideoFrame`s like the software codecs. Encoders get NV12
(else I420) in the layout the codec reports (`image-data` / `stride` /
`slice-height`, parsed by `codec::image_layout`, which is tested on every
platform); H.264 is High (preferably Constrained High) without B-frames, SPS/PPS
in front of every keyframe, keyframes on request (`request-sync`), bitrate
changes at runtime (`video-bitrate`). Encoder order: the device's hardware
encoders (H.264, VP9, VP8), then Google's software VP8 and H.264. Decoders
return the newest finished picture per `decode` call (NV12 or I420).

libvpx settings follow libwebrtc's realtime setup: one pass CBR, no lag,
`VPX_DL_REALTIME`, error resilient, keyframes only at start and on request
unless `keyframe_interval` is set, screen content tuning
(`VP8E_SET_SCREEN_CONTENT_MODE`, `VP9E_SET_TUNE_CONTENT`, static threshold),
VP9 row-mt, tile columns from the thread count and width (tiles are at least
256 pixels wide) and cyclic-refresh AQ, VP8 token partitions from the thread
count. Threads: all CPUs but one (libvpx threads spin-wait on each other and
slow down many times over when every core is taken), or the streamer's share
of them per layer, at most one per 320x240 pixels. `cpu-used` adapts to the
measured encode time against the frame interval (faster at once when one frame overruns the interval or the
mean is above 75 %; slower when the mean stayed below 35 % for 15 frames,
waiting twice as long after each slower step that had to be undone): VP8
starts at −10 and moves within −10..−6 (on screen content −12 and faster
were no quicker and about 3 dB worse, −5 and slower about ten times slower
for under 1 dB), VP9 starts at 8 within 5..9; `EncoderConfig::speed` fixes
it instead. Frame durations for rate
control follow the timestamps (variable frame rate), at least 3/4 of the
nominal interval. `set_bitrate` reconfigures the running encoder
(`vpx_codec_enc_config_set`). The C API is wrapped in
`codec/vpx/raw.rs`; existing safe wrappers either encode only or cannot
change the bitrate at runtime. The other modules with `unsafe` code, each
with SAFETY comments: the X11 and wlroots shared-memory mappings
(`capture/x11/shm.rs`, `capture/wlroots.rs`), the DMA-BUF mapping and sync
ioctl (`capture/dmabuf.rs`), the lifetime-erased job of the worker pool
(`workers.rs`), the `Arc` raw pointers of the handoff (`handoff.rs`), and
the `Send` wrapper in `codec/mediacodec.rs`.

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
| Linux Wayland (`pipewire`, default) | `PortalCapture`: xdg-desktop-portal ScreenCast via `ashpd`, frames from a PipeWire video stream (LINEAR DMA-BUF or shared memory) | `PipeWireAudioCapture`: default sink monitor (`stream.capture.sink = true`) |
| Linux wlroots (`wlroots`, default) | `WlrootsCapture`: `ext-image-copy-capture-v1`, else `wlr-screencopy-unstable-v1`, outputs only, shared memory | |
| Windows | `WindowsCapture`: Windows Graphics Capture (`windows-capture`), monitors and windows | `WasapiLoopback`: process loopback excluding our own process tree, falling back to plain loopback |
| Android | `ExternalScreenCapture` fed by the app (MediaProjection → `ImageReader`, RGBA; see [android.md](android.md)) | `ExternalAudioCapture` fed by the app (`AudioPlaybackCapture`, 48 kHz float) |

`default_screen_capture()` picks a registered external provider first
(`capture::external::set_screen_provider`, which the Android app calls at
start), then the portal when `WAYLAND_DISPLAY` is set (or
`XDG_SESSION_TYPE=wayland`), X11 when `DISPLAY` is set, WGC on Windows;
`default_audio_capture()` likewise.

Notes:

- X11 needs a 24/32-bit TrueColor visual (BGRx in memory). Window capture
  reads the window itself, clipped to the screen; without a compositor, parts
  covered by other windows may be stale. MIT-SHM needs a local server with
  MIT-SHM 1.2; otherwise GetImage is used. The only `unsafe` here is mapping
  our memfd (`capture/x11/shm.rs`).
- Portal: `start` needs a tokio runtime (zbus runs on it) and may show the
  desktop's picker. Choices persist with `PersistMode::ExplicitlyRevoked`: store
  `PortalCapture::restore_token()` (e.g. in `voelin-store` settings) and pass it to
  `PortalCapture::with_restore_token` next time. The cursor is embedded when
  the portal supports it. Buffers: LINEAR DMA-BUFs are offered first (a
  mandatory modifier property we fixate; mapped once per buffer, read
  between `DMA_BUF_IOCTL_SYNC` start and end), shared memory
  (BGRx/BGRA/RGBx/RGBA) second. If reading DMA-BUFs is slower than 6 ns per
  pixel over the first 30 frames (uncached VRAM), the stream renegotiates to
  shared memory; `VOELIN_PORTAL_DMABUF=0` or `with_dmabuf(false)` skips them.
  Tiled modifiers would need a GPU import (a later step). The frame rate
  asked of the compositor is the sink's cap, renegotiated when it changes;
  compositors send frames only when the screen changes. Without a session
  bus or portal, `start` returns `Error::CaptureUnavailable`; a cancelled
  dialog returns `Error::Cancelled`.
- wlroots: `WlrootsCapture::new()` (`$WAYLAND_DISPLAY`) or `with_display`;
  sources are the outputs (`SourceId::Monitor(i)`). The compositor copies
  into shared-memory buffers allocated once per size; frames complete only
  when the screen changed (`copy_with_damage` / image-copy sessions). The
  cursor is painted by the compositor when asked. No window capture
  (`ext-foreign-toplevel-list`) and no DMA-BUF capture yet.
- X11 reads through the shared-memory segment in place (cursor blended into
  it); the monitor or window geometry is looked up once a second rather than
  per frame. x11rb still allocates a small reply buffer per request.
- PipeWire system audio captures everything the default sink plays, including
  our own playback (TeamSpeak voices): PipeWire has no "all but this process"
  monitor. Excluding our nodes needs a private null sink with links from every
  other application's output (via the registry); TODO. Until then, play voices
  on another sink or accept that viewers hear them. Without a PipeWire daemon,
  `start` returns `Error::CaptureUnavailable`.
- Windows process loopback needs Windows 10 2004 or later.

## Performance and benchmarks

Per frame, in steady state, nothing is allocated between the capture buffer
and the encoder input (verified with `voelinctl stream bench`, which counts
heap allocations with a counting global allocator): the synthetic source
renders into one buffer, conversion and scaling write into recycled frames,
handoffs are atomic pointer swaps, and libvpx output is borrowed. The one
allocation per encoded frame is the `Arc<[u8]>` of `EncodedFrame`, whose size
varies per frame and so cannot be recycled. Backends outside our control add
their own (x11rb reply buffers; the Android provider hands owned frames).

```sh
# The real pipeline on the test pattern, no server:
voelinctl stream bench --res 1920x1080 --fps 60 --codec vp8 --seconds 10
voelinctl stream bench --res 2560x1440 --fps 30 \
    --layer scale=1,bitrate=6M --layer scale=0.5,bitrate=1500k,fps=30 \
    --layer size=640x360,bitrate=400k,fps=15
# Conversion and scaling alone:
cargo bench -p voelin-media --bench convert
```

`--pattern desktop` (default) is a code-editor-like picture whose document
scrolls three pixels per frame, about as costly to encode as real screen
content; `--pattern simple` is the flat test pattern. The bench prints
capture fps, convert time and threads, per layer fps, kbit/s, keyframes,
frames dropped by the handoff, encode time, threads and `cpu-used`, CPU use
and allocations per captured and per encoded frame.

Measured with the old pipeline (single thread, allocating) and the new one,
release builds (fat LTO), VP8, desktop pattern, one layer, 8 s, on a shared
4-core VM, runs interleaved. fps sent / CPU cores / heap allocations per
encoded frame:

| | quiet (load 1-3): before | after | busy (load 6-12): before | after |
|---|---|---|---|---|
| 720p30 | 29.4 / 0.48 / 7.1 | 30.0 / 0.40 / 1.05 | 7.9 / 1.71 / 10.0 | 6.6 / 1.24 / 1.25 |
| 720p60 | 14.7 / 2.37 / 10.2 | 60.0 / 0.69 / 1.03 | 4.6 / 1.23 / 19.1 | 6.4 / 1.28 / 1.25 |
| 1080p30 | 29.6 / 0.83 / 7.1 | 30.0 / 0.75 / 1.05 | 2.7 / 1.30 / 17.4 | 6.9 / 1.27 / 1.24 |
| 1080p60 | 57.2 / 1.47 / 7.1 | 53.7 / 1.42 / 1.03 | 2.4 / 1.33 / 31.4 | 5.7 / 1.40 / 1.28 |
| 1440p30 | 8.4 / 2.26 / 9.7 | 22.7 / 1.71 / 1.07 | 1.9 / 1.31 / 22.6 | 5.5 / 1.26 / 1.30 |
| 1440p60 | 52.5 / 2.24 / 7.2 | 3.2 / 2.02 / 1.50 * | 1.5 / 1.22 / 36.9 | 3.6 / 1.30 / 1.45 |

\* libvpx with one thread per core collapsed (290 ms per frame) when the
conversion competed for a core; encoders now use all CPUs but one. The busy
column is with that change; a quiet re-run was not possible. Bytes
allocated per encoded frame fell from 5-450 MB (a full RGBA and I420 copy
per frame, more when frames queue up) to 10-130 KB (the encoded frame).
Conversion and scaling take 0.3 ms (720p) to 3-5 ms (1440p, busy) per frame
on 4 threads. With three layers (1440p30 at 6 Mbit/s, 720p30, 360p15) the
quiet machine kept every layer at its rate with 1.6-1.7 cores.

Build profiles: the dev profile builds the media hot path (yuv,
voelin-media, str0m, x11rb-protocol, pipewire, wayland-client, ...) with
`opt-level = 3`; release builds use fat LTO with one codegen unit.

## Build requirements

Linux (Debian/Ubuntu packages): `libvpx-dev` (feature `vpx`),
`libpipewire-0.3-dev libspa-0.2-dev libclang-dev` (feature `pipewire`; the
PipeWire bindings run bindgen), `pkg-config`, and `libdav1d-dev` for
`--features av1`. X11 capture is pure Rust (x11rb) and needs no packages.
Tests of X11 capture need an X server: `xvfb`, run with `xvfb-run -a cargo
test -p voelin-media` (skipped without `DISPLAY`).

Windows: libvpx is not found through pkg-config there. Install it (e.g.
`vcpkg install libvpx:x64-windows`) and set `VPX_LIB_DIR`, `VPX_INCLUDE_DIR`
and `VPX_VERSION`, or build without the `vpx` feature. The Windows capture
code is type-checked on Linux (`cargo clippy -p voelin-media --target
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
| `wayland-client`, `wayland-protocols`, `wayland-protocols-wlr` | MIT | wlroots capture |
| `criterion` | Apache-2.0 OR MIT | benchmarks only (dev-dependency) |
| `ashpd`, `pipewire`, `libspa` / libpipewire | MIT | libpipewire is dynamic |
| `windows-capture`, `wasapi`, `windows` | MIT (`windows`: MIT OR Apache-2.0) | Windows only |
| `bzip2` / `libbz2-rs-sys` | MIT OR Apache-2.0 / bzip2-1.0.6 | unpacks the OpenH264 download; `bzip2-1.0.6` is allowed in `deny.toml` |

## Verification status

| What | How | Status |
|---|---|---|
| Frame validation, YUV ↔ RGB (values, NV12, odd sizes, strides) | unit tests | tested |
| VP8 / VP9 encode → decode (PSNR > 30 dB, rectangle colour, forced keyframes, bitrate change, resolution change) | unit tests, `tests/codec_roundtrip.rs` | tested |
| H.264 with Cisco's 2.6.0 library (PSNR, Constrained High `profile_idc` 100, Baseline 66) | `tests/openh264.rs` with `VOELIN_OPENH264_LIB` | tested locally, skipped without the library |
| OpenH264 download, SHA-256 check, reuse | `tests/openh264.rs -- --ignored` | tested locally (network) |
| H.264 library missing / unknown file | unit + integration tests | tested |
| AV1 decoder construction, garbage input | unit test (`--features av1`) | tested; no AV1 stream decoded (no encoder available) |
| X11 capture (MIT-SHM and GetImage), sources, window capture → VP8 → decode, cursor | `tests/x11_capture.rs` under Xvfb | tested |
| wlroots capture (wlr-screencopy v3): outputs, pixels, a change arriving as a new frame, queue API | `tests/wlroots_capture.rs` against headless sway 1.9 (`VOELIN_WLROOTS_TEST_DISPLAY`) | tested; the ext-image-copy-capture path is untested (sway 1.9 predates it) |
| Converter (strides, unpadded last row, odd sizes, RGBA/I420/NV12), scaler (flat, area average, half), pyramid (sharing, recycling, steady-state pools) | unit tests | tested |
| Simulcast layers (sizes, fps caps, per-layer keyframes), reconfigure (codec, layers, fps) | `voelin-core` `media::tests::simulcast_layers_and_reconfigure` | tested |
| Portal DMA-BUF negotiation | unit test of the offered formats | compiles and formats parse; no compositor with the portal here |
| Portal / PipeWire error paths (no bus, bus without portal, no daemon) | unit tests, manual probe | tested |
| Portal capture (shared memory and DMA-BUF), PipeWire video and audio streams | – | compiles only (no portal or PipeWire daemon here) |
| Windows Graphics Capture, WASAPI | – | type-checked for `x86_64-pc-windows-gnu` only |
| Hardware encoders | – | stubs |
| Test pattern → VP8 → decoder, rectangle position and colour | `voelin-core` `media::tests::local_preview_decodes_the_pattern` | tested |
| Test pattern → two engine stream tasks → str0m peers on loopback → decoder | `voelin-core` `stream::tests::test_pattern_through_stream_tasks` | tested |
| Stream audio (RTP time → jitter buffer ids, volume, end) | `voelin-core` `audio::tests::stream_audio_with_volume`, stream task test | tested |
| The same through the TeamSpeak 6 server | `voelin-core/tests/media_live.rs` (`VOELIN_LIVE=1`), `voelinctl stream start --synthetic` / `watch --expect-frames` | tested against 6.0.0-beta13.1 |
| X11 monitor capture → VP8 → server → decoder | `voelinctl stream start --source x11` under Xvfb | tested manually (debug build: about 8 fps at 1400×900) |
| Desktop app watching and sharing | Xvfb, `VOELIN_AUTOWATCH` / `VOELIN_AUTOSHARE` against voelinctl, screenshots | tested manually |
| External capture providers (start, refusal, end of capture, default selection) | unit tests | tested |
| MediaCodec buffer layouts (I420, NV12 with padding, `MediaImage2` NV21, crop) | unit tests (`codec::image_layout`) | tested |
| MediaCodec encoders and decoders | – | compiles for `aarch64-linux-android` only (no device or emulator here) |
