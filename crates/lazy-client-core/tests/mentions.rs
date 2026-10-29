//! Mentions, mention counts, user groups, and who is in a conversation (slice 2C-2).

mod common;

use common::{Driver, World, channel, people, read_state, seq};
use lazy_client_core::{
    ActionKind, ActionState, Change, EditState, Effect, Input, MentionCandidate, MentionKind,
    RequestError, StartupState,
};
use lazy_domain::{
    ChannelVisibility, ConversationId, DisplayName, GroupHandle, GroupId, MentionDirectory,
    MentionTarget, MessageDocument, UserId, Username, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, ConversationMembersResponse, ErrorCode, Event, EventEnvelope, GroupResponse,
    Mentions, Message, OperationStatus, ServerFrame, User, UserGroup,
};

// ----- Helpers -----

fn person(username: &str, display: &str) -> User {
    User {
        id: UserId::generate(),
        username: Username::parse(username).unwrap(),
        display_name: DisplayName::parse(display).unwrap(),
        is_bot: false,
        status: None,
        deactivated: false,
        avatar_version: 0,
    }
}

fn group(world: &World, handle: &str, name: &str, members: &[&User]) -> UserGroup {
    UserGroup {
        id: GroupId::generate(),
        workspace_id: world.workspace.id,
        handle: GroupHandle::parse(handle).unwrap(),
        name: DisplayName::parse(name).unwrap(),
        members: members.iter().map(|user| user.id).collect(),
    }
}

fn mentions(users: &[&User], channel: bool) -> Mentions {
    Mentions {
        users: users.iter().map(|user| user.id).collect(),
        channel,
    }
}

fn with_mentions(message: &Message, users: &[&User], channel: bool) -> Message {
    let mut mentioning = message.clone();
    mentioning.mentions = mentions(users, channel);
    mentioning
}

fn event(world: &World, at: i64, event: Event) -> Input {
    Input::Frame(ServerFrame::Event(EventEnvelope {
        workspace_id: world.workspace.id,
        seq: seq(at),
        occurred_at: lazy_domain::Timestamp::now(),
        event,
    }))
}

fn edited(world: &World, at: i64, now: &Message, previous: Mentions) -> Input {
    event(
        world,
        at,
        Event::MessageEdited {
            message: now.clone(),
            operation_id: None,
            previous_mentions: previous,
        },
    )
}

fn deleted(world: &World, at: i64, was: &Message, had: Mentions) -> Input {
    event(
        world,
        at,
        Event::MessageDeleted {
            message: world.tombstone(was),
            had_mentions: had,
        },
    )
}

/// The generation of the latest members request for `conversation`.
fn members_generation(driver: &Driver, conversation: ConversationId) -> u64 {
    driver
        .effects_matching(|effect| match effect {
            Effect::FetchMembers {
                conversation: c,
                generation,
            } if *c == conversation => Some(*generation),
            _ => None,
        })
        .pop()
        .expect("members were requested")
}

fn members_response(
    conversation: ConversationId,
    users: &[&User],
) -> Result<ConversationMembersResponse, RequestError> {
    Ok(ConversationMembersResponse {
        conversation_id: conversation,
        user_ids: users.iter().map(|user| user.id).collect(),
    })
}

fn load_members(driver: &mut Driver, conversation: ConversationId, users: &[&User]) {
    let generation = members_generation(driver, conversation);
    driver.send(Input::MembersFetched {
        conversation,
        generation,
        result: members_response(conversation, users),
    });
}

fn handles(candidates: &[MentionCandidate]) -> Vec<&str> {
    candidates.iter().map(|c| c.handle.as_str()).collect()
}

fn counts(driver: &Driver, conversation: ConversationId) -> (u32, u32) {
    let state = driver.core.user_state(conversation).unwrap();
    (state.unread_count, state.mention_count)
}

fn sent_document(driver: &mut Driver, conversation: ConversationId, text: &str) -> MessageDocument {
    let out = driver.send(Input::SendMessage {
        conversation,
        text: text.into(),
    });
    out.effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.document.clone()),
            _ => None,
        })
        .expect("the message should be sent")
}

fn rejected(code: ErrorCode, message: &str) -> RequestError {
    RequestError::Rejected(ApiError {
        code,
        message: message.into(),
    })
}

/// A workspace with alice, bob, and carol besides me and zoe. #general is the default channel
/// and #random another public one. Alice and bob are in #general; carol is in neither channel.
struct Fixture {
    world: World,
    alice: User,
    bob: User,
    carol: User,
    driver: Driver,
}

fn fixture(role: WorkspaceRole) -> Fixture {
    let alice = person("alice", "Alice Smith");
    let bob = person("bob", "Bob Jones");
    let carol = person("carol", "Carol Diaz");
    let mut world = World::new();
    world.role = role;
    world.extra_users = vec![alice.clone(), bob.clone(), carol.clone()];
    let mut driver = Driver::ready(&world);
    load_members(
        &mut driver,
        world.channel.id,
        &[&world.me, &world.other, &alice, &bob],
    );
    Fixture {
        world,
        alice,
        bob,
        carol,
        driver,
    }
}

// ----- Candidates and the directory -----

#[test]
fn candidates_list_the_conversation_then_other_members_then_groups_then_channel() {
    let mut f = fixture(WorkspaceRole::Admin);
    let general = f.world.channel.id;
    let team = group(&f.world, "design-team", "Design Team", &[&f.alice]);
    f.driver.send(event(
        &f.world,
        11,
        Event::GroupChanged {
            group: team.clone(),
        },
    ));

    let found = f.driver.core.mention_candidates(general, "");
    let listed: Vec<(&str, bool, MentionKind)> = found
        .iter()
        .map(|c| (c.handle.as_str(), c.in_conversation, c.kind))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("alice", true, MentionKind::User),
            ("bob", true, MentionKind::User),
            ("zoe", true, MentionKind::User),
            ("carol", false, MentionKind::User),
            ("design-team", true, MentionKind::Group),
            ("channel", true, MentionKind::Channel),
        ],
        "people in the channel by name, then other members of the workspace (naming them \
         notifies no one), then groups, then @channel"
    );
    assert_eq!(found[0].target, MentionTarget::User { id: f.alice.id });
    assert_eq!(found[0].display_name, "Alice Smith");
    assert_eq!(found[4].target, MentionTarget::Group { id: team.id });
    assert_eq!(found[5].target, MentionTarget::Channel);
    assert!(
        found.iter().all(|c| c.handle != "jules"),
        "the person is never offered themself"
    );
}

