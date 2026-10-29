//! Threads (slice 2D, D15): summaries and placeholders in the channel, the thread pane, replies,
//! following and read state, permalinks to replies, and holding text for a channel's people.

mod common;

use common::{Driver, World, seq};
use lazy_client_core::{
    Change, Delivery, EditState, Effect, Input, ItemKey, JournalEntry, JumpTarget, Output,
    RequestError, RetryKind, StartupState, ThreadDraft, TimelineItem,
};
use lazy_domain::{ConversationId, EventSeq, MessageId, OperationId, Permalink, Timestamp};
use lazy_protocol::{
    ApiError, ConversationMembersResponse, ErrorCode, Event, HistoryPage, Mentions, Message,
    MessageResponse, OperationStatus, SendMessageResponse, ThreadStateResponse, ThreadSummary,
    ThreadUserState, User,
};

// ----- Helpers -----

struct Fx {
    world: World,
    driver: Driver,
    general: ConversationId,
    /// A root by the other person at position 5, loaded in `#general`.
    root: Message,
}

fn setup() -> Fx {
    let mut world = World::new();
    // Events at or before the bootstrap's position are already reflected in it.
    world.head = seq(6);
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let root = world.message(general, &world.other, 5, "the root");
    driver.load_history(general, vec![root.clone()], false);
    Fx {
        world,
        driver,
        general,
        root,
    }
}

/// Like `setup`, but the channel has older messages than the one page that was loaded.
fn setup_with_older_history() -> Fx {
    let mut world = World::new();
    world.head = seq(6);
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let root = world.message(general, &world.other, 5, "the root");
    driver.load_history(general, vec![root.clone()], true);
    Fx {
        world,
        driver,
        general,
        root,
    }
}

fn summary(count: u32, last: i64, as_of: i64, repliers: &[&User]) -> ThreadSummary {
    ThreadSummary {
        reply_count: count,
        last_reply_seq: seq(last),
        last_reply_at: Timestamp::now(),
        recent_repliers: repliers.iter().map(|user| user.id).collect(),
        as_of: seq(as_of),
    }
}

fn with_summary(message: &Message, summary: Option<ThreadSummary>) -> Message {
    let mut root = message.clone();
    root.thread = summary;
    root
}

/// A reply under `root` written by `author`, only in the thread.
fn reply(world: &World, root: &Message, author: &User, at: i64, text: &str) -> Message {
    let mut reply = world.message(root.conversation_id, author, at, text);
    reply.thread_root = Some(root.id);
    reply
}

fn also_in_channel(mut message: Message) -> Message {
    message.in_channel = true;
    message
}

fn thread_changed(world: &World, at: i64, root: &Message, summary: Option<ThreadSummary>) -> Input {
    world.event_at(
        at,
        Event::ThreadChanged {
            conversation_id: root.conversation_id,
            root_id: root.id,
            summary,
        },
    )
}

fn thread_state(
    root: &Message,
    following: bool,
    last_read: i64,
    unread: u32,
    mentions: u32,
) -> ThreadUserState {
    ThreadUserState {
        root_id: root.id,
        conversation_id: root.conversation_id,
        following,
        last_read: seq(last_read),
        unread_count: unread,
        mention_count: mentions,
    }
}

fn state_event(world: &World, at: i64, state: ThreadUserState) -> Input {
    world.event_at(at, Event::ThreadStateChanged { state })
}

fn mentioning(mut message: Message, users: &[&User]) -> Message {
    message.mentions = Mentions {
        users: users.iter().map(|user| user.id).collect(),
        channel: false,
    };
    message
}

fn rejected(code: ErrorCode) -> RequestError {
    RequestError::Rejected(ApiError {
        code,
        message: format!("{code:?}"),
    })
}

fn lost() -> RequestError {
    RequestError::Unknown("timed out".into())
}

fn open(fx: &mut Fx) -> Output {
    fx.driver.send(Input::OpenThread {
        conversation: fx.general,
        root: fx.root.id,
    })
}

fn replies_generation(driver: &Driver, root: MessageId) -> u64 {
    driver
        .effects_matching(|effect| match effect {
            Effect::FetchReplies {
                root: r,
                generation,
                ..
            } if *r == root => Some(*generation),
            _ => None,
        })
        .pop()
        .expect("replies were requested")
}

fn load_replies(driver: &mut Driver, root: MessageId, messages: Vec<Message>, has_more: bool) {
    let generation = replies_generation(driver, root);
    driver.send(Input::RepliesLoaded {
        root,
        generation,
        result: Ok(HistoryPage { messages, has_more }),
    });
}

/// Opens the thread of the fixture's root and loads `replies` as its newest page.
fn open_with(fx: &mut Fx, replies: Vec<Message>, has_more: bool) {
    open(fx);
    load_replies(&mut fx.driver, fx.root.id, replies, has_more);
}

fn reply_ids(driver: &Driver, root: MessageId) -> Vec<MessageId> {
    driver
        .core
        .thread(root)
        .expect("the thread is open")
        .replies
        .iter()
        .filter_map(TimelineItem::message_id)
        .collect()
}

fn reply_rows(driver: &Driver, root: MessageId) -> Vec<TimelineItem> {
    driver
        .core
        .thread(root)
        .expect("the thread is open")
        .replies
        .to_vec()
}

fn channel_ids(driver: &Driver, conversation: ConversationId) -> Vec<MessageId> {
    driver
        .core
        .timeline(conversation)
        .iter()
        .filter_map(TimelineItem::message_id)
        .collect()
}

fn channel_row(driver: &Driver, conversation: ConversationId, message: MessageId) -> TimelineItem {
    driver
        .core
        .timeline(conversation)
        .iter()
        .find(|item| item.message_id() == Some(message))
        .cloned()
        .expect("the row should be in the channel")
}

fn journaled(out: &Output) -> Vec<JournalEntry> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.clone()),
            _ => None,
        })
        .collect()
}

fn sends(out: &Output) -> Vec<(OperationId, Option<MessageId>, bool)> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::SendMessage {
                operation,
                thread_root,
                also_in_channel,
                ..
            } => Some((*operation, *thread_root, *also_in_channel)),
            _ => None,
        })
        .collect()
}

fn unread(driver: &Driver, conversation: ConversationId) -> u32 {
    driver
        .core
        .user_state(conversation)
        .map_or(0, |state| state.unread_count)
}

/// Sends a reply and makes its journal write durable. Returns the operation and the output of
/// the second step (which carries the send).
fn send_reply(fx: &mut Fx, text: &str, in_channel: bool) -> (OperationId, Output) {
    let out = fx.driver.send(Input::SendReply {
        conversation: fx.general,
        root: fx.root.id,
        text: text.into(),
        also_in_channel: in_channel,
    });
    let operation = journaled(&out)
        .first()
        .expect("the reply is journaled first")
        .operation_id;
    let out = fx.driver.send(Input::SubmissionPersisted { operation });
    (operation, out)
}

fn confirm(fx: &mut Fx, operation: OperationId, message: &Message) {
    fx.driver.send(Input::SendResult {
        operation,
        result: Ok(SendMessageResponse {
            message: message.clone(),
        }),
    });
}

// ----- The channel: summaries -----

#[test]
fn a_page_gives_a_root_its_summary() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let root = world.message(general, &world.other, 5, "root");
    let kept = summary(2, 9, 9, &[&world.me, &world.other]);
    driver.load_history(
        general,
        vec![with_summary(&root, Some(kept.clone()))],
        false,
    );
    assert_eq!(channel_row(&driver, general, root.id).thread, Some(kept));
}

#[test]
fn a_thread_changed_event_replaces_a_summary_and_an_older_version_never_does() {
    let mut fx = setup_with_older_history();
    let general = fx.general;
    let newer = summary(3, 14, 14, &[&fx.world.me]);
    let out = fx
        .driver
        .send(thread_changed(&fx.world, 14, &fx.root, Some(newer.clone())));
    assert_eq!(
        channel_row(&fx.driver, general, fx.root.id).thread,
        Some(newer.clone())
    );
    assert!(
        out.changes
            .iter()
            .any(|c| matches!(c, Change::TimelineItemUpdated { conversation, .. } if *conversation == general)),
        "the channel row must be reported as changed so its summary line is remeasured"
    );
    // A page that was read before the change still describes the thread as it was.
    let stale = with_summary(&fx.root, Some(summary(1, 9, 9, &[&fx.world.me])));
    fx.driver.send(Input::LoadOlder {
        conversation: general,
    });
    fx.driver.load_history(general, vec![stale], false);
    assert_eq!(
        channel_row(&fx.driver, general, fx.root.id).thread,
        Some(newer)
    );
}

#[test]
fn a_summary_that_arrives_with_a_response_or_fetch_counts_by_its_version_too() {
    let mut fx = setup();
    let general = fx.general;
    let newer = summary(2, 12, 12, &[&fx.world.me]);
    fx.driver
        .send(thread_changed(&fx.world, 12, &fx.root, Some(newer.clone())));
    // An older edit payload does not take the summary back.
    let mut edited = fx.world.edited(&fx.root, "the root, edited");
    edited.thread = Some(summary(1, 9, 9, &[&fx.world.me]));
    fx.driver.send(fx.world.edited_event(13, &edited, None));
    assert_eq!(
        channel_row(&fx.driver, general, fx.root.id).thread,
        Some(newer)
    );
    // A newer one does.
    let mut edited_again = fx.world.edited(&edited, "again");
    let newest = summary(4, 15, 15, &[&fx.world.me]);
    edited_again.thread = Some(newest.clone());
    fx.driver
        .send(fx.world.edited_event(16, &edited_again, None));
    assert_eq!(
        channel_row(&fx.driver, general, fx.root.id).thread,
        Some(newest)
    );
}

#[test]
fn clearing_a_summary_keeps_the_live_root_and_is_not_undone_by_an_older_page() {
    let mut fx = setup_with_older_history();
    let general = fx.general;
    let first = summary(1, 9, 9, &[&fx.world.me]);
    fx.driver
        .send(thread_changed(&fx.world, 9, &fx.root, Some(first.clone())));
    fx.driver
        .send(thread_changed(&fx.world, 12, &fx.root, None));
    assert!(
        channel_row(&fx.driver, general, fx.root.id)
            .thread
            .is_none()
    );
    // The page was read while the reply still existed.
    fx.driver.send(Input::LoadOlder {
        conversation: general,
    });
    fx.driver
        .load_history(general, vec![with_summary(&fx.root, Some(first))], false);
    assert!(
        channel_row(&fx.driver, general, fx.root.id)
            .thread
            .is_none()
    );
}

// ----- The channel: replies -----

#[test]
fn a_reply_only_in_its_thread_is_not_in_the_channel_and_does_not_count_there() {
    let mut fx = setup();
    let general = fx.general;
    let before = unread(&fx.driver, general);
    let r = reply(&fx.world, &fx.root, &fx.world.other, 12, "in the thread");
    fx.driver.send(fx.world.event(r.clone(), None));
    fx.driver.send(thread_changed(
        &fx.world,
        13,
        &fx.root,
        Some(summary(1, 12, 13, &[&fx.world.other])),
    ));
    assert_eq!(channel_ids(&fx.driver, general), vec![fx.root.id]);
    assert_eq!(unread(&fx.driver, general), before);
    assert_eq!(
        channel_row(&fx.driver, general, fx.root.id)
            .thread
            .map(|s| s.reply_count),
        Some(1)
    );
}

