//! Help, Check for updates…: whether a newer Lazy exists, and the way to get it.
//!
//! It shows the same as the bar across the top of the window ([`crate::updates::bar`]) and adds
//! the answer to a check ("You're up to date"), the notes of the release found, and the switch for
//! the automatic checks. Opening it checks, so the answer is the one the person came for. In an
//! application with no updater it says so and offers Close.
//!
//! # Keyboard
//!
//! The main button has the keyboard when the dialog opens (Enter or Space presses it). Tab moves
//! on to "Release notes", when there is a release, and to the checkbox for the automatic
//! checks; Escape closes.

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    ParentElement, Render, Styled, Subscription, Window, div, prelude::*,
};

use lazy_client_core::CHECK_INTERVAL;

use crate::form::{self, ButtonKind};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::theme::Theme;
use crate::updates::{self, Offer, UpdateModel};

pub struct UpdatesDialog {
    updates: Option<Entity<UpdateModel>>,
    trap: ModalTrap,
    /// The main button: check, update, restart, or (with no updater) close.
    primary_focus: FocusHandle,
    notes_focus: FocusHandle,
    automatic_focus: FocusHandle,
    _observing: Option<Subscription>,
}

impl EventEmitter<DialogEvent> for UpdatesDialog {}

impl UpdatesDialog {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let updates = updates::model(cx);
        let primary_focus = cx.focus_handle().tab_stop(true);
        let notes_focus = cx.focus_handle().tab_stop(true);
        let automatic_focus = cx.focus_handle().tab_stop(true);
        let mut trap = ModalTrap::new(cx);
        trap.open(&primary_focus, window, cx);
        let observing = updates
            .as_ref()
            .map(|updates| cx.observe(updates, |_, _, cx| cx.notify()));
        if let Some(updates) = &updates {
            updates.update(cx, |model, cx| model.check_now(cx));
        }
        Self {
            updates,
            trap,
            primary_focus,
            notes_focus,
            automatic_focus,
            _observing: observing,
        }
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }

    /// The updater's model, if this application has one. For tests.
    pub fn model(&self) -> Option<&Entity<UpdateModel>> {
        self.updates.as_ref()
    }

    fn without_updater(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let close_to = cx.entity().downgrade();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .children(crate::brand::symbol(cx, 40.))
                    .child(
                        div()
                            .id("updates-unavailable")
                            .debug_selector(|| "updates-unavailable".into())
                            .flex_1()
                            .min_w_0()
                            .text_size(theme.type_scale.body)
                            .text_color(colors.text_muted)
                            .child("This copy of Lazy doesn't look for updates."),
                    ),
            )
            .child(form::button(
                theme,
                "updates-primary",
                "Close",
                ButtonKind::Primary,
                true,
                &self.primary_focus,
                move |window, cx| {
                    close_to
                        .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                        .ok();
                },
            ))
            .into_any_element()
    }

    fn with_updater(
        &self,
        theme: &Theme,
        updates: &Entity<UpdateModel>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let (status, offer, automatic, current) = {
            let model = updates.read(cx);
            (
                model.status().clone(),
                model.offer(),
                model.automatic(),
                model.current().to_owned(),
            )
        };
        let label = updates::offer_label(&status, &offer);
        let waiting = offer == Offer::Wait;
        let fraction = status.fraction();
        let has_release = status.update().is_some();
        let act_on = updates.clone();
        let notes_on = updates.clone();
        let automatic_on = updates.clone();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .children(crate::brand::symbol(cx, 40.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .id("updates-headline")
                                    .debug_selector(|| "updates-headline".into())
                                    .text_size(theme.type_scale.body)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child(updates::headline(&status, automatic)),
                            )
                            .child(
                                div()
                                    .id("updates-current")
                                    .debug_selector(|| "updates-current".into())
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_muted)
                                    .child(format!("You have Lazy {current}.")),
                            ),
                    ),
            )
            .children(fraction.map(|fraction| updates::progress_bar(theme, fraction)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(form::button(
                        theme,
                        "updates-primary",
                        label,
                        ButtonKind::Primary,
                        !waiting,
                        &self.primary_focus,
                        move |_, cx| {
                            act_on.update(cx, |model, cx| model.act(cx));
                        },
                    ))
                    .children(has_release.then(|| {
                        form::button(
                            theme,
                            "updates-notes",
                            "Release notes",
                            ButtonKind::Secondary,
                            true,
                            &self.notes_focus,
                            move |_, cx| {
                                notes_on.update(cx, |model, cx| model.open_release_notes(cx));
                            },
                        )
                    })),
            )
            .child(form::checkbox(
                theme,
                "updates-automatic",
                format!(
                    "Look for updates every {} minutes",
                    CHECK_INTERVAL.as_secs() / 60
                ),
                automatic,
                &self.automatic_focus,
                move |checked, _, cx| {
                    automatic_on.update(cx, |model, cx| model.set_automatic(checked, cx));
                },
            ))
            .into_any_element()
    }
}

impl Focusable for UpdatesDialog {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.primary_focus.clone()
    }
}

impl Render for UpdatesDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let close_to = cx.entity().downgrade();
        let body = match self.updates.clone() {
            Some(updates) => self.with_updater(&theme, &updates, cx),
            None => self.without_updater(&theme, cx),
        };
        dialog_overlay(
            &theme,
            "Updates",
            480.,
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
