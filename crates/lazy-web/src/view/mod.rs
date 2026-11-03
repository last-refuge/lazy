//! The web client's screens, as HTML built from the core's state and [`Ui`], the little state that
//! belongs to the page alone (which dialog is open, which message is being edited).
//!
//! Every function here is pure: it reads and returns text, so the screens are tested without a
//! browser. [`crate::app`] puts the text on the page, and keeps what a person is typing in
//! elements it never replaces.

pub(crate) mod dialogs;
pub(crate) mod document;
pub(crate) mod html;
pub(crate) mod icons;
pub(crate) mod sidebar;
pub(crate) mod signin;
pub(crate) mod suggest;
pub(crate) mod time;
pub(crate) mod timeline;

use lazy_client_core::{ClientCore, ConnectionState, ItemKey};
use lazy_domain::notification::PromptKind;
use lazy_domain::{ConversationId, ConversationKind, MessageId, UserId};
use lazy_protocol::api::paths;

use html::{esc, initials, push, tint};

/// A dialog over the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dialog {
    /// The channel directory.
    Browse,
    CreateChannel,
    /// Start a direct or group conversation; the people picked so far.
    NewMessage {
        picked: Vec<UserId>,
    },
    Search,
    /// Jump to a conversation by name (Ctrl+K).
    Switcher {
        query: String,
        highlighted: usize,
    },
    /// The person's own account: their profile, or their password.
    Account(AccountPage),
}

/// A page of the account dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountPage {
    Profile,
    Security,
}

impl AccountPage {
    pub(crate) fn id(self) -> &'static str {
        match self {
            AccountPage::Profile => "profile",
            AccountPage::Security => "security",
        }
    }

    pub(crate) fn parse(id: &str) -> Option<AccountPage> {
        match id {
            "profile" => Some(AccountPage::Profile),
            "security" => Some(AccountPage::Security),
            _ => None,
        }
    }
}

impl Dialog {
    /// Which shell the dialog has: a different kind means a new shell (and new fields).
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Dialog::Browse => "browse",
            Dialog::CreateChannel => "create-channel",
            Dialog::NewMessage { .. } => "new-message",
            Dialog::Search => "search",
            Dialog::Switcher { .. } => "switcher",
            Dialog::Account(AccountPage::Profile) => "account-profile",
            Dialog::Account(AccountPage::Security) => "account-security",
        }
    }
}

/// What the emoji picker is choosing for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerFor {
    Reaction {
        conversation: ConversationId,
        message: MessageId,
    },
    /// Insert into the conversation's composer.
    Composer,
    /// Insert into the thread's reply composer.
    Thread,
}

/// A small menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Menu {
    /// The workspace's menu: other workspaces, signing out.
    Workspace,
    /// The open conversation's menu.
    Conversation(ConversationId),
    /// The person's own menu: who they are, their profile and password, signing out.
    Account,
}

/// Suggestions open in a composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Suggesting {
    /// The composer's element id: `draft` or `thread-draft`.
    pub field: &'static str,
    pub token: suggest::Token,
    pub highlighted: usize,
}

/// The page's own state.
#[derive(Debug, Default)]
pub(crate) struct Ui {
    pub suggest: Option<Suggesting>,
    pub dialog: Option<Dialog>,
    pub picker: Option<PickerFor>,
    pub picker_query: String,
    pub menu: Option<Menu>,
    /// The message the composer is editing instead of writing a new one.
    pub editing: Option<(ConversationId, MessageId)>,
    /// The sign-in screen shows the registration form.
    pub registering: bool,
    /// The sidebar is open over the conversation (narrow screens).
    pub sidebar_open: bool,
    /// A password change was sent and its answer has not been seen.
    pub password_sent: bool,
    /// The last password change went through.
    pub password_changed: bool,
}

/// The window title, with the count of what needs attention.
pub(crate) fn title(count: u32) -> String {
    if count == 0 {
        "Lazy".to_owned()
    } else {
        format!("({}) Lazy", html::count(count))
    }
}

/// A person's picture, or their initials on a colour of their own.
pub(crate) fn avatar(core: &ClientCore, user: UserId, class: &str) -> String {
    let version = core.avatar_version(user);
    if version > 0 {
        format!(
            "<img class=\"avatar {class}\" src=\"{}\" alt=\"\" loading=\"lazy\">",
            esc(&paths::user_avatar(user, version))
        )
    } else {
        let name = core.display_name(user);
        format!(
            "<span class=\"avatar {class} t{}\" aria-hidden=\"true\">{}</span>",
            tint(&user.to_string()),
            esc(&initials(name))
        )
    }
}

