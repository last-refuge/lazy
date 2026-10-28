//! The conversation sidebar: Favorites, Channels, and Direct messages, with unread styling,
//! section "+" menus, empty states, and the signed-in person's card.

use std::collections::HashMap;

use gpui::{
    AnyElement, Context, ElementId, Entity, EventEmitter, FocusHandle, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::*, px,
    transparent_black,
};
use lazy_client_core::Input;
use lazy_domain::{ChannelVisibility, ConversationId, ConversationKind};

use crate::app_model::AppModel;
use crate::form::{self, ButtonKind};
use crate::keyboard;
use crate::menu::{Menu, MenuEvent, MenuItem};
use crate::theme::{Theme, avatar_color};

/// What the sidebar asks the shell to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarEvent {
    CreateChannel,
    BrowseChannels,
    NewMessage,
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
}

pub struct SidebarView {
    model: Entity<AppModel>,
    row_focus: HashMap<ConversationId, FocusHandle>,
    sign_out_focus: FocusHandle,
    add_channel_focus: FocusHandle,
    add_message_focus: FocusHandle,
    browse_link_focus: FocusHandle,
    message_link_focus: FocusHandle,
    menu: Option<Entity<Menu>>,
    _menu_subscription: Option<Subscription>,
}

impl EventEmitter<SidebarEvent> for SidebarView {}

impl SidebarView {
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            model,
            row_focus: HashMap::new(),
            sign_out_focus: cx.focus_handle().tab_stop(true),
            add_channel_focus: cx.focus_handle().tab_stop(true),
            add_message_focus: cx.focus_handle().tab_stop(true),
            browse_link_focus: cx.focus_handle().tab_stop(true),
            message_link_focus: cx.focus_handle().tab_stop(true),
            menu: None,
            _menu_subscription: None,
        }
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
        let menu = cx.new(|cx| {
            Menu::new(
                "channels-menu",
                vec![
                    MenuItem::new("create", "Create a channel")
                        .detail("Start a new public or private channel"),
                    MenuItem::new("browse", "Browse channels")
                        .detail("Find a public channel to join"),
                ],
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
                    .bg(colors.danger)
                    .text_color(gpui::white())
                    .text_size(theme.type_scale.small)
                    .font_weight(FontWeight::SEMIBOLD)
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
                    .bg(colors.accent)
                    .text_color(colors.on_accent)
                    .text_size(theme.type_scale.small)
                    .font_weight(FontWeight::SEMIBOLD)
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
                item.text_color(if unread || !row.archived {
                    colors.sidebar_text
                } else {
                    colors.sidebar_text_muted
                })
                .when(unread, |item| item.font_weight(FontWeight::SEMIBOLD))
                .hover(move |style| style.bg(colors.sidebar_hover))
            })
            .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
            .child(leading)
            .child(div().flex_1().min_w_0().truncate().child(row.title.clone()))
            .child(badges)
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

impl Render for SidebarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
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

        let mut list = div().flex().flex_col().gap_0p5().px_2();

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
            let browse_to = this.clone();
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.sidebar_text_muted)
                            .child("You haven't joined any channels."),
                    )
                    .child(form::button(
                        &theme,
                        "sidebar-browse-channels",
                        "Browse channels",
                        ButtonKind::Secondary,
                        true,
                        &self.browse_link_focus,
                        move |_, cx| {
                            browse_to
                                .update(cx, |_, cx| cx.emit(SidebarEvent::BrowseChannels))
                                .ok();
                        },
                    )),
            );
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
            let message_to = this.clone();
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.sidebar_text_muted)
                            .child("No conversations with people yet."),
                    )
                    .child(form::button(
                        &theme,
                        "sidebar-new-message",
                        "Start a conversation",
                        ButtonKind::Secondary,
                        true,
                        &self.message_link_focus,
                        move |_, cx| {
                            message_to
                                .update(cx, |_, cx| cx.emit(SidebarEvent::NewMessage))
                                .ok();
                        },
                    )),
            );
        }
        for row in people_rows {
            list = list.child(self.row(&row, &theme, cx));
        }

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
                    .border_b_1()
                    .border_color(colors.border)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.title)
                    .text_color(colors.sidebar_text)
                    .child(div().truncate().child(workspace_name)),
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
                div()
                    .flex_none()
                    .px_3()
                    .py_3()
                    .border_t_1()
                    .border_color(colors.border)
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(28.))
                            .flex_none()
                            .rounded(theme.radii.small)
                            .bg(avatar_color(me_seed))
                            .text_color(gpui::white())
                            .font_weight(FontWeight::SEMIBOLD)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                me_name
                                    .chars()
                                    .next()
                                    .unwrap_or('?')
                                    .to_uppercase()
                                    .to_string(),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.type_scale.body)
                            .text_color(colors.sidebar_text)
                            .child(me_name),
                    )
                    .child(
                        div()
                            .id("sign-out")
                            .debug_selector(|| "sign-out".into())
                            .track_focus(&self.sign_out_focus)
                            .border_1()
                            .border_color(transparent_black())
                            .focus_visible(move |style| style.border_color(colors.focus_ring))
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                if keyboard::is_activation(event) {
                                    this.model
                                        .update(cx, |model, cx| model.dispatch(Input::SignOut, cx));
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
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model
                                    .update(cx, |model, cx| model.dispatch(Input::SignOut, cx));
                            }))
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
