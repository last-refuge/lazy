//! Notifications, settings, status, the clock, the badge, and the thread inbox (slice 3D, D20).

mod common;

use common::{Driver, World, channel, people, seq, user};
use lazy_client_core::{
    Change, DesktopNotification, Effect, INBOX_PAGE, INBOX_READ_CAP, InboxStatus, Input, Output,
    RequestError, SessionPhase, SocketEvent,
};
use lazy_domain::notification::{
    ConversationNotificationPrefs, NotificationReason, NotificationSettings, NotifyLevel,
    QuietHours, UserStatus,
};
use lazy_domain::{ConversationId, MessageId, ReactionEmoji, Timestamp};
use lazy_protocol::{
    ApiError, Conversation, ConversationNotificationPrefsResponse, ErrorCode, Event, InboxEntry,
    InboxPage, InboxReadResponse, Mentions, Message, NotificationSettingsResponse, ServerFrame,
    StatusResponse, ThreadSummary, ThreadUserState,
};
use time::OffsetDateTime;

// ----- Helpers -----

fn at(seconds: i64) -> Timestamp {
    Timestamp::from(OffsetDateTime::now_utc() + time::Duration::seconds(seconds))
}

struct Fx {
    world: World,
    driver: Driver,
    general: ConversationId,
    dm: Conversation,
    group: Conversation,
    head: i64,
}

/// A person with `#general`, `#random`, a direct conversation with Zoe, and a group, on a live
/// connection, with the window elsewhere (so nothing they look at suppresses a notification).
fn setup_with(world: World) -> Fx {
    let mut world = world;
    let third = user("morgan");
    let dm = people(world.workspace.id, &[&world.me, &world.other]);
    let group = people(world.workspace.id, &[&world.me, &world.other, &third]);
    world.extra_users.push(third.clone());
    world.extra.push(dm.clone());
    world.extra.push(group.clone());
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(general, vec![], false);
    driver.send(Input::Socket(SocketEvent::Opened {
        workspace: world.workspace.id,
    }));
    driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: world.workspace.id,
        seq: world.head,
    }));
    driver.send(Input::WindowActive(false));
    let head = world.head.get();
    Fx {
        world,
        driver,
        general,
        dm,
        group,
        head,
    }
}

fn setup() -> Fx {
    setup_with(World::new())
}

impl Fx {
    fn next(&mut self) -> i64 {
        self.head += 1;
        self.head
    }

    /// A live message from `author` in `conversation`.
    fn post_with(
        &mut self,
        conversation: ConversationId,
        author: &lazy_protocol::User,
        text: &str,
        tweak: impl FnOnce(&mut Message),
    ) -> (Message, Output) {
        let at = self.next();
        let mut message = self.world.message(conversation, author, at, text);
        tweak(&mut message);
        let out = self.driver.send(self.world.event(message.clone(), None));
        (message, out)
    }

    fn post(
        &mut self,
        conversation: ConversationId,
        author: &lazy_protocol::User,
        text: &str,
    ) -> (Message, Output) {
        self.post_with(conversation, author, text, |_| {})
    }

    fn other(&self) -> lazy_protocol::User {
        self.world.other.clone()
    }

    fn mention_me(&self) -> Box<dyn FnOnce(&mut Message)> {
        let me = self.world.me.id;
        Box::new(move |message| {
            message.mentions = Mentions {
                users: vec![me],
                channel: false,
            }
        })
    }

    fn settings(&mut self, change: impl FnOnce(&mut NotificationSettings)) {
        let mut settings = *self.driver.core.notification_settings();
        change(&mut settings);
        let out = self.driver.send(Input::SetNotificationSettings(settings));
        // Answer at once, so that what is shown is what the server holds.
        if out
            .effects
            .iter()
            .any(|e| matches!(e, Effect::SetNotificationSettings { .. }))
        {
            self.driver.send(Input::NotificationSettingsResult {
                workspace: self.world.workspace.id,
                result: Ok(NotificationSettingsResponse { settings }),
            });
        }
    }

    fn server_event(&mut self, event: Event) -> Output {
        let at = self.next();
        self.driver.send(self.world.event_at(at, event))
    }
}

fn notifications(out: &Output) -> Vec<DesktopNotification> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Notify(notification) => Some(notification.clone()),
            _ => None,
        })
        .collect()
}

fn rejected(code: ErrorCode, message: &str) -> RequestError {
    RequestError::Rejected(ApiError::new(code, message))
}

fn lost() -> RequestError {
    RequestError::Unknown("timed out".into())
}

fn badges(out: &Output) -> Vec<u32> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::SetBadge(count) => Some(*count),
            _ => None,
        })
        .collect()
}

// ----- Each reason -----

#[test]
fn a_message_that_names_the_person_notifies_as_a_mention() {
    let mut fx = setup();
    let general = fx.general;
    let (message, out) = fx.post_with(general, &fx.other(), "hello @jules", {
        let me = fx.world.me.id;
        move |m| {
            m.mentions = Mentions {
                users: vec![me],
                channel: false,
            }
        }
    });
    let sent = notifications(&out);
    assert_eq!(sent.len(), 1);
    let notification = &sent[0];
    assert_eq!(notification.reason, NotificationReason::Mention);
    assert_eq!(notification.conversation, general);
    assert_eq!(notification.message, message.id);
    assert_eq!(notification.title, "#general");
    assert_eq!(notification.body, "ZOE: hello @jules");
    assert_eq!(notification.tag, general.to_string());
    assert_eq!(notification.count, 1);
    assert_eq!(notification.thread_root, None);
}

#[test]
fn a_direct_message_notifies_with_the_sender_as_its_title() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let (_, out) = fx.post(dm, &fx.other(), "are you there?");
    let sent = notifications(&out);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].reason, NotificationReason::Direct);
    assert_eq!(sent[0].title, "ZOE");
    assert_eq!(sent[0].body, "ZOE: are you there?");
    // A group is titled by the people in it.
    let group = fx.group.id;
    let (_, out) = fx.post(group, &fx.other(), "hi all");
    let sent = notifications(&out);
    assert_eq!(sent[0].title, "MORGAN, ZOE");
}

#[test]
fn channel_mentions_and_channel_traffic_follow_the_settings() {
    let mut fx = setup();
    let general = fx.general;
    let (_, out) = fx.post(general, &fx.other(), "just talking");
    assert!(
        notifications(&out).is_empty(),
        "channels notify for mentions by default"
    );

    let (_, out) = fx.post_with(general, &fx.other(), "@channel look", |m| {
        m.mentions.channel = true
    });
    assert_eq!(
        notifications(&out)[0].reason,
        NotificationReason::ChannelMention
    );

    fx.settings(|s| s.channel_mentions = false);
    let (_, out) = fx.post_with(general, &fx.other(), "@channel again", |m| {
        m.mentions.channel = true
    });
    assert!(notifications(&out).is_empty());

    fx.settings(|s| s.channel_level = NotifyLevel::All);
    let (_, out) = fx.post(general, &fx.other(), "everything now");
    assert_eq!(
        notifications(&out)[0].reason,
        NotificationReason::AllMessages
    );
}

#[test]
fn a_reply_in_a_followed_thread_notifies_and_one_in_an_unfollowed_thread_does_not() {
    let mut world = World::new();
    let general = world.channel.id;
    let root = world.message(general, &world.me, 5, "my root");
    let other_root = world.message(general, &world.other, 6, "their root");
    world.threads = vec![ThreadUserState {
        root_id: root.id,
        conversation_id: general,
        following: true,
        last_read: seq(0),
        unread_count: 0,
        mention_count: 0,
    }];
    let mut fx = setup_with(world);
    let (_, out) = fx.post_with(general, &fx.other(), "a reply", |m| {
        m.thread_root = Some(root.id)
    });
    let sent = notifications(&out);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].reason, NotificationReason::ThreadReply);
    assert_eq!(sent[0].thread_root, Some(root.id));
    let (_, out) = fx.post_with(general, &fx.other(), "another thread", |m| {
        m.thread_root = Some(other_root.id)
    });
    assert!(notifications(&out).is_empty());
}

