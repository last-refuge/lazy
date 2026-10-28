//! Management actions: create, open, browse, join, leave, and the rest. States, validation,
//! readable errors, and the unknown-outcome rules for the two actions that create things.

mod common;

use common::{Driver, World, channel, people, read_state, seq, user};
use lazy_client_core::{
    ActionKind, ActionOutcome, ActionState, Change, Effect, Input, RequestError, SessionPhase,
};
use lazy_domain::{ChannelVisibility, ConversationId, OperationId};
use lazy_protocol::{ApiError, Conversation, ConversationResponse, ErrorCode, OperationStatus};

fn rejected(code: ErrorCode, message: &str) -> RequestError {
    RequestError::Rejected(ApiError::new(code, message))
}

fn unknown() -> RequestError {
    RequestError::Unknown("timed out".into())
}

fn failed(driver: &Driver, kind: ActionKind) -> String {
    match driver.core.action(kind) {
        ActionState::Failed(message) => message.clone(),
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn create_input(name: &str) -> Input {
    Input::CreateChannel {
        name: name.into(),
        visibility: ChannelVisibility::Public,
        topic: String::new(),
        description: String::new(),
    }
}

/// The create effects emitted so far, with their operation ids.
fn creates(driver: &Driver) -> Vec<(OperationId, String)> {
    driver.effects_matching(|e| match e {
        Effect::CreateChannel { request, .. } => {
            Some((request.operation_id, request.name.to_string()))
        }
        _ => None,
    })
}

fn opens(driver: &Driver) -> Vec<OperationId> {
    driver.effects_matching(|e| match e {
        Effect::OpenConversation { request, .. } => Some(request.operation_id),
        _ => None,
    })
}

fn queries(driver: &Driver) -> Vec<OperationId> {
    driver.effects_matching(|e| match e {
        Effect::QueryOperationStatus { operation } => Some(*operation),
        _ => None,
    })
}

fn created_channel(world: &World, name: &str) -> Conversation {
    Conversation {
        created_by: Some(world.me.id),
        member_count: 1,
        ..channel(world.workspace.id, name)
    }
}

// ----- Create a channel -----

#[test]
fn creating_a_channel_normalizes_the_name_and_opens_it_on_success() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let kind = ActionKind::CreateChannel;

    let out = driver.send(Input::CreateChannel {
        name: "  #Design Team ".into(),
        visibility: ChannelVisibility::Private,
        topic: "  pixels  ".into(),
        description: " ".into(),
    });
    assert_eq!(*driver.core.action(kind), ActionState::InFlight);
    assert!(out.changes.contains(&Change::ActionChanged(kind)));
    let Some(Effect::CreateChannel { workspace, request }) = out.effects.first().cloned() else {
        panic!("expected a create request, got {:?}", out.effects);
    };
    assert_eq!(workspace, world.workspace.id);
    assert_eq!(request.name.as_str(), "design-team");
    assert_eq!(request.visibility, ChannelVisibility::Private);
    assert_eq!(request.topic.as_deref(), Some("pixels"));
    assert_eq!(request.description, None, "blank means none");

    // A second click while it is out does nothing.
    assert!(driver.send(create_input("design-team")).effects.is_empty());

    let created = created_channel(&world, "design-team");
    let out = driver.send(Input::ConversationCommandResult {
        operation: request.operation_id,
        result: Ok(ConversationResponse {
            conversation: created.clone(),
        }),
    });
    assert_eq!(*driver.core.action(kind), ActionState::Idle);
    assert_eq!(driver.core.selected_conversation(), Some(created.id));
    assert!(
        out.changes
            .contains(&Change::ConversationOpened(created.id))
    );
    assert!(out.changes.contains(&Change::SelectionChanged));
    assert!(driver.core.channels().contains(&created.id));
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::FetchHistory { conversation, .. } if *conversation == created.id)),
        "the new channel's page is requested"
    );
    // The creator's own event arriving afterwards is not a second channel.
    driver.send(world.event_at(
        11,
        lazy_protocol::Event::ConversationCreated {
            conversation: created.clone(),
        },
    ));
    assert_eq!(
        driver
            .core
            .conversations()
            .iter()
            .filter(|c| c.id == created.id)
            .count(),
        1
    );
}

