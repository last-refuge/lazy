//! Renders Lazy's real views offscreen and writes PNG screenshots.
//!
//! This uses GPUI's headless renderer: the same wgpu renderer and text system the desktop app
//! uses, drawing into a texture. No window is created and nothing is shown on any display, so it
//! is safe to run anywhere with a Vulkan device. It exists to *look* at layouts and themes.
//!
//!     cargo run -p lazy-ui --example screens -- <output-dir>
//!
//! The content shown is simulated and every scene is generated here, not read from a server.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AppContext as _, Context, Entity, FocusHandle, HeadlessAppContext, IntoElement, Keystroke,
    ParentElement as _, Render, Styled as _, Window, WindowAppearance, WindowHandle, div, px, size,
};
use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_client_core::{
    ActionKind, ActionOutcome, ComposerTarget, Delivery, EditState, Effect, Input, ItemKey,
    RequestError, RetryKind, SocketEvent, StartupState, TimelineItem,
};
use lazy_domain::document::{Block, Inline};
use lazy_domain::mention::{MapDirectory, MentionTarget};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EmojiId,
    EventSeq, GroupHandle, GroupId, MessageDocument, MessageId, Timestamp, UserId, Username,
    WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, BrowsedChannel, Conversation, ConversationMembersResponse,
    ConversationUserState, CustomEmoji, ErrorCode, Event, EventEnvelope, HistoryPage, Mentions,
    Message, PROTOCOL_VERSION, ServerFrame, ThreadSummary, ThreadUserState, User, UserGroup,
    Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::message_row::{
    EditorNotice, EditorPhase, EditorProps, RetryHandler, RowAction, RowActions, RowProps,
    render_message_row, row_layout,
};
use lazy_ui::rich_text::open_external_only;
use lazy_ui::root_view::RootView;
use lazy_ui::shell::{PickerPurpose, ShellDialog};
use lazy_ui::text_input::{TextInput, TextInputOptions};
use lazy_ui::theme::Theme;

/// Records the effects the core asks for so a scene can answer them the way a runtime would.
#[derive(Default)]
struct Recorder(RefCell<Vec<Effect>>);
impl EffectSink for Recorder {
    fn execute(&self, effects: Vec<Effect>) {
        self.0.borrow_mut().extend(effects);
    }
}

fn user(name: &str, display: &str) -> User {
    User {
        is_bot: false,
        deactivated: false,
        avatar_version: 0,
        status: None,
        id: UserId::generate(),
        username: Username::parse(name).unwrap(),
        display_name: DisplayName::parse(display).unwrap(),
    }
}

struct Cast {
    me: User,
    bob: User,
    zoe: User,
    carol: User,
    dana: User,
    /// An app: the bot of an incoming webhook.
    deploy: User,
    workspace: WorkspaceId,
    general: ConversationId,
    design: ConversationId,
    engineering: ConversationId,
    old_launch: ConversationId,
    dm_bob: ConversationId,
    group: ConversationId,
    /// User groups: `team` (alice is in it), `ops` (she is not), and `design-crew`.
    team: UserGroup,
    ops: UserGroup,
    crew: UserGroup,
    /// The two threads the screens open, built once so the conversation and the pane agree on
    /// their ids.
    short_thread_cache: std::cell::OnceCell<(Message, Vec<Message>)>,
    long_thread_cache: std::cell::OnceCell<(Message, Vec<Message>)>,
    /// The pictures the scenes attach: pixel size and a hue, by attachment, so the pretend
    /// server can answer a request for a thumbnail with a picture of the right shape.
    pictures: RefCell<std::collections::HashMap<lazy_domain::AttachmentId, (u32, u32, f32)>>,
    /// The workspace's custom emoji: eleven in use and one deleted.
    emoji: Vec<CustomEmoji>,
    /// How each is drawn (shape, hue, width, height), for the pretend server's answers.
    emoji_art: std::collections::HashMap<EmojiId, (u8, f32, u32, u32)>,
    /// Whether the workspace has emoji, and what role the person has.
    emoji_mode: std::cell::Cell<EmojiMode>,
    /// Emoji whose pictures the pretend server fails to deliver.
    failing_emoji: RefCell<std::collections::HashSet<EmojiId>>,
    /// The recent searches saved from a previous run.
    recent_searches: RefCell<Vec<String>>,
    /// Slice 3D: unread replies in followed threads, whether the group is muted, a pause, quiet
    /// hours, and the person's own status.
    followed_unread: std::cell::Cell<u32>,
    muted_group: std::cell::Cell<bool>,
    dnd_until: std::cell::Cell<Option<Timestamp>>,
    quiet: std::cell::Cell<Option<lazy_domain::notification::QuietHours>>,
    my_status: RefCell<Option<lazy_domain::notification::UserStatus>>,
    /// Whether the person is in an archived channel (`old-launch`).
    archived_member: std::cell::Cell<bool>,
    /// The workspace's settings, and the usernames of deactivated members.
    workspace_settings: std::cell::Cell<lazy_domain::workspace::WorkspaceSettings>,
    deactivated: RefCell<Vec<&'static str>>,
    /// Other people's statuses, by username.
    others_status: RefCell<Vec<(&'static str, lazy_domain::notification::UserStatus)>>,
    /// Slice 3G: the person belongs to two more workspaces (one with things needing them, one
    /// with only unread messages), and these usernames have a picture.
    more_workspaces: std::cell::Cell<bool>,
    avatars: RefCell<Vec<&'static str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmojiMode {
    /// The workspace has emoji; the person is its owner.
    Populated,
    /// None yet, and the person is a plain member.
    EmptyAsMember,
    /// None yet, and the person is an admin.
    EmptyAsAdmin,
}

impl Cast {
    fn new() -> Self {
        let (me, bob, zoe, carol, dana) = (
            user("alice", "Alice Nguyen"),
            user("bob", "Bob Okafor"),
            user("zoe", "Zoë Lindqvist"),
            user("carol", "Carol Mbeki"),
            user("dana", "Dana Whitfield"),
        );
        let workspace = WorkspaceId::generate();
        let group = |handle: &str, name: &str, members: Vec<UserId>| UserGroup {
            id: GroupId::generate(),
            workspace_id: workspace,
            handle: GroupHandle::parse(handle).unwrap(),
            name: DisplayName::parse(name).unwrap(),
            members,
        };
        let team = group("team", "Design Team", vec![me.id, bob.id, zoe.id]);
        let ops = group("ops", "Operations", vec![zoe.id, carol.id]);
        let crew = group("design-crew", "Design Crew", vec![bob.id]);
        // name, shape, hue, width, height, deleted
        let art: [(&str, u8, f32, u32, u32, bool); 12] = [
            ("shipit", 0, 0.08, 128, 128, false),
            ("partyparrot", 1, 0.85, 128, 128, false),
            ("blobheart", 2, 0.98, 128, 128, false),
            ("wide_logo", 3, 0.58, 128, 64, false),
            ("tall_tower", 4, 0.40, 64, 128, false),
            ("lgtm", 5, 0.33, 128, 128, false),
            ("this_is_fine", 0, 0.02, 128, 128, false),
            ("needs_review", 0, 0.60, 128, 128, false),
            ("hotfix", 1, 0.10, 128, 128, false),
            ("rollout", 3, 0.75, 128, 64, false),
            ("cat_typing", 0, 0.15, 128, 128, false),
            ("old_logo", 3, 0.50, 128, 64, true),
        ];
        let mut emoji_art = std::collections::HashMap::new();
        let emoji: Vec<CustomEmoji> = art
            .iter()
            .map(|(name, shape, hue, w, h, deleted)| {
                let id = EmojiId::generate();
                emoji_art.insert(id, (*shape, *hue, *w, *h));
                CustomEmoji {
                    id,
                    workspace_id: workspace,
                    name: (*name).to_owned(),
                    created_by: if name.len() % 2 == 0 { bob.id } else { zoe.id },
                    created_at: Timestamp::from(
                        time::OffsetDateTime::now_utc() - time::Duration::days(3),
                    ),
                    deleted: *deleted,
                }
            })
            .collect();
        Self {
            me,
            bob,
            zoe,
            carol,
            dana,
            deploy: User {
                is_bot: true,
                deactivated: false,
                status: None,
                ..user("deploy-bot", "Deploy bot")
            },
            workspace,
            general: ConversationId::generate(),
            design: ConversationId::generate(),
            engineering: ConversationId::generate(),
            old_launch: ConversationId::generate(),
            dm_bob: ConversationId::generate(),
            group: ConversationId::generate(),
            team,
            ops,
            crew,
            short_thread_cache: std::cell::OnceCell::new(),
            long_thread_cache: std::cell::OnceCell::new(),
            pictures: RefCell::default(),
            emoji,
            emoji_art,
            emoji_mode: std::cell::Cell::new(EmojiMode::Populated),
            failing_emoji: RefCell::default(),
            recent_searches: RefCell::default(),
            followed_unread: std::cell::Cell::new(0),
            muted_group: std::cell::Cell::new(false),
            dnd_until: std::cell::Cell::new(None),
            quiet: std::cell::Cell::new(None),
            my_status: RefCell::default(),
            others_status: RefCell::default(),
            archived_member: std::cell::Cell::new(false),
            workspace_settings: std::cell::Cell::new(Default::default()),
            deactivated: RefCell::default(),
            more_workspaces: std::cell::Cell::new(false),
            avatars: RefCell::default(),
        }
    }

    /// `user` with a picture, when the scene gave them one.
    fn with_avatar(&self, user: &User) -> User {
        User {
            avatar_version: u32::from(self.avatars.borrow().contains(&user.username.as_str())),
            ..user.clone()
        }
    }

    /// A workspace the person belongs to but is not looking at: a #general with `unread` messages
    /// of which `mentions` mention them.
    fn background_workspace(&self, name: &str, unread: u32, mentions: u32) -> WorkspaceBootstrap {
        let id = WorkspaceId::generate();
        let general = ConversationId::generate();
        WorkspaceBootstrap {
            saved: Vec::new(),
            notification_settings: Default::default(),
            conversation_notification_prefs: Vec::new(),
            custom_emoji: Vec::new(),
            threads: Vec::new(),
            groups: Vec::new(),
            workspace: Workspace {
                id,
                name: name.into(),
                settings: Default::default(),
            },
            role: WorkspaceRole::Member,
            conversations: vec![Conversation {
                workspace_id: id,
                ..self.channel(general, "general", "Everything", false)
            }],
            user_state: vec![self.state(general, false, unread, mentions)],
            members: vec![self.me.clone(), self.bob.clone()],
            head: EventSeq::ZERO,
        }
    }

    /// The custom emoji named `name`.
    fn emoji(&self, name: &str) -> &CustomEmoji {
        self.emoji
            .iter()
            .find(|emoji| emoji.name == name)
            .unwrap_or_else(|| panic!("no custom emoji named {name}"))
    }

    fn custom(&self, name: &str) -> lazy_domain::ReactionEmoji {
        lazy_domain::ReactionEmoji::Custom(self.emoji(name).id)
    }

    /// A message with `:name:` turned into the workspace's emoji, as sending would.
    fn with_emoji(&self, author: &User, source: &str, seq: i64, minutes_ago: i64) -> Message {
        let mut directory = self.directory();
        for emoji in &self.emoji {
            directory.insert_emoji(&emoji.name, emoji.id);
        }
        Message {
            document: MessageDocument::from_markdown_with(source, &directory)
                .expect("a valid message"),
            ..self.message(author, "placeholder", seq, minutes_ago)
        }
    }

    /// `message` with reactions: each entry is an emoji (standard or custom), how many people
    /// used it, and whether the person is one of them.
    fn reacted_emoji(
        &self,
        mut message: Message,
        spec: &[(lazy_domain::ReactionEmoji, usize, bool)],
    ) -> Message {
        for (emoji, count, mine) in spec {
            let mut users: Vec<UserId> = [&self.bob, &self.zoe, &self.carol]
                .iter()
                .map(|user| user.id)
                .chain(std::iter::repeat_with(UserId::generate))
                .take(*count)
                .collect();
            if *mine {
                let at = 1.min(users.len().saturating_sub(1));
                users[at] = self.me.id;
            }
            message.reactions.push(lazy_protocol::ReactionSummary {
                emoji: *emoji,
                users,
            });
        }
        message.reactions_as_of = EventSeq::new(message.seq.get() + 1).unwrap();
        message
    }

    /// A picture attached to a message: `width` by `height`, drawn in `hue` (0 to 1).
    fn picture_file(
        &self,
        name: &str,
        width: u32,
        height: u32,
        hue: f32,
    ) -> lazy_protocol::Attachment {
        let id = lazy_domain::AttachmentId::generate();
        self.pictures.borrow_mut().insert(id, (width, height, hue));
        lazy_protocol::Attachment {
            id,
            name: name.to_owned(),
            media_type: "image/png".to_owned(),
            size: u64::from(width) * u64::from(height) / 3,
            sha256: "ab".repeat(32),
            width: Some(width),
            height: Some(height),
            has_thumbnail: true,
        }
    }

    /// Any other file.
    fn plain_file(&self, name: &str, media_type: &str, size: u64) -> lazy_protocol::Attachment {
        lazy_protocol::Attachment {
            id: lazy_domain::AttachmentId::generate(),
            name: name.to_owned(),
            media_type: media_type.to_owned(),
            size,
            sha256: "cd".repeat(32),
            width: None,
            height: None,
            has_thumbnail: false,
        }
    }

    /// A message from `author` with these files and `text` (which may be empty).
    fn with_files(
        &self,
        author: &User,
        text: &str,
        seq: i64,
        minutes_ago: i64,
        files: Vec<lazy_protocol::Attachment>,
    ) -> Message {
        let base = self.message(
            author,
            if text.is_empty() { "x" } else { text },
            seq,
            minutes_ago,
        );
        Message {
            attachments: files,
            document: if text.is_empty() {
                MessageDocument::empty()
            } else {
                base.document.clone()
            },
            ..base
        }
    }

    fn channel(&self, id: ConversationId, name: &str, topic: &str, private: bool) -> Conversation {
        Conversation {
            id,
            workspace_id: self.workspace,
            kind: ConversationKind::Channel,
            name: Some(ChannelName::parse(name).unwrap()),
            topic: Some(topic.to_owned()),
            description: Some("Where the team keeps its shared context.".into()),
            visibility: if private {
                ChannelVisibility::Private
            } else {
                ChannelVisibility::Public
            },
            archived: false,
            is_default: name == "general",
            created_by: Some(self.me.id),
            participants: vec![],
            member_count: 12,
        }
    }

    fn people(&self, id: ConversationId, others: &[&User]) -> Conversation {
        let mut participants = vec![self.me.id];
        participants.extend(others.iter().map(|user| user.id));
        Conversation {
            id,
            workspace_id: self.workspace,
            kind: if others.len() == 1 {
                ConversationKind::Direct
            } else {
                ConversationKind::Group
            },
            name: None,
            topic: None,
            description: None,
            visibility: ChannelVisibility::Private,
            archived: false,
            is_default: false,
            created_by: Some(self.me.id),
            participants,
            member_count: 1 + others.len() as u32,
        }
    }

    fn state(
        &self,
        id: ConversationId,
        favorite: bool,
        unread: u32,
        mentions: u32,
    ) -> ConversationUserState {
        ConversationUserState {
            conversation_id: id,
            favorite,
            last_read: EventSeq::ZERO,
            unread_count: unread,
            mention_count: mentions,
        }
    }

    /// A busy sidebar: a favorite, unread channels, and a direct and a group message.
    fn bootstrap(&self) -> BootstrapResponse {
        let me = User {
            status: self.my_status.borrow().clone(),
            ..self.with_avatar(&self.me)
        };
        let more = self.more_workspaces.get();
        let mut bootstrap = BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: Vec::new(),
                notification_settings: lazy_domain::notification::NotificationSettings {
                    dnd_until: self.dnd_until.get(),
                    quiet_hours: self.quiet.get(),
                    ..Default::default()
                },
                conversation_notification_prefs: if self.muted_group.get() {
                    vec![lazy_domain::notification::ConversationNotificationPrefs {
                        conversation_id: self.group,
                        level: None,
                        muted: true,
                    }]
                } else {
                    Vec::new()
                },
                custom_emoji: match self.emoji_mode.get() {
                    EmojiMode::Populated => self.emoji.clone(),
                    EmojiMode::EmptyAsMember | EmojiMode::EmptyAsAdmin => Vec::new(),
                },
                threads: Vec::new(),
                groups: vec![self.crew.clone(), self.ops.clone(), self.team.clone()],
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: self.workspace_settings.get(),
                },
                role: match self.emoji_mode.get() {
                    EmojiMode::Populated => WorkspaceRole::Owner,
                    EmojiMode::EmptyAsMember => WorkspaceRole::Member,
                    EmojiMode::EmptyAsAdmin => WorkspaceRole::Admin,
                },
                conversations: vec![
                    self.channel(
                        self.general,
                        "general",
                        "Company-wide announcements and chatter",
                        false,
                    ),
                    self.channel(self.design, "design", "Pixels, type, and feelings", true),
                    self.channel(
                        self.engineering,
                        "engineering",
                        "Builds, reviews, and incidents",
                        false,
                    ),
                    self.people(self.dm_bob, &[&self.bob]),
                    self.people(self.group, &[&self.bob, &self.zoe]),
                ]
                .into_iter()
                .chain(self.archived_member.get().then(|| {
                    let mut archived = self.channel(
                        self.old_launch,
                        "old-launch",
                        "Retired after the launch",
                        false,
                    );
                    archived.archived = true;
                    archived
                }))
                .collect(),
                user_state: vec![
                    self.state(self.general, false, 0, 0),
                    self.state(self.design, true, 0, 0),
                    self.state(self.dm_bob, false, 3, 0),
                    self.state(self.group, false, 120, 2),
                ]
                .into_iter()
                .chain(
                    self.archived_member
                        .get()
                        .then(|| self.state(self.old_launch, false, 0, 0)),
                )
                .collect(),
                members: std::iter::once(me)
                    .chain(
                        [&self.bob, &self.zoe, &self.carol, &self.dana, &self.deploy]
                            .into_iter()
                            .map(|user| User {
                                status: self
                                    .others_status
                                    .borrow()
                                    .iter()
                                    .find(|(name, _)| *name == user.username.as_str())
                                    .map(|(_, status)| status.clone()),
                                deactivated: self
                                    .deactivated
                                    .borrow()
                                    .contains(&user.username.as_str()),
                                ..self.with_avatar(user)
                            }),
                    )
                    .collect(),
                head: EventSeq::ZERO,
            }],
        };
        if more {
            bootstrap
                .workspaces
                .push(self.background_workspace("Design Guild", 14, 3));
            bootstrap
                .workspaces
                .push(self.background_workspace("Weekend Climbers", 6, 0));
        }
        bootstrap
    }

    /// What `@name` resolves to in the cast's world.
    fn directory(&self) -> MapDirectory {
        let mut directory = MapDirectory::new();
        for user in [&self.me, &self.bob, &self.zoe, &self.carol] {
            directory.insert(user.username.as_str(), MentionTarget::User { id: user.id });
        }
        for group in [&self.team, &self.ops, &self.crew] {
            directory.insert(group.handle.as_str(), MentionTarget::Group { id: group.id });
        }
        directory.insert("channel", MentionTarget::Channel);
        directory
    }

    /// A message with `@mentions` in it. `notified` is who the server says it notifies.
    fn mentioning(
        &self,
        author: &User,
        source: &str,
        notified: &[&User],
        channel: bool,
        seq: i64,
        minutes_ago: i64,
    ) -> Message {
        Message {
            attachments: vec![],
            mentions: Mentions {
                users: notified.iter().map(|user| user.id).collect(),
                channel,
            },
            document: MessageDocument::from_markdown_with(source, &self.directory())
                .expect("a valid message"),
            ..self.message(author, "placeholder", seq, minutes_ago)
        }
    }

    /// Messages that mention people: me, others, groups, `@channel`, a name that is gone, and
    /// mentions inside formatting and next to code.
    fn mentions_history(&self) -> Vec<Message> {
        let (me, bob, zoe, carol) = (&self.me, &self.bob, &self.zoe, &self.carol);
        let ghost = Message {
            attachments: vec![],
            document: MessageDocument::new(vec![Block::Paragraph {
                inlines: vec![
                    Inline::text("Ask "),
                    Inline::mention(
                        MentionTarget::User {
                            id: UserId::generate(),
                        },
                        "ghost.user",
                    ),
                    Inline::text(" about the old runbook, they wrote most of it."),
                ],
            }])
            .unwrap(),
            ..self.message(carol, "placeholder", 7, 22)
        };
        vec![
            self.mentioning(
                bob,
                "Morning @alice, can you review the new onboarding flow before standup?",
                &[me],
                false,
                1,
                95,
            ),
            self.mentioning(
                zoe,
                "@bob @carol I pushed the fix. Could one of you check it?",
                &[bob, carol],
                false,
                2,
                80,
            ),
            self.mentioning(
                carol,
                "Heads up @team: the design review moved to **3pm**. Please update your calendars.",
                &[me, bob, zoe],
                false,
                3,
                64,
            ),
            self.mentioning(
                bob,
                "@ops the deploy window opens at _midnight_, so please be ready.",
                &[zoe, carol],
                false,
                4,
                50,
            ),
            self.mentioning(
                zoe,
                "**@channel** the office is closed on Friday. Enjoy the long weekend!",
                &[],
                true,
                5,
                41,
            ),
            self.mentioning(
                me,
                "Thanks @bob, and @zoe: I'll look at the PR after lunch. (cc @design-crew)",
                &[bob, zoe],
                false,
                6,
                30,
            ),
            ghost,
            self.mentioning(
                bob,
                "`@alice` in code stays code, and so does a@b.com, but this one is real, and it \
                 sits at the end of a sentence long enough to wrap onto a second line: @alice.",
                &[me],
                false,
                8,
                14,
            ),
            self.mentioning(
                zoe,
                "All together: @alice @bob @carol @zoe @team @ops",
                &[me, bob, carol, zoe],
                false,
                9,
                6,
            ),
        ]
    }

    /// A thread summary: `replies` replies, the newest `minutes_ago` old, at log position `as_of`.
    fn thread_summary(
        &self,
        replies: u32,
        repliers: &[&User],
        minutes_ago: i64,
        as_of: i64,
    ) -> ThreadSummary {
        let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago);
        ThreadSummary {
            reply_count: replies,
            last_reply_seq: EventSeq::new(as_of).unwrap(),
            last_reply_at: Timestamp::from(at),
            recent_repliers: repliers.iter().map(|user| user.id).collect(),
            as_of: EventSeq::new(as_of).unwrap(),
        }
    }

    /// A thread's root message.
    fn thread_root(
        &self,
        author: &User,
        source: &str,
        seq: i64,
        minutes_ago: i64,
        summary: Option<ThreadSummary>,
    ) -> Message {
        Message {
            attachments: vec![],
            thread: summary,
            ..self.mentioning(author, source, &[], false, seq, minutes_ago)
        }
    }

    /// A reply to `root`.
    fn thread_reply(
        &self,
        root: &Message,
        author: &User,
        source: &str,
        seq: i64,
        minutes_ago: i64,
    ) -> Message {
        Message {
            attachments: vec![],
            thread_root: Some(root.id),
            ..self.mentioning(author, source, &[], false, seq, minutes_ago)
        }
    }

    /// Bob's question, with the thread under it: a short one.
    fn short_thread(&self) -> (Message, Vec<Message>) {
        self.short_thread_cache
            .get_or_init(|| self.build_short_thread())
            .clone()
    }

    fn build_short_thread(&self) -> (Message, Vec<Message>) {
        let root = self.thread_root(
            &self.bob,
            "Should we move the launch to next week? The migration still has **two open items**.",
            20,
            58,
            Some(self.thread_summary(2, &[&self.zoe, &self.carol], 12, 23)),
        );
        let replies = vec![
            self.thread_reply(
                &root,
                &self.carol,
                "I would. The second item is the data backfill and it is not close.",
                21,
                44,
            ),
            self.thread_reply(
                &root,
                &self.zoe,
                "Agreed, and it gives support a week to read the notes. @bob can you tell the customers?",
                23,
                12,
            ),
        ];
        (root, replies)
    }

    /// Carol's postmortem, with a long thread of replies from five people.
    fn long_thread(&self) -> (Message, Vec<Message>) {
        self.long_thread_cache
            .get_or_init(|| self.build_long_thread())
            .clone()
    }

    fn build_long_thread(&self) -> (Message, Vec<Message>) {
        let people = [&self.bob, &self.zoe, &self.dana, &self.me, &self.carol];
        let root = self.thread_root(
            &self.carol,
            "Postmortem draft for last week's incident is ready for review. Please read the timeline first.",
            30,
            240,
            Some(self.thread_summary(
                14,
                &[&self.dana, &self.zoe, &self.me, &self.bob, &self.carol],
                3,
                60,
            )),
        );
        let lines = [
            "Read it. The timeline is right, but the first alert fired four minutes earlier than the draft says.",
            "Which alert? The pager history shows two within the same minute.",
            "The disk one. The latency alert came after.",
            "Updated the draft with both. I also added the rollback steps we took, in order.",
            "One thing missing: nobody owned the customer message for the first forty minutes.",
            "That is on me. I thought support had it.",
            "Not blaming anyone, the point is the runbook does not say who sends it. Can we add that?",
            "Yes. Adding a section called **Communication** with an owner per phase.",
            "Should the owner be a role or a person?",
            "A role, with the on-call rotation filling it in. People change; roles do not.",
            "Makes sense. Draft three is up with that change.",
            "Looks good to me.",
            "Same. Shipping it to the wider team tomorrow morning unless someone objects.",
            "No objection. Thanks, everyone, for keeping this blameless.",
        ];
        let replies = lines
            .iter()
            .enumerate()
            .map(|(n, text)| {
                self.thread_reply(
                    &root,
                    people[n % people.len()],
                    text,
                    31 + n as i64,
                    230 - n as i64 * 16,
                )
            })
            .collect();
        (root, replies)
    }

    /// A conversation with threads in every state: one reply, many replies from five people, an
    /// unread followed thread, a deleted root that still has replies, and a reply that was also
    /// sent to the channel. Also returns the thread read states for the bootstrap.
    fn threads_history(&self) -> (Vec<Message>, Vec<ThreadUserState>) {
        let (short_root, short_replies) = self.short_thread();
        let (long_root, _) = self.long_thread();
        let mut deleted_root = self.thread_root(
            &self.dana,
            "placeholder",
            40,
            200,
            Some(self.thread_summary(3, &[&self.me, &self.carol], 150, 45)),
        );
        deleted_root.deleted = true;
        let also = Message {
            attachments: vec![],
            in_channel: true,
            ..self.thread_reply(
                &short_root,
                &self.zoe,
                "Posting this here as well since it affects everyone: next week works for me.",
                24,
                11,
            )
        };
        let one_reply_root = self.thread_root(
            &self.zoe,
            "Who has the staging credentials? I need them for a demo this afternoon.",
            50,
            30,
            Some(self.thread_summary(1, &[&self.bob], 25, 52)),
        );
        let history = vec![
            self.message(
                &self.zoe,
                "Morning all. Release notes are up; shout if anything is missing.",
                1,
                300,
            ),
            long_root.clone(),
            self.message(&self.bob, "Reading it now.", 32, 235),
            deleted_root,
            short_root,
            also,
            one_reply_root,
            self.message(
                &self.me,
                "Thanks all. I will pick the staging question up after lunch.",
                60,
                5,
            ),
        ];
        let _ = short_replies;
        let unread = ThreadUserState {
            root_id: long_root.id,
            conversation_id: self.general,
            following: true,
            last_read: EventSeq::new(52).unwrap(),
            unread_count: 3,
            mention_count: 0,
        };
        (history, vec![unread])
    }

    /// The member lists the server would answer with: everyone is in #general; #design and
    /// #engineering have only alice and bob.
    fn members_of(&self, conversation: ConversationId) -> Vec<UserId> {
        if conversation == self.general {
            vec![
                self.me.id,
                self.bob.id,
                self.zoe.id,
                self.carol.id,
                self.dana.id,
            ]
        } else {
            vec![self.me.id, self.bob.id]
        }
    }

    fn message(&self, author: &User, text: &str, seq: i64, minutes_ago: i64) -> Message {
        let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago);
        Message {
            attachments: vec![],
            thread_root: None,
            in_channel: false,
            thread: None,
            mentions: Default::default(),
            id: MessageId::generate(),
            conversation_id: self.general,
            author_id: author.id,
            document: MessageDocument::from_plain_text(text).unwrap(),
            created_at: Timestamp::from(at),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            rich: None,
            deleted: false,
        }
    }

    /// A message written the way people write in the composer, formatting and all.
    fn formatted(
        &self,
        author: &User,
        source: &str,
        seq: i64,
        minutes_ago: i64,
        edited: bool,
    ) -> Message {
        let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago);
        Message {
            attachments: vec![],
            document: MessageDocument::from_markdown(source).expect("a valid message"),
            revision: if edited { 2 } else { 1 },
            edited_at: edited.then(|| Timestamp::from(at + time::Duration::minutes(1))),
            ..self.message(author, "placeholder", seq, minutes_ago)
        }
    }

    /// `message` with reactions: each entry is an emoji, how many people used it, and whether the
    /// person is one of them. People beyond the cast are anonymous.
    fn reacted(&self, mut message: Message, spec: &[(&str, usize, bool)]) -> Message {
        for (glyph, count, mine) in spec {
            let mut users: Vec<UserId> = [&self.bob, &self.zoe, &self.carol]
                .iter()
                .map(|user| user.id)
                .chain(std::iter::repeat_with(UserId::generate))
                .take(*count)
                .collect();
            if *mine {
                let at = 1.min(users.len().saturating_sub(1));
                users[at] = self.me.id;
            }
            message.reactions.push(lazy_protocol::ReactionSummary {
                emoji: lazy_domain::ReactionEmoji::parse(glyph).expect("a reaction emoji"),
                users,
            });
        }
        message.reactions_as_of = EventSeq::new(message.seq.get() + 1).unwrap();
        message
    }

    /// Messages with reaction chips: varied counts, the person's own highlighted, and a long list
    /// that wraps.
    fn reactions_history(&self) -> Vec<Message> {
        let (me, bob, zoe) = (&self.me, &self.bob, &self.zoe);
        let many = [
            "🎉", "😀", "👍", "👎", "❤️", "🔥", "👀", "🙏", "💯", "✅", "❌", "⭐", "🚀", "😮",
            "😢", "🤔", "😅", "🙌", "👏", "🤝", "💪", "🥳", "😍", "🤩", "😴", "🍕", "☕", "🌈",
            "🦄", "🐶",
        ];
        let long: Vec<(&str, usize, bool)> = many
            .iter()
            .enumerate()
            .map(|(n, glyph)| (*glyph, 1 + n % 3, n % 7 == 0))
            .collect();
        vec![
            self.reacted(
                self.formatted(
                    zoe,
                    "Shipped the release. Thanks everyone! 🚀",
                    1,
                    50,
                    false,
                ),
                &[
                    ("🎉", 4, false),
                    ("👍", 12, true),
                    ("❤️", 1, false),
                    ("🚀", 2, true),
                    ("😂", 99, false),
                ],
            ),
            self.reacted(
                self.formatted(
                    bob,
                    "Reactions pile up, and wrap onto more lines when there are many.",
                    2,
                    40,
                    false,
                ),
                &long,
            ),
            self.reacted(
                self.formatted(me, "A single heart from me.", 3, 20, false),
                &[("❤️", 1, true)],
            ),
            self.formatted(bob, "And a message nobody reacted to.", 4, 10, false),
        ]
    }

    /// Every kind of formatting a message can carry.
    fn rich_history(&self) -> Vec<Message> {
        let (me, bob, zoe) = (&self.me, &self.bob, &self.zoe);
        let permalink = lazy_domain::Permalink {
            workspace_id: self.workspace,
            conversation_id: self.design,
            message_id: MessageId::generate(),
        };
        vec![
            self.formatted(
                zoe,
                "Release notes are up. *Bold*, _italic_, ~struck~, and `inline code` all work, and so do **bold _and italic_** together.",
                1,
                90,
                false,
            ),
            self.formatted(
                bob,
                &format!(
                    "Links: [the runbook](https://example.com/runbook), a bare https://example.com/a/very/long/address/that/keeps/going/and/going, and a permalink to the design thread: {permalink}"
                ),
                2,
                80,
                false,
            ),
            self.formatted(
                me,
                "Here's the fix:\n```rust\nfn retry(policy: &Policy, attempt: u32) -> Option<Duration> {\n    if attempt >= policy.max_attempts { return None; }\n    Some(policy.base * 2u32.pow(attempt)) // a long trailing comment that is much wider than the message column so it has to wrap\n}\n```\nIt backs off exponentially.",
                3,
                60,
                true,
            ),
            self.formatted(
                bob,
                "> We should keep failures visible.\n> Dropped deliveries are worse than slow ones.\nAgreed. Two things to decide:\n- retry count\n- how failures *show up*\n- who gets paged\nAnd the order of work:\n1. write the spec\n2. build the queue\n3. ship behind a flag",
                4,
                40,
                false,
            ),
            self.formatted(
                zoe,
                "```\nplain block, no language\n```",
                5,
                20,
                false,
            ),
            self.formatted(me, "Thanks all. Edited to fix a typo.", 6, 10, true),
        ]
    }

    /// A release going out, announced by an app.
    fn bot_history(&self) -> Vec<Message> {
        let (me, bob, zoe, bot) = (&self.me, &self.bob, &self.zoe, &self.deploy);
        vec![
            self.message(bob, "Kicking off the release now.", 1, 60 * 3),
            self.formatted(
                bot,
                "Deploy to production started for `v1.4.2`.",
                2,
                60 * 3 - 1,
                false,
            ),
            self.formatted(
                bot,
                "Deployed `v1.4.2` to production in 3m 12s. [View the run](https://example.com/runs/4821)",
                3,
                60 * 2,
                false,
            ),
            self.message(zoe, "Nice, thanks!", 4, 60 * 2 - 3),
            self.message(me, "Watching the error rate for the next hour.", 5, 30),
        ]
    }

    /// GitHub activity as the GitHub app posts it (the server's `github::summarize`).
    fn github_history(&self) -> Vec<Message> {
        let bot = &self.deploy;
        GITHUB_FEED
            .iter()
            .enumerate()
            .map(|(n, text)| self.formatted(bot, text, n as i64 + 1, 60 - n as i64, false))
            .collect()
    }

    /// What apps post with Slack's blocks, drawn as rich content (D40).
    fn block_kit_history(&self) -> Vec<Message> {
        use lazy_domain::{Button, ButtonStyle, RichBlock, RichContent};
        let doc = |s: &str| MessageDocument::from_markdown(s).unwrap();
        let button = |id: &str, text: &str, style: ButtonStyle, url: Option<&str>| Button {
            action_id: id.into(),
            text: text.into(),
            value: None,
            url: url.map(str::to_owned),
            style,
        };
        let with = |mut message: Message, fallback: &str, blocks: Vec<RichBlock>| {
            message.document = doc(fallback);
            message.rich = Some(RichContent::new(blocks).unwrap());
            message
        };
        let deploy = with(
            self.message(&self.deploy, "-", 1, 42),
            "Deploy 1423 is waiting for approval",
            vec![
                RichBlock::Header {
                    text: "Deploy #1423 is waiting for approval".into(),
                },
                RichBlock::Section {
                    text: Some(doc(
                        "*web* `v2.14.0` to *production*. 3 commits since the last deploy, \
                         including [the checkout fix](https://github.com/lazy/web/pull/912).",
                    )),
                    fields: vec![
                        doc("*Service*\nweb"),
                        doc("*Environment*\nproduction"),
                        doc("*Requested by*\nBob Chen"),
                        doc("*Checks*\n42 passed"),
                    ],
                    accessory: Some(button(
                        "logs",
                        "View build",
                        ButtonStyle::Default,
                        Some("https://ci.lazyto.day/builds/1423"),
                    )),
                },
                RichBlock::Divider,
                RichBlock::Actions {
                    block_id: Some("deploy-1423".into()),
                    buttons: vec![
                        button("approve", "Approve", ButtonStyle::Primary, None),
                        button("reject", "Reject", ButtonStyle::Danger, None),
                        button("later", "Remind me in 1 hour", ButtonStyle::Default, None),
                    ],
                },
                RichBlock::Context {
                    items: vec![doc("Deploy bot"), doc("Requested 42 minutes ago")],
                },
            ],
        );
        let incident = with(
            self.message(&self.deploy, "-", 2, 12),
            "High error rate on checkout",
            vec![
                RichBlock::Section {
                    text: Some(doc(
                        "*High error rate on checkout*\n5xx responses at 4.2% for 10 minutes \
                         (threshold 1%).",
                    )),
                    fields: Vec::new(),
                    accessory: Some(button("ack", "Acknowledge", ButtonStyle::Primary, None)),
                },
                RichBlock::Context {
                    items: vec![doc("PagerDuty · triggered 12 minutes ago")],
                },
            ],
        );
        let mut newer = with(
            self.message(&self.deploy, "-", 3, 3),
            "Weekly report",
            vec![RichBlock::Section {
                text: Some(doc("*Weekly report* is ready: 128 deploys, 2 rollbacks.")),
                fields: Vec::new(),
                accessory: None,
            }],
        );
        // A block from a newer server, read the way this build reads it.
        let json = serde_json::json!({"version": 2, "blocks": [
            {"type": "section", "text": serde_json::to_value(doc("*Weekly report* is ready: 128 deploys, 2 rollbacks.")).unwrap()},
            {"type": "chart", "series": [1, 2, 3]}
        ]});
        newer.rich = Some(serde_json::from_value(json).unwrap());
        vec![
            self.message(&self.bob, "Shipping the checkout fix now.", 0, 45),
            deploy,
            incident,
            newer,
        ]
    }

    /// People sharing links, for link previews (D41).
    fn link_history(&self) -> Vec<Message> {
        vec![
            self.formatted(
                &self.bob,
                "Here's the board for Thursday: https://miro.com/app/board/uXjVN3q4Ab8=/",
                1,
                30,
                false,
            ),
            self.formatted(
                &self.zoe,
                "Two reads before the review: [the RFC](https://github.com/lazy/rfcs/pull/41) and \
                 https://www.rust-lang.org/",
                2,
                12,
                false,
            ),
            self.message(&self.me, "Thanks, reading now.", 3, 4),
        ]
    }

    /// The incoming webhooks of `conversation`, oldest first.
    fn webhooks(
        &self,
        conversation: ConversationId,
        names: &[&str],
    ) -> Vec<lazy_protocol::Webhook> {
        names
            .iter()
            .enumerate()
            .map(|(n, name)| lazy_protocol::Webhook {
                id: lazy_domain::WebhookId::generate(),
                conversation_id: conversation,
                name: DisplayName::parse(name).unwrap(),
                bot_user_id: self.deploy.id,
                created_by: if n % 2 == 0 { self.me.id } else { self.bob.id },
                created_at: Timestamp::from(
                    time::OffsetDateTime::now_utc() - time::Duration::days(40 - 9 * n as i64),
                ),
            })
            .collect()
    }

    fn history(&self) -> Vec<Message> {
        let (me, bob, zoe) = (&self.me, &self.bob, &self.zoe);
        vec![
            self.message(zoe, "Morning all! The new build is out. 🎉", 1, 60 * 30),
            self.message(bob, "Nice. Did the migration finish cleanly?", 2, 60 * 29),
            self.message(zoe, "Yes — all green. I also left notes in the runbook.", 3, 60 * 29 - 1),
            self.message(me, "Thanks Zoë, reading them now.", 4, 60 * 5),
            self.message(me, "One question: does the retry policy apply to webhooks too?", 5, 60 * 5 - 1),
            self.message(
                bob,
                "Good catch. Short answer: not yet. Longer answer, because it matters for the rollout: webhook deliveries are queued separately and we planned to add bounded retries with visible failures in the integrations phase, so for now a failed delivery is simply dropped and logged.",
                6,
                42,
            ),
            self.message(bob, "I'll write it up properly.\n\nTwo things to decide:\n• retry count\n• how failures show up", 7, 41),
            self.message(zoe, "café ☕ — naïve résumé — 日本語のテキスト — 👩‍👩‍👧‍👦", 8, 20),
            self.message(me, "Sounds good. Let's pick this up after lunch.", 9, 3),
        ]
    }
}

