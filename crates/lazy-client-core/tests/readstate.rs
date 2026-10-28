//! Unread counting, the mark-read policy, and the "New messages" divider.

mod common;

use common::{Driver, World, read_state, seq};
use lazy_client_core::{Change, Effect, Input, ItemKey, RequestError};
use lazy_domain::{ConversationId, EventSeq};
use lazy_protocol::{ErrorCode, Event};

/// General has two unread messages after the read position 5 (messages 6 and 7), loaded.
fn world_with_unread() -> World {
    let mut world = World::new();
    world.user_states = vec![
        read_state(world.channel.id, 5, 2),
        read_state(world.second_channel.id, 0, 0),
    ];
    world
}

fn loaded_general(world: &World) -> Driver {
    let mut driver = Driver::ready(world);
    let general = world.channel.id;
    let page = (4..=7)
        .map(|at| world.message(general, &world.other, at, &format!("m{at}")))
        .collect();
    driver.load_history(general, page, false);
    driver
}

fn state(driver: &Driver, conversation: ConversationId) -> (i64, u32) {
    let state = driver.core.user_state(conversation).expect("a read state");
    (state.last_read.get(), state.unread_count)
}

// ----- Counting -----

#[test]
fn the_baseline_comes_from_the_bootstrap() {
    let world = world_with_unread();
    let driver = Driver::ready(&world);
    assert_eq!(state(&driver, world.channel.id), (5, 2));
    assert_eq!(state(&driver, world.second_channel.id), (0, 0));
    assert_eq!(driver.core.total_unread(), 2);
}

#[test]
fn a_message_from_someone_else_adds_one_and_a_repeat_adds_nothing() {
    let world = world_with_unread();
    let mut driver = loaded_general(&world);
    let message = world.message(world.channel.id, &world.other, 11, "hi");
    driver.send(world.event(message.clone(), None));
    assert_eq!(state(&driver, world.channel.id), (5, 3));
    // The same event again, and an event at an old position, change nothing.
    driver.send(world.event(message, None));
    driver.send(world.event(
        world.message(world.channel.id, &world.other, 9, "old"),
        None,
    ));
    assert_eq!(state(&driver, world.channel.id), (5, 3));
}

#[test]
fn a_message_at_or_before_the_read_position_is_not_unread() {
    // Another device has read up to 15 already; events 11 and 15 are news to this client but not
    // to the person.
    let mut world = World::new();
    world.user_states = vec![read_state(world.second_channel.id, 15, 0)];
    let mut driver = Driver::ready(&world);
    let random = world.second_channel.id;
    driver.send(world.event(world.message(random, &world.other, 11, "a"), None));
    driver.send(world.event(world.message(random, &world.other, 15, "b"), None));
    assert_eq!(state(&driver, random), (15, 0));
    driver.send(world.event(world.message(random, &world.other, 16, "c"), None));
    assert_eq!(state(&driver, random), (15, 1));
}

#[test]
fn my_own_message_moves_the_read_position_and_clears_the_count() {
    let world = world_with_unread();
    let mut driver = loaded_general(&world);
    driver.send(world.event(world.message(world.channel.id, &world.me, 12, "mine"), None));
    assert_eq!(state(&driver, world.channel.id), (12, 0));
    driver.send(world.event(
        world.message(world.channel.id, &world.other, 13, "reply"),
        None,
    ));
    assert_eq!(state(&driver, world.channel.id), (12, 1));
}

#[test]
fn user_state_changed_replaces_the_state() {
    let world = world_with_unread();
    let mut driver = loaded_general(&world);
    let mut replacement = read_state(world.channel.id, 9, 7);
    replacement.favorite = true;
    driver.send(world.event_at(
        11,
        Event::UserStateChanged {
            state: replacement.clone(),
        },
    ));
    assert_eq!(driver.core.user_state(world.channel.id), Some(&replacement));
    // Counting resumes from the replaced state, for events after it only.
    driver.send(world.event(world.message(world.channel.id, &world.other, 12, "x"), None));
    assert_eq!(state(&driver, world.channel.id), (9, 8));
}

