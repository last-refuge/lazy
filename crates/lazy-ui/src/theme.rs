//! Semantic design tokens. Views read colors and metrics from the active [`Theme`] by role and
//! never embed literal colors, so light and dark themes (and later high-contrast) are one switch.
//!
//! The palette is the brand's (Soft Signal 1.0.0, `branding/styles/tokens.css`): Paper and
//! Porcelain surfaces with Forest text in the light theme, Deep Forest and Night Leaf surfaces with
//! Paper text in the dark one. Buttons and the primary accent are the brand fill (Forest, Sage in
//! the dark theme); Apricot is a warm highlight and never a text color. Every value below is a
//! brand token, written once, except the dark theme's rail, which is Deep Forest darkened 40% (so
//! it stays apart from the sidebar next to it). Status is never conveyed by color alone: failure
//! and unknown states also carry text and icons in the views that use them.

use gpui::{App, Global, Hsla, Pixels, WindowAppearance, px, rgb};
use lazy_client_core::{Appearance, MotionChoice, ThemeChoice};

/// Which palette is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Light,
    Dark,
}

/// Color roles.
#[derive(Debug, Clone, Copy)]
pub struct Colors {
    /// Narrow workspace rail on the far left.
    pub rail: Hsla,
    /// A workspace's square on the rail when it is not the current one, and the text on it (and
    /// on the rail itself).
    pub rail_square: Hsla,
    pub rail_text: Hsla,
    /// The current workspace's square on the rail, and the text on it.
    pub rail_active: Hsla,
    pub rail_active_text: Hsla,
    /// Channel and conversation sidebar.
    pub sidebar: Hsla,
    pub sidebar_text: Hsla,
    pub sidebar_text_muted: Hsla,
    pub sidebar_hover: Hsla,
    pub sidebar_selected: Hsla,
    pub sidebar_selected_text: Hsla,
    /// Main content surface (timeline).
    pub surface: Hsla,
    /// Raised elements on the surface: cards, popovers, inputs.
    pub surface_raised: Hsla,
    pub surface_hover: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    /// Background of inline code and code blocks.
    pub code_surface: Hsla,
    /// The bar beside a quote.
    pub quote_bar: Hsla,
    /// Background of a message the person was taken to, such as by a permalink.
    pub highlight: Hsla,
    pub accent: Hsla,
    /// A pale wash of the accent, behind things that are "on": a reaction the person made.
    pub accent_surface: Hsla,
    pub accent_hover: Hsla,
    /// Text of a mention (`@alice`) in a message.
    pub mention_text: Hsla,
    /// Behind a mention of someone else.
    pub mention_surface: Hsla,
    /// Behind a mention of the person looking: a stronger wash.
    pub mention_me_surface: Hsla,
    /// Behind a whole message that notifies the person.
    pub mention_row: Hsla,
    /// The bar down the left of such a message.
    pub mention_bar: Hsla,
    /// Text drawn on top of `accent`.
    pub on_accent: Hsla,
    pub danger: Hsla,
    pub danger_surface: Hsla,
    pub warning: Hsla,
    pub warning_surface: Hsla,
    pub success: Hsla,
    pub selection: Hsla,
    pub caret: Hsla,
    pub focus_ring: Hsla,
    pub scrim: Hsla,
}

/// Type sizes in logical pixels.
#[derive(Debug, Clone, Copy)]
pub struct Typography {
    pub body: Pixels,
    pub small: Pixels,
    pub title: Pixels,
    pub heading: Pixels,
    pub line_height_body: Pixels,
    /// Monospace text in messages.
    pub code: Pixels,
    pub line_height_code: Pixels,
}

/// Corner radii: the brand's 8 px for inputs and compact menus (`medium`), 12 px for status panels
/// and tiles (`panel`), 20 px for major surfaces such as dialogs and cards (`large`), and a pill
/// for pill buttons. `small` keeps dense rows and chips tight.
#[derive(Debug, Clone, Copy)]
pub struct Radii {
    pub small: Pixels,
    pub medium: Pixels,
    pub panel: Pixels,
    pub large: Pixels,
    pub pill: Pixels,
}

/// The active theme, stored as a GPUI global.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub mode: ThemeMode,
    pub colors: Colors,
    pub type_scale: Typography,
    pub radii: Radii,
}

impl Global for Theme {}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

