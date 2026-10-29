//! Reactions, emoji preferences, and shortcode conversion (slice 2C-1).

mod common;

use common::{Driver, World, channel, seq};
use lazy_client_core::{
    Change, EditState, Effect, EmojiPrefs, Input, MAX_RECENT_EMOJI, Output, RequestError,
    StartupState, TimelineItem,
};
use lazy_domain::{ConversationId, MessageDocument, MessageId, ReactionEmoji};
use lazy_protocol::{ApiError, ErrorCode, Event, Message, MessageResponse, SetReactionRequest};

// ----- Helpers -----

const THUMBS: &str = "👍";
const TADA: &str = "🎉";
const HEART: &str = "❤️";

fn emoji(text: &str) -> ReactionEmoji {
    ReactionEmoji::parse(text).unwrap()
}

/// A world with one message by the other person (position 5, no reactions) loaded in `#general`.
fn setup() -> (World, Driver, Message) {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let theirs = world.message(general, &world.other, 5, "their words");
    driver.load_history(general, vec![theirs.clone()], false);
    (world, driver, theirs)
}

fn toggle(conversation: ConversationId, message: &Message, text: &str) -> Input {
    Input::ToggleReaction {
        conversation,
        message: message.id,
        emoji: emoji(text),
    }
}

fn row(driver: &Driver, conversation: ConversationId, message: MessageId) -> &TimelineItem {
    driver
        .core
        .timeline(conversation)
        .iter()
        .find(|item| item.message_id() == Some(message))
        .expect("the row should be in the timeline")
}

/// What a row shows: each emoji with the people who used it, by name.
fn shown(
    world: &World,
    driver: &Driver,
    conversation: ConversationId,
    message: &Message,
) -> Vec<(String, Vec<&'static str>)> {
    row(driver, conversation, message.id)
        .reactions
        .iter()
        .map(|reaction| {
            let people = reaction
                .users
                .iter()
                .map(|user| {
                    if *user == world.me.id {
                        "me"
                    } else if *user == world.other.id {
                        "other"
                    } else {
                        "someone"
                    }
                })
                .collect();
            (reaction.emoji.to_string(), people)
        })
        .collect()
}

fn requests(out: &Output) -> Vec<(MessageId, SetReactionRequest)> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::SetReaction { message, request } => Some((*message, request.clone())),
            _ => None,
        })
        .collect()
}

fn respond(message: &Message) -> Result<MessageResponse, RequestError> {
    Ok(MessageResponse {
        message: message.clone(),
    })
}

fn rejected(code: ErrorCode) -> RequestError {
    RequestError::Rejected(ApiError {
        code,
        message: String::new(),
    })
}

fn lost() -> RequestError {
    RequestError::Unknown("timed out".into())
}

fn result(
    driver: &mut Driver,
    message: &Message,
    text: &str,
    result: Result<MessageResponse, RequestError>,
) -> Output {
    driver.send(Input::ReactionResult {
        message: message.id,
        emoji: emoji(text),
        result,
    })
}

// ----- Reading reactions -----

#[test]
fn reactions_in_a_page_are_shown_and_read_by_me() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let plain = world.message(general, &world.other, 5, "their words");
    let reacted = world.with_reactions(
        &plain,
        7,
        &[
            (THUMBS, &[&world.other]),
            (TADA, &[&world.me, &world.other]),
        ],
    );
    driver.load_history(general, vec![reacted.clone()], false);

    assert_eq!(
        shown(&world, &driver, general, &reacted),
        vec![
            (THUMBS.to_owned(), vec!["other"]),
            (TADA.to_owned(), vec!["me", "other"])
        ]
    );
    assert_eq!(row(&driver, general, reacted.id).reactions_as_of, seq(7));
    assert!(driver.core.reacted_by_me(general, reacted.id, emoji(TADA)));
    assert!(
        !driver
            .core
            .reacted_by_me(general, reacted.id, emoji(THUMBS))
    );
    assert!(row(&driver, general, reacted.id).reacted_by(world.me.id, emoji(TADA)));
}

#[test]
fn anyone_who_can_post_may_react_to_anything_but_not_in_a_read_only_place() {
    let (world, driver, theirs) = setup();
    let general = world.channel.id;
    assert!(driver.core.can_react_message(general, theirs.id));
    assert!(
        !driver
            .core
            .can_react_message(general, MessageId::generate())
    );

    let mut world = World::new();
    world.channel.archived = true;
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let theirs = world.message(general, &world.other, 5, "theirs");
    driver.load_history(general, vec![theirs.clone()], false);
    assert!(!driver.core.can_react_message(general, theirs.id));
    let out = driver.send(toggle(general, &theirs, THUMBS));
    assert!(requests(&out).is_empty());
    assert!(driver.core.notice().is_some());
    assert!(row(&driver, general, theirs.id).reactions.is_empty());
}

// ----- Toggling -----

#[test]
fn a_toggle_shows_at_once_and_sends_one_request_with_no_journal() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    let out = driver.send(toggle(general, &theirs, THUMBS));
    assert_eq!(
        requests(&out),
        vec![(
            theirs.id,
            SetReactionRequest {
                emoji: emoji(THUMBS),
                present: true
            }
        )]
    );
    assert!(
        out.effects
            .iter()
            .all(|effect| !matches!(effect, Effect::PersistSubmission(_))),
        "an idempotent state-setting request is not journaled"
    );
    assert_eq!(driver.core.unsettled_operations(), 0);
    assert_eq!(
        shown(&world, &driver, general, &theirs),
        vec![(THUMBS.to_owned(), vec!["me"])]
    );
    assert!(driver.core.reacted_by_me(general, theirs.id, emoji(THUMBS)));
    assert!(
        out.changes
            .iter()
            .any(|c| matches!(c, Change::TimelineItemUpdated { .. }))
    );

    // The answer replaces what is shown with the server's state.
    let server = world.with_reactions(&theirs, 12, &[(THUMBS, &[&world.other, &world.me])]);
    let out = result(&mut driver, &theirs, THUMBS, respond(&server));
    assert!(requests(&out).is_empty());
    assert_eq!(
        shown(&world, &driver, general, &theirs),
        vec![(THUMBS.to_owned(), vec!["other", "me"])]
    );
    assert_eq!(row(&driver, general, theirs.id).reactions_as_of, seq(12));

    // Toggling again removes it.
    let out = driver.send(toggle(general, &theirs, THUMBS));
    assert!(!requests(&out)[0].1.present);
    assert!(
        shown(&world, &driver, general, &theirs)
            .iter()
            .all(|(_, who)| !who.contains(&"me"))
    );
}

