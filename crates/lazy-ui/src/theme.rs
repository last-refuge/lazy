//! Semantic design tokens. Views read colors and metrics from the active [`Theme`] by role and
//! never embed literal colors, so light and dark themes (and later high-contrast) are one switch.
//!
//! Surfaces are warm neutrals with a single restrained accent. Status is never conveyed by color
//! alone: failure and unknown states also carry text and icons in the views that use them.

use gpui::{App, Global, Hsla, Pixels, WindowAppearance, px, rgb};

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
    pub accent: Hsla,
    pub accent_hover: Hsla,
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
}

/// Corner radii.
#[derive(Debug, Clone, Copy)]
pub struct Radii {
    pub small: Pixels,
    pub medium: Pixels,
    pub large: Pixels,
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

impl Theme {
    pub fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            colors: Colors {
                rail: c(0x2f2a26),
                sidebar: c(0xf1ebe2),
                sidebar_text: c(0x2b2622),
                sidebar_text_muted: c(0x6b6259),
                sidebar_hover: c(0xe8e0d4),
                sidebar_selected: c(0x1f7a6d),
                sidebar_selected_text: c(0xffffff),
                surface: c(0xfbf8f3),
                surface_raised: c(0xffffff),
                surface_hover: c(0xf4efe7),
                border: c(0xe4dcd0),
                border_strong: c(0xc9bfb0),
                text: c(0x2b2622),
                text_muted: c(0x6b6259),
                text_faint: c(0x8a8076),
                accent: c(0x1f7a6d),
                accent_hover: c(0x186157),
                on_accent: c(0xffffff),
                danger: c(0xb3261e),
                danger_surface: c(0xfbe9e7),
                warning: c(0x8a5a00),
                warning_surface: c(0xfdf1d6),
                success: c(0x2e7d32),
                selection: c(0xbfe3dc),
                caret: c(0x1f7a6d),
                focus_ring: c(0x1f7a6d),
                scrim: Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 0.0,
                    a: 0.35,
                },
            },
            type_scale: Typography {
                body: px(14.),
                small: px(12.),
                title: px(15.),
                heading: px(20.),
                line_height_body: px(21.),
            },
            radii: Radii {
                small: px(4.),
                medium: px(8.),
                large: px(12.),
            },
        }
    }

    pub fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            colors: Colors {
                rail: c(0x0f0e0d),
                sidebar: c(0x181614),
                sidebar_text: c(0xe9e3da),
                sidebar_text_muted: c(0x9d9489),
                sidebar_hover: c(0x24211e),
                sidebar_selected: c(0x2c9d8c),
                sidebar_selected_text: c(0x081513),
                surface: c(0x1d1b19),
                surface_raised: c(0x272421),
                surface_hover: c(0x24211e),
                border: c(0x34302c),
                border_strong: c(0x4a443e),
                text: c(0xece6dd),
                text_muted: c(0xa59c91),
                text_faint: c(0x82796f),
                accent: c(0x4fb3a2),
                accent_hover: c(0x6cc4b5),
                on_accent: c(0x081513),
                danger: c(0xf2877f),
                danger_surface: c(0x3a1f1d),
                warning: c(0xe5b54d),
                warning_surface: c(0x33280f),
                success: c(0x7bc67f),
                selection: c(0x24534c),
                caret: c(0x4fb3a2),
                focus_ring: c(0x4fb3a2),
                scrim: Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 0.0,
                    a: 0.55,
                },
            },
            type_scale: Typography {
                body: px(14.),
                small: px(12.),
                title: px(15.),
                heading: px(20.),
                line_height_body: px(21.),
            },
            radii: Radii {
                small: px(4.),
                medium: px(8.),
                large: px(12.),
            },
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

/// Installs the theme matching `appearance` as the application's global theme.
pub fn install(cx: &mut App, appearance: WindowAppearance) {
    cx.set_global(Theme::for_appearance(appearance));
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
                ("sidebar text", k.sidebar_text, k.sidebar),
                ("sidebar muted", k.sidebar_text_muted, k.sidebar),
                ("selected", k.sidebar_selected_text, k.sidebar_selected),
                ("on accent", k.on_accent, k.accent),
                ("danger", k.danger, k.danger_surface),
                ("warning", k.warning, k.warning_surface),
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

    #[test]
    fn avatar_colors_are_stable() {
        assert_eq!(avatar_color(42), avatar_color(42));
    }
}
