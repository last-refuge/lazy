//! The conversation sidebar: Favorites, Channels, and Direct messages, with unread styling,
//! section "+" menus, empty states, and the signed-in person's card.

use std::collections::HashMap;

use gpui::{
    AnyElement, Context, ElementId, Entity, EventEmitter, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, Role, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::*, px,
    transparent_black,
};
use lazy_client_core::Input;
use lazy_domain::{ChannelVisibility, ConversationId, ConversationKind};

use crate::app_model::AppModel;
use crate::emoji_font;
use crate::form::{self, ButtonKind};
use crate::keyboard;
use crate::menu::{Menu, MenuEvent, MenuItem};
use crate::states;
use crate::theme::{Theme, avatar_color};
use crate::tooltip::TextTooltip;

/// Below this window width the sidebar is in its compact layout (the shell narrows it too): the
/// app's symbol at the top gives way to the workspace's name.
const COMPACT_BELOW: f32 = 760.0;

/// One of the two finders at the top: what it is called, its key, and what it does.
struct Finder {
    selector: &'static str,
    aria: &'static str,
    /// The name on the button.
    short: &'static str,
    /// The key: in the tooltip and the accessible name, not on the button.
    hint: &'static str,
    event: SidebarEvent,
}

/// What the sidebar asks the shell to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarEvent {
    /// Open the command menu (the "Menu" button; a Mac has the native menu bar instead).
    Menu,
    /// Sign out.
    SignOut,
    CreateChannel,
    BrowseChannels,
    NewMessage,
    /// Open search (the same as Ctrl+F).
    Search,
    /// Open the thread inbox.
    Inbox,
    /// Open the saved messages.
    Saved,
    /// Open the people directory.
    People,
    /// Open the quick switcher (the same as Ctrl+K).
    QuickSwitcher,
    /// Open the preferences dialog (the person's own row at the bottom).
    Preferences,
}

/// Unread counts above this show as `99+`.
const BADGE_CAP: u32 = 99;

/// Text for an unread badge, or `None` when there is nothing unread.
pub fn badge_text(count: u32) -> Option<String> {
    match count {
        0 => None,
        1..=BADGE_CAP => Some(count.to_string()),
        _ => Some(format!("{BADGE_CAP}+")),
    }
}

/// The entries above the channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavKind {
    Threads,
    Saved,
    People,
}

/// Everything one sidebar row shows, read from the core.
#[derive(Debug, Clone)]
struct RowData {
    id: ConversationId,
    title: String,
    kind: ConversationKind,
    visibility: ChannelVisibility,
    archived: bool,
    unread: u32,
    mentions: u32,
    selected: bool,
    /// Seed for the avatar color of a direct or group conversation.
    avatar_seed: u128,
    participants: usize,
    /// The person muted it: its unread count is shown, quietly.
    muted: bool,
}

pub struct SidebarView {
    model: Entity<AppModel>,
    row_focus: HashMap<ConversationId, FocusHandle>,
    sign_out_focus: FocusHandle,
    add_channel_focus: FocusHandle,
    /// The "Menu" button at the top (not on a Mac): the sidebar's first tab stop.
    menu_button_focus: FocusHandle,
    add_message_focus: FocusHandle,
    browse_link_focus: FocusHandle,
    message_link_focus: FocusHandle,
    search_focus: FocusHandle,
    switcher_focus: FocusHandle,
    threads_focus: FocusHandle,
    saved_focus: FocusHandle,
    people_focus: FocusHandle,
    /// The person's own row at the bottom, which opens Preferences.
    prefs_focus: FocusHandle,
    menu: Option<Entity<Menu>>,
    _menu_subscription: Option<Subscription>,
}

impl EventEmitter<SidebarEvent> for SidebarView {}