#[test]
fn events_for_conversations_the_client_does_not_know_are_ignored() {
    let world = world_with_unread();
    let mut driver = Driver::ready(&world);
    let stranger = ConversationId::generate();
    let message = world.message(stranger, &world.other, 11, "who?");
    driver.send(world.event(message, None));
    driver.send(world.event_at(
        12,
        Event::UserStateChanged {
            state: read_state(stranger, 3, 3),
        },
    ));
    driver.send(world.event_at(
        13,
        Event::MemberRemoved {
            conversation_id: stranger,
            user_id: world.me.id,
        },
    ));
    assert!(driver.core.user_state(stranger).is_none());
    assert!(
        driver.core.timeline(stranger).is_empty(),
        "no ghost timeline"
    );
    assert_eq!(driver.core.conversations().len(), 2);
    assert!(driver.core.notice().is_none());
    // The position still moved on, so these events are not replayed forever.
    assert_eq!(driver.last_cache().unwrap().workspaces[0].cursor, seq(13));
}

// ----- Marking read -----

#[test]
fn a_conversation_is_marked_read_only_when_it_is_looked_at() {
    let world = world_with_unread();
    let general = world.channel.id;

    // Everything in place except that the timeline is not at the bottom.
    let mut driver = loaded_general(&world);
    assert!(driver.mark_reads().is_empty(), "nothing is sent unprompted");

    // At the bottom but the window is in the background: not yet.
    driver.send(Input::WindowActive(false));
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    assert!(driver.mark_reads().is_empty());

    // The window comes to the front: now.
    driver.send(Input::WindowActive(true));
    assert_eq!(driver.mark_reads(), vec![(general, 7)]);
}

#[test]
fn nothing_is_marked_without_unread_messages_a_loaded_timeline_or_the_selection() {
    let world = world_with_unread();
    let (general, random) = (world.channel.id, world.second_channel.id);

    // The page has not arrived: the person cannot be looking at the newest message yet.
    let mut driver = Driver::ready(&world);
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    assert!(driver.mark_reads().is_empty());

    // A conversation that is not open is never marked, even if its view reports the bottom.
    let mut driver = loaded_general(&world);
    driver.send(Input::TimelineAtBottom {
        conversation: random,
        at_bottom: true,
    });
    assert!(driver.mark_reads().is_empty());

    // Nothing unread, nothing to send.
    let mut quiet = World::new();
    quiet.user_states = vec![read_state(quiet.channel.id, 7, 0)];
    let mut driver = Driver::ready(&quiet);
    driver.load_history(
        quiet.channel.id,
        vec![quiet.message(quiet.channel.id, &quiet.other, 7, "old")],
        false,
    );
    driver.send(Input::TimelineAtBottom {
        conversation: quiet.channel.id,
        at_bottom: true,
    });
    assert!(driver.mark_reads().is_empty());
}

#[test]
fn leaving_the_bottom_stops_and_returning_resumes_marking() {
    let world = world_with_unread();
    let general = world.channel.id;
    let mut driver = loaded_general(&world);
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: false,
    });
    // The first report already sent a request; scrolling away adds nothing, and a new message
    // while scrolled up is not marked.
    let sent = driver.mark_reads().len();
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(7),
        result: Ok(read_state(general, 7, 0)),
    });
    driver.send(world.event(world.message(general, &world.other, 11, "later"), None));
    assert_eq!(driver.mark_reads().len(), sent);
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    assert_eq!(driver.mark_reads().last(), Some(&(general, 11)));
}

#[test]
fn a_burst_is_covered_by_one_request_in_flight_and_one_follow_up() {
    let world = world_with_unread();
    let general = world.channel.id;
    let mut driver = loaded_general(&world);
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    assert_eq!(driver.mark_reads(), vec![(general, 7)]);

    // Three messages arrive while the request is out: no more requests yet.
    for at in 11..=13 {
        driver.send(world.event(world.message(general, &world.other, at, "burst"), None));
    }
    assert_eq!(driver.mark_reads().len(), 1);
    assert_eq!(state(&driver, general), (5, 5));

    // The answer comes back: the new position applies, the three late messages stay unread, and
    // one follow-up covers them.
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(7),
        result: Ok(read_state(general, 7, 0)),
    });
    assert_eq!(driver.mark_reads(), vec![(general, 7), (general, 13)]);
    assert_eq!(state(&driver, general), (7, 3));

    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(13),
        result: Ok(read_state(general, 13, 0)),
    });
    assert_eq!(state(&driver, general), (13, 0));
    assert_eq!(driver.mark_reads().len(), 2, "nothing further to mark");
}

