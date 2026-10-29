//! Slice 3D on GPUI's test platform: the conversation bell and its menu, the preferences dialog
//! (levels, switches, pause, quiet hours, status), the sidebar's Threads entry and the thread
//! inbox, muted rows, and the paused and status marks beside the person's name. The real views
//! and the real core run; a recording effect sink stands in for the runtime.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch, every mouse claim
//! with real mouse events. The test platform gives every character the same width and never
//! reports a window as active; whether a screen looks right is decided by the offscreen renders
//! (`.dev/screens-3d`). Nothing here exercises a real input method, notification daemon, or
//! screen reader.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowAppearance,
};
use lazy_client_core::{Effect, Input, RequestError, StartupState};
use lazy_domain::notification::{NotificationSettings, NotifyLevel, QuietHours, UserStatus};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, ReactionEmoji, Timestamp, UserId, Username, WorkspaceId,
    WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationMembersResponse, ConversationUserState,
    HistoryPage, InboxEntry, InboxPage, Message, PROTOCOL_VERSION, ThreadSummary, ThreadUserState,
    User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::root_view::RootView;

#[derive(Default)]
struct Recorder(RefCell<Vec<Effect>>);

impl EffectSink for Recorder {
    fn execute(&self, effects: Vec<Effect>) {
        self.0.borrow_mut().extend(effects);
    }
}

struct World {
    model: Entity<AppModel>,
    effects: Rc<Recorder>,
    root: Entity<RootView>,
    me: User,
    alice: User,
    bob: User,
    workspace: WorkspaceId,
    general: ConversationId,
    design: ConversationId,
    /// Unread replies the bootstrap reports for followed threads, for the sidebar's count.
    followed_unread: u32,
    /// The generation of the request for who is in #general, as the last `reach` saw it.
    members_generation: std::cell::Cell<Option<u64>>,
}