impl SidebarView {
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        // A picture that finishes decoding is drawn at once.
        cx.observe(&crate::image_store::ImageStore::global(cx), |_, _, cx| {
            cx.notify()
        })
        .detach();
        Self {
            model,
            row_focus: HashMap::new(),
            sign_out_focus: cx.focus_handle().tab_stop(true),
            add_channel_focus: cx.focus_handle().tab_stop(true),
            menu_button_focus: cx.focus_handle().tab_stop(true),
            add_message_focus: cx.focus_handle().tab_stop(true),
            browse_link_focus: cx.focus_handle().tab_stop(true),
            message_link_focus: cx.focus_handle().tab_stop(true),
            search_focus: cx.focus_handle().tab_stop(true),
            switcher_focus: cx.focus_handle().tab_stop(true),
            threads_focus: cx.focus_handle().tab_stop(true),
            saved_focus: cx.focus_handle().tab_stop(true),
            people_focus: cx.focus_handle().tab_stop(true),
            prefs_focus: cx.focus_handle().tab_stop(true),
            menu: None,
            _menu_subscription: None,
        }
    }

    /// The focus handle of the Threads entry, for a view to return to.
    pub fn threads_focus(&self) -> FocusHandle {
        self.threads_focus.clone()
    }

    /// The focus handle of the Saved entry, for a view to return to.
    pub fn saved_focus(&self) -> FocusHandle {
        self.saved_focus.clone()
    }

    /// The focus handle of the People entry, for a view to return to.
    pub fn people_focus(&self) -> FocusHandle {
        self.people_focus.clone()
    }

    /// The focus handle of the quick switcher hint at the top.
    pub fn switcher_focus(&self) -> FocusHandle {
        self.switcher_focus.clone()
    }

    /// The focus handle of the person's own row in the footer, which opens Preferences.
    pub fn prefs_focus(&self) -> FocusHandle {
        self.prefs_focus.clone()
    }

    /// The focus handle of the Search entry at the top, for tests and for the shell to return to.
    pub fn search_focus(&self) -> FocusHandle {
        self.search_focus.clone()
    }

    /// The focus handle of the "+" next to Channels, for a dialog to return to.
    pub fn channels_plus_focus(&self) -> FocusHandle {
        self.add_channel_focus.clone()
    }

    fn rows(&self, ids: &[ConversationId], cx: &gpui::App) -> Vec<RowData> {
        let core = self.model.read(cx).core();
        let selected = core.selected_conversation();
        let me = core.me().map(|user| user.id);
        ids.iter()
            .filter_map(|id| {
                let conversation = core.conversation(*id)?;
                let state = core.user_state(*id);
                let other = conversation
                    .participants
                    .iter()
                    .find(|user| Some(**user) != me);
                Some(RowData {
                    id: *id,
                    title: core.conversation_title(*id),
                    kind: conversation.kind,
                    visibility: conversation.visibility,
                    archived: conversation.archived,
                    unread: state.map_or(0, |s| s.unread_count),
                    mentions: state.map_or(0, |s| s.mention_count),
                    selected: selected == Some(*id),
                    avatar_seed: other.map_or(id.as_uuid().as_u128(), |u| u.as_uuid().as_u128()),
                    participants: conversation.participants.len(),
                    muted: core.is_muted(*id),
                })
            })
            .collect()
    }

    fn select(&mut self, id: ConversationId, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::SelectConversation(id), cx)
        });
    }

    fn open_channels_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // When only admins may create channels, everyone else sees why the item is off: the
        // registry's rule, the same one the File menu and the palette use.
        let create = crate::commands::availability(
            crate::commands::CommandId::CreateChannel,
            &crate::commands::AppSnapshot::from_core(self.model.read(cx).core()),
        );
        // Only a reason is worth a second line; what an item does is in its name.
        let mut create_item =
            MenuItem::new("create", "Create a channel").enabled(create.is_enabled());
        if let crate::commands::Availability::Disabled(reason) = create {
            create_item = create_item.detail(reason);
        }
        let menu = cx.new(|cx| {
            Menu::new(
                "channels-menu",
                vec![create_item, MenuItem::new("browse", "Browse channels")],
                cx,
            )
        });
        self._menu_subscription =
            Some(cx.subscribe(&menu, |this, _menu, event: &MenuEvent, cx| {
                if let MenuEvent::Select(id) = event {
                    match id.as_ref() {
                        "create" => cx.emit(SidebarEvent::CreateChannel),
                        "browse" => cx.emit(SidebarEvent::BrowseChannels),
                        _ => {}
                    }
                }
                this.menu = None;
                cx.notify();
            }));
        menu.update(cx, |menu, cx| menu.open(window, cx));
        self.menu = Some(menu);
        cx.notify();
    }

    fn row(&mut self, row: &RowData, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let id = row.id;
        let focus = self
            .row_focus
            .entry(id)
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone();
        let unread = row.unread > 0 && !row.selected;
        let muted = row.muted && !row.selected;
        let name = format!("conv-{}", row.title);

        let leading: AnyElement = match row.kind {
            ConversationKind::Channel => div()
                .w(px(16.))
                .flex_none()
                .text_color(if row.selected {
                    colors.sidebar_selected_text
                } else {
                    colors.sidebar_text_muted
                })
                .child(if row.visibility == ChannelVisibility::Private {
                    "🔒"
                } else {
                    "#"
                })
                .into_any_element(),
            ConversationKind::Direct | ConversationKind::Group => div()
                .size(px(18.))
                .flex_none()
                .rounded(theme.radii.small)
                .bg(avatar_color(row.avatar_seed))
                .text_color(gpui::white())
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .flex()
                .items_center()
                .justify_center()
                .child(if row.kind == ConversationKind::Group {
                    row.participants.to_string()
                } else {
                    row.title
                        .chars()
                        .next()
                        .unwrap_or('?')
                        .to_uppercase()
                        .to_string()
                })
                .into_any_element(),
        };

        let mut badges = div().flex().items_center().gap_1().flex_none();
        if let Some(text) = badge_text(row.mentions).filter(|_| !row.selected) {
            let selector = format!("mention-badge-{}", row.title);
            badges = badges.child(
                div()
                    .debug_selector(move || selector)
                    .px_1p5()
                    .rounded(theme.radii.small)
                    .when(!muted, |badge| {
                        badge
                            .bg(colors.danger)
                            .text_color(gpui::white())
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(muted, |badge| {
                        badge
                            .bg(colors.sidebar_hover)
                            .text_color(colors.sidebar_text_muted)
                    })
                    .text_size(theme.type_scale.small)
                    .child(format!("@{text}")),
            );
        }
        if let Some(text) = badge_text(row.unread).filter(|_| unread) {
            let selector = format!("badge-{}", row.title);
            badges = badges.child(
                div()
                    .debug_selector(move || selector)
                    .px_1p5()
                    .rounded(theme.radii.small)
                    .when(!muted, |badge| {
                        badge
                            .bg(colors.accent)
                            .text_color(colors.on_accent)
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(muted, |badge| {
                        badge
                            .bg(colors.sidebar_hover)
                            .text_color(colors.sidebar_text_muted)
                    })
                    .text_size(theme.type_scale.small)
                    .child(text),
            );
        }

        div()
            .id(ElementId::Name(format!("channel-{id}").into()))
            .debug_selector(move || name)
            .track_focus(&focus)
            .border_1()
            .border_color(transparent_black())
            .focus_visible(move |style| style.border_color(colors.focus_ring))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if keyboard::is_activation(event) {
                    this.select(id, cx);
                    cx.stop_propagation();
                }
            }))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(theme.radii.small)
            .cursor_pointer()
            .text_size(theme.type_scale.body)
            .when(row.selected, |item| {
                item.bg(colors.sidebar_selected)
                    .text_color(colors.sidebar_selected_text)
                    .font_weight(FontWeight::SEMIBOLD)
            })
            .when(!row.selected, |item| {
                item.text_color(if (unread || !row.archived) && !muted {
                    colors.sidebar_text
                } else {
                    colors.sidebar_text_muted
                })
                .when(unread && !muted, |item| {
                    item.font_weight(FontWeight::SEMIBOLD)
                })
                .hover(move |style| style.bg(colors.sidebar_hover))
            })
            .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
            .child(leading)
            .child(div().flex_1().min_w_0().truncate().child(row.title.clone()))
            .child(badges)
            .into_any_element()
    }

    /// An entry above the channels that opens a view in place of the conversation: Threads,
    /// Saved, or People. `unread` is a count shown quietly as a pill (0 shows none).
    fn nav_row(
        &mut self,
        theme: &Theme,
        kind: NavKind,
        open: bool,
        unread: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let (selector, label, glyph, event, badge) = match kind {
            NavKind::Threads => (
                "sidebar-threads",
                "Threads",
                "↳",
                SidebarEvent::Inbox,
                "badge-threads",
            ),
            NavKind::Saved => (
                "sidebar-saved",
                "Saved",
                "★",
                SidebarEvent::Saved,
                "badge-saved",
            ),
            NavKind::People => (
                "sidebar-people",
                "People",
                "@",
                SidebarEvent::People,
                "badge-people",
            ),
        };
        let focus = match kind {
            NavKind::Threads => self.threads_focus.clone(),
            NavKind::Saved => self.saved_focus.clone(),
            NavKind::People => self.people_focus.clone(),
        };
        div()
            .id(selector)
            .debug_selector(move || selector.into())
            .role(Role::Button)
            .aria_label(if unread > 0 {
                format!("{label}, {unread} unread")
            } else {
                label.to_owned()
            })
            .track_focus(&focus)
            .border_1()
            .border_color(transparent_black())
            .focus_visible(move |style| style.border_color(colors.focus_ring))
            .on_key_down(cx.listener(move |_, event_key: &KeyDownEvent, _, cx| {
                if keyboard::is_activation(event_key) {
                    cx.emit(event);
                    cx.stop_propagation();
                }
            }))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(theme.radii.small)
            .cursor_pointer()
            .text_size(theme.type_scale.body)
            .when(open, |item| {
                item.bg(colors.sidebar_selected)
                    .text_color(colors.sidebar_selected_text)
                    .font_weight(FontWeight::SEMIBOLD)
            })
            .when(!open, |item| {
                item.text_color(colors.sidebar_text)
                    .when(unread > 0, |item| item.font_weight(FontWeight::SEMIBOLD))
                    .hover(move |style| style.bg(colors.sidebar_hover))
            })
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(event)))
            .child(
                div()
                    .w(px(16.))
                    .flex_none()
                    .text_color(if open {
                        colors.sidebar_selected_text
                    } else {
                        colors.sidebar_text_muted
                    })
                    .child(glyph),
            )
            .child(div().flex_1().min_w_0().truncate().child(label))
            .children((unread > 0 && !open).then(|| {
                div()
                    .debug_selector(move || badge.into())
                    .px_1p5()
                    .rounded(theme.radii.small)
                    .bg(colors.accent)
                    .text_color(colors.on_accent)
                    .text_size(theme.type_scale.small)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(badge_text(unread).unwrap_or_default())
            }))
            .into_any_element()
    }

    fn section_header(
        &self,
        theme: &Theme,
        label: &'static str,
        plus: Option<AnyElement>,
    ) -> AnyElement {
        div()
            .px_4()
            .pt_3()
            .pb_1()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.colors.sidebar_text_muted)
                    .child(label),
            )
            .children(plus)
            .into_any_element()
    }

    fn plus_button(
        &self,
        theme: &Theme,
        id: &'static str,
        focus: &FocusHandle,
        on_activate: impl Fn(&mut Window, &mut gpui::App) + 'static,
    ) -> gpui::Stateful<gpui::Div> {
        let colors = theme.colors;
        let on_activate = std::rc::Rc::new(on_activate);
        let on_key = on_activate.clone();
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .track_focus(focus)
            .border_1()
            .border_color(transparent_black())
            .focus_visible(move |style| style.border_color(colors.focus_ring))
            .cursor_pointer()
            .px_1p5()
            .rounded(theme.radii.small)
            .text_size(theme.type_scale.body)
            .text_color(colors.sidebar_text_muted)
            .hover(move |style| {
                style
                    .bg(colors.sidebar_hover)
                    .text_color(colors.sidebar_text)
            })
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                if keyboard::is_activation(event) {
                    on_key(window, cx);
                    cx.stop_propagation();
                }
            })
            .on_click(move |_, window, cx| on_activate(window, cx))
            .child(SharedString::from("+"))
    }
}

