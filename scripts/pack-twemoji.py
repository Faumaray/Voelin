#!/usr/bin/env python3
"""Pack the Twemoji SVGs into one compressed archive for the UI.

Colour emoji cannot go through the software renderer's text path, so the UI
shows them as images: `crates/voelin-ui/src/emoji.rs` reads this archive
(embedded in the binary) and decodes single emoji on demand.

Usage:
    scripts/pack-twemoji.py <twemoji package dir> [out]

The package is @twemoji/svg (15.0.0: `npm pack @twemoji/svg`, unpacked); the
directory holds the SVGs named by code points (`1f600.svg`,
`1f468-200d-1f4bb.svg`). `out` defaults to
crates/voelin-ui/assets/twemoji.bin. Only the Python standard library is
needed; names come from its `unicodedata` plus a table for the emoji newer
than it (Unicode 15), so any Python 3.11+ gives the same file.

Format (little-endian):
    magic         b"VTWEMOJ1"
    u32 entries, u32 blocks, u32 strings_len
    blocks        [u32 offset, u32 compressed_len, u32 raw_len]; offset
                  from the start of the data section
    entries       sorted by key: [u32 key_off, u16 key_len, u16 category,
                  u32 block, u32 offset, u32 len, u32 name_off, u16 name_len,
                  u16 flags]; flags bit 0: shown in the picker
    strings       keys and names (UTF-8, lower case)
    data          raw deflate blocks of about BLOCK bytes of SVG each, in
                  picker order, so a category decodes few blocks
"""

import os
import struct
import sys
import unicodedata
import zlib

BLOCK = 64 * 1024

# Picker categories, in order; the index is stored per entry.
CATEGORIES = [
	"smileys",
	"people",
	"nature",
	"food",
	"activities",
	"travel",
	"objects",
	"symbols",
	"flags",
]

