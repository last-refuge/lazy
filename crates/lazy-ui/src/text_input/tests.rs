//! GPUI-level tests for [`TextInput`].
//!
//! These run on GPUI's test platform, whose text system gives every character the same advance.
//! They prove command routing, event semantics, platform-handler arithmetic, and the geometry
//! *logic* (wrapping, growth, scrolling, candidate-window placement relative to laid-out rows).
//! They cannot prove real glyph metrics, real IME behavior or candidate-window position on screen,
//! screen-reader output, or display scaling.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    ClipboardItem, Entity, EntityInputHandler, IntoElement, Modifiers, MouseButton, Pixels, Render,
    TestAppContext, VisualTestContext, Window, div, point, prelude::*, px,
};

use super::*;

const MODIFIER: &str = if cfg!(target_os = "macos") {
    "cmd"
} else {
    "ctrl"
};

/// Hosts an input in a container of a fixed width so wrapping can be exercised.
struct Harness {
    input: Entity<TextInput>,
    width: Pixels,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div().w(self.width).child(self.input.clone())
    }
}

type Events = Rc<RefCell<Vec<TextInputEvent>>>;

fn setup(
    cx: &mut TestAppContext,
    options: TextInputOptions,
    width: f32,
) -> (Entity<TextInput>, Events, &mut VisualTestContext) {
    cx.update(bind_keys);
    let (harness, cx) = cx.add_window_view(|_, cx| Harness {
        input: cx.new(|cx| TextInput::new(options, cx)),
        width: px(width),
    });
    let input = harness.read_with(cx, |harness, _| harness.input.clone());
    let events: Events = Rc::default();
    cx.update(|_, cx| {
        let events = events.clone();
        cx.subscribe(&input, move |_, event: &TextInputEvent, _| {
            events.borrow_mut().push(*event)
        })
        .detach();
    });
    input.update_in(cx, |input, window, cx| input.focus(window, cx));
    cx.run_until_parked();
    (input, events, cx)
}

fn text(input: &Entity<TextInput>, cx: &VisualTestContext) -> String {
    input.read_with(cx, |input, _| input.text().to_owned())
}

fn take(events: &Events) -> Vec<TextInputEvent> {
    std::mem::take(&mut *events.borrow_mut())
}

fn mark(input: &Entity<TextInput>, cx: &mut VisualTestContext, text: &str) {
    input.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, text, None, window, cx)
    });
    cx.run_until_parked();
}

fn row_count(input: &Entity<TextInput>, cx: &VisualTestContext) -> usize {
    input.read_with(cx, |input, _| {
        input.layout.borrow().as_ref().map_or(0, |l| l.rows.len())
    })
}

fn area_height(input: &Entity<TextInput>, cx: &VisualTestContext) -> Pixels {
    input.read_with(cx, |input, _| {
        input
            .layout
            .borrow()
            .as_ref()
            .and_then(|l| l.bounds)
            .expect("painted")
            .size
            .height
    })
}

#[gpui::test]
fn typing_inserts_text_and_emits_changed(cx: &mut TestAppContext) {
    let (input, events, cx) = setup(cx, TextInputOptions::new("Message"), 400.);
    cx.simulate_input("héllo 🎉");
    assert_eq!(text(&input, cx), "héllo 🎉");
    assert!(take(&events).iter().all(|e| *e == TextInputEvent::Changed));
}

#[gpui::test]
fn enter_submits_when_not_composing(cx: &mut TestAppContext) {
    let (input, events, cx) = setup(cx, TextInputOptions::new(""), 400.);
    cx.simulate_input("hi");
    take(&events);
    cx.simulate_keystrokes("enter");
    assert_eq!(take(&events), vec![TextInputEvent::Submit]);
    assert_eq!(text(&input, cx), "hi", "the host decides whether to clear");
}

#[gpui::test]
fn enter_does_not_submit_while_composing(cx: &mut TestAppContext) {
    let (input, events, cx) = setup(cx, TextInputOptions::new(""), 400.);
    mark(&input, cx, "かな");
    assert!(input.read_with(cx, |i, _| i.is_composing()));
    take(&events);

    cx.simulate_keystrokes("enter");
    let seen = take(&events);
    assert!(
        !seen.contains(&TextInputEvent::Submit),
        "submitted during composition: {seen:?}"
    );
    assert!(
        !input.read_with(cx, |i, _| i.is_composing()),
        "Enter commits the composition"
    );
    assert_eq!(text(&input, cx), "かな");

    cx.simulate_keystrokes("enter");
    assert_eq!(
        take(&events),
        vec![TextInputEvent::Submit],
        "the next Enter submits"
    );
}

#[gpui::test]
fn shift_enter_inserts_a_newline_instead_of_submitting(cx: &mut TestAppContext) {
    let (input, events, cx) = setup(cx, TextInputOptions::new(""), 400.);
    cx.simulate_input("a");
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input("b");
    assert_eq!(text(&input, cx), "a\nb");
    assert!(!take(&events).contains(&TextInputEvent::Submit));
}

