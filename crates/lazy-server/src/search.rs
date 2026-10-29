//! Search (llm-docs/DECISIONS.md, D19): full-text search over the messages a person can read, and
//! over the names of their attached files.
//!
//! # How a search runs
//!
//! One `REPEATABLE READ`, read-only transaction with a statement timeout does everything, so the
//! page is consistent and a slow search is cancelled rather than holding a connection.
//!
//! 1. **Authorization is part of the search.** In the same snapshot, the conversations of the
//!    workspace that the caller is a member of, or that are public channels (archived ones
//!    included), are listed ([`accessible`]; an `in:` filter narrows the list to channels of that
//!    name), and the search itself only ever looks at messages whose conversation is in that list
//!    (`m.conversation_id = ANY(…)`). A private channel or direct conversation the caller is not in
//!    is never in the list, so nothing in it is ever a candidate, and an `in:` filter naming one
//!    matches nothing, exactly like one that does not exist. (The list is passed as a value, not as
//!    a subquery, so the planner sees how many conversations there are: a person with access to
//!    one small conversation must not make it walk the whole table.) Nothing is filtered after
//!    the fact.
//! 2. **The query text becomes a `tsquery` string built with escaping and bound as one parameter**
//!    (`'word':*`, phrases as `<->` chains, exclusions as `!(…)`); no user text is ever part of the
//!    SQL itself, and filter values are bound arrays.
//! 3. **Candidates**: the newest 2,000 matches (`rank_candidates`; one more is fetched to know
//!    whether there were more) are ranked with `ts_rank_cd`, ties newest first. A filters-only query,
//!    and `sort=recent`, are newest first with no ranking and no cap beyond the page's reach.
//! 4. **One hit per message.** A message whose text matches is a [`SearchHitKind::Message`] hit. A
//!    message whose text does not match but one of its files' names does is a
//!    [`SearchHitKind::Attachment`] hit for the first such file (lowest position), so a message is
//!    never listed twice and paging stays simple. The whole query must match the text, or must match
//!    one file's name; terms are not combined across the two.
//! 5. **Only the page** gets details: the messages and conversations are loaded with the same batch
//!    queries as history, and the snippets (`ts_headline`) are produced for those rows alone.

use std::collections::HashMap;

use lazy_domain::search::{
    DEFAULT_SEARCH_LIMIT, FileKind, MAX_SEARCH_LIMIT, MAX_SEARCH_OFFSET, SearchQuery,
};
use lazy_domain::{AttachmentId, UserId, WorkspaceId};
use lazy_protocol::{
    ErrorCode, SearchHit, SearchHitKind, SearchParams, SearchResponse, SearchSort, Snippet,
    SnippetRange,
};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::config::SearchOptions;
use crate::error::AppError;
use crate::{accounts, conversations, messages};

/// What a snippet marks a match with while it is still text: control characters no message holds.
const MARK_START: char = '\u{1}';
const MARK_STOP: char = '\u{2}';
/// How much of a message a snippet without a match (a filters-only search) shows.
const PLAIN_SNIPPET_WORDS: usize = 24;
const PLAIN_SNIPPET_CHARS: usize = 200;
/// The media types `has:image` finds: the pictures the server decodes and shows inline.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

fn too_slow() -> AppError {
    AppError::new(
        ErrorCode::Unavailable,
        "that search took too long; narrow it with in: or from:",
    )
}

/// Turns the database cancelling a statement for its time limit into the answer D19 gives.
fn timeout_as_unavailable(error: AppError) -> AppError {
    match &error {
        AppError::Database(sqlx::Error::Database(db)) if db.code().as_deref() == Some("57014") => {
            too_slow()
        }
        _ => error,
    }
}

// --- The tsquery ---------------------------------------------------------------------------

/// One word as a quoted prefix lexeme. Inside the quotes only `'` and `\` have a meaning, and both
/// are escaped; the text-search parser then splits it exactly as it split the indexed text
/// (`foo-bar` is `'foo-bar' <-> 'foo' <-> 'bar'`, each a prefix).
fn lexeme(word: &str) -> String {
    format!("'{}':*", word.replace('\\', "\\\\").replace('\'', "''"))
}

