//! Editing, deleting, and permalinks (slice 2B).

mod common;

use common::{Driver, World, channel, read_state, seq};
use lazy_client_core::{
    ActionKind, ActionState, Change, EditState, Effect, Input, ItemKey, JournalEntry, JournalKind,
    JumpTarget, Output, PERMALINK_MAX_PAGES, RequestError, RetryKind, StartupState, TimelineItem,
};
use lazy_domain::{
    ConversationId, MessageDocument, MessageId, OperationId, Permalink, Timestamp, WorkspaceRole,
};
use lazy_protocol::{ApiError, ErrorCode, Event, Message, MessageResponse, OperationStatus};

// ----- Helpers -----

/// A world with one message of mine (position 5, revision 1) loaded in `#general`.
fn setup() -> (World, Driver, Message) {
    setup_as(WorkspaceRole::Member)
}

fn setup_as(role: WorkspaceRole) -> (World, Driver, Message) {
    let mut world = World::new();
    world.role = role;
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let mine = world.message(general, &world.me, 5, "my words");
    driver.load_history(general, vec![mine.clone()], false);
    (world, driver, mine)
}

fn edit(conversation: ConversationId, message: MessageId, text: &str) -> Input {
    Input::EditMessage {
        conversation,
        message,
        text: text.into(),
    }
}

fn parsed(text: &str) -> MessageDocument {
    MessageDocument::from_markdown(text).unwrap()
}

fn row(driver: &Driver, conversation: ConversationId, message: MessageId) -> &TimelineItem {
    driver
        .core
        .timeline(conversation)
        .iter()
        .find(|item| item.message_id() == Some(message))
        .expect("the row should be in the timeline")
}

fn has_row(driver: &Driver, conversation: ConversationId, message: MessageId) -> bool {
    driver
        .core
        .timeline(conversation)
        .iter()
        .any(|item| item.message_id() == Some(message))
}

fn journaled(out: &Output) -> Vec<JournalEntry> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.clone()),
            _ => None,
        })
        .collect()
}

fn edit_requests(out: &Output) -> Vec<(MessageId, lazy_protocol::EditMessageRequest)> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::EditMessage { message, request } => Some((*message, request.clone())),
            _ => None,
        })
        .collect()
}

fn settled(out: &Output) -> Vec<OperationId> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::SettleSubmission { operation } => Some(*operation),
            _ => None,
        })
        .collect()
}

fn queried(out: &Output) -> Vec<OperationId> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::QueryOperationStatus { operation } => Some(*operation),
            _ => None,
        })
        .collect()
}

fn fetches(out: &Output) -> Vec<MessageId> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::FetchMessage { message } => Some(*message),
            _ => None,
        })
        .collect()
}

fn respond(message: &Message) -> Result<MessageResponse, RequestError> {
    Ok(MessageResponse {
        message: message.clone(),
    })
}

fn rejected(code: ErrorCode) -> RequestError {
    RequestError::Rejected(ApiError {
        code,
        message: format!("{code:?}"),
    })
}

fn lost() -> RequestError {
    RequestError::Unknown("timed out".into())
}

/// Starts an edit and makes its journal write durable. Returns the operation and the outputs of
/// the two steps.
fn start_edit(
    driver: &mut Driver,
    conversation: ConversationId,
    message: MessageId,
    text: &str,
) -> OperationId {
    let out = driver.send(edit(conversation, message, text));
    let entries = journaled(&out);
    assert_eq!(entries.len(), 1, "one journal entry for the edit");
    let operation = entries[0].operation_id;
    let out = driver.send(Input::SubmissionPersisted { operation });
    assert_eq!(
        edit_requests(&out).len(),
        1,
        "the edit is sent once durable"
    );
    operation
}

fn edit_result(
    driver: &mut Driver,
    operation: OperationId,
    result: Result<MessageResponse, RequestError>,
) -> Output {
    driver.send(Input::EditResult { operation, result })
}

fn unread(driver: &Driver, conversation: ConversationId) -> u32 {
    driver.core.user_state(conversation).unwrap().unread_count
}

// ----- Sending text -----

#[test]
fn composer_text_is_parsed_as_rich_text() {
    let (world, mut driver, _) = setup();
    let general = world.channel.id;
    let out = driver.send(Input::SendMessage {
        conversation: general,
        text: "hello *world*".into(),
    });
    let entry = &journaled(&out)[0];
    assert_eq!(entry.document, parsed("hello *world*"));
    assert_ne!(
        entry.document,
        MessageDocument::from_plain_text("hello *world*").unwrap()
    );
    let pending = driver.core.timeline(general).last().unwrap();
    assert_eq!(
        pending.document, entry.document,
        "the pending row shows the parsed document"
    );
}

#[test]
fn text_over_the_limits_is_a_notice_and_the_draft_stays() {
    let (world, mut driver, _) = setup();
    let general = world.channel.id;
    driver.send(Input::SetDraft {
        conversation: general,
        text: "x".repeat(40_001),
    });
    let out = driver.send(Input::SendMessage {
        conversation: general,
        text: "x".repeat(40_001),
    });
    assert!(journaled(&out).is_empty());
    assert!(driver.core.notice().is_some());
    assert_eq!(
        driver.core.draft(general).len(),
        40_001,
        "the draft is kept"
    );
}

// ----- Editing: the happy path and what to offer -----

#[test]
fn an_edit_is_journaled_before_it_is_sent_and_shows_at_once() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    assert!(driver.core.can_edit_message(general, mine.id));

    let out = driver.send(edit(general, mine.id, "now *bold*"));
    let entries = journaled(&out);
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(
        entry.kind,
        JournalKind::Edit {
            message_id: mine.id,
            base_revision: 1
        }
    );
    assert_eq!(entry.document, parsed("now *bold*"));
    assert_eq!(entry.conversation_id, general);
    assert!(
        edit_requests(&out).is_empty(),
        "nothing is sent before the journal is durable"
    );
    let saving = row(&driver, general, mine.id);
    assert_eq!(saving.edit, EditState::Saving);
    assert_eq!(
        saving.document, entry.document,
        "the row shows the new text"
    );
    assert_eq!(
        driver.core.edit_text(general, mine.id).unwrap(),
        "now *bold*"
    );
    assert_eq!(driver.core.unsettled_operations(), 1);

    let out = driver.send(Input::SubmissionPersisted {
        operation: entry.operation_id,
    });
    let requests = edit_requests(&out);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, mine.id);
    assert_eq!(requests[0].1.operation_id, entry.operation_id);
    assert_eq!(requests[0].1.base_revision, 1);
    assert_eq!(requests[0].1.document, entry.document);

    let committed = world.edited(&mine, "now *bold*");
    let out = edit_result(&mut driver, entry.operation_id, respond(&committed));
    assert_eq!(settled(&out), vec![entry.operation_id]);
    let done = row(&driver, general, mine.id);
    assert_eq!(done.edit, EditState::None);
    assert_eq!(done.document, parsed("now *bold*"));
    assert_eq!(done.revision(), Some(2));
    assert!(done.is_edited());
    assert_eq!(driver.core.unsettled_operations(), 0);
    assert_eq!(
        driver.core.edit_text(general, mine.id).unwrap(),
        "now *bold*",
        "the editor starts from the saved text"
    );
}

#[test]
fn only_the_author_edits_and_admins_delete_anyones() {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let mine = world.message(general, &world.me, 4, "mine");
    let theirs = world.message(general, &world.other, 5, "theirs");
    driver.load_history(general, vec![mine.clone(), theirs.clone()], false);

    assert!(driver.core.can_edit_message(general, mine.id));
    assert!(
        !driver.core.can_edit_message(general, theirs.id),
        "no one edits another's words"
    );
    assert!(driver.core.can_delete_message(general, mine.id));
    assert!(
        driver.core.can_delete_message(general, theirs.id),
        "an admin moderates"
    );

    let out = driver.send(edit(general, theirs.id, "hijacked"));
    assert!(journaled(&out).is_empty());
    assert!(driver.core.notice().is_some());
    assert_eq!(row(&driver, general, theirs.id).edit, EditState::None);
}

