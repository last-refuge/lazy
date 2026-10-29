//! The pieces an attachment is drawn with: an image tile, a file chip with its download state, a
//! chip in the composer's tray, and a progress bar.
//!
//! Everything here is a plain function from what to show to elements; nothing reads the client
//! core or the image store, so the timeline, the thread pane, the composer, and the viewer feed it
//! and decide what the clicks do. Keyboard focus is shown with a ring but is not tracked here:
//! the timeline's cursor and the tray's cursor own it.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, ElementId, FontWeight, InteractiveElement as _, IntoElement, ObjectFit,
    ParentElement as _, RenderImage, Role, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Window, div, img, px, relative,
};

use crate::attachments::{FileKind, fraction, middle_ellipsis};
use crate::theme::Theme;

/// What an action on an attachment does when it is clicked.
pub type Action = Rc<dyn Fn(&mut Window, &mut App)>;

/// How much of a file name a chip shows before it cuts the middle.
const NAME_CHARS: usize = 28;

/// What an image tile shows.
#[derive(Clone)]
pub enum Preview {
    /// The thumbnail is on its way: a placeholder of the right shape.
    Loading,
    Ready(Arc<RenderImage>),
    /// It could not be shown; the text says why.
    Failed(String),
}

/// An image in a message: its preview in a box of the image's own shape.
pub struct ImageTile {
    pub id: ElementId,
    /// Test label, and the base of the labels of the tile's parts.
    pub selector: String,
    pub name: String,
    /// The box, in logical pixels ([`crate::attachments::preview_box`]).
    pub size: (f32, f32),
    pub preview: Preview,
    /// The keyboard cursor is on this tile.
    pub focused: bool,
    pub on_open: Action,
}

/// A progress bar, `fraction` of the way across.
pub fn progress_bar(theme: &Theme, fraction: f32, selector: String) -> AnyElement {
    div()
        .debug_selector(move || selector)
        .h(px(4.))
        .w_full()
        .rounded_full()
        .bg(theme.colors.border)
        .child(
            div()
                .h_full()
                .w(relative(fraction.clamp(0.0, 1.0)))
                .rounded_full()
                .bg(theme.colors.accent),
        )
        .into_any_element()
}

pub fn image_tile(tile: ImageTile, theme: &Theme) -> AnyElement {
    let colors = theme.colors;
    let (width, height) = tile.size;
    let on_open = tile.on_open.clone();
    let label = format!("Open image {}", tile.name);
    let selector = tile.selector.clone();
    let failed = matches!(tile.preview, Preview::Failed(_));
    let body: AnyElement = match tile.preview {
        Preview::Ready(image) => img(image)
            .size_full()
            .object_fit(ObjectFit::Cover)
            .rounded(theme.radii.medium)
            .into_any_element(),
        Preview::Loading => div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(theme.type_scale.small)
            .text_color(colors.text_faint)
            .child("Loading…")
            .into_any_element(),
        Preview::Failed(reason) => div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .px_2()
            .text_size(theme.type_scale.small)
            .text_color(colors.danger)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(middle_ellipsis(&tile.name, 28)),
            )
            .child(div().text_color(colors.text_muted).child(reason))
            .into_any_element(),
    };
    div()
        .id(tile.id)
        .debug_selector(move || selector)
        .role(Role::Button)
        .aria_label(label)
        .flex_none()
        .w(px(width))
        .h(px(height))
        .rounded(theme.radii.medium)
        .border_2()
        .border_color(if tile.focused {
            colors.focus_ring
        } else {
            gpui::transparent_black()
        })
        .bg(if failed {
            colors.danger_surface
        } else {
            colors.surface_hover
        })
        .cursor_pointer()
        .on_click(move |_, window, cx| on_open(window, cx))
        .child(body)
        .children(tile.focused.then(cursor_marker))
        .into_any_element()
}

/// Where a file's download stands.
#[derive(Debug, Clone, PartialEq)]
pub enum Transfer {
    /// The message is still being sent: the file is on its way up, and there is nothing to do
    /// with it yet. `fraction` is `None` before the first byte.
    Uploading {
        fraction: Option<f32>,
        label: String,
    },
    Idle,
    Active {
        fraction: f32,
    },
    Done {
        path: String,
    },
    Failed(String),
}

/// A file in a message that is not shown as an image.
pub struct FileChip {
    pub id: ElementId,
    pub selector: String,
    pub name: String,
    pub size_text: String,
    pub kind: FileKind,
    pub transfer: Transfer,
    /// The keyboard cursor is on this chip.
    pub focused: bool,
    pub on_download: Action,
    pub on_cancel: Action,
    pub on_retry: Action,
    pub on_reveal: Action,
}