fn person(name: &str, display: &str) -> User {
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

fn start(cx: &mut TestAppContext) -> (World, &mut VisualTestContext) {
    cx.update(|cx| lazy_ui::init(cx, WindowAppearance::Light));
    let effects = Rc::new(Recorder::default());
    let model = cx.new(|_| AppModel::new(effects.clone()));
    let (root, cx) = cx.add_window_view({
        let model = model.clone();
        move |window, cx| RootView::new(model, window, cx)
    });
    let world = World {
        model,
        effects,
        root,
        me: person("myself", "Me Myself"),
        alice: person("alice", "Alice Nguyen"),
        bob: person("bob", "Bob Okafor"),
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
        design: ConversationId::generate(),
        followed_unread: 0,
        members_generation: std::cell::Cell::new(None),
    };
    (world, cx)
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

impl World {
    fn channel(&self, id: ConversationId, name: &str, default: bool) -> Conversation {
        Conversation {
            id,
            workspace_id: self.workspace,
            kind: ConversationKind::Channel,
            name: Some(ChannelName::parse(name).unwrap()),
            topic: None,
            description: None,
            visibility: ChannelVisibility::Public,
            archived: false,
            is_default: default,
            created_by: None,
            participants: vec![],
            member_count: 3,
        }
    }

    fn bootstrap(&self, role: WorkspaceRole) -> BootstrapResponse {
        let state = |id: ConversationId, unread: u32| ConversationUserState {
            conversation_id: id,
            favorite: false,
            last_read: EventSeq::ZERO,
            unread_count: unread,
            mention_count: 0,
        };
        let thread_states = if self.followed_unread > 0 {
            vec![ThreadUserState {
                root_id: MessageId::generate(),
                conversation_id: self.general,
                following: true,
                last_read: EventSeq::ZERO,
                unread_count: self.followed_unread,
                mention_count: 0,
            }]
        } else {
            Vec::new()
        };
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: Vec::new(),
                notification_settings: NotificationSettings::default(),
                conversation_notification_prefs: Vec::new(),
                custom_emoji: Vec::new(),
                threads: thread_states,
                groups: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role,
                conversations: vec![
                    self.channel(self.general, "general", true),
                    self.channel(self.design, "design", false),
                ],
                user_state: vec![state(self.general, 0), state(self.design, 3)],
                members: vec![self.me.clone(), self.alice.clone(), self.bob.clone()],
                head: EventSeq::ZERO,
            }],
        }
    }

    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    fn reach(&self, cx: &mut VisualTestContext) {
        // A big window, so the dialogs and the inbox beside the pane have the room they need.
        cx.simulate_resize(gpui::size(gpui::px(1500.), gpui::px(1600.)));
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(
            cx,
            Input::BootstrapLoaded(Box::new(self.bootstrap(WorkspaceRole::Member))),
        );
        // The runtime sends the clock; here the test does.
        self.dispatch(cx, Input::Clock(Timestamp::now()));
        let effects = self.take_effects();
        self.members_generation
            .set(effects.iter().find_map(|effect| match effect {
                Effect::FetchMembers { generation, .. } => Some(*generation),
                _ => None,
            }));
        let generation = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::FetchHistory { generation, .. } => Some(generation),
                _ => None,
            })
            .expect("loading #general fetches its history");
        self.dispatch(
            cx,
            Input::HistoryLoaded {
                conversation: self.general,
                generation,
                result: Ok(HistoryPage {
                    messages: vec![self.message(self.general, &self.alice, "welcome", 1)],
                    has_more: false,
                }),
            },
        );
        draw(cx);
    }

    fn message(
        &self,
        conversation: ConversationId,
        author: &User,
        text: &str,
        seq: i64,
    ) -> Message {
        let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(500 - seq);
        Message {
            attachments: vec![],
            mentions: Default::default(),
            thread_root: None,
            in_channel: false,
            thread: None,
            id: MessageId::generate(),
            conversation_id: conversation,
            author_id: author.id,
            document: MessageDocument::from_markdown(text).unwrap(),
            created_at: Timestamp::from(at),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            deleted: false,
        }
    }

    fn on_screen(&self, cx: &mut VisualTestContext, selector: &'static str) -> bool {
        draw(cx);
        cx.debug_bounds(selector).is_some()
    }

    fn click(&self, cx: &mut VisualTestContext, selector: &'static str) {
        draw(cx);
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing is labelled {selector:?} on screen"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }

    fn settings(&self, cx: &VisualTestContext) -> NotificationSettings {
        self.model
            .read_with(cx, |m, _| *m.core().notification_settings())
    }

    fn shell(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::shell::ShellView> {
        self.root
            .read_with(cx, |root, _| root.shell().expect("the shell").clone())
    }

    fn inbox_rows(&self, cx: &mut VisualTestContext) -> Vec<lazy_ui::inbox_view::RowInfo> {
        let shell = self.shell(cx);
        let view = shell.read_with(cx, |shell, _| shell.inbox_view().clone());
        view.read_with(cx, |view, _| view.rows_info())
    }

    fn inbox_stop(&self, cx: &mut VisualTestContext) -> &'static str {
        let shell = self.shell(cx);
        let view = shell.read_with(cx, |shell, _| shell.inbox_view().clone());
        cx.update(|window, app| view.read(app).stop(window))
    }

    fn inbox_active(&self, cx: &mut VisualTestContext) -> Option<usize> {
        let shell = self.shell(cx);
        let view = shell.read_with(cx, |shell, _| shell.inbox_view().clone());
        view.read_with(cx, |view, _| view.active())
    }

    fn inbox_loads(&self) -> Vec<(u64, Option<bool>)> {
        self.effects
            .0
            .borrow()
            .iter()
            .filter_map(|effect| match effect {
                Effect::LoadInbox {
                    generation, query, ..
                } => Some((*generation, query.unread)),
                _ => None,
            })
            .collect()
    }

    fn answer_inbox(&self, cx: &mut VisualTestContext, entries: Vec<InboxEntry>, more: bool) {
        let (generation, _) = self
            .inbox_loads()
            .pop()
            .expect("the inbox asked for a page");
        self.effects
            .0
            .borrow_mut()
            .retain(|effect| !matches!(effect, Effect::LoadInbox { .. }));
        self.dispatch(
            cx,
            Input::InboxLoaded {
                generation,
                result: Ok(InboxPage {
                    entries,
                    has_more: more,
                }),
            },
        );
        draw(cx);
    }

    /// A followed thread in #design: a root by Bob, `replies` replies, the newest by Alice.
    fn entry(&self, n: usize, unread: u32) -> InboxEntry {
        let conversation = self.channel(self.design, "design", false);
        let mut root = self.message(
            self.design,
            &self.bob,
            &format!("Thread number {n}: should we ship it?"),
            10 + n as i64,
        );
        root.thread = Some(ThreadSummary {
            reply_count: 2,
            last_reply_seq: EventSeq::new(100 + n as i64).unwrap(),
            last_reply_at: Timestamp::now(),
            recent_repliers: vec![self.alice.id],
            as_of: EventSeq::new(100 + n as i64).unwrap(),
        });
        let mut reply = self.message(
            self.design,
            &self.alice,
            "Yes, after the freeze.",
            100 + n as i64,
        );
        reply.thread_root = Some(root.id);
        InboxEntry {
            state: ThreadUserState {
                root_id: root.id,
                conversation_id: self.design,
                following: true,
                last_read: EventSeq::ZERO,
                unread_count: unread,
                mention_count: 0,
            },
            root,
            last_reply: Some(reply),
            conversation,
        }
    }
}

