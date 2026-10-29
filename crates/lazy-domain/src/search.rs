//! The search query language: words, phrases, exclusions, and a few filters.
//!
//! Parsing is pure and never fails because of what is typed, only because there is nothing to
//! search for or the query is unreasonably large. The server turns a parsed query into a database
//! query; clients parse the same text to show the person what it was understood as (filter chips).

use time::Date;
use time::macros::format_description;

/// Longest query text, in characters.
pub const MAX_QUERY_CHARS: usize = 200;
/// Most words and phrases in one query.
pub const MAX_QUERY_TERMS: usize = 12;
/// Most values of one filter (`from:a from:b`).
pub const MAX_FILTER_VALUES: usize = 5;
/// Results per page unless the request says otherwise, and the most it may ask for.
pub const DEFAULT_SEARCH_LIMIT: usize = 20;
pub const MAX_SEARCH_LIMIT: usize = 50;
/// How far into the results a request may page: later results are not reachable, so a query is
/// narrowed instead (the cost of a query stays bounded).
pub const MAX_SEARCH_OFFSET: usize = 200;

/// A kind of file to look for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Any attachment.
    Any,
    /// A picture.
    Image,
}

/// What a query asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    /// Words that must appear, lowercase, each matching as a prefix (so `deplo` finds `deploy`).
    pub words: Vec<String>,
    /// Quoted phrases whose words must appear next to each other in order.
    pub phrases: Vec<Vec<String>>,
    /// Words that must not appear (`-draft`).
    pub excluded: Vec<String>,
    /// `from:alice`: messages by these people (usernames, without `@`).
    pub from: Vec<String>,
    /// `in:design`: messages in these channels (names, without `#`).
    pub channels: Vec<String>,
    /// `after:2026-10-01`: on or after this day (UTC).
    pub after: Option<Date>,
    /// `before:2026-10-01`: before this day (UTC).
    pub before: Option<Date>,
    /// `has:file` or `has:image`.
    pub has: Option<FileKind>,
}

/// Why a query could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    #[error("type something to search for")]
    Empty,
    #[error("a search can be at most {MAX_QUERY_CHARS} characters")]
    TooLong,
    #[error("a search can use at most {MAX_QUERY_TERMS} words and phrases")]
    TooManyTerms,
    #[error("a filter can have at most {MAX_FILTER_VALUES} values")]
    TooManyValues,
    #[error("{0:?} is not a date; write it as year-month-day, like 2026-10-01")]
    BadDate(String),
    #[error("has: can be file or image, not {0:?}")]
    BadHas(String),
}

impl SearchQuery {
    /// Whether the query has any text to match (as opposed to only filters).
    pub fn has_text(&self) -> bool {
        !self.words.is_empty() || !self.phrases.is_empty()
    }

    /// Parses what the person typed.
    pub fn parse(text: &str) -> Result<Self, SearchError> {
        if text.chars().count() > MAX_QUERY_CHARS {
            return Err(SearchError::TooLong);
        }
        let mut query = SearchQuery::default();
        for token in tokens(text) {
            match token {
                Token::Phrase(words) => {
                    if !words.is_empty() {
                        query.phrases.push(words);
                    }
                }
                Token::Plain(raw) => query.take_plain(&raw)?,
            }
        }
        if query.words.len() + query.phrases.len() > MAX_QUERY_TERMS {
            return Err(SearchError::TooManyTerms);
        }
        let filters = query.from.len().max(query.channels.len());
        if filters > MAX_FILTER_VALUES {
            return Err(SearchError::TooManyValues);
        }
        let any_filter = !query.from.is_empty()
            || !query.channels.is_empty()
            || query.after.is_some()
            || query.before.is_some()
            || query.has.is_some();
        if !query.has_text() && !any_filter {
            return Err(SearchError::Empty);
        }
        Ok(query)
    }

    fn take_plain(&mut self, raw: &str) -> Result<(), SearchError> {
        if let Some((key, value)) = raw.split_once(':') {
            let value = value.trim();
            match key.to_lowercase().as_str() {
                "from" if !value.is_empty() => {
                    push_unique(&mut self.from, value.trim_start_matches('@').to_lowercase());
                    return Ok(());
                }
                "in" if !value.is_empty() => {
                    push_unique(
                        &mut self.channels,
                        value.trim_start_matches('#').to_lowercase(),
                    );
                    return Ok(());
                }
                "after" if !value.is_empty() => {
                    self.after = Some(parse_date(value)?);
                    return Ok(());
                }
                "before" if !value.is_empty() => {
                    self.before = Some(parse_date(value)?);
                    return Ok(());
                }
                "has" if !value.is_empty() => {
                    self.has = Some(match value.to_lowercase().as_str() {
                        "file" | "files" | "attachment" => FileKind::Any,
                        "image" | "images" | "photo" => FileKind::Image,
                        other => return Err(SearchError::BadHas(other.to_owned())),
                    });
                    return Ok(());
                }
                // Anything else with a colon (a time, a web address) is just text.
                _ => {}
            }
        }
        if let Some(word) = raw.strip_prefix('-')
            && let Some(word) = clean_word(word)
        {
            push_unique(&mut self.excluded, word);
            return Ok(());
        }
        if let Some(word) = clean_word(raw) {
            push_unique(&mut self.words, word);
        }
        Ok(())
    }
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !value.is_empty() && !list.contains(&value) {
        list.push(value);
    }
}

fn parse_date(text: &str) -> Result<Date, SearchError> {
    Date::parse(text, format_description!("[year]-[month]-[day]"))
        .map_err(|_| SearchError::BadDate(text.to_owned()))
}