fn small_button(
    theme: &Theme,
    id: SharedString,
    label: &'static str,
    on_click: Action,
) -> AnyElement {
    let selector = id.to_string();
    div()
        .id(ElementId::Name(id))
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

/// The square with the kind of file written in it.
fn type_icon(theme: &Theme, kind: FileKind, selector: String) -> AnyElement {
    div()
        .id(ElementId::Name(selector.clone().into()))
        .debug_selector(move || selector)
        .role(Role::Image)
        .aria_label(kind.label())
        .size(px(36.))
        .flex_none()
        .rounded(theme.radii.small)
        .bg(theme.colors.accent_surface)
        .text_color(theme.colors.accent)
        .text_size(px(10.))
        .font_weight(FontWeight::BOLD)
        .flex()
        .items_center()
        .justify_center()
        .child(kind.glyph())
        .into_any_element()
}

pub fn file_chip(chip: FileChip, theme: &Theme) -> AnyElement {
    let colors = theme.colors;
    let base = chip.selector.clone();
    let name = middle_ellipsis(&chip.name, NAME_CHARS);
    let button_id = |what: &str| SharedString::from(format!("{base}-{what}"));
    let (detail, action): (AnyElement, Option<AnyElement>) = match &chip.transfer {
        Transfer::Idle => (
            div()
                .text_size(theme.type_scale.small)
                .text_color(colors.text_muted)
                .child(chip.size_text.clone())
                .into_any_element(),
            Some(small_button(
                theme,
                button_id("download"),
                "Download",
                chip.on_download.clone(),
            )),
        ),
        Transfer::Uploading {
            fraction: done,
            label,
        } => (
            div()
                .debug_selector({
                    let selector = format!("{base}-uploading");
                    move || selector
                })
                .flex()
                .items_center()
                .gap_2()
                .text_size(theme.type_scale.small)
                .text_color(colors.text_muted)
                .child(div().flex_1().child(progress_bar(
                    theme,
                    done.unwrap_or(0.0),
                    format!("{base}-progress"),
                )))
                .child(label.clone())
                .into_any_element(),
            None,
        ),
        Transfer::Active { fraction: done } => (
            // One line, so the row does not change height as the download goes.
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(theme.type_scale.small)
                .text_color(colors.text_muted)
                .child(
                    div()
                        .flex_1()
                        .child(progress_bar(theme, *done, format!("{base}-progress"))),
                )
                .child(format!("{}%", (done * 100.0).round() as u32))
                .into_any_element(),
            Some(small_button(
                theme,
                button_id("cancel"),
                "Cancel",
                chip.on_cancel.clone(),
            )),
        ),
        Transfer::Done { path } => (
            div()
                .debug_selector({
                    let selector = format!("{base}-saved");
                    move || selector
                })
                .text_size(theme.type_scale.small)
                .text_color(colors.success)
                .truncate()
                .child(format!("Saved to {path}"))
                .into_any_element(),
            Some(small_button(
                theme,
                button_id("reveal"),
                "Show in folder",
                chip.on_reveal.clone(),
            )),
        ),
        Transfer::Failed(reason) => (
            div()
                .debug_selector({
                    let selector = format!("{base}-failed");
                    move || selector
                })
                .text_size(theme.type_scale.small)
                .text_color(colors.danger)
                .child(reason.clone())
                .into_any_element(),
            Some(small_button(
                theme,
                button_id("retry"),
                "Retry",
                chip.on_retry.clone(),
            )),
        ),
    };
    let selector = chip.selector.clone();
    div()
        .id(chip.id)
        .debug_selector(move || selector)
        .flex()
        .items_center()
        .gap_3()
        .w(px(340.))
        .max_w_full()
        .px_3()
        .py_2()
        .rounded(theme.radii.medium)
        .border_2()
        .border_color(if chip.focused {
            colors.focus_ring
        } else {
            colors.border
        })
        .bg(colors.surface_raised)
        .child(type_icon(theme, chip.kind, format!("{base}-icon")))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .truncate()
                        .text_size(theme.type_scale.body)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors.text)
                        .child(name),
                )
                .child(detail),
        )
        .children(action)
        .children(chip.focused.then(cursor_marker))
        .into_any_element()
}

/// A one-pixel mark inside whatever holds the keyboard cursor, for tests to find it by.
fn cursor_marker() -> AnyElement {
    div()
        .debug_selector(|| "attachment-cursor".into())
        .absolute()
        .top_0()
        .left_0()
        .size(px(1.))
        .into_any_element()
}

/// Where a chip in the composer's tray stands.
#[derive(Debug, Clone, PartialEq)]
pub enum TrayState {
    /// The file is being copied into the profile.
    Reading,
    Uploading {
        fraction: f32,
    },
    Uploaded,
    Failed(String),
}

