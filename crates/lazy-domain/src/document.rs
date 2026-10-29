//! The versioned message document.
//!
//! A message is stored and sent as a structured document, not a string, so formatting, mentions,
//! and rich integration blocks can be added without reinterpreting old messages. The plain-text
//! projection feeds search and notifications.
//!
//! Version 2 adds formatting marks, links, code blocks, quotes, and lists to version 1's
//! paragraphs of text and line breaks. Version 3 adds mentions and version 4 adds custom emoji. Older documents still read: they
//! are newer documents that happen to use none of the additions. A document is written with the
//! lowest version that can hold it (4 for custom emoji, 3 for mentions, otherwise 2), so a message
//! without them is byte-for-byte what the previous build wrote.
//!
//! Composer text becomes a document through [`MessageDocument::from_markdown`], and
//! [`MessageDocument::to_markdown`] turns a document back into text that parses to the same
//! document, which is what editing a message needs. See [`crate::markdown`] for the syntax.

use serde::{Deserialize, Serialize};

use crate::EmojiId;
use crate::emoji::{MAX_EMOJI_PER_MESSAGE, custom_name_is_well_formed};
use crate::mention::{MAX_MENTIONS, MentionTarget, handle_is_valid};

/// The version written for a document without mentions. Version 1 is also accepted when reading.
pub const CURRENT_VERSION: u32 = 2;
/// The version written for a document with a mention.
pub const MENTION_VERSION: u32 = 3;
/// The version written for a document with a custom emoji, and the newest accepted when reading.
pub const EMOJI_VERSION: u32 = 4;
/// Most blocks in one document.
pub const MAX_BLOCKS: usize = 100;
/// Most inline items in one block, list item, or quote paragraph.
pub const MAX_INLINES_PER_BLOCK: usize = 1_000;
/// Most items in one list, and most paragraphs in one quote.
pub const MAX_LIST_ITEMS: usize = 100;
/// Most UTF-8 bytes of text across the whole document, code and link text included.
pub const MAX_TEXT_BYTES: usize = 40_000;
/// Longest link target, in bytes.
pub const MAX_URL_BYTES: usize = 2_048;
/// Longest code-block language tag, in characters.
pub const MAX_LANGUAGE_CHARS: usize = 32;
/// Link schemes that may appear in a message. `lazy` is an in-app permalink.
pub const ALLOWED_SCHEMES: [&str; 4] = ["http", "https", "mailto", "lazy"];

/// Why a document was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DocumentError {
    #[error("unsupported document version {0}")]
    UnsupportedVersion(u32),
    #[error("message has no text")]
    Empty,
    #[error("message has more than {MAX_BLOCKS} blocks")]
    TooManyBlocks,
    #[error("a block has more than {MAX_INLINES_PER_BLOCK} items")]
    TooManyInlines,
    #[error("a list or quote has more than {MAX_LIST_ITEMS} items")]
    TooManyItems,
    #[error("message text exceeds {MAX_TEXT_BYTES} bytes")]
    TooLong,
    #[error("text contains a control character; use a line break item for new lines")]
    ControlCharacter,
    #[error(
        "a link is not allowed: it must be http, https, mailto, or a lazy permalink, with no spaces, within {MAX_URL_BYTES} bytes"
    )]
    InvalidLink,
    #[error(
        "a code block language tag must be 1 to {MAX_LANGUAGE_CHARS} letters, digits, or + # . _ -"
    )]
    InvalidLanguage,
    #[error("a mention needs a valid handle and cannot be formatted as code")]
    InvalidMention,
    #[error("message has more than {MAX_MENTIONS} mentions")]
    TooManyMentions,
    #[error("a custom emoji needs a valid name and cannot be formatted as code")]
    InvalidEmoji,
    #[error("message has more than {MAX_EMOJI_PER_MESSAGE} custom emoji")]
    TooManyEmoji,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Formatting applied to a run of text. Marks combine; `code` is for inline code.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Marks {
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub strike: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub code: bool,
}

