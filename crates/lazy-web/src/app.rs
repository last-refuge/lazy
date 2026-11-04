//! The page: holds the core, feeds it inputs, puts the views on the page, and turns what the
//! person does into inputs.
//!
//! # Inputs and drawing
//!
//! [`dispatch`] queues an input and, unless the queue is already being worked through, handles
//! every queued input in order: the core changes, its effects go to the [`Runtime`] (which may
//! queue more inputs, handled in the same pass), and a frame is requested. Drawing happens once per
//! animation frame however many inputs arrived, and never while the core is borrowed for an input.
//!
//! # What is never redrawn
//!
//! The text a person is typing lives in elements that are drawn once (the composers, the dialogs'
//! fields): drawing sets their value only when the core changed it (a sent message empties the
//! composer), never while they type. Lists are drawn by key ([`patch_rows`]): a row is replaced only
//! when its markup changed, so an image does not reload and a hovered row does not flicker when a
//! message arrives.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use lazy_client_core::{
    ClientCore, ComposerTarget, FileBytes, FileSource, HistoryStatus, Input, LocalFile,
    SessionPhase,
};
use lazy_domain::{
    AttachmentId, ConversationId, MessageId, OperationId, ReactionEmoji, UserId, WorkspaceId,
};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{
    Document, Element, Event, HtmlElement, HtmlInputElement, HtmlTextAreaElement, KeyboardEvent,
};

use crate::runtime::Runtime;
use crate::view::time::BrowserClock;
use crate::view::timeline::{Place, Row};
use crate::view::{self, Dialog, Menu, PickerFor, Suggesting, Ui};

/// How often the core is told the time when nothing else happens.
const CLOCK_EVERY_MS: i32 = 30_000;

/// How close to the edge of a list counts as being at it, in pixels.
const EDGE: i32 = 48;

/// What the page shows as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Screen {
    None,
    Splash(String),
    SignIn { registering: bool },
    Shell,
}

/// What is on the page now, so drawing changes only what differs.
struct Page {
    screen: Screen,
    regions: HashMap<&'static str, String>,
    rows: Vec<(String, String)>,
    thread_rows: Vec<(String, String)>,
    dialog: Option<&'static str>,
    picker: bool,
    shown_conversation: Option<ConversationId>,
    shown_thread: Option<MessageId>,
    /// The next drawing scrolls the conversation to its newest message.
    to_bottom: bool,
    thread_to_bottom: bool,
    /// What was last told to the core about the lists' bottoms.
    reported_bottom: Option<(ConversationId, bool)>,
    reported_thread_bottom: Option<(MessageId, bool)>,
    /// The editing state the composer shows.
    shown_editing: Option<(ConversationId, MessageId)>,
}

pub(crate) struct App {
    core: RefCell<ClientCore>,
    ui: RefCell<Ui>,
    runtime: RefCell<Option<Rc<Runtime>>>,
    queue: RefCell<VecDeque<Input>>,
    draining: Cell<bool>,
    frame_requested: Cell<bool>,
    clock: BrowserClock,
    page: RefCell<Page>,
    /// A channel joined from the directory, to open once the core has it.
    open_when_joined: Cell<Option<ConversationId>>,
    /// Which composer the file chooser is for.
    attach_target: Cell<Option<ComposerTarget>>,
    /// Where the emoji picker was opened, to place it.
    picker_anchor: RefCell<Option<Element>>,
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

fn app() -> Option<Rc<App>> {
    APP.with(|app| app.borrow().clone())
}

/// Queues an input for the core and handles the queue.
pub(crate) fn dispatch(input: Input) {
    if let Some(app) = app() {
        app.dispatch(input);
    }
}

fn document() -> Document {
    web_sys::window()
        .and_then(|window| window.document())
        .expect("a document")
}

fn by_id(id: &str) -> Option<Element> {
    document().get_element_by_id(id)
}

fn textarea(id: &str) -> Option<HtmlTextAreaElement> {
    by_id(id).and_then(|element| element.dyn_into().ok())
}

fn input(id: &str) -> Option<HtmlInputElement> {
    by_id(id).and_then(|element| element.dyn_into().ok())
}

/// Starts the web client on the page's `#app` element.
pub(crate) fn start() {
    let app = Rc::new(App {
        core: RefCell::new(ClientCore::new()),
        ui: RefCell::new(Ui::default()),
        runtime: RefCell::new(None),
        queue: RefCell::new(VecDeque::new()),
        draining: Cell::new(false),
        frame_requested: Cell::new(false),
        clock: BrowserClock::new(),
        page: RefCell::new(Page {
            screen: Screen::None,
            regions: HashMap::new(),
            rows: Vec::new(),
            thread_rows: Vec::new(),
            dialog: None,
            picker: false,
            shown_conversation: None,
            shown_thread: None,
            to_bottom: true,
            thread_to_bottom: true,
            reported_bottom: None,
            reported_thread_bottom: None,
            shown_editing: None,
        }),
        open_when_joined: Cell::new(None),
        attach_target: Cell::new(None),
        picker_anchor: RefCell::new(None),
    });
    let runtime = Runtime::new(Rc::new(dispatch));
    let startup = runtime.startup_state();
    *app.runtime.borrow_mut() = Some(runtime);
    APP.with(|slot| *slot.borrow_mut() = Some(app.clone()));
    listen();
    if let Some(window) = web_sys::window() {
        let shell = window
            .navigator()
            .user_agent()
            .is_ok_and(|agent| agent.contains("LazyShell/"));
        view::html::set_in_shell(shell);
        let back = Closure::<dyn Fn() -> bool>::new(go_back);
        let _ = js_sys::Reflect::set(&window, &JsValue::from_str("lazyBack"), back.as_ref());
        back.forget();
    }
    // The core has no clock: it is told the time now and every half minute, as the desktop
    // runtime does (and before each batch of realtime frames, `realtime.rs`).
    app.dispatch(Input::Clock(lazy_domain::Timestamp::now()));
    let tick = Closure::<dyn Fn()>::new(|| dispatch(Input::Clock(lazy_domain::Timestamp::now())));
    if let Some(window) = web_sys::window() {
        let _ = window.set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            CLOCK_EVERY_MS,
        );
    }
    tick.forget();
    app.dispatch(Input::Started(startup));
    app.dispatch(Input::NotificationPermissionChanged(
        crate::runtime::notification_permission(),
    ));
    report_window_active();
}

impl App {
    fn dispatch(self: &Rc<Self>, input: Input) {
        self.queue.borrow_mut().push_back(input);
        if self.draining.replace(true) {
            return;
        }
        loop {
            let Some(input) = self.queue.borrow_mut().pop_front() else {
                break;
            };
            let output = self.core.borrow_mut().handle(input);
            for change in &output.changes {
                self.note_change(change);
            }
            let runtime = self.runtime.borrow().clone();
            if let Some(runtime) = runtime {
                for effect in output.effects {
                    runtime.execute(effect);
                }
            }
        }
        self.draining.set(false);
        self.after_inputs();
        self.request_frame();
    }

    /// What a change means for the page beyond redrawing.
    fn note_change(&self, change: &lazy_client_core::Change) {
        use lazy_client_core::Change;
        match change {
            Change::TimelineReset { conversation } => {
                if Some(*conversation) == self.core.borrow().selected_conversation() {
                    self.page.borrow_mut().to_bottom = true;
                }
            }
            Change::ThreadReset { .. } => self.page.borrow_mut().thread_to_bottom = true,
            Change::ConversationOpened(_) => {
                let mut ui = self.ui.borrow_mut();
                if matches!(
                    ui.dialog,
                    Some(Dialog::CreateChannel | Dialog::NewMessage { .. } | Dialog::Browse)
                ) {
                    ui.dialog = None;
                }
            }
            Change::PhaseChanged => {
                let signed_out =
                    matches!(self.core.borrow().phase(), SessionPhase::SignedOut { .. });
                if signed_out {
                    let mut ui = self.ui.borrow_mut();
                    *ui = Ui {
                        registering: ui.registering,
                        ..Ui::default()
                    };
                }
            }
            _ => {}
        }
    }

    /// Follow-ups that need the inputs to have settled.
    fn after_inputs(self: &Rc<Self>) {
        // A password change that was answered: a refusal is shown by the action; one that went
        // through empties the fields and says so.
        if self.ui.borrow().password_sent {
            let state = self
                .core
                .borrow()
                .action(lazy_client_core::ActionKind::ChangePassword)
                .clone();
            match state {
                lazy_client_core::ActionState::InFlight => {}
                lazy_client_core::ActionState::Failed(_) => {
                    self.ui.borrow_mut().password_sent = false;
                }
                lazy_client_core::ActionState::Idle => {
                    {
                        let mut ui = self.ui.borrow_mut();
                        ui.password_sent = false;
                        ui.password_changed = true;
                    }
                    if let Ok(fields) =
                        document().query_selector_all("#password-form input[type=password]")
                    {
                        for index in 0..fields.length() {
                            if let Some(field) = fields
                                .item(index)
                                .and_then(|node| node.dyn_into::<HtmlInputElement>().ok())
                            {
                                field.set_value("");
                            }
                        }
                    }
                }
            }
        }
        if let Some(id) = self.open_when_joined.get() {
            let joined = self.core.borrow().conversation(id).is_some();
            if joined {
                self.open_when_joined.set(None);
                self.ui.borrow_mut().dialog = None;
                self.dispatch(Input::SelectConversation(id));
            }
        }
    }

    fn request_frame(self: &Rc<Self>) {
        if self.frame_requested.replace(true) {
            return;
        }
        let this = self.clone();
        let callback = Closure::once_into_js(move || {
            this.frame_requested.set(false);
            this.draw();
        });
        if let Some(window) = web_sys::window() {
            let _ = window.request_animation_frame(callback.unchecked_ref());
        }
    }

    // ----- Drawing -----

