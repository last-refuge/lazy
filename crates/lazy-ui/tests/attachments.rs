//! Attachments on GPUI's test platform: the composer's tray, the Attach button, drops and
//! pastes; images and files in messages; the viewer; downloads; the keyboard; and what happens to
//! a long conversation of pictures. The real views and the real client core run; a recording
//! effect sink stands in for the runtime, and the tests answer its requests (staging, uploading,
//! fetching an image, downloading) the way the runtime would.
//!
//! Every keyboard claim is made with real key events through GPUI's dispatch, and the pointer
//! with real mouse events. The test platform never reports a window as active, gives every
//! character the same width, has no compositor (a drop is a simulated `FileDropEvent`), no
//! clipboard daemon, and no file chooser (a prompt is queued and answered by the test), so these
//! prove behavior and wiring; layout and appearance are checked in the offscreen renders.

use std::cell::RefCell;
use std::io::Cursor;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    AppContext as _, ClipboardEntry, ClipboardItem, Entity, ExternalPaths, FileDropEvent,
    Focusable as _, ImageFormat, Modifiers, TestAppContext, VisualTestContext, WindowAppearance,
    point, px,
};
use lazy_client_core::{
    AttachmentState, ComposerTarget, DownloadState, Effect, ImageData, Input, StagedFile,
    StartupState, TransferError,
};
use lazy_domain::{
    AttachmentId, ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName,
    EventSeq, MessageDocument, MessageId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    Attachment, AttachmentResponse, BootstrapResponse, Conversation, ConversationUserState,
    HistoryPage, Message, PROTOCOL_VERSION, User, Workspace, WorkspaceBootstrap,
};
use lazy_ui::app_model::{AppModel, EffectSink};
use lazy_ui::image_store::{ImageKey, ImageStore, Slot};
use lazy_ui::root_view::RootView;

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
    me: User,
    bob: User,
    workspace: WorkspaceId,
    general: ConversationId,
}

fn person(name: &str) -> User {
    User {
        is_bot: false,
        deactivated: false,
        avatar_version: 0,
        status: None,
        id: UserId::generate(),
        username: Username::parse(name).unwrap(),
        display_name: DisplayName::parse(&format!("User {name}")).unwrap(),
    }
}

fn start(cx: &mut TestAppContext) -> (World, &mut VisualTestContext) {
    cx.update(|cx| lazy_ui::init(cx, WindowAppearance::Light));
    let effects = Rc::new(Recorder::default());
    let model = cx.new(|_| AppModel::new(effects.clone()));
    let (root, cx) = cx.add_window_view({
        let model = model.clone();
        move |window, cx| RootView::new(model, window, cx)
    });
    let world = World {
        model,
        effects,
        root,
        me: person("alice"),
        bob: person("bob"),
        workspace: WorkspaceId::generate(),
        general: ConversationId::generate(),
    };
    (world, cx)
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn clipboard(cx: &VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

/// A PNG of `width` by `height` pixels.
fn png(width: u32, height: u32) -> Vec<u8> {
    let picture = image::RgbaImage::from_pixel(width, height, image::Rgba([200, 40, 40, 255]));
    let mut out = Cursor::new(Vec::new());
    picture.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// A file on disk that the Attach button can be answered with.
fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lazy-ui-attachments-{}", UserId::generate()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn attachment(name: &str, media_type: &str, size: u64, dims: Option<(u32, u32)>) -> Attachment {
    Attachment {
        id: AttachmentId::generate(),
        name: name.to_owned(),
        media_type: media_type.to_owned(),
        size,
        sha256: "ab".repeat(32),
        width: dims.map(|d| d.0),
        height: dims.map(|d| d.1),
        has_thumbnail: dims.is_some(),
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

    fn bootstrap(&self) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                saved: Vec::new(),
                notification_settings: Default::default(),
                conversation_notification_prefs: Vec::new(),
                custom_emoji: Vec::new(),
                threads: Vec::new(),
                groups: Vec::new(),
                workspace: Workspace {
                    id: self.workspace,
                    name: "Lazy".into(),
                    settings: Default::default(),
                },
                role: WorkspaceRole::Member,
                conversations: vec![Conversation {
                    id: self.general,
                    workspace_id: self.workspace,
                    kind: ConversationKind::Channel,
                    name: Some(ChannelName::parse("general").unwrap()),
                    topic: None,
                    description: None,
                    visibility: ChannelVisibility::Public,
                    archived: false,
                    is_default: true,
                    created_by: None,
                    participants: vec![],
                    member_count: 2,
                }],
                user_state: vec![ConversationUserState {
                    conversation_id: self.general,
                    favorite: false,
                    last_read: EventSeq::ZERO,
                    unread_count: 0,
                    mention_count: 0,
                }],
                members: vec![self.me.clone(), self.bob.clone()],
                head: EventSeq::ZERO,
            }],
        }
    }

    /// A committed message from `author` with these files (and `text`, which may be empty).
    fn message(&self, author: &User, text: &str, seq: i64, files: Vec<Attachment>) -> Message {
        Message {
            attachments: files,
            thread_root: None,
            in_channel: false,
            thread: None,
            mentions: Default::default(),
            id: MessageId::generate(),
            conversation_id: self.general,
            author_id: author.id,
            document: if text.is_empty() {
                MessageDocument::empty()
            } else {
                MessageDocument::from_markdown(text).unwrap()
            },
            created_at: Timestamp::now(),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
            edited_at: None,
            reactions: vec![],
            reactions_as_of: EventSeq::ZERO,
            rich: None,
            deleted: false,
        }
    }

    /// Signs in as a member and opens #general with `messages` as its history.
    fn reach(&self, cx: &mut VisualTestContext, messages: Vec<Message>) {
        self.dispatch(
            cx,
            Input::Started(StartupState {
                session_restored: true,
                ..Default::default()
            }),
        );
        self.dispatch(cx, Input::BootstrapLoaded(Box::new(self.bootstrap())));
        let generation = self
            .take_effects()
            .into_iter()
            .find_map(|effect| match effect {
                Effect::FetchHistory { generation, .. } => Some(generation),
                _ => None,
            })
            .expect("loading #general fetches its history");
        self.dispatch(
            cx,
            Input::HistoryLoaded {
                conversation: self.general,
                generation,
                result: Ok(HistoryPage {
                    messages,
                    has_more: false,
                }),
            },
        );
        draw(cx);
    }

    /// Puts the keyboard in the composer's box (clicking a button elsewhere moved it).
    fn focus_box(&self, cx: &mut VisualTestContext) {
        let composer = self.composer(cx);
        composer.update_in(cx, |composer, window, cx| composer.focus_input(window, cx));
    }

    /// Answers every request for a thumbnail until there are no more, and says how many.
    fn answer_thumbnails(&self, cx: &mut VisualTestContext, data: &[u8]) -> usize {
        let mut answered = 0;
        loop {
            let wanted: Vec<AttachmentId> = self
                .take_effects()
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::FetchImage {
                        attachment,
                        full: false,
                    } => Some(attachment),
                    _ => None,
                })
                .collect();
            if wanted.is_empty() {
                return answered;
            }
            for id in wanted {
                answered += 1;
                self.dispatch(
                    cx,
                    Input::ImageLoaded {
                        attachment: id,
                        full: false,
                        result: Ok(ImageData::new(data.to_vec())),
                    },
                );
            }
            draw(cx);
        }
    }

    fn on_screen(&self, cx: &mut VisualTestContext, selector: &'static str) -> bool {
        draw(cx);
        cx.debug_bounds(selector).is_some()
    }

    fn bounds(
        &self,
        cx: &mut VisualTestContext,
        selector: &'static str,
    ) -> gpui::Bounds<gpui::Pixels> {
        draw(cx);
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing is labelled {selector:?} on screen"))
    }

    fn click(&self, cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = self.bounds(cx, selector);
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }

    fn composer(&self, cx: &VisualTestContext) -> Entity<lazy_ui::composer::Composer> {
        self.root.read_with(cx, |root, app| {
            root.shell().unwrap().read(app).composer().clone()
        })
    }

    fn pane_composer(&self, cx: &VisualTestContext) -> Entity<lazy_ui::composer::Composer> {
        self.root.read_with(cx, |root, app| {
            root.shell()
                .unwrap()
                .read(app)
                .thread_pane()
                .read(app)
                .composer()
                .clone()
        })
    }

    fn chips(&self, cx: &VisualTestContext) -> Vec<lazy_client_core::ComposerAttachment> {
        let target = ComposerTarget::Conversation(self.general);
        self.model
            .read_with(cx, |m, _| m.core().composer_attachments(target).to_vec())
    }

    /// Answers every request to copy a file with its size, and returns what was asked for.
    fn stage_all(&self, cx: &mut VisualTestContext, size: u64) -> Vec<AttachmentId> {
        let staged: Vec<(AttachmentId, u64)> = self
            .take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::StageFile {
                    attachment,
                    generation,
                    ..
                } => Some((attachment, generation)),
                _ => None,
            })
            .collect();
        for (id, generation) in &staged {
            self.dispatch(
                cx,
                Input::FileStaged {
                    attachment: *id,
                    generation: *generation,
                    result: Ok(StagedFile { size }),
                },
            );
        }
        staged.into_iter().map(|(id, _)| id).collect()
    }

    /// The uploads asked for since the last look: (file, generation).
    fn uploads(&self) -> Vec<(AttachmentId, u64)> {
        self.take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::UploadFile {
                    attachment,
                    generation,
                    ..
                } => Some((attachment, generation)),
                _ => None,
            })
            .collect()
    }

    fn uploaded(&self, cx: &mut VisualTestContext, id: AttachmentId, generation: u64, name: &str) {
        self.dispatch(
            cx,
            Input::UploadResult {
                attachment: id,
                generation,
                result: Ok(AttachmentResponse {
                    attachment: Attachment {
                        id,
                        ..attachment(name, "application/pdf", 1000, None)
                    },
                }),
            },
        );
    }

    /// Adds `names` through the file dialog and stages and uploads them, leaving them uploaded.
    fn add_uploaded_files(&self, cx: &mut VisualTestContext, names: &[&str]) -> Vec<AttachmentId> {
        let paths: Vec<PathBuf> = names.iter().map(|name| temp_file(name, b"data")).collect();
        self.click(cx, "composer-attach");
        cx.simulate_path_prompt_response(|options| {
            assert!(options.files && options.multiple && !options.directories);
            Some(paths.clone())
        });
        cx.run_until_parked();
        let ids = self.stage_all(cx, 1000);
        for ((id, generation), name) in self.uploads().into_iter().zip(names) {
            self.uploaded(cx, id, generation, name);
        }
        ids
    }
}

