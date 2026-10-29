#!/usr/bin/env python3
"""Adds one cmap entry (U+006D -> the space glyph) to Noto Color Emoji.

GPUI's text system only accepts a font as a primary family if it maps 'm' (it measures with it),
and its emoji fonts carry no Latin letters, so an unmodified Noto Color Emoji can never be asked
for by name. Nothing else in the font changes.
"""
import struct, sys

src, dst = sys.argv[1], sys.argv[2]
data = bytearray(open(src, 'rb').read())

num_tables = struct.unpack('>H', data[4:6])[0]
dirs = []
for i in range(num_tables):
    tag, checksum, offset, length = struct.unpack('>4sIII', data[12 + 16 * i:28 + 16 * i])
    dirs.append([tag, checksum, offset, length])
by_tag = {d[0]: d for d in dirs}
cmap = by_tag[b'cmap']
cmap_off, cmap_len = cmap[2], cmap[3]

# Find the format 12 subtable.
version, count = struct.unpack('>HH', data[cmap_off:cmap_off + 4])
sub_off = None
for i in range(count):
    pid, eid, so = struct.unpack('>HHI', data[cmap_off + 4 + 8 * i:cmap_off + 12 + 8 * i])
    if struct.unpack('>H', data[cmap_off + so:cmap_off + so + 2])[0] == 12:
        sub_off = cmap_off + so
assert sub_off is not None
_, _, sub_len, lang, ngroups = struct.unpack('>HHIII', data[sub_off:sub_off + 16])
groups = [list(struct.unpack('>III', data[sub_off + 16 + 12 * g:sub_off + 28 + 12 * g])) for g in range(ngroups)]
space_glyph = next(g[2] for g in groups if g[0] == 32 and g[1] == 32)
assert not any(g[0] <= 109 <= g[1] for g in groups), "m is already mapped"
groups.append([109, 109, space_glyph])
groups.sort()
assert sub_off + sub_len == cmap_off + cmap_len, "the format 12 subtable must end the cmap table"
new_sub = struct.pack('>HHIII', 12, 0, 16 + 12 * len(groups), lang, len(groups)) + b''.join(
    struct.pack('>III', *g) for g in groups)
new_cmap = bytes(data[cmap_off:sub_off - 0]) [: sub_off - cmap_off] + new_sub
growth = len(new_cmap) - cmap_len

# Rebuild the file: tables keep their order in the file; later ones shift.
tables = sorted(dirs, key=lambda d: d[2])
out = bytearray(data[:tables[0][2]])
new_offsets = {}
cursor = len(out)
for tag, checksum, offset, length in tables:
    body = bytes(data[offset:offset + length]) if tag != b'cmap' else new_cmap
    new_offsets[tag] = (cursor, len(body))
    out += body
    pad = (-len(out)) % 4
    out += b'\0' * pad
    cursor = len(out)

def table_checksum(b):
    b = b + b'\0' * ((-len(b)) % 4)
    return sum(struct.unpack('>%dI' % (len(b) // 4), b)) & 0xFFFFFFFF

for i, (tag, checksum, offset, length) in enumerate(dirs):
    new_off, new_len = new_offsets[tag]
    body = bytes(out[new_off:new_off + new_len])
    if tag == b'head':
        body = body[:8] + b'\0\0\0\0' + body[12:]
    struct.pack_into('>4sIII', out, 12 + 16 * i, tag, table_checksum(body), new_off, new_len)

# head.checkSumAdjustment
head_off = new_offsets[b'head'][0]
struct.pack_into('>I', out, head_off + 8, 0)
adjust = (0xB1B0AFBA - table_checksum(bytes(out))) & 0xFFFFFFFF
struct.pack_into('>I', out, head_off + 8, adjust)
open(dst, 'wb').write(out)
print(f"wrote {dst}: {len(out)} bytes (cmap grew {growth})")