#[test]
fn an_ordinary_member_deletes_only_their_own_messages() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let mine = world.message(general, &world.me, 4, "mine");
    let theirs = world.message(general, &world.other, 5, "theirs");
    driver.load_history(general, vec![mine.clone(), theirs.clone()], false);
    assert!(driver.core.can_delete_message(general, mine.id));
    assert!(!driver.core.can_delete_message(general, theirs.id));
    let out = driver.send(Input::DeleteMessage {
        conversation: general,
        message: theirs.id,
    });
    assert!(
        out.effects
            .iter()
            .all(|e| !matches!(e, Effect::DeleteMessage { .. }))
    );
    assert!(driver.core.notice().is_some());
}

#[test]
fn an_archived_conversation_is_read_only_for_authors_too() {
    let mut world = World::new();
    world.channel.archived = true;
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let mine = world.message(general, &world.me, 5, "mine");
    driver.load_history(general, vec![mine.clone()], false);
    assert!(!driver.core.can_edit_message(general, mine.id));
    assert!(!driver.core.can_delete_message(general, mine.id));
    let out = driver.send(edit(general, mine.id, "late"));
    assert!(journaled(&out).is_empty());
    assert!(driver.core.notice().is_some());
}

#[test]
fn empty_unchanged_and_oversized_edits_do_not_start_an_operation() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    for text in ["", "   \n "] {
        let out = driver.send(edit(general, mine.id, text));
        assert!(journaled(&out).is_empty(), "{text:?}");
        assert!(driver.core.notice().is_some());
        driver.send(Input::DismissNotice);
    }
    let out = driver.send(edit(general, mine.id, "my words"));
    assert!(out.is_empty(), "text that changes nothing is ignored");
    let out = driver.send(edit(general, mine.id, &"x".repeat(40_001)));
    assert!(journaled(&out).is_empty());
    assert!(driver.core.notice().is_some());
    assert_eq!(row(&driver, general, mine.id).edit, EditState::None);
}

// ----- Editing: one in flight, the rest coalesced -----

#[test]
fn later_edits_wait_and_go_as_one_on_top_of_the_acknowledged_revision() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let first = start_edit(&mut driver, general, mine.id, "one");

    // More text while the first is in flight: nothing new is journaled or sent.
    let out = driver.send(edit(general, mine.id, "two"));
    assert!(journaled(&out).is_empty() && edit_requests(&out).is_empty());
    let out = driver.send(edit(general, mine.id, "three"));
    assert!(journaled(&out).is_empty() && edit_requests(&out).is_empty());
    assert_eq!(
        row(&driver, general, mine.id).document,
        parsed("three"),
        "the latest text shows"
    );
    assert_eq!(driver.core.edit_text(general, mine.id).unwrap(), "three");
    let out = driver.send(edit(general, mine.id, "three"));
    assert!(out.is_empty(), "the same text again changes nothing");

    // The first is acknowledged: one edit with the latest text, on top of revision 2.
    let acked = world.edited(&mine, "one");
    let out = edit_result(&mut driver, first, respond(&acked));
    assert_eq!(settled(&out), vec![first]);
    let next = journaled(&out);
    assert_eq!(next.len(), 1);
    assert_eq!(
        next[0].kind,
        JournalKind::Edit {
            message_id: mine.id,
            base_revision: 2
        }
    );
    assert_eq!(next[0].document, parsed("three"));
    assert_eq!(row(&driver, general, mine.id).edit, EditState::Saving);
    assert_eq!(row(&driver, general, mine.id).document, parsed("three"));
    let out = driver.send(Input::SubmissionPersisted {
        operation: next[0].operation_id,
    });
    assert_eq!(edit_requests(&out)[0].1.base_revision, 2);

    let acked = world.edited(&acked, "three");
    edit_result(&mut driver, next[0].operation_id, respond(&acked));
    let done = row(&driver, general, mine.id);
    assert_eq!(
        (done.edit.clone(), done.revision()),
        (EditState::None, Some(3))
    );
    assert_eq!(driver.core.unsettled_operations(), 0);
}

#[test]
fn a_waiting_edit_can_be_cancelled_and_a_waiting_revert_is_sent() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let first = start_edit(&mut driver, general, mine.id, "one");
    driver.send(edit(general, mine.id, "two"));

    // Cancelling drops only the waiting text; the request in flight cannot be taken back.
    driver.send(Input::CancelEdit {
        conversation: general,
        message: mine.id,
    });
    assert_eq!(row(&driver, general, mine.id).document, parsed("one"));
    assert_eq!(row(&driver, general, mine.id).edit, EditState::Saving);
    let out = driver.send(Input::CancelEdit {
        conversation: general,
        message: mine.id,
    });
    assert!(
        out.is_empty(),
        "nothing waits, so there is nothing to cancel"
    );

    // Writing the original text back while "one" is in flight must still be sent afterwards.
    driver.send(edit(general, mine.id, "my words"));
    let out = edit_result(&mut driver, first, respond(&world.edited(&mine, "one")));
    let next = journaled(&out);
    assert_eq!(next.len(), 1, "the revert goes on top of the saved edit");
    assert_eq!(next[0].document, parsed("my words"));
}

#[test]
fn an_edit_that_leaves_the_message_as_it_is_needs_no_request() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let first = start_edit(&mut driver, general, mine.id, "one");
    driver.send(edit(general, mine.id, "two"));
    driver.send(edit(general, mine.id, "one"));
    // The waiting text now equals what the server is about to have; the in-flight text is
    // intended, so the waiting edit was ignored or collapsed.
    let out = edit_result(&mut driver, first, respond(&world.edited(&mine, "one")));
    assert!(journaled(&out).is_empty());
    assert_eq!(row(&driver, general, mine.id).edit, EditState::None);
}

#[test]
fn the_edit_event_with_our_operation_settles_it_and_the_late_response_is_ignored() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "one");

    let out = driver.send(world.edited_event(11, &world.edited(&mine, "one"), Some(operation)));
    assert_eq!(settled(&out), vec![operation]);
    assert_eq!(row(&driver, general, mine.id).edit, EditState::None);
    assert_eq!(row(&driver, general, mine.id).revision(), Some(2));
    assert_eq!(driver.core.unsettled_operations(), 0);

    let out = edit_result(&mut driver, operation, respond(&world.edited(&mine, "one")));
    assert!(
        out.is_empty(),
        "the response arrives after the event and changes nothing"
    );
}

// ----- Editing: unknown outcomes -----

#[test]
fn a_lost_response_is_reconciled_by_asking_never_by_resending_blindly() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "one");

    let out = edit_result(&mut driver, operation, Err(lost()));
    assert_eq!(queried(&out), vec![operation]);
    assert!(edit_requests(&out).is_empty());
    assert_eq!(
        row(&driver, general, mine.id).edit,
        EditState::Saving,
        "still believed"
    );

    let committed = world.edited(&mine, "one");
    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::EditCommitted {
            message: committed.clone(),
        }),
    });
    assert_eq!(settled(&out), vec![operation]);
    assert_eq!(row(&driver, general, mine.id).edit, EditState::None);
    assert_eq!(row(&driver, general, mine.id).revision(), Some(2));
    assert_eq!(driver.core.unsettled_operations(), 0);
}

