//! Incoming webhooks and bots (slice 2E, D16): who may manage them, the address shown once and
//! kept out of every debug string, what happens when answers are lost, and apps that are never
//! offered as people.

mod common;

use common::{Driver, World, seq};
use lazy_client_core::{
    ActionKind, ActionOutcome, ActionState, Change, CreatedWebhook, Effect, Input, SecretUrl,
};
use lazy_domain::{
    ConversationId, DisplayName, MAX_WEBHOOKS_PER_CONVERSATION, Timestamp, UserId, Username,
    WebhookId, WorkspaceRole,
};
use lazy_protocol::{ApiError, ErrorCode, Event, Mentions, User, Webhook};

const SECRET: &str = "https://chat.example/hooks/0123/s3cr3t-s3cr3t-s3cr3t";

// ----- Helpers -----

fn bot(name: &str) -> User {
    User {
        id: UserId::generate(),
        username: Username::parse(&format!(
            "bot-{}",
            &name.to_lowercase()[..4.min(name.len())]
        ))
        .unwrap(),
        display_name: DisplayName::parse(name).unwrap(),
        is_bot: true,
        status: None,
        deactivated: false,
        avatar_version: 0,
    }
}

fn webhook_for(world: &World, conversation: ConversationId, name: &str, bot: &User) -> Webhook {
    Webhook {
        id: WebhookId::generate(),
        conversation_id: conversation,
        name: DisplayName::parse(name).unwrap(),
        bot_user_id: bot.id,
        created_by: world.me.id,
        created_at: Timestamp::now(),
    }
}

fn admin_world() -> (World, Driver) {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let driver = Driver::ready(&world);
    (world, driver)
}

fn rejected(code: ErrorCode) -> lazy_client_core::RequestError {
    lazy_client_core::RequestError::Rejected(ApiError {
        code,
        message: String::new(),
    })
}

fn lost() -> lazy_client_core::RequestError {
    lazy_client_core::RequestError::Unknown("timed out".into())
}

fn created(webhook: &Webhook) -> CreatedWebhook {
    CreatedWebhook {
        webhook: webhook.clone(),
        url: SecretUrl::new(SECRET.to_owned()),
    }
}

fn create_input(conversation: ConversationId, name: &str) -> Input {
    Input::CreateWebhook {
        conversation,
        name: name.into(),
    }
}

fn answer(
    kind: ActionKind,
    result: Result<ActionOutcome, lazy_client_core::RequestError>,
) -> Input {
    Input::ActionResult { kind, result }
}

