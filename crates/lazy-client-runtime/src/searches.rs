//! Recent searches: the last queries whose results the person opened, of one profile.
//!
//! A small plain JSON file next to the encrypted state, like [`crate::prefs`]: a convenience, not
//! an account's content, so it is not encrypted and not synced, and reading never fails. A
//! missing, unreadable, damaged, or differently versioned file is no recent searches, and the next
//! save writes a fresh one. Fields this build does not know are ignored. The core cuts what is
//! read to its limits (repeats, blanks, the count), so a hand-edited file cannot make it hold
//! more.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::files::write_atomic;

/// The file's name inside the profile directory.
pub(crate) const FILE_NAME: &str = "recent-searches.json";

/// The only version this build reads or writes.
const VERSION: u32 = 1;

#[derive(Serialize)]
struct Saved<'a> {
    version: u32,
    /// Most recent first.
    queries: &'a [String],
}

#[derive(Deserialize)]
struct Loaded {
    version: Option<u32>,
    #[serde(default)]
    queries: Vec<String>,
}

/// The recent searches saved in `dir`, most recent first, or none.
pub(crate) fn load(dir: &Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(dir.join(FILE_NAME)) else {
        return Vec::new();
    };
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Vec<String> {
    match serde_json::from_slice::<Loaded>(bytes) {
        Ok(loaded) if loaded.version == Some(VERSION) => loaded.queries,
        _ => Vec::new(),
    }
}

/// Saves `queries` in `dir`, replacing the file in one step.
pub(crate) fn save(dir: &Path, queries: &[String]) -> io::Result<()> {
    let saved = Saved {
        version: VERSION,
        queries,
    };
    let bytes = serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?;
    write_atomic(&dir.join(FILE_NAME), &bytes, &mut |_| Ok(()))
}

/// Deletes the saved searches, and any half-written copy.
pub(crate) fn remove(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(FILE_NAME));
    let _ = std::fs::remove_file(dir.join(format!("{FILE_NAME}.tmp")));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queries(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn searches_round_trip_in_order_as_plain_json() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty(), "no file");
        let saved = queries(&["deploy in:ops", "\"quoted phrase\" -draft", "日本語 😀"]);
        save(dir.path(), &saved).unwrap();
        assert_eq!(load(dir.path()), saved);
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(
            text.contains("\"version\": 1") && text.contains("日本語 😀"),
            "{text}"
        );
    }

    #[test]
    fn a_damaged_or_foreign_file_is_no_searches_and_is_replaced_by_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        for content in [
            &b""[..],
            b"{ not json",
            b"[\"a\", \"b\"]",
            b"{\"version\": \"one\", \"queries\": [\"a\"]}",
            b"{\"version\": 1, \"queries\": \"a\"}",
            b"{\"version\": 1, \"queries\": [1, 2]}",
            &[0xff, 0xfe, 0x00],
        ] {
            std::fs::write(&path, content).unwrap();
            assert!(load(dir.path()).is_empty(), "{content:?}");
        }
        save(dir.path(), &queries(&["fresh"])).unwrap();
        assert_eq!(load(dir.path()), queries(&["fresh"]));
    }

    #[test]
    fn a_file_with_another_version_is_no_searches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        // Before versions were recorded, and versions this build does not know.
        for content in [
            r#"{"queries": ["a"]}"#,
            r#"{"version": 0, "queries": ["a"]}"#,
            r#"{"version": 2, "queries": ["a"]}"#,
        ] {
            std::fs::write(&path, content).unwrap();
            assert!(load(dir.path()).is_empty(), "{content}");
        }
    }

    #[test]
    fn unknown_fields_are_ignored_and_a_missing_list_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(
            &path,
            r#"{"version": 1, "pinned": ["x"], "queries": ["a", "b"], "colour": 3}"#,
        )
        .unwrap();
        assert_eq!(load(dir.path()), queries(&["a", "b"]));
        std::fs::write(&path, r#"{"version": 1}"#).unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn removing_deletes_the_file_and_a_leftover_temporary() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &queries(&["a"])).unwrap();
        std::fs::write(dir.path().join(format!("{FILE_NAME}.tmp")), "x").unwrap();
        remove(dir.path());
        assert!(!dir.path().join(FILE_NAME).exists());
        assert!(!dir.path().join(format!("{FILE_NAME}.tmp")).exists());
    }
}
