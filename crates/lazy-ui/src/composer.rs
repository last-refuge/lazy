//! The message composer: a growing text input bound to the selected conversation's draft.
//!
//! The core owns drafts. The composer reports edits with `SetDraft`, sends with `SendMessage`,
//! and follows the core when the draft changes underneath it (a send clears it, a conversation
//! switch loads another). Text is only replaced when it differs, so an IME composition in
//! progress is never overwritten.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    Anchor, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    PathPromptOptions, Pixels, Point, Render, Role, Subscription, Window, anchored, deferred, div,
    point, prelude::*, px, relative,
};
use lazy_client_core::{
    AttachmentState, Change, ComposerAttachment, ComposerTarget, FileBytes, FileSource, Input,
    LocalFile,
};
use lazy_domain::document::MAX_TEXT_BYTES;
use lazy_domain::{ConversationId, MessageId, ReactionEmoji};

use crate::app_model::{AppEvent, AppModel};
use crate::attachment_view::{self, TrayChip, TrayState};
use crate::attachments::{self, Pasted};
use crate::emoji_font;
use crate::emoji_images;
use crate::form;
use crate::image_store::{CHIP_EDGE, ImageKey, ImageStore, Slot, Source};
use crate::keyboard;
use crate::mention_source;
use crate::states;
use crate::suggest::{self, PickSuggestion, Sources, Suggestions};
use crate::text_input::{
    SuggestionKey, TextInput, TextInputEvent, TextInputOptions, TextInputStyle,
};
use crate::theme::Theme;

/// A text-input style that takes its colors and metrics from the theme.
pub fn input_style(theme: &Theme) -> TextInputStyle {
    TextInputStyle::from_theme(theme)
}

/// Requests the composer passes to its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerEvent {
    /// Up was pressed in the empty composer: the person wants to edit their latest message.
    EditPrevious,
    /// The person pressed the emoji button: open the picker above `anchor` (a window position) and
    /// hand the choice back through [`Composer::insert_emoji`].
    PickEmoji { anchor: Point<Pixels> },
    /// Text in the box was selected: there is one selection at a time, so the host clears the
    /// others.
    SelectionStarted,
}

/// What a composer writes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerMode {
    /// A new message in the selected conversation.
    Conversation,
    /// A reply in the open thread (the thread pane's composer).
    Thread,
}

/// The test labels of a composer's controls. Both composers can be on screen at once, so the
/// thread composer's are distinct.
struct Names {
    suggestions: &'static str,
    emoji: &'static str,
    send: &'static str,
    also: &'static str,
    attach: &'static str,
    tray: &'static str,
}

impl ComposerMode {
    fn names(self) -> Names {
        match self {
            Self::Conversation => Names {
                suggestions: "composer-suggestions",
                emoji: "composer-emoji",
                send: "send-message",
                also: "composer-also",
                attach: "composer-attach",
                tray: "composer-tray",
            },
            Self::Thread => Names {
                suggestions: "thread-composer-suggestions",
                emoji: "thread-composer-emoji",
                send: "thread-send-reply",
                also: "thread-also-in-channel",
                attach: "thread-composer-attach",
                tray: "thread-composer-tray",
            },
        }
    }
}