#[test]
fn creating_a_channel_checks_the_input_before_asking_the_server() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let kind = ActionKind::CreateChannel;

    let out = driver.send(create_input("   "));
    assert!(out.effects.is_empty());
    assert_eq!(failed(&driver, kind), "Give the channel a name.");

    driver.send(create_input("bad/name"));
    assert!(failed(&driver, kind).contains("letters, numbers, hyphens"));

    driver.send(create_input("-leading"));
    assert!(failed(&driver, kind).contains("start with a letter or number"));

    driver.send(create_input(&"x".repeat(81)));
    assert!(failed(&driver, kind).contains("at most 80"));

    let out = driver.send(Input::CreateChannel {
        name: "fine".into(),
        visibility: ChannelVisibility::Public,
        topic: "x".repeat(251),
        description: String::new(),
    });
    assert!(out.effects.is_empty());
    assert!(failed(&driver, kind).contains("250"));

    let out = driver.send(Input::CreateChannel {
        name: "fine".into(),
        visibility: ChannelVisibility::Public,
        topic: String::new(),
        description: "x".repeat(1001),
    });
    assert!(out.effects.is_empty());
    assert!(failed(&driver, kind).contains("1,000"));
    assert!(creates(&driver).is_empty());
}

#[test]
fn a_taken_name_is_a_readable_error_that_a_new_name_replaces() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let kind = ActionKind::CreateChannel;
    driver.send(create_input("general2"));
    let (first, _) = creates(&driver)[0].clone();
    let out = driver.send(Input::ConversationCommandResult {
        operation: first,
        result: Err(rejected(ErrorCode::NameTaken, "name_taken")),
    });
    assert_eq!(
        failed(&driver, kind),
        "That name is already taken. Choose another."
    );
    assert!(out.changes.contains(&Change::ActionChanged(kind)));
    assert!(
        queries(&driver).is_empty(),
        "a refusal needs no reconciling"
    );

    // Dismissing clears it; trying a different name is a fresh operation.
    driver.send(Input::DismissAction(kind));
    assert_eq!(*driver.core.action(kind), ActionState::Idle);
    driver.send(create_input("general3"));
    let sent = creates(&driver);
    assert_eq!(sent.len(), 2);
    assert_ne!(sent[0].0, sent[1].0);
    assert_eq!(*driver.core.action(kind), ActionState::InFlight);
}

#[test]
fn an_unknown_outcome_is_checked_with_the_server_and_never_created_again_blindly() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let kind = ActionKind::CreateChannel;
    driver.send(create_input("design"));
    let (operation, _) = creates(&driver)[0].clone();

    // The answer was lost: ask, do not repeat.
    driver.send(Input::ConversationCommandResult {
        operation,
        result: Err(unknown()),
    });
    assert_eq!(queries(&driver), vec![operation]);
    assert_eq!(creates(&driver).len(), 1);
    assert_eq!(
        *driver.core.action(kind),
        ActionState::InFlight,
        "still working on it"
    );

    // It had been created after all.
    let created = created_channel(&world, "design");
    driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::ConversationCommitted {
            conversation: created.clone(),
        }),
    });
    assert_eq!(*driver.core.action(kind), ActionState::Idle);
    assert_eq!(driver.core.selected_conversation(), Some(created.id));
    assert_eq!(creates(&driver).len(), 1);
}

#[test]
fn when_the_server_has_no_record_the_same_operation_is_sent_again() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(create_input("design"));
    let (operation, _) = creates(&driver)[0].clone();
    driver.send(Input::ConversationCommandResult {
        operation,
        result: Err(unknown()),
    });
    driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    let sent = creates(&driver);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].0, operation, "the same operation, never a new one");

    // After that the answer is a definitive refusal: done, nothing left to reconcile.
    driver.send(Input::ConversationCommandResult {
        operation,
        result: Err(rejected(ErrorCode::Forbidden, "no")),
    });
    assert_eq!(
        failed(&driver, ActionKind::CreateChannel),
        "You're not allowed to do that."
    );
}

