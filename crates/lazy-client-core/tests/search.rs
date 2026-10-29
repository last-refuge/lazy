//! Search (slice 3C, D19): the view, the requests, paging, errors, opening a hit, what happens to
//! hits when messages change, and the recent searches.

mod common;

use common::{Driver, World, channel, people, seq, user};
use lazy_client_core::{
    Change, Effect, Input, MAX_RECENT_SEARCHES, Output, RequestError, SEARCH_PAGE, SearchStatus,
    SessionPhase, StartupState,
};
use lazy_domain::search::{MAX_QUERY_CHARS, MAX_SEARCH_OFFSET, SearchError};
use lazy_domain::{AttachmentId, ConversationId, MessageId};
use lazy_protocol::{
    ApiError, Conversation, ErrorCode, Message, MessageResponse, SearchHit, SearchHitKind,
    SearchParams, SearchResponse, SearchSort, Snippet, SnippetRange,
};

// ----- Helpers -----

struct Fx {
    world: World,
    driver: Driver,
    general: ConversationId,
}

fn setup() -> Fx {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(general, vec![], false);
    Fx {
        world,
        driver,
        general,
    }
}

/// A hit on `message` whose snippet is `word` with the word highlighted.
fn hit_in(message: &Message, conversation: &Conversation, word: &str) -> SearchHit {
    SearchHit {
        message: message.clone(),
        conversation: conversation.clone(),
        kind: SearchHitKind::Message,
        snippet: Snippet {
            text: format!("… {word} …"),
            highlights: vec![SnippetRange {
                start: 2,
                end: 2 + u32::try_from(word.chars().count()).unwrap(),
            }],
        },
    }
}

impl Fx {
    /// A message by someone else in `#general`, as a hit on `word`.
    fn general_hit(&self, at: i64, word: &str) -> SearchHit {
        let message =
            self.world
                .message(self.general, &self.world.other, at, &format!("{word} {at}"));
        hit_in(&message, &self.world.channel, word)
    }

    fn hits(&self, first: i64, count: i64, word: &str) -> Vec<SearchHit> {
        (first..first + count)
            .map(|at| self.general_hit(at, word))
            .collect()
    }

    fn type_text(&mut self, text: &str) -> Output {
        self.driver
            .send(Input::SetSearchQuery { text: text.into() })
    }

    /// Types `text` and runs it. Returns the request made, if any.
    fn run(&mut self, text: &str) -> Option<(u64, SearchParams)> {
        self.type_text(text);
        let out = self.driver.send(Input::RunSearch);
        searches(&out).pop()
    }

    fn answer(&mut self, generation: u64, hits: Vec<SearchHit>, has_more: bool) {
        self.driver.send(Input::SearchResult {
            generation,
            result: Ok(SearchResponse {
                hits,
                has_more,
                truncated: false,
            }),
        });
    }

    fn fail(&mut self, generation: u64, error: RequestError) {
        self.driver.send(Input::SearchResult {
            generation,
            result: Err(error),
        });
    }

    fn view(&self) -> &lazy_client_core::SearchView {
        self.driver.core.search().expect("the search is open")
    }

    fn texts(&self) -> Vec<String> {
        self.view()
            .hits
            .iter()
            .map(|hit| hit.message.document.plain_text())
            .collect()
    }

    /// An event at the next position.
    fn next_seq(&mut self) -> i64 {
        self.world.head = seq(self.world.head.get() + 1);
        self.world.head.get()
    }
}

fn searches(out: &Output) -> Vec<(u64, SearchParams)> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Search {
                generation, params, ..
            } => Some((*generation, params.clone())),
            _ => None,
        })
        .collect()
}

fn all_searches(driver: &Driver) -> Vec<(u64, SearchParams)> {
    driver.effects_matching(|effect| match effect {
        Effect::Search {
            generation, params, ..
        } => Some((*generation, params.clone())),
        _ => None,
    })
}

fn cancels(driver: &Driver) -> usize {
    driver
        .effects_matching(|effect| matches!(effect, Effect::CancelSearch).then_some(()))
        .len()
}

fn rejected(code: ErrorCode, message: &str) -> RequestError {
    RequestError::Rejected(ApiError::new(code, message))
}

fn lost() -> RequestError {
    RequestError::Unknown("timed out".into())
}

fn saved_searches(driver: &Driver) -> Vec<Vec<String>> {
    driver.effects_matching(|effect| match effect {
        Effect::PersistRecentSearches(list) => Some(list.clone()),
        _ => None,
    })
}

fn failure(fx: &Fx) -> (String, bool) {
    match &fx.view().status {
        SearchStatus::Failed { reason, can_retry } => (reason.clone(), *can_retry),
        other => panic!("expected a failure, found {other:?}"),
    }
}

// ----- The view and what is asked -----

#[test]
fn search_opens_idle_and_closing_clears_it() {
    let mut fx = setup();
    assert!(fx.driver.core.search().is_none());
    let out = fx.driver.send(Input::OpenSearch);
    assert!(out.changes.contains(&Change::SearchChanged));
    assert!(out.effects.is_empty());
    let view = fx.view();
    assert_eq!(view.status, SearchStatus::Idle);
    assert!(view.hits.is_empty() && view.query.is_empty());
    assert_eq!(view.sort, SearchSort::Relevance);
    assert_eq!(view.parsed, Err(SearchError::Empty));

    // Opening it again changes nothing.
    let out = fx.driver.send(Input::OpenSearch);
    assert!(out.changes.is_empty());

    let out = fx.driver.send(Input::CloseSearch);
    assert!(out.changes.contains(&Change::SearchChanged));
    assert!(fx.driver.core.search().is_none());
    assert!(fx.driver.send(Input::CloseSearch).changes.is_empty());
}

