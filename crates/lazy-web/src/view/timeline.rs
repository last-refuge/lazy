//! The rows of a conversation or a thread: messages, notices, day dividers, and the line where
//! unread messages start. Each row has a key, so the page replaces only the rows that changed.

use lazy_client_core::{
    ClientCore, Delivery, EditState, HistoryStatus, ItemKey, RetryKind, TimelineItem,
};
use lazy_domain::{ConversationId, ConversationKind, ReactionEmoji, UserId};
use lazy_protocol::api::paths;

use super::document::{self, Context};
use super::html::{data, esc, push, size};
use super::icons::icon;
use super::time::{Clock, day_label, time_of_day};
use super::{Ui, avatar, conversation_name, key_text};

/// Rows by the same person within this many minutes read as one block.
const GROUP_MINUTES: i64 = 5;

/// Which list the rows are for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    Conversation,
    Thread,
}

/// A row: its key and its markup.
pub(crate) type Row = (String, String);

/// The rows of a conversation, oldest first.
pub(crate) fn conversation_rows(
    core: &ClientCore,
    ui: &Ui,
    clock: &dyn Clock,
    id: ConversationId,
) -> Vec<Row> {
    let mut rows = Vec::new();
    rows.push(("history".to_owned(), history_row(core, id)));
    let marker = core.unread_marker(id);
    rows.extend(item_rows(
        core,
        ui,
        clock,
        id,
        core.timeline(id),
        marker,
        Place::Conversation,
    ));
    rows
}

/// What is above the oldest message: more to load, loading, the start, or a failure.
fn history_row(core: &ClientCore, id: ConversationId) -> String {
    match core.history_status(id) {
        HistoryStatus::NotLoaded | HistoryStatus::Loading => {
            "<div class=\"history\">Loading messages…</div>".to_owned()
        }
        HistoryStatus::Ready { has_more: true } => format!(
            "<div class=\"history\"><button type=\"button\" class=\"link\" data-action=\"load-older\" data-conv=\"{id}\">Show older messages</button></div>"
        ),
        HistoryStatus::Ready { has_more: false } => {
            let name = conversation_name(core, id);
            let text = match core.conversation(id).map(|c| c.kind) {
                Some(ConversationKind::Channel) => {
                    format!("This is the very beginning of {}.", esc(&name))
                }
                _ => format!(
                    "This is the beginning of your conversation with {}.",
                    esc(&name)
                ),
            };
            format!(
                "<div class=\"history start\"><h2>{}</h2><p>{text}</p></div>",
                esc(&name)
            )
        }
        HistoryStatus::Failed { message } => format!(
            "<div class=\"history failed\">{} <button type=\"button\" class=\"link\" data-action=\"load-older\" data-conv=\"{id}\">Try again</button></div>",
            esc(message)
        ),
    }
}

/// Rows for `items`, with day dividers, the unread line before `marker`, and messages by the same
/// person close together drawn as one block.
pub(crate) fn item_rows(
    core: &ClientCore,
    ui: &Ui,
    clock: &dyn Clock,
    conversation: ConversationId,
    items: &[TimelineItem],
    marker: Option<ItemKey>,
    place: Place,
) -> Vec<Row> {
    let mut rows = Vec::with_capacity(items.len() + 4);
    let mut previous: Option<&TimelineItem> = None;
    let mut last_day = None;
    for item in items {
        let at = item.created_at().unwrap_or_else(|| clock.now());
        let day = clock.local(at).day_key();
        let mut grouped = previous.is_some_and(|prev| continues(prev, item, clock));
        if last_day != Some(day) {
            rows.push((
                format!("d{}-{}-{}", day.0, day.1, day.2),
                format!(
                    "<div class=\"day\"><span>{}</span></div>",
                    esc(&day_label(clock, at))
                ),
            ));
            last_day = Some(day);
            grouped = false;
        }
        if marker == Some(item.key) {
            rows.push((
                "unread".to_owned(),
                "<div class=\"unread-line\"><span>New</span></div>".to_owned(),
            ));
            grouped = false;
        }
        rows.push((
            key_text(item.key),
            row(core, ui, clock, conversation, item, grouped, place),
        ));
        previous = Some(item);
    }
    rows
}

