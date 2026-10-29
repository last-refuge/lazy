//! Choosing the server (dogfood packaging) on GPUI's test platform: the "Server" row under the
//! sign-in form, on the unreachable-server screen, and on the no-access screen; the register
//! form's relabelled invitation field. The real views and the real core run; a recording effect
//! sink stands in for the runtime, and its answers (a check's result, the move to a new server)
//! are sent as the runtime would send them.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch, every mouse claim
//! with real mouse events. A composition is simulated by the input handler's own calls, not by a
//! real input method.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, EntityInputHandler as _, Modifiers, TestAppContext, VisualTestContext,
    WindowAppearance,
};
use lazy_client_core::{
    Effect, Input, RequestError, ServerCheck, ServerInfo, ServerSource, SessionPhase, StartupState,
};
use lazy_protocol::{BootstrapResponse, PROTOCOL_VERSION, User};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::root_view::RootView;
use lazy_ui::server_row::{ServerRow, ServerStatus, server_view};

#[derive(Default)]
struct Recorder(RefCell<Vec<Effect>>);

impl EffectSink for Recorder {
    fn execute(&self, effects: Vec<Effect>) {
        self.0.borrow_mut().extend(effects);
    }
}

struct World {
    model: Entity<AppModel>,
    effects: Rc<Recorder>,
    root: Entity<RootView>,
}

fn start(cx: &mut TestAppContext) -> (World, &mut VisualTestContext) {
    cx.update(|cx| lazy_ui::init(cx, WindowAppearance::Light));
    let effects = Rc::new(Recorder::default());
    let model = cx.new(|_| AppModel::new(effects.clone()));
    let (root, cx) = cx.add_window_view({
        let model = model.clone();
        move |window, cx| RootView::new(model, window, cx)
    });
    cx.simulate_resize(gpui::size(gpui::px(1100.), gpui::px(900.)));
    (
        World {
            model,
            effects,
            root,
        },
        cx,
    )
}

/// Two frames: what is laid out in one is what the mouse can hit in the next.
fn draw(cx: &mut VisualTestContext) {
    for _ in 0..2 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
}

fn user() -> User {
    User {
        is_bot: false,
        deactivated: false,
        avatar_version: 0,
        status: None,
        id: lazy_domain::UserId::generate(),
        username: lazy_domain::Username::parse("quentin").unwrap(),
        display_name: lazy_domain::DisplayName::parse("Quentin").unwrap(),
    }
}

impl World {
    fn dispatch(&self, cx: &mut VisualTestContext, input: Input) {
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.run_until_parked();
    }

    fn take_effects(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.effects.0.borrow_mut())
    }

    /// The addresses asked, oldest first, with their generations.
    fn checks(&self) -> Vec<(String, u64)> {
        self.effects
            .0
            .borrow()
            .iter()
            .filter_map(|effect| match effect {
                Effect::CheckServer { url, generation } => Some((url.clone(), *generation)),
                _ => None,
            })
            .collect()
    }

    fn core<R>(
        &self,
        cx: &VisualTestContext,
        read: impl FnOnce(&lazy_client_core::ClientCore) -> R,
    ) -> R {
        self.model.read_with(cx, |model, _| read(model.core()))
    }

    fn signed_out(&self, cx: &mut VisualTestContext) {
        self.dispatch(cx, Input::Started(StartupState::default()));
        draw(cx);
    }

    fn on_screen(&self, cx: &mut VisualTestContext, selector: &'static str) -> bool {
        draw(cx);
        cx.debug_bounds(selector).is_some()
    }

    fn click(&self, cx: &mut VisualTestContext, selector: &'static str) {
        draw(cx);
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing is labelled {selector:?} on screen"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }

    fn row(&self, cx: &mut VisualTestContext) -> Entity<ServerRow> {
        self.root.read_with(cx, |root, cx| root.server_row(cx))
    }

    fn editing(&self, cx: &mut VisualTestContext) -> bool {
        self.row(cx).read_with(cx, |row, _| row.editing())
    }

    fn view(&self, cx: &VisualTestContext) -> lazy_ui::server_row::ServerView {
        self.core(cx, server_view)
    }

    /// The runtime answers the latest check.
    fn answer(&self, cx: &mut VisualTestContext, result: Result<u32, String>) {
        let (_, generation) = *self.checks().last().expect("a check was asked");
        self.dispatch(cx, Input::ServerChecked { generation, result });
        draw(cx);
    }

    fn keyboard_on_change(&self, cx: &mut VisualTestContext) -> bool {
        let focus = self.row(cx).read_with(cx, |row, _| row.change_focus());
        cx.update(|window, _| focus.is_focused(window))
    }
}

