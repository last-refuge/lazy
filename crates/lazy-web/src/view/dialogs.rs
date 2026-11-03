//! Dialogs and the emoji picker. Each has a shell, drawn once when it opens (its fields keep what
//! is typed), and a body drawn from the core's state as results arrive.

use lazy_client_core::{
    ActionKind, ActionState, ClientCore, LoadStatus, QuickSwitcherTarget, SearchStatus,
};
use lazy_domain::emoji::{self, SkinTone};
use lazy_domain::{ChannelVisibility, ReactionEmoji, UserId};
use lazy_protocol::{SearchHitKind, Snippet};

use super::html::{count, data, esc, push};
use super::icons::icon;
use super::time::{Clock, day_label, time_of_day};
use super::timeline::{emoji_html, emoji_wire};
use super::{Dialog, avatar};

/// The dialog's frame and fields.
pub(crate) fn shell(dialog: &Dialog) -> String {
    let (title, fields) = match dialog {
        Dialog::Browse => (
            "Channels",
            "<div class=\"dlg-row\"><input id=\"browse-q\" type=\"search\" placeholder=\"Search channels\" autocomplete=\"off\" aria-label=\"Search channels\"><button type=\"button\" class=\"primary\" data-action=\"create-channel\">Create channel</button></div>",
        ),
        Dialog::CreateChannel => (
            "Create a channel",
            "<form id=\"create-channel-form\" class=\"stack\"><label>Name<input name=\"name\" required maxlength=\"80\" placeholder=\"e.g. plans\" autocomplete=\"off\" autocapitalize=\"none\" spellcheck=\"false\"></label><label>Topic <span class=\"muted\">(optional)</span><input name=\"topic\" maxlength=\"250\" autocomplete=\"off\"></label><label class=\"check\"><input type=\"checkbox\" name=\"private\"> Private: only people who are added can find it</label><div id=\"dlg-status\" class=\"form-status\" role=\"status\"></div><div class=\"dlg-buttons\"><button type=\"button\" data-action=\"close-dialog\">Cancel</button><button type=\"submit\" class=\"primary\">Create</button></div></form>",
        ),
        Dialog::NewMessage { .. } => (
            "New message",
            "<input id=\"people-q\" type=\"search\" placeholder=\"Find people by name\" autocomplete=\"off\" aria-label=\"Find people\">",
        ),
        Dialog::Search => (
            "Search",
            "<form id=\"search-form\" class=\"dlg-row\"><input id=\"search-q\" type=\"search\" placeholder=\"Search messages (from:@name in:#channel)\" autocomplete=\"off\" aria-label=\"Search messages\"><button type=\"submit\" class=\"primary\">Search</button></form>",
        ),
        Dialog::Switcher { .. } => (
            "Jump to",
            "<input id=\"switcher-q\" type=\"search\" placeholder=\"Jump to a conversation or person\" autocomplete=\"off\" aria-label=\"Jump to\">",
        ),
    };
    format!(
        "<div class=\"scrim\" data-action=\"close-dialog\"></div><div class=\"dialog {}\" role=\"dialog\" aria-modal=\"true\" aria-label=\"{}\"><div class=\"dlg-head\"><h2>{}</h2><button type=\"button\" class=\"icon\" data-action=\"close-dialog\" aria-label=\"Close\">{}</button></div>{fields}<div id=\"dlg-body\" class=\"dlg-body\"></div></div>",
        dialog.kind(),
        esc(title),
        esc(title),
        icon("close")
    )
}

/// The dialog's changing part.
pub(crate) fn body(core: &ClientCore, clock: &dyn Clock, dialog: &Dialog) -> String {
    match dialog {
        Dialog::Browse => browse(core),
        Dialog::CreateChannel => String::new(),
        Dialog::NewMessage { picked } => people(core, picked),
        Dialog::Search => search(core, clock),
        Dialog::Switcher { query, highlighted } => switcher(core, query, *highlighted),
    }
}

/// The line under a form: what went wrong with the action, or that it is on its way.
pub(crate) fn action_status(core: &ClientCore, kind: ActionKind) -> String {
    match core.action(kind) {
        ActionState::InFlight => "Working…".to_owned(),
        ActionState::Failed(reason) => format!("<span class=\"error\">{}</span>", esc(reason)),
        ActionState::Idle => String::new(),
    }
}