#[test]
fn a_new_emoji_goes_last_and_the_person_goes_last_among_those_who_used_it() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    let with_thumbs = world.with_reactions(&theirs, 11, &[(THUMBS, &[&world.other])]);
    driver.send(world.reactions_event(11, &theirs, &[(THUMBS, &[&world.other])]));
    driver.send(toggle(general, &theirs, TADA));
    driver.send(toggle(general, &theirs, THUMBS));
    assert_eq!(
        shown(&world, &driver, general, &theirs),
        vec![
            (THUMBS.to_owned(), vec!["other", "me"]),
            (TADA.to_owned(), vec!["me"])
        ]
    );
    let _ = with_thumbs;
    // Different emoji are separate requests, each in flight at once.
    assert_eq!(
        driver
            .effects_matching(|e| matches!(e, Effect::SetReaction { .. }).then_some(()))
            .len(),
        2
    );
}

#[test]
fn rapid_toggles_collapse_to_the_final_state() {
    // On, then off while the first request is in flight: the first is answered, then the final
    // wish is sent.
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    let out = driver.send(toggle(general, &theirs, THUMBS));
    assert_eq!(requests(&out).len(), 1);
    let out = driver.send(toggle(general, &theirs, THUMBS));
    assert!(requests(&out).is_empty(), "one request in flight per emoji");
    assert!(
        shown(&world, &driver, general, &theirs).is_empty(),
        "off again, as shown"
    );

    let added = world.with_reactions(&theirs, 11, &[(THUMBS, &[&world.me])]);
    let out = result(&mut driver, &theirs, THUMBS, respond(&added));
    let next = requests(&out);
    assert_eq!(next.len(), 1);
    assert!(!next[0].1.present, "the final wish is off");
    assert!(shown(&world, &driver, general, &theirs).is_empty());
    let removed = world.with_reactions(&theirs, 12, &[]);
    let out = result(&mut driver, &theirs, THUMBS, respond(&removed));
    assert!(requests(&out).is_empty());
    assert_eq!(row(&driver, general, theirs.id).reactions_as_of, seq(12));

    // On, off, on: the first request already did what was finally wanted.
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(toggle(general, &theirs, THUMBS));
    driver.send(toggle(general, &theirs, THUMBS));
    driver.send(toggle(general, &theirs, THUMBS));
    let added = world.with_reactions(&theirs, 11, &[(THUMBS, &[&world.me])]);
    let out = result(&mut driver, &theirs, THUMBS, respond(&added));
    assert!(requests(&out).is_empty());
    assert!(driver.core.reacted_by_me(general, theirs.id, emoji(THUMBS)));
}

#[test]
fn a_toggle_the_server_already_satisfies_is_not_sent() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(toggle(general, &theirs, THUMBS)); // on, in flight
    driver.send(toggle(general, &theirs, THUMBS)); // the person now wants it off
    // Another device removes it first (the event is newer than the answer to come)...
    driver.send(world.reactions_event(12, &theirs, &[]));
    // ...so the late answer to the first request is older than what is held and is ignored.
    let added = world.with_reactions(&theirs, 11, &[(THUMBS, &[&world.me])]);
    let out = result(&mut driver, &theirs, THUMBS, respond(&added));
    assert!(
        requests(&out).is_empty(),
        "the server already has what the person wants"
    );
    assert!(shown(&world, &driver, general, &theirs).is_empty());
    assert_eq!(row(&driver, general, theirs.id).reactions_as_of, seq(12));
}

#[test]
fn other_peoples_changes_arrive_underneath_a_pending_toggle() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(toggle(general, &theirs, THUMBS));
    // Someone else reacts meanwhile; the person's pending change stays on top.
    driver.send(world.reactions_event(
        12,
        &theirs,
        &[(THUMBS, &[&world.other]), (TADA, &[&world.other])],
    ));
    assert_eq!(
        shown(&world, &driver, general, &theirs),
        vec![
            (THUMBS.to_owned(), vec!["other", "me"]),
            (TADA.to_owned(), vec!["other"])
        ]
    );
    // The answer (at a later position) shows the server's state.
    let server = world.with_reactions(
        &theirs,
        13,
        &[
            (THUMBS, &[&world.other, &world.me]),
            (TADA, &[&world.other]),
        ],
    );
    result(&mut driver, &theirs, THUMBS, respond(&server));
    assert_eq!(
        shown(&world, &driver, general, &theirs),
        vec![
            (THUMBS.to_owned(), vec!["other", "me"]),
            (TADA.to_owned(), vec!["other"])
        ]
    );
}

#[test]
fn a_pending_removal_is_not_undone_by_events_that_still_list_the_person() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let plain = world.message(general, &world.other, 5, "theirs");
    let reacted = world.with_reactions(&plain, 7, &[(THUMBS, &[&world.me, &world.other])]);
    driver.load_history(general, vec![reacted.clone()], false);
    let out = driver.send(toggle(general, &reacted, THUMBS));
    assert!(!requests(&out)[0].1.present);
    // An event from before the removal reaches the server still lists the person.
    driver.send(world.reactions_event(
        12,
        &reacted,
        &[
            (THUMBS, &[&world.me, &world.other]),
            (TADA, &[&world.other]),
        ],
    ));
    assert_eq!(
        shown(&world, &driver, general, &reacted),
        vec![
            (THUMBS.to_owned(), vec!["other"]),
            (TADA.to_owned(), vec!["other"])
        ]
    );
}

