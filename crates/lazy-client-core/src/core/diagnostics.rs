//! The diagnostics report (slice 3I, D24): what the core knows about its own health, and the
//! codes of the last few errors. Nothing here ever holds a name, text, path, or secret.

use std::collections::VecDeque;

use lazy_protocol::PROTOCOL_VERSION;

use crate::diagnostics::{
    DiagnosticEvent, Diagnostics, MAX_DIAGNOSTIC_EVENTS, WorkspaceDiagnostics,
};
use crate::model::{ConnectionState, RequestError, SessionPhase};

use super::ClientCore;

/// Where the server's address came from and what the last check found, in fixed words.
fn server_text(server: &crate::server::ServerInfo) -> String {
    use crate::server::{ServerCheck, ServerSource};
    let source = match server.source {
        ServerSource::Default => "default",
        ServerSource::Saved => "saved",
        ServerSource::Environment => "environment",
    };
    let check = match &server.check {
        ServerCheck::Unchecked => "unchecked",
        ServerCheck::Checking => "checking",
        ServerCheck::Reachable { .. } => "reachable",
        ServerCheck::Incompatible { .. } => "incompatible",
        ServerCheck::Unreachable(_) => "unreachable",
    };
    format!("{source}, {check}")
}

/// A short word for a connection state.
pub(super) fn connection_text(state: ConnectionState) -> String {
    match state {
        ConnectionState::Connecting => "connecting".to_owned(),
        ConnectionState::CatchingUp => "catching_up".to_owned(),
        ConnectionState::Live => "live".to_owned(),
        ConnectionState::Reconnecting { attempt } => format!("reconnecting (attempt {attempt})"),
        ConnectionState::Offline => "offline".to_owned(),
    }
}

impl ClientCore {
    /// What may be shown and copied about the core's health: no content, no names, no secrets.
    pub fn diagnostics(&self) -> Diagnostics {
        let phase = match self.phase {
            SessionPhase::Loading => "loading",
            SessionPhase::SignedOut { .. } => "signed_out",
            SessionPhase::SigningIn => "signing_in",
            SessionPhase::Unreachable { .. } => "unreachable",
            SessionPhase::Ready => "ready",
        };
        let workspaces = self
            .workspaces
            .iter()
            .map(|info| {
                let id = info.workspace.id;
                WorkspaceDiagnostics {
                    id,
                    active: self.active == Some(id),
                    connection: connection_text(
                        self.sockets
                            .get(&id)
                            .copied()
                            .unwrap_or(ConnectionState::Offline),
                    ),
                    applied: self.applied.get(&id).copied(),
                }
            })
            .collect();
        let conversations = self.all_conversation_ids().len();
        Diagnostics {
            protocol_version: PROTOCOL_VERSION,
            phase: phase.to_owned(),
            connection: connection_text(self.connection),
            server: server_text(&self.server),
            workspaces,
            conversations,
            unsettled_operations: self.unsettled_operations(),
            uploads_in_flight: self.files.uploading(),
            cached_messages: self.cache_size,
            recent_errors: self.recent_errors.iter().cloned().collect(),
        }
    }

    /// Remembers an error or notice by its category and code (never its text).
    pub(super) fn note_error(&mut self, category: &str, code: impl Into<String>) {
        if self.recent_errors.len() == MAX_DIAGNOSTIC_EVENTS {
            self.recent_errors.pop_front();
        }
        self.recent_errors.push_back(DiagnosticEvent {
            at: self.notif.clock(),
            category: category.to_owned(),
            code: code.into(),
        });
    }

    /// Remembers that a request failed.
    pub(super) fn note_request_error(&mut self, category: &str, error: &RequestError) {
        self.note_error(category, error.code());
    }
}

/// The ring of recent errors, oldest first.
pub(super) type RecentErrors = VecDeque<DiagnosticEvent>;
