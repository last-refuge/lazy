//! Desktop notifications and the launcher's count: one small interface, so another platform
//! adds an implementation without touching the rules that decide what to show.
//!
//! What to notify about is decided by the client core (D20). This module only shows what it is
//! given. Nothing here may block the UI thread or take the app down: the Linux implementation
//! does its D-Bus calls on worker threads, and every failure is logged without any message
//! content and then ignored. In a session with no notification service the notifier does
//! nothing, once logged.

use std::sync::Mutex;

use lazy_domain::{ConversationId, MessageId};

/// One notification to show. A newer toast with the same `tag` replaces the earlier one (the
/// tag is the conversation, so a busy conversation is one notification saying how many).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub tag: String,
    pub title: String,
    pub body: String,
    /// What clicking it opens, as a permalink would.
    pub conversation: ConversationId,
    pub message: MessageId,
}

/// A notification was clicked: open this message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activation {
    pub conversation: ConversationId,
    pub message: MessageId,
}

/// What a platform provides.
pub trait Notifier: Send + Sync {
    /// Shows `toast`, replacing the one with the same tag.
    fn show(&self, toast: Toast);
    /// Takes down the notification with this tag, if it is showing.
    fn withdraw(&self, tag: &str);
    /// Sets the launcher's count (the dock or taskbar badge), where the platform has one.
    fn set_launcher_count(&self, count: u32);
}

/// The window title for an attention count: `Lazy`, `(3) Lazy`, `(99+) Lazy`.
pub fn badge_title(count: u32) -> String {
    match count {
        0 => "Lazy".to_owned(),
        1..=99 => format!("({count}) Lazy"),
        _ => "(99+) Lazy".to_owned(),
    }
}

/// What a [`FakeNotifier`] was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Show(Toast),
    Withdraw(String),
    LauncherCount(u32),
}

/// What the platform calls when a notification is clicked.
type OnActivate = Box<dyn Fn(Activation) + Send + Sync>;

/// Records calls, for tests of the wiring.
#[derive(Default)]
pub struct FakeNotifier {
    calls: Mutex<Vec<Call>>,
    on_activate: Mutex<Option<OnActivate>>,
}

impl FakeNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything asked so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls").clone()
    }

    /// Takes the calls so far.
    pub fn take(&self) -> Vec<Call> {
        std::mem::take(&mut *self.calls.lock().expect("calls"))
    }

    /// Sets what a click on a notification does, as the platform's callback would.
    pub fn on_activate(&self, callback: impl Fn(Activation) + Send + Sync + 'static) {
        *self.on_activate.lock().expect("callback") = Some(Box::new(callback));
    }

    /// Clicks the notification showing with this tag, if there is one: calls the callback with
    /// what it opens. Returns whether there was one.
    pub fn click(&self, tag: &str) -> bool {
        let Some(toast) = self.showing(tag) else {
            return false;
        };
        if let Some(callback) = self.on_activate.lock().expect("callback").as_ref() {
            callback(Activation {
                conversation: toast.conversation,
                message: toast.message,
            });
        }
        true
    }

    /// The notification showing now with this tag: the last shown and not since withdrawn.
    pub fn showing(&self, tag: &str) -> Option<Toast> {
        let mut current = None;
        for call in self.calls.lock().expect("calls").iter() {
            match call {
                Call::Show(toast) if toast.tag == tag => current = Some(toast.clone()),
                Call::Withdraw(withdrawn) if withdrawn == tag => current = None,
                _ => {}
            }
        }
        current
    }
}

impl Notifier for FakeNotifier {
    fn show(&self, toast: Toast) {
        self.calls.lock().expect("calls").push(Call::Show(toast));
    }

    fn withdraw(&self, tag: &str) {
        self.calls
            .lock()
            .expect("calls")
            .push(Call::Withdraw(tag.to_owned()));
    }

    fn set_launcher_count(&self, count: u32) {
        self.calls
            .lock()
            .expect("calls")
            .push(Call::LauncherCount(count));
    }
}

/// Does nothing: for platforms without an implementation yet.
pub struct NoNotifier;

impl Notifier for NoNotifier {
    fn show(&self, _toast: Toast) {}
    fn withdraw(&self, _tag: &str) {}
    fn set_launcher_count(&self, _count: u32) {}
}

