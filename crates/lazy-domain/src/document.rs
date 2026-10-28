//! The versioned message document.
//!
//! A message is stored and sent as a structured document, not a string, so formatting, mentions,
//! and rich integration blocks can be added without reinterpreting old messages. Version 1
//! supports paragraphs of text and line breaks. The plain-text projection feeds search and
//! notifications.

use serde::{Deserialize, Serialize};

/// The only document version this build reads and writes.
pub const CURRENT_VERSION: u32 = 1;
/// Most blocks in one document.
pub const MAX_BLOCKS: usize = 100;
/// Most inline items in one block.
pub const MAX_INLINES_PER_BLOCK: usize = 1_000;
/// Most UTF-8 bytes of text across the whole document.
pub const MAX_TEXT_BYTES: usize = 40_000;

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
    #[error("message text exceeds {MAX_TEXT_BYTES} bytes")]
    TooLong,
    #[error("text contains a control character; use a line break item for new lines")]
    ControlCharacter,
}

/// An item inside a paragraph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Inline {
    Text { text: String },
    LineBreak,
}

/// A block-level element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Paragraph { inlines: Vec<Inline> },
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

    /// Converts composer text to a document.
    ///
    /// Line endings are normalized, control characters other than tab are dropped, blank lines
    /// separate paragraphs, and single newlines become line breaks. Leading blank lines and
    /// trailing whitespace are removed. Text that is empty after this is rejected.
    pub fn from_plain_text(text: &str) -> Result<Self, DocumentError> {
        let cleaned: String = text
            .replace("\r\n", "\n")
            .chars()
            .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
            .collect();

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

    /// The plain-text projection: paragraphs separated by a blank line, line breaks as `\n`.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for (index, block) in self.blocks.iter().enumerate() {
            if index > 0 {
                out.push_str("\n\n");
            }
            match block {
                Block::Paragraph { inlines } => {
                    for inline in inlines {
                        match inline {
                            Inline::Text { text } => out.push_str(text),
                            Inline::LineBreak => out.push('\n'),
                        }
                    }
                }
            }
        }
        out
    }
}

fn paragraph_from_lines(lines: &[&str]) -> Block {
    let mut inlines = Vec::with_capacity(lines.len() * 2);
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            inlines.push(Inline::LineBreak);
        }
        inlines.push(Inline::Text {
            text: (*line).to_owned(),
        });
    }
    Block::Paragraph { inlines }
}

fn validate(blocks: &[Block]) -> Result<(), DocumentError> {
    if blocks.len() > MAX_BLOCKS {
        return Err(DocumentError::TooManyBlocks);
    }
    let mut bytes = 0usize;
    let mut has_text = false;
    for block in blocks {
        let Block::Paragraph { inlines } = block;
        if inlines.len() > MAX_INLINES_PER_BLOCK {
            return Err(DocumentError::TooManyInlines);
        }
        for inline in inlines {
            if let Inline::Text { text } = inline {
                if text.chars().any(|c| c.is_control() && c != '\t') {
                    return Err(DocumentError::ControlCharacter);
                }
                bytes = bytes.saturating_add(text.len());
                if bytes > MAX_TEXT_BYTES {
                    return Err(DocumentError::TooLong);
                }
                has_text |= !text.trim().is_empty();
            }
        }
    }
    if has_text {
        Ok(())
    } else {
        Err(DocumentError::Empty)
    }
}

impl TryFrom<Wire> for MessageDocument {
    type Error = DocumentError;

    fn try_from(wire: Wire) -> Result<Self, Self::Error> {
        if wire.version != CURRENT_VERSION {
            return Err(DocumentError::UnsupportedVersion(wire.version));
        }
        Self::new(wire.blocks)
    }
}

impl From<MessageDocument> for Wire {
    fn from(document: MessageDocument) -> Self {
        Wire {
            version: CURRENT_VERSION,
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
    fn json_shape_is_versioned() {
        let doc = MessageDocument::from_plain_text("hi").unwrap();
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "version": 1,
                "blocks": [{"type": "paragraph", "inlines": [{"type": "text", "text": "hi"}]}]
            })
        );
        assert_eq!(
            serde_json::from_value::<MessageDocument>(json).unwrap(),
            doc
        );
    }

    #[test]
    fn unknown_versions_are_rejected() {
        let json = serde_json::json!({"version": 2, "blocks": []});
        assert!(serde_json::from_value::<MessageDocument>(json).is_err());
    }

    #[test]
    fn oversized_text_is_rejected() {
        let text = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            MessageDocument::from_plain_text(&text),
            Err(DocumentError::TooLong)
        );
        assert!(MessageDocument::from_plain_text(&"x".repeat(MAX_TEXT_BYTES)).is_ok());
    }

    #[test]
    fn too_many_blocks_are_rejected() {
        let text = vec!["x"; MAX_BLOCKS + 1].join("\n\n");
        assert_eq!(
            MessageDocument::from_plain_text(&text),
            Err(DocumentError::TooManyBlocks)
        );
    }

    #[test]
    fn manual_documents_reject_embedded_newlines() {
        let blocks = vec![Block::Paragraph {
            inlines: vec![Inline::Text {
                text: "a\nb".into(),
            }],
        }];
        assert_eq!(
            MessageDocument::new(blocks),
            Err(DocumentError::ControlCharacter)
        );
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
