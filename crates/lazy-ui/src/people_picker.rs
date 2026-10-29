//! A people picker: a search box with selected people shown as chips, and matching people listed
//! underneath.
//!
//! Keyboard (the search box passes these through, see `TextInputOptions::pass_through_navigation`):
//! type to filter, Up and Down move the highlight, Enter adds the highlighted person
//! (or, with an empty search box, tells the owner to confirm), Backspace in an empty search box
//! removes the last chip. Tab and Shift+Tab are reported for the owning dialog to handle.

use gpui::{
    Context, ElementId, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::*, px,
};
use lazy_domain::UserId;

use crate::text_input::actions::{Backspace, MoveDown, MoveUp};
use crate::text_input::{TextInput, TextInputEvent, TextInputOptions};
use crate::theme::{Theme, avatar_color};

/// Rows of results shown at once.
const VISIBLE_RESULTS: usize = 6;

/// Someone who can be picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub id: UserId,
    pub name: SharedString,
    /// Secondary text, such as `@username`. Searched too.
    pub detail: SharedString,
    /// What they have said they are doing, shown beside the name.
    pub status: Option<crate::status_mark::StatusMark>,
}

/// What the picker reports to its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerEvent {
    /// The selection changed.
    Changed,
    /// Enter was pressed with nothing typed: the owner should confirm.
    Submit,
    /// Tab or Shift+Tab was pressed in the search box.
    Tab { backwards: bool },
}

/// Indices into `people` that match `query` and are not already selected: names that start with
/// the query first, then other matches, each group in name order.
pub fn matching(people: &[Person], selected: &[UserId], query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    let mut hits: Vec<(bool, usize)> = people
        .iter()
        .enumerate()
        .filter(|(_, person)| !selected.contains(&person.id))
        .filter_map(|(index, person)| {
            let name = person.name.to_lowercase();
            let detail = person.detail.to_lowercase();
            if needle.is_empty()
                || name.starts_with(&needle)
                || detail.trim_start_matches('@').starts_with(&needle)
            {
                Some((true, index))
            } else if name.contains(&needle) || detail.contains(&needle) {
                Some((false, index))
            } else {
                None
            }
        })
        .collect();
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            people[a.1]
                .name
                .to_lowercase()
                .cmp(&people[b.1].name.to_lowercase())
        })
    });
    hits.into_iter().map(|(_, index)| index).collect()
}

pub struct PeoplePicker {
    people: Vec<Person>,
    selected: Vec<UserId>,
    max: usize,
    query: Entity<TextInput>,
    highlighted: usize,
    _subscription: Subscription,
}

impl EventEmitter<PickerEvent> for PeoplePicker {}

