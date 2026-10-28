//! Renders Lazy's real views offscreen and writes PNG screenshots.
//!
//! This uses GPUI's headless renderer: the same wgpu renderer and text system the desktop app
//! uses, drawing into a texture. No window is created and nothing is shown on any display, so it
//! is safe to run anywhere with a Vulkan device. It exists to *look* at layouts and themes.
//!
//!     cargo run -p lazy-ui --example screens -- <output-dir>
//!
//! The content shown is simulated and every scene is generated here, not read from a server.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AppContext as _, Entity, HeadlessAppContext, Keystroke, WindowAppearance, WindowHandle, px,
    size,
};
use lazy_client_core::{
    ActionKind, ActionOutcome, Effect, Input, RequestError, SocketEvent, StartupState,
};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, BrowsedChannel, Conversation, ConversationUserState, ErrorCode,
    HistoryPage, Message, PROTOCOL_VERSION, ServerFrame, User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::root_view::RootView;
use lazy_ui::shell::ShellDialog;

/// Records the effects the core asks for so a scene can answer them the way a runtime would.
#[derive(Default)]
struct Recorder(RefCell<Vec<Effect>>);
impl EffectSink for Recorder {
    fn execute(&self, effects: Vec<Effect>) {
        self.0.borrow_mut().extend(effects);
    }
}

fn user(name: &str, display: &str) -> User {
    User {
        id: UserId::generate(),
        username: Username::parse(name).unwrap(),
        display_name: DisplayName::parse(display).unwrap(),
    }
}

struct Cast {
    me: User,
    bob: User,
    zoe: User,
    carol: User,
    workspace: WorkspaceId,
    general: ConversationId,
    design: ConversationId,
    dm_bob: ConversationId,
    group: ConversationId,
}

impl Cast {
    fn new() -> Self {
        Self {
            me: user("alice", "Alice Nguyen"),
            bob: user("bob", "Bob Okafor"),
            zoe: user("zoe", "Zoë Lindqvist"),
            carol: user("carol", "Carol Mbeki"),
            workspace: WorkspaceId::generate(),
            general: ConversationId::generate(),
            design: ConversationId::generate(),
            dm_bob: ConversationId::generate(),
            group: ConversationId::generate(),
        }
    }

    fn channel(&self, id: ConversationId, name: &str, topic: &str, private: bool) -> Conversation {
        Conversation {
            id,
            workspace_id: self.workspace,
            kind: ConversationKind::Channel,
            name: Some(ChannelName::parse(name).unwrap()),
            topic: Some(topic.to_owned()),
            description: Some("Where the team keeps its shared context.".into()),
            visibility: if private {
                ChannelVisibility::Private
            } else {
                ChannelVisibility::Public
            },
            archived: false,
            is_default: name == "general",
            created_by: Some(self.me.id),
            participants: vec![],
            member_count: 12,
        }
    }

    fn people(&self, id: ConversationId, others: &[&User]) -> Conversation {
        let mut participants = vec![self.me.id];
        participants.extend(others.iter().map(|user| user.id));
        Conversation {
            id,
            workspace_id: self.workspace,
            kind: if others.len() == 1 {
                ConversationKind::Direct
            } else {
                ConversationKind::Group
            },
            name: None,
            topic: None,
            description: None,
            visibility: ChannelVisibility::Private,
            archived: false,
            is_default: false,
            created_by: Some(self.me.id),
            participants,
            member_count: 1 + others.len() as u32,
        }
    }

    fn state(
        &self,
        id: ConversationId,
        favorite: bool,
        unread: u32,
        mentions: u32,
    ) -> ConversationUserState {
        ConversationUserState {
            conversation_id: id,
            favorite,
            last_read: EventSeq::ZERO,
            unread_count: unread,
            mention_count: mentions,
        }
    }

