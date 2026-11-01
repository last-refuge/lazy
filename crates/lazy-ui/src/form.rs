//! Small themed building blocks shared by dialogs and menus: buttons, labelled fields, and a
//! segmented (radio-group) toggle. Every control here is a tab stop with a visible focus ring and
//! is operable with Enter or Space.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Div, ElementId, Entity, FocusHandle, FontWeight, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Role, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, div, prelude::*, px, transparent_black,
};

use crate::keyboard::is_activation;
use crate::text_input::TextInput;
use crate::theme::Theme;

/// How prominent a button is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Danger,
    /// No fill and no border until hovered: a quiet link-like action ("‹ All integrations").
    Ghost,
}

/// A text button that is a tab stop. When `enabled` is false it looks inert and ignores clicks
/// and keys (it stays focusable so keyboard users can discover it).
///
/// `id` doubles as the element id and the test label.
pub fn button(
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    enabled: bool,
    focus: &FocusHandle,
    on_activate: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    let selector = id.to_string();
    let colors = theme.colors;
    let on_activate = Rc::new(on_activate);
    let on_key = on_activate.clone();
    let on_click = on_activate;
    // The ring is the brand's focus color, which holds 3:1 against the surface around a button
    // (tested in `theme`); it is drawn on the button's edge, so a filled button's own fill, which
    // it does not contrast with, is not relied on.
    let focus_ring = colors.focus_ring;
    let (bg, fg, hover_bg, border) = match kind {
        ButtonKind::Primary => (
            colors.accent,
            colors.on_accent,
            colors.accent_hover,
            colors.accent,
        ),
        ButtonKind::Secondary => (
            colors.surface_raised,
            colors.text,
            colors.surface_hover,
            colors.border_strong,
        ),
        ButtonKind::Danger => (
            colors.danger_surface,
            colors.danger,
            colors.danger_surface,
            colors.danger,
        ),
        ButtonKind::Ghost => (
            gpui::transparent_black(),
            colors.text_muted,
            colors.surface_hover,
            gpui::transparent_black(),
        ),
    };
    div()
        .id(ElementId::Name(id))
        .debug_selector(move || selector)
        .track_focus(focus)
        .flex_none()
        .px_4()
        .py_1p5()
        // The brand's pill button.
        .rounded(theme.radii.pill)
        .border_1()
        .border_color(border)
        .text_size(theme.type_scale.body)
        .font_weight(FontWeight::SEMIBOLD)
        .flex()
        .items_center()
        .justify_center()
        .focus_visible(move |style| style.border_color(focus_ring).border_2())
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .bg(bg)
                .text_color(fg)
                .hover(move |style| style.bg(hover_bg))
                .on_key_down(move |event: &KeyDownEvent, window, cx| {
                    if is_activation(event) {
                        on_key(window, cx);
                        cx.stop_propagation();
                    }
                })
                .on_click(move |_, window, cx| on_click(window, cx))
        })
        .when(!enabled, |button| {
            button
                .bg(colors.surface_hover)
                .text_color(colors.text_faint)
                .border_color(colors.border)
        })
        .child(label.into())
}

/// A labelled checkbox that is one tab stop: Space or Enter toggles it, and so does a click on
/// the box or its label. `id` doubles as the element id and the test label.
pub fn checkbox(
    theme: &Theme,
    id: &'static str,
    label: impl Into<SharedString>,
    checked: bool,
    focus: &FocusHandle,
    on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let colors = theme.colors;
    let label: SharedString = label.into();
    let on_toggle = Rc::new(on_toggle);
    let on_key = on_toggle.clone();
    div()
        .id(ElementId::Name(id.into()))
        .debug_selector(move || id.to_owned())
        .track_focus(focus)
        .role(Role::CheckBox)
        .aria_label(label.clone())
        .aria_toggled(if checked {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .flex()
        .items_center()
        .gap_2()
        // As wide as its label, so the focus ring wraps the control and not the whole row.
        .self_start()
        .px_1()
        .py_0p5()
        .rounded(theme.radii.small)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(move |style| style.border_color(colors.focus_ring))
        .cursor_pointer()
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if is_activation(event) {
                on_key(!checked, window, cx);
                cx.stop_propagation();
            }
        })
        .on_click(move |_, window, cx| on_toggle(!checked, window, cx))
        .child(
            div()
                .size(px(16.))
                .flex_none()
                .rounded(px(4.))
                .border_1()
                .border_color(if checked {
                    colors.accent
                } else {
                    colors.border_strong
                })
                .bg(if checked {
                    colors.accent
                } else {
                    colors.surface_raised
                })
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.))
                .font_weight(FontWeight::BOLD)
                .text_color(colors.on_accent)
                .child(if checked { "✓" } else { "" }),
        )
        .child(
            div()
                .text_size(theme.type_scale.small)
                .text_color(colors.text_muted)
                .child(label),
        )
}

