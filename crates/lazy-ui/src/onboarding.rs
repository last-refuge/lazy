//! The first run (D29): from "where is my team's server?" to the first conversation, in a few
//! screens that each ask one thing.
//!
//! 1. **Connect.** The server's address and the invitation code the person was given. Lazy checks
//!    the address, remembers it, and asks the server what the code is for, without using it.
//! 2. **You're invited.** The workspace's name and who invited the person: the proof, before any
//!    password exists, that they reached the right server.
//! 3. **Let's set you up.** A name, a username (suggested from the name), and a password. This is
//!    the one step that creates the account.
//! 4. **Add a picture.** Optional, and it only uploads once the account exists.
//! 5. **You're all set.**
//!
//! Whoever joins with the server's setup code is the owner, and two screens come before the last:
//! **Name your workspace** (the server seeds it as "Lazy") and **Invite your team**, which makes
//! the first invitation and offers the whole message to send, so the owner's first run ends with
//! the thing the next person needs.
//!
//! A server too old to preview a code skips the second screen; one that needs no code skips the
//! code. The screens are drawn by this view and nothing else; what they know comes from the core
//! (`server()`, `server_facts()`, `invitation_check()`, the session phase), and what they ask goes
//! to it as inputs. Nothing here performs I/O.
//!
//! The invitation code a person types is read in one place ([`OnboardingView::code_text`]), handed
//! to the core, and never formatted into any text or log line. The first code the owner makes is
//! shown in one selectable field, as in the invitations dialog, which is where it is read.
//!
//! The view stays on screen after the account exists (steps 4 and 5 happen signed in): the root
//! view asks [`OnboardingView::shows`] and puts the shell up when it says no.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, Div, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, IntoElement, ParentElement, PathPromptOptions, Render,
    Stateful, Styled, Subscription, Window, div, prelude::*, px,
};
use gpui::ClipboardItem;
use jiff::tz::TimeZone;
use lazy_client_core::{
    ActionKind, ActionState, FileSource, Input, InvitationCheck, ServerSource, SessionPhase,
};
use lazy_domain::{Timestamp, Username, WorkspaceName};
use lazy_protocol::{InvitationKind, InvitationPreview, ServerRegistration};

use crate::app_model::AppModel;
use crate::avatar::Avatar;
use crate::brand::{self, Moss};
use crate::form::{self, ButtonKind};
use crate::keyboard::{self, FocusNextControl, FocusPrevControl};
use crate::server_row::{self, Tone};
use crate::text_input::{TextInput, TextInputEvent, TextInputOptions};
use crate::theme::Theme;
use crate::workspace_form::{self, Expiry, Uses};

/// The words of the screens.
pub mod copy {
    pub const CONNECT_HEADING: &str = "Welcome to Lazy.";
    pub const CONNECT_SUPPORT: &str = "Connect to your team's server to get started.";
    pub const ADDRESS_LABEL: &str = "Server address";
    pub const ADDRESS_PLACEHOLDER: &str = "chat.example.com";
    pub const ADDRESS_HINT: &str = "The address whoever runs your server gave you.";
    pub const CODE_LABEL: &str = "Invitation code";
    pub const CODE_PLACEHOLDER: &str = "XXXX-XXXX-XXXX-XXXX-XXXX";
    pub const CODE_HINT: &str = "From the same person.";
    pub const CODE_NOT_NEEDED: &str = "This server doesn't need one.";
    pub const CODE_MISSING: &str = "Enter the invitation code you were given.";
    pub const CODE_INVALID: &str =
        "That invitation code isn't valid. It may have expired or been used. Ask for a new one.";
    pub const SERVER_CLOSED: &str = "This server isn't taking new accounts.";
    pub const CODE_UNCHECKED_FAILED: &str = "Couldn't check the code";
    pub const CONTINUE: &str = "Continue";
    pub const CHECKING: &str = "Checking…";
    pub const HAVE_ACCOUNT: &str = "I already have an account";

    pub const SETUP_HEADING: &str = "Set up your workspace.";
    pub const SETUP_SUPPORT: &str = "You're the first one here, so you'll be its owner.";
    pub const JOIN: &str = "Join workspace";
    pub const BACK: &str = "Back";

    pub const ACCOUNT_HEADING: &str = "Let's set you up.";
    pub const ACCOUNT_HEADING_OWNER: &str = "Create your account.";
    pub const ACCOUNT_SUPPORT: &str = "Three things, and you're in.";
    pub const NAME_LABEL: &str = "Your name";
    pub const NAME_PLACEHOLDER: &str = "What should people call you?";
    pub const USERNAME_LABEL: &str = "Username";
    pub const USERNAME_PLACEHOLDER: &str = "username";
    pub const USERNAME_HINT: &str = "How people mention you.";
    pub const PASSWORD_LABEL: &str = "Password";
    pub const PASSWORD_HINT: &str = "At least 8 characters. A short phrase works well.";
    pub const CREATE: &str = "Create account";
    pub const CREATING: &str = "Creating account…";

    pub const PICTURE_HEADING: &str = "Add a picture.";
    pub const PICTURE_SUPPORT: &str = "So people know it's you.";
    pub const CHOOSE_PICTURE: &str = "Choose a picture";
    pub const UPLOADING: &str = "Uploading…";
    pub const SKIP: &str = "Skip for now";

    pub const WORKSPACE_HEADING: &str = "Name your workspace.";
    pub const WORKSPACE_SUPPORT: &str = "It's the first thing people see when you invite them.";
    pub const WORKSPACE_LABEL: &str = "Workspace name";
    pub const WORKSPACE_PLACEHOLDER: &str = "Acme Design";
    pub const WORKSPACE_HINT: &str = "A team, a company, or a project. You can change it later.";
    pub const SAVING: &str = "Saving…";

    pub const SHARE_HEADING: &str = "Invite your team.";
    pub const SHARE_SUPPORT: &str =
        "Send people this message. It has everything they need to get started.";
    pub const SHARE_WAITING: &str = "Making your first invitation…";
    pub const SHARE_FAILED: &str = "Couldn't make an invitation right now.";
    pub const SHARE_RETRY: &str = "Try again";
    pub const GET_LAZY: &str = "Get Lazy";
    pub const SERVER: &str = "Server";
    pub const SHARE_CODE_LABEL: &str = "Invitation code";
    pub const COPY_MESSAGE: &str = "Copy message to send";
    pub const COPIED: &str = "Copied. Paste it into an email or a message.";
    pub const SHARE_MORE: &str = "This code is shown only here. You can make more any time with Invite people.";
    pub const SHARE_CONTINUE: &str = "Continue";

    pub const DONE_SUPPORT: &str = "Find your people and say hello.";
    pub const DONE_SUPPORT_OWNER: &str = "Your workspace is ready for its first hello.";
    pub const OPEN: &str = "Open Lazy";
}

/// Marks an application that opens on the first-run screens (D29) when nothing has been chosen
/// yet. Without it the sign-in card is the first screen, as it always was: the tests and the
/// offscreen renders that have not been moved to the new screens keep working.
struct FirstRun;

impl gpui::Global for FirstRun {}

/// Opens the application on the first-run screens. Called once at startup, before the window.
pub fn enable(cx: &mut gpui::App) {
    cx.set_global(FirstRun);
}

/// Whether [`enable`] was called.
pub fn enabled(cx: &gpui::App) -> bool {
    cx.try_global::<FirstRun>().is_some()
}

/// The windows at least this wide get the brand panel beside the step; narrower ones stack.
pub const WIDE_FROM: f32 = 900.0;
/// How long a screen takes to arrive (the brand allows up to 240 ms).
const ENTRANCE: Duration = Duration::from_millis(220);