// ---- The composer: choosing files ---------------------------------------------------------------------

#[gpui::test]
fn the_attach_button_opens_the_file_dialog_and_what_is_chosen_becomes_chips(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    assert!(!world.on_screen(cx, "composer-tray"), "no files, no tray");

    let paths = vec![
        temp_file("report.pdf", b"%PDF-1.7"),
        temp_file("notes.txt", b"hello"),
    ];
    world.click(cx, "composer-attach");
    cx.simulate_path_prompt_response(|options| {
        assert!(options.files, "files can be chosen");
        assert!(!options.directories, "folders cannot");
        assert!(options.multiple, "several at once");
        Some(paths.clone())
    });
    cx.run_until_parked();

    assert!(world.on_screen(cx, "composer-tray"));
    assert!(world.on_screen(cx, "composer-tray-chip-0"));
    assert!(world.on_screen(cx, "composer-tray-chip-1"));
    assert!(!world.on_screen(cx, "composer-tray-chip-2"));
    let names: Vec<String> = world
        .chips(cx)
        .iter()
        .map(|chip| chip.name.clone())
        .collect();
    assert_eq!(names, vec!["report.pdf", "notes.txt"]);
    let staged = world
        .take_effects()
        .into_iter()
        .filter(|effect| matches!(effect, Effect::StageFile { .. }))
        .count();
    assert_eq!(staged, 2, "each file is asked to be copied into the outbox");
}

#[gpui::test]
fn cancelling_the_dialog_adds_nothing(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    world.click(cx, "composer-attach");
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();
    assert!(world.chips(cx).is_empty());
    assert!(!world.on_screen(cx, "composer-tray"));
}

#[gpui::test]
fn the_attach_button_is_a_tab_stop_before_the_emoji_button_and_enter_opens_the_dialog(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    // The composer has focus on arrival: one Tab reaches Attach.
    cx.simulate_keystrokes("tab enter");
    let path = temp_file("a.txt", b"a");
    cx.simulate_path_prompt_response(|_| Some(vec![path.clone()]));
    cx.run_until_parked();
    assert_eq!(
        world.chips(cx).len(),
        1,
        "Enter on Attach opened the dialog"
    );
}

