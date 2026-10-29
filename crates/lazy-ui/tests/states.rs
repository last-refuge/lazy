//! The states a person can be left looking at (task 2.8): a screen with nothing on it, a load
//! that failed, a server that cannot be reached. Each is checked for what it says and for the
//! action it offers, and every action is checked with the keyboard where there is one to reach.
//!
//! What the views own is checked here (selectors, effects, the composer's hint, which is a method
//! so it can be read as text); wording that belongs to the client core is checked in its tests,
//! and wording owned by `lazy_ui::states` in that module's tests. The real views and the real
//! core run; a recording effect sink stands in for the runtime. Whether a state *looks* right is
//! decided by reading the offscreen renders, not here.
//!
//! The audit, with the test or render that covers each:
//!
//! | State                                   | Says                                   | Offers                         |
//! |-----------------------------------------|----------------------------------------|--------------------------------|
//! | no conversations at all                 | shell: "A good place to start." + Moss | Browse channels, New message   |
//! | no channels / no direct messages        | sidebar lines ("Say hello. ...")       | Browse channels, Find people   |
//! | empty channel                           | "Start the conversation."              | the composer                   |
//! | empty archived channel                  | "... archived, so it is read-only"     | none (read-only)               |
//! | history loading                         | "Loading messages…"                    | -                              |
//! | first page of history failed            | title + reason                         | Try again                      |
//! | older page of history failed            | bar over the messages + reason         | Try again                      |
//! | thread failed to load                   | "The thread didn't load" + reason      | Try again                      |
//! | webhooks: empty, loading, failed, full  | integrations section                   | Add, Retry, Delete             |
//! | groups: empty                           | "No groups yet ..."                    | New group                      |
//! | browse: loading, empty, failed          | dialog lines                           | Try again                      |
//! | server unreachable at start             | "Can't reach the server" + reason      | Try again (focused)            |
//! | session expired                         | sign-in with the reason                | sign in                        |
//! | connection lost                         | banner "Reconnecting… (attempt n)"     | (automatic)                    |
//! | notice (removed, link failed, ...)      | banner with the core's words           | Dismiss (keyboard), Join       |
//! | archived channel                        | composer hint, input disabled          | -                              |
//! | people not loaded while typing `@`      | composer hint                          | -                              |
//! | no custom emoji (picker, member)        | "No custom emoji yet."                 | -                              |
//! | no custom emoji (picker, admin)         | the same                               | Add custom emoji               |
//! | no custom emoji (admin dialog)          | "No custom emoji yet. Add one ..."     | Add custom emoji (focused)     |
//! | custom emoji picture loading / failed   | `:name:` in the text; a quiet square   | (fetched again on reconnect)   |
//! |   in the admin list                     | "..." / "!" and "Picture unavailable"  | -                              |
//! | custom emoji upload failed              | the core's reason under the name       | Try again                      |
//! | custom emoji deleted or unknown         | `:name:` where the picture was         | reactions: withdraw only       |
//! | search idle, no recents                 | "Type to search ... from:alice ..."    | the field                      |
//! | search idle, with recents               | "Recent searches" + the filter hint    | Enter or click on a recent     |
//! | search loading (first page)             | skeleton rows the size of results      | -                              |
//! | search with no results                  | "No messages found." + "Nothing matched ..." | edit the query, remove a chip  |
//! | search query that cannot run            | the parser's reason under the field    | fix the text                   |
//! | search took too long                    | "...took too long. Narrow it with ..." | Try again                      |
//! | search network failure                  | "Couldn't reach the server. Check ..." | Try again                      |
//! | search workspace gone                   | "You can't search this workspace ..."  | none (Escape leaves)           |
//! | next page loading / failed              | "Loading more ..." / "More results ..."| Try again                      |
//! | more results than ranked / at the limit | the truncated note / the end note      | narrow the search              |
//! | thread inbox loading (first page)       | skeleton rows the size of rows         | -                              |
//! | thread inbox, follows no threads        | "You're all caught up." + how a thread becomes followed (Moss) | Close |
//! | thread inbox, Unread, none unread       | "You're all caught up." + "No unread threads." (Moss) | All             |
//! | thread inbox failed (first page)        | "The threads didn't load" + reason     | Try again (focused)            |
//! | thread inbox, next page failed          | the note under the rows + reason       | Try again                      |
//! | thread whose first message was deleted  | "This message was deleted" + replies   | open it, Mark all read         |
//! | notifications paused (snooze / quiet)   | "paused until ..." / "... quiet hours" | Resume; the mark beside the name|
//! | quiet hours: bad / same time, no day    | the reason under the times             | fix the text or the days       |
//! | status too long                         | counter in red + "at most 100 ..."     | shorten the text               |
//! | muted conversation                      | dimmed row, quiet count, 🔕 bell       | bell menu "Unmute"             |
//!
//! | channel directory loading / failed      | "Loading channels…" / "The channels didn't load" | Try again (search box keeps the keyboard) |
//! | channel directory: nothing found        | "No channels match “x”." / "You're in every public channel." | edit the search, All |
//! | channel directory: next page loading / failed | "Loading more …" / the note under the rows | Load more / Try again |
//! | people directory loading / failed / none| skeleton / "The people didn't load" / "No one matches “x”." | Try again, edit the search |
//! | profile loading / failed / person gone  | a bar / "The rest of this profile didn't load" / "no longer in the workspace" | Try again |
//! | quick switcher, no recents              | "Nothing yet. Type a name …"           | type                           |
//! | quick switcher, nothing matches         | one row: "Browse channels matching …"  | Enter opens the directory      |
//! | saved messages loading / failed / none  | skeleton / "The saved messages didn't load" / "Nothing saved yet. …" | Try again, Close |
//! | saved: next page loading / failed       | "Loading more …" / the note under the rows | Show more / Try again       |
//! | invitations loading / failed / none     | skeleton / "The invitations didn't load" / "No invitations yet. …" | Try again (keyboard stays in the dialog), Create |
//! | invitation create refused / unknown     | the server's refusal under the button / the core's notice | Create again           |
//! | revoke asks first                       | "Revoke this invitation?" + what happens | Keep (focused), Revoke        |
//! | new invitation code                     | the code once, "shown only once", Copy | Copy, Done                     |
//! | workspace name empty / too long         | the limit under the field, Save off    | edit the name                  |
//! | workspace settings refused              | the server's refusal above Save        | change, Save again             |
//! | role change refused (last owner, …)     | the server's refusal under the role    | choose another                 |
//! | deactivate asks first                   | "Deactivate <name>?" + what happens    | Cancel (focused), Deactivate   |
//! | deactivated person (list, profile)      | muted row + "Deactivated" badge, note  | Reactivate (admins)            |
//! | only admins create channels (member)    | the menu item off with the reason      | Browse channels                |
//! | registration code refused               | the core's wording above the button    | edit the code or clear it      |
//! | access lost (deactivated)               | "You no longer have access to <name>"  | Sign out                       |
//!
//! The thread inbox, bell, Preferences and status rows are driven in `tests/notifications.rs`,
//! the discovery rows in `tests/discovery.rs`, the administration rows in `tests/administration.rs` (one test each, named for the state); their wording
//! is checked in `lazy_ui::states`.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowAppearance,
};
use lazy_client_core::{Effect, Input, RequestError, SessionPhase, StartupState};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationMembersResponse, ConversationUserState,
    HistoryPage, Message, PROTOCOL_VERSION, User, Workspace, WorkspaceBootstrap,
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
    bob: User,
    workspace: WorkspaceId,
    general: ConversationId,
    /// The generation of the request for who is in #general, once it has been made.
    members_generation: std::cell::Cell<Option<u64>>,
    /// The workspace's custom emoji, and the role the person has, for the bootstrap.
    emoji: RefCell<Vec<lazy_protocol::CustomEmoji>>,
    role: std::cell::Cell<WorkspaceRole>,
}

