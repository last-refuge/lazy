//! The custom emoji dialog, for admins and owners: the workspace's emoji (picture, `:name:`, who
//! added it), with Rename and Delete for each, and a form to add one.
//!
//! # Keyboard
//!
//! The list is one tab stop (it can hold 500 emoji): Up and Down move the cursor, Home and End
//! jump, Enter renames the emoji under the cursor and Delete removes it (after asking). Then Tab
//! reaches Add custom emoji and Close. The Add form's stops are, in order: Choose image…, the
//! name, Cancel, Add. Renaming has the name, Cancel, Save; deleting starts on Cancel. Escape steps
//! back one view and, from the list, closes the dialog; while an input method is composing in a
//! name field it belongs to the composition.
//!
//! Nothing here reads a file: the file dialog is the system's, and the chosen picture is decoded
//! off the main thread like every other picture.

use std::path::PathBuf;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, ParentElement, PathPromptOptions, Render, Role, Styled, Subscription,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_client_core::{ActionKind, ActionState, FileSource, Input, LocalFile};
use lazy_domain::EmojiId;
use lazy_protocol::CustomEmoji;

use super::{any_composing, single_line_field};
use crate::app_model::{AppEvent, AppModel};
use crate::emoji_images;
use crate::form::{self, ButtonKind};
use crate::image_store::{EMOJI_EDGE, ImageKey, ImageStore, Slot, Source};
use crate::modal::{DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay};
use crate::states::{self, EmojiNameCheck};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;
use crate::time_fmt::{ago_label, to_zoned};

const ROW_HEIGHT: f32 = 52.0;
/// Just over four rows, so a fifth is always seen cut off when there are more to scroll to.
const LIST_HEIGHT: f32 = 242.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    List,
    Add,
    Rename(EmojiId),
    ConfirmDelete(EmojiId),
}