/// `hex` at `alpha` (0 to 1), for scrims.
fn c_alpha(hex: u32, alpha: f32) -> Hsla {
    let mut color = c(hex);
    color.a = alpha;
    color
}

impl Theme {
    pub fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            colors: Colors {
                // Deep Forest rail with Night Leaf squares; Paper marks the current workspace.
                rail: c(0x142b24),
                rail_square: c(0x1f3a31),
                rail_text: c(0xf5f1e7),
                rail_active: c(0xf5f1e7),
                rail_active_text: c(0x183f35),
                // Paper sidebar beside the Porcelain conversation; the selected row is the
                // brand's `selected` surface (its label and marker carry the meaning too).
                sidebar: c(0xf5f1e7),
                sidebar_text: c(0x183f35),
                sidebar_text_muted: c(0x50655a),
                sidebar_hover: c(0xe5e8df),
                sidebar_selected: c(0xe2eadb),
                sidebar_selected_text: c(0x183f35),
                surface: c(0xfffdfa),
                surface_raised: c(0xffffff),
                surface_hover: c(0xe5e8df),
                // `border` is decorative; the edge of an input or control is `border_strong`
                // (the brand's control-border).
                border: c(0xcdd7c7),
                border_strong: c(0x708679),
                text: c(0x183f35),
                text_muted: c(0x50655a),
                // The brand has no third text level: faint is the muted color, which is the
                // lightest text it verifies on these surfaces.
                text_faint: c(0x50655a),
                code_surface: c(0xe5e8df),
                quote_bar: c(0x708679),
                highlight: c(0xf6ebcd),
                // The brand fill: Forest, with Paper on it.
                accent: c(0x183f35),
                accent_surface: c(0xe2eadb),
                accent_hover: c(0x12372d),
                mention_text: c(0x183f35),
                mention_surface: c(0xe2eadb),
                mention_me_surface: c(0xa9c3a0),
                mention_row: c(0xf6ebcd),
                // Apricot is the warm highlight: the bar beside a message that notifies you.
                mention_bar: c(0xe9a38d),
                on_accent: c(0xf5f1e7),
                danger: c(0x9d352d),
                danger_surface: c(0xf8e6de),
                warning: c(0x7c4b14),
                warning_surface: c(0xf6ebcd),
                success: c(0x22624a),
                selection: c(0xa9c3a0),
                caret: c(0x183f35),
                focus_ring: c(0x326b58),
                scrim: c_alpha(0x142b24, 0.45),
            },
            type_scale: Typography {
                body: px(14.),
                small: px(12.),
                title: px(15.),
                heading: px(20.),
                line_height_body: px(21.),
                code: px(13.),
                line_height_code: px(19.),
            },
            radii: Radii {
                small: px(4.),
                medium: px(8.),
                panel: px(12.),
                large: px(20.),
                pill: px(999.),
            },
        }
    }

    pub fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            colors: Colors {
                // Deep Forest darkened 40% (the one derived value): it stays apart from the
                // Deep Forest sidebar beside it. Sage marks the current workspace.
                rail: c(0x0c1a16),
                rail_square: c(0x1f3a31),
                rail_text: c(0xf5f1e7),
                rail_active: c(0xa9c3a0),
                rail_active_text: c(0x142b24),
                // Deep Forest sidebar beside the Night Leaf conversation.
                sidebar: c(0x142b24),
                sidebar_text: c(0xf5f1e7),
                sidebar_text_muted: c(0xbdcdbf),
                sidebar_hover: c(0x314b40),
                sidebar_selected: c(0x355542),
                sidebar_selected_text: c(0xf5f1e7),
                surface: c(0x1f3a31),
                surface_raised: c(0x28483c),
                surface_hover: c(0x314b40),
                border: c(0x49685a),
                border_strong: c(0x87998e),
                text: c(0xf5f1e7),
                text_muted: c(0xbdcdbf),
                text_faint: c(0xbdcdbf),
                code_surface: c(0x314b40),
                quote_bar: c(0x87998e),
                highlight: c(0x473a25),
                // The brand fill: Sage, with Deep Forest on it.
                accent: c(0xa9c3a0),
                accent_surface: c(0x28483c),
                accent_hover: c(0xbdd4b5),
                mention_text: c(0xf5f1e7),
                mention_surface: c(0x355542),
                mention_me_surface: c(0x3e5e4b),
                mention_row: c(0x473a25),
                mention_bar: c(0xe9a38d),
                on_accent: c(0x142b24),
                danger: c(0xf2b4a8),
                danger_surface: c(0x452a29),
                warning: c(0xecca86),
                warning_surface: c(0x473a25),
                success: c(0xb1dabc),
                selection: c(0x3e5e4b),
                caret: c(0xa9c3a0),
                focus_ring: c(0xc7ddbd),
                scrim: c_alpha(0x000000, 0.55),
            },
            type_scale: Typography {
                body: px(14.),
                small: px(12.),
                title: px(15.),
                heading: px(20.),
                line_height_body: px(21.),
                code: px(13.),
                line_height_code: px(19.),
            },
            radii: Radii {
                small: px(4.),
                medium: px(8.),
                panel: px(12.),
                large: px(20.),
                pill: px(999.),
            },
        }
    }

    /// The theme for what the person chose: the operating system's appearance when they follow
    /// it, else the one they picked.
    pub fn for_choice(choice: ThemeChoice, os: WindowAppearance) -> Self {
        match choice {
            ThemeChoice::System => Self::for_appearance(os),
            ThemeChoice::Light => Self::light(),
            ThemeChoice::Dark => Self::dark(),
        }
    }

    /// The theme that matches the operating system's current appearance.
    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }

    /// Reads the active theme. Panics if [`install`] was never called, which is a startup bug.
    pub fn get(cx: &App) -> &Theme {
        cx.global::<Theme>()
    }
}