/// Whether `item` continues `prev`'s block: the same author, no notice, within a few minutes.
fn continues(prev: &TimelineItem, item: &TimelineItem, clock: &dyn Clock) -> bool {
    if prev.author_id != item.author_id || prev.notice.is_some() || item.notice.is_some() {
        return false;
    }
    if prev.deleted || item.deleted {
        return false;
    }
    let prev_at = prev.created_at().unwrap_or_else(|| clock.now());
    let at = item.created_at().unwrap_or_else(|| clock.now());
    (at.as_offset_date_time() - prev_at.as_offset_date_time()).whole_minutes() < GROUP_MINUTES
}

fn row(
    core: &ClientCore,
    ui: &Ui,
    clock: &dyn Clock,
    conversation: ConversationId,
    item: &TimelineItem,
    grouped: bool,
    place: Place,
) -> String {
    let ctx = Context { core };
    let me = core.me().map(|user| user.id);
    let at = item.created_at().unwrap_or_else(|| clock.now());
    let author = core.display_name(item.author_id);
    let message = item.message_id();

    if let Some(notice) = &item.notice {
        return notice_row(core, clock, conversation, item, notice, author);
    }

    let mut classes = vec!["msg"];
    if grouped {
        classes.push("grouped");
    }
    if me.is_some_and(|me| item.mentions_user(me)) && Some(item.author_id) != me {
        classes.push("mentioned");
    }
    match &item.delivery {
        Delivery::Pending | Delivery::UnknownOutcome => classes.push("pending"),
        Delivery::Failed { .. } => classes.push("failed"),
        Delivery::Confirmed { .. } => {}
    }
    if message.is_some() && ui.editing.map(|(_, m)| Some(m)) == Some(message) {
        classes.push("editing");
    }

    let mut out = String::new();
    push!(out, "<div class=\"{}\">", classes.join(" "));
    if grouped {
        push!(
            out,
            "<span class=\"gutter-time\">{}</span>",
            esc(&time_of_day(clock, at))
        );
    } else {
        push!(
            out,
            "<button type=\"button\" class=\"avatar-button\" tabindex=\"-1\" aria-hidden=\"true\">{}</button>",
            avatar(core, item.author_id, "")
        );
    }
    out.push_str("<div class=\"msg-body\">");
    if !grouped {
        push!(
            out,
            "<div class=\"msg-meta\"><span class=\"msg-author\">{}</span>",
            esc(author)
        );
        if core.member(item.author_id).is_some_and(|user| user.is_bot) {
            out.push_str("<span class=\"badge-bot\">App</span>");
        }
        push!(
            out,
            "<time title=\"{}\">{}</time>",
            esc(&day_label(clock, at)),
            esc(&time_of_day(clock, at))
        );
        out.push_str("</div>");
    }

    if item.deleted {
        out.push_str("<div class=\"msg-text deleted\">This message was deleted.</div>");
    } else if !item.document.has_no_content() {
        let mut content = document::document(&ctx, &item.document);
        if item.is_edited() {
            // At the end of the last paragraph, where it reads as part of the message.
            let mark = "<span class=\"edited\">(edited)</span>";
            match content.strip_suffix("</p>") {
                Some(start) => content = format!("{start} {mark}</p>"),
                None => content.push_str(mark),
            }
        }
        push!(out, "<div class=\"msg-text\">{content}</div>");
    }
    if item.in_channel && place == Place::Conversation && item.thread_root.is_some() {
        out.push_str("<div class=\"replied\">Replied to a thread</div>");
    }
    out.push_str(&files(item));
    if let Some(upload) = &item.upload {
        push!(
            out,
            "<div class=\"delivery\">Uploading {} of {} files…</div>",
            upload.uploaded_files() + 1,
            upload.total_files()
        );
    }
    out.push_str(&delivery(conversation, item));
    out.push_str(&edit_state(conversation, item));
    if let Some(message) = message {
        out.push_str(&reactions(core, conversation, message, item, me));
        if place == Place::Conversation
            && let Some(thread) = item.thread.as_ref().filter(|t| t.reply_count > 0)
        {
            let mut faces = String::new();
            for user in thread.recent_repliers.iter().take(3) {
                faces.push_str(&avatar(core, *user, "tiny"));
            }
            push!(
                out,
                "<button type=\"button\" class=\"thread-link\"{}>{faces}<span>{} {}</span><span class=\"muted\">Last reply {}</span></button>",
                data(&[
                    ("action", "open-thread"),
                    ("conv", &conversation.to_string()),
                    ("msg", &message.to_string())
                ]),
                thread.reply_count,
                if thread.reply_count == 1 {
                    "reply"
                } else {
                    "replies"
                },
                esc(&day_or_time(clock, thread.last_reply_at))
            );
        }
    }
    out.push_str("</div>");
    if let Some(message) = message.filter(|_| !item.deleted) {
        out.push_str(&actions(core, conversation, message, place));
    }
    out.push_str("</div>");
    out
}

