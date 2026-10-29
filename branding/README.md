# Lazy branding

**Soft Signal is the selected Lazy identity.** Version 1.0.0, approved October 29, 2026.

Lazy is a native workplace chat app. Its brand is warm, direct and attentive: good work, with easy conversation. The product name is **Lazy**; Soft Signal is the internal name of this visual direction. The mascot is **Moss**.

## Start here

- [Visual brand library](index.html): logos, colors, typography, Moss, component examples, light/dark preview and download links. Open locally; all resources are included.
- [Brand guidelines](BRAND-GUIDELINES.md): positioning, identity, logo rules, palette, typography, illustration and mascot use.
- [Website and product style guide](STYLE-GUIDE.md): layout, themes, UI states, accessibility, implementation and copy-ready examples.
- [Voice and copy](VOICE-AND-COPY.md): tone, naming and reusable messages.
- [Asset guide](ASSETS.md): which file to use, dimensions, export conventions and font licenses.
- [Machine-readable asset inventory](asset-manifest.json): file paths, dimensions, byte counts and SHA-256 digests.
- [Color contrast report](styles/contrast-report.json): 44 verified text, button, control and focus pairs across light and dark themes.

## Folder map

```text
branding/
  README.md                     Selected identity and handoff index
  BRAND-GUIDELINES.md            Brand identity and usage rules
  STYLE-GUIDE.md                 Website and product design system
  VOICE-AND-COPY.md               Tone and reusable copy
  ASSETS.md                      Asset selection and export guide
  index.html                     Offline visual brand library
  brand-guidelines.pdf           Printable visual identity guide
  logos/                         SVG masters and transparent PNG exports
    horizontal/                  Primary horizontal lockups
    stacked/                     Portrait/square compositions
    wordmark/                    Outlined “lazy” lettering
    symbol/                      Sloth speech-bubble symbol
  icons/                         SVG, PNG, ICO and ICNS application icons
  mascot/                        Four Moss poses in PNG and WebP
  illustrations/                 Canopy master, responsive WebP and SVG pattern
  fonts/                         Nunito and DM Sans; TTF, WOFF2 and licenses
  styles/                        Fonts, theme tokens, reference components
  social/                        Share cards, profile icon and ready-to-copy metadata
  previews/                      Brand board, visual QA and printable source
  source/                        Approved vector source, generation prompts and build scripts
  asset-manifest.json            Current production files and checksums
  lazy-soft-signal-brand-kit.zip  Packaged selected identity
  explorations-2026-10-29/        Original five-direction exploration, retained as history
```

## Use in a website

Keep the relative folder structure or update font/image URLs when moving assets. Import these styles in order:

```html
<link rel="stylesheet" href="/branding/styles/fonts.css">
<link rel="stylesheet" href="/branding/styles/tokens.css">
<link rel="stylesheet" href="/branding/styles/components.css">
```

`tokens.css` contains the authoritative semantic palette. It follows the system appearance by default. Set `data-lazy-theme="light"` or `data-lazy-theme="dark"` on `<html>` to override it. `components.css` is an optional, prefixed reference stylesheet. Add `class="lazy-page"` to the page body to use its base typography and surface.

Use `logos/horizontal/lazy-logo-forest.svg` on light backgrounds and `lazy-logo-paper.svg` on dark backgrounds. Wordmarks are outlined and need no installed font. Use the font families for real text; never reconstruct the logo with live type.

## Build and provenance

`source/build_brand.py` creates the SVG family, WebP assets and CSS/JSON tokens. `source/export_brand.cjs` creates raster logo/icon exports, social images, the printable manual and preview images. `source/package_brand.py` refreshes the manifest and ZIP. See [ASSETS.md](ASSETS.md) for runtime dependencies and commands.

The original mascot and hero, plus three additional mascot poses, were created with built-in `image_gen`. Exact prompts and source references are in [source/generation-manifest.json](source/generation-manifest.json). SVG logo variants extend the approved original vector geometry; the website pattern is native SVG. The approved exploration is preserved unchanged.
