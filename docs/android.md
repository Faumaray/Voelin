# Android app

The Android app is the desktop UI (`voelin-ui`, Slint) running in a
`NativeActivity`, plus a thin Kotlin layer for what only Java APIs can do:
foreground services, the screen-capture consent, the Keystore.

```
android/                         Gradle project (AGP 8.13, Kotlin 2.3, Gradle 8.14.3 wrapper)
  app/src/main/java/.../voelin/     MainActivity (NativeActivity), Bridge, Native,
                                 VoiceService, ScreenCaptureService, SecretStore
crates/voelin-android/              libvoelin_android.so: android_main, engine host,
                                 JNI glue, capture providers, Keystore secrets
```

## Building

Requirements:

- Android SDK with `platforms;android-36` and `build-tools;36.0.0`
  (`ANDROID_HOME`, or `sdk.dir` in `android/local.properties`), NDK
  `27.3.13750724` (`ndk;27.3.13750724`; the version is pinned in
  `app/build.gradle.kts`)
- JDK 17 or newer
- Rust targets: `rustup target add aarch64-linux-android x86_64-linux-android`
- `cargo install cargo-ndk --locked` (4.x)
- CMake and make (or `CMAKE_GENERATOR=Ninja` with ninja): libopus is built
  from source

```sh
cd android
./gradlew assembleDebug                         # arm64-v8a + x86_64 (emulator)
./gradlew assembleDebug -Pvoelin.abis=arm64-v8a    # phones only, half the build
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb logcat -s RustStdoutStderr ScreenCaptureService VoiceService SecretStore
```

`assemble<BuildType>` runs `cargo ndk ... build -p voelin-android` (with
`--release` for release) into `app/build/rust/<build type>/<abi>/` before the
native libraries are merged; `-Pvoelin.skipCargo=true` packages what is there
(for CI steps that built the library already). The app's `versionName` is the
version of `crates/voelin-android`, and `versionCode` is derived from it
(docs/release.md). The Kotlin half of `rustls-platform-verifier` comes from
its Maven archive in the version `Cargo.lock` pins.

Release builds are minified (R8) and signed only when the signing settings
are given as Gradle properties (e.g. `~/.gradle/gradle.properties`) or
environment variables; nothing of it lives in the repository:

| Gradle property | Environment variable |
|---|---|
| `voelin.signing.storeFile` | `VOELIN_SIGNING_STORE_FILE` |
| `voelin.signing.storePassword` | `VOELIN_SIGNING_STORE_PASSWORD` |
| `voelin.signing.keyAlias` | `VOELIN_SIGNING_KEY_ALIAS` |
| `voelin.signing.keyPassword` | `VOELIN_SIGNING_KEY_PASSWORD` |

```sh
VOELIN_SIGNING_STORE_FILE=~/keys/voelin-release.jks VOELIN_SIGNING_STORE_PASSWORD=... \
VOELIN_SIGNING_KEY_ALIAS=voelin VOELIN_SIGNING_KEY_PASSWORD=... ./gradlew assembleRelease bundleRelease
```

minSdk is 29 (Android 10: MediaProjection foreground service type, playback
capture, NDK MediaCodec format queries); targetSdk and compileSdk 36.

## How it fits together

- **Entry**: the activity loads `libvoelin_android.so` (`android.app.lib_name`);
  android-activity calls `android_main` on a new thread for every activity
  instance. It installs crash reports (opt-in, `crash_reports` setting,
  `<files>/crash-reports`), looks up the Kotlin `Bridge` class, initialises
  `rustls-platform-verifier` (HTTPS certificate checks against the system
  trust store), asks for the microphone and notification permissions, sets
  Slint's Android backend and runs `voelin_ui::run` with the app's files
  directory, Keystore secrets and the process's engine.
- **Engine for the process** (`host.rs`): one tokio runtime and `Engine`
  per process. The activity may be destroyed while voice goes on; a new
  window attaches and first receives events that rebuild the current state
  (server info, state, presence, the last 300 chat messages per session,
  streams), then live events (`voelin_ui::HostedEngine`). Back moves the task
  to the background instead of finishing the activity; configuration changes
  do not recreate it.
- **Voice in the background**: while any session has a voice connection,
  `VoiceService` runs as a foreground service of type `microphone` with a
  notification ("In voice on …", Mute or Unmute, Disconnect). Rust drives it
  from engine events (`foreground.rs`, `Bridge.setVoiceNotification`). Audio
  itself is cpal on AAudio, as on desktop.
