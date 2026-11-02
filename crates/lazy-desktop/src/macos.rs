//! macOS notifications through the system's notification centre (`UserNotifications`), so they
//! carry the app's own name and icon, are controlled in System Settings → Notifications → Lazy,
//! and can be clicked to open the message.
//!
//! This works only from an installed, signed app bundle (`Lazy.app`). Calling the framework from
//! a bare binary (`cargo run`) raises an Objective-C exception that Rust cannot catch, so
//! [`UserNotificationsNotifier::start`] checks for a bundle first and returns `None` without
//! touching the framework when there is none.
//!
//! The person is asked once, by the system's own dialog, and only when the client core says so
//! (`Effect::RequestNotificationPermission`); nothing here asks by itself. The framework calls
//! back on its own threads, so every callback only forwards a plain value.

use std::path::Path;
use std::process::Command;
use std::ptr::NonNull;
use std::str::FromStr;
use std::sync::Arc;

use block2::RcBlock;
use lazy_domain::notification::NotificationPermission;
use lazy_domain::{ConversationId, MessageId};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send};
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSBundle, NSDictionary, NSError, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSettings, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use crate::notifier::{Activation, Notifier, Toast};

/// Where System Settings shows the notification settings (macOS 13 and later).
const SETTINGS_URL: &str = "x-apple.systempreferences:com.apple.Notifications-Settings.extension";

const KEY_CONVERSATION: &str = "conversation";
const KEY_MESSAGE: &str = "message";

type OnActivate = Arc<dyn Fn(Activation) + Send + Sync>;
type OnPermission = Arc<dyn Fn(NotificationPermission) + Send + Sync>;

/// Whether `exe` sits at `Something.app/Contents/MacOS/<name>`.
pub fn is_in_app_bundle(exe: &Path) -> bool {
    let name = |path: Option<&Path>| {
        path.and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_owned)
    };
    let macos = exe.parent();
    let contents = macos.and_then(Path::parent);
    let app = contents.and_then(Path::parent);
    name(macos).as_deref() == Some("MacOS")
        && name(contents).as_deref() == Some("Contents")
        && app
            .and_then(|p| p.extension())
            .is_some_and(|ext| ext == "app")
}

/// Whether this process may use `UserNotifications`: an app bundle with an identifier.
fn is_bundled() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    is_in_app_bundle(&exe) && NSBundle::mainBundle().bundleIdentifier().is_some()
}

/// What the system's answer means to the client.
fn permission_of(status: UNAuthorizationStatus) -> NotificationPermission {
    if status == UNAuthorizationStatus::NotDetermined {
        NotificationPermission::NotDetermined
    } else if status == UNAuthorizationStatus::Denied {
        NotificationPermission::Denied
    } else if status == UNAuthorizationStatus::Authorized
        || status == UNAuthorizationStatus::Provisional
        || status == UNAuthorizationStatus::Ephemeral
    {
        NotificationPermission::Authorized
    } else {
        NotificationPermission::Unavailable
    }
}

struct DelegateIvars {
    on_activate: OnActivate,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this class does not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "LazyNotificationDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        // A notification arriving while the app is running (and not focused on that message,
        // which the client core has already decided) is shown like any other.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        // The notification was clicked: open the message it is about.
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &block2::DynBlock<dyn Fn()>,
        ) {
            let info = response.notification().request().content().userInfo();
            let read = |key: &str| -> Option<String> {
                let value = info.objectForKey(&*NSString::from_str(key))?;
                let text = value.downcast_ref::<NSString>()?;
                Some(text.to_string())
            };
            if let (Some(conversation), Some(message)) = (read(KEY_CONVERSATION), read(KEY_MESSAGE))
                && let (Ok(conversation), Ok(message)) = (
                    ConversationId::from_str(&conversation),
                    MessageId::from_str(&message),
                )
            {
                (self.ivars().on_activate)(Activation {
                    conversation,
                    message,
                });
            }
            completion_handler.call(());
        }
    }
);

impl Delegate {
    fn new(on_activate: OnActivate) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { on_activate });
        // SAFETY: NSObject's `init` takes no arguments.
        unsafe { msg_send![super(this), init] }
    }
}