#[gpui::test]
fn modifier_enter_chord_swaps_the_roles(cx: &mut TestAppContext) {
    let options = TextInputOptions::new("").submit_chord(SubmitChord::ModifierEnter);
    let (input, events, cx) = setup(cx, options, 400.);
    cx.simulate_input("a");
    cx.simulate_keystrokes("enter");
    assert_eq!(text(&input, cx), "a\n");
    assert!(!take(&events).contains(&TextInputEvent::Submit));
    cx.simulate_keystrokes(&format!("{MODIFIER}-enter"));
    assert_eq!(take(&events), vec![TextInputEvent::Submit]);
}

#[gpui::test]
fn set_text_and_clear_are_silent(cx: &mut TestAppContext) {
    let (input, events, cx) = setup(cx, TextInputOptions::new(""), 400.);
    input.update(cx, |input, cx| input.set_text("draft 🎉", cx));
    cx.run_until_parked();
    assert_eq!(text(&input, cx), "draft 🎉");
    assert_eq!(
        input.read_with(cx, |i, _| i.selection_utf16().0),
        8..8,
        "caret goes to the end (UTF-16 units)"
    );
    input.update(cx, |input, cx| input.clear(cx));
    assert_eq!(text(&input, cx), "");
    assert!(
        take(&events).is_empty(),
        "programmatic edits emit no events"
    );
}

#[gpui::test]
fn disabled_input_rejects_edits_and_platform_input(cx: &mut TestAppContext) {
    let (input, events, cx) = setup(cx, TextInputOptions::new(""), 400.);
    cx.simulate_input("ab");
    input.update(cx, |input, cx| input.set_enabled(false, cx));
    take(&events);

    cx.simulate_input("zzz");
    cx.simulate_keystrokes("backspace enter");
    assert_eq!(text(&input, cx), "ab");
    assert!(take(&events).is_empty());

    input.update_in(cx, |input, window, cx| {
        assert!(!input.accepts_text_input(window, cx));
        assert!(input.selected_text_range(false, window, cx).is_none());
        assert!(input.selected_text_range(true, window, cx).is_some());
        input.replace_text_in_range(None, "x", window, cx);
        input.replace_and_mark_text_in_range(None, "y", None, window, cx);
        assert_eq!(input.text(), "ab");
    });
}

#[gpui::test]
fn platform_handler_uses_utf16_with_emoji_and_combining_marks(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 400.);
    input.update(cx, |input, cx| input.set_text("e\u{301}🎉x", cx));
    input.update_in(cx, |input, window, cx| {
        assert_eq!(input.text_length_utf16(window, cx), Some(5));

        let mut actual = None;
        let slice = input.text_for_range(2..4, &mut actual, window, cx);
        assert_eq!(slice.as_deref(), Some("🎉"));
        assert_eq!(actual, Some(2..4));

        let mut actual = None;
        let slice = input.text_for_range(3..4, &mut actual, window, cx);
        assert_eq!(
            slice.as_deref(),
            Some("🎉"),
            "half a surrogate pair covers the character"
        );
        assert_eq!(actual, Some(2..4));

        input.set_selected_text_range(1..3, window, cx);
        let selection = input
            .selected_text_range(false, window, cx)
            .expect("selection");
        assert_eq!(selection.range, 0..4, "snapped out to grapheme boundaries");
        assert!(!selection.reversed);

        input.replace_text_in_range(Some(2..4), "!", window, cx);
        assert_eq!(input.text(), "e\u{301}!x");
        assert_eq!(
            input
                .selected_text_range(false, window, cx)
                .expect("caret")
                .range,
            3..3
        );
    });
}

#[gpui::test]
fn ime_marking_reports_utf16_ranges(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 400.);
    cx.simulate_input("a🎉");
    input.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "日本", Some(1..2), window, cx);
        assert_eq!(input.text(), "a🎉日本");
        assert_eq!(input.marked_text_range(window, cx), Some(3..5));
        let selection = input
            .selected_text_range(false, window, cx)
            .expect("selection");
        assert_eq!(selection.range, 4..5);

        input.replace_text_in_range(None, "日本", window, cx);
        assert_eq!(input.marked_text_range(window, cx), None);
        input.replace_and_mark_text_in_range(None, "", None, window, cx);
        assert_eq!(input.text(), "a🎉日本");
    });
}

#[gpui::test]
fn undo_and_redo_run_through_key_bindings(cx: &mut TestAppContext) {
    let (input, events, cx) = setup(cx, TextInputOptions::new(""), 400.);
    cx.simulate_input("abc");
    take(&events);
    cx.simulate_keystrokes(&format!("{MODIFIER}-z"));
    assert_eq!(text(&input, cx), "");
    assert_eq!(take(&events), vec![TextInputEvent::Changed]);
    cx.simulate_keystrokes(&format!("{MODIFIER}-shift-z"));
    assert_eq!(text(&input, cx), "abc");
}

#[gpui::test]
fn selection_keys_and_typing_replace(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 400.);
    cx.simulate_input("hello world");
    cx.simulate_keystrokes("shift-left shift-left shift-left");
    cx.simulate_input("rld");
    cx.simulate_keystrokes(&format!("{MODIFIER}-a"));
    cx.simulate_input("done");
    assert_eq!(text(&input, cx), "done");
}

