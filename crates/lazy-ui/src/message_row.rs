//! One row of the message timeline: avatar, author, time, body, and delivery status.
//!
//! Layout decisions (day separators, whether to repeat the author header) are computed by pure
//! functions from the neighboring rows, so they are testable without a window and cheap enough
//! to evaluate for each visible row on every frame.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ElementId, Entity, FocusHandle, FontWeight, Pixels, Point, Role, SharedString,
    Window, div, prelude::*, px,
};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Zoned};
use lazy_client_core::{Delivery, EditState, ItemKey, RetryKind, TimelineItem};
use lazy_domain::ReactionEmoji;

use crate::emoji_font;
use crate::form::{self, ButtonKind};
use crate::reactions::Chip;
use crate::rich_text::{DocumentProps, LinkHandler, MentionLabels, SelectProps, render_document};
use crate::suggest::{self, PickSuggestion, Suggestions};
use crate::text_input::TextInput;
use crate::theme::{Theme, avatar_color};
use crate::time_fmt::{clock_label, day_label, to_zoned};
use crate::tooltip::TextTooltip;

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

/// Something a person can do to a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowAction {
    /// Open the emoji picker to add a reaction.
    React,
    /// Flip the person's reaction with this emoji (an item for each reaction in the menu).
    ToggleReaction(ReactionEmoji),
    /// Open the message's thread, to read or add replies.
    Reply,
    Edit,
    Delete,
    CopyLink,
    CopyText,
    /// Keep the message in the saved list.
    Save,
    /// Take it out of the saved list again.
    Unsave,
    /// Open the author's profile.
    ViewProfile,
    /// Save the message's one file (offered in the menu for a message with a single file).
    DownloadFile,
    /// Copy the name of the message's one file.
    CopyFileName,
}

impl RowAction {
    /// The menu wording. A reaction toggle's wording depends on its count, so the menu supplies it.
    pub fn label(self) -> &'static str {
        match self {
            Self::React => "Add reaction…",
            Self::ToggleReaction(_) => "Reaction",
            Self::Reply => "Reply in thread",
            Self::Edit => "Edit",
            Self::Delete => "Delete",
            Self::CopyLink => "Copy link",
            Self::CopyText => "Copy text",
            Self::Save => crate::states::SAVE_FOR_LATER,
            Self::Unsave => crate::states::REMOVE_FROM_SAVED,
            Self::ViewProfile => "View profile",
            Self::DownloadFile => "Download",
            Self::CopyFileName => "Copy file name",
        }
    }

    /// Stable name for tests and menu items.
    pub fn id(self) -> String {
        match self {
            Self::React => "react".to_owned(),
            Self::ToggleReaction(emoji) => format!("react-{emoji}"),
            Self::Reply => "reply".to_owned(),
            Self::Edit => "edit".to_owned(),
            Self::Delete => "delete".to_owned(),
            Self::CopyLink => "copy-link".to_owned(),
            Self::CopyText => "copy-text".to_owned(),
            Self::Save => "save".to_owned(),
            Self::Unsave => "unsave".to_owned(),
            Self::ViewProfile => "view-profile".to_owned(),
            Self::DownloadFile => "download-file".to_owned(),
            Self::CopyFileName => "copy-file-name".to_owned(),
        }
    }
}

/// Called when the person clicks a reaction chip.
pub type ReactionHandler = Rc<dyn Fn(ReactionEmoji, &mut Window, &mut App)>;

/// Called with the window position of the control the person clicked, to open something beside it.
pub type AnchorHandler = Rc<dyn Fn(Point<Pixels>, &mut Window, &mut App)>;

/// Called when a person picks an action for a row, from the hover toolbar or the keyboard menu.
pub type ActionHandler = Rc<dyn Fn(RowAction, &mut Window, &mut App)>;

/// What the person may do to one message, and where to report it.
pub struct RowActions {
    /// In toolbar order.
    pub available: Vec<RowAction>,
    pub on_action: ActionHandler,
}

/// What the editor asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorCommand {
    Save,
    Cancel,
    /// Delete the message, offered when the text was emptied.
    Delete,
    /// Dismiss the "can't be empty" prompt and go back to the text.
    KeepEditing,
    /// Save over a newer version, after a conflict.
    SaveAnyway,
}

pub type EditorHandler = Rc<dyn Fn(EditorCommand, &mut Window, &mut App)>;

/// Where the editor is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorPhase {
    Editing,
    /// Sent to the server; waiting to hear.
    Saving,
    /// The text was emptied; offer to delete instead.
    EmptyPrompt,
}

/// Replaces a row's body with an editor.
pub struct EditorProps {
    pub input: Entity<TextInput>,
    pub phase: EditorPhase,
    /// Why the last save did not happen, shown above the text.
    pub notice: EditorNotice,
    pub on_command: EditorHandler,
    pub save_focus: FocusHandle,
    pub cancel_focus: FocusHandle,
    pub delete_focus: FocusHandle,
    pub keep_focus: FocusHandle,
    /// The shortcode suggestions to show under the text, when the person is typing one.
    pub suggestions: Option<Suggestions>,
    /// The pictures of the custom emoji among the suggestions.
    pub suggestion_pictures: crate::suggest::Pictures,
    pub on_pick_suggestion: PickSuggestion,
}