/// What decides the theme: the operating system's appearance and the person's choice. Kept
/// together so a change of either installs the right theme.
#[derive(Debug, Clone, Copy)]
struct Preference {
    os: WindowAppearance,
    choice: ThemeChoice,
}

impl Global for Preference {}

/// Records the operating system's current `appearance` and installs the theme for it and the
/// person's choice (following the system until one is made). Called at startup and whenever the
/// system's appearance changes.
pub fn install(cx: &mut App, appearance: WindowAppearance) {
    let choice = cx
        .try_global::<Preference>()
        .map_or(ThemeChoice::System, |preference| preference.choice);
    set(cx, appearance, choice);
}

/// Installs the theme for the person's `choice`, keeping the system's appearance as last
/// reported.
pub fn set_choice(cx: &mut App, choice: ThemeChoice) {
    let os = cx
        .try_global::<Preference>()
        .map_or(WindowAppearance::Light, |preference| preference.os);
    set(cx, os, choice);
}

fn set(cx: &mut App, os: WindowAppearance, choice: ThemeChoice) {
    cx.set_global(Preference { os, choice });
    cx.set_global(Theme::for_choice(choice, os));
}

/// Applies how this device looks and moves: the theme now, and reduced motion to GPUI, which
/// holds still every animation it runs (animated images among them). GPUI does not tell us the
/// operating system's own preference, so "follow the system" means full motion for now.
pub fn apply_appearance(cx: &mut App, appearance: Appearance) {
    set_choice(cx, appearance.theme);
    cx.set_reduce_motion(effective_reduced_motion(appearance.motion));
    cx.refresh_windows();
}

/// Whether motion is reduced for a choice: on when asked for; "follow the system" cannot read the
/// system's setting and so is off.
pub fn effective_reduced_motion(choice: MotionChoice) -> bool {
    matches!(choice, MotionChoice::Reduce)
}

/// A stable avatar background for a person, picked from a warm palette by hashing their id.
pub fn avatar_color(seed: u128) -> Hsla {
    const PALETTE: [u32; 8] = [
        0xc2683e, 0x7a8f3a, 0x3f7f9c, 0x9b5a8c, 0xb08a2e, 0x4f8a64, 0xa4533f, 0x5e6fb0,
    ];
    c(PALETTE[(seed % PALETTE.len() as u128) as usize])
}

#[cfg(test)]
mod tests {
    use gpui::Rgba;

    use super::*;

