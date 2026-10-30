//! The virtualized message timeline, for a conversation or for a thread.
//!
//! One [`ListState`] is kept per [`Scope`] (a conversation, or a thread), so switching restores
//! each one's scroll position. The view has two modes: the conversation timeline follows the
//! selected conversation, and the thread pane's timeline follows the open thread (its root, then
//! its replies). Everything below works the same in both; the differences are marked. The core describes row-list changes precisely (`Change::TimelineSplice` and
//! friends) and this view mirrors them into the list state; rows are drawn from the core's
//! current data. No I/O or parsing happens while rendering.
//!
//! # Keyboard
//!
//! The timeline is one tab stop. While it has focus, a cursor (the "roving" focus) marks one
//! message: Up and Down move it, Home and End jump to the oldest and newest loaded message, and
//! Enter opens a menu of what can be done to that message. Rows themselves are not tab stops, so
//! tabbing through the window does not stop at every message. The hover toolbar offers the same
//! actions to the pointer.
//!
//! # Editing
//!
//! At most one message is edited at a time. The editor replaces the message body in place; the
//! core owns what happens to a saved edit (`TimelineItem::edit`), and this view follows it: the
//! editor closes when the save succeeds and shows the reason when it does not.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Anchor, App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FollowMode,
    IntoElement, KeyDownEvent, ListAlignment, ListState, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, SharedString, Subscription, Task, Window,
    anchored, canvas, deferred, div, list, point, prelude::*, px,
};
use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_client_core::{
    Change, ClientCore, Delivery, DownloadState, EditState, HistoryStatus, ImageState, Input,
    ItemKey, JumpTarget, PendingFileState, TimelineItem,
};
use lazy_domain::document::MAX_TEXT_BYTES;
use lazy_domain::{
    AttachmentId, ConversationId, ConversationKind, MessageId, Permalink, ReactionEmoji,
};
use lazy_protocol::Attachment;

use crate::app_model::{AppEvent, AppModel};
use crate::attachment_view::{self, FileChip, ImageTile, Preview, Transfer};
use crate::attachments;
use crate::downloads::{self, Outcome};
use crate::emoji_images;
use crate::form::{self, ButtonKind};
use crate::image_store::{ImageKey, ImageStore, Slot, Source, THUMBNAIL_EDGE};
use crate::mention_source;
use crate::menu::{Menu, MenuEvent, MenuItem};
use crate::message_row::{
    AnchorHandler, EditorCommand, EditorNotice, EditorPhase, EditorProps, PaneRowProps,
    ReactionHandler, ReplierAvatar, ReplyHeaderProps, RetryHandler, RowAction, RowActions,
    RowProps, ThreadLineProps, render_message_row, row_layout,
};
use crate::reactions;
use crate::rich_text::{self, HitSink, LinkHandler, LinkTarget, SelectProps};
use crate::selection::{self, CopyRow, Pos, Selection};
use crate::states;
use crate::suggest::{PickSuggestion, Sources, Suggestions};
use crate::text_input::{SuggestionKey, TextInput, TextInputEvent, TextInputOptions};
use crate::theme::Theme;
use crate::time_fmt::{ago_label, clock_label, to_zoned};

/// How long a message the person was taken to stays highlighted.
const HIGHLIGHT_FOR: Duration = Duration::from_millis(2500);
/// How far the pointer must move after pressing before the press becomes a drag selection.
const DRAG_THRESHOLD: f32 = 3.0;
/// Within this distance of the top or bottom edge of the list (or beyond it), a drag scrolls.
const AUTOSCROLL_EDGE: f32 = 32.0;
/// The most a drag scrolls in one tick, in pixels.
const AUTOSCROLL_MAX: f32 = 48.0;
/// Time between drag ticks (auto-scroll while the pointer is held near an edge).
const DRAG_TICK: Duration = Duration::from_millis(30);
/// Key context of the timeline when it holds the keyboard cursor.
pub const TIMELINE_CONTEXT: &str = "LazyTimeline";

/// Rows rendered beyond the viewport on each side, in pixels. Bounds the mounted rows.
const OVERDRAW: f32 = 600.0;
/// Request older history when the first visible row is within this many rows of the top.
///
/// This prefetches: a page is 50 rows, a single flick can cover about 18, and the range a scroll
/// handler reports trails the layout by one event, so a small threshold could be skipped over
/// entirely and leave the reader stuck at the top with nothing loading.
const LOAD_OLDER_THRESHOLD: usize = 30;

/// What a timeline lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// A conversation's history.
    Conversation(ConversationId),
    /// A thread: its root message, then its replies, oldest first.
    Thread {
        conversation: ConversationId,
        root: MessageId,
    },
}

impl Scope {
    /// The conversation the messages belong to.
    pub fn conversation(self) -> ConversationId {
        match self {
            Self::Conversation(id) => id,
            Self::Thread { conversation, .. } => conversation,
        }
    }

    /// The root of the thread, for a thread scope.
    pub fn root(self) -> Option<MessageId> {
        match self {
            Self::Conversation(_) => None,
            Self::Thread { root, .. } => Some(root),
        }
    }
}

/// Which scope a timeline view follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The selected conversation.
    Conversation,
    /// The open thread (the thread pane).
    Thread,
}

/// The rows of a scope, borrowed from the core: for a thread, the root (when the core has it)
/// followed by the replies, so one index runs through both.
pub struct Rows<'a> {
    head: Option<&'a TimelineItem>,
    tail: &'a [TimelineItem],
}

impl<'a> Rows<'a> {
    pub fn len(&self) -> usize {
        usize::from(self.head.is_some()) + self.tail.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the first row is a thread's root (so replies are numbered one further on).
    pub fn head_is_root(&self) -> bool {
        self.head.is_some()
    }

    pub fn get(&self, index: usize) -> Option<&'a TimelineItem> {
        match self.head {
            Some(head) if index == 0 => Some(head),
            Some(_) => self.tail.get(index - 1),
            None => self.tail.get(index),
        }
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &'a TimelineItem> + use<'a> {
        self.head.into_iter().chain(self.tail.iter())
    }

    pub fn last(&self) -> Option<&'a TimelineItem> {
        self.tail.last().or(self.head)
    }

    pub fn position(&self, key: ItemKey) -> Option<usize> {
        self.iter().position(|item| item.key == key)
    }

    pub fn find(&self, key: ItemKey) -> Option<&'a TimelineItem> {
        self.iter().find(|item| item.key == key)
    }
}

/// The rows of `scope` as the core holds them now. A thread the core does not have is empty.
pub fn rows<'a>(core: &'a ClientCore, scope: Scope) -> Rows<'a> {
    match scope {
        Scope::Conversation(id) => Rows {
            head: None,
            tail: core.timeline(id),
        },
        Scope::Thread { root, .. } => match core.thread(root) {
            Some(view) => Rows {
                head: view.root,
                tail: view.replies,
            },
            None => Rows {
                head: None,
                tail: &[],
            },
        },
    }
}

/// A row to put back where it was after older history was prepended.
///
/// GPUI keeps the same row anchored when rows are inserted above it, but the old first row also
/// loses its day separator and author header once it has a predecessor, so it changes height and
/// everything below it would jump. The render after the splice lays out and measures the new
/// rows; the render after that can read where the watched row ended up and scroll by the
/// difference. Scroll offset is view geometry, not I/O, and nothing is dispatched here.
struct PendingRestore {
    scope: Scope,
    /// Index of the watched row after the splice.
    item: usize,
    /// Where the row was before the splice, in window coordinates.
    y_before: Pixels,
    /// Renders seen since the splice.
    renders: u8,
}

/// Renders to wait for the new rows to be measured before giving up on a restore.
const RESTORE_GIVE_UP_AFTER: u8 = 4;

/// A move of the keyboard cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorMove {
    Older,
    Newer,
    Home,
    End,
}

/// What happened to a list's rows, as the core described it.
#[derive(Debug, Clone, Copy)]
enum RowChange {
    Reset,
    /// The thread's root row appeared, changed, or went.
    Root,
    Splice {
        at: usize,
        removed: usize,
        inserted: usize,
    },
    Updated {
        index: usize,
    },
}

/// Requests the timeline passes to its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineEvent {
    /// The person asked to delete a message. Deleting cannot be undone, so the host confirms
    /// first.
    ConfirmDelete {
        conversation: ConversationId,
        message: MessageId,
    },
    /// Put the keyboard back in the composer: an edit that began there has ended.
    FocusComposer,
    /// The person wants to add a reaction: open the emoji picker near `anchor` (a window
    /// position) and, on a choice, react to this message.
    PickReaction {
        conversation: ConversationId,
        message: MessageId,
        anchor: gpui::Point<Pixels>,
    },
    /// The person asked to open the thread of `root`, from its summary line, the toolbar, or the
    /// menu. The host opens the pane and moves the keyboard into it.
    OpenThread {
        conversation: ConversationId,
        root: MessageId,
    },
    /// Text was selected here: there is one selection at a time, so the host clears the others
    /// (the other timeline's, and the composers').
    SelectionStarted,
    /// The person opened an image: show the message's `images`, starting at `index`.
    OpenViewer {
        images: Vec<Attachment>,
        index: usize,
    },
    /// The person asked for the profile of `user`, from a message's author or the menu.
    OpenProfile { user: lazy_domain::UserId },
}

/// A mouse press that may turn into a drag selection.
struct Drag {
    /// Where the button went down.
    press: Point<Pixels>,
    /// The pointer has moved far enough for this to be a selection, not a click.
    started: bool,
    /// 1 for a click, 2 for a double-click (extend by words), 3 for a triple (by messages).
    clicks: usize,
    /// For a double or triple click: the word or message first selected, which a drag keeps.
    origin: Option<(Pos<ItemKey>, Pos<ItemKey>)>,
    /// Keeps scrolling while the pointer is held at an edge; dropped when the button is released.
    _ticker: Task<()>,
}

/// The actions menu opened from the keyboard.
struct OpenMenu {
    key: ItemKey,
    menu: Entity<Menu>,
    _subscription: Subscription,
}

/// Where an edit began, which is where the keyboard returns when it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOrigin {
    /// Up in the empty composer. The edit ends back in the composer.
    Composer,
    /// The hover toolbar, the actions menu, or the roving cursor. The edit ends on the message.
    Timeline,
}

/// The message being edited in place.
struct EditSession {
    /// Where the keyboard goes when the edit ends.
    origin: EditOrigin,
    scope: Scope,
    conversation: ConversationId,
    message: MessageId,
    key: ItemKey,
    input: Entity<TextInput>,
    /// The text was emptied and the person is offered deletion instead of saving.
    empty_prompt: bool,
    /// A save was sent, and the core has not yet said how it went.
    saving: bool,
    save_focus: FocusHandle,
    cancel_focus: FocusHandle,
    delete_focus: FocusHandle,
    keep_focus: FocusHandle,
    /// Emoji shortcode suggestions for what is being typed.
    suggestions: Suggestions,
    _subscriptions: Vec<Subscription>,
}

pub struct TimelineView {
    model: Entity<AppModel>,
    mode: Mode,
    lists: HashMap<Scope, ListState>,
    restore: Option<PendingRestore>,
    /// The "New messages" row of each conversation as last drawn, to remeasure when it moves.
    markers: HashMap<ConversationId, Option<ItemKey>>,
    /// What was last reported to the core about whether each list is at the bottom.
    bottom_reported: HashMap<Scope, Rc<Cell<Option<bool>>>>,
    /// Tab stops for the retry buttons of failed messages, by message identity.
    retry_focus: Rc<RefCell<HashMap<ItemKey, FocusHandle>>>,
    /// Tab stop for the button that asks again after history failed to load.
    history_retry_focus: FocusHandle,
    /// The timeline's one tab stop. While it is focused, `cursor` marks a message.
    focus: FocusHandle,
    /// The selected text, if any, by message identity.
    selection: Option<Selection<ItemKey>>,
    /// The selection was made with the mouse, which moved the keyboard here: Escape gives it back.
    selection_by_mouse: bool,
    /// Where each paragraph on screen was drawn, for turning the pointer into a text position.
    sink: Rc<HitSink>,
    /// The selection laid against the rows as of the last render.
    resolved: Option<selection::Resolved>,
    /// Counts paints of this view; see [`SelectProps::epoch`].
    epoch: Rc<Cell<u64>>,
    /// A mouse press in progress.
    drag: Option<Drag>,
    /// The message a keyboard selection by whole messages began on.
    keyboard_anchor: Option<ItemKey>,
    /// The message the keyboard cursor is on.
    cursor: Option<ItemKey>,
    /// The cursor is on the message's thread summary line rather than the message itself. Only
    /// a message with a summary line has this stop (conversation mode).
    cursor_on_summary: bool,
    /// The cursor is on one of the message's files (its position among them) rather than the
    /// message itself. The stops of a message, in order: the message, its files, its thread
    /// summary line.
    cursor_on_attachment: Option<usize>,
    /// Where each file was saved, or is being saved, so a retry does not ask again.
    download_paths: HashMap<AttachmentId, PathBuf>,
    /// Downloads whose chip is showing a failure, which is taller than the others: the row is
    /// measured again when a download enters or leaves that state.
    download_failed: HashSet<AttachmentId>,
    /// Why a file could not be saved when no download state can say (the save dialog failed).
    file_notice: Option<String>,
    /// Thumbnails and decodes the rows drawn since the last flush asked for.
    wanted_thumbnails: Vec<AttachmentId>,
    wanted_decodes: Vec<(ImageKey, Source)>,
    /// Custom emoji the rows drawn since the last flush showed.
    wanted_emoji: Vec<lazy_domain::EmojiId>,
    /// Authors on screen whose avatars are not known yet.
    wanted_avatars: Vec<lazy_domain::UserId>,
    /// How many custom emoji pictures had finished when the rows were last measured.
    emoji_generation: u64,
    requests_queued: bool,
    /// How wide the list was when the view last rendered, in pixels (0 before the first).
    column_width: f32,
    /// Tab stops for the Discard buttons of failed messages, by message identity.
    discard_focus: Rc<RefCell<HashMap<ItemKey, FocusHandle>>>,
    menu: Option<OpenMenu>,
    editor: Option<EditSession>,
    /// The message just jumped to, drawn highlighted for a moment.
    highlight: Option<ItemKey>,
    highlight_generation: u64,
    /// A jump the core announced before this scope's list existed or was drawn.
    pending_jump: Option<JumpTarget>,
    zone: TimeZone,
    /// The time rows are laid out against; refreshed on every render.
    now: Zoned,
    _subscription: Subscription,
    /// Repaints when an image finishes decoding.
    _images: Subscription,
}

