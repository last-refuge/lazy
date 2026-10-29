//! The content cache: restarts that show saved content immediately and resume from the saved
//! cursor, and the situations in which the cache must be thrown away.

mod common;

use common::{Driver, World, channel, people, read_state, seq, user};
use lazy_client_core::{
    Change, ConnectionState, ContentCache, Delivery, Effect, HistoryStatus, Input, ItemKey,
    JournalEntry, JournalKind, MAX_CACHED_CONVERSATIONS, MAX_CACHED_MESSAGES, RequestError,
    SessionPhase, SocketEvent, StartupState,
};
use lazy_domain::{ConversationId, MessageDocument, OperationId, Timestamp};
use lazy_protocol::{
    Conversation, ErrorCode, HistoryPage, OperationStatus, SendMessageResponse, ServerFrame,
    SnapshotReason,
};

/// A cache produced the way the client produces one: a session whose history was loaded.
fn warm_cache(world: &World, general: &[i64], random: Option<&[i64]>) -> ContentCache {
    let mut driver = Driver::ready(world);
    let page = |conversation, seqs: &[i64]| HistoryPage {
        messages: seqs
            .iter()
            .map(|at| world.message(conversation, &world.other, *at, &format!("msg {at}")))
            .collect(),
        has_more: false,
    };
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(page(world.channel.id, general)),
    });
    if let Some(random) = random {
        driver.send(Input::SelectConversation(world.second_channel.id));
        driver.send(Input::HistoryLoaded {
            conversation: world.second_channel.id,
            generation: 1,
            result: Ok(page(world.second_channel.id, random)),
        });
        driver.send(Input::SelectConversation(world.channel.id));
    }
    driver.last_cache().expect("a cache was produced").clone()
}

fn restart(world: &World, cache: Option<ContentCache>) -> (Driver, lazy_client_core::Output) {
    restart_with_journal(world, cache, Vec::new())
}

fn restart_with_journal(
    world: &World,
    cache: Option<ContentCache>,
    journal: Vec<JournalEntry>,
) -> (Driver, lazy_client_core::Output) {
    let _ = world;
    let mut driver = Driver::new();
    let out = driver.send(Input::Started(StartupState {
        session_restored: true,
        cache,
        journal,
        ..Default::default()
    }));
    (driver, out)
}

fn subscriptions(driver: &Driver) -> Vec<i64> {
    driver.effects_matching(|e| match e {
        Effect::Subscribe { after, .. } => Some(after.get()),
        _ => None,
    })
}

fn history_fetches(driver: &Driver) -> Vec<ConversationId> {
    driver.effects_matching(|e| match e {
        Effect::FetchHistory { conversation, .. } => Some(*conversation),
        _ => None,
    })
}

fn texts(driver: &Driver, conversation: ConversationId) -> Vec<String> {
    driver
        .timeline(conversation)
        .iter()
        .map(|row| row.document.plain_text())
        .collect()
}

fn world_with_head(head: i64) -> World {
    let mut world = World::new();
    world.head = seq(head);
    world
}

// ----- What gets cached -----

#[test]
fn the_cache_holds_the_directory_the_cursor_and_loaded_conversations_only() {
    let world = world_with_head(20);
    let mut driver = Driver::ready(&world);
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages: vec![world.message(world.channel.id, &world.other, 15, "loaded")],
            has_more: true,
        }),
    });
    // The other conversation only ever sees a live event, so its timeline is partial.
    driver.send(world.event(
        world.message(world.second_channel.id, &world.other, 21, "live only"),
        None,
    ));

    let cache = driver.last_cache().unwrap();
    assert_eq!(cache.user.id, world.me.id);
    let workspace = &cache.workspaces[0];
    assert_eq!(workspace.workspace.id, world.workspace.id);
    assert_eq!(workspace.conversations.len(), 2);
    assert_eq!(workspace.members.len(), 2);
    assert_eq!(workspace.cursor, seq(21));
    assert_eq!(
        workspace.timelines.len(),
        1,
        "only the loaded conversation is cached"
    );
    let timeline = &workspace.timelines[0];
    assert_eq!(timeline.conversation, world.channel.id);
    assert_eq!(timeline.messages.len(), 1);
    assert!(timeline.has_more);
}

#[test]
fn unconfirmed_messages_are_never_cached_but_confirmed_own_messages_are() {
    let world = world_with_head(20);
    let mut driver = Driver::ready(&world);
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages: vec![],
            has_more: false,
        }),
    });
    let out = driver.send(Input::SendMessage {
        conversation: world.channel.id,
        text: "mine".into(),
    });
    let Some(Effect::PersistSubmission(entry)) = out.effects.first().cloned() else {
        panic!("expected a journal write");
    };
    let operation = entry.operation_id;
    let cached = |d: &Driver| {
        d.last_cache().unwrap().workspaces[0].timelines[0]
            .messages
            .len()
    };
    assert_eq!(cached(&driver), 0, "a pending message is not cached");

    driver.send(Input::SubmissionPersisted { operation });
    let message = world.message(world.channel.id, &world.me, 21, "mine");
    driver.send(Input::SendResult {
        operation,
        result: Ok(SendMessageResponse { message }),
    });
    assert_eq!(
        cached(&driver),
        1,
        "once the server confirms it, it is cached"
    );
    // The acknowledgment arrived before the event, so the cursor is behind the message: safe.
    assert_eq!(driver.last_cache().unwrap().workspaces[0].cursor, seq(20));
}

