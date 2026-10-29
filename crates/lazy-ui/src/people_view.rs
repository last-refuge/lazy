//! The people directory: everyone in the workspace by name, searchable, in place of the
//! conversation.
//!
//! Each row shows a picture, the display name with their status emoji, `@username`, and a badge
//! for owners and admins. Admins and owners can also list deactivated people ("Show deactivated"),
//! whose rows are muted and say so. Enter or a click opens the person's profile (their join date is there).
//!
//! # Keyboard
//!
//! The search box has the keyboard when the view opens. Up and Down move the active row, Page Up
//! and Page Down by a screenful, Enter opens the active person's profile, Escape closes the view.
//! Moving to the last row asks for the next page. Tab order: the search box, "Show deactivated"
//! (admins and owners), "Show more" or "Try again", and Close.

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    ParentElement, Render, Role, ScrollStrategy, Styled, Subscription, UniformListScrollHandle,
    Window, div, prelude::*, px, transparent_black, uniform_list,
};
use lazy_client_core::{Input, LoadStatus};
use lazy_domain::UserId;

use crate::app_model::{AppEvent, AppModel};
use crate::form::{self, ButtonKind};
use crate::inbox_view::note_block;
use crate::states;
use crate::status_mark::StatusMark;
use crate::text_input::actions::{MoveDown, MoveUp};
use crate::text_input::{TextInput, TextInputEvent, TextInputOptions};
use crate::theme::Theme;

/// The height of a person's row, so the list never re-measures.
pub const ROW_HEIGHT: f32 = 56.0;
const SKELETON_ROWS: usize = 9;
const FALLBACK_PAGE: usize = 8;

/// What the directory asks the shell to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeopleEvent {
    /// Escape or Close: leave the directory and give the keyboard back.
    Close,
    /// Enter or a click on a person: show their profile.
    OpenProfile(UserId),
}

/// What a row says, for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowInfo {
    pub user: UserId,
    pub name: String,
    pub username: String,
    pub role: Option<&'static str>,
    pub status: Option<String>,
    pub deactivated: bool,
}

/// What one row draws: its index, the person, name, `@username`, role badge, whether it is the
/// person looking, and their status.
type RowParts = (
    usize,
    UserId,
    String,
    String,
    Option<&'static str>,
    bool,
    Option<StatusMark>,
    bool,
);