// ---- The bell ---------------------------------------------------------------------------------------

#[gpui::test]
fn the_bell_opens_a_menu_and_the_keyboard_chooses_how_much_notifies(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    assert!(world.on_screen(cx, "notify-bell"));
    assert_eq!(
        world
            .model
            .read_with(cx, |m, _| m.core().effective_level(world.general)),
        NotifyLevel::Mentions,
        "a channel follows the workspace default, which is mentions"
    );
    world.click(cx, "notify-bell");
    for item in ["level-All", "level-Mentions", "level-Nothing", "mute"] {
        let selector = Box::leak(format!("bell-menu-{item}").into_boxed_str());
        assert!(world.on_screen(cx, selector), "{item} is offered");
    }
    world.take_effects();
    // The menu has the keyboard: Down twice and Enter choose the third, Nothing.
    cx.simulate_keystrokes("down down enter");
    let (level, effective) = world.model.read_with(cx, |m, _| {
        (
            m.core()
                .conversation_notification_prefs(world.general)
                .and_then(|prefs| prefs.level),
            m.core().effective_level(world.general),
        )
    });
    assert_eq!(level, Some(NotifyLevel::Nothing));
    assert_eq!(effective, NotifyLevel::Nothing);
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::SetConversationNotificationPrefs { .. })),
        "the choice is saved"
    );
    assert!(
        !world.on_screen(cx, "bell-menu"),
        "choosing closes the menu"
    );
}

#[gpui::test]
fn muting_toggles_and_a_muted_row_keeps_a_readable_unread_count(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    // #design has 3 unread; it is not the open conversation.
    assert!(world.on_screen(cx, "badge-design"));
    world.dispatch(
        cx,
        Input::SetConversationNotificationPrefs {
            conversation: world.design,
            level: None,
            muted: true,
        },
    );
    assert!(
        world
            .model
            .read_with(cx, |m, _| m.core().is_muted(world.design))
    );
    assert!(
        world.on_screen(cx, "badge-design"),
        "the count is still there to read, in a quiet style"
    );

    // From the bell: general is muted and unmuted again.
    world.click(cx, "notify-bell");
    cx.simulate_keystrokes("down down down enter");
    assert!(
        world
            .model
            .read_with(cx, |m, _| m.core().is_muted(world.general))
    );
    world.click(cx, "notify-bell");
    cx.simulate_keystrokes("down down down enter");
    assert!(
        !world
            .model
            .read_with(cx, |m, _| m.core().is_muted(world.general))
    );
}

#[gpui::test]
fn escape_closes_the_bell_menu_and_the_bell_has_the_keyboard_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "notify-bell");
    assert!(world.on_screen(cx, "bell-menu"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "bell-menu"));
    // The bell has the keyboard: Enter opens the menu again.
    cx.simulate_keystrokes("enter");
    assert!(world.on_screen(cx, "bell-menu"));
}

// ---- The preferences dialog -------------------------------------------------------------------------

#[gpui::test]
fn everyone_has_preferences_and_the_level_control_works_from_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-preferences");
    assert!(world.on_screen(cx, "prefs-channel-level"));
    assert_eq!(world.settings(cx).channel_level, NotifyLevel::Mentions);
    // The first control has the keyboard: Right moves to Nothing, Left twice to All.
    cx.simulate_keystrokes("right");
    assert_eq!(world.settings(cx).channel_level, NotifyLevel::Nothing);
    cx.simulate_keystrokes("left left");
    assert_eq!(world.settings(cx).channel_level, NotifyLevel::All);
    // Tab to direct messages, which default to all.
    cx.simulate_keystrokes("tab right");
    assert_eq!(world.settings(cx).direct_level, NotifyLevel::Mentions);
    cx.simulate_keystrokes("right");
    assert_eq!(world.settings(cx).direct_level, NotifyLevel::Nothing);
    cx.simulate_keystrokes("escape");
    assert!(
        !world.on_screen(cx, "prefs-channel-level"),
        "Escape closes it"
    );
}