impl Marks {
    pub const NONE: Marks = Marks {
        bold: false,
        italic: false,
        strike: false,
        code: false,
    };

    pub fn is_empty(&self) -> bool {
        !(self.bold || self.italic || self.strike || self.code)
    }
}

/// An item inside a paragraph, list item, or quote paragraph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Inline {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Marks::is_empty")]
        marks: Marks,
    },
    LineBreak,
    /// A hyperlink. `text` is what is shown; for a bare address it equals `url`. A link carries
    /// formatting like text does (a bold link), so a document records exactly which spans a link
    /// sits inside. `code` is not allowed on a link.
    Link {
        url: String,
        text: String,
        #[serde(default, skip_serializing_if = "Marks::is_empty")]
        marks: Marks,
    },
    /// A custom emoji of the workspace, written `:name:`. `name` is what the author typed (kept so
    /// the message reads sensibly if the emoji has since been renamed or deleted); `id` is what
    /// is shown. `code` is not allowed.
    Emoji {
        id: EmojiId,
        name: String,
        #[serde(default, skip_serializing_if = "Marks::is_empty")]
        marks: Marks,
    },
    /// A mention of a person, a group, or the whole conversation. `handle` is what the author
    /// wrote after the `@`; the target is what is notified. `code` is not allowed on a mention.
    Mention {
        target: MentionTarget,
        handle: String,
        #[serde(default, skip_serializing_if = "Marks::is_empty")]
        marks: Marks,
    },
}

impl Inline {
    /// Unformatted text.
    pub fn text(text: impl Into<String>) -> Self {
        Inline::Text {
            text: text.into(),
            marks: Marks::NONE,
        }
    }

    /// Text with formatting.
    pub fn marked(text: impl Into<String>, marks: Marks) -> Self {
        Inline::Text {
            text: text.into(),
            marks,
        }
    }

    pub fn link(url: impl Into<String>, text: impl Into<String>) -> Self {
        Inline::Link {
            url: url.into(),
            text: text.into(),
            marks: Marks::NONE,
        }
    }

    pub fn emoji(id: EmojiId, name: impl Into<String>) -> Self {
        Inline::Emoji {
            id,
            name: name.into(),
            marks: Marks::NONE,
        }
    }

    pub fn mention(target: MentionTarget, handle: impl Into<String>) -> Self {
        Inline::Mention {
            target,
            handle: handle.into(),
            marks: Marks::NONE,
        }
    }

    /// A link with formatting.
    pub fn marked_link(url: impl Into<String>, text: impl Into<String>, marks: Marks) -> Self {
        Inline::Link {
            url: url.into(),
            text: text.into(),
            marks,
        }
    }
}

/// A block-level element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Paragraph {
        inlines: Vec<Inline>,
    },
    /// Preformatted text. Line breaks are real `\n` characters in `text`.
    CodeBlock {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        text: String,
    },
    /// Quoted paragraphs.
    Quote {
        paragraphs: Vec<Vec<Inline>>,
    },
    BulletList {
        items: Vec<Vec<Inline>>,
    },
    /// A numbered list that starts at `start`.
    OrderedList {
        start: u32,
        items: Vec<Vec<Inline>>,
    },
}

#[derive(Serialize, Deserialize)]
struct Wire {
    version: u32,
    blocks: Vec<Block>,
}

/// A validated message document. Every instance satisfies the version, size, and content rules,
/// including instances deserialized from the network or storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Wire", into = "Wire")]
pub struct MessageDocument {
    blocks: Vec<Block>,
}

impl MessageDocument {
    /// Builds a document from blocks, validating it.
    pub fn new(blocks: Vec<Block>) -> Result<Self, DocumentError> {
        validate(&blocks)?;
        Ok(Self { blocks })
    }

    /// A document with no content. Only a message that has attachments may carry it; every way
    /// of making a document from text still refuses empty text.
    pub fn empty() -> Self {
        Self { blocks: Vec::new() }
    }

