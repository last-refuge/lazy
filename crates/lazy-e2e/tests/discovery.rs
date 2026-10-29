//! Phase 3E end to end: the channel directory, the people directory and profiles, the quick
//! switcher, and saved messages, with real client stacks, a real server, and real Postgres.

mod support;

use lazy_client_core::{
    ActionKind, ActionState, ClientCore, Input, LoadStatus, QuickSwitcherTarget, SessionPhase,
};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{ChannelVisibility, ConversationId, MessageId, WorkspaceRole};
use lazy_protocol::ChannelSort;
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

fn general(client: &Client) -> ConversationId {
    client.read(|core| {
        core.conversations()
            .iter()
            .find(|c| c.is_default)
            .map(|c| c.id)
            .expect("the default channel")
    })
}

fn channel_id(core: &ClientCore, name: &str) -> Option<ConversationId> {
    core.conversations()
        .iter()
        .find(|c| c.name.as_ref().is_some_and(|n| n.as_str() == name))
        .map(|c| c.id)
}

async fn create_channel(
    client: &Client,
    name: &str,
    visibility: ChannelVisibility,
) -> ConversationId {
    client.act(Input::CreateChannel {
        name: name.to_owned(),
        visibility,
        topic: format!("about {name}"),
        description: String::new(),
    });
    let wanted = name.to_owned();
    client
        .wait_until("the new channel", move |core| {
            channel_id(core, &wanted).is_some()
                && *core.action(ActionKind::CreateChannel) == ActionState::Idle
        })
        .await;
    client.read(|core| channel_id(core, name).unwrap())
}

fn root_of(client: &Client, conversation: ConversationId, text: &str) -> MessageId {
    client.read(|core| {
        core.timeline(conversation)
            .iter()
            .find(|i| i.document.plain_text() == text)
            .and_then(|i| i.message_id())
            .unwrap_or_else(|| panic!("no confirmed row {text:?}"))
    })
}

async fn sign_in_second_device(client: &Client, username: &str) {
    client
        .wait_until("the sign-in form", |core| {
            matches!(core.phase(), SessionPhase::SignedOut { .. })
        })
        .await;
    client.act(Input::SignIn {
        username: username.to_owned(),
        password: PASSWORD.to_owned(),
    });
    client.wait_until_ready().await;
}

fn browse_names(core: &ClientCore) -> Vec<String> {
    core.browse()
        .channels
        .iter()
        .map(|b| {
            b.conversation
                .name
                .as_ref()
                .map(|n| n.as_str().to_owned())
                .unwrap_or_default()
        })
        .collect()
}