impl EventEmitter<TimelineEvent> for TimelineView {}

impl Focusable for TimelineView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TimelineView {
    /// The timeline of the selected conversation.
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_mode(model, Mode::Conversation, window, cx)
    }

    /// The timeline of the open thread, for the thread pane.
    pub fn for_thread(
        model: Entity<AppModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_mode(model, Mode::Thread, window, cx)
    }

    fn with_mode(
        model: Entity<AppModel>,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe_in(
            &model,
            window,
            |this, _model, event: &AppEvent, window, cx| {
                let AppEvent::Changed(changes) = event;
                this.apply(changes, window, cx);
            },
        );
        let images = cx.observe(&ImageStore::global(cx), |this, store, cx| {
            // A custom emoji's picture is a different width from its `:name:`, so lines that
            // hold one may wrap differently: measure again, once for each batch that finished.
            let generation = store.read(cx).emoji_generation();
            if generation != this.emoji_generation {
                this.emoji_generation = generation;
                for state in this.lists.values() {
                    state.remeasure();
                }
            }
            cx.notify()
        });
        Self {
            model,
            mode,
            lists: HashMap::new(),
            restore: None,
            markers: HashMap::new(),
            bottom_reported: HashMap::new(),
            retry_focus: Rc::default(),
            history_retry_focus: cx.focus_handle().tab_stop(true),
            focus: cx.focus_handle().tab_stop(true),
            selection: None,
            selection_by_mouse: false,
            sink: Rc::default(),
            resolved: None,
            epoch: Rc::default(),
            drag: None,
            keyboard_anchor: None,
            cursor: None,
            cursor_on_summary: false,
            cursor_on_attachment: None,
            download_paths: HashMap::new(),
            download_failed: HashSet::new(),
            file_notice: None,
            wanted_thumbnails: Vec::new(),
            wanted_decodes: Vec::new(),
            wanted_emoji: Vec::new(),
            wanted_avatars: Vec::new(),
            emoji_generation: 0,
            requests_queued: false,
            column_width: 0.0,
            discard_focus: Rc::default(),
            menu: None,
            editor: None,
            highlight: None,
            highlight_generation: 0,
            pending_jump: None,
            zone: TimeZone::system(),
            now: Zoned::now(),
            _subscription: subscription,
            _images: images,
        }
    }

    /// The scope this view shows right now: the selected conversation, or the open thread.
    pub fn scope(&self, cx: &App) -> Option<Scope> {
        let core = self.model.read(cx).core();
        match self.mode {
            Mode::Conversation => core.selected_conversation().map(Scope::Conversation),
            Mode::Thread => core.open_thread().map(|open| Scope::Thread {
                conversation: open.conversation,
                root: open.root,
            }),
        }
    }

    /// Where the list sits now: the first row showing, how far into it, and whether it is
    /// following the newest row. `None` before the list has been drawn. For tests of reading
    /// position.
    pub fn scroll_snapshot(&self, cx: &App) -> Option<(usize, Pixels, bool)> {
        let scope = self.scope(cx)?;
        let state = self.lists.get(&scope)?;
        let top = state.logical_scroll_top();
        Some((top.item_ix, top.offset_in_item, state.is_following_tail()))
    }

    /// Mirrors the core's row-list changes into the matching list state.
    fn apply(&mut self, changes: &[Change], window: &mut Window, cx: &mut Context<Self>) {
        let current = self.scope(cx);
        if self.mode == Mode::Thread {
            // A thread that is no longer open keeps nothing: opening it again starts at its end.
            self.lists.retain(|scope, _| Some(*scope) == current);
            self.bottom_reported
                .retain(|scope, _| Some(*scope) == current);
        }
        let model = self.model.read(cx);
        let mut downloads: Vec<AttachmentId> = Vec::new();
        let mut restore = None;
        // Scopes reset in this batch: the reset already counts every row the core holds, so any
        // change after it in the same batch describes rows the list now has.
        let mut reset: Vec<Scope> = Vec::new();
        for change in changes {
            // What happened to which scope's rows.
            let step = match (self.mode, *change) {
                (Mode::Conversation, Change::TimelineReset { conversation }) => {
                    Some((Scope::Conversation(conversation), RowChange::Reset))
                }
                (
                    Mode::Conversation,
                    Change::TimelineSplice {
                        conversation,
                        at,
                        removed,
                        inserted,
                    },
                ) => Some((
                    Scope::Conversation(conversation),
                    RowChange::Splice {
                        at,
                        removed,
                        inserted,
                    },
                )),
                (
                    Mode::Conversation,
                    Change::TimelineItemUpdated {
                        conversation,
                        index,
                    },
                ) => Some((
                    Scope::Conversation(conversation),
                    RowChange::Updated { index },
                )),
                // A thread's replies are numbered from zero; the root, when there is one, is the
                // row before them.
                (Mode::Thread, Change::ThreadReset { root }) => current
                    .filter(|s| s.root() == Some(root))
                    .map(|s| (s, RowChange::Reset)),
                (
                    Mode::Thread,
                    Change::ThreadSplice {
                        root,
                        at,
                        removed,
                        inserted,
                    },
                ) => current.filter(|s| s.root() == Some(root)).map(|s| {
                    let head = usize::from(rows(model.core(), s).head_is_root());
                    (
                        s,
                        RowChange::Splice {
                            at: at + head,
                            removed,
                            inserted,
                        },
                    )
                }),
                (Mode::Thread, Change::ThreadItemUpdated { root, index }) => {
                    current.filter(|s| s.root() == Some(root)).map(|s| {
                        let head = usize::from(rows(model.core(), s).head_is_root());
                        (
                            s,
                            RowChange::Updated {
                                index: index + head,
                            },
                        )
                    })
                }
                // The root arrived, changed, or went: its row and the one after it are redrawn,
                // and a root that appeared or vanished changes the row count.
                (Mode::Thread, Change::ThreadRootChanged { root }) => current
                    .filter(|s| s.root() == Some(root))
                    .map(|s| (s, RowChange::Root)),
                _ => None,
            };
            if let Some((scope, step)) = step
                && !reset.contains(&scope)
                && let Some(state) = self.lists.get(&scope)
            {
                let total = rows(model.core(), scope).len();
                match step {
                    RowChange::Reset => {
                        state.reset(total);
                        reset.push(scope);
                    }
                    RowChange::Root => {
                        if state.item_count() == total {
                            state.remeasure_items(0..total.min(2));
                        } else {
                            state.reset(total);
                            reset.push(scope);
                        }
                    }
                    RowChange::Splice {
                        at,
                        removed,
                        inserted,
                    } => {
                        // Older history prepended above a reader who is at the top: remember
                        // where the next row sits so it can be put back after relayout. In a
                        // thread the root stays first and older replies go in right after it.
                        let base = usize::from(rows(model.core(), scope).head_is_root());
                        let watch = (at == base
                            && removed == 0
                            && inserted > 0
                            && state.logical_scroll_top().item_ix <= base)
                            .then(|| {
                                state
                                    .bounds_for_item(base + 1)
                                    .map(|bounds| bounds.origin.y)
                            })
                            .flatten();
                        state.splice(at..at + removed, inserted);
                        // The rows next to the splice may change shape: a header can appear or
                        // disappear when its neighbor changes.
                        let count = state.item_count();
                        let start = at.saturating_sub(1);
                        let end = (at + inserted + 1).min(count);
                        if start < end {
                            state.remeasure_items(start..end);
                        }
                        if let Some(y_before) = watch {
                            restore = Some(PendingRestore {
                                scope,
                                item: base + 1 + inserted,
                                y_before,
                                renders: 0,
                            });
                        }
                    }
                    RowChange::Updated { index } => {
                        let count = state.item_count();
                        let end = (index + 2).min(count);
                        if index < end {
                            state.remeasure_items(index..end);
                        }
                    }
                }
            }
            match *change {
                Change::ImageChanged { .. } => {}
                Change::DownloadChanged { attachment } => downloads.push(attachment),
                Change::JumpTargetChanged => {
                    self.pending_jump = model.core().jump_target();
                }
                Change::GroupsChanged | Change::DirectoryChanged => {
                    // A renamed group or person changes how wide the pills that name them are,
                    // so rows may wrap differently.
                    for state in self.lists.values() {
                        state.remeasure();
                    }
                }
                _ => {}
            }
        }
        if restore.is_some() {
            self.restore = restore;
        }
        for attachment in downloads {
            self.download_changed(attachment, cx);
        }
        let selection_changed = changes.iter().any(|c| {
            matches!(
                (self.mode, c),
                (Mode::Conversation, Change::SelectionChanged)
                    | (Mode::Thread, Change::OpenThreadChanged)
            )
        });
        if selection_changed {
            // The cursor and an open menu belong to the conversation (or thread) that was left.
            // An editor stays with its message and reappears if the person comes back.
            self.cursor = None;
            self.cursor_on_summary = false;
            self.cursor_on_attachment = None;
            self.menu = None;
        }
        self.follow_edit(window, cx);
        if let Some(scope) = self.scope(cx) {
            if selection_changed {
                self.report_bottom_again(scope, cx);
            }
            if let Scope::Conversation(conversation) = scope {
                self.sync_marker(conversation, cx);
            }
        }
        cx.notify();
    }

    // ----- Selecting text -----

    /// The selectable text of a message: what a copy of the whole message writes.
    fn row_text(core: &ClientCore, item: &TimelineItem) -> String {
        if item.deleted {
            return String::new();
        }
        let author_is_me = core.me().map(|user| user.id) == Some(item.author_id);
        let table = mention_source::MentionTable::for_document(core, &item.document, author_is_me);
        let mut text = rich_text::message_text(&item.document, &table);
        // Each file is a line after the text: `[file: name (size)]`.
        for attachment in &item.attachments {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&attachments::copy_marker(&attachment.name, attachment.size));
        }
        text
    }

    /// The `Author  time` line of a message in a copy. `with_date` adds the day, for a copy that
    /// spans more than one.
    fn copy_header(
        &self,
        core: &ClientCore,
        item: &TimelineItem,
        with_date: bool,
    ) -> (String, String) {
        let at = item
            .created_at()
            .map_or_else(|| self.now.clone(), |at| to_zoned(at, &self.zone));
        let time = if with_date {
            at.strftime("%b %-d, %-I:%M %p").to_string()
        } else {
            clock_label(&at)
        };
        (core.display_name(item.author_id).to_owned(), time)
    }

    /// Where the text at `offset` of the message in row `row` was drawn, in window coordinates.
    /// `None` when that message's text is not on screen. For tests that press and drag on text.
    pub fn text_point(&self, row: usize, offset: usize, cx: &App) -> Option<Point<Pixels>> {
        let scope = self.scope(cx)?;
        let key = rows(self.model.read(cx).core(), scope).get(row)?.key;
        let sink = self.sink.borrow();
        sink.iter()
            .filter(|((message, _), _)| *message == key)
            .find(|(_, hit)| hit.contains_offset(offset))
            .and_then(|(_, hit)| hit.point_for(offset))
    }

    /// Whether any text is selected.
    pub fn has_selection(&self) -> bool {
        self.selection
            .is_some_and(|selection| !selection.is_empty())
    }

    /// The text a copy would write now, or `None` when nothing is selected.
    pub fn selected_text(&self, cx: &App) -> Option<String> {
        let scope = self.scope(cx)?;
        let core = self.model.read(cx).core();
        let items = rows(core, scope);
        let resolved = selection::resolve(&self.selection?, |key| items.position(key))?;
        if resolved.is_empty() {
            return None;
        }
        let day = |row: usize| {
            items
                .get(row)
                .and_then(|item| item.created_at().map(|at| to_zoned(at, &self.zone).date()))
        };
        let with_date = day(resolved.first_row) != day(resolved.last_row);
        let text = selection::copy_text(&resolved, |row| {
            let item = items.get(row)?;
            let (author, time) = self.copy_header(core, item, with_date);
            Some(CopyRow {
                author,
                time,
                text: Self::row_text(core, item),
            })
        });
        (!text.is_empty()).then_some(text)
    }

    /// Clears the selection, for the host when something else was selected.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.selection.take().is_some() {
            self.keyboard_anchor = None;
            self.selection_by_mouse = false;
            cx.notify();
        }
    }

    /// Selects the text from `anchor` to `head`, each a message's row and a byte offset into its
    /// selectable text. A way to put a selection on screen without a pointer, for renders.
    pub fn select_range(
        &mut self,
        anchor: (usize, usize),
        head: (usize, usize),
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = self.scope(cx) else {
            return;
        };
        let (anchor_key, head_key) = {
            let items = rows(self.model.read(cx).core(), scope);
            let (Some(anchor_item), Some(head_item)) = (items.get(anchor.0), items.get(head.0))
            else {
                return;
            };
            (anchor_item.key, head_item.key)
        };
        self.set_selection(
            Some(Selection {
                anchor: Pos {
                    key: anchor_key,
                    offset: anchor.1,
                },
                head: Pos {
                    key: head_key,
                    offset: head.1,
                },
            }),
            cx,
        );
    }

    fn set_selection(&mut self, selection: Option<Selection<ItemKey>>, cx: &mut Context<Self>) {
        let started = !self.has_selection() && selection.is_some_and(|s| !s.is_empty());
        self.selection = selection;
        if started {
            cx.emit(TimelineEvent::SelectionStarted);
        }
        cx.notify();
    }

    /// Where the document order puts `pos`: its row, then its offset.
    fn order_of(&self, pos: Pos<ItemKey>, cx: &App) -> Option<(usize, usize)> {
        let scope = self.scope(cx)?;
        let row = rows(self.model.read(cx).core(), scope).position(pos.key)?;
        Some((row, pos.offset))
    }

    /// The text position under `point`, or nearest to it among the text on screen, and whether
    /// the point is on text. `None` when no text is on screen.
    fn text_at(&self, point: Point<Pixels>, cx: &App) -> Option<(Pos<ItemKey>, bool)> {
        let scope = self.scope(cx)?;
        let state = self.lists.get(&scope)?;
        let viewport = state.viewport_bounds();
        let core = self.model.read(cx).core();
        let items = rows(core, scope);
        let sink = self.sink.borrow();
        // (vertical distance, horizontal distance, message, hit)
        let mut best: Option<(f32, f32, ItemKey, &rich_text::UnitHit)> = None;
        for ((key, _), hit) in sink.iter() {
            let bounds = hit.bounds();
            if bounds.size.height <= px(0.) || bounds.size.width <= px(0.) {
                continue; // Laid out but not painted: its bounds are not a place on screen.
            }
            // Only text on screen now: painted in the latest paint, and inside the list's window.
            if !hit.painted_in(self.epoch.get()) || !viewport.intersects(&bounds) {
                continue;
            }
            let vertical = if point.y < bounds.top() {
                f32::from(bounds.top() - point.y)
            } else if point.y > bounds.bottom() {
                f32::from(point.y - bounds.bottom())
            } else {
                0.0
            };
            let horizontal = if point.x < bounds.left() {
                f32::from(bounds.left() - point.x)
            } else if point.x > bounds.right() {
                f32::from(point.x - bounds.right())
            } else {
                0.0
            };
            if best.is_none_or(|(v, h, ..)| (vertical, horizontal) < (v, h)) {
                best = Some((vertical, horizontal, *key, hit));
            }
        }
        let (vertical, horizontal, key, hit) = best?;
        let bounds = hit.bounds();
        let offset = if vertical > 0.0 && point.y < bounds.top() {
            hit.first_offset()
        } else if vertical > 0.0 {
            hit.last_offset()
        } else {
            hit.offset_at(point)
        };
        let text = items.find(key).map(|item| Self::row_text(core, item))?;
        let offset = selection::snap(&text, offset.min(text.len()));
        Some((Pos { key, offset }, vertical == 0.0 && horizontal == 0.0))
    }

    /// A press on the list's text: begins a click, a drag, or (double and triple clicks) a word or
    /// a message.
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        let hit = self.text_at(event.position, cx);
        let Some((pos, true)) = hit else {
            // Not on text: a click on the gutter, a header, or a control. It clears the selection.
            self.drag = None;
            if self.has_selection() {
                self.selection = None;
                self.end_mouse_selection(cx);
                cx.notify();
            }
            return;
        };
        let Some(scope) = self.scope(cx) else {
            return;
        };
        let (word, whole) = {
            let core = self.model.read(cx).core();
            let text = rows(core, scope)
                .find(pos.key)
                .map(|item| Self::row_text(core, item))
                .unwrap_or_default();
            (selection::word_at(&text, pos.offset), 0..text.len())
        };
        let clicks = event.click_count.clamp(1, 3);
        let origin = match clicks {
            2 => Some((
                Pos {
                    key: pos.key,
                    offset: word.start,
                },
                Pos {
                    key: pos.key,
                    offset: word.end,
                },
            )),
            3 => Some((
                Pos {
                    key: pos.key,
                    offset: whole.start,
                },
                Pos {
                    key: pos.key,
                    offset: whole.end,
                },
            )),
            _ => None,
        };
        match (origin, event.modifiers.shift, self.selection) {
            (Some((start, end)), ..) => {
                self.set_selection(
                    Some(Selection {
                        anchor: start,
                        head: end,
                    }),
                    cx,
                );
            }
            // Shift+click extends what is selected to the press.
            (None, true, Some(existing)) => {
                self.set_selection(
                    Some(Selection {
                        anchor: existing.anchor,
                        head: pos,
                    }),
                    cx,
                );
            }
            (None, ..) => self.selection = Some(Selection::caret(pos)),
        }
        let started = origin.is_some() || (event.modifiers.shift && self.has_selection());
        if started {
            self.selection_by_mouse = true;
            window.focus(&self.focus, cx);
        }
        let ticker = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(DRAG_TICK).await;
                let alive = this
                    .update_in(cx, |this, window, cx| this.drag_tick(window, cx))
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        });
        self.drag = Some(Drag {
            press: event.position,
            started,
            clicks,
            origin,
            _ticker: ticker,
        });
        cx.notify();
    }

    /// Moves the end of a drag selection to the text under `point`.
    fn drag_to(&mut self, point: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pos, _)) = self.text_at(point, cx) else {
            return;
        };
        let Some(scope) = self.scope(cx) else {
            return;
        };
        let (clicks, origin, press_started) = match &self.drag {
            Some(drag) => (drag.clicks, drag.origin, drag.started),
            None => return,
        };
        if !press_started {
            if let Some(drag) = self.drag.as_mut() {
                drag.started = true;
            }
            self.selection_by_mouse = true;
            window.focus(&self.focus, cx);
        }
        let Some(selection) = self.selection else {
            return;
        };
        let next = match origin {
            // Double and triple clicks keep what they first selected and grow by words or messages.
            Some((first, last)) if clicks >= 2 => {
                let (word, whole) = {
                    let core = self.model.read(cx).core();
                    let text = rows(core, scope)
                        .find(pos.key)
                        .map(|item| Self::row_text(core, item))
                        .unwrap_or_default();
                    (selection::word_at(&text, pos.offset), 0..text.len())
                };
                let reach = if clicks == 2 { word } else { whole };
                let (low, high) = (
                    Pos {
                        key: pos.key,
                        offset: reach.start,
                    },
                    Pos {
                        key: pos.key,
                        offset: reach.end,
                    },
                );
                let before_origin = self.order_of(pos, cx) < self.order_of(first, cx);
                if before_origin {
                    Selection {
                        anchor: last,
                        head: low,
                    }
                } else {
                    Selection {
                        anchor: first,
                        head: high,
                    }
                }
            }
            _ => Selection {
                anchor: selection.anchor,
                head: pos,
            },
        };
        self.set_selection(Some(next), cx);
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = &self.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if !drag.started {
            let (dx, dy) = (
                f32::from(event.position.x - drag.press.x),
                f32::from(event.position.y - drag.press.y),
            );
            if (dx * dx + dy * dy).sqrt() < DRAG_THRESHOLD {
                return;
            }
        }
        self.drag_to(event.position, window, cx);
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        // A press that never became a drag was a click: it leaves nothing selected, and ends a
        // selection made with the mouse, which gives the keyboard back to the composer.
        if !drag.started && drag.clicks == 1 && !self.has_selection() {
            self.selection = None;
            self.end_mouse_selection(cx);
        }
        cx.notify();
    }

    /// A selection made with the mouse ended without Escape (a click elsewhere): the keyboard it
    /// took goes back to the composer.
    fn end_mouse_selection(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.selection_by_mouse) {
            cx.emit(TimelineEvent::FocusComposer);
        }
    }

    /// While the button is held: scrolls when the pointer is at or past the top or bottom edge,
    /// and follows the pointer with the selection. Returns whether the drag is still going.
    fn drag_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(drag) = &self.drag else {
            return false;
        };
        if !drag.started {
            return true;
        }
        let pointer = window.mouse_position();
        if let Some(state) = self.scope(cx).and_then(|scope| self.lists.get(&scope)) {
            let viewport = state.viewport_bounds();
            let (top, bottom) = (viewport.top(), viewport.bottom());
            let edge = px(AUTOSCROLL_EDGE);
            let delta = if pointer.y < top + edge {
                -f32::from(top + edge - pointer.y)
            } else if pointer.y > bottom - edge {
                f32::from(pointer.y - (bottom - edge))
            } else {
                0.0
            };
            if delta != 0.0 {
                state.scroll_by(px(delta.clamp(-AUTOSCROLL_MAX, AUTOSCROLL_MAX)));
                cx.notify();
            }
        }
        self.drag_to(pointer, window, cx);
        true
    }

    /// Selects every loaded message, whole.
    fn select_all(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = self.scope(cx) else {
            return;
        };
        let core = self.model.read(cx).core();
        let items = rows(core, scope);
        let (Some(first), Some(last)) = (items.iter().next(), items.last()) else {
            return;
        };
        let end = Self::row_text(core, last).len();
        let selection = Selection {
            anchor: Pos {
                key: first.key,
                offset: 0,
            },
            head: Pos {
                key: last.key,
                offset: end,
            },
        };
        self.keyboard_anchor = None;
        self.set_selection(Some(selection), cx);
    }

    /// Shift+Up and Shift+Down: grows the selection by whole messages from the one the keyboard
    /// cursor was on when it began.
    fn extend_by_messages(&mut self, movement: CursorMove, cx: &mut Context<Self>) {
        let Some(scope) = self.scope(cx) else {
            return;
        };
        let anchor_key = {
            let core = self.model.read(cx).core();
            let items = rows(core, scope);
            if items.is_empty() {
                return;
            }
            let start = self
                .keyboard_anchor
                .or(self.cursor)
                .filter(|key| items.position(*key).is_some())
                .or_else(|| items.last().map(|item| item.key));
            start.expect("there is at least one row")
        };
        let had_anchor = self.keyboard_anchor.is_some();
        // The cursor moves over messages only: a summary line is not a message to select.
        self.cursor_on_summary = false;
        self.cursor_on_attachment = None;
        if !had_anchor {
            // The first press selects the message the cursor is on; the next ones grow it.
            self.cursor = Some(anchor_key);
            self.keyboard_anchor = Some(anchor_key);
        } else {
            self.move_cursor(movement, cx);
            self.cursor_on_summary = false;
            self.cursor_on_attachment = None;
        }
        let core = self.model.read(cx).core();
        let items = rows(core, scope);
        let Some(head_key) = self.cursor else {
            return;
        };
        let (Some(a), Some(h)) = (items.position(anchor_key), items.position(head_key)) else {
            return;
        };
        let (anchor_len, head_len) = (
            items
                .get(a)
                .map_or(0, |item| Self::row_text(core, item).len()),
            items
                .get(h)
                .map_or(0, |item| Self::row_text(core, item).len()),
        );
        let selection = if h >= a {
            Selection {
                anchor: Pos {
                    key: anchor_key,
                    offset: 0,
                },
                head: Pos {
                    key: head_key,
                    offset: head_len,
                },
            }
        } else {
            Selection {
                anchor: Pos {
                    key: anchor_key,
                    offset: anchor_len,
                },
                head: Pos {
                    key: head_key,
                    offset: 0,
                },
            }
        };
        self.selection_by_mouse = false;
        self.set_selection(Some(selection), cx);
    }

    /// Copies the selection; with none, the message under the keyboard cursor. Returns whether
    /// anything was copied.
    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let text = match self.selected_text(cx) {
            Some(text) => Some(text),
            None => self.cursor.and_then(|key| {
                let scope = self.scope(cx)?;
                let core = self.model.read(cx).core();
                let item = rows(core, scope).find(key)?;
                Some(Self::row_text(core, item))
            }),
        };
        match text.filter(|text| !text.is_empty()) {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            }
            None => false,
        }
    }

    // ----- Keyboard cursor and the actions menu -----

    fn row_index(&self, scope: Scope, key: ItemKey, cx: &App) -> Option<usize> {
        rows(self.model.read(cx).core(), scope).position(key)
    }

    /// Whether `item` has a thread summary line the cursor can stop on. Only the conversation
    /// timeline draws one; inside the thread pane the thread is the context.
    fn has_summary_stop(&self, item: &TimelineItem) -> bool {
        self.mode == Mode::Conversation && item.thread.is_some()
    }

    /// How many of `item`'s files the cursor can stop on: those of a message the server has.
    /// The placeholders of a message still being sent have nothing to act on yet.
    fn file_stops(item: &TimelineItem) -> usize {
        if item.is_confirmed() && !item.deleted {
            item.attachments.len()
        } else {
            0
        }
    }

    /// How many stops `item` has: the message, then each of its files, then its summary line.
    fn stop_count(&self, item: &TimelineItem) -> usize {
        1 + Self::file_stops(item) + usize::from(self.has_summary_stop(item))
    }

    /// Which stop of its row the cursor is on: 0 the message, 1.. its files, then the summary.
    fn current_stop(&self, item: &TimelineItem) -> usize {
        if self.cursor_on_summary {
            self.stop_count(item) - 1
        } else {
            self.cursor_on_attachment.map_or(0, |file| 1 + file)
        }
    }

    fn set_stop(&mut self, item: &TimelineItem, stop: usize) {
        let files = Self::file_stops(item);
        self.cursor_on_summary = false;
        self.cursor_on_attachment = None;
        if stop == 0 {
        } else if stop <= files {
            self.cursor_on_attachment = Some(stop - 1);
        } else {
            self.cursor_on_summary = true;
        }
    }

    /// Moves the keyboard cursor by one stop (or to an end) and scrolls it into view.
    ///
    /// A message has a stop for itself, one for each of its files, and one for its thread summary
    /// line if it has one, so Down from the message reaches its first file, then the next, then
    /// the line, then the next message, and Up goes back the same way.
    fn move_cursor(&mut self, movement: CursorMove, cx: &mut Context<Self>) {
        let Some(scope) = self.scope(cx) else {
            return;
        };
        let core = self.model.read(cx).core();
        let items = rows(core, scope);
        if items.is_empty() {
            return;
        }
        let current = self.cursor.and_then(|key| items.position(key));
        let last = items.len() - 1;
        let stops = |index: usize| items.get(index).map_or(1, |item| self.stop_count(item));
        let stop_now = current
            .and_then(|at| items.get(at))
            .map_or(0, |item| self.current_stop(item));
        let (next, next_stop) = match (movement, current) {
            (CursorMove::Home, _) => (0, 0),
            (CursorMove::End, _) | (CursorMove::Older, None) | (CursorMove::Newer, None) => {
                (last, 0)
            }
            (CursorMove::Older, Some(at)) if stop_now > 0 => (at, stop_now - 1),
            (CursorMove::Older, Some(0)) => (0, 0),
            (CursorMove::Older, Some(at)) => (at - 1, stops(at - 1) - 1),
            (CursorMove::Newer, Some(at)) if stop_now + 1 < stops(at) => (at, stop_now + 1),
            (CursorMove::Newer, Some(at)) if at == last => (at, stop_now),
            (CursorMove::Newer, Some(at)) => (at + 1, 0),
        };
        let item = items.get(next).cloned();
        let key = item.as_ref().map(|item| item.key);
        self.cursor = key;
        if let Some(item) = item {
            self.set_stop(&item, next_stop);
        }
        if let Some(state) = self.lists.get(&scope) {
            state.scroll_to_reveal_item(next);
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Only the timeline itself owns these keys. A focused retry button or editor keeps its own.
        if !self.focus.is_focused(window) || self.menu.is_some() {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        // Selecting: Ctrl+A everything loaded, Ctrl+C copy, Shift+Up and Shift+Down by whole
        // messages, Escape to clear.
        if modifiers.secondary(cx) && !modifiers.shift && !modifiers.alt {
            match key {
                "a" => {
                    self.select_all(cx);
                    cx.stop_propagation();
                }
                "c" if self.copy_selection(cx) => cx.stop_propagation(),
                _ => {}
            }
            return;
        }
        if modifiers.shift && !modifiers.secondary(cx) && !modifiers.alt && !modifiers.control {
            match key {
                "up" => self.extend_by_messages(CursorMove::Older, cx),
                "down" => self.extend_by_messages(CursorMove::Newer, cx),
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        if modifiers == gpui::Modifiers::default() && key == "escape" && self.has_selection() {
            let by_mouse = self.selection_by_mouse;
            self.clear_selection(cx);
            // A selection made with the mouse took the keyboard here; Escape gives it back.
            if by_mouse {
                cx.emit(TimelineEvent::FocusComposer);
            }
            cx.stop_propagation();
            return;
        }
        if modifiers != gpui::Modifiers::default() {
            return;
        }
        // Moving the cursor without Shift ends a selection by messages.
        if matches!(key, "up" | "down" | "home" | "end") {
            self.keyboard_anchor = None;
            if self.has_selection() && !self.selection_by_mouse {
                self.selection = None;
            }
        }
        match key {
            "up" => self.move_cursor(CursorMove::Older, cx),
            "down" => self.move_cursor(CursorMove::Newer, cx),
            "home" => self.move_cursor(CursorMove::Home, cx),
            "end" => self.move_cursor(CursorMove::End, cx),
            "enter" if !event.is_held => {
                if let Some(key) = self.cursor {
                    if self.cursor_on_summary {
                        self.open_thread_of(key, cx);
                    } else if let Some(file) = self.cursor_on_attachment {
                        self.activate_attachment(key, file, window, cx);
                    } else {
                        self.open_menu(key, window, cx);
                    }
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// What the person can do to a message, in toolbar order.
    fn available_actions(
        &self,
        core: &ClientCore,
        conversation: ConversationId,
        item: &TimelineItem,
    ) -> Vec<RowAction> {
        let mut actions = Vec::new();
        // What remains of a deleted message that still has replies has nothing to act on.
        if item.deleted {
            return actions;
        }
        if let Some(message) = item.message_id() {
            if core.can_react_message(conversation, message) {
                actions.push(RowAction::React);
            }
            // Replying belongs to the conversation timeline: in the pane the composer is right
            // there, and a reply cannot be replied to.
            if self.mode == Mode::Conversation && core.can_reply_message(conversation, message) {
                actions.push(RowAction::Reply);
            }
            if core.can_edit_message(conversation, message) {
                actions.push(RowAction::Edit);
            }
            if core.can_delete_message(conversation, message) {
                actions.push(RowAction::Delete);
            }
            actions.push(RowAction::CopyLink);
        }
        actions.push(RowAction::CopyText);
        actions
    }

    fn open_menu(&mut self, key: ItemKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(scope) = self.scope(cx) else {
            return;
        };
        let conversation = scope.conversation();
        // Each entry: what it does and the words for it. The reactions already on the message
        // follow "Add reaction…", each as a toggle.
        let entries: Vec<(RowAction, String)> = {
            let core = self.model.read(cx).core();
            let Some(item) = rows(core, scope).find(key) else {
                return;
            };
            let mut entries = Vec::new();
            for action in self.available_actions(core, conversation, item) {
                entries.push((action, action.label().to_owned()));
                if action == RowAction::React {
                    for chip in reactions::chips_for(core, ImageStore::global(cx).read(cx), item) {
                        entries.push((
                            RowAction::ToggleReaction(chip.emoji),
                            reactions::menu_label(&chip),
                        ));
                    }
                }
            }
            // A message with one file offers it by name; with several, each file is a stop of its
            // own.
            if Self::file_stops(item) == 1 {
                entries.push((
                    RowAction::DownloadFile,
                    RowAction::DownloadFile.label().to_owned(),
                ));
                entries.push((
                    RowAction::CopyFileName,
                    RowAction::CopyFileName.label().to_owned(),
                ));
            }
            // Saving and the author's profile come last, so the items above keep their places.
            if !item.deleted {
                if let Some(message) = item.message_id() {
                    let action = if core.is_saved(message) {
                        RowAction::Unsave
                    } else {
                        RowAction::Save
                    };
                    entries.push((action, action.label().to_owned()));
                }
                entries.push((
                    RowAction::ViewProfile,
                    RowAction::ViewProfile.label().to_owned(),
                ));
            }
            entries
        };
        if entries.is_empty() {
            // A deleted message that still has replies has nothing to act on.
            return;
        }
        let actions: Vec<RowAction> = entries.iter().map(|(action, _)| *action).collect();
        let items = entries
            .iter()
            .map(|(action, label)| MenuItem::new(action.id(), label.clone()))
            .collect();
        let menu = cx.new(|cx| Menu::new("message-actions", items, cx));
        let subscription = cx.subscribe_in(
            &menu,
            window,
            move |this, _menu, event: &MenuEvent, window, cx| {
                this.menu = None;
                if let MenuEvent::Select(id) = event
                    && let Some(action) = actions.iter().copied().find(|a| a.id() == id.as_ref())
                {
                    this.perform(key, action, window, cx);
                }
                cx.notify();
            },
        );
        menu.update(cx, |menu, cx| menu.open(window, cx));
        self.menu = Some(OpenMenu {
            key,
            menu,
            _subscription: subscription,
        });
        cx.notify();
    }

    // ----- Files -----

    /// The message in row `key` and its files, if it has any.
    fn files_of(&self, key: ItemKey, cx: &App) -> Option<(Scope, Vec<Attachment>)> {
        let scope = self.scope(cx)?;
        let core = self.model.read(cx).core();
        let item = rows(core, scope).find(key)?;
        (!item.attachments.is_empty() && item.is_confirmed())
            .then(|| (scope, item.attachments.clone()))
    }

    /// Whether `attachment` is shown as an image preview.
    fn is_image(attachment: &Attachment) -> bool {
        lazy_domain::attachment::is_previewable(&attachment.media_type)
    }

    /// Opens the viewer on the `file`th of the message's files, among its pictures.
    fn open_viewer(&mut self, key: ItemKey, file: usize, cx: &mut Context<Self>) {
        let Some((_, files)) = self.files_of(key, cx) else {
            return;
        };
        let Some(chosen) = files.get(file).map(|file| file.id) else {
            return;
        };
        let images: Vec<Attachment> = files.into_iter().filter(Self::is_image).collect();
        let Some(index) = images.iter().position(|image| image.id == chosen) else {
            return;
        };
        self.cursor = Some(key);
        cx.emit(TimelineEvent::OpenViewer { images, index });
    }

    /// Enter on a file: an image opens the viewer; any other file does what its chip's button
    /// says (download, cancel, try again, or show where it was saved).
    fn activate_attachment(
        &mut self,
        key: ItemKey,
        file: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, files)) = self.files_of(key, cx) else {
            return;
        };
        let Some(attachment) = files.get(file) else {
            return;
        };
        if Self::is_image(attachment) {
            self.open_viewer(key, file, cx);
            return;
        }
        let state = self.model.read(cx).core().download(attachment.id).cloned();
        let id = attachment.id;
        match state {
            Some(DownloadState::Downloading { .. }) => {
                self.model.update(cx, |model, cx| {
                    model.dispatch(Input::CancelDownload(id), cx)
                });
            }
            Some(DownloadState::Done { path }) => cx.reveal_path(&path),
            Some(DownloadState::Failed { .. }) | None => self.start_download(key, file, cx),
        }
    }

    /// Saves the `file`th of the message's files: where the person chose before if this is a
    /// retry, otherwise the save dialog asks.
    fn start_download(&mut self, key: ItemKey, file: usize, cx: &mut Context<Self>) {
        let Some((_, files)) = self.files_of(key, cx) else {
            return;
        };
        let Some(attachment) = files.get(file).cloned() else {
            return;
        };
        let id = attachment.id;
        let remembered = self.download_paths.get(&id).cloned();
        self.file_notice = None;
        let this = cx.entity().downgrade();
        downloads::start(
            self.model.clone(),
            attachment,
            remembered,
            move |outcome, cx| {
                this.update(cx, |timeline, cx| {
                    match outcome {
                        Outcome::Started(path) => {
                            timeline.download_paths.insert(id, path);
                        }
                        Outcome::Failed(reason) => timeline.file_notice = Some(reason),
                        Outcome::Cancelled => {}
                    }
                    cx.notify();
                })
                .ok();
            },
            cx,
        );
    }

    /// A download moved on: repaint, and measure the row again when its chip changed height (a
    /// failure explains itself on a second line).
    fn download_changed(&mut self, attachment: AttachmentId, cx: &mut Context<Self>) {
        let failed = matches!(
            self.model.read(cx).core().download(attachment),
            Some(DownloadState::Failed { .. })
        );
        if failed == self.download_failed.contains(&attachment) {
            return;
        }
        if failed {
            self.download_failed.insert(attachment);
        } else {
            self.download_failed.remove(&attachment);
        }
        let Some(scope) = self.scope(cx) else {
            return;
        };
        let row = {
            let core = self.model.read(cx).core();
            let items = rows(core, scope);
            (0..items.len()).find(|&index| {
                items
                    .get(index)
                    .is_some_and(|item| item.attachments.iter().any(|file| file.id == attachment))
            })
        };
        if let (Some(row), Some(state)) = (row, self.lists.get(&scope)) {
            state.remeasure_items(row..row + 1);
        }
    }

    /// Sends what the rows drawn since the last flush asked for: thumbnails from the core, and
    /// decoding of the ones it has. Done after the frame, never while drawing.
    fn flush_requests(&mut self, cx: &mut Context<Self>) {
        self.requests_queued = false;
        let thumbnails = std::mem::take(&mut self.wanted_thumbnails);
        let decodes = std::mem::take(&mut self.wanted_decodes);
        let mut emoji = std::mem::take(&mut self.wanted_emoji);
        emoji.sort_by_key(|id| id.to_string());
        emoji.dedup();
        emoji_images::request(&self.model, &emoji, cx);
        let mut authors = std::mem::take(&mut self.wanted_avatars);
        authors.sort_by_key(|id| id.to_string());
        authors.dedup();
        crate::avatar_images::request(&self.model, &authors, cx);
        if !thumbnails.is_empty() {
            self.model.update(cx, |model, cx| {
                for id in thumbnails {
                    model.dispatch(Input::LoadThumbnail(id), cx);
                }
            });
        }
        if !decodes.is_empty() {
            ImageStore::global(cx).update(cx, |store, cx| {
                for (key, source) in decodes {
                    store.request(key, source, THUMBNAIL_EDGE, cx);
                }
            });
        }
    }

    /// Draws a message's files: a tile for each picture, a chip for anything else. While the
    /// message is still being sent they are chips with the upload's progress.
    fn file_elements(
        &mut self,
        row: FileRow<'_>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let FileRow {
            key,
            row,
            files,
            upload,
            confirmed,
            focused_here,
        } = row;
        let prefix = match self.mode {
            Mode::Conversation => "",
            Mode::Thread => "thread-",
        };
        let mut elements = Vec::new();
        let sending = !confirmed;
        let store = ImageStore::global(cx);
        // Pictures never take more than the column they are in: the pane is narrower than the box.
        // (The list's own state cannot be read while it draws its rows, so the width was noted
        // when the view last rendered.)
        let available = Some(self.column_width)
            .filter(|width| *width > 0.0)
            .map_or(attachments::PREVIEW_MAX_WIDTH, |width| {
                width - crate::message_row::content_inset()
            });
        for (index, attachment) in files.iter().enumerate() {
            let selector = format!("{prefix}message-{row}-file-{index}");
            let element_id = gpui::ElementId::Name(format!("file-{key:?}-{index}").into());
            let focused = focused_here && self.cursor_on_attachment == Some(index);
            if sending {
                let progress = upload
                    .and_then(|upload| upload.files.iter().find(|file| file.id == attachment.id));
                let (fraction, label) = match progress.map(|file| &file.state) {
                    Some(PendingFileState::Uploading { sent, total }) => (
                        Some(attachments::fraction(*sent, *total)),
                        format!(
                            "{}%",
                            (attachments::fraction(*sent, *total) * 100.0).round() as u32
                        ),
                    ),
                    Some(PendingFileState::Uploaded) => (Some(1.0), "Uploaded".to_owned()),
                    Some(PendingFileState::Reading) | None => (None, "Waiting".to_owned()),
                };
                elements.push(attachment_view::file_chip(
                    FileChip {
                        id: element_id,
                        selector,
                        name: attachment.name.clone(),
                        size_text: attachments::format_size(attachment.size),
                        kind: attachments::file_kind(&attachment.media_type, &attachment.name),
                        transfer: Transfer::Uploading { fraction, label },
                        focused: false,
                        on_download: noop(),
                        on_cancel: noop(),
                        on_retry: noop(),
                        on_reveal: noop(),
                    },
                    theme,
                ));
                continue;
            }
            let open: attachment_view::Action = {
                let handler = cx.processor(
                    move |this, (): (), _w: &mut Window, cx: &mut Context<Self>| {
                        this.open_viewer(key, index, cx)
                    },
                );
                Rc::new(move |window, cx| handler((), window, cx))
            };
            if Self::is_image(attachment) {
                let thumbnail_key = ImageKey::Thumbnail(attachment.id);
                let core = self.model.read(cx).core();
                let preview = match store.read(cx).get(&thumbnail_key) {
                    Some(Slot::Ready(decoded)) => Preview::Ready(decoded.image.clone()),
                    Some(Slot::Failed(error)) => Preview::Failed(error.text()),
                    Some(Slot::Decoding) => Preview::Loading,
                    None => match core.thumbnail(attachment.id) {
                        _ if !attachment.has_thumbnail => {
                            Preview::Failed("No preview available.".to_owned())
                        }
                        None => {
                            self.wanted_thumbnails.push(attachment.id);
                            Preview::Loading
                        }
                        Some(ImageState::Loading) => Preview::Loading,
                        Some(ImageState::Failed) => {
                            Preview::Failed("Preview unavailable.".to_owned())
                        }
                        Some(ImageState::Ready(data)) => {
                            self.wanted_decodes
                                .push((thumbnail_key, Source::Data(data.clone())));
                            Preview::Loading
                        }
                    },
                };
                elements.push(attachment_view::image_tile(
                    ImageTile {
                        id: element_id,
                        selector,
                        name: attachment.name.clone(),
                        size: attachments::preview_box_within(
                            attachment.width,
                            attachment.height,
                            available,
                        ),
                        preview,
                        focused,
                        on_open: open,
                    },
                    theme,
                ));
                continue;
            }
            let state = self.model.read(cx).core().download(attachment.id).cloned();
            let transfer = match &state {
                None => Transfer::Idle,
                Some(DownloadState::Downloading { received, total }) => Transfer::Active {
                    fraction: attachments::fraction(*received, *total),
                },
                // Where it went, in a few words: the folder's name.
                Some(DownloadState::Done { path }) => Transfer::Done {
                    path: attachments::saved_where(path),
                },
                Some(DownloadState::Failed { reason }) => Transfer::Failed(reason.clone()),
            };
            let id = attachment.id;
            let on_download: attachment_view::Action = {
                let handler = cx.processor(
                    move |this, (): (), _w: &mut Window, cx: &mut Context<Self>| {
                        this.start_download(key, index, cx)
                    },
                );
                Rc::new(move |window, cx| handler((), window, cx))
            };
            let on_cancel: attachment_view::Action = {
                let model = self.model.clone();
                Rc::new(move |_, cx| {
                    model.update(cx, |model, cx| {
                        model.dispatch(Input::CancelDownload(id), cx)
                    });
                })
            };
            let on_reveal: attachment_view::Action = match &state {
                Some(DownloadState::Done { path }) => {
                    let path = path.clone();
                    Rc::new(move |_, cx| cx.reveal_path(&path))
                }
                _ => noop(),
            };
            elements.push(attachment_view::file_chip(
                FileChip {
                    id: element_id,
                    selector,
                    name: attachment.name.clone(),
                    size_text: attachments::format_size(attachment.size),
                    kind: attachments::file_kind(&attachment.media_type, &attachment.name),
                    transfer,
                    focused,
                    on_download: on_download.clone(),
                    on_cancel,
                    on_retry: on_download,
                    on_reveal,
                },
                theme,
            ));
        }
        elements
    }

    /// Asks the host to open the thread of the message in row `key`, leaving the cursor on it.
    fn open_thread_of(&mut self, key: ItemKey, cx: &mut Context<Self>) {
        let Some(scope) = self.scope(cx) else {
            return;
        };
        let Some(root) = rows(self.model.read(cx).core(), scope)
            .find(key)
            .and_then(|item| item.message_id())
        else {
            return;
        };
        self.cursor = Some(key);
        cx.emit(TimelineEvent::OpenThread {
            conversation: scope.conversation(),
            root,
        });
    }

    /// Carries out an action on a message, from the toolbar or the menu.
    fn perform(
        &mut self,
        key: ItemKey,
        action: RowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scope) = self.scope(cx) else {
            return;
        };
        let conversation = scope.conversation();
        let (message, text, workspace, author) = {
            let core = self.model.read(cx).core();
            let Some(item) = rows(core, scope).find(key) else {
                return;
            };
            (
                item.message_id(),
                Self::row_text(core, item),
                core.conversation(conversation).map(|c| c.workspace_id),
                item.author_id,
            )
        };
        match action {
            RowAction::Save | RowAction::Unsave => {
                if let Some(message_id) = message {
                    self.model.update(cx, |model, cx| {
                        model.dispatch(
                            Input::SetMessageSaved {
                                message_id,
                                saved: action == RowAction::Save,
                            },
                            cx,
                        )
                    });
                }
            }
            RowAction::ViewProfile => cx.emit(TimelineEvent::OpenProfile { user: author }),
            RowAction::CopyText => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            RowAction::DownloadFile => self.start_download(key, 0, cx),
            RowAction::CopyFileName => {
                let name = {
                    let core = self.model.read(cx).core();
                    rows(core, scope)
                        .find(key)
                        .and_then(|item| item.attachments.first())
                        .map(|attachment| attachment.name.clone())
                };
                if let Some(name) = name {
                    cx.write_to_clipboard(ClipboardItem::new_string(name));
                }
            }
            RowAction::CopyLink => {
                if let (Some(message_id), Some(workspace_id)) = (message, workspace) {
                    let link = Permalink {
                        workspace_id,
                        conversation_id: conversation,
                        message_id,
                    };
                    cx.write_to_clipboard(ClipboardItem::new_string(link.to_string()));
                }
            }
            RowAction::React => {
                if let Some(message) = message {
                    let anchor = self.pick_anchor(scope, key, window, cx);
                    cx.emit(TimelineEvent::PickReaction {
                        conversation,
                        message,
                        anchor,
                    });
                }
            }
            RowAction::ToggleReaction(emoji) => self.toggle_reaction(scope, key, emoji, cx),
            RowAction::Reply => {
                // The cursor is on the message itself, not its summary line, so coming back
                // from the thread lands where the person left.
                self.cursor_on_summary = false;
                self.cursor_on_attachment = None;
                self.open_thread_of(key, cx);
            }
            RowAction::Edit => self.begin_edit(scope, key, EditOrigin::Timeline, window, cx),
            RowAction::Delete => {
                if let Some(message) = message {
                    cx.emit(TimelineEvent::ConfirmDelete {
                        conversation,
                        message,
                    });
                }
            }
        }
    }

    /// Where to open the picker for a message: at the pointer if it is on the row (a click on a
    /// button), else at the row's top right (the keyboard).
    fn pick_anchor(
        &self,
        scope: Scope,
        key: ItemKey,
        window: &Window,
        cx: &App,
    ) -> gpui::Point<Pixels> {
        let bounds = self
            .row_index(scope, key, cx)
            .and_then(|index| self.lists.get(&scope)?.bounds_for_item(index));
        let pointer = window.mouse_position();
        match bounds {
            Some(bounds) if bounds.contains(&pointer) => pointer,
            Some(bounds) => point(bounds.right() - px(24.), bounds.top() + px(8.)),
            None => pointer,
        }
    }

    /// Adds the person's reaction, or takes it back if it is already there.
    fn toggle_reaction(
        &mut self,
        scope: Scope,
        key: ItemKey,
        emoji: ReactionEmoji,
        cx: &mut Context<Self>,
    ) {
        let Some(message) = rows(self.model.read(cx).core(), scope)
            .find(key)
            .and_then(|item| item.message_id())
        else {
            return;
        };
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::ToggleReaction {
                    conversation: scope.conversation(),
                    message,
                    emoji,
                },
                cx,
            );
        });
    }

    /// Opens a link from a message: external addresses go to the system, permalinks to the core.
    fn open_link(&mut self, target: LinkTarget, cx: &mut Context<Self>) {
        match target {
            LinkTarget::External(url) => cx.open_url(&url),
            LinkTarget::Permalink(link) => {
                self.model.update(cx, |model, cx| {
                    model.dispatch(Input::OpenPermalink(link), cx)
                });
            }
        }
    }

    // ----- Editing in place -----

    fn remeasure_row(&self, scope: Scope, key: ItemKey, cx: &App) {
        if let (Some(state), Some(index)) = (self.lists.get(&scope), self.row_index(scope, key, cx))
        {
            state.remeasure_items(index..index + 1);
        }
    }

    /// Starts editing the message in row `key`, seeded with its source or the person's unsaved
    /// text. Does nothing if the message cannot be edited.
    ///
    /// `origin` is where the keyboard goes back to when the edit ends (saved, cancelled, or the
    /// message deleted): the composer for an edit begun with Up there, the message's row otherwise.
    pub fn begin_edit(
        &mut self,
        scope: Scope,
        key: ItemKey,
        origin: EditOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conversation = scope.conversation();
        let (message, text) = {
            let core = self.model.read(cx).core();
            let Some(message) = rows(core, scope)
                .find(key)
                .and_then(|item| item.message_id())
            else {
                return;
            };
            if !core.can_edit_message(conversation, message) {
                return;
            }
            let Some(text) = core.edit_text(conversation, message) else {
                return;
            };
            (message, text)
        };
        if let Some(session) = &self.editor
            && session.key == key
            && session.scope == scope
        {
            let input = session.input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        if let Some(previous) = self.editor.take() {
            self.remeasure_row(previous.scope, previous.key, cx);
        }
        let input = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new("Edit message")
                    .accessible_label("Edit message")
                    .max_lines(10)
                    .max_bytes(MAX_TEXT_BYTES)
                    .formatting_shortcuts(true),
                cx,
            )
        });
        input.update(cx, |input, cx| input.set_text(&text, cx));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.editor_command(EditorCommand::Save, window, cx),
                TextInputEvent::Changed => {
                    if let Some(session) = this.editor.as_mut() {
                        session.empty_prompt = false;
                    }
                    cx.notify();
                }
                TextInputEvent::Suggestion(key) => this.editor_suggestion_key(*key, window, cx),
                TextInputEvent::Tab { .. }
                | TextInputEvent::UpWhenEmpty
                | TextInputEvent::PasteAttachments => {}
            },
        );
        // Shortcode suggestions follow the text and the caret.
        let observer = cx.observe(&input, |this, _input, cx| {
            this.refresh_editor_suggestions(cx)
        });
        self.editor = Some(EditSession {
            origin,
            scope,
            conversation,
            message,
            key,
            input: input.clone(),
            empty_prompt: false,
            saving: false,
            save_focus: cx.focus_handle().tab_stop(true),
            cancel_focus: cx.focus_handle().tab_stop(true),
            delete_focus: cx.focus_handle().tab_stop(true),
            keep_focus: cx.focus_handle().tab_stop(true),
            suggestions: Suggestions::default(),
            _subscriptions: vec![subscription, observer],
        });
        self.cursor = Some(key);
        self.cursor_on_summary = false;
        self.cursor_on_attachment = None;
        self.remeasure_row(scope, key, cx);
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// The text input of the open editor, if one is open.
    pub fn editor_input(&self) -> Option<Entity<TextInput>> {
        self.editor.as_ref().map(|session| session.input.clone())
    }

    /// Edits the person's newest message in the selected conversation (or newest reply in the
    /// open thread), for Up in an empty composer. Returns whether there was one to edit.
    pub fn edit_latest_message(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(scope) = self.scope(cx) else {
            return false;
        };
        let conversation = scope.conversation();
        let latest = {
            let core = self.model.read(cx).core();
            rows(core, scope)
                .iter()
                .rev()
                // In the pane Up reaches the person's replies, not the root above them.
                .filter(|item| self.mode == Mode::Conversation || item.thread_root.is_some())
                .find(|item| {
                    item.message_id()
                        .is_some_and(|message| core.can_edit_message(conversation, message))
                })
                .map(|item| item.key)
        };
        match latest {
            Some(key) => {
                self.begin_edit(scope, key, EditOrigin::Composer, window, cx);
                self.editor
                    .as_ref()
                    .is_some_and(|session| session.key == key)
            }
            None => false,
        }
    }

    /// Brings the editor's suggestion list up to date with its text and caret.
    fn refresh_editor_suggestions(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.editor.as_mut() else {
            return;
        };
        let input = session.input.clone();
        let before = session.suggestions.visible();
        let conversation = session.conversation;
        let core = self.model.read(cx).core();
        let mentions = |prefix: &str| mention_source::choices(core, conversation, prefix);
        let custom_emoji = |prefix: &str| emoji_images::choices(core, prefix);
        let sources = Sources {
            mentions: &mentions,
            custom_emoji: &custom_emoji,
        };
        let visible = session.suggestions.refresh(input.read(cx), &sources);
        let wanted = session.suggestions.custom_ids();
        input.update(cx, |input, _| input.set_suggestions_open(visible));
        if !wanted.is_empty() {
            emoji_images::request_later(&self.model, wanted, cx);
        }
        if visible || before {
            cx.notify();
        }
    }

    /// Up, Down, and Enter (or Tab) while the editor's suggestions are open.
    fn editor_suggestion_key(
        &mut self,
        key: SuggestionKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.editor.as_mut() else {
            return;
        };
        match key {
            SuggestionKey::Accept => {
                let selected = session.suggestions.selected();
                self.accept_editor_suggestion(selected, window, cx);
            }
            up_or_down => {
                session.suggestions.highlight(up_or_down);
                cx.notify();
            }
        }
    }

    fn accept_editor_suggestion(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.editor.as_mut() else {
            return;
        };
        let Some(accept) = session.suggestions.accept(index) else {
            return;
        };
        let input = session.input.clone();
        input.update(cx, |input, cx| {
            input.replace_range(accept.replace.clone(), &accept.insert, window, cx);
            input.focus(window, cx);
        });
        let used = match (accept.emoji, accept.custom) {
            (Some(glyph), _) => ReactionEmoji::parse(glyph).ok(),
            (None, Some(id)) => Some(ReactionEmoji::Custom(id)),
            (None, None) => None,
        };
        if let Some(emoji) = used {
            self.model.update(cx, |model, cx| {
                model.dispatch(Input::RecordEmojiUse(emoji), cx)
            });
        }
    }

    fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.editor.take() else {
            return;
        };
        self.remeasure_row(session.scope, session.key, cx);
        self.cursor = Some(session.key);
        self.cursor_on_summary = false;
        self.cursor_on_attachment = None;
        // The keyboard goes back to where the edit began. If that was the message's row and the
        // row is gone (the message was deleted, or another conversation is showing), the composer
        // is the sensible place.
        let row_remains = self.scope(cx) == Some(session.scope)
            && self.row_index(session.scope, session.key, cx).is_some();
        match session.origin {
            EditOrigin::Timeline if row_remains => window.focus(&self.focus, cx),
            _ => cx.emit(TimelineEvent::FocusComposer),
        }
        cx.notify();
    }

    fn editor_command(
        &mut self,
        command: EditorCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.editor.as_mut() else {
            return;
        };
        let (scope, conversation, message, key) = (
            session.scope,
            session.conversation,
            session.message,
            session.key,
        );
        match command {
            EditorCommand::Save | EditorCommand::SaveAnyway => {
                if session.saving {
                    return;
                }
                let text = session.input.read(cx).text().to_owned();
                if text.trim().is_empty() {
                    session.empty_prompt = true;
                    let keep = session.keep_focus.clone();
                    window.focus(&keep, cx);
                    cx.notify();
                    return;
                }
                self.save(scope, message, key, text, window, cx);
            }
            EditorCommand::Cancel => {
                if session.saving {
                    return;
                }
                // Escape closes an open suggestion list before it closes the editor.
                if session.suggestions.visible() {
                    session.suggestions.dismiss();
                    let input = session.input.clone();
                    input.update(cx, |input, _| input.set_suggestions_open(false));
                    cx.notify();
                    return;
                }
                // A failed or conflicted edit is forgotten, so the row stops reporting it.
                let outstanding = rows(self.model.read(cx).core(), scope)
                    .find(key)
                    .is_some_and(|item| {
                        matches!(
                            item.edit,
                            EditState::Failed { .. } | EditState::Conflict { .. }
                        )
                    });
                if outstanding {
                    self.model.update(cx, |model, cx| {
                        model.dispatch(
                            Input::CancelEdit {
                                conversation,
                                message,
                            },
                            cx,
                        )
                    });
                }
                self.close_editor(window, cx);
            }
            EditorCommand::Delete => {
                cx.emit(TimelineEvent::ConfirmDelete {
                    conversation,
                    message,
                });
            }
            EditorCommand::KeepEditing => {
                session.empty_prompt = false;
                let input = session.input.clone();
                input.update(cx, |input, cx| input.focus(window, cx));
                cx.notify();
            }
        }
    }

    /// Sends the editor's text. An edit the core already holds as failed or conflicted, saved
    /// unchanged, is retried as such, so that one whose outcome is unknown is checked with the
    /// server first instead of being sent as a new operation.
    fn save(
        &mut self,
        scope: Scope,
        message: MessageId,
        key: ItemKey,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conversation = scope.conversation();
        let (unchanged_source, held) = {
            let core = self.model.read(cx).core();
            let item = rows(core, scope).find(key);
            let source = item.map(|item| item.document.to_markdown());
            let held = item.and_then(|item| match &item.edit {
                EditState::Failed { text, .. } | EditState::Conflict { text } => Some(text.clone()),
                _ => None,
            });
            (source.as_deref() == Some(text.as_str()), held)
        };
        if unchanged_source && held.is_none() {
            // Nothing to save.
            self.close_editor(window, cx);
            return;
        }
        let input = if held.as_deref() == Some(text.as_str()) {
            Input::RetryEdit {
                conversation,
                message,
            }
        } else {
            Input::EditMessage {
                conversation,
                message,
                text,
            }
        };
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        // What the core did with it decides whether the editor stays.
        let (state, refused) = {
            let core = self.model.read(cx).core();
            let state = rows(core, scope).find(key).map(|item| item.edit.clone());
            (state, core.notice().is_some())
        };
        match state {
            Some(EditState::Saving) => {
                if let Some(session) = self.editor.as_mut() {
                    session.saving = true;
                    let input = session.input.clone();
                    input.update(cx, |input, cx| input.set_enabled(false, cx));
                }
                cx.notify();
            }
            // Refused with a notice: keep the editor and the text.
            _ if refused => cx.notify(),
            _ => self.close_editor(window, cx),
        }
    }

    /// Keeps the editor in step with what the core says about the edit it sent.
    fn follow_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.editor else {
            return;
        };
        let (scope, key, saving) = (session.scope, session.key, session.saving);
        let state = rows(self.model.read(cx).core(), scope)
            .find(key)
            .map(|item| item.edit.clone());
        match state {
            // The message is gone: deleted, or the timeline was reloaded without it.
            None => {
                if self.scope(cx) == Some(scope) {
                    self.close_editor(window, cx);
                }
            }
            Some(edit) if saving => match edit {
                EditState::Saving => {}
                EditState::None => self.close_editor(window, cx),
                EditState::Failed { .. } | EditState::Conflict { .. } => {
                    if let Some(session) = self.editor.as_mut() {
                        session.saving = false;
                        let input = session.input.clone();
                        input.update(cx, |input, cx| {
                            input.set_enabled(true, cx);
                            input.focus(window, cx);
                        });
                    }
                    self.remeasure_row(scope, key, cx);
                    cx.notify();
                }
            },
            Some(_) => {}
        }
    }

    // ----- Jumping to a message -----

    /// Scrolls to and highlights the message a permalink named, then tells the core it is done.
    fn perform_pending_jump(&mut self, scope: Scope, state: &ListState, cx: &mut Context<Self>) {
        let Some(target) = self.pending_jump else {
            return;
        };
        // A reply is shown in its thread, other messages in their conversation.
        let belongs = match scope {
            Scope::Conversation(conversation) => {
                target.conversation == conversation && target.thread.is_none()
            }
            Scope::Thread { root, .. } => target.thread == Some(root),
        };
        if !belongs {
            // A target for another conversation is stale; one for the other kind of view is that
            // view's to perform.
            if self.mode == Mode::Conversation && target.thread.is_none() {
                self.pending_jump = None;
            }
            return;
        }
        let Some(index) = self.row_index(scope, target.key, cx) else {
            return; // Not in the timeline yet; the next change tries again.
        };
        self.pending_jump = None;
        state.scroll_to_reveal_item(index);
        self.cursor = Some(target.key);
        self.cursor_on_summary = false;
        self.cursor_on_attachment = None;
        self.highlight = Some(target.key);
        self.highlight_generation += 1;
        let generation = self.highlight_generation;
        let model = self.model.clone();
        cx.defer(move |cx| {
            model.update(cx, |model, cx| model.dispatch(Input::ClearJumpTarget, cx));
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HIGHLIGHT_FOR).await;
            this.update(cx, |this, cx| {
                if this.highlight_generation == generation {
                    this.highlight = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Everything one row needs besides its message: handlers, keyboard state, and the editor.
    fn render_row(
        &mut self,
        scope: Scope,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = *Theme::get(cx);
        // The row's files first: drawing them can queue requests, which needs the view mutably.
        let file_row = {
            let core = self.model.read(cx).core();
            rows(core, scope).get(index).map(|item| {
                (
                    item.key,
                    item.attachments.clone(),
                    item.upload.clone(),
                    item.is_confirmed(),
                )
            })
        };
        let files = match &file_row {
            Some((key, files, upload, confirmed)) if !files.is_empty() => {
                let focused_here = self.cursor == Some(*key) && self.focus.is_focused(window);
                self.file_elements(
                    FileRow {
                        key: *key,
                        row: index,
                        files,
                        upload: upload.as_ref(),
                        confirmed: *confirmed,
                        focused_here,
                    },
                    &theme,
                    cx,
                )
            }
            _ => Vec::new(),
        };
        let model = self.model.clone();
        let core = model.read(cx).core();
        let conversation = scope.conversation();
        let items = rows(core, scope);
        let Some(item) = items.get(index) else {
            return div().into_any_element();
        };
        let key = item.key;
        let previous = index.checked_sub(1).and_then(|prev| items.get(prev));
        let mut layout = row_layout(previous, item, &self.zone, &self.now);
        if self.mode == Mode::Thread {
            // Inside the pane the thread is the context: no date heading above the root, and the
            // first reply starts a new run under the divider.
            if index == 0 {
                layout.day_separator = None;
            }
            if index == 1 && items.head_is_root() {
                layout.show_header = true;
            }
        }
        let author_name: SharedString = core.display_name(item.author_id).to_owned().into();
        let marker = match scope {
            Scope::Conversation(conversation) => core.unread_marker(conversation),
            Scope::Thread { .. } => None,
        };

        let retry_model = self.model.clone();
        let operation = item.operation_id();
        let on_retry: RetryHandler = Rc::new(move |_window, cx| {
            if let Some(operation) = operation {
                retry_model.update(cx, |model, cx| {
                    model.dispatch(
                        Input::RetryOperation {
                            conversation,
                            operation,
                        },
                        cx,
                    );
                });
            }
        });
        let retry_focus = matches!(item.delivery, Delivery::Failed { .. }).then(|| {
            self.retry_focus
                .borrow_mut()
                .entry(key)
                .or_insert_with(|| cx.focus_handle().tab_stop(true))
                .clone()
        });
        let discard_focus = matches!(item.delivery, Delivery::Failed { .. }).then(|| {
            self.discard_focus
                .borrow_mut()
                .entry(key)
                .or_insert_with(|| cx.focus_handle().tab_stop(true))
                .clone()
        });
        let discard_model = self.model.clone();
        let on_discard: RetryHandler = Rc::new(move |_window, cx| {
            if let Some(operation) = operation {
                discard_model.update(cx, |model, cx| {
                    model.dispatch(
                        Input::DiscardOperation {
                            conversation,
                            operation,
                        },
                        cx,
                    );
                });
            }
        });
        let upload_note = item.upload.as_ref().map(|upload| {
            attachments::uploading_text(upload.uploaded_files(), upload.total_files())
        });

        let on_link: LinkHandler = Rc::new(cx.processor(
            |this, target: LinkTarget, _window: &mut Window, cx: &mut Context<Self>| {
                this.open_link(target, cx)
            },
        ));
        let on_profile: RetryHandler = {
            let (author, this) = (item.author_id, cx.entity().downgrade());
            Rc::new(move |_window, cx| {
                this.update(cx, |_, cx| {
                    cx.emit(TimelineEvent::OpenProfile { user: author })
                })
                .ok();
            })
        };
        let actions = RowActions {
            available: self.available_actions(core, conversation, item),
            on_action: Rc::new(cx.processor(
                move |this, action: RowAction, window: &mut Window, cx: &mut Context<Self>| {
                    this.perform(key, action, window, cx)
                },
            )),
        };
        let store_handle = ImageStore::global(cx);
        let chips = reactions::chips_for(core, store_handle.read(cx), item);
        // The custom emoji this row draws, so their pictures can be asked for after the frame.
        let mut emoji_ids: Vec<lazy_domain::EmojiId> = item.document.custom_emoji();
        emoji_ids.extend(
            item.reactions
                .iter()
                .filter_map(|reaction| reaction.emoji.custom_id()),
        );
        let can_react = item
            .message_id()
            .is_some_and(|message| core.can_react_message(conversation, message));
        let on_toggle_reaction: ReactionHandler = Rc::new(cx.processor(
            move |this, emoji: ReactionEmoji, _window: &mut Window, cx: &mut Context<Self>| {
                this.toggle_reaction(scope, key, emoji, cx)
            },
        ));
        let on_add_reaction: AnchorHandler = Rc::new(cx.processor(
            move |this,
                  anchor: gpui::Point<Pixels>,
                  _window: &mut Window,
                  cx: &mut Context<Self>| {
                if let Some(message) = rows(this.model.read(cx).core(), scope)
                    .find(key)
                    .and_then(|item| item.message_id())
                {
                    cx.emit(TimelineEvent::PickReaction {
                        conversation,
                        message,
                        anchor,
                    });
                }
            },
        ));
        let on_pick_suggestion: PickSuggestion = Rc::new(cx.processor(
            |this, index: usize, window: &mut Window, cx: &mut Context<Self>| {
                this.accept_editor_suggestion(index, window, cx)
            },
        ));
        let on_edit_again: RetryHandler = {
            let handler = cx.processor(
                move |this, (): (), window: &mut Window, cx: &mut Context<Self>| {
                    this.begin_edit(scope, key, EditOrigin::Timeline, window, cx)
                },
            );
            Rc::new(move |window, cx| handler((), window, cx))
        };
        // The editor's suggestion pictures arrive after its list is first drawn: ask again for any
        // that are still missing (a no-op for the ones already requested).
        if let Some(session) = self
            .editor
            .as_ref()
            .filter(|session| session.scope == scope && session.key == key)
        {
            self.wanted_emoji.extend(session.suggestions.custom_ids());
        }
        let editor = self
            .editor
            .as_ref()
            .filter(|session| session.scope == scope && session.key == key)
            .map(|session| EditorProps {
                input: session.input.clone(),
                phase: if session.saving {
                    EditorPhase::Saving
                } else if session.empty_prompt {
                    EditorPhase::EmptyPrompt
                } else {
                    EditorPhase::Editing
                },
                notice: match &item.edit {
                    EditState::Failed { reason, .. } => EditorNotice::Failed(reason.clone()),
                    EditState::Conflict { .. } => EditorNotice::Conflict,
                    EditState::None | EditState::Saving => EditorNotice::None,
                },
                on_command: Rc::new(cx.processor(
                    |this, command: EditorCommand, window: &mut Window, cx: &mut Context<Self>| {
                        this.editor_command(command, window, cx)
                    },
                )),
                save_focus: session.save_focus.clone(),
                cancel_focus: session.cancel_focus.clone(),
                delete_focus: session.delete_focus.clone(),
                keep_focus: session.keep_focus.clone(),
                suggestions: session
                    .suggestions
                    .visible()
                    .then(|| session.suggestions.clone()),
                suggestion_pictures: emoji_images::pictures(
                    store_handle.read(cx),
                    &session.suggestions.custom_ids(),
                ),
                on_pick_suggestion,
            });

        let me = core.me().map(|user| user.id);
        let author_is_me = me == Some(item.author_id);
        let mentions =
            mention_source::MentionTable::for_document(core, &item.document, author_is_me)
                .show_pictures(store_handle.read(cx));
        let mentions_me = !author_is_me && me.is_some_and(|me| item.mentions.includes(me));

        // The thread: a summary line under a root in the conversation, a header over a reply that
        // was also sent there, the divider under the root in the pane.
        let on_open_thread = |root: MessageId| -> RetryHandler {
            let handler = cx.processor(
                move |this, (): (), _window: &mut Window, cx: &mut Context<Self>| {
                    this.cursor = Some(key);
                    cx.emit(TimelineEvent::OpenThread { conversation, root });
                },
            );
            Rc::new(move |window, cx| handler((), window, cx))
        };
        let root_id = item.message_id();
        let focused_here = self.cursor == Some(key) && self.focus.is_focused(window);
        let thread_line = match (self.mode, &item.thread, root_id) {
            (Mode::Conversation, Some(summary), Some(root)) => Some(ThreadLineProps {
                replies: summary.reply_count,
                repliers: summary
                    .recent_repliers
                    .iter()
                    .take(5)
                    .map(|id| {
                        let name = core.display_name(*id);
                        ReplierAvatar {
                            seed: id.as_uuid().as_u128(),
                            initial: name
                                .chars()
                                .next()
                                .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().to_string()),
                            name: name.to_owned(),
                        }
                    })
                    .collect(),
                last_reply: ago_label(&to_zoned(summary.last_reply_at, &self.zone), &self.now),
                unread: core
                    .thread_state(root)
                    .is_some_and(|state| state.following && state.unread_count > 0),
                focused: focused_here && self.cursor_on_summary,
                on_open: on_open_thread(root),
            }),
            _ => None,
        };
        let reply_header = match (self.mode, item.in_channel, item.thread_root) {
            (Mode::Conversation, true, Some(root)) => Some(ReplyHeaderProps {
                excerpt: core.message_excerpt(conversation, root).map(|excerpt| {
                    (
                        SharedString::from(core.display_name(excerpt.author).to_owned()),
                        SharedString::from(first_line(&excerpt.text, 90)),
                    )
                }),
                on_open: on_open_thread(root),
            }),
            _ => None,
        };
        let pane = (self.mode == Mode::Thread).then(|| PaneRowProps {
            root_divider: (index == 0 && item.thread_root.is_none()).then(|| {
                let count = item
                    .thread
                    .as_ref()
                    .map_or(items.len().saturating_sub(1) as u32, |s| s.reply_count);
                match count {
                    0 => "No replies yet".to_owned(),
                    1 => "1 reply".to_owned(),
                    n => format!("{n} replies"),
                }
            }),
            also_sent: item.in_channel.then(|| {
                let title = core.conversation_title(conversation);
                let is_channel = core
                    .conversation(conversation)
                    .is_some_and(|c| c.kind == ConversationKind::Channel);
                if is_channel {
                    format!("Also sent to #{title}")
                } else {
                    format!("Also sent to {title}")
                }
            }),
        });

        let shows_author = layout.show_header;
        let element = render_message_row(RowProps {
            new_messages_divider: marker == Some(key),
            retry_focus,
            discard_focus,
            on_discard,
            upload_note,
            attachments: files,
            index,
            item,
            author_name,
            layout,
            zone: &self.zone,
            now: &self.now,
            theme: &theme,
            on_retry,
            on_link,
            actions,
            chips,
            can_react,
            on_toggle_reaction,
            on_add_reaction,
            focused: focused_here && !self.cursor_on_summary && self.cursor_on_attachment.is_none(),
            highlighted: self.highlight == Some(key),
            editor,
            on_edit_again,
            mentions: &mentions,
            mentions_me,
            thread_line,
            reply_header,
            tombstone: item.deleted,
            pane,
            author_is_bot: core.is_bot(item.author_id),
            author_status: crate::status_mark::StatusMark::lookup(core, item.author_id),
            author_avatar: crate::avatar_images::image_of(
                core,
                store_handle.read(cx),
                item.author_id,
            ),
            author_deactivated: core
                .member(item.author_id)
                .is_some_and(|member| member.deactivated),
            saved: item
                .message_id()
                .is_some_and(|message| core.is_saved(message)),
            on_profile,
            select: Some(SelectProps {
                key,
                range: self
                    .resolved
                    .and_then(|resolved| selection::row_selection(&resolved, index, usize::MAX)),
                sink: &self.sink,
                epoch: &self.epoch,
            }),
            label_prefix: match self.mode {
                Mode::Conversation => "",
                Mode::Thread => "thread-",
            },
        });
        self.wanted_emoji.extend(emoji_ids);
        if shows_author {
            self.wanted_avatars.push(item.author_id);
        }
        if (!self.wanted_thumbnails.is_empty()
            || !self.wanted_decodes.is_empty()
            || !self.wanted_emoji.is_empty()
            || !self.wanted_avatars.is_empty())
            && !self.requests_queued
        {
            self.requests_queued = true;
            let this = cx.entity();
            cx.defer(move |cx| this.update(cx, |timeline, cx| timeline.flush_requests(cx)));
        }
        element
    }

    /// The core forgets a conversation's on-screen report when the person leaves it, so tell it
    /// again, from the list's current state, when they come back.
    fn report_bottom_again(&mut self, scope: Scope, cx: &mut Context<Self>) {
        let (Some(state), Some(reported)) =
            (self.lists.get(&scope), self.bottom_reported.get(&scope))
        else {
            return; // A list created on the next render reports for itself.
        };
        let at_bottom = state.is_following_tail();
        reported.set(Some(at_bottom));
        self.model.update(cx, |model, cx| {
            model.dispatch(bottom_input(scope, at_bottom), cx)
        });
    }

    /// Remeasures the rows that gained or lost the "New messages" divider.
    fn sync_marker(&mut self, conversation: ConversationId, cx: &mut Context<Self>) {
        let marker = self.model.read(cx).core().unread_marker(conversation);
        let previous = self.markers.insert(conversation, marker).flatten();
        if previous == marker {
            return;
        }
        let scope = Scope::Conversation(conversation);
        let Some(state) = self.lists.get(&scope) else {
            return;
        };
        let items = rows(self.model.read(cx).core(), scope);
        for key in [previous, marker].into_iter().flatten() {
            if let Some(index) = items.position(key) {
                state.remeasure_items(index..index + 1);
            }
        }
    }

    /// Advances a pending position restore by one render. See [`PendingRestore`].
    fn advance_restore(&mut self, scope: Scope, cx: &mut Context<Self>) {
        let Some(restore) = self.restore.as_mut() else {
            return;
        };
        if restore.scope != scope {
            return;
        }
        restore.renders += 1;
        let Some(state) = self.lists.get(&scope) else {
            self.restore = None;
            return;
        };
        // The first render after the splice only lays the new rows out.
        let moved = if restore.renders >= 2 {
            state
                .bounds_for_item(restore.item)
                .map(|bounds| bounds.origin.y - restore.y_before)
        } else {
            None
        };
        match moved {
            Some(moved) => {
                if moved.abs() > px(0.5) {
                    state.scroll_by(moved);
                }
                self.restore = None;
            }
            None if restore.renders >= RESTORE_GIVE_UP_AFTER => self.restore = None,
            None => {}
        }
        // Ask for another frame so the next step (or the corrected position) is drawn.
        cx.notify();
    }

    fn list_state(&mut self, scope: Scope, cx: &mut Context<Self>) -> ListState {
        if let Some(state) = self.lists.get(&scope) {
            return state.clone();
        }
        let count = rows(self.model.read(cx).core(), scope).len();
        // A conversation is a chat log, its newest message at the bottom of the window. A thread
        // starts at the top (the root, then the replies below it) and, once it is longer than
        // the pane, follows its newest reply.
        let alignment = match scope {
            Scope::Conversation(_) => ListAlignment::Bottom,
            Scope::Thread { .. } => ListAlignment::Top,
        };
        let state = ListState::new(count, alignment, px(OVERDRAW));
        state.set_follow_mode(FollowMode::Tail);

        let model = self.model.clone();
        // A new list starts at the newest message, so that is what the core is told.
        let reported = Rc::new(Cell::new(Some(true)));
        self.bottom_reported.insert(scope, reported.clone());
        {
            let model = model.clone();
            cx.defer(move |cx| {
                model.update(cx, |model, cx| {
                    model.dispatch(bottom_input(scope, true), cx);
                });
            });
        }
        state.set_scroll_handler(move |event, _window, cx| {
            let at_bottom = event.is_following_tail;
            if reported.get() != Some(at_bottom) {
                reported.set(Some(at_bottom));
                // Dispatching splices nothing here, but the list state is borrowed while its
                // scroll handler runs, so report after the handler returns.
                let model = model.clone();
                cx.defer(move |cx| {
                    model.update(cx, |model, cx| {
                        model.dispatch(bottom_input(scope, at_bottom), cx);
                    });
                });
            }
            if event.visible_range.start > LOAD_OLDER_THRESHOLD {
                return;
            }
            let wants_older = matches!(
                history_of(model.read(cx).core(), scope),
                Some(HistoryStatus::Ready { has_more: true })
            );
            if wants_older {
                // The list state is borrowed while its scroll handler runs, and loading history
                // splices it, so run the request after the handler returns.
                let model = model.clone();
                cx.defer(move |cx| {
                    model.update(cx, |model, cx| {
                        model.dispatch(load_older_input(scope), cx);
                    });
                });
            }
        });
        self.lists.insert(scope, state.clone());
        state
    }

    /// The focus handle of the button that asks again after history failed to load, for tests of
    /// the keyboard.
    pub fn history_retry_focus(&self) -> FocusHandle {
        self.history_retry_focus.clone()
    }

    /// The button that asks the core for the history again.
    fn history_retry(&self, scope: Scope, theme: &Theme) -> gpui::Stateful<gpui::Div> {
        let model = self.model.clone();
        let prefix = match self.mode {
            Mode::Conversation => "",
            Mode::Thread => "thread-",
        };
        form::button(
            theme,
            format!("{prefix}history-retry"),
            states::TRY_AGAIN,
            ButtonKind::Secondary,
            true,
            &self.history_retry_focus,
            move |_, cx| {
                model.update(cx, |model, cx| model.dispatch(load_older_input(scope), cx));
            },
        )
    }

    /// An empty timeline whose first page of history failed: what happened, and a way to ask
    /// again.
    fn failed_state(&self, scope: Scope, detail: &str, theme: &Theme) -> gpui::AnyElement {
        let prefix = match self.mode {
            Mode::Conversation => "",
            Mode::Thread => "thread-",
        };
        div()
            .debug_selector(move || format!("{prefix}history-failed"))
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .px_8()
            .child(
                div()
                    .text_size(theme.type_scale.body)
                    .text_color(theme.colors.text)
                    .child(states::history_failed_title(self.mode == Mode::Thread)),
            )
            .child(
                div()
                    .max_w(px(420.))
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.text_muted)
                    .child(states::sentence(detail)),
            )
            .child(div().mt_1().child(self.history_retry(scope, theme)))
            .into_any_element()
    }

    /// The bar over the top of a timeline that could not fetch the messages before the oldest it
    /// has. Without it the failure would be silent: the list just stops growing upward.
    fn older_failed_bar(&self, scope: Scope, detail: &str, theme: &Theme) -> gpui::AnyElement {
        let prefix = match self.mode {
            Mode::Conversation => "",
            Mode::Thread => "thread-",
        };
        div()
            .debug_selector(move || format!("{prefix}older-failed"))
            // In the flow, above the list, so it never covers the oldest message.
            .flex_none()
            .px_4()
            .py_1p5()
            .flex()
            .items_center()
            .gap_3()
            .bg(theme.colors.danger_surface)
            .border_b_1()
            .border_color(theme.colors.danger)
            .text_size(theme.type_scale.small)
            .text_color(theme.colors.danger)
            .child(div().flex_1().min_w_0().child(states::older_history_failed(
                self.mode == Mode::Thread,
                detail,
            )))
            .child(self.history_retry(scope, theme))
            .into_any_element()
    }

    /// A line in the middle of the timeline's area. `which` names the state for tests:
    /// `loading`, `empty`, or `none-selected`.
    fn empty_state(
        &self,
        which: &'static str,
        heading: Option<&'static str>,
        text: impl Into<SharedString>,
        mascot: Option<gpui::AnyElement>,
        theme: &Theme,
    ) -> gpui::AnyElement {
        let prefix = match self.mode {
            Mode::Conversation => "",
            Mode::Thread => "thread-",
        };
        div()
            .debug_selector(move || format!("{prefix}timeline-{which}"))
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .px_8()
            .text_center()
            .children(mascot)
            .children(heading.map(|heading| {
                crate::fonts::heading(
                    div()
                        .text_size(theme.type_scale.title)
                        .text_color(theme.colors.text)
                        .child(heading),
                )
            }))
            .child(
                div()
                    .text_size(theme.type_scale.body)
                    .text_color(theme.colors.text_muted)
                    .child(text.into()),
            )
            .into_any_element()
    }
}

/// The input that tells the core whether the reader is at the newest row of `scope`.
fn bottom_input(scope: Scope, at_bottom: bool) -> Input {
    match scope {
        Scope::Conversation(conversation) => Input::TimelineAtBottom {
            conversation,
            at_bottom,
        },
        Scope::Thread { root, .. } => Input::ThreadAtBottom { root, at_bottom },
    }
}

/// The input that asks the core for the page of rows before the oldest of `scope`.
fn load_older_input(scope: Scope) -> Input {
    match scope {
        Scope::Conversation(conversation) => Input::LoadOlder { conversation },
        Scope::Thread { root, .. } => Input::LoadOlderReplies { root },
    }
}

/// How loading `scope`'s history stands.
fn history_of(core: &ClientCore, scope: Scope) -> Option<HistoryStatus> {
    match scope {
        Scope::Conversation(conversation) => Some(core.history_status(conversation).clone()),
        Scope::Thread { root, .. } => core.thread(root).map(|view| view.history.clone()),
    }
}

/// The first line of `text`, shortened to `max` characters with an ellipsis.
fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_owned()
    } else {
        let mut cut: String = line.chars().take(max.saturating_sub(1)).collect();
        cut.truncate(cut.trim_end().len());
        cut.push('…');
        cut
    }
}