    /// Whether the document has no content at all.
    pub fn has_no_content(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Converts text to a document without interpreting any markup: blank lines separate
    /// paragraphs and single newlines become line breaks. For programmatic messages. Composer
    /// text goes through [`MessageDocument::from_markdown`].
    ///
    /// Line endings are normalized, control characters other than tab are dropped, leading blank
    /// lines and trailing whitespace are removed, and text that is empty after this is rejected.
    pub fn from_plain_text(text: &str) -> Result<Self, DocumentError> {
        let cleaned = sanitize(text);
        let mut blocks = Vec::new();
        let mut lines: Vec<&str> = Vec::new();
        for line in cleaned.trim_end().split('\n').chain(std::iter::once("")) {
            if line.trim().is_empty() {
                if !lines.is_empty() {
                    blocks.push(paragraph_from_lines(&lines));
                    lines.clear();
                }
            } else {
                lines.push(line);
            }
        }
        Self::new(blocks)
    }

    /// The blocks of this document.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// The distinct custom emoji used, in the order they first appear.
    pub fn custom_emoji(&self) -> Vec<EmojiId> {
        let mut found = Vec::new();
        for_each_inline(&self.blocks, |inline| {
            if let Inline::Emoji { id, .. } = inline
                && !found.contains(id)
            {
                found.push(*id);
            }
        });
        found
    }

    /// The distinct targets mentioned, in the order they first appear.
    pub fn mentions(&self) -> Vec<MentionTarget> {
        let mut found = Vec::new();
        for_each_inline(&self.blocks, |inline| {
            if let Inline::Mention { target, .. } = inline
                && !found.contains(target)
            {
                found.push(*target);
            }
        });
        found
    }

    /// The plain-text projection, for search and notifications. Formatting marks are dropped,
    /// quotes are prefixed with `> `, list items with `- ` or their number, and a link whose text
    /// differs from its address reads `text (address)`. Blocks are separated by a blank line.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for (index, block) in self.blocks.iter().enumerate() {
            if index > 0 {
                out.push_str("\n\n");
            }
            match block {
                Block::Paragraph { inlines } => push_inlines(&mut out, inlines),
                Block::CodeBlock { text, .. } => out.push_str(text),
                Block::Quote { paragraphs } => {
                    for (n, paragraph) in paragraphs.iter().enumerate() {
                        if n > 0 {
                            out.push_str("\n>\n");
                        }
                        let mut line = String::new();
                        push_inlines(&mut line, paragraph);
                        out.push_str(&prefix_lines(&line, "> "));
                    }
                }
                Block::BulletList { items } => {
                    for (n, item) in items.iter().enumerate() {
                        if n > 0 {
                            out.push('\n');
                        }
                        out.push_str("- ");
                        push_inlines(&mut out, item);
                    }
                }
                Block::OrderedList { start, items } => {
                    for (n, item) in items.iter().enumerate() {
                        if n > 0 {
                            out.push('\n');
                        }
                        out.push_str(&format!("{}. ", u64::from(*start) + n as u64));
                        push_inlines(&mut out, item);
                    }
                }
            }
        }
        out
    }
}

fn push_inlines(out: &mut String, inlines: &[Inline]) {
    for inline in inlines {
        match inline {
            Inline::Text { text, .. } => out.push_str(text),
            Inline::LineBreak => out.push('\n'),
            Inline::Link { url, text, .. } => {
                out.push_str(text);
                if text != url {
                    out.push_str(" (");
                    out.push_str(url);
                    out.push(')');
                }
            }
            Inline::Mention { handle, .. } => {
                out.push('@');
                out.push_str(handle);
            }
            Inline::Emoji { name, .. } => {
                out.push(':');
                out.push_str(name);
                out.push(':');
            }
        }
    }
}

