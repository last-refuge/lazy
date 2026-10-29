//! Custom emoji (slice 3B, D18): the workspace's list, the images, and the admin actions.
//!
//! The list arrives with the bootstrap and is kept current by `EmojiChanged` events, which carry
//! the whole emoji and are applied by id (a deleted emoji is never brought back). A message names
//! an emoji by id and reads `:name:` where the emoji is deleted or its image is missing. Adding,
//! renaming, and deleting are admin actions, not messages: they have no journal, and an unknown
//! outcome is reported rather than reconciled (adding repeats the same upload, which is
//! idempotent by id).

use std::collections::{HashMap, HashSet, VecDeque};

use lazy_domain::emoji::{MAX_CUSTOM_EMOJI, MAX_EMOJI_BYTES};
use lazy_domain::{EmojiId, EmojiName, NameError, WorkspaceId, WorkspaceRole};
use lazy_protocol::{CustomEmoji, CustomEmojiResponse, ErrorCode, RenameEmojiRequest};

use crate::attachments::{
    FileSource, ImageData, ImageState, LocalFile, MAX_CONCURRENT_IMAGE_FETCHES,
    MAX_HELD_EMOJI_IMAGE_BYTES, MAX_HELD_EMOJI_IMAGES, MAX_QUEUED_EMOJI_FETCHES,
    MAX_UPLOAD_ATTEMPTS, TransferError,
};
use crate::change::{Change, Output};
use crate::effect::Effect;
use crate::input::Input;
use crate::model::{
    ActionKind, ActionOutcome, ActionState, EmojiPrefs, RequestError, SessionPhase,
};

use super::ClientCore;

/// An add of a custom emoji that is being uploaded.
#[derive(Debug)]
struct Adding {
    id: EmojiId,
    workspace: WorkspaceId,
    name: EmojiName,
    source: FileSource,
    /// Uploads of unknown outcome so far.
    attempts: u32,
}

/// What the core keeps about custom emoji.
#[derive(Debug, Default)]
pub(super) struct Emojis {
    /// Every emoji of the workspace, deleted ones included, ordered by name and then id.
    list: Vec<CustomEmoji>,
    images: HashMap<EmojiId, ImageState>,
    /// Ready images, least recently used first.
    order: VecDeque<EmojiId>,
    bytes: usize,
    queue: VecDeque<EmojiId>,
    in_flight: HashSet<EmojiId>,
    adding: Option<Adding>,
    /// What the emoji preferences show: the saved ones without custom emoji that are not live.
    shown_prefs: EmojiPrefs,
}

impl Emojis {
    /// Whether `id` is one of this workspace's emoji (or being added, or being fetched).
    pub(super) fn holds(&self, id: EmojiId) -> bool {
        self.list.iter().any(|emoji| emoji.id == id)
            || self.images.contains_key(&id)
            || self.in_flight.contains(&id)
            || self.adding.as_ref().is_some_and(|adding| adding.id == id)
    }

    pub(super) fn list_slice(&self) -> &[CustomEmoji] {
        &self.list
    }
}

impl ClientCore {
    /// Every custom emoji of the workspace, live and deleted, ordered by name.
    pub fn custom_emoji(&self) -> &[CustomEmoji] {
        &self.emojis.list
    }

    pub fn custom_emoji_by_id(&self, id: EmojiId) -> Option<&CustomEmoji> {
        self.emojis.list.iter().find(|emoji| emoji.id == id)
    }

    /// The emoji that can be used now, ordered by name.
    pub fn live_custom_emoji(&self) -> impl Iterator<Item = &CustomEmoji> {
        self.emojis.list.iter().filter(|emoji| !emoji.deleted)
    }