fn failed_text(driver: &Driver, kind: ActionKind) -> String {
    match driver.core.action(kind) {
        ActionState::Failed(text) => text.clone(),
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn webhook_effects(driver: &Driver) -> Vec<Effect> {
    driver
        .effects
        .iter()
        .filter(|e| {
            matches!(
                e,
                Effect::CreateWebhook { .. }
                    | Effect::DeleteWebhook { .. }
                    | Effect::LoadWebhooks { .. }
            )
        })
        .cloned()
        .collect()
}

/// Loads `list` as the conversation's webhooks.
fn load(driver: &mut Driver, conversation: ConversationId, list: Vec<Webhook>) {
    driver.send(Input::LoadWebhooks(conversation));
    driver.send(answer(
        ActionKind::LoadWebhooks(conversation),
        Ok(ActionOutcome::Webhooks(list)),
    ));
}

// ----- Who may manage webhooks -----

#[test]
fn only_admins_and_owners_manage_webhooks() {
    {
        let mut world = World::new();
        world.role = WorkspaceRole::Member;
        let general = world.channel.id;
        let mut driver = Driver::ready(&world);
        assert!(!driver.core.can_manage_webhooks(general));
        assert!(driver.core.webhooks(general).is_empty());
        driver.send(create_input(general, "Deploys"));
        driver.send(Input::LoadWebhooks(general));
        assert!(
            webhook_effects(&driver).is_empty(),
            "nothing is asked of the server"
        );
        assert!(
            failed_text(&driver, ActionKind::CreateWebhook(general)).contains("admins and owners")
        );
    }
    for role in [WorkspaceRole::Admin, WorkspaceRole::Owner] {
        let mut world = World::new();
        world.role = role;
        let driver = Driver::ready(&world);
        assert!(driver.core.can_manage_webhooks(world.channel.id));
    }
}

#[test]
fn signing_out_forgets_the_webhooks_and_the_address() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let b = bot("Deploys");
    load(
        &mut driver,
        general,
        vec![webhook_for(&world, general, "Deploys", &b)],
    );
    assert_eq!(driver.core.webhooks(general).len(), 1);
    // Sign-out forgets it all.
    driver.send(Input::SignOut);
    assert!(driver.core.webhooks(general).is_empty());
    assert!(driver.core.created_webhook().is_none());
}

// ----- Creating -----

#[test]
fn creating_a_webhook_shows_its_address_once_and_adds_it_to_the_list() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let b = bot("Deploys");
    let hook = webhook_for(&world, general, "Deploys", &b);
    load(&mut driver, general, vec![]);

    let out = driver.send(create_input(general, "  Deploys  "));
    assert_eq!(
        driver.core.action(ActionKind::CreateWebhook(general)),
        &ActionState::InFlight
    );
    let asked: Vec<_> = out
        .effects
        .iter()
        .filter_map(|e| match e {
            Effect::CreateWebhook {
                conversation,
                request,
            } => Some((*conversation, request.name.as_str().to_owned())),
            _ => None,
        })
        .collect();
    assert_eq!(
        asked,
        vec![(general, "Deploys".to_owned())],
        "the name is trimmed"
    );

    // A second click while it is in flight asks nothing more.
    let again = driver.send(create_input(general, "Deploys"));
    assert!(webhook_effects_in(&again).is_empty());

    let out = driver.send(answer(
        ActionKind::CreateWebhook(general),
        Ok(ActionOutcome::WebhookCreated(created(&hook))),
    ));
    assert!(out.changes.contains(&Change::CreatedWebhookChanged));
    assert!(out.changes.contains(&Change::WebhooksChanged {
        conversation: general
    }));
    let shown = driver.core.created_webhook().expect("the address is held");
    assert_eq!(shown.url.as_str(), SECRET);
    assert_eq!(shown.webhook, hook);
    assert_eq!(driver.core.webhooks(general), std::slice::from_ref(&hook));
    assert_eq!(
        driver.core.action(ActionKind::CreateWebhook(general)),
        &ActionState::Idle
    );

    let out = driver.send(Input::DismissCreatedWebhook);
    assert!(out.changes.contains(&Change::CreatedWebhookChanged));
    assert!(driver.core.created_webhook().is_none());
    assert_eq!(
        driver.core.webhooks(general).len(),
        1,
        "the webhook stays, the address does not"
    );
}

fn webhook_effects_in(out: &lazy_client_core::Output) -> Vec<&Effect> {
    out.effects
        .iter()
        .filter(|e| matches!(e, Effect::CreateWebhook { .. }))
        .collect()
}

#[test]
fn the_address_is_in_no_debug_string() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let b = bot("Deploys");
    let hook = webhook_for(&world, general, "Deploys", &b);
    load(&mut driver, general, vec![]);
    driver.send(create_input(general, "Deploys"));
    let input = answer(
        ActionKind::CreateWebhook(general),
        Ok(ActionOutcome::WebhookCreated(created(&hook))),
    );
    let in_input = format!("{input:?}");
    let out = driver.send(input);
    let in_output = format!("{out:?}");
    let in_core = format!("{:?}", driver.core);
    let in_created = format!("{:?}", driver.core.created_webhook());
    let in_effects = format!("{:?}", driver.effects);
    let in_changes = format!("{:?}", driver.changes);
    for (what, text) in [
        ("the input", in_input),
        ("the output", in_output),
        ("the core", in_core),
        ("the created webhook", in_created),
        ("the recorded effects", in_effects),
        ("the recorded changes", in_changes),
    ] {
        assert!(!text.contains("s3cr3t"), "{what} shows the secret: {text}");
        assert!(!text.contains("hooks/0123"), "{what} shows the address");
    }
    assert!(format!("{:?}", driver.core.created_webhook()).contains("redacted"));
}

#[test]
fn the_address_is_never_cached() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let b = bot("Deploys");
    let hook = webhook_for(&world, general, "Deploys", &b);
    load(&mut driver, general, vec![]);
    driver.send(create_input(general, "Deploys"));
    driver.send(answer(
        ActionKind::CreateWebhook(general),
        Ok(ActionOutcome::WebhookCreated(created(&hook))),
    ));
    // Anything that changes the cache afterwards writes one; none of it may carry the address.
    driver.send(world.event(world.message(general, &world.other, 12, "hi"), None));
    let cache = driver.last_cache().expect("a cache was written");
    let json = serde_json::to_string(cache).unwrap();
    assert!(!json.contains("s3cr3t") && !json.contains("hooks/0123"));
    // And nothing is saved for a restart: a new core starts with no address.
    let restarted = Driver::new();
    assert!(restarted.core.created_webhook().is_none());
}

