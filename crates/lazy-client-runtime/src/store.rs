//! The client's persistent local state: the submission journal, drafts, and the content cache.
//!
//! Each kind lives in its own encrypted file (see [`crate::files`]). The journal is the one that
//! matters: an entry is written durably before its message is sent and removed only once the
//! outcome is settled, so an interrupted send can be reconciled after a restart. The content
//! cache is disposable but must be internally consistent: its cursor and messages are one value
//! in one file, replaced atomically, so a crash leaves either the old cache or the new one,
//! never a cursor from one and messages from the other.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use lazy_client_core::{ContentCache, JournalEntry, ThreadDraft, ViewState};
use lazy_domain::{ConversationId, OperationId};
use serde::{Deserialize, Serialize};

use crate::files::{FileError, FileKind, FileStore, Loaded, WriteStep};

/// Most unsettled submissions kept. More are refused so the file stays bounded.
pub(crate) const MAX_JOURNAL_ENTRIES: usize = 256;
/// Most drafts kept.
pub(crate) const MAX_DRAFTS: usize = 200;
/// Most thread drafts kept.
pub(crate) const MAX_THREAD_DRAFTS: usize = 200;
/// Most workspaces whose last open conversation is remembered.
pub(crate) const MAX_REMEMBERED_WORKSPACES: usize = 50;
/// Longest saved draft, in bytes.
pub(crate) const MAX_DRAFT_BYTES: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct JournalFile {
    entries: Vec<JournalEntry>,
}

#[derive(Serialize, Deserialize)]
struct DraftsFile {
    drafts: Vec<(ConversationId, String)>,
    /// Reply composer texts. Absent in files written before threads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    thread_drafts: Vec<ThreadDraft>,
    /// The last open conversation per workspace and the caret of each draft. Absent in files
    /// written before they were kept.
    #[serde(default, skip_serializing_if = "ViewState::is_empty")]
    view: ViewState,
}

/// Why a state write failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error("the {0} was written by a newer version of Lazy, so it can't be changed")]
    Locked(&'static str),
    #[error("too many unsent messages are waiting to be confirmed")]
    JournalFull,
    #[error(transparent)]
    File(#[from] FileError),
}

/// What `cache.enc` holds: the cache plus the protocol version whose types it contains.
///
/// The cache embeds the protocol's own types (conversations, messages, users), so it can only be
/// read by a build speaking the same protocol. Recording the version lets an old cache be told
/// apart from a damaged one: an old cache is simply discarded, because it is regenerable, instead
/// of being reported as corruption.
#[derive(serde::Serialize)]
struct CacheOnDisk<'a> {
    protocol_version: u32,
    cache: &'a ContentCache,
}

/// Reads [`CacheOnDisk`] loosely, so that a file written before the version was recorded (which
/// is the bare cache) still parses as "from an older protocol" instead of failing.
#[derive(serde::Deserialize)]
struct CacheFromDisk {
    #[serde(default)]
    protocol_version: Option<u32>,
    #[serde(default)]
    cache: Option<serde_json::Value>,
}

/// What was found on disk.
#[derive(Debug, Default)]
pub(crate) struct Recovered {
    pub(crate) journal: Vec<JournalEntry>,
    pub(crate) drafts: HashMap<ConversationId, String>,
    pub(crate) thread_drafts: Vec<ThreadDraft>,
    pub(crate) view: ViewState,
    /// The saved content cache, normalized and bounded, if one could be read.
    pub(crate) cache: Option<ContentCache>,
    pub(crate) problems: Vec<String>,
}

pub(crate) struct StateStore {
    files: FileStore,
    journal: Vec<JournalEntry>,
    drafts: BTreeMap<ConversationId, String>,
    thread_drafts: Vec<ThreadDraft>,
    view: ViewState,
    /// Kinds that must not be written because the file on disk cannot be safely replaced.
    locked: Vec<FileKind>,
}

impl StateStore {
    /// Loads whatever state exists in `dir`.
    pub(crate) fn open(dir: PathBuf, key: &[u8; 32]) -> (Self, Recovered) {
        let files = FileStore::new(dir, key);
        let mut recovered = Recovered::default();
        let mut locked = Vec::new();

        let mut note = |kind: FileKind, message: String, lock: bool, problems: &mut Vec<String>| {
            problems.push(message);
            if lock {
                locked.push(kind);
            }
        };

        let mut journal = Vec::new();
        match files.load::<JournalFile>(FileKind::Journal) {
            Loaded::Value(file) => journal = file.entries,
            Loaded::Missing => {}
            Loaded::Corrupt(message) => {
                let message = format!(
                    "{message} Messages that were still being sent may need to be sent again."
                );
                note(FileKind::Journal, message, false, &mut recovered.problems)
            }
            Loaded::NewerFormat(found) => note(
                FileKind::Journal,
                format!(
                    "The unsent-message journal was written by a newer version of Lazy (format {found}); sending is disabled."
                ),
                true,
                &mut recovered.problems,
            ),
            Loaded::Unreadable(message) => {
                note(FileKind::Journal, message, true, &mut recovered.problems)
            }
        }
        journal.truncate(MAX_JOURNAL_ENTRIES);

        let mut drafts = BTreeMap::new();
        let mut thread_drafts = Vec::new();
        let mut view = ViewState::default();
        match files.load::<DraftsFile>(FileKind::Drafts) {
            Loaded::Value(file) => {
                drafts.extend(file.drafts.into_iter().take(MAX_DRAFTS));
                thread_drafts.extend(file.thread_drafts.into_iter().take(MAX_THREAD_DRAFTS));
                view = file.view;
                view.last_selected.truncate(MAX_REMEMBERED_WORKSPACES);
                view.draft_selections.truncate(MAX_DRAFTS);
            }
            Loaded::Missing => {}
            Loaded::Corrupt(message) | Loaded::Unreadable(message) => {
                note(FileKind::Drafts, message, false, &mut recovered.problems)
            }
            Loaded::NewerFormat(found) => note(
                FileKind::Drafts,
                format!(
                    "Drafts were saved by a newer version of Lazy (format {found}) and won't be loaded."
                ),
                true,
                &mut recovered.problems,
            ),
        }

        // The cache is disposable: if it cannot be read the client behaves as on a first run.
        // A cache written by a newer version is left untouched and never overwritten.
        match files.load::<CacheFromDisk>(FileKind::Cache) {
            Loaded::Value(stored) => match (stored.protocol_version, stored.cache) {
                (Some(version), Some(value)) if version == lazy_protocol::PROTOCOL_VERSION => {
                    match serde_json::from_value::<ContentCache>(value) {
                        Ok(cache) => recovered.cache = Some(cache.bounded()),
                        Err(error) => {
                            // Same protocol, wrong shape: that is damage, not age.
                            tracing::warn!(
                                reason = %crate::api::describe_json(&error),
                                "discarding the saved message cache: its contents are invalid"
                            );
                            files.remove_current(FileKind::Cache);
                            note(
                                FileKind::Cache,
                                format!(
                                    "The saved message cache had invalid contents ({}) and was discarded. Saved messages will be downloaded again.",
                                    crate::api::describe_json(&error)
                                ),
                                false,
                                &mut recovered.problems,
                            );
                        }
                    }
                }
                (Some(version), _) if version > lazy_protocol::PROTOCOL_VERSION => note(
                    FileKind::Cache,
                    format!(
                        "The saved message cache was written by a newer version of Lazy (protocol {version}) and is not used."
                    ),
                    true,
                    &mut recovered.problems,
                ),
                _ => {
                    // Written by an older protocol, or before versions were recorded. The cache
                    // is regenerable, so this is expected after an upgrade: no warning.
                    tracing::info!("discarding the saved message cache from an older protocol");
                    files.remove_current(FileKind::Cache);
                }
            },
            Loaded::Missing => {}
            Loaded::Corrupt(message) => {
                let message = format!("{message} Saved messages will be downloaded again.");
                note(FileKind::Cache, message, false, &mut recovered.problems)
            }
            Loaded::Unreadable(message) => {
                note(FileKind::Cache, message, false, &mut recovered.problems)
            }
            Loaded::NewerFormat(found) => note(
                FileKind::Cache,
                format!(
                    "The saved message cache was written by a newer version of Lazy (format {found}) and is not used."
                ),
                true,
                &mut recovered.problems,
            ),
        }

        recovered.journal = journal.clone();
        recovered.drafts = drafts.iter().map(|(k, v)| (*k, v.clone())).collect();
        recovered.thread_drafts = thread_drafts.clone();
        recovered.view = view.clone();
        (
            Self {
                files,
                journal,
                drafts,
                thread_drafts,
                view,
                locked,
            },
            recovered,
        )
    }