/// How a conversation is named in a heading: `#name` for a channel, the people otherwise.
pub(crate) fn conversation_name(core: &ClientCore, id: ConversationId) -> String {
    let title = core.conversation_title(id);
    match core.conversation(id).map(|c| c.kind) {
        Some(ConversationKind::Channel) => format!("#{title}"),
        _ => title,
    }
}

/// The text the composer shows when empty.
pub(crate) fn composer_placeholder(core: &ClientCore, id: ConversationId) -> String {
    format!("Message {}", conversation_name(core, id))
}

/// The other person in a direct conversation.
pub(crate) fn direct_partner(core: &ClientCore, id: ConversationId) -> Option<UserId> {
    let conversation = core.conversation(id)?;
    if conversation.kind != ConversationKind::Direct {
        return None;
    }
    let me = core.me()?.id;
    conversation
        .participants
        .iter()
        .copied()
        .find(|user| *user != me)
        .or(Some(me))
}

/// The conversation's header: its name, topic, and what can be done with it.
pub(crate) fn header(core: &ClientCore, id: ConversationId) -> String {
    let mut out = String::new();
    let Some(conversation) = core.conversation(id) else {
        return out;
    };
    let favorite = core.user_state(id).is_some_and(|state| state.favorite);
    push!(
        out,
        "<button type=\"button\" class=\"icon menu-toggle\" data-action=\"toggle-sidebar\" aria-label=\"Conversations\">{}</button>",
        icons::icon("menu")
    );
    out.push_str("<div class=\"hd-main\"><h1 class=\"hd-title\">");
    if let Some(partner) = direct_partner(core, id) {
        out.push_str(&presence_dot(core, partner));
    }
    push!(out, "{}</h1>", esc(&conversation_name(core, id)));
    push!(
        out,
        "<button type=\"button\" class=\"icon star{}\" data-action=\"favorite\" data-conv=\"{id}\" aria-pressed=\"{favorite}\" title=\"{}\">{}</button>",
        if favorite { " on" } else { "" },
        if favorite {
            "Remove from favorites"
        } else {
            "Add to favorites"
        },
        if favorite {
            icons::icon("star-filled")
        } else {
            icons::icon("star")
        }
    );
    if let Some(topic) = conversation.topic.as_deref().filter(|t| !t.is_empty()) {
        push!(
            out,
            "<span class=\"hd-topic\" title=\"{0}\">{0}</span>",
            esc(topic)
        );
    }
    out.push_str("</div><div class=\"hd-side\">");
    if let Some(drop_in) = &conversation.drop_in {
        push!(
            out,
            "<span class=\"live\" title=\"Join drop-ins from the Lazy app\"><span class=\"dot online\"></span> Drop-in · {}</span>",
            drop_in.participants.len()
        );
    }
    if conversation.kind == ConversationKind::Channel {
        push!(
            out,
            "<span class=\"members\" title=\"Members\">{} {}</span>",
            icons::icon("people"),
            conversation.member_count
        );
    }
    push!(
        out,
        "<button type=\"button\" class=\"icon\" data-action=\"conversation-menu\" data-conv=\"{id}\" aria-label=\"More\">{}</button>",
        icons::icon("more")
    );
    out.push_str("</div>");
    out
}

/// A dot for whether someone is online; nothing while that is not known.
pub(crate) fn presence_dot(core: &ClientCore, user: UserId) -> String {
    match core.is_online(user) {
        Some(true) => "<span class=\"dot online\" title=\"Online\"></span>".to_owned(),
        Some(false) => "<span class=\"dot offline\" title=\"Away\"></span>".to_owned(),
        None => String::new(),
    }
}