pub struct CustomEmojiDialog {
    model: Entity<AppModel>,
    trap: ModalTrap,
    view: View,
    /// The row the keyboard is on.
    cursor: usize,
    scroll: UniformListScrollHandle,
    list_focus: FocusHandle,
    add_focus: FocusHandle,
    close_focus: FocusHandle,
    choose_focus: FocusHandle,
    submit_focus: FocusHandle,
    cancel_focus: FocusHandle,
    confirm_cancel_focus: FocusHandle,
    confirm_delete_focus: FocusHandle,
    /// The name field of the Add and Rename forms.
    name: Entity<TextInput>,
    /// The file chosen for a new emoji, and its name.
    chosen: Option<PathBuf>,
    /// Why the file dialog could not open, until the next try.
    picker_error: Option<String>,
    /// The action whose completion ends the view, and whether it has been seen starting.
    waiting: Option<(ActionKind, bool)>,
    zone: TimeZone,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for CustomEmojiDialog {}

/// The key the chosen file's preview is kept under in the image store.
fn preview_key() -> ImageKey {
    ImageKey::Chip("custom-emoji-add".to_owned())
}

impl CustomEmojiDialog {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = single_line_field("e.g. shipit", "Emoji name", 34, cx);
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &name,
            window,
            |this, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.submit(cx),
                TextInputEvent::Tab { backwards } => this.trap.tab(*backwards, window, cx),
                TextInputEvent::Changed => cx.notify(),
                TextInputEvent::UpWhenEmpty
                | TextInputEvent::PasteAttachments
                | TextInputEvent::Suggestion(_) => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, _event: &AppEvent, window, cx| {
                this.follow_core(window, cx);
                cx.notify();
            },
        ));
        subscriptions.push(cx.observe(&ImageStore::global(cx), |_, _, cx| cx.notify()));
        let mut trap = ModalTrap::new(cx);
        let list_focus = cx.focus_handle().tab_stop(true);
        let add_focus = cx.focus_handle().tab_stop(true);
        // With nothing listed, the way forward is Add.
        let empty = model.read(cx).core().live_custom_emoji().next().is_none();
        trap.open(if empty { &add_focus } else { &list_focus }, window, cx);
        Self {
            model,
            trap,
            view: View::List,
            cursor: 0,
            scroll: UniformListScrollHandle::new(),
            list_focus,
            add_focus,
            close_focus: cx.focus_handle().tab_stop(true),
            choose_focus: cx.focus_handle().tab_stop(true),
            submit_focus: cx.focus_handle().tab_stop(true),
            cancel_focus: cx.focus_handle().tab_stop(true),
            confirm_cancel_focus: cx.focus_handle().tab_stop(true),
            confirm_delete_focus: cx.focus_handle().tab_stop(true),
            name,
            chosen: None,
            picker_error: None,
            waiting: None,
            zone: TimeZone::system(),
            _subscriptions: subscriptions,
        }
    }

    /// Which view is showing, for tests.
    pub fn is_listing(&self) -> bool {
        self.view == View::List
    }

    /// The name box, for tests.
    pub fn name_input(&self) -> &Entity<TextInput> {
        &self.name
    }

    fn live(&self, cx: &gpui::App) -> Vec<CustomEmoji> {
        self.model
            .read(cx)
            .core()
            .live_custom_emoji()
            .cloned()
            .collect()
    }

    fn action(&self, kind: ActionKind, cx: &gpui::App) -> ActionState {
        self.model.read(cx).core().action(kind).clone()
    }

    fn dispatch(&self, input: Input, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    fn dismiss_action(&self, kind: ActionKind, cx: &mut Context<Self>) {
        self.dispatch(Input::DismissAction(kind), cx);
    }

    // ----- Following the core -----

    fn follow_core(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // An emoji that is gone (deleted elsewhere) takes its view with it.
        let gone = match self.view {
            View::Rename(id) | View::ConfirmDelete(id) => self
                .model
                .read(cx)
                .core()
                .custom_emoji_by_id(id)
                .is_none_or(|emoji| emoji.deleted),
            _ => false,
        };
        if gone && self.waiting.is_none() {
            self.back_to_list(window, cx);
        }
        let count = self.live(cx).len();
        self.cursor = self.cursor.min(count.saturating_sub(1));
        self.sync_name_enabled(cx);
        let Some((kind, started)) = self.waiting else {
            return;
        };
        match self.action(kind, cx) {
            ActionState::InFlight => self.waiting = Some((kind, true)),
            ActionState::Idle if started => {
                self.waiting = None;
                self.back_to_list(window, cx);
            }
            ActionState::Failed(_) => self.waiting = None,
            ActionState::Idle => {}
        }
    }

    /// The name box is read-only while its form is being sent.
    fn sync_name_enabled(&mut self, cx: &mut Context<Self>) {
        let kind = match self.view {
            View::Add => Some(ActionKind::AddCustomEmoji),
            View::Rename(id) => Some(ActionKind::RenameCustomEmoji(id)),
            _ => None,
        };
        let working = kind.is_some_and(|kind| self.action(kind, cx) == ActionState::InFlight);
        self.name
            .update(cx, |input, cx| input.set_enabled(!working, cx));
    }

    // ----- Moving between views -----

    fn back_to_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == View::Add {
            ImageStore::global(cx).update(cx, |store, _| store.forget(&preview_key()));
            self.chosen = None;
        }
        self.view = View::List;
        self.picker_error = None;
        // Leaving a view abandons what it was waiting for (an upload goes on; the new emoji
        // arrives in the list when it lands).
        self.waiting = None;
        self.sync_name_enabled(cx);
        let empty = self.live(cx).is_empty();
        window.focus(
            if empty {
                &self.add_focus
            } else {
                &self.list_focus
            },
            cx,
        );
        cx.notify();
    }

    fn start_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_action(ActionKind::AddCustomEmoji, cx);
        self.name.update(cx, |input, cx| input.set_text("", cx));
        self.chosen = None;
        self.picker_error = None;
        self.view = View::Add;
        window.focus(&self.choose_focus, cx);
        cx.notify();
    }

    fn start_rename(&mut self, id: EmojiId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self
            .model
            .read(cx)
            .core()
            .custom_emoji_label(id)
            .map(str::to_owned)
        else {
            return;
        };
        self.dismiss_action(ActionKind::RenameCustomEmoji(id), cx);
        self.name.update(cx, |input, cx| input.set_text(&name, cx));
        self.view = View::Rename(id);
        let handle = self.name.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn start_delete(&mut self, id: EmojiId, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_action(ActionKind::DeleteCustomEmoji(id), cx);
        self.view = View::ConfirmDelete(id);
        window.focus(&self.confirm_cancel_focus, cx);
        cx.notify();
    }

    /// Escape: back one view, or close from the list.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if any_composing(&[&self.name], cx) {
            return;
        }
        match self.view {
            View::List => self.finish(window, cx),
            View::ConfirmDelete(id) => {
                self.dismiss_action(ActionKind::DeleteCustomEmoji(id), cx);
                self.back_to_list(window, cx);
            }
            View::Rename(id) => {
                self.dismiss_action(ActionKind::RenameCustomEmoji(id), cx);
                self.back_to_list(window, cx);
            }
            View::Add => {
                self.dismiss_action(ActionKind::AddCustomEmoji, cx);
                self.back_to_list(window, cx);
            }
        }
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        ImageStore::global(cx).update(cx, |store, _| store.forget(&preview_key()));
        self.dismiss_action(ActionKind::AddCustomEmoji, cx);
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }

    // ----- Acting -----

    fn choose_file(&mut self, cx: &mut Context<Self>) {
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
                    dialog.file_chosen(paths.remove(0), cx)
                }
                Ok(Err(error)) => {
                    dialog.picker_error =
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
    pub fn file_chosen(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.picker_error = None;
        self.dismiss_action(ActionKind::AddCustomEmoji, cx);
        let store = ImageStore::global(cx);
        let source = Source::File(path.clone());
        store.update(cx, |store, cx| {
            store.forget(&preview_key());
            store.request(preview_key(), source, EMOJI_EDGE, cx);
        });
        // An empty name box is filled in from the file's name.
        if self.name.read(cx).text().trim().is_empty()
            && let Some(file) = path.file_name().and_then(|name| name.to_str())
        {
            let suggestion = states::emoji_name_from_file(file);
            self.name
                .update(cx, |input, cx| input.set_text(&suggestion, cx));
        }
        self.chosen = Some(path);
        cx.notify();
    }

    /// The names other live emoji have, for the "already used" check.
    fn taken_names(&self, except: Option<EmojiId>, cx: &gpui::App) -> Vec<String> {
        self.model
            .read(cx)
            .core()
            .live_custom_emoji()
            .filter(|emoji| Some(emoji.id) != except)
            .map(|emoji| emoji.name.clone())
            .collect()
    }

    fn name_check(&self, cx: &gpui::App) -> EmojiNameCheck {
        let except = match self.view {
            View::Rename(id) => Some(id),
            _ => None,
        };
        let taken = self.taken_names(except, cx);
        let taken: Vec<&str> = taken.iter().map(String::as_str).collect();
        states::check_emoji_name(self.name.read(cx).text(), &taken)
    }

    /// Enter in the name box, and the Add or Save button.
    fn submit(&mut self, cx: &mut Context<Self>) {
        let EmojiNameCheck::Good(name) = self.name_check(cx) else {
            return;
        };
        match self.view {
            View::Add => {
                let Some(path) = self.chosen.clone() else {
                    return;
                };
                if self.action(ActionKind::AddCustomEmoji, cx) == ActionState::InFlight {
                    return;
                }
                let file_name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "emoji".to_owned());
                self.waiting = Some((ActionKind::AddCustomEmoji, false));
                self.dispatch(
                    Input::AddCustomEmoji {
                        name,
                        file: LocalFile {
                            source: FileSource::Path(path),
                            name: file_name,
                        },
                    },
                    cx,
                );
            }
            View::Rename(id) => {
                let kind = ActionKind::RenameCustomEmoji(id);
                if self.action(kind, cx) == ActionState::InFlight {
                    return;
                }
                self.waiting = Some((kind, false));
                self.dispatch(Input::RenameCustomEmoji { id, name }, cx);
            }
            _ => {}
        }
    }

    fn delete(&mut self, id: EmojiId, cx: &mut Context<Self>) {
        let kind = ActionKind::DeleteCustomEmoji(id);
        if self.action(kind, cx) == ActionState::InFlight {
            return;
        }
        self.waiting = Some((kind, false));
        self.dispatch(Input::DeleteCustomEmoji(id), cx);
    }

    fn on_list_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.list_focus.is_focused(window) || event.keystroke.modifiers.modified() {
            return;
        }
        let rows = self.live(cx);
        if rows.is_empty() {
            return;
        }
        let last = rows.len() - 1;
        match event.keystroke.key.as_str() {
            "up" => self.cursor = self.cursor.saturating_sub(1),
            "down" => self.cursor = (self.cursor + 1).min(last),
            "home" => self.cursor = 0,
            "end" => self.cursor = last,
            "enter" if !event.is_held => {
                self.start_rename(rows[self.cursor.min(last)].id, window, cx)
            }
            "delete" | "backspace" => self.start_delete(rows[self.cursor.min(last)].id, window, cx),
            _ => return,
        }
        self.scroll
            .scroll_to_item(self.cursor.min(last), gpui::ScrollStrategy::Nearest);
        cx.stop_propagation();
        cx.notify();
    }

    // ----- Drawing -----

    fn picture(&self, id: EmojiId, size: f32, theme: &Theme, cx: &gpui::App) -> AnyElement {
        let store = ImageStore::global(cx);
        let picture = emoji_images::picture(store.read(cx), id);
        let failed = matches!(
            self.model.read(cx).core().emoji_image(id),
            Some(lazy_client_core::ImageState::Failed)
        );
        picture_box(picture.map(|p| p.image), failed, size, theme)
    }

    fn list_view(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let add_to = this.clone();
        let count = self.live(cx).len();
        let list_focused = self.list_focus.is_focused(window);
        let list: AnyElement = if count == 0 {
            div()
                .debug_selector(|| "emoji-admin-empty".into())
                .px_3()
                .py_3()
                .text_size(theme.type_scale.body)
                .text_color(colors.text_muted)
                .child(states::NO_CUSTOM_EMOJI)
                .into_any_element()
        } else {
            uniform_list(
                "emoji-admin-list",
                count,
                cx.processor(
                    move |this,
                          range: std::ops::Range<usize>,
                          window: &mut Window,
                          cx: &mut Context<Self>| {
                        let theme = *Theme::get(cx);
                        this.rows(range, &theme, window, cx)
                    },
                ),
            )
            .track_scroll(&self.scroll)
            .h(px(LIST_HEIGHT.min(count as f32 * ROW_HEIGHT)))
            .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child("Everyone in the workspace can use these in messages and reactions."),
            )
            .child(
                div()
                    .id("emoji-admin-list-area")
                    .debug_selector(|| "emoji-admin-list".into())
                    .role(Role::List)
                    .aria_label("Custom emoji")
                    // With nothing listed there is nothing to stop on; the way on is Add.
                    .when(count > 0, |area| area.track_focus(&self.list_focus))
                    .on_key_down(cx.listener(Self::on_list_key))
                    .rounded(theme.radii.medium)
                    .border_1()
                    .border_color(if list_focused {
                        colors.focus_ring
                    } else {
                        colors.border
                    })
                    .child(list),
            )
            .children((count > 0).then(|| {
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_faint)
                    .child("Up and Down choose · Enter renames · Delete removes")
            }))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "emoji-admin-add",
                        "Add custom emoji",
                        ButtonKind::Primary,
                        true,
                        &self.add_focus,
                        move |window, cx| {
                            add_to
                                .update(cx, |dialog, cx| dialog.start_add(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "emoji-admin-close",
                        "Close",
                        ButtonKind::Secondary,
                        true,
                        &self.close_focus,
                        {
                            let close_to = this;
                            move |window, cx| {
                                close_to
                                    .update(cx, |dialog, cx| dialog.finish(window, cx))
                                    .ok();
                            }
                        },
                    )),
            )
            .into_any_element()
    }

    fn rows(
        &mut self,
        range: std::ops::Range<usize>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = theme.colors;
        let focused = self.list_focus.is_focused(window);
        let now = Zoned::now();
        let rows: Vec<CustomEmoji> = self
            .live(cx)
            .into_iter()
            .skip(range.start)
            .take(range.len())
            .collect();
        emoji_images::request_later(&self.model, rows.iter().map(|e| e.id).collect(), cx);
        let this = cx.entity().downgrade();
        rows.into_iter()
            .enumerate()
            .map(|(offset, emoji)| {
                let index = range.start + offset;
                let id = emoji.id;
                let by = self
                    .model
                    .read(cx)
                    .core()
                    .display_name(emoji.created_by)
                    .to_owned();
                let when = ago_label(&to_zoned(emoji.created_at, &self.zone), &now);
                let unavailable = matches!(
                    self.model.read(cx).core().emoji_image(id),
                    Some(lazy_client_core::ImageState::Failed)
                );
                let credit = if unavailable {
                    format!(
                        "{} · Added by {by} · {when}",
                        states::EMOJI_PICTURE_UNAVAILABLE
                    )
                } else {
                    format!("Added by {by} · {when}")
                };
                let (rename_to, delete_to) = (this.clone(), this.clone());
                let current = focused && index == self.cursor;
                div()
                    .id(gpui::ElementId::NamedInteger(
                        "emoji-admin-row".into(),
                        index as u64,
                    ))
                    .debug_selector(move || format!("emoji-admin-row-{index}"))
                    .h(px(ROW_HEIGHT))
                    // The list sizes its rows to what they hold unless told to fill it.
                    .w_full()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_b_1()
                    .border_color(colors.border)
                    .when(current, |row| row.bg(colors.selection))
                    .child(self.picture(id, 32.0, theme, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.type_scale.body)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child(format!(":{}:", emoji.name)),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_muted)
                                    .child(credit),
                            ),
                    )
                    .child(row_button(
                        theme,
                        format!("emoji-admin-rename-{index}"),
                        "Rename",
                        move |window, cx| {
                            rename_to
                                .update(cx, |dialog, cx| {
                                    dialog.cursor = index;
                                    dialog.start_rename(id, window, cx)
                                })
                                .ok();
                        },
                    ))
                    .child(row_button(
                        theme,
                        format!("emoji-admin-delete-{index}"),
                        "Delete…",
                        move |window, cx| {
                            delete_to
                                .update(cx, |dialog, cx| {
                                    dialog.cursor = index;
                                    dialog.start_delete(id, window, cx)
                                })
                                .ok();
                        },
                    ))
                    .into_any_element()
            })
            .collect()
    }

    fn add_view(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let (choose_to, cancel_to, submit_to) = (this.clone(), this.clone(), this);
        let state = self.action(ActionKind::AddCustomEmoji, cx);
        let working = state == ActionState::InFlight;
        let core_error = match &state {
            ActionState::Failed(text) => Some(text.clone()),
            _ => None,
        };
        let check = self.name_check(cx);
        let (hint, problem) = match &check {
            EmojiNameCheck::Empty => (
                Some("Letters, digits, and underscores, 2 to 32.".to_owned()),
                None,
            ),
            EmojiNameCheck::Good(name) => (Some(format!("It will be written :{name}:")), None),
            EmojiNameCheck::Problem(text) => (None, Some(text.clone())),
        };
        let ready = matches!(check, EmojiNameCheck::Good(_)) && self.chosen.is_some() && !working;
        let picture_slot = ImageStore::global(cx).read(cx).get(&preview_key()).cloned();
        let preview: AnyElement = match (&self.chosen, picture_slot.clone()) {
            (Some(_), Some(Slot::Ready(decoded))) => {
                picture_box(Some(decoded.image), false, 64.0, theme)
            }
            (Some(_), Some(Slot::Failed(error))) => div()
                .debug_selector(|| "emoji-add-preview-failed".into())
                .text_size(theme.type_scale.small)
                .text_color(colors.danger)
                .child(error.text())
                .into_any_element(),
            (Some(_), _) => picture_box(None, false, 64.0, theme),
            (None, _) => div()
                .size(px(64.))
                .rounded(theme.radii.medium)
                .border_1()
                .border_color(colors.border_strong)
                .flex()
                .items_center()
                .justify_center()
                .text_size(theme.type_scale.small)
                .text_color(colors.text_faint)
                .child("No image")
                .into_any_element(),
        };
        // The file's name, and its size in pixels once its picture has been read. (Nothing here
        // asks the disk; the byte limit is the runtime's to enforce and report.)
        let pixels = match &picture_slot {
            Some(Slot::Ready(decoded)) => Some((decoded.width, decoded.height)),
            _ => None,
        };
        let file_line = self.chosen.as_ref().map(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            match pixels {
                Some((w, h)) => format!("{name} · {w} × {h}"),
                None => name,
            }
        });
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .debug_selector(|| "emoji-add-preview".into())
                            .child(preview),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(form::button(
                                theme,
                                "emoji-add-choose",
                                if self.chosen.is_some() {
                                    "Choose another image…"
                                } else {
                                    "Choose image…"
                                },
                                ButtonKind::Secondary,
                                !working,
                                &self.choose_focus,
                                move |_, cx| {
                                    choose_to
                                        .update(cx, |dialog, cx| dialog.choose_file(cx))
                                        .ok();
                                },
                            ))
                            .child(
                                div()
                                    .debug_selector(|| "emoji-add-file".into())
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_muted)
                                    .child(file_line.unwrap_or_else(|| {
                                        "A PNG, JPEG, GIF, or WebP picture, at most 512 KB."
                                            .to_owned()
                                    })),
                            ),
                    ),
            )
            .children(self.picker_error.clone().map(|text| {
                div()
                    .debug_selector(|| "emoji-add-picker-error".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.danger)
                    .child(text)
            }))
            .child(form::labeled_field(
                theme,
                "Name",
                &self.name,
                hint.map(Into::into),
                problem.map(Into::into),
                window,
                cx,
            ))
            // What the core says went wrong with sending (the name field is for the name's own
            // problems: this one may be about the picture).
            .children(
                core_error
                    .clone()
                    .map(|text| error_block("emoji-add-error", text, theme)),
            )
            .children(working.then(|| {
                div()
                    .debug_selector(|| "emoji-add-progress".into())
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child("Uploading the picture…")
            }))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "emoji-add-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        true,
                        &self.cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "emoji-add-submit",
                        if working {
                            "Adding…"
                        } else if core_error.is_some() {
                            states::TRY_AGAIN
                        } else {
                            "Add emoji"
                        },
                        ButtonKind::Primary,
                        ready,
                        &self.submit_focus,
                        move |_, cx| {
                            submit_to.update(cx, |dialog, cx| dialog.submit(cx)).ok();
                        },
                    )),
            )
            .into_any_element()
    }

    fn rename_view(
        &mut self,
        id: EmojiId,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let (cancel_to, save_to) = (this.clone(), this);
        let kind = ActionKind::RenameCustomEmoji(id);
        let state = self.action(kind, cx);
        let working = state == ActionState::InFlight;
        let core_error = match &state {
            ActionState::Failed(text) => Some(text.clone()),
            _ => None,
        };
        let check = self.name_check(cx);
        let current = self
            .model
            .read(cx)
            .core()
            .custom_emoji_label(id)
            .unwrap_or("")
            .to_owned();
        let unchanged = matches!(&check, EmojiNameCheck::Good(name) if *name == current);
        let (hint, problem) = match &check {
            EmojiNameCheck::Empty => (
                Some("Letters, digits, and underscores, 2 to 32.".to_owned()),
                None,
            ),
            EmojiNameCheck::Good(name) => (
                Some(if unchanged {
                    "That is its name now.".to_owned()
                } else {
                    format!("Messages and reactions will show :{name}:")
                }),
                None,
            ),
            EmojiNameCheck::Problem(text) => (None, Some(text.clone())),
        };
        let ready = matches!(check, EmojiNameCheck::Good(_)) && !unchanged && !working;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(self.picture(id, 40.0, theme, cx))
                    .child(
                        div()
                            .text_size(theme.type_scale.body)
                            .text_color(theme.colors.text)
                            .child(format!("Rename :{current}:")),
                    ),
            )
            .child(form::labeled_field(
                theme,
                "Name",
                &self.name,
                hint.map(Into::into),
                problem.map(Into::into),
                window,
                cx,
            ))
            .children(core_error.map(|text| error_block("emoji-rename-error", text, theme)))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "emoji-rename-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        true,
                        &self.cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "emoji-rename-save",
                        if working { "Saving…" } else { "Save" },
                        ButtonKind::Primary,
                        ready,
                        &self.submit_focus,
                        move |_, cx| {
                            save_to.update(cx, |dialog, cx| dialog.submit(cx)).ok();
                        },
                    )),
            )
            .into_any_element()
    }

    fn delete_view(&mut self, id: EmojiId, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let (cancel_to, delete_to) = (this.clone(), this);
        let kind = ActionKind::DeleteCustomEmoji(id);
        let state = self.action(kind, cx);
        let working = state == ActionState::InFlight;
        let error = match &state {
            ActionState::Failed(text) => Some(text.clone()),
            _ => None,
        };
        let name = self
            .model
            .read(cx)
            .core()
            .custom_emoji_label(id)
            .unwrap_or("emoji")
            .to_owned();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(self.picture(id, 40.0, theme, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(theme.type_scale.body)
                            .text_color(theme.colors.text)
                            .child(states::delete_emoji_prompt(&name)),
                    ),
            )
            .children(error.map(|text| {
                div()
                    .debug_selector(|| "emoji-delete-error".into())
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.danger)
                    .child(text)
            }))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(form::button(
                        theme,
                        "emoji-delete-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        !working,
                        &self.confirm_cancel_focus,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        theme,
                        "emoji-delete-confirm",
                        if working {
                            "Deleting…"
                        } else {
                            "Delete emoji"
                        },
                        ButtonKind::Danger,
                        !working,
                        &self.confirm_delete_focus,
                        move |_, cx| {
                            delete_to
                                .update(cx, |dialog, cx| dialog.delete(id, cx))
                                .ok();
                        },
                    )),
            )
            .into_any_element()
    }
}