#[test]
fn typing_records_and_parses_the_text_but_asks_for_nothing() {
    let mut fx = setup();
    // Typing opens the view by itself.
    let out = fx.type_text("deploy in:ops from:jules");
    assert!(out.effects.is_empty(), "the screen decides when to ask");
    assert!(out.changes.contains(&Change::SearchChanged));
    let view = fx.view();
    assert_eq!(view.query, "deploy in:ops from:jules");
    let parsed = view.parsed.as_ref().unwrap();
    assert_eq!(parsed.words, vec!["deploy"]);
    assert_eq!(parsed.channels, vec!["ops"]);
    assert_eq!(parsed.from, vec!["jules"]);
    assert_eq!(view.status, SearchStatus::Idle);

    // The same text again is no change.
    assert!(fx.type_text("deploy in:ops from:jules").changes.is_empty());
}

#[test]
fn a_run_asks_for_the_first_page_of_the_trimmed_text_in_the_primary_workspace() {
    let mut fx = setup();
    let out = {
        fx.type_text("  deplo  ");
        fx.driver.send(Input::RunSearch)
    };
    let workspace = fx.world.workspace.id;
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::Search { workspace: w, .. } if *w == workspace
    )));
    let (generation, params) = searches(&out).pop().unwrap();
    assert_eq!(
        params,
        SearchParams {
            q: "deplo".into(),
            limit: Some(SEARCH_PAGE),
            offset: Some(0),
            sort: Some(SearchSort::Relevance),
        }
    );
    let view = fx.view();
    assert_eq!(view.status, SearchStatus::Loading);
    assert_eq!(view.generation, generation);
    assert!(view.hits.is_empty());
}

#[test]
fn a_query_that_cannot_be_used_asks_for_nothing_and_says_why() {
    let mut fx = setup();
    let too_long = "a".repeat(MAX_QUERY_CHARS + 1);
    let many = (0..20)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    for (text, expected) in [
        ("", SearchError::Empty),
        ("   ", SearchError::Empty),
        (
            "after:2026-13-45",
            SearchError::BadDate("2026-13-45".into()),
        ),
        ("has:video", SearchError::BadHas("video".into())),
        (too_long.as_str(), SearchError::TooLong),
        (many.as_str(), SearchError::TooManyTerms),
    ] {
        fx.type_text(text);
        let out = fx.driver.send(Input::RunSearch);
        assert!(out.effects.is_empty(), "{text:?} asked for something");
        assert_eq!(fx.view().parsed, Err(expected), "{text:?}");
        assert_eq!(fx.view().status, SearchStatus::Idle);
    }
    assert!(all_searches(&fx.driver).is_empty());
}

#[test]
fn a_run_without_an_open_view_or_before_sign_in_does_nothing() {
    let mut fx = setup();
    assert!(
        fx.driver.send(Input::RunSearch).effects.is_empty(),
        "no view"
    );
    assert!(fx.driver.send(Input::LoadMoreSearch).effects.is_empty());

    let mut signed_out = Driver::new();
    for input in [
        Input::OpenSearch,
        Input::SetSearchQuery {
            text: "hello".into(),
        },
        Input::RunSearch,
        Input::LoadMoreSearch,
        Input::SetSearchSort(SearchSort::Recent),
    ] {
        let out = signed_out.send(input);
        assert!(out.effects.is_empty() && out.changes.is_empty());
    }
    assert!(signed_out.core.search().is_none());
    assert_ne!(signed_out.core.phase(), &SessionPhase::Ready);
}

#[test]
fn the_answer_to_a_run_replaces_the_hits_and_says_whether_there_are_more() {
    let mut fx = setup();
    let (generation, _) = fx.run("hello").unwrap();
    let hits = fx.hits(20, 3, "hello");
    let out = fx.driver.send(Input::SearchResult {
        generation,
        result: Ok(SearchResponse {
            hits: hits.clone(),
            has_more: true,
            truncated: true,
        }),
    });
    assert!(out.changes.contains(&Change::SearchChanged));
    let view = fx.view();
    assert_eq!(view.status, SearchStatus::Ready);
    assert_eq!(view.hits, hits);
    assert!(view.has_more && view.truncated && !view.at_limit);
}

#[test]
fn running_again_drops_the_hits_of_the_earlier_run() {
    let mut fx = setup();
    let (first, _) = fx.run("hello").unwrap();
    fx.answer(first, fx.hits(20, 2, "hello"), false);
    assert_eq!(fx.view().hits.len(), 2);

    // The hits always answer the last run: the old ones are gone as soon as a new one starts,
    // even though the text in the box was changed in between.
    fx.type_text("goodbye");
    assert_eq!(fx.view().hits.len(), 2, "typing alone keeps them");
    let (second, params) = fx
        .driver
        .send(Input::RunSearch)
        .effects
        .iter()
        .fold(None, |found, effect| match effect {
            Effect::Search {
                generation, params, ..
            } => Some((*generation, params.clone())),
            _ => found,
        })
        .unwrap();
    assert!(second > first);
    assert_eq!(params.q, "goodbye");
    assert!(fx.view().hits.is_empty());
    assert_eq!(fx.view().status, SearchStatus::Loading);
}

// ----- Superseded and late answers -----

