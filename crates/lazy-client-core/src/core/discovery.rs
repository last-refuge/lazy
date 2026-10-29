//! Saved messages, the people directory and profiles, and the channel directory (slice 3E, D21).
//!
//! Each list is loaded a page at a time and works like the thread inbox: requests have
//! generations that only go up, an answer is shown only if it answers the request made last, a
//! failure keeps what was loaded, and a snapshot that skipped the log asks for the first page
//! again. Saving a message is changed optimistically, like the settings: the marker flips at
//! once, one idempotent `PUT` carries the latest wish, an unknown outcome repeats it up to three
//! times, and a failure puts back what the server last said with a notice.

use std::collections::HashMap;

use lazy_domain::notification::UserStatus;
use lazy_domain::{ConversationId, MessageId, UserId, WorkspaceRole};
use lazy_protocol::{
    BootstrapResponse, BrowseChannelsQuery, BrowseChannelsResponse, BrowsedChannel, ErrorCode,
    Message, PeoplePage, PeopleQuery, PersonResponse, SavedPage, SavedQuery, SavedResponse,
    SetSavedRequest,
};

use crate::change::{Change, Output};
use crate::discovery::{
    BrowseView, DIRECTORY_PAGE, MAX_RECENT_CONVERSATIONS, PeopleView, ProfileView, SAVED_PAGE,
    SavedView,
};
use crate::effect::Effect;
use crate::input::Input;
use crate::model::{RequestError, SessionPhase};
use crate::notify::LoadStatus;

use super::ClientCore;
use super::notify::{Step, Synced, give_up_text, refusal_text};

/// The longest text a directory query may hold, in characters.
const MAX_QUERY_CHARS: usize = lazy_protocol::MAX_DIRECTORY_QUERY_CHARS;

/// The request a list waits for.
#[derive(Debug, Clone, Copy)]
pub(super) struct Flight {
    pub(super) generation: u64,
    /// The first page, replacing the list (otherwise the next one, appended).
    pub(super) first: bool,
}

/// The numbering of a list's requests and the one in flight.
#[derive(Debug, Default)]
pub(super) struct Pager {
    /// Only goes up, and survives closing the list.
    pub(super) last: u64,
    pub(super) flight: Option<Flight>,
    /// Something happened while a request was out: its answer may be out of date.
    pub(super) stale: bool,
}

impl Pager {
    /// Exchanges the request in flight (the counter is shared by all workspaces, so a late answer
    /// is never taken for another workspace's request).
    pub(super) fn swap_flight(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.flight, &mut other.flight);
        std::mem::swap(&mut self.stale, &mut other.stale);
    }

    pub(super) fn begin(&mut self, first: bool) -> u64 {
        self.last += 1;
        self.flight = Some(Flight {
            generation: self.last,
            first,
        });
        self.stale = false;
        self.last
    }

    /// The flight an answer belongs to, if it is the request made last.
    pub(super) fn accept(&mut self, generation: u64) -> Option<Flight> {
        if self
            .flight
            .is_some_and(|flight| flight.generation == generation)
        {
            self.flight.take()
        } else {
            None
        }
    }

    /// Forgets the request in flight; whether there was one.
    pub(super) fn cancel(&mut self) -> bool {
        self.stale = false;
        self.flight.take().is_some()
    }
}

/// What the core keeps about discovery.
#[derive(Debug, Default)]
pub(super) struct Discovery {
    /// The messages known to be saved, and changes not saved yet. A message that is not here is
    /// not saved.
    saved_ids: HashMap<MessageId, Synced<bool>>,
    saved: SavedView,
    saved_pager: Pager,
    people: PeopleView,
    people_pager: Pager,
    profile: Option<ProfileView>,
    profile_pager: Pager,
    browse: BrowseView,
    browse_pager: Pager,
    /// The conversations visited, most recent first.
    pub(super) recent: Vec<ConversationId>,
}

impl Discovery {
    /// Exchanges the lists and profile of one workspace and leaves the saved marks and the
    /// recently visited conversations, which are shared.
    pub(super) fn swap_scoped(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.saved, &mut other.saved);
        std::mem::swap(&mut self.people, &mut other.people);
        std::mem::swap(&mut self.profile, &mut other.profile);
        std::mem::swap(&mut self.browse, &mut other.browse);
        self.saved_pager.swap_flight(&mut other.saved_pager);
        self.people_pager.swap_flight(&mut other.people_pager);
        self.profile_pager.swap_flight(&mut other.profile_pager);
        self.browse_pager.swap_flight(&mut other.browse_pager);
    }
}

impl ClientCore {
    // ----- Reading -----

    /// Whether a message is saved, including a change that is not saved yet.
    pub fn is_saved(&self, message: MessageId) -> bool {
        self.discovery
            .saved_ids
            .get(&message)
            .is_some_and(|synced| *synced.shown())
    }

