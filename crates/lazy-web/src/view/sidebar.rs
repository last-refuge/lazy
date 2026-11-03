//! The sidebar: the workspace, its conversations with what is unread, and the person signed in.

use lazy_client_core::ClientCore;
use lazy_domain::{ChannelVisibility, ConversationId, ConversationKind};

use super::html::{count, esc, push};
use super::icons::icon;
use super::{Menu, Ui, avatar, direct_partner, presence_dot};

pub(crate) fn sidebar(core: &ClientCore, ui: &Ui) -> String {
    let mut out = String::new();
    let rail = core.workspace_rail();
    let active = rail.iter().find(|item| item.active);
    let others_need_attention = rail
        .iter()
        .any(|item| !item.active && (item.attention > 0 || item.unread));
    push!(
        out,
        "<div class=\"sb-head\"><button type=\"button\" class=\"ws\" data-action=\"workspace-menu\" aria-haspopup=\"menu\" aria-expanded=\"{}\"><span class=\"ws-name\">{}</span>{}<span class=\"caret\">{}</span></button></div>",
        ui.menu == Some(Menu::Workspace),
        esc(active.map(|item| item.name.as_str()).unwrap_or("Lazy")),
        if others_need_attention {
            "<span class=\"dot unread\" title=\"Another workspace has unread messages\"></span>"
        } else {
            ""
        },
        icon("caret")
    );
    push!(
        out,
        "<div class=\"sb-tools\"><button type=\"button\" class=\"tool\" data-action=\"open-switcher\" title=\"Jump to (Ctrl+K)\">{} Jump to…</button><button type=\"button\" class=\"tool\" data-action=\"open-search\" title=\"Search messages\">{} Search</button></div>",
        icon("jump"),
        icon("search")
    );
    out.push_str("<div class=\"sb-scroll\">");
    let sidebar = core.sidebar();
    let selected = core.selected_conversation();
    if !sidebar.favorites.is_empty() {
        section(
            &mut out,
            core,
            "Favorites",
            &sidebar.favorites,
            selected,
            None,
        );
    }
    section(
        &mut out,
        core,
        "Channels",
        &sidebar.channels,
        selected,
        Some(("browse", "Browse or create channels")),
    );
    section(
        &mut out,
        core,
        "Direct messages",
        &sidebar.direct_and_group,
        selected,
        Some(("new-message", "New message")),
    );
    out.push_str("</div>");
    if let Some(me) = core.me() {
        push!(
            out,
            "<div class=\"sb-me\"><button type=\"button\" class=\"me\" data-action=\"account-menu\" aria-haspopup=\"menu\" aria-expanded=\"{}\" title=\"Your profile, password, and signing out\">{}<span class=\"me-name\">{}</span><span class=\"caret\">{}</span></button><button type=\"button\" class=\"link\" data-action=\"sign-out\">Sign out</button></div>",
            ui.menu == Some(Menu::Account),
            avatar(core, me.id, "small"),
            esc(me.display_name.as_str()),
            icon("caret")
        );
    }
    out
}

fn section(
    out: &mut String,
    core: &ClientCore,
    title: &str,
    ids: &[ConversationId],
    selected: Option<ConversationId>,
    add: Option<(&str, &str)>,
) {
    push!(
        out,
        "<div class=\"sb-section\"><div class=\"sb-title\"><span>{}</span>",
        esc(title)
    );
    if let Some((action, label)) = add {
        push!(
            out,
            "<button type=\"button\" class=\"icon add\" data-action=\"{action}\" title=\"{0}\" aria-label=\"{0}\">{1}</button>",
            esc(label),
            icon("plus")
        );
    }
    out.push_str("</div><ul>");
    for id in ids {
        item(out, core, *id, selected == Some(*id));
    }
    out.push_str("</ul></div>");
}

fn item(out: &mut String, core: &ClientCore, id: ConversationId, active: bool) {
    let Some(conversation) = core.conversation(id) else {
        return;
    };
    let state = core.user_state(id);
    let unread = state.is_some_and(|s| s.unread_count > 0);
    let mentions = state.map_or(0, |s| s.mention_count);
    let title = core.conversation_title(id);
    let mut classes = vec!["conv"];
    if active {
        classes.push("active");
    }
    if unread {
        classes.push("unread");
    }
    if conversation.archived {
        classes.push("archived");
    }
    let icon = match conversation.kind {
        ConversationKind::Channel => match conversation.visibility {
            ChannelVisibility::Private => format!("<span class=\"glyph\">{}</span>", icon("lock")),
            ChannelVisibility::Public => format!("<span class=\"glyph\">{}</span>", icon("hash")),
        },
        ConversationKind::Direct => {
            let partner = direct_partner(core, id);
            match partner {
                Some(partner) => format!(
                    "<span class=\"face\">{}{}</span>",
                    avatar(core, partner, "tiny"),
                    presence_dot(core, partner)
                ),
                None => "<span class=\"glyph\">@</span>".to_owned(),
            }
        }
        ConversationKind::Group => format!(
            "<span class=\"glyph group\">{}</span>",
            conversation.participants.len().saturating_sub(1)
        ),
    };
    push!(
        out,
        "<li><button type=\"button\" class=\"{}\" data-action=\"select\" data-conv=\"{id}\"{}>{icon}<span class=\"conv-name\">{}</span>",
        classes.join(" "),
        if active { " aria-current=\"page\"" } else { "" },
        esc(&title)
    );
    if conversation.drop_in.is_some() {
        out.push_str("<span class=\"live-mark\" title=\"A drop-in is going on\">Live</span>");
    }
    if mentions > 0 {
        push!(
            out,
            "<span class=\"badge\" aria-label=\"{mentions} mentions\">{}</span>",
            count(mentions)
        );
    }
    out.push_str("</button></li>");
}