#[test]
fn a_late_answer_to_an_older_run_is_not_shown() {
    let mut fx = setup();
    let (old, _) = fx.run("hello").unwrap();
    let (new, _) = fx.run("goodbye").unwrap();
    assert!(new > old);

    // The old one arrives first: nothing changes.
    let out = fx.driver.send(Input::SearchResult {
        generation: old,
        result: Ok(SearchResponse {
            hits: fx.hits(20, 2, "hello"),
            has_more: true,
            truncated: false,
        }),
    });
    assert!(out.changes.is_empty());
    assert!(fx.view().hits.is_empty());
    assert_eq!(fx.view().status, SearchStatus::Loading);

    // Neither does a late failure of it.
    fx.fail(old, lost());
    assert_eq!(fx.view().status, SearchStatus::Loading);

    fx.answer(new, fx.hits(30, 1, "goodbye"), false);
    assert_eq!(fx.texts(), vec!["goodbye 30"]);

    // And once the newer one has been answered, the old answer still changes nothing.
    fx.answer(old, fx.hits(40, 5, "hello"), false);
    assert_eq!(fx.texts(), vec!["goodbye 30"]);
}

#[test]
fn an_answer_is_dropped_after_the_view_is_closed_even_if_it_is_opened_again() {
    let mut fx = setup();
    let (old, _) = fx.run("hello").unwrap();
    fx.driver.send(Input::CloseSearch);
    assert_eq!(
        cancels(&fx.driver),
        1,
        "the runtime is told to stop waiting"
    );

    let out = fx.driver.send(Input::SearchResult {
        generation: old,
        result: Ok(SearchResponse {
            hits: fx.hits(20, 2, "hello"),
            has_more: false,
            truncated: false,
        }),
    });
    assert!(out.changes.is_empty());
    assert!(fx.driver.core.search().is_none());

    // A view opened since is not answered by a request of the closed one, and its own
    // requests are numbered after it.
    fx.driver.send(Input::OpenSearch);
    fx.answer(old, fx.hits(20, 2, "hello"), false);
    assert!(fx.view().hits.is_empty());
    let (next, _) = fx.run("hello").unwrap();
    assert!(next > old, "generations are never reused");
}

#[test]
fn closing_an_idle_or_answered_view_cancels_nothing() {
    let mut fx = setup();
    fx.driver.send(Input::OpenSearch);
    fx.driver.send(Input::CloseSearch);
    let (generation, _) = fx.run("hello").unwrap();
    fx.answer(generation, fx.hits(20, 1, "hello"), false);
    fx.driver.send(Input::CloseSearch);
    assert_eq!(cancels(&fx.driver), 0);
}

// ----- Paging -----

#[test]
fn more_results_are_asked_from_where_the_shown_ones_end_with_the_text_that_was_answered() {
    let mut fx = setup();
    let (first, _) = fx.run("hello").unwrap();
    fx.answer(first, fx.hits(100, 20, "hello"), true);

    // The person types on; the next page is still the run's.
    fx.type_text("hello wor");
    let out = fx.driver.send(Input::LoadMoreSearch);
    let (second, params) = searches(&out).pop().unwrap();
    assert!(second > first);
    assert_eq!(
        params,
        SearchParams {
            q: "hello".into(),
            limit: Some(SEARCH_PAGE),
            offset: Some(20),
            sort: Some(SearchSort::Relevance),
        }
    );
    // While it loads, the hits stay and a second request is not made.
    assert_eq!(fx.view().status, SearchStatus::Loading);
    assert_eq!(fx.view().hits.len(), 20);
    assert!(fx.driver.send(Input::LoadMoreSearch).effects.is_empty());

    fx.answer(second, fx.hits(200, 5, "hello"), false);
    let view = fx.view();
    assert_eq!(view.hits.len(), 25);
    assert_eq!(view.status, SearchStatus::Ready);
    assert!(!view.has_more);
    assert_eq!(view.hits[20].message.document.plain_text(), "hello 200");
    // Nothing more to ask.
    assert!(fx.driver.send(Input::LoadMoreSearch).effects.is_empty());
}

#[test]
fn a_hit_that_comes_again_on_a_later_page_is_shown_once() {
    let mut fx = setup();
    let (first, _) = fx.run("hello").unwrap();
    let page = fx.hits(100, 20, "hello");
    fx.answer(first, page.clone(), true);
    let (second, _) = searches(&fx.driver.send(Input::LoadMoreSearch))
        .pop()
        .unwrap();

    // New messages shifted the results: the last two of the first page come again.
    let mut next = page[18..].to_vec();
    next.extend(fx.hits(300, 3, "hello"));
    fx.answer(second, next, false);
    assert_eq!(fx.view().hits.len(), 23);
    let mut ids: Vec<MessageId> = fx.view().hits.iter().map(|h| h.message.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 23);
}

#[test]
fn the_same_message_may_be_a_hit_for_its_text_and_for_a_file() {
    let mut fx = setup();
    let (first, _) = fx.run("report").unwrap();
    let text_hit = fx.general_hit(20, "report");
    let file_hit = SearchHit {
        kind: SearchHitKind::Attachment {
            attachment_id: AttachmentId::generate(),
        },
        ..text_hit.clone()
    };
    fx.answer(first, vec![text_hit.clone()], true);
    let (second, _) = searches(&fx.driver.send(Input::LoadMoreSearch))
        .pop()
        .unwrap();
    fx.answer(second, vec![file_hit.clone(), text_hit], false);
    assert_eq!(fx.view().hits.len(), 2, "two kinds of hit on one message");
    assert_eq!(fx.view().hits[1], file_hit);
}