// ----- Failures -----

#[test]
fn an_unknown_outcome_is_repeated_then_reverts_with_a_notice() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    let first = driver.send(toggle(general, &theirs, THUMBS));
    let sent = requests(&first);
    // Three repeats of the same request...
    for attempt in 1..=3 {
        let out = result(&mut driver, &theirs, THUMBS, Err(lost()));
        assert_eq!(requests(&out), sent, "repeat {attempt} is the same request");
        assert!(driver.core.reacted_by_me(general, theirs.id, emoji(THUMBS)));
        assert!(driver.core.notice().is_none());
    }
    // ...then it gives up, shows the server's state, and says so.
    let out = result(&mut driver, &theirs, THUMBS, Err(lost()));
    assert!(requests(&out).is_empty());
    assert!(!driver.core.reacted_by_me(general, theirs.id, emoji(THUMBS)));
    assert!(driver.core.notice().is_some());
}

#[test]
fn a_repeat_that_succeeds_shows_the_servers_state() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(toggle(general, &theirs, TADA));
    result(&mut driver, &theirs, TADA, Err(lost()));
    result(&mut driver, &theirs, TADA, Err(lost()));
    let server = world.with_reactions(&theirs, 12, &[(TADA, &[&world.me])]);
    let out = result(&mut driver, &theirs, TADA, respond(&server));
    assert!(requests(&out).is_empty());
    assert!(driver.core.reacted_by_me(general, theirs.id, emoji(TADA)));
    assert!(driver.core.notice().is_none());
}

#[test]
fn a_repeat_asks_for_the_latest_wish() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(toggle(general, &theirs, TADA)); // on
    driver.send(toggle(general, &theirs, TADA)); // the person changes their mind: off
    let out = result(&mut driver, &theirs, TADA, Err(lost()));
    let repeat = requests(&out);
    assert_eq!(repeat.len(), 1);
    assert!(
        !repeat[0].1.present,
        "setting the latest state is right whatever happened"
    );
}

#[test]
fn refusals_revert_the_display_and_say_why() {
    for (code, words) in [
        (ErrorCode::LimitReached, "most different reactions"),
        (ErrorCode::Forbidden, "can't react"),
        (ErrorCode::NotFound, "no longer exists"),
        (ErrorCode::InvalidRequest, "emoji"),
    ] {
        let (world, mut driver, theirs) = setup();
        let general = world.channel.id;
        driver.send(toggle(general, &theirs, THUMBS));
        let out = result(&mut driver, &theirs, THUMBS, Err(rejected(code)));
        assert!(requests(&out).is_empty(), "{code:?} is final");
        assert!(
            shown(&world, &driver, general, &theirs).is_empty(),
            "{code:?} reverts"
        );
        let notice = driver.core.notice().unwrap_or_default().to_owned();
        assert!(notice.contains(words), "{code:?}: {notice:?}");
    }
}

#[test]
fn the_limit_is_checked_before_asking() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let plain = world.message(general, &world.other, 5, "theirs");
    let all: Vec<ReactionEmoji> = lazy_domain::emoji::groups()
        .flat_map(lazy_domain::emoji::in_group)
        .map(|e| emoji(e.as_str()))
        .take(lazy_domain::MAX_REACTION_KINDS + 1)
        .collect();
    let mut full = plain.clone();
    full.reactions = all[..lazy_domain::MAX_REACTION_KINDS]
        .iter()
        .map(|e| lazy_protocol::ReactionSummary {
            emoji: *e,
            users: vec![world.other.id],
        })
        .collect();
    full.reactions_as_of = seq(9);
    driver.load_history(general, vec![full.clone()], false);

    // A 51st different emoji is refused without a request...
    let out = driver.send(Input::ToggleReaction {
        conversation: general,
        message: full.id,
        emoji: all[lazy_domain::MAX_REACTION_KINDS],
    });
    assert!(requests(&out).is_empty());
    assert!(
        driver
            .core
            .notice()
            .unwrap()
            .contains("most different reactions")
    );
    // ...but joining one that is already there is fine.
    let out = driver.send(Input::ToggleReaction {
        conversation: general,
        message: full.id,
        emoji: all[0],
    });
    assert_eq!(requests(&out).len(), 1);
}

#[test]
fn an_expired_session_reverts_pending_reactions() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(toggle(general, &theirs, THUMBS));
    driver.send(Input::SessionExpired);
    assert!(shown(&world, &driver, general, &theirs).is_empty());
    let out = result(&mut driver, &theirs, THUMBS, Err(lost()));
    assert!(
        out.is_empty(),
        "nothing is waiting for that answer any more"
    );
}

#[test]
fn a_unauthenticated_answer_ends_the_session() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(toggle(general, &theirs, THUMBS));
    result(
        &mut driver,
        &theirs,
        THUMBS,
        Err(rejected(ErrorCode::Unauthenticated)),
    );
    assert!(matches!(
        driver.core.phase(),
        lazy_client_core::SessionPhase::SignedOut { .. }
    ));
    assert!(shown(&world, &driver, general, &theirs).is_empty());
}

// ----- The version rule -----

