//! The client state machine.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use lazy_domain::{
    AttachmentId, ConversationId, DisplayName, DocumentError, EventSeq, MessageDocument, MessageId,
    OperationId, Permalink, ReactionEmoji, Timestamp, UserId, Username, WorkspaceId,
    workspace::{canonical_invite_code, invite_code_is_wellformed},
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, Event, EventEnvelope, InviteCode,
    LoginRequest, Mentions, Message, OperationStatus, PROTOCOL_VERSION, Password, RegisterRequest,
    ServerFrame, ThreadUserState, User, UserGroup,
};

use crate::attachments::ComposerTarget;
use crate::cache::{CachedThreadState, CachedUserState, CachedWorkspace, ContentCache};
use crate::change::{Change, Output};
use crate::effect::Effect;
use crate::input::{Input, SocketEvent};
use crate::model::{
    ActionKind, ActionState, ConnectionState, Delivery, EditState, EmojiPrefs, HistoryStatus,
    ItemKey, JournalEntry, JournalKind, JumpTarget, RequestError, RetryKind, SessionPhase,
    StartupState, TimelineItem, UNKNOWN_MEMBER_NAME, WorkspaceInfo,
};
use crate::server::ServerInfo;
use crate::timeline::{DeletedRoot, Timeline};
use threads::Home;

mod admin;
mod attachments;
mod conversations;
mod diagnostics;
mod discovery;
mod emoji;
mod groups;
mod inbox;
mod mentions;
mod messages;
mod notify;
mod onboarding;
mod permalink;
mod reactions;
mod readstate;
mod search;
mod server;
mod switcher;
mod threads;
mod view;
mod webhooks;
mod workspaces;

use threads::local_row;

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
    /// A file of the message is still being copied into the outbox; the journal entry waits for
    /// the copy so it never names a file that is not there.
    Staging,
    /// Waiting for the journal write to become durable.
    Persisting,
    /// Journaled, and waiting for the message's files to reach the server.
    Uploading,
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
    /// For a reply: the thread it goes to.
    thread: Option<threads::ReplyTo>,
    /// When the person sent it, which is the order a restart recovers messages in.
    submitted_at: Timestamp,
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

    // ----- Conversations and read state -----
    /// The person's favorite flag and read position for each conversation they belong to.
    user_states: HashMap<ConversationId, ConversationUserState>,
    /// The log position each state is current to. Message events at or before it are already
    /// reflected in the state and are not counted again.
    state_as_of: HashMap<ConversationId, EventSeq>,
    /// Where the "New messages" divider goes: the read position captured when the conversation
    /// was opened, kept until the person leaves it.
    marker_anchor: HashMap<ConversationId, EventSeq>,
    actions: HashMap<ActionKind, ActionState>,
    /// Channel creations and conversation openings whose outcome is not settled, by operation.
    commands: HashMap<OperationId, conversations::PendingCommand>,
    window_active: bool,
    /// Conversations whose newest message the timeline view reports as on screen.
    at_bottom: HashSet<ConversationId>,
    /// The position of the mark-read request in flight, per conversation.
    mark_in_flight: HashMap<ConversationId, EventSeq>,
    /// The position of a mark-read request that failed; not retried for that position until
    /// something changes.
    mark_failed: HashMap<ConversationId, EventSeq>,
    /// Conversations the person asked to leave, so their removal is not reported as a surprise.
    leaving: HashSet<ConversationId>,

    // ----- Editing, deleting, permalinks -----
    /// Edits that have not reached a final state, by message.
    edits: HashMap<MessageId, messages::EditEntry>,
    /// The message each unresolved edit operation belongs to.
    edit_ops: HashMap<OperationId, MessageId>,
    /// Edit operations dropped while their journal write was still outstanding. Their entry is
    /// released once the write is reported, never before.
    abandoned_persists: HashSet<OperationId>,
    /// The permalink being opened, if any.
    permalink: Option<permalink::Job>,
    /// The message the view should scroll to and highlight.
    jump: Option<JumpTarget>,
    /// Positions of messages counted as unread from live events, per conversation, that the
    /// state's position does not yet cover (see `count_message_deleted`).
    counted_unread: HashMap<ConversationId, BTreeMap<EventSeq, bool>>,
    /// Per conversation, the position up to which the unread count was last rebuilt from the
    /// timeline's rows. At or before it, a message is counted only if it has an entry in
    /// `counted_unread`: the rows are the count, and a message that has none (it was deleted
    /// before the page was fetched) is not in it. Forgotten whenever a server state replaces the
    /// count.
    rebuilt_through: HashMap<ConversationId, EventSeq>,
    /// The workspace's user groups, ordered by handle.
    groups: Vec<UserGroup>,
    /// Who is in each channel the person has looked at, fetched on selection.
    member_lists: HashMap<ConversationId, groups::MemberList>,
    /// Source of `MemberList::generation`.
    next_members_fetch: u64,
    /// The person's reaction changes that are not yet saved, by message and emoji.
    reaction_toggles: HashMap<(MessageId, ReactionEmoji), reactions::Toggle>,
    /// Source of `Toggle::order`, so pending changes are shown in the order they were made.
    next_toggle: u64,
    /// The skin tone and recent emoji of this profile.
    emoji_prefs: EmojiPrefs,
    /// A channel the person could join to read the message of the last permalink they opened.
    join_offer: Option<(ConversationId, Permalink)>,

    // ----- Threads -----
    /// The open thread pane.
    pane: Option<threads::Pane>,
    /// Replies of threads that are not open but still have rows being sent (or failed), by root.
    parked_replies: HashMap<MessageId, Timeline>,
    /// Reply composer texts, with the conversation each thread belongs to.
    thread_drafts: HashMap<MessageId, (ConversationId, String)>,
    /// The person's state for the threads the client holds one for.
    thread_states: HashMap<MessageId, ThreadUserState>,
    /// The log position each thread state is current to (see `state_as_of`).
    thread_as_of: HashMap<MessageId, EventSeq>,
    /// Replies counted as unread from live events, with whether each mentions the person (see
    /// `counted_unread`).
    thread_counted: HashMap<MessageId, BTreeMap<EventSeq, bool>>,
    /// See `rebuilt_through`.
    thread_rebuilt: HashMap<MessageId, EventSeq>,
    /// Open threads whose newest reply the view reports as on screen.
    thread_at_bottom: HashSet<MessageId>,
    /// The position of the thread mark-read request in flight, per thread.
    thread_mark_in_flight: HashMap<MessageId, EventSeq>,
    /// The position of a thread mark-read request that failed.
    thread_mark_failed: HashMap<MessageId, EventSeq>,
    /// Follow changes that are not yet saved, by thread.
    follows: HashMap<MessageId, threads::Follow>,
    /// Sends, replies, and edits waiting for a channel's people (see `must_wait_for_members`).
    held_text: Vec<Input>,
    /// Source of the generation of replies requests, so an answer to a request made for a pane
    /// that has since been closed is never taken for the answer to a later one.
    next_replies_fetch: u64,

    // ----- Webhooks and what the person was looking at -----
    /// The webhooks of each conversation, as last loaded.
    webhook_lists: HashMap<ConversationId, Vec<lazy_protocol::Webhook>>,
    /// The webhook just created, with its one-time address. Never cached or saved.
    created_webhook: Option<crate::model::CreatedWebhook>,
    /// Where the caret and selection of each conversation's draft were, as reported.
    draft_selections: HashMap<ConversationId, crate::model::TextSelection>,
    /// The same for reply drafts.
    thread_selections: HashMap<MessageId, crate::model::TextSelection>,
    /// The conversation last open in each workspace.
    last_selected: HashMap<WorkspaceId, ConversationId>,

    // ----- Attachments -----
    files: attachments::Files,

    // ----- Custom emoji -----
    emojis: emoji::Emojis,

    // ----- Search -----
    searching: search::Searching,

    // ----- Discovery -----
    discovery: discovery::Discovery,

    // ----- Notifications and the thread inbox -----
    notif: notify::Notifications,
    inboxing: inbox::Inboxing,

    // ----- Workspace administration -----
    admin: admin::Administration,

    // ----- Several workspaces, profile, appearance -----
    appearance: crate::workspaces::Appearance,
    /// The last few errors, by category and code only (see `diagnostics`).
    recent_errors: diagnostics::RecentErrors,
    /// The server in use, and what is known about it (slice: dogfood packaging).
    server: ServerInfo,
    server_job: Option<server::ServerJob>,
    server_generation: u64,
    /// What the last check of an address heard about taking accounts, with the address it heard
    /// it from (D29).
    facts: Option<(String, crate::server::ServerFacts)>,
    /// What is known about the invitation code last asked about, and which question that is.
    invitation: crate::onboarding::InvitationCheck,
    invitation_generation: u64,
    /// A bootstrap has been applied in this run, so the person's notification settings are the
    /// server's (the cache does not hold them).
    settings_known: bool,
    /// The workspace the screen shows.
    active: Option<WorkspaceId>,
    /// The workspace whose scope is loaded in the fields above (the active one, except while an
    /// event or answer of another is being applied).
    scope: Option<WorkspaceId>,
    /// The scopes of the other workspaces.
    parked: HashMap<WorkspaceId, workspaces::WorkspaceScope>,
    /// The workspace each action in flight was started in, so its answer is applied there.
    action_scope: HashMap<ActionKind, WorkspaceId>,
    avatars: workspaces::Avatars,
    /// The workspace that was active when the app last ran.
    preferred_active: Option<WorkspaceId>,
    creating: Option<workspaces::CreateJob>,
    /// What the rail last showed (see `sync_rail`).
    last_rail: Vec<(WorkspaceId, String, u32, bool, ConnectionState, bool)>,
}

