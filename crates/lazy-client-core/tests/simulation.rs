//! Randomized interleavings of server events, history pages, acknowledgments, reconnects, and
//! app restarts against a scripted server. After every step the timelines must stay ordered and
//! free of duplicates; every cache write must satisfy the cache invariant (the cursor never runs
//! ahead of the messages it covers); once everything is delivered, every operation must be
//! settled exactly once and no committed message may be missing, even after restarts that find
//! any of the cache writes that could have survived a crash.
//!
//! Messages are also edited and deleted, by the person (through the core) and by others (on the
//! server). The server rewrites the earlier events of a deleted message as tombstones, so a
//! device that learns of a message only by replay sees it created already deleted. After
//! everything is delivered, every timeline must show each surviving message at the server's
//! latest revision, no deleted message, and an unread count that excludes deleted messages, in
//! every ordering.
//!
//! The person also searches (slice 3C) against a model of the server's search: single words,
//! answered from the messages as they stood when the request was seen, a page of at most one hit.
//! Requests are superseded, cancelled, lost, refused, and answered late; hits are opened, deleted
//! and edited under the person's eyes; the app restarts and the log is skipped by a snapshot.
//! Throughout, every hit on screen answers the last run, the view is loading exactly while its
//! request is out, and the recent searches are what the core last asked to save; at the end no
//! hit is a deleted message or an older revision, and signing out forgets it all.

mod common;

use std::collections::{HashMap, HashSet};

use common::{Driver, World, channel, seq};
use lazy_client_core::{
    ActionKind, ActionOutcome, ComposerTarget, ContentCache, Delivery, DesktopNotification,
    EditState, Effect, FileBytes, FileSource, ImageData, ImageState, InboxStatus, Input,
    JournalEntry, JournalKind, LoadStatus, LocalFile, MAX_CONCURRENT_IMAGE_FETCHES,
    MAX_CONCURRENT_UPLOADS, MAX_RECENT_CONVERSATIONS, MAX_RECENT_SEARCHES, Output,
    QUICK_SWITCHER_LIMIT, QuickSwitcherTarget, RequestError, SearchStatus, SessionPhase,
    SocketEvent, StagedFile, StartupState, TimelineItem, TransferError, ViewState,
};
use lazy_domain::notification::{
    ConversationNotificationPrefs, IncomingMessage, NotificationSettings, NotifyLevel, UserStatus,
    decide,
};
use lazy_domain::search::{MAX_SEARCH_LIMIT, MAX_SEARCH_OFFSET};
use lazy_domain::workspace::{
    ChannelCreationPolicy, RegistrationPolicy, RoleChangeError, WorkspaceSettings,
    check_member_action, check_role_change,
};
use lazy_domain::{
    AttachmentId, ConversationId, EmojiId, EventSeq, InvitationId, MessageDocument, MessageId,
    OperationId, ReactionEmoji, Timestamp, UserId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, Attachment, AttachmentResponse, BrowseChannelsQuery, BrowseChannelsResponse,
    BrowsedChannel, ChannelSort, ConversationMembersResponse,
    ConversationNotificationPrefsResponse, ConversationUserState, CustomEmoji, ErrorCode, Event,
    EventEnvelope, HistoryPage, InboxEntry, InboxPage, InboxQuery, InboxReadResponse, Mentions,
    Message, MessageResponse, NotificationSettingsResponse, OperationStatus, PeoplePage,
    PeopleQuery, Person, PersonResponse, ReactionSummary, SavedItem, SavedPage, SavedQuery,
    SavedResponse, SearchHit, SearchHitKind, SearchParams, SearchResponse, SearchSort,
    SendMessageResponse, ServerFrame, Snippet, SnippetRange, StatusResponse, ThreadStateResponse,
    ThreadSummary, ThreadUserState,
};
use lazy_protocol::{
    CreatedInvitation, Invitation, InvitationList, InvitationState, InviteCode, Workspace,
};
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, TestRng, TestRunner};
use std::sync::atomic::{AtomicUsize, Ordering};

const SERVER_PAGE: usize = 4;
const BASE_HEAD: i64 = 10;
/// The most results the simulated server returns in one page, however many are asked for. A
/// server may return fewer than the page asked for, and runs this short have few matches, so only
/// a page this small makes them page.
const SEARCH_SERVER_PAGE: usize = 1;
/// The words the person searches for, each matching the beginning of words. The long ones match
/// different messages ("hi" and "hey" none of the same), so an answer to the wrong word shows in
/// its snippets; the letters match several kinds of messages, so that runs have results to page,
/// open, lose to deletions and see edited.
/// The time the simulation's clock starts at, and a moment `seconds` after it. Nothing here reads
/// the real clock, so the default run is the same every time.
fn stamp(seconds: i64) -> lazy_domain::Timestamp {
    lazy_domain::Timestamp::from(
        time::OffsetDateTime::from_unix_timestamp(1_800_000_000 + seconds).unwrap(),
    )
}

/// The most a simulated directory or saved page holds, so that runs page.
const DISCOVERY_SERVER_PAGE: usize = 2;
const PEOPLE_QUERIES: [&str; 5] = ["", "z", "j", "zo", "q"];
const BROWSE_QUERIES: [&str; 5] = ["", "a", "gen", "ra", "q"];
const SWITCHER_QUERIES: [&str; 7] = ["", "g", "ra", "zo", "thr", "sav", "q"];

/// The most threads the simulated server returns in one page of the inbox.
const INBOX_SERVER_PAGE: usize = 2;

const SEARCH_WORDS: [&str; 7] = ["mine", "edited", "hi", "hey", "h", "m", "e"];
const ROLES: [WorkspaceRole; 3] = [
    WorkspaceRole::Member,
    WorkspaceRole::Admin,
    WorkspaceRole::Owner,
];
/// Invitations the server allows to be active at once in the simulation (small, to be reached).
const ACTIVE_INVITATION_CAP: usize = 3;

/// A change of the workspace's name or settings: some are refused before any request.
fn workspace_variant(pick: usize) -> (Option<String>, Option<WorkspaceSettings>) {
    let invite_only = WorkspaceSettings {
        registration: RegistrationPolicy::InviteOnly,
        channel_creation: ChannelCreationPolicy::Everyone,
    };
    let admins_only = WorkspaceSettings {
        registration: RegistrationPolicy::Open,
        channel_creation: ChannelCreationPolicy::AdminsOnly,
    };
    match pick % 6 {
        0 => (Some("Acme".into()), None),
        1 => (None, Some(invite_only)),
        2 => (Some("Zed Corp".into()), Some(admins_only)),
        3 => (Some("   ".into()), None),
        4 => (None, Some(WorkspaceSettings::default())),
        _ => (Some("Lazy".into()), None),
    }
}

/// How long and how often an invitation is asked for: some are refused before any request.
const INVITATION_REQUESTS: [(Option<u64>, Option<u32>); 6] = [
    (None, None),
    (Some(3600), Some(1)),
    (Some(7200), None),
    (Some(10), None),
    (None, Some(0)),
    (Some(86_400), Some(5)),
];

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
    /// The person (or a late answer) touches the server choice. While anyone is signed in, or
    /// signing in, it must do nothing at all.
    ServerPoke(usize),
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
    /// Another person edits one of their messages (or a message is edited from another device).
    OtherEdits(usize),
    /// A message is deleted on the server by someone other than this client: its author, or an
    /// admin. Any message may be picked, including the person's own.
    ServerDeletes(usize),
    /// The person edits one of their own messages that is on screen.
    UserEdits(usize),
    /// The person deletes a message on screen that they may delete.
    UserDeletes(usize),
    /// The person edits one of their own messages and, before the server hears it, someone else
    /// edits it too: the person's edit must be refused as stale.
    UserEditRaces(usize),
    /// Another person posts a message that mentions someone (see `Sim::mentions_of`).
    OtherMentions(bool, usize),
    /// A third person joins or leaves a channel.
    ThirdToggles(bool),
    /// The person toggles a reaction on a message on screen.
    UserReacts(usize, usize),
    /// Someone reacts (or stops reacting) on the server: another person, a third person, or the
    /// signed-in person on another device. Arguments pick the message, the emoji, and who.
    OtherReacts(usize, usize, usize),
    /// Another person replies in the thread of a live top-level message: which root, whether the
    /// reply is also sent to the channel, and what it mentions.
    OtherReplies(usize, bool, usize),
    /// The person toggles a reaction chosen from a wide palette (more kinds than a message can
    /// hold) on a message on screen.
    UserReactsWide(usize, usize),
    /// Other people react to a live message with many different kinds, until it holds as many as
    /// it can; the first kind is the person's own, from another device.
    FillReactions(usize),
    /// The person withdraws a kind they used on a message on screen and, at once, adds a new
    /// one: two requests that the server must see in the right order.
    UserSwapsReaction(usize),
    /// Another person replies in the thread that is open, if any.
    OtherRepliesToOpen(bool, usize),
    /// The person opens the thread of a top-level message on screen.
    OpenThread(usize),
    CloseThread,
    LoadOlderReplies,
    /// The person replies in the open thread, also to the channel or not.
    UserReplies(bool),
    /// The open thread reports whether its newest reply is on screen.
    ThreadBottom(bool),
    /// The person follows or stops following a thread (the open one, else one on screen).
    UserFollows(usize, bool),
    /// The person follows or stops following a thread on another device.
    OtherDeviceFollows(usize, bool),
    /// The person reads a thread on another device.
    OtherDeviceReadsThread(usize),
    /// The person attaches files to a conversation's composer: which conversation, how many,
    /// and whether they are pasted bytes rather than files on disk.
    UserAttaches(bool, usize, bool),
    /// The person attaches files to the reply box of the open thread.
    UserAttachesToThread(usize),
    /// The person attaches files and sends them at once: which conversation, how many, and
    /// whether the message has words or is only files.
    UserSendsFiles(bool, usize, bool),
    /// The person removes, cancels, or retries a chip: which composer, which chip, which of the
    /// three.
    UserHandlesChip(bool, usize, usize),
    /// The person retries or discards a message with files that failed.
    UserHandlesFailedFiles(usize, bool),
    /// The server forgets the uploads that were never attached (they expire).
    ServerExpiresUploads,
    /// The person looks at the files of a message on screen: previews one, opens one, saves one,
    /// or stops saving.
    UserViewsFile(usize, usize),
    /// An admin adds a custom emoji.
    ServerAddsEmoji,
    /// An admin renames a live custom emoji.
    ServerRenamesEmoji(usize),
    /// An admin deletes a live custom emoji.
    ServerDeletesEmoji(usize),
    /// The person sends a message that uses a custom emoji their client lists as live: which
    /// conversation, which emoji.
    UserSendsEmoji(bool, usize),
    /// The person reacts with a custom emoji their client lists as live.
    UserReactsCustom(usize, usize),
    /// Someone else reacts, or stops reacting, with a custom emoji that is live on the server.
    OtherReactsCustom(usize, usize),
    /// The person's client asks for the image of a custom emoji (live or deleted).
    UserViewsEmoji(usize),
    /// The person uses a custom emoji from the picker (it becomes a recent one).
    UserRecordsEmoji(usize),
    /// The person types a search word, opening the search if it is closed, and does not run it.
    SearchTypes(usize),
    /// The person types a search word and runs it.
    SearchRuns(usize),
    /// The person asks for more results.
    SearchMore,
    /// The person chooses how results are ordered (true: newest first).
    SearchSorts(bool),
    /// The person opens one of the results.
    SearchOpensHit(usize),
    /// The person closes the search.
    SearchCloses,
    /// The server answers the newest search request still out.
    SearchAnswered(Outcome),
    /// The person runs a search and the server answers it at once.
    SearchRunsAnswered(usize, Outcome),
    /// The person asks for more results and the server answers at once.
    SearchPagesOn(Outcome),
    /// Someone deletes a message the person is looking at as a search result.
    ServerDeletesHit(usize),
    /// Someone edits a message the person is looking at as a search result.
    OtherEditsHit(usize),
    /// The clock moves on by this many minutes and the core is told.
    Tick(usize),
    /// The person changes their notification settings (one of a table of them).
    UserSetsSettings(usize),
    /// The person pauses notifications for a while, or resumes them.
    UserSnoozes(usize),
    /// The person sets a conversation's preference.
    UserSetsPrefs(bool, usize),
    /// The person's settings, a conversation's preference, or their status change on another
    /// device.
    OtherDeviceSetsSettings(usize),
    OtherDeviceSetsPrefs(bool, usize),
    OtherDeviceSetsStatus(usize),
    /// The person sets or clears their status.
    UserSetsStatus(usize),
    /// Someone else sets or clears their status.
    OtherSetsStatus(usize),
    /// The person clicks the latest desktop notification.
    NotificationClicked,
    /// Someone deletes the message of the latest desktop notification.
    NotifiedMessageDeleted,
    /// The thread inbox: open, filter, page, mark read, close, and be answered.
    InboxOpens,
    InboxFilters(bool),
    InboxMore,
    InboxMarksRead,
    InboxCloses,
    InboxAnswered(Outcome),
    /// The person saves or unsaves a message; the same on another device.
    UserSaves(usize, bool),
    OtherDeviceSaves(usize, bool),
    /// The saved list: open, page, close, be answered.
    SavedOpens,
    SavedMore,
    SavedCloses,
    SavedAnswered(Outcome),
    /// The people directory and profiles.
    PeopleOpens,
    PeopleSearches(usize),
    PeopleMore,
    PeopleCloses,
    PeopleAnswered(Outcome),
    ProfileOpens(bool),
    ProfileCloses,
    ProfileAnswered(Outcome),
    /// The channel directory.
    BrowseOpens,
    BrowseSearches(usize),
    BrowseFilters(usize),
    BrowseSorts(bool),
    BrowseMore,
    BrowseCloses,
    BrowseAnswered(Outcome),
    /// The person uses the quick switcher: which text, which result.
    QuickSwitches(usize, usize),
    /// Workspace administration: another admin changes the workspace; the person does.
    AdminRenames(usize),
    UserUpdatesWorkspace(usize),
    /// The server changes the person's role, or the other person's.
    ServerSetsMyRole(usize),
    ServerSetsOtherRole(usize),
    /// An admin deactivates or reactivates the other person.
    ServerDeactivatesOther(bool),
    /// The person sets a role (their own, or the other person's) or deactivates the other person.
    UserSetsRole(bool, usize),
    UserDeactivates(bool),
    PeopleIncludeDeactivated(bool),
    /// The invitations: open, close, create, revoke, be answered, dismiss the code.
    InvitationsOpens,
    InvitationsCloses,
    UserCreatesInvitation(usize),
    UserRevokesInvitation(usize),
    InvitationsAnswered(Outcome),
    InvitationCodeDismissed,
    /// The person is deactivated (they lose the workspace) and later let back in; the person
    /// checks again.
    ServerDeactivatesMe,
    ServerReactivatesMe,
    AccessRechecked,
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
        2 => any::<usize>().prop_map(Action::ServerPoke),
        3 => any::<usize>().prop_map(Action::OtherEdits),
        2 => any::<usize>().prop_map(Action::ServerDeletes),
        8 => any::<usize>().prop_map(Action::UserEdits),
        8 => any::<usize>().prop_map(Action::UserEditRaces),
        1 => any::<usize>().prop_map(Action::UserDeletes),
        4 => (any::<usize>(), any::<usize>()).prop_map(|(m, e)| Action::UserReacts(m, e)),
        5 => (any::<bool>(), any::<usize>()).prop_map(|(c, k)| Action::OtherMentions(c, k)),
        2 => any::<bool>().prop_map(Action::ThirdToggles),
        4 => (any::<usize>(), any::<usize>(), any::<usize>())
            .prop_map(|(m, e, w)| Action::OtherReacts(m, e, w)),
        6 => (any::<usize>(), any::<bool>(), any::<usize>())
            .prop_map(|(r, c, k)| Action::OtherReplies(r, c, k)),
        3 => (any::<usize>(), any::<usize>()).prop_map(|(m, e)| Action::UserReactsWide(m, e)),
        1 => any::<usize>().prop_map(Action::FillReactions),
        3 => any::<usize>().prop_map(Action::UserSwapsReaction),
        5 => (any::<bool>(), any::<usize>()).prop_map(|(c, k)| Action::OtherRepliesToOpen(c, k)),
        4 => any::<usize>().prop_map(Action::OpenThread),
        1 => Just(Action::CloseThread),
        2 => Just(Action::LoadOlderReplies),
        5 => any::<bool>().prop_map(Action::UserReplies),
        6 => any::<bool>().prop_map(Action::ThreadBottom),
        3 => (any::<usize>(), any::<bool>()).prop_map(|(r, f)| Action::UserFollows(r, f)),
        1 => (any::<usize>(), any::<bool>()).prop_map(|(r, f)| Action::OtherDeviceFollows(r, f)),
        2 => any::<usize>().prop_map(Action::OtherDeviceReadsThread),
        2 => (any::<bool>(), any::<usize>(), any::<bool>())
            .prop_map(|(c, n, b)| Action::UserAttaches(c, n, b)),
        1 => any::<usize>().prop_map(Action::UserAttachesToThread),
        5 => (any::<bool>(), any::<usize>(), any::<bool>())
            .prop_map(|(c, n, o)| Action::UserSendsFiles(c, n, o)),
        4 => (any::<bool>(), any::<usize>(), any::<usize>())
            .prop_map(|(c, p, w)| Action::UserHandlesChip(c, p, w)),
        2 => (any::<usize>(), any::<bool>()).prop_map(|(p, r)| Action::UserHandlesFailedFiles(p, r)),
        1 => Just(Action::ServerExpiresUploads),
        2 => (any::<usize>(), any::<usize>()).prop_map(|(p, w)| Action::UserViewsFile(p, w)),
        2 => Just(Action::ServerAddsEmoji),
        2 => any::<usize>().prop_map(Action::ServerRenamesEmoji),
        2 => any::<usize>().prop_map(Action::ServerDeletesEmoji),
        2 => (any::<bool>(), any::<usize>()).prop_map(|(c, e)| Action::UserSendsEmoji(c, e)),
        2 => (any::<usize>(), any::<usize>()).prop_map(|(m, e)| Action::UserReactsCustom(m, e)),
        2 => (any::<usize>(), any::<usize>()).prop_map(|(m, e)| Action::OtherReactsCustom(m, e)),
        2 => any::<usize>().prop_map(Action::UserViewsEmoji),
        1 => any::<usize>().prop_map(Action::UserRecordsEmoji),
        1 => any::<usize>().prop_map(Action::SearchTypes),
        2 => any::<usize>().prop_map(Action::SearchRuns),
        2 => Just(Action::SearchMore),
        1 => any::<bool>().prop_map(Action::SearchSorts),
        3 => any::<usize>().prop_map(Action::SearchOpensHit),
        1 => Just(Action::SearchCloses),
        3 => outcome().prop_map(Action::SearchAnswered),
        3 => (any::<usize>(), outcome()).prop_map(|(w, o)| Action::SearchRunsAnswered(w, o)),
        2 => outcome().prop_map(Action::SearchPagesOn),
        2 => any::<usize>().prop_map(Action::ServerDeletesHit),
        2 => any::<usize>().prop_map(Action::OtherEditsHit),
        2 => (1usize..90).prop_map(Action::Tick),
        2 => any::<usize>().prop_map(Action::UserSetsSettings),
        1 => any::<usize>().prop_map(Action::UserSnoozes),
        2 => (any::<bool>(), any::<usize>()).prop_map(|(c, p)| Action::UserSetsPrefs(c, p)),
        1 => any::<usize>().prop_map(Action::OtherDeviceSetsSettings),
        1 => (any::<bool>(), any::<usize>()).prop_map(|(c, p)| Action::OtherDeviceSetsPrefs(c, p)),
        1 => any::<usize>().prop_map(Action::OtherDeviceSetsStatus),
        1 => any::<usize>().prop_map(Action::UserSetsStatus),
        1 => any::<usize>().prop_map(Action::OtherSetsStatus),
        3 => Just(Action::NotificationClicked),
        2 => Just(Action::NotifiedMessageDeleted),
        1 => Just(Action::InboxOpens),
        1 => any::<bool>().prop_map(Action::InboxFilters),
        1 => Just(Action::InboxMore),
        1 => Just(Action::InboxMarksRead),
        1 => Just(Action::InboxCloses),
        3 => outcome().prop_map(Action::InboxAnswered),
        2 => (any::<usize>(), any::<bool>()).prop_map(|(m, s)| Action::UserSaves(m, s)),
        1 => (any::<usize>(), any::<bool>()).prop_map(|(m, s)| Action::OtherDeviceSaves(m, s)),
        2 => Just(Action::SavedOpens),
        1 => Just(Action::SavedMore),
        1 => Just(Action::SavedCloses),
        3 => outcome().prop_map(Action::SavedAnswered),
        1 => Just(Action::PeopleOpens),
        1 => any::<usize>().prop_map(Action::PeopleSearches),
        1 => Just(Action::PeopleMore),
        1 => Just(Action::PeopleCloses),
        3 => outcome().prop_map(Action::PeopleAnswered),
        1 => any::<bool>().prop_map(Action::ProfileOpens),
        1 => Just(Action::ProfileCloses),
        2 => outcome().prop_map(Action::ProfileAnswered),
        1 => Just(Action::BrowseOpens),
        1 => any::<usize>().prop_map(Action::BrowseSearches),
        1 => any::<usize>().prop_map(Action::BrowseFilters),
        1 => any::<bool>().prop_map(Action::BrowseSorts),
        1 => Just(Action::BrowseMore),
        1 => Just(Action::BrowseCloses),
        3 => outcome().prop_map(Action::BrowseAnswered),
        2 => (any::<usize>(), any::<usize>()).prop_map(|(t, r)| Action::QuickSwitches(t, r)),
        1 => any::<usize>().prop_map(Action::AdminRenames),
        1 => any::<usize>().prop_map(Action::UserUpdatesWorkspace),
        1 => any::<usize>().prop_map(Action::ServerSetsMyRole),
        1 => any::<usize>().prop_map(Action::ServerSetsOtherRole),
        2 => any::<bool>().prop_map(Action::ServerDeactivatesOther),
        2 => (any::<bool>(), any::<usize>()).prop_map(|(s, r)| Action::UserSetsRole(s, r)),
        2 => any::<bool>().prop_map(Action::UserDeactivates),
        1 => any::<bool>().prop_map(Action::PeopleIncludeDeactivated),
        2 => Just(Action::InvitationsOpens),
        1 => Just(Action::InvitationsCloses),
        4 => any::<usize>().prop_map(Action::UserCreatesInvitation),
        1 => any::<usize>().prop_map(Action::UserRevokesInvitation),
        2 => outcome().prop_map(Action::InvitationsAnswered),
        1 => Just(Action::InvitationCodeDismissed),
        1 => Just(Action::ServerDeactivatesMe),
        1 => Just(Action::ServerReactivatesMe),
        1 => Just(Action::AccessRechecked),
    ]
}

enum LogEvent {
    Message {
        message: Message,
        operation: Option<OperationId>,
    },
    /// The person's read state changed on the server (a mark-read, or another device).
    State(ConversationUserState),
    /// A message was edited; `message` is the new revision.
    Edited {
        message: Message,
        operation: Option<OperationId>,
        /// Who the message notified before this edit.
        previous: Mentions,
    },
    /// A message was deleted; `message` is the tombstone and `had` who it notified.
    Deleted { message: Message, had: Mentions },
    /// A third person joined or left a channel.
    Membership {
        conversation: ConversationId,
        user: UserId,
        added: bool,
    },
    /// The reactions on a message changed; `reactions` is the complete new list.
    Reactions {
        conversation: ConversationId,
        message: MessageId,
        reactions: Vec<ReactionSummary>,
    },
    /// The replies under a root changed; `summary` is the complete new summary.
    ThreadChanged {
        conversation: ConversationId,
        root: MessageId,
        summary: Option<ThreadSummary>,
    },
    /// The person's own state for a thread changed.
    ThreadState(ThreadUserState),
    /// A custom emoji was added, renamed, or deleted; `emoji` is the whole emoji now.
    Emoji { emoji: CustomEmoji },
    /// The person's notification settings changed (announced to every device of theirs).
    Settings(NotificationSettings),
    /// The person's preference for a conversation changed.
    Prefs(ConversationNotificationPrefs),
    /// Someone set or cleared their status.
    UserStatus {
        user: UserId,
        status: Option<UserStatus>,
    },
    /// The person saved a message or removed it from their saved messages.
    Saved { message: MessageId, saved: bool },
    /// The workspace's name or settings changed; `Workspace` is the whole workspace now.
    Workspace(Workspace),
    /// Someone's role changed.
    Role { user: UserId, role: WorkspaceRole },
    /// Someone was deactivated or reactivated.
    Deactivated { user: UserId, deactivated: bool },
}

/// The person's record for a thread on the server (`thread_user_state`). `unfollowed` is the
/// choice to stop, which replies and mentions by others do not undo.
#[derive(Debug, Clone, Copy, Default)]
struct ThreadRec {
    following: bool,
    unfollowed: bool,
    last_read: i64,
}

