//! Emoji flows on GPUI's test platform: the picker, reaction chips, the keyboard path to reactions,
//! and shortcode suggestions in the composer and the editor. The real views and the real core run;
//! a recording effect sink stands in for the runtime.
//!
//! Every keyboard claim here is made with real key events through GPUI's dispatch. The test
//! platform gives every character the same width and never reports a window as active, so these
//! prove behavior and wiring; real glyph layout is checked in the offscreen renders. Nothing here
//! exercises a real input method (composition is simulated), screen reader, or display.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    AppContext as _, Entity, EntityInputHandler as _, Modifiers, TestAppContext, VisualTestContext,
    WindowAppearance,
};
use lazy_client_core::{Effect, Input, StartupState};
use lazy_domain::emoji::{self, SkinTone};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, ReactionEmoji, Timestamp, UserId, Username, WorkspaceId,
    WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, Event, EventEnvelope, HistoryPage,
    Message, PROTOCOL_VERSION, ReactionSummary, ServerFrame, User, Workspace, WorkspaceBootstrap,
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
    zoe: User,
    workspace: WorkspaceId,
    general: ConversationId,
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
        zoe: person("zoe"),
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
    };
    (world, cx)
}

fn emoji(glyph: &str) -> ReactionEmoji {
    ReactionEmoji::parse(glyph).unwrap()
}

impl World {
    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    fn conversation(&self, archived: bool) -> Conversation {
        Conversation {
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
            member_count: 3,
        }
    }

    fn bootstrap(&self) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: Vec::new(),
                notification_settings: Default::default(),
                conversation_notification_prefs: Vec::new(),
                custom_emoji: Vec::new(),
                threads: Vec::new(),
                groups: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role: WorkspaceRole::Member,
                conversations: vec![self.conversation(false)],
                user_state: vec![ConversationUserState {
                    conversation_id: self.general,
                    favorite: false,
                    last_read: EventSeq::ZERO,
                    unread_count: 0,
                    mention_count: 0,
                }],
                members: vec![self.me.clone(), self.bob.clone(), self.zoe.clone()],
                head: EventSeq::ZERO,
            }],
        }
    }

    fn message(&self, author: &User, text: &str, seq: i64) -> Message {
        Message {
            attachments: vec![],
            thread_root: None,
            in_channel: false,
            thread: None,
            mentions: Default::default(),
            id: MessageId::generate(),
            conversation_id: self.general,
            author_id: author.id,
            document: MessageDocument::from_markdown(text).unwrap(),
            created_at: Timestamp::now(),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            rich: None,
            deleted: false,
        }
    }

    /// `message` with these reactions, each from the people given.
    fn reacted(&self, mut message: Message, spec: &[(&str, &[&User])]) -> Message {
        for (glyph, users) in spec {
            message.reactions.push(ReactionSummary {
                emoji: emoji(glyph),
                users: users.iter().map(|user| user.id).collect(),
            });
        }
        message.reactions_as_of = EventSeq::new(message.seq.get() + 1).unwrap();
        message
    }

    fn reach(&self, cx: &mut VisualTestContext, messages: Vec<Message>) {
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(cx, Input::BootstrapLoaded(Box::new(self.bootstrap())));
        let generation = self
            .take_effects()
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
                    messages,
                    has_more: false,
                }),
            },
        );
        draw(cx);
    }

    fn draft(&self, cx: &VisualTestContext) -> String {
        self.model
            .read_with(cx, |m, _| m.core().draft(self.general).to_owned())
    }

    fn prefs(&self, cx: &VisualTestContext) -> lazy_client_core::EmojiPrefs {
        self.model
            .read_with(cx, |m, _| m.core().emoji_prefs().clone())
    }

    fn item(&self, cx: &VisualTestContext, index: usize) -> lazy_client_core::TimelineItem {
        self.model
            .read_with(cx, |m, _| m.core().timeline(self.general)[index].clone())
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

    fn hover_row(&self, cx: &mut VisualTestContext, index: usize) {
        draw(cx);
        let row = cx
            .debug_bounds(Box::leak(format!("message-row-{index}").into_boxed_str()))
            .unwrap_or_else(|| panic!("row {index} is not on screen"));
        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        cx.run_until_parked();
    }

    fn picker_is_open(&self, cx: &mut VisualTestContext) -> bool {
        self.on_screen(cx, "emoji-picker")
    }

    fn open_composer_picker(&self, cx: &mut VisualTestContext) {
        self.click(cx, "composer-emoji");
        assert!(self.picker_is_open(cx), "the picker opens");
    }

    /// Reaction effects, as (emoji, whether to be present).
    fn reaction_effects(&self) -> Vec<(ReactionEmoji, bool)> {
        self.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::SetReaction { request, .. } => Some((request.emoji, request.present)),
                _ => None,
            })
            .collect()
    }

    fn sent_texts(&self) -> Vec<String> {
        self.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::PersistSubmission(entry) => Some(entry.document.plain_text()),
                _ => None,
            })
            .collect()
    }

    fn event(&self, cx: &mut VisualTestContext, seq: i64, event: Event) {
        self.dispatch(
            cx,
            Input::Frame(ServerFrame::Event(EventEnvelope {
                workspace_id: self.workspace,
                seq: EventSeq::new(seq).unwrap(),
                occurred_at: Timestamp::now(),
                event,
            })),
        );
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn clipboard(cx: &VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

/// The first emoji of group number `n`, and the last of group `n`, as the picker lists them.
fn group_first(n: usize) -> &'static str {
    emoji::in_group(emoji::groups().nth(n).unwrap())
        .next()
        .unwrap()
        .as_str()
}

fn group_last(n: usize) -> &'static str {
    emoji::in_group(emoji::groups().nth(n).unwrap())
        .last()
        .unwrap()
        .as_str()
}