/// A row's files and where it stands, for drawing them.
struct FileRow<'a> {
    key: ItemKey,
    /// The row's position in the list, for test labels.
    row: usize,
    files: &'a [Attachment],
    /// How far the files of a message still being sent have got.
    upload: Option<&'a lazy_client_core::UploadProgress>,
    /// The server has the message (and so the files).
    confirmed: bool,
    /// The keyboard is on this row.
    focused_here: bool,
}

/// An action that does nothing, for the parts of a chip that are not offered yet.
fn noop() -> attachment_view::Action {
    Rc::new(|_, _| {})
}

impl Render for TimelineView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let model = self.model.clone();
        let scope = self.scope(cx);
        let (count, status, empty_text) = {
            let core = model.read(cx).core();
            let count = scope.map_or(0, |scope| rows(core, scope).len());
            let status = scope.and_then(|scope| history_of(core, scope));
            let empty_text = match scope {
                Some(Scope::Conversation(id)) => {
                    core.conversation(id).map(|conversation| {
                        let title = core.conversation_title(id);
                        match conversation.kind {
                            ConversationKind::Channel if conversation.archived => (
                                None,
                                format!(
                                    "This is the beginning of #{title}. It is archived, so it is read-only."
                                ),
                            ),
                            ConversationKind::Channel => (
                                Some("Start the conversation."),
                                "Send the first message in this channel.".to_owned(),
                            ),
                            ConversationKind::Direct => (
                                Some("Say hello."),
                                format!("This is the start of your conversation with {title}."),
                            ),
                            ConversationKind::Group => (
                                Some("Say hello."),
                                format!(
                                    "This is the start of your group conversation with {title}."
                                ),
                            ),
                        }
                    })
                }
                _ => None,
            };
            (count, status, empty_text)
        };

        let Some(scope) = scope else {
            return match self.mode {
                Mode::Conversation => self.empty_state(
                    "none-selected",
                    Some("A good place to start."),
                    "Pick a conversation from the sidebar, or find your people and say hello.",
                    // Moss welcomes the person here: this is not the message list.
                    crate::brand::moss(cx, crate::brand::Moss::Welcome, 128.)
                        .map(IntoElement::into_any_element),
                    &theme,
                ),
                // No thread is open: the pane is not showing, so there is nothing to draw.
                Mode::Thread => div().into_any_element(),
            };
        };
        if count == 0 {
            let loading = match self.mode {
                Mode::Conversation => "Loading messages…",
                Mode::Thread => "Loading the thread…",
            };
            let (which, heading, text) = match status {
                Some(HistoryStatus::Failed { message }) => {
                    return self.failed_state(scope, &message, &theme);
                }
                Some(HistoryStatus::Ready { .. }) => {
                    let (heading, text) = empty_text.unwrap_or_else(|| {
                        (Some("Say hello."), "Send the first message.".to_owned())
                    });
                    ("empty", heading, text)
                }
                _ => ("loading", None, loading.to_owned()),
            };
            // Never Moss here: this is the place of the message list.
            return self.empty_state(which, heading, text, None, &theme);
        }

        let state = self.list_state(scope, cx);
        self.column_width = f32::from(state.viewport_bounds().size.width);
        self.advance_restore(scope, cx);
        self.now = Zoned::now();
        self.perform_pending_jump(scope, &state, cx);
        if self.cursor.is_none() && self.focus.is_focused(window) {
            self.cursor = rows(self.model.read(cx).core(), scope)
                .last()
                .map(|item| item.key);
        }
        // The selection as it lies on the messages now loaded. One whose message is gone (deleted)
        // is dropped. The rows about to be drawn report where their text lands.
        self.resolved = match self.selection {
            Some(selection) => {
                let items = rows(self.model.read(cx).core(), scope);
                match selection::resolve(&selection, |key| items.position(key)) {
                    Some(resolved) => Some(resolved).filter(|resolved| !resolved.is_empty()),
                    None => {
                        self.selection = None;
                        None
                    }
                }
            }
            None => None,
        };
        self.sink.borrow_mut().clear();
        let selected_messages = self
            .resolved
            .map(|resolved| resolved.last_row - resolved.first_row + 1);

        let older_failed = match &status {
            Some(HistoryStatus::Failed { message }) => {
                Some(self.older_failed_bar(scope, message, &theme))
            }
            _ => None,
        };
        let list_rows = list(
            state.clone(),
            cx.processor(
                move |this, index: usize, window: &mut Window, cx: &mut Context<Self>| {
                    this.render_row(scope, index, window, cx)
                },
            ),
        )
        .size_full();

        // The actions menu hangs off the right edge of the message it was opened on.
        let menu = self.menu.as_ref().map(|open| {
            let anchor_at = self
                .row_index(scope, open.key, cx)
                .and_then(|index| state.bounds_for_item(index))
                .map(|bounds| point(bounds.right() - px(20.), bounds.top() + px(6.)))
                .unwrap_or_else(|| {
                    let viewport = state.viewport_bounds();
                    point(viewport.right() - px(20.), viewport.top() + px(6.))
                });
            deferred(
                anchored()
                    .position(anchor_at)
                    .anchor(Anchor::TopRight)
                    .snap_to_window_with_margin(px(8.))
                    .child(open.menu.clone()),
            )
            .with_priority(1)
        });

        div()
            .id(match self.mode {
                Mode::Conversation => "timeline",
                Mode::Thread => "thread-timeline",
            })
            .key_context(TIMELINE_CONTEXT)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            // Clicking a message must not take the keyboard from the composer: GPUI moves focus
            // to a focusable element on mouse down unless the default is prevented first. The
            // editor and the buttons inside rows focus themselves.
            .capture_any_mouse_down(|_, window, _| window.prevent_default())
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .when_some(selected_messages, |timeline, count| {
                // What assistive technology can be told about the selection.
                timeline.aria_description(if count == 1 {
                    "Text selected in 1 message".to_owned()
                } else {
                    format!("Text selected in {count} messages")
                })
            })
            .size_full()
            .relative()
            .flex()
            .flex_col()
            // Painted before the rows: each paint of the list starts a new count.
            .child({
                let epoch = self.epoch.clone();
                canvas(|_, _, _| (), move |_, (), _, _| epoch.set(epoch.get() + 1))
                    .absolute()
                    .size_0()
            })
            .children(older_failed)
            .child(div().flex_1().min_h_0().child(list_rows))
            .children(menu)
            .into_any_element()
    }
}
