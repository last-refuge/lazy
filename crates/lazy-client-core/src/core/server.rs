//! Choosing the server (dogfood packaging): the address in use, a check of an address, and
//! switching to another one. Only while signed out: a session belongs to one server, so the
//! address cannot change under a person who is signed in.

use lazy_protocol::PROTOCOL_VERSION;

use crate::change::{Change, Output};
use crate::effect::Effect;
use crate::model::{SessionPhase, StartupState};
use crate::server::{ServerCheck, ServerInfo, ServerSource, normalize_server_url};

use super::ClientCore;

const NOT_NOW: &str = "The server can only be changed while you're signed out.";
const LOCKED: &str =
    "The server is set by the environment (LAZY_SERVER_URL), so it can't be changed here.";
const UNSENT: &str = "Messages you haven't sent are waiting for this server. Let them send, or sign out, before changing it.";
/// The longest reason the runtime may give for a failed check, in characters.
const MAX_REASON_CHARS: usize = 80;

/// Where a check of an address stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum JobStage {
    /// The runtime is asking the address.
    Probing,
    /// The address passed and the runtime is saving it and moving to it.
    Switching { protocol_version: u32 },
}

/// The check, and maybe the switch, in flight. Newer ones replace older ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ServerJob {
    generation: u64,
    url: String,
    /// The address is another one than the one in use: move to it if it passes.
    switch: bool,
    stage: JobStage,
}

fn bounded(reason: &str) -> String {
    reason
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_REASON_CHARS)
        .collect()
}

impl ClientCore {
    /// The server in use and what is known about it.
    pub fn server(&self) -> &ServerInfo {
        &self.server
    }

    /// Whether the server may be looked at or changed now: only while signed out (or when a
    /// stored session could not be loaded). Says so with a notice otherwise.
    fn server_input_allowed(&mut self, out: &mut Output) -> bool {
        if matches!(
            self.phase,
            SessionPhase::SignedOut { .. } | SessionPhase::Unreachable { .. }
        ) {
            return true;
        }
        self.set_notice(Some(NOT_NOW.to_owned()), out);
        false
    }

    /// Messages waiting to be sent belong to the server in use.
    fn has_unsent_work(&self) -> bool {
        self.unsettled_operations() > 0 || !self.recovered_journal.is_empty()
    }

    /// The runtime is moving to another server (`Effect::UseServer` is out). That cannot be
    /// stopped and the core must hear how it ended, so nothing else about the server is taken
    /// until then.
    fn server_move_in_flight(&self) -> bool {
        self.server_job
            .as_ref()
            .is_some_and(|job| matches!(job.stage, JobStage::Switching { .. }))
    }

    /// `Input::SetServerUrl`: check what the person typed and, if it is another server that
    /// passes, move to it.
    pub(super) fn set_server_url(&mut self, text: &str, out: &mut Output) {
        if !self.server_input_allowed(out) || self.server_move_in_flight() {
            return;
        }
        if self.server.locked() {
            self.set_notice(Some(LOCKED.to_owned()), out);
            return;
        }
        match normalize_server_url(text) {
            Ok(url) => self.begin_server_check(url, out),
            Err(error) => {
                // The address in use stays; what was typed is not kept.
                self.server_job = None;
                self.server.trying = None;
                self.server.check = ServerCheck::Unreachable(error.message().to_owned());
                out.changes.push(Change::ServerChanged);
            }
        }
    }

    /// `Input::CheckServer`: ask the address on screen (the one being tried, else the one in
    /// use) again. Asking while it is already being asked does nothing.
    pub(super) fn check_server(&mut self, out: &mut Output) {
        if !self.server_input_allowed(out) || self.server_move_in_flight() {
            return;
        }
        let url = self
            .server
            .trying
            .clone()
            .unwrap_or_else(|| self.server.url.clone());
        self.begin_server_check(url, out);
    }

