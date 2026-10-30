//! Joining for the first time (D29): what the server in use says about taking accounts, and what
//! the invitation code in the field is for. Neither creates or uses anything; they let the first
//! screens say the right thing before a person has chosen a password.

use lazy_domain::workspace::{canonical_invite_code, invite_code_is_wellformed};
use lazy_protocol::{InvitationPreview, InviteCode, PreviewInvitationRequest};

use crate::change::{Change, Output};
use crate::effect::Effect;
use crate::model::SessionPhase;
use crate::onboarding::{InvitationCheck, PreviewFailure};
use crate::server::ServerFacts;

use super::ClientCore;

/// The most a failure may say, in characters.
const MAX_REASON_CHARS: usize = 80;

impl ClientCore {
    /// What the address on screen says about taking accounts: the one being tried, else the one in
    /// use. `None` until a check has heard from it, and for a server too old to say.
    pub fn server_facts(&self) -> Option<ServerFacts> {
        let url = self.server.trying.as_deref().unwrap_or(&self.server.url);
        self.facts
            .as_ref()
            .filter(|(known, _)| known == url)
            .map(|(_, facts)| *facts)
    }

    /// What is known about the invitation code last asked about.
    pub fn invitation_check(&self) -> &InvitationCheck {
        &self.invitation
    }

    /// `Input::ServerFacts`.
    pub(super) fn server_facts_arrived(
        &mut self,
        url: String,
        facts: Option<ServerFacts>,
        out: &mut Output,
    ) {
        // Only the address on screen is listened for: a late answer about one that was typed over
        // is nothing now.
        let on_screen = self.server.trying.as_deref().unwrap_or(&self.server.url) == url;
        if !on_screen {
            return;
        }
        let before = self.server_facts();
        match facts {
            Some(facts) => self.facts = Some((url, facts)),
            None => {
                if self.facts.as_ref().is_some_and(|(known, _)| *known == url) {
                    self.facts = None;
                }
            }
        }
        if self.server_facts() != before {
            out.changes.push(Change::ServerChanged);
        }
    }

    /// `Input::PreviewInvitation`.
    pub(super) fn preview_invitation(&mut self, code: &str, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::SignedOut { .. }) {
            return;
        }
        // A newer question makes any answer on its way pointless.
        self.invitation_generation += 1;
        let generation = self.invitation_generation;
        let canonical = canonical_invite_code(code);
        if canonical.is_empty() {
            return self.set_invitation(InvitationCheck::Idle, out);
        }
        // The server being asked is the one in use; an address still being tried has no answer to
        // give yet. The screen asks again once it has moved.
        if self.server.trying.is_some() || self.server_move_in_flight() {
            return self.set_invitation(InvitationCheck::Idle, out);
        }
        if !invite_code_is_wellformed(&canonical) {
            // The server's one answer for a code that cannot be one, without asking.
            return self.set_invitation(InvitationCheck::Invalid, out);
        }
        self.set_invitation(InvitationCheck::Checking, out);
        out.effects.push(Effect::PreviewInvitation {
            request: PreviewInvitationRequest {
                code: InviteCode::new(canonical),
            },
            generation,
        });
    }

    /// `Input::InvitationPreviewed`.
    pub(super) fn invitation_previewed(
        &mut self,
        generation: u64,
        result: Result<InvitationPreview, PreviewFailure>,
        out: &mut Output,
    ) {
        if generation != self.invitation_generation
            || !matches!(self.invitation, InvitationCheck::Checking)
        {
            return;
        }
        let check = match result {
            Ok(preview) => InvitationCheck::Valid(preview),
            Err(PreviewFailure::Invalid) => InvitationCheck::Invalid,
            Err(PreviewFailure::Closed) => InvitationCheck::Closed,
            Err(PreviewFailure::Unsupported) => InvitationCheck::Unchecked,
            Err(PreviewFailure::Failed(reason)) => {
                self.note_error("invitation", "preview_failed");
                InvitationCheck::Failed(
                    reason
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(MAX_REASON_CHARS)
                        .collect(),
                )
            }
        };
        self.set_invitation(check, out);
    }

    fn set_invitation(&mut self, check: InvitationCheck, out: &mut Output) {
        if self.invitation != check {
            self.invitation = check;
            out.changes.push(Change::InvitationChanged);
        }
    }
}