#[gpui::test]
fn each_switch_changes_one_setting(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-preferences");
    let before = world.settings(cx);
    world.click(cx, "prefs-desktop");
    assert!(!world.settings(cx).desktop);
    world.click(cx, "prefs-show-content");
    assert!(!world.settings(cx).show_content);
    world.click(cx, "prefs-thread-replies");
    assert!(!world.settings(cx).thread_replies);
    world.click(cx, "prefs-channel-mentions");
    assert!(!world.settings(cx).channel_mentions);
    let after = world.settings(cx);
    assert_eq!(
        after.channel_level, before.channel_level,
        "nothing else moved"
    );
    assert_eq!(after.direct_level, before.direct_level);
    world.click(cx, "prefs-desktop");
    assert!(
        world.settings(cx).desktop,
        "and a second click turns it back on"
    );
}

#[gpui::test]
fn quiet_hours_turn_on_with_the_systems_offset_and_check_what_is_typed(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-preferences");
    assert!(world.settings(cx).quiet_hours.is_none());
    assert!(
        !world.on_screen(cx, "prefs-quiet-start"),
        "the times appear when they matter"
    );
    world.click(cx, "prefs-quiet");
    let hours = world.settings(cx).quiet_hours.expect("quiet hours are on");
    assert_eq!((hours.start_minute, hours.end_minute), (22 * 60, 7 * 60));
    assert_eq!(hours.days, QuietHours::EVERY_DAY);
    assert!(hours.is_valid());
    assert!(
        world.on_screen(cx, "prefs-quiet-zone"),
        "the zone is shown, read-only"
    );

    // A start that is not a time changes nothing and says what to write.
    world.click(cx, "prefs-quiet-start");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("25:00");
    assert!(world.on_screen(cx, "prefs-quiet-problem"));
    assert_eq!(
        world.settings(cx).quiet_hours.unwrap().start_minute,
        22 * 60
    );
    // A good one is applied at once and the complaint goes.
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("21:30");
    assert!(!world.on_screen(cx, "prefs-quiet-problem"));
    assert_eq!(
        world.settings(cx).quiet_hours.unwrap().start_minute,
        21 * 60 + 30
    );
    // The same time twice is refused.
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("07:00");
    assert!(world.on_screen(cx, "prefs-quiet-problem"));
    assert_eq!(
        world.settings(cx).quiet_hours.unwrap().start_minute,
        21 * 60 + 30
    );
}

#[gpui::test]
fn quiet_days_toggle_but_the_last_one_cannot_be_turned_off(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-preferences");
    world.click(cx, "prefs-quiet");
    world.click(cx, "prefs-day-6");
    assert_eq!(
        world.settings(cx).quiet_hours.unwrap().days,
        0b0011_1111,
        "Sunday is off"
    );
    for day in [
        "prefs-day-0",
        "prefs-day-1",
        "prefs-day-2",
        "prefs-day-3",
        "prefs-day-4",
    ] {
        world.click(cx, day);
    }
    assert_eq!(world.settings(cx).quiet_hours.unwrap().days, 0b0010_0000);
    world.click(cx, "prefs-day-5");
    assert_eq!(
        world.settings(cx).quiet_hours.unwrap().days,
        0b0010_0000,
        "at least one day stays"
    );
    assert!(world.on_screen(cx, "prefs-quiet-problem"));
    world.click(cx, "prefs-quiet");
    assert!(
        world.settings(cx).quiet_hours.is_none(),
        "or turn the hours off"
    );
}

#[gpui::test]
fn pausing_for_an_hour_shows_in_the_dialog_and_beside_the_name_and_resume_ends_it(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    assert!(!world.on_screen(cx, "sidebar-paused"));
    world.click(cx, "sidebar-preferences");
    world.click(cx, "prefs-pause");
    for item in 0..5 {
        let selector = Box::leak(format!("pause-menu-pause-{item}").into_boxed_str());
        assert!(world.on_screen(cx, selector), "choice {item}");
    }
    // 1 hour is the second choice.
    cx.simulate_keystrokes("down enter");
    let until = world.settings(cx).dnd_until.expect("paused");
    let from_now = until.as_offset_date_time() - Timestamp::now().as_offset_date_time();
    assert!(
        from_now > time::Duration::minutes(55) && from_now < time::Duration::minutes(65),
        "{from_now}"
    );
    assert!(
        world
            .model
            .read_with(cx, |m, _| m.core().notifications_paused())
    );
    assert!(world.on_screen(cx, "prefs-resume"));
    assert!(
        world.on_screen(cx, "sidebar-paused"),
        "the mark beside the person's name"
    );

    world.click(cx, "prefs-resume");
    assert!(world.settings(cx).dnd_until.is_none());
    assert!(!world.on_screen(cx, "prefs-resume"));
    assert!(!world.on_screen(cx, "sidebar-paused"));
}

