//! An integration's rich content (D40) drawn with Lazy's own components: a header, sections with
//! fields and a button beside them, dividers, quiet context lines, and rows of buttons. Nothing
//! remote is loaded; text is drawn as message text is ([`render_document`]), but is not part of the
//! message's selectable text (copying a message copies its fallback text).

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, App, ElementId, FontWeight, SharedString, Window, div, px};
use lazy_domain::{Block, Button, ButtonStyle, Inline, MessageDocument, RichBlock, RichContent};

use crate::rich_text::{DocumentProps, LinkHandler, LinkTarget, MentionLabels, render_document};
use crate::theme::Theme;

/// Called when a button that goes to the app is pressed: its block, and the button.
pub type PressHandler = Rc<dyn Fn(u32, &Button, &mut Window, &mut App)>;

/// What a message's rich content needs to be drawn.
pub struct RichProps<'a> {
    pub content: &'a RichContent,
    pub theme: &'a Theme,
    /// Distinguishes this message's controls from other messages'.
    pub id: ElementId,
    /// Put before test labels (`thread-` in the thread pane), like the row's own.
    pub label_prefix: &'static str,
    /// The row's position, for test labels.
    pub index: usize,
    pub on_link: LinkHandler,
    pub mentions: &'a dyn MentionLabels,
    /// `None` where buttons cannot be pressed (a preview): they are drawn, but inert.
    pub on_press: Option<PressHandler>,
    /// The button waiting for the app's answer, as (block, action id).
    pub pressing: Option<(u32, &'a str)>,
}

/// Said under content with a block this build cannot draw.
pub const UNSUPPORTED_NOTE: &str = "Part of this message needs a newer version of Lazy.";

fn document(
    props: &RichProps<'_>,
    document: &MessageDocument,
    part: String,
    muted: bool,
) -> AnyElement {
    render_document(DocumentProps {
        document,
        theme: props.theme,
        id: ElementId::Name(format!("{}-{part}", props.id).into()),
        on_link: props.on_link.clone(),
        mentions: props.mentions,
        color: if muted {
            props.theme.colors.text_muted
        } else {
            props.theme.colors.text
        },
        select: None,
    })
}

/// Context items that are each one paragraph, as one paragraph joined by ` · `: the quiet line
/// they are meant to be.
fn one_line(items: &[MessageDocument]) -> Option<MessageDocument> {
    let mut inlines = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let [Block::Paragraph { inlines: part }] = item.blocks() else {
            return None;
        };
        if i > 0 {
            inlines.push(Inline::text(" \u{b7} "));
        }
        inlines.extend(part.iter().cloned());
    }
    MessageDocument::new(vec![Block::Paragraph { inlines }]).ok()
}

fn button(props: &RichProps<'_>, block: u32, button: &Button) -> AnyElement {
    let theme = props.theme;
    let colors = theme.colors;
    let waiting = props.pressing == Some((block, button.action_id.as_str()));
    let busy = props.pressing.is_some();
    let link = button.url.as_deref().and_then(LinkTarget::classify);
    let (bg, fg, border, hover_bg) = match button.style {
        ButtonStyle::Primary => (
            colors.accent,
            colors.on_accent,
            colors.accent,
            colors.accent_hover,
        ),
        ButtonStyle::Danger => (
            colors.danger_surface,
            colors.danger,
            colors.danger,
            colors.danger_surface,
        ),
        ButtonStyle::Default => (
            colors.surface_raised,
            colors.text,
            colors.border_strong,
            colors.surface_hover,
        ),
    };
    let prefix = props.label_prefix;
    let index = props.index;
    let action = button.action_id.clone();
    let label: SharedString = if waiting {
        format!("{}…", button.text).into()
    } else {
        button.text.clone().into()
    };
    let aria = match (&link, waiting) {
        (_, true) => format!("{}, waiting for the app", button.text),
        (Some(_), false) => format!("{} (opens a link)", button.text),
        (None, false) => button.text.clone(),
    };
    // A link opens whatever is going on; a press waits for the one before it.
    let enabled = link.is_some() || (!busy && props.on_press.is_some());
    let mut element = div()
        .id(ElementId::Name(
            format!("{}-button-{block}-{action}", props.id).into(),
        ))
        .debug_selector(move || format!("{prefix}rich-button-{index}-{block}-{action}"))
        .role(gpui::Role::Button)
        .aria_label(aria)
        .flex()
        .items_center()
        .gap_1()
        .h(px(28.))
        .px_3()
        .rounded(theme.radii.small)
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_size(theme.type_scale.small)
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .child(label)
        .when(link.is_some(), |el| {
            el.child(div().text_color(fg.opacity(0.7)).child("↗"))
        });
    if enabled {
        element = element
            .cursor_pointer()
            .hover(move |style| style.bg(hover_bg));
        match link {
            Some(target) => {
                let on_link = props.on_link.clone();
                element =
                    element.on_click(move |_, window, cx| on_link(target.clone(), window, cx));
            }
            None => {
                if let Some(on_press) = props.on_press.clone() {
                    let pressed = button.clone();
                    element = element.on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        on_press(block, &pressed, window, cx)
                    });
                }
            }
        }
    } else {
        element = element.opacity(if waiting { 0.8 } else { 0.55 });
    }
    element.into_any_element()
}

