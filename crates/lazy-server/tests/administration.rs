//! Workspace administration: settings, roles, deactivation, invitations (D22).
//!
//! Deactivation is a security boundary, so the central test asks the same questions as a
//! deactivated person and as someone who was never in the workspace, in every family of endpoint,
//! and requires the same answers.

mod common;

use common::*;
use futures_util::future::join_all;
use lazy_domain::workspace::{
    ChannelCreationPolicy, RegistrationPolicy, WorkspaceSettings, check_member_action,
    check_role_change,
};
use lazy_domain::{ConversationId, InvitationId, OperationId, UserId, WorkspaceId, WorkspaceRole};
use lazy_protocol::{ErrorCode, Event, EventEnvelope, InvitationState, PROTOCOL_VERSION};
use serde_json::json;
use sqlx::PgPool;

struct World {
    app: App,
    owner: Account,
    admin: Account,
    bob: Account,
    carol: Account,
    dave: Account,
}

async fn world(pool: PgPool) -> World {
    let app = App::start(pool).await;
    let owner = app.register("owner").await;
    let admin = app.register("admin").await;
    app.set_role(&admin, "admin").await;
    let bob = app.register("bob").await;
    let carol = app.register("carol").await;
    let dave = app.register("dave").await;
    World {
        app,
        owner,
        admin,
        bob,
        carol,
        dave,
    }
}

async fn finish(world: World) {
    world.app.server.shutdown().await.unwrap();
}

async fn denied(response: reqwest::Response, code: ErrorCode) -> lazy_protocol::ApiError {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
    error
}

async fn listener(app: &App, who: &Account) -> Ws {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, app.head().await).await;
    ws.replay().await;
    ws
}