/// One file waiting in the composer.
pub struct TrayChip {
    pub id: ElementId,
    pub selector: String,
    pub name: String,
    pub size_text: String,
    pub kind: FileKind,
    /// A small preview, for an image whose pixels have been read.
    pub preview: Option<Arc<RenderImage>>,
    pub state: TrayState,
    /// A failed file can be uploaded again (not one that could not be read or was too large).
    pub can_retry: bool,
    /// The tray's cursor is on this chip.
    pub focused: bool,
    pub on_remove: Action,
    pub on_retry: Action,
}

/// A square button with a glyph in it, for the small actions on a chip.
fn icon_button(
    theme: &Theme,
    id: SharedString,
    glyph: &'static str,
    label: &'static str,
    on_click: Action,
) -> AnyElement {
    let selector = id.to_string();
    div()
        .id(ElementId::Name(id))
        .debug_selector(move || selector)
        .role(Role::Button)
        .aria_label(label)
        .size(px(24.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .rounded(theme.radii.small)
        .text_size(theme.type_scale.small)
        .text_color(theme.colors.text_muted)
        .hover(|style| {
            style
                .bg(theme.colors.surface_hover)
                .text_color(theme.colors.text)
        })
        .on_click(move |_, window, cx| on_click(window, cx))
        .child(glyph)
        .into_any_element()
}

pub fn tray_chip(chip: TrayChip, theme: &Theme) -> AnyElement {
    let colors = theme.colors;
    let base = chip.selector.clone();
    let lead: AnyElement = match chip.preview {
        Some(image) => img(image)
            .size(px(36.))
            .flex_none()
            .object_fit(ObjectFit::Cover)
            .rounded(theme.radii.small)
            .into_any_element(),
        None => type_icon(theme, chip.kind, format!("{base}-icon")),
    };
    let small = theme.type_scale.small;
    let size_then = |rest: String| {
        if chip.size_text.is_empty() {
            rest
        } else {
            format!("{} · {rest}", chip.size_text)
        }
    };
    let detail: AnyElement = match &chip.state {
        TrayState::Reading => div()
            .truncate()
            .text_size(small)
            .text_color(colors.text_muted)
            .child(size_then("Reading…".to_owned()))
            .into_any_element(),
        TrayState::Uploading { fraction: done } => div()
            .flex()
            .flex_col()
            .gap_1()
            .child(progress_bar(theme, *done, format!("{base}-progress")))
            .child(
                div()
                    .truncate()
                    .text_size(small)
                    .text_color(colors.text_muted)
                    .child(size_then(if *done <= 0.0 {
                        "Uploading…".to_owned()
                    } else {
                        format!("{}%", (done * 100.0).round() as u32)
                    })),
            )
            .into_any_element(),
        TrayState::Uploaded | TrayState::Failed(_) => div()
            .truncate()
            .text_size(small)
            .text_color(colors.text_muted)
            .child(chip.size_text.clone())
            .into_any_element(),
    };
    let failure = match &chip.state {
        TrayState::Failed(reason) => Some(reason.clone()),
        _ => None,
    };
    let failed = failure.is_some();
    let remove_label = if matches!(chip.state, TrayState::Uploading { .. } | TrayState::Reading) {
        "Cancel"
    } else {
        "Remove"
    };
    let selector = chip.selector.clone();
    let retry = (failed && chip.can_retry).then(|| {
        small_button(
            theme,
            SharedString::from(format!("{base}-retry")),
            "Retry",
            chip.on_retry.clone(),
        )
    });
    div()
        .id(chip.id)
        .debug_selector(move || selector)
        .flex()
        .flex_col()
        .gap_1p5()
        .w(px(236.))
        .max_w_full()
        .flex_none()
        .px_2()
        .py_1p5()
        .rounded(theme.radii.medium)
        .border_2()
        .border_color(if chip.focused {
            colors.focus_ring
        } else if failed {
            colors.danger
        } else {
            colors.border
        })
        .bg(if failed {
            colors.danger_surface
        } else {
            colors.surface_raised
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(lead)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .text_size(small)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors.text)
                                .child(middle_ellipsis(&chip.name, 26)),
                        )
                        .child(detail),
                )
                .child(icon_button(
                    theme,
                    SharedString::from(format!("{base}-remove")),
                    "✕",
                    remove_label,
                    chip.on_remove.clone(),
                )),
        )
        .children(failure.map(|reason| {
            div()
                .flex()
                .items_start()
                .gap_2()
                .child(
                    div()
                        .debug_selector({
                            let selector = format!("{base}-failed");
                            move || selector
                        })
                        .flex_1()
                        .min_w_0()
                        .text_size(small)
                        .text_color(colors.danger)
                        .child(reason),
                )
                .children(retry)
        }))
        .children(chip.focused.then(cursor_marker))
        .into_any_element()
}

/// A fraction for a transfer of `sent` of `total` bytes.
pub fn transfer_fraction(sent: u64, total: u64) -> f32 {
    fraction(sent, total)
}