#[gpui::test]
fn clipboard_actions_work(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 400.);
    cx.simulate_input("abc");
    cx.simulate_keystrokes(&format!("{MODIFIER}-a {MODIFIER}-x"));
    assert_eq!(text(&input, cx), "");
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some("abc")
    );
    cx.write_to_clipboard(ClipboardItem::new_string("p\r\nq".into()));
    cx.simulate_keystrokes(&format!("{MODIFIER}-v"));
    assert_eq!(text(&input, cx), "p\nq");
}

#[gpui::test]
fn placeholder_is_shown_only_when_empty(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new("Message #general"), 400.);
    assert_eq!(
        input.read_with(cx, |i, _| i.display_text()).as_ref(),
        "Message #general"
    );
    assert!(input.read_with(cx, |i, _| {
        i.layout
            .borrow()
            .as_ref()
            .is_some_and(|l| l.showing_placeholder)
    }));
    cx.simulate_input("x");
    assert_eq!(input.read_with(cx, |i, _| i.display_text()).as_ref(), "x");
}

#[gpui::test]
fn long_text_wraps_and_the_input_grows_then_scrolls(cx: &mut TestAppContext) {
    let options = TextInputOptions::new("").max_lines(3);
    let (input, _events, cx) = setup(cx, options, 120.);
    let line_height = input.read_with(cx, |i, _| i.style.line_height);
    assert_eq!(area_height(&input, cx), line_height, "one row when empty");

    input.update(cx, |input, cx| input.set_text("one\ntwo", cx));
    cx.run_until_parked();
    assert_eq!(row_count(&input, cx), 2);
    assert_eq!(area_height(&input, cx), line_height * 2.);

    let words = "word ".repeat(40);
    input.update(cx, |input, cx| input.set_text(&words, cx));
    cx.run_until_parked();
    assert!(
        row_count(&input, cx) > 3,
        "soft wrapping produced several rows"
    );
    assert_eq!(
        area_height(&input, cx),
        line_height * 3.,
        "capped at max_lines"
    );
    assert!(
        input.read_with(cx, |i, _| i.scroll.y) > px(0.),
        "caret at the end is scrolled into view"
    );

    cx.simulate_keystrokes(&format!("{MODIFIER}-home"));
    cx.run_until_parked();
    assert_eq!(
        input.read_with(cx, |i, _| i.scroll.y),
        px(0.),
        "moving to the start scrolls back"
    );
}

#[gpui::test]
fn up_and_down_move_by_visual_row_and_keep_the_column(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 120.);
    input.update(cx, |input, cx| input.set_text(&"abcd ".repeat(20), cx));
    cx.run_until_parked();
    let rows = row_count(&input, cx);
    assert!(rows > 3);

    cx.simulate_keystrokes(&format!("{MODIFIER}-home"));
    cx.simulate_keystrokes("right right");
    let start = input.read_with(cx, |i, _| i.model.cursor());
    assert_eq!(start, 2);
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    let (second_row_start, second_row_end) = input.read_with(cx, |i, _| {
        i.layout
            .borrow()
            .as_ref()
            .map(|l| (l.rows[1].start, l.rows[1].end))
            .unwrap()
    });
    let after_down = input.read_with(cx, |i, _| i.model.cursor());
    assert!(
        (second_row_start..second_row_end).contains(&after_down),
        "{after_down} not in row 2"
    );
    assert_eq!(
        after_down - second_row_start,
        2,
        "same column on the next row"
    );
    cx.simulate_keystrokes("up");
    assert_eq!(input.read_with(cx, |i, _| i.model.cursor()), 2);
    cx.simulate_keystrokes("up");
    assert_eq!(
        input.read_with(cx, |i, _| i.model.cursor()),
        0,
        "up from the first row goes to the start"
    );
}

#[gpui::test]
fn home_and_end_stay_on_the_visual_row(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 120.);
    input.update(cx, |input, cx| input.set_text(&"abcd ".repeat(20), cx));
    cx.run_until_parked();
    cx.simulate_keystrokes(&format!("{MODIFIER}-home down"));
    cx.run_until_parked();
    let (start, end) = input.read_with(cx, |i, _| {
        let layout = i.layout.borrow();
        let layout = layout.as_ref().unwrap();
        (layout.rows[1].start, layout.caret_end_of_row(1))
    });
    cx.simulate_keystrokes("end");
    assert_eq!(input.read_with(cx, |i, _| i.model.cursor()), end);
    assert!(end < input.read_with(cx, |i, _| i.model.len()));
    cx.simulate_keystrokes("home");
    assert_eq!(input.read_with(cx, |i, _| i.model.cursor()), start);
}