    /// A busy sidebar: a favorite, unread channels, and a direct and a group message.
    fn bootstrap(&self) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                },
                role: WorkspaceRole::Owner,
                conversations: vec![
                    self.channel(
                        self.general,
                        "general",
                        "Company-wide announcements and chatter",
                        false,
                    ),
                    self.channel(self.design, "design", "Pixels, type, and feelings", true),
                    self.channel(
                        ConversationId::generate(),
                        "engineering",
                        "Builds, reviews, and incidents",
                        false,
                    ),
                    self.people(self.dm_bob, &[&self.bob]),
                    self.people(self.group, &[&self.bob, &self.zoe]),
                ],
                user_state: vec![
                    self.state(self.general, false, 0, 0),
                    self.state(self.design, true, 0, 0),
                    self.state(self.dm_bob, false, 3, 0),
                    self.state(self.group, false, 120, 2),
                ],
                members: vec![
                    self.me.clone(),
                    self.bob.clone(),
                    self.zoe.clone(),
                    self.carol.clone(),
                ],
                head: EventSeq::ZERO,
            }],
        }
    }

    fn message(&self, author: &User, text: &str, seq: i64, minutes_ago: i64) -> Message {
        let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes_ago);
        Message {
            id: MessageId::generate(),
            conversation_id: self.general,
            author_id: author.id,
            document: MessageDocument::from_plain_text(text).unwrap(),
            created_at: Timestamp::from(at),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
        }
    }

    fn history(&self) -> Vec<Message> {
        let (me, bob, zoe) = (&self.me, &self.bob, &self.zoe);
        vec![
            self.message(zoe, "Morning all! The new build is out. 🎉", 1, 60 * 30),
            self.message(bob, "Nice. Did the migration finish cleanly?", 2, 60 * 29),
            self.message(zoe, "Yes — all green. I also left notes in the runbook.", 3, 60 * 29 - 1),
            self.message(me, "Thanks Zoë, reading them now.", 4, 60 * 5),
            self.message(me, "One question: does the retry policy apply to webhooks too?", 5, 60 * 5 - 1),
            self.message(
                bob,
                "Good catch. Short answer: not yet. Longer answer, because it matters for the rollout: webhook deliveries are queued separately and we planned to add bounded retries with visible failures in the integrations phase, so for now a failed delivery is simply dropped and logged.",
                6,
                42,
            ),
            self.message(bob, "I'll write it up properly.\n\nTwo things to decide:\n• retry count\n• how failures show up", 7, 41),
            self.message(zoe, "café ☕ — naïve résumé — 日本語のテキスト — 👩‍👩‍👧‍👦", 8, 20),
            self.message(me, "Sounds good. Let's pick this up after lunch.", 9, 3),
        ]
    }
}

/// Drives the core into the state a scene shows.
type BuildScene =
    dyn Fn(&Entity<AppModel>, &Cast, &mut HeadlessAppContext, &Recorder, &WindowHandle<RootView>);

struct Scene<'a> {
    name: &'a str,
    width: f32,
    height: f32,
    appearance: WindowAppearance,
    build: &'a BuildScene,
}

fn dispatch(cx: &mut HeadlessAppContext, model: &Entity<AppModel>, input: Input) {
    cx.update(|app| model.update(app, |model, cx| model.dispatch(input, cx)));
    cx.run_until_parked();
}

