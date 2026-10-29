//! Printing that cannot leak (slice 3I, D24): the `Debug` text of every input and effect that can
//! carry something the person typed, a name, a path, or a secret shows what happened and never
//! that. The strings below are distinctive words put into every such field; none may come out.

mod common;

use std::path::PathBuf;

use common::{Driver, World};
use lazy_client_core::{
    ContentCache, Effect, FileBytes, FileSource, Input, JournalEntry, LocalFile, RequestError,
    StartupState, TransferError,
};
use lazy_domain::{AttachmentId, ConversationId, MessageDocument, OperationId, Timestamp};
use lazy_protocol::{
    ApiError, ErrorCode, InviteCode, Password, RegisterRequest, SearchResponse, ServerFrame,
};

/// Goes into every field below. A word nobody would write by accident.
const SECRET: &str = "quokka-zephyr-4471";

fn typed(what: &str) -> String {
    format!("{what}-{SECRET}")
}

fn assert_quiet(label: &str, text: &str) {
    assert!(
        !text.contains(SECRET) && !text.contains("quokka") && !text.contains("zephyr"),
        "{label} printed what was typed:\n{text}"
    );
}

fn path_file(name: &str) -> LocalFile {
    LocalFile {
        source: FileSource::Path(
            PathBuf::from("/home/jules")
                .join(typed("folder"))
                .join(name),
        ),
        name: name.to_owned(),
    }
}

/// Every input a person can type something into, with the typed words in every text field.
fn typed_inputs(world: &World, conversation: ConversationId) -> Vec<(&'static str, Input)> {
    let root = world
        .message(conversation, &world.other, 3, &typed("root"))
        .id;
    let status = lazy_domain::notification::UserStatus::new(&typed("status"), None, None).unwrap();
    vec![
        (
            "Register",
            Input::Register {
                username: typed("user"),
                display_name: typed("display"),
                password: typed("password"),
                invite_code: typed("invite"),
            },
        ),
        (
            "SignIn",
            Input::SignIn {
                username: typed("user"),
                password: typed("password"),
            },
        ),
        (
            "SetDraft",
            Input::SetDraft {
                conversation,
                text: typed("draft"),
            },
        ),
        (
            "SendMessage",
            Input::SendMessage {
                conversation,
                text: typed("message"),
            },
        ),
        (
            "EditMessage",
            Input::EditMessage {
                conversation,
                message: root,
                text: typed("edit"),
            },
        ),
        (
            "SendReply",
            Input::SendReply {
                conversation,
                root,
                text: typed("reply"),
                also_in_channel: false,
            },
        ),
        (
            "SetThreadDraft",
            Input::SetThreadDraft {
                root,
                text: typed("thread draft"),
            },
        ),
        (
            "SetSearchQuery",
            Input::SetSearchQuery {
                text: typed("search"),
            },
        ),
        ("RunSearch", Input::RunSearch),
        (
            "SetServerUrl",
            Input::SetServerUrl(format!(
                "https://{}:{}@host.example",
                typed("user"),
                typed("pw")
            )),
        ),
        ("CheckServer", Input::CheckServer),
        (
            "ServerChecked",
            Input::ServerChecked {
                generation: 1,
                result: Err(typed("why")),
            },
        ),
        (
            "ServerSwitched",
            Input::ServerSwitched {
                generation: 1,
                result: Err(typed("why")),
            },
        ),
        ("SetStatus", Input::SetStatus(Some(status))),
        ("SetPeopleQuery", Input::SetPeopleQuery(typed("people"))),
        (
            "CreateChannel",
            Input::CreateChannel {
                name: typed("channel"),
                visibility: lazy_domain::ChannelVisibility::Public,
                topic: typed("topic"),
                description: typed("description"),
            },
        ),
        (
            "CreateWorkspace",
            Input::CreateWorkspace {
                name: typed("workspace"),
            },
        ),
        (
            "AcceptInvitation",
            Input::AcceptInvitation {
                code: typed("code"),
            },
        ),
        (
            "UpdateProfile",
            Input::UpdateProfile {
                display_name: Some(typed("profile")),
            },
        ),
        (
            "SetAvatar",
            Input::SetAvatar {
                source: FileSource::Path(PathBuf::from("/home/jules").join(typed("avatar"))),
            },
        ),
        (
            "AttachFiles",
            Input::AttachFiles {
                target: lazy_client_core::ComposerTarget::Conversation(conversation),
                files: vec![
                    path_file(&typed("report.pdf")),
                    LocalFile {
                        source: FileSource::Bytes(FileBytes::new(SECRET.as_bytes().to_vec())),
                        name: typed("pasted.png"),
                    },
                ],
            },
        ),
        // Results that carry what the server or the system said.
        (
            "SendResult (unknown)",
            Input::SendResult {
                operation: OperationId::generate(),
                result: Err(RequestError::Unknown(typed("detail"))),
            },
        ),
        (
            "SendResult (rejected)",
            Input::SendResult {
                operation: OperationId::generate(),
                result: Err(RequestError::Rejected(ApiError::new(
                    ErrorCode::InvalidRequest,
                    typed("server message"),
                ))),
            },
        ),
        (
            "SearchResult",
            Input::SearchResult {
                generation: 1,
                result: Ok(SearchResponse {
                    hits: Vec::new(),
                    has_more: false,
                    truncated: false,
                }),
            },
        ),
        (
            "UploadResult (disk)",
            Input::UploadResult {
                attachment: AttachmentId::generate(),
                generation: 1,
                result: Err(TransferError::Disk(typed("/home/jules/file"))),
            },
        ),
        (
            "UploadResult (unreadable)",
            Input::UploadResult {
                attachment: AttachmentId::generate(),
                generation: 1,
                result: Err(TransferError::Unreadable(typed("/home/jules/file"))),
            },
        ),
        (
            "DownloadResult",
            Input::DownloadResult {
                attachment: AttachmentId::generate(),
                result: Err(TransferError::Disk(typed("/home/jules/file"))),
            },
        ),
        ("StorageProblem", Input::StorageProblem(typed("problem"))),
        (
            "SubmissionPersistFailed",
            Input::SubmissionPersistFailed {
                operation: OperationId::generate(),
                message: typed("could not write"),
            },
        ),
        (
            "BootstrapFailed",
            Input::BootstrapFailed(RequestError::Unknown(typed("detail"))),
        ),
    ]
}