    fn draw(self: &Rc<Self>) {
        let phase = self.core.borrow().phase().clone();
        match phase {
            SessionPhase::Loading => self.show_screen(Screen::Splash("Loading…".to_owned())),
            SessionPhase::Unreachable { message } => {
                self.show_screen(Screen::Splash(message));
            }
            SessionPhase::SignedOut { .. } | SessionPhase::SigningIn => {
                let registering = self.ui.borrow().registering;
                self.show_screen(Screen::SignIn { registering });
                let (status, busy) = view::signin::status(&self.core.borrow());
                self.patch("signin-status", status);
                if let Some(button) = by_id("signin-submit") {
                    let _ = if busy {
                        button.set_attribute("disabled", "")
                    } else {
                        button.remove_attribute("disabled")
                    };
                }
            }
            SessionPhase::Ready => {
                if self.core.borrow().workspaces().is_empty() {
                    self.show_screen(Screen::Splash(
                        "You're not in a workspace on this server yet. Ask someone for an invitation, then sign in again."
                            .to_owned(),
                    ));
                    return;
                }
                self.show_screen(Screen::Shell);
                self.draw_shell();
            }
        }
    }

    fn show_screen(&self, screen: Screen) {
        let mut page = self.page.borrow_mut();
        if page.screen == screen {
            return;
        }
        let Some(root) = by_id("app") else { return };
        let html = match &screen {
            Screen::None => String::new(),
            Screen::Splash(text) => {
                let retry = matches!(self.core.borrow().phase(), SessionPhase::Unreachable { .. });
                view::splash(text, retry)
            }
            Screen::SignIn { registering } => {
                let core = self.core.borrow();
                view::signin::form(&core, *registering)
            }
            Screen::Shell => SHELL
                .replace("{attach}", view::icons::icon("attach"))
                .replace("{smile}", view::icons::icon("smile")),
        };
        root.set_inner_html(&html);
        *page = Page {
            screen: screen.clone(),
            regions: HashMap::new(),
            rows: Vec::new(),
            thread_rows: Vec::new(),
            dialog: None,
            picker: false,
            shown_conversation: None,
            shown_thread: None,
            to_bottom: true,
            thread_to_bottom: true,
            reported_bottom: None,
            reported_thread_bottom: None,
            shown_editing: None,
        };
        drop(page);
        if let Screen::SignIn { .. } = screen
            && let Some(field) = document()
                .query_selector(".signin input")
                .ok()
                .flatten()
                .and_then(|e| e.dyn_into::<HtmlElement>().ok())
        {
            let _ = field.focus();
        }
        if screen == Screen::Shell {
            listen_scrolling();
        }
    }

    /// Sets a region's markup if it changed.
    fn patch(&self, id: &'static str, html: String) {
        let mut page = self.page.borrow_mut();
        if page.regions.get(id) == Some(&html) {
            return;
        }
        if let Some(element) = by_id(id) {
            element.set_inner_html(&html);
            page.regions.insert(id, html);
        }
    }

    fn draw_shell(self: &Rc<Self>) {
        let core = self.core.borrow();
        let ui = self.ui.borrow();
        self.patch("sidebar", view::sidebar::sidebar(&core, &ui));
        set_class(&by_id("shell"), "sidebar-open", ui.sidebar_open);
        let selected = core.selected_conversation();

        // The composer follows the conversation.
        let changed = self.page.borrow().shown_conversation != selected;
        if changed {
            {
                let mut page = self.page.borrow_mut();
                page.shown_conversation = selected;
                page.rows.clear();
                page.to_bottom = true;
                page.reported_bottom = None;
                page.shown_editing = None;
            }
            if let Some(rows) = by_id("rows") {
                rows.set_inner_html("");
            }
            if let (Some(draft), Some(id)) = (textarea("draft"), selected) {
                draft.set_value(core.draft(id));
                draft.set_placeholder(&view::composer_placeholder(&core, id));
                autosize(&draft);
                if !is_narrow() {
                    let _ = draft.focus();
                }
            }
        }
        let Some(id) = selected else {
            self.patch("header", String::new());
            self.patch(
                "rows",
                "<div class=\"empty\">Pick a conversation on the left.</div>".to_owned(),
            );
            return;
        };
        self.patch("header", view::header(&core, id));
        self.patch("banner", view::banner(&core));

        // Composer: blocked, editing, or writing.
        let blocker = view::composer_blocker(&core, id);
        set_hidden(&by_id("composer"), blocker.is_some());
        self.patch("blocker", blocker.unwrap_or_default());
        self.patch(
            "chips",
            view::chips(&core, ComposerTarget::Conversation(id)),
        );
        self.patch("suggest", suggestions(&core, &ui, "draft", id));
        let editing = ui.editing.filter(|(conversation, _)| *conversation == id);
        if self.page.borrow().shown_editing != editing {
            self.page.borrow_mut().shown_editing = editing;
            if let Some(draft) = textarea("draft") {
                match editing {
                    Some((conversation, message)) => {
                        let text = core.edit_text(conversation, message).unwrap_or_default();
                        draft.set_value(&text);
                        let _ = draft.focus();
                    }
                    None => draft.set_value(core.draft(id)),
                }
                autosize(&draft);
            }
            self.patch(
                "editing-bar",
                if editing.is_some() {
                    "<span>Editing your message</span><button type=\"button\" class=\"link\" data-action=\"stop-editing\">Cancel</button><span class=\"muted\">Enter to save · Esc to cancel</span>".to_owned()
                } else {
                    String::new()
                },
            );
            set_class(&by_id("composer"), "is-editing", editing.is_some());
        } else if editing.is_none()
            && let Some(draft) = textarea("draft")
            && draft.value() != core.draft(id)
        {
            // The core changed the draft (a message was sent).
            draft.set_value(core.draft(id));
            autosize(&draft);
        }

        // The messages.
        let rows = view::timeline::conversation_rows(&core, &ui, &self.clock, id);
        if let Some(scroller) = by_id("timeline") {
            let to_bottom = self.page.borrow().to_bottom;
            let was_at_bottom = at_bottom(&scroller);
            let (height, top) = (scroller.scroll_height(), scroller.scroll_top());
            let first_before = first_message_key(&self.page.borrow().rows);
            {
                let mut page = self.page.borrow_mut();
                if let Some(container) = by_id("rows") {
                    patch_rows(&container, &mut page.rows, rows);
                }
            }
            let first_after = first_message_key(&self.page.borrow().rows);
            if to_bottom || was_at_bottom {
                scroller.set_scroll_top(scroller.scroll_height());
                self.page.borrow_mut().to_bottom = false;
            } else if first_before != first_after {
                // Older messages arrived above: keep what was on screen where it was.
                scroller.set_scroll_top(scroller.scroll_height() - (height - top));
            }
        }

        // A message to show: a permalink or a search result.
        if let Some(target) = core.jump_target()
            && target.thread.is_none()
            && target.conversation == id
        {
            let key = view::key_text(target.key);
            if let Some(row) = document()
                .query_selector(&format!("#rows [data-key=\"{key}\"]"))
                .ok()
                .flatten()
            {
                row.scroll_into_view_with_bool(false);
                flash(&row);
                self.page.borrow_mut().to_bottom = false;
                let this = self.clone();
                defer(move || this.dispatch(Input::ClearJumpTarget));
            }
        }

        drop(ui);
        drop(core);
        self.draw_thread();
        self.draw_overlays();
        self.report_bottoms();
    }

    fn draw_thread(self: &Rc<Self>) {
        let core = self.core.borrow();
        let ui = self.ui.borrow();
        let open = core.open_thread();
        set_hidden(&by_id("thread"), open.is_none());
        set_class(&by_id("shell"), "thread-open", open.is_some());
        let Some(open) = open else {
            self.page.borrow_mut().shown_thread = None;
            return;
        };
        let root = open.root;
        if self.page.borrow().shown_thread != Some(root) {
            {
                let mut page = self.page.borrow_mut();
                page.shown_thread = Some(root);
                page.thread_rows.clear();
                page.thread_to_bottom = true;
                page.reported_thread_bottom = None;
            }
            if let Some(rows) = by_id("thread-rows") {
                rows.set_inner_html("");
            }
            if let Some(draft) = textarea("thread-draft") {
                draft.set_value(core.thread_draft(root));
                autosize(&draft);
                let _ = draft.focus();
            }
        } else if let Some(draft) = textarea("thread-draft")
            && draft.value() != core.thread_draft(root)
        {
            draft.set_value(core.thread_draft(root));
            autosize(&draft);
        }
        let following = core.thread_state(root).is_some_and(|state| state.following);
        self.patch(
            "thread-head",
            format!(
                "<div><h2>Thread</h2><span class=\"muted\">{}</span></div><div class=\"hd-side\"><button type=\"button\" class=\"small\" data-action=\"follow-thread\" data-msg=\"{root}\" aria-pressed=\"{following}\">{}</button><button type=\"button\" class=\"icon\" data-action=\"close-thread\" aria-label=\"Close the thread\">{}</button></div>",
                view::html::esc(&view::conversation_name(&core, open.conversation)),
                if following { "Following" } else { "Follow" },
                view::icons::icon("close")
            ),
        );
        self.patch(
            "thread-chips",
            view::chips(&core, ComposerTarget::Thread(root)),
        );
        self.patch(
            "thread-suggest",
            suggestions(&core, &ui, "thread-draft", open.conversation),
        );
        let Some(thread) = core.thread(root) else {
            return;
        };
        // The root and the replies each have their own day dividers: their keys are kept apart.
        let section = |prefix: &str, rows: Vec<Row>| -> Vec<Row> {
            rows.into_iter()
                .map(|(key, html)| (format!("{prefix}{key}"), html))
                .collect()
        };
        let mut rows: Vec<Row> = Vec::new();
        if let Some(root_item) = thread.root {
            rows.extend(section(
                "t",
                view::timeline::item_rows(
                    &core,
                    &ui,
                    &self.clock,
                    open.conversation,
                    std::slice::from_ref(root_item),
                    None,
                    Place::Thread,
                ),
            ));
            let count = thread.replies.len();
            rows.push((
                "replies".to_owned(),
                format!(
                    "<div class=\"replies-line\"><span>{}</span></div>",
                    match count {
                        0 => "No replies yet".to_owned(),
                        1 => "1 reply".to_owned(),
                        n => format!("{n} replies"),
                    }
                ),
            ));
        }
        match thread.history {
            HistoryStatus::Ready { has_more: true } => rows.push((
                "older".to_owned(),
                format!("<div class=\"history\"><button type=\"button\" class=\"link\" data-action=\"older-replies\" data-msg=\"{root}\">Show older replies</button></div>"),
            )),
            HistoryStatus::Loading | HistoryStatus::NotLoaded => rows.push((
                "older".to_owned(),
                "<div class=\"history\">Loading replies…</div>".to_owned(),
            )),
            HistoryStatus::Failed { message } => rows.push((
                "older".to_owned(),
                format!(
                    "<div class=\"history failed\">{} <button type=\"button\" class=\"link\" data-action=\"older-replies\" data-msg=\"{root}\">Try again</button></div>",
                    view::html::esc(message)
                ),
            )),
            HistoryStatus::Ready { has_more: false } => {}
        }
        rows.extend(section(
            "r",
            view::timeline::item_rows(
                &core,
                &ui,
                &self.clock,
                open.conversation,
                thread.replies,
                None,
                Place::Thread,
            ),
        ));
        if let Some(scroller) = by_id("thread-scroll") {
            let to_bottom = self.page.borrow().thread_to_bottom;
            let was_at_bottom = at_bottom(&scroller);
            {
                let mut page = self.page.borrow_mut();
                if let Some(container) = by_id("thread-rows") {
                    patch_rows(&container, &mut page.thread_rows, rows);
                }
            }
            if to_bottom || was_at_bottom {
                scroller.set_scroll_top(scroller.scroll_height());
                self.page.borrow_mut().thread_to_bottom = false;
            }
        }
    }

