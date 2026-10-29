//! The `TextInput` entity: state, actions, key bindings, and the platform input handler.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    Action, App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, IntoElement, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, Role, ScrollEvent,
    SharedString, UTF16Selection, WeakEntity, Window, actions, div, point, prelude::*, px,
};
use unicode_segmentation::UnicodeSegmentation;

use super::element::TextElement;
use super::layout::LayoutState;
use super::model::{Direction, EditorModel, Unit};
use super::style::{SubmitChord, TextInputOptions, TextInputStyle};
use super::unicode::{floor_char_boundary, next_grapheme_boundary};
use crate::theme::Theme;

/// Key context set on the input's root element. Bindings from [`bind_keys`] apply inside it.
pub const KEY_CONTEXT: &str = "LazyTextInput";

/// What masked inputs display for each grapheme.
const MASK_CHAR: char = '•';

actions!(
    lazy_text_input,
    [
        Backspace,
        DeleteForward,
        DeleteWordBackward,
        DeleteWordForward,
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        MoveWordLeft,
        MoveWordRight,
        MoveLineStart,
        MoveLineEnd,
        MoveDocumentStart,
        MoveDocumentEnd,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectWordLeft,
        SelectWordRight,
        SelectLineStart,
        SelectLineEnd,
        SelectDocumentStart,
        SelectDocumentEnd,
        SelectAll,
        CopySelection,
        CutSelection,
        PasteClipboard,
        Undo,
        Redo,
        Enter,
        InsertNewline,
        SubmitModified,
        Tab,
        TabBackwards,
        ShowCharacterPalette,
        FormatBold,
        FormatItalic,
        FormatStrike,
    ]
);

/// Registers the default key bindings for [`KEY_CONTEXT`]. Call once at startup; calling it again
/// is harmless.
pub fn bind_keys(cx: &mut App) {
    if cx.has_global::<KeysBound>() {
        return;
    }
    cx.set_global(KeysBound);
    cx.bind_keys(editing_bindings(cfg!(target_os = "macos")));
}

/// The keystrokes the input takes for editing, on this platform. A shell shortcut must not be one
/// of them (a test in the commands module compares the two tables).
pub fn editing_keystrokes() -> Vec<gpui::Keystroke> {
    editing_keystrokes_for(cfg!(target_os = "macos"))
}

/// Like [`editing_keystrokes`], for either platform's table.
pub fn editing_keystrokes_for(mac: bool) -> Vec<gpui::Keystroke> {
    editing_bindings(mac)
        .iter()
        .flat_map(|binding| {
            binding
                .keystrokes()
                .iter()
                .map(|keystroke| keystroke.inner().clone())
        })
        .collect()
}

fn editing_bindings(on_mac: bool) -> Vec<KeyBinding> {
    fn bind_for<A: Action>(
        on_mac: bool,
        mac: &'static str,
        other: &'static str,
        action: A,
    ) -> KeyBinding {
        KeyBinding::new(if on_mac { mac } else { other }, action, Some(KEY_CONTEXT))
    }
    macro_rules! bind {
        ($mac:expr, $other:expr, $action:expr $(,)?) => {
            bind_for(on_mac, $mac, $other, $action)
        };
    }

    let mut bindings = vec![
        bind!("backspace", "backspace", Backspace),
        bind!("delete", "delete", DeleteForward),
        bind!("alt-backspace", "ctrl-backspace", DeleteWordBackward),
        bind!("alt-delete", "ctrl-delete", DeleteWordForward),
        bind!("left", "left", MoveLeft),
        bind!("right", "right", MoveRight),
        bind!("up", "up", MoveUp),
        bind!("down", "down", MoveDown),
        bind!("alt-left", "ctrl-left", MoveWordLeft),
        bind!("alt-right", "ctrl-right", MoveWordRight),
        bind!("cmd-left", "home", MoveLineStart),
        bind!("cmd-right", "end", MoveLineEnd),
        bind!("cmd-up", "ctrl-home", MoveDocumentStart),
        bind!("cmd-down", "ctrl-end", MoveDocumentEnd),
        bind!("shift-left", "shift-left", SelectLeft),
        bind!("shift-right", "shift-right", SelectRight),
        bind!("shift-up", "shift-up", SelectUp),
        bind!("shift-down", "shift-down", SelectDown),
        bind!("alt-shift-left", "ctrl-shift-left", SelectWordLeft),
        bind!("alt-shift-right", "ctrl-shift-right", SelectWordRight),
        bind!("cmd-shift-left", "shift-home", SelectLineStart),
        bind!("cmd-shift-right", "shift-end", SelectLineEnd),
        bind!("cmd-shift-up", "ctrl-shift-home", SelectDocumentStart),
        bind!("cmd-shift-down", "ctrl-shift-end", SelectDocumentEnd),
        bind!("cmd-a", "ctrl-a", SelectAll),
        bind!("cmd-c", "ctrl-c", CopySelection),
        bind!("cmd-x", "ctrl-x", CutSelection),
        bind!("cmd-v", "ctrl-v", PasteClipboard),
        bind!("cmd-z", "ctrl-z", Undo),
        bind!("cmd-shift-z", "ctrl-shift-z", Redo),
        bind!("enter", "enter", Enter),
        bind!("shift-enter", "shift-enter", InsertNewline),
        bind!("cmd-enter", "ctrl-enter", SubmitModified),
        bind!("tab", "tab", Tab),
        bind!("shift-tab", "shift-tab", TabBackwards),
        bind!("cmd-b", "ctrl-b", FormatBold),
        bind!("cmd-i", "ctrl-i", FormatItalic),
        bind!("cmd-shift-x", "ctrl-shift-x", FormatStrike),
    ];
    if on_mac {
        bindings.push(bind!(
            "ctrl-cmd-space",
            "ctrl-cmd-space",
            ShowCharacterPalette,
        ));
    } else {
        bindings.push(bind!("ctrl-y", "ctrl-y", Redo));
    }
    bindings
}