// ----- On the sign-in screen -----

#[gpui::test]
fn the_sign_in_screen_shows_the_server_and_asks_what_it_is_once(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.signed_out(cx);
    assert!(world.on_screen(cx, "server-row"));
    assert!(world.on_screen(cx, "server-address"));
    assert!(world.on_screen(cx, "server-change"));
    // Showing it asks once, however many frames are drawn.
    draw(cx);
    draw(cx);
    let checks = world.checks();
    assert_eq!(checks.len(), 1, "one check: {checks:?}");
    assert_eq!(checks[0].0, lazy_client_core::DEFAULT_SERVER_URL);
    assert_eq!(world.view(cx).status, ServerStatus::Checking);
    assert!(world.on_screen(cx, "server-status"), "Checking…");
    // The answer.
    world.answer(cx, Ok(PROTOCOL_VERSION));
    assert_eq!(world.view(cx).status, ServerStatus::Connected);
    assert!(world.on_screen(cx, "server-status"), "Connected");
    assert!(
        !world.on_screen(cx, "server-check"),
        "nothing to check again"
    );
    assert_eq!(world.checks().len(), 1, "an answer is not asked for again");
}

#[gpui::test]
fn the_server_row_comes_after_the_forms_own_tab_stops(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.signed_out(cx);
    // The form: username (it has the keyboard), password, Sign in, "Create an account"; then the
    // server's "Change…". The order the sign-in always had is untouched.
    for _ in 0..3 {
        cx.simulate_keystrokes("tab");
        assert!(!world.keyboard_on_change(cx));
    }
    cx.simulate_keystrokes("enter");
    // That Enter was on "Create an account": the form is now the register form, six stops long.
    assert!(world.on_screen(cx, "register-code-hint"));
    cx.simulate_keystrokes("shift-tab shift-tab shift-tab shift-tab shift-tab");
    // Back on the username, as `gui.rs` expects; and the way round ends at the server row.
    cx.simulate_keystrokes("shift-tab");
    assert!(
        world.keyboard_on_change(cx),
        "Shift+Tab from the first field wraps to the last stop"
    );
    cx.simulate_keystrokes("tab");
    cx.simulate_input("x");
    // Tab from the server row's last stop leaves it for the form's first.
    assert!(!world.keyboard_on_change(cx));
}

#[gpui::test]
fn changing_the_server_asks_the_new_address_and_moves_when_it_passes(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.signed_out(cx);
    world.answer(cx, Ok(PROTOCOL_VERSION));
    world.take_effects();
    world.click(cx, "server-change");
    assert!(world.editing(cx));
    assert!(world.on_screen(cx, "server-field"));
    assert!(world.on_screen(cx, "server-connect") && world.on_screen(cx, "server-cancel"));
    assert!(
        !world.on_screen(cx, "server-change"),
        "the button gave way to the field"
    );
    // The field starts with the address in use and the keyboard is in it.
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("Chat.Example.com");
    cx.simulate_keystrokes("enter");
    assert!(!world.editing(cx), "Enter connects");
    assert!(
        world.keyboard_on_change(cx),
        "the keyboard is back on Change…"
    );
    // The core made it an address and asked it, without moving yet.
    assert_eq!(
        world.checks().last().map(|(url, _)| url.as_str()),
        Some("https://chat.example.com")
    );
    assert_eq!(world.view(cx).status, ServerStatus::Checking);
    assert_eq!(
        world.view(cx).address,
        "https://chat.example.com",
        "the row shows the address being tried"
    );
    assert_eq!(
        world.core(cx, |core| core.server().url.clone()),
        lazy_client_core::DEFAULT_SERVER_URL,
        "the address in use has not changed"
    );
    // It passes: the core asks the runtime to move there, and the runtime does.
    world.dispatch(
        cx,
        Input::ServerChecked {
            generation: world.core_job_generation(cx),
            result: Ok(PROTOCOL_VERSION),
        },
    );
    let moved: Vec<(String, u64)> = world
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::UseServer { url, generation } => Some((url, generation)),
            _ => None,
        })
        .collect();
    assert_eq!(moved.len(), 1, "one move");
    world.dispatch(
        cx,
        Input::ServerSwitched {
            generation: moved[0].1,
            result: Ok(Box::new(StartupState {
                server: ServerInfo {
                    url: moved[0].0.clone(),
                    source: ServerSource::Saved,
                    trying: None,
                    check: ServerCheck::Reachable {
                        protocol_version: PROTOCOL_VERSION,
                    },
                },
                ..Default::default()
            })),
        },
    );
    draw(cx);
    assert_eq!(
        world.core(cx, |core| core.server().url.clone()),
        "https://chat.example.com"
    );
    assert_eq!(world.view(cx).status, ServerStatus::Connected);
}