/// Calls `f` on every inline item of every block.
fn for_each_inline(blocks: &[Block], mut f: impl FnMut(&Inline)) {
    for block in blocks {
        match block {
            Block::Paragraph { inlines } => inlines.iter().for_each(&mut f),
            Block::CodeBlock { .. } => {}
            Block::Quote { paragraphs } => paragraphs.iter().flatten().for_each(&mut f),
            Block::BulletList { items } | Block::OrderedList { items, .. } => {
                items.iter().flatten().for_each(&mut f)
            }
        }
    }
}

fn prefix_lines(text: &str, prefix: &str) -> String {
    text.split('\n')
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalizes composer text: CRLF becomes LF, and control characters other than tab and newline
/// are dropped.
pub(crate) fn sanitize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

fn paragraph_from_lines(lines: &[&str]) -> Block {
    let mut inlines = Vec::with_capacity(lines.len() * 2);
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            inlines.push(Inline::LineBreak);
        }
        inlines.push(Inline::text(*line));
    }
    Block::Paragraph { inlines }
}

/// Whether `url` may appear in a link.
pub fn link_is_allowed(url: &str) -> bool {
    if url.is_empty() || url.len() > MAX_URL_BYTES {
        return false;
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    ALLOWED_SCHEMES
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
        && !rest.is_empty()
}

/// Whether `language` is a valid code-block language tag.
pub fn language_is_valid(language: &str) -> bool {
    let count = language.chars().count();
    (1..=MAX_LANGUAGE_CHARS).contains(&count)
        && language
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '.' | '_' | '-'))
}

struct Budget {
    bytes: usize,
    has_text: bool,
    mentions: usize,
    emoji: usize,
}

impl Budget {
    fn spend(&mut self, text: &str) -> Result<(), DocumentError> {
        self.bytes = self.bytes.saturating_add(text.len());
        if self.bytes > MAX_TEXT_BYTES {
            return Err(DocumentError::TooLong);
        }
        self.has_text |= !text.trim().is_empty();
        Ok(())
    }
}

fn check_inlines(inlines: &[Inline], budget: &mut Budget) -> Result<(), DocumentError> {
    if inlines.len() > MAX_INLINES_PER_BLOCK {
        return Err(DocumentError::TooManyInlines);
    }
    for inline in inlines {
        match inline {
            Inline::Text { text, .. } => {
                if text.chars().any(|c| c.is_control() && c != '\t') {
                    return Err(DocumentError::ControlCharacter);
                }
                budget.spend(text)?;
            }
            Inline::LineBreak => {}
            Inline::Link { url, text, marks } => {
                if !link_is_allowed(url) || marks.code {
                    return Err(DocumentError::InvalidLink);
                }
                if text.chars().any(|c| c.is_control()) {
                    return Err(DocumentError::ControlCharacter);
                }
                budget.spend(text)?;
                budget.spend(url)?;
            }
            Inline::Emoji { name, marks, .. } => {
                if !custom_name_is_well_formed(name) || marks.code {
                    return Err(DocumentError::InvalidEmoji);
                }
                budget.emoji += 1;
                if budget.emoji > MAX_EMOJI_PER_MESSAGE {
                    return Err(DocumentError::TooManyEmoji);
                }
                budget.spend(name)?;
                budget.has_text = true;
            }
            Inline::Mention { handle, marks, .. } => {
                if !handle_is_valid(handle) || marks.code {
                    return Err(DocumentError::InvalidMention);
                }
                budget.mentions += 1;
                if budget.mentions > MAX_MENTIONS {
                    return Err(DocumentError::TooManyMentions);
                }
                // A message that is only a mention has something to say.
                budget.spend(handle)?;
                budget.has_text = true;
            }
        }
    }
    Ok(())
}