fn status_line(status: &LoadStatus, empty: bool, nothing: &str) -> String {
    match status {
        LoadStatus::Loading if empty => "<p class=\"muted\">Loading…</p>".to_owned(),
        LoadStatus::Failed { .. } => {
            "<p class=\"error\">That didn't load. <button type=\"button\" class=\"link\" data-action=\"dialog-retry\">Try again</button></p>".to_owned()
        }
        LoadStatus::Ready if empty => format!("<p class=\"muted\">{}</p>", esc(nothing)),
        _ => String::new(),
    }
}

fn browse(core: &ClientCore) -> String {
    let view = core.browse();
    let mut out = String::from("<ul class=\"list\">");
    for channel in &view.channels {
        let conversation = &channel.conversation;
        let id = conversation.id;
        let name = conversation
            .name
            .as_ref()
            .map(|name| name.as_str().to_owned())
            .unwrap_or_default();
        push!(
            out,
            "<li><button type=\"button\" class=\"list-main\" data-action=\"open-channel\" data-conv=\"{id}\"><span class=\"list-title\">{} {}</span><span class=\"muted\">{} {}{}</span></button>",
            if conversation.visibility == ChannelVisibility::Private {
                icon("lock")
            } else {
                icon("hash")
            },
            esc(&name),
            conversation.member_count,
            if conversation.member_count == 1 {
                "member"
            } else {
                "members"
            },
            conversation
                .topic
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|topic| format!(" · {}", esc(topic)))
                .unwrap_or_default()
        );
        if channel.joined {
            out.push_str("<span class=\"muted joined\">Joined</span>");
        } else if core.can_join(channel) {
            let busy = matches!(core.action(ActionKind::Join(id)), ActionState::InFlight);
            push!(
                out,
                "<button type=\"button\" class=\"small\" data-action=\"join\" data-conv=\"{id}\"{}>Join</button>",
                if busy { " disabled" } else { "" }
            );
        }
        out.push_str("</li>");
    }
    out.push_str("</ul>");
    out.push_str(&status_line(
        &view.status,
        view.channels.is_empty(),
        "No channels match.",
    ));
    if view.has_more {
        out.push_str("<button type=\"button\" class=\"link more\" data-action=\"browse-more\">Show more</button>");
    }
    out
}

fn people(core: &ClientCore, picked: &[UserId]) -> String {
    let view = core.people();
    let me = core.me().map(|user| user.id);
    let mut out = String::new();
    if !picked.is_empty() {
        out.push_str("<div class=\"picked\">");
        for user in picked {
            push!(
                out,
                "<button type=\"button\" class=\"chip\" data-action=\"unpick-person\" data-id=\"{user}\">{} {}</button>",
                esc(core.display_name(*user)),
                icon("close")
            );
        }
        let busy = matches!(
            core.action(ActionKind::OpenConversation),
            ActionState::InFlight
        );
        push!(
            out,
            "<button type=\"button\" class=\"primary\" data-action=\"start-conversation\"{}>{}</button></div>",
            if busy { " disabled" } else { "" },
            if picked.len() == 1 {
                "Open conversation"
            } else {
                "Start group conversation"
            }
        );
        out.push_str(&format!(
            "<div class=\"form-status\">{}</div>",
            action_status(core, ActionKind::OpenConversation)
        ));
    }
    out.push_str("<ul class=\"list\">");
    for person in &view.people {
        let user = &person.user;
        if Some(user.id) == me || user.is_bot || user.deactivated {
            continue;
        }
        let chosen = picked.contains(&user.id);
        push!(
            out,
            "<li><button type=\"button\" class=\"list-main{}\" data-action=\"pick-person\" data-id=\"{}\" aria-pressed=\"{chosen}\">{}<span class=\"list-title\">{}</span><span class=\"muted\">@{}</span></button></li>",
            if chosen { " chosen" } else { "" },
            user.id,
            avatar(core, user.id, "small"),
            esc(user.display_name.as_str()),
            esc(user.username.as_str())
        );
    }
    out.push_str("</ul>");
    out.push_str(&status_line(
        &view.status,
        view.people.is_empty(),
        "Nobody matches.",
    ));
    if view.has_more {
        out.push_str("<button type=\"button\" class=\"link more\" data-action=\"people-more\">Show more</button>");
    }
    out
}

