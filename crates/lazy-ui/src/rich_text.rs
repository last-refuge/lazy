//! Draws a message document: styled runs, links, code blocks, quotes, and lists.
//!
//! The document is already structured (the core parsed the composer's text once), so nothing here
//! parses markup. Turning a paragraph's inline items into text plus style runs is a pure function
//! ([`lay_out`]) and cheap enough to repeat for each visible row on every frame; no I/O happens.
//!
//! Links are the one interactive part. A click on `http`, `https` or `mailto` opens the system
//! handler; a `lazy://` permalink is handed to the app, which asks the core to jump to the
//! message. Either way the view only reports the click through the [`LinkHandler`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Bounds, ClipboardItem, ElementId, FontFeatures, FontStyle, FontWeight, Hsla,
    InteractiveText, Pixels, Point, RenderImage, Role, SharedString, StrikethroughStyle,
    StyledText, TextLayout, TextRun, UnderlineStyle, Window, canvas, div, font, prelude::*, px,
};
use lazy_client_core::ItemKey;
use lazy_domain::{Block, EmojiId, Inline, Marks, MentionTarget, MessageDocument, Permalink};

use crate::fonts::{FAMILY, MONO_FAMILY};
use crate::theme::Theme;

/// Spaces a tab is drawn as. The text keeps its tab; only what is shown changes.
const TAB_WIDTH: usize = 4;

/// What a clicked link asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// An address for the system to open: `http`, `https`, or `mailto`.
    External(String),
    /// A message in this app.
    Permalink(Permalink),
}

impl LinkTarget {
    /// Classifies a link address from a document. Returns `None` for anything the app should not
    /// act on (a `lazy://` address that is not a permalink, or a scheme a document cannot hold).
    pub fn classify(url: &str) -> Option<Self> {
        if url.starts_with("lazy:") {
            return Permalink::parse(url).map(Self::Permalink);
        }
        let (scheme, _) = url.split_once(':')?;
        matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "mailto"
        )
        .then(|| Self::External(url.to_owned()))
    }
}

/// Called when a link is clicked.
pub type LinkHandler = Rc<dyn Fn(LinkTarget, &mut Window, &mut App)>;

/// The handler for views that have no app model to forward permalinks to: external addresses open
/// in the system handler and permalinks do nothing.
pub fn open_external_only() -> LinkHandler {
    Rc::new(|target, _window, cx| {
        if let LinkTarget::External(url) = target {
            cx.open_url(&url);
        }
    })
}

/// How a mention reads on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionLook {
    /// What the pill says, starting with `@`: the person's name, the group's name, `@channel`, or
    /// the handle as written when the person or group is not known any more.
    pub text: String,
    /// The mention notifies the person looking, so it is drawn stronger.
    pub aimed_at_me: bool,
}

/// A custom emoji's picture, decoded and ready to paint.
#[derive(Clone)]
pub struct EmojiPicture {
    pub image: Arc<RenderImage>,
    /// Width over height.
    pub aspect: f32,
}

impl EmojiPicture {
    /// How tall the picture is drawn, in em: a little more than the line's text.
    pub const HEIGHT_EM: f32 = 1.25;

    /// The size to draw it at, in em: [`Self::HEIGHT_EM`] tall, as wide as its aspect makes it,
    /// at most twice as wide as tall (and at least half).
    pub fn size_em(&self) -> (f32, f32) {
        let width =
            (Self::HEIGHT_EM * self.aspect).clamp(Self::HEIGHT_EM * 0.5, Self::HEIGHT_EM * 2.0);
        (width, Self::HEIGHT_EM)
    }
}

impl std::fmt::Debug for EmojiPicture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EmojiPicture({:.2})", self.aspect)
    }
}

impl PartialEq for EmojiPicture {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.image, &other.image)
    }
}

impl Eq for EmojiPicture {}

/// How a custom emoji reads on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiLook {
    /// `:name:`: what a copy writes, and what is drawn, muted, until (or unless) the picture can
    /// be.
    pub label: String,
    /// The picture, once it has been fetched and decoded. `None` while it loads, when it could not
    /// be, and for an emoji that was deleted or is not known.
    pub picture: Option<EmojiPicture>,
}

/// Turns a mention in a document into what is drawn for it. The timeline supplies one backed by
/// the client core's people and groups; rendering itself looks nothing up.
pub trait MentionLabels {
    fn look(&self, target: MentionTarget, handle: &str) -> MentionLook;

    /// How the custom emoji `id` reads. `name` is what the document called it when it was written,
    /// which is all there is for an emoji nobody here knows.
    fn emoji(&self, id: EmojiId, name: &str) -> EmojiLook {
        let _ = id;
        EmojiLook {
            label: format!(":{name}:"),
            picture: None,
        }
    }
}

/// Shows every mention as `@handle`, aimed at no one: for views with no people to look up.
#[derive(Debug, Clone, Copy, Default)]
pub struct HandlesOnly;

impl MentionLabels for HandlesOnly {
    fn look(&self, _target: MentionTarget, handle: &str) -> MentionLook {
        MentionLook {
            text: format!("@{handle}"),
            aimed_at_me: false,
        }
    }
}

/// A run of text sharing one style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    /// Length in bytes of the text drawn.
    pub len: usize,
    pub marks: Marks,
    pub link: bool,
    /// A mention, and whether it notifies the person looking.
    pub mention: Option<bool>,
    /// Muted text: a custom emoji's `:name:` standing in for a picture it does not have.
    pub muted: bool,
}

/// What a mention pill's text is padded with, so its background reaches past the letters. A
/// no-break space, so the pill never wraps apart from its edges.
const PILL_PADDING: char = '\u{a0}';

/// How a stretch of the drawn text corresponds to the text that is selected and copied.
///
/// They differ in two places: a tab is drawn as spaces, and a mention's pill is padded on both
/// sides, while the copied text keeps the tab and holds only the pill's label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    /// Byte range of the drawn text.
    pub drawn: Range<usize>,
    /// Byte range of the copied text.
    pub flat: Range<usize>,
    /// The two ranges are not the same length: positions inside map to one end.
    pub atomic: bool,
}