impl PeoplePicker {
    pub fn new(
        people: Vec<Person>,
        max: usize,
        placeholder: &'static str,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new(placeholder)
                    .accessible_label("Search people")
                    .single_line(true)
                    .pass_through_navigation(true)
                    .max_bytes(200),
                cx,
            )
        });
        let subscription =
            cx.subscribe(
                &query,
                |this, _input, event: &TextInputEvent, cx| match event {
                    TextInputEvent::Changed => {
                        this.highlighted = 0;
                        cx.notify();
                    }
                    TextInputEvent::Submit => this.confirm(cx),
                    TextInputEvent::Tab { backwards } => cx.emit(PickerEvent::Tab {
                        backwards: *backwards,
                    }),
                    TextInputEvent::UpWhenEmpty
                    | TextInputEvent::PasteAttachments
                    | TextInputEvent::Suggestion(_) => {}
                },
            );
        Self {
            people,
            selected: Vec::new(),
            max,
            query,
            highlighted: 0,
            _subscription: subscription,
        }
    }

    /// Replaces the selection, dropping ids that are not in the list or exceed the maximum.
    pub fn set_selected(&mut self, ids: Vec<UserId>, cx: &mut Context<Self>) {
        self.selected = ids
            .into_iter()
            .filter(|id| self.people.iter().any(|p| p.id == *id))
            .take(self.max)
            .collect();
        cx.notify();
    }

    pub fn selected(&self) -> &[UserId] {
        &self.selected
    }

    /// The search box, for focusing and for the owner's tab order.
    pub fn input(&self) -> &Entity<TextInput> {
        &self.query
    }

    pub fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.query.focus_handle(cx)
    }

    fn results(&self, cx: &gpui::App) -> Vec<usize> {
        matching(&self.people, &self.selected, self.query.read(cx).text())
    }

    fn add(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(person) = self.people.get(index) else {
            return;
        };
        if self.selected.len() >= self.max || self.selected.contains(&person.id) {
            return;
        }
        self.selected.push(person.id);
        self.query.update(cx, |input, cx| input.clear(cx));
        self.highlighted = 0;
        cx.emit(PickerEvent::Changed);
        cx.notify();
    }

    fn remove(&mut self, id: UserId, cx: &mut Context<Self>) {
        if let Some(position) = self.selected.iter().position(|selected| *selected == id) {
            self.selected.remove(position);
            cx.emit(PickerEvent::Changed);
            cx.notify();
        }
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let typed = !self.query.read(cx).text().trim().is_empty();
        if !typed {
            cx.emit(PickerEvent::Submit);
            return;
        }
        let results = self.results(cx);
        if let Some(index) = results.get(self.highlighted.min(results.len().saturating_sub(1))) {
            self.add(*index, cx);
        }
    }

    fn move_highlight(&mut self, forwards: bool, cx: &mut Context<Self>) {
        let count = self.results(cx).len().min(VISIBLE_RESULTS);
        if count == 0 {
            return;
        }
        self.highlighted = if forwards {
            (self.highlighted + 1) % count
        } else {
            (self.highlighted + count - 1) % count
        };
        cx.notify();
    }

    /// Backspace in an empty search box removes the last chip.
    fn remove_last(&mut self, cx: &mut Context<Self>) {
        if self.query.read(cx).text().is_empty() && self.selected.pop().is_some() {
            cx.emit(PickerEvent::Changed);
            cx.notify();
        }
    }
}