    fn begin_server_check(&mut self, url: String, out: &mut Output) {
        if self
            .server_job
            .as_ref()
            .is_some_and(|job| job.url == url && job.stage == JobStage::Probing)
        {
            return;
        }
        let switch = url != self.server.url;
        if switch && self.has_unsent_work() {
            self.set_notice(Some(UNSENT.to_owned()), out);
            return;
        }
        self.server_generation += 1;
        let generation = self.server_generation;
        self.server_job = Some(ServerJob {
            generation,
            url: url.clone(),
            switch,
            stage: JobStage::Probing,
        });
        self.server.trying = switch.then(|| url.clone());
        self.server.check = ServerCheck::Checking;
        out.effects.push(Effect::CheckServer { url, generation });
        out.changes.push(Change::ServerChanged);
    }

    /// `Input::ServerChecked`: what the runtime found at the address.
    pub(super) fn server_checked(
        &mut self,
        generation: u64,
        result: Result<u32, String>,
        out: &mut Output,
    ) {
        let Some(job) = self.server_job.take_if(|job| job.generation == generation) else {
            return;
        };
        if !matches!(
            self.phase,
            SessionPhase::SignedOut { .. } | SessionPhase::Unreachable { .. }
        ) {
            // Signed in (or signing in) since: the address stays as it was.
            self.server.trying = None;
            self.server.check = ServerCheck::Unchecked;
            out.changes.push(Change::ServerChanged);
            return;
        }
        let check = match result {
            Ok(version) if version == PROTOCOL_VERSION => ServerCheck::Reachable {
                protocol_version: version,
            },
            Ok(server) => ServerCheck::Incompatible {
                server,
                client: PROTOCOL_VERSION,
            },
            Err(reason) => ServerCheck::Unreachable(bounded(&reason)),
        };
        match (&check, job.switch) {
            (ServerCheck::Reachable { protocol_version }, true) => {
                // Still "Checking" until the move is done.
                self.server_job = Some(ServerJob {
                    stage: JobStage::Switching {
                        protocol_version: *protocol_version,
                    },
                    ..job.clone()
                });
                out.effects.push(Effect::UseServer {
                    url: job.url,
                    generation,
                });
                return;
            }
            (ServerCheck::Reachable { .. }, false) => {
                self.server.check = check;
                if matches!(self.phase, SessionPhase::Unreachable { .. }) {
                    // The stored session's server answers again: load as "check again" would.
                    self.set_phase(SessionPhase::Loading, out);
                    self.request_bootstrap(out);
                }
            }
            (ServerCheck::Incompatible { .. }, _) => {
                self.note_error("server", "incompatible");
                self.server.check = check;
            }
            _ => {
                self.note_error("server", "unreachable");
                self.server.check = check;
            }
        }
        out.changes.push(Change::ServerChanged);
    }

    /// `Input::ServerSwitched`: the runtime saved the address and moved to it, or could not.
    pub(super) fn server_switched(
        &mut self,
        generation: u64,
        result: Result<Box<StartupState>, String>,
        out: &mut Output,
    ) {
        let protocol_version = match &self.server_job {
            Some(ServerJob {
                generation: g,
                stage: JobStage::Switching { protocol_version },
                ..
            }) if *g == generation => *protocol_version,
            _ => return,
        };
        let Some(job) = self.server_job.take() else {
            return;
        };
        match result {
            Err(reason) => {
                self.note_error("server", "switch_failed");
                self.server.check = ServerCheck::Unreachable(bounded(&reason));
                out.changes.push(Change::ServerChanged);
            }
            Ok(state) => {
                // Nothing of the old server is kept: its session, messages, and drafts were
                // deleted by the runtime, and the other server's are loaded now.
                self.reset_local(out);
                let mut state = *state;
                state.server = ServerInfo {
                    url: job.url,
                    source: ServerSource::Saved,
                    trying: None,
                    check: ServerCheck::Reachable { protocol_version },
                };
                self.start_from(state, out);
            }
        }
    }
}