/// One paragraph's text, with the styles and links over it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Line {
    pub text: String,
    pub spans: Vec<Span>,
    /// Byte ranges of `text` that are links, with the address each opens.
    pub links: Vec<(Range<usize>, String)>,
    /// The text as it is selected and copied: tabs kept, a mention as its label.
    pub flat: String,
    /// How `text` and `flat` line up, in order.
    pub segments: Vec<Segment>,
    /// The custom emoji drawn as pictures, in order: where the room for each is in `text`.
    pub emoji: Vec<EmojiAt>,
}

/// A custom emoji's picture and the room reserved for it in a line's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EmojiAt {
    /// Byte range of the reserving characters in the drawn text.
    pub drawn: Range<usize>,
    pub picture: EmojiPicture,
    /// `:name:`, which a screen reader is told about.
    pub label: String,
}

impl Line {
    fn push(&mut self, text: &str, marks: Marks, link: Option<&str>) {
        self.push_styled(text, marks, link, None);
    }

    /// Adds `len` bytes of drawn text in one style, merging it into the span before it when that
    /// has the same style. Two mentions never share a span: each is its own pill.
    fn add_span(&mut self, len: usize, marks: Marks, link: bool, mention: Option<bool>) {
        self.add_span_muted(len, marks, link, mention, false)
    }

    fn add_span_muted(
        &mut self,
        len: usize,
        marks: Marks,
        link: bool,
        mention: Option<bool>,
        muted: bool,
    ) {
        match self.spans.last_mut() {
            Some(last)
                if last.marks == marks
                    && last.link == link
                    && last.mention.is_none()
                    && mention.is_none()
                    && last.muted == muted =>
            {
                last.len += len
            }
            _ => self.spans.push(Span {
                len,
                marks,
                link,
                mention,
                muted,
            }),
        }
    }

    fn push_styled(&mut self, text: &str, marks: Marks, link: Option<&str>, mention: Option<bool>) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        for (index, piece) in text.split('\t').enumerate() {
            if index > 0 {
                // A tab is drawn as spaces and copied as the tab.
                let (drawn, flat) = (self.text.len(), self.flat.len());
                self.text.push_str(&" ".repeat(TAB_WIDTH));
                self.flat.push('\t');
                self.segments.push(Segment {
                    drawn: drawn..self.text.len(),
                    flat: flat..self.flat.len(),
                    atomic: true,
                });
            }
            if !piece.is_empty() {
                let (drawn, flat) = (self.text.len(), self.flat.len());
                self.text.push_str(piece);
                self.flat.push_str(piece);
                self.segments.push(Segment {
                    drawn: drawn..self.text.len(),
                    flat: flat..self.flat.len(),
                    atomic: false,
                });
            }
        }
        let len = self.text.len() - start;
        if let Some(url) = link {
            self.links.push((start..start + len, url.to_owned()));
        }
        self.add_span(len, marks, link.is_some(), mention);
    }

    /// Adds a custom emoji. With its picture, room for the picture is reserved in the drawn text
    /// and the copied text holds `:name:`; without, `:name:` is drawn (muted, in the marks it was
    /// written with) and copied.
    fn push_emoji(&mut self, look: &EmojiLook, marks: Marks) {
        let flat = self.flat.len();
        self.flat.push_str(&look.label);
        let flat = flat..self.flat.len();
        let drawn_start = self.text.len();
        match &look.picture {
            Some(picture) => {
                let (width_em, _) = picture.size_em();
                let (room, _) = crate::emoji_spacing::reserve(width_em);
                self.text.push_str(&room);
                let drawn = drawn_start..self.text.len();
                self.segments.push(Segment {
                    drawn: drawn.clone(),
                    flat,
                    atomic: true,
                });
                self.emoji.push(EmojiAt {
                    drawn,
                    picture: picture.clone(),
                    label: look.label.clone(),
                });
                self.add_span(self.text.len() - drawn_start, Marks::NONE, false, None);
            }
            None => {
                self.text.push_str(&look.label);
                self.segments.push(Segment {
                    drawn: drawn_start..self.text.len(),
                    flat,
                    atomic: false,
                });
                self.add_span_muted(self.text.len() - drawn_start, marks, false, None, true);
            }
        }
    }

    /// Adds a mention's pill: its label, with padding on both sides that is drawn but not copied.
    fn push_mention(&mut self, label: &str, marks: Marks, aimed_at_me: bool) {
        let pad = PILL_PADDING.len_utf8();
        let flat = self.flat.len();
        let drawn = self.text.len();
        self.text.push(PILL_PADDING);
        self.segments.push(Segment {
            drawn: drawn..drawn + pad,
            flat: flat..flat,
            atomic: true,
        });
        let label_start = self.text.len();
        self.text.push_str(label);
        self.flat.push_str(label);
        self.segments.push(Segment {
            drawn: label_start..self.text.len(),
            flat: flat..self.flat.len(),
            atomic: false,
        });
        let trailing = self.text.len();
        self.text.push(PILL_PADDING);
        self.segments.push(Segment {
            drawn: trailing..trailing + pad,
            flat: self.flat.len()..self.flat.len(),
            atomic: true,
        });
        self.add_span(self.text.len() - drawn, marks, false, Some(aimed_at_me));
    }

    /// The copied-text position of byte `drawn` of the drawn text. Inside a tab or a pill's
    /// padding it is the nearer edge.
    pub fn drawn_to_flat(&self, drawn: usize) -> usize {
        for segment in &self.segments {
            if drawn >= segment.drawn.end {
                continue;
            }
            let into = drawn.saturating_sub(segment.drawn.start);
            return if segment.atomic {
                if into * 2 <= segment.drawn.len() {
                    segment.flat.start
                } else {
                    segment.flat.end
                }
            } else {
                segment.flat.start + into
            };
        }
        self.flat.len()
    }

    /// A drawn position for copied-text position `flat`. Where several do (either side of a
    /// pill's padding), `later` picks the last, so a selection that ends at a pill's end takes
    /// its trailing padding with it, and one that starts at its start takes the leading padding.
    pub fn flat_to_drawn(&self, flat: usize, later: bool) -> usize {
        let mut best: Option<usize> = None;
        let mut consider = |position: usize| {
            best = Some(match best {
                None => position,
                Some(current) if later => current.max(position),
                Some(current) => current.min(position),
            });
        };
        for segment in &self.segments {
            if !(segment.flat.start..=segment.flat.end).contains(&flat) {
                continue;
            }
            if segment.atomic {
                if flat == segment.flat.start {
                    consider(segment.drawn.start);
                }
                if flat == segment.flat.end {
                    consider(segment.drawn.end);
                }
            } else {
                consider(segment.drawn.start + (flat - segment.flat.start));
            }
        }
        best.unwrap_or(self.text.len())
    }
}