/// Every text input that is alive, so the shell can ask whether one is mid-composition or has the
/// keyboard without knowing which view owns it.
#[derive(Default)]
struct LiveInputs(Vec<WeakEntity<TextInput>>);

impl gpui::Global for LiveInputs {}

/// Whether any text input has text in composition (an input method's preedit).
pub fn any_input_composing(cx: &App) -> bool {
    cx.try_global::<LiveInputs>().is_some_and(|live| {
        live.0.iter().any(|weak| {
            weak.upgrade()
                .is_some_and(|input| input.read(cx).is_composing())
        })
    })
}

/// Cancels every composition in progress (Escape belongs to the input method then). Returns
/// whether there was one.
pub fn cancel_any_composition(window: &mut Window, cx: &mut App) -> bool {
    let composing: Vec<Entity<TextInput>> = cx
        .try_global::<LiveInputs>()
        .map(|live| {
            live.0
                .iter()
                .filter_map(WeakEntity::upgrade)
                .filter(|input| input.read(cx).is_composing())
                .collect()
        })
        .unwrap_or_default();
    for input in &composing {
        input.update(cx, |input, cx| input.cancel_composition(window, cx));
    }
    !composing.is_empty()
}

/// The text input that has the keyboard, if one does.
pub fn focused_input(window: &Window, cx: &App) -> Option<Entity<TextInput>> {
    cx.try_global::<LiveInputs>()?
        .0
        .iter()
        .filter_map(WeakEntity::upgrade)
        .find(|input| input.read(cx).is_focused(window))
}

/// Marks that the default bindings are registered, so repeated [`bind_keys`] calls do nothing.
struct KeysBound;
impl gpui::Global for KeysBound {}

/// Events emitted by a [`TextInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputEvent {
    /// The text changed through typing, deleting, pasting, cutting, undo/redo, or an IME
    /// composition update. Not emitted for [`TextInput::set_text`] or [`TextInput::clear`].
    /// While [`TextInput::is_composing`] the text includes uncommitted preedit text.
    Changed,
    /// The submit key was pressed outside an IME composition. The input does not clear itself.
    Submit,
    /// Tab or Shift+Tab was pressed in a single-line input. Nothing was done with it; move focus
    /// if appropriate.
    Tab { backwards: bool },
    /// Up was pressed in an empty multiline input that asked for it
    /// ([`TextInputOptions::report_up_when_empty`]).
    UpWhenEmpty,
    /// Files or an image were pasted (and the input asked for them: see
    /// [`TextInputOptions::paste_attachments`]). Nothing was inserted; the host reads the
    /// clipboard with [`crate::attachments::classify_clipboard`].
    PasteAttachments,
    /// A key that belongs to an open suggestion list ([`TextInput::set_suggestions_open`]). The
    /// input did nothing with it.
    Suggestion(SuggestionKey),
}

/// A key the input hands to an open suggestion list instead of acting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKey {
    Up,
    Down,
    /// Enter, or Tab: take the highlighted suggestion.
    Accept,
}

