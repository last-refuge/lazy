# Lazy brand guidelines

Soft Signal · Version 1.0.0 · October 29, 2026

## 1. Brand foundation

**Good work. Easy conversation.**

Lazy is a native workplace chat app for channels, direct messages and the people you work with. The brand makes work feel approachable. It promises ease and clarity through its tone and design; performance or feature claims must reflect the actual product.

| Element | Direction |
| --- | --- |
| Product name | Lazy, capital L in prose |
| Logo lettering | lowercase “lazy”, as supplied in the SVG |
| Visual direction | Soft Signal, an internal design name |
| Mascot | Moss, the paper-cut sloth |
| Personality | Gentle, welcoming, grounded |
| Primary headline | Good work. Easy conversation. |
| Supporting line | A little less noise. A lot more human. |
| Plain product description | A native place for channels, messages and the people you work with. |

Be warm without becoming vague, relaxed without seeming unavailable, and expressive without crowding the workspace. Keep Moss awake and the UI responsive in both language and presentation.

## 2. Logo system

The approved symbol is a sloth face within a rounded speech bubble. Nunito-based outlined lettering creates the horizontal lockup. Use supplied artwork to preserve the spacing and shapes.

| Asset | Use | Minimum on-screen width |
| --- | --- | --- |
| Horizontal lockup | Navigation, marketing, document headers | 120 CSS px |
| Stacked lockup | Square/portrait layouts and covers | 96 CSS px |
| Wordmark | Constrained brand signatures with established context | 80 CSS px |
| Standard symbol | App surfaces, avatars, compact navigation | 24 CSS px |
| Reduced-detail symbol | Favicon artwork at small sizes | 16–23 CSS px |

The standard symbol remains the primary logo. The small-size symbol removes tiny pupil cutouts that become subpixel detail; its silhouette and face patches remain the same.

### Clear space

Use the symbol's outer ink width as **S**. Leave at least **S/4** of clear space around every side of a lockup. With the standalone symbol, use one quarter of its ink width. The transparent padding in the export does not count as the complete clear-space allowance. Text, controls, other logos, container edges and imagery stay outside this zone.

The horizontal file has a `293.9 × 110` viewBox. Use an automatic height to preserve its proportions. At 160 CSS px wide it is approximately 60 CSS px high including its transparent canvas.

### Colors and backgrounds

- **Forest** is the default on Paper, Porcelain, white or another very light background.
- **Paper** is the preferred reverse on Forest or Deep Forest.
- **Sage** is an alternate reverse on Deep Forest when a green logo is useful.
- **Black/white** are for true single-ink or monochrome applications.
- Use an opaque brand surface behind the logo when a photograph or illustration is busy.
- Keep the standard logo at full opacity. Place it as an external SVG image to avoid duplicate mask IDs from repeated inline SVG copies.

### Keep the identity intact

Preserve the approved proportions, face geometry, lettering and symbol-to-wordmark gap. Do not retype the logo, stretch it, tilt it, add shadows, outline its edges, insert gradients, put illustrations inside the symbol or substitute Moss's detailed portrait for the app mark. On variable backgrounds, switch to a suitable supplied variant or opaque brand surface when mark-to-background contrast falls below 3:1.

## 3. Color palette

| Color | HEX | RGB | Role |
| --- | --- | --- | --- |
| Forest | `#183F35` | 24, 63, 53 | Core identity, text and light-theme primary actions |
| Paper | `#F5F1E7` | 245, 241, 231 | Default canvas and reversed logo |
| Sage | `#A9C3A0` | 169, 195, 160 | Supporting surfaces, illustration and dark-theme primary actions |
| Apricot | `#E9A38D` | 233, 163, 141 | Warm accents and conversational illustration |
| Porcelain | `#FFFDFA` | 255, 253, 250 | Light-theme cards and inputs |
| Fern | `#50655A` | 80, 101, 90 | Secondary light-theme text |
| Mist | `#CDD7C7` | 205, 215, 199 | Decorative light-theme dividers |
| Deep Forest | `#142B24` | 20, 43, 36 | Dark-theme canvas |
| Night Leaf | `#1F3A31` | 31, 58, 49 | Dark-theme cards and inputs |
| Pale Leaf | `#BDCDBF` | 189, 205, 191 | Secondary dark-theme text |

As a starting composition, aim for roughly 70% Paper or Deep Forest, 20% Forest/surface structure, 7% Sage and 3% Apricot. This is an art-direction guide, not a requirement to tint each UI section.

Use semantic tokens from `styles/tokens.css` for UI. The same brand swatch can have different jobs in light and dark themes. Never infer success or error from the brand palette; dedicated status tokens are included.

| Tested pairing | Contrast |
| --- | --- |
| Forest on Paper | 10.34:1 |
| Fern on Paper | 5.56:1 |
| Forest on Apricot | 5.60:1 |
| Paper on Deep Forest | 13.29:1 |
| Pale Leaf on Deep Forest | 9.03:1 |
| Deep Forest on Sage | 7.85:1 |