#[test]
fn a_reply_also_sent_to_the_channel_is_a_channel_row_and_counts_there() {
    let mut fx = setup();
    let general = fx.general;
    let before = unread(&fx.driver, general);
    let r = also_in_channel(reply(
        &fx.world,
        &fx.root,
        &fx.world.other,
        12,
        "to everyone",
    ));
    fx.driver.send(fx.world.event(r.clone(), None));
    assert_eq!(channel_ids(&fx.driver, general), vec![fx.root.id, r.id]);
    let row = channel_row(&fx.driver, general, r.id);
    assert_eq!(row.thread_root, Some(fx.root.id));
    assert!(row.in_channel);
    assert_eq!(unread(&fx.driver, general), before + 1);
}

#[test]
fn deleting_a_reply_that_is_also_in_the_channel_takes_it_off_the_channel() {
    let mut fx = setup();
    let general = fx.general;
    let r = also_in_channel(reply(
        &fx.world,
        &fx.root,
        &fx.world.other,
        12,
        "to everyone",
    ));
    fx.driver.send(fx.world.event(r.clone(), None));
    assert_eq!(unread(&fx.driver, general), 1);
    fx.driver
        .send(fx.world.deleted_event(13, &fx.world.tombstone(&r)));
    assert_eq!(channel_ids(&fx.driver, general), vec![fx.root.id]);
    assert_eq!(unread(&fx.driver, general), 0);
}

// ----- The channel: deleted roots -----

#[test]
fn a_deleted_root_with_replies_stays_as_a_placeholder_with_its_summary() {
    let mut fx = setup();
    let general = fx.general;
    let kept = summary(2, 9, 9, &[&fx.world.other]);
    fx.driver
        .send(thread_changed(&fx.world, 9, &fx.root, Some(kept.clone())));
    let mut tombstone = fx.world.tombstone(&fx.root);
    tombstone.thread = Some(kept.clone());
    fx.driver.send(fx.world.deleted_event(10, &tombstone));
    let row = channel_row(&fx.driver, general, fx.root.id);
    assert!(row.deleted);
    assert_eq!(row.thread, Some(kept));
    assert_eq!(row.document, lazy_domain::message::deleted_placeholder());
}

#[test]
fn a_root_deleted_without_replies_is_removed_as_before() {
    let mut fx = setup();
    let general = fx.general;
    fx.driver
        .send(fx.world.deleted_event(10, &fx.world.tombstone(&fx.root)));
    assert!(channel_ids(&fx.driver, general).is_empty());
}

#[test]
fn what_the_row_knows_decides_when_the_deleted_message_lacks_a_summary() {
    let mut fx = setup();
    let general = fx.general;
    let kept = summary(1, 9, 9, &[&fx.world.other]);
    fx.driver
        .send(thread_changed(&fx.world, 9, &fx.root, Some(kept.clone())));
    // The tombstone as an old replay serves it, without the summary.
    fx.driver
        .send(fx.world.deleted_event(10, &fx.world.tombstone(&fx.root)));
    let row = channel_row(&fx.driver, general, fx.root.id);
    assert!(row.deleted);
    assert_eq!(row.thread, Some(kept));
}

#[test]
fn the_last_reply_of_a_deleted_root_going_removes_the_placeholder() {
    let mut fx = setup();
    let general = fx.general;
    let kept = summary(1, 9, 9, &[&fx.world.other]);
    fx.driver
        .send(thread_changed(&fx.world, 9, &fx.root, Some(kept.clone())));
    let mut tombstone = fx.world.tombstone(&fx.root);
    tombstone.thread = Some(kept);
    fx.driver.send(fx.world.deleted_event(10, &tombstone));
    assert_eq!(channel_ids(&fx.driver, general), vec![fx.root.id]);
    let out = fx
        .driver
        .send(thread_changed(&fx.world, 12, &fx.root, None));
    assert!(channel_ids(&fx.driver, general).is_empty());
    assert!(
        out.changes
            .iter()
            .any(|c| matches!(c, Change::TimelineSplice { removed: 1, .. }))
    );
}

#[test]
fn a_placeholder_does_not_count_as_unread_and_is_not_a_divider() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let mut placeholder = world.tombstone(&world.message(general, &world.other, 5, "gone"));
    placeholder.thread = Some(summary(1, 9, 9, &[&world.other]));
    let live = world.message(general, &world.other, 6, "live");
    driver.load_history(general, vec![placeholder, live.clone()], false);
    let rows = driver.core.timeline(general);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].deleted && !rows[1].deleted);
}

#[test]
fn a_page_in_flight_cannot_bring_back_a_placeholder_whose_last_reply_went() {
    let mut fx = setup_with_older_history();
    let general = fx.general;
    // An older root, not loaded yet, is on the page that is about to arrive.
    let older = fx.world.message(general, &fx.world.other, 2, "older root");
    let mut placeholder = fx.world.tombstone(&older);
    placeholder.thread = Some(summary(1, 3, 3, &[&fx.world.other]));
    fx.driver.send(Input::LoadOlder {
        conversation: general,
    });
    fx.driver.send(thread_changed(&fx.world, 11, &older, None));
    fx.driver.load_history(general, vec![placeholder], false);
    assert_eq!(channel_ids(&fx.driver, general), vec![fx.root.id]);
}

#[test]
fn a_deleted_root_that_is_not_loaded_but_has_replies_is_added_as_a_placeholder() {
    let mut fx = setup();
    let general = fx.general;
    let other_root = fx.world.message(general, &fx.world.other, 7, "second root");
    // The creation was never seen (a replay served it as a tombstone), the deletion keeps it.
    let mut tombstone = fx.world.tombstone(&other_root);
    tombstone.thread = Some(summary(2, 11, 11, &[&fx.world.other]));
    fx.driver.send(fx.world.deleted_event(12, &tombstone));
    let ids = channel_ids(&fx.driver, general);
    assert_eq!(ids, vec![fx.root.id, other_root.id]);
    assert!(channel_row(&fx.driver, general, other_root.id).deleted);
}

// ----- The pane -----

#[test]
fn opening_a_thread_shows_the_root_at_once_and_fetches_the_newest_replies() {
    let mut fx = setup();
    let out = open(&mut fx);
    assert_eq!(
        fx.driver
            .core
            .open_thread()
            .map(|t| (t.conversation, t.root)),
        Some((fx.general, fx.root.id))
    );
    assert!(out.changes.contains(&Change::OpenThreadChanged));
    let view = fx.driver.core.thread(fx.root.id).unwrap();
    assert_eq!(
        view.root.and_then(TimelineItem::message_id),
        Some(fx.root.id)
    );
    assert!(view.replies.is_empty());
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::FetchReplies { root, before: None, .. } if *root == fx.root.id
    )));
    // The root was in the channel, so nothing needs fetching.
    assert!(
        !out.effects
            .iter()
            .any(|e| matches!(e, Effect::FetchMessage { .. }))
    );
    // The channel's reading position is untouched.
    assert!(fx.driver.mark_reads().is_empty());
}

#[test]
fn a_root_the_channel_does_not_hold_is_fetched() {
    let mut fx = setup();
    let general = fx.general;
    let far = fx.world.message(general, &fx.world.other, 1, "far back");
    let out = fx.driver.send(Input::OpenThread {
        conversation: general,
        root: far.id,
    });
    assert!(
        out.effects
            .contains(&Effect::FetchMessage { message: far.id })
    );
    assert!(fx.driver.core.thread(far.id).unwrap().root.is_none());
    let out = fx.driver.send(Input::MessageFetched {
        message: far.id,
        result: Ok(MessageResponse {
            message: far.clone(),
        }),
    });
    assert!(
        out.changes
            .contains(&Change::ThreadRootChanged { root: far.id })
    );
    assert_eq!(
        fx.driver
            .core
            .thread(far.id)
            .unwrap()
            .root
            .and_then(TimelineItem::message_id),
        Some(far.id)
    );
}

#[test]
fn a_page_of_replies_is_shown_oldest_first_and_older_ones_are_prepended() {
    let mut fx = setup();
    let a = reply(&fx.world, &fx.root, &fx.world.other, 20, "second");
    let b = reply(&fx.world, &fx.root, &fx.world.me, 21, "third");
    let older = reply(&fx.world, &fx.root, &fx.world.other, 15, "first");
    open_with(&mut fx, vec![a.clone(), b.clone()], true);
    assert_eq!(reply_ids(&fx.driver, fx.root.id), vec![a.id, b.id]);
    let out = fx.driver.send(Input::LoadOlderReplies { root: fx.root.id });
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::FetchReplies { before: Some(b), .. } if *b == seq(20)
    )));
    load_replies(&mut fx.driver, fx.root.id, vec![older.clone()], false);
    assert_eq!(
        reply_ids(&fx.driver, fx.root.id),
        vec![older.id, a.id, b.id]
    );
    // The root is never among the replies.
    assert!(!reply_ids(&fx.driver, fx.root.id).contains(&fx.root.id));
}

#[test]
fn an_answer_to_an_old_replies_request_is_ignored() {
    let mut fx = setup();
    open(&mut fx);
    let first = replies_generation(&fx.driver, fx.root.id);
    fx.driver.send(Input::CloseThread);
    open(&mut fx);
    let late = reply(&fx.world, &fx.root, &fx.world.other, 20, "late");
    fx.driver.send(Input::RepliesLoaded {
        root: fx.root.id,
        generation: first,
        result: Ok(HistoryPage {
            messages: vec![late],
            has_more: false,
        }),
    });
    assert!(reply_ids(&fx.driver, fx.root.id).is_empty());
}

#[test]
fn a_live_reply_joins_the_open_thread_and_an_also_in_channel_reply_joins_the_channel_too() {
    let mut fx = setup();
    let general = fx.general;
    open_with(&mut fx, vec![], false);
    let only = reply(&fx.world, &fx.root, &fx.world.other, 20, "only here");
    let both = also_in_channel(reply(&fx.world, &fx.root, &fx.world.other, 21, "both"));
    let out = fx.driver.send(fx.world.event(only.clone(), None));
    assert!(
        out.changes
            .iter()
            .any(|c| matches!(c, Change::ThreadSplice { inserted: 1, .. }))
    );
    fx.driver.send(fx.world.event(both.clone(), None));
    assert_eq!(reply_ids(&fx.driver, fx.root.id), vec![only.id, both.id]);
    assert_eq!(channel_ids(&fx.driver, general), vec![fx.root.id, both.id]);
}

#[test]
fn a_reply_for_a_thread_that_is_not_open_is_not_held() {
    let mut fx = setup();
    let r = reply(&fx.world, &fx.root, &fx.world.other, 20, "elsewhere");
    fx.driver.send(fx.world.event(r.clone(), None));
    open_with(&mut fx, vec![r.clone()], false);
    assert_eq!(reply_ids(&fx.driver, fx.root.id), vec![r.id]);
}

#[test]
fn an_edit_a_reaction_and_a_deletion_reach_every_copy_of_a_reply() {
    let mut fx = setup();
    let general = fx.general;
    let r = also_in_channel(reply(&fx.world, &fx.root, &fx.world.other, 20, "hello"));
    fx.driver.send(fx.world.event(r.clone(), None));
    open_with(&mut fx, vec![r.clone()], false);
    let edited = fx.world.edited(&r, "hello again");
    fx.driver.send(fx.world.edited_event(21, &edited, None));
    let in_pane = reply_rows(&fx.driver, fx.root.id);
    assert_eq!(in_pane[0].document, edited.document);
    assert_eq!(
        channel_row(&fx.driver, general, r.id).document,
        edited.document
    );

    fx.driver.send(
        fx.world
            .reactions_event(22, &edited, &[("👍", &[&fx.world.me])]),
    );
    assert_eq!(reply_rows(&fx.driver, fx.root.id)[0].reactions.len(), 1);
    assert_eq!(channel_row(&fx.driver, general, r.id).reactions.len(), 1);

    fx.driver
        .send(fx.world.deleted_event(23, &fx.world.tombstone(&edited)));
    assert!(reply_ids(&fx.driver, fx.root.id).is_empty());
    assert_eq!(channel_ids(&fx.driver, general), vec![fx.root.id]);
}