/// The workspace menu: the other workspaces, and signing out.
pub(crate) fn workspace_menu(core: &ClientCore) -> String {
    let mut out = String::from("<div class=\"menu\" role=\"menu\">");
    for item in core.workspace_rail() {
        push!(
            out,
            "<button type=\"button\" role=\"menuitem\" class=\"{}\" data-action=\"switch-workspace\" data-id=\"{}\">{}{}</button>",
            if item.active { "active" } else { "" },
            item.id,
            esc(&item.name),
            if item.attention > 0 {
                format!("<span class=\"badge\">{}</span>", count(item.attention))
            } else if item.unread {
                "<span class=\"dot unread\"></span>".to_owned()
            } else {
                String::new()
            }
        );
    }
    out.push_str("<hr><button type=\"button\" role=\"menuitem\" data-action=\"sign-out\">Sign out</button></div>");
    out
}

/// The person's own menu: who they are (name and username), then their profile and password,
/// then signing out.
pub(crate) fn account_menu(core: &ClientCore) -> String {
    let mut out = String::from("<div class=\"menu\" role=\"menu\">");
    if let Some(me) = core.me() {
        push!(
            out,
            "<div class=\"menu-who\">{}<span><span class=\"menu-name\">{}</span><span class=\"muted\">@{}</span></span></div><hr>",
            avatar(core, me.id, "small"),
            esc(me.display_name.as_str()),
            esc(me.username.as_str())
        );
    }
    out.push_str("<button type=\"button\" role=\"menuitem\" data-action=\"account\" data-page=\"profile\"><span>Profile…</span><span class=\"muted\">Name and username</span></button>");
    out.push_str("<button type=\"button\" role=\"menuitem\" data-action=\"account\" data-page=\"security\"><span>Password &amp; security…</span><span class=\"muted\">Change your password</span></button>");
    out.push_str("<hr><button type=\"button\" role=\"menuitem\" data-action=\"sign-out\">Sign out</button></div>");
    out
}

/// The open conversation's menu.
pub(crate) fn conversation_menu(core: &ClientCore, id: ConversationId) -> String {
    let mut out = String::from("<div class=\"menu\" role=\"menu\">");
    let favorite = core.user_state(id).is_some_and(|s| s.favorite);
    push!(
        out,
        "<button type=\"button\" role=\"menuitem\" data-action=\"favorite\" data-conv=\"{id}\">{}</button>",
        if favorite {
            "Remove from favorites"
        } else {
            "Add to favorites"
        }
    );
    if core.can_edit_details(id) {
        push!(
            out,
            "<button type=\"button\" role=\"menuitem\" data-action=\"edit-topic\" data-conv=\"{id}\">Edit topic</button>"
        );
    }
    if core.can_rename(id) {
        push!(
            out,
            "<button type=\"button\" role=\"menuitem\" data-action=\"rename\" data-conv=\"{id}\">Rename channel</button>"
        );
    }
    if core.can_leave(id) {
        push!(
            out,
            "<button type=\"button\" role=\"menuitem\" data-action=\"leave\" data-conv=\"{id}\">Leave channel</button>"
        );
    }
    out.push_str("</div>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::testing::world;

    #[test]
    fn the_sidebar_lists_channels_and_people() {
        let w = world();
        let html = sidebar(&w.core, &Ui::default());
        assert!(
            html.contains("<span class=\"ws-name\">Lazy</span>"),
            "{html}"
        );
        assert!(html.contains("Channels"));
        assert!(html.contains(&format!("data-conv=\"{}\"", w.general)));
        assert!(html.contains(">general</span>"), "{html}");
        assert!(html.contains(">Bo</span>"), "{html}");
        assert!(html.contains("Ada Lovelace"));
        assert!(html.contains("data-action=\"sign-out\""));
        assert!(
            html.contains("data-action=\"account-menu\""),
            "the name opens a menu"
        );
        let menu = account_menu(&w.core);
        assert!(
            menu.contains(&format!("@{}", w.me.username.as_str())),
            "{menu}"
        );
        assert!(menu.contains("data-page=\"profile\""));
        assert!(menu.contains("data-page=\"security\""));
    }
}