- **Sharing the screen**: `voelin_media::capture::default_screen_capture()`
  returns the MediaProjection provider (`capture.rs`) registered at start.
  Starting shows the system consent dialog; on consent `ScreenCaptureService`
  (type `mediaProjection`) mirrors the display into an `ImageReader` at the
  requested frame rate, longest side at most 1920 px, and hands RGBA frames to
  `Native.onScreenFrame`. System audio (`default_audio_capture()`) is
  `AudioPlaybackCapture` of media, game and unknown usages, 48 kHz stereo
  float, only while the projection runs. Ending the projection from the
  status bar or the notification closes the capture channels.
- **Watching and encoding**: `voelin_media::Codecs` uses the device's
  MediaCodec encoders (hardware first) and decoders on Android
  ([media.md](media.md)).
- **Secrets**: `SecretStore` encrypts each password with AES-256-GCM under a
  non-exportable Android Keystore key and keeps the ciphertext in private
  preferences. The app disables Android backups (the database holds the
  identity's private key).
- **JNI**: Rust calls static methods of `Bridge` (class looked up once on the
  `android_main` thread, usable from any thread); Kotlin calls the
  `external` methods of `Native`, exported by `native_method!` (panics and
  errors become Java exceptions). Keep `bridge.rs`, `Bridge.kt` and
  `Native.kt` in step.

Permissions: `INTERNET`, `ACCESS_NETWORK_STATE`, `RECORD_AUDIO`,
`MODIFY_AUDIO_SETTINGS`, `FOREGROUND_SERVICE` with `_MICROPHONE` and
`_MEDIA_PROJECTION`, `POST_NOTIFICATIONS`.

## Status

| What | How | Status |
|---|---|---|
| `libvoelin_android.so` for `aarch64-linux-android` (Slint Android backend with Skia, AAudio, AWS-LC, SQLite, libopus) | `cargo ndk -t arm64-v8a build -p voelin-android` | builds; exports `android_main` and the `Native` methods |
| Debug APK | `./gradlew assembleDebug -Pvoelin.abis=arm64-v8a` | builds |
| Engine host replay, voice service policy | unit tests (`cargo test -p voelin-android`) | tested |
| External capture providers, MediaCodec buffer layouts | `voelin-media` unit tests | tested |
| The app on a device: UI, voice in the background, screen sharing, watching, Keystore | manual matrix row IN3 and the Android rows | not run yet (no device or emulator in this environment) |
| Waydroid (Mesa GL) | the debug APK, `waydroid logcat` | the first start failed: Skia could not create its GL context; worked around (below), to be re-checked |

Known gaps:

- The streams panel of the UI and the stream media pipeline of `voelin-core`
  must build without the desktop media backends on Android (libvpx, X11,
  PipeWire): `voelin-media` with `default-features = false` there, so that
  `Codecs` uses MediaCodec.
- Rotating the device while sharing letterboxes the picture (the virtual
  display keeps its size).
- The safe area (status and navigation bars) and the on-screen keyboard pad
  the main window; dialogs drawn over it do not account for them yet.
- Only `arm64-v8a` and `x86_64` are built; 32-bit devices are not supported.
- No audio focus handling yet: a phone call does not pause our audio
  (matrix row AN4). After a network change (Wi-Fi to mobile data) only
  tsclientlib's own reconnect applies; untested on a phone.

### Mesa-based Android (Waydroid)

Slint draws with Skia over OpenGL ES. Before Skia looks up its functions,
Slint's surface loads glow's whole GL function table through
`eglGetProcAddress`. Android's libEGL gives every function it has no built-in
entry point for one of 256 "extension slots", and Mesa answers every name,
desktop GL included, so the slots run out; Skia then gets nothing for an
extension function it needs (`glTextureBarrierNV`) and the app ends right
after start:

```
E libEGL  : no more slots for eglGetProcAddress("glTextureBarrierNV")
I RustStdoutStderr: … Could not create Skia Direct Context from GL interface
```

`crates/voelin-android/src/egl.rs` looks up Skia's extension functions once
at start, before Slint's surface, so they get slots first (logcat:
`EGL: N of 83 Skia extension functions resolved`). Vendor GPU drivers on
phones return nothing for desktop-only names and are not affected.