#[test]
fn a_late_page_cannot_bring_back_a_deleted_reply() {
    let mut fx = setup();
    let r = reply(&fx.world, &fx.root, &fx.world.other, 20, "doomed");
    let keep = reply(&fx.world, &fx.root, &fx.world.other, 21, "kept");
    open(&mut fx);
    // The page was read before the deletion and arrives after it.
    fx.driver
        .send(fx.world.deleted_event(22, &fx.world.tombstone(&r)));
    load_replies(
        &mut fx.driver,
        fx.root.id,
        vec![r.clone(), keep.clone()],
        false,
    );
    assert_eq!(reply_ids(&fx.driver, fx.root.id), vec![keep.id]);
}

#[test]
fn a_late_page_cannot_undo_an_edit_or_a_reaction_on_a_reply() {
    let mut fx = setup();
    let r = reply(&fx.world, &fx.root, &fx.world.other, 20, "first words");
    open(&mut fx);
    let edited = fx.world.edited(&r, "second words");
    fx.driver.send(fx.world.edited_event(22, &edited, None));
    fx.driver.send(
        fx.world
            .reactions_event(23, &edited, &[("🎉", &[&fx.world.other])]),
    );
    load_replies(&mut fx.driver, fx.root.id, vec![r.clone()], false);
    let rows = reply_rows(&fx.driver, fx.root.id);
    assert_eq!(rows[0].document, edited.document);
    assert_eq!(rows[0].reactions.len(), 1);
}

#[test]
fn selecting_another_conversation_closes_the_pane() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let random = fx.world.second_channel.id;
    let out = fx.driver.send(Input::SelectConversation(random));
    assert!(fx.driver.core.open_thread().is_none());
    assert!(fx.driver.core.thread(fx.root.id).is_none());
    assert!(out.changes.contains(&Change::OpenThreadChanged));
}

#[test]
fn opening_a_thread_in_another_conversation_selects_it_first() {
    let mut fx = setup();
    let random = fx.world.second_channel.id;
    let there = fx.world.message(random, &fx.world.other, 6, "elsewhere");
    fx.driver.send(Input::OpenThread {
        conversation: random,
        root: there.id,
    });
    assert_eq!(fx.driver.core.selected_conversation(), Some(random));
    assert_eq!(fx.driver.core.open_thread().map(|t| t.root), Some(there.id));
}

#[test]
fn losing_access_to_the_thread_closes_it_with_a_notice() {
    let mut fx = setup();
    open(&mut fx);
    let generation = replies_generation(&fx.driver, fx.root.id);
    let out = fx.driver.send(Input::RepliesLoaded {
        root: fx.root.id,
        generation,
        result: Err(rejected(ErrorCode::NotFound)),
    });
    assert!(fx.driver.core.open_thread().is_none());
    assert!(fx.driver.core.notice().is_some());
    assert!(out.changes.contains(&Change::OpenThreadChanged));
}

#[test]
fn the_thread_of_a_conversation_the_person_leaves_is_closed_and_forgotten() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    fx.driver.send(state_event(
        &fx.world,
        12,
        thread_state(&fx.root, true, 0, 1, 0),
    ));
    fx.driver.send(Input::SetThreadDraft {
        root: fx.root.id,
        text: "half a reply".into(),
    });
    fx.driver.send(fx.world.event_at(
        13,
        Event::MemberRemoved {
            conversation_id: fx.general,
            user_id: fx.world.me.id,
        },
    ));
    assert!(fx.driver.core.open_thread().is_none());
    assert!(fx.driver.core.thread_state(fx.root.id).is_none());
    assert_eq!(fx.driver.core.thread_draft(fx.root.id), "");
}

#[test]
fn what_can_be_done_with_a_message_works_on_the_pane_root_and_on_replies() {
    let mut fx = setup();
    let general = fx.general;
    let mine = reply(&fx.world, &fx.root, &fx.world.me, 20, "mine");
    open_with(&mut fx, vec![mine.clone()], false);
    assert!(fx.driver.core.can_edit_message(general, mine.id));
    assert!(fx.driver.core.can_delete_message(general, mine.id));
    assert!(fx.driver.core.can_react_message(general, mine.id));
    assert!(fx.driver.core.can_react_message(general, fx.root.id));
    // A reply is not itself replied to; a top-level message is.
    assert!(!fx.driver.core.can_reply_message(general, mine.id));
    assert!(fx.driver.core.can_reply_message(general, fx.root.id));
    assert_eq!(
        fx.driver.core.edit_text(general, mine.id).as_deref(),
        Some("mine")
    );
    let excerpt = fx.driver.core.message_excerpt(general, mine.id).unwrap();
    assert_eq!(
        (excerpt.author, excerpt.text.as_str()),
        (fx.world.me.id, "mine")
    );
}

#[test]
fn editing_reacting_and_deleting_a_reply_in_the_pane_work() {
    let mut fx = setup();
    let general = fx.general;
    let mine = reply(&fx.world, &fx.root, &fx.world.me, 20, "mine");
    open_with(&mut fx, vec![mine.clone()], false);

    let out = fx.driver.send(Input::EditMessage {
        conversation: general,
        message: mine.id,
        text: "mine, better".into(),
    });
    assert!(!journaled(&out).is_empty());
    assert_eq!(
        reply_rows(&fx.driver, fx.root.id)[0].edit,
        EditState::Saving
    );

    let out = fx.driver.send(Input::ToggleReaction {
        conversation: general,
        message: mine.id,
        emoji: lazy_domain::ReactionEmoji::parse("👍").unwrap(),
    });
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::SetReaction { .. }))
    );
    assert!(fx.driver.core.reacted_by_me(
        general,
        mine.id,
        lazy_domain::ReactionEmoji::parse("👍").unwrap()
    ));

    let out = fx.driver.send(Input::DeleteMessage {
        conversation: general,
        message: mine.id,
    });
    assert!(
        out.effects
            .contains(&Effect::DeleteMessage { message: mine.id })
    );
}

// ----- Replying -----

#[test]
fn a_reply_is_an_optimistic_row_journaled_with_its_thread_and_sent_with_it() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let out = fx.driver.send(Input::SendReply {
        conversation: fx.general,
        root: fx.root.id,
        text: "an answer".into(),
        also_in_channel: true,
    });
    let entries = journaled(&out);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].thread_root, Some(fx.root.id));
    assert!(entries[0].also_in_channel);
    assert!(
        sends(&out).is_empty(),
        "nothing is sent before the journal write is durable"
    );
    let rows = reply_rows(&fx.driver, fx.root.id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].delivery, Delivery::Pending);
    assert_eq!(rows[0].thread_root, Some(fx.root.id));
    assert!(rows[0].in_channel);
    // Not in the channel until the server says so.
    assert_eq!(channel_ids(&fx.driver, fx.general), vec![fx.root.id]);

    let operation = entries[0].operation_id;
    let out = fx.driver.send(Input::SubmissionPersisted { operation });
    assert_eq!(sends(&out), vec![(operation, Some(fx.root.id), true)]);
}

#[test]
fn a_confirmed_reply_takes_its_place_and_an_also_in_channel_one_appears_in_the_channel() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let (operation, _) = send_reply(&mut fx, "an answer", true);
    let mut committed = reply(&fx.world, &fx.root, &fx.world.me, 20, "an answer");
    committed.in_channel = true;
    confirm(&mut fx, operation, &committed);
    let rows = reply_rows(&fx.driver, fx.root.id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].message_id(), Some(committed.id));
    assert_eq!(rows[0].key, ItemKey::Operation(operation));
    assert_eq!(
        channel_ids(&fx.driver, fx.general),
        vec![fx.root.id, committed.id]
    );
    assert_eq!(fx.driver.core.unsettled_operations(), 0);
    // The event that follows changes nothing.
    fx.driver
        .send(fx.world.event(committed.clone(), Some(operation)));
    assert_eq!(reply_ids(&fx.driver, fx.root.id), vec![committed.id]);
    assert_eq!(
        channel_ids(&fx.driver, fx.general),
        vec![fx.root.id, committed.id]
    );
}

#[test]
fn a_reply_that_is_only_in_its_thread_never_reaches_the_channel() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let (operation, _) = send_reply(&mut fx, "quietly", false);
    let committed = reply(&fx.world, &fx.root, &fx.world.me, 20, "quietly");
    confirm(&mut fx, operation, &committed);
    assert_eq!(channel_ids(&fx.driver, fx.general), vec![fx.root.id]);
    assert_eq!(unread(&fx.driver, fx.general), 0);
}

#[test]
fn a_reply_confirmed_by_the_event_before_the_response_is_not_doubled() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let (operation, _) = send_reply(&mut fx, "raced", false);
    let committed = reply(&fx.world, &fx.root, &fx.world.me, 20, "raced");
    fx.driver
        .send(fx.world.event(committed.clone(), Some(operation)));
    confirm(&mut fx, operation, &committed);
    assert_eq!(reply_ids(&fx.driver, fx.root.id), vec![committed.id]);
    assert_eq!(fx.driver.core.unsettled_operations(), 0);
}

#[test]
fn replies_go_out_one_at_a_time_in_order_with_the_channels_own_messages() {
    let mut fx = setup();
    let general = fx.general;
    open_with(&mut fx, vec![], false);
    let (first, out) = send_reply(&mut fx, "one", false);
    assert_eq!(sends(&out).len(), 1);
    let out = fx.driver.send(Input::SendMessage {
        conversation: general,
        text: "channel words".into(),
    });
    let second = journaled(&out)[0].operation_id;
    let out = fx
        .driver
        .send(Input::SubmissionPersisted { operation: second });
    assert!(sends(&out).is_empty(), "the second waits behind the first");
    let committed = reply(&fx.world, &fx.root, &fx.world.me, 20, "one");
    fx.driver.send(Input::SendResult {
        operation: first,
        result: Ok(SendMessageResponse { message: committed }),
    });
    assert_eq!(fx.driver.core.unsettled_operations(), 1);
}

#[test]
fn a_reply_with_an_unknown_outcome_is_reconciled_through_the_operation_status() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let (operation, _) = send_reply(&mut fx, "did it land", true);
    let out = fx.driver.send(Input::SendResult {
        operation,
        result: Err(lost()),
    });
    assert!(
        out.effects
            .contains(&Effect::QueryOperationStatus { operation })
    );
    assert_eq!(
        reply_rows(&fx.driver, fx.root.id)[0].delivery,
        Delivery::UnknownOutcome
    );
    let mut committed = reply(&fx.world, &fx.root, &fx.world.me, 20, "did it land");
    committed.in_channel = true;
    fx.driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::Committed {
            message: committed.clone(),
        }),
    });
    assert_eq!(reply_ids(&fx.driver, fx.root.id), vec![committed.id]);
    assert_eq!(
        channel_ids(&fx.driver, fx.general),
        vec![fx.root.id, committed.id]
    );
}

#[test]
fn a_refused_reply_keeps_its_text_and_can_be_tried_again_as_a_new_operation() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let (operation, _) = send_reply(&mut fx, "refused", true);
    fx.driver.send(Input::SendResult {
        operation,
        result: Err(rejected(ErrorCode::NotFound)),
    });
    let rows = reply_rows(&fx.driver, fx.root.id);
    assert!(matches!(
        rows[0].delivery,
        Delivery::Failed {
            retry: RetryKind::NewOperation,
            ..
        }
    ));
    assert_eq!(rows[0].document.plain_text(), "refused");
    let out = fx.driver.send(Input::RetryOperation {
        conversation: fx.general,
        operation,
    });
    let entries = journaled(&out);
    assert_eq!(entries.len(), 1);
    assert_ne!(entries[0].operation_id, operation);
    assert_eq!(entries[0].thread_root, Some(fx.root.id));
    assert!(
        entries[0].also_in_channel,
        "the retry keeps the reply's choices"
    );
    assert_eq!(reply_rows(&fx.driver, fx.root.id).len(), 1);
}

