//! Phase 3G end to end: several workspaces, joining one by invitation, profile changes and
//! avatars, and appearance, with real client stacks, a real server, and real Postgres.

mod support;

use lazy_client_core::{
    ActionKind, ActionState, Appearance, ClientCore, Effect, FileBytes, FileSource, ImageState,
    Input, LoadStatus, MotionChoice, SessionPhase, ThemeChoice,
};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{ConversationId, UserId, WorkspaceId, WorkspaceRole};
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

fn id_of(client: &Client) -> UserId {
    client.read(|core| core.me().unwrap().id)
}

fn active(client: &Client) -> WorkspaceId {
    client.read(|core| core.active_workspace().expect("an active workspace"))
}

fn rail_len(core: &ClientCore) -> usize {
    core.workspace_rail().len()
}

fn conversation_in_workspace(core: &ClientCore, workspace: WorkspaceId) -> Option<ConversationId> {
    core.conversations()
        .iter()
        .find(|c| c.workspace_id == workspace && c.is_default)
        .map(|c| c.id)
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

fn failed(core: &ClientCore, kind: ActionKind) -> bool {
    matches!(core.action(kind), ActionState::Failed(_))
}

/// Creates a workspace from `client` and waits until it is the active one.
async fn create_workspace(client: &Client, name: &str) -> WorkspaceId {
    let before = active(client);
    client.act(Input::CreateWorkspace {
        name: name.to_owned(),
    });
    client
        .wait_until("the new workspace to be active", move |core| {
            core.active_workspace().is_some_and(|id| id != before)
                && *core.action(ActionKind::CreateWorkspace) == ActionState::Idle
        })
        .await;
    active(client)
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_person_runs_two_workspaces_and_each_keeps_to_itself(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice_profile = dir.path().join("alice");
    let alice_secrets = MemoryStorage::new();
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &alice_profile,
        alice_secrets.clone(),
    );
    let bob = start(&world, dir.path(), "bob");
    alice.register("alice").await;
    bob.register("bob").await;
    let (alice_id, bob_id) = (id_of(&alice), id_of(&bob));
    alice
        .wait_until("Bob in the member list", move |core| {
            core.member(bob_id).is_some()
        })
        .await;
    let home = active(&alice);

    // Alice makes a second workspace: it is hers alone, with its own default channel.
    let acme = create_workspace(&alice, "Acme").await;
    assert_eq!(alice.read(rail_len), 2);
    assert_eq!(alice.read(|c| c.my_role()), Some(WorkspaceRole::Owner));
    assert!(
        alice.read(|c| c.member(bob_id).is_none()),
        "Bob is not in Acme"
    );
    let acme_general = alice.read(|c| conversation_in_workspace(c, acme)).unwrap();
    assert_ne!(
        Some(acme_general),
        alice.read(|c| conversation_in_workspace(c, home))
    );
    assert_eq!(bob.read(rail_len), 1);
    assert!(
        bob.read(|c| c.conversations().iter().all(|c| c.workspace_id != acme)),
        "nothing of Acme reaches Bob"
    );
    // The rail names both, with the new one active.
    let rail = alice.read(|c| c.workspace_rail());
    assert_eq!(rail.iter().filter(|r| r.active).count(), 1);
    assert!(
        rail.iter()
            .any(|r| r.id == acme && r.active && r.name == "Acme")
    );

    // Alice, looking at Acme, is reached in the other workspace: it shows on the rail and in the
    // total, not in what Acme shows, and the notification says which workspace it came from.
    let alice_in_home = alice
        .read(|c| c.workspace_rail())
        .iter()
        .find(|r| r.id == home)
        .cloned();
    assert_eq!(alice_in_home.map(|r| r.attention), Some(0));
    bob.act(Input::OpenConversation {
        participants: vec![alice_id],
    });
    bob.wait_until("the direct conversation", |core| {
        core.selected_conversation().is_some_and(|c| {
            core.conversation(c)
                .is_some_and(|c| c.kind != lazy_domain::ConversationKind::Channel)
        })
    })
    .await;
    bob.send("are you there?");
    alice
        .wait_until("attention in the other workspace", move |core| {
            core.workspace_rail()
                .iter()
                .any(|r| r.id == home && r.attention == 1)
        })
        .await;
    assert_eq!(alice.read(|c| c.attention_total()), 1);
    assert_eq!(
        alice.read(|c| c.attention_count()),
        0,
        "the active workspace is quiet"
    );
    let titles: Vec<String> = alice
        .effects()
        .iter()
        .filter_map(|e| match e {
            Effect::Notify(n) if n.body.contains("are you there?") => Some(n.title.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(titles.len(), 1, "{titles:?}");
    let home_name = alice
        .read(|c| {
            c.workspaces()
                .iter()
                .find(|w| w.workspace.id == home)
                .map(|w| w.workspace.name.clone())
        })
        .unwrap();
    assert!(
        titles[0].contains(&home_name),
        "{titles:?} should name {home_name}"
    );

    // Switching shows that workspace's state, and the other keeps its own.
    alice.act(Input::SwitchWorkspace(home));
    alice
        .wait_until("the home workspace", move |core| {
            core.active_workspace() == Some(home) && core.attention_count() == 1
        })
        .await;
    assert!(alice.read(|c| c.member(bob_id).is_some()));
    alice.act(Input::SwitchWorkspace(acme));
    alice
        .wait_until("Acme again", move |core| {
            core.active_workspace() == Some(acme) && core.member(bob_id).is_none()
        })
        .await;

    // A restart opens on the workspace left active.
    alice.stop().await;
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &alice_profile,
        alice_secrets,
    );
    alice
        .wait_until("the saved workspace", move |core| {
            core.active_workspace() == Some(acme)
        })
        .await;
    alice.wait_until_ready().await;
    assert_eq!(alice.read(rail_len), 2);
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn an_invitation_brings_an_existing_account_into_another_workspace(pool: PgPool) {
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
    let (alice_id, bob_id) = (id_of(&alice), id_of(&bob));
    let acme = create_workspace(&alice, "Acme").await;

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
        .wait_until("the code", |core| core.invitations().created.is_some())
        .await;
    let code = alice
        .read(|c| c.invitations().created.clone().unwrap())
        .code
        .expose()
        .to_owned();

    // A code that cannot be right is refused here; Bob's real one lets him in as a member and he
    // lands in Acme, with Alice in its member list.
    bob.act(Input::AcceptInvitation {
        code: "short".to_owned(),
    });
    bob.wait_until("a local refusal", |core| {
        failed(core, ActionKind::AcceptInvitation)
    })
    .await;
    bob.act(Input::AcceptInvitation { code: code.clone() });
    bob.wait_until("Acme for Bob", move |core| {
        core.active_workspace() == Some(acme) && core.workspace_rail().len() == 2
    })
    .await;
    assert_eq!(bob.read(|c| c.my_role()), Some(WorkspaceRole::Member));
    bob.wait_until("Alice in Acme", move |c| c.member(alice_id).is_some())
        .await;
    alice
        .wait_until("Bob joined Acme", move |c| c.member(bob_id).is_some())
        .await;

    // Using the same code again changes nothing; the one use is spent for everyone else.
    bob.act(Input::AcceptInvitation { code: code.clone() });
    bob.wait_until("settled", |core| {
        *core.action(ActionKind::AcceptInvitation) == ActionState::Idle
    })
    .await;
    assert_eq!(bob.read(rail_len), 2);
    carol.act(Input::AcceptInvitation { code });
    carol
        .wait_until("a refusal for Carol", |core| {
            failed(core, ActionKind::AcceptInvitation)
        })
        .await;
    assert_eq!(carol.read(rail_len), 1);
    assert!(
        carol.read(|c| c.conversations().iter().all(|c| c.workspace_id != acme)),
        "Carol sees nothing of Acme"
    );
    alice.act(Input::CloseInvitations);
    alice.act(Input::OpenInvitations);
    alice
        .wait_until("one use counted", |core| {
            core.invitations().items.iter().any(|i| i.uses == 1)
        })
        .await;
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_profile_change_reaches_every_shared_workspace_and_the_avatar_loads(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let bob_id = id_of(&bob);
    let home = active(&alice);
    alice
        .wait_until("Bob in the member list", move |c| {
            c.member(bob_id).is_some()
        })
        .await;
    let acme = create_workspace(&alice, "Acme").await;
    alice.act(Input::OpenInvitations);
    alice
        .wait_until("the invitations", |c| {
            matches!(c.invitations().status, LoadStatus::Ready)
        })
        .await;
    alice.act(Input::CreateInvitation {
        expires_in_secs: None,
        max_uses: None,
    });
    alice
        .wait_until("the code", |c| c.invitations().created.is_some())
        .await;
    let code = alice
        .read(|c| c.invitations().created.clone().unwrap())
        .code
        .expose()
        .to_owned();
    bob.act(Input::AcceptInvitation { code });
    bob.wait_until("Acme for Bob", move |c| c.active_workspace() == Some(acme))
        .await;
    alice
        .wait_until("Bob in Acme", move |c| c.member(bob_id).is_some())
        .await;

    // A new name is seen in both workspaces.
    bob.act(Input::UpdateProfile {
        display_name: Some("Robert Okafor".to_owned()),
    });
    alice
        .wait_until("the new name in Acme", move |c| {
            c.member(bob_id)
                .is_some_and(|u| u.display_name.as_str() == "Robert Okafor")
        })
        .await;
    alice.act(Input::SwitchWorkspace(home));
    alice
        .wait_until("the new name at home", move |c| {
            c.active_workspace() == Some(home)
                && c.member(bob_id)
                    .is_some_and(|u| u.display_name.as_str() == "Robert Okafor")
        })
        .await;

    // A picture: Bob uploads one, the version moves for Alice, and she can load it.
    bob.act(Input::SetAvatar {
        source: FileSource::Bytes(FileBytes::new(png(640, 480))),
    });
    alice
        .wait_until("an avatar version", move |c| {
            c.member(bob_id).is_some_and(|u| u.avatar_version > 0)
        })
        .await;
    alice.act(Input::LoadAvatar(bob_id));
    alice
        .wait_until("the avatar", move |c| {
            matches!(c.avatar(bob_id), Some(ImageState::Ready(_)))
        })
        .await;
    // Something that is not a picture is refused and changes nothing.
    let before = alice.read(|c| c.member(bob_id).map(|u| u.avatar_version));
    bob.act(Input::SetAvatar {
        source: FileSource::Bytes(FileBytes::new(b"definitely not an image".to_vec())),
    });
    bob.wait_until("a refusal", |c| failed(c, ActionKind::SetAvatar))
        .await;
    assert_eq!(
        alice.read(|c| c.member(bob_id).map(|u| u.avatar_version)),
        before
    );
    bob.act(Input::RemoveAvatar);
    alice
        .wait_until("the avatar gone", move |c| {
            c.member(bob_id).is_some_and(|u| u.avatar_version == 0)
        })
        .await;
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn appearance_is_kept_on_the_device(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("alice");
    let secrets = MemoryStorage::new();
    let alice = Client::start("alice", &world.server.base_url, &profile, secrets.clone());
    alice.register("alice").await;
    assert_eq!(alice.read(|c| c.appearance()), Appearance::default());
    let chosen = Appearance {
        theme: ThemeChoice::Dark,
        motion: MotionChoice::Reduce,
    };
    alice.act(Input::SetAppearance(chosen));
    assert_eq!(alice.read(|c| c.appearance()), chosen);
    alice.stop().await;

    let alice = Client::start("alice", &world.server.base_url, &profile, secrets);
    alice
        .wait_until("the saved appearance", move |c| c.appearance() == chosen)
        .await;
    // Another device has its own.
    let phone = start(&world, dir.path(), "phone");
    phone
        .wait_until("the sign-in form", |c| {
            matches!(c.phase(), SessionPhase::SignedOut { .. })
        })
        .await;
    phone.act(Input::SignIn {
        username: "alice".to_owned(),
        password: PASSWORD.to_owned(),
    });
    phone.wait_until_ready().await;
    assert_eq!(phone.read(|c| c.appearance()), Appearance::default());
}
