mod common;

use common::{Driver, World, message_ids, seq, user};
use lazy_client_core::{
    Change, ConnectionState, Delivery, Effect, HistoryStatus, Input, ItemKey, JournalEntry,
    RequestError, RetryKind, SessionPhase, SocketEvent, StartupState, UNKNOWN_MEMBER_NAME,
};
use lazy_domain::{ConversationId, MessageDocument, OperationId, Timestamp};
use lazy_protocol::{
    ApiError, ErrorCode, Event, EventEnvelope, HistoryPage, OperationStatus, SendMessageResponse,
    ServerFrame, SnapshotReason,
};

/// Sends `text` and returns the new operation id.
fn compose(driver: &mut Driver, conversation: ConversationId, text: &str) -> OperationId {
    let out = driver.send(Input::SendMessage {
        conversation,
        text: text.into(),
    });
    out.effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.operation_id),
            _ => None,
        })
        .expect("a submission should be persisted")
}

/// Sends `text` through the journal write so a send request is in flight.
fn compose_and_persist(
    driver: &mut Driver,
    conversation: ConversationId,
    text: &str,
) -> OperationId {
    let operation = compose(driver, conversation, text);
    driver.send(Input::SubmissionPersisted { operation });
    operation
}

fn sends(driver: &Driver) -> Vec<OperationId> {
    driver.effects_matching(|e| match e {
        Effect::SendMessage { operation, .. } => Some(*operation),
        _ => None,
    })
}

fn settles(driver: &Driver) -> Vec<OperationId> {
    driver.effects_matching(|e| match e {
        Effect::SettleSubmission { operation } => Some(*operation),
        _ => None,
    })
}

fn queries(driver: &Driver) -> Vec<OperationId> {
    driver.effects_matching(|e| match e {
        Effect::QueryOperationStatus { operation } => Some(*operation),
        _ => None,
    })
}

fn unknown_error() -> RequestError {
    RequestError::Unknown("timed out".into())
}

fn rejected(code: ErrorCode, message: &str) -> RequestError {
    RequestError::Rejected(ApiError::new(code, message))
}

#[test]
fn starts_signed_out_without_a_stored_session() {
    let mut driver = Driver::new();
    assert_eq!(driver.core.phase(), &SessionPhase::Loading);
    driver.send(Input::Started(StartupState::default()));
    assert_eq!(
        driver.core.phase(),
        &SessionPhase::SignedOut { last_error: None }
    );
}

#[test]
fn restored_session_loads_the_bootstrap() {
    let mut driver = Driver::new();
    let out = driver.send(Input::Started(StartupState {
        session_restored: true,
        ..Default::default()
    }));
    assert_eq!(driver.core.phase(), &SessionPhase::Loading);
    assert_eq!(out.effects, vec![Effect::LoadBootstrap]);
}

#[test]
fn sign_in_validates_before_asking_the_server() {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState::default()));

    let out = driver.send(Input::SignIn {
        username: "x".into(),
        password: "pw".into(),
    });
    assert!(out.effects.is_empty());
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut {
            last_error: Some(_)
        }
    ));

    let out = driver.send(Input::SignIn {
        username: "Zoe".into(),
        password: "correct horse".into(),
    });
    assert_eq!(driver.core.phase(), &SessionPhase::SigningIn);
    assert!(
        matches!(&out.effects[..], [Effect::Login(request)] if request.username.as_str() == "zoe")
    );

    let me = user("zoe");
    let out = driver.send(Input::Authenticated { user: me });
    assert_eq!(driver.core.phase(), &SessionPhase::Loading);
    assert_eq!(out.effects, vec![Effect::LoadBootstrap]);
}

#[test]
fn registration_checks_name_and_password() {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState::default()));
    let out = driver.send(Input::Register {
        username: "zoe".into(),
        display_name: "Zoë".into(),
        password: "short".into(),
    });
    assert!(out.effects.is_empty());
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut { last_error: Some(message) } if message.contains("at least 8")
    ));

    let out = driver.send(Input::Register {
        username: "zoe".into(),
        display_name: "Zoë".into(),
        password: "long enough".into(),
    });
    assert!(matches!(&out.effects[..], [Effect::Register(_)]));
}

#[test]
fn auth_failure_returns_to_sign_in_with_the_reason() {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState::default()));
    driver.send(Input::SignIn {
        username: "zoe".into(),
        password: "whatever1".into(),
    });
    driver.send(Input::AuthFailed(rejected(
        ErrorCode::InvalidCredentials,
        "Wrong password.",
    )));
    assert_eq!(
        driver.core.phase(),
        &SessionPhase::SignedOut {
            last_error: Some("Wrong password.".into())
        }
    );
}

#[test]
fn bootstrap_builds_the_directory_subscribes_and_loads_the_first_channel() {
    let world = World::new();
    let driver = Driver::ready(&world);
    let core = &driver.core;

    assert_eq!(core.me().unwrap().id, world.me.id);
    assert_eq!(core.conversations().len(), 2);
    assert_eq!(
        core.primary_workspace().unwrap().workspace.id,
        world.workspace.id
    );
    assert_eq!(core.display_name(world.other.id), "ZOE");
    assert_eq!(core.selected_conversation(), Some(world.channel.id));
    assert_eq!(core.connection(), ConnectionState::Connecting);

    assert!(driver.effects.contains(&Effect::Subscribe {
        workspace: world.workspace.id,
        after: world.head
    }));
    let fetches = driver.effects_matching(|e| match e {
        Effect::FetchHistory {
            conversation,
            before,
            generation,
            ..
        } => Some((*conversation, *before, *generation)),
        _ => None,
    });
    assert_eq!(fetches, vec![(world.channel.id, None, 1)]);
    assert_eq!(
        core.history_status(world.channel.id),
        &HistoryStatus::Loading
    );
}