// ---- The picker, opened from the composer -----------------------------------------------------------

#[gpui::test]
fn the_emoji_button_opens_the_picker_and_typing_then_enter_inserts_the_first_result(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    assert!(!world.picker_is_open(cx));
    world.open_composer_picker(cx);

    // The search field has focus: typing goes to it, and the first result is highlighted.
    cx.simulate_input("tada");
    assert_eq!(world.draft(cx), "", "nothing was typed into the composer");
    cx.simulate_keystrokes("enter");
    assert!(!world.picker_is_open(cx), "choosing closes the picker");
    assert_eq!(world.draft(cx), "🎉");
    assert_eq!(
        world.prefs(cx).recent,
        vec![emoji("🎉")],
        "the choice is remembered"
    );

    // Focus is back in the composer, so typing continues there.
    cx.simulate_input(" ok");
    assert_eq!(world.draft(cx), "🎉 ok");
}

#[gpui::test]
fn the_grid_is_driven_by_the_arrow_keys_and_chosen_with_enter(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);

    // Down from the search field enters the grid on its first emoji.
    cx.simulate_keystrokes("down right right enter");
    assert_eq!(
        world.draft(cx),
        emoji::in_group(emoji::groups().next().unwrap())
            .nth(2)
            .unwrap()
            .as_str()
    );
}

/// Opens the picker, presses `keys`, and checks which emoji Enter chose. (Each case has its own
/// world: a choice is remembered, and the recently used row would shift the grid.)
fn keys_choose(cx: &mut TestAppContext, keys: &str, expected: &str) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);
    cx.simulate_keystrokes(keys);
    assert_eq!(world.draft(cx), expected, "{keys}");
}

#[gpui::test]
fn page_down_goes_to_the_start_of_the_next_group(cx: &mut TestAppContext) {
    keys_choose(cx, "down pagedown enter", group_first(1));
}

#[gpui::test]
fn end_goes_to_the_last_emoji_and_home_to_the_first(cx: &mut TestAppContext) {
    keys_choose(cx, "down end enter", group_last(8));
    keys_choose(cx, "down end home enter", group_first(0));
}

#[gpui::test]
fn page_up_goes_to_the_start_of_this_group_then_the_one_before(cx: &mut TestAppContext) {
    keys_choose(cx, "down pagedown right right pageup enter", group_first(1));
    keys_choose(cx, "down pagedown pagedown pageup enter", group_first(1));
}