/// Flattens inline items into text, style spans, and link ranges.
pub(crate) fn lay_out(inlines: &[Inline], mentions: &dyn MentionLabels) -> Line {
    let mut line = Line::default();
    for inline in inlines {
        match inline {
            Inline::Text { text, marks } => line.push(text, *marks, None),
            Inline::Link { url, text, marks } => line.push(text, *marks, Some(url)),
            Inline::LineBreak => line.push("\n", Marks::NONE, None),
            Inline::Mention {
                target,
                handle,
                marks,
            } => {
                let look = mentions.look(*target, handle);
                line.push_mention(&look.text, *marks, look.aimed_at_me);
            }
            Inline::Emoji { id, name, marks } => {
                line.push_emoji(&mentions.emoji(*id, name), *marks);
            }
        }
    }
    line
}

/// How strongly a link's underline is drawn, relative to the link's color.
const LINK_UNDERLINE_OPACITY: f32 = 0.45;

/// The GPUI style runs for a laid-out line. `color` is the text color for ordinary text.
pub(crate) fn runs(line: &Line, theme: &Theme, color: Hsla) -> Vec<TextRun> {
    line.spans
        .iter()
        .map(|span| {
            let marks = span.marks;
            let mut face = font(if marks.code { MONO_FAMILY } else { FAMILY });
            if marks.code {
                // Code is shown as typed: no joined arrows or comparison signs.
                face.features = FontFeatures::disable_ligatures();
            }
            face.weight = if marks.bold || span.mention == Some(true) {
                if marks.code {
                    FontWeight::BOLD
                } else {
                    FontWeight::SEMIBOLD
                }
            } else {
                FontWeight::NORMAL
            };
            face.style = if marks.italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            };
            let color = if span.link {
                theme.colors.accent
            } else if span.mention.is_some() {
                theme.colors.mention_text
            } else if span.muted {
                theme.colors.text_muted
            } else {
                color
            };
            let background = match span.mention {
                Some(true) => Some(theme.colors.mention_me_surface),
                Some(false) => Some(theme.colors.mention_surface),
                None => marks.code.then_some(theme.colors.code_surface),
            };
            TextRun {
                len: span.len,
                font: face,
                color,
                background_color: background,
                // A link keeps an underline (its color alone would not mark it), drawn fainter
                // than the text so that a line of several links still reads as text.
                underline: span.link.then_some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(color.opacity(LINK_UNDERLINE_OPACITY)),
                    wavy: false,
                }),
                strikethrough: marks.strike.then_some(StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(color),
                }),
            }
        })
        .collect()
}

/// What to draw and how to react to clicks.
pub struct DocumentProps<'a> {
    pub document: &'a MessageDocument,
    pub theme: &'a Theme,
    /// Distinguishes this document's interactive pieces from other documents on screen.
    pub id: ElementId,
    pub on_link: LinkHandler,
    /// How mentions read, and which are aimed at the person looking.
    pub mentions: &'a dyn MentionLabels,
    /// Text color of ordinary text.
    pub color: Hsla,
    /// Present when the text can be selected: where to report what was drawn, and which part of
    /// it is selected now.
    pub select: Option<SelectProps<'a>>,
}

/// Selection for one message: what to draw as selected, and where to record where text was drawn.
pub struct SelectProps<'a> {
    /// The message the document belongs to.
    pub key: ItemKey,
    /// The selected part of the message's selectable text ([`message_text`]), if any.
    pub range: Option<Range<usize>>,
    /// Receives the drawn text of each paragraph, for turning a pointer position into an offset.
    pub sink: &'a HitSink,
    /// Counts paints of the list: a paragraph records the count when it is painted, which is how
    /// text that is on screen is told from text that was drawn earlier and has since scrolled away.
    pub epoch: &'a Rc<Cell<u64>>,
}

/// Where each unit of a message's text was last drawn, by message and unit.
pub type HitSink = RefCell<HashMap<(ItemKey, usize), UnitHit>>;

/// One drawn paragraph, list item, or code block, for hit testing.
#[derive(Clone)]
pub struct UnitHit {
    layout: TextLayout,
    line: Rc<Line>,
    /// Bytes of the unit's selectable text before the drawn text (a list marker).
    prefix: usize,
    /// Where the unit's selectable text starts in the message's.
    start: usize,
    /// The paint count of the last time it was painted; see [`SelectProps::epoch`].
    painted: Rc<Cell<u64>>,
}

impl UnitHit {
    /// Where the text was last painted, in window coordinates. Empty before it has been.
    pub fn bounds(&self) -> Bounds<Pixels> {
        self.layout.bounds()
    }

    /// Whether the text was painted in the paint whose count is `epoch`: it is on screen.
    pub fn painted_in(&self, epoch: u64) -> bool {
        self.painted.get() == epoch
    }

    /// The offset into the message's selectable text nearest to `point`.
    pub fn offset_at(&self, point: Point<Pixels>) -> usize {
        let drawn = match self.layout.index_for_position(point) {
            Ok(index) | Err(index) => index,
        };
        self.start + self.prefix + self.line.drawn_to_flat(drawn)
    }

    /// Whether `offset` (into the message's selectable text) is in this unit's text.
    pub fn contains_offset(&self, offset: usize) -> bool {
        (self.first_offset()..=self.last_offset()).contains(&offset)
    }