/// "10:42 AM" today, the day otherwise.
fn day_or_time(clock: &dyn Clock, at: lazy_domain::Timestamp) -> String {
    let label = day_label(clock, at);
    if label == "Today" {
        time_of_day(clock, at)
    } else {
        label
    }
}

fn notice_row(
    core: &ClientCore,
    clock: &dyn Clock,
    conversation: ConversationId,
    item: &TimelineItem,
    notice: &lazy_domain::Notice,
    author: &str,
) -> String {
    let at = item.created_at().unwrap_or_else(|| clock.now());
    let place = match core.conversation(conversation).map(|c| c.kind) {
        Some(ConversationKind::Channel) => conversation_name(core, conversation),
        _ => "the conversation".to_owned(),
    };
    let name = |user: UserId| core.display_name(user).to_owned();
    let words = match notice {
        lazy_domain::Notice::Unsupported => {
            lazy_domain::MessageDocument::plain_text(&item.document)
        }
        notice => notice.describe(&place, &name),
    };
    let mut out = String::new();
    let live = notice.as_drop_in().is_some_and(|d| d.is_live());
    push!(
        out,
        "<div class=\"msg notice{}\"><span class=\"notice-mark\">{}</span><div class=\"msg-body\"><span class=\"msg-author\">{}</span> {}",
        if live { " live" } else { "" },
        if notice.as_drop_in().is_some() {
            "<span class=\"dot online\"></span>"
        } else {
            icon("arrow")
        },
        esc(author),
        esc(&words)
    );
    if live {
        out.push_str(if super::html::in_shell() {
            " <span class=\"muted\">Join from Lazy on a computer.</span>"
        } else {
            " <span class=\"muted\">Join from the Lazy app.</span>"
        });
    }
    push!(
        out,
        " <time class=\"muted\">{}</time></div></div>",
        esc(&time_of_day(clock, at))
    );
    out
}

fn files(item: &TimelineItem) -> String {
    if item.attachments.is_empty() {
        return String::new();
    }
    let confirmed = item.is_confirmed();
    let mut out = String::from("<div class=\"msg-files\">");
    for file in &item.attachments {
        let url = paths::attachment(file.id);
        if confirmed && file.has_thumbnail && file.media_type.starts_with("image/") {
            let (width, height) = fit(file.width, file.height);
            push!(
                out,
                "<a class=\"image\" href=\"{}\" target=\"_blank\" rel=\"noopener\" title=\"{}\"><img src=\"{}\" alt=\"{}\" width=\"{width}\" height=\"{height}\" loading=\"lazy\"></a>",
                esc(&url),
                esc(&file.name),
                esc(&paths::attachment_thumbnail(file.id)),
                esc(&file.name)
            );
        } else if confirmed {
            push!(
                out,
                "<a class=\"file\" href=\"{}\" download=\"{}\"><span class=\"file-icon\">{}</span><span class=\"file-name\">{}</span><span class=\"muted\">{}</span></a>",
                esc(&url),
                esc(&file.name),
                icon("file"),
                esc(&file.name),
                esc(&size(file.size))
            );
        } else {
            push!(
                out,
                "<span class=\"file\"><span class=\"file-icon\">{}</span><span class=\"file-name\">{}</span><span class=\"muted\">{}</span></span>",
                icon("file"),
                esc(&file.name),
                esc(&size(file.size))
            );
        }
    }
    out.push_str("</div>");
    out
}