/// Where in the sequence the person is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Connect,
    Invited,
    Account,
    Picture,
    /// The owner names the workspace (only after a setup code).
    Workspace,
    /// The owner is given the first invitation to send (only after a setup code).
    Share,
    Done,
}

impl Step {
    /// The number shown ("Step 2 of 4"); the last screen is not a step.
    fn number(self) -> Option<u32> {
        match self {
            Step::Connect => Some(1),
            Step::Invited => Some(2),
            Step::Account => Some(3),
            Step::Picture => Some(4),
            Step::Workspace => Some(5),
            Step::Share => Some(6),
            Step::Done => None,
        }
    }

    /// How many steps there are: two more for the owner.
    fn total(owner: bool) -> u32 {
        if owner { 6 } else { 4 }
    }

    fn index(self) -> u64 {
        match self {
            Step::Connect => 0,
            Step::Invited => 1,
            Step::Account => 2,
            Step::Picture => 3,
            Step::Workspace => 4,
            Step::Share => 5,
            Step::Done => 6,
        }
    }

    /// The pose of Moss that goes with the screen.
    fn moss(self) -> Moss {
        match self {
            Step::Connect | Step::Invited | Step::Share => Moss::Welcome,
            Step::Account | Step::Picture | Step::Workspace => Moss::Primary,
            Step::Done => Moss::Celebrate,
        }
    }
}

/// What the Continue button is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waiting {
    Nothing,
    /// The address is being checked (and moved to).
    Server,
    /// The server is being asked what the code is for.
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    Address,
    Code,
    Name,
    Username,
    Password,
    Workspace,
}

/// What the view tells the root view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingEvent {
    /// The person has an account already: show the sign-in screen.
    HaveAccount,
    /// The last screen was left: show the shell.
    Finished,
}

pub struct OnboardingView {
    model: Entity<AppModel>,
    step: Step,
    address: Entity<TextInput>,
    code: Entity<TextInput>,
    display_name: Entity<TextInput>,
    username: Entity<TextInput>,
    password: Entity<TextInput>,
    workspace: Entity<TextInput>,
    /// The person typed in the username field, so it is no longer suggested from the name.
    username_edited: bool,
    /// What the username field last held that the view put there.
    suggested: String,
    waiting: Waiting,
    /// Why Continue stopped, under the field it is about.
    code_problem: Option<String>,
    /// What the second screen shows.
    invitation: Option<InvitationPreview>,
    /// The account is being created.
    registering: bool,
    /// The account exists (steps 4 and 5 are signed in).
    registered: bool,
    /// A picture was chosen and its upload has been watched.
    picture_started: bool,
    picture_seen_busy: bool,
    picture_error: Option<String>,
    /// The person joined with the setup code: the owner's two extra steps follow the picture.
    owner: bool,
    /// The workspace's new name is being saved.
    naming: bool,
    naming_seen_busy: bool,
    naming_error: Option<String>,
    /// The first invitation's code, in groups of four, in the selectable field it is shown in.
    code_field: Option<Entity<TextInput>>,
    code_shown: Option<String>,
    copied: bool,
    zone: TimeZone,
    continue_focus: FocusHandle,
    account_link_focus: FocusHandle,
    back_focus: FocusHandle,
    join_focus: FocusHandle,
    create_focus: FocusHandle,
    choose_focus: FocusHandle,
    skip_focus: FocusHandle,
    open_focus: FocusHandle,
    name_focus: FocusHandle,
    name_skip_focus: FocusHandle,
    message_focus: FocusHandle,
    share_continue_focus: FocusHandle,
    share_retry_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<OnboardingEvent> for OnboardingView {}

/// A single-line field.
fn field<T: 'static>(
    placeholder: &'static str,
    label: &'static str,
    masked: bool,
    cx: &mut Context<T>,
) -> Entity<TextInput> {
    cx.new(|cx| {
        TextInput::new(
            TextInputOptions::new(placeholder)
                .accessible_label(label)
                .single_line(true)
                .masked(masked)
                .max_bytes(256),
            cx,
        )
    })
}