#[test]
fn incompatible_protocol_versions_are_reported() {
    let world = World::new();
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        ..Default::default()
    }));
    let mut bootstrap = world.bootstrap();
    bootstrap.protocol_version += 1;
    driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut { last_error: Some(message) } if message.contains("Update")
    ));
}

#[test]
fn unreachable_server_offers_a_retry() {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        ..Default::default()
    }));
    driver.send(Input::BootstrapFailed(unknown_error()));
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::Unreachable { .. }
    ));
    let out = driver.send(Input::RetryLoad);
    assert_eq!(out.effects, vec![Effect::LoadBootstrap]);
    assert_eq!(driver.core.phase(), &SessionPhase::Loading);
}

// ----- Sending -----

#[test]
fn empty_text_creates_no_operation() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let out = driver.send(Input::SendMessage {
        conversation: world.channel.id,
        text: " \n ".into(),
    });
    assert!(out.is_empty());
    assert_eq!(driver.core.unsettled_operations(), 0);
}

#[test]
fn a_send_waits_for_the_journal_write() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(Input::SetDraft {
        conversation: world.channel.id,
        text: "hello".into(),
    });

    let out = driver.send(Input::SendMessage {
        conversation: world.channel.id,
        text: "hello".into(),
    });
    let [Effect::PersistSubmission(entry), ..] = &out.effects[..] else {
        panic!("expected the journal write first, got {:?}", out.effects);
    };
    let operation = entry.operation_id;
    assert_eq!(entry.user_id, world.me.id);
    assert!(
        sends(&driver).is_empty(),
        "no send before the journal write is durable"
    );

    let row = &driver.timeline(world.channel.id)[0];
    assert_eq!(row.key, ItemKey::Operation(operation));
    assert_eq!(row.delivery, Delivery::Pending);
    assert_eq!(
        driver.core.draft(world.channel.id),
        "",
        "an accepted send clears the draft"
    );

    driver.send(Input::SubmissionPersisted { operation });
    assert_eq!(sends(&driver), vec![operation]);
}

#[test]
fn failure_to_journal_fails_the_message_without_sending() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operation = compose(&mut driver, world.channel.id, "hello");
    driver.send(Input::SubmissionPersistFailed {
        operation,
        message: "disk full".into(),
    });
    assert!(sends(&driver).is_empty());
    assert!(matches!(
        &driver.timeline(world.channel.id)[0].delivery,
        Delivery::Failed { retry: RetryKind::NewOperation, reason } if reason.contains("disk full")
    ));
    assert_eq!(driver.core.unsettled_operations(), 0);
}

#[test]
fn acknowledgment_confirms_the_row_and_settles_once() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operation = compose_and_persist(&mut driver, world.channel.id, "hello");

    let message = world.message(world.channel.id, &world.me, 11, "hello");
    let response = SendMessageResponse {
        message: message.clone(),
    };
    driver.send(Input::SendResult {
        operation,
        result: Ok(response.clone()),
    });
    // A duplicate acknowledgment changes nothing.
    driver.send(Input::SendResult {
        operation,
        result: Ok(response),
    });

    let rows = driver.timeline(world.channel.id);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].key,
        ItemKey::Operation(operation),
        "identity survives confirmation"
    );
    assert_eq!(rows[0].message_id(), Some(message.id));
    assert_eq!(settles(&driver), vec![operation]);
    assert_eq!(driver.core.unsettled_operations(), 0);
}

#[test]
fn event_before_acknowledgment_yields_one_row() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operation = compose_and_persist(&mut driver, world.channel.id, "hello");

    let message = world.message(world.channel.id, &world.me, 11, "hello");
    driver.send(world.event(message.clone(), Some(operation)));
    driver.send(Input::SendResult {
        operation,
        result: Ok(SendMessageResponse { message }),
    });

    assert_eq!(driver.timeline(world.channel.id).len(), 1);
    assert_eq!(settles(&driver), vec![operation]);
}

#[test]
fn acknowledgment_before_event_yields_one_row() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operation = compose_and_persist(&mut driver, world.channel.id, "hello");

    let message = world.message(world.channel.id, &world.me, 11, "hello");
    driver.send(Input::SendResult {
        operation,
        result: Ok(SendMessageResponse {
            message: message.clone(),
        }),
    });
    driver.send(world.event(message, Some(operation)));

    assert_eq!(driver.timeline(world.channel.id).len(), 1);
    assert_eq!(settles(&driver), vec![operation]);
}