pub struct PeopleDirectoryView {
    model: Entity<AppModel>,
    scroll: UniformListScrollHandle,
    active: usize,
    search: Entity<TextInput>,
    deactivated_focus: FocusHandle,
    more_focus: FocusHandle,
    retry_focus: FocusHandle,
    close_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PeopleEvent> for PeopleDirectoryView {}

impl PeopleDirectoryView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new("Search people")
                    .accessible_label("Search people")
                    .single_line(true)
                    .pass_through_navigation(true)
                    .max_bytes(400),
                cx,
            )
        });
        let mut subscriptions = Vec::new();
        // A picture that finishes decoding is drawn at once.
        subscriptions.push(
            cx.observe(&crate::image_store::ImageStore::global(cx), |_, _, cx| {
                cx.notify()
            }),
        );
        subscriptions.push(cx.subscribe_in(
            &search,
            window,
            |this, input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Changed => {
                    this.active = 0;
                    let text = input.read(cx).text().to_owned();
                    this.dispatch(Input::SetPeopleQuery(text), cx);
                    cx.notify();
                }
                TextInputEvent::Submit => this.open(this.active, cx),
                TextInputEvent::Tab { backwards } => {
                    if *backwards {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                }
                TextInputEvent::UpWhenEmpty
                | TextInputEvent::PasteAttachments
                | TextInputEvent::Suggestion(_) => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &model,
            window,
            |this, _model, _event: &AppEvent, _window, cx| {
                this.active = this.active.min(this.count(cx).saturating_sub(1));
                cx.notify();
            },
        ));
        Self {
            model,
            scroll: UniformListScrollHandle::new(),
            active: 0,
            search,
            deactivated_focus: cx.focus_handle().tab_stop(true),
            more_focus: cx.focus_handle().tab_stop(true),
            retry_focus: cx.focus_handle().tab_stop(true),
            close_focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
        }
    }

    // ----- For the shell and tests -----

    /// What each row says. For tests.
    pub fn rows_info(&self, cx: &gpui::App) -> Vec<RowInfo> {
        let core = self.model.read(cx).core();
        core.people()
            .people
            .iter()
            .map(|person| RowInfo {
                user: person.user.id,
                name: person.user.display_name.to_string(),
                username: person.user.username.to_string(),
                role: states::role_label(person.role),
                status: StatusMark::lookup(core, person.user.id).map(|mark| mark.text),
                deactivated: person.user.deactivated,
            })
            .collect()
    }

    pub fn active(&self) -> usize {
        self.active
    }

    /// The search box, for tests.
    pub fn search(&self) -> &Entity<TextInput> {
        &self.search
    }

    /// Which tab stop has the keyboard. For tests.
    pub fn stop(&self, window: &Window, cx: &gpui::App) -> &'static str {
        if self.search.read(cx).is_focused(window) {
            "search"
        } else if self.deactivated_focus.is_focused(window) {
            "deactivated"
        } else if self.more_focus.is_focused(window) {
            "more"
        } else if self.retry_focus.is_focused(window) {
            "retry"
        } else if self.close_focus.is_focused(window) {
            "close"
        } else {
            "elsewhere"
        }
    }

    /// Starts afresh with the keyboard in the search box.
    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active = 0;
        self.search.update(cx, |input, cx| input.clear(cx));
        let focus = self.search.focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    // ----- Acting -----

    fn dispatch(&self, input: Input, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
    }

    fn count(&self, cx: &gpui::App) -> usize {
        self.model.read(cx).core().people().people.len()
    }

    fn page(&self) -> usize {
        let state = self.scroll.0.borrow();
        state
            .last_item_size
            .map(|size| (f32::from(size.item.height) / ROW_HEIGHT).floor() as usize)
            .filter(|rows| *rows > 0)
            .unwrap_or(FALLBACK_PAGE)
    }

    fn move_to(&mut self, index: usize, cx: &mut Context<Self>) {
        let count = self.count(cx);
        if count == 0 {
            return;
        }
        self.active = index.min(count - 1);
        self.scroll
            .scroll_to_item(self.active, ScrollStrategy::Nearest);
        if self.active + 1 == count {
            self.load_more(cx);
        }
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let target = (self.active as isize + delta).max(0) as usize;
        self.move_to(target, cx);
    }

    fn load_more(&self, cx: &mut Context<Self>) {
        let more = {
            let view = self.model.read(cx).core().people();
            view.has_more && matches!(view.status, LoadStatus::Ready)
        };
        if more {
            self.dispatch(Input::LoadMorePeople, cx);
        }
    }

    fn open(&mut self, index: usize, cx: &mut Context<Self>) {
        let user = self
            .model
            .read(cx)
            .core()
            .people()
            .people
            .get(index)
            .map(|person| person.user.id);
        if let Some(user) = user {
            cx.emit(PeopleEvent::OpenProfile(user));
        }
    }

    /// Asks again: for the next page when there are rows, else for the first with the text in
    /// the box (opening a directory that failed to load asks again). The keyboard goes to the
    /// search box, which is always there: the button it was on goes away.
    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.count(cx) > 0 {
            self.dispatch(Input::LoadMorePeople, cx);
        } else {
            self.dispatch(Input::OpenPeople, cx);
        }
        let focus = self.search.focus_handle(cx);
        window.focus(&focus, cx);
    }

    // ----- Drawing -----

    fn header(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let (can_manage, shown) = {
            let core = self.model.read(cx).core();
            (
                core.my_role()
                    .is_some_and(|role| role.can_manage_workspace()),
                core.people().include_deactivated,
            )
        };
        let toggle_to = cx.entity().downgrade();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .h(px(52.))
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(theme.type_scale.title)
                    .text_color(colors.text)
                    .child("People"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .max_w(px(420.))
                    .px_3()
                    .py_1()
                    .rounded(theme.radii.medium)
                    .bg(colors.surface_raised)
                    .border_1()
                    .border_color(if self.search.read(cx).is_focused(window) {
                        colors.focus_ring
                    } else {
                        colors.border_strong
                    })
                    .child(self.search.clone()),
            )
            .child(div().flex_1())
            .children(can_manage.then(|| {
                // The checkbox is as wide as its label (it aligns itself to the start): this keeps
                // it on the line with the title and the box.
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .child(form::checkbox(
                        theme,
                        "people-show-deactivated",
                        states::SHOW_DEACTIVATED,
                        shown,
                        &self.deactivated_focus,
                        move |value, _, cx| {
                            toggle_to
                                .update(cx, |view, cx| {
                                    view.active = 0;
                                    view.dispatch(Input::SetPeopleIncludeDeactivated(value), cx);
                                })
                                .ok();
                        },
                    ))
            }))
            // Room for the Close button, which sits over the right end.
            .child(div().w(px(76.)))
            .into_any_element()
    }

    fn rows_for(
        &mut self,
        range: std::ops::Range<usize>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = theme.colors;
        let this = cx.entity().downgrade();
        let rows: Vec<RowParts> = {
            let core = self.model.read(cx).core();
            let me = core.me().map(|user| user.id);
            range
                .filter_map(|index| {
                    core.people().people.get(index).map(|person| {
                        (
                            index,
                            person.user.id,
                            person.user.display_name.to_string(),
                            format!("@{}", person.user.username),
                            states::role_label(person.role),
                            Some(person.user.id) == me,
                            StatusMark::lookup(core, person.user.id),
                            person.user.deactivated,
                        )
                    })
                })
                .collect()
        };
        // The pictures of the people drawn here, if they are decoded; the rest are asked for.
        // The directory hands over each person with their avatar version, so the member list need
        // not hold them.
        let wanted: Vec<(lazy_domain::UserId, u32)> = {
            let core = self.model.read(cx).core();
            rows.iter()
                .map(|row| {
                    let version = core
                        .people()
                        .people
                        .get(row.0)
                        .map_or(0, |person| person.user.avatar_version);
                    (row.1, version)
                })
                .collect()
        };
        let images: Vec<Option<std::sync::Arc<gpui::RenderImage>>> = {
            let store = crate::image_store::ImageStore::global(cx);
            wanted
                .iter()
                .map(|(user, version)| {
                    crate::avatar_images::image_at(store.read(cx), *user, *version)
                })
                .collect()
        };
        crate::avatar_images::request_versions_later(&self.model, wanted, cx);
        rows.into_iter()
            .zip(images)
            .map(
                |((index, user, name, username, role, is_me, status, deactivated), image)| {
                    let active = self.active == index;
                    let to = this.clone();
                    let hover_to = this.clone();
                    let label = format!("{name}, {username}");
                    div()
                        .id(("people-row", index))
                        .debug_selector(move || format!("people-row-{index}"))
                        .role(Role::ListItem)
                        .aria_label(label)
                        .h(px(ROW_HEIGHT))
                        .w_full()
                        .px_3()
                        .py_0p5()
                        .rounded(theme.radii.medium)
                        .cursor_pointer()
                        .when(!active, |d| d.hover(move |s| s.bg(colors.surface_hover)))
                        .on_mouse_move(move |_, _, cx| {
                            hover_to
                                .update(cx, |view, cx| {
                                    if view.active != index {
                                        view.active = index;
                                        cx.notify();
                                    }
                                })
                                .ok();
                        })
                        .on_click(move |_, _, cx| {
                            to.update(cx, |view, cx| {
                                view.active = index;
                                cx.emit(PeopleEvent::OpenProfile(user));
                            })
                            .ok();
                        })
                        .child(
                            div()
                                .h_full()
                                .w_full()
                                .px_3()
                                .flex()
                                .items_center()
                                .gap_3()
                                .rounded(theme.radii.medium)
                                .border_1()
                                .border_color(if active {
                                    colors.focus_ring
                                } else {
                                    transparent_black()
                                })
                                .when(active, |d| d.bg(colors.selection))
                                .when(deactivated, |d| d.opacity(0.7))
                                .child(
                                    crate::avatar::Avatar::new(
                                        user.as_uuid().as_u128(),
                                        name.clone(),
                                        32.,
                                    )
                                    .text_size(theme.type_scale.body)
                                    .image(image)
                                    .into_div()
                                    .debug_selector(move || format!("people-avatar-{index}")),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .min_w_0()
                                                        .truncate()
                                                        .text_size(theme.type_scale.body)
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(if deactivated {
                                                            colors.text_muted
                                                        } else {
                                                            colors.text
                                                        })
                                                        .child(if is_me {
                                                            format!("{name} (you)")
                                                        } else {
                                                            name.clone()
                                                        }),
                                                )
                                                .children(status.as_ref().map(|mark| {
                                                    crate::status_mark::view(
                                                        gpui::ElementId::NamedInteger(
                                                            "people-status".into(),
                                                            index as u64,
                                                        ),
                                                        format!("people-status-{index}"),
                                                        mark,
                                                        theme.type_scale.body,
                                                    )
                                                })),
                                        )
                                        .child(
                                            div()
                                                .truncate()
                                                .text_size(theme.type_scale.small)
                                                .text_color(colors.text_muted)
                                                .child(username),
                                        ),
                                )
                                .children(deactivated.then(|| {
                                    div()
                                        .id(("people-deactivated", index))
                                        .debug_selector(move || {
                                            format!("people-deactivated-{index}")
                                        })
                                        .flex_none()
                                        .px_1p5()
                                        .rounded(theme.radii.small)
                                        .border_1()
                                        .border_color(colors.border_strong)
                                        .text_color(colors.text_muted)
                                        .text_size(theme.type_scale.small)
                                        .tooltip(|_, cx| {
                                            crate::tooltip::TextTooltip::view(
                                                states::DEACTIVATED_NOTE,
                                                cx,
                                            )
                                        })
                                        .child(states::DEACTIVATED)
                                }))
                                .children(role.map(|role| {
                                    div()
                                        .debug_selector(move || format!("people-role-{index}"))
                                        .flex_none()
                                        .px_1p5()
                                        .rounded(theme.radii.small)
                                        .bg(colors.accent_surface)
                                        .text_color(colors.accent)
                                        .text_size(theme.type_scale.small)
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(role)
                                })),
                        )
                        .into_any_element()
                },
            )
            .collect()
    }

    fn body(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let (status, count, has_more, query) = {
            let view = self.model.read(cx).core().people();
            (
                view.status.clone(),
                view.people.len(),
                view.has_more,
                view.query.clone(),
            )
        };
        let this = cx.entity().downgrade();
        let retry_button = |view: &Self, label: &'static str| {
            let to = this.clone();
            form::button(
                theme,
                "people-retry",
                label,
                ButtonKind::Primary,
                true,
                &view.retry_focus,
                move |window, cx| {
                    to.update(cx, |view, cx| view.retry(window, cx)).ok();
                },
            )
            .into_any_element()
        };
        match (&status, count) {
            (LoadStatus::Loading | LoadStatus::Idle, 0) => skeleton(theme),
            (LoadStatus::Failed { reason, can_retry }, 0) => note_block(
                theme,
                "people-failed",
                Some(states::PEOPLE_FAILED_TITLE),
                states::sentence(reason),
                can_retry.then(|| retry_button(self, states::TRY_AGAIN)),
            ),
            (LoadStatus::Ready, 0) => note_block(
                theme,
                "people-empty",
                None,
                states::people_no_match(&query),
                None,
            ),
            _ => {
                let list = uniform_list(
                    "people-items",
                    count,
                    cx.processor(
                        |this,
                         range: std::ops::Range<usize>,
                         _window: &mut Window,
                         cx: &mut Context<Self>| {
                            let theme = *Theme::get(cx);
                            this.rows_for(range, &theme, cx)
                        },
                    ),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0();
                let footer: Option<AnyElement> = match &status {
                    LoadStatus::Loading => Some(
                        div()
                            .debug_selector(|| "people-loading-more".into())
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child("Loading more people…")
                            .into_any_element(),
                    ),
                    LoadStatus::Failed { reason, can_retry } => Some(
                        div()
                            .debug_selector(|| "people-more-failed".into())
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.danger)
                                    .child(states::people_more_failed(reason)),
                            )
                            .children(can_retry.then(|| retry_button(self, states::TRY_AGAIN)))
                            .into_any_element(),
                    ),
                    LoadStatus::Ready | LoadStatus::Idle if has_more => {
                        let to = this.clone();
                        Some(
                            form::button(
                                theme,
                                "people-show-more",
                                states::SEARCH_SHOW_MORE,
                                ButtonKind::Secondary,
                                true,
                                &self.more_focus,
                                move |_, cx| {
                                    to.update(cx, |view, cx| {
                                        view.dispatch(Input::LoadMorePeople, cx)
                                    })
                                    .ok();
                                },
                            )
                            .into_any_element(),
                        )
                    }
                    _ => None,
                };
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("people-list")
                            .debug_selector(|| "people-list".into())
                            .role(Role::List)
                            .aria_label("People")
                            // Names and badges stay within reach of each other on a wide window.
                            .w_full()
                            .max_w(px(760.))
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(list),
                    )
                    .children(footer.map(|footer| {
                        div()
                            .flex_none()
                            .px_4()
                            .py_2()
                            .border_t_1()
                            .border_color(colors.border)
                            .child(footer)
                    }))
                    .into_any_element()
            }
        }
    }
}

