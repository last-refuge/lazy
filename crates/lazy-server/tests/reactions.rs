//! Reactions to messages: setting and clearing, ordering, the limit, canonical emoji,
//! authorization, events and replay, how every message carries its reactions, deletion, and the
//! races.

mod common;

use std::collections::BTreeSet;

use common::*;
use futures_util::future::join_all;
use lazy_domain::emoji;
use lazy_domain::{EventSeq, MAX_REACTION_KINDS, MessageId, OperationId, ReactionEmoji, UserId};
use lazy_protocol::{
    ErrorCode, Event, EventEnvelope, MessageResponse, OperationStatus, ReactionSummary,
};
use serde_json::json;
use sqlx::PgPool;

const THUMBS: &str = "👍";
const PARTY: &str = "🎉";
/// U+2764 with its emoji presentation selector: the canonical heart.
const HEART: &str = "\u{2764}\u{FE0F}";
/// U+2764 alone, how many keyboards type it.
const HEART_BARE: &str = "\u{2764}";

fn emoji_of(glyph: &str) -> ReactionEmoji {
    ReactionEmoji::parse(glyph).expect("a real emoji")
}

fn summary(glyph: &str, users: &[UserId]) -> ReactionSummary {
    ReactionSummary {
        emoji: emoji_of(glyph),
        users: users.to_vec(),
    }
}

/// Every (emoji, person) pair in a list of reactions.
fn pairs(reactions: &[ReactionSummary]) -> BTreeSet<(String, UserId)> {
    reactions
        .iter()
        .flat_map(|r| r.users.iter().map(|u| (r.emoji.to_string(), *u)))
        .collect()
}

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

async fn subscribed(app: &App, who: &Account) -> Ws {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, app.head().await).await;
    ws.replay().await;
    ws
}

fn reaction_changes(
    events: &[EventEnvelope],
) -> Vec<(&EventSeq, &MessageId, &Vec<ReactionSummary>)> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ReactionsChanged {
                message_id,
                reactions,
                ..
            } => Some((&e.seq, message_id, reactions)),
            _ => None,
        })
        .collect()
}

/// An owner, an admin, an author, a member and a stranger, all in #general.
struct Chat {
    app: App,
    owner: Account,
    admin: Account,
    author: Account,
    member: Account,
    stranger: Account,
}

async fn chat(pool: PgPool) -> Chat {
    let app = App::start(pool).await;
    let owner = app.register("owner").await;
    let admin = app.register("admin").await;
    app.set_role(&admin, "admin").await;
    let author = app.register("author").await;
    let member = app.register("member").await;
    let stranger = app.register("stranger").await;
    Chat {
        app,
        owner,
        admin,
        author,
        member,
        stranger,
    }
}

async fn finish(chat: Chat) {
    chat.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_reaction_is_added_announced_and_removed(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "react to me").await;
    let mut author_ws = subscribed(app, &c.author).await;
    let mut member_ws = subscribed(app, &c.member).await;

    let reacted = app.react(&c.member, message.id, THUMBS, true).await;
    assert_eq!(
        reacted.reactions,
        vec![summary(THUMBS, &[c.member.user.id])]
    );
    assert!(
        reacted.reactions_as_of > message.seq,
        "a position of its own"
    );
    // The content is untouched: a reaction is not an edit.
    assert_eq!(
        (
            reacted.revision,
            reacted.edited_at,
            reacted.seq,
            &reacted.document
        ),
        (1, None, message.seq, &message.document)
    );
    assert_eq!(app.get_message(&c.author, message.id).await, reacted);
    assert_eq!(
        app.history_ok(&c.author, app.general, &[]).await.messages,
        vec![reacted.clone()]
    );

    let cleared = app.react(&c.member, message.id, THUMBS, false).await;
    assert!(cleared.reactions.is_empty());
    assert!(cleared.reactions_as_of > reacted.reactions_as_of);
    assert_eq!(app.reaction_rows(message.id).await, 0);

    // Both members were told, twice, with the complete list each time, and each event's
    // position is the version it carries.
    let marker = app.register("marker").await;
    for ws in [&mut author_ws, &mut member_ws] {
        let events = ws.events_until_marker(marker.user.id).await;
        let changes = reaction_changes(&events);
        assert_eq!(changes.len(), 2);
        assert_eq!(
            (*changes[0].0, changes[0].1, changes[0].2),
            (reacted.reactions_as_of, &message.id, &reacted.reactions)
        );
        assert_eq!(*changes[1].0, cleared.reactions_as_of);
        assert!(
            changes[1].2.is_empty(),
            "the complete new list, not a delta"
        );
        if let Event::ReactionsChanged {
            conversation_id, ..
        } = &events[0].event
        {
            assert_eq!(*conversation_id, app.general);
        }
    }
    finish(c).await;
}