#[test]
fn a_message_confirmed_below_the_loaded_range_is_not_cached_as_if_contiguous() {
    let world = world_with_head(30);
    let mut driver = Driver::ready(&world);
    let page: Vec<_> = (17..=20)
        .map(|at| world.message(world.channel.id, &world.other, at, "page"))
        .collect();
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages: page,
            has_more: true,
        }),
    });
    let out = driver.send(Input::SendMessage {
        conversation: world.channel.id,
        text: "late".into(),
    });
    let Some(Effect::PersistSubmission(entry)) = out.effects.first().cloned() else {
        panic!()
    };
    driver.send(Input::SubmissionPersisted {
        operation: entry.operation_id,
    });
    let stray = world.message(world.channel.id, &world.me, 14, "late");
    driver.send(Input::SendResult {
        operation: entry.operation_id,
        result: Ok(SendMessageResponse { message: stray }),
    });

    let messages = &driver.last_cache().unwrap().workspaces[0].timelines[0].messages;
    let seqs: Vec<_> = messages.iter().map(|m| m.seq.get()).collect();
    assert_eq!(
        seqs,
        [17, 18, 19, 20],
        "position 14 would claim a contiguity we don't have"
    );
}

#[test]
fn the_cache_is_bounded_per_conversation_and_overall() {
    let world = world_with_head(1000);
    let mut driver = Driver::ready(&world);
    let long: Vec<_> = (1..=(MAX_CACHED_MESSAGES as i64 + 50))
        .map(|at| world.message(world.channel.id, &world.other, at, "m"))
        .collect();
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages: long,
            has_more: false,
        }),
    });
    let timeline = &driver.last_cache().unwrap().workspaces[0].timelines[0];
    assert_eq!(timeline.messages.len(), MAX_CACHED_MESSAGES);
    assert!(
        timeline.has_more,
        "trimmed messages are reported as older ones that exist"
    );
    assert_eq!(
        timeline.messages.last().unwrap().seq.get(),
        MAX_CACHED_MESSAGES as i64 + 50
    );

    // Sixty loaded conversations: only the fifty most recently active are kept.
    let mut bootstrap = world.bootstrap();
    let extra: Vec<Conversation> = (0..60)
        .map(|i| channel(world.workspace.id, &format!("channel-{i}")))
        .collect();
    bootstrap.workspaces[0].conversations = extra.clone();
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    for (index, conversation) in extra.iter().enumerate() {
        driver.send(Input::SelectConversation(conversation.id));
        let generation = driver.effects_matching(|e| match e {
            Effect::FetchHistory {
                conversation: c,
                generation,
                ..
            } if *c == conversation.id => Some(*generation),
            _ => None,
        });
        driver.send(Input::HistoryLoaded {
            conversation: conversation.id,
            generation: generation[0],
            result: Ok(HistoryPage {
                messages: vec![world.message(
                    conversation.id,
                    &world.other,
                    10 + index as i64,
                    "m",
                )],
                has_more: false,
            }),
        });
    }
    let cache = driver.last_cache().unwrap();
    assert_eq!(cache.timeline_count(), MAX_CACHED_CONVERSATIONS);
    assert_eq!(
        cache.workspaces[0].conversations.len(),
        60,
        "the directory is complete"
    );
    let kept: Vec<_> = cache.workspaces[0]
        .timelines
        .iter()
        .map(|t| t.conversation)
        .collect();
    assert!(
        !kept.contains(&extra[0].id),
        "the least recently active is evicted first"
    );
    assert!(kept.contains(&extra[59].id));
}

// ----- Restarting with a cache -----

#[test]
fn a_restart_with_a_cache_shows_content_before_any_network_result() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5, 6, 7], Some(&[8]));
    let (driver, out) = restart(&world, Some(cache));

    // Nothing has come back from the network yet, but the app is ready with its content.
    assert_eq!(driver.core.phase(), &SessionPhase::Ready);
    assert_eq!(driver.core.me().unwrap().id, world.me.id);
    assert_eq!(driver.core.conversations().len(), 2);
    assert_eq!(driver.core.selected_conversation(), Some(world.channel.id));
    assert_eq!(
        texts(&driver, world.channel.id),
        ["msg 5", "msg 6", "msg 7"]
    );
    assert_eq!(texts(&driver, world.second_channel.id), ["msg 8"]);
    assert_eq!(driver.core.display_name(world.other.id), "ZOE");
    assert_eq!(
        driver.core.history_status(world.channel.id),
        &HistoryStatus::Ready { has_more: false }
    );
    assert_eq!(driver.core.connection(), ConnectionState::Connecting);
    assert!(out.changes.contains(&Change::TimelineReset {
        conversation: world.channel.id
    }));
    assert!(out.changes.contains(&Change::DirectoryChanged));
    assert!(
        out.effects.contains(&Effect::LoadBootstrap),
        "revalidation starts in the background"
    );
    assert!(history_fetches(&driver).is_empty());
}

