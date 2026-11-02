//! What the app has already asked this device about notifications: a small plain JSON file next
//! to the encrypted state, like [`crate::appearance`]. A permission belongs to the device, not
//! the account, so it is not synced and is not deleted at sign-out (signing out and in again must
//! not make the app ask afresh). A missing, unreadable, damaged, or differently versioned file is
//! "never asked", and the next prompt writes a fresh one.

use std::io;
use std::path::Path;

use lazy_domain::notification::PromptHistory;
use serde::{Deserialize, Serialize};

use crate::files::write_atomic;

/// The file's name inside the profile directory.
pub(crate) const FILE_NAME: &str = "notification-prompts.json";

/// The only version this build reads or writes.
const VERSION: u32 = 1;

#[derive(Serialize)]
struct Saved {
    version: u32,
    history: PromptHistory,
}

#[derive(Deserialize)]
struct Loaded {
    version: Option<u32>,
    history: Option<PromptHistory>,
}

/// The history saved in `dir`, or none.
pub(crate) fn load(dir: &Path) -> PromptHistory {
    let Ok(bytes) = std::fs::read(dir.join(FILE_NAME)) else {
        return PromptHistory::default();
    };
    match serde_json::from_slice::<Loaded>(&bytes) {
        Ok(Loaded {
            version: Some(VERSION),
            history: Some(history),
        }) => history,
        _ => PromptHistory::default(),
    }
}

/// Saves `history` in `dir`, replacing the file in one step.
pub(crate) fn save(dir: &Path, history: PromptHistory) -> io::Result<()> {
    let saved = Saved {
        version: VERSION,
        history,
    };
    let bytes = serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?;
    write_atomic(&dir.join(FILE_NAME), &bytes, &mut |_| Ok(()))
}

#[cfg(test)]
mod tests {
    use lazy_domain::Timestamp;
    use time::macros::datetime;

    use super::*;

    #[test]
    fn a_saved_history_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let history = PromptHistory {
            shown: 2,
            last_shown: Some(Timestamp::from(datetime!(2026-11-02 12:00 UTC))),
        };
        save(dir.path(), history).unwrap();
        assert_eq!(load(dir.path()), history);
    }

    #[test]
    fn a_missing_damaged_or_foreign_file_is_never_asked() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), PromptHistory::default());
        for bytes in [
            &b"not json"[..],
            br#"{"version": 2, "history": {"shown": 3, "last_shown": null}}"#,
            br#"{"history": {"shown": 3, "last_shown": null}}"#,
            br#"{"version": 1, "history": {"shown": "many"}}"#,
            br#"[1, 2]"#,
        ] {
            std::fs::write(dir.path().join(FILE_NAME), bytes).unwrap();
            assert_eq!(load(dir.path()), PromptHistory::default(), "{bytes:?}");
        }
    }
}