#[test]
fn matches_rank_handle_prefix_then_handle_substring_then_display_name_ignoring_case() {
    let malice = person("malice", "Malice Wonder");
    let ally = person("dev.ops", "Ally Brown");
    let dali = person("painter", "Salvador Dali");
    let mut world = World::new();
    world.extra_users = vec![malice.clone(), ally.clone(), dali.clone()];
    let mut driver = Driver::ready(&world);
    let general = world.channel.id;
    load_members(&mut driver, general, &[&world.me, &malice, &ally, &dali]);

    // "AL": malice contains it, ally's name starts with it, dali's name has a word containing it.
    let found = driver.core.mention_candidates(general, "AL");
    assert_eq!(
        handles(&found),
        ["malice", "dev.ops", "painter"],
        "handle contains, then a display-name word starts with it, then it appears in a word"
    );
    // Handle prefix beats everything, whatever the display name.
    let found = driver.core.mention_candidates(general, "@mal");
    assert_eq!(handles(&found), ["malice"]);
    // Nothing matches: nothing is offered (and not even @channel, which does not contain it).
    assert!(driver.core.mention_candidates(general, "zzz").is_empty());
    // @channel matches the start of its own name.
    assert_eq!(
        handles(
            &driver
                .core
                .mention_candidates(world.second_channel.id, "chan")
        ),
        ["channel"]
    );
}

#[test]
fn channel_mentions_follow_the_role_and_the_default_channel() {
    // An ordinary member may use @channel anywhere except the default channel.
    let member = fixture(WorkspaceRole::Member);
    let general = member.world.channel.id;
    let random = member.world.second_channel.id;
    assert!(
        member
            .driver
            .core
            .mention_candidates(general, "chan")
            .is_empty()
    );
    assert!(
        member
            .driver
            .core
            .mention_directory(general)
            .resolve("channel")
            .is_none()
    );
    assert_eq!(
        handles(&member.driver.core.mention_candidates(random, "chan")),
        ["channel"]
    );
    assert!(
        member
            .driver
            .core
            .mention_directory(random)
            .resolve("channel")
            .is_some()
    );

    // Admins and owners may use it in the default channel too.
    for role in [WorkspaceRole::Admin, WorkspaceRole::Owner] {
        let admin = fixture(role);
        assert_eq!(
            handles(
                &admin
                    .driver
                    .core
                    .mention_candidates(admin.world.channel.id, "chan")
            ),
            ["channel"],
            "{role:?}"
        );
    }
}

#[test]
fn private_and_direct_conversations_offer_only_their_people() {
    let alice = person("alice", "Alice Smith");
    let bob = person("bob", "Bob Jones");
    let mut world = World::new();
    world.extra_users = vec![alice.clone(), bob.clone()];
    let mut secret = channel(world.workspace.id, "secret");
    secret.visibility = ChannelVisibility::Private;
    let direct = people(world.workspace.id, &[&world.me, &alice]);
    let huddle = people(world.workspace.id, &[&world.me, &alice, &bob]);
    world.extra = vec![secret.clone(), direct.clone(), huddle.clone()];
    let mut driver = Driver::ready(&world);

    // Direct and group conversations list their participants without asking.
    assert_eq!(
        handles(&driver.core.mention_candidates(direct.id, "")),
        ["alice", "channel"]
    );
    assert_eq!(
        handles(&driver.core.mention_candidates(huddle.id, "")),
        ["alice", "bob", "channel"]
    );
    assert_eq!(
        driver.core.conversation_members(direct.id).unwrap().len(),
        2
    );

    // A private channel: only its members, never other people of the workspace, once known.
    driver.send(Input::SelectConversation(secret.id));
    assert_eq!(
        handles(&driver.core.mention_candidates(secret.id, "")),
        ["channel"],
        "until its people are known nobody is offered"
    );
    load_members(&mut driver, secret.id, &[&world.me, &bob]);
    assert_eq!(
        handles(&driver.core.mention_candidates(secret.id, "")),
        ["bob", "channel"]
    );
    let directory = driver.core.mention_directory(secret.id);
    assert!(directory.resolve("bob").is_some());
    assert!(directory.resolve("alice").is_none(), "not in the channel");
    assert!(directory.resolve("zoe").is_none(), "not in the channel");
    assert!(directory.resolve("jules").is_none(), "never the person");
}

#[test]
fn people_who_cannot_be_written_as_a_handle_are_never_offered() {
    let trailing = person("dot.", "Dot Matrix");
    let reserved = person("here", "Here Now");
    let channel_named = person("everyone", "Every One");
    let fine = person("dora", "Dora Fine");
    let mut world = World::new();
    world.extra_users = vec![
        trailing.clone(),
        reserved.clone(),
        channel_named.clone(),
        fine.clone(),
    ];
    let mut driver = Driver::ready(&world);
    let general = world.channel.id;
    load_members(
        &mut driver,
        general,
        &[&world.me, &trailing, &reserved, &channel_named, &fine],
    );
    // Only dora can be written as a handle; zoe is a member of the workspace outside the list.
    let found = driver.core.mention_candidates(general, "");
    assert_eq!(handles(&found), ["dora", "zoe"]);
    assert!(driver.core.mention_candidates(general, "dot").is_empty());
    let directory = driver.core.mention_directory(general);
    for unusable in ["dot.", "here", "everyone", "channel"] {
        assert!(directory.resolve(unusable).is_none(), "{unusable}");
    }
    assert!(directory.resolve("dora").is_some());
}

#[test]
fn the_directory_is_what_autocomplete_offers_and_sending_uses_it() {
    let mut f = fixture(WorkspaceRole::Member);
    let general = f.world.channel.id;
    let directory = f.driver.core.mention_directory(general);
    // Everything that was offered resolves, whatever the case; the person themself does not.
    for candidate in f.driver.core.mention_candidates(general, "") {
        let (target, handle) = directory
            .resolve(&candidate.handle.to_uppercase())
            .unwrap_or_else(|| panic!("{} should resolve", candidate.handle));
        assert_eq!((target, handle), (candidate.target, candidate.handle));
    }
    assert!(directory.resolve("jules").is_none());
    assert!(directory.resolve("nobody").is_none());

    // So a typed name becomes a mention exactly when it was offered.
    let document = sent_document(
        &mut f.driver,
        general,
        "hello @Alice, @carol and @ghost, not @jules or @channel",
    );
    assert_eq!(
        document.mentions(),
        vec![
            MentionTarget::User { id: f.alice.id },
            MentionTarget::User { id: f.carol.id }
        ],
        "carol is a member of the workspace; a member may not use @channel in #general"
    );
    assert!(document.plain_text().contains("@ghost"));
    assert!(document.plain_text().contains("@channel"));
}

#[test]
fn groups_and_channel_mentions_are_parsed_where_allowed() {
    let mut f = fixture(WorkspaceRole::Admin);
    let general = f.world.channel.id;
    let team = group(&f.world, "design-team", "Design Team", &[&f.alice]);
    f.driver.send(event(
        &f.world,
        11,
        Event::GroupChanged {
            group: team.clone(),
        },
    ));
    let document = sent_document(&mut f.driver, general, "@design-team, @channel: standup");
    assert_eq!(
        document.mentions(),
        vec![MentionTarget::Group { id: team.id }, MentionTarget::Channel]
    );
    // The editor shows the source again, and parsing it again gives the same document.
    let source = document.to_markdown();
    assert!(source.starts_with("@design-team, @channel"), "{source}");
}