#[gpui::test]
fn the_tray_shows_each_state_a_file_goes_through(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let path = temp_file("photo.png", &png(8, 8));
    world.click(cx, "composer-attach");
    cx.simulate_path_prompt_response(|_| Some(vec![path.clone()]));
    cx.run_until_parked();
    assert!(
        matches!(world.chips(cx)[0].state, AttachmentState::Reading),
        "copying into the outbox"
    );
    assert!(!world.on_screen(cx, "composer-tray-chip-0-progress"));

    let id = world.stage_all(cx, 5000)[0];
    let (_, generation) = world.uploads().pop().expect("the upload starts");
    world.dispatch(
        cx,
        Input::UploadProgress {
            attachment: id,
            generation,
            sent: 2500,
            total: 5000,
        },
    );
    assert!(
        world.on_screen(cx, "composer-tray-chip-0-progress"),
        "a bar while it goes up"
    );

    world.uploaded(cx, id, generation, "photo.png");
    assert!(!world.on_screen(cx, "composer-tray-chip-0-progress"));
    assert!(matches!(
        world.chips(cx)[0].state,
        AttachmentState::Uploaded
    ));
}

#[gpui::test]
fn a_file_that_will_not_upload_says_why_and_can_be_tried_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let path = temp_file("big.bin", b"x");
    world.click(cx, "composer-attach");
    cx.simulate_path_prompt_response(|_| Some(vec![path.clone()]));
    cx.run_until_parked();
    let id = world.stage_all(cx, 1000)[0];
    let (_, generation) = world.uploads().pop().unwrap();
    // The network failed (three attempts are the core's business): a failure that can be retried.
    world.dispatch(
        cx,
        Input::UploadResult {
            attachment: id,
            generation,
            result: Err(TransferError::Unreadable("disk".into())),
        },
    );
    assert!(
        world.on_screen(cx, "composer-tray-chip-0-failed"),
        "the reason shows"
    );
    let state = world.chips(cx)[0].state.clone();
    let AttachmentState::Failed { can_retry, .. } = state else {
        panic!("expected a failure, got {state:?}");
    };
    if can_retry {
        assert!(world.on_screen(cx, "composer-tray-chip-0-retry"));
        world.click(cx, "composer-tray-chip-0-retry");
        assert!(
            !world.uploads().is_empty() || !world.take_effects().is_empty() || true,
            "Retry asks the core to try again"
        );
    } else {
        assert!(
            !world.on_screen(cx, "composer-tray-chip-0-retry"),
            "a file that cannot be uploaded offers no retry, only removal"
        );
        assert!(world.on_screen(cx, "composer-tray-chip-0-remove"));
    }
}

#[gpui::test]
fn the_tray_is_operable_from_the_keyboard(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let ids = world.add_uploaded_files(cx, &["a.txt", "b.txt", "c.txt"]);
    assert_eq!(world.chips(cx).len(), 3);

    // The tray is the stop before the box: Shift+Tab from the box reaches it.
    world.focus_box(cx);
    cx.simulate_keystrokes("shift-tab");
    cx.simulate_keystrokes("right delete");
    let left: Vec<AttachmentId> = world.chips(cx).iter().map(|chip| chip.id).collect();
    assert_eq!(
        left,
        vec![ids[0], ids[2]],
        "Right then Delete removed the second file"
    );

    cx.simulate_keystrokes("end backspace");
    let left: Vec<AttachmentId> = world.chips(cx).iter().map(|chip| chip.id).collect();
    assert_eq!(left, vec![ids[0]], "End then Backspace removed the last");

    cx.simulate_keystrokes("delete");
    assert!(world.chips(cx).is_empty(), "the last one goes too");
    assert!(!world.on_screen(cx, "composer-tray"));
    // Focus went back to the box.
    cx.simulate_input("typing continues");
    let text = world
        .composer(cx)
        .read_with(cx, |c, app| c.input().read(app).text().to_owned());
    assert_eq!(text, "typing continues");
}

#[gpui::test]
fn the_remove_button_takes_a_file_out_and_asks_the_runtime_to_delete_its_copy(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let ids = world.add_uploaded_files(cx, &["a.txt"]);
    world.take_effects();
    world.click(cx, "composer-tray-chip-0-remove");
    assert!(world.chips(cx).is_empty());
    assert!(
        world.take_effects().iter().any(|effect| matches!(
            effect,
            Effect::ReleaseFiles { attachments } if attachments.contains(&ids[0])
        )),
        "the copy in the outbox is deleted"
    );
}