#[test]
fn confirmed_rows_sort_by_sequence_and_unconfirmed_rows_follow() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let first = compose_and_persist(&mut driver, world.channel.id, "first");
    let second = compose_and_persist(&mut driver, world.channel.id, "second");
    assert_eq!(
        sends(&driver),
        vec![first],
        "the second waits behind the first"
    );

    // The first send's outcome cannot be established, so it is given up on and the second goes.
    driver.send(Input::SendResult {
        operation: first,
        result: Err(unknown_error()),
    });
    driver.send(Input::OperationStatusResult {
        operation: first,
        result: Err(unknown_error()),
    });
    assert_eq!(sends(&driver), vec![first, second]);

    // Someone else's message arrives, then the second send is confirmed after it.
    driver.send(world.event(
        world.message(world.channel.id, &world.other, 11, "hi"),
        None,
    ));
    let second_message = world.message(world.channel.id, &world.me, 13, "second");
    driver.send(Input::SendResult {
        operation: second,
        result: Ok(SendMessageResponse {
            message: second_message,
        }),
    });

    let rows = driver.timeline(world.channel.id);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].seq(), Some(seq(11)));
    assert_eq!(rows[1].seq(), Some(seq(13)));
    assert_eq!(rows[1].key, ItemKey::Operation(second));
    assert_eq!(
        rows[2].key,
        ItemKey::Operation(first),
        "unconfirmed rows stay last"
    );
    assert!(!rows[2].is_confirmed());
}

#[test]
fn definitive_rejection_fails_with_a_new_operation_retry() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operation = compose_and_persist(&mut driver, world.channel.id, "hello");
    driver.send(Input::SendResult {
        operation,
        result: Err(rejected(ErrorCode::Forbidden, "You can't post here.")),
    });

    assert_eq!(
        driver.timeline(world.channel.id)[0].delivery,
        Delivery::Failed {
            reason: "You can't post here.".into(),
            retry: RetryKind::NewOperation
        }
    );
    assert_eq!(settles(&driver), vec![operation]);

    let out = driver.send(Input::RetryOperation {
        conversation: world.channel.id,
        operation,
    });
    let [Effect::PersistSubmission(entry)] = &out.effects[..] else {
        panic!("retry should start a new submission, got {:?}", out.effects);
    };
    assert_ne!(
        entry.operation_id, operation,
        "a definitive failure may use a fresh operation"
    );
    let rows = driver.timeline(world.channel.id);
    assert_eq!(rows.len(), 1, "the failed row is replaced");
    assert_eq!(rows[0].key, ItemKey::Operation(entry.operation_id));
}

#[test]
fn unknown_outcome_queries_status_and_never_makes_a_new_operation() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operation = compose_and_persist(&mut driver, world.channel.id, "hello");
    driver.send(Input::SendResult {
        operation,
        result: Err(unknown_error()),
    });

    assert_eq!(
        driver.timeline(world.channel.id)[0].delivery,
        Delivery::UnknownOutcome
    );
    assert_eq!(queries(&driver), vec![operation]);
    assert_eq!(
        sends(&driver).len(),
        1,
        "no resend until the server answers"
    );
    assert!(settles(&driver).is_empty());

    let message = world.message(world.channel.id, &world.me, 11, "hello");
    driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::Committed { message }),
    });
    assert!(driver.timeline(world.channel.id)[0].is_confirmed());
    assert_eq!(sends(&driver).len(), 1);
    assert_eq!(settles(&driver), vec![operation]);
}

#[test]
fn a_server_with_no_record_gets_the_same_operation_again() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operation = compose_and_persist(&mut driver, world.channel.id, "hello");
    driver.send(Input::SendResult {
        operation,
        result: Err(unknown_error()),
    });
    driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });

    assert_eq!(sends(&driver), vec![operation, operation]);
    assert_eq!(
        driver.timeline(world.channel.id)[0].delivery,
        Delivery::Pending
    );
    assert_eq!(driver.core.unsettled_operations(), 1);
}

#[test]
fn repeated_unknown_outcomes_stop_resending_and_wait_for_the_person() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operation = compose_and_persist(&mut driver, world.channel.id, "hello");

    // Each round: the send fails ambiguously and the server says it never saw it.
    for _ in 0..8 {
        driver.send(Input::SendResult {
            operation,
            result: Err(unknown_error()),
        });
        driver.send(Input::OperationStatusResult {
            operation,
            result: Ok(OperationStatus::NotFound),
        });
    }

    assert!(
        matches!(
            &driver.timeline(world.channel.id)[0].delivery,
            Delivery::Failed {
                retry: RetryKind::SameOperation,
                ..
            }
        ),
        "{:?}",
        driver.timeline(world.channel.id)[0].delivery
    );
    assert!(
        sends(&driver).iter().all(|id| *id == operation),
        "only ever the same operation"
    );
    assert!(sends(&driver).len() <= 1 + 3, "resends are bounded");
    assert!(
        settles(&driver).is_empty(),
        "the journal entry stays while the outcome is unknown"
    );

    // The person retries: the server is asked first.
    let before = queries(&driver).len();
    driver.send(Input::RetryOperation {
        conversation: world.channel.id,
        operation,
    });
    assert_eq!(queries(&driver).len(), before + 1);
    assert_eq!(
        driver.timeline(world.channel.id)[0].delivery,
        Delivery::UnknownOutcome
    );
}

#[test]
fn inability_to_check_status_leaves_a_retryable_failure() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operation = compose_and_persist(&mut driver, world.channel.id, "hello");
    driver.send(Input::SendResult {
        operation,
        result: Err(unknown_error()),
    });
    driver.send(Input::OperationStatusResult {
        operation,
        result: Err(unknown_error()),
    });
    assert!(matches!(
        &driver.timeline(world.channel.id)[0].delivery,
        Delivery::Failed {
            retry: RetryKind::SameOperation,
            ..
        }
    ));
    assert!(settles(&driver).is_empty());
}