#[gpui::test]
fn until_i_turn_it_back_on_is_a_long_pause_that_resume_ends(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-preferences");
    world.click(cx, "prefs-pause");
    cx.simulate_keystrokes("down down down down enter");
    let until = world.settings(cx).dnd_until.expect("paused");
    assert!(
        until.as_offset_date_time() - Timestamp::now().as_offset_date_time()
            > time::Duration::days(365)
    );
    assert!(
        world
            .model
            .read_with(cx, |m, _| m.core().notifications_paused())
    );
    world.click(cx, "prefs-resume");
    assert!(
        !world
            .model
            .read_with(cx, |m, _| m.core().notifications_paused())
    );
}

#[gpui::test]
fn a_status_is_checked_counted_saved_and_cleared(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-preferences");
    world.click(cx, "prefs-status-text");
    cx.simulate_input("In a meeting");
    assert!(world.on_screen(cx, "prefs-status-count"));
    world.take_effects();
    world.click(cx, "prefs-status-save");
    let status = world
        .model
        .read_with(cx, |m, _| m.core().status_of(world.me.id).cloned());
    assert_eq!(status.as_ref().map(UserStatus::text), Some("In a meeting"));
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::SetStatus { .. })),
        "the status is sent"
    );
    assert!(
        world.on_screen(cx, "sidebar-status"),
        "and shows beside the name"
    );

    // A status that is too long is refused in words and changes nothing.
    world.click(cx, "prefs-status-text");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input(&"x".repeat(101));
    world.click(cx, "prefs-status-save");
    assert!(world.on_screen(cx, "prefs-status-problem"));
    assert_eq!(
        world.model.read_with(cx, |m, _| m
            .core()
            .status_of(world.me.id)
            .map(|s| s.text().to_owned())),
        Some("In a meeting".to_owned())
    );
    // An empty one too.
    cx.simulate_keystrokes("ctrl-a backspace");
    world.click(cx, "prefs-status-save");
    assert!(world.on_screen(cx, "prefs-status-problem"));

    world.click(cx, "prefs-status-clear");
    assert!(
        world
            .model
            .read_with(cx, |m, _| m.core().status_of(world.me.id).is_none())
    );
    assert!(!world.on_screen(cx, "sidebar-status"));
}

#[gpui::test]
fn clear_after_ends_a_status_at_the_chosen_moment(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-preferences");
    world.click(cx, "prefs-status-text");
    cx.simulate_input("Out for lunch");
    world.click(cx, "prefs-clear-after");
    // "1 hour" is the third choice.
    cx.simulate_keystrokes("down down enter");
    world.click(cx, "prefs-status-save");
    let until = world
        .model
        .read_with(cx, |m, _| {
            m.core().status_of(world.me.id).and_then(|s| s.expires_at)
        })
        .expect("it ends");
    let from_now = until.as_offset_date_time() - Timestamp::now().as_offset_date_time();
    assert!(
        from_now > time::Duration::minutes(55) && from_now < time::Duration::minutes(65),
        "{from_now}"
    );
}

// ---- The thread inbox -------------------------------------------------------------------------------

#[gpui::test]
fn the_threads_entry_shows_the_unread_count_and_opens_the_inbox_with_a_skeleton(
    cx: &mut TestAppContext,
) {
    let (mut world, cx) = start(cx);
    world.followed_unread = 3;
    world.reach(cx);
    assert!(world.on_screen(cx, "sidebar-threads"));
    assert!(world.on_screen(cx, "badge-threads"), "3 unread replies");
    world.click(cx, "sidebar-threads");
    assert!(world.on_screen(cx, "inbox-view"));
    assert!(
        world.on_screen(cx, "inbox-loading"),
        "rows the shape of threads, not a spinner"
    );
    assert_eq!(world.inbox_loads().len(), 1, "it asked for the first page");
    assert_eq!(
        world.inbox_stop(cx),
        "list",
        "the keyboard starts in the list"
    );
    assert!(
        !world.on_screen(cx, "send-message"),
        "in place of the conversation"
    );
}