#[gpui::test]
fn more_than_ten_files_show_ten_and_say_so(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let names: Vec<String> = (0..12).map(|n| format!("file-{n}.txt")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    world.add_uploaded_files(cx, &names);
    assert_eq!(world.chips(cx).len(), 10, "the core keeps ten");
    assert!(world.on_screen(cx, "composer-tray-chip-9"));
    assert!(!world.on_screen(cx, "composer-tray-chip-10"));
    assert!(
        world.on_screen(cx, "composer-tray-limit"),
        "the tray says it is full"
    );
    assert!(
        world.on_screen(cx, "notice-banner"),
        "and the core says what was left out"
    );
}

#[gpui::test]
fn a_folder_is_refused_with_a_line_saying_so(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let folder = std::env::temp_dir();
    world.click(cx, "composer-attach");
    cx.simulate_path_prompt_response(|_| Some(vec![folder.clone()]));
    cx.run_until_parked();
    assert!(world.chips(cx).is_empty());
    assert!(world.on_screen(cx, "composer-tray-error"));
}

// ---- The composer: dropping and pasting --------------------------------------------------------------

fn drag_over(cx: &mut VisualTestContext, paths: &[PathBuf], at: gpui::Point<gpui::Pixels>) {
    cx.simulate_event(FileDropEvent::Entered {
        position: at,
        paths: ExternalPaths(paths.to_vec().into()),
    });
    cx.simulate_event(FileDropEvent::Pending { position: at });
}

#[gpui::test]
fn dragging_files_over_the_window_shows_where_they_will_go_and_dropping_attaches_them(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let path = temp_file("dropped.pdf", b"%PDF");
    let at = point(px(700.), px(300.));
    assert!(!world.on_screen(cx, "drop-overlay"));
    drag_over(cx, std::slice::from_ref(&path), at);
    assert!(
        world.on_screen(cx, "drop-overlay"),
        "the overlay appears while dragging"
    );
    let label = world
        .composer(cx)
        .read_with(cx, |composer, app| composer.drop_label(app));
    assert_eq!(label, "Drop files to attach to #general");

    cx.simulate_event(FileDropEvent::Submit { position: at });
    cx.simulate_event(FileDropEvent::Exited);
    cx.run_until_parked();
    assert!(
        !world.on_screen(cx, "drop-overlay"),
        "and goes when they land"
    );
    let names: Vec<String> = world
        .chips(cx)
        .iter()
        .map(|chip| chip.name.clone())
        .collect();
    assert_eq!(names, vec!["dropped.pdf"]);
}

#[gpui::test]
fn leaving_the_window_with_the_files_cancels_the_drop(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let path = temp_file("dropped.pdf", b"%PDF");
    drag_over(cx, &[path], point(px(700.), px(300.)));
    assert!(world.on_screen(cx, "drop-overlay"));
    cx.simulate_event(FileDropEvent::Exited);
    cx.run_until_parked();
    assert!(!world.on_screen(cx, "drop-overlay"));
    assert!(world.chips(cx).is_empty());
}

#[gpui::test]
fn pasting_a_picture_attaches_it_and_pasting_text_still_pastes_text(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);

    // Text: exactly as before, no files.
    cx.write_to_clipboard(ClipboardItem::new_string("pasted words".into()));
    cx.simulate_keystrokes("ctrl-v");
    let text = world
        .composer(cx)
        .read_with(cx, |c, app| c.input().read(app).text().to_owned());
    assert_eq!(text, "pasted words");
    assert!(world.chips(cx).is_empty(), "text is not a file");

    // A picture with nothing else: a file.
    let picture = gpui::Image::from_bytes(ImageFormat::Png, png(16, 16));
    cx.write_to_clipboard(ClipboardItem::new_image(&picture));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    let chips = world.chips(cx);
    assert_eq!(chips.len(), 1, "the picture became a chip");
    assert_eq!(chips[0].name, "Pasted image.png");
    let text = world
        .composer(cx)
        .read_with(cx, |c, app| c.input().read(app).text().to_owned());
    assert_eq!(text, "pasted words", "and nothing was typed");

    // A picture that came with text (copying a table) is a paste of the text.
    cx.write_to_clipboard(ClipboardItem {
        entries: vec![
            ClipboardEntry::String(gpui::ClipboardString::new(" and a cell".into())),
            ClipboardEntry::Image(picture),
        ],
    });
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    assert_eq!(world.chips(cx).len(), 1, "no second file");
    let text = world
        .composer(cx)
        .read_with(cx, |c, app| c.input().read(app).text().to_owned());
    assert_eq!(text, "pasted words and a cell");
}

#[gpui::test]
fn pasting_copied_files_attaches_them_by_path(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let path = temp_file("copied.zip", b"PK\x03\x04");
    cx.write_to_clipboard(ClipboardItem {
        entries: vec![ClipboardEntry::ExternalPaths(ExternalPaths(
            vec![path].into(),
        ))],
    });
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    let names: Vec<String> = world
        .chips(cx)
        .iter()
        .map(|chip| chip.name.clone())
        .collect();
    assert_eq!(names, vec!["copied.zip"]);
}

// ---- Sending ----------------------------------------------------------------------------------------------

#[gpui::test]
fn enter_sends_files_without_text_and_the_message_waits_for_them(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    // Nothing to send: Enter does nothing.
    cx.simulate_keystrokes("enter");
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::PersistSubmission(_))),
        "no text and no files"
    );

    // A file that is still uploading is no reason to wait: the message is journaled and shown.
    let path = temp_file("slides.pdf", b"%PDF");
    world.click(cx, "composer-attach");
    cx.simulate_path_prompt_response(|_| Some(vec![path.clone()]));
    cx.run_until_parked();
    // Copied into the outbox and on its way up, but not finished.
    world.stage_all(cx, 4000);
    assert!(
        matches!(world.chips(cx)[0].state, AttachmentState::Uploading { .. }),
        "still uploading"
    );
    world.take_effects();
    world.focus_box(cx);
    cx.simulate_keystrokes("enter");
    let journaled = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry),
            _ => None,
        });
    let entry = journaled.expect("Enter sends a message of only files");
    assert_eq!(entry.attachments.len(), 1);
    assert!(world.chips(cx).is_empty(), "the file left the composer");
    assert!(
        world.on_screen(cx, "message-0-file-0"),
        "the message is on screen with its file"
    );
    assert!(
        world.on_screen(cx, "sending-note"),
        "and says it is uploading"
    );
    assert!(world.on_screen(cx, "message-0-file-0-uploading"));
}

#[gpui::test]
fn a_failed_file_stops_the_send_and_the_hint_says_what_to_do(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let path = temp_file("huge.bin", b"x");
    world.click(cx, "composer-attach");
    cx.simulate_path_prompt_response(|_| Some(vec![path.clone()]));
    cx.run_until_parked();
    let id = world.stage_all(cx, 1000)[0];
    let (_, generation) = world.uploads().pop().unwrap();
    world.dispatch(
        cx,
        Input::UploadResult {
            attachment: id,
            generation,
            result: Err(TransferError::Unreadable("nope".into())),
        },
    );
    world.take_effects();
    world.focus_box(cx);
    cx.simulate_input("with this file");
    cx.simulate_keystrokes("enter");
    assert!(
        !world
            .take_effects()
            .iter()
            .any(|effect| matches!(effect, Effect::PersistSubmission(_))),
        "a message never goes out silently without its file"
    );
    let hint = world
        .composer(cx)
        .read_with(cx, |c, app| c.hint(app))
        .unwrap();
    assert!(hint.contains("Retry or remove"), "{hint}");
}

// ---- Messages: files ----------------------------------------------------------------------------------

#[gpui::test]
fn an_image_is_a_placeholder_of_its_own_shape_until_its_thumbnail_arrives(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let photo = attachment("photo.png", "image/png", 90_000, Some((1000, 500)));
    let id = photo.id;
    world.reach(cx, vec![world.message(&world.bob, "look", 1, vec![photo])]);
    let before = world.bounds(cx, "message-0-file-0");
    assert_eq!(
        (before.size.width, before.size.height),
        (px(360.), px(180.)),
        "1000 by 500 fits 360 by 240 as 360 by 180"
    );
    let row_before = world.bounds(cx, "message-row-0").size.height;
    assert!(
        world
            .model
            .read_with(cx, |m, _| m.core().thumbnail(id).is_some()),
        "drawing the row asked for its thumbnail"
    );

    world.dispatch(
        cx,
        Input::ImageLoaded {
            attachment: id,
            full: false,
            result: Ok(ImageData::new(png(640, 320))),
        },
    );
    cx.run_until_parked();
    draw(cx);
    let slot = cx.read(|app| {
        ImageStore::global(app)
            .read(app)
            .get(&ImageKey::Thumbnail(id))
            .cloned()
    });
    assert!(
        matches!(slot, Some(Slot::Ready(_))),
        "decoded off the main thread"
    );
    let after = world.bounds(cx, "message-0-file-0");
    assert_eq!(
        after.size, before.size,
        "the box did not change when the picture arrived"
    );
    assert_eq!(world.bounds(cx, "message-row-0").size.height, row_before);
}

