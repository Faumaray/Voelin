# Why watched streams played at a low frame rate

Watching a TeamSpeak 6 stream (someone's screen share in the viewer) showed
fewer frames than were sent, with freezes until the next keyframe. This
note records what was measured, what changed, and what is left.

## Measurements

On a 4-core 2.1 GHz VM (one core busy with another build for part of the
runs), with the app's own code: a 2560x1440 stream at 60 fps and about
25 Mbit/s, encoded by `VpxEncoder` from synthetic content (moving gradients,
a scrolling text-like area), fed to the viewer's decode loop in real time
with the same queue rules as `decode_loop`.

| Stream (VP8 token partitions) | Decode per picture | Shown | Freezes in 10 s |
|---|---|---|---|
| 4 or 8 (a sender with 6 cores or more) | 18-21 ms | 35-49 of 60 fps | 2-3 queue drops, 34-167 frames skipped waiting for a keyframe |
| 1 or 2 | 10.0-10.5 ms | 60 of 60 fps | none |

FFmpeg's command line on the same streams: `vp8` on 1 thread 95-101 fps, on
4 slice threads (what the viewer used) 54-60 fps; libvpx on 4 threads 23 and
11 fps on the 4- and 8-partition streams, on 2 threads 143 and 129. On clean
1080p test clips, 4 partitions decode at 290 fps on one thread and 47-74 fps
on 3-4 slice threads.

Handing a picture to the window (`video.rs`, a new zeroed `SharedPixelBuffer`
and the I420 to RGBA conversion, on the decode thread):

| Size | New buffer + convert | Reused buffer + convert |
|---|---|---|
| 1920x1080 | 3.0-3.6 ms | 0.6-0.8 ms |
| 2560x1440 | 5.1 ms | 1.0 ms |
| 3840x2160 | 12.4-12.8 ms | 2.4-2.5 ms |

## Causes and what changed

1. **FFmpeg's VP8 decoder on slice threads** (the main cause). Every software
   decoder got all cores (up to 8) as slice threads. VP8's slice threads work
   on the frame's token partitions, and Voelin's VP8 senders use 4 (6-8
   cores) or 8 (9 or more) partitions, so the threads wait on each other and
   one 1440p picture took 18-21 ms: the decoder could not keep 60 fps, fell
   30 frames behind, dropped its queue and skipped everything until the next
   keyframe. FFmpeg's `vp8` comes before libvpx in the ladder, and Windows
   (and AMD under VA-API) have no VP8 hardware decoder, so most viewers took
   this path. **Now** VP8 decodes on one thread (`ffmpeg::decoder::decoder_threads`).
2. **The picture's conversion on the decode thread.** Every decoded picture
   was converted to RGBA on the decode thread, into a new zeroed buffer, even
   when the window would only show the newest. **Now** the decoder hands its
   newest frame to a converter thread (`video.rs`, `deliver`), which converts
   only what can be shown, into buffers reused three pictures later (a buffer
   the window still holds copies itself first).
3. **libvpx's decoder threads** spin waiting for each other and collapse when
   another core is busy (the fallback without FFmpeg, e.g. Windows without its
   FFmpeg libraries). **Now** at most 2 threads (`codec/vpx.rs`).
4. **dav1d** with a frame delay of one is slower on 3-4 threads (155-166 fps)
   than on one (211 fps) on this 4-core machine, while 8 threads on a
   32-core one gave 412 fps at 1440p. **Now** a quarter of the cores, 1 to 4
   (`codec::dav1d_threads`, for our dav1d and FFmpeg's `libdav1d`).

## Left as they are

- **The sender.** The software VP8 encoder took 18-29 ms per 1440p frame on
  this machine, so a similar sender tops out at 35-55 fps; frames beyond that
  are dropped before encoding. The automatic bitrate is 0.12 bits per pixel
  per frame (27 Mbit/s at 1440p60); about 0.07 would suit screen content and
  decode faster. Showing the sender's encode rate next to the viewer's decode
  rate would tell the two apart.
- **H.264 with one slice per frame** (the official client's AMF streams):
  slice threads give nothing (about 180 fps either way); frame threads give
  336-423 fps but add a frame of delay per thread, and the decoder self-test
  expects a picture from every frame at once.
- **The window's texture upload.** The image has no cache key, so the
  renderer uploads a new full-size texture per frame (not measured, no GPU
  here).
- **Lossy links.** str0m waits up to 30 complete frames for a lost packet
  before the frame is handed over as non-contiguous, and VP8/VP9/AV1 then
  wait for a keyframe (asked for at most every 500 ms).

The measuring harnesses were throwaway programs around `voelin-media`'s
`VpxEncoder` and decoders and FFmpeg's command line; they are not part of the
repository.
