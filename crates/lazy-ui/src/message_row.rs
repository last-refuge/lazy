//! One row of the message timeline: avatar, author, time, body, and delivery status.
//!
//! Layout decisions (day separators, whether to repeat the author header) are computed by pure
//! functions from the neighboring rows, so they are testable without a window and cheap enough
//! to evaluate for each visible row on every frame.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClipboardItem, ElementId, FocusHandle, FontWeight, SharedString, Window, div,
    prelude::*, px,
};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Zoned};
use lazy_client_core::{Delivery, ItemKey, RetryKind, TimelineItem};
use lazy_domain::{Block, Inline};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::{Theme, avatar_color};
use crate::time_fmt::{clock_label, day_label, to_zoned};

/// Messages from the same author closer together than this share one header.
const GROUP_WINDOW: SignedDuration = SignedDuration::from_mins(5);
const AVATAR: f32 = 36.0;

/// How a row presents itself relative to the row above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowLayout {
    /// A date heading rendered above the row when the calendar day changes.
    pub day_separator: Option<String>,
    /// Whether to show the avatar, name, and time, or continue the previous author's run.
    pub show_header: bool,
}

/// Local time of a row. Rows that are not yet confirmed have no server time and are treated as
/// sent `now`.
fn row_time(item: &TimelineItem, zone: &TimeZone, now: &Zoned) -> Zoned {
    item.created_at()
        .map_or_else(|| now.clone(), |at| to_zoned(at, zone))
}

/// Computes the layout of `item` given the row above it (if any).
pub fn row_layout(
    previous: Option<&TimelineItem>,
    item: &TimelineItem,
    zone: &TimeZone,
    now: &Zoned,
) -> RowLayout {
    let time = row_time(item, zone, now);
    let Some(previous) = previous else {
        return RowLayout {
            day_separator: Some(day_label(time.date(), now.date())),
            show_header: true,
        };
    };
    let previous_time = row_time(previous, zone, now);
    let new_day = previous_time.date() != time.date();
    let gap_exceeded = time
        .timestamp()
        .duration_since(previous_time.timestamp())
        .abs()
        > GROUP_WINDOW;
    RowLayout {
        day_separator: new_day.then(|| day_label(time.date(), now.date())),
        show_header: new_day || previous.author_id != item.author_id || gap_exceeded,
    }
}

/// Plain text of a message body, for copying.
pub fn body_text(item: &TimelineItem) -> String {
    item.document.plain_text()
}

/// Callback run when the person asks to retry a failed message.
pub type RetryHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// Everything needed to draw one row.
pub struct RowProps<'a> {
    /// Position in the timeline, used to label the row for tests.
    pub index: usize,
    pub item: &'a TimelineItem,
    pub author_name: SharedString,
    pub layout: RowLayout,
    pub zone: &'a TimeZone,
    pub now: &'a Zoned,
    pub theme: &'a Theme,
    /// Called when the person asks to retry a failed message.
    pub on_retry: RetryHandler,
    /// Draw the "New messages" divider above this row.
    pub new_messages_divider: bool,
    /// Tab stop for the retry button of a failed message; kept by the timeline so focus survives
    /// redraws.
    pub retry_focus: Option<FocusHandle>,
}

fn element_id(key: ItemKey) -> ElementId {
    match key {
        ItemKey::Operation(id) => ElementId::Name(format!("op-{id}").into()),
        ItemKey::Message(id) => ElementId::Name(format!("msg-{id}").into()),
    }
}

fn initial(name: &str) -> String {
    name.graphemes(true).next().unwrap_or("?").to_uppercase()
}

fn avatar(props: &RowProps<'_>) -> AnyElement {
    div()
        .size(px(AVATAR))
        .flex_none()
        .rounded(props.theme.radii.medium)
        .bg(avatar_color(props.item.author_id.as_uuid().as_u128()))
        .flex()
        .items_center()
        .justify_center()
        .text_color(gpui::white())
        .font_weight(FontWeight::SEMIBOLD)
        .child(initial(&props.author_name))
        .into_any_element()
}

fn day_separator(label: &str, theme: &Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .px_5()
        .pt_4()
        .pb_2()
        .child(div().h(px(1.)).flex_1().bg(theme.colors.border))
        .child(
            div()
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.colors.text_muted)
                .child(label.to_owned()),
        )
        .child(div().h(px(1.)).flex_1().bg(theme.colors.border))
        .into_any_element()
}

