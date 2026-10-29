//! Notification settings, conversation preferences, the person's status, the clock, desktop
//! notifications, and the attention count (slice 3D, D20).
//!
//! Settings, preferences, and the status are changed optimistically: what the person sees is what
//! they asked for, saved with one idempotent `PUT` that is repeated after an unknown outcome and
//! given up on after three attempts, when what the server last said is shown again with a notice.
//! The server's events replace what it last said; a change that is not saved yet stays on screen
//! over them. Nothing here is a journaled operation: repeating a `PUT` of the same whole value is
//! always safe.

use std::collections::HashMap;

use lazy_domain::notification::{
    ConversationNotificationPrefs, IncomingMessage, NotificationSettings, NotifyLevel, UserStatus,
    decide,
};
use lazy_domain::{
    ConversationId, ConversationKind, EventSeq, MessageId, Permalink, Timestamp, UserId,
};
use lazy_protocol::{
    BootstrapResponse, ErrorCode, Message, SetConversationNotificationPrefsRequest,
    SetStatusRequest,
};

use crate::change::{Change, Output};
use crate::effect::Effect;
use crate::input::Input;
use crate::model::{RequestError, SessionPhase};
use crate::notify::{DesktopNotification, MAX_NOTIFICATION_BODY};

use super::ClientCore;

/// How many times one value is sent when the outcome is unknown.
const SAVE_ATTEMPTS: u32 = 3;

/// A value the server keeps, with a change the person made that is not saved yet.
#[derive(Debug, Clone)]
pub(super) struct Synced<T> {
    /// What the server last said.
    server: T,
    pending: Option<Pending<T>>,
}

#[derive(Debug, Clone)]
struct Pending<T> {
    /// What the person wants now.
    wish: T,
    /// What the request in flight carries.
    sent: T,
    attempts: u32,
}

/// What to do after a request's answer.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step<T> {
    /// Nothing was waiting for this.
    Stale,
    /// Settled.
    Done,
    /// Send this next.
    Send(T),
    /// Gave up: the server's value is shown again.
    GiveUp,
}

impl<T: Clone + PartialEq> Synced<T> {
    pub(super) fn new(server: T) -> Self {
        Self {
            server,
            pending: None,
        }
    }

    /// What the person sees.
    pub(super) fn shown(&self) -> &T {
        self.pending.as_ref().map_or(&self.server, |p| &p.wish)
    }

    /// The person wants `wish`. Returns the value to send now; while a request is out the wish
    /// waits for its answer.
    pub(super) fn want(&mut self, wish: T) -> Option<T> {
        match &mut self.pending {
            Some(pending) => {
                pending.wish = wish;
                None
            }
            None if wish == self.server => None,
            None => {
                self.pending = Some(Pending {
                    wish: wish.clone(),
                    sent: wish.clone(),
                    attempts: 1,
                });
                Some(wish)
            }
        }
    }

    /// An event or a bootstrap says what the server holds.
    pub(super) fn server_says(&mut self, value: T) {
        self.server = value;
    }

    pub(super) fn answered(&mut self, value: T) -> Step<T> {
        let Some(pending) = &mut self.pending else {
            self.server = value;
            return Step::Stale;
        };
        self.server = value;
        if pending.wish == pending.sent {
            self.pending = None;
            Step::Done
        } else {
            pending.sent = pending.wish.clone();
            pending.attempts = 1;
            Step::Send(pending.sent.clone())
        }
    }

    pub(super) fn lost(&mut self) -> Step<T> {
        let Some(pending) = &mut self.pending else {
            return Step::Stale;
        };
        if pending.attempts < SAVE_ATTEMPTS {
            pending.attempts += 1;
            pending.sent = pending.wish.clone();
            Step::Send(pending.sent.clone())
        } else {
            self.pending = None;
            Step::GiveUp
        }
    }

    /// The server refused: the change is dropped. False if nothing was waiting.
    pub(super) fn refused(&mut self) -> bool {
        self.pending.take().is_some()
    }

    pub(super) fn settled(&self) -> bool {
        self.pending.is_none()
    }
}

/// A notification that was shown and has not been seen.
#[derive(Debug, Clone)]
struct Held {
    message: MessageId,
    thread_root: Option<MessageId>,
    /// Whether the message is in the conversation's own list (not only in a thread).
    channel_visible: bool,
    seq: EventSeq,
}