#[gpui::test]
fn tab_cycles_search_tabs_grid_and_tones_and_the_tabs_jump_to_a_group(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);

    // Search -> tabs, which start on Custom (always the first, even with nothing in it). Right on
    // the tabs jumps to the next group; Down enters the grid there.
    cx.simulate_keystrokes("tab right right down enter");
    assert_eq!(
        world.draft(cx),
        group_first(1),
        "People & body starts the second group"
    );
    cx.simulate_keystrokes("ctrl-a backspace");

    // Search -> tabs -> grid -> tones -> search again: four Tabs come back to the search field,
    // where typing goes into the field.
    world.open_composer_picker(cx);
    cx.simulate_keystrokes("tab tab tab tab");
    cx.simulate_input("rocket");
    cx.simulate_keystrokes("enter");
    assert_eq!(world.draft(cx), "🚀");
    cx.simulate_keystrokes("ctrl-a backspace");

    // Shift+Tab goes the other way: from the search field to the skin tones.
    world.open_composer_picker(cx);
    cx.simulate_keystrokes("shift-tab right");
    assert_eq!(
        world.prefs(cx).skin_tone,
        1,
        "Right on the tones picks the first tone"
    );
}

#[gpui::test]
fn a_chosen_skin_tone_is_saved_and_applies_to_what_is_inserted(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);
    cx.simulate_keystrokes("tab tab tab right right");
    assert_eq!(world.prefs(cx).skin_tone, 2);
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::PersistEmojiPrefs(p) if p.skin_tone == 2)),
        "the choice is handed to the runtime to save"
    );

    cx.simulate_keystrokes("tab");
    cx.simulate_input("waving hand");
    cx.simulate_keystrokes("enter");
    let expected =
        emoji::with_tone(emoji::lookup("👋").unwrap(), emoji::skin_tone_from_index(2)).as_str();
    assert_eq!(world.draft(cx), expected);
    assert_eq!(emoji::skin_tone_from_index(2), SkinTone::MediumLight);
}

#[gpui::test]
fn recently_used_emoji_come_first(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.dispatch(cx, Input::RecordEmojiUse(emoji("🚀")));
    world.dispatch(cx, Input::RecordEmojiUse(emoji("🎉")));
    world.open_composer_picker(cx);
    cx.simulate_keystrokes("down enter");
    assert_eq!(
        world.draft(cx),
        "🎉",
        "the most recent pick is the first cell"
    );
    assert_eq!(world.prefs(cx).recent, vec![emoji("🎉"), emoji("🚀")]);
}

#[gpui::test]
fn a_search_with_no_matches_says_so_and_enter_does_nothing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);
    cx.simulate_input("zzzzqqqq");
    assert!(!world.on_screen(cx, "emoji-cell-0"), "no cells");
    cx.simulate_keystrokes("enter");
    assert!(world.picker_is_open(cx), "the picker stays open");
    assert_eq!(world.draft(cx), "");
    // Backspacing the search away shows everything again.
    cx.simulate_keystrokes("ctrl-a backspace");
    assert!(world.on_screen(cx, "emoji-cell-0"));
}

#[gpui::test]
fn escape_closes_the_picker_and_returns_focus_to_the_composer(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);
    cx.simulate_keystrokes("escape");
    assert!(!world.picker_is_open(cx));
    cx.simulate_input("still typing");
    assert_eq!(world.draft(cx), "still typing");
    assert!(world.reaction_effects().is_empty());
}

#[gpui::test]
fn escape_during_an_input_method_composition_belongs_to_the_composition(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.open_composer_picker(cx);
    let search = world.root.read_with(cx, |root, cx| {
        root.shell()
            .unwrap()
            .read(cx)
            .emoji_picker()
            .unwrap()
            .read(cx)
            .search_input()
            .clone()
    });
    // A composition in progress (simulated, not a real input method).
    search.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "かな", None, window, cx)
    });
    assert!(search.read_with(cx, |input, _| input.is_composing()));
    cx.simulate_keystrokes("escape");
    assert!(world.picker_is_open(cx), "Escape did not close it");

    search.update_in(cx, |input, window, cx| input.unmark_text(window, cx));
    cx.simulate_keystrokes("escape");
    assert!(!world.picker_is_open(cx), "and now it does");
}

#[gpui::test]
fn a_click_on_a_cell_chooses_it_and_a_click_elsewhere_closes_the_picker(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.bob, "hello", 1)]);
    world.open_composer_picker(cx);
    world.click(cx, "emoji-cell-3");
    assert_eq!(
        world.draft(cx),
        emoji::in_group(emoji::groups().next().unwrap())
            .nth(3)
            .unwrap()
            .as_str()
    );
    assert!(!world.picker_is_open(cx));

    world.open_composer_picker(cx);
    // Outside the popover: on a message.
    draw(cx);
    let row = cx.debug_bounds("message-row-0").unwrap();
    cx.simulate_click(row.center(), Modifiers::default());
    assert!(!world.picker_is_open(cx), "clicking away closes it");
}