/// The platform's notifier, calling `on_activate` (from a worker thread) when a notification is
/// clicked.
pub fn platform_notifier(
    on_activate: impl Fn(Activation) + Send + Sync + 'static,
) -> Box<dyn Notifier> {
    #[cfg(target_os = "linux")]
    {
        Box::new(freedesktop::FreedesktopNotifier::start(on_activate))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = on_activate;
        Box::new(NoNotifier)
    }
}

#[cfg(target_os = "linux")]
pub mod freedesktop {
    //! The freedesktop notification service (`org.freedesktop.Notifications`) over D-Bus.
    //!
    //! One worker thread owns the calls; another listens for the service's `ActionInvoked` and
    //! `NotificationClosed` signals. A notification is replaced by sending the id the service
    //! gave the earlier one, and taken down by closing that id. Clicking the notification's body
    //! invokes the `default` action, which opens the message.

    use std::collections::HashMap;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use zbus::blocking::{Connection, MessageIterator};
    use zbus::zvariant::Value;

    use super::{Activation, Notifier, Toast};

    const SERVICE: &str = "org.freedesktop.Notifications";
    const PATH: &str = "/org/freedesktop/Notifications";
    /// The application's desktop entry, so the daemon can group and show its icon.
    const DESKTOP_ENTRY: &str = "lazy";

    enum Command {
        Show(Toast),
        Withdraw(String),
        Count(u32),
    }

    /// Notification ids still showing, with what clicking each opens.
    type Open = Arc<Mutex<HashMap<u32, Activation>>>;

    pub struct FreedesktopNotifier {
        commands: Sender<Command>,
    }

    impl FreedesktopNotifier {
        pub fn start(on_activate: impl Fn(Activation) + Send + Sync + 'static) -> Self {
            let (commands, inbox) = channel();
            let open: Open = Arc::default();
            let spawned = thread::Builder::new().name("lazy-notifier".into()).spawn({
                let open = open.clone();
                move || work(inbox, open, Arc::new(on_activate))
            });
            if let Err(error) = spawned {
                tracing::warn!(%error, "could not start the notification worker");
            }
            Self { commands }
        }
    }

    impl Notifier for FreedesktopNotifier {
        fn show(&self, toast: Toast) {
            self.commands.send(Command::Show(toast)).ok();
        }

        fn withdraw(&self, tag: &str) {
            self.commands.send(Command::Withdraw(tag.to_owned())).ok();
        }

        fn set_launcher_count(&self, count: u32) {
            self.commands.send(Command::Count(count)).ok();
        }
    }

    fn work(
        inbox: Receiver<Command>,
        open: Open,
        on_activate: Arc<dyn Fn(Activation) + Send + Sync>,
    ) {
        let connection = match Connection::session() {
            Ok(connection) => connection,
            Err(error) => {
                // No session bus: there is nothing to show notifications with. Keep taking
                // commands so senders never block, and do nothing with them.
                tracing::warn!(%error, "no session bus: desktop notifications are off");
                for _ in inbox {}
                return;
            }
        };
        listen(&connection, open.clone(), on_activate);
        // The id the service gave the notification showing for each tag.
        let mut ids: HashMap<String, u32> = HashMap::new();
        let mut failing = false;
        for command in inbox {
            match command {
                Command::Show(toast) => {
                    let replaces = ids.get(&toast.tag).copied().unwrap_or(0);
                    match notify(&connection, &toast, replaces) {
                        Ok(id) => {
                            failing = false;
                            if replaces != 0 && replaces != id {
                                open.lock().expect("open").remove(&replaces);
                            }
                            ids.insert(toast.tag.clone(), id);
                            open.lock().expect("open").insert(
                                id,
                                Activation {
                                    conversation: toast.conversation,
                                    message: toast.message,
                                },
                            );
                        }
                        Err(error) => {
                            // Said once per run of failures, never with the message's content.
                            if !failing {
                                tracing::warn!(%error, "could not show a notification");
                            }
                            failing = true;
                        }
                    }
                }
                Command::Withdraw(tag) => {
                    if let Some(id) = ids.remove(&tag) {
                        open.lock().expect("open").remove(&id);
                        if let Err(error) = close(&connection, id) {
                            tracing::debug!(%error, "could not close a notification");
                        }
                    }
                }
                Command::Count(count) => {
                    if let Err(error) = launcher_count(&connection, count) {
                        tracing::debug!(%error, "could not set the launcher count");
                    }
                }
            }
        }
    }

