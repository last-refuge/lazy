//! The virtualized message timeline.
//!
//! One [`ListState`] is kept per conversation, so switching conversations restores each one's
//! scroll position. The core describes row-list changes precisely (`Change::TimelineSplice` and
//! friends) and this view mirrors them into the list state; rows are drawn from the core's
//! current data. No I/O or parsing happens while rendering.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    App, Context, Entity, FocusHandle, FollowMode, IntoElement, ListAlignment, ListState, Pixels,
    Render, SharedString, Subscription, Window, div, list, prelude::*, px,
};
use jiff::Zoned;
use jiff::tz::TimeZone;
use lazy_client_core::{Change, Delivery, HistoryStatus, Input, ItemKey};
use lazy_domain::{ConversationId, ConversationKind};

use crate::app_model::{AppEvent, AppModel};
use crate::message_row::{RetryHandler, RowProps, render_message_row, row_layout};
use crate::theme::Theme;

/// Rows rendered beyond the viewport on each side, in pixels. Bounds the mounted rows.
const OVERDRAW: f32 = 600.0;
/// Request older history when the first visible row is within this many rows of the top.
///
/// This prefetches: a page is 50 rows, a single flick can cover about 18, and the range a scroll
/// handler reports trails the layout by one event, so a small threshold could be skipped over
/// entirely and leave the reader stuck at the top with nothing loading.
const LOAD_OLDER_THRESHOLD: usize = 30;

/// A row to put back where it was after older history was prepended.
///
/// GPUI keeps the same row anchored when rows are inserted above it, but the old first row also
/// loses its day separator and author header once it has a predecessor, so it changes height and
/// everything below it would jump. The render after the splice lays out and measures the new
/// rows; the render after that can read where the watched row ended up and scroll by the
/// difference. Scroll offset is view geometry, not I/O, and nothing is dispatched here.
struct PendingRestore {
    conversation: ConversationId,
    /// Index of the watched row after the splice.
    item: usize,
    /// Where the row was before the splice, in window coordinates.
    y_before: Pixels,
    /// Renders seen since the splice.
    renders: u8,
}

/// Renders to wait for the new rows to be measured before giving up on a restore.
const RESTORE_GIVE_UP_AFTER: u8 = 4;

pub struct TimelineView {
    model: Entity<AppModel>,
    lists: HashMap<ConversationId, ListState>,
    restore: Option<PendingRestore>,
    /// The "New messages" row of each conversation as last drawn, to remeasure when it moves.
    markers: HashMap<ConversationId, Option<ItemKey>>,
    /// What was last reported to the core about whether each list is at the bottom.
    bottom_reported: HashMap<ConversationId, Rc<Cell<Option<bool>>>>,
    /// Tab stops for the retry buttons of failed messages, by message identity.
    retry_focus: Rc<RefCell<HashMap<ItemKey, FocusHandle>>>,
    zone: TimeZone,
    _subscription: Subscription,
}