fn reach_chat(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    cast: &Cast,
    recorder: &Recorder,
    with_history: bool,
) {
    dispatch(
        cx,
        model,
        Input::Started(StartupState {
            session_restored: true,
            ..Default::default()
        }),
    );
    dispatch(
        cx,
        model,
        Input::BootstrapLoaded(Box::new(cast.bootstrap())),
    );
    dispatch(
        cx,
        model,
        Input::Socket(SocketEvent::Opened {
            workspace: cast.workspace,
        }),
    );
    dispatch(
        cx,
        model,
        Input::Frame(ServerFrame::CaughtUp {
            workspace_id: cast.workspace,
            seq: EventSeq::ZERO,
        }),
    );
    let messages = if with_history { cast.history() } else { vec![] };
    let generation = recorder
        .0
        .borrow()
        .iter()
        .find_map(|effect| match effect {
            Effect::FetchHistory { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("loading #general fetches its history");
    dispatch(
        cx,
        model,
        Input::HistoryLoaded {
            conversation: cast.general,
            generation,
            result: Ok(HistoryPage {
                messages,
                has_more: false,
            }),
        },
    );
}

/// Opens a dialog the way a menu command would.
fn open_dialog(cx: &mut HeadlessAppContext, window: &WindowHandle<RootView>, dialog: ShellDialog) {
    cx.update_window((*window).into(), |_, window_ref, app| {
        // The shell is created on the first draw after the session becomes ready.
        let _ = (window_ref, app);
    })
    .expect("window");
    draw(cx, window);
    window
        .update(cx, |root, window, app| {
            let shell = root
                .shell()
                .expect("the signed-in shell is showing")
                .clone();
            shell.update(app, |shell, cx| shell.open_dialog(dialog, window, cx));
        })
        .expect("open the dialog");
    draw(cx, window);
}

fn draw(cx: &mut HeadlessAppContext, window: &WindowHandle<RootView>) {
    for _ in 0..3 {
        cx.update_window((*window).into(), |_, w, app| w.draw(app).clear(app))
            .expect("draw");
        cx.run_until_parked();
    }
}

/// Presses keys, one keystroke per space-separated word, like a person at the keyboard.
fn press(cx: &mut HeadlessAppContext, window: &WindowHandle<RootView>, keys: &str) {
    let os = cx.update(|app| app.platform_info().os);
    for key in keys.split(' ') {
        let keystroke = Keystroke::parse_with_os(key, os).expect("a valid keystroke");
        cx.update_window((*window).into(), |_, w, app| {
            w.dispatch_keystroke(keystroke, app)
        })
        .expect("dispatch key");
        draw(cx, window);
    }
}

fn type_text(cx: &mut HeadlessAppContext, window: &WindowHandle<RootView>, text: &str) {
    for ch in text.chars() {
        press(
            cx,
            window,
            &if ch == ' ' {
                "space".to_owned()
            } else {
                ch.to_string()
            },
        );
    }
}

/// Answers the newest history request for `conversation` with `messages`.
fn answer_history(
    cx: &mut HeadlessAppContext,
    model: &Entity<AppModel>,
    recorder: &Recorder,
    conversation: ConversationId,
    messages: Vec<Message>,
) {
    let generation = recorder
        .0
        .borrow()
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::FetchHistory {
                conversation: c,
                generation,
                ..
            } if *c == conversation => Some(*generation),
            _ => None,
        })
        .expect("the core asked for that history");
    dispatch(
        cx,
        model,
        Input::HistoryLoaded {
            conversation,
            generation,
            result: Ok(HistoryPage {
                messages,
                has_more: false,
            }),
        },
    );
}

/// The window opens in one theme and the system switches to the other while the form is up, as
/// happens when the compositor reports the appearance after the window exists. Text typed after
/// the switch must be readable against the new surface.
fn theme_switch_scene(out: &std::path::Path) {
    use gpui::Keystroke;

    let mut cx = HeadlessAppContext::with_platform(
        Arc::new(gpui_wgpu::CosmicTextSystem::new("IBM Plex Sans")),
        Arc::new(()),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|app| lazy_ui::init(app, WindowAppearance::Light));
    let recorder = Rc::new(Recorder::default());
    let model = cx.update(|app| app.new(|_| AppModel::new(recorder.clone())));
    let window = cx
        .open_window(size(px(1100.0), px(720.0)), {
            let model = model.clone();
            move |window, app| app.new(|cx| RootView::new(model, window, cx))
        })
        .expect("open window");
    dispatch(&mut cx, &model, Input::Started(StartupState::default()));
    let settle = |cx: &mut HeadlessAppContext| {
        for _ in 0..3 {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .expect("draw");
            cx.run_until_parked();
        }
    };
    settle(&mut cx);

    // The system turns out to be dark.
    cx.update(|app| lazy_ui::theme::install(app, WindowAppearance::Dark));
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .expect("refresh");
    settle(&mut cx);

    let os = cx.update(|app| app.platform_info().os);
    let key = |cx: &mut HeadlessAppContext, text: &str| {
        let keystroke = Keystroke::parse_with_os(text, os).expect("keystroke");
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_keystroke(keystroke, cx)
        })
        .expect("key");
        settle(cx);
    };
    for ch in "alice".chars() {
        key(&mut cx, &ch.to_string());
    }
    key(&mut cx, "tab");
    for ch in "hunter22".chars() {
        key(&mut cx, &ch.to_string());
    }
    let image = cx.capture_screenshot(window.into()).expect("screenshot");
    let path = out.join("23-theme-switch-signin.png");
    image.save(&path).expect("write png");
    println!(
        "wrote {} ({}x{})",
        path.display(),
        image.width(),
        image.height()
    );
}