/// Someone who was never in the workspace (registered, then taken out).
async fn outsider(app: &App, name: &str) -> Account {
    let account = app.register(name).await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(account.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    account
}

async fn role_of(app: &App, who: &Account) -> String {
    sqlx::query_scalar(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(app.workspace_id.as_uuid())
    .bind(who.user.id.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap()
}

async fn set_roles(app: &App, roles: &[(&Account, &str)]) {
    for (who, role) in roles {
        sqlx::query("UPDATE workspace_members SET role = $3, deactivated_at = NULL WHERE workspace_id = $1 AND user_id = $2")
            .bind(app.workspace_id.as_uuid())
            .bind(who.user.id.as_uuid())
            .bind(*role)
            .execute(&app.pool)
            .await
            .unwrap();
    }
}

fn role_name(role: WorkspaceRole) -> &'static str {
    match role {
        WorkspaceRole::Owner => "owner",
        WorkspaceRole::Admin => "admin",
        WorkspaceRole::Member => "member",
    }
}

const ROLES: [WorkspaceRole; 3] = [
    WorkspaceRole::Member,
    WorkspaceRole::Admin,
    WorkspaceRole::Owner,
];

fn admin_events(events: &[EventEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::WorkspaceUpdated { workspace } => Some(format!("workspace:{}", workspace.name)),
            Event::MemberRoleChanged { user_id, role } => {
                Some(format!("role:{user_id}:{}", role_name(*role)))
            }
            Event::MemberDeactivated {
                user_id,
                deactivated,
            } => Some(format!("deactivated:{user_id}:{deactivated}")),
            _ => None,
        })
        .collect()
}

// --- Roles -----------------------------------------------------------------------------------

#[sqlx::test]
async fn role_changes_follow_the_domain_table_through_the_api(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let (actor, target, spare) = (&w.bob, &w.carol, &w.dave);
    for actor_role in ROLES {
        for target_role in ROLES {
            for new_role in ROLES {
                for spare_role in ["member", "owner", "deactivated owner"] {
                    // The original owner and admin are out of the picture (members) for this case.
                    set_roles(
                        app,
                        &[
                            (&w.owner, "member"),
                            (&w.admin, "member"),
                            (actor, role_name(actor_role)),
                            (target, role_name(target_role)),
                            (spare, spare_role.trim_start_matches("deactivated ")),
                        ],
                    )
                    .await;
                    if spare_role == "deactivated owner" {
                        // An owner who is deactivated does not count as one that remains.
                        sqlx::query("UPDATE workspace_members SET deactivated_at = now() WHERE workspace_id = $1 AND user_id = $2")
                            .bind(app.workspace_id.as_uuid())
                            .bind(spare.user.id.as_uuid())
                            .execute(&app.pool)
                            .await
                            .unwrap();
                    }
                    let other_owners = usize::from(spare_role == "owner")
                        + usize::from(actor_role == WorkspaceRole::Owner);
                    let want = check_role_change(actor_role, target_role, new_role, other_owners);
                    let response = app
                        .set_role_raw(actor, app.workspace_id, target.user.id, role_name(new_role))
                        .await;
                    let label = format!(
                        "{actor_role:?} sets {target_role:?} to {new_role:?} ({other_owners} others)"
                    );
                    match want {
                        Ok(()) => {
                            assert_eq!(response.status().as_u16(), 200, "{label}");
                            let person: lazy_protocol::PersonResponse =
                                response.json().await.unwrap();
                            assert_eq!(person.person.role, new_role, "{label}");
                            assert_eq!(role_of(app, target).await, role_name(new_role), "{label}");
                        }
                        Err(lazy_domain::workspace::RoleChangeError::NotAllowed) => {
                            denied(response, ErrorCode::Forbidden).await;
                            assert_eq!(
                                role_of(app, target).await,
                                role_name(target_role),
                                "{label}"
                            );
                        }
                        Err(lazy_domain::workspace::RoleChangeError::LastOwner) => {
                            denied(response, ErrorCode::Conflict).await;
                            assert_eq!(
                                role_of(app, target).await,
                                role_name(target_role),
                                "{label}"
                            );
                        }
                    }
                }
            }
        }
    }
    finish(w).await;
}

#[sqlx::test]
async fn a_role_change_is_announced_once_to_everyone_and_a_repeat_is_nothing(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let stranger = outsider(app, "stranger").await;
    let before = app.head().await;
    let mut bob = listener(app, &w.bob).await;
    let mut carol = listener(app, &w.carol).await;
    let response = app
        .set_role_raw(&w.owner, app.workspace_id, w.bob.user.id, "admin")
        .await;
    assert!(response.status().is_success());
    let head = app.head().await;
    assert_eq!(head.get(), before.get() + 1);
    // Again: nothing, from the owner and from an admin who could have done it.
    assert!(
        app.set_role_raw(&w.owner, app.workspace_id, w.bob.user.id, "admin")
            .await
            .status()
            .is_success()
    );
    assert!(
        app.set_role_raw(&w.admin, app.workspace_id, w.bob.user.id, "admin")
            .await
            .status()
            .is_success()
    );
    assert_eq!(app.head().await, head);
    let marker = app.register("marker").await;
    let expected = vec![format!("role:{}:admin", w.bob.user.id)];
    // The person themself hears it too, as does everyone else.
    assert_eq!(
        admin_events(&bob.events_until_marker(marker.user.id).await),
        expected
    );
    assert_eq!(
        admin_events(&carol.events_until_marker(marker.user.id).await),
        expected
    );
    // The bootstrap and the directory say the same.
    let boot = app.bootstrap_ok(&w.bob).await;
    assert_eq!(boot.workspaces[0].role, WorkspaceRole::Admin);
    assert_eq!(
        app.people(&w.carol, &[("query", "bob")]).await.people[0].role,
        WorkspaceRole::Admin
    );
    // Refusals: a stranger, an unknown person, an app, a bad role, a bad body.
    denied(
        app.set_role_raw(&stranger, app.workspace_id, w.bob.user.id, "member")
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.set_role_raw(&w.owner, app.workspace_id, UserId::generate(), "member")
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.set_role_raw(&w.owner, WorkspaceId::generate(), w.bob.user.id, "member")
            .await,
        ErrorCode::NotFound,
    )
    .await;
    let hook = app.create_webhook(&w.owner, app.general, "Deploybot").await;
    denied(
        app.set_role_raw(
            &w.owner,
            app.workspace_id,
            hook.webhook.bot_user_id,
            "admin",
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        app.set_role_raw(&w.owner, app.workspace_id, w.bob.user.id, "emperor")
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        app.set_role_raw(&w.carol, app.workspace_id, w.bob.user.id, "member")
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    let unsigned = app
        .http
        .put(app.url(&lazy_protocol::api::paths::person_role(
            app.workspace_id,
            w.bob.user.id,
        )))
        .json(&json!({"role": "member"}))
        .send()
        .await
        .unwrap();
    denied(unsigned, ErrorCode::Unauthenticated).await;
    // A deactivated person's role can be set.
    app.deactivate(&w.owner, &w.dave, true).await;
    assert!(
        app.set_role_raw(&w.owner, app.workspace_id, w.dave.user.id, "admin")
            .await
            .status()
            .is_success()
    );
    assert_eq!(role_of(app, &w.dave).await, "admin");
    finish(w).await;
}

#[sqlx::test]
async fn the_last_active_owner_cannot_leave_even_when_a_deactivated_owner_remains(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let (a, b) = (&w.bob, &w.carol);
    set_roles(
        app,
        &[
            (&w.owner, "member"),
            (&w.admin, "member"),
            (a, "owner"),
            (b, "owner"),
        ],
    )
    .await;
    // With another active owner an owner may step down.
    assert!(
        app.set_role_raw(a, app.workspace_id, a.user.id, "admin")
            .await
            .status()
            .is_success()
    );
    set_roles(app, &[(a, "owner")]).await;
    // With the other owner deactivated, the one active owner cannot: nobody would be left.
    app.deactivate(a, b, true).await;
    let refused = app
        .set_role_raw(a, app.workspace_id, a.user.id, "member")
        .await;
    let error = denied(refused, ErrorCode::Conflict).await;
    assert_eq!(error.message, "a workspace needs at least one owner");
    assert_eq!(role_of(app, a).await, "owner");
    // Reactivating the other lifts it.
    app.deactivate(a, b, false).await;
    assert!(
        app.set_role_raw(a, app.workspace_id, a.user.id, "member")
            .await
            .status()
            .is_success()
    );
    finish(w).await;
}

#[sqlx::test]
async fn two_owners_changing_each_other_at_once_leave_an_owner(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let (a, b) = (&w.bob, &w.carol);
    for round in 0..10 {
        set_roles(
            app,
            &[
                (&w.owner, "member"),
                (&w.admin, "member"),
                (&w.dave, "member"),
                (a, "owner"),
                (b, "owner"),
            ],
        )
        .await;
        // Each demotes the other, or each demotes themself, or one deactivates the other.
        let (r1, r2) = match round % 3 {
            0 => tokio::join!(
                app.set_role_raw(a, app.workspace_id, b.user.id, "member"),
                app.set_role_raw(b, app.workspace_id, a.user.id, "member"),
            ),
            1 => tokio::join!(
                app.set_role_raw(a, app.workspace_id, a.user.id, "member"),
                app.set_role_raw(b, app.workspace_id, b.user.id, "member"),
            ),
            _ => tokio::join!(
                app.set_deactivated_raw(a, app.workspace_id, b.user.id, true),
                app.set_deactivated_raw(b, app.workspace_id, a.user.id, true),
            ),
        };
        let statuses = [r1.status().as_u16(), r2.status().as_u16()];
        assert_eq!(
            statuses.iter().filter(|s| **s == 200).count(),
            1,
            "round {round}: {statuses:?}"
        );
        assert!(
            statuses.iter().all(|s| matches!(*s, 200 | 403 | 404 | 409)),
            "{statuses:?}"
        );
        let owners: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM workspace_members WHERE workspace_id = $1 AND role = 'owner' AND deactivated_at IS NULL",
        )
        .bind(app.workspace_id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert!(owners >= 1, "round {round}: no active owner left");
    }
    finish(w).await;
}

// --- Deactivation ----------------------------------------------------------------------------

#[sqlx::test]
async fn deactivation_follows_the_domain_table_and_nobody_deactivates_themself(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let (actor, target) = (&w.bob, &w.carol);
    for actor_role in ROLES {
        for target_role in ROLES {
            set_roles(
                app,
                &[
                    (&w.owner, "member"),
                    (&w.admin, "member"),
                    (actor, role_name(actor_role)),
                    (target, role_name(target_role)),
                ],
            )
            .await;
            let want = check_member_action(actor_role, target_role, false).is_ok();
            let response = app
                .set_deactivated_raw(actor, app.workspace_id, target.user.id, true)
                .await;
            assert_eq!(
                response.status().is_success(),
                want,
                "{actor_role:?} on {target_role:?}"
            );
            if want {
                let person: lazy_protocol::PersonResponse = response.json().await.unwrap();
                assert!(person.person.user.deactivated);
                // Reactivating is held to the same rule.
                let back = app
                    .set_deactivated_raw(actor, app.workspace_id, target.user.id, false)
                    .await;
                assert!(back.status().is_success());
            } else {
                denied(response, ErrorCode::Forbidden).await;
            }
        }
        // Nobody deactivates themself, an owner included.
        set_roles(app, &[(actor, role_name(actor_role))]).await;
        denied(
            app.set_deactivated_raw(actor, app.workspace_id, actor.user.id, true)
                .await,
            ErrorCode::Forbidden,
        )
        .await;
    }
    // A stranger, an unknown person, an app.
    let stranger = outsider(app, "stranger").await;
    set_roles(app, &[(&w.owner, "owner")]).await;
    denied(
        app.set_deactivated_raw(&stranger, app.workspace_id, w.bob.user.id, true)
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.set_deactivated_raw(&w.owner, app.workspace_id, UserId::generate(), true)
            .await,
        ErrorCode::NotFound,
    )
    .await;
    let hook = app.create_webhook(&w.owner, app.general, "Deploybot").await;
    denied(
        app.set_deactivated_raw(&w.owner, app.workspace_id, hook.webhook.bot_user_id, true)
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    finish(w).await;
}

#[sqlx::test]
async fn deactivating_announces_to_the_others_ends_the_persons_subscription_and_a_repeat_is_nothing(
    pool: PgPool,
) {
    let w = world(pool).await;
    let app = &w.app;
    let before = app.head().await;
    let mut dave = listener(app, &w.dave).await;
    let mut other_device = listener(app, &w.dave).await;
    let mut carol = listener(app, &w.carol).await;
    app.deactivate(&w.admin, &w.dave, true).await;
    let head = app.head().await;
    assert_eq!(head.get(), before.get() + 1);
    app.deactivate(&w.admin, &w.dave, true).await;
    assert_eq!(app.head().await, head, "again is nothing");
    // The others hear it; the person's connections end with the refusal a stranger would get.
    let marker = app.register("marker").await;
    assert_eq!(
        admin_events(&carol.events_until_marker(marker.user.id).await),
        [format!("deactivated:{}:true", w.dave.user.id)]
    );
    for ws in [&mut dave, &mut other_device] {
        match ws.next().await {
            lazy_protocol::ServerFrame::Error { code, .. } => {
                assert_eq!(code, ErrorCode::Forbidden)
            }
            other => panic!("expected the subscription to end, got {other:?}"),
        }
    }
    // They cannot resume, from the start or from where they were.
    for cursor in [lazy_domain::EventSeq::ZERO, before, app.head().await] {
        let mut again = app.connect_ws(&w.dave).await;
        again.subscribe(app.workspace_id, cursor).await;
        match again.next().await {
            lazy_protocol::ServerFrame::Error { code, .. } => {
                assert_eq!(code, ErrorCode::Forbidden)
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    // Reactivating is announced the same way, and they can subscribe again.
    app.deactivate(&w.admin, &w.dave, false).await;
    let mut back = app.connect_ws(&w.dave).await;
    back.subscribe(app.workspace_id, app.head().await).await;
    back.replay().await;
    let marker = app.register("marker2").await;
    assert_eq!(
        admin_events(&carol.events_until_marker(marker.user.id).await),
        [format!("deactivated:{}:false", w.dave.user.id)]
    );
    finish(w).await;
}

/// What one request gets: its status, and the error code if it was refused.
type Answer = (String, u16, Option<ErrorCode>);

async fn answer(out: &mut Vec<Answer>, name: &str, response: reqwest::Response) {
    let status = response.status().as_u16();
    let code = if status >= 400 {
        Some(
            response
                .json::<lazy_protocol::ApiError>()
                .await
                .unwrap()
                .code,
        )
    } else {
        None
    };
    out.push((name.to_owned(), status, code));
}

/// Things made before anyone was deactivated, for the requests below to point at.
struct Things {
    conversation: ConversationId,
    private: ConversationId,
    message: lazy_domain::MessageId,
    thread_root: lazy_domain::MessageId,
    file: lazy_domain::AttachmentId,
    pending: lazy_domain::AttachmentId,
    emoji: lazy_domain::EmojiId,
    group: lazy_domain::GroupId,
    invitation: InvitationId,
    webhook: lazy_domain::WebhookId,
}

/// Every family of endpoint, asked by `who`. Each is something a workspace member could do or read;
/// for a deactivated person all must be refused exactly as for a stranger.
async fn ask_everything(app: &App, who: &Account, things: &Things, peer: &Account) -> Vec<Answer> {
    use lazy_protocol::api::paths;
    let mut out = Vec::new();
    let ws = app.workspace_id;
    let c = things.conversation;
    // Messages: read, send, change, react, thread.
    answer(&mut out, "history", app.history(who, c, &[]).await).await;
    answer(
        &mut out,
        "get message",
        app.get_message_raw(who, things.message).await,
    )
    .await;
    answer(
        &mut out,
        "replies",
        app.call(
            reqwest::Method::GET,
            &paths::replies(things.thread_root),
            who,
            None,
        )
        .await,
    )
    .await;
    answer(
        &mut out,
        "send",
        app.send_raw(who, c, OperationId::generate(), "hello").await,
    )
    .await;
    answer(&mut out, "edit", app.call(reqwest::Method::PATCH, &paths::message(things.message), who, Some(json!({"operation_id": OperationId::generate(), "base_revision": 1, "document": {"version": 2, "blocks": [{"type": "paragraph", "inlines": [{"type": "text", "text": "x"}]}]}}))).await).await;
    answer(
        &mut out,
        "delete",
        app.delete_raw(who, things.message).await,
    )
    .await;
    answer(
        &mut out,
        "react",
        app.react_raw(who, things.message, "👍", true).await,
    )
    .await;
    answer(
        &mut out,
        "follow thread",
        app.follow_raw(who, things.thread_root, true).await,
    )
    .await;
    answer(
        &mut out,
        "mark thread read",
        app.read_thread_raw(
            who,
            things.thread_root,
            lazy_domain::EventSeq::new(1_000_000).unwrap(),
        )
        .await,
    )
    .await;
    // Conversations.
    answer(
        &mut out,
        "members",
        app.call(
            reqwest::Method::GET,
            &paths::conversation_action(c, "members"),
            who,
            None,
        )
        .await,
    )
    .await;
    answer(
        &mut out,
        "private history",
        app.history(who, things.private, &[]).await,
    )
    .await;
    answer(&mut out, "leave", app.act(who, c, "leave").await).await;
    answer(&mut out, "favorite", app.favorite(who, c, true).await).await;
    answer(&mut out, "mark read", app.mark_read(who, c, 1).await).await;
    answer(&mut out, "browse", app.browse_raw(who, ws, &[]).await).await;
    answer(
        &mut out,
        "create channel",
        app.create_channel_raw(who, "fresh", "public", OperationId::generate())
            .await,
    )
    .await;
    answer(
        &mut out,
        "open conversation",
        app.open_raw(who, &[peer], OperationId::generate()).await,
    )
    .await;
    answer(
        &mut out,
        "add members",
        app.add_members(who, c, &[peer]).await,
    )
    .await;
    // Search, saved, inbox, people, notifications, status.
    answer(
        &mut out,
        "search",
        app.search_raw(who, ws, &[("q", "needle")]).await,
    )
    .await;
    answer(&mut out, "saved list", app.saved_raw(who, ws, &[]).await).await;
    answer(
        &mut out,
        "save",
        app.set_saved_raw(who, things.message, true).await,
    )
    .await;
    answer(&mut out, "inbox", app.inbox_raw(who, ws, &[]).await).await;
    answer(&mut out, "inbox read", app.inbox_read_raw(who, ws).await).await;
    answer(&mut out, "people", app.people_raw(who, ws, &[]).await).await;
    answer(
        &mut out,
        "profile",
        app.person_raw(who, ws, peer.user.id).await,
    )
    .await;
    answer(
        &mut out,
        "settings",
        app.set_settings_raw(
            who,
            ws,
            json!(lazy_domain::notification::NotificationSettings::default()),
        )
        .await,
    )
    .await;
    answer(
        &mut out,
        "prefs",
        app.set_prefs_raw(who, c, json!({"level": "all", "muted": false}))
            .await,
    )
    .await;
    answer(
        &mut out,
        "status",
        app.set_status_raw(who, ws, json!({"status": null})).await,
    )
    .await;
    // Files, emoji, groups, webhooks.
    answer(
        &mut out,
        "upload",
        app.upload_raw(
            who,
            c,
            lazy_domain::AttachmentId::generate(),
            b"bytes".to_vec(),
            "application/zip",
            "a.bin",
        )
        .await,
    )
    .await;
    answer(
        &mut out,
        "download sent file",
        app.download_raw(who, things.file, false, false, &[]).await,
    )
    .await;
    answer(
        &mut out,
        "download pending file",
        app.download_raw(who, things.pending, false, false, &[])
            .await,
    )
    .await;
    answer(
        &mut out,
        "emoji image",
        app.emoji_raw(who, things.emoji, reqwest::Method::GET, None, &[])
            .await,
    )
    .await;
    answer(
        &mut out,
        "emoji upload",
        app.upload_emoji_raw(
            who,
            ws,
            lazy_domain::EmojiId::generate(),
            png_bytes(20, 20, 255),
            Some("fresh_one"),
        )
        .await,
    )
    .await;
    answer(
        &mut out,
        "emoji rename",
        app.rename_emoji_raw(who, things.emoji, "renamed").await,
    )
    .await;
    answer(
        &mut out,
        "emoji delete",
        app.emoji_raw(who, things.emoji, reqwest::Method::DELETE, None, &[])
            .await,
    )
    .await;
    answer(
        &mut out,
        "groups",
        app.call(reqwest::Method::GET, &paths::groups(ws), who, None)
            .await,
    )
    .await;
    answer(
        &mut out,
        "group delete",
        app.call(
            reqwest::Method::DELETE,
            &paths::group(things.group),
            who,
            None,
        )
        .await,
    )
    .await;
    answer(
        &mut out,
        "create webhook",
        app.create_webhook_raw(who, c, "bot").await,
    )
    .await;
    answer(
        &mut out,
        "list webhooks",
        app.list_webhooks_raw(who, c).await,
    )
    .await;
    answer(
        &mut out,
        "workspace webhooks",
        app.call(
            reqwest::Method::GET,
            &paths::workspace_webhooks(ws),
            who,
            None,
        )
        .await,
    )
    .await;
    answer(
        &mut out,
        "webhook deliveries",
        app.call(
            reqwest::Method::GET,
            &paths::webhook_deliveries(things.webhook),
            who,
            None,
        )
        .await,
    )
    .await;
    answer(
        &mut out,
        "new webhook address",
        app.call(
            reqwest::Method::POST,
            &paths::webhook_address(things.webhook),
            who,
            None,
        )
        .await,
    )
    .await;
    // Administration.
    answer(
        &mut out,
        "patch workspace",
        app.patch_workspace_raw(who, ws, json!({"name": "Hijacked"}))
            .await,
    )
    .await;
    answer(
        &mut out,
        "set role",
        app.set_role_raw(who, ws, peer.user.id, "admin").await,
    )
    .await;
    answer(
        &mut out,
        "deactivate",
        app.set_deactivated_raw(who, ws, peer.user.id, true).await,
    )
    .await;
    answer(
        &mut out,
        "list invitations",
        app.list_invitations_raw(who, ws).await,
    )
    .await;
    answer(
        &mut out,
        "create invitation",
        app.create_invitation_raw(who, ws, json!({})).await,
    )
    .await;
    answer(
        &mut out,
        "revoke invitation",
        app.revoke_invitation_raw(who, things.invitation).await,
    )
    .await;
    out
}

#[sqlx::test]
async fn a_deactivated_person_gets_what_a_stranger_gets_from_every_family_of_endpoint(
    pool: PgPool,
) {
    let w = world(pool).await;
    let app = &w.app;
    let stranger = outsider(app, "stranger").await;
    // The things the requests point at, made while everyone is active.
    let private = app.create_channel(&w.owner, "secret", "private").await;
    assert!(
        app.add_members(&w.owner, private.id, &[&w.dave])
            .await
            .status()
            .is_success()
    );
    let message = app.send(&w.owner, app.general, "needle here").await;
    let thread_root = app.send(&w.owner, app.general, "a thread").await;
    app.reply(&w.dave, &thread_root, "dave replies").await;
    let file = app
        .upload(
            &w.owner,
            app.general,
            b"bytes".to_vec(),
            "application/zip",
            "f.bin",
        )
        .await;
    app.send_with_files(&w.owner, app.general, "with a file", &[file.id])
        .await;
    let pending = app
        .upload(
            &w.dave,
            app.general,
            b"mine".to_vec(),
            "application/zip",
            "mine.bin",
        )
        .await;
    let emoji = app
        .upload_emoji(
            &w.owner,
            app.workspace_id,
            "party_one",
            png_bytes(20, 20, 255),
        )
        .await;
    let group = app.create_group(&w.owner, "team", &[&w.bob]).await;
    let invitation = app.create_invitation(&w.owner, json!({})).await.invitation;
    let webhook = app
        .create_webhook(&w.owner, app.general, "Watched")
        .await
        .webhook
        .id;
    let things = Things {
        conversation: app.general,
        private: private.id,
        message: message.id,
        thread_root: thread_root.id,
        file: file.id,
        pending: pending.id,
        emoji: emoji.id,
        group: group.id,
        invitation: invitation.id,
        webhook,
    };
    // Before: Dave, a member, gets answers a stranger does not (so the comparison means something).
    let active = ask_everything(app, &w.dave, &things, &w.carol).await;
    let strangers = ask_everything(app, &stranger, &things, &w.carol).await;
    let differing = active
        .iter()
        .zip(&strangers)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        differing >= 25,
        "{differing} of {} differ while he is active",
        active.len()
    );

    app.deactivate(&w.owner, &w.dave, true).await;
    let deactivated = ask_everything(app, &w.dave, &things, &w.carol).await;
    let strangers = ask_everything(app, &stranger, &things, &w.carol).await;
    for (d, s) in deactivated.iter().zip(&strangers) {
        assert_eq!(
            d, s,
            "a deactivated person must get what a stranger gets from `{}`",
            d.0
        );
    }
    // And none of those requests changed anything.
    assert_eq!(
        app.bootstrap_ok(&w.carol).await.workspaces[0]
            .workspace
            .name,
        "Lazy"
    );

    // The workspace is gone from their bootstrap, though their account and session are fine.
    let boot = app.bootstrap_ok(&w.dave).await;
    assert!(boot.workspaces.is_empty());
    assert_eq!(boot.user.id, w.dave.user.id);
    // What they made earlier tells them nothing now.
    let operation = OperationId::generate();
    let sent = app
        .send_raw(&w.dave, app.general, operation, "before")
        .await;
    assert!(!sent.status().is_success(), "they can no longer send");
    finish(w).await;
}

#[sqlx::test]
async fn an_operation_a_person_made_is_not_reported_to_them_once_deactivated(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let operation = OperationId::generate();
    let sent = app.send_raw(&w.dave, app.general, operation, "mine").await;
    assert!(sent.status().is_success());
    assert!(matches!(
        app.operation(&w.dave, operation).await,
        lazy_protocol::OperationStatus::Committed { .. }
    ));
    let channel_operation = OperationId::generate();
    assert!(
        app.create_channel_raw(&w.dave, "daves", "public", channel_operation)
            .await
            .status()
            .is_success()
    );
    assert!(matches!(
        app.operation(&w.dave, channel_operation).await,
        lazy_protocol::OperationStatus::ConversationCommitted { .. }
    ));
    app.deactivate(&w.owner, &w.dave, true).await;
    assert!(matches!(
        app.operation(&w.dave, operation).await,
        lazy_protocol::OperationStatus::NotFound
    ));
    assert!(matches!(
        app.operation(&w.dave, channel_operation).await,
        lazy_protocol::OperationStatus::NotFound
    ));
    app.deactivate(&w.owner, &w.dave, false).await;
    assert!(matches!(
        app.operation(&w.dave, operation).await,
        lazy_protocol::OperationStatus::Committed { .. }
    ));
    finish(w).await;
}

#[sqlx::test]
async fn reactivating_restores_everything_the_person_had(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let (dave, owner) = (&w.dave, &w.owner);
    let secret = app.create_channel(owner, "secret", "private").await;
    assert!(
        app.add_members(owner, secret.id, &[dave])
            .await
            .status()
            .is_success()
    );
    app.set_role(dave, "admin").await;
    let mine = app.send(dave, app.general, "needle from dave").await;
    let theirs = app.send(owner, secret.id, "needle in secret").await;
    app.set_saved(dave, theirs.id, true).await;
    app.set_status(dave, "Around", None).await;
    app.set_prefs(dave, app.general, None, true).await;
    let before = app.bootstrap_ok(dave).await;

    app.deactivate(owner, dave, true).await;
    // Their messages stay and others still read them, with the author's name.
    let page = app.history_ok(&w.carol, app.general, &[]).await;
    assert!(
        page.messages
            .iter()
            .any(|m| m.id == mine.id && m.author_id == dave.user.id)
    );
    let members = app.bootstrap_ok(&w.carol).await.workspaces[0]
        .members
        .clone();
    assert!(
        members
            .iter()
            .find(|m| m.id == dave.user.id)
            .unwrap()
            .deactivated
    );
    // The conversation memberships stay, counted.
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversation_members WHERE user_id = $1")
            .bind(dave.user.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(count >= 2);
    assert_eq!(
        app.conversations_of(&w.carol)
            .await
            .into_iter()
            .find(|c| c.id == app.general)
            .unwrap()
            .member_count,
        5,
        "channel counts still count them"
    );

    app.deactivate(owner, dave, false).await;
    let after = app.bootstrap_ok(dave).await;
    assert_eq!(after.workspaces.len(), 1);
    assert_eq!(after.workspaces[0].role, WorkspaceRole::Admin);
    let ids = |b: &lazy_protocol::BootstrapResponse| {
        let mut ids: Vec<ConversationId> =
            b.workspaces[0].conversations.iter().map(|c| c.id).collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(&after), ids(&before));
    assert_eq!(after.workspaces[0].saved, before.workspaces[0].saved);
    assert_eq!(
        after.workspaces[0].conversation_notification_prefs,
        before.workspaces[0].conversation_notification_prefs
    );
    assert_eq!(app.history_ok(dave, secret.id, &[]).await.messages.len(), 1);
    assert!(
        app.send_raw(dave, app.general, OperationId::generate(), "back")
            .await
            .status()
            .is_success()
    );
    assert_eq!(app.saved_page(dave, &[]).await.items.len(), 1);
    assert_eq!(app.find(dave, "needle").await.hits.len(), 2);
    let me = app.people(&w.carol, &[("query", "dave")]).await.people[0].clone();
    assert!(!me.user.deactivated);
    assert_eq!(me.user.status.as_ref().map(|s| s.text()), Some("Around"));
    finish(w).await;
}

#[sqlx::test]
async fn a_deactivated_person_is_left_out_of_mentions_adding_opening_and_the_directory(
    pool: PgPool,
) {
    use lazy_domain::mention::MentionTarget;
    let w = world(pool).await;
    let app = &w.app;
    let group = app.create_group(&w.owner, "team", &[&w.bob, &w.dave]).await;
    let dm = app.open(&w.carol, &[&w.dave]).await;
    app.deactivate(&w.owner, &w.dave, true).await;

    // A mention of them stays plain text and notifies no one; so does a group they are in.
    let sent = app
        .send_mentioning(
            &w.carol,
            app.general,
            "hello @dave and @bob",
            &[
                ("dave", MentionTarget::User { id: w.dave.user.id }),
                ("bob", MentionTarget::User { id: w.bob.user.id }),
            ],
        )
        .await;
    assert_eq!(sent.mentions.users, vec![w.bob.user.id]);
    assert!(sent.document.plain_text().contains("@dave"));
    let grouped = app
        .send_mentioning(
            &w.carol,
            app.general,
            "@team hello",
            &[("team", MentionTarget::Group { id: group.id })],
        )
        .await;
    assert_eq!(grouped.mentions.users, vec![w.bob.user.id]);
    // They cannot be added to a channel, opened a conversation with, or put in a group.
    let open = app.create_channel(&w.owner, "open", "public").await;
    denied(
        app.add_members(&w.owner, open.id, &[&w.dave]).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        app.open_raw(&w.carol, &[&w.dave], OperationId::generate())
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let update = app
        .call(
            reqwest::Method::PATCH,
            &lazy_protocol::api::paths::group(group.id),
            &w.owner,
            Some(json!({"members": [w.bob.user.id, w.dave.user.id]})),
        )
        .await;
    assert_eq!(update.status().as_u16(), 400);
    // The conversation that exists stays, and others can still write in it.
    assert!(
        app.send_raw(&w.carol, dm.id, OperationId::generate(), "still here")
            .await
            .status()
            .is_success()
    );
    // The directory leaves them out, except for an admin who asks; everyone else may not ask.
    let names = |page: &lazy_protocol::PeoplePage| {
        page.people
            .iter()
            .map(|p| p.user.username.to_string())
            .collect::<Vec<_>>()
    };
    assert!(!names(&app.people(&w.carol, &[]).await).contains(&"dave".to_owned()));
    denied(
        app.people_raw(
            &w.carol,
            app.workspace_id,
            &[("include_deactivated", "true")],
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;
    let listed = app
        .people(&w.admin, &[("include_deactivated", "true")])
        .await;
    let dave = listed
        .people
        .iter()
        .find(|p| p.user.username.as_str() == "dave")
        .unwrap();
    assert!(dave.user.deactivated);
    // A profile still resolves, flagged, for the old messages' sake.
    let response = app
        .person_raw(&w.carol, app.workspace_id, w.dave.user.id)
        .await;
    assert!(
        response
            .json::<lazy_protocol::PersonResponse>()
            .await
            .unwrap()
            .person
            .user
            .deactivated
    );
    // Webhooks keep posting: they are not a person.
    let hook = app.create_webhook(&w.owner, app.general, "Deploybot").await;
    assert!(
        app.deliver_raw(&hook.url, "still delivering", &[])
            .await
            .status()
            .is_success()
    );
    finish(w).await;
}

// --- Workspace settings ----------------------------------------------------------------------

#[sqlx::test]
async fn settings_and_the_name_change_for_admins_are_announced_and_validated(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let stranger = outsider(app, "stranger").await;
    let before = app.head().await;
    let mut carol = listener(app, &w.carol).await;
    let boot = app.bootstrap_ok(&w.carol).await;
    assert_eq!(
        boot.workspaces[0].workspace.settings,
        WorkspaceSettings::default()
    );

    let response = app
        .patch_workspace_raw(&w.admin, app.workspace_id, json!({"name": "  Acme  ", "settings": {"registration": "invite_only", "channel_creation": "admins_only"}}))
        .await;
    assert_eq!(response.status().as_u16(), 200);
    let updated: lazy_protocol::WorkspaceResponse = response.json().await.unwrap();
    assert_eq!(updated.workspace.name, "Acme");
    assert_eq!(
        updated.workspace.settings.registration,
        RegistrationPolicy::InviteOnly
    );
    assert_eq!(
        updated.workspace.settings.channel_creation,
        ChannelCreationPolicy::AdminsOnly
    );
    let head = app.head().await;
    assert_eq!(head.get(), before.get() + 1);
    // The same again, and a partial one that changes nothing: no event.
    app.patch_workspace_raw(&w.admin, app.workspace_id, json!({"name": "Acme"}))
        .await;
    app.patch_workspace_raw(&w.owner, app.workspace_id, json!({}))
        .await;
    app.patch_workspace_raw(
        &w.owner,
        app.workspace_id,
        json!({"settings": {"registration": "invite_only", "channel_creation": "admins_only"}}),
    )
    .await;
    assert_eq!(app.head().await, head);
    // A name alone keeps the settings; settings alone keep the name.
    let renamed: lazy_protocol::WorkspaceResponse = app
        .patch_workspace_raw(&w.owner, app.workspace_id, json!({"name": "Acme Inc"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        renamed.workspace.settings.registration,
        RegistrationPolicy::InviteOnly
    );
    let marker = outsider_marker(app, &w.owner).await;
    let events = carol.events_until_marker(marker).await;
    assert_eq!(
        admin_events(&events),
        ["workspace:Acme", "workspace:Acme Inc"]
    );
    assert!(
        app.bootstrap_ok(&w.carol).await.workspaces[0]
            .workspace
            .name
            == "Acme Inc"
    );
    // Who may, and what is refused.
    denied(
        app.patch_workspace_raw(&w.bob, app.workspace_id, json!({"name": "Mine"}))
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.patch_workspace_raw(&stranger, app.workspace_id, json!({"name": "Mine"}))
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.patch_workspace_raw(&w.owner, WorkspaceId::generate(), json!({}))
            .await,
        ErrorCode::NotFound,
    )
    .await;
    for bad in [
        json!({"name": ""}),
        json!({"name": "   "}),
        json!({"name": "x".repeat(65)}),
        json!({"name": "bad\nname"}),
        json!({"settings": {"registration": "whenever"}}),
        json!({"settings": {"channel_creation": 3}}),
        json!({"name": 5}),
    ] {
        denied(
            app.patch_workspace_raw(&w.owner, app.workspace_id, bad.clone())
                .await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    assert!(
        app.patch_workspace_raw(&w.owner, app.workspace_id, json!({"name": "x".repeat(64)}))
            .await
            .status()
            .is_success()
    );
    finish(w).await;
}

/// Someone joins, which the workspace is told after everything committed before. Joins by
/// invitation, so it works whatever the registration policy.
async fn outsider_marker(app: &App, by: &Account) -> UserId {
    let invite = app.create_invitation(by, json!({"max_uses": 1})).await;
    let name = format!("marker{}", rand_suffix());
    let response = app
        .try_register_with(&name, "correct horse battery", Some(invite.code.expose()))
        .await;
    assert!(
        response.status().is_success(),
        "marker: {}",
        response.status()
    );
    let session: lazy_protocol::SessionResponse = response.json().await.unwrap();
    session.user.id
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[sqlx::test]
async fn the_channel_creation_policy_is_enforced_for_channels_only(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    assert!(
        app.create_channel_raw(&w.bob, "everyone-1", "public", OperationId::generate())
            .await
            .status()
            .is_success()
    );
    app.patch_workspace_raw(
        &w.owner,
        app.workspace_id,
        json!({"settings": {"channel_creation": "admins_only"}}),
    )
    .await;
    denied(
        app.create_channel_raw(&w.bob, "everyone-2", "public", OperationId::generate())
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.create_channel_raw(&w.bob, "everyone-3", "private", OperationId::generate())
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    assert!(
        app.create_channel_raw(&w.admin, "admins-1", "public", OperationId::generate())
            .await
            .status()
            .is_success()
    );
    assert!(
        app.create_channel_raw(&w.owner, "owners-1", "private", OperationId::generate())
            .await
            .status()
            .is_success()
    );
    // Direct and group conversations are not channels.
    assert!(
        app.open_raw(&w.bob, &[&w.carol], OperationId::generate())
            .await
            .status()
            .is_success()
    );
    assert!(
        app.open_raw(&w.bob, &[&w.carol, &w.dave], OperationId::generate())
            .await
            .status()
            .is_success()
    );
    // A promoted member may; a demoted admin may not.
    app.set_role_raw(&w.owner, app.workspace_id, w.bob.user.id, "admin")
        .await;
    assert!(
        app.create_channel_raw(&w.bob, "everyone-4", "public", OperationId::generate())
            .await
            .status()
            .is_success()
    );
    app.set_role_raw(&w.owner, app.workspace_id, w.bob.user.id, "member")
        .await;
    denied(
        app.create_channel_raw(&w.bob, "everyone-5", "public", OperationId::generate())
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    app.patch_workspace_raw(
        &w.owner,
        app.workspace_id,
        json!({"settings": {"channel_creation": "everyone"}}),
    )
    .await;
    assert!(
        app.create_channel_raw(&w.bob, "everyone-6", "public", OperationId::generate())
            .await
            .status()
            .is_success()
    );
    finish(w).await;
}

// --- Invitations -----------------------------------------------------------------------------

#[sqlx::test]
async fn invitations_are_validated_limited_listed_with_their_state_and_revoked(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    // Validation: lifetime between an hour and thirty days, uses between 1 and 1,000.
    for bad in [
        json!({"expires_in_secs": 3599}),
        json!({"expires_in_secs": 30 * 24 * 3600 + 1}),
        json!({"expires_in_secs": 0}),
        json!({"max_uses": 0}),
        json!({"max_uses": 1001}),
        json!({"max_uses": -1}),
        json!({"expires_in_secs": "soon"}),
    ] {
        denied(
            app.create_invitation_raw(&w.owner, app.workspace_id, bad.clone())
                .await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    let edge_low = app
        .create_invitation(&w.owner, json!({"expires_in_secs": 3600, "max_uses": 1}))
        .await;
    let edge_high = app
        .create_invitation(
            &w.admin,
            json!({"expires_in_secs": 30 * 24 * 3600, "max_uses": 1000}),
        )
        .await;
    // The default: seven days, unlimited.
    let default = app.create_invitation(&w.owner, json!({})).await;
    let week = default.invitation.expires_at.as_offset_date_time()
        - default.invitation.created_at.as_offset_date_time();
    assert!((week - time::Duration::days(7)).abs() < time::Duration::seconds(2));
    assert_eq!(
        (
            default.invitation.max_uses,
            default.invitation.uses,
            default.invitation.state
        ),
        (None, 0, InvitationState::Active)
    );
    assert_eq!(default.invitation.created_by, w.owner.user.id);
    // The code: 20 characters of the alphabet without lookalikes, different every time, never in
    // the listing.
    let codes = [
        edge_low.code.expose(),
        edge_high.code.expose(),
        default.code.expose(),
    ];
    for code in codes {
        assert!(
            lazy_domain::workspace::invite_code_is_wellformed(code),
            "{code}"
        );
        assert_eq!(code.len(), 20);
        assert!(
            code.bytes()
                .all(|b| lazy_domain::workspace::INVITE_CODE_ALPHABET.contains(&b)),
            "{code}"
        );
        assert_eq!(lazy_domain::workspace::canonical_invite_code(code), code);
    }
    assert!(codes[0] != codes[1] && codes[1] != codes[2] && codes[0] != codes[2]);
    let listing = serde_json::to_string(&app.list_invitations(&w.owner).await).unwrap();
    assert!(codes.iter().all(|c| !listing.contains(c)));
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM workspace_invitations WHERE code_hash = decode(repeat('00', 32), 'hex')")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(stored, 0);
    let plain: i64 = sqlx::query_scalar("SELECT count(*) FROM workspace_invitations WHERE encode(code_hash, 'escape') LIKE '%' || $1 || '%'")
        .bind(codes[0])
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(plain, 0, "only a hash is stored");

    // Who may: admins and owners; members refused, strangers told nothing.
    denied(
        app.create_invitation_raw(&w.bob, app.workspace_id, json!({}))
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.list_invitations_raw(&w.bob, app.workspace_id).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.revoke_invitation_raw(&w.bob, default.invitation.id)
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    let stranger = outsider(app, "stranger").await;
    denied(
        app.create_invitation_raw(&stranger, app.workspace_id, json!({}))
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.list_invitations_raw(&stranger, app.workspace_id).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.revoke_invitation_raw(&stranger, default.invitation.id)
            .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.revoke_invitation_raw(&w.owner, InvitationId::generate())
            .await,
        ErrorCode::NotFound,
    )
    .await;

    // States and order: revoked, used up, expired, active; newest first.
    let used = app
        .create_invitation(&w.owner, json!({"max_uses": 1}))
        .await;
    assert!(
        app.try_register_with("newbie", "correct horse battery", Some(used.code.expose()))
            .await
            .status()
            .is_success()
    );
    let expiring = app.create_invitation(&w.owner, json!({})).await;
    sqlx::query(
        "UPDATE workspace_invitations SET expires_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(expiring.invitation.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    let revoked = app.create_invitation(&w.owner, json!({})).await;
    let first = app
        .revoke_invitation_raw(&w.admin, revoked.invitation.id)
        .await;
    assert_eq!(first.status().as_u16(), 200);
    let first: lazy_protocol::InvitationResponse = first.json().await.unwrap();
    assert_eq!(first.invitation.state, InvitationState::Revoked);
    // Revoking a revoked one returns it unchanged.
    let again: lazy_protocol::InvitationResponse = app
        .revoke_invitation_raw(&w.owner, revoked.invitation.id)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(again.invitation, first.invitation);
    let list = app.list_invitations(&w.admin).await;
    let states: Vec<(InvitationId, InvitationState)> =
        list.iter().map(|i| (i.id, i.state)).collect();
    assert_eq!(
        states,
        [
            (revoked.invitation.id, InvitationState::Revoked),
            (expiring.invitation.id, InvitationState::Expired),
            (used.invitation.id, InvitationState::UsedUp),
            (default.invitation.id, InvitationState::Active),
            (edge_high.invitation.id, InvitationState::Active),
            (edge_low.invitation.id, InvitationState::Active),
        ]
    );
    assert_eq!(
        list.iter()
            .find(|i| i.id == used.invitation.id)
            .map(|i| (i.uses, i.max_uses)),
        Some((1, Some(1)))
    );
    // The window: old and finished ones leave the list; old and still active ones stay.
    sqlx::query("UPDATE workspace_invitations SET created_at = now() - interval '31 days' WHERE id = ANY($1)")
        .bind(vec![revoked.invitation.id.as_uuid(), default.invitation.id.as_uuid()])
        .execute(&app.pool)
        .await
        .unwrap();
    let ids: Vec<InvitationId> = app
        .list_invitations(&w.owner)
        .await
        .iter()
        .map(|i| i.id)
        .collect();
    assert!(
        !ids.contains(&revoked.invitation.id),
        "an old finished one is gone"
    );
    assert!(
        ids.contains(&default.invitation.id),
        "an old active one stays"
    );
    finish(w).await;
}

#[sqlx::test]
async fn a_workspace_has_at_most_a_hundred_active_invitations(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    sqlx::query(
        "INSERT INTO workspace_invitations (id, workspace_id, code_hash, created_by, expires_at) \
         SELECT gen_random_uuid(), $1, sha256(convert_to('seed' || n, 'UTF8')), $2, now() + interval '1 day' \
         FROM generate_series(1, 99) n",
    )
    .bind(app.workspace_id.as_uuid())
    .bind(w.owner.user.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    // The hundredth is fine; the next is not, and a race for the last place has one winner.
    let hundredth = app.create_invitation(&w.owner, json!({})).await;
    denied(
        app.create_invitation_raw(&w.owner, app.workspace_id, json!({}))
            .await,
        ErrorCode::LimitReached,
    )
    .await;
    // Revoked, expired and used-up ones do not count.
    assert!(
        app.revoke_invitation_raw(&w.owner, hundredth.invitation.id)
            .await
            .status()
            .is_success()
    );
    sqlx::query("UPDATE workspace_invitations SET expires_at = now() - interval '1 hour' WHERE id = (SELECT id FROM workspace_invitations WHERE revoked_at IS NULL LIMIT 1)")
        .execute(&app.pool)
        .await
        .unwrap();
    let responses =
        join_all((0..6).map(|_| app.create_invitation_raw(&w.owner, app.workspace_id, json!({}))))
            .await;
    let created = responses.iter().filter(|r| r.status().is_success()).count();
    assert_eq!(
        created, 2,
        "two places were free: the revoked and the expired"
    );
    assert!(
        responses
            .iter()
            .all(|r| matches!(r.status().as_u16(), 200 | 422))
    );
    finish(w).await;
}

// --- Registering with a code -----------------------------------------------------------------

#[sqlx::test]
async fn a_code_joins_the_invitations_workspace_as_a_member_and_counts_one_use(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let invite = app
        .create_invitation(&w.owner, json!({"max_uses": 3}))
        .await;
    let mut ws = listener(app, &w.bob).await;
    let response = app
        .try_register_with(
            "newcomer",
            "correct horse battery",
            Some(invite.code.expose()),
        )
        .await;
    assert_eq!(response.status().as_u16(), 200);
    let session: lazy_protocol::SessionResponse = response.json().await.unwrap();
    let newcomer = Account {
        token: session.token.expose().to_owned(),
        user: session.user,
    };
    assert_eq!(role_of(app, &newcomer).await, "member");
    let boot = app.bootstrap_ok(&newcomer).await;
    assert_eq!(boot.workspaces.len(), 1);
    assert!(
        boot.workspaces[0]
            .conversations
            .iter()
            .any(|c| c.id == app.general),
        "joined the default channel"
    );
    assert_eq!(app.list_invitations(&w.owner).await[0].uses, 1);
    // The workspace heard of them.
    let marker = outsider_marker(app, &w.owner).await;
    let events = ws.events_until_marker(marker).await;
    assert!(events.iter().any(|e| matches!(&e.event, Event::MemberJoined { user } if user.id == newcomer.user.id && user.status.is_none())));
    // The code is not in the answer or in any refusal.
    let refused = app
        .try_register_with(
            "newcomer",
            "correct horse battery",
            Some(invite.code.expose()),
        )
        .await;
    let body = refused.text().await.unwrap();
    assert!(!body.contains(invite.code.expose()));
    finish(w).await;
}

#[sqlx::test]
async fn every_bad_code_is_the_same_answer_and_creates_nothing(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let users_before: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let expired = app.create_invitation(&w.owner, json!({})).await;
    sqlx::query(
        "UPDATE workspace_invitations SET expires_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(expired.invitation.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    let revoked = app.create_invitation(&w.owner, json!({})).await;
    app.revoke_invitation_raw(&w.owner, revoked.invitation.id)
        .await;
    let used_up = app
        .create_invitation(&w.owner, json!({"max_uses": 1}))
        .await;
    assert!(
        app.try_register_with(
            "first",
            "correct horse battery",
            Some(used_up.code.expose())
        )
        .await
        .status()
        .is_success()
    );
    let users_before = users_before + 1;
    let wrong = "a".repeat(32);
    let cases = [
        ("wrong", wrong.as_str()),
        ("expired", expired.code.expose()),
        ("revoked", revoked.code.expose()),
        ("used up", used_up.code.expose()),
        ("too short", "abc"),
        (
            "bad characters",
            "this code has spaces and is long enough ok",
        ),
        ("too long", &"z".repeat(65)),
    ];
    let mut answers = Vec::new();
    for (name, code) in cases {
        let response = app
            .try_register_with("shouldfail", "correct horse battery", Some(code))
            .await;
        assert_eq!(response.status().as_u16(), 400, "{name}");
        let (_, error) = api_error(response).await;
        assert_eq!(error.code, ErrorCode::InvitationInvalid, "{name}");
        answers.push(error.message);
    }
    assert!(
        answers.windows(2).all(|p| p[0] == p[1]),
        "one message for all: {answers:?}"
    );
    assert!(answers.iter().all(|m| !m.contains("aaaa")));
    let users_after: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(users_after, users_before, "nothing was created");
    // The username was never taken by a failed attempt.
    let ok = app.create_invitation(&w.owner, json!({})).await;
    assert!(
        app.try_register_with(
            "shouldfail",
            "correct horse battery",
            Some(ok.code.expose())
        )
        .await
        .status()
        .is_success()
    );
    // A failure after the code was counted gives the use back.
    let two = app
        .create_invitation(&w.owner, json!({"max_uses": 2}))
        .await;
    let taken = app
        .try_register_with(
            "shouldfail",
            "correct horse battery",
            Some(two.code.expose()),
        )
        .await;
    assert_eq!(taken.status().as_u16(), 409);
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM workspace_invitations WHERE id = $1")
        .bind(two.invitation.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(uses, 0, "the use was given back");
    finish(w).await;
}

#[sqlx::test]
async fn a_one_use_code_has_exactly_one_winner_among_simultaneous_registrations(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    for round in 0..4 {
        let invite = app
            .create_invitation(&w.owner, json!({"max_uses": 1}))
            .await;
        let responses = join_all((0..8).map(|n| {
            let name = format!("racer{round}x{n}");
            let code = invite.code.expose().to_owned();
            async move {
                app.try_register_with(&name, "correct horse battery", Some(&code))
                    .await
            }
        }))
        .await;
        let statuses: Vec<u16> = responses.iter().map(|r| r.status().as_u16()).collect();
        assert_eq!(
            statuses.iter().filter(|s| **s == 200).count(),
            1,
            "round {round}: {statuses:?}"
        );
        assert!(
            statuses.iter().all(|s| *s == 200 || *s == 400),
            "{statuses:?}"
        );
        let uses: i32 = sqlx::query_scalar("SELECT uses FROM workspace_invitations WHERE id = $1")
            .bind(invite.invitation.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert_eq!(uses, 1);
    }
    // A limit of three lets exactly three through.
    let three = app
        .create_invitation(&w.owner, json!({"max_uses": 3}))
        .await;
    let responses = join_all((0..8).map(|n| {
        let name = format!("three{n}");
        let code = three.code.expose().to_owned();
        async move {
            app.try_register_with(&name, "correct horse battery", Some(&code))
                .await
        }
    }))
    .await;
    assert_eq!(
        responses.iter().filter(|r| r.status().is_success()).count(),
        3
    );
    finish(w).await;
}

#[sqlx::test]
async fn the_registration_policy_and_the_server_option_decide_who_may_register(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    app.patch_workspace_raw(
        &w.owner,
        app.workspace_id,
        json!({"settings": {"registration": "invite_only"}}),
    )
    .await;
    let error = denied(
        app.try_register("walkin", "correct horse battery").await,
        ErrorCode::Forbidden,
    )
    .await;
    assert_eq!(error.message, "registration is by invitation");
    let invite = app.create_invitation(&w.owner, json!({})).await;
    assert!(
        app.try_register_with(
            "invited",
            "correct horse battery",
            Some(invite.code.expose())
        )
        .await
        .status()
        .is_success()
    );
    // Open again: no code needed, and a code still works.
    app.patch_workspace_raw(
        &w.owner,
        app.workspace_id,
        json!({"settings": {"registration": "open"}}),
    )
    .await;
    assert!(
        app.try_register("walkin", "correct horse battery")
            .await
            .status()
            .is_success()
    );
    finish(w).await;
}

#[sqlx::test]
async fn the_server_option_that_closes_registration_closes_invitations_too(pool: PgPool) {
    let app = App::start_with(
        pool,
        lazy_server::ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: lazy_server::PasswordCost::FAST_FOR_TESTS,
            retention: lazy_server::RetentionPolicy::disabled(),
        },
    )
    .await;
    let owner = app.register("owner").await;
    let invite = app.create_invitation(&owner, json!({})).await;
    app.server.shutdown().await.unwrap();
    // The same database under a server that has closed registration.
    let closed = App::restart(
        &app.pool,
        lazy_server::ServerOptions {
            open_registration: false,
            open_workspace_creation: true,
            password_cost: lazy_server::PasswordCost::FAST_FOR_TESTS,
            retention: lazy_server::RetentionPolicy::disabled(),
        },
        app.workspace_id,
        app.general,
    )
    .await;
    denied(
        closed
            .try_register_with("late", "correct horse battery", Some(invite.code.expose()))
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        closed.try_register("late", "correct horse battery").await,
        ErrorCode::Forbidden,
    )
    .await;
    closed.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_code_of_another_workspace_joins_that_workspace(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let elsewhere = app.other_workspace(&[(&w.owner, "owner")]).await;
    let created: lazy_protocol::CreatedInvitation = app
        .create_invitation_raw(&w.owner, elsewhere, json!({}))
        .await
        .json()
        .await
        .unwrap();
    let response = app
        .try_register_with(
            "abroad",
            "correct horse battery",
            Some(created.code.expose()),
        )
        .await;
    assert_eq!(response.status().as_u16(), 200);
    let session: lazy_protocol::SessionResponse = response.json().await.unwrap();
    let who = Account {
        token: session.token.expose().to_owned(),
        user: session.user,
    };
    let boot = app.bootstrap_ok(&who).await;
    assert_eq!(boot.workspaces.len(), 1);
    assert_eq!(boot.workspaces[0].workspace.id, elsewhere);
    // Not in the default workspace.
    denied(
        app.people_raw(&who, app.workspace_id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    finish(w).await;
}

#[sqlx::test]
async fn the_server_speaks_a_protocol_that_has_administration(pool: PgPool) {
    const { assert!(PROTOCOL_VERSION >= 13) };
    let w = world(pool).await;
    assert_eq!(
        w.app.bootstrap_ok(&w.owner).await.protocol_version,
        PROTOCOL_VERSION
    );
    finish(w).await;
}
