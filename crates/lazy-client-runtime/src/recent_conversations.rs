//! Recently visited conversations: the last ones the person had open, of one profile.
//!
//! A small plain JSON file next to the encrypted state, like [`crate::searches`]: a convenience
//! that is not synced between devices, so it is not encrypted, and reading never fails. A missing,
//! unreadable, damaged, or differently versioned file is nothing visited, and the next save writes
//! a fresh one. Fields this build does not know are ignored; entries that are not ids are
//! skipped. The core cuts what is read to its limits and leaves out conversations the person is
//! no longer in.

use std::io;
use std::path::Path;

use lazy_domain::ConversationId;
use serde::{Deserialize, Serialize};

use crate::files::write_atomic;

/// The file's name inside the profile directory.
pub(crate) const FILE_NAME: &str = "recent-conversations.json";

/// The only version this build reads or writes.
const VERSION: u32 = 1;

#[derive(Serialize)]
struct Saved<'a> {
    version: u32,
    /// Most recent first.
    conversations: &'a [ConversationId],
}

#[derive(Deserialize)]
struct Loaded {
    version: Option<u32>,
    #[serde(default)]
    conversations: Vec<serde_json::Value>,
}

/// The conversations saved in `dir`, most recent first, or none.
pub(crate) fn load(dir: &Path) -> Vec<ConversationId> {
    let Ok(bytes) = std::fs::read(dir.join(FILE_NAME)) else {
        return Vec::new();
    };
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Vec<ConversationId> {
    match serde_json::from_slice::<Loaded>(bytes) {
        Ok(loaded) if loaded.version == Some(VERSION) => loaded
            .conversations
            .into_iter()
            .filter_map(|value| serde_json::from_value(value).ok())
            .collect(),
        _ => Vec::new(),
    }
}

/// Saves `conversations` in `dir`, replacing the file in one step.
pub(crate) fn save(dir: &Path, conversations: &[ConversationId]) -> io::Result<()> {
    let saved = Saved {
        version: VERSION,
        conversations,
    };
    let bytes = serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?;
    write_atomic(&dir.join(FILE_NAME), &bytes, &mut |_| Ok(()))
}

/// Deletes the saved conversations, and any half-written copy.
pub(crate) fn remove(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(FILE_NAME));
    let _ = std::fs::remove_file(dir.join(format!("{FILE_NAME}.tmp")));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversations_round_trip_in_order_as_plain_json() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty(), "no file");
        let saved: Vec<ConversationId> = (0..3).map(|_| ConversationId::generate()).collect();
        save(dir.path(), &saved).unwrap();
        assert_eq!(load(dir.path()), saved);
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
        assert!(text.contains(&saved[0].to_string()), "{text}");
    }

    #[test]
    fn a_damaged_or_foreign_file_is_nothing_and_is_replaced_by_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        for content in [
            &b""[..],
            b"{ not json",
            b"[\"a\"]",
            b"{\"version\": \"one\", \"conversations\": []}",
            b"{\"version\": 1, \"conversations\": \"a\"}",
            &[0xff, 0xfe, 0x00],
        ] {
            std::fs::write(&path, content).unwrap();
            assert!(load(dir.path()).is_empty(), "{content:?}");
        }
        let fresh = vec![ConversationId::generate()];
        save(dir.path(), &fresh).unwrap();
        assert_eq!(load(dir.path()), fresh);
    }

    #[test]
    fn another_version_is_nothing_and_unknown_fields_and_bad_entries_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let id = ConversationId::generate();
        for content in [
            format!(r#"{{"conversations": ["{id}"]}}"#),
            format!(r#"{{"version": 0, "conversations": ["{id}"]}}"#),
            format!(r#"{{"version": 2, "conversations": ["{id}"]}}"#),
        ] {
            std::fs::write(&path, content).unwrap();
            assert!(load(dir.path()).is_empty());
        }
        std::fs::write(
            &path,
            format!(
                r#"{{"version": 1, "pinned": 3, "conversations": [7, "not an id", "{id}", null]}}"#
            ),
        )
        .unwrap();
        assert_eq!(load(dir.path()), vec![id]);
    }

    #[test]
    fn removing_deletes_the_file_and_a_leftover_temporary() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &[ConversationId::generate()]).unwrap();
        std::fs::write(dir.path().join(format!("{FILE_NAME}.tmp")), "x").unwrap();
        remove(dir.path());
        assert!(!dir.path().join(FILE_NAME).exists());
        assert!(!dir.path().join(format!("{FILE_NAME}.tmp")).exists());
    }
}