    fn notify(connection: &Connection, toast: &Toast, replaces: u32) -> zbus::Result<u32> {
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("desktop-entry", Value::from(DESKTOP_ENTRY));
        hints.insert("category", Value::from("im.received"));
        let reply = connection.call_method(
            Some(SERVICE),
            PATH,
            Some(SERVICE),
            "Notify",
            &(
                super::super::APP_NAME,
                replaces,
                // No icon of our own yet: the daemon shows its default.
                "",
                toast.title.as_str(),
                toast.body.as_str(),
                vec!["default", "Open"],
                hints,
                // The service's own default timeout.
                -1i32,
            ),
        )?;
        reply.body().deserialize::<u32>()
    }

    fn close(connection: &Connection, id: u32) -> zbus::Result<()> {
        connection
            .call_method(
                Some(SERVICE),
                PATH,
                Some(SERVICE),
                "CloseNotification",
                &(id,),
            )
            .map(|_| ())
    }

    /// The Unity launcher entry protocol, which several docks and shells honor.
    fn launcher_count(connection: &Connection, count: u32) -> zbus::Result<()> {
        let mut properties: HashMap<&str, Value<'_>> = HashMap::new();
        properties.insert("count", Value::from(i64::from(count)));
        properties.insert("count-visible", Value::from(count > 0));
        connection.emit_signal(
            None::<&str>,
            "/com/canonical/unity/launcherentry/1",
            "com.canonical.Unity.LauncherEntry",
            "Update",
            &("application://lazy.desktop", properties),
        )
    }

    /// Hears the service's signals: a click on a notification opens its message, and a closed
    /// notification is forgotten.
    fn listen(
        connection: &Connection,
        open: Open,
        on_activate: Arc<dyn Fn(Activation) + Send + Sync>,
    ) {
        let rule = match zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(SERVICE)
        {
            Ok(builder) => builder.build(),
            Err(error) => {
                tracing::warn!(%error, "could not listen for notification actions");
                return;
            }
        };
        let signals = match MessageIterator::for_match_rule(rule, connection, None) {
            Ok(signals) => signals,
            Err(error) => {
                tracing::warn!(%error, "could not listen for notification actions");
                return;
            }
        };
        let spawned = thread::Builder::new()
            .name("lazy-notifier-signals".into())
            .spawn(move || {
                for message in signals.flatten() {
                    let header = message.header();
                    let Some(member) = header.member().map(|member| member.as_str().to_owned())
                    else {
                        continue;
                    };
                    let body = message.body();
                    match member.as_str() {
                        "ActionInvoked" => {
                            let Ok((id, action)) = body.deserialize::<(u32, String)>() else {
                                continue;
                            };
                            if action == "default" || action == "Open" {
                                let target = open.lock().expect("open").get(&id).copied();
                                if let Some(target) = target {
                                    on_activate(target);
                                }
                            }
                        }
                        "NotificationClosed" => {
                            if let Ok((id, _reason)) = body.deserialize::<(u32, u32)>() {
                                open.lock().expect("open").remove(&id);
                            }
                        }
                        _ => {}
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the notification listener");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toast(tag: &str, body: &str) -> Toast {
        Toast {
            tag: tag.to_owned(),
            title: "#design".to_owned(),
            body: body.to_owned(),
            conversation: ConversationId::generate(),
            message: MessageId::generate(),
        }
    }

    #[test]
    fn the_title_carries_the_count_up_to_99() {
        assert_eq!(badge_title(0), "Lazy");
        assert_eq!(badge_title(1), "(1) Lazy");
        assert_eq!(badge_title(99), "(99) Lazy");
        assert_eq!(badge_title(100), "(99+) Lazy");
        assert_eq!(badge_title(4_000), "(99+) Lazy");
    }

    #[test]
    fn the_fake_records_calls_in_order_and_knows_what_is_showing() {
        let fake = FakeNotifier::new();
        fake.show(toast("a", "first"));
        fake.show(toast("b", "other"));
        fake.show(toast("a", "second"));
        assert_eq!(
            fake.showing("a").unwrap().body,
            "second",
            "a newer one replaces"
        );
        fake.withdraw("a");
        assert_eq!(fake.showing("a"), None);
        assert!(fake.showing("b").is_some());
        fake.set_launcher_count(3);
        let calls = fake.take();
        assert_eq!(calls.len(), 5);
        assert_eq!(calls.last(), Some(&Call::LauncherCount(3)));
        assert!(fake.calls().is_empty());
    }
}