#[gpui::test]
fn threads_arrive_as_rows_and_the_keyboard_walks_them_and_opens_one_in_the_pane(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-threads");
    let entries: Vec<InboxEntry> = (0..6)
        .map(|n| world.entry(n, if n == 0 { 2 } else { 0 }))
        .collect();
    let second_root = entries[1].root.id;
    world.answer_inbox(cx, entries, false);
    let rows = world.inbox_rows(cx);
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[0].place, "#design");
    assert_eq!(rows[0].root_author, "Bob Okafor");
    assert!(
        rows[0]
            .root_excerpt
            .as_deref()
            .unwrap()
            .starts_with("Thread number 0")
    );
    assert_eq!(rows[0].last_reply.as_ref().unwrap().0, "Alice Nguyen");
    assert_eq!(rows[0].replies, 2);
    assert_eq!(rows[0].unread, 2);
    assert!(world.on_screen(cx, "inbox-thread-0"));

    assert_eq!(world.inbox_active(cx), None);
    cx.simulate_keystrokes("down down");
    assert_eq!(world.inbox_active(cx), Some(1));
    cx.simulate_keystrokes("up");
    assert_eq!(world.inbox_active(cx), Some(0));
    cx.simulate_keystrokes("end");
    assert_eq!(world.inbox_active(cx), Some(5));
    cx.simulate_keystrokes("home down enter");
    assert_eq!(
        world
            .model
            .read_with(cx, |m, _| m.core().open_thread().map(|t| t.root)),
        Some(second_root),
        "Enter opened the second thread in the pane"
    );
    assert!(world.on_screen(cx, "thread-pane"));
    assert!(
        world.on_screen(cx, "inbox-view"),
        "the inbox stays beside the pane"
    );
}

#[gpui::test]
fn a_click_opens_a_thread_and_escape_closes_the_inbox_and_returns_the_keyboard(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-threads");
    let entries = vec![world.entry(0, 0), world.entry(1, 0)];
    let first = entries[0].root.id;
    world.answer_inbox(cx, entries, false);
    world.click(cx, "inbox-thread-0");
    assert_eq!(
        world
            .model
            .read_with(cx, |m, _| m.core().open_thread().map(|t| t.root)),
        Some(first)
    );
    cx.simulate_keystrokes("escape");
    // Escape in the pane closes the pane; another, in the inbox, closes the inbox.
    world.click(cx, "inbox-view");
    cx.simulate_keystrokes("escape");
    assert!(!world.model.read_with(cx, |m, _| m.core().inbox().is_some()));
    assert!(
        world.on_screen(cx, "send-message"),
        "the conversation is back"
    );
}

#[gpui::test]
fn an_empty_inbox_says_how_a_thread_becomes_followed_and_unread_only_says_nothing_is_unread(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-threads");
    world.answer_inbox(cx, vec![], false);
    assert!(world.on_screen(cx, "inbox-empty"));
    assert!(lazy_ui::states::INBOX_EMPTY.contains("reply to it"));
    // Unread: asks again with the filter.
    world.click(cx, "inbox-unread");
    let (_, unread) = world.inbox_loads().pop().expect("asked again");
    assert_eq!(unread, Some(true));
    world.answer_inbox(cx, vec![], false);
    assert!(world.on_screen(cx, "inbox-empty"));
    assert_eq!(lazy_ui::states::INBOX_NO_UNREAD, "No unread threads.");
    // All again.
    world.click(cx, "inbox-all");
    let (_, unread) = world.inbox_loads().pop().expect("asked again");
    assert!(matches!(unread, None | Some(false)), "{unread:?}");
}

#[gpui::test]
fn a_failed_first_page_says_so_and_try_again_asks_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-threads");
    let (generation, _) = world.inbox_loads().pop().unwrap();
    world.dispatch(
        cx,
        Input::InboxLoaded {
            generation,
            result: Err(RequestError::Unknown("reset".into())),
        },
    );
    assert!(world.on_screen(cx, "inbox-failed"));
    assert!(world.on_screen(cx, "inbox-retry"));
    world.take_effects();
    world.click(cx, "inbox-retry");
    assert_eq!(world.inbox_loads().len(), 1);
}

