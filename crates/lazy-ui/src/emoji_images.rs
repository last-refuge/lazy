//! Custom emoji pictures for the views: how an emoji reads (a picture, or its `:name:`), and
//! asking for the pictures that are about to be drawn.
//!
//! The client core fetches and keeps the encoded image (`emoji_image`); the picture itself is
//! decoded on a background thread into the window's [`ImageStore`], once per emoji however many
//! times it is drawn. Views ask from [`request`], after a frame, never while drawing.

use std::sync::Arc;

use gpui::{App, Div, Entity, ParentElement as _, RenderImage, Styled as _, div, px};
use lazy_client_core::{ClientCore, ImageState, Input};
use lazy_domain::EmojiId;

use crate::app_model::AppModel;
use crate::image_store::{EMOJI_EDGE, ImageKey, ImageStore, Slot, Source};
use crate::rich_text::{EmojiLook, EmojiPicture};
use crate::suggest::{CustomChoice, MAX_SUGGESTIONS, Pictures};

/// `:name:` for the emoji: its current name if the workspace knows it (a rename shows at once),
/// else `written`, the name a document gave it, else a stand-in.
pub fn label(core: &ClientCore, id: EmojiId, written: Option<&str>) -> String {
    let name = core
        .custom_emoji_label(id)
        .or(written)
        .unwrap_or("unknown-emoji");
    format!(":{name}:")
}

/// How the emoji reads now. A picture only for a live emoji whose picture has been decoded.
pub fn look(
    core: &ClientCore,
    store: &ImageStore,
    id: EmojiId,
    written: Option<&str>,
) -> EmojiLook {
    let live = core
        .custom_emoji_by_id(id)
        .is_some_and(|emoji| !emoji.deleted);
    let picture = live.then(|| picture(store, id)).flatten();
    EmojiLook {
        label: label(core, id, written),
        picture,
    }
}

/// The custom emoji whose names begin with what was typed after a colon, for autocomplete.
pub fn choices(core: &ClientCore, prefix: &str) -> Vec<CustomChoice> {
    core.custom_emoji_suggestions(prefix, MAX_SUGGESTIONS)
        .into_iter()
        .map(|emoji| CustomChoice {
            id: emoji.id,
            name: emoji.name.clone(),
        })
        .collect()
}

/// The decoded pictures the store has among `ids`.
pub fn pictures(store: &ImageStore, ids: &[EmojiId]) -> Pictures {
    ids.iter()
        .filter_map(|id| picture(store, *id).map(|picture| (*id, picture.image)))
        .collect()
}

/// The decoded picture of `id`, if the store has it.
pub fn picture(store: &ImageStore, id: EmojiId) -> Option<EmojiPicture> {
    match store.get(&ImageKey::Emoji(id)) {
        Some(Slot::Ready(decoded)) if decoded.height > 0 => Some(EmojiPicture {
            image: decoded.image.clone(),
            aspect: decoded.width as f32 / decoded.height as f32,
        }),
        _ => None,
    }
}

/// Asks for the pictures of these emoji that are not known yet: the core to fetch each image, and
/// the store to decode the ones the core already has. Deleted and unknown emoji are not asked
/// for. Call from a deferred callback, not from `render`.
pub fn request(model: &Entity<AppModel>, ids: &[EmojiId], cx: &mut App) {
    let store = ImageStore::global(cx);
    let mut load = Vec::new();
    let mut decode = Vec::new();
    {
        let core = model.read(cx).core();
        let known = store.read(cx);
        for id in ids {
            if core
                .custom_emoji_by_id(*id)
                .is_none_or(|emoji| emoji.deleted)
                || known.has(&ImageKey::Emoji(*id))
            {
                continue;
            }
            match core.emoji_image(*id) {
                None => load.push(*id),
                Some(ImageState::Ready(data)) => decode.push((*id, data.clone())),
                Some(ImageState::Loading | ImageState::Failed) => {}
            }
        }
    }
    if !load.is_empty() {
        model.update(cx, |model, cx| {
            for id in load {
                model.dispatch(Input::LoadEmojiImage(id), cx);
            }
        });
    }
    if !decode.is_empty() {
        store.update(cx, |store, cx| {
            for (id, data) in decode {
                store.request(ImageKey::Emoji(id), Source::Data(data), EMOJI_EDGE, cx);
            }
        });
    }
}

/// Queues [`request`] to run after the current frame. For `render` code: it collects the ids and
/// hands them here.
pub fn request_later(model: &Entity<AppModel>, ids: Vec<EmojiId>, cx: &mut App) {
    if ids.is_empty() {
        return;
    }
    let model = model.clone();
    cx.defer(move |cx| request(&model, &ids, cx));
}

/// `image` as large as fits a `size` px square with its shape kept, centered in the square. The
/// size is worked out here rather than left to the image element: a tall picture given
/// `size_full()` inside a flex row is laid out at its own height, overflowing its cell.
pub fn square_picture(image: &Arc<RenderImage>, size: f32) -> Div {
    let pixels = image.size(0);
    let (w, h) = (
        (pixels.width.0 as f32).max(1.0),
        (pixels.height.0 as f32).max(1.0),
    );
    let (w, h) = if w >= h {
        (size, size * h / w)
    } else {
        (size * w / h, size)
    };
    div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(gpui::img(image.clone()).w(px(w)).h(px(h)))
}
