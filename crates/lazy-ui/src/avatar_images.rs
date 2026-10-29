//! People's avatars for the views: asking for the pictures about to be drawn and reading the
//! decoded ones.
//!
//! The client core fetches and keeps the encoded picture (`avatar`); it is decoded on a background
//! thread into the window's [`ImageStore`], once per person and version however many times it is
//! drawn. Views ask from [`request_later`], after a frame, for the people on screen only: never
//! for every member of a large workspace, and never while drawing. A person with no avatar
//! (`avatar_version` 0), or whose picture has not arrived, shows their initial.

use std::sync::Arc;

use gpui::{App, Entity, RenderImage};
use lazy_client_core::{ClientCore, ImageState, Input};
use lazy_domain::UserId;

use crate::app_model::AppModel;
use crate::image_store::{AVATAR_EDGE, ImageKey, ImageStore, Slot, Source};

/// The avatar version the core knows for `user` (0: none): the person's own record, the people of
/// any workspace, the directory pages held, and the loaded profile.
fn version_of(core: &ClientCore, user: UserId) -> u32 {
    core.avatar_version(user)
}

/// The decoded picture of `user` at `version` (0: none), if the store holds it. For views that
/// were handed the person with their version (the directory, a profile) and so need not rely on
/// the member list holding them.
pub fn image_at(store: &ImageStore, user: UserId, version: u32) -> Option<Arc<RenderImage>> {
    if version == 0 {
        return None;
    }
    match store.get(&ImageKey::Avatar(user, version)) {
        Some(Slot::Ready(decoded)) => Some(decoded.image.clone()),
        _ => None,
    }
}

/// The decoded picture of `user`, if they have one and the store holds it.
pub fn image_of(core: &ClientCore, store: &ImageStore, user: UserId) -> Option<Arc<RenderImage>> {
    image_at(store, user, version_of(core, user))
}

/// Asks for the avatars of these people that are not known yet: the core to fetch each picture,
/// and the store to decode the ones the core already has. Call from a deferred callback.
pub fn request(model: &Entity<AppModel>, users: &[UserId], cx: &mut App) {
    let wanted: Vec<(UserId, u32)> = {
        let core = model.read(cx).core();
        users
            .iter()
            .map(|user| (*user, version_of(core, *user)))
            .collect()
    };
    request_versions(model, &wanted, cx);
}

/// Like [`request`], for people whose avatar version the caller knows.
pub fn request_versions(model: &Entity<AppModel>, wanted: &[(UserId, u32)], cx: &mut App) {
    let store = ImageStore::global(cx);
    let mut load = Vec::new();
    let mut decode = Vec::new();
    {
        let core = model.read(cx).core();
        let known = store.read(cx);
        for (user, version) in wanted {
            let (user, version) = (*user, *version);
            if version == 0 || known.has(&ImageKey::Avatar(user, version)) {
                continue;
            }
            match core.avatar(user) {
                None => load.push(user),
                Some(ImageState::Ready(data)) => decode.push((user, version, data.clone())),
                Some(ImageState::Loading | ImageState::Failed) => {}
            }
        }
    }
    if !load.is_empty() {
        model.update(cx, |model, cx| {
            for user in load {
                model.dispatch(Input::LoadAvatar(user), cx);
            }
        });
    }
    if !decode.is_empty() {
        store.update(cx, |store, cx| {
            for (user, version, data) in decode {
                store.request(
                    ImageKey::Avatar(user, version),
                    Source::Data(data),
                    AVATAR_EDGE,
                    cx,
                );
            }
        });
    }
}

/// Queues [`request_versions`] to run after the current frame.
pub fn request_versions_later(model: &Entity<AppModel>, wanted: Vec<(UserId, u32)>, cx: &mut App) {
    if wanted.is_empty() {
        return;
    }
    let model = model.clone();
    cx.defer(move |cx| request_versions(&model, &wanted, cx));
}

/// Queues [`request`] to run after the current frame. For `render` code: it collects the ids and
/// hands them here.
pub fn request_later(model: &Entity<AppModel>, users: Vec<UserId>, cx: &mut App) {
    if users.is_empty() {
        return;
    }
    let model = model.clone();
    cx.defer(move |cx| request(&model, &users, cx));
}
