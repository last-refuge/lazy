//! Randomized interleavings of server events, history pages, acknowledgments, reconnects, and
//! app restarts against a scripted server. After every step the timelines must stay ordered and
//! free of duplicates; every cache write must satisfy the cache invariant (the cursor never runs
//! ahead of the messages it covers); once everything is delivered, every operation must be
//! settled exactly once and no committed message may be missing, even after restarts that find
//! any of the cache writes that could have survived a crash.

mod common;

use std::collections::{HashMap, HashSet};

use common::{Driver, World, seq};
use lazy_client_core::{
    ContentCache, Delivery, Effect, Input, JournalEntry, Output, RequestError, StartupState,
    TimelineItem,
};
use lazy_domain::{ConversationId, EventSeq, MessageId, OperationId};
use lazy_protocol::{
    ApiError, ConversationUserState, ErrorCode, Event, EventEnvelope, HistoryPage, Message,
    OperationStatus, SendMessageResponse, ServerFrame,
};
use proptest::prelude::*;
use proptest::test_runner::TestRunner;
use std::sync::atomic::{AtomicUsize, Ordering};

const SERVER_PAGE: usize = 4;
const BASE_HEAD: i64 = 10;

#[derive(Debug, Clone, Copy)]
enum Outcome {
    /// The server answers normally.
    Ok,
    /// The server commits, but the answer is lost.
    LostAnswer,
    /// The request is lost before the server acts on it.
    LostRequest,
    /// A definitive refusal (only possible when nothing was committed).
    Reject,
}

#[derive(Debug, Clone, Copy)]
enum Action {
    OtherPosts(bool),
    UserSends(bool),
    Resolve(usize, Outcome),
    Deliver(usize),
    Reconnect(usize),
    Select(bool),
    LoadOlder(bool),
    Snapshot,
    /// The window gains or loses focus.
    Window(bool),
    /// The timeline of a conversation reports whether its newest message is on screen.
    Bottom(bool, bool),
    /// The person reads a conversation on another device.
    OtherDeviceReads(bool),
    /// The app is killed and restarted. The disk holds whichever of the cache writes survived.
    Restart(usize),
}

fn outcome() -> impl Strategy<Value = Outcome> {
    prop_oneof![
        5 => Just(Outcome::Ok),
        1 => Just(Outcome::LostAnswer),
        1 => Just(Outcome::LostRequest),
        1 => Just(Outcome::Reject),
    ]
}

fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        3 => any::<bool>().prop_map(Action::OtherPosts),
        3 => any::<bool>().prop_map(Action::UserSends),
        9 => (any::<usize>(), outcome()).prop_map(|(pick, o)| Action::Resolve(pick, o)),
        4 => (1usize..4).prop_map(Action::Deliver),
        1 => (0usize..6).prop_map(Action::Reconnect),
        1 => any::<bool>().prop_map(Action::Select),
        1 => any::<bool>().prop_map(Action::LoadOlder),
        1 => Just(Action::Snapshot),
        3 => any::<bool>().prop_map(Action::Window),
        4 => (any::<bool>(), any::<bool>()).prop_map(|(c, b)| Action::Bottom(c, b)),
        1 => any::<bool>().prop_map(Action::OtherDeviceReads),
        2 => any::<usize>().prop_map(Action::Restart),
    ]
}

enum LogEvent {
    Message {
        message: Message,
        operation: Option<OperationId>,
    },
    /// The person's read state changed on the server (a mark-read, or another device).
    State(ConversationUserState),
}

struct LogEntry {
    event: LogEvent,
}

impl LogEntry {
    fn message_entry(message: Message, operation: Option<OperationId>) -> Self {
        Self {
            event: LogEvent::Message { message, operation },
        }
    }

    fn message(&self) -> Option<&Message> {
        match &self.event {
            LogEvent::Message { message, .. } => Some(message),
            LogEvent::State(_) => None,
        }
    }
}

