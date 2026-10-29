//! Search (slice 3C, D19): the view, the requests, and what happens to hits when messages change.
//!
//! One search is in flight at a time and every request has a generation: an answer is shown only
//! if it answers the request made last, so a slow answer to a query the person has since changed
//! is never shown. Results are never saved; only the text of recent searches is.

use std::collections::{HashMap, HashSet};

use lazy_domain::search::{MAX_QUERY_CHARS, MAX_SEARCH_OFFSET, SearchQuery};
use lazy_domain::{ConversationKind, MessageId, Permalink};
use lazy_protocol::{ErrorCode, Message, SearchHit, SearchParams, SearchResponse, SearchSort};

use crate::change::{Change, Output};
use crate::effect::Effect;
use crate::input::Input;
use crate::model::{RequestError, SessionPhase};
use crate::search::{MAX_RECENT_SEARCHES, SEARCH_PAGE, SearchStatus, SearchView};

use super::ClientCore;

/// The request being waited for.
#[derive(Debug)]
struct Request {
    generation: u64,
    /// How many hits were already shown (0 for a first page).
    offset: usize,
    text: String,
    sort: SearchSort,
}

/// What the core keeps about search.
#[derive(Debug, Default)]
pub(super) struct Searching {
    view: Option<SearchView>,
    /// Most recent first, at most [`MAX_RECENT_SEARCHES`].
    recent: Vec<String>,
    /// The last generation used; only goes up, and survives closing the view, so an answer to a
    /// request of a closed view can never be taken for a later one.
    last_generation: u64,
    /// The text and sort the hits shown answer.
    answered: Option<(String, SearchSort)>,
    in_flight: Option<Request>,
    /// Messages deleted or edited while a request was in flight: the answer was read before the
    /// change and must not undo it.
    deleted_meanwhile: HashSet<MessageId>,
    edited_meanwhile: HashMap<MessageId, Message>,
}

impl Searching {
    /// Exchanges what belongs to one workspace (the view and its request) and leaves the recent
    /// searches and the generation counter, which are shared.
    pub(super) fn swap_scoped(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.view, &mut other.view);
        std::mem::swap(&mut self.answered, &mut other.answered);
        std::mem::swap(&mut self.in_flight, &mut other.in_flight);
        std::mem::swap(&mut self.deleted_meanwhile, &mut other.deleted_meanwhile);
        std::mem::swap(&mut self.edited_meanwhile, &mut other.edited_meanwhile);
    }
}

impl ClientCore {
    /// The search view, once opened with `Input::OpenSearch`.
    pub fn search(&self) -> Option<&SearchView> {
        self.searching.view.as_ref()
    }

    /// The last searches whose results the person opened, most recent first.
    pub fn recent_searches(&self) -> &[String] {
        &self.searching.recent
    }

    /// Where a hit is, for showing: `#design`, or the other people's names for a conversation.
    /// Works for a channel the person has not joined.
    pub fn search_hit_title(&self, hit: &SearchHit) -> String {
        let title = self.title_of(&hit.conversation);
        match hit.conversation.kind {
            ConversationKind::Channel => format!("#{title}"),
            _ => title,
        }
    }

    /// Whether the person is in the conversation of a hit (otherwise opening it offers to join).
    pub fn search_hit_joined(&self, hit: &SearchHit) -> bool {
        self.conversation(hit.conversation.id).is_some()
    }

    pub(super) fn search_input(&mut self, input: Input, out: &mut Output) {
        match input {
            Input::OpenSearch => self.open_search(out),
            Input::SetSearchQuery { text } => self.set_search_query(text, out),
            Input::RunSearch => self.run_search(out),
            Input::LoadMoreSearch => self.load_more_search(out),
            Input::SetSearchSort(sort) => self.set_search_sort(sort, out),
            Input::CloseSearch => self.close_search(out),
            Input::OpenSearchHit(index) => self.open_search_hit(index, out),
            Input::SearchResult { generation, result } => {
                self.search_result(generation, result, out)
            }
            _ => {}
        }
    }

    fn ensure_search_view(&mut self, out: &mut Output) -> &mut SearchView {
        if self.searching.view.is_none() {
            self.searching.view = Some(SearchView {
                query: String::new(),
                parsed: SearchQuery::parse(""),
                sort: SearchSort::default(),
                status: SearchStatus::Idle,
                hits: Vec::new(),
                has_more: false,
                truncated: false,
                at_limit: false,
                generation: 0,
            });
            out.changes.push(Change::SearchChanged);
        }
        self.searching
            .view
            .as_mut()
            .expect("the view was just made")
    }

    pub(super) fn open_search(&mut self, out: &mut Output) {
        if matches!(self.phase, SessionPhase::Ready) {
            self.ensure_search_view(out);
        }
    }

    fn set_search_query(&mut self, text: String, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let view = self.ensure_search_view(out);
        if view.query == text {
            return;
        }
        view.parsed = SearchQuery::parse(&text);
        view.query = text;
        out.changes.push(Change::SearchChanged);
    }