#[test]
fn reactions_with_a_higher_position_win_however_they_arrive() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    let people = [&world.other];

    driver.send(world.reactions_event(12, &theirs, &[(THUMBS, &people)]));
    assert_eq!(row(&driver, general, theirs.id).reactions_as_of, seq(12));

    // Older: ignored. Equal: what is held is kept. Newer: taken.
    driver.send(world.reactions_event(11, &theirs, &[(TADA, &people)]));
    assert_eq!(
        row(&driver, general, theirs.id).reactions[0].emoji,
        emoji(THUMBS)
    );
    let tie = world.with_reactions(&theirs, 12, &[(HEART, &people)]);
    driver.send(world.edited_event(13, &world.edited(&tie, "edited"), None));
    // The edit's payload carried the tie, which changes no reactions; its content did apply.
    assert_eq!(
        row(&driver, general, theirs.id).reactions[0].emoji,
        emoji(THUMBS)
    );
    assert_eq!(row(&driver, general, theirs.id).revision(), Some(2));
    driver.send(world.reactions_event(14, &theirs, &[(HEART, &people)]));
    assert_eq!(
        row(&driver, general, theirs.id).reactions[0].emoji,
        emoji(HEART)
    );

    // A duplicate delivery changes nothing at all.
    let out = driver.send(world.reactions_event(14, &theirs, &[(HEART, &people)]));
    assert!(
        out.changes
            .iter()
            .all(|c| !matches!(c, Change::TimelineItemUpdated { .. }))
    );
}

#[test]
fn an_answer_ahead_of_the_event_stream_wins_over_the_older_events_that_follow() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(toggle(general, &theirs, THUMBS));
    // The answer is at position 20; the stream is still at 10, so events 11..=20 are yet to come.
    let answer = world.with_reactions(&theirs, 20, &[(THUMBS, &[&world.other, &world.me])]);
    result(&mut driver, &theirs, THUMBS, respond(&answer));
    // An older event changes nothing; so does one at the same position (what is held is kept).
    driver.send(world.reactions_event(15, &theirs, &[(TADA, &[&world.other])]));
    driver.send(world.reactions_event(20, &theirs, &[(HEART, &[&world.other])]));
    assert_eq!(
        shown(&world, &driver, general, &theirs),
        vec![(THUMBS.to_owned(), vec!["other", "me"])]
    );
    // A newer one is taken.
    driver.send(world.reactions_event(21, &theirs, &[(TADA, &[&world.other])]));
    assert_eq!(
        shown(&world, &driver, general, &theirs),
        vec![(TADA.to_owned(), vec!["other"])]
    );
}

#[test]
fn an_edit_or_a_page_never_downgrades_reactions() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let plain = world.message(general, &world.other, 5, "theirs");
    // The first page is full, so a second (overlapping) page can be asked for.
    let newest = world.message(general, &world.other, 9, "newest");
    driver.load_history(general, vec![plain.clone(), newest], true);
    driver.send(world.reactions_event(
        12,
        &plain,
        &[(THUMBS, &[&world.other]), (TADA, &[&world.me])],
    ));

    // An edit whose payload was built before that reaction change.
    let stale = world.with_reactions(
        &world.edited(&plain, "edited"),
        6,
        &[(HEART, &[&world.other])],
    );
    driver.send(world.edited_event(13, &stale, None));
    let after = row(&driver, general, plain.id);
    assert_eq!(after.revision(), Some(2), "the content was newer");
    assert_eq!(after.reactions_as_of, seq(12));
    assert_eq!(after.reactions.len(), 2, "the reactions were not");

    // A page that overlaps the row and predates the change.
    driver.send(Input::LoadOlder {
        conversation: general,
    });
    driver.load_history(general, vec![stale.clone()], false);
    assert_eq!(row(&driver, general, plain.id).reactions_as_of, seq(12));
    assert_eq!(row(&driver, general, plain.id).reactions.len(), 2);
}

#[test]
fn a_page_in_flight_does_not_undo_a_reaction_that_arrived_meanwhile() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let newest = world.message(general, &world.other, 9, "newest");
    driver.load_history(general, vec![newest], true);
    let old = world.message(general, &world.other, 2, "old message");

    // The older page is requested; a reaction change for a message in it arrives first; the
    // page, computed before the change, arrives after.
    driver.send(Input::LoadOlder {
        conversation: general,
    });
    driver.send(world.reactions_event(12, &old, &[(THUMBS, &[&world.other])]));
    let before = world.with_reactions(&old, 6, &[(TADA, &[&world.other])]);
    driver.load_history(general, vec![before], false);
    assert_eq!(
        shown(&world, &driver, general, &old),
        vec![(THUMBS.to_owned(), vec!["other"])],
        "the newer list wins over the page"
    );
    assert_eq!(row(&driver, general, old.id).reactions_as_of, seq(12));

    // A page that is NEWER than the event keeps its list.
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let newest = world.message(general, &world.other, 9, "newest");
    driver.load_history(general, vec![newest], true);
    let old = world.message(general, &world.other, 2, "old message");
    driver.send(Input::LoadOlder {
        conversation: general,
    });
    driver.send(world.reactions_event(12, &old, &[(THUMBS, &[&world.other])]));
    let after = world.with_reactions(&old, 15, &[(TADA, &[&world.other])]);
    driver.load_history(general, vec![after], false);
    assert_eq!(
        shown(&world, &driver, general, &old),
        vec![(TADA.to_owned(), vec!["other"])]
    );
}

#[test]
fn events_for_what_is_not_known_are_ignored() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    let stranger = world.message(general, &world.other, 3, "never loaded");
    driver.send(world.reactions_event(12, &stranger, &[(THUMBS, &[&world.other])]));
    assert!(
        !driver
            .core
            .timeline(general)
            .iter()
            .any(|item| item.message_id() == Some(stranger.id))
    );
    let elsewhere = channel(world.workspace.id, "elsewhere");
    let foreign = world.message(elsewhere.id, &world.other, 7, "foreign");
    driver.send(world.reactions_event(13, &foreign, &[(THUMBS, &[&world.other])]));
    assert!(driver.core.timeline(elsewhere.id).is_empty());
    assert!(row(&driver, general, theirs.id).reactions.is_empty());
}