fn person(name: &str) -> User {
    User {
        is_bot: false,
        deactivated: false,
        avatar_version: 0,
        status: None,
        id: UserId::generate(),
        username: Username::parse(name).unwrap(),
        display_name: DisplayName::parse(&format!("User {name}")).unwrap(),
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
        me: person("alice"),
        bob: person("bob"),
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
        members_generation: Default::default(),
        emoji: RefCell::default(),
        role: std::cell::Cell::new(WorkspaceRole::Member),
    };
    (world, cx)
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

impl World {
    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    fn bootstrap(&self, archived: bool) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: Vec::new(),
                notification_settings: Default::default(),
                conversation_notification_prefs: Vec::new(),
                custom_emoji: self.emoji.borrow().clone(),
                threads: Vec::new(),
                groups: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role: self.role.get(),
                conversations: vec![Conversation {
                    id: self.general,
                    workspace_id: self.workspace,
                    kind: ConversationKind::Channel,
                    name: Some(ChannelName::parse("general").unwrap()),
                    topic: None,
                    description: None,
                    visibility: ChannelVisibility::Public,
                    archived,
                    is_default: true,
                    created_by: None,
                    participants: vec![],
                    member_count: 2,
                }],
                user_state: vec![ConversationUserState {
                    conversation_id: self.general,
                    favorite: false,
                    last_read: EventSeq::ZERO,
                    unread_count: 0,
                    mention_count: 0,
                }],
                members: vec![self.me.clone(), self.bob.clone()],
                head: EventSeq::ZERO,
            }],
        }
    }

    fn message(&self, source: &str, seq: i64) -> Message {
        Message {
            attachments: vec![],
            thread_root: None,
            in_channel: false,
            thread: None,
            mentions: Default::default(),
            id: MessageId::generate(),
            conversation_id: self.general,
            author_id: self.bob.id,
            document: MessageDocument::from_markdown(source).unwrap(),
            created_at: Timestamp::now(),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            deleted: false,
        }
    }

    /// Signs in and delivers the bootstrap, up to the point where #general asks for its history.
    /// Returns the generation of that request.
    fn open_general(&self, cx: &mut VisualTestContext, archived: bool) -> u64 {
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(
            cx,
            Input::BootstrapLoaded(Box::new(self.bootstrap(archived))),
        );
        let effects = self.take_effects();
        self.members_generation
            .set(effects.iter().find_map(|effect| match effect {
                Effect::FetchMembers { generation, .. } => Some(*generation),
                _ => None,
            }));
        effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::FetchHistory { generation, .. } => Some(generation),
                _ => None,
            })
            .expect("loading #general fetches its history")
    }

    fn history_arrives(
        &self,
        cx: &mut VisualTestContext,
        generation: u64,
        result: Result<HistoryPage, RequestError>,
    ) {
        self.dispatch(
            cx,
            Input::HistoryLoaded {
                conversation: self.general,
                generation,
                result,
            },
        );
        draw(cx);
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

    /// Opens the custom emoji dialog the way a person does: Menu, Workspace, Custom emoji.
    fn open_emoji_admin(&self, cx: &mut VisualTestContext) {
        self.click(cx, "sidebar-menu");
        self.click(cx, "command-menu-tab-Workspace");
        self.click(cx, "command-menu-CustomEmoji");
    }

    fn history_requests(&self) -> Vec<(Option<EventSeq>, u64)> {
        self.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::FetchHistory {
                    before, generation, ..
                } => Some((before, generation)),
                _ => None,
            })
            .collect()
    }

    fn hint(&self, cx: &VisualTestContext) -> Option<String> {
        self.root.read_with(cx, |root, app| {
            root.shell()
                .unwrap()
                .read(app)
                .composer()
                .read(app)
                .hint(app)
        })
    }
}