#[test]
fn a_webhook_name_is_checked_before_asking() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let kind = ActionKind::CreateWebhook(general);
    driver.send(create_input(general, "   "));
    assert_eq!(failed_text(&driver, kind), "Give the webhook a name.");
    driver.send(Input::DismissAction(kind));
    driver.send(create_input(general, &"x".repeat(65)));
    assert!(failed_text(&driver, kind).contains("at most 64"));
    driver.send(Input::DismissAction(kind));
    driver.send(create_input(general, "bad\u{7}name"));
    assert!(failed_text(&driver, kind).contains("control characters"));
    assert!(webhook_effects(&driver).is_empty());
}

#[test]
fn a_conversation_holds_at_most_ten_webhooks() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let list: Vec<Webhook> = (0..MAX_WEBHOOKS_PER_CONVERSATION)
        .map(|i| {
            webhook_for(
                &world,
                general,
                &format!("Hook {i}"),
                &bot(&format!("Bot{i}")),
            )
        })
        .collect();
    load(&mut driver, general, list);
    let before = webhook_effects(&driver).len();
    driver.send(create_input(general, "One too many"));
    assert!(failed_text(&driver, ActionKind::CreateWebhook(general)).contains("at most 10"));
    assert_eq!(webhook_effects(&driver).len(), before);
}

#[test]
fn an_archived_conversation_cannot_get_a_webhook() {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    world.channel.archived = true;
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.send(create_input(general, "Deploys"));
    assert!(failed_text(&driver, ActionKind::CreateWebhook(general)).contains("archived"));
    assert!(webhook_effects(&driver).is_empty());
}

#[test]
fn every_refusal_says_what_to_do() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let kind = ActionKind::CreateWebhook(general);
    let cases = [
        (ErrorCode::Forbidden, "admins and owners"),
        (ErrorCode::NotFound, "no longer exists"),
        (ErrorCode::Conflict, "archived"),
        (ErrorCode::LimitReached, "at most 10"),
    ];
    for (code, expected) in cases {
        driver.send(create_input(general, "Deploys"));
        driver.send(answer(kind, Err(rejected(code))));
        let text = failed_text(&driver, kind);
        assert!(text.contains(expected), "{code:?}: {text}");
        driver.send(Input::DismissAction(kind));
    }
    assert!(driver.core.created_webhook().is_none());
}

#[test]
fn a_lost_answer_when_creating_shows_the_list_so_the_webhook_can_be_deleted() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let b = bot("Deploys");
    let hook = webhook_for(&world, general, "Deploys", &b);
    load(&mut driver, general, vec![]);
    driver.send(create_input(general, "Deploys"));
    let out = driver.send(answer(ActionKind::CreateWebhook(general), Err(lost())));
    let text = failed_text(&driver, ActionKind::CreateWebhook(general));
    assert!(text.contains("isn't known whether"), "{text}");
    assert!(
        out.effects.contains(&Effect::LoadWebhooks {
            conversation: general
        }),
        "the list is loaded to see whether it was created"
    );
    // It was: it is on the list, without an address, and can be deleted.
    driver.send(answer(
        ActionKind::LoadWebhooks(general),
        Ok(ActionOutcome::Webhooks(vec![hook.clone()])),
    ));
    assert_eq!(driver.core.webhooks(general), std::slice::from_ref(&hook));
    assert!(driver.core.created_webhook().is_none());
    let out = driver.send(Input::DeleteWebhook(hook.id));
    assert!(
        out.effects
            .contains(&Effect::DeleteWebhook { webhook: hook.id })
    );
}

#[test]
fn a_create_for_a_conversation_whose_list_was_never_loaded_loads_it() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let b = bot("Deploys");
    let hook = webhook_for(&world, general, "Deploys", &b);
    driver.send(create_input(general, "Deploys"));
    let out = driver.send(answer(
        ActionKind::CreateWebhook(general),
        Ok(ActionOutcome::WebhookCreated(created(&hook))),
    ));
    assert!(out.effects.contains(&Effect::LoadWebhooks {
        conversation: general
    }));
    assert!(driver.core.created_webhook().is_some());
}

// ----- Deleting and loading -----