    /// The saved messages, once opened with `Input::OpenSaved`.
    pub fn saved(&self) -> &SavedView {
        &self.discovery.saved
    }

    /// The people directory, once opened with `Input::OpenPeople`.
    pub fn people(&self) -> &PeopleView {
        &self.discovery.people
    }

    /// The profile opened with `Input::OpenProfile`.
    pub fn profile(&self) -> Option<&ProfileView> {
        self.discovery.profile.as_ref()
    }

    /// The channel directory as last asked for with `Input::BrowseChannels` and the
    /// `SetBrowse…` inputs.
    pub fn browse(&self) -> &BrowseView {
        &self.discovery.browse
    }

    /// The conversations visited most recently, most recent first, at most
    /// [`MAX_RECENT_CONVERSATIONS`]. One the person is no longer in may still be listed; the quick
    /// switcher leaves it out.
    pub fn recent_conversations(&self) -> &[ConversationId] {
        &self.discovery.recent
    }

    fn discovery_ready(&self) -> bool {
        matches!(self.phase, SessionPhase::Ready)
    }

    // ----- Inputs -----

    pub(super) fn discovery_input(&mut self, input: Input, out: &mut Output) {
        match input {
            Input::SetMessageSaved { message_id, saved } => {
                self.set_message_saved(message_id, saved, out)
            }
            Input::MessageSavedResult { message_id, result } => {
                self.message_saved_result(message_id, result, out)
            }
            Input::OpenSaved => self.open_saved(out),
            Input::CloseSaved => self.close_saved(out),
            Input::LoadMoreSaved => self.load_more_saved(out),
            Input::SavedLoaded { generation, result } => self.saved_loaded(generation, result, out),
            Input::OpenPeople => self.open_people(out),
            Input::ClosePeople => self.close_people(out),
            Input::SetPeopleQuery(text) => self.set_people_query(text, out),
            Input::SetPeopleIncludeDeactivated(include) => {
                self.set_people_include_deactivated(include, out)
            }
            Input::LoadMorePeople => self.load_more_people(out),
            Input::PeopleLoaded { generation, result } => {
                self.people_loaded(generation, result, out)
            }
            Input::OpenProfile { user_id } => self.open_profile(user_id, out),
            Input::CloseProfile => self.close_profile(out),
            Input::ProfileLoaded { generation, result } => {
                self.profile_loaded(generation, result, out)
            }
            Input::BrowseChannels => self.browse_first_page(out),
            Input::SetBrowseQuery(text) => self.discovery_set_browse_query(text, out),
            Input::SetBrowseJoined(joined) => {
                self.discovery.browse.joined = joined;
                self.browse_first_page(out);
            }
            Input::SetBrowseSort(sort) => {
                self.discovery.browse.sort = sort;
                self.browse_first_page(out);
            }
            Input::LoadMoreBrowse => self.load_more_browse(out),
            Input::CloseBrowse => self.close_browse(out),
            Input::BrowseLoaded { generation, result } => {
                self.browse_loaded(generation, result, out)
            }
            _ => {}
        }
    }

    // ----- Saving a message -----

    fn set_message_saved(&mut self, message: MessageId, saved: bool, out: &mut Output) {
        if !self.discovery_ready() {
            return;
        }
        let entry = self
            .discovery
            .saved_ids
            .entry(message)
            .or_insert_with(|| Synced::new(false));
        let before = *entry.shown();
        let send = entry.want(saved);
        let after = *entry.shown();
        if let Some(saved) = send {
            out.effects.push(Effect::SetMessageSaved {
                message,
                request: SetSavedRequest { saved },
            });
        }
        self.saved_flipped(message, (before, after), true, false, out);
        self.tidy_saved(message);
    }

    fn message_saved_result(
        &mut self,
        message: MessageId,
        result: Result<SavedResponse, RequestError>,
        out: &mut Output,
    ) {
        let confirmed = matches!(&result, Ok(response) if response.saved);
        let Some(entry) = self.discovery.saved_ids.get_mut(&message) else {
            return;
        };
        let before = *entry.shown();
        let mut notice = None;
        let step = match result {
            Ok(response) => entry.answered(response.saved),
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(RequestError::Rejected(api)) => {
                if entry.refused() {
                    notice = Some(match api.code {
                        ErrorCode::NotFound => {
                            "That message can't be saved: it is gone, or you can't read it."
                                .to_owned()
                        }
                        _ => refusal_text(&api, "that message"),
                    });
                }
                Step::Done
            }
            Err(RequestError::Unknown(_)) => entry.lost(),
        };
        let after = *entry.shown();
        match step {
            Step::Send(saved) => out.effects.push(Effect::SetMessageSaved {
                message,
                request: SetSavedRequest { saved },
            }),
            Step::GiveUp => notice = Some(give_up_text("that message")),
            Step::Stale | Step::Done => {}
        }
        self.saved_flipped(message, (before, after), false, confirmed, out);
        self.tidy_saved(message);
        if notice.is_some() {
            self.set_notice(notice, out);
        }
    }