/// A snippet with its matched words marked.
fn snippet_html(snippet: &Snippet) -> String {
    let text = &snippet.text;
    let mut out = String::new();
    let mut at = 0usize;
    let mut ranges: Vec<(usize, usize)> = snippet
        .highlights
        .iter()
        .map(|range| (range.start as usize, range.end as usize))
        .filter(|(start, end)| start < end && *end <= text.len())
        .filter(|(start, end)| text.is_char_boundary(*start) && text.is_char_boundary(*end))
        .collect();
    ranges.sort();
    for (start, end) in ranges {
        if start < at {
            continue;
        }
        out.push_str(&esc(&text[at..start]));
        push!(out, "<mark>{}</mark>", esc(&text[start..end]));
        at = end;
    }
    out.push_str(&esc(&text[at..]));
    out
}

fn search(core: &ClientCore, clock: &dyn Clock) -> String {
    let Some(view) = core.search() else {
        return String::new();
    };
    let mut out = String::new();
    if let Err(error) = &view.parsed
        && !view.query.trim().is_empty()
    {
        push!(out, "<p class=\"error\">{}</p>", esc(&error.to_string()));
    }
    match &view.status {
        SearchStatus::Idle => {
            if view.hits.is_empty() {
                let recent = core.recent_searches();
                if !recent.is_empty() {
                    out.push_str("<p class=\"muted\">Recent searches</p><ul class=\"list\">");
                    for query in recent.iter().take(8) {
                        push!(
                            out,
                            "<li><button type=\"button\" class=\"list-main\" data-action=\"recent-search\" data-q=\"{0}\">{0}</button></li>",
                            esc(query)
                        );
                    }
                    out.push_str("</ul>");
                }
            }
        }
        SearchStatus::Loading if view.hits.is_empty() => {
            out.push_str("<p class=\"muted\">Searching…</p>")
        }
        SearchStatus::Failed { reason, .. } => push!(out, "<p class=\"error\">{}</p>", esc(reason)),
        SearchStatus::Ready if view.hits.is_empty() => {
            out.push_str("<p class=\"muted\">No messages match.</p>")
        }
        _ => {}
    }
    out.push_str("<ul class=\"list hits\">");
    for (index, hit) in view.hits.iter().enumerate() {
        let at = hit.message.created_at;
        push!(
            out,
            "<li><button type=\"button\" class=\"list-main hit\" data-action=\"open-hit\" data-index=\"{index}\"><span class=\"hit-head\"><span class=\"list-title\">{}</span><span class=\"muted\">{} · {} {}</span></span><span class=\"hit-text\">{}{}</span></button></li>",
            esc(&core.search_hit_title(hit)),
            esc(core.display_name(hit.message.author_id)),
            esc(&day_label(clock, at)),
            esc(&time_of_day(clock, at)),
            if matches!(hit.kind, SearchHitKind::Attachment { .. }) {
                icon("file")
            } else {
                ""
            },
            snippet_html(&hit.snippet)
        );
    }
    out.push_str("</ul>");
    if view.has_more {
        out.push_str("<button type=\"button\" class=\"link more\" data-action=\"search-more\">Show more</button>");
    }
    out
}

/// The switcher's results, as the core ranks them.
fn switcher(core: &ClientCore, query: &str, highlighted: usize) -> String {
    let results = core.quick_switcher_results(query);
    let mut out = String::from("<ul class=\"list\" role=\"listbox\">");
    for (index, item) in results.iter().enumerate() {
        let selected = index == highlighted.min(results.len().saturating_sub(1));
        let icon = match &item.target {
            QuickSwitcherTarget::Conversation(_) => "",
            QuickSwitcherTarget::Person(_) => "@ ",
            _ => "",
        };
        push!(
            out,
            "<li><button type=\"button\" role=\"option\" aria-selected=\"{selected}\" class=\"list-main{}\" data-action=\"switch-to\" data-index=\"{index}\"><span class=\"list-title\">{icon}{}</span>{}{}</button></li>",
            if selected { " chosen" } else { "" },
            esc(&item.title),
            item.subtitle
                .as_deref()
                .map(|s| format!("<span class=\"muted\">{}</span>", esc(s)))
                .unwrap_or_default(),
            if item.mentions > 0 {
                format!("<span class=\"badge\">{}</span>", count(item.mentions))
            } else {
                String::new()
            }
        );
    }
    out.push_str("</ul>");
    if results.is_empty() {
        out.push_str("<p class=\"muted\">Nothing matches.</p>");
    }
    out
}