/// The delegate the centre holds only weakly, kept alive for as long as the notifier.
struct KeepAlive(#[allow(dead_code)] Retained<Delegate>);

// SAFETY: the delegate's only state is an `Arc<dyn Fn + Send + Sync>`, and the framework calls its
// methods from its own threads by design.
unsafe impl Send for KeepAlive {}
unsafe impl Sync for KeepAlive {}

pub struct UserNotificationsNotifier {
    _delegate: KeepAlive,
    on_permission: OnPermission,
}

impl UserNotificationsNotifier {
    /// Connects to the notification centre and reads the permission, or returns `None` (touching
    /// nothing) when this is not an installed app.
    pub fn start(
        on_activate: impl Fn(Activation) + Send + Sync + 'static,
        on_permission: impl Fn(NotificationPermission) + Send + Sync + 'static,
    ) -> Option<Self> {
        if !is_bundled() {
            return None;
        }
        let delegate = Delegate::new(Arc::new(on_activate));
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        let notifier = Self {
            _delegate: KeepAlive(delegate),
            on_permission: Arc::new(on_permission),
        };
        notifier.refresh_permission();
        Some(notifier)
    }
}

fn report(on_permission: &OnPermission) {
    let on_permission = on_permission.clone();
    let handler = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
        // SAFETY: the framework passes a valid settings object for the duration of the call.
        let status = unsafe { settings.as_ref() }.authorizationStatus();
        on_permission(permission_of(status));
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .getNotificationSettingsWithCompletionHandler(&handler);
}

impl Notifier for UserNotificationsNotifier {
    fn show(&self, toast: Toast) {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&toast.title));
        content.setBody(&NSString::from_str(&toast.body));
        // The conversation groups them in the notification centre.
        content.setThreadIdentifier(&NSString::from_str(&toast.tag));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        let keys = [
            &*NSString::from_str(KEY_CONVERSATION),
            &*NSString::from_str(KEY_MESSAGE),
        ];
        let values: [Retained<AnyObject>; 2] = [
            NSString::from_str(&toast.conversation.to_string()).into(),
            NSString::from_str(&toast.message.to_string()).into(),
        ];
        let info = NSDictionary::from_retained_objects(&keys, &values);
        // SAFETY: a dictionary of strings, which is property-list safe as the property requires.
        unsafe {
            content.setUserInfo(&Retained::cast_unchecked::<
                NSDictionary<AnyObject, AnyObject>,
            >(info))
        };
        // The same identifier replaces the earlier notification of that conversation.
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&toast.tag),
            &content,
            None,
        );
        let handler = RcBlock::new(|error: *mut NSError| {
            if !error.is_null() {
                tracing::warn!("the system refused a notification");
            }
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, Some(&handler));
    }

    fn withdraw(&self, tag: &str) {
        let ids = objc2_foundation::NSArray::from_retained_slice(&[NSString::from_str(tag)]);
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.removeDeliveredNotificationsWithIdentifiers(&ids);
        center.removePendingNotificationRequestsWithIdentifiers(&ids);
    }

    fn set_launcher_count(&self, count: u32) {
        // The dock tile belongs to the main thread, which is where effects run.
        let Some(main) = MainThreadMarker::new() else {
            return;
        };
        let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
        NSApplication::sharedApplication(main)
            .dockTile()
            .setBadgeLabel(label.as_deref());
    }

    fn refresh_permission(&self) {
        report(&self.on_permission);
    }

    fn request_permission(&self) {
        let on_permission = self.on_permission.clone();
        let handler = RcBlock::new(move |_granted: Bool, error: *mut NSError| {
            if !error.is_null() {
                tracing::warn!("the notification permission could not be requested");
            }
            // Whatever the answer, read back what the system now says.
            report(&on_permission);
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert
                    | UNAuthorizationOptions::Sound
                    | UNAuthorizationOptions::Badge,
                &handler,
            );
    }

    fn open_settings(&self) {
        if let Err(error) = Command::new("/usr/bin/open").arg(SETTINGS_URL).spawn() {
            tracing::warn!(%error, "could not open the system settings");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn only_a_binary_inside_an_app_bundle_counts() {
        let yes = |p: &str| is_in_app_bundle(&PathBuf::from(p));
        assert!(yes("/Applications/Lazy.app/Contents/MacOS/lazy"));
        assert!(yes("/tmp/x y/Lazy Dev.app/Contents/MacOS/lazy"));
        assert!(!yes("/Users/me/Projects/lazy/target/release/lazy-desktop"));
        assert!(!yes("/Applications/Lazy.app/Contents/Resources/lazy"));
        assert!(!yes("/Applications/Lazy/Contents/MacOS/lazy"));
        assert!(!yes("lazy"));
    }

    #[test]
    fn the_systems_answer_maps_to_the_clients_four_states() {
        assert_eq!(
            permission_of(UNAuthorizationStatus::NotDetermined),
            NotificationPermission::NotDetermined
        );
        assert_eq!(
            permission_of(UNAuthorizationStatus::Denied),
            NotificationPermission::Denied
        );
        for granted in [
            UNAuthorizationStatus::Authorized,
            UNAuthorizationStatus::Provisional,
            UNAuthorizationStatus::Ephemeral,
        ] {
            assert_eq!(permission_of(granted), NotificationPermission::Authorized);
        }
    }
}