    /// The server says a message is saved or not (an event, or a page of the saved list).
    pub(super) fn event_saved(&mut self, message: MessageId, saved: bool, out: &mut Output) {
        let entry = self
            .discovery
            .saved_ids
            .entry(message)
            .or_insert_with(|| Synced::new(false));
        let before = *entry.shown();
        entry.server_says(saved);
        let after = *entry.shown();
        self.saved_flipped(message, (before, after), false, saved, out);
        self.tidy_saved(message);
    }

    /// The marker of a message changed (or was confirmed): keep the open list in step. A removal
    /// takes the item off at once. A message that is saved and not in the list sends the list for
    /// the first page again when the server says so (`confirmed`: an event, or the answer to the
    /// save) or when a removal is undone (not when the person has just asked to save it: the
    /// server has not heard yet).
    fn saved_flipped(
        &mut self,
        message: MessageId,
        flip: (bool, bool),
        local: bool,
        confirmed: bool,
        out: &mut Output,
    ) {
        let (before, after) = flip;
        if before != after {
            out.changes.push(Change::SavedChanged);
        }
        if !self.discovery.saved.open {
            return;
        }
        let listed = self
            .discovery
            .saved
            .items
            .iter()
            .any(|item| item.message.id == message);
        if !after {
            let items = &mut self.discovery.saved.items;
            let held = items.len();
            items.retain(|item| item.message.id != message);
            if items.len() != held {
                out.changes.push(Change::SavedChanged);
            }
        } else if !listed && !local && (confirmed || before != after) {
            self.saved_refresh(out);
        }
    }

    /// Forgets a message's record once it is settled and not saved.
    fn tidy_saved(&mut self, message: MessageId) {
        if self
            .discovery
            .saved_ids
            .get(&message)
            .is_some_and(|synced| synced.settled() && !*synced.shown())
        {
            self.discovery.saved_ids.remove(&message);
        }
    }