#[sqlx::test]
async fn setting_what_already_holds_commits_nothing(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "x").await;
    let mut ws = subscribed(app, &c.author).await;

    // Removing what is not there.
    let head = app.head().await;
    let absent = app.react(&c.member, message.id, THUMBS, false).await;
    assert!(absent.reactions.is_empty());
    assert_eq!(
        (absent.reactions_as_of, app.head().await),
        (EventSeq::ZERO, head)
    );

    // Adding twice: the second changes nothing, reserves no position, appends no event.
    let first = app.react(&c.member, message.id, THUMBS, true).await;
    let head = app.head().await;
    let again = app.react(&c.member, message.id, THUMBS, true).await;
    assert_eq!(again, first);
    assert_eq!(app.head().await, head, "no position was reserved");
    assert_eq!(app.reaction_events(message.id).await.len(), 1);

    // Removing twice likewise, and removing one of several leaves the others alone.
    app.react(&c.author, message.id, PARTY, true).await;
    let cleared = app.react(&c.member, message.id, THUMBS, false).await;
    let head = app.head().await;
    assert_eq!(
        app.react(&c.member, message.id, THUMBS, false).await,
        cleared
    );
    assert_eq!(app.head().await, head);
    assert_eq!(cleared.reactions, vec![summary(PARTY, &[c.author.user.id])]);
    assert_eq!(app.reaction_events(message.id).await.len(), 3);

    let marker = app.register("marker").await;
    let events = ws.events_until_marker(marker.user.id).await;
    assert_eq!(reaction_changes(&events).len(), 3);
    finish(c).await;
}

#[sqlx::test]
async fn reactions_are_listed_by_first_use_and_people_by_when_they_reacted(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "x").await;
    let (a, b, author) = (&c.member, &c.admin, &c.author);

    app.react(a, message.id, THUMBS, true).await;
    app.react(b, message.id, PARTY, true).await;
    app.react(author, message.id, THUMBS, true).await;
    let listed = app.react(a, message.id, HEART, true).await;
    assert_eq!(
        listed.reactions,
        vec![
            summary(THUMBS, &[a.user.id, author.user.id]),
            summary(PARTY, &[b.user.id]),
            summary(HEART, &[a.user.id]),
        ]
    );

    // An emoji's place is its earliest surviving reaction, so when the first person withdraws,
    // the emoji moves to where the next person reacted.
    let listed = app.react(a, message.id, THUMBS, false).await;
    assert_eq!(
        listed.reactions,
        vec![
            summary(PARTY, &[b.user.id]),
            summary(THUMBS, &[author.user.id]),
            summary(HEART, &[a.user.id]),
        ]
    );
    // An emoji nobody uses is dropped.
    app.react(author, message.id, THUMBS, false).await;
    let listed = app.react(b, message.id, PARTY, false).await;
    assert_eq!(listed.reactions, vec![summary(HEART, &[a.user.id])]);

    // Reacting again after withdrawing goes to the end, under the emoji and in the list.
    app.react(b, message.id, HEART, true).await;
    app.react(a, message.id, HEART, false).await;
    let listed = app.react(a, message.id, HEART, true).await;
    assert_eq!(
        listed.reactions,
        vec![summary(HEART, &[b.user.id, a.user.id])]
    );
    assert_eq!(app.get_message(&c.stranger, message.id).await, listed);
    finish(c).await;
}