/// The `tsquery` text for the words, phrases and exclusions of a query, or `None` if there are
/// none. Words and phrases are all required (`&`); a phrase is its words joined with `<->`; an
/// exclusion is `!(…)`. Every word is a prefix, in phrases and exclusions too.
fn tsquery_text(query: &SearchQuery) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for word in &query.words {
        parts.push(lexeme(word));
    }
    for phrase in &query.phrases {
        let chain: Vec<String> = phrase.iter().map(|word| lexeme(word)).collect();
        parts.push(format!("({})", chain.join(" <-> ")));
    }
    for word in &query.excluded {
        parts.push(format!("!({})", lexeme(word)));
    }
    (!parts.is_empty()).then(|| parts.join(" & "))
}

// --- The SQL -------------------------------------------------------------------------------

enum Bind {
    Uuid(Uuid),
    Uuids(Vec<Uuid>),
    Text(String),
    Texts(Vec<String>),
    Time(OffsetDateTime),
    Int(i64),
}

/// The SQL text and the values it binds. The text is composed only of fixed fragments chosen by
/// the shape of the query; every value, including everything the person typed, is a parameter.
struct Plan {
    sql: String,
    binds: Vec<Bind>,
}

struct Binder(Vec<Bind>);

impl Binder {
    fn bind(&mut self, value: Bind) -> String {
        self.0.push(value);
        format!("${}", self.0.len())
    }
}

/// How a request is answered.
struct Shape {
    /// Relevance ranking among the newest candidates (a text query, unless `sort=recent`).
    ranked: bool,
    offset: usize,
    /// The page size after clamping, and never beyond what a query may page to.
    limit: usize,
}

/// The conditions on a message `m` that the filters add.
fn message_filters(query: &SearchQuery, binder: &mut Binder) -> String {
    let mut sql = String::new();
    if !query.from.is_empty() {
        let names = binder.bind(Bind::Texts(query.from.clone()));
        sql.push_str(&format!(
            " AND m.author_id IN (SELECT u.id FROM users u WHERE u.username = ANY({names}))"
        ));
    }
    if let Some(day) = query.after {
        let at = binder.bind(Bind::Time(day.midnight().assume_utc()));
        sql.push_str(&format!(" AND m.created_at >= {at}"));
    }
    if let Some(day) = query.before {
        let at = binder.bind(Bind::Time(day.midnight().assume_utc()));
        sql.push_str(&format!(" AND m.created_at < {at}"));
    }
    match query.has {
        None => {}
        Some(FileKind::Any) => sql.push_str(
            " AND EXISTS (SELECT 1 FROM attachments fa WHERE fa.message_id = m.id \
             AND fa.deleted_at IS NULL)",
        ),
        Some(FileKind::Image) => {
            let types = binder.bind(Bind::Texts(
                IMAGE_TYPES.iter().map(|t| (*t).to_owned()).collect(),
            ));
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM attachments fa WHERE fa.message_id = m.id \
                 AND fa.deleted_at IS NULL AND fa.media_type = ANY({types}))"
            ));
        }
    }
    sql
}

