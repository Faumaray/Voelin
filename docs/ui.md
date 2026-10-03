# The user interface

`crates/voelin-ui` is the Slint UI of the desktop app and of the Android app
(`crates/voelin-android` runs the same window). It uses Slint's software
renderer, the Inter font, Lucide icons and Twemoji; the look follows the
design mockups (deep navy panels, a blue accent, a server rail, a top bar, a
sidebar with the user card, an optional right panel).

![Server page](screenshots/desktop-server.png)

## Structure

```
crates/voelin-ui/
  ui/
    app.slint            MainWindow: picks the shell, puts the page's screens in it, dialogs
    theme.slint          Theme global: design tokens (colours, radii, spacing, type)
    icons.slint          Icons global: Lucide icons (tint them with `Icon`)
    bridge.slint         structs, the Bridge global (data and callbacks for Rust),
                         Images (decoded images) and Emoji (the picker's data)
    nav.slint            Nav global: page, settings section, phone tab, dialogs, panel size
    studio-bridge.slint  the Stream Studio's structs and globals (StudioBridge, StudioNav)
    studio-window.slint  StudioWindow: the studio in a window of its own
    components/          the design system (catalogue below); index.slint exports all
    shells/              desktop.slint (rail, top bar, Sidebar), mobile.slint (bottom
                         navigation), common.slint (Panel, VoiceCard, UserCard, ...)
    screens/             channels, chat, members (panel, card), drawers (pins, topics),
                         voice (the voice channel), streams (viewer; the phone's
                         panel), settings, home, dialogs, mobile (phone-only pages),
                         studio-parts (the studio's pieces), studio (its page, its
                         phone layout)
    assets/              fonts/ (Inter, OFL), icons/ (Lucide, ISC), logo.svg
  assets/twemoji.bin     the Twemoji SVGs packed by scripts/pack-twemoji.py
  src/
    app.rs               setup, App state, event dispatch
    servers.rs chat.rs members.rs streams.rs settings_page.rs appearance.rs
    studio.rs            the logic of each area (studio.rs: the Stream Studio's controller)
    vm/                  pure view models (engine state → Slint structs), unit-tested
    bind/                callbacks of each area, wired once
    images.rs            the image cache (LRU, bounded by `ui.image_cache_mb`)
    emoji.rs             the Twemoji archive, text → runs of text and emoji
    dev.rs               development switches (screenshots, sample data)
```

Models are created once (`app::Models`) and updated row by row with
`vm::list::sync`, which keeps the rows shared at both ends and changes,
inserts or removes the rest: a client that starts talking changes one row of
the tree. Every chat tab owns a `VecModel` of its lines; the chat view shows
the selected tab's model, so a new message is one `push` and switching tabs
swaps the model. Lists that can grow (channels, messages, notices, the emoji
grid) use `ListView`, which builds only the visible rows.

## Theme tokens

`Theme` (ui/theme.slint) holds every colour and size; components never use
literal colours.