/// A label above an input box. The border takes the focus-ring color while the input is
/// focused. `hint` is small text under the box; `error` replaces it in the danger color.
pub fn labeled_field(
    theme: &Theme,
    label: impl Into<SharedString>,
    input: &Entity<TextInput>,
    hint: Option<SharedString>,
    error: Option<SharedString>,
    window: &Window,
    cx: &App,
) -> AnyElement {
    let focused = input.read(cx).is_focused(window);
    let colors = theme.colors;
    let label: SharedString = label.into();
    // Test labels for the text under the box: `field-error-Name`, `field-hint-Name`.
    let error_selector = format!("field-error-{label}");
    let hint_selector = format!("field-hint-{label}");
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text_muted)
                .child(label),
        )
        .child(
            div()
                .px_3()
                .py_2()
                .rounded(theme.radii.medium)
                .bg(colors.surface_raised)
                .border_1()
                .border_color(if error.is_some() {
                    colors.danger
                } else if focused {
                    colors.focus_ring
                } else {
                    colors.border_strong
                })
                .child(input.clone()),
        )
        .children(match (error, hint) {
            (Some(error), _) => Some(
                div()
                    .debug_selector(move || error_selector)
                    .text_size(theme.type_scale.small)
                    .text_color(colors.danger)
                    .child(error)
                    .into_any_element(),
            ),
            (None, Some(hint)) => Some(
                div()
                    .debug_selector(move || hint_selector)
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_faint)
                    .child(hint)
                    .into_any_element(),
            ),
            (None, None) => None,
        })
        .into_any_element()
}

/// One choice in a [`segmented`] control.
#[derive(Debug, Clone)]
pub struct Segment {
    pub label: SharedString,
    /// Small explanatory text under the label.
    pub detail: SharedString,
}

/// A radio group drawn as side-by-side options. It is one tab stop: Left and Right (or Up and
/// Down) move the selection, Space or Enter keep it. Clicking an option selects it.
///
/// `id` prefixes the test labels (`{id}-{index}`).
pub fn segmented(
    theme: &Theme,
    id: &'static str,
    segments: &[Segment],
    selected: usize,
    focus: &FocusHandle,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let colors = theme.colors;
    let count = segments.len();
    let on_select = Rc::new(on_select);
    let on_key = on_select.clone();
    let mut row = div()
        .id(ElementId::Name(id.into()))
        .debug_selector(move || id.to_owned())
        .track_focus(focus)
        .flex()
        .gap_2()
        .p_0p5()
        .rounded(theme.radii.medium)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(move |style| style.border_color(colors.focus_ring))
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if event.is_held || count == 0 {
                return;
            }
            let next = match event.keystroke.key.as_str() {
                "left" | "up" => Some((selected + count - 1) % count),
                "right" | "down" => Some((selected + 1) % count),
                _ => None,
            };
            if let Some(next) = next {
                on_key(next, window, cx);
                cx.stop_propagation();
            }
        });
    for (index, segment) in segments.iter().enumerate() {
        let is_selected = index == selected;
        let on_select = on_select.clone();
        let name = format!("{id}-{index}");
        row = row.child(
            div()
                .id(ElementId::Name(name.clone().into()))
                .debug_selector(move || name)
                .flex_1()
                .min_w_0()
                .cursor_pointer()
                .px_3()
                .py_2()
                .rounded(theme.radii.medium)
                .border_1()
                .when(is_selected, |option| {
                    option.border_color(colors.accent).bg(colors.surface_raised)
                })
                .when(!is_selected, |option| {
                    option
                        .border_color(colors.border)
                        .hover(move |style| style.bg(colors.surface_hover))
                })
                .on_click(move |_, window, cx| on_select(index, window, cx))
                .child(
                    div()
                        .text_size(theme.type_scale.body)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if is_selected {
                            colors.accent
                        } else {
                            colors.text
                        })
                        .child(segment.label.clone()),
                )
                .children((!segment.detail.is_empty()).then(|| {
                    div()
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_muted)
                        .child(segment.detail.clone())
                })),
        );
    }
    row
}

/// A compact toggle of two or more choices, one of them chosen, as one tab stop: Left and Right
/// move the choice (Enter and Space step to the next, wrapping), and a click chooses. `options`
/// are `(label, selector)`; `aria` names the group for a screen reader.
pub fn toggle_group(
    theme: &Theme,
    id: &'static str,
    aria: &'static str,
    options: &[(&'static str, &'static str)],
    selected: usize,
    focus: &FocusHandle,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let colors = theme.colors;
    let count = options.len();
    let on_select = Rc::new(on_select);
    let on_key = on_select.clone();
    let mut group = div()
        .id(id)
        .debug_selector(move || id.into())
        .role(Role::RadioGroup)
        .aria_label(aria)
        .track_focus(focus)
        .flex_none()
        .flex()
        .items_center()
        .gap_0p5()
        .p_0p5()
        .rounded(theme.radii.medium)
        .bg(colors.surface_hover)
        .border_1()
        .border_color(transparent_black())
        .focus_visible(move |style| style.border_color(colors.focus_ring))
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if event.keystroke.modifiers.modified() || count == 0 {
                return;
            }
            let next = match event.keystroke.key.as_str() {
                "left" if selected > 0 => selected - 1,
                "right" if selected + 1 < count => selected + 1,
                "enter" | "space" if !event.is_held => (selected + 1) % count,
                _ => return,
            };
            on_key(next, window, cx);
            cx.stop_propagation();
        });
    for (index, (label, selector)) in options.iter().copied().enumerate() {
        let chosen = index == selected;
        let on_select = on_select.clone();
        group = group.child(
            div()
                .id(selector)
                .debug_selector(move || selector.into())
                .role(Role::RadioButton)
                .aria_label(label)
                .px_2()
                .py_0p5()
                .rounded(theme.radii.small)
                .text_size(theme.type_scale.small)
                .cursor_pointer()
                .when(chosen, |d| {
                    d.bg(colors.surface_raised)
                        .border_1()
                        .border_color(colors.border_strong)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text)
                })
                .when(!chosen, |d| d.text_color(colors.text_muted))
                .on_click(move |_, window, cx| on_select(index, window, cx))
                .child(label),
        );
    }
    group
}
