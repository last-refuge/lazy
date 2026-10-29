//! Where mentions get their names and suggestions: the client core.
//!
//! Rendering a message needs a display name for each mention; the composer needs a ranked list of
//! things to mention. Both read the core's people and groups, and this is the one place that does,
//! so the drawing and autocomplete code stay free of it.

use lazy_client_core::{ClientCore, MentionCandidate, MentionKind as CoreKind};
use lazy_domain::document::MessageDocument;
use lazy_domain::mention::MentionTarget;
use lazy_domain::{ConversationId, EmojiId, UserId};

use crate::rich_text::{EmojiLook, MentionLabels, MentionLook};
use crate::status_mark::StatusMark;
use crate::suggest::{MentionChoice, MentionKind};

/// The names for the mentions of one message, copied out of the core so a row can be drawn without
/// borrowing it.
#[derive(Debug, Clone, Default)]
pub struct MentionTable {
    entries: Vec<(MentionTarget, MentionLook)>,
    /// The custom emoji in the document: how each reads, and whether it is live (only a live
    /// emoji is drawn as a picture).
    emoji: Vec<(EmojiId, EmojiLook, bool)>,
}

impl MentionTable {
    /// Names for every mention in `document`.
    ///
    /// `author_is_me` leaves every pill unemphasised: the server notifies no one of their own
    /// message, so nothing in it is "aimed at" the person who wrote it.
    pub fn for_document(core: &ClientCore, document: &MessageDocument, author_is_me: bool) -> Self {
        let me = core.me().map(|user| user.id);
        let mut entries: Vec<(MentionTarget, MentionLook)> = Vec::new();
        for target in document.mentions() {
            if entries.iter().any(|(known, _)| *known == target) {
                continue;
            }
            if let Some(look) = look_up(core, target, me, author_is_me) {
                entries.push((target, look));
            }
        }
        // Only the emoji the workspace knows are listed here (live or deleted, by their current
        // name); one it has never heard of reads as the name its message was written with, which
        // only the document has.
        let emoji = document
            .custom_emoji()
            .into_iter()
            .filter_map(|id| {
                let known = core.custom_emoji_by_id(id)?;
                let look = EmojiLook {
                    label: crate::emoji_images::label(core, id, None),
                    picture: None,
                };
                Some((id, look, !known.deleted))
            })
            .collect();
        Self { entries, emoji }
    }
}

/// How `target` reads, or `None` when the core has nothing to say about it (the pill then shows
/// the handle the author wrote).
fn look_up(
    core: &ClientCore,
    target: MentionTarget,
    me: Option<UserId>,
    author_is_me: bool,
) -> Option<MentionLook> {
    let (text, aimed) = match target {
        MentionTarget::User { id } => (
            format!("@{}", core.member(id)?.display_name),
            Some(id) == me,
        ),
        MentionTarget::Group { id } => {
            let group = core.group(id)?;
            (
                format!("@{}", group.name),
                me.is_some_and(|me| group.members.contains(&me)),
            )
        }
        MentionTarget::Channel => ("@channel".to_owned(), true),
    };
    Some(MentionLook {
        text,
        aimed_at_me: aimed && !author_is_me,
    })
}

impl MentionTable {
    /// Draws the custom emoji whose pictures the store has decoded as pictures. Without this they
    /// are `:name:`.
    pub fn show_pictures(mut self, store: &crate::image_store::ImageStore) -> Self {
        for (id, look, live) in &mut self.emoji {
            if *live {
                look.picture = crate::emoji_images::picture(store, *id);
            }
        }
        self
    }
}

impl MentionLabels for MentionTable {
    fn emoji(&self, id: EmojiId, name: &str) -> EmojiLook {
        self.emoji
            .iter()
            .find(|(known, _, _)| *known == id)
            .map(|(_, look, _)| look.clone())
            .unwrap_or_else(|| EmojiLook {
                label: format!(":{name}:"),
                picture: None,
            })
    }

    fn look(&self, target: MentionTarget, handle: &str) -> MentionLook {
        self.entries
            .iter()
            .find(|(known, _)| *known == target)
            .map(|(_, look)| look.clone())
            .unwrap_or_else(|| MentionLook {
                // Gone from the workspace (or never known here): the handle as it was written.
                text: format!("@{handle}"),
                aimed_at_me: false,
            })
    }
}

/// What to offer for the text typed after an `@` in `conversation`, best first.
pub fn choices(core: &ClientCore, conversation: ConversationId, query: &str) -> Vec<MentionChoice> {
    core.mention_candidates(conversation, query)
        .into_iter()
        .map(|candidate| choice(core, candidate))
        .collect()
}

fn choice(core: &ClientCore, candidate: MentionCandidate) -> MentionChoice {
    let members = match candidate.target {
        MentionTarget::Group { id } => core.group(id).map(|group| group.members.len()),
        _ => None,
    };
    MentionChoice {
        handle: candidate.handle,
        name: candidate.display_name,
        kind: match candidate.kind {
            CoreKind::User => MentionKind::User,
            CoreKind::Group => MentionKind::Group,
            CoreKind::Channel => MentionKind::Channel,
        },
        outside: !candidate.in_conversation,
        members,
        status: match candidate.target {
            MentionTarget::User { id } => StatusMark::lookup(core, id),
            _ => None,
        },
    }
}