#[test]
fn a_deleted_message_takes_its_reactions_and_pending_changes_with_it() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(toggle(general, &theirs, THUMBS));
    driver.send(world.deleted_event(11, &world.tombstone(&theirs)));
    assert!(driver.core.timeline(general).is_empty());
    let out = result(&mut driver, &theirs, THUMBS, Err(lost()));
    assert!(out.is_empty());
    assert!(driver.core.notice().is_none());
}

#[test]
fn a_response_that_is_a_tombstone_removes_the_message() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(toggle(general, &theirs, THUMBS));
    result(
        &mut driver,
        &theirs,
        THUMBS,
        respond(&world.tombstone(&theirs)),
    );
    assert!(driver.core.timeline(general).is_empty());
}

#[test]
fn a_confirmed_send_keeps_the_newer_reactions_of_a_row_that_beat_it() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let out = driver.send(Input::SendMessage {
        conversation: general,
        text: "hello".into(),
    });
    let operation = out
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::PersistSubmission(entry) => Some(entry.operation_id),
            _ => None,
        })
        .unwrap();
    driver.send(Input::SubmissionPersisted { operation });
    // A page brings the message in (already reacted to) before the send is confirmed.
    let created = world.message(general, &world.me, 11, "hello");
    let reacted = world.with_reactions(&created, 12, &[(THUMBS, &[&world.other])]);
    driver.load_history(general, vec![reacted], false);
    // The confirmation describes the message as it was committed, with no reactions.
    driver.send(Input::SendResult {
        operation,
        result: Ok(lazy_protocol::SendMessageResponse {
            message: created.clone(),
        }),
    });
    assert_eq!(
        shown(&world, &driver, general, &created),
        vec![(THUMBS.to_owned(), vec!["other"])]
    );
}

// ----- The cache -----

#[test]
fn the_cache_holds_the_servers_reactions_not_unsaved_ones() {
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(world.reactions_event(11, &theirs, &[(THUMBS, &[&world.other])]));
    driver.send(toggle(general, &theirs, TADA)); // pending
    driver.send(world.event(world.message(general, &world.other, 12, "later"), None));
    let cached = driver.last_cache().unwrap();
    let timeline = cached.workspaces[0]
        .timelines
        .iter()
        .find(|t| t.conversation == general)
        .unwrap();
    let message = timeline
        .messages
        .iter()
        .find(|m| m.id == theirs.id)
        .unwrap();
    assert_eq!(message.reactions_as_of, seq(11));
    assert_eq!(message.reactions.len(), 1, "only what the server has");
    assert_eq!(message.reactions[0].emoji, emoji(THUMBS));

    // Saved, it is in the cache; a restart shows it at once.
    let server = world.with_reactions(
        &theirs,
        13,
        &[(THUMBS, &[&world.other]), (TADA, &[&world.me])],
    );
    result(&mut driver, &theirs, TADA, respond(&server));
    driver.send(world.event(world.message(general, &world.other, 14, "again"), None));
    let cache = driver.last_cache().unwrap().clone();
    let mut restarted = Driver::new();
    restarted.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache),
        ..Default::default()
    }));
    assert_eq!(row(&restarted, general, theirs.id).reactions_as_of, seq(13));
    assert!(
        restarted
            .core
            .reacted_by_me(general, theirs.id, emoji(TADA))
    );
}

// ----- The limit on different kinds, and the order of requests -----

/// A message by the other person at position 5 that already holds as many different emoji as it
/// can: the first is the person's alone, the others are the other person's. Returns the emoji.
fn full_message(count: usize) -> (World, Driver, Message, Vec<String>) {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let kinds: Vec<String> =
        lazy_domain::emoji::in_group(lazy_domain::emoji::Group::SmileysAndEmotion)
            .take(lazy_domain::MAX_REACTION_KINDS + 1)
            .map(|e| e.as_str().to_owned())
            .collect();
    let plain = world.message(general, &world.other, 5, "their words");
    let list: Vec<(&str, Vec<&lazy_protocol::User>)> = kinds[..count]
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            (
                kind.as_str(),
                if i == 0 {
                    vec![&world.me]
                } else {
                    vec![&world.other]
                },
            )
        })
        .collect();
    let borrowed: Vec<(&str, &[&lazy_protocol::User])> = list
        .iter()
        .map(|(kind, users)| (*kind, users.as_slice()))
        .collect();
    let message = world.with_reactions(&plain, 7, &borrowed);
    driver.load_history(general, vec![message.clone()], false);
    (world, driver, message, kinds)
}

/// `message` with `kind` taken off its list, as the server answers a withdrawal.
fn without(world: &World, message: &Message, kind: &str, at: i64) -> Message {
    let mut answered = message.clone();
    answered.reactions.retain(|r| r.emoji != emoji(kind));
    answered.reactions_as_of = seq(at);
    let _ = world;
    answered
}

#[test]
fn an_add_that_would_not_fit_waits_for_the_withdrawal_that_makes_room() {
    let (world, mut driver, message, kinds) = full_message(lazy_domain::MAX_REACTION_KINDS);
    let general = message.conversation_id;
    let out = driver.send(toggle(general, &message, &kinds[0]));
    assert_eq!(requests(&out).len(), 1, "the withdrawal goes");
    assert!(!requests(&out)[0].1.present);
    let extra = &kinds[lazy_domain::MAX_REACTION_KINDS];
    let out = driver.send(toggle(general, &message, extra));
    assert!(
        requests(&out).is_empty(),
        "the add would be refused, so it waits"
    );
    // It is shown meanwhile, and counts as unsaved.
    assert!(driver.core.reacted_by_me(general, message.id, emoji(extra)));
    assert_eq!(driver.core.pending_reaction_changes(), 2);
    // The withdrawal is answered: now the add goes.
    let out = result(
        &mut driver,
        &message,
        &kinds[0],
        respond(&without(&world, &message, &kinds[0], 20)),
    );
    let sent = requests(&out);
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].1.emoji, sent[0].1.present), (emoji(extra), true));
    assert_eq!(driver.core.pending_reaction_changes(), 1);
}

