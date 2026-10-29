//! Behavior of the standalone UI components (dialog, menu, people picker, segmented toggle) on
//! GPUI's test platform, driven by real key presses and clicks.
//!
//! The test platform gives every character the same width and has no real input method, so these
//! prove command routing, focus behavior, and event semantics, not real glyph layout or IME.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    AppContext as _, Context, Entity, EntityInputHandler as _, FocusHandle, Focusable,
    InteractiveElement, IntoElement, Modifiers, Render, TestAppContext, VisualTestContext, Window,
    WindowAppearance, div, prelude::*, px,
};
use lazy_domain::UserId;
use lazy_ui::form::{self, ButtonKind, Segment};
use lazy_ui::keyboard::{self, FocusNextControl, FocusPrevControl};
use lazy_ui::menu::{Menu, MenuEvent, MenuItem};
use lazy_ui::modal::{
    DialogDismiss, DialogEvent, DialogNext, DialogPrev, ModalTrap, dialog_overlay,
};
use lazy_ui::people_picker::{PeoplePicker, Person, PickerEvent};
use lazy_ui::text_input::{TextInput, TextInputEvent, TextInputOptions};
use lazy_ui::theme::Theme;

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| lazy_ui::init(cx, WindowAppearance::Light));
}

fn is_focused(cx: &mut VisualTestContext, handle: &FocusHandle) -> bool {
    cx.update(|window, _| handle.is_focused(window))
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("nothing labelled {selector:?} is on screen"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

// ---- Dialog ----

/// A dialog with one single-line input and two buttons, built only from the library pieces.
struct DemoDialog {
    trap: ModalTrap,
    input: Entity<TextInput>,
    cancel: FocusHandle,
    save: FocusHandle,
    saves: Rc<Cell<u32>>,
    _subscription: gpui::Subscription,
}

impl gpui::EventEmitter<DialogEvent> for DemoDialog {}

impl DemoDialog {
    fn new(saves: Rc<Cell<u32>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut trap = ModalTrap::new(cx);
        let input = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new("Name")
                    .single_line(true)
                    .accessible_label("Name"),
                cx,
            )
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &TextInputEvent, window, cx| {
                if let TextInputEvent::Tab { backwards } = event {
                    this.trap.tab(*backwards, window, cx);
                }
            },
        );
        let initial = input.focus_handle(cx);
        trap.open(&initial, window, cx);
        Self {
            trap,
            input,
            cancel: cx.focus_handle().tab_stop(true),
            save: cx.focus_handle().tab_stop(true),
            saves,
            _subscription: subscription,
        }
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !ModalTrap::dismiss_allowed(self.input.read(cx).is_composing()) {
            return;
        }
        self.trap.close(window, cx);
        cx.emit(DialogEvent::Dismiss);
    }
}

impl Render for DemoDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let this = cx.entity().downgrade();
        let saves = self.saves.clone();
        let cancel_to = this.clone();
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.input.clone())
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(form::button(
                        &theme,
                        "demo-cancel",
                        "Cancel",
                        ButtonKind::Secondary,
                        true,
                        &self.cancel,
                        move |window, cx| {
                            cancel_to
                                .update(cx, |dialog, cx| dialog.dismiss(window, cx))
                                .ok();
                        },
                    ))
                    .child(form::button(
                        &theme,
                        "demo-save",
                        "Save",
                        ButtonKind::Primary,
                        true,
                        &self.save,
                        move |_, _| {
                            saves.set(saves.get() + 1);
                        },
                    )),
            )
            .into_any_element();
        let on_close = {
            let this = this.clone();
            move |window: &mut Window, cx: &mut gpui::App| {
                this.update(cx, |dialog, cx| dialog.dismiss(window, cx))
                    .ok();
            }
        };
        dialog_overlay(
            &theme,
            "Demo dialog",
            360.,
            &self.trap,
            body,
            "Close",
            on_close,
        )
        .on_action(cx.listener(|this, _: &DialogDismiss, window, cx| this.dismiss(window, cx)))
        .on_action(cx.listener(|this, _: &DialogNext, window, cx| this.trap.tab(false, window, cx)))
        .on_action(cx.listener(|this, _: &DialogPrev, window, cx| this.trap.tab(true, window, cx)))
    }
}

/// A window with a focusable background button and, when open, a dialog above it.
struct DialogHost {
    background: FocusHandle,
    background_activations: Rc<Cell<u32>>,
    saves: Rc<Cell<u32>>,
    dialog: Option<Entity<DemoDialog>>,
    _subscription: Option<gpui::Subscription>,
}

