//! Suggestions while typing: people and groups after `@`, emoji after `:`. The core ranks them
//! ([`ClientCore::mention_candidates`], the emoji search); the composer replaces the word being
//! typed with the one chosen.

use lazy_client_core::ClientCore;
use lazy_domain::ConversationId;
use lazy_domain::emoji;

use super::avatar;
use super::html::{esc, push};

/// How many suggestions are shown.
const SHOWN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Mention,
    Emoji,
}

/// The word being typed that has suggestions: where it is (in UTF-16 units, as the text field
/// counts) and what it asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub start: u32,
    pub end: u32,
    pub kind: Kind,
    /// What follows the `@` or `:`.
    pub query: String,
}

/// The word before the caret, if it is one that has suggestions: `@` and anything (even nothing),
/// or `:` and at least two more characters, starting the text or after a space.
pub(crate) fn token_at(text: &str, caret: u32) -> Option<Token> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let caret = (caret as usize).min(units.len());
    let before = String::from_utf16_lossy(&units[..caret]);
    let start_byte = before
        .rfind(|c: char| c.is_whitespace())
        .map(|at| at + before[at..].chars().next().map_or(1, char::len_utf8))
        .unwrap_or(0);
    let word = &before[start_byte..];
    let start = before[..start_byte].encode_utf16().count() as u32;
    let (kind, query) = match word.strip_prefix('@') {
        Some(query) => (Kind::Mention, query),
        None => {
            let query = word.strip_prefix(':')?;
            if query.chars().count() < 2 || query.contains(':') {
                return None;
            }
            (Kind::Emoji, query)
        }
    };
    if query.contains('@') {
        return None;
    }
    Some(Token {
        start,
        end: caret as u32,
        kind,
        query: query.to_owned(),
    })
}

/// One suggestion: how it is shown, and the text that replaces the word.
pub(crate) struct Item {
    pub html: String,
    pub insert: String,
}

pub(crate) fn items(core: &ClientCore, conversation: ConversationId, token: &Token) -> Vec<Item> {
    match token.kind {
        Kind::Mention => core
            .mention_candidates(conversation, &token.query)
            .into_iter()
            .take(SHOWN)
            .map(|candidate| {
                let face = match candidate.target {
                    lazy_domain::mention::MentionTarget::User { id } => avatar(core, id, "tiny"),
                    _ => "<span class=\"avatar tiny t1\">@</span>".to_owned(),
                };
                let mut html = String::new();
                push!(
                    html,
                    "{face}<span class=\"list-title\">{}</span><span class=\"muted\">@{}</span>",
                    esc(&candidate.display_name),
                    esc(&candidate.handle)
                );
                if !candidate.in_conversation {
                    html.push_str("<span class=\"muted\">· not in this channel</span>");
                }
                Item {
                    html,
                    insert: format!("@{} ", candidate.handle),
                }
            })
            .collect(),
        Kind::Emoji => {
            let tone = emoji::skin_tone_from_index(core.emoji_prefs().skin_tone);
            let mut items: Vec<Item> = core
                .custom_emoji_suggestions(&token.query, 3)
                .into_iter()
                .map(|custom| Item {
                    html: format!(
                        "<img class=\"emoji\" src=\"{}\" alt=\"\"><span class=\"muted\">:{}:</span>",
                        esc(&lazy_protocol::api::paths::emoji(custom.id)),
                        esc(&custom.name)
                    ),
                    insert: format!(":{}: ", custom.name),
                })
                .collect();
            let room = SHOWN.saturating_sub(items.len());
            items.extend(emoji::search(&token.query, room).into_iter().map(|found| {
                let found = emoji::with_tone(found, tone);
                Item {
                    html: format!(
                        "<span class=\"glyph-emoji\">{}</span><span class=\"muted\">:{}:</span>",
                        esc(found.as_str()),
                        esc(found.shortcode().unwrap_or(found.name()))
                    ),
                    insert: found.as_str().to_owned(),
                }
            }));
            items
        }
    }
}

/// The suggestion list.
pub(crate) fn list(items: &[Item], highlighted: usize) -> String {
    if items.is_empty() {
        return String::new();
    }
    let highlighted = highlighted.min(items.len() - 1);
    let mut out = String::from("<ul class=\"suggest-list\" role=\"listbox\">");
    for (index, item) in items.iter().enumerate() {
        push!(
            out,
            "<li><button type=\"button\" role=\"option\" aria-selected=\"{0}\" class=\"list-main{1}\" data-action=\"suggest-pick\" data-index=\"{index}\">{2}</button></li>",
            index == highlighted,
            if index == highlighted { " chosen" } else { "" },
            item.html
        );
    }
    out.push_str("</ul>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(text: &str) -> Option<Token> {
        token_at(text, text.encode_utf16().count() as u32)
    }

    #[test]
    fn the_word_before_the_caret_asks_for_suggestions() {
        assert_eq!(
            token("hi @ad"),
            Some(Token {
                start: 3,
                end: 6,
                kind: Kind::Mention,
                query: "ad".into()
            })
        );
        assert_eq!(token("@").map(|t| t.query), Some(String::new()));
        assert_eq!(
            token("so :smi").map(|t| (t.kind, t.query)),
            Some((Kind::Emoji, "smi".into()))
        );
        assert_eq!(token("so :s"), None, "two letters before emoji are offered");
        assert_eq!(token("a:smile"), None, "not inside a word");
        assert_eq!(token("mail me@example"), None);
        assert_eq!(token("done :smile: "), None);
        assert_eq!(token("hello"), None);
        // UTF-16 positions, as the text field counts them.
        let t = token("😀 @bo").unwrap();
        assert_eq!((t.start, t.end), (3, 6));
        // The caret in the middle of the text.
        assert_eq!(
            token_at("@ada hello", 4).map(|t| t.query),
            Some("ada".into())
        );
        assert_eq!(token_at("@ada hello", 10), None);
    }
}
