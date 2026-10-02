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
    components/          the design system (catalogue below); index.slint exports all
    shells/              desktop.slint (rail, top bar, Sidebar), mobile.slint (bottom
                         navigation), common.slint (Panel, VoiceCard, UserCard, ...)
    screens/             channels, chat, streams (panel and viewer), settings, home,
                         dialogs, mobile (phone-only pages)
    assets/              fonts/ (Inter, OFL), icons/ (Lucide, ISC), logo.svg
  assets/twemoji.bin     the Twemoji SVGs packed by scripts/pack-twemoji.py
  src/
    app.rs               setup, App state, event dispatch
    servers.rs chat.rs streams.rs settings_page.rs appearance.rs
                         the logic of each area
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
| Sizes | `row-height`, `control-height` 36, `control-height-sm` 28, `icon-sm/md/lg` 16/20/24, `rail-width` 72, `sidebar-width` 264 (grows with the font scale), `topbar-height` 56, `right-panel-min/max` |
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
  `about`, `share`, `bookmark`, `emoji`, `client`, `panel`, `no-panel`,
  `tab:<home|servers|chat|activity|you>` (phone layout).
- `VOELIN_WINDOW_SIZE=390x844`: window size (phone layout below the breakpoint).
- `VOELIN_SCREENSHOT=<png>`, `VOELIN_SCREENSHOT_DELAY=<s>`: save the window and exit.
- `VOELIN_DEMO_STREAM=1`, `VOELIN_AUTOCONNECT`, `VOELIN_AUTOWATCH`,
  `VOELIN_AUTOSHARE`, `VOELIN_DATA_DIR` as before.

```sh
scripts/shots.sh shot.png settings:appearance 1440x960
# which runs, headless:
env -u WAYLAND_DISPLAY SLINT_BACKEND=winit-software VOELIN_DATA_DIR=$(mktemp -d) \
VOELIN_DEMO_UI=1 VOELIN_OPEN=settings:appearance VOELIN_WINDOW_SIZE=1440x960 \
VOELIN_SCREENSHOT=shot.png VOELIN_SCREENSHOT_DELAY=4 \
xvfb-run -a -s "-screen 0 1440x960x24" target/debug/voelin
```

Remove `WAYLAND_DISPLAY` as above in a Wayland session: winit prefers
Wayland, so the window would open on the desktop instead of on the Xvfb
server (and take whatever size the compositor gives it). The pictures in
`docs/screenshots/` were made this way (reduced to 256 colours with
`convert -colors 256 PNG8:`).

| Desktop | |
|---|---|
| ![home](screenshots/desktop-home.png) | ![viewer](screenshots/desktop-stream-viewer.png) |
| ![settings](screenshots/desktop-settings-voice.png) | ![appearance](screenshots/desktop-settings-appearance.png) |
| ![emoji](screenshots/desktop-emoji-picker.png) | ![light](screenshots/desktop-light.png) |
| ![share](screenshots/desktop-share-dialog.png) | ![add server](screenshots/desktop-add-server.png) |
| ![about](screenshots/desktop-about.png) | ![volume](screenshots/desktop-client-volume.png) |

| Phone layout | | | | |
|---|---|---|---|---|
| ![chat](screenshots/mobile-chat.png) | ![servers](screenshots/mobile-servers.png) | ![home](screenshots/mobile-home.png) | ![you](screenshots/mobile-you.png) | ![settings](screenshots/mobile-settings.png) |

## Limits

- No drop shadows or blur (the software renderer draws none): glows are
  translucent rings, the backdrop is a gradient rectangle.
- Avatars are initials on a colour from the name; the image cache is ready
  for pictures (server icons, avatars) once the engine provides them.
- The window keeps its native decorations (no custom title bar).