Keep every foreground/background role together. The full report covers statuses, interaction states and essential control borders. Mist is decorative and must not be the sole boundary of an input.

Digital exports use sRGB. For printed collateral, give the printer the SVG master and these RGB/HEX values; agree a printer-specific color proof instead of treating a generic CMYK conversion as an approved match.

## 4. Typography

**Nunito** provides rounded, welcoming headlines. **DM Sans** keeps body copy and interface text crisp and readable. Both are self-hosted with original font licenses.

| Role | Family / weight | Size / line height |
| --- | --- | --- |
| Hero/display | Nunito 750 | 44–80 px / 1.05 |
| Page heading | Nunito 750 | 36–56 px / 1.10 |
| Section heading | Nunito 700 | 28–40 px / 1.15 |
| Subheading | Nunito 700 | 24 px / 1.25 |
| Lead paragraph | DM Sans 400 | 18 px / 1.60 |
| Body | DM Sans 400 | 16 px / 1.60 |
| Navigation and controls | DM Sans 600 | 14–16 px / 1.20–1.50 |
| Supporting text | DM Sans 400/600 | 14 px / 1.50 |
| Captions | DM Sans 400/600 | 12 px / 1.50 |

Use sentence case. Headlines may track at −0.025 to −0.035 em; body text uses normal tracking. Keep reading copy near 55–70 characters per line. Avoid all-caps paragraphs, ultra-light weights and synthetic italics. Supplied font masters are upright variable fonts; obtain a true italic family before adding italic UI styles.

The logo uses outlined Nunito at weight 800 with fixed spacing. Its lettering is artwork, not a typography preset. If brand fonts cannot load, use the supplied system font fallbacks and preserve hierarchy.

## 5. Illustration language

Build large, rounded organic forms from layered paper. Use tactile fiber, gentle layer shadows and restrained Forest/Sage/Paper/Apricot color. Speech-bubble leaves and curved branches connect natural ease with conversation.

The master `illustrations/conversation-canopy.png` is 1536 × 1024. Responsive WebP exports are provided at 480, 960 and 1536 px wide. Keep text outside the artwork. Use the illustration as a separate visual with live copy beside it; on mobile, place it beneath the headline rather than cropping away Moss.

The vector conversation-leaf pattern is a low-contrast background accent for covers and quiet decorative areas. Do not use it behind body text, form fields or chat transcripts. Keep image texture out of essential controls and message backgrounds.

## 6. Moss mascot

Moss is awake, friendly and reassuring. Its recognizable features are a sage pear-shaped body, cream face and belly, dark slanted eye patches, a small dark nose, peach cheeks and three cream claws per hand. The coral speech bubble is its recurring prop.

| Pose | Asset | Suitable use |
| --- | --- | --- |
| Primary | `mascot/moss-primary.png` | Brand introduction and general signature |
| Welcome | `mascot/moss-welcome.png` | First-run, invitations and workspace welcome |
| Caught up | `mascot/moss-caught-up.png` | Empty thread inbox or completed review |
| Celebrate | `mascot/moss-celebrate.png` | Small successes and milestones |

Use Moss as a companion to a clear message and a useful action. Typical display size is 160–320 CSS px in onboarding and 96–160 px in a compact empty state; use the symbol instead below 80 px. Keep the original image proportions and transparent margins. All PNG masters have alpha transparency; WebP derivatives preserve it.

Avoid sleeping poses, frantic expressions, costumes, recolored bodies, additional characters and decorative animation in message lists. Do not use a cheerful mascot to minimize a destructive action, security problem or outage. A calm text explanation is more appropriate there.

New poses should reference the approved primary PNG, preserve the character geometry and paper material, and pass review at small sizes on both light and dark surfaces. Exact generation prompts are saved in `source/generation-manifest.json`.

## 7. Layout and motion

Use a 4 px spacing unit, generous outside margins, a 1200 px website content width and rounded 20 px major surfaces. Buttons use pill corners; inputs and dense controls use an 8 px radius. Full design-system values and examples are in [STYLE-GUIDE.md](STYLE-GUIDE.md).

Keep motion purposeful: 120 ms for small feedback, 160 ms for state changes and up to 240 ms for entrances. Respect reduced-motion preferences and avoid looping Moss animations in work areas.

## 8. Applications and asset ownership

The horizontal lockup leads website headers, social cards and document covers. The symbol or app tile is used for desktop launchers, browser tabs and small avatars. Social share cards are supplied at 1200 × 630; profile artwork at 400 × 400; square campaign artwork at 1080 × 1080.

Treat this directory as the source of truth. Update the build source, relevant guide and version together when changing identity assets. Keep `explorations-2026-10-29/` as historical work; do not copy colors or characters from other directions into the selected identity.
