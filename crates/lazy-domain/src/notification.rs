//! Who is told about a message, and how loudly: the rules, as one pure function.
//!
//! Clients evaluate [`decide`] for every message they receive live, to show a desktop
//! notification; a push worker will evaluate the same function on the server for devices that are
//! not connected. Keeping the rules here, with no I/O and no clock of their own, is what lets both
//! agree and lets the rules be tested exhaustively. The server stores the preferences; it does not
//! store a notification per recipient (a message to a large channel would write a row for every
//! member).

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, Weekday};

use crate::{ConversationId, ConversationKind, ReactionEmoji, Timestamp, UserId};

/// Longest status text, in characters.
pub const MAX_STATUS_CHARS: usize = 100;

/// Why a status text was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StatusError {
    #[error("a status needs some text")]
    Empty,
    #[error("a status can be at most {MAX_STATUS_CHARS} characters")]
    TooLong,
    #[error("a status cannot contain control characters")]
    ControlCharacter,
}

/// What a person says they are doing: "In a meeting", with an emoji, until a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "StatusWire", into = "StatusWire")]
pub struct UserStatus {
    text: String,
    pub emoji: Option<ReactionEmoji>,
    /// The status is shown until this moment and then disappears; `None` keeps it until changed.
    pub expires_at: Option<Timestamp>,
}

#[derive(Serialize, Deserialize)]
struct StatusWire {
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    emoji: Option<ReactionEmoji>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<Timestamp>,
}

impl UserStatus {
    /// A status with `text` (trimmed, 1 to [`MAX_STATUS_CHARS`] characters, no control characters).
    pub fn new(
        text: &str,
        emoji: Option<ReactionEmoji>,
        expires_at: Option<Timestamp>,
    ) -> Result<Self, StatusError> {
        let trimmed = text.trim();
        let count = trimmed.chars().count();
        if count == 0 {
            return Err(StatusError::Empty);
        }
        if count > MAX_STATUS_CHARS {
            return Err(StatusError::TooLong);
        }
        if trimmed.chars().any(char::is_control) {
            return Err(StatusError::ControlCharacter);
        }
        Ok(Self {
            text: trimmed.to_owned(),
            emoji,
            expires_at,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the status is still showing at `now`.
    pub fn is_active(&self, now: Timestamp) -> bool {
        self.expires_at.is_none_or(|until| until > now)
    }
}

impl TryFrom<StatusWire> for UserStatus {
    type Error = StatusError;

    fn try_from(wire: StatusWire) -> Result<Self, StatusError> {
        UserStatus::new(&wire.text, wire.emoji, wire.expires_at)
    }
}

impl From<UserStatus> for StatusWire {
    fn from(status: UserStatus) -> Self {
        StatusWire {
            text: status.text,
            emoji: status.emoji,
            expires_at: status.expires_at,
        }
    }
}

/// How much of a conversation's traffic should notify.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyLevel {
    /// Every message.
    All,
    /// Only what concerns the person: being named, `@channel`, and threads they follow.
    #[default]
    Mentions,
    /// Nothing, whatever it is.
    Nothing,
}

/// Daily hours when notifications are silenced, in the person's own time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    /// Minutes after local midnight when quiet starts and ends. If `end` is not after `start`
    /// the hours run past midnight (22:00 to 07:00 is 1320 to 420).
    pub start_minute: u16,
    pub end_minute: u16,
    /// The person's offset from UTC, in minutes, when they set it (so the server and every device
    /// agree on what "22:00" means without knowing the person's time zone database).
    pub utc_offset_minutes: i16,
    /// The days quiet starts on, a bit mask: bit 0 is Monday through bit 6 for Sunday.
    pub days: u8,
}

impl QuietHours {
    pub const EVERY_DAY: u8 = 0b0111_1111;

    pub fn is_valid(&self) -> bool {
        self.start_minute < 1440
            && self.end_minute < 1440
            && self.start_minute != self.end_minute
            && self.days & !Self::EVERY_DAY == 0
            && self.days != 0
            && (-14 * 60..=14 * 60).contains(&self.utc_offset_minutes)
    }

