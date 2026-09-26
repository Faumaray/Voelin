# Manual test matrix

What CI cannot check: other clients' WebRTC stacks, real desktops and
portals, real audio hardware, Android power management and the installers.
Run this matrix before every release (see [release.md](../release.md)) and
after changes to the area in question.

## How to record results

- Fill in **Result**, **Date** (YYYY-MM-DD) and **Version** (from the About
  page; the commit hash for unreleased builds) in the tables below and
  commit the update with the release.
- Result: `pass`, `fail` with a link to the issue, `partial` with a note, or
  `n/a` when the platform does not support the feature. Empty means not run.
- Keep only the latest run per row; earlier runs are in the git history.
- Note the desktop and OS versions in **Notes** the first time a row is run
  on a new release of them (e.g. "GNOME 48.2, Fedora 42").

Platforms:

| Name | Session |
|---|---|
| GNOME | GNOME Shell 48 or newer, Wayland (older GNOME has no GlobalShortcuts portal) |
| KDE | KDE Plasma 6, Wayland |
| X11 | Any X11 session (e.g. Xfce, or GNOME/KDE on Xorg) |
| Win10 | Windows 10 22H2 |
| Win11 | Windows 11 24H2 or newer |
| Android | A phone with Android 12 or newer; note the model |

Servers: TeamSpeak 6 (streams need it; use the version pinned in
`dev/docker-compose.yml` or newer) and TeamSpeak 3.13 for voice checks. The
official client is the current TeamSpeak 6 client for the same OS unless
noted.

## Streams with the official TeamSpeak 6 client

- **ST1** The official client watches our stream: we share a monitor at
  1080p30 with sound in our channel; the official client joins it. Pass:
  picture within 5 s, smooth motion (move a window around), sound in sync
  (play a video with speech), no freeze over 10 minutes, stop from either
  side ends it cleanly.
- **ST2** We watch the official client's stream (screen with sound). Same
  pass criteria. Note the codec it sent (debug log or stats).
- **ST3** Screen share with sound: system audio reaches viewers; voices of
  the call are not sent back to them (known gap on Linux PipeWire, see
  [media.md](../media.md): note what viewers hear).
- **ST4** Portal restore token (Wayland): share, stop, share again. Pass: the
  second share starts without the picker; after revoking the permission in
  the desktop's settings the picker shows again.
- **ST5** H.264: enable OpenH264 (download on first use, attribution text
  shown next to the switch), stream to the official client and watch its
  H.264 stream. Disable it again: H.264 is no longer offered.

| ID | Platform | Result | Date | Version | Notes |
|---|---|---|---|---|---|
| ST1 | GNOME | | | | |
| ST1 | KDE | | | | |
| ST1 | X11 | | | | |
| ST1 | Win10 | | | | |
| ST1 | Win11 | | | | |
| ST2 | GNOME | | | | |
| ST2 | KDE | | | | |
| ST2 | X11 | | | | |
| ST2 | Win10 | | | | |
| ST2 | Win11 | | | | |
| ST3 | GNOME | | | | |
| ST3 | KDE | | | | |
| ST3 | X11 | | | | |
| ST3 | Win10 | | | | |
| ST3 | Win11 | | | | |
| ST4 | GNOME | | | | |
| ST4 | KDE | | | | |
| ST5 | GNOME or KDE | | | | |
| ST5 | Win11 | | | | |

## Global push-to-talk

- **PT1** Bind push-to-talk (Wayland: the desktop's dialog appears; accept
  or change the key), focus another application (a browser, a game in
  fullscreen), hold the key: the talking indicator lights and others hear
  us; release: it stops within 100 ms. Repeat 20 times quickly: no stuck
  transmission. Hold the key and Alt+Tab away: release still ends it.
- **PT2** Change the binding and restart the app: the new binding works; on
  Wayland the desktop keeps the binding without asking again.
- **PT3** Windows: with an elevated window (e.g. Task Manager) focused, note
  whether push-to-talk works (a hook of a non-elevated app does not see
  those keys; document, do not fail).

| ID | Platform | Result | Date | Version | Notes |
|---|---|---|---|---|---|
| PT1 | GNOME | | | | |
| PT1 | KDE | | | | |
| PT1 | X11 | | | | |
| PT1 | Win10 | | | | |
| PT1 | Win11 | | | | |
| PT1 | Flatpak on GNOME or KDE | | | | |
| PT2 | GNOME | | | | |
| PT2 | KDE | | | | |
| PT2 | X11 | | | | |
| PT2 | Win11 | | | | |
| PT3 | Win11 | | | | |

## Audio

- **AU1** Echo cancellation in a real one-hour call: laptop or desk
  speakers (no headset) on our side, a second person on another client.
  Pass: the other side hears no echo of their own voice (also while both
  talk), our voice is not cut off, and the delay at the end of the hour is
  the same as at the start (clap test at 0, 30 and 60 minutes). Note CPU
  usage.
