//! The client state machine.

use std::collections::{BTreeSet, HashMap, HashSet};

use lazy_domain::{
    ConversationId, DisplayName, DocumentError, EventSeq, MessageDocument, OperationId, Timestamp,
    UserId, Username, WorkspaceId,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, Event, EventEnvelope, LoginRequest, Message, OperationStatus,
    PROTOCOL_VERSION, Password, RegisterRequest, ServerFrame, User,
};

use crate::cache::{CachedWorkspace, ContentCache};
use crate::change::{Change, Output};
use crate::effect::Effect;
use crate::input::{Input, SocketEvent};
use crate::model::{
    ConnectionState, Delivery, HistoryStatus, ItemKey, JournalEntry, RequestError, RetryKind,
    SessionPhase, TimelineItem, UNKNOWN_MEMBER_NAME, WorkspaceInfo,
};
use crate::timeline::Timeline;

/// Messages fetched per history request.
const HISTORY_PAGE_LIMIT: u32 = 50;
/// How many times an operation the server has no record of is resent automatically.
const MAX_RESENDS: u32 = 3;
/// How many cached messages cost one skipped cache write. Building the cache is proportional to
/// its size and runs on the UI thread, so a big cache is saved less often when events arrive in
/// a stream (for example a backlog replayed after a long absence). Skipping a write is always
/// safe: the saved cache is just a little older, and replay covers the difference.
const CACHE_MESSAGES_PER_SKIPPED_WRITE: usize = 500;
const MIN_PASSWORD_BYTES: usize = 8;
const MAX_PASSWORD_BYTES: usize = 256;

/// Where an unsettled operation is. Within one conversation only the earliest operation that is
/// not `Failed` may have a send in flight (see [`ClientCore::pump`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpStage {
    /// Waiting for the journal write to become durable.
    Persisting,
    /// Ready to be sent (journaled, or known to be absent on the server) and waiting for its
    /// turn behind earlier operations.
    Queued,
    /// A send request is in flight.
    Sending,
    /// A status query is in flight.
    Querying,
    /// Not delivered and not in flight; waiting for the person. Does not hold up later messages.
    Failed,
}

/// An operation that has not reached a final state.
#[derive(Debug)]
struct LocalOp {
    conversation: ConversationId,
    document: MessageDocument,
    stage: OpStage,
    /// Resends triggered by "no record" answers since the person last retried.
    resends: u32,
    /// Position in the send queue: lower goes first. Assigned at submission (journal-recovered
    /// operations get theirs first, ordered by when they were submitted) and again when the
    /// person retries an operation whose outcome was unknown, which sends it to the back.
    order: u64,
}

/// The client's state machine.
///
/// All state changes go through [`ClientCore::handle`]. The core performs no I/O: it returns
/// [`Effect`]s for the runtime to execute and [`Change`]s describing what the UI should refresh.
#[derive(Debug)]
pub struct ClientCore {
    phase: SessionPhase,
    me: Option<User>,
    users: HashMap<UserId, User>,
    workspaces: Vec<WorkspaceInfo>,
    conversations: Vec<Conversation>,
    selected: Option<ConversationId>,
    drafts: HashMap<ConversationId, String>,
    timelines: HashMap<ConversationId, Timeline>,
    ops: HashMap<OperationId, LocalOp>,
    sockets: HashMap<WorkspaceId, ConnectionState>,
    connection: ConnectionState,
    /// Last event position applied, per workspace.
    applied: HashMap<WorkspaceId, EventSeq>,
    /// Journaled submissions from a previous run that are not yet reconciled.
    recovered_journal: Vec<JournalEntry>,
    bootstrap_in_flight: bool,
    notice: Option<String>,
    /// Source of `LocalOp::order`.
    next_order: u64,
    /// Members of each workspace, as of the last bootstrap and later joins; saved in the cache.
    members: HashMap<WorkspaceId, BTreeSet<UserId>>,
    /// Workspaces with a realtime subscription requested.
    subscribed: HashSet<WorkspaceId>,
    /// The screen shows cached content and the first bootstrap has not yet confirmed it.
    revalidating: bool,
    /// Something the cache holds has changed; a `PersistCache` is due at the end of `handle`.
    cache_dirty: bool,
    /// What changed is structural (directory, a loaded page, members), so it is saved at once
    /// instead of waiting out the write-skipping budget for a stream of live events.
    cache_urgent: bool,
    /// Writes skipped since the last `PersistCache`, and the size of that cache in messages.
    cache_skipped: usize,
    cache_size: usize,
    /// The server asked for a fresh snapshot; nothing may be cached until the bootstrap that
    /// answers it has been applied, because the cursor and content on hand are stale.
    cache_suppressed: bool,
    /// Whose cache is on disk, as far as this run knows (loaded, or saved by this run).
    cache_owner: Option<UserId>,
}

impl Default for ClientCore {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientCore {
    /// A core waiting for the runtime's `Input::Started`.
    pub fn new() -> Self {
        Self {
            phase: SessionPhase::Loading,
            me: None,
            users: HashMap::new(),
            workspaces: Vec::new(),
            conversations: Vec::new(),
            selected: None,
            drafts: HashMap::new(),
            timelines: HashMap::new(),
            ops: HashMap::new(),
            sockets: HashMap::new(),
            connection: ConnectionState::Offline,
            applied: HashMap::new(),
            recovered_journal: Vec::new(),
            bootstrap_in_flight: false,
            notice: None,
            next_order: 0,
            members: HashMap::new(),
            subscribed: HashSet::new(),
            revalidating: false,
            cache_dirty: false,
            cache_urgent: false,
            cache_skipped: 0,
            cache_size: 0,
            cache_suppressed: false,
            cache_owner: None,
        }
    }

    // ----- Read model -----

    pub fn phase(&self) -> &SessionPhase {
        &self.phase
    }

    pub fn connection(&self) -> ConnectionState {
        self.connection
    }