#[gpui::test]
fn candidate_window_bounds_follow_the_laid_out_caret(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 120.);
    input.update(cx, |input, cx| input.set_text(&"abcd ".repeat(20), cx));
    cx.run_until_parked();
    let bounds = input
        .read_with(cx, |i, _| i.layout.borrow().as_ref().and_then(|l| l.bounds))
        .unwrap();

    input.update_in(cx, |input, window, cx| {
        let first = input
            .bounds_for_range(0..1, bounds, window, cx)
            .expect("laid out");
        let later = input
            .bounds_for_range(3..4, bounds, window, cx)
            .expect("laid out");
        assert_eq!(first.origin.y, later.origin.y, "same row");
        assert!(
            later.origin.x > first.origin.x,
            "later characters sit further right"
        );
        assert_eq!(first.size.height, input.style.line_height);

        let wrapped = input
            .bounds_for_range(70..71, bounds, window, cx)
            .expect("laid out");
        assert!(
            wrapped.origin.y > first.origin.y,
            "later text is on a lower row"
        );

        // After an edit the old layout no longer describes the text.
        input.replace_text_in_range(None, "!", window, cx);
        assert!(input.bounds_for_range(0..1, bounds, window, cx).is_none());
    });
    cx.run_until_parked();
    input.update_in(cx, |input, window, cx| {
        assert!(
            input.bounds_for_range(0..1, bounds, window, cx).is_some(),
            "valid again after repaint"
        );
    });
}

#[gpui::test]
fn character_index_for_point_inverts_bounds_for_range(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 120.);
    input.update(cx, |input, cx| input.set_text(&"abcd ".repeat(10), cx));
    cx.run_until_parked();
    let bounds = input
        .read_with(cx, |i, _| i.layout.borrow().as_ref().and_then(|l| l.bounds))
        .unwrap();
    input.update_in(cx, |input, window, cx| {
        for utf16 in [0, 2, 7, 13, 30] {
            let rect = input
                .bounds_for_range(utf16..utf16, bounds, window, cx)
                .expect("laid out");
            let probe = point(
                rect.origin.x + px(0.5),
                rect.origin.y + rect.size.height / 2.,
            );
            assert_eq!(
                input.character_index_for_point(probe, window, cx),
                Some(utf16)
            );
        }
        assert_eq!(
            input.character_index_for_point(point(px(-50.), px(-50.)), window, cx),
            None
        );
    });
}

#[gpui::test]
fn clicking_places_the_caret_and_shift_click_extends(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 300.);
    input.update(cx, |input, cx| input.set_text("abcdefghij", cx));
    cx.run_until_parked();
    let rect = input.update_in(cx, |input, window, cx| {
        let bounds = input
            .layout
            .borrow()
            .as_ref()
            .and_then(|l| l.bounds)
            .unwrap();
        (
            input.bounds_for_range(4..4, bounds, window, cx).unwrap(),
            input.bounds_for_range(8..8, bounds, window, cx).unwrap(),
        )
    });
    let at = |rect: gpui::Bounds<Pixels>| point(rect.origin.x, rect.origin.y + px(5.));
    cx.simulate_mouse_down(at(rect.0), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(at(rect.0), MouseButton::Left, Modifiers::none());
    assert_eq!(input.read_with(cx, |i, _| i.selection_utf16().0), 4..4);
    cx.simulate_mouse_down(at(rect.1), MouseButton::Left, Modifiers::shift());
    cx.simulate_mouse_up(at(rect.1), MouseButton::Left, Modifiers::shift());
    assert_eq!(input.read_with(cx, |i, _| i.selection_utf16().0), 4..8);
}

#[gpui::test]
fn clicking_the_left_half_of_the_last_character_stays_before_it(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 300.);
    input.update(cx, |input, cx| input.set_text("abcd", cx));
    cx.run_until_parked();
    let (last, end) = input.update_in(cx, |input, window, cx| {
        let bounds = input
            .layout
            .borrow()
            .as_ref()
            .and_then(|l| l.bounds)
            .unwrap();
        (
            input.bounds_for_range(3..3, bounds, window, cx).unwrap(),
            input.bounds_for_range(4..4, bounds, window, cx).unwrap(),
        )
    });
    let glyph_width = end.origin.x - last.origin.x;
    let y = last.origin.y + px(5.);
    for (dx, expected) in [(0.25, 3..3), (0.75, 4..4)] {
        let at = point(last.origin.x + glyph_width * dx, y);
        cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
        assert_eq!(
            input.read_with(cx, |i, _| i.selection_utf16().0),
            expected,
            "dx={dx}"
        );
    }
}

#[gpui::test]
fn double_and_triple_click_select_a_word_and_a_line(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 300.);
    input.update(cx, |input, cx| input.set_text("one two\nthree", cx));
    cx.run_until_parked();
    let at = input.update_in(cx, |input, window, cx| {
        let bounds = input
            .layout
            .borrow()
            .as_ref()
            .and_then(|l| l.bounds)
            .unwrap();
        let rect = input.bounds_for_range(5..5, bounds, window, cx).unwrap();
        point(rect.origin.x + px(1.), rect.origin.y + px(5.))
    });
    let click = |cx: &mut VisualTestContext, count| {
        cx.simulate_event(gpui::MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count: count,
            first_mouse: false,
        });
        cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
    };
    click(cx, 2);
    assert_eq!(
        input.read_with(cx, |i, _| i.selection_utf16().0),
        4..7,
        "the word \"two\""
    );
    click(cx, 3);
    assert_eq!(
        input.read_with(cx, |i, _| i.selection_utf16().0),
        0..7,
        "the first line"
    );
}