# First code point → category, first match wins (a coarse grouping; the
# picker also searches by name).
RANGES = [
	((0x1F1E6, 0x1F1FF), "flags"),
	((0x1F3F3, 0x1F3F4), "flags"),
	((0x1F6A9, 0x1F6A9), "flags"),
	((0x1F38C, 0x1F38C), "flags"),
	((0x1F600, 0x1F644), "smileys"),
	((0x1F645, 0x1F64F), "people"),
	((0x1F910, 0x1F917), "smileys"),
	((0x1F920, 0x1F925), "smileys"),
	((0x1F927, 0x1F92F), "smileys"),
	((0x1F970, 0x1F97A), "smileys"),
	((0x1F9D0, 0x1F9D0), "smileys"),
	((0x1FAE0, 0x1FAE8), "smileys"),
	((0x1F479, 0x1F480), "smileys"),
	((0x1F4A9, 0x1F4A9), "smileys"),
	((0x1F48B, 0x1F48B), "smileys"),
	((0x1F493, 0x1F49F), "symbols"),
	((0x2764, 0x2764), "symbols"),
	((0x1F90D, 0x1F90E), "symbols"),
	((0x1F9E1, 0x1F9E1), "symbols"),
	((0x1FA75, 0x1FA77), "symbols"),
	((0x263A, 0x263A), "smileys"),
	((0x2639, 0x2639), "smileys"),
	((0x1F440, 0x1F450), "people"),
	((0x1F466, 0x1F487), "people"),
	((0x1F48F, 0x1F491), "people"),
	((0x1F574, 0x1F57A), "people"),
	((0x1F590, 0x1F596), "people"),
	((0x1F6B4, 0x1F6B6), "people"),
	((0x1F6C0, 0x1F6C0), "people"),
	((0x1F6CC, 0x1F6CC), "people"),
	((0x1F90C, 0x1F90F), "people"),
	((0x1F918, 0x1F91F), "people"),
	((0x1F926, 0x1F926), "people"),
	((0x1F930, 0x1F93E), "people"),
	((0x1F9B0, 0x1F9B9), "people"),
	((0x1F9BB, 0x1F9BB), "people"),
	((0x1F9CD, 0x1F9CF), "people"),
	((0x1F9D1, 0x1F9DF), "people"),
	((0x1FAC3, 0x1FAC5), "people"),
	((0x1FAF0, 0x1FAF8), "people"),
	((0x261D, 0x261D), "people"),
	((0x26F9, 0x26F9), "people"),
	((0x270A, 0x270D), "people"),
	((0x1F3C2, 0x1F3C4), "people"),
	((0x1F3C7, 0x1F3C7), "people"),
	((0x1F3CA, 0x1F3CC), "people"),
	((0x1F463, 0x1F463), "people"),
	((0x1F5E3, 0x1F5E3), "people"),
	((0x1F464, 0x1F465), "people"),
	((0x1FAC2, 0x1FAC2), "people"),
	((0x1F9E0, 0x1F9E0), "people"),
	((0x1FAC0, 0x1FAC1), "people"),
	((0x1F400, 0x1F43F), "nature"),
	((0x1F330, 0x1F344), "nature"),
	((0x1F490, 0x1F490), "nature"),
	((0x1F4AE, 0x1F4AE), "nature"),
	((0x1F940, 0x1F940), "nature"),
	((0x1F980, 0x1F9AE), "nature"),
	((0x1FAB0, 0x1FABF), "nature"),
	((0x1FACE, 0x1FACF), "nature"),
	((0x1F54A, 0x1F54A), "nature"),
	((0x1F577, 0x1F578), "nature"),
	((0x1F308, 0x1F308), "travel"),
	((0x2618, 0x2618), "nature"),
	((0x1F345, 0x1F37F), "food"),
	((0x1F942, 0x1F96F), "food"),
	((0x1F9C0, 0x1F9CB), "food"),
	((0x1FAD0, 0x1FADB), "food"),
	((0x2615, 0x2615), "food"),
	((0x1F380, 0x1F393), "activities"),
	((0x1F396, 0x1F3C1), "activities"),
	((0x1F3C5, 0x1F3C6), "activities"),
	((0x1F3C8, 0x1F3C9), "activities"),
	((0x1F3CD, 0x1F3F0), "activities"),
	((0x1F3F5, 0x1F3FA), "activities"),
	((0x1F93F, 0x1F93F), "activities"),
	((0x1F945, 0x1F94F), "activities"),
	((0x1F941, 0x1F941), "activities"),
	((0x1F9E9, 0x1F9E9), "activities"),
	((0x1FA80, 0x1FA88), "activities"),
	((0x26BD, 0x26BE), "activities"),
	((0x26F3, 0x26F3), "activities"),
	((0x26F8, 0x26F8), "activities"),
	((0x265F, 0x265F), "activities"),
	((0x1F0CF, 0x1F0CF), "activities"),
	((0x1F004, 0x1F004), "activities"),
	((0x1F680, 0x1F6FF), "travel"),
	((0x1F300, 0x1F32F), "travel"),
	((0x1F5FA, 0x1F5FF), "travel"),
	((0x2600, 0x2604), "travel"),
	((0x26C4, 0x26C8), "travel"),
	((0x26E9, 0x26FA), "travel"),
	((0x26FD, 0x26FD), "travel"),
	((0x2708, 0x2708), "travel"),
	((0x2744, 0x2744), "travel"),
	((0x1F3D4, 0x1F3DF), "travel"),
	((0x2693, 0x2693), "travel"),
	((0x231A, 0x231B), "objects"),
	((0x23F0, 0x23F3), "objects"),
	((0x1F4A0, 0x1F4FF), "objects"),
	((0x1F500, 0x1F53D), "symbols"),
	((0x1F549, 0x1F573), "objects"),
	((0x1F57B, 0x1F5F9), "objects"),
	((0x1F9E2, 0x1F9FF), "objects"),
	((0x1FA70, 0x1FA7F), "objects"),
	((0x1FA90, 0x1FAAF), "objects"),
	((0x260E, 0x260E), "objects"),
	((0x2692, 0x2699), "objects"),
	((0x26CF, 0x26D4), "objects"),
	((0x2702, 0x2712), "objects"),
	((0x1F3FB, 0x1F3FF), "people"),
]

# Emoji newer than Python 3.11's Unicode database (14.0).
EXTRA_NAMES = {
	0x1F6DC: "wireless",
	0x1FA75: "light blue heart",
	0x1FA76: "grey heart",
	0x1FA77: "pink heart",
	0x1FA87: "maracas",
	0x1FA88: "flute",
	0x1FAAD: "folding hand fan",
	0x1FAAE: "hair pick",
	0x1FAAF: "khanda",
	0x1FABB: "hyacinth",
	0x1FABC: "jellyfish",
	0x1FABD: "wing",
	0x1FABF: "goose",
	0x1FACE: "moose",
	0x1FACF: "donkey",
	0x1FADA: "ginger root",
	0x1FADB: "pea pod",
	0x1FAE8: "shaking face",
	0x1FAF7: "leftwards pushing hand",
	0x1FAF8: "rightwards pushing hand",
}

