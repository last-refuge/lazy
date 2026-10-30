//! Wiring between the client runtime and the UI, shared by the desktop binary and the
//! end-to-end tests that drive the real views.
//!
//! The runtime performs side effects on its own threads; the UI model lives on GPUI's foreground
//! executor. [`start_model`] connects them: the model's effects go to the runtime, and everything
//! the runtime reports is delivered to the model on the foreground executor.
//!
//! [`start_desktop`] does the same and also handles the effects only a desktop can perform
//! (D20): desktop notifications, withdrawing them, and the attention count in the window title
//! and the launcher. A notification's click raises the window and opens the message.

pub mod logging;
pub mod notifier;
pub mod updater;
pub mod window_state;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{AnyWindowHandle, App, AppContext as _, Entity};
use lazy_client_core::{Effect, Input};
use lazy_client_runtime::Runtime;
use lazy_client_runtime::async_channel::{Receiver, Sender, unbounded};
use lazy_domain::MessageId;
use lazy_ui::app_model::{AppModel, EffectSink};

use crate::notifier::{Activation, Notifier, Toast, badge_title, platform_notifier};

/// The name notifications are shown under.
pub const APP_NAME: &str = "Lazy";

/// Hands the core's effects to the runtime.
pub struct RuntimeSink(Rc<Runtime>);

impl RuntimeSink {
    pub fn new(runtime: Rc<Runtime>) -> Self {
        Self(runtime)
    }
}

impl EffectSink for RuntimeSink {
    fn execute(&self, effects: Vec<Effect>) {
        self.0.execute(effects);
    }
}

/// Handles the effects only a desktop can perform and hands everything else on.
///
/// Notifications are shown by the platform's [`Notifier`]; the attention count goes to the
/// launcher through it and to the window's title through `badges`, which the foreground
/// ([`Desktop::attach`]) reads.
pub struct DesktopSink {
    inner: Rc<dyn EffectSink>,
    notifier: Arc<dyn Notifier>,
    /// The newest message each showing notification opens, by tag, so a retracted message
    /// takes down the notification that is about it and no other.
    shown: RefCell<HashMap<String, MessageId>>,
    badges: Sender<u32>,
}

impl DesktopSink {
    pub fn new(
        inner: Rc<dyn EffectSink>,
        notifier: Arc<dyn Notifier>,
        badges: Sender<u32>,
    ) -> Self {
        Self {
            inner,
            notifier,
            shown: RefCell::default(),
            badges,
        }
    }
}

impl EffectSink for DesktopSink {
    fn execute(&self, effects: Vec<Effect>) {
        let mut rest = Vec::with_capacity(effects.len());
        for effect in effects {
            match effect {
                Effect::Notify(notification) => {
                    self.shown
                        .borrow_mut()
                        .insert(notification.tag.clone(), notification.message);
                    self.notifier.show(Toast {
                        tag: notification.tag,
                        title: notification.title,
                        body: notification.body,
                        conversation: notification.conversation,
                        message: notification.message,
                    });
                }
                Effect::ClearNotifications { conversation } => {
                    let tag = conversation.to_string();
                    if self.shown.borrow_mut().remove(&tag).is_some() {
                        self.notifier.withdraw(&tag);
                    }
                }
                Effect::RetractNotification { message } => {
                    let tag = self
                        .shown
                        .borrow()
                        .iter()
                        .find(|(_, shown)| **shown == message)
                        .map(|(tag, _)| tag.clone());
                    if let Some(tag) = tag {
                        self.shown.borrow_mut().remove(&tag);
                        self.notifier.withdraw(&tag);
                    }
                }
                Effect::SetBadge(count) => {
                    self.notifier.set_launcher_count(count);
                    self.badges.try_send(count).ok();
                }
                other => rest.push(other),
            }
        }
        if !rest.is_empty() {
            self.inner.execute(rest);
        }
    }
}

/// What a click on a notification asks of the core.
pub fn activation_input(activation: Activation) -> Input {
    Input::NotificationActivated {
        conversation: activation.conversation,
        message: activation.message,
    }
}

