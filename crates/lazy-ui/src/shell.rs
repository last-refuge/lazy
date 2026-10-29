//! The signed-in layout: workspace rail, conversation sidebar, and the conversation pane, with
//! the dialog layer above them.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Anchor, AnyElement, AnyView, Context, DragMoveEvent, Entity, EventEmitter, FocusHandle,
    FontWeight, IntoElement, ParentElement, Pixels, Point, Render, Role, Styled, Subscription,
    Window, anchored, deferred, div, prelude::*, px,
};
use lazy_client_core::{
    ActionKind, ActionState, Change, ConnectionState, Input, QuickSwitcherTarget,
};
use lazy_domain::notification::NotifyLevel;
use lazy_domain::{
    ChannelVisibility, ConversationId, ConversationKind, MessageId, Permalink, ReactionEmoji,
    UserId,
};

use gpui::{Action as _, Focusable as _};

use crate::app_model::{AppEvent, AppModel};
use crate::commands::{self, Availability, CommandId, DialogState, Group};
use crate::composer::{Composer, ComposerEvent};
use crate::dialogs::{
    AddWorkspaceDialog, BrowseChannelsDialog, CreateChannelDialog, CustomEmojiDialog,
    DeleteMessageDialog, DetailsDialog, DetailsRequest, DiagnosticsDialog, GroupsDialog,
    InvitationsDialog, PeopleDialog, PeopleMode, PreferencesDialog, ProfileDialog, ProfileRequest,
    QuickSwitcherChoice, QuickSwitcherDialog, QuickSwitcherRun, ShortcutsDialog,
    WorkspaceSettingsDialog,
};
use crate::emoji_font;
use crate::emoji_picker::{EmojiPicker, EmojiPickerEvent, PickerPrefs};
use crate::form::{self, ButtonKind};
use crate::inbox_view::{InboxEvent, ThreadInboxView};
use crate::keyboard::{self, FocusNextControl, FocusPrevControl};
use crate::menu::{Menu, MenuEvent, MenuItem};
use crate::modal::{DialogDismiss, DialogEvent};
use crate::people_view::{PeopleDirectoryView, PeopleEvent};
use crate::rail::{self, RailEvent, RailView};
use crate::saved_view::{SavedEvent, SavedMessagesView};
use crate::search_view::{SearchEvent, SearchResultsView};
use crate::sidebar::{SidebarEvent, SidebarView};
use crate::theme::Theme;
use crate::thread_layout::{self, HANDLE_WIDTH};
use crate::thread_pane::{PaneLayout, ThreadPane, ThreadPaneEvent, ThreadResizeDrag};
use crate::timeline::{TimelineEvent, TimelineView};
use crate::viewer::ImageViewer;

const SIDEBAR_WIDTH: f32 = 260.0;
/// Below this window width the sidebar narrows and the workspace rail is hidden.
pub const COMPACT_BELOW: f32 = 760.0;
const COMPACT_SIDEBAR_WIDTH: f32 = 208.0;

/// The dialog currently open, with the subscriptions that close it.
struct Overlay {
    view: AnyView,
    _subscriptions: Vec<Subscription>,
}

/// Where text is selected. There is one selection at a time across these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionPlace {
    Timeline,
    Pane,
    Composer,
    PaneComposer,
}

/// What the emoji picker was opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerPurpose {
    /// React to a message with the chosen emoji.
    Reaction {
        conversation: ConversationId,
        message: MessageId,
    },
    /// Type the chosen emoji into the composer.
    Composer,
    /// Type the chosen emoji into the thread pane's reply composer.
    ThreadComposer,
}

/// The emoji picker popover and what it needs to hand focus back.
struct PickerOverlay {
    view: Entity<EmojiPicker>,
    purpose: PickerPurpose,
    anchor: Point<Pixels>,
    /// What had focus when the picker opened.
    previous_focus: Option<FocusHandle>,
    _subscription: Subscription,
}

/// A dialog the shell can open, for commands that are not a click on a visible control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellDialog {
    CreateChannel,
    BrowseChannels,
    NewMessage,
    /// Manage user groups (admins and owners).
    Groups,
    /// Manage the workspace's custom emoji (admins and owners).
    CustomEmoji,
    /// Notification settings and status.
    Preferences,
    Details(ConversationId),
    /// Add people to a channel.
    AddPeople(ConversationId),
    /// The quick switcher (Ctrl+K).
    QuickSwitcher,
    /// A person's profile.
    Profile(UserId),
    /// The workspace's name and settings (admins and owners).
    WorkspaceSettings,
    /// The workspace's invitations (admins and owners).
    Invitations,
    /// Create a workspace or join one with a code (the rail's "+").
    AddWorkspace,
    /// Confirm deleting a message.
    DeleteMessage {
        conversation: ConversationId,
        message: MessageId,
    },
    /// The keyboard shortcuts (Ctrl+/).
    Shortcuts,
    /// Help, Diagnostics…
    Diagnostics,
}

/// A search result being opened.
#[derive(Debug, Clone, Copy)]
struct OpeningHit {
    /// For a reply, the root of its thread.
    thread_root: Option<MessageId>,
}

pub struct ShellView {
    model: Entity<AppModel>,
    /// The workspace rail at the left edge.
    rail: Entity<RailView>,
    sidebar: Entity<SidebarView>,
    timeline: Entity<TimelineView>,
    composer: Entity<Composer>,
    thread_pane: Entity<ThreadPane>,
    /// The search results, shown in place of the conversation while search is open.
    search: Entity<SearchResultsView>,
    /// What had the keyboard when search opened: Escape gives it back.
    search_return_focus: Option<FocusHandle>,
    /// The conversation selected when search opened. Choosing another one (in the sidebar) ends
    /// the search; opening a result does so itself.
    search_selected_at_open: Option<ConversationId>,
    /// A chosen result the core is still finding (its message may have to be fetched): search stays
    /// up, saying so, until the conversation (or thread, or a notice) is there.
    search_opening: Option<OpeningHit>,
    /// The thread inbox, shown in place of the conversation while it is open.
    inbox: Entity<ThreadInboxView>,
    /// What had the keyboard when the inbox opened: Escape gives it back.
    inbox_return_focus: Option<FocusHandle>,
    inbox_selected_at_open: Option<ConversationId>,
    /// The saved messages, shown in place of the conversation while they are open.
    saved: Entity<SavedMessagesView>,
    saved_return_focus: Option<FocusHandle>,
    saved_selected_at_open: Option<ConversationId>,
    /// A chosen saved message the core is still finding: the view stays up, saying so.
    saved_opening: Option<MessageId>,
    /// The people directory, shown in place of the conversation while it is open.
    people: Entity<PeopleDirectoryView>,
    people_return_focus: Option<FocusHandle>,
    people_selected_at_open: Option<ConversationId>,
    /// "Message" or a person chosen in the quick switcher asked for a conversation: when it
    /// opens, the keyboard goes to its composer.
    awaiting_conversation: bool,
    /// The bell in the conversation header and the menu it opens.
    bell_focus: FocusHandle,
    bell_menu: Option<Entity<Menu>>,
    /// The command menu ("Menu" at the top of the sidebar) while it is open.
    command_menu: Option<Entity<Menu>>,
    /// How the pane is laid out this frame; the pane reads it when it draws.
    pane_layout: Rc<Cell<PaneLayout>>,
    /// What had the keyboard when the thread opened: Escape in the pane gives it back.
    thread_return_focus: Option<FocusHandle>,
    overlay: Option<Overlay>,
    picker: Option<PickerOverlay>,
    /// The conversation selected when the open dialog appeared, to notice when the dialog's work
    /// moved the person somewhere new (and then put them in that conversation's composer).
    selected_at_open: Option<ConversationId>,
    star_focus: FocusHandle,
    join_focus: FocusHandle,
    dismiss_notice_focus: FocusHandle,
    dismiss_favorite_focus: FocusHandle,
    details_focus: FocusHandle,
    empty_browse_focus: FocusHandle,
    empty_message_focus: FocusHandle,
    /// Files are being dragged over the window (see `drag_over_pane` for which side).
    dragging_files: bool,
    drag_over_pane: bool,
    _subscriptions: Vec<Subscription>,
}

