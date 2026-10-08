# Android app

The Android app is the desktop UI (`voelin-ui`, Slint) running in a
`NativeActivity`, plus a thin Kotlin layer for what only Java APIs can do:
foreground services and their notifications, the screen-capture consent,
the cameras, the share sheet, the Keystore.

```
android/                         Gradle project (AGP 8.13, Kotlin 2.3, Gradle 8.14.3 wrapper)
  app/src/main/java/.../voelin/     MainActivity (NativeActivity, Share to Voelin), Bridge,
                                 Native, VoiceService, ScreenCaptureService, Notifications,
                                 CameraCapture, SecretStore
crates/voelin-android/              libvoelin_android.so: android_main, engine host,
                                 JNI glue, capture and camera providers, notifications
                                 (foreground.rs), Keystore secrets
```

The phone layout of the UI (the voice channel with a stream, the bottom
navigation, the studio, the share dialog) is described in
[ui.md](ui.md); its screenshots are taken on the desktop at 390×844.

## Building

Requirements:

- Android SDK with `platforms;android-36` and `build-tools;36.0.0`
  (`ANDROID_HOME`, or `sdk.dir` in `android/local.properties`), NDK
  `27.3.13750724` (`ndk;27.3.13750724`; the versions are pinned in
  `app/build.gradle.kts`)
- JDK 17 or newer
- Rust targets: `rustup target add aarch64-linux-android x86_64-linux-android`
- `cargo install cargo-ndk --locked` (4.x)
- CMake and make (or `CMAKE_GENERATOR=Ninja` with ninja): libopus is built
  from source

