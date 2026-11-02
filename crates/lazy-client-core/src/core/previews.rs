//! Link previews (D41): asked for when a message with links is shown, once per revision, and kept
//! for the session. The server decides which links have previews and checks the person can read
//! the message on every request.

use lazy_domain::MessageId;
use lazy_protocol::LinkPreview;

use super::*;

/// The most messages whose previews are held; past it they are forgotten and asked for again.
const MAX_HELD: usize = 2_000;

/// A message's previews, for one revision of it.
#[derive(Debug, Clone)]
pub(super) struct PreviewEntry {
    revision: u32,
    /// `None` while the request is on its way.
    previews: Option<Vec<LinkPreview>>,
}

impl ClientCore {
    /// The previews of `message` at `revision`, once they have arrived; empty when it has none
    /// (or they could not be read).
    pub fn link_previews(&self, message: MessageId, revision: u32) -> Option<&[LinkPreview]> {
        self.previews
            .get(&message)
            .filter(|entry| entry.revision == revision)
            .and_then(|entry| entry.previews.as_deref())
    }

    /// Whether the previews of `message` at `revision` have been asked for (or have arrived).
    pub fn previews_requested(&self, message: MessageId, revision: u32) -> bool {
        self.previews
            .get(&message)
            .is_some_and(|entry| entry.revision >= revision)
    }

    pub(super) fn load_previews(&mut self, message: MessageId, revision: u32, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) || self.previews_requested(message, revision)
        {
            return;
        }
        if self.previews.len() >= MAX_HELD {
            self.previews.clear();
        }
        self.previews.insert(
            message,
            PreviewEntry {
                revision,
                previews: None,
            },
        );
        out.effects.push(Effect::LoadPreviews { message, revision });
    }

    pub(super) fn previews_loaded(
        &mut self,
        message: MessageId,
        revision: u32,
        result: Result<Vec<LinkPreview>, RequestError>,
        out: &mut Output,
    ) {
        let Some(entry) = self.previews.get_mut(&message) else {
            return;
        };
        if entry.revision != revision {
            return;
        }
        // A failure is shown as no previews; the next revision asks again.
        let previews = result.unwrap_or_default();
        let changed = !previews.is_empty();
        entry.previews = Some(previews);
        if changed {
            out.changes.push(Change::PreviewsChanged { message });
        }
    }
}
