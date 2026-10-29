# Lazy website and product style guide

Soft Signal · Version 1.0.0

## Foundation and implementation

The CSS and JSON in `styles/` are the implementation source of truth. `fonts.css` defines local Nunito and DM Sans. `tokens.css` defines color, type, spacing, radii and motion. `components.css` supplies optional prefixed reference components. It does not apply a blanket reset or create a complete website.

```html
<html lang="en" data-lazy-theme="light">
  <head>
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <link rel="stylesheet" href="/branding/styles/fonts.css">
    <link rel="stylesheet" href="/branding/styles/tokens.css">
    <link rel="stylesheet" href="/branding/styles/components.css">
  </head>
  <body class="lazy-page">
    <main class="lazy-container">
      <h1 class="lazy-display">Good work.<br>Easy conversation.</h1>
    </main>
  </body>
</html>
```

Omit `data-lazy-theme` to follow the system. Set it to `light` or `dark` for an explicit preference. When implementing a preference switch, retain the user's choice. The brand-library switch is a local reference interaction, not application preference code.

## Color roles

| Token | Use |
| --- | --- |
| `--lazy-page` | Overall canvas |
| `--lazy-surface` / `--lazy-surface-raised` | Inputs, panels / elevated surfaces |
| `--lazy-text` / `--lazy-text-muted` | Primary / secondary content |
| `--lazy-brand` / `--lazy-on-brand` | Primary button fill / label |
| `--lazy-brand-hover` / `--lazy-brand-active` | Primary interactive states |
| `--lazy-accent` / `--lazy-on-accent` | Apricot highlight / dark label |
| `--lazy-support` / `--lazy-on-support` | Sage or dark supporting panel / label |
| `--lazy-selected` / `--lazy-on-selected` | Selected item surface / content |
| `--lazy-border` | Decorative divider or nonessential container edge |
| `--lazy-control-border` | Essential visible boundary of an input/control |
| `--lazy-focus` | Keyboard focus outline on page/surface |
| `--lazy-link` | Underlined inline links |
| `--lazy-disabled` / `--lazy-on-disabled` | Inactive controls |
| `--lazy-success`, `--lazy-warning`, `--lazy-danger`, `--lazy-info` | Status content, each paired with `-surface` |

Do not put muted text onto a brand-filled button, or use the decorative border to define a required control. Status messages pair a named state or meaningful icon with text. Keep destructive actions distinct from primary actions.

44 pairings are verified in `styles/contrast-report.json`: normal text and buttons at least 4.5:1; tested control edges and focus rings at least 3:1 against adjacent page/surface fills. New overlays, transparency and image-backed content require their own checks. This report verifies color pairs; complete product accessibility also depends on semantics and behavior.

## Typography and spacing

Use the fluid type scale in `tokens.json`. Readable body text starts at 16 px; input text stays at least 16 px on mobile. The 12 px caption style is reserved for short secondary labels. Use `rem` units to respect browser text sizing.

Spacing tokens are named by 4 px units: `space-1` = 4, `space-2` = 8, `space-3` = 12, `space-4` = 16, `space-6` = 24, `space-8` = 32, `space-12` = 48, `space-16` = 64, `space-28` = 112 px.

Use 8–12 px between a label and supporting content, 16–24 px inside small groups, 24–32 px inside major surfaces, 48–64 px between local sections and 64–112 px between marketing sections. Group related content with spacing before adding a border or background.

## Responsive website layout

| Width | Guidance |
| --- | --- |
| Under 640 px | Single column, 24 px gutters, stack hero copy and illustration; keep controls reachable |
| 640–1023 px | Two columns where content remains readable; use 32 px gutters |
| 1024 px and up | Up to 1200 px content, 48 px gutters, up to 12-column alignment grid |

Use content-driven breakpoints. A hero may split approximately 45/55 between copy and illustration. Leave at least 32 px between columns. Keep headings unbroken at meaningful phrase boundaries. Avoid fixed heights for text areas and never rely on horizontal scrolling for core content.

Suggested page sequence: a clear product statement; a real product preview; a small set of supported benefits; feature details; installation or sign-in actions. Reflect the implementation's actual features and supported platforms. The supplied gallery is a brand reference, not a claim that a marketing site or download service has shipped.

## Buttons and links

Primary buttons use Forest/Paper in light mode and Sage/Deep Forest in dark mode. Use one primary action per meaningful group. Secondary buttons use a transparent fill and essential border. Inline links are underlined so they remain recognizable without color.