#[gpui::test]
fn the_picker_opens_without_building_every_emoji_as_an_element(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let started = Instant::now();
    world.open_composer_picker(cx);
    let elapsed = started.elapsed();
    // About 1,900 emoji in about 215 rows are laid out as data; only the visible rows are elements.
    assert!(world.on_screen(cx, "emoji-cell-0"));
    assert!(
        !world.on_screen(cx, "emoji-cell-1500"),
        "an emoji far down the grid is not built"
    );
    eprintln!("opening the picker took {elapsed:?} (debug build, test platform)");
    assert!(
        elapsed.as_millis() < 2_000,
        "opening took {elapsed:?}, far longer than building the visible rows should"
    );
}

// ---- Reaction chips ------------------------------------------------------------------------------

fn chat_with_reactions(world: &World, cx: &mut VisualTestContext) {
    let reacted = world.reacted(
        world.message(&world.bob, "ship it", 1),
        &[
            ("👍", &[&world.bob, &world.zoe, &world.me]),
            ("🎉", &[&world.bob]),
        ],
    );
    world.reach(cx, vec![reacted]);
}

#[gpui::test]
fn chips_show_what_people_used_and_clicking_one_toggles_the_persons_own(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    chat_with_reactions(&world, cx);
    assert!(world.on_screen(cx, "reaction-chips"));
    assert!(world.on_screen(cx, "reaction-chip-0") && world.on_screen(cx, "reaction-chip-1"));
    assert!(!world.on_screen(cx, "reaction-chip-2"));
    assert!(world.on_screen(cx, "add-reaction-chip"));

    // 🎉 is not mine: clicking adds it.
    world.take_effects();
    world.click(cx, "reaction-chip-1");
    assert_eq!(world.reaction_effects(), vec![(emoji("🎉"), true)]);
    assert!(
        world.item(cx, 0).reacted_by(world.me.id, emoji("🎉")),
        "shown at once"
    );

    // 👍 is mine: clicking takes it back.
    world.click(cx, "reaction-chip-0");
    assert_eq!(world.reaction_effects(), vec![(emoji("👍"), false)]);
    assert!(!world.item(cx, 0).reacted_by(world.me.id, emoji("👍")));
}

#[gpui::test]
fn the_add_button_and_the_toolbar_open_the_picker_and_choosing_reacts(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    chat_with_reactions(&world, cx);

    world.click(cx, "add-reaction-chip");
    assert!(world.picker_is_open(cx), "the chip's + opens the picker");
    cx.simulate_input("rocket");
    cx.simulate_keystrokes("enter");
    assert!(!world.picker_is_open(cx));
    assert_eq!(world.reaction_effects(), vec![(emoji("🚀"), true)]);
    assert_eq!(world.prefs(cx).recent, vec![emoji("🚀")]);
    assert_eq!(world.draft(cx), "", "the composer was not touched");

    // The hover toolbar's React button does the same.
    world.hover_row(cx, 0);
    world.click(cx, "react-message");
    assert!(world.picker_is_open(cx));
    cx.simulate_input("tada");
    cx.simulate_keystrokes("enter");
    assert_eq!(world.reaction_effects(), vec![(emoji("🎉"), true)]);
}

#[gpui::test]
fn choosing_an_emoji_the_person_already_used_never_takes_it_back(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    chat_with_reactions(&world, cx);
    world.take_effects();
    world.click(cx, "add-reaction-chip");
    cx.simulate_input("thumbs up");
    cx.simulate_keystrokes("enter");
    assert!(!world.picker_is_open(cx));
    assert_eq!(
        world.reaction_effects(),
        vec![],
        "👍 was already theirs: nothing changes"
    );
    assert!(world.item(cx, 0).reacted_by(world.me.id, emoji("👍")));
}

