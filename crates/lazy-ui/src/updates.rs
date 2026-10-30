//! Updates (D28): telling the person a newer Lazy exists, and getting it installed.
//!
//! [`UpdateModel`] holds what the desktop's updater last reported (a
//! [`lazy_client_core::UpdateStatus`]) and passes the person's choices to an [`UpdateHost`]; it
//! does no I/O and starts no processes itself. The desktop installs the model once at startup
//! ([`install`]); where nothing is installed (the tests, the screens example) no bar is drawn and
//! Help, Check for updates… says this copy does not look for updates.
//!
//! What the person sees:
//!
//! * a bar across the top of the window ([`bar`]) whenever there is a release to tell them about,
//!   fetch, or install: "Lazy 0.6.0 is available" with **Update now** and **Later**; progress
//!   while the file comes; "ready to install" with **Restart to update**; a failure with
//!   **Try again**. **Later** hides it for that release until the next start or the next time the
//!   person asks;
//! * Help, Check for updates… ([`crate::dialogs::UpdatesDialog`]): the same, plus the answer to
//!   a check ("You're up to date") and the switch for the automatic checks.
//!
//! Installing is two steps on purpose. **Update now** fetches the file while the person keeps
//! working; **Restart to update** is the one that closes Lazy, because that is the one with a
//! cost.
//!
//! Where an install cannot be replaced in place (a portable copy, or one built from source) the
//! button is **View release**, which opens the release page.

use std::rc::Rc;

use gpui::{
    App, Context, Div, Entity, FocusHandle, Global, ParentElement, Role, Stateful, Styled, div,
    prelude::*, px,
};
use lazy_client_core::{CHECK_INTERVAL, UpdateFailure, UpdateStatus, Version};

use crate::form::{self, ButtonKind};
use crate::theme::Theme;

/// What the desktop does for the updates. Implemented by the desktop application and by a
/// recorder in tests. Must not block: results arrive through [`UpdateModel::set_status`].
pub trait UpdateHost {
    /// Whether this install can be replaced from inside Lazy. If not, the person is sent to the
    /// release page instead.
    fn can_install(&self) -> bool;
    /// Whether checks run on their own.
    fn automatic(&self) -> bool;
    fn set_automatic(&self, automatic: bool);
    /// Checks now and reports what it finds, even if that is nothing.
    fn check_now(&self);
    /// Fetches the file of the release found.
    fn download(&self);
    /// Starts the installer for the file that is ready. On `Ok` Lazy is about to be replaced and
    /// the caller quits; the installer starts the new one.
    fn install(&self) -> Result<(), UpdateFailure>;
}

/// What the main button does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    /// Look for a newer release.
    Check,
    /// Fetch the file.
    Download,
    /// Replace this Lazy with the file that was fetched, and start the new one.
    Install,
    /// Open this address: the release page.
    Page(String),
    /// Nothing: something is under way.
    Wait,
}

/// The main button for `status`.
pub fn offer(status: &UpdateStatus, can_install: bool) -> Offer {
    match status {
        UpdateStatus::Idle | UpdateStatus::UpToDate | UpdateStatus::Failed { update: None, .. } => {
            Offer::Check
        }
        UpdateStatus::Checking | UpdateStatus::Downloading { .. } => Offer::Wait,
        UpdateStatus::Available(update)
        | UpdateStatus::Failed {
            update: Some(update),
            ..
        } => {
            if can_install {
                Offer::Download
            } else {
                Offer::Page(update.release_url.clone())
            }
        }
        UpdateStatus::Ready { .. } => Offer::Install,
    }
}

/// The main button's words.
pub fn offer_label(status: &UpdateStatus, offer: &Offer) -> &'static str {
    let failed = matches!(status, UpdateStatus::Failed { .. });
    match offer {
        Offer::Check if failed => "Try again",
        Offer::Check => "Check now",
        Offer::Download if failed => "Try again",
        Offer::Download => "Update now",
        Offer::Install => "Restart to update",
        Offer::Page(_) => "View release",
        Offer::Wait if matches!(status, UpdateStatus::Checking) => "Checking…",
        Offer::Wait => "Downloading…",
    }
}

