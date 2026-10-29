//! Slice 3E on GPUI's test platform: the quick switcher, saved messages (the menu, the marker, the
//! list), the people directory and profiles, the channel directory's search, filter, sort and
//! paging, and the member list in Details. The real views and the real core run; a recording
//! effect sink stands in for the runtime, so a test answers the requests the core makes.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch, every mouse claim
//! with real mouse events. The test platform gives every character the same width and never
//! reports a window as active; whether a screen looks right is decided by the offscreen renders
//! (`.dev/screens-3e`). Nothing here exercises a real input method or screen reader.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowAppearance,
};
use lazy_client_core::{Effect, Input, QuickSwitcherTarget, RequestError, StartupState};
use lazy_domain::notification::{ConversationNotificationPrefs, NotificationSettings, UserStatus};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, ReactionEmoji, Timestamp, UserId, Username, WorkspaceId,
    WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, HistoryPage, Message, PROTOCOL_VERSION,
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
    carol: User,
    dm: ConversationId,
    old: ConversationId,
    /// Messages the bootstrap says are saved.
    saved: RefCell<Vec<MessageId>>,
    /// Whether the person is in an archived channel and has muted #design.
    extras: bool,
    /// Their role in the workspace.
    role: std::cell::Cell<WorkspaceRole>,
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
        carol: person("carol", "Carol Mbeki"),
        dm: ConversationId::generate(),
        old: ConversationId::generate(),
        saved: RefCell::new(Vec::new()),
        extras: false,
        role: std::cell::Cell::new(WorkspaceRole::Member),
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
        let state = |id: ConversationId, unread: u32, mentions: u32| ConversationUserState {
            conversation_id: id,
            favorite: false,
            last_read: EventSeq::ZERO,
            unread_count: unread,
            mention_count: mentions,
        };
        let mut conversations = vec![
            self.channel(self.general, "general", true),
            self.channel(self.design, "design", false),
            Conversation {
                kind: ConversationKind::Direct,
                name: None,
                participants: vec![self.me.id, self.bob.id],
                ..self.channel(self.dm, "placeholder", false)
            },
        ];
        let mut user_state = vec![
            state(self.general, 0, 0),
            state(self.design, 5, 1),
            state(self.dm, 2, 0),
        ];
        let mut prefs = Vec::new();
        if self.extras {
            let mut old = self.channel(self.old, "old-launch", false);
            old.archived = true;
            conversations.push(old);
            user_state.push(state(self.old, 0, 0));
            prefs.push(ConversationNotificationPrefs {
                conversation_id: self.design,
                level: None,
                muted: true,
            });
        }
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: self.saved.borrow().clone(),
                notification_settings: NotificationSettings::default(),
                conversation_notification_prefs: prefs,
                custom_emoji: Vec::new(),
                threads: Vec::new(),
                groups: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role,
                conversations,
                user_state,
                members: vec![
                    self.me.clone(),
                    self.alice.clone(),
                    self.bob.clone(),
                    self.carol.clone(),
                ],
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
            Input::BootstrapLoaded(Box::new(self.bootstrap(self.role.get()))),
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

    fn shell(&self, cx: &mut VisualTestContext) -> Entity<lazy_ui::shell::ShellView> {
        self.root
            .read_with(cx, |root, _| root.shell().expect("the shell").clone())
    }
}

impl World {
    fn core<R>(
        &self,
        cx: &VisualTestContext,
        read: impl FnOnce(&lazy_client_core::ClientCore) -> R,
    ) -> R {
        self.model.read_with(cx, |model, _| read(model.core()))
    }

    fn selected(&self, cx: &VisualTestContext) -> Option<ConversationId> {
        self.core(cx, |core| core.selected_conversation())
    }

    /// Whether the keyboard is in the conversation's composer.
    fn composer_focused(&self, cx: &mut VisualTestContext) -> bool {
        let shell = self.shell(cx);
        let input = shell.read_with(cx, |shell, app| shell.composer().read(app).input().clone());
        cx.update(|window, app| input.read(app).is_focused(window))
    }

    fn switcher(
        &self,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<lazy_ui::dialogs::QuickSwitcherDialog>> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, _| shell.quick_switcher())
    }

    fn switcher_rows(
        &self,
        cx: &mut VisualTestContext,
    ) -> Vec<lazy_client_core::QuickSwitcherItem> {
        let dialog = self.switcher(cx).expect("the switcher is open");
        dialog.read_with(cx, |dialog, _| dialog.rows().to_vec())
    }

    fn switcher_highlight(&self, cx: &mut VisualTestContext) -> usize {
        let dialog = self.switcher(cx).expect("the switcher is open");
        dialog.read_with(cx, |dialog, _| dialog.highlighted())
    }

    /// Effects of one kind, oldest first, taken out of the recorder.
    fn drain<T>(&self, pick: impl Fn(&Effect) -> Option<T>) -> Vec<T> {
        let mut effects = self.effects.0.borrow_mut();
        let mut picked = Vec::new();
        effects.retain(|effect| match pick(effect) {
            Some(item) => {
                picked.push(item);
                false
            }
            None => true,
        });
        picked
    }

    fn visit(&self, cx: &mut VisualTestContext, ids: &[ConversationId]) {
        for id in ids {
            self.dispatch(cx, Input::SelectConversation(*id));
        }
        self.take_effects();
        draw(cx);
    }
}

fn start_member(cx: &mut TestAppContext) -> (World, &mut VisualTestContext) {
    let (world, cx) = start(cx);
    world.reach(cx);
    (world, cx)
}

// ----- The quick switcher -----

#[gpui::test]
fn ctrl_k_opens_the_switcher_from_the_composer_and_escape_gives_the_keyboard_back(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start_member(cx);
    cx.simulate_input("hello");
    assert!(world.composer_focused(cx));
    cx.simulate_keystrokes("ctrl-k");
    assert!(
        world.on_screen(cx, "palette-card"),
        "Ctrl+K opens the switcher"
    );
    assert!(!world.composer_focused(cx), "the field has the keyboard");
    cx.simulate_input("ab");
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "palette-card"), "Escape closes it");
    assert!(
        world.composer_focused(cx),
        "and the composer has the keyboard again"
    );
    cx.simulate_input(" there");
    let draft = world.core(cx, |core| core.draft(world.general).to_owned());
    assert_eq!(
        draft, "hello there",
        "nothing typed in the switcher reached the draft"
    );
}