// ----- Everything that suppresses a notification -----

#[test]
fn nothing_notifies_for_the_persons_own_messages_muted_conversations_or_level_nothing() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let me = fx.world.me.clone();
    let (_, out) = fx.post(dm, &me, "talking to myself");
    assert!(notifications(&out).is_empty(), "own message");

    fx.driver.send(Input::SetConversationNotificationPrefs {
        conversation: dm,
        level: None,
        muted: true,
    });
    let (_, out) = fx.post_with(dm, &fx.other(), "muted", fx.mention_me());
    assert!(notifications(&out).is_empty(), "muted beats a mention");

    fx.driver.send(Input::SetConversationNotificationPrefs {
        conversation: dm,
        level: Some(NotifyLevel::Nothing),
        muted: false,
    });
    let (_, out) = fx.post_with(dm, &fx.other(), "nothing", fx.mention_me());
    assert!(
        notifications(&out).is_empty(),
        "nothing silences mentions too"
    );
}

#[test]
fn a_snooze_and_quiet_hours_pause_notifications_without_counting_them_out() {
    let mut fx = setup();
    let dm = fx.dm.id;
    fx.driver.send(Input::Clock(at(0)));
    fx.driver.send(Input::PauseNotifications {
        until: Some(at(3600)),
    });
    assert!(fx.driver.core.notifications_paused());
    let (message, out) = fx.post(dm, &fx.other(), "while snoozed");
    assert!(notifications(&out).is_empty());
    // Still unread, still in the badge.
    assert_eq!(
        fx.driver.core.user_state(dm).map(|s| s.unread_count),
        Some(1)
    );
    assert_eq!(fx.driver.core.attention_count(), 1, "{}", message.id);

    // Resuming lets the next one through.
    fx.driver.send(Input::PauseNotifications { until: None });
    assert!(!fx.driver.core.notifications_paused());
    let (_, out) = fx.post(dm, &fx.other(), "back");
    assert_eq!(notifications(&out).len(), 1);

    // Quiet hours around now pause too.
    let now = OffsetDateTime::now_utc();
    let minute = u16::from(now.hour()) * 60 + u16::from(now.minute());
    fx.settings(|s| {
        s.quiet_hours = Some(QuietHours {
            start_minute: (minute + 1440 - 5) % 1440,
            end_minute: (minute + 5) % 1440,
            utc_offset_minutes: 0,
            days: QuietHours::EVERY_DAY,
        })
    });
    let (_, out) = fx.post(dm, &fx.other(), "in quiet hours");
    assert!(notifications(&out).is_empty());
}

#[test]
fn history_and_the_burst_after_a_reconnect_are_not_news() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let workspace = fx.world.workspace.id;
    // The connection drops and comes back: events replay until it has caught up.
    fx.driver.send(Input::Socket(SocketEvent::Closed {
        workspace,
        attempt: 1,
    }));
    fx.driver
        .send(Input::Socket(SocketEvent::Opened { workspace }));
    let (_, out) = fx.post(dm, &fx.other(), "missed while away");
    assert!(notifications(&out).is_empty(), "replayed");
    // Still counted as unread.
    assert_eq!(
        fx.driver.core.user_state(dm).map(|s| s.unread_count),
        Some(1)
    );
    let head = fx.head;
    fx.driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: workspace,
        seq: seq(head),
    }));
    let (_, out) = fx.post(dm, &fx.other(), "live again");
    assert_eq!(notifications(&out).len(), 1);

    // Before the first catch-up nothing is live either.
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(Input::WindowActive(false));
    let general = world.channel.id;
    let named = Message {
        mentions: Mentions {
            users: vec![world.me.id],
            channel: false,
        },
        ..world.message(general, &world.other, 11, "early")
    };
    let out = driver.send(world.event(named, None));
    assert!(notifications(&out).is_empty());
}

#[test]
fn edits_and_deleted_creations_never_notify() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let (message, _) = fx.post_with(dm, &fx.other(), "first", |_| {});
    let edited = fx.world.edited(&message, "changed");
    let at = fx.next();
    let out = fx.driver.send(fx.world.edited_event(at, &edited, None));
    assert!(notifications(&out).is_empty());
    // A message that arrives already deleted (a replay rewrites it so) is not news.
    let at = fx.next();
    let ghost = fx
        .world
        .tombstone(&fx.world.message(dm, &fx.other(), at, "gone"));
    let out = fx.driver.send(fx.world.event(ghost, None));
    assert!(notifications(&out).is_empty());
}

#[test]
fn the_desktop_switch_turns_the_popups_off_but_not_the_counts() {
    let mut fx = setup();
    let dm = fx.dm.id;
    fx.settings(|s| s.desktop = false);
    let (_, out) = fx.post(dm, &fx.other(), "quiet popup");
    assert!(notifications(&out).is_empty());
    assert_eq!(fx.driver.core.attention_count(), 1);
}

#[test]
fn looking_at_the_conversation_suppresses_the_notification_looking_elsewhere_does_not() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let general = fx.general;
    fx.driver.send(Input::WindowActive(true));
    fx.driver.send(Input::SelectConversation(general));
    // In the conversation on screen with the window active: no popup.
    let (_, out) = fx.post_with(general, &fx.other(), "here", fx.mention_me());
    assert!(notifications(&out).is_empty());
    // Another conversation: popup.
    let (_, out) = fx.post(dm, &fx.other(), "there");
    assert_eq!(notifications(&out).len(), 1);
    // The window in the background: popup, even in the selected conversation.
    fx.driver.send(Input::WindowActive(false));
    let (_, out) = fx.post_with(general, &fx.other(), "away", fx.mention_me());
    assert_eq!(notifications(&out).len(), 1);
}

#[test]
fn a_thread_only_reply_is_seen_only_in_its_open_thread() {
    let mut fx = setup();
    let general = fx.general;
    let (root, _) = fx.post(general, &fx.world.me.clone(), "my root");
    fx.driver.send(Input::WindowActive(true));
    fx.driver.send(Input::SelectConversation(general));
    // A reply to a thread the person follows (they started it).
    let me = fx.world.me.id;
    let at = fx.next();
    fx.driver.send(fx.world.event_at(
        at,
        Event::ThreadStateChanged {
            state: ThreadUserState {
                root_id: root.id,
                conversation_id: general,
                following: true,
                last_read: seq(0),
                unread_count: 0,
                mention_count: 0,
            },
        },
    ));
    let _ = me;
    // The channel is on screen but the thread is not open: the reply is not on screen.
    let (_, out) = fx.post_with(general, &fx.other(), "reply one", |m| {
        m.thread_root = Some(root.id)
    });
    assert_eq!(notifications(&out).len(), 1);
    // A reply also sent to the channel is in the list on screen.
    let (_, out) = fx.post_with(general, &fx.other(), "reply two", |m| {
        m.thread_root = Some(root.id);
        m.in_channel = true;
    });
    assert!(notifications(&out).is_empty());
    // With the thread open, its replies are on screen.
    fx.driver.send(Input::OpenThread {
        conversation: general,
        root: root.id,
    });
    let (_, out) = fx.post_with(general, &fx.other(), "reply three", |m| {
        m.thread_root = Some(root.id)
    });
    assert!(notifications(&out).is_empty());
}

#[test]
fn a_bot_follows_the_same_rules() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let mut bot = user("hooky");
    bot.is_bot = true;
    let (_, out) = fx.post_with(dm, &bot, "build finished", |_| {});
    assert_eq!(notifications(&out).len(), 1);
}