#[test]
fn resuming_subscribes_after_the_cached_cursor_and_does_not_fetch_the_newest_page() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5, 6, 7], None);
    let (mut driver, _) = restart(&world, Some(cache));
    assert_eq!(
        subscriptions(&driver),
        [20],
        "resume after the cached cursor"
    );

    // The bootstrap confirms the directory; the log has moved on to 25 meanwhile.
    let mut bootstrap = world.bootstrap();
    bootstrap.workspaces[0].head = seq(25);
    driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    assert_eq!(
        subscriptions(&driver),
        [20],
        "never restarted from the bootstrap head"
    );
    assert!(
        history_fetches(&driver).is_empty(),
        "the newest page is not refetched"
    );
    assert_eq!(driver.core.phase(), &SessionPhase::Ready);

    // Replay fills the gap and the cursor catches up.
    let workspace = world.workspace.id;
    driver.send(Input::Socket(SocketEvent::Opened { workspace }));
    for at in 21..=25 {
        driver.send(world.event(
            world.message(
                world.channel.id,
                &world.other,
                at,
                &format!("replayed {at}"),
            ),
            None,
        ));
    }
    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: workspace,
        seq: seq(25),
    }));
    assert_eq!(driver.core.connection(), ConnectionState::Live);
    let all = texts(&driver, world.channel.id);
    assert_eq!(all.len(), 8);
    assert_eq!(all[3], "replayed 21");
    assert_eq!(driver.last_cache().unwrap().workspaces[0].cursor, seq(25));
}

#[test]
fn replayed_events_already_in_the_cache_are_not_duplicated() {
    let world = world_with_head(20);
    let mut cache = warm_cache(&world, &[7], None);
    // The cached timeline includes a message beyond the cursor, as happens when a history page
    // ran ahead of the events that had been applied.
    let ahead = world.message(world.channel.id, &world.other, 22, "ahead");
    cache.workspaces[0].timelines[0]
        .messages
        .push(ahead.clone());

    let (mut restarted, _) = restart(&world, Some(cache));
    restarted.send(world.event(ahead, None));
    restarted.send(world.event(
        world.message(world.channel.id, &world.other, 23, "new"),
        None,
    ));
    assert_eq!(
        texts(&restarted, world.channel.id),
        ["msg 7", "ahead", "new"]
    );
}

#[test]
fn revalidation_refreshes_the_directory_and_drops_conversations_the_person_lost() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5, 6], Some(&[8, 9]));
    let (mut driver, _) = restart(&world, Some(cache));
    assert_eq!(texts(&driver, world.second_channel.id).len(), 2);

    // The person has since lost access to #random and gained a new channel.
    let newcomer = Conversation {
        topic: Some("fresh".into()),
        ..channel(world.workspace.id, "new-channel")
    };
    let mut bootstrap = world.bootstrap();
    bootstrap.workspaces[0].conversations = vec![world.channel.clone(), newcomer.clone()];
    bootstrap.workspaces[0].members.push(user("mallory"));
    let out = driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));

    assert!(driver.core.conversation(world.second_channel.id).is_none());
    assert!(
        driver.core.timeline(world.second_channel.id).is_empty(),
        "its content is dropped"
    );
    assert!(out.changes.contains(&Change::TimelineReset {
        conversation: world.second_channel.id
    }));
    assert!(driver.core.conversation(newcomer.id).is_some());
    assert_eq!(
        texts(&driver, world.channel.id),
        ["msg 5", "msg 6"],
        "other content stays"
    );
    assert!(history_fetches(&driver).is_empty());
    let cached = driver.last_cache().unwrap();
    assert!(
        cached.workspaces[0]
            .timelines
            .iter()
            .all(|t| t.conversation != world.second_channel.id)
    );
    assert_eq!(cached.workspaces[0].members.len(), 3);
}

#[test]
fn losing_the_selected_conversation_moves_the_selection() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5], Some(&[8]));
    let (mut driver, _) = restart(&world, Some(cache));
    assert_eq!(driver.core.selected_conversation(), Some(world.channel.id));

    let mut bootstrap = world.bootstrap();
    bootstrap.workspaces[0].conversations = vec![world.second_channel.clone()];
    let out = driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    assert_eq!(
        driver.core.selected_conversation(),
        Some(world.second_channel.id)
    );
    assert!(out.changes.contains(&Change::SelectionChanged));
    assert!(
        history_fetches(&driver).is_empty(),
        "the new selection is cached too"
    );
}

#[test]
fn an_uncached_selected_conversation_still_fetches_its_newest_page() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5], None);
    let (mut driver, _) = restart(&world, Some(cache));
    driver.send(Input::SelectConversation(world.second_channel.id));
    assert_eq!(history_fetches(&driver), [world.second_channel.id]);
}