/// What the status says, in a sentence. `automatic` is whether checks run on their own.
pub fn headline(status: &UpdateStatus, automatic: bool) -> String {
    match status {
        UpdateStatus::Idle if automatic => format!(
            "Lazy looks for updates every {} minutes.",
            CHECK_INTERVAL.as_secs() / 60
        ),
        UpdateStatus::Idle => "Lazy isn't looking for updates.".to_owned(),
        UpdateStatus::Checking => "Checking for updates…".to_owned(),
        UpdateStatus::UpToDate => "You're up to date.".to_owned(),
        UpdateStatus::Available(update) => format!("Lazy {} is available.", update.version),
        UpdateStatus::Downloading { update, .. } => match status.fraction() {
            Some(fraction) => format!(
                "Downloading Lazy {}… {}%",
                update.version,
                (fraction * 100.0).round() as u32
            ),
            None => format!("Downloading Lazy {}…", update.version),
        },
        UpdateStatus::Ready { update, .. } => {
            format!("Lazy {} is ready. Lazy restarts to finish.", update.version)
        }
        UpdateStatus::Failed {
            update: Some(update),
            failure,
        } => format!(
            "Couldn't update to Lazy {}. {}",
            update.version,
            failure.describe()
        ),
        UpdateStatus::Failed {
            update: None,
            failure,
        } => format!("Couldn't check for updates. {}", failure.describe()),
    }
}

/// The updates, as the windows see them.
pub struct UpdateModel {
    host: Rc<dyn UpdateHost>,
    status: UpdateStatus,
    /// The release whose bar the person closed with "Later".
    dismissed: Option<Version>,
    /// The running version, for the dialog.
    current: String,
}

impl UpdateModel {
    pub fn new(host: Rc<dyn UpdateHost>, current: impl Into<String>) -> Self {
        Self {
            host,
            status: UpdateStatus::Idle,
            dismissed: None,
            current: current.into(),
        }
    }

    pub fn status(&self) -> &UpdateStatus {
        &self.status
    }

    /// The version that is running.
    pub fn current(&self) -> &str {
        &self.current
    }

    pub fn can_install(&self) -> bool {
        self.host.can_install()
    }

    pub fn automatic(&self) -> bool {
        self.host.automatic()
    }

    /// The updater reported a new status.
    pub fn set_status(&mut self, status: UpdateStatus, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.notify();
        }
    }

    /// Whether the bar is showing: there is something to say about a release the person has not
    /// put off.
    pub fn bar_visible(&self) -> bool {
        match self.status.update() {
            Some(update) => self.dismissed.as_ref() != Some(&update.version),
            None => false,
        }
    }

    /// The main button's action now.
    pub fn offer(&self) -> Offer {
        offer(&self.status, self.host.can_install())
    }

    /// Does what the main button says.
    pub fn act(&mut self, cx: &mut Context<Self>) {
        match self.offer() {
            Offer::Check => self.check_now(cx),
            Offer::Download => self.download(cx),
            Offer::Install => self.install(cx),
            Offer::Page(url) => cx.open_url(&url),
            Offer::Wait => {}
        }
    }

    /// Asks for a check, which is answered even when there is nothing new.
    pub fn check_now(&mut self, cx: &mut Context<Self>) {
        self.dismissed = None;
        self.host.check_now();
        cx.notify();
    }

    pub fn download(&mut self, cx: &mut Context<Self>) {
        self.dismissed = None;
        self.host.download();
        cx.notify();
    }

    /// Starts the installer and quits so that it can replace this Lazy. If the installer cannot
    /// be started, says so and stays.
    pub fn install(&mut self, cx: &mut Context<Self>) {
        match self.host.install() {
            Ok(()) => cx.quit(),
            Err(failure) => {
                let update = self.status.update().cloned();
                self.status = UpdateStatus::Failed { update, failure };
                self.dismissed = None;
                cx.notify();
            }
        }
    }

    /// "Later": the bar for this release stays away until the next start or the next check the
    /// person asks for.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        if let Some(update) = self.status.update() {
            self.dismissed = Some(update.version.clone());
            cx.notify();
        }
    }

    pub fn set_automatic(&mut self, automatic: bool, cx: &mut Context<Self>) {
        self.host.set_automatic(automatic);
        cx.notify();
    }

    /// Opens the release's notes, if there is a release.
    pub fn open_release_notes(&self, cx: &App) {
        if let Some(update) = self.status.update() {
            cx.open_url(&update.release_url);
        }
    }
}