impl DialogHost {
    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let saves = self.saves.clone();
        let dialog = cx.new(|cx| DemoDialog::new(saves, window, cx));
        self._subscription = Some(cx.subscribe(
            &dialog,
            |this, _dialog, event: &DialogEvent, cx| {
                let DialogEvent::Dismiss = event;
                this.dialog = None;
                cx.notify();
            },
        ));
        self.dialog = Some(dialog);
        cx.notify();
    }
}

impl Render for DialogHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let activations = self.background_activations.clone();
        div()
            .size_full()
            .relative()
            .key_context(keyboard::SHELL_CONTEXT)
            .on_action(cx.listener(|_, _: &FocusNextControl, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &FocusPrevControl, window, cx| window.focus_prev(cx)))
            .bg(theme.colors.surface)
            .child(div().p_4().child(form::button(
                &theme,
                "background-button",
                "Background",
                ButtonKind::Secondary,
                true,
                &self.background,
                move |_, _| activations.set(activations.get() + 1),
            )))
            .children(self.dialog.clone())
    }
}

struct DialogFixture {
    host: Entity<DialogHost>,
    saves: Rc<Cell<u32>>,
    activations: Rc<Cell<u32>>,
    background: FocusHandle,
}

fn dialog_host(cx: &mut TestAppContext) -> (DialogFixture, &mut VisualTestContext) {
    init(cx);
    let saves = Rc::new(Cell::new(0));
    let activations = Rc::new(Cell::new(0));
    let (host, cx) = cx.add_window_view({
        let (saves, activations) = (saves.clone(), activations.clone());
        move |_, cx| DialogHost {
            background: cx.focus_handle().tab_stop(true),
            background_activations: activations,
            saves,
            dialog: None,
            _subscription: None,
        }
    });
    let background = host.read_with(cx, |host, _| host.background.clone());
    (
        DialogFixture {
            host,
            saves,
            activations,
            background,
        },
        cx,
    )
}

impl DialogFixture {
    fn open(&self, cx: &mut VisualTestContext) {
        self.host
            .update_in(cx, |host, window, cx| host.open(window, cx));
        cx.run_until_parked();
    }

    fn dialog(&self, cx: &mut VisualTestContext) -> Option<Entity<DemoDialog>> {
        self.host.read_with(cx, |host, _| host.dialog.clone())
    }
}

#[gpui::test]
fn opening_a_dialog_focuses_its_first_input(cx: &mut TestAppContext) {
    let (fixture, cx) = dialog_host(cx);
    cx.update(|window, app| window.focus(&fixture.background, app));
    fixture.open(cx);
    let dialog = fixture.dialog(cx).expect("the dialog opened");
    let input = dialog.read_with(cx, |dialog, _| dialog.input.clone());
    let focus = input.read_with(cx, |input, app| input.focus_handle(app));
    assert!(is_focused(cx, &focus), "the input has focus");
}

#[gpui::test]
fn tab_stays_inside_the_dialog_and_wraps_in_both_directions(cx: &mut TestAppContext) {
    let (fixture, cx) = dialog_host(cx);
    cx.update(|window, app| window.focus(&fixture.background, app));
    fixture.open(cx);
    let dialog = fixture.dialog(cx).unwrap();
    let (input, cancel, save) = dialog.read_with(cx, |d, app| {
        (
            d.input.read(app).focus_handle(app),
            d.cancel.clone(),
            d.save.clone(),
        )
    });

    // input -> cancel -> save -> (wrap) input -> cancel ...
    for expected in [&cancel, &save, &input, &cancel] {
        cx.simulate_keystrokes("tab");
        assert!(is_focused(cx, expected), "forward order");
        assert!(
            !is_focused(cx, &fixture.background),
            "focus never reaches the window behind"
        );
    }
    // backwards from cancel -> input -> (wrap) save -> cancel
    for expected in [&input, &save, &cancel] {
        cx.simulate_keystrokes("shift-tab");
        assert!(is_focused(cx, expected), "backward order");
        assert!(!is_focused(cx, &fixture.background));
    }
}

#[gpui::test]
fn escape_dismisses_and_focus_returns_to_where_it_was(cx: &mut TestAppContext) {
    let (fixture, cx) = dialog_host(cx);
    cx.update(|window, app| window.focus(&fixture.background, app));
    fixture.open(cx);
    assert!(fixture.dialog(cx).is_some());
    assert!(!is_focused(cx, &fixture.background));
    cx.simulate_keystrokes("escape");
    assert!(fixture.dialog(cx).is_none(), "Escape closes the dialog");
    assert!(
        is_focused(cx, &fixture.background),
        "focus is restored to the previous control"
    );
}