fn body(props: &RowProps<'_>) -> AnyElement {
    let theme = props.theme;
    let mut column = div().flex().flex_col().gap_1().min_w_0().w_full();
    for block in props.item.document.blocks() {
        let Block::Paragraph { inlines } = block;
        let mut text = String::new();
        for inline in inlines {
            match inline {
                Inline::Text { text: run } => text.push_str(run),
                Inline::LineBreak => text.push('\n'),
            }
        }
        column = column.child(
            div()
                .text_size(theme.type_scale.body)
                .line_height(theme.type_scale.line_height_body)
                .text_color(theme.colors.text)
                .child(text),
        );
    }
    column.into_any_element()
}

fn status_line(props: &RowProps<'_>) -> Option<AnyElement> {
    let theme = props.theme;
    let small = theme.type_scale.small;
    match &props.item.delivery {
        Delivery::Confirmed { .. } => None,
        Delivery::Pending => Some(
            div()
                .text_size(small)
                .text_color(theme.colors.text_muted)
                .child("Sending…")
                .into_any_element(),
        ),
        Delivery::UnknownOutcome => Some(
            div()
                .text_size(small)
                .text_color(theme.colors.warning)
                .child("Checking whether this was delivered…")
                .into_any_element(),
        ),
        Delivery::Failed { reason, retry } => {
            let label = match retry {
                RetryKind::NewOperation => "Send again",
                RetryKind::SameOperation => "Check and retry",
            };
            let on_retry = props.on_retry.clone();
            let id = format!(
                "retry-{}",
                match props.item.key {
                    ItemKey::Operation(id) => id.to_string(),
                    ItemKey::Message(id) => id.to_string(),
                }
            );
            Some(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(small)
                    .child(
                        div()
                            .text_color(theme.colors.danger)
                            .child(format!("Not sent: {reason}")),
                    )
                    .child(
                        div()
                            .id(ElementId::Name(id.into()))
                            .debug_selector(|| "retry-message".into())
                            .when_some(props.retry_focus.clone(), |button, focus| {
                                button.track_focus(&focus)
                            })
                            .focus_visible(|style| style.border_color(theme.colors.focus_ring))
                            .on_key_down({
                                let on_retry = props.on_retry.clone();
                                move |event, window, cx| {
                                    if crate::keyboard::is_activation(event) {
                                        on_retry(window, cx);
                                        cx.stop_propagation();
                                    }
                                }
                            })
                            .cursor_pointer()
                            .px_2()
                            .py_0p5()
                            .rounded(theme.radii.small)
                            .border_1()
                            .border_color(theme.colors.danger)
                            .text_color(theme.colors.danger)
                            .hover(|style| style.bg(theme.colors.danger_surface))
                            .on_click(move |_, window, cx| on_retry(window, cx))
                            .child(label),
                    )
                    .into_any_element(),
            )
        }
    }
}

/// Draws one timeline row.
pub fn render_message_row(props: RowProps<'_>) -> AnyElement {
    let theme = props.theme;
    let id = element_id(props.item.key);
    let faded = matches!(
        props.item.delivery,
        Delivery::Pending | Delivery::UnknownOutcome
    );
    let time_text = match props.item.created_at() {
        Some(at) => clock_label(&to_zoned(at, props.zone)),
        None => clock_label(props.now),
    };

    let header = props.layout.show_header.then(|| {
        div()
            .flex()
            .items_baseline()
            .gap_2()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.body)
                    .text_color(theme.colors.text)
                    .child(props.author_name.clone()),
            )
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.text_faint)
                    .child(time_text),
            )
    });

    let leading = if props.layout.show_header {
        avatar(&props)
    } else {
        div().w(px(AVATAR)).flex_none().into_any_element()
    };

    let copy_text = body_text(props.item);
    let copy_id = ElementId::Name(format!("copy-{}", id).into());
    let group: SharedString = format!("row-{id}").into();

    let toolbar = div()
        .absolute()
        .top(px(-10.))
        .right(px(20.))
        .invisible()
        .group_hover(group.clone(), |style| style.visible())
        .child(
            div()
                .id(copy_id)
                .debug_selector(|| "copy-message".into())
                .cursor_pointer()
                .px_2()
                .py_0p5()
                .rounded(theme.radii.small)
                .border_1()
                .border_color(theme.colors.border_strong)
                .bg(theme.colors.surface_raised)
                .text_size(theme.type_scale.small)
                .text_color(theme.colors.text_muted)
                .hover(|style| style.text_color(theme.colors.text))
                .on_click(move |_, _window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                })
                .child("Copy"),
        );

    let mut content = div().flex().flex_col().gap_0p5().min_w_0().flex_1();
    if let Some(header) = header {
        content = content.child(header);
    }
    content = content.child(body(&props));
    if let Some(status) = status_line(&props) {
        content = content.child(status);
    }

    let row = div()
        .id(id)
        .debug_selector(|| format!("message-row-{}", props.index))
        .group(group)
        .relative()
        .flex()
        .gap_3()
        .px_5()
        .py(if props.layout.show_header {
            px(6.)
        } else {
            px(1.)
        })
        .hover(|style| style.bg(theme.colors.surface_hover))
        .when(faded, |row| row.opacity(0.7))
        .child(leading)
        .child(content)
        .child(toolbar);

    let divider = props
        .new_messages_divider
        .then(|| new_messages_divider(theme));
    match &props.layout.day_separator {
        Some(label) => div()
            .flex()
            .flex_col()
            .w_full()
            .child(day_separator(label, theme))
            .children(divider)
            .child(row)
            .into_any_element(),
        None if divider.is_some() => div()
            .flex()
            .flex_col()
            .w_full()
            .children(divider)
            .child(row)
            .into_any_element(),
        None => div().w_full().child(row).into_any_element(),
    }
}

