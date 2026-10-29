//! Help, Diagnostics…: a report with no secrets and no content, shown and copied.
//!
//! The report is what the core knows about itself ([`lazy_client_core::Diagnostics`]: protocol and
//! session, connection states, counts, the last error codes) plus what the desktop knows (version,
//! operating system, the server's address without credentials, the profile and log folders).
//! It never holds a message, a name, a password, a token, or an invitation code; the tests scan
//! it for the ones a scenario used.
//!
//! # Keyboard
//!
//! "Copy diagnostics" has the keyboard when the dialog opens (Enter or Space copies); it is the
//! only tab stop. Escape closes. The folder of the log files is shown as a path to open by hand:
//! the program starts nothing else.

use gpui::{
    ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    ParentElement, Render, Styled, Window, div, prelude::*, px,
};
use lazy_client_core::Diagnostics;

use crate::app_model::AppModel;
use crate::form::{self, ButtonKind};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::theme::Theme;

/// What the desktop knows that the core does not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopInfo {
    pub app_version: String,
    pub os: String,
    /// The server's address, already without credentials (see [`sanitize_server_url`]).
    pub server_url: String,
    pub profile_dir: String,
    pub log_dir: String,
}

impl gpui::Global for DesktopInfo {}

impl DesktopInfo {
    /// Builds the info, dropping credentials, the query, and the fragment from the server's URL.
    pub fn new(
        app_version: impl Into<String>,
        os: impl Into<String>,
        server_url: &str,
        profile_dir: impl Into<String>,
        log_dir: impl Into<String>,
    ) -> Self {
        Self {
            app_version: app_version.into(),
            os: os.into(),
            server_url: sanitize_server_url(server_url),
            profile_dir: profile_dir.into(),
            log_dir: log_dir.into(),
        }
    }
}

/// Gives the diagnostics what the desktop knows. Called once at startup.
pub fn set_desktop_info(cx: &mut gpui::App, info: DesktopInfo) {
    cx.set_global(info);
}

/// The server's address as scheme, host, port, and path only: no `user:password@`, no query, no
/// fragment, whatever the configuration held.
pub fn sanitize_server_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, url),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, ""),
    };
    // Everything up to the last '@' of the authority is credentials.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    match scheme {
        Some(scheme) => format!("{scheme}://{host}{path}"),
        None => format!("{host}{path}"),
    }
}

/// The text copied to the clipboard.
pub fn report_text(info: &DesktopInfo, diagnostics: &Diagnostics) -> String {
    let unknown = |text: &str| {
        if text.is_empty() {
            "unknown".to_owned()
        } else {
            text.to_owned()
        }
    };
    let mut text = String::from("Lazy diagnostics (no messages, names or passwords)\n");
    text.push_str(&format!("application: {}\n", unknown(&info.app_version)));
    text.push_str(&format!("operating system: {}\n", unknown(&info.os)));
    text.push_str(&format!("server: {}\n", unknown(&info.server_url)));
    text.push_str(&format!("profile folder: {}\n", unknown(&info.profile_dir)));
    text.push_str(&format!("log folder: {}\n", unknown(&info.log_dir)));
    text.push_str(&diagnostics.report());
    text
}

pub struct DiagnosticsDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    copy_focus: FocusHandle,
    copied: bool,
}

impl EventEmitter<DialogEvent> for DiagnosticsDialog {}

impl DiagnosticsDialog {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let copy_focus = cx.focus_handle().tab_stop(true);
        let mut trap = ModalTrap::new(cx);
        trap.open(&copy_focus, window, cx);
        Self {
            model,
            trap,
            copy_focus,
            copied: false,
        }
    }

    /// The report as it reads now.
    pub fn report(&self, cx: &gpui::App) -> String {
        let info = cx.try_global::<DesktopInfo>().cloned().unwrap_or_default();
        report_text(&info, &self.model.read(cx).core().diagnostics())
    }

    /// Whether the report was copied (for tests).
    pub fn copied(&self) -> bool {
        self.copied
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let report = self.report(cx);
        cx.write_to_clipboard(ClipboardItem::new_string(report));
        self.copied = true;
        cx.notify();
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }
}

impl Focusable for DiagnosticsDialog {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.copy_focus.clone()
    }
}

impl Render for DiagnosticsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let copy_to = this.clone();
        let close_to = this.clone();
        let report = self.report(cx);
        let log_dir = cx
            .try_global::<DesktopInfo>()
            .map(|info| info.log_dir.clone())
            .filter(|dir| !dir.is_empty());
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    // The symbol stands for the app itself here, where a generic glyph would.
                    .children(crate::brand::symbol(cx, 40.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(theme.type_scale.body)
                            .text_color(colors.text_muted)
                            .child(
                                "This has no messages, names, or passwords. Copy it into a bug report.",
                            ),
                    ),
            )
            .child(
                div()
                    .id("diagnostics-report")
                    .debug_selector(|| "diagnostics-report".into())
                    .max_h(px(
                        (f32::from(window.viewport_size().height) - 340.0).clamp(120.0, 520.0)
                    ))
                    .overflow_y_scroll()
                    .p_3()
                    .rounded(theme.radii.medium)
                    .bg(colors.surface)
                    .border_1()
                    .border_color(colors.border_strong)
                    .flex()
                    .flex_col()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text)
                    .children(report.lines().map(|line| div().child(line.to_owned()))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(form::button(
                        &theme,
                        "diagnostics-copy",
                        if self.copied {
                            "Copied"
                        } else {
                            "Copy diagnostics"
                        },
                        ButtonKind::Primary,
                        true,
                        &self.copy_focus,
                        move |_, cx| {
                            copy_to.update(cx, |dialog, cx| dialog.copy(cx)).ok();
                        },
                    ))
                    .children(log_dir.map(|dir| {
                        div()
                            .id("diagnostics-logs")
                            .debug_selector(|| "diagnostics-logs".into())
                            .flex_1()
                            .min_w_0()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Log files are in"),
                            )
                            .child(div().child(dir))
                    })),
            )
            .into_any_element();
        dialog_overlay(
            &theme,
            "Diagnostics",
            620.,
            &self.trap,
            body,
            "Close",
            move |window, cx| {
                close_to
                    .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                    .ok();
            },
        )
        .on_action(cx.listener(|this, _: &DialogDismiss, window, cx| this.dismiss(window, cx)))
        .on_action(cx.listener(|this, _: &DialogNext, window, cx| this.trap.tab(false, window, cx)))
        .on_action(cx.listener(|this, _: &DialogPrev, window, cx| this.trap.tab(true, window, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_query_and_fragment_are_dropped_from_the_server_address() {
        assert_eq!(
            sanitize_server_url("https://alice:hunter2@chat.example.com:8443/api?token=abc#x"),
            "https://chat.example.com:8443/api"
        );
        assert_eq!(
            sanitize_server_url("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            sanitize_server_url("http://tok3n@localhost/"),
            "http://localhost/"
        );
        assert_eq!(
            sanitize_server_url("user:pw@host.example"),
            "host.example",
            "no scheme"
        );
        assert_eq!(sanitize_server_url(""), "");
    }
}