#[test]
fn an_add_goes_at_once_when_the_server_has_room() {
    let (_, mut driver, message, kinds) = full_message(lazy_domain::MAX_REACTION_KINDS - 1);
    let general = message.conversation_id;
    driver.send(toggle(general, &message, &kinds[0]));
    let out = driver.send(toggle(
        general,
        &message,
        &kinds[lazy_domain::MAX_REACTION_KINDS - 1],
    ));
    assert_eq!(requests(&out).len(), 1, "there is room, so nothing waits");
}

#[test]
fn an_add_to_a_kind_already_on_the_message_never_waits() {
    let (_, mut driver, message, kinds) = full_message(lazy_domain::MAX_REACTION_KINDS);
    let general = message.conversation_id;
    driver.send(toggle(general, &message, &kinds[0]));
    let out = driver.send(toggle(general, &message, &kinds[1]));
    assert_eq!(requests(&out).len(), 1);
}

#[test]
fn a_withdrawal_that_leaves_the_kind_in_use_makes_no_room_so_the_add_is_refused_at_once() {
    // The first kind is also used by the other person, so withdrawing it frees no slot.
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let kinds: Vec<String> =
        lazy_domain::emoji::in_group(lazy_domain::emoji::Group::SmileysAndEmotion)
            .take(lazy_domain::MAX_REACTION_KINDS + 1)
            .map(|e| e.as_str().to_owned())
            .collect();
    let plain = world.message(general, &world.other, 5, "their words");
    let list: Vec<(&str, Vec<&lazy_protocol::User>)> = kinds[..lazy_domain::MAX_REACTION_KINDS]
        .iter()
        .map(|kind| (kind.as_str(), vec![&world.me, &world.other]))
        .collect();
    let borrowed: Vec<(&str, &[&lazy_protocol::User])> = list
        .iter()
        .map(|(kind, users)| (*kind, users.as_slice()))
        .collect();
    let message = world.with_reactions(&plain, 7, &borrowed);
    driver.load_history(general, vec![message.clone()], false);
    driver.send(toggle(general, &message, &kinds[0]));
    let out = driver.send(toggle(
        general,
        &message,
        &kinds[lazy_domain::MAX_REACTION_KINDS],
    ));
    assert!(requests(&out).is_empty());
    assert!(
        driver.core.notice().is_some(),
        "the row still shows as many kinds as it can hold, so the add is refused with a notice"
    );
    assert_eq!(driver.core.pending_reaction_changes(), 1);
}

#[test]
fn a_waiting_add_goes_when_its_withdrawal_is_refused() {
    let (_, mut driver, message, kinds) = full_message(lazy_domain::MAX_REACTION_KINDS);
    let general = message.conversation_id;
    driver.send(toggle(general, &message, &kinds[0]));
    driver.send(toggle(
        general,
        &message,
        &kinds[lazy_domain::MAX_REACTION_KINDS],
    ));
    let out = result(
        &mut driver,
        &message,
        &kinds[0],
        Err(rejected(ErrorCode::Forbidden)),
    );
    assert_eq!(
        requests(&out).len(),
        1,
        "the add goes; the server will say what it thinks"
    );
    assert!(requests(&out)[0].1.present);
}

#[test]
fn a_waiting_add_goes_when_its_withdrawal_gives_up_after_repeating() {
    let (_, mut driver, message, kinds) = full_message(lazy_domain::MAX_REACTION_KINDS);
    let general = message.conversation_id;
    driver.send(toggle(general, &message, &kinds[0]));
    driver.send(toggle(
        general,
        &message,
        &kinds[lazy_domain::MAX_REACTION_KINDS],
    ));
    for _ in 0..3 {
        let out = result(&mut driver, &message, &kinds[0], Err(lost()));
        let sent = requests(&out);
        assert_eq!(sent.len(), 1, "only the withdrawal is repeated");
        assert!(!sent[0].1.present);
    }
    let out = result(&mut driver, &message, &kinds[0], Err(lost()));
    let sent = requests(&out);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].1.present, "the withdrawal gave up, so the add goes");
}

#[test]
fn a_waiting_add_goes_when_someone_else_makes_room() {
    let (world, mut driver, message, kinds) = full_message(lazy_domain::MAX_REACTION_KINDS);
    let general = message.conversation_id;
    driver.send(toggle(general, &message, &kinds[0]));
    driver.send(toggle(
        general,
        &message,
        &kinds[lazy_domain::MAX_REACTION_KINDS],
    ));
    // The other person withdraws their only kind meanwhile.
    let after = without(&world, &message, &kinds[1], 21);
    let out = driver.send(world.event_at(
        21,
        Event::ReactionsChanged {
            conversation_id: general,
            message_id: message.id,
            reactions: after.reactions.clone(),
        },
    ));
    let sent = requests(&out);
    assert_eq!(sent.len(), 1);
    assert!(sent[0].1.present);
}

#[test]
fn a_waiting_add_the_person_takes_back_is_never_sent() {
    let (world, mut driver, message, kinds) = full_message(lazy_domain::MAX_REACTION_KINDS);
    let general = message.conversation_id;
    let extra = &kinds[lazy_domain::MAX_REACTION_KINDS];
    driver.send(toggle(general, &message, &kinds[0]));
    driver.send(toggle(general, &message, extra));
    let out = driver.send(toggle(general, &message, extra));
    assert!(requests(&out).is_empty());
    assert!(!driver.core.reacted_by_me(general, message.id, emoji(extra)));
    let out = result(
        &mut driver,
        &message,
        &kinds[0],
        respond(&without(&world, &message, &kinds[0], 20)),
    );
    assert!(requests(&out).is_empty());
    assert_eq!(driver.core.pending_reaction_changes(), 0);
}