#[test]
fn journal_recovery_checks_with_the_server_before_resending() {
    let world = World::new();
    let operation = OperationId::generate();
    let stranger_op = OperationId::generate();
    let entry = |op, user| JournalEntry {
        operation_id: op,
        user_id: user,
        conversation_id: world.channel.id,
        document: MessageDocument::from_plain_text("from last run").unwrap(),
        submitted_at: Timestamp::now(),
    };

    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        journal: vec![
            entry(operation, world.me.id),
            entry(stranger_op, world.other.id),
        ],
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));

    let rows = driver.timeline(world.channel.id);
    assert_eq!(rows.len(), 1, "other people's entries are not shown");
    assert_eq!(rows[0].key, ItemKey::Operation(operation));
    assert_eq!(rows[0].delivery, Delivery::UnknownOutcome);
    assert_eq!(queries(&driver), vec![operation]);
    assert!(sends(&driver).is_empty());

    // The server has it: the row is confirmed without any resend.
    let message = world.message(world.channel.id, &world.me, 11, "from last run");
    driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::Committed { message }),
    });
    assert!(driver.timeline(world.channel.id)[0].is_confirmed());
    assert_eq!(settles(&driver), vec![operation]);
}

#[test]
fn a_recovered_operation_the_server_never_saw_is_resent_with_its_id() {
    let world = World::new();
    let operation = OperationId::generate();
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        journal: vec![JournalEntry {
            operation_id: operation,
            user_id: world.me.id,
            conversation_id: world.channel.id,
            document: MessageDocument::from_plain_text("lost").unwrap(),
            submitted_at: Timestamp::now(),
        }],
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    assert_eq!(sends(&driver), vec![operation]);
}

#[test]
fn a_history_page_that_has_the_message_before_the_acknowledgment_does_not_duplicate_it() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operation = compose_and_persist(&mut driver, world.channel.id, "hello");
    let message = world.message(world.channel.id, &world.me, 11, "hello");

    // History (fetched concurrently) contains the committed message before the ack arrives.
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages: vec![message.clone()],
            has_more: false,
        }),
    });
    assert_eq!(driver.timeline(world.channel.id).len(), 2);

    driver.send(Input::SendResult {
        operation,
        result: Ok(SendMessageResponse { message }),
    });
    let rows = driver.timeline(world.channel.id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, ItemKey::Operation(operation));
}

// ----- Realtime and history -----

#[test]
fn events_are_applied_once_and_old_positions_are_ignored() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let message = world.message(world.channel.id, &world.other, 11, "hi");

    driver.send(world.event(message.clone(), None));
    driver.send(world.event(message, None));
    driver.send(world.event(
        world.message(world.channel.id, &world.other, 10, "old"),
        None,
    ));

    assert_eq!(driver.timeline(world.channel.id).len(), 1);
    // The saved cursor moves once per new position, never for duplicates or old positions.
    let cursors: Vec<_> = driver
        .caches()
        .iter()
        .map(|cache| cache.workspaces[0].cursor)
        .collect();
    assert_eq!(cursors, vec![seq(10), seq(11)]);
}

#[test]
fn caught_up_makes_the_connection_live_and_advances_the_cursor() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let workspace = world.workspace.id;

    driver.send(Input::Socket(SocketEvent::Opened { workspace }));
    assert_eq!(driver.core.connection(), ConnectionState::CatchingUp);
    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: workspace,
        seq: seq(15),
    }));
    assert_eq!(driver.core.connection(), ConnectionState::Live);
    assert_eq!(driver.last_cache().unwrap().workspaces[0].cursor, seq(15));
}

#[test]
fn connection_state_follows_socket_events() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let workspace = world.workspace.id;

    driver.send(Input::Socket(SocketEvent::Opened { workspace }));
    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: workspace,
        seq: seq(10),
    }));
    driver.send(Input::Socket(SocketEvent::Closed {
        workspace,
        attempt: 1,
    }));
    assert_eq!(
        driver.core.connection(),
        ConnectionState::Reconnecting { attempt: 1 }
    );
    driver.send(Input::Socket(SocketEvent::Connecting {
        workspace,
        attempt: 2,
    }));
    assert_eq!(
        driver.core.connection(),
        ConnectionState::Reconnecting { attempt: 2 }
    );
    driver.send(Input::Socket(SocketEvent::Opened { workspace }));
    assert_eq!(driver.core.connection(), ConnectionState::CatchingUp);
}

#[test]
fn snapshot_required_rebuilds_from_a_fresh_bootstrap() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(world.event(
        world.message(world.channel.id, &world.other, 11, "hi"),
        None,
    ));
    let pending = compose(&mut driver, world.channel.id, "still sending");
    assert_eq!(driver.timeline(world.channel.id).len(), 2);

    let out = driver.send(Input::Frame(ServerFrame::SnapshotRequired {
        workspace_id: world.workspace.id,
        reason: SnapshotReason::CursorAhead,
    }));
    // The saved cache is discarded along with the stale content.
    assert_eq!(
        out.effects,
        vec![Effect::LoadBootstrap, Effect::DiscardCache]
    );
    // A second request while one is in flight is not duplicated.
    let out = driver.send(Input::Frame(ServerFrame::SnapshotRequired {
        workspace_id: world.workspace.id,
        reason: SnapshotReason::CursorAhead,
    }));
    assert!(out.effects.is_empty());

    let mut bootstrap = world.bootstrap();
    bootstrap.workspaces[0].head = seq(40);
    let out = driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    assert!(out.effects.contains(&Effect::Subscribe {
        workspace: world.workspace.id,
        after: seq(40)
    }));
    assert!(out.changes.contains(&Change::TimelineReset {
        conversation: world.channel.id
    }));
    let rows = driver.timeline(world.channel.id);
    assert_eq!(
        rows.len(),
        1,
        "confirmed rows are dropped, rows being sent survive"
    );
    assert_eq!(rows[0].key, ItemKey::Operation(pending));
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::FetchHistory { .. }))
    );
}

