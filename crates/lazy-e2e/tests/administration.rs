//! Phase 3F end to end: roles, workspace settings, invitations, and deactivation, with real client
//! stacks, a real server, and real Postgres.

mod support;

use lazy_client_core::{ActionKind, ActionState, ClientCore, Input, LoadStatus, SessionPhase};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::workspace::{ChannelCreationPolicy, RegistrationPolicy, WorkspaceSettings};
use lazy_domain::{ChannelVisibility, ConversationId, UserId, WorkspaceRole};
use lazy_protocol::InvitationState;
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

fn id_of(client: &Client) -> UserId {
    client.read(|core| core.me().unwrap().id)
}

/// Every listed client knows every listed person (members arrive by event after registering).
async fn know_each_other(clients: &[(&Client, UserId)], everyone: &[UserId]) {
    for (client, me) in clients {
        let wanted: Vec<UserId> = everyone.iter().copied().filter(|id| id != me).collect();
        client
            .wait_until("the other members", move |core| {
                wanted.iter().all(|id| core.member(*id).is_some())
            })
            .await;
    }
}

fn failed(core: &ClientCore, kind: ActionKind) -> bool {
    matches!(core.action(kind), ActionState::Failed(_))
}

fn settings(
    registration: RegistrationPolicy,
    channels: ChannelCreationPolicy,
) -> WorkspaceSettings {
    WorkspaceSettings {
        registration,
        channel_creation: channels,
    }
}

fn register_with(client: &Client, username: &str, code: &str) {
    client.act(Input::Register {
        username: username.to_owned(),
        display_name: format!("User {username}"),
        password: PASSWORD.to_owned(),
        invite_code: code.to_owned(),
    });
}