/// An image's size on screen: at most 360 by 240, keeping its shape.
fn fit(width: Option<u32>, height: Option<u32>) -> (u32, u32) {
    const MAX_W: f64 = 360.0;
    const MAX_H: f64 = 240.0;
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            let scale = (MAX_W / f64::from(w)).min(MAX_H / f64::from(h)).min(1.0);
            (
                ((f64::from(w) * scale).round() as u32).max(1),
                ((f64::from(h) * scale).round() as u32).max(1),
            )
        }
        _ => (240, 160),
    }
}

fn delivery(conversation: ConversationId, item: &TimelineItem) -> String {
    let Some(operation) = item.operation_id() else {
        return String::new();
    };
    let ids = |action: &str| {
        data(&[
            ("action", action),
            ("conv", &conversation.to_string()),
            ("op", &operation.to_string()),
        ])
    };
    match &item.delivery {
        Delivery::Confirmed { .. } => String::new(),
        Delivery::Pending => "<div class=\"delivery\">Sending…</div>".to_owned(),
        Delivery::UnknownOutcome => {
            "<div class=\"delivery\">Checking whether it was sent…</div>".to_owned()
        }
        Delivery::Failed { reason, retry } => format!(
            "<div class=\"delivery failed\">Not sent: {} <button type=\"button\" class=\"link\"{}>{}</button> <button type=\"button\" class=\"link\"{}>Delete</button></div>",
            esc(reason),
            ids("retry-send"),
            match retry {
                RetryKind::NewOperation => "Try again",
                RetryKind::SameOperation => "Check and resend",
            },
            ids("discard-send")
        ),
    }
}

fn edit_state(conversation: ConversationId, item: &TimelineItem) -> String {
    let Some(message) = item.message_id() else {
        return String::new();
    };
    let ids = |action: &str| {
        data(&[
            ("action", action),
            ("conv", &conversation.to_string()),
            ("msg", &message.to_string()),
        ])
    };
    match &item.edit {
        EditState::None => String::new(),
        EditState::Saving => "<div class=\"delivery\">Saving the edit…</div>".to_owned(),
        EditState::Failed { reason, .. } => format!(
            "<div class=\"delivery failed\">The edit was not saved: {} <button type=\"button\" class=\"link\"{}>Try again</button> <button type=\"button\" class=\"link\"{}>Discard</button></div>",
            esc(reason),
            ids("retry-edit"),
            ids("cancel-edit")
        ),
        EditState::Conflict { .. } => format!(
            "<div class=\"delivery failed\">Someone changed this message while you were editing it. <button type=\"button\" class=\"link\"{}>Save mine anyway</button> <button type=\"button\" class=\"link\"{}>Keep theirs</button></div>",
            ids("retry-edit"),
            ids("cancel-edit")
        ),
    }
}

/// How a reaction's emoji is drawn.
pub(crate) fn emoji_html(core: &ClientCore, emoji: ReactionEmoji) -> String {
    match emoji {
        ReactionEmoji::Unicode(emoji) => esc(emoji.as_str()),
        ReactionEmoji::Custom(id) => match core.custom_emoji_by_id(id).filter(|e| !e.deleted) {
            Some(custom) => format!(
                "<img class=\"emoji\" src=\"{}\" alt=\":{}:\">",
                esc(&paths::emoji(id)),
                esc(&custom.name)
            ),
            None => format!(":{}:", esc(core.custom_emoji_label(id).unwrap_or("emoji"))),
        },
    }
}

