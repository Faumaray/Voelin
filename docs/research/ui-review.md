# UI review, and what to take from TeamSpeak 6 and Discord

**Question.** How good is the UI, what should change, and which features of
TeamSpeak 6 and Discord should Voelin port or add?

**Answer, short.** The UI is not bad. It has a real design system, two
themes, a phone layout from the same components, screen reader labels and a
Ctrl+K palette. Its problems are elsewhere:

1. The desktop shows the same things two or three times, and the app's own
   advertising takes space from the content.
2. Behaviour every TeamSpeak user expects is missing: right-click menus, drag
   and drop, moderation, channel creation, whisper, sounds.
3. Some things show wrong data: raw BBCode, raw spacer names, a password lock
   that cannot be opened.

Against TeamSpeak 6, the gap is mostly the *classic* client (admin,
whisper, sounds, chat formatting). TS6's genuinely new features Voelin
mostly has already, or beats (Android, channel history, simulcast, the
studio). Against Discord, the most useful ideas need no server support at
all: unread state, search, notification levels per channel, ducking, hotkeys,
a call bar, an overlay.

What was checked:

- The UI at `1064cce`: the Slint code, plus the screenshots in
  `docs/screenshots/` (taken 2026-10-03/04, sample data).
- TeamSpeak 6: client 6.0.0-beta4.1 and server 6.0.0-beta13.1, from their
  release notes and the TeamSpeak forum.
- Discord: its changelogs up to September 2026.

Sources are at the end.

## 1. The UI

### Keep

- `Theme` tokens for every colour and size. The light palette is complete,
  and the font scale works everywhere.
- The component catalogue (`ui/components/`) is used consistently. The
  screens look like one app.
- The phone layout is built from the same components. Its bottom navigation
  is sane: Home, Servers, Chats, Activity, You.
- About 470 `accessible-*` attributes. Tree rows have default actions and
  descriptions ("talking", "streaming", "microphone off").
- The Ctrl+K palette shows keyboard hints. The member card, the event cards,
  the pins and topics drawers and the Stream Studio are good screens.

### Wrong or broken (fix first)