/// The line under the header: the connection, when it is not live, and the core's notice.
pub(crate) fn banner(core: &ClientCore) -> String {
    let mut out = String::new();
    match core.connection() {
        ConnectionState::Reconnecting { attempt } if attempt > 1 => {
            out.push_str("<div class=\"bar warn\">Reconnecting… Messages you send will go when the connection is back.</div>");
        }
        ConnectionState::Reconnecting { .. } | ConnectionState::Connecting => {
            out.push_str("<div class=\"bar\">Connecting…</div>");
        }
        _ => {}
    }
    match core.notification_prompt() {
        Some(PromptKind::Enable | PromptKind::EnableAgain) => out.push_str(
            "<div class=\"bar\"><span>Get a notification in this browser when someone messages or mentions you.</span><span><button type=\"button\" class=\"primary\" data-action=\"notifications-on\">Turn on</button> <button type=\"button\" class=\"link\" data-action=\"notifications-later\">Not now</button></span></div>",
        ),
        Some(PromptKind::OpenSettings) => out.push_str(
            "<div class=\"bar\"><span>Notifications are blocked for this site. Allow them in your browser's site settings to be notified.</span><button type=\"button\" class=\"link\" data-action=\"notifications-later\">Dismiss</button></div>",
        ),
        None => {}
    }
    if let Some(notice) = core.notice() {
        push!(
            out,
            "<div class=\"bar notice\"><span>{}</span><button type=\"button\" class=\"link\" data-action=\"dismiss-notice\">Dismiss</button></div>",
            esc(notice)
        );
    }
    out
}

/// What stands where the composer would be when there is nothing to write in: a channel the
/// person can join, or an archived one. `None` shows the composer.
pub(crate) fn composer_blocker(core: &ClientCore, id: ConversationId) -> Option<String> {
    let conversation = core.conversation(id);
    if core.join_offer() == Some(id) {
        return Some(format!(
            "<div class=\"blocker\"><span>You're reading {}.</span><button type=\"button\" class=\"primary\" data-action=\"join\" data-conv=\"{id}\">Join channel</button></div>",
            esc(&conversation_name(core, id))
        ));
    }
    if conversation.is_some_and(|c| c.archived) {
        return Some(
            "<div class=\"blocker\"><span>This channel is archived.</span></div>".to_owned(),
        );
    }
    None
}

/// The files waiting in a composer, as chips.
pub(crate) fn chips(core: &ClientCore, target: lazy_client_core::ComposerTarget) -> String {
    use lazy_client_core::AttachmentState;
    let mut out = String::new();
    let target_name = match target {
        lazy_client_core::ComposerTarget::Conversation(_) => "conversation",
        lazy_client_core::ComposerTarget::Thread(_) => "thread",
    };
    for file in core.composer_attachments(target) {
        let (state, class) = match &file.state {
            AttachmentState::Reading => ("Reading…".to_owned(), ""),
            AttachmentState::Uploading { sent, total } => {
                let percent = if *total == 0 { 0 } else { sent * 100 / total };
                (format!("Uploading {percent}%"), "")
            }
            AttachmentState::Uploaded => (html::size(file.size), ""),
            AttachmentState::Failed { reason, .. } => (reason.clone(), " failed"),
        };
        push!(
            out,
            "<span class=\"chip{class}\"><span class=\"chip-name\">{}</span><span class=\"chip-state\">{}</span>",
            esc(&file.name),
            esc(&state)
        );
        if let AttachmentState::Failed {
            can_retry: true, ..
        } = file.state
        {
            push!(
                out,
                "<button type=\"button\" class=\"link\" data-action=\"retry-file\" data-target=\"{target_name}\" data-id=\"{}\">Retry</button>",
                file.id
            );
        }
        push!(
            out,
            "<button type=\"button\" class=\"icon\" data-action=\"remove-file\" data-target=\"{target_name}\" data-id=\"{}\" aria-label=\"Remove {}\">{}</button></span>",
            file.id,
            esc(&file.name),
            icons::icon("close")
        );
    }
    out
}

/// A timeline row's key, as the page names it.
pub(crate) fn key_text(key: ItemKey) -> String {
    match key {
        ItemKey::Operation(operation) => format!("o{operation}"),
        ItemKey::Message(message) => format!("m{message}"),
    }
}

/// The page while there is no session or it is loading.
pub(crate) fn splash(text: &str, retry: bool) -> String {
    let mut out = String::new();
    push!(
        out,
        "<div class=\"splash\"><div class=\"brand\">Lazy</div><p>{}</p>",
        esc(text)
    );
    if retry {
        out.push_str(
            "<button type=\"button\" class=\"primary\" data-action=\"retry-load\">Try again</button>",
        );
    }
    out.push_str("</div>");
    out
}

#[cfg(test)]
pub(crate) mod testing {
    //! A core with a signed-in person and a workspace, for the views' tests.

    use lazy_client_core::{ClientCore, Input, StartupState};
    use lazy_domain::{
        ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
        MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
    };
    use lazy_protocol::{
        BootstrapResponse, Conversation, ConversationUserState, Message, User, Workspace,
        WorkspaceBootstrap,
    };