#[test]
fn deleting_a_webhook_takes_it_off_the_list_and_off_the_screen() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let b = bot("Deploys");
    let hook = webhook_for(&world, general, "Deploys", &b);
    load(&mut driver, general, vec![]);
    driver.send(create_input(general, "Deploys"));
    driver.send(answer(
        ActionKind::CreateWebhook(general),
        Ok(ActionOutcome::WebhookCreated(created(&hook))),
    ));
    let out = driver.send(Input::DeleteWebhook(hook.id));
    assert_eq!(
        out.effects
            .iter()
            .filter(|e| matches!(e, Effect::DeleteWebhook { .. }))
            .count(),
        1
    );
    driver.send(answer(
        ActionKind::DeleteWebhook(hook.id),
        Ok(ActionOutcome::Done),
    ));
    assert!(driver.core.webhooks(general).is_empty());
    assert!(
        driver.core.created_webhook().is_none(),
        "its address is of no use any more"
    );
}

#[test]
fn deleting_a_webhook_that_is_already_gone_is_what_was_wanted() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let b = bot("Deploys");
    let hook = webhook_for(&world, general, "Deploys", &b);
    load(&mut driver, general, vec![hook.clone()]);
    driver.send(Input::DeleteWebhook(hook.id));
    driver.send(answer(
        ActionKind::DeleteWebhook(hook.id),
        Err(rejected(ErrorCode::NotFound)),
    ));
    assert!(driver.core.webhooks(general).is_empty());
    assert_eq!(
        driver.core.action(ActionKind::DeleteWebhook(hook.id)),
        &ActionState::Idle
    );
}

#[test]
fn a_failed_delete_keeps_the_webhook_and_says_why() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let b = bot("Deploys");
    let hook = webhook_for(&world, general, "Deploys", &b);
    load(&mut driver, general, vec![hook.clone()]);
    driver.send(Input::DeleteWebhook(hook.id));
    driver.send(answer(ActionKind::DeleteWebhook(hook.id), Err(lost())));
    assert_eq!(driver.core.webhooks(general).len(), 1);
    assert!(failed_text(&driver, ActionKind::DeleteWebhook(hook.id)).contains("connection"));
}

#[test]
fn loading_keeps_only_the_conversations_own_webhooks() {
    let (world, mut driver) = admin_world();
    let general = world.channel.id;
    let random = world.second_channel.id;
    let mine = webhook_for(&world, general, "Here", &bot("Here"));
    let elsewhere = webhook_for(&world, random, "There", &bot("There"));
    load(&mut driver, general, vec![mine.clone(), elsewhere]);
    assert_eq!(driver.core.webhooks(general), std::slice::from_ref(&mine));
    assert!(driver.core.webhooks(random).is_empty());
}

#[test]
fn losing_a_conversation_forgets_its_webhooks_and_the_address_shown_for_it() {
    let (world, mut driver) = admin_world();
    let random = world.second_channel.id;
    let b = bot("Deploys");
    let hook = webhook_for(&world, random, "Deploys", &b);
    load(&mut driver, random, vec![]);
    driver.send(create_input(random, "Deploys"));
    driver.send(answer(
        ActionKind::CreateWebhook(random),
        Ok(ActionOutcome::WebhookCreated(created(&hook))),
    ));
    assert!(driver.core.created_webhook().is_some());
    driver.send(world.event_at(
        12,
        Event::MemberRemoved {
            conversation_id: random,
            user_id: world.me.id,
        },
    ));
    assert!(driver.core.created_webhook().is_none());
    assert!(driver.core.webhooks(random).is_empty());
}

// ----- Bots -----

/// A world with a bot in `#general`, and its member list loaded.
fn bot_world() -> (World, Driver, User) {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let b = bot("Deploys");
    world.extra_users.push(b.clone());
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let generation = driver
        .effects_matching(|e| match e {
            Effect::FetchMembers {
                conversation,
                generation,
            } if *conversation == general => Some(*generation),
            _ => None,
        })
        .pop()
        .expect("members were requested");
    driver.send(Input::MembersFetched {
        conversation: general,
        generation,
        result: Ok(lazy_protocol::ConversationMembersResponse {
            conversation_id: general,
            user_ids: vec![world.me.id, world.other.id, b.id],
        }),
    });
    (world, driver, b)
}