| Group | Tokens |
|---|---|
| Mode | `mode` ("dark", "light", "system"; set from `ui.theme`), `dark` (resolved), `font-scale` (from `ui.font_scale`) |
| Backgrounds | `bg-app`, `backdrop` (gradient), `bg-rail`, `surface` (panels), `surface-2` (cards, inputs), `surface-3` (hover, menus), `surface-4` (pressed), `scrim`, `overlay` |
| Lines | `border`, `border-strong`, `glow`, `glow-width` (focus/selection ring; the software renderer draws no shadows) |
| Accent | `accent`, `accent-hover`, `accent-pressed`, `accent-soft` (selected rows), `accent-soft-hover`, `accent-text` (links), `name-text` (chat authors), `on-accent` |
| States | `live`, `live-soft`, `danger`, `danger-soft`, `success`, `success-soft`, `warning`, `warning-soft`, `idle`, `dnd`, `offline`, `gold` (crown), `info` |
| Text | `text`, `text-secondary`, `text-muted`, `text-disabled`, `icon` |
| Meter | `meter-low` (green), `meter-mid` (yellow), `meter-high` (red), `meter-off` |
| Radii | `radius-xs` 4, `radius-sm` 6, `radius-md` 8, `radius-lg` 12, `radius-xl` 16, `radius-pill` |
| Spacing | `space-1` 2 … `space-8` 32 (2, 4, 8, 12, 16, 20, 24, 32) |
| Type (scaled) | `font-xs` 11, `font-sm` 12, `font-body` 14, `font-md` 15, `font-lg` 17, `font-xl` 20, `font-2xl` 24, `font-3xl` 30; `weight-regular` … `weight-bold` |
| Sizes | `row-height`, `control-height` 36, `control-height-sm` 28, `icon-sm/md/lg` 16/20/24, `rail-width` 72, `sidebar-width` 264 (grows with the font scale), `topbar-height` 56, `right-panel-min` |
| Motion | `fast` 120 ms, `normal` 200 ms |

A light palette is filled in for every colour; `mode` switches at runtime.
The standard widgets (scroll bars, context menus) follow through
`Palette.color-scheme`.

## Components

All in `ui/components/`, exported by `components/index.slint`.

| Component | File | Key properties |
|---|---|---|
| `Icon` | icon.slint | `source` (an `Icons.*`), `size`, `tint` |
| `Spinner` | icon.slint | `size`, `tint`, `running` |
| `Button` | button.slint | `text`, `icon`, `kind` (`ButtonKind.primary/secondary/danger/ghost`), `enabled`, `checked`, `small`; `clicked` |
| `IconButton` | button.slint | `icon`, `label` (screen readers), `checked`, `danger`, `round`, `filled`, `size`, `icon-size`, `tint`, `dot`; `clicked` |
| `ActionButton` | button.slint | round button with a caption (Mute, Deafen, Go Live): `icon`, `text`, `checked`, `danger` |
| `FocusRing` | button.slint | the accent ring of focused controls |
| `TextField` | text-field.slint | `text`, `placeholder`, `input-type`, `icon`, `label`, `bare`, `read-only`; `accepted`, `edited`, `key-pressed`; `clear()`, `select-all()` |
| `SearchBox` | text-field.slint | a TextField with a magnifier and the Ctrl K hint (`show-shortcut`) |
| `Kbd` | text-field.slint | a key cap |
| `Field` | text-field.slint | `label` above a control (@children), `hint` below |
| `Select` | select.slint | drop-down: `model`, `current-index`, `current-value`, `icon`, `label`; `selected(string)` |
| `Toggle` | toggle.slint | the pill switch: `checked`, `label`; `toggled(bool)` |
| `ToggleRow` | toggle.slint | settings row: `icon`, `text`, `description`, `checked`; `toggled(bool)` |
| `RadioRow` | toggle.slint | `checked`, `text`, `description`, `icon`; `clicked` |
| `Slider` | slider.slint | `value`, `minimum`, `maximum`, `step`, `label`; `changed(float)` while dragging, `released(float)` |
| `SegmentedTabs` | tabs.slint | `model`, `current-index`, `label`; `selected(int)` |
| `TabItem` | tabs.slint | underlined tab: `text`, `selected`, `badge`, `closable`; `clicked`, `close` |
| `Avatar` | avatar.slint | `image` or `initials` + `tint`, `size`, `status` (`Status.online/idle/dnd/offline/info`), `speaking` (green ring), `crown`, `square` (server icons) |
| `CountBadge` | badge.slint | red count bubble: `count`, `fill` |
| `LiveBadge` | badge.slint | `text` (LIVE), `large` |
| `Chip` | badge.slint | tag: `text`, `icon`, `tint`, `fill`, `outlined` |
| `Dot` | badge.slint | a status dot |
| `Card` | card.slint | panel card with optional `title`, `icon`, `subtitle`, `selected`; `inset`, `gap` |
| `SectionHeader` | card.slint | `text`, `icon`, `count`, `action` ("See All >"), `small`; `action-clicked` |
| `Divider` | card.slint | horizontal line |
| `Banner` | card.slint | warning/error strip: `text`, `icon`, `tone`, actions as @children |
| `Meter` | meter.slint | segmented level meter: `level` (dBFS), `threshold`, `show-threshold`, `active`, `segments` |
| `SpeakingBars` | meter.slint | animated "speaking" bars |
| `NavItem` | list-item.slint | navigation entry: `icon`, `text`, `subtitle`, `selected`, `badge`, `chevron`; `clicked` |
| `ListItem` | list-item.slint | row with a leading slot (@children), `title`, `subtitle`, `trailing`, `selected` |
| `PopupMenu` | overlay.slint | themed menu: `entries` ([MenuEntry]), `show(x, y)`; `activated(int)` |
| `Tooltip` | overlay.slint | wraps @children, shows `text` on hover |
| `Modal` | overlay.slint | dialog with backdrop: `title`, `subtitle`, `icon`, `card-width`, `card-height`; `dismissed` (backdrop, Escape, ×) |
| `Toast` | overlay.slint | `text`, `icon`, `timeout`, `shown` |
| `ResizeHandle` | overlay.slint | drag to resize a side panel: `size` (two-way), `minimum`, `maximum`, `left` |
| `RichText` | emoji.slint | text with inline emoji: `runs` ([TextRun]) flow and wrap in a FlexboxLayout |
| `EmojiPicker` | emoji.slint | search, categories, virtualised grid; `picked(EmojiCell)`, `close` |
| `ListView`, `ScrollView` | std-widgets | re-exported (virtualised lists) |