/// A problem in the danger color, announced as an alert.
fn error_block(selector: &'static str, text: String, theme: &Theme) -> AnyElement {
    div()
        .id(selector)
        .debug_selector(move || selector.into())
        .role(Role::Alert)
        .text_size(theme.type_scale.small)
        .text_color(theme.colors.danger)
        .child(text)
        .into_any_element()
}

/// A picture in a `size` square, a stand-in while it loads, or a note when it could not be fetched.
fn picture_box(
    image: Option<std::sync::Arc<gpui::RenderImage>>,
    failed: bool,
    size: f32,
    theme: &Theme,
) -> AnyElement {
    let colors = theme.colors;
    match image {
        Some(image) => emoji_images::square_picture(&image, size)
            .debug_selector(|| "emoji-picture".into())
            .into_any_element(),
        None => div()
            .debug_selector(move || {
                if failed {
                    "emoji-picture-failed".into()
                } else {
                    "emoji-picture-loading".into()
                }
            })
            .size(px(size))
            .flex_none()
            .rounded(theme.radii.small)
            .bg(colors.surface_hover)
            .flex()
            .items_center()
            .justify_center()
            .text_size(theme.type_scale.small)
            .text_color(if failed {
                colors.danger
            } else {
                colors.text_faint
            })
            .child(if failed { "!" } else { "…" })
            .into_any_element(),
    }
}