#[gpui::test]
fn escape_during_an_input_method_composition_belongs_to_the_composition(cx: &mut TestAppContext) {
    let (fixture, cx) = dialog_host(cx);
    fixture.open(cx);
    let dialog = fixture.dialog(cx).unwrap();
    let input = dialog.read_with(cx, |d, _| d.input.clone());
    input.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "かな", None, window, cx)
    });
    cx.run_until_parked();
    assert!(input.read_with(cx, |i, _| i.is_composing()));
    cx.simulate_keystrokes("escape");
    assert!(
        fixture.dialog(cx).is_some(),
        "the dialog stays open while composing"
    );
}

#[gpui::test]
fn typing_and_enter_inside_the_dialog_edit_the_input_and_do_not_dismiss(cx: &mut TestAppContext) {
    let (fixture, cx) = dialog_host(cx);
    fixture.open(cx);
    cx.simulate_input("héllo 🎉");
    cx.simulate_keystrokes("enter");
    let dialog = fixture
        .dialog(cx)
        .expect("Enter in a field does not close the dialog");
    let text = dialog.read_with(cx, |d, app| d.input.read(app).text().to_owned());
    assert_eq!(text, "héllo 🎉");
}

#[gpui::test]
fn clicking_the_backdrop_dismisses_but_clicking_the_card_does_not(cx: &mut TestAppContext) {
    let (fixture, cx) = dialog_host(cx);
    fixture.open(cx);
    // Click inside the card, on its title area.
    let card = {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx.debug_bounds("dialog-card")
            .expect("the card is on screen")
    };
    cx.simulate_click(card.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        fixture.dialog(cx).is_some(),
        "a click in the card keeps the dialog open"
    );
    // Click on the backdrop, away from the card.
    cx.simulate_click(gpui::point(px(900.), px(700.)), Modifiers::default());
    cx.run_until_parked();
    assert!(
        fixture.dialog(cx).is_none(),
        "a click on the backdrop dismisses"
    );
}

#[gpui::test]
fn the_backdrop_blocks_clicks_to_controls_behind_it(cx: &mut TestAppContext) {
    let (fixture, cx) = dialog_host(cx);
    fixture.open(cx);
    let behind = cx.debug_bounds("background-button");
    // Whether or not the covered button reports bounds, a click where it sits must not activate it.
    let point = behind.map_or(gpui::point(px(40.), px(30.)), |b| b.center());
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        fixture.activations.get(),
        0,
        "the covered button was not activated"
    );
}

#[gpui::test]
fn dialog_buttons_work_with_the_keyboard(cx: &mut TestAppContext) {
    let (fixture, cx) = dialog_host(cx);
    fixture.open(cx);
    cx.simulate_keystrokes("tab tab enter"); // input -> cancel -> save, activate Save
    assert_eq!(fixture.saves.get(), 1, "Enter on Save activates it");
    cx.simulate_keystrokes("shift-tab enter"); // back to Cancel, activate it
    assert!(fixture.dialog(cx).is_none(), "Enter on Cancel dismisses");
    click(cx, "background-button");
    assert_eq!(
        fixture.activations.get(),
        1,
        "the background is live again once the dialog is gone"
    );
}

// ---- Menu ----

struct MenuHost {
    trigger: FocusHandle,
    menu: Option<Entity<Menu>>,
    events: Rc<RefCell<Vec<MenuEvent>>>,
    _subscription: Option<gpui::Subscription>,
}

impl MenuHost {
    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let menu = cx.new(|cx| {
            Menu::new(
                "test-menu",
                vec![
                    MenuItem::new("create", "Create channel"),
                    MenuItem::new("hidden", "Disabled item").enabled(false),
                    MenuItem::new("browse", "Browse channels").detail("Find one to join"),
                ],
                cx,
            )
        });
        self._subscription = Some(cx.subscribe(&menu, |this, _menu, event: &MenuEvent, cx| {
            this.events.borrow_mut().push(event.clone());
            this.menu = None;
            cx.notify();
        }));
        menu.update(cx, |menu, cx| menu.open(window, cx));
        self.menu = Some(menu);
        cx.notify();
    }
}

impl Render for MenuHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        div().size_full().bg(theme.colors.surface).p_4().child(
            div()
                .relative()
                .child(form::button(
                    &theme,
                    "menu-trigger",
                    "+",
                    ButtonKind::Secondary,
                    true,
                    &self.trigger,
                    |_, _| {},
                ))
                .children(self.menu.clone().map(|menu| {
                    gpui::deferred(div().absolute().top(px(34.)).left_0().child(menu))
                })),
        )
    }
}