#[test]
fn giving_up_keeps_the_operation_so_trying_again_asks_first() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let kind = ActionKind::CreateChannel;
    driver.send(create_input("design"));
    let (operation, _) = creates(&driver)[0].clone();
    driver.send(Input::ConversationCommandResult {
        operation,
        result: Err(unknown()),
    });
    // The server keeps saying it never heard of it, and each resend is lost.
    for _ in 0..8 {
        driver.send(Input::OperationStatusResult {
            operation,
            result: Ok(OperationStatus::NotFound),
        });
        driver.send(Input::ConversationCommandResult {
            operation,
            result: Err(unknown()),
        });
    }
    assert!(matches!(driver.core.action(kind), ActionState::Failed(m) if m.contains("Try again")));
    let sent = creates(&driver);
    assert!(sent.len() <= 4, "resends are bounded: {}", sent.len());
    assert!(sent.iter().all(|(op, _)| *op == operation));

    // Trying again with the same content continues that operation, asking before sending.
    let before = (sent.len(), queries(&driver).len());
    driver.send(create_input("design"));
    assert_eq!(creates(&driver).len(), before.0, "no blind second create");
    assert_eq!(queries(&driver).len(), before.1 + 1);
    assert_eq!(*queries(&driver).last().unwrap(), operation);
    assert_eq!(*driver.core.action(kind), ActionState::InFlight);

    // Different content is a different thing to create: a new operation.
    driver.send(Input::ConversationCommandResult {
        operation,
        result: Err(unknown()),
    });
    driver.send(Input::OperationStatusResult {
        operation,
        result: Err(unknown()),
    });
    assert!(matches!(driver.core.action(kind), ActionState::Failed(_)));
    driver.send(create_input("something-else"));
    let sent = creates(&driver);
    assert_ne!(sent.last().unwrap().0, operation);
}

#[test]
fn an_expired_session_ends_the_session_not_the_dialog() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(create_input("design"));
    let (operation, _) = creates(&driver)[0].clone();
    driver.send(Input::ConversationCommandResult {
        operation,
        result: Err(rejected(ErrorCode::Unauthenticated, "expired")),
    });
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut { .. }
    ));
    assert_eq!(
        *driver.core.action(ActionKind::CreateChannel),
        ActionState::Idle
    );
    assert!(driver.effects.contains(&Effect::ForgetSession));
}

// ----- Open a conversation -----

#[test]
fn opening_a_conversation_names_only_the_other_people_and_selects_it() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let kind = ActionKind::OpenConversation;

    // Naming myself, or nobody, is not a conversation.
    let out = driver.send(Input::OpenConversation {
        participants: vec![],
    });
    assert!(out.effects.is_empty());
    assert_eq!(failed(&driver, kind), "Choose at least one other person.");
    driver.send(Input::OpenConversation {
        participants: vec![world.me.id],
    });
    assert_eq!(failed(&driver, kind), "Choose at least one other person.");
    let crowd: Vec<_> = (0..9).map(|i| user(&format!("crowd{i}")).id).collect();
    driver.send(Input::OpenConversation {
        participants: crowd,
    });
    assert!(failed(&driver, kind).contains("at most 9"));
    assert!(opens(&driver).is_empty());

    // Order and duplicates do not matter; the request lists the others.
    let kai = user("kai");
    let out = driver.send(Input::OpenConversation {
        participants: vec![world.other.id, kai.id, world.other.id, world.me.id],
    });
    let Some(Effect::OpenConversation { request, .. }) = out.effects.first().cloned() else {
        panic!("expected an open request");
    };
    let mut expected = vec![world.other.id, kai.id];
    expected.sort();
    assert_eq!(request.participants, expected);

    let opened = people(world.workspace.id, &[&world.me, &world.other, &kai]);
    let out = driver.send(Input::ConversationCommandResult {
        operation: request.operation_id,
        result: Ok(ConversationResponse {
            conversation: opened.clone(),
        }),
    });
    assert_eq!(driver.core.selected_conversation(), Some(opened.id));
    assert!(out.changes.contains(&Change::ConversationOpened(opened.id)));
    assert_eq!(*driver.core.action(kind), ActionState::Idle);
}

