# Media: frames, codecs and capture (`voelin-media`)

`crates/voelin-media` turns screens and what applications play into frames,
mixes the stream's audio, encodes and decodes video, and converts pixels for
rendering. It does no networking:
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
| `Codecs` | `new()` (probes FFmpeg / MediaCodec once per process), `builtin()` (compiled-in codecs only), `with_openh264(lib)`, `with_preference(EncoderPreference)` / `set_preference`, `decoders()` (viewer order), `encoders()` / `encoder_codecs()` (streamer order under the preference), `encoders_for(&pref)`, `new_decoder(codec)`, `new_encoder(codec, config)`, `new_encoder_preferring(codec, config, &pref)`, `new_encoder_with(codec, backend, config)`, `pick_encoder(&accepted)`, `is_hardware(backend)`, `report()`; cheap to clone |
| `EncoderPreference { hardware, backend: BackendChoice }` | settings `stream.hardware_acceleration` and `stream.encoder_backend` (`auto`, `software`, or a backend name such as `h264_vaapi`, `libx264`, `libvpx`, `openh264`) |
| `EncoderReport { ffmpeg, zero_copy, encoders: Vec<EncoderInfo> }` | for the UI: FFmpeg's release and path (or why none), whether DMA-BUF import works, and per backend `name`, `api`, `codec`, `hardware`, `status` (self-test result or why it cannot be used) and `rank` under the current preference |
| `EncoderBackend` | `Libvpx`, `OpenH264`, `Ffmpeg("h264_vaapi")`, `Hardware("mediacodec")`; `name()` is the settings spelling |
| `ffmpeg::{Ffmpeg, probe, FfmpegEncoder, BACKENDS}` | FFmpeg loaded at runtime (feature `ffmpeg`), see [FFmpeg encoders](#ffmpeg-encoders-loaded-at-runtime) |
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
| `mix::{StreamMixer, MixerHandle, SourceHandle, SourceInput, BlockClock}` | the stream's audio mixer: any number of sources, each with gain, mute, level meters and any number of inputs (see [Stream audio](#stream-audio)) |
| `capture::playback::{start_playback, audio_apps, window_pid}` | what applications play, captured into a mixer source (`PlaybackFilter::AllButSelf` or `App(AppMatch::Name / Pid)`); the applications that play, updated live (`AudioApps`) |

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
`media-desktop` adds voelin-media's default backends (libvpx, OpenH264, FFmpeg, X11,
PipeWire). The desktop app and voelinctl use `media-desktop`, the Android app
`media`. Codecs are always chosen through `Codecs`, so each build uses its own.

```
streamer: capture thread ── FrameSink: pace, convert + scale (Pyramid on all cores)
            │ one Handoff per layer (newest frame wins)
            ├─ encoder thread, layer 0 ─┐
            ├─ encoder thread, layer 1 ─┤
            └─ ...                      ├─ MediaSink: StreamSink / EncodedSource
          audio sources ─ StreamMixer (thread, every 20 ms) ─ stereo Opus (128 kbit/s) ─┘
viewer:   Engine::subscribe_frames ─ VideoPipeline (thread) ─ VideoDecoder ─ picture callback ─ Latest
          stream audio ─ session audio thread (mixer, own volume) ─ speakers
```

