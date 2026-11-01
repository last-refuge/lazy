//! The look shared by the Integrations dialog's views (the list, GitHub, webhook connectors, and
//! apps): a service's tile, a status chip, the header with its way back, sections, and numbered
//! setup steps. Drawing only; no state.

use gpui::{
    AnyElement, FocusHandle, FontWeight, Hsla, IntoElement, ParentElement, Styled, div, prelude::*,
    px, rgb,
};

use crate::form::{self, ButtonKind};
use crate::theme::Theme;

/// How a status reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Nothing set up, or nothing to say.
    Quiet,
    /// Working.
    Good,
    /// Needs a step from the admin.
    Attention,
    /// Something failed.
    Problem,
}

fn tone_color(theme: &Theme, tone: Tone) -> Hsla {
    match tone {
        Tone::Quiet => theme.colors.text_faint,
        Tone::Good => theme.colors.success,
        Tone::Attention => theme.colors.warning,
        Tone::Problem => theme.colors.danger,
    }
}

/// A service's mark: its logo (or, without one, its initial) in white on its brand color, in a
/// rounded square.
pub fn service_mark(theme: &Theme, mark: Mark, size: f32) -> AnyElement {
    let logo = mark.logo.and_then(crate::service_logos::logo);
    div()
        .flex_none()
        .size(px(size))
        .rounded(theme.radii.medium)
        .bg(rgb(mark.color))
        .border_1()
        .border_color(theme.colors.border)
        .flex()
        .items_center()
        .justify_center()
        .map(|tile| match logo {
            Some(logo) => tile.child(
                gpui::img(logo)
                    .size(px((size * 0.6).round()))
                    .object_fit(gpui::ObjectFit::Contain),
            ),
            None => tile
                .text_size(px(size * 0.5))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(0xffffff))
                .child(mark.glyph),
        })
        .into_any_element()
}

/// A status: a colored dot and words.
pub fn status_line(theme: &Theme, selector: String, text: String, tone: Tone) -> AnyElement {
    div()
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .gap_1p5()
        .min_w_0()
        .child(
            div()
                .flex_none()
                .size(px(7.))
                .rounded(theme.radii.pill)
                .bg(tone_color(theme, tone)),
        )
        .child(
            div()
                .truncate()
                .text_size(theme.type_scale.small)
                .text_color(match tone {
                    Tone::Problem => theme.colors.danger,
                    Tone::Attention => theme.colors.warning,
                    Tone::Quiet | Tone::Good => theme.colors.text_muted,
                })
                .child(text),
        )
        .into_any_element()
}

/// A small pill: a count of failures, or a state in a header.
pub fn chip(theme: &Theme, text: String, tone: Tone) -> AnyElement {
    let (bg, fg) = match tone {
        Tone::Problem => (theme.colors.danger_surface, theme.colors.danger),
        Tone::Attention => (theme.colors.warning_surface, theme.colors.warning),
        Tone::Good => (theme.colors.accent_surface, theme.colors.text),
        Tone::Quiet => (theme.colors.surface_hover, theme.colors.text_muted),
    };
    div()
        .flex_none()
        .px_2()
        .py_0p5()
        .rounded(theme.radii.pill)
        .bg(bg)
        .text_size(theme.type_scale.small)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(fg)
        .child(text)
        .into_any_element()
}

/// A section's title, with an optional line under it.
pub fn section(theme: &Theme, title: impl Into<String>, hint: Option<String>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_0p5()
        .pt_1()
        .child(
            div()
                .text_size(theme.type_scale.body)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.colors.text)
                .child(title.into()),
        )
        .children(hint.map(|hint| {
            div()
                .text_size(theme.type_scale.small)
                .text_color(theme.colors.text_muted)
                .child(hint)
        }))
        .into_any_element()
}

/// A view's header: the way back to the list, then the service's mark, name, and state.
#[allow(clippy::too_many_arguments)]
pub fn header(
    theme: &Theme,
    back_selector: String,
    back_label: &'static str,
    back_focus: &FocusHandle,
    on_back: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
    mark: Option<AnyElement>,
    title: String,
    state: Option<AnyElement>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(div().flex().ml(px(-12.)).child(form::button(
            theme,
            back_selector,
            back_label,
            ButtonKind::Ghost,
            true,
            back_focus,
            on_back,
        )))
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .children(mark)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(theme.type_scale.title)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.colors.text)
                        .child(title),
                )
                .children(state),
        )
        .into_any_element()
}

/// One numbered setup step: its number in a circle (a check once `done`), a title, and what to do.
pub fn step(
    theme: &Theme,
    number: u8,
    done: bool,
    title: impl Into<String>,
    body: AnyElement,
) -> AnyElement {
    let colors = theme.colors;
    div()
        .flex()
        .gap_3()
        .child(
            div()
                .flex_none()
                .size(px(24.))
                .rounded(theme.radii.pill)
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::BOLD)
                .when(done, |c| c.bg(colors.success).text_color(colors.on_accent))
                .when(!done, |c| {
                    c.bg(colors.accent_surface)
                        .text_color(colors.text)
                        .border_1()
                        .border_color(colors.border_strong)
                })
                .child(if done {
                    "✓".to_owned()
                } else {
                    number.to_string()
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .pt(px(2.))
                        .text_size(theme.type_scale.body)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text)
                        .child(title.into()),
                )
                .child(body),
        )
        .into_any_element()
}

/// Short instructions, one per line, each with a bullet.
pub fn bullets(theme: &Theme, lines: &[&str]) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .children(lines.iter().map(|line| {
            div()
                .flex()
                .gap_2()
                .text_size(theme.type_scale.small)
                .text_color(theme.colors.text_muted)
                .child(div().flex_none().child("•"))
                .child(div().flex_1().min_w_0().child((*line).to_owned()))
        }))
        .into_any_element()
}

/// A thin line between parts of a view.
pub fn divider(theme: &Theme) -> AnyElement {
    div().h(px(1.)).bg(theme.colors.border).into_any_element()
}

/// How a service is marked: an initial (when there is no logo), its color, and its logo's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub glyph: &'static str,
    pub color: u32,
    pub logo: Option<&'static str>,
}

/// The marks of the services Lazy connects to, in their brand colors (as Simple Icons lists them),
/// and Lazy's own for apps and webhooks.
pub mod marks {
    use super::Mark;

    const fn mark(glyph: &'static str, color: u32, logo: Option<&'static str>) -> Mark {
        Mark { glyph, color, logo }
    }

    pub const GITHUB: Mark = mark("G", 0x181717, Some("github"));
    pub const LINEAR: Mark = mark("L", 0x5e6ad2, Some("linear"));
    pub const JIRA: Mark = mark("J", 0x0052cc, Some("jira"));
    pub const SENTRY: Mark = mark("S", 0x362d59, Some("sentry"));
    pub const PAGERDUTY: Mark = mark("P", 0x06ac38, Some("pagerduty"));
    pub const APPS: Mark = mark("A", 0x2f6f5e, None);
    pub const WEBHOOK: Mark = mark("W", 0x6b5b3e, None);

    use lazy_domain::connector::ConnectorProvider;

    pub fn of(provider: ConnectorProvider) -> Mark {
        match provider {
            ConnectorProvider::Linear => LINEAR,
            ConnectorProvider::Jira => JIRA,
            ConnectorProvider::Sentry => SENTRY,
            ConnectorProvider::PagerDuty => PAGERDUTY,
            ConnectorProvider::Unknown => mark("?", 0x6b7280, None),
        }
    }
}