#[test]
fn opening_a_conversation_that_already_exists_just_selects_it() {
    let mut world = World::new();
    let dm = people(world.workspace.id, &[&world.me, &world.other]);
    world.extra = vec![dm.clone()];
    let mut driver = Driver::ready(&world);
    assert_eq!(driver.core.selected_conversation(), Some(world.channel.id));
    driver.send(Input::OpenConversation {
        participants: vec![world.other.id],
    });
    let operation = opens(&driver)[0];
    driver.send(Input::ConversationCommandResult {
        operation,
        result: Ok(ConversationResponse {
            conversation: dm.clone(),
        }),
    });
    assert_eq!(driver.core.selected_conversation(), Some(dm.id));
    assert_eq!(
        driver
            .core
            .conversations()
            .iter()
            .filter(|c| c.id == dm.id)
            .count(),
        1
    );
}

#[test]
fn opening_reconciles_an_unknown_outcome_like_creating() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(Input::OpenConversation {
        participants: vec![world.other.id],
    });
    let operation = opens(&driver)[0];
    driver.send(Input::ConversationCommandResult {
        operation,
        result: Err(unknown()),
    });
    assert_eq!(queries(&driver), vec![operation]);
    assert_eq!(opens(&driver).len(), 1, "not repeated blindly");
    driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    assert_eq!(opens(&driver), vec![operation, operation]);
    let dm = people(world.workspace.id, &[&world.me, &world.other]);
    driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::ConversationCommitted {
            conversation: dm.clone(),
        }),
    });
    assert_eq!(driver.core.selected_conversation(), Some(dm.id));
}

// ----- The rest -----

fn ready_with_alpha() -> (World, Driver, Conversation) {
    let mut world = World::new();
    let alpha = Conversation {
        created_by: Some(world.me.id),
        ..channel(world.workspace.id, "alpha")
    };
    world.extra = vec![alpha.clone()];
    let driver = Driver::ready(&world);
    (world, driver, alpha)
}

#[test]
fn browsing_reports_progress_and_failures() {
    let (_, mut driver, _) = ready_with_alpha();
    let kind = ActionKind::Browse;
    let out = driver.send(Input::BrowseChannels);
    assert!(matches!(&out.effects[..], [Effect::BrowseChannels { .. }]));
    assert_eq!(*driver.core.action(kind), ActionState::InFlight);
    assert!(
        driver.send(Input::BrowseChannels).effects.is_empty(),
        "already loading"
    );
    driver.send(Input::ActionResult {
        kind,
        result: Err(unknown()),
    });
    assert!(failed(&driver, kind).contains("Couldn't reach the server"));
    assert!(driver.core.browse_results().is_empty());
}

#[test]
fn joining_adds_the_channel_and_fetches_its_page() {
    let (world, mut driver, _) = ready_with_alpha();
    let open = channel(world.workspace.id, "open");
    let kind = ActionKind::Join(open.id);
    let out = driver.send(Input::JoinChannel(open.id));
    assert!(out.effects.contains(&Effect::JoinChannel {
        conversation: open.id
    }));
    assert_eq!(*driver.core.action(kind), ActionState::InFlight);

    let out = driver.send(Input::ActionResult {
        kind,
        result: Ok(ActionOutcome::Conversation(open.clone())),
    });
    assert_eq!(*driver.core.action(kind), ActionState::Idle);
    assert!(driver.core.channels().contains(&open.id));
    assert!(out.effects.iter().any(
        |e| matches!(e, Effect::FetchHistory { conversation, .. } if *conversation == open.id)
    ));
    assert_eq!(
        driver.core.selected_conversation(),
        Some(world.channel.id),
        "joining does not navigate"
    );
    assert_eq!(driver.core.user_state(open.id).unwrap().unread_count, 0);
}

#[test]
fn a_refusal_is_explained_and_the_dialog_can_try_again() {
    let (world, mut driver, _) = ready_with_alpha();
    let secret = channel(world.workspace.id, "secret");
    let kind = ActionKind::Join(secret.id);
    driver.send(Input::JoinChannel(secret.id));
    driver.send(Input::ActionResult {
        kind,
        result: Err(rejected(ErrorCode::Forbidden, "private")),
    });
    assert_eq!(failed(&driver, kind), "You're not allowed to do that.");
    // Pressing the button again starts over.
    let out = driver.send(Input::JoinChannel(secret.id));
    assert!(out.effects.contains(&Effect::JoinChannel {
        conversation: secret.id
    }));
    assert_eq!(*driver.core.action(kind), ActionState::InFlight);
}