- `Streamer::start(&codecs, StreamerConfig)` starts capturing right away (the
  portal asks the user then), so a cancelled dialog never starts a stream;
  frames are converted and encoded only after `attach(sink)`, i.e. once the
  stream is live. The audio is mixed from `StreamerConfig::audio_sources`
  (see [Stream audio](#stream-audio)); without any, it is desktop audio
  without Voelin, or a quiet sine tone with the test pattern
  (`SourceId::Synthetic`, `synthetic_pattern`). The portal's restore token
  is available afterwards (`restore_token()`) to store.
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
  codec, encoder, layers, audio_sources })` applies from the next frame without restarting the
  capture: encoders are created first (an error changes nothing), layers
  keep their encoder when their id stays, new ids get threads, removed ones
  stop, a new codec or encoder preference (`encoder`: hardware on/off, another
  backend) swaps every encoder (first frame a keyframe), and the
  frame-rate cap reaches the capture backend (X11 and the test pattern
  retime, the portal renegotiates the rate with the compositor).
- One encoder per codec viewers chose: the offer lists several codecs (see
  `peer_config` below), the streamer's peer reports the codec each answer
  chose (`PeerEvent::VideoCodec`), and the session sends a video frame only
  to viewers of its codec (`StreamerSession::write_frame_in`) and tells the
  encoders which codecs are wanted (`LayerFeedback::set_codecs`,
  `MediaSink::video_codecs`). Each layer's encoder thread keeps the stream
  codec's encoder and makes one more per other wanted codec when a viewer
  needs it (with the streamer's current `Codecs` and preference), drops it
  when none does, and skips the stream codec while nobody takes it; frames
  go out through `MediaSink::send_video(frame, codec)`. Sinks that do not
  tell codecs apart (`EncodedSource`, a `FrameSource`) get the stream codec
  only, so their offers must list it alone (`voelinctl stream start` does).
- `stats()`: `StreamerStats` with capture fps, convert time and threads,
  dropped frames, codec, and per layer (`LayerStats`) size, frames,
  keyframes, dropped, fps, kbit/s, encode time, target bitrate, threads,
  encoder speed, the stream codec's `backend` and the `codecs` encoded; the audio mix's level and limiter gain, and per audio
  source (`AudioSourceStats`) level, state, latency, underruns and why it
  captures nothing, if so. Counters are atomics; rates are computed once a
  second, and a one-line summary is logged every 5 s at debug level
  (`RUST_LOG=voelin_core::media=debug`).
- `CaptureBackend`: `Auto` (the session default: portal on Wayland, X11,
  WGC), `Portal`, `X11`, `Wlroots`; `name()` / `FromStr` use the settings
  spellings `auto`, `portal`, `x11`, `wlroots`.
- `peer_config(&codecs, config)` makes a viewer accept only what we decode
  and a streamer offer its stream codec (the first of `config.video_codecs`
  it can encode) followed by `offer_codecs`: the codecs of hardware
  encoders, VP8 through libvpx (every TeamSpeak client decodes it), and HEVC
  last. Other software encoders (x264, SVT-AV1, libvpx VP9, ...) are only
  ever the stream codec, so a viewer's answer never starts an expensive
  second encoder. Answers keep the offer's order, so viewers take the stream
  codec when they decode it. `preferred_codec(&codecs, configured)` picks the
  stream codec: `stream.codec` if encodable, else the preference's first
  (hardware first when enabled, VP8 in software; never HEVC);
  `encoder_preference(&settings)` and `configured_codec(&settings)` read the
  settings. H.264 is offered as Constrained High at the level the stream
  needs (`PeerConfig::set_h264_format(profile, width, height, fps, bitrate)`:
  macroblocks per second and per frame, and bitrate, per H.264 Table A-1;
  never below 3.1, the level offered before).
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

## Stream audio

The audio of our stream is a mix of any number of sources
(`StreamerConfig::audio_sources`: `AudioSourceSpec { kind, gain, muted }`):

| `AudioSourceKind` | Captures |
|---|---|
| `DesktopWithoutSelf` (default) | everything that plays except Voelin: viewers do not hear the voices or streams we play |
| `App(AppMatch::Name(..))` | one application by name, ignoring case: application name or executable (`.exe` optional), the package on Android; found again when it restarts |
| `App(AppMatch::Pid(..))` | one process and its children (Linux, Windows) |
| `WindowAudio` | the application of the shared window: X11 `_NET_WM_PID`, the owner of a Windows `HWND`. The ScreenCast portal does not say whose window it shares; on Wayland, pick the application |
| `Microphone` | our microphone as the voice connection sends it (after echo cancellation, noise suppression and gain control), through `voelin_audio::tap::microphone()`, while a voice connection runs |
| `Synthetic { hz }` | a quiet sine tone (tests, and the default with the test pattern) |

- Mixer (`voelin_media::mix::StreamMixer`): each source has inputs,
  lock-free single-producer rings that capture threads fill at their own
  pace, rate and channel count. Each input is held at a small latency
  (40 ms by default, more for an input that delivers in bursts or keeps
  arriving late). An input that runs dry fades out and buffers again
  while the others go on. Clock drift is absorbed by resampling up to
  0.5 % while a buffer is off target. The sum passes a master gain and a
  soft limiter, so it never exceeds full scale. Levels (peak and RMS) of
  every source and of the output are atomics. Nothing is allocated per
  block (`tests/mix_alloc.rs`; `benches/mix.rs` measures it).
- The streamer's audio thread mixes one 20 ms block whenever
  `BlockClock` says it is due (monotonic clock, exact, no drift) and
  encodes it with Opus in music mode. The block's number is its RTP time,
  so the audio follows real time across silences and stalls; after a
  stall of more than 25 blocks it jumps ahead instead of sending a burst.
  It also mixes before a sink is attached, so the inputs keep draining.
- `StreamerConfigUpdate::audio_sources` changes the sources live.
  Sources whose kind stays keep their capture and take the new gain and
  mute; `Some(vec![])` leaves a silent track. A source that cannot
  capture (an application that is not running, no PipeWire, a window
  whose process is unknown) does not fail the stream: its
  `AudioSourceStats::error` and `Streamer::audio_error()` say why.
  `Streamer::audio_mixer()` hands out the `MixerHandle` for meters.
- The settings key `stream.audio_sources` stores the sources as JSON, e.g.
  `[{"kind": "desktop"}, {"kind": "app", "name": "firefox", "gain": 0.5},
  {"kind": "microphone", "muted": true}]`. Other kinds are `"window"` and
  `"synthetic"` (`frequency`), and an app can be given by `"pid"`. The
  default is desktop audio; `[]` shares without sound. The desktop app
  reads it when sharing starts (`media::audio_source_specs`).
  `media::audio_apps()` lists the applications for a picker.

What applications play is captured by `capture::playback::start_playback`:

| Platform | All but ours | One application | Application list |
|---|---|---|---|
| Linux (PipeWire) | `pipewire_links::LinkManager`: a capture stream of ours that the session manager leaves unlinked (`node.autoconnect = false`) gets links from the output ports of every other playback stream (`Stream/Output/Audio`); PipeWire sums them. Links follow streams as they come and go | links from that application's streams only | playback streams in the registry, one entry per application, live |
| Windows | WASAPI process loopback excluding our process tree (before Windows 10 2004: plain loopback of the default device, which includes our playback) | process loopback including the process tree; by name, one capture per matching process, matched again every 2 s | processes with audio sessions on any playback device, polled every 2 s |
| Android | `AudioPlaybackCapture` excluding our uid | matching the package's uid | launchable apps (Android cannot tell which play) |

On Linux, our own streams are recognised by the process id of the node, of
its client, or of the client's socket (`pipewire.sec.pid`, compared with our
own connection's, which also works in a sandbox), walked up the `/proc`
parent chain. The playing sides of loopbacks and filters (`node.link-group`)
are skipped: what they replay is captured at its source. This needs no
session-manager policy and no extra sink, and leaves the user's routing and
volumes alone. On Android, a capture needs the running screen capture and
the microphone permission, and apps can opt out
(`allowAudioPlaybackCapture`, or usages other than media, game and unknown,
e.g. calls).

## Codecs

Preference: a viewer accepts VP9 > VP8 > AV1 > H.264; a streamer encodes with
hardware (H.264 > AV1 > VP9 > VP8 > HEVC) > VP8 (libvpx) > H.264 (x264 and
OpenH264 through FFmpeg, then Cisco's OpenH264) > VP9 (libvpx, expensive in
software) > AV1 (SVT-AV1, libaom through FFmpeg). `Codecs` filters both lists
by what is compiled in, loaded and passed its self-test, and applies the
`EncoderPreference`: `auto` (hardware first unless
`stream.hardware_acceleration` is off), `software` (no hardware), or a named
backend first for the codecs it encodes (even hardware with acceleration
off), the automatic order after it.

| Backend | Feature | Encode | Decode | Library |
|---|---|---|---|---|
| libvpx | `vpx` (default) | VP8, VP9 | VP8, VP9 | system libvpx (1.14 tested), dynamically linked; bindings from `libvpx-native-sys` (pre-generated, no bindgen) |
| OpenH264 | `openh264` (default) | H.264 Constrained High / Baseline | H.264 | Cisco's prebuilt binary, loaded at runtime |
| dav1d | `av1` | – | AV1 | system libdav1d >= 1.3 (1.4.1 tested) |
| FFmpeg | `ffmpeg` (default; off on Android) | H.264, HEVC, AV1, VP9, VP8 in hardware; H.264 (x264, OpenH264), AV1 (SVT-AV1, libaom, rav1e) in software | – | the system's (or `VOELIN_FFMPEG_DIR`'s) libavcodec / libavutil, any major version, loaded at runtime; see below |
| MediaCodec (Android) | – | H.264, VP8, VP9 | VP8, VP9, H.264, AV1 | the device's default codec per type through the NDK (`ndk` crate); an encoder factory named `mediacodec` |

HEVC (`Codec::H265`): str0m packetizes and depacketizes H.265, so it can be
negotiated; it is offered last and only when a hardware encoder has it, i.e.
used only by a peer that takes none of the other codecs. There is no HEVC
decoder, so viewers never accept it.

### FFmpeg encoders, loaded at runtime

`crate::ffmpeg` (feature `ffmpeg`) uses whatever FFmpeg is installed; it is
never linked or shipped. Without it, or when a backend fails its self-test,
the software encoders above are used as before.

- Finding it: `VOELIN_FFMPEG_DIR` (only that directory; its libavutil and
  libswresample are loaded first so libavcodec's dependencies resolve
  there), else the system: `libavcodec.so.N` for N from 80 down to 54 and
  `libavcodec.so`, then whatever `ldconfig -p` lists (Linux);
  `avcodec-N.dll` next to the executable or on `PATH` (Windows);
  `libavcodec.N.dylib` on the library path, Homebrew (`/opt/homebrew/lib`,
  `/usr/local/lib`) and MacPorts (`/opt/local/lib`) (macOS). No version is
  refused. libavutil's functions come from the libavutil libavcodec itself
  loaded (through its handle; on Windows the already loaded DLL), so the two
  always match; libavformat of the same major is opened if present (for RTMP
  output later). `VOELIN_FFMPEG=0` disables FFmpeg. Only functions are
  looked up by name (`sys.rs`); FFmpeg's warnings and errors go to
  `tracing` (target `ffmpeg`) and into the self-test's failure reasons.
- ABI across major versions: every codec setting goes through AVOptions
  (`av_opt_set*`: `video_size`, `pixel_format`, `time_base`, `b`,
  `maxrate`, `bufsize`, `g`, `bf`, `threads`, colour properties, `profile`
  and each encoder's private options), frames and packets are allocated by
  FFmpeg, and of `AVFrame` / `AVPacket` only the leading fields are read or
  written (`data[8]`, `linesize[8]`, `width`, `height`, `format`; `pts`,
  `data`, `size`, `flags`), unchanged since FFmpeg 0.9 / 2.1. Fields without
  an accessor are found and checked at runtime (`layout.rs`):
  `AVFrame.pict_type` and `pts` (after `format`, with or without
  `key_frame`, which libavutil 60 removed; a fresh frame must read NONE,
  {0, 1}, `AV_NOPTS_VALUE` in exactly one layout), `AVCodecContext.hw_frames_ctx`
  (for VA-API; located from the offsets `av_opt_find` reports for its
  neighbours `hwaccel_flags` and `err_detect` / `extra_hw_frames` (libavcodec
  61+) or `max_pixels` (up to 60), and NULL in a new context),
  `AVHWFramesContext` `format`, `sw_format`, `width`, `height`,
  `initial_pool_size` (with or without `internal`, which libavutil 59
  removed; `device_ctx` and both formats checked in a new context), and for
  the DMA-BUF import `AVFrame.buf[0]` and `hw_frames_ctx` (a table per
  libavutil major, 56-61 on 64-bit; `buf[0]` checked on a real frame at
  load, `hw_frames_ctx` on the first surface). The layouts were compiled
  from the headers of FFmpeg 4.4, 5.1, 6.1, 7.1, 8.0 and 9.0. A major that
  is not in the table turns the import off instead of reusing the newest
  entry: `AVFrame` has shrunk as well as grown between majors (424 bytes in
  libavutil 61 against 536 in 56), so an offset guessed for an unknown
  layout could point past the end of the struct. A failed check disables
  only what needs the field (VA-API, or the import) and says why
  (`LibraryInfo::hw_frames`, `dmabuf_import`, the report).
- Backends (`ffmpeg::BACKENDS`, FFmpeg's encoder names, also the names in
  `stream.encoder_backend`): H.264 `h264_nvenc`, `h264_amf`, `h264_vaapi`,
  `h264_qsv`, `h264_videotoolbox`, `h264_mf`, software `libx264`,
  `libopenh264`; HEVC `hevc_nvenc`, `hevc_amf`, `hevc_vaapi`, `hevc_qsv`,
  `hevc_videotoolbox`, `hevc_mf`; AV1 `av1_nvenc`, `av1_amf`, `av1_vaapi`,
  `av1_qsv`, software `libsvtav1`, `librav1e`, `libaom-av1`; VP9
  `vp9_vaapi`, `vp9_qsv`; VP8 `vp8_vaapi`. `ffmpeg::probe()` runs each one
  present in the build once per process (in parallel): one 320x240 frame,
  forced keyframe, flushed; a backend that gives no keyframe packet is
  skipped with FFmpeg's reason ("Cannot load libcuda.so.1", "no DRM render
  node", "not in this FFmpeg build", ...). rav1e holds about 20 frames
  before its first packet even in low-latency mode (measured, rav1e 0.7), so
  it is used only when named.
- Probe cost: a family whose vendor is not in the machine is not opened at
  all. Its driver would still run its initialisation before failing, which
  was the bulk of the startup cost (measured here: `av1_nvenc` 1.7 s,
  `vp9_qsv` 1.2 s), and the probe takes as long as its slowest test because
  they run in parallel. The PCI vendors of the DRM render nodes
  (`/sys/class/drm/*/device/vendor`) are read once; NVENC needs NVIDIA (or
  `/dev/nvidiactl`, for a driver without a DRM node), Quick Sync Intel, AMF
  AMD. When the vendors cannot be read nothing is skipped. On the AMD
  machine below this took `voelinctl stream encoders` from 1.77 s to
  0.93-1.12 s, testing 12 backends instead of 20, and the report says "no
  NVIDIA GPU in this machine" instead of a CUDA error code.
- Third-party output: FFmpeg's own log goes to `tracing`, but the libraries
  behind the encoders write to standard error themselves — SVT-AV1 prints a
  build banner and its allocation totals, AMD's AMF runtime prints
  `GetProperty(...) not found` warnings, and Mesa prints a `RADV_PERFTEST`
  deprecation notice when AMF brings up Vulkan. That was about thirty lines
  on every start. While the self-tests run, standard error is pointed at a
  temporary file and what landed there comes back as one `debug` record;
  `VOELIN_FFMPEG_PROBE_STDERR=1` leaves it alone.
- Realtime settings: no B-frames, no lookahead, keyframes only at the start
  and on request (a GOP of 2^30 frames, or what each wrapper takes as
  unlimited: 65535 for Quick Sync, 0 for AMF and VideoToolbox, 2^29 for
  rav1e; `keyframe_interval` if set) with forced IDR (`pict_type` I,
  `forced-idr`), CBR (`maxrate` = `b`, a one-second buffer; SVT-AV1 `rc=2`),
  time base 1/fps (hardware rate control budgets per frame from it; pts are
  frame numbers from the capture timestamps), BT.601 limited range. Per
  family: x264 `veryfast` (`EncoderConfig::speed` picks another preset),
  `zerolatency`, no scene cuts; NVENC `p2` (`llhp` on old releases), tune
  `ull`, `zerolatency`, `delay 0`; Quick Sync `veryfast`, `async_depth 1`,
  `low_delay_brc`, scenario display remoting; AMF usage `ultralowlatency`,
  quality `speed`, SPS/PPS with every IDR; VA-API `rc_mode CBR`,
  `async_depth 1`, `low_power` retried when the normal entry point fails;
  VideoToolbox `realtime`, `prio_speed`; Media Foundation hardware MFT,
  scenario display remoting; SVT-AV1 preset 10, `pred-struct=1` (low
  delay), no lookahead or scene detection; libaom `usage realtime`,
  `cpu-used 8`, `lag-in-frames 0`, `row-mt`, screen tuning. H.264 is High
  without B-frames (Constrained High, what TeamSpeak decodes) or Constrained
  Baseline (`EncoderConfig::h264_profile`). Options a release does not know
  are skipped; alternatives are tried in order.
- H.264 level: the level the signalling offers
  (`voelin_stream::h264::level_idc`, ITU-T Table A-1) is computed again from
  the session's own size, rate and bitrate and set on both the codec context
  and the wrapper's private option. Without it every wrapper picked its own,
  and a generous one: measured on the GPU below, `h264_amf` wrote level 4.2
  into the SPS of a 720p30 stream whose offer said 3.1, and 5.0 for a
  1440p60 stream offered as 5.1. A viewer that sizes its decoder from our
  SDP may refuse a stream that declares more than the offer did, which is
  how a stream fails against an official client. With the level set,
  `h264_vaapi`, `h264_amf` and `libx264` all emit exactly the offered level
  (`tests/ffmpeg.rs`, `sps_carries_the_offered_profile_and_level`, parses
  the SPS). The level table is a second copy of voelin-stream's, because
  neither crate can depend on the other; a unit test pins the two together.
  The constraint-flag byte still differs and is left alone: the offer claims
  Constrained High (`0c`) and Constrained Baseline (`e0`) as WebRTC
  implementations conventionally do, `h264_vaapi` and `h264_amf` emit `0c`
  for the former and `libx264` emits `00`, and all three emit `40` or `c0`
  for the latter. Those bits do not change what a decoder can decode.
- Runtime changes: x264, NVENC and Quick Sync take bitrate changes on the
  running encoder (FFmpeg's wrappers compare `b` / `maxrate` / `bufsize`
  before each frame); the others reopen at the next keyframe, at once
  when the target falls below half (congestion), and when it rose by half
  or more and the session is 5 s old (the estimate ramping up after the
  start would otherwise never reach the encoder). A new frame rate or size
  opens a new session (keyframe); the old one is flushed first, so frames an
  encoder with a delay still held come out.
- Buffers: per session one software `AVFrame` (`av_frame_get_buffer`,
  written after `av_frame_make_writable`, NV12 interleaved where the backend
  wants it) and one `AVPacket`, reused; packets are handed out borrowed
  (`encode_with`). VA-API frames come from a surface pool
  (`av_hwframe_ctx_init`, surfaces allocated on demand and recycled) filled
  with `av_hwframe_transfer_data`; the device is `VOELIN_VAAPI_DEVICE` or the
  first `/dev/dri/renderD*`, shared by all encoders. Odd sizes are cropped to
  even ones. `encode_dmabuf` takes a buffer with one NV12 layer of two
  planes, which is how a compositor hands one over; a driver exporting its
  own surface may instead describe it as two layers (`R8` for luma, `GR88`
  for chroma) over the same object, and that form is not accepted yet. The streamer's steady state is 1.1 heap allocations per encoded
  frame with x264 or SVT-AV1, as with libvpx (`voelinctl stream bench`).
- Zero-copy (Linux): `FfmpegEncoder::encode_dmabuf(&DmaBufRef, ...)` maps a
  DRM PRIME frame onto a VA-API surface (`av_hwframe_map`) that the encoder
  reads without a copy, for NV12 buffers. Screen capture delivers RGB
  (BGRx), which needs a colour conversion on the GPU first (VA-API video
  processing); that step is not implemented, so RGB buffers are refused and
  mapped for the CPU path as before. The capture side has the hook
  (`FrameSink::accepts_dmabuf` / `dmabuf`: the portal offers a buffer before
  mapping it and would pass tiled ones through), but no sink accepts yet
  and the portal still negotiates LINEAR buffers only; offering tiled
  modifiers needs the encoder's import modifiers. Untested on a GPU.

Installing FFmpeg (runtime libraries only, no `-dev` packages):

| OS | |
|---|---|
| Debian / Ubuntu | `apt install libavcodec60` (Ubuntu 24.04; Debian 12: `libavcodec59`); VA-API drivers: `intel-media-va-driver` / `mesa-va-drivers`; NVENC needs the NVIDIA driver (`libcuda.so.1`); x264 comes with Ubuntu's libavcodec |
| Fedora | `dnf install ffmpeg-libs` from RPM Fusion (Fedora's own `ffmpeg-free` lacks x264 and some hardware encoders) |
| Arch | `pacman -S ffmpeg` |
| Windows | a shared build (e.g. gyan.dev or BtbN "shared"): put `avcodec-*.dll`, `avutil-*.dll` and their neighbours next to the executable, on `PATH`, or in `VOELIN_FFMPEG_DIR` |
| macOS | `brew install ffmpeg` (VideoToolbox is always there) |

`voelinctl stream encoders` lists what was found, every backend's self-test
result and the order the streamer uses them in.

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
(`workers.rs`), the `Arc` raw pointers of the handoff (`handoff.rs`), the
`Send` wrapper in `codec/mediacodec.rs`, and the FFmpeg bindings
(`ffmpeg/sys.rs`, `layout.rs`, `mod.rs`, `encoder.rs`).

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

| Platform | Screen | System audio (`AudioCapture`; streams use [Stream audio](#stream-audio)) |
|---|---|---|
| any | `SyntheticScreen` | `SineSource` |
| Linux X11 (`x11`, default) | `X11Capture`: MIT-SHM 1.2 (memfd passed to the server) or GetImage; RandR 1.5 monitors; windows from `_NET_CLIENT_LIST` (root children without a WM); XFixes cursor blended in | |
| Linux Wayland (`pipewire`, default) | `PortalCapture`: xdg-desktop-portal ScreenCast via `ashpd`, frames from a PipeWire video stream (LINEAR DMA-BUF or shared memory) | `PipeWireAudioCapture`: default sink monitor (`stream.capture.sink = true`) |
| Linux wlroots (`wlroots`, default) | `WlrootsCapture`: `ext-image-copy-capture-v1`, else `wlr-screencopy-unstable-v1`, outputs only, shared memory | |
| Windows | `WindowsCapture`: Windows Graphics Capture (`windows-capture`), monitors and windows | `WasapiLoopback`: process loopback excluding our own process tree, falling back to plain loopback |
| Android | `ExternalScreenCapture` fed by the app (MediaProjection → `ImageReader`, RGBA; see [android.md](android.md)) | `ExternalAudioCapture` fed by the app (`AudioPlaybackCapture` of every app but ours, 48 kHz float) |

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
- `PipeWireAudioCapture` (the plain `AudioCapture`) captures everything the
  default sink plays, including our own playback. Streams do not use it: their
  desktop audio links every other application's playback into a capture
  stream of ours ([Stream audio](#stream-audio)). Without a PipeWire daemon,
  both return `Error::CaptureUnavailable`.
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
# Any encoder (see `voelinctl stream encoders`); --no-hardware for software:
voelinctl stream bench --res 1280x720 --encoder libx264
voelinctl stream bench --res 1280x720 --encoder h264_vaapi
voelinctl stream bench --res 2560x1440 --fps 30 \
    --layer scale=1,bitrate=6M --layer scale=0.5,bitrate=1500k,fps=30 \
    --layer size=640x360,bitrate=400k,fps=15
# A real screen capture instead of the pattern (the portal asks the user;
# --start-timeout bounds the wait so it cannot hang):
voelinctl stream bench --source portal --fps 60 --encoder h264_vaapi
voelinctl stream bench --source monitor:0 --fps 60      # X11
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

### Hardware encoders, measured

Release build, AMD Radeon RX 7900 GRE (radeonsi / RADV, `renderD128`), 32
cores, FFmpeg 9.0.1, desktop pattern, one layer, 10 s after a 2 s warm-up,
`voelinctl stream bench`. "cpu" is cores of the whole process (capture,
conversion and encode together), "encode" the wall time of one
`encode_with`.

1080p60, 6000 kbit/s asked for:

| encoder | fps | kbit/s | encode ms | cpu cores | allocations per frame |
|---|---|---|---|---|---|
| `h264_vaapi` | 60.0 | 5951 | 2.52 | 0.25 | 1.02 |
| `h264_amf` | 60.0 | 5910 | 2.17 | 0.31 | 1.02 |
| `av1_vaapi` | 60.0 | 6211 | 2.23 | 0.20 | 1.02 |
| `av1_amf` | 60.0 | 5962 | 2.75 | 0.29 | 1.02 |
| `libx264` | 60.0 | 5674 | 2.33 | 1.16 | 1.02 |
| libvpx VP8 | 60.0 | 6031 | 4.93 | 1.69 | 1.02 |

1440p60, 10000 kbit/s asked for:

| encoder | fps | kbit/s | encode ms | cpu cores | allocations per frame |
|---|---|---|---|---|---|
| `h264_vaapi` | 60.0 | 10112 | 3.96 | 0.33 | 1.02 |
| `h264_amf` | 60.0 | 10188 | 2.80 | 0.41 | 1.02 |
| `av1_vaapi` | 60.0 | 10794 | 3.56 | 0.34 | 1.02 |
| `av1_amf` | 60.0 | 10079 | 2.91 | 0.42 | 1.02 |
| `libx264` | 59.9 | 9406 | 11.88 | 2.17 | 1.03 |
| libvpx VP8 | 60.0 | 10038 | 4.32 | 2.55 | 1.02 |

Simulcast, 1440p30 source, three layers (1440p30 at 8 Mbit/s, 720p30 at
2 Mbit/s, 360p15 at 600 kbit/s), same run:

| encoder | layer 0 | layer 1 | layer 2 | cpu cores |
|---|---|---|---|---|
| `h264_vaapi` | 30.0 fps, 7666 kbit/s, 4.37 ms | 30.0, 2121, 1.71 ms | 15.0, 596, 1.22 ms | 0.25 |
| `h264_amf` | 30.0 fps, 7499 kbit/s, 3.56 ms | 30.0, 2015, 2.02 ms | 15.0, 598, 1.94 ms | 0.32 |
| `libx264` | 30.0 fps, 7428 kbit/s, 4.40 ms | 30.0, 1993, 2.19 ms | 15.0, 593, 2.51 ms | 1.33 |

What the numbers say: every encoder, hardware and software, holds the full
frame rate at both sizes on this machine, so the difference is CPU, not
throughput. Hardware costs 0.20-0.42 cores against 1.16-2.55 for software,
a factor of four to eight, and the gap widens with size — at 1440p60 x264
needs 2.17 cores and is the only encoder that dropped a frame. Every backend
honoured the target bitrate within 2 % except `av1_vaapi`, which overshoots
by 4-8 %. The steady state is 1.02 allocations per encoded frame (the
`Arc<[u8]>` of `EncodedFrame`) with hardware as with software: no reopen
storm, no per-frame allocation in the import or upload path. Conversion and
scaling cost 0.16-0.30 ms per frame at these sizes and are not the
bottleneck. The per-layer "threads" the bench prints is the CPU budget the
layer was given, not what a hardware encoder uses — it encodes on the GPU.

### Wayland screen capture, measured

Same machine, a 2560x1440 Wayland desktop through the ScreenCast portal,
`h264_vaapi` at 60 fps and 12 Mbit/s, `voelinctl stream bench --source
portal`. This path had never been run against a real compositor.

| buffers | capture fps | convert + scale | cpu cores | dropped |
|---|---|---|---|---|
| DMA-BUF (LINEAR), as first written | – | – | – | every frame |
| DMA-BUF (LINEAR), mapping fixed | 60.0 | 11.95 ms | 19.54 | 0 |
| shared memory | 59.9 | 0.27 ms | 0.41 | 0 |
| what it does now (switches by itself) | 60.1 | 0.30 ms | 0.55 | 0 |

The first row is the bug behind the low frame rates on Wayland: the mapping
length was taken from `mapoffset + maxsize`, which describe a mapping that
only shared memory has, while a DMA-BUF's size lives in its buffer object and
this compositor leaves both fields 0. Every map was refused and nothing was
ever delivered.

The second row is why shared memory is the right choice on a discrete GPU:
the CPU reads its memory uncached, so the same conversion costs 44 times the
time and 48 times the CPU. `SLOW_DMABUF_NS_PER_PIXEL` exists for exactly
this, but at 6.0 ns per pixel it never fired against the DMA-BUF path's
3.24; it now sits between the two measurements. Zero-copy is the real answer
for this hardware — the buffer would never be read by the CPU at all — but
that needs the GPU colour conversion that is still missing.

Build profiles: the dev profile builds the media hot path (yuv,
voelin-media, str0m, x11rb-protocol, pipewire, wayland-client, ...) with
`opt-level = 3`; release builds use fat LTO with one codegen unit.

## Build requirements

Linux (Debian/Ubuntu packages): `libvpx-dev` (feature `vpx`),
`libpipewire-0.3-dev libspa-0.2-dev libclang-dev` (feature `pipewire`; the
PipeWire bindings run bindgen), `pkg-config`, and `libdav1d-dev` for
`--features av1`. X11 capture is pure Rust (x11rb) and needs no packages.
Tests of X11 capture need an X server: `xvfb`, run with `xvfb-run -a cargo
test -p voelin-media` (skipped without `DISPLAY`). FFmpeg needs nothing at
build time (feature `ffmpeg` only adds `libloading`); its tests run with the
runtime libraries installed (`libavcodec60` on Ubuntu 24.04) and are
skipped without them; `VOELIN_OPENH264_LIB` lets them decode x264's output
with Cisco's OpenH264, `--features av1` AV1 with dav1d.

Windows: libvpx is not found through pkg-config there. Install it (e.g.
`vcpkg install libvpx:x64-windows`) and set `VPX_LIB_DIR`, `VPX_INCLUDE_DIR`
and `VPX_VERSION`, or build without the `vpx` feature. The Windows capture
code is type-checked on Linux (`cargo clippy -p voelin-media --target
x86_64-pc-windows-gnu --no-default-features --features openh264,ffmpeg`) but
has not run on Windows yet.

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
| FFmpeg (libavcodec, libavutil, libavformat) | LGPL-2.1+ (GPL-2+ in builds with x264 and other GPL parts) | the user's installed libraries, loaded at runtime; never linked or shipped |
| `libloading` | ISC | opens FFmpeg (and OpenH264, through the `openh264` crate) |
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
| Portal capture (shared memory and DMA-BUF), PipeWire video and audio streams | `voelinctl stream bench --source portal` on a 2560x1440 Wayland desktop | tested: it delivered no frames at all (the DMA-BUF mapping length), and the DMA-BUF path cost 19.5 cores where shared memory costs 0.41. Both fixed; 1440p60 now runs on 0.55 cores (see the table above). Portal audio is still untested |
| Windows Graphics Capture, WASAPI | – | type-checked for `x86_64-pc-windows-gnu` only |
| FFmpeg loader: sonames, missing FFmpeg, layout checks on a real release | `ffmpeg::sys` / `ffmpeg` unit tests; mirrors compared with offsets compiled from the 4.4-9.0 headers | tested (FFmpeg 6.1.1 on Ubuntu 24.04; FFmpeg 9.0.1 / libavutil 61 / libavcodec 63 on Arch, every offset compared with that release's own headers) |
| FFmpeg software encoders → our decoders: x264 → OpenH264, SVT-AV1 / rav1e / libaom → dav1d (PSNR > 28 dB, keyframes at start and on request, timestamps, bitrate change, size change, odd sizes, Constrained High / Baseline) | `tests/ffmpeg.rs` (`VOELIN_OPENH264_LIB`, `--features av1`) | tested |
| Self-test failures (NVENC without CUDA, Quick Sync without a session, VA-API without a render node, encoders not in the build) | `tests/ffmpeg.rs`, `voelinctl stream encoders` | tested: each fails alone with FFmpeg's reason |
| Encoder preference (auto, software, named, hardware off), report ranks | `codec::tests::encoder_preference` | tested |
| An encoder per codec viewers chose (made, dropped, stream codec skipped, preference change) | `voelin-core` `media::tests::an_encoder_per_codec_viewers_chose` | tested |
| Answer's codec reported to the streamer, H.264 level in the offer, HEVC offered | `voelin-stream` `peer::tests::streamer_learns_the_answered_codec`, `h264::tests` | tested (str0m on loopback) |
| x264 / SVT-AV1 / libaom in the streamer pipeline | `voelinctl stream bench --encoder ...` | 720p30: x264 8.7 ms per frame, SVT-AV1 1.3 ms, libaom 37 ms; 1.1 allocations per encoded frame |
| Hardware encoders VA-API and AMF (H.264, HEVC, AV1) in the streamer pipeline | `voelinctl stream bench` on a Radeon RX 7900 GRE, 1080p60 / 1440p60 / simulcast | tested: full frame rate, 0.20-0.42 cores against 1.16-2.55 for software, 1.02 allocations per encoded frame (see the table above) |
| Hardware encoders NVENC, Quick Sync, Media Foundation, VideoToolbox | – | not tested (no such hardware here); NVENC and Quick Sync are skipped by the vendor check and the Windows code type-checks for `x86_64-pc-windows-gnu` |
| H.264 SPS against the offered `profile-level-id` (`profile_idc`, `level_idc`) for every usable backend, two sizes, both profiles | `tests/ffmpeg.rs` `sps_carries_the_offered_profile_and_level` | tested: it found `h264_amf` emitting level 4.2 where the offer said 3.1; with the level now set, all backends emit the offered level |
| The encoder's H.264 level table against the signalling's | `ffmpeg::encoder::tests::levels_agree_with_the_signalling` | tested (8 sizes x 5 rates x 4 bitrates x 2 profiles) |
| Zero-copy DMA-BUF import into a VA-API encoder | `ffmpeg::encoder::tests::a_dmabuf_really_reaches_a_vaapi_encoder` (a VA-API surface exported as DRM PRIME and fed back in) | tested: imports and encodes, with an AMD **tiling** modifier (`0x200000028a01f04`), not only LINEAR |
| Zero-copy from screen capture | – | not reached: the import works (row above), but the portal delivers RGB and the GPU colour conversion to NV12 is missing, so no captured frame has gone through it. `Ingest` also never returns true from `FrameSink::accepts_dmabuf`, so the portal's DMA-BUF offer is never taken |
| Offer [VP9, VP8], a viewer that decodes only VP8 → its own VP8 encoder, VP9 idle, pictures decoded | `voelin-core/tests/media_live.rs` `ts6_viewer_gets_the_codec_it_chose` (`VOELIN_LIVE=1`) | tested against the TeamSpeak 6 dev server (our client on both ends) |
| Several codecs against official TeamSpeak viewers | – | not tested (no official client here) |
| Test pattern → VP8 → decoder, rectangle position and colour | `voelin-core` `media::tests::local_preview_decodes_the_pattern` | tested |
| Test pattern → two engine stream tasks → str0m peers on loopback → decoder | `voelin-core` `stream::tests::test_pattern_through_stream_tasks` | tested |
| Stream audio (RTP time → jitter buffer ids, volume, end) | `voelin-core` `audio::tests::stream_audio_with_volume`, stream task test | tested |
| Stream mixer (latency, fade on underrun, drift correction, rate conversion, limiter, levels, live source changes), `BlockClock` | `voelin-media` `mix::tests`, `tests/mix_alloc.rs` (no allocation per block) | tested |
| Application audio on PipeWire: desktop without our own stream, by name, by pid, a new player linked live, a quit one dropped, a restarted one matched again, the app list | `voelin-media/tests/pipewire_apps.rs` (private PipeWire, WirePlumber and D-Bus; tones told apart by frequency) | tested with PipeWire 1.0 and WirePlumber 0.4; skipped without them |
| Streamer audio sources (mix levels, gain, mute, live change, microphone tap, window source error, silence) | `voelin-core` `media::tests::audio_sources_change_live`, `audio_sources_from_settings`, `settings::tests::audio_sources` | tested |
| X11 `_NET_WM_PID` of the shared window | `tests/x11_capture.rs` under Xvfb | tested |
| WASAPI per-process loopback, audio sessions, `HWND` owner | – | type-checked for `x86_64-pc-windows-gnu` only |
| Android per-app and all-but-ours playback capture | `cargo ndk -t arm64-v8a clippy`, `gradlew compileDebugKotlin` | compiles only (no device or emulator here) |
| The same through the TeamSpeak 6 server | `voelin-core/tests/media_live.rs` (`VOELIN_LIVE=1`), `voelinctl stream start --synthetic` / `watch --expect-frames` | tested against 6.0.0-beta13.1 |
| X11 monitor capture → VP8 → server → decoder | `voelinctl stream start --source x11` under Xvfb | tested manually (debug build: about 8 fps at 1400×900) |
| Desktop app watching and sharing | Xvfb, `VOELIN_AUTOWATCH` / `VOELIN_AUTOSHARE` against voelinctl, screenshots | tested manually |
| External capture providers (start, refusal, end of capture, default selection) | unit tests | tested |
| MediaCodec buffer layouts (I420, NV12 with padding, `MediaImage2` NV21, crop) | unit tests (`codec::image_layout`) | tested |
| MediaCodec encoders and decoders | – | compiles for `aarch64-linux-android` only (no device or emulator here) |