#[sqlx::test]
async fn a_message_holds_at_most_fifty_different_emoji(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    assert_eq!(MAX_REACTION_KINDS, 50);
    let message = app.send(&c.author, app.general, "x").await;
    let glyphs: Vec<String> = emoji::groups()
        .next()
        .into_iter()
        .flat_map(emoji::in_group)
        .take(MAX_REACTION_KINDS + 2)
        .map(|e| e.as_str().to_owned())
        .collect();
    assert_eq!(glyphs.len(), MAX_REACTION_KINDS + 2);

    for glyph in &glyphs[..MAX_REACTION_KINDS] {
        app.react(&c.member, message.id, glyph, true).await;
    }
    assert_eq!(
        app.get_message(&c.member, message.id).await.reactions.len(),
        MAX_REACTION_KINDS
    );

    // The 51st different emoji is refused, from anyone, and leaves nothing behind.
    let head = app.head().await;
    let refused = app
        .react_raw(&c.author, message.id, &glyphs[MAX_REACTION_KINDS], true)
        .await;
    assert_eq!(refused.status().as_u16(), 422);
    denied(refused, ErrorCode::LimitReached).await;
    assert_eq!(app.head().await, head);
    assert_eq!(
        app.reaction_rows(message.id).await,
        MAX_REACTION_KINDS as i64
    );

    // Adding to a kind that exists is not a new kind, however full the message is.
    let shared = app.react(&c.author, message.id, &glyphs[0], true).await;
    assert_eq!(shared.reactions.len(), MAX_REACTION_KINDS);
    assert_eq!(shared.reactions[0].users.len(), 2);
    // So is asking again for a reaction you already have.
    app.react(&c.member, message.id, &glyphs[1], true).await;

    // One person leaving a kind that another still holds frees nothing...
    app.react(&c.member, message.id, &glyphs[0], false).await;
    denied(
        app.react_raw(&c.admin, message.id, &glyphs[MAX_REACTION_KINDS], true)
            .await,
        ErrorCode::LimitReached,
    )
    .await;
    // ...but the last person leaving a kind does, and the place can be taken by a new one.
    app.react(&c.author, message.id, &glyphs[0], false).await;
    let after = app
        .react(&c.admin, message.id, &glyphs[MAX_REACTION_KINDS], true)
        .await;
    assert_eq!(after.reactions.len(), MAX_REACTION_KINDS);
    // At the limit again: withdrawing a kind and putting the same one back is always fine.
    app.react(&c.member, message.id, &glyphs[1], false).await;
    let restored = app.react(&c.member, message.id, &glyphs[1], true).await;
    assert_eq!(restored.reactions.len(), MAX_REACTION_KINDS);
    assert_eq!(
        restored.reactions.last().and_then(|r| r.emoji.glyph()),
        Some(glyphs[1].as_str()),
        "a restored reaction goes to the end"
    );
    // The place that was freed is gone again, for a different emoji.
    denied(
        app.react_raw(&c.owner, message.id, &glyphs[0], true).await,
        ErrorCode::LimitReached,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn a_body_that_is_not_an_emoji_is_an_invalid_request_not_a_server_error(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "x").await;
    let head = app.head().await;

    for body in [
        json!({"emoji": "hello", "present": true}),
        json!({"emoji": "", "present": true}),
        json!({"emoji": "👍👍", "present": true}),
        json!({"emoji": ":thumbsup:", "present": true}),
        json!({"emoji": 5, "present": true}),
        json!({"emoji": "👍"}),
        json!({"present": true}),
        json!({"emoji": "👍", "present": "yes"}),
    ] {
        let response = app.react_body(&c.member, message.id, body.clone()).await;
        assert_eq!(response.status().as_u16(), 400, "{body}");
        denied(response, ErrorCode::InvalidRequest).await;
    }
    // Not even JSON.
    let response = app
        .http
        .put(app.url(&lazy_protocol::api::paths::reactions(message.id)))
        .bearer_auth(&c.member.token)
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    denied(response, ErrorCode::InvalidRequest).await;
    assert_eq!(app.head().await, head);
    assert_eq!(app.reaction_rows(message.id).await, 0);
    finish(c).await;
}

#[sqlx::test]
async fn however_an_emoji_is_spelled_it_is_one_canonical_reaction(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "x").await;
    assert_ne!(HEART, HEART_BARE);

    // Typed without the variation selector, it is stored and returned with it.
    let first = app.react(&c.member, message.id, HEART_BARE, true).await;
    assert_eq!(first.reactions[0].emoji.glyph(), Some(HEART));
    // The other spelling from someone else joins the same reaction.
    let second = app.react(&c.admin, message.id, HEART, true).await;
    assert_eq!(
        second.reactions,
        vec![summary(HEART, &[c.member.user.id, c.admin.user.id])]
    );
    // Withdrawing by the other spelling withdraws it.
    app.react(&c.member, message.id, HEART_BARE, false).await;
    assert_eq!(
        app.get_message(&c.author, message.id).await.reactions,
        vec![summary(HEART, &[c.admin.user.id])]
    );
    // A skin tone is a different reaction.
    let toned = app.react(&c.member, message.id, "👍🏽", true).await;
    app.react(&c.member, message.id, THUMBS, true).await;
    assert_eq!(
        app.get_message(&c.author, message.id).await.reactions.len(),
        3
    );
    assert_ne!(toned.reactions.last().unwrap().emoji, emoji_of(THUMBS));

    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT emoji FROM message_reactions WHERE message_id = $1 ORDER BY emoji",
    )
    .bind(message.id.as_uuid())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert!(stored.contains(&HEART.to_owned()) && !stored.contains(&HEART_BARE.to_owned()));
    finish(c).await;
}