/// A problem with the last save attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorNotice {
    None,
    /// The save failed; the text is still here.
    Failed(String),
    /// Someone changed the message first. The row's own document is the newer version.
    Conflict,
}

/// One person shown as a small avatar on a thread's summary line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplierAvatar {
    /// Seed for the avatar color: the person's id.
    pub seed: u128,
    pub initial: String,
    pub name: String,
}

/// The line under a root message in the conversation that opens its thread: who replied, how many
/// replies there are, and when the latest came. The whole line is one button.
pub struct ThreadLineProps {
    pub replies: u32,
    /// Up to five, most recent first.
    pub repliers: Vec<ReplierAvatar>,
    /// `5 minutes ago`: when the newest reply came.
    pub last_reply: String,
    /// The person follows the thread and has replies they have not read.
    pub unread: bool,
    /// The keyboard cursor is on this line (it is its own stop between two messages).
    pub focused: bool,
    pub on_open: RetryHandler,
}

impl ThreadLineProps {
    /// `1 reply` or `3 replies`.
    pub fn count_label(&self) -> String {
        match self.replies {
            1 => "1 reply".to_owned(),
            n => format!("{n} replies"),
        }
    }

    /// What a screen reader says for the line.
    pub fn aria_label(&self) -> String {
        format!(
            "{}, last reply {}{}. Open thread",
            self.count_label(),
            self.last_reply,
            if self.unread { ", unread" } else { "" }
        )
    }
}

/// The small header over a reply that was also sent to the channel: it says what the message is
/// and quotes the root, and opens the thread.
pub struct ReplyHeaderProps {
    /// The root's author and the start of its text, when the root is known.
    pub excerpt: Option<(SharedString, SharedString)>,
    pub on_open: RetryHandler,
}

/// How a row is drawn inside the thread pane instead of the conversation.
pub struct PaneRowProps {
    /// Under the root message: `3 replies`, or `No replies yet`.
    pub root_divider: Option<String>,
    /// On a reply that was also sent to the channel: `Also sent to #general`.
    pub also_sent: Option<String>,
}

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
    pub on_link: LinkHandler,
    pub actions: RowActions,
    /// The message's reactions, as chips.
    pub chips: Vec<Chip>,
    /// Whether the person may react: chips toggle only then, and the add button shows.
    pub can_react: bool,
    pub on_toggle_reaction: ReactionHandler,
    pub on_add_reaction: AnchorHandler,
    /// The row holds the timeline's keyboard cursor, and the timeline has focus.
    pub focused: bool,
    /// The row was just jumped to, for example from a permalink.
    pub highlighted: bool,
    /// Set while this message is being edited.
    pub editor: Option<EditorProps>,
    /// Called for "Edit again" on an edit that was not saved and whose editor is closed.
    pub on_edit_again: RetryHandler,
    /// How the message's mentions read.
    pub mentions: &'a dyn MentionLabels,
    /// The message notifies the person looking (someone else's message that mentions them, one
    /// of their groups, or `@channel`): the row is tinted.
    pub mentions_me: bool,
    /// The summary line of the thread this message starts, in the conversation.
    pub thread_line: Option<ThreadLineProps>,
    /// The header over a reply that was also sent to the channel.
    pub reply_header: Option<ReplyHeaderProps>,
    /// The message was deleted but its replies remain: a muted placeholder, no actions.
    pub tombstone: bool,
    /// Present when the row is drawn in the thread pane.
    pub pane: Option<PaneRowProps>,
    /// The author is an app (an incoming webhook's bot): its name gets an APP badge.
    pub author_is_bot: bool,
    /// The author's status, shown as an emoji beside the name.
    pub author_status: Option<crate::status_mark::StatusMark>,
    /// The author's picture, when the cache has it; their initial is drawn otherwise.
    pub author_avatar: Option<std::sync::Arc<gpui::RenderImage>>,
    /// The author's membership is deactivated: a quiet badge says so beside the name.
    pub author_deactivated: bool,
    /// The person has saved the message: a bookmark shows beside its time.
    pub saved: bool,
    /// Called when the author's name or picture is clicked, to open their profile.
    pub on_profile: RetryHandler,
    /// The message's files, already drawn (the timeline knows their previews and downloads).
    pub attachments: Vec<AnyElement>,
    /// `Uploading 2 of 3 files`, while the message waits for its files.
    pub upload_note: Option<String>,
    /// Called for "Discard" on a message that failed to send.
    pub on_discard: RetryHandler,
    /// Tab stop for the Discard button of a failed message.
    pub discard_focus: Option<FocusHandle>,
    /// Selected text, and where to record where the text was drawn.
    pub select: Option<SelectProps<'a>>,
    /// Put before the test labels of the row's own controls, so a message shown in the thread
    /// pane and in the conversation at once can be told apart: empty in the conversation,
    /// `thread-` in the pane.
    pub label_prefix: &'static str,
    /// Pressing a button in the message's rich content (D40); `None` where it cannot be.
    pub on_press: Option<crate::rich_blocks::PressHandler>,
    /// The rich-content button of this message waiting for its app, as (block, action id).
    pub pressing: Option<(u32, &'a str)>,
    /// What the pages the message links to say about themselves (D41), drawn under the text.
    pub link_previews: &'a [lazy_protocol::LinkPreview],
}

