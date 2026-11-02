//! The camera, microphone, and speakers chosen for drop-ins on this device: a small plain JSON
//! file next to the others, like [`crate::appearance`]. A preference of the device, not of the
//! account: not synced, not deleted at sign-out. A missing, unreadable, damaged, or differently
//! versioned file is "automatic" for everything, and the next choice writes a fresh one; a field
//! this build does not know is ignored, and a value that is not text is automatic.

use std::io;
use std::path::Path;

use lazy_client_core::DeviceChoices;
use serde::{Deserialize, Serialize};

use crate::files::write_atomic;

/// The file's name inside the profile directory.
pub(crate) const FILE_NAME: &str = "drop-in-devices.json";

/// The only version this build reads or writes.
const VERSION: u32 = 1;

#[derive(Serialize)]
struct Saved<'a> {
    version: u32,
    devices: &'a DeviceChoices,
}

#[derive(Deserialize)]
struct Loaded {
    version: Option<u32>,
    #[serde(default)]
    devices: serde_json::Value,
}

/// The choices saved in `dir`, or automatic for everything.
pub(crate) fn load(dir: &Path) -> DeviceChoices {
    let Ok(bytes) = std::fs::read(dir.join(FILE_NAME)) else {
        return DeviceChoices::default();
    };
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> DeviceChoices {
    let Ok(loaded) = serde_json::from_slice::<Loaded>(bytes) else {
        return DeviceChoices::default();
    };
    if loaded.version != Some(VERSION) {
        return DeviceChoices::default();
    }
    // Each choice is read on its own, so one bad value costs only itself.
    let choice = |name: &str| {
        loaded
            .devices
            .get(name)
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    };
    DeviceChoices {
        camera: choice("camera"),
        microphone: choice("microphone"),
        speakers: choice("speakers"),
    }
}

/// Saves `devices` in `dir`, replacing the file in one step.
pub(crate) fn save(dir: &Path, devices: &DeviceChoices) -> io::Result<()> {
    let saved = Saved {
        version: VERSION,
        devices,
    };
    let bytes = serde_json::to_vec_pretty(&saved).map_err(io::Error::other)?;
    write_atomic(&dir.join(FILE_NAME), &bytes, &mut |_| Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_choices_are_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let devices = DeviceChoices {
            camera: Some("Logitech BRIO".into()),
            microphone: None,
            speakers: Some("pulseaudio:alsa_output.usb-headset".into()),
        };
        save(dir.path(), &devices).unwrap();
        assert_eq!(load(dir.path()), devices);
    }

    #[test]
    fn a_missing_damaged_or_foreign_file_is_automatic() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), DeviceChoices::default());
        for bytes in [
            &b"not json"[..],
            br#"{"version": 2, "devices": {"camera": "x"}}"#,
            br#"{"devices": {"camera": "x"}}"#,
            br#"[1, 2]"#,
        ] {
            std::fs::write(dir.path().join(FILE_NAME), bytes).unwrap();
            assert_eq!(load(dir.path()), DeviceChoices::default(), "{bytes:?}");
        }
    }

    #[test]
    fn a_value_this_build_does_not_understand_costs_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            br#"{"version": 1, "future": 1, "devices": {"camera": 7, "microphone": "mic", "lights": "on"}}"#,
        )
        .unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded.camera, None);
        assert_eq!(loaded.microphone.as_deref(), Some("mic"));
    }
}