    pub(crate) struct World {
        pub core: ClientCore,
        pub me: User,
        pub bo: User,
        pub workspace: WorkspaceId,
        pub general: ConversationId,
        pub direct: ConversationId,
        /// The history requests the core made while loading, by conversation.
        pub history: Vec<(ConversationId, u64)>,
    }

    pub(crate) fn user(name: &str) -> User {
        User {
            id: UserId::generate(),
            username: Username::parse(&format!(
                "{}_x",
                name.split_whitespace().next().unwrap().to_lowercase()
            ))
            .unwrap(),
            display_name: DisplayName::parse(name).unwrap(),
            is_bot: false,
            status: None,
            deactivated: false,
            avatar_version: 0,
        }
    }

    pub(crate) fn message(
        conversation: ConversationId,
        author: UserId,
        text: &str,
        seq: i64,
        at: Timestamp,
    ) -> Message {
        Message {
            id: MessageId::generate(),
            conversation_id: conversation,
            author_id: author,
            document: MessageDocument::from_markdown(text).unwrap(),
            rich: None,
            notice: None,
            created_at: at,
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            thread_root: None,
            in_channel: false,
            thread: None,
            attachments: Vec::new(),
            mentions: Default::default(),
            reactions: Vec::new(),
            reactions_as_of: EventSeq::ZERO,
            deleted: false,
        }
    }

    pub(crate) fn world() -> World {
        let mut core = ClientCore::new();
        let me = user("Ada Lovelace");
        let bo = user("Bo");
        let workspace = WorkspaceId::generate();
        let general = ConversationId::generate();
        let direct = ConversationId::generate();
        let channel = Conversation {
            id: general,
            workspace_id: workspace,
            kind: ConversationKind::Channel,
            name: Some(ChannelName::parse("general").unwrap()),
            topic: Some("Say <hi>".to_owned()),
            description: None,
            visibility: ChannelVisibility::Public,
            archived: false,
            is_default: true,
            created_by: None,
            participants: Vec::new(),
            member_count: 2,
            drop_in: None,
        };
        let dm = Conversation {
            id: direct,
            kind: ConversationKind::Direct,
            name: None,
            topic: None,
            is_default: false,
            participants: vec![me.id, bo.id],
            member_count: 2,
            ..channel.clone()
        };
        let state = |id| ConversationUserState {
            conversation_id: id,
            favorite: false,
            last_read: EventSeq::ZERO,
            unread_count: 0,
            mention_count: 0,
        };
        let snapshot = BootstrapResponse {
            protocol_version: lazy_protocol::PROTOCOL_VERSION,
            user: me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                workspace: Workspace {
                    id: workspace,
                    name: "Lazy".to_owned(),
                    settings: Default::default(),
                },
                role: WorkspaceRole::Member,
                conversations: vec![channel, dm],
                user_state: vec![state(general), state(direct)],
                members: vec![me.clone(), bo.clone()],
                groups: Vec::new(),
                custom_emoji: Vec::new(),
                notification_settings: Default::default(),
                conversation_notification_prefs: Vec::new(),
                threads: Vec::new(),
                saved: Vec::new(),
                head: EventSeq::new(10).unwrap(),
            }],
        };
        core.handle(Input::Started(StartupState {
            session_restored: true,
            ..StartupState::default()
        }));
        let out = core.handle(Input::BootstrapLoaded(Box::new(snapshot)));
        let history = out
            .effects
            .iter()
            .filter_map(|effect| match effect {
                lazy_client_core::Effect::FetchHistory {
                    conversation,
                    generation,
                    ..
                } => Some((*conversation, *generation)),
                _ => None,
            })
            .collect();
        World {
            history,
            core,
            me,
            bo,
            workspace,
            general,
            direct,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_names_the_conversation_and_escapes_its_topic() {
        let world = testing::world();
        let html = header(&world.core, world.general);
        assert!(html.contains("#general</h1>"), "{html}");
        assert!(html.contains("Say &lt;hi&gt;"), "{html}");
        assert!(!html.contains("<hi>"));
        let html = header(&world.core, world.direct);
        assert!(html.contains("Bo</h1>"), "{html}");
    }

    #[test]
    fn titles_count_what_needs_attention() {
        assert_eq!(title(0), "Lazy");
        assert_eq!(title(3), "(3) Lazy");
        assert_eq!(title(500), "(99+) Lazy");
    }
}