/// The width a row spends beside its content: the padding on both sides, the avatar, and the
/// gap after it.
pub fn content_inset() -> f32 {
    2.0 * 20.0 + AVATAR + 12.0
}

fn element_id(key: ItemKey) -> ElementId {
    match key {
        ItemKey::Operation(id) => ElementId::Name(format!("op-{id}").into()),
        ItemKey::Message(id) => ElementId::Name(format!("msg-{id}").into()),
    }
}

fn avatar(props: &RowProps<'_>) -> AnyElement {
    let on_profile = props.on_profile.clone();
    crate::avatar::Avatar::new(
        props.item.author_id.as_uuid().as_u128(),
        props.author_name.clone(),
        AVATAR,
    )
    .text_size(props.theme.type_scale.body)
    .image(props.author_avatar.clone())
    .into_div()
    .id(ElementId::NamedInteger(
        "author-avatar".into(),
        props.index as u64,
    ))
    .debug_selector({
        let prefix = props.label_prefix;
        let index = props.index;
        move || format!("{prefix}author-avatar-{index}")
    })
    .cursor_pointer()
    .on_click(move |_, window, cx| on_profile(window, cx))
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

fn body(props: &RowProps<'_>, id: &ElementId) -> AnyElement {
    if props.tombstone {
        return div()
            .debug_selector(|| "deleted-message".into())
            .text_size(props.theme.type_scale.body)
            .italic()
            .text_color(props.theme.colors.text_muted)
            .child("This message was deleted")
            .into_any_element();
    }
    render_document(DocumentProps {
        document: &props.item.document,
        theme: props.theme,
        id: id.clone(),
        on_link: props.on_link.clone(),
        mentions: props.mentions,
        color: props.theme.colors.text,
        select: props.select.as_ref().map(|select| SelectProps {
            key: select.key,
            range: select.range.clone(),
            sink: select.sink,
            epoch: select.epoch,
        }),
    })
}

/// A quiet card for each linked page: its site, its title (which opens the link), and the start of
/// its description. Plain text only; nothing is loaded from the page.
fn preview_cards(props: &RowProps<'_>) -> AnyElement {
    let theme = props.theme;
    let colors = theme.colors;
    let prefix = props.label_prefix;
    let index = props.index;
    div()
        .flex()
        .flex_col()
        .gap_1p5()
        .pt_1()
        .children(props.link_previews.iter().enumerate().map(|(n, preview)| {
            let target = crate::rich_text::LinkTarget::classify(&preview.url);
            let on_link = props.on_link.clone();
            let title = preview.title.clone().unwrap_or_else(|| preview.url.clone());
            let aria = format!(
                "Link preview: {title}{}",
                preview
                    .site_name
                    .as_ref()
                    .map(|site| format!(", from {site}"))
                    .unwrap_or_default()
            );
            div()
                .debug_selector(move || format!("{prefix}link-preview-{index}-{n}"))
                .max_w(px(520.))
                .pl_3()
                .py_0p5()
                .border_l_4()
                .border_color(colors.quote_bar)
                .flex()
                .flex_col()
                .gap_0p5()
                .children(preview.site_name.clone().map(|site| {
                    div()
                        .text_size(theme.type_scale.small)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text_muted)
                        .truncate()
                        .child(site)
                }))
                .child(
                    div()
                        .id(ElementId::NamedInteger(
                            format!("link-preview-title-{n}").into(),
                            index as u64,
                        ))
                        .role(gpui::Role::Link)
                        .aria_label(aria)
                        .text_size(theme.type_scale.body)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.accent)
                        .line_clamp(2)
                        .when_some(target, |el, target| {
                            el.cursor_pointer()
                                .hover(|style| style.underline())
                                .on_click(move |_, window, cx| on_link(target.clone(), window, cx))
                        })
                        .child(title),
                )
                .children(preview.description.clone().map(|text| {
                    div()
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_muted)
                        .line_clamp(2)
                        .child(text)
                }))
        }))
        .into_any_element()
}

fn small_button(
    theme: &Theme,
    id: ElementId,
    selector: String,
    label: &'static str,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .debug_selector(move || selector)
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
        .on_click(move |_, window, cx| on_click(window, cx))
        .child(label)
        .into_any_element()
}

