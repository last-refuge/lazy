//! Selection restoration (slice 2E, D16): the app reopens on the conversation last open, and
//! each draft keeps the caret and selection it had.

mod common;

use common::{Driver, World};
use lazy_client_core::{
    DraftSelection, Effect, Input, Output, StartupState, TextSelection, ThreadDraft, ViewState,
};
use lazy_domain::{ConversationId, MessageId, WorkspaceId};

// ----- Helpers -----

fn caret(anchor: usize, head: usize) -> TextSelection {
    TextSelection { anchor, head }
}

fn views(out: &Output) -> Vec<ViewState> {
    out.effects
        .iter()
        .filter_map(|e| match e {
            Effect::PersistView(view) => Some(view.clone()),
            _ => None,
        })
        .collect()
}

fn thread_drafts(out: &Output) -> Vec<ThreadDraft> {
    out.effects
        .iter()
        .filter_map(|e| match e {
            Effect::PersistThreadDrafts(drafts) => Some(drafts.clone()),
            _ => None,
        })
        .next_back()
        .unwrap_or_default()
}

fn set_draft(driver: &mut Driver, conversation: ConversationId, text: &str) -> Output {
    driver.send(Input::SetDraft {
        conversation,
        text: text.into(),
    })
}

fn set_caret(
    driver: &mut Driver,
    conversation: ConversationId,
    anchor: usize,
    head: usize,
) -> Output {
    driver.send(Input::SetDraftSelection {
        conversation,
        anchor,
        head,
    })
}

/// A new run of the app that finds `view` and `drafts` saved, with its session restored.
fn restart(world: &World, view: ViewState, drafts: Vec<(ConversationId, &str)>) -> Driver {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        drafts: drafts
            .into_iter()
            .map(|(c, text)| (c, text.to_owned()))
            .collect(),
        view,
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    driver
}

fn last_view(driver: &Driver) -> ViewState {
    driver
        .effects
        .iter()
        .rev()
        .find_map(|e| match e {
            Effect::PersistView(view) => Some(view.clone()),
            _ => None,
        })
        .expect("the view was saved")
}

// ----- The conversation last open -----

#[test]
fn the_conversation_last_open_is_saved_and_reopened() {
    let world = World::new();
    let random = world.second_channel.id;
    let mut driver = Driver::ready(&world);
    // The default channel opened first, and that is remembered too.
    assert_eq!(
        last_view(&driver).last_selected,
        vec![(world.workspace.id, world.channel.id)]
    );
    let out = driver.send(Input::SelectConversation(random));
    assert_eq!(
        views(&out).last().map(|v| v.last_selected.clone()),
        Some(vec![(world.workspace.id, random)])
    );
    // A restart that finds that save reopens on it, not on the default channel.
    let restarted = restart(&world, last_view(&driver), vec![]);
    assert_eq!(restarted.core.selected_conversation(), Some(random));
}

#[test]
fn nothing_is_saved_again_when_the_selection_does_not_change() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let out = driver.send(Input::SelectConversation(world.channel.id));
    assert!(views(&out).is_empty());
}

#[test]
fn a_conversation_the_person_is_no_longer_in_is_not_reopened() {
    let world = World::new();
    let view = ViewState {
        last_selected: vec![(world.workspace.id, ConversationId::generate())],
        draft_selections: vec![],
    };
    let restarted = restart(&world, view, vec![]);
    assert_eq!(
        restarted.core.selected_conversation(),
        Some(world.channel.id),
        "the default opens as before"
    );
}

#[test]
fn only_the_workspace_that_is_open_counts() {
    let world = World::new();
    let view = ViewState {
        last_selected: vec![
            (WorkspaceId::generate(), world.second_channel.id),
            (world.workspace.id, world.channel.id),
        ],
        draft_selections: vec![],
    };
    let restarted = restart(&world, view, vec![]);
    assert_eq!(
        restarted.core.selected_conversation(),
        Some(world.channel.id)
    );
}