#[test]
fn an_edit_is_parsed_with_the_same_directory() {
    let mut f = fixture(WorkspaceRole::Member);
    let general = f.world.channel.id;
    let mine = f.world.message(general, &f.world.me, 5, "my words");
    f.driver.load_history(general, vec![mine.clone()], false);
    let out = f.driver.send(Input::EditMessage {
        conversation: general,
        message: mine.id,
        text: "my words, cc @bob and @nobody".into(),
    });
    let entry = out
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.clone()),
            _ => None,
        })
        .expect("the edit is journaled");
    assert_eq!(
        entry.document.mentions(),
        vec![MentionTarget::User { id: f.bob.id }]
    );
    // The editor's source stays as typed while the edit is saved.
    assert_eq!(
        f.driver.core.edit_text(general, mine.id).unwrap(),
        "my words, cc @bob and @nobody"
    );
}

#[test]
fn a_refused_mention_says_why_and_keeps_the_words() {
    let mut f = fixture(WorkspaceRole::Admin);
    let general = f.world.channel.id;
    // A send: the failed row keeps its document and says what was wrong.
    let out = f.driver.send(Input::SendMessage {
        conversation: general,
        text: "@channel hi".into(),
    });
    let operation = out
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.operation_id),
            _ => None,
        })
        .unwrap();
    f.driver.send(Input::SubmissionPersisted { operation });
    f.driver.send(Input::SendResult {
        operation,
        result: Err(rejected(ErrorCode::Forbidden, "no")),
    });
    let failed = f.driver.core.timeline(general).last().unwrap();
    let lazy_client_core::Delivery::Failed { reason, retry } = &failed.delivery else {
        panic!("expected a failed send, got {:?}", failed.delivery);
    };
    assert!(
        reason.contains("@channel") && reason.contains("admins"),
        "{reason}"
    );
    assert_eq!(*retry, lazy_client_core::RetryKind::NewOperation);
    assert_eq!(failed.document.mentions(), vec![MentionTarget::Channel]);

    // An unknown person or group is invalid_request.
    let out = f.driver.send(Input::SendMessage {
        conversation: general,
        text: "hi @alice".into(),
    });
    let operation = out
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.operation_id),
            _ => None,
        })
        .unwrap();
    f.driver.send(Input::SubmissionPersisted { operation });
    f.driver.send(Input::SendResult {
        operation,
        result: Err(rejected(ErrorCode::InvalidRequest, "unknown mention")),
    });
    let failed = f.driver.core.timeline(general).last().unwrap();
    let lazy_client_core::Delivery::Failed { reason, .. } = &failed.delivery else {
        panic!("expected a failed send");
    };
    assert!(reason.contains("mention"), "{reason}");

    // A refusal that has nothing to do with mentions keeps the server's words.
    let out = f.driver.send(Input::SendMessage {
        conversation: general,
        text: "plain".into(),
    });
    let operation = out
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.operation_id),
            _ => None,
        })
        .unwrap();
    f.driver.send(Input::SubmissionPersisted { operation });
    f.driver.send(Input::SendResult {
        operation,
        result: Err(rejected(ErrorCode::Forbidden, "The channel is archived.")),
    });
    let failed = f.driver.core.timeline(general).last().unwrap();
    let lazy_client_core::Delivery::Failed { reason, .. } = &failed.delivery else {
        panic!("expected a failed send");
    };
    assert_eq!(reason, "The channel is archived.");
}

#[test]
fn a_refused_edit_with_a_mention_keeps_the_text_to_fix() {
    let mut f = fixture(WorkspaceRole::Admin);
    let general = f.world.channel.id;
    let mine = f.world.message(general, &f.world.me, 5, "my words");
    f.driver.load_history(general, vec![mine.clone()], false);
    let out = f.driver.send(Input::EditMessage {
        conversation: general,
        message: mine.id,
        text: "@channel look".into(),
    });
    let operation = out
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.operation_id),
            _ => None,
        })
        .unwrap();
    f.driver.send(Input::SubmissionPersisted { operation });
    f.driver.send(Input::EditResult {
        operation,
        result: Err(rejected(ErrorCode::Forbidden, "no")),
    });
    let row = f
        .driver
        .core
        .timeline(general)
        .iter()
        .find(|item| item.message_id() == Some(mine.id))
        .unwrap();
    assert!(
        matches!(&row.edit, EditState::Failed { text, reason, .. } if text == "@channel look" && reason.contains("@channel")),
        "{:?}",
        row.edit
    );
    // The row shows what the server has again, and nothing is left unsettled.
    assert_eq!(row.document, mine.document);
    assert_eq!(f.driver.core.unsettled_operations(), 0);
}

// ----- Mention counts -----

/// A world where the person has read through position 4 of #general (log head 10).
fn counting() -> (World, Driver) {
    let mut world = World::new();
    world.user_states = vec![read_state(world.channel.id, 4, 0)];
    let driver = Driver::ready(&world);
    (world, driver)
}

fn mention_of_me(world: &World, message: &Message) -> Message {
    with_mentions(message, &[&world.me], false)
}

#[test]
fn a_message_counts_as_a_mention_when_it_names_the_person_or_everyone() {
    let (world, mut driver) = counting();
    let general = world.channel.id;
    let named = mention_of_me(&world, &world.message(general, &world.other, 11, "named"));
    driver.send(world.event(named, None));
    assert_eq!(counts(&driver, general), (1, 1));

    let everyone = with_mentions(&world.message(general, &world.other, 12, "all"), &[], true);
    driver.send(world.event(everyone, None));
    assert_eq!(counts(&driver, general), (2, 2), "@channel names everyone");

    // Someone else named, or nobody: unread but not a mention.
    let third = person("alice", "Alice");
    let elsewhere = with_mentions(
        &world.message(general, &world.other, 13, "hey"),
        &[&third],
        false,
    );
    driver.send(world.event(elsewhere, None));
    driver.send(world.event(world.message(general, &world.other, 14, "plain"), None));
    assert_eq!(counts(&driver, general), (4, 2));

    // The person's own message reads everything, mentions included.
    let own = with_mentions(
        &world.message(general, &world.me, 15, "mine"),
        &[&world.me],
        false,
    );
    driver.send(world.event(own, None));
    assert_eq!(counts(&driver, general), (0, 0));
}