/// The line marking where unread messages begin.
fn new_messages_divider(theme: &Theme) -> AnyElement {
    div()
        .debug_selector(|| "new-messages-divider".into())
        .flex()
        .items_center()
        .gap_3()
        .px_5()
        .pt_3()
        .pb_1()
        .child(div().h(px(1.)).flex_1().bg(theme.colors.accent))
        .child(
            div()
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.colors.accent)
                .child("New messages"),
        )
        .child(div().h(px(1.)).flex_1().bg(theme.colors.accent))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp as Instant;
    use lazy_client_core::{Delivery, ItemKey};
    use lazy_domain::{EventSeq, MessageDocument, MessageId, OperationId, Timestamp, UserId};

    use super::*;

    fn item(author: UserId, unix: i64, seq: i64) -> TimelineItem {
        let id = MessageId::generate();
        TimelineItem {
            key: ItemKey::Message(id),
            author_id: author,
            document: MessageDocument::from_plain_text("hi").unwrap(),
            delivery: Delivery::Confirmed {
                message_id: id,
                seq: EventSeq::new(seq).unwrap(),
                created_at: Timestamp::from(
                    time::OffsetDateTime::from_unix_timestamp(unix).unwrap(),
                ),
                revision: 1,
            },
        }
    }

    fn now_at(unix: i64) -> Zoned {
        Instant::from_second(unix).unwrap().to_zoned(TimeZone::UTC)
    }

    const BASE: i64 = 1_800_000_000; // 2027-01-15 08:00:00 UTC

    #[test]
    fn first_row_shows_header_and_day_separator() {
        let a = UserId::generate();
        let layout = row_layout(None, &item(a, BASE, 1), &TimeZone::UTC, &now_at(BASE));
        assert!(layout.show_header);
        assert_eq!(layout.day_separator.as_deref(), Some("Today"));
    }

    #[test]
    fn same_author_within_window_continues_the_run() {
        let a = UserId::generate();
        let first = item(a, BASE, 1);
        let second = item(a, BASE + 60, 2);
        let layout = row_layout(Some(&first), &second, &TimeZone::UTC, &now_at(BASE + 120));
        assert!(!layout.show_header);
        assert_eq!(layout.day_separator, None);
    }

    #[test]
    fn different_author_or_long_gap_starts_a_new_run() {
        let (a, b) = (UserId::generate(), UserId::generate());
        let first = item(a, BASE, 1);
        let zone = TimeZone::UTC;
        let now = now_at(BASE + 3_600);
        assert!(row_layout(Some(&first), &item(b, BASE + 10, 2), &zone, &now).show_header);
        assert!(row_layout(Some(&first), &item(a, BASE + 600, 2), &zone, &now).show_header);
    }

    #[test]
    fn a_new_calendar_day_adds_a_separator() {
        let a = UserId::generate();
        let first = item(a, BASE, 1);
        let next_day = item(a, BASE + 86_400, 2);
        let layout = row_layout(
            Some(&first),
            &next_day,
            &TimeZone::UTC,
            &now_at(BASE + 86_400),
        );
        assert!(layout.show_header);
        assert_eq!(layout.day_separator.as_deref(), Some("Today"));
    }

    #[test]
    fn unconfirmed_rows_are_treated_as_sent_now() {
        let a = UserId::generate();
        let first = item(a, BASE, 1);
        let pending = TimelineItem {
            key: ItemKey::Operation(OperationId::generate()),
            author_id: a,
            document: MessageDocument::from_plain_text("sending").unwrap(),
            delivery: Delivery::Pending,
        };
        let layout = row_layout(Some(&first), &pending, &TimeZone::UTC, &now_at(BASE + 30));
        assert!(!layout.show_header);
    }

    #[test]
    fn initials_respect_grapheme_clusters() {
        assert_eq!(initial("zoë"), "Z");
        assert_eq!(initial("👩‍👩‍👧‍👦 Family"), "👩‍👩‍👧‍👦");
        assert_eq!(initial(""), "?");
    }
}