#[test]
fn what_a_person_typed_never_appears_in_the_debug_of_an_input() {
    let world = World::new();
    for (label, input) in typed_inputs(&world, world.channel.id) {
        let text = format!("{input:?}");
        assert_quiet(label, &text);
        let variant = label.split(' ').next().unwrap();
        assert!(
            text.starts_with(variant),
            "{label} should still say what happened: {text}"
        );
    }
}

#[test]
fn lengths_and_identifiers_stay_so_a_log_can_still_be_read() {
    let world = World::new();
    let conversation = world.channel.id;
    let text = format!(
        "{:?}",
        Input::SendMessage {
            conversation,
            text: "twelve chars".to_owned(),
        }
    );
    assert!(text.contains("<redacted 12 chars>"), "{text}");
    assert!(text.contains(&conversation.to_string()), "{text}");
    let text = format!(
        "{:?}",
        Input::SignIn {
            username: "abc".to_owned(),
            password: "defgh".to_owned(),
        }
    );
    assert!(
        text.contains("<redacted 3 chars>") && text.contains("<redacted 5 chars>"),
        "{text}"
    );
    let text = format!(
        "{:?}",
        Input::SendResult {
            operation: OperationId::generate(),
            result: Err(RequestError::Rejected(ApiError::new(
                ErrorCode::Forbidden,
                "you may not",
            ))),
        }
    );
    assert!(text.contains("Rejected(Forbidden)"), "{text}");
}