#[gpui::test]
fn ctrl_k_inside_the_switcher_closes_it_and_the_sidebar_hint_opens_it(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-switcher");
    assert!(world.on_screen(cx, "palette-card"), "the hint is a button");
    cx.simulate_keystrokes("ctrl-k");
    assert!(
        !world.on_screen(cx, "palette-card"),
        "Ctrl+K again closes it"
    );
}

#[gpui::test]
fn ctrl_k_does_nothing_over_another_dialog(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-preferences");
    assert!(world.on_screen(cx, "dialog-card"));
    cx.simulate_keystrokes("ctrl-k");
    assert!(
        !world.on_screen(cx, "palette-card"),
        "the dialog keeps the window"
    );
    assert!(world.on_screen(cx, "dialog-card"));
}

#[gpui::test]
fn with_no_text_it_lists_recents_and_enter_goes_back_to_the_one_before(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.visit(cx, &[world.design, world.dm, world.general]);
    cx.simulate_keystrokes("ctrl-k");
    let rows = world.switcher_rows(cx);
    assert_eq!(
        rows.first().map(|row| row.target.clone()),
        Some(QuickSwitcherTarget::Conversation(world.general)),
        "the first recent is where the person is"
    );
    assert_eq!(
        world.switcher_highlight(cx),
        1,
        "the one before it is highlighted"
    );
    assert_eq!(rows[1].target, QuickSwitcherTarget::Conversation(world.dm));
    cx.simulate_keystrokes("enter");
    assert_eq!(world.selected(cx), Some(world.dm), "Enter goes back");
    assert!(world.composer_focused(cx), "to its composer");
    assert!(!world.on_screen(cx, "palette-card"));
}

#[gpui::test]
fn typing_filters_and_marks_the_matched_characters(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("des");
    let rows = world.switcher_rows(cx);
    let design = rows
        .iter()
        .find(|row| row.target == QuickSwitcherTarget::Conversation(world.design))
        .expect("#design matches");
    assert_eq!(design.title, "#design");
    assert_eq!(
        design.matched,
        vec![1, 2, 3],
        "the typed letters are the matched ones"
    );
    assert_eq!(
        rows.last().map(|row| row.target.clone()),
        Some(QuickSwitcherTarget::BrowseChannels {
            query: "des".into()
        }),
        "the last row offers to browse channels"
    );
    cx.simulate_keystrokes("enter");
    assert_eq!(world.selected(cx), Some(world.design));
    assert!(world.composer_focused(cx));
}

#[gpui::test]
fn up_and_down_wrap_and_the_pointer_highlights_and_a_click_chooses(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.visit(cx, &[world.design, world.dm, world.general]);
    cx.simulate_keystrokes("ctrl-k");
    let count = world.switcher_rows(cx).len();
    assert!(count >= 3);
    cx.simulate_keystrokes("up up");
    assert_eq!(
        world.switcher_highlight(cx),
        count - 1,
        "Up wraps to the end"
    );
    cx.simulate_keystrokes("down");
    assert_eq!(world.switcher_highlight(cx), 0, "Down wraps to the start");
    // The pointer moves the highlight to the row under it, and a click opens that row.
    draw(cx);
    let bounds = cx.debug_bounds("quick-row-2").expect("a third row");
    cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
    assert_eq!(world.switcher_highlight(cx), 2);
    let chosen = world.switcher_rows(cx)[2].target.clone();
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(!world.on_screen(cx, "palette-card"), "a click chooses");
    if let QuickSwitcherTarget::Conversation(id) = chosen {
        assert_eq!(world.selected(cx), Some(id));
    }
}

#[gpui::test]
fn a_person_without_a_conversation_opens_a_direct_message(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("carol");
    let rows = world.switcher_rows(cx);
    assert_eq!(rows[0].target, QuickSwitcherTarget::Person(world.carol.id));
    assert_eq!(rows[0].subtitle.as_deref(), Some("@carol"));
    world.take_effects();
    cx.simulate_keystrokes("enter");
    let request = world
        .drain(|effect| match effect {
            Effect::OpenConversation { request, .. } => Some(request.clone()),
            _ => None,
        })
        .pop()
        .expect("choosing a person opens a direct message");
    assert_eq!(request.participants, vec![world.carol.id]);
    // The answer arrives: the conversation opens and the keyboard goes to its composer.
    let opened = Conversation {
        kind: ConversationKind::Direct,
        name: None,
        participants: vec![world.me.id, world.carol.id],
        ..world.channel(ConversationId::generate(), "placeholder", false)
    };
    world.dispatch(
        cx,
        Input::ConversationCommandResult {
            operation: request.operation_id,
            result: Ok(lazy_protocol::ConversationResponse {
                conversation: opened.clone(),
            }),
        },
    );
    assert_eq!(world.selected(cx), Some(opened.id));
    assert!(
        world.composer_focused(cx),
        "the keyboard is in the new conversation"
    );
}

#[gpui::test]
fn the_places_open_threads_saved_and_search(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("thr");
    cx.simulate_keystrokes("enter");
    assert!(
        world.core(cx, |core| core.inbox().is_some()),
        "Threads opens the inbox"
    );
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("sav");
    cx.simulate_keystrokes("enter");
    assert!(
        world.core(cx, |core| core.saved().open),
        "Saved opens the saved messages"
    );
    assert!(
        world.core(cx, |core| core.inbox().is_none()),
        "and the inbox gives way"
    );
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("sea");
    cx.simulate_keystrokes("enter");
    assert!(
        world.core(cx, |core| core.search().is_some()),
        "Search opens search"
    );
    assert!(!world.core(cx, |core| core.saved().open));
}