#[test]
fn a_replayed_tombstone_never_counts_as_a_mention() {
    let (world, mut driver) = counting();
    let general = world.channel.id;
    let named = mention_of_me(&world, &world.message(general, &world.other, 11, "gone"));
    // The creation arrives already deleted: placeholder text, no mentions.
    driver.send(world.event(world.tombstone(&named), None));
    assert_eq!(counts(&driver, general), (0, 0));
    // An edit of it is a tombstone too, and keeps who the message notified before.
    driver.send(edited(
        &world,
        12,
        &world.tombstone(&named),
        mentions(&[&world.me], false),
    ));
    assert_eq!(counts(&driver, general), (0, 0));
    // And the deletion that follows has nothing to take away.
    driver.send(deleted(&world, 13, &named, mentions(&[&world.me], false)));
    assert_eq!(counts(&driver, general), (0, 0));
}

#[test]
fn an_edit_adds_or_removes_the_person_only_for_a_message_that_was_counted() {
    let (world, mut driver) = counting();
    let general = world.channel.id;
    let plain = world.message(general, &world.other, 11, "hello");
    driver.send(world.event(plain.clone(), None));
    assert_eq!(counts(&driver, general), (1, 0));

    // The edit names the person.
    let named = mention_of_me(&world, &world.edited(&plain, "hello @jules"));
    driver.send(edited(&world, 12, &named, Mentions::default()));
    assert_eq!(counts(&driver, general), (1, 1));
    // Naming them again (or the same edit delivered again) changes nothing.
    let again = world.edited(&named, "hello again @jules");
    driver.send(edited(&world, 13, &again, mentions(&[&world.me], false)));
    driver.send(edited(&world, 13, &again, mentions(&[&world.me], false)));
    assert_eq!(counts(&driver, general), (1, 1));
    // Then it stops naming them.
    let unnamed = with_mentions(&world.edited(&again, "never mind"), &[], false);
    driver.send(edited(&world, 14, &unnamed, mentions(&[&world.me], false)));
    assert_eq!(counts(&driver, general), (1, 0));

    // A message that was never counted: the person's own, or one they have read.
    let own = world.message(general, &world.me, 15, "my words");
    driver.send(world.event(own.clone(), None));
    assert_eq!(counts(&driver, general), (0, 0));
    let named_own = mention_of_me(&world, &world.edited(&own, "me again"));
    driver.send(edited(&world, 16, &named_own, Mentions::default()));
    assert_eq!(
        counts(&driver, general),
        (0, 0),
        "nobody is notified of their own words"
    );
}

#[test]
fn the_baseline_is_authoritative_and_events_adjust_what_it_counted() {
    let mut world = World::new();
    // Three unread messages, two of which mention the person.
    world.user_states = vec![lazy_protocol::ConversationUserState {
        conversation_id: world.channel.id,
        favorite: false,
        last_read: seq(4),
        unread_count: 3,
        mention_count: 2,
    }];
    let mut driver = Driver::ready(&world);
    let general = world.channel.id;
    let a = mention_of_me(&world, &world.message(general, &world.other, 6, "a"));
    let b = mention_of_me(&world, &world.message(general, &world.other, 7, "b"));
    let c = world.message(general, &world.other, 8, "c");
    assert_eq!(counts(&driver, general), (3, 2));

    // The first edit after the baseline says what the baseline held for that message.
    let a_now = with_mentions(&world.edited(&a, "a, edited"), &[], false);
    driver.send(edited(&world, 11, &a_now, mentions(&[&world.me], false)));
    assert_eq!(counts(&driver, general), (3, 1));
    // A deletion removes what the message notified before it went.
    driver.send(deleted(&world, 12, &b, mentions(&[&world.me], false)));
    assert_eq!(counts(&driver, general), (2, 0));
    driver.send(deleted(&world, 13, &c, Mentions::default()));
    assert_eq!(counts(&driver, general), (1, 0));
    // Something at or before the baseline's position was already reflected in it.
    driver.send(deleted(&world, 9, &a, mentions(&[&world.me], false)));
    assert_eq!(counts(&driver, general), (1, 0));
}

#[test]
fn a_deletion_takes_away_what_the_client_holds_for_the_message() {
    let (world, mut driver) = counting();
    let general = world.channel.id;
    // Counted as a mention, then edited away from the person, then deleted.
    let named = mention_of_me(&world, &world.message(general, &world.other, 11, "named"));
    driver.send(world.event(named.clone(), None));
    let unnamed = with_mentions(&world.edited(&named, "unnamed"), &[], false);
    driver.send(edited(&world, 12, &unnamed, mentions(&[&world.me], false)));
    assert_eq!(counts(&driver, general), (1, 0));
    driver.send(deleted(&world, 13, &unnamed, Mentions::default()));
    assert_eq!(counts(&driver, general), (0, 0));
}

#[test]
fn the_count_is_the_same_live_or_through_a_replay_in_every_order() {
    // 1. Seen live, then deleted.
    let (world, mut driver) = counting();
    let general = world.channel.id;
    let named = mention_of_me(&world, &world.message(general, &world.other, 11, "named"));
    driver.send(world.event(named.clone(), None));
    assert_eq!(counts(&driver, general), (1, 1));
    driver.send(deleted(&world, 12, &named, mentions(&[&world.me], false)));
    assert_eq!(counts(&driver, general), (0, 0));

    // 2. Seen only as a replayed tombstone, then deleted.
    let (world, mut driver) = counting();
    let general = world.channel.id;
    let named = mention_of_me(&world, &world.message(general, &world.other, 11, "named"));
    driver.send(world.event(world.tombstone(&named), None));
    driver.send(deleted(&world, 12, &named, mentions(&[&world.me], false)));
    assert_eq!(counts(&driver, general), (0, 0));

    // 3. Seen live, then the cursor is behind two edits and the deletion: the edits come as
    // tombstones that still say who the message notified before them.
    let (world, mut driver) = counting();
    let general = world.channel.id;
    let named = mention_of_me(&world, &world.message(general, &world.other, 11, "named"));
    driver.send(world.event(named.clone(), None));
    let me = mentions(&[&world.me], false);
    // Edit 1 removes the person, edit 2 names them again, then it is deleted.
    driver.send(edited(&world, 12, &world.tombstone(&named), me.clone()));
    driver.send(edited(
        &world,
        13,
        &world.tombstone(&named),
        Mentions::default(),
    ));
    driver.send(deleted(&world, 14, &named, me));
    assert_eq!(counts(&driver, general), (0, 0));
}

#[test]
fn a_state_event_replaces_the_mention_count() {
    let (world, mut driver) = counting();
    let general = world.channel.id;
    let named = mention_of_me(&world, &world.message(general, &world.other, 11, "named"));
    driver.send(world.event(named, None));
    driver.send(event(
        &world,
        12,
        Event::UserStateChanged {
            state: lazy_protocol::ConversationUserState {
                conversation_id: general,
                favorite: false,
                last_read: seq(4),
                unread_count: 5,
                mention_count: 3,
            },
        },
    ));
    assert_eq!(counts(&driver, general), (5, 3));
}

