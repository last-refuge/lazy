//! What the interface may show, copy, and attach to a report about a problem (slice 3I, D24):
//! counts, states, versions, and the codes of the last few errors. Never a name, a piece of text, a
//! file name, a path, an address, or a secret: everything here is a number, an identifier of the
//! kind that names nothing, or a word from a fixed list.

use std::fmt;

use lazy_domain::{EventSeq, Timestamp, WorkspaceId};

/// The most errors and notices the report keeps (the newest).
pub const MAX_DIAGNOSTIC_EVENTS: usize = 10;

/// One error or notice, by its category and code only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticEvent {
    /// The time the core last knew when it happened (`None` before it was ever told the time).
    pub at: Option<Timestamp>,
    /// What was being done: `bootstrap`, `history`, `send`, `upload`, `socket`, `realtime`,
    /// `action`, `auth`, `snapshot`, `session`, `storage`, `cache`.
    pub category: String,
    /// Why, as a snake_case word: the server's error code (`forbidden`), `unknown_outcome` (the
    /// request may or may not have happened), or a fixed word for what is not a request
    /// (`closed`, `expired`, `discarded_snapshot_required`). A message shown to the person is not
    /// an error of the program and is not recorded.
    pub code: String,
}

/// One workspace's connection, named by nothing but its identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDiagnostics {
    pub id: WorkspaceId,
    pub active: bool,
    /// `live`, `catching_up`, `connecting`, `reconnecting (attempt N)`, or `offline`.
    pub connection: String,
    /// The log position the core has applied, if it has subscribed.
    pub applied: Option<EventSeq>,
}

/// The report. Plain data, no GPUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostics {
    pub protocol_version: u32,
    /// `loading`, `signed_out`, `signing_in`, `unreachable`, or `ready` (no message text).
    pub phase: String,
    /// The aggregate connection, in the words of `WorkspaceDiagnostics::connection`.
    pub connection: String,
    /// Where the server's address came from and what the last check found, in words
    /// (`saved, reachable`). The address itself is `ClientCore::server().url`.
    pub server: String,
    pub workspaces: Vec<WorkspaceDiagnostics>,
    /// Conversations the person can read, in all workspaces.
    pub conversations: usize,
    /// Messages and edits sent or waiting to be sent (not yet confirmed, failed ones included).
    pub unsettled_operations: usize,
    pub uploads_in_flight: usize,
    /// Messages in the content cache as last saved.
    pub cached_messages: usize,
    /// The last [`MAX_DIAGNOSTIC_EVENTS`] errors and notices, oldest first.
    pub recent_errors: Vec<DiagnosticEvent>,
}

impl Diagnostics {
    /// The report as plain text, one fact to a line, for copying.
    pub fn report(&self) -> String {
        use std::fmt::Write as _;
        let mut text = String::new();
        let _ = writeln!(text, "protocol: {}", self.protocol_version);
        let _ = writeln!(text, "session: {}", self.phase);
        let _ = writeln!(text, "connection: {}", self.connection);
        let _ = writeln!(text, "server: {}", self.server);
        for (index, workspace) in self.workspaces.iter().enumerate() {
            let applied = workspace
                .applied
                .map_or_else(|| "none".to_owned(), |seq| seq.to_string());
            let _ = writeln!(
                text,
                "workspace {}: {}{}, applied {}, id {}",
                index + 1,
                workspace.connection,
                if workspace.active { " (active)" } else { "" },
                applied,
                workspace.id
            );
        }
        let _ = writeln!(text, "conversations: {}", self.conversations);
        let _ = writeln!(text, "unsettled operations: {}", self.unsettled_operations);
        let _ = writeln!(text, "uploads in flight: {}", self.uploads_in_flight);
        let _ = writeln!(text, "cached messages: {}", self.cached_messages);
        if self.recent_errors.is_empty() {
            let _ = writeln!(text, "recent errors: none");
        } else {
            let _ = writeln!(text, "recent errors (oldest first):");
            for event in &self.recent_errors {
                let _ = writeln!(text, "  {event}");
            }
        }
        text
    }
}

impl fmt::Display for DiagnosticEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.at {
            Some(at) => write!(f, "{at} ")?,
            None => write!(f, "(no clock) ")?,
        }
        write!(f, "{}: {}", self.category, self.code)
    }
}

/// `InvalidRequest` as `invalid_request`.
pub(crate) fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (index, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}