fn only<T>(mut items: Vec<T>) -> T {
    assert_eq!(items.len(), 1, "exactly one expected");
    items.pop().unwrap()
}

// ---- The server cannot be reached at start --------------------------------------------------------------

#[gpui::test]
fn an_unreachable_server_offers_try_again_and_the_keyboard_is_already_on_it(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.dispatch(
        cx,
        Input::Started(StartupState {
            session_restored: true,
            ..Default::default()
        }),
    );
    world.take_effects();
    world.dispatch(
        cx,
        Input::BootstrapFailed(RequestError::Unknown("timed out".into())),
    );
    assert!(matches!(
        world.model.read_with(cx, |m, _| m.core().phase().clone()),
        SessionPhase::Unreachable { .. }
    ));
    assert!(world.on_screen(cx, "retry-load"), "there is a way forward");

    // Nothing was clicked or tabbed to: Enter is all it takes.
    cx.simulate_keystrokes("enter");
    assert_eq!(
        world.model.read_with(cx, |m, _| m.core().phase().clone()),
        SessionPhase::Loading,
        "the attempt is under way"
    );
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::LoadBootstrap)),
        "and the workspace is requested again"
    );

    // Failing again shows the screen again, with the keyboard on the button once more.
    world.dispatch(
        cx,
        Input::BootstrapFailed(RequestError::Unknown("still down".into())),
    );
    assert!(world.on_screen(cx, "retry-load"));
    cx.simulate_keystrokes("space");
    assert_eq!(
        world.model.read_with(cx, |m, _| m.core().phase().clone()),
        SessionPhase::Loading
    );
}