/// Whether a message is in its conversation's history: top-level messages, and replies sent also
/// to the channel.
fn is_listed(message: &Message) -> bool {
    message.thread_root.is_none() || message.in_channel
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

    /// The message a creation event is about.
    fn message(&self) -> Option<&Message> {
        match &self.event {
            LogEvent::Message { message, .. } => Some(message),
            _ => None,
        }
    }

    fn deleted(&self) -> Option<&Message> {
        match &self.event {
            LogEvent::Deleted { message, .. } => Some(message),
            _ => None,
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
    /// The latest version of every message, a tombstone once deleted.
    versions: HashMap<MessageId, Message>,
    /// What the server answers to a repeated edit operation: its original result.
    edit_receipts: HashMap<OperationId, Message>,
    /// Operations that are edits, not sends.
    edit_operations: HashSet<OperationId>,
    /// Source of distinct edit texts.
    edits_made: usize,
    /// A third person, who only appears in reaction lists.
    third: UserId,
    /// The highest reaction version each message has shown since this run of the app started.
    /// A row never goes back to an older list.
    shown_reactions: HashMap<MessageId, EventSeq>,
    /// The server's record of the person's following flag and read position per thread.
    thread_records: HashMap<MessageId, ThreadRec>,
    /// Roots the person has replied to, whose threads must be opened to settle those replies.
    reply_roots: HashSet<MessageId>,
    /// Operations that are replies, and those of them whose delivery was ever disturbed (a lost
    /// request or answer, or a restart). The person cannot always see a reply that was given up
    /// on (its thread may be closed), so these are exempt from the order check, as a retried
    /// message in the channel is.
    reply_ops: HashSet<OperationId>,
    disturbed: HashSet<OperationId>,
    /// Threads whose root was deleted and whose last reply then went: the server dropped
    /// everyone's state for them without telling anyone.
    purged: HashSet<MessageId>,
    /// The states the file of what the person was looking at could be in: every write since the
    /// file was last cleared, oldest first, starting from nothing saved. After a crash any may
    /// have survived.
    view_disk: Vec<ViewState>,
    /// After a restart: the conversation the surviving save names, which the app must reopen on
    /// unless the person went elsewhere first.
    expected_restore: Option<ConversationId>,

    // ----- Files -----
    /// Source of file names.
    files_attached: usize,
    /// The runtime's outbox: the copies of files that are made and not yet released.
    outbox: HashSet<AttachmentId>,
    /// How many times each file has been released, so a copy that finishes after its release
    /// (and is deleted by it) can be told from one that does not.
    release_epoch: HashMap<AttachmentId, usize>,
    /// The release count when each request to copy a file was made.
    stage_epoch: HashMap<(AttachmentId, u64), usize>,
    /// The uploads the server holds that no message has claimed.
    server_uploads: HashMap<AttachmentId, Attachment>,
    /// The files that committed messages carry.
    attached: HashSet<AttachmentId>,
    /// The uploads the runtime would be running: file and attempt.
    uploads_running: HashMap<AttachmentId, u64>,
    /// Uploads the core cancelled whose answer may still turn up.
    upload_cancelled: HashSet<(AttachmentId, u64)>,
    downloads_running: HashSet<AttachmentId>,
    images_in_flight: HashSet<(AttachmentId, bool)>,
    requested_images: HashSet<AttachmentId>,
    /// The files each journaled message named, in order.
    files_by_op: HashMap<OperationId, Vec<AttachmentId>>,
    /// Failed messages with files that the person has tried again, bounding how often.
    file_retries_left: usize,
    /// Source of custom emoji names.
    emoji_made: usize,
    emoji_in_flight: HashSet<EmojiId>,
    requested_emoji: HashSet<EmojiId>,
    /// The numbers of messages the person tried again after a refusal: the new operation lands
    /// behind later messages, as a retried message does.
    retried_numbers: HashSet<usize>,

    // ----- Search -----
    /// The word of the last run the core asked for; every hit on screen answers it.
    last_run_word: Option<String>,
    /// The order of that run.
    last_run_sort: SearchSort,
    /// The highest search generation seen: requests are numbered upward, never reused.
    last_search_generation: u64,
    // ----- Notifications -----
    /// Seconds since the simulation's start, and whether the core has been told the time.
    clock: i64,
    ticked: bool,
    server_settings: NotificationSettings,
    server_prefs: HashMap<ConversationId, ConversationNotificationPrefs>,
    server_status: Option<UserStatus>,
    other_status: Option<UserStatus>,
    /// Whether the connection has caught up since it was last opened (what makes an event news).
    socket_live: bool,
    /// The notification the last input produced, and the latest one the person could click.
    last_notified: Option<DesktopNotification>,
    latest_notification: Option<DesktopNotification>,
    notified: HashSet<MessageId>,
    /// The attention count last announced.
    badge_seen: u32,

    // ----- Discovery -----
    /// The messages the person has saved, each with the number the server gave it when saved.
    server_saved: HashMap<MessageId, u64>,
    saved_counter: u64,
    /// The public channels of the workspace, with their member counts: the two the person is in
    /// and three they are not.
    directory: Vec<(lazy_protocol::Conversation, u32)>,
    /// The newest request of each kind the core has made.
    saved_newest: u64,
    people_newest: u64,
    profile_newest: u64,
    browse_newest: u64,
    /// What the core must list as recently visited conversations, and the states the file could
    /// be in.
    recent_conversations_expected: Vec<ConversationId>,
    recent_conversations_disk: Vec<Vec<ConversationId>>,
    /// What the core must list as recent searches: the last list it asked to save, or the one
    /// found at start.
    recents_expected: Vec<String>,
    /// The states the file of recent searches could be in, as with the cache.
    recents_disk: Vec<Vec<String>>,

    // ----- Administration -----
    /// The workspace's name and settings, and the two people's roles, as the server has them.
    server_workspace: Workspace,
    my_role: WorkspaceRole,
    other_role: WorkspaceRole,
    other_deactivated: bool,
    /// The person has been deactivated: the server lists no workspace for them and refuses them.
    me_deactivated: bool,
    /// The server ended the person's subscription (they were refused it): nothing more is
    /// delivered until the core subscribes again.
    connection_lost: bool,
    /// The workspace's invitations, newest first, and every code the server has issued.
    invitations: Vec<Invitation>,
    codes_issued: Vec<String>,
    invitations_newest: u64,
    /// The core started from a cache that no bootstrap has confirmed yet.
    unconfirmed: bool,
}

impl Sim {
    fn new() -> Self {
        let mut world = World::new();
        // The person starts as an admin: the administration tools are theirs until a role event
        // takes them away.
        world.role = WorkspaceRole::Admin;
        world.custom_emoji = ["sim_alpha", "sim_beta"]
            .into_iter()
            .map(|name| CustomEmoji {
                id: EmojiId::generate(),
                workspace_id: world.workspace.id,
                name: name.to_owned(),
                created_by: world.other.id,
                created_at: Timestamp::now(),
                deleted: false,
            })
            .collect();
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
            versions: HashMap::new(),
            edit_receipts: HashMap::new(),
            edit_operations: HashSet::new(),
            edits_made: 0,
            third: UserId::generate(),
            shown_reactions: HashMap::new(),
            thread_records: HashMap::new(),
            reply_roots: HashSet::new(),
            reply_ops: HashSet::new(),
            disturbed: HashSet::new(),
            purged: HashSet::new(),
            view_disk: vec![ViewState::default()],
            expected_restore: None,
            files_attached: 0,
            outbox: HashSet::new(),
            release_epoch: HashMap::new(),
            stage_epoch: HashMap::new(),
            server_uploads: HashMap::new(),
            attached: HashSet::new(),
            uploads_running: HashMap::new(),
            upload_cancelled: HashSet::new(),
            downloads_running: HashSet::new(),
            images_in_flight: HashSet::new(),
            requested_images: HashSet::new(),
            files_by_op: HashMap::new(),
            file_retries_left: 60,
            emoji_made: 0,
            emoji_in_flight: HashSet::new(),
            requested_emoji: HashSet::new(),
            retried_numbers: HashSet::new(),
            last_run_word: None,
            last_run_sort: SearchSort::Relevance,
            last_search_generation: 0,
            clock: 0,
            ticked: false,
            server_settings: NotificationSettings::default(),
            server_prefs: HashMap::new(),
            server_status: None,
            other_status: None,
            socket_live: false,
            last_notified: None,
            latest_notification: None,
            notified: HashSet::new(),
            badge_seen: 0,
            server_saved: HashMap::new(),
            saved_counter: 0,
            directory: Vec::new(),
            saved_newest: 0,
            people_newest: 0,
            profile_newest: 0,
            browse_newest: 0,
            recent_conversations_expected: Vec::new(),
            recent_conversations_disk: vec![Vec::new()],
            recents_expected: Vec::new(),
            recents_disk: vec![Vec::new()],
            server_workspace: Workspace {
                id: lazy_domain::WorkspaceId::generate(),
                name: String::new(),
                settings: WorkspaceSettings::default(),
            },
            my_role: WorkspaceRole::Admin,
            other_role: WorkspaceRole::Member,
            other_deactivated: false,
            me_deactivated: false,
            connection_lost: false,
            invitations: Vec::new(),
            codes_issued: Vec::new(),
            invitations_newest: 0,
            unconfirmed: false,
        };
        sim.server_workspace = sim.world.workspace.clone();
        let workspace = sim.world.workspace.id;
        sim.directory = vec![
            (sim.world.channel.clone(), 5),
            (sim.world.second_channel.clone(), 3),
        ];
        for (name, members) in [("alpha", 9), ("beta", 1), ("gamma", 4)] {
            sim.directory.push((channel(workspace, name), members));
        }
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
            Input::Frame(ServerFrame::Event(e)) => {
                let me = self.world.me.id;
                let what = match &e.event {
                    Event::MessageCreated { message, .. } => format!(
                        "created at {} by {} mentions_me={} deleted={}",
                        message.seq,
                        if message.author_id == me {
                            "me"
                        } else {
                            "other"
                        },
                        message.mentions.includes(me),
                        message.deleted
                    ),
                    Event::MessageEdited {
                        message,
                        previous_mentions,
                        ..
                    } => format!(
                        "edited message at {} previous_mentions_me={} now_mentions_me={} deleted={}",
                        message.seq,
                        previous_mentions.includes(me),
                        message.mentions.includes(me),
                        message.deleted
                    ),
                    Event::MessageDeleted {
                        message,
                        had_mentions,
                    } => format!(
                        "deleted message at {} had_mentions_me={}",
                        message.seq,
                        had_mentions.includes(me)
                    ),
                    Event::UserStateChanged { state } => format!(
                        "state last_read={} unread={} mentions={}",
                        state.last_read, state.unread_count, state.mention_count
                    ),
                    other => debug_capped(other, 30),
                };
                format!("Event seq={} {what}", e.seq)
            }
            Input::BootstrapLoaded(b) => match b.workspaces.first() {
                Some(w) => format!("BootstrapLoaded head={}", w.head),
                None => "BootstrapLoaded with no workspace".to_owned(),
            },
            Input::SelectConversation(c) => format!("Select {}", name(*c)),
            Input::LoadOlder { conversation } => format!("LoadOlder {}", name(*conversation)),
            other => debug_capped(other, 240),
        };
        self.trace.push(format!("log={} <- {line}", self.log.len()));
        let opens_a_session = matches!(input, Input::Started(_) | Input::BootstrapLoaded(_));
        // A start from a cache is unconfirmed until a bootstrap answers: the cache holds no
        // notification settings, so nothing is notified meanwhile.
        match &input {
            Input::Started(state) => self.unconfirmed = state.cache.is_some(),
            Input::BootstrapLoaded(_) => self.unconfirmed = false,
            _ => {}
        }
        let hits_before = matches!(input, Input::Frame(_)).then(|| self.search_snapshot());
        let out = self.driver.send(input);
        // A debugging aid: `SIM_DEBUG=1` prints each input and the read states after it.
        if std::env::var_os("SIM_DEBUG").is_some() {
            eprintln!("  >> {}", self.trace.last().cloned().unwrap_or_default());
            for c in [self.world.channel.id, self.world.second_channel.id] {
                if let Some(st) = self.driver.core.user_state(c) {
                    eprintln!(
                        "      state {} unread={} mentions={} last_read={}",
                        if c == self.world.channel.id {
                            "general"
                        } else {
                            "random"
                        },
                        st.unread_count,
                        st.mention_count,
                        st.last_read
                    );
                }
            }
        }
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
                other => debug_capped(other, 200),
            };
            self.trace.push(format!("      -> {line}"));
        }
        self.process(out);
        self.trim_superseded_requests();
        self.check_invariants();
        self.note_given_up();
        self.check_restored_selection(opens_a_session);
        if let Some(before) = hits_before {
            self.check_hits_after_event(before);
        }
        // The screen scrolls to a message the person opened from a search and says so.
        if self.driver.core.jump_target().is_some() {
            JUMPS_REACHED.fetch_add(1, Ordering::Relaxed);
            self.driver.send(Input::ClearJumpTarget);
        }
    }

    /// The view's generation and the revision of each hit.
    fn search_snapshot(&self) -> Option<(u64, HashMap<MessageId, u32>)> {
        self.driver.core.search().map(|view| {
            (
                view.generation,
                view.hits
                    .iter()
                    .map(|hit| (hit.message.id, hit.message.revision))
                    .collect(),
            )
        })
    }

    /// An event changes hits in two ways only: a deleted message stops being one, and an edited
    /// one shows its new revision.
    fn check_hits_after_event(&self, before: Option<(u64, HashMap<MessageId, u32>)>) {
        let (then, before, now, after) = match (before, self.search_snapshot()) {
            (None, None) => return,
            (Some((then, before)), Some((now, after))) => (then, before, now, after),
            _ => panic!("an event opened or closed the search"),
        };
        if then != now {
            return;
        }
        for (id, revision) in &before {
            match after.get(id) {
                None => {
                    assert!(
                        self.versions[id].deleted,
                        "a hit went although its message is not deleted\n{}",
                        self.trace.join("\n")
                    );
                    HITS_REMOVED_BY_DELETION.fetch_add(1, Ordering::Relaxed);
                }
                Some(now) => {
                    assert!(now >= revision, "a hit went back to an older revision");
                    if now > revision {
                        HITS_UPDATED_BY_EDIT.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        assert!(
            after.keys().all(|id| before.contains_key(id)),
            "an event added a hit"
        );
    }

    /// After a restart the app reopens on the conversation it was last on, if the person is
    /// still in it, unless they chose another first.
    fn check_restored_selection(&mut self, just_started_or_bootstrapped: bool) {
        if !just_started_or_bootstrapped {
            return;
        }
        let (Some(expected), Some(selected)) = (
            self.expected_restore,
            self.driver.core.selected_conversation(),
        ) else {
            return;
        };
        self.expected_restore = None;
        RESTORED_SELECTIONS_CHECKED.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            selected,
            expected,
            "the app did not reopen on the conversation it was last on\n{}",
            self.trace.join("\n")
        );
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
                    // A new subscription replays before it is live.
                    self.socket_live = false;
                    self.connection_lost = false;
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
                Effect::PersistDrafts(_)
                | Effect::PersistThreadDrafts(_)
                | Effect::PersistEmojiPrefs(_) => {}
                Effect::PersistView(view) => self.view_disk.push(view),
                Effect::SetBadge(count) => {
                    assert_eq!(
                        count,
                        self.driver.core.attention_count(),
                        "the badge announced is the attention count\n{}",
                        self.trace.join("\n")
                    );
                    self.badge_seen = count;
                    BADGES_ANNOUNCED.fetch_add(1, Ordering::Relaxed);
                }
                Effect::Notify(notification) => {
                    assert!(
                        self.notified.insert(notification.message),
                        "a message notified twice\n{}",
                        self.trace.join("\n")
                    );
                    assert!(notification.count >= 1);
                    assert!(notification.body.chars().count() <= 140);
                    self.last_notified = Some(notification.clone());
                    self.latest_notification = Some(notification);
                    NOTIFICATIONS_SENT.fetch_add(1, Ordering::Relaxed);
                }
                Effect::ClearNotifications { .. } => {
                    NOTIFICATIONS_CLEARED.fetch_add(1, Ordering::Relaxed);
                }
                Effect::RetractNotification { message } => {
                    assert!(
                        self.notified.contains(&message),
                        "a notification retracted that was never shown"
                    );
                    NOTIFICATIONS_RETRACTED.fetch_add(1, Ordering::Relaxed);
                }
                Effect::CancelInbox => {
                    assert!(
                        self.pending
                            .iter()
                            .any(|(pending, _)| matches!(pending, Effect::LoadInbox { .. })),
                        "the core cancelled an inbox request that nothing was waiting for"
                    );
                }
                Effect::LoadSaved {
                    generation,
                    ref query,
                    ..
                } => {
                    assert!(generation > self.saved_newest);
                    self.saved_newest = generation;
                    let view = self.driver.core.saved();
                    assert!(
                        view.open,
                        "the saved list is asked for only while it is open"
                    );
                    if let Some(before) = query.before {
                        assert_eq!(
                            view.items.last().map(|item| item.cursor),
                            Some(before),
                            "more saved items start after the last one shown"
                        );
                    }
                    self.pending.push((effect, self.log.len()));
                }
                Effect::LoadPeople {
                    generation,
                    ref query,
                    ..
                } => {
                    assert!(generation > self.people_newest);
                    self.people_newest = generation;
                    let view = self.driver.core.people();
                    assert!(view.open);
                    if let Some(offset) = query.offset {
                        assert_eq!(offset as usize, view.people.len());
                    }
                    self.pending.push((effect, self.log.len()));
                }
                Effect::LoadProfile { generation, .. } => {
                    assert!(generation > self.profile_newest);
                    self.profile_newest = generation;
                    assert!(self.driver.core.profile().is_some());
                    self.pending.push((effect, self.log.len()));
                }
                Effect::LoadBrowse {
                    generation,
                    ref query,
                    ..
                } => {
                    assert!(generation > self.browse_newest);
                    self.browse_newest = generation;
                    let view = self.driver.core.browse();
                    assert_eq!(query.joined, view.joined);
                    assert_eq!(query.sort, Some(view.sort));
                    if let Some(offset) = query.offset {
                        assert_eq!(offset as usize, view.channels.len());
                    }
                    self.pending.push((effect, self.log.len()));
                }
                Effect::CancelSaved
                | Effect::CancelPeople
                | Effect::CancelProfile
                | Effect::CancelBrowse => {
                    let kind = |e: &Effect| match e {
                        Effect::LoadSaved { .. } => 0,
                        Effect::LoadPeople { .. } => 1,
                        Effect::LoadProfile { .. } => 2,
                        Effect::LoadBrowse { .. } => 3,
                        _ => 9,
                    };
                    let wanted = match effect {
                        Effect::CancelSaved => 0,
                        Effect::CancelPeople => 1,
                        Effect::CancelProfile => 2,
                        _ => 3,
                    };
                    assert!(
                        self.pending
                            .iter()
                            .any(|(pending, _)| kind(pending) == wanted),
                        "the core cancelled a request that nothing was waiting for"
                    );
                }
                Effect::LoadInvitations { generation, .. } => {
                    assert!(
                        generation > self.invitations_newest,
                        "invitation requests are numbered upward"
                    );
                    self.invitations_newest = generation;
                    assert!(
                        self.driver.core.invitations().open,
                        "invitations are asked for only while they are open\n{}",
                        self.trace.join("\n")
                    );
                    assert!(self.driver.core.can_manage_workspace());
                    self.pending.push((effect, self.log.len()));
                }
                Effect::CancelInvitations => {
                    assert!(
                        self.pending
                            .iter()
                            .any(|(pending, _)| matches!(pending, Effect::LoadInvitations { .. })),
                        "the core cancelled an invitations request that nothing was waiting for"
                    );
                }
                Effect::Unsubscribe { .. } => {
                    assert!(
                        self.me_deactivated,
                        "the core stopped reading a workspace the server still lists\n{}",
                        self.trace.join("\n")
                    );
                    SUBSCRIPTIONS_ENDED.fetch_add(1, Ordering::Relaxed);
                }
                Effect::PersistRecentConversations(list) => {
                    assert!(list.len() <= MAX_RECENT_CONVERSATIONS);
                    let distinct: HashSet<_> = list.iter().collect();
                    assert_eq!(distinct.len(), list.len(), "a conversation twice: {list:?}");
                    self.recent_conversations_expected = list.clone();
                    self.recent_conversations_disk.push(list);
                }
                Effect::PersistRecentSearches(list) => {
                    assert!(
                        list.len() <= MAX_RECENT_SEARCHES,
                        "too many recent searches"
                    );
                    let mut distinct = HashSet::new();
                    for text in &list {
                        assert!(
                            !text.is_empty() && text.trim() == text,
                            "a recent search that is blank or untrimmed: {text:?}"
                        );
                        assert!(
                            distinct.insert(text.to_lowercase()),
                            "a recent search twice: {list:?}"
                        );
                    }
                    RECENT_SEARCHES_SAVED.fetch_add(1, Ordering::Relaxed);
                    self.recents_expected = list.clone();
                    self.recents_disk.push(list);
                }
                Effect::Search {
                    generation,
                    ref params,
                    ..
                } => {
                    assert!(
                        generation > self.last_search_generation,
                        "search requests are numbered upward"
                    );
                    self.last_search_generation = generation;
                    let limit = params.limit.expect("a page size is asked for") as usize;
                    let offset = params.offset.expect("an offset is asked for") as usize;
                    assert!(limit <= MAX_SEARCH_LIMIT && offset <= MAX_SEARCH_OFFSET);
                    let view = self
                        .driver
                        .core
                        .search()
                        .expect("a search is asked for only while it is open");
                    assert_eq!(view.generation, generation);
                    assert_eq!(view.status, SearchStatus::Loading);
                    assert_eq!(offset, view.hits.len(), "pages continue where the hits end");
                    if offset == 0 {
                        // The text and order the person set, or, after a resync, the run
                        // that was on screen.
                        assert!(
                            params.q == view.query.trim()
                                || Some(&params.q) == self.last_run_word.as_ref(),
                            "a search asks for text that is neither typed nor shown"
                        );
                        assert!(
                            params.sort == Some(view.sort)
                                || params.sort == Some(self.last_run_sort),
                        );
                        self.last_run_word = Some(params.q.clone());
                        self.last_run_sort = view.sort;
                        SEARCH_RUNS_ASKED.fetch_add(1, Ordering::Relaxed);
                    } else {
                        // More results of the run the hits answer, in its order, whatever has
                        // been typed since.
                        assert_eq!(Some(&params.q), self.last_run_word.as_ref());
                        assert_eq!(params.sort, Some(self.last_run_sort));
                        SEARCH_PAGES_ASKED.fetch_add(1, Ordering::Relaxed);
                    }
                    // A request the core has moved on from may still be answered: the runtime
                    // stops it, but an answer already on its way can arrive.
                    self.pending.push((effect, self.log.len()));
                }
                Effect::CancelSearch => {
                    assert!(
                        self.pending
                            .iter()
                            .any(|(pending, _)| matches!(pending, Effect::Search { .. })),
                        "the core cancelled a search that nothing was waiting for"
                    );
                    SEARCHES_CANCELLED.fetch_add(1, Ordering::Relaxed);
                }
                Effect::SettleSubmission { operation } => {
                    *self.settle_counts.entry(operation).or_default() += 1;
                    self.journal.remove(&operation);
                }
                Effect::PersistSubmission(ref entry) => {
                    self.created_ops.push(entry.operation_id);
                    if let Some(root) = entry.thread_root {
                        self.reply_roots.insert(root);
                        self.reply_ops.insert(entry.operation_id);
                    }
                    if matches!(entry.kind, JournalKind::Edit { .. }) {
                        self.edit_operations.insert(entry.operation_id);
                    } else {
                        // A message has its number as its words, or, when it is only files, in
                        // the names of the files that were attached with it.
                        let text = entry.document.plain_text();
                        let number: usize = text
                            .strip_prefix("mine-")
                            .and_then(leading_number)
                            .or_else(|| {
                                entry.attachments.iter().find_map(|file| {
                                    file.name
                                        .strip_prefix('f')
                                        .and_then(|rest| rest.split('-').next())
                                        .and_then(|n| n.parse().ok())
                                })
                            })
                            .expect("simulated message text");
                        // The same words sent again after a refusal come behind later messages.
                        if self.retried_numbers.contains(&number)
                            || self.submission_number.values().any(|seen| *seen == number)
                        {
                            self.given_up.insert(entry.operation_id);
                        }
                        self.submission_number.insert(entry.operation_id, number);
                        // A journal entry never names a file that is not in the outbox.
                        for file in &entry.attachments {
                            assert!(
                                self.outbox.contains(&file.id),
                                "the journal names a file that is not in the outbox\n{}",
                                self.trace.join("\n")
                            );
                        }
                        self.files_by_op.insert(
                            entry.operation_id,
                            entry.attachments.iter().map(|file| file.id).collect(),
                        );
                    }
                    self.pending.push((effect, self.log.len()));
                }
                Effect::StageFile {
                    attachment,
                    generation,
                    ..
                } => {
                    let epoch = self.release_epoch.get(&attachment).copied().unwrap_or(0);
                    self.stage_epoch.insert((attachment, generation), epoch);
                    self.pending.push((effect, self.log.len()));
                }
                Effect::UploadFile {
                    attachment,
                    generation,
                    ..
                } => {
                    self.uploads_running.insert(attachment, generation);
                    assert!(
                        self.uploads_running.len() <= MAX_CONCURRENT_UPLOADS,
                        "more than {MAX_CONCURRENT_UPLOADS} uploads at once\n{}",
                        self.trace.join("\n")
                    );
                    self.pending.push((effect, self.log.len()));
                }
                Effect::CancelUpload { attachment } => {
                    if let Some(generation) = self.uploads_running.remove(&attachment) {
                        self.upload_cancelled.insert((attachment, generation));
                    }
                }
                Effect::ReleaseFiles { attachments } => {
                    for id in attachments {
                        self.outbox.remove(&id);
                        *self.release_epoch.entry(id).or_default() += 1;
                        if let Some(generation) = self.uploads_running.remove(&id) {
                            self.upload_cancelled.insert((id, generation));
                        }
                    }
                }
                Effect::DownloadAttachment { ref attachment, .. } => {
                    assert!(
                        self.downloads_running.insert(attachment.id),
                        "a second download of one file was started while the first runs"
                    );
                    self.pending.push((effect, self.log.len()));
                }
                Effect::CancelDownload { attachment } => {
                    // The runtime stops without a word.
                    self.downloads_running.remove(&attachment);
                    self.pending.retain(|(pending, _)| {
                        !matches!(pending, Effect::DownloadAttachment { attachment: a, .. } if a.id == attachment)
                    });
                }
                Effect::FetchImage { attachment, full } => {
                    assert!(
                        self.images_in_flight.insert((attachment, full)),
                        "an image was asked for twice at once"
                    );
                    assert!(
                        self.images_in_flight.len() <= MAX_CONCURRENT_IMAGE_FETCHES,
                        "more image fetches at once than allowed"
                    );
                    self.pending.push((effect, self.log.len()));
                }
                Effect::ForgetAttachments { .. } | Effect::ForgetEmojiImages { .. } => {}
                Effect::PersistActiveWorkspace(_) | Effect::PersistAppearance(_) => {}
                Effect::FetchEmojiImage { emoji } => {
                    assert!(
                        self.emoji_in_flight.insert(emoji),
                        "an emoji image was asked for twice at once"
                    );
                    assert!(
                        self.emoji_in_flight.len() <= MAX_CONCURRENT_IMAGE_FETCHES,
                        "more emoji image fetches at once than allowed"
                    );
                    self.pending.push((effect, self.log.len()));
                }
                Effect::SetReaction {
                    message,
                    ref request,
                } if request.present => {
                    self.check_add_does_not_overtake_its_withdrawal(message, request.emoji);
                    self.pending.push((effect, self.log.len()));
                }
                other => self.pending.push((other, self.log.len())),
            }
        }
    }

    /// Requests the core has moved on from may still be answered (an answer already on its way
    /// when the runtime hangs up), but not unboundedly many: keep the newest two of each kind.
    fn trim_superseded_requests(&mut self) {
        let kind = |effect: &Effect| match effect {
            Effect::Search { .. } => Some(0),
            Effect::LoadInbox { .. } => Some(1),
            Effect::LoadSaved { .. } => Some(2),
            Effect::LoadPeople { .. } => Some(3),
            Effect::LoadProfile { .. } => Some(4),
            Effect::LoadBrowse { .. } => Some(5),
            Effect::LoadInvitations { .. } => Some(6),
            _ => None,
        };
        let mut seen = [0usize; 7];
        let mut keep = vec![true; self.pending.len()];
        for (index, (effect, _)) in self.pending.iter().enumerate().rev() {
            if let Some(kind) = kind(effect) {
                seen[kind] += 1;
                keep[index] = seen[kind] <= 2;
            }
        }
        let mut index = 0;
        self.pending.retain(|_| {
            index += 1;
            keep[index - 1]
        });
    }

    /// The core must not send an add that cannot fit while a withdrawal of the person's own that
    /// makes room for it is still in flight: the server would refuse it. (Checked only while the
    /// client has heard everything the server has, since it cannot know more than that.)
    fn check_add_does_not_overtake_its_withdrawal(&self, message: MessageId, emoji: ReactionEmoji) {
        if self.pointer != self.log.len() || !self.would_exceed_kinds(message, emoji) {
            return;
        }
        // The client can only act on what it holds: the message on screen at the version of the
        // server's reactions.
        let server = &self.versions[&message];
        let holds_current_list = self
            .driver
            .core
            .timeline(server.conversation_id)
            .iter()
            .any(|row| {
                row.message_id() == Some(message) && row.reactions_as_of == server.reactions_as_of
            });
        if !holds_current_list {
            return;
        }
        HOLD_CHECKS_MADE.fetch_add(1, Ordering::Relaxed);
        let me = self.world.me.id;
        let room_is_coming = self.pending.iter().any(|(effect, _)| match effect {
            Effect::SetReaction {
                message: pending,
                request,
            } if *pending == message && !request.present => self.versions[&message]
                .reactions
                .iter()
                .any(|r| r.emoji == request.emoji && r.users.iter().all(|user| *user == me)),
            _ => false,
        });
        assert!(
            !room_is_coming,
            "an add that cannot fit was sent while the withdrawal that makes room was in flight\n{}",
            self.trace.join("\n")
        );
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
        let deleted = self.deleted_ids();
        let unread_messages: Vec<&Message> = self
            .log
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| {
                m.conversation_id == conversation
                    && is_listed(m)
                    && m.author_id != me
                    && m.seq.get() > last_read
                    && m.seq.get() <= position
                    && !deleted.contains(&m.id)
            })
            .collect();
        let mentioning = unread_messages
            .iter()
            .filter(|m| self.versions[&m.id].mentions.includes(me))
            .count();
        ConversationUserState {
            conversation_id: conversation,
            favorite: false,
            last_read: seq(last_read),
            unread_count: u32::try_from(unread_messages.len()).unwrap(),
            mention_count: u32::try_from(mentioning).unwrap(),
        }
    }

    /// Moves the person's read position forward, as a mark-read or another device would, and
    /// records the change in the log so every client learns of it in order.
    fn server_reads(&mut self, conversation: ConversationId, up_to: i64) {
        let newest = self
            .log
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| m.conversation_id == conversation && is_listed(m))
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
        if self.me_deactivated {
            // The server no longer lists the workspace for a deactivated person.
            bootstrap.workspaces.clear();
            bootstrap.user.status.clone_from(&self.server_status);
            self.feed(Input::BootstrapLoaded(Box::new(bootstrap)));
            return;
        }
        bootstrap.workspaces[0].workspace = self.server_workspace.clone();
        bootstrap.workspaces[0].role = self.my_role;
        for member in &mut bootstrap.workspaces[0].members {
            if member.id == self.world.other.id {
                member.deactivated = self.other_deactivated;
            }
        }
        bootstrap.workspaces[0].head = self.head();
        bootstrap.workspaces[0].custom_emoji = self.emoji_as_of(self.log.len());
        bootstrap.workspaces[0].notification_settings = self.server_settings;
        bootstrap.workspaces[0].conversation_notification_prefs =
            self.server_prefs.values().copied().collect();
        bootstrap.workspaces[0].saved = self
            .saved_items()
            .into_iter()
            .map(|item| item.message.id)
            .collect();
        bootstrap.user.status.clone_from(&self.server_status);
        for member in &mut bootstrap.workspaces[0].members {
            if member.id == self.world.me.id {
                member.status.clone_from(&self.server_status);
            } else if member.id == self.world.other.id {
                member.status.clone_from(&self.other_status);
            }
        }
        bootstrap.workspaces[0].user_state = [self.world.channel.id, self.world.second_channel.id]
            .into_iter()
            .map(|c| self.server_state(c, BASE_HEAD + self.log.len() as i64))
            .collect();
        // The threads the person follows or has unread replies in.
        let mut roots: Vec<MessageId> = self.thread_records.keys().copied().collect();
        roots.sort();
        bootstrap.workspaces[0].threads = roots
            .into_iter()
            .map(|root| self.thread_state_at(root, BASE_HEAD + self.log.len() as i64))
            .filter(|state| state.following || state.unread_count > 0)
            .collect();
        self.feed(Input::BootstrapLoaded(Box::new(bootstrap)));
    }

    /// The server commits a send, once per operation. A reply needs a root that is a live
    /// top-level message of the same conversation; otherwise nothing is committed.
    fn commit(
        &mut self,
        operation: OperationId,
        conversation: ConversationId,
        document: &MessageDocument,
        thread: Option<(MessageId, bool)>,
        files: &[AttachmentId],
    ) -> Result<Message, RequestError> {
        if let Some(message) = self.committed.get(&operation) {
            // A repeated send answers with the message as it stands now: possibly edited, or a
            // tombstone.
            return Ok(self.versions[&message.id].clone());
        }
        let text = document.plain_text();
        let text = text.as_str();
        // Every emoji in the words must be one the workspace has now.
        let live = self.live_emoji();
        if document
            .custom_emoji()
            .iter()
            .any(|id| !live.iter().any(|e| e.id == *id))
        {
            EMOJI_USES_REFUSED.fetch_add(1, Ordering::Relaxed);
            return Err(RequestError::Rejected(ApiError::new(
                ErrorCode::InvalidRequest,
                "that emoji is gone",
            )));
        }
        // The files must be uploads the server holds that no message has claimed.
        let mut attachments = Vec::new();
        for id in files {
            match self.server_uploads.get(id) {
                Some(file) if !self.attached.contains(id) => attachments.push(file.clone()),
                _ => {
                    return Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::NotFound,
                        "no such upload",
                    )));
                }
            }
        }
        let me = self.world.me.id;
        let message = match thread {
            None => {
                let at = BASE_HEAD + 1 + self.log.len() as i64;
                let mut message = self.world.message(
                    conversation,
                    &self.world.me.clone(),
                    at,
                    if text.is_empty() { "x" } else { text },
                );
                message.document = document.clone();
                message.attachments = attachments;
                self.log
                    .push(LogEntry::message_entry(message.clone(), Some(operation)));
                // The server moves the author's read position in the same transaction, without
                // an event.
                let read = self.server_last_read.entry(conversation).or_insert(0);
                *read = (*read).max(at);
                self.versions.insert(message.id, message.clone());
                message
            }
            Some((root, in_channel)) => {
                let valid = self.versions.get(&root).is_some_and(|r| {
                    !r.deleted && r.thread_root.is_none() && r.conversation_id == conversation
                });
                if !valid {
                    return Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::NotFound,
                        "no such thread",
                    )));
                }
                self.server_reply_with(
                    me,
                    conversation,
                    root,
                    in_channel,
                    Mentions::default(),
                    text,
                    Some(operation),
                    attachments,
                    Some(document),
                )
            }
        };
        if !message.document.custom_emoji().is_empty() {
            EMOJI_MESSAGES_COMMITTED.fetch_add(1, Ordering::Relaxed);
        }
        if !message.attachments.is_empty() {
            MESSAGES_WITH_FILES_COMMITTED.fetch_add(1, Ordering::Relaxed);
            if message.document.has_no_content() {
                FILE_ONLY_MESSAGES_COMMITTED.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.attached
            .extend(message.attachments.iter().map(|file| file.id));
        self.committed.insert(operation, message.clone());
        Ok(message)
    }

    /// A reply commits: the message, the change to its root's summary, and the changes to the
    /// person's record of the thread that follow from it. Returns the reply.
    #[allow(clippy::too_many_arguments)]
    fn server_reply(
        &mut self,
        author: UserId,
        conversation: ConversationId,
        root: MessageId,
        in_channel: bool,
        mentions: Mentions,
        text: &str,
        operation: Option<OperationId>,
    ) -> Message {
        self.server_reply_with(
            author,
            conversation,
            root,
            in_channel,
            mentions,
            text,
            operation,
            Vec::new(),
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn server_reply_with(
        &mut self,
        author: UserId,
        conversation: ConversationId,
        root: MessageId,
        in_channel: bool,
        mentions: Mentions,
        text: &str,
        operation: Option<OperationId>,
        attachments: Vec<Attachment>,
        document: Option<&MessageDocument>,
    ) -> Message {
        let me = self.world.me.id;
        let previous_newest = self.live_replies(root).last().map_or(0, |m| m.seq.get());
        let at = BASE_HEAD + 1 + self.log.len() as i64;
        let author_user = if author == me {
            self.world.me.clone()
        } else {
            self.world.other.clone()
        };
        let mut message = self.world.message(
            conversation,
            &author_user,
            at,
            if text.is_empty() { "x" } else { text },
        );
        if text.is_empty() {
            message.document = MessageDocument::empty();
        }
        if let Some(document) = document {
            message.document = document.clone();
        }
        message.attachments = attachments;
        message.thread_root = Some(root);
        message.in_channel = in_channel;
        message.mentions = mentions;
        self.versions.insert(message.id, message.clone());
        self.log
            .push(LogEntry::message_entry(message.clone(), operation));
        // A reply never moves the author's read position in the channel, even one that was also
        // sent there: it may have been written from a thread pane.
        self.push_thread_changed(conversation, root);
        // Following: the replier always follows and has read the thread through their reply
        // (silently, unless following starts); the root's author and each person named follow
        // unless they chose to stop, starting with only this reply unread.
        let root_by_me = self.versions[&root].author_id == me;
        let named = message.mentions.users.contains(&me);
        let held = self.thread_records.get(&root).copied();
        let mut changed = false;
        if author == me {
            self.thread_records.insert(
                root,
                ThreadRec {
                    following: true,
                    unfollowed: false,
                    last_read: held.map_or(0, |rec| rec.last_read).max(at),
                },
            );
            changed = !held.is_some_and(|rec| rec.following);
        } else if root_by_me || named {
            match held {
                None => {
                    self.thread_records.insert(
                        root,
                        ThreadRec {
                            following: true,
                            unfollowed: false,
                            last_read: previous_newest,
                        },
                    );
                    changed = true;
                }
                Some(rec) if !rec.following && !rec.unfollowed => {
                    self.thread_records.insert(
                        root,
                        ThreadRec {
                            following: true,
                            ..rec
                        },
                    );
                    changed = true;
                }
                Some(_) => {}
            }
        }
        if changed {
            self.push_thread_state(root);
        }
        message
    }

    /// The live replies under a root, newest last.
    fn live_replies(&self, root: MessageId) -> Vec<&Message> {
        let mut replies: Vec<&Message> = self
            .log
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| m.thread_root == Some(root))
            .map(|m| &self.versions[&m.id])
            .filter(|m| !m.deleted)
            .collect();
        replies.sort_by_key(|m| m.seq);
        replies
    }

    /// Records that a root's replies changed: the new summary, at the position of the event.
    fn push_thread_changed(&mut self, conversation: ConversationId, root: MessageId) {
        let position = BASE_HEAD + 1 + self.log.len() as i64;
        let replies = self.live_replies(root);
        let summary = replies.last().map(|last| {
            let mut repliers: Vec<UserId> = Vec::new();
            for reply in replies.iter().rev() {
                if !repliers.contains(&reply.author_id) {
                    repliers.push(reply.author_id);
                }
            }
            repliers.truncate(lazy_domain::MAX_THREAD_REPLIERS_LISTED);
            ThreadSummary {
                reply_count: u32::try_from(replies.len()).unwrap(),
                last_reply_seq: last.seq,
                last_reply_at: last.created_at,
                recent_repliers: repliers,
                as_of: seq(position),
            }
        });
        if let Some(held) = self.versions.get_mut(&root) {
            held.thread = summary.clone();
        }
        self.log.push(LogEntry {
            event: LogEvent::ThreadChanged {
                conversation,
                root,
                summary,
            },
        });
    }

    /// The person's state for a thread as the server computes it for log position `position`.
    fn thread_state_at(&self, root: MessageId, position: i64) -> ThreadUserState {
        let Some(rec) = self.thread_records.get(&root).copied() else {
            // No state: the thread counts as read to its newest reply.
            return ThreadUserState {
                root_id: root,
                conversation_id: self.versions[&root].conversation_id,
                following: false,
                last_read: seq(self.live_replies(root).last().map_or(0, |m| m.seq.get())),
                unread_count: 0,
                mention_count: 0,
            };
        };
        let me = self.world.me.id;
        let deleted = self.deleted_ids();
        let unread: Vec<&Message> = self
            .log
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| {
                m.thread_root == Some(root)
                    && m.author_id != me
                    && m.seq.get() > rec.last_read
                    && m.seq.get() <= position
                    && !deleted.contains(&m.id)
            })
            .collect();
        let mentioning = unread
            .iter()
            .filter(|m| self.versions[&m.id].mentions.includes(me))
            .count();
        ThreadUserState {
            root_id: root,
            conversation_id: self.versions[&root].conversation_id,
            following: rec.following,
            last_read: seq(rec.last_read),
            unread_count: u32::try_from(unread.len()).unwrap(),
            mention_count: u32::try_from(mentioning).unwrap(),
        }
    }

    /// Records a change to the person's state for a thread, for every device to learn in order.
    fn push_thread_state(&mut self, root: MessageId) {
        let position = BASE_HEAD + 1 + self.log.len() as i64;
        let state = self.thread_state_at(root, position);
        self.log.push(LogEntry {
            event: LogEvent::ThreadState(state),
        });
    }

    /// Whether the person's thread commands apply: the root is a top-level message that exists,
    /// or one that was deleted but still has replies.
    fn thread_command_valid(&self, root: MessageId) -> bool {
        self.versions.get(&root).is_some_and(|r| {
            r.thread_root.is_none() && (!r.deleted || !self.live_replies(root).is_empty())
        })
    }

    /// Sets whether the person follows a thread. A person with no state who follows starts at the
    /// thread's newest reply (nothing unread); one who unfollows is remembered, silently. Only a
    /// change of the flag is announced.
    fn server_set_follow(&mut self, root: MessageId, following: bool) {
        let newest = self.live_replies(root).last().map_or(0, |m| m.seq.get());
        match self.thread_records.get(&root).copied() {
            None => {
                self.thread_records.insert(
                    root,
                    ThreadRec {
                        following,
                        unfollowed: !following,
                        last_read: newest,
                    },
                );
                if following {
                    self.push_thread_state(root);
                }
            }
            Some(rec) => {
                if rec.following != following || rec.unfollowed == following {
                    self.thread_records.insert(
                        root,
                        ThreadRec {
                            following,
                            unfollowed: !following,
                            last_read: rec.last_read,
                        },
                    );
                }
                if rec.following != following {
                    self.push_thread_state(root);
                }
            }
        }
    }

    /// Moves the person's read position in a thread forward, clamped to its newest reply that
    /// exists. A person with no state has nothing to move.
    fn server_reads_thread(&mut self, root: MessageId, up_to: i64) {
        let newest = self.live_replies(root).last().map_or(0, |m| m.seq.get());
        let Some(rec) = self.thread_records.get(&root).copied() else {
            return;
        };
        let target = up_to.min(newest);
        if target <= rec.last_read {
            return;
        }
        self.thread_records.insert(
            root,
            ThreadRec {
                last_read: target,
                ..rec
            },
        );
        self.push_thread_state(root);
    }

    fn deleted_ids(&self) -> HashSet<MessageId> {
        self.log
            .iter()
            .filter_map(LogEntry::deleted)
            .map(|m| m.id)
            .collect()
    }

    /// Records a message a person other than this client wrote.
    fn server_creates(&mut self, message: Message) {
        self.versions.insert(message.id, message.clone());
        self.log.push(LogEntry::message_entry(message, None));
    }

    /// An edit commits: the next revision with new text, and an event. Returns the new version.
    fn server_edit(
        &mut self,
        id: MessageId,
        document: MessageDocument,
        operation: Option<OperationId>,
        mentions: Option<Mentions>,
    ) -> Message {
        let mut message = self.versions[&id].clone();
        let previous = message.mentions.clone();
        message.document = document;
        message.revision += 1;
        message.edited_at = Some(Timestamp::now());
        if let Some(mentions) = mentions {
            message.mentions = mentions;
        }
        self.versions.insert(id, message.clone());
        self.log.push(LogEntry {
            event: LogEvent::Edited {
                message: message.clone(),
                operation,
                previous,
            },
        });
        message
    }

    /// A deletion commits (once): the tombstone, and an event. Returns the tombstone.
    fn server_delete(&mut self, id: MessageId) -> Message {
        if self.versions[&id].deleted {
            return self.versions[&id].clone();
        }
        let had = self.versions[&id].mentions.clone();
        let tombstone = self.world.tombstone(&self.versions[&id]);
        self.versions.insert(id, tombstone.clone());
        self.log.push(LogEntry {
            event: LogEvent::Deleted {
                message: tombstone.clone(),
                had,
            },
        });
        if tombstone.thread_root.is_none() && self.live_replies(id).is_empty() {
            // A root deleted with nothing under it has no thread left.
            if self.thread_records.remove(&id).is_some() {
                self.purged.insert(id);
            }
        }
        if let Some(root) = tombstone.thread_root {
            self.push_thread_changed(tombstone.conversation_id, root);
            // A deleted root with nothing left under it goes, and so does everyone's state.
            if self.versions[&root].deleted && self.live_replies(root).is_empty() {
                self.thread_records.remove(&root);
                self.purged.insert(root);
            }
        }
        tombstone
    }

    /// Someone sets whether they have reacted with `emoji`. Asking for what already holds commits
    /// nothing. A new emoji goes last, a new reactor goes last, and an emoji nobody uses is
    /// dropped. Returns the message as it now stands (the change is an event at its position).
    fn server_react(
        &mut self,
        id: MessageId,
        who: UserId,
        emoji: ReactionEmoji,
        present: bool,
    ) -> Message {
        let mut message = self.versions[&id].clone();
        if message.deleted {
            return message;
        }
        let at = message.reactions.iter().position(|r| r.emoji == emoji);
        let changed = match (at, present) {
            (Some(i), true) if !message.reactions[i].users.contains(&who) => {
                message.reactions[i].users.push(who);
                true
            }
            (None, true) => {
                message.reactions.push(ReactionSummary {
                    emoji,
                    users: vec![who],
                });
                true
            }
            (Some(i), false) if message.reactions[i].users.contains(&who) => {
                message.reactions[i].users.retain(|user| *user != who);
                if message.reactions[i].users.is_empty() {
                    message.reactions.remove(i);
                }
                true
            }
            _ => false,
        };
        if !changed {
            return message;
        }
        message.reactions_as_of = seq(BASE_HEAD + 1 + self.log.len() as i64);
        self.versions.insert(id, message.clone());
        self.log.push(LogEntry {
            event: LogEvent::Reactions {
                conversation: message.conversation_id,
                message: id,
                reactions: message.reactions.clone(),
            },
        });
        message
    }

    /// Whether `who` has reacted with `emoji`, on the server.
    fn server_has_reaction(&self, id: MessageId, who: UserId, emoji: ReactionEmoji) -> bool {
        self.versions[&id]
            .reactions
            .iter()
            .any(|r| r.emoji == emoji && r.users.contains(&who))
    }

    /// What a page, a bootstrap, or a fetch sees of every message once the log had `visible`
    /// entries: the latest version at that time. Deleted messages are tombstones.
    fn versions_as_of(&self, visible: usize) -> HashMap<MessageId, Message> {
        let mut seen: HashMap<MessageId, Message> = HashMap::new();
        for (index, entry) in self.log[..visible.min(self.log.len())].iter().enumerate() {
            match &entry.event {
                LogEvent::Reactions {
                    message, reactions, ..
                } => {
                    if let Some(known) = seen.get_mut(message)
                        && !known.deleted
                    {
                        known.reactions = reactions.clone();
                        known.reactions_as_of = seq(BASE_HEAD + 1 + index as i64);
                    }
                }
                LogEvent::Message { message, .. } => {
                    seen.insert(message.id, message.clone());
                }
                LogEvent::Edited { message, .. } => {
                    seen.insert(message.id, message.clone());
                }
                LogEvent::Deleted { message, .. } => {
                    seen.insert(message.id, message.clone());
                }
                LogEvent::ThreadChanged { root, summary, .. } => {
                    if let Some(known) = seen.get_mut(root) {
                        known.thread = summary.clone();
                    }
                }
                LogEvent::Membership { .. } => {}
                LogEvent::State(_)
                | LogEvent::ThreadState(_)
                | LogEvent::Emoji { .. }
                | LogEvent::Settings(_)
                | LogEvent::Prefs(_)
                | LogEvent::UserStatus { .. }
                | LogEvent::Saved { .. }
                | LogEvent::Workspace(_)
                | LogEvent::Role { .. }
                | LogEvent::Deactivated { .. } => {}
            }
        }
        seen
    }

    /// The workspace's custom emoji once the log had `visible` entries.
    fn emoji_as_of(&self, visible: usize) -> Vec<CustomEmoji> {
        let mut emoji = self.world.custom_emoji.clone();
        for entry in &self.log[..visible.min(self.log.len())] {
            if let LogEvent::Emoji { emoji: changed } = &entry.event {
                match emoji.iter_mut().find(|e| e.id == changed.id) {
                    Some(held) => *held = changed.clone(),
                    None => emoji.push(changed.clone()),
                }
            }
        }
        emoji
    }

    /// The emoji the server lets a message or a new reaction use now.
    fn live_emoji(&self) -> Vec<CustomEmoji> {
        let mut live: Vec<CustomEmoji> = self
            .emoji_as_of(self.log.len())
            .into_iter()
            .filter(|e| !e.deleted)
            .collect();
        live.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        live
    }

    /// An emoji changes on the server.
    fn server_changes_emoji(&mut self, emoji: CustomEmoji) {
        self.log.push(LogEntry {
            event: LogEvent::Emoji { emoji },
        });
    }

    /// The result a repeated edit operation returns: its original, or the tombstone if the
    /// message has been deleted since.
    fn edit_receipt(&self, operation: OperationId) -> Option<Message> {
        let receipt = self.edit_receipts.get(&operation)?;
        let current = &self.versions[&receipt.id];
        Some(if current.deleted {
            current.clone()
        } else {
            receipt.clone()
        })
    }

    /// The server's handling of an edit request: not found, then stale, then commit. Failures
    /// leave no receipt, and an edit that changes nothing commits nothing.
    fn server_applies_edit(
        &mut self,
        id: MessageId,
        request: &lazy_protocol::EditMessageRequest,
    ) -> Result<Message, RequestError> {
        if let Some(receipt) = self.edit_receipt(request.operation_id) {
            return Ok(receipt);
        }
        let refuse = |code, text: &str| Err(RequestError::Rejected(ApiError::new(code, text)));
        let Some(current) = self.versions.get(&id).cloned() else {
            return refuse(ErrorCode::NotFound, "no such message");
        };
        if current.deleted {
            return refuse(ErrorCode::NotFound, "deleted");
        }
        if current.revision != request.base_revision {
            EDITS_REFUSED_AS_STALE.fetch_add(1, Ordering::Relaxed);
            return refuse(ErrorCode::StaleRevision, "stale");
        }
        let committed = if current.document == request.document {
            current
        } else {
            EDITS_COMMITTED.fetch_add(1, Ordering::Relaxed);
            self.server_edit(
                id,
                request.document.clone(),
                Some(request.operation_id),
                None,
            )
        };
        self.edit_receipts
            .insert(request.operation_id, committed.clone());
        Ok(committed)
    }

    fn page(
        &self,
        conversation: ConversationId,
        before: Option<EventSeq>,
        visible: usize,
    ) -> HistoryPage {
        let as_of = self.versions_as_of(visible);
        // History leaves deleted messages out.
        let candidates: Vec<&Message> = self.log[..visible.min(self.log.len())]
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| m.conversation_id == conversation && is_listed(m))
            .filter(|m| before.is_none_or(|b| m.seq < b))
            .map(|m| &as_of[&m.id])
            // A deleted root that still has replies stays as a placeholder.
            .filter(|m| !m.deleted || (m.thread_root.is_none() && m.thread.is_some()))
            .collect();
        let start = candidates.len().saturating_sub(SERVER_PAGE);
        HistoryPage {
            messages: candidates[start..].iter().map(|m| (*m).clone()).collect(),
            has_more: start > 0,
        }
    }

    /// A page of a thread's replies, newest page first like history.
    fn replies_page(
        &self,
        root: MessageId,
        before: Option<EventSeq>,
        visible: usize,
    ) -> HistoryPage {
        let as_of = self.versions_as_of(visible);
        let candidates: Vec<&Message> = self.log[..visible.min(self.log.len())]
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| m.thread_root == Some(root))
            .filter(|m| before.is_none_or(|b| m.seq < b))
            .map(|m| &as_of[&m.id])
            .filter(|m| !m.deleted)
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
        // A deactivated person is refused everything the server does for the workspace.
        let outcome = if self.me_deactivated
            && !matches!(
                effect,
                Effect::LoadBootstrap | Effect::PersistSubmission(_) | Effect::StageFile { .. }
            ) {
            Outcome::Reject
        } else {
            outcome
        };
        match effect {
            Effect::PersistSubmission(entry) => {
                let operation = entry.operation_id;
                let input = match outcome {
                    Outcome::Reject => Input::SubmissionPersistFailed {
                        operation,
                        message: "disk".into(),
                    },
                    _ => {
                        // The journal applies its commands in order: a settle that was asked
                        // for after this write leaves nothing of it behind.
                        if !self.settle_counts.contains_key(&operation) {
                            self.journal.insert(operation, entry);
                        }
                        Input::SubmissionPersisted { operation }
                    }
                };
                self.feed(input);
            }
            Effect::SendMessage {
                operation,
                conversation,
                document,
                thread_root,
                also_in_channel,
                attachments,
            } => {
                let thread = thread_root.map(|root| (root, also_in_channel));
                if !matches!(outcome, Outcome::Ok) {
                    self.disturbed.insert(operation);
                }
                let result = match outcome {
                    Outcome::Ok => self
                        .commit(operation, conversation, &document, thread, &attachments)
                        .map(|message| SendMessageResponse { message }),
                    Outcome::LostAnswer => {
                        let _ =
                            self.commit(operation, conversation, &document, thread, &attachments);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => match self.committed.get(&operation) {
                        // Idempotent: a committed operation always returns its result.
                        Some(message) => Ok(SendMessageResponse {
                            message: self.versions[&message.id].clone(),
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
                if !matches!(outcome, Outcome::Ok) {
                    self.disturbed.insert(operation);
                }
                let result = match outcome {
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                    _ => Ok(match self.committed.get(&operation) {
                        Some(message) => OperationStatus::Committed {
                            message: self.versions[&message.id].clone(),
                        },
                        None => match self.edit_receipt(operation) {
                            Some(message) => OperationStatus::EditCommitted { message },
                            None => OperationStatus::NotFound,
                        },
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
            Effect::EditMessage { message, request } => {
                let operation = request.operation_id;
                let result = match outcome {
                    Outcome::Ok => self.server_applies_edit(message, &request),
                    Outcome::LostAnswer => {
                        let _ = self.server_applies_edit(message, &request);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => match self.edit_receipt(operation) {
                        Some(receipt) => Ok(receipt),
                        None => Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Forbidden,
                            "no",
                        ))),
                    },
                };
                self.feed(Input::EditResult {
                    operation,
                    result: result.map(|message| MessageResponse { message }),
                });
            }
            Effect::SetReaction { message, request } => {
                let me = self.world.me.id;
                let emoji = request.emoji;
                // A reaction with a custom emoji that is gone can be withdrawn, never added.
                let gone = request.present
                    && emoji
                        .custom_id()
                        .is_some_and(|id| !self.live_emoji().iter().any(|e| e.id == id));
                let apply = |sim: &mut Self| {
                    let before = sim.server_has_reaction(message, me, emoji);
                    if gone {
                        return sim.versions[&message].clone();
                    }
                    // A new kind beyond the limit changes nothing.
                    if request.present && sim.would_exceed_kinds(message, emoji) {
                        return sim.versions[&message].clone();
                    }
                    let after = sim.server_react(message, me, emoji, request.present);
                    if before != request.present {
                        REACTIONS_SET_FOR_THE_PERSON.fetch_add(1, Ordering::Relaxed);
                        if emoji.custom_id().is_some() {
                            CUSTOM_REACTIONS_SET.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    after
                };
                let over_the_limit = request.present && self.would_exceed_kinds(message, emoji);
                if over_the_limit && matches!(outcome, Outcome::Ok) {
                    REACTIONS_REFUSED_AT_THE_LIMIT.fetch_add(1, Ordering::Relaxed);
                }
                let result = match outcome {
                    Outcome::Ok if gone => {
                        EMOJI_USES_REFUSED.fetch_add(1, Ordering::Relaxed);
                        Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::InvalidRequest,
                            "that emoji is gone",
                        )))
                    }
                    Outcome::Ok if over_the_limit => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::LimitReached,
                        "too many kinds",
                    ))),
                    Outcome::Ok => {
                        let current = apply(self);
                        if current.deleted {
                            Err(RequestError::Rejected(ApiError::new(
                                ErrorCode::NotFound,
                                "deleted",
                            )))
                        } else {
                            Ok(current)
                        }
                    }
                    Outcome::LostAnswer => {
                        REACTION_REQUESTS_LOST.fetch_add(1, Ordering::Relaxed);
                        apply(self);
                        Err(unknown())
                    }
                    Outcome::LostRequest => {
                        REACTION_REQUESTS_LOST.fetch_add(1, Ordering::Relaxed);
                        Err(unknown())
                    }
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::Forbidden,
                        "no",
                    ))),
                };
                self.feed(Input::ReactionResult {
                    message,
                    emoji,
                    result: result.map(|message| MessageResponse { message }),
                });
            }
            Effect::FetchMessage { message } => {
                let result = match outcome {
                    Outcome::Ok => Ok(MessageResponse {
                        message: self.versions_as_of(seen)[&message].clone(),
                    }),
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::NotFound,
                        "no",
                    ))),
                };
                self.feed(Input::MessageFetched { message, result });
            }
            Effect::DeleteMessage { message } => {
                let result = match outcome {
                    Outcome::Ok => Ok(self.server_delete(message)),
                    Outcome::LostAnswer => {
                        self.server_delete(message);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::Forbidden,
                        "no",
                    ))),
                };
                self.feed(Input::DeleteResult {
                    message,
                    result: result.map(|message| MessageResponse { message }),
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
            Effect::FetchMembers {
                conversation,
                generation,
            } => {
                let result = match outcome {
                    Outcome::Ok | Outcome::Reject => {
                        MEMBER_LISTS_FETCHED.fetch_add(1, Ordering::Relaxed);
                        Ok(ConversationMembersResponse {
                            conversation_id: conversation,
                            user_ids: self.server_members(conversation, seen),
                        })
                    }
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                };
                self.feed(Input::MembersFetched {
                    conversation,
                    generation,
                    result,
                });
            }
            Effect::FetchReplies {
                root,
                before,
                generation,
                ..
            } => {
                let result = match outcome {
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                    _ => Ok(self.replies_page(root, before, seen)),
                };
                self.feed(Input::RepliesLoaded {
                    root,
                    generation,
                    result,
                });
            }
            Effect::SetThreadFollow { root, following } => {
                FOLLOW_REQUESTS_ANSWERED.fetch_add(1, Ordering::Relaxed);
                let now = |sim: &Self| {
                    Ok(ThreadStateResponse {
                        state: sim.thread_state_at(root, BASE_HEAD + sim.log.len() as i64),
                    })
                };
                let valid = self.thread_command_valid(root);
                let gone = || {
                    Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::NotFound,
                        "no such thread",
                    )))
                };
                let result = match outcome {
                    Outcome::Ok if !valid => gone(),
                    Outcome::Ok => {
                        self.server_set_follow(root, following);
                        now(self)
                    }
                    Outcome::LostAnswer => {
                        if valid {
                            self.server_set_follow(root, following);
                        }
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::Forbidden,
                        "no",
                    ))),
                };
                self.feed(Input::ThreadFollowResult { root, result });
            }
            Effect::MarkThreadRead { root, up_to } => {
                THREAD_READS_ANSWERED.fetch_add(1, Ordering::Relaxed);
                let valid = self.thread_command_valid(root);
                let result = match outcome {
                    Outcome::Ok if !valid => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::NotFound,
                        "no such thread",
                    ))),
                    Outcome::Ok => {
                        self.server_reads_thread(root, up_to.get());
                        Ok(ThreadStateResponse {
                            state: self.thread_state_at(root, BASE_HEAD + self.log.len() as i64),
                        })
                    }
                    Outcome::LostAnswer => {
                        if valid {
                            self.server_reads_thread(root, up_to.get());
                        }
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::Forbidden,
                        "no",
                    ))),
                };
                self.feed(Input::ThreadReadResult {
                    root,
                    up_to,
                    result,
                });
            }
            Effect::StageFile {
                attachment,
                generation,
                source,
            } => {
                let released_since = self.release_epoch.get(&attachment).copied().unwrap_or(0)
                    != self
                        .stage_epoch
                        .get(&(attachment, generation))
                        .copied()
                        .unwrap_or(0);
                let size = match &source {
                    FileSource::Bytes(bytes) => bytes.len() as u64,
                    FileSource::Path(_) => 1 + (attachment.as_uuid().as_u128() % 4000) as u64,
                };
                let result = match outcome {
                    Outcome::Reject => Err(TransferError::Unreadable("denied".into())),
                    _ => {
                        // A copy finished after its release is deleted by it.
                        if !released_since {
                            self.outbox.insert(attachment);
                        }
                        Ok(StagedFile { size })
                    }
                };
                // Even an answer nobody wants any more is delivered: the core ignores it.
                self.feed(Input::FileStaged {
                    attachment,
                    generation,
                    result,
                });
            }
            Effect::UploadFile {
                attachment,
                conversation,
                generation,
                name,
                media_type,
                size,
            } => {
                let cancelled = self.upload_cancelled.remove(&(attachment, generation));
                if self.uploads_running.get(&attachment) == Some(&generation) {
                    self.uploads_running.remove(&attachment);
                }
                // A transfer that was stopped usually says nothing more; sometimes its answer
                // was already on its way.
                if cancelled && generation % 2 == 0 {
                    return;
                }
                if cancelled {
                    STALE_UPLOAD_RESULTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                }
                let stored = |sim: &mut Self| {
                    let file = sim
                        .server_uploads
                        .entry(attachment)
                        .or_insert_with(|| {
                            FILES_UPLOADED.fetch_add(1, Ordering::Relaxed);
                            Attachment {
                                id: attachment,
                                name: name.clone(),
                                media_type: media_type.clone(),
                                size,
                                sha256: "ab".repeat(32),
                                width: None,
                                height: None,
                                has_thumbnail: media_type.starts_with("image/"),
                            }
                        })
                        .clone();
                    let _ = conversation;
                    AttachmentResponse { attachment: file }
                };
                let result = if !self.outbox.contains(&attachment) {
                    Err(TransferError::Missing)
                } else {
                    match outcome {
                        Outcome::Ok => Ok(stored(self)),
                        Outcome::LostAnswer => {
                            stored(self);
                            Err(TransferError::Request(unknown()))
                        }
                        Outcome::LostRequest => Err(TransferError::Request(unknown())),
                        Outcome::Reject => {
                            let code = if generation % 2 == 0 {
                                ErrorCode::UnsupportedMedia
                            } else {
                                ErrorCode::LimitReached
                            };
                            Err(TransferError::Request(RequestError::Rejected(
                                ApiError::new(code, "no"),
                            )))
                        }
                    }
                };
                self.feed(Input::UploadResult {
                    attachment,
                    generation,
                    result,
                });
            }
            Effect::DownloadAttachment {
                attachment,
                destination,
            } => {
                self.downloads_running.remove(&attachment.id);
                let result = match outcome {
                    Outcome::Ok => {
                        self.feed(Input::DownloadProgress {
                            attachment: attachment.id,
                            received: attachment.size / 2,
                            total: attachment.size,
                        });
                        Ok(destination)
                    }
                    Outcome::Reject => Err(TransferError::Request(RequestError::Rejected(
                        ApiError::new(ErrorCode::NotFound, "gone"),
                    ))),
                    Outcome::LostRequest => Err(TransferError::Disk("no space".into())),
                    Outcome::LostAnswer => Err(TransferError::ChecksumMismatch),
                };
                self.feed(Input::DownloadResult {
                    attachment: attachment.id,
                    result,
                });
            }
            Effect::FetchEmojiImage { emoji } => {
                self.emoji_in_flight.remove(&emoji);
                let result = match outcome {
                    Outcome::Ok => {
                        EMOJI_IMAGES_LOADED.fetch_add(1, Ordering::Relaxed);
                        Ok(ImageData::new(vec![2; 10]))
                    }
                    Outcome::Reject => Err(TransferError::Request(RequestError::Rejected(
                        ApiError::new(ErrorCode::NotFound, "gone"),
                    ))),
                    Outcome::LostRequest | Outcome::LostAnswer => {
                        Err(TransferError::Request(unknown()))
                    }
                };
                self.feed(Input::EmojiImageLoaded { emoji, result });
            }
            Effect::FetchImage { attachment, full } => {
                self.images_in_flight.remove(&(attachment, full));
                let result = match outcome {
                    Outcome::Ok => Ok(ImageData::new(vec![1; 10])),
                    Outcome::Reject => Err(TransferError::Request(RequestError::Rejected(
                        ApiError::new(ErrorCode::NotFound, "gone"),
                    ))),
                    Outcome::LostRequest | Outcome::LostAnswer => {
                        Err(TransferError::Request(unknown()))
                    }
                };
                self.feed(Input::ImageLoaded {
                    attachment,
                    full,
                    result,
                });
            }
            Effect::Search {
                generation, params, ..
            } => {
                let waiting = self.driver.core.search().map(|view| view.generation);
                let before = self.driver.core.search().cloned();
                let result = match outcome {
                    Outcome::Ok => Ok(self.search_page(&params, seen)),
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::Unavailable,
                        "that search took too long",
                    ))),
                };
                let answered = result.is_ok();
                let first_page = params.offset == Some(0);
                self.feed(Input::SearchResult { generation, result });
                let view = self.driver.core.search().cloned();
                if waiting != Some(generation) {
                    // An answer to a request the core has moved on from, or to a closed
                    // search, changes nothing.
                    assert_eq!(view, before, "a late search answer changed the view");
                    STALE_SEARCH_ANSWERS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                } else {
                    let view = view.expect("the search is open");
                    if answered {
                        assert_eq!(view.status, SearchStatus::Ready);
                        let counter = if first_page {
                            &SEARCH_ANSWERS_SHOWN
                        } else {
                            &SEARCH_PAGES_APPENDED
                        };
                        counter.fetch_add(1, Ordering::Relaxed);
                    } else {
                        assert!(
                            matches!(
                                view.status,
                                SearchStatus::Failed {
                                    can_retry: true,
                                    ..
                                }
                            ),
                            "a search that could not be answered can be asked again: {:?}",
                            view.status
                        );
                        SEARCH_FAILURES_SHOWN.fetch_add(1, Ordering::Relaxed);
                        if first_page {
                            assert!(view.hits.is_empty(), "a failed first page shows no hits");
                        }
                    }
                }
            }
            Effect::SetNotificationSettings { settings, .. } => {
                let result = match outcome {
                    Outcome::Ok => {
                        self.server_sets_settings(settings);
                        Ok(NotificationSettingsResponse { settings })
                    }
                    Outcome::LostAnswer => {
                        self.server_sets_settings(settings);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::InvalidRequest,
                        "no",
                    ))),
                };
                SETTINGS_SAVES_ANSWERED.fetch_add(1, Ordering::Relaxed);
                let workspace = self.world.workspace.id;
                self.feed(Input::NotificationSettingsResult { workspace, result });
            }
            Effect::SetConversationNotificationPrefs {
                conversation,
                request,
            } => {
                let prefs = ConversationNotificationPrefs {
                    conversation_id: conversation,
                    level: request.level,
                    muted: request.muted,
                };
                let result = match outcome {
                    Outcome::Ok => {
                        self.server_sets_prefs(prefs);
                        Ok(ConversationNotificationPrefsResponse { prefs })
                    }
                    Outcome::LostAnswer => {
                        self.server_sets_prefs(prefs);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::Forbidden,
                        "no",
                    ))),
                };
                SETTINGS_SAVES_ANSWERED.fetch_add(1, Ordering::Relaxed);
                self.feed(Input::ConversationPrefsResult {
                    conversation,
                    result,
                });
            }
            Effect::SetStatus { request, .. } => {
                let me = self.world.me.id;
                let result = match outcome {
                    Outcome::Ok => {
                        self.server_sets_status(me, request.status.clone());
                        let mut user = self.world.me.clone();
                        user.status = request.status;
                        Ok(StatusResponse { user })
                    }
                    Outcome::LostAnswer => {
                        self.server_sets_status(me, request.status);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::InvalidRequest,
                        "no",
                    ))),
                };
                SETTINGS_SAVES_ANSWERED.fetch_add(1, Ordering::Relaxed);
                let workspace = self.world.workspace.id;
                self.feed(Input::StatusResult { workspace, result });
            }
            Effect::UpdateWorkspace { request, .. } => {
                let allowed = self.my_role.can_manage_workspace();
                let apply = |sim: &mut Self| {
                    let mut workspace = sim.server_workspace.clone();
                    if let Some(name) = &request.name {
                        workspace.name = name.as_str().to_owned();
                    }
                    if let Some(settings) = request.settings {
                        workspace.settings = settings;
                    }
                    sim.server_sets_workspace(workspace);
                    sim.server_workspace.clone()
                };
                let result = match outcome {
                    Outcome::Ok if allowed => {
                        ADMIN_CHANGES_ANSWERED.fetch_add(1, Ordering::Relaxed);
                        Ok(ActionOutcome::Workspace(apply(self)))
                    }
                    Outcome::LostAnswer if allowed => {
                        apply(self);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    _ => {
                        ADMIN_REFUSALS.fetch_add(1, Ordering::Relaxed);
                        Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Forbidden,
                            "only admins and owners change the workspace",
                        )))
                    }
                };
                self.feed(Input::ActionResult {
                    kind: ActionKind::UpdateWorkspace,
                    result,
                });
            }
            Effect::SetMemberRole { user, request, .. } => {
                let (me, other) = (self.world.me.id, self.world.other.id);
                let target_role = if user == me {
                    self.my_role
                } else {
                    self.other_role
                };
                let owners_besides = [(me, self.my_role), (other, self.other_role)]
                    .into_iter()
                    .filter(|(id, role)| *id != user && *role == WorkspaceRole::Owner)
                    .count();
                let refusal = match check_role_change(
                    self.my_role,
                    target_role,
                    request.role,
                    owners_besides,
                ) {
                    Ok(()) => None,
                    Err(RoleChangeError::NotAllowed) => Some(ErrorCode::Forbidden),
                    Err(RoleChangeError::LastOwner) => Some(ErrorCode::Conflict),
                };
                let result = match outcome {
                    Outcome::Ok if refusal.is_none() => {
                        ADMIN_CHANGES_ANSWERED.fetch_add(1, Ordering::Relaxed);
                        self.server_sets_role(user, request.role);
                        Ok(ActionOutcome::Role {
                            user,
                            role: request.role,
                        })
                    }
                    Outcome::LostAnswer if refusal.is_none() => {
                        self.server_sets_role(user, request.role);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    _ => {
                        ADMIN_REFUSALS.fetch_add(1, Ordering::Relaxed);
                        if refusal == Some(ErrorCode::Conflict) {
                            LAST_OWNER_REFUSALS.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(RequestError::Rejected(ApiError::new(
                            refusal.unwrap_or(ErrorCode::Forbidden),
                            "not allowed",
                        )))
                    }
                };
                self.feed(Input::ActionResult {
                    kind: ActionKind::SetMemberRole(user),
                    result,
                });
            }
            Effect::SetMemberDeactivated { user, request, .. } => {
                assert_ne!(
                    user, self.world.me.id,
                    "the core asked to deactivate the person"
                );
                let allowed = check_member_action(self.my_role, self.other_role, false).is_ok();
                let result = match outcome {
                    Outcome::Ok if allowed => {
                        ADMIN_CHANGES_ANSWERED.fetch_add(1, Ordering::Relaxed);
                        self.server_sets_deactivated(user, request.deactivated);
                        Ok(ActionOutcome::Deactivated {
                            user,
                            deactivated: request.deactivated,
                        })
                    }
                    Outcome::LostAnswer if allowed => {
                        self.server_sets_deactivated(user, request.deactivated);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    _ => {
                        ADMIN_REFUSALS.fetch_add(1, Ordering::Relaxed);
                        Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Forbidden,
                            "not allowed",
                        )))
                    }
                };
                self.feed(Input::ActionResult {
                    kind: ActionKind::SetMemberDeactivated(user),
                    result,
                });
            }
            Effect::LoadInvitations { generation, .. } => {
                let current = generation == self.invitations_newest;
                let before = self.driver.core.invitations().clone();
                let result = match outcome {
                    Outcome::Ok if self.my_role.can_manage_workspace() => Ok(InvitationList {
                        invitations: self.invitations.clone(),
                    }),
                    Outcome::Ok => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::Forbidden,
                        "admins only",
                    ))),
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::RateLimited,
                        "slow down",
                    ))),
                };
                let answered = result.is_ok();
                self.feed(Input::InvitationsLoaded { generation, result });
                if !current {
                    assert_eq!(
                        *self.driver.core.invitations(),
                        before,
                        "a late invitations answer changed the view"
                    );
                    STALE_ADMIN_ANSWERS.fetch_add(1, Ordering::Relaxed);
                } else if answered && self.driver.core.invitations().open {
                    INVITATION_LISTS_SHOWN.fetch_add(1, Ordering::Relaxed);
                }
            }
            Effect::CreateInvitation { request, .. } => {
                let active = self
                    .invitations
                    .iter()
                    .filter(|i| i.state == InvitationState::Active)
                    .count();
                let refusal = if !self.my_role.can_manage_workspace() {
                    Some(ErrorCode::Forbidden)
                } else if active >= ACTIVE_INVITATION_CAP {
                    Some(ErrorCode::LimitReached)
                } else {
                    None
                };
                let result = match outcome {
                    Outcome::Ok if refusal.is_none() => {
                        ADMIN_CHANGES_ANSWERED.fetch_add(1, Ordering::Relaxed);
                        Ok(ActionOutcome::InvitationCreated(
                            self.server_creates_invitation(&request),
                        ))
                    }
                    Outcome::LostAnswer if refusal.is_none() => {
                        // Made, and the code is lost for good.
                        let _ = self.server_creates_invitation(&request);
                        INVITATION_CREATES_LOST.fetch_add(1, Ordering::Relaxed);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    _ => {
                        ADMIN_REFUSALS.fetch_add(1, Ordering::Relaxed);
                        Err(RequestError::Rejected(ApiError::new(
                            refusal.unwrap_or(ErrorCode::LimitReached),
                            "no",
                        )))
                    }
                };
                self.feed(Input::ActionResult {
                    kind: ActionKind::CreateInvitation,
                    result,
                });
            }
            Effect::RevokeInvitation { invitation } => {
                let allowed = self.my_role.can_manage_workspace();
                let known = self.invitations.iter().position(|i| i.id == invitation);
                let revoke = |sim: &mut Self| {
                    known.map(|index| {
                        sim.invitations[index].state = InvitationState::Revoked;
                        sim.invitations[index].clone()
                    })
                };
                let result = match outcome {
                    Outcome::Ok if allowed => match revoke(self) {
                        Some(revoked) => {
                            ADMIN_CHANGES_ANSWERED.fetch_add(1, Ordering::Relaxed);
                            Ok(ActionOutcome::Invitation(revoked))
                        }
                        None => Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::NotFound,
                            "no such invitation",
                        ))),
                    },
                    Outcome::LostAnswer if allowed => {
                        revoke(self);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    _ => {
                        ADMIN_REFUSALS.fetch_add(1, Ordering::Relaxed);
                        Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Forbidden,
                            "admins only",
                        )))
                    }
                };
                self.feed(Input::ActionResult {
                    kind: ActionKind::RevokeInvitation(invitation),
                    result,
                });
            }
            Effect::LoadInbox {
                generation, query, ..
            } => {
                let waiting = self.driver.core.inbox().map(|view| view.generation);
                let before = self.driver.core.inbox().cloned();
                let result = match outcome {
                    Outcome::Ok => Ok(self.inbox_page(&query)),
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::RateLimited,
                        "slow down",
                    ))),
                };
                let answered = result.is_ok();
                self.feed(Input::InboxLoaded { generation, result });
                if waiting != Some(generation) {
                    assert_eq!(
                        self.driver.core.inbox().cloned(),
                        before,
                        "a late inbox answer changed the view"
                    );
                    STALE_INBOX_ANSWERS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                } else if answered {
                    INBOX_PAGES_SHOWN.fetch_add(1, Ordering::Relaxed);
                } else {
                    let view = self.driver.core.inbox().expect("the inbox is open");
                    assert!(
                        matches!(
                            view.status,
                            InboxStatus::Failed {
                                can_retry: true,
                                ..
                            }
                        ),
                        "{:?}",
                        view.status
                    );
                }
            }
            Effect::MarkInboxRead { .. } => {
                let roots: Vec<MessageId> = self
                    .thread_records
                    .iter()
                    .filter(|(_, rec)| rec.following)
                    .map(|(root, _)| *root)
                    .collect();
                let result = match outcome {
                    Outcome::Ok | Outcome::LostAnswer => {
                        let mut updated = 0u32;
                        for root in roots {
                            let before = self.thread_records[&root].last_read;
                            self.server_reads_thread(root, i64::MAX);
                            updated += u32::from(self.thread_records[&root].last_read != before);
                        }
                        if matches!(outcome, Outcome::Ok) {
                            Ok(InboxReadResponse { updated })
                        } else {
                            Err(unknown())
                        }
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::Forbidden,
                        "no",
                    ))),
                };
                INBOX_MARKED_READ.fetch_add(1, Ordering::Relaxed);
                self.feed(Input::InboxReadResult { result });
            }
            Effect::SetMessageSaved { message, request } => {
                let gone = self.versions.get(&message).is_none_or(|m| m.deleted);
                let result = match outcome {
                    // A deleted message cannot be saved, but can be removed.
                    Outcome::Ok | Outcome::LostAnswer if request.saved && gone => {
                        if matches!(outcome, Outcome::Ok) {
                            Err(RequestError::Rejected(ApiError::new(
                                ErrorCode::NotFound,
                                "gone",
                            )))
                        } else {
                            Err(unknown())
                        }
                    }
                    Outcome::Ok => {
                        self.server_sets_saved(message, request.saved);
                        Ok(SavedResponse {
                            message_id: message,
                            saved: request.saved,
                        })
                    }
                    Outcome::LostAnswer => {
                        self.server_sets_saved(message, request.saved);
                        Err(unknown())
                    }
                    Outcome::LostRequest => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::NotFound,
                        "no",
                    ))),
                };
                SAVE_REQUESTS_ANSWERED.fetch_add(1, Ordering::Relaxed);
                self.feed(Input::MessageSavedResult {
                    message_id: message,
                    result,
                });
            }
            Effect::LoadSaved {
                generation, query, ..
            } => {
                let current = generation == self.saved_newest;
                let before = self.driver.core.saved().clone();
                let result = match outcome {
                    Outcome::Ok => Ok(self.saved_page(&query)),
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::RateLimited,
                        "slow down",
                    ))),
                };
                let answered = result.is_ok();
                self.feed(Input::SavedLoaded { generation, result });
                if !current {
                    assert_eq!(
                        *self.driver.core.saved(),
                        before,
                        "a late saved answer changed the list"
                    );
                    STALE_DISCOVERY_ANSWERS.fetch_add(1, Ordering::Relaxed);
                } else if answered && self.driver.core.saved().open {
                    SAVED_PAGES_SHOWN.fetch_add(1, Ordering::Relaxed);
                }
            }
            Effect::LoadPeople {
                generation, query, ..
            } => {
                let current = generation == self.people_newest;
                let before = self.driver.core.people().clone();
                let result = match outcome {
                    Outcome::Ok
                        if query.include_deactivated == Some(true)
                            && !self.my_role.can_manage_workspace() =>
                    {
                        Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Forbidden,
                            "admins only",
                        )))
                    }
                    Outcome::Ok => Ok(self.people_page(&query)),
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::RateLimited,
                        "slow down",
                    ))),
                };
                let answered = result.is_ok();
                self.feed(Input::PeopleLoaded { generation, result });
                if !current {
                    assert_eq!(
                        *self.driver.core.people(),
                        before,
                        "a late people answer changed the directory"
                    );
                    STALE_DISCOVERY_ANSWERS.fetch_add(1, Ordering::Relaxed);
                } else if answered && self.driver.core.people().open {
                    PEOPLE_PAGES_SHOWN.fetch_add(1, Ordering::Relaxed);
                }
            }
            Effect::LoadProfile {
                generation, user, ..
            } => {
                let current = generation == self.profile_newest;
                let before = self.driver.core.profile().cloned();
                let result = match (outcome, self.person_model(user)) {
                    (Outcome::Ok, Some(person)) => Ok(PersonResponse { person }),
                    (Outcome::Ok | Outcome::Reject, None) | (Outcome::Reject, Some(_)) => {
                        Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::NotFound,
                            "no such person",
                        )))
                    }
                    (Outcome::LostRequest | Outcome::LostAnswer, _) => Err(unknown()),
                };
                let answered = result.is_ok();
                self.feed(Input::ProfileLoaded { generation, result });
                if !current {
                    assert_eq!(
                        self.driver.core.profile().cloned(),
                        before,
                        "a late profile changed the profile"
                    );
                    STALE_DISCOVERY_ANSWERS.fetch_add(1, Ordering::Relaxed);
                } else if answered && self.driver.core.profile().is_some() {
                    PROFILES_SHOWN.fetch_add(1, Ordering::Relaxed);
                }
            }
            Effect::LoadBrowse {
                generation, query, ..
            } => {
                let current = generation == self.browse_newest;
                let before = self.driver.core.browse().clone();
                let result = match outcome {
                    Outcome::Ok => Ok(self.browse_page(&query)),
                    Outcome::LostRequest | Outcome::LostAnswer => Err(unknown()),
                    Outcome::Reject => Err(RequestError::Rejected(ApiError::new(
                        ErrorCode::RateLimited,
                        "slow down",
                    ))),
                };
                let answered = result.is_ok();
                self.feed(Input::BrowseLoaded { generation, result });
                if !current {
                    assert_eq!(
                        *self.driver.core.browse(),
                        before,
                        "a late directory answer changed the directory"
                    );
                    STALE_DISCOVERY_ANSWERS.fetch_add(1, Ordering::Relaxed);
                } else if answered && self.driver.core.browse().status == LoadStatus::Ready {
                    BROWSE_PAGES_SHOWN.fetch_add(1, Ordering::Relaxed);
                }
            }
            other => panic!("unexpected pending effect {other:?}"),
        }
    }

    // ----- The server's side of discovery -----

    /// The person saves or unsaves a message (nothing happens when it already holds).
    fn server_sets_saved(&mut self, message: MessageId, saved: bool) {
        if saved && !self.server_saved.contains_key(&message) {
            self.saved_counter += 1;
            self.server_saved.insert(message, self.saved_counter);
            self.log.push(LogEntry {
                event: LogEvent::Saved {
                    message,
                    saved: true,
                },
            });
        } else if !saved && self.server_saved.remove(&message).is_some() {
            self.log.push(LogEntry {
                event: LogEvent::Saved {
                    message,
                    saved: false,
                },
            });
        }
    }

    fn conversation_of(&self, id: ConversationId) -> lazy_protocol::Conversation {
        if id == self.world.channel.id {
            self.world.channel.clone()
        } else {
            self.world.second_channel.clone()
        }
    }

    /// The saved messages that still exist, newest saved first.
    fn saved_items(&self) -> Vec<SavedItem> {
        let mut items: Vec<SavedItem> = self
            .server_saved
            .iter()
            .filter_map(|(id, cursor)| {
                let message = self.versions.get(id).filter(|m| !m.deleted)?;
                Some(SavedItem {
                    message: message.clone(),
                    conversation: self.conversation_of(message.conversation_id),
                    saved_at: stamp(i64::try_from(*cursor).unwrap()),
                    cursor: *cursor,
                })
            })
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.cursor));
        items
    }

    fn saved_page(&self, query: &SavedQuery) -> SavedPage {
        let mut items = self.saved_items();
        if let Some(before) = query.before {
            items.retain(|item| item.cursor < before);
        }
        let limit = (query.limit.unwrap_or(20) as usize).min(DISCOVERY_SERVER_PAGE);
        let has_more = items.len() > limit;
        items.truncate(limit);
        SavedPage { items, has_more }
    }

    fn person_model(&self, user: UserId) -> Option<Person> {
        let (user, status, role, deactivated) = if user == self.world.me.id {
            (
                self.world.me.clone(),
                &self.server_status,
                self.my_role,
                false,
            )
        } else if user == self.world.other.id {
            (
                self.world.other.clone(),
                &self.other_status,
                self.other_role,
                self.other_deactivated,
            )
        } else {
            return None;
        };
        let mut user = user;
        user.status = status.clone().filter(|s| s.is_active(self.sim_now()));
        user.deactivated = deactivated;
        Some(Person {
            user,
            role,
            joined_at: stamp(0),
        })
    }

    /// Everyone in the workspace who matches the text, by name.
    fn people_model(&self, text: &str, include_deactivated: bool) -> Vec<Person> {
        let text = text.trim().to_lowercase();
        let mut people: Vec<Person> = [self.world.me.id, self.world.other.id]
            .into_iter()
            .filter_map(|id| self.person_model(id))
            .filter(|person| include_deactivated || !person.user.deactivated)
            .filter(|person| {
                text.is_empty()
                    || person
                        .user
                        .display_name
                        .as_str()
                        .to_lowercase()
                        .contains(&text)
                    || person.user.username.as_str().to_lowercase().contains(&text)
            })
            .collect();
        people.sort_by(|a, b| {
            (
                a.user.display_name.as_str().to_lowercase(),
                a.user.username.as_str().to_owned(),
                a.user.id,
            )
                .cmp(&(
                    b.user.display_name.as_str().to_lowercase(),
                    b.user.username.as_str().to_owned(),
                    b.user.id,
                ))
        });
        people
    }

    fn people_page(&self, query: &PeopleQuery) -> PeoplePage {
        let all = self.people_model(
            query.query.as_deref().unwrap_or(""),
            query.include_deactivated == Some(true),
        );
        let offset = query.offset.unwrap_or(0) as usize;
        let limit = (query.limit.unwrap_or(50) as usize).min(DISCOVERY_SERVER_PAGE);
        let rest: Vec<Person> = all.into_iter().skip(offset).collect();
        PeoplePage {
            has_more: rest.len() > limit,
            people: rest.into_iter().take(limit).collect(),
        }
    }

    /// The public channels that match, in the order asked for.
    fn browse_model(&self, query: &BrowseChannelsQuery) -> Vec<BrowsedChannel> {
        let text = query.query.as_deref().unwrap_or("").trim().to_lowercase();
        let mut found: Vec<(&lazy_protocol::Conversation, u32)> = self
            .directory
            .iter()
            .filter(|(conversation, _)| {
                let joined = conversation.id == self.world.channel.id
                    || conversation.id == self.world.second_channel.id;
                query.joined.is_none_or(|wanted| wanted == joined)
                    && (text.is_empty()
                        || conversation
                            .name
                            .as_ref()
                            .is_some_and(|name| name.as_str().to_lowercase().contains(&text)))
            })
            .map(|(conversation, members)| (conversation, *members))
            .collect();
        found.sort_by(|a, b| {
            let by_name =
                a.0.name
                    .as_ref()
                    .map(ToString::to_string)
                    .cmp(&b.0.name.as_ref().map(ToString::to_string));
            match query.sort.unwrap_or_default() {
                ChannelSort::Name => by_name.then(a.0.id.cmp(&b.0.id)),
                ChannelSort::Members => b.1.cmp(&a.1).then(by_name).then(a.0.id.cmp(&b.0.id)),
            }
        });
        found
            .into_iter()
            .map(|(conversation, _)| BrowsedChannel {
                conversation: conversation.clone(),
                joined: conversation.id == self.world.channel.id
                    || conversation.id == self.world.second_channel.id,
            })
            .collect()
    }

    fn browse_page(&self, query: &BrowseChannelsQuery) -> BrowseChannelsResponse {
        let all = self.browse_model(query);
        let offset = query.offset.unwrap_or(0) as usize;
        let limit = (query.limit.unwrap_or(50) as usize).min(DISCOVERY_SERVER_PAGE);
        let rest: Vec<BrowsedChannel> = all.into_iter().skip(offset).collect();
        BrowseChannelsResponse {
            has_more: rest.len() > limit,
            channels: rest.into_iter().take(limit).collect(),
        }
    }

    /// The server answers the newest request of a kind still out.
    fn answer_newest(&mut self, outcome: Outcome, generation_of: fn(&Effect) -> Option<u64>) {
        let newest = self
            .pending
            .iter()
            .enumerate()
            .filter_map(|(index, (effect, _))| generation_of(effect).map(|g| (g, index)))
            .max();
        if let Some((_, index)) = newest {
            self.resolve(index, outcome);
        }
    }

    // ----- The server's side of notifications, status, and the inbox -----

    fn server_sets_settings(&mut self, settings: NotificationSettings) {
        if self.server_settings != settings {
            self.server_settings = settings;
            self.log.push(LogEntry {
                event: LogEvent::Settings(settings),
            });
        }
    }

    fn server_sets_prefs(&mut self, prefs: ConversationNotificationPrefs) {
        let default = ConversationNotificationPrefs {
            conversation_id: prefs.conversation_id,
            level: None,
            muted: false,
        };
        let held = self
            .server_prefs
            .get(&prefs.conversation_id)
            .copied()
            .unwrap_or(default);
        if held != prefs {
            if prefs == default {
                self.server_prefs.remove(&prefs.conversation_id);
            } else {
                self.server_prefs.insert(prefs.conversation_id, prefs);
            }
            self.log.push(LogEntry {
                event: LogEvent::Prefs(prefs),
            });
        }
    }

    fn server_sets_status(&mut self, user: UserId, status: Option<UserStatus>) {
        let slot = if user == self.world.me.id {
            &mut self.server_status
        } else {
            &mut self.other_status
        };
        if *slot != status {
            slot.clone_from(&status);
            self.log.push(LogEntry {
                event: LogEvent::UserStatus { user, status },
            });
        }
    }

    // ----- The server's side of administration -----

    fn server_sets_workspace(&mut self, workspace: Workspace) {
        if self.server_workspace != workspace {
            self.server_workspace = workspace.clone();
            self.log.push(LogEntry {
                event: LogEvent::Workspace(workspace),
            });
        }
    }

    fn server_sets_role(&mut self, user: UserId, role: WorkspaceRole) {
        let slot = if user == self.world.me.id {
            &mut self.my_role
        } else {
            &mut self.other_role
        };
        if *slot != role {
            *slot = role;
            self.log.push(LogEntry {
                event: LogEvent::Role { user, role },
            });
        }
    }

    /// Only the other person is deactivated or reactivated this way.
    fn server_sets_deactivated(&mut self, user: UserId, deactivated: bool) {
        if user == self.world.other.id && self.other_deactivated != deactivated {
            self.other_deactivated = deactivated;
            self.log.push(LogEntry {
                event: LogEvent::Deactivated { user, deactivated },
            });
        }
    }

    fn server_creates_invitation(
        &mut self,
        request: &lazy_protocol::CreateInvitationRequest,
    ) -> CreatedInvitation {
        let number = self.codes_issued.len() + 1;
        let code = format!("sim-invite-code-{number:08}-xyz");
        assert!(lazy_domain::workspace::invite_code_is_wellformed(&code));
        let ttl = i64::try_from(request.expires_in_secs.unwrap_or(604_800)).unwrap_or(0);
        let invitation = Invitation {
            id: InvitationId::generate(),
            workspace_id: self.world.workspace.id,
            created_by: self.world.me.id,
            created_at: stamp(self.clock),
            expires_at: stamp(self.clock + ttl),
            max_uses: request.max_uses,
            uses: 0,
            state: InvitationState::Active,
        };
        self.invitations.insert(0, invitation.clone());
        self.codes_issued.push(code.clone());
        CreatedInvitation {
            invitation,
            code: InviteCode::new(code),
        }
    }

    fn sim_now(&self) -> lazy_domain::Timestamp {
        stamp(self.clock)
    }

    fn settings_variant(&self, pick: usize) -> NotificationSettings {
        let levels = [
            NotifyLevel::All,
            NotifyLevel::Mentions,
            NotifyLevel::Nothing,
        ];
        NotificationSettings {
            channel_level: levels[pick % 3],
            direct_level: levels[(pick / 3) % 3],
            thread_replies: (pick / 9).is_multiple_of(2),
            channel_mentions: (pick / 18).is_multiple_of(2),
            desktop: !(pick / 36).is_multiple_of(4),
            show_content: (pick / 144).is_multiple_of(2),
            dnd_until: match (pick / 288) % 4 {
                0 | 1 => None,
                2 => Some(stamp(self.clock + 900)),
                _ => Some(stamp(self.clock - 900)),
            },
            quiet_hours: None,
        }
    }

    fn prefs_variant(
        &self,
        conversation: ConversationId,
        pick: usize,
    ) -> ConversationNotificationPrefs {
        ConversationNotificationPrefs {
            conversation_id: conversation,
            level: [
                None,
                Some(NotifyLevel::All),
                Some(NotifyLevel::Mentions),
                Some(NotifyLevel::Nothing),
            ][pick % 4],
            muted: (pick / 4).is_multiple_of(3),
        }
    }

    fn status_variant(&self, pick: usize) -> Option<UserStatus> {
        if pick.is_multiple_of(4) {
            return None;
        }
        let emoji = (pick.is_multiple_of(3))
            .then(|| ReactionEmoji::parse("📅").ok())
            .flatten();
        let ends = match (pick / 8) % 3 {
            0 => None,
            1 => Some(stamp(self.clock + 600)),
            _ => Some(stamp(self.clock + 3600)),
        };
        UserStatus::new(&format!("busy {}", pick % 7), emoji, ends).ok()
    }

    /// The server's answer to a page of the inbox: the threads the person follows that still
    /// have replies, newest activity first, a page of at most [`INBOX_SERVER_PAGE`].
    fn inbox_page(&self, query: &InboxQuery) -> InboxPage {
        let position = BASE_HEAD + self.log.len() as i64;
        let mut entries = self.server_inbox();
        if query.unread == Some(true) {
            entries.retain(|entry| entry.state.unread_count > 0);
        }
        if let Some(before) = query.before {
            entries.retain(|entry| inbox_activity(entry) < before.get());
        }
        let limit = (query.limit.unwrap_or(20) as usize).min(INBOX_SERVER_PAGE);
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        let _ = position;
        InboxPage { entries, has_more }
    }

    /// Every followed thread that has replies, newest activity first.
    fn server_inbox(&self) -> Vec<InboxEntry> {
        let position = BASE_HEAD + self.log.len() as i64;
        let mut entries: Vec<InboxEntry> = self
            .thread_records
            .iter()
            .filter(|(root, rec)| rec.following && !self.purged.contains(root))
            .filter_map(|(root, _)| {
                let held = self.versions.get(root)?;
                let replies = self.live_replies(*root);
                let last = replies.last()?;
                let conversation = if held.conversation_id == self.world.channel.id {
                    self.world.channel.clone()
                } else {
                    self.world.second_channel.clone()
                };
                Some(InboxEntry {
                    root: held.clone(),
                    last_reply: Some((*last).clone()),
                    conversation,
                    state: self.thread_state_at(*root, position),
                })
            })
            .collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(inbox_activity(entry)));
        entries
    }

    /// The server answers the newest request of a kind still out.
    fn answer_newest_inbox(&mut self, outcome: Outcome) {
        let newest = self
            .pending
            .iter()
            .enumerate()
            .filter_map(|(index, (effect, _))| match effect {
                Effect::LoadInbox { generation, .. } => Some((*generation, index)),
                _ => None,
            })
            .max();
        if let Some((_, index)) = newest {
            self.resolve(index, outcome);
        }
    }

    /// What the rules say about a message that has just arrived, from the state the core shows:
    /// whether a desktop notification is due.
    fn notification_due(&self, message: &Message, live: bool) -> bool {
        let core = &self.driver.core;
        let me = self.world.me.id;
        if !live
            || self.unconfirmed
            || message.deleted
            || message.author_id == me
            || core.conversation(message.conversation_id).is_none()
        {
            return false;
        }
        let kind = core.conversation(message.conversation_id).unwrap().kind;
        let incoming = IncomingMessage {
            author: message.author_id,
            conversation_kind: kind,
            is_reply: message.thread_root.is_some(),
            in_channel: message.in_channel,
            mentions_me: message.mentions.users.contains(&me),
            mentions_channel: message.mentions.channel,
        };
        let following = message
            .thread_root
            .and_then(|root| core.thread_state(root))
            .is_some_and(|state| state.following);
        let settings = core.notification_settings();
        let Some(decision) = decide(
            &incoming,
            me,
            settings,
            core.conversation_notification_prefs(message.conversation_id),
            following,
            self.sim_now(),
        ) else {
            return false;
        };
        if decision.paused {
            NOTIFICATIONS_PAUSED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if !settings.desktop {
            return false;
        }
        let channel_traffic = message.thread_root.is_none() || message.in_channel;
        let in_open_thread = message
            .thread_root
            .is_some_and(|root| core.open_thread().is_some_and(|open| open.root == root));
        let looking = core.window_active()
            && core.selected_conversation() == Some(message.conversation_id)
            && (channel_traffic || in_open_thread);
        if looking {
            NOTIFICATIONS_SUPPRESSED_BY_FOCUS.fetch_add(1, Ordering::Relaxed);
        }
        !looking
    }

    /// The server gets to the newest search request still out.
    fn answer_newest_search(&mut self, outcome: Outcome) {
        let newest = self
            .pending
            .iter()
            .enumerate()
            .filter_map(|(index, (effect, _))| match effect {
                Effect::Search { generation, .. } => Some((*generation, index)),
                _ => None,
            })
            .max();
        if let Some((_, index)) = newest {
            self.resolve(index, outcome);
        }
    }

    /// A message the person is looking at as a search result.
    fn hit_message(&self, pick: usize) -> Option<MessageId> {
        let hits = &self.driver.core.search()?.hits;
        (!hits.is_empty()).then(|| hits[pick % hits.len()].message.id)
    }

    /// What the server answers to a search: single-word queries, each word a prefix of a word of
    /// the message, among the messages as they stood once the log had `seen` entries. Relevance
    /// puts whole-word matches first; both orders end on newest first. A page holds at most
    /// [`SEARCH_SERVER_PAGE`] results.
    fn search_page(&self, params: &SearchParams, seen: usize) -> SearchResponse {
        let word = params.q.to_lowercase();
        let versions = self.versions_as_of(seen);
        let mut found: Vec<(&Message, bool, u32, u32)> = Vec::new();
        for message in versions.values().filter(|message| !message.deleted) {
            let words = words_of(&message.document.plain_text());
            let Some((start, first)) = words.iter().find(|(_, w)| w.starts_with(&word)) else {
                continue;
            };
            let exact = words.iter().any(|(_, w)| *w == word);
            let start = u32::try_from(*start).unwrap();
            found.push((
                message,
                exact,
                start,
                start + u32::try_from(first.chars().count()).unwrap(),
            ));
        }
        found.sort_by_key(|(message, exact, ..)| {
            let rank = match params.sort {
                Some(SearchSort::Recent) => false,
                _ => !*exact,
            };
            (rank, std::cmp::Reverse(message.seq), message.id)
        });
        let offset = params.offset.unwrap_or(0) as usize;
        let limit = (params.limit.unwrap_or(20) as usize).min(SEARCH_SERVER_PAGE);
        let hits: Vec<SearchHit> = found
            .iter()
            .skip(offset)
            .take(limit)
            .map(|(message, _, start, end)| SearchHit {
                message: (*message).clone(),
                conversation: if message.conversation_id == self.world.channel.id {
                    self.world.channel.clone()
                } else {
                    self.world.second_channel.clone()
                },
                kind: SearchHitKind::Message,
                snippet: Snippet {
                    text: message.document.plain_text(),
                    highlights: vec![SnippetRange {
                        start: *start,
                        end: *end,
                    }],
                },
            })
            .collect();
        SearchResponse {
            has_more: offset + hits.len() < found.len(),
            hits,
            truncated: false,
        }
    }

    fn deliver(&mut self, count: usize) {
        // A deletion rewrites the earlier events of its message as tombstones, so a message that
        // is deleted by the time its events are delivered arrives deleted, at its final revision.
        let finals: HashMap<MessageId, Message> = self
            .deleted_ids()
            .into_iter()
            .map(|id| (id, self.versions[&id].clone()))
            .collect();
        // The rewritten earlier events of a deleted root carry no thread summary: the thread's
        // own events describe it.
        let as_delivered = |message: &Message| {
            finals
                .get(&message.id)
                .cloned()
                .map(|mut tombstone| {
                    if tombstone.thread_root.is_none() {
                        tombstone.thread = None;
                    }
                    tombstone
                })
                .unwrap_or_else(|| message.clone())
        };
        for _ in 0..count {
            // A deactivated person's connection is ended: nothing more reaches them.
            if self.pointer >= self.log.len() || self.me_deactivated || self.connection_lost {
                break;
            }
            let entry = &self.log[self.pointer];
            let event = match &entry.event {
                LogEvent::Message { message, operation } => {
                    if message.thread_root.is_some() && !finals.contains_key(&message.id) {
                        REPLIES_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    }
                    if finals.contains_key(&message.id) {
                        TOMBSTONE_CREATIONS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    } else if message.mentions.includes(self.world.me.id)
                        && message.author_id != self.world.me.id
                    {
                        MENTIONS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    }
                    Event::MessageCreated {
                        message: as_delivered(message),
                        operation_id: *operation,
                    }
                }
                LogEvent::Edited {
                    message,
                    operation,
                    previous,
                } => {
                    if finals.contains_key(&message.id) {
                        TOMBSTONE_EDITS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    } else if previous.includes(self.world.me.id)
                        != message.mentions.includes(self.world.me.id)
                    {
                        MENTION_EDITS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    }
                    // A rewritten edit keeps `previous_mentions`: they are user ids, not content.
                    Event::MessageEdited {
                        message: as_delivered(message),
                        operation_id: *operation,
                        previous_mentions: previous.clone(),
                    }
                }
                LogEvent::Membership {
                    conversation,
                    user,
                    added,
                } => {
                    MEMBER_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    if *added {
                        Event::MemberAdded {
                            conversation: self.conversation_at(*conversation, self.pointer + 1),
                            user_id: *user,
                        }
                    } else {
                        Event::MemberRemoved {
                            conversation_id: *conversation,
                            user_id: *user,
                        }
                    }
                }
                LogEvent::Reactions {
                    conversation,
                    message,
                    reactions,
                } => {
                    REACTION_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::ReactionsChanged {
                        conversation_id: *conversation,
                        message_id: *message,
                        reactions: reactions.clone(),
                    }
                }
                LogEvent::Deleted { message, had } => {
                    if had.includes(self.world.me.id) {
                        MENTION_DELETIONS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    }
                    let held = [self.world.channel.id, self.world.second_channel.id]
                        .into_iter()
                        .any(|c| {
                            self.driver
                                .core
                                .timeline(c)
                                .iter()
                                .any(|row| row.message_id() == Some(message.id))
                        });
                    let counter = if held {
                        &DELETIONS_OF_HELD_MESSAGES
                    } else {
                        &DELETIONS_OF_UNSEEN_MESSAGES
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                    Event::MessageDeleted {
                        message: message.clone(),
                        had_mentions: had.clone(),
                    }
                }
                LogEvent::State(state) => {
                    READ_STATE_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::UserStateChanged {
                        state: state.clone(),
                    }
                }
                LogEvent::ThreadChanged {
                    conversation,
                    root,
                    summary,
                } => {
                    THREAD_CHANGES_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::ThreadChanged {
                        conversation_id: *conversation,
                        root_id: *root,
                        summary: summary.clone(),
                    }
                }
                LogEvent::ThreadState(state) => {
                    THREAD_STATE_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::ThreadStateChanged {
                        state: state.clone(),
                    }
                }
                LogEvent::Emoji { emoji } => {
                    EMOJI_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::EmojiChanged {
                        emoji: emoji.clone(),
                    }
                }
                LogEvent::Settings(settings) => {
                    SETTINGS_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::NotificationSettingsChanged {
                        settings: *settings,
                    }
                }
                LogEvent::Prefs(prefs) => {
                    SETTINGS_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::ConversationNotificationPrefsChanged { prefs: *prefs }
                }
                LogEvent::UserStatus { user, status } => {
                    STATUS_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::UserStatusChanged {
                        user_id: *user,
                        status: status.clone(),
                    }
                }
                LogEvent::Saved { message, saved } => {
                    SAVED_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::SavedChanged {
                        message_id: *message,
                        saved: *saved,
                    }
                }
                LogEvent::Workspace(workspace) => {
                    WORKSPACE_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::WorkspaceUpdated {
                        workspace: workspace.clone(),
                    }
                }
                LogEvent::Role { user, role } => {
                    ROLE_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::MemberRoleChanged {
                        user_id: *user,
                        role: *role,
                    }
                }
                LogEvent::Deactivated { user, deactivated } => {
                    DEACTIVATION_EVENTS_DELIVERED.fetch_add(1, Ordering::Relaxed);
                    Event::MemberDeactivated {
                        user_id: *user,
                        deactivated: *deactivated,
                    }
                }
            };
            // Whether this is news: only after the connection has caught up.
            let live = self.socket_live;
            let created = match &event {
                Event::MessageCreated { message, .. } => Some(message.clone()),
                _ => None,
            };
            let envelope = EventEnvelope {
                workspace_id: self.world.workspace.id,
                seq: seq(BASE_HEAD + 1 + self.pointer as i64),
                occurred_at: self.sim_now(),
                event,
            };
            self.pointer += 1;
            self.last_notified = None;
            self.feed(Input::Frame(ServerFrame::Event(envelope)));
            if let Some(message) = created {
                let due = self.notification_due(&message, live);
                assert_eq!(
                    self.last_notified.as_ref().map(|n| n.message),
                    due.then_some(message.id),
                    "a notification was {} for a message{}\n{}",
                    if due { "missing" } else { "sent" },
                    if live { "" } else { " that was replayed" },
                    self.trace.join("\n")
                );
            }
        }
        if self.pointer == self.log.len() {
            let frame = ServerFrame::CaughtUp {
                workspace_id: self.world.workspace.id,
                seq: self.head(),
            };
            self.feed(Input::Frame(frame));
            self.socket_live = true;
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
                assert!(
                    saved.state.mention_count <= saved.state.unread_count,
                    "a saved state counts {} mentions among {} unread messages\n{}",
                    saved.state.mention_count,
                    saved.state.unread_count,
                    self.trace.join("\n")
                );
                let horizon = saved.as_of.get().max(cursor);
                let others = self.others_created(saved.state.conversation_id, horizon, 0);
                assert!(
                    saved.state.unread_count as usize <= others,
                    "a saved unread count of {} exceeds the {others} messages written by \
                     other people by position {horizon}",
                    saved.state.unread_count
                );
            }
            for saved in &workspace.threads {
                assert!(
                    saved.state.mention_count <= saved.state.unread_count,
                    "a saved thread state counts {} mentions among {} unread replies\n{}",
                    saved.state.mention_count,
                    saved.state.unread_count,
                    self.trace.join("\n")
                );
                let horizon = saved.as_of.get().max(cursor);
                let others = self.others_replies(saved.state.root_id, horizon);
                assert!(
                    saved.state.unread_count as usize <= others,
                    "a saved thread unread count of {} exceeds the {others} replies written by \
                     other people by position {horizon}",
                    saved.state.unread_count
                );
            }
            let deleted = self.deleted_ids();
            let as_of_cursor = self.versions_as_of((cursor - BASE_HEAD).max(0) as usize);
            for timeline in &workspace.timelines {
                let cached: HashSet<MessageId> = timeline.messages.iter().map(|m| m.id).collect();
                let floor = timeline.messages.first().map(|m| m.seq.get());
                // Edits and deletions the cursor has passed are reflected in what is cached.
                for saved in &timeline.messages {
                    // Only a root that still has replies is cached after its deletion.
                    assert!(
                        !saved.deleted || (saved.thread_root.is_none() && saved.thread.is_some()),
                        "a deleted message was cached"
                    );
                    assert!(
                        is_listed(saved),
                        "a reply that is only in its thread was cached in the conversation"
                    );
                    // Files never change, so a cached message has exactly the server's.
                    if !saved.deleted {
                        assert_eq!(
                            saved.attachments,
                            self.versions[&saved.id].attachments,
                            "message {} is cached with the wrong files\n{}",
                            saved.seq,
                            self.trace.join("\n")
                        );
                    }
                    if let Some(known) = as_of_cursor.get(&saved.id) {
                        assert!(
                            !known.deleted
                                || (known.thread_root.is_none() && known.thread.is_some()),
                            "message {} was deleted before the cursor {cursor} but is cached\n{}",
                            saved.seq,
                            self.trace.join("\n")
                        );
                        // A message that is deleted later has its edits served as tombstones,
                        // which wait for the deletion event, so only survivors are held to it.
                        assert!(
                            deleted.contains(&saved.id)
                                || saved.reactions_as_of >= known.reactions_as_of,
                            "message {} is cached with reactions from {} but the cursor {cursor} is past a change at {}\n{}",
                            saved.seq,
                            saved.reactions_as_of,
                            known.reactions_as_of,
                            self.trace.join("\n")
                        );
                        assert!(
                            deleted.contains(&saved.id) || saved.revision >= known.revision,
                            "message {} is cached at revision {} but the cursor {cursor} is past revision {}\n{}",
                            saved.seq,
                            saved.revision,
                            known.revision,
                            self.trace.join("\n")
                        );
                    }
                }
                for message in self.log.iter().filter_map(LogEntry::message) {
                    if message.conversation_id != timeline.conversation
                        || message.seq.get() > cursor
                        || !is_listed(message)
                        || deleted.contains(&message.id)
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
        let others = self.others_created(conversation, i64::MAX, 0);
        if let Some(state) = self.driver.core.user_state(conversation) {
            assert!(
                state.unread_count as usize <= others,
                "{} unread but only {others} messages from other people exist",
                state.unread_count
            );
            assert!(
                state.mention_count <= state.unread_count,
                "{} mentions among only {} unread messages\n{}",
                state.mention_count,
                state.unread_count,
                self.trace.join("\n")
            );
        }
        let sidebar = self.driver.core.sidebar();
        assert_eq!(
            sidebar.favorites.len() + sidebar.channels.len() + sidebar.direct_and_group.len(),
            self.driver.core.conversations().len()
        );
    }

    /// How many messages other people wrote in a conversation after `after`, up to `until`, that
    /// have not been deleted.
    fn others_messages(&self, conversation: ConversationId, until: i64, after: i64) -> usize {
        let deleted = self.deleted_ids();
        self.others_created(conversation, until, after)
            - self
                .log
                .iter()
                .filter_map(LogEntry::message)
                .filter(|m| {
                    m.conversation_id == conversation
                        && is_listed(m)
                        && m.author_id != self.world.me.id
                        && m.seq.get() > after
                        && m.seq.get() <= until
                        && deleted.contains(&m.id)
                })
                .count()
    }

    /// How many replies other people wrote under a root up to `until`, deleted ones included:
    /// an upper bound for a thread's unread count.
    fn others_replies(&self, root: MessageId, until: i64) -> usize {
        self.log
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| {
                m.thread_root == Some(root)
                    && m.author_id != self.world.me.id
                    && m.seq.get() <= until
            })
            .count()
    }

    /// Like [`Sim::others_messages`] but counting deleted messages too: an upper bound for a
    /// count that has not yet heard of a deletion.
    fn others_created(&self, conversation: ConversationId, until: i64, after: i64) -> usize {
        self.log
            .iter()
            .filter_map(LogEntry::message)
            .filter(|m| {
                m.conversation_id == conversation
                    && is_listed(m)
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
        // Nothing the dead process was doing goes on, and the new one sweeps its outbox of any
        // copy the journal does not name.
        self.uploads_running.clear();
        self.upload_cancelled.clear();
        self.downloads_running.clear();
        self.images_in_flight.clear();
        self.emoji_in_flight.clear();
        let named: HashSet<AttachmentId> = self
            .journal
            .values()
            .flat_map(|entry| entry.attachments.iter().map(|file| file.id))
            .collect();
        if !named.is_empty() {
            RESTARTS_WITH_FILES_IN_THE_JOURNAL.fetch_add(1, Ordering::Relaxed);
        }
        self.outbox.retain(|id| named.contains(id));
        self.disturbed.extend(self.journal.keys().copied());
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
        self.shown_reactions.clear();
        // Whichever write of what the person was looking at survived is what the next run finds.
        let view = self.view_disk[pick.wrapping_mul(31) % self.view_disk.len()].clone();
        self.view_disk = vec![view.clone()];
        self.expected_restore = view
            .last_selected
            .iter()
            .find(|(workspace, _)| *workspace == self.world.workspace.id)
            .map(|(_, conversation)| *conversation);
        // Whichever write of the recent searches survived is what the next run finds. The search
        // itself was on screen only.
        let recents = self.recents_disk[pick.wrapping_mul(17) % self.recents_disk.len()].clone();
        self.recents_disk = vec![recents.clone()];
        self.recents_expected = recents.clone();
        self.last_run_word = None;
        // A new process numbers its requests from the start, has no idea of the time until it is
        // told, has caught up with nothing, and has shown nothing yet.
        self.last_search_generation = 0;
        self.ticked = false;
        self.socket_live = false;
        self.badge_seen = 0;
        self.last_notified = None;
        self.latest_notification = None;
        self.saved_newest = 0;
        self.people_newest = 0;
        self.profile_newest = 0;
        self.browse_newest = 0;
        self.invitations_newest = 0;
        self.connection_lost = false;
        // Whichever write of the recently visited conversations survived is what the next run
        // finds.
        let visited = self.recent_conversations_disk
            [pick.wrapping_mul(13) % self.recent_conversations_disk.len()]
        .clone();
        self.recent_conversations_disk = vec![visited.clone()];
        self.recent_conversations_expected = visited.clone();
        if !visited.is_empty() {
            RESTARTS_WITH_RECENT_CONVERSATIONS.fetch_add(1, Ordering::Relaxed);
        }
        if !recents.is_empty() {
            RESTARTS_WITH_RECENT_SEARCHES.fetch_add(1, Ordering::Relaxed);
        }
        self.driver = Driver::new();
        let journal: Vec<JournalEntry> = self.journal.values().cloned().collect();
        self.feed(Input::Started(StartupState {
            session_restored: true,
            journal,
            cache: surviving,
            view,
            recent_searches: recents,
            recent_conversations: visited,
            ..Default::default()
        }));
    }

    /// One of more kinds than a message can hold.
    fn wide_palette(&self, pick: usize) -> ReactionEmoji {
        static KINDS: std::sync::OnceLock<Vec<ReactionEmoji>> = std::sync::OnceLock::new();
        let kinds = KINDS.get_or_init(|| {
            lazy_domain::emoji::in_group(lazy_domain::emoji::Group::SmileysAndEmotion)
                .take(lazy_domain::MAX_REACTION_KINDS + 2)
                .map(|e| ReactionEmoji::parse(e.as_str()).unwrap())
                .collect()
        });
        kinds[pick % kinds.len()]
    }

    /// Whether adding `emoji` would give the message more different kinds than it can hold.
    fn would_exceed_kinds(&self, id: MessageId, emoji: ReactionEmoji) -> bool {
        let message = &self.versions[&id];
        message.reactions.len() >= lazy_domain::MAX_REACTION_KINDS
            && message.reactions.iter().all(|r| r.emoji != emoji)
    }

    /// One of a few emoji, so reactions collide often enough to matter.
    fn palette(&self, pick: usize) -> ReactionEmoji {
        ["👍", "🎉", "❤️"].map(|text| ReactionEmoji::parse(text).unwrap())[pick % 3]
    }

    /// Who a message notifies, by kind: nobody; the person; a third person; everyone; the person
    /// and a third person.
    fn mentions_of(&self, kind: usize) -> Mentions {
        let me = self.world.me.id;
        match kind % 5 {
            0 => Mentions::default(),
            1 => Mentions {
                users: vec![me],
                channel: false,
            },
            2 => Mentions {
                users: vec![self.third],
                channel: false,
            },
            3 => Mentions {
                users: Vec::new(),
                channel: true,
            },
            _ => Mentions {
                users: vec![me, self.third],
                channel: false,
            },
        }
    }

    /// Who is in a channel once the log had `visible` entries: the two people who start in it,
    /// and the third person while joined.
    fn server_members(&self, conversation: ConversationId, visible: usize) -> Vec<UserId> {
        let mut members = vec![self.world.me.id, self.world.other.id];
        for entry in &self.log[..visible.min(self.log.len())] {
            if let LogEvent::Membership {
                conversation: c,
                user,
                added,
            } = &entry.event
                && *c == conversation
            {
                members.retain(|member| member != user);
                if *added {
                    members.push(*user);
                }
            }
        }
        members
    }

    /// The conversation as an event at log index `visible - 1` would carry it.
    fn conversation_at(
        &self,
        conversation: ConversationId,
        visible: usize,
    ) -> lazy_protocol::Conversation {
        let mut found = if conversation == self.world.channel.id {
            self.world.channel.clone()
        } else {
            self.world.second_channel.clone()
        };
        found.member_count =
            u32::try_from(self.server_members(conversation, visible).len()).unwrap();
        found
    }

    /// A message that exists (is not deleted), chosen by `pick` in a stable order.
    fn live_message(&self, pick: usize) -> Option<MessageId> {
        let mut live: Vec<&Message> = self.versions.values().filter(|m| !m.deleted).collect();
        live.sort_by_key(|m| m.seq);
        (!live.is_empty()).then(|| live[pick % live.len()].id)
    }

    fn next_edit_text(&mut self) -> MessageDocument {
        let text = format!("edited-{}", self.edits_made);
        self.edits_made += 1;
        MessageDocument::from_markdown(&text).unwrap()
    }

    /// The messages on screen (confirmed rows) that satisfy `wanted`, in screen order: the
    /// conversations' rows, then the replies of the open thread.
    fn rows_where(
        &self,
        wanted: impl Fn(&lazy_client_core::ClientCore, ConversationId, MessageId) -> bool,
    ) -> Vec<(ConversationId, MessageId)> {
        let wanted = &wanted;
        let mut rows: Vec<(ConversationId, MessageId)> =
            [self.world.channel.id, self.world.second_channel.id]
                .into_iter()
                .flat_map(|conversation| {
                    self.driver
                        .core
                        .timeline(conversation)
                        .iter()
                        .filter_map(TimelineItem::message_id)
                        .filter(move |id| wanted(&self.driver.core, conversation, *id))
                        .map(move |id| (conversation, id))
                })
                .collect();
        if let Some(open) = self.driver.core.open_thread()
            && let Some(view) = self.driver.core.thread(open.root)
        {
            rows.extend(
                view.replies
                    .iter()
                    .filter_map(TimelineItem::message_id)
                    .filter(|id| wanted(&self.driver.core, open.conversation, *id))
                    .map(|id| (open.conversation, id)),
            );
        }
        rows
    }

    /// A live top-level message, chosen by `pick` in a stable order.
    fn live_root(&self, pick: usize) -> Option<MessageId> {
        let mut roots: Vec<&Message> = self
            .versions
            .values()
            .filter(|m| !m.deleted && m.thread_root.is_none())
            .collect();
        roots.sort_by_key(|m| m.seq);
        (!roots.is_empty()).then(|| roots[pick % roots.len()].id)
    }

    /// A top-level message on screen (live or a placeholder), chosen by `pick`.
    fn root_on_screen(&self, pick: usize) -> Option<(ConversationId, MessageId)> {
        let roots: Vec<(ConversationId, MessageId)> =
            [self.world.channel.id, self.world.second_channel.id]
                .into_iter()
                .flat_map(|conversation| {
                    self.driver
                        .core
                        .timeline(conversation)
                        .iter()
                        .filter(|row| row.thread_root.is_none())
                        .filter_map(TimelineItem::message_id)
                        .map(move |id| (conversation, id))
                })
                .collect();
        (!roots.is_empty()).then(|| roots[pick % roots.len()])
    }

    /// Files for the person to attach: pasted bytes or files on disk, named by `prefix`.
    fn new_files(&mut self, count: usize, bytes: bool, prefix: &str) -> Vec<LocalFile> {
        (0..count)
            .map(|k| {
                self.files_attached += 1;
                let name = format!("{prefix}{k}-{}.bin", self.files_attached);
                let source = if bytes {
                    FileSource::Bytes(FileBytes::new(vec![k as u8; 1 + self.files_attached % 300]))
                } else {
                    FileSource::Path(std::path::PathBuf::from(format!("/files/{name}")))
                };
                LocalFile { source, name }
            })
            .collect()
    }

    /// The number a message of the person's carries, in its words or the names of its files.
    fn number_of(row: &TimelineItem) -> Option<usize> {
        row.document
            .plain_text()
            .strip_prefix("mine-")
            .and_then(leading_number)
            .or_else(|| {
                row.attachments.iter().find_map(|file| {
                    file.name
                        .strip_prefix('f')
                        .and_then(|rest| rest.split('-').next())
                        .and_then(|n| n.parse().ok())
                })
            })
    }

    /// Notes that a failed message is about to be sent again.
    fn note_retry(&mut self, conversation: ConversationId, operation: OperationId) {
        let number = self
            .driver
            .core
            .timeline(conversation)
            .iter()
            .chain(
                self.driver
                    .core
                    .open_thread()
                    .and_then(|open| self.driver.core.thread(open.root))
                    .map(|view| view.replies)
                    .unwrap_or_default(),
            )
            .find(|row| row.operation_id() == Some(operation))
            .and_then(Self::number_of);
        if let Some(number) = number {
            self.retried_numbers.insert(number);
        }
    }

    /// Why a failed message failed, as its row says.
    fn failure_reason(&self, conversation: ConversationId, operation: OperationId) -> String {
        let pane = self
            .driver
            .core
            .open_thread()
            .and_then(|open| self.driver.core.thread(open.root))
            .map(|view| view.replies)
            .unwrap_or_default();
        self.driver
            .core
            .timeline(conversation)
            .iter()
            .chain(pane)
            .find(|row| row.operation_id() == Some(operation))
            .and_then(|row| match &row.delivery {
                Delivery::Failed { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Messages with files that failed, on screen in a conversation or in the open thread.
    fn failed_rows_with_files(&self) -> Vec<(ConversationId, OperationId)> {
        let failed = |row: &TimelineItem| {
            matches!(row.delivery, Delivery::Failed { .. }) && !row.attachments.is_empty()
        };
        let mut rows = Vec::new();
        for conversation in [self.world.channel.id, self.world.second_channel.id] {
            for row in self.driver.core.timeline(conversation) {
                if let (true, Some(operation)) = (failed(row), row.operation_id()) {
                    rows.push((conversation, operation));
                }
            }
        }
        if let Some(open) = self.driver.core.open_thread()
            && let Some(view) = self.driver.core.thread(open.root)
        {
            for row in view.replies {
                if let (true, Some(operation)) = (failed(row), row.operation_id()) {
                    rows.push((open.conversation, operation));
                }
            }
        }
        rows
    }

    /// Whether there are chips to remove or failed messages with files to settle.
    fn has_file_work(&self) -> bool {
        let chips = |target| !self.driver.core.composer_attachments(target).is_empty();
        chips(ComposerTarget::Conversation(self.world.channel.id))
            || chips(ComposerTarget::Conversation(self.world.second_channel.id))
            || self
                .versions
                .keys()
                .any(|id| chips(ComposerTarget::Thread(*id)))
            || !self.failed_rows_with_files().is_empty()
    }

    /// The person tidies up: every chip is removed, and every failed message with files is tried
    /// again (a bounded number of times) or discarded, so nothing is left in the outbox that
    /// nothing on screen accounts for.
    fn tidy_files(&mut self) {
        let mut targets = vec![
            ComposerTarget::Conversation(self.world.channel.id),
            ComposerTarget::Conversation(self.world.second_channel.id),
        ];
        let mut roots: Vec<MessageId> = self.versions.keys().copied().collect();
        roots.sort();
        targets.extend(roots.into_iter().map(ComposerTarget::Thread));
        for target in targets {
            let chips = self.driver.core.composer_attachments(target).to_vec();
            for chip in chips {
                self.feed(Input::RemoveAttachment {
                    target,
                    attachment: chip.id,
                });
            }
        }
        for (conversation, operation) in self.failed_rows_with_files() {
            // A message that names an emoji that is gone is refused every time: nobody retries it
            // more than once.
            let hopeless = self
                .failure_reason(conversation, operation)
                .contains("emoji is gone");
            let retry = self.file_retries_left > 0 && !hopeless;
            self.file_retries_left = self.file_retries_left.saturating_sub(1);
            if retry {
                FAILED_FILE_ROWS_RETRIED.fetch_add(1, Ordering::Relaxed);
                self.note_retry(conversation, operation);
            }
            self.feed(if retry {
                Input::RetryOperation {
                    conversation,
                    operation,
                }
            } else {
                Input::DiscardOperation {
                    conversation,
                    operation,
                }
            });
        }
    }

    /// Everything that can be done to the server choice, while the session is not one where it
    /// may be changed: nothing is asked, nothing moves, and nothing but a notice shows.
    fn server_poke(&mut self, pick: usize) {
        if matches!(
            self.driver.core.phase(),
            SessionPhase::SignedOut { .. } | SessionPhase::Unreachable { .. }
        ) {
            return;
        }
        let server = self.driver.core.server().clone();
        let notice = self.driver.core.notice().map(str::to_owned);
        let generation = (pick / 6 % 5) as u64;
        let input = match pick % 6 {
            0 => Input::SetServerUrl("chat.example.com".into()),
            1 => Input::CheckServer,
            2 => Input::ServerChecked {
                generation,
                result: Ok(lazy_protocol::PROTOCOL_VERSION),
            },
            3 => Input::ServerSwitched {
                generation,
                result: Ok(Box::default()),
            },
            4 => Input::SetServerUrl("not an address".into()),
            _ => Input::SetServerUrl(server.url.clone()),
        };
        let out = self.driver.send(input);
        assert!(
            out.effects.is_empty(),
            "the server choice asked for something while signed in: {:?}\n{}",
            out.effects,
            self.trace.join("\n")
        );
        assert_eq!(
            self.driver.core.server(),
            &server,
            "the server changed under a session"
        );
        if notice.is_none() && self.driver.core.notice().is_some() {
            self.driver.send(Input::DismissNotice);
        }
    }

    fn step(&mut self, action: Action) {
        match action {
            Action::ServerPoke(pick) => self.server_poke(pick),
            Action::OtherPosts(flag) => {
                let conversation = self.conversation(flag);
                let at = BASE_HEAD + 1 + self.log.len() as i64;
                let message = self
                    .world
                    .message(conversation, &self.world.other.clone(), at, "hi");
                self.server_creates(message);
            }
            Action::OtherEdits(pick) => {
                if let Some(id) = self.live_message(pick) {
                    let document = self.next_edit_text();
                    // The rest of the number picks what the edit says about mentions.
                    let mentions = self.mentions_of(pick / 7919);
                    self.server_edit(id, document, None, Some(mentions));
                }
            }
            Action::OtherMentions(flag, kind) => {
                let conversation = self.conversation(flag);
                let at = BASE_HEAD + 1 + self.log.len() as i64;
                let mut message =
                    self.world
                        .message(conversation, &self.world.other.clone(), at, "hey");
                message.mentions = self.mentions_of(kind);
                self.server_creates(message);
            }
            Action::ThirdToggles(flag) => {
                let conversation = self.conversation(flag);
                let joined = self
                    .server_members(conversation, self.log.len())
                    .contains(&self.third);
                self.log.push(LogEntry {
                    event: LogEvent::Membership {
                        conversation,
                        user: self.third,
                        added: !joined,
                    },
                });
            }
            Action::ServerDeletes(pick) => {
                if let Some(id) = self.live_message(pick) {
                    self.server_delete(id);
                }
            }
            Action::UserEdits(pick) => {
                let rows = self
                    .rows_where(|core, conversation, id| core.can_edit_message(conversation, id));
                if !rows.is_empty() {
                    let (conversation, message) = rows[pick % rows.len()];
                    let text = if pick % 3 == 0 && !self.versions[&message].attachments.is_empty() {
                        // The files are the message; the words may go.
                        String::new()
                    } else {
                        format!("edited-{}", self.edits_made)
                    };
                    self.edits_made += 1;
                    self.feed(Input::EditMessage {
                        conversation,
                        message,
                        text,
                    });
                }
            }
            Action::UserEditRaces(pick) => {
                let rows = self
                    .rows_where(|core, conversation, id| core.can_edit_message(conversation, id));
                if !rows.is_empty() {
                    let (conversation, message) = rows[pick % rows.len()];
                    let text = format!("edited-{}", self.edits_made);
                    self.edits_made += 1;
                    self.feed(Input::EditMessage {
                        conversation,
                        message,
                        text,
                    });
                    if !self.versions[&message].deleted {
                        let document = self.next_edit_text();
                        self.server_edit(message, document, None, None);
                    }
                }
            }
            Action::UserReacts(pick, which) => {
                let rows = self
                    .rows_where(|core, conversation, id| core.can_react_message(conversation, id));
                if !rows.is_empty() {
                    let (conversation, message) = rows[pick % rows.len()];
                    self.feed(Input::ToggleReaction {
                        conversation,
                        message,
                        emoji: self.palette(which),
                    });
                }
            }
            Action::UserReactsWide(pick, which) => {
                let rows = self
                    .rows_where(|core, conversation, id| core.can_react_message(conversation, id));
                if !rows.is_empty() {
                    let (conversation, message) = rows[pick % rows.len()];
                    let emoji = self.wide_palette(which);
                    if self.would_exceed_kinds(message, emoji) {
                        TOGGLES_ON_FULL_MESSAGES.fetch_add(1, Ordering::Relaxed);
                    }
                    self.feed(Input::ToggleReaction {
                        conversation,
                        message,
                        emoji,
                    });
                }
            }
            Action::UserSwapsReaction(pick) => {
                let me = self.world.me.id;
                let rows = self
                    .rows_where(|core, conversation, id| core.can_react_message(conversation, id));
                // A message that holds all it can is the interesting one; take it if there is one.
                let full: Vec<_> = rows
                    .iter()
                    .copied()
                    .filter(|(_, id)| {
                        self.versions[id].reactions.len() >= lazy_domain::MAX_REACTION_KINDS
                    })
                    .collect();
                let on_a_full_message = !full.is_empty();
                let rows = if full.is_empty() { rows } else { full };
                if on_a_full_message {
                    // The person sees the message as it is: they have heard what the server has.
                    let behind = self.log.len() - self.pointer;
                    self.deliver(behind);
                }
                if !rows.is_empty() {
                    let (conversation, message) = rows[pick % rows.len()];
                    let held = &self.versions[&message].reactions;
                    let withdrawn = held.iter().find(|r| r.users.contains(&me)).map(|r| r.emoji);
                    let added = (0..lazy_domain::MAX_REACTION_KINDS + 2)
                        .map(|kind| self.wide_palette(kind))
                        .find(|emoji| held.iter().all(|r| r.emoji != *emoji));
                    if let (Some(withdrawn), Some(added)) = (withdrawn, added) {
                        if held.len() >= lazy_domain::MAX_REACTION_KINDS {
                            SWAPS_ON_FULL_MESSAGES.fetch_add(1, Ordering::Relaxed);
                        }
                        for emoji in [withdrawn, added] {
                            self.feed(Input::ToggleReaction {
                                conversation,
                                message,
                                emoji,
                            });
                        }
                    }
                }
            }
            Action::FillReactions(pick) => {
                if let Some(id) = self.live_message(pick) {
                    let actors = [self.world.other.id, self.third];
                    let me = self.world.me.id;
                    for kind in 0..lazy_domain::MAX_REACTION_KINDS {
                        let emoji = self.wide_palette(kind + pick);
                        if self.would_exceed_kinds(id, emoji) {
                            break;
                        }
                        let actor = if kind == 0 { me } else { actors[kind % 2] };
                        self.server_react(id, actor, emoji, true);
                    }
                }
            }
            Action::OtherReacts(pick, which, who) => {
                if let Some(id) = self.live_message(pick) {
                    let actor = [self.world.other.id, self.third, self.world.me.id][who % 3];
                    let emoji = self.palette(which);
                    let present = !self.server_has_reaction(id, actor, emoji);
                    self.server_react(id, actor, emoji, present);
                }
            }
            Action::UserDeletes(pick) => {
                let rows = self
                    .rows_where(|core, conversation, id| core.can_delete_message(conversation, id));
                if !rows.is_empty() {
                    let (conversation, message) = rows[pick % rows.len()];
                    self.feed(Input::DeleteMessage {
                        conversation,
                        message,
                    });
                }
            }
            Action::UserSends(flag) => {
                let conversation = self.conversation(flag);
                let text = format!("mine-{}", self.submitted);
                self.submitted += 1;
                self.feed(Input::SendMessage { conversation, text });
            }
            Action::OtherReplies(pick, in_channel, kind) => {
                if let Some(root) = self.live_root(pick) {
                    let conversation = self.versions[&root].conversation_id;
                    let mentions = self.mentions_of(kind);
                    let other = self.world.other.id;
                    self.server_reply(other, conversation, root, in_channel, mentions, "re", None);
                }
            }
            Action::OtherRepliesToOpen(in_channel, kind) => {
                if let Some(open) = self.driver.core.open_thread()
                    && self.versions.get(&open.root).is_some_and(|r| !r.deleted)
                {
                    let mentions = self.mentions_of(kind);
                    let other = self.world.other.id;
                    self.server_reply(
                        other,
                        open.conversation,
                        open.root,
                        in_channel,
                        mentions,
                        "re",
                        None,
                    );
                }
            }
            Action::OpenThread(pick) => {
                self.expected_restore = None;
                if let Some((conversation, root)) = self.root_on_screen(pick) {
                    THREADS_OPENED.fetch_add(1, Ordering::Relaxed);
                    self.feed(Input::OpenThread { conversation, root });
                }
            }
            Action::CloseThread => self.feed(Input::CloseThread),
            Action::LoadOlderReplies => {
                if let Some(open) = self.driver.core.open_thread() {
                    self.feed(Input::LoadOlderReplies { root: open.root });
                }
            }
            Action::UserReplies(in_channel) => {
                if let Some(open) = self.driver.core.open_thread() {
                    let text = format!("mine-{}", self.submitted);
                    self.submitted += 1;
                    REPLIES_SENT_BY_THE_PERSON.fetch_add(1, Ordering::Relaxed);
                    self.feed(Input::SendReply {
                        conversation: open.conversation,
                        root: open.root,
                        text,
                        also_in_channel: in_channel,
                    });
                }
            }
            Action::ThreadBottom(at_bottom) => {
                if let Some(open) = self.driver.core.open_thread() {
                    self.feed(Input::ThreadAtBottom {
                        root: open.root,
                        at_bottom,
                    });
                }
            }
            Action::UserFollows(pick, following) => {
                let root = match self.driver.core.open_thread() {
                    Some(open) => Some(open.root),
                    None => self.root_on_screen(pick).map(|(_, root)| root),
                };
                if let Some(root) = root {
                    self.feed(Input::SetThreadFollow { root, following });
                }
            }
            Action::OtherDeviceFollows(pick, following) => {
                if let Some(root) = self.live_root(pick) {
                    self.server_set_follow(root, following);
                }
            }
            Action::OtherDeviceReadsThread(pick) => {
                if let Some(root) = self.live_root(pick) {
                    self.server_reads_thread(root, i64::MAX);
                }
            }
            Action::UserAttaches(flag, count, bytes) => {
                let target = ComposerTarget::Conversation(self.conversation(flag));
                let files = self.new_files(count % 3 + 1, bytes, "c");
                self.feed(Input::AttachFiles { target, files });
            }
            Action::UserAttachesToThread(count) => {
                if let Some(open) = self.driver.core.open_thread() {
                    let files = self.new_files(count % 3 + 1, count % 2 == 0, "c");
                    self.feed(Input::AttachFiles {
                        target: ComposerTarget::Thread(open.root),
                        files,
                    });
                }
            }
            Action::UserSendsFiles(flag, count, only_files) => {
                let conversation = self.conversation(flag);
                let target = ComposerTarget::Conversation(conversation);
                let number = self.submitted;
                self.submitted += 1;
                let prefix = format!("f{number}-");
                let files = self.new_files(count % 3 + 1, count % 2 == 0, &prefix);
                self.feed(Input::AttachFiles { target, files });
                // A message that is only files carries its number in their names, so it needs
                // at least one of them to have been accepted.
                let own_file = self
                    .driver
                    .core
                    .composer_attachments(target)
                    .iter()
                    .any(|chip| chip.name.starts_with(&prefix));
                let text = if only_files && own_file {
                    String::new()
                } else {
                    format!("mine-{number}")
                };
                self.feed(Input::SendMessage { conversation, text });
            }
            Action::UserHandlesChip(flag, pick, which) => {
                let target = match (flag, self.driver.core.open_thread()) {
                    (false, Some(open)) => ComposerTarget::Thread(open.root),
                    (flag, _) => ComposerTarget::Conversation(self.conversation(flag)),
                };
                let chips = self.driver.core.composer_attachments(target).to_vec();
                if !chips.is_empty() {
                    let chip = &chips[pick % chips.len()];
                    let attachment = chip.id;
                    self.feed(match which % 3 {
                        0 => Input::RemoveAttachment { target, attachment },
                        1 => Input::CancelAttachment { target, attachment },
                        _ => Input::RetryAttachment { target, attachment },
                    });
                }
            }
            Action::UserHandlesFailedFiles(pick, retry) => {
                let rows = self.failed_rows_with_files();
                if !rows.is_empty() {
                    let (conversation, operation) = rows[pick % rows.len()];
                    if retry {
                        self.note_retry(conversation, operation);
                    }
                    self.feed(if retry {
                        Input::RetryOperation {
                            conversation,
                            operation,
                        }
                    } else {
                        Input::DiscardOperation {
                            conversation,
                            operation,
                        }
                    });
                }
            }
            Action::ServerExpiresUploads => {
                self.server_uploads
                    .retain(|id, _| self.attached.contains(id));
            }
            Action::UserViewsFile(pick, which) => {
                let mut files: Vec<Attachment> = Vec::new();
                for conversation in [self.world.channel.id, self.world.second_channel.id] {
                    for row in self.driver.core.timeline(conversation) {
                        if row.is_confirmed() {
                            files.extend(row.attachments.iter().cloned());
                        }
                    }
                }
                if !files.is_empty() {
                    let file = files[pick % files.len()].clone();
                    self.requested_images.insert(file.id);
                    self.feed(match which % 5 {
                        0 | 1 => Input::LoadThumbnail(file.id),
                        2 => Input::LoadFullImage(file.id),
                        3 => Input::DownloadAttachment {
                            attachment: file,
                            destination: std::path::PathBuf::from("/saved/file"),
                        },
                        _ => Input::CancelDownload(file.id),
                    });
                }
            }
            Action::ServerAddsEmoji => {
                self.emoji_made += 1;
                let emoji = CustomEmoji {
                    id: EmojiId::generate(),
                    workspace_id: self.world.workspace.id,
                    name: format!("sim_{}", self.emoji_made),
                    created_by: self.world.other.id,
                    created_at: Timestamp::now(),
                    deleted: false,
                };
                self.server_changes_emoji(emoji);
            }
            Action::ServerRenamesEmoji(pick) => {
                let live = self.live_emoji();
                if !live.is_empty() {
                    let mut emoji = live[pick % live.len()].clone();
                    self.emoji_made += 1;
                    emoji.name = format!("sim_{}", self.emoji_made);
                    EMOJI_RENAMES.fetch_add(1, Ordering::Relaxed);
                    self.server_changes_emoji(emoji);
                }
            }
            Action::ServerDeletesEmoji(pick) => {
                let live = self.live_emoji();
                if !live.is_empty() {
                    let mut emoji = live[pick % live.len()].clone();
                    emoji.deleted = true;
                    EMOJI_DELETIONS.fetch_add(1, Ordering::Relaxed);
                    self.server_changes_emoji(emoji);
                }
            }
            Action::UserSendsEmoji(flag, pick) => {
                let conversation = self.conversation(flag);
                let live: Vec<CustomEmoji> =
                    self.driver.core.live_custom_emoji().cloned().collect();
                let number = self.submitted;
                self.submitted += 1;
                let text = match live.get(pick.checked_rem(live.len()).unwrap_or(0)) {
                    Some(emoji) => format!("mine-{number} :{}:", emoji.name),
                    None => format!("mine-{number}"),
                };
                self.feed(Input::SendMessage { conversation, text });
            }
            Action::UserReactsCustom(pick, which) => {
                let live: Vec<CustomEmoji> =
                    self.driver.core.live_custom_emoji().cloned().collect();
                let rows = self
                    .rows_where(|core, conversation, id| core.can_react_message(conversation, id));
                if !rows.is_empty() && !live.is_empty() {
                    let (conversation, message) = rows[pick % rows.len()];
                    self.feed(Input::ToggleReaction {
                        conversation,
                        message,
                        emoji: lazy_domain::ReactionEmoji::custom(live[which % live.len()].id),
                    });
                }
            }
            Action::OtherReactsCustom(pick, which) => {
                let live = self.live_emoji();
                if let (Some(id), false) = (self.live_message(pick), live.is_empty()) {
                    let actor = [self.world.other.id, self.third][which % 2];
                    let emoji = lazy_domain::ReactionEmoji::custom(live[which / 2 % live.len()].id);
                    let present = !self.server_has_reaction(id, actor, emoji);
                    if !(present && self.would_exceed_kinds(id, emoji)) {
                        self.server_react(id, actor, emoji, present);
                    }
                }
            }
            Action::UserViewsEmoji(pick) => {
                let ids: Vec<EmojiId> = self
                    .driver
                    .core
                    .custom_emoji()
                    .iter()
                    .map(|emoji| emoji.id)
                    .collect();
                if !ids.is_empty() {
                    let id = ids[pick % ids.len()];
                    self.requested_emoji.insert(id);
                    self.feed(Input::LoadEmojiImage(id));
                }
            }
            Action::UserRecordsEmoji(pick) => {
                let live: Vec<CustomEmoji> =
                    self.driver.core.live_custom_emoji().cloned().collect();
                if !live.is_empty() {
                    self.feed(Input::RecordEmojiUse(lazy_domain::ReactionEmoji::custom(
                        live[pick % live.len()].id,
                    )));
                }
            }
            Action::Resolve(pick, outcome) => self.resolve(pick, outcome),
            Action::Deliver(count) => self.deliver(count),
            Action::Reconnect(_) if self.me_deactivated => {
                // The server refuses the subscription.
                self.connection_lost = true;
                self.socket_live = false;
                self.feed(Input::Frame(ServerFrame::Error {
                    code: ErrorCode::Forbidden,
                    message: "you are no longer a member of that workspace".into(),
                }));
                self.shown_reactions.clear();
            }
            Action::Reconnect(back) => {
                self.pointer = self.pointer.saturating_sub(back).max(self.subscribed_at);
                // The connection dropped and came back: what it replays is not news until it has
                // caught up again.
                let workspace = self.world.workspace.id;
                self.feed(Input::Socket(SocketEvent::Closed {
                    workspace,
                    attempt: 1,
                }));
                self.feed(Input::Socket(SocketEvent::Opened { workspace }));
                self.socket_live = false;
            }
            Action::Select(flag) => {
                self.expected_restore = None;
                let conversation = self.conversation(flag);
                self.feed(Input::SelectConversation(conversation));
            }
            Action::LoadOlder(flag) => {
                let conversation = self.conversation(flag);
                self.feed(Input::LoadOlder { conversation });
            }
            Action::Restart(pick) => self.restart(pick),
            Action::SearchTypes(pick) => {
                let text = SEARCH_WORDS[pick % SEARCH_WORDS.len()].to_owned();
                self.feed(Input::SetSearchQuery { text });
            }
            Action::SearchRuns(pick) => {
                let text = SEARCH_WORDS[pick % SEARCH_WORDS.len()].to_owned();
                self.feed(Input::SetSearchQuery { text });
                self.feed(Input::RunSearch);
            }
            Action::SearchMore => self.feed(Input::LoadMoreSearch),
            Action::SearchAnswered(outcome) => self.answer_newest_search(outcome),
            Action::SearchRunsAnswered(pick, outcome) => {
                let text = SEARCH_WORDS[pick % SEARCH_WORDS.len()].to_owned();
                self.feed(Input::SetSearchQuery { text });
                self.feed(Input::RunSearch);
                self.answer_newest_search(outcome);
            }
            Action::SearchPagesOn(outcome) => {
                self.feed(Input::LoadMoreSearch);
                self.answer_newest_search(outcome);
            }
            Action::ServerDeletesHit(pick) => {
                if let Some(id) = self.hit_message(pick) {
                    self.server_delete(id);
                }
            }
            Action::OtherEditsHit(pick) => {
                if let Some(id) = self.hit_message(pick)
                    && !self.versions[&id].deleted
                {
                    let document = self.next_edit_text();
                    self.server_edit(id, document, None, None);
                }
            }
            Action::SearchSorts(recent) => self.feed(Input::SetSearchSort(if recent {
                SearchSort::Recent
            } else {
                SearchSort::Relevance
            })),
            Action::SearchOpensHit(pick) => {
                let hits = self.driver.core.search().map_or(0, |view| view.hits.len());
                if hits > 0 {
                    self.feed(Input::OpenSearchHit(pick % hits));
                    // The words of the run those hits answer are the most recent search.
                    assert_eq!(
                        self.driver.core.recent_searches().first(),
                        self.last_run_word.as_ref(),
                        "{}",
                        self.trace.join("\n")
                    );
                    SEARCH_HITS_OPENED.fetch_add(1, Ordering::Relaxed);
                }
            }
            Action::SearchCloses => {
                self.feed(Input::CloseSearch);
                self.last_run_word = None;
            }
            Action::Tick(minutes) => {
                self.clock += i64::try_from(minutes).unwrap() * 60;
                self.ticked = true;
                let now = self.sim_now();
                self.feed(Input::Clock(now));
            }
            Action::UserSetsSettings(pick) => {
                let settings = self.settings_variant(pick);
                self.feed(Input::SetNotificationSettings(settings));
            }
            Action::UserSnoozes(pick) => {
                let until =
                    (pick % 3 != 0).then(|| stamp(self.clock + (pick % 7) as i64 * 600 - 600));
                self.feed(Input::PauseNotifications { until });
            }
            Action::UserSetsPrefs(flag, pick) => {
                let conversation = self.conversation(flag);
                let prefs = self.prefs_variant(conversation, pick);
                self.feed(Input::SetConversationNotificationPrefs {
                    conversation,
                    level: prefs.level,
                    muted: prefs.muted,
                });
            }
            Action::OtherDeviceSetsSettings(pick) => {
                let settings = self.settings_variant(pick);
                self.server_sets_settings(settings);
            }
            Action::OtherDeviceSetsPrefs(flag, pick) => {
                let conversation = self.conversation(flag);
                let prefs = self.prefs_variant(conversation, pick);
                self.server_sets_prefs(prefs);
            }
            Action::OtherDeviceSetsStatus(pick) => {
                let status = self.status_variant(pick);
                self.server_sets_status(self.world.me.id, status);
            }
            Action::UserSetsStatus(pick) => {
                let status = self.status_variant(pick);
                self.feed(Input::SetStatus(status));
            }
            Action::OtherSetsStatus(pick) => {
                let status = self.status_variant(pick);
                self.server_sets_status(self.world.other.id, status);
            }
            Action::AdminRenames(pick) => {
                let (name, settings) = workspace_variant(pick);
                let mut workspace = self.server_workspace.clone();
                if let Some(name) = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) {
                    workspace.name = name;
                }
                if let Some(settings) = settings {
                    workspace.settings = settings;
                }
                self.server_sets_workspace(workspace);
            }
            Action::UserUpdatesWorkspace(pick) => {
                let (name, settings) = workspace_variant(pick);
                self.feed(Input::UpdateWorkspace { name, settings });
            }
            Action::ServerSetsMyRole(pick) => {
                let me = self.world.me.id;
                self.server_sets_role(me, ROLES[pick % ROLES.len()]);
            }
            Action::ServerSetsOtherRole(pick) => {
                let other = self.world.other.id;
                self.server_sets_role(other, ROLES[pick % ROLES.len()]);
            }
            Action::ServerDeactivatesOther(flag) => {
                let other = self.world.other.id;
                self.server_sets_deactivated(other, flag);
            }
            Action::UserSetsRole(own, pick) => {
                let user_id = if own {
                    self.world.me.id
                } else {
                    self.world.other.id
                };
                self.feed(Input::SetMemberRole {
                    user_id,
                    role: ROLES[pick % ROLES.len()],
                });
            }
            Action::UserDeactivates(flag) => {
                // (Now and then the person tries it on themself, which is refused here.)
                let user_id = if flag && self.trace.len().is_multiple_of(5) {
                    self.world.me.id
                } else {
                    self.world.other.id
                };
                self.feed(Input::SetMemberDeactivated {
                    user_id,
                    deactivated: flag,
                });
            }
            Action::PeopleIncludeDeactivated(flag) => {
                self.feed(Input::SetPeopleIncludeDeactivated(flag))
            }
            Action::InvitationsOpens => self.feed(Input::OpenInvitations),
            Action::InvitationsCloses => self.feed(Input::CloseInvitations),
            Action::UserCreatesInvitation(pick) => {
                let (expires_in_secs, max_uses) =
                    INVITATION_REQUESTS[pick % INVITATION_REQUESTS.len()];
                self.feed(Input::CreateInvitation {
                    expires_in_secs,
                    max_uses,
                });
            }
            Action::UserRevokesInvitation(pick) => {
                let shown: Vec<InvitationId> = self
                    .driver
                    .core
                    .invitations()
                    .items
                    .iter()
                    .map(|i| i.id)
                    .collect();
                let id = if shown.is_empty() || pick.is_multiple_of(7) {
                    InvitationId::generate()
                } else {
                    shown[pick % shown.len()]
                };
                self.feed(Input::RevokeInvitation(id));
            }
            Action::InvitationsAnswered(outcome) => self.answer_newest(outcome, |e| match e {
                Effect::LoadInvitations { generation, .. } => Some(*generation),
                _ => None,
            }),
            Action::InvitationCodeDismissed => self.feed(Input::DismissCreatedInvitation),
            Action::ServerDeactivatesMe => {
                if !self.me_deactivated {
                    self.me_deactivated = true;
                    self.connection_lost = true;
                    self.socket_live = false;
                    ACCESS_LOSSES.fetch_add(1, Ordering::Relaxed);
                    self.feed(Input::Frame(ServerFrame::Error {
                        code: ErrorCode::Forbidden,
                        message: "you are no longer a member of that workspace".into(),
                    }));
                    // As after a snapshot, the picture starts over.
                    self.shown_reactions.clear();
                }
            }
            Action::ServerReactivatesMe => self.me_deactivated = false,
            Action::AccessRechecked => self.feed(Input::RetryLoad),
            Action::NotificationClicked => {
                if let Some(notification) = self.latest_notification.clone() {
                    NOTIFICATIONS_CLICKED.fetch_add(1, Ordering::Relaxed);
                    self.feed(Input::NotificationActivated {
                        conversation: notification.conversation,
                        message: notification.message,
                    });
                }
            }
            Action::UserSaves(pick, save) => {
                let message = if save {
                    self.live_message(pick)
                } else {
                    let mut saved: Vec<MessageId> = self.server_saved.keys().copied().collect();
                    saved.sort();
                    (!saved.is_empty()).then(|| saved[pick % saved.len()])
                };
                if let Some(message_id) = message {
                    SAVES_ASKED.fetch_add(1, Ordering::Relaxed);
                    self.feed(Input::SetMessageSaved {
                        message_id,
                        saved: save,
                    });
                }
            }
            Action::OtherDeviceSaves(pick, save) => {
                let message = if save {
                    self.live_message(pick)
                } else {
                    let mut saved: Vec<MessageId> = self.server_saved.keys().copied().collect();
                    saved.sort();
                    (!saved.is_empty()).then(|| saved[pick % saved.len()])
                };
                if let Some(message) = message {
                    self.server_sets_saved(message, save);
                }
            }
            Action::SavedOpens => self.feed(Input::OpenSaved),
            Action::SavedMore => self.feed(Input::LoadMoreSaved),
            Action::SavedCloses => self.feed(Input::CloseSaved),
            Action::SavedAnswered(outcome) => self.answer_newest(outcome, |e| match e {
                Effect::LoadSaved { generation, .. } => Some(*generation),
                _ => None,
            }),
            Action::PeopleOpens => self.feed(Input::OpenPeople),
            Action::PeopleSearches(pick) => {
                let text = PEOPLE_QUERIES[pick % PEOPLE_QUERIES.len()].to_owned();
                self.feed(Input::SetPeopleQuery(text));
            }
            Action::PeopleMore => self.feed(Input::LoadMorePeople),
            Action::PeopleCloses => self.feed(Input::ClosePeople),
            Action::PeopleAnswered(outcome) => self.answer_newest(outcome, |e| match e {
                Effect::LoadPeople { generation, .. } => Some(*generation),
                _ => None,
            }),
            Action::ProfileOpens(flag) => {
                let user_id = if flag {
                    self.world.me.id
                } else {
                    self.world.other.id
                };
                self.feed(Input::OpenProfile { user_id });
            }
            Action::ProfileCloses => self.feed(Input::CloseProfile),
            Action::ProfileAnswered(outcome) => self.answer_newest(outcome, |e| match e {
                Effect::LoadProfile { generation, .. } => Some(*generation),
                _ => None,
            }),
            Action::BrowseOpens => self.feed(Input::BrowseChannels),
            Action::BrowseSearches(pick) => {
                let text = BROWSE_QUERIES[pick % BROWSE_QUERIES.len()].to_owned();
                self.feed(Input::SetBrowseQuery(text));
            }
            Action::BrowseFilters(pick) => {
                let joined = [None, Some(true), Some(false)][pick % 3];
                self.feed(Input::SetBrowseJoined(joined));
            }
            Action::BrowseSorts(by_members) => self.feed(Input::SetBrowseSort(if by_members {
                ChannelSort::Members
            } else {
                ChannelSort::Name
            })),
            Action::BrowseMore => self.feed(Input::LoadMoreBrowse),
            Action::BrowseCloses => self.feed(Input::CloseBrowse),
            Action::BrowseAnswered(outcome) => self.answer_newest(outcome, |e| match e {
                Effect::LoadBrowse { generation, .. } => Some(*generation),
                _ => None,
            }),
            Action::QuickSwitches(text, pick) => {
                let query = SWITCHER_QUERIES[text % SWITCHER_QUERIES.len()];
                let items = self.driver.core.quick_switcher_results(query);
                // A person with no conversation would open one on the server; the simulation
                // has no such route.
                let choices: Vec<_> = items
                    .into_iter()
                    .filter(|item| !matches!(item.target, QuickSwitcherTarget::Person(_)))
                    .collect();
                if !choices.is_empty() {
                    let item = choices[pick % choices.len()].clone();
                    QUICK_SWITCHES_CHOSEN.fetch_add(1, Ordering::Relaxed);
                    self.feed(Input::ChooseQuickSwitcher(item.target));
                }
            }
            Action::NotifiedMessageDeleted => {
                if let Some(id) = self.latest_notification.as_ref().map(|n| n.message) {
                    self.server_delete(id);
                }
            }
            Action::InboxOpens => {
                self.feed(Input::OpenInbox);
                if self.driver.core.inbox().is_some() {
                    INBOX_OPENED.fetch_add(1, Ordering::Relaxed);
                }
            }
            Action::InboxFilters(unread_only) => self.feed(Input::LoadInbox { unread_only }),
            Action::InboxMore => self.feed(Input::LoadMoreInbox),
            Action::InboxMarksRead => self.feed(Input::MarkInboxRead),
            Action::InboxCloses => self.feed(Input::CloseInbox),
            Action::InboxAnswered(outcome) => self.answer_newest_inbox(outcome),
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
                self.socket_live = false;
                // The picture starts over: rows come back from whatever replays next.
                self.shown_reactions.clear();
            }
        }
    }

    fn check_invariants(&mut self) {
        self.check_admin_invariants();
        for conversation in [self.world.channel.id, self.world.second_channel.id] {
            let rows = self.driver.core.timeline(conversation);
            for row in rows {
                // A placeholder for a deleted root has no reactions to go back on.
                if row.deleted {
                    continue;
                }
                if let Some(id) = row.message_id() {
                    let highest = self.shown_reactions.entry(id).or_insert(EventSeq::ZERO);
                    assert!(
                        row.reactions_as_of >= *highest,
                        "message {id} went back from reactions at {highest} to ones at {}\n{}",
                        row.reactions_as_of,
                        self.trace.join("\n")
                    );
                    *highest = row.reactions_as_of;
                }
            }
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
        self.check_thread_invariants();
        self.check_search();
        self.check_notifications();
        self.check_discovery();
    }

    /// The lists load exactly while a request is out, the recent conversations are what was last
    /// saved, and the quick switcher gives only what it should.
    fn check_discovery(&self) {
        let core = &self.driver.core;
        let waiting = |newest: u64, kind: fn(&Effect) -> Option<u64>| {
            self.pending
                .iter()
                .any(|(effect, _)| kind(effect) == Some(newest))
        };
        let saved = core.saved();
        let mut ids = HashSet::new();
        for item in &saved.items {
            assert!(ids.insert(item.message.id), "a saved item twice");
        }
        match saved.status {
            LoadStatus::Idle => assert!(saved.items.is_empty()),
            LoadStatus::Loading => assert!(
                waiting(self.saved_newest, |e| match e {
                    Effect::LoadSaved { generation, .. } => Some(*generation),
                    _ => None,
                }),
                "the saved list loads although nothing is out\n{}",
                self.trace.join("\n")
            ),
            _ => {}
        }
        assert!(saved.open || saved.items.is_empty());
        if let LoadStatus::Loading = core.people().status {
            assert!(waiting(self.people_newest, |e| match e {
                Effect::LoadPeople { generation, .. } => Some(*generation),
                _ => None,
            }));
        }
        let mut people = HashSet::new();
        for person in &core.people().people {
            assert!(people.insert(person.user.id), "a person twice");
        }
        if let Some(profile) = core.profile()
            && profile.status == LoadStatus::Loading
        {
            assert!(waiting(self.profile_newest, |e| match e {
                Effect::LoadProfile { generation, .. } => Some(*generation),
                _ => None,
            }));
        }
        if let LoadStatus::Loading = core.browse().status {
            assert!(waiting(self.browse_newest, |e| match e {
                Effect::LoadBrowse { generation, .. } => Some(*generation),
                _ => None,
            }));
        }
        let mut channels = HashSet::new();
        for entry in &core.browse().channels {
            assert!(channels.insert(entry.conversation.id), "a channel twice");
        }

        let recent = core.recent_conversations();
        assert_eq!(
            recent,
            self.recent_conversations_expected.as_slice(),
            "the recent conversations are what was last saved\n{}",
            self.trace.join("\n")
        );
        if let Some(selected) = core.selected_conversation() {
            assert_eq!(
                recent.first(),
                Some(&selected),
                "the selection is the most recent"
            );
        }
        // (Sampled: the switcher is a pure function of what the checks above already read.)
        let sampled = self.trace.len();
        if sampled.is_multiple_of(4) {
            let query = SWITCHER_QUERIES[sampled / 4 % SWITCHER_QUERIES.len()];
            let items = core.quick_switcher_results(query);
            assert!(items.len() <= QUICK_SWITCHER_LIMIT);
            let mut targets = HashSet::new();
            for item in &items {
                assert!(
                    targets.insert(format!("{:?}", item.target)),
                    "a result twice"
                );
                if let QuickSwitcherTarget::Conversation(id) = item.target {
                    assert!(
                        core.conversation(id).is_some(),
                        "not a conversation of the person"
                    );
                }
            }
            // (Before the person is known there is nothing to offer.)
            if !query.is_empty() && !items.is_empty() {
                assert!(matches!(
                    items.last().map(|i| &i.target),
                    Some(QuickSwitcherTarget::BrowseChannels { .. })
                ));
            }
        }
    }

    /// The badge always shows the attention count, and the inbox's status is true of its requests.
    fn check_notifications(&self) {
        let core = &self.driver.core;
        assert_eq!(
            self.badge_seen,
            core.attention_count(),
            "the badge is not the attention count\n{}",
            self.trace.join("\n")
        );
        let Some(view) = core.inbox() else {
            return;
        };
        let mut roots = HashSet::new();
        for entry in &view.entries {
            assert!(roots.insert(entry.root.id), "a thread twice in the inbox");
        }
        let waiting = self.pending.iter().any(|(effect, _)| {
            matches!(effect, Effect::LoadInbox { generation, .. } if *generation == view.generation)
        });
        match &view.status {
            InboxStatus::Idle => assert!(view.entries.is_empty() && view.generation == 0),
            InboxStatus::Loading => assert!(waiting, "the inbox loads although nothing is out"),
            _ => {}
        }
    }

    /// What the search shows answers the last run, and what the core says about where it stands
    /// is true of the requests the server has not answered.
    fn check_search(&self) {
        assert_eq!(
            self.driver.core.recent_searches(),
            self.recents_expected.as_slice(),
            "the recent searches are what was last saved\n{}",
            self.trace.join("\n")
        );
        let Some(view) = self.driver.core.search() else {
            return;
        };
        if !view.hits.is_empty() {
            let word = self
                .last_run_word
                .as_deref()
                .expect("hits are on screen although nothing was run");
            let mut distinct = HashSet::new();
            for hit in &view.hits {
                assert!(
                    words_of(&hit.snippet.text)
                        .iter()
                        .any(|(_, found)| found.starts_with(word)),
                    "a hit that does not answer {word:?} is shown: {:?}\n{}",
                    hit.snippet.text,
                    self.trace.join("\n")
                );
                assert!(distinct.insert(hit.message.id), "a hit is shown twice");
            }
        }
        let waiting = self.pending.iter().any(|(effect, _)| {
            matches!(effect, Effect::Search { generation, .. } if *generation == view.generation)
        });
        if view.status == SearchStatus::Idle {
            assert!(view.hits.is_empty());
            assert_eq!(view.generation, 0, "a view that asked for nothing");
        } else {
            assert_eq!(
                view.status == SearchStatus::Loading,
                waiting,
                "the view is loading exactly while its request is out: {:?}\n{}",
                view.status,
                self.trace.join("\n")
            );
        }
        assert!(!view.at_limit || view.has_more);
    }

    /// The open thread's rows are ordered and unique, and no thread state counts more than
    /// exists.
    fn check_thread_invariants(&self) {
        if let Some(open) = self.driver.core.open_thread() {
            let view = self
                .driver
                .core
                .thread(open.root)
                .expect("the open thread has a view");
            let confirmed = view.replies.iter().take_while(|r| r.is_confirmed()).count();
            assert!(
                view.replies[confirmed..].iter().all(|r| !r.is_confirmed()),
                "confirmed replies must precede unconfirmed ones"
            );
            let seqs: Vec<_> = view.replies[..confirmed]
                .iter()
                .filter_map(TimelineItem::seq)
                .collect();
            assert!(
                seqs.windows(2).all(|w| w[0] < w[1]),
                "replies not strictly ordered: {seqs:?}\n{}",
                self.trace.join("\n")
            );
            let mut keys = HashSet::new();
            let mut ids = HashSet::new();
            for row in view.replies {
                assert!(keys.insert(row.key), "duplicate reply key");
                if let Some(id) = row.message_id() {
                    assert!(ids.insert(id), "duplicate reply");
                }
                assert_eq!(
                    row.thread_root,
                    Some(open.root),
                    "a reply of another thread"
                );
                assert!(!row.deleted, "a deleted reply is shown");
            }
            if let Some(root) = view.root {
                assert!(
                    root.thread_root.is_none(),
                    "the root of a thread is a reply"
                );
            }
        }
        for root in self
            .versions
            .values()
            .filter(|m| m.thread_root.is_none())
            .map(|m| m.id)
        {
            if let Some(state) = self.driver.core.thread_state(root) {
                assert!(
                    state.mention_count <= state.unread_count,
                    "{} mentions among {} unread replies\n{}",
                    state.mention_count,
                    state.unread_count,
                    self.trace.join("\n")
                );
                assert!(
                    state.unread_count as usize <= self.others_replies(root, i64::MAX),
                    "{} unread replies but only {} exist from other people\n{}",
                    state.unread_count,
                    self.others_replies(root, i64::MAX),
                    self.trace.join("\n")
                );
            }
        }
    }

    /// Delivers everything, answers every request normally, and retries stuck sends until the
    /// system is quiet.
    fn drain(&mut self) {
        // The person is let back in, and checks.
        self.me_deactivated = false;
        if self.driver.core.primary_workspace().is_none() {
            self.feed(Input::RetryLoad);
        }
        // The connection comes back a few times while the system settles; that is when edits
        // recovered from the journal are asked about again.
        let mut reconnects = 6;
        let mut next_root = 0usize;
        let mut opens_left = 60;
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
                // Edits are only visible on the rows of a conversation that has been opened, so
                // the person eventually opens each one.
                if matches!(
                    self.driver.core.history_status(conversation),
                    lazy_client_core::HistoryStatus::NotLoaded
                        | lazy_client_core::HistoryStatus::Failed { .. }
                ) {
                    self.feed(Input::SelectConversation(conversation));
                    progressed = true;
                }
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
                // Edits that stopped: an unknown outcome is asked about again; anything else is
                // abandoned, which shows the message as the server has it.
                let stopped: Vec<(MessageId, bool)> = self
                    .driver
                    .core
                    .timeline(conversation)
                    .iter()
                    .filter_map(|row| match (&row.edit, row.message_id()) {
                        (
                            EditState::Failed {
                                retry: lazy_client_core::RetryKind::SameOperation,
                                ..
                            },
                            Some(id),
                        ) => Some((id, true)),
                        (EditState::Failed { .. } | EditState::Conflict { .. }, Some(id)) => {
                            Some((id, false))
                        }
                        _ => None,
                    })
                    .collect();
                for (message, retry) in stopped {
                    self.feed(if retry {
                        Input::RetryEdit {
                            conversation,
                            message,
                        }
                    } else {
                        Input::CancelEdit {
                            conversation,
                            message,
                        }
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
            // The open thread: replies that stopped are retried, edits that stopped are settled,
            // and older replies are loaded.
            if let Some(open) = self.driver.core.open_thread() {
                let (rows, history, root_row) = {
                    let view = self
                        .driver
                        .core
                        .thread(open.root)
                        .expect("the open thread has a view");
                    (
                        view.replies.to_vec(),
                        view.history.clone(),
                        view.root.cloned(),
                    )
                };
                let stuck: Vec<OperationId> = rows
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
                        conversation: open.conversation,
                        operation,
                    });
                    progressed = true;
                }
                let stopped: Vec<(MessageId, bool)> = rows
                    .iter()
                    .chain(root_row.iter())
                    .filter_map(|row| match (&row.edit, row.message_id()) {
                        (
                            EditState::Failed {
                                retry: lazy_client_core::RetryKind::SameOperation,
                                ..
                            },
                            Some(id),
                        ) => Some((id, true)),
                        (EditState::Failed { .. } | EditState::Conflict { .. }, Some(id)) => {
                            Some((id, false))
                        }
                        _ => None,
                    })
                    .collect();
                for (message, retry) in stopped {
                    let conversation = open.conversation;
                    self.feed(if retry {
                        Input::RetryEdit {
                            conversation,
                            message,
                        }
                    } else {
                        Input::CancelEdit {
                            conversation,
                            message,
                        }
                    });
                    progressed = true;
                }
                if matches!(
                    history,
                    lazy_client_core::HistoryStatus::NotLoaded
                        | lazy_client_core::HistoryStatus::Failed { .. }
                        | lazy_client_core::HistoryStatus::Ready { has_more: true }
                ) {
                    self.feed(Input::LoadOlderReplies { root: open.root });
                    progressed = true;
                }
            }
            if !progressed {
                if self.has_file_work() {
                    self.tidy_files();
                    continue;
                }
                // Replies that are still unsettled live in threads that are not open (their rows
                // are parked); opening each shows them, and a stuck one can then be retried.
                if self.driver.core.unsettled_operations() > 0 && opens_left > 0 {
                    let open = self.driver.core.open_thread().map(|t| t.root);
                    let mut roots: Vec<MessageId> = self
                        .reply_roots
                        .iter()
                        .copied()
                        .filter(|root| Some(*root) != open)
                        .collect();
                    roots.sort();
                    if !roots.is_empty() {
                        let root = roots[next_root % roots.len()];
                        next_root += 1;
                        opens_left -= 1;
                        let conversation = self.versions[&root].conversation_id;
                        self.feed(Input::OpenThread { conversation, root });
                        continue;
                    }
                }
                if reconnects == 0 {
                    return;
                }
                reconnects -= 1;
                self.deliver(0);
            }
        }
        panic!("the simulation did not quiesce");
    }

    /// Every thread, opened (or kept open) and fully loaded, shows exactly the server's replies
    /// at their latest versions, and its root as the server has it.
    fn verify_threads(&mut self) {
        let mut roots: Vec<(EventSeq, MessageId)> = self
            .versions
            .values()
            .filter(|m| m.thread_root.is_none() && (!m.deleted || m.thread.is_some()))
            .map(|m| (m.seq, m.id))
            .collect();
        roots.sort();
        for (_, root) in roots {
            let conversation = self.versions[&root].conversation_id;
            self.feed(Input::OpenThread { conversation, root });
            self.drain();
            let view =
                self.driver.core.thread(root).unwrap_or_else(|| {
                    panic!("the thread should be open\n{}", self.trace.join("\n"))
                });
            let server_root = &self.versions[&root];
            let shown_root = view.root.expect("the root of an open thread is shown");
            assert_eq!(
                (shown_root.deleted, &shown_root.thread),
                (server_root.deleted, &server_root.thread),
                "the thread's root\n{}",
                self.trace.join("\n")
            );
            if !server_root.deleted {
                assert_eq!(
                    (
                        &shown_root.document,
                        shown_root.revision(),
                        &shown_root.reactions
                    ),
                    (
                        &server_root.document,
                        Some(server_root.revision),
                        &server_root.reactions
                    ),
                    "the thread's root content\n{}",
                    self.trace.join("\n")
                );
            }
            let shown: Vec<&TimelineItem> =
                view.replies.iter().filter(|r| r.is_confirmed()).collect();
            let server = self.live_replies(root);
            assert_eq!(
                shown
                    .iter()
                    .filter_map(|r| r.message_id())
                    .collect::<Vec<_>>(),
                server.iter().map(|m| m.id).collect::<Vec<_>>(),
                "the replies of a thread\n{}",
                self.trace.join("\n")
            );
            for (row, message) in shown.iter().zip(&server) {
                assert_eq!(
                    (&row.document, row.revision(), row.edited_at, &row.reactions),
                    (
                        &message.document,
                        Some(message.revision),
                        message.edited_at,
                        &message.reactions
                    ),
                    "a reply is not at the server's latest version\n{}",
                    self.trace.join("\n")
                );
                assert_eq!(
                    (row.thread_root, row.in_channel),
                    (message.thread_root, message.in_channel)
                );
                assert_eq!(
                    &row.attachments,
                    &message.attachments,
                    "a reply does not show exactly its files\n{}",
                    self.trace.join("\n")
                );
            }
            REPLY_ROWS_AT_THE_END.fetch_add(shown.len(), Ordering::Relaxed);
        }
    }

    /// Once every event is delivered, a hit is for a message that still exists, as the server
    /// last had it.
    fn check_search_final(&self) {
        let Some(view) = self.driver.core.search() else {
            return;
        };
        assert_ne!(view.status, SearchStatus::Loading, "a search is still out");
        for hit in &view.hits {
            let latest = &self.versions[&hit.message.id];
            assert!(
                !latest.deleted,
                "a deleted message is still a hit\n{}",
                self.trace.join("\n")
            );
            assert_eq!(
                hit.message.revision,
                latest.revision,
                "a hit shows an older revision than the server holds\n{}",
                self.trace.join("\n")
            );
        }
        if !view.hits.is_empty() {
            SEARCH_HITS_ON_SCREEN_AT_THE_END.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Signing out forgets the search and the recent searches, and stops asking.
    fn signs_out(&mut self) {
        self.recents_expected.clear();
        self.recent_conversations_expected.clear();
        let out = self.driver.send(Input::SignOut);
        assert!(self.driver.core.search().is_none());
        assert!(self.driver.core.recent_searches().is_empty());
        assert!(self.driver.core.inbox().is_none());
        assert!(!self.driver.core.saved().open && self.driver.core.saved().items.is_empty());
        assert!(!self.driver.core.people().open && self.driver.core.profile().is_none());
        assert!(self.driver.core.browse().channels.is_empty());
        assert!(self.driver.core.recent_conversations().is_empty());
        let invitations = self.driver.core.invitations();
        assert!(!invitations.open && invitations.items.is_empty() && invitations.created.is_none());
        assert_eq!(self.driver.core.access_lost(), None);
        assert_eq!(self.driver.core.my_role(), None);
        assert_eq!(
            self.driver.core.workspace_settings(),
            WorkspaceSettings::default()
        );
        assert_eq!(self.driver.core.attention_count(), 0);
        assert_eq!(
            *self.driver.core.notification_settings(),
            NotificationSettings::default()
        );
        assert!(
            !out.effects
                .iter()
                .any(|effect| matches!(effect, Effect::PersistRecentSearches(_))),
            "nothing is saved at sign-out; the runtime deletes the file"
        );
    }

    /// Once everything is delivered and answered, settings, preferences, and statuses are the
    /// server's, and the inbox lists the threads the person follows.
    fn check_notifications_final(&self) {
        let core = &self.driver.core;
        assert_eq!(
            *core.notification_settings(),
            self.server_settings,
            "the settings are not the server's\n{}",
            self.trace.join("\n")
        );
        for conversation in [self.world.channel.id, self.world.second_channel.id] {
            let held = self.server_prefs.get(&conversation);
            assert_eq!(
                core.conversation_notification_prefs(conversation),
                held,
                "a preference is not the server's\n{}",
                self.trace.join("\n")
            );
            assert_eq!(core.is_muted(conversation), held.is_some_and(|p| p.muted));
        }
        let visible = |status: &Option<UserStatus>| {
            status
                .clone()
                .filter(|s| !self.ticked || s.is_active(self.sim_now()))
        };
        assert_eq!(
            core.status_of(self.world.me.id).cloned(),
            visible(&self.server_status),
            "the person's status is not the server's\n{}",
            self.trace.join("\n")
        );
        assert_eq!(
            core.status_of(self.world.other.id).cloned(),
            visible(&self.other_status),
            "someone's status is not the server's\n{}",
            self.trace.join("\n")
        );
        if self.ticked {
            let paused = self.server_settings.is_paused(self.sim_now());
            assert_eq!(core.notifications_paused(), paused);
        }
        let Some(view) = core.inbox() else {
            return;
        };
        assert_ne!(
            view.status,
            InboxStatus::Loading,
            "the inbox is still loading"
        );
        if view.status != InboxStatus::Ready || view.unread_only {
            return;
        }
        // The first threads of the server's list, as they are now.
        let server = self.server_inbox();
        let shown: Vec<MessageId> = view.entries.iter().map(|e| e.root.id).collect();
        let expected: Vec<MessageId> = server
            .iter()
            .take(view.entries.len())
            .map(|e| e.root.id)
            .collect();
        assert_eq!(
            shown,
            expected,
            "the inbox does not list the threads the person follows\n{}",
            self.trace.join("\n")
        );
        if !view.has_more {
            assert_eq!(shown.len(), server.len(), "the inbox lacks threads");
        }
        for (entry, held) in view.entries.iter().zip(&server) {
            assert_eq!(
                entry.state,
                held.state,
                "an inbox thread's state is out of date\n{}",
                self.trace.join("\n")
            );
            assert_eq!(
                entry.root.thread.as_ref().map(|t| t.reply_count),
                held.root.thread.as_ref().map(|t| t.reply_count)
            );
        }
        if !view.entries.is_empty() {
            INBOX_ENTRIES_AT_THE_END.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Once everything is delivered and answered, a marker is the server's, and what each open
    /// list shows is the start of the server's list.
    /// What must hold after every step, whatever is in flight.
    fn check_admin_invariants(&self) {
        let core = &self.driver.core;
        // A deactivated person is never offered, and a workspace the person was let out of is
        // gone from the screen.
        for user in core.members() {
            assert!(!user.deactivated, "a deactivated person is offered");
        }
        let other = self.world.other.id;
        if core.member(other).is_some_and(|user| user.deactivated)
            && !core.members().iter().any(|user| user.id == other)
        {
            DEACTIVATED_HIDDEN_FROM_PICKERS.fetch_add(1, Ordering::Relaxed);
        }
        assert_eq!(
            core.can_manage_workspace(),
            core.my_role()
                .is_some_and(WorkspaceRole::can_manage_workspace)
        );
        assert_eq!(
            core.can_create_channel(),
            core.my_role()
                .is_some_and(|role| core.workspace_settings().channel_creation.allows(role))
        );
        if core.access_lost().is_some() {
            ACCESS_LOSSES_SHOWN.fetch_add(1, Ordering::Relaxed);
            assert!(
                core.primary_workspace().is_none()
                    && core.conversations().is_empty()
                    && core.selected_conversation().is_none()
                    && !core.can_manage_workspace(),
                "access is lost but a workspace is still on screen\n{}",
                self.trace.join("\n")
            );
        }
        if core.primary_workspace().is_some() {
            assert_eq!(core.access_lost(), None);
        }
        // The invitations and the code.
        let view = core.invitations();
        if !view.open {
            assert!(
                view.items.is_empty() && view.created.is_none() && view.status == LoadStatus::Idle,
                "a closed invitations view holds something\n{}",
                self.trace.join("\n")
            );
        } else {
            assert!(
                core.can_manage_workspace(),
                "invitations are open for a non-manager"
            );
            if view.status == LoadStatus::Loading {
                assert!(
                    self.pending.iter().any(|(effect, _)| matches!(
                        effect,
                        Effect::LoadInvitations { generation, .. }
                            if *generation == self.invitations_newest
                    )),
                    "the invitations load although nothing is out\n{}",
                    self.trace.join("\n")
                );
            }
        }
        let mut ids = HashSet::new();
        for item in &view.items {
            assert!(ids.insert(item.id), "an invitation twice");
        }
        if let Some(created) = &view.created {
            assert!(
                self.codes_issued
                    .iter()
                    .any(|code| code == created.code.expose()),
                "a code is shown that the server never issued"
            );
            CODES_SHOWN.fetch_add(1, Ordering::Relaxed);
        }
        let debug = format!("{view:?}");
        for code in &self.codes_issued {
            assert!(!debug.contains(code.as_str()), "the code is printed");
        }
        if core.people().include_deactivated {
            assert!(
                core.can_manage_workspace(),
                "deactivated people asked for by a non-manager"
            );
        }
    }

    /// Once everything is delivered and answered, the workspace, the roles, and who is
    /// deactivated are the server's, and no code has been kept anywhere.
    fn check_admin_final(&self) {
        let core = &self.driver.core;
        assert!(!self.me_deactivated);
        let info = core.primary_workspace().unwrap_or_else(|| {
            panic!(
                "the person is back but the workspace is not\n{}",
                self.trace.join("\n")
            )
        });
        assert_eq!(info.workspace, self.server_workspace, "the workspace");
        assert_eq!(info.role, self.my_role, "the person's role");
        assert_eq!(core.workspace_settings(), self.server_workspace.settings);
        assert_eq!(core.access_lost(), None);
        let other = core
            .member(self.world.other.id)
            .expect("the other person is known");
        assert_eq!(other.deactivated, self.other_deactivated, "deactivation");
        assert_eq!(
            core.members().iter().any(|user| user.id == other.id),
            !self.other_deactivated
        );
        let view = core.invitations();
        if view.open && view.status == LoadStatus::Ready {
            assert_eq!(
                view.items,
                self.invitations,
                "the invitations are not the server's\n{}",
                self.trace.join("\n")
            );
            if !view.items.is_empty() {
                INVITATION_LISTS_AT_THE_END.fetch_add(1, Ordering::Relaxed);
            }
        }
        // (A list or profile whose refresh failed keeps what it had, and says so.)
        let profile_ready = core
            .profile()
            .is_some_and(|profile| profile.status == LoadStatus::Ready);
        if let Some(person) = core
            .profile()
            .and_then(|profile| profile.person.as_ref())
            .filter(|_| profile_ready)
        {
            let held = self.person_model(person.user.id).expect("a known person");
            assert_eq!(person.role, held.role, "a profile's role");
            assert_eq!(
                person.user.deactivated, held.user.deactivated,
                "a profile's state"
            );
        }
        let people_ready = core.people().open && core.people().status == LoadStatus::Ready;
        for person in core.people().people.iter().filter(|_| people_ready) {
            let held = self.person_model(person.user.id).expect("a known person");
            assert_eq!(
                person.role,
                held.role,
                "a directory entry's role\n{}",
                self.trace.join("\n")
            );
            assert_eq!(
                person.user.deactivated,
                held.user.deactivated,
                "a directory entry's state\n{}",
                self.trace.join("\n")
            );
            if person.user.deactivated {
                DEACTIVATED_LISTED_FOR_ADMINS.fetch_add(1, Ordering::Relaxed);
            }
        }
        // The code is in no file the app would have written.
        if !self.codes_issued.is_empty() {
            for cache in self.disk.iter().flatten() {
                let json = serde_json::to_string(cache).unwrap();
                for code in &self.codes_issued {
                    assert!(
                        !json.contains(code.as_str()),
                        "a cache holds an invitation code"
                    );
                }
            }
        }
    }

    fn check_discovery_final(&self) {
        let core = &self.driver.core;
        for (id, message) in &self.versions {
            if !message.deleted {
                assert_eq!(
                    core.is_saved(*id),
                    self.server_saved.contains_key(id),
                    "a saved marker is not the server's\n{}",
                    self.trace.join("\n")
                );
            }
        }
        let saved = core.saved();
        if saved.open && saved.status == LoadStatus::Ready {
            let server = self.saved_items();
            let shown: Vec<MessageId> = saved.items.iter().map(|i| i.message.id).collect();
            let expected: Vec<MessageId> = server
                .iter()
                .take(shown.len())
                .map(|i| i.message.id)
                .collect();
            assert_eq!(
                shown,
                expected,
                "the saved list is not the server's\n{}",
                self.trace.join("\n")
            );
            if !saved.has_more {
                assert_eq!(shown.len(), server.len(), "the saved list lacks items");
            }
            for (item, held) in saved.items.iter().zip(&server) {
                assert_eq!(item.message.revision, held.message.revision);
            }
            if !shown.is_empty() {
                SAVED_LISTS_AT_THE_END.fetch_add(1, Ordering::Relaxed);
            }
        }
        let people = core.people();
        if people.open && people.status == LoadStatus::Ready {
            let server = self.people_model(&people.query, people.include_deactivated);
            let shown: Vec<UserId> = people.people.iter().map(|p| p.user.id).collect();
            let expected: Vec<UserId> =
                server.iter().take(shown.len()).map(|p| p.user.id).collect();
            assert_eq!(
                shown,
                expected,
                "the people directory is not the server's\n{}",
                self.trace.join("\n")
            );
            if !people.has_more {
                assert_eq!(shown.len(), server.len());
            }
            for (person, held) in people.people.iter().zip(&server) {
                // (A copy in the directory is raw: an ended status is hidden by the screen, which
                // asks `status_of`, not by the copy.)
                let visible = |status: &Option<UserStatus>| {
                    status.clone().filter(|s| s.is_active(self.sim_now()))
                };
                assert_eq!(visible(&person.user.status), visible(&held.user.status));
            }
        }
        let browse = core.browse();
        if browse.status == LoadStatus::Ready {
            let server = self.browse_model(&BrowseChannelsQuery {
                query: Some(browse.query.clone()),
                joined: browse.joined,
                include_archived: None,
                sort: Some(browse.sort),
                offset: None,
                limit: None,
            });
            let shown: Vec<ConversationId> =
                browse.channels.iter().map(|c| c.conversation.id).collect();
            let expected: Vec<ConversationId> = server
                .iter()
                .take(shown.len())
                .map(|c| c.conversation.id)
                .collect();
            assert_eq!(
                shown,
                expected,
                "the channel directory is not the server's\n{}",
                self.trace.join("\n")
            );
            if !browse.has_more {
                assert_eq!(shown.len(), server.len());
            }
            for (entry, held) in browse.channels.iter().zip(&server) {
                assert_eq!(entry.joined, held.joined);
            }
        }
        if let Some(profile) = core.profile() {
            // (A person known from the directory is shown even if the profile could not be
            // fetched.)
            assert!(profile.status != LoadStatus::Ready || profile.person.is_some());
            if let Some(person) = &profile.person {
                assert_eq!(person.user.id, profile.user_id);
            }
        }
    }

    fn check_final(&self) {
        self.check_admin_final();
        self.check_discovery_final();
        self.check_notifications_final();
        self.check_search_final();
        assert_eq!(
            self.driver.core.pending_reaction_changes(),
            0,
            "reaction changes left unsaved\n{}",
            self.trace.join("\n")
        );
        assert_eq!(
            self.driver.core.unsettled_operations(),
            0,
            "operations left unsettled\n{}",
            self.trace.join("\n")
        );
        for operation in &self.created_ops {
            assert_eq!(
                self.settle_counts.get(operation).copied().unwrap_or(0),
                1,
                "operation {operation} must be settled exactly once\n{}",
                self.trace.join("\n")
            );
        }
        // Every message that was sent with files carries exactly those files, in order.
        for (operation, message) in &self.committed {
            if let Some(files) = self.files_by_op.get(operation) {
                assert_eq!(
                    &message
                        .attachments
                        .iter()
                        .map(|file| file.id)
                        .collect::<Vec<_>>(),
                    files,
                    "message {} does not carry the files it was sent with\n{}",
                    message.seq,
                    self.trace.join("\n")
                );
            }
        }
        // The client's emoji are the server's, with no image held for one that is gone and no
        // deleted emoji among the recent ones it shows.
        let mut server = self.emoji_as_of(self.log.len());
        server.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        assert_eq!(
            self.driver.core.custom_emoji(),
            server.as_slice(),
            "the custom emoji list\n{}",
            self.trace.join("\n")
        );
        for emoji in &server {
            let image = self.driver.core.emoji_image(emoji.id);
            assert_ne!(
                image,
                Some(&ImageState::Loading),
                "an emoji image is loading"
            );
            assert!(
                !(emoji.deleted && image.is_some()),
                "an image is held for a deleted emoji\n{}",
                self.trace.join("\n")
            );
        }
        assert!(
            self.emoji_in_flight.is_empty(),
            "an emoji image is still being fetched"
        );
        for recent in &self.driver.core.emoji_prefs().recent {
            if let Some(id) = recent.custom_id() {
                assert!(
                    server.iter().any(|e| e.id == id && !e.deleted),
                    "a recent emoji is not live\n{}",
                    self.trace.join("\n")
                );
            }
        }
        // Nothing is left in the outbox once every message is sent or discarded and the
        // composers are empty, and nothing is still being transferred.
        assert!(
            self.outbox.is_empty(),
            "copies left in the outbox: {:?}\n{}",
            self.outbox,
            self.trace.join("\n")
        );
        assert!(
            self.uploads_running.is_empty(),
            "an upload is still running"
        );
        assert!(
            self.downloads_running.is_empty(),
            "a download is still running"
        );
        assert!(
            self.images_in_flight.is_empty(),
            "an image is still being fetched"
        );
        // No preview is held for the files of a message that is gone, and none is left waiting.
        let gone: HashSet<AttachmentId> = self
            .versions
            .values()
            .filter(|message| message.deleted)
            .flat_map(|message| message.attachments.iter().map(|file| file.id))
            .collect();
        for id in &self.requested_images {
            assert_ne!(
                self.driver.core.thumbnail(*id),
                Some(&ImageState::Loading),
                "a thumbnail is still loading\n{}",
                self.trace.join("\n")
            );
            assert_ne!(
                self.driver.core.full_image(*id),
                Some(&ImageState::Loading),
                "an image is still loading\n{}",
                self.trace.join("\n")
            );
            if gone.contains(id) {
                assert!(
                    self.driver.core.thumbnail(*id).is_none()
                        && self.driver.core.full_image(*id).is_none(),
                    "a preview is held for the file of a deleted message\n{}",
                    self.trace.join("\n")
                );
            }
            if let Some(state) = self.driver.core.download(*id) {
                assert!(
                    !matches!(state, lazy_client_core::DownloadState::Downloading { .. }),
                    "a download is still running"
                );
            }
        }
        // For one sender, messages are committed in the order they were submitted. (Messages the
        // client gave up on may be retried later and then legitimately land behind newer ones.)
        for conversation in [self.world.channel.id, self.world.second_channel.id] {
            let mut committed: Vec<_> = self
                .committed
                .iter()
                .filter(|(operation, message)| {
                    message.conversation_id == conversation
                        && !self.given_up.contains(operation)
                        && !(self.reply_ops.contains(operation)
                            && self.disturbed.contains(operation))
                })
                .map(|(operation, message)| (message.seq, self.submission_number[operation]))
                .collect();
            committed.sort();
            let numbers: Vec<usize> = committed.iter().map(|(_, number)| *number).collect();
            assert!(
                numbers.windows(2).all(|w| w[0] < w[1]),
                "committed out of submission order: {numbers:?}\n{}",
                self.trace.join("\n")
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
            let deleted = self.deleted_ids();
            let me = self.world.me.id;
            let mentioning = self
                .log
                .iter()
                .filter_map(LogEntry::message)
                .filter(|m| {
                    m.conversation_id == conversation
                        && is_listed(m)
                        && m.author_id != me
                        && m.seq.get() > server_read
                        && !deleted.contains(&m.id)
                        && self.versions[&m.id].mentions.includes(me)
                })
                .count();
            if mentioning > 0 {
                FINAL_MENTION_COUNTS_ABOVE_ZERO.fetch_add(1, Ordering::Relaxed);
            }
            assert_eq!(
                state.mention_count as usize,
                mentioning,
                "mention count\n{}",
                self.trace.join("\n")
            );
            // Whoever is in a channel, as far as the core knows, is whoever is in it.
            if let Some(known) = self.driver.core.conversation_members(conversation) {
                let mut known = known.to_vec();
                let mut server = self.server_members(conversation, self.log.len());
                known.sort();
                server.sort();
                assert_eq!(known, server, "channel members\n{}", self.trace.join("\n"));
            }
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
                            && !row.deleted
                            && row.seq().is_some_and(|s| s.get() > server_read)
                    })
                    .count();
                assert_eq!(state.unread_count as usize, loaded, "unread vs loaded rows");
            }
        }
        // The person's thread states are the server's.
        let mut roots: Vec<MessageId> = self.thread_records.keys().copied().collect();
        roots.sort();
        for root in roots {
            let server = self.thread_state_at(root, i64::MAX);
            match self.driver.core.thread_state(root) {
                Some(held) => {
                    assert_eq!(held, &server, "thread state\n{}", self.trace.join("\n"));
                    if server.unread_count > 0 {
                        THREAD_STATES_WITH_UNREAD_AT_THE_END.fetch_add(1, Ordering::Relaxed);
                    }
                    if server.mention_count > 0 {
                        THREAD_MENTION_COUNTS_AT_THE_END.fetch_add(1, Ordering::Relaxed);
                    }
                }
                // A thread the person does not follow can gain unread replies without any event
                // reaching a client that holds no state for it, and nothing shows those counts.
                None => assert!(
                    !server.following,
                    "the client holds no state for a thread the person follows: {server:?}\n{}",
                    self.trace.join("\n")
                ),
            }
        }
        for root in self
            .versions
            .values()
            .filter(|m| m.thread_root.is_none())
            .map(|m| m.id)
        {
            if self.driver.core.thread_state(root).is_some() {
                // The server drops the state of a thread that ended without telling anyone; a
                // client that had no way to know it ended (the root was not on screen) keeps it.
                assert!(
                    self.thread_records.contains_key(&root) || self.purged.contains(&root),
                    "a thread state with no record on the server\n{}",
                    self.trace.join("\n")
                );
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
                assert_eq!(row.edit, EditState::None, "an edit is still outstanding");
                // Every message on screen is the server's latest version of a message that
                // still exists.
                if let Some(id) = row.message_id() {
                    let server = &self.versions[&id];
                    if row.deleted {
                        // A placeholder: a deleted root whose replies remain.
                        assert!(
                            server.deleted
                                && server.thread_root.is_none()
                                && server.thread.is_some(),
                            "a placeholder for message {} that is not a deleted root with replies\n{}",
                            server.seq,
                            self.trace.join("\n")
                        );
                        assert_eq!(
                            row.thread,
                            server.thread,
                            "the summary of a placeholder\n{}",
                            self.trace.join("\n")
                        );
                        PLACEHOLDERS_AT_THE_END.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    assert!(
                        !server.deleted,
                        "deleted message {} is still on screen\n{}",
                        server.seq,
                        self.trace.join("\n")
                    );
                    assert_eq!(
                        (row.thread_root, row.in_channel),
                        (server.thread_root, server.in_channel)
                    );
                    if server.thread_root.is_none() {
                        assert_eq!(
                            row.thread,
                            server.thread,
                            "the thread summary of message {}\n{}",
                            server.seq,
                            self.trace.join("\n")
                        );
                        if server.thread.is_some() {
                            ROOTS_WITH_SUMMARIES_AT_THE_END.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if !server.reactions.is_empty() {
                        ROWS_WITH_REACTIONS_AT_THE_END.fetch_add(1, Ordering::Relaxed);
                    }
                    assert_eq!(
                        (&row.reactions, row.reactions_as_of),
                        (&server.reactions, server.reactions_as_of),
                        "message {} does not show the server's reactions\n{}",
                        server.seq,
                        self.trace.join("\n")
                    );
                    assert_eq!(
                        (&row.document, row.revision(), row.edited_at),
                        (&server.document, Some(server.revision), server.edited_at),
                        "message {} is not at the server's latest revision\n{}",
                        server.seq,
                        self.trace.join("\n")
                    );
                    assert_eq!(
                        (&row.attachments, row.upload.is_none()),
                        (&server.attachments, true),
                        "message {} does not show exactly its files\n{}",
                        server.seq,
                        self.trace.join("\n")
                    );
                }
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
            let server = &self.versions[&message.id];
            // The history lists top-level messages, replies also sent to the channel, and deleted
            // roots that still have replies.
            let expected =
                !server.deleted || (server.thread_root.is_none() && server.thread.is_some());
            if !is_listed(message) || !expected {
                continue;
            }
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
            if loaded && !self.versions[&message.id].deleted && is_listed(message) {
                assert!(present[&message.conversation_id].contains(&message.id));
            }
        }
    }
}

/// The start of a value's `Debug` text, at most `limit` characters: formatting stops once there
/// are enough, which keeps a trace of big values cheap.
fn debug_capped(value: &impl std::fmt::Debug, limit: usize) -> String {
    struct Capped {
        text: String,
        limit: usize,
    }
    impl std::fmt::Write for Capped {
        fn write_str(&mut self, piece: &str) -> std::fmt::Result {
            let room = self.limit - self.text.chars().count();
            self.text.extend(piece.chars().take(room));
            if piece.chars().count() >= room {
                Err(std::fmt::Error)
            } else {
                Ok(())
            }
        }
    }
    let mut capped = Capped {
        text: String::new(),
        limit,
    };
    let _ = std::fmt::write(&mut capped, format_args!("{value:?}"));
    capped.text
}

/// The position of the newest reply of a thread in the inbox.
fn inbox_activity(entry: &InboxEntry) -> i64 {
    entry
        .root
        .thread
        .as_ref()
        .map_or(0, |thread| thread.last_reply_seq.get())
}

/// The words of a text, lowercase, with the character where each begins: what the simulated
/// server indexes.
fn words_of(text: &str) -> Vec<(usize, String)> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut start = 0;
    for (index, c) in text.chars().enumerate() {
        if c.is_alphanumeric() {
            if current.is_empty() {
                start = index;
            }
            current.extend(c.to_lowercase());
        } else if !current.is_empty() {
            words.push((start, std::mem::take(&mut current)));
        }
    }
    if !current.is_empty() {
        words.push((start, current));
    }
    words
}

/// The number a message's words begin with: `12` in `12 :blob:`.
fn leading_number(text: &str) -> Option<usize> {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// How many cases the default run makes, and the seed it makes them from. The seed was chosen so
/// that this run reaches every counter below with a wide margin (see the figures printed by
/// `cargo test -p lazy-client-core --test simulation timelines_stay -- --nocapture`); change the
/// weights of the actions and it must be chosen again. With the weights as they are, the rarest
/// counters in this run are 2 edits committed, 2 notifications clicked, 4 restarts with files in
/// the journal, 5 thread reads answered, 6 answers to uploads that were cancelled, 6 notifications
/// retracted, and 6 restarts with recent searches; each is asserted to be at least 1.
const DEFAULT_CASES: u32 = 1500;
const DEFAULT_SEED: [u8; 32] = *b"lazy-simulation-default-seed-v50";

/// How often the random runs exercised the paths this test exists for.
static RESTARTS_WITH_CACHED_MESSAGES: AtomicUsize = AtomicUsize::new(0);
static RESTARTS_WITHOUT_CACHE: AtomicUsize = AtomicUsize::new(0);
static CACHE_WRITES_CHECKED: AtomicUsize = AtomicUsize::new(0);
static MARK_READS_ANSWERED: AtomicUsize = AtomicUsize::new(0);
static READ_STATE_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
/// Edits the server committed for the person, and ones it refused as stale.
static EDITS_COMMITTED: AtomicUsize = AtomicUsize::new(0);
static EDITS_REFUSED_AS_STALE: AtomicUsize = AtomicUsize::new(0);
/// Events delivered as tombstones (the earlier events of a deleted message), and deletions
/// delivered for a message the client had on screen.
static TOMBSTONE_CREATIONS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static TOMBSTONE_EDITS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static DELETIONS_OF_HELD_MESSAGES: AtomicUsize = AtomicUsize::new(0);
static DELETIONS_OF_UNSEEN_MESSAGES: AtomicUsize = AtomicUsize::new(0);
/// Reaction changes the server committed for the person, reaction events delivered, requests the
/// person's client had to repeat after a lost answer, and rows that ended with reactions.
static REACTIONS_SET_FOR_THE_PERSON: AtomicUsize = AtomicUsize::new(0);
static REACTION_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static REACTION_REQUESTS_LOST: AtomicUsize = AtomicUsize::new(0);
static ROWS_WITH_REACTIONS_AT_THE_END: AtomicUsize = AtomicUsize::new(0);
/// Mentions of the person: messages delivered that mention them, edits that add or remove them,
/// deletions of messages that mentioned them, final counts above zero, and member-list traffic.
static MENTIONS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static MENTION_EDITS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static MENTION_DELETIONS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static FINAL_MENTION_COUNTS_ABOVE_ZERO: AtomicUsize = AtomicUsize::new(0);
static MEMBER_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static MEMBER_LISTS_FETCHED: AtomicUsize = AtomicUsize::new(0);
/// Threads: replies and thread events delivered, requests answered, and what ended up on screen.
static REPLIES_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static THREAD_CHANGES_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static THREAD_STATE_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static FOLLOW_REQUESTS_ANSWERED: AtomicUsize = AtomicUsize::new(0);
static THREAD_READS_ANSWERED: AtomicUsize = AtomicUsize::new(0);
static REPLIES_SENT_BY_THE_PERSON: AtomicUsize = AtomicUsize::new(0);
static THREADS_OPENED: AtomicUsize = AtomicUsize::new(0);
static PLACEHOLDERS_AT_THE_END: AtomicUsize = AtomicUsize::new(0);
static ROOTS_WITH_SUMMARIES_AT_THE_END: AtomicUsize = AtomicUsize::new(0);
static REPLY_ROWS_AT_THE_END: AtomicUsize = AtomicUsize::new(0);
static SAVES_ASKED: AtomicUsize = AtomicUsize::new(0);
static SAVE_REQUESTS_ANSWERED: AtomicUsize = AtomicUsize::new(0);
static SAVED_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static SAVED_PAGES_SHOWN: AtomicUsize = AtomicUsize::new(0);
static SAVED_LISTS_AT_THE_END: AtomicUsize = AtomicUsize::new(0);
static PEOPLE_PAGES_SHOWN: AtomicUsize = AtomicUsize::new(0);
static PROFILES_SHOWN: AtomicUsize = AtomicUsize::new(0);
static BROWSE_PAGES_SHOWN: AtomicUsize = AtomicUsize::new(0);
static STALE_DISCOVERY_ANSWERS: AtomicUsize = AtomicUsize::new(0);
static QUICK_SWITCHES_CHOSEN: AtomicUsize = AtomicUsize::new(0);
static RESTARTS_WITH_RECENT_CONVERSATIONS: AtomicUsize = AtomicUsize::new(0);
static NOTIFICATIONS_SENT: AtomicUsize = AtomicUsize::new(0);
static NOTIFICATIONS_PAUSED: AtomicUsize = AtomicUsize::new(0);
static NOTIFICATIONS_SUPPRESSED_BY_FOCUS: AtomicUsize = AtomicUsize::new(0);
static NOTIFICATIONS_CLEARED: AtomicUsize = AtomicUsize::new(0);
static NOTIFICATIONS_RETRACTED: AtomicUsize = AtomicUsize::new(0);
static NOTIFICATIONS_CLICKED: AtomicUsize = AtomicUsize::new(0);
static BADGES_ANNOUNCED: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_SAVES_ANSWERED: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static STATUS_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static INBOX_OPENED: AtomicUsize = AtomicUsize::new(0);
static INBOX_PAGES_SHOWN: AtomicUsize = AtomicUsize::new(0);
static STALE_INBOX_ANSWERS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static INBOX_MARKED_READ: AtomicUsize = AtomicUsize::new(0);
static INBOX_ENTRIES_AT_THE_END: AtomicUsize = AtomicUsize::new(0);
static SEARCH_RUNS_ASKED: AtomicUsize = AtomicUsize::new(0);
static SEARCH_PAGES_ASKED: AtomicUsize = AtomicUsize::new(0);
static SEARCH_ANSWERS_SHOWN: AtomicUsize = AtomicUsize::new(0);
static SEARCH_PAGES_APPENDED: AtomicUsize = AtomicUsize::new(0);
static STALE_SEARCH_ANSWERS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static SEARCH_FAILURES_SHOWN: AtomicUsize = AtomicUsize::new(0);
static SEARCHES_CANCELLED: AtomicUsize = AtomicUsize::new(0);
static SEARCH_HITS_OPENED: AtomicUsize = AtomicUsize::new(0);
static JUMPS_REACHED: AtomicUsize = AtomicUsize::new(0);
static HITS_REMOVED_BY_DELETION: AtomicUsize = AtomicUsize::new(0);
static HITS_UPDATED_BY_EDIT: AtomicUsize = AtomicUsize::new(0);
static RECENT_SEARCHES_SAVED: AtomicUsize = AtomicUsize::new(0);
static RESTARTS_WITH_RECENT_SEARCHES: AtomicUsize = AtomicUsize::new(0);
static SEARCH_HITS_ON_SCREEN_AT_THE_END: AtomicUsize = AtomicUsize::new(0);
static EMOJI_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static EMOJI_RENAMES: AtomicUsize = AtomicUsize::new(0);
static EMOJI_DELETIONS: AtomicUsize = AtomicUsize::new(0);
static EMOJI_MESSAGES_COMMITTED: AtomicUsize = AtomicUsize::new(0);
static EMOJI_USES_REFUSED: AtomicUsize = AtomicUsize::new(0);
static EMOJI_IMAGES_LOADED: AtomicUsize = AtomicUsize::new(0);
static CUSTOM_REACTIONS_SET: AtomicUsize = AtomicUsize::new(0);
static FILES_UPLOADED: AtomicUsize = AtomicUsize::new(0);
static MESSAGES_WITH_FILES_COMMITTED: AtomicUsize = AtomicUsize::new(0);
static FILE_ONLY_MESSAGES_COMMITTED: AtomicUsize = AtomicUsize::new(0);
static STALE_UPLOAD_RESULTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static FAILED_FILE_ROWS_RETRIED: AtomicUsize = AtomicUsize::new(0);
static RESTARTS_WITH_FILES_IN_THE_JOURNAL: AtomicUsize = AtomicUsize::new(0);
static TOGGLES_ON_FULL_MESSAGES: AtomicUsize = AtomicUsize::new(0);
static SWAPS_ON_FULL_MESSAGES: AtomicUsize = AtomicUsize::new(0);
static HOLD_CHECKS_MADE: AtomicUsize = AtomicUsize::new(0);
static REACTIONS_REFUSED_AT_THE_LIMIT: AtomicUsize = AtomicUsize::new(0);
static RESTORED_SELECTIONS_CHECKED: AtomicUsize = AtomicUsize::new(0);
static THREAD_STATES_WITH_UNREAD_AT_THE_END: AtomicUsize = AtomicUsize::new(0);
static THREAD_MENTION_COUNTS_AT_THE_END: AtomicUsize = AtomicUsize::new(0);
static WORKSPACE_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static ROLE_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static DEACTIVATION_EVENTS_DELIVERED: AtomicUsize = AtomicUsize::new(0);
static ADMIN_CHANGES_ANSWERED: AtomicUsize = AtomicUsize::new(0);
static ADMIN_REFUSALS: AtomicUsize = AtomicUsize::new(0);
static LAST_OWNER_REFUSALS: AtomicUsize = AtomicUsize::new(0);
static INVITATION_LISTS_SHOWN: AtomicUsize = AtomicUsize::new(0);
static INVITATION_LISTS_AT_THE_END: AtomicUsize = AtomicUsize::new(0);
static INVITATION_CREATES_LOST: AtomicUsize = AtomicUsize::new(0);
static STALE_ADMIN_ANSWERS: AtomicUsize = AtomicUsize::new(0);
static CODES_SHOWN: AtomicUsize = AtomicUsize::new(0);
static ACCESS_LOSSES: AtomicUsize = AtomicUsize::new(0);
static ACCESS_LOSSES_SHOWN: AtomicUsize = AtomicUsize::new(0);
static SUBSCRIPTIONS_ENDED: AtomicUsize = AtomicUsize::new(0);
static DEACTIVATED_HIDDEN_FROM_PICKERS: AtomicUsize = AtomicUsize::new(0);
static DEACTIVATED_LISTED_FOR_ADMINS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn timelines_stay_consistent_under_any_interleaving() {
    // The default run (no `PROPTEST_CASES`) is deterministic: a fixed seed, so the same 1000
    // cases run every time and the counters below, which the test insists were reached, are
    // reached by the same cases every time. A long run (`PROPTEST_CASES=120000`) uses a random
    // seed, to keep looking for what the fixed cases do not find, and insists on the same
    // counters over its larger sample.
    let (cases, mut runner) = match std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        Some(cases) => (
            cases,
            TestRunner::new(ProptestConfig {
                cases,
                ..ProptestConfig::default()
            }),
        ),
        None => (
            DEFAULT_CASES,
            TestRunner::new_with_rng(
                ProptestConfig {
                    cases: DEFAULT_CASES,
                    ..ProptestConfig::default()
                },
                TestRng::from_seed(RngAlgorithm::ChaCha, &DEFAULT_SEED),
            ),
        ),
    };
    let _ = cases;
    runner
        .run(&prop::collection::vec(action(), 1..120), |actions| {
            let mut sim = Sim::new();
            for action in actions {
                sim.step(action);
            }
            sim.drain();
            sim.verify_threads();
            sim.check_final();
            sim.signs_out();
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
    let mut missing = Vec::new();
    for (name, counter) in [
        ("edits committed", &EDITS_COMMITTED),
        ("edits refused as stale", &EDITS_REFUSED_AS_STALE),
        (
            "tombstone creations delivered",
            &TOMBSTONE_CREATIONS_DELIVERED,
        ),
        ("tombstone edits delivered", &TOMBSTONE_EDITS_DELIVERED),
        ("deletions of held messages", &DELETIONS_OF_HELD_MESSAGES),
        (
            "deletions of unseen messages",
            &DELETIONS_OF_UNSEEN_MESSAGES,
        ),
        (
            "reaction changes made for the person",
            &REACTIONS_SET_FOR_THE_PERSON,
        ),
        ("reaction events delivered", &REACTION_EVENTS_DELIVERED),
        ("reaction requests lost", &REACTION_REQUESTS_LOST),
        (
            "rows with reactions at the end",
            &ROWS_WITH_REACTIONS_AT_THE_END,
        ),
        ("messages that mention the person", &MENTIONS_DELIVERED),
        (
            "edits that add or remove the person",
            &MENTION_EDITS_DELIVERED,
        ),
        (
            "deletions of messages that mentioned the person",
            &MENTION_DELETIONS_DELIVERED,
        ),
        (
            "final mention counts above zero",
            &FINAL_MENTION_COUNTS_ABOVE_ZERO,
        ),
        ("member events delivered", &MEMBER_EVENTS_DELIVERED),
        ("member lists fetched", &MEMBER_LISTS_FETCHED),
        ("replies delivered", &REPLIES_DELIVERED),
        ("thread changes delivered", &THREAD_CHANGES_DELIVERED),
        (
            "thread state events delivered",
            &THREAD_STATE_EVENTS_DELIVERED,
        ),
        ("follow requests answered", &FOLLOW_REQUESTS_ANSWERED),
        ("thread reads answered", &THREAD_READS_ANSWERED),
        ("replies sent by the person", &REPLIES_SENT_BY_THE_PERSON),
        ("threads opened", &THREADS_OPENED),
        ("placeholders at the end", &PLACEHOLDERS_AT_THE_END),
        (
            "roots with summaries at the end",
            &ROOTS_WITH_SUMMARIES_AT_THE_END,
        ),
        ("reply rows at the end", &REPLY_ROWS_AT_THE_END),
        (
            "withdraw-and-add swaps on full messages",
            &SWAPS_ON_FULL_MESSAGES,
        ),
        (
            "restored selections checked after a restart",
            &RESTORED_SELECTIONS_CHECKED,
        ),
        ("custom emoji events delivered", &EMOJI_EVENTS_DELIVERED),
        ("custom emoji renamed", &EMOJI_RENAMES),
        ("custom emoji deleted", &EMOJI_DELETIONS),
        (
            "messages with custom emoji committed",
            &EMOJI_MESSAGES_COMMITTED,
        ),
        ("uses of a gone emoji refused", &EMOJI_USES_REFUSED),
        ("emoji images loaded", &EMOJI_IMAGES_LOADED),
        (
            "custom reactions made for the person",
            &CUSTOM_REACTIONS_SET,
        ),
        ("files uploaded", &FILES_UPLOADED),
        (
            "messages with files committed",
            &MESSAGES_WITH_FILES_COMMITTED,
        ),
        (
            "messages of only files committed",
            &FILE_ONLY_MESSAGES_COMMITTED,
        ),
        (
            "answers to uploads that were cancelled",
            &STALE_UPLOAD_RESULTS_DELIVERED,
        ),
        (
            "failed messages with files tried again",
            &FAILED_FILE_ROWS_RETRIED,
        ),
        (
            "restarts with files in the journal",
            &RESTARTS_WITH_FILES_IN_THE_JOURNAL,
        ),
        (
            "thread states with unread replies at the end",
            &THREAD_STATES_WITH_UNREAD_AT_THE_END,
        ),
        (
            "thread mention counts above zero at the end",
            &THREAD_MENTION_COUNTS_AT_THE_END,
        ),
        ("searches asked", &SEARCH_RUNS_ASKED),
        ("more pages of a search asked", &SEARCH_PAGES_ASKED),
        ("search answers shown", &SEARCH_ANSWERS_SHOWN),
        ("search pages appended", &SEARCH_PAGES_APPENDED),
        (
            "answers to searches the core had moved on from",
            &STALE_SEARCH_ANSWERS_DELIVERED,
        ),
        ("search failures shown", &SEARCH_FAILURES_SHOWN),
        ("searches cancelled", &SEARCHES_CANCELLED),
        ("hits opened", &SEARCH_HITS_OPENED),
        ("jumps reached from a hit", &JUMPS_REACHED),
        ("hits removed by a deletion", &HITS_REMOVED_BY_DELETION),
        ("saves asked", &SAVES_ASKED),
        ("save requests answered", &SAVE_REQUESTS_ANSWERED),
        ("saved events delivered", &SAVED_EVENTS_DELIVERED),
        ("saved pages shown", &SAVED_PAGES_SHOWN),
        ("saved lists with items at the end", &SAVED_LISTS_AT_THE_END),
        ("people pages shown", &PEOPLE_PAGES_SHOWN),
        ("profiles shown", &PROFILES_SHOWN),
        ("directory pages shown", &BROWSE_PAGES_SHOWN),
        (
            "directory, people, profile, and saved answers for requests the core had moved on from",
            &STALE_DISCOVERY_ANSWERS,
        ),
        ("quick switcher results chosen", &QUICK_SWITCHES_CHOSEN),
        ("workspace events delivered", &WORKSPACE_EVENTS_DELIVERED),
        ("role events delivered", &ROLE_EVENTS_DELIVERED),
        (
            "deactivation events delivered",
            &DEACTIVATION_EVENTS_DELIVERED,
        ),
        ("administration changes answered", &ADMIN_CHANGES_ANSWERED),
        ("administration changes refused", &ADMIN_REFUSALS),
        ("last-owner refusals", &LAST_OWNER_REFUSALS),
        ("invitation lists shown", &INVITATION_LISTS_SHOWN),
        ("invitation lists at the end", &INVITATION_LISTS_AT_THE_END),
        (
            "invitation creations with a lost answer",
            &INVITATION_CREATES_LOST,
        ),
        (
            "invitation answers for requests the core had moved on from",
            &STALE_ADMIN_ANSWERS,
        ),
        ("codes shown", &CODES_SHOWN),
        ("accesses lost", &ACCESS_LOSSES),
        ("accesses lost and shown", &ACCESS_LOSSES_SHOWN),
        ("subscriptions ended", &SUBSCRIPTIONS_ENDED),
        (
            "deactivated people hidden from pickers",
            &DEACTIVATED_HIDDEN_FROM_PICKERS,
        ),
        (
            "deactivated people listed for admins",
            &DEACTIVATED_LISTED_FOR_ADMINS,
        ),
        (
            "restarts with recent conversations",
            &RESTARTS_WITH_RECENT_CONVERSATIONS,
        ),
        ("desktop notifications sent", &NOTIFICATIONS_SENT),
        ("notifications held back by a pause", &NOTIFICATIONS_PAUSED),
        (
            "notifications held back by looking at the conversation",
            &NOTIFICATIONS_SUPPRESSED_BY_FOCUS,
        ),
        ("notifications cleared", &NOTIFICATIONS_CLEARED),
        ("notifications retracted", &NOTIFICATIONS_RETRACTED),
        ("notifications clicked", &NOTIFICATIONS_CLICKED),
        ("badges announced", &BADGES_ANNOUNCED),
        ("settings saves answered", &SETTINGS_SAVES_ANSWERED),
        ("settings events delivered", &SETTINGS_EVENTS_DELIVERED),
        ("status events delivered", &STATUS_EVENTS_DELIVERED),
        ("inboxes opened", &INBOX_OPENED),
        ("inbox pages shown", &INBOX_PAGES_SHOWN),
        (
            "inbox answers for requests the core had moved on from",
            &STALE_INBOX_ANSWERS_DELIVERED,
        ),
        ("inbox mark-reads answered", &INBOX_MARKED_READ),
        ("inboxes with threads at the end", &INBOX_ENTRIES_AT_THE_END),
        ("hits updated by an edit", &HITS_UPDATED_BY_EDIT),
        ("recent searches saved", &RECENT_SEARCHES_SAVED),
        (
            "restarts with recent searches",
            &RESTARTS_WITH_RECENT_SEARCHES,
        ),
        (
            "searches with hits on screen at the end",
            &SEARCH_HITS_ON_SCREEN_AT_THE_END,
        ),
    ] {
        eprintln!(
            "simulation coverage: {} {name}",
            counter.load(Ordering::Relaxed)
        );
        missing.extend((counter.load(Ordering::Relaxed) == 0).then_some(name));
    }
    assert!(missing.is_empty(), "never exercised: {missing:?}");
    // Rarer paths: reported, but too few in a short run to demand.
    eprintln!(
        "simulation coverage: {} toggles on messages holding all they can, {} adds checked against a withdrawal in flight",
        TOGGLES_ON_FULL_MESSAGES.load(Ordering::Relaxed),
        HOLD_CHECKS_MADE.load(Ordering::Relaxed),
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

/// Found by the randomized run: an edit whose outcome is unknown, for a message that was then
/// deleted while the client was cut off by a snapshot. Nothing on screen can retry it, so it is
/// asked about again when the connection returns.
#[test]
fn regression_unknown_edit_of_a_message_deleted_behind_a_snapshot() {
    let mut sim = Sim::new();
    use Action::*;
    let steps = [
        UserSends(false),
        UserSends(false),
        Resolve(2512496696553363361, Outcome::Ok),
        Restart(0),
        Resolve(0, Outcome::Ok),
        UserSends(false),
        Resolve(12683036080724599933, Outcome::Ok),
        Resolve(0, Outcome::Ok),
        OtherEdits(0),
        OtherPosts(false),
        Resolve(1830363664218845161, Outcome::Ok),
        UserEdits(0),
        Resolve(0, Outcome::Ok),
        Resolve(0, Outcome::Ok),
        UserSends(false),
        OtherEdits(0),
        Resolve(98494218103356065, Outcome::Ok),
        UserDeletes(0),
        Resolve(12509970005774976695, Outcome::LostAnswer),
        Resolve(4654555628295666851, Outcome::LostAnswer),
        Snapshot,
        Resolve(8888612227510884800, Outcome::Ok),
        Resolve(3799685701215156361, Outcome::LostAnswer),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.check_final();
}

/// Found by the randomized run: the person's own edit commits and its event arrives while the
/// message's row is gone (a snapshot reset the timeline) and the first page is in flight. The
/// page was computed before the edit, so it must not bring the old revision back.
#[test]
fn regression_edit_event_for_a_message_whose_page_is_in_flight() {
    let mut sim = Sim::new();
    use Action::*;
    let steps = [
        Window(false),
        Resolve(0, Outcome::Ok),
        UserSends(true),
        Resolve(0, Outcome::Ok),
        Resolve(0, Outcome::Ok),
        Resolve(0, Outcome::Ok),
        UserEdits(0),
        Snapshot,
        Resolve(11203092791255224837, Outcome::Ok),
        Resolve(5635627865856582470, Outcome::Ok),
        Resolve(11554323638185764955, Outcome::LostAnswer),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.check_final();
}

/// Found by the randomized run: a mark-read answer computed after a deletion the client has not
/// heard of yet. Its count already leaves the message out, so the deletion event that follows
/// must not subtract it again (which also left more mentions than unread messages).
#[test]
fn regression_mark_read_answer_ahead_of_a_deletion_event() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        Resolve(7903509901207135867, Outcome::Ok),
        UserSends(false),
        Resolve(124786210649190062, Outcome::Ok),
        Deliver(1),
        Resolve(3040465422033572375, Outcome::Ok),
        Resolve(0, Outcome::Ok),
        Restart(269928742724572358),
        Resolve(12790236962398047, Outcome::Reject),
        UserSends(true),
        Resolve(9489899771400716, Outcome::Ok),
        OtherPosts(false),
        OtherPosts(false),
        OtherPosts(false),
        Deliver(1),
        Resolve(4176633440271649404, Outcome::Ok),
        UserSends(false),
        Deliver(1),
        UserReacts(0, 0),
        Resolve(7907335276532002625, Outcome::Ok),
        OtherMentions(true, 0),
        ServerDeletes(446877553587422736),
        Bottom(true, true),
        ThirdToggles(false),
        Deliver(2),
        Window(false),
        OtherPosts(false),
        OtherMentions(false, 0),
        OtherPosts(true),
        Resolve(8208553131247757709, Outcome::LostAnswer),
        OtherMentions(true, 4067286690987339478),
        Bottom(true, false),
        Deliver(3),
        ServerDeletes(820713654502695317),
        OtherPosts(false),
        Window(true),
        UserSends(false),
        Bottom(true, true),
        ServerDeletes(7097066105481965512),
        UserSends(false),
        Deliver(3),
        Resolve(5387654648423531573, Outcome::Ok),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.check_final();
}

/// Found by the randomized run: reading past a message that the bootstrap counted as mentioning
/// the person leaves nothing to say it did, so the count came from the rows of the timeline.
/// An edit that adds the person to a later message then made more mentions than unread messages.
#[test]
fn regression_reading_past_a_baseline_mention() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        UserSends(false),
        Resolve(12821840284898202418, Outcome::Ok),
        OtherMentions(false, 0),
        Deliver(1),
        OtherMentions(true, 154702135677734943),
        UserSends(false),
        Reconnect(0),
        Reconnect(0),
        OtherMentions(false, 0),
        OtherMentions(false, 0),
        Restart(3827898618728968155),
        Resolve(0, Outcome::Ok),
        OtherPosts(false),
        ServerDeletes(872814151375465282),
        OtherMentions(true, 204801668745558592),
        OtherMentions(false, 0),
        UserSends(false),
        OtherEdits(16231101454762749982),
        Bottom(true, true),
        Resolve(1038364137429977487, Outcome::Ok),
        Deliver(1),
        Deliver(2),
        Resolve(10401794339011740950, Outcome::Ok),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.check_final();
}

/// Found by the randomized run: a thread the person stopped following gains an unread reply
/// that no event announces to a client holding no state for it. That is not a divergence.
#[test]
fn regression_unfollowed_thread_gaining_an_unread_reply() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        OtherMentions(false, 0),
        OtherPosts(false),
        OtherReplies(9757229812128071094, false, 494606776087488878),
        OtherPosts(false),
        OtherDeviceFollows(2322499819127764350, false),
        OtherPosts(false),
        OtherMentions(false, 0),
        ServerDeletes(72094177097899322),
        ServerDeletes(7048200253818636),
        ThirdToggles(false),
        OtherPosts(false),
        OtherPosts(false),
        OtherMentions(false, 0),
        OtherMentions(false, 0),
        ThirdToggles(false),
        OtherMentions(false, 0),
        Restart(827788655502204098),
        OtherMentions(false, 0),
        ThirdToggles(false),
        ThirdToggles(false),
        Resolve(0, Outcome::Ok),
        OtherMentions(false, 0),
        OtherReplies(10139332359941938252, false, 1390805085289271302),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.verify_threads();
    sim.check_final();
}

/// Found by the randomized run: a root's summary was cleared by an event while the root's page
/// was still to come.
#[test]
fn regression_summary_cleared_before_the_page_arrives() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        OtherPosts(false),
        OtherMentions(false, 0),
        OtherPosts(true),
        Snapshot,
        Restart(0),
        Resolve(0, Outcome::Ok),
        OtherPosts(false),
        OtherReplies(6976875464335799934, false, 0),
        Deliver(3),
        Resolve(16615193060020820210, Outcome::LostAnswer),
        ServerDeletes(7030408673963224349),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.verify_threads();
    sim.check_final();
}

/// Found by the randomized run: the open thread's root kept a summary older than the thread's.
#[test]
fn regression_pane_root_summary_after_two_snapshots() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        OtherMentions(true, 0),
        OtherReplies(0, false, 0),
        OtherReplies(0, false, 0),
        ServerDeletes(3394806720155680),
        Restart(14999273568061956595),
        OtherPosts(false),
        Resolve(2660182470316263184, Outcome::Ok),
        UserSends(false),
        OtherPosts(false),
        Deliver(1),
        OpenThread(0),
        Snapshot,
        OtherReplies(2792392338083414484, false, 2277192672921835034),
        LoadOlderReplies,
        Resolve(15341316650268307798, Outcome::Ok),
        OtherMentions(false, 0),
        Resolve(165192441, Outcome::Ok),
        Snapshot,
        UserSends(false),
        OtherRepliesToOpen(false, 0),
        Deliver(1),
        OtherReplies(0, false, 0),
        UserReacts(0, 1743375226981074624),
        OtherReplies(0, false, 0),
        Resolve(448055382649970084, Outcome::Ok),
        OtherEdits(786106951428587985),
        UserSends(false),
        UserFollows(0, false),
        UserReacts(0, 841886687315828515),
        UserSends(false),
        OtherPosts(false),
        Resolve(6284677470846908749, Outcome::Ok),
        OtherEdits(736893224411),
        OtherPosts(false),
        UserEdits(0),
        LoadOlder(false),
        Deliver(1),
        OtherEdits(9920834330430),
        OtherPosts(false),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.verify_threads();
    sim.check_final();
}

/// Found by a random run: a reply with files was refused because the thread was deleted behind
/// the person, and its copies were left in the outbox.
#[test]
fn regression_copies_left_after_a_refused_reply() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        OtherMentions(false, 0),
        Deliver(1),
        FillReactions(0),
        ServerDeletes(0),
        UserSends(true),
        UserReacts(0, 9363513080126882977),
        UserSends(false),
        OpenThread(0),
        UserAttachesToThread(943664565400379047),
        UserAttaches(true, 11170589519720757887, false),
        UserSends(false),
        UserAttaches(false, 0, false),
        UserReacts(0, 2022222938849430521),
        Resolve(387334131995896050, Outcome::Ok),
        UserReacts(0, 240505461848441790),
        UserReactsWide(0, 0),
        UserSendsFiles(false, 4822090908048787447, false),
        OtherPosts(false),
        UserAttachesToThread(365890154685471712),
        LoadOlderReplies,
        OtherPosts(false),
        UserReplies(false),
        UserFollows(0, true),
        OtherPosts(false),
        Resolve(47622873057165106, Outcome::Ok),
        UserSends(false),
        UserSends(false),
        Resolve(5152273434341152194, Outcome::Reject),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.verify_threads();
    sim.check_final();
}

/// Found by a random run: copies left in the outbox.
#[test]
fn regression_copies_left_after_the_thread_drains() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        OtherMentions(false, 0),
        Deliver(1),
        OtherMentions(false, 0),
        Restart(4643088366335511766),
        UserSwapsReaction(0),
        UserSwapsReaction(0),
        OtherReplies(7619299259577676230, false, 3365521976478908897),
        Deliver(2),
        UserSendsFiles(false, 9465250542834290968, false),
        ServerDeletes(11670801076929245002),
        Select(false),
        Resolve(2135447007328129332, Outcome::Ok),
        OpenThread(519562210670294263),
        UserReplies(false),
        UserReplies(false),
        UserReplies(false),
        UserAttachesToThread(16954624228422377018),
        UserSends(false),
        UserAttachesToThread(4372460774762585590),
        UserReplies(false),
        Resolve(188586628529970026, Outcome::Ok),
        Resolve(931802431603406378, Outcome::Ok),
        Resolve(4870591105006872474, Outcome::Ok),
        Deliver(3),
        Resolve(1229716777636179339, Outcome::Reject),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.verify_threads();
    sim.check_final();
}

/// Found by a random run: the drain never quiesced.
#[test]
fn regression_emoji_drain_does_not_quiesce() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        UserAttaches(false, 523123344106347802, false),
        ServerDeletesEmoji(5671458115737327647),
        UserAttaches(false, 10932006386914500797, false),
        UserSendsEmoji(false, 4347747156418839703),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.verify_threads();
    sim.check_final();
}

/// A hit for a message that was deleted while the log was skipped (a snapshot replaced it) stayed on screen: the deletion event was never delivered. A resync now asks for the search again.
#[test]
fn regression_search_hit_deleted_behind_a_snapshot() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        Resolve(0, Outcome::Ok),
        Snapshot,
        UserSendsEmoji(false, 0),
        OtherPosts(false),
        UserAttaches(false, 9452502579192307933, false),
        SearchRuns(8381418920584229445),
        Select(false),
        SearchAnswered(Outcome::Ok),
        UserSendsFiles(false, 13630170531740517109, false),
        ServerDeletesHit(0),
        Resolve(494090771021836902, Outcome::Reject),
        SearchOpensHit(0),
        UserSendsFiles(false, 8894918091553114774, false),
        UserSendsFiles(false, 15896850248331339482, false),
        Resolve(10969371332480492855, Outcome::Ok),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.verify_threads();
    sim.check_final();
    sim.signs_out();
}

/// After a snapshot replaced the log, a thread that became followed during the gap (its state arrives in the bootstrap, not as an event) was missing from an open inbox. A resync now asks for the first page again.
#[test]
fn regression_inbox_misses_a_thread_followed_behind_a_snapshot() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        UserSends(false),
        Window(false),
        OtherMentions(false, 0),
        OtherPosts(false),
        InboxOpens,
        InboxAnswered(Outcome::Ok),
        Snapshot,
        OtherReplies(587193651448729850, false, 9314608262522862971),
        UserAttaches(false, 12583151106147659603, false),
        UserSendsEmoji(true, 0),
        OtherDeviceReadsThread(1842407717914788180),
        Resolve(12870980056033589795, Outcome::Ok),
        OtherPosts(false),
        UserEdits(0),
        InboxMore,
        OtherPosts(false),
        OtherPosts(false),
        OtherEdits(0),
        OtherPosts(false),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.verify_threads();
    sim.check_final();
    sim.signs_out();
}

/// A directory whose quiet refresh after a snapshot failed keeps its old rows, and says so (it
/// is not `Ready`), so only a ready directory is held to the server's roles.
#[test]
fn regression_directory_role_after_a_snapshot() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        UserSendsEmoji(false, 0),
        Resolve(5768598576008238096, Outcome::Ok),
        SearchPagesOn(Outcome::Ok),
        InvitationsCloses,
        Resolve(0, Outcome::Ok),
        UserRevokesInvitation(0),
        UserSendsEmoji(false, 0),
        SearchPagesOn(Outcome::Ok),
        Window(false),
        InvitationsCloses,
        ServerDeactivatesOther(true),
        PeopleOpens,
        SearchMore,
        PeopleAnswered(Outcome::Ok),
        UserViewsEmoji(7926195295593491641),
        UserDeactivates(false),
        UserSetsRole(false, 0),
        UserViewsEmoji(14983732075145542764),
        UserAttaches(false, 16838898354007430746, false),
        BrowseOpens,
        ServerSetsMyRole(12500098954126435886),
        Snapshot,
        UserAttaches(false, 1021995827326940379, false),
        UserAttaches(false, 295769505617782826, false),
        UserSendsFiles(false, 7148050712121638935, false),
        UserSendsFiles(false, 8435701978097999244, false),
        Resolve(7896195335240115184, Outcome::Ok),
        PeopleAnswered(Outcome::LostAnswer),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.verify_threads();
    sim.check_final();
}

/// An open inbox kept an old read position of a followed thread after the person lost the workspace and was let back in.
#[test]
fn regression_inbox_thread_state_after_access_was_lost() {
    use Action::*;
    let mut sim = Sim::new();
    let steps = [
        OtherMentions(true, 0),
        Deliver(1),
        Resolve(0, Outcome::Ok),
        OpenThread(0),
        BrowseOpens,
        InboxOpens,
        UserSendsEmoji(false, 0),
        BrowseOpens,
        UserSetsSettings(14437947206),
        UserSendsFiles(true, 1659423946920312146, false),
        InboxMarksRead,
        OtherReplies(0, false, 1902344737586313029),
        ServerDeactivatesMe,
        UserReplies(false),
        Resolve(15675071846989404640, Outcome::Ok),
    ];
    for step in steps {
        sim.step(step);
    }
    sim.drain();
    sim.verify_threads();
    sim.check_final();
}