    /// Whether the journal was read (or there was none), so what it names is what is wanted. A
    /// journal that could not be read may still name files that matter.
    pub(crate) fn journal_is_trusted(&self) -> bool {
        !self.locked.contains(&FileKind::Journal)
    }

    fn check_writable(&self, kind: FileKind) -> Result<(), StoreError> {
        if self.locked.contains(&kind) {
            Err(StoreError::Locked(kind.describe()))
        } else {
            Ok(())
        }
    }

    /// Durably records a submission. Returns only after the data is flushed to disk. Recording
    /// the same operation again is a no-op.
    pub(crate) fn add_journal(&mut self, entry: JournalEntry) -> Result<(), StoreError> {
        self.check_writable(FileKind::Journal)?;
        if self
            .journal
            .iter()
            .any(|e| e.operation_id == entry.operation_id)
        {
            return Ok(());
        }
        if self.journal.len() >= MAX_JOURNAL_ENTRIES {
            return Err(StoreError::JournalFull);
        }
        self.journal.push(entry);
        if let Err(error) = self.write_journal() {
            self.journal.pop();
            return Err(error);
        }
        Ok(())
    }

    /// Removes a settled submission.
    pub(crate) fn settle(&mut self, operation: OperationId) -> Result<(), StoreError> {
        self.check_writable(FileKind::Journal)?;
        let Some(index) = self
            .journal
            .iter()
            .position(|e| e.operation_id == operation)
        else {
            return Ok(());
        };
        let removed = self.journal.remove(index);
        if let Err(error) = self.write_journal() {
            self.journal.insert(index, removed);
            return Err(error);
        }
        Ok(())
    }

    fn write_journal(&self) -> Result<(), StoreError> {
        let file = JournalFile {
            entries: self.journal.clone(),
        };
        Ok(self.files.save(FileKind::Journal, &file)?)
    }

    /// Replaces all saved drafts. Oversized drafts are shortened and extra drafts dropped, since
    /// drafts are a convenience and the file must stay bounded.
    pub(crate) fn save_drafts(
        &mut self,
        drafts: &[(ConversationId, String)],
    ) -> Result<(), StoreError> {
        self.check_writable(FileKind::Drafts)?;
        self.drafts = drafts
            .iter()
            .take(MAX_DRAFTS)
            .map(|(id, text)| (*id, truncate_to(text, MAX_DRAFT_BYTES).to_owned()))
            .collect();
        self.write_drafts()
    }

    /// Replaces all saved reply drafts, bounded like the conversation drafts. They share a file
    /// with those, which is rewritten whole.
    pub(crate) fn save_thread_drafts(&mut self, drafts: &[ThreadDraft]) -> Result<(), StoreError> {
        self.check_writable(FileKind::Drafts)?;
        self.thread_drafts = drafts
            .iter()
            .take(MAX_THREAD_DRAFTS)
            .map(|draft| ThreadDraft {
                text: truncate_to(&draft.text, MAX_DRAFT_BYTES).to_owned(),
                ..draft.clone()
            })
            .collect();
        self.write_drafts()
    }

    /// Replaces what was saved of what the person was looking at, beside the drafts.
    pub(crate) fn save_view(&mut self, view: &ViewState) -> Result<(), StoreError> {
        self.check_writable(FileKind::Drafts)?;
        let mut view = view.clone();
        view.last_selected.truncate(MAX_REMEMBERED_WORKSPACES);
        view.draft_selections.truncate(MAX_DRAFTS);
        self.view = view;
        self.write_drafts()
    }

    fn write_drafts(&self) -> Result<(), StoreError> {
        let file = DraftsFile {
            drafts: self.drafts.iter().map(|(k, v)| (*k, v.clone())).collect(),
            thread_drafts: self.thread_drafts.clone(),
            view: self.view.clone(),
        };
        Ok(self.files.save(FileKind::Drafts, &file)?)
    }

    /// Replaces the saved content cache in one atomic write. If the sealed file would exceed the
    /// size limit, whole conversation timelines are evicted, least recently active first, until
    /// it fits; dropping a timeline never makes the cache inconsistent. A directory so large
    /// that nothing fits leaves no cache at all.
    pub(crate) fn save_cache(&mut self, cache: ContentCache) -> Result<(), StoreError> {
        self.save_cache_with_hook(cache, &mut |_| Ok(()))
    }