/// Drives the core into the state a scene shows.
type BuildScene =
    dyn Fn(&Entity<AppModel>, &Cast, &mut HeadlessAppContext, &Recorder, &WindowHandle<RootView>);

struct Scene<'a> {
    name: &'a str,
    width: f32,
    height: f32,
    appearance: WindowAppearance,
    build: &'a BuildScene,
}

fn dispatch(cx: &mut HeadlessAppContext, model: &Entity<AppModel>, input: Input) {
    cx.update(|app| model.update(app, |model, cx| model.dispatch(input, cx)));
    cx.run_until_parked();
}

fn reach_chat(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    recorder: &Recorder,
    with_history: bool,
) {
    let messages = if with_history { cast.history() } else { vec![] };
    reach_chat_with(cx, model, cast, recorder, messages);
}

/// Signs in and opens #general with `messages` as its history.
fn reach_chat_with(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    recorder: &Recorder,
    messages: Vec<Message>,
) {
    reach_chat_threads(cx, model, cast, recorder, messages, Vec::new());
}

/// Like [`reach_chat_with`], with the person's read state for some threads in the bootstrap.
fn reach_chat_threads(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    recorder: &Recorder,
    messages: Vec<Message>,
    threads: Vec<ThreadUserState>,
) {
    dispatch(
        cx,
        model,
        Input::Started(StartupState {
            session_restored: true,
            recent_searches: cast.recent_searches.borrow().clone(),
            ..Default::default()
        }),
    );
    dispatch(
        cx,
        model,
        Input::BootstrapLoaded(Box::new({
            let mut bootstrap = cast.bootstrap();
            bootstrap.workspaces[0].threads = threads;
            if cast.followed_unread.get() > 0 && bootstrap.workspaces[0].threads.is_empty() {
                bootstrap.workspaces[0].threads.push(ThreadUserState {
                    root_id: MessageId::generate(),
                    conversation_id: cast.general,
                    following: true,
                    last_read: EventSeq::ZERO,
                    unread_count: cast.followed_unread.get(),
                    mention_count: 0,
                });
            }
            bootstrap
        })),
    );
    dispatch(cx, model, Input::Clock(Timestamp::now()));
    dispatch(
        cx,
        model,
        Input::Socket(SocketEvent::Opened {
            workspace: cast.workspace,
        }),
    );
    dispatch(
        cx,
        model,
        Input::Frame(ServerFrame::CaughtUp {
            workspace_id: cast.workspace,
            seq: EventSeq::ZERO,
        }),
    );
    let generation = recorder
        .0
        .borrow()
        .iter()
        .find_map(|effect| match effect {
            Effect::FetchHistory { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("loading #general fetches its history");
    dispatch(
        cx,
        model,
        Input::HistoryLoaded {
            conversation: cast.general,
            generation,
            result: Ok(HistoryPage {
                messages,
                has_more: false,
            }),
        },
    );
    answer_members(cx, model, cast, recorder);
}

/// Opens a dialog the way a menu command would.
fn open_dialog(cx: &mut HeadlessAppContext, window: &WindowHandle<RootView>, dialog: ShellDialog) {
    cx.update_window((*window).into(), |_, window_ref, app| {
        // The shell is created on the first draw after the session becomes ready.
        let _ = (window_ref, app);
    })
    .expect("window");
    draw(cx, window);
    window
        .update(cx, |root, window, app| {
            let shell = root
                .shell()
                .expect("the signed-in shell is showing")
                .clone();
            shell.update(app, |shell, cx| shell.open_dialog(dialog, window, cx));
        })
        .expect("open the dialog");
    draw(cx, window);
}

/// Opens the emoji picker the way the composer's button or a message's toolbar would.
fn open_picker(
    cx: &mut HeadlessAppContext,
    window: &WindowHandle<RootView>,
    purpose: PickerPurpose,
    anchor: gpui::Point<gpui::Pixels>,
) {
    draw(cx, window);
    window
        .update(cx, |root, window, app| {
            let shell = root.shell().expect("the shell is showing").clone();
            shell.update(app, |shell, cx| {
                shell.open_picker(purpose, anchor, window, cx)
            });
        })
        .expect("open the picker");
    draw(cx, window);
}

/// Records a handful of picks so the picker has a "Recently used" row.
fn use_some_emoji(cx: &mut HeadlessAppContext, model: &Entity<AppModel>) {
    for glyph in ["🙏", "😂", "🚀", "❤️", "👍", "🎉"] {
        dispatch(
            cx,
            model,
            Input::RecordEmojiUse(lazy_domain::ReactionEmoji::parse(glyph).unwrap()),
        );
    }
}

/// Answers the member lists the core has asked for, once each, the way the server would.
/// Bob is heads down and Zoë is away, for the scenes that show other people's statuses.
fn set_other_statuses(cast: &Cast) {
    use lazy_domain::ReactionEmoji;
    use lazy_domain::notification::UserStatus;
    *cast.others_status.borrow_mut() = vec![
        (
            "bob",
            UserStatus::new("Heads down until 3", ReactionEmoji::parse("🎯").ok(), None).unwrap(),
        ),
        (
            "zoe",
            UserStatus::new("On holiday", ReactionEmoji::parse("🌴").ok(), None).unwrap(),
        ),
    ];
}

/// Answers the newest directory request the recorder holds with one page of `channels`.
fn answer_browse(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    recorder: &Recorder,
    channels: Vec<BrowsedChannel>,
    has_more: bool,
) {
    let generation = recorder
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::LoadBrowse { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the directory was asked for");
    recorder
        .0
        .borrow_mut()
        .retain(|effect| !matches!(effect, Effect::LoadBrowse { .. }));
    dispatch(
        cx,
        model,
        Input::BrowseLoaded {
            generation,
            result: Ok(lazy_protocol::BrowseChannelsResponse { channels, has_more }),
        },
    );
}

// ---- Slice 3E: discovery ----

fn day(days: i64) -> Timestamp {
    Timestamp::from(time::OffsetDateTime::now_utc() - time::Duration::days(days))
}

/// Someone with no status, for the directory's long list.
fn extra_user(name: &str, display: &str) -> User {
    User {
        is_bot: false,
        deactivated: false,
        avatar_version: 0,
        status: None,
        id: UserId::generate(),
        username: lazy_domain::Username::parse(name).unwrap(),
        display_name: lazy_domain::DisplayName::parse(display).unwrap(),
    }
}

/// The directory's first page: the cast and a few more, with roles and join dates.
fn people_page(cast: &Cast) -> Vec<lazy_protocol::Person> {
    let person = |user: &User, role: WorkspaceRole, days: i64| lazy_protocol::Person {
        user: cast.with_avatar(user),
        role,
        joined_at: day(days),
    };
    vec![
        person(&cast.me, WorkspaceRole::Owner, 900),
        person(&cast.bob, WorkspaceRole::Admin, 640),
        person(&cast.carol, WorkspaceRole::Member, 410),
        person(&cast.dana, WorkspaceRole::Member, 300),
        person(&extra_user("eli", "Eli Brandt"), WorkspaceRole::Member, 220),
        person(
            &extra_user("fatima", "Fatima Al-Sayed"),
            WorkspaceRole::Member,
            180,
        ),
        person(
            &extra_user("gabriel", "Gabriel Pérez"),
            WorkspaceRole::Admin,
            95,
        ),
        person(
            &extra_user("hana", "Hana Kobayashi"),
            WorkspaceRole::Member,
            40,
        ),
        person(&cast.zoe, WorkspaceRole::Member, 12),
    ]
}

/// What the person saved: a channel message, a reply, a direct message, and one from a channel
/// they have left.
fn saved_items(cast: &Cast) -> Vec<lazy_protocol::SavedItem> {
    let design = cast.channel(cast.design, "design", "", true);
    let engineering = cast.channel(cast.engineering, "engineering", "", false);
    let dm = cast.people(cast.dm_bob, &[&cast.bob]);
    let left = cast.channel(ConversationId::generate(), "old-launch", "", false);
    let item =
        |conversation: &Conversation, message: Message, saved_minutes_ago: i64, cursor: u64| {
            lazy_protocol::SavedItem {
                message: Message {
                    conversation_id: conversation.id,
                    ..message
                },
                conversation: conversation.clone(),
                saved_at: Timestamp::from(
                    time::OffsetDateTime::now_utc() - time::Duration::minutes(saved_minutes_ago),
                ),
                cursor,
            }
        };
    let mut reply = cast.message(
        &cast.zoe,
        "I would. The second item is the data backfill and it is not close, so next week is the safe date.",
        30,
        60 * 5,
    );
    reply.thread_root = Some(MessageId::generate());
    vec![
        item(
            &design,
            cast.message(
                &cast.carol,
                "The launch checklist is in the wiki now: owners for each item, the rollback plan, and who is on call over the weekend. Please read it before Thursday's review and add anything missing.",
                10,
                60 * 3,
            ),
            12,
            50,
        ),
        item(&design, reply, 60 * 4, 40),
        item(
            &dm,
            cast.message(&cast.bob, "Dinner on Friday? 7:30 at the usual place.", 11, 60 * 30),
            60 * 24 * 2,
            30,
        ),
        item(
            &engineering,
            cast.message(
                &cast.me,
                "Rollout plan for the retry policy: ship behind a flag, then widen over a week, watching the failure dashboards and the queue depth the whole time.",
                12,
                60 * 24 * 6,
            ),
            60 * 24 * 5,
            20,
        ),
        item(
            &left,
            cast.message(&cast.dana, "Retro notes are here.", 13, 60 * 24 * 40),
            60 * 24 * 30,
            10,
        ),
    ]
}

/// Opens the saved messages and answers the first page with `items`.
fn saved_scene(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    result: Result<lazy_protocol::SavedPage, RequestError>,
) {
    draw(cx, win);
    window_shell_do(cx, win, |shell, window, cx| shell.open_saved(window, cx));
    let generation = rec
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::LoadSaved { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the saved messages asked for a page");
    dispatch(cx, model, Input::SavedLoaded { generation, result });
    draw(cx, win);
}

/// Opens the people directory and answers the first page.
fn people_scene(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    result: Result<lazy_protocol::PeoplePage, RequestError>,
) {
    draw(cx, win);
    window_shell_do(cx, win, |shell, window, cx| {
        shell.open_people_directory(window, cx)
    });
    answer_people(cx, model, rec, win, result);
}

fn answer_people(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    result: Result<lazy_protocol::PeoplePage, RequestError>,
) {
    let generation = rec
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::LoadPeople { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the directory asked for a page");
    dispatch(cx, model, Input::PeopleLoaded { generation, result });
    draw(cx, win);
}

/// Opens a profile and answers its request with `person`.
fn profile_scene(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    user: UserId,
    answer: Option<Result<lazy_protocol::PersonResponse, RequestError>>,
) {
    open_dialog(cx, win, ShellDialog::Profile(user));
    if let Some(result) = answer {
        let generation = rec
            .0
            .borrow()
            .iter()
            .rev()
            .find_map(|effect| match effect {
                Effect::LoadProfile { generation, .. } => Some(*generation),
                _ => None,
            })
            .expect("the profile was asked for");
        dispatch(cx, model, Input::ProfileLoaded { generation, result });
    }
    draw(cx, win);
}

/// Visits some conversations so the quick switcher has recents: the newest is the open one.
fn visit(cx: &mut HeadlessAppContext, model: &Entity<AppModel>, cast: &Cast, rec: &Recorder) {
    for id in [
        cast.engineering,
        cast.group,
        cast.design,
        cast.dm_bob,
        cast.general,
    ] {
        dispatch(cx, model, Input::SelectConversation(id));
        // The pretend server has nothing to say about the history of what is visited.
        rec.0.borrow_mut().retain(|effect| {
            !matches!(
                effect,
                Effect::FetchHistory { .. } | Effect::FetchMembers { .. }
            )
        });
    }
}

/// Puts the keyboard in the directory's search box and types `text`.
fn shift_focus_to_search_and_type(
    cx: &mut HeadlessAppContext,
    win: &WindowHandle<RootView>,
    text: &str,
) {
    type_text(cx, win, text);
}

/// What the diagnostics dialog needs: the desktop's facts, a signed-in app with two more
/// workspaces, and a refusal or two for the list of recent errors.
fn diagnostics_setup(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    rec: &Recorder,
) {
    cx.update(|app| {
        lazy_ui::dialogs::set_desktop_info(
            app,
            lazy_ui::dialogs::DesktopInfo::new(
                "0.1.0",
                "linux x86_64",
                "https://operator:not-shown@chat.example.com:8443/?token=hidden",
                "/home/alice/.local/share/lazy",
                "/home/alice/.local/share/lazy/logs",
            ),
        )
    });
    cast.more_workspaces.set(true);
    reach_chat(cx, model, cast, rec, true);
    for (kind, code) in [
        (ActionKind::UpdateWorkspace, ErrorCode::Forbidden),
        (ActionKind::AcceptInvitation, ErrorCode::NotFound),
    ] {
        dispatch(
            cx,
            model,
            Input::ActionResult {
                kind,
                result: Err(RequestError::Rejected(ApiError::new(code, "never shown"))),
            },
        );
    }
}

/// The runtime's answer to the latest check of a server's address.
fn answer_server(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    rec: &Recorder,
    result: Result<u32, String>,
) {
    let generation = rec
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::CheckServer { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the server row asked what the server is");
    dispatch(cx, model, Input::ServerChecked { generation, result });
}

/// A stored session whose server did not answer: the unreachable screen.
fn unreachable_session(cx: &mut HeadlessAppContext, model: &Entity<AppModel>, rec: &Recorder) {
    dispatch(
        cx,
        model,
        Input::Started(StartupState {
            session_restored: true,
            ..Default::default()
        }),
    );
    dispatch(
        cx,
        model,
        Input::BootstrapFailed(RequestError::Unknown("timed out".into())),
    );
    let _ = rec;
}

// ---- Slice 3F: workspace administration ----

fn minutes(count: i64) -> Timestamp {
    Timestamp::from(time::OffsetDateTime::now_utc() + time::Duration::minutes(count))
}

fn invite(
    cast: &Cast,
    by: &User,
    state: lazy_protocol::InvitationState,
    uses: u32,
    max: Option<u32>,
    created_minutes_ago: i64,
    expires_in_minutes: i64,
) -> lazy_protocol::Invitation {
    lazy_protocol::Invitation {
        id: lazy_domain::InvitationId::generate(),
        workspace_id: cast.workspace,
        created_by: by.id,
        created_at: minutes(-created_minutes_ago),
        expires_at: minutes(expires_in_minutes),
        max_uses: max,
        uses,
        state,
    }
}

/// A spread of invitations: active, used up, revoked, and expired, newest first.
fn invitations_list(cast: &Cast) -> Vec<lazy_protocol::Invitation> {
    use lazy_protocol::InvitationState::{Active, Expired, Revoked, UsedUp};
    vec![
        invite(cast, &cast.me, Active, 0, Some(5), 12, 60 * 24 * 7),
        invite(cast, &cast.bob, Active, 3, None, 60 * 5, 60 * 24 * 2),
        invite(cast, &cast.me, UsedUp, 1, Some(1), 60 * 24 * 2, 60 * 24 * 5),
        invite(cast, &cast.bob, Revoked, 0, Some(25), 60 * 24 * 6, 60 * 24),
        invite(
            cast,
            &cast.me,
            Expired,
            4,
            Some(100),
            60 * 24 * 12,
            -60 * 24 * 5,
        ),
    ]
}

/// The workspace's webhooks for the Integrations dialog: one posting fine, one failing, and one
/// that has never been used.
fn workspace_webhooks(cast: &Cast) -> Vec<lazy_protocol::WorkspaceWebhook> {
    let hook = |name: &str, channel: ConversationId, minutes_ago: i64| lazy_protocol::Webhook {
        id: lazy_domain::WebhookId::generate(),
        conversation_id: channel,
        name: DisplayName::parse(name).unwrap(),
        bot_user_id: UserId::generate(),
        created_by: cast.me.id,
        created_at: Timestamp::from(
            time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago),
        ),
    };
    let delivery = |outcome, minutes_ago: i64| lazy_protocol::WebhookDelivery {
        received_at: Timestamp::from(
            time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago),
        ),
        outcome,
        message_id: None,
        body_bytes: 212,
        idempotency_key: true,
    };
    vec![
        lazy_protocol::WorkspaceWebhook {
            webhook: hook("Deploy bot", cast.engineering, 60 * 24 * 12),
            channel_name: ChannelName::parse("engineering").unwrap(),
            last_delivery: Some(delivery(lazy_protocol::WebhookDeliveryOutcome::Posted, 4)),
            recent_failures: 0,
        },
        lazy_protocol::WorkspaceWebhook {
            webhook: hook("Uptime alerts", cast.general, 60 * 24 * 3),
            channel_name: ChannelName::parse("general").unwrap(),
            last_delivery: Some(delivery(lazy_protocol::WebhookDeliveryOutcome::Invalid, 37)),
            recent_failures: 6,
        },
        lazy_protocol::WorkspaceWebhook {
            webhook: hook("Release notes", cast.design, 90),
            channel_name: ChannelName::parse("design").unwrap(),
            last_delivery: None,
            recent_failures: 0,
        },
    ]
}

/// A delivery log with one of each kind of thing that happens.
fn webhook_log() -> Vec<lazy_protocol::WebhookDelivery> {
    use lazy_protocol::WebhookDeliveryOutcome as O;
    [
        (O::Invalid, 37, 18, false),
        (O::Posted, 52, 640, true),
        (O::Duplicate, 53, 640, true),
        (O::RateLimited, 70, 0, false),
        (O::TooLarge, 95, 70_000, false),
        (O::Archived, 60 * 5, 120, false),
        (O::Posted, 60 * 26, 1_900, true),
    ]
    .into_iter()
    .map(
        |(outcome, minutes_ago, body_bytes, idempotency_key)| lazy_protocol::WebhookDelivery {
            received_at: Timestamp::from(
                time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago),
            ),
            outcome,
            message_id: outcome.posted().then(MessageId::generate),
            body_bytes,
            idempotency_key,
        },
    )
    .collect()
}

/// Opens the Integrations dialog and answers its list with `list` (or leaves it loading).
fn integrations_scene(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    win: &WindowHandle<RootView>,
    list: Option<Vec<lazy_protocol::WorkspaceWebhook>>,
) {
    open_dialog(cx, win, ShellDialog::Integrations);
    if let Some(list) = list {
        dispatch(
            cx,
            model,
            Input::ActionResult {
                kind: ActionKind::LoadWorkspaceWebhooks(cast.workspace),
                result: Ok(ActionOutcome::WorkspaceWebhooks(list)),
            },
        );
    }
    draw(cx, win);
}

/// Runs `act` on the open Integrations dialog.
fn with_integrations(
    cx: &mut HeadlessAppContext,
    win: &WindowHandle<RootView>,
    act: impl FnOnce(
        &mut lazy_ui::dialogs::IntegrationsDialog,
        &mut Window,
        &mut gpui::Context<lazy_ui::dialogs::IntegrationsDialog>,
    ),
) {
    win.update(cx, |root, window, app| {
        let shell = root.shell().expect("the shell is showing").clone();
        let dialog = shell
            .read(app)
            .integrations_dialog()
            .expect("the integrations are open");
        dialog.update(app, |dialog, cx| act(dialog, window, cx));
    })
    .expect("the integrations dialog");
    draw(cx, win);
}

/// A GitHub status for the Integrations dialog: `stage` 0 is unavailable, 1 not connected,
/// 2 connected but not installed, 3 installed with subscriptions and deliveries.
fn github_status(cast: &Cast, stage: u8) -> lazy_protocol::GithubStatusResponse {
    use lazy_domain::github::GithubEvent as E;
    use lazy_protocol::GithubDeliveryState as S;
    let ago = |minutes: i64| {
        Timestamp::from(time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes))
    };
    let connection = (stage >= 2).then(|| lazy_protocol::GithubConnection {
        app_slug: "lazy-chat-lazyto-day".to_owned(),
        html_url: "https://github.com/apps/lazy-chat-lazyto-day".to_owned(),
        owner: "last-refuge".to_owned(),
        bot_user_id: cast.deploy.id,
        created_by: cast.me.id,
        created_at: ago(60 * 24),
    });
    let installed = stage >= 3;
    let delivery = |id: i64,
                    event: &str,
                    action: Option<&str>,
                    state,
                    attempts,
                    error: Option<&str>,
                    minutes| {
        lazy_protocol::GithubDelivery {
            id,
            event: event.to_owned(),
            action: action.map(str::to_owned),
            repository: Some("last-refuge/lazy".to_owned()),
            state,
            attempts,
            last_error: error.map(str::to_owned),
            received_at: ago(minutes),
        }
    };
    lazy_protocol::GithubStatusResponse {
        available: stage >= 1,
        connection,
        installations: if installed {
            vec![lazy_protocol::GithubInstallation {
                account: "last-refuge".to_owned(),
                all_repositories: true,
                suspended: false,
            }]
        } else {
            Vec::new()
        },
        subscriptions: if installed {
            vec![
                lazy_protocol::GithubSubscription {
                    id: lazy_domain::GithubSubscriptionId::generate(),
                    conversation_id: cast.engineering,
                    channel_name: ChannelName::parse("engineering").unwrap(),
                    repository: "last-refuge/lazy".to_owned(),
                    events: E::DEFAULT.to_vec(),
                    created_by: cast.me.id,
                    created_at: ago(60 * 20),
                },
                lazy_protocol::GithubSubscription {
                    id: lazy_domain::GithubSubscriptionId::generate(),
                    conversation_id: cast.general,
                    channel_name: ChannelName::parse("general").unwrap(),
                    repository: "last-refuge/lazy".to_owned(),
                    events: vec![E::Releases],
                    created_by: cast.me.id,
                    created_at: ago(60 * 19),
                },
            ]
        } else {
            Vec::new()
        },
        deliveries: if installed {
            vec![
                delivery(9, "pull_request", Some("opened"), S::Posted, 1, None, 3),
                delivery(
                    8,
                    "push",
                    None,
                    S::Queued,
                    2,
                    Some("the database failed (pool timed out)"),
                    9,
                ),
                delivery(
                    7,
                    "issues",
                    Some("closed"),
                    S::Failed,
                    6,
                    Some("the database failed (pool timed out)"),
                    52,
                ),
                delivery(6, "release", Some("published"), S::Posted, 1, None, 60 * 5),
            ]
        } else {
            Vec::new()
        },
    }
}

/// Opens Integrations with GitHub at `stage`, and (if `panel`) its GitHub view.
fn github_scene(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    win: &WindowHandle<RootView>,
    stage: u8,
    panel: bool,
) {
    integrations_scene(cx, model, cast, win, Some(workspace_webhooks(cast)));
    let answer = |cx: &mut HeadlessAppContext| {
        dispatch(
            cx,
            model,
            Input::ActionResult {
                kind: ActionKind::LoadGithub(cast.workspace),
                result: Ok(ActionOutcome::Github(Box::new(github_status(cast, stage)))),
            },
        );
    };
    answer(cx);
    if panel {
        with_integrations(cx, win, |dialog, window, cx| dialog.open_github(window, cx));
        answer(cx);
    }
    draw(cx, win);
}

/// A webhook connector's status: `stage` 1 is not connected, 2 connected without a secret, 3 set
/// up with subscriptions and deliveries.
fn connector_status(
    cast: &Cast,
    provider: lazy_domain::connector::ConnectorProvider,
    stage: u8,
) -> lazy_protocol::ConnectorStatusResponse {
    use lazy_protocol::GithubDeliveryState as S;
    let ago = |minutes: i64| {
        Timestamp::from(time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes))
    };
    let (scope, other) = match provider {
        lazy_domain::connector::ConnectorProvider::Jira => ("WEB", "APP"),
        _ => ("ENG", "OPS"),
    };
    let ready = stage >= 3;
    let delivery = |id: i64, summary: String, state, attempts, error: Option<&str>, minutes| {
        lazy_protocol::ConnectorDelivery {
            id,
            summary,
            scope: scope.to_owned(),
            state,
            attempts,
            last_error: error.map(str::to_owned),
            received_at: ago(minutes),
        }
    };
    lazy_protocol::ConnectorStatusResponse {
        provider,
        available: true,
        connection: (stage >= 2).then(|| lazy_protocol::ConnectorConnection {
            webhook_url: format!(
                "https://chat.lazyto.day/connectors/{}/5f0c2a7e-91d3-4b8a-a6e2-0c4d9b7f1e23",
                provider.as_str()
            ),
            secret_set: ready,
            bot_user_id: cast.deploy.id,
            created_by: cast.me.id,
            created_at: ago(60 * 24),
        }),
        subscriptions: if ready {
            vec![
                lazy_protocol::ConnectorSubscription {
                    id: lazy_domain::ConnectorSubscriptionId::generate(),
                    conversation_id: cast.engineering,
                    channel_name: ChannelName::parse("engineering").unwrap(),
                    scope: scope.to_owned(),
                    events: provider
                        .default_kinds()
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    created_by: cast.me.id,
                    created_at: ago(60 * 20),
                },
                lazy_protocol::ConnectorSubscription {
                    id: lazy_domain::ConnectorSubscriptionId::generate(),
                    conversation_id: cast.general,
                    channel_name: ChannelName::parse("general").unwrap(),
                    scope: String::new(),
                    events: vec!["issue_created".to_owned()],
                    created_by: cast.me.id,
                    created_at: ago(60 * 19),
                },
            ]
        } else {
            Vec::new()
        },
        deliveries: if ready {
            vec![
                delivery(
                    14,
                    format!("{scope}-12 status change"),
                    S::Posted,
                    1,
                    None,
                    4,
                ),
                delivery(13, format!("{other}-3 created"), S::Posted, 1, None, 11),
                delivery(
                    12,
                    format!("{scope}-9 comment"),
                    S::Failed,
                    6,
                    Some("the database failed (pool timed out)"),
                    48,
                ),
            ]
        } else {
            Vec::new()
        },
    }
}

/// Opens Integrations with each connector at its stage (and GitHub installed), and the view of
/// `open`, if any. A `made` secret is shown as just made for that connector.
fn connector_scene(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    win: &WindowHandle<RootView>,
    stages: &[(lazy_domain::connector::ConnectorProvider, u8)],
    open: Option<lazy_domain::connector::ConnectorProvider>,
    made: Option<&str>,
) {
    integrations_scene(cx, model, cast, win, Some(workspace_webhooks(cast)));
    let answer = |cx: &mut HeadlessAppContext| {
        dispatch(
            cx,
            model,
            Input::ActionResult {
                kind: ActionKind::LoadGithub(cast.workspace),
                result: Ok(ActionOutcome::Github(Box::new(github_status(cast, 3)))),
            },
        );
        for (provider, stage) in stages {
            dispatch(
                cx,
                model,
                Input::ActionResult {
                    kind: ActionKind::LoadConnector(cast.workspace, *provider),
                    result: Ok(ActionOutcome::Connector(Box::new(connector_status(
                        cast, *provider, *stage,
                    )))),
                },
            );
        }
    };
    answer(cx);
    if let Some(provider) = open {
        with_integrations(cx, win, |dialog, window, cx| {
            dialog.open_connector(provider, window, cx)
        });
        answer(cx);
        if let Some(secret) = made {
            dispatch(
                cx,
                model,
                Input::GenerateConnectorSecret {
                    workspace: cast.workspace,
                    provider,
                },
            );
            dispatch(
                cx,
                model,
                Input::ActionResult {
                    kind: ActionKind::SetConnectorSecret(cast.workspace, provider),
                    result: Ok(ActionOutcome::ConnectorSecret(
                        lazy_client_core::SecretUrl::new(secret.to_owned()),
                    )),
                },
            );
        }
    }
    draw(cx, win);
}

/// The sample apps' ids, the same in every call.
fn sample_app_ids() -> (lazy_domain::AppId, lazy_domain::AppId) {
    static IDS: std::sync::OnceLock<(lazy_domain::AppId, lazy_domain::AppId)> =
        std::sync::OnceLock::new();
    *IDS.get_or_init(|| {
        (
            lazy_domain::AppId::generate(),
            lazy_domain::AppId::generate(),
        )
    })
}

/// Two apps: one in #engineering, one in no channel.
fn sample_apps(cast: &Cast) -> Vec<lazy_protocol::App> {
    use lazy_domain::app::AppScope;
    let ago = |minutes: i64| {
        Timestamp::from(time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes))
    };
    vec![
        lazy_protocol::App {
            id: sample_app_ids().0,
            name: "Deploy bot".into(),
            bot_user_id: cast.deploy.id,
            scopes: AppScope::DEFAULT.to_vec(),
            token_hint: "9c3e".into(),
            channels: vec![lazy_protocol::AppChannel {
                conversation_id: cast.engineering,
                name: ChannelName::parse("engineering").unwrap(),
            }],
            created_by: cast.me.id,
            created_at: ago(60 * 24 * 3),
            last_used_at: Some(ago(12)),
            commands: vec![lazy_protocol::AppCommand {
                id: lazy_domain::AppCommandId::generate(),
                name: "deploy".into(),
                url: "https://deploy.lazyto.day/slack/commands".into(),
                description: "Deploy a service".into(),
                usage_hint: "[service] [environment]".into(),
            }],
            events: Some(lazy_protocol::AppEvents {
                url: "https://deploy.lazyto.day/slack/events".into(),
                types: vec!["app_mention".into()],
            }),
            interactivity_url: Some("https://deploy.lazyto.day/slack/interactive".into()),
        },
        lazy_protocol::App {
            id: sample_app_ids().1,
            name: "Standup reminder".into(),
            bot_user_id: cast.deploy.id,
            scopes: vec![AppScope::ChatWrite],
            token_hint: "41b0".into(),
            channels: Vec::new(),
            created_by: cast.me.id,
            created_at: ago(60 * 5),
            last_used_at: None,
            commands: Vec::new(),
            events: None,
            interactivity_url: None,
        },
    ]
}

/// Opens Integrations and its apps view. `stage` 0 shows the list, 1 the first app just made with
/// its token, 2 making an app.
fn apps_scene(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    win: &WindowHandle<RootView>,
    stage: u8,
) {
    integrations_scene(cx, model, cast, win, Some(workspace_webhooks(cast)));
    let answer = |cx: &mut HeadlessAppContext| {
        dispatch(
            cx,
            model,
            Input::ActionResult {
                kind: ActionKind::LoadApps(cast.workspace),
                result: Ok(ActionOutcome::Apps(sample_apps(cast))),
            },
        );
    };
    answer(cx);
    with_integrations(cx, win, |dialog, window, cx| dialog.open_apps(window, cx));
    answer(cx);
    match stage {
        1 => {
            let app = sample_apps(cast).remove(0);
            let id = app.id;
            dispatch(cx, model, Input::NewAppToken(id));
            dispatch(
                cx,
                model,
                Input::ActionResult {
                    kind: ActionKind::NewAppToken(id),
                    result: Ok(ActionOutcome::AppToken(
                        Box::new(app),
                        lazy_client_core::SecretUrl::new(
                            "lzb_5e2a9d0c71f3b84e6a1c0d9f2b7e3a5c8d1f4b6e9a0c2d5f7b3e1a8c4d6f9c3e"
                                .to_owned(),
                        ),
                    )),
                },
            );
            with_integrations(cx, win, |dialog, window, cx| {
                if let Some(panel) = dialog.apps_panel() {
                    panel.update(cx, |panel, cx| panel.show_details(id, window, cx));
                }
            });
        }
        2 => with_integrations(cx, win, |dialog, window, cx| {
            if let Some(panel) = dialog.apps_panel() {
                panel.update(cx, |panel, cx| panel.start_making(window, cx));
            }
        }),
        _ => {}
    }
    draw(cx, win);
}

/// Runs `/deploy web prod` in the open channel and answers it with a private reply.
/// People's links with the previews the server found for them (D41).
fn link_previews_scene(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    recorder: &Recorder,
    win: &WindowHandle<RootView>,
) {
    let history = cast.link_history();
    let ids: Vec<MessageId> = history.iter().map(|m| m.id).collect();
    reach_chat_with(cx, model, cast, recorder, history);
    let preview = |url: &str, title: &str, description: Option<&str>, site: &str| {
        lazy_protocol::LinkPreview {
            url: url.into(),
            title: Some(title.into()),
            description: description.map(Into::into),
            site_name: Some(site.into()),
        }
    };
    let found = [
        vec![preview(
            "https://miro.com/app/board/uXjVN3q4Ab8=/",
            "Q4 launch plan",
            Some("Timeline, owners, and risks for the Q4 launch. Shared with the design team."),
            "Miro",
        )],
        vec![
            preview(
                "https://github.com/lazy/rfcs/pull/41",
                "RFC 41: Rich messages and buttons by alice-n · Pull Request #41 · lazy/rfcs",
                Some(
                    "Integrations can post a header, sections with fields, and buttons that \
                     tell the app they were pressed, checked on every press.",
                ),
                "GitHub",
            ),
            preview(
                "https://www.rust-lang.org/",
                "Rust Programming Language",
                Some("A language empowering everyone to build reliable and efficient software."),
                "rust-lang.org",
            ),
        ],
    ];
    for (message, previews) in ids.into_iter().zip(found) {
        dispatch(
            cx,
            model,
            Input::LoadPreviews {
                message,
                revision: 1,
            },
        );
        dispatch(
            cx,
            model,
            Input::PreviewsLoaded {
                message,
                revision: 1,
                result: Ok(previews),
            },
        );
    }
    draw(cx, win);
}

fn command_reply_scene(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    win: &WindowHandle<RootView>,
) {
    dispatch(
        cx,
        model,
        Input::SetDraft {
            conversation: cast.general,
            text: "/deploy web prod".into(),
        },
    );
    dispatch(
        cx,
        model,
        Input::ActionResult {
            kind: ActionKind::LoadCommands(cast.workspace),
            result: Ok(ActionOutcome::Commands(vec![
                lazy_protocol::WorkspaceCommand {
                    name: "deploy".into(),
                    description: "Deploy a service".into(),
                    usage_hint: "[service] [environment]".into(),
                    app_name: "Deploy bot".into(),
                },
            ])),
        },
    );
    dispatch(
        cx,
        model,
        Input::SendMessage {
            conversation: cast.general,
            text: "/deploy web prod".into(),
        },
    );
    let reply = lazy_domain::MessageDocument::from_markdown(
        "Deploying *web* to *prod*: build 4821 is queued. [Follow the run](https://ci.example/runs/4821)",
    )
    .unwrap();
    dispatch(
        cx,
        model,
        Input::ActionResult {
            kind: ActionKind::RunCommand(cast.general),
            result: Ok(ActionOutcome::CommandRan(Box::new(
                lazy_protocol::RunCommandResponse {
                    app_name: "Deploy bot".into(),
                    reply: Some(reply),
                    posted: None,
                },
            ))),
        },
    );
    draw(cx, win);
}

/// Opens the invitations dialog and answers its request with `result`.
fn invitations_scene(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    result: Option<Result<lazy_protocol::InvitationList, RequestError>>,
) {
    open_dialog(cx, win, ShellDialog::Invitations);
    if let Some(result) = result {
        let generation = rec
            .0
            .borrow()
            .iter()
            .rev()
            .find_map(|effect| match effect {
                Effect::LoadInvitations { generation, .. } => Some(*generation),
                _ => None,
            })
            .expect("the invitations were asked for");
        dispatch(cx, model, Input::InvitationsLoaded { generation, result });
    }
    draw(cx, win);
}

/// The people page with some people deactivated.
fn people_with_deactivated(cast: &Cast) -> Vec<lazy_protocol::Person> {
    let mut people = people_page(cast);
    for person in &mut people {
        if matches!(person.user.username.as_str(), "carol" | "hana") {
            person.user.deactivated = true;
        }
    }
    people
}

/// The server stops listing the workspace: the person was deactivated.
fn lose_access(cx: &mut HeadlessAppContext, model: &Entity<AppModel>, cast: &Cast) {
    let mut bootstrap = cast.bootstrap();
    bootstrap.workspaces.clear();
    dispatch(cx, model, Input::BootstrapLoaded(Box::new(bootstrap)));
}

fn answer_members(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    recorder: &Recorder,
) {
    let asked: Vec<(ConversationId, u64)> = recorder
        .0
        .borrow()
        .iter()
        .filter_map(|effect| match effect {
            Effect::FetchMembers {
                conversation,
                generation,
            } => Some((*conversation, *generation)),
            _ => None,
        })
        .collect();
    recorder
        .0
        .borrow_mut()
        .retain(|effect| !matches!(effect, Effect::FetchMembers { .. }));
    for (conversation, generation) in asked {
        dispatch(
            cx,
            model,
            Input::MembersFetched {
                conversation,
                generation,
                result: Ok(ConversationMembersResponse {
                    conversation_id: conversation,
                    user_ids: cast.members_of(conversation),
                }),
            },
        );
    }
}

/// Opens the thread of `root` and answers the request for its replies, the way the runtime does.
fn open_thread(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    window: &WindowHandle<RootView>,
    recorder: &Recorder,
    cast: &Cast,
    root: &Message,
    replies: Vec<Message>,
) {
    dispatch(
        cx,
        model,
        Input::OpenThread {
            conversation: cast.general,
            root: root.id,
        },
    );
    let generation = recorder
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::FetchReplies {
                root: r,
                generation,
                ..
            } if *r == root.id => Some(*generation),
            _ => None,
        })
        .expect("opening the thread asks for its replies");
    dispatch(
        cx,
        model,
        Input::RepliesLoaded {
            root: root.id,
            generation,
            result: Ok(HistoryPage {
                messages: replies,
                has_more: false,
            }),
        },
    );
    draw(cx, window);
}

