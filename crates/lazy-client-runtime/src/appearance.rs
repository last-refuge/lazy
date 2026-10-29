//! How this device looks and moves (slice 3G, D23): a small plain JSON file next to the encrypted
//! state, like [`crate::recent_conversations`]. It is a preference of the device, not of the
//! account: it is not synced and is not deleted at sign-out. A missing, unreadable, damaged, or
//! differently versioned file is the defaults, and the next change writes a fresh one; a field
//! this build does not know is ignored, and a value it does not know falls back to the default.

use std::io;
use std::path::Path;

use lazy_client_core::{Appearance, MotionChoice, ThemeChoice};
use serde::{Deserialize, Serialize};

use crate::files::write_atomic;

/// The file's name inside the profile directory.
pub(crate) const FILE_NAME: &str = "appearance.json";

/// The only version this build reads or writes.
const VERSION: u32 = 1;

#[derive(Serialize)]
struct Saved {
    version: u32,
    appearance: Appearance,
}

#[derive(Deserialize)]
struct Loaded {
    version: Option<u32>,
    #[serde(default)]
    appearance: serde_json::Value,
}

/// The appearance saved in `dir`, or the defaults.
pub(crate) fn load(dir: &Path) -> Appearance {
    let Ok(bytes) = std::fs::read(dir.join(FILE_NAME)) else {
        return Appearance::default();
    };
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Appearance {
    let Ok(loaded) = serde_json::from_slice::<Loaded>(bytes) else {
        return Appearance::default();
    };
    if loaded.version != Some(VERSION) {
        return Appearance::default();
    }
    // Each choice is read on its own, so one value this build does not know costs only itself.
    let choice = |name: &str| loaded.appearance.get(name).cloned();
    Appearance {
        theme: choice("theme")
            .and_then(|value| serde_json::from_value::<ThemeChoice>(value).ok())
            .unwrap_or_default(),
        motion: choice("motion")
            .and_then(|value| serde_json::from_value::<MotionChoice>(value).ok())
            .unwrap_or_default(),
    }
}

/// Saves `appearance` in `dir`, replacing the file in one step.
pub(crate) fn save(dir: &Path, appearance: Appearance) -> io::Result<()> {
    let saved = Saved {
        version: VERSION,
        appearance,
    };
    let bytes = serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?;
    write_atomic(&dir.join(FILE_NAME), &bytes, &mut |_| Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_appearance_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let appearance = Appearance {
            theme: ThemeChoice::Dark,
            motion: MotionChoice::Reduce,
        };
        save(dir.path(), appearance).unwrap();
        assert_eq!(load(dir.path()), appearance);
    }

    #[test]
    fn a_missing_damaged_or_foreign_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), Appearance::default());
        for bytes in [
            &b"not json"[..],
            br#"{"version": 2, "appearance": {"theme": "dark"}}"#,
            br#"{"appearance": {"theme": "dark"}}"#,
            br#"[1, 2]"#,
        ] {
            std::fs::write(dir.path().join(FILE_NAME), bytes).unwrap();
            assert_eq!(load(dir.path()), Appearance::default(), "{bytes:?}");
        }
    }

    #[test]
    fn a_value_this_build_does_not_know_costs_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{"version": 1, "future": true, "appearance": {"theme": "sepia", "motion": "reduce"}}"#,
        )
        .unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded.theme, ThemeChoice::System);
        assert_eq!(loaded.motion, MotionChoice::Reduce);
    }
}
