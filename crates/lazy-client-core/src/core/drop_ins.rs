//! Joining, leaving, and following a drop-in. One at a time: joining another leaves the first.

use lazy_domain::ConversationId;

use super::*;
use crate::drop_in::{DeviceChoices, DeviceKind, DropIn, DropInReport, DropInStatus, MediaDevices};

impl ClientCore {
    /// The drop-in this person is in, if any.
    pub fn drop_in(&self) -> Option<&DropIn> {
        self.drop_in.as_ref()
    }

    /// Why the last drop-in ended, until dismissed or another is joined.
    pub fn drop_in_error(&self) -> Option<&str> {
        self.drop_in_error.as_deref()
    }

    /// The cameras, microphones, and speakers this device offered when last listed
    /// (`Input::ListMediaDevices`).
    pub fn media_devices(&self) -> &MediaDevices {
        &self.media_devices
    }

    /// The devices drop-ins use on this device.
    pub fn device_choices(&self) -> &DeviceChoices {
        &self.device_choices
    }

    pub(super) fn media_devices_listed(&mut self, devices: MediaDevices, out: &mut Output) {
        if self.media_devices != devices {
            self.media_devices = devices;
            out.changes.push(Change::MediaDevicesChanged);
        }
    }

    pub(super) fn choose_device(&mut self, kind: DeviceKind, id: Option<String>, out: &mut Output) {
        if self.device_choices.get(kind) == id.as_deref() {
            return;
        }
        self.device_choices.set(kind, id.clone());
        out.effects
            .push(Effect::PersistDeviceChoices(self.device_choices.clone()));
        if self.drop_in.is_some() {
            out.effects.push(Effect::SetDropInDevice {
                session: self.drop_in_session,
                kind,
                id,
            });
        }
        out.changes.push(Change::MediaDevicesChanged);
    }

    pub(super) fn join_drop_in(&mut self, conversation: ConversationId, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) || self.conversation(conversation).is_none() {
            return;
        }
        if self
            .drop_in
            .as_ref()
            .is_some_and(|h| h.conversation == conversation)
        {
            return;
        }
        self.leave_drop_in(out);
        self.drop_in_session += 1;
        self.drop_in_error = None;
        let drop_in = DropIn {
            conversation,
            status: DropInStatus::Joining,
            muted: false,
            video_on: true,
            peers: Vec::new(),
            speaking: Vec::new(),
            problem: None,
        };
        out.effects.push(Effect::JoinDropIn {
            session: self.drop_in_session,
            conversation,
            muted: drop_in.muted,
            video_on: drop_in.video_on,
            devices: self.device_choices.clone(),
        });
        // The device menu is ready by the time someone opens it.
        out.effects.push(Effect::ListMediaDevices);
        self.drop_in = Some(drop_in);
        out.changes.push(Change::DropInChanged);
    }

    pub(super) fn leave_drop_in(&mut self, out: &mut Output) {
        if self.drop_in.take().is_some() {
            out.effects.push(Effect::LeaveDropIn {
                session: self.drop_in_session,
            });
            out.changes.push(Change::DropInChanged);
        }
    }

    pub(super) fn set_drop_in_muted(&mut self, muted: bool, out: &mut Output) {
        let session = self.drop_in_session;
        let Some(drop_in) = self.drop_in.as_mut().filter(|h| h.muted != muted) else {
            return;
        };
        drop_in.muted = muted;
        if muted {
            drop_in.speaking.retain(|s| s.is_some());
        }
        out.effects.push(Effect::SetDropInMuted { session, muted });
        out.changes.push(Change::DropInChanged);
    }

    pub(super) fn set_drop_in_video(&mut self, on: bool, out: &mut Output) {
        let session = self.drop_in_session;
        let Some(drop_in) = self.drop_in.as_mut().filter(|h| h.video_on != on) else {
            return;
        };
        drop_in.video_on = on;
        out.effects.push(Effect::SetDropInVideo { session, on });
        out.changes.push(Change::DropInChanged);
    }

    pub(super) fn dismiss_drop_in_error(&mut self, out: &mut Output) {
        if self.drop_in_error.take().is_some() {
            out.changes.push(Change::DropInChanged);
        }
    }

    pub(super) fn drop_in_report(&mut self, session: u64, report: DropInReport, out: &mut Output) {
        if session != self.drop_in_session {
            return;
        }
        let Some(drop_in) = self.drop_in.as_mut() else {
            return;
        };
        match report {
            DropInReport::Joined => {
                if drop_in.status == DropInStatus::Joining {
                    drop_in.status = DropInStatus::Connecting;
                }
            }
            DropInReport::Connected => drop_in.status = DropInStatus::Live,
            DropInReport::Peers(peers) => drop_in.peers = peers,
            DropInReport::Speaking(speaking) => {
                if drop_in.speaking == speaking {
                    return;
                }
                drop_in.speaking = speaking;
            }
            DropInReport::DeviceProblem(problem) => {
                if problem.starts_with("Camera") {
                    drop_in.video_on = false;
                }
                drop_in.problem = Some(problem);
            }
            DropInReport::Ended(reason) => {
                self.drop_in = None;
                self.drop_in_error = reason;
            }
        }
        out.changes.push(Change::DropInChanged);
    }
}