// ----- What a notification says -----

#[test]
fn the_body_is_one_short_line_of_the_text_files_or_nothing() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let long = "word ".repeat(60);
    let (_, out) = fx.post(dm, &fx.other(), &long);
    let body = &notifications(&out)[0].body;
    assert_eq!(
        body.chars().count(),
        lazy_client_core::MAX_NOTIFICATION_BODY
    );
    assert!(body.starts_with("ZOE: word word") && body.ends_with('…'));

    // Line breaks become spaces.
    fx.driver.send(Input::SelectConversation(fx.general));
    let (_, out) = fx.post(dm, &fx.other(), "one\n\ntwo");
    assert!(notifications(&out)[0].body.ends_with("one two"));

    // Files only.
    fx.settings(|_| {});
    let at = fx.next();
    let mut files = fx.world.message(dm, &fx.other(), at, "x");
    files.document = lazy_domain::MessageDocument::empty();
    files.attachments = vec![sample_attachment(), sample_attachment()];
    let out = fx.driver.send(fx.world.event(files, None));
    assert!(
        notifications(&out)[0].body.ends_with("[2 files]"),
        "{:?}",
        notifications(&out)
    );

    // With the text hidden: who and where only.
    fx.settings(|s| s.show_content = false);
    let (_, out) = fx.post(dm, &fx.other(), "secret");
    assert!(!notifications(&out)[0].body.contains("secret"));
    let group = fx.group.id;
    let (_, out) = fx.post(group, &fx.other(), "secret");
    assert_eq!(notifications(&out)[0].body, "ZOE: New message");
}

fn sample_attachment() -> lazy_protocol::Attachment {
    lazy_protocol::Attachment {
        id: lazy_domain::AttachmentId::generate(),
        name: "a.png".into(),
        media_type: "image/png".into(),
        size: 10,
        sha256: "00".repeat(32),
        width: None,
        height: None,
        has_thumbnail: false,
    }
}

#[test]
fn newer_notifications_for_a_conversation_replace_the_earlier_and_count_them() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let first = notifications(&fx.post(dm, &fx.other(), "one").1);
    assert_eq!((first[0].count, first[0].body.as_str()), (1, "ZOE: one"));
    let second = notifications(&fx.post(dm, &fx.other(), "two").1);
    assert_eq!(second[0].count, 2);
    assert_eq!(second[0].tag, first[0].tag, "the same tag replaces");
    assert_eq!(second[0].body, "2 new messages. Latest: ZOE: two");
    let third = notifications(&fx.post(dm, &fx.other(), "three").1);
    assert_eq!(third[0].count, 3);
    assert!(third[0].body.starts_with("3 new messages."));
    // Another conversation counts apart.
    let group = fx.group.id;
    let other = notifications(&fx.post(group, &fx.other(), "elsewhere").1);
    assert_eq!(other[0].count, 1);
    assert_ne!(other[0].tag, third[0].tag);
}

// ----- Clearing and retracting -----

#[test]
fn reading_the_conversation_clears_its_notifications() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let (message, _) = fx.post(dm, &fx.other(), "read me");
    // Read on another device: the read position moves past it.
    let out = fx.server_event(Event::UserStateChanged {
        state: lazy_protocol::ConversationUserState {
            conversation_id: dm,
            favorite: false,
            last_read: message.seq,
            unread_count: 0,
            mention_count: 0,
        },
    });
    assert!(
        out.effects
            .contains(&Effect::ClearNotifications { conversation: dm })
    );
    // Nothing left to clear the second time.
    let out = fx.server_event(Event::UserStateChanged {
        state: lazy_protocol::ConversationUserState {
            conversation_id: dm,
            favorite: false,
            last_read: message.seq,
            unread_count: 0,
            mention_count: 0,
        },
    });
    assert!(
        !out.effects
            .iter()
            .any(|e| matches!(e, Effect::ClearNotifications { .. }))
    );
}

#[test]
fn looking_at_the_conversation_at_the_bottom_clears_its_notifications() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let general = fx.general;
    fx.post(dm, &fx.other(), "waiting");
    fx.driver.send(Input::SelectConversation(dm));
    fx.driver.load_history(dm, vec![], false);
    fx.driver.send(Input::WindowActive(true));
    let out = fx.driver.send(Input::TimelineAtBottom {
        conversation: dm,
        at_bottom: true,
    });
    assert!(
        out.effects
            .contains(&Effect::ClearNotifications { conversation: dm })
    );
    let _ = general;
}

#[test]
fn deleting_a_notified_message_retracts_its_notification() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let (message, _) = fx.post(dm, &fx.other(), "oops");
    let at = fx.next();
    let out = fx
        .driver
        .send(fx.world.deleted_event(at, &fx.world.tombstone(&message)));
    assert!(out.effects.contains(&Effect::RetractNotification {
        message: message.id
    }));
    // A message that was never notified retracts nothing.
    let (quiet, _) = fx.post(fx.general, &fx.other(), "plain");
    let at = fx.next();
    let out = fx
        .driver
        .send(fx.world.deleted_event(at, &fx.world.tombstone(&quiet)));
    assert!(
        !out.effects
            .iter()
            .any(|e| matches!(e, Effect::RetractNotification { .. }))
    );
}

#[test]
fn clicking_a_notification_opens_its_message_like_a_permalink() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let (message, out) = fx.post(dm, &fx.other(), "open me");
    assert_eq!(notifications(&out).len(), 1);
    let out = fx.driver.send(Input::NotificationActivated {
        conversation: dm,
        message: message.id,
    });
    // A message the core holds is jumped to; one it does not is fetched.
    assert!(
        fx.driver.core.jump_target().is_some()
            || out.effects.contains(&Effect::FetchMessage {
                message: message.id
            })
    );
    let unknown = MessageId::generate();
    let out = fx.driver.send(Input::NotificationActivated {
        conversation: dm,
        message: unknown,
    });
    assert!(
        out.effects
            .contains(&Effect::FetchMessage { message: unknown })
    );
    // A conversation the core does not know is ignored.
    let out = fx.driver.send(Input::NotificationActivated {
        conversation: ConversationId::generate(),
        message: unknown,
    });
    assert!(out.effects.is_empty());
}

// ----- The badge -----

#[test]
fn the_attention_count_adds_mentions_in_channels_unread_direct_messages_and_followed_threads() {
    let mut world = World::new();
    let general = world.channel.id;
    let root = world.message(general, &world.me, 4, "root");
    world.threads = vec![ThreadUserState {
        root_id: root.id,
        conversation_id: general,
        following: true,
        last_read: seq(0),
        unread_count: 2,
        mention_count: 0,
    }];
    let mut fx = setup_with(world);
    let dm = fx.dm.id;
    assert_eq!(fx.driver.core.attention_count(), 2, "the followed thread");
    // A plain channel message does not count; a mention of the person does.
    let (_, out) = fx.post(general, &fx.other(), "plain");
    assert!(badges(&out).is_empty());
    let (_, out) = fx.post_with(general, &fx.other(), "named", fx.mention_me());
    assert_eq!(badges(&out), vec![3]);
    // Each unread direct message counts.
    let (_, out) = fx.post(dm, &fx.other(), "one");
    assert_eq!(badges(&out), vec![4]);
    let (_, out) = fx.post(dm, &fx.other(), "two");
    assert_eq!(badges(&out), vec![5]);
    assert!(out.changes.contains(&Change::AttentionChanged));
    assert_eq!(fx.driver.core.attention_count(), 5);
    // Muting the direct conversation takes it out; the unread count itself is untouched.
    let out = fx.driver.send(Input::SetConversationNotificationPrefs {
        conversation: dm,
        level: None,
        muted: true,
    });
    assert_eq!(badges(&out), vec![3]);
    assert_eq!(
        fx.driver.core.user_state(dm).map(|s| s.unread_count),
        Some(2)
    );
    assert!(fx.driver.core.is_muted(dm));
    // Muting the channel takes out its mention and the followed thread in it.
    let out = fx.driver.send(Input::SetConversationNotificationPrefs {
        conversation: general,
        level: None,
        muted: true,
    });
    assert_eq!(badges(&out), vec![0]);
}

