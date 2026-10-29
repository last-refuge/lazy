//! What the person was looking at: the last open conversation of each workspace and the caret of
//! each draft (D16), kept so the app reopens as they left it.
//!
//! A caret is a pair of byte offsets into the draft. The composer reports them as they move;
//! they are held as reported and read back clamped to the draft as it is now and moved to a
//! grapheme boundary, so a draft that changed or was loaded from an older save can never put the
//! caret inside a character or beyond the end. The saved file holds the clamped values.

use std::collections::HashMap;

use unicode_segmentation::UnicodeSegmentation;

use super::*;
use crate::model::{DraftSelection, TextSelection, ViewState};

/// `selection` moved onto grapheme boundaries within `text`: an offset beyond the end becomes the
/// end, and one inside a character or a cluster (an emoji with a skin tone, a letter with
/// combining marks) moves back to where it starts.
pub(crate) fn clamp_selection(text: &str, selection: TextSelection) -> TextSelection {
    let fix = |offset: usize| -> usize {
        if offset >= text.len() {
            return text.len();
        }
        let mut boundary = 0;
        for (start, _) in text.grapheme_indices(true) {
            if start > offset {
                break;
            }
            boundary = start;
        }
        boundary
    };
    TextSelection {
        anchor: fix(selection.anchor),
        head: fix(selection.head),
    }
}

impl ClientCore {
    /// Where the caret and selection of a conversation's draft were, for restoring them when the
    /// person comes back to it. `None` when there is no draft or nothing was reported. Always
    /// valid for the draft as it is now.
    pub fn draft_selection(&self, conversation: ConversationId) -> Option<TextSelection> {
        let text = self.drafts.get(&conversation)?;
        let held = self.draft_selections.get(&conversation)?;
        Some(clamp_selection(text, *held))
    }

    /// The same for a thread's reply draft.
    pub fn thread_draft_selection(&self, root: MessageId) -> Option<TextSelection> {
        let (_, text) = self.thread_drafts.get(&root)?;
        let held = self.thread_selections.get(&root)?;
        Some(clamp_selection(text, *held))
    }

    pub(super) fn set_draft_selection(
        &mut self,
        conversation: ConversationId,
        anchor: usize,
        head: usize,
        out: &mut Output,
    ) {
        if !self.drafts.contains_key(&conversation) {
            return;
        }
        let held = TextSelection { anchor, head };
        if self.draft_selections.get(&conversation) == Some(&held) {
            return;
        }
        self.draft_selections.insert(conversation, held);
        self.persist_view(out);
    }

    pub(super) fn set_thread_draft_selection(
        &mut self,
        root: MessageId,
        anchor: usize,
        head: usize,
        out: &mut Output,
    ) {
        if !self.thread_drafts.contains_key(&root) {
            return;
        }
        let held = TextSelection { anchor, head };
        if self.thread_selections.get(&root) == Some(&held) {
            return;
        }
        self.thread_selections.insert(root, held);
        self.persist_thread_drafts(out);
    }

    /// What is saved: the last open conversation per workspace, and the caret of each draft that
    /// still has text, clamped to that text.
    pub(super) fn view_state(&self) -> ViewState {
        let mut last_selected: Vec<(WorkspaceId, ConversationId)> = self
            .last_selected
            .iter()
            .map(|(workspace, conversation)| (*workspace, *conversation))
            .collect();
        last_selected.sort();
        let mut draft_selections: Vec<DraftSelection> = self
            .draft_selections
            .iter()
            .filter_map(|(conversation, held)| {
                let text = self.drafts.get(conversation)?;
                Some(DraftSelection {
                    conversation: *conversation,
                    selection: clamp_selection(text, *held),
                })
            })
            .collect();
        draft_selections.sort_by_key(|saved| saved.conversation);
        ViewState {
            last_selected,
            draft_selections,
        }
    }

    pub(super) fn persist_view(&self, out: &mut Output) {
        out.effects.push(Effect::PersistView(self.view_state()));
    }

    /// Takes what was saved at startup.
    pub(super) fn restore_view(&mut self, view: ViewState) {
        self.last_selected = view.last_selected.into_iter().collect::<HashMap<_, _>>();
        self.draft_selections = view
            .draft_selections
            .into_iter()
            .map(|saved| (saved.conversation, saved.selection))
            .collect();
    }

    /// Remembers that `conversation` is the one open in its workspace.
    pub(super) fn remember_selected(&mut self, conversation: ConversationId, out: &mut Output) {
        let Some(workspace) = self.conversation(conversation).map(|c| c.workspace_id) else {
            return;
        };
        if self.last_selected.get(&workspace) != Some(&conversation) {
            self.last_selected.insert(workspace, conversation);
            self.persist_view(out);
        }
    }

    /// The conversation to open when none is: the one last open in the primary workspace, if the
    /// person is still in it.
    pub(super) fn restored_selection(&self) -> Option<ConversationId> {
        let workspace = self.primary_workspace_id()?;
        let id = *self.last_selected.get(&workspace)?;
        self.conversation(id).map(|c| c.id)
    }
}