    /// Where the text at `offset` was drawn, in window coordinates: the middle of its line. For
    /// tests that press and drag on text.
    pub fn point_for(&self, offset: usize) -> Option<Point<Pixels>> {
        let local = offset.checked_sub(self.first_offset())?;
        let drawn = self
            .line
            .flat_to_drawn(local.min(self.line.flat.len()), false);
        let at = self.layout.position_for_index(drawn)?;
        // A pixel in, so the point is unmistakably over this character.
        Some(gpui::point(
            at.x + px(1.),
            at.y + self.layout.line_height() / 2.0,
        ))
    }

    /// The offset of the start of the unit's text.
    pub fn first_offset(&self) -> usize {
        self.start + self.prefix
    }

    /// The offset just past the unit's text.
    pub fn last_offset(&self) -> usize {
        self.start + self.prefix + self.line.flat.len()
    }
}

/// One run of text a selection can cover: a paragraph, a list item, or a code block.
pub(crate) struct Unit {
    /// A list marker, selectable and copied with the item but not part of the drawn paragraph.
    pub prefix: String,
    pub line: Rc<Line>,
}

impl Unit {
    pub fn flat_len(&self) -> usize {
        self.prefix.len() + self.line.flat.len()
    }
}

/// The units of `document` in drawing order: each paragraph, quoted paragraph, list item, and
/// code block.
pub(crate) fn units(document: &MessageDocument, mentions: &dyn MentionLabels) -> Vec<Unit> {
    let mut out = Vec::new();
    let paragraph = |inlines: &[Inline], prefix: String| Unit {
        prefix,
        line: Rc::new(lay_out(inlines, mentions)),
    };
    for block in document.blocks() {
        match block {
            Block::Paragraph { inlines } => out.push(paragraph(inlines, String::new())),
            Block::CodeBlock { text, .. } => {
                let mut line = Line::default();
                line.push(text, Marks::NONE, None);
                out.push(Unit {
                    prefix: String::new(),
                    line: Rc::new(line),
                });
            }
            Block::Quote { paragraphs } => {
                for inlines in paragraphs {
                    out.push(paragraph(inlines, String::new()));
                }
            }
            Block::BulletList { items } => {
                for inlines in items {
                    out.push(paragraph(inlines, "• ".to_owned()));
                }
            }
            Block::OrderedList { start, items } => {
                for (n, inlines) in items.iter().enumerate() {
                    out.push(paragraph(
                        inlines,
                        format!("{}. ", u64::from(*start) + n as u64),
                    ));
                }
            }
        }
    }
    out
}

/// Where each unit's selectable text starts in the message's, and how long the whole is. Units
/// are separated by one line break.
pub(crate) fn unit_starts(units: &[Unit]) -> (Vec<usize>, usize) {
    let mut starts = Vec::with_capacity(units.len());
    let mut at = 0;
    for (index, unit) in units.iter().enumerate() {
        if index > 0 {
            at += 1;
        }
        starts.push(at);
        at += unit.flat_len();
    }
    (starts, at)
}

/// A message's selectable text: what a copy of the whole message writes. Mentions read as the
/// name the pill shows, links as their text, code as written, and list items keep their markers.
pub fn message_text(document: &MessageDocument, mentions: &dyn MentionLabels) -> String {
    let units = units(document, mentions);
    let mut out = String::new();
    for (index, unit) in units.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&unit.prefix);
        out.push_str(&unit.line.flat);
    }
    out
}

/// The part of `selected` (a range of the message's text) that falls inside the unit starting at
/// `start`, as a range of the unit's own selectable text after its prefix.
fn within_unit(selected: &Range<usize>, start: usize, unit: &Unit) -> Option<Range<usize>> {
    let body = start + unit.prefix.len();
    let lo = selected.start.max(body);
    let hi = selected.end.min(body + unit.line.flat.len());
    (lo < hi).then(|| lo - body..hi - body)
}

/// Gives `range` (bytes of the drawn text) the selection background, splitting the runs it cuts.
fn highlight(runs: Vec<TextRun>, range: Range<usize>, background: Hsla) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut at = 0;
    for run in runs {
        let (start, end) = (at, at + run.len);
        at = end;
        let (lo, hi) = (range.start.max(start), range.end.min(end));
        if lo >= hi {
            out.push(run);
            continue;
        }
        for (from, to, selected) in [(start, lo, false), (lo, hi, true), (hi, end, false)] {
            if from < to {
                let mut piece = run.clone();
                piece.len = to - from;
                if selected {
                    piece.background_color = Some(background);
                }
                out.push(piece);
            }
        }
    }
    out
}

/// Where the drawing walk is: the units of the message, and the next one to draw.
struct Walk {
    units: Vec<Unit>,
    starts: Vec<usize>,
    next: Cell<usize>,
}

impl Walk {
    fn take(&self) -> usize {
        let index = self.next.get();
        self.next.set(index + 1);
        index
    }
}

/// Draws the document's blocks, one under another.
pub fn render_document(props: DocumentProps<'_>) -> AnyElement {
    let units = units(props.document, props.mentions);
    let (starts, _) = unit_starts(&units);
    let walk = Walk {
        units,
        starts,
        next: Cell::new(0),
    };
    let mut column = div().flex().flex_col().gap(px(6.)).min_w_0().w_full();
    for (index, block) in props.document.blocks().iter().enumerate() {
        let id = SharedString::from(format!("{}-b{index}", props.id));
        column = column.child(match block {
            Block::Paragraph { inlines } => paragraph(inlines, id, &props, &walk),
            Block::CodeBlock { language, text } => {
                code_block(language.as_deref(), text, id, &props, &walk)
            }
            Block::Quote { paragraphs } => quote(paragraphs, &id, &props, &walk),
            Block::BulletList { items } => list(items, &id, &props, &walk),
            Block::OrderedList { items, .. } => list(items, &id, &props, &walk),
        });
    }
    column.into_any_element()
}