/// Runs the typed inputs through a core and prints everything that comes out.
#[test]
fn nothing_the_core_asks_for_prints_what_was_typed() {
    let world = World::new();
    let conversation = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(conversation, vec![], false);
    let mut printed = String::new();
    for (label, input) in typed_inputs(&world, conversation) {
        let out = driver.send(input);
        // The whole output (effects and changes) as the host would log it.
        let text = format!("{out:?}");
        assert_quiet(&format!("the output of {label}"), &text);
        printed.push_str(&text);
    }
    // The scenario really did make requests, so the test is looking at something.
    assert!(
        driver
            .effects
            .iter()
            .any(|e| matches!(e, Effect::PersistSubmission(_))),
        "a message was journaled"
    );
    assert!(
        driver
            .effects
            .iter()
            .any(|e| matches!(e, Effect::StageFile { .. })),
        "a file was staged"
    );
    assert!(
        driver
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Search { .. })),
        "a search was made"
    );
    for effect in &driver.effects {
        assert_quiet("an effect", &format!("{effect:?}"));
    }
    assert_quiet("the diagnostics", &driver.core.diagnostics().report());
    assert!(!printed.is_empty());
}

#[test]
fn a_loaded_conversation_prints_no_message_text() {
    let mut world = World::new();
    world.me = common::user(&typed("me"));
    let conversation = world.channel.id;
    let mut driver = Driver::ready(&world);
    let messages: Vec<_> = (1..=3)
        .map(|at| world.message(conversation, &world.other, at, &typed("said")))
        .collect();
    // The inputs the runtime hands the core carry the messages; their Debug must not.
    let page = Input::HistoryLoaded {
        conversation,
        generation: 1,
        result: Ok(lazy_protocol::HistoryPage {
            messages: messages.clone(),
            has_more: false,
        }),
    };
    assert_quiet("a page of history", &format!("{page:?}"));
    driver.load_history(conversation, messages, false);
    let live = world.event(
        world.message(conversation, &world.other, 11, &typed("live")),
        None,
    );
    assert_quiet("a live message", &format!("{live:?}"));
    driver.send(live);
    let said = Input::Frame(ServerFrame::Error {
        code: ErrorCode::InvalidRequest,
        message: typed("what the server said"),
    });
    assert_quiet("a frame with a message", &format!("{said:?}"));
    for effect in &driver.effects {
        assert_quiet("an effect", &format!("{effect:?}"));
    }
    // The cache the core asks to have saved holds those messages, and still prints none of them.
    let caches: Vec<_> = driver
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::PersistCache(cache) => Some(cache),
            _ => None,
        })
        .collect();
    assert!(!caches.is_empty(), "the cache was saved");
    assert!(
        caches.iter().any(|cache| cache.message_count() > 0),
        "with messages in it"
    );
    for cache in caches {
        assert_quiet("a content cache", &format!("{cache:?}"));
    }
    assert_quiet("the diagnostics", &driver.core.diagnostics().report());
}