/// What the core keeps about notifications.
#[derive(Debug)]
pub(super) struct Notifications {
    settings: Synced<NotificationSettings>,
    prefs: HashMap<ConversationId, Synced<ConversationNotificationPrefs>>,
    status: Synced<Option<UserStatus>>,
    now: Option<Timestamp>,
    held: HashMap<ConversationId, Vec<Held>>,
    /// The attention count last announced.
    badge: u32,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            settings: Synced::new(NotificationSettings::default()),
            prefs: HashMap::new(),
            status: Synced::new(None),
            now: None,
            held: HashMap::new(),
            badge: 0,
        }
    }
}

impl Notifications {
    /// The time the core was last told.
    pub(super) fn clock(&self) -> Option<Timestamp> {
        self.now
    }

    /// Exchanges what belongs to one workspace (the settings and the person's status there) and
    /// leaves what is shared by all (preferences for conversations, the clock, the notifications
    /// held, the badge).
    pub(super) fn swap_scoped(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.settings, &mut other.settings);
        std::mem::swap(&mut self.status, &mut other.status);
    }
}

fn default_prefs(conversation: ConversationId) -> ConversationNotificationPrefs {
    ConversationNotificationPrefs {
        conversation_id: conversation,
        level: None,
        muted: false,
    }
}

impl ClientCore {
    // ----- Reading -----

    /// The person's notification settings for the workspace, including a change that is not saved
    /// yet.
    pub fn notification_settings(&self) -> &NotificationSettings {
        self.notif.settings.shown()
    }

    /// The person's own preference for a conversation, if they have set one.
    pub fn conversation_notification_prefs(
        &self,
        conversation: ConversationId,
    ) -> Option<&ConversationNotificationPrefs> {
        self.notif
            .prefs
            .get(&conversation)
            .map(Synced::shown)
            .filter(|prefs| prefs.level.is_some() || prefs.muted)
    }

    /// How much of a conversation notifies: its own level, else the workspace's for its kind.
    pub fn effective_level(&self, conversation: ConversationId) -> NotifyLevel {
        if let Some(level) = self
            .conversation_notification_prefs(conversation)
            .and_then(|prefs| prefs.level)
        {
            return level;
        }
        let settings = self.notification_settings();
        match self.conversation(conversation).map(|c| c.kind) {
            Some(ConversationKind::Channel) | None => settings.channel_level,
            Some(_) => settings.direct_level,
        }
    }

    pub fn is_muted(&self, conversation: ConversationId) -> bool {
        self.conversation_notification_prefs(conversation)
            .is_some_and(|prefs| prefs.muted)
    }

    /// Whether notifications are paused now, by a snooze or by quiet hours, at the core's clock
    /// (`Input::Clock`). False until the clock has been set.
    pub fn notifications_paused(&self) -> bool {
        self.notif
            .now
            .is_some_and(|now| self.notification_settings().is_paused(now))
    }

    /// A person's status, unless it has ended at the core's clock. The person's own includes a
    /// change that is not saved yet.
    pub fn status_of(&self, user: UserId) -> Option<&UserStatus> {
        let status = if self.me.as_ref().is_some_and(|me| me.id == user) {
            self.notif.status.shown().as_ref()
        } else {
            self.users.get(&user)?.status.as_ref()
        };
        status.filter(|status| self.notif.now.is_none_or(|now| status.is_active(now)))
    }

    /// Checks a status the person typed, in words they can read: the text, a custom emoji that is
    /// still live, and an end that has not passed.
    pub fn make_status(
        &self,
        text: &str,
        emoji: Option<lazy_domain::ReactionEmoji>,
        expires_at: Option<Timestamp>,
    ) -> Result<UserStatus, String> {
        let status = UserStatus::new(text, emoji, expires_at).map_err(|error| match error {
            lazy_domain::notification::StatusError::Empty => "Write what you are up to.".to_owned(),
            lazy_domain::notification::StatusError::TooLong => {
                "A status can be at most 100 characters.".to_owned()
            }
            lazy_domain::notification::StatusError::ControlCharacter => {
                "A status can't contain line breaks or other control characters.".to_owned()
            }
        })?;
        self.check_status(&status)?;
        Ok(status)
    }