/// The wire text of a reaction, as the page passes it back.
pub(crate) fn emoji_wire(emoji: ReactionEmoji) -> String {
    serde_json::to_value(emoji)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn reactions(
    core: &ClientCore,
    conversation: ConversationId,
    message: lazy_domain::MessageId,
    item: &TimelineItem,
    me: Option<UserId>,
) -> String {
    if item.reactions.is_empty() {
        return String::new();
    }
    let can_react = core.can_react_message(conversation, message);
    let mut out = String::from("<div class=\"msg-reactions\">");
    for reaction in &item.reactions {
        if reaction.users.is_empty() {
            continue;
        }
        let mine = me.is_some_and(|me| reaction.users.contains(&me));
        let names: Vec<&str> = reaction
            .users
            .iter()
            .take(10)
            .map(|user| core.display_name(*user))
            .collect();
        let mut who = names.join(", ");
        if reaction.users.len() > names.len() {
            who.push_str(&format!(" and {} more", reaction.users.len() - names.len()));
        }
        push!(
            out,
            "<button type=\"button\" class=\"reaction{}\" title=\"{}\" aria-pressed=\"{mine}\"{}{}>{} <span>{}</span></button>",
            if mine { " mine" } else { "" },
            esc(&who),
            if can_react { "" } else { " disabled" },
            data(&[
                ("action", "toggle-reaction"),
                ("conv", &conversation.to_string()),
                ("msg", &message.to_string()),
                ("emoji", &emoji_wire(reaction.emoji))
            ]),
            emoji_html(core, reaction.emoji),
            reaction.users.len()
        );
    }
    if can_react {
        push!(
            out,
            "<button type=\"button\" class=\"reaction add\" aria-label=\"Add a reaction\"{}>{}</button>",
            data(&[
                ("action", "react"),
                ("conv", &conversation.to_string()),
                ("msg", &message.to_string())
            ]),
            icon("smile")
        );
    }
    out.push_str("</div>");
    out
}

/// The buttons that appear over a message.
fn actions(
    core: &ClientCore,
    conversation: ConversationId,
    message: lazy_domain::MessageId,
    place: Place,
) -> String {
    let ids = |action: &str| {
        data(&[
            ("action", action),
            ("conv", &conversation.to_string()),
            ("msg", &message.to_string()),
        ])
    };
    let mut out = String::from("<div class=\"msg-actions\">");
    if core.can_react_message(conversation, message) {
        push!(
            out,
            "<button type=\"button\" title=\"Add a reaction\" aria-label=\"Add a reaction\"{}>{}</button>",
            ids("react"),
            icon("smile")
        );
    }
    if place == Place::Conversation && core.can_reply_message(conversation, message) {
        push!(
            out,
            "<button type=\"button\" title=\"Reply in thread\" aria-label=\"Reply in thread\"{}>{}</button>",
            ids("open-thread"),
            icon("reply")
        );
    }
    if core.can_edit_message(conversation, message) {
        push!(
            out,
            "<button type=\"button\" title=\"Edit\" aria-label=\"Edit\"{}>{}</button>",
            ids("edit"),
            icon("edit")
        );
    }
    if core.can_delete_message(conversation, message) {
        push!(
            out,
            "<button type=\"button\" title=\"Delete\" aria-label=\"Delete\"{}>{}</button>",
            ids("delete"),
            icon("delete")
        );
    }
    out.push_str("</div>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::testing::{message, world};
    use crate::view::time::{FixedClock, at};
    use lazy_client_core::Input;
    use lazy_protocol::{Event, EventEnvelope, ServerFrame};

    fn deliver(
        core: &mut ClientCore,
        workspace: lazy_domain::WorkspaceId,
        seq: i64,
        msg: lazy_protocol::Message,
    ) {
        core.handle(Input::Frame(ServerFrame::Event(EventEnvelope {
            workspace_id: workspace,
            seq: lazy_domain::EventSeq::new(seq).unwrap(),
            occurred_at: msg.created_at,
            event: Event::MessageCreated {
                message: msg,
                operation_id: None,
            },
        })));
    }

    #[test]
    fn messages_group_by_author_and_divide_by_day() {
        let mut w = world();
        let clock = FixedClock {
            now: at("2026-11-02T15:00:00Z"),
            hour12: true,
        };
        let out = w.core.handle(Input::SelectConversation(w.general));
        let generation = out
            .effects
            .iter()
            .find_map(|effect| match effect {
                lazy_client_core::Effect::FetchHistory { generation, .. } => Some(*generation),
                _ => None,
            })
            .or_else(|| {
                w.history
                    .iter()
                    .find(|(conversation, _)| *conversation == w.general)
                    .map(|(_, generation)| *generation)
            })
            .expect("the core asks for the history");
        let page = lazy_protocol::HistoryPage {
            messages: vec![
                message(
                    w.general,
                    w.bo.id,
                    "yesterday's",
                    3,
                    at("2026-11-01T09:00:00Z"),
                ),
                message(
                    w.general,
                    w.bo.id,
                    "hello **there**",
                    4,
                    at("2026-11-02T10:00:00Z"),
                ),
                message(
                    w.general,
                    w.bo.id,
                    "<b>again</b>",
                    5,
                    at("2026-11-02T10:02:00Z"),
                ),
                message(w.general, w.me.id, "hi Bo", 6, at("2026-11-02T10:03:00Z")),
            ],
            has_more: false,
        };
        w.core.handle(Input::HistoryLoaded {
            conversation: w.general,
            generation,
            result: Ok(page),
        });
        let rows = conversation_rows(&w.core, &Ui::default(), &clock, w.general);
        let html: Vec<&str> = rows.iter().map(|(_, html)| html.as_str()).collect();
        let all = html.join("\n");
        assert!(
            all.contains("This is the very beginning of #general."),
            "{all}"
        );
        assert!(all.contains("<span>Yesterday</span>"), "{all}");
        assert!(all.contains("<span>Today</span>"), "{all}");
        assert!(all.contains("hello <strong>there</strong>"), "{all}");
        assert!(all.contains("&lt;b&gt;again&lt;/b&gt;"), "{all}");
        assert!(!all.contains("<b>again"));
        let grouped = html
            .iter()
            .filter(|row| row.contains("msg grouped"))
            .count();
        assert_eq!(
            grouped, 1,
            "only Bo's second message today continues his block: {all}"
        );
        // My own message can be edited and deleted; Bo's cannot.
        let mine = rows.iter().find(|(_, h)| h.contains("hi Bo")).unwrap();
        assert!(mine.1.contains("data-action=\"edit\""));
        assert!(mine.1.contains("data-action=\"delete\""));
        let his = rows.iter().find(|(_, h)| h.contains("hello")).unwrap();
        assert!(!his.1.contains("data-action=\"edit\""));
        assert!(his.1.contains("data-action=\"open-thread\""));
        // Keys are unique.
        let mut keys: Vec<&String> = rows.iter().map(|(k, _)| k).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), rows.len());

        // A live message arrives at the end.
        deliver(
            &mut w.core,
            w.workspace,
            11,
            message(
                w.general,
                w.bo.id,
                "new one",
                11,
                at("2026-11-02T14:00:00Z"),
            ),
        );
        let rows = conversation_rows(&w.core, &Ui::default(), &clock, w.general);
        assert!(rows.last().unwrap().1.contains("new one"));
    }

    #[test]
    fn images_fit_the_box() {
        assert_eq!(fit(Some(720), Some(480)), (360, 240));
        assert_eq!(fit(Some(100), Some(50)), (100, 50));
        assert_eq!(fit(Some(240), Some(2400)), (24, 240));
        assert_eq!(fit(None, None), (240, 160));
    }
}