#[gpui::test]
fn dragging_extends_the_selection(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, TextInputOptions::new(""), 300.);
    input.update(cx, |input, cx| input.set_text("abcdefghij", cx));
    cx.run_until_parked();
    let (from, to) = input.update_in(cx, |input, window, cx| {
        let bounds = input
            .layout
            .borrow()
            .as_ref()
            .and_then(|l| l.bounds)
            .unwrap();
        let probe = |utf16,
                     input: &mut TextInput,
                     window: &mut Window,
                     cx: &mut gpui::Context<TextInput>| {
            let rect = input
                .bounds_for_range(utf16..utf16, bounds, window, cx)
                .unwrap();
            point(rect.origin.x, rect.origin.y + px(5.))
        };
        (probe(2, input, window, cx), probe(7, input, window, cx))
    });
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
    assert_eq!(input.read_with(cx, |i, _| i.selection_utf16().0), 2..7);
    // Moving the pointer after release no longer changes the selection.
    cx.simulate_mouse_move(from, None, Modifiers::none());
    assert_eq!(input.read_with(cx, |i, _| i.selection_utf16().0), 2..7);
}

// ---- Single-line mode --------------------------------------------------------------------------

fn single_line() -> TextInputOptions {
    TextInputOptions::new("Username").single_line(true)
}

#[gpui::test]
fn single_line_paste_converts_newlines_to_spaces(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, single_line(), 300.);
    cx.write_to_clipboard(ClipboardItem::new_string("a\nb\r\nc".into()));
    cx.simulate_keystrokes(&format!("{MODIFIER}-v"));
    assert_eq!(text(&input, cx), "a b c");
}

#[gpui::test]
fn single_line_enter_always_submits_but_not_while_composing(cx: &mut TestAppContext) {
    let (input, events, cx) = setup(cx, single_line(), 300.);
    cx.simulate_input("jason");
    take(&events);
    cx.simulate_keystrokes("enter");
    assert_eq!(take(&events), vec![TextInputEvent::Submit]);
    cx.simulate_keystrokes("shift-enter");
    assert_eq!(
        take(&events),
        vec![TextInputEvent::Submit],
        "Shift+Enter is not a newline either"
    );
    assert_eq!(text(&input, cx), "jason");

    mark(&input, cx, "かな");
    take(&events);
    cx.simulate_keystrokes("enter");
    assert!(!take(&events).contains(&TextInputEvent::Submit));
    assert!(!input.read_with(cx, |i, _| i.is_composing()));
}

#[gpui::test]
fn single_line_is_one_row_and_scrolls_horizontally(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, single_line(), 100.);
    let line_height = input.read_with(cx, |i, _| i.style.line_height);
    input.update(cx, |input, cx| input.set_text(&"w".repeat(80), cx));
    cx.run_until_parked();
    assert_eq!(row_count(&input, cx), 1, "never wraps");
    assert_eq!(area_height(&input, cx), line_height);
    assert!(
        input.read_with(cx, |i, _| i.scroll.x) > px(0.),
        "caret at the end is scrolled into view"
    );
    assert_eq!(input.read_with(cx, |i, _| i.scroll.y), px(0.));
    cx.simulate_keystrokes("home");
    cx.run_until_parked();
    assert_eq!(input.read_with(cx, |i, _| i.scroll.x), px(0.));
}

#[gpui::test]
fn single_line_reports_tab_without_handling_it(cx: &mut TestAppContext) {
    let (input, events, cx) = setup(cx, single_line(), 300.);
    cx.simulate_input("ab");
    take(&events);
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(
        take(&events),
        vec![
            TextInputEvent::Tab { backwards: false },
            TextInputEvent::Tab { backwards: true }
        ]
    );
    assert_eq!(text(&input, cx), "ab", "Tab inserts nothing");
}

#[gpui::test]
fn multiline_does_not_report_tab(cx: &mut TestAppContext) {
    let (input, events, cx) = setup(cx, TextInputOptions::new(""), 300.);
    cx.simulate_keystrokes("tab shift-tab");
    assert!(
        take(&events).is_empty(),
        "multiline Tab is neither reported nor typed"
    );
    assert_eq!(text(&input, cx), "");
}

#[gpui::test]
fn single_line_up_and_down_jump_to_the_ends(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, single_line(), 300.);
    cx.simulate_input("abcdef");
    cx.simulate_keystrokes("up");
    assert_eq!(input.read_with(cx, |i, _| i.model.cursor()), 0);
    cx.simulate_keystrokes("down");
    assert_eq!(input.read_with(cx, |i, _| i.model.cursor()), 6);
}

// ---- Masked mode -------------------------------------------------------------------------------

fn masked() -> TextInputOptions {
    TextInputOptions::new("Password")
        .single_line(true)
        .masked(true)
}

#[gpui::test]
fn masked_input_shows_one_bullet_per_grapheme_but_keeps_the_real_value(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, masked(), 300.);
    input.update(cx, |input, cx| input.set_text("pässe\u{301}👨‍👩‍👧‍👦", cx));
    cx.run_until_parked();
    assert_eq!(text(&input, cx), "pässe\u{301}👨‍👩‍👧‍👦");
    assert_eq!(
        input.read_with(cx, |i, _| i.display_text()).as_ref(),
        "••••••"
    );
}