#[test]
fn the_badge_is_announced_only_when_it_changes_and_signing_out_resets_it() {
    let mut fx = setup();
    let dm = fx.dm.id;
    let (_, out) = fx.post(dm, &fx.other(), "one");
    assert_eq!(badges(&out), vec![1]);
    let out = fx.driver.send(Input::WindowActive(false));
    assert!(badges(&out).is_empty());
    let out = fx.driver.send(Input::SignOut);
    assert!(out.effects.contains(&Effect::SetBadge(0)));
    assert!(
        out.effects
            .contains(&Effect::ClearNotifications { conversation: dm })
    );
}

// ----- Settings -----

#[test]
fn a_settings_change_shows_at_once_and_is_saved_with_one_put() {
    let mut fx = setup();
    let workspace = fx.world.workspace.id;
    assert_eq!(
        *fx.driver.core.notification_settings(),
        NotificationSettings::default()
    );
    let wanted = NotificationSettings {
        channel_level: NotifyLevel::All,
        show_content: false,
        ..Default::default()
    };
    let out = fx.driver.send(Input::SetNotificationSettings(wanted));
    assert_eq!(*fx.driver.core.notification_settings(), wanted);
    assert!(out.changes.contains(&Change::NotificationsChanged));
    assert_eq!(
        out.effects
            .iter()
            .filter(|e| matches!(e, Effect::SetNotificationSettings { .. }))
            .count(),
        1
    );
    assert!(out.effects.contains(&Effect::SetNotificationSettings {
        workspace,
        settings: wanted
    }));
    let out = fx.driver.send(Input::NotificationSettingsResult {
        workspace: fx.world.workspace.id,
        result: Ok(NotificationSettingsResponse { settings: wanted }),
    });
    assert!(out.effects.is_empty());
    assert_eq!(*fx.driver.core.notification_settings(), wanted);
    // Asking for what is already so sends nothing.
    let out = fx.driver.send(Input::SetNotificationSettings(wanted));
    assert!(out.effects.is_empty());
}

#[test]
fn quiet_hours_that_cannot_work_are_refused_in_words_and_nothing_changes() {
    let mut fx = setup();
    let bad = NotificationSettings {
        quiet_hours: Some(QuietHours {
            start_minute: 60,
            end_minute: 60,
            utc_offset_minutes: 0,
            days: QuietHours::EVERY_DAY,
        }),
        ..Default::default()
    };
    let out = fx.driver.send(Input::SetNotificationSettings(bad));
    assert!(out.effects.is_empty());
    assert_eq!(
        *fx.driver.core.notification_settings(),
        NotificationSettings::default()
    );
    assert!(fx.driver.core.notice().unwrap().contains("Quiet hours"));
}

#[test]
fn a_wish_made_while_a_save_is_out_is_saved_after_its_answer() {
    let mut fx = setup();
    let first = NotificationSettings {
        desktop: false,
        ..Default::default()
    };
    let second = NotificationSettings {
        desktop: false,
        thread_replies: false,
        ..Default::default()
    };
    fx.driver.send(Input::SetNotificationSettings(first));
    let out = fx.driver.send(Input::SetNotificationSettings(second));
    assert!(out.effects.is_empty(), "one at a time");
    assert_eq!(*fx.driver.core.notification_settings(), second);
    let out = fx.driver.send(Input::NotificationSettingsResult {
        workspace: fx.world.workspace.id,
        result: Ok(NotificationSettingsResponse { settings: first }),
    });
    assert!(matches!(
        out.effects.as_slice(),
        [Effect::SetNotificationSettings { settings, .. }] if *settings == second
    ));
    assert_eq!(
        *fx.driver.core.notification_settings(),
        second,
        "never flickers back"
    );
    fx.driver.send(Input::NotificationSettingsResult {
        workspace: fx.world.workspace.id,
        result: Ok(NotificationSettingsResponse { settings: second }),
    });
    assert_eq!(*fx.driver.core.notification_settings(), second);
}

#[test]
fn a_lost_save_is_repeated_three_times_then_the_old_settings_come_back_with_a_notice() {
    let mut fx = setup();
    let wanted = NotificationSettings {
        desktop: false,
        ..Default::default()
    };
    let mut sends = 1;
    fx.driver.send(Input::SetNotificationSettings(wanted));
    for _ in 0..2 {
        let out = fx.driver.send(Input::NotificationSettingsResult {
            workspace: fx.world.workspace.id,
            result: Err(lost()),
        });
        assert!(
            matches!(out.effects.as_slice(), [Effect::SetNotificationSettings { settings, .. }] if *settings == wanted)
        );
        sends += 1;
        assert_eq!(*fx.driver.core.notification_settings(), wanted);
    }
    assert_eq!(sends, 3);
    let out = fx.driver.send(Input::NotificationSettingsResult {
        workspace: fx.world.workspace.id,
        result: Err(lost()),
    });
    assert!(out.effects.is_empty());
    assert_eq!(
        *fx.driver.core.notification_settings(),
        NotificationSettings::default()
    );
    assert!(
        fx.driver
            .core
            .notice()
            .unwrap()
            .contains("notification settings")
    );
}

#[test]
fn a_refused_save_reverts_with_the_reason() {
    let mut fx = setup();
    fx.driver
        .send(Input::SetNotificationSettings(NotificationSettings {
            desktop: false,
            ..Default::default()
        }));
    fx.driver.send(Input::NotificationSettingsResult {
        workspace: fx.world.workspace.id,
        result: Err(rejected(
            ErrorCode::InvalidRequest,
            "that time is not allowed",
        )),
    });
    assert_eq!(
        *fx.driver.core.notification_settings(),
        NotificationSettings::default()
    );
    assert_eq!(fx.driver.core.notice(), Some("that time is not allowed"));
    // A late answer to nothing changes nothing.
    fx.driver.send(Input::NotificationSettingsResult {
        workspace: fx.world.workspace.id,
        result: Ok(NotificationSettingsResponse {
            settings: NotificationSettings::default(),
        }),
    });
    // An expired session ends the session.
    fx.driver
        .send(Input::SetNotificationSettings(NotificationSettings {
            desktop: false,
            ..Default::default()
        }));
    fx.driver.send(Input::NotificationSettingsResult {
        workspace: fx.world.workspace.id,
        result: Err(rejected(ErrorCode::Unauthenticated, "expired")),
    });
    assert!(matches!(
        fx.driver.core.phase(),
        SessionPhase::SignedOut { .. }
    ));
}