fn validate(blocks: &[Block]) -> Result<(), DocumentError> {
    if blocks.len() > MAX_BLOCKS {
        return Err(DocumentError::TooManyBlocks);
    }
    let mut budget = Budget {
        bytes: 0,
        has_text: false,
        mentions: 0,
        emoji: 0,
    };
    for block in blocks {
        match block {
            Block::Paragraph { inlines } => check_inlines(inlines, &mut budget)?,
            Block::CodeBlock { language, text } => {
                if language.as_deref().is_some_and(|l| !language_is_valid(l)) {
                    return Err(DocumentError::InvalidLanguage);
                }
                if text
                    .chars()
                    .any(|c| c.is_control() && c != '\t' && c != '\n')
                {
                    return Err(DocumentError::ControlCharacter);
                }
                budget.spend(text)?;
            }
            Block::Quote { paragraphs } => {
                if paragraphs.len() > MAX_LIST_ITEMS {
                    return Err(DocumentError::TooManyItems);
                }
                for paragraph in paragraphs {
                    check_inlines(paragraph, &mut budget)?;
                }
            }
            Block::BulletList { items } | Block::OrderedList { items, .. } => {
                if items.len() > MAX_LIST_ITEMS {
                    return Err(DocumentError::TooManyItems);
                }
                for item in items {
                    check_inlines(item, &mut budget)?;
                }
            }
        }
    }
    if budget.has_text {
        Ok(())
    } else {
        Err(DocumentError::Empty)
    }
}

impl TryFrom<Wire> for MessageDocument {
    type Error = DocumentError;

    fn try_from(wire: Wire) -> Result<Self, Self::Error> {
        if !(1..=EMOJI_VERSION).contains(&wire.version) {
            return Err(DocumentError::UnsupportedVersion(wire.version));
        }
        // The empty document is valid on the wire; whether a message may have one is for the
        // server to decide (it needs attachments).
        if wire.blocks.is_empty() {
            return Ok(Self::empty());
        }
        Self::new(wire.blocks)
    }
}