| What | Where | Why it matters |
|---|---|---|
| BBCode is shown raw in chat. Only `[URL=ts3file://…]` file links are turned into cards (`src/vm/chat.rs:84`). Previews strip the tags (`src/vm/social.rs:53`) but the message view does not | `ui/screens/chat.slint` (`RichText` = text + emoji runs only) | Every official TS3/TS6 client sends `[b]`, `[url]`, `[color]` and `[img]`. The server welcome message is BBCode too (see the sample `[b]Nightfall Guild[/b]` in `src/dev.rs:428`) |
| Links in chat are not clickable | chat | People post links all day |
| Message text cannot be copied: no selection, no "Copy text" action | `MessageRow` hover actions: react, pin, topic only | Basic |
| Only `[cspacerN]` is understood. `[spacer]`, `[lspacer]`, `[rspacer]` and `[*spacer]` (repeated characters such as `[*spacer]---`) show their raw names | `src/vm/tree.rs:107` | Most community servers decorate their tree with these, so it looks broken on exactly those servers |
| A locked channel shows a lock, but joining always sends `password: None`, and nothing asks for the password | `src/bind/servers.rs:26` | The join fails with a server error and the user has no way in |
| A poke is always sent with an empty message | `src/members.rs:99` | A poke is usually *about* something |
| Invite links can be made but not opened. Voelin copies `ts3server://` links, but registers no URL handler: no `MimeType` in the `.desktop` file, no protocol in the installer, and `main.rs` does not read a link argument | packaging, `src/main.rs` | Half a feature; also blocks TS6's `tmspk.gg` invite links |
| Other clients' badges are parsed (`voelin-model/src/presence.rs:110`) but never shown | members, tree | TS6 users care about their badges |
| The priority speaker is shown with an icon, but others are not dimmed by `virtualserver_priority_speaker_dimm_modificator` (the server's dimming level for a priority speaker, −18 dB by default) | audio mixer | Every official client dims others while a priority speaker talks |
| The README and `docs/ui.md` screenshots still show the "via relay" chips, which `8deb778` removed | `docs/screenshots/` | The first impression is outdated. Regenerate them with `VOELIN_SCREENSHOT` |

### Desktop layout: too much repetition

At 1440×960 the server page has four columns: the rail (72), the sidebar
(264), the chat (~770) and the members panel (~280).

- **The same people are shown up to three times.** On the voice page you
  see who is in Chill Zone in the tree under the channel, as the large
  avatars on the stage, and again as "In Voice — 5" in the members panel.
  On the chat page you see them in the tree and in the panel. On
  TeamSpeak the tree *is* the member list.
  - Close the members panel by default whenever the tree shows clients.
    Keep it for observe-only servers, for searching, and for groups.
  - Or offer one setting, "people in the tree / in a panel" (the Discord
    model), and show them only once.
- **The top bar (56 px) adds little.** It holds a second Voelin logo, the
  search field, the bell, the members toggle and help.
  - Drop the wordmark, since the rail already has the logo.
  - Move the members toggle into the chat header, where it also is.
  - Then the bar can be lower, or merged with the chat header.
- **The same actions appear several times.**
  - The settings gear is both at the bottom of the rail and in the user
    card.
  - On Home, "Add a Server" appears **four times**: the hero button, the
    sidebar's Quick Actions, the "Your Servers" link and the right column's
    Quick Actions.
  - Quick Actions appear on both the left and the right.
  - The "Friends Online" bubbles and the "Friend Activity" column list the
    same people.
- **The app advertises itself.**
  - The Home hero is a carousel ("Voice, chat and streams on TeamSpeak
    servers. Talk. Watch. Share.").
  - The sidebar's lower half shows decorative art ("Hang out / Play
    together / Find your people") on Home, Friends and Direct Messages.
  - That is a quarter of the screen for someone who already installed the
    app. Show the hero on first run only. Use the space for "continue where
    you left off" (last server and channel, with Join), unread mentions and
    pinned DMs.
- **"Join a Server" and "Add a Server"** are two names for nearly the same
  dialog. Make it one dialog with two buttons: *Connect* and *Save*.
- **The chat header squeezes the channel topic.** "Pinned Messages" and
  "Topics" are wide labelled buttons, so the topic is cut short ("Hang out,
  chat, and explore t…"). Make them icon buttons with a tooltip and a
  count.
- **Direct messages live in two places.** The chat tabs mix the server, the
  channels and DMs (`@Kairo`, `@Mira`…), and there is also a Direct
  Messages page. Keep channels in the tabs and DMs on their page. If the
  tabs are still wanted, add "Open in a tab" as an option.
- **Channel icons disagree.** The tree draws voice channels with a speaker,
  but the chat header and the search results use `#`. On TeamSpeak every
  channel is a voice channel with a chat, so pick one icon.
- **Unlabelled numbers.** The "75" and "100" at the right of member rows
  are talk power. Label them ("TP 75"), or leave them to the member card.
- **Tree avatars are 22 px with two-letter initials** ("RI", "DE"), which
  is unreadable. Use the picture, one letter, or a coloured dot.
- **The server rail is all blue squares with initials.** Use the server's
  icon (`virtualserver_icon_id`), or a different hue per server (`Avatar`
  already has `tint`).
- **The voice page has no call bar.** Mute and deafen exist only in the
  sidebar's user card. The phone has a bottom bar (Mute, Deafen, Go Live,
  Share, Leave). Give the desktop voice page the same bar under the stage,
  with mute and deafen showing their state.
- **The voice page leaves little room for chat.** Under the stage and the
  stream card, the chat gets about 300 px. Collapse the stage into a strip
  of avatars when the chat scrolls, or add a divider that can be dragged.
- **The stream card shows a placeholder icon**, not a thumbnail of the
  stream.
- **The stream viewer:**
  - The header (streamer, title, "12 watching") is drawn straight on the
    video, so it collides with the picture. Add top and bottom gradients,
    and hide the controls after 2–3 s without mouse movement.
  - Add a theatre mode between "in the page" and "full screen" that hides
    the sidebar and the members panel.
  - Make pop-out a real window (it is documented as missing).
- **Pop-ups have no edge.** The notifications panel has no shadow and no
  scrim (the software renderer draws no shadows), so it blends into the
  page behind it; LIVE badges from behind show next to it. Give pop-ups
  `border-strong`, a darker surface, or a light scrim. Also, its fixed
  height leaves an empty area.
- **Light theme:** the server card's title is dark text on the dark banner.
  Text over pictures should be white with a scrim in both themes.
- **Older messages need a click.** Load them when the list reaches the top,
  and keep the button as a fallback.
- **Unread is not marked.** There is no "new messages" divider and no
  "jump to unread". Unread counts live in memory only.
- **Icon-only buttons.** The friend rows (message, poke, join, watch) and
  the DM header (poke, join, profile, more) need visible tooltips.

### Settings: twelve sections that overlap

- **Streaming settings are scattered.** Streaming permissions are in
  Voice & Video, Streaming and Privacy. Stream quality is in Voice & Video
  and Streaming. Devices lists the microphones, speakers and cameras that
  Voice & Video lists too. Suggested sections:
  - **Voice**: microphone, output, processing, input mode and sensitivity.
  - **Video & Streaming**: camera, quality, codec, simulcast, permissions,
    recording.
  - Make **Devices** a read-only diagnostics page, or drop it.
- **The voice activation threshold is in Keybinds**, away from the level
  meter. Discord and TS6 put the input mode and the threshold on the meter.
  `Meter` already has `threshold` and `show-threshold`.
- **"My Account" vs "Profiles" is unclear.** Call them "myTeamSpeak
  account" and "Identities"; Profiles is `identities` underneath anyway.
- **Device names are cut off and confusing.** One microphone reads "System
  default (Default ALSA Output (cu…"; it is called an Output, yet it is a
  microphone. Show the PipeWire/Pulse description, with the full name in a
  tooltip.
- **The phone shows Keybinds.** It has no keyboard in most cases.

### Phone

- **The voice screen:**
  - It has two Leave buttons, in the header and in the bottom bar.
  - Two headers repeat each other ("Voelin / Nightfall Guild", then
    "Chill Zone").
  - Below the player and the avatar strip, the chat shows about two
    messages.
  - There is a microphone button next to the composer as well as Mute in
    the bar. Say which is hold-to-talk.
- **The Servers tab names the server three times**: the top bar, the server
  pills and a banner card about 130 px tall.
- **Home:** the marketing hero takes about a quarter of the screen.
- **Untested on a device** (`docs/android.md`). Dialogs ignore the safe
  areas, and audio focus is not handled. Expect more issues on the first
  real device.

## 2. What to take from TeamSpeak 6

TS6 is still a beta: client 6.0.0-beta4.1 (2026-05-27), server
6.0.0-beta13.1 (2026-09-22). Most of what its users would miss in Voelin
is not new in TS6. It is the classic client that TS3 already had.

### 2a. Classic client features (TS3 and TS6 both have them)

| Feature | Voelin today | Notes |
|---|---|---|
| Right-click / long-press menus on channels and clients | none (click = chat or card, double-click = join) | The way TS users do everything. Channel menu: join, chat, edit, subscribe, copy link. Client menu: whisper, poke with a message, volume, mute, move, kick, ban, groups, info |
| Moving clients by drag and drop | none | Also: dragging channels to sort them |
| Kick from channel or server, ban, ban list | none (`Command` has no variant) | |
| Creating, editing and deleting channels: temporary, semi-permanent, permanent; password; max clients; codec quality; topic; description | none | Without it a user cannot even open a temporary channel, the most common TS action |
| Assigning server and channel groups, privilege keys (tokens) | none | |
| Permission editor | none | TS6 discusses a simpler "easy view"; start there |
| Server edit, "Recent server events" (log), complaints, client database, snapshots (TS6 beta4) | none | For admins; lower priority |
| Whisper lists and whisper hotkeys | incoming whispers only (`voelin-audio/src/encode.rs:90`: C2S only) | A TS signature feature: team leads whisper to the other leads |
| Asking for talk power | none (`client_talk_request` is not modelled) | Moderated channels |
| Channel description (BBCode, pictures) | none (`ChannelInfo` has the topic only) | Shown when a channel is selected |
| Server welcome message and host message in the chat | only stripped, in Home's news | Official clients print them in the Server tab, and host message mode 3 as a dialog |
| Notification sounds (sound pack): someone joined or left the channel, poke, message, connection lost, muted | none | Important for a voice app used while gaming: people do not look at the window |
| Hotkeys for mute, deafen, away, whisper, switching channel | PTT only | |
| Away with a message, auto-away when idle, mute while away | away is only shown | |
| Subscribing to channels one by one | always all channels (`voice.rs:424`) | Costs bandwidth and presence noise on large servers |
| Bookmark folders, importing the official clients' bookmarks | flat list; no import | TS6 syncs bookmarks through myTeamSpeak |
| Client info: version, platform, connected for, idle for, country flag, badges | partly (country as text) | |
| A file browser for the channel's files | none (the engine has `ListFiles`, `DeleteFiles`, `RenameFile`, `CreateDirectory`) | Only the UI is missing |
| Uploading an avatar | none (the engine has `SetAvatar`) | Only the UI is missing |
| Server and channel group icons next to names | crown, shield, chips | TS shows the group icons right of the name; servers rely on them |

### 2b. What is new in TS6

| Feature | Status in TS6 | Voelin | Recommendation |
|---|---|---|---|
| Chat editor: Markdown with live preview, toolbar, slash commands, undo; tables, code, KaTeX, Mermaid; full BBCode (beta4) | shipped | plain text | Render BBCode first: `b i u s url img color size quote code list table`. Then a composer with a formatting toolbar that writes BBCode. Check what beta4 puts on the wire for channel chat before matching its Markdown |
| YouTube inline, picture lightbox with zoom and pan, GIFs | shipped | picture overlay for `ts3file://` pictures only | Add http(s) pictures (opt-in, size-limited) and zoom/pan in the overlay |
| Several streams of a channel at once, pause, hide your own and inactive streams, auto-focus | shipped | one stream at a time (`viewer-streamer-id`) | Show a grid of the channel's streams on the voice page |
| Camera as a quick share | shipped | camera only as a Stream Studio source | Add "Camera" to the Share menu |
| Viewer limit, privacy (channel / contacts / private), join requests | shipped (fixed in beta4) | permissions plus accept/deny exist | Add the viewer limit |
| Global chat: DMs and group chats across servers, through myTeamSpeak (Matrix based, optional E2EE), with offline delivery, reply, edit, delete, reactions, pins, search | shipped | DMs need a voice connection to the peer's server | The largest interop item. Voelin already signs in to myTeamSpeak. Research the chat service (extend `research/myteamspeak.md`); it would give real DMs with official TS6 users, and streaming in DMs |
| myTeamSpeak cloud sync: bookmarks, contacts, group chats, badges | shipped | sign-in only (`docs/identity.md:239`) | Sync bookmarks and contacts first |
| Direct calls 1:1 with picture-in-picture | alpha (2026-08-20) | — | Wait; needs the global chat |
| Multi-track recording (a track per speaker) | alpha (2026-09-04) | the Studio records streams | Possible later with the Studio's recorder |
| Per-application audio for screen sharing | alpha | **already in Voelin** | — |
| Server directory and discovery, Communities (hosted servers) | shipped | — | A "Browse servers" page if the directory can be read |
| Several PTT bindings, voice activation modes (volume gate / hybrid / automatic) | shipped | one PTT key, one VAD | Several keys; an automatic threshold |
| Auto-connect to bookmarks (optionally muted), a hotkey to stream the focused application | shipped | — | Cheap |
| Custom themes (CSS extensions), custom fonts | shipped | dark/light | CSS does not apply to Slint, but every colour is a `Theme` token: an accent picker, a true-black theme, and importing token sets as JSON are cheap |
| Server snapshots from the client | shipped | — | Admin; later |
| Server-routed streaming (SFU) | announced, unreleased | — | Follow the server releases; it changes the stream negotiation |

**Where Voelin is already ahead of TS6.** Don't spend time catching up on
these; put them forward instead.

- An Android app. TS6 has no mobile client.
- Channel chat history, pins, reactions, topics and events through the
  gateway. TS6 channel chat is not persistent and has no edit, delete or
  reactions.
- Chatting in channels and seeing presence without joining.
- Simulcast layers, the Stream Studio (scenes, mixer, replay buffer),
  AV1/VP8 decoding.
- Push-to-talk on Wayland through the portal.

## 3. What to take from Discord

The TeamSpeak protocol has no message ids, so some ideas need the `tsgw`
gateway. The table says which need what:

- **client**: works on any server.
- **gateway**: needs `tsgw`. Users without a gateway still see the plain
  message.
- **TS**: maps onto a TeamSpeak feature that already exists.

| Feature | Needs | Notes |
|---|---|---|
| Read state per channel that survives restarts, a "new messages" divider, jump to unread, Esc marks read, Alt+↑/↓ and Alt+Shift+↑/↓ to move between channels and unreads | client | The store already keeps history; keep a "last read" per chat |
| Searching messages (`from:`, `in:`, `has:file`, before and after a date) | client | SQLite FTS5 over the stored history |
| Notification level per server and channel (all / mentions / nothing), mute for 1 h / 8 h / until switched back | client | Today the levels are per kind only |
| A Mentions tab in the bell, mentions highlighted in the chat, @ autocomplete in the composer | client | Mentions are already detected (`vm/social.rs:104`) |
| Typing indicator in DMs | client / TS | `clientchatcomposing` is declared and unused |
| Global mute and deafen hotkeys (Ctrl+Shift+M / D), a shortcut sheet on Ctrl+/, mouse back and forward | client | |
| Ducking: other apps get quieter while someone talks | client | Windows session volume; PipeWire stream volume |
| A call bar on the voice page | client | See §1 |
| Overlay: who is talking and our mute state, as a transparent always-on-top window that does not hook into games | client | Discord rebuilt its overlay this way in 2025 |
| Streamer mode: hide addresses, unique ids and server passwords, on its own when the Studio is live or OBS runs | client | Home shows `127.0.0.1:9987` today |
| Compact message layout, UI density, a true-black theme, an accent colour | client | Tokens make this cheap |
| Soundboard: short clips mixed into *our own* voice | client | Everyone hears it on any server, since it is just our voice. The gateway could share a server's sounds; respect talk power |
| Voice clips: "save the last 30 s" of the channel | client | The Studio has a replay buffer; ask for consent and show the recording flag |
| Zoom and pan while watching, picture-in-picture, a real pop-out window | client | Voelin already shows the stream statistics |
| "Ask to stream" (a request to someone to share their screen) | client | A poke with a fixed message, or a gateway request |
| Voice invite links with a preview of who is inside | gateway | `ts3server://` plus a page served by the gateway |
| How long a channel has been busy, its topic in the tree | client / TS | The channel topic exists |
| Stage-style view of moderated channels: speakers vs audience, "raise hand" | TS | TS has moderated channels and talk power requests; present them the Discord way |
| Priority speaker lowers the others | TS | See §1: the server already sends the dimming level |
| Replies | gateway | Others see a `> quote`-style prefix |
| Edit and delete | gateway | Only gateway users see the change; say so in the UI |
| Polls | gateway | Next to events and topics |
| Forwarding | client | Post again with "forwarded from" |
| Bookmarks and reminders for messages | client | Local |
| Link previews (OpenGraph), http pictures inline | client | Opt-in: fetching leaks the IP to the linked site |
| Onboarding and rules screening on a server | gateway | Later |
| Roles UI ("view as role") | TS | Maps onto server groups; after the permission editor |

**Not worth copying.**

- Nitro, boosts, quests: no equivalent.
- Activities: embedded web apps, and Slint has no web view.
- E2EE voice: the TS protocol cannot do it.
- Server discovery: TS6 has its own directory, see §2b.

## 4. Order of work

| # | Work | Size |
|---|---|---|
| 1 | BBCode rendering, clickable links, Copy text | S–M |
| 2 | Spacers, channel password prompt, poke with a message, badges, priority-speaker dimming | S |
| 3 | Regenerate the screenshots | S |
| 4 | Context menus and drag and drop. Then moderation: move, kick, ban; create, edit and delete channels; assign groups | L |
| 5 | Notification sounds; mute, deafen and away hotkeys; auto-away | M |
| 6 | Unread state, "new messages" divider, loading older messages on scroll, mention highlight and autocomplete | M |
| 7 | Desktop layout: members panel closed by default when the tree shows people, no duplicate actions or advertising, call bar, viewer scrim and theatre mode | M |
| 8 | `ts3server://` and `tmspk.gg` link handler | S–M |
| 9 | Sending whispers, asking for talk power, per-channel subscription | M |
| 10 | Message search (FTS5), notification level per server and channel | M |
| 11 | Merge the settings sections; tray, minimise to tray, autostart; streamer mode | M |

Later: the myTeamSpeak global chat and cloud sync, the overlay, the
soundboard, the stream grid and camera share, direct calls, the permission
editor, translations (the `.pot` exists, no `.po` yet).

## Sources

TeamSpeak 6:

- beta1: https://community.teamspeak.com/t/teamspeak-6-0-0-beta1-screen-camera-sharing-communities-design-overhaul/54925
- beta3 "Foundation I": https://community.teamspeak.com/t/teamspeak-6-0-0-beta3-foundation-i-update/62417
- beta3.3: https://community.teamspeak.com/t/teamspeak-6-0-0-beta3-3-minor-update/62656
- beta4 "Foundation II": https://community.teamspeak.com/t/teamspeak-6-0-0-beta4-foundation-ii-update/64497
- beta4.1: https://community.teamspeak.com/t/teamspeak-6-0-0-beta4-1-hotfix/64672
- Direct calls: https://community.teamspeak.com/t/first-look-at-1-1-direct-calls-for-teamspeak-6/65124
- Per-application audio: https://community.teamspeak.com/t/multi-app-audio-picker-for-screensharing/65236
- Multi-track recording: https://community.teamspeak.com/t/multi-track-audio-recording/65237
- Server beta13: https://community.teamspeak.com/t/ts6-server-beta-v6-0-0-beta13/65349
- Server repository: https://github.com/teamspeak/teamspeak6-server
- Channel chat is not persistent: https://community.teamspeak.com/t/cant-delete-messages-or-pictures-posted-in-ts6-server-channels/63343
- Staff on persistent text channels: https://community.teamspeak.com/t/subject-making-ts6-the-ultimate-discord-alternative-feedback-suggestions/63483/26
- What myTeamSpeak syncs: https://community.teamspeak.com/t/feedback-add-indicator-what-settings-are-synced-through-myts/56152

Discord:

- https://discord.com/blog/discord-update-march-25-2025-changelog (call bar, overlay rebuild, density)
- https://discord.com/blog/discord-update-september-25-2025-changelog
- https://discord.com/blog/discord-patch-notes-february-4-2026 (stream zoom/pan, stats)
- https://discord.com/blog/discord-patch-notes-june-4-2026 (clips, voice invites)
- https://discord.com/blog/discord-patch-notes-july-7-2026 (Request to Stream)
- https://discord.com/blog/discord-update-september-25-2026
- https://support.discord.com/hc/en-us/articles/22163184112407-Polls-FAQ
- https://support.discord.com/hc/en-us/articles/26442819646999-Message-Bookmarks-and-Reminders
- https://discord.com/blog/improving-mobile-with-squircles-styles-and-spacing