#[test]
fn paging_stops_at_the_limit_and_the_view_says_so() {
    let mut fx = setup();
    let pages = MAX_SEARCH_OFFSET / SEARCH_PAGE as usize;
    let (mut generation, _) = fx.run("hello").unwrap();
    for page in 0..pages {
        let first = 1000 + (page as i64) * 100;
        fx.answer(
            generation,
            fx.hits(first, i64::from(SEARCH_PAGE), "hello"),
            true,
        );
        if page + 1 < pages {
            assert!(!fx.view().at_limit);
            generation = searches(&fx.driver.send(Input::LoadMoreSearch))
                .pop()
                .expect("more can be asked for")
                .0;
        }
    }
    let view = fx.view();
    assert_eq!(view.hits.len(), MAX_SEARCH_OFFSET);
    assert!(view.has_more && view.at_limit);
    assert!(
        fx.driver.send(Input::LoadMoreSearch).effects.is_empty(),
        "no request past the limit"
    );
}

#[test]
fn running_out_of_results_at_the_limit_is_not_the_limit() {
    let mut fx = setup();
    let (generation, _) = fx.run("hello").unwrap();
    fx.answer(
        generation,
        fx.hits(1000, MAX_SEARCH_OFFSET as i64, "hello"),
        false,
    );
    assert!(!fx.view().at_limit);
}

#[test]
fn changing_the_sort_runs_again_with_the_new_order() {
    let mut fx = setup();
    let (first, _) = fx.run("hello").unwrap();
    fx.answer(first, fx.hits(20, 2, "hello"), true);

    let out = fx.driver.send(Input::SetSearchSort(SearchSort::Recent));
    let (second, params) = searches(&out).pop().unwrap();
    assert!(second > first);
    assert_eq!(params.sort, Some(SearchSort::Recent));
    assert_eq!(params.offset, Some(0));
    assert_eq!(params.q, "hello");
    assert_eq!(fx.view().sort, SearchSort::Recent);
    assert!(fx.view().hits.is_empty());

    // Later pages keep the order of their run.
    fx.answer(second, fx.hits(40, 20, "hello"), true);
    let (_, params) = searches(&fx.driver.send(Input::LoadMoreSearch))
        .pop()
        .unwrap();
    assert_eq!(params.sort, Some(SearchSort::Recent));

    // Choosing the order it already has asks for nothing.
    let out = fx.driver.send(Input::SetSearchSort(SearchSort::Recent));
    assert!(out.effects.is_empty());
}

#[test]
fn the_sort_is_chosen_before_there_is_anything_to_search() {
    let mut fx = setup();
    let out = fx.driver.send(Input::SetSearchSort(SearchSort::Recent));
    assert!(out.effects.is_empty(), "nothing to search for yet");
    assert_eq!(fx.view().sort, SearchSort::Recent);
    let (_, params) = fx.run("hello").unwrap();
    assert_eq!(params.sort, Some(SearchSort::Recent));
}

// ----- Failures -----

#[test]
fn a_refused_query_shows_the_servers_reason_and_cannot_be_retried() {
    let mut fx = setup();
    let (generation, _) = fx.run("hello").unwrap();
    fx.fail(
        generation,
        rejected(
            ErrorCode::InvalidRequest,
            "a search can use at most 12 words",
        ),
    );
    assert_eq!(
        failure(&fx),
        ("a search can use at most 12 words".to_owned(), false)
    );
    assert!(fx.view().hits.is_empty());
}

#[test]
fn a_search_that_took_too_long_says_to_narrow_it_and_can_be_retried() {
    let mut fx = setup();
    let (generation, _) = fx.run("hello").unwrap();
    fx.fail(
        generation,
        rejected(ErrorCode::Unavailable, "that search took too long"),
    );
    let (reason, can_retry) = failure(&fx);
    assert_eq!(
        reason,
        "That search took too long. Narrow it with in: or from:"
    );
    assert!(can_retry);

    // Running again is a fresh request.
    let (again, _) = searches(&fx.driver.send(Input::RunSearch)).pop().unwrap();
    assert!(again > generation);
    assert_eq!(fx.view().status, SearchStatus::Loading);
}

#[test]
fn losing_the_server_and_being_rate_limited_can_be_retried() {
    let mut fx = setup();
    let (generation, _) = fx.run("hello").unwrap();
    fx.fail(generation, lost());
    assert_eq!(
        failure(&fx),
        ("Couldn't reach the server.".to_owned(), true)
    );
    let (generation, _) = searches(&fx.driver.send(Input::RunSearch)).pop().unwrap();
    fx.fail(generation, rejected(ErrorCode::RateLimited, "slow down"));
    let (reason, can_retry) = failure(&fx);
    assert!(can_retry);
    assert!(reason.contains("Too many searches"), "{reason}");
}

#[test]
fn a_workspace_that_can_no_longer_be_searched_is_a_failure_and_a_notice() {
    for code in [ErrorCode::Forbidden, ErrorCode::NotFound] {
        let mut fx = setup();
        let (generation, _) = fx.run("hello").unwrap();
        fx.fail(generation, rejected(code, "no"));
        let (reason, can_retry) = failure(&fx);
        assert_eq!(reason, "You can't search this workspace any more.");
        assert!(!can_retry);
        assert!(fx.driver.core.notice().is_some(), "{code:?}");
    }
}

#[test]
fn an_expired_session_closes_the_search_and_signs_out() {
    let mut fx = setup();
    let (generation, _) = fx.run("hello").unwrap();
    fx.fail(generation, rejected(ErrorCode::Unauthenticated, "expired"));
    assert!(matches!(
        fx.driver.core.phase(),
        SessionPhase::SignedOut { .. }
    ));
    assert!(fx.driver.core.search().is_none());
    assert_eq!(
        cancels(&fx.driver),
        0,
        "the answer was the end of the request"
    );
}