# Code points that are not part of a name.
SILENT = {0x200D, 0xFE0F, 0x20E3}
SKIN_TONES = range(0x1F3FB, 0x1F400)


def cp_name(cp):
	if cp in EXTRA_NAMES:
		return EXTRA_NAMES[cp]
	if 0x1F1E6 <= cp <= 0x1F1FF:
		return chr(cp - 0x1F1E6 + ord("a"))
	if 0xE0020 <= cp <= 0xE007F:
		return ""
	return unicodedata.name(chr(cp), "").lower()


def entry_name(cps):
	if all(0x1F1E6 <= cp <= 0x1F1FF for cp in cps):
		return "flag " + "".join(cp_name(cp) for cp in cps)
	parts = [cp_name(cp) for cp in cps if cp not in SILENT]
	return " ".join(p for p in parts if p).replace("emoji modifier fitzpatrick type-", "skin tone ")


def category(cps):
	if all(0x1F1E6 <= cp <= 0x1F1FF for cp in cps) and len(cps) == 2:
		return "flags"
	first = cps[0]
	# Keycaps and other text symbols.
	if first < 0x2000 or (0x20E3 in cps):
		return "symbols"
	for (lo, hi), cat in RANGES:
		if lo <= first <= hi:
			return cat
	return "symbols"


def in_picker(cps):
	if any(cp in SKIN_TONES for cp in cps):
		return False
	# Single regional indicators and tags are building blocks.
	if len(cps) == 1 and 0x1F1E6 <= cps[0] <= 0x1F1FF:
		return False
	return True


def main():
	if len(sys.argv) < 2:
		print(__doc__, file=sys.stderr)
		sys.exit(2)
	src = sys.argv[1]
	root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
	out = sys.argv[2] if len(sys.argv) > 2 else os.path.join(root, "crates/voelin-ui/assets/twemoji.bin")
	items = []
	for file in os.listdir(src):
		if not file.endswith(".svg"):
			continue
		key = file[:-4]
		cps = [int(p, 16) for p in key.split("-")]
		with open(os.path.join(src, file), "rb") as f:
			svg = f.read().strip()
		cat = category(cps)
		items.append({
			"key": key,
			"cps": cps,
			"svg": svg,
			"cat": CATEGORIES.index(cat),
			"name": entry_name(cps),
			"picker": in_picker(cps) and entry_name(cps) != "",
		})
	# Data in picker order: category, then code points.
	items.sort(key=lambda it: (it["cat"], it["cps"]))
	blocks = []
	raw = bytearray()
	for it in items:
		if len(raw) >= BLOCK:
			blocks.append(bytes(raw))
			raw = bytearray()
		it["block"] = len(blocks)
		it["offset"] = len(raw)
		raw += it["svg"]
	if raw:
		blocks.append(bytes(raw))

	strings = bytearray()
	for it in items:
		it["key_off"] = len(strings)
		strings += it["key"].encode()
		it["name_off"] = len(strings)
		strings += it["name"].encode()

	data = bytearray()
	block_table = bytearray()
	for b in blocks:
		c = zlib.compressobj(9, zlib.DEFLATED, -15)
		comp = c.compress(b) + c.flush()
		block_table += struct.pack("<III", len(data), len(comp), len(b))
		data += comp

	entries = bytearray()
	for it in sorted(items, key=lambda it: it["key"]):
		entries += struct.pack(
			"<IHHIIIIHH",
			it["key_off"],
			len(it["key"]),
			it["cat"],
			it["block"],
			it["offset"],
			len(it["svg"]),
			it["name_off"],
			len(it["name"].encode()),
			1 if it["picker"] else 0,
		)

	with open(out, "wb") as f:
		f.write(b"VTWEMOJ1")
		f.write(struct.pack("<III", len(items), len(blocks), len(strings)))
		f.write(block_table)
		f.write(entries)
		f.write(strings)
		f.write(data)
	size = os.path.getsize(out)
	print(f"wrote {out}: {len(items)} emoji, {len(blocks)} blocks, {size} bytes")


if __name__ == "__main__":
	main()
