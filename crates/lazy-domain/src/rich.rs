//! Rich messages (llm-docs/DECISIONS.md, D40): a versioned, declarative list of blocks that an
//! integration can send alongside a message's text, drawn by each client with its own components.
//!
//! The model is Slack's Block Kit, cut to what Lazy draws: a header, sections (text, a grid of
//! fields, and a button beside them), dividers, small context lines, and rows of buttons. There is
//! no HTML and nothing remote: text is a [`MessageDocument`], a button opens an allowed link or
//! tells the app that sent the message that it was pressed (the server checks, on every press,
//! that the person may still see the message and that the button is in it).
//!
//! A message with rich content still has its ordinary document: that is the text older clients
//! show, notifications use, and search finds. A client that knows rich content shows the blocks
//! instead.
//!
//! # Reading and writing
//!
//! [`RichContent::new`] checks everything (sizes, links, ids) and is how content is made. Reading
//! is lenient, so a newer server can add a kind of block without breaking this client: a block
//! this build does not know, or cannot read, becomes [`RichBlock::Unsupported`] and the rest of the
//! message still shows.

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::MessageDocument;
use crate::document::{MAX_TEXT_BYTES, link_is_allowed};

/// The version written.
pub const RICH_VERSION: u32 = 1;
/// Most blocks in rich content.
pub const MAX_RICH_BLOCKS: usize = 50;
/// Most fields in a section.
pub const MAX_FIELDS: usize = 10;
/// Most buttons in a row.
pub const MAX_BUTTONS: usize = 10;
/// Most items in a context line.
pub const MAX_CONTEXT_ITEMS: usize = 10;
/// Longest header, in characters.
pub const MAX_HEADER_CHARS: usize = 150;
/// Longest button label, in characters.
pub const MAX_BUTTON_TEXT_CHARS: usize = 75;
/// Longest action id or block id, in characters.
pub const MAX_ACTION_ID_CHARS: usize = 255;
/// Longest button value, in characters.
pub const MAX_VALUE_CHARS: usize = 2_000;

/// Why rich content was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RichError {
    #[error("rich content has no blocks")]
    Empty,
    #[error("rich content has more than {MAX_RICH_BLOCKS} blocks")]
    TooManyBlocks,
    #[error(
        "a section has more than {MAX_FIELDS} fields, or a row more than {MAX_BUTTONS} buttons, or a context line more than {MAX_CONTEXT_ITEMS} items"
    )]
    TooManyItems,
    #[error("a block is empty")]
    EmptyBlock,
    #[error("a header must be 1 to {MAX_HEADER_CHARS} characters on one line")]
    InvalidHeader,
    #[error(
        "a button needs a label of 1 to {MAX_BUTTON_TEXT_CHARS} characters, an action id of 1 to {MAX_ACTION_ID_CHARS}, a value of at most {MAX_VALUE_CHARS}, and an allowed link if it has one"
    )]
    InvalidButton,
    #[error("two buttons in one block have the same action id")]
    DuplicateAction,
    #[error("a block id must be 1 to {MAX_ACTION_ID_CHARS} characters")]
    InvalidBlockId,
    #[error("rich content text exceeds {MAX_TEXT_BYTES} bytes")]
    TooLong,
    #[error("a block of a kind this version does not know cannot be written")]
    Unsupported,
}

/// How a button looks: the usual outline, the one main choice, or a destructive one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonStyle {
    #[default]
    Default,
    Primary,
    Danger,
}

impl ButtonStyle {
    fn is_default(&self) -> bool {
        *self == ButtonStyle::Default
    }
}

/// A button. With `url` it opens that link; without, pressing it tells the app that sent the
/// message, which is given `action_id` and `value`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Button {
    pub action_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "ButtonStyle::is_default")]
    pub style: ButtonStyle,
}

impl Button {
    /// Whether pressing it goes to the app (rather than opening a link).
    pub fn is_action(&self) -> bool {
        self.url.is_none()
    }
}

/// One block of rich content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RichBlock {
    /// A short title, in plain text.
    Header { text: String },
    /// Text, a grid of short fields under it, and a button beside it; at least one of the three.
    Section {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<MessageDocument>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<MessageDocument>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accessory: Option<Button>,
    },
    /// A thin rule.
    Divider,
    /// A line of small, quiet text items.
    Context { items: Vec<MessageDocument> },
    /// A row of buttons.
    Actions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block_id: Option<String>,
        buttons: Vec<Button>,
    },
    /// A block this build cannot show (from a newer server). Never written.
    #[serde(other)]
    Unsupported,
}

impl RichBlock {
    /// The buttons in this block, in order.
    pub fn buttons(&self) -> Vec<&Button> {
        match self {
            RichBlock::Section {
                accessory: Some(button),
                ..
            } => vec![button],
            RichBlock::Actions { buttons, .. } => buttons.iter().collect(),
            _ => Vec::new(),
        }
    }
}