/// The runs and styled text of unit `index`, with its selection drawn and its place recorded.
fn drawn_text(
    index: usize,
    walk: &Walk,
    props: &DocumentProps<'_>,
    base_runs: Vec<TextRun>,
) -> (StyledText, Option<AnyElement>) {
    let unit = &walk.units[index];
    let line = &unit.line;
    let mut runs = base_runs;
    if let Some(select) = &props.select
        && let Some(selected) = select
            .range
            .as_ref()
            .and_then(|range| within_unit(range, walk.starts[index], unit))
    {
        let drawn =
            line.flat_to_drawn(selected.start, false)..line.flat_to_drawn(selected.end, true);
        runs = highlight(runs, drawn, props.theme.colors.selection);
    }
    let runs = crate::emoji_font::with_emoji_font(&line.text, runs);
    let styled = StyledText::new(line.text.clone()).with_runs(runs);
    let emoji = (!line.emoji.is_empty()).then(|| line.emoji.clone());
    let font_px = f32::from(props.theme.type_scale.body);
    let paint_emoji = emoji.map(|emoji| {
        let layout = styled.layout().clone();
        move |window: &mut gpui::Window| {
            let line_height = layout.line_height();
            for at in &emoji {
                let Some(origin) = layout.position_for_index(at.drawn.start) else {
                    continue;
                };
                let (w_em, h_em) = at.picture.size_em();
                let size = gpui::size(px(w_em * font_px), px(h_em * font_px));
                let top = origin.y + (line_height - size.height) / 2.0;
                let bounds = Bounds::new(gpui::point(origin.x, top), size);
                window
                    .paint_image(
                        bounds,
                        bounds,
                        gpui::Corners::default(),
                        at.picture.image.clone(),
                        0,
                        false,
                    )
                    .ok();
            }
        }
    });
    let stamp = props.select.as_ref().map(|select| {
        let painted = Rc::new(Cell::new(0));
        select.sink.borrow_mut().insert(
            (select.key, index),
            UnitHit {
                layout: styled.layout().clone(),
                line: line.clone(),
                prefix: unit.prefix.len(),
                start: walk.starts[index],
                painted: painted.clone(),
            },
        );
        // Zero size, painted with the text: records that this text is on screen.
        let epoch = select.epoch.clone();
        canvas(|_, _, _| (), move |_, (), _, _| painted.set(epoch.get()))
            .absolute()
            .size_0()
            .into_any_element()
    });
    // The custom emoji pictures go over their reserved room once the text has been painted.
    let stamp = match (stamp, paint_emoji) {
        (stamp, None) => stamp,
        (stamp, Some(paint)) => Some(
            div()
                .absolute()
                .size_0()
                .children(stamp)
                .child(
                    canvas(|_, _, _| (), move |_, (), window, _| paint(window))
                        .absolute()
                        .size_0(),
                )
                .into_any_element(),
        ),
    };
    (styled, stamp)
}

/// One paragraph of inline items, as text with clickable links.
fn paragraph(
    inlines: &[Inline],
    id: SharedString,
    props: &DocumentProps<'_>,
    walk: &Walk,
) -> AnyElement {
    let index = walk.take();
    let theme = props.theme;
    let line = walk.units[index].line.clone();
    let has_mention = line.spans.iter().any(|span| span.mention.is_some())
        || inlines
            .iter()
            .any(|inline| matches!(inline, Inline::Emoji { .. }));
    let (styled, stamp) = drawn_text(index, walk, props, runs(&line, theme, props.color));
    let inner: AnyElement = if line.links.is_empty() {
        styled.into_any_element()
    } else {
        let (ranges, urls): (Vec<_>, Vec<_>) = line.links.iter().cloned().unzip();
        let on_link = props.on_link.clone();
        InteractiveText::new(ElementId::Name(id.clone()), styled)
            .on_click(ranges, move |index, window, cx| {
                if let Some(target) = LinkTarget::classify(&urls[index]) {
                    on_link(target, window, cx);
                }
            })
            .into_any_element()
    };
    let body = div()
        .debug_selector(|| "message-paragraph".into())
        .relative()
        .text_size(theme.type_scale.body)
        .line_height(theme.type_scale.line_height_body)
        .text_color(props.color)
        .child(inner)
        .children(stamp);
    if has_mention {
        // What the pill shows is a name; what is read aloud is the handle that was written.
        body.id(ElementId::Name(format!("{id}-text").into()))
            .role(Role::Paragraph)
            .aria_label(plain_text(inlines))
            .into_any_element()
    } else {
        body.into_any_element()
    }
}

/// The text of inline items as it is copied and read aloud: mentions as `@handle`.
pub fn plain_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text { text, .. } | Inline::Link { text, .. } => out.push_str(text),
            Inline::LineBreak => out.push('\n'),
            Inline::Mention { handle, .. } => {
                out.push('@');
                out.push_str(handle);
            }
            // Read aloud as the word it is, not as colons and underscores.
            Inline::Emoji { name, .. } => {
                out.push(':');
                out.push_str(name);
                out.push_str(": emoji");
            }
        }
    }
    out
}

/// Preformatted text in a framed block that wraps long lines. Hovering the block reveals Copy.
fn code_block(
    language: Option<&str>,
    text: &str,
    id: SharedString,
    props: &DocumentProps<'_>,
    walk: &Walk,
) -> AnyElement {
    let theme = props.theme;
    let index = walk.take();
    let line = walk.units[index].line.clone();
    // Code is shown as typed: one mono run, no joined arrows or comparison signs.
    let mut face = font(MONO_FAMILY);
    face.features = FontFeatures::disable_ligatures();
    let base = vec![TextRun {
        len: line.text.len(),
        font: face,
        color: theme.colors.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    }];
    let (shown, stamp) = drawn_text(index, walk, props, base);
    let copy_text = text.to_owned();
    let group: SharedString = format!("{id}-group").into();
    div()
        .debug_selector(|| "message-code-block".into())
        .group(group.clone())
        .relative()
        .w_full()
        .min_w_0()
        .rounded(theme.radii.medium)
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.code_surface)
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .gap_1()
        .when_some(language, |block, language| {
            block.child(
                div()
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.text_muted)
                    .child(language.to_owned()),
            )
        })
        .child(
            div()
                .font_family(MONO_FAMILY)
                .font_features(FontFeatures::disable_ligatures())
                .text_size(theme.type_scale.code)
                .line_height(theme.type_scale.line_height_code)
                .text_color(theme.colors.text)
                .relative()
                .child(shown)
                .children(stamp),
        )
        .child(
            div()
                .absolute()
                .top(px(6.))
                .right(px(6.))
                .invisible()
                .group_hover(group, |style| style.visible())
                .child(
                    div()
                        .id(ElementId::Name(format!("{id}-copy").into()))
                        .debug_selector(|| "copy-code".into())
                        .cursor_pointer()
                        .px_2()
                        .py_0p5()
                        .rounded(theme.radii.small)
                        .border_1()
                        .border_color(theme.colors.border_strong)
                        .bg(theme.colors.surface_raised)
                        .text_size(theme.type_scale.small)
                        .text_color(theme.colors.text_muted)
                        .hover(|style| style.text_color(theme.colors.text))
                        .on_click(move |_, _window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                        })
                        .child("Copy"),
                ),
        )
        .into_any_element()
}