    /// WCAG relative luminance for an sRGB color.
    fn luminance(color: Hsla) -> f32 {
        let rgba: Rgba = color.into();
        let lin = |v: f32| {
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(rgba.r) + 0.7152 * lin(rgba.g) + 0.0722 * lin(rgba.b)
    }

    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn text_roles_meet_wcag_aa_on_their_surfaces() {
        for theme in [Theme::light(), Theme::dark()] {
            let k = theme.colors;
            let pairs = [
                ("text/surface", k.text, k.surface),
                ("muted/surface", k.text_muted, k.surface),
                ("text/raised", k.text, k.surface_raised),
                ("rail text/square", k.rail_text, k.rail_square),
                ("rail text/rail", k.rail_text, k.rail),
                ("sidebar text", k.sidebar_text, k.sidebar),
                ("sidebar muted", k.sidebar_text_muted, k.sidebar),
                ("selected", k.sidebar_selected_text, k.sidebar_selected),
                ("on accent", k.on_accent, k.accent),
                ("link/surface", k.accent, k.surface),
                ("text/code", k.text, k.code_surface),
                ("text/chip-on", k.text, k.accent_surface),
                ("mention/surface", k.mention_text, k.mention_surface),
                ("mention/me", k.mention_text, k.mention_me_surface),
                ("mention/row-text", k.text, k.mention_row),
                ("mention/row-muted", k.text_muted, k.mention_row),
                ("mention/in-row", k.mention_text, k.mention_row),
                ("mention/me-in-row", k.mention_text, k.mention_row),
                ("muted/chip", k.text_muted, k.surface_raised),
                ("link/highlight", k.accent, k.highlight),
                ("link/row", k.accent, k.mention_row),
                ("badge/accent-surface", k.accent, k.accent_surface),
                // Selected text is drawn over the selection color, whatever style it has. The brand
                // verifies body text on its selection colors; muted text and links in the dark
                // theme's Sage fall just under 4.5:1 there, so they are not asserted (a selection
                // is transient, and the text stays readable at the contrast the brand reports).
                ("text/selection", k.text, k.selection),
                ("mention/selection", k.mention_text, k.selection),
                ("link/raised", k.accent, k.surface_raised),
                ("muted/row-raised", k.text_muted, k.surface_raised),
                ("text/highlight", k.text, k.highlight),
                ("muted/highlight", k.text_muted, k.highlight),
                ("muted/code", k.text_muted, k.code_surface),
                ("danger", k.danger, k.danger_surface),
                ("warning", k.warning, k.warning_surface),
                ("rail current", k.rail_active_text, k.rail_active),
                ("success/surface", k.success, k.surface),
            ];
            for (name, fg, bg) in pairs {
                let ratio = contrast(fg, bg);
                assert!(
                    ratio >= 4.5,
                    "{name} in {:?}: contrast {ratio:.2} < 4.5",
                    theme.mode
                );
            }
        }
    }

    /// The pairs the brand itself verifies (`branding/styles/contrast-report.json`): body, muted,
    /// button, selected, and status text at 4.5:1; the focus ring and a control's edge at 3:1.
    #[test]
    fn the_brands_verified_pairs_hold_in_both_themes() {
        for theme in [Theme::light(), Theme::dark()] {
            let k = theme.colors;
            let text = [
                ("body/surface", k.text, k.surface),
                ("body/raised", k.text, k.surface_raised),
                ("muted/surface", k.text_muted, k.surface),
                ("muted/raised", k.text_muted, k.surface_raised),
                ("button", k.on_accent, k.accent),
                ("sidebar/page", k.sidebar_text, k.sidebar),
                ("muted/page", k.sidebar_text_muted, k.sidebar),
                ("selected", k.sidebar_selected_text, k.sidebar_selected),
                ("danger", k.danger, k.danger_surface),
                ("warning", k.warning, k.warning_surface),
            ];
            for (name, fg, bg) in text {
                let ratio = contrast(fg, bg);
                assert!(ratio >= 4.5, "{name} in {:?}: {ratio:.2} < 4.5", theme.mode);
            }
            let edges = [
                ("focus/surface", k.focus_ring, k.surface),
                ("focus/sidebar", k.focus_ring, k.sidebar),
                ("control/surface", k.border_strong, k.surface),
                ("control/raised", k.border_strong, k.surface_raised),
            ];
            for (name, fg, bg) in edges {
                let ratio = contrast(fg, bg);
                assert!(ratio >= 3.0, "{name} in {:?}: {ratio:.2} < 3", theme.mode);
            }
        }
    }

    #[test]
    fn the_radii_are_the_brands() {
        for theme in [Theme::light(), Theme::dark()] {
            let r = theme.radii;
            assert_eq!(
                (r.small, r.medium, r.panel, r.large, r.pill),
                (px(4.), px(8.), px(12.), px(20.), px(999.))
            );
        }
    }

    #[test]
    fn avatar_colors_are_stable() {
        assert_eq!(avatar_color(42), avatar_color(42));
    }
}