struct Sim {
    world: World,
    driver: Driver,
    /// The workspace event log; entry `i` has position `BASE_HEAD + 1 + i`.
    log: Vec<LogEntry>,
    committed: HashMap<OperationId, Message>,
    /// Effects awaiting resolution, with the log length when the server saw them.
    pending: Vec<(Effect, usize)>,
    /// Index of the next log entry the socket delivers.
    pointer: usize,
    /// Where the current subscription started.
    subscribed_at: usize,
    created_ops: Vec<OperationId>,
    settle_counts: HashMap<OperationId, usize>,
    bootstrap_head: usize,
    trace: Vec<String>,
    /// Number of messages the person has tried to send; each carries its number as its text.
    submitted: usize,
    /// The submission number of each operation, read from its journal entry.
    submission_number: HashMap<OperationId, usize>,
    /// Operations that were ever given up on with an unknown outcome. The person may retry
    /// those at any time, which deliberately puts them behind later messages.
    given_up: HashSet<OperationId>,
    /// The states the cache file could be in: every write since the file was last deleted, oldest
    /// first. Writes are debounced, so after a crash any of them may be what survived.
    disk: Vec<Option<ContentCache>>,
    /// The durable journal: submissions whose write completed and that are not settled.
    journal: HashMap<OperationId, JournalEntry>,
    /// The server's record of how far the person has read each conversation.
    server_last_read: HashMap<ConversationId, i64>,
}

impl Sim {
    fn new() -> Self {
        let world = World::new();
        let mut sim = Self {
            driver: Driver::new(),
            world,
            log: Vec::new(),
            committed: HashMap::new(),
            pending: Vec::new(),
            pointer: 0,
            subscribed_at: 0,
            created_ops: Vec::new(),
            settle_counts: HashMap::new(),
            bootstrap_head: 0,
            trace: Vec::new(),
            submitted: 0,
            submission_number: HashMap::new(),
            given_up: HashSet::new(),
            disk: vec![None],
            journal: HashMap::new(),
            server_last_read: HashMap::new(),
        };
        let out = sim.driver.send(Input::Started(StartupState {
            session_restored: true,
            ..Default::default()
        }));
        sim.process(out);
        // The first bootstrap is answered right away rather than left pending.
        sim.pending.clear();
        sim.resolve_bootstrap();
        sim
    }

    fn conversation(&self, flag: bool) -> ConversationId {
        if flag {
            self.world.channel.id
        } else {
            self.world.second_channel.id
        }
    }

    fn head(&self) -> EventSeq {
        seq(BASE_HEAD + self.log.len() as i64)
    }

    fn feed(&mut self, input: Input) {
        let name = |id: ConversationId| {
            if id == self.world.channel.id {
                "general"
            } else {
                "random"
            }
        };
        let line = match &input {
            Input::HistoryLoaded {
                conversation,
                generation,
                result,
            } => format!(
                "HistoryLoaded {} gen={generation} {:?}",
                name(*conversation),
                result.as_ref().map(|p| (
                    p.messages.iter().map(|m| m.seq.get()).collect::<Vec<_>>(),
                    p.has_more
                ))
            ),
            Input::Frame(ServerFrame::Event(e)) => format!("Event seq={}", e.seq),
            Input::BootstrapLoaded(b) => format!("BootstrapLoaded head={}", b.workspaces[0].head),
            Input::SelectConversation(c) => format!("Select {}", name(*c)),
            Input::LoadOlder { conversation } => format!("LoadOlder {}", name(*conversation)),
            other => {
                let mut line = format!("{other:?}");
                line.truncate(90);
                line
            }
        };
        self.trace.push(format!("log={} <- {line}", self.log.len()));
        let out = self.driver.send(input);
        for effect in &out.effects {
            let line = match effect {
                Effect::FetchHistory {
                    conversation,
                    before,
                    generation,
                    ..
                } => format!(
                    "FetchHistory {} gen={generation} before={:?}",
                    name(*conversation),
                    before.map(|b| b.get())
                ),
                Effect::Subscribe { after, .. } => format!("Subscribe after={}", after),
                other => {
                    let mut line = format!("{other:?}");
                    line.truncate(60);
                    line
                }
            };
            self.trace.push(format!("      -> {line}"));
        }
        self.process(out);
        self.check_invariants();
        self.note_given_up();
    }

    fn note_given_up(&mut self) {
        for conversation in [self.world.channel.id, self.world.second_channel.id] {
            for row in self.driver.core.timeline(conversation) {
                let gave_up = matches!(
                    row.delivery,
                    Delivery::Failed {
                        retry: lazy_client_core::RetryKind::SameOperation,
                        ..
                    }
                );
                if let (true, Some(operation)) = (gave_up, row.operation_id()) {
                    self.given_up.insert(operation);
                }
            }
        }
    }