struct Installed(Entity<UpdateModel>);

impl Global for Installed {}

/// Makes `host` the application's updater. Called once at startup, before the window opens;
/// returns the model, which the desktop feeds with the updater's reports.
pub fn install(
    cx: &mut App,
    host: Rc<dyn UpdateHost>,
    current: impl Into<String>,
) -> Entity<UpdateModel> {
    let model = cx.new(|_| UpdateModel::new(host, current));
    cx.set_global(Installed(model.clone()));
    model
}

/// The installed model, if this application has an updater.
pub fn model(cx: &App) -> Option<Entity<UpdateModel>> {
    cx.try_global::<Installed>()
        .map(|installed| installed.0.clone())
}

/// The bar's two buttons' keyboard stops.
pub struct BarFocus {
    primary: FocusHandle,
    later: FocusHandle,
}

impl BarFocus {
    pub fn new(cx: &mut App) -> Self {
        Self {
            primary: cx.focus_handle().tab_stop(true),
            later: cx.focus_handle().tab_stop(true),
        }
    }
}

/// The bar across the top of the window, when there is something to say.
pub fn bar(
    theme: &Theme,
    updates: &Entity<UpdateModel>,
    focus: &BarFocus,
    cx: &App,
) -> Option<Stateful<Div>> {
    let model = updates.read(cx);
    if !model.bar_visible() {
        return None;
    }
    let status = model.status().clone();
    let offer = model.offer();
    let label = offer_label(&status, &offer);
    let waiting = offer == Offer::Wait;
    let can_put_off = !matches!(status, UpdateStatus::Downloading { .. });
    let colors = theme.colors;
    let act_on = updates.clone();
    let later_on = updates.clone();
    Some(
        div()
            .id("update-bar")
            .debug_selector(|| "update-bar".into())
            .role(Role::Status)
            .flex_none()
            .w_full()
            .px_4()
            .py_2()
            .flex()
            .items_center()
            .gap_3()
            .bg(colors.accent_surface)
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .id("update-bar-text")
                    .debug_selector(|| "update-bar-text".into())
                    .flex_1()
                    .min_w_0()
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text)
                    .child(headline(&status, true)),
            )
            .child(form::button(
                theme,
                "update-bar-act",
                label,
                ButtonKind::Primary,
                !waiting,
                &focus.primary,
                move |_, cx| {
                    act_on.update(cx, |model, cx| model.act(cx));
                },
            ))
            .children(can_put_off.then(|| {
                form::button(
                    theme,
                    "update-bar-later",
                    "Later",
                    ButtonKind::Secondary,
                    true,
                    &focus.later,
                    move |_, cx| {
                        later_on.update(cx, |model, cx| model.dismiss(cx));
                    },
                )
            })),
    )
}

/// A thin bar showing how much of a file has come, from 0 to 1.
pub fn progress_bar(theme: &Theme, fraction: f32) -> Stateful<Div> {
    let colors = theme.colors;
    div()
        .id("updates-progress")
        .debug_selector(|| "updates-progress".into())
        .w_full()
        .h(px(6.))
        .rounded(theme.radii.pill)
        .bg(colors.border)
        .child(
            div()
                .h_full()
                .w(gpui::relative(fraction.clamp(0.0, 1.0)))
                .rounded(theme.radii.pill)
                .bg(colors.accent),
        )
}