#[test]
fn reading_past_a_mention_stops_counting_it() {
    let mut world = World::new();
    world.user_states = vec![lazy_protocol::ConversationUserState {
        conversation_id: world.channel.id,
        favorite: false,
        last_read: seq(4),
        unread_count: 2,
        mention_count: 1,
    }];
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    // The page has the two unread messages; the first names the person.
    let first = mention_of_me(&world, &world.message(general, &world.other, 6, "first"));
    let second = world.message(general, &world.other, 7, "second");
    driver.load_history(general, vec![first.clone(), second], false);
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(6),
        result: Ok(read_state(general, 6, 1)),
    });
    assert_eq!(counts(&driver, general), (1, 0));

    // A live mention that is then read stops counting too, whatever the answer says.
    let live = mention_of_me(&world, &world.message(general, &world.other, 11, "live"));
    driver.send(world.event(live, None));
    assert_eq!(counts(&driver, general), (2, 1));
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(11),
        result: Ok(read_state(general, 11, 0)),
    });
    assert_eq!(counts(&driver, general), (0, 0));
}

#[test]
fn what_was_counted_as_a_mention_survives_a_restart() {
    let (world, mut driver) = counting();
    let general = world.channel.id;
    let named = mention_of_me(&world, &world.message(general, &world.other, 11, "named"));
    driver.send(world.event(named.clone(), None));
    let plain = world.message(general, &world.other, 12, "plain");
    driver.send(world.event(plain, None));
    assert_eq!(counts(&driver, general), (2, 1));
    let cache = driver.last_cache().unwrap().clone();

    // The app restarts from that cache and replay delivers the deletion of the mentioning
    // message. Even a deletion that carries no `had_mentions` takes the mention away, because
    // the client remembers that it counted one.
    let mut restarted = Driver::new();
    restarted.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache),
        ..Default::default()
    }));
    assert_eq!(counts(&restarted, general), (2, 1));
    restarted.send(event(
        &world,
        13,
        Event::MessageDeleted {
            message: world.tombstone(&named),
            had_mentions: Mentions::default(),
        },
    ));
    assert_eq!(counts(&restarted, general), (1, 0));
}

#[test]
fn saved_read_states_from_before_mentions_still_read() {
    let json = serde_json::json!({
        "state": {
            "conversation_id": ConversationId::generate().to_string(),
            "favorite": false,
            "last_read": 4,
            "unread_count": 2,
            "mention_count": 0
        },
        "as_of": 12,
        "counted": [13]
    });
    let saved: lazy_client_core::CachedUserState = serde_json::from_value(json).unwrap();
    assert!(saved.counted_mentions.is_empty() && saved.rebuilt_through.is_none());
    let json = serde_json::to_value(&saved).unwrap();
    assert!(json.get("counted_mentions").is_none() && json.get("rebuilt_through").is_none());
}

// ----- Groups -----

fn group_event(world: &World, at: i64, group: &UserGroup) -> Input {
    event(
        world,
        at,
        Event::GroupChanged {
            group: group.clone(),
        },
    )
}

fn started_with_groups(world: &World, groups: Vec<UserGroup>) -> Driver {
    let mut world_with = World::new();
    world_with.role = world.role;
    world_with.workspace = world.workspace.clone();
    world_with.channel = world.channel.clone();
    world_with.second_channel = world.second_channel.clone();
    world_with.me = world.me.clone();
    world_with.other = world.other.clone();
    world_with.groups = groups;
    Driver::ready(&world_with)
}

fn create_effect(out: &lazy_client_core::Output) -> Option<lazy_protocol::CreateGroupRequest> {
    out.effects.iter().find_map(|effect| match effect {
        Effect::CreateGroup { request, .. } => Some(request.clone()),
        _ => None,
    })
}

#[test]
fn groups_come_with_the_bootstrap_and_follow_events() {
    let world = World::new();
    let zoe_team = group(&world, "zoes", "Zoe's People", &[&world.other]);
    let design = group(
        &world,
        "design-team",
        "Design Team",
        &[&world.me, &world.other],
    );
    let mut driver = started_with_groups(&world, vec![zoe_team.clone(), design.clone()]);
    let handles: Vec<&str> = driver
        .core
        .groups()
        .iter()
        .map(|g| g.handle.as_str())
        .collect();
    assert_eq!(handles, ["design-team", "zoes"], "ordered by handle");
    assert_eq!(driver.core.group(design.id).unwrap().members.len(), 2);
    assert!(driver.core.group(GroupId::generate()).is_none());

    // A changed group replaces the old one; a new one is added.
    let renamed = UserGroup {
        handle: GroupHandle::parse("designers").unwrap(),
        members: vec![world.me.id],
        ..design.clone()
    };
    let out = driver.send(group_event(&world, 11, &renamed));
    assert!(out.changes.contains(&Change::GroupsChanged));
    assert_eq!(driver.core.groups().len(), 2);
    assert_eq!(
        driver.core.group(design.id).unwrap().handle.as_str(),
        "designers"
    );
    assert_eq!(
        driver.core.group(design.id).unwrap().members,
        vec![world.me.id]
    );
    let fresh = group(&world, "ops", "Ops", &[]);
    driver.send(group_event(&world, 12, &fresh));
    assert_eq!(driver.core.groups().len(), 3);

    // A deleted one goes, once; a group of a workspace that is not known is ignored.
    let out = driver.send(event(
        &world,
        13,
        Event::GroupDeleted {
            group_id: renamed.id,
        },
    ));
    assert!(out.changes.contains(&Change::GroupsChanged));
    assert!(driver.core.group(design.id).is_none());
    let out = driver.send(event(
        &world,
        14,
        Event::GroupDeleted {
            group_id: renamed.id,
        },
    ));
    assert!(!out.changes.contains(&Change::GroupsChanged));
    let mut foreign = fresh.clone();
    foreign.id = GroupId::generate();
    foreign.workspace_id = lazy_domain::WorkspaceId::generate();
    driver.send(group_event(&world, 15, &foreign));
    assert!(driver.core.group(foreign.id).is_none());
}

#[test]
fn groups_are_saved_in_the_cache_and_mentionable_after_a_restart() {
    let world = World::new();
    let team = group(&world, "design-team", "Design Team", &[&world.other]);
    let mut driver = started_with_groups(&world, vec![team.clone()]);
    driver.send(world.event(
        world.message(world.channel.id, &world.other, 11, "later"),
        None,
    ));
    let cache = driver.last_cache().unwrap().clone();
    assert_eq!(cache.workspaces[0].groups, vec![team.clone()]);

    let mut restarted = Driver::new();
    restarted.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache),
        ..Default::default()
    }));
    assert_eq!(restarted.core.groups(), std::slice::from_ref(&team));
    assert!(
        restarted
            .core
            .mention_directory(world.channel.id)
            .resolve("design-team")
            .is_some()
    );
}