    /// Takes the effects the core asked for: some are handled by the simulated runtime
    /// immediately, the rest wait in `pending` to be resolved at an arbitrary later time.
    fn process(&mut self, out: Output) {
        for effect in out.effects {
            match effect {
                Effect::Subscribe { after, .. } => {
                    self.pointer = (after.get() - BASE_HEAD) as usize;
                    self.subscribed_at = self.pointer;
                    // Events from here on arrive by replay, so every message after this point
                    // must end up in its timeline.
                    self.bootstrap_head = self.pointer;
                }
                Effect::PersistCache(cache) => {
                    self.check_cache_invariant(&cache);
                    self.disk.push(Some(*cache));
                }
                Effect::DiscardCache => self.disk = vec![None],
                Effect::PersistDrafts(_) => {}
                Effect::SettleSubmission { operation } => {
                    *self.settle_counts.entry(operation).or_default() += 1;
                    self.journal.remove(&operation);
                }
                Effect::PersistSubmission(ref entry) => {
                    self.created_ops.push(entry.operation_id);
                    let text = entry.document.plain_text();
                    let number = text.strip_prefix("mine-").and_then(|n| n.parse().ok());
                    self.submission_number
                        .insert(entry.operation_id, number.expect("simulated message text"));
                    self.pending.push((effect, self.log.len()));
                }
                other => self.pending.push((other, self.log.len())),
            }
        }
    }