#[test]
fn snapshot_required_discards_the_cache_clears_content_and_starts_fresh() {
    for reason in [SnapshotReason::CursorExpired, SnapshotReason::CursorAhead] {
        let world = world_with_head(20);
        let cache = warm_cache(&world, &[5, 6, 7], None);
        let (mut driver, _) = restart(&world, Some(cache));
        let caches_before = driver.caches().len();

        let out = driver.send(Input::Frame(ServerFrame::SnapshotRequired {
            workspace_id: world.workspace.id,
            reason,
        }));
        assert!(out.effects.contains(&Effect::DiscardCache), "{reason:?}");
        assert!(out.changes.contains(&Change::TimelineReset {
            conversation: world.channel.id
        }));
        assert!(
            driver.core.timeline(world.channel.id).is_empty(),
            "content is cleared"
        );

        // A bootstrap was already in flight from the start; its answer is treated as a first run.
        let mut bootstrap = world.bootstrap();
        bootstrap.workspaces[0].head = seq(80);
        let out = driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
        assert!(out.effects.contains(&Effect::Subscribe {
            workspace: world.workspace.id,
            after: seq(80)
        }));
        assert!(
            out.effects.iter().any(|e| matches!(e, Effect::FetchHistory { conversation, before: None, .. } if *conversation == world.channel.id)),
            "the newest page is fetched like on a first run"
        );
        assert_eq!(
            driver.core.history_status(world.channel.id),
            &HistoryStatus::Loading
        );

        // Nothing was saved while the old cursor was known to be bad, and the next cache starts
        // from the fresh snapshot with no stale timelines.
        let fresh: Vec<_> = driver.caches().into_iter().skip(caches_before).collect();
        assert_eq!(fresh.len(), 1, "{reason:?}");
        assert_eq!(fresh[0].workspaces[0].cursor, seq(80));
        assert!(fresh[0].workspaces[0].timelines.is_empty());
    }
}

#[test]
fn nothing_is_cached_between_a_snapshot_request_and_its_answer() {
    let world = world_with_head(20);
    let mut driver = Driver::ready(&world);
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages: vec![],
            has_more: false,
        }),
    });
    driver.send(Input::Frame(ServerFrame::SnapshotRequired {
        workspace_id: world.workspace.id,
        reason: SnapshotReason::CursorExpired,
    }));
    let before = driver.caches().len();
    driver.send(world.event(world.message(world.channel.id, &world.other, 21, "x"), None));
    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: world.workspace.id,
        seq: seq(21),
    }));
    assert_eq!(
        driver.caches().len(),
        before,
        "a stale cursor must not be saved"
    );
}

#[test]
fn a_cursor_ahead_of_the_servers_log_is_treated_as_a_first_run() {
    let world = world_with_head(60);
    let cache = warm_cache(&world, &[5, 6], None);
    // The server was restored from a backup: its log now ends at 30, behind the cached cursor 60.
    let mut bootstrap = World::new();
    bootstrap.workspace = world.workspace.clone();
    bootstrap.channel = world.channel.clone();
    bootstrap.second_channel = world.second_channel.clone();
    bootstrap.me = world.me.clone();
    bootstrap.other = world.other.clone();
    bootstrap.head = seq(30);

    let (mut driver, _) = restart(&world, Some(cache));
    let out = driver.send(Input::BootstrapLoaded(Box::new(bootstrap.bootstrap())));
    assert!(
        driver.core.timeline(world.channel.id).is_empty(),
        "stale content is dropped"
    );
    assert!(out.effects.contains(&Effect::Subscribe {
        workspace: world.workspace.id,
        after: seq(30)
    }));
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::FetchHistory { .. }))
    );
    assert_eq!(driver.last_cache().unwrap().workspaces[0].cursor, seq(30));
}

#[test]
fn a_cache_of_another_person_is_never_shown_or_kept() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5], None);

    // Not restored: the cache is ignored, and another person signing in discards it.
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: false,
        cache: Some(cache.clone()),
        ..Default::default()
    }));
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut { .. }
    ));
    assert!(driver.core.conversations().is_empty());
    let out = driver.send(Input::Authenticated {
        user: user("intruder"),
    });
    assert!(out.effects.contains(&Effect::DiscardCache));

    // The same person signing in again is not told to discard it.
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: false,
        cache: Some(cache),
        ..Default::default()
    }));
    let out = driver.send(Input::Authenticated {
        user: world.me.clone(),
    });
    assert!(!out.effects.contains(&Effect::DiscardCache));
}

#[test]
fn a_bootstrap_for_a_different_person_replaces_the_cached_content() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5], None);
    let (mut driver, _) = restart(&world, Some(cache));
    let mut bootstrap = world.bootstrap();
    bootstrap.user = user("stranger");
    driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    assert!(driver.core.timeline(world.channel.id).is_empty());
    assert!(driver.last_cache().unwrap().user.username.as_str() == "stranger");
}

#[test]
fn an_empty_cache_is_ignored() {
    let world = world_with_head(20);
    let mut cache = warm_cache(&world, &[5], None);
    cache.workspaces.clear();
    let (driver, out) = restart(&world, Some(cache));
    assert_eq!(driver.core.phase(), &SessionPhase::Loading);
    assert_eq!(out.effects, vec![Effect::LoadBootstrap]);
}