    fn run_search(&mut self, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let Some(view) = self.searching.view.as_ref() else {
            return;
        };
        if view.parsed.is_err() {
            return;
        }
        let (text, sort) = (view.query.trim().to_owned(), view.sort);
        self.start_search_request(text, sort, 0, out);
    }

    fn load_more_search(&mut self, out: &mut Output) {
        let Some(view) = self.searching.view.as_ref() else {
            return;
        };
        if !matches!(self.phase, SessionPhase::Ready)
            || !view.has_more
            || view.at_limit
            || view.status == SearchStatus::Loading
        {
            return;
        }
        let Some((text, sort)) = self.searching.answered.clone() else {
            return;
        };
        let offset = view.hits.len();
        self.start_search_request(text, sort, offset, out);
    }

    fn set_search_sort(&mut self, sort: SearchSort, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        let view = self.ensure_search_view(out);
        if view.sort == sort {
            return;
        }
        view.sort = sort;
        out.changes.push(Change::SearchChanged);
        self.run_search(out);
    }

    pub(super) fn close_search(&mut self, out: &mut Output) {
        if self.searching.view.take().is_none() {
            return;
        }
        self.searching.answered = None;
        self.drop_search_request(out);
        out.changes.push(Change::SearchChanged);
    }

    /// Forgets the request in flight, telling the runtime nothing waits for it.
    fn drop_search_request(&mut self, out: &mut Output) {
        if self.searching.in_flight.take().is_some() {
            out.effects.push(Effect::CancelSearch);
        }
        self.searching.deleted_meanwhile.clear();
        self.searching.edited_meanwhile.clear();
    }

    fn start_search_request(
        &mut self,
        text: String,
        sort: SearchSort,
        offset: usize,
        out: &mut Output,
    ) {
        let Some(workspace) = self.primary_workspace().map(|info| info.workspace.id) else {
            return;
        };
        self.searching.last_generation += 1;
        let generation = self.searching.last_generation;
        self.searching.deleted_meanwhile.clear();
        self.searching.edited_meanwhile.clear();
        // A first page drops what was shown: it answered another query.
        let first = offset == 0;
        if first {
            self.searching.answered = None;
        }
        if let Some(view) = self.searching.view.as_mut() {
            view.generation = generation;
            view.status = SearchStatus::Loading;
            if first {
                view.hits.clear();
                view.has_more = false;
                view.truncated = false;
                view.at_limit = false;
            }
        }
        out.effects.push(Effect::Search {
            workspace,
            generation,
            params: SearchParams {
                q: text.clone(),
                limit: Some(SEARCH_PAGE),
                offset: Some(offset as u32),
                sort: Some(sort),
            },
        });
        self.searching.in_flight = Some(Request {
            generation,
            offset,
            text,
            sort,
        });
        out.changes.push(Change::SearchChanged);
    }

    fn search_result(
        &mut self,
        generation: u64,
        result: Result<SearchResponse, RequestError>,
        out: &mut Output,
    ) {
        // Only the answer to the request made last is shown.
        if self
            .searching
            .in_flight
            .as_ref()
            .is_none_or(|request| request.generation != generation)
            || self.searching.view.is_none()
        {
            return;
        }
        let Some(request) = self.searching.in_flight.take() else {
            return;
        };
        match result {
            Ok(response) => self.apply_search_page(request, response),
            Err(error) if error.is_unauthenticated() => self.session_expired(out),
            Err(error) => {
                let (reason, can_retry, forgotten) = search_failure(&error);
                if forgotten {
                    let notice = "You can't search this workspace any more.";
                    self.set_notice(Some(notice.to_owned()), out);
                }
                self.search_failed(reason, can_retry);
            }
        }
        self.searching.deleted_meanwhile.clear();
        self.searching.edited_meanwhile.clear();
        out.changes.push(Change::SearchChanged);
    }

    fn search_failed(&mut self, reason: String, can_retry: bool) {
        if let Some(view) = self.searching.view.as_mut() {
            view.status = SearchStatus::Failed { reason, can_retry };
        }
    }

    fn apply_search_page(&mut self, request: Request, response: SearchResponse) {
        let mut hits = response.hits;
        // What changed while the request was out was read after the answer was.
        hits.retain(|hit| !self.searching.deleted_meanwhile.contains(&hit.message.id));
        for hit in &mut hits {
            if let Some(newer) = self.searching.edited_meanwhile.get(&hit.message.id)
                && newer.revision > hit.message.revision
            {
                hit.message = newer.clone();
            }
        }
        let Some(view) = self.searching.view.as_mut() else {
            return;
        };
        if request.offset == 0 {
            view.hits = hits;
        } else {
            for hit in hits {
                // The results shift when messages come and go: never show a hit twice.
                let known = view
                    .hits
                    .iter()
                    .any(|held| held.message.id == hit.message.id && held.kind == hit.kind);
                if !known {
                    view.hits.push(hit);
                }
            }
        }
        view.has_more = response.has_more;
        view.truncated = response.truncated;
        view.at_limit = response.has_more && view.hits.len() >= MAX_SEARCH_OFFSET;
        view.status = SearchStatus::Ready;
        self.searching.answered = Some((request.text, request.sort));
    }