#[test]
fn members_joining_update_names_and_unknown_authors_get_a_placeholder() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let newcomer = user("newbie");
    assert_eq!(driver.core.display_name(newcomer.id), UNKNOWN_MEMBER_NAME);

    driver.send(Input::Frame(ServerFrame::Event(EventEnvelope {
        workspace_id: world.workspace.id,
        seq: seq(11),
        occurred_at: Timestamp::now(),
        event: Event::MemberJoined {
            user: newcomer.clone(),
        },
    })));
    assert_eq!(driver.core.display_name(newcomer.id), "NEWBIE");
}

#[test]
fn history_results_go_to_their_own_conversation_and_stale_ones_are_dropped() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    // general's first fetch is generation 1. Switch away and back before it returns.
    driver.send(Input::SelectConversation(world.second_channel.id));
    let general_old = world.message(world.channel.id, &world.other, 5, "from general");

    // A late result for general arrives while random is selected: it lands in general.
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages: vec![general_old.clone()],
            has_more: true,
        }),
    });
    assert_eq!(
        message_ids(driver.timeline(world.channel.id)),
        vec![general_old.id]
    );
    assert!(driver.timeline(world.second_channel.id).is_empty());
    assert_eq!(
        driver.core.history_status(world.channel.id),
        &HistoryStatus::Ready { has_more: true }
    );

    // A response for an obsolete generation changes nothing.
    let out = driver.send(Input::HistoryLoaded {
        conversation: world.second_channel.id,
        generation: 99,
        result: Ok(HistoryPage {
            messages: vec![world.message(world.second_channel.id, &world.other, 6, "ghost")],
            has_more: false,
        }),
    });
    assert!(out.is_empty());
    assert!(driver.timeline(world.second_channel.id).is_empty());
    assert_eq!(
        driver.core.history_status(world.second_channel.id),
        &HistoryStatus::Loading
    );
}

#[test]
fn selecting_fetches_the_newest_page_only_once() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let fetches = |d: &Driver| {
        d.effects_matching(|e| match e {
            Effect::FetchHistory { conversation, .. } => Some(*conversation),
            _ => None,
        })
    };
    assert_eq!(fetches(&driver), vec![world.channel.id]);
    driver.send(Input::SelectConversation(world.channel.id));
    assert_eq!(fetches(&driver).len(), 1, "already loading");

    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages: vec![],
            has_more: false,
        }),
    });
    driver.send(Input::SelectConversation(world.second_channel.id));
    driver.send(Input::SelectConversation(world.channel.id));
    assert_eq!(
        fetches(&driver),
        vec![world.channel.id, world.second_channel.id]
    );
}

#[test]
fn older_history_prepends_as_one_splice_at_zero() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let newest: Vec<_> = (6..=8)
        .map(|i| world.message(world.channel.id, &world.other, i, "new"))
        .collect();
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages: newest.clone(),
            has_more: true,
        }),
    });

    let out = driver.send(Input::LoadOlder {
        conversation: world.channel.id,
    });
    let [
        Effect::FetchHistory {
            before, generation, ..
        },
    ] = &out.effects[..]
    else {
        panic!("expected one history fetch, got {:?}", out.effects);
    };
    assert_eq!(*before, Some(seq(6)));

    let older: Vec<_> = (1..=3)
        .map(|i| world.message(world.channel.id, &world.other, i, "old"))
        .collect();
    let out = driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: *generation,
        result: Ok(HistoryPage {
            messages: older.clone(),
            has_more: false,
        }),
    });
    assert!(out.changes.contains(&Change::TimelineSplice {
        conversation: world.channel.id,
        at: 0,
        removed: 0,
        inserted: 3
    }));
    let seqs: Vec<_> = driver
        .timeline(world.channel.id)
        .iter()
        .filter_map(|i| i.seq())
        .collect();
    assert_eq!(seqs, (1..=3).chain(6..=8).map(seq).collect::<Vec<_>>());
    assert_eq!(
        driver.core.history_status(world.channel.id),
        &HistoryStatus::Ready { has_more: false }
    );

    // Nothing more to load.
    assert!(
        driver
            .send(Input::LoadOlder {
                conversation: world.channel.id
            })
            .effects
            .is_empty()
    );
}

#[test]
fn history_overlapping_live_events_does_not_duplicate() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let live = world.message(world.channel.id, &world.other, 11, "live");
    driver.send(world.event(live.clone(), None));
    let older = world.message(world.channel.id, &world.other, 9, "older");
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages: vec![older.clone(), live.clone()],
            has_more: false,
        }),
    });
    assert_eq!(
        message_ids(driver.timeline(world.channel.id)),
        vec![older.id, live.id]
    );
}

#[test]
fn history_failure_is_shown_and_retried_on_demand() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Err(unknown_error()),
    });
    assert!(matches!(
        driver.core.history_status(world.channel.id),
        HistoryStatus::Failed { .. }
    ));
    let out = driver.send(Input::LoadOlder {
        conversation: world.channel.id,
    });
    assert!(matches!(
        &out.effects[..],
        [Effect::FetchHistory {
            before: None,
            generation: 2,
            ..
        }]
    ));
}

// ----- Session end, drafts -----