#[test]
fn signing_out_discards_nothing_itself_because_the_runtime_wipes_all_local_data() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5], None);
    let (mut driver, _) = restart(&world, Some(cache));
    let out = driver.send(Input::SignOut);
    assert!(out.effects.contains(&Effect::SignOut));
    assert!(driver.core.timeline(world.channel.id).is_empty());
    // And nothing is cached afterwards.
    let before = driver.caches().len();
    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: world.workspace.id,
        seq: seq(30),
    }));
    assert_eq!(driver.caches().len(), before);
}

// ----- Offline and failure -----

#[test]
fn starting_offline_keeps_the_cached_content_and_revalidates_when_the_connection_returns() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5, 6], None);
    let (mut driver, _) = restart(&world, Some(cache));

    driver.send(Input::BootstrapFailed(RequestError::Unknown(
        "offline".into(),
    )));
    assert_eq!(
        driver.core.phase(),
        &SessionPhase::Ready,
        "no error screen over cached content"
    );
    assert_eq!(texts(&driver, world.channel.id).len(), 2);
    driver.send(Input::Socket(SocketEvent::Closed {
        workspace: world.workspace.id,
        attempt: 1,
    }));
    assert_eq!(
        driver.core.connection(),
        ConnectionState::Reconnecting { attempt: 1 }
    );

    // The realtime connection comes back: the bootstrap is asked for again.
    let out = driver.send(Input::Socket(SocketEvent::Opened {
        workspace: world.workspace.id,
    }));
    assert!(out.effects.contains(&Effect::LoadBootstrap));
    // And it is not duplicated while in flight.
    let out = driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: world.workspace.id,
        seq: seq(20),
    }));
    assert!(!out.effects.contains(&Effect::LoadBootstrap));
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    assert_eq!(texts(&driver, world.channel.id).len(), 2);
    assert!(history_fetches(&driver).is_empty());

    // Once revalidated, a failed refresh is the ordinary kind.
    driver.send(Input::BootstrapFailed(RequestError::Unknown(
        "again".into(),
    )));
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::Unreachable { .. }
    ));
}

#[test]
fn an_expired_session_over_cached_content_signs_out() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5], None);
    let (mut driver, _) = restart(&world, Some(cache));
    driver.send(Input::BootstrapFailed(RequestError::Rejected(
        lazy_protocol::ApiError::new(ErrorCode::Unauthenticated, "expired"),
    )));
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut {
            last_error: Some(_)
        }
    ));
    assert!(driver.effects.contains(&Effect::ForgetSession));
}

// ----- Cache together with journal recovery and the send queue -----

fn journal_entry(world: &World, text: &str, at: i64) -> JournalEntry {
    JournalEntry {
        operation_id: OperationId::generate(),
        user_id: world.me.id,
        conversation_id: world.channel.id,
        document: MessageDocument::from_plain_text(text).unwrap(),
        submitted_at: Timestamp::from(
            time::OffsetDateTime::from_unix_timestamp(1_800_000_000 + at).unwrap(),
        ),
        kind: JournalKind::Send,
        thread_root: None,
        also_in_channel: false,
        attachments: Vec::new(),
    }
}

#[test]
fn cached_content_and_journal_recovery_work_together() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5, 6], None);
    let first = journal_entry(&world, "recovered first", 0);
    let second = journal_entry(&world, "recovered second", 10);
    let (mut driver, out) =
        restart_with_journal(&world, Some(cache), vec![second.clone(), first.clone()]);

    // Both the cached messages and the unconfirmed ones are on screen right away.
    let rows = driver.timeline(world.channel.id);
    assert_eq!(rows.len(), 4);
    assert!(rows[..2].iter().all(|r| r.is_confirmed()));
    assert!(
        rows[2..]
            .iter()
            .all(|r| matches!(r.delivery, Delivery::UnknownOutcome))
    );
    // Recovery asks the server before resending anything.
    let queried: Vec<_> = out
        .effects
        .iter()
        .filter_map(|e| match e {
            Effect::QueryOperationStatus { operation } => Some(*operation),
            _ => None,
        })
        .collect();
    assert_eq!(queried.len(), 2);
    let sent = |d: &Driver| -> Vec<OperationId> {
        d.effects_matching(|e| match e {
            Effect::SendMessage { operation, .. } => Some(*operation),
            _ => None,
        })
    };
    assert!(sent(&driver).is_empty());

    // A message typed right away queues behind the recovered ones.
    let out = driver.send(Input::SendMessage {
        conversation: world.channel.id,
        text: "typed now".into(),
    });
    let Some(Effect::PersistSubmission(typed)) = out.effects.first().cloned() else {
        panic!()
    };
    driver.send(Input::SubmissionPersisted {
        operation: typed.operation_id,
    });
    for entry in [&second, &first] {
        driver.send(Input::OperationStatusResult {
            operation: entry.operation_id,
            result: Ok(OperationStatus::NotFound),
        });
    }
    assert_eq!(
        sent(&driver),
        vec![first.operation_id],
        "journal order, then new messages"
    );

    // Once confirmed, a recovered message becomes part of the next cache.
    let message = world.message(world.channel.id, &world.me, 21, "recovered first");
    driver.send(Input::SendResult {
        operation: first.operation_id,
        result: Ok(SendMessageResponse { message }),
    });
    assert_eq!(sent(&driver), vec![first.operation_id, second.operation_id]);
    let cached = &driver.last_cache().unwrap().workspaces[0].timelines[0].messages;
    assert!(
        cached
            .iter()
            .any(|m| m.document.plain_text() == "recovered first")
    );
    assert!(
        driver
            .timeline(world.channel.id)
            .iter()
            .any(|r| r.key == ItemKey::Operation(typed.operation_id))
    );
}