#[test]
fn results_for_actions_that_are_not_in_flight_are_ignored() {
    let (world, mut driver, alpha) = ready_with_alpha();
    let kind = ActionKind::Archive(alpha.id);
    let archived = Conversation {
        archived: true,
        ..alpha.clone()
    };
    let out = driver.send(Input::ActionResult {
        kind,
        result: Ok(ActionOutcome::Conversation(archived)),
    });
    assert!(out.is_empty());
    assert!(!driver.core.conversation(alpha.id).unwrap().archived);
    let _ = world;
}

#[test]
fn editing_a_channel_sends_only_what_changed() {
    let (_, mut driver, alpha) = ready_with_alpha();
    let kind = ActionKind::UpdateConversation(alpha.id);

    // Checked before sending.
    let out = driver.send(Input::UpdateConversation {
        id: alpha.id,
        name: Some("not valid!".into()),
        topic: None,
        description: None,
    });
    assert!(out.effects.is_empty());
    assert!(failed(&driver, kind).contains("letters, numbers"));

    let out = driver.send(Input::UpdateConversation {
        id: alpha.id,
        name: Some("Omega".into()),
        topic: Some(String::new()),
        description: None,
    });
    let Some(Effect::UpdateConversation {
        conversation,
        request,
    }) = out.effects.first().cloned()
    else {
        panic!("expected an update request");
    };
    assert_eq!(conversation, alpha.id);
    assert_eq!(request.name.unwrap().as_str(), "omega");
    assert_eq!(
        request.topic.as_deref(),
        Some(""),
        "an empty topic clears it"
    );
    assert_eq!(request.description, None, "absent means unchanged");

    let renamed = Conversation {
        name: Some(lazy_domain::ChannelName::parse("omega").unwrap()),
        ..alpha.clone()
    };
    driver.send(Input::ActionResult {
        kind,
        result: Ok(ActionOutcome::Conversation(renamed)),
    });
    assert_eq!(driver.core.conversation_title(alpha.id), "omega");
    assert_eq!(*driver.core.action(kind), ActionState::Idle);
}

#[test]
fn renaming_to_a_name_in_use_says_so() {
    let (_, mut driver, alpha) = ready_with_alpha();
    let kind = ActionKind::UpdateConversation(alpha.id);
    driver.send(Input::UpdateConversation {
        id: alpha.id,
        name: Some("general".into()),
        topic: None,
        description: None,
    });
    driver.send(Input::ActionResult {
        kind,
        result: Err(rejected(ErrorCode::NameTaken, "taken")),
    });
    assert_eq!(
        failed(&driver, kind),
        "That name is already taken. Choose another."
    );
    assert_eq!(
        driver.core.conversation_title(alpha.id),
        "alpha",
        "nothing changed"
    );
}

#[test]
fn archiving_and_unarchiving_update_the_conversation() {
    let (_, mut driver, alpha) = ready_with_alpha();
    let archived = Conversation {
        archived: true,
        ..alpha.clone()
    };
    let out = driver.send(Input::ArchiveConversation(alpha.id));
    assert!(out.effects.contains(&Effect::ArchiveConversation {
        conversation: alpha.id
    }));
    driver.send(Input::ActionResult {
        kind: ActionKind::Archive(alpha.id),
        result: Ok(ActionOutcome::Conversation(archived)),
    });
    assert!(driver.core.conversation(alpha.id).unwrap().archived);
    assert!(driver.core.can_unarchive(alpha.id));

    let out = driver.send(Input::UnarchiveConversation(alpha.id));
    assert!(out.effects.contains(&Effect::UnarchiveConversation {
        conversation: alpha.id
    }));
    driver.send(Input::ActionResult {
        kind: ActionKind::Unarchive(alpha.id),
        result: Ok(ActionOutcome::Conversation(alpha.clone())),
    });
    assert!(!driver.core.conversation(alpha.id).unwrap().archived);
}