#[test]
fn events_replace_the_settings_but_never_flip_a_change_that_is_not_saved() {
    let mut fx = setup();
    let from_other_device = NotificationSettings {
        channel_level: NotifyLevel::All,
        ..Default::default()
    };
    let out = fx.server_event(Event::NotificationSettingsChanged {
        settings: from_other_device,
    });
    assert_eq!(*fx.driver.core.notification_settings(), from_other_device);
    assert!(out.changes.contains(&Change::NotificationsChanged));

    let mine = NotificationSettings {
        desktop: false,
        ..from_other_device
    };
    fx.driver.send(Input::SetNotificationSettings(mine));
    // An older event arrives while the save is out.
    fx.server_event(Event::NotificationSettingsChanged {
        settings: NotificationSettings::default(),
    });
    assert_eq!(*fx.driver.core.notification_settings(), mine);
    fx.driver.send(Input::NotificationSettingsResult {
        workspace: fx.world.workspace.id,
        result: Ok(NotificationSettingsResponse { settings: mine }),
    });
    assert_eq!(*fx.driver.core.notification_settings(), mine);
    // The refused case goes back to what the events said last.
    fx.driver
        .send(Input::SetNotificationSettings(from_other_device));
    fx.server_event(Event::NotificationSettingsChanged {
        settings: NotificationSettings::default(),
    });
    fx.driver.send(Input::NotificationSettingsResult {
        workspace: fx.world.workspace.id,
        result: Err(rejected(ErrorCode::Forbidden, "no")),
    });
    assert_eq!(
        *fx.driver.core.notification_settings(),
        NotificationSettings::default()
    );
}

#[test]
fn the_bootstrap_seeds_settings_and_preferences_and_signing_out_forgets_them() {
    let mut world = World::new();
    world.notification_settings = NotificationSettings {
        direct_level: NotifyLevel::Mentions,
        ..Default::default()
    };
    world.notification_prefs = vec![ConversationNotificationPrefs {
        conversation_id: world.channel.id,
        level: Some(NotifyLevel::All),
        muted: false,
    }];
    let mut fx = setup_with(world);
    let general = fx.general;
    assert_eq!(
        fx.driver.core.notification_settings().direct_level,
        NotifyLevel::Mentions
    );
    assert_eq!(fx.driver.core.effective_level(general), NotifyLevel::All);
    assert_eq!(
        fx.driver.core.effective_level(fx.dm.id),
        NotifyLevel::Mentions
    );
    assert_eq!(
        fx.driver.core.effective_level(fx.world.second_channel.id),
        NotifyLevel::Mentions
    );
    assert!(
        fx.driver
            .core
            .conversation_notification_prefs(general)
            .is_some()
    );
    fx.driver.send(Input::SignOut);
    assert_eq!(
        *fx.driver.core.notification_settings(),
        NotificationSettings::default()
    );
    assert!(
        fx.driver
            .core
            .conversation_notification_prefs(general)
            .is_none()
    );
}

// ----- Conversation preferences -----

#[test]
fn a_conversation_preference_is_optimistic_and_resetting_it_removes_it() {
    let mut fx = setup();
    let general = fx.general;
    assert!(
        fx.driver
            .core
            .conversation_notification_prefs(general)
            .is_none()
    );
    let out = fx.driver.send(Input::SetConversationNotificationPrefs {
        conversation: general,
        level: Some(NotifyLevel::All),
        muted: false,
    });
    assert_eq!(fx.driver.core.effective_level(general), NotifyLevel::All);
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::SetConversationNotificationPrefs { conversation, request }
            if *conversation == general && request.level == Some(NotifyLevel::All) && !request.muted
    )));
    let prefs = ConversationNotificationPrefs {
        conversation_id: general,
        level: Some(NotifyLevel::All),
        muted: false,
    };
    fx.driver.send(Input::ConversationPrefsResult {
        conversation: general,
        result: Ok(ConversationNotificationPrefsResponse { prefs }),
    });
    assert_eq!(
        fx.driver.core.conversation_notification_prefs(general),
        Some(&prefs)
    );
    // Back to the defaults.
    fx.driver.send(Input::SetConversationNotificationPrefs {
        conversation: general,
        level: None,
        muted: false,
    });
    assert!(
        fx.driver
            .core
            .conversation_notification_prefs(general)
            .is_none()
    );
    fx.driver.send(Input::ConversationPrefsResult {
        conversation: general,
        result: Ok(ConversationNotificationPrefsResponse {
            prefs: ConversationNotificationPrefs {
                conversation_id: general,
                level: None,
                muted: false,
            },
        }),
    });
    assert_eq!(
        fx.driver.core.effective_level(general),
        NotifyLevel::Mentions
    );
    // A conversation the core does not know is ignored.
    let out = fx.driver.send(Input::SetConversationNotificationPrefs {
        conversation: ConversationId::generate(),
        level: None,
        muted: true,
    });
    assert!(out.effects.is_empty());
}

#[test]
fn preferences_repeat_revert_and_follow_events_like_the_settings() {
    let mut fx = setup();
    let general = fx.general;
    let ask = |fx: &mut Fx| {
        fx.driver.send(Input::SetConversationNotificationPrefs {
            conversation: general,
            level: None,
            muted: true,
        })
    };
    ask(&mut fx);
    assert!(fx.driver.core.is_muted(general));
    for _ in 0..2 {
        let out = fx.driver.send(Input::ConversationPrefsResult {
            conversation: general,
            result: Err(lost()),
        });
        assert_eq!(out.effects.len(), 1);
    }
    let out = fx.driver.send(Input::ConversationPrefsResult {
        conversation: general,
        result: Err(lost()),
    });
    assert!(out.effects.is_empty());
    assert!(!fx.driver.core.is_muted(general), "given up");
    assert!(fx.driver.core.notice().is_some());
    // The save that was thought lost had in fact worked: the event says so.
    fx.server_event(Event::ConversationNotificationPrefsChanged {
        prefs: ConversationNotificationPrefs {
            conversation_id: general,
            level: None,
            muted: true,
        },
    });
    assert!(fx.driver.core.is_muted(general));
    // A refusal.
    fx.driver.send(Input::SetConversationNotificationPrefs {
        conversation: general,
        level: None,
        muted: false,
    });
    fx.driver.send(Input::ConversationPrefsResult {
        conversation: general,
        result: Err(rejected(ErrorCode::Forbidden, "no")),
    });
    assert!(
        fx.driver.core.is_muted(general),
        "back to what the server holds"
    );
}

// ----- Status and the clock -----

#[test]
fn a_status_is_optimistic_saved_with_one_put_and_shown_on_the_person() {
    let mut fx = setup();
    let me = fx.world.me.id;
    let workspace = fx.world.workspace.id;
    let status = UserStatus::new("In a meeting", ReactionEmoji::parse("📅").ok(), None).unwrap();
    let out = fx.driver.send(Input::SetStatus(Some(status.clone())));
    assert_eq!(fx.driver.core.status_of(me), Some(&status));
    assert_eq!(
        fx.driver.core.member(me).unwrap().status.as_ref(),
        Some(&status)
    );
    assert!(out.changes.contains(&Change::StatusChanged));
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::SetStatus { workspace: w, request } if *w == workspace && request.status.as_ref() == Some(&status)
    )));
    let mut user = fx.world.me.clone();
    user.status = Some(status.clone());
    fx.driver.send(Input::StatusResult {
        workspace: fx.world.workspace.id,
        result: Ok(StatusResponse { user }),
    });
    assert_eq!(fx.driver.core.status_of(me), Some(&status));
    // Clearing it.
    let out = fx.driver.send(Input::SetStatus(None));
    assert_eq!(fx.driver.core.status_of(me), None);
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::SetStatus { request, .. } if request.status.is_none()))
    );
}

#[test]
fn other_peoples_statuses_come_from_the_bootstrap_and_events() {
    let mut world = World::new();
    let status = UserStatus::new("Out today", None, None).unwrap();
    world.other.status = Some(status.clone());
    let mut fx = setup_with(world);
    let zoe = fx.world.other.id;
    assert_eq!(fx.driver.core.status_of(zoe), Some(&status));
    let newer = UserStatus::new("Back", None, None).unwrap();
    let out = fx.server_event(Event::UserStatusChanged {
        user_id: zoe,
        status: Some(newer.clone()),
    });
    assert_eq!(fx.driver.core.status_of(zoe), Some(&newer));
    assert!(out.changes.contains(&Change::StatusChanged));
    fx.server_event(Event::UserStatusChanged {
        user_id: zoe,
        status: None,
    });
    assert_eq!(fx.driver.core.status_of(zoe), None);
    // The person's own, from another device.
    let me = fx.world.me.id;
    fx.server_event(Event::UserStatusChanged {
        user_id: me,
        status: Some(newer.clone()),
    });
    assert_eq!(fx.driver.core.status_of(me), Some(&newer));
}