fn menu_host(
    cx: &mut TestAppContext,
) -> (
    Entity<MenuHost>,
    Rc<RefCell<Vec<MenuEvent>>>,
    &mut VisualTestContext,
) {
    init(cx);
    let events = Rc::new(RefCell::new(Vec::new()));
    let (host, cx) = cx.add_window_view({
        let events = events.clone();
        move |_, cx| MenuHost {
            trigger: cx.focus_handle().tab_stop(true),
            menu: None,
            events,
            _subscription: None,
        }
    });
    (host, events, cx)
}

#[gpui::test]
fn the_menu_is_driven_from_the_keyboard_and_skips_disabled_items(cx: &mut TestAppContext) {
    let (host, events, cx) = menu_host(cx);
    let trigger = host.read_with(cx, |h, _| h.trigger.clone());
    cx.update(|window, app| window.focus(&trigger, app));
    host.update_in(cx, |h, window, cx| h.open(window, cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("down"); // skips the disabled item
    cx.simulate_keystrokes("enter");
    assert_eq!(*events.borrow(), vec![MenuEvent::Select("browse".into())]);
    assert!(
        is_focused(cx, &trigger),
        "focus returns to the trigger after a choice"
    );
}

#[gpui::test]
fn escape_and_clicking_outside_dismiss_the_menu(cx: &mut TestAppContext) {
    let (host, events, cx) = menu_host(cx);
    let trigger = host.read_with(cx, |h, _| h.trigger.clone());
    cx.update(|window, app| window.focus(&trigger, app));
    host.update_in(cx, |h, window, cx| h.open(window, cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("escape");
    assert_eq!(*events.borrow(), vec![MenuEvent::Dismiss]);
    assert!(is_focused(cx, &trigger));

    host.update_in(cx, |h, window, cx| h.open(window, cx));
    cx.run_until_parked();
    cx.simulate_click(gpui::point(px(700.), px(500.)), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        events.borrow().last(),
        Some(&MenuEvent::Dismiss),
        "a click elsewhere dismisses"
    );
}

#[gpui::test]
fn clicking_a_menu_item_selects_it_and_disabled_items_do_nothing(cx: &mut TestAppContext) {
    let (host, events, cx) = menu_host(cx);
    host.update_in(cx, |h, window, cx| h.open(window, cx));
    cx.run_until_parked();
    click(cx, "test-menu-hidden");
    assert!(
        events.borrow().is_empty(),
        "a disabled item cannot be chosen"
    );
    click(cx, "test-menu-create");
    assert_eq!(*events.borrow(), vec![MenuEvent::Select("create".into())]);
}

// ---- People picker ----

struct PickerHost {
    picker: Entity<PeoplePicker>,
    _subscription: gpui::Subscription,
}

impl Render for PickerHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        div()
            .size_full()
            .bg(theme.colors.surface)
            .p_4()
            .child(self.picker.clone())
    }
}

/// A picker window, the people it offers, and the events it has emitted.
type PickerFixture<'a> = (
    Entity<PickerHost>,
    Vec<Person>,
    Rc<RefCell<Vec<PickerEvent>>>,
    &'a mut VisualTestContext,
);

fn picker_host(cx: &mut TestAppContext, max: usize) -> PickerFixture<'_> {
    init(cx);
    let people: Vec<Person> = [
        ("Alice Nguyen", "@alice"),
        ("Bob Okafor", "@bob"),
        ("Zoë Lindqvist", "@zoe"),
        ("Albert Camus", "@albert"),
    ]
    .into_iter()
    .map(|(name, detail)| Person {
        id: UserId::generate(),
        name: name.into(),
        detail: detail.into(),
        status: None,
    })
    .collect();
    let events = Rc::new(RefCell::new(Vec::new()));
    let (host, cx) = cx.add_window_view({
        let (people, events) = (people.clone(), events.clone());
        move |window, cx| {
            let picker = cx.new(|cx| PeoplePicker::new(people, max, "Search people", cx));
            let focus = picker.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            let sink = events.clone();
            let subscription = cx.subscribe(&picker, move |_, _, event: &PickerEvent, _| {
                sink.borrow_mut().push(*event)
            });
            PickerHost {
                picker,
                _subscription: subscription,
            }
        }
    });
    cx.run_until_parked();
    (host, people, events, cx)
}

