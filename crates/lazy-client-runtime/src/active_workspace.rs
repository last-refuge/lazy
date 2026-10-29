//! The workspace that was active when the app last ran (slice 3G, D23): a small plain JSON file
//! next to the encrypted state, like [`crate::recent_conversations`]. The core falls back to the
//! first workspace when the saved one is gone. A missing, unreadable, damaged, or differently
//! versioned file is nothing saved; it is deleted at sign-out.

use std::io;
use std::path::Path;

use lazy_domain::WorkspaceId;
use serde::{Deserialize, Serialize};

use crate::files::write_atomic;

/// The file's name inside the profile directory.
pub(crate) const FILE_NAME: &str = "active-workspace.json";

/// The only version this build reads or writes.
const VERSION: u32 = 1;

#[derive(Serialize)]
struct Saved {
    version: u32,
    workspace: Option<WorkspaceId>,
}

#[derive(Deserialize)]
struct Loaded {
    version: Option<u32>,
    #[serde(default)]
    workspace: serde_json::Value,
}

/// The workspace saved in `dir`, if any.
pub(crate) fn load(dir: &Path) -> Option<WorkspaceId> {
    let bytes = std::fs::read(dir.join(FILE_NAME)).ok()?;
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Option<WorkspaceId> {
    let loaded = serde_json::from_slice::<Loaded>(bytes).ok()?;
    if loaded.version != Some(VERSION) {
        return None;
    }
    serde_json::from_value(loaded.workspace).ok()
}

/// Saves `workspace` in `dir`, replacing the file in one step.
pub(crate) fn save(dir: &Path, workspace: Option<WorkspaceId>) -> io::Result<()> {
    let saved = Saved {
        version: VERSION,
        workspace,
    };
    let bytes = serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?;
    write_atomic(&dir.join(FILE_NAME), &bytes, &mut |_| Ok(()))
}

/// Deletes the saved workspace, and any half-written copy.
pub(crate) fn remove(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(FILE_NAME));
    let _ = std::fs::remove_file(dir.join(format!("{FILE_NAME}.tmp")));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_workspace_is_read_back_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), None);
        let workspace = WorkspaceId::generate();
        save(dir.path(), Some(workspace)).unwrap();
        assert_eq!(load(dir.path()), Some(workspace));
        save(dir.path(), None).unwrap();
        assert_eq!(load(dir.path()), None);
        save(dir.path(), Some(workspace)).unwrap();
        remove(dir.path());
        assert_eq!(load(dir.path()), None);
    }

    #[test]
    fn a_damaged_or_foreign_file_is_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = WorkspaceId::generate();
        for bytes in [
            b"nonsense".to_vec(),
            format!(r#"{{"version": 2, "workspace": "{workspace}"}}"#).into_bytes(),
            br#"{"version": 1, "workspace": "not an id"}"#.to_vec(),
            br#"{"version": 1, "workspace": 7}"#.to_vec(),
        ] {
            std::fs::write(dir.path().join(FILE_NAME), &bytes).unwrap();
            assert_eq!(
                load(dir.path()),
                None,
                "{:?}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }
}