#[test]
fn no_record_means_the_same_request_is_sent_again_with_the_same_id() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "one");
    edit_result(&mut driver, operation, Err(lost()));

    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    let again = edit_requests(&out);
    assert_eq!(again.len(), 1);
    assert_eq!(
        again[0].1.operation_id, operation,
        "never a fresh operation"
    );
    assert_eq!(again[0].1.base_revision, 1);
    assert_eq!(again[0].1.document, parsed("one"));
    assert!(
        journaled(&out).is_empty(),
        "the journal entry is still there"
    );

    let out = edit_result(&mut driver, operation, respond(&world.edited(&mine, "one")));
    assert_eq!(settled(&out), vec![operation]);
}

#[test]
fn giving_up_keeps_the_journal_and_a_retry_asks_again() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "one");
    edit_result(&mut driver, operation, Err(lost()));
    // Three "no record" answers are resent; the next gives up.
    for _ in 0..3 {
        driver.send(Input::OperationStatusResult {
            operation,
            result: Ok(OperationStatus::NotFound),
        });
        edit_result(&mut driver, operation, Err(lost()));
    }
    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    assert!(
        settled(&out).is_empty(),
        "an unknown outcome keeps its journal entry"
    );
    let failed = row(&driver, general, mine.id);
    assert!(matches!(
        &failed.edit,
        EditState::Failed { retry: RetryKind::SameOperation, text, .. } if text == "one"
    ));
    assert_eq!(
        failed.document,
        parsed("my words"),
        "the row shows what the server has"
    );
    assert_eq!(driver.core.unsettled_operations(), 1);

    let out = driver.send(Input::RetryEdit {
        conversation: general,
        message: mine.id,
    });
    assert_eq!(queried(&out), vec![operation], "retrying asks first");
    assert_eq!(row(&driver, general, mine.id).edit, EditState::Saving);
}

#[test]
fn new_text_after_an_unknown_outcome_waits_for_the_old_edit_to_be_settled() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "one");
    edit_result(&mut driver, operation, Err(lost()));
    driver.send(Input::OperationStatusResult {
        operation,
        result: Err(lost()),
    });
    assert!(matches!(
        row(&driver, general, mine.id).edit,
        EditState::Failed {
            retry: RetryKind::SameOperation,
            ..
        }
    ));

    let out = driver.send(edit(general, mine.id, "two"));
    assert!(
        journaled(&out).is_empty(),
        "no new operation while the old one may have committed"
    );
    assert_eq!(queried(&out), vec![operation]);
    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::EditCommitted {
            message: world.edited(&mine, "one"),
        }),
    });
    let next = journaled(&out);
    assert_eq!(next.len(), 1);
    assert_eq!(
        next[0].kind,
        JournalKind::Edit {
            message_id: mine.id,
            base_revision: 2
        }
    );
    assert_eq!(next[0].document, parsed("two"));
}

#[test]
fn new_text_after_no_record_replaces_the_old_edit() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "one");
    edit_result(&mut driver, operation, Err(lost()));
    driver.send(edit(general, mine.id, "two"));
    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    assert_eq!(settled(&out), vec![operation]);
    let next = journaled(&out);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].document, parsed("two"));
    assert_eq!(
        next[0].kind,
        JournalKind::Edit {
            message_id: mine.id,
            base_revision: 1
        },
        "still on top of the revision the person saw"
    );
}

// ----- Editing: rejections and conflicts -----

#[test]
fn a_stale_edit_shows_the_newer_version_and_keeps_the_text() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "my version");

    let out = edit_result(
        &mut driver,
        operation,
        Err(rejected(ErrorCode::StaleRevision)),
    );
    assert_eq!(fetches(&out), vec![mine.id]);
    assert_eq!(
        settled(&out),
        vec![operation],
        "a definitive refusal releases the journal"
    );
    assert_eq!(row(&driver, general, mine.id).edit, EditState::Saving);

    let newer = world.edited(&world.edited(&mine, "elsewhere"), "elsewhere again");
    driver.send(Input::MessageFetched {
        message: mine.id,
        result: respond(&newer),
    });
    let conflicted = row(&driver, general, mine.id);
    assert_eq!(
        conflicted.edit,
        EditState::Conflict {
            text: "my version".into()
        }
    );
    assert_eq!(
        conflicted.document,
        parsed("elsewhere again"),
        "the newer version shows"
    );
    assert_eq!(conflicted.revision(), Some(3));
    assert!(driver.core.notice().is_some());
    assert_eq!(
        driver.core.edit_text(general, mine.id).unwrap(),
        "my version"
    );
    assert_eq!(driver.core.unsettled_operations(), 0);

    // Saving over it is a new operation on top of the newer revision.
    let out = driver.send(Input::RetryEdit {
        conversation: general,
        message: mine.id,
    });
    let entry = &journaled(&out)[0];
    assert_ne!(entry.operation_id, operation);
    assert_eq!(
        entry.kind,
        JournalKind::Edit {
            message_id: mine.id,
            base_revision: 3
        }
    );
    assert_eq!(entry.document, parsed("my version"));
}

#[test]
fn a_conflict_can_be_abandoned() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "mine");
    edit_result(
        &mut driver,
        operation,
        Err(rejected(ErrorCode::StaleRevision)),
    );
    driver.send(Input::MessageFetched {
        message: mine.id,
        result: respond(&world.edited(&mine, "theirs")),
    });
    driver.send(Input::CancelEdit {
        conversation: general,
        message: mine.id,
    });
    let after = row(&driver, general, mine.id);
    assert_eq!(after.edit, EditState::None);
    assert_eq!(after.document, parsed("theirs"));
    assert_eq!(driver.core.edit_text(general, mine.id).unwrap(), "theirs");
}

#[test]
fn text_written_during_a_conflict_replaces_the_kept_text_and_is_sent_fresh() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "mine");
    edit_result(
        &mut driver,
        operation,
        Err(rejected(ErrorCode::StaleRevision)),
    );
    driver.send(Input::MessageFetched {
        message: mine.id,
        result: respond(&world.edited(&mine, "theirs")),
    });
    let out = driver.send(edit(general, mine.id, "merged"));
    let entry = &journaled(&out)[0];
    assert_eq!(entry.document, parsed("merged"));
    assert_eq!(
        entry.kind,
        JournalKind::Edit {
            message_id: mine.id,
            base_revision: 2
        }
    );
}

#[test]
fn text_written_while_the_stale_answer_is_fetched_becomes_the_kept_text() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "first try");
    edit_result(
        &mut driver,
        operation,
        Err(rejected(ErrorCode::StaleRevision)),
    );
    driver.send(edit(general, mine.id, "second try"));
    driver.send(Input::MessageFetched {
        message: mine.id,
        result: respond(&world.edited(&mine, "theirs")),
    });
    assert_eq!(
        row(&driver, general, mine.id).edit,
        EditState::Conflict {
            text: "second try".into()
        }
    );
}

#[test]
fn a_failed_fetch_after_a_stale_edit_leaves_a_retryable_failure() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "mine");
    edit_result(
        &mut driver,
        operation,
        Err(rejected(ErrorCode::StaleRevision)),
    );
    driver.send(Input::MessageFetched {
        message: mine.id,
        result: Err(lost()),
    });
    assert!(matches!(
        &row(&driver, general, mine.id).edit,
        EditState::Failed { retry: RetryKind::NewOperation, text, .. } if text == "mine"
    ));
    assert_eq!(row(&driver, general, mine.id).document, parsed("my words"));
}

#[test]
fn forbidden_and_missing_messages_revert_with_a_notice() {
    for code in [ErrorCode::Forbidden, ErrorCode::NotFound] {
        let (world, mut driver, mine) = setup();
        let general = world.channel.id;
        let operation = start_edit(&mut driver, general, mine.id, "mine");
        let out = edit_result(&mut driver, operation, Err(rejected(code)));
        assert_eq!(settled(&out), vec![operation], "{code:?}");
        let after = row(&driver, general, mine.id);
        assert_eq!(after.edit, EditState::None, "{code:?}");
        assert_eq!(after.document, parsed("my words"), "{code:?}");
        assert!(driver.core.notice().is_some(), "{code:?}");
        assert_eq!(driver.core.unsettled_operations(), 0);
    }
}

