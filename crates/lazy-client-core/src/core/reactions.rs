//! Reactions and emoji preferences.
//!
//! # Reactions
//!
//! A reaction is a state, not an operation (D13): `PUT /v1/messages/{id}/reactions` sets whether
//! the person has reacted with one emoji, so asking again is harmless and, like a favorite, it
//! carries no operation id and is not journaled. (The rule to journal before sending is about
//! requests that are not idempotent; repeating this one can only set the same state again.)
//!
//! * **Optimistic.** A toggle changes the row at once. The person's pending changes are laid over
//!   the server's list (see [`crate::timeline`]), so a reaction from someone else that arrives
//!   meanwhile updates the list without flipping the person's own pending state. After the
//!   response the row shows the server's state.
//! * **One in flight per (message, emoji).** Further toggles only change what the person wants;
//!   when the request finishes, the final wish is sent if it differs from what the server then
//!   has. If it already equals the server's state no request is made at all.
//! * **Unknown outcome.** The same request is repeated, at most [`MAX_RESENDS`] times, always
//!   asking for the person's latest wish; then the display reverts with a notice. If one of
//!   the attempts had in fact been applied, the event brings the row up to date.
//! * **Refusals** revert the display and say why.
//!
//! Each message's reactions carry their own version, `reactions_as_of`, and a row keeps whichever
//! list has the highest one, however it arrives (history page, response, fetch, an edit's or a
//! creation's payload, or [`ReactionsChanged`](lazy_protocol::Event::ReactionsChanged) whose
//! version is the event's own position). That is what makes a late page harmless.

use lazy_domain::{MAX_REACTION_KINDS, ReactionEmoji};
use lazy_protocol::{ErrorCode, MessageResponse, ReactionSummary, SetReactionRequest};

use super::conversations::readable_error;
use super::*;
use crate::model::{MAX_RECENT_EMOJI, MAX_SKIN_TONE};

const REACTION_LIMIT_NOTICE: &str =
    "This message already has the most different reactions it can hold.";

/// A change to the person's own reaction with one emoji that is not saved yet.
#[derive(Debug)]
pub(super) struct Toggle {
    conversation: ConversationId,
    /// What the person wants now.
    desired: bool,
    /// Position among pending changes, so they are shown in the order they were made.
    order: u64,
    in_flight: Option<InFlight>,
}

#[derive(Debug)]
struct InFlight {
    /// What the request in flight asks for.
    present: bool,
    /// Repeats after an unknown outcome so far.
    repeats: u32,
}

// ----- What to offer -----

impl ClientCore {
    /// Whether to offer reacting to a message: anyone who could post in its conversation may,
    /// to any message in it. Not in an archived conversation, nor to a deleted message. The
    /// server decides; this only avoids offering the impossible.
    pub fn can_react_message(&self, conversation: ConversationId, message: MessageId) -> bool {
        self.message_facts(conversation, message)
            .is_some_and(|facts| facts.can_react())
    }

    /// Whether the person has reacted to a message with `emoji`, as shown (including a change
    /// that is still being saved).
    pub fn reacted_by_me(
        &self,
        conversation: ConversationId,
        message: MessageId,
        emoji: ReactionEmoji,
    ) -> bool {
        let Some(me) = self.my_id() else {
            return false;
        };
        self.find_row(conversation, message)
            .is_some_and(|row| row.reacted_by(me, emoji))
    }

    /// Number of reaction changes the person has made that are not yet saved. Intended for
    /// diagnostics and tests.
    pub fn pending_reaction_changes(&self) -> usize {
        self.reaction_toggles.len()
    }

    /// The skin tone and recently used emoji of this profile.
    pub fn emoji_prefs(&self) -> &EmojiPrefs {
        self.shown_prefs()
    }
}

// ----- Toggling -----