impl World {
    /// The generation of the check in flight, read from what the runtime was last asked.
    fn core_job_generation(&self, _cx: &VisualTestContext) -> u64 {
        self.checks().last().expect("a check was asked").1
    }
}

#[gpui::test]
fn a_server_that_does_not_answer_is_said_so_and_can_be_asked_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.signed_out(cx);
    world.click(cx, "server-change");
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("down.example.com");
    cx.simulate_keystrokes("enter");
    world.answer(cx, Err("no answer".into()));
    assert_eq!(
        world.view(cx).status,
        ServerStatus::Unreachable("no answer".into())
    );
    assert!(
        world.on_screen(cx, "server-status"),
        "Can't reach this server"
    );
    assert!(
        world.on_screen(cx, "server-still-using"),
        "the server still in use is named"
    );
    assert_eq!(
        world.core(cx, |core| core.server().url.clone()),
        lazy_client_core::DEFAULT_SERVER_URL,
        "nothing moved"
    );
    // "Check again" asks the same address.
    let asked = world.checks().len();
    world.click(cx, "server-check");
    let checks = world.checks();
    assert_eq!(checks.len(), asked + 1);
    assert_eq!(checks.last().unwrap().0, "https://down.example.com");
}

#[gpui::test]
fn a_server_for_another_protocol_says_which_way_to_update(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.signed_out(cx);
    world.answer(cx, Ok(PROTOCOL_VERSION + 1));
    assert_eq!(world.view(cx).status, ServerStatus::NeedsNewerLazy);
    assert!(world.on_screen(cx, "server-status"));
    assert!(world.on_screen(cx, "server-check"), "it can be asked again");
    world.click(cx, "server-check");
    world.answer(cx, Ok(PROTOCOL_VERSION - 1));
    assert_eq!(world.view(cx).status, ServerStatus::NeedsOlderLazy);
    // Neither is a reason to move: nothing was asked to be used.
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::UseServer { .. }))
    );
}

#[gpui::test]
fn escape_goes_back_without_changing_anything_and_returns_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.signed_out(cx);
    world.answer(cx, Ok(PROTOCOL_VERSION));
    world.take_effects();
    world.click(cx, "server-change");
    cx.simulate_input("junk");
    cx.simulate_keystrokes("escape");
    assert!(!world.editing(cx));
    assert!(world.keyboard_on_change(cx));
    assert!(world.take_effects().is_empty(), "nothing was asked");
    assert_eq!(world.view(cx).address, lazy_client_core::DEFAULT_SERVER_URL);
    // Cancel with the mouse does the same.
    world.click(cx, "server-change");
    world.click(cx, "server-cancel");
    assert!(!world.editing(cx));
    assert!(world.take_effects().is_empty());
}

#[gpui::test]
fn escape_during_composition_cancels_the_composition_and_stays_in_the_field(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.signed_out(cx);
    world.click(cx, "server-change");
    let field = cx
        .update(|window, cx| lazy_ui::text_input::focused_input(window, cx))
        .expect("the field has the keyboard");
    field.update_in(cx, |input, window, cx| {
        input.replace_and_mark_text_in_range(None, "かな", None, window, cx)
    });
    cx.simulate_keystrokes("escape");
    assert!(world.editing(cx), "Escape belonged to the composition");
    assert!(!field.read_with(cx, |input, _| input.is_composing()));
    cx.simulate_keystrokes("escape");
    assert!(!world.editing(cx), "and the next Escape goes back");
}