#[test]
fn only_admins_and_owners_manage_groups() {
    for (role, may) in [
        (WorkspaceRole::Member, false),
        (WorkspaceRole::Admin, true),
        (WorkspaceRole::Owner, true),
    ] {
        let mut world = World::new();
        world.role = role;
        let mut driver = Driver::ready(&world);
        assert_eq!(driver.core.can_manage_groups(), may, "{role:?}");
        let out = driver.send(Input::CreateGroup {
            handle: "design-team".into(),
            name: "Design Team".into(),
            members: vec![world.other.id],
        });
        assert_eq!(create_effect(&out).is_some(), may, "{role:?}");
        if !may {
            assert!(matches!(
                driver.core.action(ActionKind::CreateGroup),
                ActionState::Failed(text) if text.contains("admins")
            ));
        }
    }
}

#[test]
fn creating_a_group_carries_an_operation_and_is_reconciled_like_a_channel() {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let mut driver = Driver::ready(&world);
    let out = driver.send(Input::CreateGroup {
        handle: "@Design-Team".into(),
        name: "  Design Team ".into(),
        members: vec![world.other.id, world.other.id, world.me.id],
    });
    let request = create_effect(&out).expect("the request is sent");
    assert_eq!(
        request.handle.as_str(),
        "design-team",
        "the @ and the case are forgiven"
    );
    assert_eq!(request.name.as_str(), "Design Team");
    assert_eq!(
        request.members,
        vec![world.other.id, world.me.id],
        "no repeats"
    );
    assert_eq!(
        driver.core.action(ActionKind::CreateGroup),
        &ActionState::InFlight
    );
    // A second click while it is in flight does nothing.
    let again = driver.send(Input::CreateGroup {
        handle: "design-team".into(),
        name: "Design Team".into(),
        members: vec![],
    });
    assert!(create_effect(&again).is_none());

    // The answer is lost: the server is asked, never asked to create again with a new id.
    let operation = request.operation_id;
    let out = driver.send(Input::GroupCommandResult {
        operation,
        result: Err(RequestError::Unknown("timed out".into())),
    });
    assert!(
        out.effects
            .contains(&Effect::QueryOperationStatus { operation })
    );
    // It has no record: the same request is sent again with the same id.
    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    assert_eq!(create_effect(&out).unwrap().operation_id, operation);
    // Then it committed after all: the group is kept and the action is finished.
    let created = UserGroup {
        id: GroupId::generate(),
        workspace_id: world.workspace.id,
        handle: request.handle.clone(),
        name: request.name.clone(),
        members: request.members.clone(),
    };
    driver.send(Input::GroupCommandResult {
        operation,
        result: Ok(GroupResponse {
            group: created.clone(),
        }),
    });
    assert_eq!(driver.core.groups(), [created]);
    assert_eq!(
        driver.core.action(ActionKind::CreateGroup),
        &ActionState::Idle
    );
}

#[test]
fn a_late_group_committed_answer_finishes_the_creation() {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let mut driver = Driver::ready(&world);
    let out = driver.send(Input::CreateGroup {
        handle: "ops".into(),
        name: "Ops".into(),
        members: vec![],
    });
    let request = create_effect(&out).unwrap();
    let operation = request.operation_id;
    driver.send(Input::GroupCommandResult {
        operation,
        result: Err(RequestError::Unknown("lost".into())),
    });
    let created = UserGroup {
        id: GroupId::generate(),
        workspace_id: world.workspace.id,
        handle: request.handle,
        name: request.name,
        members: vec![],
    };
    driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::GroupCommitted {
            group: created.clone(),
        }),
    });
    assert_eq!(driver.core.group(created.id), Some(&created));
    assert_eq!(
        driver.core.action(ActionKind::CreateGroup),
        &ActionState::Idle
    );
}

#[test]
fn creating_a_group_checks_what_it_can_and_explains_refusals() {
    let mut world = World::new();
    world.role = WorkspaceRole::Owner;
    let mut driver = Driver::ready(&world);
    let try_create = |driver: &mut Driver, handle: &str, name: &str| {
        let out = driver.send(Input::CreateGroup {
            handle: handle.into(),
            name: name.into(),
            members: vec![],
        });
        (
            create_effect(&out).is_some(),
            driver.core.action(ActionKind::CreateGroup).clone(),
        )
    };

    // Not a handle, a reserved word, a person's name, a name that is empty.
    for (handle, name) in [
        ("x", "Name"),
        ("-bad", "Name"),
        ("has space", "Name"),
        ("channel", "Name"),
        ("here", "Name"),
        ("everyone", "Name"),
        ("ok-handle", "   "),
    ] {
        let (sent, action) = try_create(&mut driver, handle, name);
        assert!(!sent, "{handle:?} {name:?}");
        assert!(
            matches!(action, ActionState::Failed(_)),
            "{handle:?} {name:?}"
        );
    }
    let (sent, action) = try_create(&mut driver, "zoe", "Zoe");
    assert!(!sent, "a person already has that name");
    assert!(
        matches!(&action, ActionState::Failed(text) if text.contains("already")),
        "{action:?}"
    );

    // Too many people.
    let many: Vec<UserId> = (0..=lazy_domain::MAX_GROUP_MEMBERS)
        .map(|_| UserId::generate())
        .collect();
    let out = driver.send(Input::CreateGroup {
        handle: "crowd".into(),
        name: "Crowd".into(),
        members: many,
    });
    assert!(create_effect(&out).is_none());

    // The server's refusals, in words.
    for (code, words) in [
        (ErrorCode::NameTaken, "already used"),
        (ErrorCode::LimitReached, "at most"),
        (ErrorCode::Forbidden, "admins and owners"),
    ] {
        let out = driver.send(Input::CreateGroup {
            handle: "fresh".into(),
            name: "Fresh".into(),
            members: vec![],
        });
        let operation = create_effect(&out).unwrap().operation_id;
        driver.send(Input::GroupCommandResult {
            operation,
            result: Err(rejected(code, "")),
        });
        let action = driver.core.action(ActionKind::CreateGroup).clone();
        assert!(
            matches!(&action, ActionState::Failed(text) if text.contains(words)),
            "{code:?}: {action:?}"
        );
        driver.send(Input::DismissAction(ActionKind::CreateGroup));
        assert_eq!(driver.core.unsettled_operations(), 0);
    }
}