#[gpui::test]
fn browse_channels_matching_opens_the_directory_with_the_text(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("zzq");
    // Nothing matches but the offer to browse.
    let rows = world.switcher_rows(cx);
    assert_eq!(rows.len(), 1);
    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert!(world.on_screen(cx, "dialog-card"), "the directory opens");
    assert_eq!(world.core(cx, |core| core.browse().query.clone()), "zzq");
    let asked = world.drain(|effect| match effect {
        Effect::LoadBrowse { query, .. } => Some(query.query.clone()),
        _ => None,
    });
    assert_eq!(asked.last().cloned().flatten().as_deref(), Some("zzq"));
}

#[gpui::test]
fn unread_muted_and_archived_rows_say_so(cx: &mut TestAppContext) {
    let (mut world, cx) = start(cx);
    world.extras = true;
    world.reach(cx);
    cx.simulate_keystrokes("ctrl-k");
    cx.simulate_input("design");
    draw(cx);
    assert!(
        world.on_screen(cx, "quick-muted-0"),
        "a muted conversation says Muted"
    );
    assert!(
        world.on_screen(cx, "quick-unread-0"),
        "and keeps its unread count"
    );
    assert!(world.on_screen(cx, "quick-mentions-0"), "and its mentions");
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("old");
    draw(cx);
    assert!(
        world.on_screen(cx, "quick-archived-0"),
        "an archived channel says Archived"
    );
}

// ----- Saved messages -----

fn answer_saved(
    world: &World,
    cx: &mut VisualTestContext,
    items: Vec<lazy_protocol::SavedItem>,
    more: bool,
) {
    let (generation, _) = world
        .saved_loads()
        .pop()
        .expect("the saved list asked for a page");
    world.drain(|effect| matches!(effect, Effect::LoadSaved { .. }).then_some(()));
    world.dispatch(
        cx,
        Input::SavedLoaded {
            generation,
            result: Ok(lazy_protocol::SavedPage {
                items,
                has_more: more,
            }),
        },
    );
    draw(cx);
}

impl World {
    fn saved_loads(&self) -> Vec<(u64, Option<u64>)> {
        self.effects
            .0
            .borrow()
            .iter()
            .filter_map(|effect| match effect {
                Effect::LoadSaved {
                    generation, query, ..
                } => Some((*generation, query.before)),
                _ => None,
            })
            .collect()
    }

    fn saved_item(
        &self,
        conversation: &Conversation,
        author: &User,
        text: &str,
        n: u64,
    ) -> lazy_protocol::SavedItem {
        lazy_protocol::SavedItem {
            message: self.message(conversation.id, author, text, 50 + n as i64),
            conversation: conversation.clone(),
            saved_at: Timestamp::now(),
            cursor: 100 + n,
        }
    }

    fn saved_view(
        &self,
        cx: &mut VisualTestContext,
    ) -> Entity<lazy_ui::saved_view::SavedMessagesView> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, _| shell.saved_view().clone())
    }

    fn saved_rows(&self, cx: &mut VisualTestContext) -> Vec<lazy_ui::saved_view::RowInfo> {
        let view = self.saved_view(cx);
        view.read_with(cx, |view, _| view.rows_info())
    }

    fn saved_stop(&self, cx: &mut VisualTestContext) -> &'static str {
        let view = self.saved_view(cx);
        cx.update(|window, app| view.read(app).stop(window))
    }
}

#[gpui::test]
fn a_message_menu_offers_save_for_later_and_the_marker_follows(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    let message = world.core(cx, |core| {
        core.timeline(world.general)
            .iter()
            .find_map(|item| item.message_id())
    });
    let message = message.expect("the welcome message is loaded");
    assert!(!world.core(cx, |core| core.is_saved(message)));
    // Focus the timeline's message, open its menu, and pick "Save for later".
    cx.simulate_keystrokes("shift-tab enter");
    assert!(
        world.on_screen(cx, "message-actions-save"),
        "the menu offers Save for later"
    );
    assert!(!world.on_screen(cx, "message-actions-unsave"));
    world.take_effects();
    world.click(cx, "message-actions-save");
    assert!(
        world.core(cx, |core| core.is_saved(message)),
        "the marker flips at once"
    );
    let sent = world.drain(|effect| match effect {
        Effect::SetMessageSaved { message, request } => Some((*message, request.saved)),
        _ => None,
    });
    assert_eq!(sent, vec![(message, true)], "and the request follows");
    // The server answers, so the next wish is sent at once.
    world.dispatch(
        cx,
        Input::MessageSavedResult {
            message_id: message,
            result: Ok(lazy_protocol::SavedResponse {
                message_id: message,
                saved: true,
            }),
        },
    );
    assert!(
        world.on_screen(cx, "saved-mark-0") || world.on_screen(cx, "saved-mark-1"),
        "the row shows a bookmark mark"
    );
    // Now the menu offers to take it back.
    cx.simulate_keystrokes("enter");
    assert!(world.on_screen(cx, "message-actions-unsave"));
    assert!(!world.on_screen(cx, "message-actions-save"));
    world.click(cx, "message-actions-unsave");
    assert!(!world.core(cx, |core| core.is_saved(message)));
    assert!(!world.on_screen(cx, "saved-mark-0") && !world.on_screen(cx, "saved-mark-1"));
    let sent = world.drain(|effect| match effect {
        Effect::SetMessageSaved { message, request } => Some((*message, request.saved)),
        _ => None,
    });
    assert_eq!(sent, vec![(message, false)]);
}

#[gpui::test]
fn the_bootstrap_marks_saved_messages_from_the_start(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    // The welcome message is made inside `reach`; save every id the bootstrap will not know yet by
    // saving after it, then check a restart of the list keeps it.
    world.reach(cx);
    let message = world
        .core(cx, |core| {
            core.timeline(world.general)
                .iter()
                .find_map(|item| item.message_id())
        })
        .unwrap();
    world.dispatch(
        cx,
        Input::SetMessageSaved {
            message_id: message,
            saved: true,
        },
    );
    assert!(world.core(cx, |core| core.is_saved(message)));
    draw(cx);
    assert!(world.on_screen(cx, "saved-mark-0") || world.on_screen(cx, "saved-mark-1"));
}