fn block(props: &RichProps<'_>, n: usize, value: &RichBlock) -> Option<AnyElement> {
    let theme = props.theme;
    let colors = theme.colors;
    let at = u32::try_from(n).unwrap_or(u32::MAX);
    Some(match value {
        RichBlock::Header { text } => div()
            .debug_selector(|| "rich-header".into())
            .text_size(theme.type_scale.title)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(colors.text)
            .child(text.clone())
            .into_any_element(),
        RichBlock::Section {
            text,
            fields,
            accessory,
        } => {
            // Wide enough for its text before a button sits beside it; narrower, the button
            // wraps below.
            let mut main = div().flex().flex_col().gap_1().flex_1().min_w(px(260.));
            if let Some(text) = text {
                main = main.child(document(props, text, format!("{n}-text"), false));
            }
            if !fields.is_empty() {
                main = main.child(
                    div()
                        .debug_selector(|| "rich-fields".into())
                        .flex()
                        .flex_wrap()
                        .gap_y_1()
                        .children(fields.iter().enumerate().map(|(f, field)| {
                            div().w_1_2().pr_3().min_w_0().child(document(
                                props,
                                field,
                                format!("{n}-field-{f}"),
                                false,
                            ))
                        })),
                );
            }
            div()
                .debug_selector(|| "rich-section".into())
                .flex()
                .flex_wrap()
                .items_start()
                .gap_x_3()
                .gap_y_2()
                .child(main)
                .children(
                    accessory
                        .as_ref()
                        .map(|b| div().flex_none().child(button(props, at, b))),
                )
                .into_any_element()
        }
        RichBlock::Divider => div()
            .debug_selector(|| "rich-divider".into())
            .my_1()
            .h(px(1.))
            .bg(colors.border)
            .into_any_element(),
        RichBlock::Context { items } => {
            let line =
                match one_line(items) {
                    Some(line) => document(props, &line, format!("{n}-context"), true),
                    None => div()
                        .flex()
                        .flex_col()
                        .children(items.iter().enumerate().map(|(i, item)| {
                            document(props, item, format!("{n}-context-{i}"), true)
                        }))
                        .into_any_element(),
                };
            div()
                .debug_selector(|| "rich-context".into())
                .child(line)
                .into_any_element()
        }
        RichBlock::Actions { buttons, .. } => div()
            .debug_selector(|| "rich-actions".into())
            .flex()
            .flex_wrap()
            .gap_2()
            .pt_0p5()
            .children(buttons.iter().map(|b| button(props, at, b)))
            .into_any_element(),
        RichBlock::Unsupported => return None,
    })
}

/// Draws the content, top to bottom.
pub fn render_rich(props: RichProps<'_>) -> AnyElement {
    let theme = props.theme;
    let blocks = props
        .content
        .blocks()
        .iter()
        .enumerate()
        .filter_map(|(n, value)| block(&props, n, value))
        .collect::<Vec<_>>();
    div()
        .debug_selector(|| "rich-content".into())
        .flex()
        .flex_col()
        .gap_2()
        .py_0p5()
        .children(blocks)
        .when(props.content.has_unsupported(), |el| {
            el.child(
                div()
                    .debug_selector(|| "rich-unsupported".into())
                    .text_size(theme.type_scale.small)
                    .italic()
                    .text_color(theme.colors.text_faint)
                    .child(UNSUPPORTED_NOTE),
            )
        })
        .into_any_element()
}
