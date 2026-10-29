//! Managing channels: who may rename, join, leave, add, remove, and archive, and the events each
//! command appends. "Not found" is how a private channel hides from people outside it.

mod common;

use common::*;
use lazy_domain::OperationId;
use lazy_protocol::{Conversation, ConversationResponse, ErrorCode, Event};
use reqwest::Method;
use serde_json::json;
use sqlx::PgPool;

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

async fn ok_conversation(response: reqwest::Response) -> Conversation {
    assert!(
        response.status().is_success(),
        "status {}",
        response.status()
    );
    response
        .json::<ConversationResponse>()
        .await
        .unwrap()
        .conversation
}

async fn no_content(response: reqwest::Response) {
    assert_eq!(
        response.status().as_u16(),
        204,
        "status {}",
        response.status()
    );
}

/// A workspace with one person in every role the rules distinguish.
struct World {
    app: App,
    /// The first to register: the workspace owner. Not in any channel but #general.
    owner: Account,
    /// A workspace admin, likewise.
    admin: Account,
    /// An ordinary member who creates the channels under test.
    creator: Account,
    /// An ordinary member who joins them.
    member: Account,
    /// An ordinary member who is in none of them.
    stranger: Account,
}

async fn world(pool: PgPool) -> World {
    let app = App::start(pool).await;
    let owner = app.register("owner").await;
    let admin = app.register("admin").await;
    app.set_role(&admin, "admin").await;
    let creator = app.register("creator").await;
    let member = app.register("member").await;
    let stranger = app.register("stranger").await;
    World {
        app,
        owner,
        admin,
        creator,
        member,
        stranger,
    }
}