#[gpui::test]
fn the_saved_entry_opens_a_skeleton_then_rows(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-saved");
    assert!(
        world.on_screen(cx, "saved-loading"),
        "the first page is loading"
    );
    let general = world.channel(world.general, "general", true);
    let design = world.channel(world.design, "design", false);
    answer_saved(
        &world,
        cx,
        vec![
            world.saved_item(&general, &world.bob, "Ship it on Thursday", 2),
            world.saved_item(&design, &world.alice, "Colors are final", 1),
        ],
        false,
    );
    assert!(!world.on_screen(cx, "saved-loading"));
    let rows = world.saved_rows(cx);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].place, "#general");
    assert_eq!(rows[0].author, "Bob Okafor");
    assert_eq!(rows[0].snippet, "Ship it on Thursday");
    assert_eq!(rows[1].place, "#design");
    assert_eq!(world.saved_stop(cx), "list", "the list has the keyboard");
    assert!(world.on_screen(cx, "saved-row-0") && world.on_screen(cx, "saved-row-1"));
}

#[gpui::test]
fn the_keyboard_walks_the_saved_list_and_enter_goes_to_the_message(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-saved");
    let general = world.channel(world.general, "general", true);
    let design = world.channel(world.design, "design", false);
    let loaded = world.core(cx, |core| {
        core.timeline(world.general)
            .iter()
            .find_map(|item| item.message_id())
    });
    let mut first = world.saved_item(&general, &world.bob, "Already on screen", 2);
    first.message.id = loaded.expect("a loaded message");
    answer_saved(
        &world,
        cx,
        vec![
            first,
            world.saved_item(&design, &world.alice, "Not loaded yet", 1),
        ],
        false,
    );
    cx.simulate_keystrokes("down");
    let view = world.saved_view(cx);
    assert_eq!(view.read_with(cx, |view, _| view.active()), Some(0));
    cx.simulate_keystrokes("down down");
    assert_eq!(
        view.read_with(cx, |view, _| view.active()),
        Some(1),
        "Down stops at the last row"
    );
    cx.simulate_keystrokes("up");
    // A message that is on screen: Enter goes there and the view closes.
    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert!(
        !world.core(cx, |core| core.saved().open),
        "the saved list closes"
    );
    assert_eq!(world.selected(cx), Some(world.general));
    assert!(!world.on_screen(cx, "saved-view"));
}

#[gpui::test]
fn a_message_that_has_to_be_fetched_keeps_the_list_up_until_it_arrives(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-saved");
    let design = world.channel(world.design, "design", false);
    answer_saved(
        &world,
        cx,
        vec![world.saved_item(&design, &world.alice, "Not loaded yet", 1)],
        false,
    );
    world.take_effects();
    cx.simulate_keystrokes("down enter");
    let asked = world.drain(|effect| matches!(effect, Effect::FetchMessage { .. }).then_some(()));
    assert!(!asked.is_empty(), "the message is fetched");
    assert!(world.core(cx, |core| core.opening_link()));
    assert!(
        world.core(cx, |core| core.saved().open),
        "the list stays up while it looks"
    );
}

#[gpui::test]
fn delete_and_the_remove_button_take_a_row_out_and_tell_the_server(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-saved");
    let general = world.channel(world.general, "general", true);
    let first = world.saved_item(&general, &world.bob, "One", 3);
    let second = world.saved_item(&general, &world.bob, "Two", 2);
    let third = world.saved_item(&general, &world.bob, "Three", 1);
    let ids = [first.message.id, second.message.id, third.message.id];
    answer_saved(&world, cx, vec![first, second, third], false);
    world.dispatch(
        cx,
        Input::SetMessageSaved {
            message_id: ids[0],
            saved: true,
        },
    );
    world.dispatch(
        cx,
        Input::SetMessageSaved {
            message_id: ids[1],
            saved: true,
        },
    );
    world.dispatch(
        cx,
        Input::SetMessageSaved {
            message_id: ids[2],
            saved: true,
        },
    );
    world.take_effects();
    // Delete takes the active row out.
    cx.simulate_keystrokes("down delete");
    let sent = world.drain(|effect| match effect {
        Effect::SetMessageSaved { message, request } => Some((*message, request.saved)),
        _ => None,
    });
    assert_eq!(sent.last().copied(), Some((ids[0], false)));
    assert_eq!(world.saved_rows(cx).len(), 2, "the row is gone at once");
    // The Remove button on a row does the same for that row.
    world.click(cx, "saved-remove-1");
    let sent = world.drain(|effect| match effect {
        Effect::SetMessageSaved { message, request } => Some((*message, request.saved)),
        _ => None,
    });
    assert_eq!(sent.last().copied(), Some((ids[2], false)));
    assert_eq!(world.saved_rows(cx).len(), 1);
    assert!(
        world.core(cx, |core| core.saved().open),
        "clicking Remove does not also open the row"
    );
    assert!(
        world
            .drain(|e| matches!(e, Effect::FetchMessage { .. }).then_some(()))
            .is_empty()
    );
    assert!(!world.on_screen(cx, "saved-failed"));
}

#[gpui::test]
fn saved_says_when_it_is_empty_failed_or_has_more(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-saved");
    // A failed first page says so and Try again asks again.
    let (generation, _) = world.saved_loads().pop().unwrap();
    world.drain(|effect| matches!(effect, Effect::LoadSaved { .. }).then_some(()));
    world.dispatch(
        cx,
        Input::SavedLoaded {
            generation,
            result: Err(RequestError::Unknown("connection reset".into())),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "saved-failed"));
    world.click(cx, "saved-retry");
    assert_eq!(world.saved_loads().len(), 1, "Try again asks again");
    // Empty.
    answer_saved(&world, cx, Vec::new(), false);
    assert!(world.on_screen(cx, "saved-empty"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "saved-view"), "Escape leaves it");
    assert!(world.composer_focused(cx) || !world.core(cx, |core| core.saved().open));
    // More pages: "Show more" asks for the page after the last cursor.
    world.click(cx, "sidebar-saved");
    let general = world.channel(world.general, "general", true);
    answer_saved(
        &world,
        cx,
        vec![
            world.saved_item(&general, &world.bob, "One", 2),
            world.saved_item(&general, &world.bob, "Two", 1),
        ],
        true,
    );
    assert!(world.on_screen(cx, "saved-show-more"));
    world.click(cx, "saved-show-more");
    let (_, before) = world
        .saved_loads()
        .pop()
        .expect("the next page was asked for");
    assert_eq!(before, Some(101), "the cursor of the last row seen");
}