/// Quoted paragraphs behind a bar.
fn quote(
    paragraphs: &[Vec<Inline>],
    id: &str,
    props: &DocumentProps<'_>,
    walk: &Walk,
) -> AnyElement {
    let theme = props.theme;
    let mut column = div().flex().flex_col().gap(px(6.)).min_w_0().flex_1();
    for (index, inlines) in paragraphs.iter().enumerate() {
        column = column.child(paragraph(
            inlines,
            format!("{id}-q{index}").into(),
            props,
            walk,
        ));
    }
    div()
        .flex()
        .gap_3()
        .w_full()
        .min_w_0()
        .child(
            div()
                .w(px(3.))
                .flex_none()
                .rounded(px(2.))
                .bg(theme.colors.quote_bar),
        )
        .child(column)
        .into_any_element()
}

/// A bulleted or numbered list: each item is a unit whose marker is part of its selectable text.
fn list(items: &[Vec<Inline>], id: &str, props: &DocumentProps<'_>, walk: &Walk) -> AnyElement {
    let theme = props.theme;
    let mut column = div().flex().flex_col().gap(px(2.)).w_full().min_w_0();
    for (index, inlines) in items.iter().enumerate() {
        let unit_index = walk.next.get();
        let marker = walk.units[unit_index].prefix.trim_end().to_owned();
        let marker_selected = props.select.as_ref().is_some_and(|select| {
            select.range.as_ref().is_some_and(|range| {
                let start = walk.starts[unit_index];
                let end = start + walk.units[unit_index].prefix.len();
                range.start < end && range.end > start
            })
        });
        column = column.child(
            div()
                .flex()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .min_w(px(22.))
                        .flex_none()
                        .flex()
                        .justify_end()
                        .text_size(theme.type_scale.body)
                        .line_height(theme.type_scale.line_height_body)
                        .text_color(theme.colors.text_muted)
                        .when(marker_selected, |marker| marker.bg(theme.colors.selection))
                        .child(marker),
                )
                .child(div().flex_1().min_w_0().child(paragraph(
                    inlines,
                    format!("{id}-i{index}").into(),
                    props,
                    walk,
                ))),
        );
    }
    column.into_any_element()
}

#[cfg(test)]
mod tests {
    use lazy_domain::Inline;

    use super::*;

    fn bold() -> Marks {
        Marks {
            bold: true,
            ..Marks::NONE
        }
    }

    fn code() -> Marks {
        Marks {
            code: true,
            ..Marks::NONE
        }
    }

    #[test]
    fn spans_cover_the_text_exactly() {
        let line = lay_out(
            &[
                Inline::text("one "),
                Inline::marked("two", bold()),
                Inline::LineBreak,
                Inline::marked("three", code()),
            ],
            &HandlesOnly,
        );
        assert_eq!(line.text, "one two\nthree");
        assert_eq!(
            line.spans.iter().map(|s| s.len).sum::<usize>(),
            line.text.len()
        );
        assert_eq!(line.spans.len(), 4);
    }