/// The lines under a message body that explain where it stands: delivery, and edits.
fn status_lines(props: &RowProps<'_>, id: &ElementId) -> Vec<AnyElement> {
    let theme = props.theme;
    let small = theme.type_scale.small;
    let mut lines = Vec::new();
    match &props.item.delivery {
        Delivery::Confirmed { .. } => {}
        Delivery::Pending => lines.push(
            div()
                .debug_selector(|| "sending-note".into())
                .text_size(small)
                .text_color(theme.colors.text_muted)
                .child(
                    props
                        .upload_note
                        .clone()
                        .unwrap_or_else(|| "Sending…".to_owned()),
                )
                .into_any_element(),
        ),
        Delivery::UnknownOutcome => lines.push(
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
            let retry_id = format!("retry-{id}");
            lines.push(
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
                            .id(ElementId::Name(retry_id.into()))
                            .debug_selector(|| format!("{}retry-message", props.label_prefix))
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
                    .child({
                        let on_discard = props.on_discard.clone();
                        let key_discard = props.on_discard.clone();
                        div()
                            .id(ElementId::Name(format!("discard-{id}").into()))
                            .debug_selector(|| format!("{}discard-message", props.label_prefix))
                            .when_some(props.discard_focus.clone(), |button, focus| {
                                button.track_focus(&focus)
                            })
                            .focus_visible(|style| style.border_color(theme.colors.focus_ring))
                            .on_key_down(move |event, window, cx| {
                                if crate::keyboard::is_activation(event) {
                                    key_discard(window, cx);
                                    cx.stop_propagation();
                                }
                            })
                            .cursor_pointer()
                            .px_2()
                            .py_0p5()
                            .rounded(theme.radii.small)
                            .border_1()
                            .border_color(theme.colors.border_strong)
                            .text_color(theme.colors.text_muted)
                            .hover(|style| style.bg(theme.colors.surface_hover))
                            .on_click(move |_, window, cx| on_discard(window, cx))
                            .child("Discard")
                    })
                    .into_any_element(),
            );
        }
    }
    if props.editor.is_none() {
        let edit_again = |message: String, color, theme: &Theme| {
            let on_edit_again = props.on_edit_again.clone();
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(small)
                .child(div().text_color(color).child(message))
                .child(small_button(
                    theme,
                    ElementId::Name(format!("edit-again-{id}").into()),
                    "edit-again".to_owned(),
                    "Edit again",
                    move |window, cx| on_edit_again(window, cx),
                ))
                .into_any_element()
        };
        match &props.item.edit {
            EditState::None => {}
            EditState::Saving => lines.push(
                div()
                    .debug_selector(|| "edit-saving".into())
                    .text_size(small)
                    .text_color(theme.colors.text_muted)
                    .child("Saving…")
                    .into_any_element(),
            ),
            EditState::Failed { reason, .. } => lines.push(edit_again(
                format!("Edit not saved: {reason}"),
                theme.colors.danger,
                theme,
            )),
            EditState::Conflict { .. } => lines.push(edit_again(
                "Someone else changed this message first, so your edit was not applied.".to_owned(),
                theme.colors.warning,
                theme,
            )),
        }
    }
    lines
}

/// The editor that replaces a message's body while it is being edited.
fn editor_body(props: &RowProps<'_>, editor: &EditorProps, id: &ElementId) -> AnyElement {
    let theme = props.theme;
    let colors = theme.colors;
    let small = theme.type_scale.small;
    let command = |which: EditorCommand| {
        let handler = editor.on_command.clone();
        move |window: &mut Window, cx: &mut App| handler(which, window, cx)
    };
    let saving = editor.phase == EditorPhase::Saving;
    let mut column = div()
        .debug_selector(|| format!("{}message-editor", props.label_prefix))
        .flex()
        .flex_col()
        .gap_2()
        .min_w_0()
        .w_full()
        // Escape belongs to an input method that is composing; otherwise it cancels.
        .on_key_down({
            let handler = editor.on_command.clone();
            let input = editor.input.clone();
            move |event, window, cx| {
                if event.keystroke.key == "escape"
                    && event.keystroke.modifiers == gpui::Modifiers::default()
                    && !input.read(cx).is_composing()
                {
                    handler(EditorCommand::Cancel, window, cx);
                    cx.stop_propagation();
                }
            }
        });

    match &editor.notice {
        EditorNotice::None => {}
        EditorNotice::Failed(reason) => {
            column = column.child(
                div()
                    .debug_selector(|| "edit-failed".into())
                    .text_size(small)
                    .text_color(colors.danger)
                    .child(format!("Not saved: {reason}")),
            );
        }
        EditorNotice::Conflict => {
            column =
                column.child(
                    div()
                        .debug_selector(|| "edit-conflict".into())
                        .flex()
                        .flex_col()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .rounded(theme.radii.medium)
                        .bg(colors.warning_surface)
                        .child(
                            div()
                                .text_size(small)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.warning)
                                .child("Someone changed this message while you were editing it."),
                        )
                        .child(div().text_size(small).text_color(colors.text_muted).child(
                            "Your text is below, and nothing was lost. This is their version:",
                        ))
                        .child(render_document(DocumentProps {
                            document: &props.item.document,
                            theme,
                            id: ElementId::Name(format!("current-{id}").into()),
                            on_link: props.on_link.clone(),
                            mentions: props.mentions,
                            color: colors.text_muted,
                            select: None,
                        })),
                );
        }
    }

    column = column.child(
        div()
            .px_3()
            .py_2()
            .rounded(theme.radii.medium)
            .bg(colors.surface_raised)
            .border_1()
            .border_color(colors.focus_ring)
            .when(saving, |frame| frame.opacity(0.7))
            .child(editor.input.clone()),
    );
    if let Some(suggestions) = editor.suggestions.as_ref().filter(|s| s.visible()) {
        column = column.child(suggest::render_list(
            suggestions,
            &editor.suggestion_pictures,
            theme,
            "editor-suggestions",
            editor.on_pick_suggestion.clone(),
        ));
    }

    column = match editor.phase {
        EditorPhase::EmptyPrompt => column.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .text_size(small)
                        .text_color(colors.text_muted)
                        .child("A message can't be empty. Delete it instead?"),
                )
                .child(form::button(
                    theme,
                    "edit-keep",
                    "Keep editing",
                    ButtonKind::Secondary,
                    true,
                    &editor.keep_focus,
                    command(EditorCommand::KeepEditing),
                ))
                .child(form::button(
                    theme,
                    "edit-delete",
                    "Delete message",
                    ButtonKind::Danger,
                    true,
                    &editor.delete_focus,
                    command(EditorCommand::Delete),
                )),
        ),
        EditorPhase::Editing | EditorPhase::Saving => {
            let conflict = editor.notice == EditorNotice::Conflict;
            column.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(small)
                            .text_color(colors.text_faint)
                            .child(if saving {
                                "Saving…"
                            } else {
                                "Enter to save · Esc to cancel · Shift+Enter for a new line"
                            }),
                    )
                    .child(form::button(
                        theme,
                        "edit-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        !saving,
                        &editor.cancel_focus,
                        command(EditorCommand::Cancel),
                    ))
                    .child(form::button(
                        theme,
                        "edit-save",
                        if conflict {
                            "Replace their change"
                        } else {
                            "Save"
                        },
                        ButtonKind::Primary,
                        !saving,
                        &editor.save_focus,
                        command(if conflict {
                            EditorCommand::SaveAnyway
                        } else {
                            EditorCommand::Save
                        }),
                    )),
            )
        }
    };
    column.into_any_element()
}

