//! The diagnostics report (slice 3I, D24): what the core says about its own health, and that it says
//! nothing about what the person has typed, read, or been told.

mod common;

use std::path::PathBuf;

use common::{Driver, World};
use lazy_client_core::{
    ClientCore, ComposerTarget, FileSource, Input, LocalFile, MAX_DIAGNOSTIC_EVENTS, RequestError,
    SocketEvent, StagedFile, StartupState, TransferError,
};
use lazy_domain::Timestamp;
use lazy_protocol::{ApiError, ErrorCode, PROTOCOL_VERSION, ServerFrame};

const SECRET: &str = "quokka-zephyr-4471";

fn typed(what: &str) -> String {
    format!("{what}-{SECRET}")
}

fn assert_quiet(label: &str, text: &str) {
    assert!(
        !text.contains("quokka") && !text.contains("zephyr"),
        "{label} holds what was typed:\n{text}"
    );
}

fn codes(core: &ClientCore) -> Vec<(String, String)> {
    core.diagnostics()
        .recent_errors
        .into_iter()
        .map(|event| (event.category, event.code))
        .collect()
}

fn refused(code: ErrorCode) -> RequestError {
    RequestError::Rejected(ApiError::new(code, typed("the server's words")))
}

#[test]
fn a_new_core_reports_that_it_is_loading_and_has_no_errors() {
    let core = ClientCore::new();
    let report = core.diagnostics();
    assert_eq!(report.protocol_version, PROTOCOL_VERSION);
    assert_eq!(report.phase, "loading");
    assert!(report.workspaces.is_empty());
    assert_eq!(
        (
            report.conversations,
            report.unsettled_operations,
            report.uploads_in_flight,
            report.cached_messages
        ),
        (0, 0, 0, 0)
    );
    assert!(report.recent_errors.is_empty());
    let text = report.report();
    assert!(
        text.contains(&format!("protocol: {PROTOCOL_VERSION}")),
        "{text}"
    );
    assert!(text.contains("session: loading"), "{text}");
    assert!(text.contains("recent errors: none"), "{text}");
}

#[test]
fn a_ready_core_reports_its_workspaces_and_how_each_is_connected() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let report = driver.core.diagnostics();
    assert_eq!(report.phase, "ready");
    assert_eq!(report.workspaces.len(), 1);
    assert_eq!(report.workspaces[0].id, world.workspace.id);
    assert!(report.workspaces[0].active);
    assert!(report.conversations >= 2, "the two channels are counted");

    let workspace = world.workspace.id;
    driver.send(Input::Socket(SocketEvent::Connecting {
        workspace,
        attempt: 0,
    }));
    driver.send(Input::Socket(SocketEvent::Opened { workspace }));
    let live = driver.core.diagnostics();
    assert_ne!(live.workspaces[0].connection, "offline");

    driver.send(Input::Socket(SocketEvent::Closed {
        workspace,
        attempt: 1,
    }));
    let closed = driver.core.diagnostics();
    assert!(
        closed.workspaces[0].connection.starts_with("reconnecting"),
        "{}",
        closed.workspaces[0].connection
    );
    assert_eq!(
        codes(&driver.core),
        vec![("socket".into(), "closed".into())]
    );
    let text = closed.report();
    assert!(text.contains("(active)"), "{text}");
    assert!(text.contains(&workspace.to_string()), "{text}");
}