/// A small text button on a row (mouse; the keyboard acts on the row's cursor).
fn row_button(
    theme: &Theme,
    selector: String,
    label: &'static str,
    on_click: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .id(gpui::ElementId::Name(selector.clone().into()))
        .debug_selector(move || selector)
        .role(Role::Button)
        .aria_label(label)
        .flex_none()
        .cursor_pointer()
        .px_2()
        .py_0p5()
        .rounded(theme.radii.small)
        .border_1()
        .border_color(theme.colors.border_strong)
        .bg(theme.colors.surface_raised)
        .text_size(theme.type_scale.small)
        .text_color(theme.colors.text)
        .hover(|style| style.bg(theme.colors.surface_hover))
        .on_click(move |_, window, cx| on_click(window, cx))
        .child(label)
        .into_any_element()
}

impl Render for CustomEmojiDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let this = cx.entity().downgrade();
        let body = match self.view {
            View::List => self.list_view(&theme, window, cx),
            View::Add => self.add_view(&theme, window, cx),
            View::Rename(id) => self.rename_view(id, &theme, window, cx),
            View::ConfirmDelete(id) => self.delete_view(id, &theme, cx),
        };
        let title = match self.view {
            View::List => "Custom emoji",
            View::Add => "Add custom emoji",
            View::Rename(_) => "Rename custom emoji",
            View::ConfirmDelete(_) => "Delete custom emoji",
        };
        dialog_overlay(
            &theme,
            title,
            520.,
            &self.trap,
            body,
            "Close",
            move |window, cx| {
                this.update(cx, |dialog, cx| dialog.finish(window, cx)).ok();
            },
        )
        .on_action(cx.listener(|this, _: &DialogDismiss, window, cx| this.dismiss(window, cx)))
        .on_action(cx.listener(|this, _: &DialogNext, window, cx| this.trap.tab(false, window, cx)))
        .on_action(cx.listener(|this, _: &DialogPrev, window, cx| this.trap.tab(true, window, cx)))
    }
}