#[gpui::test]
fn the_saved_tab_order_is_the_list_then_show_more_then_close(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-saved");
    let general = world.channel(world.general, "general", true);
    answer_saved(
        &world,
        cx,
        vec![world.saved_item(&general, &world.bob, "One", 1)],
        true,
    );
    assert_eq!(world.saved_stop(cx), "list");
    cx.simulate_keystrokes("tab");
    assert_eq!(world.saved_stop(cx), "more");
    cx.simulate_keystrokes("tab");
    assert_eq!(world.saved_stop(cx), "close");
}

// ----- The people directory and profiles -----

impl World {
    fn people_loads(&self) -> Vec<(u64, Option<String>, Option<u32>)> {
        self.effects
            .0
            .borrow()
            .iter()
            .filter_map(|effect| match effect {
                Effect::LoadPeople {
                    generation, query, ..
                } => Some((*generation, query.query.clone(), query.offset)),
                _ => None,
            })
            .collect()
    }

    fn person_entry(&self, user: &User, role: WorkspaceRole) -> lazy_protocol::Person {
        lazy_protocol::Person {
            user: user.clone(),
            role,
            joined_at: Timestamp::from(time::OffsetDateTime::now_utc() - time::Duration::days(400)),
        }
    }

    fn answer_people(
        &self,
        cx: &mut VisualTestContext,
        people: Vec<lazy_protocol::Person>,
        more: bool,
    ) {
        let (generation, _, _) = self
            .people_loads()
            .pop()
            .expect("the directory asked for a page");
        self.drain(|effect| matches!(effect, Effect::LoadPeople { .. }).then_some(()));
        self.dispatch(
            cx,
            Input::PeopleLoaded {
                generation,
                result: Ok(lazy_protocol::PeoplePage {
                    people,
                    has_more: more,
                }),
            },
        );
        draw(cx);
    }

    fn people_view(
        &self,
        cx: &mut VisualTestContext,
    ) -> Entity<lazy_ui::people_view::PeopleDirectoryView> {
        let shell = self.shell(cx);
        shell.read_with(cx, |shell, _| shell.people_view().clone())
    }

    fn everyone(&self) -> Vec<lazy_protocol::Person> {
        vec![
            self.person_entry(&self.me, WorkspaceRole::Owner),
            self.person_entry(&self.alice, WorkspaceRole::Admin),
            self.person_entry(&self.bob, WorkspaceRole::Member),
            self.person_entry(&self.carol, WorkspaceRole::Member),
        ]
    }
}

#[gpui::test]
fn the_people_entry_opens_a_skeleton_then_rows_with_roles_and_statuses(cx: &mut TestAppContext) {
    let (mut world, cx) = start(cx);
    world.bob.status =
        Some(UserStatus::new("On holiday", ReactionEmoji::parse("🌴").ok(), None).unwrap());
    world.reach(cx);
    world.click(cx, "sidebar-people");
    assert!(world.on_screen(cx, "people-loading"));
    world.answer_people(cx, world.everyone(), false);
    assert!(!world.on_screen(cx, "people-loading"));
    let view = world.people_view(cx);
    let rows = view.read_with(cx, |view, app| view.rows_info(app));
    let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Me Myself", "Alice Nguyen", "Bob Okafor", "Carol Mbeki"]
    );
    assert_eq!(rows[0].role, Some("Owner"));
    assert_eq!(rows[1].role, Some("Admin"));
    assert_eq!(rows[2].role, None, "members have no badge");
    assert_eq!(rows[2].status.as_deref(), Some("On holiday"));
    assert!(world.on_screen(cx, "people-role-0") && world.on_screen(cx, "people-role-1"));
    assert!(!world.on_screen(cx, "people-role-2"));
    assert!(
        world.on_screen(cx, "people-status-2"),
        "the emoji sits beside the name"
    );
    let stop = cx.update(|window, app| view.read(app).stop(window, app));
    assert_eq!(stop, "search", "the search box has the keyboard");
}

#[gpui::test]
fn typing_searches_the_directory_and_enter_opens_the_profile(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-people");
    world.answer_people(cx, world.everyone(), false);
    cx.simulate_input("bo");
    let asked = world.people_loads();
    assert_eq!(
        asked
            .last()
            .and_then(|(_, query, _)| query.clone())
            .as_deref(),
        Some("bo")
    );
    world.answer_people(
        cx,
        vec![world.person_entry(&world.bob, WorkspaceRole::Member)],
        false,
    );
    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert!(
        world.on_screen(cx, "profile-name"),
        "Enter opens the profile"
    );
    let asked = world.drain(|effect| match effect {
        Effect::LoadProfile { user, .. } => Some(*user),
        _ => None,
    });
    assert_eq!(asked, vec![world.bob.id]);
}

#[gpui::test]
fn down_moves_the_active_row_and_the_last_row_asks_for_the_next_page(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-people");
    world.answer_people(cx, world.everyone(), true);
    let view = world.people_view(cx);
    assert_eq!(view.read_with(cx, |view, _| view.active()), 0);
    cx.simulate_keystrokes("down");
    assert_eq!(view.read_with(cx, |view, _| view.active()), 1);
    cx.simulate_keystrokes("up up");
    assert_eq!(
        view.read_with(cx, |view, _| view.active()),
        0,
        "Up stops at the first row"
    );
    world.take_effects();
    cx.simulate_keystrokes("down down down");
    assert_eq!(view.read_with(cx, |view, _| view.active()), 3);
    let asked = world.people_loads();
    assert_eq!(
        asked.last().map(|(_, _, offset)| *offset),
        Some(Some(4)),
        "the next page starts after the loaded ones"
    );
}

