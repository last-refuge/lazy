//! Notifications describing what changed in the read model.

use lazy_domain::ConversationId;

use crate::Effect;

/// What changed. Apply the changes of one [`crate::Output`] in order, then read the new state
/// from the core. A timeline change describes the row list as it was at that point in the
/// sequence; row contents are always read from the core's final state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    PhaseChanged,
    ConnectionChanged,
    /// Workspaces, conversations, members, or display names changed.
    DirectoryChanged,
    SelectionChanged,
    DraftChanged {
        conversation: ConversationId,
    },
    /// All rows of the timeline were replaced; rebuild any list state.
    TimelineReset {
        conversation: ConversationId,
    },
    /// `removed` rows starting at `at` were replaced by `inserted` new rows. Prepending older
    /// history is a splice at 0.
    TimelineSplice {
        conversation: ConversationId,
        at: usize,
        removed: usize,
        inserted: usize,
    },
    /// The row at `index` changed in place (delivery state or content); remeasure it.
    TimelineItemUpdated {
        conversation: ConversationId,
        index: usize,
    },
    HistoryStatusChanged {
        conversation: ConversationId,
    },
    NoticeChanged,
}

/// Result of [`crate::ClientCore::handle`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Work for the runtime. Pass to `Runtime::execute`.
    pub effects: Vec<Effect>,
    /// Read-model changes for the UI, in order.
    pub changes: Vec<Change>,
}

impl Output {
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty() && self.changes.is_empty()
    }
}