pub struct Composer {
    model: Entity<AppModel>,
    mode: ComposerMode,
    input: Entity<TextInput>,
    /// The conversation written to (for a reply, the thread's conversation).
    conversation: Option<ConversationId>,
    /// The thread's root, in thread mode.
    root: Option<MessageId>,
    /// Whether a reply is also sent to the channel. Starts off for each thread.
    also_in_channel: bool,
    also_focus: FocusHandle,
    send_focus: FocusHandle,
    emoji_focus: FocusHandle,
    /// The Attach button.
    attach_focus: FocusHandle,
    /// The tray of files above the box: one tab stop, with a cursor over its chips.
    tray_focus: FocusHandle,
    tray_cursor: usize,
    /// Scrolls the chips when there are more than two rows of them.
    tray_scroll: gpui::ScrollHandle,
    /// Why files could not be added (the picker failed, a folder was dropped), until the next
    /// attempt.
    file_error: Option<String>,
    /// Where the emoji button was last drawn, to open the picker beside it.
    emoji_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Emoji shortcode and mention suggestions for what is being typed.
    suggestions: Suggestions,
    /// The caret or selection last reported to the core (or restored from it), so only a move is
    /// reported.
    last_selection: (usize, usize),
    /// Where the caret was last seen, so the list can stay beside it in the frame after an edit,
    /// before the input has been painted with the new text.
    last_caret: Option<Point<Pixels>>,
    /// A repaint is already queued to pick up the caret's new position.
    caret_repaint_queued: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    /// The composer for the selected conversation.
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::with_mode(model, ComposerMode::Conversation, window, cx)
    }

    /// The composer for replies in the open thread.
    pub fn for_thread(
        model: Entity<AppModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_mode(model, ComposerMode::Thread, window, cx)
    }

    fn with_mode(
        model: Entity<AppModel>,
        mode: ComposerMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let label = match mode {
            ComposerMode::Conversation => "Message",
            ComposerMode::Thread => "Reply",
        };
        let input = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new(label)
                    .accessible_label(label)
                    .max_lines(8)
                    .max_bytes(MAX_TEXT_BYTES)
                    .formatting_shortcuts(true)
                    .report_up_when_empty(true)
                    .paste_attachments(true),
                cx,
            )
        });

        let input_events = cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Changed => this.report_draft(cx),
                TextInputEvent::Submit => this.submit(cx),
                TextInputEvent::Tab { .. } => {}
                TextInputEvent::UpWhenEmpty => cx.emit(ComposerEvent::EditPrevious),
                TextInputEvent::PasteAttachments => this.paste_attachments(cx),
                TextInputEvent::Suggestion(key) => this.suggestion_key(*key, window, cx),
            },
        );
        // Shortcode suggestions follow the text and the caret.
        let input_changes = cx.observe(&input, |this, _input, cx| {
            this.refresh_suggestions(cx);
            this.report_selection(cx);
        });
        // The pictures of custom emoji on offer arrive after the list is first drawn.
        let pictures = cx.observe(&ImageStore::global(cx), |this, _store, cx| {
            if this.suggestions.visible() {
                cx.notify();
            }
        });
        let model_events = cx.subscribe_in(
            &model,
            window,
            |this, _model, event: &AppEvent, window, cx| {
                let AppEvent::Changed(changes) = event;
                this.follow_core(changes, window, cx);
            },
        );

        let mut composer = Self {
            model,
            mode,
            input,
            conversation: None,
            root: None,
            also_in_channel: false,
            also_focus: cx.focus_handle().tab_stop(true),
            send_focus: cx.focus_handle().tab_stop(true),
            emoji_focus: cx.focus_handle().tab_stop(true),
            attach_focus: cx.focus_handle().tab_stop(true),
            tray_focus: cx.focus_handle().tab_stop(true),
            tray_cursor: 0,
            tray_scroll: gpui::ScrollHandle::new(),
            file_error: None,
            emoji_bounds: Rc::default(),
            suggestions: Suggestions::default(),
            last_selection: (0, 0),
            last_caret: None,
            caret_repaint_queued: false,
            _subscriptions: vec![input_events, input_changes, pictures, model_events],
        };
        composer.switch_conversation(window, cx);
        composer
    }

    /// What the suggestion list offers right now (empty while it is closed), for tests.
    pub fn suggestion_items(&self) -> &[suggest::Item] {
        self.suggestions.items()
    }

    /// The text input, for hosts and tests that need to reach it.
    pub fn input(&self) -> &Entity<TextInput> {
        &self.input
    }

    /// Puts the caret in the composer, for after a dialog has moved the person to a conversation.
    pub fn focus_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.input.focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Types `glyph` at the caret and puts the caret back in the input, for an emoji chosen in the
    /// picker.
    pub fn insert_emoji(&mut self, glyph: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.insert_text(glyph, window, cx);
            input.focus(window, cx);
        });
    }

    /// Brings the suggestion list up to date with the text and caret.
    fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        let before = self.suggestions.visible();
        let conversation = self.conversation;
        let core = self.model.read(cx).core();
        let mentions = |prefix: &str| {
            conversation
                .map(|id| mention_source::choices(core, id, prefix))
                .unwrap_or_default()
        };
        let custom_emoji = |prefix: &str| emoji_images::choices(core, prefix);
        let sources = Sources {
            mentions: &mentions,
            custom_emoji: &custom_emoji,
        };
        let visible = self.suggestions.refresh(self.input.read(cx), &sources);
        self.input
            .update(cx, |input, _| input.set_suggestions_open(visible));
        // The pictures of the custom emoji on offer are fetched after the frame that shows them.
        let wanted = self.suggestions.custom_ids();
        if !wanted.is_empty() {
            emoji_images::request_later(&self.model, wanted, cx);
        }
        if visible || before {
            cx.notify();
        }
    }

    /// Up, Down, and Enter (or Tab) while suggestions are open.
    fn suggestion_key(&mut self, key: SuggestionKey, window: &mut Window, cx: &mut Context<Self>) {
        match key {
            SuggestionKey::Accept => {
                self.accept_suggestion(self.suggestions.selected(), window, cx)
            }
            up_or_down => {
                self.suggestions.highlight(up_or_down);
                cx.notify();
            }
        }
    }

    fn accept_suggestion(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(accept) = self.suggestions.accept(index) else {
            return;
        };
        self.input.update(cx, |input, cx| {
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

    /// Whether the composer accepts input: the conversation exists and is not archived, and for a
    /// reply the root can still be replied to.
    fn can_post(&self, cx: &gpui::App) -> bool {
        let core = self.model.read(cx).core();
        let open = self
            .conversation
            .and_then(|id| core.conversation(id))
            .is_some_and(|conversation| !conversation.archived);
        match (self.mode, self.conversation, self.root) {
            (ComposerMode::Conversation, ..) => open,
            (ComposerMode::Thread, Some(conversation), Some(root)) => {
                open && core.can_reply_message(conversation, root)
            }
            (ComposerMode::Thread, ..) => false,
        }
    }

    /// What this composer is writing to now: the selected conversation, or the open thread.
    fn target(&self, cx: &Context<Self>) -> (Option<ConversationId>, Option<MessageId>) {
        let core = self.model.read(cx).core();
        match self.mode {
            ComposerMode::Conversation => (core.selected_conversation(), None),
            ComposerMode::Thread => match core.open_thread() {
                Some(open) => (Some(open.conversation), Some(open.root)),
                None => (None, None),
            },
        }
    }

    /// The saved draft of the current target.
    fn saved_draft(&self, cx: &Context<Self>) -> String {
        let core = self.model.read(cx).core();
        match (self.conversation, self.root) {
            (_, Some(root)) => core
                .thread(root)
                .map(|view| view.draft.to_owned())
                .unwrap_or_default(),
            (Some(conversation), None) => core.draft(conversation).to_owned(),
            (None, None) => String::new(),
        }
    }

    /// Loads the current target's draft into the input and focuses it.
    fn switch_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        (self.conversation, self.root) = self.target(cx);
        // Each thread starts with replies going to the thread only.
        self.also_in_channel = false;
        let draft = self.saved_draft(cx);
        let enabled = self.can_post(cx);
        // The caret and selection the draft had when it was left, if the core kept them.
        let saved = {
            let core = self.model.read(cx).core();
            match (self.conversation, self.root) {
                (_, Some(root)) => core.thread_draft_selection(root),
                (Some(conversation), None) => core.draft_selection(conversation),
                (None, None) => None,
            }
        };
        let restored = saved.map_or((draft.len(), draft.len()), |saved| {
            (saved.anchor, saved.head)
        });
        self.input.update(cx, |input, cx| {
            input.set_text(&draft, cx);
            input.set_selection(restored.0, restored.1, cx);
            input.set_enabled(enabled, cx);
        });
        // What was restored is not a move to report back.
        self.last_selection = self.input.read(cx).selection();
        if enabled {
            let handle = self.input.focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    fn follow_core(&mut self, changes: &[Change], window: &mut Window, cx: &mut Context<Self>) {
        if self.target(cx) != (self.conversation, self.root) {
            self.switch_conversation(window, cx);
            return;
        }
        let people_changed = changes.iter().any(|change| match change {
            Change::GroupsChanged | Change::DirectoryChanged => true,
            Change::ConversationMembersChanged { conversation } => {
                Some(*conversation) == self.conversation
            }
            _ => false,
        });
        if people_changed {
            // Who can be mentioned changed under a mention that is being typed.
            self.suggestions.invalidate();
            self.refresh_suggestions(cx);
        }
        for change in changes {
            if let Change::ComposerAttachmentsChanged { target } = *change
                && Some(target) == self.composer_target()
            {
                let count = self.chips(cx).len().min(attachments::MAX_CHIPS_SHOWN);
                self.tray_cursor = self.tray_cursor.min(count.saturating_sub(1));
                cx.notify();
            }
            if let Change::ConversationStateChanged { conversation } = *change
                && Some(conversation) == self.conversation
            {
                // Archiving (or unarchiving) toggles whether the composer accepts input.
                let enabled = self.can_post(cx);
                self.input
                    .update(cx, |input, cx| input.set_enabled(enabled, cx));
                cx.notify();
            }
            let draft_changed = match (self.mode, *change) {
                (ComposerMode::Conversation, Change::DraftChanged { conversation }) => {
                    Some(conversation) == self.conversation
                }
                (ComposerMode::Thread, Change::ThreadDraftChanged { root }) => {
                    Some(root) == self.root
                }
                _ => false,
            };
            if draft_changed {
                let draft = self.saved_draft(cx);
                self.input.update(cx, |input, cx| {
                    if input.text() != draft {
                        input.set_text(&draft, cx);
                    }
                });
                cx.notify();
            }
            // Whether replies are still allowed can change under an open thread: the root was
            // deleted, or the channel archived.
            if self.mode == ComposerMode::Thread
                && matches!(
                    change,
                    Change::ThreadRootChanged { .. } | Change::ConversationStateChanged { .. }
                )
            {
                let enabled = self.can_post(cx);
                self.input
                    .update(cx, |input, cx| input.set_enabled(enabled, cx));
                cx.notify();
            }
        }
    }

    /// Tells the core where the caret or selection moved to (it saves that with the draft), and
    /// the host when text became selected.
    fn report_selection(&mut self, cx: &mut Context<Self>) {
        let (anchor, head) = self.input.read(cx).selection();
        if (anchor, head) == self.last_selection {
            return;
        }
        self.last_selection = (anchor, head);
        let Some(conversation) = self.conversation else {
            return;
        };
        let input = match self.root {
            Some(root) => Input::SetThreadDraftSelection { root, anchor, head },
            None => Input::SetDraftSelection {
                conversation,
                anchor,
                head,
            },
        };
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        if anchor != head {
            cx.emit(ComposerEvent::SelectionStarted);
        }
    }

    fn report_draft(&mut self, cx: &mut Context<Self>) {
        let Some(conversation) = self.conversation else {
            return;
        };
        let text = self.input.read(cx).text().to_owned();
        let input = match self.root {
            Some(root) => Input::SetThreadDraft { root, text },
            None => Input::SetDraft { conversation, text },
        };
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.notify();
    }

    // ----- Files -----

    /// Which composer's files these are.
    fn composer_target(&self) -> Option<ComposerTarget> {
        match (self.mode, self.conversation, self.root) {
            (ComposerMode::Thread, _, Some(root)) => Some(ComposerTarget::Thread(root)),
            (ComposerMode::Conversation, Some(conversation), _) => {
                Some(ComposerTarget::Conversation(conversation))
            }
            _ => None,
        }
    }

    /// The files waiting in this composer, oldest first.
    fn chips(&self, cx: &gpui::App) -> Vec<ComposerAttachment> {
        self.composer_target()
            .map(|target| {
                self.model
                    .read(cx)
                    .core()
                    .composer_attachments(target)
                    .to_vec()
            })
            .unwrap_or_default()
    }

    /// What the drop overlay says while files are dragged over this composer's side of the
    /// window.
    pub fn drop_label(&self, cx: &gpui::App) -> String {
        let core = self.model.read(cx).core();
        let Some(id) = self.conversation else {
            return "Select a conversation to attach files.".to_owned();
        };
        let title = core.conversation_title(id);
        let is_channel = core
            .conversation(id)
            .is_some_and(|c| c.kind == lazy_domain::ConversationKind::Channel);
        let place = if is_channel {
            format!("#{title}")
        } else {
            title
        };
        if !self.can_post(cx) {
            return format!("{place} is archived. Files can't be attached.");
        }
        match self.mode {
            ComposerMode::Conversation => format!("Drop files to attach to {place}"),
            ComposerMode::Thread => format!("Drop files to attach to your reply in {place}"),
        }
    }

    /// The files in the tray, for tests.
    pub fn attached_files(&self, cx: &gpui::App) -> Vec<ComposerAttachment> {
        self.chips(cx)
    }

    fn has_failed_file(&self, cx: &gpui::App) -> bool {
        self.chips(cx)
            .iter()
            .any(|chip| matches!(chip.state, AttachmentState::Failed { .. }))
    }

    /// Adds files by path: from the file dialog, a drop, or pasted file paths. A folder is
    /// refused with a line saying so; the rest are added.
    pub fn attach_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if !self.can_post(cx) {
            return;
        }
        let Some(target) = self.composer_target() else {
            return;
        };
        let mut files = Vec::new();
        self.file_error = None;
        for path in paths {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_owned());
            if path.is_dir() {
                self.file_error = Some(attachments::folder_text(&name));
                continue;
            }
            files.push(LocalFile {
                source: FileSource::Path(path),
                name,
            });
        }
        if !files.is_empty() {
            self.model.update(cx, |model, cx| {
                model.dispatch(Input::AttachFiles { target, files }, cx)
            });
        }
        cx.notify();
    }

    /// Adds a picture that has no file, such as one pasted from the clipboard.
    fn attach_pasted_png(&mut self, png: Vec<u8>, cx: &mut Context<Self>) {
        let Some(target) = self.composer_target() else {
            return;
        };
        if !self.can_post(cx) {
            return;
        }
        self.file_error = None;
        let files = vec![LocalFile {
            source: FileSource::Bytes(FileBytes::new(png)),
            name: "Pasted image.png".to_owned(),
        }];
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::AttachFiles { target, files }, cx)
        });
        cx.notify();
    }

    /// Opens the system's file picker and attaches what is chosen.
    fn open_file_dialog(&mut self, cx: &mut Context<Self>) {
        if !self.can_post(cx) {
            return;
        }
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |composer, cx| match result {
                Ok(Ok(Some(paths))) => composer.attach_paths(paths, cx),
                Ok(Err(error)) => {
                    composer.file_error = Some(attachments::picker_failed_text(&error.to_string()));
                    cx.notify();
                }
                // Cancelled, or the picker went away.
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    /// Ctrl+V with files or a picture (and no text) on the clipboard.
    fn paste_attachments(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        match attachments::classify_clipboard(&item) {
            Some(Pasted::Files(paths)) => self.attach_paths(paths, cx),
            Some(Pasted::Image { bytes, format }) => {
                // Anything but a PNG is converted, which can take a moment: off the main thread.
                let converted =
                    cx.background_spawn(async move { attachments::to_png(bytes, format) });
                cx.spawn(async move |this, cx| {
                    let result = converted.await;
                    this.update(cx, |composer, cx| match result {
                        Ok(png) => composer.attach_pasted_png(png, cx),
                        Err(error) => {
                            composer.file_error =
                                Some(format!("The pasted picture couldn't be read: {error}"));
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            }
            None => {}
        }
    }

    fn remove_file(&mut self, attachment: lazy_domain::AttachmentId, cx: &mut Context<Self>) {
        let Some(target) = self.composer_target() else {
            return;
        };
        self.file_error = None;
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::RemoveAttachment { target, attachment }, cx)
        });
    }

    fn retry_file(&mut self, attachment: lazy_domain::AttachmentId, cx: &mut Context<Self>) {
        let Some(target) = self.composer_target() else {
            return;
        };
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::RetryAttachment { target, attachment }, cx)
        });
    }

    /// Keys on the tray: Left and Right between files, Delete or Backspace removes one, Enter
    /// tries a failed one again, Escape goes back to the box.
    fn tray_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tray_focus.is_focused(window) || event.keystroke.modifiers.modified() {
            return;
        }
        let chips = self.chips(cx);
        let shown = chips.len().min(attachments::MAX_CHIPS_SHOWN);
        if shown == 0 {
            return;
        }
        let at = self.tray_cursor.min(shown - 1);
        match event.keystroke.key.as_str() {
            "left" => self.tray_cursor = at.saturating_sub(1),
            "right" => self.tray_cursor = (at + 1).min(shown - 1),
            "home" => self.tray_cursor = 0,
            "end" => self.tray_cursor = shown - 1,
            "delete" | "backspace" => {
                let id = chips[at].id;
                self.remove_file(id, cx);
                if chips.len() == 1 {
                    self.focus_input(window, cx);
                }
            }
            "enter" if !event.is_held => {
                if let AttachmentState::Failed {
                    can_retry: true, ..
                } = chips[at].state
                {
                    self.retry_file(chips[at].id, cx);
                }
            }
            "escape" => self.focus_input(window, cx),
            _ => return,
        }
        self.tray_scroll
            .scroll_to_item(self.tray_cursor.min(shown - 1));
        cx.stop_propagation();
        cx.notify();
    }

    /// Asks for previews of the image files in the tray that have none yet. The decoding happens
    /// on a background thread.
    fn request_previews(&self, chips: &[ComposerAttachment], cx: &mut Context<Self>) {
        let store = ImageStore::global(cx);
        let wanted: Vec<(ImageKey, Source)> = chips
            .iter()
            .filter(|chip| lazy_domain::attachment::is_previewable(&chip.media_type_guess))
            .filter_map(|chip| {
                let source = match chip.source.as_ref()? {
                    FileSource::Path(path) => Source::File(path.clone()),
                    FileSource::Bytes(bytes) => Source::Pasted(bytes.clone()),
                };
                Some((ImageKey::Chip(chip.id.to_string()), source))
            })
            .filter(|(key, _)| !store.read(cx).has(key))
            .collect();
        if wanted.is_empty() {
            return;
        }
        cx.defer(move |cx| {
            store.update(cx, |store, cx| {
                for (key, source) in wanted {
                    store.request(key, source, CHIP_EDGE, cx);
                }
            });
        });
    }

    /// The tray: one chip per file, a note when there are more than it shows or the limit is
    /// reached, and the reason a file could not be added.
    fn tray(
        &mut self,
        chips: &[ComposerAttachment],
        names: &Names,
        window: &Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if chips.is_empty() && self.file_error.is_none() {
            return None;
        }
        self.request_previews(chips, cx);
        let focused = self.tray_focus.is_focused(window);
        let cursor = self.tray_cursor.min(chips.len().saturating_sub(1));
        let this = cx.entity().downgrade();
        let store = ImageStore::global(cx);
        // Two rows show; more scroll, and the keyboard cursor brings its chip into view.
        let mut row = div()
            .id(format!("{}-chips", names.tray))
            .flex()
            .flex_wrap()
            .items_start()
            .gap_2()
            .max_h(px(136.))
            .overflow_y_scroll()
            .track_scroll(&self.tray_scroll);
        for (index, chip) in chips.iter().take(attachments::MAX_CHIPS_SHOWN).enumerate() {
            let preview = match store.read(cx).get(&ImageKey::Chip(chip.id.to_string())) {
                Some(Slot::Ready(decoded)) => Some(decoded.image.clone()),
                _ => None,
            };
            let (remove_to, retry_to) = (this.clone(), this.clone());
            let id = chip.id;
            let (state, can_retry) = match &chip.state {
                AttachmentState::Reading => (TrayState::Reading, false),
                AttachmentState::Uploading { sent, total } => (
                    TrayState::Uploading {
                        fraction: attachments::fraction(*sent, *total),
                    },
                    false,
                ),
                AttachmentState::Uploaded => (TrayState::Uploaded, false),
                AttachmentState::Failed { reason, can_retry } => {
                    (TrayState::Failed(reason.clone()), *can_retry)
                }
            };
            row = row.child(attachment_view::tray_chip(
                TrayChip {
                    id: gpui::ElementId::Name(format!("{}-chip-{id}", names.tray).into()),
                    selector: format!("{}-chip-{index}", names.tray),
                    name: chip.name.clone(),
                    size_text: if chip.size == 0 {
                        String::new()
                    } else {
                        attachments::format_size(chip.size)
                    },
                    kind: attachments::file_kind(&chip.media_type_guess, &chip.name),
                    preview,
                    state,
                    can_retry,
                    focused: focused && index == cursor,
                    on_remove: Rc::new(move |_, cx| {
                        remove_to
                            .update(cx, |composer, cx| composer.remove_file(id, cx))
                            .ok();
                    }),
                    on_retry: Rc::new(move |_, cx| {
                        retry_to
                            .update(cx, |composer, cx| composer.retry_file(id, cx))
                            .ok();
                    }),
                },
                theme,
            ));
        }
        let note = |selector: String, text: String, color| {
            div()
                .debug_selector(move || selector)
                .text_size(theme.type_scale.small)
                .text_color(color)
                .child(text)
        };
        let hidden = chips.len().saturating_sub(attachments::MAX_CHIPS_SHOWN);
        let at_limit = chips.len() >= lazy_domain::attachment::MAX_ATTACHMENTS_PER_MESSAGE;
        let tray_id = names.tray;
        Some(
            div()
                .id(tray_id)
                .debug_selector(move || tray_id.into())
                .when(!chips.is_empty(), |tray| tray.track_focus(&self.tray_focus))
                .on_key_down(cx.listener(Self::tray_key))
                .flex()
                .flex_col()
                .gap_1()
                .child(row)
                .children((hidden > 0).then(|| {
                    note(
                        format!("{tray_id}-more"),
                        attachments::more_files_text(hidden),
                        theme.colors.text_muted,
                    )
                }))
                .children(at_limit.then(|| {
                    note(
                        format!("{tray_id}-limit"),
                        attachments::too_many_files_text(
                            lazy_domain::attachment::MAX_ATTACHMENTS_PER_MESSAGE,
                        ),
                        theme.colors.text_muted,
                    )
                }))
                .children(
                    self.file_error
                        .clone()
                        .map(|text| note(format!("{tray_id}-error"), text, theme.colors.danger)),
                )
                .into_any_element(),
        )
    }

    /// Asks the host to open the emoji picker above the emoji button.
    fn request_emoji_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_post(cx) {
            return;
        }
        let anchor = match self.emoji_bounds.get() {
            Some(bounds) => point(bounds.right(), bounds.top() - px(6.)),
            None => window.mouse_position(),
        };
        cx.emit(ComposerEvent::PickEmoji { anchor });
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(conversation) = self.conversation else {
            return;
        };
        let text = self.input.read(cx).text().to_owned();
        let input = match self.root {
            Some(root) => Input::SendReply {
                conversation,
                root,
                text,
                also_in_channel: self.also_in_channel,
            },
            None => Input::SendMessage { conversation, text },
        };
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    /// Chooses whether the reply is also sent to the channel.
    fn set_also_in_channel(&mut self, also: bool, cx: &mut Context<Self>) {
        self.also_in_channel = also;
        cx.notify();
    }

    /// Whether the reply is also going to the channel (thread mode), for tests.
    pub fn also_in_channel(&self) -> bool {
        self.also_in_channel
    }

    /// The focus handle of the Send button, for tab-order tests.
    pub fn send_focus(&self) -> FocusHandle {
        self.send_focus.clone()
    }

    /// The small line under the box: what Enter does, or why nothing can be posted here.
    ///
    /// `None` when no conversation is selected.
    pub fn hint(&self, cx: &gpui::App) -> Option<String> {
        let can_post = self.can_post(cx);
        if can_post && self.conversation.is_some() && self.has_failed_file(cx) {
            return Some(attachments::failed_file_blocks_send_text().to_owned());
        }
        self.conversation.map(|id| {
            let core = self.model.read(cx).core();
            let title = core.conversation_title(id);
            let is_channel = core
                .conversation(id)
                .is_some_and(|c| c.kind == lazy_domain::ConversationKind::Channel);
            // Typing an `@` in a channel whose people have not arrived: say why the list is short.
            let typing_a_mention = {
                let input = self.input.read(cx);
                suggest::mention_trigger(input.text(), input.cursor()).is_some()
            };
            if can_post && is_channel && typing_a_mention && core.conversation_members(id).is_none()
            {
                states::members_loading(&title)
            } else if self.mode == ComposerMode::Thread {
                if can_post {
                    "Reply · Enter to send, Shift+Enter for a new line".to_owned()
                } else {
                    "This thread can't be replied to.".to_owned()
                }
            } else if !can_post {
                format!(
                    "{} is archived. You can read it, but not post.",
                    if is_channel {
                        format!("#{title}")
                    } else {
                        title
                    }
                )
            } else if is_channel {
                format!("Message #{title} · Enter to send, Shift+Enter for a new line")
            } else {
                format!("Message {title} · Enter to send, Shift+Enter for a new line")
            }
        })
    }

    /// The focus handle of the emoji button, for tab-order tests.
    pub fn emoji_focus(&self) -> FocusHandle {
        self.emoji_focus.clone()
    }

    /// The focus handle of the "also send to channel" checkbox, for tab-order tests.
    pub fn also_focus(&self) -> FocusHandle {
        self.also_focus.clone()
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let focused = self.input.read(cx).is_focused(window) && self.can_post(cx);
        let has_text = !self.input.read(cx).text().trim().is_empty();
        let can_post = self.can_post(cx);
        let chips = self.chips(cx);
        let can_send = (has_text || !chips.is_empty()) && can_post;
        let names = self.mode.names();
        let tray = self.tray(&chips, &names, window, &theme, cx);
        let hint = self.hint(cx);

        let suggestions = self.suggestions.visible().then(|| {
            let on_pick: PickSuggestion = Rc::new(cx.processor(
                |this, index: usize, window: &mut Window, cx: &mut Context<Self>| {
                    this.accept_suggestion(index, window, cx)
                },
            ));
            // Pictures arrive after the list is first drawn: ask again for any that are still
            // missing (a no-op for the ones already requested).
            emoji_images::request_later(&self.model, self.suggestions.custom_ids(), cx);
            let pictures = emoji_images::pictures(
                ImageStore::global(cx).read(cx),
                &self.suggestions.custom_ids(),
            );
            suggest::render_list(
                &self.suggestions,
                &pictures,
                &theme,
                names.suggestions,
                on_pick,
            )
        });
        // The list sits at the caret. The input only knows where the caret is once it has painted
        // the new text, so the frame after an edit uses the last known place and asks for one more
        // frame to settle.
        let caret = self.suggestions.visible().then(|| {
            match self.input.read(cx).caret_bounds() {
                Some(bounds) => {
                    self.last_caret = Some(bounds.origin);
                    self.caret_repaint_queued = false;
                }
                None if !self.caret_repaint_queued => {
                    self.caret_repaint_queued = true;
                    let this = cx.entity();
                    window.on_next_frame(move |_, cx| this.update(cx, |_, cx| cx.notify()));
                }
                None => {}
            }
            self.last_caret
        });
        let caret = caret.flatten();
        let emoji_bounds = self.emoji_bounds.clone();

        div()
            .flex_none()
            .relative()
            .px_5()
            .pb_4()
            .pt_2()
            // Escape closes an open suggestion list, unless an input method is composing.
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape"
                    && !event.keystroke.modifiers.modified()
                    && this.suggestions.visible()
                    && !this.input.read(cx).is_composing()
                {
                    this.suggestions.dismiss();
                    this.input
                        .update(cx, |input, _| input.set_suggestions_open(false));
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .children(suggestions.map(|list| {
                match caret {
                    // Floats just above the line being typed, kept inside the window.
                    Some(at) => deferred(
                        anchored()
                            .position(point(at.x, at.y - px(4.)))
                            .anchor(Anchor::BottomLeft)
                            .snap_to_window_with_margin(px(8.))
                            .child(list),
                    )
                    .with_priority(1)
                    .into_any_element(),
                    // The caret has not been drawn yet: above the composer's left edge.
                    None => div()
                        .absolute()
                        .left(px(28.))
                        .bottom(relative(1.))
                        .child(list)
                        .into_any_element(),
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.medium)
                    .bg(if can_post {
                        theme.colors.surface_raised
                    } else {
                        theme.colors.surface_hover
                    })
                    .border_1()
                    .border_color(if focused {
                        theme.colors.focus_ring
                    } else {
                        theme.colors.border_strong
                    })
                    .children(tray)
                    .child(div().w_full().child(self.input.clone()))
                    .children((self.mode == ComposerMode::Thread).then(|| {
                        let core = self.model.read(cx).core();
                        let label = match self.conversation {
                            Some(id) => {
                                let title = core.conversation_title(id);
                                let is_channel = core.conversation(id).is_some_and(|c| {
                                    c.kind == lazy_domain::ConversationKind::Channel
                                });
                                if is_channel {
                                    format!("Also send to #{title}")
                                } else {
                                    format!("Also send to {title}")
                                }
                            }
                            None => "Also send to the conversation".to_owned(),
                        };
                        let this = cx.entity().downgrade();
                        form::checkbox(
                            &theme,
                            names.also,
                            label,
                            self.also_in_channel,
                            &self.also_focus,
                            move |also, _, cx| {
                                this.update(cx, |composer, cx| {
                                    composer.set_also_in_channel(also, cx)
                                })
                                .ok();
                            },
                        )
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(theme.type_scale.small)
                                    .text_color(theme.colors.text_faint)
                                    .child(match hint {
                                        Some(text) => text,
                                        None => "Select a conversation to write".to_owned(),
                                    }),
                            )
                            .child(
                                div()
                                    .id(names.attach)
                                    .debug_selector(move || names.attach.into())
                                    .role(Role::Button)
                                    .aria_label("Attach files")
                                    .track_focus(&self.attach_focus)
                                    .border_1()
                                    .border_color(gpui::transparent_black())
                                    .focus_visible(|style| {
                                        style.border_color(theme.colors.focus_ring)
                                    })
                                    .on_key_down(cx.listener(
                                        |this, event: &gpui::KeyDownEvent, _, cx| {
                                            if keyboard::is_activation(event) {
                                                this.open_file_dialog(cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .flex_none()
                                    .ml_3()
                                    .px_2()
                                    .py_1()
                                    .rounded(theme.radii.small)
                                    .text_size(px(16.))
                                    .when(can_post, |button| {
                                        button
                                            .cursor_pointer()
                                            .hover(|style| style.bg(theme.colors.surface_hover))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.open_file_dialog(cx)
                                            }))
                                    })
                                    .when(!can_post, |button| button.opacity(0.4))
                                    .child(emoji_font::glyphs("📎")),
                            )
                            .child(
                                div()
                                    .on_children_prepainted(move |bounds, _, _| {
                                        emoji_bounds.set(bounds.first().copied());
                                    })
                                    .child(
                                        div()
                                            .id(names.emoji)
                                            .debug_selector(move || names.emoji.into())
                                            .role(Role::Button)
                                            .aria_label("Add emoji")
                                            .track_focus(&self.emoji_focus)
                                            .border_1()
                                            .border_color(gpui::transparent_black())
                                            .focus_visible(|style| {
                                                style.border_color(theme.colors.focus_ring)
                                            })
                                            .on_key_down(cx.listener(
                                                |this, event: &gpui::KeyDownEvent, window, cx| {
                                                    if keyboard::is_activation(event) {
                                                        this.request_emoji_picker(window, cx);
                                                        cx.stop_propagation();
                                                    }
                                                },
                                            ))
                                            .flex_none()
                                            .ml_3()
                                            .px_2()
                                            .py_1()
                                            .rounded(theme.radii.small)
                                            .text_size(px(16.))
                                            .when(can_post, |button| {
                                                button
                                                    .cursor_pointer()
                                                    .hover(|style| {
                                                        style.bg(theme.colors.surface_hover)
                                                    })
                                                    .on_click(cx.listener(|this, _, window, cx| {
                                                        this.request_emoji_picker(window, cx)
                                                    }))
                                            })
                                            .when(!can_post, |button| button.opacity(0.4))
                                            .child(emoji_font::glyphs("🙂")),
                                    ),
                            )
                            .child(
                                div()
                                    .id(names.send)
                                    .debug_selector(move || names.send.into())
                                    .track_focus(&self.send_focus)
                                    .border_1()
                                    .border_color(gpui::transparent_black())
                                    // Filled with the accent when it can send, which is also
                                    // the ring's color: outline it in the text color then.
                                    .focus_visible(move |style| {
                                        style.border_color(if can_send {
                                            theme.colors.text
                                        } else {
                                            theme.colors.focus_ring
                                        })
                                    })
                                    .on_key_down(cx.listener(
                                        |this, event: &gpui::KeyDownEvent, _, cx| {
                                            if keyboard::is_activation(event) {
                                                this.submit(cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ))
                                    .flex_none()
                                    .ml_3()
                                    .px_3()
                                    .py_1()
                                    .rounded(theme.radii.small)
                                    .text_size(theme.type_scale.small)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .when(can_send, |button| {
                                        button
                                            .cursor_pointer()
                                            .bg(theme.colors.accent)
                                            .text_color(theme.colors.on_accent)
                                            .hover(|style| style.bg(theme.colors.accent_hover))
                                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                                    })
                                    .when(!can_send, |button| {
                                        button
                                            .bg(theme.colors.surface_hover)
                                            .text_color(theme.colors.text_faint)
                                    })
                                    .child("Send"),
                            ),
                    ),
            )
    }
}
