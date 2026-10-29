//! Phase 2C-2 end to end: mentions, mention counts, user groups, and autocomplete candidates across
//! real clients, a real server, and real Postgres.

mod support;

use lazy_client_core::{
    ActionKind, ActionState, ClientCore, Input, MentionKind, SessionPhase, TimelineItem,
};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{ChannelVisibility, ConversationId, GroupId, MentionTarget, MessageId, UserId};
use sqlx::PgPool;
use support::{Client, PASSWORD, World};

fn start(world: &World, dir: &std::path::Path, name: &'static str) -> Client {
    Client::start(
        name,
        &world.server.base_url,
        &dir.join(name),
        MemoryStorage::new(),
    )
}

fn user_id(client: &Client, username: &str) -> UserId {
    client.read(|core| {
        core.members()
            .into_iter()
            .find(|u| u.username.as_str() == username)
            .map(|u| u.id)
            .unwrap_or_else(|| panic!("no member {username}"))
    })
}

fn general(client: &Client) -> ConversationId {
    client.read(|core| {
        core.conversations()
            .iter()
            .find(|c| c.is_default)
            .map(|c| c.id)
            .expect("the default channel")
    })
}

async fn select(client: &Client, id: ConversationId) {
    client.act(Input::SelectConversation(id));
    client
        .wait_until("the conversation to be selected", move |core| {
            core.selected_conversation() == Some(id)
        })
        .await;
}

async fn create_public_channel(client: &Client, name: &str) -> ConversationId {
    client.act(Input::CreateChannel {
        name: name.to_owned(),
        visibility: ChannelVisibility::Public,
        topic: String::new(),
        description: String::new(),
    });
    let wanted = name.to_owned();
    client
        .wait_until("the new channel", move |core| {
            core.conversations()
                .iter()
                .any(|c| c.name.as_ref().is_some_and(|n| n.as_str() == wanted))
        })
        .await;
    client
        .read(|core| {
            core.conversations()
                .iter()
                .find(|c| c.name.as_ref().is_some_and(|n| n.as_str() == name))
                .map(|c| c.id)
        })
        .unwrap()
}

async fn join(client: &Client, id: ConversationId) {
    client.act(Input::BrowseChannels);
    client
        .wait_until("the directory", move |core| {
            core.browse()
                .channels
                .iter()
                .any(|b| b.conversation.id == id)
        })
        .await;
    client.act(Input::JoinChannel(id));
    client
        .wait_until("the join", move |core| core.conversation(id).is_some())
        .await;
}

fn row_with(core: &ClientCore, conversation: ConversationId, text: &str) -> Option<TimelineItem> {
    core.timeline(conversation)
        .iter()
        .find(|item| item.document.plain_text() == text)
        .cloned()
}

fn mention_count(core: &ClientCore, conversation: ConversationId) -> u32 {
    core.user_state(conversation).map_or(0, |s| s.mention_count)
}

fn unread(core: &ClientCore, conversation: ConversationId) -> u32 {
    core.user_state(conversation).map_or(0, |s| s.unread_count)
}

async fn stored_mentions(world: &World, id: MessageId) -> (i64, bool) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM message_mentions WHERE message_id = $1), mentions_channel \
         FROM messages WHERE id = $1",
    )
    .bind(id.as_uuid())
    .fetch_one(&world.pool)
    .await
    .expect("stored mentions")
}

/// Three people, all in #general and #design; Bob and Carol are reading #design, so what Alice
/// says in #general is unread for them.
struct Scene {
    world: World,
    _dir: tempfile::TempDir,
    alice: Client,
    bob: Client,
    carol: Client,
    general: ConversationId,
    design: ConversationId,
}