#[test]
fn an_expired_session_keeps_unfinished_sends_retryable() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operation = compose_and_persist(&mut driver, world.channel.id, "hello");
    driver.send(Input::SendResult {
        operation,
        result: Err(rejected(ErrorCode::Unauthenticated, "expired")),
    });

    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut {
            last_error: Some(_)
        }
    ));
    assert!(driver.effects.contains(&Effect::ForgetSession));
    assert!(matches!(
        &driver.timeline(world.channel.id)[0].delivery,
        Delivery::Failed {
            retry: RetryKind::SameOperation,
            ..
        }
    ));
    assert!(settles(&driver).is_empty());
    assert_eq!(driver.core.connection(), ConnectionState::Offline);
}

#[test]
fn signing_out_clears_everything_and_asks_the_runtime_to_wipe_local_data() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(Input::SetDraft {
        conversation: world.channel.id,
        text: "draft".into(),
    });
    compose(&mut driver, world.channel.id, "hello");

    let out = driver.send(Input::SignOut);
    assert!(out.effects.contains(&Effect::SignOut));
    assert_eq!(
        driver.core.phase(),
        &SessionPhase::SignedOut { last_error: None }
    );
    assert!(driver.core.me().is_none());
    assert!(driver.core.conversations().is_empty());
    assert_eq!(driver.core.draft(world.channel.id), "");
    assert_eq!(driver.core.unsettled_operations(), 0);
}

#[test]
fn drafts_are_saved_with_every_change_and_restored_at_startup() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let out = driver.send(Input::SetDraft {
        conversation: world.channel.id,
        text: "hel".into(),
    });
    assert_eq!(
        out.changes,
        vec![Change::DraftChanged {
            conversation: world.channel.id
        }]
    );
    assert_eq!(
        out.effects,
        vec![Effect::PersistDrafts(vec![(
            world.channel.id,
            "hel".into()
        )])]
    );
    // Setting the same text is a no-op.
    assert!(
        driver
            .send(Input::SetDraft {
                conversation: world.channel.id,
                text: "hel".into()
            })
            .is_empty()
    );

    let mut fresh = Driver::new();
    fresh.send(Input::Started(StartupState {
        drafts: [(world.channel.id, "restored".to_owned())].into(),
        ..Default::default()
    }));
    assert_eq!(fresh.core.draft(world.channel.id), "restored");
}

#[test]
fn too_long_messages_are_refused_with_a_notice_and_the_draft_is_kept() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let long = "x".repeat(lazy_domain::document::MAX_TEXT_BYTES + 1);
    driver.send(Input::SetDraft {
        conversation: world.channel.id,
        text: long.clone(),
    });
    let out = driver.send(Input::SendMessage {
        conversation: world.channel.id,
        text: long,
    });
    assert!(out.effects.is_empty());
    assert!(driver.core.notice().is_some());
    assert!(!driver.core.draft(world.channel.id).is_empty());
    driver.send(Input::DismissNotice);
    assert!(driver.core.notice().is_none());
}

#[test]
fn older_history_continues_from_the_loaded_window_not_from_a_stray_confirmed_row() {
    // A message sent before a refresh can be acknowledged at a position below the newest page.
    // Paging back from that row would skip the messages between it and the page.
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let page: Vec<_> = (17..=20)
        .map(|i| world.message(world.channel.id, &world.other, i, "page"))
        .collect();
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages: page,
            has_more: true,
        }),
    });

    let operation = compose_and_persist(&mut driver, world.channel.id, "late ack");
    let stray = world.message(world.channel.id, &world.me, 14, "late ack");
    driver.send(Input::SendResult {
        operation,
        result: Ok(SendMessageResponse { message: stray }),
    });
    assert_eq!(driver.timeline(world.channel.id)[0].seq(), Some(seq(14)));

    let out = driver.send(Input::LoadOlder {
        conversation: world.channel.id,
    });
    assert!(matches!(
        &out.effects[..],
        [Effect::FetchHistory { before: Some(before), .. }] if *before == seq(17)
    ));
}

// ----- Send queue: one message in flight per conversation, in submission order -----

fn ack(driver: &mut Driver, world: &World, operation: OperationId, at: i64, text: &str) {
    let message = world.message(world.channel.id, &world.me, at, text);
    driver.send(Input::SendResult {
        operation,
        result: Ok(SendMessageResponse { message }),
    });
}

fn row_keys(driver: &Driver, conversation: ConversationId) -> Vec<ItemKey> {
    driver
        .timeline(conversation)
        .iter()
        .map(|row| row.key)
        .collect()
}

#[test]
fn twenty_rapid_sends_go_out_one_at_a_time_in_submission_order() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let channel = world.channel.id;
    let operations: Vec<OperationId> = (0..20)
        .map(|i| compose(&mut driver, channel, &format!("message {i:03}")))
        .collect();
    assert!(
        sends(&driver).is_empty(),
        "nothing is sent before it is journaled"
    );

    // Every journal write completes at once, as it would when the person types quickly.
    for operation in &operations {
        driver.send(Input::SubmissionPersisted {
            operation: *operation,
        });
    }
    assert_eq!(
        sends(&driver),
        vec![operations[0]],
        "only the first is in flight"
    );

    let expected_rows: Vec<ItemKey> = operations
        .iter()
        .map(|op| ItemKey::Operation(*op))
        .collect();
    for (index, operation) in operations.iter().enumerate() {
        ack(&mut driver, &world, *operation, 11 + index as i64, "x");
        let released = (index + 2).min(operations.len());
        assert_eq!(
            sends(&driver),
            operations[..released].to_vec(),
            "after acknowledging message {index} exactly the next one is released"
        );
        assert_eq!(
            row_keys(&driver, channel),
            expected_rows,
            "no row moves when its message is confirmed"
        );
    }
    let seqs: Vec<_> = driver
        .timeline(channel)
        .iter()
        .filter_map(|row| row.seq())
        .collect();
    assert_eq!(seqs, (11..31).map(seq).collect::<Vec<_>>());
    assert_eq!(settles(&driver), operations);
    assert_eq!(driver.core.unsettled_operations(), 0);
}