#[test]
fn a_session_that_expires_elsewhere_stops_the_search_being_waited_for() {
    let mut fx = setup();
    let at = fx.next_seq();
    let mine = fx.world.message(fx.general, &fx.world.me, at, "mine");
    fx.driver.send(fx.world.event(mine.clone(), None));
    fx.run("hello").unwrap();
    // Another request is told the session is gone.
    fx.driver.send(Input::DeleteMessage {
        conversation: fx.general,
        message: mine.id,
    });
    fx.driver.send(Input::DeleteResult {
        message: mine.id,
        result: Err(rejected(ErrorCode::Unauthenticated, "expired")),
    });
    assert!(matches!(
        fx.driver.core.phase(),
        SessionPhase::SignedOut { .. }
    ));
    assert!(fx.driver.core.search().is_none());
    assert_eq!(
        cancels(&fx.driver),
        1,
        "nothing waits for the answer any more"
    );
}

#[test]
fn a_failed_next_page_keeps_the_hits_and_is_retried_from_the_same_place() {
    let mut fx = setup();
    let (first, _) = fx.run("hello").unwrap();
    fx.answer(first, fx.hits(100, 20, "hello"), true);
    let (second, _) = searches(&fx.driver.send(Input::LoadMoreSearch))
        .pop()
        .unwrap();
    fx.fail(second, lost());
    assert_eq!(fx.view().hits.len(), 20, "what was shown stays");
    assert_eq!(
        failure(&fx),
        ("Couldn't reach the server.".to_owned(), true)
    );
    assert!(fx.view().has_more);

    let (third, params) = searches(&fx.driver.send(Input::LoadMoreSearch))
        .pop()
        .unwrap();
    assert!(third > second);
    assert_eq!(params.offset, Some(20));
    fx.answer(third, fx.hits(200, 2, "hello"), false);
    assert_eq!(fx.view().hits.len(), 22);
    assert_eq!(fx.view().status, SearchStatus::Ready);
}

#[test]
fn a_failed_first_page_cannot_be_continued() {
    let mut fx = setup();
    let (first, _) = fx.run("hello").unwrap();
    fx.fail(first, lost());
    assert!(fx.driver.send(Input::LoadMoreSearch).effects.is_empty());
}

// ----- Opening a hit -----

#[test]
fn opening_a_hit_in_a_loaded_conversation_jumps_to_it() {
    let mut fx = setup();
    let general = fx.general;
    let at = fx.next_seq();
    let message = fx
        .world
        .message(general, &fx.world.other, at, "needle in a haystack");
    fx.driver
        .send(Input::SelectConversation(fx.world.second_channel.id));
    fx.driver
        .load_history(fx.world.second_channel.id, vec![], false);
    // The message is loaded already (its channel was loaded before).
    fx.driver.send(fx.world.event(message.clone(), None));

    let (generation, _) = fx.run("needle").unwrap();
    fx.answer(
        generation,
        vec![hit_in(&message, &fx.world.channel, "needle")],
        false,
    );
    let out = fx.driver.send(Input::OpenSearchHit(0));
    assert!(
        !out.effects
            .iter()
            .any(|e| matches!(e, Effect::FetchMessage { .. })),
        "no request for a message that is loaded"
    );
    assert_eq!(fx.driver.core.selected_conversation(), Some(general));
    let target = fx.driver.core.jump_target().expect("a jump");
    assert_eq!(target.conversation, general);
    assert!(fx.driver.core.search().is_some(), "the screen closes it");
}

#[test]
fn opening_a_hit_that_is_not_loaded_goes_through_the_permalink_machinery() {
    let world = World::new();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    let newer: Vec<Message> = (1000..1003)
        .map(|at| world.message(general, &world.other, at, "recent"))
        .collect();
    driver.load_history(general, newer, true);
    let mut fx = Fx {
        world,
        driver,
        general,
    };
    let message = fx.world.message(general, &fx.world.other, 3, "needle");
    let (generation, _) = fx.run("needle").unwrap();
    fx.answer(
        generation,
        vec![hit_in(&message, &fx.world.channel, "needle")],
        false,
    );
    assert!(!fx.driver.core.opening_link());
    let out = fx.driver.send(Input::OpenSearchHit(0));
    assert!(out.effects.contains(&Effect::FetchMessage {
        message: message.id
    }));
    assert!(
        fx.driver.core.opening_link(),
        "the message is being fetched"
    );
    let out = fx.driver.send(Input::MessageFetched {
        message: message.id,
        result: Ok(MessageResponse {
            message: message.clone(),
        }),
    });
    assert!(fx.driver.core.opening_link(), "its history is being paged");
    assert!(
        out.effects.iter().any(|e| matches!(
            e,
            Effect::FetchHistory {
                before: Some(_),
                ..
            }
        )),
        "older history is paged in until the message is found"
    );
    assert!(fx.driver.core.jump_target().is_none());
    fx.driver
        .load_history(general, vec![message.clone()], false);
    assert!(!fx.driver.core.opening_link(), "found");
    let target = fx.driver.core.jump_target().expect("found it");
    assert_eq!(target.conversation, general);
    assert_eq!(target.key, lazy_client_core::ItemKey::Message(message.id));
}