    /// Whether `now` falls inside the quiet hours.
    pub fn contains(&self, now: Timestamp) -> bool {
        let Ok(offset) =
            time::UtcOffset::from_whole_seconds(i32::from(self.utc_offset_minutes) * 60)
        else {
            return false;
        };
        let local: OffsetDateTime = now.as_offset_date_time().to_offset(offset);
        let minute = u16::from(local.hour()) * 60 + u16::from(local.minute());
        let weekday_bit = |weekday: Weekday| 1u8 << weekday.number_days_from_monday();
        let spans_midnight = self.end_minute <= self.start_minute;
        if !spans_midnight {
            minute >= self.start_minute
                && minute < self.end_minute
                && self.days & weekday_bit(local.weekday()) != 0
        } else if minute >= self.start_minute {
            // The evening part belongs to today's start.
            self.days & weekday_bit(local.weekday()) != 0
        } else if minute < self.end_minute {
            // The morning part belongs to the start on the previous day.
            self.days & weekday_bit(local.weekday().previous()) != 0
        } else {
            false
        }
    }
}

/// A person's notification settings for one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationSettings {
    /// For channels with no preference of their own.
    pub channel_level: NotifyLevel,
    /// For direct and group conversations with no preference of their own.
    pub direct_level: NotifyLevel,
    /// Whether replies in threads the person follows notify (unless the conversation says
    /// `nothing` or is muted).
    pub thread_replies: bool,
    /// Whether `@channel` notifies.
    pub channel_mentions: bool,
    /// Whether this device shows desktop notifications at all.
    pub desktop: bool,
    /// Whether a desktop notification shows the message's text (off: only who and where).
    pub show_content: bool,
    /// Notifications are paused until this moment.
    pub dnd_until: Option<Timestamp>,
    pub quiet_hours: Option<QuietHours>,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            channel_level: NotifyLevel::Mentions,
            direct_level: NotifyLevel::All,
            thread_replies: true,
            channel_mentions: true,
            desktop: true,
            show_content: true,
            dnd_until: None,
            quiet_hours: None,
        }
    }
}

impl NotificationSettings {
    /// Whether notifications are paused at `now`, by a snooze or by quiet hours.
    pub fn is_paused(&self, now: Timestamp) -> bool {
        self.dnd_until.is_some_and(|until| until > now)
            || self.quiet_hours.is_some_and(|hours| hours.contains(now))
    }
}

/// A person's preference for one conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationNotificationPrefs {
    pub conversation_id: ConversationId,
    /// `None` follows the workspace setting for this kind of conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<NotifyLevel>,
    /// Muted: no notifications at all, and its unread messages do not count toward badges.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
}

/// What a message is, as far as the rules care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncomingMessage {
    pub author: UserId,
    pub conversation_kind: ConversationKind,
    /// Whether it is a reply in a thread.
    pub is_reply: bool,
    /// For a reply: whether it was also sent to the channel.
    pub in_channel: bool,
    /// Whether it names the person (directly or through a group).
    pub mentions_me: bool,
    /// Whether it used `@channel`.
    pub mentions_channel: bool,
}

/// Why a message notifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationReason {
    /// The person is named.
    Mention,
    /// A reply in a thread the person follows.
    ThreadReply,
    /// `@channel`.
    ChannelMention,
    /// A direct or group message.
    Direct,
    /// A message in a conversation set to notify for everything.
    AllMessages,
}

/// That a message should notify, and whether notifications are paused right now (it still counts
/// as something that happened, but nothing should pop up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub reason: NotificationReason,
    pub paused: bool,
}