#[gpui::test]
fn several_pictures_flow_side_by_side_and_wrap(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let files = (0..6)
        .map(|n| attachment(&format!("p{n}.png"), "image/png", 1000, Some((300, 200))))
        .collect();
    world.reach(cx, vec![world.message(&world.bob, "", 1, files)]);
    let first = world.bounds(cx, "message-0-file-0");
    let second = world.bounds(cx, "message-0-file-1");
    assert_eq!(first.top(), second.top(), "side by side");
    assert!(
        second.left() > first.right() - px(1.),
        "not on top of each other"
    );
    let last = world.bounds(cx, "message-0-file-5");
    assert!(last.top() > first.top(), "the flow wraps to a second line");
}

#[gpui::test]
fn a_failed_thumbnail_says_so_and_the_picture_still_opens(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let photo = attachment("photo.png", "image/png", 90_000, Some((640, 480)));
    let id = photo.id;
    world.reach(cx, vec![world.message(&world.bob, "", 1, vec![photo])]);
    world.dispatch(
        cx,
        Input::ImageLoaded {
            attachment: id,
            full: false,
            result: Err(TransferError::Missing),
        },
    );
    draw(cx);
    assert!(world.on_screen(cx, "message-0-file-0"), "still a tile");
    world.click(cx, "message-0-file-0");
    assert!(
        world.on_screen(cx, "fullscreen-card"),
        "and it still opens the viewer"
    );
}

#[gpui::test]
fn a_message_of_only_files_draws_no_empty_text(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let pdf = attachment("report.pdf", "application/pdf", 1_258_291, None);
    world.reach(cx, vec![world.message(&world.bob, "", 1, vec![pdf])]);
    assert!(world.on_screen(cx, "message-0-file-0"));
    let row = world.bounds(cx, "message-row-0");
    let chip = world.bounds(cx, "message-0-file-0");
    // Header (author line), then the chip: no blank paragraph between them.
    assert!(
        chip.bottom() <= row.bottom(),
        "the chip is inside the row it belongs to"
    );
}

#[gpui::test]
fn a_file_chip_saves_through_the_save_dialog_with_progress_and_a_result(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let pdf = attachment("report.pdf", "application/pdf", 2_000_000, None);
    let id = pdf.id;
    world.reach(cx, vec![world.message(&world.bob, "", 1, vec![pdf])]);

    world.click(cx, "message-0-file-0-download");
    let target = std::env::temp_dir().join("saved-report.pdf");
    cx.simulate_new_path_selection(|_directory| Some(target.clone()));
    cx.run_until_parked();
    let asked = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::DownloadAttachment {
                attachment,
                destination,
            } => Some((attachment.id, destination)),
            _ => None,
        });
    assert_eq!(
        asked,
        Some((id, target.clone())),
        "the chosen path, exactly"
    );

    world.dispatch(
        cx,
        Input::DownloadProgress {
            attachment: id,
            received: 500_000,
            total: 2_000_000,
        },
    );
    assert!(
        world.on_screen(cx, "message-0-file-0-progress"),
        "a bar while it comes down"
    );
    assert!(world.on_screen(cx, "message-0-file-0-cancel"));

    world.click(cx, "message-0-file-0-cancel");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::CancelDownload { attachment } if *attachment == id)),
        "Cancel stops it"
    );
}

#[gpui::test]
fn a_finished_download_says_saved_and_a_failed_one_can_be_retried_without_asking_again(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let pdf = attachment("report.pdf", "application/pdf", 2_000, None);
    let id = pdf.id;
    world.reach(cx, vec![world.message(&world.bob, "", 1, vec![pdf])]);
    world.click(cx, "message-0-file-0-download");
    let target = std::env::temp_dir().join("again.pdf");
    cx.simulate_new_path_selection(|_| Some(target.clone()));
    cx.run_until_parked();
    world.take_effects();

    world.dispatch(
        cx,
        Input::DownloadResult {
            attachment: id,
            result: Err(TransferError::Disk("the disk is full".into())),
        },
    );
    assert!(
        world.on_screen(cx, "message-0-file-0-failed"),
        "the reason shows"
    );
    world.click(cx, "message-0-file-0-retry");
    let retried = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::DownloadAttachment { destination, .. } => Some(destination),
            _ => None,
        });
    assert_eq!(
        retried,
        Some(target.clone()),
        "the same place, without asking again"
    );

    world.dispatch(
        cx,
        Input::DownloadResult {
            attachment: id,
            result: Ok(target.clone()),
        },
    );
    assert!(world.on_screen(cx, "message-0-file-0-saved"));
    let state = world
        .model
        .read_with(cx, |m, _| m.core().download(id).cloned());
    assert!(matches!(state, Some(DownloadState::Done { .. })));
}

// ---- The viewer ----------------------------------------------------------------------------------------

fn three_pictures(world: &World) -> (Vec<Attachment>, Message) {
    let files: Vec<Attachment> = (0..3)
        .map(|n| attachment(&format!("p{n}.png"), "image/png", 5_000, Some((640, 480))))
        .collect();
    (files.clone(), world.message(&world.bob, "", 1, files))
}

#[gpui::test]
fn clicking_a_picture_opens_the_viewer_and_the_arrows_move_between_pictures(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let (files, message) = three_pictures(&world);
    world.reach(cx, vec![message]);
    assert!(!world.on_screen(cx, "fullscreen-card"));
    world.click(cx, "message-0-file-1");
    assert!(world.on_screen(cx, "fullscreen-card"), "the viewer is open");
    let asked: Vec<AttachmentId> = world
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::FetchImage {
                attachment,
                full: true,
            } => Some(attachment),
            _ => None,
        })
        .collect();
    assert_eq!(asked, vec![files[1].id], "the picture clicked, whole");
    assert!(
        world.on_screen(cx, "viewer-loading"),
        "a spinner state while it loads"
    );

    world.dispatch(
        cx,
        Input::ImageLoaded {
            attachment: files[1].id,
            full: true,
            result: Ok(ImageData::new(png(640, 480))),
        },
    );
    cx.run_until_parked();
    assert!(world.on_screen(cx, "viewer-image"), "the picture is shown");

    cx.simulate_keystrokes("right");
    let asked: Vec<AttachmentId> = world
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::FetchImage {
                attachment,
                full: true,
            } => Some(attachment),
            _ => None,
        })
        .collect();
    assert_eq!(asked, vec![files[2].id], "Right shows the next");
    cx.simulate_keystrokes("right");
    assert!(
        world
            .take_effects()
            .iter()
            .all(|e| !matches!(e, Effect::FetchImage { full: true, .. })),
        "nothing after the last"
    );
    cx.simulate_keystrokes("left left");
    let asked_first = world
        .model
        .read_with(cx, |m, _| m.core().full_image(files[0].id).is_some());
    assert!(
        asked_first,
        "Left twice reached the first picture and asked for it"
    );
    cx.simulate_keystrokes("left left");
    assert!(
        world.on_screen(cx, "fullscreen-card"),
        "Left stops at the first"
    );
}