async fn refused_with(client: &Client, words: &'static str) {
    client
        .wait_until("a refusal", move |core| {
            matches!(
                core.phase(),
                SessionPhase::SignedOut { last_error: Some(message) } if message.contains(words)
            )
        })
        .await;
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn roles_and_settings_follow_the_rules_and_reach_everyone(pool: PgPool) {
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
    let (alice_id, bob_id, carol_id) = (id_of(&alice), id_of(&bob), id_of(&carol));
    know_each_other(
        &[(&alice, alice_id), (&bob, bob_id), (&carol, carol_id)],
        &[alice_id, bob_id, carol_id],
    )
    .await;
    assert_eq!(alice.read(|c| c.my_role()), Some(WorkspaceRole::Owner));
    assert_eq!(carol.read(|c| c.my_role()), Some(WorkspaceRole::Member));
    assert!(!carol.read(|c| c.can_manage_workspace()));

    // Carol is watching the directory when Alice promotes Bob: Bob's own client learns what he
    // may now do, and Carol's list changes in place.
    carol.act(Input::OpenPeople);
    carol
        .wait_until("the directory", |core| {
            matches!(core.people().status, LoadStatus::Ready) && core.people().people.len() == 3
        })
        .await;
    alice.act(Input::SetMemberRole {
        user_id: bob_id,
        role: WorkspaceRole::Admin,
    });
    bob.wait_until("his new role", |core| {
        core.my_role() == Some(WorkspaceRole::Admin) && core.can_manage_workspace()
    })
    .await;
    carol
        .wait_until("Bob's badge", move |core| {
            core.people()
                .people
                .iter()
                .any(|p| p.user.id == bob_id && p.role == WorkspaceRole::Admin)
        })
        .await;

    // An admin never touches an owner or makes one.
    bob.act(Input::SetMemberRole {
        user_id: alice_id,
        role: WorkspaceRole::Member,
    });
    bob.wait_until("a refusal", move |core| {
        failed(core, ActionKind::SetMemberRole(alice_id))
    })
    .await;
    bob.act(Input::SetMemberRole {
        user_id: carol_id,
        role: WorkspaceRole::Owner,
    });
    bob.wait_until("a second refusal", move |core| {
        failed(core, ActionKind::SetMemberRole(carol_id))
    })
    .await;
    // A workspace keeps its last owner, even against its owner.
    alice.act(Input::SetMemberRole {
        user_id: alice_id,
        role: WorkspaceRole::Member,
    });
    alice
        .wait_until("the last-owner refusal", move |core| {
            failed(core, ActionKind::SetMemberRole(alice_id))
        })
        .await;
    assert_eq!(alice.read(|c| c.my_role()), Some(WorkspaceRole::Owner));

    // An admin changes the name and the policy; everyone sees both at once, and a member who
    // tries anyway is refused.
    bob.act(Input::UpdateWorkspace {
        name: Some("Acme Corp".to_owned()),
        settings: Some(settings(
            RegistrationPolicy::Open,
            ChannelCreationPolicy::AdminsOnly,
        )),
    });
    carol
        .wait_until("the new name and policy", |core| {
            core.primary_workspace()
                .is_some_and(|w| w.workspace.name == "Acme Corp")
                && !core.can_create_channel()
        })
        .await;
    assert!(alice.read(|c| c.can_create_channel()));
    carol.act(Input::UpdateWorkspace {
        name: Some("Mine now".to_owned()),
        settings: None,
    });
    carol
        .wait_until("a refusal", |core| {
            failed(core, ActionKind::UpdateWorkspace)
        })
        .await;
    carol.act(Input::CreateChannel {
        name: "nope".to_owned(),
        visibility: ChannelVisibility::Public,
        topic: String::new(),
        description: String::new(),
    });
    carol
        .wait_until("a channel refusal", |core| {
            failed(core, ActionKind::CreateChannel)
        })
        .await;
    assert!(alice.read(|c| {
        c.conversations()
            .iter()
            .all(|c| c.name.as_ref().is_none_or(|n| n.as_str() != "nope"))
    }));
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn invitations_let_people_in_once_and_only_while_valid(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = start(&world, dir.path(), "alice");
    alice.register("alice").await;

    alice.act(Input::UpdateWorkspace {
        name: None,
        settings: Some(settings(
            RegistrationPolicy::InviteOnly,
            ChannelCreationPolicy::Everyone,
        )),
    });
    alice
        .wait_until("invitation-only", |core| {
            core.workspace_settings().registration == RegistrationPolicy::InviteOnly
        })
        .await;

    // Without a code, or with one that is not an invitation, nobody registers.
    let dave = start(&world, dir.path(), "dave");
    dave.wait_until("the sign-in form", |core| {
        matches!(core.phase(), SessionPhase::SignedOut { .. })
    })
    .await;
    register_with(&dave, "dave", "");
    refused_with(&dave, "invite-only").await;
    register_with(&dave, "dave", &"a".repeat(30));
    refused_with(&dave, "isn't valid").await;
    // A code that cannot be right is refused before any request.
    register_with(&dave, "dave", "short");
    refused_with(&dave, "valid invite code").await;

    // An invitation for one person.
    alice.act(Input::OpenInvitations);
    alice
        .wait_until("the invitations", |core| {
            matches!(core.invitations().status, LoadStatus::Ready)
        })
        .await;
    alice.act(Input::CreateInvitation {
        expires_in_secs: Some(3600),
        max_uses: Some(1),
    });
    alice
        .wait_until("the new code", |core| core.invitations().created.is_some())
        .await;
    let first = alice.read(|core| core.invitations().created.clone().unwrap());
    assert_eq!(first.invitation.max_uses, Some(1));
    assert_eq!(first.invitation.state, InvitationState::Active);

    register_with(&dave, "dave", first.code.expose());
    dave.wait_until_ready().await;
    assert_eq!(dave.read(|c| c.my_role()), Some(WorkspaceRole::Member));
    let general_id = general(&dave);
    assert!(dave.read(|c| c.conversation(general_id).is_some()));
    assert_eq!(
        dave.read(|c| c.primary_workspace().map(|w| w.workspace.id)),
        alice.read(|c| c.primary_workspace().map(|w| w.workspace.id))
    );

    // It is used up: the same code brings in no one else, and says only that it is not valid.
    let erin = start(&world, dir.path(), "erin");
    erin.wait_until("the sign-in form", |core| {
        matches!(core.phase(), SessionPhase::SignedOut { .. })
    })
    .await;
    register_with(&erin, "erin", first.code.expose());
    refused_with(&erin, "isn't valid").await;

    // The list says so; a revoked one stops working too.
    alice.act(Input::DismissCreatedInvitation);
    alice.act(Input::CloseInvitations);
    alice.act(Input::OpenInvitations);
    alice
        .wait_until("the used-up invitation", |core| {
            core.invitations()
                .items
                .iter()
                .any(|i| i.state == InvitationState::UsedUp && i.uses == 1 && i.max_uses == Some(1))
        })
        .await;
    alice.act(Input::CreateInvitation {
        expires_in_secs: None,
        max_uses: None,
    });
    alice
        .wait_until("the second code", |core| {
            core.invitations().created.is_some()
        })
        .await;
    let second = alice.read(|core| core.invitations().created.clone().unwrap());
    alice.act(Input::RevokeInvitation(second.invitation.id));
    alice
        .wait_until("the revoked invitation", move |core| {
            core.invitations()
                .items
                .iter()
                .any(|i| i.id == second.invitation.id && i.state == InvitationState::Revoked)
        })
        .await;
    register_with(&erin, "erin", second.code.expose());
    refused_with(&erin, "isn't valid").await;
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_deactivated_person_loses_access_and_gets_it_back(pool: PgPool) {
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
    let (alice_id, bob_id, carol_id) = (id_of(&alice), id_of(&bob), id_of(&carol));
    know_each_other(
        &[(&alice, alice_id), (&bob, bob_id), (&carol, carol_id)],
        &[alice_id, bob_id, carol_id],
    )
    .await;
    let general = general(&alice);
    bob.send("hello from bob");
    bob.wait_confirmed("hello from bob").await;
    carol.wait_for_text("hello from bob").await;
    carol
        .wait_until("Bob offered for a mention", move |core| {
            core.mention_candidates(general, "bo")
                .iter()
                .any(|c| c.handle == "bob")
        })
        .await;

    // Nobody deactivates themself, and a member cannot deactivate anyone.
    alice.act(Input::SetMemberDeactivated {
        user_id: alice_id,
        deactivated: true,
    });
    alice
        .wait_until("a refusal", move |core| {
            failed(core, ActionKind::SetMemberDeactivated(alice_id))
        })
        .await;
    carol.act(Input::SetMemberDeactivated {
        user_id: bob_id,
        deactivated: true,
    });
    carol
        .wait_until("a refusal", move |core| {
            failed(core, ActionKind::SetMemberDeactivated(bob_id))
        })
        .await;

    // The owner deactivates Bob: his client is told he has no access, the others see him flagged
    // and stop offering him, and what is said afterwards does not reach him.
    alice.act(Input::SetMemberDeactivated {
        user_id: bob_id,
        deactivated: true,
    });
    bob.wait_until("no access", |core| core.access_lost().is_some())
        .await;
    carol
        .wait_until("Bob flagged", move |core| {
            core.member(bob_id).is_some_and(|u| u.deactivated)
        })
        .await;
    assert!(
        carol
            .read(|core| core.mention_candidates(general, "bo"))
            .iter()
            .all(|c| c.handle != "bob")
    );
    carol.send("said while Bob is away");
    carol.wait_confirmed("said while Bob is away").await;
    alice.wait_for_text("said while Bob is away").await;
    assert!(
        !bob.read(|core| core
            .timeline(general)
            .iter()
            .any(|item| item.document.plain_text() == "said while Bob is away")),
        "a deactivated person receives nothing"
    );

    // The owner can list him; others cannot.
    alice.act(Input::OpenPeople);
    alice
        .wait_until("the directory to open", |core| {
            matches!(core.people().status, LoadStatus::Ready)
        })
        .await;
    assert!(
        alice.read(|core| core.people().people.iter().all(|p| p.user.id != bob_id)),
        "deactivated people are not listed unless asked for"
    );
    alice.act(Input::SetPeopleIncludeDeactivated(true));
    alice
        .wait_until("the directory with him", move |core| {
            matches!(core.people().status, LoadStatus::Ready)
                && core.people().people.iter().any(|p| p.user.id == bob_id)
        })
        .await;
    let listed = alice.read(|core| {
        core.people()
            .people
            .iter()
            .map(|p| (p.user.username.to_string(), p.user.deactivated))
            .collect::<Vec<_>>()
    });
    assert!(listed.contains(&("bob".to_owned(), true)), "{listed:?}");
    alice.act(Input::SetPeopleIncludeDeactivated(false));
    alice
        .wait_until("the directory without him", move |core| {
            matches!(core.people().status, LoadStatus::Ready)
                && core.people().people.iter().all(|p| p.user.id != bob_id)
        })
        .await;

    // Reactivated, he signs in on a fresh device and finds his place: the old messages under his
    // name, and what was said while he was away.
    alice.act(Input::SetMemberDeactivated {
        user_id: bob_id,
        deactivated: false,
    });
    carol
        .wait_until("Bob active again", move |core| {
            core.member(bob_id).is_some_and(|u| !u.deactivated)
        })
        .await;
    let bob_phone = start(&world, dir.path(), "bob-phone");
    bob_phone
        .wait_until("the sign-in form", |core| {
            matches!(core.phase(), SessionPhase::SignedOut { .. })
        })
        .await;
    bob_phone.act(Input::SignIn {
        username: "bob".to_owned(),
        password: PASSWORD.to_owned(),
    });
    bob_phone.wait_until_ready().await;
    bob_phone.wait_for_text("said while Bob is away").await;
    bob_phone.wait_for_text("hello from bob").await;
    assert_eq!(bob_phone.read(|c| c.my_role()), Some(WorkspaceRole::Member));
}