    fn draw_overlays(self: &Rc<Self>) {
        let core = self.core.borrow();
        let ui = self.ui.borrow();
        // A dialog.
        let kind = ui.dialog.as_ref().map(Dialog::kind);
        if self.page.borrow().dialog != kind {
            self.page.borrow_mut().dialog = kind;
            self.page.borrow_mut().regions.remove("dlg-body");
            self.page.borrow_mut().regions.remove("dlg-status");
            if let Some(overlay) = by_id("overlay") {
                match &ui.dialog {
                    Some(dialog) => {
                        overlay.set_inner_html(&view::dialogs::shell(&core, dialog));
                        if let Some(field) = overlay
                            .query_selector(".dialog input")
                            .ok()
                            .flatten()
                            .and_then(|e| e.dyn_into::<HtmlElement>().ok())
                        {
                            let _ = field.focus();
                        }
                    }
                    None => overlay.set_inner_html(""),
                }
            }
        }
        if let Some(dialog) = &ui.dialog {
            self.patch("dlg-body", view::dialogs::body(&core, &self.clock, dialog));
            if let Dialog::Account(page) = dialog {
                self.patch(
                    "dlg-status",
                    view::dialogs::account_status(&core, *page, ui.password_changed),
                );
            }
            if let Dialog::CreateChannel = dialog {
                self.patch(
                    "dlg-status",
                    view::dialogs::action_status(
                        &core,
                        lazy_client_core::ActionKind::CreateChannel,
                    ),
                );
            }
        }
        // The emoji picker or a menu, in the popover.
        let popover = by_id("popover");
        if let Some(_target) = ui.picker {
            if !self.page.borrow().picker {
                self.page.borrow_mut().picker = true;
                self.page.borrow_mut().regions.remove("picker-body");
                self.page.borrow_mut().regions.remove("popover-menu");
                if let Some(popover) = &popover {
                    popover.set_inner_html(&view::dialogs::picker_shell());
                    place_popover(popover, self.picker_anchor.borrow().as_ref(), 340, 380);
                }
                if let Some(field) = input("picker-q") {
                    field.set_value(&ui.picker_query);
                    let _ = field.focus();
                }
            }
            self.patch(
                "picker-body",
                view::dialogs::picker_body(&core, &ui.picker_query),
            );
        } else {
            let menu_html = match ui.menu {
                Some(Menu::Workspace) => Some(view::sidebar::workspace_menu(&core)),
                Some(Menu::Account) => Some(view::sidebar::account_menu(&core)),
                Some(Menu::Conversation(id)) => Some(view::sidebar::conversation_menu(&core, id)),
                None => None,
            };
            let was_picker = self.page.borrow_mut().picker;
            self.page.borrow_mut().picker = false;
            if let Some(popover) = &popover {
                match menu_html {
                    Some(html) => {
                        let key = format!("{:?}", ui.menu);
                        let shown = self.page.borrow().regions.get("popover-menu").cloned();
                        if shown.as_deref() != Some(&format!("{key}{html}")) || was_picker {
                            popover.set_inner_html(&html);
                            let anchor = match ui.menu {
                                Some(Menu::Workspace) => {
                                    document().query_selector(".sb-head .ws").ok().flatten()
                                }
                                Some(Menu::Account) => {
                                    document().query_selector(".sb-me .me").ok().flatten()
                                }
                                _ => document()
                                    .query_selector("[data-action=\"conversation-menu\"]")
                                    .ok()
                                    .flatten(),
                            };
                            let height = if ui.menu == Some(Menu::Account) {
                                270
                            } else {
                                200
                            };
                            place_popover(popover, anchor.as_ref(), 260, height);
                            self.page
                                .borrow_mut()
                                .regions
                                .insert("popover-menu", format!("{key}{html}"));
                        }
                    }
                    None => {
                        if self.page.borrow().regions.contains_key("popover-menu") || was_picker {
                            popover.set_inner_html("");
                            self.page.borrow_mut().regions.remove("popover-menu");
                        }
                    }
                }
            }
        }
    }

    /// Tells the core whether the newest message of each list is on screen (it marks them read
    /// only then).
    fn report_bottoms(self: &Rc<Self>) {
        let selected = self.core.borrow().selected_conversation();
        if let (Some(id), Some(scroller)) = (selected, by_id("timeline")) {
            let bottom = at_bottom(&scroller) && page_visible();
            if self.page.borrow().reported_bottom != Some((id, bottom)) {
                self.page.borrow_mut().reported_bottom = Some((id, bottom));
                let this = self.clone();
                defer(move || {
                    this.dispatch(Input::TimelineAtBottom {
                        conversation: id,
                        at_bottom: bottom,
                    })
                });
            }
        }
        let thread = self.core.borrow().open_thread();
        if let (Some(open), Some(scroller)) = (thread, by_id("thread-scroll")) {
            let bottom = at_bottom(&scroller) && page_visible();
            if self.page.borrow().reported_thread_bottom != Some((open.root, bottom)) {
                self.page.borrow_mut().reported_thread_bottom = Some((open.root, bottom));
                let this = self.clone();
                defer(move || {
                    this.dispatch(Input::ThreadAtBottom {
                        root: open.root,
                        at_bottom: bottom,
                    })
                });
            }
        }
    }

    /// Asks for older messages when the list is scrolled near its top.
    fn scrolled(self: &Rc<Self>) {
        let selected = self.core.borrow().selected_conversation();
        if let (Some(id), Some(scroller)) = (selected, by_id("timeline"))
            && scroller.scroll_top() < EDGE * 4
        {
            let more = matches!(
                self.core.borrow().history_status(id),
                HistoryStatus::Ready { has_more: true }
            );
            if more {
                self.dispatch(Input::LoadOlder { conversation: id });
            }
        }
        let thread = self.core.borrow().open_thread();
        if let Some(open) = thread
            && let Some(scroller) = by_id("thread-scroll")
            && scroller.scroll_top() < EDGE
        {
            let more = self.core.borrow().thread(open.root).is_some_and(|thread| {
                matches!(thread.history, HistoryStatus::Ready { has_more: true })
            });
            if more {
                let this = self.clone();
                defer(move || this.dispatch(Input::LoadOlderReplies { root: open.root }));
            }
        }
        self.report_bottoms();
    }

    // ----- What the person does -----

    fn send_composer(self: &Rc<Self>) {
        let Some(draft) = textarea("draft") else {
            return;
        };
        let Some(id) = self.core.borrow().selected_conversation() else {
            return;
        };
        let text = draft.value();
        self.ui.borrow_mut().suggest = None;
        let editing = self.ui.borrow().editing;
        if let Some((conversation, message)) = editing {
            self.ui.borrow_mut().editing = None;
            self.dispatch(Input::EditMessage {
                conversation,
                message,
                text,
            });
            return;
        }
        self.dispatch(Input::SendMessage {
            conversation: id,
            text,
        });
        self.page.borrow_mut().to_bottom = true;
    }

    fn send_reply(self: &Rc<Self>) {
        let Some(open) = self.core.borrow().open_thread() else {
            return;
        };
        let Some(draft) = textarea("thread-draft") else {
            return;
        };
        let also = input("also-channel").is_some_and(|check| check.checked());
        self.ui.borrow_mut().suggest = None;
        self.dispatch(Input::SendReply {
            conversation: open.conversation,
            root: open.root,
            text: draft.value(),
            also_in_channel: also,
        });
        if let Some(check) = input("also-channel") {
            check.set_checked(false);
        }
        self.page.borrow_mut().thread_to_bottom = true;
    }

    /// Edits the person's newest message (Up in an empty composer).
    fn edit_last(self: &Rc<Self>) -> bool {
        let core = self.core.borrow();
        let Some(id) = core.selected_conversation() else {
            return false;
        };
        let last = core
            .timeline(id)
            .iter()
            .rev()
            .filter_map(|item| item.message_id())
            .find(|message| core.can_edit_message(id, *message));
        drop(core);
        match last {
            Some(message) => {
                self.ui.borrow_mut().editing = Some((id, message));
                self.request_frame();
                true
            }
            None => false,
        }
    }