#[test]
fn the_server_choice_prints_no_address_and_nothing_typed_into_it() {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState::default()));
    // An address that is accepted holds the words; it is asked, and no print of it shows them.
    let address = format!("{}.example.com", typed("chat"));
    let typed_input = Input::SetServerUrl(address);
    assert_quiet("a typed address", &format!("{typed_input:?}"));
    let out = driver.send(typed_input);
    let asked: Vec<_> = out
        .effects
        .iter()
        .filter(|effect| matches!(effect, Effect::CheckServer { .. }))
        .collect();
    assert_eq!(asked.len(), 1, "the address was asked about");
    for effect in &out.effects {
        assert_quiet("the check", &format!("{effect:?}"));
    }
    assert_quiet("the output", &format!("{out:?}"));
    assert!(
        format!("{:?}", asked[0]).contains("<redacted"),
        "{:?}",
        asked[0]
    );
    // The core's own view of it is readable by the screen but not by a log of the core's inputs.
    assert!(
        driver
            .core
            .server()
            .trying
            .as_deref()
            .is_some_and(|url| url.contains("quokka"))
    );
    assert_quiet("the diagnostics", &driver.core.diagnostics().report());
    // The same for the effect that moves, and for the state the move carries.
    let Effect::CheckServer { generation, .. } = asked[0] else {
        unreachable!()
    };
    let out = driver.send(Input::ServerChecked {
        generation: *generation,
        result: Ok(lazy_protocol::PROTOCOL_VERSION),
    });
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::UseServer { .. }))
    );
    for effect in &out.effects {
        assert_quiet("the move", &format!("{effect:?}"));
    }
    let switched = Input::ServerSwitched {
        generation: *generation,
        result: Ok(Box::new(StartupState {
            server: lazy_client_core::ServerInfo {
                url: format!("https://{}.example.com", typed("chat")),
                ..Default::default()
            },
            ..Default::default()
        })),
    };
    assert_quiet("the switched input", &format!("{switched:?}"));
    let started = Input::Started(StartupState {
        server: lazy_client_core::ServerInfo {
            url: format!("https://{}.example.com", typed("chat")),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_quiet("the started input", &format!("{started:?}"));
}

#[test]
fn state_read_from_disk_prints_no_content() {
    let world = World::new();
    let conversation = world.channel.id;
    let mut entry_drafts = std::collections::HashMap::new();
    entry_drafts.insert(conversation, typed("draft"));
    let entry = JournalEntry {
        operation_id: OperationId::generate(),
        user_id: world.me.id,
        conversation_id: conversation,
        document: MessageDocument::from_plain_text(&typed("unsent")).unwrap(),
        submitted_at: Timestamp::now(),
        kind: Default::default(),
        thread_root: None,
        also_in_channel: false,
        attachments: Vec::new(),
    };
    assert_quiet("a journal entry", &format!("{entry:?}"));
    let startup = StartupState {
        session_restored: true,
        journal: vec![entry.clone()],
        drafts: entry_drafts,
        recent_searches: vec![typed("search")],
        problems: vec![typed("problem")],
        cache: Some(ContentCache {
            user: world.me.clone(),
            workspaces: Vec::new(),
        }),
        ..Default::default()
    };
    assert_quiet("the startup state", &format!("{startup:?}"));
    assert_quiet(
        "the startup input",
        &format!("{:?}", Input::Started(startup)),
    );
    assert_quiet(
        "a persisted journal",
        &format!("{:?}", Effect::PersistSubmission(entry)),
    );
    assert_quiet(
        "persisted drafts",
        &format!(
            "{:?}",
            Effect::PersistDrafts(vec![(conversation, typed("draft"))])
        ),
    );
    assert_quiet(
        "persisted searches",
        &format!("{:?}", Effect::PersistRecentSearches(vec![typed("search")])),
    );
}

#[test]
fn files_and_notifications_print_no_names_or_paths() {
    let file = path_file(&typed("report.pdf"));
    assert_quiet("a local file", &format!("{file:?}"));
    assert_quiet(
        "a file source",
        &format!(
            "{:?}",
            FileSource::Path(PathBuf::from(typed("/home/jules/x")))
        ),
    );
    let world = World::new();
    let notification = lazy_client_core::DesktopNotification {
        conversation: world.channel.id,
        message: lazy_domain::MessageId::generate(),
        thread_root: None,
        title: typed("#design"),
        body: typed("Zoe: hello"),
        reason: lazy_domain::notification::NotificationReason::Mention,
        tag: typed("tag"),
        count: 2,
    };
    assert_quiet("a notification", &format!("{notification:?}"));
    assert_quiet(
        "the notify effect",
        &format!("{:?}", Effect::Notify(notification)),
    );
}

#[test]
fn the_secrets_keep_their_own_redaction() {
    let request = RegisterRequest {
        username: lazy_domain::Username::parse("someone").unwrap(),
        display_name: lazy_domain::DisplayName::parse("Someone").unwrap(),
        password: Password::new(typed("password")),
        invite_code: Some(InviteCode::new(typed("invite"))),
    };
    assert_quiet("a register request", &format!("{request:?}"));
    assert_quiet(
        "a register effect",
        &format!("{:?}", Effect::Register(request)),
    );
}