fn selected(host: &Entity<PickerHost>, cx: &mut VisualTestContext) -> Vec<UserId> {
    host.read_with(cx, |h, app| h.picker.read(app).selected().to_vec())
}

#[gpui::test]
fn typing_filters_arrow_keys_move_and_enter_adds_a_person(cx: &mut TestAppContext) {
    let (host, people, events, cx) = picker_host(cx, 8);
    cx.simulate_input("al");
    // "Alice" and "Albert" match, in name order: Albert, Alice.
    cx.simulate_keystrokes("down enter");
    assert_eq!(
        selected(&host, cx),
        vec![people[0].id],
        "the second match (Alice Nguyen) was added"
    );
    assert_eq!(events.borrow().last(), Some(&PickerEvent::Changed));
    let text = host.read_with(cx, |h, app| {
        h.picker.read(app).input().read(app).text().to_owned()
    });
    assert_eq!(text, "", "the search box clears after adding");
}

#[gpui::test]
fn backspace_in_an_empty_search_removes_the_last_chip_and_enter_confirms(cx: &mut TestAppContext) {
    let (host, people, events, cx) = picker_host(cx, 8);
    cx.simulate_input("bob");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("zo");
    cx.simulate_keystrokes("enter");
    assert_eq!(selected(&host, cx), vec![people[1].id, people[2].id]);
    cx.simulate_keystrokes("backspace");
    assert_eq!(
        selected(&host, cx),
        vec![people[1].id],
        "Backspace removed Zoë"
    );
    events.borrow_mut().clear();
    cx.simulate_keystrokes("enter");
    assert_eq!(
        *events.borrow(),
        vec![PickerEvent::Submit],
        "Enter with nothing typed asks the owner to confirm"
    );
}

#[gpui::test]
fn chips_have_a_remove_button_and_the_maximum_is_enforced(cx: &mut TestAppContext) {
    let (host, people, _events, cx) = picker_host(cx, 2);
    for name in ["alice", "bob", "zoe"] {
        cx.simulate_input(name);
        cx.simulate_keystrokes("enter");
    }
    assert_eq!(selected(&host, cx).len(), 2, "no more than the maximum");
    let remove: &'static str =
        Box::leak(format!("picker-remove-{}", people[0].id).into_boxed_str());
    click(cx, remove);
    assert_eq!(
        selected(&host, cx),
        vec![people[1].id],
        "the chip's × removed Alice"
    );
}

#[gpui::test]
fn tab_in_the_search_box_is_reported_to_the_owner(cx: &mut TestAppContext) {
    let (_host, _people, events, cx) = picker_host(cx, 8);
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(
        *events.borrow(),
        vec![
            PickerEvent::Tab { backwards: false },
            PickerEvent::Tab { backwards: true }
        ]
    );
}

// ---- Segmented toggle ----

struct SegmentedHost {
    focus: FocusHandle,
    selected: usize,
}

impl Render for SegmentedHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let this = cx.entity().downgrade();
        let segments = [
            Segment {
                label: "Public".into(),
                detail: "Anyone can join".into(),
            },
            Segment {
                label: "Private".into(),
                detail: "Invite only".into(),
            },
        ];
        div()
            .size_full()
            .bg(theme.colors.surface)
            .p_4()
            .child(form::segmented(
                &theme,
                "visibility",
                &segments,
                self.selected,
                &self.focus,
                move |index, _, cx| {
                    this.update(cx, |host, cx| {
                        host.selected = index;
                        cx.notify();
                    })
                    .ok();
                },
            ))
    }
}

#[gpui::test]
fn the_segmented_toggle_is_one_tab_stop_moved_with_arrows_or_clicks(cx: &mut TestAppContext) {
    init(cx);
    let (host, cx) = cx.add_window_view(|_, cx| SegmentedHost {
        focus: cx.focus_handle().tab_stop(true),
        selected: 0,
    });
    let focus = host.read_with(cx, |h, _| h.focus.clone());
    cx.update(|window, app| window.focus(&focus, app));
    cx.simulate_keystrokes("right");
    assert_eq!(host.read_with(cx, |h, _| h.selected), 1);
    cx.simulate_keystrokes("right");
    assert_eq!(host.read_with(cx, |h, _| h.selected), 0, "wraps around");
    cx.simulate_keystrokes("left");
    assert_eq!(host.read_with(cx, |h, _| h.selected), 1);
    click(cx, "visibility-0");
    assert_eq!(
        host.read_with(cx, |h, _| h.selected),
        0,
        "clicking an option selects it"
    );
}