Shell pieces (ui/shells/): `DesktopShell` (rail + top bar + panels as
@children), `Sidebar` (page navigation above the voice card and user card),
`Panel`, `ServerRail`, `TopBar`, `MobileShell` (top bar, page, bottom
navigation), `VoiceCard`, `VoiceButtons`, `UserCard`, `HoldToTalk`.

## The server page

The server page follows the design mockups 03 to 08 (desktop). Each part
comes from the engine's events; what a gateway adds is hidden without it.

| Part | `VOELIN_OPEN` | What it shows | Engine data |
|---|---|---|---|
| Chat | `server` | Header with Pinned Messages, Topics, the voice channel and the members button; chat tabs; messages grouped by author ("Today at 10:14"), avatars, emoji, reactions with an add button, pin and topic marks, file cards with download; composer with attach, emoji and send. Opens at the newest message and follows new ones while at the end | `ChatHistory` (and `Chat` for servers without history), `AvatarReady`, `Transfer`, `Gateway` (pins; reactions arrive as stored messages); `Command::LoadOlderHistory`, `DownloadChatFile`, `UploadFile`, `GatewayRequest::React`, `Unreact`, `Pin`, `Unpin` |
| Members panel | `server` (`panel`, `no-panel`) | "Members — N" and a search; those streaming first (the people in our channel while the voice channel or a stream is shown), then each server group in the server's order with its icon, then those without a group. Rows: avatar with status, name, crown (admin groups), priority speaker, channel commander, recording, moderator role, talk power, what they do or the channel they are in. Resizable: the width is `ui.members_width` | `Presence`, `Groups`, `Talking`, `IconReady` |
| Member card | `member` | Description, groups, talk power, country; private message, poke, friend, block; volume and mute for us | `ContactsChanged`; `Command::SetContact`, `SetClientVolume`, `SetClientMuted`, `Poke` |
| Pinned messages | `pins` | In the members panel's place: cards with author, time, text, files and reactions; the pin unpins, a click jumps to the message | `Gateway` `Pins`, `Pinned`, `Unpinned`; `GatewayRequest::Pins`, `Unpin` |
| Topics | `topics`, `topic:<id>` | In the members panel's place: search, cards with the message count, creator and last activity, Create Topic; an open topic replaces the chat's messages and takes replies | `Gateway` `Topics`, `Topic`, `TopicHistory`; `GatewayRequest::Topics`, `TopicHistory`, `CreateTopic`, `Post` |
| Voice channel | `voice` | Title, topic, "5 in voice / 50 total", Voice Settings, Leave; the people as large avatars (talking ring and bars, muted, crown, streaming); the streams as cards (LIVE, viewers, kind, bitrate, sound, Watch Stream); the channel's chat | `Presence`, `Talking`, `StreamsChanged`, `Gateway` stream directory (viewer counts) |
| Watching a stream | `watch`, `popout` | The channel's header with "5 in voice" and Leave; the player with the streamer, title, viewers, LIVE, the picture's height (the simulcast picker when the streamer offers layers), volume, elapsed time, back to the chat, pop out, full screen; a note that the stream belongs to the channel; the channel's chat and a Stream Info tab. Popped out (and in full screen) it fills the window | `WatchState`, decoded frames (`src/video.rs`), `Gateway` stream directory |