#[test]
fn a_status_that_has_ended_is_hidden_by_the_clock_and_the_screen_is_told() {
    let mut world = World::new();
    let ends = at(60);
    world.other.status = Some(UserStatus::new("Lunch", None, Some(ends)).unwrap());
    let mut fx = setup_with(world);
    let zoe = fx.world.other.id;
    fx.driver.send(Input::Clock(at(0)));
    assert!(fx.driver.core.status_of(zoe).is_some());
    let out = fx.driver.send(Input::Clock(at(120)));
    assert!(fx.driver.core.status_of(zoe).is_none());
    assert!(out.changes.contains(&Change::StatusChanged));
    // Nothing more to say about it.
    let out = fx.driver.send(Input::Clock(at(180)));
    assert!(!out.changes.contains(&Change::StatusChanged));
    // A snooze ends the same way.
    fx.driver.send(Input::PauseNotifications {
        until: Some(at(300)),
    });
    assert!(fx.driver.core.notifications_paused());
    let out = fx.driver.send(Input::Clock(at(400)));
    assert!(!fx.driver.core.notifications_paused());
    assert!(out.changes.contains(&Change::NotificationsChanged));
}

#[test]
fn a_status_is_checked_in_words_before_it_is_sent() {
    let mut fx = setup();
    fx.driver.send(Input::Clock(at(0)));
    assert_eq!(
        fx.driver.core.make_status("  ", None, None).unwrap_err(),
        "Write what you are up to."
    );
    assert!(
        fx.driver
            .core
            .make_status(&"x".repeat(101), None, None)
            .unwrap_err()
            .contains("100")
    );
    assert!(
        fx.driver
            .core
            .make_status("a\nb", None, None)
            .unwrap_err()
            .contains("control")
    );
    assert!(
        fx.driver
            .core
            .make_status("late", None, Some(at(-5)))
            .unwrap_err()
            .contains("passed")
    );
    let gone = ReactionEmoji::custom(lazy_domain::EmojiId::generate());
    assert!(
        fx.driver
            .core
            .make_status("hi", Some(gone), None)
            .unwrap_err()
            .contains("deleted")
    );
    assert!(
        fx.driver
            .core
            .make_status("fine", None, Some(at(600)))
            .is_ok()
    );
    // SetStatus checks the same things, whoever built the status.
    let stale = UserStatus::new("old", None, Some(at(-5))).unwrap();
    let out = fx.driver.send(Input::SetStatus(Some(stale)));
    assert!(out.effects.is_empty());
    assert!(fx.driver.core.notice().is_some());
    let emoji = UserStatus::new("hi", Some(gone), None).unwrap();
    let out = fx.driver.send(Input::SetStatus(Some(emoji)));
    assert!(out.effects.is_empty());
}

#[test]
fn a_status_save_repeats_reverts_and_follows_events() {
    let mut fx = setup();
    let me = fx.world.me.id;
    let status = UserStatus::new("Heads down", None, None).unwrap();
    fx.driver.send(Input::SetStatus(Some(status.clone())));
    for _ in 0..2 {
        let out = fx.driver.send(Input::StatusResult {
            workspace: fx.world.workspace.id,
            result: Err(lost()),
        });
        assert_eq!(out.effects.len(), 1);
    }
    let out = fx.driver.send(Input::StatusResult {
        workspace: fx.world.workspace.id,
        result: Err(lost()),
    });
    assert!(out.effects.is_empty());
    assert_eq!(fx.driver.core.status_of(me), None, "given up");
    assert!(fx.driver.core.notice().is_some());
    // The event of the save that did work arrives later.
    fx.server_event(Event::UserStatusChanged {
        user_id: me,
        status: Some(status.clone()),
    });
    assert_eq!(fx.driver.core.status_of(me), Some(&status));
    // A refusal puts the server's status back.
    fx.driver.send(Input::SetStatus(None));
    fx.driver.send(Input::StatusResult {
        workspace: fx.world.workspace.id,
        result: Err(rejected(ErrorCode::InvalidRequest, "that status has ended")),
    });
    assert_eq!(fx.driver.core.status_of(me), Some(&status));
    assert_eq!(fx.driver.core.notice(), Some("that status has ended"));
}

// ----- The thread inbox -----

fn entry(
    world: &World,
    conversation: &Conversation,
    root_seq: i64,
    last_seq: i64,
    unread: u32,
) -> InboxEntry {
    let mut root = world.message(conversation.id, &world.other, root_seq, "a root");
    root.thread = Some(ThreadSummary {
        reply_count: 2,
        last_reply_seq: seq(last_seq),
        last_reply_at: Timestamp::now(),
        recent_repliers: vec![world.other.id],
        as_of: seq(last_seq),
    });
    let mut reply = world.message(conversation.id, &world.other, last_seq, "latest reply");
    reply.thread_root = Some(root.id);
    InboxEntry {
        state: ThreadUserState {
            root_id: root.id,
            conversation_id: conversation.id,
            following: true,
            last_read: seq(0),
            unread_count: unread,
            mention_count: 0,
        },
        root,
        last_reply: Some(reply),
        conversation: conversation.clone(),
    }
}

fn inbox_requests(out: &Output) -> Vec<(u64, lazy_protocol::InboxQuery)> {
    out.effects
        .iter()
        .filter_map(|e| match e {
            Effect::LoadInbox {
                generation, query, ..
            } => Some((*generation, query.clone())),
            _ => None,
        })
        .collect()
}

fn page(entries: Vec<InboxEntry>, has_more: bool) -> Result<InboxPage, RequestError> {
    Ok(InboxPage { entries, has_more })
}

#[test]
fn opening_the_inbox_asks_for_the_first_page_and_shows_the_answer() {
    let mut fx = setup();
    assert!(fx.driver.core.inbox().is_none());
    let out = fx.driver.send(Input::OpenInbox);
    let (generation, query) = inbox_requests(&out).pop().unwrap();
    assert_eq!(query.limit, Some(INBOX_PAGE));
    assert_eq!((query.unread, query.before), (None, None));
    let view = fx.driver.core.inbox().unwrap();
    assert_eq!(view.status, InboxStatus::Loading);
    assert!(view.entries.is_empty());
    let first = entry(&fx.world, &fx.world.channel, 20, 30, 2);
    let out = fx.driver.send(Input::InboxLoaded {
        generation,
        result: page(vec![first.clone()], true),
    });
    assert!(out.changes.contains(&Change::InboxChanged));
    let view = fx.driver.core.inbox().unwrap();
    assert_eq!(view.status, InboxStatus::Ready);
    assert_eq!(view.entries, vec![first.clone()]);
    assert!(view.has_more);
    assert_eq!(fx.driver.core.inbox_entry_title(&first), "#general");
    assert!(fx.driver.core.inbox_entry_joined(&first));
    // Opening again changes nothing.
    assert!(inbox_requests(&fx.driver.send(Input::OpenInbox)).is_empty());
}