    pub(crate) fn save_cache_with_hook(
        &mut self,
        cache: ContentCache,
        hook: &mut dyn FnMut(WriteStep) -> std::io::Result<()>,
    ) -> Result<(), StoreError> {
        self.check_writable(FileKind::Cache)?;
        let mut cache = cache.bounded();
        loop {
            let on_disk = CacheOnDisk {
                protocol_version: lazy_protocol::PROTOCOL_VERSION,
                cache: &cache,
            };
            match self.files.save_with_hook(FileKind::Cache, &on_disk, hook) {
                Ok(()) => return Ok(()),
                Err(FileError::TooLarge { .. }) => {
                    if !cache.shrink() {
                        self.files.remove_current(FileKind::Cache);
                        return Ok(());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Deletes the saved content cache.
    pub(crate) fn discard_cache(&mut self) {
        if !self.locked.contains(&FileKind::Cache) {
            self.files.remove_current(FileKind::Cache);
        }
    }

    /// Deletes all local state, including temporary and quarantined files.
    pub(crate) fn wipe(&mut self) {
        for kind in FileKind::ALL {
            self.files.remove_all(kind);
        }
        // Position files written by earlier versions.
        self.files.remove_legacy("cursors.enc");
        self.journal.clear();
        self.drafts.clear();
        self.locked.clear();
    }
}

fn truncate_to(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use lazy_domain::{EventSeq, MessageDocument, Timestamp, UserId};

    use super::*;

    fn entry(text: &str) -> JournalEntry {
        JournalEntry {
            operation_id: OperationId::generate(),
            user_id: UserId::generate(),
            conversation_id: ConversationId::generate(),
            document: MessageDocument::from_plain_text(text).unwrap(),
            submitted_at: Timestamp::now(),
            kind: lazy_client_core::JournalKind::Send,
            thread_root: None,
            also_in_channel: false,
            attachments: Vec::new(),
        }
    }

    const KEY: [u8; 32] = [3; 32];

    #[test]
    fn the_journal_survives_a_restart_until_settled() {
        let dir = tempfile::tempdir().unwrap();
        let first = entry("one");
        let second = entry("two");
        {
            let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
            assert!(recovered.journal.is_empty());
            store.add_journal(first.clone()).unwrap();
            store.add_journal(second.clone()).unwrap();
            store.add_journal(second.clone()).unwrap();
            store.settle(first.operation_id).unwrap();
        }
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(recovered.journal, vec![second]);
        assert!(recovered.problems.is_empty());
    }

    #[test]
    fn the_journal_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
        for _ in 0..MAX_JOURNAL_ENTRIES {
            store.add_journal(entry("x")).unwrap();
        }
        assert!(matches!(
            store.add_journal(entry("overflow")),
            Err(StoreError::JournalFull)
        ));
    }

    #[test]
    fn a_failed_write_does_not_pretend_the_entry_was_saved() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let kept = entry("kept");
        store.add_journal(kept.clone()).unwrap();

        // Make the directory unwritable by replacing it with a file path.
        let missing = dir.path().join("gone");
        let (mut broken, _) = StateStore::open(missing, &KEY);
        assert!(broken.add_journal(entry("lost")).is_err());
        assert!(
            broken.journal.is_empty(),
            "memory matches disk after a failed write"
        );
    }

    #[test]
    fn a_corrupt_journal_is_reported_and_not_silently_dropped() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
            store.add_journal(entry("x")).unwrap();
        }
        let path = dir.path().join("journal.enc");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..10]).unwrap();

        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(recovered.journal.is_empty());
        assert_eq!(recovered.problems.len(), 1);
        assert!(recovered.problems[0].contains("set aside"));
        assert!(dir.path().read_dir().unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .contains("journal.enc.corrupt-")
        }));
        store.add_journal(entry("works again")).unwrap();
    }

    #[test]
    fn a_journal_from_a_newer_version_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
            store.add_journal(entry("x")).unwrap();
        }
        let path = dir.path().join("journal.enc");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[4..6].copy_from_slice(&99u16.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(recovered.problems.len(), 1);
        assert!(matches!(
            store.add_journal(entry("y")),
            Err(StoreError::Locked(_))
        ));
        assert!(matches!(
            store.settle(OperationId::generate()),
            Err(StoreError::Locked(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn drafts_round_trip_with_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let conversation = ConversationId::generate();
        {
            let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
            let long = "é".repeat(MAX_DRAFT_BYTES);
            store.save_drafts(&[(conversation, long)]).unwrap();
        }
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        let draft = &recovered.drafts[&conversation];
        assert!(draft.len() <= MAX_DRAFT_BYTES);
        assert!(
            draft.chars().all(|c| c == 'é'),
            "truncation respects characters"
        );
    }

    // ----- Content cache -----

    fn cache_of(timelines: usize, messages: usize, text_bytes: usize, cursor: i64) -> ContentCache {
        use lazy_client_core::{CachedTimeline, CachedWorkspace};
        use lazy_domain::{
            ChannelName, ChannelVisibility, DisplayName, MessageDocument, MessageId, UserId,
            Username, WorkspaceId, WorkspaceRole,
        };
        use lazy_protocol::{Conversation, Message, User, Workspace};
        let me = User {
            id: UserId::generate(),
            username: Username::parse("cacher").unwrap(),
            display_name: DisplayName::parse("Cacher").unwrap(),
            is_bot: false,
            status: None,
            deactivated: false,
            avatar_version: 0,
        };
        let workspace = Workspace {
            id: WorkspaceId::generate(),
            name: "W".into(),
            settings: Default::default(),
        };
        let mut conversations = Vec::new();
        let mut cached = Vec::new();
        for index in 0..timelines {
            let conversation = Conversation {
                id: ConversationId::generate(),
                workspace_id: workspace.id,
                kind: lazy_domain::ConversationKind::Channel,
                name: Some(ChannelName::parse(&format!("channel-{index}")).unwrap()),
                topic: None,
                description: None,
                visibility: ChannelVisibility::Public,
                archived: false,
                is_default: false,
                created_by: None,
                participants: Vec::new(),
                member_count: 1,
            };
            let text = "x".repeat(text_bytes);
            cached.push(CachedTimeline {
                conversation: conversation.id,
                // Later conversations are more recently active.
                messages: (0..messages)
                    .map(|m| Message {
                        id: MessageId::generate(),
                        conversation_id: conversation.id,
                        author_id: me.id,
                        document: MessageDocument::from_plain_text(&text).unwrap(),
                        created_at: Timestamp::now(),
                        revision: 1,
                        seq: EventSeq::new((index * 1000 + m + 1) as i64).unwrap(),
                        edited_at: None,
                        thread_root: None,
                        in_channel: false,
                        thread: None,
                        reactions: Vec::new(),
                        reactions_as_of: EventSeq::ZERO,
                        mentions: lazy_protocol::Mentions::default(),
                        deleted: false,
                        attachments: Vec::new(),
                    })
                    .collect(),
                has_more: false,
            });
            conversations.push(conversation);
        }
        // Read state is saved with the cursor it is current to.
        let user_state = conversations
            .iter()
            .enumerate()
            .map(|(index, conversation)| lazy_client_core::CachedUserState {
                state: lazy_protocol::ConversationUserState {
                    conversation_id: conversation.id,
                    favorite: index % 2 == 0,
                    last_read: EventSeq::new(index as i64).unwrap(),
                    unread_count: u32::try_from(index).unwrap() + 1,
                    mention_count: 0,
                },
                as_of: EventSeq::new(cursor).unwrap(),
                counted: Vec::new(),
                counted_mentions: Vec::new(),
                rebuilt_through: None,
            })
            .collect();
        ContentCache {
            user: me.clone(),
            workspaces: vec![CachedWorkspace {
                workspace,
                role: WorkspaceRole::Member,
                conversations,
                members: vec![me],
                cursor: EventSeq::new(cursor).unwrap(),
                timelines: cached,
                user_state,
                groups: Vec::new(),
                custom_emoji: Vec::new(),
                threads: Vec::new(),
            }],
        }
    }

    #[test]
    fn the_cache_round_trips_with_its_cursor_in_one_value() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_of(2, 5, 20, 77);
        {
            let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
            assert!(recovered.cache.is_none());
            store.save_cache(cache.clone()).unwrap();
        }
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(recovered.cache, Some(cache.clone()));
        assert!(recovered.problems.is_empty());
        // The read state came back with the cursor it is current to.
        let workspace = &recovered.cache.unwrap().workspaces[0];
        assert_eq!(workspace.user_state.len(), 2);
        assert!(
            workspace
                .user_state
                .iter()
                .all(|s| s.as_of == workspace.cursor)
        );
        assert!(workspace.user_state[0].state.favorite && !workspace.user_state[1].state.favorite);
    }

    #[test]
    fn a_crash_during_a_cache_write_leaves_the_old_cache_whole_never_a_mixture() {
        let old = cache_of(1, 3, 10, 10);
        let new = cache_of(1, 6, 10, 20);
        for (crash_after, expect_new) in [
            (WriteStep::TempWritten, false),
            (WriteStep::TempSynced, false),
            (WriteStep::Renamed, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
            store.save_cache(old.clone()).unwrap();
            let result = store.save_cache_with_hook(new.clone(), &mut |step| {
                if step == crash_after {
                    Err(std::io::Error::other("simulated crash"))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err());

            let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
            let found = recovered.cache.expect("a cache survives the crash");
            // Whatever survived is exactly one of the two writes: the cursor and the messages
            // always come from the same one.
            assert_eq!(
                found,
                if expect_new { new.clone() } else { old.clone() },
                "{crash_after:?}"
            );
            assert!(recovered.problems.is_empty());
        }
    }

    #[test]
    fn an_oversized_cache_evicts_whole_timelines_least_recent_first_until_it_fits() {
        let dir = tempfile::tempdir().unwrap();
        // About 4 x 100 x 20 KB = 8 MB of text, far over the 4 MiB limit.
        let cache = cache_of(4, 100, 20_000, 5000);
        let most_recent = cache.workspaces[0].timelines[3].conversation;
        let least_recent = cache.workspaces[0].timelines[0].conversation;
        {
            let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
            store.save_cache(cache).unwrap();
        }
        let size = std::fs::metadata(dir.path().join("cache.enc"))
            .unwrap()
            .len();
        assert!(size <= lazy_client_core::MAX_CACHE_BYTES, "{size} bytes");
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        let kept = recovered.cache.expect("the cache fits after eviction");
        let ids: Vec<_> = kept.workspaces[0]
            .timelines
            .iter()
            .map(|t| t.conversation)
            .collect();
        assert!(!ids.is_empty() && ids.len() < 4);
        assert!(
            ids.contains(&most_recent),
            "the most recently active timeline is kept"
        );
        assert!(
            !ids.contains(&least_recent),
            "the least recently active is evicted first"
        );
        assert_eq!(
            kept.workspaces[0].cursor.get(),
            5000,
            "the cursor is unchanged by eviction"
        );
        assert_eq!(
            kept.workspaces[0].conversations.len(),
            4,
            "the directory is untouched"
        );
    }

    #[test]
    fn counts_are_bounded_when_loading_a_file_that_exceeds_them() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        // Written behind the store's back, as an older or hand-edited file might be.
        let files = FileStore::new(dir.path().to_owned(), &KEY);

        let too_many_conversations = cache_of(60, 3, 5, 9);
        files
            .save(FileKind::Cache, &cache_file(&too_many_conversations))
            .unwrap();
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(
            recovered.cache.unwrap().timeline_count(),
            lazy_client_core::MAX_CACHED_CONVERSATIONS
        );

        let too_many_messages = cache_of(2, lazy_client_core::MAX_CACHED_MESSAGES + 20, 5, 9);
        files
            .save(FileKind::Cache, &cache_file(&too_many_messages))
            .unwrap();
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        let loaded = recovered.cache.unwrap();
        assert!(
            loaded.workspaces[0].timelines.iter().all(|t| {
                t.messages.len() == lazy_client_core::MAX_CACHED_MESSAGES && t.has_more
            })
        );
    }

    /// The cache as `cache.enc` stores it, for tests that write it behind the store's back.
    fn cache_file(cache: &ContentCache) -> CacheOnDisk<'_> {
        CacheOnDisk {
            protocol_version: lazy_protocol::PROTOCOL_VERSION,
            cache,
        }
    }

    #[test]
    fn a_cache_from_an_older_protocol_is_discarded_without_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);

        // The bare shape that was written before the protocol version was recorded, whose
        // conversations lack fields the current protocol requires (the case a real upgrade hit).
        let legacy = serde_json::json!({
            "user": { "id": "3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2c", "username": "alice", "display_name": "Alice" },
            "workspaces": [{ "conversations": [{ "id": "3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2d", "name": "general" }] }]
        });
        files.save(FileKind::Cache, &legacy).unwrap();
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "the app starts from a fresh download"
        );
        assert!(
            recovered.problems.is_empty(),
            "no alarming message: {:?}",
            recovered.problems
        );
        assert!(
            !dir.path().read_dir().unwrap().any(|e| {
                e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("cache.enc")
            }),
            "the old cache is removed, not kept as evidence of damage"
        );

        // The same for a file that records an older protocol version.
        let older = serde_json::json!({ "protocol_version": 1, "cache": { "anything": true } });
        files.save(FileKind::Cache, &older).unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(recovered.cache.is_none() && recovered.problems.is_empty());
        store.save_cache(cache_of(1, 1, 5, 6)).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(again.cache.is_some() && again.problems.is_empty());
    }

    #[test]
    fn a_cache_written_by_protocol_two_is_discarded_without_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);

        // A cache exactly as version 2 wrote it: valid for its protocol, but its messages have
        // neither `edited_at` nor `deleted`, and it is stamped with version 2.
        let mut previous = serde_json::to_value(cache_of(1, 3, 10, 5)).unwrap();
        for workspace in previous["workspaces"].as_array_mut().unwrap() {
            for timeline in workspace["timelines"].as_array_mut().unwrap() {
                for message in timeline["messages"].as_array_mut().unwrap() {
                    let message = message.as_object_mut().unwrap();
                    message.remove("edited_at");
                    message.remove("deleted");
                }
            }
        }
        files
            .save(
                FileKind::Cache,
                &serde_json::json!({ "protocol_version": 2, "cache": previous }),
            )
            .unwrap();
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "an older protocol's cache is not used"
        );
        assert!(
            recovered.problems.is_empty(),
            "and not reported as damage: {:?}",
            recovered.problems
        );
        assert!(
            !dir.path().join("cache.enc").exists(),
            "it is removed, since the next run writes a fresh one"
        );
    }

    #[test]
    fn a_cache_written_by_protocol_three_is_discarded_without_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);

        // A cache exactly as version 3 wrote it: valid for its protocol, but its messages have no
        // reactions and its read states no `counted`, and it is stamped with version 3.
        let mut previous = serde_json::to_value(cache_of(1, 3, 10, 5)).unwrap();
        for workspace in previous["workspaces"].as_array_mut().unwrap() {
            for timeline in workspace["timelines"].as_array_mut().unwrap() {
                for message in timeline["messages"].as_array_mut().unwrap() {
                    let message = message.as_object_mut().unwrap();
                    message.remove("reactions");
                    message.remove("reactions_as_of");
                }
            }
        }
        files
            .save(
                FileKind::Cache,
                &serde_json::json!({ "protocol_version": 3, "cache": previous }),
            )
            .unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "an older protocol's cache is not used"
        );
        assert!(
            recovered.problems.is_empty(),
            "and not reported as damage: {:?}",
            recovered.problems
        );
        assert!(
            !dir.path().join("cache.enc").exists(),
            "it is removed, since the next run writes a fresh one"
        );
        // The next save is a normal version 4 cache with reactions.
        let mut fresh = cache_of(1, 2, 10, 5);
        let message = &mut fresh.workspaces[0].timelines[0].messages[0];
        message.reactions = vec![lazy_protocol::ReactionSummary {
            emoji: lazy_domain::ReactionEmoji::parse("🎉").unwrap(),
            users: vec![UserId::generate()],
        }];
        message.reactions_as_of = EventSeq::new(9).unwrap();
        store.save_cache(fresh.clone()).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(again.cache, Some(fresh.bounded()));
    }

    #[test]
    fn a_cache_written_by_protocol_four_is_discarded_without_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);

        // A cache exactly as version 4 wrote it: valid for its protocol, but its messages have
        // no mentions, its workspaces no groups, and its read states nothing about mentions.
        let mut previous = serde_json::to_value(cache_of(1, 3, 10, 5)).unwrap();
        for workspace in previous["workspaces"].as_array_mut().unwrap() {
            workspace.as_object_mut().unwrap().remove("groups");
            for timeline in workspace["timelines"].as_array_mut().unwrap() {
                for message in timeline["messages"].as_array_mut().unwrap() {
                    message.as_object_mut().unwrap().remove("mentions");
                }
            }
            for state in workspace["user_state"].as_array_mut().unwrap() {
                let state = state.as_object_mut().unwrap();
                state.remove("counted_mentions");
                state.remove("rebuilt_through");
            }
        }
        files
            .save(
                FileKind::Cache,
                &serde_json::json!({ "protocol_version": 4, "cache": previous }),
            )
            .unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "an older protocol's cache is not used"
        );
        assert!(
            recovered.problems.is_empty(),
            "and not reported as damage: {:?}",
            recovered.problems
        );
        assert!(
            !dir.path().join("cache.enc").exists(),
            "it is removed, since the next run writes a fresh one"
        );
        // The next save is a normal cache that carries mentions and groups.
        let mut fresh = cache_of(1, 2, 10, 5);
        fresh.workspaces[0].timelines[0].messages[0].mentions = lazy_protocol::Mentions {
            users: vec![UserId::generate()],
            channel: true,
        };
        fresh.workspaces[0].groups = vec![lazy_protocol::UserGroup {
            id: lazy_domain::GroupId::generate(),
            workspace_id: fresh.workspaces[0].workspace.id,
            handle: lazy_domain::GroupHandle::parse("design-team").unwrap(),
            name: lazy_domain::DisplayName::parse("Design Team").unwrap(),
            members: vec![UserId::generate()],
        }];
        store.save_cache(fresh.clone()).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(again.cache, Some(fresh.bounded()));
    }

    #[test]
    fn a_cache_message_without_mentions_and_a_workspace_without_groups_still_read() {
        let mut value = serde_json::to_value(cache_of(1, 1, 10, 5)).unwrap();
        let workspace = &mut value["workspaces"][0];
        workspace.as_object_mut().unwrap().remove("groups");
        workspace["timelines"][0]["messages"][0]
            .as_object_mut()
            .unwrap()
            .remove("mentions");
        let cache: ContentCache = serde_json::from_value(value).unwrap();
        assert!(cache.workspaces[0].groups.is_empty());
        assert!(
            cache.workspaces[0].timelines[0].messages[0]
                .mentions
                .is_empty()
        );
    }

    #[test]
    fn a_cache_written_by_protocol_five_is_discarded_without_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);

        // A cache exactly as version 5 wrote it: its messages have no thread fields and its
        // workspaces no thread states.
        let mut previous = serde_json::to_value(cache_of(1, 3, 10, 5)).unwrap();
        for workspace in previous["workspaces"].as_array_mut().unwrap() {
            workspace.as_object_mut().unwrap().remove("threads");
            for timeline in workspace["timelines"].as_array_mut().unwrap() {
                for message in timeline["messages"].as_array_mut().unwrap() {
                    let message = message.as_object_mut().unwrap();
                    message.remove("thread_root");
                    message.remove("in_channel");
                    message.remove("thread");
                }
            }
        }
        files
            .save(
                FileKind::Cache,
                &serde_json::json!({ "protocol_version": 5, "cache": previous }),
            )
            .unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "an older protocol's cache is not used"
        );
        assert!(
            recovered.problems.is_empty(),
            "and not reported as damage: {:?}",
            recovered.problems
        );
        assert!(!dir.path().join("cache.enc").exists());
        // The next save is a normal cache that carries thread state.
        let mut fresh = cache_of(1, 2, 10, 5);
        let root = lazy_domain::MessageId::generate();
        let conversation = fresh.workspaces[0].conversations[0].id;
        fresh.workspaces[0].threads = vec![lazy_client_core::CachedThreadState {
            state: lazy_protocol::ThreadUserState {
                root_id: root,
                conversation_id: conversation,
                following: true,
                last_read: EventSeq::new(3).unwrap(),
                unread_count: 2,
                mention_count: 1,
            },
            as_of: EventSeq::new(5).unwrap(),
            counted: vec![EventSeq::new(6).unwrap()],
            counted_mentions: vec![EventSeq::new(6).unwrap()],
            rebuilt_through: None,
        }];
        store.save_cache(fresh.clone()).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(again.cache, Some(fresh.bounded()));
    }

    #[test]
    fn a_cache_written_by_protocol_seven_is_discarded_without_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);

        // A cache exactly as version 7 wrote it: its messages have no `attachments`.
        let mut previous = serde_json::to_value(cache_of(1, 3, 10, 5)).unwrap();
        for workspace in previous["workspaces"].as_array_mut().unwrap() {
            for timeline in workspace["timelines"].as_array_mut().unwrap() {
                for message in timeline["messages"].as_array_mut().unwrap() {
                    assert!(
                        message
                            .as_object_mut()
                            .unwrap()
                            .remove("attachments")
                            .is_none()
                    );
                }
            }
        }
        files
            .save(
                FileKind::Cache,
                &serde_json::json!({ "protocol_version": 7, "cache": previous }),
            )
            .unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "an older protocol's cache is not used"
        );
        assert!(
            recovered.problems.is_empty(),
            "and not reported as damage: {:?}",
            recovered.problems
        );
        assert!(!dir.path().join("cache.enc").exists());

        // The next save is a normal cache, and a message with files keeps them across a restart.
        let mut fresh = cache_of(1, 2, 10, 5);
        let file = lazy_protocol::Attachment {
            id: lazy_domain::AttachmentId::generate(),
            name: "photo.png".into(),
            media_type: "image/png".into(),
            size: 900,
            sha256: "ab".repeat(32),
            width: Some(40),
            height: Some(30),
            has_thumbnail: true,
        };
        fresh.workspaces[0].timelines[0].messages[0].attachments = vec![file.clone()];
        store.save_cache(fresh.clone()).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        let again = again.cache.expect("the cache is read");
        assert_eq!(
            again.workspaces[0].timelines[0].messages[0].attachments,
            vec![file]
        );
        assert_eq!(again, fresh.bounded());
    }

    #[test]
    fn a_cache_written_by_protocol_thirteen_is_discarded_without_a_warning_and_avatars_survive_a_save()
     {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);
        // A cache exactly as version 13 wrote it: its people have no `avatar_version`.
        let mut previous = serde_json::to_value(cache_of(1, 3, 10, 5)).unwrap();
        previous["user"]
            .as_object_mut()
            .unwrap()
            .remove("avatar_version");
        for workspace in previous["workspaces"].as_array_mut().unwrap() {
            for member in workspace["members"].as_array_mut().unwrap() {
                member.as_object_mut().unwrap().remove("avatar_version");
            }
        }
        files
            .save(
                FileKind::Cache,
                &serde_json::json!({ "protocol_version": 13, "cache": previous }),
            )
            .unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "an older protocol's cache is not used"
        );
        assert!(recovered.problems.is_empty(), "{:?}", recovered.problems);
        assert!(!dir.path().join("cache.enc").exists());

        // The next save keeps a person's avatar version, and a cache with several workspaces.
        let mut fresh = cache_of(1, 2, 10, 5);
        fresh.user.avatar_version = 3;
        let mut second = fresh.workspaces[0].clone();
        second.workspace.id = lazy_domain::WorkspaceId::generate();
        second.workspace.name = "Second".into();
        for conversation in &mut second.conversations {
            conversation.workspace_id = second.workspace.id;
        }
        second.timelines.clear();
        second.user_state.clear();
        fresh.workspaces.push(second);
        store.save_cache(fresh.clone()).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        let again = again.cache.expect("the cache is read");
        assert_eq!(again.workspaces.len(), 2);
        assert_eq!(again.user.avatar_version, 3);
        assert_eq!(again, fresh.bounded());
    }

    #[test]
    fn a_cache_written_by_protocol_twelve_is_discarded_without_a_warning_and_settings_survive_a_save()
     {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);
        // A cache exactly as version 12 wrote it: its workspaces have no `settings` and its
        // people no `deactivated`.
        let mut previous = serde_json::to_value(cache_of(1, 3, 10, 5)).unwrap();
        previous["user"]
            .as_object_mut()
            .unwrap()
            .remove("deactivated");
        for workspace in previous["workspaces"].as_array_mut().unwrap() {
            workspace["workspace"]
                .as_object_mut()
                .unwrap()
                .remove("settings");
            for member in workspace["members"].as_array_mut().unwrap() {
                member.as_object_mut().unwrap().remove("deactivated");
            }
        }
        files
            .save(
                FileKind::Cache,
                &serde_json::json!({ "protocol_version": 12, "cache": previous }),
            )
            .unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "an older protocol's cache is not used"
        );
        assert!(recovered.problems.is_empty(), "{:?}", recovered.problems);
        assert!(!dir.path().join("cache.enc").exists());

        // The next save keeps the workspace's settings, a person's deactivation, and the role.
        let mut fresh = cache_of(1, 2, 10, 5);
        fresh.workspaces[0].workspace.settings = lazy_domain::workspace::WorkspaceSettings {
            registration: lazy_domain::workspace::RegistrationPolicy::InviteOnly,
            channel_creation: lazy_domain::workspace::ChannelCreationPolicy::AdminsOnly,
        };
        fresh.workspaces[0].role = lazy_domain::WorkspaceRole::Admin;
        let mut amy = fresh.user.clone();
        amy.id = lazy_domain::UserId::generate();
        amy.deactivated = true;
        fresh.workspaces[0].members.push(amy);
        store.save_cache(fresh.clone()).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(again.cache.expect("the cache is read"), fresh.bounded());
    }

    #[test]
    fn a_cache_written_by_protocol_eleven_is_discarded_without_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);
        // A cache exactly as version 11 wrote it: nothing in it names the saved messages, because
        // the cache never holds them.
        let previous = serde_json::to_value(cache_of(1, 3, 10, 5)).unwrap();
        files
            .save(
                FileKind::Cache,
                &serde_json::json!({ "protocol_version": 11, "cache": previous }),
            )
            .unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "an older protocol's cache is not used"
        );
        assert!(recovered.problems.is_empty(), "{:?}", recovered.problems);
        assert!(!dir.path().join("cache.enc").exists());
        // The next save is a normal cache.
        let fresh = cache_of(1, 2, 10, 5);
        store.save_cache(fresh.clone()).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(again.cache.expect("the cache is read"), fresh.bounded());
    }

    #[test]
    fn a_cache_written_by_protocol_ten_is_discarded_without_a_warning_and_statuses_survive_a_save()
    {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);

        // A cache exactly as version 10 wrote it: its people have no status.
        let previous = serde_json::to_value(cache_of(1, 3, 10, 5)).unwrap();
        for member in previous["workspaces"][0]["members"].as_array().unwrap() {
            assert!(member.get("status").is_none());
        }
        assert!(previous["user"].get("status").is_none());
        files
            .save(
                FileKind::Cache,
                &serde_json::json!({ "protocol_version": 10, "cache": previous }),
            )
            .unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "an older protocol's cache is not used"
        );
        assert!(
            recovered.problems.is_empty(),
            "and not reported as damage: {:?}",
            recovered.problems
        );
        assert!(!dir.path().join("cache.enc").exists());

        // The next save is a normal cache, and a person's status comes back with it.
        let mut fresh = cache_of(1, 2, 10, 5);
        let status =
            lazy_domain::notification::UserStatus::new("In a meeting", None, None).unwrap();
        fresh.workspaces[0].members[0].status = Some(status.clone());
        store.save_cache(fresh.clone()).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        let again = again.cache.expect("the cache is read");
        assert_eq!(again.workspaces[0].members[0].status, Some(status));
        assert_eq!(again, fresh.bounded());

        // A version 11 cache whose people simply have no status field reads as having none.
        let plain = serde_json::to_value(cache_of(1, 2, 10, 5)).unwrap();
        files
            .save(
                FileKind::Cache,
                &serde_json::json!({
                    "protocol_version": lazy_protocol::PROTOCOL_VERSION,
                    "cache": plain,
                }),
            )
            .unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        let again = again.cache.expect("the cache is read");
        assert!(
            again.workspaces[0]
                .members
                .iter()
                .all(|m| m.status.is_none())
        );
    }

    #[test]
    fn a_cache_written_by_protocol_eight_is_discarded_without_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);

        // A cache exactly as version 8 wrote it: its workspaces have no custom emoji.
        let mut previous = serde_json::to_value(cache_of(1, 3, 10, 5)).unwrap();
        for workspace in previous["workspaces"].as_array_mut().unwrap() {
            assert!(
                workspace
                    .as_object_mut()
                    .unwrap()
                    .remove("custom_emoji")
                    .is_none()
            );
        }
        files
            .save(
                FileKind::Cache,
                &serde_json::json!({ "protocol_version": 8, "cache": previous }),
            )
            .unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "an older protocol's cache is not used"
        );
        assert!(
            recovered.problems.is_empty(),
            "and not reported as damage: {:?}",
            recovered.problems
        );
        assert!(!dir.path().join("cache.enc").exists());

        // The next save is a normal cache, and the emoji list survives a restart.
        let mut fresh = cache_of(1, 2, 10, 5);
        let emoji = lazy_protocol::CustomEmoji {
            id: lazy_domain::EmojiId::generate(),
            workspace_id: fresh.workspaces[0].workspace.id,
            name: "party_parrot".into(),
            created_by: fresh.user.id,
            created_at: lazy_domain::Timestamp::now(),
            deleted: false,
        };
        let gone = lazy_protocol::CustomEmoji {
            id: lazy_domain::EmojiId::generate(),
            name: "old_one".into(),
            deleted: true,
            ..emoji.clone()
        };
        fresh.workspaces[0].custom_emoji = vec![emoji, gone];
        store.save_cache(fresh.clone()).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        let again = again.cache.expect("the cache is read");
        assert_eq!(again.workspaces[0].custom_emoji.len(), 2);
        assert_eq!(again, fresh.bounded());
    }

    #[test]
    fn a_cache_workspace_without_custom_emoji_still_reads_as_having_none() {
        let mut value = serde_json::to_value(cache_of(1, 1, 10, 5)).unwrap();
        value["workspaces"][0]
            .as_object_mut()
            .unwrap()
            .remove("custom_emoji");
        let cache: ContentCache = serde_json::from_value(value).unwrap();
        assert!(cache.workspaces[0].custom_emoji.is_empty());
    }

    #[test]
    fn a_cache_message_without_attachments_still_reads_as_having_none() {
        let mut value = serde_json::to_value(cache_of(1, 1, 10, 5)).unwrap();
        value["workspaces"][0]["timelines"][0]["messages"][0]
            .as_object_mut()
            .unwrap()
            .remove("attachments");
        let cache: ContentCache = serde_json::from_value(value).unwrap();
        assert!(
            cache.workspaces[0].timelines[0].messages[0]
                .attachments
                .is_empty()
        );
    }

    #[test]
    fn a_journal_from_before_attachments_loads_and_one_with_files_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let old = JournalEntry {
            operation_id: OperationId::generate(),
            user_id: lazy_domain::UserId::generate(),
            conversation_id: ConversationId::generate(),
            document: MessageDocument::from_plain_text("old").unwrap(),
            submitted_at: lazy_domain::Timestamp::now(),
            kind: lazy_client_core::JournalKind::Send,
            thread_root: None,
            also_in_channel: false,
            attachments: Vec::new(),
        };
        // The file exactly as the previous version wrote it: no `attachments` in the entry.
        let mut entry = serde_json::to_value(&old).unwrap();
        assert!(
            entry
                .as_object_mut()
                .unwrap()
                .remove("attachments")
                .is_none()
        );
        FileStore::new(dir.path().to_owned(), &KEY)
            .save(
                FileKind::Journal,
                &serde_json::json!({ "entries": [entry] }),
            )
            .unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(recovered.journal, vec![old.clone()]);

        let mut with_files = old.clone();
        with_files.operation_id = OperationId::generate();
        with_files.document = MessageDocument::empty();
        with_files.attachments = vec![lazy_client_core::JournalAttachment {
            id: lazy_domain::AttachmentId::generate(),
            name: "a.pdf".into(),
            size: 12,
        }];
        store.add_journal(with_files.clone()).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(again.journal, vec![old, with_files]);
    }

    #[test]
    fn a_cache_message_without_thread_fields_and_a_workspace_without_threads_still_read() {
        let mut value = serde_json::to_value(cache_of(1, 1, 10, 5)).unwrap();
        let workspace = &mut value["workspaces"][0];
        workspace.as_object_mut().unwrap().remove("threads");
        let message = workspace["timelines"][0]["messages"][0]
            .as_object_mut()
            .unwrap();
        message.remove("thread_root");
        message.remove("in_channel");
        message.remove("thread");
        let cache: ContentCache = serde_json::from_value(value).unwrap();
        let message = &cache.workspaces[0].timelines[0].messages[0];
        assert!(cache.workspaces[0].threads.is_empty());
        assert!(message.thread_root.is_none() && !message.in_channel && message.thread.is_none());
    }

    #[test]
    fn a_drafts_file_from_before_threads_loads_and_thread_drafts_are_saved_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let conversation = ConversationId::generate();
        // The file exactly as the previous version wrote it: no thread drafts at all.
        {
            let files = FileStore::new(dir.path().to_owned(), &KEY);
            files
                .save(
                    FileKind::Drafts,
                    &serde_json::json!({ "drafts": [[conversation, "an old draft"]] }),
                )
                .unwrap();
        }
        let thread = ThreadDraft {
            root: lazy_domain::MessageId::generate(),
            conversation,
            text: "a reply".into(),
            selection: None,
        };
        {
            let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
            assert_eq!(recovered.drafts[&conversation], "an old draft");
            assert!(recovered.thread_drafts.is_empty());
            store
                .save_thread_drafts(std::slice::from_ref(&thread))
                .unwrap();
            // Saving the conversation drafts keeps the thread drafts, and the other way round.
            store
                .save_drafts(&[(conversation, "a newer draft".into())])
                .unwrap();
        }
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(recovered.drafts[&conversation], "a newer draft");
        assert_eq!(recovered.thread_drafts, vec![thread]);
    }

    #[test]
    fn a_drafts_file_from_before_carets_loads_and_the_view_is_saved_beside_the_drafts() {
        use lazy_client_core::{DraftSelection, TextSelection};
        let dir = tempfile::tempdir().unwrap();
        let conversation = ConversationId::generate();
        let root = lazy_domain::MessageId::generate();
        // The file exactly as the previous version wrote it: drafts and reply drafts, with no
        // carets and no remembered conversation.
        {
            let files = FileStore::new(dir.path().to_owned(), &KEY);
            files
                .save(
                    FileKind::Drafts,
                    &serde_json::json!({
                        "drafts": [[conversation, "a draft"]],
                        "thread_drafts": [{ "root": root, "conversation": conversation, "text": "a reply" }],
                    }),
                )
                .unwrap();
        }
        let view = lazy_client_core::ViewState {
            last_selected: vec![(lazy_domain::WorkspaceId::generate(), conversation)],
            draft_selections: vec![DraftSelection {
                conversation,
                selection: TextSelection { anchor: 1, head: 4 },
            }],
        };
        {
            let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
            assert_eq!(recovered.drafts[&conversation], "a draft");
            assert_eq!(recovered.thread_drafts[0].text, "a reply");
            assert_eq!(recovered.thread_drafts[0].selection, None);
            assert!(recovered.view.is_empty());
            store.save_view(&view).unwrap();
            // Saving the drafts keeps the view, and the other way round.
            store
                .save_drafts(&[(conversation, "a newer draft".into())])
                .unwrap();
        }
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(recovered.drafts[&conversation], "a newer draft");
        assert_eq!(recovered.view, view);
        assert_eq!(
            recovered.thread_drafts.len(),
            1,
            "the reply draft is still there"
        );
    }

    #[test]
    fn the_remembered_view_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let view = lazy_client_core::ViewState {
            last_selected: (0..MAX_REMEMBERED_WORKSPACES + 20)
                .map(|_| {
                    (
                        lazy_domain::WorkspaceId::generate(),
                        ConversationId::generate(),
                    )
                })
                .collect(),
            draft_selections: (0..MAX_DRAFTS + 20)
                .map(|_| lazy_client_core::DraftSelection {
                    conversation: ConversationId::generate(),
                    selection: lazy_client_core::TextSelection { anchor: 0, head: 0 },
                })
                .collect(),
        };
        {
            let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
            store.save_view(&view).unwrap();
        }
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(
            recovered.view.last_selected.len(),
            MAX_REMEMBERED_WORKSPACES
        );
        assert_eq!(recovered.view.draft_selections.len(), MAX_DRAFTS);
    }

    #[test]
    fn a_cache_message_without_the_reaction_fields_still_reads() {
        let mut value = serde_json::to_value(cache_of(1, 1, 10, 5)).unwrap();
        let message = &mut value["workspaces"][0]["timelines"][0]["messages"][0];
        message.as_object_mut().unwrap().remove("reactions");
        message.as_object_mut().unwrap().remove("reactions_as_of");
        let cache: ContentCache = serde_json::from_value(value).unwrap();
        let message = &cache.workspaces[0].timelines[0].messages[0];
        assert!(message.reactions.is_empty() && message.reactions_as_of == EventSeq::ZERO);
    }

    #[test]
    fn a_cache_message_without_the_edit_fields_still_reads() {
        // The new fields default, so a message that lacks them is an unedited, undeleted one.
        let mut value = serde_json::to_value(cache_of(1, 1, 10, 5)).unwrap();
        let message = &mut value["workspaces"][0]["timelines"][0]["messages"][0];
        message.as_object_mut().unwrap().remove("edited_at");
        message.as_object_mut().unwrap().remove("deleted");
        let cache: ContentCache = serde_json::from_value(value).unwrap();
        let message = &cache.workspaces[0].timelines[0].messages[0];
        assert!(!message.deleted && message.edited_at.is_none());
    }

    #[test]
    fn a_journal_written_before_edits_existed_still_loads_and_edits_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);

        // An entry exactly as the previous build stored it: no `kind`.
        let sent = entry("sent last time");
        let mut old = serde_json::to_value(&sent).unwrap();
        old.as_object_mut().unwrap().remove("kind");
        files
            .save(FileKind::Journal, &serde_json::json!({ "entries": [old] }))
            .unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(recovered.journal, vec![sent.clone()]);
        assert!(recovered.problems.is_empty(), "{:?}", recovered.problems);

        // An edit is journaled alongside it and comes back as an edit.
        let edit = JournalEntry {
            kind: lazy_client_core::JournalKind::Edit {
                message_id: lazy_domain::MessageId::generate(),
                base_revision: 4,
            },
            ..entry("edited text")
        };
        store.add_journal(edit.clone()).unwrap();
        drop(store);
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(recovered.journal, vec![sent, edit]);
        assert!(recovered.problems.is_empty());
    }

    #[test]
    fn a_cache_with_the_right_protocol_but_invalid_contents_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);
        let damaged = serde_json::json!({
            "protocol_version": lazy_protocol::PROTOCOL_VERSION,
            "cache": { "user": 7 }
        });
        files.save(FileKind::Cache, &damaged).unwrap();
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(recovered.cache.is_none());
        assert_eq!(recovered.problems.len(), 1, "{:?}", recovered.problems);
        assert!(recovered.problems[0].contains("invalid contents"));
    }

    #[test]
    fn a_cache_from_a_newer_protocol_is_neither_used_nor_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (_, _) = StateStore::open(dir.path().to_owned(), &KEY);
        let files = FileStore::new(dir.path().to_owned(), &KEY);
        let newer = serde_json::json!({
            "protocol_version": lazy_protocol::PROTOCOL_VERSION + 1,
            "cache": {}
        });
        files.save(FileKind::Cache, &newer).unwrap();
        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(recovered.cache.is_none());
        assert_eq!(recovered.problems.len(), 1);
        assert!(recovered.problems[0].contains("newer version"));
        // Not overwritten: saving is refused while the file is locked.
        assert!(store.save_cache(cache_of(1, 1, 5, 6)).is_err());
        assert!(dir.path().join("cache.enc").exists());
    }

    #[test]
    fn a_corrupt_cache_is_set_aside_reported_and_discarded() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
            store.save_cache(cache_of(1, 3, 10, 5)).unwrap();
        }
        let path = dir.path().join("cache.enc");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();

        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(
            recovered.cache.is_none(),
            "the app falls back to a first run"
        );
        assert_eq!(recovered.problems.len(), 1);
        assert!(recovered.problems[0].contains("set aside"));
        assert!(dir.path().read_dir().unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .contains("cache.enc.corrupt-")
        }));
        store.save_cache(cache_of(1, 1, 5, 6)).unwrap();
        let (_, again) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(again.cache.is_some() && again.problems.is_empty());
    }

    #[test]
    fn a_cache_from_a_newer_version_is_neither_used_nor_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
            store.save_cache(cache_of(1, 2, 5, 3)).unwrap();
        }
        let path = dir.path().join("cache.enc");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[4..6].copy_from_slice(&99u16.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let (mut store, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(recovered.cache.is_none());
        assert_eq!(recovered.problems.len(), 1);
        assert!(matches!(
            store.save_cache(cache_of(1, 1, 5, 4)),
            Err(StoreError::Locked(_))
        ));
        store.discard_cache();
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "left untouched");
    }

    #[test]
    fn discarding_removes_only_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
        store.save_cache(cache_of(1, 1, 5, 1)).unwrap();
        store.add_journal(entry("keep me")).unwrap();
        store.discard_cache();
        assert!(!dir.path().join("cache.enc").exists());
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert_eq!(recovered.journal.len(), 1);
        assert!(recovered.cache.is_none());
    }

    #[test]
    fn wipe_removes_everything() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = StateStore::open(dir.path().to_owned(), &KEY);
        store.add_journal(entry("x")).unwrap();
        store
            .save_drafts(&[(ConversationId::generate(), "d".into())])
            .unwrap();
        store.save_cache(cache_of(1, 1, 5, 1)).unwrap();
        std::fs::write(dir.path().join("cursors.enc"), b"legacy").unwrap();
        store.wipe();
        assert_eq!(dir.path().read_dir().unwrap().count(), 0);
        let (_, recovered) = StateStore::open(dir.path().to_owned(), &KEY);
        assert!(recovered.journal.is_empty() && recovered.drafts.is_empty());
    }
}