#[test]
fn leaving_the_remembered_conversation_forgets_it() {
    let world = World::new();
    let random = world.second_channel.id;
    let mut driver = Driver::ready(&world);
    driver.send(Input::SelectConversation(random));
    driver.send(world.event_at(
        12,
        lazy_protocol::Event::MemberRemoved {
            conversation_id: random,
            user_id: world.me.id,
        },
    ));
    // The default opened instead, and that is what is remembered now.
    assert_eq!(
        last_view(&driver).last_selected,
        vec![(world.workspace.id, world.channel.id)]
    );
}

#[test]
fn signing_out_forgets_what_was_open() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(Input::SelectConversation(world.second_channel.id));
    driver.send(Input::SignOut);
    assert!(
        driver
            .core
            .draft_selection(world.second_channel.id)
            .is_none()
    );
    let restarted = restart(&world, ViewState::default(), vec![]);
    assert_eq!(
        restarted.core.selected_conversation(),
        Some(world.channel.id)
    );
}

// ----- The caret of a draft -----

#[test]
fn a_draft_keeps_its_caret_across_a_restart() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    set_draft(&mut driver, general, "hello wonderful world");
    let out = set_caret(&mut driver, general, 6, 15);
    assert_eq!(driver.core.draft_selection(general), Some(caret(6, 15)));
    let saved = views(&out);
    assert_eq!(
        saved.last().map(|v| v.draft_selections.clone()),
        Some(vec![DraftSelection {
            conversation: general,
            selection: caret(6, 15),
        }])
    );
    let restarted = restart(
        &world,
        last_view(&driver),
        vec![(general, "hello wonderful world")],
    );
    assert_eq!(restarted.core.draft_selection(general), Some(caret(6, 15)));
}

#[test]
fn reporting_the_same_caret_again_saves_nothing() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    set_draft(&mut driver, general, "hello");
    set_caret(&mut driver, general, 2, 2);
    let out = set_caret(&mut driver, general, 2, 2);
    assert!(views(&out).is_empty());
}

#[test]
fn a_caret_for_a_conversation_with_no_draft_is_ignored() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let out = set_caret(&mut driver, general, 3, 3);
    assert!(views(&out).is_empty());
    assert!(driver.core.draft_selection(general).is_none());
}

#[test]
fn clearing_a_draft_drops_its_caret() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    set_draft(&mut driver, general, "hello");
    set_caret(&mut driver, general, 5, 5);
    let out = set_draft(&mut driver, general, "");
    assert!(driver.core.draft_selection(general).is_none());
    assert_eq!(
        views(&out).last().map(|v| v.draft_selections.len()),
        Some(0)
    );
    // A new draft starts with no caret to restore.
    set_draft(&mut driver, general, "again");
    assert!(driver.core.draft_selection(general).is_none());
}

#[test]
fn the_order_text_and_caret_arrive_in_does_not_matter() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    set_draft(&mut driver, general, "hel");
    // The composer reports the caret of the longer text before the text itself.
    set_caret(&mut driver, general, 5, 5);
    set_draft(&mut driver, general, "hello");
    assert_eq!(driver.core.draft_selection(general), Some(caret(5, 5)));
}

// ----- Clamping -----

#[test]
fn a_caret_beyond_the_text_is_clamped_to_the_end() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    set_draft(&mut driver, general, "hello");
    set_caret(&mut driver, general, 2, 50);
    assert_eq!(driver.core.draft_selection(general), Some(caret(2, 5)));
    // Text that got shorter pulls it in as well.
    set_draft(&mut driver, general, "hi");
    assert_eq!(driver.core.draft_selection(general), Some(caret(2, 2)));
}

#[test]
fn a_caret_inside_a_character_moves_back_to_where_it_starts() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    // "é" is two bytes, "🎉" four.
    set_draft(&mut driver, general, "aé🎉b");
    set_caret(&mut driver, general, 2, 5);
    assert_eq!(driver.core.draft_selection(general), Some(caret(1, 3)));
    let text = "aé🎉b";
    let selection = driver.core.draft_selection(general).unwrap();
    assert!(text.is_char_boundary(selection.anchor) && text.is_char_boundary(selection.head));
}