impl ClientCore {
    pub(super) fn toggle_reaction(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        emoji: ReactionEmoji,
        out: &mut Output,
    ) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let Some(facts) = self.message_facts(conversation, message) else {
            return;
        };
        if !facts.can_react() {
            let notice = if facts.access.archived {
                "This conversation is read-only, so you can't react to its messages."
            } else {
                "You can't react to messages here."
            };
            self.set_notice(Some(notice.to_owned()), out);
            return;
        }
        let desired = !self.reacted_by_me(conversation, message, emoji);
        // A deleted emoji (or one the core has never heard of) cannot be newly used, but a
        // reaction already made with it can be withdrawn.
        if desired
            && emoji
                .custom_id()
                .is_some_and(|id| !self.custom_emoji_is_live(id))
        {
            self.set_notice(
                Some("That emoji has been deleted, so it can't be used.".to_owned()),
                out,
            );
            return;
        }
        if desired && self.would_exceed_reaction_limit(conversation, message, emoji) {
            self.set_notice(Some(REACTION_LIMIT_NOTICE.to_owned()), out);
            return;
        }
        let order = self.next_toggle;
        self.next_toggle += 1;
        let toggle = self
            .reaction_toggles
            .entry((message, emoji))
            .or_insert(Toggle {
                conversation,
                desired,
                order,
                in_flight: None,
            });
        toggle.desired = desired;
        self.sync_reaction_row(conversation, message, out);
        self.pump_toggle(message, emoji, out);
    }

    /// Whether adding `emoji` would make the message hold more different emoji than it may.
    fn would_exceed_reaction_limit(
        &self,
        conversation: ConversationId,
        message: MessageId,
        emoji: ReactionEmoji,
    ) -> bool {
        self.find_row(conversation, message).is_some_and(|row| {
            row.reactions.len() >= MAX_REACTION_KINDS
                && row.reactions.iter().all(|reaction| reaction.emoji != emoji)
        })
    }

    /// Sends the request for a toggle that has none in flight, or finishes it if the server
    /// already has what the person wants.
    fn pump_toggle(&mut self, message: MessageId, emoji: ReactionEmoji, out: &mut Output) {
        let Some(me) = self.my_id() else {
            return;
        };
        let Some(toggle) = self.reaction_toggles.get_mut(&(message, emoji)) else {
            return;
        };
        if toggle.in_flight.is_some() {
            return;
        }
        let (conversation, desired) = (toggle.conversation, toggle.desired);
        let held = self.server_reacted_any(conversation, message, me, emoji);
        if held == Some(desired) {
            self.reaction_toggles.remove(&(message, emoji));
            self.sync_reaction_row(conversation, message, out);
            return;
        }
        // A new kind that would not fit waits for the removal that makes room for it: sent
        // first, the server would refuse it. The row keeps showing it meanwhile.
        if desired && self.add_must_wait(conversation, message, emoji) {
            return;
        }
        if let Some(toggle) = self.reaction_toggles.get_mut(&(message, emoji)) {
            toggle.in_flight = Some(InFlight {
                present: desired,
                repeats: 0,
            });
        }
        out.effects.push(Effect::SetReaction {
            message,
            request: SetReactionRequest {
                emoji,
                present: desired,
            },
        });
    }

    pub(super) fn reaction_result(
        &mut self,
        message: MessageId,
        emoji: ReactionEmoji,
        result: Result<MessageResponse, RequestError>,
        out: &mut Output,
    ) {
        self.reaction_result_inner(message, emoji, result, out);
        // Whatever this was, a change that waited for room may go now.
        self.pump_waiting_toggles(message, out);
    }

    fn reaction_result_inner(
        &mut self,
        message: MessageId,
        emoji: ReactionEmoji,
        result: Result<MessageResponse, RequestError>,
        out: &mut Output,
    ) {
        let key = (message, emoji);
        // A repeat of an add that cannot fit yet waits for room like any other add.
        let must_wait = self.reaction_toggles.get(&key).is_some_and(|toggle| {
            toggle.desired && self.add_must_wait(toggle.conversation, message, emoji)
        });
        let Some(toggle) = self.reaction_toggles.get_mut(&key) else {
            // Reverted or dropped while the request was in flight (the message was deleted, the
            // conversation left, the session ended).
            return;
        };
        let Some(in_flight) = toggle.in_flight.as_mut() else {
            return;
        };
        let conversation = toggle.conversation;
        match result {
            Ok(response) => {
                let asked = in_flight.present;
                toggle.in_flight = None;
                let committed = response.message;
                if committed.id != message || committed.conversation_id != conversation {
                    return self.drop_toggle(message, emoji, out);
                }
                if committed.deleted {
                    // The message is gone, which removes the row and every pending change.
                    return self.remove_deleted(&committed, out);
                }
                self.each_home(conversation, out, |_, timeline, changes| {
                    timeline.apply_update(conversation, &committed, changes);
                });
                self.cache_changed(false);
                // The request did what it asked. If the person has asked for something else
                // since, the next request carries that; otherwise the row shows the server's
                // state, which may since have been changed by someone else (or on another
                // device), and that is not undone.
                let wanted = self
                    .reaction_toggles
                    .get(&key)
                    .is_some_and(|toggle| toggle.desired);
                if wanted == asked {
                    self.drop_toggle(message, emoji, out);
                } else {
                    self.pump_toggle(message, emoji, out);
                }
            }
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(RequestError::Rejected(error)) => {
                self.drop_toggle(message, emoji, out);
                let notice = match error.code {
                    ErrorCode::LimitReached => REACTION_LIMIT_NOTICE.to_owned(),
                    ErrorCode::Forbidden => "You can't react to messages here.".to_owned(),
                    ErrorCode::NotFound => "That message no longer exists.".to_owned(),
                    ErrorCode::InvalidRequest => {
                        "That emoji can't be used as a reaction.".to_owned()
                    }
                    _ => readable_error(&RequestError::Rejected(error)),
                };
                self.set_notice(Some(notice), out);
            }
            Err(RequestError::Unknown(_)) => {
                if in_flight.repeats >= MAX_RESENDS {
                    self.drop_toggle(message, emoji, out);
                    let notice = "Couldn't reach the server, so your reaction wasn't saved.";
                    self.set_notice(Some(notice.to_owned()), out);
                    return;
                }
                if must_wait {
                    // It goes once the withdrawal that makes room has returned.
                    toggle.in_flight = None;
                    return;
                }
                // The same request again, for the person's latest wish: it sets a state, so it
                // is right whatever became of the earlier attempt.
                in_flight.repeats += 1;
                in_flight.present = toggle.desired;
                out.effects.push(Effect::SetReaction {
                    message,
                    request: SetReactionRequest {
                        emoji,
                        present: toggle.desired,
                    },
                });
            }
        }
    }

    /// Whether adding `emoji` would exceed the limit on different kinds on the server as it now
    /// stands, while the person has already asked to withdraw another kind of the message. The
    /// removal makes room, so the add goes after it; with no removal to wait for, the add goes
    /// at once and the server decides.
    fn add_must_wait(
        &self,
        conversation: ConversationId,
        message: MessageId,
        emoji: ReactionEmoji,
    ) -> bool {
        let (Some(server), Some(me)) = (
            self.server_reactions_any(conversation, message),
            self.my_id(),
        ) else {
            return false;
        };
        if server.len() < MAX_REACTION_KINDS || server.iter().any(|r| r.emoji == emoji) {
            return false;
        }
        // Withdrawing a kind makes room only when nobody else uses it.
        self.reaction_toggles
            .iter()
            .any(|((pending, other), toggle)| {
                *pending == message
                    && *other != emoji
                    && !toggle.desired
                    && server
                        .iter()
                        .any(|r| r.emoji == *other && r.users.iter().all(|user| *user == me))
            })
    }

    /// Sends the changes of a message that were waiting for room, if there is room now.
    pub(super) fn pump_waiting_toggles(&mut self, message: MessageId, out: &mut Output) {
        let waiting: Vec<ReactionEmoji> = self
            .reaction_toggles
            .iter()
            .filter(|((pending, _), toggle)| {
                *pending == message && toggle.desired && toggle.in_flight.is_none()
            })
            .map(|((_, emoji), _)| *emoji)
            .collect();
        for emoji in waiting {
            self.pump_toggle(message, emoji, out);
        }
    }

    /// Forgets a pending change and shows the server's state for that emoji again.
    fn drop_toggle(&mut self, message: MessageId, emoji: ReactionEmoji, out: &mut Output) {
        if let Some(toggle) = self.reaction_toggles.remove(&(message, emoji)) {
            self.sync_reaction_row(toggle.conversation, message, out);
        }
    }

    /// Forgets every pending change to a message's reactions.
    pub(super) fn drop_reactions_of(&mut self, message: MessageId) {
        self.reaction_toggles
            .retain(|(pending, _), _| *pending != message);
    }

    /// Forgets every pending reaction change in a conversation that is no longer here.
    pub(super) fn drop_reactions_in(&mut self, conversation: ConversationId) {
        self.reaction_toggles
            .retain(|_, toggle| toggle.conversation != conversation);
    }

    /// The session ended: pending changes cannot be saved, so the rows show the server's
    /// reactions again.
    pub(super) fn drop_all_reaction_toggles(&mut self, out: &mut Output) {
        let pending: Vec<(ConversationId, MessageId)> = self
            .reaction_toggles
            .iter()
            .map(|((message, _), toggle)| (toggle.conversation, *message))
            .collect();
        self.reaction_toggles.clear();
        for (conversation, message) in pending {
            self.sync_reaction_row(conversation, message, out);
        }
    }

    /// Makes the row of `message` show the person's pending changes over the server's
    /// reactions, or the server's alone when there are none.
    pub(super) fn sync_reaction_row(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        out: &mut Output,
    ) {
        let Some(me) = self.my_id() else {
            return;
        };
        let mut pending: Vec<(u64, ReactionEmoji, bool)> = self
            .reaction_toggles
            .iter()
            .filter(|((pending, _), _)| *pending == message)
            .map(|((_, emoji), toggle)| (toggle.order, *emoji, toggle.desired))
            .collect();
        pending.sort_by_key(|(order, _, _)| *order);
        let pending: Vec<(ReactionEmoji, bool)> = pending
            .into_iter()
            .map(|(_, emoji, wanted)| (emoji, wanted))
            .collect();
        self.each_home(conversation, out, |_, timeline, changes| {
            timeline.set_pending_reactions(conversation, message, me, pending.clone(), changes);
        });
    }

    /// Re-applies pending changes to a row that has just appeared (a page, an event, or the
    /// confirmation of a send).
    pub(super) fn resync_reaction_rows(&mut self, conversation: ConversationId, out: &mut Output) {
        let mut messages: Vec<MessageId> = self
            .reaction_toggles
            .iter()
            .filter(|(_, toggle)| toggle.conversation == conversation)
            .map(|((message, _), _)| *message)
            .collect();
        messages.sort();
        messages.dedup();
        for message in messages {
            self.sync_reaction_row(conversation, message, out);
        }
    }

    // ----- Events -----

    /// `ReactionsChanged`: the complete new list, whose version is the event's own position.
    pub(super) fn apply_reactions_changed(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        reactions: Vec<ReactionSummary>,
        as_of: EventSeq,
        out: &mut Output,
    ) {
        if self.conversation(conversation).is_none() {
            return;
        }
        let mut changed = false;
        self.each_home(conversation, out, |_, timeline, changes| {
            changed |= timeline.apply_reactions(conversation, message, as_of, &reactions, changes);
        });
        if changed {
            self.cache_changed(false);
            // Someone else's change may have made the room a waiting change needed.
            self.pump_waiting_toggles(message, out);
        }
    }
}

// ----- Emoji preferences -----

impl ClientCore {
    pub(super) fn record_emoji_use(&mut self, emoji: ReactionEmoji, out: &mut Output) {
        if self.emoji_prefs.recent.first() == Some(&emoji) {
            return;
        }
        self.emoji_prefs.recent.retain(|recent| *recent != emoji);
        self.emoji_prefs.recent.insert(0, emoji);
        self.emoji_prefs.recent.truncate(MAX_RECENT_EMOJI);
        self.recompute_shown_prefs();
        self.emoji_prefs_changed(out);
    }

    pub(super) fn set_skin_tone(&mut self, tone: u8, out: &mut Output) {
        if tone > MAX_SKIN_TONE || tone == self.emoji_prefs.skin_tone {
            return;
        }
        self.emoji_prefs.skin_tone = tone;
        self.recompute_shown_prefs();
        self.emoji_prefs_changed(out);
    }

    fn emoji_prefs_changed(&mut self, out: &mut Output) {
        out.changes.push(Change::EmojiPrefsChanged);
        out.effects
            .push(Effect::PersistEmojiPrefs(self.emoji_prefs.clone()));
    }
}