    fn stop_editing(self: &Rc<Self>) {
        self.ui.borrow_mut().editing = None;
        self.request_frame();
    }

    fn close_dialog(self: &Rc<Self>) {
        let dialog = self.ui.borrow_mut().dialog.take();
        match dialog {
            Some(Dialog::Browse) => self.dispatch(Input::CloseBrowse),
            Some(Dialog::NewMessage { .. }) => self.dispatch(Input::ClosePeople),
            Some(Dialog::Search) => self.dispatch(Input::CloseSearch),
            Some(Dialog::CreateChannel) => self.dispatch(Input::DismissAction(
                lazy_client_core::ActionKind::CreateChannel,
            )),
            Some(Dialog::Account(_)) => {
                {
                    let mut ui = self.ui.borrow_mut();
                    ui.password_sent = false;
                    ui.password_changed = false;
                }
                self.dispatch(Input::DismissAction(
                    lazy_client_core::ActionKind::ChangePassword,
                ));
                self.dispatch(Input::DismissAction(
                    lazy_client_core::ActionKind::UpdateProfile,
                ));
            }
            _ => self.request_frame(),
        }
    }

    fn open_dialog(self: &Rc<Self>, dialog: Dialog) {
        self.close_popover();
        if self.ui.borrow().dialog.is_some() {
            self.close_dialog();
        }
        let input = match &dialog {
            Dialog::Browse => Some(Input::BrowseChannels),
            Dialog::NewMessage { .. } => Some(Input::OpenPeople),
            Dialog::Search => Some(Input::OpenSearch),
            _ => None,
        };
        self.ui.borrow_mut().dialog = Some(dialog);
        match input {
            Some(input) => self.dispatch(input),
            None => self.request_frame(),
        }
    }

    fn close_popover(self: &Rc<Self>) {
        let mut ui = self.ui.borrow_mut();
        let open = ui.picker.is_some() || ui.menu.is_some();
        ui.picker = None;
        ui.picker_query.clear();
        ui.menu = None;
        drop(ui);
        if open {
            self.request_frame();
        }
    }

    fn open_picker(self: &Rc<Self>, target: PickerFor, anchor: Element) {
        *self.picker_anchor.borrow_mut() = Some(anchor);
        let mut ui = self.ui.borrow_mut();
        ui.menu = None;
        ui.picker = Some(target);
        ui.picker_query.clear();
        drop(ui);
        self.page.borrow_mut().picker = false;
        self.request_frame();
    }

    fn pick_emoji(self: &Rc<Self>, emoji: ReactionEmoji) {
        let target = self.ui.borrow().picker;
        self.close_popover();
        match target {
            Some(PickerFor::Reaction {
                conversation,
                message,
            }) => {
                self.dispatch(Input::RecordEmojiUse(emoji));
                if !self
                    .core
                    .borrow()
                    .reacted_by_me(conversation, message, emoji)
                {
                    self.dispatch(Input::ToggleReaction {
                        conversation,
                        message,
                        emoji,
                    });
                }
            }
            Some(PickerFor::Composer) => {
                self.dispatch(Input::RecordEmojiUse(emoji));
                let text = emoji_text(&self.core.borrow(), emoji);
                self.insert_text("draft", &text);
            }
            Some(PickerFor::Thread) => {
                self.dispatch(Input::RecordEmojiUse(emoji));
                let text = emoji_text(&self.core.borrow(), emoji);
                self.insert_text("thread-draft", &text);
            }
            None => {}
        }
    }

    /// Puts text at the caret of a composer, as if typed.
    fn insert_text(self: &Rc<Self>, id: &str, text: &str) {
        let Some(area) = textarea(id) else { return };
        let value = area.value();
        let units: Vec<u16> = value.encode_utf16().collect();
        let start = area
            .selection_start()
            .ok()
            .flatten()
            .unwrap_or(units.len() as u32) as usize;
        let end = area.selection_end().ok().flatten().unwrap_or(start as u32) as usize;
        let (start, end) = (
            start.min(units.len()),
            end.min(units.len()).max(start.min(units.len())),
        );
        let before = String::from_utf16_lossy(&units[..start]);
        let after = String::from_utf16_lossy(&units[end..]);
        let new = format!("{before}{text}{after}");
        area.set_value(&new);
        let caret = (start + text.encode_utf16().count()) as u32;
        let _ = area.set_selection_range(caret, caret);
        let _ = area.focus();
        self.typed(id);
    }

    /// A composer's text changed.
    fn typed(self: &Rc<Self>, id: &str) {
        let Some(area) = textarea(id) else { return };
        autosize(&area);
        self.update_suggest(id, &area);
        match id {
            "draft" => {
                if self.ui.borrow().editing.is_some() {
                    return;
                }
                let selected = self.core.borrow().selected_conversation();
                if let Some(conversation) = selected {
                    self.dispatch(Input::SetDraft {
                        conversation,
                        text: area.value(),
                    });
                }
            }
            "thread-draft" => {
                let open = self.core.borrow().open_thread();
                if let Some(open) = open {
                    self.dispatch(Input::SetThreadDraft {
                        root: open.root,
                        text: area.value(),
                    });
                }
            }
            _ => {}
        }
    }