fn draw(cx: &mut HeadlessAppContext, window: &WindowHandle<RootView>) {
    for _ in 0..3 {
        cx.update_window((*window).into(), |_, w, app| w.draw(app).clear(app))
            .expect("draw");
        cx.run_until_parked();
    }
}

/// Presses keys, one keystroke per space-separated word, like a person at the keyboard.
fn press(cx: &mut HeadlessAppContext, window: &WindowHandle<RootView>, keys: &str) {
    let os = cx.update(|app| app.platform_info().os);
    for key in keys.split(' ') {
        let keystroke = Keystroke::parse_with_os(key, os).expect("a valid keystroke");
        cx.update_window((*window).into(), |_, w, app| {
            w.dispatch_keystroke(keystroke, app)
        })
        .expect("dispatch key");
        draw(cx, window);
    }
}

fn type_text(cx: &mut HeadlessAppContext, window: &WindowHandle<RootView>, text: &str) {
    for ch in text.chars() {
        press(
            cx,
            window,
            &if ch == ' ' {
                "space".to_owned()
            } else {
                ch.to_string()
            },
        );
    }
}

/// Answers the newest history request for `conversation` with `messages`.
fn answer_history(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    recorder: &Recorder,
    conversation: ConversationId,
    messages: Vec<Message>,
) {
    let generation = recorder
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::FetchHistory {
                conversation: c,
                generation,
                ..
            } if *c == conversation => Some(*generation),
            _ => None,
        })
        .expect("the core asked for that history");
    dispatch(
        cx,
        model,
        Input::HistoryLoaded {
            conversation,
            generation,
            result: Ok(HistoryPage {
                messages,
                has_more: false,
            }),
        },
    );
}

/// What [`reach_chat_custom`] should make of the first page of history and the list of people.
struct Chat {
    /// The first page, or the reason it failed.
    history: Result<Vec<Message>, RequestError>,
    /// Whether older messages exist.
    has_more: bool,
    /// Whether the channel's people have arrived.
    answer_people: bool,
    /// Storage problems found at startup, which show as a notice.
    problems: Vec<String>,
}