    /// The person's state in a conversation as the server computes it at log position
    /// `position`: messages from other people after the read position, up to that position.
    fn server_state(&self, conversation: ConversationId, position: i64) -> ConversationUserState {
        let last_read = self
            .server_last_read
            .get(&conversation)
            .copied()
            .unwrap_or(0);
        let me = self.world.me.id;
        let unread = self
            .log
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| {
                m.conversation_id == conversation
                    && m.author_id != me
                    && m.seq.get() > last_read
                    && m.seq.get() <= position
            })
            .count();
        ConversationUserState {
            conversation_id: conversation,
            favorite: false,
            last_read: seq(last_read),
            unread_count: u32::try_from(unread).unwrap(),
            mention_count: 0,
        }
    }

    /// Moves the person's read position forward, as a mark-read or another device would, and
    /// records the change in the log so every client learns of it in order.
    fn server_reads(&mut self, conversation: ConversationId, up_to: i64) {
        let newest = self
            .log
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| m.conversation_id == conversation)
            .map(|m| m.seq.get())
            .max()
            .unwrap_or(0);
        let target = up_to.min(newest);
        if target
            <= self
                .server_last_read
                .get(&conversation)
                .copied()
                .unwrap_or(0)
        {
            return;
        }
        self.server_last_read.insert(conversation, target);
        let position = BASE_HEAD + 1 + self.log.len() as i64;
        let state = self.server_state(conversation, position);
        self.log.push(LogEntry {
            event: LogEvent::State(state),
        });
    }

    fn resolve_bootstrap(&mut self) {
        let mut bootstrap = self.world.bootstrap();
        bootstrap.workspaces[0].head = self.head();
        bootstrap.workspaces[0].user_state = [self.world.channel.id, self.world.second_channel.id]
            .into_iter()
            .map(|c| self.server_state(c, BASE_HEAD + self.log.len() as i64))
            .collect();
        self.feed(Input::BootstrapLoaded(Box::new(bootstrap)));
    }

    fn commit(
        &mut self,
        operation: OperationId,
        conversation: ConversationId,
        text: &str,
    ) -> Message {
        if let Some(message) = self.committed.get(&operation) {
            return message.clone();
        }
        let at = BASE_HEAD + 1 + self.log.len() as i64;
        let message = self
            .world
            .message(conversation, &self.world.me.clone(), at, text);
        self.log
            .push(LogEntry::message_entry(message.clone(), Some(operation)));
        // The server moves the author's read position in the same transaction, without an event.
        let read = self.server_last_read.entry(conversation).or_insert(0);
        *read = (*read).max(at);
        self.committed.insert(operation, message.clone());
        message
    }

    fn page(
        &self,
        conversation: ConversationId,
        before: Option<EventSeq>,
        visible: usize,
    ) -> HistoryPage {
        let candidates: Vec<&Message> = self.log[..visible.min(self.log.len())]
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| m.conversation_id == conversation)
            .filter(|m| before.is_none_or(|b| m.seq < b))
            .collect();
        let start = candidates.len().saturating_sub(SERVER_PAGE);
        HistoryPage {
            messages: candidates[start..].iter().map(|m| (*m).clone()).collect(),
            has_more: start > 0,
        }
    }

    fn resolve(&mut self, pick: usize, outcome: Outcome) {
        if self.pending.is_empty() {
            return;
        }
        let (effect, seen) = self.pending.remove(pick % self.pending.len());
        let unknown = || RequestError::Unknown("lost".into());
        match effect {
            Effect::PersistSubmission(entry) => {
                let operation = entry.operation_id;
                let input = match outcome {
                    Outcome::Reject => Input::SubmissionPersistFailed {
                        operation,
                        message: "disk".into(),
                    },
                    _ => {
                        self.journal.insert(operation, entry);
                        Input::SubmissionPersisted { operation }
                    }
                };
                self.feed(input);
            }
            Effect::SendMessage {
                operation,
                conversation,
                document,
            } => {
                let text = document.plain_text();
                let result = match outcome {
                    Outcome::Ok => Ok(SendMessageResponse {
                        message: self.commit(operation, conversation, &text),
                    }),
                    Outcome::LostAnswer => {
                        self.commit(operation, conversation, &text);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => match self.committed.get(&operation) {
                        // Idempotent: a committed operation always returns its result.
                        Some(message) => Ok(SendMessageResponse {
                            message: message.clone(),
                        }),
                        None => Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Forbidden,
                            "no",
                        ))),
                    },
                };
                self.feed(Input::SendResult { operation, result });
            }
            Effect::QueryOperationStatus { operation } => {
                let result = match outcome {
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                    _ => Ok(match self.committed.get(&operation) {
                        Some(message) => OperationStatus::Committed {
                            message: message.clone(),
                        },
                        None => OperationStatus::NotFound,
                    }),
                };
                self.feed(Input::OperationStatusResult { operation, result });
            }
            Effect::FetchHistory {
                conversation,
                before,
                generation,
                ..
            } => {
                let result = match outcome {
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                    _ => Ok(self.page(conversation, before, seen)),
                };
                self.feed(Input::HistoryLoaded {
                    conversation,
                    generation,
                    result,
                });
            }
            Effect::LoadBootstrap => self.resolve_bootstrap(),
            Effect::MarkRead {
                conversation,
                up_to,
            } => {
                MARK_READS_ANSWERED.fetch_add(1, Ordering::Relaxed);
                let result = match outcome {
                    Outcome::Ok => {
                        self.server_reads(conversation, up_to.get());
                        Ok(self.server_state(conversation, BASE_HEAD + self.log.len() as i64))
                    }
                    Outcome::LostAnswer => {
                        self.server_reads(conversation, up_to.get());
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::Forbidden,
                        "no",
                    ))),
                };
                self.feed(Input::MarkReadResult {
                    conversation,
                    up_to,
                    result,
                });
            }
            other => panic!("unexpected pending effect {other:?}"),
        }
    }

    fn deliver(&mut self, count: usize) {
        for _ in 0..count {
            if self.pointer >= self.log.len() {
                break;
            }
            let entry = &self.log[self.pointer];
            let event = match &entry.event {
                LogEvent::Message { message, operation } => Event::MessageCreated {
                    message: message.clone(),
                    operation_id: *operation,
                },
                LogEvent::State(state) => {
                    READ_STATE_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::UserStateChanged {
                        state: state.clone(),
                    }
                }
            };
            let envelope = EventEnvelope {
                workspace_id: self.world.workspace.id,
                seq: seq(BASE_HEAD + 1 + self.pointer as i64),
                occurred_at: lazy_domain::Timestamp::now(),
                event,
            };
            self.pointer += 1;
            self.feed(Input::Frame(ServerFrame::Event(envelope)));
        }
        if self.pointer == self.log.len() {
            let frame = ServerFrame::CaughtUp {
                workspace_id: self.world.workspace.id,
                seq: self.head(),
            };
            self.feed(Input::Frame(frame));
        }
    }

    /// The cache invariant, checked against the server's log at every write: the cursor never
    /// runs ahead of the messages it covers. Every message of a cached conversation between its
    /// oldest cached message and the cursor is cached; if older ones exist, `has_more` says so.
    fn check_cache_invariant(&self, cache: &ContentCache) {
        CACHE_WRITES_CHECKED.fetch_add(1, Ordering::Relaxed);
        for workspace in &cache.workspaces {
            let cursor = workspace.cursor.get();
            assert!(
                cursor <= BASE_HEAD + self.log.len() as i64,
                "the cached cursor {cursor} is beyond the server's log"
            );
            for saved in &workspace.user_state {
                let horizon = saved.as_of.get().max(cursor);
                let others = self.others_messages(saved.state.conversation_id, horizon, 0);
                assert!(
                    saved.state.unread_count as usize <= others,
                    "a saved unread count of {} exceeds the {others} messages written by \
                     other people by position {horizon}",
                    saved.state.unread_count
                );
            }
            for timeline in &workspace.timelines {
                let cached: HashSet<MessageId> = timeline.messages.iter().map(|m| m.id).collect();
                let floor = timeline.messages.first().map(|m| m.seq.get());
                for message in self.log.iter().filter_map(LogEntry::message) {
                    if message.conversation_id != timeline.conversation
                        || message.seq.get() > cursor
                    {
                        continue;
                    }
                    match floor {
                        Some(floor) if message.seq.get() >= floor => assert!(
                            cached.contains(&message.id),
                            "message {} is missing from a cache whose cursor is {cursor}\n{}",
                            message.seq,
                            self.trace.join("\n")
                        ),
                        _ => assert!(
                            timeline.has_more,
                            "older message {} exists but the cache says there is none (cursor {cursor}); cached floor {floor:?}\n{}",
                            message.seq,
                            self.trace.join("\n")
                        ),
                    }
                }
            }
        }
    }

    /// Counts of unread messages are sane at every step: present, never more than the number of
    /// messages other people have written, and the sidebar lists every conversation once.
    fn check_read_state(&self, conversation: ConversationId) {
        let others = self.others_messages(conversation, i64::MAX, 0);
        if let Some(state) = self.driver.core.user_state(conversation) {
            assert!(
                state.unread_count as usize <= others,
                "{} unread but only {others} messages from other people exist",
                state.unread_count
            );
        }
        let sidebar = self.driver.core.sidebar();
        assert_eq!(
            sidebar.favorites.len() + sidebar.channels.len() + sidebar.direct_and_group.len(),
            self.driver.core.conversations().len()
        );
    }

    /// How many messages other people wrote in a conversation after `after`, up to `until`.
    fn others_messages(&self, conversation: ConversationId, until: i64, after: i64) -> usize {
        self.log
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| {
                m.conversation_id == conversation
                    && m.author_id != self.world.me.id
                    && m.seq.get() > after
                    && m.seq.get() <= until
            })
            .count()
    }

    /// The app is killed. Requests in flight die with it; durable state (the journal, and
    /// whichever cache write survived) is what the next start finds.
    fn restart(&mut self, pick: usize) {
        // A submission whose journal write had not finished never existed as far as the next
        // run is concerned.
        self.created_ops
            .retain(|op| self.journal.contains_key(op) || self.settle_counts.contains_key(op));
        self.pending.clear();
        let surviving = self.disk[pick % self.disk.len()].clone();
        match &surviving {
            Some(cache) if cache.message_count() > 0 => {
                RESTARTS_WITH_CACHED_MESSAGES.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                RESTARTS_WITHOUT_CACHE.fetch_add(1, Ordering::Relaxed);
            }
            Some(_) => {}
        }
        self.disk = vec![surviving.clone()];
        self.driver = Driver::new();
        let journal: Vec<JournalEntry> = self.journal.values().cloned().collect();
        self.feed(Input::Started(StartupState {
            session_restored: true,
            journal,
            cache: surviving,
            ..Default::default()
        }));
    }

    fn step(&mut self, action: Action) {
        match action {
            Action::OtherPosts(flag) => {
                let conversation = self.conversation(flag);
                let at = BASE_HEAD + 1 + self.log.len() as i64;
                let message = self
                    .world
                    .message(conversation, &self.world.other.clone(), at, "hi");
                self.log.push(LogEntry::message_entry(message, None));
            }
            Action::UserSends(flag) => {
                let conversation = self.conversation(flag);
                let text = format!("mine-{}", self.submitted);
                self.submitted += 1;
                self.feed(Input::SendMessage { conversation, text });
            }
            Action::Resolve(pick, outcome) => self.resolve(pick, outcome),
            Action::Deliver(count) => self.deliver(count),
            Action::Reconnect(back) => {
                self.pointer = self.pointer.saturating_sub(back).max(self.subscribed_at);
            }
            Action::Select(flag) => {
                let conversation = self.conversation(flag);
                self.feed(Input::SelectConversation(conversation));
            }
            Action::LoadOlder(flag) => {
                let conversation = self.conversation(flag);
                self.feed(Input::LoadOlder { conversation });
            }
            Action::Restart(pick) => self.restart(pick),
            Action::Window(active) => self.feed(Input::WindowActive(active)),
            Action::Bottom(flag, at_bottom) => {
                let conversation = self.conversation(flag);
                self.feed(Input::TimelineAtBottom {
                    conversation,
                    at_bottom,
                });
            }
            Action::OtherDeviceReads(flag) => {
                let conversation = self.conversation(flag);
                self.server_reads(conversation, i64::MAX);
            }
            Action::Snapshot => {
                let frame = ServerFrame::SnapshotRequired {
                    workspace_id: self.world.workspace.id,
                    reason: lazy_protocol::SnapshotReason::CursorExpired,
                };
                self.feed(Input::Frame(ServerFrame::Event(EventEnvelope {
                    workspace_id: self.world.workspace.id,
                    seq: seq(0),
                    occurred_at: lazy_domain::Timestamp::now(),
                    event: Event::MemberJoined {
                        user: self.world.other.clone(),
                    },
                })));
                self.feed(Input::Frame(frame));
            }
        }
    }

    fn check_invariants(&self) {
        for conversation in [self.world.channel.id, self.world.second_channel.id] {
            let rows = self.driver.core.timeline(conversation);
            let confirmed = rows.iter().take_while(|r| r.is_confirmed()).count();
            assert!(
                rows[confirmed..].iter().all(|r| !r.is_confirmed()),
                "confirmed rows must precede unconfirmed rows"
            );
            let seqs: Vec<_> = rows[..confirmed]
                .iter()
                .filter_map(TimelineItem::seq)
                .collect();
            assert!(
                seqs.windows(2).all(|w| w[0] < w[1]),
                "rows not strictly ordered: {seqs:?}"
            );
            let mut ids = HashSet::new();
            let mut keys = HashSet::new();
            self.check_read_state(conversation);
            for row in rows {
                if let Some(id) = row.message_id() {
                    assert!(ids.insert(id), "duplicate message id in timeline");
                }
                assert!(keys.insert(row.key), "duplicate row key");
            }
        }
    }

    /// Delivers everything, answers every request normally, and retries stuck sends until the
    /// system is quiet.
    fn drain(&mut self) {
        for _ in 0..500 {
            let mut progressed = false;
            if self.pointer < self.log.len() {
                self.deliver(self.log.len());
                progressed = true;
            }
            if !self.pending.is_empty() {
                self.resolve(0, Outcome::Ok);
                continue;
            }
            for conversation in [self.world.channel.id, self.world.second_channel.id] {
                let stuck: Vec<OperationId> = self
                    .driver
                    .core
                    .timeline(conversation)
                    .iter()
                    .filter_map(|row| match (&row.delivery, row.operation_id()) {
                        (
                            Delivery::Failed {
                                retry: lazy_client_core::RetryKind::SameOperation,
                                ..
                            },
                            Some(op),
                        ) => Some(op),
                        _ => None,
                    })
                    .collect();
                for operation in stuck {
                    self.feed(Input::RetryOperation {
                        conversation,
                        operation,
                    });
                    progressed = true;
                }
                if matches!(
                    self.driver.core.history_status(conversation),
                    lazy_client_core::HistoryStatus::Ready { has_more: true }
                ) {
                    self.feed(Input::LoadOlder { conversation });
                    progressed = true;
                }
            }
            if !progressed {
                return;
            }
        }
        panic!("the simulation did not quiesce");
    }

    fn check_final(&self) {
        assert_eq!(
            self.driver.core.unsettled_operations(),
            0,
            "operations left unsettled"
        );
        for operation in &self.created_ops {
            assert_eq!(
                self.settle_counts.get(operation).copied().unwrap_or(0),
                1,
                "operation {operation} must be settled exactly once"
            );
        }
        // For one sender, messages are committed in the order they were submitted. (Messages the
        // client gave up on may be retried later and then legitimately land behind newer ones.)
        for conversation in [self.world.channel.id, self.world.second_channel.id] {
            let mut committed: Vec<_> = self
                .committed
                .iter()
                .filter(|(operation, message)| {
                    message.conversation_id == conversation && !self.given_up.contains(operation)
                })
                .map(|(operation, message)| (message.seq, self.submission_number[operation]))
                .collect();
            committed.sort();
            let numbers: Vec<usize> = committed.iter().map(|(_, number)| *number).collect();
            assert!(
                numbers.windows(2).all(|w| w[0] < w[1]),
                "committed out of submission order: {numbers:?}"
            );
        }
        // Read state has converged on the server's: the same read position, and an unread count
        // equal to the messages other people wrote after it, which for a complete timeline is
        // also the number of loaded rows from other people after the read position.
        for conversation in [self.world.channel.id, self.world.second_channel.id] {
            let state = self
                .driver
                .core
                .user_state(conversation)
                .expect("a read state for every conversation");
            let server_read = self
                .server_last_read
                .get(&conversation)
                .copied()
                .unwrap_or(0);
            assert_eq!(
                state.last_read.get(),
                server_read,
                "read position\n{}",
                self.trace.join("\n")
            );
            let expected = self.others_messages(conversation, i64::MAX, server_read);
            assert_eq!(
                state.unread_count as usize,
                expected,
                "unread count\n{}",
                self.trace.join("\n")
            );
            if matches!(
                self.driver.core.history_status(conversation),
                lazy_client_core::HistoryStatus::Ready { has_more: false }
            ) {
                let loaded = self
                    .driver
                    .core
                    .timeline(conversation)
                    .iter()
                    .filter(|row| {
                        row.author_id != self.world.me.id
                            && row.seq().is_some_and(|s| s.get() > server_read)
                    })
                    .count();
                assert_eq!(state.unread_count as usize, loaded, "unread vs loaded rows");
            }
        }
        let mut present: HashMap<ConversationId, HashSet<MessageId>> = HashMap::new();
        for conversation in [self.world.channel.id, self.world.second_channel.id] {
            let rows = self.driver.core.timeline(conversation);
            for row in rows {
                assert!(
                    matches!(
                        row.delivery,
                        Delivery::Confirmed { .. } | Delivery::Failed { .. }
                    ),
                    "row left in flight: {:?}",
                    row.delivery
                );
            }
            present.insert(
                conversation,
                rows.iter().filter_map(TimelineItem::message_id).collect(),
            );
        }
        for (index, entry) in self.log.iter().enumerate() {
            let Some(message) = entry.message() else {
                continue;
            };
            let ids = &present[&message.conversation_id];
            let loaded = matches!(
                self.driver.core.history_status(message.conversation_id),
                lazy_client_core::HistoryStatus::Ready { .. }
            );
            if loaded || index >= self.bootstrap_head {
                let rows = self.driver.core.timeline(message.conversation_id);
                let log: Vec<_> = self
                    .log
                    .iter()
                    .filter_map(LogEntry::message)
                    .map(|m| (m.seq.get(), m.conversation_id == self.world.channel.id))
                    .collect();
                assert!(
                    ids.contains(&message.id),
                    "message at {} (index {index}, general={}) missing; loaded={loaded} bootstrap_head={} status={:?}\ntrace:\n{}\ntimeline seqs={:?}\nlog(seq, general)={log:?}",
                    message.seq,
                    message.conversation_id == self.world.channel.id,
                    self.bootstrap_head,
                    self.driver.core.history_status(message.conversation_id),
                    self.trace.join("\n"),
                    rows.iter()
                        .map(|r| (r.seq().map(|s| s.get()), r.is_confirmed()))
                        .collect::<Vec<_>>(),
                );
            }
        }
        for message in self.committed.values() {
            let loaded = matches!(
                self.driver.core.history_status(message.conversation_id),
                lazy_client_core::HistoryStatus::Ready { .. }
            );
            if loaded {
                assert!(present[&message.conversation_id].contains(&message.id));
            }
        }
    }
}