#[test]
fn more_threads_are_asked_from_the_oldest_shown_and_never_twice() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let out = fx.driver.send(Input::OpenInbox);
    let (generation, _) = inbox_requests(&out).pop().unwrap();
    let a = entry(&fx.world, &general, 20, 40, 0);
    let b = entry(&fx.world, &general, 21, 30, 0);
    fx.driver.send(Input::InboxLoaded {
        generation,
        result: page(vec![a.clone(), b.clone()], true),
    });
    let out = fx.driver.send(Input::LoadMoreInbox);
    let (more, query) = inbox_requests(&out).pop().unwrap();
    assert_eq!(query.before, Some(seq(30)), "older than the oldest shown");
    assert!(
        inbox_requests(&fx.driver.send(Input::LoadMoreInbox)).is_empty(),
        "one at a time"
    );
    let c = entry(&fx.world, &general, 22, 10, 0);
    fx.driver.send(Input::InboxLoaded {
        generation: more,
        result: page(vec![b.clone(), c.clone()], false),
    });
    let view = fx.driver.core.inbox().unwrap();
    assert_eq!(
        view.entries,
        vec![a, b, c],
        "a thread shown is not added again"
    );
    assert!(!view.has_more);
    assert!(inbox_requests(&fx.driver.send(Input::LoadMoreInbox)).is_empty());
}

#[test]
fn the_filter_reloads_and_an_older_answer_is_never_shown() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let out = fx.driver.send(Input::OpenInbox);
    let (old, _) = inbox_requests(&out).pop().unwrap();
    let out = fx.driver.send(Input::LoadInbox { unread_only: true });
    let (new, query) = inbox_requests(&out).pop().unwrap();
    assert!(new > old);
    assert_eq!(query.unread, Some(true));
    assert!(fx.driver.core.inbox().unwrap().unread_only);
    fx.driver.send(Input::InboxLoaded {
        generation: old,
        result: page(vec![entry(&fx.world, &general, 20, 30, 1)], false),
    });
    assert!(fx.driver.core.inbox().unwrap().entries.is_empty());
    assert_eq!(fx.driver.core.inbox().unwrap().status, InboxStatus::Loading);
    fx.driver.send(Input::InboxLoaded {
        generation: new,
        result: page(vec![], false),
    });
    assert_eq!(fx.driver.core.inbox().unwrap().status, InboxStatus::Ready);
    // Closing stops what is out, and a late answer finds no inbox.
    let out = fx.driver.send(Input::LoadInbox { unread_only: false });
    let (late, _) = inbox_requests(&out).pop().unwrap();
    let out = fx.driver.send(Input::CloseInbox);
    assert!(out.effects.contains(&Effect::CancelInbox));
    fx.driver.send(Input::InboxLoaded {
        generation: late,
        result: page(vec![], false),
    });
    assert!(fx.driver.core.inbox().is_none());
}

#[test]
fn inbox_failures_say_what_happened_and_a_failed_next_page_keeps_the_threads() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let out = fx.driver.send(Input::OpenInbox);
    let (generation, _) = inbox_requests(&out).pop().unwrap();
    fx.driver.send(Input::InboxLoaded {
        generation,
        result: Err(lost()),
    });
    assert_eq!(
        fx.driver.core.inbox().unwrap().status,
        InboxStatus::Failed {
            reason: "Couldn't reach the server.".into(),
            can_retry: true
        }
    );
    let out = fx.driver.send(Input::LoadInbox { unread_only: false });
    let (generation, _) = inbox_requests(&out).pop().unwrap();
    let a = entry(&fx.world, &general, 20, 40, 0);
    fx.driver.send(Input::InboxLoaded {
        generation,
        result: page(vec![a.clone()], true),
    });
    let out = fx.driver.send(Input::LoadMoreInbox);
    let (more, _) = inbox_requests(&out).pop().unwrap();
    fx.driver.send(Input::InboxLoaded {
        generation: more,
        result: Err(rejected(ErrorCode::RateLimited, "slow")),
    });
    let view = fx.driver.core.inbox().unwrap();
    assert_eq!(view.entries, vec![a]);
    assert!(matches!(
        view.status,
        InboxStatus::Failed {
            can_retry: true,
            ..
        }
    ));
    // Asking again is allowed.
    assert_eq!(
        inbox_requests(&fx.driver.send(Input::LoadMoreInbox)).len(),
        1
    );
    // A workspace that is gone is a notice and no retry.
    let out = fx.driver.send(Input::LoadInbox { unread_only: false });
    let (generation, _) = inbox_requests(&out).pop().unwrap();
    fx.driver.send(Input::InboxLoaded {
        generation,
        result: Err(rejected(ErrorCode::Forbidden, "no")),
    });
    assert!(matches!(
        fx.driver.core.inbox().unwrap().status,
        InboxStatus::Failed {
            can_retry: false,
            ..
        }
    ));
    assert!(fx.driver.core.notice().is_some());
}

fn open_inbox_with(fx: &mut Fx, entries: Vec<InboxEntry>) {
    let out = fx.driver.send(Input::OpenInbox);
    let (generation, _) = inbox_requests(&out).pop().unwrap();
    fx.driver.send(Input::InboxLoaded {
        generation,
        result: page(entries, false),
    });
}

#[test]
fn threads_shown_follow_their_events() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let a = entry(&fx.world, &general, 20, 5, 1);
    let b = entry(&fx.world, &general, 21, 8, 0);
    open_inbox_with(&mut fx, vec![b.clone(), a.clone()]);

    // The person's state for a thread changes.
    let mut state = a.state.clone();
    state.unread_count = 5;
    fx.server_event(Event::ThreadStateChanged {
        state: state.clone(),
    });
    assert_eq!(
        fx.driver.core.inbox().unwrap().entries[1]
            .state
            .unread_count,
        5
    );

    // A new reply moves the thread to the top and becomes its newest reply.
    let mut summary = a.root.thread.clone().unwrap();
    let latest = fx.next();
    summary.reply_count = 3;
    summary.last_reply_seq = seq(latest);
    summary.as_of = seq(latest);
    let mut reply = fx
        .world
        .message(general.id, &fx.other(), latest, "brand new");
    reply.thread_root = Some(a.root.id);
    fx.driver.send(fx.world.event(reply.clone(), None));
    fx.server_event(Event::ThreadChanged {
        conversation_id: general.id,
        root_id: a.root.id,
        summary: Some(summary),
    });
    let view = fx.driver.core.inbox().unwrap();
    assert_eq!(view.entries[0].root.id, a.root.id);
    assert_eq!(view.entries[0].root.thread.as_ref().unwrap().reply_count, 3);
    assert_eq!(view.entries[0].last_reply.as_ref().unwrap().id, reply.id);

    // Unfollowing removes it; losing the replies removes it too.
    let mut unfollowed = state;
    unfollowed.following = false;
    fx.server_event(Event::ThreadStateChanged { state: unfollowed });
    let ids: Vec<MessageId> = fx
        .driver
        .core
        .inbox()
        .unwrap()
        .entries
        .iter()
        .map(|e| e.root.id)
        .collect();
    assert_eq!(ids, vec![b.root.id]);
    fx.server_event(Event::ThreadChanged {
        conversation_id: general.id,
        root_id: b.root.id,
        summary: None,
    });
    assert!(fx.driver.core.inbox().unwrap().entries.is_empty());
}

#[test]
fn a_thread_that_is_not_listed_asks_for_the_first_page_again_quietly() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let entries = vec![entry(&fx.world, &general, 20, 30, 0)];
    open_inbox_with(&mut fx, entries);
    let newcomer = entry(&fx.world, &general, 25, 60, 1);
    let out = fx.server_event(Event::ThreadStateChanged {
        state: newcomer.state.clone(),
    });
    let (generation, _) = inbox_requests(&out).pop().expect("asked again");
    let view = fx.driver.core.inbox().unwrap();
    assert_eq!(
        view.status,
        InboxStatus::Ready,
        "the list stays as it was meanwhile"
    );
    assert_eq!(view.entries.len(), 1);
    fx.driver.send(Input::InboxLoaded {
        generation,
        result: page(
            vec![newcomer.clone(), entry(&fx.world, &general, 20, 30, 0)],
            false,
        ),
    });
    assert_eq!(fx.driver.core.inbox().unwrap().entries.len(), 2);
}