#[test]
fn cached_start_keeps_the_send_queue_working_for_new_messages() {
    let world = world_with_head(20);
    let cache = warm_cache(&world, &[5], None);
    let (mut driver, _) = restart(&world, Some(cache));
    let mut operations = Vec::new();
    for text in ["a", "b", "c"] {
        let out = driver.send(Input::SendMessage {
            conversation: world.channel.id,
            text: text.into(),
        });
        let Some(Effect::PersistSubmission(entry)) = out.effects.first().cloned() else {
            panic!()
        };
        operations.push(entry.operation_id);
    }
    for operation in &operations {
        driver.send(Input::SubmissionPersisted {
            operation: *operation,
        });
    }
    let sent = driver.effects_matching(|e| match e {
        Effect::SendMessage { operation, .. } => Some(*operation),
        _ => None,
    });
    assert_eq!(sent, vec![operations[0]], "one at a time, in order");
}

#[test]
fn members_who_join_are_remembered() {
    let world = world_with_head(20);
    let mut driver = Driver::ready(&world);
    let newcomer = user("newbie");
    driver.send(Input::Frame(ServerFrame::Event(
        lazy_protocol::EventEnvelope {
            workspace_id: world.workspace.id,
            seq: seq(21),
            occurred_at: Timestamp::now(),
            event: lazy_protocol::Event::MemberJoined {
                user: newcomer.clone(),
            },
        },
    )));
    let members = &driver.last_cache().unwrap().workspaces[0].members;
    assert!(members.iter().any(|m| m.id == newcomer.id));
}

// ----- Cost -----

#[test]
fn a_full_cache_does_not_cost_a_frame_per_live_event() {
    let world = world_with_head(100_000);
    let mut bootstrap = world.bootstrap();
    let conversations: Vec<Conversation> = (0..MAX_CACHED_CONVERSATIONS)
        .map(|i| channel(world.workspace.id, &format!("channel-{i}")))
        .collect();
    bootstrap.workspaces[0].conversations = conversations.clone();
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    for (index, conversation) in conversations.iter().enumerate() {
        driver.send(Input::SelectConversation(conversation.id));
        let generation = driver.effects_matching(|e| match e {
            Effect::FetchHistory {
                conversation: c,
                generation,
                ..
            } if *c == conversation.id => Some(*generation),
            _ => None,
        });
        let messages = (0..MAX_CACHED_MESSAGES as i64)
            .map(|at| {
                world.message(
                    conversation.id,
                    &world.other,
                    1 + index as i64 * 1000 + at,
                    "a typical chat message of moderate length, with a few words in it",
                )
            })
            .collect();
        driver.send(Input::HistoryLoaded {
            conversation: conversation.id,
            generation: generation[0],
            result: Ok(HistoryPage {
                messages,
                has_more: true,
            }),
        });
    }
    assert_eq!(
        driver.last_cache().unwrap().message_count(),
        MAX_CACHED_CONVERSATIONS * MAX_CACHED_MESSAGES
    );

    // Time the handling of a stream of live events. Building the cache is proportional to its
    // size, so on a big cache the writes are batched; skipping one only makes the saved cache a
    // little older, which replay covers.
    let target = conversations[0].id;
    let writes_before = driver.caches().len();
    let started = std::time::Instant::now();
    let events = 50;
    for i in 0..events {
        driver.send(world.event(
            world.message(target, &world.other, 100_001 + i, "live"),
            None,
        ));
    }
    let per_event = started.elapsed() / events as u32;
    eprintln!("handling one event with a full cache: {per_event:?}");
    // A frame is 16 ms and this runs on the UI thread. The bound is loose enough for an
    // unoptimized build on a busy machine and tight enough to catch a regression.
    assert!(
        per_event < std::time::Duration::from_millis(5),
        "{per_event:?}"
    );

    let writes = driver.caches().len() - writes_before;
    assert!(writes >= 1, "the cache is still being saved");
    assert!(writes <= 5, "{writes} writes for {events} events: batched");
    // Whatever was last saved is still consistent: its cursor is not ahead of the events seen.
    let saved = driver.last_cache().unwrap().workspaces[0].cursor;
    assert!(saved.get() <= 100_000 + events);
}