/// The emoji picker's frame: a search field and the grid's place.
pub(crate) fn picker_shell() -> String {
    "<div class=\"picker\" role=\"dialog\" aria-label=\"Emoji\"><input id=\"picker-q\" type=\"search\" placeholder=\"Search emoji\" autocomplete=\"off\" aria-label=\"Search emoji\"><div id=\"picker-body\" class=\"picker-body\"></div></div>".to_owned()
}

/// The emoji picker's grid: what matches the query, or the recent ones and then every group.
pub(crate) fn picker_body(core: &ClientCore, query: &str) -> String {
    let tone = skin_tone(core.emoji_prefs().skin_tone);
    let mut out = String::new();
    let button = |out: &mut String, emoji: ReactionEmoji, label: &str| {
        push!(
            out,
            "<button type=\"button\" class=\"pick\" title=\"{}\"{}>{}</button>",
            esc(label),
            data(&[("action", "pick-emoji"), ("emoji", &emoji_wire(emoji))]),
            emoji_html(core, emoji)
        );
    };
    let query = query.trim();
    if !query.is_empty() {
        out.push_str("<div class=\"grid\">");
        for custom in core.custom_emoji_search(query, 40) {
            button(
                &mut out,
                ReactionEmoji::Custom(custom.id),
                &format!(":{}:", custom.name),
            );
        }
        for found in emoji::search(query, 120) {
            let found = emoji::with_tone(found, tone);
            button(&mut out, ReactionEmoji::Unicode(found), found.name());
        }
        out.push_str("</div>");
        if out == "<div class=\"grid\"></div>" {
            out.push_str("<p class=\"muted\">No emoji match.</p>");
        }
        return out;
    }
    let recent = &core.emoji_prefs().recent;
    if !recent.is_empty() {
        out.push_str("<h3>Recent</h3><div class=\"grid\">");
        for emoji in recent {
            let label = match emoji {
                ReactionEmoji::Unicode(e) => e.name().to_owned(),
                ReactionEmoji::Custom(id) => {
                    format!(":{}:", core.custom_emoji_label(*id).unwrap_or("emoji"))
                }
            };
            button(&mut out, *emoji, &label);
        }
        out.push_str("</div>");
    }
    let customs: Vec<_> = core.live_custom_emoji().collect();
    if !customs.is_empty() {
        out.push_str("<h3>This workspace</h3><div class=\"grid\">");
        for custom in customs {
            button(
                &mut out,
                ReactionEmoji::Custom(custom.id),
                &format!(":{}:", custom.name),
            );
        }
        out.push_str("</div>");
    }
    for group in emoji::groups() {
        push!(
            out,
            "<h3>{}</h3><div class=\"grid\">",
            esc(group_name(group))
        );
        for item in emoji::in_group(group) {
            let item = emoji::with_tone(item, tone);
            button(&mut out, ReactionEmoji::Unicode(item), item.name());
        }
        out.push_str("</div>");
    }
    out
}

fn skin_tone(index: u8) -> SkinTone {
    lazy_domain::emoji::skin_tone_from_index(index)
}

fn group_name(group: emoji::Group) -> &'static str {
    use emoji::Group;
    match group {
        Group::SmileysAndEmotion => "Smileys & emotion",
        Group::PeopleAndBody => "People & body",
        Group::AnimalsAndNature => "Animals & nature",
        Group::FoodAndDrink => "Food & drink",
        Group::TravelAndPlaces => "Travel & places",
        Group::Activities => "Activities",
        Group::Objects => "Objects",
        Group::Symbols => "Symbols",
        Group::Flags => "Flags",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazy_protocol::SnippetRange;

    #[test]
    fn snippets_mark_matches_and_escape_the_rest() {
        let snippet = Snippet {
            text: "a <b> deploy é deploy".to_owned(),
            highlights: vec![
                SnippetRange { start: 6, end: 12 },
                SnippetRange { start: 16, end: 22 },
                SnippetRange { start: 3, end: 100 },
            ],
        };
        assert_eq!(
            snippet_html(&snippet),
            "a &lt;b&gt; <mark>deploy</mark> é <mark>deploy</mark>"
        );
    }

    #[test]
    fn the_picker_finds_emoji_by_name() {
        let core = ClientCore::new();
        let html = picker_body(&core, "thumbs up");
        assert!(html.contains("👍"), "{html}");
        let html = picker_body(&core, "");
        assert!(html.contains("<h3>Smileys &amp; emotion</h3>"), "{html}");
    }
}