#[test]
fn another_rejection_keeps_the_text_for_a_retry_as_a_new_operation() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "mine");
    let out = edit_result(
        &mut driver,
        operation,
        Err(rejected(ErrorCode::RateLimited)),
    );
    assert_eq!(settled(&out), vec![operation]);
    assert!(matches!(
        &row(&driver, general, mine.id).edit,
        EditState::Failed { retry: RetryKind::NewOperation, text, .. } if text == "mine"
    ));
    let out = driver.send(Input::RetryEdit {
        conversation: general,
        message: mine.id,
    });
    let entry = &journaled(&out)[0];
    assert_ne!(entry.operation_id, operation);
    assert_eq!(entry.document, parsed("mine"));
}

#[test]
fn a_journal_write_failure_fails_the_edit_without_sending() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let out = driver.send(edit(general, mine.id, "mine"));
    let operation = journaled(&out)[0].operation_id;
    let out = driver.send(Input::SubmissionPersistFailed {
        operation,
        message: "disk full".into(),
    });
    assert!(edit_requests(&out).is_empty());
    assert!(matches!(
        row(&driver, general, mine.id).edit,
        EditState::Failed {
            retry: RetryKind::NewOperation,
            ..
        }
    ));
    assert_eq!(driver.core.unsettled_operations(), 0);
}

#[test]
fn an_expired_session_fails_running_edits_for_a_retry_after_signing_in() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "mine");
    driver.send(Input::SessionExpired);
    assert!(matches!(
        row(&driver, general, mine.id).edit,
        EditState::Failed {
            retry: RetryKind::SameOperation,
            ..
        }
    ));
    assert_eq!(
        driver.core.unsettled_operations(),
        1,
        "the journal entry stays"
    );
    let _ = (world, operation);
}

// ----- Editing: restart -----

fn recovered_edit(world: &World, mine: &Message, text: &str) -> JournalEntry {
    JournalEntry {
        operation_id: OperationId::generate(),
        user_id: world.me.id,
        conversation_id: mine.conversation_id,
        document: parsed(text),
        submitted_at: Timestamp::now(),
        kind: JournalKind::Edit {
            message_id: mine.id,
            base_revision: mine.revision,
        },
        thread_root: None,
        also_in_channel: false,
        attachments: Vec::new(),
    }
}

fn restarted_with(world: &World, entries: Vec<JournalEntry>) -> Driver {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        journal: entries,
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    driver
}

#[test]
fn a_journaled_edit_is_reconciled_after_a_restart() {
    let (world, _, mine) = setup();
    let general = world.channel.id;
    let entry = recovered_edit(&world, &mine, "typed before the crash");
    let operation = entry.operation_id;
    let mut driver = restarted_with(&world, vec![entry]);
    assert!(
        driver
            .effects_matching(|e| match e {
                Effect::QueryOperationStatus { operation: o } => Some(*o),
                _ => None,
            })
            .contains(&operation),
        "the server is asked before anything is resent"
    );
    assert_eq!(driver.core.unsettled_operations(), 1);

    // The message arrives with the first page and shows the edit as being saved.
    driver.load_history(general, vec![mine.clone()], false);
    let saving = row(&driver, general, mine.id);
    assert_eq!(saving.edit, EditState::Saving);
    assert_eq!(saving.document, parsed("typed before the crash"));

    // The server never saw it: the same operation is sent again.
    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    let requests = edit_requests(&out);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1.operation_id, operation);
    assert_eq!(requests[0].1.base_revision, 1);

    let committed = world.edited(&mine, "typed before the crash");
    let out = edit_result(&mut driver, operation, respond(&committed));
    assert_eq!(settled(&out), vec![operation]);
    assert_eq!(row(&driver, general, mine.id).edit, EditState::None);
    assert_eq!(driver.core.unsettled_operations(), 0);
}

#[test]
fn a_recovered_edit_that_had_committed_updates_the_row_and_settles() {
    let (world, _, mine) = setup();
    let general = world.channel.id;
    let entry = recovered_edit(&world, &mine, "it went through");
    let operation = entry.operation_id;
    let mut driver = restarted_with(&world, vec![entry]);
    driver.load_history(general, vec![mine.clone()], false);
    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::EditCommitted {
            message: world.edited(&mine, "it went through"),
        }),
    });
    assert_eq!(settled(&out), vec![operation]);
    let done = row(&driver, general, mine.id);
    assert_eq!(
        (done.edit.clone(), done.revision()),
        (EditState::None, Some(2))
    );
}

#[test]
fn a_recovered_edit_that_went_stale_becomes_a_conflict() {
    let (world, _, mine) = setup();
    let general = world.channel.id;
    let entry = recovered_edit(&world, &mine, "typed before the crash");
    let operation = entry.operation_id;
    let mut driver = restarted_with(&world, vec![entry]);
    driver.load_history(
        general,
        vec![world.edited(&mine, "changed elsewhere")],
        false,
    );
    driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    edit_result(
        &mut driver,
        operation,
        Err(rejected(ErrorCode::StaleRevision)),
    );
    driver.send(Input::MessageFetched {
        message: mine.id,
        result: respond(&world.edited(&mine, "changed elsewhere")),
    });
    assert_eq!(
        row(&driver, general, mine.id).edit,
        EditState::Conflict {
            text: "typed before the crash".into()
        }
    );
}

#[test]
fn a_recovered_edit_for_a_conversation_that_is_gone_is_released() {
    let (world, _, mine) = setup();
    let mut entry = recovered_edit(&world, &mine, "orphan");
    entry.conversation_id = ConversationId::generate();
    let operation = entry.operation_id;
    let driver = restarted_with(&world, vec![entry]);
    assert!(
        driver
            .effects
            .contains(&Effect::SettleSubmission { operation })
    );
    assert_eq!(driver.core.unsettled_operations(), 0);
}

#[test]
fn journal_files_from_before_edits_still_load() {
    let (world, _, _) = setup();
    let general = world.channel.id;
    let sent = JournalEntry {
        operation_id: OperationId::generate(),
        user_id: world.me.id,
        conversation_id: general,
        document: parsed("old send"),
        submitted_at: Timestamp::now(),
        kind: JournalKind::Send,
        thread_root: None,
        also_in_channel: false,
        attachments: Vec::new(),
    };
    // A send is stored exactly as it was before edits existed: no `kind` field at all.
    let mut stored = serde_json::to_value(&sent).unwrap();
    assert!(stored.get("kind").is_none(), "{stored}");
    let loaded: JournalEntry = serde_json::from_value(stored.clone()).unwrap();
    assert_eq!(loaded, sent);
    // And an entry written by a build that knew nothing of `kind` has none to read.
    stored.as_object_mut().unwrap().remove("kind");
    assert_eq!(
        serde_json::from_value::<JournalEntry>(stored).unwrap().kind,
        JournalKind::Send
    );

    let edit = recovered_edit(&world, &world.message(general, &world.me, 5, "x"), "edited");
    let json = serde_json::to_value(&edit).unwrap();
    assert_eq!(json["kind"]["type"], "edit");
    assert_eq!(serde_json::from_value::<JournalEntry>(json).unwrap(), edit);
}

// ----- Deleting -----