The pins and topics share the place of the members panel: opening one
closes the other, and the members button brings the panel back. On the
phone the chat is the Chat tab and the members of our channel are on the
Activity tab.

## The Stream Studio

![Stream Studio](screenshots/desktop-studio-live.png)

The studio (mockups 09, 10 and the phone's 04) is a page of the main window
(the Share button's menu: "Share screen" for a quick share of a screen or
window, "Stream Studio" for the studio; while a share runs the button opens
its dialog), a window of its own (the button in its
header; closing that window brings it back) and a phone page. Its controller
is `src/studio.rs` on the engine's studio ([studio.md](studio.md)): the
studio runs while it is shown, live or recording, and a `Streamer` started
for it encodes the composite from the start, so Record Clip and recordings
work before going live.

| Part | `VOELIN_OPEN` | What it shows | Engine data |
|---|---|---|---|
| Scenes | `studio` | The scenes, the live one highlighted, with what each shows; click to switch live, double-click or the menu to rename, remove; Add Scene | `Event::Scenes`; `AddScene`, `SetActiveScene`, `RenameScene`, `RemoveScene` |
| Sources | `studio`, `studio:source`, `studio:camera` | The live scene's sources, front first: add a screen, window (on Wayland through the portal, on X11 the monitors and windows), camera, image, text or colour; show or hide; a menu with transform, crop, opacity, lock, background effect (cameras: off, blur, image, colour), forward, backward, rename, remove | `AddSource` (as `SetScenes`, with a placement), `UpdateSource`, `ReorderSource`, `RemoveSource`; per source size, rate and error from `Stats` |
| Header | `studio`, `studio:live` | "Streaming to channel • server", the connection (the viewers' bandwidth estimates against the bitrate, and the composed frame rate), resolution • fps • kbit/s, details on the info button | `Stats`, `Streamer::stats`, the stream's viewers |
| Preview | `studio`, `studio:live`, `studio:record` | The composite, LIVE and the time (or PREVIEW, REC), watching and joined viewers, title • destination | `Studio::preview` turned into pictures on a thread of its own, latest wins; `State` |
| Audio mixer | `studio`, `studio:audio` | One row per `stream.audio_sources` entry: meter, gain (dB), mute, a menu; + adds the microphone, desktop audio without Voelin, the shared window's application or an application that plays | the mixer's meters (lock-free, about 15 times a second); `Streamer::reconfigure` follows the setting |
| Stream chat | `studio` | The chat of the channel the stream goes to and a composer | the destination tab's stored messages, made into lines as the chat view does |
| Stream settings | `studio` | Destination (the voice channel of each TeamSpeak 6 server we are in), title, game, go-live message (sent to the channel when the stream is up), Show Viewer Count / Chat Overlay / Now Playing, Enable Stream Audio, Advanced Settings, output size and rate (presets or any numbers), bitrate or simulcast layers | `studio.ui`, `stream.bitrate_kbps`, `stream.layers`, `SetOutput` |
| Bottom bar | `studio`, `studio:settings` | Share Window, Share Screen, Record Clip (and start or stop a recording, the replay length, the folder), Go Live, End Stream, Studio Settings (replay seconds and memory, folder, preview size) | `SaveClip`, `StartRecording`, `StopRecording`, `studio.*`, `SetPreview`; `Command::StartStream`, `Streamer::attach`, `GoLive`, `EndStream` |
| Window of its own | `studio:window` | The same pieces, compact | — |

The studio's window has globals of its own: the controller sets the same
models (shared `VecModel`s) and the same values on both windows' globals
and wires both to the same callbacks (`bind/studio.rs`). The overlay
switches keep text sources named "Viewer count", "Chat overlay" and "Now
playing" in the live scene (they can be moved like any source). A TeamSpeak
stream has a name only, so the game goes into it after the title ("title —
game"), which is also what the gateway's directory shows. With sample data
(`VOELIN_DEMO_UI`) the studio runs from synthetic sources (the test pattern,
the synthetic camera with background blur, text, colours) and test tones,
on settings in memory, and recordings go to a temporary folder.

### Sound in the quick share

The share dialog (`share`, and `share:live` for a running share) mixes its
sound with the studio's pieces: while "Share system audio" is on (off: no
audio at all) it shows the studio's mixer rows (`MixerRow`: meter, gain,
mute, a menu) for `stream.audio_sources`, and Add opens the studio's picker
(`PickDialog`) over it: the microphone, desktop audio without Voelin, the
shared window's application, any application that plays (on Android the
launchable apps with their icons), several at once. The rows read the
main window's `StudioBridge`, filled without starting the studio
(`App::mixer_show`). Starting a share hands the sources to the capture
(`CaptureRequest::audio_sources`); while it runs the dialog shows the
same rows with the share mixer's meters (about 15 times a second), and
every change of `stream.audio_sources`, from the dialog or anywhere else,
goes to the running capture at once (`Capture::follow_audio`, which calls
`Streamer::reconfigure`). A share started without sound has no audio track
to add sources to. With sample data `share:live` shares the test pattern
with the sample studio's sources, live at once with sample viewers.