impl TimelineView {
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&model, |this, _model, event: &AppEvent, cx| {
            let AppEvent::Changed(changes) = event;
            this.apply(changes, cx);
        });
        Self {
            model,
            lists: HashMap::new(),
            restore: None,
            markers: HashMap::new(),
            bottom_reported: HashMap::new(),
            retry_focus: Rc::default(),
            zone: TimeZone::system(),
            _subscription: subscription,
        }
    }

    /// Mirrors the core's row-list changes into the matching list state.
    fn apply(&mut self, changes: &[Change], cx: &mut Context<Self>) {
        let model = self.model.read(cx);
        let mut restore = None;
        for change in changes {
            match *change {
                Change::TimelineReset { conversation } => {
                    if let Some(state) = self.lists.get(&conversation) {
                        state.reset(model.core().timeline(conversation).len());
                    }
                }
                Change::TimelineSplice {
                    conversation,
                    at,
                    removed,
                    inserted,
                } => {
                    if let Some(state) = self.lists.get(&conversation) {
                        // Older history prepended above a reader who is at the top: remember
                        // where the next row sits so it can be put back after relayout.
                        let watch = (at == 0
                            && removed == 0
                            && inserted > 0
                            && state.logical_scroll_top().item_ix == 0)
                            .then(|| state.bounds_for_item(1).map(|bounds| bounds.origin.y))
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
                                conversation,
                                item: 1 + inserted,
                                y_before,
                                renders: 0,
                            });
                        }
                    }
                }
                Change::TimelineItemUpdated {
                    conversation,
                    index,
                } => {
                    if let Some(state) = self.lists.get(&conversation) {
                        let count = state.item_count();
                        let end = (index + 2).min(count);
                        if index < end {
                            state.remeasure_items(index..end);
                        }
                    }
                }
                _ => {}
            }
        }
        if restore.is_some() {
            self.restore = restore;
        }
        let selection_changed = changes
            .iter()
            .any(|c| matches!(c, Change::SelectionChanged));
        if let Some(selected) = self.model.read(cx).core().selected_conversation() {
            if selection_changed {
                self.report_bottom_again(selected, cx);
            }
            self.sync_marker(selected, cx);
        }
        cx.notify();
    }

    /// The core forgets a conversation's on-screen report when the person leaves it, so tell it
    /// again, from the list's current state, when they come back.
    fn report_bottom_again(&mut self, conversation: ConversationId, cx: &mut Context<Self>) {
        let (Some(state), Some(reported)) = (
            self.lists.get(&conversation),
            self.bottom_reported.get(&conversation),
        ) else {
            return; // A list created on the next render reports for itself.
        };
        let at_bottom = state.is_following_tail();
        reported.set(Some(at_bottom));
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::TimelineAtBottom {
                    conversation,
                    at_bottom,
                },
                cx,
            );
        });
    }

    /// Remeasures the rows that gained or lost the "New messages" divider.
    fn sync_marker(&mut self, conversation: ConversationId, cx: &mut Context<Self>) {
        let marker = self.model.read(cx).core().unread_marker(conversation);
        let previous = self.markers.insert(conversation, marker).flatten();
        if previous == marker {
            return;
        }
        let Some(state) = self.lists.get(&conversation) else {
            return;
        };
        let items = self.model.read(cx).core().timeline(conversation);
        for key in [previous, marker].into_iter().flatten() {
            if let Some(index) = items.iter().position(|item| item.key == key) {
                state.remeasure_items(index..index + 1);
            }
        }
    }

    /// Advances a pending position restore by one render. See [`PendingRestore`].
    fn advance_restore(&mut self, conversation: ConversationId, cx: &mut Context<Self>) {
        let Some(restore) = self.restore.as_mut() else {
            return;
        };
        if restore.conversation != conversation {
            return;
        }
        restore.renders += 1;
        let Some(state) = self.lists.get(&conversation) else {
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

    fn list_state(&mut self, conversation: ConversationId, cx: &mut Context<Self>) -> ListState {
        if let Some(state) = self.lists.get(&conversation) {
            return state.clone();
        }
        let count = self.model.read(cx).core().timeline(conversation).len();
        let state = ListState::new(count, ListAlignment::Bottom, px(OVERDRAW));
        state.set_follow_mode(FollowMode::Tail);

        let model = self.model.clone();
        // A new list starts at the newest message, so that is what the core is told.
        let reported = Rc::new(Cell::new(Some(true)));
        self.bottom_reported.insert(conversation, reported.clone());
        {
            let model = model.clone();
            cx.defer(move |cx| {
                model.update(cx, |model, cx| {
                    model.dispatch(
                        Input::TimelineAtBottom {
                            conversation,
                            at_bottom: true,
                        },
                        cx,
                    );
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
                        model.dispatch(
                            Input::TimelineAtBottom {
                                conversation,
                                at_bottom,
                            },
                            cx,
                        );
                    });
                });
            }
            if event.visible_range.start > LOAD_OLDER_THRESHOLD {
                return;
            }
            let wants_older = matches!(
                model.read(cx).core().history_status(conversation),
                HistoryStatus::Ready { has_more: true }
            );
            if wants_older {
                // The list state is borrowed while its scroll handler runs, and loading history
                // splices it, so run the request after the handler returns.
                let model = model.clone();
                cx.defer(move |cx| {
                    model.update(cx, |model, cx| {
                        model.dispatch(Input::LoadOlder { conversation }, cx);
                    });
                });
            }
        });
        self.lists.insert(conversation, state.clone());
        state
    }

    fn empty_state(&self, text: impl Into<SharedString>, theme: &Theme) -> gpui::AnyElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .px_8()
            .text_size(theme.type_scale.body)
            .text_color(theme.colors.text_muted)
            .child(text.into())
            .into_any_element()
    }
}