#[test]
fn deleting_sends_the_request_and_the_row_goes_when_it_is_answered() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let kind = ActionKind::DeleteMessage(mine.id);
    let out = driver.send(Input::DeleteMessage {
        conversation: general,
        message: mine.id,
    });
    assert!(
        out.effects
            .contains(&Effect::DeleteMessage { message: mine.id })
    );
    assert_eq!(driver.core.action(kind), &ActionState::InFlight);
    assert!(
        has_row(&driver, general, mine.id),
        "the row stays until the server answers"
    );

    // A second click while it is in flight does nothing.
    let out = driver.send(Input::DeleteMessage {
        conversation: general,
        message: mine.id,
    });
    assert!(out.effects.is_empty());

    driver.send(Input::DeleteResult {
        message: mine.id,
        result: respond(&world.tombstone(&mine)),
    });
    assert!(!has_row(&driver, general, mine.id));
    assert_eq!(driver.core.action(kind), &ActionState::Idle);
    assert!(driver.core.timeline(general).is_empty());
}

#[test]
fn a_failed_delete_keeps_the_message_and_can_be_retried() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let kind = ActionKind::DeleteMessage(mine.id);
    let delete = Input::DeleteMessage {
        conversation: general,
        message: mine.id,
    };
    driver.send(delete.clone());
    driver.send(Input::DeleteResult {
        message: mine.id,
        result: Err(lost()),
    });
    assert!(has_row(&driver, general, mine.id));
    assert!(matches!(driver.core.action(kind), ActionState::Failed(_)));
    assert!(driver.core.notice().is_some());

    let out = driver.send(delete);
    assert!(
        out.effects
            .contains(&Effect::DeleteMessage { message: mine.id })
    );
    driver.send(Input::DeleteResult {
        message: mine.id,
        result: respond(&world.tombstone(&mine)),
    });
    assert!(!has_row(&driver, general, mine.id));
}

#[test]
fn deleting_a_message_that_is_already_gone_just_removes_it() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    driver.send(Input::DeleteMessage {
        conversation: general,
        message: mine.id,
    });
    driver.send(Input::DeleteResult {
        message: mine.id,
        result: Err(rejected(ErrorCode::NotFound)),
    });
    assert!(!has_row(&driver, general, mine.id));
    assert_eq!(
        driver.core.action(ActionKind::DeleteMessage(mine.id)),
        &ActionState::Idle
    );
}

#[test]
fn the_deletion_event_removes_the_row_and_the_late_response_changes_nothing() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    driver.send(Input::DeleteMessage {
        conversation: general,
        message: mine.id,
    });
    driver.send(world.deleted_event(11, &world.tombstone(&mine)));
    assert!(!has_row(&driver, general, mine.id));
    assert_eq!(
        driver.core.action(ActionKind::DeleteMessage(mine.id)),
        &ActionState::Idle
    );
    let out = driver.send(Input::DeleteResult {
        message: mine.id,
        result: respond(&world.tombstone(&mine)),
    });
    assert!(out.is_empty());
}

#[test]
fn a_deletion_that_races_an_edit_wins() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "too late");

    let out = driver.send(world.deleted_event(11, &world.tombstone(&mine)));
    assert!(!has_row(&driver, general, mine.id));
    assert_eq!(
        settled(&out),
        vec![operation],
        "the edit's journal entry is released"
    );
    assert_eq!(driver.core.unsettled_operations(), 0);

    // The edit's response was refused, because the message is gone: nothing comes back to life.
    let out = edit_result(&mut driver, operation, Err(rejected(ErrorCode::NotFound)));
    assert!(out.is_empty());
    assert!(!has_row(&driver, general, mine.id));
}

#[test]
fn an_edit_result_that_is_a_tombstone_removes_the_message_with_a_notice() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "too late");
    edit_result(&mut driver, operation, Err(lost()));
    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::EditCommitted {
            message: world.tombstone(&mine),
        }),
    });
    assert_eq!(settled(&out), vec![operation]);
    assert!(!has_row(&driver, general, mine.id));
    assert!(driver.core.notice().is_some());
    assert_eq!(driver.core.unsettled_operations(), 0);
}

#[test]
fn deleting_a_message_of_mine_that_is_still_being_confirmed_never_shows_it() {
    let (world, mut driver, _) = setup();
    let general = world.channel.id;
    let out = driver.send(Input::SendMessage {
        conversation: general,
        text: "short lived".into(),
    });
    let operation = journaled(&out)[0].operation_id;
    driver.send(Input::SubmissionPersisted { operation });
    let created = world.message(general, &world.me, 11, "short lived");
    // The replay gives the creation as a tombstone carrying the operation id.
    let out = driver.send(world.event(world.tombstone(&created), Some(operation)));
    assert_eq!(settled(&out), vec![operation]);
    assert!(
        !driver
            .core
            .timeline(general)
            .iter()
            .any(|item| item.document == parsed("short lived"))
    );
    assert_eq!(driver.core.unsettled_operations(), 0);
}

// ----- Events: order, duplicates, unknowns -----

#[test]
fn edit_events_apply_only_when_newer_and_duplicates_change_nothing() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let rev2 = world.edited(&mine, "two");
    let rev3 = world.edited(&rev2, "three");

    driver.send(world.edited_event(11, &rev3, None));
    assert_eq!(row(&driver, general, mine.id).document, parsed("three"));
    // An older revision arriving later (out of order) is ignored.
    driver.send(world.edited_event(12, &rev2, None));
    assert_eq!(row(&driver, general, mine.id).document, parsed("three"));
    assert_eq!(row(&driver, general, mine.id).revision(), Some(3));
    // A duplicate changes nothing at all.
    let out = driver.send(world.edited_event(13, &rev3, None));
    assert!(
        out.changes
            .iter()
            .all(|c| !matches!(c, Change::TimelineItemUpdated { .. })),
        "{:?}",
        out.changes
    );
}

#[test]
fn events_for_messages_or_conversations_that_are_not_known_are_ignored() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let stranger = world.message(general, &world.other, 6, "never loaded");
    let out = driver.send(world.edited_event(11, &world.edited(&stranger, "x"), None));
    assert!(
        out.changes
            .iter()
            .all(|c| !matches!(c, Change::TimelineSplice { .. }))
    );
    assert!(!has_row(&driver, general, stranger.id));
    driver.send(world.deleted_event(12, &world.tombstone(&stranger)));
    assert!(!has_row(&driver, general, stranger.id));

    let elsewhere = channel(world.workspace.id, "elsewhere");
    let foreign = world.message(elsewhere.id, &world.other, 7, "foreign");
    driver.send(world.edited_event(13, &world.edited(&foreign, "x"), None));
    driver.send(world.deleted_event(14, &world.tombstone(&foreign)));
    assert!(driver.core.timeline(elsewhere.id).is_empty());
    assert_eq!(row(&driver, general, mine.id).revision(), Some(1));
}

#[test]
fn an_edit_that_lands_while_a_page_is_in_flight_is_not_lost_to_the_older_page() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(
        general,
        vec![world.message(general, &world.me, 5, "newest page")],
        true,
    );
    let old = world.message(general, &world.other, 2, "old text");
    driver.send(Input::LoadOlder {
        conversation: general,
    });
    // The edit and a deletion arrive before the page that predates them.
    let doomed = world.message(general, &world.other, 3, "doomed");
    driver.send(world.edited_event(11, &world.edited(&old, "new text"), None));
    driver.send(world.deleted_event(12, &world.tombstone(&doomed)));
    driver.load_history(general, vec![old.clone(), doomed.clone()], false);
    assert_eq!(row(&driver, general, old.id).document, parsed("new text"));
    assert!(
        !has_row(&driver, general, doomed.id),
        "a deleted message is not resurrected"
    );
}