fn main() {
    let out: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "screens".into())
        .into();
    std::fs::create_dir_all(&out).expect("create output directory");

    let scenes: Vec<Scene<'_>> = vec![
        Scene {
            name: "01-signin-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, _, cx, _, _win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
            },
        },
        Scene {
            name: "02-signin-error-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, _, cx, _, _win| {
                dispatch(cx, model, Input::Started(StartupState::default()));
                dispatch(
                    cx,
                    model,
                    Input::SignIn {
                        username: "al".into(),
                        password: "x".into(),
                    },
                );
            },
        },
        Scene {
            name: "03-chat-light-wide",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "04-chat-dark-wide",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "05-chat-light-medium",
            width: 820.0,
            height: 600.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "06-chat-light-narrow-minimum",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "07-empty-channel-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| reach_chat(cx, model, cast, rec, false),
        },
        Scene {
            name: "08-delivery-states-and-reconnecting-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat(cx, model, cast, rec, true);
                // Three sends: one stays pending, one is in doubt, one is rejected.
                let mut ops = Vec::new();
                for text in [
                    "Sending this one right now",
                    "I lost my connection mid-send",
                    "This one the server refused",
                ] {
                    dispatch(
                        cx,
                        model,
                        Input::SendMessage {
                            conversation: cast.general,
                            text: text.into(),
                        },
                    );
                    ops.push(model.read_with(cx, |m, _| {
                        m.core()
                            .timeline(cast.general)
                            .last()
                            .and_then(|i| i.operation_id())
                            .unwrap()
                    }));
                }
                // Messages are sent one at a time, so release them in order.
                dispatch(cx, model, Input::SubmissionPersisted { operation: ops[0] });
                dispatch(
                    cx,
                    model,
                    Input::SendResult {
                        operation: ops[0],
                        result: Err(RequestError::Unknown("connection reset".into())),
                    },
                );
                dispatch(
                    cx,
                    model,
                    Input::Socket(SocketEvent::Closed {
                        workspace: cast.workspace,
                        attempt: 2,
                    }),
                );
            },
        },
        Scene {
            name: "09-composer-multiline-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, _win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(
                    cx,
                    model,
                    Input::SetDraft {
                        conversation: cast.general,
                        text: "Draft with several lines\nand an emoji 🙂\nand a third line that is quite long so that it wraps around the composer width when the window is narrow".into(),
                    },
                );
            },
        },
        Scene {
            name: "10-rejected-send-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(
                    cx,
                    model,
                    Input::SendMessage {
                        conversation: cast.general,
                        text: "A message the server will refuse".into(),
                    },
                );
                let op = model.read_with(cx, |m, _| {
                    m.core()
                        .timeline(cast.general)
                        .last()
                        .and_then(|i| i.operation_id())
                        .unwrap()
                });
                dispatch(cx, model, Input::SubmissionPersisted { operation: op });
                dispatch(
                    cx,
                    model,
                    Input::SendResult {
                        operation: op,
                        result: Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::Forbidden,
                            "you cannot post in this conversation",
                        ))),
                    },
                );
            },
        },
        Scene {
            name: "11-emoji-and-scripts-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, _win| {
                reach_chat(cx, model, cast, rec, false);
                for (n, text) in [
                    "Plain emoji: 🎉 😀 👍 ❤️ ☕ 🚀 🙂",
                    "Skin tones: 👍🏽 👋🏿",
                    "Flags and sequences: 🇨🇦 🏳️‍🌈 👩‍👩‍👧‍👦 👨🏻‍💻",
                    "Accents and CJK: café naïve Zoë — 日本語 — 한국어 — 中文",
                    "Right to left: שלום עולם — مرحبا بالعالم",
                ]
                .into_iter()
                .enumerate()
                {
                    let event = lazy_protocol::EventEnvelope {
                        workspace_id: cast.workspace,
                        seq: EventSeq::new(100 + n as i64).unwrap(),
                        occurred_at: Timestamp::now(),
                        event: lazy_protocol::Event::MessageCreated {
                            message: cast.message(&cast.zoe, text, 100 + n as i64, 5 - n as i64),
                            operation_id: None,
                        },
                    };
                    dispatch(cx, model, Input::Frame(ServerFrame::Event(event)));
                }
            },
        },
        Scene {
            name: "12-create-channel-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::CreateChannel);
                type_text(cx, win, "Design Team!");
            },
        },
        Scene {
            name: "13-create-channel-name-taken-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::CreateChannel);
                type_text(cx, win, "general");
                press(cx, win, "enter");
                let operation = rec
                    .0
                    .borrow()
                    .iter()
                    .find_map(|effect| match effect {
                        Effect::CreateChannel { request, .. } => Some(request.operation_id),
                        _ => None,
                    })
                    .expect("the dialog sent a create request");
                dispatch(
                    cx,
                    model,
                    Input::ConversationCommandResult {
                        operation,
                        result: Err(RequestError::Rejected(ApiError::new(
                            ErrorCode::NameTaken,
                            "A channel with that name already exists.",
                        ))),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "14-browse-channels-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::BrowseChannels);
                let mut archived =
                    cast.channel(ConversationId::generate(), "old-launch", "Retired", false);
                archived.archived = true;
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::Browse,
                        result: Ok(ActionOutcome::Browse(vec![
                            BrowsedChannel { conversation: cast.channel(cast.general, "general", "Company-wide announcements and chatter", false), joined: true },
                            BrowsedChannel { conversation: cast.channel(ConversationId::generate(), "random", "Anything goes, within reason", false), joined: false },
                            BrowsedChannel { conversation: cast.channel(ConversationId::generate(), "support", "Customer questions and escalations, and the long-running thread about the ticket backlog", false), joined: false },
                            BrowsedChannel { conversation: archived, joined: false },
                        ])),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "15-new-message-picker-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::NewMessage);
                type_text(cx, win, "bo");
                press(cx, win, "enter");
                type_text(cx, win, "z");
            },
        },
        Scene {
            name: "16-add-people-to-direct-message-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SelectConversation(cast.dm_bob));
                answer_history(cx, model, rec, cast.dm_bob, vec![]);
                open_dialog(cx, win, ShellDialog::Details(cast.dm_bob));
                // The close button has focus; one Shift+Tab reaches "Add people…", Enter opens it.
                press(cx, win, "shift-tab enter");
            },
        },
        Scene {
            name: "17-channel-details-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SelectConversation(cast.design));
                answer_history(cx, model, rec, cast.design, vec![]);
                open_dialog(cx, win, ShellDialog::Details(cast.design));
            },
        },
        Scene {
            name: "18-leave-confirmation-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SelectConversation(cast.design));
                answer_history(cx, model, rec, cast.design, vec![]);
                open_dialog(cx, win, ShellDialog::Details(cast.design));
                // Topic has focus; description, Add people, then Leave channel.
                press(cx, win, "tab tab tab enter");
            },
        },
        Scene {
            name: "19-new-messages-divider-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                let mut bootstrap = cast.bootstrap();
                bootstrap.workspaces[0].user_state[0] = ConversationUserState {
                    conversation_id: cast.general,
                    favorite: false,
                    last_read: EventSeq::new(5).unwrap(),
                    unread_count: 4,
                    mention_count: 0,
                };
                dispatch(
                    cx,
                    model,
                    Input::Started(StartupState {
                        session_restored: true,
                        ..Default::default()
                    }),
                );
                dispatch(cx, model, Input::BootstrapLoaded(Box::new(bootstrap)));
                dispatch(
                    cx,
                    model,
                    Input::Socket(SocketEvent::Opened {
                        workspace: cast.workspace,
                    }),
                );
                dispatch(
                    cx,
                    model,
                    Input::Frame(ServerFrame::CaughtUp {
                        workspace_id: cast.workspace,
                        seq: EventSeq::ZERO,
                    }),
                );
                answer_history(cx, model, rec, cast.general, cast.history());
                draw(cx, win);
            },
        },
        Scene {
            name: "20-archived-channel-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                dispatch(cx, model, Input::SelectConversation(cast.design));
                answer_history(cx, model, rec, cast.design, vec![]);
                let mut archived =
                    cast.channel(cast.design, "design", "Pixels, type, and feelings", true);
                archived.archived = true;
                // The core only accepts an answer to an action it started.
                dispatch(cx, model, Input::ArchiveConversation(cast.design));
                dispatch(
                    cx,
                    model,
                    Input::ActionResult {
                        kind: ActionKind::Archive(cast.design),
                        result: Ok(ActionOutcome::Conversation(archived)),
                    },
                );
                draw(cx, win);
            },
        },
        Scene {
            name: "21-empty-workspace-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, _rec, win| {
                let mut bootstrap = cast.bootstrap();
                bootstrap.workspaces[0].conversations.clear();
                bootstrap.workspaces[0].user_state.clear();
                dispatch(
                    cx,
                    model,
                    Input::Started(StartupState {
                        session_restored: true,
                        ..Default::default()
                    }),
                );
                dispatch(cx, model, Input::BootstrapLoaded(Box::new(bootstrap)));
                draw(cx, win);
            },
        },
        Scene {
            name: "22-dialog-at-minimum-size-light",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec, win| {
                reach_chat(cx, model, cast, rec, true);
                open_dialog(cx, win, ShellDialog::CreateChannel);
                type_text(cx, win, "Design Team");
            },
        },
    ];

    for scene in scenes {
        let mut cx = HeadlessAppContext::with_platform(
            Arc::new(gpui_wgpu::CosmicTextSystem::new("IBM Plex Sans")),
            Arc::new(()),
            gpui_platform::current_headless_renderer,
        );
        let cast = Cast::new();
        cx.update(|app| lazy_ui::init(app, scene.appearance));
        let recorder = Rc::new(Recorder::default());
        let model = cx.update(|app| app.new(|_| AppModel::new(recorder.clone())));
        let window = cx
            .open_window(size(px(scene.width), px(scene.height)), {
                let model = model.clone();
                move |window, app| app.new(|cx| RootView::new(model, window, cx))
            })
            .expect("open window");
        cx.run_until_parked();
        (scene.build)(&model, &cast, &mut cx, &recorder, &window);
        // Draw a couple of frames so list measurement settles.
        for _ in 0..3 {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .expect("draw");
            cx.run_until_parked();
        }
        match cx.capture_screenshot(window.into()) {
            Ok(image) => {
                let path = out.join(format!("{}.png", scene.name));
                image.save(&path).expect("write png");
                println!(
                    "wrote {} ({}x{})",
                    path.display(),
                    image.width(),
                    image.height()
                );
            }
            Err(error) => {
                eprintln!("{}: screenshot failed: {error:#}", scene.name);
                std::process::exit(1);
            }
        }
    }
    theme_switch_scene(&out);
}