#[test]
fn structural_changes_are_saved_at_once_even_on_a_big_cache() {
    let world = world_with_head(100);
    let mut driver = Driver::ready(&world);
    let messages = (0..MAX_CACHED_MESSAGES as i64)
        .map(|at| world.message(world.channel.id, &world.other, at + 1, "m"))
        .collect();
    driver.send(Input::HistoryLoaded {
        conversation: world.channel.id,
        generation: 1,
        result: Ok(HistoryPage {
            messages,
            has_more: true,
        }),
    });
    driver.send(Input::SelectConversation(world.second_channel.id));
    let generation = driver.effects_matching(|e| match e {
        Effect::FetchHistory {
            conversation,
            generation,
            ..
        } if *conversation == world.second_channel.id => Some(*generation),
        _ => None,
    });
    let before = driver.caches().len();
    let page = (0..MAX_CACHED_MESSAGES as i64)
        .map(|at| world.message(world.second_channel.id, &world.other, 1000 + at, "m"))
        .collect();
    driver.send(Input::HistoryLoaded {
        conversation: world.second_channel.id,
        generation: generation[0],
        result: Ok(HistoryPage {
            messages: page,
            has_more: false,
        }),
    });
    assert_eq!(
        driver.caches().len(),
        before + 1,
        "a loaded page is saved immediately"
    );
}

#[test]
fn a_message_acknowledged_beyond_the_cursor_does_not_hide_what_replay_delivers_below_it() {
    // Found by the randomized run. The cache holds only our own message at position 12, while the
    // cursor is still 10. Replay then delivers position 11, which sorts below it. Both must be
    // in every cache written afterwards, or restarting would silently lose message 11.
    for has_more in [false, true] {
        let world = world_with_head(10);
        let mut cache = warm_cache(&world, &[], None);
        let own = world.message(world.channel.id, &world.me, 12, "mine");
        cache.workspaces[0].timelines[0].messages = vec![own];
        cache.workspaces[0].timelines[0].has_more = has_more;
        assert_eq!(cache.workspaces[0].cursor, seq(10));

        let (mut driver, _) = restart(&world, Some(cache));
        driver.send(world.event(
            world.message(world.channel.id, &world.other, 11, "from replay"),
            None,
        ));
        let cached = &driver.last_cache().unwrap().workspaces[0];
        assert_eq!(cached.cursor, seq(11));
        let seqs: Vec<_> = cached.timelines[0]
            .messages
            .iter()
            .map(|m| m.seq.get())
            .collect();
        assert_eq!(seqs, [11, 12], "has_more = {has_more}");

        if has_more {
            // Older history is still there to fetch, starting just after the cursor's range.
            let out = driver.send(Input::LoadOlder {
                conversation: world.channel.id,
            });
            assert!(matches!(
                &out.effects[..],
                [Effect::FetchHistory { before: Some(before), .. }] if *before == seq(11)
            ));
        }
    }
}

#[test]
fn older_history_pages_from_the_cached_floor() {
    let world = world_with_head(20);
    let mut cache = warm_cache(&world, &[10, 11, 12], None);
    cache.workspaces[0].timelines[0].has_more = true;
    let (mut driver, _) = restart(&world, Some(cache));
    assert_eq!(
        driver.core.history_status(world.channel.id),
        &HistoryStatus::Ready { has_more: true }
    );
    let out = driver.send(Input::LoadOlder {
        conversation: world.channel.id,
    });
    assert!(matches!(
        &out.effects[..],
        [Effect::FetchHistory { before: Some(before), .. }] if *before == seq(10)
    ));
}

// ----- Read state in the cache -----

fn unread(driver: &Driver, conversation: ConversationId) -> u32 {
    driver
        .core
        .user_state(conversation)
        .map_or(0, |state| state.unread_count)
}

/// A world where general has two unread messages (6 and 7 after the read position 5) as of 20.
fn world_with_unread_as_of_20() -> World {
    let mut world = world_with_head(20);
    world.user_states = vec![read_state(world.channel.id, 5, 2)];
    world
}

#[test]
fn a_restart_shows_the_sidebar_with_its_unread_counts_and_favorites_at_once() {
    let mut world = world_with_unread_as_of_20();
    let mut starred = read_state(world.second_channel.id, 3, 4);
    starred.favorite = true;
    world.user_states.push(starred);
    let cache = warm_cache(&world, &[6, 7], None);
    let saved = &cache.workspaces[0].user_state;
    assert_eq!(saved.len(), 2, "every conversation's state is saved");
    assert!(saved.iter().all(|s| s.as_of == seq(20)));

    let (driver, _) = restart(&world, Some(cache));
    assert_eq!(unread(&driver, world.channel.id), 2);
    assert_eq!(driver.core.total_unread(), 6);
    assert_eq!(driver.core.favorites(), vec![world.second_channel.id]);
    // The divider position is known from the saved read position as soon as the rows are.
    assert!(driver.core.unread_marker(world.channel.id).is_some());
}