#[sqlx::test]
async fn who_may_react_and_what_is_hidden_from_the_rest(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "x").await;

    // Everyone who could post may react to anyone's message, themselves and moderators included.
    for who in [&c.author, &c.member, &c.admin, &c.owner, &c.stranger] {
        let reacted = app.react(who, message.id, THUMBS, true).await;
        assert!(reacted.reactions[0].users.contains(&who.user.id));
    }

    // A public channel you have not joined can be read but not reacted in.
    let team = app.create_channel(&c.author, "team", "public").await;
    let in_team = app.send(&c.author, team.id, "in team").await;
    denied(
        app.react_raw(&c.member, in_team.id, THUMBS, true).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.react_raw(&c.admin, in_team.id, THUMBS, true).await,
        ErrorCode::Forbidden,
    )
    .await;
    // A private channel does not exist for people outside it, owner and admin included.
    let hush = app.create_channel(&c.author, "hush", "private").await;
    let secret = app.send(&c.author, hush.id, "secret").await;
    for outsider in [&c.owner, &c.admin, &c.stranger] {
        denied(
            app.react_raw(outsider, secret.id, THUMBS, true).await,
            ErrorCode::NotFound,
        )
        .await;
    }
    // A message that does not exist, and someone outside the workspace.
    denied(
        app.react_raw(&c.author, MessageId::generate(), THUMBS, true)
            .await,
        ErrorCode::NotFound,
    )
    .await;
    let gone = app.register("gone").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(gone.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    denied(
        app.react_raw(&gone, message.id, THUMBS, true).await,
        ErrorCode::NotFound,
    )
    .await;

    // An archived channel is closed to reactions, and only that.
    assert!(
        app.act(&c.member, team.id, "join")
            .await
            .status()
            .is_success()
    );
    assert!(
        app.act(&c.author, team.id, "archive")
            .await
            .status()
            .is_success()
    );
    denied(
        app.react_raw(&c.member, in_team.id, THUMBS, true).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.react_raw(&c.author, in_team.id, THUMBS, true).await,
        ErrorCode::Forbidden,
    )
    .await;
    assert!(
        app.get_message_raw(&c.member, in_team.id)
            .await
            .status()
            .is_success()
    );

    // A deleted message has nothing to react to.
    let doomed = app.send(&c.author, app.general, "doomed").await;
    app.delete(&c.author, doomed.id).await;
    denied(
        app.react_raw(&c.member, doomed.id, THUMBS, true).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.react_raw(&c.member, doomed.id, THUMBS, false).await,
        ErrorCode::NotFound,
    )
    .await;

    // None of the refusals changed anything.
    assert_eq!(app.reaction_rows(in_team.id).await, 0);
    assert_eq!(app.reaction_rows(secret.id).await, 0);
    assert_eq!(app.reaction_rows(doomed.id).await, 0);
    finish(c).await;
}

#[sqlx::test]
async fn someone_who_leaves_keeps_their_past_reactions_but_adds_no_more(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let team = app.create_channel(&c.author, "team", "public").await;
    assert!(
        app.act(&c.member, team.id, "join")
            .await
            .status()
            .is_success()
    );
    let message = app.send(&c.author, team.id, "x").await;
    app.react(&c.member, message.id, THUMBS, true).await;
    assert!(
        app.act(&c.member, team.id, "leave")
            .await
            .status()
            .is_success()
    );

    let after = app.get_message(&c.author, message.id).await;
    assert_eq!(after.reactions, vec![summary(THUMBS, &[c.member.user.id])]);
    denied(
        app.react_raw(&c.member, message.id, PARTY, true).await,
        ErrorCode::Forbidden,
    )
    .await;
    // Withdrawing is a reaction change too, so it needs the same standing.
    denied(
        app.react_raw(&c.member, message.id, THUMBS, false).await,
        ErrorCode::Forbidden,
    )
    .await;
    finish(c).await;
}