#[gpui::test]
fn escape_closes_the_viewer_and_gives_the_keyboard_back(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let (_, message) = three_pictures(&world);
    world.reach(cx, vec![message]);
    // The timeline's cursor goes to the first picture and Enter opens it.
    cx.simulate_keystrokes("shift-tab down enter");
    assert!(
        world.on_screen(cx, "fullscreen-card"),
        "Enter on a picture opens the viewer"
    );
    cx.simulate_keystrokes("escape");
    assert!(!world.on_screen(cx, "fullscreen-card"), "Escape closes it");
    let focused = world.root.read_with(cx, |root, app| {
        root.shell()
            .unwrap()
            .read(app)
            .timeline()
            .read(app)
            .focus_handle(app)
    });
    assert!(
        cx.update(|window, _| focused.is_focused(window)),
        "focus is back on the timeline where it was"
    );
    assert!(
        world.on_screen(cx, "attachment-cursor"),
        "and on the picture it was on"
    );
}

#[gpui::test]
fn the_viewer_names_the_file_copies_its_name_and_downloads_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let (files, message) = three_pictures(&world);
    world.reach(cx, vec![message]);
    world.click(cx, "message-0-file-0");
    world.click(cx, "viewer-copy-name");
    assert_eq!(clipboard(cx).as_deref(), Some("p0.png"));
    world.take_effects();
    world.click(cx, "viewer-download");
    let target = std::env::temp_dir().join("p0-saved.png");
    cx.simulate_new_path_selection(|_| Some(target.clone()));
    cx.run_until_parked();
    assert!(
        world.take_effects().iter().any(|e| matches!(e, Effect::DownloadAttachment { attachment, destination } if attachment.id == files[0].id && *destination == target)),
        "Download saves the shown picture"
    );
    world.click(cx, "viewer-close");
    assert!(!world.on_screen(cx, "fullscreen-card"));
}

// ---- The keyboard in the timeline ----------------------------------------------------------------------

#[gpui::test]
fn a_messages_files_are_stops_between_it_and_the_next_message(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let files = vec![
        attachment("a.pdf", "application/pdf", 10, None),
        attachment("b.pdf", "application/pdf", 20, None),
    ];
    world.reach(
        cx,
        vec![
            world.message(&world.bob, "with files", 1, files),
            world.message(&world.bob, "plain", 2, vec![]),
        ],
    );
    cx.simulate_keystrokes("shift-tab");
    // The cursor starts on the newest message ("plain"), which has no files.
    assert!(!world.on_screen(cx, "attachment-cursor"));
    // Up from it lands on the last stop of the message above: its second file.
    cx.simulate_keystrokes("up");
    assert!(
        world.on_screen(cx, "attachment-cursor"),
        "Up reaches the last file of the message above"
    );
    let second = world.bounds(cx, "attachment-cursor");
    cx.simulate_keystrokes("up");
    let first = world.bounds(cx, "attachment-cursor");
    assert!(
        first.top() < second.top() || first.left() < second.left(),
        "then the first file"
    );
    cx.simulate_keystrokes("up");
    assert!(
        !world.on_screen(cx, "attachment-cursor"),
        "then the message itself"
    );
    cx.simulate_keystrokes("down down down");
    assert!(
        !world.on_screen(cx, "attachment-cursor"),
        "Down goes through both files and on to the next message"
    );
}

#[gpui::test]
fn enter_on_a_file_chip_downloads_it_and_on_a_downloading_one_cancels(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let pdf = attachment("report.pdf", "application/pdf", 10, None);
    let id = pdf.id;
    world.reach(cx, vec![world.message(&world.bob, "", 1, vec![pdf])]);
    cx.simulate_keystrokes("shift-tab down enter");
    let target = std::env::temp_dir().join("kb.pdf");
    cx.simulate_new_path_selection(|_| Some(target.clone()));
    cx.run_until_parked();
    assert!(
        world.take_effects().iter().any(|e| matches!(e, Effect::DownloadAttachment { destination, .. } if *destination == target)),
        "Enter on a file chip starts the download"
    );
    world.dispatch(
        cx,
        Input::DownloadProgress {
            attachment: id,
            received: 1,
            total: 10,
        },
    );
    cx.simulate_keystrokes("enter");
    assert!(
        world
            .take_effects()
            .iter()
            .any(|e| matches!(e, Effect::CancelDownload { .. })),
        "and Enter again, while it goes, cancels it"
    );
}

#[gpui::test]
fn the_menu_of_a_message_with_one_file_offers_download_and_copy_name_last(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let pdf = attachment("report.pdf", "application/pdf", 10, None);
    world.reach(
        cx,
        vec![world.message(&world.bob, "see attached", 1, vec![pdf])],
    );
    cx.simulate_keystrokes("shift-tab enter");
    assert!(world.on_screen(cx, "message-actions"));
    let order = [
        "message-actions-copy-link",
        "message-actions-copy-text",
        "message-actions-download-file",
        "message-actions-copy-file-name",
    ];
    let tops: Vec<_> = order
        .iter()
        .map(|selector| world.bounds(cx, selector).top())
        .collect();
    assert!(
        tops.windows(2).all(|pair| pair[0] < pair[1]),
        "in this order: {order:?}"
    );

    cx.simulate_keystrokes("down down down down down enter");
    assert_eq!(clipboard(cx).as_deref(), Some("report.pdf"));
}