async fn scene(pool: PgPool) -> Scene {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob, carol) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
        start(&world, dir.path(), "carol"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    carol.register("carol").await;
    let general = general(&alice);
    let design = create_public_channel(&alice, "design").await;
    join(&bob, design).await;
    join(&carol, design).await;
    select(&bob, design).await;
    select(&carol, design).await;
    select(&alice, general).await;
    let (bob_id, carol_id) = (
        bob.read(|core| core.me().unwrap().id),
        carol.read(|core| core.me().unwrap().id),
    );
    alice
        .wait_until("everyone in the member list", move |core| {
            core.conversation_members(general)
                .is_some_and(|m| m.contains(&bob_id) && m.contains(&carol_id))
        })
        .await;
    Scene {
        world,
        _dir: dir,
        alice,
        bob,
        carol,
        general,
        design,
    }
}

impl Scene {
    async fn finish(self) {
        self.alice.stop().await;
        self.bob.stop().await;
        self.carol.stop().await;
        self.world.server.shutdown().await.unwrap();
    }
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_mention_counts_for_the_person_named_until_they_read(pool: PgPool) {
    let s = scene(pool).await;
    let (general, bob_id) = (s.general, user_id(&s.alice, "bob"));

    // Alice names Bob. Both are unread for Bob and Carol, but only Bob has a mention.
    s.alice.send("hey @bob look at this");
    s.alice.wait_confirmed("hey @bob look at this").await;
    s.bob
        .wait_until("Bob's mention", move |core| {
            mention_count(core, general) == 1 && unread(core, general) == 1
        })
        .await;
    s.carol
        .wait_until("Carol's unread", move |core| unread(core, general) == 1)
        .await;
    assert_eq!(s.carol.read(|core| mention_count(core, general)), 0);

    // The server decided who is notified, and every client sees the same message.
    let item = s
        .bob
        .read(|core| row_with(core, general, "hey @bob look at this"));
    assert!(
        item.is_some(),
        "the message arrived with the mention in its text"
    );
    let mentioned =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM message_mentions WHERE user_id = $1")
            .bind(bob_id.as_uuid())
            .fetch_one(&s.world.pool)
            .await
            .unwrap();
    assert_eq!(mentioned, 1);

    // Reading the conversation clears the count: it is selected, and the newest message is on
    // screen (a headless client has to say so, as the timeline view does).
    select(&s.bob, general).await;
    s.bob.act(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    s.bob
        .wait_until("the mention to be read", move |core| {
            mention_count(core, general) == 0 && unread(core, general) == 0
        })
        .await;
    s.finish().await;
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn channel_mentions_reach_everyone_and_are_for_admins_in_the_default_channel(pool: PgPool) {
    let s = scene(pool).await;
    let general = s.general;

    // The owner may use @channel in the default channel; everyone in it is mentioned.
    s.alice.send("@channel standup in five");
    s.alice.wait_confirmed("@channel standup in five").await;
    for client in [&s.bob, &s.carol] {
        client
            .wait_until("a channel mention", move |core| {
                mention_count(core, general) == 1
            })
            .await;
    }
    let (_, id) = s.alice.read(|core| {
        let c = core.selected_conversation().unwrap();
        let item = row_with(core, c, "@channel standup in five").unwrap();
        (c, item.message_id().unwrap())
    });
    let (rows, channel) = stored_mentions(&s.world, id).await;
    assert!(
        channel,
        "@channel is stored as a flag, not as a list of people"
    );
    assert_eq!(rows, 0);

    // A member is not offered @channel in the default channel, and typing it mentions no one.
    select(&s.bob, general).await;
    let offered = s.bob.read(|core| core.mention_candidates(general, "chan"));
    assert!(
        offered.iter().all(|c| c.kind != MentionKind::Channel),
        "{offered:?}"
    );
    s.bob.send("@channel hello");
    s.bob.wait_confirmed("@channel hello").await;
    let (_, bobs) = s.bob.read(|core| {
        let c = core.selected_conversation().unwrap();
        let item = row_with(core, c, "@channel hello").unwrap();
        (c, item.message_id().unwrap())
    });
    assert_eq!(stored_mentions(&s.world, bobs).await, (0, false));

    // In another channel a member may use it.
    let design = s.design;
    let offered = s.bob.read(|core| core.mention_candidates(design, "chan"));
    assert!(offered.iter().any(|c| c.kind == MentionKind::Channel));
    s.finish().await;
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn editing_and_deleting_a_message_correct_the_mention_count(pool: PgPool) {
    let s = scene(pool).await;
    let general = s.general;
    let (bob_id, carol_id) = (user_id(&s.alice, "bob"), user_id(&s.alice, "carol"));

    s.alice.send("@bob and @carol please review");
    s.alice
        .wait_confirmed("@bob and @carol please review")
        .await;
    for client in [&s.bob, &s.carol] {
        client
            .wait_until("the mention", move |core| mention_count(core, general) == 1)
            .await;
    }
    let id = s.alice.read(|core| {
        row_with(core, general, "@bob and @carol please review")
            .and_then(|i| i.message_id())
            .unwrap()
    });
    let mentions = s.alice.read(|core| {
        row_with(core, general, "@bob and @carol please review")
            .unwrap()
            .document
            .mentions()
    });
    assert_eq!(
        mentions,
        [
            MentionTarget::User { id: bob_id },
            MentionTarget::User { id: carol_id }
        ]
    );

    // Editing Carol out takes her count back and leaves Bob's.
    s.alice.act(Input::EditMessage {
        conversation: general,
        message: id,
        text: "@bob please review".to_owned(),
    });
    s.carol
        .wait_until("Carol to lose the mention", move |core| {
            mention_count(core, general) == 0 && unread(core, general) == 1
        })
        .await;
    assert_eq!(s.bob.read(|core| mention_count(core, general)), 1);

    // Editing her back in adds it again.
    s.alice.act(Input::EditMessage {
        conversation: general,
        message: id,
        text: "@carol and @bob please review".to_owned(),
    });
    s.carol
        .wait_until("Carol to be mentioned again", move |core| {
            mention_count(core, general) == 1
        })
        .await;

    // Deleting the message removes it from both counts and from the unread count.
    s.alice.act(Input::DeleteMessage {
        conversation: general,
        message: id,
    });
    for client in [&s.bob, &s.carol] {
        client
            .wait_until("the deletion to clear the counts", move |core| {
                mention_count(core, general) == 0 && unread(core, general) == 0
            })
            .await;
    }
    s.finish().await;
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_user_group_is_made_by_an_admin_and_mentions_its_members(pool: PgPool) {
    let s = scene(pool).await;
    let design = s.design;
    let (bob_id, carol_id) = (user_id(&s.alice, "bob"), user_id(&s.alice, "carol"));

    // Members cannot manage groups.
    assert!(s.bob.read(|core| !core.can_manage_groups()));
    assert!(s.alice.read(|core| core.can_manage_groups()));

    s.alice.act(Input::CreateGroup {
        handle: "Design-Team".to_owned(),
        name: "Design team".to_owned(),
        members: vec![bob_id],
    });
    for client in [&s.alice, &s.bob, &s.carol] {
        client
            .wait_until("the group to arrive", |core| core.groups().len() == 1)
            .await;
    }
    s.alice
        .wait_until("the dialog to settle", |core| {
            *core.action(ActionKind::CreateGroup) == ActionState::Idle
        })
        .await;
    let group: GroupId = s.alice.read(|core| core.groups()[0].id);
    assert_eq!(
        s.carol.read(|core| core.groups()[0].handle.to_string()),
        "design-team"
    );

    // The group is offered everywhere, and naming it notifies only its members.
    select(&s.alice, design).await;
    let offered = s
        .alice
        .read(|core| core.mention_candidates(design, "design-t"));
    assert!(
        offered
            .iter()
            .any(|c| c.kind == MentionKind::Group && c.handle == "design-team")
    );
    // Bob and Carol are reading #general now, so #design is unread for them.
    select(&s.bob, s.general).await;
    select(&s.carol, s.general).await;
    s.alice.send("@design-team ship it");
    s.alice.wait_confirmed("@design-team ship it").await;
    s.bob
        .wait_until("Bob, a member, to be mentioned", move |core| {
            mention_count(core, design) == 1
        })
        .await;
    s.carol
        .wait_until("Carol to see it unread", move |core| {
            unread(core, design) == 1
        })
        .await;
    assert_eq!(s.carol.read(|core| mention_count(core, design)), 0);

    // Changing the membership applies to later messages only: the earlier one keeps who it
    // notified.
    s.alice.act(Input::UpdateGroup {
        id: group,
        handle: None,
        name: None,
        members: Some(vec![carol_id]),
    });
    s.carol
        .wait_until("Carol to join the group", move |core| {
            core.group(group).is_some_and(|g| g.members == [carol_id])
        })
        .await;
    s.alice.send("@design-team second");
    s.alice.wait_confirmed("@design-team second").await;
    s.carol
        .wait_until("Carol, now a member, to be mentioned", move |core| {
            mention_count(core, design) == 1
        })
        .await;
    assert_eq!(
        s.bob.read(|core| mention_count(core, design)),
        1,
        "Bob keeps the mention he already had and gets no new one"
    );

    // Deleting the group leaves the messages that named it as they were.
    s.alice.act(Input::DeleteGroup(group));
    for client in [&s.alice, &s.bob, &s.carol] {
        client
            .wait_until("the group to go", |core| core.groups().is_empty())
            .await;
    }
    assert!(
        s.bob
            .read(|core| row_with(core, design, "@design-team ship it").is_some())
    );
    s.finish().await;
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_member_cannot_create_a_group_and_handles_are_not_shared(pool: PgPool) {
    let s = scene(pool).await;

    // The core refuses without a request, and says so.
    s.bob.act(Input::CreateGroup {
        handle: "ops".to_owned(),
        name: "Ops".to_owned(),
        members: vec![],
    });
    s.bob
        .wait_until("a refusal", |core| {
            matches!(core.action(ActionKind::CreateGroup), ActionState::Failed(_))
        })
        .await;
    let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM user_groups")
        .fetch_one(&s.world.pool)
        .await
        .unwrap();
    assert_eq!(groups, 0);

    // A group cannot take a person's handle or a reserved word.
    for handle in ["bob", "channel"] {
        s.alice.act(Input::CreateGroup {
            handle: handle.to_owned(),
            name: "Clash".to_owned(),
            members: vec![],
        });
        s.alice
            .wait_until("a refusal", |core| {
                matches!(core.action(ActionKind::CreateGroup), ActionState::Failed(_))
            })
            .await;
        s.alice.act(Input::DismissAction(ActionKind::CreateGroup));
    }
    let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM user_groups")
        .fetch_one(&s.world.pool)
        .await
        .unwrap();
    assert_eq!(groups, 0);

    // And a new person cannot register a group's handle or a reserved word.
    s.alice.act(Input::CreateGroup {
        handle: "ops".to_owned(),
        name: "Ops".to_owned(),
        members: vec![],
    });
    s.alice
        .wait_until("the group", |core| core.groups().len() == 1)
        .await;
    for taken in ["ops", "channel"] {
        let dir = tempfile::tempdir().unwrap();
        let newcomer = start(&s.world, dir.path(), "newcomer");
        newcomer
            .wait_until("the sign-in screen", |core| {
                matches!(core.phase(), SessionPhase::SignedOut { .. })
            })
            .await;
        newcomer.act(Input::Register {
            username: taken.to_owned(),
            display_name: "Newcomer".to_owned(),
            password: PASSWORD.to_owned(),
            invite_code: String::new(),
        });
        newcomer
            .wait_until("a refusal to register", |core| {
                matches!(
                    core.phase(),
                    SessionPhase::SignedOut {
                        last_error: Some(_)
                    }
                )
            })
            .await;
        newcomer.stop().await;
    }
    s.finish().await;
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn autocomplete_offers_only_people_who_can_see_the_conversation(pool: PgPool) {
    let s = scene(pool).await;
    let (bob_id, carol_id) = (user_id(&s.alice, "bob"), user_id(&s.alice, "carol"));

    // A private channel with Alice and Bob only.
    s.alice.act(Input::CreateChannel {
        name: "secret".to_owned(),
        visibility: ChannelVisibility::Private,
        topic: String::new(),
        description: String::new(),
    });
    s.alice
        .wait_until("the private channel", |core| {
            core.conversations()
                .iter()
                .any(|c| c.name.as_ref().is_some_and(|n| n.as_str() == "secret"))
        })
        .await;
    let secret = s
        .alice
        .read(|core| {
            core.conversations()
                .iter()
                .find(|c| c.name.as_ref().is_some_and(|n| n.as_str() == "secret"))
                .map(|c| c.id)
        })
        .unwrap();
    s.alice.act(Input::AddMembers {
        id: secret,
        users: vec![bob_id],
    });
    s.bob
        .wait_until("Bob to be added", move |core| {
            core.conversation(secret).is_some()
        })
        .await;
    select(&s.alice, secret).await;
    s.alice
        .wait_until("the member list", move |core| {
            core.conversation_members(secret)
                .is_some_and(|m| m.contains(&bob_id))
        })
        .await;

    let handles = |client: &Client, conversation, query: &str| -> Vec<(String, bool)> {
        client.read(|core| {
            core.mention_candidates(conversation, query)
                .into_iter()
                .filter(|c| c.kind == MentionKind::User)
                .map(|c| (c.handle, c.in_conversation))
                .collect()
        })
    };
    // In the private channel: Bob, never Carol.
    assert_eq!(handles(&s.alice, secret, ""), [("bob".to_owned(), true)]);
    assert!(handles(&s.alice, secret, "car").is_empty());
    // In the public channel: members first, then others marked as outside it.
    s.alice
        .wait_until("the #design member list", {
            let design = s.design;
            move |core| core.conversation_members(design).is_some()
        })
        .await;
    let in_design = handles(&s.alice, s.design, "");
    assert!(in_design.contains(&("bob".to_owned(), true)));
    assert!(in_design.contains(&("carol".to_owned(), true)));
    // Carol is not a member of the private channel, and nothing there is offered to her.
    assert!(
        s.carol
            .read(|core| core.mention_candidates(secret, "").is_empty())
    );

    // Alice mentions Carol in the private channel by typing it: it stays text, notifies no one.
    s.alice.send("@carol are you there");
    s.alice.wait_confirmed("@carol are you there").await;
    let id = s.alice.read(|core| {
        row_with(core, secret, "@carol are you there")
            .and_then(|i| i.message_id())
            .unwrap()
    });
    assert_eq!(stored_mentions(&s.world, id).await, (0, false));
    let _ = carol_id;
    s.finish().await;
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_mention_made_while_a_client_was_away_arrives_by_replay(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = start(&world, dir.path(), "alice");
    let bob_profile = dir.path().join("bob");
    let bob_secrets = MemoryStorage::new();
    let bob = Client::start(
        "bob",
        &world.server.base_url,
        &bob_profile,
        bob_secrets.clone(),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    bob.wait_until("the cache to be saved", |core| {
        core.selected_conversation().is_some()
    })
    .await;
    let design = create_public_channel(&alice, "design").await;
    join(&bob, design).await;
    select(&bob, design).await;
    bob.send("anyone here?");
    bob.wait_confirmed("anyone here?").await;
    let cached_cursor = bob.last_cached_cursor().expect("bob saved his cache");
    let bob_id = bob.read(|core| core.me().unwrap().id);
    bob.stop().await;

    select(&alice, general).await;
    // Mentions resolve against the conversation's member list, which arrives a moment after it is
    // selected; a name typed before then stays plain text.
    alice
        .wait_until("Bob in the member list", move |core| {
            core.conversation_members(general)
                .is_some_and(|members| members.contains(&bob_id))
        })
        .await;
    alice.send("@bob welcome back");
    alice.wait_confirmed("@bob welcome back").await;

    let bob = Client::start("bob", &world.server.base_url, &bob_profile, bob_secrets);
    bob.wait_until_ready().await;
    bob.wait_until("the mention from replay", move |core| {
        mention_count(core, general) == 1 && unread(core, general) == 1
    })
    .await;
    assert!(
        bob.subscribe_positions().contains(&cached_cursor),
        "resumed from the cached cursor"
    );

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}