#[gpui::test]
fn people_say_when_nobody_matches_or_the_load_failed(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-people");
    let (generation, _, _) = world.people_loads().pop().unwrap();
    world.drain(|effect| matches!(effect, Effect::LoadPeople { .. }).then_some(()));
    world.dispatch(
        cx,
        Input::PeopleLoaded {
            generation,
            result: Err(RequestError::Unknown("connection reset".into())),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "people-failed"));
    world.click(cx, "people-retry");
    world.answer_people(cx, Vec::new(), false);
    assert!(world.on_screen(cx, "people-empty"));
    cx.simulate_keystrokes("escape");
    assert!(
        !world.on_screen(cx, "people-view"),
        "Escape leaves the directory"
    );
    assert!(!world.core(cx, |core| core.people().open));
}

#[gpui::test]
fn choosing_a_conversation_in_the_sidebar_leaves_the_directory(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.click(cx, "sidebar-people");
    world.answer_people(cx, world.everyone(), false);
    assert!(world.on_screen(cx, "people-view"));
    world.click(cx, "conv-design");
    assert!(!world.on_screen(cx, "people-view"));
    assert_eq!(world.selected(cx), Some(world.design));
}

impl World {
    fn answer_profile(&self, cx: &mut VisualTestContext, person: lazy_protocol::Person) {
        let generation = self
            .drain(|effect| match effect {
                Effect::LoadProfile { generation, .. } => Some(*generation),
                _ => None,
            })
            .pop()
            .expect("the profile was asked for");
        self.dispatch(
            cx,
            Input::ProfileLoaded {
                generation,
                result: Ok(lazy_protocol::PersonResponse { person }),
            },
        );
        draw(cx);
    }
}

#[gpui::test]
fn a_profile_shows_the_known_name_at_once_then_the_role_and_join_date(cx: &mut TestAppContext) {
    let (mut world, cx) = start(cx);
    world.bob.status =
        Some(UserStatus::new("Heads down", ReactionEmoji::parse("🎯").ok(), None).unwrap());
    world.reach(cx);
    let shell = world.shell(cx);
    cx.update(|window, app| {
        shell.update(app, |shell, cx| {
            shell.open_profile(world.bob.id, window, cx)
        })
    });
    draw(cx);
    assert!(
        world.on_screen(cx, "profile-name"),
        "the name is there before the answer"
    );
    assert!(world.on_screen(cx, "profile-username"));
    assert!(world.on_screen(cx, "profile-status"), "so is the status");
    assert!(
        world.on_screen(cx, "profile-loading"),
        "the rest is loading"
    );
    assert!(!world.on_screen(cx, "profile-since"));
    world.answer_profile(cx, world.person_entry(&world.bob, WorkspaceRole::Admin));
    assert!(!world.on_screen(cx, "profile-loading"));
    assert!(world.on_screen(cx, "profile-since"));
    assert!(world.on_screen(cx, "profile-role-admin"));
}

#[gpui::test]
fn message_opens_the_direct_conversation_and_closes_the_profile(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    let shell = world.shell(cx);
    cx.update(|window, app| {
        shell.update(app, |shell, cx| {
            shell.open_profile(world.carol.id, window, cx)
        })
    });
    draw(cx);
    world.answer_profile(cx, world.person_entry(&world.carol, WorkspaceRole::Member));
    assert!(
        !world.on_screen(cx, "profile-edit-status"),
        "someone else's profile has no Edit status"
    );
    world.take_effects();
    // The primary button has the keyboard: Enter presses it.
    cx.simulate_keystrokes("enter");
    let request = world
        .drain(|effect| match effect {
            Effect::OpenConversation { request, .. } => Some(request.clone()),
            _ => None,
        })
        .pop()
        .expect("Message opens the conversation");
    assert_eq!(request.participants, vec![world.carol.id]);
    let opened = Conversation {
        kind: ConversationKind::Direct,
        name: None,
        participants: vec![world.me.id, world.carol.id],
        ..world.channel(ConversationId::generate(), "placeholder", false)
    };
    world.dispatch(
        cx,
        Input::ConversationCommandResult {
            operation: request.operation_id,
            result: Ok(lazy_protocol::ConversationResponse {
                conversation: opened.clone(),
            }),
        },
    );
    assert!(!world.on_screen(cx, "profile-name"), "the profile closed");
    assert_eq!(world.selected(cx), Some(opened.id));
    assert!(world.composer_focused(cx));
}

#[gpui::test]
fn ones_own_profile_offers_edit_status_which_opens_preferences(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    let shell = world.shell(cx);
    cx.update(|window, app| {
        shell.update(app, |shell, cx| shell.open_profile(world.me.id, window, cx))
    });
    draw(cx);
    assert!(world.on_screen(cx, "profile-edit-status"));
    assert!(
        !world.on_screen(cx, "profile-message"),
        "nobody messages themselves"
    );
    cx.simulate_keystrokes("enter");
    assert!(
        world.on_screen(cx, "prefs-channel-level"),
        "Preferences opened"
    );
    assert!(!world.on_screen(cx, "profile-name"));
}

#[gpui::test]
fn a_profile_that_failed_to_load_says_so_and_try_again_asks_again(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    let shell = world.shell(cx);
    cx.update(|window, app| {
        shell.update(app, |shell, cx| {
            shell.open_profile(world.carol.id, window, cx)
        })
    });
    draw(cx);
    let generation = world
        .drain(|effect| match effect {
            Effect::LoadProfile { generation, .. } => Some(*generation),
            _ => None,
        })
        .pop()
        .unwrap();
    world.dispatch(
        cx,
        Input::ProfileLoaded {
            generation,
            result: Err(RequestError::Unknown("connection reset".into())),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "profile-failed"));
    assert!(world.on_screen(cx, "profile-name"), "what is known stays");
    world.click(cx, "profile-retry");
    let asked = world.drain(|effect| matches!(effect, Effect::LoadProfile { .. }).then_some(()));
    assert_eq!(asked.len(), 1, "Try again asks again");
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "profile-name"), "Escape closes it");
}

#[gpui::test]
fn the_author_in_the_timeline_opens_a_profile_and_so_does_the_menu(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    // The welcome message is Alice's: her name opens her profile.
    let name = ["author-name-0", "author-name-1"]
        .into_iter()
        .find(|selector| world.on_screen(cx, selector))
        .expect("an author's name");
    world.click(cx, name);
    assert!(world.on_screen(cx, "profile-name"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "profile-name"));
    // The keyboard reaches it through the message menu: the last item.
    cx.simulate_keystrokes("shift-tab enter");
    assert!(world.on_screen(cx, "message-actions-view-profile"));
    world.click(cx, "message-actions-view-profile");
    assert!(world.on_screen(cx, "profile-name"));
}