fn browse_ready(core: &ClientCore) -> bool {
    matches!(core.browse().status, LoadStatus::Ready)
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn the_channel_directory_filters_sorts_and_pages(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let design = create_channel(&alice, "design", ChannelVisibility::Public).await;
    create_channel(&alice, "dev-ops", ChannelVisibility::Public).await;
    create_channel(&alice, "random-stuff", ChannelVisibility::Public).await;
    create_channel(&alice, "secret-plans", ChannelVisibility::Private).await;

    // Bob browses: every public channel, never the private one, and he has joined only #general.
    bob.act(Input::BrowseChannels);
    bob.wait_until("the directory", |core| {
        browse_ready(core) && core.browse().channels.len() == 4
    })
    .await;
    let names = bob.read(browse_names);
    assert_eq!(names, ["design", "dev-ops", "general", "random-stuff"]);
    assert!(bob.read(|core| {
        core.browse()
            .channels
            .iter()
            .all(|b| b.joined == b.conversation.is_default)
    }));

    // A text filter matches names and topics; the newest settings win.
    bob.act(Input::SetBrowseQuery("dev".to_owned()));
    bob.wait_until("the filtered directory", |core| {
        browse_ready(core) && browse_names(core) == ["dev-ops"]
    })
    .await;
    bob.act(Input::SetBrowseQuery("about rand".to_owned()));
    bob.wait_until("a topic match", |core| {
        browse_ready(core) && browse_names(core) == ["random-stuff"]
    })
    .await;
    bob.act(Input::SetBrowseQuery(String::new()));
    bob.act(Input::SetBrowseJoined(Some(false)));
    bob.wait_until("only channels not joined", |core| {
        browse_ready(core) && browse_names(core) == ["design", "dev-ops", "random-stuff"]
    })
    .await;

    // Joining updates the loaded rows without a new request, and the member count sorts.
    bob.act(Input::JoinChannel(design));
    bob.wait_until("the join", move |core| {
        core.browse()
            .channels
            .iter()
            .any(|b| b.conversation.id == design && b.joined)
            || core
                .browse()
                .channels
                .iter()
                .all(|b| b.conversation.id != design)
    })
    .await;
    bob.act(Input::SetBrowseJoined(None));
    bob.act(Input::SetBrowseSort(ChannelSort::Members));
    bob.wait_until("sorted by members", |core| {
        browse_ready(core)
            && core.browse().sort == ChannelSort::Members
            && core.browse().channels.len() == 4
            && core
                .browse()
                .channels
                .windows(2)
                .all(|w| w[0].conversation.member_count >= w[1].conversation.member_count)
    })
    .await;
    // #general has both people, #design has both now, the other two only Alice.
    let first_two: Vec<_> = bob.read(|core| browse_names(core)[..2].to_vec());
    assert!(first_two.contains(&"general".to_owned()) && first_two.contains(&"design".to_owned()));
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_long_channel_directory_pages(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = start(&world, dir.path(), "alice");
    alice.register("alice").await;
    for n in 0..52 {
        create_channel(&alice, &format!("room-{n:02}"), ChannelVisibility::Public).await;
    }

    alice.act(Input::BrowseChannels);
    alice
        .wait_until("page one", |core| {
            browse_ready(core) && core.browse().channels.len() == 50
        })
        .await;
    assert!(alice.read(|core| core.browse().has_more));
    alice.act(Input::LoadMoreBrowse);
    alice
        .wait_until("page two", |core| {
            browse_ready(core) && core.browse().channels.len() == 53
        })
        .await;
    assert!(!alice.read(|core| core.browse().has_more));
    // No channel twice, none missing: general and the 52 rooms.
    let mut names = alice.read(browse_names);
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 53);
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn the_people_directory_lists_searches_and_opens_profiles(pool: PgPool) {
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
    let bob_id = bob.read(|core| core.me().unwrap().id);

    carol.act(Input::OpenPeople);
    carol
        .wait_until("the directory", |core| {
            matches!(core.people().status, LoadStatus::Ready) && core.people().people.len() == 3
        })
        .await;
    let people = carol.read(|core| core.people().people.clone());
    let usernames: Vec<_> = people
        .iter()
        .map(|p| p.user.username.as_str().to_owned())
        .collect();
    assert_eq!(usernames, ["alice", "bob", "carol"]);
    assert_eq!(
        people[0].role,
        WorkspaceRole::Owner,
        "the first to register owns it"
    );
    assert_eq!(people[1].role, WorkspaceRole::Member);

    // Searching narrows it, on the username or the display name.
    carol.act(Input::SetPeopleQuery("BO".to_owned()));
    carol
        .wait_until("the search", |core| {
            matches!(core.people().status, LoadStatus::Ready)
                && core.people().people.len() == 1
                && core.people().people[0].user.username.as_str() == "bob"
        })
        .await;

    // A profile shows the role and when they joined, and "Message" is the existing direct open.
    carol.act(Input::OpenProfile { user_id: bob_id });
    carol
        .wait_until("the profile", move |core| {
            core.profile()
                .is_some_and(|p| p.user_id == bob_id && p.person.is_some())
        })
        .await;
    let person = carol.read(|core| core.profile().unwrap().person.clone().unwrap());
    assert_eq!(person.role, WorkspaceRole::Member);
    assert_eq!(person.user.id, bob_id);

    // Bob's status shows on his profile for others once they ask again.
    bob.act(Input::SetStatus(Some(
        lazy_domain::notification::UserStatus::new("Out today", None, None).unwrap(),
    )));
    carol
        .wait_until("his status", move |core| {
            core.member(bob_id)
                .and_then(|u| u.status.as_ref())
                .is_some_and(|s| s.text() == "Out today")
        })
        .await;
    carol.act(Input::CloseProfile);
    carol.act(Input::OpenProfile { user_id: bob_id });
    carol
        .wait_until("the profile again", move |core| {
            core.profile().is_some_and(|p| {
                p.person
                    .as_ref()
                    .and_then(|person| person.user.status.as_ref())
                    .is_some_and(|s| s.text() == "Out today")
            })
        })
        .await;
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn the_quick_switcher_finds_places_and_remembers_where_you_were(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("alice");
    let secrets = MemoryStorage::new();
    let alice = Client::start("alice", &world.server.base_url, &profile, secrets.clone());
    let bob = start(&world, dir.path(), "bob");
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    let design = create_channel(&alice, "design", ChannelVisibility::Public).await;
    let ops = create_channel(&alice, "ops-room", ChannelVisibility::Public).await;
    let bob_id = bob.read(|core| core.me().unwrap().id);
    alice
        .wait_until("Bob in the member list", move |core| {
            core.member(bob_id).is_some()
        })
        .await;

    // Matching is fuzzy over names; the best match is first and nothing outside the sidebar is
    // offered except people (to message) and the fixed places.
    let items = alice.read(|core| core.quick_switcher_results("dsg"));
    assert_eq!(
        items.first().map(|i| i.target.clone()),
        Some(QuickSwitcherTarget::Conversation(design)),
        "{items:?}"
    );
    assert!(items[0].matched.windows(2).all(|w| w[0] < w[1]));
    let items = alice.read(|core| core.quick_switcher_results("bob"));
    assert!(
        items
            .iter()
            .any(|i| i.target == QuickSwitcherTarget::Person(bob_id)),
        "a person with no conversation yet is offered: {items:?}"
    );
    let items = alice.read(|core| core.quick_switcher_results("thr"));
    assert!(
        items
            .iter()
            .any(|i| i.target == QuickSwitcherTarget::Threads)
    );

    // Choosing a person opens the direct conversation and selects it.
    alice.act(Input::ChooseQuickSwitcher(QuickSwitcherTarget::Person(
        bob_id,
    )));
    alice
        .wait_until("the direct conversation", |core| {
            core.selected_conversation().is_some_and(|c| {
                core.conversation(c)
                    .is_some_and(|c| c.kind != lazy_domain::ConversationKind::Channel)
            })
        })
        .await;
    let direct = alice.read(|core| core.selected_conversation().unwrap());
    // Now that conversation is what "bob" finds, not the person.
    let items = alice.read(|core| core.quick_switcher_results("bob"));
    assert_eq!(
        items.first().map(|i| i.target.clone()),
        Some(QuickSwitcherTarget::Conversation(direct)),
        "{items:?}"
    );

    // Visiting places makes them the first suggestions, most recent first.
    alice.act(Input::ChooseQuickSwitcher(
        QuickSwitcherTarget::Conversation(ops),
    ));
    alice.act(Input::ChooseQuickSwitcher(
        QuickSwitcherTarget::Conversation(design),
    ));
    alice
        .wait_until("design selected", move |core| {
            core.selected_conversation() == Some(design)
        })
        .await;
    let recents = alice.read(|core| core.recent_conversations().to_vec());
    assert_eq!(&recents[..3], [design, ops, direct]);
    // With no text the suggestions are the places visited, most recent first, and nothing the
    // person is not in.
    let blank: Vec<_> = alice
        .read(|core| core.quick_switcher_results(""))
        .into_iter()
        .map(|i| i.target)
        .collect();
    assert_eq!(
        &blank[..3],
        [
            QuickSwitcherTarget::Conversation(design),
            QuickSwitcherTarget::Conversation(ops),
            QuickSwitcherTarget::Conversation(direct),
        ],
        "{blank:?}"
    );
    let known: Vec<ConversationId> =
        alice.read(|core| core.conversations().iter().map(|c| c.id).collect());
    assert!(blank.iter().all(|target| match target {
        QuickSwitcherTarget::Conversation(id) => known.contains(id),
        _ => true,
    }));

    // The list survives a restart (it is a local preference), and mentions only places that exist.
    alice.stop().await;
    let alice = Client::start("alice", &world.server.base_url, &profile, secrets);
    alice
        .wait_until("the saved recents", move |core| {
            core.recent_conversations().first() == Some(&design)
        })
        .await;
    let recents = alice.read(|core| core.recent_conversations().to_vec());
    assert_eq!(&recents[..3], [design, ops, direct]);
    // The place the app opened on is the oldest of them.
    assert_eq!(recents, [design, ops, direct, general]);
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn saved_messages_follow_the_person_across_devices_and_restarts(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice_secrets = MemoryStorage::new();
    let alice_profile = dir.path().join("alice");
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &alice_profile,
        alice_secrets.clone(),
    );
    let (bob, phone) = (
        start(&world, dir.path(), "bob"),
        start(&world, dir.path(), "phone"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    sign_in_second_device(&phone, "alice").await;

    bob.send("the plan is at noon");
    bob.wait_confirmed("the plan is at noon").await;
    alice.wait_for_text("the plan is at noon").await;
    phone.wait_for_text("the plan is at noon").await;
    bob.send("second thing");
    bob.wait_confirmed("second thing").await;
    alice.wait_for_text("second thing").await;
    let plan = root_of(&alice, general, "the plan is at noon");
    let second = root_of(&alice, general, "second thing");

    // Saving flips the marker at once and reaches the person's other device, not other people.
    alice.act(Input::SetMessageSaved {
        message_id: plan,
        saved: true,
    });
    assert!(alice.read(|core| core.is_saved(plan)));
    phone
        .wait_until("the marker on the phone", move |core| core.is_saved(plan))
        .await;
    assert!(!bob.read(|core| core.is_saved(plan)));
    alice.act(Input::SetMessageSaved {
        message_id: second,
        saved: true,
    });
    phone
        .wait_until("the second marker", move |core| core.is_saved(second))
        .await;

    // The list is newest saved first.
    phone.act(Input::OpenSaved);
    phone
        .wait_until("the saved list", |core| {
            matches!(core.saved().status, LoadStatus::Ready) && core.saved().items.len() == 2
        })
        .await;
    let texts: Vec<_> = phone.read(|core| {
        core.saved()
            .items
            .iter()
            .map(|i| i.message.document.plain_text())
            .collect()
    });
    assert_eq!(texts, ["second thing", "the plan is at noon"]);
    assert!(phone.read(|core| {
        core.saved()
            .items
            .iter()
            .all(|i| i.conversation.id == general)
    }));

    // Saving again changes nothing; removing is seen by the open list on the other device.
    alice.act(Input::SetMessageSaved {
        message_id: plan,
        saved: true,
    });
    alice.act(Input::SetMessageSaved {
        message_id: second,
        saved: false,
    });
    phone
        .wait_until("the removal", move |core| {
            !core.is_saved(second) && core.is_saved(plan)
        })
        .await;
    phone.act(Input::OpenSaved);
    phone
        .wait_until("the shorter list", |core| {
            matches!(core.saved().status, LoadStatus::Ready) && core.saved().items.len() == 1
        })
        .await;

    // A restarted client learns what is saved from the bootstrap.
    alice.stop().await;
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &alice_profile,
        alice_secrets,
    );
    alice.wait_until_ready().await;
    alice
        .wait_until("the marker after a restart", move |core| {
            core.is_saved(plan)
        })
        .await;
    assert!(!alice.read(|core| core.is_saved(second)));

    // A deleted message leaves the saved list.
    bob.act(Input::DeleteMessage {
        conversation: general,
        message: plan,
    });
    alice.act(Input::OpenSaved);
    alice
        .wait_until("an empty list", |core| {
            matches!(core.saved().status, LoadStatus::Ready) && core.saved().items.is_empty()
        })
        .await;
}