/// Reactions are events: a subscriber who was away catches up with each change in order, a member
/// removed meanwhile receives nothing, and people outside a private channel never hear of them.
#[sqlx::test]
async fn reactions_reach_members_live_and_on_replay_and_nobody_else(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hush = app.create_channel(&c.author, "hush", "private").await;
    assert!(
        app.add_members(&c.author, hush.id, &[&c.member, &c.admin])
            .await
            .status()
            .is_success()
    );
    let message = app.send(&c.author, hush.id, "private talk").await;
    let cursor = app.head().await;
    let mut live = subscribed(app, &c.author).await;
    let mut outsider = subscribed(app, &c.stranger).await;

    // While the member and the admin are "away" (they hold the cursor), the author reacts, the
    // admin is removed, and then more reactions arrive.
    app.react(&c.author, message.id, THUMBS, true).await;
    app.react(&c.member, message.id, PARTY, true).await;
    assert!(
        app.remove_member(&c.author, hush.id, &c.admin)
            .await
            .status()
            .is_success()
    );
    let last = app.react(&c.author, message.id, HEART, true).await;
    let marker = app.register("marker").await;

    // The member who stayed receives every change, in order, each with the complete list.
    let mut ws = app.connect_ws(&c.member).await;
    ws.subscribe(app.workspace_id, cursor).await;
    let (replay, _) = ws.replay().await;
    let changes = reaction_changes(&replay);
    assert_eq!(changes.len(), 3);
    assert!(changes.windows(2).all(|pair| pair[0].0 < pair[1].0));
    assert_eq!(
        changes.iter().map(|c| c.2.len()).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(*changes[2].0, last.reactions_as_of);
    assert_eq!(changes[2].2, &last.reactions);

    // The removed admin gets their own removal and not one reaction, before or after it.
    let mut ws = app.connect_ws(&c.admin).await;
    ws.subscribe(app.workspace_id, cursor).await;
    let (replay, _) = ws.replay().await;
    assert!(reaction_changes(&replay).is_empty());
    assert!(replay.iter().any(|e| matches!(&e.event,
        Event::MemberRemoved { conversation_id, user_id }
            if *conversation_id == hush.id && *user_id == c.admin.user.id)));

    // A connected author heard them live, and someone outside the channel heard nothing.
    assert_eq!(
        reaction_changes(&live.events_until_marker(marker.user.id).await).len(),
        3
    );
    assert!(
        outsider
            .events_until_marker(marker.user.id)
            .await
            .iter()
            .all(|e| !matches!(e.event, Event::ReactionsChanged { .. }))
    );
    finish(c).await;
}

#[sqlx::test]
async fn history_carries_reactions_and_their_version(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let mut sent = Vec::new();
    for n in 0..6 {
        sent.push(app.send(&c.author, app.general, &format!("m{n}")).await);
    }
    app.react(&c.member, sent[1].id, THUMBS, true).await;
    app.react(&c.admin, sent[1].id, THUMBS, true).await;
    let heart = app.react(&c.member, sent[4].id, HEART, true).await;
    let toggled = app.react(&c.member, sent[4].id, PARTY, true).await;
    app.react(&c.member, sent[4].id, PARTY, false).await;
    let final_state = app.get_message(&c.author, sent[4].id).await;
    assert!(final_state.reactions_as_of > toggled.reactions_as_of);
    assert_eq!(heart.reactions.len(), 1);

    let page = app.history_ok(&c.owner, app.general, &[]).await;
    assert_eq!(page.messages.len(), 6);
    let by_id = |id: MessageId| page.messages.iter().find(|m| m.id == id).unwrap();
    assert_eq!(
        by_id(sent[1].id).reactions,
        vec![summary(THUMBS, &[c.member.user.id, c.admin.user.id])]
    );
    assert_eq!(by_id(sent[4].id), &final_state);
    assert_eq!(
        by_id(sent[4].id).reactions,
        vec![summary(HEART, &[c.member.user.id])]
    );
    for untouched in [0, 2, 3, 5] {
        let message = by_id(sent[untouched].id);
        assert!(message.reactions.is_empty());
        assert_eq!(message.reactions_as_of, EventSeq::ZERO, "never reacted to");
    }
    // Pages cut the same way.
    let tail = app
        .history_ok(&c.owner, app.general, &[("limit", "2".into())])
        .await;
    assert_eq!(
        tail.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![sent[4].id, sent[5].id]
    );
    assert_eq!(tail.messages[0].reactions.len(), 1);
    finish(c).await;
}

#[sqlx::test]
async fn every_message_a_client_receives_carries_its_reactions(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "first").await;
    assert!(message.reactions.is_empty() && message.reactions_as_of == EventSeq::ZERO);
    let reacted = app.react(&c.member, message.id, THUMBS, true).await;
    let mut ws = subscribed(app, &c.member).await;

    // An edit does not disturb the reactions, and its response and event show them as they were
    // at that moment, with their version.
    let operation = OperationId::generate();
    let edit_response = app
        .edit_raw(&c.author, message.id, operation, 1, "second")
        .await;
    assert!(edit_response.status().is_success());
    let edited = edit_response
        .json::<MessageResponse>()
        .await
        .unwrap()
        .message;
    assert_eq!(edited.revision, 2);
    assert_eq!(edited.reactions, reacted.reactions);
    assert_eq!(edited.reactions_as_of, reacted.reactions_as_of);
    let marker = app.register("marker").await;
    let events = ws.events_until_marker(marker.user.id).await;
    match &events[0].event {
        Event::MessageEdited { message, .. } => assert_eq!(message, &edited),
        other => panic!("unexpected {other:?}"),
    }

    // Reacting does not change the content, the revision, or when it was edited.
    let later = app.react(&c.admin, message.id, PARTY, true).await;
    assert_eq!(
        (later.revision, later.edited_at, &later.document),
        (edited.revision, edited.edited_at, &edited.document)
    );
    assert!(later.reactions_as_of > edited.reactions_as_of);

    // A repeated edit, and the operation status, answer with the message as it now is.
    let again = app
        .edit_raw(&c.author, message.id, operation, 1, "second")
        .await;
    let again = again.json::<MessageResponse>().await.unwrap().message;
    assert_eq!(
        again.reactions, later.reactions,
        "the reactions are current; only the words are original"
    );
    match app.operation(&c.author, operation).await {
        OperationStatus::EditCommitted { message } => {
            assert_eq!(message.reactions, later.reactions)
        }
        other => panic!("unexpected {other:?}"),
    }
    // An edit that changes nothing returns the reactions too.
    let noop = app
        .edit_raw(&c.author, message.id, OperationId::generate(), 2, "second")
        .await;
    assert_eq!(
        noop.json::<MessageResponse>()
            .await
            .unwrap()
            .message
            .reactions,
        later.reactions
    );
    // A send that is repeated returns the message with them.
    let send_operation = OperationId::generate();
    let sent = app
        .send_raw(&c.author, app.general, send_operation, "third")
        .await;
    let sent = sent
        .json::<lazy_protocol::SendMessageResponse>()
        .await
        .unwrap()
        .message;
    app.react(&c.member, sent.id, HEART, true).await;
    let resent = app
        .send_raw(&c.author, app.general, send_operation, "third")
        .await;
    let resent = resent
        .json::<lazy_protocol::SendMessageResponse>()
        .await
        .unwrap()
        .message;
    assert_eq!(resent.reactions, vec![summary(HEART, &[c.member.user.id])]);
    finish(c).await;
}