#[gpui::test]
fn show_more_and_reaching_the_last_row_load_the_next_page(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-threads");
    world.answer_inbox(cx, (0..20).map(|n| world.entry(n, 0)).collect(), true);
    assert!(world.on_screen(cx, "inbox-show-more"));
    world.click(cx, "inbox-show-more");
    assert_eq!(world.inbox_loads().len(), 1);
    assert!(world.on_screen(cx, "inbox-loading-more"));
    world.answer_inbox(cx, (20..40).map(|n| world.entry(n, 0)).collect(), true);
    assert_eq!(world.inbox_rows(cx).len(), 40);
    // Reaching the end asks without a click.
    cx.simulate_keystrokes("end");
    assert_eq!(world.inbox_loads().len(), 1);
}

#[gpui::test]
fn a_thread_whose_first_message_was_deleted_says_so_and_mark_all_read_asks(
    cx: &mut TestAppContext,
) {
    let (mut world, cx) = start(cx);
    world.followed_unread = 2;
    world.reach(cx);
    world.click(cx, "sidebar-threads");
    let mut gone = world.entry(0, 2);
    gone.root.deleted = true;
    world.answer_inbox(cx, vec![gone], false);
    assert_eq!(
        world.inbox_rows(cx)[0].root_excerpt,
        None,
        "no text for a deleted first message"
    );
    world.take_effects();
    world.click(cx, "inbox-mark-read");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::MarkInboxRead { .. })),
        "Mark all read asks the server"
    );
}

#[gpui::test]
fn the_inbox_tab_order_is_toggle_mark_read_list_and_close(cx: &mut TestAppContext) {
    let (mut world, cx) = start(cx);
    world.followed_unread = 1;
    world.reach(cx);
    world.click(cx, "sidebar-threads");
    world.answer_inbox(cx, vec![world.entry(0, 1)], false);
    assert_eq!(world.inbox_stop(cx), "list", "it starts in the list");
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(world.inbox_stop(cx), "mark");
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(world.inbox_stop(cx), "toggle");
    cx.simulate_keystrokes("tab tab");
    assert_eq!(world.inbox_stop(cx), "list");
    cx.simulate_keystrokes("tab");
    assert_eq!(world.inbox_stop(cx), "close");
    cx.simulate_keystrokes("enter");
    assert!(!world.model.read_with(cx, |m, _| m.core().inbox().is_some()));
}

#[gpui::test]
fn the_clock_can_move_without_disturbing_the_paused_mark(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.dispatch(
        cx,
        Input::PauseNotifications {
            until: Some(Timestamp::from(
                time::OffsetDateTime::now_utc() + time::Duration::minutes(30),
            )),
        },
    );
    assert!(world.on_screen(cx, "sidebar-paused"));
    // The pause ends when the core's clock passes it.
    world.dispatch(
        cx,
        Input::Clock(Timestamp::from(
            time::OffsetDateTime::now_utc() + time::Duration::minutes(31),
        )),
    );
    cx.executor().advance_clock(Duration::from_millis(10));
    assert!(
        !world.on_screen(cx, "sidebar-paused"),
        "the mark follows the clock"
    );
}

#[gpui::test]
fn the_keyboard_scrolls_a_short_preferences_dialog_to_the_control_it_reaches(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-preferences");
    cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(520.)));
    draw(cx);
    let viewport = cx.debug_bounds("prefs-scroll").expect("the body");
    let save = cx
        .debug_bounds("prefs-status-save")
        .expect("the save button");
    assert!(
        save.bottom() > viewport.bottom(),
        "in a short window Save status starts below the fold"
    );
    // After each run of Tabs the control that has the keyboard is wholly in view: Pause is the
    // sixth stop after the first, Quiet hours the seventh, then emoji, text, Clear after, Save,
    // Clear, and the Profile and Appearance sections under them.
    for (tabs, selector) in [
        (6, "prefs-pause"),
        (1, "prefs-quiet"),
        (1, "prefs-status-emoji"),
        (1, "prefs-status-text"),
        (1, "prefs-clear-after"),
        (1, "prefs-status-save"),
        (1, "prefs-status-clear"),
        (1, "prefs-choose-picture"),
        (1, "prefs-remove-picture"),
        (1, "prefs-display-name"),
        (1, "prefs-save-name"),
        (1, "prefs-theme"),
        (1, "prefs-motion"),
    ] {
        for _ in 0..tabs {
            cx.simulate_keystrokes("tab");
        }
        draw(cx);
        let viewport = cx.debug_bounds("prefs-scroll").expect("the body");
        let control = cx.debug_bounds(selector).expect(selector);
        assert!(
            control.top() >= viewport.top() && control.bottom() <= viewport.bottom(),
            "{selector} is in view: {control:?} in {viewport:?}"
        );
    }
    // Close, then round the dialog back to the first stop: its heading is in view again.
    cx.simulate_keystrokes("tab tab");
    draw(cx);
    let viewport = cx.debug_bounds("prefs-scroll").expect("the body");
    let level = cx
        .debug_bounds("prefs-channel-level")
        .expect("the first control");
    assert!(
        level.top() >= viewport.top(),
        "the first control is in view again"
    );
}

