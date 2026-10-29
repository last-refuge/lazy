//! The image viewer: a modal over the whole window showing one image of a message at a time.
//!
//! Left and Right move between the message's images, Escape (or the scrim, or Close) closes it
//! and gives focus back to where it was. The full image is fetched when it is shown
//! (`LoadFullImage`) and released when it is left (`ReleaseFullImage`); while it loads, the
//! message's thumbnail stands in. The caption names the file and its size, and Download and Copy
//! name sit beside it.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Render, Role,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use lazy_client_core::{Change, DownloadState, ImageState, Input};
use lazy_domain::AttachmentId;
use lazy_protocol::Attachment;

use crate::app_model::{AppEvent, AppModel};
use crate::attachment_view::progress_bar;
use crate::attachments::{format_size, fraction, middle_ellipsis};
use crate::downloads::{self, Outcome};
use crate::form::{self, ButtonKind};
use crate::image_store::{FULL_EDGE, ImageKey, ImageStore, Slot, Source};
use crate::modal::{
    DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, fullscreen_overlay,
};
use crate::theme::Theme;

pub struct ImageViewer {
    model: Entity<AppModel>,
    images: Vec<Attachment>,
    index: usize,
    trap: ModalTrap,
    /// The picture's own stop, where focus starts: Left and Right work from here.
    picture_focus: FocusHandle,
    prev_focus: FocusHandle,
    next_focus: FocusHandle,
    download_focus: FocusHandle,
    copy_focus: FocusHandle,
    close_focus: FocusHandle,
    retry_focus: FocusHandle,
    copied: bool,
    /// Why the save dialog could not open, until the next try.
    note: Option<String>,
    /// Where each file was saved or is being saved, so a retry does not ask again.
    destinations: HashMap<AttachmentId, PathBuf>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DialogEvent> for ImageViewer {}

impl ImageViewer {
    /// Opens on `images[index]`. `images` is the message's pictures, in order.
    pub fn new(
        model: Entity<AppModel>,
        images: Vec<Attachment>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let index = index.min(images.len().saturating_sub(1));
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |_this, _model, event: &AppEvent, _window, cx| {
                let AppEvent::Changed(changes) = event;
                if changes.iter().any(|change| {
                    matches!(
                        change,
                        Change::ImageChanged { .. } | Change::DownloadChanged { .. }
                    )
                }) {
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.observe(&ImageStore::global(cx), |_, _, cx| cx.notify()));
        let mut trap = ModalTrap::new(cx);
        let picture_focus = cx.focus_handle().tab_stop(true);
        trap.open(&picture_focus, window, cx);
        let viewer = Self {
            model,
            images,
            index,
            trap,
            picture_focus,
            prev_focus: cx.focus_handle().tab_stop(true),
            next_focus: cx.focus_handle().tab_stop(true),
            download_focus: cx.focus_handle().tab_stop(true),
            copy_focus: cx.focus_handle().tab_stop(true),
            close_focus: cx.focus_handle().tab_stop(true),
            retry_focus: cx.focus_handle().tab_stop(true),
            copied: false,
            note: None,
            destinations: HashMap::new(),
            _subscriptions: subscriptions,
        };
        viewer.load_current(cx);
        viewer
    }

    /// The image being shown.
    pub fn current(&self) -> Option<&Attachment> {
        self.images.get(self.index)
    }

    /// Which of the images is showing (from 0), for tests.
    pub fn index(&self) -> usize {
        self.index
    }

    fn load_current(&self, cx: &mut Context<Self>) {
        if let Some(image) = self.current() {
            let id = image.id;
            self.model
                .update(cx, |model, cx| model.dispatch(Input::LoadFullImage(id), cx));
        }
    }

    fn release_current(&self, cx: &mut Context<Self>) {
        if let Some(image) = self.current() {
            let id = image.id;
            self.model.update(cx, |model, cx| {
                model.dispatch(Input::ReleaseFullImage(id), cx)
            });
        }
    }

    /// Shows the image `by` places on (`-1` for the one before), if there is one.
    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let next = self.index as isize + by;
        if next < 0 || next >= self.images.len() as isize {
            return;
        }
        self.release_current(cx);
        self.index = next as usize;
        self.copied = false;
        self.note = None;
        self.load_current(cx);
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.release_current(cx);
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }

    fn copy_name(&mut self, cx: &mut Context<Self>) {
        if let Some(image) = self.current() {
            cx.write_to_clipboard(ClipboardItem::new_string(image.name.clone()));
            self.copied = true;
            cx.notify();
        }
    }

    fn download(&mut self, cx: &mut Context<Self>) {
        let Some(image) = self.current().cloned() else {
            return;
        };
        let id = image.id;
        let remembered = self.destinations.get(&id).cloned();
        self.note = None;
        let this = cx.entity().downgrade();
        downloads::start(
            self.model.clone(),
            image,
            remembered,
            move |outcome, cx| {
                this.update(cx, |viewer, cx| {
                    match outcome {
                        Outcome::Started(path) => {
                            viewer.destinations.insert(id, path);
                        }
                        Outcome::Failed(reason) => viewer.note = Some(reason),
                        Outcome::Cancelled => {}
                    }
                    cx.notify();
                })
                .ok();
            },
            cx,
        );
    }

    fn picture(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(image) = self.current() else {
            return div().into_any_element();
        };
        let colors = theme.colors;
        let store = ImageStore::global(cx);
        let id = image.id;
        let full = self.model.read(cx).core().full_image(id).cloned();
        let thumbnail = match store.read(cx).get(&ImageKey::Thumbnail(id)) {
            Some(Slot::Ready(decoded)) => Some(decoded.image.clone()),
            _ => None,
        };
        let message = |text: &str| {
            div()
                .debug_selector(|| "viewer-status".into())
                .text_size(theme.type_scale.body)
                .text_color(colors.text_muted)
                .child(text.to_owned())
                .into_any_element()
        };
        let shown = match full {
            Some(ImageState::Ready(data)) => match store.read(cx).get(&ImageKey::Full(id)) {
                Some(Slot::Ready(decoded)) => Some(decoded.image.clone()),
                Some(Slot::Failed(error)) => {
                    return self.centered(message(&error.text()));
                }
                Some(Slot::Decoding) => None,
                None => {
                    let key = ImageKey::Full(id);
                    let store = store.clone();
                    cx.defer(move |cx| {
                        store.update(cx, |store, cx| {
                            store.request(key, Source::Data(data), FULL_EDGE, cx)
                        });
                    });
                    None
                }
            },
            Some(ImageState::Failed) => {
                let this = cx.entity().downgrade();
                return self.centered(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .debug_selector(|| "viewer-failed".into())
                                .text_size(theme.type_scale.body)
                                .text_color(colors.danger)
                                .child("The image couldn't be loaded."),
                        )
                        .child(form::button(
                            theme,
                            "viewer-retry",
                            "Try again",
                            ButtonKind::Secondary,
                            true,
                            &self.retry_focus,
                            move |_, cx| {
                                this.update(cx, |viewer, cx| viewer.load_current(cx)).ok();
                            },
                        ))
                        .into_any_element(),
                );
            }
            Some(ImageState::Loading) | None => None,
        };
        let _ = window;
        match (shown, thumbnail) {
            (Some(image), _) => div()
                .debug_selector(|| "viewer-image".into())
                .flex_1()
                .flex()
                .flex_col()
                .child(fitted(image))
                .into_any_element(),
            // The thumbnail stands in while the whole image is on its way.
            (None, Some(thumb)) => div()
                .flex_1()
                .flex()
                .flex_col()
                .relative()
                .child(fitted(thumb))
                .child(
                    div()
                        .absolute()
                        .bottom_2()
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .debug_selector(|| "viewer-loading".into())
                                .px_3()
                                .py_1()
                                .rounded(theme.radii.medium)
                                .bg(colors.surface_raised)
                                .text_size(theme.type_scale.small)
                                .text_color(colors.text_muted)
                                .child("Loading the full image…"),
                        ),
                )
                .into_any_element(),
            (None, None) => self.centered(
                div()
                    .debug_selector(|| "viewer-loading".into())
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text_muted)
                    .child("Loading…")
                    .into_any_element(),
            ),
        }
    }

    fn centered(&self, child: gpui::AnyElement) -> gpui::AnyElement {
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .child(child)
            .into_any_element()
    }
}

