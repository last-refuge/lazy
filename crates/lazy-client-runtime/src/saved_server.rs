//! The server the person chose in the application: a small plain JSON file next to the encrypted
//! state, like [`crate::appearance`]. It is a choice of the device, so sign-out leaves it. Only the
//! address is stored, and it has no credentials by construction. A missing, unreadable, damaged,
//! or differently versioned file, and an address that is not one, mean "nothing was chosen".

use std::io;
use std::path::Path;

use lazy_client_core::normalize_server_url;
use serde::{Deserialize, Serialize};

use crate::files::write_atomic;

/// The file's name inside the profile directory.
pub(crate) const FILE_NAME: &str = "server.json";

/// The only version this build reads or writes.
const VERSION: u32 = 1;

#[derive(Serialize)]
struct Saved<'a> {
    version: u32,
    server_url: &'a str,
}

#[derive(Deserialize)]
struct Loaded {
    version: Option<u32>,
    server_url: Option<String>,
}

/// The address saved in `dir`, normalised, if there is a usable one.
pub(crate) fn load(dir: &Path) -> Option<String> {
    let bytes = std::fs::read(dir.join(FILE_NAME)).ok()?;
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Option<String> {
    let loaded = serde_json::from_slice::<Loaded>(bytes).ok()?;
    if loaded.version != Some(VERSION) {
        return None;
    }
    normalize_server_url(&loaded.server_url?).ok()
}

/// Saves `url` (already normalised) in `dir`, replacing the file in one step.
pub(crate) fn save(dir: &Path, url: &str) -> io::Result<()> {
    let saved = Saved {
        version: VERSION,
        server_url: url,
    };
    let bytes = serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?;
    write_atomic(&dir.join(FILE_NAME), &bytes, &mut |_| Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_saved_is_loaded_and_nothing_else_is_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), None, "a profile without the file");
        save(dir.path(), "https://chat.example.com/lazy").unwrap();
        assert_eq!(
            load(dir.path()).as_deref(),
            Some("https://chat.example.com/lazy")
        );
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["server_url", "version"], "only the address: {text}");
    }

    #[test]
    fn a_damaged_old_or_unusable_file_means_nothing_was_chosen() {
        let cases: [&[u8]; 9] = [
            b"",
            b"not json",
            b"[1, 2]",
            br#"{"version": 2, "server_url": "https://chat.example.com"}"#,
            br#"{"server_url": "https://chat.example.com"}"#,
            br#"{"version": 1}"#,
            br#"{"version": 1, "server_url": 5}"#,
            br#"{"version": 1, "server_url": "ftp://chat.example.com"}"#,
            br#"{"version": 1, "server_url": "https://user:secret@chat.example.com"}"#,
        ];
        for bytes in cases {
            assert_eq!(parse(bytes), None, "{}", String::from_utf8_lossy(bytes));
        }
    }

    #[test]
    fn a_saved_address_is_read_in_its_normal_form() {
        let bytes = br#"{"version": 1, "server_url": "  Chat.Example.COM/lazy/ ", "extra": true}"#;
        assert_eq!(
            parse(bytes).as_deref(),
            Some("https://chat.example.com/lazy")
        );
    }
}