#[test]
fn unsent_messages_and_uploads_are_counted() {
    let world = World::new();
    let conversation = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(conversation, vec![], false);
    assert_eq!(driver.core.diagnostics().unsettled_operations, 0);
    driver.send(Input::SendMessage {
        conversation,
        text: typed("one"),
    });
    assert_eq!(driver.core.diagnostics().unsettled_operations, 1);
    driver.send(Input::SendMessage {
        conversation,
        text: typed("two"),
    });
    assert_eq!(driver.core.diagnostics().unsettled_operations, 2);

    driver.send(Input::AttachFiles {
        target: ComposerTarget::Conversation(conversation),
        files: vec![LocalFile {
            source: FileSource::Path(PathBuf::from("/home/jules").join(typed("file"))),
            name: typed("file.txt"),
        }],
    });
    let attachment = driver
        .core
        .composer_attachments(ComposerTarget::Conversation(conversation))[0]
        .id;
    let generation = driver
        .effects
        .iter()
        .rev()
        .find_map(|effect| match effect {
            lazy_client_core::Effect::StageFile { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the file is being copied");
    assert_eq!(
        driver.core.diagnostics().uploads_in_flight,
        0,
        "copying is not uploading"
    );
    driver.send(Input::FileStaged {
        attachment,
        generation,
        result: Ok(StagedFile { size: 10 }),
    });
    assert_eq!(driver.core.diagnostics().uploads_in_flight, 1);
    // The upload fails: it is no longer in flight, and the report says why in one word.
    let generation = driver
        .effects
        .iter()
        .rev()
        .find_map(|effect| match effect {
            lazy_client_core::Effect::UploadFile { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the file is uploading");
    driver.send(Input::UploadResult {
        attachment,
        generation,
        result: Err(TransferError::Disk(typed("/home/jules/secret-place"))),
    });
    let report = driver.core.diagnostics();
    assert_eq!(report.uploads_in_flight, 0);
    assert_eq!(
        codes(&driver.core).last(),
        Some(&("upload".to_owned(), "disk".to_owned()))
    );
    assert_quiet("the report", &report.report());
}

#[test]
fn only_the_last_ten_errors_are_kept_oldest_first() {
    let mut core = ClientCore::new();
    core.handle(Input::Started(StartupState::default()));
    let all = [
        ErrorCode::InvalidRequest,
        ErrorCode::Unauthenticated,
        ErrorCode::InvalidCredentials,
        ErrorCode::Forbidden,
        ErrorCode::NotFound,
        ErrorCode::Conflict,
        ErrorCode::StaleRevision,
        ErrorCode::UsernameTaken,
        ErrorCode::NameTaken,
        ErrorCode::InvitationInvalid,
        ErrorCode::PayloadTooLarge,
        ErrorCode::UnsupportedMedia,
    ];
    assert!(all.len() > MAX_DIAGNOSTIC_EVENTS);
    for code in all {
        core.handle(Input::AuthFailed(refused(code)));
    }
    let kept = codes(&core);
    assert_eq!(kept.len(), MAX_DIAGNOSTIC_EVENTS);
    let words: Vec<&str> = kept.iter().map(|(_, code)| code.as_str()).collect();
    assert_eq!(
        words,
        [
            "invalid_credentials",
            "forbidden",
            "not_found",
            "conflict",
            "stale_revision",
            "username_taken",
            "name_taken",
            "invitation_invalid",
            "payload_too_large",
            "unsupported_media",
        ]
    );
    assert!(kept.iter().all(|(category, _)| category == "auth"));
}

#[test]
fn an_unknown_outcome_and_a_realtime_error_are_told_by_code() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(Input::BootstrapFailed(RequestError::Unknown(typed(
        "detail",
    ))));
    driver.send(Input::Frame(ServerFrame::Error {
        code: ErrorCode::InvalidRequest,
        message: typed("frame message"),
    }));
    assert_eq!(
        codes(&driver.core),
        vec![
            ("bootstrap".to_owned(), "unknown_outcome".to_owned()),
            ("realtime".to_owned(), "invalid_request".to_owned()),
        ]
    );
}

#[test]
fn an_error_carries_the_time_the_core_was_last_told() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(Input::Socket(SocketEvent::Closed {
        workspace: world.workspace.id,
        attempt: 1,
    }));
    let before = driver.core.diagnostics().recent_errors;
    let now = Timestamp::now();
    driver.send(Input::Clock(now));
    driver.send(Input::Socket(SocketEvent::Closed {
        workspace: world.workspace.id,
        attempt: 2,
    }));
    let after = driver.core.diagnostics().recent_errors;
    assert_eq!(after.len(), 2);
    assert_eq!(after[0], before[0], "an earlier error keeps its time");
    assert_eq!(after[1].at, Some(now));
    assert!(
        after[1].to_string().contains("socket: closed"),
        "{}",
        after[1]
    );
}

#[test]
fn nothing_typed_or_said_by_a_server_reaches_the_report() {
    let world = World::new();
    let conversation = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(conversation, vec![], false);
    let sent = driver.send(Input::SendMessage {
        conversation,
        text: typed("secret message"),
    });
    assert!(!sent.effects.is_empty());
    driver.send(Input::SetDraft {
        conversation,
        text: typed("draft"),
    });
    driver.send(Input::SetSearchQuery {
        text: typed("search"),
    });
    driver.send(Input::RunSearch);
    // Every kind of failure, each with the secret in what it says.
    let operation = driver
        .effects
        .iter()
        .find_map(|effect| match effect {
            lazy_client_core::Effect::PersistSubmission(entry) => Some(entry.operation_id),
            _ => None,
        })
        .expect("the message was journaled");
    driver.send(Input::SendResult {
        operation,
        result: Err(RequestError::Unknown(typed("detail"))),
    });
    driver.send(Input::SendResult {
        operation,
        result: Err(refused(ErrorCode::Forbidden)),
    });
    driver.send(Input::BootstrapFailed(refused(ErrorCode::NotFound)));
    driver.send(Input::StorageProblem(typed("disk said")));
    driver.send(Input::Frame(ServerFrame::Error {
        code: ErrorCode::InvalidRequest,
        message: typed("frame"),
    }));
    let report = driver.core.diagnostics();
    assert!(!report.recent_errors.is_empty());
    assert_quiet("the report text", &report.report());
    assert_quiet("the report as Debug", &format!("{report:?}"));
    for event in &report.recent_errors {
        assert!(
            event
                .code
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_'),
            "a code is one word: {event}"
        );
    }
}