#[test]
fn changing_and_deleting_a_group_are_plain_requests() {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let team = group(&world, "design-team", "Design Team", &[&world.other]);
    let mut driver = started_with_groups(&world, vec![team.clone()]);
    driver.send(Input::SelectConversation(world.channel.id));
    // Make the admin role stick (started_with_groups copies it).
    assert!(driver.core.can_manage_groups());

    let kind = ActionKind::UpdateGroup(team.id);
    let out = driver.send(Input::UpdateGroup {
        id: team.id,
        handle: None,
        name: Some("Designers".into()),
        members: Some(vec![world.me.id, world.other.id, world.me.id]),
    });
    let sent = out
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::UpdateGroup { group, request } => Some((*group, request.clone())),
            _ => None,
        })
        .expect("the change is sent");
    assert_eq!(sent.0, team.id);
    assert_eq!(sent.1.handle, None);
    assert_eq!(sent.1.name.as_ref().unwrap().as_str(), "Designers");
    assert_eq!(sent.1.members, Some(vec![world.me.id, world.other.id]));
    assert_eq!(driver.core.action(kind), &ActionState::InFlight);
    let changed = UserGroup {
        name: DisplayName::parse("Designers").unwrap(),
        members: vec![world.me.id, world.other.id],
        ..team.clone()
    };
    driver.send(Input::ActionResult {
        kind,
        result: Ok(lazy_client_core::ActionOutcome::Group(changed.clone())),
    });
    assert_eq!(driver.core.group(team.id), Some(&changed));
    assert_eq!(driver.core.action(kind), &ActionState::Idle);

    // Nothing to change, an unknown group, a handle that a person has.
    let out = driver.send(Input::UpdateGroup {
        id: team.id,
        handle: None,
        name: None,
        members: None,
    });
    assert!(out.effects.is_empty());
    assert!(matches!(driver.core.action(kind), ActionState::Failed(_)));
    let out = driver.send(Input::UpdateGroup {
        id: team.id,
        handle: Some("zoe".into()),
        name: None,
        members: None,
    });
    assert!(out.effects.is_empty());
    let out = driver.send(Input::UpdateGroup {
        id: GroupId::generate(),
        handle: None,
        name: Some("x".into()),
        members: None,
    });
    assert!(out.effects.is_empty());

    // Deleting.
    let kind = ActionKind::DeleteGroup(team.id);
    let out = driver.send(Input::DeleteGroup(team.id));
    assert!(
        out.effects
            .contains(&Effect::DeleteGroup { group: team.id })
    );
    assert_eq!(driver.core.action(kind), &ActionState::InFlight);
    driver.send(Input::ActionResult {
        kind,
        result: Ok(lazy_client_core::ActionOutcome::Done),
    });
    assert!(driver.core.group(team.id).is_none());
    assert_eq!(driver.core.action(kind), &ActionState::Idle);
}

#[test]
fn deleting_a_group_that_is_already_gone_is_deleting_it() {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let team = group(&world, "design-team", "Design Team", &[]);
    let mut driver = started_with_groups(&world, vec![team.clone()]);
    let kind = ActionKind::DeleteGroup(team.id);
    driver.send(Input::DeleteGroup(team.id));
    driver.send(Input::ActionResult {
        kind,
        result: Err(rejected(ErrorCode::NotFound, "")),
    });
    assert!(driver.core.group(team.id).is_none());
    assert_eq!(driver.core.action(kind), &ActionState::Idle);

    // Other refusals are explained and the group stays.
    let other = group(&world, "ops", "Ops", &[]);
    driver.send(group_event(&world, 11, &other));
    let kind = ActionKind::DeleteGroup(other.id);
    driver.send(Input::DeleteGroup(other.id));
    driver.send(Input::ActionResult {
        kind,
        result: Err(rejected(ErrorCode::Forbidden, "")),
    });
    assert!(driver.core.group(other.id).is_some());
    assert!(
        matches!(driver.core.action(kind), ActionState::Failed(text) if text.contains("admins"))
    );
}

// ----- Who is in a conversation -----

fn member_added(
    world: &World,
    at: i64,
    conversation: &lazy_protocol::Conversation,
    user: &User,
) -> Input {
    event(
        world,
        at,
        Event::MemberAdded {
            conversation: conversation.clone(),
            user_id: user.id,
        },
    )
}

fn member_removed(world: &World, at: i64, conversation: ConversationId, user: &User) -> Input {
    event(
        world,
        at,
        Event::MemberRemoved {
            conversation_id: conversation,
            user_id: user.id,
        },
    )
}

fn fetches(driver: &Driver, conversation: ConversationId) -> usize {
    driver
        .effects_matching(|effect| match effect {
            Effect::FetchMembers {
                conversation: c, ..
            } if *c == conversation => Some(()),
            _ => None,
        })
        .len()
}

fn member_ids(driver: &Driver, conversation: ConversationId) -> Option<Vec<UserId>> {
    driver.core.conversation_members(conversation).map(|users| {
        let mut ids = users.to_vec();
        ids.sort();
        ids
    })
}

fn sorted(users: &[&User]) -> Vec<UserId> {
    let mut ids: Vec<UserId> = users.iter().map(|user| user.id).collect();
    ids.sort();
    ids
}

#[test]
fn a_channels_people_are_fetched_when_it_is_selected_and_only_once() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let general = world.channel.id;
    let random = world.second_channel.id;
    assert_eq!(
        fetches(&driver, general),
        1,
        "the selected channel is asked about at once"
    );
    assert_eq!(
        driver.core.conversation_members(general),
        None,
        "until the answer"
    );
    assert_eq!(fetches(&driver, random), 0);

    let generation = members_generation(&driver, general);
    let out = driver.send(Input::MembersFetched {
        conversation: general,
        generation,
        result: members_response(general, &[&world.me, &world.other]),
    });
    assert!(out.changes.contains(&Change::ConversationMembersChanged {
        conversation: general
    }));
    assert_eq!(
        member_ids(&driver, general),
        Some(sorted(&[&world.me, &world.other]))
    );

    // Selecting it again does not ask again; selecting another channel asks about that one.
    driver.send(Input::SelectConversation(random));
    driver.send(Input::SelectConversation(general));
    assert_eq!(fetches(&driver, general), 1);
    assert_eq!(fetches(&driver, random), 1);
}

#[test]
fn member_events_keep_a_list_current_and_a_removed_person_takes_the_conversation_away() {
    let world = World::new();
    let alice = person("alice", "Alice");
    let mut driver = Driver::ready(&world);
    let general = world.channel.id;
    load_members(&mut driver, general, &[&world.me, &world.other]);

    let out = driver.send(member_added(&world, 11, &world.channel, &alice));
    assert!(out.changes.contains(&Change::ConversationMembersChanged {
        conversation: general
    }));
    assert_eq!(
        member_ids(&driver, general),
        Some(sorted(&[&world.me, &world.other, &alice]))
    );
    // Added twice, or removed twice, is the same as once.
    driver.send(member_added(&world, 12, &world.channel, &alice));
    driver.send(member_removed(&world, 13, general, &world.other));
    driver.send(member_removed(&world, 14, general, &world.other));
    assert_eq!(
        member_ids(&driver, general),
        Some(sorted(&[&world.me, &alice]))
    );

    // When the person is the one removed, the conversation and its list go.
    driver.send(member_removed(&world, 15, general, &world.me));
    assert_eq!(driver.core.conversation_members(general), None);
    assert!(driver.core.conversation(general).is_none());
}