    /// Live emoji whose name starts with `query` first, then those that merely contain it, each
    /// group ordered by name; at most `limit`. An empty query finds them all. For the picker.
    pub fn custom_emoji_search(&self, query: &str, limit: usize) -> Vec<&CustomEmoji> {
        let query = query.trim().trim_matches(':').to_ascii_lowercase();
        let mut found: Vec<&CustomEmoji> = self
            .live_custom_emoji()
            .filter(|emoji| emoji.name.starts_with(&query))
            .collect();
        if !query.is_empty() {
            found
                .extend(self.live_custom_emoji().filter(|emoji| {
                    !emoji.name.starts_with(&query) && emoji.name.contains(&query)
                }));
        }
        found.truncate(limit);
        found
    }

    /// Live emoji whose name starts with `prefix` (typed after a `:`), ordered by name; at most
    /// `limit`. Nothing for an empty prefix. For autocomplete.
    pub fn custom_emoji_suggestions(&self, prefix: &str, limit: usize) -> Vec<&CustomEmoji> {
        let prefix = prefix.trim_start_matches(':').to_ascii_lowercase();
        if prefix.is_empty() {
            return Vec::new();
        }
        self.live_custom_emoji()
            .filter(|emoji| emoji.name.starts_with(&prefix))
            .take(limit)
            .collect()
    }

    /// The name an emoji is shown by (`:name:`), even when it has been deleted. `None` when the
    /// emoji is not known.
    pub fn custom_emoji_label(&self, id: EmojiId) -> Option<&str> {
        self.custom_emoji_by_id(id).map(|emoji| emoji.name.as_str())
    }

    /// Whether the person may add, rename, and delete custom emoji: admins and owners.
    pub fn can_manage_emoji(&self) -> bool {
        self.primary_workspace()
            .is_some_and(|info| matches!(info.role, WorkspaceRole::Admin | WorkspaceRole::Owner))
    }

    /// The image of a custom emoji, once asked for with `Input::LoadEmojiImage`.
    pub fn emoji_image(&self, id: EmojiId) -> Option<&ImageState> {
        self.emojis.images.get(&id)
    }

    /// Whether `id` is a custom emoji that can be used now.
    pub(super) fn custom_emoji_is_live(&self, id: EmojiId) -> bool {
        self.live_custom_emoji().any(|emoji| emoji.id == id)
    }

    pub(super) fn emoji_input(&mut self, input: Input, out: &mut Output) {
        match input {
            Input::AddCustomEmoji { name, file } => self.add_custom_emoji(&name, file, out),
            Input::RenameCustomEmoji { id, name } => self.rename_custom_emoji(id, &name, out),
            Input::DeleteCustomEmoji(id) => self.delete_custom_emoji(id, out),
            Input::LoadEmojiImage(id) => self.load_emoji_image(id, out),
            Input::CustomEmojiUploaded { emoji, result } => {
                self.custom_emoji_uploaded(emoji, result, out)
            }
            Input::EmojiImageLoaded { emoji, result } => {
                self.emoji_image_loaded(emoji, result, out)
            }
            _ => {}
        }
    }
}

// ----- The list -----

impl ClientCore {
    /// Replaces the list with those of a bootstrap.
    pub(super) fn adopt_workspace_emoji(&mut self, workspace: &lazy_protocol::WorkspaceBootstrap) {
        let mut list: Vec<CustomEmoji> = workspace.custom_emoji.clone();
        sort_emoji(&mut list);
        // An emoji a bootstrap no longer lists has nothing to show.
        let gone: Vec<EmojiId> = self
            .emojis
            .list
            .iter()
            .filter(|held| {
                !list
                    .iter()
                    .any(|emoji| emoji.id == held.id && !emoji.deleted)
            })
            .map(|held| held.id)
            .collect();
        self.emojis.list = list;
        for id in gone {
            self.drop_emoji_image(id);
        }
        self.recompute_shown_prefs();
    }

    pub(super) fn restore_emoji(&mut self, emoji: Vec<CustomEmoji>) {
        self.emojis.list.extend(emoji);
        sort_emoji(&mut self.emojis.list);
        self.recompute_shown_prefs();
    }

