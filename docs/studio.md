# Stream Studio (`voelin_media::studio`, `voelin_core::studio`)

The Stream Studio composites scenes of sources (screens, windows, cameras,
images, text, colours) into one picture that becomes the video of our stream,
and keeps or forwards what the stream's encoders make: a recording, a replay
buffer to save clips from after the fact, and WHIP or RTMP to a broadcast
service. It
is the engine behind the Studio screen of the UI (scenes and sources, a live
preview, LIVE / timer, the audio mixer, Record Clip, Go Live / End Stream,
Share Window / Share Screen, a camera with background blur); this page is
about the engine and the API the UI drives. The screen itself is described
in [ui.md](ui.md#the-stream-studio).

```
sources (a thread each) ─ Feed (latest wins) ┐
  screen / window / portal: capture backends  │
  camera: PipeWire (+ background filter)      ├─ compose thread, at the output rate:
  image / text / colour: drawn once           │  Compositor → pooled RGBA canvas
                                              ┘     │            └─ preview tap (downscaled, own rate)
                                                    └─ StudioCapture: a ScreenCapture backend
                                                         │
Streamer::start_studio: convert → layers → encoders ─────┼─ MediaSink (the live stream, when attached)
                        audio sources → mixer → Opus ────┘
                             │ every packet of the stream codec, attached or not
                             └─ Studio::write_packet ─ replay buffer, recording, WHIP and RTMP outputs
```

Everything is changeable while it runs, and nothing is capped: any number of
scenes, sources and outputs, and a replay window limited only by its setting.

## Scenes (`studio::scene`)

`Scenes` is the whole studio as one serde value, the `studio.scenes`
setting: the scenes, which one is live (`active`), and the output `width`,
`height` (made even) and `fps`. Every field has a default, so settings
written by an older version load. A `Scene` is an id, a name, a background
colour and its `sources`, drawn first to last (0 is furthest back). A
`Source` has an id (unique in its scene), a name, a kind, a `transform`
(position, a box or a scale, `fit`: `contain` letterboxes, `cover` crops,
`stretch`), a `crop` in source pixels, `opacity`, `visible`, `locked` (for
the UI) and a `background` (replacement, below).

| Kind (`"kind"`) | Fields | Input |
|---|---|---|
| `screen` | `monitor`, `backend` (`portal`, `wlroots`, `x11`, `windows`; none: this session's), `cursor` | a capture backend, frames borrowed and copied into pooled frames |
| `window` | `handle` (X11 id, `HWND`), `backend`, `cursor` | the same |
| `portal` | `restore_token`, `cursor` | the ScreenCast portal's dialog (Wayland); the token is written back so it is asked once |
| `camera` | `device` (a `camera::Camera::id`; empty: the first), `size`, `fps`, `mirror` | see [Cameras](#cameras) |
| `image` | `path` | PNG or JPEG, alpha kept, read once |
| `text` | `text`, `font` (none: the bundled Inter), `size_px`, `colour`, `backdrop`, `align`, `padding` | rasterised once with `ab_glyph` |
| `colour` | `colour`, `size` | drawn once |
| `pattern` | `size` | the synthetic test pattern |

```json
{
  "width": 1920, "height": 1080, "fps": 60, "active": 1,
  "scenes": [{
    "id": 1, "name": "Live", "background": {"r": 18, "g": 18, "b": 24},
    "sources": [
      {"id": 1, "kind": "screen", "monitor": 0, "transform": {"width": 1920, "height": 1080, "fit": "cover"}},
      {"id": 2, "kind": "camera", "device": "/dev/video0", "mirror": true,
       "transform": {"x": 1500, "y": 780, "width": 384, "height": 216},
       "background": {"mode": "blur", "strength": 0.05}},
      {"id": 3, "kind": "text", "text": "LIVE", "size_px": 40,
       "backdrop": {"r": 200, "g": 30, "b": 30}, "padding": 8, "transform": {"x": 40, "y": 1000}}
    ]
  }]
}
```

A source whose input fails (a camera that is gone, an unreadable image)
draws nothing and reports its error in the stats; it never stops the studio.
Changing a source restarts its input only when the input itself changed
(`SourceKind::same_input`): a drag, a crop, an opacity or a camera's
mirroring is a new plan for the compositor, not a new capture; a new text
or colour is drawn again; a portal token written back is not a reason to
ask the user again. Another background effect (or blur strength, image,
colour) starts the input again: its filter is set up with the effect.

## Compositing (`studio::compose`)

One compose thread ticks at the output rate. It takes every source's newest
picture from its `Feed` (a one-slot latest-wins handoff; a source that has
nothing new keeps its last picture, so a 30 fps camera does not blink in a
60 fps composite) and draws the live scene back to front into an RGBA canvas
from a `FramePool`, in bands of output rows on a `Workers` pool (all CPUs).
Scaling is the separable fixed-point filter of `scale.rs` (area averaging
down, bilinear up) with an inner loop for interleaved pixels, and a straight
copy at 1:1; images and text are alpha-blended, opacity applies to any
source, cameras can be mirrored, BGRA sources are swizzled on the way.

The plan (rectangles and filter weights) is rebuilt only when the scene, a
source's size or the output size changes, so a steady composite allocates
nothing (`tests/studio_alloc.rs` counts it). The canvas goes to the
streamer's ordinary conversion (one SIMD pass to I420 for the encoders, all
layers derived from it), so the encoders, simulcast and peers see an
ordinary screen source.

## Cameras

`camera::list()` reads V4L2 directly (`/dev/video*`: the card name, every
pixel format and every resolution the driver reports, no hard-coded list);
only three read-only enumeration ioctls are written by hand. Capture goes
through PipeWire, which owns the devices on a current desktop: from the
user's daemon, or in a sandbox from the XDG Camera portal (`ashpd`), both on
the same PipeWire thread as screen capture. Raw formats (YUYV, UYVY, NV12,
I420, packed RGB) are converted into pooled frames; MJPEG is not offered, so
a camera that has both gives its raw format (often at a lower size at the
highest rates). `camera::SYNTHETIC` is always listed: the test pattern, for
tests and machines without a camera. `voelinctl studio cameras` prints the
list.

## Background replacement (`studio::segment`)

A source's `background` is `keep`, `blur` (`strength`: radius as a fraction
of the frame height), `image` (`path`, scaled to cover) or `colour`.
Anything but `keep` runs a `BackgroundFilter` on the source's own thread: the
segmenter gets a copy reduced to 256 pixels wide on a thread of its own and
makes a mask at 10 fps (a mask a few frames old is still right, and
segmentation costs far more than a frame); the frame path takes the newest
mask latest-wins, samples it up (nearest; the edge is only as soft as the
mask's own) and mixes the subject over the backdrop. The
blur is a three-pass box blur (close to a Gaussian), separable, with a
running sum so its cost does not grow with the radius, in bands on a worker
pool over pooled buffers.

**What segments today is not a person.** `segment::Ellipse`, the segmenter
that is always there, is a centred oval about where a person sits in front
of a camera. A person segmentation model (MediaPipe selfie segmentation,
Apache-2.0, through `tract-onnx`) is meant to sit behind the `Segmenter`
trait; it is not in this build, because the model could not be fetched where
this was built (see [Status](#status)). Dropping it in changes nothing on the
frame path.

## Outputs (`studio::output`)

Every packet of the stream codec goes to `Studio::write_packet` as it is
encoded: the video of one simulcast layer (`Studio::set_layer`, default 0;
a codec only a viewer chose is never teed) and the Opus audio. An output is
an `OutputSink` (`wants(track)`, `needs_keyframe()`, `write(packet)`,
`finish()`, `bytes()`); none of them may block an encoder. An output that
fails is removed and reported, and the rest go on.

- **Recording** (`record::Recorder`): WebM (VP8, VP9, AV1, Opus) or
  Matroska (also H.264, H.265), from the file name; a codec WebM cannot hold
  moves the file to Matroska (and it keeps its name). The container is our
  own EBML writer (`ebml`): a header, the tracks, clusters of SimpleBlocks,
  segment size unknown and no cues, so a recording cut short by a crash still
  plays; the duration is written back at the end. H.264 is stored
  length-prefixed with its parameter sets in `CodecPrivate`. A recording
  starts at the next keyframe (it asks for one) and keeps the last two
  seconds of audio while it waits, writing what belongs after the keyframe:
  the encoders are behind the audio, so the audio of the keyframe's moment
  has already gone by when it arrives.
- **Replay buffer** (`replay::ReplayBuffer`): the last `studio.replay_seconds`
  of packets, from a keyframe; past `studio.replay_memory_mb` the oldest go to
  a temporary file that is dropped again once they age out. While it is on it
  asks for a keyframe every two seconds (the encoders make keyframes only on
  request), so a clip is at most two seconds longer than the window.
  `SaveClip` writes it through a recorder, without re-encoding, and keeps the
  buffer.
- **WHIP** (`whip::Whip`, feature `whip`): `POST` the offer (with a bearer
  token), apply the answer, `DELETE` the `Location` on stop. The session
  (str0m, on a thread of its own, fed through a bounded queue) offers exactly
  the codec the studio encodes and Opus. One host candidate on the interface
  that reaches the service: no STUN or TURN yet. It asks for a keyframe when
  it starts, when ICE comes up, when the service sends a PLI and after its
  queue overflowed.
- **RTMP and RTMPS** (`rtmp::Rtmp`, feature `ffmpeg`): through libavformat
  loaded at runtime, never linked ([media.md](media.md#rtmp-output)). Its
  `rtmp://` and `rtmps://` protocols do the handshake, `connect`, `publish`
  and TLS and take an FLV byte stream; the FLV is ours (`flv`: `onMetaData`,
  AVC and AAC sequence headers, then one tag per frame), as the recordings'
  Matroska is, so no libavformat struct is touched. Video is the studio's
  H.264 of the output's layer as encoded; another stream codec is refused
  with the reason (classic RTMP carries H.264; Enhanced RTMP's HEVC, AV1,
  VP9 and Opus are not written). Audio is the studio's Opus decoded and
  encoded as AAC-LC (160 kbit/s, 48 kHz stereo) by FFmpeg's own `opus`
  decoder and `aac` encoder, on the output's thread. The stream key is the
  last part of the URL's path (`rtmp://host/app/key`) or the output's token
  (sent as the play path, the URL's path as the application); the output's
  name never shows it. The first connection is made when the output is
  added, so a wrong address or key fails there; a connection that fails
  later (the service restarting, the network) is made again after 1, 2, 4,
  ... up to 30 seconds for as long as the output exists, each new one
  starting at a keyframe it asks for, while the stats show why it is down.
  A thread of its own runs the connection behind a bounded queue (about two
  seconds): a slow upload drops packets rather than holding up an encoder,
  and video resumes at the next keyframe. Audio goes out in time order with
  the video (it waits for the video of its time, at most a second), and the
  audio of the first keyframe's moment, which comes before the keyframe,
  is kept as a recording keeps it. `EndStream` unpublishes and closes; a
  network that does not answer is cut off after three seconds.

Recording and the replay buffer need no live stream: a studio's streamer
encodes from the start (see below).

## The controller (`studio::Studio`)

`Studio::start(scenes)` (on a Tokio runtime) starts composing at once, so
the preview has a picture before anything is live. The UI drives it with
`Studio::apply(Command)` and follows `Studio::events()` (a broadcast
channel; late subscribers miss what came before).

| Command | |
|---|---|
| `SetScenes(Box<Scenes>)` | replace the whole graph (a scene file, the setting) |
| `AddScene { name }` / `RemoveScene { scene }` / `RenameScene { scene, name }` | scenes; `SceneAdded` carries the new id |
| `SetActiveScene { scene }` | switch what is composited, live |
| `SetOutput { width, height, fps }` | the composite's size and rate |
| `AddSource { scene, name, kind }` / `RemoveSource { scene, source }` | sources; `SourceAdded` carries the new id |
| `UpdateSource { scene, source, change: Box<SourceChange> }` | name, kind, transform, crop, opacity, visible, locked, background (`None` leaves a field alone) |
| `ReorderSource { scene, source, to }` | z-order (0 is furthest back) |
| `SetPreview { width, height, fps }` | the preview tap's size and rate (default 480x270 at 15 fps) |
| `StartRecording { path }` / `StopRecording` | `.webm` or `.mkv` |
| `SetReplay { seconds, memory_mb }` / `SaveClip { path }` | the replay buffer (0 seconds: off) |
| `AddOutput(OutputSpec::Record { path } \| Url { url, token })` / `RemoveOutput { id }` | more recordings, WHIP (`http(s)://`, `token`: the bearer token), RTMP (`rtmp(s)://`, `token`: the stream key unless the URL ends with it) |
| `GoLive` / `EndStream` | the studio's state for the header (LIVE, timer); `EndStream` also closes the outputs that push (WHIP and RTMP push from when they are added). The TeamSpeak stream itself is started and attached as usual |

| Event | |
|---|---|
| `State(Status)` | anything in the header changed: `state` (Idle, Live), `live_for`, `recording`, `recorded_for`, `active_scene`, `output`, `fps`, `outputs` |
| `Stats(Box<Stats>)` | once a second: compose time, composed fps, per source (size, fps, frames the composite never used, error), per output (bytes, kbit/s, error), the replay buffer, preview frames, frames handed to the streamer |
| `Scenes(Arc<Scenes>)` | the graph changed; to show and to persist |
| `SceneAdded { scene }` / `SourceAdded { scene, source }` | new ids, to select them |
| `RecordingStarted { path }` / `RecordingStopped { path, duration, bytes }` | |
| `ClipSaved { path, duration }` | |
| `OutputAdded { id, name }` / `OutputRemoved { id, name }` | also when an output failed |
| `Error { context, message }` | something failed; the studio goes on |

Also: `scenes()`, `status()`, `cameras()`, `epoch()` (the clock of the
composite's timestamps), `set_layer(layer)`, `needs_keyframe(layer)`.

**Preview tap**: `Studio::preview()` is an `Arc<Handoff<VideoFrame>>`; the
UI takes the newest downscaled RGBA frame (`take()`, or `wait_timeout()` on
a thread of its own) at the size and rate of `SetPreview`. It is latest-wins:
a UI that is slow just skips frames and never holds up the composite.
`convert::to_rgba` (or the frame's RGBA plane directly) fills a Slint
`SharedPixelBuffer`.

## In the engine (`voelin_core::studio`, feature `media`)

- `voelin_core::studio` re-exports the studio. `studio::start(&settings)`
  starts it from `studio.scenes` and the replay settings and keeps them in
  step both ways: every scene edit is written back to `studio.scenes`, and a
  change of `studio.scenes`, `studio.replay_seconds` or
  `studio.replay_memory_mb` made anywhere else (a settings page, `--set`)
  is applied to the running studio. Equal values are not written back, so
  neither side echoes the other. `studio::recording_dir(&settings)` is where
  recordings and clips go.
- `media::Streamer::start_studio(&codecs, config, studio)` makes the
  composite the stream's video (`SourceId::Studio`): the same conversion,
  simulcast layers, encoders, audio mixer (the Studio screen's mixer is the
  streamer's: `audio_mixer()`, `audio_sources()`) and keyframe handling as
  any share. Unlike other sources it is encoded from the start, attached or
  not, and its audio clock starts at the studio's epoch, so recordings have
  their audio in sync with the picture. Going live is unchanged:
  `Command::StartStream`, then `attach(sink)` on `StreamState::Live`;
  `detach()` (or the stream ending) leaves the studio recording.

| Setting | Default | |
|---|---|---|
| `studio.scenes` | no scenes, 1920x1080 at 30 fps | the scene graph (JSON as above); validated (unique ids, sane transforms) |
| `studio.replay_seconds` | 30 | replay window, 0: off, no maximum |
| `studio.replay_memory_mb` | 256 | MiB kept in memory before the oldest packets go to a temporary file |
| `studio.recording_dir` | empty: `Videos/Voelin` | where recordings and clips go |

## voelinctl

```sh
# A scene file for 10 s: preview picture, recording, replay clip, a scene
# switch halfway (live, while recording), a test tone as the audio.
voelinctl studio run scenes.json --seconds 10 --switch-to 2 \
  --preview preview.png --record rec.webm --clip clip.mkv --replay 30 --tone 440 --codec vp8
# Push to a WHIP endpoint (token also from VOELIN_WHIP_TOKEN).
voelinctl studio run scenes.json --seconds 60 --whip https://ingest.example/whip --token ...
# Push over RTMP(S) (H.264; the key in the URL or in --token).
voelinctl studio run scenes.json --seconds 60 --codec h264 --tone 440 \
  --rtmp rtmps://live.example/app --token <stream key>
# The compositor alone: compose time and heap allocations per frame.
voelinctl studio bench --sources 4 --res 1920x1080 --fps 60
voelinctl studio cameras
```

## Performance

`voelinctl studio bench` (debug build; voelin-media is built at opt-level 3
in dev, so the compositor runs optimised), 32 threads, every source
delivering a new picture every frame, 1920x1080 at 60 fps:

| Sources | Compose (mean / p95) | Heap allocations per frame |
|---|---|---|
| 1 (a 3840x2160 screen scaled down) | 2.67 / 2.86 ms | 0 |
| 4 (that, a 1280x720 camera cropped and mirrored, a 256x256 image with alpha, text) | 3.10 / 3.42 ms | 0 |
| 16 (the four, four times) | 11.0 / 12.1 ms | 0 |

The 4K-to-1080p scale dominates; the scaler's inner loops are plain integer
code the compiler vectorises only in part, the place to look first if the
compositor ever needs to be faster. The preview tap (1920x1080 to 480x270,
two threads) takes about 3 ms at its own rate (15 fps by default).

## Status

| What | How | Status |
|---|---|---|
| Scene serde (defaults, round trip, invalid graphs refused) | `scene::tests`, `voelin-core` `studio::tests` | tested |
| Compositor: positions, alpha and opacity, scale, crop, clip, fit, BGRA, mirror, a source keeping its last picture, replanning | `compose::tests` | tested |
| No allocation per composed frame (and after a size change) | `tests/studio_alloc.rs`, `voelinctl studio bench` | tested: 0 |
| Text (bundled Inter), images with alpha, colour, test pattern sources | `source::tests` | tested |
| Controller: commands, events, preview tap, capture backend | `studio::tests` | tested |
| Recording and clips read back by ffprobe and decoded (VP8 + Opus; H.264 + Opus through a hardware encoder by hand) | `tests/studio_record.rs`, `voelin-core/tests/studio.rs`, `voelinctl studio run` | tested |
| Replay buffer: starts at a keyframe, keeps its window, spills to disk and back, codec change, keyframe requests, late keyframes keep their audio | `replay::tests` | tested |
| A studio stream recorded while its scene switches live, with a replay clip; audio and video start together | `voelin-core/tests/studio.rs` (decoded: red before the switch, blue after) | tested |
| Settings ⇄ studio in both directions | `voelin-core` `studio::tests` | tested |
| WHIP: offer, answer, ICE, DTLS, VP8 and Opus arriving, refusal with the service's reason, `DELETE` on stop | `tests/studio_whip.rs` against a WHIP server built on str0m in the test (`--features whip`) | tested; no public WHIP service tried |
| Real camera through the user's PipeWire (Fifine K420, 1280x720) | `camera::tests::a_real_camera_delivers` (`--ignored`) | tested on one webcam |
| Camera through the XDG Camera portal (sandboxes) | – | compiles only |
| Screen, window and portal sources | the existing capture backends ([media.md](media.md#capture)) | as tested there; not run inside the studio here |
| Background blur, image and colour backdrops on the oval mask | `segment::tests` | tested; **no person segmentation model** (see above) |
| Windows and Android cameras | – | not implemented: `camera::list()` has only the test pattern there |
| RTMP: the FLV tags (sizes, times past 24 bits, AVC from Annex B, `onMetaData` in AMF0), URL and key handling (the name hides the key), codecs other than H.264 and dead servers refused | `flv::tests`, `rtmp::tests` | tested |
| RTMP: write errors seen (`AVIOContext.error`, found at load), an abort ends a stuck connect, the sample format field of the AAC encoder, Opus → AAC (frames back to back from the packets' clock, priming, a gap restarts the clock) | `ffmpeg::avio::tests`, `ffmpeg::audio::tests`, `ffmpeg::tests` | tested with FFmpeg 9.0.1 (libavformat 63: `error` at 84, `sample_fmt` at 348) and FFmpeg 4.4.8 (58: 120 and 408) |
| RTMP end to end: a studio stream (H.264 through the hardware encoder, a 440 Hz tone) pushed to FFmpeg's own RTMP server (`ffmpeg -listen 1`) with the key apart from the URL; the server killed and another started on its port; `EndStream` | `voelin-core/tests/studio.rs` `a_studio_stream_goes_out_over_rtmp_and_comes_back_after_the_server_did` | tested: ffprobe reads `h264` 320x180 and `aac` 48000 Hz stereo in both servers' files, each starting at a keyframe, every picture decodes (red), the audio decodes to 440 Hz; the key arrived as the stream name; the second server finished its file and exited by itself. No public service (Twitch, YouTube) tried; RTMPS only through FFmpeg's TLS, untried here |