## Adding a screen

1. Write the screen in `ui/screens/<name>.slint` from the components
   (`import { ... } from "../components/index.slint";`), reading data from
   `Bridge` and navigating through `Nav` (e.g. `Nav.open-settings(...)`,
   `Nav.show(Page.home)`).
2. New data: add a struct and a property (models: `[Struct]`) and callbacks to
   `ui/bridge.slint`.
3. Place it: add a `Page` value in `ui/nav.slint` if it is a page, and in
   `ui/app.slint` put its sidebar and main parts into `DesktopShell` under
   `if Nav.page == Page.<name>:` (and a case in `MobileShell`). Dialogs go at
   the end of `MainWindow` with an `*-open` flag in `Nav`.
4. Rust: a pure view model in `src/vm/<name>.rs` (with tests) that turns
   engine state into the structs; a model in `app::Models` set on the Bridge
   once and updated with `vm::list::sync`; callbacks in `src/bind/<name>.rs`
   (call it from `bind::wire`); the logic as `impl App` in `src/<name>.rs`.
5. A development switch to open it (`dev.rs`, `VOELIN_OPEN=<name>`), a
   screenshot (below) and a line in this file.

## Settings

The UI's keys (besides the engine's): `ui` (push-to-talk, H.264, share
dialog; one JSON value), `client_playback`, and the appearance keys, applied
live when they change (settings page, `--set`, another window):