    /// An emoji changed (an event, or the answer to a rename or an add): it replaces the one
    /// held by id. A deleted emoji is final.
    pub(super) fn store_custom_emoji(&mut self, emoji: CustomEmoji, out: &mut Output) {
        if !self
            .workspaces
            .iter()
            .any(|info| info.workspace.id == emoji.workspace_id)
        {
            return;
        }
        match self.emojis.list.iter_mut().find(|held| held.id == emoji.id) {
            Some(held) if held.deleted || *held == emoji => return,
            Some(held) => *held = emoji.clone(),
            None => self.emojis.list.push(emoji.clone()),
        }
        sort_emoji(&mut self.emojis.list);
        if emoji.deleted {
            let id = emoji.id;
            self.drop_emoji_image(id);
            out.effects
                .push(Effect::ForgetEmojiImages { emojis: vec![id] });
            out.changes.push(Change::EmojiImageChanged { emoji: id });
        }
        out.changes.push(Change::CustomEmojiChanged);
        if self.recompute_shown_prefs() {
            out.changes.push(Change::EmojiPrefsChanged);
        }
        self.cache_changed(true);
    }

    pub(super) fn apply_emoji_changed(&mut self, emoji: CustomEmoji, out: &mut Output) {
        self.store_custom_emoji(emoji, out);
    }

    /// Forgets everything about custom emoji (sign-out).
    pub(super) fn clear_emoji(&mut self) {
        self.emojis = Emojis::default();
    }

    /// The preferences as saved, with custom emoji that are not live left out of the recent ones
    /// (the saved file keeps them until they are pushed out). Returns whether what is shown
    /// changed.
    pub(super) fn recompute_shown_prefs(&mut self) -> bool {
        let mut shown = self.emoji_prefs.clone();
        shown.recent.retain(|emoji| {
            emoji
                .custom_id()
                .is_none_or(|id| self.custom_emoji_is_live(id))
        });
        let changed = shown != self.emojis.shown_prefs;
        self.emojis.shown_prefs = shown;
        changed
    }

    /// The emoji preferences to show.
    pub(super) fn shown_prefs(&self) -> &EmojiPrefs {
        &self.emojis.shown_prefs
    }
}

fn sort_emoji(list: &mut [CustomEmoji]) {
    list.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
}

// ----- Images -----

