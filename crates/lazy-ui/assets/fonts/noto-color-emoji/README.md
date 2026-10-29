Noto Color Emoji 2.057 (Google), licensed under the SIL Open Font License 1.1; see `LICENSE.txt`.
The copyright and license notices inside the font are unchanged, and the font declares no Reserved
Font Name.

This is a **modified** copy. The only change is one added `cmap` entry mapping U+006D (`m`) to the
font's space glyph, made by `patch_cmap.py` (run it on the upstream `NotoColorEmoji.ttf`). GPUI's
text system accepts a font as a primary family only if it maps `m`, and an emoji font has no Latin
letters, so the upstream file can never be requested by name. The family and PostScript names
(`NotoColorEmoji`, which GPUI also uses to draw color bitmaps) are unchanged. No glyph or bitmap
is touched.

Why it is bundled at all, and how it is used: `lazy-ui/src/emoji_font.rs`.