#[test]
fn a_later_message_never_overtakes_an_earlier_one_whose_journal_write_is_slower() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let first = compose(&mut driver, world.channel.id, "first");
    let second = compose(&mut driver, world.channel.id, "second");

    driver.send(Input::SubmissionPersisted { operation: second });
    assert!(
        sends(&driver).is_empty(),
        "the second must wait for the first"
    );
    driver.send(Input::SubmissionPersisted { operation: first });
    assert_eq!(sends(&driver), vec![first]);
    ack(&mut driver, &world, first, 11, "first");
    assert_eq!(sends(&driver), vec![first, second]);
}

#[test]
fn queues_of_different_conversations_do_not_wait_for_each_other() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let general = compose_and_persist(&mut driver, world.channel.id, "a");
    let random = compose_and_persist(&mut driver, world.second_channel.id, "b");
    assert_eq!(sends(&driver), vec![general, random]);
}

#[test]
fn an_earlier_definitive_failure_does_not_block_later_messages() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let operations: Vec<_> = (0..3)
        .map(|i| compose_and_persist(&mut driver, world.channel.id, &format!("m{i}")))
        .collect();
    assert_eq!(sends(&driver), vec![operations[0]]);

    driver.send(Input::SendResult {
        operation: operations[0],
        result: Err(rejected(ErrorCode::Forbidden, "You can't post here.")),
    });
    assert_eq!(
        sends(&driver),
        vec![operations[0], operations[1]],
        "the failure releases the next message, and only the next"
    );
    ack(&mut driver, &world, operations[1], 11, "m1");
    assert_eq!(sends(&driver), operations);
    ack(&mut driver, &world, operations[2], 12, "m2");

    let rows = driver.timeline(world.channel.id);
    assert!(matches!(rows[2].delivery, Delivery::Failed { .. }));
    assert_eq!(rows[0].key, ItemKey::Operation(operations[1]));
    assert_eq!(rows[1].key, ItemKey::Operation(operations[2]));
}

#[test]
fn a_journal_failure_does_not_block_later_messages_either() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let first = compose(&mut driver, world.channel.id, "first");
    let second = compose_and_persist(&mut driver, world.channel.id, "second");
    assert!(sends(&driver).is_empty());
    driver.send(Input::SubmissionPersistFailed {
        operation: first,
        message: "disk full".into(),
    });
    assert_eq!(sends(&driver), vec![second]);
}

#[test]
fn an_earlier_unknown_outcome_blocks_later_messages_until_it_is_resolved() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let first = compose_and_persist(&mut driver, world.channel.id, "first");
    let second = compose_and_persist(&mut driver, world.channel.id, "second");
    let third = compose_and_persist(&mut driver, world.channel.id, "third");

    // The first send reports nothing useful, so the client asks the server about it.
    driver.send(Input::SendResult {
        operation: first,
        result: Err(unknown_error()),
    });
    assert_eq!(queries(&driver), vec![first]);
    assert_eq!(sends(&driver), vec![first], "the others keep waiting");

    // The server never saw it: it is resent first, and the others still wait.
    driver.send(Input::OperationStatusResult {
        operation: first,
        result: Ok(OperationStatus::NotFound),
    });
    assert_eq!(sends(&driver), vec![first, first]);

    // Still no answer: another status check, still holding the queue.
    driver.send(Input::SendResult {
        operation: first,
        result: Err(unknown_error()),
    });
    assert_eq!(queries(&driver), vec![first, first]);
    assert_eq!(sends(&driver), vec![first, first]);

    // It turns out the earlier attempt did commit. Now the second is released, then the third.
    let committed = world.message(world.channel.id, &world.me, 11, "first");
    driver.send(Input::OperationStatusResult {
        operation: first,
        result: Ok(OperationStatus::Committed { message: committed }),
    });
    assert_eq!(sends(&driver), vec![first, first, second]);
    ack(&mut driver, &world, second, 12, "second");
    assert_eq!(sends(&driver), vec![first, first, second, third]);
}

#[test]
fn giving_up_on_an_unknown_outcome_releases_the_queue() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let first = compose_and_persist(&mut driver, world.channel.id, "first");
    let second = compose_and_persist(&mut driver, world.channel.id, "second");
    driver.send(Input::SendResult {
        operation: first,
        result: Err(unknown_error()),
    });
    assert_eq!(sends(&driver), vec![first]);
    driver.send(Input::OperationStatusResult {
        operation: first,
        result: Err(unknown_error()),
    });
    assert_eq!(
        sends(&driver),
        vec![first, second],
        "a failed message waiting for the person does not hold up the rest"
    );
    assert!(settles(&driver).is_empty(), "its journal entry is kept");
}