#[test]
fn closing_the_pane_keeps_a_reply_that_is_still_being_sent_and_a_confirmation_finds_it() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let (operation, _) = send_reply(&mut fx, "in flight", true);
    fx.driver.send(Input::CloseThread);
    let mut committed = reply(&fx.world, &fx.root, &fx.world.me, 20, "in flight");
    committed.in_channel = true;
    confirm(&mut fx, operation, &committed);
    // The channel has it; reopening shows it once, from the page.
    assert_eq!(
        channel_ids(&fx.driver, fx.general),
        vec![fx.root.id, committed.id]
    );
    open(&mut fx);
    load_replies(&mut fx.driver, fx.root.id, vec![committed.clone()], false);
    assert_eq!(reply_ids(&fx.driver, fx.root.id), vec![committed.id]);
    assert_eq!(reply_rows(&fx.driver, fx.root.id).len(), 1);
}

#[test]
fn a_reply_that_failed_while_the_pane_was_closed_is_there_when_it_opens_again() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let (operation, _) = send_reply(&mut fx, "will fail", false);
    fx.driver.send(Input::CloseThread);
    fx.driver.send(Input::SendResult {
        operation,
        result: Err(rejected(ErrorCode::Forbidden)),
    });
    open(&mut fx);
    let rows = reply_rows(&fx.driver, fx.root.id);
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].delivery, Delivery::Failed { .. }));
    assert_eq!(rows[0].document.plain_text(), "will fail");
}

#[test]
fn a_reply_to_a_deleted_or_read_only_conversation_is_refused_with_a_notice() {
    let mut fx = setup();
    let mut tombstone = fx.world.tombstone(&fx.root);
    tombstone.thread = Some(summary(1, 9, 9, &[&fx.world.other]));
    fx.driver.send(fx.world.deleted_event(10, &tombstone));
    let out = fx.driver.send(Input::SendReply {
        conversation: fx.general,
        root: fx.root.id,
        text: "too late".into(),
        also_in_channel: false,
    });
    assert!(journaled(&out).is_empty());
    assert!(fx.driver.core.notice().is_some());
    assert!(!fx.driver.core.can_reply_message(fx.general, fx.root.id));
}

#[test]
fn a_reply_to_a_reply_goes_to_the_same_root() {
    let mut fx = setup();
    let first = reply(&fx.world, &fx.root, &fx.world.other, 20, "first");
    open_with(&mut fx, vec![first.clone()], false);
    let out = fx.driver.send(Input::SendReply {
        conversation: fx.general,
        root: first.id,
        text: "to the reply".into(),
        also_in_channel: false,
    });
    assert_eq!(journaled(&out)[0].thread_root, Some(fx.root.id));
}

#[test]
fn a_journaled_reply_from_before_a_restart_comes_back_as_a_reply() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let (operation, _) = send_reply(&mut fx, "survives", true);
    let entry = JournalEntry {
        operation_id: operation,
        user_id: fx.world.me.id,
        conversation_id: fx.general,
        document: lazy_domain::MessageDocument::from_markdown("survives").unwrap(),
        submitted_at: Timestamp::now(),
        kind: lazy_client_core::JournalKind::Send,
        thread_root: Some(fx.root.id),
        also_in_channel: true,
        attachments: Vec::new(),
    };
    let mut restarted = Driver::new();
    restarted.send(Input::Started(StartupState {
        session_restored: true,
        journal: vec![entry],
        ..Default::default()
    }));
    let out = restarted.send(Input::BootstrapLoaded(Box::new(fx.world.bootstrap())));
    assert!(
        out.effects
            .contains(&Effect::QueryOperationStatus { operation })
            || restarted
                .effects
                .contains(&Effect::QueryOperationStatus { operation })
    );
    // The server has no record: the same operation is resent as a reply.
    let out = restarted.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    assert_eq!(sends(&out), vec![(operation, Some(fx.root.id), true)]);
    // Opening the thread shows the reply being sent.
    restarted.send(Input::OpenThread {
        conversation: fx.general,
        root: fx.root.id,
    });
    assert_eq!(reply_rows(&restarted, fx.root.id).len(), 1);
}

#[test]
fn a_journal_entry_from_before_threads_is_a_send_to_the_channel() {
    // A send to the channel is written exactly as it was before threads, so a file written by
    // the previous version has neither field.
    let entry = JournalEntry {
        operation_id: OperationId::generate(),
        user_id: lazy_domain::UserId::generate(),
        conversation_id: ConversationId::generate(),
        document: lazy_domain::MessageDocument::from_markdown("hi").unwrap(),
        submitted_at: Timestamp::now(),
        kind: lazy_client_core::JournalKind::Send,
        thread_root: None,
        also_in_channel: false,
        attachments: Vec::new(),
    };
    let previous = serde_json::to_value(&entry).unwrap();
    assert!(previous.get("thread_root").is_none());
    assert!(previous.get("also_in_channel").is_none());
    assert!(previous.get("kind").is_none());
    let read: JournalEntry = serde_json::from_value(previous).expect("the previous shape reads");
    assert_eq!(read, entry);
}

// ----- Drafts -----

#[test]
fn a_reply_draft_is_saved_per_thread_and_cleared_by_sending() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let out = fx.driver.send(Input::SetThreadDraft {
        root: fx.root.id,
        text: "half".into(),
    });
    assert!(
        out.changes
            .contains(&Change::ThreadDraftChanged { root: fx.root.id })
    );
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::PersistThreadDrafts(drafts) if drafts == &vec![ThreadDraft {
            root: fx.root.id,
            conversation: fx.general,
            text: "half".into(),
            selection: None,
        }]
    )));
    assert_eq!(fx.driver.core.thread(fx.root.id).unwrap().draft, "half");
    let out = fx.driver.send(Input::SendReply {
        conversation: fx.general,
        root: fx.root.id,
        text: "half".into(),
        also_in_channel: false,
    });
    assert_eq!(fx.driver.core.thread_draft(fx.root.id), "");
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::PersistThreadDrafts(drafts) if drafts.is_empty()
    )));
}

#[test]
fn reply_drafts_come_back_after_a_restart() {
    let fx = setup();
    let mut restarted = Driver::new();
    restarted.send(Input::Started(StartupState {
        session_restored: true,
        thread_drafts: vec![ThreadDraft {
            root: fx.root.id,
            conversation: fx.general,
            text: "saved".into(),
            selection: None,
        }],
        ..Default::default()
    }));
    assert_eq!(restarted.core.thread_draft(fx.root.id), "saved");
}

// ----- Read state: counting -----

/// A world where the person follows the fixture's root with the given baseline. The bootstrap is
/// current to position `head`.
fn setup_following(head: i64, last_read: i64, unread: u32, mentions: u32) -> Fx {
    let mut world = World::new();
    world.head = seq(head);
    let general = world.channel.id;
    let root = world.message(general, &world.other, 5, "the root");
    world.threads = vec![thread_state(&root, true, last_read, unread, mentions)];
    let mut driver = Driver::ready(&world);
    driver.load_history(general, vec![root.clone()], false);
    Fx {
        world,
        driver,
        general,
        root,
    }
}

fn held_state(fx: &Fx) -> ThreadUserState {
    fx.driver
        .core
        .thread_state(fx.root.id)
        .cloned()
        .expect("the client holds a state for the thread")
}

fn counts(fx: &Fx) -> (u32, u32) {
    let state = held_state(fx);
    (state.unread_count, state.mention_count)
}

fn deleted_with(world: &World, at: i64, message: &Message, had: &[&User]) -> Input {
    world.event_at(
        at,
        Event::MessageDeleted {
            message: world.tombstone(message),
            had_mentions: Mentions {
                users: had.iter().map(|user| user.id).collect(),
                channel: false,
            },
        },
    )
}

fn edited_with(world: &World, at: i64, now: &Message, previous: &[&User]) -> Input {
    world.event_at(
        at,
        Event::MessageEdited {
            message: now.clone(),
            operation_id: None,
            previous_mentions: Mentions {
                users: previous.iter().map(|user| user.id).collect(),
                channel: false,
            },
        },
    )
}

#[test]
fn the_bootstrap_seeds_thread_states_and_an_event_replaces_one() {
    let mut fx = setup_following(6, 0, 2, 1);
    assert_eq!(counts(&fx), (2, 1));
    assert_eq!(fx.driver.core.followed_thread_unread_total(), 2);
    let out = fx.driver.send(state_event(
        &fx.world,
        12,
        thread_state(&fx.root, false, 9, 0, 0),
    ));
    assert!(
        out.changes
            .contains(&Change::ThreadStateChanged { root: fx.root.id })
    );
    assert_eq!(counts(&fx), (0, 0));
    assert_eq!(held_state(&fx).last_read, seq(9));
    assert!(!held_state(&fx).following);
    assert_eq!(fx.driver.core.followed_thread_unread_total(), 0);
}

#[test]
fn only_followed_threads_add_to_the_total() {
    let mut fx = setup_following(6, 0, 2, 0);
    let other_root = fx.world.message(fx.general, &fx.world.other, 4, "another");
    fx.driver.send(state_event(
        &fx.world,
        12,
        thread_state(&other_root, false, 0, 7, 0),
    ));
    assert_eq!(fx.driver.core.followed_thread_unread_total(), 2);
}

#[test]
fn a_reply_from_someone_else_counts_and_so_does_a_mention() {
    let mut fx = setup_following(6, 0, 0, 0);
    let plain = reply(&fx.world, &fx.root, &fx.world.other, 12, "plain");
    fx.driver.send(fx.world.event(plain, None));
    assert_eq!(counts(&fx), (1, 0));
    let aimed = mentioning(
        reply(&fx.world, &fx.root, &fx.world.other, 13, "for you"),
        &[&fx.world.me],
    );
    fx.driver.send(fx.world.event(aimed, None));
    assert_eq!(counts(&fx), (2, 1));
}

#[test]
fn a_reply_of_my_own_reads_the_thread() {
    let mut fx = setup_following(6, 0, 2, 1);
    let mine = reply(&fx.world, &fx.root, &fx.world.me, 12, "mine");
    fx.driver.send(fx.world.event(mine, None));
    assert_eq!(counts(&fx), (0, 0));
    assert_eq!(held_state(&fx).last_read, seq(12));
}

#[test]
fn a_reply_at_or_before_the_read_position_is_not_unread() {
    let mut fx = setup_following(6, 15, 0, 0);
    let early = reply(&fx.world, &fx.root, &fx.world.other, 12, "already read");
    fx.driver.send(fx.world.event(early, None));
    assert_eq!(counts(&fx), (0, 0));
}

#[test]
fn a_reply_to_a_thread_without_a_held_state_changes_nothing() {
    let mut fx = setup();
    let r = reply(&fx.world, &fx.root, &fx.world.other, 12, "unheld");
    fx.driver.send(fx.world.event(r, None));
    assert!(fx.driver.core.thread_state(fx.root.id).is_none());
    assert_eq!(fx.driver.core.followed_thread_unread_total(), 0);
}