#[cfg(test)]
mod tests {
    use lazy_client_core::{AvailableUpdate, Package, parse_checksums};

    use super::*;

    fn update(version: &str) -> AvailableUpdate {
        let package = Package::LinuxDeb;
        let name = package.file_name(&Version::parse(version).unwrap());
        AvailableUpdate::newest(
            &parse_checksums(&format!("{}  {name}\n", "ab".repeat(32))),
            package,
            &Version::new(0, 1, 0),
            "https://example.test/releases",
        )
        .expect("newer than 0.1.0")
    }

    #[test]
    fn the_main_button_follows_the_status() {
        let found = update("0.6.0");
        let failed = |update: Option<AvailableUpdate>| UpdateStatus::Failed {
            update,
            failure: UpdateFailure::Damaged,
        };
        let downloading = UpdateStatus::Downloading {
            update: found.clone(),
            received: 1,
            total: Some(2),
        };
        let ready = UpdateStatus::Ready {
            update: found.clone(),
            file: "f".into(),
        };
        let page = Offer::Page(found.release_url.clone());

        for (status, can_install, expected, label) in [
            (UpdateStatus::Idle, true, Offer::Check, "Check now"),
            (UpdateStatus::UpToDate, true, Offer::Check, "Check now"),
            (UpdateStatus::Checking, true, Offer::Wait, "Checking…"),
            (
                UpdateStatus::Available(found.clone()),
                true,
                Offer::Download,
                "Update now",
            ),
            (
                UpdateStatus::Available(found.clone()),
                false,
                page.clone(),
                "View release",
            ),
            (downloading, true, Offer::Wait, "Downloading…"),
            (ready, true, Offer::Install, "Restart to update"),
            (failed(None), true, Offer::Check, "Try again"),
            (
                failed(Some(found.clone())),
                true,
                Offer::Download,
                "Try again",
            ),
            (failed(Some(found)), false, page, "View release"),
        ] {
            let chosen = offer(&status, can_install);
            assert_eq!(chosen, expected, "{status:?}");
            assert_eq!(offer_label(&status, &chosen), label, "{status:?}");
        }
    }

    #[test]
    fn the_headline_says_what_is_happening_in_words_that_name_the_version() {
        let found = update("0.6.0");
        assert_eq!(
            headline(&UpdateStatus::Idle, true),
            "Lazy looks for updates every 30 minutes."
        );
        assert_eq!(
            headline(&UpdateStatus::Idle, false),
            "Lazy isn't looking for updates."
        );
        assert_eq!(
            headline(&UpdateStatus::UpToDate, true),
            "You're up to date."
        );
        assert_eq!(
            headline(&UpdateStatus::Available(found.clone()), true),
            "Lazy 0.6.0 is available."
        );
        let downloading = |received, total| UpdateStatus::Downloading {
            update: found.clone(),
            received,
            total,
        };
        assert_eq!(
            headline(&downloading(50, Some(200)), true),
            "Downloading Lazy 0.6.0… 25%"
        );
        assert_eq!(
            headline(&downloading(50, None), true),
            "Downloading Lazy 0.6.0…"
        );
        assert_eq!(
            headline(
                &UpdateStatus::Failed {
                    update: Some(found.clone()),
                    failure: UpdateFailure::Unreachable
                },
                true
            ),
            "Couldn't update to Lazy 0.6.0. Couldn't reach GitHub."
        );
        assert_eq!(
            headline(
                &UpdateStatus::Failed {
                    update: None,
                    failure: UpdateFailure::Unreachable
                },
                true
            ),
            "Couldn't check for updates. Couldn't reach GitHub."
        );
    }
}