/// The reaction chips under a message, wrapping onto more lines as needed, and a trailing button to
/// add another when the person may react.
fn reaction_chips(props: &RowProps<'_>, id: &ElementId) -> Option<AnyElement> {
    if props.chips.is_empty() {
        return None;
    }
    let theme = props.theme;
    let colors = theme.colors;
    let can_react = props.can_react;
    let prefix = props.label_prefix;
    let mut row = div()
        .debug_selector(move || format!("{prefix}reaction-chips"))
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1()
        .pt_1();
    for (index, chip) in props.chips.iter().enumerate() {
        let emoji = chip.emoji;
        let on_toggle = props.on_toggle_reaction.clone();
        let tooltip = chip.tooltip();
        let mine = chip.mine;
        row = row.child(
            div()
                .id(ElementId::Name(format!("chip-{id}-{index}").into()))
                .debug_selector(move || format!("{prefix}reaction-chip-{index}"))
                .role(Role::Button)
                .aria_label(chip.label())
                .flex()
                .items_center()
                .gap_1()
                .h(px(24.))
                .px_2()
                .rounded(px(12.))
                .border_1()
                .border_color(if mine {
                    colors.accent
                } else {
                    colors.border_strong
                })
                .bg(if mine {
                    colors.accent_surface
                } else {
                    colors.surface_raised
                })
                .tooltip(move |_, cx| TextTooltip::view(tooltip.clone(), cx))
                .when(can_react, |chip| {
                    chip.cursor_pointer()
                        .hover(|style| style.border_color(colors.accent))
                        .on_click(move |_, window, cx| on_toggle(emoji, window, cx))
                })
                .child(chip_face(chip, theme))
                .child(
                    div()
                        .text_size(theme.type_scale.small)
                        .font_weight(if mine {
                            FontWeight::SEMIBOLD
                        } else {
                            FontWeight::NORMAL
                        })
                        .text_color(if mine { colors.text } else { colors.text_muted })
                        .child(chip.count.to_string()),
                ),
        );
    }
    if can_react {
        let on_add = props.on_add_reaction.clone();
        row = row.child(
            div()
                .id(ElementId::Name(format!("add-reaction-{id}").into()))
                .debug_selector(|| "add-reaction-chip".into())
                .role(Role::Button)
                .aria_label("Add reaction")
                .flex()
                .items_center()
                .gap_0p5()
                .h(px(24.))
                .px_2()
                .rounded(px(12.))
                .border_1()
                .border_color(colors.border_strong)
                .bg(colors.surface_raised)
                .text_size(theme.type_scale.small)
                .text_color(colors.text_muted)
                .cursor_pointer()
                .hover(|style| style.border_color(colors.accent).text_color(colors.text))
                .on_click(move |_, window, cx| {
                    let at = window.mouse_position();
                    on_add(at, window, cx);
                })
                .child(div().text_size(px(14.)).child(emoji_font::glyphs("🙂")))
                .child("+"),
        );
    }
    Some(row.into_any_element())
}

/// The small "APP" tag beside the name of a bot.
pub fn app_badge(theme: &Theme) -> AnyElement {
    div()
        .id("app-badge")
        .debug_selector(|| "app-badge".into())
        .role(Role::Label)
        .aria_label("App")
        .flex_none()
        .px_1p5()
        .rounded(theme.radii.small)
        .bg(theme.colors.accent_surface)
        .text_size(px(10.))
        .font_weight(FontWeight::BOLD)
        .text_color(theme.colors.accent)
        .child("APP")
        .into_any_element()
}