#[test]
fn after_a_resync_the_inbox_is_asked_for_again_because_threads_may_have_changed_unseen() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let entries = vec![entry(&fx.world, &general, 20, 30, 0)];
    open_inbox_with(&mut fx, entries);
    // The log is skipped; the snapshot says a thread is followed that the inbox has never seen.
    fx.driver.send(Input::Frame(ServerFrame::SnapshotRequired {
        workspace_id: fx.world.workspace.id,
        reason: lazy_protocol::SnapshotReason::CursorExpired,
    }));
    let out = fx
        .driver
        .send(Input::BootstrapLoaded(Box::new(fx.world.bootstrap())));
    let (generation, query) = inbox_requests(&out).pop().expect("asked again");
    assert_eq!(query.before, None, "the first page");
    let view = fx.driver.core.inbox().unwrap();
    assert_eq!(
        view.status,
        InboxStatus::Ready,
        "quietly: the list stays meanwhile"
    );
    assert_eq!(view.entries.len(), 1);
    let newcomer = entry(&fx.world, &general, 25, 60, 1);
    fx.driver.send(Input::InboxLoaded {
        generation,
        result: page(vec![newcomer.clone()], false),
    });
    assert_eq!(fx.driver.core.inbox().unwrap().entries.len(), 1);
    assert_eq!(
        fx.driver.core.inbox().unwrap().entries[0].root.id,
        newcomer.root.id
    );
    // An inbox never opened, or never answered, has nothing to refresh.
    let mut idle = setup();
    idle.driver
        .send(Input::Frame(ServerFrame::SnapshotRequired {
            workspace_id: idle.world.workspace.id,
            reason: lazy_protocol::SnapshotReason::CursorExpired,
        }));
    let out = idle
        .driver
        .send(Input::BootstrapLoaded(Box::new(idle.world.bootstrap())));
    assert!(inbox_requests(&out).is_empty());
}

#[test]
fn an_event_during_a_request_makes_the_core_ask_again_when_it_is_answered() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let out = fx.driver.send(Input::OpenInbox);
    let (generation, _) = inbox_requests(&out).pop().unwrap();
    let a = entry(&fx.world, &general, 20, 30, 1);
    let mut state = a.state.clone();
    state.unread_count = 0;
    fx.server_event(Event::ThreadStateChanged { state });
    let out = fx.driver.send(Input::InboxLoaded {
        generation,
        result: page(vec![a], false),
    });
    let (again, _) = inbox_requests(&out)
        .pop()
        .expect("the answer may predate the event");
    assert!(again > generation);
}

#[test]
fn deleting_a_root_a_reply_or_losing_the_conversation_updates_the_entries() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let random = fx.world.second_channel.clone();
    let a = entry(&fx.world, &general, 20, 30, 1);
    let b = entry(&fx.world, &random, 21, 40, 1);
    open_inbox_with(&mut fx, vec![b.clone(), a.clone()]);
    // The newest reply is deleted: it is no longer shown as the newest.
    let reply = a.last_reply.clone().unwrap();
    let at = fx.next();
    fx.driver
        .send(fx.world.deleted_event(at, &fx.world.tombstone(&reply)));
    let view = fx.driver.core.inbox().unwrap();
    assert!(
        view.entries
            .iter()
            .find(|e| e.root.id == a.root.id)
            .unwrap()
            .last_reply
            .is_none()
    );
    // The root is deleted: it shows as deleted.
    let at = fx.next();
    fx.driver
        .send(fx.world.deleted_event(at, &fx.world.tombstone(&a.root)));
    assert!(
        fx.driver
            .core
            .inbox()
            .unwrap()
            .entries
            .iter()
            .find(|e| e.root.id == a.root.id)
            .unwrap()
            .root
            .deleted
    );
    // Leaving a conversation takes its threads away.
    let me = fx.world.me.id;
    fx.server_event(Event::MemberRemoved {
        conversation_id: random.id,
        user_id: me,
    });
    let ids: Vec<MessageId> = fx
        .driver
        .core
        .inbox()
        .unwrap()
        .entries
        .iter()
        .map(|e| e.root.id)
        .collect();
    assert_eq!(ids, vec![a.root.id]);
}

#[test]
fn an_edit_updates_the_root_and_reply_shown() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let a = entry(&fx.world, &general, 20, 30, 0);
    open_inbox_with(&mut fx, vec![a.clone()]);
    let edited = fx.world.edited(&a.root, "a root, reworded");
    let at = fx.next();
    fx.driver.send(fx.world.edited_event(at, &edited, None));
    assert_eq!(fx.driver.core.inbox().unwrap().entries[0].root, edited);
}

#[test]
fn marking_every_thread_read_is_shown_at_once_and_repeats_up_to_the_cap() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let entries = vec![entry(&fx.world, &general, 20, 30, 4)];
    open_inbox_with(&mut fx, entries);
    let out = fx.driver.send(Input::MarkInboxRead);
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::MarkInboxRead { .. }))
    );
    let view = fx.driver.core.inbox().unwrap();
    assert!(view.marking_read);
    assert_eq!(view.entries[0].state.unread_count, 0);
    assert!(
        fx.driver.send(Input::MarkInboxRead).effects.is_empty(),
        "one run at a time"
    );
    // The server stopped at its cap: ask again.
    let out = fx.driver.send(Input::InboxReadResult {
        result: Ok(InboxReadResponse {
            updated: INBOX_READ_CAP,
        }),
    });
    assert_eq!(out.effects.len(), 1);
    assert!(fx.driver.core.inbox().unwrap().marking_read);
    fx.driver.send(Input::InboxReadResult {
        result: Ok(InboxReadResponse { updated: 3 }),
    });
    assert!(!fx.driver.core.inbox().unwrap().marking_read);
    // A failure says so and shows what the server holds.
    fx.driver.send(Input::MarkInboxRead);
    let out = fx.driver.send(Input::InboxReadResult {
        result: Err(lost()),
    });
    assert!(fx.driver.core.notice().unwrap().contains("Couldn't mark"));
    assert_eq!(inbox_requests(&out).len(), 1, "asked for the list again");
    assert!(!fx.driver.core.inbox().unwrap().marking_read);
}

#[test]
fn the_inbox_is_forgotten_at_sign_out_and_when_the_session_expires() {
    let mut fx = setup();
    open_inbox_with(&mut fx, vec![]);
    fx.driver.send(Input::SignOut);
    assert!(fx.driver.core.inbox().is_none());
    let mut fx = setup();
    let out = fx.driver.send(Input::OpenInbox);
    let (generation, _) = inbox_requests(&out).pop().unwrap();
    fx.driver.send(Input::InboxLoaded {
        generation,
        result: Err(rejected(ErrorCode::Unauthenticated, "expired")),
    });
    assert!(fx.driver.core.inbox().is_none());
    assert!(matches!(
        fx.driver.core.phase(),
        SessionPhase::SignedOut { .. }
    ));
    // The inbox needs a signed-in person.
    let mut signed_out = Driver::new();
    assert!(signed_out.send(Input::OpenInbox).effects.is_empty());
    let _ = channel;
}

#[test]
fn the_sidebar_count_is_the_unread_replies_of_followed_threads() {
    let mut world = World::new();
    let general = world.channel.id;
    let root = world.message(general, &world.me, 4, "root");
    world.threads = vec![ThreadUserState {
        root_id: root.id,
        conversation_id: general,
        following: true,
        last_read: seq(0),
        unread_count: 3,
        mention_count: 0,
    }];
    let fx = setup_with(world);
    assert_eq!(fx.driver.core.inbox_unread_total(), 3);
}