#[gpui::test]
fn masked_input_disables_copy_and_cut(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, masked(), 300.);
    cx.write_to_clipboard(ClipboardItem::new_string("sentinel".into()));
    cx.simulate_input("secret");
    cx.simulate_keystrokes(&format!("{MODIFIER}-a {MODIFIER}-c"));
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some("sentinel")
    );
    cx.simulate_keystrokes(&format!("{MODIFIER}-x"));
    assert_eq!(text(&input, cx), "secret");
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some("sentinel")
    );
}

#[gpui::test]
fn masked_word_navigation_treats_the_field_as_one_word(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, masked(), 300.);
    cx.simulate_input("one two three");
    let word_left = if cfg!(target_os = "macos") {
        "alt-left"
    } else {
        "ctrl-left"
    };
    cx.simulate_keystrokes(word_left);
    assert_eq!(input.read_with(cx, |i, _| i.model.cursor()), 0);
    let word_right = if cfg!(target_os = "macos") {
        "alt-right"
    } else {
        "ctrl-right"
    };
    cx.simulate_keystrokes(word_right);
    assert_eq!(
        input.read_with(cx, |i, _| i.model.cursor()),
        "one two three".len()
    );
    let delete_word = if cfg!(target_os = "macos") {
        "alt-backspace"
    } else {
        "ctrl-backspace"
    };
    cx.simulate_keystrokes(delete_word);
    assert_eq!(
        text(&input, cx),
        "",
        "word deletion removes the whole field"
    );
}

#[gpui::test]
fn masked_input_does_not_leak_surrounding_text_to_the_platform(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, masked(), 300.);
    cx.simulate_input("hunter2");
    mark(&input, cx, "かな");
    input.update_in(cx, |input, window, cx| {
        let mut actual = None;
        let outside = input.text_for_range(0..4, &mut actual, window, cx);
        assert_eq!(outside.as_deref(), Some(""));
        assert_eq!(actual, Some(0..0));
        let mut actual = None;
        let preedit = input.text_for_range(7..9, &mut actual, window, cx);
        assert_eq!(
            preedit.as_deref(),
            Some("かな"),
            "the preedit text itself stays readable"
        );
    });
}

#[gpui::test]
fn masked_mouse_selection_maps_back_to_real_offsets(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, masked(), 300.);
    // Bullets: a, é, the family emoji, z. In UTF-16 the family starts at 2 and z at 13.
    input.update(cx, |input, cx| input.set_text("aé👨‍👩‍👧‍👦z", cx));
    cx.run_until_parked();
    input.update_in(cx, |input, window, cx| {
        let bounds = input
            .layout
            .borrow()
            .as_ref()
            .and_then(|l| l.bounds)
            .unwrap();
        for (utf16, byte) in [(0, 0), (1, 1), (2, 3), (13, 28), (14, 29)] {
            let rect = input
                .bounds_for_range(utf16..utf16, bounds, window, cx)
                .expect("laid out");
            let probe = point(rect.origin.x + px(0.5), rect.origin.y + px(5.));
            assert_eq!(
                input.character_index_for_point(probe, window, cx),
                Some(utf16),
                "bullet boundary for byte {byte}"
            );
            assert_eq!(input.real_offset(input.display_offset(byte)), byte);
        }
    });
}

#[gpui::test]
fn masked_input_withholds_its_value_from_accessibility(cx: &mut TestAppContext) {
    // The tree is built in `Render`; this checks the branch it takes rather than the platform
    // tree, which these tests cannot inspect.
    let (input, _events, cx) = setup(cx, masked(), 300.);
    assert!(input.read_with(cx, |i, _| i.masked));
    cx.simulate_input("topsecret");
    assert!(
        input.read_with(cx, |i, _| i.text_cache.is_none()),
        "no copy of the text is prepared for accessibility"
    );
}

#[gpui::test]
fn masked_double_click_selects_the_whole_field(cx: &mut TestAppContext) {
    let (input, _events, cx) = setup(cx, masked(), 300.);
    input.update(cx, |input, cx| input.set_text("one two", cx));
    cx.run_until_parked();
    let at = input.update_in(cx, |input, window, cx| {
        let bounds = input
            .layout
            .borrow()
            .as_ref()
            .and_then(|l| l.bounds)
            .unwrap();
        let rect = input.bounds_for_range(5..5, bounds, window, cx).unwrap();
        point(rect.origin.x + px(1.), rect.origin.y + px(5.))
    });
    cx.simulate_event(gpui::MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::none(),
        click_count: 2,
        first_mouse: false,
    });
    assert_eq!(input.read_with(cx, |i, _| i.selection_utf16().0), 0..7);
}

// ---- Theme following ---------------------------------------------------------------------

mod theme_following {
    use gpui::WindowAppearance;

    use super::*;
    use crate::theme::{self, Theme};