/// The first page loading: rows the shape of people, so nothing moves when they arrive.
fn skeleton(theme: &Theme) -> AnyElement {
    let colors = theme.colors;
    div()
        .id("people-loading")
        .debug_selector(|| "people-loading".into())
        .role(Role::Status)
        .aria_label("Loading people")
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .children((0..SKELETON_ROWS).map(|index| {
            let wide = 160.0 + (index % 3) as f32 * 50.0;
            div()
                .h(px(ROW_HEIGHT))
                .w_full()
                .px_6()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .size(px(32.))
                        .flex_none()
                        .rounded(theme.radii.medium)
                        .bg(colors.border),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1p5()
                        .child(
                            div()
                                .h(px(11.))
                                .w(px(wide))
                                .rounded(theme.radii.small)
                                .bg(colors.border),
                        )
                        .child(
                            div()
                                .h(px(10.))
                                .w(px(90.))
                                .rounded(theme.radii.small)
                                .bg(colors.border),
                        ),
                )
        }))
        .into_any_element()
}

impl Focusable for PeopleDirectoryView {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.search.focus_handle(cx)
    }
}

impl Render for PeopleDirectoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let header = self.header(&theme, window, cx);
        let body = self.body(&theme, cx);
        let this = cx.entity().downgrade();
        let close = div()
            .absolute()
            .top(px(8.))
            .right(px(16.))
            .child(form::button(
                &theme,
                "people-close",
                "Close",
                ButtonKind::Secondary,
                true,
                &self.close_focus,
                move |_, cx| {
                    this.update(cx, |_, cx| cx.emit(PeopleEvent::Close)).ok();
                },
            ));
        div()
            .id("people-view")
            .debug_selector(|| "people-view".into())
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.surface)
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.step(-1, cx)))
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                    if event.keystroke.modifiers.modified() {
                        return;
                    }
                    match event.keystroke.key.as_str() {
                        "pagedown" => this.step(this.page() as isize, cx),
                        "pageup" => this.step(-(this.page() as isize), cx),
                        "escape" => cx.emit(PeopleEvent::Close),
                        _ => return,
                    }
                    cx.stop_propagation();
                }),
            )
            .child(header)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .child(close)
    }
}