/// Like [`reach_chat_with`], with control over what the first page of history and the list of
/// people do: `history` is the first page (or the reason it failed), `has_more` says whether
/// older messages exist, and `answer_people` says whether the channel's people have arrived.
fn reach_chat_custom(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    recorder: &Recorder,
    chat: Chat,
) {
    let Chat {
        history,
        has_more,
        answer_people,
        problems,
    } = chat;

    dispatch(
        cx,
        model,
        Input::Started(StartupState {
            session_restored: true,
            problems,
            recent_searches: cast.recent_searches.borrow().clone(),
            ..Default::default()
        }),
    );
    dispatch(
        cx,
        model,
        Input::BootstrapLoaded(Box::new(cast.bootstrap())),
    );
    dispatch(cx, model, Input::Clock(Timestamp::now()));
    dispatch(
        cx,
        model,
        Input::Socket(SocketEvent::Opened {
            workspace: cast.workspace,
        }),
    );
    dispatch(
        cx,
        model,
        Input::Frame(ServerFrame::CaughtUp {
            workspace_id: cast.workspace,
            seq: EventSeq::ZERO,
        }),
    );
    let generation = recorder
        .0
        .borrow()
        .iter()
        .find_map(|effect| match effect {
            Effect::FetchHistory { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("loading #general fetches its history");
    dispatch(
        cx,
        model,
        Input::HistoryLoaded {
            conversation: cast.general,
            generation,
            result: history.map(|messages| HistoryPage { messages, has_more }),
        },
    );
    if answer_people {
        answer_members(cx, model, cast, recorder);
    }
}

/// Selects text in the main timeline (`pane` false) or the thread pane's, from `anchor` to
/// `head`: each a message's row and a byte offset into its selectable text.
fn select_text(
    cx: &mut HeadlessAppContext,
    window: &WindowHandle<RootView>,
    pane: bool,
    anchor: (usize, usize),
    head: (usize, usize),
) {
    draw(cx, window);
    window
        .update(cx, |root, _window, app| {
            let shell = root.shell().expect("the shell is showing").clone();
            let timeline = if pane {
                shell.read(app).thread_pane().read(app).timeline().clone()
            } else {
                shell.read(app).timeline().clone()
            };
            timeline.update(app, |timeline, cx| timeline.select_range(anchor, head, cx));
        })
        .expect("select text");
    draw(cx, window);
}

/// Opens #design's details with `webhooks` already listed, the way the dialog does on its own.
fn open_integrations(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    window: &WindowHandle<RootView>,
    recorder: &Recorder,
    cast: &Cast,
    webhooks: Vec<lazy_protocol::Webhook>,
) {
    reach_chat(cx, model, cast, recorder, true);
    dispatch(cx, model, Input::SelectConversation(cast.design));
    answer_history(cx, model, recorder, cast.design, vec![]);
    open_dialog(cx, window, ShellDialog::Details(cast.design));
    dispatch(
        cx,
        model,
        Input::ActionResult {
            kind: ActionKind::LoadWebhooks(cast.design),
            result: Ok(ActionOutcome::Webhooks(webhooks)),
        },
    );
    draw(cx, window);
}

/// Moves from where the details dialog opens (the topic) to "Add incoming webhook": the name,
/// then backwards around the ends through Save and Close.
fn focus_add_webhook(cx: &mut HeadlessAppContext, window: &WindowHandle<RootView>) {
    press(cx, window, "shift-tab shift-tab shift-tab shift-tab");
}

/// The address the server hands back once, as a made-up secret.
const SHOWN_ADDRESS: &str = "http://127.0.0.1:8080/hooks/9f2c1d3e-6a5b-4c7d-8e90-1a2b3c4d5e6f/Zk3bQ8vRtYw0xN2mLpUo7sHcEaJdFgIi";

/// Types a name into the new-webhook form and answers the creation the way the server would.
fn create_webhook(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    window: &WindowHandle<RootView>,
    cast: &Cast,
    name: &str,
) {
    focus_add_webhook(cx, window);
    press(cx, window, "enter");
    type_text(cx, window, name);
    press(cx, window, "enter");
    let webhook = cast.webhooks(cast.design, &[name]).remove(0);
    dispatch(
        cx,
        model,
        Input::ActionResult {
            kind: ActionKind::CreateWebhook(cast.design),
            result: Ok(ActionOutcome::WebhookCreated(
                lazy_client_core::CreatedWebhook {
                    webhook,
                    url: lazy_client_core::SecretUrl::new(SHOWN_ADDRESS.to_owned()),
                },
            )),
        },
    );
    draw(cx, window);
}

// ---- Custom emoji (slice 3B) ------------------------------------------------------------------------

/// A custom emoji's picture: a simple shape in `hue`, `width` by `height`, with transparent
/// corners (as a real emoji has) so how it sits on the text can be judged.
fn emoji_png(shape: u8, hue: f32, width: u32, height: u32) -> Vec<u8> {
    let mut picture = image::RgbaImage::new(width, height);
    for (x, y, pixel) in picture.enumerate_pixels_mut() {
        let (mut r, mut g, mut b, mut covered) = (0u32, 0u32, 0u32, 0u32);
        for sy in 0..2 {
            for sx in 0..2 {
                // -1 to 1 across the picture; v grows downward.
                let u = (x as f32 + 0.25 + 0.5 * sx as f32) / width as f32 * 2.0 - 1.0;
                let v = (y as f32 + 0.25 + 0.5 * sy as f32) / height as f32 * 2.0 - 1.0;
                if let Some((pr, pg, pb)) = emoji_shade(shape, hue, u, v) {
                    r += u32::from(pr);
                    g += u32::from(pg);
                    b += u32::from(pb);
                    covered += 1;
                }
            }
        }
        *pixel = match (
            r.checked_div(covered),
            g.checked_div(covered),
            b.checked_div(covered),
        ) {
            (Some(r), Some(g), Some(b)) => {
                image::Rgba([r as u8, g as u8, b as u8, (covered * 255 / 4) as u8])
            }
            _ => image::Rgba([0, 0, 0, 0]),
        };
    }
    let mut out = std::io::Cursor::new(Vec::new());
    picture
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode");
    out.into_inner()
}

/// The color at (`u`, `v`) of an emoji shape, or `None` where it is transparent.
fn emoji_shade(shape: u8, hue: f32, u: f32, v: f32) -> Option<(u8, u8, u8)> {
    let body = hsv(hue, 0.75, 0.95);
    let dark = hsv(hue, 0.8, 0.30);
    let light = (255, 250, 235);
    // Signed distance to a rounded rectangle with half sizes (hx, hy) and corner radius r.
    let rounded = |hx: f32, hy: f32, r: f32| {
        let (qx, qy) = (u.abs() - hx + r, v.abs() - hy + r);
        (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
    };
    match shape {
        // A round face.
        0 => {
            let d = (u * u + v * v).sqrt();
            if d > 0.94 {
                return None;
            }
            if d > 0.80 {
                return Some(dark);
            }
            for ex in [-0.34, 0.34] {
                if ((u - ex).powi(2) + (v + 0.2).powi(2)).sqrt() < 0.12 {
                    return Some(dark);
                }
            }
            let mouth = (u * u + (v - 0.05).powi(2)).sqrt();
            if v > 0.2 && (mouth - 0.5).abs() < 0.07 {
                return Some(dark);
            }
            Some(body)
        }
        // A five pointed star.
        1 => {
            let points: Vec<(f32, f32)> = (0..10)
                .map(|i| {
                    let angle =
                        -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
                    let radius = if i % 2 == 0 { 0.97 } else { 0.42 };
                    (radius * angle.cos(), radius * angle.sin())
                })
                .collect();
            let mut inside = false;
            for i in 0..points.len() {
                let (a, b) = (points[i], points[(i + 1) % points.len()]);
                if (a.1 > v) != (b.1 > v) && u < (b.0 - a.0) * (v - a.1) / (b.1 - a.1) + a.0 {
                    inside = !inside;
                }
            }
            inside.then(|| {
                if (u * u + v * v).sqrt() < 0.2 {
                    light
                } else {
                    body
                }
            })
        }
        // A heart.
        2 => {
            let (x, y) = (u * 1.25, -v * 1.25 + 0.1);
            let f = (x * x + y * y - 1.0).powi(3) - x * x * y.powi(3);
            (f <= 0.0).then(|| if x < -0.35 && y > 0.35 { light } else { body })
        }
        // A wide banner with stripes.
        3 => {
            let d = rounded(0.96, 0.78, 0.3);
            if d > 0.0 {
                return None;
            }
            if d > -0.12 {
                return Some(dark);
            }
            let band = (((v + 0.78) / 1.56) * 4.0).floor() as i32;
            Some(if band % 2 == 0 { body } else { light })
        }
        // A tall tower with windows.
        4 => {
            let d = rounded(0.78, 0.96, 0.25);
            if d > 0.0 {
                return None;
            }
            if d > -0.14 {
                return Some(dark);
            }
            let window = [-0.4f32, 0.0, 0.4].iter().any(|&wx| {
                [-0.6f32, -0.2, 0.2, 0.6]
                    .iter()
                    .any(|&wy| (u - wx).abs() < 0.11 && (v - wy).abs() < 0.11)
            });
            Some(if window { light } else { body })
        }
        // A round badge with a check mark.
        _ => {
            let d = (u * u + v * v).sqrt();
            if d > 0.94 {
                return None;
            }
            if d > 0.80 {
                return Some(dark);
            }
            let near = |a: (f32, f32), b: (f32, f32)| {
                let (abx, aby) = (b.0 - a.0, b.1 - a.1);
                let t =
                    (((u - a.0) * abx + (v - a.1) * aby) / (abx * abx + aby * aby)).clamp(0.0, 1.0);
                ((u - a.0 - t * abx).powi(2) + (v - a.1 - t * aby).powi(2)).sqrt()
            };
            let check = near((-0.4, 0.0), (-0.1, 0.32)).min(near((-0.1, 0.32), (0.45, -0.3)));
            Some(if check < 0.09 { light } else { body })
        }
    }
}

/// A person's picture: a soft gradient in a hue of their own with a head-and-shoulders figure, as
/// the server would hand back after cropping an upload to a square.
fn avatar_png(seed: u128) -> Vec<u8> {
    let hue = (seed % 360) as f32 / 360.0;
    let edge = 256u32;
    let mut picture = image::RgbaImage::new(edge, edge);
    let (top, bottom) = (hsv(hue, 0.35, 1.0), hsv(hue, 0.65, 0.78));
    let figure = hsv(hue, 0.55, 0.30);
    for (x, y, pixel) in picture.enumerate_pixels_mut() {
        let (u, v) = (
            x as f32 / edge as f32 * 2.0 - 1.0,
            y as f32 / edge as f32 * 2.0 - 1.0,
        );
        let t = (v + 1.0) / 2.0;
        let mix = |a: u8, b: u8| (f32::from(a) * (1.0 - t) + f32::from(b) * t) as u8;
        let head = (u * u + (v + 0.25) * (v + 0.25)) < 0.12;
        let shoulders = v > 0.25 && (u * u) / 0.64 + ((v - 0.95) * (v - 0.95)) / 0.45 < 1.0;
        *pixel = if head || shoulders {
            image::Rgba([figure.0, figure.1, figure.2, 255])
        } else {
            image::Rgba([
                mix(top.0, bottom.0),
                mix(top.1, bottom.1),
                mix(top.2, bottom.2),
                255,
            ])
        };
    }
    let mut out = std::io::Cursor::new(Vec::new());
    picture
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode");
    out.into_inner()
}

/// Answers every request for a person's picture the way the server would.
fn answer_avatars(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    recorder: &Recorder,
    window: &WindowHandle<RootView>,
) {
    loop {
        let asked: Vec<(UserId, u32)> = {
            let mut effects = recorder.0.borrow_mut();
            let asked = effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::FetchAvatar { user, version } => Some((*user, *version)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            effects.retain(|effect| !matches!(effect, Effect::FetchAvatar { .. }));
            asked
        };
        if asked.is_empty() {
            break;
        }
        for (user, version) in asked {
            let result = Ok(lazy_client_core::ImageData::new(avatar_png(
                user.as_uuid().as_u128(),
            )));
            dispatch(
                cx,
                model,
                Input::AvatarLoaded {
                    user,
                    version,
                    result,
                },
            );
        }
        draw(cx, window);
    }
}

/// Answers every request for a custom emoji's picture the way the server would: the picture, or
/// (for the emoji in `cast.failing_emoji`) a failure.
fn answer_emoji_images(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    recorder: &Recorder,
    window: &WindowHandle<RootView>,
) {
    loop {
        let asked: Vec<EmojiId> = {
            let mut effects = recorder.0.borrow_mut();
            let asked = effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::FetchEmojiImage { emoji } => Some(*emoji),
                    _ => None,
                })
                .collect::<Vec<_>>();
            effects.retain(|effect| !matches!(effect, Effect::FetchEmojiImage { .. }));
            asked
        };
        if asked.is_empty() {
            break;
        }
        for id in asked {
            let result = if cast.failing_emoji.borrow().contains(&id) {
                Err(lazy_client_core::TransferError::Unreadable(
                    "the server did not answer".into(),
                ))
            } else {
                let (shape, hue, w, h) = cast.emoji_art[&id];
                Ok(lazy_client_core::ImageData::new(emoji_png(
                    shape, hue, w, h,
                )))
            };
            dispatch(cx, model, Input::EmojiImageLoaded { emoji: id, result });
        }
        draw(cx, window);
    }
}

/// Messages that use custom emoji every way the text can: alone, several in a row, in bold and
/// italics, in a quote and a list, wide and tall pictures among words, a deleted emoji, and one
/// nobody here has heard of.
fn emoji_history(cast: &Cast) -> Vec<Message> {
    let mut unknown = cast.with_emoji(
        &cast.zoe,
        "A deleted one, :old_logo:, reads as its name; so does one from nowhere:",
        5,
        20,
    );
    let mut blocks = unknown.document.blocks().to_vec();
    if let Block::Paragraph { inlines } = &mut blocks[0] {
        inlines.push(Inline::text(" "));
        inlines.push(Inline::emoji(EmojiId::generate(), "from_nowhere"));
    }
    unknown.document = MessageDocument::new(blocks).expect("valid");
    vec![
        cast.with_emoji(&cast.bob, "Deploy is out :shipit: thanks everyone!", 1, 50),
        cast.with_emoji(
            &cast.zoe,
            ":partyparrot::partyparrot::partyparrot: three in a row, then **bold :blobheart: text**, \
             and *italic :shipit:*, and ~~struck :lgtm:~~",
            2,
            40,
        ),
        cast.with_emoji(
            &cast.carol,
            "Mixed shapes: :wide_logo: :tall_tower: :lgtm: :blobheart: inline with a long run of \
             ordinary words, so the line wraps around at narrower widths and the pictures can be \
             seen sitting on the text :shipit: until the very end :partyparrot:",
            3,
            30,
        ),
        cast.with_emoji(
            &cast.bob,
            "> quoted :shipit: from before\n\n- a list item :lgtm:\n- another with a link: \
             [the plan :rollout:](https://example.com/plan)",
            4,
            25,
        ),
        unknown,
        cast.with_emoji(&cast.carol, ":shipit:", 6, 10),
        cast.with_emoji(&cast.me, "Nice! :blobheart: back to you all", 7, 4),
    ]
}

/// The inline emoji scenes: `ready` answers the pictures; `failing` names the ones that fail.
fn inline_emoji_scene(
    model: &Entity<AppModel>,
    cast: &Cast,
    cx: &mut HeadlessAppContext,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    ready: bool,
    failing: &[&str],
) {
    for name in failing {
        cast.failing_emoji.borrow_mut().insert(cast.emoji(name).id);
    }
    reach_chat_with(cx, model, cast, rec, emoji_history(cast));
    draw(cx, win);
    if ready {
        answer_emoji_images(cx, model, cast, rec, win);
    }
    draw(cx, win);
}

/// Messages with custom reactions, among standard ones.
fn emoji_reactions_history(cast: &Cast) -> Vec<Message> {
    vec![
        cast.reacted_emoji(
            cast.with_emoji(
                &cast.bob,
                "Release notes are up. Please look them over.",
                1,
                30,
            ),
            &[
                (cast.custom("shipit"), 4, true),
                (lazy_domain::ReactionEmoji::parse("🎉").unwrap(), 3, false),
                (cast.custom("partyparrot"), 2, false),
                (cast.custom("blobheart"), 1, true),
                (cast.custom("old_logo"), 2, true),
                (cast.custom("wide_logo"), 1, false),
            ],
        ),
        cast.reacted_emoji(
            cast.with_emoji(
                &cast.zoe,
                "The old logo is retired :old_logo: for good.",
                2,
                20,
            ),
            &[
                (cast.custom("old_logo"), 5, false),
                (cast.custom("lgtm"), 12, true),
            ],
        ),
    ]
}

/// The open custom emoji dialog (admins and owners), its pictures delivered.
fn emoji_dialog_scene(
    model: &Entity<AppModel>,
    cast: &Cast,
    cx: &mut HeadlessAppContext,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
) {
    reach_chat(cx, model, cast, rec, true);
    open_dialog(cx, win, ShellDialog::CustomEmoji);
    answer_emoji_images(cx, model, cast, rec, win);
    draw(cx, win);
}

fn emoji_dialog(
    cx: &mut HeadlessAppContext,
    window: &WindowHandle<RootView>,
) -> Entity<lazy_ui::dialogs::CustomEmojiDialog> {
    window
        .update(cx, |root, _, app| {
            root.shell()
                .expect("the shell is showing")
                .read(app)
                .custom_emoji_dialog()
                .expect("the custom emoji dialog is open")
        })
        .expect("the window")
}

/// Chooses a picture file in the dialog, as the file dialog would hand it over.
fn choose_emoji_file(
    cx: &mut HeadlessAppContext,
    window: &WindowHandle<RootView>,
    name: &str,
    shape: u8,
    hue: f32,
) {
    let dir = std::env::temp_dir().join(format!("lazy-screens-3b-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    std::fs::write(&path, emoji_png(shape, hue, 128, 128)).expect("write the picture");
    let dialog = emoji_dialog(cx, window);
    window
        .update(cx, |_, _, app| {
            dialog.update(app, |dialog, cx| dialog.file_chosen(path.clone(), cx))
        })
        .expect("choose the file");
    cx.run_until_parked();
    draw(cx, window);
}

/// Opens the dialog's Add form from the list with the keyboard and chooses a picture.
fn emoji_add_form(
    model: &Entity<AppModel>,
    cast: &Cast,
    cx: &mut HeadlessAppContext,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
) {
    emoji_dialog_scene(model, cast, cx, rec, win);
    press(cx, win, "tab enter");
    choose_emoji_file(cx, win, "blob_cat.png", 0, 0.62);
}

// ---- Search (slice 3C) --------------------------------------------------------------------------------

/// Opens search with Ctrl+F, as a person at the keyboard would.
fn open_search(cx: &mut HeadlessAppContext, window: &WindowHandle<RootView>) {
    draw(cx, window);
    press(cx, window, "ctrl-f");
}

/// Puts `text` in the search field and tells the core, as typing it would.
fn search_text(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    window: &WindowHandle<RootView>,
    text: &str,
) {
    let field = window
        .update(cx, |root, _, app| {
            root.shell()
                .expect("the shell")
                .read(app)
                .search_view()
                .read(app)
                .field()
                .clone()
        })
        .expect("the window");
    window
        .update(cx, |_, _, app| {
            field.update(app, |field, cx| field.set_text(text, cx))
        })
        .expect("type");
    dispatch(
        cx,
        model,
        Input::SetSearchQuery {
            text: text.to_owned(),
        },
    );
    draw(cx, window);
}

/// Searches for what is in the field and answers with `hits`.
fn search_answer(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    rec: &Recorder,
    window: &WindowHandle<RootView>,
    hits: Vec<lazy_protocol::SearchHit>,
    more: bool,
    truncated: bool,
) {
    dispatch(cx, model, Input::RunSearch);
    let generation = rec
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::Search { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("a search was asked for");
    dispatch(
        cx,
        model,
        Input::SearchResult {
            generation,
            result: Ok(lazy_protocol::SearchResponse {
                hits,
                has_more: more,
                truncated,
            }),
        },
    );
    draw(cx, window);
}

/// Searches and fails with `error`.
fn search_fail(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    rec: &Recorder,
    window: &WindowHandle<RootView>,
    error: RequestError,
) {
    dispatch(cx, model, Input::RunSearch);
    let generation = rec
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::Search { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("a search was asked for");
    dispatch(
        cx,
        model,
        Input::SearchResult {
            generation,
            result: Err(error),
        },
    );
    draw(cx, window);
}

/// The highlight ranges (in characters) of every word of `text` that begins with `stem`, case
/// aside, as a search snippet marks them: the whole word.
fn marks(text: &str, stem: &str) -> Vec<(u32, u32)> {
    let chars: Vec<char> = text.chars().collect();
    let stem: Vec<char> = stem.to_lowercase().chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let starts_word = at == 0 || !chars[at - 1].is_alphanumeric();
        let fits = at + stem.len() <= chars.len()
            && chars[at..at + stem.len()]
                .iter()
                .zip(&stem)
                .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()));
        if starts_word && fits {
            let mut end = at + stem.len();
            while end < chars.len() && chars[end].is_alphanumeric() {
                end += 1;
            }
            out.push((at as u32, end as u32));
            at = end;
        } else {
            at += 1;
        }
    }
    out
}

impl Cast {
    /// A search hit in `conversation`, with `ranges` (in characters) highlighted.
    fn search_hit(
        &self,
        conversation: &Conversation,
        author: &User,
        text: &str,
        ranges: &[(u32, u32)],
        minutes_ago: i64,
    ) -> lazy_protocol::SearchHit {
        let message = Message {
            conversation_id: conversation.id,
            ..self.message(author, "placeholder", 50, minutes_ago)
        };
        lazy_protocol::SearchHit {
            message,
            conversation: conversation.clone(),
            kind: lazy_protocol::SearchHitKind::Message,
            snippet: lazy_protocol::Snippet {
                text: text.to_owned(),
                highlights: if ranges.is_empty() {
                    marks(text, "deploy")
                } else {
                    ranges.to_vec()
                }
                .iter()
                .map(|&(start, end)| lazy_protocol::SnippetRange { start, end })
                .collect(),
            },
        }
    }

    /// Results for "deploy": messages, a reply, a file, a channel not joined, a conversation,
    /// emoji, CJK, and a long snippet.
    fn deploy_hits(&self) -> Vec<lazy_protocol::SearchHit> {
        let general = self.channel(self.general, "general", "", false);
        let engineering = self.channel(self.engineering, "engineering", "", false);
        let design = self.channel(self.design, "design", "", true);
        let random = self.channel(ConversationId::generate(), "random", "", false);
        let dm = self.people(self.dm_bob, &[&self.bob]);
        let group = self.people(self.group, &[&self.bob, &self.zoe]);
        let mut reply = self.search_hit(
            &engineering,
            &self.zoe,
            "we should deploy on Tuesday, after the freeze ends",
            &[],
            130,
        );
        reply.message.thread_root = Some(MessageId::generate());
        let mut file = self.search_hit(
            &design,
            &self.carol,
            "deploy-checklist-v3.pdf",
            &[(0, 6)],
            1_500,
        );
        file.kind = lazy_protocol::SearchHitKind::Attachment {
            attachment_id: lazy_domain::AttachmentId::generate(),
        };
        vec![
            self.search_hit(
                &general,
                &self.bob,
                "Did the deploy finish cleanly? The checklist says deploy is blocked until the migration completes.",
                &[],
                3,
            ),
            reply,
            file,
            self.search_hit(
                &random,
                &self.dana,
                "deploy windows for next week are posted in the wiki",
                &[],
                7_200,
            ),
            self.search_hit(
                &dm,
                &self.bob,
                "can you deploy 🚀 it now? Deployments 🎉 are green",
                &[],
                60 * 24 * 5,
            ),
            self.search_hit(
                &group,
                &self.zoe,
                "日本語のデプロイ手順 deploy steps 完了 and a note about the rollback plan we agreed on last quarter",
                &[],
                60 * 24 * 40,
            ),
            self.search_hit(
                &general,
                &self.carol,
                "A very long message that goes on and on about how every deploy should be announced in the channel before it starts, \
                 with the version, the owner, the rollback steps, and a link to the dashboard that shows the progress of the rollout",
                &[],
                60 * 24 * 90,
            ),
        ]
    }

    /// `count` plain results for the paging scenes.
    fn many_hits(&self, count: usize) -> Vec<lazy_protocol::SearchHit> {
        let general = self.channel(self.general, "general", "", false);
        (0..count)
            .map(|n| {
                self.search_hit(
                    &general,
                    [&self.bob, &self.zoe, &self.carol][n % 3],
                    &format!("deploy number {n}: another message that mentions the deploy"),
                    &[],
                    n as i64 * 40,
                )
            })
            .collect()
    }
}

/// Opens search, types a query, and answers with `hits`.
#[allow(clippy::too_many_arguments)]
fn searched(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    text: &str,
    hits: Vec<lazy_protocol::SearchHit>,
    more: bool,
    truncated: bool,
) {
    open_search(cx, win);
    search_text(cx, model, win, text);
    search_answer(cx, model, rec, win, hits, more, truncated);
}

// ---- Notifications, status, the thread inbox (slice 3D) ------------------------------------------------

/// Followed threads in #design, newest first: unread pills, a long first message, a deleted first
/// message, and a thread in a conversation of people.
fn inbox_entries(cast: &Cast) -> Vec<lazy_protocol::InboxEntry> {
    let design = cast.channel(cast.design, "design", "", true);
    let engineering = cast.channel(cast.engineering, "engineering", "", false);
    let group = cast.people(cast.group, &[&cast.bob, &cast.zoe]);
    let entry = |conversation: &Conversation,
                 root_author: &User,
                 root_text: &str,
                 reply_author: &User,
                 reply_text: &str,
                 replies: u32,
                 unread: u32,
                 mentions: u32,
                 minutes_ago: i64,
                 deleted: bool| {
        let mut root = Message {
            conversation_id: conversation.id,
            deleted,
            ..cast.message(root_author, root_text, 20, minutes_ago + 600)
        };
        root.thread = Some(ThreadSummary {
            reply_count: replies,
            last_reply_seq: EventSeq::new(200).unwrap(),
            last_reply_at: Timestamp::from(
                time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago),
            ),
            recent_repliers: vec![reply_author.id],
            as_of: EventSeq::new(200).unwrap(),
        });
        let mut reply = Message {
            conversation_id: conversation.id,
            thread_root: Some(root.id),
            ..cast.message(reply_author, reply_text, 200, minutes_ago)
        };
        reply.thread_root = Some(root.id);
        lazy_protocol::InboxEntry {
            state: ThreadUserState {
                root_id: root.id,
                conversation_id: conversation.id,
                following: true,
                last_read: EventSeq::ZERO,
                unread_count: unread,
                mention_count: mentions,
            },
            root,
            last_reply: Some(reply),
            conversation: conversation.clone(),
        }
    };
    vec![
        entry(
            &design,
            &cast.me,
            "Should we move the launch to next week? The migration still has two open items.",
            &cast.zoe,
            "I would. The second item is the data backfill and it is not close.",
            6,
            2,
            1,
            4,
            false,
        ),
        entry(
            &engineering,
            &cast.bob,
            "Rollout plan for the retry policy: ship behind a flag, then widen over a week, watching the failure dashboards and the queue depth the whole time.",
            &cast.carol,
            "Flag is in. Starting at 5% tomorrow morning.",
            14,
            0,
            0,
            95,
            false,
        ),
        entry(
            &design,
            &cast.zoe,
            "x",
            &cast.bob,
            "Thanks, that settles it.",
            3,
            1,
            0,
            60 * 26,
            true,
        ),
        entry(
            &group,
            &cast.me,
            "Dinner on Friday?",
            &cast.bob,
            "Works for me. 7:30?",
            1,
            0,
            0,
            60 * 24 * 4,
            false,
        ),
    ]
}

/// Opens the thread inbox and answers its first page with `entries`.
fn inbox_scene(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    entries: Vec<lazy_protocol::InboxEntry>,
    more: bool,
) {
    draw(cx, win);
    window_shell_do(cx, win, |shell, window, cx| shell.open_inbox(window, cx));
    answer_inbox_page(
        cx,
        model,
        rec,
        win,
        Ok(lazy_protocol::InboxPage {
            entries,
            has_more: more,
        }),
    );
}

fn window_shell_do(
    cx: &mut HeadlessAppContext,
    window: &WindowHandle<RootView>,
    act: impl FnOnce(
        &mut lazy_ui::shell::ShellView,
        &mut Window,
        &mut Context<lazy_ui::shell::ShellView>,
    ),
) {
    window
        .update(cx, |root, window, app| {
            let shell = root.shell().expect("the shell").clone();
            shell.update(app, |shell, cx| act(shell, window, cx));
        })
        .expect("the window");
    draw(cx, window);
}

/// Answers the inbox's newest request.
fn answer_inbox_page(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    result: Result<lazy_protocol::InboxPage, RequestError>,
) {
    let generation = rec
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::LoadInbox { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the inbox asked for a page");
    dispatch(cx, model, Input::InboxLoaded { generation, result });
    draw(cx, win);
}

/// A picture of `width` by `height` pixels in `hue`: a diagonal gradient with a lighter disc, so
/// its shape and rounded corners are easy to see.
fn photo_png(width: u32, height: u32, hue: f32) -> Vec<u8> {
    let mut picture = image::RgbaImage::new(width, height);
    let (cx, cy) = (width as f32 * 0.68, height as f32 * 0.38);
    let radius = width.min(height) as f32 * 0.22;
    for (x, y, pixel) in picture.enumerate_pixels_mut() {
        let t = (x as f32 / width as f32 + y as f32 / height as f32) / 2.0;
        let (r, g, b) = hsv(hue, 0.55, 0.45 + 0.5 * t);
        let inside = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt() < radius;
        *pixel = if inside {
            image::Rgba([255, 244, 214, 255])
        } else {
            image::Rgba([r, g, b, 255])
        };
    }
    let mut out = std::io::Cursor::new(Vec::new());
    picture
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode");
    out.into_inner()
}

fn hsv(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - f * s), v * (1.0 - (1.0 - f) * s));
    let (r, g, b) = match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

/// Answers every request for a picture the way the server would: a thumbnail at most 640 on its
/// long side, or (for `full`) the picture itself, until nothing more is asked.
fn answer_images(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    recorder: &Recorder,
    window: &WindowHandle<RootView>,
) {
    loop {
        let asked: Vec<(lazy_domain::AttachmentId, bool)> = {
            let mut effects = recorder.0.borrow_mut();
            let asked = effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::FetchImage { attachment, full } => Some((*attachment, *full)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            effects.retain(|effect| !matches!(effect, Effect::FetchImage { .. }));
            asked
        };
        if asked.is_empty() {
            break;
        }
        for (attachment, full) in asked {
            let Some((width, height, hue)) = cast.pictures.borrow().get(&attachment).copied()
            else {
                continue;
            };
            let edge = if full { 1600.0 } else { 640.0 };
            let scale = (edge / width as f32).min(edge / height as f32).min(1.0);
            let bytes = photo_png(
                ((width as f32 * scale) as u32).max(1),
                ((height as f32 * scale) as u32).max(1),
                hue,
            );
            dispatch(
                cx,
                model,
                Input::ImageLoaded {
                    attachment,
                    full,
                    result: Ok(lazy_client_core::ImageData::new(bytes)),
                },
            );
        }
        draw(cx, window);
    }
}

/// Adds `files` to the message box as a person would (a dialog or a drop), and moves each as far
/// as `progress` says: `Some(None)` uploaded, `Some(Some(f))` uploading `f` of the way,
/// `None` still being read.
fn attach_files(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    recorder: &Recorder,
    target: lazy_client_core::ComposerTarget,
    files: Vec<(&str, Vec<u8>)>,
    progress: Vec<Option<Option<f32>>>,
) -> Vec<(lazy_domain::AttachmentId, u64)> {
    let _ = cast;
    let local: Vec<lazy_client_core::LocalFile> = files
        .into_iter()
        .map(|(name, bytes)| lazy_client_core::LocalFile {
            source: lazy_client_core::FileSource::Bytes(lazy_client_core::FileBytes::new(bytes)),
            name: name.to_owned(),
        })
        .collect();
    dispatch(
        cx,
        model,
        Input::AttachFiles {
            target,
            files: local,
        },
    );
    let staged_with: Vec<(lazy_domain::AttachmentId, u64)> = recorder
        .0
        .borrow()
        .iter()
        .filter_map(|effect| match effect {
            Effect::StageFile {
                attachment,
                generation,
                ..
            } => Some((*attachment, *generation)),
            _ => None,
        })
        .collect();
    let staged: Vec<lazy_domain::AttachmentId> = staged_with.iter().map(|(id, _)| *id).collect();
    recorder
        .0
        .borrow_mut()
        .retain(|effect| !matches!(effect, Effect::StageFile { .. }));
    let chips: Vec<(lazy_domain::AttachmentId, u64)> = model.read_with(cx, |m, _| {
        m.core()
            .composer_attachments(target)
            .iter()
            .map(|chip| (chip.id, chip.size))
            .collect()
    });
    for (index, id) in staged.iter().enumerate() {
        // Files not yet read stay as chips in the Reading state.
        if matches!(progress.get(index), Some(None)) {
            continue;
        }
        let size = chips
            .iter()
            .find(|(chip, _)| chip == id)
            .map_or(1_000, |(_, size)| *size);
        dispatch(
            cx,
            model,
            Input::FileStaged {
                attachment: *id,
                generation: staged_with[index].1,
                result: Ok(lazy_client_core::StagedFile { size }),
            },
        );
    }
    let uploads: Vec<(lazy_domain::AttachmentId, u64, u64)> = recorder
        .0
        .borrow()
        .iter()
        .filter_map(|effect| match effect {
            Effect::UploadFile {
                attachment,
                generation,
                size,
                ..
            } => Some((*attachment, *generation, *size)),
            _ => None,
        })
        .collect();
    recorder
        .0
        .borrow_mut()
        .retain(|effect| !matches!(effect, Effect::UploadFile { .. }));
    for (id, generation, size) in uploads {
        let index = staged.iter().position(|staged| *staged == id).unwrap_or(0);
        match progress.get(index).copied().flatten() {
            Some(Some(fraction)) => dispatch(
                cx,
                model,
                Input::UploadProgress {
                    attachment: id,
                    generation,
                    sent: (size as f32 * fraction) as u64,
                    total: size,
                },
            ),
            Some(None) => dispatch(
                cx,
                model,
                Input::UploadResult {
                    attachment: id,
                    generation,
                    result: Ok(lazy_protocol::AttachmentResponse {
                        attachment: lazy_protocol::Attachment {
                            id,
                            name: "uploaded".into(),
                            media_type: "application/octet-stream".into(),
                            size,
                            sha256: String::new(),
                            width: None,
                            height: None,
                            has_thumbnail: false,
                        },
                    }),
                },
            ),
            None => {}
        }
    }
    staged_with
}

/// Presents the drag of `paths` over the window at `(x, y)`, as a compositor would.
fn drag_files_over(cx: &mut HeadlessAppContext, window: &WindowHandle<RootView>, x: f32, y: f32) {
    let position = gpui::point(px(x), px(y));
    let paths = gpui::ExternalPaths(vec![PathBuf::from("/tmp/quarterly-report.pdf")].into());
    cx.update_window((*window).into(), |_, w, app| {
        w.dispatch_event(
            gpui::PlatformInput::FileDrop(gpui::FileDropEvent::Entered { position, paths }),
            app,
        );
        w.dispatch_event(
            gpui::PlatformInput::FileDrop(gpui::FileDropEvent::Pending { position }),
            app,
        );
    })
    .expect("drag");
    draw(cx, window);
}

/// The window opens in one theme and the system switches to the other while the form is up, as
/// happens when the compositor reports the appearance after the window exists. Text typed after
/// the switch must be readable against the new surface.
fn theme_switch_scene(out: &std::path::Path) {
    use gpui::Keystroke;

    let mut cx = HeadlessAppContext::with_platform(
        Arc::new(gpui_wgpu::CosmicTextSystem::new("DM Sans")),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|app| lazy_ui::init(app, WindowAppearance::Light));
    let recorder = Rc::new(Recorder::default());
    let model = cx.update(|app| app.new(|_| AppModel::new(recorder.clone())));
    let window = cx
        .open_window(size(px(1100.0), px(720.0)), {
            let model = model.clone();
            move |window, app| app.new(|cx| RootView::new(model, window, cx))
        })
        .expect("open window");
    dispatch(&mut cx, &model, Input::Started(StartupState::default()));
    let settle = |cx: &mut HeadlessAppContext| {
        for _ in 0..3 {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .expect("draw");
            cx.run_until_parked();
        }
    };
    settle(&mut cx);

    // The system turns out to be dark.
    cx.update(|app| lazy_ui::theme::install(app, WindowAppearance::Dark));
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .expect("refresh");
    settle(&mut cx);

    let os = cx.update(|app| app.platform_info().os);
    let key = |cx: &mut HeadlessAppContext, text: &str| {
        let keystroke = Keystroke::parse_with_os(text, os).expect("keystroke");
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_keystroke(keystroke, cx)
        })
        .expect("key");
        settle(cx);
    };
    for ch in "alice".chars() {
        key(&mut cx, &ch.to_string());
    }
    key(&mut cx, "tab");
    for ch in "hunter22".chars() {
        key(&mut cx, &ch.to_string());
    }
    let image = cx.capture_screenshot(window.into()).expect("screenshot");
    let path = out.join("23-theme-switch-signin.png");
    image.save(&path).expect("write png");
    println!(
        "wrote {} ({}x{})",
        path.display(),
        image.width(),
        image.height()
    );
}

// ---- Message row gallery -----------------------------------------------------------------------

/// One row to show: a fabricated timeline item plus how the row presents it.
struct GalleryRow {
    item: TimelineItem,
    author: &'static str,
    editor: Option<(Entity<TextInput>, EditorPhase, EditorNotice)>,
    /// Shortcode suggestions under the editor, when its text ends in one being typed.
    suggestions: Option<lazy_ui::suggest::Suggestions>,
    focus_handles: [FocusHandle; 4],
    focused: bool,
    highlighted: bool,
    actions: Vec<RowAction>,
}

/// Draws message rows in states the live core only reaches over time: an open editor, a save in
/// flight, a conflict, a failed save. These are real `render_message_row` calls, not mock-ups.
struct Gallery {
    rows: Vec<GalleryRow>,
    zone: TimeZone,
    /// A reaction chip's tooltip, drawn on its own: hovering needs a pointer and a delay.
    tooltip: gpui::AnyView,
    show_tooltip: bool,
}

impl Gallery {
    fn new(cast: &Cast, kind: GalleryKind, cx: &mut Context<Self>) -> Self {
        let confirmed = |author: &User, markdown: &str, minutes_ago: i64, edited: bool, edit| {
            let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago);
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
                author_id: author.id,
                document: MessageDocument::from_markdown(markdown).expect("valid"),
                delivery: Delivery::Confirmed {
                    message_id: id,
                    seq: EventSeq::new(1).unwrap(),
                    created_at: Timestamp::from(at),
                    revision: if edited { 2 } else { 1 },
                },
                edited_at: edited.then(|| Timestamp::from(at)),
                edit,
                reactions: vec![],
                reactions_as_of: EventSeq::ZERO,
                rich: None,
            }
        };
        let handles = |cx: &mut Context<Self>| [(); 4].map(|()| cx.focus_handle().tab_stop(true));
        let own = [
            RowAction::Edit,
            RowAction::Delete,
            RowAction::CopyLink,
            RowAction::CopyText,
        ];
        let others = [RowAction::CopyLink, RowAction::CopyText];
        let mut rows = Vec::new();
        let mut push = |cx: &mut Context<Self>,
                        item: TimelineItem,
                        author: &'static str,
                        editor: Option<(&str, EditorPhase, EditorNotice)>,
                        focused: bool,
                        highlighted: bool,
                        actions: &[RowAction]| {
            let editor = editor.map(|(text, phase, notice)| {
                let input = cx.new(|cx| {
                    TextInput::new(
                        TextInputOptions::new("Edit message")
                            .accessible_label("Edit message")
                            .max_lines(10)
                            .formatting_shortcuts(true),
                        cx,
                    )
                });
                input.update(cx, |input, cx| input.set_text(text, cx));
                (input, phase, notice)
            });
            let suggestions = editor.as_ref().and_then(|(input, _, _)| {
                let mut suggestions = lazy_ui::suggest::Suggestions::default();
                suggestions
                    .refresh(
                        input.read(cx),
                        &lazy_ui::suggest::Sources {
                            mentions: &|_| Vec::new(),
                            custom_emoji: &|_| Vec::new(),
                        },
                    )
                    .then_some(suggestions)
            });
            rows.push(GalleryRow {
                item,
                author,
                editor,
                suggestions,
                focus_handles: handles(cx),
                focused,
                highlighted,
                actions: actions.to_vec(),
            });
        };
        match kind {
            GalleryKind::Editing => {
                push(
                    cx,
                    confirmed(
                        &cast.bob,
                        "Did the migration finish cleanly?",
                        50,
                        false,
                        EditState::None,
                    ),
                    "Bob Okafor",
                    None,
                    true,
                    false,
                    &others,
                );
                push(
                    cx,
                    confirmed(
                        &cast.me,
                        "The retry policy applies to *webhooks* too.",
                        45,
                        false,
                        EditState::None,
                    ),
                    "Alice Nguyen",
                    Some((
                        "The retry policy applies to *webhooks* too, with `max_attempts = 5`.\nSee [the runbook](https://example.com/runbook).",
                        EditorPhase::Editing,
                        EditorNotice::None,
                    )),
                    false,
                    false,
                    &own,
                );
                push(
                    cx,
                    confirmed(
                        &cast.me,
                        "Saving this edit right now.",
                        30,
                        true,
                        EditState::Saving,
                    ),
                    "Alice Nguyen",
                    Some((
                        "Saving this edit right now.",
                        EditorPhase::Saving,
                        EditorNotice::None,
                    )),
                    false,
                    false,
                    &own,
                );
                push(
                    cx,
                    confirmed(
                        &cast.me,
                        "Their version: **retry three times** and then page someone.",
                        25,
                        true,
                        EditState::Conflict {
                            text: "mine".into(),
                        },
                    ),
                    "Alice Nguyen",
                    Some((
                        "My version: retry five times, then page someone.",
                        EditorPhase::Editing,
                        EditorNotice::Conflict,
                    )),
                    false,
                    false,
                    &own,
                );
                push(
                    cx,
                    confirmed(
                        &cast.me,
                        "This text could not be saved.",
                        20,
                        false,
                        EditState::Failed {
                            reason: "Couldn't reach the server (timed out).".into(),
                            text: "x".into(),
                            retry: RetryKind::SameOperation,
                        },
                    ),
                    "Alice Nguyen",
                    Some((
                        "This text could not be saved, but it is still here.",
                        EditorPhase::Editing,
                        EditorNotice::Failed("Couldn't reach the server (timed out).".into()),
                    )),
                    false,
                    false,
                    &own,
                );
                push(
                    cx,
                    confirmed(&cast.me, "Almost done.", 15, false, EditState::None),
                    "Alice Nguyen",
                    Some((
                        "Almost done, thanks all :tad",
                        EditorPhase::Editing,
                        EditorNotice::None,
                    )),
                    false,
                    false,
                    &own,
                );
                push(
                    cx,
                    confirmed(
                        &cast.me,
                        "An empty message is not allowed.",
                        10,
                        false,
                        EditState::None,
                    ),
                    "Alice Nguyen",
                    Some(("", EditorPhase::EmptyPrompt, EditorNotice::None)),
                    false,
                    false,
                    &own,
                );
            }
            GalleryKind::Statuses => {
                push(
                    cx,
                    confirmed(
                        &cast.bob,
                        "Edited once. Shows the marker next to the time.",
                        90,
                        true,
                        EditState::None,
                    ),
                    "Bob Okafor",
                    None,
                    false,
                    false,
                    &others,
                );
                push(
                    cx,
                    confirmed(
                        &cast.bob,
                        "A second message right after it, also edited, with no header.",
                        89,
                        true,
                        EditState::None,
                    ),
                    "Bob Okafor",
                    None,
                    false,
                    false,
                    &others,
                );
                push(
                    cx,
                    confirmed(
                        &cast.me,
                        "Saved on top, new text shown while it saves.",
                        60,
                        true,
                        EditState::Saving,
                    ),
                    "Alice Nguyen",
                    None,
                    false,
                    false,
                    &own,
                );
                push(
                    cx,
                    confirmed(
                        &cast.me,
                        "The server still has this version.",
                        50,
                        false,
                        EditState::Failed {
                            reason: "Couldn't reach the server (timed out).".into(),
                            text: "lost?".into(),
                            retry: RetryKind::SameOperation,
                        },
                    ),
                    "Alice Nguyen",
                    None,
                    false,
                    false,
                    &own,
                );
                push(
                    cx,
                    confirmed(
                        &cast.me,
                        "Someone else's newer version of my message.",
                        40,
                        true,
                        EditState::Conflict {
                            text: "mine".into(),
                        },
                    ),
                    "Alice Nguyen",
                    None,
                    false,
                    false,
                    &own,
                );
                push(
                    cx,
                    confirmed(
                        &cast.zoe,
                        "This is the message a permalink took you to.",
                        30,
                        false,
                        EditState::None,
                    ),
                    "Zoë Lindqvist",
                    None,
                    false,
                    true,
                    &others,
                );
                push(
                    cx,
                    confirmed(
                        &cast.zoe,
                        "This one holds the keyboard cursor, so the toolbar shows too.",
                        29,
                        false,
                        EditState::None,
                    ),
                    "Zoë Lindqvist",
                    None,
                    true,
                    false,
                    &others,
                );
            }
        }
        let tooltip = lazy_ui::tooltip::TextTooltip::view(
            "Bob Okafor, Zoë Lindqvist and 2 others reacted with :tada: 🎉",
            cx,
        );
        Self {
            rows,
            zone: TimeZone::system(),
            tooltip,
            show_tooltip: matches!(kind, GalleryKind::Statuses),
        }
    }
}