#[test]
fn a_remote_edit_while_mine_is_saving_moves_the_server_version_underneath() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "mine");
    driver.send(world.edited_event(11, &world.edited(&mine, "theirs"), None));
    // The row still shows what the person is saving.
    assert_eq!(row(&driver, general, mine.id).document, parsed("mine"));
    // Our edit was based on revision 1, so the server refuses it; the newer version appears.
    edit_result(
        &mut driver,
        operation,
        Err(rejected(ErrorCode::StaleRevision)),
    );
    driver.send(Input::MessageFetched {
        message: mine.id,
        result: respond(&world.edited(&mine, "theirs")),
    });
    let conflicted = row(&driver, general, mine.id);
    assert_eq!(conflicted.document, parsed("theirs"));
    assert!(matches!(conflicted.edit, EditState::Conflict { .. }));
}

// ----- Unread counts -----

fn unread_world() -> (World, Driver) {
    let mut world = World::new();
    world.user_states = vec![read_state(world.channel.id, 4, 0)];
    let driver = Driver::ready(&world);
    (world, driver)
}

#[test]
fn deleting_an_unread_message_from_someone_else_stops_counting_it() {
    let (world, mut driver) = unread_world();
    let general = world.channel.id;
    let one = world.message(general, &world.other, 11, "one");
    let two = world.message(general, &world.other, 12, "two");
    driver.send(world.event(one.clone(), None));
    driver.send(world.event(two.clone(), None));
    assert_eq!(unread(&driver, general), 2);

    driver.send(world.deleted_event(13, &world.tombstone(&one)));
    assert_eq!(unread(&driver, general), 1);
    // Deleting it again (a duplicate delivery) changes nothing.
    driver.send(world.deleted_event(13, &world.tombstone(&one)));
    assert_eq!(unread(&driver, general), 1);
    assert_eq!(driver.core.total_unread(), 1);
}

#[test]
fn deleting_a_message_that_was_not_unread_changes_no_count() {
    let (world, mut driver) = unread_world();
    let general = world.channel.id;
    // Already read: its position is at or before the read position.
    let read = world.message(general, &world.other, 3, "read");
    driver.load_history(general, vec![read.clone()], false);
    let unread_one = world.message(general, &world.other, 11, "unread");
    driver.send(world.event(unread_one, None));
    driver.send(world.deleted_event(12, &world.tombstone(&read)));
    assert_eq!(unread(&driver, general), 1);

    // My own messages never counted.
    let mine = world.message(general, &world.me, 13, "mine");
    driver.send(world.event(mine.clone(), None));
    let before = unread(&driver, general);
    driver.send(world.deleted_event(14, &world.tombstone(&mine)));
    assert_eq!(unread(&driver, general), before);
}

#[test]
fn a_message_counted_in_the_baseline_stops_counting_even_when_it_is_not_loaded() {
    let mut world = World::new();
    world.user_states = vec![read_state(world.channel.id, 4, 3)];
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    // Nothing is loaded for #random, which has an unread count from the bootstrap too.
    let unloaded = world.message(general, &world.other, 8, "in the baseline");
    driver.send(world.deleted_event(11, &world.tombstone(&unloaded)));
    assert_eq!(unread(&driver, general), 2);
}

#[test]
fn a_deletion_at_or_before_the_baseline_is_already_reflected() {
    let mut world = World::new();
    world.user_states = vec![read_state(world.channel.id, 4, 1)];
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    // The log is at 10; the state was computed after this deletion, so it must not subtract again.
    let gone = world.message(general, &world.other, 8, "deleted before the baseline");
    driver.send(world.deleted_event(9, &world.tombstone(&gone)));
    assert_eq!(unread(&driver, general), 1);
}

#[test]
fn a_tombstone_creation_never_appears_and_never_counts() {
    let (world, mut driver) = unread_world();
    let general = world.channel.id;
    let created = world.message(general, &world.other, 11, "gone");
    let tombstone = world.tombstone(&created);

    driver.send(world.event(tombstone.clone(), None));
    assert!(!has_row(&driver, general, created.id));
    assert_eq!(unread(&driver, general), 0);
    driver.send(world.edited_event(12, &tombstone, None));
    assert!(!has_row(&driver, general, created.id));
    assert_eq!(unread(&driver, general), 0);
    // And the deletion that follows subtracts nothing, since nothing was added.
    driver.send(world.deleted_event(13, &tombstone));
    assert_eq!(unread(&driver, general), 0);
}

#[test]
fn the_count_is_the_same_whether_the_client_saw_the_creation_live_or_by_replay() {
    // Two other messages stay; one is created and deleted. Three ways the client can learn it.
    let scenarios = [
        "live then deleted",
        "replayed tombstone then deleted",
        "cursor between",
    ];
    for scenario in scenarios {
        let (world, mut driver) = unread_world();
        let general = world.channel.id;
        let stays = world.message(general, &world.other, 11, "stays");
        let doomed = world.message(general, &world.other, 12, "doomed");
        let also = world.message(general, &world.other, 14, "also stays");
        let tombstone = world.tombstone(&doomed);
        match scenario {
            "live then deleted" => {
                driver.send(world.event(stays.clone(), None));
                driver.send(world.event(doomed.clone(), None));
                assert_eq!(unread(&driver, general), 2);
                driver.send(world.deleted_event(13, &tombstone));
            }
            "replayed tombstone then deleted" => {
                driver.send(world.event(stays.clone(), None));
                driver.send(world.event(tombstone.clone(), None));
                driver.send(world.deleted_event(13, &tombstone));
            }
            _ => {
                // The creation was seen live; then the device was away. The replay starts after
                // it and serves the deletion (and a rewritten edit) only.
                driver.send(world.event(stays.clone(), None));
                driver.send(world.event(doomed.clone(), None));
                driver.send(world.edited_event(13, &tombstone, None));
                driver.send(world.deleted_event(14, &tombstone));
            }
        }
        let later = if scenario == "cursor between" { 15 } else { 14 };
        let also = {
            let mut m = also.clone();
            m.seq = seq(later);
            m
        };
        driver.send(world.event(also.clone(), None));
        assert_eq!(unread(&driver, general), 2, "{scenario}");
        assert!(!has_row(&driver, general, doomed.id), "{scenario}");
        assert!(has_row(&driver, general, stays.id), "{scenario}");
        assert!(has_row(&driver, general, also.id), "{scenario}");
    }
}

#[test]
fn a_state_event_between_creation_and_deletion_is_not_subtracted_twice() {
    let (world, mut driver) = unread_world();
    let general = world.channel.id;
    let doomed = world.message(general, &world.other, 11, "doomed");
    driver.send(world.event(doomed.clone(), None));
    // The server's own state after the creation counts it.
    driver.send(world.event_at(
        12,
        Event::UserStateChanged {
            state: read_state(general, 4, 1),
        },
    ));
    assert_eq!(unread(&driver, general), 1);
    driver.send(world.deleted_event(13, &world.tombstone(&doomed)));
    assert_eq!(
        unread(&driver, general),
        0,
        "the state included it, so the deletion removes it"
    );
    // A deletion after a state that was computed with the message already gone does nothing.
    driver.send(world.deleted_event(13, &world.tombstone(&doomed)));
    assert_eq!(unread(&driver, general), 0);
}

#[test]
fn a_deletion_before_a_conversation_baseline_that_replay_still_delivers_is_not_subtracted() {
    // Start from a cache that knows nothing of #random's read state.
    let mut world = World::new();
    let general = world.channel.id;
    let random = world.second_channel.id;
    let mut first = Driver::ready(&world);
    first.load_history(general, vec![], false);
    let mut cache = first.last_cache().unwrap().clone();
    cache.workspaces[0]
        .user_state
        .retain(|saved| saved.state.conversation_id != random);
    cache.workspaces[0].timelines.clear();

    // The bootstrap is far ahead of the cached cursor and says #random has one unread message.
    world.head = seq(20);
    world.user_states = vec![read_state(random, 4, 1)];
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache),
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    assert_eq!(unread(&driver, random), 1);

    // Replay delivers a deletion at 15 that the baseline (current to 20) already reflects.
    let gone = world.message(random, &world.other, 14, "deleted before the baseline");
    driver.send(world.deleted_event(15, &world.tombstone(&gone)));
    assert_eq!(unread(&driver, random), 1);
    // One after the baseline does subtract, if it was counted there.
    let counted = world.message(random, &world.other, 8, "in the baseline");
    driver.send(world.deleted_event(25, &world.tombstone(&counted)));
    assert_eq!(unread(&driver, random), 0);
}