/// A username to suggest for a name: its first word, lower case, letters and digits only; or, when
/// that is too short to be a username, all the words together. Empty when nothing usable is left.
pub fn suggest_username(display_name: &str) -> String {
    let plain = |text: &str| -> String {
        text.chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    [
        plain(display_name.split_whitespace().next().unwrap_or("")),
        plain(display_name),
    ]
    .into_iter()
    .find(|candidate| Username::parse(candidate).is_ok())
    .unwrap_or_default()
}

/// A different username to offer when one is taken: the same with a number after it.
pub fn alternative_username(username: &str) -> String {
    let base = username.trim_end_matches(|c: char| c.is_ascii_digit());
    let base = if base.is_empty() { username } else { base };
    format!("{base}2")
}

/// "Alice invited you. 12 people are already here." (what is known of it).
pub fn invited_line(preview: &InvitationPreview) -> String {
    let who = preview
        .inviter
        .as_ref()
        .map(|name| format!("{} invited you.", name.as_str()));
    let here = match preview.member_count {
        0 => None,
        1 => Some("1 person is already here.".to_owned()),
        n => Some(format!("{n} people are already here.")),
    };
    [who, here]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether the person must have a code to get in: unless the server said it is open to anyone. A
/// server that said nothing (an older one) is asked for one, and decides.
fn needs_code(facts: Option<lazy_client_core::ServerFacts>) -> bool {
    facts.is_none_or(|facts| {
        facts.registration != ServerRegistration::Open || facts.setup_code_required
    })
}

impl OnboardingView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let address = field(copy::ADDRESS_PLACEHOLDER, copy::ADDRESS_LABEL, false, cx);
        let code = field(copy::CODE_PLACEHOLDER, copy::CODE_LABEL, false, cx);
        let display_name = field(copy::NAME_PLACEHOLDER, copy::NAME_LABEL, false, cx);
        let username = field(copy::USERNAME_PLACEHOLDER, copy::USERNAME_LABEL, false, cx);
        let password = field(copy::PASSWORD_LABEL, copy::PASSWORD_LABEL, true, cx);
        let workspace = field(copy::WORKSPACE_PLACEHOLDER, copy::WORKSPACE_LABEL, false, cx);

        let mut subscriptions = Vec::new();
        for (input, which) in [
            (&address, Which::Address),
            (&code, Which::Code),
            (&display_name, Which::Name),
            (&username, Which::Username),
            (&password, Which::Password),
            (&workspace, Which::Workspace),
        ] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this, _input, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Submit => this.submit(window, cx),
                    TextInputEvent::Tab { backwards } => this.move_focus(*backwards, window, cx),
                    TextInputEvent::Changed => this.field_changed(which, cx),
                    TextInputEvent::UpWhenEmpty
                    | TextInputEvent::PasteAttachments
                    | TextInputEvent::Suggestion(_) => {}
                },
            ));
        }
        subscriptions.push(cx.observe_in(&model, window, |this, _model, window, cx| {
            this.model_changed(window, cx)
        }));

        // A server the person chose before (or one the environment fixed) is already in the field.
        let (known, locked) = {
            let server = model.read(cx).core().server();
            (
                (server.source != ServerSource::Default).then(|| server.url.clone()),
                server.locked(),
            )
        };
        if let Some(url) = &known {
            address.update(cx, |input, cx| input.set_text(url, cx));
        }
        if locked {
            address.update(cx, |input, cx| input.set_enabled(false, cx));
        }
        let first = if known.is_some() { &code } else { &address };
        let handle = first.focus_handle(cx);
        window.focus(&handle, cx);

        let stop = |cx: &mut Context<Self>| cx.focus_handle().tab_stop(true);
        Self {
            model,
            step: Step::Connect,
            address,
            code,
            display_name,
            username,
            password,
            workspace,
            username_edited: false,
            suggested: String::new(),
            waiting: Waiting::Nothing,
            code_problem: None,
            invitation: None,
            registering: false,
            registered: false,
            picture_started: false,
            picture_seen_busy: false,
            picture_error: None,
            owner: false,
            naming: false,
            naming_seen_busy: false,
            naming_error: None,
            code_field: None,
            code_shown: None,
            copied: false,
            zone: TimeZone::system(),
            continue_focus: stop(cx),
            account_link_focus: stop(cx),
            back_focus: stop(cx),
            join_focus: stop(cx),
            create_focus: stop(cx),
            choose_focus: stop(cx),
            skip_focus: stop(cx),
            open_focus: stop(cx),
            name_focus: stop(cx),
            name_skip_focus: stop(cx),
            message_focus: stop(cx),
            share_continue_focus: stop(cx),
            share_retry_focus: stop(cx),
            _subscriptions: subscriptions,
        }
    }

    pub fn step(&self) -> Step {
        self.step
    }

    /// Whether the person joined with the setup code, and so is the workspace's owner.
    pub fn is_owner(&self) -> bool {
        self.owner
    }

    /// Whether the steps should be counted for an owner: from the moment the server has said the
    /// code is the setup code.
    fn owner_flow(&self) -> bool {
        self.owner || self.is_setup()
    }

    /// Whether the code the person entered is the server's setup code.
    fn is_setup(&self) -> bool {
        self.invitation
            .as_ref()
            .is_some_and(|preview| preview.kind == InvitationKind::Setup)
    }

    /// Whether this view has the screen in `phase`: while the person is joining (signed out,
    /// signing in, and the load that follows creating the account), and afterwards until the last
    /// screen is left.
    pub fn shows(&self, phase: &SessionPhase) -> bool {
        match phase {
            SessionPhase::SignedOut { .. } | SessionPhase::SigningIn => !self.registered,
            SessionPhase::Loading => self.registering,
            SessionPhase::Ready => self.registered || self.registering,
            SessionPhase::Unreachable { .. } => false,
        }
    }

    /// The invitation code, trimmed. The one place its text is read.
    fn code_text(&self, cx: &gpui::App) -> String {
        self.code.read(cx).text().trim().to_owned()
    }

    fn focus_on(&self, handle: &FocusHandle, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(handle, cx);
    }

    fn go(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        self.step = step;
        match step {
            Step::Connect => {
                let handle = self.code.focus_handle(cx);
                self.focus_on(&handle, window, cx);
            }
            Step::Invited => {
                let handle = self.join_focus.clone();
                self.focus_on(&handle, window, cx);
            }
            Step::Account => {
                let handle = self.display_name.focus_handle(cx);
                self.focus_on(&handle, window, cx);
            }
            Step::Picture => {
                let handle = self.choose_focus.clone();
                self.focus_on(&handle, window, cx);
            }
            Step::Workspace => {
                let handle = self.workspace.focus_handle(cx);
                self.focus_on(&handle, window, cx);
            }
            Step::Share => {
                // The first thing to do is to copy; until the code arrives, there is nothing else.
                let handle = self.share_continue_focus.clone();
                self.focus_on(&handle, window, cx);
            }
            Step::Done => {
                let handle = self.open_focus.clone();
                self.focus_on(&handle, window, cx);
            }
        }
        cx.notify();
    }

    fn move_focus(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        if backwards {
            window.focus_prev(cx);
        } else {
            window.focus_next(cx);
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.step {
            Step::Connect => self.connect(cx),
            Step::Account => self.create_account(cx),
            Step::Workspace => self.name_workspace(window, cx),
            Step::Share => self.copy_message(cx),
            Step::Invited | Step::Picture | Step::Done => {}
        }
    }

    fn field_changed(&mut self, which: Which, cx: &mut Context<Self>) {
        match which {
            Which::Address | Which::Code => {
                // What was said about them no longer is.
                self.code_problem = None;
                cx.notify();
            }
            Which::Name => {
                if !self.username_edited {
                    let suggestion = suggest_username(self.display_name.read(cx).text());
                    self.suggested.clone_from(&suggestion);
                    let username = self.username.clone();
                    username.update(cx, |input, cx| input.set_text(&suggestion, cx));
                }
                cx.notify();
            }
            Which::Username => {
                if self.username.read(cx).text() != self.suggested {
                    self.username_edited = true;
                }
                cx.notify();
            }
            Which::Password => cx.notify(),
            Which::Workspace => {
                // What was said about the name no longer is.
                self.naming_error = None;
                cx.notify();
            }
        }
    }

    // ----- Step 1: connect -----

    /// Continue on the first screen: check the address, then the code.
    fn connect(&mut self, cx: &mut Context<Self>) {
        if self.waiting != Waiting::Nothing || self.step != Step::Connect {
            return;
        }
        self.code_problem = None;
        let locked = server_row::server_view(self.model.read(cx).core()).locked;
        let typed = self.address.read(cx).text().trim().to_owned();
        self.waiting = Waiting::Server;
        // The answer arrives through `model_changed`.
        let input = if locked {
            Input::CheckServer
        } else {
            Input::SetServerUrl(typed)
        };
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.notify();
    }

    fn model_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.step {
            Step::Connect => self.connect_progress(window, cx),
            Step::Account => self.account_progress(window, cx),
            Step::Picture => self.picture_progress(window, cx),
            Step::Workspace => self.workspace_progress(window, cx),
            Step::Share => self.share_progress(window, cx),
            Step::Invited | Step::Done => {}
        }
        cx.notify();
    }

    fn connect_progress(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.waiting {
            Waiting::Nothing => {}
            Waiting::Server => self.server_progress(window, cx),
            Waiting::Code => self.code_progress(window, cx),
        }
    }

    /// The address has been asked: when it has settled, ask about the code.
    fn server_progress(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (view, facts) = {
            let core = self.model.read(cx).core();
            (server_row::server_view(core), core.server_facts())
        };
        match view.status {
            // Still asking, or still moving to the address.
            server_row::ServerStatus::Unchecked | server_row::ServerStatus::Checking => {}
            server_row::ServerStatus::Connected if view.still_using.is_some() => {}
            // The status line under the address says why.
            server_row::ServerStatus::NeedsNewerLazy
            | server_row::ServerStatus::NeedsOlderLazy
            | server_row::ServerStatus::Unreachable(_) => self.waiting = Waiting::Nothing,
            server_row::ServerStatus::Connected => {
                let code = self.code_text(cx);
                if facts.is_some_and(|facts| facts.registration == ServerRegistration::Closed) {
                    self.waiting = Waiting::Nothing;
                    self.code_problem = Some(copy::SERVER_CLOSED.to_owned());
                } else if code.is_empty() {
                    if needs_code(facts) {
                        self.waiting = Waiting::Nothing;
                        self.code_problem = Some(copy::CODE_MISSING.to_owned());
                    } else {
                        // Open to anyone: nothing to preview.
                        self.waiting = Waiting::Nothing;
                        self.invitation = None;
                        self.go(Step::Account, window, cx);
                    }
                } else {
                    self.waiting = Waiting::Code;
                    self.model.update(cx, |model, cx| {
                        model.dispatch(Input::PreviewInvitation { code }, cx)
                    });
                }
            }
        }
    }

    /// The code has been asked about: what the server said.
    fn code_progress(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let check = self.model.read(cx).core().invitation_check().clone();
        match check {
            InvitationCheck::Checking => {}
            // Nothing was asked after all (the core had nobody to ask): stop waiting.
            InvitationCheck::Idle => self.waiting = Waiting::Nothing,
            InvitationCheck::Valid(preview) => {
                self.waiting = Waiting::Nothing;
                self.invitation = Some(preview);
                self.go(Step::Invited, window, cx);
            }
            // An older server cannot say; the code is checked when the account is created.
            InvitationCheck::Unchecked => {
                self.waiting = Waiting::Nothing;
                self.invitation = None;
                self.go(Step::Account, window, cx);
            }
            InvitationCheck::Invalid => {
                self.waiting = Waiting::Nothing;
                self.code_problem = Some(copy::CODE_INVALID.to_owned());
            }
            InvitationCheck::Closed => {
                self.waiting = Waiting::Nothing;
                self.code_problem = Some(copy::SERVER_CLOSED.to_owned());
            }
            InvitationCheck::Failed(reason) => {
                self.waiting = Waiting::Nothing;
                self.code_problem = Some(format!("{}: {reason}.", copy::CODE_UNCHECKED_FAILED));
            }
        }
    }

    // ----- Step 3: the account -----

    /// "Create account": the one request that makes the account.
    fn create_account(&mut self, cx: &mut Context<Self>) {
        if self.registering || self.step != Step::Account {
            return;
        }
        let input = Input::Register {
            username: self.username.read(cx).text().trim().to_owned(),
            display_name: self.display_name.read(cx).text().trim().to_owned(),
            password: self.password.read(cx).text().to_owned(),
            invite_code: self.code_text(cx),
        };
        self.registering = true;
        self.owner = self.is_setup();
        // A refusal comes back at once, through `account_progress`, as the phase.
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.notify();
    }

    fn account_progress(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let phase = self.model.read(cx).core().phase().clone();
        match phase {
            SessionPhase::SignedOut { .. } => self.registering = false,
            SessionPhase::Ready if self.registering => {
                self.registering = false;
                self.registered = true;
                self.go(Step::Picture, window, cx);
            }
            _ => {}
        }
    }

    // ----- Step 4: the picture -----

    /// Opens the system's file dialog for the picture.
    fn choose_picture(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            this.update(cx, |view, cx| match result {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => {
                    view.picture_chosen(paths.remove(0), cx)
                }
                Ok(Err(error)) => {
                    view.picture_error =
                        Some(crate::attachments::picker_failed_text(&error.to_string()));
                    cx.notify();
                }
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    /// The picture the person chose. (Public so tests and the offscreen renders, which have no
    /// file dialog to answer, can stand in for it.)
    pub fn picture_chosen(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.picture_error = None;
        self.picture_started = true;
        self.picture_seen_busy = false;
        self.model.update(cx, |model, cx| {
            model.dispatch(
                Input::SetAvatar {
                    source: FileSource::Path(path),
                },
                cx,
            )
        });
        cx.notify();
    }

    fn picture_progress(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.picture_started {
            return;
        }
        let (state, has_picture) = {
            let core = self.model.read(cx).core();
            (
                core.action(ActionKind::SetAvatar).clone(),
                core.me().is_some_and(|me| me.avatar_version > 0),
            )
        };
        match state {
            ActionState::InFlight => self.picture_seen_busy = true,
            ActionState::Failed(message) => {
                self.picture_started = false;
                self.picture_error = Some(message);
            }
            ActionState::Idle if has_picture || self.picture_seen_busy => {
                self.picture_started = false;
                self.after_picture(window, cx);
            }
            ActionState::Idle => {}
        }
    }

    /// The picture step is over (chosen or skipped): the owner goes on to name the workspace.
    fn after_picture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = if self.owner { Step::Workspace } else { Step::Done };
        self.go(next, window, cx);
    }

    // ----- Step 5 (owner): the workspace's name -----

    /// What the name field holds, parsed; `None` while it is empty or not a name.
    fn workspace_name_choice(&self, cx: &gpui::App) -> Option<WorkspaceName> {
        WorkspaceName::parse(self.workspace.read(cx).text()).ok()
    }

    /// "Continue": rename the workspace, and go on when the server has agreed.
    fn name_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.naming || self.step != Step::Workspace {
            return;
        }
        let Some(name) = self.workspace_name_choice(cx) else {
            return;
        };
        let current = self
            .model
            .read(cx)
            .core()
            .primary_workspace()
            .map(|info| info.workspace.name.clone());
        if current.as_deref() == Some(name.as_str()) {
            // Nothing to change.
            self.begin_share(window, cx);
            return;
        }
        self.naming = true;
        self.naming_seen_busy = false;
        self.naming_error = None;
        let input = Input::UpdateWorkspace {
            name: Some(name.as_str().to_owned()),
            settings: None,
        };
        self.model.update(cx, |model, cx| model.dispatch(input, cx));
        cx.notify();
    }

    fn workspace_progress(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.naming {
            return;
        }
        let state = self
            .model
            .read(cx)
            .core()
            .action(ActionKind::UpdateWorkspace)
            .clone();
        match state {
            ActionState::InFlight => self.naming_seen_busy = true,
            ActionState::Failed(message) => {
                self.naming = false;
                self.naming_error = Some(message);
            }
            ActionState::Idle if self.naming_seen_busy => {
                self.naming = false;
                self.begin_share(window, cx);
            }
            ActionState::Idle => {}
        }
    }

    // ----- Step 6 (owner): the first invitation -----

    /// Asks for the first invitation (a week, for up to 25 people) and shows the step that waits
    /// for it.
    fn begin_share(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_for_invitation(cx);
        self.go(Step::Share, window, cx);
    }

    fn ask_for_invitation(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::OpenInvitations, cx);
            model.dispatch(
                Input::CreateInvitation {
                    expires_in_secs: Some(Expiry::DEFAULT.secs()),
                    max_uses: Uses::TwentyFive.max(),
                },
                cx,
            );
        });
    }

    /// Shows the code in its field the moment it arrives.
    fn share_progress(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let code = self
            .model
            .read(cx)
            .core()
            .invitations()
            .created
            .as_ref()
            .map(crate::dialogs::shown_code);
        let (Some(code), None) = (code, &self.code_shown) else {
            return;
        };
        let field = cx.new(|cx| {
            TextInput::new(
                TextInputOptions::new("")
                    .accessible_label(copy::SHARE_CODE_LABEL)
                    .single_line(true)
                    .max_bytes(128),
                cx,
            )
        });
        field.update(cx, |input, cx| input.set_text(&code, cx));
        // The field is for selecting and copying: typing in it changes nothing.
        let restore = cx.subscribe_in(
            &field,
            window,
            |this, input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Changed => {
                    if let Some(code) = this.code_shown.clone() {
                        input.update(cx, |input, cx| input.set_text(&code, cx));
                    }
                }
                TextInputEvent::Tab { backwards } => this.move_focus(*backwards, window, cx),
                TextInputEvent::Submit => this.copy_message(cx),
                _ => {}
            },
        );
        self._subscriptions.push(restore);
        self.code_field = Some(field);
        self.code_shown = Some(code);
        self.copied = false;
        // Copying is what to do next.
        let handle = self.message_focus.clone();
        window.focus(&handle, cx);
    }

    /// Copies the whole message to send: where to get Lazy, the server, the code, and for how long
    /// and for how many it works.
    fn copy_message(&mut self, cx: &mut Context<Self>) {
        let Some(code) = self.code_shown.clone() else {
            return;
        };
        let (server, created) = {
            let core = self.model.read(cx).core();
            (
                core.server().url.clone(),
                core.invitations().created.clone(),
            )
        };
        let Some(created) = created else {
            return;
        };
        let message = workspace_form::invitation_message(
            &server,
            &code,
            &created.invitation,
            Timestamp::now(),
            &self.zone,
        );
        cx.write_to_clipboard(ClipboardItem::new_string(message));
        self.copied = true;
        cx.notify();
    }

    fn retry_invitation(&mut self, cx: &mut Context<Self>) {
        self.ask_for_invitation(cx);
        cx.notify();
    }

    /// Lets go of the code (it is shown once) and of the actions this step used.
    fn release_invitation(&mut self, cx: &mut Context<Self>) {
        self.code_field = None;
        self.code_shown = None;
        self.copied = false;
        self.model.update(cx, |model, cx| {
            model.dispatch(Input::CloseInvitations, cx);
            model.dispatch(Input::DismissAction(ActionKind::CreateInvitation), cx);
            model.dispatch(Input::DismissAction(ActionKind::UpdateWorkspace), cx);
        });
    }

    /// "Continue" on the invitation step.
    fn leave_share(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.release_invitation(cx);
        self.go(Step::Done, window, cx);
    }

    /// The last screen was left.
    fn finish(&mut self, cx: &mut Context<Self>) {
        self.registered = false;
        cx.emit(OnboardingEvent::Finished);
        cx.notify();
    }

    // ----- Drawing -----

    fn tone_color(theme: &Theme, tone: Tone) -> gpui::Hsla {
        let colors = theme.colors;
        match tone {
            Tone::Neutral => colors.text_muted,
            Tone::Good => colors.success,
            Tone::Warning => colors.warning,
            Tone::Bad => colors.danger,
        }
    }

    /// A note under a field, in words (the color only adds to them).
    fn note(theme: &Theme, id: &'static str, text: String, tone: Tone) -> AnyElement {
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .text_size(theme.type_scale.small)
            .text_color(Self::tone_color(theme, tone))
            .child(text)
            .into_any_element()
    }

    /// A labelled field with an optional note under it.
    fn labeled(
        theme: &Theme,
        label: &'static str,
        input: &Entity<TextInput>,
        note: Option<AnyElement>,
        window: &Window,
        cx: &gpui::App,
    ) -> AnyElement {
        let colors = theme.colors;
        let focused = input.read(cx).is_focused(window);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(theme.type_scale.small)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text_muted)
                    .child(label),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(theme.radii.medium)
                    .bg(colors.surface_raised)
                    .border_1()
                    .border_color(if focused {
                        colors.focus_ring
                    } else {
                        colors.border_strong
                    })
                    .child(input.clone()),
            )
            .children(note)
            .into_any_element()
    }

    fn title(theme: &Theme, id: &'static str, text: String) -> AnyElement {
        crate::fonts::heading(
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .text_size(px(28.))
                .text_color(theme.colors.text)
                .child(text),
        )
        .into_any_element()
    }

    fn support(theme: &Theme, id: &'static str, text: String) -> AnyElement {
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .text_size(theme.type_scale.body)
            .text_color(theme.colors.text_muted)
            .child(text)
            .into_any_element()
    }

    fn kicker(&self, theme: &Theme) -> Option<AnyElement> {
        let number = self.step.number()?;
        // What the server says about the code is not known on the first screen, and the owner has
        // two more steps than anyone else: it does not promise a total before it is known.
        let text = if self.step == Step::Connect {
            format!("Step {number}")
        } else {
            format!("Step {number} of {}", Step::total(self.owner_flow()))
        };
        Some(
            div()
                .id("onboarding-step-number")
                .debug_selector(|| "onboarding-step-number".into())
                .text_size(theme.type_scale.small)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.colors.text_faint)
                .child(text)
                .into_any_element(),
        )
    }

    /// The error box of the account screen: what the server or the core refused.
    fn problem_box(theme: &Theme, text: String) -> AnyElement {
        div()
            .id("onboarding-problem")
            .debug_selector(|| "onboarding-problem".into())
            .role(gpui::Role::Alert)
            .px_3()
            .py_2()
            .rounded(theme.radii.small)
            .bg(theme.colors.danger_surface)
            .text_size(theme.type_scale.small)
            .text_color(theme.colors.danger)
            .child(text)
            .into_any_element()
    }

    fn connect_card(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let (view, facts) = {
            let core = self.model.read(cx).core();
            (server_row::server_view(core), core.server_facts())
        };
        let busy = self.waiting != Waiting::Nothing;
        let address_note = match server_row::status_text(&view.status) {
            Some((text, tone)) => Self::note(theme, "onboarding-address-status", text, tone),
            None => Self::note(
                theme,
                "onboarding-address-hint",
                copy::ADDRESS_HINT.to_owned(),
                Tone::Neutral,
            ),
        };
        let code_note = match &self.code_problem {
            Some(problem) => {
                Self::note(theme, "onboarding-code-problem", problem.clone(), Tone::Bad)
            }
            None if facts.is_some() && !needs_code(facts) => Self::note(
                theme,
                "onboarding-code-hint",
                copy::CODE_NOT_NEEDED.to_owned(),
                Tone::Neutral,
            ),
            None => Self::note(
                theme,
                "onboarding-code-hint",
                copy::CODE_HINT.to_owned(),
                Tone::Neutral,
            ),
        };
        let this = cx.entity().downgrade();
        let connect_to = this.clone();
        let account_to = this;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(self.kicker(theme))
            .child(Self::title(
                theme,
                "onboarding-title",
                copy::CONNECT_HEADING.into(),
            ))
            .child(Self::support(
                theme,
                "onboarding-support",
                copy::CONNECT_SUPPORT.into(),
            ))
            .child(Self::labeled(
                theme,
                copy::ADDRESS_LABEL,
                &self.address,
                Some(address_note),
                window,
                cx,
            ))
            .child(Self::labeled(
                theme,
                copy::CODE_LABEL,
                &self.code,
                Some(code_note),
                window,
                cx,
            ))
            .child(
                form::button(
                    theme,
                    "onboarding-continue",
                    if busy { copy::CHECKING } else { copy::CONTINUE },
                    ButtonKind::Primary,
                    !busy,
                    &self.continue_focus,
                    move |_, cx| {
                        connect_to.update(cx, |view, cx| view.connect(cx)).ok();
                    },
                )
                .w_full(),
            )
            .child(
                form::button(
                    theme,
                    "onboarding-have-account",
                    copy::HAVE_ACCOUNT,
                    ButtonKind::Secondary,
                    true,
                    &self.account_link_focus,
                    move |_, cx| {
                        account_to
                            .update(cx, |_, cx| cx.emit(OnboardingEvent::HaveAccount))
                            .ok();
                    },
                )
                .w_full(),
            )
            .into_any_element()
    }

    fn invited_card(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let setup = self.is_setup();
        let (heading, support) = match &self.invitation {
            Some(preview) if !setup => (
                format!("You're invited to {}.", preview.workspace_name),
                invited_line(preview),
            ),
            _ => (
                copy::SETUP_HEADING.to_owned(),
                copy::SETUP_SUPPORT.to_owned(),
            ),
        };
        let this = cx.entity().downgrade();
        let join_to = this.clone();
        let back_to = this;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(self.kicker(theme))
            .child(Self::title(theme, "onboarding-title", heading))
            .child(Self::support(theme, "onboarding-support", support))
            .child(
                form::button(
                    theme,
                    "onboarding-join",
                    if setup { copy::CONTINUE } else { copy::JOIN },
                    ButtonKind::Primary,
                    true,
                    &self.join_focus,
                    move |window, cx| {
                        join_to
                            .update(cx, |view, cx| view.go(Step::Account, window, cx))
                            .ok();
                    },
                )
                .w_full(),
            )
            .child(
                form::button(
                    theme,
                    "onboarding-back",
                    copy::BACK,
                    ButtonKind::Secondary,
                    true,
                    &self.back_focus,
                    move |window, cx| {
                        back_to
                            .update(cx, |view, cx| view.go(Step::Connect, window, cx))
                            .ok();
                    },
                )
                .w_full(),
            )
            .into_any_element()
    }

    fn account_card(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let setup = self.is_setup();
        let typed = self.username.read(cx).text().trim().to_owned();
        let problem = match self.model.read(cx).core().phase() {
            SessionPhase::SignedOut {
                last_error: Some(message),
            } => Some(Self::problem_text(message, &typed)),
            _ => None,
        };
        let this = cx.entity().downgrade();
        let create_to = this.clone();
        let back_to = this;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(self.kicker(theme))
            .child(Self::title(
                theme,
                "onboarding-title",
                if setup {
                    copy::ACCOUNT_HEADING_OWNER
                } else {
                    copy::ACCOUNT_HEADING
                }
                .into(),
            ))
            .child(Self::support(
                theme,
                "onboarding-support",
                copy::ACCOUNT_SUPPORT.into(),
            ))
            .child(Self::labeled(
                theme,
                copy::NAME_LABEL,
                &self.display_name,
                None,
                window,
                cx,
            ))
            .child(Self::labeled(
                theme,
                copy::USERNAME_LABEL,
                &self.username,
                Some(Self::note(
                    theme,
                    "onboarding-username-hint",
                    copy::USERNAME_HINT.to_owned(),
                    Tone::Neutral,
                )),
                window,
                cx,
            ))
            .child(Self::labeled(
                theme,
                copy::PASSWORD_LABEL,
                &self.password,
                Some(Self::note(
                    theme,
                    "onboarding-password-hint",
                    copy::PASSWORD_HINT.to_owned(),
                    Tone::Neutral,
                )),
                window,
                cx,
            ))
            .children(problem.map(|text| Self::problem_box(theme, text)))
            .child(
                form::button(
                    theme,
                    "onboarding-create",
                    if self.registering {
                        copy::CREATING
                    } else {
                        copy::CREATE
                    },
                    ButtonKind::Primary,
                    !self.registering,
                    &self.create_focus,
                    move |_, cx| {
                        create_to
                            .update(cx, |view, cx| view.create_account(cx))
                            .ok();
                    },
                )
                .w_full(),
            )
            .child(
                form::button(
                    theme,
                    "onboarding-account-back",
                    copy::BACK,
                    ButtonKind::Secondary,
                    !self.registering,
                    &self.back_focus,
                    move |window, cx| {
                        back_to
                            .update(cx, |view, cx| {
                                let back = if view.invitation.is_some() {
                                    Step::Invited
                                } else {
                                    Step::Connect
                                };
                                view.go(back, window, cx)
                            })
                            .ok();
                    },
                )
                .w_full(),
            )
            .into_any_element()
    }

    /// What a refusal of the account says: the core's words, and for a name that is taken a way
    /// out.
    fn problem_text(message: &str, username: &str) -> String {
        if message.to_lowercase().contains("taken") && !username.is_empty() {
            return format!(
                "That username is taken. Try {}.",
                alternative_username(username)
            );
        }
        message.to_owned()
    }

    fn picture_card(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (me, busy) = {
            let core = self.model.read(cx).core();
            (
                core.me().cloned(),
                self.picture_started
                    && *core.action(ActionKind::SetAvatar) == ActionState::InFlight,
            )
        };
        let seed = me.as_ref().map_or(0, |me| me.id.as_uuid().as_u128());
        let name = me
            .as_ref()
            .map(|me| me.display_name.to_string())
            .unwrap_or_default();
        if let Some(me) = &me {
            crate::avatar_images::request_later(&self.model, vec![me.id], cx);
        }
        let image = me.as_ref().and_then(|me| {
            crate::avatar_images::image_of(
                self.model.read(cx).core(),
                crate::image_store::ImageStore::global(cx).read(cx),
                me.id,
            )
        });
        let this = cx.entity().downgrade();
        let choose_to = this.clone();
        let skip_to = this;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(self.kicker(theme))
            .child(Self::title(
                theme,
                "onboarding-title",
                copy::PICTURE_HEADING.into(),
            ))
            .child(Self::support(
                theme,
                "onboarding-support",
                copy::PICTURE_SUPPORT.into(),
            ))
            .child(
                div().flex().justify_center().py_2().child(
                    div()
                        .id("onboarding-avatar")
                        .debug_selector(|| "onboarding-avatar".into())
                        .child(Avatar::new(seed, name, 112.).image(image).into_div()),
                ),
            )
            .children(
                self.picture_error
                    .clone()
                    .map(|text| Self::problem_box(theme, text)),
            )
            .child(
                form::button(
                    theme,
                    "onboarding-choose-picture",
                    if busy {
                        copy::UPLOADING
                    } else {
                        copy::CHOOSE_PICTURE
                    },
                    ButtonKind::Primary,
                    !busy,
                    &self.choose_focus,
                    move |_, cx| {
                        choose_to
                            .update(cx, |view, cx| view.choose_picture(cx))
                            .ok();
                    },
                )
                .w_full(),
            )
            .child(
                form::button(
                    theme,
                    "onboarding-skip",
                    copy::SKIP,
                    ButtonKind::Secondary,
                    !busy,
                    &self.skip_focus,
                    move |window, cx| {
                        skip_to
                            .update(cx, |view, cx| view.after_picture(window, cx))
                            .ok();
                    },
                )
                .w_full(),
            )
            .into_any_element()
    }

    fn workspace_card(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let typed = self.workspace.read(cx).text().to_owned();
        // An empty field is not scolded; a name that cannot be used says why.
        let problem = if typed.trim().is_empty() {
            None
        } else {
            WorkspaceName::parse(&typed)
                .err()
                .map(|error| crate::states::workspace_name_problem(&error))
        };
        let can_continue = !self.naming && self.workspace_name_choice(cx).is_some();
        let note = match problem {
            Some(text) => Self::note(theme, "onboarding-workspace-problem", text, Tone::Bad),
            None => Self::note(
                theme,
                "onboarding-workspace-hint",
                copy::WORKSPACE_HINT.to_owned(),
                Tone::Neutral,
            ),
        };
        let this = cx.entity().downgrade();
        let continue_to = this.clone();
        let skip_to = this;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(self.kicker(theme))
            .child(Self::title(
                theme,
                "onboarding-title",
                copy::WORKSPACE_HEADING.into(),
            ))
            .child(Self::support(
                theme,
                "onboarding-support",
                copy::WORKSPACE_SUPPORT.into(),
            ))
            .child(Self::labeled(
                theme,
                copy::WORKSPACE_LABEL,
                &self.workspace,
                Some(note),
                window,
                cx,
            ))
            .children(
                self.naming_error
                    .clone()
                    .map(|text| Self::problem_box(theme, text)),
            )
            .child(
                form::button(
                    theme,
                    "onboarding-workspace-continue",
                    if self.naming {
                        copy::SAVING
                    } else {
                        copy::CONTINUE
                    },
                    ButtonKind::Primary,
                    can_continue,
                    &self.name_focus,
                    move |window, cx| {
                        continue_to
                            .update(cx, |view, cx| view.name_workspace(window, cx))
                            .ok();
                    },
                )
                .w_full(),
            )
            .child(
                form::button(
                    theme,
                    "onboarding-workspace-skip",
                    copy::SKIP,
                    ButtonKind::Secondary,
                    !self.naming,
                    &self.name_skip_focus,
                    move |window, cx| {
                        skip_to
                            .update(cx, |view, cx| view.begin_share(window, cx))
                            .ok();
                    },
                )
                .w_full(),
            )
            .into_any_element()
    }

    /// A line of the message the owner is about to send: what it is, and what it says.
    fn message_row(theme: &Theme, id: &'static str, label: &'static str, value: String) -> Div {
        div()
            .flex()
            .items_baseline()
            .gap_3()
            .child(
                div()
                    .w(px(64.))
                    .flex_none()
                    .text_size(theme.type_scale.small)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text_muted)
                    .child(label),
            )
            .child(
                div()
                    .id(id)
                    .debug_selector(move || id.to_owned())
                    .flex_1()
                    .min_w_0()
                    .text_size(theme.type_scale.body)
                    .text_color(theme.colors.text)
                    .child(value),
            )
    }

    fn share_card(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let colors = theme.colors;
        let (state, server, created) = {
            let core = self.model.read(cx).core();
            (
                core.action(ActionKind::CreateInvitation).clone(),
                core.server().url.clone(),
                core.invitations().created.clone(),
            )
        };
        let this = cx.entity().downgrade();
        let copy_to = this.clone();
        let retry_to = this.clone();
        let continue_to = this;
        let body = match (&self.code_field, &created) {
            (Some(field), Some(created)) => {
                let facts = [
                    Some(workspace_form::capacity_text(created.invitation.max_uses)),
                    workspace_form::expiry_text(&created.invitation, Timestamp::now(), &self.zone),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                let shown_server = server.strip_prefix("https://").unwrap_or(&server).to_owned();
                let releases = lazy_client_core::RELEASES_URL;
                let releases = releases.strip_prefix("https://").unwrap_or(releases);
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .id("onboarding-message")
                            .debug_selector(|| "onboarding-message".into())
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_4()
                            .rounded(theme.radii.medium)
                            .bg(colors.accent_surface)
                            .border_1()
                            .border_color(colors.accent)
                            .child(Self::message_row(
                                theme,
                                "onboarding-message-get",
                                copy::GET_LAZY,
                                releases.to_owned(),
                            ))
                            .child(Self::message_row(
                                theme,
                                "onboarding-message-server",
                                copy::SERVER,
                                shown_server,
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(theme.type_scale.small)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(colors.text_muted)
                                            .child(copy::SHARE_CODE_LABEL),
                                    )
                                    .child(
                                        div()
                                            .px_3()
                                            .py_2()
                                            .rounded(theme.radii.medium)
                                            .bg(colors.surface_raised)
                                            .border_1()
                                            .border_color(if field.read(cx).is_focused(window) {
                                                colors.focus_ring
                                            } else {
                                                colors.border_strong
                                            })
                                            .font_family(crate::fonts::MONO_FAMILY)
                                            .text_size(theme.type_scale.body)
                                            .child(field.clone()),
                                    ),
                            )
                            .children((!facts.is_empty()).then(|| {
                                div()
                                    .text_size(theme.type_scale.small)
                                    .text_color(colors.text_muted)
                                    .child(facts)
                            })),
                    )
                    .child(
                        form::button(
                            theme,
                            "onboarding-copy-message",
                            copy::COPY_MESSAGE,
                            ButtonKind::Primary,
                            true,
                            &self.message_focus,
                            move |_, cx| {
                                copy_to.update(cx, |view, cx| view.copy_message(cx)).ok();
                            },
                        )
                        .w_full(),
                    )
                    .child(
                        div()
                            .id("onboarding-copied")
                            .debug_selector(|| "onboarding-copied".into())
                            .text_size(theme.type_scale.small)
                            .text_color(if self.copied {
                                colors.success
                            } else {
                                colors.text_muted
                            })
                            .child(if self.copied {
                                copy::COPIED
                            } else {
                                copy::SHARE_MORE
                            }),
                    )
                    .into_any_element()
            }
            _ if state == ActionState::InFlight => Self::note(
                theme,
                "onboarding-share-waiting",
                copy::SHARE_WAITING.to_owned(),
                Tone::Neutral,
            ),
            // Failed, or it was never asked: say so and offer to ask again.
            _ => {
                let reason = match &state {
                    ActionState::Failed(message) => message.clone(),
                    _ => copy::SHARE_FAILED.to_owned(),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(Self::problem_box(theme, reason))
                    .child(
                        form::button(
                            theme,
                            "onboarding-share-retry",
                            copy::SHARE_RETRY,
                            ButtonKind::Primary,
                            true,
                            &self.share_retry_focus,
                            move |_, cx| {
                                retry_to
                                    .update(cx, |view, cx| view.retry_invitation(cx))
                                    .ok();
                            },
                        )
                        .w_full(),
                    )
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .children(self.kicker(theme))
            .child(Self::title(
                theme,
                "onboarding-title",
                copy::SHARE_HEADING.into(),
            ))
            .child(Self::support(
                theme,
                "onboarding-support",
                copy::SHARE_SUPPORT.into(),
            ))
            .child(body)
            .child(
                form::button(
                    theme,
                    "onboarding-share-continue",
                    copy::SHARE_CONTINUE,
                    if self.code_shown.is_some() && !self.copied {
                        ButtonKind::Secondary
                    } else {
                        ButtonKind::Primary
                    },
                    true,
                    &self.share_continue_focus,
                    move |window, cx| {
                        continue_to
                            .update(cx, |view, cx| view.leave_share(window, cx))
                            .ok();
                    },
                )
                .w_full(),
            )
            .into_any_element()
    }

    fn done_card(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let first_name = self
            .model
            .read(cx)
            .core()
            .me()
            .and_then(|me| me.display_name.as_str().split_whitespace().next())
            .map(str::to_owned);
        let heading = match first_name {
            Some(name) => format!("You're all set, {name}."),
            None => "You're all set.".to_owned(),
        };
        let tip = format!(
            "Press {} any time to jump to a person or a channel.",
            crate::commands::shortcut_text("secondary-k")
        );
        let this = cx.entity().downgrade();
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(Self::title(theme, "onboarding-title", heading))
            .child(Self::support(
                theme,
                "onboarding-support",
                if self.owner {
                    copy::DONE_SUPPORT_OWNER
                } else {
                    copy::DONE_SUPPORT
                }
                .into(),
            ))
            .child(
                div()
                    .id("onboarding-tip")
                    .debug_selector(|| "onboarding-tip".into())
                    .text_size(theme.type_scale.small)
                    .text_color(theme.colors.text_faint)
                    .child(tip),
            )
            .child(
                form::button(
                    theme,
                    "onboarding-open",
                    copy::OPEN,
                    ButtonKind::Primary,
                    true,
                    &self.open_focus,
                    move |_, cx| {
                        this.update(cx, |view, cx| view.finish(cx)).ok();
                    },
                )
                .w_full(),
            )
            .into_any_element()
    }

    /// The brand panel: the logo, Moss, and the tagline, on Forest. On a narrow window it is a
    /// header above the step.
    fn panel(&self, theme: &Theme, wide: bool, cx: &gpui::App) -> Stateful<Div> {
        let colors = theme.colors;
        let moss_size = if wide { 288. } else { 128. };
        let panel = div()
            .id("onboarding-panel")
            .debug_selector(|| "onboarding-panel".into())
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_6();
        if wide {
            panel
                .h_full()
                .w(px(440.))
                .flex_none()
                .p_8()
                .bg(colors.rail)
                .children(brand::logo_on_dark(cx, 168.))
                .children(brand::moss(cx, self.step.moss(), moss_size))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_2()
                        .text_center()
                        .child(crate::fonts::heading(
                            div()
                                .text_size(px(24.))
                                .text_color(colors.rail_text)
                                .child(brand::HEADLINE),
                        ))
                        .child(
                            div()
                                .text_size(theme.type_scale.body)
                                .text_color(colors.rail_text.opacity(0.78))
                                .child(crate::states::BRAND_SUPPORT),
                        ),
                )
        } else {
            panel
                .w_full()
                .pt_8()
                .pb_2()
                .px_6()
                .gap_3()
                .children(brand::moss(cx, self.step.moss(), moss_size))
        }
    }
}

impl Render for OnboardingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::get(cx);
        let wide = f32::from(window.viewport_size().width) >= WIDE_FROM;
        let card = match self.step {
            Step::Connect => self.connect_card(&theme, window, cx),
            Step::Invited => self.invited_card(&theme, cx),
            Step::Account => self.account_card(&theme, window, cx),
            Step::Picture => self.picture_card(&theme, cx),
            Step::Workspace => self.workspace_card(&theme, window, cx),
            Step::Share => self.share_card(&theme, window, cx),
            Step::Done => self.done_card(&theme, cx),
        };
        // Each screen arrives in a fraction of a second (GPUI shows it at once when the person
        // asked for reduced motion).
        let arriving = div()
            .id("onboarding-card")
            .debug_selector(|| "onboarding-card".into())
            .w_full()
            .max_w(px(420.))
            .child(card)
            .with_animation(
                ElementId::NamedInteger("onboarding-step".into(), self.step.index()),
                Animation::new(ENTRANCE).with_easing(gpui::ease_out_quint()),
                |element, delta| {
                    element
                        .opacity(delta)
                        .relative()
                        .top(px(10.0 * (1.0 - delta)))
                },
            );
        let panel = self.panel(&theme, wide, cx);
        let root = div()
            .id("onboarding")
            .debug_selector(|| "onboarding".into())
            .size_full()
            .bg(theme.colors.surface)
            // Tab moves between the buttons, as it does on the sign-in form (a text field passes
            // its own Tab up through `move_focus`).
            .key_context(keyboard::SHELL_CONTEXT)
            .on_action(cx.listener(|this, _: &FocusNextControl, window, cx| {
                this.move_focus(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPrevControl, window, cx| {
                this.move_focus(true, window, cx)
            }))
            .flex();
        if wide {
            root.child(panel).child(
                div()
                    .id("onboarding-pane")
                    .flex_1()
                    .h_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .min_h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .p_8()
                            .child(arriving),
                    ),
            )
        } else {
            root.flex_col().child(
                div()
                    .id("onboarding-pane")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .min_h_full()
                            .flex()
                            .flex_col()
                            .items_center()
                            .child(panel)
                            .child(div().w_full().flex().justify_center().p_6().child(arriving)),
                    ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_username_is_suggested_from_the_first_word_of_the_name() {
        assert_eq!(suggest_username("Quentin Fakename"), "quentin");
        // Too short alone (a username has at least three characters): all the words.
        assert_eq!(suggest_username("  Zoë Ames"), "zoames");
        assert_eq!(suggest_username("J. R. R. Tolkien"), "jrrtolkien");
        assert_eq!(suggest_username("Li Wei"), "liwei");
        assert_eq!(suggest_username("Mary-Jane"), "maryjane");
        assert_eq!(suggest_username("42"), "", "two characters are too few");
        assert_eq!(suggest_username("A"), "");
        assert_eq!(suggest_username(""), "");
        assert_eq!(suggest_username("🎉"), "");
        assert_eq!(suggest_username("日本語"), "");
        // Whatever is suggested is a username the server would take the shape of.
        for name in ["Quentin Fakename", "Zoë", "A", "Mary-Jane", "O'Brien"] {
            let suggestion = suggest_username(name);
            assert!(
                suggestion.is_empty() || Username::parse(&suggestion).is_ok(),
                "{name:?} -> {suggestion:?}"
            );
        }
    }

    #[test]
    fn a_taken_username_is_offered_another_with_a_number() {
        assert_eq!(alternative_username("quentin"), "quentin2");
        assert_eq!(alternative_username("quentin2"), "quentin2");
        assert_eq!(alternative_username("quentin7"), "quentin2");
        assert_eq!(alternative_username("42"), "422");
    }

    #[test]
    fn a_refusal_of_the_account_is_shown_and_a_taken_name_gets_a_way_out() {
        assert_eq!(
            OnboardingView::problem_text("that username is taken", "quentin"),
            "That username is taken. Try quentin2."
        );
        assert_eq!(
            OnboardingView::problem_text("That username is taken", ""),
            "That username is taken",
            "nothing to offer without a name"
        );
        assert_eq!(
            OnboardingView::problem_text("Password must be at least 8 characters", "quentin"),
            "Password must be at least 8 characters"
        );
    }

    #[test]
    fn the_invitation_line_says_who_and_how_many_in_the_words_that_fit() {
        let preview = |inviter: Option<&str>, member_count| InvitationPreview {
            kind: InvitationKind::Invitation,
            workspace_name: "Zebrafish Collective".into(),
            inviter: inviter.map(|name| lazy_domain::DisplayName::parse(name).unwrap()),
            member_count,
        };
        assert_eq!(
            invited_line(&preview(Some("Alice Example"), 12)),
            "Alice Example invited you. 12 people are already here."
        );
        assert_eq!(
            invited_line(&preview(Some("Alice Example"), 1)),
            "Alice Example invited you. 1 person is already here."
        );
        assert_eq!(
            invited_line(&preview(Some("Alice Example"), 0)),
            "Alice Example invited you."
        );
        assert_eq!(
            invited_line(&preview(None, 3)),
            "3 people are already here."
        );
        assert_eq!(invited_line(&preview(None, 0)), "");
    }

    #[test]
    fn a_code_is_needed_unless_the_server_said_it_is_open_to_anyone() {
        use lazy_client_core::ServerFacts;
        let facts = |registration, setup_code_required| {
            Some(ServerFacts {
                registration,
                setup_code_required,
            })
        };
        assert!(needs_code(None), "an older server decides for itself");
        assert!(needs_code(facts(ServerRegistration::InviteOnly, false)));
        assert!(needs_code(facts(ServerRegistration::Open, true)));
        assert!(!needs_code(facts(ServerRegistration::Open, false)));
    }

    #[test]
    fn the_steps_are_numbered_and_the_last_screen_is_not_a_step() {
        let all = [
            Step::Connect,
            Step::Invited,
            Step::Account,
            Step::Picture,
            Step::Workspace,
            Step::Share,
            Step::Done,
        ];
        let numbers: Vec<Option<u32>> = all.into_iter().map(Step::number).collect();
        assert_eq!(
            numbers,
            [Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), None]
        );
        // The owner's two screens come after the fourth step and are counted in the total.
        assert_eq!((Step::total(false), Step::total(true)), (4, 6));
        let indexes: Vec<u64> = all.into_iter().map(Step::index).collect();
        assert_eq!(indexes, [0, 1, 2, 3, 4, 5, 6]);
    }
}