fn status(text: &str, glyph: &str, until: Option<Timestamp>) -> UserStatus {
    UserStatus::new(text, ReactionEmoji::parse(glyph).ok(), until).unwrap()
}

const AUTHOR_STATUS: [&str; 3] = ["author-status-0", "author-status-1", "author-status-2"];

fn some_author_shows_a_status(cx: &mut VisualTestContext) -> bool {
    draw(cx);
    AUTHOR_STATUS
        .iter()
        .any(|selector| cx.debug_bounds(selector).is_some())
}

#[gpui::test]
fn an_authors_status_shows_beside_the_name_and_goes_when_it_ends(cx: &mut TestAppContext) {
    let (mut world, cx) = start(cx);
    let until = Timestamp::from(time::OffsetDateTime::now_utc() + time::Duration::minutes(30));
    world.alice.status = Some(status("Heads down", "🎯", Some(until)));
    world.reach(cx);
    assert!(
        some_author_shows_a_status(cx),
        "the first message is Alice's, and she has a status"
    );
    // The status ends when the core's clock passes its end.
    world.dispatch(
        cx,
        Input::Clock(Timestamp::from(
            time::OffsetDateTime::now_utc() + time::Duration::minutes(31),
        )),
    );
    cx.executor().advance_clock(Duration::from_millis(10));
    assert!(
        !some_author_shows_a_status(cx),
        "an ended status is not drawn"
    );
}

#[gpui::test]
fn nobody_without_a_status_gets_a_mark(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    assert!(!some_author_shows_a_status(cx));
}

#[gpui::test]
fn the_mention_list_shows_a_persons_status(cx: &mut TestAppContext) {
    let (mut world, cx) = start(cx);
    world.bob.status = Some(status("On holiday", "🌴", None));
    world.reach(cx);
    // Everyone is in #general.
    let generation = world
        .members_generation
        .get()
        .expect("#general asked who is in it");
    world.dispatch(
        cx,
        Input::MembersFetched {
            conversation: world.general,
            generation,
            result: Ok(ConversationMembersResponse {
                conversation_id: world.general,
                user_ids: vec![world.me.id, world.alice.id, world.bob.id],
            }),
        },
    );
    cx.simulate_input("@");
    draw(cx);
    assert!(
        cx.debug_bounds("mention-status-bob").is_some(),
        "Bob's status is beside his name"
    );
    assert!(
        cx.debug_bounds("mention-status-alice").is_none(),
        "Alice has none"
    );
}

#[gpui::test]
fn the_people_picker_shows_a_persons_status(cx: &mut TestAppContext) {
    let (mut world, cx) = start(cx);
    world.alice.status = Some(status("In a meeting", "📅", None));
    world.reach(cx);
    world.click(cx, "new-message");
    assert!(world.on_screen(cx, "person-status-alice"));
    assert!(!world.on_screen(cx, "person-status-bob"));
}

#[gpui::test]
fn the_inbox_keeps_the_keyboard_while_it_loads_and_after_try_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    world.click(cx, "sidebar-threads");
    assert!(world.on_screen(cx, "inbox-loading"));
    assert_eq!(
        world.inbox_stop(cx),
        "list",
        "the keyboard is in the view before the page arrives"
    );
    cx.simulate_keystrokes("escape");
    assert!(
        !world.on_screen(cx, "inbox-view"),
        "Escape closes it while it loads"
    );
    world.click(cx, "sidebar-threads");
    let (generation, _) = world.inbox_loads().pop().unwrap();
    world
        .effects
        .0
        .borrow_mut()
        .retain(|e| !matches!(e, Effect::LoadInbox { .. }));
    world.dispatch(
        cx,
        Input::InboxLoaded {
            generation,
            result: Err(RequestError::Unknown("connection reset".into())),
        },
    );
    draw(cx);
    world.click(cx, "inbox-retry");
    assert_eq!(
        world.inbox_stop(cx),
        "list",
        "Try again leaves the keyboard in the list"
    );
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "inbox-view"), "and Escape still works");
}
