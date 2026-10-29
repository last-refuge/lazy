//! Preferences' Profile and Appearance sections (D23).
//!
//! **Profile**: the avatar (a preview, "Choose picture…" through the system's file dialog, and
//! "Remove picture") and the display name (saved with its own button, since a half-typed name
//! should not go out), each with progress and the server's refusal in place.
//!
//! **Appearance**: the theme (follow the system, light, dark) and reduced motion (follow the
//! system, on, off). Both apply at once and are kept on this device. GPUI does not tell us the
//! system's own reduced-motion preference, so "follow the system" cannot read it and keeps motion
//! on; the dialog says so.

use std::path::PathBuf;

use gpui::{
    AnyElement, Context, FontWeight, IntoElement, ParentElement, PathPromptOptions, Role, Styled,
    Window, div, prelude::*,
};
use lazy_client_core::{
    ActionKind, ActionState, Appearance, FileSource, Input, MotionChoice, ThemeChoice,
};
use lazy_domain::DisplayName;

use super::PreferencesDialog;
use crate::form::{self, ButtonKind};
use crate::states;
use crate::theme::Theme;

impl PreferencesDialog {
    /// The name as it would be saved, `Ok(None)` when it is what it already is.
    pub(super) fn name_change(
        &self,
        cx: &gpui::App,
    ) -> Result<Option<String>, lazy_domain::NameError> {
        let parsed = DisplayName::parse(self.display_name.read(cx).text())?;
        let current = self
            .model
            .read(cx)
            .core()
            .me()
            .map(|me| me.display_name.to_string());
        Ok((current.as_deref() != Some(parsed.as_str())).then(|| parsed.as_str().to_owned()))
    }

    /// What the display name field holds. For tests.
    pub fn display_name_text(&self, cx: &gpui::App) -> String {
        self.display_name.read(cx).text().to_owned()
    }

    pub(super) fn save_name(&mut self, cx: &mut Context<Self>) {
        let state = self
            .model
            .read(cx)
            .core()
            .action(ActionKind::UpdateProfile)
            .clone();
        if state == ActionState::InFlight {
            return;
        }
        match self.name_change(cx) {
            Ok(Some(name)) => {
                self.name_problem = None;
                self.model.update(cx, |model, cx| {
                    model.dispatch(
                        Input::UpdateProfile {
                            display_name: Some(name),
                        },
                        cx,
                    )
                });
            }
            Ok(None) => {}
            Err(error) => {
                self.name_problem = Some(states::display_name_problem(&error));
                cx.notify();
            }
        }
    }