#[sqlx::test]
async fn deleting_a_message_removes_its_reactions(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "x").await;
    app.react(&c.member, message.id, THUMBS, true).await;
    app.react(&c.admin, message.id, PARTY, true).await;
    assert_eq!(app.reaction_rows(message.id).await, 2);
    let changes_before = app.reaction_events(message.id).await;
    assert_eq!(changes_before.len(), 2);

    let tombstone = app.delete(&c.author, message.id).await;
    assert!(tombstone.deleted && tombstone.reactions.is_empty());
    assert_eq!(app.reaction_rows(message.id).await, 0);
    // Clearing them is a change made at the deletion's position, so a client that keeps the
    // highest version ends with none.
    let events = app.message_events(message.id).await;
    let deleted_at = events
        .iter()
        .find(|(_, kind)| kind == "message_deleted")
        .unwrap()
        .0;
    assert_eq!(tombstone.reactions_as_of.get(), deleted_at);
    assert!(tombstone.reactions_as_of > EventSeq::new(*changes_before.last().unwrap()).unwrap());
    assert_eq!(app.get_message(&c.member, message.id).await, tombstone);

    // The earlier reaction events are left as they were: they hold no content.
    assert_eq!(app.reaction_events(message.id).await, changes_before);
    denied(
        app.react_raw(&c.member, message.id, THUMBS, true).await,
        ErrorCode::NotFound,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn reactions_never_touch_unread_counts_or_read_positions(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let first = app.send(&c.author, app.general, "one").await;
    app.send(&c.author, app.general, "two").await;
    let mut ws = subscribed(app, &c.member).await;
    let before = app.state_of(&c.member, app.general).await;
    assert_eq!(before.unread_count, 2);

    // Reacting to a message neither reads it nor adds to what is unread, for the reactor or anyone.
    app.react(&c.member, first.id, THUMBS, true).await;
    app.react(&c.admin, first.id, THUMBS, true).await;
    app.react(&c.author, first.id, PARTY, true).await;
    assert_eq!(app.state_of(&c.member, app.general).await, before);
    let authors = app.state_of(&c.author, app.general).await;
    assert_eq!(authors.unread_count, 0);

    // No personal-state event came with them.
    let marker = app.register("marker").await;
    let events = ws.events_until_marker(marker.user.id).await;
    assert_eq!(reaction_changes(&events).len(), 3);
    assert!(
        events
            .iter()
            .all(|e| !matches!(e.event, Event::UserStateChanged { .. }))
    );
    finish(c).await;
}

// --- Races ------------------------------------------------------------------------------------

/// Registers `count` members named `{prefix}0`, `{prefix}1`, ...
async fn people(app: &App, prefix: &str, count: usize) -> Vec<Account> {
    let mut accounts = Vec::new();
    for n in 0..count {
        accounts.push(app.register(&format!("{prefix}{n}")).await);
    }
    accounts
}

fn put_reaction(
    app: &App,
    message: MessageId,
    who: &Account,
    glyph: &str,
    present: bool,
) -> tokio::task::JoinHandle<u16> {
    let http = app.http.clone();
    let url = app.url(&lazy_protocol::api::paths::reactions(message));
    let token = who.token.clone();
    let body = json!({"emoji": glyph, "present": present});
    tokio::spawn(async move {
        http.put(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    })
}

#[sqlx::test]
async fn many_people_reacting_with_the_same_emoji_at_once(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "x").await;
    let crowd = people(app, "fan", 12).await;
    let baseline = app.head().await;

    // Each person asks twice at once; only one of each pair changes anything.
    let requests = crowd.iter().flat_map(|who| {
        [
            put_reaction(app, message.id, who, THUMBS, true),
            put_reaction(app, message.id, who, THUMBS, true),
        ]
    });
    let statuses = tokio::time::timeout(DEADLINE, join_all(requests))
        .await
        .expect("the reactions finish");
    assert!(statuses.into_iter().all(|s| s.unwrap() == 200));

    let message = app.get_message(&c.author, message.id).await;
    assert_eq!(message.reactions.len(), 1);
    assert_eq!(message.reactions[0].users.len(), crowd.len());
    assert_eq!(app.reaction_events(message.id).await.len(), crowd.len());
    // The people are listed in the order their reactions took effect.
    let in_order: Vec<String> = sqlx::query_scalar(
        "SELECT user_id::text FROM message_reactions WHERE message_id = $1 ORDER BY seq",
    )
    .bind(message.id.as_uuid())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    let listed: Vec<String> = message.reactions[0]
        .users
        .iter()
        .map(|u| u.to_string())
        .collect();
    assert_eq!(listed, in_order);
    assert_log(app, &c.author, message.id, baseline, crowd.len()).await;
    finish(c).await;
}

async fn assert_log(
    app: &App,
    reader: &Account,
    message: MessageId,
    baseline: EventSeq,
    changes: usize,
) {
    let events: Vec<(i64, serde_json::Value)> = sqlx::query_as(
        "SELECT seq, payload -> 'reactions' FROM events \
         WHERE message_id = $1 AND kind = 'reactions_changed' ORDER BY seq",
    )
    .bind(message.as_uuid())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        events.len(),
        changes,
        "one event per change that changed something"
    );

    let mut previous: BTreeSet<(String, String)> = BTreeSet::new();
    for (seq, list) in &events {
        let list: Vec<ReactionSummary> = serde_json::from_value(list.clone()).unwrap();
        let now: BTreeSet<(String, String)> = pairs(&list)
            .into_iter()
            .map(|(emoji, user)| (emoji, user.to_string()))
            .collect();
        assert_eq!(
            now.symmetric_difference(&previous).count(),
            1,
            "the event at {seq} should differ from the one before by exactly one reaction"
        );
        previous = now;
    }
    let log = app.log_positions_after(baseline).await;
    assert_eq!(
        log,
        (baseline.get() + 1..=app.head().await.get()).collect::<Vec<_>>(),
        "positions have no holes"
    );
    let current = app.get_message(reader, message).await;
    assert_eq!(
        current.reactions_as_of.get(),
        events.last().map(|(seq, _)| *seq).unwrap_or(0)
    );
    let last: Vec<ReactionSummary> =
        serde_json::from_value(events.last().unwrap().1.clone()).unwrap();
    assert_eq!(current.reactions, last);
}