    /// The signed-in person.
    pub fn me(&self) -> Option<&User> {
        self.me.as_ref()
    }

    pub fn workspaces(&self) -> &[WorkspaceInfo] {
        &self.workspaces
    }

    /// The first workspace, which is the only one the MVP shows.
    pub fn primary_workspace(&self) -> Option<&WorkspaceInfo> {
        self.workspaces.first()
    }

    /// Conversations the person can read, across workspaces.
    pub fn conversations(&self) -> &[Conversation] {
        &self.conversations
    }

    pub fn conversation(&self, id: ConversationId) -> Option<&Conversation> {
        self.conversations.iter().find(|c| c.id == id)
    }

    pub fn selected_conversation(&self) -> Option<ConversationId> {
        self.selected
    }

    pub fn member(&self, id: UserId) -> Option<&User> {
        self.users.get(&id)
    }

    /// A person's display name, or [`UNKNOWN_MEMBER_NAME`] when the profile is not known.
    pub fn display_name(&self, id: UserId) -> &str {
        self.users
            .get(&id)
            .map_or(UNKNOWN_MEMBER_NAME, |user| user.display_name.as_str())
    }

    /// The composer text saved for a conversation.
    pub fn draft(&self, conversation: ConversationId) -> &str {
        self.drafts.get(&conversation).map_or("", String::as_str)
    }

    /// Rows of a conversation in display order: confirmed messages by event position, then
    /// messages still being delivered in submission order.
    pub fn timeline(&self, conversation: ConversationId) -> &[TimelineItem] {
        self.timelines
            .get(&conversation)
            .map_or(&[], |timeline| timeline.items())
    }

    pub fn history_status(&self, conversation: ConversationId) -> &HistoryStatus {
        self.timelines
            .get(&conversation)
            .map_or(&HistoryStatus::NotLoaded, |timeline| &timeline.history)
    }

    /// A message for the person, such as a validation problem or a storage failure.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Number of messages that have not reached a final state. Intended for diagnostics and
    /// tests.
    pub fn unsettled_operations(&self) -> usize {
        self.ops.len()
    }

    // ----- State machine -----

    /// Applies one input and returns the resulting effects and changes.
    pub fn handle(&mut self, input: Input) -> Output {
        let mut out = self.dispatch(input);
        self.flush_cache(&mut out);
        out
    }

    fn dispatch(&mut self, input: Input) -> Output {
        let mut out = Output::default();
        match input {
            Input::Register {
                username,
                display_name,
                password,
            } => self.register(&username, &display_name, password, &mut out),
            Input::SignIn { username, password } => self.sign_in(&username, password, &mut out),
            Input::SignOut => self.sign_out(&mut out),
            Input::RetryLoad => {
                if matches!(self.phase, SessionPhase::Unreachable { .. }) {
                    self.set_phase(SessionPhase::Loading, &mut out);
                    self.request_bootstrap(&mut out);
                } else if self.revalidating {
                    self.request_bootstrap(&mut out);
                }
            }
            Input::SelectConversation(id) => self.select_conversation(id, &mut out),
            Input::SetDraft { conversation, text } => self.set_draft(conversation, text, &mut out),
            Input::SendMessage { conversation, text } => {
                self.send_message(conversation, &text, &mut out)
            }
            Input::RetryOperation {
                conversation,
                operation,
            } => self.retry_operation(conversation, operation, &mut out),
            Input::LoadOlder { conversation } => self.load_older(conversation, &mut out),
            Input::DismissNotice => self.set_notice(None, &mut out),

            Input::Started(state) => {
                self.drafts = state.drafts;
                self.recovered_journal = state.journal;
                if !state.problems.is_empty() {
                    self.set_notice(Some(state.problems.join(" ")), &mut out);
                }
                let cache = state
                    .cache
                    .map(ContentCache::bounded)
                    .filter(|cache| !cache.workspaces.is_empty());
                self.cache_owner = cache.as_ref().map(|cache| cache.user.id);
                match (state.session_restored, cache) {
                    (true, Some(cache)) => self.restore_from_cache(cache, &mut out),
                    (true, None) => {
                        self.set_phase(SessionPhase::Loading, &mut out);
                        self.request_bootstrap(&mut out);
                    }
                    (false, _) => {
                        self.set_phase(SessionPhase::SignedOut { last_error: None }, &mut out)
                    }
                }
            }
            Input::Authenticated { user } => self.authenticated(user, &mut out),
            Input::AuthFailed(error) => {
                let message = error.message();
                self.set_phase(
                    SessionPhase::SignedOut {
                        last_error: Some(message),
                    },
                    &mut out,
                );
            }
            Input::SessionExpired => self.session_expired(&mut out),
            Input::BootstrapLoaded(response) => self.apply_bootstrap(*response, &mut out),
            Input::BootstrapFailed(error) => {
                self.bootstrap_in_flight = false;
                if error.is_unauthenticated() {
                    self.session_expired(&mut out);
                } else if self.revalidating {
                    // Cached content stays on screen. The realtime connection keeps retrying on
                    // its own, and revalidation is tried again each time it opens.
                } else {
                    let message = error.message();
                    self.set_phase(SessionPhase::Unreachable { message }, &mut out);
                }
            }
            Input::HistoryLoaded {
                conversation,
                generation,
                result,
            } => self.history_loaded(conversation, generation, result, &mut out),
            Input::SubmissionPersisted { operation } => {
                if let Some(op) = self.ops.get_mut(&operation)
                    && op.stage == OpStage::Persisting
                {
                    op.stage = OpStage::Queued;
                    let conversation = op.conversation;
                    self.pump(conversation, &mut out);
                }
            }
            Input::SubmissionPersistFailed { operation, message } => {
                if self
                    .ops
                    .get(&operation)
                    .is_some_and(|op| op.stage == OpStage::Persisting)
                {
                    let reason = format!("Couldn't save the message before sending: {message}");
                    self.fail_operation(
                        operation,
                        reason,
                        RetryKind::NewOperation,
                        false,
                        &mut out,
                    );
                }
            }
            Input::SendResult { operation, result } => {
                self.send_result(operation, result, &mut out)
            }
            Input::OperationStatusResult { operation, result } => {
                self.status_result(operation, result, &mut out)
            }
            Input::Socket(event) => self.socket_event(event, &mut out),
            Input::Frame(frame) => self.frame(frame, &mut out),
            Input::StorageProblem(message) => self.set_notice(Some(message), &mut out),
        }
        out
    }