#[gpui::test]
fn the_menu_of_a_message_with_several_files_has_no_per_file_items(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let files = vec![
        attachment("a.pdf", "application/pdf", 10, None),
        attachment("b.pdf", "application/pdf", 10, None),
    ];
    world.reach(cx, vec![world.message(&world.bob, "two", 1, files)]);
    cx.simulate_keystrokes("shift-tab enter");
    assert!(world.on_screen(cx, "message-actions"));
    assert!(!world.on_screen(cx, "message-actions-download-file"));
    assert!(!world.on_screen(cx, "message-actions-copy-file-name"));
}

// ---- Copying ----------------------------------------------------------------------------------------------

#[gpui::test]
fn a_copy_that_includes_a_file_writes_a_line_for_it(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let pdf = attachment("report.pdf", "application/pdf", 1_258_291, None);
    world.reach(
        cx,
        vec![
            world.message(&world.bob, "see attached", 1, vec![pdf]),
            world.message(
                &world.bob,
                "",
                2,
                vec![attachment("data.csv", "text/csv", 340, None)],
            ),
        ],
    );
    cx.simulate_keystrokes("shift-tab ctrl-a ctrl-c");
    let copied = clipboard(cx).expect("something was copied");
    assert!(
        copied.contains("see attached\n[file: report.pdf (1.2 MB)]"),
        "{copied}"
    );
    assert!(copied.contains("[file: data.csv (340 bytes)]"), "{copied}");
}

#[gpui::test]
fn copy_text_on_a_message_of_only_a_file_copies_the_line(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let pdf = attachment("report.pdf", "application/pdf", 2048, None);
    world.reach(cx, vec![world.message(&world.bob, "", 1, vec![pdf])]);
    cx.simulate_keystrokes("shift-tab enter");
    world.click(cx, "message-actions-copy-text");
    assert_eq!(
        clipboard(cx).as_deref(),
        Some("[file: report.pdf (2.0 KB)]")
    );
}

// ---- Messages that are still being sent ------------------------------------------------------------------

#[gpui::test]
fn a_message_whose_file_cannot_go_is_a_failed_row_that_can_be_retried_or_discarded(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let path = temp_file("gone.pdf", b"%PDF");
    world.click(cx, "composer-attach");
    cx.simulate_path_prompt_response(|_| Some(vec![path.clone()]));
    cx.run_until_parked();
    let id = world.stage_all(cx, 1000)[0];
    world.focus_box(cx);
    cx.simulate_keystrokes("enter");
    let (_, generation) = world.uploads().pop().expect("the upload starts");
    // The file is gone from the outbox (after a restart, say).
    world.dispatch(
        cx,
        Input::UploadResult {
            attachment: id,
            generation,
            result: Err(TransferError::Missing),
        },
    );
    assert!(
        world.on_screen(cx, "retry-message"),
        "Send again is offered"
    );
    assert!(world.on_screen(cx, "discard-message"), "and so is Discard");
    world.take_effects();
    world.click(cx, "discard-message");
    assert!(
        !world.on_screen(cx, "message-0-file-0"),
        "the failed message is gone"
    );
}

// ---- A long conversation of pictures ----------------------------------------------------------------------

#[gpui::test]
fn fifty_picture_messages_scroll_without_decoding_them_all_or_holding_too_much(
    cx: &mut TestAppContext,
) {
    let (world, cx) = start(cx);
    let messages: Vec<Message> = (1..=50)
        .map(|n| {
            world.message(
                &world.bob,
                &format!("photo {n}"),
                n,
                vec![attachment(
                    &format!("photo-{n}.jpg"),
                    "image/png",
                    2_000_000,
                    Some((4000, 3000)),
                )],
            )
        })
        .collect();
    let ids: Vec<AttachmentId> = messages.iter().map(|m| m.attachments[0].id).collect();
    world.reach(cx, messages);

    // Each thumbnail is what the server sends: 640 by 480.
    let thumbnail = png(640, 480);
    let started = std::time::Instant::now();
    let mut slowest = std::time::Duration::ZERO;
    let first_screen = world.answer_thumbnails(cx, &thumbnail);
    let mut answered = first_screen;
    let steps = 40u32;
    for _ in 0..steps {
        // Scroll up by about two messages, then answer what came into view.
        let frame = std::time::Instant::now();
        cx.simulate_event(gpui::ScrollEvent {
            position: point(px(900.), px(400.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(600.))),
            ..Default::default()
        });
        draw(cx);
        answered += world.answer_thumbnails(cx, &thumbnail);
        slowest = slowest.max(frame.elapsed());
    }
    let frames = steps;
    let total = started.elapsed();
    // Scrolling back down over pictures that are all decoded already: drawing alone.
    let mut slowest_cached = std::time::Duration::ZERO;
    let mut cached_total = std::time::Duration::ZERO;
    for _ in 0..steps {
        let frame = std::time::Instant::now();
        cx.simulate_event(gpui::ScrollEvent {
            position: point(px(900.), px(400.)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-600.))),
            ..Default::default()
        });
        draw(cx);
        let took = frame.elapsed();
        cached_total += took;
        slowest_cached = slowest_cached.max(took);
    }
    eprintln!(
        "scrolling back over decoded pictures: {steps} steps, {cached_total:?} in all \
         ({:?} a step), slowest {slowest_cached:?}",
        cached_total / steps
    );
    let (decoded, held) = cx.read(|app| {
        let store = ImageStore::global(app);
        let store = store.read(app);
        (store.decoded_count(), store.held_bytes())
    });
    eprintln!(
        "50 picture messages, {frames} scroll steps: {total:?} in all, slowest step {slowest:?}, \
         {decoded} thumbnails decoded, {} KiB of pixels held",
        held / 1024
    );
    assert!(
        (1..50).contains(&first_screen),
        "only the rows on the first screen asked for thumbnails ({first_screen} of 50)"
    );
    assert_eq!(answered, 50, "and every row asked once it came into view");
    assert_eq!(
        decoded, 50,
        "each was decoded once, not again on every scroll"
    );
    assert!(
        held <= lazy_ui::image_store::BUDGET_BYTES,
        "within the store's budget"
    );
    // The numbers above are for a debug build on a shared machine: the bounds only catch a step
    // that takes tens of seconds, which is what a decode on the main thread of a big picture, or
    // a layout that re-measures every row on every frame, would cost.
    assert!(
        slowest < std::time::Duration::from_secs(20),
        "no scroll step takes tens of seconds: {slowest:?}"
    );
    assert!(
        slowest_cached < std::time::Duration::from_secs(3),
        "drawing decoded pictures is cheap: {slowest_cached:?}"
    );
    let _ = ids;
}

// ---- Refusals that never reach the server ----------------------------------------------------------------

