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

use gpui::{AppContext as _, Entity, HeadlessAppContext, WindowAppearance, px, size};
use lazy_client_core::{Effect, Input, RequestError, SocketEvent, StartupState};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, DisplayName, EventSeq, MessageDocument,
    MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, BootstrapResponse, Conversation, ErrorCode, HistoryPage, Message, PROTOCOL_VERSION,
    ServerFrame, User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::root_view::RootView;

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
    workspace: WorkspaceId,
    general: ConversationId,
}

impl Cast {
    fn new() -> Self {
        Self {
            me: user("alice", "Alice Nguyen"),
            bob: user("bob", "Bob Okafor"),
            zoe: user("zoe", "Zoë Lindqvist"),
            workspace: WorkspaceId::generate(),
            general: ConversationId::generate(),
        }
    }

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
                conversations: vec![Conversation {
                    id: self.general,
                    workspace_id: self.workspace,
                    name: ChannelName::parse("general").unwrap(),
                    topic: Some("Company-wide announcements and chatter".into()),
                    visibility: ChannelVisibility::Public,
                    archived: false,
                }],
                members: vec![self.me.clone(), self.bob.clone(), self.zoe.clone()],
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
type BuildScene = dyn Fn(&Entity<AppModel>, &Cast, &mut HeadlessAppContext, &Recorder);

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
            build: &|model, _, cx, _| {
                dispatch(cx, model, Input::Started(StartupState::default()));
            },
        },
        Scene {
            name: "02-signin-error-dark",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, _, cx, _| {
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
            build: &|model, cast, cx, rec| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "04-chat-dark-wide",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Dark,
            build: &|model, cast, cx, rec| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "05-chat-light-medium",
            width: 820.0,
            height: 600.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "06-chat-light-narrow-minimum",
            width: 640.0,
            height: 480.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec| reach_chat(cx, model, cast, rec, true),
        },
        Scene {
            name: "07-empty-channel-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec| reach_chat(cx, model, cast, rec, false),
        },
        Scene {
            name: "08-delivery-states-and-reconnecting-light",
            width: 1100.0,
            height: 720.0,
            appearance: WindowAppearance::Light,
            build: &|model, cast, cx, rec| {
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
            build: &|model, cast, cx, rec| {
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
            build: &|model, cast, cx, rec| {
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
            build: &|model, cast, cx, rec| {
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
        (scene.build)(&model, &cast, &mut cx, &recorder);
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
}