impl SidebarView {
    /// A quiet text link under a section's heading, in place of an explanation (Tab-reachable,
    /// Enter and Space activate it). Its text lines up with the names of the rows above and below.
    fn quiet_link(
        &self,
        selector: &'static str,
        text: &'static str,
        focus: &FocusHandle,
        event: SidebarEvent,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let colors = theme.colors;
        div()
            .id(selector)
            .debug_selector(move || selector.into())
            .role(Role::Button)
            .aria_label(text)
            .track_focus(focus)
            .border_1()
            .border_color(transparent_black())
            .focus_visible(move |style| style.border_color(colors.focus_ring))
            .on_key_down(cx.listener(move |_, key: &KeyDownEvent, _, cx| {
                if keyboard::is_activation(key) {
                    cx.emit(event);
                    cx.stop_propagation();
                }
            }))
            .cursor_pointer()
            // A row's own padding (8), its glyph column (16), and the gap after it (8).
            .pl(px(32.))
            .pr_2()
            .py_1()
            .rounded(theme.radii.small)
            .text_size(theme.type_scale.small)
            .text_color(colors.sidebar_text_muted)
            .hover(move |style| {
                style
                    .bg(colors.sidebar_hover)
                    .text_color(colors.sidebar_text)
            })
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(event)))
            .child(text)
    }
}

