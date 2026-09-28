# Bundled assets

- `twemoji.bin`: the Twemoji 15.0 SVGs (3720 emoji, from the `@twemoji/svg`
  15.0.0 package) packed by `scripts/pack-twemoji.py` into blocks of raw
  deflate with an index; `src/emoji.rs` reads it. Twemoji graphics by Twitter,
  Inc and other contributors, licensed under CC-BY 4.0
  (https://creativecommons.org/licenses/by/4.0/). The files are unchanged
  apart from surrounding whitespace.

The fonts (Inter, SIL OFL 1.1) and icons (Lucide, ISC) that the Slint files
import are under `ui/assets/`, each with its license. All three are credited
in `THIRD_PARTY_NOTICES.md` and the About page.