#[test]
fn adding_and_removing_people() {
    let (world, mut driver, alpha) = ready_with_alpha();
    let kai = user("kai");
    let add = ActionKind::AddMembers(alpha.id);

    let out = driver.send(Input::AddMembers {
        id: alpha.id,
        users: vec![],
    });
    assert!(out.effects.is_empty());
    assert_eq!(failed(&driver, add), "Choose at least one person to add.");

    let out = driver.send(Input::AddMembers {
        id: alpha.id,
        users: vec![kai.id, world.other.id],
    });
    let Some(Effect::AddMembers {
        conversation,
        request,
    }) = out.effects.first().cloned()
    else {
        panic!("expected an add request");
    };
    assert_eq!(
        (conversation, request.user_ids),
        (alpha.id, vec![kai.id, world.other.id])
    );
    let grown = Conversation {
        member_count: 3,
        ..alpha.clone()
    };
    driver.send(Input::ActionResult {
        kind: add,
        result: Ok(ActionOutcome::Conversation(grown)),
    });
    assert_eq!(driver.core.conversation(alpha.id).unwrap().member_count, 3);

    let remove = ActionKind::RemoveMember(alpha.id, kai.id);
    let out = driver.send(Input::RemoveMember {
        id: alpha.id,
        user: kai.id,
    });
    assert!(out.effects.contains(&Effect::RemoveMember {
        conversation: alpha.id,
        user: kai.id
    }));
    driver.send(Input::ActionResult {
        kind: remove,
        result: Ok(ActionOutcome::Done),
    });
    assert_eq!(*driver.core.action(remove), ActionState::Idle);
    // The count follows the event that announces the removal, not this answer.
    assert_eq!(driver.core.conversation(alpha.id).unwrap().member_count, 3);
    driver.send(world.event_at(
        11,
        lazy_protocol::Event::MemberRemoved {
            conversation_id: alpha.id,
            user_id: kai.id,
        },
    ));
    assert_eq!(driver.core.conversation(alpha.id).unwrap().member_count, 2);
}

#[test]
fn favorites_move_a_conversation_to_the_top_section() {
    let (world, mut driver, alpha) = ready_with_alpha();
    let kind = ActionKind::SetFavorite(alpha.id);
    let out = driver.send(Input::SetFavorite {
        id: alpha.id,
        favorite: true,
    });
    assert!(out.effects.contains(&Effect::SetFavorite {
        conversation: alpha.id,
        favorite: true
    }));
    assert!(
        driver.core.favorites().is_empty(),
        "not until the server agrees"
    );

    let mut state = read_state(alpha.id, 0, 0);
    state.favorite = true;
    driver.send(Input::ActionResult {
        kind,
        result: Ok(ActionOutcome::UserState(state)),
    });
    assert_eq!(driver.core.favorites(), vec![alpha.id]);
    assert!(!driver.core.channels().contains(&alpha.id));

    // The same change made on another device arrives as an event.
    let mut cleared = read_state(alpha.id, 0, 0);
    cleared.favorite = false;
    driver.send(world.event_at(
        11,
        lazy_protocol::Event::UserStateChanged { state: cleared },
    ));
    assert!(driver.core.favorites().is_empty());
    assert!(driver.core.channels().contains(&alpha.id));
}

#[test]
fn every_action_has_its_own_state() {
    let (_, mut driver, alpha) = ready_with_alpha();
    driver.send(Input::ArchiveConversation(alpha.id));
    driver.send(Input::SetFavorite {
        id: alpha.id,
        favorite: true,
    });
    assert_eq!(
        *driver.core.action(ActionKind::Archive(alpha.id)),
        ActionState::InFlight
    );
    assert_eq!(
        *driver.core.action(ActionKind::SetFavorite(alpha.id)),
        ActionState::InFlight
    );
    assert_eq!(*driver.core.action(ActionKind::Browse), ActionState::Idle);
    assert_eq!(
        *driver.core.action(ActionKind::Unarchive(alpha.id)),
        ActionState::Idle
    );
    let _ = ConversationId::generate();
    let _ = seq(1);
}