    fn text_color(input: &Entity<TextInput>, cx: &VisualTestContext) -> gpui::Hsla {
        input.read_with(cx, |input, _| input.style.text_color)
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn an_input_follows_the_theme_when_the_system_appearance_changes(cx: &mut TestAppContext) {
        // The window opens while the app believes the system is light...
        cx.update(|cx| theme::install(cx, WindowAppearance::Light));
        let (input, _, cx) = setup(cx, TextInputOptions::new("Username"), 400.);
        redraw(cx);
        assert_eq!(text_color(&input, cx), Theme::light().colors.text);

        // ...then it turns out to be dark. Typed text must contrast with the dark surface, not
        // stay the light theme's dark brown (which made text invisible on the dark box).
        cx.update(|_, cx| theme::install(cx, WindowAppearance::Dark));
        redraw(cx);
        assert_eq!(text_color(&input, cx), Theme::dark().colors.text);
        let (placeholder, caret, selection) = input.read_with(cx, |input, _| {
            (
                input.style.placeholder_color,
                input.style.caret_color,
                input.style.selection_color,
            )
        });
        let dark = Theme::dark().colors;
        assert_eq!(
            (placeholder, caret, selection),
            (dark.text_faint, dark.caret, dark.selection)
        );

        // And back again.
        cx.update(|_, cx| theme::install(cx, WindowAppearance::Light));
        redraw(cx);
        assert_eq!(text_color(&input, cx), Theme::light().colors.text);
    }

    #[gpui::test]
    fn typed_text_is_shaped_in_the_new_color_after_a_change(cx: &mut TestAppContext) {
        cx.update(|cx| theme::install(cx, WindowAppearance::Light));
        let (input, _, cx) = setup(cx, TextInputOptions::new(""), 400.);
        cx.simulate_input("alice");
        redraw(cx);
        cx.update(|_, cx| theme::install(cx, WindowAppearance::Dark));
        redraw(cx);
        // The shaped-text cache is keyed by color, so the text is reshaped rather than reused.
        let shaped_color = input.read_with(cx, |input, _| {
            input
                .layout
                .borrow()
                .as_ref()
                .map(|state| state.key.text_color)
        });
        assert_eq!(shaped_color, Some(Theme::dark().colors.text));
        assert_eq!(
            super::text(&input, cx),
            "alice",
            "the text itself is untouched"
        );
    }

    #[gpui::test]
    fn an_explicit_style_opts_out_of_following_the_theme(cx: &mut TestAppContext) {
        cx.update(|cx| theme::install(cx, WindowAppearance::Light));
        let fixed = TextInputStyle {
            text_color: gpui::red(),
            ..TextInputStyle::default()
        };
        let (input, _, cx) = setup(cx, TextInputOptions::new("").style(fixed), 400.);
        redraw(cx);
        cx.update(|_, cx| theme::install(cx, WindowAppearance::Dark));
        redraw(cx);
        assert_eq!(text_color(&input, cx), gpui::red());
    }

    #[gpui::test]
    fn an_input_without_an_installed_theme_keeps_its_default_style(cx: &mut TestAppContext) {
        let (input, _, cx) = setup(cx, TextInputOptions::new(""), 400.);
        redraw(cx);
        assert_eq!(text_color(&input, cx), TextInputStyle::default().text_color);
    }
}

mod formatting {
    use super::*;

    fn composer_options() -> TextInputOptions {
        TextInputOptions::new("Message")
            .formatting_shortcuts(true)
            .report_up_when_empty(true)
    }

    #[gpui::test]
    fn the_shortcuts_wrap_the_selection(cx: &mut TestAppContext) {
        let (input, events, cx) = setup(cx, composer_options(), 400.);
        cx.simulate_input("make it loud");
        cx.simulate_keystrokes("shift-ctrl-left");
        cx.simulate_keystrokes(&format!("{MODIFIER}-b"));
        assert_eq!(text(&input, cx), "make it *loud*");
        cx.simulate_keystrokes(&format!("{MODIFIER}-i"));
        assert_eq!(text(&input, cx), "make it *_loud_*");
        cx.simulate_keystrokes(&format!("shift-{MODIFIER}-x"));
        assert_eq!(text(&input, cx), "make it *_~loud~_*");
        assert!(take(&events).contains(&TextInputEvent::Changed));
    }

    #[gpui::test]
    fn the_shortcuts_do_nothing_unless_asked_for(cx: &mut TestAppContext) {
        let (input, events, cx) = setup(cx, TextInputOptions::new("Message"), 400.);
        cx.simulate_input("plain");
        take(&events);
        cx.simulate_keystrokes(&format!("{MODIFIER}-b"));
        assert_eq!(text(&input, cx), "plain");
        assert!(take(&events).is_empty());
    }

    #[gpui::test]
    fn up_in_an_empty_input_is_reported_only_when_asked_for(cx: &mut TestAppContext) {
        let (_input, events, cx) = setup(cx, composer_options(), 400.);
        cx.simulate_keystrokes("up");
        assert_eq!(take(&events), vec![TextInputEvent::UpWhenEmpty]);

        let (_plain, plain_events, cx) = setup(cx, TextInputOptions::new("Message"), 400.);
        cx.simulate_keystrokes("up");
        assert!(take(&plain_events).is_empty());
    }