#[gpui::test]
fn a_message_that_cannot_be_reacted_to_shows_chips_but_no_controls(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    chat_with_reactions(&world, cx);
    world.event(
        cx,
        5,
        Event::ConversationUpdated {
            conversation: world.conversation(true),
        },
    );
    assert!(
        world.on_screen(cx, "reaction-chip-0"),
        "the reactions are still shown"
    );
    assert!(
        !world.on_screen(cx, "add-reaction-chip"),
        "but no new one can be added"
    );
    world.take_effects();
    world.click(cx, "reaction-chip-1");
    assert_eq!(world.reaction_effects(), vec![], "and chips do not toggle");
    world.hover_row(cx, 0);
    assert!(!world.on_screen(cx, "react-message"));
}

#[gpui::test]
fn reactions_from_other_people_update_the_chips(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let plain = world.message(&world.bob, "no reactions yet", 1);
    world.reach(cx, vec![plain.clone()]);
    assert!(!world.on_screen(cx, "reaction-chips"));
    world.event(
        cx,
        2,
        Event::ReactionsChanged {
            conversation_id: world.general,
            message_id: plain.id,
            reactions: vec![ReactionSummary {
                emoji: emoji("❤️"),
                users: vec![world.zoe.id, world.bob.id],
            }],
        },
    );
    assert!(world.on_screen(cx, "reaction-chip-0"));
    assert!(world.on_screen(cx, "add-reaction-chip"));
}

// ---- Reactions from the keyboard -----------------------------------------------------------------

#[gpui::test]
fn the_actions_menu_offers_a_toggle_for_each_reaction_and_adds_new_ones(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    chat_with_reactions(&world, cx);
    world.take_effects();

    // The composer has focus; one Shift+Tab reaches the timeline.
    cx.simulate_keystrokes("shift-tab enter");
    assert!(
        world.on_screen(cx, "message-actions"),
        "Enter opens the menu"
    );
    assert!(
        world.on_screen(cx, "message-actions-react"),
        "Add reaction…"
    );
    assert!(
        world.on_screen(cx, "message-actions-react-👍"),
        "a toggle per reaction"
    );
    assert!(world.on_screen(cx, "message-actions-react-🎉"));

    // Down to the first toggle, 👍 (mine): Enter takes it back.
    cx.simulate_keystrokes("down enter");
    assert_eq!(world.reaction_effects(), vec![(emoji("👍"), false)]);

    // Again, down twice to 🎉 (not mine): it is added.
    cx.simulate_keystrokes("enter down down enter");
    assert_eq!(world.reaction_effects(), vec![(emoji("🎉"), true)]);
}

#[gpui::test]
fn add_reaction_from_the_menu_opens_the_picker_and_escape_returns_to_the_timeline(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let first = world.message(&world.bob, "first", 1);
    let second = world.message(&world.bob, "second", 2);
    world.reach(cx, vec![first, second]);
    world.take_effects();

    cx.simulate_keystrokes("shift-tab enter enter");
    assert!(
        world.picker_is_open(cx),
        "the first menu item is Add reaction…"
    );
    cx.simulate_keystrokes("escape");
    assert!(!world.picker_is_open(cx));
    // Focus is back on the timeline: its cursor still answers to the arrow keys.
    cx.simulate_keystrokes("up");
    draw(cx);
    let cursor = cx
        .debug_bounds("message-cursor")
        .expect("the cursor is drawn");
    let first_row = cx.debug_bounds("message-row-0").unwrap();
    assert!(
        first_row.contains(&cursor.center()),
        "Up moved it to the older message"
    );

    // And this time choose something.
    cx.simulate_keystrokes("enter enter");
    cx.simulate_input("fire");
    cx.simulate_keystrokes("enter");
    assert_eq!(world.reaction_effects(), vec![(emoji("🔥"), true)]);
}

// ---- Shortcode suggestions ----------------------------------------------------------------------

#[gpui::test]
fn typing_a_shortcode_offers_it_and_enter_accepts_without_sending(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input("nice :tad");
    assert!(world.on_screen(cx, "composer-suggestions"));
    assert!(world.on_screen(cx, "composer-suggestions-0"));

    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert_eq!(
        world.sent_texts(),
        Vec::<String>::new(),
        "Enter accepted, it did not send"
    );
    assert_eq!(
        world.draft(cx),
        "nice 🎉",
        "the typed part was replaced by the emoji"
    );
    assert!(!world.on_screen(cx, "composer-suggestions"));
    assert_eq!(world.prefs(cx).recent, vec![emoji("🎉")]);

    // With no popup, Enter sends as always.
    cx.simulate_keystrokes("enter");
    assert_eq!(world.sent_texts(), vec!["nice 🎉".to_owned()]);
}

