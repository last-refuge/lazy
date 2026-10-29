# Lazy asset guide

All current assets belong to the selected **Soft Signal** identity. The directory is portable; keep relative paths when copying it. The versioned exploration directory and five-direction ZIP are historical material.

## Logos

| Folder | Format / colors | Use |
| --- | --- | --- |
| `logos/horizontal/` | SVG; Forest, Paper, Sage, black, white | Primary lockup |
| `logos/stacked/` | SVG; same five colors | Covers and square/portrait compositions |
| `logos/wordmark/` | SVG; same five colors | Lettering alone where Lazy is already established |
| `logos/symbol/` | SVG; same five colors | Standard mark at 24 px and above |
| `logos/symbol/lazy-symbol-small-forest.svg` | SVG, reduced-detail face | 16–23 px use |

Horizontal transparent PNGs are provided at 600 and 1200 px wide. Stacked PNGs are 600 px wide. Wordmark PNGs are 600 px wide. Symbol PNGs are 512 px square. Black/white wordmarks are real solid color with transparent cutouts; the paper variant is the preferred warm reverse. All lettering is outlined. SVGs include masks for negative space and descriptive titles; use an external `<img>` or import with unique IDs when inlining multiple copies.

## Icons

- `icons/lazy-icon-forest-rounded.svg`: standard marketing/desktop tile.
- `icons/lazy-icon-sage-rounded.svg`: alternate Sage tile.
- `icons/lazy-icon-forest-square.svg`: full-bleed square foundation for platform masking.
- `icons/lazy-icon-forest-maskable.svg`: conservative central mark for PWA masks.
- `icons/favicon.svg`: reduced-detail Forest browser icon.
- `icons/favicon-16.png`, `favicon-32.png`, `favicon-48.png`, `favicon.ico`: browser fallbacks.
- `icons/apple-touch-icon.png`: 180 × 180 full-bleed square; the platform applies corner masking.
- `icons/lazy-app-16.png` through `lazy-app-1024.png`: transparent outside the rounded tile, for desktop launchers and general use.
- `icons/lazy-app.ico`: embedded 16, 24, 32, 48, 64, 128 and 256 px Windows icon images.
- `icons/lazy-app.icns`: macOS icon container, exported from the 1024 px raster.
- `icons/android-chrome-192.png`, `android-chrome-512.png`: standard PWA icons.
- `icons/maskable-512.png`: full-bleed safe-area version.
- `icons/site.webmanifest`: portable icon metadata, with icon paths relative to the manifest.

Desktop SVG and raster icons are ready for integration into packaging. They do not change an installer or application's configured icon until those consuming files are updated. The ICNS is a supplied artwork container; native platform presentation should still be reviewed when packaged.

## Moss

Four transparent 1254 × 1254 PNG masters: `moss-primary`, `moss-welcome`, `moss-caught-up`, `moss-celebrate`. Each has a full-size WebP and 256/512 px WebP variants. Alpha is preserved. Use the full or 512 px version for a 2× render of a typical 160–256 CSS px illustration.

For a purely decorative pose next to explanatory text, set `alt=""`. For an introduction, use a concise description such as “Moss, Lazy’s paper-cut sloth, waving hello”. Avoid cropping hands, face or the celebration leaves.

## Illustrations and patterns

`illustrations/conversation-canopy.png` is the unaltered 1536 × 1024 hero master. `conversation-canopy-480.webp`, `-960.webp` and `-1536.webp` provide responsive web sources without upscaling. `conversation-leaves-pattern.svg` is a lightweight vector motif designed to repeat at 480 × 320.

## Social

- Light/dark social cards: `social/lazy-open-graph-light.png` and `social/lazy-open-graph-dark.png`, both 1200 × 630.
- Square introduction: `social/lazy-square.png`, 1080 × 1080.
- Profile avatar: `social/lazy-profile.png`, 400 × 400.
- `social/metadata-example.html`: copy-ready metadata paths. Replace the marked deployment URL before publishing it.

The social files are rendered compositions of real SVG logo art, live brand text and approved illustrations. Edit `source/social-templates.html` and rerun the exporter to change them.

## Fonts and licensing

Nunito: `fonts/nunito/Nunito-Variable.ttf` and `.woff2`, plus `OFL.txt`.

DM Sans: `fonts/dmsans/DMSans-Variable.ttf` and `.woff2`, plus `OFL.txt`.

The files were sourced from [Google Fonts’ Nunito directory](https://github.com/google/fonts/tree/main/ofl/nunito) and [DM Sans directory](https://github.com/google/fonts/tree/main/ofl/dmsans). Both families include their upstream SIL Open Font License. Preserve those files with redistribution. The TTFs are suitable for design-tool installation; WOFF2 files serve the website. The bundled families are upright variable fonts, not an italic family.

## Source, verification and packaging

The approved SVG source is retained in `source/approved-logo.svg` and `source/approved-symbol.svg`. `source/generation-manifest.json` contains the original and additional image-generation prompts and references. Generated PNG masters were copied without visual alteration; WebP variants are resized/encoded derivatives. Native SVG variants, the pattern and HTML compositions are deterministic build outputs.

The Python build needs Pillow. The exporter needs Node.js, Sharp, Playwright and Chromium; the packaging script uses the Python standard library. The installed runtime paths are selected through environment variables or the bundled defaults documented in the scripts. No network is required to rebuild with those dependencies present.

```sh
python branding/source/build_brand.py
node branding/source/export_brand.cjs
python branding/source/package_brand.py
```

`export_brand.cjs` verifies local links, image/font loading, theme interaction and layout at 320, 390, 768 and 1360 px. It renders preview images and the visual PDF. `package_brand.py` records file sizes, dimensions and SHA-256 digests, then creates `lazy-soft-signal-brand-kit.zip` and validates its ZIP structure. Historical explorations and earlier ZIPs are excluded from the selected kit.

The manifest excludes itself and the ZIP to avoid circular hashes. Large editable PNG masters are intentionally included in the ZIP. Keep this directory as the canonical source and export platform-specific copies into application packaging only when integrating the brand.