    #[test]
    fn neighbors_with_the_same_style_share_a_span() {
        let line = lay_out(
            &[Inline::text("a"), Inline::text("b"), Inline::LineBreak],
            &HandlesOnly,
        );
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].len, 3);
    }

    #[test]
    fn links_record_their_byte_ranges_and_addresses() {
        let line = lay_out(
            &[
                Inline::text("see é "),
                Inline::link("https://example.com", "the site"),
                Inline::text("!"),
            ],
            &HandlesOnly,
        );
        assert_eq!(line.text, "see é the site!");
        let (range, url) = &line.links[0];
        assert_eq!(&line.text[range.clone()], "the site");
        assert_eq!(url, "https://example.com");
        assert!(line.spans.iter().any(|s| s.link));
    }

    #[test]
    fn a_link_keeps_its_formatting() {
        let line = lay_out(
            &[Inline::marked_link(
                "https://a.example",
                "bold link",
                bold(),
            )],
            &HandlesOnly,
        );
        assert!(line.spans[0].link && line.spans[0].marks.bold);
    }

    #[test]
    fn tabs_are_drawn_as_spaces_and_lengths_stay_consistent() {
        let line = lay_out(
            &[
                Inline::text("a\tb"),
                Inline::link("https://x.example", "c\td"),
            ],
            &HandlesOnly,
        );
        assert_eq!(line.text, "a    bc    d");
        assert_eq!(
            line.spans.iter().map(|s| s.len).sum::<usize>(),
            line.text.len()
        );
        let (range, _) = &line.links[0];
        assert_eq!(&line.text[range.clone()], "c    d");
    }

    #[test]
    fn style_runs_follow_the_marks() {
        let theme = Theme::light();
        let marks = Marks {
            bold: true,
            italic: true,
            strike: true,
            code: false,
        };
        let line = lay_out(
            &[
                Inline::marked("x", marks),
                Inline::marked("y", code()),
                Inline::link("https://a.example", "z"),
            ],
            &HandlesOnly,
        );
        let runs = runs(&line, &theme, theme.colors.text);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].font.weight, FontWeight::SEMIBOLD);
        assert_eq!(runs[0].font.style, FontStyle::Italic);
        assert!(runs[0].strikethrough.is_some() && runs[0].underline.is_none());
        assert_eq!(runs[1].font.family.as_ref(), MONO_FAMILY);
        assert_eq!(runs[1].background_color, Some(theme.colors.code_surface));
        assert_eq!(runs[2].color, theme.colors.accent);
        assert!(runs[2].underline.is_some());
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), line.text.len());
    }

    #[test]
    fn only_safe_links_are_acted_on() {
        use lazy_domain::{ConversationId, MessageId, WorkspaceId};
        assert_eq!(
            LinkTarget::classify("https://example.com/a"),
            Some(LinkTarget::External("https://example.com/a".into()))
        );
        assert!(matches!(
            LinkTarget::classify("mailto:a@example.com"),
            Some(LinkTarget::External(_))
        ));
        assert_eq!(LinkTarget::classify("javascript:alert(1)"), None);
        assert_eq!(LinkTarget::classify("file:///etc/passwd"), None);
        assert_eq!(LinkTarget::classify("lazy://not-a-permalink"), None);
        assert_eq!(LinkTarget::classify("no scheme"), None);
        let link = Permalink {
            workspace_id: WorkspaceId::generate(),
            conversation_id: ConversationId::generate(),
            message_id: MessageId::generate(),
        };
        assert_eq!(
            LinkTarget::classify(&link.to_string()),
            Some(LinkTarget::Permalink(link))
        );
    }

    // ---- Mentions ----

    use lazy_domain::{GroupId, UserId};

    /// Names for a fixed cast, as the timeline's core-backed labels would give them.
    struct Cast {
        alice: UserId,
        me: UserId,
    }

    impl MentionLabels for Cast {
        fn look(&self, target: MentionTarget, handle: &str) -> MentionLook {
            match target {
                MentionTarget::User { id } if id == self.alice => MentionLook {
                    text: "@Alice Nguyen".into(),
                    aimed_at_me: false,
                },
                MentionTarget::User { id } if id == self.me => MentionLook {
                    text: "@Me Myself".into(),
                    aimed_at_me: true,
                },
                MentionTarget::Channel => MentionLook {
                    text: "@channel".into(),
                    aimed_at_me: true,
                },
                _ => MentionLook {
                    text: format!("@{handle}"),
                    aimed_at_me: false,
                },
            }
        }
    }

    fn cast() -> Cast {
        Cast {
            alice: UserId::generate(),
            me: UserId::generate(),
        }
    }

    #[test]
    fn a_mention_is_one_padded_span_showing_the_name() {
        let cast = cast();
        let line = lay_out(
            &[
                Inline::text("hi "),
                Inline::mention(MentionTarget::User { id: cast.alice }, "alice"),
                Inline::text(", ok"),
            ],
            &cast,
        );
        assert_eq!(line.text, "hi \u{a0}@Alice Nguyen\u{a0}, ok");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].mention, Some(false));
        assert_eq!(
            line.spans.iter().map(|s| s.len).sum::<usize>(),
            line.text.len()
        );
    }

    #[test]
    fn an_unknown_person_or_group_falls_back_to_the_handle_written() {
        let cast = cast();
        let gone = MentionTarget::User {
            id: UserId::generate(),
        };
        let group = MentionTarget::Group {
            id: GroupId::generate(),
        };
        let line = lay_out(
            &[
                Inline::mention(gone, "bob"),
                Inline::mention(group, "design-team"),
            ],
            &cast,
        );
        assert!(
            line.text.contains("@bob") && line.text.contains("@design-team"),
            "{}",
            line.text
        );
    }

    #[test]
    fn neighboring_mentions_stay_separate_pills() {
        let cast = cast();
        let line = lay_out(
            &[
                Inline::mention(MentionTarget::Channel, "channel"),
                Inline::mention(MentionTarget::Channel, "channel"),
            ],
            &cast,
        );
        assert_eq!(line.spans.len(), 2);
    }

    #[test]
    fn mention_runs_are_tinted_and_the_ones_aimed_at_the_person_are_stronger() {
        let theme = Theme::light();
        let cast = cast();
        let line = lay_out(
            &[
                Inline::mention(MentionTarget::User { id: cast.alice }, "alice"),
                Inline::mention(MentionTarget::User { id: cast.me }, "me"),
            ],
            &cast,
        );
        let runs = runs(&line, &theme, theme.colors.text);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].background_color, Some(theme.colors.mention_surface));
        assert_eq!(
            runs[1].background_color,
            Some(theme.colors.mention_me_surface)
        );
        assert_eq!(runs[0].color, theme.colors.mention_text);
        assert_eq!(runs[0].font.weight, FontWeight::NORMAL);
        assert_eq!(runs[1].font.weight, FontWeight::SEMIBOLD, "emphasised");
    }

    #[test]
    fn a_mention_keeps_its_formatting() {
        let cast = cast();
        let marks = Marks {
            bold: true,
            italic: true,
            strike: true,
            code: false,
        };
        let line = lay_out(
            &[Inline::Mention {
                target: MentionTarget::User { id: cast.alice },
                handle: "alice".into(),
                marks,
            }],
            &cast,
        );
        let theme = Theme::dark();
        let run = &runs(&line, &theme, theme.colors.text)[0];
        assert_eq!(run.font.weight, FontWeight::SEMIBOLD);
        assert_eq!(run.font.style, FontStyle::Italic);
        assert!(run.strikethrough.is_some());
    }

    #[test]
    fn copied_and_read_aloud_text_uses_the_handle() {
        let cast = cast();
        let inlines = [
            Inline::text("thanks "),
            Inline::mention(MentionTarget::User { id: cast.alice }, "alice"),
            Inline::LineBreak,
            Inline::mention(MentionTarget::Channel, "channel"),
        ];
        assert_eq!(plain_text(&inlines), "thanks @alice\n@channel");
    }

    // ---- Custom emoji ----

    /// A picture `width` by `height`, decoded the way the image store does.
    fn picture(width: u32, height: u32) -> EmojiPicture {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 120, 200, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let decoded = crate::image_store::decode(bytes.get_ref(), 128).unwrap();
        EmojiPicture {
            image: decoded.image,
            aspect: decoded.width as f32 / decoded.height as f32,
        }
    }

    /// Emoji that have a picture are the ones in `with`; the rest read as their names.
    struct Pictures {
        with: Vec<(EmojiId, EmojiPicture)>,
    }

    impl MentionLabels for Pictures {
        fn look(&self, _: MentionTarget, handle: &str) -> MentionLook {
            MentionLook {
                text: format!("@{handle}"),
                aimed_at_me: false,
            }
        }

        fn emoji(&self, id: EmojiId, name: &str) -> EmojiLook {
            EmojiLook {
                label: format!(":{name}:"),
                picture: self
                    .with
                    .iter()
                    .find(|(known, _)| *known == id)
                    .map(|(_, picture)| picture.clone()),
            }
        }
    }

    #[test]
    fn a_picture_is_a_fixed_height_and_keeps_its_shape_within_limits() {
        assert_eq!(picture(64, 64).size_em(), (1.25, 1.25), "square");
        let (wide, height) = picture(128, 32).size_em();
        assert_eq!(
            (wide, height),
            (2.5, 1.25),
            "no more than twice as wide as tall"
        );
        let (narrow, _) = picture(8, 128).size_em();
        assert_eq!(narrow, 0.625, "no less than half as wide as tall");
        let (a_bit_wide, _) = picture(96, 64).size_em();
        assert!((a_bit_wide - 1.875).abs() < 0.01, "{a_bit_wide}");
    }

    #[test]
    fn an_emoji_with_a_picture_reserves_room_and_copies_as_its_name() {
        let id = EmojiId::generate();
        let labels = Pictures {
            with: vec![(id, picture(64, 64))],
        };
        let line = lay_out(
            &[
                Inline::text("go "),
                Inline::emoji(id, "shipit"),
                Inline::text(" now"),
            ],
            &labels,
        );
        assert_eq!(line.flat, "go :shipit: now", "what a copy writes");
        assert_eq!(line.emoji.len(), 1);
        let at = &line.emoji[0];
        assert_eq!(at.label, ":shipit:");
        assert_eq!(at.drawn.start, 3, "right after the text before it");
        assert!(
            line.text[at.drawn.clone()]
                .chars()
                .all(|c| matches!(c, '\u{2007}' | '\u{a0}' | '\u{202f}')),
            "room is made of spaces that never wrap or collapse"
        );
        assert!(line.text.ends_with(" now"));
        // The picture is one thing to select: from before it to after it takes its whole name.
        assert_eq!(line.flat_to_drawn(3, false), at.drawn.start);
        assert_eq!(line.flat_to_drawn(11, true), at.drawn.end);
        assert!(line.spans.iter().all(|span| !span.muted), "no muted text");
    }

    #[test]
    fn an_emoji_without_a_picture_is_its_name_muted_in_the_marks_it_was_written_with() {
        let id = EmojiId::generate();
        let labels = Pictures { with: vec![] };
        let line = lay_out(
            &[
                Inline::text("go "),
                Inline::Emoji {
                    id,
                    name: "shipit".into(),
                    marks: bold(),
                },
                Inline::text(" now"),
            ],
            &labels,
        );
        assert_eq!(line.text, "go :shipit: now", "the name is what is drawn");
        assert_eq!(line.flat, "go :shipit: now");
        assert!(line.emoji.is_empty(), "there is nothing to paint");
        let muted: Vec<&Span> = line.spans.iter().filter(|span| span.muted).collect();
        assert_eq!(muted.len(), 1);
        assert_eq!(muted[0].len, ":shipit:".len());
        assert!(muted[0].marks.bold, "formatting applies to the stand-in");
        assert!(
            line.spans
                .iter()
                .filter(|span| !span.muted)
                .all(|span| !span.marks.bold),
            "and to nothing else"
        );
    }

    #[test]
    fn formatting_does_not_reach_a_picture() {
        let id = EmojiId::generate();
        let labels = Pictures {
            with: vec![(id, picture(64, 64))],
        };
        let line = lay_out(
            &[Inline::Emoji {
                id,
                name: "shipit".into(),
                marks: bold(),
            }],
            &labels,
        );
        assert!(line.spans.iter().all(|span| !span.marks.bold));
    }

    #[test]
    fn several_in_a_row_each_get_their_own_room_in_order() {
        let ids = [
            EmojiId::generate(),
            EmojiId::generate(),
            EmojiId::generate(),
        ];
        let labels = Pictures {
            with: vec![
                (ids[0], picture(64, 64)),
                (ids[1], picture(128, 32)),
                (ids[2], picture(64, 64)),
            ],
        };
        let line = lay_out(
            &[
                Inline::emoji(ids[0], "a_one"),
                Inline::emoji(ids[1], "b_two"),
                Inline::emoji(ids[2], "c_three"),
            ],
            &labels,
        );
        assert_eq!(line.flat, ":a_one::b_two::c_three:");
        assert_eq!(line.emoji.len(), 3);
        let places: Vec<&Range<usize>> = line.emoji.iter().map(|at| &at.drawn).collect();
        assert_eq!(places[0].end, places[1].start, "no gap between neighbors");
        assert_eq!(places[1].end, places[2].start);
        assert!(
            places[1].len() > places[0].len(),
            "the wide one is given more room"
        );
        assert_eq!(line.text.len(), places[2].end);
    }

    #[test]
    fn a_mix_of_ready_and_missing_emoji_draws_each_as_it_is() {
        let ready = EmojiId::generate();
        let waiting = EmojiId::generate();
        let labels = Pictures {
            with: vec![(ready, picture(64, 64))],
        };
        let line = lay_out(
            &[
                Inline::emoji(ready, "done"),
                Inline::emoji(waiting, "later"),
            ],
            &labels,
        );
        assert_eq!(line.emoji.len(), 1);
        assert!(line.text.ends_with(":later:"));
        assert_eq!(line.flat, ":done::later:");
    }

    #[test]
    fn read_aloud_an_emoji_is_its_name_and_the_word_emoji() {
        let id = EmojiId::generate();
        let inlines = [Inline::text("ship "), Inline::emoji(id, "shipit")];
        assert_eq!(plain_text(&inlines), "ship :shipit: emoji");
    }
}