#[gpui::test]
fn an_address_that_cannot_be_one_is_refused_without_repeating_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.signed_out(cx);
    world.take_effects();
    world.click(cx, "server-change");
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("https://alice:hunter2-secret@chat.example.com/?token=abc");
    cx.simulate_keystrokes("enter");
    assert!(world.checks().is_empty(), "nothing was asked");
    let status = world.view(cx).status;
    match status {
        ServerStatus::Unreachable(reason) => {
            for secret in ["hunter2-secret", "alice", "token=abc"] {
                assert!(
                    !reason.contains(secret),
                    "the refusal repeats {secret:?}: {reason}"
                );
            }
        }
        other => panic!("expected a refusal, found {other:?}"),
    }
    assert!(world.on_screen(cx, "server-status"));
    assert_eq!(
        world.view(cx).address,
        lazy_client_core::DEFAULT_SERVER_URL,
        "what was typed is not kept or shown"
    );
}

#[gpui::test]
fn an_address_from_the_environment_is_shown_locked_with_the_reason(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.dispatch(
        cx,
        Input::Started(StartupState {
            server: ServerInfo {
                url: "http://127.0.0.1:9999".into(),
                source: ServerSource::Environment,
                trying: None,
                check: ServerCheck::Unchecked,
            },
            ..Default::default()
        }),
    );
    draw(cx);
    assert!(world.on_screen(cx, "server-address"));
    assert!(world.on_screen(cx, "server-locked"), "the reason is shown");
    assert!(
        !world.on_screen(cx, "server-change"),
        "there is nothing to click"
    );
    assert!(world.view(cx).locked);
    // It can still be looked at: asked once, and asked again when it does not answer.
    world.answer(cx, Err("no answer".into()));
    assert!(world.on_screen(cx, "server-check"));
    world.click(cx, "server-check");
    assert_eq!(world.checks().len(), 2);
}

// ----- The other screens -----

#[gpui::test]
fn the_unreachable_screen_shows_the_server_and_warns_that_changing_it_forgets_the_session(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.dispatch(
        cx,
        Input::Started(StartupState {
            session_restored: true,
            ..Default::default()
        }),
    );
    world.dispatch(
        cx,
        Input::BootstrapFailed(RequestError::Unknown("timed out".into())),
    );
    draw(cx);
    assert!(matches!(
        world.core(cx, |core| core.phase().clone()),
        SessionPhase::Unreachable { .. }
    ));
    assert!(world.on_screen(cx, "retry-load"));
    assert!(world.on_screen(cx, "server-row"));
    assert!(world.on_screen(cx, "server-address"));
    // "Try again" still has the keyboard when the screen appears, and Tab goes on to the server.
    assert!(!world.keyboard_on_change(cx));
    cx.simulate_keystrokes("tab");
    assert!(
        world.keyboard_on_change(cx),
        "Tab reaches Change… from Try again (it did not reach anything before)"
    );
    cx.simulate_keystrokes("shift-tab");
    assert!(!world.keyboard_on_change(cx));
    world.click(cx, "server-change");
    assert!(world.editing(cx), "the server can be changed from here");
    assert!(
        world.on_screen(cx, "server-removes-session"),
        "and it says what that costs"
    );
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "server-removes-session"));
}

#[gpui::test]
fn the_no_access_screen_shows_the_server_but_says_to_sign_out_to_change_it(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.dispatch(
        cx,
        Input::Started(StartupState {
            session_restored: true,
            ..Default::default()
        }),
    );
    world.dispatch(
        cx,
        Input::BootstrapLoaded(Box::new(BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: user(),
            workspaces: Vec::new(),
        })),
    );
    draw(cx);
    assert!(world.on_screen(cx, "no-access"));
    assert!(world.on_screen(cx, "server-row"));
    assert!(world.on_screen(cx, "server-address"));
    assert!(world.on_screen(cx, "server-blocked"), "the reason is shown");
    world.click(cx, "server-change");
    assert!(
        !world.editing(cx),
        "a signed-in session does not change server"
    );
    // The two buttons the screen had keep the keyboard first.
    assert!(world.on_screen(cx, "no-access-check") && world.on_screen(cx, "no-access-sign-out"));
}