async fn finish(world: World) {
    world.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn who_may_change_a_channels_name_and_details(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let team = app.create_channel(&w.creator, "team", "public").await;
    no_content_or_ok(app.act(&w.member, team.id, "join").await).await;
    let mut creator_ws = common_subscribed(app, &w.creator).await;

    // Renaming: the creator and admins only. An admin need not be a member of a public channel.
    denied(
        app.update(&w.member, team.id, json!({"name": "mine"}))
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.update(&w.stranger, team.id, json!({"name": "mine"}))
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    let renamed = ok_conversation(
        app.update(&w.admin, team.id, json!({"name": "squad"}))
            .await,
    )
    .await;
    assert_eq!(renamed.name.as_ref().unwrap().as_str(), "squad");
    denied(
        app.update(&w.creator, team.id, json!({"name": "general"}))
            .await,
        ErrorCode::NameTaken,
    )
    .await;
    ok_conversation(
        app.update(&w.creator, team.id, json!({"name": "team"}))
            .await,
    )
    .await;

    // Topic and description: members. Not strangers, and not an admin who has not joined.
    let topic = ok_conversation(
        app.update(&w.member, team.id, json!({"topic": "Ship it"}))
            .await,
    )
    .await;
    assert_eq!(topic.topic.as_deref(), Some("Ship it"));
    denied(
        app.update(&w.stranger, team.id, json!({"topic": "x"}))
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.update(&w.admin, team.id, json!({"description": "x"}))
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    // A request mixing allowed and forbidden fields is refused as a whole.
    denied(
        app.update(
            &w.member,
            team.id,
            json!({"topic": "sneaky", "name": "mine"}),
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;
    assert_eq!(
        app.conversations_of(&w.member)
            .await
            .iter()
            .find(|c| c.id == team.id)
            .unwrap()
            .topic
            .as_deref(),
        Some("Ship it")
    );

    // Empty text clears; limits apply.
    let cleared = ok_conversation(app.update(&w.member, team.id, json!({"topic": ""})).await).await;
    assert_eq!(cleared.topic, None);
    denied(
        app.update(&w.member, team.id, json!({"topic": "x".repeat(251)}))
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        app.update(&w.member, team.id, json!({"description": "z".repeat(1001)}))
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let described = ok_conversation(
        app.update(&w.member, team.id, json!({"description": "Two\nlines"}))
            .await,
    )
    .await;
    assert_eq!(described.description.as_deref(), Some("Two\nlines"));

    // Changing nothing appends nothing.
    let head = app.head().await;
    ok_conversation(
        app.update(&w.member, team.id, json!({"description": "Two\nlines"}))
            .await,
    )
    .await;
    ok_conversation(app.update(&w.member, team.id, json!({})).await).await;
    assert_eq!(app.head().await, head);

    // The members hear about each real change, carrying the conversation as it then was.
    let marker = app.register("marker").await;
    let events = creator_ws.events_until_marker(marker.user.id).await;
    let updates: Vec<&Conversation> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ConversationUpdated { conversation } => Some(conversation),
            _ => None,
        })
        .collect();
    assert_eq!(
        updates.len(),
        5,
        "rename, rename back, topic, clear topic, description; refused or no-op requests add none"
    );
    finish(w).await;
}

#[sqlx::test]
async fn private_channels_are_invisible_to_everyone_outside_them(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let hush = app.create_channel(&w.creator, "hush", "private").await;
    ok_conversation(app.add_members(&w.creator, hush.id, &[&w.member]).await).await;

    // Even the owner and an admin get "not found", never "forbidden": it does not exist for them.
    for outsider in [&w.owner, &w.admin, &w.stranger] {
        denied(
            app.update(outsider, hush.id, json!({"topic": "peek"}))
                .await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.act(outsider, hush.id, "join").await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.act(outsider, hush.id, "archive").await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.act(outsider, hush.id, "leave").await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.add_members(outsider, hush.id, &[outsider]).await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.remove_member(outsider, hush.id, &w.member).await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.favorite(outsider, hush.id, true).await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.mark_read(outsider, hush.id, 1).await,
            ErrorCode::NotFound,
        )
        .await;
        denied(
            app.history(outsider, hush.id, &[]).await,
            ErrorCode::NotFound,
        )
        .await;
    }
    // A conversation that does not exist looks identical.
    let missing = lazy_domain::ConversationId::generate();
    denied(
        app.act(&w.owner, missing, "join").await,
        ErrorCode::NotFound,
    )
    .await;
    // Browsing does not list it either.
    let browse = app
        .call(
            Method::GET,
            &lazy_protocol::api::paths::browse_channels(app.workspace_id),
            &w.owner,
            None,
        )
        .await;
    let listed: lazy_protocol::BrowseChannelsResponse = browse.json().await.unwrap();
    assert!(listed.channels.iter().all(|c| c.conversation.id != hush.id));
    finish(w).await;
}

#[sqlx::test]
async fn joining_and_leaving_a_channel(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let team = app.create_channel(&w.creator, "team", "public").await;
    let mut creator_ws = common_subscribed(app, &w.creator).await;
    let mut member_ws = common_subscribed(app, &w.member).await;

    // Anyone in the workspace may join a public channel, once.
    let joined = ok_conversation(app.act(&w.member, team.id, "join").await).await;
    assert_eq!(joined.member_count, 2);
    let head = app.head().await;
    assert_eq!(
        ok_conversation(app.act(&w.member, team.id, "join").await)
            .await
            .member_count,
        2
    );
    assert_eq!(app.head().await, head, "joining again is a no-op");
    assert!(
        app.conversations_of(&w.member)
            .await
            .iter()
            .any(|c| c.id == team.id)
    );
    let state = app.state_of(&w.member, team.id).await;
    assert_eq!(
        state.unread_count, 0,
        "history from before joining is not unread"
    );

    // Leaving: the leaver and the remaining members are told; the count follows.
    no_content(app.act(&w.member, team.id, "leave").await).await;
    let head = app.head().await;
    no_content(app.act(&w.member, team.id, "leave").await).await;
    assert_eq!(app.head().await, head, "leaving again is a no-op");
    assert!(
        !app.conversations_of(&w.member)
            .await
            .iter()
            .any(|c| c.id == team.id)
    );
    assert_eq!(
        app.conversations_of(&w.creator)
            .await
            .iter()
            .find(|c| c.id == team.id)
            .unwrap()
            .member_count,
        1
    );

    let marker = app.register("marker").await;
    let hers = creator_ws.events_until_marker(marker.user.id).await;
    let kinds: Vec<_> = hers.iter().map(|e| crate_label(e)).collect();
    assert_eq!(
        kinds,
        vec!["member_added", "member_removed", "conversation_updated"]
    );
    match &hers[2].event {
        Event::ConversationUpdated { conversation } => assert_eq!(conversation.member_count, 1),
        other => panic!("unexpected {other:?}"),
    }
    // The person who left learns of their own removal but not of the count update after it.
    // (Whether they also saw their own arrival depends on whether it was delivered before they
    // left, so assert the stable part: their removal is the last word on the channel.)
    let his = member_ws.events_until_marker(marker.user.id).await;
    assert_ends_with_removal(&his, team.id, w.member.user.id);
    assert!(
        his.iter()
            .all(|e| matches!(crate_label(e), "member_added" | "member_removed"))
    );

    // Joining is for public, unarchived channels; the default channel can never be left.
    let old = app.create_channel(&w.creator, "old", "public").await;
    ok_conversation(app.act(&w.creator, old.id, "archive").await).await;
    denied(
        app.act(&w.stranger, old.id, "join").await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.act(&w.member, app.general, "leave").await,
        ErrorCode::Forbidden,
    )
    .await;
    // Leaving something you are not in is quietly fine for a public channel.
    no_content(app.act(&w.stranger, team.id, "leave").await).await;
    finish(w).await;
}

fn crate_label(envelope: &lazy_protocol::EventEnvelope) -> &'static str {
    match envelope.event {
        Event::MessageCreated { .. } => "message_created",
        Event::MessageEdited { .. } => "message_edited",
        Event::MessageDeleted { .. } => "message_deleted",
        Event::ReactionsChanged { .. } => "reactions_changed",
        Event::GroupChanged { .. } => "group_changed",
        Event::GroupDeleted { .. } => "group_deleted",
        Event::MemberJoined { .. } => "member_joined",
        Event::ConversationCreated { .. } => "conversation_created",
        Event::ConversationUpdated { .. } => "conversation_updated",
        Event::MemberAdded { .. } => "member_added",
        Event::MemberRemoved { .. } => "member_removed",
        Event::UserStateChanged { .. } => "user_state_changed",
        Event::ThreadChanged { .. } => "thread_changed",
        Event::ThreadStateChanged { .. } => "thread_state_changed",
        Event::EmojiChanged { .. } => "emoji_changed",
        Event::NotificationSettingsChanged { .. } => "notification_settings_changed",
        Event::ConversationNotificationPrefsChanged { .. } => {
            "conversation_notification_prefs_changed"
        }
        Event::UserStatusChanged { .. } => "user_status_changed",
        Event::SavedChanged { .. } => "saved_changed",
        Event::WorkspaceUpdated { .. } => "workspace_updated",
        Event::MemberRoleChanged { .. } => "member_role_changed",
        Event::MemberDeactivated { .. } => "member_deactivated",
        Event::UserProfileChanged { .. } => "user_profile_changed",
    }
}

/// The last event about a conversation that someone received is their own removal from it.
///
/// Delivery checks *current* membership, so a person who was removed may or may not have been sent
/// earlier events live before the removal committed; what can never happen is anything arriving
/// after it.
fn assert_ends_with_removal(
    events: &[lazy_protocol::EventEnvelope],
    conversation: lazy_domain::ConversationId,
    user: lazy_domain::UserId,
) {
    let last = events.last().expect("at least the removal");
    assert!(
        matches!(
            &last.event,
            Event::MemberRemoved { conversation_id, user_id }
                if *conversation_id == conversation && *user_id == user
        ),
        "the removal should be last, got {:?}",
        events.iter().map(crate_label).collect::<Vec<_>>()
    );
}

async fn common_subscribed(app: &App, who: &Account) -> Ws {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, app.head().await).await;
    ws.replay().await;
    ws
}

async fn no_content_or_ok(response: reqwest::Response) {
    assert!(
        response.status().is_success(),
        "status {}",
        response.status()
    );
}

#[sqlx::test]
async fn adding_people_to_a_channel(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let hush = app.create_channel(&w.creator, "hush", "private").await;
    let mut creator_ws = common_subscribed(app, &w.creator).await;
    let mut member_ws = common_subscribed(app, &w.member).await;

    // A member adds two people at once; each event carries the count as of that person.
    let result = ok_conversation(
        app.add_members(&w.creator, hush.id, &[&w.member, &w.stranger])
            .await,
    )
    .await;
    assert_eq!(result.member_count, 3);
    // People already in are ignored, as are repeats within one request, and nothing is appended.
    let head = app.head().await;
    let again = ok_conversation(
        app.add_members(&w.creator, hush.id, &[&w.member, &w.member])
            .await,
    )
    .await;
    assert_eq!(again.member_count, 3);
    ok_conversation(app.add_members(&w.creator, hush.id, &[]).await).await;
    assert_eq!(app.head().await, head);

    for added in [&w.member, &w.stranger] {
        assert!(
            app.conversations_of(added)
                .await
                .iter()
                .any(|c| c.id == hush.id)
        );
    }
    let marker = app.register("marker").await;
    let events = creator_ws.events_until_marker(marker.user.id).await;
    let counts: Vec<(lazy_domain::UserId, u32)> = events
        .iter()
        .map(|e| match &e.event {
            Event::MemberAdded {
                conversation,
                user_id,
            } => (*user_id, conversation.member_count),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(counts, vec![(w.member.user.id, 2), (w.stranger.user.id, 3)]);
    // The person added receives the event too (and so does everyone else already inside).
    assert_eq!(member_ws.events_until_marker(marker.user.id).await.len(), 2);

    // Any member may add people, but they must belong to the workspace.
    ok_conversation(app.add_members(&w.member, hush.id, &[&w.admin]).await).await;
    let outsider = app.register("outsider").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(outsider.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(
        app.add_members(&w.creator, hush.id, &[&outsider]).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let too_many =
        json!({"user_ids": (0..101).map(|_| lazy_domain::UserId::generate()).collect::<Vec<_>>()});
    denied(
        app.call(
            Method::POST,
            &lazy_protocol::api::paths::conversation_action(hush.id, "members"),
            &w.creator,
            Some(too_many),
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    finish(w).await;
}

#[sqlx::test]
async fn admins_add_to_public_channels_but_never_around_a_private_one(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let team = app.create_channel(&w.creator, "team", "public").await;
    let hush = app.create_channel(&w.creator, "hush", "private").await;

    // A non-member cannot add people, unless they are an admin and the channel is public.
    denied(
        app.add_members(&w.stranger, team.id, &[&w.member]).await,
        ErrorCode::Forbidden,
    )
    .await;
    assert_eq!(
        ok_conversation(app.add_members(&w.admin, team.id, &[&w.member]).await)
            .await
            .member_count,
        2
    );
    assert_eq!(
        ok_conversation(app.add_members(&w.owner, team.id, &[&w.stranger]).await)
            .await
            .member_count,
        3
    );
    denied(
        app.add_members(&w.admin, hush.id, &[&w.member]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.add_members(&w.owner, hush.id, &[&w.member]).await,
        ErrorCode::NotFound,
    )
    .await;

    // Archived channels are closed to additions.
    ok_conversation(app.act(&w.creator, team.id, "archive").await).await;
    denied(
        app.add_members(&w.creator, team.id, &[&w.admin]).await,
        ErrorCode::Forbidden,
    )
    .await;
    finish(w).await;
}

#[sqlx::test]
async fn who_may_remove_people_from_a_channel(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let team = app.create_channel(&w.creator, "team", "public").await;
    ok_conversation(
        app.add_members(&w.creator, team.id, &[&w.member, &w.stranger])
            .await,
    )
    .await;
    let mut member_ws = common_subscribed(app, &w.member).await;
    let mut stranger_ws = common_subscribed(app, &w.stranger).await;

    // An ordinary member cannot remove anyone.
    denied(
        app.remove_member(&w.member, team.id, &w.stranger).await,
        ErrorCode::Forbidden,
    )
    .await;
    // The creator can; so can an admin, even without being in the channel.
    no_content(app.remove_member(&w.creator, team.id, &w.stranger).await).await;
    no_content(app.remove_member(&w.admin, team.id, &w.member).await).await;
    assert_eq!(
        app.conversations_of(&w.creator)
            .await
            .iter()
            .find(|c| c.id == team.id)
            .unwrap()
            .member_count,
        1
    );
    assert_eq!(app.counted_members(team.id).await, 1);

    // Removing someone who is not there changes nothing.
    let head = app.head().await;
    no_content(app.remove_member(&w.creator, team.id, &w.stranger).await).await;
    assert_eq!(app.head().await, head);

    // Both removed people were told of their own removal, and nothing after it.
    // Both removed people were told of their own removal, and nothing about the channel follows it.
    let marker = app.register("marker").await;
    // `member` was removed second, so it may or may not have received the first removal live before
    // its own took effect; either way its own removal is the last thing about the channel.
    let mine = member_ws.events_until_marker(marker.user.id).await;
    assert_ends_with_removal(&mine, team.id, w.member.user.id);
    // `stranger` was removed first and could never again be a member, so its stream is exact.
    let theirs = stranger_ws.events_until_marker(marker.user.id).await;
    assert_eq!(
        theirs.iter().map(crate_label).collect::<Vec<_>>(),
        vec!["member_removed"]
    );

    // Nobody is removed from the default channel; removing yourself is leaving.
    denied(
        app.remove_member(&w.admin, app.general, &w.member).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.remove_member(&w.owner, app.general, &w.member).await,
        ErrorCode::Forbidden,
    )
    .await;
    let hush = app.create_channel(&w.creator, "hush", "private").await;
    denied(
        app.remove_member(&w.admin, hush.id, &w.creator).await,
        ErrorCode::NotFound,
    )
    .await;
    no_content(app.remove_member(&w.creator, hush.id, &w.creator).await).await;
    finish(w).await;
}

#[sqlx::test]
async fn archiving_closes_a_channel_but_keeps_its_history(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let team = app.create_channel(&w.creator, "team", "public").await;
    ok_conversation(app.act(&w.member, team.id, "join").await).await;
    let before = app.send(&w.member, team.id, "before").await;
    let mut member_ws = common_subscribed(app, &w.member).await;

    // Ordinary members cannot archive; the creator and admins can; nobody can the default channel.
    denied(
        app.act(&w.member, team.id, "archive").await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.act(&w.admin, app.general, "archive").await,
        ErrorCode::Forbidden,
    )
    .await;
    let archived = ok_conversation(app.act(&w.creator, team.id, "archive").await).await;
    assert!(archived.archived);

    // History stays readable; posting, joining, and detail changes are closed.
    assert_eq!(
        app.history_ok(&w.member, team.id, &[]).await.messages,
        vec![before]
    );
    denied(
        app.send_raw(&w.member, team.id, OperationId::generate(), "after")
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.act(&w.stranger, team.id, "join").await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.update(&w.member, team.id, json!({"topic": "x"})).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.update(&w.creator, team.id, json!({"name": "other"}))
            .await,
        ErrorCode::Forbidden,
    )
    .await;

    // Archiving again is a no-op for someone who could; only the right people may reopen it.
    let head = app.head().await;
    assert!(
        ok_conversation(app.act(&w.admin, team.id, "archive").await)
            .await
            .archived
    );
    denied(
        app.act(&w.member, team.id, "archive").await,
        ErrorCode::Forbidden,
    )
    .await;
    assert_eq!(app.head().await, head);
    denied(
        app.act(&w.member, team.id, "unarchive").await,
        ErrorCode::Forbidden,
    )
    .await;
    let reopened = ok_conversation(app.act(&w.admin, team.id, "unarchive").await).await;
    assert!(!reopened.archived);
    // Reopening a channel that is already open is a no-op for anyone who could reopen it.
    let again = ok_conversation(app.act(&w.creator, team.id, "unarchive").await).await;
    assert!(!again.archived);
    app.send(&w.member, team.id, "after reopening").await;

    // Members saw each state change.
    let marker = app.register("marker").await;
    let events = member_ws.events_until_marker(marker.user.id).await;
    let states: Vec<bool> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ConversationUpdated { conversation } => Some(conversation.archived),
            _ => None,
        })
        .collect();
    assert_eq!(states, vec![true, false]);
    finish(w).await;
}

#[sqlx::test]
async fn admins_archive_public_channels_they_have_not_joined(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let team = app.create_channel(&w.creator, "team", "public").await;
    assert!(
        ok_conversation(app.act(&w.admin, team.id, "archive").await)
            .await
            .archived
    );
    assert!(
        !ok_conversation(app.act(&w.owner, team.id, "unarchive").await)
            .await
            .archived
    );
    // Not an admin, not the creator, not even a member: forbidden for a public channel.
    denied(
        app.act(&w.stranger, team.id, "archive").await,
        ErrorCode::Forbidden,
    )
    .await;
    finish(w).await;
}

#[sqlx::test]
async fn the_default_channel_can_be_renamed_by_admins_but_nothing_else(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let renamed = ok_conversation(
        app.update(&w.admin, app.general, json!({"name": "town-square"}))
            .await,
    )
    .await;
    assert!(renamed.is_default);
    assert_eq!(renamed.name.as_ref().unwrap().as_str(), "town-square");
    denied(
        app.update(&w.member, app.general, json!({"name": "mine"}))
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    // Everyone is a member, so it still cannot be left, archived, or have members removed.
    denied(
        app.act(&w.creator, app.general, "leave").await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.act(&w.owner, app.general, "archive").await,
        ErrorCode::Forbidden,
    )
    .await;
    finish(w).await;
}

#[sqlx::test]
async fn the_maintained_member_count_always_matches_the_members(pool: PgPool) {
    let w = world(pool).await;
    let app = &w.app;
    let people = [&w.owner, &w.admin, &w.creator, &w.member, &w.stranger];
    let team = app.create_channel(&w.creator, "team", "public").await;

    // Churn from several tasks at once: joins, leaves, adds, removes.
    let mut tasks = Vec::new();
    for round in 0..4 {
        for (index, person) in people.iter().enumerate() {
            let http = app.http.clone();
            let base = app.server.base_url.clone();
            let token = person.token.clone();
            let creator_token = w.creator.token.clone();
            let target = people[(index + round) % people.len()].user.id;
            let conversation = team.id;
            tasks.push(tokio::spawn(async move {
                let action = (index + round) % 4;
                let request = match action {
                    0 => http
                        .post(format!(
                            "{base}{}",
                            lazy_protocol::api::paths::conversation_action(conversation, "join")
                        ))
                        .bearer_auth(&token),
                    1 => http
                        .post(format!(
                            "{base}{}",
                            lazy_protocol::api::paths::conversation_action(conversation, "leave")
                        ))
                        .bearer_auth(&token),
                    2 => http
                        .post(format!(
                            "{base}{}",
                            lazy_protocol::api::paths::conversation_action(conversation, "members")
                        ))
                        .bearer_auth(&creator_token)
                        .json(&json!({"user_ids": [target]})),
                    _ => http
                        .delete(format!(
                            "{base}{}",
                            lazy_protocol::api::paths::member(conversation, target)
                        ))
                        .bearer_auth(&creator_token),
                };
                // Some of these are refused (not allowed, or already so); that is part of the churn.
                request.send().await.unwrap().status().as_u16()
            }));
        }
    }
    for task in tasks {
        let status = task.await.unwrap();
        assert!(status < 500, "server error {status}");
    }

    for conversation in [team.id, app.general] {
        let counted = app.counted_members(conversation).await;
        let stored: i32 =
            sqlx::query_scalar("SELECT member_count FROM conversations WHERE id = $1")
                .bind(conversation.as_uuid())
                .fetch_one(&app.pool)
                .await
                .unwrap();
        assert_eq!(
            i64::from(stored),
            counted,
            "member_count drifted for {conversation}"
        );
        let states: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM conversation_user_state WHERE conversation_id = $1",
        )
        .bind(conversation.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!(
            states, counted,
            "state rows must exist exactly while someone is a member"
        );
    }
    finish(w).await;
}
