//! Emoji preferences: the skin tone and the recently used emoji of one profile.
//!
//! A small plain JSON file next to the encrypted state. It holds a preference, not an account's
//! content: it is not secret, not encrypted, and not synced. Because it is only a convenience,
//! reading never fails: a missing, unreadable, damaged, or differently versioned file is
//! the defaults, and the next save writes a fresh one. Fields this build does not know are
//! ignored, and entries that are no longer emoji (a dataset change) are skipped.

use std::io;
use std::path::Path;

use lazy_client_core::EmojiPrefs;
use lazy_domain::ReactionEmoji;
use serde::{Deserialize, Serialize};

use crate::files::write_atomic;

/// The file's name inside the profile directory.
pub(crate) const FILE_NAME: &str = "emoji-prefs.json";

/// The only version this build reads or writes.
const VERSION: u32 = 1;

/// `recent` holds a standard emoji as its glyph and a custom one as `custom:` and its id, so a
/// file written before custom emoji reads unchanged. An entry for a custom emoji that is no
/// longer live stays in the file until newer ones push it out; the core leaves it out of what it
/// shows.
#[derive(Serialize)]
struct Saved {
    version: u32,
    skin_tone: u8,
    recent: Vec<String>,
}

#[derive(Deserialize)]
struct Loaded {
    version: Option<u32>,
    #[serde(default)]
    skin_tone: u8,
    #[serde(default)]
    recent: Vec<String>,
}

/// The preferences saved in `dir`, or the defaults.
pub(crate) fn load(dir: &Path) -> EmojiPrefs {
    let Ok(bytes) = std::fs::read(dir.join(FILE_NAME)) else {
        return EmojiPrefs::default();
    };
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> EmojiPrefs {
    let Ok(loaded) = serde_json::from_slice::<Loaded>(bytes) else {
        return EmojiPrefs::default();
    };
    if loaded.version != Some(VERSION) {
        return EmojiPrefs::default();
    }
    EmojiPrefs {
        skin_tone: loaded.skin_tone,
        recent: loaded
            .recent
            .iter()
            .filter_map(|text| ReactionEmoji::parse(text).ok())
            .collect(),
    }
    .sanitized()
}

/// Saves `prefs` in `dir`, replacing the file in one step.
pub(crate) fn save(dir: &Path, prefs: &EmojiPrefs) -> io::Result<()> {
    let saved = Saved {
        version: VERSION,
        skin_tone: prefs.skin_tone,
        recent: prefs.recent.iter().map(ReactionEmoji::to_string).collect(),
    };
    let bytes = serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?;
    write_atomic(&dir.join(FILE_NAME), &bytes, &mut |_| Ok(()))
}

/// Deletes the saved preferences, and any half-written copy.
pub(crate) fn remove(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(FILE_NAME));
    let _ = std::fs::remove_file(dir.join(format!("{FILE_NAME}.tmp")));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emoji(text: &str) -> ReactionEmoji {
        ReactionEmoji::parse(text).unwrap()
    }

    #[test]
    fn preferences_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), EmojiPrefs::default(), "no file");
        let prefs = EmojiPrefs {
            skin_tone: 3,
            recent: vec![emoji("🎉"), emoji("👍"), emoji("❤️")],
        };
        save(dir.path(), &prefs).unwrap();
        assert_eq!(load(dir.path()), prefs);
        // The file is plain JSON a person can read.
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(
            text.contains("\"version\": 1") && text.contains("🎉"),
            "{text}"
        );
    }

    #[test]
    fn custom_emoji_are_saved_by_id_and_a_file_from_before_them_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let id = lazy_domain::EmojiId::generate();
        let prefs = EmojiPrefs {
            skin_tone: 1,
            recent: vec![ReactionEmoji::custom(id), emoji("🎉")],
        };
        save(dir.path(), &prefs).unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(text.contains(&format!("custom:{id}")), "{text}");
        assert!(
            text.contains("\"version\": 1"),
            "the version did not change"
        );
        assert_eq!(load(dir.path()), prefs);
        // A file exactly as the previous version wrote it (glyphs only) loads unchanged.
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"{ "version": 1, "skin_tone": 2, "recent": ["🎉", "👍"] }"#,
        )
        .unwrap();
        assert_eq!(
            load(dir.path()),
            EmojiPrefs {
                skin_tone: 2,
                recent: vec![emoji("🎉"), emoji("👍")]
            }
        );
        // An entry that is not an emoji, not even a custom one, is skipped.
        std::fs::write(
            dir.path().join(FILE_NAME),
            format!(r#"{{ "version": 1, "recent": ["custom:not-an-id", "custom:{id}", "🎉"] }}"#),
        )
        .unwrap();
        assert_eq!(
            load(dir.path()).recent,
            vec![ReactionEmoji::custom(id), emoji("🎉")]
        );
    }

    #[test]
    fn a_damaged_or_foreign_file_is_the_defaults_and_is_replaced_by_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        for content in [
            &b""[..],
            b"{ not json",
            b"[1, 2, 3]",
            b"{\"version\": \"one\"}",
            &[0xff, 0xfe, 0x00],
        ] {
            std::fs::write(&path, content).unwrap();
            assert_eq!(load(dir.path()), EmojiPrefs::default(), "{content:?}");
        }
        let prefs = EmojiPrefs {
            skin_tone: 1,
            recent: vec![emoji("🙌")],
        };
        save(dir.path(), &prefs).unwrap();
        assert_eq!(load(dir.path()), prefs);
    }

    #[test]
    fn a_file_with_another_version_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        // Before versions were recorded, and a version this build does not know.
        for content in [
            r#"{"skin_tone": 2, "recent": ["🎉"]}"#,
            r#"{"version": 0, "skin_tone": 2, "recent": ["🎉"]}"#,
            r#"{"version": 2, "skin_tone": 2, "recent": ["🎉"]}"#,
        ] {
            std::fs::write(&path, content).unwrap();
            assert_eq!(load(dir.path()), EmojiPrefs::default(), "{content}");
        }
    }

    #[test]
    fn unknown_fields_are_ignored_and_bad_entries_are_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let many: Vec<String> = lazy_domain::emoji::groups()
            .flat_map(lazy_domain::emoji::in_group)
            .take(40)
            .map(|e| format!("\"{}\"", e.as_str()))
            .collect();
        let content = format!(
            r#"{{"version": 1, "skin_tone": 9, "favorite_color": "blue",
                "recent": ["🎉", "not an emoji", "🎉", "👍", {}]}}"#,
            many.join(", ")
        );
        std::fs::write(&path, content).unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded.skin_tone, 0, "a tone beyond the last is the default");
        assert_eq!(
            loaded.recent[..2],
            [emoji("🎉"), emoji("👍")],
            "the repeat and the non-emoji are dropped, order kept"
        );
        assert_eq!(
            loaded.recent.len(),
            lazy_client_core::MAX_RECENT_EMOJI,
            "cut to the limit"
        );
    }

    #[test]
    fn removing_deletes_the_file_and_a_leftover_temporary() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &EmojiPrefs::default()).unwrap();
        std::fs::write(dir.path().join(format!("{FILE_NAME}.tmp")), "x").unwrap();
        remove(dir.path());
        assert!(!dir.path().join(FILE_NAME).exists());
        assert!(!dir.path().join(format!("{FILE_NAME}.tmp")).exists());
    }
}