fn build_plan(
    query: &SearchQuery,
    accessible: Vec<Uuid>,
    shape: &Shape,
    options: &SearchOptions,
) -> Plan {
    let mut binder = Binder(Vec::new());
    // The conversations the caller may search (see [`accessible`]). Giving the planner the list
    // itself, rather than a subquery, lets it see how many there are and choose its plan for them.
    let acc = binder.bind(Bind::Uuids(accessible));
    let filters = message_filters(query, &mut binder);
    let tsquery = tsquery_text(query).map(|text| binder.bind(Bind::Text(text)));
    let offset = binder.bind(Bind::Int(shape.offset as i64));
    // One more than the page, to know whether there is another.
    let fetch = binder.bind(Bind::Int(shape.limit as i64 + 1));

    let sql = match (&tsquery, query.has_text()) {
        // Only filters (and maybe exclusions): newest first.
        (_, false) => {
            let exclusion = tsquery
                .as_ref()
                .map(|q| format!(" AND m.search_vector @@ to_tsquery('simple', {q})"))
                .unwrap_or_default();
            format!(
                "SELECT m.id, m.seq, 0 AS src, NULL::uuid AS att, 0::bigint AS total \
                 FROM messages m WHERE m.conversation_id = ANY({acc}) AND NOT m.deleted{filters}{exclusion} \
                 ORDER BY m.seq DESC OFFSET {offset} LIMIT {fetch}"
            )
        }
        (Some(q), true) => {
            let tq = format!("to_tsquery('simple', {q})");
            // How many candidates each branch contributes: all that ranking looks at, or just
            // enough to fill the pages reachable by `recent`.
            let reach = if shape.ranked {
                binder.bind(Bind::Int(options.rank_candidates as i64 + 1))
            } else {
                binder.bind(Bind::Int((shape.offset + shape.limit + 1) as i64))
            };
            let rank_text = if shape.ranked {
                format!("ts_rank_cd(m.search_vector, {tq}, 1)")
            } else {
                "0::real".to_owned()
            };
            let rank_file = if shape.ranked {
                format!("ts_rank_cd(f.name_vector, {tq}, 1)")
            } else {
                "0::real".to_owned()
            };
            let text_branch = format!(
                "SELECT m.id, m.seq, 0 AS src, NULL::uuid AS att, {rank_text} AS rank \
                 FROM messages m \
                 WHERE m.conversation_id = ANY({acc}) AND NOT m.deleted \
                   AND m.search_vector @@ {tq}{filters} \
                 ORDER BY m.seq DESC LIMIT {reach}"
            );
            // A message whose text does not match, by the first of its files whose name does.
            let file_branch = format!(
                "SELECT m.id, m.seq, 1 AS src, f.id AS att, {rank_file} AS rank \
                 FROM (SELECT DISTINCT ON (a.message_id) a.id, a.message_id, a.name_vector \
                       FROM attachments a \
                       WHERE a.name_vector @@ {tq} AND a.message_id IS NOT NULL \
                         AND a.conversation_id = ANY({acc}) AND a.deleted_at IS NULL \
                       ORDER BY a.message_id, a.ordinal) f \
                 JOIN messages m ON m.id = f.message_id \
                 WHERE m.conversation_id = ANY({acc}) AND NOT m.deleted \
                   AND NOT (m.search_vector @@ {tq}){filters} \
                 ORDER BY m.seq DESC LIMIT {reach}"
            );
            if shape.ranked {
                let cap = binder.bind(Bind::Int(options.rank_candidates as i64));
                format!(
                    "WITH cand AS MATERIALIZED (SELECT * FROM (({text_branch}) UNION ALL \
                     ({file_branch})) u ORDER BY seq DESC LIMIT {reach}), \
                     numbered AS (SELECT cand.*, row_number() OVER (ORDER BY seq DESC) AS rn, \
                                  count(*) OVER () AS total FROM cand) \
                     SELECT id, seq, src, att, total FROM numbered WHERE rn <= {cap} \
                     ORDER BY rank DESC, seq DESC OFFSET {offset} LIMIT {fetch}"
                )
            } else {
                format!(
                    "SELECT id, seq, src, att, 0::bigint AS total \
                     FROM (({text_branch}) UNION ALL ({file_branch})) u \
                     ORDER BY seq DESC OFFSET {offset} LIMIT {fetch}"
                )
            }
        }
        (None, true) => unreachable!("a query with text has a tsquery"),
    };
    Plan {
        sql,
        binds: binder.0,
    }
}

type CandidateRow = (Uuid, i64, i32, Option<Uuid>, i64);

fn bind_all<'q, O>(
    mut query: sqlx::query::QueryAs<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments>,
    binds: Vec<Bind>,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments> {
    for value in binds {
        query = match value {
            Bind::Uuid(v) => query.bind(v),
            Bind::Uuids(v) => query.bind(v),
            Bind::Text(v) => query.bind(v),
            Bind::Texts(v) => query.bind(v),
            Bind::Time(v) => query.bind(v),
            Bind::Int(v) => query.bind(v),
        };
    }
    query
}