// ---- Nothing in the conversation yet -------------------------------------------------------------------

#[gpui::test]
fn loading_and_empty_are_different_screens(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let generation = world.open_general(cx, false);
    draw(cx);
    assert!(world.on_screen(cx, "timeline-loading"));
    assert!(!world.on_screen(cx, "timeline-empty"));

    world.history_arrives(
        cx,
        generation,
        Ok(HistoryPage {
            messages: vec![],
            has_more: false,
        }),
    );
    assert!(
        world.on_screen(cx, "timeline-empty"),
        "a ready, empty channel"
    );
    assert!(!world.on_screen(cx, "timeline-loading"));
}

#[gpui::test]
fn an_archived_channel_says_why_nothing_can_be_posted(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let generation = world.open_general(cx, true);
    world.history_arrives(
        cx,
        generation,
        Ok(HistoryPage {
            messages: vec![],
            has_more: false,
        }),
    );
    let hint = world.hint(cx).expect("a conversation is selected");
    assert!(hint.contains("#general is archived"), "{hint}");
    assert!(hint.contains("can read it, but not post"), "{hint}");
}

// ---- History that failed to load ---------------------------------------------------------------------

#[gpui::test]
fn a_failed_first_page_says_so_and_try_again_asks_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let generation = world.open_general(cx, false);
    world.history_arrives(
        cx,
        generation,
        Err(RequestError::Unknown("timed out".into())),
    );
    assert!(world.on_screen(cx, "history-failed"));
    assert!(world.on_screen(cx, "history-retry"));
    assert!(
        !world.on_screen(cx, "timeline-empty"),
        "a channel that did not load is not claimed to be empty"
    );
    world.take_effects();

    // The button is a tab stop with Enter and Space: the keyboard reaches it and it asks again.
    let focus = world.root.read_with(cx, |root, app| {
        root.shell()
            .unwrap()
            .read(app)
            .timeline()
            .read(app)
            .history_retry_focus()
    });
    cx.update(|window, app| window.focus(&focus, app));
    cx.simulate_keystrokes("enter");
    let (before, generation) = only(world.history_requests());
    assert_eq!(before, None, "the first page again");
    assert!(
        !world.on_screen(cx, "history-failed"),
        "the failure is cleared while the new attempt runs"
    );
    assert!(world.on_screen(cx, "timeline-loading"));

    world.history_arrives(
        cx,
        generation,
        Ok(HistoryPage {
            messages: vec![world.message("hello again", 1)],
            has_more: false,
        }),
    );
    assert!(world.on_screen(cx, "message-row-0"), "and now it is there");
}