#[test]
fn a_tombstone_creation_in_flight_keeps_the_page_from_bringing_the_message_back() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let doomed = world.message(general, &world.other, 11, "doomed");
    // The first page is still in flight when the replayed tombstone arrives.
    driver.send(world.event(world.tombstone(&doomed), None));
    driver.load_history(general, vec![doomed.clone()], false);
    assert!(!has_row(&driver, general, doomed.id));
}

#[test]
fn a_saved_read_state_from_before_counted_messages_were_recorded_still_reads() {
    let world = World::new();
    let saved = lazy_client_core::CachedUserState {
        state: read_state(world.channel.id, 4, 2),
        as_of: seq(12),
        counted: Vec::new(),
        counted_mentions: Vec::new(),
        rebuilt_through: None,
    };
    // Written as it always was: nothing extra for a state that counted no live message.
    let json = serde_json::to_value(&saved).unwrap();
    assert!(json.get("counted").is_none(), "{json}");
    assert_eq!(
        serde_json::from_value::<lazy_client_core::CachedUserState>(json).unwrap(),
        saved
    );

    let counting = lazy_client_core::CachedUserState {
        counted: vec![seq(13), seq(15)],
        counted_mentions: vec![seq(15)],
        rebuilt_through: None,
        ..saved
    };
    let json = serde_json::to_value(&counting).unwrap();
    assert_eq!(
        serde_json::from_value::<lazy_client_core::CachedUserState>(json).unwrap(),
        counting
    );
}

#[test]
fn what_was_counted_live_is_remembered_across_a_restart() {
    let (world, mut driver) = unread_world();
    let general = world.channel.id;
    let counted = world.message(general, &world.other, 11, "counted live");
    let replayed = world.message(general, &world.other, 12, "seen only as a tombstone");
    driver.send(world.event(counted.clone(), None));
    driver.send(world.event(world.tombstone(&replayed), None));
    assert_eq!(unread(&driver, general), 1);
    let cache = driver.last_cache().unwrap().clone();

    // The app restarts from that cache; replay then delivers both deletions.
    let mut restarted = Driver::new();
    restarted.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache),
        ..Default::default()
    }));
    assert_eq!(unread(&restarted, general), 1);
    restarted.send(world.deleted_event(13, &world.tombstone(&replayed)));
    assert_eq!(unread(&restarted, general), 1, "it was never counted");
    restarted.send(world.deleted_event(14, &world.tombstone(&counted)));
    assert_eq!(unread(&restarted, general), 0, "it was");
}

// ----- The cache -----

#[test]
fn the_cache_holds_the_servers_version_of_an_edited_message() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    let operation = start_edit(&mut driver, general, mine.id, "unsaved text");
    // Something unrelated dirties the cache while the edit is still in flight.
    driver.send(Input::DismissNotice);
    driver.send(world.event(world.message(general, &world.other, 11, "later"), None));
    let cached = driver.last_cache().unwrap();
    let timeline = cached.workspaces[0]
        .timelines
        .iter()
        .find(|t| t.conversation == general)
        .unwrap();
    let message = timeline.messages.iter().find(|m| m.id == mine.id).unwrap();
    assert_eq!(message.document, mine.document, "never the unsaved text");

    // Once saved, the cache carries the new revision and the edit time.
    let committed = world.edited(&mine, "unsaved text");
    edit_result(&mut driver, operation, respond(&committed));
    driver.send(world.event(world.message(general, &world.other, 12, "again"), None));
    let cached = driver.last_cache().unwrap();
    let timeline = cached.workspaces[0]
        .timelines
        .iter()
        .find(|t| t.conversation == general)
        .unwrap();
    let message = timeline.messages.iter().find(|m| m.id == mine.id).unwrap();
    assert_eq!(message.revision, 2);
    assert_eq!(message.edited_at, committed.edited_at);
    assert_eq!(message.document, parsed("unsaved text"));
}

#[test]
fn a_deleted_message_leaves_the_cache() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    driver.send(world.deleted_event(11, &world.tombstone(&mine)));
    let cached = driver.last_cache().unwrap();
    let timeline = cached.workspaces[0]
        .timelines
        .iter()
        .find(|t| t.conversation == general)
        .unwrap();
    assert!(
        timeline
            .messages
            .iter()
            .all(|m| m.id != mine.id && !m.deleted)
    );
}

// ----- Permalinks -----

fn link(world: &World, conversation: ConversationId, message: MessageId) -> Permalink {
    Permalink {
        workspace_id: world.workspace.id,
        conversation_id: conversation,
        message_id: message,
    }
}

/// `count` messages by someone else at positions `first..first + count`.
fn page(world: &World, conversation: ConversationId, first: i64, count: i64) -> Vec<Message> {
    (first..first + count)
        .map(|at| world.message(conversation, &world.other, at, &format!("m{at}")))
        .collect()
}

fn older_requests(driver: &Driver, conversation: ConversationId) -> usize {
    driver
        .effects_matching(|e| match e {
            Effect::FetchHistory {
                conversation: c,
                before: Some(_),
                ..
            } if *c == conversation => Some(()),
            _ => None,
        })
        .len()
}

#[test]
fn a_loaded_message_is_jumped_to_without_any_request() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    driver.send(Input::SelectConversation(world.second_channel.id));
    let out = driver.send(Input::OpenPermalink(link(&world, general, mine.id)));
    assert!(fetches(&out).is_empty());
    assert_eq!(driver.core.selected_conversation(), Some(general));
    assert_eq!(
        driver.core.jump_target(),
        Some(JumpTarget {
            conversation: general,
            key: ItemKey::Message(mine.id),
            thread: None,
        })
    );
    assert!(out.changes.contains(&Change::JumpTargetChanged));

    driver.send(Input::ClearJumpTarget);
    assert_eq!(driver.core.jump_target(), None);

    // Opening it a second time announces it again.
    let out = driver.send(Input::OpenPermalink(link(&world, general, mine.id)));
    assert!(out.changes.contains(&Change::JumpTargetChanged));
}

#[test]
fn a_message_of_mine_is_found_by_its_operation_key() {
    let (world, mut driver, _) = setup();
    let general = world.channel.id;
    let out = driver.send(Input::SendMessage {
        conversation: general,
        text: "sent from here".into(),
    });
    let operation = journaled(&out)[0].operation_id;
    driver.send(Input::SubmissionPersisted { operation });
    let created = world.message(general, &world.me, 11, "sent from here");
    driver.send(world.event(created.clone(), Some(operation)));
    driver.send(Input::OpenPermalink(link(&world, general, created.id)));
    assert_eq!(
        driver.core.jump_target().unwrap().key,
        ItemKey::Operation(operation)
    );
}