// --- Snippets ------------------------------------------------------------------------------

/// A snippet from text in which matches are wrapped in the two control characters. Ranges are in
/// characters (Unicode scalar values) of the text left once the markers are taken out, so they
/// stay right however the headline was trimmed, and for emoji, combining marks and CJK alike.
fn snippet_from_marked(marked: &str) -> Snippet {
    let mut text = String::with_capacity(marked.len());
    let mut highlights = Vec::new();
    let mut count: u32 = 0;
    let mut open: Option<u32> = None;
    for c in marked.chars() {
        match c {
            MARK_START => {
                open.get_or_insert(count);
            }
            MARK_STOP => {
                if let Some(start) = open.take()
                    && count > start
                {
                    highlights.push(SnippetRange { start, end: count });
                }
            }
            c => {
                text.push(c);
                count += 1;
            }
        }
    }
    if let Some(start) = open
        && count > start
    {
        highlights.push(SnippetRange { start, end: count });
    }
    Snippet { text, highlights }
}

/// The start of a message, for a hit that matched on filters alone.
fn plain_snippet(text: &str) -> Snippet {
    let words: Vec<&str> = text.split_whitespace().take(PLAIN_SNIPPET_WORDS).collect();
    let joined = words.join(" ");
    let text = if joined.chars().count() > PLAIN_SNIPPET_CHARS {
        joined.chars().take(PLAIN_SNIPPET_CHARS).collect()
    } else {
        joined
    };
    Snippet {
        text,
        highlights: Vec::new(),
    }
}

/// A file name as a snippet, with the words of the name that start with a query word marked.
/// Words are runs of letters and digits, as the index sees them.
fn file_snippet(name: &str, terms: &[String]) -> Snippet {
    let mut highlights = Vec::new();
    let mut word = String::new();
    let mut start = 0u32;
    let mut index = 0u32;
    let mut flush = |word: &mut String, start: u32, end: u32| {
        if !word.is_empty() {
            let lower = word.to_lowercase();
            if terms.iter().any(|term| lower.starts_with(term.as_str())) {
                highlights.push(SnippetRange { start, end });
            }
            word.clear();
        }
    };
    for c in name.chars() {
        if c.is_alphanumeric() {
            if word.is_empty() {
                start = index;
            }
            word.push(c);
        } else {
            flush(&mut word, start, index);
        }
        index += 1;
    }
    flush(&mut word, start, index);
    Snippet {
        text: name.to_owned(),
        highlights,
    }
}

// --- The search ----------------------------------------------------------------------------

/// Searches `workspace` as `user`.
pub async fn search(
    pool: &PgPool,
    options: &SearchOptions,
    user: UserId,
    workspace: WorkspaceId,
    params: SearchParams,
) -> Result<SearchResponse, AppError> {
    if accounts::workspace_role(pool, workspace, user)
        .await?
        .is_none()
    {
        return Err(AppError::not_found("workspace not found"));
    }
    let (query, shape) = prepare(&params)?;
    if shape.limit == 0 {
        // Paged as far as a query may be.
        return Ok(SearchResponse {
            hits: Vec::new(),
            has_more: false,
            truncated: false,
        });
    }
    run(pool, options, user, workspace, &query, &shape)
        .await
        .map_err(timeout_as_unavailable)
}