- **AU2** Device hotplug during a call: unplug the USB headset in use: audio
  moves to the default device within a few seconds and an error or notice
  is shown; plug it back in: it moves back within 5 s. Repeat with a
  Bluetooth headset (connect, disconnect, switch between headset and
  high-quality profiles).
- **AU3** Change the system default output and input while connected with
  "default" selected: the app follows.

| ID | Platform | Result | Date | Version | Notes |
|---|---|---|---|---|---|
| AU1 | GNOME or KDE | | | | |
| AU1 | Win11 | | | | |
| AU1 | Android | | | | |
| AU2 | GNOME | | | | |
| AU2 | KDE | | | | |
| AU2 | X11 | | | | |
| AU2 | Win10 | | | | |
| AU2 | Win11 | | | | |
| AU2 | Android | | | | |
| AU3 | GNOME or KDE | | | | |
| AU3 | Win11 | | | | |

## Android

- **AN1** Background voice: connect, lock the phone, 30 minutes with the
  screen off (unplugged, not on a charger). Pass: still connected, both
  directions audible the whole time, the foreground notification is shown
  and its mute / disconnect actions work. Note the battery used.
- **AN2** Watch a stream: the official desktop client's and ours.
- **AN3** Share the screen (MediaProjection) with sound to the official
  client and to our desktop app; stop from the notification.
- **AN4** A phone call during a voice session: our audio pauses, the session
  resumes afterwards. Switch between Wi-Fi and mobile data: it reconnects.

| ID | Platform | Result | Date | Version | Notes |
|---|---|---|---|---|---|
| AN1 | Android | | | | |
| AN2 | Android | | | | |
| AN3 | Android | | | | |
| AN4 | Android | | | | |

## Install and uninstall

- **IN1** Flatpak (bundle or Flathub): install, first run (microphone
  permission, portals, notifications, keyring), update over the previous
  version (data kept), `flatpak uninstall` (data kept) and
  `flatpak uninstall --delete-data` (`~/.var/app/<app id>` removed).
- **IN2** Windows installer on a clean VM (no Visual C++ redistributable,
  no Rust toolchain): SmartScreen and UAC show the publisher of the
  signature, the app starts from the Start menu, an update over the
  previous version keeps the settings, uninstall from Settings > Apps
  removes the files, shortcut and registry key and keeps `%APPDATA%\tsc`;
  `uninstall.exe /S` works too.
- **IN3** Android APK: install, update over the previous version (same
  signature, data kept), uninstall.

| ID | Platform | Result | Date | Version | Notes |
|---|---|---|---|---|---|
| IN1 | GNOME | | | | |
| IN1 | KDE | | | | |
| IN2 | Win10 | | | | |
| IN2 | Win11 | | | | |
| IN3 | Android | | | | |

## General

- **GE1** About page: the AboutSlint widget, the OpenH264 attribution and
  the full third-party notices (scrollable) are shown.
- **GE2** Crash reports: with reports enabled, start the app with
  `TSC_TEST_CRASH=1`: it exits with a panic and, on the next start, offers
  the report (open folder, delete). With reports disabled nothing is
  written. Reports are in `~/.local/state/tsc/crash-reports` (Flatpak:
  `~/.var/app/<app id>/.local/state/tsc/crash-reports`) or
  `%LOCALAPPDATA%\tsc\crash-reports`.
- **GE3** Saved passwords survive a restart (GNOME Keyring, KWallet, Windows
  Credential Manager, Android Keystore); without a keyring the app warns
  and keeps them for the session only.
- **GE4** Notifications for pokes and private messages; clicking one brings
  the window up.
- **GE5** Suspend and resume the machine while connected: the app
  reconnects.
- **GE6** HiDPI and fractional scaling (150 %), two monitors with different
  scales: text is sharp, windows open on the right monitor.
- **GE7** Another UI language (once translations exist): switch, restart,
  no untranslated or truncated strings on the main screens.

| ID | Platform | Result | Date | Version | Notes |
|---|---|---|---|---|---|
| GE1 | any desktop | | | | |
| GE1 | Android | | | | |
| GE2 | Linux | | | | |
| GE2 | Win11 | | | | |
| GE3 | GNOME | | | | |
| GE3 | KDE | | | | |
| GE3 | Win11 | | | | |
| GE3 | Android | | | | |
| GE4 | GNOME | | | | |
| GE4 | KDE | | | | |
| GE4 | Win11 | | | | |
| GE5 | Linux | | | | |
| GE5 | Win11 | | | | |
| GE6 | GNOME | | | | |
| GE6 | KDE | | | | |
| GE6 | Win11 | | | | |
| GE7 | any desktop | | | | |