#[test]
fn a_reply_also_in_the_channel_counts_in_both_places() {
    let mut fx = setup_following(6, 0, 0, 0);
    let both = also_in_channel(reply(&fx.world, &fx.root, &fx.world.other, 12, "both"));
    fx.driver.send(fx.world.event(both, None));
    assert_eq!(counts(&fx), (1, 0));
    assert_eq!(unread(&fx.driver, fx.general), 1);
}

#[test]
fn a_replayed_tombstone_never_counts_and_its_deletion_subtracts_nothing() {
    let mut fx = setup_following(6, 0, 0, 0);
    let gone = reply(&fx.world, &fx.root, &fx.world.other, 12, "gone");
    fx.driver
        .send(fx.world.event(fx.world.tombstone(&gone), None));
    assert_eq!(counts(&fx), (0, 0));
    fx.driver.send(deleted_with(&fx.world, 13, &gone, &[]));
    assert_eq!(counts(&fx), (0, 0));
}

#[test]
fn a_deletion_stops_a_counted_reply_counting_with_its_mention() {
    let mut fx = setup_following(6, 0, 0, 0);
    let aimed = mentioning(
        reply(&fx.world, &fx.root, &fx.world.other, 12, "for you"),
        &[&fx.world.me],
    );
    fx.driver.send(fx.world.event(aimed.clone(), None));
    assert_eq!(counts(&fx), (1, 1));
    fx.driver
        .send(deleted_with(&fx.world, 13, &aimed, &[&fx.world.me]));
    assert_eq!(counts(&fx), (0, 0));
}

#[test]
fn a_reply_the_baseline_counted_is_subtracted_with_the_mentions_it_had() {
    let mut fx = setup_following(6, 0, 1, 1);
    let baseline = reply(&fx.world, &fx.root, &fx.world.other, 4, "was counted");
    fx.driver
        .send(deleted_with(&fx.world, 12, &baseline, &[&fx.world.me]));
    assert_eq!(counts(&fx), (0, 0));
}

#[test]
fn an_edit_that_adds_or_removes_the_person_moves_the_mention_count() {
    let mut fx = setup_following(6, 0, 0, 0);
    let original = reply(&fx.world, &fx.root, &fx.world.other, 12, "hello");
    fx.driver.send(fx.world.event(original.clone(), None));
    assert_eq!(counts(&fx), (1, 0));
    let aimed = mentioning(fx.world.edited(&original, "hello you"), &[&fx.world.me]);
    fx.driver.send(edited_with(&fx.world, 13, &aimed, &[]));
    assert_eq!(counts(&fx), (1, 1));
    let mut plain = fx.world.edited(&aimed, "hello");
    plain.mentions = Mentions::default();
    fx.driver
        .send(edited_with(&fx.world, 14, &plain, &[&fx.world.me]));
    assert_eq!(counts(&fx), (1, 0));
}

// ----- Read state: marking read -----

/// The newest page of two replies by the other person, positions 20 and 21, with a baseline that
/// counts both.
fn unread_thread() -> (Fx, Message, Message) {
    let mut fx = setup_following(21, 0, 2, 0);
    let a = reply(&fx.world, &fx.root, &fx.world.other, 20, "one");
    let b = reply(&fx.world, &fx.root, &fx.world.other, 21, "two");
    open_with(&mut fx, vec![a.clone(), b.clone()], false);
    (fx, a, b)
}

fn thread_reads(driver: &Driver) -> Vec<(MessageId, i64)> {
    driver.effects_matching(|effect| match effect {
        Effect::MarkThreadRead { root, up_to } => Some((*root, up_to.get())),
        _ => None,
    })
}

fn read_answer(
    root: &Message,
    last_read: i64,
    unread: u32,
) -> Result<ThreadStateResponse, RequestError> {
    Ok(ThreadStateResponse {
        state: thread_state(root, true, last_read, unread, 0),
    })
}

#[test]
fn an_open_thread_at_the_bottom_marks_its_newest_reply_read_once() {
    let (mut fx, _, _) = unread_thread();
    assert!(
        thread_reads(&fx.driver).is_empty(),
        "not until the view says it is at the bottom"
    );
    fx.driver.send(Input::ThreadAtBottom {
        root: fx.root.id,
        at_bottom: true,
    });
    assert_eq!(thread_reads(&fx.driver), vec![(fx.root.id, 21)]);
    // One request at a time.
    fx.driver.send(Input::ThreadAtBottom {
        root: fx.root.id,
        at_bottom: true,
    });
    assert_eq!(thread_reads(&fx.driver).len(), 1);
    fx.driver.send(Input::ThreadReadResult {
        root: fx.root.id,
        up_to: seq(21),
        result: read_answer(&fx.root, 21, 0),
    });
    assert_eq!(counts(&fx), (0, 0));
    assert_eq!(held_state(&fx).last_read, seq(21));
    // Nothing more to read, nothing more to send.
    fx.driver.send(Input::WindowActive(true));
    assert_eq!(thread_reads(&fx.driver).len(), 1);
}

#[test]
fn a_thread_is_marked_read_only_while_open_active_and_at_the_bottom() {
    let (mut fx, _, _) = unread_thread();
    fx.driver.send(Input::WindowActive(false));
    fx.driver.send(Input::ThreadAtBottom {
        root: fx.root.id,
        at_bottom: true,
    });
    assert!(thread_reads(&fx.driver).is_empty());
    fx.driver.send(Input::WindowActive(true));
    assert_eq!(thread_reads(&fx.driver).len(), 1);

    let (mut closed, _, _) = unread_thread();
    closed.driver.send(Input::CloseThread);
    closed.driver.send(Input::ThreadAtBottom {
        root: closed.root.id,
        at_bottom: true,
    });
    assert!(thread_reads(&closed.driver).is_empty());
    // And the channel's own read position is a separate matter.
    assert!(closed.driver.mark_reads().is_empty());
}

#[test]
fn a_failed_thread_read_is_not_repeated_for_the_same_position() {
    let (mut fx, _, _) = unread_thread();
    fx.driver.send(Input::ThreadAtBottom {
        root: fx.root.id,
        at_bottom: true,
    });
    fx.driver.send(Input::ThreadReadResult {
        root: fx.root.id,
        up_to: seq(21),
        result: Err(lost()),
    });
    assert_eq!(thread_reads(&fx.driver).len(), 1);
    // Scrolling away and back is something changing.
    fx.driver.send(Input::ThreadAtBottom {
        root: fx.root.id,
        at_bottom: false,
    });
    fx.driver.send(Input::ThreadAtBottom {
        root: fx.root.id,
        at_bottom: true,
    });
    assert_eq!(thread_reads(&fx.driver).len(), 2);
}

#[test]
fn replies_that_arrive_while_a_thread_read_is_in_flight_stay_unread() {
    let (mut fx, _, _) = unread_thread();
    fx.driver.send(Input::ThreadAtBottom {
        root: fx.root.id,
        at_bottom: true,
    });
    let late = reply(&fx.world, &fx.root, &fx.world.other, 22, "late");
    fx.driver.send(fx.world.event(late, None));
    assert_eq!(counts(&fx).0, 3);
    fx.driver.send(Input::ThreadReadResult {
        root: fx.root.id,
        up_to: seq(21),
        result: read_answer(&fx.root, 21, 0),
    });
    assert_eq!(counts(&fx).0, 1);
    // The next request covers it.
    assert_eq!(thread_reads(&fx.driver).last(), Some(&(fx.root.id, 22)));
}

#[test]
fn a_thread_read_answer_from_ahead_of_the_events_is_not_taken_on_an_incomplete_thread() {
    // Two unread replies from the baseline, and the replies are not loaded, so the rows are not
    // the count.
    let mut fx = setup_following(21, 0, 2, 0);
    let a = reply(&fx.world, &fx.root, &fx.world.other, 30, "one");
    let b = reply(&fx.world, &fx.root, &fx.world.other, 31, "two");
    open(&mut fx);
    fx.driver.send(fx.world.event(a, None));
    fx.driver.send(fx.world.event(b.clone(), None));
    assert_eq!(counts(&fx).0, 4);
    // Reading the first live reply. The answer is from later: one of the baseline's replies is
    // already deleted there, so it says only one is unread. The deletion has not arrived.
    fx.driver.send(Input::ThreadReadResult {
        root: fx.root.id,
        up_to: seq(30),
        result: read_answer(&fx.root, 30, 1),
    });
    // The answer's count is not used: what is known to have been read comes off, and the rest
    // waits for the server's state event.
    assert_eq!(counts(&fx).0, 3);
    assert_eq!(held_state(&fx).last_read, seq(30));
    fx.driver.send(state_event(
        &fx.world,
        40,
        thread_state(&fx.root, true, 30, 1, 0),
    ));
    assert_eq!(counts(&fx).0, 1);
}

// ----- Following -----

fn follow_requests(driver: &Driver) -> Vec<(MessageId, bool)> {
    driver.effects_matching(|effect| match effect {
        Effect::SetThreadFollow { root, following } => Some((*root, *following)),
        _ => None,
    })
}

fn follow_answer(root: &Message, following: bool) -> Result<ThreadStateResponse, RequestError> {
    Ok(ThreadStateResponse {
        state: thread_state(root, following, 0, 0, 0),
    })
}

#[test]
fn following_a_thread_shows_at_once_and_is_saved_with_one_request() {
    let mut fx = setup();
    let out = fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: true,
    });
    assert!(held_state(&fx).following);
    assert!(
        out.changes
            .contains(&Change::ThreadStateChanged { root: fx.root.id })
    );
    assert_eq!(follow_requests(&fx.driver), vec![(fx.root.id, true)]);
    fx.driver.send(Input::ThreadFollowResult {
        root: fx.root.id,
        result: follow_answer(&fx.root, true),
    });
    assert!(held_state(&fx).following);
    assert_eq!(follow_requests(&fx.driver).len(), 1);
}

#[test]
fn asking_for_what_is_already_shown_sends_nothing() {
    let mut fx = setup_following(6, 0, 0, 0);
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: true,
    });
    assert!(follow_requests(&fx.driver).is_empty());
}

#[test]
fn quick_changes_end_with_the_final_wish() {
    let mut fx = setup_following(6, 0, 0, 0);
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: false,
    });
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: true,
    });
    assert!(held_state(&fx).following);
    // Only the first went out; the answer to it leaves the final wish equal to the server's
    // state again... which here is not the case, so the final wish is sent next.
    assert_eq!(follow_requests(&fx.driver), vec![(fx.root.id, false)]);
    fx.driver.send(Input::ThreadFollowResult {
        root: fx.root.id,
        result: follow_answer(&fx.root, false),
    });
    assert_eq!(
        follow_requests(&fx.driver),
        vec![(fx.root.id, false), (fx.root.id, true)]
    );
    fx.driver.send(Input::ThreadFollowResult {
        root: fx.root.id,
        result: follow_answer(&fx.root, true),
    });
    assert!(held_state(&fx).following);
}

#[test]
fn a_lost_answer_repeats_the_request_three_times_and_then_gives_up() {
    let mut fx = setup();
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: true,
    });
    for _ in 0..3 {
        fx.driver.send(Input::ThreadFollowResult {
            root: fx.root.id,
            result: Err(lost()),
        });
    }
    assert_eq!(follow_requests(&fx.driver).len(), 4);
    assert!(
        held_state(&fx).following,
        "still shown while it is being tried"
    );
    fx.driver.send(Input::ThreadFollowResult {
        root: fx.root.id,
        result: Err(lost()),
    });
    assert!(
        fx.driver.core.thread_state(fx.root.id).is_none(),
        "the display returned to what the server has"
    );
    assert!(fx.driver.core.notice().is_some());
    assert_eq!(follow_requests(&fx.driver).len(), 4);
}