#[derive(Clone, Copy)]
enum GalleryKind {
    Editing,
    Statuses,
}

impl Render for Gallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let now = Zoned::now();
        let mut previous: Option<&TimelineItem> = None;
        let mut column = div()
            .size_full()
            .flex()
            .flex_col()
            .pt_6()
            .gap_2()
            .bg(theme.colors.surface)
            .font_family(lazy_ui::fonts::FAMILY);
        for (index, row) in self.rows.iter().enumerate() {
            let layout = row_layout(previous, &row.item, &self.zone, &now);
            previous = Some(&row.item);
            let [save_focus, cancel_focus, delete_focus, keep_focus] = row.focus_handles.clone();
            let on_retry: RetryHandler = Rc::new(|_, _| {});
            column = column.child(render_message_row(RowProps {
                thread_line: None,
                reply_header: None,
                tombstone: false,
                pane: None,
                author_is_bot: false,
                author_status: None,
                saved: false,
                author_avatar: None,
                author_deactivated: false,
                on_profile: std::rc::Rc::new(|_, _| {}),
                attachments: vec![],
                upload_note: None,
                on_discard: Rc::new(|_, _| {}),
                discard_focus: None,
                select: None,
                label_prefix: "",
                on_press: None,
                pressing: None,
                link_previews: &[],
                index,
                item: &row.item,
                author_name: row.author.into(),
                layout,
                zone: &self.zone,
                now: &now,
                theme: &theme,
                on_retry: on_retry.clone(),
                new_messages_divider: false,
                retry_focus: None,
                on_link: open_external_only(),
                actions: RowActions {
                    available: row.actions.clone(),
                    on_action: Rc::new(|_, _, _| {}),
                },
                focused: row.focused,
                highlighted: row.highlighted,
                editor: row
                    .editor
                    .as_ref()
                    .map(|(input, phase, notice)| EditorProps {
                        input: input.clone(),
                        phase: *phase,
                        notice: notice.clone(),
                        on_command: Rc::new(|_, _, _| {}),
                        save_focus,
                        cancel_focus,
                        delete_focus,
                        keep_focus,
                        suggestions: row.suggestions.clone(),
                        suggestion_pictures: Default::default(),
                        on_pick_suggestion: Rc::new(|_, _, _| {}),
                    }),
                chips: vec![],
                can_react: false,
                on_toggle_reaction: Rc::new(|_, _, _| {}),
                on_add_reaction: Rc::new(|_, _, _| {}),
                on_edit_again: on_retry,
                mentions: &lazy_ui::rich_text::HandlesOnly,
                mentions_me: false,
            }));
        }
        // Only the status gallery has room for the tooltip sample.
        if self.show_tooltip {
            column = column.child(div().px_5().pt_4().child(self.tooltip.clone()));
        }
        column
    }
}

fn gallery_scene(
    out: &std::path::Path,
    name: &str,
    kind: GalleryKind,
    appearance: WindowAppearance,
    width: f32,
    height: f32,
) {
    let mut cx = HeadlessAppContext::with_platform(
        Arc::new(gpui_wgpu::CosmicTextSystem::new("DM Sans")),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|app| lazy_ui::init(app, appearance));
    let cast = Cast::new();
    let window = cx
        .open_window(size(px(width), px(height)), move |_, app| {
            app.new(|cx| Gallery::new(&cast, kind, cx))
        })
        .expect("open window");
    for _ in 0..4 {
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .expect("draw");
        cx.run_until_parked();
    }
    let image = cx.capture_screenshot(window.into()).expect("screenshot");
    let path = out.join(format!("{name}.png"));
    image.save(&path).expect("write png");
    println!(
        "wrote {} ({}x{})",
        path.display(),
        image.width(),
        image.height()
    );
}

// ---- The first run (D29) ----

/// How far into the first-run screens a scene goes.
#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum Upto {
    Connect,
    Checking,
    Unreachable,
    BadCode,
    Invited,
    Account,
    Taken,
    Picture,
    /// The owner is asked what the workspace is called.
    Workspace,
    /// The name is typed.
    Naming,
    /// The first invitation is being made.
    Share,
    /// The first invitation is there, with the message to send.
    Shared,
    Done,
}

/// The generation of the newest effect `pick` selects.
fn newest_generation(rec: &Recorder, pick: impl Fn(&Effect) -> Option<u64>) -> u64 {
    rec.0
        .borrow()
        .iter()
        .rev()
        .find_map(pick)
        .expect("the effect was asked for")
}

/// Does what a person does on the first-run screens, with the server answering, up to `upto`.
fn first_run(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    upto: Upto,
) {
    first_run_as(cx, model, cast, rec, win, upto, false)
}

/// The same for the person who runs the server: the code is the setup code, and the picture is
/// followed by the workspace's name and the first invitation.
fn first_run_owner(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    upto: Upto,
) {
    first_run_as(cx, model, cast, rec, win, upto, true)
}

fn first_run_as(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    rec: &Recorder,
    win: &WindowHandle<RootView>,
    upto: Upto,
    owner: bool,
) {
    dispatch(cx, model, Input::Started(StartupState::default()));
    // The screens arrive with a fade; the stills show them arrived. (Starting applies the saved
    // appearance, so the choice is made after it.)
    dispatch(
        cx,
        model,
        Input::SetAppearance(lazy_client_core::Appearance {
            motion: lazy_client_core::MotionChoice::Reduce,
            ..Default::default()
        }),
    );
    draw(cx, win);
    if upto == Upto::Connect {
        return;
    }
    // The address, then the code (the keyboard starts in the address).
    type_text(cx, win, "chat.example.com");
    press(cx, win, "tab");
    type_text(cx, win, "k7qm4txd9bwph2vazx3c");
    press(cx, win, "enter");
    if upto == Upto::Checking {
        return;
    }
    if upto == Upto::Unreachable {
        answer_server(cx, model, rec, Err("connection failed".into()));
        draw(cx, win);
        return;
    }
    dispatch(
        cx,
        model,
        Input::ServerFacts {
            url: "https://chat.example.com".into(),
            facts: Some(lazy_client_core::ServerFacts {
                registration: lazy_protocol::ServerRegistration::InviteOnly,
                setup_code_required: owner,
            }),
        },
    );
    answer_server(cx, model, rec, Ok(lazy_protocol::PROTOCOL_VERSION));
    let moved = newest_generation(rec, |effect| match effect {
        Effect::UseServer { generation, .. } => Some(*generation),
        _ => None,
    });
    dispatch(
        cx,
        model,
        Input::ServerSwitched {
            generation: moved,
            result: Ok(Box::default()),
        },
    );
    let asked = newest_generation(rec, |effect| match effect {
        Effect::PreviewInvitation { generation, .. } => Some(*generation),
        _ => None,
    });
    if upto == Upto::BadCode {
        dispatch(
            cx,
            model,
            Input::InvitationPreviewed {
                generation: asked,
                result: Err(lazy_client_core::PreviewFailure::Invalid),
            },
        );
        draw(cx, win);
        return;
    }
    dispatch(
        cx,
        model,
        Input::InvitationPreviewed {
            generation: asked,
            result: Ok(lazy_protocol::InvitationPreview {
                kind: if owner {
                    lazy_protocol::InvitationKind::Setup
                } else {
                    lazy_protocol::InvitationKind::Invitation
                },
                workspace_name: "Zebrafish Collective".into(),
                inviter: Some(DisplayName::parse("Alice Example").unwrap()),
                member_count: 12,
            }),
        },
    );
    draw(cx, win);
    if upto == Upto::Invited {
        return;
    }
    // "Join workspace" has the keyboard.
    press(cx, win, "enter");
    if upto == Upto::Account {
        return;
    }
    type_text(cx, win, "Quentin Fakename");
    press(cx, win, "tab tab");
    type_text(cx, win, "correct horse");
    press(cx, win, "enter");
    if upto == Upto::Taken {
        dispatch(
            cx,
            model,
            Input::AuthFailed(lazy_client_core::RequestError::Rejected(
                lazy_protocol::ApiError::new(
                    lazy_protocol::ErrorCode::UsernameTaken,
                    "that username is taken",
                ),
            )),
        );
        draw(cx, win);
        return;
    }
    dispatch(
        cx,
        model,
        Input::Authenticated {
            user: cast.me.clone(),
        },
    );
    dispatch(
        cx,
        model,
        Input::BootstrapLoaded(Box::new(cast.bootstrap())),
    );
    draw(cx, win);
    if upto == Upto::Picture {
        return;
    }
    // "Skip for now".
    press(cx, win, "tab enter");
    if !owner {
        return;
    }
    // The owner goes on to name the workspace.
    draw(cx, win);
    if upto == Upto::Workspace {
        return;
    }
    type_text(cx, win, "Acme Design");
    if upto == Upto::Naming {
        return;
    }
    press(cx, win, "enter");
    dispatch(
        cx,
        model,
        Input::ActionResult {
            kind: lazy_client_core::ActionKind::UpdateWorkspace,
            result: Ok(lazy_client_core::ActionOutcome::Workspace(
                lazy_protocol::Workspace {
                    id: cast.workspace,
                    name: "Acme Design".into(),
                    settings: Default::default(),
                },
            )),
        },
    );
    draw(cx, win);
    if upto == Upto::Share {
        return;
    }
    dispatch(
        cx,
        model,
        Input::ActionResult {
            kind: lazy_client_core::ActionKind::CreateInvitation,
            result: Ok(lazy_client_core::ActionOutcome::InvitationCreated(
                lazy_protocol::CreatedInvitation {
                    invitation: lazy_protocol::Invitation {
                        id: lazy_domain::InvitationId::generate(),
                        workspace_id: cast.workspace,
                        created_by: cast.me.id,
                        created_at: lazy_domain::Timestamp::now(),
                        expires_at: lazy_domain::Timestamp::from(
                            time::OffsetDateTime::now_utc() + time::Duration::days(7),
                        ),
                        max_uses: Some(25),
                        uses: 0,
                        state: lazy_protocol::InvitationState::Active,
                    },
                    code: lazy_protocol::InviteCode::new("K7QM4TXD9BWPH2VAZX3C"),
                },
            )),
        },
    );
    draw(cx, win);
    if upto == Upto::Shared {
        return;
    }
    // Copy has the keyboard; Continue is next.
    press(cx, win, "tab enter");
}