impl ShellView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| SidebarView::new(model.clone(), cx));
        let rail = cx.new(|cx| RailView::new(model.clone(), window, cx));
        let timeline = cx.new(|cx| TimelineView::new(model.clone(), window, cx));
        let composer = cx.new(|cx| Composer::new(model.clone(), window, cx));
        let pane_layout = Rc::new(Cell::new(PaneLayout::default()));
        let thread_pane =
            cx.new(|cx| ThreadPane::new(model.clone(), pane_layout.clone(), window, cx));
        let search = cx.new(|cx| SearchResultsView::new(model.clone(), window, cx));
        let inbox = cx.new(|cx| ThreadInboxView::new(model.clone(), window, cx));
        let saved = cx.new(|cx| SavedMessagesView::new(model.clone(), window, cx));
        let people = cx.new(|cx| PeopleDirectoryView::new(model.clone(), window, cx));
        let thread_timeline = thread_pane.read(cx).timeline().clone();
        let thread_composer = thread_pane.read(cx).composer().clone();
        let mut subscriptions = Vec::new();
        // Re-render when the core changes so the header, banners, and empty states stay current.
        subscriptions.push(cx.observe_in(&model, window, |this, _, window, cx| {
            this.sync_picker(cx);
            // Choosing another conversation in the sidebar leaves the search.
            if let Some(opening) = this.search_opening {
                if this.opening_done(opening, cx) {
                    this.finish_opening(opening, window, cx);
                }
            } else {
                let core = this.model.read(cx).core();
                if core.search().is_some()
                    && core.selected_conversation() != this.search_selected_at_open
                {
                    this.close_search(false, window, cx);
                }
                let core = this.model.read(cx).core();
                if core.inbox().is_some()
                    && core.selected_conversation() != this.inbox_selected_at_open
                {
                    this.close_inbox(false, window, cx);
                }
                let core = this.model.read(cx).core();
                if let Some(message) = this.saved_opening {
                    if !core.opening_link() {
                        this.finish_saved_opening(message, window, cx);
                    }
                } else if core.saved().open
                    && core.selected_conversation() != this.saved_selected_at_open
                {
                    this.close_saved(false, window, cx);
                }
                let core = this.model.read(cx).core();
                if core.people().open
                    && core.selected_conversation() != this.people_selected_at_open
                {
                    this.close_people(false, window, cx);
                }
            }
            cx.notify();
        }));
        // A conversation someone asked for has opened (or could not be): the keyboard goes to
        // its composer, once.
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, event: &AppEvent, window, cx| {
                let AppEvent::Changed(changes) = event;
                if this.awaiting_conversation
                    && changes
                        .iter()
                        .any(|change| matches!(change, Change::ConversationOpened(_)))
                {
                    this.awaiting_conversation = false;
                    this.composer
                        .update(cx, |composer, cx| composer.focus_input(window, cx));
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &saved,
            window,
            |this, _saved, event: &SavedEvent, window, cx| match event {
                SavedEvent::Close => this.close_saved(true, window, cx),
                SavedEvent::Open {
                    conversation,
                    message,
                } => this.open_saved_message(*conversation, *message, window, cx),
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &people,
            window,
            |this, _people, event: &PeopleEvent, window, cx| match event {
                PeopleEvent::Close => this.close_people(true, window, cx),
                PeopleEvent::OpenProfile(user) => this.open_profile(*user, window, cx),
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &rail,
            window,
            |this, _rail, event: &RailEvent, window, cx| match event {
                RailEvent::Add => this.run_command(CommandId::AddWorkspace, window, cx),
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &inbox,
            window,
            |this, _inbox, event: &InboxEvent, window, cx| match event {
                InboxEvent::Close => this.close_inbox(true, window, cx),
                InboxEvent::OpenThread { conversation, root } => {
                    this.open_thread(*conversation, *root, window, cx);
                    // Opening a thread selects its conversation behind the pane; that is the
                    // inbox's own doing, so the inbox stays (otherwise it would close, as it
                    // does when another conversation is chosen in the sidebar).
                    this.inbox_selected_at_open =
                        this.model.read(cx).core().selected_conversation();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &search,
            window,
            |this, _search, event: &SearchEvent, window, cx| match event {
                SearchEvent::Close => this.close_search(true, window, cx),
                SearchEvent::Open { index, reply } => {
                    this.open_search_hit(*index, *reply, window, cx)
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &sidebar,
            window,
            // Every button asks the command registry, as the shortcuts and the menus do.
            |this, _sidebar, event: &SidebarEvent, window, cx| {
                let id = match event {
                    SidebarEvent::CreateChannel => CommandId::CreateChannel,
                    SidebarEvent::BrowseChannels => CommandId::BrowseChannels,
                    SidebarEvent::NewMessage => CommandId::NewMessage,
                    SidebarEvent::Search => CommandId::Search,
                    SidebarEvent::Inbox => CommandId::Threads,
                    SidebarEvent::Saved => CommandId::Saved,
                    SidebarEvent::People => CommandId::People,
                    SidebarEvent::QuickSwitcher => CommandId::QuickSwitcher,
                    SidebarEvent::Preferences => CommandId::Preferences,
                    SidebarEvent::Menu => return this.open_command_menu(window, cx),
                    SidebarEvent::SignOut => CommandId::SignOut,
                };
                this.run_command(id, window, cx);
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &timeline,
            window,
            |this, _timeline, event: &TimelineEvent, window, cx| {
                match event {
                    TimelineEvent::ConfirmDelete {
                        conversation,
                        message,
                    } => this.open_delete_message(*conversation, *message, window, cx),
                    TimelineEvent::FocusComposer => this
                        .composer
                        .update(cx, |composer, cx| composer.focus_input(window, cx)),
                    TimelineEvent::PickReaction {
                        conversation,
                        message,
                        anchor,
                    } => this.open_picker(
                        PickerPurpose::Reaction {
                            conversation: *conversation,
                            message: *message,
                        },
                        *anchor,
                        window,
                        cx,
                    ),
                    TimelineEvent::OpenThread { conversation, root } => {
                        this.open_thread(*conversation, *root, window, cx)
                    }
                    TimelineEvent::SelectionStarted => {
                        this.clear_other_selections(SelectionPlace::Timeline, cx)
                    }
                    TimelineEvent::OpenViewer { images, index } => {
                        this.open_viewer(images.clone(), *index, window, cx)
                    }
                    TimelineEvent::OpenProfile { user } => this.open_profile(*user, window, cx),
                }
            },
        ));
        // The pane's replies ask for the same things the conversation's messages do.
        subscriptions.push(cx.subscribe_in(
            &thread_timeline,
            window,
            |this, _timeline, event: &TimelineEvent, window, cx| match event {
                TimelineEvent::ConfirmDelete {
                    conversation,
                    message,
                } => this.open_delete_message(*conversation, *message, window, cx),
                TimelineEvent::FocusComposer => this.thread_pane.update(cx, |pane, cx| {
                    pane.composer()
                        .update(cx, |composer, cx| composer.focus_input(window, cx))
                }),
                TimelineEvent::PickReaction {
                    conversation,
                    message,
                    anchor,
                } => this.open_picker(
                    PickerPurpose::Reaction {
                        conversation: *conversation,
                        message: *message,
                    },
                    *anchor,
                    window,
                    cx,
                ),
                // A reply cannot be replied to: the pane never asks.
                TimelineEvent::OpenThread { .. } => {}
                TimelineEvent::SelectionStarted => {
                    this.clear_other_selections(SelectionPlace::Pane, cx)
                }
                TimelineEvent::OpenViewer { images, index } => {
                    this.open_viewer(images.clone(), *index, window, cx)
                }
                TimelineEvent::OpenProfile { user } => this.open_profile(*user, window, cx),
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &thread_composer,
            window,
            |this, _composer, event: &ComposerEvent, window, cx| match event {
                ComposerEvent::EditPrevious => {
                    this.thread_pane.update(cx, |pane, cx| {
                        pane.timeline()
                            .update(cx, |timeline, cx| timeline.edit_latest_message(window, cx))
                    });
                }
                ComposerEvent::PickEmoji { anchor } => {
                    this.open_picker(PickerPurpose::ThreadComposer, *anchor, window, cx)
                }
                ComposerEvent::SelectionStarted => {
                    this.clear_other_selections(SelectionPlace::PaneComposer, cx)
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &thread_pane,
            window,
            |this, _pane, event: &ThreadPaneEvent, window, cx| match event {
                ThreadPaneEvent::Close => this.close_thread(window, cx),
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &composer,
            window,
            |this, _composer, event: &ComposerEvent, window, cx| match event {
                ComposerEvent::EditPrevious => {
                    this.timeline
                        .update(cx, |timeline, cx| timeline.edit_latest_message(window, cx));
                }
                ComposerEvent::PickEmoji { anchor } => {
                    this.open_picker(PickerPurpose::Composer, *anchor, window, cx)
                }
                ComposerEvent::SelectionStarted => {
                    this.clear_other_selections(SelectionPlace::Composer, cx)
                }
            },
        ));
        // Unread messages are marked read only while the window is active.
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            this.model.update(cx, |model, cx| {
                model.dispatch(Input::WindowActive(active), cx)
            });
        }));
        let active = window.is_window_active();
        model.update(cx, |model, cx| {
            model.dispatch(Input::WindowActive(active), cx)
        });
        Self {
            model,
            rail,
            sidebar,
            timeline,
            composer,
            thread_pane,
            search,
            search_return_focus: None,
            search_selected_at_open: None,
            search_opening: None,
            inbox,
            inbox_return_focus: None,
            inbox_selected_at_open: None,
            saved,
            saved_return_focus: None,
            saved_selected_at_open: None,
            saved_opening: None,
            people,
            people_return_focus: None,
            people_selected_at_open: None,
            awaiting_conversation: false,
            bell_focus: cx.focus_handle().tab_stop(true),
            bell_menu: None,
            command_menu: None,
            pane_layout,
            thread_return_focus: None,
            overlay: None,
            picker: None,
            selected_at_open: None,
            star_focus: cx.focus_handle().tab_stop(true),
            join_focus: cx.focus_handle().tab_stop(true),
            dismiss_notice_focus: cx.focus_handle().tab_stop(true),
            dismiss_favorite_focus: cx.focus_handle().tab_stop(true),
            details_focus: cx.focus_handle().tab_stop(true),
            empty_browse_focus: cx.focus_handle().tab_stop(true),
            empty_message_focus: cx.focus_handle().tab_stop(true),
            dragging_files: false,
            drag_over_pane: false,
            _subscriptions: subscriptions,
        }
    }

    /// The quick switcher, while it is the dialog showing. For tests.
    pub fn quick_switcher(&self) -> Option<Entity<QuickSwitcherDialog>> {
        self.overlay
            .as_ref()?
            .view
            .clone()
            .downcast::<QuickSwitcherDialog>()
            .ok()
    }

    /// The channel directory, while it is the dialog showing. For tests.
    pub fn browse_dialog(&self) -> Option<Entity<BrowseChannelsDialog>> {
        self.overlay
            .as_ref()?
            .view
            .clone()
            .downcast::<BrowseChannelsDialog>()
            .ok()
    }

    /// Preferences, while it is the dialog showing. For tests.
    pub fn preferences_dialog(&self) -> Option<Entity<PreferencesDialog>> {
        self.overlay
            .as_ref()?
            .view
            .clone()
            .downcast::<PreferencesDialog>()
            .ok()
    }

    /// A profile, while it is the dialog showing. For tests.
    pub fn profile_dialog(&self) -> Option<Entity<ProfileDialog>> {
        self.overlay
            .as_ref()?
            .view
            .clone()
            .downcast::<ProfileDialog>()
            .ok()
    }

    /// The custom emoji dialog, while it is the dialog showing. For tests.
    pub fn custom_emoji_dialog(&self) -> Option<Entity<CustomEmojiDialog>> {
        self.overlay
            .as_ref()?
            .view
            .clone()
            .downcast::<CustomEmojiDialog>()
            .ok()
    }

    pub fn emoji_picker(&self) -> Option<&Entity<EmojiPicker>> {
        self.picker.as_ref().map(|open| &open.view)
    }

    /// The focus handle of the notice banner's Dismiss button, for tests of the keyboard.
    pub fn notice_dismiss_focus(&self) -> FocusHandle {
        self.dismiss_notice_focus.clone()
    }

    /// The composer, for hosts that drive it (and for tests).
    pub fn composer(&self) -> &Entity<Composer> {
        &self.composer
    }

    /// The thread pane, for hosts and tests that need it.
    pub fn thread_pane(&self) -> &Entity<ThreadPane> {
        &self.thread_pane
    }

    // ----- Selection -----

    /// Text was just selected in `keep`: the selections everywhere else end, so there is only ever
    /// one (and Ctrl+C or the next typing can only mean that one).
    fn clear_other_selections(&mut self, keep: SelectionPlace, cx: &mut Context<Self>) {
        if keep != SelectionPlace::Timeline {
            self.timeline
                .update(cx, |timeline, cx| timeline.clear_selection(cx));
        }
        if keep != SelectionPlace::Pane {
            let pane_timeline = self.thread_pane.read(cx).timeline().clone();
            pane_timeline.update(cx, |timeline, cx| timeline.clear_selection(cx));
        }
        if keep != SelectionPlace::Composer {
            let input = self.composer.read(cx).input().clone();
            input.update(cx, |input, cx| input.collapse_selection(cx));
        }
        if keep != SelectionPlace::PaneComposer {
            let composer = self.thread_pane.read(cx).composer().clone();
            let input = composer.read(cx).input().clone();
            input.update(cx, |input, cx| input.collapse_selection(cx));
        }
    }

    // ----- Search -----

    /// The search results, for tests.
    pub fn search_view(&self) -> &Entity<SearchResultsView> {
        &self.search
    }

    /// Opens search (Ctrl+F, or the sidebar's Search) and puts the keyboard in its field; with
    /// search already open, only moves the keyboard there. Does nothing while a dialog is up.
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay.is_some() {
            return;
        }
        if self.model.read(cx).core().search().is_some() {
            self.search
                .update(cx, |search, cx| search.focus_field(window, cx));
            return;
        }
        self.close_inbox(false, window, cx);
        self.close_saved(false, window, cx);
        self.close_people(false, window, cx);
        self.search_return_focus = window.focused(cx);
        self.search_selected_at_open = self.model.read(cx).core().selected_conversation();
        self.picker = None;
        // A thread pane that would hide the results (a narrow window) gives way.
        if self.pane_layout.get().replaces_conversation
            && self.model.read(cx).core().open_thread().is_some()
        {
            self.model
                .update(cx, |model, cx| model.dispatch(Input::CloseThread, cx));
        }
        self.model
            .update(cx, |model, cx| model.dispatch(Input::OpenSearch, cx));
        self.search
            .update(cx, |search, cx| search.reset(window, cx));
        cx.notify();
    }

    /// Closes search. With `restore`, the keyboard goes back to where it was.
    fn close_search(&mut self, restore: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.read(cx).core().search().is_none() {
            return;
        }
        self.search_opening = None;
        self.model
            .update(cx, |model, cx| model.dispatch(Input::CloseSearch, cx));
        let previous = self.search_return_focus.take();
        if restore {
            match previous {
                Some(previous) => window.focus(&previous, cx),
                None => self
                    .composer
                    .update(cx, |composer, cx| composer.focus_input(window, cx)),
            }
        }
        cx.notify();
    }

    /// Opens result `index`: the core selects its conversation and scrolls to the message (or opens
    /// the thread, for a reply, or offers to join a channel the person is not in). The search
    /// stays up, saying it is looking, until that has happened (the message may have to be
    /// fetched), then closes and the keyboard goes to what was opened.
    fn open_search_hit(
        &mut self,
        index: usize,
        reply: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_root) = self
            .model
            .read(cx)
            .core()
            .search()
            .and_then(|view| view.hits.get(index))
            .map(|hit| hit.message.thread_root)
        else {
            return;
        };
        let opening = OpeningHit {
            thread_root: thread_root.filter(|_| reply),
        };
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::OpenSearchHit(index), cx)
        });
        if self.opening_done(opening, cx) {
            self.finish_opening(opening, window, cx);
        } else {
            self.search_opening = Some(opening);
            self.search
                .update(cx, |search, cx| search.set_opening(true, cx));
        }
        cx.notify();
    }

    /// Whether the core has finished looking for the chosen result: it fetched the message and
    /// paged to it (the conversation or thread shows it), or stopped with a notice or an offer to
    /// join.
    fn opening_done(&self, _opening: OpeningHit, cx: &gpui::App) -> bool {
        !self.model.read(cx).core().opening_link()
    }

    fn finish_opening(&mut self, opening: OpeningHit, window: &mut Window, cx: &mut Context<Self>) {
        self.search_opening = None;
        self.search
            .update(cx, |search, cx| search.set_opening(false, cx));
        self.close_search(false, window, cx);
        let in_thread =
            opening.thread_root.is_some() && self.model.read(cx).core().open_thread().is_some();
        let focus = if in_thread {
            self.thread_pane
                .read(cx)
                .timeline()
                .read(cx)
                .focus_handle(cx)
        } else {
            self.timeline.read(cx).focus_handle(cx)
        };
        window.focus(&focus, cx);
    }

    // ----- The thread inbox -----

    /// The thread inbox, for tests.
    pub fn inbox_view(&self) -> &Entity<ThreadInboxView> {
        &self.inbox
    }

    /// Opens the thread inbox in place of the conversation, with the keyboard in its list.
    pub fn open_inbox(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay.is_some() {
            return;
        }
        if self.model.read(cx).core().inbox().is_some() {
            self.inbox.update(cx, |inbox, cx| inbox.reset(window, cx));
            return;
        }
        // Search, the inbox, the saved messages and the people share the conversation's place.
        self.close_search(false, window, cx);
        self.close_saved(false, window, cx);
        self.close_people(false, window, cx);
        self.inbox_return_focus = window.focused(cx);
        self.inbox_selected_at_open = self.model.read(cx).core().selected_conversation();
        self.picker = None;
        self.model
            .update(cx, |model, cx| model.dispatch(Input::OpenInbox, cx));
        self.inbox.update(cx, |inbox, cx| inbox.reset(window, cx));
        cx.notify();
    }

    /// Closes the inbox. With `restore`, the keyboard goes back to where it was.
    fn close_inbox(&mut self, restore: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.read(cx).core().inbox().is_none() {
            return;
        }
        self.model
            .update(cx, |model, cx| model.dispatch(Input::CloseInbox, cx));
        let previous = self.inbox_return_focus.take();
        if restore {
            match previous {
                Some(previous) => window.focus(&previous, cx),
                None => self
                    .composer
                    .update(cx, |composer, cx| composer.focus_input(window, cx)),
            }
        }
        cx.notify();
    }

    // ----- Saved messages -----

    /// The saved messages, for tests.
    pub fn saved_view(&self) -> &Entity<SavedMessagesView> {
        &self.saved
    }

    /// Opens the saved messages in place of the conversation, with the keyboard in their list.
    pub fn open_saved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay.is_some() {
            return;
        }
        if self.model.read(cx).core().saved().open {
            self.saved.update(cx, |saved, cx| saved.reset(window, cx));
            return;
        }
        self.close_search(false, window, cx);
        self.close_inbox(false, window, cx);
        self.close_people(false, window, cx);
        self.saved_return_focus = window.focused(cx);
        self.saved_selected_at_open = self.model.read(cx).core().selected_conversation();
        self.picker = None;
        self.model
            .update(cx, |model, cx| model.dispatch(Input::OpenSaved, cx));
        self.saved.update(cx, |saved, cx| saved.reset(window, cx));
        cx.notify();
    }

    /// Closes the saved messages. With `restore`, the keyboard goes back to where it was.
    fn close_saved(&mut self, restore: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).core().saved().open {
            return;
        }
        self.saved_opening = None;
        self.saved
            .update(cx, |saved, cx| saved.set_opening(false, cx));
        self.model
            .update(cx, |model, cx| model.dispatch(Input::CloseSaved, cx));
        let previous = self.saved_return_focus.take();
        if restore {
            match previous {
                Some(previous) => window.focus(&previous, cx),
                None => self
                    .composer
                    .update(cx, |composer, cx| composer.focus_input(window, cx)),
            }
        }
        cx.notify();
    }

    /// Goes to a saved message in its conversation (a reply opens in its thread). The view stays
    /// up, saying it is looking, until the core has found it (the message may have to be
    /// fetched).
    fn open_saved_message(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace_id) = self
            .model
            .read(cx)
            .core()
            .primary_workspace()
            .map(|info| info.workspace.id)
        else {
            return;
        };
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::OpenPermalink(Permalink {
                    workspace_id,
                    conversation_id: conversation,
                    message_id: message,
                }),
                cx,
            )
        });
        if self.model.read(cx).core().opening_link() {
            self.saved_opening = Some(message);
            self.saved
                .update(cx, |saved, cx| saved.set_opening(true, cx));
        } else {
            self.finish_saved_opening(message, window, cx);
        }
        cx.notify();
    }

    /// The core has found the chosen saved message (or stopped with a notice): the view closes
    /// and the keyboard goes to what was opened.
    fn finish_saved_opening(
        &mut self,
        message: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.model.read(cx).core();
        let reply = core
            .saved()
            .items
            .iter()
            .any(|item| item.message.id == message && item.message.thread_root.is_some());
        let in_thread = reply && core.open_thread().is_some();
        self.close_saved(false, window, cx);
        let focus = if in_thread {
            self.thread_pane
                .read(cx)
                .timeline()
                .read(cx)
                .focus_handle(cx)
        } else {
            self.timeline.read(cx).focus_handle(cx)
        };
        window.focus(&focus, cx);
    }

    // ----- The people directory and profiles -----

    /// The people directory, for tests.
    pub fn people_view(&self) -> &Entity<PeopleDirectoryView> {
        &self.people
    }

    /// Opens the people directory in place of the conversation, with the keyboard in its search
    /// box.
    pub fn open_people_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay.is_some() {
            return;
        }
        if self.model.read(cx).core().people().open {
            self.people
                .update(cx, |people, cx| people.reset(window, cx));
            return;
        }
        self.close_search(false, window, cx);
        self.close_inbox(false, window, cx);
        self.close_saved(false, window, cx);
        self.people_return_focus = window.focused(cx);
        self.people_selected_at_open = self.model.read(cx).core().selected_conversation();
        self.picker = None;
        self.model
            .update(cx, |model, cx| model.dispatch(Input::OpenPeople, cx));
        self.people
            .update(cx, |people, cx| people.reset(window, cx));
        cx.notify();
    }

    /// Closes the people directory. With `restore`, the keyboard goes back to where it was.
    fn close_people(&mut self, restore: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).core().people().open {
            return;
        }
        self.model
            .update(cx, |model, cx| model.dispatch(Input::ClosePeople, cx));
        let previous = self.people_return_focus.take();
        if restore {
            match previous {
                Some(previous) => window.focus(&previous, cx),
                None => self
                    .composer
                    .update(cx, |composer, cx| composer.focus_input(window, cx)),
            }
        }
        cx.notify();
    }

    /// Shows a person's profile above everything. "Edit status", on one's own, opens Preferences.
    pub fn open_profile(&mut self, user: UserId, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let dialog = cx.new(|cx| ProfileDialog::new(model, user, window, cx));
        let request = cx.subscribe_in(
            &dialog,
            window,
            |this, _dialog, request: &ProfileRequest, window, cx| match request {
                ProfileRequest::EditStatus => this.open_preferences(window, cx),
            },
        );
        self.show(dialog, vec![request], window, cx);
    }

    // ----- Workspaces -----

    /// The workspace rail, for tests.
    pub fn rail(&self) -> &Entity<RailView> {
        &self.rail
    }

    /// Opens "Add a workspace".
    pub fn open_add_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay.is_some() {
            return;
        }
        let model = self.model.clone();
        let dialog = cx.new(|cx| AddWorkspaceDialog::new(model, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    /// "Add a workspace", while it is the dialog showing. For tests.
    pub fn add_workspace_dialog(&self) -> Option<Entity<AddWorkspaceDialog>> {
        self.overlay
            .as_ref()?
            .view
            .clone()
            .downcast::<AddWorkspaceDialog>()
            .ok()
    }

    // ----- Commands (D24) -----

    /// What is open over the shell, as the command rules see it: the dialog's own command, any
    /// other dialog, picker or menu, or nothing.
    fn dialog_state(&self, cx: &gpui::App) -> DialogState {
        let Some(overlay) = self.overlay.as_ref() else {
            return if self.picker.is_some() || self.command_menu.is_some() {
                DialogState::Other
            } else {
                DialogState::Closed
            };
        };
        macro_rules! own {
            ($($ty:ty => $command:expr),* $(,)?) => {
                $(if overlay.view.clone().downcast::<$ty>().is_ok() {
                    return DialogState::Own($command);
                })*
            };
        }
        own!(
            QuickSwitcherDialog => CommandId::QuickSwitcher,
            PreferencesDialog => CommandId::Preferences,
            WorkspaceSettingsDialog => CommandId::WorkspaceSettings,
            InvitationsDialog => CommandId::InvitePeople,
            GroupsDialog => CommandId::UserGroups,
            CustomEmojiDialog => CommandId::CustomEmoji,
            BrowseChannelsDialog => CommandId::BrowseChannels,
            CreateChannelDialog => CommandId::CreateChannel,
            AddWorkspaceDialog => CommandId::AddWorkspace,
            ShortcutsDialog => CommandId::Shortcuts,
            DiagnosticsDialog => CommandId::Diagnostics,
            DetailsDialog => CommandId::ConversationDetails,
        );
        let _ = cx;
        DialogState::Other
    }

    /// Everything the command rules read, now.
    pub fn snapshot(&self, cx: &gpui::App) -> commands::AppSnapshot {
        let mut snapshot = commands::AppSnapshot::from_core(self.model.read(cx).core());
        snapshot.dialog = self.dialog_state(cx);
        snapshot.composing = crate::text_input::any_input_composing(cx);
        snapshot
    }

    /// Runs a command if its rule allows it now: the one door for shortcuts, buttons, menu items,
    /// and the palette. A dialog's own command closes the dialog (as Escape does).
    pub fn run_command(&mut self, id: CommandId, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.snapshot(cx);
        if commands::availability(id, &snapshot) != Availability::Enabled {
            return;
        }
        if snapshot.dialog == DialogState::Own(id) {
            window.dispatch_action(DialogDismiss.boxed_clone(), cx);
            return;
        }
        let selected = self.model.read(cx).core().selected_conversation();
        match id {
            CommandId::QuickSwitcher => self.open_quick_switcher(window, cx),
            CommandId::CommandPalette => self.open_quick_switcher_with(">", window, cx),
            CommandId::Search => self.open_search(window, cx),
            CommandId::Threads => self.open_inbox(window, cx),
            CommandId::Saved => self.open_saved(window, cx),
            CommandId::People => self.open_people_directory(window, cx),
            CommandId::BrowseChannels => self.open_browse(None, window, cx),
            CommandId::NewMessage => self.open_new_message(window, cx),
            CommandId::CreateChannel => self.open_create_channel(window, cx),
            CommandId::PreviousConversation => self.step_conversation(false, false, window, cx),
            CommandId::NextConversation => self.step_conversation(true, false, window, cx),
            CommandId::PreviousUnread => self.step_conversation(false, true, window, cx),
            CommandId::NextUnread => self.step_conversation(true, true, window, cx),
            CommandId::SwitchWorkspace(n) => self.switch_to_nth(usize::from(n) - 1, cx),
            CommandId::AddWorkspace => self.open_add_workspace(window, cx),
            CommandId::Preferences => self.open_preferences(window, cx),
            CommandId::WorkspaceSettings => self.open_workspace_settings(window, cx),
            CommandId::InvitePeople => self.open_invitations(window, cx),
            CommandId::UserGroups => self.open_groups(window, cx),
            CommandId::CustomEmoji => self.open_custom_emoji(window, cx),
            CommandId::ConversationDetails => {
                if let Some(conversation) = selected {
                    self.open_details(conversation, window, cx);
                }
            }
            CommandId::ToggleFavorite => {
                if let Some(conversation) = selected {
                    let favorite = self
                        .model
                        .read(cx)
                        .core()
                        .user_state(conversation)
                        .is_some_and(|state| state.favorite);
                    self.model.update(cx, |model, cx| {
                        model.dispatch(
                            Input::SetFavorite {
                                id: conversation,
                                favorite: !favorite,
                            },
                            cx,
                        )
                    });
                }
            }
            CommandId::Shortcuts => self.open_shortcuts(window, cx),
            CommandId::Diagnostics => self.open_diagnostics(window, cx),
            CommandId::SignOut => self
                .model
                .update(cx, |model, cx| model.dispatch(Input::SignOut, cx)),
        }
    }

    /// Goes to the conversation after (or before) the open one in the sidebar's order, wrapping
    /// round; with `unread_only`, to the next that has unread messages.
    fn step_conversation(
        &mut self,
        forwards: bool,
        unread_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (order, current, unread): (Vec<ConversationId>, Option<ConversationId>, Vec<bool>) = {
            let core = self.model.read(cx).core();
            let sidebar = core.sidebar();
            let order: Vec<ConversationId> = sidebar
                .favorites
                .iter()
                .chain(&sidebar.channels)
                .chain(&sidebar.direct_and_group)
                .copied()
                .collect();
            let unread = order
                .iter()
                .map(|id| {
                    core.user_state(*id)
                        .is_some_and(|state| state.unread_count > 0 || state.mention_count > 0)
                })
                .collect();
            (order, core.selected_conversation(), unread)
        };
        let count = order.len();
        if count == 0 {
            return;
        }
        // Before the first when nothing in the list is open, so "next" is the first.
        let at = current.and_then(|id| order.iter().position(|other| *other == id));
        let mut index = at.unwrap_or(if forwards { count - 1 } else { 0 });
        for _ in 0..count {
            index = if forwards {
                (index + 1) % count
            } else {
                (index + count - 1) % count
            };
            if Some(index) == at || (unread_only && !unread[index]) {
                continue;
            }
            let target = order[index];
            self.close_main_views(window, cx);
            self.model.update(cx, |model, cx| {
                model.dispatch(Input::SelectConversation(target), cx)
            });
            self.composer
                .update(cx, |composer, cx| composer.focus_input(window, cx));
            return;
        }
    }

    /// Opens the command menu: the groups of the registry as tabs, with what cannot run dimmed and
    /// saying why.
    pub fn open_command_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay.is_some() || self.picker.is_some() || self.command_menu.is_some() {
            return;
        }
        let snapshot = self.snapshot(cx);
        let tabs: Vec<(gpui::SharedString, Vec<MenuItem>)> = Group::ALL
            .iter()
            .filter_map(|group| {
                let items: Vec<MenuItem> = commands::group_rows(*group, &snapshot)
                    .into_iter()
                    .map(|row| {
                        let item = MenuItem::new(format!("{:?}", row.id), row.title);
                        match row.availability {
                            Availability::Disabled(reason) => item.enabled(false).detail(reason),
                            _ => match row.shortcut {
                                Some(shortcut) => item.shortcut(shortcut),
                                None => item,
                            },
                        }
                    })
                    .collect();
                (!items.is_empty()).then(|| (group.title().into(), items))
            })
            .collect();
        let menu = cx.new(|cx| Menu::tabbed("command-menu", tabs, cx));
        cx.subscribe_in(
            &menu,
            window,
            |this, _menu, event: &MenuEvent, window, cx| {
                this.command_menu = None;
                cx.notify();
                if let MenuEvent::Select(key) = event
                    && let Some(command) = commands::all()
                        .iter()
                        .find(|command| format!("{:?}", command.id) == key.as_ref())
                {
                    let id = command.id;
                    // The menu has closed and given the keyboard back: run it now.
                    cx.defer_in(window, move |this, window, cx| {
                        this.run_command(id, window, cx)
                    });
                }
            },
        )
        .detach();
        menu.update(cx, |menu, cx| menu.open(window, cx));
        self.command_menu = Some(menu);
        cx.notify();
    }

    /// The open command menu, for tests.
    pub fn command_menu(&self) -> Option<&Entity<Menu>> {
        self.command_menu.as_ref()
    }

    /// Whether a dialog is showing. For tests.
    pub fn has_overlay(&self) -> bool {
        self.overlay.is_some()
    }

    /// Whether the emoji picker is showing. For tests.
    pub fn has_picker(&self) -> bool {
        self.picker.is_some()
    }

    /// The keyboard shortcuts dialog.
    fn open_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = cx.new(|cx| ShortcutsDialog::new(window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    /// Help, Diagnostics…
    fn open_diagnostics(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let dialog = cx.new(|cx| DiagnosticsDialog::new(model, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    /// The shortcuts dialog, while it is the dialog showing. For tests.
    pub fn shortcuts_dialog(&self) -> Option<Entity<ShortcutsDialog>> {
        self.overlay
            .as_ref()?
            .view
            .clone()
            .downcast::<ShortcutsDialog>()
            .ok()
    }

    /// The diagnostics dialog, while it is the dialog showing. For tests.
    pub fn diagnostics_dialog(&self) -> Option<Entity<DiagnosticsDialog>> {
        self.overlay
            .as_ref()?
            .view
            .clone()
            .downcast::<DiagnosticsDialog>()
            .ok()
    }

    /// Ctrl+`n`: switches to the `index`-th workspace on the rail (0-based). Nothing happens over
    /// a dialog, or when there is no such workspace.
    fn switch_to_nth(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.overlay.is_some() {
            return;
        }
        let rail = self.model.read(cx).core().workspace_rail();
        if let Some(item) = rail.get(index) {
            let id = item.id;
            self.model.update(cx, |model, cx| {
                model.dispatch(Input::SwitchWorkspace(id), cx)
            });
        }
    }

    // ----- Workspace administration -----

    /// Opens the workspace settings. Only admins and owners have them; the sidebar hides the
    /// link from everyone else.
    fn open_workspace_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).core().can_manage_workspace() {
            return;
        }
        let model = self.model.clone();
        let dialog = cx.new(|cx| WorkspaceSettingsDialog::new(model, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    /// Opens the invitations (admins and owners).
    fn open_invitations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.read(cx).core().can_manage_workspace() {
            return;
        }
        let model = self.model.clone();
        let dialog = cx.new(|cx| InvitationsDialog::new(model, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    /// The workspace settings, while it is the dialog showing. For tests.
    pub fn workspace_settings_dialog(&self) -> Option<Entity<WorkspaceSettingsDialog>> {
        self.overlay
            .as_ref()?
            .view
            .clone()
            .downcast::<WorkspaceSettingsDialog>()
            .ok()
    }

    /// The invitations, while they are the dialog showing. For tests.
    pub fn invitations_dialog(&self) -> Option<Entity<InvitationsDialog>> {
        self.overlay
            .as_ref()?
            .view
            .clone()
            .downcast::<InvitationsDialog>()
            .ok()
    }

    // ----- The quick switcher -----

    /// Opens the quick switcher (Ctrl+K). Ctrl+K inside it closes it, so this only opens; it
    /// does nothing while another dialog is up.
    pub fn open_quick_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_quick_switcher_with("", window, cx);
    }

    /// Opens the quick switcher with `query` already typed (`>` lists commands).
    pub fn open_quick_switcher_with(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlay.is_some() {
            return;
        }
        let model = self.model.clone();
        let query = query.to_owned();
        let dialog = cx.new(|cx| QuickSwitcherDialog::with_query(model, &query, window, cx));
        let run = cx.subscribe_in(
            &dialog,
            window,
            |_this, _dialog, run: &QuickSwitcherRun, window, cx| {
                let id = run.0;
                // The switcher is still closing: run the command once it has.
                cx.defer_in(window, move |this, window, cx| {
                    this.run_command(id, window, cx)
                });
            },
        );
        let choice = cx.subscribe_in(
            &dialog,
            window,
            |_this, _dialog, choice: &QuickSwitcherChoice, window, cx| {
                let target = choice.0.clone();
                // The switcher is still closing: act once it has.
                cx.defer_in(window, move |this, window, cx| {
                    this.go_to(target, window, cx)
                });
            },
        );
        self.show(dialog, vec![choice, run], window, cx);
    }

    /// Ends whichever view stands in for the conversation, so the conversation shows.
    fn close_main_views(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_search(false, window, cx);
        self.close_inbox(false, window, cx);
        self.close_saved(false, window, cx);
        self.close_people(false, window, cx);
    }

    /// Goes where a quick switcher result says.
    fn go_to(&mut self, target: QuickSwitcherTarget, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            QuickSwitcherTarget::Conversation(_) => {
                self.close_main_views(window, cx);
                self.model.update(cx, |model, cx| {
                    model.dispatch(Input::ChooseQuickSwitcher(target), cx)
                });
                self.composer
                    .update(cx, |composer, cx| composer.focus_input(window, cx));
            }
            QuickSwitcherTarget::Person(_) => {
                self.close_main_views(window, cx);
                self.awaiting_conversation = true;
                self.model.update(cx, |model, cx| {
                    model.dispatch(Input::ChooseQuickSwitcher(target), cx)
                });
            }
            QuickSwitcherTarget::Threads => self.open_inbox(window, cx),
            QuickSwitcherTarget::Saved => self.open_saved(window, cx),
            QuickSwitcherTarget::Search => self.open_search(window, cx),
            QuickSwitcherTarget::BrowseChannels { ref query } => {
                let query = query.clone();
                self.model.update(cx, |model, cx| {
                    model.dispatch(Input::ChooseQuickSwitcher(target), cx)
                });
                self.open_browse(Some(query), window, cx);
            }
        }
        cx.notify();
    }

    // ----- Notification preferences -----

    fn open_preferences(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let dialog = cx.new(|cx| PreferencesDialog::new(model, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    /// Opens the bell's menu for `id`: how much of this conversation notifies, and mute.
    fn open_bell_menu(&mut self, id: ConversationId, window: &mut Window, cx: &mut Context<Self>) {
        let core = self.model.read(cx).core();
        let own = core
            .conversation_notification_prefs(id)
            .and_then(|prefs| prefs.level);
        let effective = core.effective_level(id);
        let muted = core.is_muted(id);
        let noun = if core
            .conversation(id)
            .is_some_and(|c| c.kind == ConversationKind::Channel)
        {
            "channel"
        } else {
            "conversation"
        };
        let mut items = Vec::new();
        for (level, label) in [
            (NotifyLevel::All, "All messages"),
            (NotifyLevel::Mentions, "Mentions only"),
            (NotifyLevel::Nothing, "Nothing"),
        ] {
            let chosen = effective == level;
            let mut item = MenuItem::new(
                format!("level-{level:?}"),
                format!("{}{label}", if chosen { "✓ " } else { "    " }),
            );
            if chosen && own.is_none() {
                item = item.detail("Following your default for this kind of conversation");
            }
            items.push(item);
        }
        items.push(MenuItem::new(
            "mute",
            if muted {
                format!("Unmute {noun}")
            } else {
                format!("Mute {noun}")
            },
        ));
        let menu = cx.new(|cx| Menu::new("bell-menu", items, cx));
        cx.subscribe_in(
            &menu,
            window,
            move |this, _menu, event: &MenuEvent, _window, cx| {
                if let MenuEvent::Select(item) = event {
                    let (level, muted_now) = {
                        let core = this.model.read(cx).core();
                        (
                            core.conversation_notification_prefs(id)
                                .and_then(|prefs| prefs.level),
                            core.is_muted(id),
                        )
                    };
                    let input = match item.as_ref() {
                        "level-All" => Some((Some(NotifyLevel::All), muted_now)),
                        "level-Mentions" => Some((Some(NotifyLevel::Mentions), muted_now)),
                        "level-Nothing" => Some((Some(NotifyLevel::Nothing), muted_now)),
                        "mute" => Some((level, !muted_now)),
                        _ => None,
                    };
                    if let Some((level, muted)) = input {
                        this.model.update(cx, |model, cx| {
                            model.dispatch(
                                Input::SetConversationNotificationPrefs {
                                    conversation: id,
                                    level,
                                    muted,
                                },
                                cx,
                            )
                        });
                    }
                }
                this.bell_menu = None;
                cx.notify();
            },
        )
        .detach();
        menu.update(cx, |menu, cx| menu.open(window, cx));
        self.bell_menu = Some(menu);
        cx.notify();
    }

    // ----- Threads -----

    /// Opens the thread of `root` in the pane and puts the keyboard in its reply composer. The
    /// conversation behind it is not touched: its reading position stays where it was. What had
    /// the keyboard is remembered, and Escape in the pane gives it back.
    pub fn open_thread(
        &mut self,
        conversation: ConversationId,
        root: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(focused) = window.focused(cx) {
            self.thread_return_focus = Some(focused);
        }
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::OpenThread { conversation, root }, cx)
        });
        self.thread_pane.update(cx, |pane, cx| {
            pane.composer()
                .update(cx, |composer, cx| composer.focus_input(window, cx))
        });
        cx.notify();
    }

    /// Closes the pane and returns the keyboard to where the thread was opened from, or to the
    /// conversation's composer when that is gone.
    pub fn close_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.read(cx).core().open_thread().is_none() {
            return;
        }
        self.model
            .update(cx, |model, cx| model.dispatch(Input::CloseThread, cx));
        match self.thread_return_focus.take() {
            Some(previous) => window.focus(&previous, cx),
            None => self
                .composer
                .update(cx, |composer, cx| composer.focus_input(window, cx)),
        }
        cx.notify();
    }

    /// The sidebar, for hosts and tests that need its focus handles.
    pub fn sidebar(&self) -> &Entity<SidebarView> {
        &self.sidebar
    }

    /// The message timeline, for hosts that drive it (and for tests).
    pub fn timeline(&self) -> &Entity<TimelineView> {
        &self.timeline
    }

    // ----- The emoji picker -----

    /// Opens the picker as a popover near `anchor`, for `purpose`, replacing any open one.
    pub fn open_picker(
        &mut self,
        purpose: PickerPurpose,
        anchor: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The person's focus goes back there when the picker closes.
        let previous_focus = self
            .picker
            .as_ref()
            .and_then(|open| open.previous_focus.clone())
            .or_else(|| window.focused(cx));
        self.picker = None;
        let prefs = {
            let prefs = self.model.read(cx).core().emoji_prefs();
            PickerPrefs {
                skin_tone: prefs.skin_tone,
                recent: prefs.recent.clone(),
            }
        };
        let model = self.model.clone();
        let view = cx.new(|cx| EmojiPicker::new(prefs, model, window, cx));
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this, _picker, event: &EmojiPickerEvent, window, cx| match event {
                EmojiPickerEvent::Chosen(emoji) => this.picker_chose(*emoji, window, cx),
                EmojiPickerEvent::SkinToneChosen(tone) => {
                    let tone = *tone;
                    this.model
                        .update(cx, |model, cx| model.dispatch(Input::SetSkinTone(tone), cx));
                }
                EmojiPickerEvent::Dismiss => this.close_picker(window, cx),
                EmojiPickerEvent::AddCustomEmoji => {
                    this.close_picker(window, cx);
                    this.open_dialog(ShellDialog::CustomEmoji, window, cx);
                }
            },
        );
        self.picker = Some(PickerOverlay {
            view,
            purpose,
            anchor,
            previous_focus,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Closes the picker and puts focus back where it was (in the composer's input, for an emoji
    /// that was going there).
    pub fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(overlay) = self.picker.take() else {
            return;
        };
        match overlay.purpose {
            PickerPurpose::Composer => self
                .composer
                .update(cx, |composer, cx| composer.focus_input(window, cx)),
            PickerPurpose::ThreadComposer => self.thread_pane.update(cx, |pane, cx| {
                pane.composer()
                    .update(cx, |composer, cx| composer.focus_input(window, cx))
            }),
            PickerPurpose::Reaction { .. } => {
                if let Some(previous) = overlay.previous_focus {
                    window.focus(&previous, cx);
                }
            }
        }
        cx.notify();
    }

    /// The person chose an emoji: remember it, use it, and close the picker.
    fn picker_chose(&mut self, emoji: ReactionEmoji, window: &mut Window, cx: &mut Context<Self>) {
        let Some(purpose) = self.picker.as_ref().map(|open| open.purpose) else {
            return;
        };
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::RecordEmojiUse(emoji), cx)
        });
        if let PickerPurpose::Reaction {
            conversation,
            message,
        } = purpose
        {
            // Picking what the person already reacted with adds nothing: it never takes it back.
            let already = self
                .model
                .read(cx)
                .core()
                .reacted_by_me(conversation, message, emoji);
            if !already {
                self.model.update(cx, |model, cx| {
                    model.dispatch(
                        Input::ToggleReaction {
                            conversation,
                            message,
                            emoji,
                        },
                        cx,
                    );
                });
            }
        }
        // Close first so that focus is in place before the emoji is typed. A standard emoji is
        // typed as itself, a custom one as `:name:`.
        let typed = match emoji {
            ReactionEmoji::Unicode(emoji) => emoji.as_str().to_owned(),
            ReactionEmoji::Custom(id) => {
                crate::emoji_images::label(self.model.read(cx).core(), id, None)
            }
        };
        self.close_picker(window, cx);
        match purpose {
            PickerPurpose::Composer => self
                .composer
                .update(cx, |composer, cx| composer.insert_emoji(&typed, window, cx)),
            PickerPurpose::ThreadComposer => self.thread_pane.update(cx, |pane, cx| {
                pane.composer()
                    .update(cx, |composer, cx| composer.insert_emoji(&typed, window, cx))
            }),
            PickerPurpose::Reaction { .. } => {}
        }
    }

    /// Follows a skin tone saved from another place while the picker is open.
    fn sync_picker(&mut self, cx: &mut Context<Self>) {
        let tone = self.model.read(cx).core().emoji_prefs().skin_tone;
        if let Some(open) = &self.picker {
            open.view
                .update(cx, |picker, cx| picker.set_skin_tone(tone, cx));
        }
    }

    // ----- Dialogs -----

    /// Opens `dialog`, replacing any dialog that is already open.
    pub fn open_dialog(
        &mut self,
        dialog: ShellDialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match dialog {
            ShellDialog::CreateChannel => self.open_create_channel(window, cx),
            ShellDialog::BrowseChannels => self.open_browse(None, window, cx),
            ShellDialog::QuickSwitcher => self.open_quick_switcher(window, cx),
            ShellDialog::Profile(user) => self.open_profile(user, window, cx),
            ShellDialog::WorkspaceSettings => self.open_workspace_settings(window, cx),
            ShellDialog::Invitations => self.open_invitations(window, cx),
            ShellDialog::AddWorkspace => self.open_add_workspace(window, cx),
            ShellDialog::NewMessage => self.open_new_message(window, cx),
            ShellDialog::Groups => self.open_groups(window, cx),
            ShellDialog::CustomEmoji => self.open_custom_emoji(window, cx),
            ShellDialog::Preferences => self.open_preferences(window, cx),
            ShellDialog::Details(id) => self.open_details(id, window, cx),
            ShellDialog::AddPeople(id) => {
                self.open_people(PeopleMode::AddToChannel(id), window, cx)
            }
            ShellDialog::DeleteMessage {
                conversation,
                message,
            } => self.open_delete_message(conversation, message, window, cx),
            ShellDialog::Shortcuts => self.open_shortcuts(window, cx),
            ShellDialog::Diagnostics => self.open_diagnostics(window, cx),
        }
    }

    /// Shows `dialog` above everything, and closes it when it asks to be dismissed.
    fn show<D>(
        &mut self,
        dialog: Entity<D>,
        mut subscriptions: Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        D: Render + EventEmitter<DialogEvent> + 'static,
    {
        // A dialog replaces the picker.
        self.picker = None;
        self.selected_at_open = self.model.read(cx).core().selected_conversation();
        subscriptions.push(cx.subscribe_in(
            &dialog,
            window,
            |this, _dialog, _event: &DialogEvent, window, cx| this.close_overlay(window, cx),
        ));
        self.overlay = Some(Overlay {
            view: dialog.into(),
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = None;
        // A dialog that created, opened, or joined a conversation selected it: write there.
        if self.model.read(cx).core().selected_conversation() != self.selected_at_open {
            self.composer
                .update(cx, |composer, cx| composer.focus_input(window, cx));
        }
        cx.notify();
    }

    fn open_create_channel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let dialog = cx.new(|cx| CreateChannelDialog::new(model, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    /// Opens the channel directory, with `query` already searched for when it came from the quick
    /// switcher.
    fn open_browse(&mut self, query: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let dialog = cx.new(|cx| BrowseChannelsDialog::new(model, query, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    fn open_custom_emoji(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Only admins and owners manage custom emoji; the sidebar hides the entry from everyone
        // else.
        if !self.model.read(cx).core().can_manage_emoji() {
            return;
        }
        let model = self.model.clone();
        let dialog = cx.new(|cx| CustomEmojiDialog::new(model, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    fn open_groups(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Only admins and owners manage groups; the sidebar hides the entry from everyone else.
        if !self.model.read(cx).core().can_manage_groups() {
            return;
        }
        let model = self.model.clone();
        let dialog = cx.new(|cx| GroupsDialog::new(model, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    fn open_people(&mut self, mode: PeopleMode, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let dialog = cx.new(|cx| PeopleDialog::new(model, mode, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    fn open_new_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_people(
            PeopleMode::NewConversation {
                prefill: Vec::new(),
                from: None,
            },
            window,
            cx,
        );
    }

    fn open_delete_message(
        &mut self,
        conversation: ConversationId,
        message: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = self.model.clone();
        let dialog =
            cx.new(|cx| DeleteMessageDialog::new(model, conversation, message, window, cx));
        self.show(dialog, Vec::new(), window, cx);
    }

    /// Opens the image viewer on `images[index]`.
    pub fn open_viewer(
        &mut self,
        images: Vec<lazy_protocol::Attachment>,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = self.model.clone();
        let viewer = cx.new(|cx| ImageViewer::new(model, images, index, window, cx));
        self.show(viewer, Vec::new(), window, cx);
    }

    fn open_details(&mut self, id: ConversationId, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let dialog = cx.new(|cx| DetailsDialog::new(model, id, window, cx));
        // The details dialog can ask for another dialog once it has closed itself.
        let request = cx.subscribe_in(
            &dialog,
            window,
            |this, _dialog, request: &DetailsRequest, window, cx| match request {
                DetailsRequest::AddPeopleToChannel(id) => {
                    this.open_people(PeopleMode::AddToChannel(*id), window, cx);
                }
                DetailsRequest::OpenProfile(user) => this.open_profile(*user, window, cx),
                DetailsRequest::StartLargerGroup { from, prefill } => this.open_people(
                    PeopleMode::NewConversation {
                        prefill: prefill.clone(),
                        from: Some(*from),
                    },
                    window,
                    cx,
                ),
            },
        );
        self.show(dialog, vec![request], window, cx);
    }

    // ----- Pieces -----

    fn banner(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let core = self.model.read(cx).core();
        // The banner is about the workspace on screen: another workspace's connection trouble is
        // the rail's to show (its square is dimmed), not a warning over a chat that works.
        let own = core
            .workspace_rail()
            .into_iter()
            .find(|item| item.active)
            .map_or_else(|| core.connection(), |item| item.connection);
        let connection = match own {
            ConnectionState::Reconnecting { attempt } => Some((
                format!(
                    "Reconnecting… (attempt {attempt}) Your conversations will update when you're back online."
                ),
                true,
            )),
            ConnectionState::Connecting => Some(("Connecting…".to_owned(), false)),
            ConnectionState::CatchingUp => Some(("Catching up…".to_owned(), false)),
            ConnectionState::Live | ConnectionState::Offline => None,
        };
        let notice = core.notice().map(str::to_owned);
        // A permalink to a channel the person has not joined: the notice explains, and joining
        // is one click away.
        let join_offer = core.join_offer();
        let favorite_error = core.selected_conversation().and_then(|id| {
            match core.action(ActionKind::SetFavorite(id)) {
                ActionState::Failed(message) => Some((id, message.clone())),
                _ => None,
            }
        });
        if connection.is_none() && notice.is_none() && favorite_error.is_none() {
            return None;
        }
        let colors = theme.colors;
        let mut stack = div().flex().flex_col();
        if let Some((text, is_warning)) = connection {
            stack = stack.child(
                div()
                    .px_4()
                    .py_1()
                    .text_size(theme.type_scale.small)
                    .bg(if is_warning {
                        colors.warning_surface
                    } else {
                        colors.surface_hover
                    })
                    .text_color(if is_warning {
                        colors.warning
                    } else {
                        colors.text_muted
                    })
                    .child(text),
            );
        }
        if let Some(text) = notice {
            stack = stack.child(
                div()
                    .debug_selector(|| "notice-banner".into())
                    .px_4()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(theme.type_scale.small)
                    .bg(colors.danger_surface)
                    .text_color(colors.danger)
                    .child(div().flex_1().child(text))
                    .children(join_offer.map(|channel| {
                        let this = cx.entity().downgrade();
                        form::button(
                            theme,
                            "join-offer",
                            "Join channel",
                            ButtonKind::Secondary,
                            true,
                            &self.join_focus,
                            move |_, cx| {
                                this.update(cx, |shell, cx| {
                                    shell.model.update(cx, |model, cx| {
                                        model.dispatch(Input::JoinChannel(channel), cx)
                                    });
                                })
                                .ok();
                            },
                        )
                    }))
                    .child({
                        let this = cx.entity().downgrade();
                        form::button(
                            theme,
                            "dismiss-notice",
                            "Dismiss",
                            ButtonKind::Secondary,
                            true,
                            &self.dismiss_notice_focus,
                            move |_, cx| {
                                this.update(cx, |shell, cx| {
                                    shell.model.update(cx, |model, cx| {
                                        model.dispatch(Input::DismissNotice, cx)
                                    });
                                })
                                .ok();
                            },
                        )
                    }),
            );
        }
        if let Some((id, message)) = favorite_error {
            stack = stack.child(
                div()
                    .px_4()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_size(theme.type_scale.small)
                    .bg(colors.danger_surface)
                    .text_color(colors.danger)
                    .child(
                        div()
                            .flex_1()
                            .child(format!("Couldn't change favorite: {message}")),
                    )
                    .child({
                        let this = cx.entity().downgrade();
                        form::button(
                            theme,
                            "dismiss-favorite-error",
                            "Dismiss",
                            ButtonKind::Secondary,
                            true,
                            &self.dismiss_favorite_focus,
                            move |_, cx| {
                                this.update(cx, |shell, cx| {
                                    shell.model.update(cx, |model, cx| {
                                        model.dispatch(
                                            Input::DismissAction(ActionKind::SetFavorite(id)),
                                            cx,
                                        );
                                    });
                                })
                                .ok();
                            },
                        )
                    }),
            );
        }
        Some(stack.into_any_element())
    }

    fn header(
        &self,
        theme: &Theme,
        id: ConversationId,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let core = self.model.read(cx).core();
        let title = core.conversation_title(id);
        let conversation = core.conversation(id).cloned();
        let favorite = core.user_state(id).is_some_and(|state| state.favorite);
        let favoriting = core.action(ActionKind::SetFavorite(id)) == &ActionState::InFlight;
        let (prefix, topic, members, archived) = match &conversation {
            Some(c) if c.kind == ConversationKind::Channel => (
                if c.visibility == ChannelVisibility::Private {
                    "🔒 "
                } else {
                    "# "
                },
                c.topic.clone().unwrap_or_default(),
                Some(format!(
                    "{} member{}",
                    c.member_count,
                    if c.member_count == 1 { "" } else { "s" }
                )),
                c.archived,
            ),
            Some(c) => (
                "",
                String::new(),
                Some(format!("{} people", c.participants.len())),
                false,
            ),
            None => ("", String::new(), None, false),
        };
        let this = cx.entity().downgrade();
        let star_to = this.clone();
        let details_to = this.clone();
        div()
            .h(px(52.))
            .flex_none()
            .px_5()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex_none()
                    .max_w(px(260.))
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.title)
                    .text_color(colors.text)
                    .child(format!("{prefix}{title}")),
            )
            .children(archived.then(|| {
                div()
                    .flex_none()
                    .px_2()
                    .rounded(theme.radii.small)
                    .bg(colors.warning_surface)
                    .text_color(colors.warning)
                    .text_size(theme.type_scale.small)
                    .child("Archived")
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child(topic),
            )
            // In the compact layout the rail takes the room the count would need; Details has it.
            .children(members.filter(|_| !compact).map(|text| {
                div()
                    .flex_none()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_faint)
                    .child(text)
            }))
            .child(self.bell(theme, id, cx))
            .child(form::button(
                theme,
                "favorite-toggle",
                if favorite {
                    "★ Favorited"
                } else {
                    "☆ Favorite"
                },
                ButtonKind::Secondary,
                !favoriting,
                &self.star_focus,
                move |window, cx| {
                    star_to
                        .update(cx, |shell, cx| {
                            shell.run_command(CommandId::ToggleFavorite, window, cx)
                        })
                        .ok();
                },
            ))
            .child(form::button(
                theme,
                "details-button",
                "Details",
                ButtonKind::Secondary,
                true,
                &self.details_focus,
                move |window, cx| {
                    details_to
                        .update(cx, |shell, cx| {
                            shell.run_command(CommandId::ConversationDetails, window, cx)
                        })
                        .ok();
                },
            ))
            .into_any_element()
    }

    /// The bell in the conversation header: how much of this conversation notifies.
    fn bell(&self, theme: &Theme, id: ConversationId, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let muted = self.model.read(cx).core().is_muted(id);
        let this = cx.entity().downgrade();
        let key_to = this.clone();
        let menu = self.bell_menu.clone();
        div()
            .relative()
            .flex_none()
            .child(
                div()
                    .id("notify-bell")
                    .debug_selector(|| "notify-bell".into())
                    .role(Role::Button)
                    .aria_label(if muted {
                        "Notifications for this conversation (muted)"
                    } else {
                        "Notifications for this conversation"
                    })
                    .track_focus(&self.bell_focus)
                    .px_2()
                    .py_1()
                    .rounded(theme.radii.medium)
                    .border_1()
                    .border_color(colors.border_strong)
                    .bg(colors.surface_raised)
                    .cursor_pointer()
                    .focus_visible(move |style| style.border_color(colors.focus_ring).border_2())
                    .hover(move |style| style.bg(colors.surface_hover))
                    .on_key_down(move |event, window, cx| {
                        if keyboard::is_activation(event) {
                            key_to
                                .update(cx, |shell, cx| shell.open_bell_menu(id, window, cx))
                                .ok();
                            cx.stop_propagation();
                        }
                    })
                    .on_click(move |_, window, cx| {
                        this.update(cx, |shell, cx| shell.open_bell_menu(id, window, cx))
                            .ok();
                    })
                    .child(emoji_font::glyphs(if muted { "🔕" } else { "🔔" })),
            )
            .children(menu.map(|menu| {
                deferred(div().absolute().top(px(40.)).right_0().child(menu)).with_priority(2)
            }))
            .into_any_element()
    }

    /// Shown instead of a conversation when none is selected.
    fn empty_state(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let browse_to = this.clone();
        let message_to = this;
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .px_8()
            // Moss welcomes a person who has no conversations yet, with the next step beside it.
            .children(crate::brand::moss(cx, crate::brand::Moss::Welcome, 128.))
            .child(crate::fonts::heading(
                div()
                    .text_size(theme.type_scale.heading)
                    .text_color(colors.text)
                    .child("A good place to start."),
            ))
            .child(
                div()
                    .text_size(theme.type_scale.body)
                    .text_color(colors.text_muted)
                    .child("Find your people and say hello."),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(form::button(theme, "empty-browse", "Browse channels", ButtonKind::Primary, true, &self.empty_browse_focus, move |window, cx| {
                        browse_to.update(cx, |shell, cx| shell.open_browse(None, window, cx)).ok();
                    }))
                    .child(form::button(theme, "empty-message", "New message", ButtonKind::Secondary, true, &self.empty_message_focus, move |window, cx| {
                        message_to.update(cx, |shell, cx| shell.open_new_message(window, cx)).ok();
                    })),
            )
            .into_any_element()
    }
}

impl Render for ShellView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let compact = f32::from(window.viewport_size().width) < COMPACT_BELOW;
        let selected = self.model.read(cx).core().selected_conversation();

        let mut main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme.colors.surface);
        let searching = self.model.read(cx).core().search().is_some();
        let inbox_open = self.model.read(cx).core().inbox().is_some();
        let saved_open = self.model.read(cx).core().saved().open;
        let people_open = self.model.read(cx).core().people().open;
        match selected {
            _ if saved_open => {
                if let Some(banner) = self.banner(&theme, cx) {
                    main = main.child(banner);
                }
                main = main.child(div().flex_1().min_h_0().child(self.saved.clone()));
            }
            _ if people_open => {
                if let Some(banner) = self.banner(&theme, cx) {
                    main = main.child(banner);
                }
                main = main.child(div().flex_1().min_h_0().child(self.people.clone()));
            }
            _ if inbox_open && !searching => {
                if let Some(banner) = self.banner(&theme, cx) {
                    main = main.child(banner);
                }
                main = main.child(div().flex_1().min_h_0().child(self.inbox.clone()));
            }
            _ if searching => {
                if let Some(banner) = self.banner(&theme, cx) {
                    main = main.child(banner);
                }
                main = main.child(div().flex_1().min_h_0().child(self.search.clone()));
            }
            Some(id) => {
                main = main.child(self.header(&theme, id, compact, cx));
                if let Some(banner) = self.banner(&theme, cx) {
                    main = main.child(banner);
                }
                main = main
                    .child(div().flex_1().min_h_0().child(self.timeline.clone()))
                    .child(self.composer.clone());
            }
            None => {
                if let Some(banner) = self.banner(&theme, cx) {
                    main = main.child(banner);
                }
                main = main.child(div().flex_1().min_h_0().child(self.empty_state(&theme, cx)));
            }
        }

        let sidebar_width = if compact {
            COMPACT_SIDEBAR_WIDTH
        } else {
            SIDEBAR_WIDTH
        };
        // The thread pane: beside the conversation when both fit, else instead of it.
        let thread_open = self.model.read(cx).core().open_thread().is_some();
        let viewport_width = f32::from(window.viewport_size().width);
        let rail_width = if compact {
            rail::COMPACT_RAIL_WIDTH
        } else {
            rail::RAIL_WIDTH
        };
        let available = viewport_width - sidebar_width - rail_width;
        let side_by_side = thread_open && thread_layout::fits_side_by_side(available);
        let replaces = thread_open && !side_by_side;
        if thread_open {
            self.pane_layout.set(PaneLayout {
                width: thread_layout::pane_width(
                    self.model.read(cx).thread_pane_width(),
                    available,
                ),
                replaces_conversation: replaces,
            });
        }
        // While files are dragged over the window, a veil over the side they would be attached to.
        let dragging = self.dragging_files && cx.has_active_drag();
        let drop_overlay = dragging.then(|| {
            let over_pane = self.drag_over_pane && thread_open;
            let (text, region) = if over_pane {
                (
                    self.thread_pane.read(cx).composer().read(cx).drop_label(cx),
                    self.pane_layout.get().width + HANDLE_WIDTH,
                )
            } else {
                (self.composer.read(cx).drop_label(cx), 0.0)
            };
            let veil = div()
                .debug_selector(|| "drop-overlay".into())
                .absolute()
                .top_0()
                .bottom_0()
                .m_2()
                .rounded(theme.radii.large)
                .border_2()
                .border_color(theme.colors.accent)
                .bg(theme.colors.accent_surface.opacity(0.9))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .px_4()
                        .py_2()
                        .rounded(theme.radii.medium)
                        .bg(theme.colors.surface_raised)
                        .text_size(theme.type_scale.title)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.colors.text)
                        .child(text),
                );
            if over_pane {
                veil.right_0().w(px(region))
            } else if side_by_side {
                veil.left_0()
                    .right(px(self.pane_layout.get().width + HANDLE_WIDTH))
            } else {
                veil.left_0().right_0()
            }
        });
        div()
            .size_full()
            .relative()
            .key_context(keyboard::SHELL_CONTEXT)
            // Dragging the thread pane's handle: the pane's width follows the pointer.
            .on_drag_move::<ThreadResizeDrag>(cx.listener(
                |this, event: &DragMoveEvent<ThreadResizeDrag>, window, cx| {
                    let right = f32::from(window.viewport_size().width);
                    let pointer = f32::from(event.event.position.x) + HANDLE_WIDTH / 2.0;
                    let wish = thread_layout::wish_from_drag(pointer, right);
                    this.model
                        .update(cx, |model, cx| model.set_thread_pane_width(wish, cx));
                },
            ))
            // Files dragged in from outside: the overlay says where they will go, and dropping
            // them attaches them to that composer (the pane's own drop handler takes those
            // dropped on the pane).
            .on_drag_move::<gpui::ExternalPaths>(cx.listener(
                |this, event: &DragMoveEvent<gpui::ExternalPaths>, window, cx| {
                    let layout = this.pane_layout.get();
                    let right = f32::from(window.viewport_size().width);
                    let over_pane = this.model.read(cx).core().open_thread().is_some()
                        && (layout.replaces_conversation
                            || f32::from(event.event.position.x)
                                >= right - layout.width - HANDLE_WIDTH);
                    if !this.dragging_files || this.drag_over_pane != over_pane {
                        this.dragging_files = true;
                        this.drag_over_pane = over_pane;
                        cx.notify();
                    }
                },
            ))
            .on_drop::<gpui::ExternalPaths>(cx.listener(
                |this, paths: &gpui::ExternalPaths, _window, cx| {
                    this.dragging_files = false;
                    this.composer.update(cx, |composer, cx| {
                        composer.attach_paths(paths.paths().to_vec(), cx)
                    });
                    cx.notify();
                },
            ))
            // Every shortcut and every native menu item is a command (see `commands`).
            .on_action(
                cx.listener(|this, command: &commands::RunCommand, window, cx| {
                    this.run_command(command.0, window, cx)
                }),
            )
            .on_action(cx.listener(|_, _: &FocusNextControl, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &FocusPrevControl, window, cx| window.focus_prev(cx)))
            .flex()
            .bg(theme.colors.surface)
            .child(self.rail.clone())
            .child(
                div()
                    .w(px(sidebar_width))
                    .flex_none()
                    .h_full()
                    .bg(theme.colors.sidebar)
                    .border_r_1()
                    .border_color(theme.colors.border)
                    .child(self.sidebar.clone()),
            )
            .when(!replaces, |layout| layout.child(main))
            .when(thread_open, |layout| layout.child(self.thread_pane.clone()))
            .children(drop_overlay)
            .children(self.overlay.as_ref().map(|overlay| overlay.view.clone()))
            .children(self.command_menu.as_ref().map(|menu| {
                // Under the "Menu" button at the top of the sidebar.
                let rail_width = if compact {
                    rail::COMPACT_RAIL_WIDTH
                } else {
                    rail::RAIL_WIDTH
                };
                deferred(
                    div()
                        .absolute()
                        .top(px(48.))
                        .left(px(rail_width + 8.))
                        .child(menu.clone()),
                )
                .with_priority(3)
            }))
            .children(self.picker.as_ref().map(|open| {
                let (corner, nudge) = match open.purpose {
                    // Below the button that was clicked, or above the composer's.
                    PickerPurpose::Reaction { .. } => (Anchor::TopRight, px(4.)),
                    PickerPurpose::Composer | PickerPurpose::ThreadComposer => {
                        (Anchor::BottomRight, px(-4.))
                    }
                };
                deferred(
                    anchored()
                        .position(open.anchor)
                        .anchor(corner)
                        .offset(gpui::point(px(0.), nudge))
                        .snap_to_window_with_margin(px(8.))
                        .child(open.view.clone()),
                )
                .with_priority(2)
            }))
    }
}