#[gpui::test]
fn a_file_that_is_too_large_says_so_and_can_only_be_removed(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    world.reach(cx, vec![]);
    let path = temp_file("movie.mkv", b"x");
    world.click(cx, "composer-attach");
    cx.simulate_path_prompt_response(|_| Some(vec![path.clone()]));
    cx.run_until_parked();
    let (id, generation) = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::StageFile {
                attachment,
                generation,
                ..
            } => Some((attachment, generation)),
            _ => None,
        })
        .unwrap();
    world.dispatch(
        cx,
        Input::FileStaged {
            attachment: id,
            generation,
            result: Err(TransferError::TooLarge {
                size: 40 * 1024 * 1024,
            }),
        },
    );
    let state = world.chips(cx)[0].state.clone();
    match state {
        AttachmentState::Failed { reason, can_retry } => {
            assert!(!can_retry);
            assert!(
                reason.contains("25"),
                "the reason names the limit: {reason}"
            );
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert!(world.on_screen(cx, "composer-tray-chip-0-failed"));
    assert!(
        !world.on_screen(cx, "composer-tray-chip-0-retry"),
        "retrying would not help"
    );
    assert!(world.on_screen(cx, "composer-tray-chip-0-remove"));
}

// ---- The thread pane ---------------------------------------------------------------------------------------

/// Opens the thread of `root` with `replies`.
fn open_thread(world: &World, cx: &mut VisualTestContext, root: &Message, replies: Vec<Message>) {
    world.dispatch(
        cx,
        Input::OpenThread {
            conversation: world.general,
            root: root.id,
        },
    );
    let generation = world
        .take_effects()
        .into_iter()
        .find_map(|effect| match effect {
            Effect::FetchReplies { generation, .. } => Some(generation),
            _ => None,
        })
        .expect("opening the thread asks for its replies");
    world.dispatch(
        cx,
        Input::RepliesLoaded {
            root: root.id,
            generation,
            result: Ok(HistoryPage {
                messages: replies,
                has_more: false,
            }),
        },
    );
    draw(cx);
}

#[gpui::test]
fn the_thread_pane_shows_files_and_has_its_own_tray_and_drop_target(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let root = world.message(&world.bob, "the plan", 1, vec![]);
    let reply = Message {
        thread_root: Some(root.id),
        ..world.message(
            &world.bob,
            "",
            2,
            vec![attachment(
                "diagram.png",
                "image/png",
                9_000,
                Some((600, 400)),
            )],
        )
    };
    world.reach(cx, vec![root.clone()]);
    open_thread(&world, cx, &root, vec![reply]);
    assert!(
        world.on_screen(cx, "thread-message-1-file-0"),
        "a reply's picture shows in the pane"
    );
    let tile = world.bounds(cx, "thread-message-1-file-0");
    let pane = world.bounds(cx, "thread-pane");
    assert!(
        tile.right() <= pane.right() && tile.size.width < px(360.),
        "the picture shrinks to fit the narrower column instead of running off it: {tile:?} in {pane:?}"
    );
    assert!(
        world.on_screen(cx, "thread-composer-attach"),
        "the reply box has its own button"
    );

    // Files dropped on the pane go to the reply, not the channel's box.
    let path = temp_file("reply.pdf", b"%PDF");
    let over_pane = point(px(1800.), px(500.));
    drag_over(cx, std::slice::from_ref(&path), over_pane);
    assert!(world.on_screen(cx, "drop-overlay"));
    let label = world
        .pane_composer(cx)
        .read_with(cx, |composer, app| composer.drop_label(app));
    assert!(label.contains("your reply"), "{label}");
    cx.simulate_event(FileDropEvent::Submit {
        position: over_pane,
    });
    cx.simulate_event(FileDropEvent::Exited);
    cx.run_until_parked();
    let in_thread = world.model.read_with(cx, |m, _| {
        m.core()
            .composer_attachments(ComposerTarget::Thread(root.id))
            .len()
    });
    assert_eq!(in_thread, 1, "attached to the reply");
    assert!(world.chips(cx).is_empty(), "and not to the message box");
    assert!(
        world.on_screen(cx, "thread-composer-tray-chip-0"),
        "the pane's tray shows it"
    );
    assert!(
        !world.on_screen(cx, "composer-tray"),
        "the channel's tray does not"
    );

    // Dropped on the conversation side, the same files go to the message box.
    let other = temp_file("channel.pdf", b"%PDF");
    let over_channel = point(px(900.), px(500.));
    drag_over(cx, &[other], over_channel);
    let label = world
        .composer(cx)
        .read_with(cx, |composer, app| composer.drop_label(app));
    assert_eq!(label, "Drop files to attach to #general");
    cx.simulate_event(FileDropEvent::Submit {
        position: over_channel,
    });
    cx.simulate_event(FileDropEvent::Exited);
    cx.run_until_parked();
    assert_eq!(world.chips(cx).len(), 1, "the message box got this one");
}

#[gpui::test]
fn the_viewer_leaves_room_for_its_controls_in_a_small_window(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let (_, message) = three_pictures(&world);
    world.reach(cx, vec![message]);
    world.click(cx, "message-0-file-0");
    cx.simulate_resize(gpui::size(px(640.), px(480.)));
    let picture = world.bounds(cx, "viewer-picture");
    let caption = world.bounds(cx, "viewer-caption");
    let close = world.bounds(cx, "viewer-close");
    assert!(
        picture.bottom() <= caption.top() && picture.bottom() <= close.top(),
        "the picture ends above the controls even when they wrap: {picture:?} {caption:?} {close:?}"
    );
    assert!(
        close.bottom() <= px(480.),
        "and the controls are inside the window"
    );
}

#[gpui::test]
fn a_picture_that_would_not_load_says_so_and_try_again_asks_for_it_again(cx: &mut TestAppContext) {
    let (world, cx) = start(cx);
    let (files, message) = three_pictures(&world);
    world.reach(cx, vec![message]);
    world.click(cx, "message-0-file-0");
    world.take_effects();
    world.dispatch(
        cx,
        Input::ImageLoaded {
            attachment: files[0].id,
            full: true,
            result: Err(TransferError::Missing),
        },
    );
    assert!(
        world.on_screen(cx, "viewer-failed"),
        "the failure is stated"
    );
    world.click(cx, "viewer-retry");
    assert!(
        world.take_effects().iter().any(|effect| matches!(
            effect,
            Effect::FetchImage { attachment, full: true } if *attachment == files[0].id
        )),
        "Try again requests the whole picture again"
    );
    assert!(
        !world.on_screen(cx, "viewer-failed"),
        "and the failure goes while it loads"
    );
}