fn main() {
    let out: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "screens".into())
        .into();
    std::fs::create_dir_all(&out).expect("create output directory");

    // An optional second argument renders only the scenes whose names start with it.
    let filter: Option<String> = std::env::args().nth(2);
    let scenes: Vec<Scene<'_>> = vec![
        Scene {
            name: "01-signin-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, _, cx, _, _win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
            },
        },
        Scene {
            name: "02-signin-error-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, _, cx, _, _win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                dispatch(
                    cx,
                    model,
                    Input::SignIn {
                        username: "al".into(),
                        password: "x".into(),
                    },
                );
            },
        },
        Scene {
            name: "03-chat-light-wide",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "04-chat-dark-wide",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "05-chat-light-medium",
            width: 820.0,
            height: 600.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "06-chat-light-narrow-minimum",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "07-empty-channel-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, false),
        },
        Scene {
            name: "08-delivery-states-and-reconnecting-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat(cx, model, cast, rec, true);
                // Three sends: one stays pending, one is in doubt, one is rejected.
                let mut ops = Vec::new();
                for text in [
                    "Sending this one right now",
                    "I lost my connection mid-send",
                    "This one the server refused",
                ] {
                    dispatch(
                        cx,
                        model,
                        Input::SendMessage {
                            conversation: cast.general,
                            text: text.into(),
                        },
                    );
                    ops.push(model.read_with(cx, |m, _| {
                        m.core()
                            .timeline(cast.general)
                            .last()
                            .and_then(|i| i.operation_id())
                            .unwrap()
                    }));
                }
                // Messages are sent one at a time, so release them in order.
                dispatch(cx, model, Input::SubmissionPersisted { operation: ops[0] });
                dispatch(
                    cx,
                    model,
                    Input::SendResult {
                        operation: ops[0],
                        result: Err(RequestError::Unknown("connection reset".into())),
                    },
                );
                dispatch(
                    cx,
                    model,
                    Input::Socket(SocketEvent::Closed {
                        workspace: cast.workspace,
                        attempt: 2,
                    }),
                );
            },
        },
        Scene {
            name: "09-composer-multiline-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(
                    cx,
                    model,
                    Input::SetDraft {
                        conversation: cast.general,
                        text: "Draft with several lines\nand an emoji 🙂\nand a third line that is quite long so that it wraps around the composer width when the window is narrow".into(),
                    },
                );
            },
        },
        Scene {
            name: "10-rejected-send-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(
                    cx,
                    model,
                    Input::SendMessage {
                        conversation: cast.general,
                        text: "A message the server will refuse".into(),
                    },
                );
                let op = model.read_with(cx, |m, _| {
                    m.core()
                        .timeline(cast.general)
                        .last()
                        .and_then(|i| i.operation_id())
                        .unwrap()
                });
                dispatch(cx, model, Input::SubmissionPersisted { operation: op });
                dispatch(
                    cx,
                    model,
                    Input::SendResult {
                        operation: op,
                        result: Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Forbidden,
                            "you cannot post in this conversation",
                        ))),
                    },
                );
            },
        },
        Scene {
            name: "11-emoji-and-scripts-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat(cx, model, cast, rec, false);
                for (n, text) in [
                    "Plain emoji: 🎉 😀 👍 ❤️ ☕ 🚀 🙂",
                    "Skin tones: 👍🏽 👋🏿",
                    "Flags and sequences: 🇨🇦 🏳️‍🌈 👩‍👩‍👧‍👦 👨🏻‍💻",
                    "Accents and CJK: café naïve Zoë — 日本語 — 한국어 — 中文",
                    "Right to left: שלום עולם — مرحبا بالعالم",
                ]
                .into_iter()
                .enumerate()
                {
                    let event = lazy_protocol::EventEnvelope {
                        workspace_id: cast.workspace,
                        seq: EventSeq::new(100 + n as i64).unwrap(),
                        occurred_at: Timestamp::now(),
                        event: lazy_protocol::Event::MessageCreated {
                            message: cast.message(&cast.zoe, text, 100 + n as i64, 5 - n as i64),
                            operation_id: None,
                        },
                    };
                    dispatch(cx, model, Input::Frame(ServerFrame::Event(event)));
                }
            },
        },
        Scene {
            name: "12-create-channel-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::CreateChannel);
                type_text(cx, win, "Design Team!");
            },
        },
        Scene {
            name: "13-create-channel-name-taken-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::CreateChannel);
                type_text(cx, win, "general");
                press(cx, win, "enter");
                let operation = rec
                    .0
                    .borrow()
                    .iter()
                    .find_map(|effect| match effect {
                        Effect::CreateChannel { request, .. } => Some(request.operation_id),
                        _ => None,
                    })
                    .expect("the dialog sent a create request");
                dispatch(
                    cx,
                    model,
                    Input::ConversationCommandResult {
                        operation,
                        result: Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::NameTaken,
                            "A channel with that name already exists.",
                        ))),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "14-browse-channels-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::BrowseChannels);
                let mut archived =
                    cast.channel(ConversationId::generate(), "old-launch", "Retired", false);
                archived.archived = true;
                answer_browse(
                    cx,
                    model,
                    rec,
                    vec![
                        BrowsedChannel { conversation: cast.channel(cast.general, "general", "Company-wide announcements and chatter", false), joined: true },
                        BrowsedChannel { conversation: cast.channel(ConversationId::generate(), "random", "Anything goes, within reason", false), joined: false },
                        BrowsedChannel { conversation: cast.channel(ConversationId::generate(), "support", "Customer questions and escalations, and the long-running thread about the ticket backlog", false), joined: false },
                        BrowsedChannel { conversation: archived, joined: false },
                    ],
                    false,
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "15-new-message-picker-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::NewMessage);
                type_text(cx, win, "bo");
                press(cx, win, "enter");
                type_text(cx, win, "z");
            },
        },
        Scene {
            name: "16-add-people-to-direct-message-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SelectConversation(cast.dm_bob));
                answer_history(cx, model, rec, cast.dm_bob, vec![]);
                open_dialog(cx, win, ShellDialog::Details(cast.dm_bob));
                // The close button has focus; one Shift+Tab reaches "Add people…", Enter opens it.
                press(cx, win, "shift-tab enter");
            },
        },
        Scene {
            name: "17-channel-details-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SelectConversation(cast.design));
                answer_history(cx, model, rec, cast.design, vec![]);
                open_dialog(cx, win, ShellDialog::Details(cast.design));
            },
        },
        Scene {
            name: "18-leave-confirmation-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SelectConversation(cast.design));
                answer_history(cx, model, rec, cast.design, vec![]);
                open_dialog(cx, win, ShellDialog::Details(cast.design));
                // Topic has focus; description, Add people, then Leave channel.
                press(cx, win, "tab tab tab enter");
            },
        },
        Scene {
            name: "19-new-messages-divider-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let mut bootstrap = cast.bootstrap();
                bootstrap.workspaces[0].user_state[0] = ConversationUserState {
                    conversation_id: cast.general,
                    favorite: false,
                    last_read: EventSeq::new(5).unwrap(),
                    unread_count: 4,
                    mention_count: 0,
                };
                dispatch(
                    cx,
                    model,
                    Input::Started(StartupState {
                        session_restored: true,
                        ..Default::default()
                    }),
                );
                dispatch(cx, model, Input::BootstrapLoaded(Box::new(bootstrap)));
                dispatch(
                    cx,
                    model,
                    Input::Socket(SocketEvent::Opened {
                        workspace: cast.workspace,
                    }),
                );
                dispatch(
                    cx,
                    model,
                    Input::Frame(ServerFrame::CaughtUp {
                        workspace_id: cast.workspace,
                        seq: EventSeq::ZERO,
                    }),
                );
                answer_history(cx, model, rec, cast.general, cast.history());
                draw(cx, win);
            },
        },
        Scene {
            name: "20-archived-channel-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SelectConversation(cast.design));
                answer_history(cx, model, rec, cast.design, vec![]);
                let mut archived =
                    cast.channel(cast.design, "design", "Pixels, type, and feelings", true);
                archived.archived = true;
                // The core only accepts an answer to an action it started.
                dispatch(cx, model, Input::ArchiveConversation(cast.design));
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::Archive(cast.design),
                        result: Ok(ActionOutcome::Conversation(archived)),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "21-empty-workspace-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, _rec, win| {
                let mut bootstrap = cast.bootstrap();
                bootstrap.workspaces[0].conversations.clear();
                bootstrap.workspaces[0].user_state.clear();
                dispatch(
                    cx,
                    model,
                    Input::Started(StartupState {
                        session_restored: true,
                        ..Default::default()
                    }),
                );
                dispatch(cx, model, Input::BootstrapLoaded(Box::new(bootstrap)));
                draw(cx, win);
            },
        },
        Scene {
            name: "22-dialog-at-minimum-size-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::CreateChannel);
                type_text(cx, win, "Design Team");
            },
        },
        Scene {
            name: "24-rich-messages-light",
            width: 1100.0,
            height: 1100.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat_with(cx, model, cast, rec, cast.rich_history())
            },
        },
        Scene {
            name: "25-rich-messages-dark",
            width: 1100.0,
            height: 1100.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| {
                reach_chat_with(cx, model, cast, rec, cast.rich_history())
            },
        },
        Scene {
            name: "26-rich-messages-narrow-light",
            width: 640.0,
            height: 1300.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat_with(cx, model, cast, rec, cast.rich_history())
            },
        },
        Scene {
            name: "29-delete-message-dialog-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let history = cast.rich_history();
                let message = history[5].clone();
                reach_chat_with(cx, model, cast, rec, history);
                open_dialog(
                    cx,
                    win,
                    ShellDialog::DeleteMessage {
                        conversation: cast.general,
                        message: message.id,
                    },
                );
            },
        },
        Scene {
            name: "30-delete-message-dialog-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let history = cast.rich_history();
                let message = history[2].clone();
                reach_chat_with(cx, model, cast, rec, history);
                open_dialog(
                    cx,
                    win,
                    ShellDialog::DeleteMessage {
                        conversation: cast.general,
                        message: message.id,
                    },
                );
            },
        },
        Scene {
            name: "33-reactions-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat_with(cx, model, cast, rec, cast.reactions_history())
            },
        },
        Scene {
            name: "34-reactions-dark",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| {
                reach_chat_with(cx, model, cast, rec, cast.reactions_history())
            },
        },
        Scene {
            name: "35-reactions-narrow-light",
            width: 640.0,
            height: 1000.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat_with(cx, model, cast, rec, cast.reactions_history())
            },
        },
        Scene {
            name: "36-picker-default-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                use_some_emoji(cx, model);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(1060.), px(650.)),
                );
            },
        },
        Scene {
            name: "37-picker-default-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                use_some_emoji(cx, model);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(1060.), px(650.)),
                );
            },
        },
        Scene {
            name: "38-picker-search-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(1060.), px(650.)),
                );
                type_text(cx, win, "heart");
                press(cx, win, "down right");
            },
        },
        Scene {
            name: "39-picker-search-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(1060.), px(650.)),
                );
                type_text(cx, win, "cat");
            },
        },
        Scene {
            name: "40-picker-skin-tone-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SetSkinTone(4));
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(1060.), px(650.)),
                );
                // To the group tabs, then on to People & body.
                press(cx, win, "tab right");
            },
        },
        Scene {
            name: "41-picker-for-a-reaction-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.reactions_history());
                use_some_emoji(cx, model);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Reaction {
                        conversation: cast.general,
                        message: cast.reactions_history()[3].id,
                    },
                    gpui::point(px(1050.), px(300.)),
                );
                // Into the grid, then a few moves, so the selection and its preview show.
                press(cx, win, "tab tab right right down");
            },
        },
        Scene {
            name: "42-autocomplete-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                type_text(cx, win, "Nice work :tad");
            },
        },
        Scene {
            name: "43-autocomplete-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                type_text(cx, win, "so :sm");
                press(cx, win, "down down");
            },
        },
        Scene {
            name: "44-picker-at-minimum-size-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(600.), px(400.)),
                );
            },
        },
        Scene {
            name: "45-mentions-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat_with(cx, model, cast, rec, cast.mentions_history())
            },
        },
        Scene {
            name: "46-mentions-dark",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| {
                reach_chat_with(cx, model, cast, rec, cast.mentions_history())
            },
        },
        Scene {
            name: "47-mentions-narrow-light",
            width: 640.0,
            height: 1100.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat_with(cx, model, cast, rec, cast.mentions_history())
            },
        },
        Scene {
            name: "48-mention-autocomplete-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.mentions_history());
                type_text(cx, win, "Looping in @");
                press(cx, win, "down down");
            },
        },
        Scene {
            name: "49-mention-autocomplete-filtered-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.mentions_history());
                type_text(cx, win, "Please see the notes, cc @te");
            },
        },
        Scene {
            name: "50-mention-autocomplete-outside-the-channel-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.mentions_history());
                dispatch(cx, model, Input::SelectConversation(cast.engineering));
                answer_history(cx, model, rec, cast.engineering, vec![]);
                answer_members(cx, model, cast, rec);
                draw(cx, win);
                type_text(cx, win, "@");
            },
        },
        Scene {
            name: "51-mention-autocomplete-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.mentions_history());
                type_text(cx, win, "Can someone from @o");
            },
        },
        Scene {
            name: "52-mention-autocomplete-in-the-editor-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.mentions_history());
                // Up in the empty composer edits the person's latest message.
                press(cx, win, "up ctrl-a");
                type_text(cx, win, "Thanks @bo");
            },
        },
        Scene {
            name: "53-user-groups-list-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Groups);
                press(cx, win, "down");
            },
        },
        Scene {
            name: "54-user-groups-list-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Groups);
                press(cx, win, "down down");
            },
        },
        Scene {
            name: "55-user-groups-empty-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                for (n, group) in [&cast.crew, &cast.ops, &cast.team].into_iter().enumerate() {
                    dispatch(
                        cx,
                        model,
                        Input::Frame(ServerFrame::Event(EventEnvelope {
                            workspace_id: cast.workspace,
                            seq: EventSeq::new(n as i64 + 1).unwrap(),
                            occurred_at: Timestamp::now(),
                            event: Event::GroupDeleted { group_id: group.id },
                        })),
                    );
                }
                open_dialog(cx, win, ShellDialog::Groups);
            },
        },
        Scene {
            name: "56-user-group-new-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Groups);
                // Close, Delete, Edit, New group.
                press(cx, win, "tab tab tab tab enter");
                type_text(cx, win, "@Design-Ops");
                press(cx, win, "tab");
                type_text(cx, win, "Design Operations");
                press(cx, win, "tab");
                type_text(cx, win, "bo");
                press(cx, win, "enter");
                type_text(cx, win, "c");
            },
        },
        Scene {
            name: "57-user-group-handle-problem-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Groups);
                press(cx, win, "tab tab tab tab enter");
                type_text(cx, win, "channel");
                press(cx, win, "tab");
                type_text(cx, win, "Everyone");
            },
        },
        Scene {
            name: "58-user-group-edit-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Groups);
                // The list is ordered by handle: design-crew, ops, team.
                press(cx, win, "down down enter");
            },
        },
        Scene {
            name: "59-user-group-core-error-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Groups);
                press(cx, win, "tab tab tab tab enter");
                // "bob" is a person's username, which the core will not allow as a group handle.
                type_text(cx, win, "bob");
                press(cx, win, "tab");
                type_text(cx, win, "Bobs");
                press(cx, win, "enter");
            },
        },
        Scene {
            name: "60-user-group-server-error-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Groups);
                press(cx, win, "tab tab tab tab enter");
                type_text(cx, win, "interns");
                press(cx, win, "tab");
                type_text(cx, win, "Interns");
                press(cx, win, "enter");
                let operation = rec
                    .0
                    .borrow()
                    .iter()
                    .find_map(|effect| match effect {
                        Effect::CreateGroup { request, .. } => Some(request.operation_id),
                        _ => None,
                    })
                    .expect("saving asks the server to create the group");
                dispatch(
                    cx,
                    model,
                    Input::GroupCommandResult {
                        operation,
                        result: Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::LimitReached,
                            "This workspace already has the most groups it can have.",
                        ))),
                    },
                );
            },
        },
        Scene {
            name: "61-user-group-delete-confirmation-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Groups);
                press(cx, win, "down delete");
            },
        },
        Scene {
            name: "62-user-groups-at-minimum-size-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Groups);
                press(cx, win, "tab tab tab tab enter");
                type_text(cx, win, "design-ops");
                press(cx, win, "tab");
                type_text(cx, win, "Design Operations");
            },
        },
        Scene {
            name: "63-focus-on-filled-buttons-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Groups);
                // The list, then the editor for the third group, then Save: handle, name, people,
                // Cancel, Save.
                press(cx, win, "down down enter tab tab tab tab");
            },
        },
        Scene {
            name: "64-focus-on-send-dark",
            width: 1100.0,
            height: 600.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                type_text(cx, win, "On my way");
                // The emoji button, then Send.
                press(cx, win, "tab tab");
            },
        },
        Scene {
            name: "2d-01-summaries-light",
            width: 1280.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
            },
        },
        Scene {
            name: "2d-02-summaries-dark",
            width: 1280.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
            },
        },
        Scene {
            name: "2d-03-keyboard-on-a-summary-light",
            width: 1280.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                // Shift+Tab reaches the timeline on its newest message; Up steps back, and the
                // summary line under the staging question is a stop of its own.
                press(cx, win, "shift-tab up");
            },
        },
        Scene {
            name: "2d-04-pane-short-thread-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.short_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
            },
        },
        Scene {
            name: "2d-05-pane-short-thread-dark",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.short_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
            },
        },
        Scene {
            name: "2d-06-pane-long-thread-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.long_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
            },
        },
        Scene {
            name: "2d-07-pane-long-thread-dark",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.long_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
            },
        },
        Scene {
            name: "2d-08-pane-empty-thread-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                let plain = history[0].clone();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                open_thread(cx, model, win, rec, cast, &plain, vec![]);
            },
        },
        Scene {
            name: "2d-09-pane-empty-thread-dark",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                let plain = history[0].clone();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                open_thread(cx, model, win, rec, cast, &plain, vec![]);
            },
        },
        Scene {
            name: "2d-10-pane-composer-also-send-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.short_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
                type_text(cx, win, "Telling the customers today.");
                // The checkbox follows the box; Space ticks it.
                press(cx, win, "tab space");
            },
        },
        Scene {
            name: "2d-11-pane-autocomplete-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.short_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
                type_text(cx, win, "Thanks @d");
            },
        },
        Scene {
            name: "2d-12-pane-autocomplete-dark",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.short_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
                type_text(cx, win, "Done :tad");
            },
        },
        Scene {
            name: "2d-13-pane-edit-in-progress-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, mut replies) = cast.short_thread();
                replies.push(cast.thread_reply(
                    &root,
                    &cast.me,
                    "I will tell them in the morning, once the plan is final.",
                    24,
                    5,
                ));
                open_thread(cx, model, win, rec, cast, &root, replies);
                // Up in the empty reply box edits the person's newest reply.
                press(cx, win, "up");
                type_text(cx, win, " Also updating the status page.");
            },
        },
        Scene {
            name: "2d-14-pane-edit-in-progress-dark",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, mut replies) = cast.short_thread();
                replies.push(cast.thread_reply(
                    &root,
                    &cast.me,
                    "I will tell them in the morning, once the plan is final.",
                    24,
                    5,
                ));
                open_thread(cx, model, win, rec, cast, &root, replies);
                press(cx, win, "up");
                type_text(cx, win, " Also updating the status page.");
            },
        },
        Scene {
            name: "2d-15-pane-narrow-replaces-conversation-light",
            width: 900.0,
            height: 700.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.short_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
            },
        },
        Scene {
            name: "2d-16-pane-narrow-replaces-conversation-dark",
            width: 900.0,
            height: 700.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.long_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
            },
        },
        Scene {
            name: "2d-17-pane-delete-confirmation-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, mut replies) = cast.short_thread();
                let mine = cast.thread_reply(
                    &root,
                    &cast.me,
                    "I will tell them in the morning, once the plan is final.",
                    24,
                    5,
                );
                replies.push(mine.clone());
                open_thread(cx, model, win, rec, cast, &root, replies);
                open_dialog(
                    cx,
                    win,
                    ShellDialog::DeleteMessage {
                        conversation: cast.general,
                        message: mine.id,
                    },
                );
            },
        },
        Scene {
            name: "2d-18-pane-delete-confirmation-dark",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, mut replies) = cast.short_thread();
                let mine = cast.thread_reply(
                    &root,
                    &cast.me,
                    "I will tell them in the morning, once the plan is final.",
                    24,
                    5,
                );
                replies.push(mine.clone());
                open_thread(cx, model, win, rec, cast, &root, replies);
                open_dialog(
                    cx,
                    win,
                    ShellDialog::DeleteMessage {
                        conversation: cast.general,
                        message: mine.id,
                    },
                );
            },
        },
        Scene {
            name: "2d-19-pane-wide-and-focused-handle-light",
            width: 1600.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.long_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
                // Back from the reply box through the checkbox, the replies, and the two buttons to
                // the resize handle, which then takes Left to widen the pane.
                press(
                    cx,
                    win,
                    "shift-tab shift-tab shift-tab shift-tab left left left left",
                );
            },
        },
        Scene {
            name: "2d-20-pane-menu-dark",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.short_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
                // Back from the reply box to the replies, then Enter on the newest reply.
                press(cx, win, "shift-tab enter");
            },
        },
        // ---- 2E: selecting text, apps, integrations, and the states in between ----
        Scene {
            name: "2e-01-select-in-a-message-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.rich_history());
                select_text(cx, win, false, (3, 10), (3, 140));
            },
        },
        Scene {
            name: "2e-02-select-across-code-quote-list-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.rich_history());
                select_text(cx, win, false, (2, 7), (3, 180));
            },
        },
        Scene {
            name: "2e-03-select-across-code-quote-list-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.rich_history());
                select_text(cx, win, false, (2, 7), (3, 180));
            },
        },
        Scene {
            name: "2e-04-select-across-mention-pills-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.mentions_history());
                select_text(cx, win, false, (1, 4), (3, 40));
            },
        },
        Scene {
            name: "2e-05-select-all-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.rich_history());
                // Back from the composer to the messages, then select everything.
                press(cx, win, "shift-tab ctrl-a");
            },
        },
        Scene {
            name: "2e-06-select-in-the-thread-pane-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.short_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
                select_text(cx, win, true, (0, 12), (2, 30));
            },
        },
        Scene {
            name: "2e-07-app-badge-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.bot_history());
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-08-app-badge-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.bot_history());
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-09-app-badge-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.bot_history());
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-10-integrations-loading-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SelectConversation(cast.design));
                answer_history(cx, model, rec, cast.design, vec![]);
                open_dialog(cx, win, ShellDialog::Details(cast.design));
            },
        },
        Scene {
            name: "2e-11-integrations-empty-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                open_integrations(cx, model, win, rec, cast, vec![]);
            },
        },
        Scene {
            name: "2e-12-integrations-list-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let hooks = cast.webhooks(cast.design, &["Deploy bot", "Pager", "Billing alerts"]);
                open_integrations(cx, model, win, rec, cast, hooks);
            },
        },
        Scene {
            name: "2e-13-integrations-list-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let hooks = cast.webhooks(cast.design, &["Deploy bot", "Pager", "Billing alerts"]);
                open_integrations(cx, model, win, rec, cast, hooks);
            },
        },
        Scene {
            name: "2e-14-integrations-adding-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let hooks = cast.webhooks(cast.design, &["Deploy bot"]);
                open_integrations(cx, model, win, rec, cast, hooks);
                focus_add_webhook(cx, win);
                press(cx, win, "enter");
                type_text(cx, win, "Pager");
            },
        },
        Scene {
            name: "2e-15-integrations-add-refused-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let hooks = cast.webhooks(cast.design, &["Deploy bot"]);
                open_integrations(cx, model, win, rec, cast, hooks);
                focus_add_webhook(cx, win);
                press(cx, win, "enter");
                type_text(cx, win, "Pager");
                press(cx, win, "enter");
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::CreateWebhook(cast.design),
                        result: Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Forbidden,
                            "forbidden",
                        ))),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-16-integrations-created-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                open_integrations(cx, model, win, rec, cast, vec![]);
                create_webhook(cx, model, win, cast, "Deploy bot");
            },
        },
        Scene {
            name: "2e-17-integrations-created-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                open_integrations(cx, model, win, rec, cast, vec![]);
                create_webhook(cx, model, win, cast, "Deploy bot");
            },
        },
        Scene {
            name: "2e-18-integrations-created-minimum-size-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                open_integrations(cx, model, win, rec, cast, vec![]);
                create_webhook(cx, model, win, cast, "Deploy bot");
            },
        },
        Scene {
            name: "2e-19-integrations-delete-confirmation-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let hooks = cast.webhooks(cast.design, &["Deploy bot", "Pager"]);
                open_integrations(cx, model, win, rec, cast, hooks);
                // Back from Add incoming webhook to the last row's Delete, then open it.
                focus_add_webhook(cx, win);
                press(cx, win, "shift-tab enter");
            },
        },
        Scene {
            name: "2e-20-integrations-delete-confirmation-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let hooks = cast.webhooks(cast.design, &["Deploy bot", "Pager"]);
                open_integrations(cx, model, win, rec, cast, hooks);
                focus_add_webhook(cx, win);
                press(cx, win, "shift-tab enter");
            },
        },
        Scene {
            name: "2e-21-integrations-delete-refused-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let hooks = cast.webhooks(cast.design, &["Deploy bot", "Pager"]);
                let pager = hooks[1].id;
                open_integrations(cx, model, win, rec, cast, hooks);
                focus_add_webhook(cx, win);
                // Open the confirmation, move to "Delete webhook", and press it.
                press(cx, win, "shift-tab enter tab enter");
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::DeleteWebhook(pager),
                        result: Err(RequestError::Unknown("connection reset".into())),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-22-integrations-load-failed-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SelectConversation(cast.design));
                answer_history(cx, model, rec, cast.design, vec![]);
                open_dialog(cx, win, ShellDialog::Details(cast.design));
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::LoadWebhooks(cast.design),
                        result: Err(RequestError::Unknown("timed out".into())),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-23-integrations-at-the-limit-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let names: Vec<String> = (1..=10).map(|n| format!("App {n}")).collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                let hooks = cast.webhooks(cast.design, &names);
                open_integrations(cx, model, win, rec, cast, hooks);
            },
        },
        Scene {
            name: "2e-24-integrations-archived-channel-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SelectConversation(cast.design));
                answer_history(cx, model, rec, cast.design, vec![]);
                let mut archived =
                    cast.channel(cast.design, "design", "Pixels, type, and feelings", true);
                archived.archived = true;
                dispatch(cx, model, Input::ArchiveConversation(cast.design));
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::Archive(cast.design),
                        result: Ok(ActionOutcome::Conversation(archived)),
                    },
                );
                open_dialog(cx, win, ShellDialog::Details(cast.design));
                let hooks = cast.webhooks(cast.design, &["Deploy bot"]);
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::LoadWebhooks(cast.design),
                        result: Ok(ActionOutcome::Webhooks(hooks)),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-25-integrations-list-minimum-size-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let hooks = cast.webhooks(cast.design, &["Deploy bot", "Pager", "Billing alerts"]);
                open_integrations(cx, model, win, rec, cast, hooks);
            },
        },
        Scene {
            name: "2e-30-history-failed-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_custom(
                    cx,
                    model,
                    cast,
                    rec,
                    Chat {
                        history: Err(RequestError::Unknown("timed out".into())),
                        has_more: false,
                        answer_people: true,
                        problems: vec![],
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-31-history-failed-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_custom(
                    cx,
                    model,
                    cast,
                    rec,
                    Chat {
                        history: Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Forbidden,
                            "You can't read that conversation",
                        ))),
                        has_more: false,
                        answer_people: true,
                        problems: vec![],
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-32-older-history-failed-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_custom(
                    cx,
                    model,
                    cast,
                    rec,
                    Chat {
                        history: Ok(cast.history()),
                        has_more: true,
                        answer_people: true,
                        problems: vec![],
                    },
                );
                dispatch(
                    cx,
                    model,
                    Input::LoadOlder {
                        conversation: cast.general,
                    },
                );
                let generation = rec
                    .0
                    .borrow()
                    .iter()
                    .rev()
                    .find_map(|effect| match effect {
                        Effect::FetchHistory { generation, .. } => Some(*generation),
                        _ => None,
                    })
                    .expect("asked for older messages");
                dispatch(
                    cx,
                    model,
                    Input::HistoryLoaded {
                        conversation: cast.general,
                        generation,
                        result: Err(RequestError::Unknown("connection reset".into())),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-33-older-history-failed-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_custom(
                    cx,
                    model,
                    cast,
                    rec,
                    Chat {
                        history: Ok(cast.history()),
                        has_more: true,
                        answer_people: true,
                        problems: vec![],
                    },
                );
                dispatch(
                    cx,
                    model,
                    Input::LoadOlder {
                        conversation: cast.general,
                    },
                );
                let generation = rec
                    .0
                    .borrow()
                    .iter()
                    .rev()
                    .find_map(|effect| match effect {
                        Effect::FetchHistory { generation, .. } => Some(*generation),
                        _ => None,
                    })
                    .expect("asked for older messages");
                dispatch(
                    cx,
                    model,
                    Input::HistoryLoaded {
                        conversation: cast.general,
                        generation,
                        result: Err(RequestError::Unknown("connection reset".into())),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-34-history-failed-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_custom(
                    cx,
                    model,
                    cast,
                    rec,
                    Chat {
                        history: Err(RequestError::Unknown(
                            "the connection was reset by the server while the page was being read"
                                .into(),
                        )),
                        has_more: false,
                        answer_people: true,
                        problems: vec![],
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-35-older-history-failed-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_custom(
                    cx,
                    model,
                    cast,
                    rec,
                    Chat {
                        history: Ok(cast.history()),
                        has_more: true,
                        answer_people: true,
                        problems: vec![],
                    },
                );
                dispatch(
                    cx,
                    model,
                    Input::LoadOlder {
                        conversation: cast.general,
                    },
                );
                let generation = rec
                    .0
                    .borrow()
                    .iter()
                    .rev()
                    .find_map(|effect| match effect {
                        Effect::FetchHistory { generation, .. } => Some(*generation),
                        _ => None,
                    })
                    .expect("asked for older messages");
                dispatch(
                    cx,
                    model,
                    Input::HistoryLoaded {
                        conversation: cast.general,
                        generation,
                        result: Err(RequestError::Unknown("connection reset".into())),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-36-thread-failed-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, _replies) = cast.short_thread();
                dispatch(
                    cx,
                    model,
                    Input::OpenThread {
                        conversation: cast.general,
                        root: root.id,
                    },
                );
                let generation = rec
                    .0
                    .borrow()
                    .iter()
                    .rev()
                    .find_map(|effect| match effect {
                        Effect::FetchReplies { generation, .. } => Some(*generation),
                        _ => None,
                    })
                    .expect("opening the thread asks for its replies");
                dispatch(
                    cx,
                    model,
                    Input::RepliesLoaded {
                        root: root.id,
                        generation,
                        result: Err(RequestError::Unknown("timed out".into())),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-37-server-unreachable-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, _cast, cx, _rec, win| {
                dispatch(
                    cx,
                    model,
                    Input::Started(StartupState {
                        session_restored: true,
                        ..Default::default()
                    }),
                );
                dispatch(
                    cx,
                    model,
                    Input::BootstrapFailed(RequestError::Unknown(
                        "connection refused (127.0.0.1:8080)".into(),
                    )),
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-38-server-unreachable-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, _cast, cx, _rec, win| {
                dispatch(
                    cx,
                    model,
                    Input::Started(StartupState {
                        session_restored: true,
                        ..Default::default()
                    }),
                );
                dispatch(
                    cx,
                    model,
                    Input::BootstrapFailed(RequestError::Unknown(
                        "connection refused (127.0.0.1:8080)".into(),
                    )),
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-39-session-expired-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SessionExpired);
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-40-notice-banner-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_custom(
                    cx,
                    model,
                    cast,
                    rec,
                    Chat {
                        history: Ok(cast.history()),
                        has_more: false,
                        answer_people: true,
                        problems: vec![
                            "Your saved drafts couldn't be read, so they were set aside.".into(),
                        ],
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-41-notice-banner-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_custom(
                    cx,
                    model,
                    cast,
                    rec,
                    Chat {
                        history: Ok(cast.history()),
                        has_more: false,
                        answer_people: true,
                        problems: vec![
                            "Your saved drafts couldn't be read, so they were set aside.".into(),
                        ],
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "2e-42-people-still-loading-for-a-mention-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_custom(
                    cx,
                    model,
                    cast,
                    rec,
                    Chat {
                        history: Ok(cast.history()),
                        has_more: false,
                        answer_people: false,
                        problems: vec![],
                    },
                );
                draw(cx, win);
                type_text(cx, win, "Thanks @bo");
            },
        },
        // ---- 3A: attachments ----
        Scene {
            name: "3a-01-tray-uploading-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.history());
                attach_files(
                    cx,
                    model,
                    cast,
                    rec,
                    ComposerTarget::Conversation(cast.general),
                    vec![
                        ("sunset-over-the-bay.png", photo_png(600, 400, 0.05)),
                        ("Quarterly report.pdf", vec![0; 1_258_291]),
                        ("release-1.4.2-linux-x86_64.tar.gz", vec![0; 18_000_000]),
                    ],
                    vec![Some(None), Some(Some(0.6)), None],
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-02-tray-failed-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.history());
                let target = ComposerTarget::Conversation(cast.general);
                let staged = attach_files(
                    cx,
                    model,
                    cast,
                    rec,
                    target,
                    vec![
                        ("diagram.png", photo_png(500, 500, 0.55)),
                        ("meeting-notes.txt", vec![b'x'; 4_000]),
                        ("movie.mkv", vec![0; 200]),
                    ],
                    vec![Some(None), None, None],
                );
                let ids: Vec<lazy_domain::AttachmentId> = model.read_with(cx, |m, _| {
                    m.core()
                        .composer_attachments(target)
                        .iter()
                        .map(|chip| chip.id)
                        .collect()
                });
                // The notes cannot be uploaded: the network failed three times.
                dispatch(
                    cx,
                    model,
                    Input::FileStaged {
                        attachment: ids[1],
                        generation: staged[1].1,
                        result: Ok(lazy_client_core::StagedFile { size: 4_000 }),
                    },
                );
                for _ in 0..4 {
                    let generation = rec.0.borrow().iter().rev().find_map(|effect| match effect {
                        Effect::UploadFile {
                            attachment,
                            generation,
                            ..
                        } if *attachment == ids[1] => Some(*generation),
                        _ => None,
                    });
                    if let Some(generation) = generation {
                        rec.0
                            .borrow_mut()
                            .retain(|e| !matches!(e, Effect::UploadFile { .. }));
                        dispatch(
                            cx,
                            model,
                            Input::UploadResult {
                                attachment: ids[1],
                                generation,
                                result: Err(lazy_client_core::TransferError::Request(
                                    RequestError::Unknown("timed out".into()),
                                )),
                            },
                        );
                    }
                }
                // The movie is over the size limit.
                dispatch(
                    cx,
                    model,
                    Input::FileStaged {
                        attachment: ids[2],
                        generation: staged[2].1,
                        result: Err(lazy_client_core::TransferError::TooLarge {
                            size: 40 * 1024 * 1024,
                        }),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-03-tray-ten-files-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.history());
                let mut files = Vec::new();
                for n in 0..10u32 {
                    if n % 3 == 0 {
                        files.push((
                            format!("photo-{n:02}-from-the-offsite.png"),
                            photo_png(320, 240, n as f32 / 10.0),
                        ));
                    } else {
                        files.push((
                            format!("Budget draft {n} (final).xlsx"),
                            vec![0; 90_000 * (n as usize + 1)],
                        ));
                    }
                }
                let files: Vec<(&str, Vec<u8>)> = files
                    .iter()
                    .map(|(name, bytes)| (name.as_str(), bytes.clone()))
                    .collect();
                attach_files(
                    cx,
                    model,
                    cast,
                    rec,
                    ComposerTarget::Conversation(cast.general),
                    files,
                    vec![Some(None); 10],
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-04-tray-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.history());
                attach_files(
                    cx,
                    model,
                    cast,
                    rec,
                    ComposerTarget::Conversation(cast.general),
                    vec![
                        ("sunset.png", photo_png(600, 400, 0.05)),
                        ("Quarterly report.pdf", vec![0; 1_258_291]),
                        ("notes.txt", vec![b'x'; 900]),
                    ],
                    vec![Some(None), Some(Some(0.35)), None],
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-05-pending-uploading-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.history());
                attach_files(
                    cx,
                    model,
                    cast,
                    rec,
                    ComposerTarget::Conversation(cast.general),
                    vec![
                        ("floor-plan.png", photo_png(800, 500, 0.3)),
                        ("Quarterly report.pdf", vec![0; 1_258_291]),
                        ("data-export.csv", vec![b'1'; 600_000]),
                    ],
                    vec![Some(None), Some(Some(0.45)), Some(Some(0.1))],
                );
                dispatch(
                    cx,
                    model,
                    Input::SendMessage {
                        conversation: cast.general,
                        text: "Here are the files for the launch review.".into(),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-06-one-image-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.bob,
                    "The view from the new office:",
                    10,
                    4,
                    vec![cast.picture_file("new-office.png", 1600, 900, 0.58)],
                ));
                reach_chat_with(cx, model, cast, rec, history);
                answer_images(cx, model, cast, rec, win);
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-07-one-image-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.bob,
                    "The view from the new office:",
                    10,
                    4,
                    vec![cast.picture_file("new-office.png", 1600, 900, 0.58)],
                ));
                reach_chat_with(cx, model, cast, rec, history);
                answer_images(cx, model, cast, rec, win);
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-08-several-images-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.zoe,
                    "Offsite photos, all five:",
                    10,
                    6,
                    vec![
                        cast.picture_file("landscape.png", 1200, 800, 0.08),
                        cast.picture_file("portrait.png", 600, 900, 0.35),
                        cast.picture_file("square.png", 800, 800, 0.62),
                        cast.picture_file("panorama.png", 3000, 700, 0.8),
                        cast.picture_file("tiny.png", 90, 60, 0.15),
                    ],
                ));
                reach_chat_with(cx, model, cast, rec, history);
                answer_images(cx, model, cast, rec, win);
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-09-several-images-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.zoe,
                    "",
                    10,
                    6,
                    vec![
                        cast.picture_file("landscape.png", 1200, 800, 0.08),
                        cast.picture_file("portrait.png", 600, 900, 0.35),
                        cast.picture_file("square.png", 800, 800, 0.62),
                    ],
                ));
                reach_chat_with(cx, model, cast, rec, history);
                answer_images(cx, model, cast, rec, win);
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-10-file-chips-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let files = vec![
                    cast.plain_file("Quarterly report.pdf", "application/pdf", 1_258_291),
                    cast.plain_file(
                        "release-1.4.2-linux-x86_64.tar.gz",
                        "application/gzip",
                        18_000_000,
                    ),
                    cast.plain_file("meeting-notes.txt", "text/plain", 4_200),
                    cast.plain_file("voice-memo.mp3", "audio/mpeg", 2_400_000),
                    cast.plain_file("screen-recording.mp4", "video/mp4", 52_000_000),
                ];
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.carol,
                    "Everything from the review:",
                    10,
                    5,
                    files.clone(),
                ));
                reach_chat_with(cx, model, cast, rec, history);
                let download =
                    |cx: &mut HeadlessAppContext, file: &lazy_protocol::Attachment, path: &str| {
                        dispatch(
                            cx,
                            model,
                            Input::DownloadAttachment {
                                attachment: file.clone(),
                                destination: PathBuf::from(path),
                            },
                        );
                    };
                // The tarball is part way down, the notes are saved, the voice memo failed.
                download(
                    cx,
                    &files[1],
                    "/home/alice/Downloads/release-1.4.2-linux-x86_64.tar.gz",
                );
                dispatch(
                    cx,
                    model,
                    Input::DownloadProgress {
                        attachment: files[1].id,
                        received: 7_200_000,
                        total: 18_000_000,
                    },
                );
                download(cx, &files[2], "/home/alice/Downloads/meeting-notes.txt");
                dispatch(
                    cx,
                    model,
                    Input::DownloadResult {
                        attachment: files[2].id,
                        result: Ok(PathBuf::from("/home/alice/Downloads/meeting-notes.txt")),
                    },
                );
                download(cx, &files[3], "/home/alice/Downloads/voice-memo.mp3");
                dispatch(
                    cx,
                    model,
                    Input::DownloadResult {
                        attachment: files[3].id,
                        result: Err(lazy_client_core::TransferError::Disk(
                            "No space left on device".into(),
                        )),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-11-attachment-only-messages-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.bob,
                    "",
                    10,
                    8,
                    vec![cast.picture_file("whiteboard.png", 1000, 700, 0.12)],
                ));
                history.push(cast.with_files(
                    &cast.bob,
                    "",
                    11,
                    7,
                    vec![cast.plain_file("whiteboard-notes.pdf", "application/pdf", 340_000)],
                ));
                history.push(cast.with_files(&cast.me, "Thanks, that helps.", 12, 2, vec![]));
                reach_chat_with(cx, model, cast, rec, history);
                answer_images(cx, model, cast, rec, win);
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-12-failed-thumbnail-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let broken = cast.picture_file("corrupt-scan.png", 1200, 900, 0.9);
                let fine = cast.picture_file("receipt.png", 900, 500, 0.4);
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.zoe,
                    "Two scans:",
                    10,
                    5,
                    vec![broken.clone(), fine],
                ));
                reach_chat_with(cx, model, cast, rec, history);
                // The server has no thumbnail for the first one.
                cast.pictures.borrow_mut().remove(&broken.id);
                dispatch(
                    cx,
                    model,
                    Input::ImageLoaded {
                        attachment: broken.id,
                        full: false,
                        result: Err(lazy_client_core::TransferError::Missing),
                    },
                );
                answer_images(cx, model, cast, rec, win);
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-13-viewer-dark",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.zoe,
                    "Offsite photos:",
                    10,
                    6,
                    vec![
                        cast.picture_file("landscape.png", 1200, 800, 0.08),
                        cast.picture_file("portrait.png", 600, 900, 0.35),
                        cast.picture_file("square.png", 800, 800, 0.62),
                    ],
                ));
                reach_chat_with(cx, model, cast, rec, history);
                answer_images(cx, model, cast, rec, win);
                // Back to the messages, to the first picture, and open it.
                press(cx, win, "shift-tab down enter");
                answer_images(cx, model, cast, rec, win);
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-14-viewer-loading-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.zoe,
                    "Offsite photos:",
                    10,
                    6,
                    vec![
                        cast.picture_file("landscape.png", 1200, 800, 0.08),
                        cast.picture_file("portrait.png", 600, 900, 0.35),
                    ],
                ));
                reach_chat_with(cx, model, cast, rec, history);
                answer_images(cx, model, cast, rec, win);
                press(cx, win, "shift-tab down enter");
                // The whole picture has not arrived: the thumbnail stands in.
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-15-viewer-downloading-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let photo = cast.picture_file("landscape.png", 1200, 800, 0.08);
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.zoe,
                    "One for the wall:",
                    10,
                    6,
                    vec![photo.clone()],
                ));
                reach_chat_with(cx, model, cast, rec, history);
                answer_images(cx, model, cast, rec, win);
                press(cx, win, "shift-tab down enter");
                answer_images(cx, model, cast, rec, win);
                dispatch(
                    cx,
                    model,
                    Input::DownloadAttachment {
                        attachment: photo.clone(),
                        destination: PathBuf::from("/home/alice/Downloads/landscape.png"),
                    },
                );
                dispatch(
                    cx,
                    model,
                    Input::DownloadProgress {
                        attachment: photo.id,
                        received: 150_000,
                        total: 320_000,
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-16-viewer-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.zoe,
                    "",
                    10,
                    6,
                    vec![
                        cast.picture_file("portrait.png", 600, 900, 0.35),
                        cast.picture_file("landscape.png", 1200, 800, 0.08),
                    ],
                ));
                reach_chat_with(cx, model, cast, rec, history);
                answer_images(cx, model, cast, rec, win);
                press(cx, win, "shift-tab down enter");
                answer_images(cx, model, cast, rec, win);
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-17-drop-overlay-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.history());
                drag_files_over(cx, win, 600.0, 300.0);
            },
        },
        Scene {
            name: "3a-18-drop-overlay-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.history());
                drag_files_over(cx, win, 400.0, 250.0);
            },
        },
        Scene {
            name: "3a-19-drop-over-the-thread-pane-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.short_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
                drag_files_over(cx, win, 1100.0, 400.0);
            },
        },
        Scene {
            name: "3a-20-thread-with-files-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, mut replies) = cast.short_thread();
                replies.push(Message {
                    attachments: vec![
                        cast.picture_file("rollout-chart.png", 1000, 600, 0.45),
                        cast.plain_file("rollout-plan.pdf", "application/pdf", 640_000),
                    ],
                    document: MessageDocument::empty(),
                    ..cast.thread_reply(&root, &cast.zoe, "x", 30, 3)
                });
                open_thread(cx, model, win, rec, cast, &root, replies);
                answer_images(cx, model, cast, rec, win);
                attach_files(
                    cx,
                    model,
                    cast,
                    rec,
                    ComposerTarget::Thread(root.id),
                    vec![
                        ("signoff.png", photo_png(400, 300, 0.7)),
                        ("approval-email.pdf", vec![0; 88_000]),
                    ],
                    vec![Some(None), Some(Some(0.5))],
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3a-21-keyboard-cursor-on-a-file-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let mut history = cast.history();
                history.push(cast.with_files(
                    &cast.carol,
                    "Both attached:",
                    10,
                    5,
                    vec![
                        cast.picture_file("chart.png", 900, 500, 0.5),
                        cast.plain_file("Quarterly report.pdf", "application/pdf", 1_258_291),
                    ],
                ));
                reach_chat_with(cx, model, cast, rec, history);
                answer_images(cx, model, cast, rec, win);
                // Back to the messages, then down through the message to its second file.
                press(cx, win, "shift-tab down down");
                draw(cx, win);
            },
        },
        // ---- Slice 3B: custom emoji ----
        Scene {
            name: "3b-01-inline-ready-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                inline_emoji_scene(model, cast, cx, rec, win, true, &[])
            },
        },
        Scene {
            name: "3b-02-inline-ready-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                inline_emoji_scene(model, cast, cx, rec, win, true, &[])
            },
        },
        Scene {
            name: "3b-03-inline-loading-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                inline_emoji_scene(model, cast, cx, rec, win, false, &[])
            },
        },
        Scene {
            name: "3b-04-inline-failed-and-deleted-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                inline_emoji_scene(
                    model,
                    cast,
                    cx,
                    rec,
                    win,
                    true,
                    &["partyparrot", "tall_tower"],
                )
            },
        },
        Scene {
            name: "3b-05-inline-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                inline_emoji_scene(model, cast, cx, rec, win, true, &[])
            },
        },
        Scene {
            name: "3b-06-reaction-chips-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, emoji_reactions_history(cast));
                answer_emoji_images(cx, model, cast, rec, win);
            },
        },
        Scene {
            name: "3b-07-reaction-chips-loading-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, emoji_reactions_history(cast));
                draw(cx, win);
            },
        },
        Scene {
            name: "3b-08-picker-custom-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                use_some_emoji(cx, model);
                dispatch(cx, model, Input::RecordEmojiUse(cast.custom("shipit")));
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(1060.), px(650.)),
                );
                answer_emoji_images(cx, model, cast, rec, win);
            },
        },
        Scene {
            name: "3b-09-picker-custom-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(1060.), px(650.)),
                );
                answer_emoji_images(cx, model, cast, rec, win);
                press(cx, win, "down right right");
            },
        },
        Scene {
            name: "3b-10-picker-custom-loading-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(1060.), px(650.)),
                );
            },
        },
        Scene {
            name: "3b-11-picker-custom-empty-member-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.emoji_mode.set(EmojiMode::EmptyAsMember);
                reach_chat(cx, model, cast, rec, true);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(1060.), px(650.)),
                );
            },
        },
        Scene {
            name: "3b-12-picker-custom-empty-admin-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.emoji_mode.set(EmojiMode::EmptyAsAdmin);
                reach_chat(cx, model, cast, rec, true);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(1060.), px(650.)),
                );
            },
        },
        Scene {
            name: "3b-13-picker-search-custom-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(1060.), px(650.)),
                );
                type_text(cx, win, "par");
                answer_emoji_images(cx, model, cast, rec, win);
            },
        },
        Scene {
            name: "3b-14-picker-custom-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_picker(
                    cx,
                    win,
                    PickerPurpose::Composer,
                    gpui::point(px(600.), px(430.)),
                );
                answer_emoji_images(cx, model, cast, rec, win);
            },
        },
        Scene {
            name: "3b-15-autocomplete-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(
                    cx,
                    model,
                    Input::SetDraft {
                        conversation: cast.general,
                        text: "Looks great :sh".into(),
                    },
                );
                answer_emoji_images(cx, model, cast, rec, win);
                draw(cx, win);
            },
        },
        Scene {
            name: "3b-16-autocomplete-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(
                    cx,
                    model,
                    Input::SetDraft {
                        conversation: cast.general,
                        text: "Pick one :t".into(),
                    },
                );
                // One more letter, as typed, so the list is about "th".
                press(cx, win, "h");
                answer_emoji_images(cx, model, cast, rec, win);
                press(cx, win, "down");
            },
        },
        Scene {
            name: "3b-17-autocomplete-loading-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(
                    cx,
                    model,
                    Input::SetDraft {
                        conversation: cast.general,
                        text: "Ship it :sh".into(),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3b-18-dialog-list-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| emoji_dialog_scene(model, cast, cx, rec, win),
        },
        Scene {
            name: "3b-19-dialog-list-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                emoji_dialog_scene(model, cast, cx, rec, win);
                press(cx, win, "down down down");
            },
        },
        Scene {
            name: "3b-20-dialog-list-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| emoji_dialog_scene(model, cast, cx, rec, win),
        },
        Scene {
            name: "3b-21-dialog-empty-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.emoji_mode.set(EmojiMode::EmptyAsAdmin);
                emoji_dialog_scene(model, cast, cx, rec, win);
            },
        },
        Scene {
            name: "3b-22-dialog-pictures-loading-and-failed-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::CustomEmoji);
                // Two answered (one picture, one failure); the rest are still on their way.
                let (ok, bad) = (cast.emoji("shipit").id, cast.emoji("blobheart").id);
                let (shape, hue, w, h) = cast.emoji_art[&ok];
                dispatch(
                    cx,
                    model,
                    Input::EmojiImageLoaded {
                        emoji: ok,
                        result: Ok(lazy_client_core::ImageData::new(emoji_png(
                            shape, hue, w, h,
                        ))),
                    },
                );
                dispatch(
                    cx,
                    model,
                    Input::EmojiImageLoaded {
                        emoji: bad,
                        result: Err(lazy_client_core::TransferError::Unreadable("gone".into())),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3b-23-dialog-add-empty-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                emoji_dialog_scene(model, cast, cx, rec, win);
                press(cx, win, "tab enter");
            },
        },
        Scene {
            name: "3b-24-dialog-add-valid-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| emoji_add_form(model, cast, cx, rec, win),
        },
        Scene {
            name: "3b-25-dialog-add-invalid-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                emoji_add_form(model, cast, cx, rec, win);
                press(cx, win, "tab ctrl-a");
                type_text(cx, win, "has space");
            },
        },
        Scene {
            name: "3b-26-dialog-add-taken-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                emoji_add_form(model, cast, cx, rec, win);
                press(cx, win, "tab ctrl-a");
                type_text(cx, win, "shipit");
            },
        },
        Scene {
            name: "3b-27-dialog-add-uploading-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                emoji_add_form(model, cast, cx, rec, win);
                press(cx, win, "tab enter");
            },
        },
        Scene {
            name: "3b-28-dialog-add-error-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                emoji_add_form(model, cast, cx, rec, win);
                press(cx, win, "tab enter");
                let upload = rec
                    .0
                    .borrow()
                    .iter()
                    .find_map(|effect| match effect {
                        Effect::UploadEmoji { emoji, .. } => Some(*emoji),
                        _ => None,
                    })
                    .expect("the upload started");
                dispatch(
                    cx,
                    model,
                    Input::CustomEmojiUploaded {
                        emoji: upload,
                        result: Err(lazy_client_core::TransferError::TooLarge { size: 900_000 }),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3b-29-dialog-rename-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                emoji_dialog_scene(model, cast, cx, rec, win);
                press(cx, win, "down enter");
            },
        },
        Scene {
            name: "3b-30-dialog-rename-problem-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                emoji_dialog_scene(model, cast, cx, rec, win);
                press(cx, win, "down enter ctrl-a");
                type_text(cx, win, "lgtm");
            },
        },
        Scene {
            name: "3b-31-dialog-delete-confirm-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                emoji_dialog_scene(model, cast, cx, rec, win);
                press(cx, win, "down delete");
            },
        },
        Scene {
            name: "3b-32-dialog-delete-error-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                emoji_dialog_scene(model, cast, cx, rec, win);
                press(cx, win, "down delete tab enter");
                let id = cast.emoji("cat_typing").id;
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::DeleteCustomEmoji(id),
                        result: Err(RequestError::Unknown("timed out".into())),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3b-33-dialog-add-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| emoji_add_form(model, cast, cx, rec, win),
        },
        // ---- Slice 3C: search ----
        Scene {
            name: "3c-01-entry-in-the-sidebar-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "3c-02-idle-with-recents-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                *cast.recent_searches.borrow_mut() = vec![
                    "deploy failed".into(),
                    "from:zoe in:design has:image".into(),
                    "\"rollback plan\" -draft".into(),
                    "after:2026-09-01 budget".into(),
                ];
                reach_chat(cx, model, cast, rec, true);
                open_search(cx, win);
            },
        },
        Scene {
            name: "3c-03-idle-empty-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_search(cx, win);
            },
        },
        Scene {
            name: "3c-04-recents-active-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                *cast.recent_searches.borrow_mut() =
                    vec!["deploy failed".into(), "from:zoe in:design".into()];
                reach_chat(cx, model, cast, rec, true);
                open_search(cx, win);
                press(cx, win, "down");
            },
        },
        Scene {
            name: "3c-05-loading-skeleton-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_search(cx, win);
                search_text(cx, model, win, "deploy");
                dispatch(cx, model, Input::RunSearch);
                draw(cx, win);
            },
        },
        Scene {
            name: "3c-06-results-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy",
                    cast.deploy_hits(),
                    false,
                    true,
                );
            },
        },
        Scene {
            name: "3c-07-results-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy",
                    cast.deploy_hits(),
                    false,
                    false,
                );
            },
        },
        Scene {
            name: "3c-08-active-result-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy",
                    cast.deploy_hits(),
                    false,
                    false,
                );
                press(cx, win, "down down down");
            },
        },
        Scene {
            name: "3c-09-results-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy",
                    cast.deploy_hits(),
                    false,
                    true,
                );
                press(cx, win, "down");
            },
        },
        Scene {
            name: "3c-10-filter-chips-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy from:bob from:zoe in:#general after:2026-09-01 has:image",
                    cast.deploy_hits().into_iter().take(2).collect(),
                    false,
                    false,
                );
            },
        },
        Scene {
            name: "3c-11-parse-error-with-chips-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_search(cx, win);
                search_text(cx, model, win, "deploy from:bob after:soon");
            },
        },
        Scene {
            name: "3c-12-no-results-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "zzzzz from:bob in:design",
                    vec![],
                    false,
                    false,
                );
            },
        },
        Scene {
            name: "3c-13-too-slow-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_search(cx, win);
                search_text(cx, model, win, "the");
                search_fail(
                    cx,
                    model,
                    rec,
                    win,
                    RequestError::Rejected(ApiError::new(ErrorCode::Unavailable, "slow")),
                );
            },
        },
        Scene {
            name: "3c-14-network-failure-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_search(cx, win);
                search_text(cx, model, win, "deploy");
                search_fail(cx, model, rec, win, RequestError::Unknown("reset".into()));
            },
        },
        Scene {
            name: "3c-15-workspace-gone-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_search(cx, win);
                search_text(cx, model, win, "deploy");
                search_fail(
                    cx,
                    model,
                    rec,
                    win,
                    RequestError::Rejected(ApiError::new(ErrorCode::NotFound, "gone")),
                );
            },
        },
        Scene {
            name: "3c-16-show-more-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy",
                    cast.many_hits(20),
                    true,
                    false,
                );
            },
        },
        Scene {
            name: "3c-17-loading-more-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy",
                    cast.many_hits(20),
                    true,
                    false,
                );
                dispatch(cx, model, Input::LoadMoreSearch);
                draw(cx, win);
            },
        },
        Scene {
            name: "3c-18-more-failed-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy",
                    cast.many_hits(20),
                    true,
                    false,
                );
                dispatch(cx, model, Input::LoadMoreSearch);
                let generation = rec
                    .0
                    .borrow()
                    .iter()
                    .rev()
                    .find_map(|effect| match effect {
                        Effect::Search { generation, .. } => Some(*generation),
                        _ => None,
                    })
                    .expect("the next page was asked for");
                dispatch(
                    cx,
                    model,
                    Input::SearchResult {
                        generation,
                        result: Err(RequestError::Unknown("reset".into())),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3c-19-end-of-search-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy",
                    cast.many_hits(20),
                    true,
                    true,
                );
                for _ in 0..9 {
                    press(cx, win, "alt-end");
                    let generation = rec
                        .0
                        .borrow()
                        .iter()
                        .rev()
                        .find_map(|effect| match effect {
                            Effect::Search { generation, .. } => Some(*generation),
                            _ => None,
                        })
                        .expect("a page was asked for");
                    dispatch(
                        cx,
                        model,
                        Input::SearchResult {
                            generation,
                            result: Ok(lazy_protocol::SearchResponse {
                                hits: cast.many_hits(20),
                                has_more: true,
                                truncated: true,
                            }),
                        },
                    );
                    draw(cx, win);
                }
                press(cx, win, "alt-end");
            },
        },
        Scene {
            name: "3c-20-looking-for-the-message-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy",
                    cast.deploy_hits(),
                    false,
                    false,
                );
                // A result in another channel: its message has to be fetched first.
                press(cx, win, "down down down enter");
            },
        },
        Scene {
            name: "3c-21-thread-pane-stays-light",
            width: 1280.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let (history, threads) = cast.threads_history();
                reach_chat_threads(cx, model, cast, rec, history, threads);
                let (root, replies) = cast.short_thread();
                open_thread(cx, model, win, rec, cast, &root, replies);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy",
                    cast.deploy_hits(),
                    false,
                    false,
                );
            },
        },
        Scene {
            name: "3c-22-results-medium-light",
            width: 820.0,
            height: 600.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                searched(
                    cx,
                    model,
                    rec,
                    win,
                    "deploy from:bob in:general",
                    cast.deploy_hits(),
                    false,
                    true,
                );
                press(cx, win, "down down");
            },
        },
        // ---- Slice 3D: notifications, status, the thread inbox ----
        Scene {
            name: "3d-01-sidebar-threads-muted-paused-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                cast.followed_unread.set(3);
                cast.muted_group.set(true);
                cast.dnd_until.set(Some(Timestamp::from(
                    time::OffsetDateTime::now_utc() + time::Duration::minutes(45),
                )));
                *cast.my_status.borrow_mut() = Some(
                    lazy_domain::notification::UserStatus::new(
                        "In a meeting until 3",
                        lazy_domain::ReactionEmoji::parse("📅").ok(),
                        None,
                    )
                    .unwrap(),
                );
                reach_chat(cx, model, cast, rec, true);
            },
        },
        Scene {
            name: "3d-02-sidebar-threads-muted-paused-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| {
                cast.followed_unread.set(120);
                cast.muted_group.set(true);
                cast.dnd_until.set(Some(Timestamp::from(
                    time::OffsetDateTime::now_utc() + time::Duration::days(2000),
                )));
                reach_chat(cx, model, cast, rec, true);
            },
        },
        Scene {
            name: "3d-03-bell-menu-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                // Back from the composer: timeline, Details, Favorite, the bell.
                press(cx, win, "shift-tab shift-tab shift-tab shift-tab enter");
            },
        },
        Scene {
            name: "3d-04-bell-menu-muted-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(
                    cx,
                    model,
                    Input::SetConversationNotificationPrefs {
                        conversation: cast.general,
                        level: Some(lazy_domain::notification::NotifyLevel::All),
                        muted: true,
                    },
                );
                press(cx, win, "shift-tab shift-tab shift-tab shift-tab enter");
            },
        },
        Scene {
            name: "3d-05-inbox-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.followed_unread.set(3);
                reach_chat(cx, model, cast, rec, true);
                inbox_scene(cx, model, rec, win, inbox_entries(cast), false);
            },
        },
        Scene {
            name: "3d-06-inbox-active-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.followed_unread.set(3);
                reach_chat(cx, model, cast, rec, true);
                inbox_scene(cx, model, rec, win, inbox_entries(cast), false);
                press(cx, win, "down down");
            },
        },
        Scene {
            name: "3d-07-inbox-loading-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                draw(cx, win);
                window_shell_do(cx, win, |shell, window, cx| shell.open_inbox(window, cx));
            },
        },
        Scene {
            name: "3d-08-inbox-empty-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                inbox_scene(cx, model, rec, win, vec![], false);
            },
        },
        Scene {
            name: "3d-09-inbox-no-unread-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                inbox_scene(cx, model, rec, win, inbox_entries(cast), false);
                // Tab back to the All/Unread control and choose Unread.
                press(cx, win, "shift-tab shift-tab right");
                answer_inbox_page(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::InboxPage {
                        entries: vec![],
                        has_more: false,
                    }),
                );
            },
        },
        Scene {
            name: "3d-10-inbox-failed-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                draw(cx, win);
                window_shell_do(cx, win, |shell, window, cx| shell.open_inbox(window, cx));
                answer_inbox_page(
                    cx,
                    model,
                    rec,
                    win,
                    Err(RequestError::Unknown("reset".into())),
                );
            },
        },
        Scene {
            name: "3d-11-inbox-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.followed_unread.set(3);
                reach_chat(cx, model, cast, rec, true);
                inbox_scene(cx, model, rec, win, inbox_entries(cast), true);
                press(cx, win, "down");
            },
        },
        Scene {
            name: "3d-12-prefs-notifications-light",
            width: 1100.0,
            height: 1000.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
            },
        },
        Scene {
            name: "3d-13-prefs-quiet-hours-dark",
            width: 1100.0,
            height: 1000.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.quiet.set(Some(lazy_domain::notification::QuietHours {
                    start_minute: 22 * 60,
                    end_minute: 7 * 60,
                    utc_offset_minutes: -420,
                    days: 0b0011_1111,
                }));
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
            },
        },
        Scene {
            name: "3d-14-prefs-paused-light",
            width: 1100.0,
            height: 1000.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.dnd_until.set(Some(Timestamp::from(
                    time::OffsetDateTime::now_utc() + time::Duration::minutes(45),
                )));
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
            },
        },
        Scene {
            name: "3d-15-prefs-quiet-problem-dark",
            width: 1100.0,
            height: 1000.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.quiet.set(Some(lazy_domain::notification::QuietHours {
                    start_minute: 22 * 60,
                    end_minute: 7 * 60,
                    utc_offset_minutes: -420,
                    days: lazy_domain::notification::QuietHours::EVERY_DAY,
                }));
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
                // Channel, direct, replies, @channel, desktop, text, pause, quiet, then the start.
                press(cx, win, "tab tab tab tab tab tab tab tab ctrl-a");
                type_text(cx, win, "9pm");
            },
        },
        Scene {
            name: "3d-16-prefs-status-light",
            width: 1100.0,
            height: 1000.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                *cast.my_status.borrow_mut() = Some(
                    lazy_domain::notification::UserStatus::new(
                        "Heads down on the launch",
                        lazy_domain::ReactionEmoji::parse("🎯").ok(),
                        None,
                    )
                    .unwrap(),
                );
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
            },
        },
        Scene {
            name: "3d-17-prefs-status-error-dark",
            width: 1100.0,
            height: 1000.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
                // To the status field, then more than a status can hold, then Save.
                press(cx, win, "tab tab tab tab tab tab tab tab tab");
                type_text(cx, win, &"x".repeat(101));
                press(cx, win, "tab tab enter");
            },
        },
        Scene {
            name: "3d-18-prefs-pause-menu-light",
            width: 1100.0,
            height: 1000.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
                press(cx, win, "tab tab tab tab tab tab enter down");
            },
        },
        Scene {
            name: "3d-19-prefs-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.dnd_until.set(Some(Timestamp::from(
                    time::OffsetDateTime::now_utc() + time::Duration::minutes(45),
                )));
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
            },
        },
        Scene {
            name: "3d-20-status-in-timeline-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                set_other_statuses(cast);
                reach_chat_with(cx, model, cast, rec, cast.mentions_history());
            },
        },
        Scene {
            name: "3d-21-status-in-mention-list-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                set_other_statuses(cast);
                reach_chat_with(cx, model, cast, rec, cast.mentions_history());
                type_text(cx, win, "@");
            },
        },
        Scene {
            name: "3d-22-status-in-people-picker-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                set_other_statuses(cast);
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::NewMessage);
            },
        },
        // ---- Slice 3E: discovery ----
        Scene {
            name: "3e-01-browse-filled-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::BrowseChannels);
                let mut archived =
                    cast.channel(ConversationId::generate(), "old-launch", "Retired", false);
                archived.archived = true;
                let mut crowded = cast.channel(
                    ConversationId::generate(),
                    "announcements",
                    "What the company wants everyone to know",
                    false,
                );
                crowded.member_count = 412;
                answer_browse(
                    cx,
                    model,
                    rec,
                    vec![
                        BrowsedChannel { conversation: crowded, joined: true },
                        BrowsedChannel { conversation: cast.channel(cast.general, "general", "Company-wide announcements and chatter", false), joined: true },
                        BrowsedChannel { conversation: cast.channel(ConversationId::generate(), "random", "Anything goes, within reason", false), joined: false },
                        BrowsedChannel { conversation: cast.channel(ConversationId::generate(), "support", "Customer questions and escalations, and the long-running thread about the ticket backlog", false), joined: false },
                        BrowsedChannel { conversation: archived, joined: false },
                    ],
                    true,
                );
                press(cx, win, "down down");
            },
        },
        Scene {
            name: "3e-02-browse-not-joined-search-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::BrowseChannels);
                answer_browse(cx, model, rec, Vec::new(), false);
                press(cx, win, "tab right");
                answer_browse(
                    cx,
                    model,
                    rec,
                    vec![
                        BrowsedChannel {
                            conversation: cast.channel(
                                ConversationId::generate(),
                                "random",
                                "Anything goes, within reason",
                                false,
                            ),
                            joined: false,
                        },
                        BrowsedChannel {
                            conversation: cast.channel(
                                ConversationId::generate(),
                                "support",
                                "Customer questions and escalations",
                                false,
                            ),
                            joined: false,
                        },
                    ],
                    false,
                );
                press(cx, win, "tab right");
                answer_browse(
                    cx,
                    model,
                    rec,
                    vec![
                        BrowsedChannel {
                            conversation: cast.channel(
                                ConversationId::generate(),
                                "support",
                                "Customer questions and escalations",
                                false,
                            ),
                            joined: false,
                        },
                        BrowsedChannel {
                            conversation: cast.channel(
                                ConversationId::generate(),
                                "random",
                                "Anything goes, within reason",
                                false,
                            ),
                            joined: false,
                        },
                    ],
                    false,
                );
            },
        },
        Scene {
            name: "3e-03-browse-empty-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::BrowseChannels);
                answer_browse(cx, model, rec, Vec::new(), false);
                shift_focus_to_search_and_type(cx, win, "zebra");
                answer_browse(cx, model, rec, Vec::new(), false);
            },
        },
        Scene {
            name: "3e-04-browse-loading-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::BrowseChannels);
                draw(cx, win);
            },
        },
        Scene {
            name: "3e-05-browse-failed-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::BrowseChannels);
                let generation = rec
                    .0
                    .borrow()
                    .iter()
                    .rev()
                    .find_map(|effect| match effect {
                        Effect::LoadBrowse { generation, .. } => Some(*generation),
                        _ => None,
                    })
                    .expect("the directory was asked for");
                dispatch(
                    cx,
                    model,
                    Input::BrowseLoaded {
                        generation,
                        result: Err(RequestError::Unknown("connection reset".into())),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3e-06-people-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                set_other_statuses(cast);
                reach_chat(cx, model, cast, rec, true);
                people_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::PeoplePage {
                        people: people_page(cast),
                        has_more: true,
                    }),
                );
                press(cx, win, "down down");
            },
        },
        Scene {
            name: "3e-07-people-search-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                set_other_statuses(cast);
                reach_chat(cx, model, cast, rec, true);
                people_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::PeoplePage {
                        people: people_page(cast),
                        has_more: false,
                    }),
                );
                type_text(cx, win, "o");
                let matching: Vec<_> = people_page(cast)
                    .into_iter()
                    .filter(|p| p.user.display_name.as_str().to_lowercase().contains('o'))
                    .collect();
                answer_people(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::PeoplePage {
                        people: matching,
                        has_more: false,
                    }),
                );
            },
        },
        Scene {
            name: "3e-08-people-empty-and-loading-light",
            width: 900.0,
            height: 600.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                people_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::PeoplePage {
                        people: Vec::new(),
                        has_more: false,
                    }),
                );
                type_text(cx, win, "qq");
                answer_people(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::PeoplePage {
                        people: Vec::new(),
                        has_more: false,
                    }),
                );
            },
        },
        Scene {
            name: "3e-09-profile-admin-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                set_other_statuses(cast);
                reach_chat(cx, model, cast, rec, true);
                let person = people_page(cast).into_iter().nth(1).unwrap();
                profile_scene(
                    cx,
                    model,
                    rec,
                    win,
                    cast.bob.id,
                    Some(Ok(lazy_protocol::PersonResponse { person })),
                );
            },
        },
        Scene {
            name: "3e-10-profile-loading-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                set_other_statuses(cast);
                reach_chat(cx, model, cast, rec, true);
                profile_scene(cx, model, rec, win, cast.zoe.id, None);
            },
        },
        Scene {
            name: "3e-11-profile-self-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                *cast.my_status.borrow_mut() = Some(
                    lazy_domain::notification::UserStatus::new(
                        "In a meeting until 3",
                        lazy_domain::ReactionEmoji::parse("📅").ok(),
                        None,
                    )
                    .unwrap(),
                );
                reach_chat(cx, model, cast, rec, true);
                let person = people_page(cast).into_iter().next().unwrap();
                profile_scene(
                    cx,
                    model,
                    rec,
                    win,
                    cast.me.id,
                    Some(Ok(lazy_protocol::PersonResponse { person })),
                );
            },
        },
        Scene {
            name: "3e-12-profile-failed-dark",
            width: 900.0,
            height: 600.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                profile_scene(
                    cx,
                    model,
                    rec,
                    win,
                    cast.carol.id,
                    Some(Err(RequestError::Unknown("connection reset".into()))),
                );
            },
        },
        Scene {
            name: "3e-13-switcher-empty-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.archived_member.set(true);
                reach_chat(cx, model, cast, rec, true);
                visit(cx, model, cast, rec);
                draw(cx, win);
                open_dialog(cx, win, ShellDialog::QuickSwitcher);
            },
        },
        Scene {
            name: "3e-14-switcher-query-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.archived_member.set(true);
                cast.muted_group.set(true);
                set_other_statuses(cast);
                reach_chat(cx, model, cast, rec, true);
                visit(cx, model, cast, rec);
                draw(cx, win);
                open_dialog(cx, win, ShellDialog::QuickSwitcher);
                type_text(cx, win, "o");
                press(cx, win, "down down");
            },
        },
        Scene {
            name: "3e-15-switcher-no-match-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::QuickSwitcher);
                type_text(cx, win, "zzqx");
            },
        },
        Scene {
            name: "3e-16-switcher-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.archived_member.set(true);
                reach_chat(cx, model, cast, rec, true);
                visit(cx, model, cast, rec);
                draw(cx, win);
                open_dialog(cx, win, ShellDialog::QuickSwitcher);
                type_text(cx, win, "e");
            },
        },
        Scene {
            name: "3e-17-saved-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                saved_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::SavedPage {
                        items: saved_items(cast),
                        has_more: true,
                    }),
                );
                press(cx, win, "down down");
            },
        },
        Scene {
            name: "3e-18-saved-empty-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                saved_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::SavedPage {
                        items: Vec::new(),
                        has_more: false,
                    }),
                );
            },
        },
        Scene {
            name: "3e-19-saved-failed-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                saved_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Err(RequestError::Unknown("connection reset".into())),
                );
            },
        },
        Scene {
            name: "3e-20-saved-loading-dark",
            width: 900.0,
            height: 600.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                draw(cx, win);
                window_shell_do(cx, win, |shell, window, cx| shell.open_saved(window, cx));
            },
        },
        Scene {
            name: "3e-21-saved-marker-and-menu-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                set_other_statuses(cast);
                let history = cast.mentions_history();
                let saved_id = history[1].id;
                reach_chat_with(cx, model, cast, rec, history);
                dispatch(
                    cx,
                    model,
                    Input::SetMessageSaved {
                        message_id: saved_id,
                        saved: true,
                    },
                );
                draw(cx, win);
                press(cx, win, "shift-tab up up enter");
            },
        },
        Scene {
            name: "3e-22-details-members-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                set_other_statuses(cast);
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Details(cast.group));
            },
        },
        // ---- Slice 3F: workspace administration ----
        Scene {
            name: "3f-01-settings-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::WorkspaceSettings);
            },
        },
        Scene {
            name: "3f-02-settings-edited-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::WorkspaceSettings);
                press(cx, win, "ctrl-a backspace");
                type_text(cx, win, "Lazy Labs");
                press(cx, win, "tab right tab right");
            },
        },
        Scene {
            name: "3f-03-settings-refused-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.emoji_mode.set(EmojiMode::EmptyAsAdmin);
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::WorkspaceSettings);
                type_text(cx, win, " Two");
                press(cx, win, "enter");
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::UpdateWorkspace,
                        result: Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Forbidden,
                            "Only admins and owners can change the workspace.",
                        ))),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "4a-01-integrations-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                integrations_scene(cx, model, cast, win, Some(workspace_webhooks(cast)));
            },
        },
        Scene {
            name: "4a-02-integrations-log-dark",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                let list = workspace_webhooks(cast);
                let id = list[1].webhook.id;
                integrations_scene(cx, model, cast, win, Some(list));
                with_integrations(cx, win, |dialog, window, cx| {
                    dialog.open_details(id, window, cx)
                });
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::LoadWebhookDeliveries(id),
                        result: Ok(ActionOutcome::WebhookDeliveries(webhook_log())),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "4a-03-integrations-adding-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                integrations_scene(cx, model, cast, win, Some(workspace_webhooks(cast)));
                with_integrations(cx, win, |dialog, window, cx| {
                    dialog.start_adding(window, cx);
                    dialog
                        .name_input()
                        .update(cx, |input, cx| input.set_text("Build status", cx));
                });
            },
        },
        Scene {
            name: "4a-04-integrations-new-address-dark",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                let list = workspace_webhooks(cast);
                let webhook = list[0].webhook.clone();
                let id = webhook.id;
                integrations_scene(cx, model, cast, win, Some(list));
                with_integrations(cx, win, |dialog, window, cx| {
                    dialog.open_details(id, window, cx)
                });
                // Yes to the confirmation, then the server's answer.
                with_integrations(cx, win, |dialog, window, cx| {
                    dialog.show_confirm_new_address(id, window, cx)
                });
                dispatch(cx, model, Input::NewWebhookAddress(id));
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::NewWebhookAddress(id),
                        result: Ok(ActionOutcome::WebhookCreated(
                            lazy_client_core::CreatedWebhook {
                                webhook,
                                url: lazy_client_core::SecretUrl::new(
                                    "https://chat.example.com/hooks/0192f1d2-77aa-7c3e-9a51-2b6c4f0e8d13/\
                                     5f2a9c0e7b4d16a8c3e9f0b2d7a64e1c9b8f3a2d5e6c7b8a9f0e1d2c3b4a5f6e"
                                        .to_owned(),
                                ),
                            },
                        )),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "4a-05-integrations-empty-narrow-light",
            width: 560.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                integrations_scene(cx, model, cast, win, Some(Vec::new()));
            },
        },
        Scene {
            name: "4d-01-integrations-connectors-light",
            width: 1100.0,
            height: 1100.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                use lazy_domain::connector::ConnectorProvider::{Jira, Linear};
                reach_chat(cx, model, cast, rec, true);
                connector_scene(cx, model, cast, win, &[(Linear, 3), (Jira, 2)], None, None);
            },
        },
        Scene {
            name: "4d-02-linear-setup-dark",
            width: 1100.0,
            height: 1100.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                use lazy_domain::connector::ConnectorProvider::Linear;
                reach_chat(cx, model, cast, rec, true);
                connector_scene(cx, model, cast, win, &[(Linear, 2)], Some(Linear), None);
            },
        },
        Scene {
            name: "4d-03-linear-connected-light",
            width: 1100.0,
            height: 1200.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                use lazy_domain::connector::ConnectorProvider::Linear;
                reach_chat(cx, model, cast, rec, true);
                connector_scene(cx, model, cast, win, &[(Linear, 3)], Some(Linear), None);
            },
        },
        Scene {
            name: "4d-04-jira-secret-made-light",
            width: 1100.0,
            height: 1100.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                use lazy_domain::connector::ConnectorProvider::Jira;
                reach_chat(cx, model, cast, rec, true);
                connector_scene(
                    cx,
                    model,
                    cast,
                    win,
                    &[(Jira, 2)],
                    Some(Jira),
                    Some("9b1f0c7e3a5d2b8f4e6c1a0d7b3e5f2c9a8d6b4e1f3c5a7b9d0e2f4a6c8b1d3e"),
                );
            },
        },
        Scene {
            name: "4d-05-linear-adding-dark",
            width: 1100.0,
            height: 1000.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                use lazy_domain::connector::ConnectorProvider::Linear;
                reach_chat(cx, model, cast, rec, true);
                connector_scene(cx, model, cast, win, &[(Linear, 3)], Some(Linear), None);
                with_integrations(cx, win, |dialog, window, cx| {
                    if let Some(panel) = dialog.connector_panel(Linear) {
                        panel.update(cx, |panel, cx| panel.start_adding(window, cx));
                    }
                });
                draw(cx, win);
            },
        },
        Scene {
            name: "4e-01-apps-list-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                apps_scene(cx, model, cast, win, 0);
            },
        },
        Scene {
            name: "4e-02-app-token-dark",
            width: 1100.0,
            height: 1100.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                apps_scene(cx, model, cast, win, 1);
            },
        },
        Scene {
            name: "4e-03-app-making-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                apps_scene(cx, model, cast, win, 2);
            },
        },
        Scene {
            name: "4e-04-app-commands-light",
            width: 1100.0,
            height: 1700.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                apps_scene(cx, model, cast, win, 0);
                let id = sample_app_ids().0;
                with_integrations(cx, win, |dialog, window, cx| {
                    if let Some(panel) = dialog.apps_panel() {
                        panel.update(cx, |panel, cx| panel.show_details(id, window, cx));
                    }
                });
                draw(cx, win);
            },
        },
        Scene {
            name: "4e-05-command-reply-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                command_reply_scene(cx, model, cast, win);
            },
        },
        Scene {
            name: "4f-01-block-kit-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.block_kit_history());
                draw(cx, win);
            },
        },
        Scene {
            name: "4f-02-block-kit-dark",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.block_kit_history());
                draw(cx, win);
            },
        },
        Scene {
            name: "4f-03-block-kit-narrow-light",
            width: 640.0,
            height: 1000.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.block_kit_history());
                draw(cx, win);
            },
        },
        Scene {
            name: "4f-04-block-kit-pressed-dark",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let history = cast.block_kit_history();
                let message = history[1].id;
                reach_chat_with(cx, model, cast, rec, history);
                // Never answered here: the button waits, and the reply box says so.
                dispatch(
                    cx,
                    model,
                    Input::PressButton {
                        conversation: cast.general,
                        message,
                        block: 3,
                        action_id: "approve".into(),
                        label: "Approve".into(),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "4f-05-block-kit-reply-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let history = cast.block_kit_history();
                let message = history[2].id;
                reach_chat_with(cx, model, cast, rec, history);
                dispatch(
                    cx,
                    model,
                    Input::PressButton {
                        conversation: cast.general,
                        message,
                        block: 0,
                        action_id: "ack".into(),
                        label: "Acknowledge".into(),
                    },
                );
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::RunCommand(cast.general),
                        result: Ok(ActionOutcome::CommandRan(Box::new(
                            lazy_protocol::RunCommandResponse {
                                app_name: "Deploy bot".into(),
                                reply: Some(
                                    MessageDocument::from_markdown(
                                        "Acknowledged. You're on it until *11:40*.",
                                    )
                                    .unwrap(),
                                ),
                                posted: None,
                            },
                        ))),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "4g-01-link-previews-light",
            width: 1100.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| link_previews_scene(cx, model, cast, rec, win),
        },
        Scene {
            name: "4g-02-link-previews-dark",
            width: 1100.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| link_previews_scene(cx, model, cast, rec, win),
        },
        Scene {
            name: "4g-03-link-previews-narrow-light",
            width: 640.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| link_previews_scene(cx, model, cast, rec, win),
        },
        Scene {
            name: "4c-01-github-feed-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.github_history());
                draw(cx, win);
            },
        },
        Scene {
            name: "4c-02-github-feed-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat_with(cx, model, cast, rec, cast.github_history());
                draw(cx, win);
            },
        },
        Scene {
            name: "4b-01-integrations-github-card-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                github_scene(cx, model, cast, win, 3, false);
            },
        },
        Scene {
            name: "4b-02-github-connect-dark",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                github_scene(cx, model, cast, win, 1, true);
            },
        },
        Scene {
            name: "4b-03-github-not-installed-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                github_scene(cx, model, cast, win, 2, true);
            },
        },
        Scene {
            name: "4b-04-github-installed-light",
            width: 1100.0,
            height: 1000.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                github_scene(cx, model, cast, win, 3, true);
            },
        },
        Scene {
            name: "4b-05-github-adding-dark",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                github_scene(cx, model, cast, win, 3, true);
                win.update(cx, |root, window, app| {
                    let shell = root.shell().expect("the shell").clone();
                    let dialog = shell.read(app).integrations_dialog().expect("open");
                    let panel = dialog.read(app).github_panel().expect("the GitHub view");
                    panel.update(app, |panel, cx| {
                        panel.start_adding(window, cx);
                        panel.repository_input().update(cx, |input, cx| {
                            input.set_text("last-refuge/lazy-website", cx)
                        });
                    });
                })
                .expect("the window");
                draw(cx, win);
            },
        },
        Scene {
            name: "4b-06-github-unavailable-narrow-light",
            width: 560.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                github_scene(cx, model, cast, win, 0, true);
            },
        },
        Scene {
            name: "3f-04-invitations-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                invitations_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Some(Ok(lazy_protocol::InvitationList {
                        invitations: invitations_list(cast),
                    })),
                );
            },
        },
        Scene {
            name: "3f-05-invitation-created-dark",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                invitations_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Some(Ok(lazy_protocol::InvitationList {
                        invitations: invitations_list(cast),
                    })),
                );
                dispatch(
                    cx,
                    model,
                    Input::CreateInvitation {
                        expires_in_secs: Some(60 * 60 * 24 * 7),
                        max_uses: Some(5),
                    },
                );
                let invitation = invite(
                    cast,
                    &cast.me,
                    lazy_protocol::InvitationState::Active,
                    0,
                    Some(5),
                    0,
                    60 * 24 * 7,
                );
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::CreateInvitation,
                        result: Ok(ActionOutcome::InvitationCreated(
                            lazy_protocol::CreatedInvitation {
                                invitation,
                                code: lazy_protocol::InviteCode::new("Qx7mR2vB9tLw3KpZ8cYd5N"),
                            },
                        )),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3f-06-invitations-empty-light",
            width: 900.0,
            height: 700.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                invitations_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Some(Ok(lazy_protocol::InvitationList {
                        invitations: Vec::new(),
                    })),
                );
            },
        },
        Scene {
            name: "3f-07-invitations-failed-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                invitations_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Some(Err(RequestError::Unknown("connection reset".into()))),
                );
            },
        },
        Scene {
            name: "3f-08-invitation-revoke-confirm-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                invitations_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Some(Ok(lazy_protocol::InvitationList {
                        invitations: invitations_list(cast),
                    })),
                );
                press(cx, win, "tab tab tab tab enter");
            },
        },
        Scene {
            name: "3f-09-invitations-loading-light",
            width: 900.0,
            height: 700.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                invitations_scene(cx, model, rec, win, None);
            },
        },
        Scene {
            name: "3f-10-profile-admin-light",
            width: 1100.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                set_other_statuses(cast);
                reach_chat(cx, model, cast, rec, true);
                let person = people_page(cast).into_iter().nth(2).unwrap();
                profile_scene(
                    cx,
                    model,
                    rec,
                    win,
                    cast.carol.id,
                    Some(Ok(lazy_protocol::PersonResponse { person })),
                );
            },
        },
        Scene {
            name: "3f-11-profile-deactivate-confirm-dark",
            width: 1100.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                let person = people_page(cast).into_iter().nth(2).unwrap();
                profile_scene(
                    cx,
                    model,
                    rec,
                    win,
                    cast.carol.id,
                    Some(Ok(lazy_protocol::PersonResponse { person })),
                );
                press(cx, win, "shift-tab enter");
            },
        },
        Scene {
            name: "3f-12-profile-deactivated-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.deactivated.borrow_mut().push("carol");
                reach_chat(cx, model, cast, rec, true);
                let mut person = people_page(cast).into_iter().nth(2).unwrap();
                person.user.deactivated = true;
                profile_scene(
                    cx,
                    model,
                    rec,
                    win,
                    cast.carol.id,
                    Some(Ok(lazy_protocol::PersonResponse { person })),
                );
            },
        },
        Scene {
            name: "3f-13-profile-last-owner-refused-light",
            width: 1100.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                let person = people_page(cast).into_iter().next().unwrap();
                profile_scene(
                    cx,
                    model,
                    rec,
                    win,
                    cast.me.id,
                    Some(Ok(lazy_protocol::PersonResponse { person })),
                );
                dispatch(
                    cx,
                    model,
                    Input::SetMemberRole {
                        user_id: cast.me.id,
                        role: WorkspaceRole::Member,
                    },
                );
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::SetMemberRole(cast.me.id),
                        result: Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Conflict,
                            "A workspace needs at least one owner.",
                        ))),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3f-14-people-with-deactivated-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                people_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::PeoplePage {
                        people: people_page(cast),
                        has_more: false,
                    }),
                );
                press(cx, win, "tab enter");
                answer_people(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::PeoplePage {
                        people: people_with_deactivated(cast),
                        has_more: false,
                    }),
                );
            },
        },
        Scene {
            name: "3f-15-people-with-deactivated-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                people_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::PeoplePage {
                        people: people_page(cast),
                        has_more: false,
                    }),
                );
                press(cx, win, "tab enter");
                answer_people(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::PeoplePage {
                        people: people_with_deactivated(cast),
                        has_more: false,
                    }),
                );
                press(cx, win, "tab");
            },
        },
        Scene {
            name: "3f-16-register-with-code-light",
            width: 900.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let _ = (cast, rec);
                dispatch(
                    cx,
                    model,
                    Input::Started(StartupState {
                        session_restored: false,
                        ..Default::default()
                    }),
                );
                draw(cx, win);
                press(cx, win, "tab tab tab enter");
                press(cx, win, "shift-tab shift-tab shift-tab shift-tab shift-tab");
                type_text(cx, win, "newbie");
                press(cx, win, "tab");
                type_text(cx, win, "Nora Newbie");
                press(cx, win, "tab");
                type_text(cx, win, "correct horse");
                press(cx, win, "tab");
                type_text(cx, win, "Qx7mR2vB9tLw3KpZ8cYd5N");
            },
        },
        Scene {
            name: "3f-17-register-code-refused-dark",
            width: 900.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                let _ = (cast, rec);
                dispatch(
                    cx,
                    model,
                    Input::Started(StartupState {
                        session_restored: false,
                        ..Default::default()
                    }),
                );
                draw(cx, win);
                press(cx, win, "tab tab tab enter");
                press(cx, win, "shift-tab shift-tab shift-tab shift-tab shift-tab");
                type_text(cx, win, "newbie");
                press(cx, win, "tab");
                type_text(cx, win, "Nora Newbie");
                press(cx, win, "tab");
                type_text(cx, win, "correct horse");
                press(cx, win, "tab");
                type_text(cx, win, "Qx7mR2vB9tLw3KpZ8cYd5N");
                press(cx, win, "enter");
                dispatch(
                    cx,
                    model,
                    Input::AuthFailed(RequestError::Rejected(ApiError::new(
                        ErrorCode::InvitationInvalid,
                        "That invitation code is not valid.",
                    ))),
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3f-18-no-access-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                lose_access(cx, model, cast);
                draw(cx, win);
            },
        },
        Scene {
            name: "3f-19-no-access-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                lose_access(cx, model, cast);
                draw(cx, win);
            },
        },
        Scene {
            name: "3f-20-sidebar-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat(cx, model, cast, rec, true);
            },
        },
        Scene {
            name: "3f-21-sidebar-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| {
                cast.emoji_mode.set(EmojiMode::EmptyAsAdmin);
                reach_chat(cx, model, cast, rec, true);
            },
        },
        Scene {
            name: "3f-22-channels-menu-admins-only-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.emoji_mode.set(EmojiMode::EmptyAsMember);
                cast.workspace_settings
                    .set(lazy_domain::workspace::WorkspaceSettings {
                        channel_creation: lazy_domain::workspace::ChannelCreationPolicy::AdminsOnly,
                        ..Default::default()
                    });
                reach_chat(cx, model, cast, rec, true);
                window_shell_do(cx, win, |shell, window, cx| {
                    let plus = shell.sidebar().read(cx).channels_plus_focus();
                    window.focus(&plus, cx);
                });
                press(cx, win, "enter");
            },
        },
        // ---- Slice 3G: workspace rail, profile pictures, appearance ----
        Scene {
            name: "3g-01-rail-one-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "3g-02-rail-three-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                cast.more_workspaces.set(true);
                reach_chat(cx, model, cast, rec, true);
            },
        },
        Scene {
            name: "3g-03-rail-three-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| {
                cast.more_workspaces.set(true);
                reach_chat(cx, model, cast, rec, true);
            },
        },
        Scene {
            name: "3g-04-rail-three-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                cast.more_workspaces.set(true);
                reach_chat(cx, model, cast, rec, true);
            },
        },
        Scene {
            name: "3g-05-rail-keyboard-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.more_workspaces.set(true);
                reach_chat(cx, model, cast, rec, true);
                window_shell_do(cx, win, |shell, window, cx| {
                    let focus = shell.rail().read(cx).focus();
                    window.focus(&focus, cx);
                });
                draw(cx, win);
                press(cx, win, "down");
            },
        },
        Scene {
            name: "3g-06-add-workspace-create-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.more_workspaces.set(true);
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::AddWorkspace);
                type_text(cx, win, "Book Club");
            },
        },
        Scene {
            name: "3g-07-add-workspace-join-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::AddWorkspace);
                press(cx, win, "shift-tab right tab");
                type_text(cx, win, "lazy-9f3kq2m7vx8d4ztb");
            },
        },
        Scene {
            name: "3g-08-add-workspace-joining-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::AddWorkspace);
                press(cx, win, "shift-tab right tab");
                type_text(cx, win, "lazy-9f3kq2m7vx8d4ztb");
                press(cx, win, "enter");
            },
        },
        Scene {
            name: "3g-09-add-workspace-refused-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::AddWorkspace);
                press(cx, win, "shift-tab right tab");
                type_text(cx, win, "lazy-9f3kq2m7vx8d4ztb");
                press(cx, win, "enter");
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::AcceptInvitation,
                        result: Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::NotFound,
                            "That invitation isn't valid any more. Ask the person who sent it for a new one.",
                        ))),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3g-10-prefs-profile-initial-light",
            width: 1000.0,
            height: 1800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
            },
        },
        Scene {
            name: "3g-11-prefs-profile-picture-dark",
            width: 1000.0,
            height: 1800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.avatars.borrow_mut().push("alice");
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
            },
        },
        Scene {
            name: "3g-12-prefs-picture-refused-light",
            width: 1000.0,
            height: 1800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
                dispatch(
                    cx,
                    model,
                    Input::SetAvatar {
                        source: lazy_client_core::FileSource::Path(
                            "/home/alice/holiday.tiff".into(),
                        ),
                    },
                );
                draw(cx, win);
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::SetAvatar,
                        result: Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::InvalidRequest,
                            "That picture can't be used. Choose a PNG, JPEG, WebP or GIF under 5 MB.",
                        ))),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3g-13-prefs-uploading-dark",
            width: 1000.0,
            height: 1800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.avatars.borrow_mut().push("alice");
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
                dispatch(
                    cx,
                    model,
                    Input::SetAvatar {
                        source: lazy_client_core::FileSource::Path("/home/alice/me.png".into()),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "3g-14-prefs-appearance-dark-on-light-system",
            width: 1000.0,
            height: 1800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
                dispatch(
                    cx,
                    model,
                    Input::SetAppearance(lazy_client_core::Appearance {
                        theme: lazy_client_core::ThemeChoice::Dark,
                        motion: lazy_client_core::MotionChoice::Reduce,
                    }),
                );
                draw(cx, win);
                // The keyboard walks to the last control, scrolling the Appearance section in.
                for _ in 0..18 {
                    press(cx, win, "tab");
                }
            },
        },
        Scene {
            name: "3g-15-prefs-narrow-profile-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.avatars.borrow_mut().push("alice");
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Preferences);
                // Down to the Profile section with the keyboard.
                for _ in 0..16 {
                    press(cx, win, "tab");
                }
            },
        },
        Scene {
            name: "3g-16-timeline-avatars-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                cast.more_workspaces.set(true);
                cast.avatars
                    .borrow_mut()
                    .extend(["alice", "bob", "carol", "zoe"]);
                reach_chat(cx, model, cast, rec, true);
            },
        },
        Scene {
            name: "3g-17-timeline-avatars-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| {
                cast.avatars
                    .borrow_mut()
                    .extend(["alice", "bob", "carol", "zoe"]);
                reach_chat(cx, model, cast, rec, true);
            },
        },
        Scene {
            name: "3g-18-people-avatars-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.avatars
                    .borrow_mut()
                    .extend(["alice", "bob", "carol", "eli", "fatima", "zoe"]);
                set_other_statuses(cast);
                reach_chat(cx, model, cast, rec, true);
                people_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::PeoplePage {
                        people: people_page(cast),
                        has_more: false,
                    }),
                );
            },
        },
        Scene {
            name: "3g-19-profile-avatar-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.avatars.borrow_mut().extend(["alice", "bob"]);
                set_other_statuses(cast);
                reach_chat(cx, model, cast, rec, true);
                let person = people_page(cast).into_iter().nth(1).unwrap();
                profile_scene(
                    cx,
                    model,
                    rec,
                    win,
                    cast.bob.id,
                    Some(Ok(lazy_protocol::PersonResponse { person })),
                );
            },
        },
        Scene {
            name: "3g-20-switcher-avatars-light",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.avatars
                    .borrow_mut()
                    .extend(["alice", "bob", "carol", "zoe"]);
                reach_chat(cx, model, cast, rec, true);
                visit(cx, model, cast, rec);
                open_dialog(cx, win, ShellDialog::QuickSwitcher);
                type_text(cx, win, "o");
            },
        },
        Scene {
            name: "3g-21-details-members-avatars-dark",
            width: 1100.0,
            height: 760.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                cast.avatars
                    .borrow_mut()
                    .extend(["alice", "bob", "carol", "zoe"]);
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Details(cast.general));
            },
        },
        // ---- Slice 3H: commands, menus, shortcuts, diagnostics ----
        Scene {
            name: "3h-01-menu-file-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                window_shell_do(cx, win, |shell, window, cx| {
                    shell.open_command_menu(window, cx)
                });
                draw(cx, win);
            },
        },
        Scene {
            name: "3h-02-menu-go-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                window_shell_do(cx, win, |shell, window, cx| {
                    shell.open_command_menu(window, cx)
                });
                press(cx, win, "right down down");
            },
        },
        Scene {
            name: "3h-03-menu-workspace-member-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.emoji_mode.set(EmojiMode::EmptyAsMember);
                reach_chat(cx, model, cast, rec, true);
                window_shell_do(cx, win, |shell, window, cx| {
                    shell.open_command_menu(window, cx)
                });
                press(cx, win, "right right right");
            },
        },
        Scene {
            name: "3h-04-menu-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                window_shell_do(cx, win, |shell, window, cx| {
                    shell.open_command_menu(window, cx)
                });
                press(cx, win, "right");
            },
        },
        Scene {
            name: "3h-05-shortcuts-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Shortcuts);
            },
        },
        Scene {
            name: "3h-06-shortcuts-dark",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Shortcuts);
                press(cx, win, "end");
            },
        },
        Scene {
            name: "3h-07-shortcuts-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::Shortcuts);
            },
        },
        Scene {
            name: "3h-08-palette-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                window_shell_do(cx, win, |shell, window, cx| {
                    shell.open_quick_switcher_with(">", window, cx)
                });
                draw(cx, win);
            },
        },
        Scene {
            name: "3h-09-palette-typed-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                window_shell_do(cx, win, |shell, window, cx| {
                    shell.open_quick_switcher_with(">", window, cx)
                });
                type_text(cx, win, "sh");
                press(cx, win, "down");
            },
        },
        Scene {
            name: "3h-10-palette-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                cast.emoji_mode.set(EmojiMode::EmptyAsMember);
                reach_chat(cx, model, cast, rec, true);
                window_shell_do(cx, win, |shell, window, cx| {
                    shell.open_quick_switcher_with(">", window, cx)
                });
                draw(cx, win);
            },
        },
        Scene {
            name: "3h-11-diagnostics-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                diagnostics_setup(cx, model, cast, rec);
                open_dialog(cx, win, ShellDialog::Diagnostics);
            },
        },
        Scene {
            name: "3h-12-diagnostics-copied-dark",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                diagnostics_setup(cx, model, cast, rec);
                open_dialog(cx, win, ShellDialog::Diagnostics);
                press(cx, win, "enter");
            },
        },
        Scene {
            name: "3h-13-diagnostics-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                diagnostics_setup(cx, model, cast, rec);
                open_dialog(cx, win, ShellDialog::Diagnostics);
            },
        },
        // ---- The server row (dogfood packaging) ----
        Scene {
            name: "srv-01-signin-connected-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, _, cx, rec, win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                draw(cx, win);
                answer_server(cx, model, rec, Ok(lazy_protocol::PROTOCOL_VERSION));
                draw(cx, win);
            },
        },
        Scene {
            name: "srv-02-signin-changing-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, _, cx, rec, win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                draw(cx, win);
                answer_server(cx, model, rec, Ok(lazy_protocol::PROTOCOL_VERSION));
                // Username, password, Sign in, Create an account, then Change….
                press(cx, win, "tab tab tab tab enter");
            },
        },
        Scene {
            name: "srv-03-signin-checking-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, _, cx, rec, win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                draw(cx, win);
                answer_server(cx, model, rec, Ok(lazy_protocol::PROTOCOL_VERSION));
                press(cx, win, "tab tab tab tab enter ctrl-a backspace");
                type_text(cx, win, "chat.example.com");
                press(cx, win, "enter");
            },
        },
        Scene {
            name: "srv-04-signin-unreachable-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, _, cx, rec, win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                draw(cx, win);
                answer_server(cx, model, rec, Ok(lazy_protocol::PROTOCOL_VERSION));
                press(cx, win, "tab tab tab tab enter ctrl-a backspace");
                type_text(cx, win, "chat.example.com");
                press(cx, win, "enter");
                answer_server(cx, model, rec, Err("timed out".into()));
                draw(cx, win);
            },
        },
        Scene {
            name: "srv-05-signin-incompatible-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, _, cx, rec, win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                draw(cx, win);
                answer_server(cx, model, rec, Ok(lazy_protocol::PROTOCOL_VERSION + 1));
                draw(cx, win);
            },
        },
        Scene {
            name: "srv-06-signin-locked-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, _, cx, rec, win| {
                dispatch(
                    cx,
                    model,
                    Input::Started(StartupState {
                        server: lazy_client_core::ServerInfo {
                            url: "http://127.0.0.1:8080".into(),
                            source: lazy_client_core::ServerSource::Environment,
                            trying: None,
                            check: lazy_client_core::ServerCheck::Unchecked,
                        },
                        ..Default::default()
                    }),
                );
                draw(cx, win);
                answer_server(cx, model, rec, Err("connection failed".into()));
                draw(cx, win);
            },
        },
        Scene {
            name: "srv-07-register-hint-light",
            width: 1100.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, _, cx, rec, win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                draw(cx, win);
                answer_server(cx, model, rec, Ok(lazy_protocol::PROTOCOL_VERSION));
                press(cx, win, "tab tab tab enter");
                draw(cx, win);
            },
        },
        Scene {
            name: "srv-08-unreachable-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, _, cx, rec, win| {
                unreachable_session(cx, model, rec);
                draw(cx, win);
            },
        },
        Scene {
            name: "srv-09-unreachable-changing-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, _, cx, rec, win| {
                unreachable_session(cx, model, rec);
                draw(cx, win);
                press(cx, win, "tab enter");
            },
        },
        Scene {
            name: "srv-10-no-access-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                dispatch(
                    cx,
                    model,
                    Input::Started(StartupState {
                        session_restored: true,
                        ..Default::default()
                    }),
                );
                let mut bootstrap = cast.bootstrap();
                bootstrap.workspaces.clear();
                dispatch(cx, model, Input::BootstrapLoaded(Box::new(bootstrap)));
                let _ = rec;
                draw(cx, win);
            },
        },
        Scene {
            name: "srv-11-register-checking-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, _, cx, rec, win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                draw(cx, win);
                press(cx, win, "tab tab tab enter");
                let _ = rec;
                draw(cx, win);
            },
        },
        Scene {
            name: "srv-12-signin-changing-narrow-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, _, cx, rec, win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                draw(cx, win);
                answer_server(cx, model, rec, Err("timed out".into()));
                press(cx, win, "tab tab tab tab enter");
            },
        },
        Scene {
            name: "srv-13-unreachable-narrow-dark",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Dark,
            build: &|model, _, cx, rec, win| {
                unreachable_session(cx, model, rec);
                draw(cx, win);
                press(cx, win, "tab enter");
            },
        },
        // ---- The brand (Soft Signal 1.0.0): logo, Moss, palette, fonts ----
        Scene {
            name: "brand-01-signin-light",
            width: 1100.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, _, cx, rec, win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                draw(cx, win);
                answer_server(cx, model, rec, Ok(lazy_protocol::PROTOCOL_VERSION));
                draw(cx, win);
            },
        },
        Scene {
            name: "brand-02-signin-dark",
            width: 1100.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, _, cx, rec, win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                draw(cx, win);
                answer_server(cx, model, rec, Ok(lazy_protocol::PROTOCOL_VERSION));
                draw(cx, win);
            },
        },
        Scene {
            name: "brand-03-register-light",
            width: 1100.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, _, cx, rec, win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                draw(cx, win);
                answer_server(cx, model, rec, Ok(lazy_protocol::PROTOCOL_VERSION));
                press(cx, win, "tab tab tab enter");
                draw(cx, win);
            },
        },
        Scene {
            name: "brand-04-welcome-moss-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, _, win| {
                dispatch(
                    cx,
                    model,
                    Input::Started(StartupState {
                        session_restored: true,
                        ..Default::default()
                    }),
                );
                let mut bootstrap = cast.bootstrap();
                bootstrap.workspaces[0].conversations.clear();
                bootstrap.workspaces[0].user_state.clear();
                dispatch(cx, model, Input::BootstrapLoaded(Box::new(bootstrap)));
                dispatch(cx, model, Input::Clock(Timestamp::now()));
                draw(cx, win);
            },
        },
        Scene {
            name: "brand-05-inbox-caught-up-moss-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                inbox_scene(cx, model, rec, win, vec![], false);
            },
        },
        Scene {
            name: "brand-06-saved-empty-moss-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                saved_scene(
                    cx,
                    model,
                    rec,
                    win,
                    Ok(lazy_protocol::SavedPage {
                        items: Vec::new(),
                        has_more: false,
                    }),
                );
            },
        },
        Scene {
            name: "brand-07-main-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                cast.more_workspaces.set(true);
                reach_chat(cx, model, cast, rec, true);
            },
        },
        Scene {
            name: "brand-08-main-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| {
                cast.more_workspaces.set(true);
                reach_chat(cx, model, cast, rec, true);
            },
        },
        Scene {
            name: "brand-09-add-workspace-moss-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::AddWorkspace);
            },
        },
        Scene {
            name: "brand-10-unreachable-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, _, cx, rec, win| {
                unreachable_session(cx, model, rec);
                draw(cx, win);
            },
        },
        // ---- The first run (D29): from the address to the first conversation ----
        Scene {
            name: "first-01-connect-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Connect),
        },
        Scene {
            name: "first-02-connect-dark",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Connect),
        },
        Scene {
            name: "first-03-connect-narrow-light",
            width: 560.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Connect),
        },
        Scene {
            name: "first-04-connect-checking-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                first_run(cx, model, cast, rec, win, Upto::Checking);
                draw(cx, win);
            },
        },
        Scene {
            name: "first-05-connect-unreachable-dark",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                first_run(cx, model, cast, rec, win, Upto::Unreachable)
            },
        },
        Scene {
            name: "first-06-connect-bad-code-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::BadCode),
        },
        Scene {
            name: "first-07-invited-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Invited),
        },
        Scene {
            name: "first-08-invited-dark",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Invited),
        },
        Scene {
            name: "first-09-invited-narrow-light",
            width: 560.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Invited),
        },
        Scene {
            name: "first-10-account-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                first_run(cx, model, cast, rec, win, Upto::Account);
                type_text(cx, win, "Quentin Fakename");
            },
        },
        Scene {
            name: "first-11-account-dark",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Account),
        },
        Scene {
            name: "first-12-account-taken-light",
            width: 1200.0,
            height: 900.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Taken),
        },
        Scene {
            name: "first-13-account-narrow-dark",
            width: 560.0,
            height: 900.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Taken),
        },
        Scene {
            name: "first-14-picture-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Picture),
        },
        Scene {
            name: "first-15-done-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Done),
        },
        Scene {
            name: "first-16-done-dark",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| first_run(cx, model, cast, rec, win, Upto::Done),
        },
        // The person who runs the server: the setup code, the workspace's name, the first
        // invitation.
        Scene {
            name: "first-17-owner-setup-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                first_run_owner(cx, model, cast, rec, win, Upto::Invited)
            },
        },
        Scene {
            name: "first-18-owner-name-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                first_run_owner(cx, model, cast, rec, win, Upto::Naming)
            },
        },
        Scene {
            name: "first-19-owner-name-empty-dark",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                first_run_owner(cx, model, cast, rec, win, Upto::Workspace)
            },
        },
        Scene {
            name: "first-20-owner-waiting-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                first_run_owner(cx, model, cast, rec, win, Upto::Share)
            },
        },
        Scene {
            name: "first-21-owner-invite-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                first_run_owner(cx, model, cast, rec, win, Upto::Shared)
            },
        },
        Scene {
            name: "first-22-owner-invite-dark",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                first_run_owner(cx, model, cast, rec, win, Upto::Shared)
            },
        },
        Scene {
            name: "first-23-owner-invite-narrow-light",
            width: 560.0,
            height: 1000.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                first_run_owner(cx, model, cast, rec, win, Upto::Shared)
            },
        },
        Scene {
            name: "first-24-owner-done-light",
            width: 1200.0,
            height: 800.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                first_run_owner(cx, model, cast, rec, win, Upto::Done)
            },
        },
    ];

    for scene in scenes {
        if filter
            .as_ref()
            .is_some_and(|f| !scene.name.starts_with(f.as_str()))
        {
            continue;
        }
        let mut cx = HeadlessAppContext::with_platform(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("DM Sans")),
            Arc::new(()),
            gpui_platform::current_headless_renderer,
        );
        let cast = Cast::new();
        cx.update(|app| {
            lazy_ui::init(app, scene.appearance);
            if scene.name.starts_with("first-") {
                lazy_ui::onboarding::enable(app);
            }
        });
        let recorder = Rc::new(Recorder::default());
        let model = cx.update(|app| app.new(|_| AppModel::new(recorder.clone())));
        let window = cx
            .open_window(size(px(scene.width), px(scene.height)), {
                let model = model.clone();
                move |window, app| app.new(|cx| RootView::new(model, window, cx))
            })
            .expect("open window");
        cx.run_until_parked();
        (scene.build)(&model, &cast, &mut cx, &recorder, &window);
        answer_avatars(&mut cx, &model, &recorder, &window);
        // Draw a couple of frames so list measurement settles.
        for _ in 0..3 {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .expect("draw");
            cx.run_until_parked();
        }
        match cx.capture_screenshot(window.into()) {
            Ok(image) => {
                let path = out.join(format!("{}.png", scene.name));
                image.save(&path).expect("write png");
                println!(
                    "wrote {} ({}x{})",
                    path.display(),
                    image.width(),
                    image.height()
                );
            }
            Err(error) => {
                eprintln!("{}: screenshot failed: {error:#}", scene.name);
                std::process::exit(1);
            }
        }
    }
    if filter.is_some() {
        return;
    }
    theme_switch_scene(&out);
    for (name, kind, appearance, height) in [
        (
            "27-editing-states-light",
            GalleryKind::Editing,
            WindowAppearance::Light,
            1500.0,
        ),
        (
            "28-editing-states-dark",
            GalleryKind::Editing,
            WindowAppearance::Dark,
            1500.0,
        ),
        (
            "31-row-statuses-light",
            GalleryKind::Statuses,
            WindowAppearance::Light,
            760.0,
        ),
        (
            "32-row-statuses-dark",
            GalleryKind::Statuses,
            WindowAppearance::Dark,
            760.0,
        ),
    ] {
        gallery_scene(&out, name, kind, appearance, 900.0, height);
    }
}