impl ClientCore {
    fn load_emoji_image(&mut self, id: EmojiId, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) || !self.custom_emoji_is_live(id) {
            return;
        }
        match self.emojis.images.get(&id) {
            Some(ImageState::Ready(_)) => {
                self.emojis.order.retain(|held| *held != id);
                self.emojis.order.push_back(id);
                return;
            }
            Some(ImageState::Loading | ImageState::Failed) => return,
            None => {}
        }
        self.emojis.images.insert(id, ImageState::Loading);
        self.emojis.queue.push_back(id);
        while self.emojis.queue.len() > MAX_QUEUED_EMOJI_FETCHES {
            if let Some(dropped) = self.emojis.queue.pop_front()
                && matches!(self.emojis.images.get(&dropped), Some(ImageState::Loading))
            {
                self.emojis.images.remove(&dropped);
            }
        }
        out.changes.push(Change::EmojiImageChanged { emoji: id });
        self.pump_emoji_images(out);
    }

    fn pump_emoji_images(&mut self, out: &mut Output) {
        while self.emojis.in_flight.len() < MAX_CONCURRENT_IMAGE_FETCHES {
            let Some(id) = self.emojis.queue.pop_front() else {
                break;
            };
            if matches!(self.emojis.images.get(&id), Some(ImageState::Loading))
                && self.emojis.in_flight.insert(id)
            {
                out.effects.push(Effect::FetchEmojiImage { emoji: id });
            }
        }
    }

    fn emoji_image_loaded(
        &mut self,
        id: EmojiId,
        result: Result<ImageData, TransferError>,
        out: &mut Output,
    ) {
        self.emojis.in_flight.remove(&id);
        if !matches!(self.emojis.images.get(&id), Some(ImageState::Loading)) {
            // Forgotten (the emoji was deleted) while it was on its way.
            self.pump_emoji_images(out);
            return;
        }
        match result {
            Ok(data) => {
                self.emojis.bytes += data.len();
                self.emojis.images.insert(id, ImageState::Ready(data));
                self.emojis.order.push_back(id);
                self.evict_emoji_images(id);
            }
            Err(TransferError::Request(error)) if error.is_unauthenticated() => {
                self.emojis.images.remove(&id);
                self.session_expired(out);
            }
            Err(TransferError::Cancelled) => {
                self.emojis.images.remove(&id);
            }
            Err(_) => {
                self.emojis.images.insert(id, ImageState::Failed);
            }
        }
        out.changes.push(Change::EmojiImageChanged { emoji: id });
        self.pump_emoji_images(out);
    }

    /// Drops the images used longest ago until the ones held fit their bounds. `keep` is the one
    /// that just arrived.
    fn evict_emoji_images(&mut self, keep: EmojiId) {
        while (self.emojis.order.len() > MAX_HELD_EMOJI_IMAGES
            || self.emojis.bytes > MAX_HELD_EMOJI_IMAGE_BYTES)
            && self.emojis.order.len() > 1
        {
            let Some(old) = self.emojis.order.pop_front() else {
                break;
            };
            if old == keep {
                self.emojis.order.push_back(old);
                continue;
            }
            if let Some(ImageState::Ready(data)) = self.emojis.images.remove(&old) {
                self.emojis.bytes = self.emojis.bytes.saturating_sub(data.len());
            }
        }
    }

    fn drop_emoji_image(&mut self, id: EmojiId) {
        if let Some(ImageState::Ready(data)) = self.emojis.images.remove(&id) {
            self.emojis.bytes = self.emojis.bytes.saturating_sub(data.len());
        }
        self.emojis.order.retain(|held| *held != id);
        self.emojis.queue.retain(|held| *held != id);
    }

    /// The connection is back: images that could not be fetched are asked for again the next
    /// time they are wanted.
    pub(super) fn retry_failed_emoji_images(&mut self, out: &mut Output) {
        let failed: Vec<EmojiId> = self
            .emojis
            .images
            .iter()
            .filter(|(_, state)| **state == ImageState::Failed)
            .map(|(id, _)| *id)
            .collect();
        for id in failed {
            self.emojis.images.remove(&id);
            out.changes.push(Change::EmojiImageChanged { emoji: id });
        }
    }
}

// ----- Admin actions -----

impl ClientCore {
    fn emoji_precondition(&mut self, kind: ActionKind, out: &mut Output) -> bool {
        if !matches!(self.phase, SessionPhase::Ready) {
            return false;
        }
        if self.action(kind) == &ActionState::InFlight {
            return false;
        }
        if !self.can_manage_emoji() {
            self.fail_action(kind, "Only admins and owners can manage custom emoji.", out);
            return false;
        }
        true
    }

    fn add_custom_emoji(&mut self, name: &str, file: LocalFile, out: &mut Output) {
        let kind = ActionKind::AddCustomEmoji;
        if !self.emoji_precondition(kind, out) {
            return;
        }
        let name = match EmojiName::parse(name.trim().trim_matches(':')) {
            Ok(name) => name,
            Err(error) => return self.fail_action(kind, emoji_name_problem(&error), out),
        };
        if self
            .live_custom_emoji()
            .any(|emoji| emoji.name == name.as_str())
        {
            let text = format!("There's already an emoji named :{name}:.");
            return self.fail_action(kind, text, out);
        }
        if self.live_custom_emoji().count() >= MAX_CUSTOM_EMOJI {
            let text = format!("A workspace can have at most {MAX_CUSTOM_EMOJI} custom emoji.");
            return self.fail_action(kind, text, out);
        }
        if let FileSource::Bytes(bytes) = &file.source
            && bytes.len() as u64 > MAX_EMOJI_BYTES
        {
            return self.fail_action(kind, image_too_large_text(bytes.len() as u64), out);
        }
        let Some(workspace) = self.primary_workspace().map(|info| info.workspace.id) else {
            return;
        };
        let adding = Adding {
            id: EmojiId::generate(),
            workspace,
            name,
            source: file.source,
            attempts: 0,
        };
        out.effects.push(upload_effect(&adding));
        self.emojis.adding = Some(adding);
        self.set_action(kind, ActionState::InFlight, out);
    }