| Key | Type | Default | Effect |
|---|---|---|---|
| `ui.theme` | dark / light / system | dark | colours |
| `ui.font_scale` | number above 0 | 1.0 | every type size |
| `ui.narrow_breakpoint` | pixels | 800 | below this width the phone layout |
| `ui.image_cache_mb` | megabytes | 64 | decoded images kept in memory (0: none) |
| `ui.members_width` | pixels | 280 | width of the members panel (dragging its edge sets it) |
| `studio.ui` | JSON | see below | the Stream Studio's stream settings: `title`, `game`, `message` (go-live), `show_viewers`, `show_chat`, `show_now_playing`, `audio` (stream audio), `preview_width` (960), `preview_fps` (15) |

In a config file write them as dotted keys at the top level
(`"ui.theme" = "light"`): a `[ui]` table is read as the `ui` value.

## Emoji

The software renderer has no colour glyphs, so emoji are images. The 3720
Twemoji SVGs are packed by `scripts/pack-twemoji.py` into
`assets/twemoji.bin` (64 KiB blocks of raw deflate, an index sorted by key,
picker names and categories; 1.9 MB) and embedded in the binary. `emoji.rs`
splits chat text into grapheme clusters, maps emoji to Twemoji keys (code
points in hex; U+FE0F dropped outside ZWJ sequences, as Twemoji names its
files; text-default symbols such as © or digits only with U+FE0F) and makes
runs: one per word and one per emoji. A message without emoji stays one
`Text` (fast path); one with emoji is a `RichText`, whose runs wrap in a
`FlexboxLayout` (lines break between words, not inside them). One to three
emoji alone are shown large. `Images.emoji(key)` decodes an emoji once
(`images.rs`, LRU by bytes) when a visible row asks for it.

The picker lists the emoji without skin-tone variants by category (coarse
code-point ranges) and searches Unicode names. There are no shortcodes yet.

## Development switches and screenshots

Environment variables (see `src/dev.rs`):

- `VOELIN_DEMO_UI=1`: sample servers, channels, members, chat with emoji and
  a stream, without a server (nothing is stored).
- `VOELIN_OPEN=<what>[,<what>...]`: `home`, `server`, `settings[:voice|keybinds|streaming|privacy|appearance]`,
  `about`, `share[:live]`, `bookmark`, `emoji`, `client`, `panel`, `no-panel`,
  `voice`, `pins`, `topics`, `topic:<id>`, `member`, `watch`, `popout`
  (the server page, above), `tab:<home|servers|chat|activity|you>` (phone
  layout). With sample data, `watch` plays the local test pattern in the
  sample stream's place. `studio[:window|live|record|source|audio|camera|scene|settings]`
  opens the Stream Studio (above) in that state; with its window open a
  screenshot also saves the window alone (`<name>-window.png`) and draws it
  over the main window.
- `VOELIN_WINDOW_SIZE=390x844`: window size (phone layout below the breakpoint).
- `VOELIN_SCREENSHOT=<png>`, `VOELIN_SCREENSHOT_DELAY=<s>`: save the window and exit.
- `VOELIN_DEMO_STREAM=1`, `VOELIN_AUTOCONNECT`, `VOELIN_AUTOWATCH`,
  `VOELIN_AUTOSHARE`, `VOELIN_DATA_DIR` as before.

```sh
scripts/shots.sh shot.png settings:appearance 1440x960
# which runs, headless:
env -u WAYLAND_DISPLAY -u XDG_SESSION_TYPE SLINT_BACKEND=winit-software VOELIN_DATA_DIR=$(mktemp -d) \
VOELIN_DEMO_UI=1 VOELIN_OPEN=settings:appearance VOELIN_WINDOW_SIZE=1440x960 \
VOELIN_SCREENSHOT=shot.png VOELIN_SCREENSHOT_DELAY=4 \
xvfb-run -a -s "-screen 0 1440x960x24" target/debug/voelin
```