/// Parses and bounds a request.
fn prepare(params: &SearchParams) -> Result<(SearchQuery, Shape), AppError> {
    // Text cannot hold a NUL; a person cannot type one, but a client can send one.
    let text = params.q.replace('\0', " ");
    let query = SearchQuery::parse(&text).map_err(|e| AppError::invalid(e.to_string()))?;
    let offset = params.offset.unwrap_or(0) as usize;
    if offset > MAX_SEARCH_OFFSET {
        return Err(AppError::invalid(format!(
            "a search can page at most {MAX_SEARCH_OFFSET} results in; narrow it with in: or from:"
        )));
    }
    let wanted = params
        .limit
        .map(|limit| limit as usize)
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);
    let ranked = query.has_text() && params.sort.unwrap_or_default() == SearchSort::Relevance;
    Ok((
        query,
        Shape {
            ranked,
            offset,
            // No result past the 200th is reachable.
            limit: wanted.min(MAX_SEARCH_OFFSET - offset),
        },
    ))
}

/// The conversations `user` may search in `workspace`: those they are in and the public channels
/// (archived ones too), and with `in:` only the channels of those names. Removed members are not
/// in `conversation_members`, so they have lost access the moment they were removed.
async fn accessible(
    conn: &mut PgConnection,
    query: &SearchQuery,
    user: UserId,
    workspace: WorkspaceId,
) -> Result<Vec<Uuid>, AppError> {
    let mut binder = Binder(Vec::new());
    let ws = binder.bind(Bind::Uuid(workspace.as_uuid()));
    let me = binder.bind(Bind::Uuid(user.as_uuid()));
    let mut sql = format!(
        "SELECT c.id FROM conversations c WHERE c.workspace_id = {ws} \
         AND ((c.kind = 'channel' AND c.visibility = 'public') OR EXISTS \
         (SELECT 1 FROM conversation_members cm WHERE cm.conversation_id = c.id AND cm.user_id = {me}))"
    );
    if !query.channels.is_empty() {
        let names = binder.bind(Bind::Texts(query.channels.clone()));
        sql.push_str(&format!(
            " AND c.kind = 'channel' AND c.name = ANY({names})"
        ));
    }
    let ids: Vec<(Uuid,)> = bind_all(sqlx::query_as(sqlx::AssertSqlSafe(sql)), binder.0)
        .fetch_all(conn)
        .await?;
    Ok(ids.into_iter().map(|(id,)| id).collect())
}

async fn run(
    pool: &PgPool,
    options: &SearchOptions,
    user: UserId,
    workspace: WorkspaceId,
    query: &SearchQuery,
    shape: &Shape,
) -> Result<SearchResponse, AppError> {
    let tsquery_text = tsquery_text(query);
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    set_limits(&mut tx, options).await?;

    let allowed = accessible(&mut tx, query, user, workspace).await?;
    if allowed.is_empty() {
        return Ok(SearchResponse {
            hits: Vec::new(),
            has_more: false,
            truncated: false,
        });
    }
    let plan = build_plan(query, allowed, shape, options);
    // Not a prepared statement: each search is planned for its own values, never for a generic
    // plan that a cached statement might fall back to.
    let mut rows: Vec<CandidateRow> = bind_all(
        sqlx::query_as(sqlx::AssertSqlSafe(plan.sql)).persistent(false),
        plan.binds,
    )
    .fetch_all(&mut *tx)
    .await?;
    let has_more = rows.len() > shape.limit && shape.offset + shape.limit < MAX_SEARCH_OFFSET;
    rows.truncate(shape.limit);
    let truncated = shape.ranked
        && rows
            .first()
            .is_some_and(|row| row.4 > options.rank_candidates as i64);

    let message_ids: Vec<Uuid> = rows.iter().map(|row| row.0).collect();
    let mut found = messages::messages_by_ids(&mut tx, &message_ids).await?;
    let conversation_ids: Vec<Uuid> = {
        let mut ids: Vec<Uuid> = found
            .values()
            .map(|message| message.conversation_id.as_uuid())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    };
    let conversations = conversations::conversations_by_ids(&mut tx, &conversation_ids).await?;

    // Headlines for the messages that matched on their text, names for the files that matched.
    let text_hits: Vec<Uuid> = rows
        .iter()
        .filter(|row| row.2 == 0)
        .map(|row| row.0)
        .collect();
    let headlines = match (&tsquery_text, query.has_text()) {
        (Some(text), true) => headlines(&mut tx, &text_hits, text).await?,
        _ => HashMap::new(),
    };
    let file_ids: Vec<Uuid> = rows.iter().filter_map(|row| row.3).collect();
    let file_names = file_names(&mut tx, &file_ids).await?;
    tx.commit().await?;

    let terms: Vec<String> = query
        .words
        .iter()
        .chain(query.phrases.iter().flatten())
        .cloned()
        .collect();
    let mut hits = Vec::with_capacity(rows.len());
    for (id, _seq, src, file, _total) in rows {
        let Some(message) = found.remove(&id) else {
            continue;
        };
        let Some(conversation) = conversations.get(&message.conversation_id.as_uuid()) else {
            continue;
        };
        let (kind, snippet) = match file {
            Some(file) if src == 1 => {
                let name = file_names.get(&file).cloned().unwrap_or_default();
                (
                    SearchHitKind::Attachment {
                        attachment_id: AttachmentId::from_uuid(file),
                    },
                    file_snippet(&name, &terms),
                )
            }
            _ => {
                let snippet = match headlines.get(&id) {
                    Some(marked) if !marked.is_empty() => snippet_from_marked(marked),
                    _ => plain_snippet(&message.document.plain_text()),
                };
                (SearchHitKind::Message, snippet)
            }
        };
        hits.push(SearchHit {
            message,
            conversation: conversation.clone(),
            kind,
            snippet,
        });
    }
    Ok(SearchResponse {
        hits,
        has_more,
        truncated,
    })
}