    fn check_status(&self, status: &UserStatus) -> Result<(), String> {
        if let Some(id) = status.emoji.as_ref().and_then(|emoji| emoji.custom_id())
            && !self.custom_emoji_is_live(id)
        {
            return Err("That emoji was deleted. Pick another.".to_owned());
        }
        if let (Some(now), Some(end)) = (self.notif.now, status.expires_at)
            && end <= now
        {
            return Err("That time has already passed.".to_owned());
        }
        Ok(())
    }

    /// What needs the person's attention: the mentions in channels and the unread messages of
    /// direct and group conversations (a mention there is already one of them), both unmuted,
    /// plus the unread replies of followed threads in unmuted conversations. Shown in the window
    /// title and as the launcher badge.
    pub fn attention_count(&self) -> u32 {
        self.attention_of(&self.conversations)
    }

    // ----- Inputs -----

    pub(super) fn notify_input(&mut self, input: Input, out: &mut Output) {
        match input {
            Input::Clock(now) => self.set_clock(now, out),
            Input::SetNotificationSettings(settings) => self.set_settings(settings, out),
            Input::PauseNotifications { until } => {
                let mut settings = *self.notification_settings();
                settings.dnd_until = until;
                self.set_settings(settings, out);
            }
            Input::SetConversationNotificationPrefs {
                conversation,
                level,
                muted,
            } => self.set_prefs(conversation, level, muted, out),
            Input::SetStatus(status) => self.set_status(status, out),
            Input::NotificationSettingsResult { result, .. } => self.settings_result(result, out),
            Input::ConversationPrefsResult {
                conversation,
                result,
            } => self.prefs_result(conversation, result, out),
            Input::StatusResult { result, .. } => self.save_status_result(result, out),
            Input::NotificationActivated {
                conversation,
                message,
            } => self.notification_activated(conversation, message, out),
            _ => {}
        }
    }

    fn set_clock(&mut self, now: Timestamp, out: &mut Output) {
        let before = self.notif.now;
        let paused_before = self.notifications_paused();
        self.notif.now = Some(now);
        if self.notifications_paused() != paused_before {
            out.changes.push(Change::NotificationsChanged);
        }
        // A status ends without an event: tell the screen when the clock passes its end.
        let ended = |status: &Option<UserStatus>| {
            status.as_ref().is_some_and(|status| {
                status
                    .expires_at
                    .is_some_and(|end| end <= now && before.is_none_or(|earlier| end > earlier))
            })
        };
        let mut changed = ended(self.notif.status.shown());
        changed |= self.users.values().any(|user| ended(&user.status));
        if changed {
            out.changes.push(Change::StatusChanged);
        }
    }

    fn ready_for_settings(&self) -> bool {
        matches!(self.phase, SessionPhase::Ready)
    }