/// A message with 49 kinds. The first is the person's; `shared` makes the other person use it
/// too. Returns the emoji, with two more kinds for adding.
fn nearly_full(shared: bool) -> (World, Driver, Message, Vec<String>) {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let kinds: Vec<String> =
        lazy_domain::emoji::in_group(lazy_domain::emoji::Group::SmileysAndEmotion)
            .take(lazy_domain::MAX_REACTION_KINDS + 1)
            .map(|e| e.as_str().to_owned())
            .collect();
    let plain = world.message(general, &world.other, 5, "their words");
    let list: Vec<(&str, Vec<&lazy_protocol::User>)> = kinds[..lazy_domain::MAX_REACTION_KINDS - 1]
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            let users = match (i, shared) {
                (0, false) => vec![&world.me],
                (0, true) => vec![&world.me, &world.other],
                _ => vec![&world.other],
            };
            (kind.as_str(), users)
        })
        .collect();
    let borrowed: Vec<(&str, &[&lazy_protocol::User])> = list
        .iter()
        .map(|(kind, users)| (*kind, users.as_slice()))
        .collect();
    let message = world.with_reactions(&plain, 7, &borrowed);
    driver.load_history(general, vec![message.clone()], false);
    (world, driver, message, kinds)
}

/// The other person adds `kind`, which fills the message up.
fn someone_fills_it_up(world: &World, driver: &mut Driver, message: &Message, kind: &str) {
    let mut reactions = message.reactions.clone();
    reactions.push(lazy_protocol::ReactionSummary {
        emoji: emoji(kind),
        users: vec![world.other.id],
    });
    driver.send(world.event_at(
        21,
        Event::ReactionsChanged {
            conversation_id: message.conversation_id,
            message_id: message.id,
            reactions,
        },
    ));
}

#[test]
fn a_repeat_after_a_lost_answer_waits_for_room_like_any_other_add() {
    let (world, mut driver, message, kinds) = nearly_full(false);
    let general = message.conversation_id;
    let added = &kinds[lazy_domain::MAX_REACTION_KINDS - 1];
    let filler = &kinds[lazy_domain::MAX_REACTION_KINDS];
    // There is room for the add, so it goes; the withdrawal goes beside it.
    assert_eq!(
        requests(&driver.send(toggle(general, &message, added))).len(),
        1
    );
    assert_eq!(
        requests(&driver.send(toggle(general, &message, &kinds[0]))).len(),
        1
    );
    // Meanwhile someone else takes the last slot, and the add's answer is lost.
    someone_fills_it_up(&world, &mut driver, &message, filler);
    let out = result(&mut driver, &message, added, Err(lost()));
    assert!(
        requests(&out).is_empty(),
        "repeated now it would be refused; the withdrawal is about to make room"
    );
    assert_eq!(driver.core.pending_reaction_changes(), 2);
    // The withdrawal returns, and the add goes.
    let mut answered = message.clone();
    answered.reactions.retain(|r| r.emoji != emoji(&kinds[0]));
    answered.reactions.push(lazy_protocol::ReactionSummary {
        emoji: emoji(filler),
        users: vec![world.other.id],
    });
    answered.reactions_as_of = seq(22);
    let out = result(&mut driver, &message, &kinds[0], respond(&answered));
    let sent = requests(&out);
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].1.emoji, sent[0].1.present), (emoji(added), true));
}

#[test]
fn a_withdrawal_of_a_kind_others_use_does_not_hold_back_a_repeat() {
    // The first kind is also the other person's, so withdrawing it frees nothing.
    let (world, mut driver, message, kinds) = nearly_full(true);
    let general = message.conversation_id;
    let added = &kinds[lazy_domain::MAX_REACTION_KINDS - 1];
    let filler = &kinds[lazy_domain::MAX_REACTION_KINDS];
    driver.send(toggle(general, &message, added));
    driver.send(toggle(general, &message, &kinds[0]));
    someone_fills_it_up(&world, &mut driver, &message, filler);
    let out = result(&mut driver, &message, added, Err(lost()));
    let sent = requests(&out);
    assert_eq!(
        sent.len(),
        1,
        "no withdrawal will make room, so the server decides"
    );
    assert!(sent[0].1.present);
}

// ----- Shortcodes -----

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

#[test]
fn shortcodes_become_emoji_when_a_message_is_sent() {
    let (world, mut driver, _) = setup();
    let general = world.channel.id;
    let doc = sent_document(&mut driver, general, "party :tada: time");
    assert_eq!(
        doc,
        MessageDocument::from_markdown("party 🎉 time").unwrap()
    );
    assert_eq!(doc.plain_text(), "party 🎉 time");

    // Not in code, not after a backslash, not inside a word or a time, not when unknown.
    for (typed, kept) in [
        ("`:tada:`", ":tada:"),
        ("\\:tada:", ":tada:"),
        ("at 10:30:45 sharp", "10:30:45"),
        ("the :notanemoji: here", ":notanemoji:"),
    ] {
        let doc = sent_document(&mut driver, general, typed);
        assert!(
            doc.plain_text().contains(kept),
            "{typed:?} -> {:?}",
            doc.plain_text()
        );
        assert!(!doc.plain_text().contains('🎉'), "{typed:?}");
    }
    let fenced = sent_document(&mut driver, general, "```\n:tada:\n```");
    assert!(fenced.plain_text().contains(":tada:"));
}

#[test]
fn the_draft_keeps_what_was_typed() {
    let (world, mut driver, _) = setup();
    let general = world.channel.id;
    driver.send(Input::SetDraft {
        conversation: general,
        text: "cheers :tada:".into(),
    });
    // A message that cannot be sent leaves the draft exactly as typed.
    let long = format!(":tada: {}", "x".repeat(40_001));
    driver.send(Input::SetDraft {
        conversation: general,
        text: long.clone(),
    });
    driver.send(Input::SendMessage {
        conversation: general,
        text: long.clone(),
    });
    assert_eq!(driver.core.draft(general), long);
}

