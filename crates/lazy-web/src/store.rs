//! What the web client keeps in the browser: the unsent-message journal, drafts, and small
//! preferences, in `localStorage` under keys starting `lazy.`.
//!
//! Each value is saved in an envelope that names its version (`{"v":1,"data":...}`), so a later
//! build can tell an older shape from damage. A value that cannot be read is dropped: silently when
//! it can be made again (drafts, preferences), and with a problem shown when it held unsent
//! messages (the journal), as the desktop client does.
//!
//! Signing out deletes every `lazy.` key: on a shared computer nothing of the person stays behind.
//! The session itself is in an `HttpOnly` cookie the page cannot read, so [`SIGNED_IN`] records only
//! that there is one to try.

use std::collections::HashMap;

use lazy_client_core::{EmojiPrefs, JournalEntry, ThreadDraft, ViewState};
use lazy_domain::notification::PromptHistory;
use lazy_domain::{ConversationId, OperationId, ReactionEmoji, WorkspaceId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use web_sys::Storage;

/// The version of every envelope this build writes.
const VERSION: u32 = 1;
const PREFIX: &str = "lazy.";

pub(crate) const SIGNED_IN: &str = "signed_in";
const JOURNAL: &str = "journal";
const DRAFTS: &str = "drafts";
const THREAD_DRAFTS: &str = "thread_drafts";
const VIEW: &str = "view";
const EMOJI: &str = "emoji";
const SEARCHES: &str = "searches";
const RECENT: &str = "recent_conversations";
const ACTIVE: &str = "active_workspace";
const PROMPTS: &str = "notification_prompts";

#[derive(Serialize)]
struct EnvelopeOut<'a, T> {
    v: u32,
    data: &'a T,
}

#[derive(Deserialize)]
struct EnvelopeIn<T> {
    v: u32,
    data: T,
}

/// Emoji preferences as saved.
#[derive(Serialize, Deserialize)]
struct SavedEmoji {
    skin_tone: u8,
    recent: Vec<ReactionEmoji>,
}

/// Why a value could not be read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadError {
    /// Written by a newer build.
    Newer,
    Damaged,
}

/// Decodes an envelope's text.
pub(crate) fn open<T: DeserializeOwned>(text: &str) -> Result<T, ReadError> {
    #[derive(Deserialize)]
    struct Version {
        v: u32,
    }
    let version: Version = serde_json::from_str(text).map_err(|_| ReadError::Damaged)?;
    if version.v > VERSION {
        return Err(ReadError::Newer);
    }
    let envelope: EnvelopeIn<T> = serde_json::from_str(text).map_err(|_| ReadError::Damaged)?;
    debug_assert!(envelope.v <= VERSION);
    Ok(envelope.data)
}

/// Encodes a value in an envelope.
pub(crate) fn seal<T: Serialize>(value: &T) -> String {
    serde_json::to_string(&EnvelopeOut {
        v: VERSION,
        data: value,
    })
    .unwrap_or_default()
}

fn storage() -> Option<Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn read_raw(name: &str) -> Option<String> {
    storage()?
        .get_item(&format!("{PREFIX}{name}"))
        .ok()
        .flatten()
}

/// Writes a value; `false` when the browser refused (storage full or turned off).
fn write<T: Serialize>(name: &str, value: &T) -> bool {
    storage().is_some_and(|storage| {
        storage
            .set_item(&format!("{PREFIX}{name}"), &seal(value))
            .is_ok()
    })
}

fn remove(name: &str) {
    if let Some(storage) = storage() {
        let _ = storage.remove_item(&format!("{PREFIX}{name}"));
    }
}

/// A value that can be made again: anything unreadable is the default, and is removed.
fn read_or_default<T: DeserializeOwned + Default>(name: &str) -> T {
    match read_raw(name).map(|text| open::<T>(&text)) {
        Some(Ok(value)) => value,
        Some(Err(ReadError::Damaged)) => {
            remove(name);
            T::default()
        }
        Some(Err(ReadError::Newer)) | None => T::default(),
    }
}

/// Whether local storage works at all (private windows and some settings turn it off).
pub(crate) fn available() -> bool {
    storage().is_some_and(|storage| {
        let key = format!("{PREFIX}probe");
        let ok = storage.set_item(&key, "1").is_ok();
        let _ = storage.remove_item(&key);
        ok
    })
}

pub(crate) fn signed_in() -> bool {
    read_raw(SIGNED_IN).is_some()
}

pub(crate) fn set_signed_in(on: bool) {
    if on {
        write(SIGNED_IN, &true);
    } else {
        remove(SIGNED_IN);
    }
}

/// The journal, and a problem to show if it could not be read.
pub(crate) fn journal() -> (Vec<JournalEntry>, Option<String>) {
    match read_raw(JOURNAL).map(|text| open::<Vec<JournalEntry>>(&text)) {
        Some(Ok(entries)) => (entries, None),
        None => (Vec::new(), None),
        Some(Err(ReadError::Newer)) => (
            Vec::new(),
            Some(
                "Unsent messages were saved by a newer version of Lazy and were left alone.".into(),
            ),
        ),
        Some(Err(ReadError::Damaged)) => {
            remove(JOURNAL);
            (
                Vec::new(),
                Some("Unsent messages saved in this browser could not be read.".into()),
            )
        }
    }
}

