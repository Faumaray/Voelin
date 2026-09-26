# Audio pipeline

```
microphone ─ cpal ─ resample 48 kHz mono ─ Processor (AEC3, NS, AGC2) ─ 20 ms frames
          ─ Vad / push-to-talk / continuous ─ Opus ─ voice connection

voice connection ─ Mixer (jitter buffer, Opus decode, per-client volume/mute)
          ─ output volume ─┬─ resample to device ─ cpal ─ speakers
                           └─ Processor::render (echo canceller reference)
```

The audio thread lives in `voelin-core/src/audio.rs`; the building blocks are in
`voelin-audio`. All user settings are one serde struct, `voelin_audio::AudioSettings`.

## Echo cancellation, noise suppression, gain control

[sonora](https://crates.io/crates/sonora) 0.2 (BSD-3-Clause), a pure Rust port of
the WebRTC audio processing module: AEC3, the WebRTC noise suppressor, AGC2
(adaptive digital gain + limiter) and a high-pass filter. Chosen over:

- `webrtc-audio-processing`: binds the C++ library, either a system package found
  through pkg-config or a bundled C++ build (autotools, bindgen/libclang); its
  licence is not an SPDX expression, and Windows/Android need extra toolchain work;
- `nnnoiseless` (RNNoise port): noise suppression only, no echo cancellation.

sonora needs no C/C++ toolchain, so it builds the same everywhere. It runs on 10 ms
frames; `Processor` frames arbitrary lengths internally. The dev profile compiles
the sonora crates with `opt-level = 3`: unoptimised, AEC alone takes a third of a
core; optimised, the full chain takes about 0.3 ms per 10 ms frame.

Measured with the unit tests (synthetic signals):

| | Result |
|---|---|
| Echo (far end delayed 20–120 ms, -6 dB) | about 25–33 dB attenuation within the first second, 50 dB after two; the delay estimate matches |
| Double talk (near-end tone during echo) | near-end tone loses about 3 dB |
| White noise, suppression `High` | 10 dB or more lower; a tone in 200 ms bursts keeps its level within 3 dB |

A steady tone is treated as stationary noise and suppressed, as expected from
this kind of noise suppressor, so the test uses bursts like syllables.

## Voice activity

`Vad`: a level gate in dBFS with hangover (default -40 dBFS, 300 ms). The optional
`Speech` mode also requires the RNN speech probability of WebRTC's AGC2 VAD
(`sonora-agc2`), which rejects noise loud enough to pass the gate. The RNN also
fires on pure tones, and on synthetic vowels only at low pitch, so the plain gate
stays the default until it is checked with real voices. With voice activation the
two 20 ms frames before the gate opened are sent too, so onsets are not cut.

## Jitter buffer and clock drift

`Mixer` wraps the vendored `tsclientlib::audio::AudioHandler` (one queue per
talker) and adds per-client volume and mute. The handler absorbs sender clock
drift: an underrun plays Opus loss concealment for the missing frame (one inserted
frame, and the talker is dropped after three in a row), and when the smallest
queue length over the last 255 packets exceeds its spread, it drops every 100th
sample (1 % faster) until the surplus is gone; queues above 0.5 s are truncated.

`crates/voelin-audio/tests/drift.rs` simulates one hour of continuous speech with
20–60 ms network jitter, reordering and 0.5 % loss, with the sender clock at
+200 and -200 ppm (720 ms of disagreement per hour). Marker frames every 30 s
measure the end-to-end latency:

| Sender clock | Latency min / max | First 10 min avg | Last 10 min avg |
|---|---|---|---|
| +200 ppm | 73 / 131 ms | 112 ms | 125 ms |
| -200 ppm | 67 / 95 ms | 82 ms | 84 ms |

## Settings, levels and volumes

The engine keeps one `AudioSettings` for all sessions (`Command::SetAudioSettings`;
each new audio thread starts with it) and applies changes live. The desktop app
stores them under the setting `audio` and shows them on its settings page:
devices, transmit mode (push-to-talk, voice activation, continuous), the voice
activation threshold, echo cancellation, noise suppression and its level,
automatic gain, microphone gain, output volume and the playback buffer.

The audio thread reports the loudest 10 ms level since the last report about
ten times a second (`Event::InputLevel`, with whether voice is sent), which the
settings page shows as a meter with the threshold. Without a voice connection,
`Command::TestMicrophone` opens the microphone for the meter alone.

Per-client volume and mute are local (`SetClientVolume`, `SetClientMuted`); the
app keeps them by unique id and sends them again when the client shows up. Client
ids are reused, so the session forgets the settings of clients that left.
Watched streams play through the same mixer under made-up client ids (counting
down from 65535) with their own volume (`SetStreamVolume`).

Push-to-talk works with the button in the window and a global hotkey
(`voelin_platform::HotkeyManager`: the portal on Wayland, XInput2 on X11, a hook on
Windows), configurable as e.g. `Ctrl+Shift+T`.

## Devices

Devices are selected by cpal's stable id (`host:device`, e.g. `alsa:hw:1,0`);
`device::input_devices()` / `output_devices()` list them with the system defaults
marked. cpal's error callback flags a stream as lost on fatal errors (not on xruns
or rerouting); `device::Managed` then reopens the selected device, or the default
one while the selected one is missing (retrying every 2 s, moving back within 5 s
of it reappearing). The audio thread reports these changes as session errors.