    /// Opens, moves, or closes the suggestions for the word at the caret of a composer.
    fn update_suggest(self: &Rc<Self>, id: &str, area: &HtmlTextAreaElement) {
        let field: &'static str = match id {
            "draft" => "draft",
            "thread-draft" => "thread-draft",
            _ => return,
        };
        let caret = area.selection_start().ok().flatten().unwrap_or(0);
        let token = view::suggest::token_at(&area.value(), caret);
        let mut ui = self.ui.borrow_mut();
        let next = token.map(|token| {
            let highlighted = match &ui.suggest {
                Some(open) if open.field == field && open.token.query == token.query => {
                    open.highlighted
                }
                _ => 0,
            };
            Suggesting {
                field,
                token,
                highlighted,
            }
        });
        if ui.suggest != next {
            ui.suggest = next;
            drop(ui);
            self.request_frame();
        }
    }

    /// The suggestions now on offer, for the open list.
    fn suggestion_items(&self) -> Vec<view::suggest::Item> {
        let ui = self.ui.borrow();
        let Some(open) = &ui.suggest else {
            return Vec::new();
        };
        let core = self.core.borrow();
        let conversation = match open.field {
            "thread-draft" => core.open_thread().map(|t| t.conversation),
            _ => core.selected_conversation(),
        };
        conversation
            .map(|id| view::suggest::items(&core, id, &open.token))
            .unwrap_or_default()
    }

    /// Keys while suggestions are open. `true` when the key was used.
    fn suggest_key(self: &Rc<Self>, key: &str) -> bool {
        let count = self.suggestion_items().len();
        if count == 0 {
            return false;
        }
        match key {
            "ArrowDown" | "ArrowUp" => {
                if let Some(open) = &mut self.ui.borrow_mut().suggest {
                    let step: isize = if key == "ArrowDown" { 1 } else { -1 };
                    open.highlighted =
                        (open.highlighted as isize + step).rem_euclid(count as isize) as usize;
                }
                self.request_frame();
                true
            }
            "Enter" | "Tab" => {
                let index = self
                    .ui
                    .borrow()
                    .suggest
                    .as_ref()
                    .map_or(0, |s| s.highlighted);
                self.pick_suggestion(index);
                true
            }
            "Escape" => {
                self.ui.borrow_mut().suggest = None;
                self.request_frame();
                true
            }
            _ => false,
        }
    }

    /// Replaces the word being typed with suggestion `index`.
    fn pick_suggestion(self: &Rc<Self>, index: usize) {
        let items = self.suggestion_items();
        let Some(open) = self.ui.borrow_mut().suggest.take() else {
            return;
        };
        let Some(item) = items.get(index.min(items.len().saturating_sub(1))) else {
            return;
        };
        let Some(area) = textarea(open.field) else {
            return;
        };
        let units: Vec<u16> = area.value().encode_utf16().collect();
        let start = (open.token.start as usize).min(units.len());
        let end = (open.token.end as usize).clamp(start, units.len());
        let new = format!(
            "{}{}{}",
            String::from_utf16_lossy(&units[..start]),
            item.insert,
            String::from_utf16_lossy(&units[end..])
        );
        area.set_value(&new);
        let caret = (start + item.insert.encode_utf16().count()) as u32;
        let _ = area.set_selection_range(caret, caret);
        let _ = area.focus();
        self.typed(open.field);
        self.request_frame();
    }

    fn attach(self: &Rc<Self>, target: ComposerTarget, files: web_sys::FileList) {
        let files: Vec<web_sys::File> = (0..files.length()).filter_map(|i| files.get(i)).collect();
        if files.is_empty() {
            return;
        }
        let this = self.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let mut chosen = Vec::new();
            for file in files {
                let Ok(buffer) = wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await
                else {
                    continue;
                };
                let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                chosen.push(LocalFile {
                    source: FileSource::Bytes(FileBytes::new(bytes)),
                    name: file.name(),
                });
            }
            if !chosen.is_empty() {
                this.dispatch(Input::AttachFiles {
                    target,
                    files: chosen,
                });
            }
        });
    }

    fn on_action(self: &Rc<Self>, action: &str, element: &Element, event: &Event) {
        let attr = |name: &str| {
            element
                .get_attribute(&format!("data-{name}"))
                .unwrap_or_default()
        };
        let conv = || attr("conv").parse::<ConversationId>().ok();
        let msg = || attr("msg").parse::<MessageId>().ok();
        match action {
            "select" => {
                if let Some(id) = conv() {
                    let mut ui = self.ui.borrow_mut();
                    ui.sidebar_open = false;
                    ui.editing = None;
                    ui.suggest = None;
                    drop(ui);
                    self.dispatch(Input::SelectConversation(id));
                }
            }
            "toggle-sidebar" => {
                let open = !self.ui.borrow().sidebar_open;
                self.ui.borrow_mut().sidebar_open = open;
                self.request_frame();
            }
            "load-older" => {
                if let Some(id) = conv() {
                    self.dispatch(Input::LoadOlder { conversation: id });
                }
            }
            "open-thread" => {
                if let (Some(conversation), Some(root)) = (conv(), msg()) {
                    self.dispatch(Input::OpenThread { conversation, root });
                }
            }
            "close-thread" => self.dispatch(Input::CloseThread),
            "older-replies" => {
                if let Some(root) = msg() {
                    self.dispatch(Input::LoadOlderReplies { root });
                }
            }
            "follow-thread" => {
                if let Some(root) = msg() {
                    let following = self
                        .core
                        .borrow()
                        .thread_state(root)
                        .is_some_and(|state| state.following);
                    self.dispatch(Input::SetThreadFollow {
                        root,
                        following: !following,
                    });
                }
            }
            "edit" => {
                if let (Some(conversation), Some(message)) = (conv(), msg()) {
                    self.ui.borrow_mut().editing = Some((conversation, message));
                    self.request_frame();
                }
            }
            "stop-editing" => self.stop_editing(),
            "delete" => {
                if let (Some(conversation), Some(message)) = (conv(), msg())
                    && confirm("Delete this message? This can't be undone.")
                {
                    self.dispatch(Input::DeleteMessage {
                        conversation,
                        message,
                    });
                }
            }
            "react" => {
                if let (Some(conversation), Some(message)) = (conv(), msg()) {
                    self.open_picker(
                        PickerFor::Reaction {
                            conversation,
                            message,
                        },
                        element.clone(),
                    );
                }
            }
            "toggle-reaction" => {
                if let (Some(conversation), Some(message), Ok(emoji)) =
                    (conv(), msg(), ReactionEmoji::parse(&attr("emoji")))
                {
                    self.dispatch(Input::ToggleReaction {
                        conversation,
                        message,
                        emoji,
                    });
                }
            }
            "pick-emoji" => {
                if let Ok(emoji) = ReactionEmoji::parse(&attr("emoji")) {
                    self.pick_emoji(emoji);
                }
            }
            "composer-emoji" => self.open_picker(PickerFor::Composer, element.clone()),
            "thread-emoji" => self.open_picker(PickerFor::Thread, element.clone()),
            "attach" => {
                let target = if attr("target") == "thread" {
                    self.core
                        .borrow()
                        .open_thread()
                        .map(|open| ComposerTarget::Thread(open.root))
                } else {
                    self.core
                        .borrow()
                        .selected_conversation()
                        .map(ComposerTarget::Conversation)
                };
                if let (Some(target), Some(chooser)) = (target, input("file-input")) {
                    self.attach_target.set(Some(target));
                    chooser.set_value("");
                    chooser.click();
                }
            }
            "remove-file" | "retry-file" => {
                let target = if attr("target") == "thread" {
                    self.core
                        .borrow()
                        .open_thread()
                        .map(|open| ComposerTarget::Thread(open.root))
                } else {
                    self.core
                        .borrow()
                        .selected_conversation()
                        .map(ComposerTarget::Conversation)
                };
                if let (Some(target), Ok(attachment)) = (target, attr("id").parse::<AttachmentId>())
                {
                    self.dispatch(if action == "remove-file" {
                        Input::RemoveAttachment { target, attachment }
                    } else {
                        Input::RetryAttachment { target, attachment }
                    });
                }
            }
            "send" => self.send_composer(),
            "send-reply" => self.send_reply(),
            "retry-send" | "discard-send" => {
                if let (Some(conversation), Ok(operation)) =
                    (conv(), attr("op").parse::<OperationId>())
                {
                    self.dispatch(if action == "retry-send" {
                        Input::RetryOperation {
                            conversation,
                            operation,
                        }
                    } else {
                        Input::DiscardOperation {
                            conversation,
                            operation,
                        }
                    });
                }
            }
            "retry-edit" | "cancel-edit" => {
                if let (Some(conversation), Some(message)) = (conv(), msg()) {
                    self.dispatch(if action == "retry-edit" {
                        Input::RetryEdit {
                            conversation,
                            message,
                        }
                    } else {
                        Input::CancelEdit {
                            conversation,
                            message,
                        }
                    });
                }
            }
            "favorite" => {
                if let Some(id) = conv() {
                    let favorite = self
                        .core
                        .borrow()
                        .user_state(id)
                        .is_some_and(|s| s.favorite);
                    self.ui.borrow_mut().menu = None;
                    self.dispatch(Input::SetFavorite {
                        id,
                        favorite: !favorite,
                    });
                }
            }
            "conversation-menu" => {
                if let Some(id) = conv() {
                    let open = self.ui.borrow().menu == Some(Menu::Conversation(id));
                    self.ui.borrow_mut().picker = None;
                    self.ui.borrow_mut().menu = (!open).then_some(Menu::Conversation(id));
                    self.request_frame();
                }
            }
            "account-menu" => {
                let open = self.ui.borrow().menu == Some(Menu::Account);
                self.ui.borrow_mut().picker = None;
                self.ui.borrow_mut().menu = (!open).then_some(Menu::Account);
                self.request_frame();
            }
            "account" => {
                if let Some(page) = view::AccountPage::parse(&attr("page")) {
                    self.ui.borrow_mut().menu = None;
                    self.open_dialog(Dialog::Account(page));
                }
            }
            "workspace-menu" => {
                let open = self.ui.borrow().menu == Some(Menu::Workspace);
                self.ui.borrow_mut().picker = None;
                self.ui.borrow_mut().menu = (!open).then_some(Menu::Workspace);
                self.request_frame();
            }
            "leave" => {
                if let Some(id) = conv() {
                    self.ui.borrow_mut().menu = None;
                    let name = view::conversation_name(&self.core.borrow(), id);
                    if confirm(&format!("Leave {name}?")) {
                        self.dispatch(Input::LeaveChannel(id));
                    } else {
                        self.request_frame();
                    }
                }
            }
            "join" => {
                if let Some(id) = conv() {
                    self.open_when_joined.set(Some(id));
                    self.dispatch(Input::JoinChannel(id));
                }
            }
            "open-channel" => {
                if let Some(id) = conv() {
                    if self.core.borrow().conversation(id).is_some() {
                        self.ui.borrow_mut().dialog = None;
                        self.dispatch(Input::CloseBrowse);
                        self.dispatch(Input::SelectConversation(id));
                    } else {
                        self.open_when_joined.set(Some(id));
                        self.dispatch(Input::JoinChannel(id));
                    }
                }
            }
            "switch-workspace" => {
                if let Ok(id) = attr("id").parse::<WorkspaceId>() {
                    self.ui.borrow_mut().menu = None;
                    self.dispatch(Input::SwitchWorkspace(id));
                }
            }
            // The shell takes this address and shows its server screen (D49).
            "change-server" => {
                if let Some(window) = web_sys::window() {
                    let _ = window.location().set_href("lazy-shell://change-server");
                }
            }
            "sign-out" => {
                self.ui.borrow_mut().menu = None;
                self.dispatch(Input::SignOut);
            }
            "browse" => self.open_dialog(Dialog::Browse),
            "create-channel" => self.open_dialog(Dialog::CreateChannel),
            "new-message" => self.open_dialog(Dialog::NewMessage { picked: Vec::new() }),
            "open-search" => self.open_dialog(Dialog::Search),
            "open-switcher" => self.open_dialog(Dialog::Switcher {
                query: String::new(),
                highlighted: 0,
            }),
            "close-dialog" => self.close_dialog(),
            "pick-person" | "unpick-person" => {
                if let Ok(user) = attr("id").parse::<UserId>() {
                    if let Some(Dialog::NewMessage { picked }) = &mut self.ui.borrow_mut().dialog {
                        if let Some(at) = picked.iter().position(|p| *p == user) {
                            picked.remove(at);
                        } else if action == "pick-person" {
                            picked.push(user);
                        }
                    }
                    self.request_frame();
                }
            }
            "start-conversation" => {
                let picked = match &self.ui.borrow().dialog {
                    Some(Dialog::NewMessage { picked }) => picked.clone(),
                    _ => Vec::new(),
                };
                if !picked.is_empty() {
                    self.dispatch(Input::OpenConversation {
                        participants: picked,
                    });
                }
            }
            "recent-search" => {
                let query = attr("q");
                if let Some(field) = input("search-q") {
                    field.set_value(&query);
                }
                self.dispatch(Input::SetSearchQuery { text: query });
                self.dispatch(Input::RunSearch);
            }
            "open-hit" => {
                if let Ok(index) = attr("index").parse::<usize>() {
                    self.ui.borrow_mut().dialog = None;
                    self.dispatch(Input::OpenSearchHit(index));
                    self.dispatch(Input::CloseSearch);
                }
            }
            "search-more" => self.dispatch(Input::LoadMoreSearch),
            "browse-more" => self.dispatch(Input::LoadMoreBrowse),
            "people-more" => self.dispatch(Input::LoadMorePeople),
            "dialog-retry" => {
                let dialog = self.ui.borrow().dialog.clone();
                match dialog {
                    Some(Dialog::Browse) => {
                        let query = input("browse-q").map(|f| f.value()).unwrap_or_default();
                        self.dispatch(Input::SetBrowseQuery(query));
                    }
                    Some(Dialog::NewMessage { .. }) => {
                        let query = input("people-q").map(|f| f.value()).unwrap_or_default();
                        self.dispatch(Input::SetPeopleQuery(query));
                    }
                    _ => {}
                }
            }
            "switch-to" => {
                if let Ok(index) = attr("index").parse::<usize>() {
                    self.choose_switcher(index);
                }
            }
            "suggest-pick" => {
                if let Ok(index) = attr("index").parse::<usize>() {
                    self.pick_suggestion(index);
                }
            }
            "dismiss-notice" => self.dispatch(Input::DismissNotice),
            "notifications-on" => self.dispatch(Input::TurnOnNotifications),
            "notifications-later" => self.dispatch(Input::DismissNotificationPrompt),
            "edit-topic" | "rename" => {
                if let Some(id) = conv() {
                    self.ui.borrow_mut().menu = None;
                    let (question, current) = {
                        let core = self.core.borrow();
                        let conversation = core.conversation(id);
                        if action == "rename" {
                            (
                                "The channel's new name:",
                                conversation
                                    .and_then(|c| c.name.as_ref())
                                    .map(|name| name.as_str().to_owned())
                                    .unwrap_or_default(),
                            )
                        } else {
                            (
                                "The channel's topic (empty to clear it):",
                                conversation
                                    .and_then(|c| c.topic.clone())
                                    .unwrap_or_default(),
                            )
                        }
                    };
                    let answer = web_sys::window()
                        .and_then(|w| w.prompt_with_message_and_default(question, &current).ok())
                        .flatten();
                    match answer {
                        Some(text) if text != current => self.dispatch(if action == "rename" {
                            Input::UpdateConversation {
                                id,
                                name: Some(text),
                                topic: None,
                                description: None,
                            }
                        } else {
                            Input::UpdateConversation {
                                id,
                                name: None,
                                topic: Some(text),
                                description: None,
                            }
                        }),
                        _ => self.request_frame(),
                    }
                }
            }
            "retry-load" => self.dispatch(Input::RetryLoad),
            "show-register" | "show-sign-in" => {
                self.ui.borrow_mut().registering = action == "show-register";
                self.request_frame();
            }
            "permalink" => {
                if let Some(link) = lazy_domain::Permalink::parse(&attr("url")) {
                    self.dispatch(Input::OpenPermalink(link));
                }
            }
            _ => return,
        }
        event.prevent_default();
    }

    fn choose_switcher(self: &Rc<Self>, index: usize) {
        let query = match &self.ui.borrow().dialog {
            Some(Dialog::Switcher { query, .. }) => query.clone(),
            _ => return,
        };
        let results = self.core.borrow().quick_switcher_results(&query);
        if let Some(item) = results.get(index.min(results.len().saturating_sub(1))) {
            let target = item.target.clone();
            self.ui.borrow_mut().dialog = None;
            match target {
                lazy_client_core::QuickSwitcherTarget::BrowseChannels { .. } => {
                    self.open_dialog(Dialog::Browse)
                }
                lazy_client_core::QuickSwitcherTarget::Search => self.open_dialog(Dialog::Search),
                target => self.dispatch(Input::ChooseQuickSwitcher(target)),
            }
        }
    }

    fn on_submit(self: &Rc<Self>, form: &Element) {
        let field = |name: &str| {
            form.query_selector(&format!("[name=\"{name}\"]"))
                .ok()
                .flatten()
                .and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
        };
        let value = |name: &str| field(name).map(|f| f.value()).unwrap_or_default();
        match form.id().as_str() {
            "signin-form" => self.dispatch(Input::SignIn {
                username: value("username"),
                password: value("password"),
            }),
            "register-form" => self.dispatch(Input::Register {
                username: value("username"),
                display_name: value("display_name"),
                password: value("password"),
                invite_code: value("invite_code"),
            }),
            "create-channel-form" => {
                let private = field("private").is_some_and(|f| f.checked());
                self.dispatch(Input::CreateChannel {
                    name: value("name"),
                    visibility: if private {
                        lazy_domain::ChannelVisibility::Private
                    } else {
                        lazy_domain::ChannelVisibility::Public
                    },
                    topic: value("topic"),
                    description: String::new(),
                });
            }
            "profile-form" => self.dispatch(Input::UpdateProfile {
                display_name: Some(value("display_name")),
            }),
            "password-form" => {
                {
                    let mut ui = self.ui.borrow_mut();
                    ui.password_sent = true;
                    ui.password_changed = false;
                }
                self.dispatch(Input::ChangePassword {
                    current: value("current"),
                    new: value("new"),
                    confirm: value("confirm"),
                });
            }
            "search-form" => {
                let query = input("search-q").map(|f| f.value()).unwrap_or_default();
                self.dispatch(Input::SetSearchQuery { text: query });
                self.dispatch(Input::RunSearch);
            }
            _ => {}
        }
    }

    fn on_input(self: &Rc<Self>, target: &Element) {
        match target.id().as_str() {
            id @ ("draft" | "thread-draft") => self.typed(id),
            "browse-q" => {
                if let Some(field) = input("browse-q") {
                    self.dispatch(Input::SetBrowseQuery(field.value()));
                }
            }
            "people-q" => {
                if let Some(field) = input("people-q") {
                    self.dispatch(Input::SetPeopleQuery(field.value()));
                }
            }
            "switcher-q" => {
                if let Some(field) = input("switcher-q") {
                    if let Some(Dialog::Switcher { query, highlighted }) =
                        &mut self.ui.borrow_mut().dialog
                    {
                        *query = field.value();
                        *highlighted = 0;
                    }
                    self.request_frame();
                }
            }
            "picker-q" => {
                if let Some(field) = input("picker-q") {
                    self.ui.borrow_mut().picker_query = field.value();
                    self.request_frame();
                }
            }
            _ => {}
        }
    }

    /// Keys. `true` when the key was used.
    fn on_key(self: &Rc<Self>, event: &KeyboardEvent, target: Option<&Element>) -> bool {
        let key = event.key();
        let composing = event.is_composing() || event.key_code() == 229;
        let id = target.map(|t| t.id()).unwrap_or_default();
        let command = event.ctrl_key() || event.meta_key();
        if command && key.eq_ignore_ascii_case("k") && self.page.borrow().screen == Screen::Shell {
            self.open_dialog(Dialog::Switcher {
                query: String::new(),
                highlighted: 0,
            });
            return true;
        }
        if key == "Escape" {
            if self.ui.borrow().picker.is_some() || self.ui.borrow().menu.is_some() {
                self.close_popover();
                return true;
            }
            if self.ui.borrow().dialog.is_some() {
                self.close_dialog();
                return true;
            }
            if self.ui.borrow().editing.is_some() {
                self.stop_editing();
                return true;
            }
            if self.core.borrow().open_thread().is_some() && id == "thread-draft" {
                self.dispatch(Input::CloseThread);
                return true;
            }
            return false;
        }
        let suggesting = self.ui.borrow().suggest.as_ref().map(|s| s.field);
        if !composing && suggesting == Some(id.as_str()) && self.suggest_key(&key) {
            return true;
        }
        match id.as_str() {
            "draft" | "thread-draft" if key == "Enter" && !event.shift_key() && !composing => {
                if id == "draft" {
                    self.send_composer();
                } else {
                    self.send_reply();
                }
                true
            }
            "draft" if key == "ArrowUp" && !composing => {
                let empty = textarea("draft").is_some_and(|d| d.value().is_empty());
                empty && self.ui.borrow().editing.is_none() && self.edit_last()
            }
            "switcher-q" => {
                let count = match &self.ui.borrow().dialog {
                    Some(Dialog::Switcher { query, .. }) => {
                        self.core.borrow().quick_switcher_results(query).len()
                    }
                    _ => 0,
                };
                let step: isize = match key.as_str() {
                    "ArrowDown" => 1,
                    "ArrowUp" => -1,
                    "Enter" => {
                        let index = match &self.ui.borrow().dialog {
                            Some(Dialog::Switcher { highlighted, .. }) => *highlighted,
                            _ => 0,
                        };
                        self.choose_switcher(index);
                        return true;
                    }
                    _ => return false,
                };
                if let Some(Dialog::Switcher { highlighted, .. }) = &mut self.ui.borrow_mut().dialog
                    && count > 0
                {
                    *highlighted =
                        (*highlighted as isize + step).rem_euclid(count as isize) as usize;
                }
                self.request_frame();
                true
            }
            _ => false,
        }
    }
}