impl Render for TimelineView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let model = self.model.clone();
        let (selected, count, status, start_of_history, marker) = {
            let core = model.read(cx).core();
            let selected = core.selected_conversation();
            (
                selected,
                selected.map_or(0, |id| core.timeline(id).len()),
                selected.map(|id| core.history_status(id).clone()),
                selected.and_then(|id| {
                    let conversation = core.conversation(id)?;
                    let title = core.conversation_title(id);
                    Some(match conversation.kind {
                        ConversationKind::Channel if conversation.archived => format!(
                            "This is the beginning of #{title}. It is archived, so it is read-only."
                        ),
                        ConversationKind::Channel => {
                            format!("This is the beginning of #{title}. Say hello!")
                        }
                        ConversationKind::Direct => {
                            format!("This is the start of your conversation with {title}. Say hello!")
                        }
                        ConversationKind::Group => {
                            format!("This is the start of your group conversation with {title}. Say hello!")
                        }
                    })
                }),
                selected.and_then(|id| core.unread_marker(id)),
            )
        };

        let Some(conversation) = selected else {
            return self.empty_state("Pick a conversation to start.", &theme);
        };
        if count == 0 {
            let text = match status {
                Some(HistoryStatus::Failed { message }) => {
                    format!("Couldn't load messages: {message}")
                }
                Some(HistoryStatus::Ready { .. }) => {
                    start_of_history.unwrap_or_else(|| "Say hello!".to_owned())
                }
                _ => "Loading messages…".to_owned(),
            };
            return self.empty_state(text, &theme);
        }

        let state = self.list_state(conversation, cx);
        self.advance_restore(conversation, cx);
        let zone = self.zone.clone();
        let retry_handles = self.retry_focus.clone();
        let now = Zoned::now();

        let rows = list(state, move |index, _window, cx: &mut App| {
            let model_ref = model.read(cx);
            let core = model_ref.core();
            let items = core.timeline(conversation);
            let Some(item) = items.get(index) else {
                return div().into_any_element();
            };
            let previous = index.checked_sub(1).and_then(|prev| items.get(prev));
            let layout = row_layout(previous, item, &zone, &now);
            let author_name: SharedString = core.display_name(item.author_id).to_owned().into();

            let retry_model = model.clone();
            let operation = match item.key {
                ItemKey::Operation(operation) => Some(operation),
                ItemKey::Message(_) => None,
            };
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
                retry_handles
                    .borrow_mut()
                    .entry(item.key)
                    .or_insert_with(|| cx.focus_handle().tab_stop(true))
                    .clone()
            });
            render_message_row(RowProps {
                new_messages_divider: marker == Some(item.key),
                retry_focus,
                index,
                item,
                author_name,
                layout,
                zone: &zone,
                now: &now,
                theme: Theme::get(cx),
                on_retry,
            })
        })
        .size_full();

        div().size_full().child(rows).into_any_element()
    }
}