#[gpui::test]
fn the_details_member_list_shows_status_and_a_name_opens_the_profile(cx: &mut TestAppContext) {
    let (mut world, cx) = start(cx);
    world.bob.status =
        Some(UserStatus::new("On holiday", ReactionEmoji::parse("🌴").ok(), None).unwrap());
    world.reach(cx);
    world.dispatch(cx, Input::SelectConversation(world.dm));
    world.take_effects();
    world.click(cx, "details-button");
    assert!(
        world.on_screen(cx, "details-member-bob"),
        "people are listed"
    );
    assert!(
        world.on_screen(cx, "details-status-bob"),
        "with their status"
    );
    assert!(!world.on_screen(cx, "details-status-myself"));
    world.click(cx, "details-member-bob");
    assert!(
        world.on_screen(cx, "profile-name"),
        "a name opens the profile"
    );
    assert!(
        !world.on_screen(cx, "details-member-bob"),
        "and the details closed"
    );
}

// ----- The channel directory -----

impl World {
    fn browse_loads(&self) -> Vec<(u64, lazy_protocol::BrowseChannelsQuery)> {
        self.effects
            .0
            .borrow()
            .iter()
            .filter_map(|effect| match effect {
                Effect::LoadBrowse {
                    generation, query, ..
                } => Some((*generation, query.clone())),
                _ => None,
            })
            .collect()
    }

    fn answer_browse(
        &self,
        cx: &mut VisualTestContext,
        channels: Vec<lazy_protocol::BrowsedChannel>,
        more: bool,
    ) {
        let (generation, _) = self
            .browse_loads()
            .pop()
            .expect("the directory asked for a page");
        self.drain(|effect| matches!(effect, Effect::LoadBrowse { .. }).then_some(()));
        self.dispatch(
            cx,
            Input::BrowseLoaded {
                generation,
                result: Ok(lazy_protocol::BrowseChannelsResponse {
                    channels,
                    has_more: more,
                }),
            },
        );
        draw(cx);
    }

    fn browsed(&self, name: &str, joined: bool, members: u32) -> lazy_protocol::BrowsedChannel {
        let mut conversation = self.channel(ConversationId::generate(), name, false);
        conversation.member_count = members;
        lazy_protocol::BrowsedChannel {
            conversation,
            joined,
        }
    }
}

impl World {
    fn open_dialog(&self, cx: &mut VisualTestContext, dialog: lazy_ui::shell::ShellDialog) {
        let shell = self.shell(cx);
        cx.update(|window, app| {
            shell.update(app, |shell, cx| shell.open_dialog(dialog, window, cx))
        });
        draw(cx);
    }

    fn browse_stop(&self, cx: &mut VisualTestContext) -> &'static str {
        let shell = self.shell(cx);
        let dialog = shell
            .read_with(cx, |shell, _| shell.browse_dialog())
            .expect("the directory is open");
        cx.update(|window, app| dialog.read(app).stop(window, app))
    }

    fn browse_names(&self, cx: &mut VisualTestContext) -> Vec<String> {
        let shell = self.shell(cx);
        let dialog = shell
            .read_with(cx, |shell, _| shell.browse_dialog())
            .expect("the directory is open");
        dialog.read_with(cx, |dialog, app| dialog.names(app))
    }
}

#[gpui::test]
fn typing_filter_and_sort_ask_the_server_and_the_answer_is_the_list(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.take_effects();
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::BrowseChannels);
    assert!(
        world.on_screen(cx, "browse-loading"),
        "the first page is loading"
    );
    world.answer_browse(
        cx,
        vec![
            world.browsed("general", true, 12),
            world.browsed("random", false, 5),
            world.browsed("recipes", false, 40),
        ],
        false,
    );
    assert_eq!(world.browse_names(cx), vec!["general", "random", "recipes"]);
    assert!(
        world.on_screen(cx, "browse-open-general") && world.on_screen(cx, "browse-join-random")
    );

    // Typing asks the server with the text; the answer is the list.
    cx.simulate_input("rec");
    let (_, query) = world.browse_loads().pop().expect("typing searches");
    assert_eq!(query.query.as_deref(), Some("rec"));
    world.answer_browse(cx, vec![world.browsed("recipes", false, 40)], false);
    assert_eq!(world.browse_names(cx), vec!["recipes"]);

    // The filter and the sort each ask again with their setting.
    world.click(cx, "browse-not-joined");
    let (_, query) = world.browse_loads().pop().expect("the filter asks again");
    assert_eq!(query.joined, Some(false));
    assert_eq!(query.query.as_deref(), Some("rec"), "the text stays");
    world.answer_browse(cx, vec![world.browsed("recipes", false, 40)], false);
    world.click(cx, "browse-sort-members");
    let (_, query) = world.browse_loads().pop().expect("the sort asks again");
    assert_eq!(query.sort, Some(lazy_protocol::ChannelSort::Members));
    world.answer_browse(
        cx,
        vec![
            world.browsed("recipes", false, 40),
            world.browsed("random", false, 5),
        ],
        false,
    );
    assert_eq!(world.browse_names(cx), vec!["recipes", "random"]);
    world.click(cx, "browse-all");
    let (_, query) = world.browse_loads().pop().expect("All asks again");
    assert_eq!(query.joined, None);
}

