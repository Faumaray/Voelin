# Windows build and installer

CI (`package-windows` in `.github/workflows/ci.yml`) builds `tsc-desktop.exe`,
`tsctl.exe` and `tsgw.exe` on `windows-2025` and uploads the zip and the
installer as the `tsc-windows-x86_64` artifact. Nothing is signed there.
`docker/windows.Dockerfile` cross-compiles the same packages on Linux
([docs/building.md](../../docs/building.md)).

## Build

Needs the MSVC build tools, CMake (bundled libopus), NASM (assembly of
`aws-lc-sys`, used by the WebRTC stack) and libvpx (video, `tsc-media`):

```powershell
winget install NASM.NASM Kitware.CMake NSIS.NSIS
vcpkg install libvpx:x64-windows-static-md

$vpx = "$env:VCPKG_ROOT\installed\x64-windows-static-md"
$env:VPX_LIB_DIR = "$vpx\lib"
$env:VPX_INCLUDE_DIR = "$vpx\include"
$env:VPX_VERSION = "1.15.2"   # what `vcpkg list libvpx` shows
$env:VPX_STATIC = "1"

cargo build --release --locked -p tsc-ui
makensis /DVERSION=0.1.0 packaging\windows\installer.nsi
```

This gives `target\release\tsc-desktop.exe` and
`packaging\windows\tsc-desktop-0.1.0-setup.exe`. The installer puts the app in
`Program Files\TS Client`, adds a Start menu shortcut and an uninstaller; user
data stays in `%APPDATA%\tsc`.

The executable uses the dynamic MSVC runtime (`VCRUNTIME140.dll`, present on most
systems). To drop that dependency build with
`RUSTFLAGS="-C target-feature=+crt-static"` and libvpx from the
`x64-windows-static` triplet, or ship the Visual C++ redistributable.

## Signing

Unsigned builds trigger SmartScreen warnings. Sign the executable before packaging
it, then the installer and the uninstaller it writes (NSIS 3.08+ runs the `SIGN`
command on both):

```powershell
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /f cert.pfx /p $env:CERT_PASSWORD target\release\tsc-desktop.exe
makensis /DVERSION=0.1.0 "/DSIGN=signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /f cert.pfx /p $env:CERT_PASSWORD %1" packaging\windows\installer.nsi
```

Options for the certificate: an OV/EV code-signing certificate on a hardware token
or in a cloud HSM, Azure Trusted Signing, or SignPath's free plan for open-source
projects. In CI, keep the certificate (base64) and password in repository secrets,
decode it to a file in the job, sign, and delete it.