#[test]
fn requests_only_ever_move_forward() {
    let world = world_with_unread();
    let general = world.channel.id;
    let mut driver = loaded_general(&world);
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    for at in 11..=20 {
        driver.send(world.event(world.message(general, &world.other, at, "m"), None));
        let up_to = driver.mark_reads().last().map_or(0, |(_, up_to)| *up_to);
        driver.send(Input::MarkReadResult {
            conversation: general,
            up_to: EventSeq::new(up_to).unwrap(),
            result: Ok(read_state(general, up_to, 0)),
        });
    }
    let positions: Vec<i64> = driver.mark_reads().iter().map(|(_, p)| *p).collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{positions:?}");
    assert!(
        positions.iter().all(|p| *p > 5),
        "never at or below the read position"
    );
}

#[test]
fn a_failed_request_is_not_repeated_for_the_same_position() {
    let world = world_with_unread();
    let general = world.channel.id;
    let mut driver = loaded_general(&world);
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(7),
        result: Err(RequestError::Unknown("offline".into())),
    });
    assert_eq!(driver.mark_reads().len(), 1, "no retry loop");
    assert_eq!(state(&driver, general), (5, 2), "still unread");

    // A newer message is a new position worth trying.
    driver.send(world.event(world.message(general, &world.other, 11, "again"), None));
    assert_eq!(driver.mark_reads().last(), Some(&(general, 11)));
}

#[test]
fn an_expired_session_during_mark_read_signs_out() {
    let world = world_with_unread();
    let general = world.channel.id;
    let mut driver = loaded_general(&world);
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(7),
        result: Err(RequestError::Rejected(lazy_protocol::ApiError::new(
            ErrorCode::Unauthenticated,
            "expired",
        ))),
    });
    assert!(driver.effects.contains(&Effect::ForgetSession));
}

#[test]
fn a_message_i_wrote_is_never_a_reason_to_mark_anything() {
    let mut world = World::new();
    world.user_states = vec![read_state(world.channel.id, 7, 0)];
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(
        general,
        vec![world.message(general, &world.other, 7, "old")],
        false,
    );
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    driver.send(world.event(world.message(general, &world.me, 12, "mine"), None));
    assert!(driver.mark_reads().is_empty());
    assert_eq!(state(&driver, general), (12, 0));
}

#[test]
fn the_selection_changes_what_is_marked() {
    let mut world = World::new();
    world.user_states = vec![
        read_state(world.channel.id, 7, 0),
        read_state(world.second_channel.id, 3, 1),
    ];
    let (general, random) = (world.channel.id, world.second_channel.id);
    let mut driver = Driver::ready(&world);
    driver.load_history(general, vec![], false);
    driver.send(Input::SelectConversation(random));
    driver.load_history(
        random,
        vec![world.message(random, &world.other, 4, "unread")],
        false,
    );
    // Reports about the old conversation's view are forgotten when it is left.
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    assert!(driver.mark_reads().is_empty());
    driver.send(Input::TimelineAtBottom {
        conversation: random,
        at_bottom: true,
    });
    assert_eq!(driver.mark_reads(), vec![(random, 4)]);
}

// ----- The "New messages" divider -----

#[test]
fn the_divider_goes_before_the_first_unread_message_and_stays_once_read() {
    let world = world_with_unread();
    let general = world.channel.id;
    let mut driver = loaded_general(&world);
    let first_unread = driver
        .timeline(general)
        .iter()
        .find(|row| row.seq() == Some(seq(6)))
        .unwrap()
        .key;
    assert_eq!(driver.core.unread_marker(general), Some(first_unread));

    // Reading everything does not move or remove the divider.
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(7),
        result: Ok(read_state(general, 7, 0)),
    });
    assert_eq!(state(&driver, general), (7, 0));
    assert_eq!(driver.core.unread_marker(general), Some(first_unread));
}