    fn custom_emoji_uploaded(
        &mut self,
        id: EmojiId,
        result: Result<CustomEmojiResponse, TransferError>,
        out: &mut Output,
    ) {
        let kind = ActionKind::AddCustomEmoji;
        if self
            .emojis
            .adding
            .as_ref()
            .is_none_or(|adding| adding.id != id)
        {
            return;
        }
        match result {
            Ok(response) => {
                self.emojis.adding = None;
                self.store_custom_emoji(response.emoji, out);
                self.set_action(kind, ActionState::Idle, out);
            }
            Err(TransferError::Request(error)) if error.is_unauthenticated() => {
                self.emojis.adding = None;
                self.set_action(kind, ActionState::Idle, out);
                self.session_expired(out);
            }
            Err(TransferError::Request(RequestError::Unknown(_))) => {
                let Some(adding) = self.emojis.adding.as_mut() else {
                    return;
                };
                adding.attempts += 1;
                if adding.attempts < MAX_UPLOAD_ATTEMPTS {
                    // The same upload again: the server treats a repeated id as the same emoji.
                    out.effects.push(upload_effect(adding));
                } else {
                    self.emojis.adding = None;
                    let text = "Couldn't reach the server. Check your connection and try again.";
                    self.fail_action(kind, text, out);
                }
            }
            Err(error) => {
                self.emojis.adding = None;
                let text = match error {
                    TransferError::Request(error) => emoji_error(&error),
                    TransferError::TooLarge { size } => image_too_large_text(size),
                    TransferError::Unreadable(why) => {
                        format!("The image couldn't be read: {}.", why.trim_end_matches('.'))
                    }
                    _ => "The image couldn't be uploaded.".to_owned(),
                };
                self.fail_action(kind, text, out);
            }
        }
    }

    fn rename_custom_emoji(&mut self, id: EmojiId, name: &str, out: &mut Output) {
        let kind = ActionKind::RenameCustomEmoji(id);
        if !self.emoji_precondition(kind, out) {
            return;
        }
        let Some(held) = self.custom_emoji_by_id(id).filter(|emoji| !emoji.deleted) else {
            return self.fail_action(kind, "That emoji no longer exists.", out);
        };
        let name = match EmojiName::parse(name.trim().trim_matches(':')) {
            Ok(name) => name,
            Err(error) => return self.fail_action(kind, emoji_name_problem(&error), out),
        };
        if held.name == name.as_str() {
            return;
        }
        if self
            .live_custom_emoji()
            .any(|emoji| emoji.id != id && emoji.name == name.as_str())
        {
            let text = format!("There's already an emoji named :{name}:.");
            return self.fail_action(kind, text, out);
        }
        out.effects.push(Effect::RenameEmoji {
            emoji: id,
            request: RenameEmojiRequest { name },
        });
        self.set_action(kind, ActionState::InFlight, out);
    }

    fn delete_custom_emoji(&mut self, id: EmojiId, out: &mut Output) {
        let kind = ActionKind::DeleteCustomEmoji(id);
        if !self.emoji_precondition(kind, out) {
            return;
        }
        if !self.custom_emoji_is_live(id) {
            return;
        }
        out.effects.push(Effect::DeleteEmoji { emoji: id });
        self.set_action(kind, ActionState::InFlight, out);
    }