/// The summary line under a root message: avatars of recent repliers, the reply count, and when
/// the newest reply came. Clicking it (or pressing Enter with the cursor on it) opens the thread.
fn thread_line(line: &ThreadLineProps, theme: &Theme, id: &ElementId) -> AnyElement {
    let colors = theme.colors;
    let on_open = line.on_open.clone();
    let on_key = line.on_open.clone();
    let mut avatars = div().flex().items_center().gap_0p5().flex_none();
    for replier in &line.repliers {
        avatars = avatars.child(
            div()
                .size(px(20.))
                .flex_none()
                .rounded(theme.radii.small)
                .bg(avatar_color(replier.seed))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(gpui::white())
                .child(replier.initial.clone()),
        );
    }
    let focused = line.focused;
    div()
        .id(ElementId::Name(format!("thread-line-{id}").into()))
        .debug_selector(|| "thread-line".into())
        .role(Role::Button)
        .aria_label(line.aria_label())
        .mt_1()
        // Drawn under the message text: the box reaches a little left of it so the avatars line
        // up with the text, and no further right than its content.
        .ml(px(-6.))
        .self_start()
        .px_1p5()
        .py_1()
        .flex()
        .items_center()
        .gap_2()
        .rounded(theme.radii.medium)
        .border_1()
        .border_color(if focused {
            colors.focus_ring
        } else {
            gpui::transparent_black()
        })
        .when(focused, |line| line.border_2())
        .cursor_pointer()
        .hover(|style| {
            style
                .bg(colors.surface_raised)
                .border_color(colors.border_strong)
        })
        .on_click(move |_, window, cx| on_open(window, cx))
        .on_key_down(move |event, window, cx| {
            if crate::keyboard::is_activation(event) {
                on_key(window, cx);
                cx.stop_propagation();
            }
        })
        .child(avatars)
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap_1p5()
                .when(line.unread, |count| {
                    count.child(
                        div()
                            .debug_selector(|| "thread-unread-dot".into())
                            .size(px(8.))
                            .flex_none()
                            .rounded_full()
                            .bg(colors.accent),
                    )
                })
                .child(
                    div()
                        .debug_selector(|| "thread-reply-count".into())
                        .text_size(theme.type_scale.small)
                        .font_weight(if line.unread {
                            FontWeight::BOLD
                        } else {
                            FontWeight::SEMIBOLD
                        })
                        .text_color(colors.accent)
                        .child(line.count_label()),
                ),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(theme.type_scale.small)
                .text_color(colors.text_muted)
                .child(format!("Last reply {}", line.last_reply)),
        )
        .into_any_element()
}

/// The header over a reply that was also sent to the channel.
fn reply_header(header: &ReplyHeaderProps, theme: &Theme, id: &ElementId) -> AnyElement {
    let colors = theme.colors;
    let on_open = header.on_open.clone();
    div()
        .id(ElementId::Name(format!("reply-header-{id}").into()))
        .debug_selector(|| "reply-header".into())
        .role(Role::Button)
        .aria_label("Replied to a thread. Open thread")
        .flex()
        .items_baseline()
        .gap_1p5()
        .min_w_0()
        .text_size(theme.type_scale.small)
        .text_color(colors.text_muted)
        .cursor_pointer()
        .hover(|style| style.text_color(colors.text))
        .on_click(move |_, window, cx| on_open(window, cx))
        .child(div().flex_none().child("↪ replied to a thread"))
        .children(header.excerpt.as_ref().map(|(author, text)| {
            div()
                .min_w_0()
                .truncate()
                .child(format!("{author}: {text}"))
        }))
        .into_any_element()
}