impl Default for ClientCore {
    fn default() -> Self {
        Self::new()
    }
}

/// Said when a person signs in while an address they typed has not been checked and moved to.
const SERVER_UNSETTLED: &str = "Check the server address first.";

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
            user_states: HashMap::new(),
            state_as_of: HashMap::new(),
            marker_anchor: HashMap::new(),
            actions: HashMap::new(),
            commands: HashMap::new(),
            window_active: true,
            at_bottom: HashSet::new(),
            mark_in_flight: HashMap::new(),
            mark_failed: HashMap::new(),
            leaving: HashSet::new(),
            edits: HashMap::new(),
            edit_ops: HashMap::new(),
            abandoned_persists: HashSet::new(),
            counted_unread: HashMap::new(),
            rebuilt_through: HashMap::new(),
            groups: Vec::new(),
            member_lists: HashMap::new(),
            next_members_fetch: 0,
            reaction_toggles: HashMap::new(),
            next_toggle: 0,
            emoji_prefs: EmojiPrefs::default(),
            permalink: None,
            jump: None,
            join_offer: None,
            pane: None,
            parked_replies: HashMap::new(),
            thread_drafts: HashMap::new(),
            thread_states: HashMap::new(),
            thread_as_of: HashMap::new(),
            thread_counted: HashMap::new(),
            thread_rebuilt: HashMap::new(),
            thread_at_bottom: HashSet::new(),
            thread_mark_in_flight: HashMap::new(),
            thread_mark_failed: HashMap::new(),
            follows: HashMap::new(),
            held_text: Vec::new(),
            next_replies_fetch: 0,
            webhook_lists: HashMap::new(),
            created_webhook: None,
            draft_selections: HashMap::new(),
            thread_selections: HashMap::new(),
            last_selected: HashMap::new(),
            files: attachments::Files::default(),
            emojis: emoji::Emojis::default(),
            searching: search::Searching::default(),
            discovery: discovery::Discovery::default(),
            notif: notify::Notifications::default(),
            inboxing: inbox::Inboxing::default(),
            admin: admin::Administration::default(),
            appearance: crate::workspaces::Appearance::default(),
            recent_errors: diagnostics::RecentErrors::new(),
            server: ServerInfo::default(),
            server_job: None,
            server_generation: 0,
            facts: None,
            invitation: crate::onboarding::InvitationCheck::default(),
            invitation_generation: 0,
            settings_known: false,
            active: None,
            scope: None,
            parked: HashMap::new(),
            action_scope: HashMap::new(),
            avatars: workspaces::Avatars::default(),
            preferred_active: None,
            creating: None,
            last_rail: Vec::new(),
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

    /// The active workspace's entry.
    pub fn primary_workspace(&self) -> Option<&WorkspaceInfo> {
        let scope = self.scope?;
        self.workspaces
            .iter()
            .find(|info| info.workspace.id == scope)
    }

    /// Conversations the person can read in the active workspace.
    pub fn conversations(&self) -> &[Conversation] {
        &self.conversations
    }

    /// A conversation of any workspace the person belongs to.
    pub fn conversation(&self, id: ConversationId) -> Option<&Conversation> {
        self.find_conversation(id)
    }

    pub fn selected_conversation(&self) -> Option<ConversationId> {
        self.selected
    }

    /// A person of the active workspace, as that workspace knows them (their status and whether
    /// they are deactivated are its own). Nobody else: a person of another workspace only is not
    /// found here, though `display_name` still names them.
    pub fn member(&self, id: UserId) -> Option<&User> {
        self.users.get(&id)
    }

    /// A person's display name, or [`UNKNOWN_MEMBER_NAME`] when the profile is not known. Names
    /// are the person's own, the same in every workspace, so any workspace's copy answers.
    pub fn display_name(&self, id: UserId) -> &str {
        self.find_user(id)
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

    /// The message to scroll to and highlight after `Input::OpenPermalink`. Cleared by
    /// `Input::ClearJumpTarget`, by leaving the conversation, or when the message goes away.
    pub fn jump_target(&self) -> Option<JumpTarget> {
        self.jump
    }

    /// A channel the person has not joined, offered alongside the notice that explains why the
    /// message of a permalink cannot be shown. Join it with `Input::JoinChannel`; the link then
    /// opens by itself. Cleared with the notice.
    pub fn join_offer(&self) -> Option<ConversationId> {
        self.join_offer.map(|(conversation, _)| conversation)
    }

    /// Whether a permalink (or a search hit) is still being looked for: its message fetched, or
    /// its history paged in. It ends with `jump_target`, a notice, or a `join_offer`.
    pub fn opening_link(&self) -> bool {
        self.permalink.is_some()
    }

    /// Number of submissions (sends and edits) that have not reached a final state. Intended for
    /// diagnostics and tests.
    pub fn unsettled_operations(&self) -> usize {
        self.ops.len() + self.edit_ops.len()
    }

    // ----- State machine -----

    /// Applies one input and returns the resulting effects and changes.
    pub fn handle(&mut self, input: Input) -> Output {
        let mut out = self.dispatch(input);
        self.permalink_housekeeping(&mut out);
        self.ensure_selected_members(&mut out);
        self.release_held_text(&mut out);
        self.maybe_mark_read(&mut out);
        self.maybe_mark_thread_read(&mut out);
        self.sync_inbox_states(&mut out);
        self.sync_attention(&mut out);
        self.sync_rail(&mut out);
        self.flush_cache(&mut out);
        out
    }

    fn dispatch(&mut self, input: Input) -> Output {
        // An event or an answer that belongs to a workspace in the background is applied to that
        // workspace's own scope, and the loaded scope is the active one again afterwards.
        if let Some(workspace) = self.route_scope(&input) {
            let back = self.scope;
            self.enter_scope(Some(workspace));
            let out = self.dispatch_in_scope(input);
            self.enter_scope(back);
            return out;
        }
        self.dispatch_in_scope(input)
    }

    fn dispatch_in_scope(&mut self, input: Input) -> Output {
        let mut out = Output::default();
        match input {
            Input::Register {
                username,
                display_name,
                password,
                invite_code,
            } => self.register(&username, &display_name, password, &invite_code, &mut out),
            Input::SignIn { username, password } => self.sign_in(&username, password, &mut out),
            Input::SignOut => self.sign_out(&mut out),
            Input::SetServerUrl(text) => self.set_server_url(&text, &mut out),
            Input::CheckServer => self.check_server(&mut out),
            Input::ServerChecked { generation, result } => {
                self.server_checked(generation, result, &mut out)
            }
            Input::ServerSwitched { generation, result } => {
                self.server_switched(generation, result, &mut out)
            }
            Input::ServerFacts { url, facts } => self.server_facts_arrived(url, facts, &mut out),
            Input::PreviewInvitation { code } => self.preview_invitation(&code, &mut out),
            Input::InvitationPreviewed { generation, result } => {
                self.invitation_previewed(generation, result, &mut out)
            }
            Input::RetryLoad => {
                if matches!(self.phase, SessionPhase::Unreachable { .. }) {
                    self.set_phase(SessionPhase::Loading, &mut out);
                    self.request_bootstrap(&mut out);
                } else if self.revalidating {
                    self.request_bootstrap(&mut out);
                } else if self.workspaces.is_empty() && matches!(self.phase, SessionPhase::Ready) {
                    // "Check again" with no workspace (access was lost): the person may have
                    // been let back in.
                    self.request_bootstrap(&mut out);
                }
            }
            Input::SelectConversation(id) => {
                self.permalink_selection_chosen();
                self.retry_member_fetch(id);
                self.select_conversation(id, &mut out)
            }
            Input::SetDraft { conversation, text } => self.set_draft(conversation, text, &mut out),
            Input::SendMessage { conversation, text } => {
                if self.must_wait_for_members(conversation, &text, &mut out)
                    || self.has_held_text(conversation)
                {
                    self.held_text
                        .push(Input::SendMessage { conversation, text });
                } else {
                    self.send_message(conversation, &text, &mut out)
                }
            }
            Input::RetryOperation {
                conversation,
                operation,
            } => self.retry_operation(conversation, operation, &mut out),
            Input::LoadOlder { conversation } => self.load_older(conversation, &mut out),
            Input::DismissNotice => self.set_notice(None, &mut out),
            Input::EditMessage {
                conversation,
                message,
                text,
            } => {
                if self.must_wait_for_members(conversation, &text, &mut out)
                    || self.has_held_text(conversation)
                {
                    self.held_text.push(Input::EditMessage {
                        conversation,
                        message,
                        text,
                    });
                } else {
                    self.edit_message(conversation, message, &text, &mut out)
                }
            }
            Input::CancelEdit {
                conversation,
                message,
            } => self.cancel_edit(conversation, message, &mut out),
            Input::RetryEdit {
                conversation,
                message,
            } => self.retry_edit(conversation, message, &mut out),
            Input::DeleteMessage {
                conversation,
                message,
            } => self.delete_message(conversation, message, &mut out),
            Input::OpenPermalink(link) => self.open_permalink(link, &mut out),
            Input::ClearJumpTarget => self.set_jump(None, &mut out),
            Input::CreateWebhook { conversation, name } => {
                self.create_webhook(conversation, &name, &mut out)
            }
            Input::DeleteWebhook(webhook) => self.delete_webhook(webhook, &mut out),
            Input::LoadWebhooks(conversation) => self.load_webhooks(conversation, &mut out),
            Input::DismissCreatedWebhook => self.dismiss_created_webhook(&mut out),
            Input::SetDraftSelection {
                conversation,
                anchor,
                head,
            } => self.set_draft_selection(conversation, anchor, head, &mut out),
            Input::SetThreadDraftSelection { root, anchor, head } => {
                self.set_thread_draft_selection(root, anchor, head, &mut out)
            }
            Input::OpenThread { conversation, root } => {
                self.open_thread_input(conversation, root, &mut out)
            }
            Input::CloseThread => self.close_thread_input(&mut out),
            Input::LoadOlderReplies { root } => self.load_older_replies(root, &mut out),
            Input::SendReply {
                conversation,
                root,
                text,
                also_in_channel,
            } => {
                if self.must_wait_for_members(conversation, &text, &mut out)
                    || self.has_held_text(conversation)
                {
                    self.held_text.push(Input::SendReply {
                        conversation,
                        root,
                        text,
                        also_in_channel,
                    });
                } else {
                    self.send_reply(conversation, root, &text, also_in_channel, &mut out)
                }
            }
            Input::SetThreadDraft { root, text } => self.set_thread_draft(root, text, &mut out),
            Input::SetThreadFollow { root, following } => {
                self.set_thread_follow(root, following, &mut out)
            }
            Input::ThreadAtBottom { root, at_bottom } => {
                self.thread_at_bottom_changed(root, at_bottom)
            }
            Input::RepliesLoaded {
                root,
                generation,
                result,
            } => self.replies_loaded(root, generation, result, &mut out),
            Input::ThreadFollowResult { root, result } => {
                self.thread_follow_result(root, result, &mut out)
            }
            Input::ThreadReadResult {
                root,
                up_to,
                result,
            } => self.thread_read_result(root, up_to, result, &mut out),
            Input::CreateGroup {
                handle,
                name,
                members,
            } => self.create_group(&handle, &name, members, &mut out),
            Input::UpdateGroup {
                id,
                handle,
                name,
                members,
            } => self.update_group(id, handle, name, members, &mut out),
            Input::DeleteGroup(id) => self.delete_group(id, &mut out),
            Input::GroupCommandResult { operation, result } => {
                self.group_command_result(operation, result, &mut out)
            }
            Input::MembersFetched {
                conversation,
                generation,
                result,
            } => self.members_fetched(conversation, generation, result, &mut out),
            Input::ToggleReaction {
                conversation,
                message,
                emoji,
            } => self.toggle_reaction(conversation, message, emoji, &mut out),
            Input::RecordEmojiUse(emoji) => self.record_emoji_use(emoji, &mut out),
            Input::SetSkinTone(tone) => self.set_skin_tone(tone, &mut out),
            Input::ReactionResult {
                message,
                emoji,
                result,
            } => self.reaction_result(message, emoji, result, &mut out),
            Input::EditResult { operation, result } => {
                self.edit_result(operation, result, &mut out)
            }
            Input::MessageFetched { message, result } => {
                self.message_fetched(message, result, &mut out)
            }
            Input::DeleteResult { message, result } => {
                self.delete_result(message, result, &mut out)
            }
            Input::CreateChannel {
                name,
                visibility,
                topic,
                description,
            } => self.create_channel(&name, visibility, &topic, &description, &mut out),
            Input::OpenConversation { participants } => {
                self.open_conversation(participants, &mut out)
            }
            Input::JoinChannel(id) => self.join_channel(id, &mut out),
            Input::LeaveChannel(id) => self.leave_channel(id, &mut out),
            Input::AddMembers { id, users } => self.add_members(id, users, &mut out),
            Input::RemoveMember { id, user } => self.remove_member(id, user, &mut out),
            Input::UpdateConversation {
                id,
                name,
                topic,
                description,
            } => self.update_conversation(id, name, topic, description, &mut out),
            Input::ArchiveConversation(id) => self.archive_conversation(id, true, &mut out),
            Input::UnarchiveConversation(id) => self.archive_conversation(id, false, &mut out),
            Input::SetFavorite { id, favorite } => self.set_favorite(id, favorite, &mut out),
            Input::DismissAction(kind) => self.dismiss_action(kind, &mut out),
            Input::WindowActive(active) => {
                self.window_active = active;
                self.mark_failed.clear();
            }
            Input::TimelineAtBottom {
                conversation,
                at_bottom,
            } => {
                if at_bottom {
                    self.at_bottom.insert(conversation);
                } else {
                    self.at_bottom.remove(&conversation);
                }
                self.mark_failed.remove(&conversation);
            }
            Input::ConversationCommandResult { operation, result } => {
                self.command_result(operation, result, &mut out)
            }
            Input::ActionResult { kind, result } => self.action_result(kind, result, &mut out),
            Input::MarkReadResult {
                conversation,
                up_to,
                result,
            } => self.mark_read_result(conversation, up_to, result, &mut out),

            Input::Started(state) => self.start_from(state, &mut out),
            Input::Authenticated { user } => self.authenticated(user, &mut out),
            Input::AuthFailed(error) => {
                self.note_request_error("auth", &error);
                let message = self.admin.auth_failure(&error);
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
                self.note_request_error("bootstrap", &error);
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
                if self.abandoned_persists.remove(&operation) {
                    out.effects.push(Effect::SettleSubmission { operation });
                } else if self.edit_ops.contains_key(&operation) {
                    self.edit_persisted(operation, &mut out);
                } else if self
                    .ops
                    .get(&operation)
                    .is_some_and(|op| op.stage == OpStage::Persisting)
                {
                    self.op_persisted(operation, &mut out);
                }
            }
            Input::SubmissionPersistFailed { operation, message } => {
                if self.abandoned_persists.remove(&operation) {
                    out.effects.push(Effect::SettleSubmission { operation });
                } else if self.edit_ops.contains_key(&operation) {
                    self.edit_persist_failed(operation, &message, &mut out);
                } else if self
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
            Input::StorageProblem(message) => {
                self.note_error("storage", "problem");
                self.set_notice(Some(message), &mut out)
            }
            input @ (Input::AttachFiles { .. }
            | Input::RemoveAttachment { .. }
            | Input::CancelAttachment { .. }
            | Input::RetryAttachment { .. }
            | Input::DiscardOperation { .. }
            | Input::DownloadAttachment { .. }
            | Input::CancelDownload(_)
            | Input::DismissDownload(_)
            | Input::LoadThumbnail(_)
            | Input::LoadFullImage(_)
            | Input::ReleaseFullImage(_)
            | Input::FileStaged { .. }
            | Input::UploadProgress { .. }
            | Input::UploadResult { .. }
            | Input::DownloadProgress { .. }
            | Input::DownloadResult { .. }
            | Input::ImageLoaded { .. }) => self.attachment_input(input, &mut out),
            input @ (Input::BrowseChannels
            | Input::SetMessageSaved { .. }
            | Input::MessageSavedResult { .. }
            | Input::OpenSaved
            | Input::CloseSaved
            | Input::LoadMoreSaved
            | Input::SavedLoaded { .. }
            | Input::OpenPeople
            | Input::ClosePeople
            | Input::SetPeopleQuery(_)
            | Input::SetPeopleIncludeDeactivated(_)
            | Input::LoadMorePeople
            | Input::PeopleLoaded { .. }
            | Input::OpenProfile { .. }
            | Input::CloseProfile
            | Input::ProfileLoaded { .. }
            | Input::SetBrowseQuery(_)
            | Input::SetBrowseJoined(_)
            | Input::SetBrowseSort(_)
            | Input::LoadMoreBrowse
            | Input::CloseBrowse
            | Input::BrowseLoaded { .. }) => self.discovery_input(input, &mut out),
            input @ (Input::SwitchWorkspace(_)
            | Input::CreateWorkspace { .. }
            | Input::AcceptInvitation { .. }
            | Input::UpdateProfile { .. }
            | Input::SetAvatar { .. }
            | Input::RemoveAvatar
            | Input::LoadAvatar(_)
            | Input::SetAppearance(_)
            | Input::AvatarLoaded { .. }) => self.workspace_input(input, &mut out),
            input @ (Input::UpdateWorkspace { .. }
            | Input::SetMemberRole { .. }
            | Input::SetMemberDeactivated { .. }
            | Input::OpenInvitations
            | Input::CloseInvitations
            | Input::CreateInvitation { .. }
            | Input::RevokeInvitation(_)
            | Input::DismissCreatedInvitation
            | Input::InvitationsLoaded { .. }) => self.admin_input(input, &mut out),
            Input::ChooseQuickSwitcher(target) => self.choose_quick_switcher(target, &mut out),
            input @ (Input::Clock(_)
            | Input::SetNotificationSettings(_)
            | Input::SetConversationNotificationPrefs { .. }
            | Input::PauseNotifications { .. }
            | Input::NotificationSettingsResult { .. }
            | Input::ConversationPrefsResult { .. }
            | Input::SetStatus(_)
            | Input::StatusResult { .. }
            | Input::NotificationActivated { .. }) => self.notify_input(input, &mut out),
            input @ (Input::OpenInbox
            | Input::LoadInbox { .. }
            | Input::LoadMoreInbox
            | Input::MarkInboxRead
            | Input::CloseInbox
            | Input::InboxLoaded { .. }
            | Input::InboxReadResult { .. }) => self.inbox_input(input, &mut out),
            input @ (Input::OpenSearch
            | Input::SetSearchQuery { .. }
            | Input::RunSearch
            | Input::LoadMoreSearch
            | Input::SetSearchSort(_)
            | Input::CloseSearch
            | Input::OpenSearchHit(_)
            | Input::SearchResult { .. }) => self.search_input(input, &mut out),
            input @ (Input::AddCustomEmoji { .. }
            | Input::RenameCustomEmoji { .. }
            | Input::DeleteCustomEmoji(_)
            | Input::LoadEmojiImage(_)
            | Input::CustomEmojiUploaded { .. }
            | Input::EmojiImageLoaded { .. }) => self.emoji_input(input, &mut out),
        }
        out
    }

    // ----- Session -----

    fn register(
        &mut self,
        username: &str,
        display_name: &str,
        password: String,
        invite_code: &str,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::SignedOut { .. }) {
            return;
        }
        if self.server.trying.is_some() {
            return self.reject_sign_in(SERVER_UNSETTLED.into(), out);
        }
        self.admin.set_registering(true);
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
        // The code is never echoed back in a message, and lives only in the request.
        // Written any way a person copies it (case, groups, dashes), it is the same code.
        let invite_code = canonical_invite_code(invite_code);
        let invite_code = match invite_code.as_str() {
            "" => None,
            code if invite_code_is_wellformed(code) => Some(InviteCode::new(code)),
            _ => return self.reject_sign_in(admin::MALFORMED_INVITE_CODE.into(), out),
        };
        self.set_phase(SessionPhase::SigningIn, out);
        out.effects.push(Effect::Register(RegisterRequest {
            username,
            display_name,
            password: Password::new(password),
            invite_code,
        }));
    }

    fn sign_in(&mut self, username: &str, password: String, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::SignedOut { .. }) {
            return;
        }
        if self.server.trying.is_some() {
            return self.reject_sign_in(SERVER_UNSETTLED.into(), out);
        }
        self.admin.set_registering(false);
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
            self.note_error("cache", "discarded_other_account");
            out.effects.push(Effect::DiscardCache);
        }
        self.users.insert(user.id, user.clone());
        self.me = Some(user);
        self.set_phase(SessionPhase::Loading, out);
        self.request_bootstrap(out);
    }

    /// Takes what the runtime loaded (at start, and again after the server was changed) and
    /// leaves the signed-in or signed-out phase it implies.
    fn start_from(&mut self, state: StartupState, out: &mut Output) {
        self.server = state.server.clone();
        out.changes.push(Change::ServerChanged);
        self.drafts = state.drafts;
        self.thread_selections = state
            .thread_drafts
            .iter()
            .filter_map(|draft| Some((draft.root, draft.selection?)))
            .collect();
        self.thread_drafts = state
            .thread_drafts
            .into_iter()
            .map(|draft| (draft.root, (draft.conversation, draft.text)))
            .collect();
        self.restore_view(state.view);
        self.emoji_prefs = state.emoji_prefs.sanitized();
        self.appearance = state.appearance;
        self.preferred_active = state.active_workspace;
        self.restore_recent_searches(state.recent_searches);
        self.restore_recent_conversations(state.recent_conversations);
        self.recompute_shown_prefs();
        self.recovered_journal = state.journal;
        if !state.problems.is_empty() {
            self.note_error("storage", "startup_problem");
            self.set_notice(Some(state.problems.join(" ")), out);
        }
        let cache = state
            .cache
            .map(ContentCache::bounded)
            .filter(|cache| !cache.workspaces.is_empty());
        self.cache_owner = cache.as_ref().map(|cache| cache.user.id);
        match (state.session_restored, cache) {
            (true, Some(cache)) => self.restore_from_cache(cache, out),
            (true, None) => {
                self.set_phase(SessionPhase::Loading, out);
                self.request_bootstrap(out);
            }
            (false, _) => self.set_phase(SessionPhase::SignedOut { last_error: None }, out),
        }
    }

    fn sign_out(&mut self, out: &mut Output) {
        self.reset_local(out);
        self.set_phase(SessionPhase::SignedOut { last_error: None }, out);
        out.effects.push(Effect::SignOut);
    }

    /// Forgets everything that belongs to the person and the server: what was loaded, the
    /// drafts, the unsent messages, and the emoji preferences.
    fn reset_local(&mut self, out: &mut Output) {
        self.clear_session_data(out);
        self.drafts.clear();
        self.thread_drafts.clear();
        self.draft_selections.clear();
        self.thread_selections.clear();
        self.last_selected.clear();
        self.recovered_journal.clear();
        if self.emoji_prefs != EmojiPrefs::default() {
            self.emoji_prefs = EmojiPrefs::default();
            out.changes.push(Change::EmojiPrefsChanged);
        }
        self.cache_owner = None;
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
        self.clear_files(out);
        self.sockets.clear();
        self.subscribed.clear();
        self.members.clear();
        self.applied.clear();
        self.bootstrap_in_flight = false;
        self.revalidating = false;
        self.cache_dirty = false;
        self.cache_urgent = false;
        self.user_states.clear();
        self.state_as_of.clear();
        self.marker_anchor.clear();
        self.actions.clear();
        self.commands.clear();
        self.at_bottom.clear();
        self.mark_in_flight.clear();
        self.mark_failed.clear();
        self.leaving.clear();
        self.edits.clear();
        self.edit_ops.clear();
        self.abandoned_persists.clear();
        self.counted_unread.clear();
        self.rebuilt_through.clear();
        self.groups.clear();
        self.clear_emoji();
        self.clear_search(out);
        self.clear_notifications(out);
        self.clear_inbox(out);
        self.clear_discovery(out);
        self.clear_admin(out);
        self.parked.clear();
        self.settings_known = false;
        self.scope = None;
        self.active = None;
        self.action_scope.clear();
        self.avatars = workspaces::Avatars::default();
        self.creating = None;
        self.last_rail.clear();
        self.member_lists.clear();
        self.reaction_toggles.clear();
        self.clear_threads();
        self.clear_webhooks();
        self.held_text.clear();
        self.permalink = None;
        self.join_offer = None;
        if self.jump.take().is_some() {
            out.changes.push(Change::JumpTargetChanged);
        }
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
        self.note_error("session", "expired");
        self.bootstrap_in_flight = false;
        self.abandon_emoji_add();
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
        self.fail_running_edits(out);
        self.drop_all_reaction_toggles(out);
        self.drop_all_follows(out);
        self.close_search(out);
        self.close_inbox(out);
        self.close_admin_views(out);
        self.permalink = None;
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

        let me = response.user.id;
        self.me = Some(response.user.clone());
        self.settings_known = true;

        let held_workspace = self.held_workspace();
        let held_all: Vec<WorkspaceInfo> = self.workspaces.clone();
        let listed: HashSet<WorkspaceId> = response
            .workspaces
            .iter()
            .map(|bootstrap| bootstrap.workspace.id)
            .collect();
        // What belongs to a workspace that is no longer listed goes with it.
        if self.scope.is_some_and(|scope| !listed.contains(&scope)) {
            self.enter_scope(None);
        }
        self.parked.retain(|id, _| listed.contains(id));
        self.workspaces.clear();
        self.members.clear();
        for bootstrap in &response.workspaces {
            self.workspaces.push(WorkspaceInfo {
                workspace: bootstrap.workspace.clone(),
                role: bootstrap.role,
            });
            let ids = self.members.entry(bootstrap.workspace.id).or_default();
            ids.extend(bootstrap.members.iter().map(|member| member.id));
        }
        for bootstrap in &response.workspaces {
            let user = response.user.clone();
            self.in_scope(bootstrap.workspace.id, |core| {
                core.fill_scope(bootstrap, Some(&user), out)
            });
        }
        // The workspace shown: the one that was, else the one the app last showed, else the first.
        let chosen = self
            .active
            .filter(|active| listed.contains(active))
            .or(self
                .preferred_active
                .filter(|wanted| listed.contains(wanted)))
            .or(response.workspaces.first().map(|b| b.workspace.id));
        self.enter_scope(chosen);
        if self.active != chosen {
            self.active = chosen;
            self.selected = None;
            out.changes.push(Change::ActiveWorkspaceChanged);
            out.effects.push(Effect::PersistActiveWorkspace(chosen));
        }
        self.reconcile_avatars(out);
        self.adopt_bootstrap_access(held_workspace, &held_all, out);
        self.adopt_bootstrap_prefs(&response, out);
        self.adopt_bootstrap_saved(&response, out);
        out.changes.push(Change::DirectoryChanged);
        out.changes.push(Change::GroupsChanged);

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
        self.pump_edits(out);
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
        let readable: HashSet<ConversationId> = self.all_conversation_ids();
        self.adopt_bootstrap_states(response, false);
        self.adopt_bootstrap_thread_states(response, false);
        let lost: Vec<ConversationId> = self
            .timelines
            .keys()
            .filter(|id| !readable.contains(id))
            .copied()
            .collect();
        for conversation in lost {
            self.drop_edits_in(conversation, out);
            self.drop_reactions_in(conversation);
            self.forget_threads_of(conversation, out);
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
        self.search_resynced(out);
        self.inbox_resynced(out);
        self.discovery_resynced(out);
        self.adopt_bootstrap_states(response, true);
        self.adopt_bootstrap_thread_states(response, true);
        let known: Vec<ConversationId> = self.timelines.keys().copied().collect();
        for conversation in known {
            if let Some(timeline) = self.timelines.get_mut(&conversation) {
                timeline.reset_keeping_local(conversation, &mut out.changes);
            }
        }
        self.refresh_pane(out);

        self.applied.clear();
        let previously_subscribed = std::mem::take(&mut self.subscribed);
        let previous_sockets = std::mem::take(&mut self.sockets);
        // A workspace the server no longer lists is not read any more.
        for workspace in previously_subscribed {
            if !response
                .workspaces
                .iter()
                .any(|w| w.workspace.id == workspace)
            {
                out.effects.push(Effect::Unsubscribe { workspace });
            }
        }
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
            let next = self.default_selection();
            self.change_selection(next, out);
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
        self.me = Some(me.clone());
        for cached in cache.workspaces {
            let CachedWorkspace {
                workspace,
                role,
                conversations,
                members,
                cursor,
                timelines,
                user_state,
                groups,
                custom_emoji,
                threads,
            } = cached;
            let id = workspace.id;
            self.workspaces.push(WorkspaceInfo { workspace, role });
            let ids = self.members.entry(id).or_default();
            ids.extend(members.iter().map(|member| member.id));
            let person = me.clone();
            self.in_scope(id, |core| {
                core.groups = groups;
                core.restore_emoji(custom_emoji);
                core.users = members
                    .into_iter()
                    .map(|member| (member.id, member))
                    .collect();
                core.users.entry(person.id).or_insert(person);
                core.conversations = conversations;
            });
            for cached_state in user_state {
                let conversation = cached_state.state.conversation_id;
                if !cached_state.counted.is_empty() {
                    let mentioning: HashSet<EventSeq> =
                        cached_state.counted_mentions.into_iter().collect();
                    self.counted_unread.insert(
                        conversation,
                        cached_state
                            .counted
                            .into_iter()
                            .map(|seq| (seq, mentioning.contains(&seq)))
                            .collect(),
                    );
                }
                if let Some(through) = cached_state.rebuilt_through {
                    self.rebuilt_through.insert(conversation, through);
                }
                self.state_as_of.insert(conversation, cached_state.as_of);
                self.user_states.insert(conversation, cached_state.state);
            }
            for cached_thread in threads {
                let root = cached_thread.state.root_id;
                if !cached_thread.counted.is_empty() {
                    let mentioning: HashSet<EventSeq> =
                        cached_thread.counted_mentions.into_iter().collect();
                    self.thread_counted.insert(
                        root,
                        cached_thread
                            .counted
                            .into_iter()
                            .map(|seq| (seq, mentioning.contains(&seq)))
                            .collect(),
                    );
                }
                if let Some(through) = cached_thread.rebuilt_through {
                    self.thread_rebuilt.insert(root, through);
                }
                self.thread_as_of.insert(root, cached_thread.as_of);
                self.thread_states.insert(root, cached_thread.state);
            }
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
        let chosen = self
            .preferred_active
            .filter(|wanted| self.knows_workspace(*wanted))
            .or(self.workspaces.first().map(|info| info.workspace.id));
        self.enter_scope(chosen);
        self.active = chosen;
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
                    .scope_conversations(id)
                    .iter()
                    .filter(|c| c.workspace_id == id)
                    .cloned()
                    .collect();
                let scope_users = self.scope_users(id)?;
                let members = self
                    .members
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .filter_map(|member| scope_users.get(member).cloned())
                    .collect();
                let timelines = conversations
                    .iter()
                    .filter_map(|c| self.timelines.get(&c.id)?.to_cache(c.id))
                    .collect();
                let user_state = conversations
                    .iter()
                    .filter_map(|c| {
                        Some(CachedUserState {
                            state: self.user_states.get(&c.id)?.clone(),
                            as_of: *self.state_as_of.get(&c.id)?,
                            counted: self
                                .counted_unread
                                .get(&c.id)
                                .map(|counted| counted.keys().copied().collect())
                                .unwrap_or_default(),
                            rebuilt_through: self.rebuilt_through.get(&c.id).copied(),
                            counted_mentions: self
                                .counted_unread
                                .get(&c.id)
                                .map(|counted| {
                                    counted
                                        .iter()
                                        .filter(|(_, mentioning)| **mentioning)
                                        .map(|(seq, _)| *seq)
                                        .collect()
                                })
                                .unwrap_or_default(),
                        })
                    })
                    .collect();
                let groups = self
                    .scope_groups(id)
                    .iter()
                    .filter(|group| group.workspace_id == id)
                    .cloned()
                    .collect();
                let known: HashSet<ConversationId> = conversations.iter().map(|c| c.id).collect();
                let mut threads: Vec<CachedThreadState> = self
                    .thread_states
                    .values()
                    .filter(|state| known.contains(&state.conversation_id))
                    .filter_map(|state| {
                        let root = state.root_id;
                        // A state shown for a follow that is not saved yet is not the server's,
                        // and it has no position: it is never cached.
                        if self
                            .thread_as_of
                            .get(&root)
                            .is_none_or(|as_of| *as_of == EventSeq::ZERO)
                        {
                            return None;
                        }
                        let counted = self.thread_counted.get(&root);
                        let mut saved = state.clone();
                        saved.following = self.saved_following(root, saved.following);
                        Some(CachedThreadState {
                            state: saved,
                            as_of: *self.thread_as_of.get(&root)?,
                            counted: counted
                                .map(|counted| counted.keys().copied().collect())
                                .unwrap_or_default(),
                            counted_mentions: counted
                                .map(|counted| {
                                    counted
                                        .iter()
                                        .filter(|(_, mentioning)| **mentioning)
                                        .map(|(seq, _)| *seq)
                                        .collect()
                                })
                                .unwrap_or_default(),
                            rebuilt_through: self.thread_rebuilt.get(&root).copied(),
                        })
                    })
                    .collect();
                threads.sort_by_key(|thread| thread.state.root_id);
                Some(CachedWorkspace {
                    workspace: info.workspace.clone(),
                    role: info.role,
                    conversations,
                    members,
                    cursor,
                    timelines,
                    user_state,
                    groups,
                    custom_emoji: self.scope_emoji(id).to_vec(),
                    threads,
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
        self.note_error("snapshot", "required");
        // The server is about to close the connection: what arrives until a new one has caught up
        // is not news.
        for state in self.sockets.values_mut() {
            if *state == ConnectionState::Live {
                *state = ConnectionState::CatchingUp;
            }
        }
        self.recompute_connection(out);
        self.revalidating = false;
        self.cache_suppressed = true;
        self.member_lists.clear();
        self.request_bootstrap(out);
        if self.cache_owner.take().is_some() {
            self.note_error("cache", "discarded_snapshot_required");
            out.effects.push(Effect::DiscardCache);
        }
        let known: Vec<ConversationId> = self.timelines.keys().copied().collect();
        for conversation in known {
            if let Some(timeline) = self.timelines.get_mut(&conversation) {
                timeline.reset_keeping_local(conversation, &mut out.changes);
            }
        }
        self.reset_pane(out);
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
            if self.ops.contains_key(&entry.operation_id)
                || self.edit_ops.contains_key(&entry.operation_id)
            {
                continue;
            }
            if let JournalKind::Edit {
                message_id,
                base_revision,
            } = entry.kind
            {
                self.recover_edit(entry, message_id, base_revision, out);
                continue;
            }
            let operation = entry.operation_id;
            let order = self.take_order();
            let thread = entry.thread_root.map(|root| threads::ReplyTo {
                root,
                also_in_channel: entry.also_in_channel,
            });
            self.ops.insert(
                operation,
                LocalOp {
                    conversation: entry.conversation_id,
                    document: entry.document.clone(),
                    stage: OpStage::Querying,
                    resends: 0,
                    order,
                    thread,
                    submitted_at: entry.submitted_at,
                },
            );
            self.restore_op_files(operation, entry.conversation_id, &entry.attachments);
            let (attachments, upload) = self.op_row_files(operation);
            let row = local_row(
                operation,
                me,
                entry.document,
                Delivery::UnknownOutcome,
                thread,
                attachments,
                upload,
            );
            self.push_local_row(entry.conversation_id, thread, row, out);
            out.effects.push(Effect::QueryOperationStatus { operation });
        }
    }

    // ----- Conversations and drafts -----

    fn select_conversation(&mut self, id: ConversationId, out: &mut Output) {
        let Some(workspace) = self.conversation(id).map(|c| c.workspace_id) else {
            return;
        };
        // A conversation of another workspace brings that workspace with it.
        if self.active != Some(workspace) {
            self.switch_workspace(workspace, out);
        }
        self.change_selection(Some(id), out);
        self.mark_failed.remove(&id);
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
        // What arrived since an earlier request is already in the page this one will get.
        timeline.clear_unloaded_updates();
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
                timeline.clear_unloaded_updates();
                timeline.note_page(belongs.iter().map(|m| m.seq).min(), page.has_more);
                timeline.history = HistoryStatus::Ready {
                    has_more: page.has_more,
                };
                out.changes
                    .push(Change::HistoryStatusChanged { conversation });
                self.cache_changed(true);
                // Rows that have just appeared may belong to edits that are still unresolved.
                self.resync_edit_rows(conversation, out);
                self.resync_reaction_rows(conversation, out);
            }
            Err(error) => {
                let code = error.code();
                timeline.history = HistoryStatus::Failed {
                    message: error.message(),
                };
                out.changes
                    .push(Change::HistoryStatusChanged { conversation });
                self.note_error("history", code);
                if error.is_unauthenticated() {
                    self.session_expired(out);
                }
            }
        }
        self.permalink_advance(out);
    }

    fn set_draft(&mut self, conversation: ConversationId, text: String, out: &mut Output) {
        if self.draft(conversation) == text {
            return;
        }
        let emptied = text.is_empty();
        if emptied {
            self.drafts.remove(&conversation);
        } else {
            self.drafts.insert(conversation, text);
        }
        out.changes.push(Change::DraftChanged { conversation });
        self.persist_drafts(out);
        // A draft with no text has no caret to keep.
        if emptied && self.draft_selections.remove(&conversation).is_some() {
            self.persist_view(out);
        }
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
        let target = ComposerTarget::Conversation(conversation);
        let document = match parse_composer(text, &self.mention_directory(conversation)) {
            Ok(document) => document,
            // A message that is only files has no text.
            Err(DocumentError::Empty) if self.composer_allows_empty(target) => {
                MessageDocument::empty()
            }
            Err(DocumentError::Empty) => return,
            Err(error) => {
                self.set_notice(Some(format!("Message not sent: {error}.")), out);
                return;
            }
        };
        let Some(files) = self.take_composer_files(target, out) else {
            return;
        };
        self.submit(conversation, document, None, files, out);
        if self.drafts.remove(&conversation).is_some() {
            out.changes.push(Change::DraftChanged { conversation });
            self.persist_drafts(out);
        }
    }

    /// Creates a new operation: a pending row, then a durable journal write. The send itself
    /// waits for the write to be reported durable.
    ///
    /// `files` are the attachments of a message that has them, already copied or being copied
    /// into the outbox. The journal entry waits until the copies are in, so it never names a
    /// file that is not there.
    fn submit(
        &mut self,
        conversation: ConversationId,
        document: MessageDocument,
        thread: Option<threads::ReplyTo>,
        files: Vec<AttachmentId>,
        out: &mut Output,
    ) {
        let Some(me) = self.me.as_ref().map(|user| user.id) else {
            return;
        };
        let operation = OperationId::generate();
        let order = self.take_order();
        self.adopt_files(operation, &files);
        self.restage_files(&files, out);
        let staging = self.op_is_staging(operation);
        self.ops.insert(
            operation,
            LocalOp {
                conversation,
                document: document.clone(),
                stage: if staging {
                    OpStage::Staging
                } else {
                    OpStage::Persisting
                },
                resends: 0,
                order,
                thread,
                submitted_at: Timestamp::now(),
            },
        );
        let (attachments, upload) = self.op_row_files(operation);
        let row = local_row(
            operation,
            me,
            document,
            Delivery::Pending,
            thread,
            attachments,
            upload,
        );
        self.push_local_row(conversation, thread, row, out);
        if !staging {
            self.persist_send(operation, out);
        }
    }

    /// Asks the runtime to journal a send.
    pub(super) fn persist_send(&mut self, operation: OperationId, out: &mut Output) {
        let Some(me) = self.me.as_ref().map(|user| user.id) else {
            return;
        };
        let attachments = self.journal_files(operation);
        let Some(op) = self.ops.get_mut(&operation) else {
            return;
        };
        op.stage = OpStage::Persisting;
        out.effects.push(Effect::PersistSubmission(JournalEntry {
            operation_id: operation,
            user_id: me,
            conversation_id: op.conversation,
            document: op.document.clone(),
            submitted_at: op.submitted_at,
            kind: JournalKind::Send,
            thread_root: op.thread.map(|reply| reply.root),
            also_in_channel: op.thread.is_some_and(|reply| reply.also_in_channel),
            attachments,
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
        if let Err(error) = &result {
            self.note_request_error("send", error);
        }
        match result {
            Ok(response) => self.confirm_operation(operation, &response.message, out),
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(RequestError::Rejected(error)) => {
                let op = self.ops.get(&operation);
                let document = op.map(|op| &op.document);
                let reply = op.is_some_and(|op| op.thread.is_some());
                let thread_gone = reply && error.code == lazy_protocol::ErrorCode::NotFound;
                let reason = mention_refusal(document, &error)
                    .or_else(|| {
                        (reply && error.code == lazy_protocol::ErrorCode::NotFound).then(|| {
                            "The message that started this thread was deleted, so it can't be \
                             replied to."
                                .to_owned()
                        })
                    })
                    .unwrap_or(error.message);
                self.fail_operation(operation, reason, RetryKind::NewOperation, true, out);
                // A reply to a thread that no longer exists can never be sent, and its row may
                // be out of sight: its files are not kept for a retry.
                if thread_gone {
                    self.files_of_message_settled(operation, out);
                }
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
        if self.commands.contains_key(&operation) {
            return self.command_status_result(operation, result, out);
        }
        if let Some(&message) = self.edit_ops.get(&operation) {
            return self.edit_status_result(message, operation, result, out);
        }
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
            Ok(
                OperationStatus::ConversationCommitted { .. }
                | OperationStatus::EditCommitted { .. }
                | OperationStatus::GroupCommitted { .. },
            ) => {
                let reason = "The server gave an unexpected answer.".to_owned();
                self.fail_operation(operation, reason, RetryKind::SameOperation, false, out);
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
                // Files the server may not hold go up first (the same upload repeated is
                // harmless); then the message follows.
                if !self.op_uploaded(operation) {
                    if let Some(op) = self.ops.get_mut(&operation) {
                        op.stage = OpStage::Uploading;
                    }
                    self.begin_op_uploads(operation, out);
                }
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
        // The row is in the conversation's timeline, or, for a reply, in its thread's.
        let Some((item, thread)) = self.locate_local_row(conversation, operation) else {
            return;
        };
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
                    self.in_local_timeline(conversation, thread, out, |timeline, changes| {
                        timeline.move_to_back(conversation, ItemKey::Operation(operation), changes)
                    });
                    self.begin_status_query(operation, out);
                }
            }
            RetryKind::NewOperation => {
                let document = item.document.clone();
                let files = self.take_failed_files(operation);
                self.in_local_timeline(conversation, thread, out, |timeline, changes| {
                    timeline.remove(conversation, ItemKey::Operation(operation), changes);
                });
                self.tidy_parked(thread);
                // A message of only files whose files are gone has nothing left to send.
                if document.has_no_content() && files.is_empty() {
                    return;
                }
                self.submit(conversation, document, thread, files, out);
            }
        }
    }

    /// The operation committed as `message`: the row becomes confirmed and the journal entry is
    /// released. Safe to call more than once for the same operation.
    fn confirm_operation(&mut self, operation: OperationId, message: &Message, out: &mut Output) {
        let Some(op) = self.ops.get(&operation) else {
            return;
        };
        let (conversation, thread) = (op.conversation, op.thread);
        if message.conversation_id != conversation {
            return;
        }
        if message.deleted {
            // Sent, and deleted before the client heard back: the row never becomes a message.
            self.in_local_timeline(conversation, thread, out, |timeline, changes| {
                timeline.remove(conversation, ItemKey::Operation(operation), changes);
            });
            self.tidy_parked(thread);
            self.cache_changed(false);
            self.files_of_message_settled(operation, out);
            self.settle_operation(operation, out);
            return;
        }
        self.in_local_timeline(conversation, thread, out, |timeline, changes| {
            timeline.confirm_local(conversation, operation, message, changes)
        });
        if thread.is_some() {
            // A reply is also in the channel when it was sent there, and may already be in the
            // thread's rows if a page brought it in first.
            self.place_reply(message, out);
            self.tidy_parked(thread);
        }
        if self.edits.contains_key(&message.id) {
            self.sync_edit_row(conversation, message.id, out);
        }
        self.sync_reaction_row(conversation, message.id, out);
        self.cache_changed(false);
        self.files_of_message_settled(operation, out);
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
        let thread_root = op.thread.map(|reply| reply.root);
        self.stop_op_files(operation, out);
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
        // A reply that can never be sent, because its thread has ended, keeps no copies.
        if retry == RetryKind::NewOperation
            && thread_root.is_some_and(|root| self.thread_has_ended(root))
        {
            self.files_of_message_settled(operation, out);
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
        let attachments = self.op_file_ids(operation);
        if let Some(op) = self.ops.get_mut(&operation)
            && op.stage == OpStage::Queued
        {
            op.stage = OpStage::Sending;
            out.effects.push(Effect::SendMessage {
                operation,
                conversation,
                document: op.document.clone(),
                thread_root: op.thread.map(|reply| reply.root),
                also_in_channel: op.thread.is_some_and(|reply| reply.also_in_channel),
                attachments,
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
        self.pump_uploads(out);
    }

    fn set_item_delivery(
        &mut self,
        conversation: ConversationId,
        operation: OperationId,
        delivery: Delivery,
        out: &mut Output,
    ) {
        let thread = self.ops.get(&operation).and_then(|op| op.thread);
        self.in_local_timeline(conversation, thread, out, |timeline, changes| {
            timeline.set_delivery(
                conversation,
                ItemKey::Operation(operation),
                delivery,
                changes,
            );
        });
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
                    self.retry_unseen_edits(out);
                    self.retry_unseen_replies(out);
                    self.retry_pane_root(out);
                    self.retry_failed_images(out);
                    self.retry_failed_emoji_images(out);
                    self.retry_failed_avatars(out);
                    self.members_may_be_stale();
                }
            }
            ServerFrame::SnapshotRequired { .. } => self.snapshot_required(out),
            ServerFrame::Error { code, message } => {
                self.note_error(
                    "realtime",
                    crate::diagnostics::snake_case(&format!("{code:?}")),
                );
                if code == lazy_protocol::ErrorCode::Unauthenticated {
                    self.session_expired(out);
                } else if matches!(
                    code,
                    lazy_protocol::ErrorCode::Forbidden | lazy_protocol::ErrorCode::NotFound
                ) {
                    // The server ended this subscription because the person may not read the
                    // workspace any more (they were deactivated). Whether that still holds is
                    // for the bootstrap to say.
                    self.snapshot_required(out);
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
            } => {
                // Only an event that arrives once the connection has caught up is news.
                let live = self.sockets.get(&workspace) == Some(&ConnectionState::Live);
                let seen = message.clone();
                self.apply_message(message, operation_id, out);
                self.inbox_message_created(&seen, out);
                self.notify_message(&seen, envelope.occurred_at, live, out);
            }
            Event::MessageEdited {
                message,
                operation_id,
                previous_mentions,
            } => self.apply_message_edited(
                message,
                operation_id,
                &previous_mentions,
                envelope.seq,
                out,
            ),
            Event::MessageDeleted {
                message,
                had_mentions,
            } => self.apply_message_deleted(message, &had_mentions, envelope.seq, out),
            Event::ThreadChanged {
                conversation_id,
                root_id,
                summary,
            } => {
                let seen = summary.clone();
                self.apply_thread_changed(conversation_id, root_id, summary, envelope.seq, out);
                self.inbox_thread_changed(root_id, seen.as_ref(), out);
            }
            Event::ThreadStateChanged { state } => {
                let root = state.root_id;
                self.apply_thread_state_changed(state, envelope.seq, out);
                // What is on screen, which keeps a follow change that is not saved yet.
                if let Some(shown) = self.thread_states.get(&root).cloned() {
                    self.inbox_thread_state(&shown, out);
                }
            }
            Event::SavedChanged { message_id, saved } => self.event_saved(message_id, saved, out),
            Event::NotificationSettingsChanged { settings } => {
                self.event_notification_settings(workspace, settings, out)
            }
            Event::ConversationNotificationPrefsChanged { prefs } => {
                self.event_conversation_prefs(prefs, out)
            }
            Event::UserStatusChanged { user_id, status } => {
                self.event_user_status(user_id, status, out)
            }
            Event::GroupChanged { group } => self.apply_group_changed(group, out),
            Event::EmojiChanged { emoji } => self.apply_emoji_changed(emoji, out),
            Event::GroupDeleted { group_id } => self.apply_group_deleted(group_id, out),
            Event::ReactionsChanged {
                conversation_id,
                message_id,
                reactions,
            } => self.apply_reactions_changed(
                conversation_id,
                message_id,
                reactions,
                envelope.seq,
                out,
            ),
            Event::MemberJoined { user } => {
                self.members.entry(workspace).or_default().insert(user.id);
                self.users.insert(user.id, user);
                out.changes.push(Change::DirectoryChanged);
                self.cache_changed(true);
            }
            Event::ConversationCreated { conversation } => {
                self.event_conversation_created(conversation, envelope.seq, out)
            }
            Event::ConversationUpdated { conversation } => {
                self.event_conversation_updated(conversation, out)
            }
            Event::MemberAdded {
                conversation,
                user_id,
            } => self.event_member_added(conversation, user_id, envelope.seq, out),
            Event::MemberRemoved {
                conversation_id,
                user_id,
            } => self.event_member_removed(conversation_id, user_id, out),
            Event::UserStateChanged { state } => {
                self.event_user_state_changed(state, envelope.seq, out)
            }
            Event::WorkspaceUpdated { workspace } => self.apply_workspace(workspace, out),
            Event::MemberRoleChanged { user_id, role } => self.apply_role(user_id, role, out),
            Event::MemberDeactivated {
                user_id,
                deactivated,
            } => self.apply_deactivated(user_id, deactivated, out),
            Event::UserProfileChanged {
                user_id,
                display_name,
                avatar_version,
            } => self.apply_profile(user_id, display_name, avatar_version, out),
        }
        self.advance_cursor(workspace, envelope.seq);
    }

    fn apply_message(
        &mut self,
        message: Message,
        operation: Option<OperationId>,
        out: &mut Output,
    ) {
        // Events for a conversation this client does not know are ignored: history is fetched
        // when it learns of the conversation, and covers them.
        if self.conversation(message.conversation_id).is_none() {
            return;
        }
        let own = self
            .me
            .as_ref()
            .is_some_and(|me| me.id == message.author_id);
        let own_operation = operation.filter(|operation| own && self.ops.contains_key(operation));
        let conversation = message.conversation_id;
        // A replay serves the earlier events of a deleted message as tombstones. It never appears
        // and never counts as unread (its deletion event follows), but a tombstone is still a
        // position: the person's own message moved their read position when it was sent.
        self.count_message_event(&message, out);
        self.count_reply_event(&message, out);
        if message.deleted {
            // A send of ours that was deleted before it was confirmed simply ends.
            match own_operation {
                Some(operation) => self.confirm_operation(operation, &message, out),
                None => self.each_home(conversation, out, |_, timeline, _| {
                    timeline.note_unloaded_update(&message);
                }),
            }
            return;
        }
        if let Some(operation) = own_operation {
            self.confirm_operation(operation, &message, out);
            return;
        }
        let added = self.place_message(&message, out);
        if added > 0 {
            self.cache_changed(false);
            // The row may belong to an edit recovered from the journal, or carry reaction changes
            // that are still being saved.
            if self.edits.contains_key(&message.id) {
                self.sync_edit_row(conversation, message.id, out);
            }
            self.sync_reaction_row(conversation, message.id, out);
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
            if matches!(event, SocketEvent::Closed { .. }) {
                self.note_error("socket", "closed");
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
            self.join_offer = None;
            out.changes.push(Change::NoticeChanged);
        }
    }
}

/// What to tell the person when the server refuses a message that mentions something, if the
/// mention is the likely reason. Only `@channel` in the default channel is `forbidden` for a
/// reason in the content, and `invalid_request` for a message with a mention most likely means
/// a person or group that no longer exists.
pub(crate) fn mention_refusal(
    document: Option<&MessageDocument>,
    error: &lazy_protocol::ApiError,
) -> Option<String> {
    let mentions = document?.mentions();
    match error.code {
        lazy_protocol::ErrorCode::Forbidden
            if mentions.contains(&lazy_domain::MentionTarget::Channel) =>
        {
            Some(
                "You can't use @channel here: in the default channel it is for admins and owners."
                    .to_owned(),
            )
        }
        lazy_protocol::ErrorCode::InvalidRequest if !mentions.is_empty() => Some(
            "A mention in that message isn't valid: the person may have left the workspace, or \
             the group may no longer exist."
                .to_owned(),
        ),
        _ => None,
    }
}

/// Turns composer text into a document: `:shortcode:` becomes its emoji (only where it stands
/// alone, never in code or after a backslash), then the source syntax is parsed, with `@name`
/// becoming a mention where `directory` knows the name. The text the
/// person typed (a draft, an editor's source) is not changed; only what is sent is.
pub(crate) fn parse_composer(
    text: &str,
    directory: &dyn lazy_domain::MentionDirectory,
) -> Result<MessageDocument, DocumentError> {
    MessageDocument::from_markdown_with(&lazy_domain::emoji::replace_shortcodes(text), directory)
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