#[test]
fn an_edit_converts_shortcodes_but_the_editor_keeps_the_source() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let mine = world.message(general, &world.me, 5, "my words");
    driver.load_history(general, vec![mine.clone()], false);

    let out = driver.send(Input::EditMessage {
        conversation: general,
        message: mine.id,
        text: "now :tada:".into(),
    });
    let entry = out
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::PersistSubmission(entry) => Some(entry.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        entry.document,
        MessageDocument::from_markdown("now 🎉").unwrap()
    );
    assert_eq!(
        driver.core.edit_text(general, mine.id).unwrap(),
        "now :tada:",
        "the editor shows what was typed while the edit is saved"
    );
    let row = driver
        .core
        .timeline(general)
        .iter()
        .find(|item| item.message_id() == Some(mine.id))
        .unwrap();
    assert_eq!(row.edit, EditState::Saving);

    // Typing the shortcode of what is already there changes nothing.
    let (world, mut driver, _) = setup();
    let general = world.channel.id;
    let mine = world.message(general, &world.me, 6, "👍");
    driver.send(world.event(mine.clone(), None));
    let out = driver.send(Input::EditMessage {
        conversation: general,
        message: mine.id,
        text: ":+1:".into(),
    });
    assert!(out.is_empty(), "{out:?}");
}

// ----- Emoji preferences -----

fn persisted(out: &Output) -> Vec<EmojiPrefs> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::PersistEmojiPrefs(prefs) => Some(prefs.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn recent_emoji_are_most_recent_first_without_repeats_and_bounded() {
    let (_, mut driver, _) = setup();
    assert_eq!(driver.core.emoji_prefs(), &EmojiPrefs::default());

    let out = driver.send(Input::RecordEmojiUse(emoji(THUMBS)));
    assert_eq!(persisted(&out).len(), 1);
    assert!(out.changes.contains(&Change::EmojiPrefsChanged));
    driver.send(Input::RecordEmojiUse(emoji(TADA)));
    driver.send(Input::RecordEmojiUse(emoji(HEART)));
    assert_eq!(
        driver.core.emoji_prefs().recent,
        vec![emoji(HEART), emoji(TADA), emoji(THUMBS)]
    );
    // Using one again moves it to the front instead of repeating it.
    let out = driver.send(Input::RecordEmojiUse(emoji(THUMBS)));
    assert_eq!(
        persisted(&out)[0].recent,
        vec![emoji(THUMBS), emoji(HEART), emoji(TADA)]
    );
    // Using the first one again is nothing to save.
    let out = driver.send(Input::RecordEmojiUse(emoji(THUMBS)));
    assert!(out.is_empty());

    for glyph in lazy_domain::emoji::groups()
        .flat_map(lazy_domain::emoji::in_group)
        .take(40)
    {
        driver.send(Input::RecordEmojiUse(emoji(glyph.as_str())));
    }
    let recent = &driver.core.emoji_prefs().recent;
    assert_eq!(recent.len(), MAX_RECENT_EMOJI);
    let mut distinct = recent.clone();
    distinct.sort_by_key(|e| e.to_string());
    distinct.dedup();
    assert_eq!(distinct.len(), MAX_RECENT_EMOJI);
}

#[test]
fn the_skin_tone_is_a_number_from_zero_to_five() {
    let (_, mut driver, _) = setup();
    let out = driver.send(Input::SetSkinTone(3));
    assert_eq!(persisted(&out)[0].skin_tone, 3);
    assert!(out.changes.contains(&Change::EmojiPrefsChanged));
    assert_eq!(driver.core.emoji_prefs().skin_tone, 3);
    // The same again, and anything out of range, change nothing.
    assert!(driver.send(Input::SetSkinTone(3)).is_empty());
    assert!(driver.send(Input::SetSkinTone(6)).is_empty());
    assert!(driver.send(Input::SetSkinTone(255)).is_empty());
    assert_eq!(driver.core.emoji_prefs().skin_tone, 3);
    driver.send(Input::SetSkinTone(0));
    assert_eq!(driver.core.emoji_prefs().skin_tone, 0);
}

#[test]
fn saved_preferences_are_loaded_repaired_and_forgotten_on_sign_out() {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: false,
        emoji_prefs: EmojiPrefs {
            skin_tone: 9,
            recent: vec![emoji(TADA), emoji(THUMBS), emoji(TADA)],
        },
        ..Default::default()
    }));
    assert_eq!(
        driver.core.emoji_prefs(),
        &EmojiPrefs {
            skin_tone: 0,
            recent: vec![emoji(TADA), emoji(THUMBS)]
        }
    );

    let world = World::new();
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        emoji_prefs: EmojiPrefs {
            skin_tone: 2,
            recent: vec![emoji(HEART)],
        },
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    assert_eq!(driver.core.emoji_prefs().skin_tone, 2);
    let out = driver.send(Input::SignOut);
    assert_eq!(driver.core.emoji_prefs(), &EmojiPrefs::default());
    assert!(out.changes.contains(&Change::EmojiPrefsChanged));
}

#[test]
fn reactions_do_not_pass_through_the_event_for_other_kinds() {
    // A sanity check that a non-reaction event for the same message leaves reactions alone.
    let (world, mut driver, theirs) = setup();
    let general = world.channel.id;
    driver.send(world.reactions_event(11, &theirs, &[(THUMBS, &[&world.other])]));
    driver.send(world.event_at(
        12,
        Event::MemberJoined {
            user: world.other.clone(),
        },
    ));
    assert_eq!(row(&driver, general, theirs.id).reactions.len(), 1);
}