    #[gpui::test]
    fn up_with_text_moves_the_caret_instead(cx: &mut TestAppContext) {
        let (input, events, cx) = setup(cx, composer_options(), 400.);
        cx.simulate_input("one");
        take(&events);
        cx.simulate_keystrokes("up");
        assert!(take(&events).is_empty());
        assert_eq!(input.read_with(cx, |i, _| i.selection_utf16().0), 0..0);
    }
}

mod suggestions {
    use super::*;

    fn composer_options() -> TextInputOptions {
        TextInputOptions::new("Message")
    }

    #[gpui::test]
    fn an_open_list_takes_up_down_enter_and_tab_and_nothing_else(cx: &mut TestAppContext) {
        let (input, events, cx) = setup(cx, composer_options(), 400.);
        cx.simulate_input("one");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("two");
        take(&events);
        input.update(cx, |input, _| input.set_suggestions_open(true));

        cx.simulate_keystrokes("up down enter tab");
        assert_eq!(
            take(&events),
            vec![
                TextInputEvent::Suggestion(SuggestionKey::Up),
                TextInputEvent::Suggestion(SuggestionKey::Down),
                TextInputEvent::Suggestion(SuggestionKey::Accept),
                TextInputEvent::Suggestion(SuggestionKey::Accept),
            ]
        );
        assert_eq!(
            text(&input, cx),
            "one\ntwo",
            "none of them edited or moved anything"
        );

        // Typing, deleting, and the other arrows are not the list's.
        cx.simulate_input("!");
        cx.simulate_keystrokes("left backspace");
        assert_eq!(
            take(&events)
                .iter()
                .filter(|e| matches!(e, TextInputEvent::Suggestion(_)))
                .count(),
            0
        );
        assert_eq!(text(&input, cx), "one\ntw!");
    }

    #[gpui::test]
    fn a_closed_list_leaves_every_key_alone(cx: &mut TestAppContext) {
        let (input, events, cx) = setup(cx, composer_options(), 400.);
        cx.simulate_input("one");
        take(&events);
        input.update(cx, |input, _| input.set_suggestions_open(true));
        input.update(cx, |input, _| input.set_suggestions_open(false));
        cx.simulate_keystrokes("enter");
        assert_eq!(
            take(&events),
            vec![TextInputEvent::Submit],
            "Enter submits again"
        );
        cx.simulate_keystrokes("up");
        assert!(take(&events).is_empty());
    }

    #[gpui::test]
    fn shift_tab_and_shift_up_still_work_with_the_list_open(cx: &mut TestAppContext) {
        let (input, events, cx) = setup(cx, composer_options(), 400.);
        cx.simulate_input("one two");
        input.update(cx, |input, _| input.set_suggestions_open(true));
        take(&events);
        cx.simulate_keystrokes("shift-left");
        assert_eq!(
            input.read_with(cx, |i, _| i.selection_utf16().0),
            6..7,
            "extending the selection is not a list key"
        );
    }

    #[gpui::test]
    fn replace_range_and_insert_text_edit_at_the_right_place(cx: &mut TestAppContext) {
        let (input, events, cx) = setup(cx, composer_options(), 400.);
        cx.simulate_input("nice :tad");
        take(&events);
        input.update_in(cx, |input, window, cx| {
            let end = input.text().len();
            input.replace_range(5..end, "🎉", window, cx);
        });
        assert_eq!(text(&input, cx), "nice 🎉");
        assert_eq!(
            input.read_with(cx, |i, _| i.cursor()),
            "nice 🎉".len(),
            "the caret is after it"
        );
        assert!(take(&events).contains(&TextInputEvent::Changed));

        input.update_in(cx, |input, window, cx| input.insert_text("🚀", window, cx));
        assert_eq!(text(&input, cx), "nice 🎉🚀");
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(text(&input, cx), "nice 🎉", "each is one undo step");
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(text(&input, cx), "nice :tad");
    }

    #[gpui::test]
    fn replace_range_is_ignored_while_composing(cx: &mut TestAppContext) {
        let (input, _events, cx) = setup(cx, composer_options(), 400.);
        mark(&input, cx, "かな");
        input.update_in(cx, |input, window, cx| {
            input.replace_range(0..3, "x", window, cx)
        });
        assert_eq!(text(&input, cx), "かな");
    }

    #[gpui::test]
    fn the_caret_bounds_follow_the_caret_across_rows(cx: &mut TestAppContext) {
        let (input, _events, cx) = setup(cx, composer_options(), 400.);
        assert!(
            input.read_with(cx, |i, _| i.caret_bounds()).is_none(),
            "nothing is shaped for an empty input"
        );
        cx.simulate_input("ab");
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let first = input
            .read_with(cx, |i, _| i.caret_bounds())
            .expect("painted with its current text");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("cd");
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let second = input.read_with(cx, |i, _| i.caret_bounds()).unwrap();
        assert!(second.top() > first.top(), "a second row is lower");
        assert_eq!(second.size.height, first.size.height);
        assert!(second.left() > first.left() - gpui::px(1.));
    }
}