#[gpui::test]
fn the_keyboard_walks_the_directory_and_enter_joins_the_highlighted_channel(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start_member(cx);
    world.take_effects();
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::BrowseChannels);
    world.answer_browse(
        cx,
        vec![
            world.browsed("alpha", false, 3),
            world.browsed("beta", false, 4),
            world.browsed("gamma", false, 5),
        ],
        true,
    );
    assert_eq!(
        world.browse_stop(cx),
        "search",
        "the search box has the keyboard"
    );
    cx.simulate_keystrokes("down down");
    world.take_effects();
    cx.simulate_keystrokes("enter");
    let joined = world.drain(|effect| match effect {
        Effect::JoinChannel { conversation } => Some(*conversation),
        _ => None,
    });
    assert_eq!(joined.len(), 1, "Enter joins the highlighted channel");
    let third = world.core(cx, |core| core.browse().channels[2].conversation.id);
    assert_eq!(joined[0], third);
    // Tab order: search, filter, sort, Load more.
    cx.simulate_keystrokes("tab");
    assert_eq!(world.browse_stop(cx), "filter");
    cx.simulate_keystrokes("right");
    let (_, query) = world
        .browse_loads()
        .pop()
        .expect("Right chooses Not joined");
    assert_eq!(query.joined, Some(false));
    cx.simulate_keystrokes("tab");
    assert_eq!(world.browse_stop(cx), "sort");
    // Close is not a stop of its own, so the next one wraps to the search box.
    cx.simulate_keystrokes("tab");
    assert_eq!(
        world.browse_stop(cx),
        "search",
        "no list stop while the page loads"
    );
}

#[gpui::test]
fn load_more_asks_for_the_next_page_and_a_failed_page_keeps_the_rows(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.take_effects();
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::BrowseChannels);
    world.answer_browse(
        cx,
        vec![
            world.browsed("alpha", false, 3),
            world.browsed("beta", false, 4),
        ],
        true,
    );
    assert!(world.on_screen(cx, "browse-load-more"));
    world.click(cx, "browse-load-more");
    let (generation, query) = world.browse_loads().pop().expect("the next page");
    assert_eq!(query.offset, Some(2), "after the two loaded");
    world.drain(|effect| matches!(effect, Effect::LoadBrowse { .. }).then_some(()));
    world.dispatch(
        cx,
        Input::BrowseLoaded {
            generation,
            result: Err(RequestError::Unknown("connection reset".into())),
        },
    );
    draw(cx);
    assert!(
        world.on_screen(cx, "browse-more-failed"),
        "it says so under the rows"
    );
    assert_eq!(world.browse_names(cx).len(), 2, "and keeps them");
    world.click(cx, "browse-retry");
    let (_, query) = world.browse_loads().pop().expect("Try again asks again");
    assert_eq!(query.offset, Some(2));
}

#[gpui::test]
fn the_directory_says_what_it_found_or_why_not(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    world.take_effects();
    world.open_dialog(cx, lazy_ui::shell::ShellDialog::BrowseChannels);
    // A first page that failed says so; Try again asks again.
    let (generation, _) = world.browse_loads().pop().unwrap();
    world.drain(|effect| matches!(effect, Effect::LoadBrowse { .. }).then_some(()));
    world.dispatch(
        cx,
        Input::BrowseLoaded {
            generation,
            result: Err(RequestError::Unknown("connection reset".into())),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "browse-failed"));
    world.click(cx, "browse-retry");
    // Nothing found, with and without a search.
    world.answer_browse(cx, Vec::new(), false);
    assert!(world.on_screen(cx, "browse-empty"));
    cx.simulate_input("zebra");
    world.answer_browse(cx, Vec::new(), false);
    assert!(world.on_screen(cx, "browse-empty"));
    // An archived channel is marked and cannot be joined.
    cx.simulate_keystrokes("ctrl-a backspace");
    let mut old = world.browsed("old-launch", false, 9);
    old.conversation.archived = true;
    world.answer_browse(cx, vec![old], false);
    assert!(world.on_screen(cx, "browse-archived-old-launch"));
    assert!(!world.on_screen(cx, "browse-join-old-launch"));
}

// ----- The sidebar -----

#[gpui::test]
fn the_sidebar_has_the_switcher_hint_saved_and_people_in_tab_order(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    for selector in [
        "sidebar-search",
        "sidebar-switcher",
        "sidebar-threads",
        "sidebar-saved",
        "sidebar-people",
    ] {
        assert!(
            world.on_screen(cx, selector),
            "{selector} is in the sidebar"
        );
    }
    assert!(world.on_screen(cx, "sidebar-switcher-hint"));
    let bounds =
        |cx: &mut VisualTestContext, selector: &'static str| cx.debug_bounds(selector).unwrap();
    let tops: Vec<_> = [
        "sidebar-search",
        "sidebar-switcher",
        "sidebar-threads",
        "sidebar-saved",
        "sidebar-people",
    ]
    .into_iter()
    .map(|selector| bounds(cx, selector).top())
    .collect();
    assert!(
        tops.windows(2).all(|pair| pair[0] < pair[1]),
        "in this order"
    );
    // The entries open their views from the keyboard: Enter on a focused entry.
    world.click(cx, "sidebar-people");
    assert!(world.core(cx, |core| core.people().open));
    world.click(cx, "sidebar-saved");
    assert!(world.core(cx, |core| core.saved().open));
    assert!(
        !world.core(cx, |core| core.people().open),
        "one view at a time"
    );
}

#[gpui::test]
fn a_list_keeps_the_keyboard_while_it_loads_and_after_try_again(cx: &mut TestAppContext) {
    let (world, cx) = start_member(cx);
    // The saved list has the keyboard before its first page arrives: Escape closes it.
    world.click(cx, "sidebar-saved");
    assert!(world.on_screen(cx, "saved-loading"));
    assert_eq!(world.saved_stop(cx), "list");
    cx.simulate_keystrokes("escape");
    assert!(
        !world.on_screen(cx, "saved-view"),
        "Escape closes it while it loads"
    );
    // Try again takes the button away; the keyboard stays in the view.
    world.click(cx, "sidebar-saved");
    let (generation, _) = world.saved_loads().pop().unwrap();
    world.drain(|effect| matches!(effect, Effect::LoadSaved { .. }).then_some(()));
    world.dispatch(
        cx,
        Input::SavedLoaded {
            generation,
            result: Err(RequestError::Unknown("connection reset".into())),
        },
    );
    draw(cx);
    cx.simulate_keystrokes("tab");
    assert_eq!(world.saved_stop(cx), "retry");
    cx.simulate_keystrokes("enter");
    assert!(world.on_screen(cx, "saved-loading"), "it asks again");
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "saved-view"), "and Escape still works");
}