/// The page's frame, drawn once after sign-in. The composers and fields in it keep what is typed.
const SHELL: &str = r#"<div class="shell" id="shell">
<nav class="sidebar" id="sidebar" aria-label="Conversations"></nav>
<div class="scrim-side" data-action="toggle-sidebar"></div>
<main class="main" id="main">
  <header class="header" id="header"></header>
  <div id="banner" class="banner"></div>
  <div class="timeline" id="timeline" tabindex="-1"><div class="rows" id="rows" role="log" aria-live="polite"></div></div>
  <div class="composer-wrap">
    <div id="blocker"></div>
    <div class="composer" id="composer">
      <div class="suggest" id="suggest"></div>
      <div class="editing-bar" id="editing-bar"></div>
      <div class="chips" id="chips"></div>
      <div class="composer-box">
        <textarea id="draft" rows="1" aria-label="Message" enterkeyhint="send"></textarea>
        <div class="composer-tools">
          <button type="button" class="icon" data-action="attach" data-target="conversation" title="Attach files" aria-label="Attach files">{attach}</button>
          <button type="button" class="icon" data-action="composer-emoji" title="Emoji" aria-label="Emoji">{smile}</button>
          <button type="button" class="primary send" data-action="send">Send</button>
        </div>
      </div>
      <div class="hint">Enter to send · Shift+Enter for a new line · **bold** _italic_ `code` · Ctrl+K to jump</div>
    </div>
  </div>