#[test]
fn opening_a_reply_opens_its_thread() {
    let mut fx = setup();
    let general = fx.general;
    let at = fx.next_seq();
    let root = fx.world.message(general, &fx.world.other, at, "the root");
    fx.driver.send(fx.world.event(root.clone(), None));
    let reply = Message {
        thread_root: Some(root.id),
        ..fx.world
            .message(general, &fx.world.other, 15, "needle in a reply")
    };
    let (generation, _) = fx.run("needle").unwrap();
    fx.answer(
        generation,
        vec![hit_in(&reply, &fx.world.channel, "needle")],
        false,
    );
    fx.driver.send(Input::OpenSearchHit(0));
    fx.driver.send(Input::MessageFetched {
        message: reply.id,
        result: Ok(MessageResponse {
            message: reply.clone(),
        }),
    });
    assert_eq!(fx.driver.core.open_thread().map(|t| t.root), Some(root.id));
}

#[test]
fn opening_a_hit_in_a_channel_that_is_not_joined_offers_to_join() {
    let mut fx = setup();
    let hidden = channel(fx.world.workspace.id, "hidden");
    let message = fx
        .world
        .message(hidden.id, &fx.world.other, 8, "needle in hidden");
    let hit = hit_in(&message, &hidden, "needle");
    assert!(!fx.driver.core.search_hit_joined(&hit));
    assert_eq!(fx.driver.core.search_hit_title(&hit), "#hidden");

    let (generation, _) = fx.run("needle").unwrap();
    fx.answer(generation, vec![hit], false);
    let out = fx.driver.send(Input::OpenSearchHit(0));
    assert!(out.effects.contains(&Effect::FetchMessage {
        message: message.id
    }));
    assert!(fx.driver.core.opening_link());
    fx.driver.send(Input::MessageFetched {
        message: message.id,
        result: Ok(MessageResponse { message }),
    });
    assert!(!fx.driver.core.opening_link(), "the offer ends the search");
    assert_eq!(fx.driver.core.join_offer(), Some(hidden.id));
    assert!(fx.driver.core.notice().is_some());
}

#[test]
fn opening_a_hit_that_is_not_there_does_nothing() {
    let mut fx = setup();
    let (generation, _) = fx.run("needle").unwrap();
    fx.answer(generation, fx.hits(20, 1, "needle"), false);
    let out = fx.driver.send(Input::OpenSearchHit(5));
    assert!(out.effects.is_empty() && out.changes.is_empty());
    assert!(saved_searches(&fx.driver).is_empty(), "nothing was opened");
    // No view at all.
    let mut other = setup();
    assert!(
        other
            .driver
            .send(Input::OpenSearchHit(0))
            .effects
            .is_empty()
    );
}

#[test]
fn titles_say_where_a_hit_is() {
    let mut world = World::new();
    let third = user("morgan");
    let direct = people(world.workspace.id, &[&world.me, &world.other]);
    let group = people(world.workspace.id, &[&world.me, &world.other, &third]);
    world.extra_users.push(third.clone());
    world.extra.push(direct.clone());
    world.extra.push(group.clone());
    let driver = Driver::ready(&world);
    let message = |conversation: &Conversation| {
        hit_in(
            &world.message(conversation.id, &world.other, 4, "hello"),
            conversation,
            "hello",
        )
    };

    let title = |conversation: &Conversation| driver.core.search_hit_title(&message(conversation));
    assert_eq!(title(&world.channel), "#general");
    assert_eq!(title(&direct), "ZOE");
    // The other people, in alphabetical order, and not the person.
    assert_eq!(title(&group), "MORGAN, ZOE");
    for conversation in [&world.channel, &direct, &group] {
        assert!(driver.core.search_hit_joined(&message(conversation)));
    }
    // A conversation the core does not hold is titled from the hit.
    let stranger = people(world.workspace.id, &[&world.other, &third]);
    assert_eq!(title(&stranger), "MORGAN, ZOE");
    assert!(!driver.core.search_hit_joined(&message(&stranger)));
}

// ----- Messages that change while hits are shown -----

#[test]
fn a_deleted_message_is_no_longer_a_hit() {
    let mut fx = setup();
    let hits = fx.hits(20, 3, "hello");
    let doomed = hits[1].message.clone();
    let (generation, _) = fx.run("hello").unwrap();
    fx.answer(generation, hits.clone(), false);

    let at = fx.next_seq();
    let out = fx
        .driver
        .send(fx.world.deleted_event(at, &fx.world.tombstone(&doomed)));
    assert!(out.changes.contains(&Change::SearchChanged));
    assert_eq!(
        fx.view().hits,
        vec![hits[0].clone(), hits[2].clone()],
        "only that one went"
    );

    // An event for a message that is not a hit changes nothing about the view.
    let at = fx.next_seq();
    let stranger = fx
        .world
        .message(fx.general, &fx.world.other, at, "unrelated");
    let out = fx
        .driver
        .send(fx.world.deleted_event(at, &fx.world.tombstone(&stranger)));
    assert!(!out.changes.contains(&Change::SearchChanged));
}

#[test]
fn deleting_my_own_message_removes_it_from_the_hits_at_once() {
    let mut fx = setup();
    let at = fx.next_seq();
    let mine = fx
        .world
        .message(fx.general, &fx.world.me, at, "hello from me");
    fx.driver.send(fx.world.event(mine.clone(), None));
    let (generation, _) = fx.run("hello").unwrap();
    fx.answer(
        generation,
        vec![hit_in(&mine, &fx.world.channel, "hello")],
        false,
    );
    fx.driver.send(Input::DeleteMessage {
        conversation: fx.general,
        message: mine.id,
    });
    let tombstone = fx.world.tombstone(&mine);
    fx.driver.send(Input::DeleteResult {
        message: mine.id,
        result: Ok(MessageResponse { message: tombstone }),
    });
    assert!(fx.view().hits.is_empty());
}