#[sqlx::test]
async fn many_people_reacting_with_different_emoji_at_once(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "x").await;
    let crowd = people(app, "fan", 12).await;
    let glyphs: Vec<String> = emoji::groups()
        .next()
        .into_iter()
        .flat_map(emoji::in_group)
        .take(crowd.len())
        .map(|e| e.as_str().to_owned())
        .collect();
    let baseline = app.head().await;

    let requests = crowd
        .iter()
        .zip(&glyphs)
        .map(|(who, glyph)| put_reaction(app, message.id, who, glyph, true));
    let statuses = tokio::time::timeout(DEADLINE, join_all(requests))
        .await
        .expect("the reactions finish");
    assert!(statuses.into_iter().all(|s| s.unwrap() == 200));

    let current = app.get_message(&c.author, message.id).await;
    assert_eq!(current.reactions.len(), crowd.len());
    // Listed by first use: the order the changes were committed.
    let order: Vec<String> = sqlx::query_scalar(
        "SELECT emoji FROM message_reactions WHERE message_id = $1 ORDER BY seq",
    )
    .bind(message.id.as_uuid())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        current
            .reactions
            .iter()
            .map(|r| r.emoji.to_string())
            .collect::<Vec<_>>(),
        order
    );
    assert_log(app, &c.author, message.id, baseline, crowd.len()).await;
    finish(c).await;
}

/// Everyone switches their own reaction on, off, and on again while everyone else does the same.
#[sqlx::test]
async fn toggling_reactions_all_at_once_settles_into_a_consistent_log(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "x").await;
    let crowd = people(app, "fan", 8).await;
    let baseline = app.head().await;

    let workers = crowd.iter().enumerate().map(|(index, who)| {
        let glyph = if index % 2 == 0 { THUMBS } else { PARTY };
        let steps: Vec<_> = [true, false, true]
            .into_iter()
            .map(|present| (present, who.token.clone()))
            .collect();
        let http = app.http.clone();
        let url = app.url(&lazy_protocol::api::paths::reactions(message.id));
        tokio::spawn(async move {
            for (present, token) in steps {
                let response = http
                    .put(&url)
                    .bearer_auth(token)
                    .json(&json!({"emoji": glyph, "present": present}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status().as_u16(), 200);
            }
        })
    });
    tokio::time::timeout(DEADLINE, join_all(workers))
        .await
        .expect("the togglers finish")
        .into_iter()
        .for_each(|r| r.unwrap());

    // Each person's three requests were serial, so each changed something: 3 events apiece.
    let current = app.get_message(&c.author, message.id).await;
    assert_eq!(pairs(&current.reactions).len(), crowd.len());
    assert_log(app, &c.author, message.id, baseline, crowd.len() * 3).await;
    finish(c).await;
}