</main>
<aside class="thread" id="thread" hidden aria-label="Thread">
  <div class="thread-head" id="thread-head"></div>
  <div class="thread-scroll" id="thread-scroll"><div class="rows" id="thread-rows"></div></div>
  <div class="composer" id="thread-composer">
    <div class="suggest" id="thread-suggest"></div>
    <div class="chips" id="thread-chips"></div>
    <div class="composer-box">
      <textarea id="thread-draft" rows="1" aria-label="Reply" placeholder="Reply…" enterkeyhint="send"></textarea>
      <div class="composer-tools">
        <button type="button" class="icon" data-action="attach" data-target="thread" title="Attach files" aria-label="Attach files">{attach}</button>
        <button type="button" class="icon" data-action="thread-emoji" title="Emoji" aria-label="Emoji">{smile}</button>
        <button type="button" class="primary send" data-action="send-reply">Reply</button>
      </div>
    </div>
    <label class="check"><input type="checkbox" id="also-channel"> Also send to the channel</label>
  </div>
</aside>
</div>
<div id="overlay"></div>
<div id="popover" class="popover"></div>
<input type="file" id="file-input" multiple hidden>"#;

/// Puts `rows` in `container`, replacing only the rows whose markup changed and moving the rest.
/// `shown` is what the container holds, and becomes `rows`.
fn patch_rows(container: &Element, shown: &mut Vec<(String, String)>, rows: Vec<Row>) {
    if *shown == rows {
        return;
    }
    let mut existing: HashMap<String, Element> = HashMap::new();
    let children = container.children();
    for index in 0..children.length() {
        if let Some(child) = children.item(index)
            && let Some(key) = child.get_attribute("data-key")
        {
            existing.insert(key, child);
        }
    }
    let previous: HashMap<String, String> = shown.drain(..).collect();
    let document = document();
    let mut cursor = container.first_element_child();
    for (key, html) in &rows {
        let element = match existing.remove(key) {
            Some(element) => {
                if previous.get(key) != Some(html) {
                    element.set_inner_html(html);
                }
                element
            }
            None => {
                let Ok(element) = document.create_element("div") else {
                    continue;
                };
                element.set_class_name("r");
                let _ = element.set_attribute("data-key", key);
                element.set_inner_html(html);
                element
            }
        };
        let in_place = cursor
            .as_ref()
            .is_some_and(|c| c.is_same_node(Some(element.as_ref())));
        if in_place {
            cursor = element.next_element_sibling();
        } else {
            let before: Option<&web_sys::Node> = cursor.as_ref().map(|c| c.as_ref());
            let _ = container.insert_before(&element, before);
        }
    }
    for (_, stale) in existing {
        stale.remove();
    }
    *shown = rows;
}

/// The suggestion list for a composer, when it has suggestions open.
fn suggestions(core: &ClientCore, ui: &Ui, field: &str, conversation: ConversationId) -> String {
    match &ui.suggest {
        Some(open) if open.field == field => {
            let items = view::suggest::items(core, conversation, &open.token);
            view::suggest::list(&items, open.highlighted)
        }
        _ => String::new(),
    }
}

/// The key of the first message row (not the history line or a divider).
fn first_message_key(rows: &[(String, String)]) -> Option<String> {
    rows.iter()
        .map(|(key, _)| key)
        .find(|key| key.starts_with('m') || key.starts_with('o'))
        .cloned()
}

fn at_bottom(scroller: &Element) -> bool {
    scroller.scroll_height() - scroller.scroll_top() - scroller.client_height() <= EDGE
}

fn page_visible() -> bool {
    let document = document();
    document.visibility_state() == web_sys::VisibilityState::Visible
}

fn is_narrow() -> bool {
    web_sys::window()
        .and_then(|window| window.inner_width().ok())
        .and_then(|width| width.as_f64())
        .is_some_and(|width| width < 760.0)
}

fn set_hidden(element: &Option<Element>, hidden: bool) {
    if let Some(element) = element
        && let Ok(element) = element.clone().dyn_into::<HtmlElement>()
        && element.hidden() != hidden
    {
        element.set_hidden(hidden);
    }
}

fn set_class(element: &Option<Element>, class: &str, on: bool) {
    if let Some(element) = element {
        let _ = element.class_list().toggle_with_force(class, on);
    }
}

/// Grows a composer with its text, up to a limit set in the style sheet.
fn autosize(area: &HtmlTextAreaElement) {
    let style = area.style();
    let _ = style.set_property("height", "auto");
    let _ = style.set_property("height", &format!("{}px", area.scroll_height()));
}

/// Highlights a row for a moment.
fn flash(row: &Element) {
    let _ = row.class_list().add_1("flash");
    let row = row.clone();
    let clear = Closure::once_into_js(move || {
        let _ = row.class_list().remove_1("flash");
    });
    if let Some(window) = web_sys::window() {
        let _ = window
            .set_timeout_with_callback_and_timeout_and_arguments_0(clear.unchecked_ref(), 1600);
    }
}

/// Places the popover by its anchor, inside the window.
fn place_popover(popover: &Element, anchor: Option<&Element>, width: i32, height: i32) {
    let Ok(popover) = popover.clone().dyn_into::<HtmlElement>() else {
        return;
    };
    let window = web_sys::window();
    let view_w = window
        .as_ref()
        .and_then(|w| w.inner_width().ok())
        .and_then(|v| v.as_f64())
        .unwrap_or(1024.0) as i32;
    // What can be seen: a phone's keyboard covers the rest of the window.
    let view_h = window
        .as_ref()
        .and_then(|w| w.visual_viewport())
        .map(|v| v.height())
        .or_else(|| window.as_ref()?.inner_height().ok()?.as_f64())
        .unwrap_or(768.0) as i32;
    let (mut left, mut top) = (view_w / 2 - width / 2, view_h / 2 - height / 2);
    if let Some(anchor) = anchor {
        let rect = anchor.get_bounding_client_rect();
        left = rect.left() as i32;
        top = rect.bottom() as i32 + 4;
        if top + height > view_h - 8 {
            top = rect.top() as i32 - height - 4;
        }
    }
    left = left.clamp(8, (view_w - width - 8).max(8));
    top = top.clamp(8, (view_h - height - 8).max(8));
    let style = popover.style();
    let _ = style.set_property("left", &format!("{left}px"));
    let _ = style.set_property("top", &format!("{top}px"));
}

/// Runs `task` after the current event, outside any borrow.
fn defer(task: impl FnOnce() + 'static) {
    let callback = Closure::once_into_js(task);
    if let Some(window) = web_sys::window() {
        let _ = window
            .set_timeout_with_callback_and_timeout_and_arguments_0(callback.unchecked_ref(), 0);
    }
}

fn confirm(text: &str) -> bool {
    web_sys::window()
        .and_then(|window| window.confirm_with_message(text).ok())
        .unwrap_or(false)
}

/// What an emoji is as composer text: the glyph, or a custom emoji's `:name:`.
fn emoji_text(core: &ClientCore, emoji: ReactionEmoji) -> String {
    match emoji {
        ReactionEmoji::Unicode(emoji) => emoji.as_str().to_owned(),
        ReactionEmoji::Custom(id) => core
            .custom_emoji_by_id(id)
            .map(|custom| format!(":{}: ", custom.name))
            .unwrap_or_default(),
    }
}

fn report_window_active() {
    let document = document();
    let active = document.visibility_state() == web_sys::VisibilityState::Visible
        && document.has_focus().unwrap_or(true);
    dispatch(Input::WindowActive(active));
    if let Some(app) = app() {
        app.page.borrow_mut().reported_bottom = None;
        app.page.borrow_mut().reported_thread_bottom = None;
        app.request_frame();
    }
}