/// Decides whether `message` notifies `me`.
///
/// * Your own messages never do. A muted conversation never does.
/// * The conversation's own level applies, else the workspace's for its kind. `nothing` silences
///   everything, including mentions and threads.
/// * Being named notifies at `mentions` and above; `@channel` too if the setting allows; a reply in
///   a thread you follow too if the setting allows, whatever the channel's level.
/// * A direct or group message notifies at `all` (at `mentions`, only when it names you).
/// * A channel message notifies at `all`. A reply that was not sent to the channel is not a
///   channel message: it can notify only as a mention or a followed thread.
/// * Pausing (a snooze or quiet hours) does not remove the notification, it marks it paused.
pub fn decide(
    message: &IncomingMessage,
    me: UserId,
    settings: &NotificationSettings,
    conversation: Option<&ConversationNotificationPrefs>,
    following_thread: bool,
    now: Timestamp,
) -> Option<Decision> {
    if message.author == me || conversation.is_some_and(|prefs| prefs.muted) {
        return None;
    }
    let direct = message.conversation_kind != ConversationKind::Channel;
    let level = conversation
        .and_then(|prefs| prefs.level)
        .unwrap_or(if direct {
            settings.direct_level
        } else {
            settings.channel_level
        });
    if level == NotifyLevel::Nothing {
        return None;
    }
    let channel_traffic = !message.is_reply || message.in_channel;
    let reason = if message.mentions_me {
        Some(NotificationReason::Mention)
    } else if message.is_reply && following_thread && settings.thread_replies {
        Some(NotificationReason::ThreadReply)
    } else if message.mentions_channel && settings.channel_mentions {
        Some(NotificationReason::ChannelMention)
    } else if level == NotifyLevel::All && channel_traffic {
        Some(if direct {
            NotificationReason::Direct
        } else {
            NotificationReason::AllMessages
        })
    } else {
        None
    }?;
    Some(Decision {
        reason,
        paused: settings.is_paused(now),
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use time::macros::datetime;

    use super::*;

    fn at(value: OffsetDateTime) -> Timestamp {
        Timestamp::from(value)
    }

    fn message(kind: ConversationKind) -> IncomingMessage {
        IncomingMessage {
            author: UserId::generate(),
            conversation_kind: kind,
            is_reply: false,
            in_channel: false,
            mentions_me: false,
            mentions_channel: false,
        }
    }

    fn decide_with(
        message: &IncomingMessage,
        settings: &NotificationSettings,
        conversation: Option<&ConversationNotificationPrefs>,
        following: bool,
    ) -> Option<NotificationReason> {
        decide(
            message,
            UserId::generate(),
            settings,
            conversation,
            following,
            at(datetime!(2026-10-28 12:00 UTC)),
        )
        .map(|d| d.reason)
    }

    fn prefs(level: Option<NotifyLevel>, muted: bool) -> ConversationNotificationPrefs {
        ConversationNotificationPrefs {
            conversation_id: ConversationId::generate(),
            level,
            muted,
        }
    }

    #[test]
    fn defaults_notify_for_direct_messages_and_mentions_only() {
        let settings = NotificationSettings::default();
        let channel = message(ConversationKind::Channel);
        assert_eq!(decide_with(&channel, &settings, None, false), None);
        for kind in [ConversationKind::Direct, ConversationKind::Group] {
            assert_eq!(
                decide_with(&message(kind), &settings, None, false),
                Some(NotificationReason::Direct)
            );
        }
        let named = IncomingMessage {
            mentions_me: true,
            ..channel
        };
        assert_eq!(
            decide_with(&named, &settings, None, false),
            Some(NotificationReason::Mention)
        );
        let everyone = IncomingMessage {
            mentions_channel: true,
            ..channel
        };
        assert_eq!(
            decide_with(&everyone, &settings, None, false),
            Some(NotificationReason::ChannelMention)
        );
    }

    #[test]
    fn your_own_messages_and_muted_conversations_never_notify() {
        let settings = NotificationSettings {
            channel_level: NotifyLevel::All,
            ..Default::default()
        };
        let me = UserId::generate();
        let own = IncomingMessage {
            author: me,
            mentions_me: true,
            ..message(ConversationKind::Direct)
        };
        let now = at(datetime!(2026-10-28 12:00 UTC));
        assert_eq!(decide(&own, me, &settings, None, true, now), None);
        let named = IncomingMessage {
            mentions_me: true,
            mentions_channel: true,
            ..message(ConversationKind::Channel)
        };
        assert_eq!(
            decide_with(&named, &settings, Some(&prefs(None, true)), true),
            None
        );
        // Muting beats a level of all.
        assert_eq!(
            decide_with(
                &named,
                &settings,
                Some(&prefs(Some(NotifyLevel::All), true)),
                false
            ),
            None
        );
    }

    #[test]
    fn a_conversations_own_level_overrides_the_workspaces() {
        let settings = NotificationSettings::default();
        let channel = message(ConversationKind::Channel);
        assert_eq!(
            decide_with(
                &channel,
                &settings,
                Some(&prefs(Some(NotifyLevel::All), false)),
                false
            ),
            Some(NotificationReason::AllMessages)
        );
        // Nothing silences even a mention, a channel mention, and a followed thread.
        let busy = IncomingMessage {
            mentions_me: true,
            mentions_channel: true,
            is_reply: true,
            ..channel
        };
        assert_eq!(
            decide_with(
                &busy,
                &settings,
                Some(&prefs(Some(NotifyLevel::Nothing), false)),
                true
            ),
            None
        );
        // A direct message at mentions notifies only if it names you.
        let dm = message(ConversationKind::Direct);
        let quiet = prefs(Some(NotifyLevel::Mentions), false);
        assert_eq!(decide_with(&dm, &settings, Some(&quiet), false), None);
        let named = IncomingMessage {
            mentions_me: true,
            ..dm
        };
        assert_eq!(
            decide_with(&named, &settings, Some(&quiet), false),
            Some(NotificationReason::Mention)
        );
    }

    #[test]
    fn replies_notify_through_threads_and_mentions_not_as_channel_traffic() {
        let settings = NotificationSettings {
            channel_level: NotifyLevel::All,
            ..Default::default()
        };
        let reply = IncomingMessage {
            is_reply: true,
            ..message(ConversationKind::Channel)
        };
        // A thread-only reply in a channel at `all` is not channel traffic.
        assert_eq!(decide_with(&reply, &settings, None, false), None);
        assert_eq!(
            decide_with(&reply, &settings, None, true),
            Some(NotificationReason::ThreadReply)
        );
        // Unless the person turned thread notifications off.
        let no_threads = NotificationSettings {
            thread_replies: false,
            ..settings
        };
        assert_eq!(decide_with(&reply, &no_threads, None, true), None);
        // Sent also to the channel, it is channel traffic too.
        let also = IncomingMessage {
            in_channel: true,
            ..reply
        };
        assert_eq!(
            decide_with(&also, &no_threads, None, false),
            Some(NotificationReason::AllMessages)
        );
        // A followed thread notifies even where the channel is at mentions.
        let mentions_only = NotificationSettings::default();
        assert_eq!(
            decide_with(&reply, &mentions_only, None, true),
            Some(NotificationReason::ThreadReply)
        );
        // Being named in a reply notifies whether or not the thread is followed.
        let named = IncomingMessage {
            mentions_me: true,
            ..reply
        };
        assert_eq!(
            decide_with(&named, &mentions_only, None, false),
            Some(NotificationReason::Mention)
        );
    }

    #[test]
    fn channel_mentions_can_be_turned_off() {
        let settings = NotificationSettings {
            channel_mentions: false,
            ..Default::default()
        };
        let everyone = IncomingMessage {
            mentions_channel: true,
            ..message(ConversationKind::Channel)
        };
        assert_eq!(decide_with(&everyone, &settings, None, false), None);
    }

    #[test]
    fn a_snooze_and_quiet_hours_pause_without_removing() {
        let dm = message(ConversationKind::Direct);
        let me = UserId::generate();
        let now = at(datetime!(2026-10-28 12:00 UTC));
        let snoozed = NotificationSettings {
            dnd_until: Some(at(datetime!(2026-10-28 13:00 UTC))),
            ..Default::default()
        };
        let decision = decide(&dm, me, &snoozed, None, false, now).unwrap();
        assert!(decision.paused);
        let later = at(datetime!(2026-10-28 13:00 UTC));
        assert!(
            !decide(&dm, me, &snoozed, None, false, later)
                .unwrap()
                .paused
        );
        assert!(
            !decide(&dm, me, &NotificationSettings::default(), None, false, now)
                .unwrap()
                .paused
        );
    }

    #[test]
    fn quiet_hours_use_the_persons_local_time_and_wrap_midnight() {
        // 22:00 to 07:00 in UTC-7, every day.
        let night = QuietHours {
            start_minute: 22 * 60,
            end_minute: 7 * 60,
            utc_offset_minutes: -7 * 60,
            days: QuietHours::EVERY_DAY,
        };
        assert!(night.is_valid());
        // 21:59 local is awake, 22:00 and 03:00 are quiet, 07:00 is awake. (Local = UTC - 7.)
        assert!(!night.contains(at(datetime!(2026-10-29 04:59 UTC))));
        assert!(night.contains(at(datetime!(2026-10-29 05:00 UTC))));
        assert!(night.contains(at(datetime!(2026-10-29 10:00 UTC))));
        assert!(!night.contains(at(datetime!(2026-10-29 14:00 UTC))));
        // A daytime window does not wrap.
        let work = QuietHours {
            start_minute: 9 * 60,
            end_minute: 17 * 60,
            utc_offset_minutes: 0,
            days: 0b0001_1111,
        };
        // 2026-10-28 is a Wednesday, 2026-10-31 a Saturday.
        assert!(work.contains(at(datetime!(2026-10-28 12:00 UTC))));
        assert!(!work.contains(at(datetime!(2026-10-28 17:00 UTC))));
        assert!(!work.contains(at(datetime!(2026-10-31 12:00 UTC))));
    }

    #[test]
    fn a_window_that_wraps_belongs_to_the_day_it_starts_on() {
        // Friday and Saturday nights only (22:00 to 02:00).
        let weekend = QuietHours {
            start_minute: 22 * 60,
            end_minute: 2 * 60,
            utc_offset_minutes: 0,
            days: 0b0011_0000,
        };
        // Friday 23:00 and Saturday 01:00 (still Friday's night) are quiet; Saturday 23:00 and
        // Sunday 01:00 are too; Thursday 23:00 and Friday 01:00 are not.
        assert!(weekend.contains(at(datetime!(2026-10-30 23:00 UTC))));
        assert!(weekend.contains(at(datetime!(2026-10-31 01:00 UTC))));
        assert!(weekend.contains(at(datetime!(2026-10-31 23:00 UTC))));
        assert!(weekend.contains(at(datetime!(2026-11-01 01:00 UTC))));
        assert!(!weekend.contains(at(datetime!(2026-10-29 23:00 UTC))));
        assert!(!weekend.contains(at(datetime!(2026-10-30 01:00 UTC))));
    }

    #[test]
    fn quiet_hours_are_validated() {
        let ok = QuietHours {
            start_minute: 0,
            end_minute: 60,
            utc_offset_minutes: 0,
            days: 1,
        };
        assert!(ok.is_valid());
        for bad in [
            QuietHours {
                start_minute: 1440,
                ..ok
            },
            QuietHours {
                end_minute: 1500,
                ..ok
            },
            QuietHours {
                end_minute: 0,
                ..ok
            },
            QuietHours { days: 0, ..ok },
            QuietHours {
                days: 0b1000_0000,
                ..ok
            },
            QuietHours {
                utc_offset_minutes: 15 * 60,
                ..ok
            },
        ] {
            assert!(!bad.is_valid(), "{bad:?}");
        }
    }

    #[test]
    fn a_status_is_short_text_with_an_optional_emoji_and_end() {
        let status =
            UserStatus::new("  In a meeting ", ReactionEmoji::parse("📅").ok(), None).unwrap();
        assert_eq!(status.text(), "In a meeting");
        let now = at(datetime!(2026-10-28 12:00 UTC));
        assert!(status.is_active(now));
        let ending =
            UserStatus::new("Lunch", None, Some(at(datetime!(2026-10-28 13:00 UTC)))).unwrap();
        assert!(ending.is_active(now));
        assert!(!ending.is_active(at(datetime!(2026-10-28 13:00 UTC))));
        for (text, error) in [
            ("", StatusError::Empty),
            ("   ", StatusError::Empty),
            ("a\nb", StatusError::ControlCharacter),
            (&"x".repeat(101), StatusError::TooLong),
        ] {
            assert_eq!(UserStatus::new(text, None, None), Err(error), "{text:?}");
        }
        assert!(UserStatus::new(&"x".repeat(100), None, None).is_ok());
        // It validates on the wire, and the emoji and end are left out when absent.
        let json = serde_json::to_value(&status).unwrap();
        assert!(json.get("expires_at").is_none());
        assert_eq!(serde_json::from_value::<UserStatus>(json).unwrap(), status);
        assert!(serde_json::from_str::<UserStatus>(r#"{"text":""}"#).is_err());
    }

    #[test]
    fn settings_and_prefs_have_a_stable_wire_shape() {
        let settings = NotificationSettings::default();
        let json = serde_json::to_value(settings).unwrap();
        assert_eq!(json["channel_level"], "mentions");
        assert_eq!(json["direct_level"], "all");
        assert_eq!(
            serde_json::from_value::<NotificationSettings>(json).unwrap(),
            settings
        );
        let p = prefs(None, false);
        let json = serde_json::to_value(p).unwrap();
        assert!(json.get("level").is_none() && json.get("muted").is_none());
        let p = prefs(Some(NotifyLevel::Nothing), true);
        assert_eq!(
            serde_json::from_value::<ConversationNotificationPrefs>(
                serde_json::to_value(p).unwrap()
            )
            .unwrap(),
            p
        );
    }

    fn arbitrary_message() -> impl Strategy<Value = IncomingMessage> {
        (
            prop::sample::select(vec![
                ConversationKind::Channel,
                ConversationKind::Direct,
                ConversationKind::Group,
            ]),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(
                |(kind, is_reply, in_channel, mentions_me, mentions_channel)| IncomingMessage {
                    author: UserId::generate(),
                    conversation_kind: kind,
                    is_reply,
                    in_channel: is_reply && in_channel,
                    mentions_me,
                    mentions_channel,
                },
            )
    }

    fn arbitrary_level() -> impl Strategy<Value = NotifyLevel> {
        prop::sample::select(vec![
            NotifyLevel::All,
            NotifyLevel::Mentions,
            NotifyLevel::Nothing,
        ])
    }

    proptest! {
        #[test]
        fn the_rules_hold_for_every_combination(
            message in arbitrary_message(),
            channel_level in arbitrary_level(),
            direct_level in arbitrary_level(),
            override_level in prop::option::of(arbitrary_level()),
            muted in any::<bool>(),
            thread_replies in any::<bool>(),
            channel_mentions in any::<bool>(),
            following in any::<bool>(),
        ) {
            let settings = NotificationSettings {
                channel_level, direct_level, thread_replies, channel_mentions,
                ..Default::default()
            };
            let conversation = prefs(override_level, muted);
            let me = UserId::generate();
            let now = at(datetime!(2026-10-28 12:00 UTC));
            let decision = decide(&message, me, &settings, Some(&conversation), following, now);
            let direct = message.conversation_kind != ConversationKind::Channel;
            let level = override_level.unwrap_or(if direct { direct_level } else { channel_level });
            // Muted, and a level of nothing, silence everything.
            if muted || level == NotifyLevel::Nothing {
                prop_assert_eq!(decision, None);
            }
            // A message that names you always notifies unless silenced above.
            if !muted && level != NotifyLevel::Nothing && message.mentions_me {
                prop_assert_eq!(decision.map(|d| d.reason), Some(NotificationReason::Mention));
            }
            // Anything that notifies had a reason the rules allow.
            if let Some(decision) = decision {
                match decision.reason {
                    NotificationReason::Mention => prop_assert!(message.mentions_me),
                    NotificationReason::ThreadReply => {
                        prop_assert!(message.is_reply && following && thread_replies)
                    }
                    NotificationReason::ChannelMention => {
                        prop_assert!(message.mentions_channel && channel_mentions)
                    }
                    NotificationReason::Direct => {
                        prop_assert!(direct && level == NotifyLevel::All)
                    }
                    NotificationReason::AllMessages => prop_assert!(
                        !direct && level == NotifyLevel::All && (!message.is_reply || message.in_channel)
                    ),
                }
                prop_assert!(!decision.paused);
            }
            // The author is never told.
            let own = IncomingMessage { author: me, ..message };
            prop_assert_eq!(decide(&own, me, &settings, Some(&conversation), following, now), None);
        }
    }
}