impl Render for PeoplePicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let colors = theme.colors;
        let focused = self.query.read(cx).is_focused(window);
        let results: Vec<usize> = self.results(cx).into_iter().take(VISIBLE_RESULTS).collect();
        let highlighted = self.highlighted.min(results.len().saturating_sub(1));
        let typed = !self.query.read(cx).text().trim().is_empty();
        let full = self.selected.len() >= self.max;

        let mut chips = div().flex().flex_wrap().items_center().gap_1p5();
        for id in &self.selected {
            let Some(person) = self.people.iter().find(|p| p.id == *id) else {
                continue;
            };
            let id = *id;
            let name = format!("picker-chip-{id}");
            let remove = format!("picker-remove-{id}");
            chips = chips.child(
                div()
                    .id(ElementId::Name(name.clone().into()))
                    .debug_selector(move || name)
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl_2()
                    .pr_1()
                    .py_0p5()
                    .rounded(theme.radii.small)
                    .bg(colors.surface_hover)
                    .border_1()
                    .border_color(colors.border_strong)
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text)
                    .child(person.name.clone())
                    .child(
                        div()
                            .id(ElementId::Name(remove.clone().into()))
                            .debug_selector(move || remove)
                            .cursor_pointer()
                            .px_1()
                            .rounded(theme.radii.small)
                            .text_color(colors.text_muted)
                            .hover(move |style| style.bg(colors.border).text_color(colors.text))
                            .on_click(cx.listener(move |this, _, _, cx| this.remove(id, cx)))
                            .child("×"),
                    ),
            );
        }
        chips = chips.child(div().flex_1().min_w(px(120.)).child(self.query.clone()));

        let mut list = div().flex().flex_col();
        for (row, index) in results.iter().enumerate() {
            let person = &self.people[*index];
            let index = *index;
            let name = format!("picker-result-{}", person.id);
            list = list.child(
                div()
                    .id(ElementId::Name(name.clone().into()))
                    .debug_selector(move || name)
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(theme.radii.small)
                    .cursor_pointer()
                    .when(row == highlighted, |item| item.bg(colors.surface_hover))
                    .hover(move |style| style.bg(colors.surface_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.add(index, cx)))
                    .child(
                        div()
                            .size(px(24.))
                            .flex_none()
                            .rounded(theme.radii.small)
                            .bg(avatar_color(person.id.as_uuid().as_u128()))
                            .text_color(gpui::white())
                            .text_size(theme.type_scale.small)
                            .font_weight(FontWeight::SEMIBOLD)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                person
                                    .name
                                    .chars()
                                    .next()
                                    .unwrap_or('?')
                                    .to_uppercase()
                                    .to_string(),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.type_scale.body)
                            .text_color(colors.text)
                            .child(person.name.clone()),
                    )
                    .children(person.status.as_ref().map(|mark| {
                        crate::status_mark::view(
                            ElementId::Name(format!("person-status-{}", person.detail).into()),
                            format!("person-status-{}", person.detail.trim_start_matches('@')),
                            mark,
                            theme.type_scale.small,
                        )
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.type_scale.small)
                            .text_color(colors.text_muted)
                            .child(person.detail.clone()),
                    ),
            );
        }
        if results.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_size(theme.type_scale.small)
                    .text_color(colors.text_muted)
                    .child(if typed {
                        "No one matches that."
                    } else {
                        "Everyone is already added."
                    }),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap_2()
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.move_highlight(true, cx)))
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.move_highlight(false, cx)))
            .on_action(cx.listener(|this, _: &Backspace, _, cx| this.remove_last(cx)))
            .child(
                div()
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radii.medium)
                    .bg(colors.surface_raised)
                    .border_1()
                    .border_color(if focused {
                        colors.focus_ring
                    } else {
                        colors.border_strong
                    })
                    .child(chips),
            )
            .when(full, |picker| {
                picker.child(
                    div()
                        .text_size(theme.type_scale.small)
                        .text_color(colors.text_muted)
                        .child(format!(
                            "That's the most people one conversation can hold ({}).",
                            self.max + 1
                        )),
                )
            })
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(name: &str, detail: &str) -> Person {
        Person {
            id: UserId::generate(),
            name: name.to_owned().into(),
            detail: detail.to_owned().into(),
            status: None,
        }
    }

    #[test]
    fn empty_queries_list_everyone_in_name_order() {
        let people = vec![
            person("Zoë", "@zoe"),
            person("Alice", "@alice"),
            person("Bob", "@bob"),
        ];
        let names: Vec<_> = matching(&people, &[], "")
            .iter()
            .map(|i| people[*i].name.to_string())
            .collect();
        assert_eq!(names, ["Alice", "Bob", "Zoë"]);
    }

    #[test]
    fn prefix_matches_come_before_substring_matches_and_selected_people_are_hidden() {
        let people = vec![
            person("Carol Albright", "@carol"),
            person("Al Jones", "@al"),
            person("Alice", "@alice"),
        ];
        let hits = matching(&people, &[], "al");
        let names: Vec<_> = hits.iter().map(|i| people[*i].name.to_string()).collect();
        assert_eq!(names, ["Al Jones", "Alice", "Carol Albright"]);
        let hidden = matching(&people, &[people[1].id], "al");
        assert!(hidden.iter().all(|i| *i != 1));
    }

    #[test]
    fn usernames_match_with_or_without_the_at_sign() {
        let people = vec![person("Robert", "@bob")];
        assert_eq!(matching(&people, &[], "bob"), vec![0]);
        assert_eq!(matching(&people, &[], "@bo"), vec![0]);
        assert!(matching(&people, &[], "zzz").is_empty());
    }
}