    /// Takes the saved ids out of a bootstrap. A change that is not saved yet stays on screen.
    pub(super) fn adopt_bootstrap_saved(&mut self, response: &BootstrapResponse, out: &mut Output) {
        let listed: std::collections::HashSet<MessageId> = response
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.saved.iter().copied())
            .collect();
        let mut changed = false;
        for (id, synced) in &mut self.discovery.saved_ids {
            let before = *synced.shown();
            synced.server_says(listed.contains(id));
            changed |= *synced.shown() != before;
        }
        for id in listed {
            self.discovery
                .saved_ids
                .entry(id)
                .or_insert_with(|| Synced::new(true));
        }
        self.discovery
            .saved_ids
            .retain(|_, synced| !(synced.settled() && !*synced.shown()));
        // Markers appear with the bootstrap, whether or not any was held before.
        let _ = changed;
        out.changes.push(Change::SavedChanged);
    }

    // ----- The saved list -----

    pub(super) fn open_saved(&mut self, out: &mut Output) {
        if !self.discovery_ready() {
            return;
        }
        if self.discovery.saved.open {
            // Opening a list that failed to load is asking again.
            if matches!(self.discovery.saved.status, LoadStatus::Failed { .. })
                && self.discovery.saved.items.is_empty()
            {
                self.start_saved_request(true, false, out);
            }
            return;
        }
        self.discovery.saved.open = true;
        self.start_saved_request(true, false, out);
    }

    pub(super) fn close_saved(&mut self, out: &mut Output) {
        if !self.discovery.saved.open {
            return;
        }
        if self.discovery.saved_pager.cancel() {
            out.effects.push(Effect::CancelSaved);
        }
        self.discovery.saved = SavedView::default();
        out.changes.push(Change::SavedChanged);
    }

    fn load_more_saved(&mut self, out: &mut Output) {
        let view = &self.discovery.saved;
        if !self.discovery_ready()
            || !view.open
            || !view.has_more
            || view.status == LoadStatus::Loading
            || view.items.is_empty()
        {
            return;
        }
        self.start_saved_request(false, false, out);
    }

    fn start_saved_request(&mut self, first: bool, quiet: bool, out: &mut Output) {
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let before = if first {
            None
        } else {
            self.discovery.saved.items.last().map(|item| item.cursor)
        };
        let generation = self.discovery.saved_pager.begin(first);
        if !quiet {
            let view = &mut self.discovery.saved;
            view.status = LoadStatus::Loading;
            if first {
                view.items.clear();
                view.has_more = false;
            }
        }
        out.effects.push(Effect::LoadSaved {
            workspace,
            generation,
            query: SavedQuery {
                before,
                limit: Some(SAVED_PAGE),
            },
        });
        out.changes.push(Change::SavedChanged);
    }

    /// Asks for the first page again without disturbing what is shown.
    fn saved_refresh(&mut self, out: &mut Output) {
        if !self.discovery.saved.open || self.discovery.saved.status == LoadStatus::Idle {
            return;
        }
        if self.discovery.saved_pager.flight.is_some() {
            self.discovery.saved_pager.stale = true;
        } else {
            self.start_saved_request(true, true, out);
        }
    }

    fn saved_loaded(
        &mut self,
        generation: u64,
        result: Result<SavedPage, RequestError>,
        out: &mut Output,
    ) {
        if !self.discovery.saved.open {
            return;
        }
        let Some(flight) = self.discovery.saved_pager.accept(generation) else {
            return;
        };
        match result {
            Ok(page) => {
                // What a page lists is saved: the markers follow, unless a removal is waiting.
                let mut items = Vec::with_capacity(page.items.len());
                for item in page.items {
                    let id = item.message.id;
                    let entry = self
                        .discovery
                        .saved_ids
                        .entry(id)
                        .or_insert_with(|| Synced::new(false));
                    entry.server_says(true);
                    if *entry.shown() {
                        items.push(item);
                    }
                }
                let view = &mut self.discovery.saved;
                if flight.first {
                    view.items = items;
                } else {
                    for item in items {
                        if !view
                            .items
                            .iter()
                            .any(|held| held.message.id == item.message.id)
                        {
                            view.items.push(item);
                        }
                    }
                }
                view.has_more = page.has_more;
                view.status = LoadStatus::Ready;
                out.changes.push(Change::SavedChanged);
                if self.discovery.saved_pager.stale {
                    self.start_saved_request(true, true, out);
                }
            }
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(error) => {
                let failure = directory_failure(&error);
                if failure.gone {
                    self.set_notice(Some(failure.reason.clone()), out);
                }
                self.discovery.saved.status = LoadStatus::Failed {
                    reason: failure.reason,
                    can_retry: failure.can_retry,
                };
                out.changes.push(Change::SavedChanged);
            }
        }
    }

    /// A message was deleted: it is no longer a saved item.
    pub(super) fn discovery_message_deleted(&mut self, message: &Message, out: &mut Output) {
        let view = &mut self.discovery.saved;
        let held = view.items.len();
        view.items.retain(|item| item.message.id != message.id);
        if view.items.len() != held {
            out.changes.push(Change::SavedChanged);
        }
        self.discovery.saved_pager.stale |= self.discovery.saved_pager.flight.is_some();
    }

    /// A message was edited: the saved item shows the new revision.
    pub(super) fn discovery_message_edited(&mut self, message: &Message, out: &mut Output) {
        let mut changed = false;
        for item in &mut self.discovery.saved.items {
            if item.message.id == message.id
                && item.message.revision < message.revision
                && !message.deleted
            {
                item.message = message.clone();
                changed = true;
            }
        }
        if changed {
            out.changes.push(Change::SavedChanged);
        }
    }

    /// The person no longer has a conversation: its saved items are not readable.
    pub(super) fn discovery_conversation_gone(
        &mut self,
        conversation: ConversationId,
        out: &mut Output,
    ) {
        let view = &mut self.discovery.saved;
        let held = view.items.len();
        view.items
            .retain(|item| item.conversation.id != conversation);
        if view.items.len() != held {
            out.changes.push(Change::SavedChanged);
        }
    }

    // ----- The people directory -----

    fn open_people(&mut self, out: &mut Output) {
        if !self.discovery_ready() {
            return;
        }
        if self.discovery.people.open {
            // Opening a directory that failed to load is asking again.
            if matches!(self.discovery.people.status, LoadStatus::Failed { .. })
                && self.discovery.people.people.is_empty()
            {
                self.people_first_page(out);
            }
            return;
        }
        self.discovery.people.open = true;
        self.people_first_page(out);
    }

    pub(super) fn close_people(&mut self, out: &mut Output) {
        if !self.discovery.people.open {
            return;
        }
        if self.discovery.people_pager.cancel() {
            out.effects.push(Effect::CancelPeople);
        }
        self.discovery.people = PeopleView::default();
        out.changes.push(Change::PeopleChanged);
    }

    fn set_people_query(&mut self, text: String, out: &mut Output) {
        if !self.discovery_ready() || !self.discovery.people.open {
            return;
        }
        if self.discovery.people.query == text {
            return;
        }
        self.discovery.people.query = text;
        self.people_first_page(out);
    }

    fn people_first_page(&mut self, out: &mut Output) {
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let text = self.discovery.people.query.trim().to_owned();
        if text.chars().count() > MAX_QUERY_CHARS {
            if self.discovery.people_pager.cancel() {
                out.effects.push(Effect::CancelPeople);
            }
            let view = &mut self.discovery.people;
            view.people.clear();
            view.has_more = false;
            view.status = too_long_status();
            out.changes.push(Change::PeopleChanged);
            return;
        }
        let generation = self.discovery.people_pager.begin(true);
        let view = &mut self.discovery.people;
        view.people.clear();
        view.has_more = false;
        view.status = LoadStatus::Loading;
        let query = self.people_query(text, None);
        out.effects.push(Effect::LoadPeople {
            workspace,
            generation,
            query,
        });
        out.changes.push(Change::PeopleChanged);
    }

    /// The request for a page of the directory as the view is set now.
    fn people_query(&self, text: String, offset: Option<u32>) -> PeopleQuery {
        PeopleQuery {
            query: (!text.is_empty()).then_some(text),
            include_deactivated: self.discovery.people.include_deactivated.then_some(true),
            offset,
            limit: Some(DIRECTORY_PAGE),
        }
    }

    fn load_more_people(&mut self, out: &mut Output) {
        let view = &self.discovery.people;
        if !self.discovery_ready()
            || !view.open
            || !view.has_more
            || view.status == LoadStatus::Loading
        {
            return;
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let offset = u32::try_from(view.people.len()).unwrap_or(u32::MAX);
        let text = view.query.trim().to_owned();
        let generation = self.discovery.people_pager.begin(false);
        self.discovery.people.status = LoadStatus::Loading;
        let query = self.people_query(text, Some(offset));
        out.effects.push(Effect::LoadPeople {
            workspace,
            generation,
            query,
        });
        out.changes.push(Change::PeopleChanged);
    }

    fn people_loaded(
        &mut self,
        generation: u64,
        result: Result<PeoplePage, RequestError>,
        out: &mut Output,
    ) {
        if !self.discovery.people.open {
            return;
        }
        let Some(flight) = self.discovery.people_pager.accept(generation) else {
            return;
        };
        // A resync happened while this was out: its answer may predate the gap (a role or a
        // deactivation among what it skipped).
        let again = std::mem::take(&mut self.discovery.people_pager.stale);
        match result {
            Ok(page) => {
                let view = &mut self.discovery.people;
                if flight.first {
                    view.people = page.people;
                } else {
                    for person in page.people {
                        if !view
                            .people
                            .iter()
                            .any(|held| held.user.id == person.user.id)
                        {
                            view.people.push(person);
                        }
                    }
                }
                view.has_more = page.has_more;
                view.status = LoadStatus::Ready;
            }
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(error) => {
                let failure = directory_failure(&error);
                if failure.gone {
                    self.set_notice(Some(failure.reason.clone()), out);
                }
                self.discovery.people.status = LoadStatus::Failed {
                    reason: failure.reason,
                    can_retry: failure.can_retry,
                };
            }
        }
        if again {
            self.quiet_people(out);
        }
        out.changes.push(Change::PeopleChanged);
    }

    /// Lists deactivated people too, or stops (admins and owners only; anyone else sees none).
    fn set_people_include_deactivated(&mut self, include: bool, out: &mut Output) {
        let include = include && self.can_manage_workspace();
        if !self.discovery_ready()
            || !self.discovery.people.open
            || self.discovery.people.include_deactivated == include
        {
            return;
        }
        self.discovery.people.include_deactivated = include;
        self.people_first_page(out);
    }

    /// The person's own role no longer allows asking for deactivated people.
    pub(super) fn people_demoted(&mut self, out: &mut Output) {
        if self.discovery.people.include_deactivated && !self.can_manage_workspace() {
            self.discovery.people.include_deactivated = false;
            if self.discovery.people.open && self.discovery.people.status != LoadStatus::Idle {
                self.people_first_page(out);
            } else {
                out.changes.push(Change::PeopleChanged);
            }
        }
    }

    /// Someone's role changed: the loaded copies of them show it.
    pub(super) fn directory_role_changed(
        &mut self,
        user: UserId,
        role: WorkspaceRole,
        out: &mut Output,
    ) {
        let mut people = false;
        for person in &mut self.discovery.people.people {
            if person.user.id == user && person.role != role {
                person.role = role;
                people = true;
            }
        }
        if people {
            out.changes.push(Change::PeopleChanged);
        }
        if let Some(person) = self
            .discovery
            .profile
            .as_mut()
            .and_then(|profile| profile.person.as_mut())
            && person.user.id == user
            && person.role != role
        {
            person.role = role;
            out.changes.push(Change::ProfileChanged);
        }
    }

    /// Someone was deactivated or reactivated: the loaded copies of them show it, and a directory
    /// that leaves out deactivated people drops them (or, on a reactivation, asks again, since the
    /// person is missing from it).
    pub(super) fn directory_deactivation_changed(
        &mut self,
        user: UserId,
        deactivated: bool,
        out: &mut Output,
    ) {
        let include = self.discovery.people.include_deactivated;
        let mut people = false;
        for person in &mut self.discovery.people.people {
            if person.user.id == user && person.user.deactivated != deactivated {
                person.user.deactivated = deactivated;
                people = true;
            }
        }
        if deactivated && !include {
            let before = self.discovery.people.people.len();
            self.discovery
                .people
                .people
                .retain(|person| person.user.id != user);
            people |= self.discovery.people.people.len() != before;
        }
        if people {
            out.changes.push(Change::PeopleChanged);
        }
        if !deactivated
            && !include
            && self.discovery.people.open
            && self.discovery.people.status == LoadStatus::Ready
            && !self
                .discovery
                .people
                .people
                .iter()
                .any(|person| person.user.id == user)
        {
            if self.discovery.people_pager.flight.is_some() {
                self.discovery.people_pager.stale = true;
            } else {
                self.quiet_people(out);
            }
        }
        if let Some(person) = self
            .discovery
            .profile
            .as_mut()
            .and_then(|profile| profile.person.as_mut())
            && person.user.id == user
            && person.user.deactivated != deactivated
        {
            person.user.deactivated = deactivated;
            out.changes.push(Change::ProfileChanged);
        }
    }

    // ----- Profiles -----

    fn open_profile(&mut self, user: UserId, out: &mut Output) {
        if !self.discovery_ready() {
            return;
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let known = self
            .discovery
            .people
            .people
            .iter()
            .find(|person| person.user.id == user)
            .cloned();
        let generation = self.discovery.profile_pager.begin(true);
        self.discovery.profile = Some(ProfileView {
            user_id: user,
            person: known,
            status: LoadStatus::Loading,
        });
        out.effects.push(Effect::LoadProfile {
            workspace,
            generation,
            user,
        });
        out.changes.push(Change::ProfileChanged);
    }

    pub(super) fn close_profile(&mut self, out: &mut Output) {
        if self.discovery.profile.take().is_none() {
            return;
        }
        if self.discovery.profile_pager.cancel() {
            out.effects.push(Effect::CancelProfile);
        }
        out.changes.push(Change::ProfileChanged);
    }

    fn profile_loaded(
        &mut self,
        generation: u64,
        result: Result<PersonResponse, RequestError>,
        out: &mut Output,
    ) {
        if self.discovery.profile.is_none() {
            return;
        }
        if self.discovery.profile_pager.accept(generation).is_none() {
            return;
        }
        // A resync happened while this was out: its answer may predate the gap.
        let again = std::mem::take(&mut self.discovery.profile_pager.stale);
        match result {
            Ok(response) => {
                if let Some(profile) = self.discovery.profile.as_mut()
                    && profile.user_id == response.person.user.id
                {
                    profile.person = Some(response.person);
                    profile.status = LoadStatus::Ready;
                }
            }
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(error) => {
                let (reason, can_retry) = match &error {
                    RequestError::Rejected(api) if api.code == ErrorCode::NotFound => {
                        ("That person isn't in this workspace.".to_owned(), false)
                    }
                    _ => {
                        let failure = directory_failure(&error);
                        (failure.reason, failure.can_retry)
                    }
                };
                if let Some(profile) = self.discovery.profile.as_mut() {
                    profile.status = LoadStatus::Failed { reason, can_retry };
                }
            }
        }
        if again {
            self.quiet_profile(out);
        }
        out.changes.push(Change::ProfileChanged);
    }

    /// Someone's status changed: the copies of them the directory and the profile hold follow.
    pub(super) fn discovery_status_changed(
        &mut self,
        user: UserId,
        status: &Option<UserStatus>,
        out: &mut Output,
    ) {
        let mut changed = false;
        for person in &mut self.discovery.people.people {
            if person.user.id == user && person.user.status != *status {
                person.user.status.clone_from(status);
                changed = true;
            }
        }
        if changed {
            out.changes.push(Change::PeopleChanged);
        }
        if let Some(profile) = self.discovery.profile.as_mut()
            && let Some(person) = profile.person.as_mut()
            && person.user.id == user
            && person.user.status != *status
        {
            person.user.status.clone_from(status);
            out.changes.push(Change::ProfileChanged);
        }
    }

    // ----- The channel directory -----

    /// Opens the channel directory with this text.
    pub(super) fn discovery_set_browse_query(&mut self, text: String, out: &mut Output) {
        self.discovery.browse.query = text;
        self.browse_first_page(out);
    }

    fn browse_first_page(&mut self, out: &mut Output) {
        if !self.discovery_ready() {
            return;
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let text = self.discovery.browse.query.trim().to_owned();
        if text.chars().count() > MAX_QUERY_CHARS {
            if self.discovery.browse_pager.cancel() {
                out.effects.push(Effect::CancelBrowse);
            }
            let view = &mut self.discovery.browse;
            view.channels.clear();
            view.has_more = false;
            view.status = too_long_status();
            out.changes.push(Change::BrowseChanged);
            return;
        }
        let generation = self.discovery.browse_pager.begin(true);
        let view = &mut self.discovery.browse;
        view.channels.clear();
        view.has_more = false;
        view.status = LoadStatus::Loading;
        let query = browse_query(view, &text, 0);
        out.effects.push(Effect::LoadBrowse {
            workspace,
            generation,
            query,
        });
        out.changes.push(Change::BrowseChanged);
    }

    fn load_more_browse(&mut self, out: &mut Output) {
        let view = &self.discovery.browse;
        if !self.discovery_ready()
            || !view.has_more
            || view.status == LoadStatus::Loading
            || view.channels.is_empty()
        {
            return;
        }
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let text = view.query.trim().to_owned();
        let offset = u32::try_from(view.channels.len()).unwrap_or(u32::MAX);
        let query = browse_query(view, &text, offset);
        let generation = self.discovery.browse_pager.begin(false);
        self.discovery.browse.status = LoadStatus::Loading;
        out.effects.push(Effect::LoadBrowse {
            workspace,
            generation,
            query,
        });
        out.changes.push(Change::BrowseChanged);
    }

    pub(super) fn close_browse(&mut self, out: &mut Output) {
        if self.discovery.browse_pager.cancel() {
            out.effects.push(Effect::CancelBrowse);
        }
        if self.discovery.browse != BrowseView::default() {
            self.discovery.browse = BrowseView::default();
            out.changes.push(Change::BrowseChanged);
        }
    }

    fn browse_loaded(
        &mut self,
        generation: u64,
        result: Result<BrowseChannelsResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(flight) = self.discovery.browse_pager.accept(generation) else {
            return;
        };
        match result {
            Ok(response) => {
                let joined: std::collections::HashSet<ConversationId> =
                    self.conversations.iter().map(|c| c.id).collect();
                let view = &mut self.discovery.browse;
                // What the person has joined since the request was made is already so.
                let mut channels: Vec<BrowsedChannel> = response.channels;
                for entry in &mut channels {
                    if joined.contains(&entry.conversation.id) {
                        entry.joined = true;
                    }
                }
                if flight.first {
                    view.channels = channels;
                } else {
                    for entry in channels {
                        if !view
                            .channels
                            .iter()
                            .any(|held| held.conversation.id == entry.conversation.id)
                        {
                            view.channels.push(entry);
                        }
                    }
                }
                view.has_more = response.has_more;
                view.status = LoadStatus::Ready;
            }
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(error) => {
                let failure = directory_failure(&error);
                if failure.gone {
                    self.set_notice(Some(failure.reason.clone()), out);
                }
                self.discovery.browse.status = LoadStatus::Failed {
                    reason: failure.reason,
                    can_retry: failure.can_retry,
                };
            }
        }
        out.changes.push(Change::BrowseChanged);
    }

    /// Keeps the loaded rows of the directory in step with what the client learns.
    pub(super) fn sync_browse(
        &mut self,
        conversation: &lazy_protocol::Conversation,
        joined: Option<bool>,
    ) {
        for entry in &mut self.discovery.browse.channels {
            if entry.conversation.id == conversation.id {
                entry.conversation = conversation.clone();
                if let Some(joined) = joined {
                    entry.joined = joined;
                }
            }
        }
    }

    pub(super) fn set_browse_joined(&mut self, id: ConversationId, joined: bool, out: &mut Output) {
        let mut changed = false;
        for entry in &mut self.discovery.browse.channels {
            if entry.conversation.id == id && entry.joined != joined {
                entry.joined = joined;
                changed = true;
            }
        }
        if changed {
            out.changes.push(Change::BrowseChanged);
        }
    }

    // ----- Resync, sign-out -----

    /// The log was skipped (a snapshot replaced it): what the lists show may be out of date. Ask
    /// again for the first page of each that has been loaded.
    pub(super) fn discovery_resynced(&mut self, out: &mut Output) {
        self.saved_refresh(out);
        if self.discovery.people.open && self.discovery.people.status != LoadStatus::Idle {
            if self.discovery.people_pager.flight.is_some() {
                self.discovery.people_pager.stale = true;
            } else {
                self.quiet_people(out);
            }
        }
        if self.discovery.browse.status != LoadStatus::Idle {
            if self.discovery.browse_pager.flight.is_some() {
                self.discovery.browse_pager.stale = true;
            } else {
                self.quiet_browse(out);
            }
        }
        // A profile shows a role and whether the person is deactivated, which events change.
        if self.discovery.profile.is_some() {
            if self.discovery.profile_pager.flight.is_some() {
                self.discovery.profile_pager.stale = true;
            } else {
                self.quiet_profile(out);
            }
        }
    }

    /// Asks for the profile on screen again, which keeps showing what it had until the answer.
    fn quiet_profile(&mut self, out: &mut Output) {
        let Some(user) = self
            .discovery
            .profile
            .as_ref()
            .map(|profile| profile.user_id)
        else {
            return;
        };
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let generation = self.discovery.profile_pager.begin(true);
        out.effects.push(Effect::LoadProfile {
            workspace,
            generation,
            user,
        });
    }

    fn quiet_people(&mut self, out: &mut Output) {
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let text = self.discovery.people.query.trim().to_owned();
        let generation = self.discovery.people_pager.begin(true);
        let query = self.people_query(text, None);
        out.effects.push(Effect::LoadPeople {
            workspace,
            generation,
            query,
        });
    }

    fn quiet_browse(&mut self, out: &mut Output) {
        let Some(workspace) = self.primary_workspace_id() else {
            return;
        };
        let text = self.discovery.browse.query.trim().to_owned();
        let query = browse_query(&self.discovery.browse, &text, 0);
        let generation = self.discovery.browse_pager.begin(true);
        out.effects.push(Effect::LoadBrowse {
            workspace,
            generation,
            query,
        });
    }

    /// The session ended: forget the lists, and stop what is out.
    pub(super) fn clear_discovery(&mut self, out: &mut Output) {
        for (cancelled, effect) in [
            (self.discovery.saved_pager.cancel(), Effect::CancelSaved),
            (self.discovery.people_pager.cancel(), Effect::CancelPeople),
            (self.discovery.profile_pager.cancel(), Effect::CancelProfile),
            (self.discovery.browse_pager.cancel(), Effect::CancelBrowse),
        ] {
            if cancelled {
                out.effects.push(effect);
            }
        }
        let (saved, people, profile, browse) = (
            self.discovery.saved_pager.last,
            self.discovery.people_pager.last,
            self.discovery.profile_pager.last,
            self.discovery.browse_pager.last,
        );
        self.discovery = Discovery::default();
        self.discovery.saved_pager.last = saved;
        self.discovery.people_pager.last = people;
        self.discovery.profile_pager.last = profile;
        self.discovery.browse_pager.last = browse;
        out.changes.push(Change::SavedChanged);
        out.changes.push(Change::PeopleChanged);
        out.changes.push(Change::ProfileChanged);
        out.changes.push(Change::BrowseChanged);
    }

    /// The person has no workspace any more: the lists and the changes waiting to be saved
    /// have nowhere to go. What they visited stays (it is kept for them, and means something
    /// again if they are let back in).
    pub(super) fn discovery_access_lost(&mut self, out: &mut Output) {
        let recent = std::mem::take(&mut self.discovery.recent);
        self.clear_discovery(out);
        self.discovery.recent = recent;
    }

    // ----- Recently visited conversations -----

    /// A conversation became the selected one.
    pub(super) fn note_visited(&mut self, conversation: ConversationId, out: &mut Output) {
        let recent = &mut self.discovery.recent;
        if recent.first() == Some(&conversation) {
            return;
        }
        recent.retain(|held| *held != conversation);
        recent.insert(0, conversation);
        recent.truncate(MAX_RECENT_CONVERSATIONS);
        out.effects
            .push(Effect::PersistRecentConversations(recent.clone()));
    }

    /// The recently visited conversations a previous run saved: repeats dropped, bounded.
    pub(super) fn restore_recent_conversations(&mut self, saved: Vec<ConversationId>) {
        let mut seen = std::collections::HashSet::new();
        self.discovery.recent = saved
            .into_iter()
            .filter(|id| seen.insert(*id))
            .take(MAX_RECENT_CONVERSATIONS)
            .collect();
    }
}

fn too_long_status() -> LoadStatus {
    LoadStatus::Failed {
        reason: format!("A search can be at most {MAX_QUERY_CHARS} characters."),
        can_retry: false,
    }
}

fn browse_query(view: &BrowseView, text: &str, offset: u32) -> BrowseChannelsQuery {
    BrowseChannelsQuery {
        query: (!text.is_empty()).then(|| text.to_owned()),
        joined: view.joined,
        include_archived: None,
        sort: Some(view.sort),
        offset: (offset > 0).then_some(offset),
        limit: Some(DIRECTORY_PAGE),
    }
}

struct Failure {
    reason: String,
    can_retry: bool,
    gone: bool,
}

/// Words for a person about why a directory request failed, whether asking again can help, and
/// whether the workspace is no longer theirs.
fn directory_failure(error: &RequestError) -> Failure {
    let (reason, can_retry, gone) = match error {
        RequestError::Unknown(_) => ("Couldn't reach the server.".to_owned(), true, false),
        RequestError::Rejected(api) => match api.code {
            ErrorCode::RateLimited => (
                "Too many requests. Wait a moment and try again.".to_owned(),
                true,
                false,
            ),
            ErrorCode::Forbidden | ErrorCode::NotFound => (
                "You can't open this in the workspace any more.".to_owned(),
                false,
                true,
            ),
            ErrorCode::InvalidRequest => (api.message.clone(), false, false),
            _ => ("Something went wrong.".to_owned(), true, false),
        },
    };
    Failure {
        reason,
        can_retry,
        gone,
    }
}