impl SidebarView {
    /// A finder button: Search or the quick switcher. It shows a short name and shares a row with
    /// the other; its key is in the tooltip and the accessible name, and in the shortcuts list.
    fn finder_button(
        &self,
        theme: &Theme,
        finder: Finder,
        focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = theme.colors;
        let event = finder.event;
        let hint = finder.hint;
        let selector = finder.selector;
        div()
            .id(selector)
            .debug_selector(move || selector.into())
            .role(Role::Button)
            .aria_label(format!("{}, {hint}", finder.aria))
            .tooltip(move |_, cx| TextTooltip::view(hint, cx))
            .track_focus(focus)
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(theme.radii.small)
            .bg(colors.sidebar_hover)
            .border_1()
            .border_color(transparent_black())
            .focus_visible(move |style| style.border_color(colors.focus_ring))
            .on_key_down(cx.listener(move |_, key: &KeyDownEvent, _, cx| {
                if keyboard::is_activation(key) {
                    cx.emit(event);
                    cx.stop_propagation();
                }
            }))
            .cursor_pointer()
            .text_size(theme.type_scale.small)
            .text_color(colors.sidebar_text_muted)
            .hover(move |style| style.text_color(colors.sidebar_text))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(event)))
            .child(div().flex_1().min_w_0().truncate().child(finder.short))
            .into_any_element()
    }
}

