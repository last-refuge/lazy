//! Choosing where a downloaded file goes, and starting the download.
//!
//! The save dialog is the system's (`prompt_for_new_path`); the core streams the file to a
//! temporary file beside the chosen path, checks it, and renames it into place. Nothing here
//! reads or writes a file.

use std::path::PathBuf;

use gpui::{App, Entity};
use lazy_client_core::Input;
use lazy_protocol::Attachment;

use crate::app_model::AppModel;

/// Where the save dialog starts: the Downloads folder if there is one, else the home folder.
pub fn default_directory() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home {
        Some(home) => {
            let downloads = home.join("Downloads");
            if downloads.is_dir() { downloads } else { home }
        }
        None => PathBuf::from("/"),
    }
}

/// What came of asking where to save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The download was started, to this path.
    Started(PathBuf),
    /// The person closed the dialog without choosing.
    Cancelled,
    /// The dialog could not be opened; the text says why.
    Failed(String),
}

/// Starts downloading `attachment`. With a `destination` (a retry, where the person already
/// chose) it starts at once; otherwise the save dialog opens with the file's name filled in.
/// `outcome` is told what happened, so the caller can remember the path or show the reason.
pub fn start(
    model: Entity<AppModel>,
    attachment: Attachment,
    destination: Option<PathBuf>,
    outcome: impl FnOnce(Outcome, &mut App) + 'static,
    cx: &mut App,
) {
    if let Some(path) = destination {
        // Told after the caller has finished with whatever it was updating: the caller is usually
        // a view in the middle of handling a click.
        cx.defer(move |cx| {
            model.update(cx, |model, cx| {
                model.dispatch(
                    Input::DownloadAttachment {
                        attachment,
                        destination: path.clone(),
                    },
                    cx,
                )
            });
            outcome(Outcome::Started(path), cx);
        });
        return;
    }
    let receiver = cx.prompt_for_new_path(&default_directory(), Some(&attachment.name));
    cx.spawn(async move |cx| {
        let result = receiver.await;
        cx.update(|cx| match result {
            Ok(Ok(Some(path))) => {
                model.update(cx, |model, cx| {
                    model.dispatch(
                        Input::DownloadAttachment {
                            attachment,
                            destination: path.clone(),
                        },
                        cx,
                    )
                });
                outcome(Outcome::Started(path), cx);
            }
            Ok(Err(error)) => outcome(
                Outcome::Failed(format!("Couldn't open the save dialog: {error}")),
                cx,
            ),
            _ => outcome(Outcome::Cancelled, cx),
        });
    })
    .detach();
}