#[test]
fn older_history_is_paged_until_the_message_appears() {
    let mut world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    world.head = seq(10);
    driver.load_history(general, page(&world, general, 1000, 5), true);
    let target = world.message(general, &world.other, 940, "far back");

    let out = driver.send(Input::OpenPermalink(link(&world, general, target.id)));
    assert_eq!(fetches(&out), vec![target.id]);
    assert_eq!(driver.core.jump_target(), None);

    // The message is known now; two pages do not reach it, the third does.
    let out = driver.send(Input::MessageFetched {
        message: target.id,
        result: respond(&target),
    });
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::FetchHistory {
            before: Some(_),
            ..
        }
    )));
    driver.load_history(general, page(&world, general, 990, 10), true);
    assert_eq!(driver.core.jump_target(), None);
    driver.load_history(general, page(&world, general, 960, 30), true);
    assert_eq!(driver.core.jump_target(), None);
    let mut last = page(&world, general, 930, 30);
    last.retain(|m| m.seq != target.seq);
    last.push(target.clone());
    driver.load_history(general, last, true);
    assert_eq!(
        driver.core.jump_target(),
        Some(JumpTarget {
            conversation: general,
            key: ItemKey::Message(target.id),
            thread: None,
        })
    );
    assert_eq!(older_requests(&driver, general), 3);
    assert!(driver.core.notice().is_none());
}

#[test]
fn paging_stops_at_the_bound_with_a_notice() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(general, page(&world, general, 5000, 5), true);
    let target = world.message(general, &world.other, 2, "too far");
    driver.send(Input::OpenPermalink(link(&world, general, target.id)));
    driver.send(Input::MessageFetched {
        message: target.id,
        result: respond(&target),
    });
    let mut first = 4000;
    for _ in 0..PERMALINK_MAX_PAGES {
        assert_eq!(driver.core.notice(), None);
        driver.load_history(general, page(&world, general, first, 5), true);
        first -= 100;
    }
    assert_eq!(
        older_requests(&driver, general),
        PERMALINK_MAX_PAGES as usize
    );
    assert_eq!(driver.core.jump_target(), None);
    assert!(driver.core.notice().is_some(), "{:?}", driver.core.notice());
    // No further page is requested, and the person can still scroll up for it.
    assert_eq!(
        driver
            .effects_matching(|e| matches!(e, Effect::FetchHistory { .. }).then_some(()))
            .len(),
        1 + PERMALINK_MAX_PAGES as usize,
        "the newest page, then the bounded older pages"
    );
}

#[test]
fn a_message_that_is_not_in_its_range_is_reported_as_unavailable() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    // Its position is inside what is loaded, but the message is not there (it was deleted).
    let ghost = world.message(general, &world.other, 22, "ghost");
    let mut loaded = page(&world, general, 20, 5);
    loaded.retain(|m| m.seq != ghost.seq);
    driver.load_history(general, loaded, true);
    driver.send(Input::OpenPermalink(link(&world, general, ghost.id)));
    driver.send(Input::MessageFetched {
        message: ghost.id,
        result: respond(&ghost),
    });
    assert_eq!(driver.core.jump_target(), None);
    assert!(driver.core.notice().is_some());
    assert_eq!(
        older_requests(&driver, general),
        0,
        "the range is loaded, so paging further cannot help"
    );
}

#[test]
fn links_that_cannot_open_say_why() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;

    // Another workspace.
    let mut foreign = link(&world, general, mine.id);
    foreign.workspace_id = lazy_domain::WorkspaceId::generate();
    let out = driver.send(Input::OpenPermalink(foreign));
    assert!(fetches(&out).is_empty());
    assert!(driver.core.notice().is_some());
    driver.send(Input::DismissNotice);

    // Missing or unreadable.
    let unknown = MessageId::generate();
    for error in [ErrorCode::NotFound, ErrorCode::Forbidden] {
        let out = driver.send(Input::OpenPermalink(link(&world, general, unknown)));
        assert_eq!(fetches(&out), vec![unknown]);
        driver.send(Input::MessageFetched {
            message: unknown,
            result: Err(rejected(error)),
        });
        assert!(driver.core.notice().is_some(), "{error:?}");
        assert_eq!(driver.core.jump_target(), None);
        driver.send(Input::DismissNotice);
    }

    // Deleted.
    let gone = world.tombstone(&world.message(general, &world.other, 3, "gone"));
    driver.send(Input::OpenPermalink(link(&world, general, gone.id)));
    driver.send(Input::MessageFetched {
        message: gone.id,
        result: respond(&gone),
    });
    assert!(driver.core.notice().unwrap().contains("deleted"));
    driver.send(Input::DismissNotice);

    // The server cannot be reached.
    driver.send(Input::OpenPermalink(link(&world, general, unknown)));
    driver.send(Input::MessageFetched {
        message: unknown,
        result: Err(lost()),
    });
    assert!(driver.core.notice().is_some());
}

#[test]
fn a_message_in_a_channel_that_is_not_joined_offers_to_join_and_then_opens() {
    let (world, mut driver, _) = setup();
    let hidden = channel(world.workspace.id, "hidden");
    let message = world.message(hidden.id, &world.other, 8, "in the hidden channel");
    let wanted = link(&world, hidden.id, message.id);

    driver.send(Input::OpenPermalink(wanted));
    driver.send(Input::MessageFetched {
        message: message.id,
        result: respond(&message),
    });
    assert!(driver.core.notice().is_some());
    assert_eq!(driver.core.join_offer(), Some(hidden.id));
    assert_eq!(driver.core.jump_target(), None);
    assert_eq!(
        driver.core.selected_conversation(),
        Some(world.channel.id),
        "no read-only view"
    );

    // Joining makes the channel known; the link then opens by itself.
    let out = driver.send(world.event_at(
        12,
        Event::MemberAdded {
            conversation: hidden.clone(),
            user_id: world.me.id,
        },
    ));
    assert_eq!(driver.core.join_offer(), None);
    assert_eq!(
        fetches(&out),
        vec![message.id],
        "the link is opened again, now that it can be"
    );
    assert!(driver.core.notice().is_none());

    // Any other notice withdraws the offer.
    driver.send(Input::OpenPermalink(link(
        &world,
        hidden.id,
        MessageId::generate(),
    )));
    driver.send(Input::MessageFetched {
        message: wanted.message_id,
        result: respond(&message),
    });
}

#[test]
fn choosing_another_conversation_abandons_a_link_that_is_paging() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(general, page(&world, general, 5000, 5), true);
    let target = world.message(general, &world.other, 2, "far");
    driver.send(Input::OpenPermalink(link(&world, general, target.id)));
    driver.send(Input::MessageFetched {
        message: target.id,
        result: respond(&target),
    });
    let before = older_requests(&driver, general);
    driver.send(Input::SelectConversation(world.second_channel.id));
    driver.load_history(general, page(&world, general, 4000, 5), true);
    assert_eq!(
        older_requests(&driver, general),
        before,
        "no more pages for an abandoned link"
    );
    assert_eq!(driver.core.jump_target(), None);
}

#[test]
fn the_jump_target_goes_when_the_person_leaves_or_the_message_is_deleted() {
    let (world, mut driver, mine) = setup();
    let general = world.channel.id;
    driver.send(Input::OpenPermalink(link(&world, general, mine.id)));
    assert!(driver.core.jump_target().is_some());
    driver.send(Input::SelectConversation(world.second_channel.id));
    assert_eq!(driver.core.jump_target(), None);

    driver.send(Input::OpenPermalink(link(&world, general, mine.id)));
    assert!(driver.core.jump_target().is_some());
    driver.send(world.deleted_event(11, &world.tombstone(&mine)));
    assert_eq!(driver.core.jump_target(), None);
}

#[test]
fn a_deletion_while_fetching_a_link_says_so() {
    let (world, mut driver, _) = setup();
    let general = world.channel.id;
    let other = world.message(general, &world.other, 8, "soon gone");
    driver.send(Input::OpenPermalink(link(&world, general, other.id)));
    driver.send(world.deleted_event(11, &world.tombstone(&other)));
    assert!(driver.core.notice().unwrap().contains("deleted"));
    // The answer that follows does nothing more.
    driver.send(Input::MessageFetched {
        message: other.id,
        result: respond(&other),
    });
    assert_eq!(driver.core.jump_target(), None);
}