/// A block as read: one this build understands, or anything else.
#[derive(Deserialize)]
#[serde(untagged)]
enum ReadBlock {
    Known(RichBlock),
    Other(IgnoredAny),
}

#[derive(Deserialize)]
struct ReadWire {
    #[serde(default)]
    #[allow(dead_code)]
    version: u32,
    blocks: Vec<ReadBlock>,
}

#[derive(Serialize)]
struct WriteWire<'a> {
    version: u32,
    blocks: &'a [RichBlock],
}

/// Rich content: blocks drawn in place of a message's text by clients that know them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichContent {
    blocks: Vec<RichBlock>,
}

impl Serialize for RichContent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WriteWire {
            version: RICH_VERSION,
            blocks: &self.blocks,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RichContent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ReadWire::deserialize(deserializer)?;
        let blocks = wire
            .blocks
            .into_iter()
            .map(|block| match block {
                ReadBlock::Known(block) => block,
                ReadBlock::Other(_) => RichBlock::Unsupported,
            })
            .collect();
        Ok(Self { blocks })
    }
}

impl RichContent {
    /// Checks blocks and makes content of them.
    pub fn new(blocks: Vec<RichBlock>) -> Result<Self, RichError> {
        validate(&blocks)?;
        Ok(Self { blocks })
    }

    pub fn blocks(&self) -> &[RichBlock] {
        &self.blocks
    }

    /// Whether some block could not be read by this build.
    pub fn has_unsupported(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| matches!(block, RichBlock::Unsupported))
    }

    /// The button with `action_id` in block `block`, if there is one.
    pub fn button(&self, block: usize, action_id: &str) -> Option<&Button> {
        self.blocks
            .get(block)?
            .buttons()
            .into_iter()
            .find(|button| button.action_id == action_id)
    }

    /// The block id a block was given, if any.
    pub fn block_id(&self, block: usize) -> Option<&str> {
        match self.blocks.get(block)? {
            RichBlock::Actions { block_id, .. } => block_id.as_deref(),
            _ => None,
        }
    }

    /// Whether any button goes to the app.
    pub fn has_actions(&self) -> bool {
        self.blocks
            .iter()
            .flat_map(RichBlock::buttons)
            .any(Button::is_action)
    }
}

fn one_line(text: &str, max: usize) -> bool {
    let count = text.chars().count();
    count > 0 && count <= max && !text.trim().is_empty() && !text.chars().any(char::is_control)
}

fn check_button(button: &Button) -> Result<usize, RichError> {
    let ok = one_line(&button.text, MAX_BUTTON_TEXT_CHARS)
        && one_line(&button.action_id, MAX_ACTION_ID_CHARS)
        && button
            .value
            .as_ref()
            .is_none_or(|value| value.chars().count() <= MAX_VALUE_CHARS)
        && button.url.as_deref().is_none_or(link_is_allowed);
    if !ok {
        return Err(RichError::InvalidButton);
    }
    Ok(button.text.len() + button.value.as_ref().map_or(0, String::len))
}

fn document_bytes(document: &MessageDocument) -> usize {
    document.plain_text().len()
}