#[test]
fn a_message_deleted_while_the_search_was_out_is_not_brought_back_by_its_answer() {
    let mut fx = setup();
    let hits = fx.hits(20, 3, "hello");
    let doomed = hits[0].message.clone();
    let (generation, _) = fx.run("hello").unwrap();
    // The server read the message before it was deleted; the deletion is delivered first.
    let at = fx.next_seq();
    fx.driver
        .send(fx.world.deleted_event(at, &fx.world.tombstone(&doomed)));
    fx.answer(generation, hits.clone(), true);
    assert_eq!(fx.view().hits, hits[1..].to_vec());

    // The record is for that request only.
    let (next, _) = fx.run("hello").unwrap();
    fx.answer(next, hits.clone(), false);
    assert_eq!(
        fx.view().hits.len(),
        3,
        "an answer read after the deletion is believed (the server would not send it)"
    );
}

#[test]
fn an_edit_updates_the_message_of_a_hit_but_not_its_snippet() {
    let mut fx = setup();
    let hits = fx.hits(20, 2, "hello");
    let original = hits[0].clone();
    let (generation, _) = fx.run("hello").unwrap();
    fx.answer(generation, hits.clone(), false);

    let edited = fx.world.edited(&original.message, "hello, rewritten");
    let at = fx.next_seq();
    let out = fx.driver.send(fx.world.edited_event(at, &edited, None));
    assert!(out.changes.contains(&Change::SearchChanged));
    let view = fx.view();
    assert_eq!(view.hits[0].message, edited);
    assert_eq!(view.hits[0].message.revision, 2);
    assert_eq!(
        view.hits[0].snippet, original.snippet,
        "the server's, from search time"
    );
    assert_eq!(view.hits[1], hits[1]);

    // An older revision (a replay) does not undo it.
    let at = fx.next_seq();
    fx.driver
        .send(fx.world.edited_event(at, &original.message, None));
    assert_eq!(fx.view().hits[0].message.revision, 2);
}

#[test]
fn an_edit_during_the_request_is_applied_to_the_answer() {
    let mut fx = setup();
    let hits = fx.hits(20, 2, "hello");
    let (generation, _) = fx.run("hello").unwrap();
    let edited = fx.world.edited(&hits[1].message, "hello, edited meanwhile");
    let at = fx.next_seq();
    fx.driver.send(fx.world.edited_event(at, &edited, None));
    fx.answer(generation, hits.clone(), false);
    assert_eq!(fx.view().hits[1].message, edited);
    assert_eq!(fx.view().hits[0], hits[0]);
}

#[test]
fn what_changed_during_one_request_is_not_applied_to_the_next() {
    let mut fx = setup();
    let hits = fx.hits(20, 2, "hello");
    let (first, _) = fx.run("hello").unwrap();
    let edited = fx.world.edited(&hits[0].message, "hello, edited");
    let at = fx.next_seq();
    fx.driver.send(fx.world.edited_event(at, &edited, None));
    fx.answer(first, hits.clone(), false);
    // A later run is answered with what the server holds then; the old edit is not remembered
    // against it (the server's message is as new or newer).
    let (second, _) = fx.run("hello").unwrap();
    let newer = fx.world.edited(&edited, "hello, edited again");
    fx.answer(
        second,
        vec![hit_in(&newer, &fx.world.channel, "hello")],
        false,
    );
    assert_eq!(fx.view().hits[0].message, newer);
}

// ----- A resync -----

/// The server's log cannot be replayed: the core fetches a snapshot and starts over.
fn resync(fx: &mut Fx) -> Output {
    fx.driver
        .send(Input::Frame(lazy_protocol::ServerFrame::SnapshotRequired {
            workspace_id: fx.world.workspace.id,
            reason: lazy_protocol::SnapshotReason::CursorExpired,
        }));
    fx.driver
        .send(Input::BootstrapLoaded(Box::new(fx.world.bootstrap())))
}

#[test]
fn after_a_resync_the_search_is_asked_again_for_the_run_that_is_shown() {
    let mut fx = setup();
    fx.driver.send(Input::SetSearchSort(SearchSort::Recent));
    let (first, _) = fx.run("hello").unwrap();
    fx.answer(first, fx.hits(100, 20, "hello"), true);
    let (second, _) = searches(&fx.driver.send(Input::LoadMoreSearch))
        .pop()
        .unwrap();
    fx.answer(second, fx.hits(200, 5, "hello"), false);
    // Typing since does not change which run is shown.
    fx.type_text("goodbye");

    // Deletions and edits during the gap were never delivered: what is shown may be stale.
    let out = resync(&mut fx);
    let (third, params) = searches(&out).pop().expect("asked again");
    assert!(third > second);
    assert_eq!(
        params,
        SearchParams {
            q: "hello".into(),
            limit: Some(SEARCH_PAGE),
            offset: Some(0),
            sort: Some(SearchSort::Recent),
        }
    );
    assert!(fx.view().hits.is_empty());
    assert_eq!(fx.view().status, SearchStatus::Loading);
    assert_eq!(fx.view().query, "goodbye", "the box is not touched");
    fx.answer(third, fx.hits(300, 2, "hello"), false);
    assert_eq!(fx.view().hits.len(), 2);
}