#[test]
fn a_refused_follow_returns_to_the_servers_state_with_a_notice() {
    let mut fx = setup_following(6, 0, 3, 0);
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: false,
    });
    assert!(!held_state(&fx).following);
    fx.driver.send(Input::ThreadFollowResult {
        root: fx.root.id,
        result: Err(rejected(ErrorCode::Forbidden)),
    });
    assert!(held_state(&fx).following);
    assert_eq!(counts(&fx).0, 3);
    assert!(fx.driver.core.notice().is_some());
}

#[test]
fn a_state_event_that_arrives_while_a_follow_is_pending_does_not_flip_the_display() {
    let mut fx = setup_following(6, 0, 0, 0);
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: false,
    });
    // Another device's read arrives meanwhile, with the old flag.
    fx.driver.send(state_event(
        &fx.world,
        12,
        thread_state(&fx.root, true, 9, 0, 0),
    ));
    assert!(!held_state(&fx).following);
    assert_eq!(held_state(&fx).last_read, seq(9));
    // Giving up shows what the server last said.
    for _ in 0..4 {
        fx.driver.send(Input::ThreadFollowResult {
            root: fx.root.id,
            result: Err(lost()),
        });
    }
    assert!(held_state(&fx).following);
}

#[test]
fn the_cache_records_the_servers_follow_flag_not_an_unsaved_one() {
    let mut fx = setup_following(6, 0, 0, 0);
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: false,
    });
    fx.driver.send(
        fx.world
            .event(reply(&fx.world, &fx.root, &fx.world.other, 12, "x"), None),
    );
    let cache = fx.driver.last_cache().expect("a cache was written").clone();
    let saved = &cache.workspaces[0].threads;
    assert_eq!(saved.len(), 1);
    assert!(saved[0].state.following);
}

// ----- The cache -----

#[test]
fn thread_states_and_summaries_survive_a_restart_with_their_tracking() {
    let mut fx = setup_following(6, 0, 0, 0);
    let kept = summary(2, 12, 13, &[&fx.world.other]);
    let first = reply(&fx.world, &fx.root, &fx.world.other, 12, "counted");
    fx.driver.send(fx.world.event(first.clone(), None));
    fx.driver
        .send(thread_changed(&fx.world, 13, &fx.root, Some(kept.clone())));
    let cache = fx.driver.last_cache().expect("cache").clone();
    let mut restarted = Driver::new();
    restarted.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache),
        ..Default::default()
    }));
    assert_eq!(
        channel_row(&restarted, fx.general, fx.root.id).thread,
        Some(kept)
    );
    let state = restarted.core.thread_state(fx.root.id).cloned().unwrap();
    assert_eq!((state.unread_count, state.mention_count), (1, 0));
    // A new reply counts once; the deletion of the one counted before the restart subtracts it.
    restarted.send(
        fx.world
            .event(reply(&fx.world, &fx.root, &fx.world.other, 14, "new"), None),
    );
    assert_eq!(
        restarted
            .core
            .thread_state(fx.root.id)
            .unwrap()
            .unread_count,
        2
    );
    restarted.send(deleted_with(&fx.world, 15, &first, &[]));
    assert_eq!(
        restarted
            .core
            .thread_state(fx.root.id)
            .unwrap()
            .unread_count,
        1
    );
}

#[test]
fn a_placeholder_root_is_cached_and_comes_back() {
    let mut fx = setup();
    let kept = summary(1, 9, 11, &[&fx.world.other]);
    let mut tombstone = fx.world.tombstone(&fx.root);
    tombstone.thread = Some(kept.clone());
    fx.driver
        .send(thread_changed(&fx.world, 11, &fx.root, Some(kept.clone())));
    fx.driver.send(fx.world.deleted_event(12, &tombstone));
    let cache = fx.driver.last_cache().expect("cache").clone();
    let mut restarted = Driver::new();
    restarted.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache),
        ..Default::default()
    }));
    let row = channel_row(&restarted, fx.general, fx.root.id);
    assert!(row.deleted);
    assert_eq!(row.thread, Some(kept));
}

// ----- Permalinks to replies -----

fn link(fx: &Fx, message: MessageId) -> Permalink {
    Permalink {
        workspace_id: fx.world.workspace.id,
        conversation_id: fx.general,
        message_id: message,
    }
}

fn fetched(message: &Message) -> Input {
    Input::MessageFetched {
        message: message.id,
        result: Ok(MessageResponse {
            message: message.clone(),
        }),
    }
}

#[test]
fn a_permalink_to_a_reply_opens_its_thread_pages_to_it_and_jumps() {
    let mut fx = setup();
    let target = reply(&fx.world, &fx.root, &fx.world.other, 15, "the one");
    let newer = reply(&fx.world, &fx.root, &fx.world.other, 30, "newer");
    let out = fx.driver.send(Input::OpenPermalink(link(&fx, target.id)));
    assert!(
        out.effects
            .contains(&Effect::FetchMessage { message: target.id })
    );
    fx.driver.send(fetched(&target));
    assert_eq!(
        fx.driver.core.open_thread().map(|t| t.root),
        Some(fx.root.id)
    );
    assert!(fx.driver.core.jump_target().is_none());
    // The newest page does not reach it, so the next one is asked for.
    load_replies(&mut fx.driver, fx.root.id, vec![newer.clone()], true);
    assert!(fx.driver.core.jump_target().is_none());
    assert!(
        fx.driver
            .effects
            .iter()
            .any(|e| matches!(e, Effect::FetchReplies { before: Some(b), .. } if *b == seq(30)))
    );
    load_replies(&mut fx.driver, fx.root.id, vec![target.clone()], false);
    assert_eq!(
        fx.driver.core.jump_target(),
        Some(JumpTarget {
            conversation: fx.general,
            key: ItemKey::Message(target.id),
            thread: Some(fx.root.id),
        })
    );
    // The channel behind keeps its position: nothing was marked read there.
    assert!(fx.driver.mark_reads().is_empty());
}

#[test]
fn a_permalink_to_a_reply_in_the_open_thread_needs_no_request() {
    let mut fx = setup();
    let target = reply(&fx.world, &fx.root, &fx.world.other, 15, "here");
    open_with(&mut fx, vec![target.clone()], false);
    let out = fx.driver.send(Input::OpenPermalink(link(&fx, target.id)));
    assert!(
        !out.effects
            .iter()
            .any(|e| matches!(e, Effect::FetchMessage { .. }))
    );
    assert_eq!(
        fx.driver.core.jump_target().map(|t| t.thread),
        Some(Some(fx.root.id))
    );
}

#[test]
fn a_reply_the_channel_lists_still_opens_in_its_thread() {
    let mut fx = setup();
    let shown = also_in_channel(reply(&fx.world, &fx.root, &fx.world.other, 15, "both"));
    fx.driver.send(fx.world.event(shown.clone(), None));
    fx.driver.send(Input::OpenPermalink(link(&fx, shown.id)));
    assert_eq!(
        fx.driver.core.open_thread().map(|t| t.root),
        Some(fx.root.id)
    );
    load_replies(&mut fx.driver, fx.root.id, vec![shown.clone()], false);
    assert_eq!(
        fx.driver.core.jump_target().map(|t| (t.key, t.thread)),
        Some((ItemKey::Message(shown.id), Some(fx.root.id)))
    );
}

#[test]
fn a_reply_that_is_not_among_the_replies_says_so() {
    let mut fx = setup();
    let missing = reply(
        &fx.world,
        &fx.root,
        &fx.world.other,
        15,
        "deleted meanwhile",
    );
    fx.driver.send(Input::OpenPermalink(link(&fx, missing.id)));
    fx.driver.send(fetched(&missing));
    let other = reply(&fx.world, &fx.root, &fx.world.other, 10, "older");
    load_replies(&mut fx.driver, fx.root.id, vec![other], false);
    assert!(fx.driver.core.jump_target().is_none());
    assert!(fx.driver.core.notice().is_some());
}

#[test]
fn the_search_for_a_reply_stops_after_a_bounded_number_of_pages() {
    let mut fx = setup();
    let far = reply(&fx.world, &fx.root, &fx.world.other, 11, "far");
    fx.driver.send(Input::OpenPermalink(link(&fx, far.id)));
    fx.driver.send(fetched(&far));
    let mut at = 10_000;
    let mut requests = 0;
    for _ in 0..(lazy_client_core::PERMALINK_MAX_PAGES + 3) {
        let page = reply(&fx.world, &fx.root, &fx.world.other, at, "filler");
        at -= 1;
        if fx.driver.core.thread(fx.root.id).is_none() {
            break;
        }
        if matches!(
            fx.driver.core.thread(fx.root.id).unwrap().history,
            lazy_client_core::HistoryStatus::Loading
        ) {
            load_replies(&mut fx.driver, fx.root.id, vec![page], true);
            requests += 1;
        }
    }
    assert!(requests <= lazy_client_core::PERMALINK_MAX_PAGES + 1);
    assert!(fx.driver.core.jump_target().is_none());
    assert!(fx.driver.core.notice().is_some());
}

#[test]
fn the_jump_target_goes_when_the_pane_closes() {
    let mut fx = setup();
    let target = reply(&fx.world, &fx.root, &fx.world.other, 15, "here");
    open_with(&mut fx, vec![target.clone()], false);
    fx.driver.send(Input::OpenPermalink(link(&fx, target.id)));
    assert!(fx.driver.core.jump_target().is_some());
    fx.driver.send(Input::CloseThread);
    assert!(fx.driver.core.jump_target().is_none());
}

// ----- Text that may mention someone waits for the channel's people -----

fn members_generation(driver: &Driver, conversation: ConversationId) -> u64 {
    driver
        .effects_matching(|effect| match effect {
            Effect::FetchMembers {
                conversation: c,
                generation,
            } if *c == conversation => Some(*generation),
            _ => None,
        })
        .pop()
        .expect("members were requested")
}

fn members_arrive(driver: &mut Driver, conversation: ConversationId, users: &[&User]) {
    let generation = members_generation(driver, conversation);
    driver.send(Input::MembersFetched {
        conversation,
        generation,
        result: Ok(ConversationMembersResponse {
            conversation_id: conversation,
            user_ids: users.iter().map(|user| user.id).collect(),
        }),
    });
}

fn mentions_in(entry: &JournalEntry) -> usize {
    entry.document.mentions().len()
}

#[test]
fn a_send_that_mentions_waits_for_the_channels_people_instead_of_losing_the_mention() {
    let mut fx = setup();
    let general = fx.general;
    let out = fx.driver.send(Input::SendMessage {
        conversation: general,
        text: "hello @zoe".into(),
    });
    assert!(journaled(&out).is_empty(), "held, not sent as plain text");
    assert!(
        fx.driver
            .core
            .timeline(general)
            .iter()
            .all(|item| item.is_confirmed())
    );
    members_arrive(&mut fx.driver, general, &[&fx.world.me, &fx.world.other]);
    let entries: Vec<JournalEntry> = fx.driver.effects_matching(|e| match e {
        Effect::PersistSubmission(entry) => Some(entry.clone()),
        _ => None,
    });
    assert_eq!(entries.len(), 1);
    assert_eq!(mentions_in(&entries[0]), 1);
}

#[test]
fn a_held_text_that_cannot_learn_the_channels_people_is_sent_as_it_is() {
    let mut fx = setup();
    let general = fx.general;
    fx.driver.send(Input::SendMessage {
        conversation: general,
        text: "hello @zoe".into(),
    });
    let generation = members_generation(&fx.driver, general);
    let out = fx.driver.send(Input::MembersFetched {
        conversation: general,
        generation,
        result: Err(lost()),
    });
    let entries = journaled(&out);
    assert_eq!(entries.len(), 1);
    assert_eq!(mentions_in(&entries[0]), 0);
}