/// A word with punctuation trimmed from both ends and lowercased; `None` if nothing is left.
fn clean_word(raw: &str) -> Option<String> {
    let trimmed = raw.trim_matches(|c: char| !c.is_alphanumeric());
    (!trimmed.is_empty()).then(|| trimmed.to_lowercase())
}

enum Token {
    Plain(String),
    Phrase(Vec<String>),
}

/// Splits on whitespace, keeping a quoted run together as one phrase (an unclosed quote runs to
/// the end).
fn tokens(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            let mut phrase = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                phrase.push(c);
            }
            out.push(Token::Phrase(
                phrase.split_whitespace().filter_map(clean_word).collect(),
            ));
        } else {
            let mut plain = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '"' {
                    break;
                }
                plain.push(c);
                chars.next();
            }
            out.push(Token::Plain(plain));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    fn parse(text: &str) -> SearchQuery {
        SearchQuery::parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
    }

    #[test]
    fn words_are_lowercased_and_trimmed_of_punctuation() {
        let q = parse("  Deploy,  FAILED!  (staging) ");
        assert_eq!(q.words, ["deploy", "failed", "staging"]);
        assert!(q.has_text());
        // Punctuation inside a word stays: the server decides how it is tokenised.
        assert_eq!(
            parse("foo.bar can't 3.14").words,
            ["foo.bar", "can't", "3.14"]
        );
        // Repeats count once; a bare symbol is nothing.
        assert_eq!(parse("a a ... !!! A").words, ["a"]);
    }

    #[test]
    fn quotes_make_phrases_and_a_dash_excludes() {
        let q = parse(r#"release "rollback plan" -draft -"#);
        assert_eq!(q.words, ["release"]);
        assert_eq!(q.phrases, [vec!["rollback".to_owned(), "plan".to_owned()]]);
        assert_eq!(q.excluded, ["draft"]);
        // An unclosed quote runs to the end; an empty one is ignored.
        assert_eq!(
            parse(r#""open ended"#).phrases,
            [vec!["open".to_owned(), "ended".to_owned()]]
        );
        assert_eq!(parse(r#"x "" y"#).words, ["x", "y"]);
    }

    #[test]
    fn filters_are_recognised_by_their_key() {
        let q = parse(
            "from:@Alice in:#design FROM:bob after:2026-09-01 before:2026-10-01 has:image budget",
        );
        assert_eq!(q.from, ["alice", "bob"]);
        assert_eq!(q.channels, ["design"]);
        assert_eq!(q.after, Some(date!(2026 - 09 - 01)));
        assert_eq!(q.before, Some(date!(2026 - 10 - 01)));
        assert_eq!(q.has, Some(FileKind::Image));
        assert_eq!(q.words, ["budget"]);
        assert_eq!(parse("has:file").has, Some(FileKind::Any));
        // A query of only filters is a query.
        assert!(!parse("from:alice").has_text());
    }

    #[test]
    fn colons_that_are_not_filters_are_just_text() {
        assert_eq!(parse("at 10:30").words, ["at", "10:30"]);
        assert_eq!(
            parse("https://example.com/a").words,
            ["https://example.com/a"]
        );
        assert_eq!(parse("note:this").words, ["note:this"]);
        assert_eq!(parse("from: alice").words, ["from", "alice"]);
    }

    #[test]
    fn what_cannot_be_searched_is_explained() {
        assert_eq!(SearchQuery::parse(""), Err(SearchError::Empty));
        assert_eq!(SearchQuery::parse("   "), Err(SearchError::Empty));
        assert_eq!(SearchQuery::parse("... !!"), Err(SearchError::Empty));
        // Only exclusions is nothing to look for.
        assert_eq!(SearchQuery::parse("-draft"), Err(SearchError::Empty));
        assert_eq!(
            SearchQuery::parse("after:yesterday"),
            Err(SearchError::BadDate("yesterday".to_owned()))
        );
        assert_eq!(
            SearchQuery::parse("after:2026-13-45"),
            Err(SearchError::BadDate("2026-13-45".to_owned()))
        );
        assert_eq!(
            SearchQuery::parse("has:video"),
            Err(SearchError::BadHas("video".to_owned()))
        );
        assert_eq!(
            SearchQuery::parse(&"a".repeat(MAX_QUERY_CHARS + 1)),
            Err(SearchError::TooLong)
        );
        let many = (0..=MAX_QUERY_TERMS)
            .map(|n| format!("w{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(SearchQuery::parse(&many), Err(SearchError::TooManyTerms));
        let from = (0..=MAX_FILTER_VALUES)
            .map(|n| format!("from:u{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(SearchQuery::parse(&from), Err(SearchError::TooManyValues));
    }

    #[test]
    fn text_in_any_script_and_emoji_survive() {
        assert_eq!(
            parse("日本語 Ünïcode café").words,
            ["日本語", "ünïcode", "café"]
        );
        // An emoji is not a word (the index does not hold it), so alone it is nothing to search for.
        assert_eq!(SearchQuery::parse("🎉"), Err(SearchError::Empty));
        assert_eq!(parse("🎉 party").words, ["party"]);
    }

    #[test]
    fn parsing_never_panics_whatever_is_typed() {
        for text in [
            "\"",
            "\"\"\"",
            "-",
            "--x",
            "from:",
            ":",
            "::",
            "in:#",
            "after:",
            "a\u{0}b",
            "\u{202e}x",
            "x\"y\"z",
            "from:\"quoted\"",
        ] {
            let _ = SearchQuery::parse(text);
        }
    }
}