/// An edit's event carries the reactions as they were when the edit committed: exactly the list
/// the latest earlier `reactions_changed` event announced, however the two interleave.
#[sqlx::test]
async fn edits_and_reactions_racing_agree_about_what_the_reactions_were(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.author, app.general, "v0").await;
    let crowd = people(app, "fan", 8).await;
    let glyphs: Vec<String> = emoji::groups()
        .next()
        .into_iter()
        .flat_map(emoji::in_group)
        .take(crowd.len())
        .map(|e| e.as_str().to_owned())
        .collect();

    let reactors = crowd
        .iter()
        .zip(&glyphs)
        .map(|(who, glyph)| put_reaction(app, message.id, who, glyph, true))
        .collect::<Vec<_>>();
    let editor = {
        let (app_http, url, token) = (
            app.http.clone(),
            app.url(&lazy_protocol::api::paths::message(message.id)),
            c.author.token.clone(),
        );
        tokio::spawn(async move {
            for (n, revision) in (0..8).zip(1u32..) {
                let body = json!({
                    "operation_id": OperationId::generate(), "base_revision": revision,
                    "document": lazy_domain::MessageDocument::from_markdown(&format!("v{}", n + 1)).unwrap(),
                });
                let response = app_http
                    .patch(&url)
                    .bearer_auth(&token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status().as_u16(), 200);
            }
        })
    };
    tokio::time::timeout(DEADLINE, async {
        for reactor in reactors {
            assert_eq!(reactor.await.unwrap(), 200);
        }
        editor.await.unwrap();
    })
    .await
    .expect("the race finishes");

    let events: Vec<(i64, String, serde_json::Value)> =
        sqlx::query_as("SELECT seq, kind, payload FROM events WHERE message_id = $1 ORDER BY seq")
            .bind(message.id.as_uuid())
            .fetch_all(&app.pool)
            .await
            .unwrap();
    let (mut current, mut current_as_of) = (json!([]), 0i64);
    let mut edits = 0;
    for (seq, kind, payload) in events {
        match kind.as_str() {
            "reactions_changed" => {
                current = payload["reactions"].clone();
                current_as_of = seq;
            }
            "message_edited" => {
                edits += 1;
                let shown = payload["message"]
                    .get("reactions")
                    .cloned()
                    .unwrap_or(json!([]));
                assert_eq!(
                    shown, current,
                    "the edit at {seq} should show the reactions as they were"
                );
                let as_of = payload["message"]
                    .get("reactions_as_of")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                assert_eq!(as_of, current_as_of);
            }
            _ => {}
        }
    }
    assert_eq!(edits, 8);
    finish(c).await;
}

/// Reactors race a deletion: the deletion wins, every reaction that was accepted is ordered before
/// it, the rest are refused, and nothing survives.
#[sqlx::test]
async fn reactions_racing_a_delete_are_ordered_before_it_or_refused(pool: PgPool) {
    const ROUNDS: usize = 10;
    let c = chat(pool).await;
    let app = &c.app;
    let crowd = people(app, "fan", 6).await;
    let glyphs: Vec<String> = emoji::groups()
        .next()
        .into_iter()
        .flat_map(emoji::in_group)
        .take(3)
        .map(|e| e.as_str().to_owned())
        .collect();

    for round in 0..ROUNDS {
        let message = app
            .send(&c.author, app.general, &format!("round {round}"))
            .await;
        let url = app.url(&lazy_protocol::api::paths::message(message.id));
        let deleter = {
            let (http, token) = (app.http.clone(), c.author.token.clone());
            tokio::spawn(async move {
                http.delete(url)
                    .bearer_auth(token)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        };
        let reactors = crowd.iter().map(|who| {
            let (http, url, token) = (
                app.http.clone(),
                app.url(&lazy_protocol::api::paths::reactions(message.id)),
                who.token.clone(),
            );
            let glyphs = glyphs.clone();
            tokio::spawn(async move {
                let mut accepted = 0usize;
                for glyph in glyphs {
                    let response = http
                        .put(&url)
                        .bearer_auth(&token)
                        .json(&json!({"emoji": glyph, "present": true}))
                        .send()
                        .await
                        .unwrap();
                    match response.status().as_u16() {
                        200 => accepted += 1,
                        404 => break,
                        other => panic!("unexpected status {other}"),
                    }
                }
                accepted
            })
        });
        let (deleted, accepted) = tokio::time::timeout(DEADLINE, async {
            let deleted = deleter.await.unwrap();
            let accepted: usize = join_all(reactors)
                .await
                .into_iter()
                .map(|a| a.unwrap())
                .sum();
            (deleted, accepted)
        })
        .await
        .expect("the round finishes");
        assert_eq!(deleted, 200);

        assert_eq!(app.reaction_rows(message.id).await, 0, "round {round}");
        let tombstone = app.get_message(&c.author, message.id).await;
        assert!(tombstone.deleted && tombstone.reactions.is_empty());
        let events = app.message_events(message.id).await;
        let deleted_at = events
            .iter()
            .find(|(_, k)| k == "message_deleted")
            .unwrap()
            .0;
        let changes = app.reaction_events(message.id).await;
        assert_eq!(
            changes.len(),
            accepted,
            "every accepted reaction is in the log"
        );
        assert!(
            changes.iter().all(|seq| *seq < deleted_at),
            "none after the deletion"
        );
        assert_eq!(tombstone.reactions_as_of.get(), deleted_at);
    }
    finish(c).await;
}