    fn set_settings(&mut self, settings: NotificationSettings, out: &mut Output) {
        if !self.ready_for_settings() {
            return;
        }
        if let Some(hours) = settings.quiet_hours
            && !hours.is_valid()
        {
            let notice = "Quiet hours need different start and end times, at least one day, \
                          and a time zone within 14 hours of UTC.";
            return self.set_notice(Some(notice.to_owned()), out);
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let before = *self.notif.settings.shown();
        let send = self.notif.settings.want(settings);
        if *self.notif.settings.shown() != before {
            out.changes.push(Change::NotificationsChanged);
        }
        if let Some(settings) = send {
            out.effects.push(Effect::SetNotificationSettings {
                workspace,
                settings,
            });
        }
    }

    fn settings_result(
        &mut self,
        result: Result<lazy_protocol::NotificationSettingsResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let before = *self.notif.settings.shown();
        let step = match result {
            Ok(response) => self.notif.settings.answered(response.settings),
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(RequestError::Rejected(api)) => {
                if self.notif.settings.refused() {
                    self.set_notice(Some(refusal_text(&api, "notification settings")), out);
                }
                Step::Done
            }
            Err(RequestError::Unknown(_)) => self.notif.settings.lost(),
        };
        match step {
            Step::Send(settings) => out.effects.push(Effect::SetNotificationSettings {
                workspace,
                settings,
            }),
            Step::GiveUp => self.set_notice(Some(give_up_text("notification settings")), out),
            Step::Stale | Step::Done => {}
        }
        if *self.notif.settings.shown() != before {
            out.changes.push(Change::NotificationsChanged);
        }
    }

    fn set_prefs(
        &mut self,
        conversation: ConversationId,
        level: Option<NotifyLevel>,
        muted: bool,
        out: &mut Output,
    ) {
        if !self.ready_for_settings() || self.conversation(conversation).is_none() {
            return;
        }
        let wish = ConversationNotificationPrefs {
            conversation_id: conversation,
            level,
            muted,
        };
        let entry = self
            .notif
            .prefs
            .entry(conversation)
            .or_insert_with(|| Synced::new(default_prefs(conversation)));
        let before = *entry.shown();
        let send = entry.want(wish);
        if *entry.shown() != before {
            out.changes.push(Change::NotificationsChanged);
        }
        if let Some(prefs) = send {
            out.effects.push(Effect::SetConversationNotificationPrefs {
                conversation,
                request: SetConversationNotificationPrefsRequest {
                    level: prefs.level,
                    muted: prefs.muted,
                },
            });
        }
        self.tidy_prefs(conversation);
    }

    /// Forgets the record of a conversation whose preference is the default and settled.
    fn tidy_prefs(&mut self, conversation: ConversationId) {
        if self.notif.prefs.get(&conversation).is_some_and(|synced| {
            synced.settled() && *synced.shown() == default_prefs(conversation)
        }) {
            self.notif.prefs.remove(&conversation);
        }
    }

    fn prefs_result(
        &mut self,
        conversation: ConversationId,
        result: Result<lazy_protocol::ConversationNotificationPrefsResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(entry) = self.notif.prefs.get_mut(&conversation) else {
            return;
        };
        let before = *entry.shown();
        let mut notice = None;
        let step = match result {
            Ok(response) => entry.answered(response.prefs),
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(RequestError::Rejected(api)) => {
                if entry.refused() {
                    notice = Some(refusal_text(
                        &api,
                        "that conversation's notification setting",
                    ));
                }
                Step::Done
            }
            Err(RequestError::Unknown(_)) => entry.lost(),
        };
        let after = *entry.shown();
        match step {
            Step::Send(prefs) => out.effects.push(Effect::SetConversationNotificationPrefs {
                conversation,
                request: SetConversationNotificationPrefsRequest {
                    level: prefs.level,
                    muted: prefs.muted,
                },
            }),
            Step::GiveUp => {
                notice = Some(give_up_text("that conversation's notification setting"));
            }
            Step::Stale | Step::Done => {}
        }
        if after != before {
            out.changes.push(Change::NotificationsChanged);
        }
        self.tidy_prefs(conversation);
        if notice.is_some() {
            self.set_notice(notice, out);
        }
    }

    fn set_status(&mut self, status: Option<UserStatus>, out: &mut Output) {
        if !self.ready_for_settings() {
            return;
        }
        if let Some(status) = &status
            && let Err(reason) = self.check_status(status)
        {
            return self.set_notice(Some(reason), out);
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let before = self.notif.status.shown().clone();
        let send = self.notif.status.want(status);
        self.my_status_changed(before, out);
        if let Some(status) = send {
            out.effects.push(Effect::SetStatus {
                workspace,
                request: SetStatusRequest { status },
            });
        }
    }

    /// Keeps the person's own entry in the member list at what they see.
    fn my_status_changed(&mut self, before: Option<UserStatus>, out: &mut Output) {
        let shown = self.notif.status.shown().clone();
        if let Some(me) = self.me.as_ref().map(|me| me.id)
            && let Some(user) = self.users.get_mut(&me)
        {
            user.status.clone_from(&shown);
        }
        if let Some(me) = self.me.as_mut() {
            me.status.clone_from(&shown);
        }
        if shown != before {
            out.changes.push(Change::StatusChanged);
        }
    }

    fn save_status_result(
        &mut self,
        result: Result<lazy_protocol::StatusResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let before = self.notif.status.shown().clone();
        let mut notice = None;
        let step = match result {
            Ok(response) => self.notif.status.answered(response.user.status),
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(RequestError::Rejected(api)) => {
                if self.notif.status.refused() {
                    notice = Some(refusal_text(&api, "your status"));
                }
                Step::Done
            }
            Err(RequestError::Unknown(_)) => self.notif.status.lost(),
        };
        match step {
            Step::Send(status) => out.effects.push(Effect::SetStatus {
                workspace,
                request: SetStatusRequest { status },
            }),
            Step::GiveUp => notice = Some(give_up_text("your status")),
            Step::Stale | Step::Done => {}
        }
        self.my_status_changed(before, out);
        if notice.is_some() {
            self.set_notice(notice, out);
        }
    }

    fn notification_activated(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        out: &mut Output,
    ) {
        let Some(workspace) = self.conversation(conversation).map(|c| c.workspace_id) else {
            return;
        };
        self.open_permalink(
            Permalink {
                workspace_id: workspace,
                conversation_id: conversation,
                message_id: message,
            },
            out,
        );
    }

    // ----- Events and the bootstrap -----

    pub(super) fn event_notification_settings(
        &mut self,
        workspace: lazy_domain::WorkspaceId,
        settings: NotificationSettings,
        out: &mut Output,
    ) {
        if self.primary_workspace_id() != Some(workspace) {
            return;
        }
        let before = *self.notif.settings.shown();
        self.notif.settings.server_says(settings);
        if *self.notif.settings.shown() != before {
            out.changes.push(Change::NotificationsChanged);
        }
    }

    pub(super) fn event_conversation_prefs(
        &mut self,
        prefs: ConversationNotificationPrefs,
        out: &mut Output,
    ) {
        let conversation = prefs.conversation_id;
        if self.conversation(conversation).is_none() {
            return;
        }
        let entry = self
            .notif
            .prefs
            .entry(conversation)
            .or_insert_with(|| Synced::new(default_prefs(conversation)));
        let before = *entry.shown();
        entry.server_says(prefs);
        if *entry.shown() != before {
            out.changes.push(Change::NotificationsChanged);
        }
        self.tidy_prefs(conversation);
    }

    pub(super) fn event_user_status(
        &mut self,
        user: UserId,
        status: Option<UserStatus>,
        out: &mut Output,
    ) {
        if self.me.as_ref().is_some_and(|me| me.id == user) {
            let before = self.notif.status.shown().clone();
            self.notif.status.server_says(status.clone());
            self.my_status_changed(before, out);
        } else if let Some(known) = self.users.get_mut(&user)
            && known.status != status
        {
            known.status.clone_from(&status);
            out.changes.push(Change::StatusChanged);
        }
        self.discovery_status_changed(user, &status, out);
    }

    /// Takes the settings, preferences, and status out of a bootstrap. A change that is not saved
    /// yet stays on screen.
    /// Takes the person's notification settings and status in one workspace out of its bootstrap
    /// (the loaded scope is that workspace's). `status` is what the workspace's member list says
    /// of the person, if it says anything.
    pub(super) fn adopt_workspace_settings(
        &mut self,
        workspace: &lazy_protocol::WorkspaceBootstrap,
        status: Option<Option<UserStatus>>,
        out: &mut Output,
    ) {
        self.notif
            .settings
            .server_says(workspace.notification_settings);
        let before = self.notif.status.shown().clone();
        self.notif
            .status
            .server_says(status.unwrap_or_else(|| before.clone()));
        self.my_status_changed(before, out);
        out.changes.push(Change::NotificationsChanged);
    }

    /// Takes the person's preferences for conversations out of a bootstrap (they are keyed by
    /// conversation, so every workspace's together).
    pub(super) fn adopt_bootstrap_prefs(&mut self, response: &BootstrapResponse, out: &mut Output) {
        let listed: HashMap<ConversationId, ConversationNotificationPrefs> = response
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.conversation_notification_prefs.iter())
            .map(|prefs| (prefs.conversation_id, *prefs))
            .collect();
        for (conversation, synced) in &mut self.notif.prefs {
            synced.server_says(
                listed
                    .get(conversation)
                    .copied()
                    .unwrap_or_else(|| default_prefs(*conversation)),
            );
        }
        for (conversation, prefs) in listed {
            self.notif
                .prefs
                .entry(conversation)
                .or_insert_with(|| Synced::new(prefs));
        }
        self.notif.prefs.retain(|conversation, synced| {
            !(synced.settled() && *synced.shown() == default_prefs(*conversation))
        });
        out.changes.push(Change::NotificationsChanged);
    }

    /// Forgets everything (sign-out, or another person signing in).
    pub(super) fn clear_notifications(&mut self, out: &mut Output) {
        let held: Vec<ConversationId> = self.notif.held.keys().copied().collect();
        for conversation in held {
            out.effects
                .push(Effect::ClearNotifications { conversation });
        }
        let now = self.notif.now;
        let badge = self.notif.badge;
        self.notif = Notifications::default();
        self.notif.now = now;
        if badge != 0 {
            out.effects.push(Effect::SetBadge(0));
            out.changes.push(Change::AttentionChanged);
        }
        out.changes.push(Change::NotificationsChanged);
    }

    // ----- Desktop notifications -----

    /// A message arrived by an event. Notifies if the rules say so and the person is not looking
    /// at it. `live` says the connection had caught up: replayed history never notifies.
    pub(super) fn notify_message(
        &mut self,
        message: &Message,
        occurred_at: Timestamp,
        live: bool,
        out: &mut Output,
    ) {
        // While the cached content has not been confirmed, the person's notification settings
        // (which the cache does not hold) are not known: nothing is shown on a guess.
        if !live || message.deleted || !self.settings_known {
            return;
        }
        let Some(me) = self.me.as_ref().map(|me| me.id) else {
            return;
        };
        let Some(conversation) = self.conversation(message.conversation_id).cloned() else {
            return;
        };
        let incoming = IncomingMessage {
            author: message.author_id,
            conversation_kind: conversation.kind,
            is_reply: message.thread_root.is_some(),
            in_channel: message.in_channel,
            mentions_me: message.mentions.users.contains(&me),
            mentions_channel: message.mentions.channel,
        };
        let following = message
            .thread_root
            .and_then(|root| self.thread_states.get(&root))
            .is_some_and(|state| state.following);
        let settings = *self.notif.settings.shown();
        let prefs = self
            .conversation_notification_prefs(conversation.id)
            .copied();
        let Some(decision) = decide(
            &incoming,
            me,
            &settings,
            prefs.as_ref(),
            following,
            occurred_at,
        ) else {
            return;
        };
        if decision.paused || !settings.desktop {
            return;
        }
        // The person is looking at it: in the conversation, or in the open thread.
        let channel_traffic = message.thread_root.is_none() || message.in_channel;
        let in_open_thread = message
            .thread_root
            .is_some_and(|root| self.open_thread().is_some_and(|open| open.root == root));
        if self.window_active
            && self.selected_conversation() == Some(conversation.id)
            && (channel_traffic || in_open_thread)
        {
            return;
        }
        let sender = self.display_name(message.author_id).to_owned();
        let title = match conversation.kind {
            ConversationKind::Direct => sender.clone(),
            ConversationKind::Channel => format!("#{}", self.title_of(&conversation)),
            ConversationKind::Group => self.title_of(&conversation),
        };
        // From a workspace in the background, the title says whose it is (when there are several).
        let title = match self.primary_workspace() {
            Some(info) if self.scope != self.active && self.workspaces.len() > 1 => {
                format!("{} · {title}", info.workspace.name)
            }
            _ => title,
        };
        let held = self.notif.held.entry(conversation.id).or_default();
        held.push(Held {
            message: message.id,
            thread_root: message.thread_root,
            channel_visible: channel_traffic,
            seq: message.seq,
        });
        let count = u32::try_from(held.len()).unwrap_or(u32::MAX);
        let line = if settings.show_content {
            Some(format!("{sender}: {}", message_text(message)))
        } else if conversation.kind == ConversationKind::Direct {
            Some("New message".to_owned())
        } else {
            Some(format!("{sender}: New message"))
        };
        let line = line.unwrap_or_default();
        let body = if count > 1 {
            if settings.show_content {
                shorten(&format!("{count} new messages. Latest: {line}"))
            } else {
                format!("{count} new messages")
            }
        } else {
            shorten(&line)
        };
        out.effects.push(Effect::Notify(DesktopNotification {
            conversation: conversation.id,
            message: message.id,
            thread_root: message.thread_root,
            title,
            body,
            reason: decision.reason,
            tag: conversation.id.to_string(),
            count,
        }));
    }

    /// A message was deleted: its notification, if one is still held, is withdrawn.
    pub(super) fn notify_message_deleted(&mut self, message: MessageId, out: &mut Output) {
        let mut found = None;
        for (conversation, held) in &mut self.notif.held {
            if let Some(index) = held.iter().position(|entry| entry.message == message) {
                held.remove(index);
                found = Some((*conversation, held.is_empty()));
                break;
            }
        }
        if let Some((conversation, empty)) = found {
            if empty {
                self.notif.held.remove(&conversation);
            }
            out.effects.push(Effect::RetractNotification { message });
        }
    }

    /// The person no longer has the conversation: its notifications go.
    pub(super) fn notify_conversation_gone(
        &mut self,
        conversation: ConversationId,
        out: &mut Output,
    ) {
        self.notif.prefs.remove(&conversation);
        if self.notif.held.remove(&conversation).is_some() {
            out.effects
                .push(Effect::ClearNotifications { conversation });
        }
    }

    /// After every input: notifications of what has been read are withdrawn, and the attention
    /// count is announced if it changed.
    pub(super) fn sync_attention(&mut self, out: &mut Output) {
        if !self.notif.held.is_empty() {
            let conversations: Vec<ConversationId> = self.notif.held.keys().copied().collect();
            for conversation in conversations {
                self.clear_seen_notifications(conversation, out);
            }
        }
        let count = self.attention_total();
        if count != self.notif.badge {
            self.notif.badge = count;
            out.effects.push(Effect::SetBadge(count));
            out.changes.push(Change::AttentionChanged);
        }
    }

    fn clear_seen_notifications(&mut self, conversation: ConversationId, out: &mut Output) {
        let looking = self.window_active
            && self.selected_conversation() == Some(conversation)
            && self.at_bottom.contains(&conversation);
        let read_to = self
            .user_states
            .get(&conversation)
            .map(|state| state.last_read);
        let open_root = self
            .open_thread()
            .filter(|open| open.conversation == conversation)
            .map(|open| open.root);
        let Some(held) = self.notif.held.get(&conversation) else {
            return;
        };
        let kept: Vec<Held> = held
            .iter()
            .filter(|entry| {
                let seen = if entry.channel_visible {
                    looking || read_to.is_some_and(|read| read >= entry.seq)
                } else {
                    let root = entry.thread_root;
                    let read = root
                        .and_then(|root| self.thread_states.get(&root))
                        .is_some_and(|state| state.last_read >= entry.seq);
                    let watching = self.window_active
                        && root.is_some()
                        && root == open_root
                        && root.is_some_and(|root| self.thread_at_bottom.contains(&root));
                    read || watching
                };
                !seen
            })
            .cloned()
            .collect();
        if kept.len() == held.len() {
            return;
        }
        if kept.is_empty() {
            self.notif.held.remove(&conversation);
            out.effects
                .push(Effect::ClearNotifications { conversation });
        } else {
            self.notif.held.insert(conversation, kept);
        }
    }
}

/// The text of a message for a notification: its words on one line, or its files.
fn message_text(message: &Message) -> String {
    let text = message.document.plain_text();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() && !message.attachments.is_empty() {
        let count = message.attachments.len();
        return if count == 1 {
            "[1 file]".to_owned()
        } else {
            format!("[{count} files]")
        };
    }
    text
}

/// At most [`MAX_NOTIFICATION_BODY`] characters.
fn shorten(text: &str) -> String {
    if text.chars().count() <= MAX_NOTIFICATION_BODY {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(MAX_NOTIFICATION_BODY - 1).collect();
    short.push('…');
    short
}

pub(super) fn refusal_text(api: &lazy_protocol::ApiError, what: &str) -> String {
    match api.code {
        ErrorCode::InvalidRequest => api.message.clone(),
        ErrorCode::Forbidden | ErrorCode::NotFound => {
            format!("Couldn't save {what}: you can't change it any more.")
        }
        ErrorCode::RateLimited => {
            format!("Couldn't save {what}: too many changes. Try again in a moment.")
        }
        _ => format!("Couldn't save {what}. Try again."),
    }
}

pub(super) fn give_up_text(what: &str) -> String {
    format!("Couldn't save {what}: the server could not be reached. The earlier value is back.")
}