#[gpui::test]
fn a_failed_older_page_is_shown_over_the_messages_and_try_again_asks_again(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let generation = world.open_general(cx, false);
    world.history_arrives(
        cx,
        generation,
        Ok(HistoryPage {
            messages: (5..8)
                .map(|n| world.message(&format!("message {n}"), n))
                .collect(),
            has_more: true,
        }),
    );
    assert!(world.on_screen(cx, "message-row-0"));
    assert!(!world.on_screen(cx, "older-failed"));
    world.take_effects();

    world.dispatch(
        cx,
        Input::LoadOlder {
            conversation: world.general,
        },
    );
    let (before, generation) = only(world.history_requests());
    assert!(before.is_some(), "older than what is loaded");
    world.history_arrives(
        cx,
        generation,
        Err(RequestError::Unknown("connection reset".into())),
    );
    assert!(
        world.on_screen(cx, "older-failed"),
        "the failure is not silent"
    );
    assert!(
        world.on_screen(cx, "message-row-0"),
        "what was loaded stays readable"
    );
    assert!(
        !world.on_screen(cx, "history-failed"),
        "this is not the empty screen"
    );

    world.click(cx, "history-retry");
    let (retried_before, generation) = only(world.history_requests());
    assert_eq!(retried_before, before, "the same page is asked for again");
    world.history_arrives(
        cx,
        generation,
        Ok(HistoryPage {
            messages: (1..5)
                .map(|n| world.message(&format!("message {n}"), n))
                .collect(),
            has_more: false,
        }),
    );
    assert!(!world.on_screen(cx, "older-failed"), "gone once it works");
}

// ---- Notices -----------------------------------------------------------------------------------------

#[gpui::test]
fn a_notice_can_be_dismissed_with_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.dispatch(
        cx,
        Input::Started(StartupState {
            session_restored: true,
            problems: vec!["Your saved drafts couldn't be read, so they were set aside.".into()],
            ..Default::default()
        }),
    );
    world.dispatch(cx, Input::BootstrapLoaded(Box::new(world.bootstrap(false))));
    world.take_effects();
    draw(cx);
    assert!(world.on_screen(cx, "notice-banner"), "the problem is shown");
    assert!(world.on_screen(cx, "dismiss-notice"));

    let focus = world.root.read_with(cx, |root, app| {
        root.shell().unwrap().read(app).notice_dismiss_focus()
    });
    cx.update(|window, app| window.focus(&focus, app));
    cx.simulate_keystrokes("enter");
    assert!(
        world
            .model
            .read_with(cx, |m, _| m.core().notice().is_none()),
        "Enter on Dismiss clears it"
    );
    assert!(!world.on_screen(cx, "notice-banner"));
}

// ---- People not loaded yet ---------------------------------------------------------------------------

#[gpui::test]
fn typing_an_at_before_the_people_arrive_explains_the_short_list(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let generation = world.open_general(cx, false);
    world.history_arrives(
        cx,
        generation,
        Ok(HistoryPage {
            messages: vec![],
            has_more: false,
        }),
    );
    let composer = world.root.read_with(cx, |root, app| {
        root.shell().unwrap().read(app).composer().clone()
    });
    composer.update_in(cx, |composer, window, cx| composer.focus_input(window, cx));

    cx.simulate_input("hello ");
    let plain = world.hint(cx).unwrap();
    assert!(plain.starts_with("Message #general"), "{plain}");

    cx.simulate_input("@");
    let loading = world.hint(cx).unwrap();
    assert!(
        loading.contains("Still loading who is in #general"),
        "the list is short because the people are not here yet: {loading}"
    );

    // The people arrive: the hint goes back to saying what Enter does.
    let generation = world
        .members_generation
        .get()
        .expect("selecting a channel asks who is in it");
    world.dispatch(
        cx,
        Input::MembersFetched {
            conversation: world.general,
            generation,
            result: Ok(ConversationMembersResponse {
                conversation_id: world.general,
                user_ids: vec![world.me.id, world.bob.id],
            }),
        },
    );
    let loaded = world.hint(cx).unwrap();
    assert!(
        !loaded.contains("Still loading"),
        "once the people are here the explanation goes: {loaded}"
    );
}

// ---- Custom emoji ---------------------------------------------------------------------------------------

impl World {
    /// Signs in as `role` with #general loaded and empty.
    fn reach_as(&self, cx: &mut VisualTestContext, role: WorkspaceRole) {
        self.role.set(role);
        let generation = self.open_general(cx, false);
        self.history_arrives(
            cx,
            generation,
            Ok(HistoryPage {
                messages: vec![],
                has_more: false,
            }),
        );
    }

