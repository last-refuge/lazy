//! Manual test window for `lazy_ui::text_input`.
//!
//! Run with `cargo run -p lazy-ui --example text_input_demo`.
//!
//! * The first box is the composer: multiline, grows to 6 rows then scrolls. Enter submits,
//!   Shift+Enter inserts a line break. Enter during an IME composition commits it instead.
//! * The username box is single-line; Tab and Shift+Tab move between the two fields below it.
//! * The password box is masked: it shows bullets, refuses copy and cut, and treats word
//!   navigation as whole-field. The readout shows its length, never its content.
//!
//! Everything shown here is demonstration content, not application data.

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};
use gpui_platform::application;
use lazy_ui::text_input::{TextInput, TextInputEvent, TextInputOptions, bind_keys};

struct Demo {
    composer: Entity<TextInput>,
    username: Entity<TextInput>,
    password: Entity<TextInput>,
    submitted: Vec<String>,
    last_event: String,
}

impl Demo {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new("Message #demo (Enter sends, Shift+Enter adds a line)")
                    .accessible_label("Message")
                    .max_lines(6),
                cx,
            )
        });
        let username = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new("Username")
                    .accessible_label("Username")
                    .single_line(true),
                cx,
            )
        });
        let password = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new("Password")
                    .accessible_label("Password")
                    .single_line(true)
                    .masked(true),
                cx,
            )
        });

        for input in [&composer, &username, &password] {
            cx.observe(input, |_, _, cx| cx.notify()).detach();
        }
        cx.subscribe(&composer, |this, composer, event: &TextInputEvent, cx| {
            this.last_event = format!("composer: {event:?}");
            if *event == TextInputEvent::Submit {
                let text = composer.read(cx).text().to_owned();
                if !text.trim().is_empty() {
                    this.submitted.push(text);
                    composer.update(cx, |input, cx| input.clear(cx));
                }
            }
            cx.notify();
        })
        .detach();
        cx.subscribe_in(
            &username,
            window,
            |this, _, event: &TextInputEvent, window, cx| {
                this.last_event = format!("username: {event:?}");
                if matches!(
                    event,
                    TextInputEvent::Submit | TextInputEvent::Tab { backwards: false }
                ) {
                    this.password
                        .update(cx, |input, cx| input.focus(window, cx));
                }
                cx.notify();
            },
        )
        .detach();
        cx.subscribe_in(
            &password,
            window,
            |this, password, event: &TextInputEvent, window, cx| {
                this.last_event = format!("password: {event:?}");
                match event {
                    TextInputEvent::Tab { backwards: true } => {
                        this.username
                            .update(cx, |input, cx| input.focus(window, cx));
                    }
                    TextInputEvent::Tab { backwards: false } | TextInputEvent::Submit => {
                        let length = password.read(cx).text().chars().count();
                        this.submitted
                            .push(format!("[demo sign-in with a {length}-character password]"));
                        password.update(cx, |input, cx| input.clear(cx));
                    }
                    TextInputEvent::Changed
                    | TextInputEvent::UpWhenEmpty
                    | TextInputEvent::PasteAttachments
                    | TextInputEvent::Suggestion(_) => {}
                }
                cx.notify();
            },
        )
        .detach();

        composer.update(cx, |input, cx| input.focus(window, cx));
        Self {
            composer,
            username,
            password,
            submitted: Vec::new(),
            last_event: "none yet".into(),
        }
    }

    fn field(
        &self,
        label: &str,
        input: &Entity<TextInput>,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement {
        let focused = input.read(cx).is_focused(window);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x6f665d))
                    .child(label.to_owned()),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_2()
                    .border_color(if focused {
                        rgb(0x2f6fed)
                    } else {
                        rgb(0xd8d0c5)
                    })
                    .bg(rgb(0xffffff))
                    .child(input.clone()),
            )
    }

    fn readout(&self, name: &str, input: &Entity<TextInput>, cx: &App) -> String {
        let input = input.read(cx);
        let (selection, reversed) = input.selection_utf16();
        let text = if name == "password" {
            format!("<{} chars, hidden>", input.text().chars().count())
        } else {
            format!("{:?}", input.text())
        };
        format!(
            "{name}: text={text} selection(utf16)={selection:?}{} marked(utf16)={:?} composing={}",
            if reversed { " reversed" } else { "" },
            input.marked_range_utf16(),
            input.is_composing(),
        )
    }
}

impl Focusable for Demo {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.focus_handle(cx)
    }
}

impl Render for Demo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let readouts = [
            self.readout("composer", &self.composer, cx),
            self.readout("username", &self.username, cx),
            self.readout("password", &self.password, cx),
            format!("last event: {}", self.last_event),
        ];
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_5()
            .bg(rgb(0xf6f1ea))
            .text_color(rgb(0x2b2622))
            .child(
                div()
                    .text_lg()
                    .child("Text input demo (demonstration content)"),
            )
            .child(self.field(
                "Composer (multiline, max 6 rows)",
                &self.composer,
                window,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(div().flex_1().child(self.field(
                        "Username (single line)",
                        &self.username,
                        window,
                        cx,
                    )))
                    .child(div().flex_1().child(self.field(
                        "Password (masked)",
                        &self.password,
                        window,
                        cx,
                    ))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(rgb(0x6f665d)).child("Submitted"))
                    .children(self.submitted.iter().rev().take(5).map(|text| {
                        div()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(0xeae3d8))
                            .child(text.clone())
                    })),
            )
            .child(
                div()
                    .mt_auto()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_xs()
                    .children(readouts.into_iter().map(|line| div().child(line))),
            )
    }
}

fn main() {
    application().run(|cx: &mut App| {
        bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(720.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Demo::new(window, cx)),
        )
        .expect("open the demo window");
        cx.activate(true);
    });
}
