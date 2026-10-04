# Windows build and installer

The release workflow (`package-windows` in `.github/workflows/release.yml`)
builds `voelin.exe` on `windows-2025` and attaches the zip and the installer to
the release (artifact `voelin-windows-x86_64` when run by hand). Nothing is
signed there. Regular CI only runs clippy on Windows. The gateway `tsgw`
is for servers and packaged for Linux only.
`docker/windows.Dockerfile` cross-compiles the same packages on Linux
([docs/building.md](../../docs/building.md)).

## Build

Needs the MSVC build tools, CMake (bundled libopus), NASM (assembly of
`aws-lc-sys`, used by the WebRTC stack), libvpx (VP8/VP9) and dav1d (AV1
decoding), both for `voelin-media`:

```powershell
winget install NASM.NASM Kitware.CMake NSIS.NSIS
vcpkg install libvpx:x64-windows-static-md dav1d:x64-windows-static-md

$vpx = "$env:VCPKG_ROOT\installed\x64-windows-static-md"
$env:VPX_LIB_DIR = "$vpx\lib"
$env:VPX_INCLUDE_DIR = "$vpx\include"
$env:VPX_VERSION = "1.15.2"   # what `vcpkg list libvpx` shows
$env:VPX_STATIC = "1"
# dav1d-sys through system-deps, without pkg-config.
$env:SYSTEM_DEPS_DAV1D_NO_PKG_CONFIG = "1"
$env:SYSTEM_DEPS_DAV1D_SEARCH_NATIVE = "$vpx\lib"
$env:SYSTEM_DEPS_DAV1D_LIB = "dav1d"
$env:SYSTEM_DEPS_DAV1D_LINK = "static"

cargo build --release --locked -p voelin-ui
bash scripts/fetch-ffmpeg-windows.sh ffmpeg
makensis /DVERSION=0.1.0 /DFFMPEG=..\..\ffmpeg packaging\windows\installer.nsi
```

FFmpeg (hardware encoders and decoders, H.265, more AV1 decoders) is not
linked: the app loads `avcodec-*.dll` and `avutil-*.dll` from its own
directory, then `PATH`. The packages carry BtbN's LGPL build of FFmpeg 8.1
(`scripts/fetch-ffmpeg-windows.sh`, with `FFMPEG-LICENSE.txt` and
`FFMPEG-README.txt` naming the build and its source); replacing the DLLs
with another build of the same major versions works.

This gives `target\release\voelin.exe` and
`packaging\windows\voelin-0.1.0-setup.exe`. The installer puts the app in
`Program Files\Voelin`, adds a Start menu shortcut and an uninstaller; user
data stays in `%APPDATA%\voelin`.

The executable uses the dynamic MSVC runtime (`VCRUNTIME140.dll`, present on most
systems). To drop that dependency build with
`RUSTFLAGS="-C target-feature=+crt-static"` and libvpx from the
`x64-windows-static` triplet, or ship the Visual C++ redistributable.

## Signing

Unsigned builds trigger SmartScreen warnings. Sign the executable before packaging
it, then the installer and the uninstaller it writes (NSIS 3.08+ runs the `SIGN`
command on both):

```powershell
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /f cert.pfx /p $env:CERT_PASSWORD target\release\voelin.exe
makensis /DVERSION=0.1.0 "/DSIGN=signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /f cert.pfx /p $env:CERT_PASSWORD %1" packaging\windows\installer.nsi
```

Options for the certificate: an OV/EV code-signing certificate on a hardware token
or in a cloud HSM, Azure Trusted Signing, or SignPath's free plan for open-source
projects. In CI, keep the certificate (base64) and password in repository secrets,
decode it to a file in the job, sign, and delete it.