#[test]
fn there_is_no_divider_when_nothing_was_unread_on_opening() {
    let mut world = World::new();
    world.user_states = vec![read_state(world.channel.id, 7, 0)];
    let mut driver = Driver::ready(&world);
    driver.load_history(
        world.channel.id,
        vec![world.message(world.channel.id, &world.other, 7, "old")],
        false,
    );
    assert_eq!(driver.core.unread_marker(world.channel.id), None);
}

#[test]
fn the_divider_waits_for_the_unread_messages_to_load() {
    let world = world_with_unread();
    let driver = Driver::ready(&world);
    assert_eq!(
        driver.core.unread_marker(world.channel.id),
        None,
        "no rows yet"
    );
}

#[test]
fn leaving_a_conversation_forgets_its_divider_and_returning_captures_a_new_one() {
    let mut world = world_with_unread();
    world.user_states[1] = read_state(world.second_channel.id, 0, 0);
    let (general, random) = (world.channel.id, world.second_channel.id);
    let mut driver = loaded_general(&world);
    assert!(driver.core.unread_marker(general).is_some());

    driver.send(Input::SelectConversation(random));
    assert_eq!(driver.core.unread_marker(general), None, "left behind");

    // Coming back: general still has unread messages (it was never read), so a divider appears
    // again at the same place.
    driver.send(Input::SelectConversation(general));
    assert!(driver.core.unread_marker(general).is_some());

    // Once read, leaving and returning shows no divider at all.
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(7),
        result: Ok(read_state(general, 7, 0)),
    });
    driver.send(Input::SelectConversation(random));
    driver.send(Input::SelectConversation(general));
    assert_eq!(driver.core.unread_marker(general), None);
}

#[test]
fn messages_that_arrive_unseen_start_a_divider() {
    let mut world = World::new();
    world.user_states = vec![read_state(world.channel.id, 7, 0)];
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(
        general,
        vec![world.message(general, &world.other, 7, "old")],
        false,
    );
    assert_eq!(driver.core.unread_marker(general), None);

    // The window is in the background when two messages arrive.
    driver.send(Input::WindowActive(false));
    let first = world.message(general, &world.other, 11, "first");
    driver.send(world.event(first.clone(), None));
    driver.send(world.event(world.message(general, &world.other, 12, "second"), None));
    assert_eq!(
        driver.core.unread_marker(general),
        Some(ItemKey::Message(first.id))
    );

    // Coming back marks them read, and the divider stays where it began.
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    driver.send(Input::WindowActive(true));
    assert_eq!(driver.mark_reads(), vec![(general, 12)]);
    assert_eq!(
        driver.core.unread_marker(general),
        Some(ItemKey::Message(first.id))
    );
}

#[test]
fn messages_read_the_moment_they_arrive_make_no_divider() {
    let mut world = World::new();
    world.user_states = vec![read_state(world.channel.id, 7, 0)];
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(
        general,
        vec![world.message(general, &world.other, 7, "old")],
        false,
    );
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    driver.send(world.event(world.message(general, &world.other, 11, "live"), None));
    assert_eq!(driver.core.unread_marker(general), None);
    assert!(
        driver
            .changes
            .iter()
            .any(|c| matches!(c, Change::ConversationStateChanged { conversation } if *conversation == general))
    );
}

#[test]
fn a_message_that_is_in_a_history_page_before_its_event_is_counted_once() {
    // The newest page ran ahead of the events: it already holds the message at position 15, but
    // the event for it has not arrived yet (the client has applied events up to 10).
    let world = world_with_unread();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let ahead = world.message(general, &world.other, 15, "already in the page");
    let page = vec![
        world.message(general, &world.other, 6, "a"),
        world.message(general, &world.other, 7, "b"),
        ahead.clone(),
    ];
    driver.load_history(general, page, false);
    driver.send(Input::TimelineAtBottom {
        conversation: general,
        at_bottom: true,
    });
    assert_eq!(driver.mark_reads(), vec![(general, 15)]);

    // The server only reports reading up to 7, so the message at 15 is still unread. It is not
    // counted by the answer, because its event has not been applied...
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(15),
        result: Ok(read_state(general, 7, 1)),
    });
    assert_eq!(state(&driver, general), (7, 0));

    // ...and is counted exactly once when the event does arrive.
    driver.send(world.event(ahead, None));
    assert_eq!(state(&driver, general), (7, 1));
}