    fn live_emoji(&self, name: &str) -> lazy_protocol::CustomEmoji {
        lazy_protocol::CustomEmoji {
            id: lazy_domain::EmojiId::generate(),
            workspace_id: self.workspace,
            name: name.to_owned(),
            created_by: self.bob.id,
            created_at: Timestamp::now(),
            deleted: false,
        }
    }
}

#[gpui::test]
fn the_pickers_empty_custom_section_says_so_and_only_an_admin_is_offered_to_add(
    cx: &mut TestAppContext,
) {
    for (role, can_add) in [(WorkspaceRole::Member, false), (WorkspaceRole::Admin, true)] {
        let (world, cx) = start(cx);
        world.reach_as(cx, role);
        world.click(cx, "composer-emoji");
        assert!(
            world.on_screen(cx, "emoji-custom-empty"),
            "{role:?}: the empty section says so"
        );
        assert_eq!(world.on_screen(cx, "emoji-add-custom"), can_add, "{role:?}");
    }
}

#[gpui::test]
fn an_empty_admin_dialog_says_what_it_is_for_and_the_keyboard_is_on_add(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Owner);
    world.open_emoji_admin(cx);
    assert!(world.on_screen(cx, "emoji-admin-empty"));
    assert!(world.on_screen(cx, "emoji-admin-add"));
    cx.simulate_keystrokes("enter");
    assert!(
        world.on_screen(cx, "emoji-add-choose"),
        "Enter pressed Add custom emoji, which had the keyboard"
    );
}

#[gpui::test]
fn a_picture_that_cannot_be_fetched_is_marked_in_the_admin_list_and_the_row_still_works(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.emoji.borrow_mut().push(world.live_emoji("shipit"));
    let id = world.emoji.borrow()[0].id;
    world.reach_as(cx, WorkspaceRole::Admin);
    world.open_emoji_admin(cx);
    assert!(
        world.on_screen(cx, "emoji-picture-loading"),
        "a quiet square while it loads"
    );
    world.dispatch(
        cx,
        Input::EmojiImageLoaded {
            emoji: id,
            result: Err(lazy_client_core::TransferError::Unreadable("gone".into())),
        },
    );
    draw(cx);
    assert!(
        world.on_screen(cx, "emoji-picture-failed"),
        "a failed one says so, not a spinner that never ends"
    );
    cx.simulate_keystrokes("enter");
    assert!(
        world.on_screen(cx, "emoji-rename-save"),
        "the emoji can still be renamed"
    );
}

#[gpui::test]
fn a_failed_add_says_why_in_place_and_the_button_reads_try_again(cx: &mut TestAppContext) {
    use lazy_client_core::{ActionKind, ActionState};
    let (world, cx) = start(cx);
    world.reach_as(cx, WorkspaceRole::Admin);
    world.open_emoji_admin(cx);
    world.click(cx, "emoji-admin-add");
    let dir = std::env::temp_dir().join(format!("lazy-ui-states-{}", UserId::generate()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("blast_off.png");
    std::fs::write(&path, b"not really a picture").unwrap();
    world.click(cx, "emoji-add-choose");
    cx.simulate_path_prompt_response(|_| Some(vec![path.clone()]));
    cx.run_until_parked();
    world.click(cx, "emoji-add-submit");
    let (emoji, _) = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::UploadEmoji { emoji, name, .. } => Some((emoji, name)),
            _ => None,
        })
        .expect("a valid name and a file start the upload");
    world.dispatch(
        cx,
        Input::CustomEmojiUploaded {
            emoji,
            result: Err(lazy_client_core::TransferError::TooLarge { size: 9_000_000 }),
        },
    );
    draw(cx);
    let state = world.model.read_with(cx, |m, _| {
        m.core().action(ActionKind::AddCustomEmoji).clone()
    });
    let ActionState::Failed(reason) = state else {
        panic!("the core reports the failure: {state:?}");
    };
    assert!(
        reason.contains("512") || reason.to_lowercase().contains("large"),
        "the reason names the limit: {reason}"
    );
    assert!(world.on_screen(cx, "emoji-add-submit"), "the form stays");
    world.click(cx, "emoji-add-submit");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::UploadEmoji { .. })),
        "the same button tries again"
    );
}