/// The picture, scaled to fit whatever room it is given and centered, painted directly: a
/// percentage size inside an absolutely placed box is not reliable in layout, and this needs only
/// the room that was actually laid out.
fn fitted(image: std::sync::Arc<gpui::RenderImage>) -> gpui::AnyElement {
    gpui::canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let fit = ObjectFit::Contain.get_bounds(bounds, image.size(0));
            window
                .paint_image(
                    bounds,
                    fit,
                    gpui::Corners::default(),
                    image.clone(),
                    0,
                    false,
                )
                .ok();
        },
    )
    .flex_1()
    .w_full()
    .into_any_element()
}

impl Focusable for ImageViewer {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.picture_focus.clone()
    }
}

impl Render for ImageViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let Some(image) = self.current().cloned() else {
            return fullscreen_overlay(&theme, &self.trap, div().into_any_element(), |_, _| {});
        };
        let count = self.images.len();
        let download = self.model.read(cx).core().download(image.id).cloned();
        let caption = format!(
            "{} · {}{}",
            middle_ellipsis(&image.name, 60),
            format_size(image.size),
            match (image.width, image.height) {
                (Some(w), Some(h)) => format!(" · {w}×{h}"),
                _ => String::new(),
            }
        );
        let status: Option<gpui::AnyElement> = match (&self.note, &download) {
            (Some(note), _) => Some(
                div()
                    .debug_selector(|| "viewer-download-note".into())
                    .text_color(colors.danger)
                    .child(note.clone())
                    .into_any_element(),
            ),
            (None, Some(DownloadState::Downloading { received, total })) => Some(
                div()
                    .debug_selector(|| "viewer-downloading".into())
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_color(colors.text_muted)
                    .child(div().w(px(120.)).child(progress_bar(
                        &theme,
                        fraction(*received, *total),
                        "viewer-progress".into(),
                    )))
                    .child(format!(
                        "Downloading… {}%",
                        (fraction(*received, *total) * 100.0).round() as u32
                    ))
                    .into_any_element(),
            ),
            (None, Some(DownloadState::Done { path })) => Some(
                div()
                    .debug_selector(|| "viewer-saved".into())
                    .text_color(colors.success)
                    .child(format!(
                        "Saved · {}",
                        middle_ellipsis(&path.to_string_lossy(), 50)
                    ))
                    .into_any_element(),
            ),
            (None, Some(DownloadState::Failed { reason })) => Some(
                div()
                    .debug_selector(|| "viewer-download-failed".into())
                    .text_color(colors.danger)
                    .child(reason.clone())
                    .into_any_element(),
            ),
            (None, None) => None,
        };
        let downloading = matches!(download, Some(DownloadState::Downloading { .. }));
        let picture = self.picture(&theme, window, cx);
        let (prev_to, next_to, close_to, copy_to, download_to, cancel_to) = (
            this.clone(),
            this.clone(),
            this.clone(),
            this.clone(),
            this.clone(),
            this.clone(),
        );
        let image_id = image.id;
        let can_prev = self.index > 0;
        let can_next = self.index + 1 < count;
        let body = div()
            .size_full()
            .flex()
            .flex_col()
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "left" => this.step(-1, cx),
                    "right" => this.step(1, cx),
                    "home" => this.step(-(this.index as isize), cx),
                    "end" => {
                        let last = this.images.len() as isize - 1;
                        this.step(last - this.index as isize, cx)
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(
                div()
                    .id("viewer-picture")
                    .debug_selector(|| "viewer-picture".into())
                    .role(Role::Image)
                    .aria_label(image.name.clone())
                    .track_focus(&self.picture_focus)
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .m_4()
                    // Clicking the picture is not clicking outside it.
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(picture),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .px_4()
                    .pb_4()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .debug_selector(|| "viewer-caption".into())
                            .flex()
                            .flex_col()
                            .items_center()
                            .px_3()
                            .py_1()
                            .rounded(theme.radii.medium)
                            .bg(colors.surface_raised)
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text)
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(caption))
                            .child(
                                div()
                                    .debug_selector(|| "viewer-position".into())
                                    .text_color(colors.text_muted)
                                    .child(format!("{} of {count}", self.index + 1)),
                            )
                            .children(status),
                    )
                    .child(form::button(
                        &theme,
                        "viewer-prev",
                        "‹ Previous",
                        ButtonKind::Secondary,
                        can_prev,
                        &self.prev_focus,
                        move |_, cx| {
                            prev_to.update(cx, |viewer, cx| viewer.step(-1, cx)).ok();
                        },
                    ))
                    .child(form::button(
                        &theme,
                        "viewer-next",
                        "Next ›",
                        ButtonKind::Secondary,
                        can_next,
                        &self.next_focus,
                        move |_, cx| {
                            next_to.update(cx, |viewer, cx| viewer.step(1, cx)).ok();
                        },
                    ))
                    .child(if downloading {
                        form::button(
                            &theme,
                            "viewer-cancel-download",
                            "Cancel download",
                            ButtonKind::Secondary,
                            true,
                            &self.download_focus,
                            move |_, cx| {
                                cancel_to
                                    .update(cx, |viewer, cx| {
                                        viewer.model.update(cx, |model, cx| {
                                            model.dispatch(Input::CancelDownload(image_id), cx)
                                        })
                                    })
                                    .ok();
                            },
                        )
                    } else {
                        form::button(
                            &theme,
                            "viewer-download",
                            "Download",
                            ButtonKind::Primary,
                            true,
                            &self.download_focus,
                            move |_, cx| {
                                download_to
                                    .update(cx, |viewer, cx| viewer.download(cx))
                                    .ok();
                            },
                        )
                    })
                    .child(form::button(
                        &theme,
                        "viewer-copy-name",
                        if self.copied { "Copied" } else { "Copy name" },
                        ButtonKind::Secondary,
                        true,
                        &self.copy_focus,
                        move |_, cx| {
                            copy_to.update(cx, |viewer, cx| viewer.copy_name(cx)).ok();
                        },
                    ))
                    .child(form::button(
                        &theme,
                        "viewer-close",
                        "Close",
                        ButtonKind::Secondary,
                        true,
                        &self.close_focus,
                        move |window, cx| {
                            close_to
                                .update(cx, |viewer, cx| viewer.close(window, cx))
                                .ok();
                        },
                    )),
            )
            .into_any_element();
        let scrim_to = this.clone();
        fullscreen_overlay(&theme, &self.trap, body, move |window, cx| {
            scrim_to
                .update(cx, |viewer, cx| viewer.close(window, cx))
                .ok();
        })
        .bg(gpui::black().opacity(0.88))
        .on_action(cx.listener(|this, _: &DialogDismiss, window, cx| this.close(window, cx)))
        .on_action(cx.listener(|this, _: &DialogNext, window, cx| this.trap.tab(false, window, cx)))
        .on_action(cx.listener(|this, _: &DialogPrev, window, cx| this.trap.tab(true, window, cx)))
    }
}

/// The label a screen reader gives the viewer, for hosts that name their overlays.
pub fn aria_title(image: &Attachment) -> SharedString {
    format!("Image {}", image.name).into()
}