#[test]
fn a_bot_is_known_by_name_and_flagged_but_not_offered_as_a_person() {
    let (world, driver, b) = bot_world();
    assert!(driver.core.is_bot(b.id));
    assert!(!driver.core.is_bot(world.other.id));
    assert_eq!(driver.core.display_name(b.id), "Deploys");
    assert!(driver.core.member(b.id).is_some_and(|u| u.is_bot));
    let pickers: Vec<UserId> = driver.core.members().iter().map(|u| u.id).collect();
    assert!(pickers.contains(&world.other.id));
    assert!(
        !pickers.contains(&b.id),
        "a person to message or group is never an app"
    );
}

#[test]
fn a_bot_is_never_offered_as_a_mention_and_a_typed_name_stays_text() {
    let (world, mut driver, b) = bot_world();
    let general = world.channel.id;
    let candidates = driver.core.mention_candidates(general, "");
    assert!(candidates.iter().all(|c| c.handle != b.username.as_str()));
    assert!(candidates.iter().any(|c| c.handle == "zoe"));
    assert!(driver.core.mention_candidates(general, "bot-").is_empty());
    let out = driver.send(Input::SendMessage {
        conversation: general,
        text: format!("hello @{} and @zoe", b.username),
    });
    let entry = out
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::PersistSubmission(entry) => Some(entry.clone()),
            _ => None,
        })
        .expect("the message is journaled");
    let named: Mentions =
        entry
            .document
            .mentions()
            .iter()
            .fold(Mentions::default(), |mut all, target| {
                if let lazy_domain::MentionTarget::User { id } = target {
                    all.users.push(*id);
                }
                all
            });
    assert_eq!(
        named.users,
        vec![world.other.id],
        "only the person was named"
    );
}

#[test]
fn a_bot_cannot_be_put_in_a_conversation_or_a_group() {
    let (world, mut driver, b) = bot_world();
    let general = world.channel.id;
    let before = driver.effects.len();
    driver.send(Input::OpenConversation {
        participants: vec![world.other.id, b.id],
    });
    assert!(matches!(
        driver.core.action(ActionKind::OpenConversation),
        ActionState::Failed(text) if text.contains("Apps can't be messaged")
    ));
    driver.send(Input::AddMembers {
        id: general,
        users: vec![b.id],
    });
    assert!(matches!(
        driver.core.action(ActionKind::AddMembers(general)),
        ActionState::Failed(text) if text.contains("webhook")
    ));
    driver.send(Input::CreateGroup {
        handle: "ops".into(),
        name: "Ops".into(),
        members: vec![world.other.id, b.id],
    });
    assert!(matches!(
        driver.core.action(ActionKind::CreateGroup),
        ActionState::Failed(text) if text.contains("Apps can't be members")
    ));
    assert_eq!(
        driver.effects.len(),
        before,
        "nothing was sent for any of them"
    );
}

#[test]
fn what_a_bot_posts_is_an_ordinary_message_that_nobody_can_edit() {
    let (world, mut driver, b) = bot_world();
    let general = world.channel.id;
    let before = driver
        .core
        .user_state(general)
        .map_or(0, |s| s.unread_count);
    let posted = world.message(general, &b, 12, "deploy done");
    driver.send(world.event(posted.clone(), None));
    let row = driver
        .core
        .timeline(general)
        .iter()
        .find(|row| row.message_id() == Some(posted.id))
        .expect("the message is shown");
    assert_eq!(row.author_id, b.id);
    assert_eq!(
        driver
            .core
            .user_state(general)
            .map_or(0, |s| s.unread_count),
        before + 1,
        "it counts as unread like anyone's"
    );
    assert!(
        !driver.core.can_edit_message(general, posted.id),
        "nobody is its author"
    );
    assert!(
        driver.core.can_delete_message(general, posted.id),
        "an admin deletes it"
    );
    assert!(driver.core.can_react_message(general, posted.id));
}

#[test]
fn a_member_cannot_delete_what_a_bot_posted() {
    let mut world = World::new();
    let b = bot("Deploys");
    world.extra_users.push(b.clone());
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let posted = world.message(general, &b, 12, "deploy done");
    driver.send(world.event(posted.clone(), None));
    assert!(!driver.core.can_edit_message(general, posted.id));
    assert!(!driver.core.can_delete_message(general, posted.id));
}

#[test]
fn a_bot_that_joins_is_flagged_from_the_start() {
    let (world, mut driver) = admin_world();
    let b = bot("Alerts");
    driver.send(world.event_at(12, Event::MemberJoined { user: b.clone() }));
    assert!(driver.core.is_bot(b.id));
    assert!(driver.core.members().iter().all(|u| u.id != b.id));
    let _ = seq(1);
}