    /// Opens the system's file dialog for the avatar.
    pub(super) fn choose_picture(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |dialog, cx| match result {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => {
                    dialog.picture_chosen(paths.remove(0), cx)
                }
                Ok(Err(error)) => {
                    dialog.picture_error =
                        Some(crate::attachments::picker_failed_text(&error.to_string()));
                    cx.notify();
                }
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    /// The picture the person chose in the file dialog. (Public so tests and the offscreen
    /// renders, which have no file dialog to answer, can stand in for it.)
    pub fn picture_chosen(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.picture_error = None;
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::SetAvatar {
                    source: FileSource::Path(path),
                },
                cx,
            )
        });
        cx.notify();
    }

    pub(super) fn remove_picture(&mut self, cx: &mut Context<Self>) {
        self.picture_error = None;
        self.model
            .update(cx, |model, cx| model.dispatch(Input::RemoveAvatar, cx));
        cx.notify();
    }

    fn set_appearance(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::SetAppearance(appearance), cx)
        });
    }

    pub(super) fn profile_section(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let (me, name_state, avatar_state) = {
            let core = self.model.read(cx).core();
            (
                core.me().cloned(),
                core.action(ActionKind::UpdateProfile).clone(),
                core.action(ActionKind::SetAvatar).clone(),
            )
        };
        let seed = me.as_ref().map_or(0, |me| me.id.as_uuid().as_u128());
        let shown_name = me
            .as_ref()
            .map(|me| me.display_name.to_string())
            .unwrap_or_default();
        let has_avatar = me.as_ref().is_some_and(|me| me.avatar_version > 0);
        if let Some(me) = &me {
            crate::avatar_images::request_later(&self.model, vec![me.id], cx);
        }
        let picture = me.as_ref().and_then(|me| {
            crate::avatar_images::image_of(
                self.model.read(cx).core(),
                crate::image_store::ImageStore::global(cx).read(cx),
                me.id,
            )
        });
        let name_busy = name_state == ActionState::InFlight;
        let avatar_busy = avatar_state == ActionState::InFlight;
        let (change, problem) = match self.name_change(cx) {
            Ok(change) => (change.is_some(), None),
            Err(error) => (false, Some(states::display_name_problem(&error))),
        };
        let problem = self.name_problem.clone().or(problem);
        let choose_to = this.clone();
        let remove_to = this.clone();
        let save_to = this.clone();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        crate::avatar::Avatar::new(seed, shown_name, 64.)
                            .text_size(theme.type_scale.heading)
                            .image(picture)
                            .into_div()
                            .debug_selector(|| "prefs-avatar".into()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(form::button(
                                        theme,
                                        "prefs-choose-picture",
                                        if avatar_busy {
                                            "Uploading…"
                                        } else {
                                            "Choose picture…"
                                        },
                                        ButtonKind::Secondary,
                                        !avatar_busy,
                                        &self.picture_focus,
                                        move |_, cx| {
                                            choose_to
                                                .update(cx, |dialog, cx| dialog.choose_picture(cx))
                                                .ok();
                                        },
                                    ))
                                    .child(form::button(
                                        theme,
                                        "prefs-remove-picture",
                                        "Remove picture",
                                        ButtonKind::Secondary,
                                        has_avatar && !avatar_busy,
                                        &self.remove_picture_focus,
                                        move |_, cx| {
                                            remove_to
                                                .update(cx, |dialog, cx| dialog.remove_picture(cx))
                                                .ok();
                                        },
                                    )),
                            )
                            .child(
                                div()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_faint)
                                    .child(states::AVATAR_FORMATS),
                            ),
                    ),
            )
            .children(
                self.picture_error
                    .clone()
                    .or_else(|| match &avatar_state {
                        ActionState::Failed(message) => Some(message.clone()),
                        _ => None,
                    })
                    .map(|message| {
                        div()
                            .id("prefs-picture-failed")
                            .debug_selector(|| "prefs-picture-failed".into())
                            .role(Role::Alert)
                            .text_size(theme.type_scale.small)
                            .text_color(colors.danger)
                            .child(message)
                    }),
            )
            .child(
                div()
                    .debug_selector(|| "prefs-display-name".into())
                    .child(form::labeled_field(
                        theme,
                        "Display name",
                        &self.display_name,
                        None,
                        problem.map(Into::into),
                        window,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(form::button(
                        theme,
                        "prefs-save-name",
                        if name_busy { "Saving…" } else { "Save name" },
                        ButtonKind::Primary,
                        change && !name_busy,
                        &self.name_save_focus,
                        move |_, cx| {
                            save_to.update(cx, |dialog, cx| dialog.save_name(cx)).ok();
                        },
                    ))
                    .children(match &name_state {
                        ActionState::Failed(message) => Some(
                            div()
                                .id("prefs-name-failed")
                                .debug_selector(|| "prefs-name-failed".into())
                                .role(Role::Alert)
                                .flex_1()
                                .min_w_0()
                                .text_size(theme.type_scale.small)
                                .text_color(colors.danger)
                                .child(message.clone()),
                        ),
                        _ => None,
                    }),
            )
            .into_any_element()
    }

    pub(super) fn appearance_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let appearance = self.model.read(cx).core().appearance();
        let theme_to = this.clone();
        let motion_to = this.clone();
        let heading = |text: &'static str| {
            div()
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text_muted)
                .child(text)
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(heading("Theme"))
                    .child(form::toggle_group(
                        theme,
                        "prefs-theme",
                        "Colour theme",
                        &[
                            ("System", "prefs-theme-system"),
                            ("Light", "prefs-theme-light"),
                            ("Dark", "prefs-theme-dark"),
                        ],
                        match appearance.theme {
                            ThemeChoice::System => 0,
                            ThemeChoice::Light => 1,
                            ThemeChoice::Dark => 2,
                        },
                        &self.theme_focus,
                        move |index, _, cx| {
                            theme_to
                                .update(cx, |dialog, cx| {
                                    let mut next = dialog.model.read(cx).core().appearance();
                                    next.theme = [
                                        ThemeChoice::System,
                                        ThemeChoice::Light,
                                        ThemeChoice::Dark,
                                    ][index];
                                    dialog.set_appearance(next, cx);
                                })
                                .ok();
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(heading("Reduced motion"))
                            .child(form::toggle_group(
                                theme,
                                "prefs-motion",
                                "Reduced motion",
                                &[
                                    ("System", "prefs-motion-system"),
                                    ("On", "prefs-motion-on"),
                                    ("Off", "prefs-motion-off"),
                                ],
                                match appearance.motion {
                                    MotionChoice::System => 0,
                                    MotionChoice::Reduce => 1,
                                    MotionChoice::Full => 2,
                                },
                                &self.motion_focus,
                                move |index, _, cx| {
                                    motion_to
                                        .update(cx, |dialog, cx| {
                                            let mut next =
                                                dialog.model.read(cx).core().appearance();
                                            next.motion = [
                                                MotionChoice::System,
                                                MotionChoice::Reduce,
                                                MotionChoice::Full,
                                            ][index];
                                            dialog.set_appearance(next, cx);
                                        })
                                        .ok();
                                },
                            )),
                    )
                    .child(
                        div()
                            .debug_selector(|| "prefs-motion-note".into())
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_faint)
                            .child(states::MOTION_NOTE),
                    ),
            )
            .into_any_element()
    }
}