#[test]
fn a_change_that_arrives_while_the_list_is_being_fetched_is_not_undone_by_the_answer() {
    let world = World::new();
    let alice = person("alice", "Alice");
    let mut driver = Driver::ready(&world);
    let general = world.channel.id;
    let generation = members_generation(&driver, general);

    // While the request is out, alice joins and zoe leaves.
    driver.send(member_added(&world, 11, &world.channel, &alice));
    driver.send(member_removed(&world, 12, general, &world.other));
    driver.send(Input::MembersFetched {
        conversation: general,
        generation,
        // The snapshot predates both changes.
        result: members_response(general, &[&world.me, &world.other]),
    });
    assert_eq!(
        member_ids(&driver, general),
        Some(sorted(&[&world.me, &alice]))
    );
}

#[test]
fn an_answer_to_an_earlier_request_is_not_taken_for_the_current_one() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let general = world.channel.id;
    let first = members_generation(&driver, general);

    // A refused cursor drops everything; the bootstrap that follows asks again.
    driver.send(Input::Frame(ServerFrame::SnapshotRequired {
        workspace_id: world.workspace.id,
        reason: lazy_protocol::SnapshotReason::CursorExpired,
    }));
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    let second = members_generation(&driver, general);
    assert_ne!(first, second);

    // The old answer arrives first and is ignored.
    driver.send(Input::MembersFetched {
        conversation: general,
        generation: first,
        result: members_response(general, &[&world.me]),
    });
    assert_eq!(driver.core.conversation_members(general), None);
    driver.send(Input::MembersFetched {
        conversation: general,
        generation: second,
        result: members_response(general, &[&world.me, &world.other]),
    });
    assert_eq!(
        member_ids(&driver, general),
        Some(sorted(&[&world.me, &world.other]))
    );
}

#[test]
fn a_failed_fetch_is_tried_again_when_the_person_comes_back_to_the_channel() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let general = world.channel.id;
    let generation = members_generation(&driver, general);
    driver.send(Input::MembersFetched {
        conversation: general,
        generation,
        result: Err(RequestError::Unknown("timed out".into())),
    });
    // No hammering: nothing more is asked until something changes.
    driver.send(Input::DismissNotice);
    driver.send(Input::WindowActive(true));
    assert_eq!(fetches(&driver, general), 1);
    assert_eq!(driver.core.conversation_members(general), None);
    assert!(
        driver.core.notice().is_none(),
        "a missing list is not worth an alarm"
    );
    driver.send(Input::SelectConversation(general));
    assert_eq!(fetches(&driver, general), 2);
}

#[test]
fn the_selected_list_is_fetched_again_after_the_connection_catches_up() {
    let world = World::new();
    let alice = person("alice", "Alice");
    let mut driver = Driver::ready(&world);
    let general = world.channel.id;
    load_members(&mut driver, general, &[&world.me, &world.other]);
    assert_eq!(fetches(&driver, general), 1);

    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: world.workspace.id,
        seq: seq(10),
    }));
    assert_eq!(fetches(&driver, general), 2);
    // The old list stays usable while the new one is on its way.
    assert_eq!(
        member_ids(&driver, general),
        Some(sorted(&[&world.me, &world.other]))
    );
    load_members(&mut driver, general, &[&world.me, &world.other, &alice]);
    assert_eq!(
        member_ids(&driver, general),
        Some(sorted(&[&world.me, &world.other, &alice]))
    );
    // Caught up again with nothing to fetch for another channel the person never looked at.
    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: world.workspace.id,
        seq: seq(10),
    }));
    assert_eq!(fetches(&driver, world.second_channel.id), 0);
}

#[test]
fn an_edit_that_arrives_as_a_tombstone_changes_nothing_until_the_next_event_says_more() {
    let (world, mut driver) = counting();
    let general = world.channel.id;
    let named = mention_of_me(&world, &world.message(general, &world.other, 11, "named"));
    driver.send(world.event(named.clone(), None));
    assert_eq!(counts(&driver, general), (1, 1));
    // The edit comes as a tombstone: it does not say what the message notified afterwards.
    driver.send(edited(
        &world,
        12,
        &world.tombstone(&named),
        mentions(&[&world.me], false),
    ));
    assert_eq!(counts(&driver, general), (1, 1));
    // The next event's "before" is what that edit left: the person was dropped, so catch up.
    driver.send(edited(
        &world,
        13,
        &world.tombstone(&named),
        Mentions::default(),
    ));
    assert_eq!(counts(&driver, general), (1, 0));
    // Deleted while naming nobody: nothing more to take away.
    driver.send(deleted(&world, 14, &named, Mentions::default()));
    assert_eq!(counts(&driver, general), (0, 0));
}

#[test]
fn a_mark_read_answer_from_ahead_of_the_events_is_not_taken_on_an_incomplete_timeline() {
    let mut world = World::new();
    // Two unread messages from the bootstrap (positions 6 and 7), and the history is not loaded.
    world.user_states = vec![read_state(world.channel.id, 4, 2)];
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let b = world.message(general, &world.other, 7, "b");
    driver.send(world.event(world.message(general, &world.other, 11, "c"), None));
    assert_eq!(counts(&driver, general), (3, 0));

    // Reading the first one. The answer is from later: message b is already deleted there, so
    // only c is unread. The deletion event has not arrived.
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(6),
        result: Ok(read_state(general, 6, 1)),
    });
    // The answer's count is not used; the baseline's message that was read cannot be told apart
    // from the others, so the count waits for the state event rather than guess too low.
    assert_eq!(counts(&driver, general).0, 3);
    driver.send(deleted(&world, 12, &b, Mentions::default()));
    assert_eq!(
        counts(&driver, general).0,
        2,
        "the deletion is subtracted once"
    );
    // The state event for the read makes it exact: only c is unread.
    driver.send(event(
        &world,
        13,
        Event::UserStateChanged {
            state: read_state(general, 6, 1),
        },
    ));
    assert_eq!(counts(&driver, general), (1, 0));
}

#[test]
fn a_rebuilt_count_remembers_which_rows_mention_the_person() {
    let mut world = World::new();
    world.user_states = vec![lazy_protocol::ConversationUserState {
        conversation_id: world.channel.id,
        favorite: false,
        last_read: seq(4),
        unread_count: 2,
        mention_count: 1,
    }];
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let a = world.message(general, &world.other, 6, "a");
    let b = mention_of_me(&world, &world.message(general, &world.other, 7, "b"));
    driver.load_history(general, vec![a, b.clone()], false);
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(6),
        result: Ok(read_state(general, 6, 1)),
    });
    assert_eq!(
        counts(&driver, general),
        (1, 1),
        "b is the one left, and it names the person"
    );
    // An edit that drops the person from b takes the mention away.
    let unnamed = with_mentions(&world.edited(&b, "b, edited"), &[], false);
    driver.send(edited(&world, 11, &unnamed, mentions(&[&world.me], false)));
    assert_eq!(counts(&driver, general), (1, 0));
}