#[test]
fn text_without_an_at_sign_is_never_held() {
    let mut fx = setup();
    let out = fx.driver.send(Input::SendMessage {
        conversation: fx.general,
        text: "no names here".into(),
    });
    assert_eq!(journaled(&out).len(), 1);
}

#[test]
fn held_texts_go_out_in_the_order_they_were_written_with_replies_and_edits_too() {
    let mut fx = setup();
    let general = fx.general;
    let mine = reply(&fx.world, &fx.root, &fx.world.me, 20, "mine");
    open_with(&mut fx, vec![mine.clone()], false);
    fx.driver.send(Input::SendMessage {
        conversation: general,
        text: "first @zoe".into(),
    });
    fx.driver.send(Input::SendReply {
        conversation: general,
        root: fx.root.id,
        text: "second @zoe".into(),
        also_in_channel: false,
    });
    fx.driver.send(Input::EditMessage {
        conversation: general,
        message: mine.id,
        text: "mine, to @zoe".into(),
    });
    let nothing = fx
        .driver
        .effects_matching(|e| matches!(e, Effect::PersistSubmission(_)).then_some(()));
    assert!(nothing.is_empty());
    let generation = members_generation(&fx.driver, general);
    let out = fx.driver.send(Input::MembersFetched {
        conversation: general,
        generation,
        result: Ok(ConversationMembersResponse {
            conversation_id: general,
            user_ids: vec![fx.world.me.id, fx.world.other.id],
        }),
    });
    let entries = journaled(&out);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].thread_root, None);
    assert_eq!(entries[1].thread_root, Some(fx.root.id));
    assert!(matches!(
        entries[2].kind,
        lazy_client_core::JournalKind::Edit { .. }
    ));
    assert!(entries.iter().all(|entry| mentions_in(entry) == 1));
}

#[test]
fn the_draft_stays_while_a_send_waits() {
    let mut fx = setup();
    let general = fx.general;
    fx.driver.send(Input::SetDraft {
        conversation: general,
        text: "hello @zoe".into(),
    });
    fx.driver.send(Input::SendMessage {
        conversation: general,
        text: "hello @zoe".into(),
    });
    assert_eq!(fx.driver.core.draft(general), "hello @zoe");
    members_arrive(&mut fx.driver, general, &[&fx.world.me, &fx.world.other]);
    assert_eq!(fx.driver.core.draft(general), "");
}

// ----- What the real server does, and what the random runs found -----

#[test]
fn a_reply_of_mine_never_moves_the_channels_read_position() {
    let mut fx = setup();
    let general = fx.general;
    fx.driver.send(fx.world.event(
        fx.world.message(general, &fx.world.other, 12, "unread"),
        None,
    ));
    let before = fx.driver.core.user_state(general).cloned().unwrap();
    assert_eq!(before.unread_count, 1);
    // Also sent to the channel, written from a thread pane: still the channel's business only
    // in that others see it. The person's own position stays where it was.
    let mine = also_in_channel(reply(
        &fx.world,
        &fx.root,
        &fx.world.me,
        13,
        "from the pane",
    ));
    fx.driver.send(fx.world.event(mine, None));
    let after = fx.driver.core.user_state(general).cloned().unwrap();
    assert_eq!((after.last_read, after.unread_count), (before.last_read, 1));
}

#[test]
fn a_state_shown_for_a_pending_follow_holds_no_counts_and_goes_when_the_follow_settles_off() {
    let mut fx = setup();
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: true,
    });
    // A reply arrives meanwhile: the shown state is not the server's, so it counts nothing.
    fx.driver.send(
        fx.world
            .event(reply(&fx.world, &fx.root, &fx.world.other, 12, "hi"), None),
    );
    assert_eq!(counts(&fx), (0, 0));
    // The person changes their mind before the answer; the answer says not following.
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: false,
    });
    fx.driver.send(Input::ThreadFollowResult {
        root: fx.root.id,
        result: follow_answer(&fx.root, true),
    });
    fx.driver.send(Input::ThreadFollowResult {
        root: fx.root.id,
        result: follow_answer(&fx.root, false),
    });
    assert!(fx.driver.core.thread_state(fx.root.id).is_none());
}

#[test]
fn a_refused_first_follow_leaves_no_state_even_after_replies_arrived() {
    let mut fx = setup();
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: true,
    });
    fx.driver.send(
        fx.world
            .event(reply(&fx.world, &fx.root, &fx.world.other, 12, "hi"), None),
    );
    fx.driver.send(Input::ThreadFollowResult {
        root: fx.root.id,
        result: Err(rejected(ErrorCode::Forbidden)),
    });
    assert!(fx.driver.core.thread_state(fx.root.id).is_none());
}

#[test]
fn a_follow_that_finds_the_thread_gone_forgets_it() {
    let mut fx = setup_following(6, 0, 1, 0);
    open(&mut fx);
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: false,
    });
    fx.driver.send(Input::ThreadFollowResult {
        root: fx.root.id,
        result: Err(rejected(ErrorCode::NotFound)),
    });
    assert!(fx.driver.core.thread_state(fx.root.id).is_none());
    assert!(fx.driver.core.open_thread().is_none());
}

#[test]
fn the_root_of_an_open_thread_keeps_what_arrives_while_it_is_fetched() {
    let mut fx = setup();
    let general = fx.general;
    let far = fx.world.message(general, &fx.world.other, 1, "far back");
    fx.driver.send(Input::OpenThread {
        conversation: general,
        root: far.id,
    });
    // A reaction arrives before the answer, which was read before it.
    fx.driver.send(
        fx.world
            .reactions_event(12, &far, &[("👍", &[&fx.world.other])]),
    );
    fx.driver.send(Input::MessageFetched {
        message: far.id,
        result: Ok(MessageResponse {
            message: far.clone(),
        }),
    });
    let view = fx.driver.core.thread(far.id).unwrap();
    assert_eq!(view.root.map(|root| root.reactions.len()), Some(1));
}

#[test]
fn a_failed_fetch_of_the_root_is_tried_again_when_the_connection_catches_up() {
    let mut fx = setup();
    let general = fx.general;
    let far = fx.world.message(general, &fx.world.other, 1, "far back");
    fx.driver.send(Input::OpenThread {
        conversation: general,
        root: far.id,
    });
    fx.driver.send(Input::MessageFetched {
        message: far.id,
        result: Err(lost()),
    });
    assert!(fx.driver.core.thread(far.id).unwrap().root.is_none());
    let before = fx
        .driver
        .effects
        .iter()
        .filter(|e| matches!(e, Effect::FetchMessage { message } if *message == far.id))
        .count();
    fx.driver
        .send(Input::Frame(lazy_protocol::ServerFrame::CaughtUp {
            workspace_id: fx.world.workspace.id,
            seq: seq(10),
        }));
    let after = fx
        .driver
        .effects
        .iter()
        .filter(|e| matches!(e, Effect::FetchMessage { message } if *message == far.id))
        .count();
    assert_eq!(after, before + 1);
}

/// Opens a thread whose root the channel lacks, then has a new snapshot ask for the root again
/// while the first answer is still on its way. Returns the root and the world's workspace.
fn open_far_root_across_a_snapshot(fx: &mut Fx) -> Message {
    let general = fx.general;
    let far = fx.world.message(general, &fx.world.other, 1, "far back");
    fx.driver.send(Input::OpenThread {
        conversation: general,
        root: far.id,
    });
    fx.driver
        .send(Input::Frame(lazy_protocol::ServerFrame::SnapshotRequired {
            workspace_id: fx.world.workspace.id,
            reason: lazy_protocol::SnapshotReason::CursorAhead,
        }));
    let mut bootstrap = fx.world.bootstrap();
    bootstrap.workspaces[0].head = seq(31);
    fx.driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    let asked = fx
        .driver
        .effects
        .iter()
        .filter(|e| matches!(e, Effect::FetchMessage { message } if *message == far.id))
        .count();
    assert_eq!(asked, 2, "the snapshot asks for the root again");
    far
}

fn fetched_root(far: &Message, count: u32, as_of: i64, other: &User) -> Input {
    Input::MessageFetched {
        message: far.id,
        result: Ok(MessageResponse {
            message: with_summary(far, Some(summary(count, as_of, as_of, &[other]))),
        }),
    }
}

#[test]
fn the_newer_answer_for_the_root_wins_whichever_arrives_first() {
    for older_first in [true, false] {
        let mut fx = setup();
        let far = open_far_root_across_a_snapshot(&mut fx);
        let other = fx.world.other.clone();
        let older = fetched_root(&far, 2, 21, &other);
        let newer = fetched_root(&far, 5, 29, &other);
        let (first, second) = if older_first {
            (older, newer)
        } else {
            (newer, older)
        };
        fx.driver.send(first);
        fx.driver.send(second);
        let view = fx.driver.core.thread(far.id).unwrap();
        let shown = view.root.and_then(|root| root.thread.clone());
        assert_eq!(
            shown.map(|s| (s.reply_count, s.as_of)),
            Some((5, seq(29))),
            "older_first = {older_first}"
        );
    }
}

#[test]
fn a_failed_answer_for_the_root_waits_for_the_other_one() {
    let mut fx = setup();
    let far = open_far_root_across_a_snapshot(&mut fx);
    let other = fx.world.other.clone();
    fx.driver.send(Input::MessageFetched {
        message: far.id,
        result: Err(lost()),
    });
    // The other answer is still on its way: nothing is given up yet.
    assert!(fx.driver.core.notice().is_none());
    fx.driver.send(fetched_root(&far, 3, 25, &other));
    let view = fx.driver.core.thread(far.id).unwrap();
    assert_eq!(
        view.root
            .and_then(|root| root.thread.clone())
            .map(|s| s.reply_count),
        Some(3)
    );
    assert!(fx.driver.core.notice().is_none());
}

#[test]
fn a_failed_second_answer_does_not_take_back_a_root_that_arrived() {
    let mut fx = setup();
    let far = open_far_root_across_a_snapshot(&mut fx);
    let other = fx.world.other.clone();
    fx.driver.send(fetched_root(&far, 3, 25, &other));
    fx.driver.send(Input::MessageFetched {
        message: far.id,
        result: Err(lost()),
    });
    let view = fx.driver.core.thread(far.id).unwrap();
    assert!(view.root.is_some());
    assert!(fx.driver.core.notice().is_none());
}

#[test]
fn when_every_answer_for_the_root_fails_the_person_is_told() {
    let mut fx = setup();
    let far = open_far_root_across_a_snapshot(&mut fx);
    for _ in 0..2 {
        fx.driver.send(Input::MessageFetched {
            message: far.id,
            result: Err(lost()),
        });
    }
    assert!(fx.driver.core.thread(far.id).unwrap().root.is_none());
    assert!(
        fx.driver
            .core
            .notice()
            .is_some_and(|notice| notice.contains("Couldn't load the message"))
    );
}

#[test]
fn an_unknown_reply_in_a_closed_thread_is_asked_about_again_when_the_connection_catches_up() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let (operation, _) = send_reply(&mut fx, "did it land", false);
    fx.driver.send(Input::SendResult {
        operation,
        result: Err(lost()),
    });
    fx.driver.send(Input::OperationStatusResult {
        operation,
        result: Err(lost()),
    });
    fx.driver.send(Input::CloseThread);
    // Nothing on screen can retry it now. The root is deleted meanwhile, so the thread is gone.
    fx.driver
        .send(fx.world.deleted_event(12, &fx.world.tombstone(&fx.root)));
    let out = fx
        .driver
        .send(Input::Frame(lazy_protocol::ServerFrame::CaughtUp {
            workspace_id: fx.world.workspace.id,
            seq: seq(12),
        }));
    assert!(
        out.effects
            .contains(&Effect::QueryOperationStatus { operation })
    );
    // The server has no record, the same operation is resent, and the root is gone: final.
    let out = fx.driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    assert_eq!(sends(&out).len(), 1);
    let out = fx.driver.send(Input::SendResult {
        operation,
        result: Err(rejected(ErrorCode::NotFound)),
    });
    assert!(
        out.effects
            .contains(&Effect::SettleSubmission { operation })
    );
    assert_eq!(fx.driver.core.unsettled_operations(), 0);
}