/// The statement timeout (and, for a test, the artificial delay that makes it fire).
async fn set_limits(conn: &mut PgConnection, options: &SearchOptions) -> Result<(), AppError> {
    // Searches are cheap queries with expensive-looking plans; compiling them would cost more
    // than running them.
    sqlx::query("SELECT set_config('statement_timeout', $1, true), set_config('jit', 'off', true)")
        .bind(format!(
            "{}ms",
            options.statement_timeout.as_millis().max(1)
        ))
        .execute(&mut *conn)
        .await?;
    if let Some(delay) = options.test_delay {
        sqlx::query("SELECT pg_sleep($1)")
            .bind(delay.as_secs_f64())
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// `ts_headline` of each message's text for the matched words, with the markers as control
/// characters. Any marker already in the text is replaced first.
async fn headlines(
    conn: &mut PgConnection,
    ids: &[Uuid],
    tsquery: &str,
) -> Result<HashMap<Uuid, String>, AppError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let options = format!(
        "StartSel={MARK_START}, StopSel={MARK_STOP}, MaxWords=24, MinWords=8, ShortWord=0, \
         MaxFragments=1"
    );
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT m.id, ts_headline('simple', translate(m.plain_text, $4, '  '), \
                                  to_tsquery('simple', $2), $3) \
         FROM messages m WHERE m.id = ANY($1)",
    )
    .bind(ids)
    .bind(tsquery)
    .bind(options)
    .bind(format!("{MARK_START}{MARK_STOP}"))
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().collect())
}

async fn file_names(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, String>, AppError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM attachments WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(conn)
            .await?;
    Ok(rows.into_iter().collect())
}