Remove `WAYLAND_DISPLAY` as above in a Wayland session: winit prefers
Wayland, so the window would open on the desktop instead of on the Xvfb
server (and take whatever size the compositor gives it). Remove
`XDG_SESSION_TYPE` too: with `wayland` there the push-to-talk hotkey (and
screen capture) use the desktop portal of the real session even on Xvfb,
and the desktop may ask to bind the shortcut. The script also
turns FFmpeg off (`VOELIN_FFMPEG=0`: probing hardware encoders is not
needed for pictures and has crashed in a driver), makes the screen twice
the window's size so the pointer is not over the window, and passes
`$ARGS` to the app (`ARGS="--set ui.theme=light"`). The pictures in
`docs/screenshots/` were made this way (reduced to 256 colours with
`convert -colors 256 PNG8:`).

| Server page | |
|---|---|
| ![chat and members](screenshots/desktop-server.png) | ![voice channel](screenshots/desktop-voice.png) |
| ![pinned messages](screenshots/desktop-pins.png) | ![topics](screenshots/desktop-topics.png) |
| ![a topic](screenshots/desktop-topic.png) | ![member card](screenshots/desktop-member-card.png) |
| ![watching a stream](screenshots/desktop-stream-viewer.png) | ![popped out](screenshots/desktop-popout.png) |

| Stream Studio | |
|---|---|
| ![studio](screenshots/desktop-studio.png) | ![live](screenshots/desktop-studio-live.png) |
| ![its own window](screenshots/desktop-studio-window.png) | ![the window alone](screenshots/desktop-studio-detached.png) |
| ![a source](screenshots/desktop-studio-source.png) | ![phone](screenshots/mobile-studio-details.png) |

| Desktop | |
|---|---|
| ![home](screenshots/desktop-home.png) | ![light](screenshots/desktop-light.png) |
| ![settings](screenshots/desktop-settings-voice.png) | ![appearance](screenshots/desktop-settings-appearance.png) |
| ![emoji](screenshots/desktop-emoji-picker.png) | ![volume](screenshots/desktop-client-volume.png) |
| ![share](screenshots/desktop-share-dialog.png) | ![sharing](screenshots/desktop-share-live.png) |
| ![add server](screenshots/desktop-add-server.png) | ![about](screenshots/desktop-about.png) |

| Phone layout | | | | |
|---|---|---|---|---|
| ![chat](screenshots/mobile-chat.png) | ![servers](screenshots/mobile-servers.png) | ![home](screenshots/mobile-home.png) | ![you](screenshots/mobile-you.png) | ![settings](screenshots/mobile-settings.png) |
| ![voice channel](screenshots/mobile-voice.png) | ![activity](screenshots/mobile-activity.png) | ![studio](screenshots/mobile-studio.png) | ![share](screenshots/mobile-share.png) | ![sharing](screenshots/mobile-share-live.png) |

## Limits

- No drop shadows or blur (the software renderer draws none): glows are
  translucent rings, the backdrop is a gradient rectangle.
- Avatars are the clients' pictures from the engine's cache
  (`AvatarReady`), or initials on a colour from the name; group icons come
  from `IconReady`.
- The members panel lists the people online: TeamSpeak tells a client
  nothing about offline members, so the mockup's "Offline" section and a
  server-wide member count are left out.
- The quality picker shows only when the streamer's simulcast layers are
  known; the engine does not yet tell a viewer which layers a stream has,
  so in practice the player shows the decoded picture's height.
- Popping the stream out fills the main window (no second window yet).
- A jump to a pinned message scrolls to where an average row would be
  (rows differ in height), and only to messages already loaded.
- The window keeps its native decorations (no custom title bar).
- The Stream Studio streams to the voice channel we are in (a TeamSpeak 6
  stream belongs to its channel): its destination picks among the servers,
  not among their channels. Image paths (image sources, backgrounds) are
  typed, there is no file chooser. The background effect replaces what is
  outside an oval (the engine has no person segmentation model yet). The
  studio cannot be detached on Android (one window). Its own window shows
  no toasts; status messages go to the main window.