/// Adds an entry to the journal; `false` if it could not be saved.
pub(crate) fn journal_add(entry: JournalEntry) -> bool {
    let (mut entries, _) = journal();
    entries.retain(|existing| existing.operation_id != entry.operation_id);
    entries.push(entry);
    write(JOURNAL, &entries)
}

pub(crate) fn journal_settle(operation: OperationId) {
    let (mut entries, _) = journal();
    let before = entries.len();
    entries.retain(|entry| entry.operation_id != operation);
    if entries.len() != before {
        if entries.is_empty() {
            remove(JOURNAL);
        } else {
            write(JOURNAL, &entries);
        }
    }
}

pub(crate) fn drafts() -> HashMap<ConversationId, String> {
    read_or_default::<Vec<(ConversationId, String)>>(DRAFTS)
        .into_iter()
        .collect()
}

pub(crate) fn save_drafts(drafts: &[(ConversationId, String)]) {
    write(DRAFTS, &drafts);
}

pub(crate) fn thread_drafts() -> Vec<ThreadDraft> {
    read_or_default(THREAD_DRAFTS)
}

pub(crate) fn save_thread_drafts(drafts: &[ThreadDraft]) {
    write(THREAD_DRAFTS, &drafts);
}

pub(crate) fn view() -> ViewState {
    read_or_default(VIEW)
}

pub(crate) fn save_view(view: &ViewState) {
    write(VIEW, view);
}

pub(crate) fn emoji_prefs() -> EmojiPrefs {
    let saved: Option<SavedEmoji> = read_or_default(EMOJI);
    saved
        .map(|saved| EmojiPrefs {
            skin_tone: saved.skin_tone,
            recent: saved.recent,
        })
        .unwrap_or_default()
}

pub(crate) fn save_emoji_prefs(prefs: &EmojiPrefs) {
    write(
        EMOJI,
        &Some(SavedEmoji {
            skin_tone: prefs.skin_tone,
            recent: prefs.recent.clone(),
        }),
    );
}

pub(crate) fn recent_searches() -> Vec<String> {
    read_or_default(SEARCHES)
}

pub(crate) fn save_recent_searches(queries: &[String]) {
    write(SEARCHES, &queries);
}

pub(crate) fn recent_conversations() -> Vec<ConversationId> {
    read_or_default(RECENT)
}

pub(crate) fn save_recent_conversations(ids: &[ConversationId]) {
    write(RECENT, &ids);
}

pub(crate) fn active_workspace() -> Option<WorkspaceId> {
    read_or_default(ACTIVE)
}

pub(crate) fn save_active_workspace(workspace: Option<WorkspaceId>) {
    write(ACTIVE, &workspace);
}

pub(crate) fn prompt_history() -> PromptHistory {
    read_or_default(PROMPTS)
}

pub(crate) fn save_prompt_history(history: &PromptHistory) {
    write(PROMPTS, history);
}

/// Deletes everything this client saved.
pub(crate) fn wipe() {
    let Some(storage) = storage() else { return };
    let length = storage.length().unwrap_or(0);
    let keys: Vec<String> = (0..length)
        .filter_map(|index| storage.key(index).ok().flatten())
        .filter(|key| key.starts_with(PREFIX))
        .collect();
    for key in keys {
        let _ = storage.remove_item(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_round_trip_and_tell_newer_from_damaged() {
        let text = seal(&vec![1, 2, 3]);
        assert_eq!(text, r#"{"v":1,"data":[1,2,3]}"#);
        assert_eq!(open::<Vec<u32>>(&text), Ok(vec![1, 2, 3]));
        assert_eq!(
            open::<Vec<u32>>(r#"{"v":2,"data":"anything"}"#),
            Err(ReadError::Newer)
        );
        assert_eq!(open::<Vec<u32>>("{not json"), Err(ReadError::Damaged));
        assert_eq!(
            open::<Vec<u32>>(r#"{"v":1,"data":"not a list"}"#),
            Err(ReadError::Damaged)
        );
    }

    #[test]
    fn a_journal_entry_survives_the_envelope() {
        let entry = JournalEntry {
            operation_id: OperationId::generate(),
            user_id: lazy_domain::UserId::generate(),
            conversation_id: ConversationId::generate(),
            document: lazy_domain::MessageDocument::from_plain_text("hello").unwrap(),
            submitted_at: lazy_domain::Timestamp::now(),
            kind: lazy_client_core::JournalKind::Send,
            thread_root: None,
            also_in_channel: false,
            attachments: Vec::new(),
        };
        let text = seal(&vec![entry.clone()]);
        let back: Vec<JournalEntry> = open(&text).unwrap();
        assert!(back == vec![entry]);
    }
}