/// `EXPLAIN (ANALYZE, BUFFERS)` of the candidate query a search would run, for measuring plans on
/// real data. Not part of the server's API.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub async fn explain(
    pool: &PgPool,
    options: &SearchOptions,
    user: UserId,
    workspace: WorkspaceId,
    params: SearchParams,
) -> Result<String, AppError> {
    let (query, shape) = prepare(&params)?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    set_limits(&mut tx, options).await?;
    let allowed = accessible(&mut tx, &query, user, workspace).await?;
    if allowed.is_empty() {
        return Ok("no accessible conversation: nothing is run".to_owned());
    }
    let plan = build_plan(&query, allowed, &shape, options);
    let sql = format!("EXPLAIN (ANALYZE, BUFFERS) {}", plan.sql);
    let lines: Vec<(String,)> = bind_all(
        sqlx::query_as(sqlx::AssertSqlSafe(sql)).persistent(false),
        plan.binds,
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    Ok(lines
        .into_iter()
        .map(|(line,)| line)
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> SearchQuery {
        SearchQuery::parse(text).unwrap()
    }

    #[test]
    fn words_become_escaped_prefix_lexemes() {
        assert_eq!(
            tsquery_text(&parsed("deploy fail")).unwrap(),
            "'deploy':* & 'fail':*"
        );
        // Every character with a meaning in tsquery syntax is inside quotes, and the two that
        // have one inside quotes are escaped.
        assert_eq!(
            tsquery_text(&parsed("it's a\\b x&y|z!(w)<->v:*")).unwrap(),
            "'it''s':* & 'a\\\\b':* & 'x&y|z!(w)<->v':*"
        );
        assert_eq!(
            tsquery_text(&parsed(r#"plan "rollback plan" -draft"#)).unwrap(),
            "'plan':* & ('rollback':* <-> 'plan':*) & !('draft':*)"
        );
        assert!(tsquery_text(&parsed("from:alice")).is_none());
    }

    #[test]
    fn ranges_count_characters_not_bytes() {
        let s = snippet_from_marked(
            "a \u{1}caf\u{e9}\u{2} \u{1}\u{65e5}\u{672c}\u{2} \u{1f389} \u{1}e\u{301}t\u{e9}\u{2}",
        );
        assert_eq!(
            s.text,
            "a caf\u{e9} \u{65e5}\u{672c} \u{1f389} e\u{301}t\u{e9}"
        );
        let marked: Vec<String> = s
            .highlights
            .iter()
            .map(|r| {
                s.text
                    .chars()
                    .skip(r.start as usize)
                    .take((r.end - r.start) as usize)
                    .collect()
            })
            .collect();
        assert_eq!(marked, ["caf\u{e9}", "\u{65e5}\u{672c}", "e\u{301}t\u{e9}"]);
        // An unbalanced marker is closed at the end; an empty one is dropped.
        let s = snippet_from_marked("ab \u{1}cd");
        assert_eq!((s.text.as_str(), s.highlights.len()), ("ab cd", 1));
        assert_eq!(s.highlights[0], SnippetRange { start: 3, end: 5 });
        assert!(snippet_from_marked("a\u{1}\u{2}b").highlights.is_empty());
    }

    #[test]
    fn a_file_name_marks_the_words_that_start_with_a_term() {
        let s = file_snippet(
            "Quarterly_Report-v2.pdf",
            &["report".to_owned(), "pd".to_owned()],
        );
        assert_eq!(s.text, "Quarterly_Report-v2.pdf");
        assert_eq!(
            s.highlights,
            [
                SnippetRange { start: 10, end: 16 },
                SnippetRange { start: 20, end: 23 }
            ]
        );
    }

    #[test]
    fn a_plain_snippet_is_the_start_of_the_text() {
        let long = "word ".repeat(100);
        let s = plain_snippet(&long);
        assert_eq!(s.text.split(' ').count(), PLAIN_SNIPPET_WORDS);
        assert!(s.highlights.is_empty());
        assert_eq!(plain_snippet("  a \n b ").text, "a b");
    }

    #[test]
    fn paging_is_bounded() {
        let prep = |q: &str, limit, offset| {
            prepare(&SearchParams {
                q: q.to_owned(),
                limit,
                offset,
                sort: None,
            })
        };
        let (_, shape) = prep("x", None, None).unwrap();
        assert_eq!((shape.limit, shape.offset, shape.ranked), (20, 0, true));
        assert_eq!(prep("x", Some(0), None).unwrap().1.limit, 1);
        assert_eq!(prep("x", Some(9999), None).unwrap().1.limit, 50);
        assert_eq!(prep("x", Some(50), Some(190)).unwrap().1.limit, 10);
        assert_eq!(prep("x", Some(50), Some(200)).unwrap().1.limit, 0);
        assert!(prep("x", None, Some(201)).is_err());
        assert!(!prep("from:a", None, None).unwrap().1.ranked);
        assert!(prep("\0", None, None).is_err());
    }
}