```sh
cd android
./gradlew assembleDebug                         # arm64-v8a + x86_64 (emulator, Waydroid)
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
  `VoiceService` runs as a foreground service of type `microphone`. Its
  notification reads like the app's voice card: the channel as the title,
  the server and who is there ("Home · 3 in voice", "just you"), the time
  in voice as a chronometer, and Mute, Deafen and Leave, which act on every
  voice session; with several servers, "In voice on 2 servers" and their
  names. Tapping it opens the voice channel screen (the activity hands the
  request to Rust, which queues it until the window runs:
  `voelin_ui::request`). Rust drives it from engine events (`foreground.rs`,
  `Bridge.setVoiceNotification`). Audio itself is cpal on AAudio, as on
  desktop.
- **Sharing the screen**: `voelin_media::capture::default_screen_capture()`
  returns the MediaProjection provider (`capture.rs`) registered at start.
  Starting shows the system consent dialog; on consent `ScreenCaptureService`
  (type `mediaProjection`) mirrors the display, longest side at most
  1920 px. While one MediaCodec encoder runs (one simulcast layer, one
  codec: the common case) the virtual display renders straight into that
  encoder's input surface, with no copy of a frame on the CPU; with more
  encoders (layers, or a codec a viewer chose) it renders into an
  `ImageReader` again, whose RGBA buffers go to the encoders borrowed
  (`Native.onScreenFrame`), until one encoder is left ([media.md](media.md),
  MediaCodec). System audio (`default_audio_capture()`) is
  `AudioPlaybackCapture` of media, game and unknown usages, 48 kHz stereo
  float, only while the projection runs; single apps are captured by their
  uid (`addMatchingUid`), "every app but ours" excludes ours. While our
  stream is live the notification says where and who watches ("Live in
  Chill Zone", "Home · 2 watching") with Stop sharing. Ending the
  projection from the status bar or the notification closes the capture
  channels.
- **Choosing the stream's sound**: the share dialog and the Stream Studio
  pick the sources of `stream.audio_sources` in the studio's picker:
  the microphone, every app but ours, and the launchable apps
  (PackageManager) with their own icons, several at once. Kotlin draws each
  app's icon once per installed version into a 96 px PNG in the cache
  (`Bridge.launchableApps`, `AudioApp::icon`). The shared window's audio is not
  offered: a phone shares the whole screen.
- **Cameras** (the Stream Studio's camera sources): `CameraCapture.kt`
  lists the Camera2 cameras (front first, their YUV sizes and highest frame
  rate) and asks for the `CAMERA` permission when a camera is first opened
  (the source shows why if it is refused). It opens the camera into an
  `ImageReader` (`YUV_420_888`) at the wished size or the nearest (16:9
  first, 1280×720 by default) and hands each frame's planes to Rust as
  direct buffers valid during the call, with the turn that makes it
  upright. Rust (`camera.rs`) passes upright planar or NV12 pictures on as
  they are and gathers NV21 or turned ones into I420 in a buffer each
  camera keeps: nothing is allocated per frame. Cameras that face the user
  are mirrored by default; the source menu's Switch camera moves a source
  to the next camera ([studio.md](studio.md)).
- **Share to Voelin**: the activity takes `ACTION_SEND` and
  `ACTION_SEND_MULTIPLE` of any type. Text and links go into the composer
  of the current chat, which the phone then shows (one tap sends them).
  Files are copied out of the sending app while its content URIs are
  readable, on a thread of their own, under the name the app gives (made
  safe as a path component), then uploaded to the current chat's channel
  (or the channel we are in) and linked there as the attach button does.
  The copies live in the cache and are removed at the next start.
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
`_MEDIA_PROJECTION`, `POST_NOTIFICATIONS`, `CAMERA` (asked for when a camera
is first opened; `android.hardware.camera.any` is optional).

## Status

| What | How | Status |
|---|---|---|
| `libvoelin_android.so` for `aarch64-linux-android` (Slint Android backend with Skia, AAudio, AWS-LC, SQLite, libopus) | `cargo ndk -t arm64-v8a build -p voelin-android` | builds; exports `android_main` and the `Native` methods |
| Debug APK | `./gradlew assembleDebug -Pvoelin.abis=arm64-v8a` | builds |
| Debug APK for phones and Waydroid (`arm64-v8a` + `x86_64`) | `./gradlew assembleDebug` on Linux: the Gradle 8.14.3 wrapper, Temurin JDK 17, an SDK with only `platforms;android-36`, `build-tools;36.0.0` and the NDK 27.3 | builds (Kotlin without warnings) |
| Engine host replay, voice service policy, the voice notification's text (deafen across sessions, several servers), the live stream's notice | unit tests (`cargo test -p voelin-android`) | tested |
| External capture providers, MediaCodec buffer layouts | `voelin-media` unit tests | tested |
| The phone screens: voice channel with a stream, Home, Servers, Chats, Activity, You, Settings, Stream Studio, the share dialog with its sound | headless screenshots at 390×844 ([ui.md](ui.md)) | tested on the desktop (the same Slint UI; not on a device) |
| A tapped voice notification opens the voice screen; text shared to the app lands in the composer | `VOELIN_OPEN=notification`, `VOELIN_OPEN=shared:<text>` on the desktop | the Rust side tested headless; the notifications, their actions, the share sheet and file uploads from it compile only |
| Cameras: YUV planes borrowed, gathered into I420, turned | `voelin-media` `camera::tests::camera_planes_are_borrowed_gathered_and_turned` | the Rust side tested; Camera2, the permission flow and switching compile only |
| The virtual display into the encoder's input surface, back to the reader for simulcast | `cargo ndk -t arm64-v8a clippy` | compiles only: frames per second and CPU use not measured (no device; Waydroid's encoders are software ones, if any) |
| The app picker with icons | the studio's and the share dialog's picker with sample data (headless) | the UI tested; the PackageManager list and its icons compile only |
| The app on a device: UI, voice in the background, screen sharing, watching, Keystore | manual matrix row IN3 and the Android rows | not run yet (no device or emulator in this environment) |
| Waydroid (Mesa GL) | the debug APK, `waydroid logcat` | works with the EGL workaround (below); before it, the app ended at start because Skia could not create its GL context |

Known gaps:

- The streams panel of the UI and the stream media pipeline of `voelin-core`
  must build without the desktop media backends on Android (libvpx, X11,
  PipeWire): `voelin-media` with `default-features = false` there, so that
  `Codecs` uses MediaCodec.
- Rotating the device while sharing letterboxes the picture (the virtual
  display keeps its size).
- Cameras deliver while the app is in front: there is no foreground service
  of type `camera`, so Android stops a camera source when the app goes to
  the background (the screen share goes on).
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

### Trying it on Waydroid

Waydroid on a PC runs the `x86_64` library, which the default
`assembleDebug` builds (`rustup target add x86_64-linux-android` first):

```sh
cd android && ./gradlew assembleDebug
waydroid app install app/build/outputs/apk/debug/app-debug.apk
waydroid app launch io.github.faumaray.Voelin
waydroid logcat | grep -E 'RustStdoutStderr|MainActivity|VoiceService|ScreenCaptureService|CameraCapture'
```

Things to try there: the bottom navigation (Home, Servers, Chats,
Activity, You), joining a voice channel and its screen from the top bar's
voice button, the voice notification (pull down the shade: Mute, Deafen,
Leave, tap it), Share from another app's share sheet (text, then a file),
the voice screen's Share menu: Share screen (the screen share dialog) with
sound from the audio picker (app icons) and its gain and mute while live,
and the Stream Studio with a camera source (Waydroid usually has no camera:
the source then says why).