impl Render for SidebarView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        // The window is narrow (the shell narrows the sidebar then too).
        let compact = f32::from(window.viewport_size().width) < COMPACT_BELOW;
        // The person's own picture, in the footer.
        let me_id = self.model.read(cx).core().me().map(|user| user.id);
        if let Some(me_id) = me_id {
            crate::avatar_images::request_later(&self.model, vec![me_id], cx);
        }
        let my_picture = me_id.and_then(|id| {
            crate::avatar_images::image_of(
                self.model.read(cx).core(),
                crate::image_store::ImageStore::global(cx).read(cx),
                id,
            )
        });
        let sections = self.model.read(cx).core().sidebar();
        let (workspace_name, me_name, me_seed) = {
            let core = self.model.read(cx).core();
            (
                core.primary_workspace()
                    .map_or_else(|| "Lazy".to_owned(), |w| w.workspace.name.clone()),
                core.me()
                    .map(|u| u.display_name.to_string())
                    .unwrap_or_default(),
                core.me().map_or(0, |u| u.id.as_uuid().as_u128()),
            )
        };
        let (inbox_open, inbox_unread, saved_open, people_open, my_status, paused) = {
            let core = self.model.read(cx).core();
            let me = core.me().map(|user| user.id);
            let status = me.and_then(|me| crate::status_mark::StatusMark::lookup(core, me));
            let paused = core.notifications_paused().then(|| {
                let now = lazy_domain::Timestamp::now();
                core.notification_settings()
                    .dnd_until
                    .filter(|until| *until > now)
                    .map(|until| {
                        crate::notification_form::moment_label(
                            until,
                            now,
                            &jiff::tz::TimeZone::system(),
                        )
                    })
            });
            (
                core.inbox().is_some(),
                core.inbox_unread_total(),
                core.saved().open,
                core.people().open,
                status,
                paused,
            )
        };

        let mut list = div().flex().flex_col().gap_0p5().px_2();
        list = list.child(self.nav_row(&theme, NavKind::Threads, inbox_open, inbox_unread, cx));
        list = list.child(self.nav_row(&theme, NavKind::Saved, saved_open, 0, cx));
        list = list.child(self.nav_row(&theme, NavKind::People, people_open, 0, cx));

        if !sections.favorites.is_empty() {
            list = list.child(self.section_header(&theme, "Favorites", None));
            for row in self.rows(&sections.favorites, cx) {
                list = list.child(self.row(&row, &theme, cx));
            }
        }

        // Channels, with a "+" that opens a menu.
        let this = cx.entity().downgrade();
        let plus = {
            let focus = self.add_channel_focus.clone();
            let this = this.clone();
            let button = self.plus_button(&theme, "add-channel", &focus, move |window, cx| {
                this.update(cx, |sidebar, cx| sidebar.open_channels_menu(window, cx))
                    .ok();
            });
            div()
                .relative()
                .child(button)
                .children(self.menu.clone().map(|menu| {
                    gpui::deferred(div().absolute().top(px(26.)).right_0().child(menu))
                }))
                .into_any_element()
        };
        list = list.child(self.section_header(&theme, "Channels", Some(plus)));
        let channel_rows = self.rows(&sections.channels, cx);
        if channel_rows.is_empty()
            && sections.favorites.iter().all(|id| {
                self.model
                    .read(cx)
                    .core()
                    .conversation(*id)
                    .is_none_or(|c| c.kind != ConversationKind::Channel)
            })
        {
            list = list.child(self.quiet_link(
                "sidebar-browse-channels",
                "Browse channels",
                &self.browse_link_focus,
                SidebarEvent::BrowseChannels,
                &theme,
                cx,
            ));
        }
        for row in channel_rows {
            list = list.child(self.row(&row, &theme, cx));
        }

        // Direct messages, with a "+" that opens the new-message dialog.
        let new_message_plus = {
            let focus = self.add_message_focus.clone();
            let this = this.clone();
            self.plus_button(&theme, "new-message", &focus, move |_, cx| {
                this.update(cx, |_, cx| cx.emit(SidebarEvent::NewMessage))
                    .ok();
            })
            .into_any_element()
        };
        list = list.child(self.section_header(&theme, "Direct messages", Some(new_message_plus)));
        let people_rows = self.rows(&sections.direct_and_group, cx);
        if people_rows.is_empty() {
            list = list.child(self.quiet_link(
                "sidebar-new-message",
                "Find people",
                &self.message_link_focus,
                SidebarEvent::NewMessage,
                &theme,
                cx,
            ));
        }
        for row in people_rows {
            list = list.child(self.row(&row, &theme, cx));
        }

        // The app's symbol beside the workspace's name; the narrow layout keeps the room for the
        // name. Decorative: the name beside it is what is read.
        let mark = if compact {
            None
        } else {
            crate::brand::symbol(cx, 32.)
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_4()
                    .h(px(52.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.title)
                    .text_color(colors.sidebar_text)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .children(mark)
                            .child(div().flex_1().min_w_0().truncate().child(workspace_name)),
                    )
                    // A Mac has the native menu bar; everywhere else the same menus are here.
                    .children((!cfg!(target_os = "macos")).then(|| {
                        let menu_to = this.clone();
                        form::button(
                            &theme,
                            "sidebar-menu",
                            "Menu",
                            ButtonKind::Secondary,
                            true,
                            &self.menu_button_focus,
                            move |_, cx| {
                                menu_to.update(cx, |_, cx| cx.emit(SidebarEvent::Menu)).ok();
                            },
                        )
                    })),
            )
            .child(
                // Search and the quick switcher, side by side. Their keys are in the tooltips and
                // the shortcuts list (Help), not on the buttons.
                div()
                    .flex_none()
                    .px_3()
                    .pt_2()
                    .flex()
                    .gap_1()
                    .child(self.finder_button(
                        &theme,
                        Finder {
                            selector: "sidebar-search",
                            aria: "Search",
                            short: "Search",
                            hint: if cfg!(target_os = "macos") {
                                "⌘F"
                            } else {
                                "Ctrl+F"
                            },
                            event: SidebarEvent::Search,
                        },
                        &self.search_focus.clone(),
                        cx,
                    ))
                    .child(self.finder_button(
                        &theme,
                        Finder {
                            selector: "sidebar-switcher",
                            aria: "Quick switcher",
                            short: "Jump to",
                            hint: if cfg!(target_os = "macos") {
                                "⌘K"
                            } else {
                                "Ctrl+K"
                            },
                            event: SidebarEvent::QuickSwitcher,
                        },
                        &self.switcher_focus.clone(),
                        cx,
                    )),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pb_2()
                    .child(list),
            )
            .child(
                // The person's own row. Their picture and name open Preferences; everything else
                // (workspace settings, invitations, user groups, custom emoji, sign out) is in the
                // Menu at the top, so the only other thing here is Sign out.
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .id("sidebar-preferences")
                            .debug_selector(|| "sidebar-preferences".into())
                            .role(Role::Button)
                            .aria_label("Preferences")
                            .track_focus(&self.prefs_focus)
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_1()
                            .py_1()
                            .border_1()
                            .border_color(transparent_black())
                            .focus_visible(move |style| style.border_color(colors.focus_ring))
                            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                                if keyboard::is_activation(event) {
                                    cx.emit(SidebarEvent::Preferences);
                                    cx.stop_propagation();
                                }
                            }))
                            .cursor_pointer()
                            .rounded(theme.radii.small)
                            .hover(move |style| style.bg(colors.sidebar_hover))
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::Preferences)),
                            )
                            .child(
                                crate::avatar::Avatar::new(me_seed, me_name.clone(), 28.)
                                    .text_size(theme.type_scale.body)
                                    .image(my_picture.clone())
                                    .into_div()
                                    .debug_selector(|| "sidebar-me-avatar".into()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(theme.type_scale.body)
                                            .text_color(colors.sidebar_text)
                                            .child(me_name),
                                    )
                                    .children(my_status.map(|mark| {
                                        crate::status_mark::view(
                                            "sidebar-status",
                                            "sidebar-status".into(),
                                            &mark,
                                            theme.type_scale.body,
                                        )
                                    }))
                                    .children(paused.map(|until| {
                                        let tip = states::paused_tooltip(until.as_deref());
                                        div()
                                            .id("sidebar-paused")
                                            .debug_selector(|| "sidebar-paused".into())
                                            .flex_none()
                                            .role(Role::Image)
                                            .aria_label(tip.clone())
                                            .tooltip(move |_, cx| {
                                                TextTooltip::view(tip.clone(), cx)
                                            })
                                            .text_size(theme.type_scale.body)
                                            .child(emoji_font::glyphs("🔕"))
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .id("sign-out")
                            .debug_selector(|| "sign-out".into())
                            .track_focus(&self.sign_out_focus)
                            .border_1()
                            .border_color(transparent_black())
                            .focus_visible(move |style| style.border_color(colors.focus_ring))
                            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                                if keyboard::is_activation(event) {
                                    cx.emit(SidebarEvent::SignOut);
                                    cx.stop_propagation();
                                }
                            }))
                            .cursor_pointer()
                            .px_2()
                            .py_0p5()
                            .rounded(theme.radii.small)
                            .text_size(theme.type_scale.small)
                            .text_color(colors.sidebar_text_muted)
                            .hover(move |style| {
                                style
                                    .bg(colors.sidebar_hover)
                                    .text_color(colors.sidebar_text)
                            })
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::SignOut)))
                            .child("Sign out"),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_cap_at_ninety_nine_and_hide_at_zero() {
        assert_eq!(badge_text(0), None);
        assert_eq!(badge_text(1).as_deref(), Some("1"));
        assert_eq!(badge_text(99).as_deref(), Some("99"));
        assert_eq!(badge_text(100).as_deref(), Some("99+"));
        assert_eq!(badge_text(u32::MAX).as_deref(), Some("99+"));
    }
}