/// How often the random runs exercised the paths this test exists for.
static RESTARTS_WITH_CACHED_MESSAGES: AtomicUsize = AtomicUsize::new(0);
static RESTARTS_WITHOUT_CACHE: AtomicUsize = AtomicUsize::new(0);
static CACHE_WRITES_CHECKED: AtomicUsize = AtomicUsize::new(0);
static MARK_READS_ANSWERED: AtomicUsize = AtomicUsize::new(0);
static READ_STATE_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);

#[test]
fn timelines_stay_consistent_under_any_interleaving() {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let mut runner = TestRunner::new(ProptestConfig {
        cases,
        ..ProptestConfig::default()
    });
    runner
        .run(&prop::collection::vec(action(), 1..120), |actions| {
            let mut sim = Sim::new();
            for action in actions {
                sim.step(action);
            }
            sim.drain();
            sim.check_final();
            Ok(())
        })
        .unwrap_or_else(|failure| panic!("{failure}"));

    // The run is only meaningful if it really restarted from caches that held messages, from
    // nothing at all, and checked the invariant on real writes.
    assert!(RESTARTS_WITH_CACHED_MESSAGES.load(Ordering::Relaxed) > 0);
    assert!(RESTARTS_WITHOUT_CACHE.load(Ordering::Relaxed) > 0);
    assert!(CACHE_WRITES_CHECKED.load(Ordering::Relaxed) > 0);
    assert!(
        MARK_READS_ANSWERED.load(Ordering::Relaxed) > 0,
        "mark-read was exercised"
    );
    assert!(
        READ_STATE_EVENTS_DELIVERED.load(Ordering::Relaxed) > 0,
        "read-state events were exercised"
    );
    eprintln!(
        "simulation coverage: {} mark-reads answered, {} read-state events delivered",
        MARK_READS_ANSWERED.load(Ordering::Relaxed),
        READ_STATE_EVENTS_DELIVERED.load(Ordering::Relaxed),
    );
    eprintln!(
        "simulation coverage: {} restarts from a cache with messages, {} without a cache, {} cache writes checked",
        RESTARTS_WITH_CACHED_MESSAGES.load(Ordering::Relaxed),
        RESTARTS_WITHOUT_CACHE.load(Ordering::Relaxed),
        CACHE_WRITES_CHECKED.load(Ordering::Relaxed),
    );
}