    /// The answer to a rename or a delete.
    pub(super) fn emoji_action_result(
        &mut self,
        kind: ActionKind,
        outcome: ActionOutcome,
        out: &mut Output,
    ) {
        match (kind, outcome) {
            (ActionKind::RenameCustomEmoji(_), ActionOutcome::CustomEmoji(emoji)) => {
                self.store_custom_emoji(emoji, out);
            }
            (ActionKind::DeleteCustomEmoji(id), _) => self.mark_emoji_deleted(id, out),
            _ => {}
        }
    }

    /// A delete went through, or the emoji is already gone.
    pub(super) fn mark_emoji_deleted(&mut self, id: EmojiId, out: &mut Output) {
        if let Some(held) = self.custom_emoji_by_id(id) {
            let mut deleted = held.clone();
            deleted.deleted = true;
            self.store_custom_emoji(deleted, out);
        }
    }

    /// Forgets a failed add, rename, or delete's error (when its dialog is closed) is the
    /// generic `DismissAction`; this ends an add that is in flight when the session ends.
    pub(super) fn abandon_emoji_add(&mut self) {
        self.emojis.adding = None;
    }
}

fn upload_effect(adding: &Adding) -> Effect {
    Effect::UploadEmoji {
        workspace: adding.workspace,
        emoji: adding.id,
        name: adding.name.clone(),
        source: adding.source.clone(),
    }
}

fn image_too_large_text(size: u64) -> String {
    format!(
        "That image is {} KiB; an emoji image can be at most {} KiB.",
        size.div_ceil(1024),
        MAX_EMOJI_BYTES / 1024
    )
}

/// Words for a person about why an emoji name was refused before asking.
fn emoji_name_problem(error: &NameError) -> String {
    match error {
        NameError::TooShort { .. } => "Give the emoji a name of at least 2 characters.".to_owned(),
        NameError::TooLong { .. } => "An emoji name can be at most 32 characters.".to_owned(),
        NameError::InvalidCharacter { .. } => {
            "An emoji name can only use lowercase letters, numbers, and underscores.".to_owned()
        }
        NameError::InvalidStart { .. } => {
            "An emoji name must start and end with a letter or number, and can't be the name of a \
             standard emoji."
                .to_owned()
        }
    }
}

/// Words for a person about why a request about a custom emoji failed.
pub(super) fn emoji_error(error: &RequestError) -> String {
    match error {
        RequestError::Rejected(api) => match api.code {
            ErrorCode::NameTaken => "There's already an emoji with that name.".to_owned(),
            ErrorCode::LimitReached => {
                format!("A workspace can have at most {MAX_CUSTOM_EMOJI} custom emoji.")
            }
            ErrorCode::UnsupportedMedia => {
                "That image can't be used. Use a PNG, JPEG, GIF, or WebP picture.".to_owned()
            }
            ErrorCode::PayloadTooLarge => format!(
                "That image is too large. An emoji image can be at most {} KiB.",
                MAX_EMOJI_BYTES / 1024
            ),
            ErrorCode::Forbidden => "Only admins and owners can manage custom emoji.".to_owned(),
            ErrorCode::NotFound => "That emoji no longer exists.".to_owned(),
            ErrorCode::Conflict => "That clashes with a change that was already made.".to_owned(),
            ErrorCode::RateLimited => "Too many requests. Wait a moment and try again.".to_owned(),
            ErrorCode::Unauthenticated => "Your session has expired. Sign in again.".to_owned(),
            ErrorCode::InvalidRequest
            | ErrorCode::InvalidCredentials
            | ErrorCode::StaleRevision
            | ErrorCode::UsernameTaken
            | ErrorCode::InvitationInvalid
            | ErrorCode::Internal
            | ErrorCode::Unavailable => {
                if api.message.is_empty() {
                    "Something went wrong. Try again.".to_owned()
                } else {
                    api.message.clone()
                }
            }
        },
        RequestError::Unknown(_) => {
            "Couldn't reach the server. Check your connection and try again.".to_owned()
        }
    }
}