#[test]
fn replay_after_a_cached_start_counts_each_message_exactly_once() {
    let world = world_with_unread_as_of_20();
    let cache = warm_cache(&world, &[6, 7], None);
    let (mut driver, _) = restart(&world, Some(cache));

    // By the time the client reconnects, ten more messages have arrived and the server's own
    // count has moved on to 12. Its baseline must not be added to the replay.
    let mut bootstrap = world.bootstrap();
    bootstrap.workspaces[0].head = seq(30);
    bootstrap.workspaces[0].user_state = vec![read_state(world.channel.id, 5, 12)];
    driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    assert_eq!(
        unread(&driver, world.channel.id),
        2,
        "the cached state is kept"
    );

    for at in 21..=30 {
        driver.send(world.event(
            world.message(
                world.channel.id,
                &world.other,
                at,
                &format!("replayed {at}"),
            ),
            None,
        ));
    }
    assert_eq!(
        unread(&driver, world.channel.id),
        12,
        "equal to the server's count, not double"
    );

    // A replay of the same positions again (a reconnect) changes nothing.
    for at in 21..=30 {
        driver.send(world.event(
            world.message(world.channel.id, &world.other, at, "dup"),
            None,
        ));
    }
    assert_eq!(unread(&driver, world.channel.id), 12);
}

#[test]
fn a_conversation_the_cache_did_not_know_takes_the_bootstraps_baseline_and_ignores_older_replay() {
    let world = world_with_unread_as_of_20();
    let cache = warm_cache(&world, &[6, 7], None);
    let (mut driver, _) = restart(&world, Some(cache));

    // Meanwhile the person joined #fresh, which has 4 unread as of position 30.
    let fresh = channel(world.workspace.id, "fresh");
    let mut bootstrap = world.bootstrap();
    bootstrap.workspaces[0].head = seq(30);
    bootstrap.workspaces[0].conversations.push(fresh.clone());
    bootstrap.workspaces[0].user_state = vec![
        read_state(world.channel.id, 5, 2),
        read_state(fresh.id, 0, 4),
    ];
    driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    assert_eq!(unread(&driver, fresh.id), 4);

    // Replay now delivers #fresh's older messages, which the baseline already counts.
    driver.send(world.event(world.message(fresh.id, &world.other, 25, "older"), None));
    driver.send(world.event(
        world.message(fresh.id, &world.other, 30, "at the baseline"),
        None,
    ));
    assert_eq!(unread(&driver, fresh.id), 4);
    driver.send(world.event(world.message(fresh.id, &world.other, 31, "new"), None));
    assert_eq!(unread(&driver, fresh.id), 5);
}

#[test]
fn a_cache_from_before_read_state_existed_takes_the_baseline_from_the_bootstrap() {
    let world = world_with_unread_as_of_20();
    let mut cache = warm_cache(&world, &[6, 7], None);
    cache.workspaces[0].user_state.clear();

    let (mut driver, _) = restart(&world, Some(cache));
    assert!(
        driver.core.user_state(world.channel.id).is_none(),
        "nothing known yet"
    );
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    assert_eq!(unread(&driver, world.channel.id), 2);
    // And it is current as of the bootstrap's head, so older replay is not counted again.
    driver.send(world.event(
        world.message(world.channel.id, &world.other, 20, "old"),
        None,
    ));
    assert_eq!(unread(&driver, world.channel.id), 2);
}

#[test]
fn read_state_is_saved_and_dropped_with_its_conversation() {
    let world = world_with_unread_as_of_20();
    let cache = warm_cache(&world, &[6, 7], Some(&[8]));
    let (mut driver, _) = restart(&world, Some(cache));
    let mut bootstrap = world.bootstrap();
    bootstrap.workspaces[0].conversations = vec![world.channel.clone()];
    driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    assert!(driver.core.user_state(world.second_channel.id).is_none());
    let saved = &driver.last_cache().unwrap().workspaces[0].user_state;
    assert!(
        saved
            .iter()
            .all(|s| s.state.conversation_id != world.second_channel.id)
    );
}

#[test]
fn direct_messages_survive_a_restart_with_their_titles_and_order() {
    let mut world = world_with_head(20);
    let kai = user("kai");
    let dm_zoe = people(world.workspace.id, &[&world.me, &world.other]);
    let dm_kai = people(world.workspace.id, &[&world.me, &kai]);
    world.extra = vec![dm_zoe.clone(), dm_kai.clone()];
    world.extra_users = vec![kai.clone()];
    let mut driver = Driver::ready(&world);
    driver.send(world.event(world.message(dm_kai.id, &kai, 21, "hello"), None));
    driver.send(world.event(world.message(dm_zoe.id, &world.other, 22, "hi"), None));
    driver.send(Input::SelectConversation(dm_zoe.id));
    driver.load_history(dm_zoe.id, vec![], false);
    let cache = driver.last_cache().unwrap().clone();

    let (restarted, _) = restart(&world, Some(cache));
    assert_eq!(restarted.core.conversation_title(dm_zoe.id), "ZOE");
    assert_eq!(restarted.core.conversation_title(dm_kai.id), "KAI");
    assert_eq!(
        restarted.core.members().len(),
        3,
        "the people are saved too"
    );
    // Zoe wrote last, so her conversation comes first even before anything is refetched.
    assert_eq!(
        restarted.core.direct_and_group(),
        vec![dm_zoe.id, dm_kai.id]
    );
}