impl From<MessageDocument> for Wire {
    fn from(document: MessageDocument) -> Self {
        let (mut has_mention, mut has_emoji) = (false, false);
        for_each_inline(&document.blocks, |inline| {
            has_mention |= matches!(inline, Inline::Mention { .. });
            has_emoji |= matches!(inline, Inline::Emoji { .. });
        });
        Wire {
            version: if has_emoji {
                EMOJI_VERSION
            } else if has_mention {
                MENTION_VERSION
            } else {
                CURRENT_VERSION
            },
            blocks: document.blocks,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn plain_text_becomes_paragraphs_and_line_breaks() {
        let doc = MessageDocument::from_plain_text("one\ntwo\n\nthree  \n").unwrap();
        assert_eq!(doc.blocks().len(), 2);
        assert_eq!(doc.plain_text(), "one\ntwo\n\nthree");
    }

    #[test]
    fn crlf_and_control_characters_are_normalized() {
        let doc = MessageDocument::from_plain_text("a\r\nb\u{7}c").unwrap();
        assert_eq!(doc.plain_text(), "a\nbc");
    }

    #[test]
    fn whitespace_only_text_is_rejected() {
        assert_eq!(
            MessageDocument::from_plain_text(" \n\t\n"),
            Err(DocumentError::Empty)
        );
        assert_eq!(
            MessageDocument::from_plain_text(""),
            Err(DocumentError::Empty)
        );
    }

    #[test]
    fn emoji_and_combining_marks_survive() {
        let text = "caf\u{e9} e\u{301} 👩‍👩‍👧‍👦 🎉";
        assert_eq!(
            MessageDocument::from_plain_text(text).unwrap().plain_text(),
            text
        );
    }

    #[test]
    fn the_empty_document_exists_only_where_a_message_has_attachments() {
        let empty = MessageDocument::empty();
        assert!(empty.has_no_content());
        assert_eq!(empty.plain_text(), "");
        assert_eq!(empty.to_markdown(), "");
        // It is valid on the wire, and round-trips.
        let json = serde_json::to_string(&empty).unwrap();
        assert_eq!(json, r#"{"version":2,"blocks":[]}"#);
        assert_eq!(
            serde_json::from_str::<MessageDocument>(&json).unwrap(),
            empty
        );
        // Making a document from text, or from blocks, still refuses to be empty.
        assert_eq!(
            MessageDocument::from_plain_text(""),
            Err(DocumentError::Empty)
        );
        assert_eq!(
            MessageDocument::from_markdown("  \n"),
            Err(DocumentError::Empty)
        );
        assert_eq!(MessageDocument::new(vec![]), Err(DocumentError::Empty));
        // A document with blocks but no text is still invalid.
        assert!(
            serde_json::from_str::<MessageDocument>(
                r#"{"version":2,"blocks":[{"type":"paragraph","inlines":[]}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn json_shape_is_versioned_and_plain_text_omits_marks() {
        let doc = MessageDocument::from_plain_text("hi").unwrap();
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "version": 2,
                "blocks": [{"type": "paragraph", "inlines": [{"type": "text", "text": "hi"}]}]
            })
        );
        assert_eq!(
            serde_json::from_value::<MessageDocument>(json).unwrap(),
            doc
        );
    }

    #[test]
    fn version_one_documents_still_read() {
        // Exactly what every message written before version 2 looks like in storage.
        let stored = serde_json::json!({
            "version": 1,
            "blocks": [{"type": "paragraph", "inlines": [
                {"type": "text", "text": "old"}, {"type": "line_break"}, {"type": "text", "text": "message"}
            ]}]
        });
        let doc: MessageDocument = serde_json::from_value(stored).unwrap();
        assert_eq!(doc.plain_text(), "old\nmessage");
    }

    #[test]
    fn unknown_versions_are_rejected() {
        for version in [0, 5, 99] {
            let json = serde_json::json!({"version": version, "blocks": []});
            assert!(
                serde_json::from_value::<MessageDocument>(json).is_err(),
                "{version}"
            );
        }
    }

    #[test]
    fn rich_blocks_round_trip_through_json() {
        let doc = MessageDocument::new(vec![
            Block::Paragraph {
                inlines: vec![
                    Inline::marked(
                        "bold",
                        Marks {
                            bold: true,
                            ..Marks::NONE
                        },
                    ),
                    Inline::text(" and "),
                    Inline::link("https://example.com", "a link"),
                ],
            },
            Block::CodeBlock {
                language: Some("rust".into()),
                text: "fn main() {\n}".into(),
            },
            Block::Quote {
                paragraphs: vec![vec![Inline::text("quoted")]],
            },
            Block::BulletList {
                items: vec![vec![Inline::text("one")], vec![Inline::text("two")]],
            },
            Block::OrderedList {
                start: 3,
                items: vec![vec![Inline::text("three")]],
            },
        ])
        .unwrap();
        let back: MessageDocument =
            serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn plain_text_projection_covers_every_block() {
        let doc = MessageDocument::new(vec![
            Block::Paragraph {
                inlines: vec![
                    Inline::marked(
                        "bold",
                        Marks {
                            bold: true,
                            ..Marks::NONE
                        },
                    ),
                    Inline::text(" "),
                    Inline::link("https://example.com", "site"),
                    Inline::text(" "),
                    Inline::link("https://example.com", "https://example.com"),
                ],
            },
            Block::CodeBlock {
                language: None,
                text: "a\nb".into(),
            },
            Block::Quote {
                paragraphs: vec![
                    vec![Inline::text("q1"), Inline::LineBreak, Inline::text("q1b")],
                    vec![Inline::text("q2")],
                ],
            },
            Block::BulletList {
                items: vec![vec![Inline::text("x")], vec![Inline::text("y")]],
            },
            Block::OrderedList {
                start: 9,
                items: vec![vec![Inline::text("n")], vec![Inline::text("m")]],
            },
        ])
        .unwrap();
        assert_eq!(
            doc.plain_text(),
            "bold site (https://example.com) https://example.com\n\na\nb\n\n> q1\n> q1b\n>\n> q2\n\n- x\n- y\n\n9. n\n10. m"
        );
    }

    #[test]
    fn oversized_text_is_rejected_across_every_kind_of_text() {
        let text = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            MessageDocument::from_plain_text(&text),
            Err(DocumentError::TooLong)
        );
        assert!(MessageDocument::from_plain_text(&"x".repeat(MAX_TEXT_BYTES)).is_ok());
        let code = Block::CodeBlock {
            language: None,
            text,
        };
        assert_eq!(
            MessageDocument::new(vec![code]),
            Err(DocumentError::TooLong)
        );
    }

    #[test]
    fn counts_are_bounded() {
        let paragraphs = vec!["x"; MAX_BLOCKS + 1].join("\n\n");
        assert_eq!(
            MessageDocument::from_plain_text(&paragraphs),
            Err(DocumentError::TooManyBlocks)
        );
        let items = vec![vec![Inline::text("i")]; MAX_LIST_ITEMS + 1];
        assert_eq!(
            MessageDocument::new(vec![Block::BulletList { items }]),
            Err(DocumentError::TooManyItems)
        );
    }

    #[test]
    fn links_are_restricted_to_safe_schemes() {
        for ok in [
            "http://a.b",
            "https://a.b/c?d=e#f",
            "mailto:me@a.b",
            "lazy://x/y/z",
            "HTTPS://A.B",
        ] {
            assert!(link_is_allowed(ok), "{ok}");
        }
        for bad in [
            "javascript:alert(1)",
            "data:text/html,x",
            "file:///etc/passwd",
            "ftp://a.b",
            "http:",
            "//a.b",
            "a b",
            "https://a b",
            "",
        ] {
            assert!(!link_is_allowed(bad), "{bad}");
        }
        let doc = |url: &str| {
            MessageDocument::new(vec![Block::Paragraph {
                inlines: vec![Inline::link(url, "x")],
            }])
        };
        assert_eq!(doc("javascript:alert(1)"), Err(DocumentError::InvalidLink));
        assert!(doc("https://example.com").is_ok());
        assert_eq!(
            doc(&format!("https://a.b/{}", "x".repeat(MAX_URL_BYTES))),
            Err(DocumentError::InvalidLink)
        );
    }

    #[test]
    fn language_tags_are_validated() {
        for ok in ["rust", "c++", "c#", "objective-c", "tsx", "x.y_z"] {
            assert!(language_is_valid(ok), "{ok}");
        }
        for bad in [
            "",
            "has space",
            "semi;colon",
            &"x".repeat(MAX_LANGUAGE_CHARS + 1),
        ] {
            assert!(!language_is_valid(bad), "{bad}");
        }
        let block = Block::CodeBlock {
            language: Some("bad lang".into()),
            text: "x".into(),
        };
        assert_eq!(
            MessageDocument::new(vec![block]),
            Err(DocumentError::InvalidLanguage)
        );
    }

    #[test]
    fn manual_documents_reject_embedded_newlines_in_inline_text() {
        let blocks = vec![Block::Paragraph {
            inlines: vec![Inline::text("a\nb")],
        }];
        assert_eq!(
            MessageDocument::new(blocks),
            Err(DocumentError::ControlCharacter)
        );
        // Code blocks are the one place real newlines belong.
        assert!(
            MessageDocument::new(vec![Block::CodeBlock {
                language: None,
                text: "a\nb".into()
            }])
            .is_ok()
        );
    }

    #[test]
    fn a_document_with_only_empty_formatting_is_rejected() {
        let blocks = vec![Block::Paragraph {
            inlines: vec![Inline::marked(
                "  ",
                Marks {
                    bold: true,
                    ..Marks::NONE
                },
            )],
        }];
        assert_eq!(MessageDocument::new(blocks), Err(DocumentError::Empty));
    }

    proptest! {
        #[test]
        fn plain_text_projection_is_stable(input in any::<String>()) {
            if let Ok(doc) = MessageDocument::from_plain_text(&input) {
                let projected = doc.plain_text();
                prop_assert!(projected.chars().all(|c| c == '\n' || c == '\t' || !c.is_control()));
                prop_assert_eq!(MessageDocument::from_plain_text(&projected).unwrap(), doc);
            }
        }
    }
}