/// The rule under the root message in the thread pane, naming how many replies follow.
fn pane_divider(label: &str, theme: &Theme) -> AnyElement {
    div()
        .debug_selector(|| "thread-reply-divider".into())
        .mt_2()
        .flex()
        .items_center()
        .gap_2()
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

/// What a reaction chip shows before its count: the glyph, or a custom emoji's picture (or, until
/// it has one, or when it was deleted, its muted `:name:`).
fn chip_face(chip: &crate::reactions::Chip, theme: &Theme) -> AnyElement {
    match (&chip.custom, chip.glyph()) {
        (Some(custom), _) => match &custom.picture {
            Some(picture) => {
                let (w_em, h_em) = picture.size_em();
                div()
                    .debug_selector(|| "reaction-emoji-picture".into())
                    .flex_none()
                    .w(px(w_em * 14.))
                    .h(px(h_em * 14.))
                    .child(
                        gpui::img(picture.image.clone())
                            .size_full()
                            .object_fit(gpui::ObjectFit::Contain),
                    )
                    .into_any_element()
            }
            None => div()
                .debug_selector(|| "reaction-emoji-fallback".into())
                .text_size(theme.type_scale.small)
                .text_color(theme.colors.text_muted)
                .child(custom.label.clone())
                .into_any_element(),
        },
        (None, Some(glyph)) => div()
            .text_size(px(14.))
            .child(emoji_font::glyphs(glyph))
            .into_any_element(),
        (None, None) => div().into_any_element(),
    }
}

/// The hover toolbar: one small button per available action.
fn toolbar(props: &RowProps<'_>, id: &ElementId, group: SharedString) -> AnyElement {
    let theme = props.theme;
    let mut buttons = div().flex().gap_1();
    for action in props.actions.available.iter().copied() {
        let selector = match action {
            RowAction::React => "react-message",
            RowAction::ToggleReaction(_) => continue,
            RowAction::Reply => "reply-message",
            RowAction::Edit => "edit-message",
            RowAction::Delete => "delete-message",
            RowAction::CopyLink => "copy-link",
            RowAction::CopyText => "copy-message",
            // Menu only.
            RowAction::DownloadFile
            | RowAction::CopyFileName
            | RowAction::Save
            | RowAction::Unsave
            | RowAction::ViewProfile => continue,
        };
        let on_action = props.actions.on_action.clone();
        // Also reachable by row: `message-2-react-message` is row 2's React button, for tests
        // that must tell rows apart (every row has a toolbar, shown only under the pointer).
        let by_row = format!("{}message-{}-{selector}", props.label_prefix, props.index);
        buttons = buttons.child(div().debug_selector(move || by_row).child(small_button(
            theme,
            ElementId::Name(format!("{}-{id}", action.id()).into()),
            format!("{}{selector}", props.label_prefix),
            match action {
                RowAction::CopyText => "Copy",
                RowAction::React => "React",
                RowAction::Reply => "Reply",
                other => other.label(),
            },
            move |window, cx| on_action(action, window, cx),
        )));
    }
    div()
        .absolute()
        .top(px(-10.))
        .right(px(20.))
        .when(!props.focused, |toolbar| toolbar.invisible())
        .group_hover(group, |style| style.visible())
        .child(buttons)
        .into_any_element()
}

/// Draws one timeline row.
pub fn render_message_row(mut props: RowProps<'_>) -> AnyElement {
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
    let edited = props.item.is_edited();

    let header = props.layout.show_header.then(|| {
        div()
            .flex()
            .items_baseline()
            .gap_2()
            .child({
                let on_profile = props.on_profile.clone();
                let colors = theme.colors;
                let prefix = props.label_prefix;
                let index = props.index;
                div()
                    .id(ElementId::NamedInteger("author-name".into(), index as u64))
                    .debug_selector(move || format!("{prefix}author-name-{index}"))
                    .role(gpui::Role::Button)
                    .aria_label(format!("View profile of {}", props.author_name))
                    .cursor_pointer()
                    .hover(move |style| style.underline())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text)
                    .on_click(move |_, window, cx| on_profile(window, cx))
                    .child(props.author_name.clone())
            })
            .children(props.author_is_bot.then(|| app_badge(theme)))
            .children(props.author_deactivated.then(|| {
                let prefix = props.label_prefix;
                let index = props.index;
                div()
                    .id(ElementId::NamedInteger(
                        "author-deactivated".into(),
                        index as u64,
                    ))
                    .debug_selector(move || format!("{prefix}author-deactivated-{index}"))
                    .px_1p5()
                    .rounded(theme.radii.small)
                    .border_1()
                    .border_color(theme.colors.border_strong)
                    .text_color(theme.colors.text_muted)
                    .text_size(theme.type_scale.small)
                    .tooltip(|_, cx| TextTooltip::view(crate::states::DEACTIVATED_NOTE, cx))
                    .child(crate::states::DEACTIVATED)
            }))
            .children(props.author_status.as_ref().map(|mark| {
                crate::status_mark::view(
                    ElementId::NamedInteger("author-status".into(), props.index as u64),
                    format!("author-status-{}", props.index),
                    mark,
                    theme.type_scale.body,
                )
            }))
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.text_faint)
                    .child(time_text),
            )
            .children(props.saved.then(|| {
                let prefix = props.label_prefix;
                let index = props.index;
                div()
                    .id(ElementId::NamedInteger("saved-mark".into(), index as u64))
                    .debug_selector(move || format!("{prefix}saved-mark-{index}"))
                    .role(gpui::Role::Image)
                    .aria_label("Saved for later")
                    .tooltip(|_, cx| TextTooltip::view("Saved for later", cx))
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.accent)
                    .child("★")
            }))
            .children(edited.then(|| {
                div()
                    .debug_selector(|| "edited-marker".into())
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.text_faint)
                    .child("(edited)")
            }))
    });

    let leading = if props.layout.show_header {
        avatar(&props)
    } else {
        div().w(px(AVATAR)).flex_none().into_any_element()
    };

    let group: SharedString = format!("row-{id}").into();

    let mut content = div().flex().flex_col().gap_0p5().min_w_0().flex_1();
    if let Some(reply) = &props.reply_header {
        content = content.child(reply_header(reply, theme, &id));
    }
    if let Some(header) = header {
        content = content.child(header);
    }
    // A message of only files has no text to draw: no empty block, no gap.
    let mut files = std::mem::take(&mut props.attachments);
    let files_block = |files: Vec<AnyElement>| {
        div()
            .debug_selector(|| "message-files".into())
            .flex()
            .flex_wrap()
            .gap_2()
            .pt_1()
            .children(files)
    };
    match &props.editor {
        Some(editor) => {
            content = content.child(editor_body(&props, editor, &id));
            if !files.is_empty() {
                content = content.child(files_block(std::mem::take(&mut files)));
            }
        }
        None => {
            match (&props.item.rich, props.tombstone) {
                // An integration's blocks are drawn in place of their fallback text (D40).
                (Some(rich), false) => {
                    content = content.child(crate::rich_blocks::render_rich(
                        crate::rich_blocks::RichProps {
                            content: rich,
                            theme,
                            id: id.clone(),
                            label_prefix: props.label_prefix,
                            index: props.index,
                            on_link: props.on_link.clone(),
                            mentions: props.mentions,
                            on_press: props.on_press.clone(),
                            pressing: props.pressing,
                        },
                    ));
                }
                _ if props.tombstone || !props.item.document.has_no_content() => {
                    content = content.child(body(&props, &id));
                }
                _ => {}
            }
            if !props.tombstone && !props.link_previews.is_empty() {
                content = content.child(preview_cards(&props));
            }
            if !files.is_empty() {
                content = content.child(files_block(std::mem::take(&mut files)));
            }
            if edited && !props.layout.show_header {
                content = content.child(
                    div()
                        .debug_selector(|| "edited-marker".into())
                        .text_size(theme.type_scale.small)
                        .text_color(theme.colors.text_faint)
                        .child("(edited)"),
                );
            }
            if let Some(also) = props.pane.as_ref().and_then(|pane| pane.also_sent.as_ref()) {
                content = content.child(
                    div()
                        .debug_selector(|| "also-sent-to-channel".into())
                        .text_size(theme.type_scale.small)
                        .text_color(theme.colors.text_faint)
                        .child(also.clone()),
                );
            }
            if let Some(chips) = reaction_chips(&props, &id) {
                content = content.child(chips);
            }
            if let Some(line) = &props.thread_line {
                content = content.child(thread_line(line, theme, &id));
            }
            if let Some(label) = props
                .pane
                .as_ref()
                .and_then(|pane| pane.root_divider.as_ref())
            {
                content = content.child(pane_divider(label, theme));
            }
        }
    }
    for line in status_lines(&props, &id) {
        content = content.child(line);
    }

    let editing = props.editor.is_some();
    let row = div()
        .id(id.clone())
        .debug_selector(|| format!("{}message-row-{}", props.label_prefix, props.index))
        .group(group.clone())
        .relative()
        .flex()
        .gap_3()
        .px_5()
        .py(if props.layout.show_header {
            px(6.)
        } else {
            px(1.)
        })
        // A row that mentions the person keeps its tint under the pointer: the bar and the tint are
        // the signal, and a hover wash on top would blur it.
        .when(props.mentions_me && !editing, |row| {
            row.bg(theme.colors.mention_row).child(
                div()
                    .id(SharedString::from(format!("mention-bar-{id}")))
                    .debug_selector(|| format!("mention-bar-{}", props.index))
                    .role(Role::Note)
                    .aria_label("Mentions you")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(3.))
                    .bg(theme.colors.mention_bar),
            )
        })
        .when(!editing && !props.mentions_me, |row| {
            row.hover(|style| style.bg(theme.colors.surface_hover))
        })
        .when(props.highlighted, |row| {
            row.bg(theme.colors.highlight).child(
                div()
                    .debug_selector(|| format!("{}highlighted-row", props.label_prefix))
                    .absolute()
                    .size_0(),
            )
        })
        .when(faded, |row| row.opacity(0.7))
        .child(leading)
        .child(content)
        .when(props.focused, |row| {
            row.child(
                div()
                    .debug_selector(|| format!("{}message-cursor", props.label_prefix))
                    .absolute()
                    .top(px(1.))
                    .bottom(px(1.))
                    .left(px(6.))
                    .right(px(6.))
                    .border_2()
                    .border_color(theme.colors.focus_ring)
                    .rounded(theme.radii.small),
            )
        })
        .when(!editing && !props.actions.available.is_empty(), |row| {
            row.child(toolbar(&props, &id, group.clone()))
        });

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
            thread: None,
            thread_root: None,
            in_channel: false,
            deleted: false,
            attachments: vec![],
            upload: None,
            mentions: Default::default(),
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
            edited_at: None,
            edit: EditState::None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            rich: None,
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
            thread: None,
            thread_root: None,
            in_channel: false,
            deleted: false,
            attachments: vec![],
            upload: None,
            mentions: Default::default(),
            key: ItemKey::Operation(OperationId::generate()),
            author_id: a,
            document: MessageDocument::from_plain_text("sending").unwrap(),
            delivery: Delivery::Pending,
            edited_at: None,
            edit: EditState::None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            rich: None,
        };
        let layout = row_layout(Some(&first), &pending, &TimeZone::UTC, &now_at(BASE + 30));
        assert!(!layout.show_header);
    }

    #[test]
    fn initials_respect_grapheme_clusters() {
        use crate::avatar::initial;
        assert_eq!(initial("zoë"), "Z");
        assert_eq!(initial("👩‍👩‍👧‍👦 Family"), "👩‍👩‍👧‍👦");
        assert_eq!(initial(""), "?");
    }

    fn line(replies: u32, unread: bool) -> ThreadLineProps {
        ThreadLineProps {
            replies,
            repliers: vec![],
            last_reply: "5 minutes ago".into(),
            unread,
            focused: false,
            on_open: Rc::new(|_, _| {}),
        }
    }

    #[test]
    fn a_summary_line_counts_replies_and_says_when_the_last_came() {
        assert_eq!(line(1, false).count_label(), "1 reply");
        assert_eq!(line(2, false).count_label(), "2 replies");
        assert_eq!(line(14, false).count_label(), "14 replies");
        assert_eq!(
            line(3, false).aria_label(),
            "3 replies, last reply 5 minutes ago. Open thread"
        );
        assert_eq!(
            line(3, true).aria_label(),
            "3 replies, last reply 5 minutes ago, unread. Open thread"
        );
    }

    #[test]
    fn replying_is_an_action_with_its_own_wording_and_name() {
        assert_eq!(RowAction::Reply.label(), "Reply in thread");
        assert_eq!(RowAction::Reply.id(), "reply");
        assert_ne!(RowAction::Reply.id(), RowAction::React.id());
    }
}