    // ----- Session -----

    fn register(&mut self, username: &str, display_name: &str, password: String, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::SignedOut { .. }) {
            return;
        }
        let username = match Username::parse(username) {
            Ok(username) => username,
            Err(error) => return self.reject_sign_in(error.to_string(), out),
        };
        let display_name = match DisplayName::parse(display_name) {
            Ok(name) => name,
            Err(error) => return self.reject_sign_in(error.to_string(), out),
        };
        if let Some(problem) = password_problem(&password) {
            return self.reject_sign_in(problem, out);
        }
        self.set_phase(SessionPhase::SigningIn, out);
        out.effects.push(Effect::Register(RegisterRequest {
            username,
            display_name,
            password: Password::new(password),
        }));
    }

    fn sign_in(&mut self, username: &str, password: String, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::SignedOut { .. }) {
            return;
        }
        let username = match Username::parse(username) {
            Ok(username) => username,
            Err(_) if username.trim().is_empty() => {
                return self.reject_sign_in("Enter your username.".into(), out);
            }
            Err(_) => {
                return self.reject_sign_in(
                    "That isn't a valid username: use 3 to 32 letters, digits, '.', '_' or '-'."
                        .into(),
                    out,
                );
            }
        };
        if password.is_empty() {
            return self.reject_sign_in("Enter your password.".into(), out);
        }
        self.set_phase(SessionPhase::SigningIn, out);
        out.effects.push(Effect::Login(LoginRequest {
            username,
            password: Password::new(password),
        }));
    }

    fn reject_sign_in(&mut self, message: String, out: &mut Output) {
        self.set_phase(
            SessionPhase::SignedOut {
                last_error: Some(message),
            },
            out,
        );
    }

    fn authenticated(&mut self, user: User, out: &mut Output) {
        if self.me.as_ref().is_some_and(|me| me.id != user.id) {
            self.clear_session_data(out);
        }
        // Another person's saved content must never be shown to this one.
        if self.cache_owner.is_some_and(|owner| owner != user.id) {
            self.cache_owner = None;
            out.effects.push(Effect::DiscardCache);
        }
        self.users.insert(user.id, user.clone());
        self.me = Some(user);
        self.set_phase(SessionPhase::Loading, out);
        self.request_bootstrap(out);
    }

    fn sign_out(&mut self, out: &mut Output) {
        self.clear_session_data(out);
        self.drafts.clear();
        self.recovered_journal.clear();
        self.cache_owner = None;
        self.set_phase(SessionPhase::SignedOut { last_error: None }, out);
        out.effects.push(Effect::SignOut);
    }

    /// Drops everything learned from the server for the current person.
    fn clear_session_data(&mut self, out: &mut Output) {
        self.me = None;
        self.users.clear();
        self.workspaces.clear();
        self.conversations.clear();
        self.selected = None;
        self.timelines.clear();
        self.ops.clear();
        self.sockets.clear();
        self.subscribed.clear();
        self.members.clear();
        self.applied.clear();
        self.bootstrap_in_flight = false;
        self.revalidating = false;
        self.cache_dirty = false;
        self.cache_urgent = false;
        out.changes.push(Change::DirectoryChanged);
        out.changes.push(Change::SelectionChanged);
        self.recompute_connection(out);
    }

    /// The server rejected the session. Unfinished sends cannot continue until the person signs
    /// in again, so they become retryable against the same operation.
    fn session_expired(&mut self, out: &mut Output) {
        if matches!(
            self.phase,
            SessionPhase::SignedOut { .. } | SessionPhase::SigningIn
        ) {
            return;
        }
        self.bootstrap_in_flight = false;
        // Leave the ready phase first: failing one queued message must not release the next one
        // into a request that is bound to be refused.
        self.set_phase(
            SessionPhase::SignedOut {
                last_error: Some("Your session has expired. Sign in again.".into()),
            },
            out,
        );
        let unfinished: Vec<OperationId> = self
            .ops
            .iter()
            .filter(|(_, op)| op.stage != OpStage::Failed)
            .map(|(id, _)| *id)
            .collect();
        for operation in unfinished {
            let reason = "Signed out before this message was confirmed.".to_owned();
            self.fail_operation(operation, reason, RetryKind::SameOperation, false, out);
        }
        self.sockets.clear();
        self.subscribed.clear();
        self.recompute_connection(out);
        out.effects.push(Effect::ForgetSession);
    }

    fn request_bootstrap(&mut self, out: &mut Output) {
        if !self.bootstrap_in_flight {
            self.bootstrap_in_flight = true;
            out.effects.push(Effect::LoadBootstrap);
        }
    }

    fn apply_bootstrap(&mut self, response: BootstrapResponse, out: &mut Output) {
        self.bootstrap_in_flight = false;
        if response.protocol_version != PROTOCOL_VERSION {
            let message = format!(
                "This version of Lazy can't talk to the server (protocol {} vs {}). Update the app.",
                PROTOCOL_VERSION, response.protocol_version
            );
            self.revalidating = false;
            self.set_phase(
                SessionPhase::SignedOut {
                    last_error: Some(message),
                },
                out,
            );
            return;
        }

        // Cached content is kept only if it provably belongs to this person and the server's
        // log has not been rewound behind its cursor. Otherwise this is a first run.
        let revalidate = self.revalidating && self.can_revalidate(&response);
        self.revalidating = false;
        self.cache_suppressed = false;

        self.users.insert(response.user.id, response.user.clone());
        let me = response.user.id;
        self.me = Some(response.user.clone());

        self.workspaces.clear();
        self.conversations.clear();
        self.members.clear();
        for bootstrap in &response.workspaces {
            self.workspaces.push(WorkspaceInfo {
                workspace: bootstrap.workspace.clone(),
                role: bootstrap.role,
            });
            let ids = self.members.entry(bootstrap.workspace.id).or_default();
            for member in &bootstrap.members {
                self.users.insert(member.id, member.clone());
                ids.insert(member.id);
            }
            self.conversations
                .extend(bootstrap.conversations.iter().cloned());
        }
        out.changes.push(Change::DirectoryChanged);

        if revalidate {
            self.keep_cached_content(&response, out);
        } else {
            self.rebuild_from_snapshot(&response, out);
        }
        self.recompute_connection(out);

        self.recover_journal(me, out);
        self.select_default(out);

        self.set_phase(SessionPhase::Ready, out);
        self.cache_changed(true);
        // Sends that were waiting for the session (or for this bootstrap) can go now.
        self.pump_all(out);
    }

    /// Whether the content on screen came from the cache of this same person and is still
    /// consistent with the server: every workspace is known to both sides, and no cached cursor
    /// is ahead of the server's log.
    fn can_revalidate(&self, response: &BootstrapResponse) -> bool {
        self.me.as_ref().is_some_and(|me| me.id == response.user.id)
            && response.workspaces.iter().all(|w| {
                self.applied
                    .get(&w.workspace.id)
                    .is_some_and(|cursor| *cursor <= w.head)
            })
            && self.workspaces.iter().all(|known| {
                response
                    .workspaces
                    .iter()
                    .any(|w| w.workspace.id == known.workspace.id)
            })
    }

    /// Revalidation: the cached timelines stay, current to their cursors, and the subscription
    /// that began at the cached cursor fills the gap. Only conversations the person can no
    /// longer read are dropped.
    fn keep_cached_content(&mut self, response: &BootstrapResponse, out: &mut Output) {
        let readable: HashSet<ConversationId> = self.conversations.iter().map(|c| c.id).collect();
        let lost: Vec<ConversationId> = self
            .timelines
            .keys()
            .filter(|id| !readable.contains(id))
            .copied()
            .collect();
        for conversation in lost {
            let still_sending = self
                .timelines
                .get(&conversation)
                .is_some_and(|t| t.items().iter().any(|item| !item.is_confirmed()));
            if still_sending {
                if let Some(timeline) = self.timelines.get_mut(&conversation) {
                    timeline.reset_keeping_local(conversation, &mut out.changes);
                }
            } else {
                self.timelines.remove(&conversation);
                out.changes.push(Change::TimelineReset { conversation });
            }
        }
        // Subscriptions normally exist already (they started at the cached cursor). A workspace
        // without one, for example after the session was renewed, resumes from its cursor.
        for bootstrap in &response.workspaces {
            let id = bootstrap.workspace.id;
            if self.subscribed.insert(id) {
                let after = *self.applied.entry(id).or_insert(bootstrap.head);
                self.sockets.insert(id, ConnectionState::Connecting);
                out.effects.push(Effect::Subscribe {
                    workspace: id,
                    after,
                });
            }
        }
    }

    /// First-run handling: confirmed content is fetched again from the new snapshot (rows still
    /// being delivered survive), and the subscription starts after the snapshot's `head`, which
    /// misses nothing because the snapshot reflects everything up to it.
    fn rebuild_from_snapshot(&mut self, response: &BootstrapResponse, out: &mut Output) {
        let known: Vec<ConversationId> = self.timelines.keys().copied().collect();
        for conversation in known {
            if let Some(timeline) = self.timelines.get_mut(&conversation) {
                timeline.reset_keeping_local(conversation, &mut out.changes);
            }
        }

        self.applied.clear();
        self.subscribed.clear();
        let previous_sockets = std::mem::take(&mut self.sockets);
        for bootstrap in &response.workspaces {
            let id = bootstrap.workspace.id;
            self.applied.insert(id, bootstrap.head);
            self.subscribed.insert(id);
            let state = match previous_sockets.get(&id) {
                Some(ConnectionState::Live | ConnectionState::CatchingUp) => {
                    ConnectionState::CatchingUp
                }
                _ => ConnectionState::Connecting,
            };
            self.sockets.insert(id, state);
            out.effects.push(Effect::Subscribe {
                workspace: id,
                after: bootstrap.head,
            });
        }
    }

    /// Selects `#general` (or the first conversation) if nothing valid is selected, and makes
    /// sure the selected conversation has history, which a cached timeline already does.
    fn select_default(&mut self, out: &mut Output) {
        let selection_valid = self
            .selected
            .is_some_and(|id| self.conversation(id).is_some());
        if !selection_valid {
            let next = self
                .conversations
                .iter()
                .find(|c| c.name.as_str() == "general")
                .or_else(|| self.conversations.first())
                .map(|c| c.id);
            if next != self.selected {
                self.selected = next;
                out.changes.push(Change::SelectionChanged);
            }
        }
        if let Some(selected) = self.selected {
            self.ensure_history(selected, out);
        }
    }

    // ----- Content cache -----

    /// Starts from the saved content cache: the directory and timelines appear immediately, the
    /// subscription resumes after the cached cursor, and a bootstrap revalidates everything in
    /// the background. Nothing is refetched for a cached conversation.
    fn restore_from_cache(&mut self, cache: ContentCache, out: &mut Output) {
        let me = cache.user;
        self.users.insert(me.id, me.clone());
        self.me = Some(me.clone());
        for cached in cache.workspaces {
            let CachedWorkspace {
                workspace,
                role,
                conversations,
                members,
                cursor,
                timelines,
            } = cached;
            let id = workspace.id;
            self.workspaces.push(WorkspaceInfo { workspace, role });
            let ids = self.members.entry(id).or_default();
            for member in members {
                ids.insert(member.id);
                self.users.insert(member.id, member);
            }
            self.conversations.extend(conversations);
            self.applied.insert(id, cursor);
            self.subscribed.insert(id);
            self.sockets.insert(id, ConnectionState::Connecting);
            out.effects.push(Effect::Subscribe {
                workspace: id,
                after: cursor,
            });
            for cached_timeline in timelines {
                let conversation = cached_timeline.conversation;
                self.timelines
                    .insert(conversation, Timeline::from_cache(cached_timeline, cursor));
                out.changes.push(Change::TimelineReset { conversation });
            }
        }
        out.changes.push(Change::DirectoryChanged);
        self.recompute_connection(out);
        self.revalidating = true;
        // Journal recovery needs no network: the person is known from the cache, and the
        // recovered submissions take their place at the front of the send queue before anything
        // typed from now on.
        self.recover_journal(me.id, out);
        self.select_default(out);
        self.set_phase(SessionPhase::Ready, out);
        self.request_bootstrap(out);
    }

    /// Builds the cache from the current state, or `None` when there is nothing trustworthy to
    /// save. Only conversations whose history was loaded are included (see
    /// [`ContentCache`]'s invariant), and the cursor saved with them is the position the
    /// timelines are current to.
    fn build_cache(&self) -> Option<ContentCache> {
        let me = self.me.as_ref()?;
        let workspaces: Vec<CachedWorkspace> = self
            .workspaces
            .iter()
            .filter_map(|info| {
                let id = info.workspace.id;
                let cursor = *self.applied.get(&id)?;
                let conversations: Vec<Conversation> = self
                    .conversations
                    .iter()
                    .filter(|c| c.workspace_id == id)
                    .cloned()
                    .collect();
                let members = self
                    .members
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .filter_map(|member| self.users.get(member).cloned())
                    .collect();
                let timelines = conversations
                    .iter()
                    .filter_map(|c| self.timelines.get(&c.id)?.to_cache(c.id))
                    .collect();
                Some(CachedWorkspace {
                    workspace: info.workspace.clone(),
                    role: info.role,
                    conversations,
                    members,
                    cursor,
                    timelines,
                })
            })
            .collect();
        if workspaces.is_empty() {
            return None;
        }
        Some(
            ContentCache {
                user: me.clone(),
                workspaces,
            }
            .bounded(),
        )
    }

    /// Emits one `PersistCache` if anything the cache holds changed during this `handle` call.
    fn flush_cache(&mut self, out: &mut Output) {
        if !self.cache_dirty {
            return;
        }
        let urgent = std::mem::take(&mut self.cache_urgent);
        self.cache_dirty = false;
        if self.cache_suppressed || !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        if !urgent && self.cache_skipped < self.cache_size / CACHE_MESSAGES_PER_SKIPPED_WRITE {
            self.cache_skipped += 1;
            return;
        }
        if let Some(cache) = self.build_cache() {
            self.cache_owner = Some(cache.user.id);
            self.cache_skipped = 0;
            self.cache_size = cache.message_count();
            out.effects.push(Effect::PersistCache(Box::new(cache)));
        }
    }

    /// Records that something the cache holds has changed. `urgent` changes are saved at once;
    /// the rest (live messages and the cursor moving) may be batched on a big cache.
    fn cache_changed(&mut self, urgent: bool) {
        self.cache_dirty = true;
        self.cache_urgent |= urgent;
    }

    /// The server asked for a fresh snapshot (the cursor expired or is ahead of its log). The
    /// content on hand and the saved cache can no longer be trusted: discard both and start over
    /// as on a first run.
    fn snapshot_required(&mut self, out: &mut Output) {
        self.revalidating = false;
        self.cache_suppressed = true;
        self.request_bootstrap(out);
        if self.cache_owner.take().is_some() {
            out.effects.push(Effect::DiscardCache);
        }
        let known: Vec<ConversationId> = self.timelines.keys().copied().collect();
        for conversation in known {
            if let Some(timeline) = self.timelines.get_mut(&conversation) {
                timeline.reset_keeping_local(conversation, &mut out.changes);
            }
        }
    }

    /// Shows journaled submissions from a previous run as unknown outcomes and asks the server
    /// about each one before anything is resent.
    fn recover_journal(&mut self, me: UserId, out: &mut Output) {
        let (mine, others): (Vec<_>, Vec<_>) = std::mem::take(&mut self.recovered_journal)
            .into_iter()
            .partition(|e| e.user_id == me);
        self.recovered_journal = others;
        // Recovered submissions go to the front of the queue, in the order they were made (the
        // sort is stable, so entries with equal times keep their journal order).
        let mut mine = mine;
        mine.sort_by_key(|entry| entry.submitted_at);
        for entry in mine {
            if self.ops.contains_key(&entry.operation_id) {
                continue;
            }
            let operation = entry.operation_id;
            let order = self.take_order();
            self.ops.insert(
                operation,
                LocalOp {
                    conversation: entry.conversation_id,
                    document: entry.document.clone(),
                    stage: OpStage::Querying,
                    resends: 0,
                    order,
                },
            );
            self.timeline_mut(entry.conversation_id).push_local(
                entry.conversation_id,
                TimelineItem {
                    key: ItemKey::Operation(operation),
                    author_id: me,
                    document: entry.document,
                    delivery: Delivery::UnknownOutcome,
                },
                &mut out.changes,
            );
            out.effects.push(Effect::QueryOperationStatus { operation });
        }
    }

    // ----- Conversations and drafts -----

    fn select_conversation(&mut self, id: ConversationId, out: &mut Output) {
        if self.conversation(id).is_none() {
            return;
        }
        if self.selected != Some(id) {
            self.selected = Some(id);
            out.changes.push(Change::SelectionChanged);
        }
        self.ensure_history(id, out);
    }

    /// Fetches the newest page once, and again after a failure.
    fn ensure_history(&mut self, conversation: ConversationId, out: &mut Output) {
        let timeline = self.timeline_mut(conversation);
        match timeline.history {
            HistoryStatus::NotLoaded | HistoryStatus::Failed { .. } => {
                let before = timeline.older_cursor();
                self.request_history(conversation, before, out);
            }
            HistoryStatus::Loading | HistoryStatus::Ready { .. } => {}
        }
    }

    fn load_older(&mut self, conversation: ConversationId, out: &mut Output) {
        let Some(timeline) = self.timelines.get(&conversation) else {
            return;
        };
        match timeline.history {
            HistoryStatus::Ready { has_more: true } | HistoryStatus::Failed { .. } => {
                let before = timeline.older_cursor();
                self.request_history(conversation, before, out);
            }
            HistoryStatus::NotLoaded => self.ensure_history(conversation, out),
            HistoryStatus::Ready { has_more: false } | HistoryStatus::Loading => {}
        }
    }

    fn request_history(
        &mut self,
        conversation: ConversationId,
        before: Option<EventSeq>,
        out: &mut Output,
    ) {
        let timeline = self.timeline_mut(conversation);
        timeline.generation += 1;
        timeline.history = HistoryStatus::Loading;
        out.effects.push(Effect::FetchHistory {
            conversation,
            before,
            limit: Some(HISTORY_PAGE_LIMIT),
            generation: timeline.generation,
        });
        out.changes
            .push(Change::HistoryStatusChanged { conversation });
    }

    fn history_loaded(
        &mut self,
        conversation: ConversationId,
        generation: u64,
        result: Result<lazy_protocol::HistoryPage, RequestError>,
        out: &mut Output,
    ) {
        // A response belongs to the conversation it was requested for, whatever is selected now,
        // and only the latest request for that conversation counts.
        let Some(timeline) = self.timelines.get_mut(&conversation) else {
            return;
        };
        if timeline.generation != generation || timeline.history != HistoryStatus::Loading {
            return;
        }
        match result {
            Ok(page) => {
                let belongs: Vec<Message> = page
                    .messages
                    .into_iter()
                    .filter(|m| m.conversation_id == conversation)
                    .collect();
                timeline.merge_messages(conversation, &belongs, &mut out.changes);
                timeline.note_page(belongs.iter().map(|m| m.seq).min(), page.has_more);
                timeline.history = HistoryStatus::Ready {
                    has_more: page.has_more,
                };
                out.changes
                    .push(Change::HistoryStatusChanged { conversation });
                self.cache_changed(true);
            }
            Err(error) => {
                timeline.history = HistoryStatus::Failed {
                    message: error.message(),
                };
                out.changes
                    .push(Change::HistoryStatusChanged { conversation });
                if error.is_unauthenticated() {
                    self.session_expired(out);
                }
            }
        }
    }

    fn set_draft(&mut self, conversation: ConversationId, text: String, out: &mut Output) {
        if self.draft(conversation) == text {
            return;
        }
        if text.is_empty() {
            self.drafts.remove(&conversation);
        } else {
            self.drafts.insert(conversation, text);
        }
        out.changes.push(Change::DraftChanged { conversation });
        self.persist_drafts(out);
    }

    fn persist_drafts(&self, out: &mut Output) {
        let mut drafts: Vec<(ConversationId, String)> = self
            .drafts
            .iter()
            .map(|(id, text)| (*id, text.clone()))
            .collect();
        drafts.sort_by_key(|(id, _)| *id);
        out.effects.push(Effect::PersistDrafts(drafts));
    }

    // ----- Sending -----

    fn send_message(&mut self, conversation: ConversationId, text: &str, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) || self.conversation(conversation).is_none() {
            return;
        }
        let document = match MessageDocument::from_plain_text(text) {
            Ok(document) => document,
            Err(DocumentError::Empty) => return,
            Err(error) => {
                self.set_notice(Some(format!("Message not sent: {error}.")), out);
                return;
            }
        };
        self.submit(conversation, document, out);
        if self.drafts.remove(&conversation).is_some() {
            out.changes.push(Change::DraftChanged { conversation });
            self.persist_drafts(out);
        }
    }

    /// Creates a new operation: a pending row, then a durable journal write. The send itself
    /// waits for the write to be reported durable.
    fn submit(
        &mut self,
        conversation: ConversationId,
        document: MessageDocument,
        out: &mut Output,
    ) {
        let Some(me) = self.me.as_ref().map(|user| user.id) else {
            return;
        };
        let operation = OperationId::generate();
        let order = self.take_order();
        self.ops.insert(
            operation,
            LocalOp {
                conversation,
                document: document.clone(),
                stage: OpStage::Persisting,
                resends: 0,
                order,
            },
        );
        self.timeline_mut(conversation).push_local(
            conversation,
            TimelineItem {
                key: ItemKey::Operation(operation),
                author_id: me,
                document: document.clone(),
                delivery: Delivery::Pending,
            },
            &mut out.changes,
        );
        out.effects.push(Effect::PersistSubmission(JournalEntry {
            operation_id: operation,
            user_id: me,
            conversation_id: conversation,
            document,
            submitted_at: Timestamp::now(),
        }));
    }

    fn send_result(
        &mut self,
        operation: OperationId,
        result: Result<lazy_protocol::SendMessageResponse, RequestError>,
        out: &mut Output,
    ) {
        if !self
            .ops
            .get(&operation)
            .is_some_and(|op| op.stage == OpStage::Sending)
        {
            // Already settled by the live event, or superseded.
            return;
        }
        match result {
            Ok(response) => self.confirm_operation(operation, &response.message, out),
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(RequestError::Rejected(error)) => {
                self.fail_operation(operation, error.message, RetryKind::NewOperation, true, out)
            }
            Err(RequestError::Unknown(_)) => self.begin_status_query(operation, out),
        }
    }

    fn status_result(
        &mut self,
        operation: OperationId,
        result: Result<OperationStatus, RequestError>,
        out: &mut Output,
    ) {
        if !self
            .ops
            .get(&operation)
            .is_some_and(|op| op.stage == OpStage::Querying)
        {
            return;
        }
        match result {
            Ok(OperationStatus::Committed { message }) => {
                self.confirm_operation(operation, &message, out)
            }
            Ok(OperationStatus::NotFound) => {
                let Some(op) = self.ops.get_mut(&operation) else {
                    return;
                };
                if op.resends >= MAX_RESENDS {
                    let reason = "The server never confirmed this message.".to_owned();
                    self.fail_operation(operation, reason, RetryKind::SameOperation, false, out);
                    return;
                }
                // The server has no record, and resending the same operation is idempotent. It
                // goes back in the queue and is sent when it is its turn, which is now unless an
                // earlier message is still unresolved.
                op.resends += 1;
                op.stage = OpStage::Queued;
                let conversation = op.conversation;
                self.set_item_delivery(conversation, operation, Delivery::Pending, out);
                self.pump(conversation, out);
            }
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(error) => {
                let reason = match error {
                    RequestError::Rejected(error) => error.message,
                    RequestError::Unknown(_) => {
                        "Couldn't confirm whether this message was sent.".to_owned()
                    }
                };
                self.fail_operation(operation, reason, RetryKind::SameOperation, false, out);
            }
        }
    }

    fn begin_status_query(&mut self, operation: OperationId, out: &mut Output) {
        let Some(op) = self.ops.get_mut(&operation) else {
            return;
        };
        op.stage = OpStage::Querying;
        let conversation = op.conversation;
        self.set_item_delivery(conversation, operation, Delivery::UnknownOutcome, out);
        out.effects.push(Effect::QueryOperationStatus { operation });
    }

    fn retry_operation(
        &mut self,
        conversation: ConversationId,
        operation: OperationId,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let Some(timeline) = self.timelines.get(&conversation) else {
            return;
        };
        let Some(index) = timeline.index_of(ItemKey::Operation(operation)) else {
            return;
        };
        let item = &timeline.items()[index];
        let Delivery::Failed { retry, .. } = item.delivery else {
            return;
        };
        match retry {
            RetryKind::SameOperation => {
                let order = self.next_order;
                if let Some(op) = self.ops.get_mut(&operation)
                    && op.stage == OpStage::Failed
                {
                    // The earlier attempt was given up on and later messages may have gone out
                    // since, so this one rejoins at the back of the queue, in the timeline too.
                    op.resends = 0;
                    op.order = order;
                    self.next_order += 1;
                    self.timeline_mut(conversation).move_to_back(
                        conversation,
                        ItemKey::Operation(operation),
                        &mut out.changes,
                    );
                    self.begin_status_query(operation, out);
                }
            }
            RetryKind::NewOperation => {
                let document = item.document.clone();
                self.timeline_mut(conversation).remove(
                    conversation,
                    ItemKey::Operation(operation),
                    &mut out.changes,
                );
                self.submit(conversation, document, out);
            }
        }
    }

    /// The operation committed as `message`: the row becomes confirmed and the journal entry is
    /// released. Safe to call more than once for the same operation.
    fn confirm_operation(&mut self, operation: OperationId, message: &Message, out: &mut Output) {
        let Some(op) = self.ops.get(&operation) else {
            return;
        };
        let conversation = op.conversation;
        if message.conversation_id != conversation {
            return;
        }
        self.timeline_mut(conversation).confirm_local(
            conversation,
            operation,
            message,
            &mut out.changes,
        );
        self.cache_changed(false);
        self.settle_operation(operation, out);
    }

    /// Marks the row failed. `settle` releases the journal entry, which is right only when the
    /// server definitively did not commit the operation.
    fn fail_operation(
        &mut self,
        operation: OperationId,
        reason: String,
        retry: RetryKind,
        settle: bool,
        out: &mut Output,
    ) {
        let Some(op) = self.ops.get_mut(&operation) else {
            return;
        };
        op.stage = OpStage::Failed;
        let conversation = op.conversation;
        self.set_item_delivery(
            conversation,
            operation,
            Delivery::Failed { reason, retry },
            out,
        );
        let released = settle || retry == RetryKind::NewOperation;
        if released {
            self.settle_operation(operation, out);
        }
        // A failed message no longer holds up the ones behind it.
        self.pump(conversation, out);
    }

    fn settle_operation(&mut self, operation: OperationId, out: &mut Output) {
        if let Some(op) = self.ops.remove(&operation) {
            out.effects.push(Effect::SettleSubmission { operation });
            self.pump(op.conversation, out);
        }
    }

    fn take_order(&mut self) -> u64 {
        let order = self.next_order;
        self.next_order += 1;
        order
    }

    /// Sends the next message of a conversation if nothing earlier is unresolved.
    ///
    /// Messages in a conversation go out one at a time, in the order they were submitted, so the
    /// server commits them in that order. The head of the queue is the earliest operation that is
    /// not `Failed`. If it is `Queued` it is sent; if it is still being journaled, in flight, or
    /// being checked with the server, everything behind it waits. A failed message (definitively
    /// rejected, or given up on with an unknown outcome) does not hold anything up.
    fn pump(&mut self, conversation: ConversationId, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let head = self
            .ops
            .iter()
            .filter(|(_, op)| op.conversation == conversation && op.stage != OpStage::Failed)
            .min_by_key(|(_, op)| op.order)
            .map(|(id, _)| *id);
        let Some(operation) = head else {
            return;
        };
        if let Some(op) = self.ops.get_mut(&operation)
            && op.stage == OpStage::Queued
        {
            op.stage = OpStage::Sending;
            out.effects.push(Effect::SendMessage {
                operation,
                conversation,
                document: op.document.clone(),
            });
        }
    }

    fn pump_all(&mut self, out: &mut Output) {
        let mut conversations: Vec<ConversationId> =
            self.ops.values().map(|op| op.conversation).collect();
        conversations.sort();
        conversations.dedup();
        for conversation in conversations {
            self.pump(conversation, out);
        }
    }

    fn set_item_delivery(
        &mut self,
        conversation: ConversationId,
        operation: OperationId,
        delivery: Delivery,
        out: &mut Output,
    ) {
        self.timeline_mut(conversation).set_delivery(
            conversation,
            ItemKey::Operation(operation),
            delivery,
            &mut out.changes,
        );
    }

    // ----- Realtime -----

    fn frame(&mut self, frame: ServerFrame, out: &mut Output) {
        match frame {
            ServerFrame::Event(envelope) => self.apply_event(envelope, out),
            ServerFrame::CaughtUp { workspace_id, seq } => {
                if let Some(state) = self.sockets.get_mut(&workspace_id) {
                    *state = ConnectionState::Live;
                    self.recompute_connection(out);
                    self.advance_cursor(workspace_id, seq);
                    self.retry_revalidation(out);
                }
            }
            ServerFrame::SnapshotRequired { .. } => self.snapshot_required(out),
            ServerFrame::Error { code, message } => {
                if code == lazy_protocol::ErrorCode::Unauthenticated {
                    self.session_expired(out);
                } else {
                    self.set_notice(Some(message), out);
                }
            }
        }
    }

    fn apply_event(&mut self, envelope: EventEnvelope, out: &mut Output) {
        let workspace = envelope.workspace_id;
        let Some(applied) = self.applied.get(&workspace).copied() else {
            return;
        };
        if envelope.seq <= applied {
            return;
        }
        match envelope.event {
            Event::MessageCreated {
                message,
                operation_id,
            } => self.apply_message(message, operation_id, out),
            Event::MemberJoined { user } => {
                self.members.entry(workspace).or_default().insert(user.id);
                self.users.insert(user.id, user);
                out.changes.push(Change::DirectoryChanged);
                self.cache_changed(true);
            }
        }
        self.advance_cursor(workspace, envelope.seq);
    }

    fn apply_message(
        &mut self,
        message: Message,
        operation: Option<OperationId>,
        out: &mut Output,
    ) {
        let own = self
            .me
            .as_ref()
            .is_some_and(|me| me.id == message.author_id);
        if let Some(operation) = operation
            && own
            && self.ops.contains_key(&operation)
        {
            self.confirm_operation(operation, &message, out);
            return;
        }
        let conversation = message.conversation_id;
        let added = self.timeline_mut(conversation).merge_messages(
            conversation,
            std::slice::from_ref(&message),
            &mut out.changes,
        );
        if added > 0 {
            self.cache_changed(false);
        }
    }

    fn advance_cursor(&mut self, workspace: WorkspaceId, seq: EventSeq) {
        let applied = self.applied.entry(workspace).or_insert(EventSeq::ZERO);
        if seq > *applied {
            *applied = seq;
            self.cache_changed(false);
        }
    }

    /// While cached content is on screen but unconfirmed, a live connection means the network is
    /// back: ask for the bootstrap again (a no-op if one is already in flight).
    fn retry_revalidation(&mut self, out: &mut Output) {
        if self.revalidating {
            self.request_bootstrap(out);
        }
    }

    fn socket_event(&mut self, event: SocketEvent, out: &mut Output) {
        let (workspace, state) = match event {
            SocketEvent::Connecting {
                workspace,
                attempt: 0,
            } => (workspace, ConnectionState::Connecting),
            SocketEvent::Connecting { workspace, attempt } => {
                (workspace, ConnectionState::Reconnecting { attempt })
            }
            SocketEvent::Opened { workspace } => (workspace, ConnectionState::CatchingUp),
            SocketEvent::Closed { workspace, attempt } => (
                workspace,
                ConnectionState::Reconnecting {
                    attempt: attempt.max(1),
                },
            ),
        };
        if let Some(slot) = self.sockets.get_mut(&workspace) {
            *slot = state;
            self.recompute_connection(out);
            if matches!(event, SocketEvent::Opened { .. }) {
                self.retry_revalidation(out);
            }
        }
    }

    fn recompute_connection(&mut self, out: &mut Output) {
        // The aggregate is the least healthy workspace connection.
        fn badness(state: ConnectionState) -> (u8, u32) {
            match state {
                ConnectionState::Live => (0, 0),
                ConnectionState::CatchingUp => (1, 0),
                ConnectionState::Connecting => (2, 0),
                ConnectionState::Reconnecting { attempt } => (3, attempt),
                ConnectionState::Offline => (4, 0),
            }
        }
        let next = self
            .sockets
            .values()
            .copied()
            .max_by_key(|state| badness(*state))
            .unwrap_or(ConnectionState::Offline);
        if next != self.connection {
            self.connection = next;
            out.changes.push(Change::ConnectionChanged);
        }
    }

    // ----- Helpers -----

    fn timeline_mut(&mut self, conversation: ConversationId) -> &mut Timeline {
        self.timelines
            .entry(conversation)
            .or_insert_with(Timeline::new)
    }

    fn set_phase(&mut self, phase: SessionPhase, out: &mut Output) {
        if self.phase != phase {
            self.phase = phase;
            out.changes.push(Change::PhaseChanged);
        }
    }

    fn set_notice(&mut self, notice: Option<String>, out: &mut Output) {
        if self.notice != notice {
            self.notice = notice;
            out.changes.push(Change::NoticeChanged);
        }
    }
}

fn password_problem(password: &str) -> Option<String> {
    if password.len() < MIN_PASSWORD_BYTES {
        Some(format!(
            "Password must be at least {MIN_PASSWORD_BYTES} characters."
        ))
    } else if password.len() > MAX_PASSWORD_BYTES {
        Some(format!(
            "Password must be at most {MAX_PASSWORD_BYTES} characters."
        ))
    } else {
        None
    }
}