fn validate(blocks: &[RichBlock]) -> Result<(), RichError> {
    if blocks.is_empty() {
        return Err(RichError::Empty);
    }
    if blocks.len() > MAX_RICH_BLOCKS {
        return Err(RichError::TooManyBlocks);
    }
    let mut bytes = 0;
    for block in blocks {
        match block {
            RichBlock::Header { text } => {
                if !one_line(text, MAX_HEADER_CHARS) {
                    return Err(RichError::InvalidHeader);
                }
                bytes += text.len();
            }
            RichBlock::Section {
                text,
                fields,
                accessory,
            } => {
                if text.is_none() && fields.is_empty() && accessory.is_none() {
                    return Err(RichError::EmptyBlock);
                }
                if fields.len() > MAX_FIELDS {
                    return Err(RichError::TooManyItems);
                }
                bytes += text.as_ref().map_or(0, document_bytes);
                bytes += fields.iter().map(document_bytes).sum::<usize>();
                if let Some(button) = accessory {
                    bytes += check_button(button)?;
                }
            }
            RichBlock::Divider => {}
            RichBlock::Context { items } => {
                if items.is_empty() {
                    return Err(RichError::EmptyBlock);
                }
                if items.len() > MAX_CONTEXT_ITEMS {
                    return Err(RichError::TooManyItems);
                }
                bytes += items.iter().map(document_bytes).sum::<usize>();
            }
            RichBlock::Actions { block_id, buttons } => {
                if buttons.is_empty() {
                    return Err(RichError::EmptyBlock);
                }
                if buttons.len() > MAX_BUTTONS {
                    return Err(RichError::TooManyItems);
                }
                if let Some(id) = block_id
                    && !one_line(id, MAX_ACTION_ID_CHARS)
                {
                    return Err(RichError::InvalidBlockId);
                }
                for (n, button) in buttons.iter().enumerate() {
                    bytes += check_button(button)?;
                    if buttons[..n]
                        .iter()
                        .any(|other| other.action_id == button.action_id)
                    {
                        return Err(RichError::DuplicateAction);
                    }
                }
            }
            RichBlock::Unsupported => return Err(RichError::Unsupported),
        }
    }
    if bytes > MAX_TEXT_BYTES {
        return Err(RichError::TooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> MessageDocument {
        MessageDocument::from_markdown(text).unwrap()
    }

    fn button(id: &str, text: &str) -> Button {
        Button {
            action_id: id.into(),
            text: text.into(),
            value: Some("v".into()),
            url: None,
            style: ButtonStyle::Primary,
        }
    }

    fn sample() -> RichContent {
        RichContent::new(vec![
            RichBlock::Header {
                text: "Deploy #42".into(),
            },
            RichBlock::Section {
                text: Some(doc("*api* to production")),
                fields: vec![doc("*Env*\nprod"), doc("*By*\nalice")],
                accessory: None,
            },
            RichBlock::Divider,
            RichBlock::Context {
                items: vec![doc("3 minutes ago")],
            },
            RichBlock::Actions {
                block_id: Some("deploy".into()),
                buttons: vec![
                    button("approve", "Approve"),
                    Button {
                        url: Some("https://ci.example.com/42".into()),
                        style: ButtonStyle::Default,
                        value: None,
                        ..button("open", "Open")
                    },
                ],
            },
        ])
        .unwrap()
    }

    #[test]
    fn round_trips_and_finds_buttons() {
        let content = sample();
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.starts_with(r#"{"version":1,"blocks":[{"type":"header""#));
        let back: RichContent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, content);
        assert!(back.button(4, "approve").unwrap().is_action());
        assert!(!back.button(4, "open").unwrap().is_action());
        assert!(back.button(3, "approve").is_none());
        assert_eq!(back.block_id(4), Some("deploy"));
        assert!(back.has_actions());
        assert!(!back.has_unsupported());
    }

    #[test]
    fn blocks_from_a_newer_server_are_unsupported_not_errors() {
        let json = r#"{"version":7,"blocks":[
            {"type":"header","text":"Hi"},
            {"type":"carousel","cards":[1,2]},
            {"type":"section","text":{"version":99,"blocks":[]}},
            {"type":"actions","buttons":[{"action_id":"a","text":"A","style":"sparkly"}]}
        ]}"#;
        let content: RichContent = serde_json::from_str(json).unwrap();
        assert_eq!(content.blocks().len(), 4);
        assert_eq!(content.blocks()[0], RichBlock::Header { text: "Hi".into() });
        assert!(
            content.blocks()[1..]
                .iter()
                .all(|block| *block == RichBlock::Unsupported)
        );
        assert!(content.has_unsupported());
    }

    #[test]
    fn bad_content_is_refused_when_made() {
        assert_eq!(RichContent::new(vec![]), Err(RichError::Empty));
        assert_eq!(
            RichContent::new(vec![RichBlock::Header {
                text: "a\nb".into()
            }]),
            Err(RichError::InvalidHeader)
        );
        assert_eq!(
            RichContent::new(vec![RichBlock::Section {
                text: None,
                fields: vec![],
                accessory: None
            }]),
            Err(RichError::EmptyBlock)
        );
        assert_eq!(
            RichContent::new(vec![RichBlock::Actions {
                block_id: None,
                buttons: vec![button("a", "A"), button("a", "B")]
            }]),
            Err(RichError::DuplicateAction)
        );
        assert_eq!(
            RichContent::new(vec![RichBlock::Actions {
                block_id: None,
                buttons: vec![Button {
                    url: Some("javascript:alert(1)".into()),
                    ..button("a", "A")
                }]
            }]),
            Err(RichError::InvalidButton)
        );
        assert_eq!(
            RichContent::new(vec![RichBlock::Actions {
                block_id: None,
                buttons: vec![button("a", &"x".repeat(MAX_BUTTON_TEXT_CHARS + 1))]
            }]),
            Err(RichError::InvalidButton)
        );
        assert_eq!(
            RichContent::new(vec![RichBlock::Unsupported]),
            Err(RichError::Unsupported)
        );
        let many = vec![RichBlock::Divider; MAX_RICH_BLOCKS + 1];
        assert_eq!(RichContent::new(many), Err(RichError::TooManyBlocks));
    }
}