    fn open_search_hit(&mut self, index: usize, out: &mut Output) {
        let Some(hit) = self
            .searching
            .view
            .as_ref()
            .and_then(|view| view.hits.get(index))
            .cloned()
        else {
            return;
        };
        if let Some((text, _)) = self.searching.answered.clone() {
            self.record_recent_search(&text, out);
        }
        self.open_permalink(
            Permalink {
                workspace_id: hit.conversation.workspace_id,
                conversation_id: hit.conversation.id,
                message_id: hit.message.id,
            },
            out,
        );
    }

    /// Remembers a search whose result the person opened.
    fn record_recent_search(&mut self, text: &str, out: &mut Output) {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > MAX_QUERY_CHARS {
            return;
        }
        let recent = &mut self.searching.recent;
        let lowered = text.to_lowercase();
        recent.retain(|held| held.to_lowercase() != lowered);
        recent.insert(0, text.to_owned());
        recent.truncate(MAX_RECENT_SEARCHES);
        out.changes.push(Change::RecentSearchesChanged);
        out.effects
            .push(Effect::PersistRecentSearches(self.searching.recent.clone()));
    }

    /// The recent searches a previous run saved: blanks and repeats dropped, bounded.
    pub(super) fn restore_recent_searches(&mut self, saved: Vec<String>) {
        let mut seen = HashSet::new();
        self.searching.recent = saved
            .into_iter()
            .map(|text| text.trim().to_owned())
            .filter(|text| {
                !text.is_empty()
                    && text.chars().count() <= MAX_QUERY_CHARS
                    && seen.insert(text.to_lowercase())
            })
            .take(MAX_RECENT_SEARCHES)
            .collect();
    }

    /// The log was skipped (a snapshot replaced it): deletions and edits may have been missed, so
    /// what is shown may be out of date, and an answer already on its way may predate them. Ask
    /// again for the first page of the run the view shows or waits for.
    pub(super) fn search_resynced(&mut self, out: &mut Output) {
        if self.searching.view.is_none() {
            return;
        }
        let (text, sort) = match (&self.searching.in_flight, &self.searching.answered) {
            (Some(request), _) => (request.text.clone(), request.sort),
            (None, Some(answered)) => answered.clone(),
            (None, None) => return,
        };
        self.start_search_request(text, sort, 0, out);
    }

    /// A message was deleted (an event, or the answer to the person's own delete): it is no
    /// longer a result.
    pub(super) fn search_message_deleted(&mut self, message: MessageId, out: &mut Output) {
        let Some(view) = self.searching.view.as_mut() else {
            return;
        };
        if self.searching.in_flight.is_some() {
            self.searching.deleted_meanwhile.insert(message);
        }
        let before = view.hits.len();
        view.hits.retain(|hit| hit.message.id != message);
        if view.hits.len() != before {
            out.changes.push(Change::SearchChanged);
        }
    }

    /// A message was edited: the hit shows its new text. The snippet is the server's, from when
    /// it searched, and stays until the next run.
    pub(super) fn search_message_edited(&mut self, message: &Message, out: &mut Output) {
        let Some(view) = self.searching.view.as_mut() else {
            return;
        };
        if self.searching.in_flight.is_some() {
            let newer = self
                .searching
                .edited_meanwhile
                .get(&message.id)
                .is_none_or(|held| held.revision < message.revision);
            if newer {
                self.searching
                    .edited_meanwhile
                    .insert(message.id, message.clone());
            }
        }
        let mut changed = false;
        for hit in &mut view.hits {
            if hit.message.id == message.id && hit.message.revision < message.revision {
                hit.message = message.clone();
                changed = true;
            }
        }
        if changed {
            out.changes.push(Change::SearchChanged);
        }
    }

    /// Forgets everything about search (sign-out). The recent searches go too.
    pub(super) fn clear_search(&mut self, out: &mut Output) {
        self.drop_search_request(out);
        let generation = self.searching.last_generation;
        self.searching = Searching::default();
        self.searching.last_generation = generation;
    }
}

/// Words for a person about why a search failed, whether asking again can help, and whether the
/// workspace is no longer theirs to search.
fn search_failure(error: &RequestError) -> (String, bool, bool) {
    match error {
        RequestError::Unknown(_) => ("Couldn't reach the server.".to_owned(), true, false),
        RequestError::Rejected(api) => match api.code {
            ErrorCode::InvalidRequest => (api.message.clone(), false, false),
            ErrorCode::Unavailable => (
                "That search took too long. Narrow it with in: or from:".to_owned(),
                true,
                false,
            ),
            ErrorCode::RateLimited => (
                "Too many searches. Wait a moment and try again.".to_owned(),
                true,
                false,
            ),
            ErrorCode::Forbidden | ErrorCode::NotFound => (
                "You can't search this workspace any more.".to_owned(),
                false,
                true,
            ),
            _ => (
                if api.message.is_empty() {
                    "Something went wrong with that search.".to_owned()
                } else {
                    api.message.clone()
                },
                true,
                false,
            ),
        },
    }
}