/// A cached restart in a fixed scenario: history loaded, more messages committed and some of
/// them delivered, then the app is killed with the latest cache write on disk.
#[test]
fn a_restart_from_the_latest_cache_loses_nothing() {
    let mut sim = Sim::new();
    sim.step(Action::Resolve(0, Outcome::Ok)); // the first history page
    for _ in 0..6 {
        sim.step(Action::OtherPosts(true));
    }
    sim.step(Action::Deliver(3));
    sim.step(Action::OtherPosts(true));
    let latest = sim.disk.len() - 1;
    sim.step(Action::Restart(latest));
    assert_eq!(
        sim.driver.core.phase(),
        &lazy_client_core::SessionPhase::Ready
    );
    assert!(
        !sim.driver.core.timeline(sim.world.channel.id).is_empty() || sim.log.is_empty(),
        "cached content is on screen before anything comes back from the server"
    );
    sim.drain();
    sim.check_final();
}

/// Found by the randomized run: kept as a fixed scenario.
#[test]
fn regression_found_by_the_random_run() {
    let mut sim = Sim::new();
    for action in [
        Action::Snapshot,
        Action::Resolve(0, Outcome::Ok),
        Action::Resolve(0, Outcome::Ok),
        Action::Resolve(0, Outcome::Ok),
        Action::Select(false),
        Action::Resolve(0, Outcome::Ok),
        Action::Restart(27927307348653919),
        Action::OtherPosts(false),
        Action::UserSends(false),
        Action::Resolve(8527240021953225653, Outcome::Ok),
        Action::Resolve(414403886655768013, Outcome::Ok),
        Action::Resolve(0, Outcome::Ok),
        Action::UserSends(false),
        Action::Resolve(0, Outcome::Ok),
        Action::Deliver(1),
        Action::Resolve(0, Outcome::Ok),
        Action::Restart(4943207698118845286),
        Action::OtherPosts(false),
    ] {
        sim.step(action);
    }
    sim.drain();
    sim.check_final();
}
