//! Search (slice 3C, D19): what the search view shows.
//!
//! The core parses the query with the same rules as the server (`SearchQuery::parse`), so the
//! screen can show the filters as chips and say what is wrong without a request. The person types,
//! the screen waits a moment and asks the core to run the search; the core decides nothing about
//! timing. An answer is shown only if it answers the request made last: a newer request makes
//! every older answer irrelevant.

use lazy_domain::search::{SearchError, SearchQuery};
use lazy_protocol::{SearchHit, SearchSort};

/// How many results a request asks for.
pub const SEARCH_PAGE: u32 = 20;
/// Most recent searches kept.
pub const MAX_RECENT_SEARCHES: usize = 10;

/// Where a search stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchStatus {
    /// Nothing has been asked since the view opened.
    Idle,
    /// A request is in flight: the first page (the hits are empty) or the next one (they are the
    /// ones so far).
    Loading,
    /// The last request was answered.
    Ready,
    /// The last request failed. For a next page the hits so far are kept and `can_retry` says
    /// whether `Input::LoadMoreSearch` (or `Input::RunSearch` for a first page) can try again.
    Failed { reason: String, can_retry: bool },
}

/// The search view, as the screen reads it with `ClientCore::search`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchView {
    /// The text in the search field.
    pub query: String,
    /// The text parsed with the server's rules: what the filters and words are, or why it cannot
    /// be used. Nothing is requested for a query that does not parse.
    pub parsed: Result<SearchQuery, SearchError>,
    pub sort: SearchSort,
    pub status: SearchStatus,
    /// The results of the last run, best first. They answer the text of that run, which is
    /// `query` unless the person has typed since. Empty while the first page is loading.
    pub hits: Vec<SearchHit>,
    /// Whether more results can be asked for.
    pub has_more: bool,
    /// Whether the query matched more than the server ranks (older matches may be missing).
    pub truncated: bool,
    /// Whether the results run out only because a query pages no further than 200 results: the
    /// person should narrow the search.
    pub at_limit: bool,
    /// The request this view is waiting for or last heard about, numbered upward over the life of
    /// the session; 0 until the view has asked for something.
    pub generation: u64,
}