/// Creates the app model around `runtime` and starts feeding it the runtime's reports.
///
/// The pump holds the model weakly, so it ends when the model is released or the runtime's
/// channel closes; it is detached because nothing needs to await it.
pub fn start_model(
    runtime: Rc<Runtime>,
    inputs: Receiver<Input>,
    cx: &mut App,
) -> Entity<AppModel> {
    let model = cx.new(|_| AppModel::new(Rc::new(RuntimeSink::new(runtime))));
    pump(&model, inputs, cx);
    model
}

fn pump(model: &Entity<AppModel>, inputs: Receiver<Input>, cx: &mut App) {
    let pump = model.downgrade();
    cx.spawn(async move |cx| {
        while let Ok(input) = inputs.recv().await {
            if pump
                .update(cx, |model, cx| model.dispatch(input, cx))
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

/// The model of the desktop app, and what the window needs from the platform half.
pub struct Desktop {
    pub model: Entity<AppModel>,
    badges: Receiver<u32>,
    activations: Receiver<Activation>,
}

/// Like [`start_model`], with desktop notifications and the attention count.
pub fn start_desktop(runtime: Rc<Runtime>, inputs: Receiver<Input>, cx: &mut App) -> Desktop {
    let (badge_sender, badges) = unbounded();
    let (activation_sender, activations) = unbounded();
    let notifier: Arc<dyn Notifier> = Arc::from(platform_notifier(move |activation| {
        activation_sender.try_send(activation).ok();
    }));
    let sink = DesktopSink::new(Rc::new(RuntimeSink::new(runtime)), notifier, badge_sender);
    let model = cx.new(|_| AppModel::new(Rc::new(sink)));
    pump(&model, inputs, cx);
    Desktop {
        model,
        badges,
        activations,
    }
}

impl Desktop {
    /// Connects the platform half to `window`: the attention count becomes its title, and a click
    /// on a notification raises it and opens the message.
    pub fn attach(&self, window: AnyWindowHandle, cx: &mut App) {
        let badges = self.badges.clone();
        cx.spawn(async move |cx| {
            while let Ok(count) = badges.recv().await {
                let title = badge_title(count);
                if window
                    .update(cx, |_, window, _| window.set_window_title(&title))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let activations = self.activations.clone();
        let model = self.model.downgrade();
        cx.spawn(async move |cx| {
            while let Ok(activation) = activations.recv().await {
                window
                    .update(cx, |_, window, _| window.activate_window())
                    .ok();
                if model
                    .update(cx, |model, cx| {
                        model.dispatch(activation_input(activation), cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use lazy_client_core::DesktopNotification;
    use lazy_domain::ConversationId;
    use lazy_domain::notification::NotificationReason;

    use super::*;
    use crate::notifier::{Call, FakeNotifier};

    /// Records what reaches the runtime.
    #[derive(Default)]
    struct Forwarded(RefCell<Vec<Effect>>);

    impl EffectSink for Forwarded {
        fn execute(&self, effects: Vec<Effect>) {
            self.0.borrow_mut().extend(effects);
        }
    }

    struct Rig {
        sink: DesktopSink,
        fake: Arc<FakeNotifier>,
        forwarded: Rc<Forwarded>,
        badges: Receiver<u32>,
    }

    fn rig() -> Rig {
        let fake = Arc::new(FakeNotifier::new());
        let forwarded = Rc::new(Forwarded::default());
        let (sender, badges) = unbounded();
        Rig {
            sink: DesktopSink::new(forwarded.clone(), fake.clone(), sender),
            fake,
            forwarded,
            badges,
        }
    }

    fn notification(conversation: ConversationId, body: &str) -> DesktopNotification {
        DesktopNotification {
            conversation,
            message: MessageId::generate(),
            thread_root: None,
            title: "#design".into(),
            body: body.into(),
            reason: NotificationReason::Mention,
            tag: conversation.to_string(),
            count: 1,
        }
    }

    #[test]
    fn a_notify_effect_shows_one_notification_and_a_newer_one_replaces_it() {
        let rig = rig();
        let conversation = ConversationId::generate();
        rig.sink.execute(vec![Effect::Notify(notification(
            conversation,
            "Alice: first",
        ))]);
        rig.sink.execute(vec![Effect::Notify(notification(
            conversation,
            "2 new messages",
        ))]);
        let showing = rig
            .fake
            .showing(&conversation.to_string())
            .expect("showing");
        assert_eq!(showing.body, "2 new messages");
        assert_eq!(showing.title, "#design");
        let shows = rig
            .fake
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Show(_)))
            .count();
        assert_eq!(shows, 2, "both went to the platform, which replaces by tag");
        assert!(
            rig.forwarded.0.borrow().is_empty(),
            "the runtime is not bothered with them"
        );
    }

    #[test]
    fn clearing_a_conversation_takes_its_notification_down_once() {
        let rig = rig();
        let conversation = ConversationId::generate();
        let other = ConversationId::generate();
        rig.sink.execute(vec![
            Effect::Notify(notification(conversation, "a")),
            Effect::Notify(notification(other, "b")),
        ]);
        rig.sink
            .execute(vec![Effect::ClearNotifications { conversation }]);
        assert_eq!(rig.fake.showing(&conversation.to_string()), None);
        assert!(
            rig.fake.showing(&other.to_string()).is_some(),
            "only that conversation's"
        );
        rig.fake.take();
        rig.sink
            .execute(vec![Effect::ClearNotifications { conversation }]);
        assert!(
            rig.fake.calls().is_empty(),
            "nothing showing, nothing to withdraw"
        );
    }

    #[test]
    fn retracting_a_message_takes_down_only_the_notification_about_it() {
        let rig = rig();
        let conversation = ConversationId::generate();
        let first = notification(conversation, "first");
        let first_message = first.message;
        rig.sink.execute(vec![Effect::Notify(first)]);
        let newer = notification(conversation, "newer");
        rig.sink.execute(vec![Effect::Notify(newer)]);
        // The first message was replaced by a newer one: its deletion leaves the notification.
        rig.sink.execute(vec![Effect::RetractNotification {
            message: first_message,
        }]);
        assert!(rig.fake.showing(&conversation.to_string()).is_some());
        // The newest one's deletion takes it down.
        let newest = rig.fake.showing(&conversation.to_string()).unwrap().message;
        rig.sink
            .execute(vec![Effect::RetractNotification { message: newest }]);
        assert_eq!(rig.fake.showing(&conversation.to_string()), None);
    }

    #[test]
    fn the_attention_count_goes_to_the_launcher_and_the_title() {
        let rig = rig();
        rig.sink.execute(vec![
            Effect::SetBadge(3),
            Effect::SetBadge(0),
            Effect::SetBadge(150),
        ]);
        assert_eq!(
            rig.fake.calls(),
            vec![
                Call::LauncherCount(3),
                Call::LauncherCount(0),
                Call::LauncherCount(150)
            ]
        );
        let counts: Vec<u32> = std::iter::from_fn(|| rig.badges.try_recv().ok()).collect();
        assert_eq!(counts, [3, 0, 150]);
        let titles: Vec<String> = counts.into_iter().map(badge_title).collect();
        assert_eq!(titles, ["(3) Lazy", "Lazy", "(99+) Lazy"]);
    }

    #[test]
    fn every_other_effect_goes_on_to_the_runtime_in_order() {
        let rig = rig();
        rig.sink.execute(vec![
            Effect::CancelSearch,
            Effect::SetBadge(1),
            Effect::PersistRecentSearches(vec!["a".into()]),
        ]);
        let forwarded = rig.forwarded.0.borrow();
        assert_eq!(forwarded.len(), 2);
        assert!(matches!(forwarded[0], Effect::CancelSearch));
        assert!(matches!(forwarded[1], Effect::PersistRecentSearches(_)));
    }

    #[test]
    fn a_click_on_a_notification_asks_the_core_to_open_its_message() {
        let rig = rig();
        let conversation = ConversationId::generate();
        let notice = notification(conversation, "Alice: ping");
        let message = notice.message;
        let (sender, clicks) = unbounded();
        rig.fake.on_activate(move |activation| {
            sender.try_send(activation).ok();
        });
        rig.sink.execute(vec![Effect::Notify(notice)]);
        assert!(rig.fake.click(&conversation.to_string()));
        let activation = clicks.try_recv().expect("the click reached the callback");
        match activation_input(activation) {
            Input::NotificationActivated {
                conversation: c,
                message: m,
            } => assert_eq!((c, m), (conversation, message)),
            other => panic!("{other:?}"),
        }
        assert!(!rig.fake.click("nothing-showing"));
    }
}