// ---- Search ---------------------------------------------------------------------------------------------

impl World {
    /// Signs in, opens search with Ctrl+F, and types `text` and presses Enter.
    fn search_for(&self, cx: &mut VisualTestContext, text: &str) {
        let generation = self.open_general(cx, false);
        self.history_arrives(
            cx,
            generation,
            Ok(HistoryPage {
                messages: vec![],
                has_more: false,
            }),
        );
        cx.simulate_keystrokes("ctrl-f");
        cx.simulate_input(text);
        cx.simulate_keystrokes("enter");
        draw(cx);
    }

    fn search_generation(&self) -> u64 {
        self.take_effects()
            .into_iter()
            .rev()
            .find_map(|effect| match effect {
                Effect::Search { generation, .. } => Some(generation),
                _ => None,
            })
            .expect("a search was asked for")
    }

    fn search_fails(&self, cx: &mut VisualTestContext, error: RequestError) -> String {
        let generation = self.search_generation();
        self.dispatch(
            cx,
            Input::SearchResult {
                generation,
                result: Err(error),
            },
        );
        draw(cx);
        self.model
            .read_with(cx, |m, _| match &m.core().search().unwrap().status {
                lazy_client_core::SearchStatus::Failed { reason, .. } => reason.clone(),
                other => panic!("not failed: {other:?}"),
            })
    }
}

#[gpui::test]
fn search_before_anything_is_typed_says_what_to_type_and_offers_recent_searches(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let generation = world.open_general(cx, false);
    world.history_arrives(
        cx,
        generation,
        Ok(HistoryPage {
            messages: vec![],
            has_more: false,
        }),
    );
    cx.simulate_keystrokes("ctrl-f");
    assert!(world.on_screen(cx, "search-idle"));
    assert!(lazy_ui::states::SEARCH_IDLE.contains("from:alice"));
    assert!(world.on_screen(cx, "search-filter-hint"));
    assert!(!world.on_screen(cx, "search-recents"), "none yet");
    // The keyboard is already in the field.
    cx.simulate_input("plan");
    assert!(world.on_screen(cx, "search-view"));
}

#[gpui::test]
fn a_first_page_loading_shows_a_skeleton_and_an_empty_answer_says_what_to_try(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.search_for(cx, "zzz from:alice");
    assert!(
        world.on_screen(cx, "search-loading"),
        "rows, not a bare spinner"
    );
    let generation = world.search_generation();
    world.dispatch(
        cx,
        Input::SearchResult {
            generation,
            result: Ok(lazy_protocol::SearchResponse {
                hits: vec![],
                has_more: false,
                truncated: false,
            }),
        },
    );
    assert!(world.on_screen(cx, "search-no-results"));
    let text = lazy_ui::states::search_no_results("zzz from:alice", &["from:alice".to_owned()]);
    assert!(
        text.contains("Try another word") && text.contains("from:alice"),
        "{text}"
    );
}

#[gpui::test]
fn every_search_failure_says_what_happened_and_offers_what_can_be_done(cx: &mut TestAppContext) {
    use lazy_protocol::{ApiError, ErrorCode};
    let cases: [(RequestError, &str, bool); 3] = [
        (
            RequestError::Unknown("reset".into()),
            "Couldn't reach the server",
            true,
        ),
        (
            RequestError::Rejected(ApiError::new(ErrorCode::Unavailable, "slow")),
            "took too long. Narrow it with in: or from:",
            true,
        ),
        (
            RequestError::Rejected(ApiError::new(ErrorCode::NotFound, "gone")),
            "can't search this workspace",
            false,
        ),
    ];
    for (error, words, retry) in cases {
        let (world, cx) = start(cx);
        world.search_for(cx, "deploy");
        let reason = world.search_fails(cx, error);
        assert!(reason.contains(words), "{reason}");
        assert!(world.on_screen(cx, "search-failed"));
        assert_eq!(world.on_screen(cx, "search-retry"), retry, "{reason}");
    }
}