#[test]
fn a_reply_to_a_root_that_was_deleted_meanwhile_says_why() {
    let mut fx = setup();
    open_with(&mut fx, vec![], false);
    let (operation, _) = send_reply(&mut fx, "too late", false);
    fx.driver.send(Input::SendResult {
        operation,
        result: Err(rejected(ErrorCode::NotFound)),
    });
    let rows = reply_rows(&fx.driver, fx.root.id);
    let Delivery::Failed { reason, retry } = &rows[0].delivery else {
        panic!("the reply should have failed");
    };
    assert_eq!(*retry, RetryKind::NewOperation);
    assert!(reason.contains("deleted"), "{reason}");
}

#[test]
fn what_arrived_before_a_new_request_is_already_in_its_page() {
    // A page request fails after an event about a root it never held; the next request is read
    // after a later event cleared the root's summary. The old event must not beat the new page.
    let mut fx = setup_with_older_history();
    let general = fx.general;
    let older = fx.world.message(general, &fx.world.other, 2, "older root");
    fx.driver.send(Input::LoadOlder {
        conversation: general,
    });
    fx.driver.send(thread_changed(
        &fx.world,
        11,
        &older,
        Some(summary(1, 9, 11, &[&fx.world.other])),
    ));
    let generation = fx
        .driver
        .effects_matching(|e| match e {
            Effect::FetchHistory { generation, .. } => Some(*generation),
            _ => None,
        })
        .pop()
        .unwrap();
    fx.driver.send(Input::HistoryLoaded {
        conversation: general,
        generation,
        result: Err(lost()),
    });
    fx.driver.send(thread_changed(&fx.world, 12, &older, None));
    fx.driver.send(Input::LoadOlder {
        conversation: general,
    });
    fx.driver.load_history(general, vec![older.clone()], false);
    assert!(channel_row(&fx.driver, general, older.id).thread.is_none());
}

#[test]
fn a_deletion_later_than_a_page_beats_the_summary_the_page_read() {
    let mut fx = setup_with_older_history();
    let general = fx.general;
    let older = fx.world.message(general, &fx.world.other, 2, "older root");
    fx.driver.send(Input::LoadOlder {
        conversation: general,
    });
    // The deletion arrives while the page (read before it, with a summary) is in flight; no
    // replies remain at the deletion.
    fx.driver
        .send(fx.world.deleted_event(11, &fx.world.tombstone(&older)));
    let page_root = with_summary(&older, Some(summary(1, 9, 9, &[&fx.world.other])));
    fx.driver.load_history(general, vec![page_root], false);
    assert_eq!(channel_ids(&fx.driver, general), vec![fx.root.id]);
}

#[test]
fn a_root_i_sent_takes_the_summary_the_answer_carries() {
    let mut fx = setup();
    let general = fx.general;
    let out = fx.driver.send(Input::SendMessage {
        conversation: general,
        text: "my root".into(),
    });
    let operation = journaled(&out)[0].operation_id;
    fx.driver.send(Input::SubmissionPersisted { operation });
    let committed = with_summary(
        &fx.world.message(general, &fx.world.me, 12, "my root"),
        Some(summary(1, 14, 15, &[&fx.world.other])),
    );
    confirm(&mut fx, operation, &committed);
    assert_eq!(
        channel_row(&fx.driver, general, committed.id)
            .thread
            .map(|s| s.as_of),
        Some(seq(15))
    );
}

#[test]
fn the_last_reply_of_a_deleted_root_going_makes_the_client_forget_the_thread() {
    let mut fx = setup_following(6, 0, 0, 0);
    let kept = summary(1, 9, 9, &[&fx.world.other]);
    let mut tombstone = fx.world.tombstone(&fx.root);
    tombstone.thread = Some(kept.clone());
    fx.driver.send(fx.world.deleted_event(12, &tombstone));
    assert!(fx.driver.core.thread_state(fx.root.id).is_some());
    fx.driver
        .send(thread_changed(&fx.world, 14, &fx.root, None));
    assert!(fx.driver.core.thread_state(fx.root.id).is_none());
}

#[test]
fn a_deleted_root_cannot_be_replied_to_and_a_reply_cannot_be_started_on_one() {
    let mut fx = setup();
    let mut tombstone = fx.world.tombstone(&fx.root);
    tombstone.thread = Some(summary(1, 9, 9, &[&fx.world.other]));
    fx.driver.send(fx.world.deleted_event(12, &tombstone));
    assert!(!fx.driver.core.can_reply_message(fx.general, fx.root.id));
    // The thread of a placeholder can still be read.
    open(&mut fx);
    assert!(
        fx.driver
            .core
            .thread(fx.root.id)
            .unwrap()
            .root
            .unwrap()
            .deleted
    );
}

// ----- Gaps the mutation checks found -----

#[test]
fn a_placeholder_is_not_an_unread_message_in_a_recount() {
    let world = World::new();
    let general = world.channel.id;
    let mut world = world;
    world.user_states = vec![common::read_state(general, 4, 1)];
    let mut driver = Driver::ready(&world);
    let mut placeholder = world.tombstone(&world.message(general, &world.other, 5, "gone"));
    placeholder.thread = Some(summary(1, 9, 9, &[&world.other]));
    let live = world.message(general, &world.other, 6, "live");
    driver.load_history(general, vec![placeholder, live], false);
    driver.send(Input::MarkReadResult {
        conversation: general,
        up_to: seq(4),
        result: Ok(common::read_state(general, 4, 1)),
    });
    assert_eq!(
        unread(&driver, general),
        1,
        "only the live message is unread"
    );
}

#[test]
fn a_page_in_flight_cannot_bring_back_a_root_whose_placeholder_was_removed() {
    let mut fx = setup_with_older_history();
    let general = fx.general;
    // The root is held with a summary; a page that overlaps it is requested.
    let kept = summary(1, 9, 9, &[&fx.world.other]);
    fx.driver
        .send(thread_changed(&fx.world, 11, &fx.root, Some(kept.clone())));
    fx.driver.send(Input::LoadOlder {
        conversation: general,
    });
    let stale = with_summary(&fx.root, Some(kept.clone()));
    // While it is in flight the root is deleted (a placeholder) and its last reply goes.
    let mut tombstone = fx.world.tombstone(&fx.root);
    tombstone.thread = Some(kept);
    fx.driver.send(fx.world.deleted_event(12, &tombstone));
    fx.driver
        .send(thread_changed(&fx.world, 14, &fx.root, None));
    assert!(channel_ids(&fx.driver, general).is_empty());
    fx.driver.load_history(general, vec![stale], false);
    assert!(channel_ids(&fx.driver, general).is_empty());
}

#[test]
fn a_deletion_of_a_reply_that_was_never_counted_subtracts_nothing_even_with_unread_ones() {
    let mut fx = setup_following(6, 0, 0, 0);
    let counted = reply(&fx.world, &fx.root, &fx.world.other, 12, "counted");
    fx.driver.send(fx.world.event(counted, None));
    assert_eq!(counts(&fx).0, 1);
    let ghost = reply(&fx.world, &fx.root, &fx.world.other, 13, "ghost");
    fx.driver
        .send(fx.world.event(fx.world.tombstone(&ghost), None));
    fx.driver.send(deleted_with(&fx.world, 14, &ghost, &[]));
    assert_eq!(
        counts(&fx).0,
        1,
        "the ghost was never counted, so it is not subtracted"
    );
}

#[test]
fn nothing_is_sent_to_mark_a_thread_read_when_the_newest_loaded_reply_is_already_read() {
    // The server says one reply is unread, but it is newer than anything loaded here.
    let mut fx = setup_following(21, 21, 1, 0);
    let a = reply(&fx.world, &fx.root, &fx.world.other, 20, "one");
    let b = reply(&fx.world, &fx.root, &fx.world.other, 21, "two");
    open_with(&mut fx, vec![a, b], false);
    fx.driver.send(Input::ThreadAtBottom {
        root: fx.root.id,
        at_bottom: true,
    });
    assert!(thread_reads(&fx.driver).is_empty());
}

#[test]
fn text_written_behind_a_held_text_waits_for_it_even_if_it_mentions_nobody() {
    let mut fx = setup();
    let general = fx.general;
    fx.driver.send(Input::SendMessage {
        conversation: general,
        text: "first @zoe".into(),
    });
    let out = fx.driver.send(Input::SendMessage {
        conversation: general,
        text: "second, plain".into(),
    });
    assert!(
        journaled(&out).is_empty(),
        "it does not overtake the held one"
    );
    let generation = members_generation(&fx.driver, general);
    let out = fx.driver.send(Input::MembersFetched {
        conversation: general,
        generation,
        result: Ok(ConversationMembersResponse {
            conversation_id: general,
            user_ids: vec![fx.world.me.id, fx.world.other.id],
        }),
    });
    let texts: Vec<String> = journaled(&out)
        .iter()
        .map(|entry| entry.document.plain_text())
        .collect();
    assert_eq!(
        texts,
        vec!["first @zoe".to_owned(), "second, plain".to_owned()]
    );
}

#[test]
fn an_unrelated_input_does_not_let_a_plain_text_overtake_a_held_one() {
    let mut fx = setup();
    let general = fx.general;
    fx.driver.send(Input::SendMessage {
        conversation: general,
        text: "first @zoe".into(),
    });
    fx.driver.send(Input::SendMessage {
        conversation: general,
        text: "second, plain".into(),
    });
    // Every input re-examines what is held; the first is still waiting.
    let out = fx.driver.send(Input::DismissNotice);
    assert!(journaled(&out).is_empty());
    let out = fx.driver.send(Input::SetDraft {
        conversation: general,
        text: "typing".into(),
    });
    assert!(journaled(&out).is_empty());
}

#[test]
fn a_follow_that_is_not_saved_yet_leaves_no_thread_state_in_the_cache() {
    let mut fx = setup();
    fx.driver.send(Input::SetThreadFollow {
        root: fx.root.id,
        following: true,
    });
    fx.driver.send(
        fx.world
            .event(fx.world.message(fx.general, &fx.world.other, 12, "x"), None),
    );
    let cache = fx.driver.last_cache().expect("a cache was written").clone();
    assert!(cache.workspaces[0].threads.is_empty());
}

#[test]
fn a_root_deleted_with_replies_while_the_first_page_loads_is_a_placeholder_at_once() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    // The first page is still to come; the root was created and deleted after it was read.
    let root = world.message(general, &world.other, 12, "root");
    driver.send(world.event(world.tombstone(&root), None));
    let mut tombstone = world.tombstone(&root);
    tombstone.thread = Some(summary(1, 13, 14, &[&world.other]));
    driver.send(world.deleted_event(15, &tombstone));
    assert_eq!(channel_ids(&driver, general), vec![root.id]);
    assert!(channel_row(&driver, general, root.id).deleted);
    driver.load_history(general, vec![], false);
    assert!(channel_row(&driver, general, root.id).deleted);
}

// ----- Keep imports honest -----

#[allow(dead_code)]
fn unused(
    _: (
        EventSeq,
        Permalink,
        JumpTarget,
        ThreadStateResponse,
        ConversationMembersResponse,
    ),
) {
}