#[gpui::test]
fn tab_accepts_too_and_escape_dismisses_until_another_shortcode(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input(":tad");
    cx.simulate_keystrokes("tab");
    assert_eq!(world.draft(cx), "🎉");
    cx.simulate_keystrokes("ctrl-a backspace");

    cx.simulate_input(":tad");
    assert!(world.on_screen(cx, "composer-suggestions"));
    cx.simulate_keystrokes("escape");
    assert!(
        !world.on_screen(cx, "composer-suggestions"),
        "Escape dismisses it"
    );
    cx.simulate_input("a");
    assert!(
        !world.on_screen(cx, "composer-suggestions"),
        "typing on does not bring it back"
    );
    world.take_effects();
    cx.simulate_keystrokes("enter");
    assert_eq!(
        world.sent_texts(),
        vec![":tada".to_owned()],
        "the text goes as typed"
    );
    cx.simulate_input(":smi");
    assert!(
        world.on_screen(cx, "composer-suggestions"),
        "a new shortcode is offered again"
    );
}

#[gpui::test]
fn up_and_down_choose_a_suggestion(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    cx.simulate_input(":smi");
    assert!(
        world.on_screen(cx, "composer-suggestions-1"),
        "more than one match"
    );
    cx.simulate_keystrokes("down enter");
    let second = emoji::complete_shortcode("smi", 8)[1].emoji.as_str();
    assert_eq!(world.draft(cx), second);

    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input(":smi");
    cx.simulate_keystrokes("up enter");
    let last = emoji::complete_shortcode("smi", 8)
        .last()
        .unwrap()
        .emoji
        .as_str();
    assert_eq!(world.draft(cx), last, "Up from the first wraps to the last");
}

#[gpui::test]
fn text_that_is_not_a_shortcode_never_opens_the_list_or_swallows_a_key(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    for text in ["meet at 10:30", "https://example.com", "`:tad", "a:tad"] {
        cx.simulate_input(text);
        assert!(
            !world.on_screen(cx, "composer-suggestions"),
            "{text:?} opens nothing"
        );
        world.take_effects();
        cx.simulate_keystrokes("enter");
        assert_eq!(
            world.sent_texts(),
            vec![text.to_owned()],
            "Enter sends {text:?}"
        );
    }
    // Arrow keys move the caret as usual when nothing is open.
    cx.simulate_input("one");
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input("two");
    cx.simulate_keystrokes("up");
    assert!(!world.on_screen(cx, "composer-suggestions"));
}

#[gpui::test]
fn no_suggestions_while_an_input_method_is_composing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let input = world.root.read_with(cx, |root, cx| {
        root.shell()
            .unwrap()
            .read(cx)
            .composer()
            .read(cx)
            .input()
            .clone()
    });
    input.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, ":tad", None, window, cx)
    });
    assert!(input.read_with(cx, |input, _| input.is_composing()));
    assert!(
        !world.on_screen(cx, "composer-suggestions"),
        "preedit text is not final, so nothing is offered"
    );
}

#[gpui::test]
fn the_editor_offers_suggestions_too(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![world.message(&world.me, "original", 1)]);
    world.take_effects();
    world.hover_row(cx, 0);
    world.click(cx, "edit-message");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("well done :tad");
    assert!(world.on_screen(cx, "editor-suggestions"));

    cx.simulate_keystrokes("enter");
    assert_eq!(
        world.sent_texts(),
        Vec::<String>::new(),
        "Enter accepted; it did not save"
    );
    assert!(
        world.on_screen(cx, "message-editor"),
        "the editor is still open"
    );
    cx.simulate_keystrokes("ctrl-a ctrl-c");
    assert_eq!(clipboard(cx).as_deref(), Some("well done 🎉"));

    // Escape closes an open list before it closes the editor.
    cx.simulate_input(" :smi");
    assert!(world.on_screen(cx, "editor-suggestions"));
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "editor-suggestions"));
    assert!(world.on_screen(cx, "message-editor"));
    cx.simulate_keystrokes("escape");
    assert!(
        !world.on_screen(cx, "message-editor"),
        "a second Escape cancels the edit"
    );
}
