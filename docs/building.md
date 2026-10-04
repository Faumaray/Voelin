# Packages: from releases and built locally

Every platform has installable packages. The release workflow builds them,
and the Dockerfiles in `docker/` build the same kinds of packages on any
machine with Docker. Both use `scripts/package.sh`, so file names and
contents match. Regular CI (`ci.yml`, every push and PR) only runs the fast
checks and builds no packages.

The packages hold the app, Voelin. The gateway `tsgw` runs on servers and is
packaged for Linux only, separately. `voelinctl` is a development tool and is
in no package (`cargo run -p voelinctl`).

| Platform | Files | Install |
|---|---|---|
| Linux x86_64 | `voelin-<version>-linux-x86_64.tar.gz` | unpack; `bin/voelin` plus desktop file and icon (prefix layout: `tar -xzf … --strip-components=1 -C ~/.local`) |
| | `voelin_<version>_amd64.deb` | `sudo apt install ./voelin_<version>_amd64.deb` (Ubuntu 24.04+, Debian 13+) |
| | `voelin.flatpak` (release workflow only) | `flatpak install --user voelin.flatpak` |
| Linux server | `tsgw_<version>_amd64.deb` | `sudo apt install ./tsgw_<version>_amd64.deb`, then configure and start it ([gateway-admin.md](gateway-admin.md#install)) |
| | `tsgw-<version>-linux-x86_64.tar.gz` | unpack; `bin/tsgw`, the systemd unit and the example config |
| | `tsgw-image.tar.gz` (release workflow only) | `docker load -i tsgw-image.tar.gz` |
| Windows x86_64 | `voelin-<version>-windows-x86_64.zip` | unpack; `voelin.exe` |
| | `voelin-<version>-setup.exe` | run it (per-machine install, uninstaller in Settings → Apps) |
| Android | `voelin-<version>-android-debug.apk` | `adb install …` or open it on the phone (arm64-v8a and x86_64) |
| | `voelin-<version>-android-release.apk` | only with signing configured ([release.md](release.md#android-apk-signing)) |

Each package run also writes `SHA256SUMS-<platform>`. Nothing is code-signed
yet: Windows SmartScreen warns about the installer, and the debug APK is
signed with a debug key (fine for testing, not for the Play Store).

The gateway's image can also be built directly:
`docker build -f crates/voelin-gateway/Dockerfile -t tsgw .`.

Gitea at [git.faumaray.ru](https://git.faumaray.ru) builds the same package
types through `.gitea/workflows/release.yml`: manual runs produce one
`voelin-packages` artifact; `v*` tag pushes also attach the packages to a
draft release. Windows packages there use MinGW rather than MSVC. See
[Gitea releases](release.md#gitea-releases) for signing and publishing,
and [runner setup](ci.md#gitea-release-runner) for host requirements.

## From releases

`.github/workflows/release.yml` builds every package:

- **A version tag** `v*` (`git tag -s v0.2.0 && git push origin v0.2.0`): the
  packages and a `SHA256SUMS` are attached to a **draft** GitHub release once
  all checks pass (the same checks as CI, plus the Windows tests); review and
  publish it by hand ([release.md](release.md#checklist)).
- **By hand**, for a build to test without a release: Actions → Release → Run
  workflow (or `gh workflow run release.yml --ref <branch>`). The packages are
  uploaded as artifacts, kept for 90 days: `voelin-linux-x86_64` (the app's and
  the gateway's Linux packages), `voelin-windows-x86_64`,
  `voelin-flatpak-x86_64`, `voelin-android` and `tsgw-image`.

```sh
gh workflow run release.yml --ref main
gh run list --workflow release.yml --limit 3
gh run download <run id> --dir dist
```

The Windows packages use the MSVC toolchain and libvpx from vcpkg. The
Android job builds a signed release APK only when the repository has the
signing secrets (`ANDROID_KEYSTORE_BASE64`, `VOELIN_SIGNING_STORE_PASSWORD`,
`VOELIN_SIGNING_KEY_ALIAS`, `VOELIN_SIGNING_KEY_PASSWORD`).

`.github/workflows/docker.yml` builds the Docker files below for every
release tag and by hand (Actions → Docker builds → Run workflow), and uploads
their output as `docker-linux`, `docker-windows` and `docker-android`.

## Locally with Docker

Needs Docker 23+ (BuildKit) or Docker Desktop, and about 15 GB of free disk
per platform (toolchains plus the build cache). From the repository root:

```sh
scripts/docker-build.sh linux     # → dist/linux/
scripts/docker-build.sh windows   # → dist/windows/
scripts/docker-build.sh android   # → dist/android/
scripts/docker-build.sh           # all three
```

The same with plain Docker, e.g. `docker build -f docker/linux.Dockerfile -o dist/linux .`
Rebuilds are incremental: the cargo registry, `target/` and Gradle live in
BuildKit cache mounts (`docker builder prune` frees them).

| Dockerfile | Builds | Notes |
|---|---|---|
| `docker/linux.Dockerfile` | the app's and the gateway's `.tar.gz` and `.deb` on Ubuntu 24.04 | the binaries need glibc 2.39+ and the libraries the `.deb` lists |
| `docker/windows.Dockerfile` | `.zip` and the NSIS installer | cross-compiled for `x86_64-pc-windows-gnu` with mingw-w64 and a static libvpx; works on Windows 10 and 11 |
| `docker/android.Dockerfile` | debug APK | SDK, NDK and cargo-ndk as in CI; `--build-arg ABIS=arm64-v8a,x86_64,armeabi-v7a` picks the ABIs |

Options go after the platform, straight to `docker build`:

```sh
scripts/docker-build.sh android --build-arg ABIS=arm64-v8a
scripts/docker-build.sh android --build-arg APK=release \
  --secret id=keystore,src=release.jks --secret id=signing,src=signing.env
```

For the signed release APK, `signing.env` holds `VOELIN_SIGNING_STORE_PASSWORD=…`,
`VOELIN_SIGNING_KEY_ALIAS=…` and `VOELIN_SIGNING_KEY_PASSWORD=…`. Secrets are not
stored in the image or the build cache.

**Behind a proxy:** `scripts/docker-build.sh` passes `HTTPS_PROXY`,
`HTTP_PROXY` and `NO_PROXY` on to the build and uses host networking when
the proxy is on localhost. If the proxy inspects TLS, set
`BUILD_CA_CERT=/path/to/proxy-ca.pem` so the builds trust it.

## Locally without Docker

- **Linux:** the packages from the table above need
  `sudo apt install libasound2-dev libfontconfig1-dev libxkbcommon-dev libvpx-dev libdav1d-dev libpipewire-0.3-dev libspa-0.2-dev libclang-dev cmake dpkg-dev`,
  then `cargo build --release --locked -p voelin-ui -p voelin-gateway`
  and `scripts/package.sh linux`.
- **Windows:** [packaging/windows/README.md](../packaging/windows/README.md)
  (MSVC, vcpkg libvpx, NSIS), then `bash scripts/package.sh windows` from Git Bash.
- **Android:** [android.md](android.md) (SDK, NDK, cargo-ndk), then
  `cd android && ./gradlew assembleDebug` and `scripts/package.sh android`.
- **Flatpak:** [packaging/README.md](../packaging/README.md#flatpak).
  On a trusted machine with rootful Docker,
  `scripts/flatpak-build.sh` writes `dist/flatpak/voelin.flatpak` using the
  release builder image. It requires a privileged container for Flatpak's
  nested sandbox; do not use it on an untrusted build host.