Minimum target is 44 × 44 CSS px. Reference buttons are at least 44 px tall, with 12 px vertical and 22 px horizontal padding, DM Sans 600 and pill corners. Avoid relying on hover to reveal an essential action.

```html
<button type="button" class="lazy-button">Create a workspace</button>
<button type="button" class="lazy-button lazy-button--secondary">Sign in</button>
<button type="button" class="lazy-button" disabled>Sending…</button>
```

The CSS defines default, hover, active, focus and disabled states. Announce asynchronous completion through an appropriate live region in the real application. Disabling a button is not a substitute for explaining why an action is unavailable.

## Forms

Use visible labels, 48 px minimum field height, 8 px corners and `control-border`. Placeholder text is an example, not a label. Associate help/error text with `aria-describedby`; set `aria-invalid="true"` for an invalid field. Preserve entered values on recoverable errors.

```html
<label class="lazy-label" for="workspace">Workspace name</label>
<input class="lazy-input" id="workspace" name="workspace"
       autocomplete="organization" aria-describedby="workspace-help">
<p class="lazy-help" id="workspace-help">Use a name your team will recognize.</p>
```

Do not use error styling on a field before the user has interacted with it, unless a submission exposed the problem. Show what happened and a concrete recovery step. Respect native keyboard, input-method and validation behavior.

## Surfaces and icons

Use 20 px corners for major marketing/product surfaces, 12 px for status panels and 8 px for inputs and compact menus. Use one-pixel decorative dividers and a subtle shadow only when elevation explains layering.

Use one rounded outline icon family at 20 or 24 px with a 1.75–2 px stroke. Decorative icons are hidden from assistive technology; action icons need an accessible label. No extra icon font or third-party icon package is bundled here. Keep platform icons recognizable when a platform convention is stronger than the marketing style.

## Chat and desktop product adaptation

Use DM Sans for conversation lists, message content, metadata and controls. Use Nunito sparingly for onboarding and major headings. Preserve familiar channel, thread and unread behavior. Communicate selection through a surface plus a label/marker; communicate unread state through weight and a dot/count as appropriate.

Chat timestamps may use 12 px with a high-contrast muted color. Keep message body at least 14–16 px depending on the supported platform/text-scaling settings. Dense desktop layouts may use tighter spacing than marketing pages; keep hit targets and keyboard focus clear. This kit does not change the existing Rust/GPUI client.

## Illustration and image loading

```html
<img src="/branding/illustrations/conversation-canopy-960.webp"
     srcset="/branding/illustrations/conversation-canopy-480.webp 480w,
             /branding/illustrations/conversation-canopy-960.webp 960w,
             /branding/illustrations/conversation-canopy-1536.webp 1536w"
     sizes="(min-width: 1024px) 55vw, 100vw"
     width="1536" height="1024"
     alt="Moss among paper leaves shaped like conversation bubbles">
```

Keep above-the-fold hero artwork eager; lazy-load supporting imagery. Explicit dimensions prevent layout movement. Use `alt=""` for a purely decorative mascot when adjacent text conveys its meaning. For a substantive identity image, provide a short descriptive alternative. Place editorial text in HTML, not raster art.

## Motion and focus

Use the supplied 120/160/240 ms motion values and easing. Do not animate every hover with movement. Honor `prefers-reduced-motion` and avoid autoplay loops in work areas. Focus is a 3 px outline with a 3 px offset on tested neutral surfaces. On a colored component, provide a tested neutral focus halo rather than assuming a focus color contrasts with every fill.

Respect native reading order and tab order. Every reference control is keyboard accessible. Test real forms, menus and dialogs with keyboard navigation, screen readers and text scaling before publishing them.

## Website metadata

Use `icons/favicon.svg`, ICO fallback, Apple touch icon and supplied social share artwork. `social/metadata-example.html` provides paths and a clearly marked deployment-URL placeholder. `icons/site.webmanifest` has relative icon paths for placement alongside the icon files. It provides icon metadata without asserting an offline application or service worker.

## Release handoff

Copy the chosen logo, artwork, CSS and fonts together. Preserve licenses and update relative URLs. Keep the transparent PNG master for future artwork, use WebP derivatives for the web, and use platform icon exports for installers. Run the supplied export/verification script after changing the assets, then refresh the manifest and ZIP. Review both theme modes and widths down to 320 px.