#[test]
fn journal_recovered_operations_go_first_in_the_order_they_were_submitted() {
    let world = World::new();
    let earlier =
        Timestamp::from(time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap());
    let later = Timestamp::from(time::OffsetDateTime::from_unix_timestamp(1_800_000_060).unwrap());
    let entry = |text: &str, at| JournalEntry {
        operation_id: OperationId::generate(),
        user_id: world.me.id,
        conversation_id: world.channel.id,
        document: MessageDocument::from_plain_text(text).unwrap(),
        submitted_at: at,
    };
    let first = entry("submitted first", earlier);
    let second = entry("submitted second", later);

    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        // The file lists them out of order; submission time decides.
        journal: vec![second.clone(), first.clone()],
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    assert_eq!(
        queries(&driver).len(),
        2,
        "both are checked before anything is resent"
    );

    // A new message typed right after startup waits behind the recovered ones.
    let fresh = compose_and_persist(&mut driver, world.channel.id, "typed after startup");
    assert!(sends(&driver).is_empty());

    // The later recovered one is found missing first, but it cannot jump the queue.
    driver.send(Input::OperationStatusResult {
        operation: second.operation_id,
        result: Ok(OperationStatus::NotFound),
    });
    assert!(sends(&driver).is_empty());
    driver.send(Input::OperationStatusResult {
        operation: first.operation_id,
        result: Ok(OperationStatus::NotFound),
    });
    assert_eq!(sends(&driver), vec![first.operation_id]);
    ack(
        &mut driver,
        &world,
        first.operation_id,
        11,
        "submitted first",
    );
    assert_eq!(
        sends(&driver),
        vec![first.operation_id, second.operation_id]
    );
    ack(
        &mut driver,
        &world,
        second.operation_id,
        12,
        "submitted second",
    );
    assert_eq!(
        sends(&driver),
        vec![first.operation_id, second.operation_id, fresh]
    );
}

#[test]
fn retrying_an_unresolved_failure_rejoins_at_the_back_of_the_queue() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let first = compose_and_persist(&mut driver, world.channel.id, "first");
    let second = compose_and_persist(&mut driver, world.channel.id, "second");
    driver.send(Input::SendResult {
        operation: first,
        result: Err(unknown_error()),
    });
    driver.send(Input::OperationStatusResult {
        operation: first,
        result: Err(unknown_error()),
    });
    assert_eq!(sends(&driver), vec![first, second]);

    // The person retries the first while the second is still in flight.
    driver.send(Input::RetryOperation {
        conversation: world.channel.id,
        operation: first,
    });
    assert_eq!(
        row_keys(&driver, world.channel.id),
        vec![ItemKey::Operation(second), ItemKey::Operation(first)],
        "the row moves behind the message that was sent in the meantime"
    );
    driver.send(Input::OperationStatusResult {
        operation: first,
        result: Ok(OperationStatus::NotFound),
    });
    assert_eq!(
        sends(&driver),
        vec![first, second],
        "it waits for the second to finish"
    );
    ack(&mut driver, &world, second, 11, "second");
    assert_eq!(sends(&driver), vec![first, second, first]);
}

#[test]
fn retrying_a_rejected_message_queues_a_new_operation_behind_pending_ones() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let first = compose_and_persist(&mut driver, world.channel.id, "first");
    let second = compose_and_persist(&mut driver, world.channel.id, "second");
    driver.send(Input::SendResult {
        operation: first,
        result: Err(rejected(ErrorCode::Forbidden, "no")),
    });
    assert_eq!(sends(&driver), vec![first, second]);

    let out = driver.send(Input::RetryOperation {
        conversation: world.channel.id,
        operation: first,
    });
    let [Effect::PersistSubmission(entry)] = &out.effects[..] else {
        panic!("expected a new submission, got {:?}", out.effects);
    };
    let retried = entry.operation_id;
    driver.send(Input::SubmissionPersisted { operation: retried });
    assert_eq!(
        sends(&driver),
        vec![first, second],
        "behind the second, which is in flight"
    );
    ack(&mut driver, &world, second, 11, "second");
    assert_eq!(sends(&driver), vec![first, second, retried]);
}

#[test]
fn an_expired_session_does_not_release_queued_messages() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let first = compose_and_persist(&mut driver, world.channel.id, "first");
    let second = compose_and_persist(&mut driver, world.channel.id, "second");
    driver.send(Input::SendResult {
        operation: first,
        result: Err(rejected(ErrorCode::Unauthenticated, "expired")),
    });
    assert_eq!(
        sends(&driver),
        vec![first],
        "the second is not sent into a dead session"
    );
    for operation in [first, second] {
        let row = driver
            .timeline(world.channel.id)
            .iter()
            .find(|row| row.key == ItemKey::Operation(operation))
            .unwrap();
        assert!(matches!(
            row.delivery,
            Delivery::Failed {
                retry: RetryKind::SameOperation,
                ..
            }
        ));
    }
}

#[test]
fn queued_messages_wait_for_the_session_to_be_ready_again() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let first = compose_and_persist(&mut driver, world.channel.id, "first");
    let second = compose_and_persist(&mut driver, world.channel.id, "second");
    // A refresh fails while the first is in flight; the app is not ready until it succeeds.
    driver.send(Input::Frame(ServerFrame::SnapshotRequired {
        workspace_id: world.workspace.id,
        reason: SnapshotReason::CursorExpired,
    }));
    driver.send(Input::BootstrapFailed(unknown_error()));
    ack(&mut driver, &world, first, 11, "first");
    assert_eq!(
        sends(&driver),
        vec![first],
        "nothing is sent while unreachable"
    );

    driver.send(Input::RetryLoad);
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    assert_eq!(sends(&driver), vec![first, second]);
}