/// A text editor view: multiline or single-line, optionally masked, with full IME support.
///
/// Create it with `cx.new(|cx| TextInput::new(options, cx))`, call [`bind_keys`] once at startup,
/// and subscribe to [`TextInputEvent`]s. The input draws only text, selection, and caret; give it
/// a background and focus ring by wrapping it.
pub struct TextInput {
    pub(crate) focus_handle: FocusHandle,
    pub(crate) model: EditorModel,
    pub(crate) placeholder: SharedString,
    accessible_label: SharedString,
    pub(crate) single_line: bool,
    pass_through_navigation: bool,
    formatting_shortcuts: bool,
    report_up_when_empty: bool,
    paste_attachments: bool,
    /// While a host's suggestion list is open, Up, Down, Enter, and Tab go to it.
    suggestions_open: bool,
    pub(crate) masked: bool,
    pub(crate) max_lines: usize,
    submit_chord: SubmitChord,
    pub(crate) style: TextInputStyle,
    /// Whether `style` is re-derived from the installed theme on every render.
    themed: bool,
    enabled: bool,
    /// Shaped text, shared with the element that produces it during layout.
    pub(crate) layout: Rc<RefCell<Option<LayoutState>>>,
    /// Scroll offset of the content inside the text area.
    pub(crate) scroll: Point<Pixels>,
    /// Set by anything that moves the caret; the next paint scrolls it into view.
    pub(crate) reveal_caret: bool,
    /// Horizontal position kept across consecutive up/down moves.
    goal_x: Option<Pixels>,
    selecting: bool,
    /// Set by programmatic edits, which have no window at hand; the next paint tells the
    /// platform text services.
    pub(crate) platform_dirty: bool,
    display_cache: RefCell<Option<(u64, SharedString)>>,
    pub(crate) text_cache: Option<(u64, SharedString)>,
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TextInput {
    pub fn new(options: TextInputOptions, cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();
        let live = cx.default_global::<LiveInputs>();
        live.0.retain(|other| other.upgrade().is_some());
        live.0.push(weak);
        Self {
            focus_handle: cx.focus_handle().tab_stop(true),
            model: EditorModel::new(options.max_bytes, options.single_line),
            placeholder: options.placeholder,
            accessible_label: options.accessible_label,
            single_line: options.single_line,
            pass_through_navigation: options.pass_through_navigation,
            formatting_shortcuts: options.formatting_shortcuts,
            report_up_when_empty: options.report_up_when_empty,
            paste_attachments: options.paste_attachments,
            suggestions_open: false,
            masked: options.masked,
            max_lines: options.max_lines.max(1),
            submit_chord: options.submit_chord,
            style: options.style,
            themed: options.themed,
            enabled: options.enabled,
            layout: Rc::new(RefCell::new(None)),
            scroll: point(px(0.), px(0.)),
            reveal_caret: false,
            goal_x: None,
            selecting: false,
            platform_dirty: false,
            display_cache: RefCell::new(None),
            text_cache: None,
        }
    }

    // ---- Public API --------------------------------------------------------------------------

    /// The real text, including preedit text while composing, even in a masked input.
    pub fn text(&self) -> &str {
        self.model.text()
    }

    pub fn is_empty(&self) -> bool {
        self.model.is_empty()
    }

    /// The caret, as a byte offset into [`text`](Self::text) (the end of the selection that was
    /// extended last).
    pub fn cursor(&self) -> usize {
        self.model.cursor()
    }

    /// The selection as byte offsets into [`text`](Self::text): where it began and the end that
    /// moves. Equal when it is only a caret.
    pub fn selection(&self) -> (usize, usize) {
        let selection = self.model.selection();
        (selection.anchor, selection.head)
    }

    /// Puts the selection (or, with equal ends, the caret) at these byte offsets, moved back to
    /// grapheme boundaries and kept inside the text. Does not emit
    /// [`TextInputEvent::Changed`]: it is for restoring a position, not an edit.
    pub fn set_selection(&mut self, anchor: usize, head: usize, cx: &mut Context<Self>) {
        let len = self.model.len();
        self.model.select(anchor.min(len), head.min(len));
        self.after_programmatic_change(cx);
    }

    /// Drops a selection to a caret at its head, for when something else is selected instead.
    pub fn collapse_selection(&mut self, cx: &mut Context<Self>) {
        let (anchor, head) = self.selection();
        if anchor != head {
            self.set_selection(head, head, cx);
        }
    }

    /// Where the caret was last drawn, in window coordinates, for a host that wants to put
    /// something beside it. `None` until the input has been painted with its current text (the
    /// frame after an edit), and while it shows its placeholder.
    pub fn caret_bounds(&self) -> Option<Bounds<Pixels>> {
        let layout = self.layout.borrow();
        let layout = layout.as_ref()?;
        let area = layout.bounds?;
        if layout.revision != self.model.revision() || layout.showing_placeholder {
            return None;
        }
        let at = layout.point_for_offset(self.display_offset(self.model.cursor()));
        Some(Bounds::new(
            area.origin - self.scroll + at,
            gpui::size(self.style.caret_width, layout.line_height),
        ))
    }

    /// Changes whenever the text does; lets a host cache what it derives from the text.
    pub fn revision(&self) -> u64 {
        self.model.revision()
    }

    /// While open, Up, Down, Enter, and Tab are reported as [`TextInputEvent::Suggestion`] and
    /// otherwise ignored, so a host can run a suggestion list over the input. Everything else
    /// keeps working, so typing continues as usual.
    pub fn set_suggestions_open(&mut self, open: bool) {
        self.suggestions_open = open;
    }

    /// Replaces `range` (byte offsets) with `text` as one undo step and puts the caret after it.
    pub fn replace_range(
        &mut self,
        range: Range<usize>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.goal_x = None;
        self.edit(window, cx, |model| model.replace_range(range, text));
    }

    /// Types `text` at the caret, replacing the selection, as one undo step.
    pub fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.edit(window, cx, |model| model.paste(text));
    }