/// Listens to the page: one listener per kind of event, for every element.
fn listen() {
    let document = document();
    // `Fn`, not `FnMut`: a handler can run inside itself (the attach button clicks the file
    // input, whose click reaches the same listener), which a `FnMut` closure refuses.
    let on = |target: &web_sys::EventTarget, kind: &str, handler: Box<dyn Fn(Event)>| {
        let closure = Closure::<dyn Fn(Event)>::wrap(handler);
        let _ = target.add_event_listener_with_callback(kind, closure.as_ref().unchecked_ref());
        closure.forget();
    };
    on(
        document.as_ref(),
        "click",
        Box::new(|event: Event| {
            let Some(app) = app() else { return };
            let target = event.target().and_then(|t| t.dyn_into::<Element>().ok());
            let action = target
                .as_ref()
                .and_then(|t| t.closest("[data-action]").ok().flatten());
            // A click outside the popover closes it.
            let in_popover = target
                .as_ref()
                .and_then(|t| t.closest("#popover").ok().flatten())
                .is_some();
            if shell_picture_click(target.as_ref(), &event) {
                return;
            }
            if let Some(element) = action {
                let name = element.get_attribute("data-action").unwrap_or_default();
                let keeps_popover = in_popover
                    || matches!(
                        name.as_str(),
                        "workspace-menu"
                            | "conversation-menu"
                            | "account-menu"
                            | "react"
                            | "composer-emoji"
                            | "thread-emoji"
                    );
                if !keeps_popover {
                    app.close_popover();
                }
                app.on_action(&name, &element, &event);
            } else if !in_popover {
                app.close_popover();
            }
            pick_message(target.as_ref());
        }),
    );
    on(
        document.as_ref(),
        "input",
        Box::new(|event: Event| {
            let Some(app) = app() else { return };
            if let Some(target) = event.target().and_then(|t| t.dyn_into::<Element>().ok()) {
                app.on_input(&target);
            }
        }),
    );
    on(
        document.as_ref(),
        "submit",
        Box::new(|event: Event| {
            event.prevent_default();
            let Some(app) = app() else { return };
            if let Some(form) = event.target().and_then(|t| t.dyn_into::<Element>().ok()) {
                app.on_submit(&form);
            }
        }),
    );
    on(
        document.as_ref(),
        "keydown",
        Box::new(|event: Event| {
            let Some(app) = app() else { return };
            let Ok(key) = event.clone().dyn_into::<KeyboardEvent>() else {
                return;
            };
            let target = event.target().and_then(|t| t.dyn_into::<Element>().ok());
            if app.on_key(&key, target.as_ref()) {
                event.prevent_default();
            }
        }),
    );
    on(
        document.as_ref(),
        "change",
        Box::new(|event: Event| {
            let Some(app) = app() else { return };
            let Some(chooser) = event
                .target()
                .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
                .filter(|t| t.id() == "file-input")
            else {
                return;
            };
            if let (Some(target), Some(files)) = (app.attach_target.take(), chooser.files()) {
                app.attach(target, files);
            }
        }),
    );
    on(
        document.as_ref(),
        "paste",
        Box::new(|event: Event| {
            let Some(app) = app() else { return };
            let Ok(clip) = event.clone().dyn_into::<web_sys::ClipboardEvent>() else {
                return;
            };
            let target = event
                .target()
                .and_then(|t| t.dyn_into::<Element>().ok())
                .map(|t| t.id())
                .unwrap_or_default();
            let composer = match target.as_str() {
                "draft" => app
                    .core
                    .borrow()
                    .selected_conversation()
                    .map(ComposerTarget::Conversation),
                "thread-draft" => app
                    .core
                    .borrow()
                    .open_thread()
                    .map(|open| ComposerTarget::Thread(open.root)),
                _ => None,
            };
            if let (Some(composer), Some(files)) = (
                composer,
                clip.clipboard_data().and_then(|data| data.files()),
            ) && files.length() > 0
            {
                event.prevent_default();
                app.attach(composer, files);
            }
        }),
    );
    for kind in ["dragover", "drop"] {
        on(
            document.as_ref(),
            kind,
            Box::new(move |event: Event| {
                let Some(app) = app() else { return };
                let Ok(drag) = event.clone().dyn_into::<web_sys::DragEvent>() else {
                    return;
                };
                let in_thread = event
                    .target()
                    .and_then(|t| t.dyn_into::<Element>().ok())
                    .and_then(|t| t.closest("#thread").ok().flatten())
                    .is_some();
                let composer = if in_thread {
                    app.core
                        .borrow()
                        .open_thread()
                        .map(|open| ComposerTarget::Thread(open.root))
                } else {
                    app.core
                        .borrow()
                        .selected_conversation()
                        .map(ComposerTarget::Conversation)
                };
                let Some(composer) = composer else { return };
                event.prevent_default();
                if event.type_() == "drop"
                    && let Some(files) = drag.data_transfer().and_then(|data| data.files())
                {
                    app.attach(composer, files);
                }
            }),
        );
    }
    if let Some(window) = web_sys::window() {
        for kind in ["focus", "blur"] {
            on(
                window.as_ref(),
                kind,
                Box::new(|_: Event| report_window_active()),
            );
        }
        // Width only: on Android a keyboard opening changes the height, and it opens when a field
        // in the popover (the emoji picker's search) is tapped.
        let width = Cell::new(window.inner_width().ok().and_then(|w| w.as_f64()));
        on(
            window.as_ref(),
            "resize",
            Box::new(move |_: Event| {
                let now = web_sys::window().and_then(|w| w.inner_width().ok()?.as_f64());
                if width.replace(now) != now
                    && let Some(app) = app()
                {
                    app.close_popover();
                }
            }),
        );
    }
    on(
        document.as_ref(),
        "visibilitychange",
        Box::new(|_: Event| report_window_active()),
    );
    if let Some(viewport) = web_sys::window().and_then(|w| w.visual_viewport()) {
        for kind in ["resize", "scroll"] {
            on(viewport.as_ref(), kind, Box::new(|_: Event| fit_viewport()));
        }
        fit_viewport();
    }
}

/// In a shell a picture opens over the page: following its link would replace the page with the
/// picture, with nothing to go back with on iOS. A tap on the picture puts it away.
fn shell_picture_click(target: Option<&Element>, event: &Event) -> bool {
    if !view::html::in_shell() {
        return false;
    }
    let Some(target) = target else { return false };
    if target.closest(".lightbox-save").ok().flatten().is_some() {
        return false;
    }
    if close_picture() {
        event.prevent_default();
        return true;
    }
    let Some(href) = target
        .closest("a.image")
        .ok()
        .flatten()
        .and_then(|link| link.get_attribute("href"))
    else {
        return false;
    };
    event.prevent_default();
    let document = document();
    let (Ok(cover), Ok(picture), Ok(save), Some(body)) = (
        document.create_element("div"),
        document.create_element("img"),
        document.create_element("a"),
        document.body(),
    ) else {
        return true;
    };
    cover.set_id("lightbox");
    cover.set_class_name("lightbox");
    let _ = picture.set_attribute("src", &href);
    let _ = picture.set_attribute("alt", "");
    save.set_class_name("lightbox-save primary");
    let _ = save.set_attribute("href", &href);
    let _ = save.set_attribute("download", "");
    save.set_text_content(Some("Save"));
    let _ = cover.append_child(&picture);
    let _ = cover.append_child(&save);
    let _ = body.append_child(&cover);
    true
}

/// Puts away the open picture; whether there was one.
fn close_picture() -> bool {
    match by_id("lightbox") {
        Some(cover) => {
            cover.remove();
            true
        }
        None => false,
    }
}

/// What the Android shell's back button calls: closes the topmost thing open, and answers whether
/// there was one (if not, the shell leaves the app).
fn go_back() -> bool {
    let Some(app) = app() else { return false };
    if close_picture() {
        return true;
    }
    if app.ui.borrow().picker.is_some() || app.ui.borrow().menu.is_some() {
        app.close_popover();
        return true;
    }
    if app.ui.borrow().dialog.is_some() {
        app.close_dialog();
        return true;
    }
    if app.ui.borrow().editing.is_some() {
        app.stop_editing();
        return true;
    }
    if app.ui.borrow().sidebar_open {
        app.ui.borrow_mut().sidebar_open = false;
        app.request_frame();
        return true;
    }
    if app.core.borrow().open_thread().is_some() {
        app.dispatch(Input::CloseThread);
        return true;
    }
    false
}

/// Keeps the page the size of what can be seen. A phone's keyboard covers the page without
/// resizing it, so the composer would be under the keyboard; `--viewport-height` follows the part
/// still showing. A list that was at its newest message stays there as it shrinks.
fn fit_viewport() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(viewport) = window.visual_viewport() else {
        return;
    };
    // Zooming in with two fingers also shrinks this viewport; the page must not follow that.
    if viewport.scale() > 1.01 {
        return;
    }
    // iOS scrolls the whole page to show a focused field, which would push the header off the top.
    window.scroll_to_with_x_and_y(0.0, 0.0);
    let Some(root) = document()
        .document_element()
        .and_then(|e| e.dyn_into::<HtmlElement>().ok())
    else {
        return;
    };
    let height = format!("{}px", viewport.height());
    let style = root.style();
    if style
        .get_property_value("--viewport-height")
        .ok()
        .as_deref()
        == Some(height.as_str())
    {
        return;
    }
    let following: Vec<Element> = ["timeline", "thread-scroll"]
        .into_iter()
        .filter_map(by_id)
        .filter(at_bottom)
        .collect();
    let _ = style.set_property("--viewport-height", &height);
    for scroller in following {
        scroller.set_scroll_top(scroller.scroll_height());
    }
    // An open popover moves up out of the keyboard's way.
    if let Some(popover) = by_id("popover").and_then(|e| e.dyn_into::<HtmlElement>().ok()) {
        let rect = popover.get_bounding_client_rect();
        if rect.height() > 0.0 && rect.bottom() > viewport.height() - 8.0 {
            let top = (viewport.height() - rect.height() - 8.0).max(8.0);
            let _ = popover.style().set_property("top", &format!("{top}px"));
        }
    }
}

/// A touchscreen cannot hover, which is how a message's actions appear: a tap on a message shows
/// them, and a tap on it again or anywhere else puts them away. Tapping something inside a message
/// (a link, a reaction, one of the actions) does not count as tapping the message.
fn pick_message(target: Option<&Element>) {
    let touch = web_sys::window()
        .and_then(|w| w.match_media("(hover: none)").ok().flatten())
        .is_some_and(|query| query.matches());
    if !touch {
        return;
    }
    let Some(target) = target else { return };
    let picked = document().query_selector(".msg.picked").ok().flatten();
    let inside = |selector: &str| target.closest(selector).ok().flatten();
    if inside("a, button, img, input, textarea").is_some() {
        if inside(".msg.picked").is_none()
            && let Some(picked) = picked
        {
            let _ = picked.class_list().remove_1("picked");
        }
        return;
    }
    // Finishing a text selection is not a tap.
    let selecting = web_sys::window()
        .and_then(|w| w.get_selection().ok().flatten())
        .is_some_and(|selection| !selection.is_collapsed());
    if selecting {
        return;
    }
    let tapped = inside(".msg");
    if let Some(picked) = &picked {
        let _ = picked.class_list().remove_1("picked");
    }
    if let Some(tapped) = tapped
        && picked.as_ref() != Some(&tapped)
    {
        let _ = tapped.class_list().add_1("picked");
        // The newest message's actions open below the bottom of the list.
        if let Ok(Some(actions)) = tapped.query_selector(".msg-actions") {
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_block(web_sys::ScrollLogicalPosition::Nearest);
            actions.scroll_into_view_with_scroll_into_view_options(&options);
        }
    }
}

/// Scrolling does not bubble: the lists are listened to themselves, once each is on the page.
fn listen_scrolling() {
    for id in ["timeline", "thread-scroll"] {
        if let Some(element) = by_id(id) {
            let closure = Closure::<dyn Fn(Event)>::new(|_: Event| {
                if let Some(app) = app() {
                    app.scrolled();
                }
            });
            let options = web_sys::AddEventListenerOptions::new();
            options.set_passive(true);
            let _ = element.add_event_listener_with_callback_and_add_event_listener_options(
                "scroll",
                closure.as_ref().unchecked_ref(),
                &options,
            );
            closure.forget();
        }
    }
}