// ----- The register form -----

#[gpui::test]
fn the_register_form_calls_the_code_an_invitation_or_setup_code_and_says_when_it_is_needed(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.signed_out(cx);
    assert!(
        !world.on_screen(cx, "register-code-hint"),
        "only when creating an account"
    );
    world.click(cx, "sign-in-toggle-mode");
    assert!(world.on_screen(cx, "register-code-hint"));
    assert_eq!(
        lazy_ui::states::INVITE_CODE_LABEL,
        "Invitation or setup code (optional)"
    );
    assert!(lazy_ui::states::INVITE_CODE_HINT.len() < 100, "one line");
}

#[test]
fn the_invitation_code_is_read_in_one_place_only() {
    // The form hands the code to the core and nothing else; anything that formats it (a label, a
    // tooltip, a log line) would leak it, so this reads the source and counts.
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sign_in.rs"),
    )
    .expect("the form's source");
    assert_eq!(
        source.matches("invite_code.read(cx).text()").count(),
        1,
        "the code's text is read when the form is submitted and nowhere else"
    );
    assert_eq!(
        source.matches("format!").count(),
        0,
        "nothing in the form formats text"
    );
    assert_eq!(source.matches("tracing::").count(), 0);
    // The field's accessible name is its label, never its content.
    assert!(source.contains("\"Invitation or setup code\""));
}

#[gpui::test]
fn the_sign_in_form_still_works_with_the_server_row_under_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.signed_out(cx);
    world.take_effects();
    cx.simulate_input("alice");
    cx.simulate_keystrokes("tab");
    cx.simulate_input("her-password");
    cx.simulate_keystrokes("enter");
    let signed_in: Vec<String> = world
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Login(request) => Some(request.username.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(signed_in, ["alice"]);
    // While it is under way the server cannot be changed.
    draw(cx);
    assert_eq!(
        world.view(cx).blocked,
        Some(lazy_ui::states::SERVER_WAIT_SIGN_IN)
    );
}

#[gpui::test]
fn a_candidate_that_failed_can_be_dropped_for_the_server_in_use(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.signed_out(cx);
    world.click(cx, "server-change");
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.simulate_input("down.example.com");
    cx.simulate_keystrokes("enter");
    world.answer(cx, Err("timed out".into()));
    assert!(world.core(cx, |core| core.server().trying.is_some()));
    world.click(cx, "server-keep");
    assert!(
        world.core(cx, |core| core.server().trying.is_none()),
        "the candidate is gone and the server in use is asked again"
    );
    assert_eq!(
        world.checks().last().map(|(url, _)| url.as_str()),
        Some(lazy_client_core::DEFAULT_SERVER_URL)
    );
    assert!(!world.on_screen(cx, "server-still-using"));
}

#[gpui::test]
fn in_a_short_window_the_card_scrolls_and_follows_the_keyboard_to_the_server_row(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    cx.simulate_resize(gpui::size(gpui::px(640.), gpui::px(480.)));
    world.signed_out(cx);
    world.click(cx, "sign-in-toggle-mode");
    let card = cx.debug_bounds("sign-in-card").expect("the card");
    assert!(
        card.size.height <= gpui::px(480.),
        "the card is no taller than the window: {card:?}"
    );
    // Tab from the username through the register form to the server row.
    world.click(cx, "sign-in-toggle-mode");
    cx.simulate_keystrokes("shift-tab shift-tab shift-tab");
    for _ in 0..8 {
        if world.keyboard_on_change(cx) {
            break;
        }
        cx.simulate_keystrokes("tab");
    }
    assert!(world.keyboard_on_change(cx), "the keyboard reached Change…");
    draw(cx);
    let change = cx.debug_bounds("server-change").expect("the button");
    let card = cx.debug_bounds("sign-in-card").expect("the card");
    assert!(
        change.top() >= card.top() && change.bottom() <= card.bottom(),
        "the focused button is inside the card's visible part: {change:?} in {card:?}"
    );
    // And back to the top.
    cx.simulate_keystrokes("tab");
    draw(cx);
    let username = cx.debug_bounds("sign-in-card").expect("the card");
    assert!(username.top() >= gpui::px(0.));
}