    /// Replaces the text without emitting [`TextInputEvent::Changed`], clearing undo history and
    /// placing the caret at the end.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.model.set_text(text);
        self.after_programmatic_change(cx);
    }

    /// Empties the input without emitting [`TextInputEvent::Changed`].
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_text("", cx);
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus_handle, cx);
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    /// A disabled input rejects edits and platform text input and shows no caret. Selection and
    /// copy still work.
    pub fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.selecting = false;
            self.platform_dirty = true;
            cx.notify();
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Throws away the text in composition, as an input method does when its user cancels (Escape
    /// during a composition). Returns whether there was any.
    pub fn cancel_composition(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(range) = self.model.marked_range_utf16() else {
            return false;
        };
        EntityInputHandler::replace_text_in_range(self, Some(range), "", window, cx);
        true
    }

    /// Whether an IME composition is in progress.
    pub fn is_composing(&self) -> bool {
        self.model.is_composing()
    }

    /// The selection in UTF-16 code units, and whether its head precedes its anchor.
    pub fn selection_utf16(&self) -> (Range<usize>, bool) {
        self.model.selection_utf16()
    }

    /// The composition range in UTF-16 code units.
    pub fn marked_range_utf16(&self) -> Option<Range<usize>> {
        self.model.marked_range_utf16()
    }

    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    pub fn set_style(&mut self, style: TextInputStyle, cx: &mut Context<Self>) {
        if self.style != style {
            self.style = style;
            cx.notify();
        }
    }

    // ---- Display mapping ---------------------------------------------------------------------

    /// What is shaped and painted: the placeholder when empty, bullets when masked, else the text.
    pub(crate) fn display_text(&self) -> SharedString {
        if self.model.is_empty() {
            return self.placeholder.clone();
        }
        if !self.masked {
            return SharedString::from(self.model.text().to_owned());
        }
        let revision = self.model.revision();
        if let Some((cached, text)) = &*self.display_cache.borrow()
            && *cached == revision
        {
            return text.clone();
        }
        let count = self.model.text().graphemes(true).count();
        let text = SharedString::from(MASK_CHAR.to_string().repeat(count));
        *self.display_cache.borrow_mut() = Some((revision, text.clone()));
        text
    }

    /// Display offset for a model offset. Identity unless masked.
    pub(crate) fn display_offset(&self, real: usize) -> usize {
        if self.model.is_empty() {
            return 0;
        }
        if !self.masked {
            return real;
        }
        let text = self.model.text();
        let end = floor_char_boundary(text, real);
        text[..end].graphemes(true).count() * MASK_CHAR.len_utf8()
    }

    /// Model offset for a display offset. Identity unless masked.
    pub(crate) fn real_offset(&self, display: usize) -> usize {
        if self.model.is_empty() {
            return 0;
        }
        if !self.masked {
            return display;
        }
        let text = self.model.text();
        text.grapheme_indices(true)
            .nth(display / MASK_CHAR.len_utf8())
            .map_or(text.len(), |(index, _)| index)
    }

    fn display_range(&self, range: &Range<usize>) -> Range<usize> {
        self.display_offset(range.start)..self.display_offset(range.end)
    }

    fn shared_text(&mut self) -> SharedString {
        let revision = self.model.revision();
        match &self.text_cache {
            Some((cached, text)) if *cached == revision => text.clone(),
            _ => {
                let text = SharedString::from(self.model.text().to_owned());
                self.text_cache = Some((revision, text.clone()));
                text
            }
        }
    }

    // ---- Scrolling ---------------------------------------------------------------------------

    /// Applies a pending caret reveal and clamps the scroll offset for the painted `bounds`.
    pub(crate) fn resolve_scroll(&mut self, bounds: Bounds<Pixels>) {
        let layout = self.layout.borrow();
        let Some(layout) = layout.as_ref() else {
            return;
        };
        let caret_width = self.style.caret_width;
        let current = layout.revision == self.model.revision();
        let mut scroll = self.scroll;

        if self.reveal_caret && current {
            let caret = layout.point_for_offset(self.display_offset(self.model.cursor()));
            if self.single_line {
                if caret.x < scroll.x {
                    scroll.x = caret.x;
                } else if caret.x + caret_width > scroll.x + bounds.size.width {
                    scroll.x = caret.x + caret_width - bounds.size.width;
                }
            } else if caret.y < scroll.y {
                scroll.y = caret.y;
            } else if caret.y + layout.line_height > scroll.y + bounds.size.height {
                scroll.y = caret.y + layout.line_height - bounds.size.height;
            }
            self.reveal_caret = false;
        }

        let max_x = if self.single_line {
            (layout.content_size.width + caret_width - bounds.size.width).max(px(0.))
        } else {
            px(0.)
        };
        let max_y = if self.single_line {
            px(0.)
        } else {
            (layout.content_size.height - bounds.size.height).max(px(0.))
        };
        self.scroll = point(
            scroll.x.max(px(0.)).min(max_x),
            scroll.y.max(px(0.)).min(max_y),
        );
    }

    fn max_scroll(&self) -> Point<Pixels> {
        let layout = self.layout.borrow();
        let Some((layout, bounds)) = layout.as_ref().and_then(|l| Some((l, l.bounds?))) else {
            return point(px(0.), px(0.));
        };
        if self.single_line {
            point(
                (layout.content_size.width - bounds.size.width).max(px(0.)),
                px(0.),
            )
        } else {
            point(
                px(0.),
                (layout.content_size.height - bounds.size.height).max(px(0.)),
            )
        }
    }

    // ---- Change plumbing ---------------------------------------------------------------------

    fn after_programmatic_change(&mut self, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.scroll = point(px(0.), px(0.));
        self.reveal_caret = true;
        self.platform_dirty = true;
        cx.notify();
    }

    fn after_text_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.reveal_caret = true;
        window.notify_text_input_changed(&self.focus_handle);
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn after_selection_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reveal_caret = true;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    fn edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut EditorModel) -> bool,
    ) {
        if self.enabled && apply(&mut self.model) {
            self.after_text_change(window, cx);
        }
    }

    fn select(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut EditorModel) -> bool,
    ) {
        if apply(&mut self.model) {
            self.after_selection_change(window, cx);
        }
    }

    // ---- Editing actions ---------------------------------------------------------------------

    /// Word-level units cover the whole field when masked, so they reveal nothing about content.
    fn unit(&self, unit: Unit) -> Unit {
        if self.masked && unit == Unit::Word {
            Unit::Document
        } else {
            unit
        }
    }

    fn delete_by(
        &mut self,
        direction: Direction,
        unit: Unit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unit = self.unit(unit);
        self.edit(window, cx, |model| model.delete(direction, unit));
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.pass_through_navigation && self.model.is_empty() {
            cx.propagate();
            return;
        }
        self.delete_by(Direction::Backward, Unit::Grapheme, window, cx);
    }

    fn delete_forward(&mut self, _: &DeleteForward, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_by(Direction::Forward, Unit::Grapheme, window, cx);
    }

    fn delete_word_backward(
        &mut self,
        _: &DeleteWordBackward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_by(Direction::Backward, Unit::Word, window, cx);
    }

    fn delete_word_forward(
        &mut self,
        _: &DeleteWordForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete_by(Direction::Forward, Unit::Word, window, cx);
    }

    fn copy(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        let selected = self.model.selected_text();
        if !self.masked && !selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected.to_owned()));
        }
    }

    fn cut(&mut self, _: &CutSelection, window: &mut Window, cx: &mut Context<Self>) {
        if self.masked || !self.enabled || self.model.selection().is_empty() {
            return;
        }
        let selected = self.model.selected_text().to_owned();
        if self.model.delete_selection() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected));
            self.after_text_change(window, cx);
        }
    }

    fn paste(&mut self, _: &PasteClipboard, window: &mut Window, cx: &mut Context<Self>) {
        let item = cx.read_from_clipboard();
        if self.paste_attachments
            && self.enabled
            && item
                .as_ref()
                .is_some_and(|item| crate::attachments::classify_clipboard(item).is_some())
        {
            cx.emit(TextInputEvent::PasteAttachments);
            return;
        }
        if let Some(text) = item.and_then(|item| item.text()) {
            self.edit(window, cx, |model| model.paste(&text));
        }
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(window, cx, EditorModel::undo);
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(window, cx, EditorModel::redo);
    }

    // ---- Enter, Tab ---------------------------------------------------------------------------

    /// Enter commits an active composition instead of submitting; the IME owns that key press.
    fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        if self.commit_composition(window, cx) {
            return;
        }
        if self.suggestions_open {
            cx.emit(TextInputEvent::Suggestion(SuggestionKey::Accept));
            return;
        }
        if self.single_line || self.submit_chord == SubmitChord::Enter {
            cx.emit(TextInputEvent::Submit);
        } else {
            self.edit(window, cx, EditorModel::insert_newline);
        }
    }

    fn insert_newline(&mut self, _: &InsertNewline, window: &mut Window, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        if self.commit_composition(window, cx) {
            return;
        }
        if self.single_line {
            cx.emit(TextInputEvent::Submit);
        } else {
            self.edit(window, cx, EditorModel::insert_newline);
        }
    }

    fn submit_modified(&mut self, _: &SubmitModified, window: &mut Window, cx: &mut Context<Self>) {
        if !self.enabled || self.commit_composition(window, cx) {
            return;
        }
        cx.emit(TextInputEvent::Submit);
    }

    /// Ends an active composition, keeping its text. Returns whether one was active.
    fn commit_composition(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.model.unmark() {
            return false;
        }
        self.after_text_change(window, cx);
        true
    }

    fn tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        self.tab_pressed(false, cx);
    }

    fn tab_backwards(&mut self, _: &TabBackwards, _: &mut Window, cx: &mut Context<Self>) {
        self.tab_pressed(true, cx);
    }

    fn tab_pressed(&mut self, backwards: bool, cx: &mut Context<Self>) {
        if self.suggestions_open && !backwards {
            cx.emit(TextInputEvent::Suggestion(SuggestionKey::Accept));
            return;
        }
        if self.single_line {
            cx.emit(TextInputEvent::Tab { backwards });
        } else {
            cx.propagate();
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    // ---- Movement actions --------------------------------------------------------------------

    fn move_by(
        &mut self,
        direction: Direction,
        unit: Unit,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.goal_x = None;
        let unit = self.unit(unit);
        self.select(window, cx, |model| {
            model.move_cursor(direction, unit, extend)
        });
    }

    /// Home/End go to the edge of the visual row when layout is available, else of the line.
    fn move_to_row_edge(
        &mut self,
        end: bool,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.goal_x = None;
        let target = {
            let layout = self.layout.borrow();
            layout
                .as_ref()
                .filter(|l| l.revision == self.model.revision() && !l.showing_placeholder)
                .map(|layout| {
                    let row_ix = layout.row_for_offset(self.display_offset(self.model.cursor()));
                    let display = if end {
                        layout.caret_end_of_row(row_ix)
                    } else {
                        layout.rows[row_ix].start
                    };
                    self.real_offset(display)
                })
        };
        match target {
            Some(offset) => self.select(window, cx, |model| model.move_to(offset, extend)),
            None => {
                let direction = if end {
                    Direction::Forward
                } else {
                    Direction::Backward
                };
                self.move_by(direction, Unit::Line, extend, window, cx);
            }
        }
    }

    /// Up/Down move by visual row, keeping the horizontal position across consecutive moves.
    /// Past the first or last row they go to the start or end of the text.
    fn move_vertically(
        &mut self,
        down: bool,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.single_line && self.pass_through_navigation {
            cx.propagate();
            return;
        }
        if self.suggestions_open && !extend {
            let key = if down {
                SuggestionKey::Down
            } else {
                SuggestionKey::Up
            };
            cx.emit(TextInputEvent::Suggestion(key));
            return;
        }
        if !down
            && !extend
            && self.report_up_when_empty
            && !self.single_line
            && self.model.is_empty()
        {
            cx.emit(TextInputEvent::UpWhenEmpty);
            return;
        }
        if self.single_line {
            let direction = if down {
                Direction::Forward
            } else {
                Direction::Backward
            };
            self.move_by(direction, Unit::Document, extend, window, cx);
            return;
        }
        let planned = {
            let layout = self.layout.borrow();
            layout
                .as_ref()
                .filter(|l| l.revision == self.model.revision() && !l.showing_placeholder)
                .map(|layout| {
                    let cursor = self.display_offset(self.model.cursor());
                    let row_ix = layout.row_for_offset(cursor);
                    let goal = self
                        .goal_x
                        .unwrap_or_else(|| layout.x_in_row(row_ix, cursor));
                    let target_row = if down {
                        row_ix + 1
                    } else {
                        row_ix.wrapping_sub(1)
                    };
                    let offset = if target_row < layout.rows.len() {
                        self.real_offset(layout.offset_in_row(target_row, goal))
                    } else if down {
                        self.model.len()
                    } else {
                        0
                    };
                    (offset, goal)
                })
        };
        match planned {
            Some((offset, goal)) => {
                self.select(window, cx, |model| model.move_to(offset, extend));
                self.goal_x = Some(goal);
            }
            None => {
                let offset = logical_vertical_target(self.model.text(), self.model.cursor(), down);
                self.goal_x = None;
                self.select(window, cx, |model| model.move_to(offset, extend));
            }
        }
    }

    /// Wraps the selection in a formatting marker pair. Without the option the key is not ours.
    fn format(
        &mut self,
        open: &'static str,
        close: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.formatting_shortcuts || self.single_line || self.masked {
            cx.propagate();
            return;
        }
        self.goal_x = None;
        self.edit(window, cx, |model| model.wrap_selection(open, close));
    }

    fn select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select(window, cx, EditorModel::select_all);
    }

    // ---- Mouse -------------------------------------------------------------------------------

    /// The model offset under a window-coordinate position, if layout matches the current text.
    fn offset_at_position(&self, position: Point<Pixels>) -> Option<usize> {
        let layout = self.layout.borrow();
        let layout = layout.as_ref()?;
        if layout.revision != self.model.revision() {
            return None;
        }
        let bounds = layout.bounds?;
        let local = position - bounds.origin + self.scroll;
        Some(self.real_offset(layout.offset_for_point(local)))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        window.focus(&self.focus_handle, cx);
        window.request_virtual_keyboard();
        let Some(offset) = self.offset_at_position(event.position) else {
            return;
        };
        self.goal_x = None;
        self.selecting = true;
        let masked = self.masked;
        self.select(window, cx, |model| match event.click_count {
            1 => model.move_to(offset, event.modifiers.shift),
            2 if masked => model.select_all(),
            2 => model.select_word_at(offset),
            _ if masked => model.select_all(),
            _ => model.select_line_at(offset),
        });
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting {
            return;
        }
        if let Some(offset) = self.offset_at_position(event.position) {
            self.select(window, cx, |model| model.move_to(offset, true));
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn on_scroll_wheel(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let max = self.max_scroll();
        let delta = event.delta.pixel_delta(self.style.line_height);
        let (current, step, limit) = if self.single_line {
            (self.scroll.x, delta.x, max.x)
        } else {
            (self.scroll.y, delta.y, max.y)
        };
        if limit <= px(0.) {
            return;
        }
        let next = (current - step).max(px(0.)).min(limit);
        if next != current {
            if self.single_line {
                self.scroll.x = next;
            } else {
                self.scroll.y = next;
            }
            self.reveal_caret = false;
            cx.notify();
            cx.stop_propagation();
        }
    }
}

/// Up/Down fallback when no layout exists: move by logical line, keeping the grapheme column.
fn logical_vertical_target(text: &str, offset: usize, down: bool) -> usize {
    use super::unicode::{line_end, line_start};
    let start = line_start(text, offset);
    let column = text[start..offset].graphemes(true).count();
    let target_start = if down {
        let end = line_end(text, offset);
        if end >= text.len() {
            return text.len();
        }
        end + 1
    } else {
        if start == 0 {
            return 0;
        }
        line_start(text, start - 1)
    };
    let target_end = line_end(text, target_start);
    let mut result = target_start;
    for _ in 0..column {
        let next = next_grapheme_boundary(text, result);
        if next > target_end {
            break;
        }
        result = next;
    }
    result
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Follow the live theme: the window can open in one appearance and the system can report
        // the other afterwards. The shaped-text cache is keyed by these colors, so a change
        // reshapes on its own.
        if self.themed
            && let Some(theme) = cx.try_global::<Theme>()
        {
            let style = TextInputStyle::from_theme(theme);
            if self.style != style {
                self.style = style;
            }
        }
        let role = if self.masked {
            Role::PasswordInput
        } else if self.single_line {
            Role::TextInput
        } else {
            Role::MultilineTextInput
        };
        let id = ElementId::NamedInteger("lazy-text-input".into(), cx.entity_id().as_u64());
        let cursor = if self.enabled {
            CursorStyle::IBeam
        } else {
            CursorStyle::Arrow
        };

        let mut root = div()
            .id(id)
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .role(role)
            .aria_label(self.accessible_label.clone())
            .aria_placeholder(self.placeholder.clone())
            .cursor(cursor)
            .w_full()
            .px(self.style.padding_x)
            .py(self.style.padding_y)
            .text_size(self.style.font_size)
            .line_height(self.style.line_height)
            .on_action(cx.listener(|this, _: &FormatBold, w, cx| this.format("*", "*", w, cx)))
            .on_action(cx.listener(|this, _: &FormatItalic, w, cx| this.format("_", "_", w, cx)))
            .on_action(cx.listener(|this, _: &FormatStrike, w, cx| this.format("~", "~", w, cx)))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete_forward))
            .on_action(cx.listener(Self::delete_word_backward))
            .on_action(cx.listener(Self::delete_word_forward))
            .on_action(cx.listener(|this, _: &MoveLeft, w, cx| {
                this.move_by(Direction::Backward, Unit::Grapheme, false, w, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveRight, w, cx| {
                this.move_by(Direction::Forward, Unit::Grapheme, false, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &MoveUp, w, cx| this.move_vertically(false, false, w, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveDown, w, cx| this.move_vertically(true, false, w, cx)),
            )
            .on_action(cx.listener(|this, _: &MoveWordLeft, w, cx| {
                this.move_by(Direction::Backward, Unit::Word, false, w, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveWordRight, w, cx| {
                this.move_by(Direction::Forward, Unit::Word, false, w, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveLineStart, w, cx| {
                this.move_to_row_edge(false, false, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &MoveLineEnd, w, cx| {
                    this.move_to_row_edge(true, false, w, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &MoveDocumentStart, w, cx| {
                this.move_by(Direction::Backward, Unit::Document, false, w, cx)
            }))
            .on_action(cx.listener(|this, _: &MoveDocumentEnd, w, cx| {
                this.move_by(Direction::Forward, Unit::Document, false, w, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectLeft, w, cx| {
                this.move_by(Direction::Backward, Unit::Grapheme, true, w, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectRight, w, cx| {
                this.move_by(Direction::Forward, Unit::Grapheme, true, w, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SelectUp, w, cx| this.move_vertically(false, true, w, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectDown, w, cx| this.move_vertically(true, true, w, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectWordLeft, w, cx| {
                this.move_by(Direction::Backward, Unit::Word, true, w, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWordRight, w, cx| {
                this.move_by(Direction::Forward, Unit::Word, true, w, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectLineStart, w, cx| {
                this.move_to_row_edge(false, true, w, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectLineEnd, w, cx| {
                this.move_to_row_edge(true, true, w, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectDocumentStart, w, cx| {
                this.move_by(Direction::Backward, Unit::Document, true, w, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectDocumentEnd, w, cx| {
                this.move_by(Direction::Forward, Unit::Document, true, w, cx)
            }))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::insert_newline))
            .on_action(cx.listener(Self::submit_modified))
            .on_action(cx.listener(Self::tab))
            .on_action(cx.listener(Self::tab_backwards))
            .on_action(cx.listener(Self::show_character_palette))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll(cx.listener(Self::on_scroll_wheel));
        // The value is withheld from assistive technology when masked.
        if !self.masked {
            root = root.aria_value(self.shared_text());
        }
        root.child(TextElement { input: cx.entity() })
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        if self.masked {
            // Surrounding text stays private; only the preedit text itself is readable.
            let bytes = self.model.range_from_utf16(&range_utf16);
            let inside_preedit = self
                .model
                .marked_range()
                .is_some_and(|marked| bytes.start >= marked.start && bytes.end <= marked.end);
            if !inside_preedit {
                let at = range_utf16.start.min(self.model.len_utf16());
                *actual_range = Some(at..at);
                return Some(String::new());
            }
        }
        let (text, actual) = self.model.text_for_range_utf16(&range_utf16);
        *actual_range = Some(actual);
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if !self.enabled && !ignore_disabled_input {
            return None;
        }
        let (range, reversed) = self.model.selection_utf16();
        Some(UTF16Selection { range, reversed })
    }

    fn set_selected_text_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.goal_x = None;
        self.select(window, cx, |model| model.set_selection_utf16(&range_utf16));
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.model.len_utf16())
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.model.marked_range_utf16()
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.unmark() {
            window.notify_text_input_changed(&self.focus_handle);
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        let was_composing = self.model.is_composing();
        // A bare Tab key press that no action handled arrives here as "\t". Typing never
        // inserts a tab character; pasted tabs go through `paste` and are kept.
        if text == "\t" && range_utf16.is_none() && !was_composing {
            return;
        }
        let changed = self.model.replace_text_in_range_utf16(range_utf16, text);
        if changed || was_composing {
            self.after_text_change(window, cx);
        } else {
            self.after_selection_change(window, cx);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        let changed =
            self.model
                .replace_and_mark_utf16(range_utf16, new_text, new_selected_range_utf16);
        if changed {
            self.after_text_change(window, cx);
        } else {
            self.reveal_caret = true;
            window.notify_text_input_changed(&self.focus_handle);
            cx.notify();
        }
    }

    /// Bounds of the first visual fragment of `range_utf16`, which is where the IME candidate
    /// window is placed. `None` until the current text has been laid out and painted.
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.borrow();
        let layout = layout
            .as_ref()
            .filter(|l| l.revision == self.model.revision())?;
        let range = self.display_range(&self.model.range_from_utf16(&range_utf16));
        let row_ix = layout.row_for_offset(range.start);
        let row = &layout.rows[row_ix];
        let end = range.end.min(row.end).max(range.start);
        let origin = element_bounds.origin - self.scroll;
        let (x0, x1) = (
            layout.x_in_row(row_ix, range.start),
            layout.x_in_row(row_ix, end),
        );
        Some(Bounds::from_corners(
            origin + point(x0, row.y),
            origin + point(x1, row.y + layout.line_height),
        ))
    }

    fn selection_bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Vec<Bounds<Pixels>> {
        let layout = self.layout.borrow();
        let Some(layout) = layout
            .as_ref()
            .filter(|l| l.revision == self.model.revision())
        else {
            return Vec::new();
        };
        let range = self.display_range(&self.model.range_from_utf16(&range_utf16));
        let origin = element_bounds.origin - self.scroll;
        layout
            .selection_rects(range)
            .into_iter()
            .map(|rect| Bounds::new(origin + rect.origin, rect.size))
            .collect()
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let inside = self
            .layout
            .borrow()
            .as_ref()
            .and_then(|l| l.bounds)?
            .contains(&position);
        if !inside {
            return None;
        }
        let offset = self.offset_at_position(position)?;
        Some(self.model.offset_to_utf16(offset))
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        self.enabled
    }
}