#[test]
fn a_resync_while_waiting_asks_again_and_the_older_answer_is_not_believed() {
    let mut fx = setup();
    let (first, _) = fx.run("hello").unwrap();
    let out = resync(&mut fx);
    let (second, params) = searches(&out).pop().expect("asked again");
    assert!(second > first);
    assert_eq!(params.q, "hello");
    // The answer to the request made before the gap may have been read before it.
    fx.answer(first, fx.hits(20, 3, "hello"), true);
    assert!(fx.view().hits.is_empty());
    fx.answer(second, fx.hits(30, 1, "hello"), false);
    assert_eq!(fx.texts(), vec!["hello 30"]);

    // A page being loaded when the gap came is not continued either: the run starts over.
    let (third, _) = fx.run("again").unwrap();
    fx.answer(third, fx.hits(40, 20, "again"), true);
    let (more, _) = searches(&fx.driver.send(Input::LoadMoreSearch))
        .pop()
        .unwrap();
    let (restarted, params) = searches(&resync(&mut fx)).pop().unwrap();
    assert!(restarted > more);
    assert_eq!((params.q.as_str(), params.offset), ("again", Some(0)));
    assert!(fx.view().hits.is_empty());
}

#[test]
fn a_resync_asks_for_nothing_when_nothing_was_searched() {
    let mut fx = setup();
    assert!(searches(&resync(&mut fx)).is_empty(), "no view");
    fx.driver.send(Input::OpenSearch);
    fx.type_text("hello");
    assert!(searches(&resync(&mut fx)).is_empty(), "typed, never run");
    let (first, _) = fx.run("hello").unwrap();
    fx.fail(first, lost());
    assert!(
        searches(&resync(&mut fx)).is_empty(),
        "no results to be stale"
    );
}

// ----- Recent searches -----

#[test]
fn only_opening_a_result_records_a_search_and_it_is_the_text_that_was_answered() {
    let mut fx = setup();
    let hits = fx.hits(20, 2, "needle");
    let (generation, _) = fx.run("  needle  ").unwrap();
    fx.answer(generation, hits, false);
    assert!(
        fx.driver.core.recent_searches().is_empty(),
        "running is not enough"
    );
    assert!(saved_searches(&fx.driver).is_empty());

    // The person typed on, but opens a result of the earlier run.
    fx.type_text("needle in a haystack");
    let out = fx.driver.send(Input::OpenSearchHit(0));
    assert!(out.changes.contains(&Change::RecentSearchesChanged));
    assert_eq!(fx.driver.core.recent_searches(), ["needle"]);
    assert_eq!(saved_searches(&fx.driver), vec![vec!["needle".to_owned()]]);
}

#[test]
fn recent_searches_are_distinct_newest_first_and_limited() {
    let mut fx = setup();
    let open = |fx: &mut Fx, text: &str| {
        let (generation, _) = fx.run(text).unwrap();
        fx.answer(generation, fx.hits(20, 1, "x"), false);
        fx.driver.send(Input::OpenSearchHit(0));
    };
    for text in ["alpha", "beta", "gamma"] {
        open(&mut fx, text);
    }
    assert_eq!(fx.driver.core.recent_searches(), ["gamma", "beta", "alpha"]);

    // A repeat moves to the front, whatever its case, and takes the new spelling.
    open(&mut fx, "ALPHA");
    assert_eq!(fx.driver.core.recent_searches(), ["ALPHA", "gamma", "beta"]);

    for n in 0..(MAX_RECENT_SEARCHES + 5) {
        open(&mut fx, &format!("query{n}"));
    }
    let recent = fx.driver.core.recent_searches();
    assert_eq!(recent.len(), MAX_RECENT_SEARCHES);
    assert_eq!(recent[0], format!("query{}", MAX_RECENT_SEARCHES + 4));
    assert_eq!(
        saved_searches(&fx.driver).last().unwrap().as_slice(),
        recent,
        "the last save is what is shown"
    );
}

#[test]
fn recent_searches_from_the_last_run_are_restored_and_repaired() {
    let long = "x".repeat(MAX_QUERY_CHARS + 1);
    let mut saved = vec![
        "  first  ".to_owned(),
        String::new(),
        "   ".to_owned(),
        "FIRST".to_owned(),
        long,
    ];
    saved.extend((0..20).map(|n| format!("old{n}")));
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        recent_searches: saved,
        ..Default::default()
    }));
    let recent = driver.core.recent_searches();
    assert_eq!(recent.len(), MAX_RECENT_SEARCHES);
    assert_eq!(
        &recent[..3],
        ["first", "old0", "old1"],
        "trimmed, no repeat, no blank, no long"
    );
}

#[test]
fn closing_the_search_keeps_the_recent_ones() {
    let mut fx = setup();
    let (generation, _) = fx.run("needle").unwrap();
    fx.answer(generation, fx.hits(20, 1, "needle"), false);
    fx.driver.send(Input::OpenSearchHit(0));
    fx.driver.send(Input::CloseSearch);
    assert_eq!(fx.driver.core.recent_searches(), ["needle"]);
}

// ----- Signing out -----

#[test]
fn signing_out_clears_the_search_and_the_recent_ones_and_stops_what_is_out() {
    let mut fx = setup();
    let (generation, _) = fx.run("needle").unwrap();
    fx.answer(generation, fx.hits(20, 1, "needle"), false);
    fx.driver.send(Input::OpenSearchHit(0));
    let (pending, _) = fx.run("another").unwrap();

    let out = fx.driver.send(Input::SignOut);
    assert!(out.effects.contains(&Effect::CancelSearch));
    assert!(out.effects.contains(&Effect::SignOut));
    assert!(fx.driver.core.search().is_none());
    assert!(fx.driver.core.recent_searches().is_empty());

    // Its answer, when it comes, finds nothing to show.
    fx.answer(pending, fx.hits(20, 1, "another"), false);
    assert!(fx.driver.core.search().is_none());
}