/// What the GitHub app posts for the activity in the owner's screenshot of 2026-11-01, as
/// `lazy_server::github::summarize` writes it.
const GITHUB_FEED: &[&str] = &[
    "**jasona** merged pull request [#7 fix\\(ui\\)\\: render text on macOS](https://github.com/last-refuge/lazy/pull/7) in last\\-refuge/lazy",
    "**jasona** pushed [1 commit](https://github.com/last-refuge/lazy/compare/a...b) to **main** in last\\-refuge/lazy\n- [9b57118](https://github.com/last-refuge/lazy/commit/9b57118) fix\\(ui\\)\\: render text on macOS \\(\\#7\\)",
    "Workflow [Notify Lazy \\#23](https://github.com/last-refuge/lazy/actions/runs/23) failed on **main** in last\\-refuge/lazy",
    "**jasona** opened issue [#11 test](https://github.com/last-refuge/lazy/issues/11) in last\\-refuge/lazy\n\n> testtesttest",
    "**jasona** closed issue [#11 test](https://github.com/last-refuge/lazy/issues/11) in last\\-refuge/lazy",
    "**jasona** opened pull request [#12 Remove the lazy\\-notify stopgap now that GitHub posts directly](https://github.com/last-refuge/lazy/pull/12) in last\\-refuge/lazy\n\n> The GitHub connector works end to end against the real GitHub on chat\\.lazyto\\.day \\(0\\.8\\.1\\), so lazy\\-notify\\.yml would only post every event twice\\.\n> • Deletes \\.github/workflows/lazy\\-notify\\.yml\\.\n> • VERIFICATION\\.md records the live run, the two production fixes \\(\\#8 CA certificates, …",
];