#[test]
fn a_caret_inside_a_cluster_moves_to_the_start_of_the_cluster() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    // "e" with a combining accent is one letter of two characters, and a family emoji is one
    // symbol of seven.
    let text = "e\u{301}x👨\u{200d}👩\u{200d}👧y";
    set_draft(&mut driver, general, text);
    let accent = 1; // between the "e" and its accent
    let inside_family = "e\u{301}x".len() + "👨".len(); // after the first person
    set_caret(&mut driver, general, accent, inside_family);
    assert_eq!(
        driver.core.draft_selection(general),
        Some(caret(0, "e\u{301}x".len()))
    );
}

#[test]
fn what_is_saved_is_clamped_to_the_draft_too() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    set_draft(&mut driver, general, "aé");
    let out = set_caret(&mut driver, general, 2, 99);
    assert_eq!(
        views(&out).last().unwrap().draft_selections[0].selection,
        caret(1, 3)
    );
}

#[test]
fn a_saved_caret_that_no_longer_fits_the_draft_that_was_saved_is_clamped_on_load() {
    let world = World::new();
    let general = world.channel.id;
    let view = ViewState {
        last_selected: vec![],
        draft_selections: vec![DraftSelection {
            conversation: general,
            selection: caret(2, 400),
        }],
    };
    let restarted = restart(&world, view.clone(), vec![(general, "abc")]);
    assert_eq!(restarted.core.draft_selection(general), Some(caret(2, 3)));
    // And a caret saved for a draft that is gone gives nothing.
    let restarted = restart(&world, view, vec![]);
    assert!(restarted.core.draft_selection(general).is_none());
}

// ----- Reply drafts -----

#[test]
fn a_reply_draft_keeps_its_caret_in_the_file_beside_its_text() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let root = world.message(general, &world.other, 5, "root");
    driver.load_history(general, vec![root.clone()], false);
    driver.send(Input::OpenThread {
        conversation: general,
        root: root.id,
    });
    driver.send(Input::SetThreadDraft {
        root: root.id,
        text: "my reply".into(),
    });
    let out = driver.send(Input::SetThreadDraftSelection {
        root: root.id,
        anchor: 3,
        head: 99,
    });
    assert_eq!(
        driver.core.thread_draft_selection(root.id),
        Some(caret(3, 8))
    );
    assert_eq!(
        thread_drafts(&out),
        vec![ThreadDraft {
            root: root.id,
            conversation: general,
            text: "my reply".into(),
            selection: Some(caret(3, 8)),
        }]
    );
    // A restart that finds it restores it.
    let mut restarted = Driver::new();
    restarted.send(Input::Started(StartupState {
        session_restored: true,
        thread_drafts: thread_drafts(&out),
        ..Default::default()
    }));
    assert_eq!(
        restarted.core.thread_draft_selection(root.id),
        Some(caret(3, 8))
    );
    assert_eq!(restarted.core.thread_draft(root.id), "my reply");
}

#[test]
fn a_reply_draft_written_before_carets_were_kept_loads_without_one() {
    // A thread draft exactly as the previous version wrote it.
    let previous = serde_json::json!({
        "root": MessageId::generate(),
        "conversation": ConversationId::generate(),
        "text": "old reply",
    });
    let draft: ThreadDraft = serde_json::from_value(previous).expect("the previous shape reads");
    assert_eq!(draft.selection, None);
    // And one without a caret is written as it always was.
    let written = serde_json::to_value(&draft).unwrap();
    assert!(written.get("selection").is_none());
}

#[test]
fn clearing_a_reply_draft_drops_its_caret() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let root = world.message(general, &world.other, 5, "root");
    driver.load_history(general, vec![root.clone()], false);
    driver.send(Input::OpenThread {
        conversation: general,
        root: root.id,
    });
    driver.send(Input::SetThreadDraft {
        root: root.id,
        text: "abc".into(),
    });
    driver.send(Input::SetThreadDraftSelection {
        root: root.id,
        anchor: 1,
        head: 2,
    });
    driver.send(Input::SetThreadDraft {
        root: root.id,
        text: String::new(),
    });
    assert!(driver.core.thread_draft_selection(root.id).is_none());
}
