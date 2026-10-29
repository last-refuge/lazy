//! Attachments (slice 3A, D17): composer files and their uploads, messages that carry files,
//! restarts with files in the journal, downloads, and image previews. The core never sees a
//! byte of a file; these tests play the runtime's part by answering the effects.

mod common;

use std::path::PathBuf;

use common::{Driver, World, seq};
use lazy_client_core::{
    AttachmentState, Change, ComposerAttachment, ComposerTarget, Delivery, DownloadState, Effect,
    FileBytes, FileSource, ImageData, ImageState, Input, JournalAttachment, JournalEntry,
    LocalFile, MAX_CONCURRENT_UPLOADS, Output, PendingFileState, RequestError, RetryKind,
    StagedFile, StartupState, TimelineItem, TransferError, guess_media_type,
};
use lazy_domain::{
    AttachmentId, ConversationId, MessageDocument, MessageId, OperationId, Timestamp,
};
use lazy_protocol::{
    ApiError, Attachment, AttachmentResponse, ErrorCode, Message, MessageResponse, OperationStatus,
    SendMessageResponse,
};

// ----- Helpers -----

struct Fx {
    world: World,
    driver: Driver,
    general: ConversationId,
    target: ComposerTarget,
}

fn setup() -> Fx {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let general = world.channel.id;
    driver.load_history(general, vec![], false);
    Fx {
        world,
        driver,
        general,
        target: ComposerTarget::Conversation(general),
    }
}

fn path_file(name: &str) -> LocalFile {
    LocalFile {
        source: FileSource::Path(PathBuf::from("/home/jules/Pictures").join(name)),
        name: name.to_owned(),
    }
}

fn bytes_file(name: &str, len: usize) -> LocalFile {
    LocalFile {
        source: FileSource::Bytes(FileBytes::new(vec![7; len])),
        name: name.to_owned(),
    }
}

fn server_attachment(id: AttachmentId, name: &str, size: u64) -> Attachment {
    Attachment {
        id,
        name: name.to_owned(),
        media_type: guess_media_type(name).to_owned(),
        size,
        sha256: "ab".repeat(32),
        width: None,
        height: None,
        has_thumbnail: false,
    }
}

fn rejected(code: ErrorCode) -> TransferError {
    TransferError::Request(RequestError::Rejected(ApiError::new(code, "no")))
}

fn lost() -> TransferError {
    TransferError::Request(RequestError::Unknown("timed out".into()))
}

fn chips(fx: &Fx) -> Vec<ComposerAttachment> {
    fx.driver.core.composer_attachments(fx.target).to_vec()
}

fn ids(fx: &Fx) -> Vec<AttachmentId> {
    chips(fx).iter().map(|chip| chip.id).collect()
}

/// Adds files (by name) to the conversation's composer and returns their ids.
fn attach(fx: &mut Fx, names: &[&str]) -> Vec<AttachmentId> {
    let before = ids(fx);
    fx.driver.send(Input::AttachFiles {
        target: fx.target,
        files: names.iter().map(|name| path_file(name)).collect(),
    });
    ids(fx)
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect()
}

/// The generation of the latest request to copy a file.
fn stage_generation(driver: &Driver, id: AttachmentId) -> u64 {
    driver
        .effects_matching(|effect| match effect {
            Effect::StageFile {
                attachment,
                generation,
                ..
            } if *attachment == id => Some(*generation),
            _ => None,
        })
        .pop()
        .expect("the file was to be copied")
}

fn staged(fx: &mut Fx, id: AttachmentId, size: u64) -> Output {
    fx.driver.send(Input::FileStaged {
        attachment: id,
        generation: stage_generation(&fx.driver, id),
        result: Ok(StagedFile { size }),
    })
}

/// The uploads an output starts, as (attachment, generation).
fn uploads(out: &Output) -> Vec<(AttachmentId, u64)> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::UploadFile {
                attachment,
                generation,
                ..
            } => Some((*attachment, *generation)),
            _ => None,
        })
        .collect()
}

fn generation_of(fx: &Fx, id: AttachmentId) -> u64 {
    fx.driver
        .effects_matching(|effect| match effect {
            Effect::UploadFile {
                attachment,
                generation,
                ..
            } if *attachment == id => Some(*generation),
            _ => None,
        })
        .pop()
        .expect("the file was uploaded")
}

fn uploaded(fx: &mut Fx, id: AttachmentId) -> Output {
    let generation = generation_of(fx, id);
    let size = chips(fx)
        .iter()
        .find(|chip| chip.id == id)
        .map_or(0, |chip| chip.size);
    fx.driver.send(Input::UploadResult {
        attachment: id,
        generation,
        result: Ok(AttachmentResponse {
            attachment: server_attachment(id, "x", size),
        }),
    })
}

fn upload_result(
    fx: &mut Fx,
    id: AttachmentId,
    result: Result<AttachmentResponse, TransferError>,
) -> Output {
    let generation = generation_of(fx, id);
    fx.driver.send(Input::UploadResult {
        attachment: id,
        generation,
        result,
    })
}

fn state_of(fx: &Fx, id: AttachmentId) -> AttachmentState {
    chips(fx)
        .iter()
        .find(|chip| chip.id == id)
        .map(|chip| chip.state.clone())
        .expect("the chip is in the composer")
}

fn stage_effects(out: &Output) -> Vec<AttachmentId> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::StageFile { attachment, .. } => Some(*attachment),
            _ => None,
        })
        .collect()
}

fn released(out: &Output) -> Vec<AttachmentId> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::ReleaseFiles { attachments } => Some(attachments.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn journaled(out: &Output) -> Option<JournalEntry> {
    out.effects.iter().find_map(|effect| match effect {
        Effect::PersistSubmission(entry) => Some(entry.clone()),
        _ => None,
    })
}

fn sent(out: &Output) -> Option<(OperationId, Vec<AttachmentId>, MessageDocument)> {
    out.effects.iter().find_map(|effect| match effect {
        Effect::SendMessage {
            operation,
            attachments,
            document,
            ..
        } => Some((*operation, attachments.clone(), document.clone())),
        _ => None,
    })
}

fn send_files(fx: &mut Fx, text: &str) -> Output {
    fx.driver.send(Input::SendMessage {
        conversation: fx.general,
        text: text.to_owned(),
    })
}

fn local_row(fx: &Fx, operation: OperationId) -> TimelineItem {
    fx.driver
        .timeline(fx.general)
        .iter()
        .find(|item| item.operation_id() == Some(operation))
        .cloned()
        .expect("the row is in the timeline")
}

/// The message the server commits for a send of `files` by the person.
fn committed(fx: &Fx, files: &[(AttachmentId, &str, u64)], text: &str, at: i64) -> Message {
    let mut message = fx.world.message(
        fx.general,
        &fx.world.me,
        at,
        if text.is_empty() { "x" } else { text },
    );
    if text.is_empty() {
        message.document = MessageDocument::empty();
    }
    message.attachments = files
        .iter()
        .map(|(id, name, size)| server_attachment(*id, name, *size))
        .collect();
    message
}

/// Sends a file-only message with one uploaded file; returns the operation and the file.
fn send_one_uploaded(fx: &mut Fx) -> (OperationId, AttachmentId) {
    let id = attach(fx, &["report.pdf"])[0];
    staged(fx, id, 1000);
    uploaded(fx, id);
    let out = send_files(fx, "");
    let entry = journaled(&out).expect("the send is journaled");
    fx.driver.send(Input::SubmissionPersisted {
        operation: entry.operation_id,
    });
    (entry.operation_id, id)
}

// ----- The composer -----

#[test]
fn a_chosen_file_is_a_chip_that_is_copied_then_uploaded() {
    let mut fx = setup();
    let out = fx.driver.send(Input::AttachFiles {
        target: fx.target,
        files: vec![path_file("holiday photo.png")],
    });
    let id = ids(&fx)[0];
    assert_eq!(stage_effects(&out), vec![id]);
    assert!(
        out.changes
            .contains(&Change::ComposerAttachmentsChanged { target: fx.target })
    );
    let chip = &chips(&fx)[0];
    assert_eq!(chip.name, "holiday photo.png");
    assert_eq!(chip.media_type_guess, "image/png");
    assert_eq!(chip.state, AttachmentState::Reading);
    assert_eq!(
        chip.source,
        Some(FileSource::Path(PathBuf::from(
            "/home/jules/Pictures/holiday photo.png"
        )))
    );

    // The copy is in: the upload starts at once.
    let out = staged(&mut fx, id, 2048);
    assert_eq!(uploads(&out).len(), 1);
    assert_eq!(
        state_of(&fx, id),
        AttachmentState::Uploading {
            sent: 0,
            total: 2048
        }
    );
    assert_eq!(chips(&fx)[0].size, 2048);

    let generation = generation_of(&fx, id);
    fx.driver.send(Input::UploadProgress {
        attachment: id,
        generation,
        sent: 1024,
        total: 2048,
    });
    assert_eq!(
        state_of(&fx, id),
        AttachmentState::Uploading {
            sent: 1024,
            total: 2048
        }
    );
    // Progress never goes back.
    fx.driver.send(Input::UploadProgress {
        attachment: id,
        generation,
        sent: 512,
        total: 2048,
    });
    assert_eq!(
        state_of(&fx, id),
        AttachmentState::Uploading {
            sent: 1024,
            total: 2048
        }
    );
    uploaded(&mut fx, id);
    assert_eq!(state_of(&fx, id), AttachmentState::Uploaded);
}

#[test]
fn a_name_is_made_safe_and_pasted_bytes_are_known_at_once() {
    let mut fx = setup();
    fx.driver.send(Input::AttachFiles {
        target: fx.target,
        files: vec![bytes_file("../../etc/passwd", 10), bytes_file("", 3)],
    });
    let chips = chips(&fx);
    assert_eq!(chips[0].name, "_.._etc_passwd");
    assert_eq!(chips[0].size, 10);
    assert_eq!(chips[1].name, "file");
    assert!(matches!(chips[0].source, Some(FileSource::Bytes(_))));
}

#[test]
fn bytes_over_the_limit_are_a_failed_chip_with_nothing_to_copy() {
    let mut fx = setup();
    let out = fx.driver.send(Input::AttachFiles {
        target: fx.target,
        files: vec![bytes_file("huge.bin", 26 * 1024 * 1024)],
    });
    assert!(stage_effects(&out).is_empty());
    let chip = &chips(&fx)[0];
    let AttachmentState::Failed { reason, can_retry } = &chip.state else {
        panic!("expected a failed chip");
    };
    assert_eq!(reason, "huge.bin is 26.0 MiB; files can be at most 25 MiB.");
    assert!(!can_retry);
    // The bytes are not kept for a file that cannot be sent.
    assert_eq!(chip.source, None);
}

#[test]
fn a_path_over_the_limit_is_reported_by_the_runtime_before_any_copy() {
    let mut fx = setup();
    let id = attach(&mut fx, &["movie.mov"])[0];
    fx.driver.send(Input::FileStaged {
        attachment: id,
        generation: stage_generation(&fx.driver, id),
        result: Err(TransferError::TooLarge {
            size: 31 * 1024 * 1024 + 200 * 1024,
        }),
    });
    assert_eq!(
        state_of(&fx, id),
        AttachmentState::Failed {
            reason: "movie.mov is 31.2 MiB; files can be at most 25 MiB.".to_owned(),
            can_retry: false
        }
    );
}

#[test]
fn a_file_that_cannot_be_read_says_why_and_cannot_be_retried() {
    let mut fx = setup();
    let id = attach(&mut fx, &["secret.txt"])[0];
    fx.driver.send(Input::FileStaged {
        attachment: id,
        generation: stage_generation(&fx.driver, id),
        result: Err(TransferError::Unreadable("Permission denied".into())),
    });
    assert_eq!(
        state_of(&fx, id),
        AttachmentState::Failed {
            reason: "secret.txt couldn't be read: Permission denied.".to_owned(),
            can_retry: false
        }
    );
    // Retrying it does nothing.
    let out = fx.driver.send(Input::RetryAttachment {
        target: fx.target,
        attachment: id,
    });
    assert!(out.effects.is_empty());
}

#[test]
fn only_ten_files_fit_in_a_composer() {
    let mut fx = setup();
    let names: Vec<String> = (0..12).map(|n| format!("f{n}.txt")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    fx.driver.send(Input::AttachFiles {
        target: fx.target,
        files: names.iter().map(|name| path_file(name)).collect(),
    });
    assert_eq!(chips(&fx).len(), 10);
    assert_eq!(
        fx.driver.core.notice(),
        Some("A message can have at most 10 files.")
    );
    // And no more can be added while it is full.
    let out = fx.driver.send(Input::AttachFiles {
        target: fx.target,
        files: vec![path_file("one more.txt")],
    });
    assert!(stage_effects(&out).is_empty());
    assert_eq!(chips(&fx).len(), 10);
}

#[test]
fn only_three_uploads_run_at_once() {
    let mut fx = setup();
    let all = attach(&mut fx, &["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);
    let mut started = 0;
    for id in &all {
        started += uploads(&staged(&mut fx, *id, 100)).len();
    }
    assert_eq!(started, MAX_CONCURRENT_UPLOADS);
    assert_eq!(
        state_of(&fx, all[3]),
        AttachmentState::Uploading {
            sent: 0,
            total: 100
        }
    );
    // A slot frees up: the next in line starts, in order.
    let out = uploaded(&mut fx, all[0]);
    assert_eq!(
        uploads(&out).iter().map(|u| u.0).collect::<Vec<_>>(),
        vec![all[3]]
    );
    let out = uploaded(&mut fx, all[1]);
    assert_eq!(
        uploads(&out).iter().map(|u| u.0).collect::<Vec<_>>(),
        vec![all[4]]
    );
    assert_eq!(
        fx.driver
            .effects_matching(|e| matches!(e, Effect::UploadFile { .. }).then_some(()))
            .len(),
        5
    );
}

#[test]
fn an_upload_of_unknown_outcome_is_repeated_with_the_same_id_then_given_up() {
    let mut fx = setup();
    let id = attach(&mut fx, &["a.txt"])[0];
    staged(&mut fx, id, 100);
    let first = generation_of(&fx, id);
    let out = upload_result(&mut fx, id, Err(lost()));
    let second = uploads(&out);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].0, id);
    assert!(second[0].1 > first);
    let out = upload_result(&mut fx, id, Err(lost()));
    assert_eq!(uploads(&out).len(), 1);
    // The third failure is the last.
    let out = upload_result(&mut fx, id, Err(lost()));
    assert!(uploads(&out).is_empty());
    assert_eq!(
        state_of(&fx, id),
        AttachmentState::Failed {
            reason: "Couldn't upload a.txt. Check your connection.".to_owned(),
            can_retry: true
        }
    );
    // Retrying starts over.
    let out = fx.driver.send(Input::RetryAttachment {
        target: fx.target,
        attachment: id,
    });
    assert_eq!(uploads(&out).len(), 1);
    uploaded(&mut fx, id);
    assert_eq!(state_of(&fx, id), AttachmentState::Uploaded);
}

#[test]
fn the_server_refusing_a_file_says_why() {
    let cases = [
        (
            ErrorCode::UnsupportedMedia,
            "pic.png isn't a file Lazy can accept (images must be PNG, JPEG, GIF or WebP that can be decoded).",
            false,
        ),
        (
            ErrorCode::PayloadTooLarge,
            "pic.png is larger than 25 MiB.",
            false,
        ),
        (
            ErrorCode::LimitReached,
            "You have too many uploads waiting. Send or remove some files.",
            true,
        ),
        (
            ErrorCode::Forbidden,
            "You can't attach files in this conversation.",
            false,
        ),
    ];
    for (code, text, can_retry) in cases {
        let mut fx = setup();
        let id = attach(&mut fx, &["pic.png"])[0];
        staged(&mut fx, id, 100);
        let out = upload_result(&mut fx, id, Err(rejected(code)));
        // A refusal is not repeated.
        assert!(uploads(&out).is_empty(), "{code:?}");
        assert_eq!(
            state_of(&fx, id),
            AttachmentState::Failed {
                reason: text.to_owned(),
                can_retry
            },
            "{code:?}"
        );
    }
}

#[test]
fn removing_a_chip_stops_its_upload_and_deletes_its_copy() {
    let mut fx = setup();
    let all = attach(&mut fx, &["a.txt", "b.txt", "c.txt", "d.txt"]);
    for id in &all {
        staged(&mut fx, *id, 100);
    }
    let generation = generation_of(&fx, all[0]);
    let out = fx.driver.send(Input::RemoveAttachment {
        target: fx.target,
        attachment: all[0],
    });
    assert!(
        out.effects
            .contains(&Effect::CancelUpload { attachment: all[0] })
    );
    assert_eq!(released(&out), vec![all[0]]);
    // The freed slot goes to the fourth file.
    assert_eq!(
        uploads(&out).iter().map(|u| u.0).collect::<Vec<_>>(),
        vec![all[3]]
    );
    assert_eq!(ids(&fx), vec![all[1], all[2], all[3]]);
    // A late answer for the removed file is ignored.
    let out = fx.driver.send(Input::UploadResult {
        attachment: all[0],
        generation,
        result: Ok(AttachmentResponse {
            attachment: server_attachment(all[0], "a.txt", 100),
        }),
    });
    assert!(out.is_empty());
}

#[test]
fn cancelling_keeps_a_failed_chip_that_can_be_retried_and_ignores_the_old_answer() {
    let mut fx = setup();
    let id = attach(&mut fx, &["a.txt"])[0];
    staged(&mut fx, id, 100);
    let old = generation_of(&fx, id);
    let out = fx.driver.send(Input::CancelAttachment {
        target: fx.target,
        attachment: id,
    });
    assert!(
        out.effects
            .contains(&Effect::CancelUpload { attachment: id })
    );
    assert_eq!(
        state_of(&fx, id),
        AttachmentState::Failed {
            reason: "Cancelled".to_owned(),
            can_retry: true
        }
    );
    let out = fx.driver.send(Input::RetryAttachment {
        target: fx.target,
        attachment: id,
    });
    let again = uploads(&out);
    assert_eq!(again.len(), 1);
    assert!(again[0].1 > old);
    // The cancelled transfer's answer arrives late and must not count.
    fx.driver.send(Input::UploadResult {
        attachment: id,
        generation: old,
        result: Ok(AttachmentResponse {
            attachment: server_attachment(id, "a.txt", 100),
        }),
    });
    assert_eq!(
        state_of(&fx, id),
        AttachmentState::Uploading {
            sent: 0,
            total: 100
        }
    );
    // Neither does its progress.
    fx.driver.send(Input::UploadProgress {
        attachment: id,
        generation: old,
        sent: 90,
        total: 100,
    });
    assert_eq!(
        state_of(&fx, id),
        AttachmentState::Uploading {
            sent: 0,
            total: 100
        }
    );
}

#[test]
fn cancelling_while_the_copy_is_made_can_be_retried_from_the_source() {
    let mut fx = setup();
    let id = attach(&mut fx, &["a.txt"])[0];
    let out = fx.driver.send(Input::CancelAttachment {
        target: fx.target,
        attachment: id,
    });
    assert_eq!(released(&out), vec![id]);
    // The copy finishing after the cancel changes nothing.
    staged(&mut fx, id, 100);
    assert!(matches!(state_of(&fx, id), AttachmentState::Failed { .. }));
    let out = fx.driver.send(Input::RetryAttachment {
        target: fx.target,
        attachment: id,
    });
    assert_eq!(stage_effects(&out), vec![id]);
    assert_eq!(state_of(&fx, id), AttachmentState::Reading);
}

#[test]
fn a_copy_that_was_cancelled_and_started_again_is_not_mistaken_for_the_new_one() {
    let mut fx = setup();
    let id = attach(&mut fx, &["a.txt"])[0];
    let old = stage_generation(&fx.driver, id);
    fx.driver.send(Input::CancelAttachment {
        target: fx.target,
        attachment: id,
    });
    let out = fx.driver.send(Input::RetryAttachment {
        target: fx.target,
        attachment: id,
    });
    assert_eq!(stage_effects(&out), vec![id]);
    let new = stage_generation(&fx.driver, id);
    assert!(new > old);
    // The first copy's answer arrives late: it is not the copy that is being made.
    let out = fx.driver.send(Input::FileStaged {
        attachment: id,
        generation: old,
        result: Ok(StagedFile { size: 5 }),
    });
    assert!(out.is_empty());
    assert_eq!(state_of(&fx, id), AttachmentState::Reading);
    let out = fx.driver.send(Input::FileStaged {
        attachment: id,
        generation: new,
        result: Ok(StagedFile { size: 100 }),
    });
    assert_eq!(uploads(&out).len(), 1);
}

#[test]
fn a_thread_reply_box_and_a_conversation_have_their_own_files() {
    let mut fx = setup();
    let root = fx.world.message(fx.general, &fx.world.other, 5, "root");
    fx.driver.send(fx.world.event(root.clone(), None));
    fx.driver.send(Input::OpenThread {
        conversation: fx.general,
        root: root.id,
    });
    let thread = ComposerTarget::Thread(root.id);
    fx.driver.send(Input::AttachFiles {
        target: thread,
        files: vec![path_file("t.txt")],
    });
    attach(&mut fx, &["c.txt"]);
    assert_eq!(fx.driver.core.composer_attachments(thread).len(), 1);
    assert_eq!(fx.driver.core.composer_attachments(thread)[0].name, "t.txt");
    assert_eq!(chips(&fx).len(), 1);
    assert_eq!(chips(&fx)[0].name, "c.txt");
}

#[test]
fn files_cannot_be_added_to_an_archived_conversation() {
    let mut world = World::new();
    world.channel.is_default = false;
    world.second_channel.archived = true;
    let second = world.second_channel.id;
    let mut driver = Driver::ready(&world);
    let out = driver.send(Input::AttachFiles {
        target: ComposerTarget::Conversation(second),
        files: vec![path_file("a.txt")],
    });
    assert!(stage_effects(&out).is_empty());
    assert!(driver.core.notice().is_some());
}

// ----- Sending -----

#[test]
fn a_message_of_only_files_is_sent_once_they_are_uploaded() {
    let mut fx = setup();
    let id = attach(&mut fx, &["report.pdf"])[0];
    staged(&mut fx, id, 1000);
    uploaded(&mut fx, id);
    let out = send_files(&mut fx, "");
    // The files leave the composer; the row is pending with its file.
    assert!(chips(&fx).is_empty());
    let entry = journaled(&out).expect("journaled before anything is sent");
    assert_eq!(
        entry.attachments,
        vec![JournalAttachment {
            id,
            name: "report.pdf".to_owned(),
            size: 1000
        }]
    );
    assert!(entry.document.has_no_content());
    assert!(sent(&out).is_none());
    let row = local_row(&fx, entry.operation_id);
    assert_eq!(row.delivery, Delivery::Pending);
    assert!(row.document.has_no_content());
    assert_eq!(row.attachments.len(), 1);
    assert_eq!(row.attachments[0].id, id);
    assert_eq!(row.attachments[0].size, 1000);
    assert_eq!(row.attachments[0].sha256, "");
    let progress = row.upload.expect("the row shows its files");
    assert_eq!(progress.total_files(), 1);
    assert_eq!(progress.uploaded_files(), 1);

    let out = fx.driver.send(Input::SubmissionPersisted {
        operation: entry.operation_id,
    });
    let (operation, attachments, document) = sent(&out).expect("everything is uploaded: send");
    assert_eq!(operation, entry.operation_id);
    assert_eq!(attachments, vec![id]);
    assert!(document.has_no_content());
}

#[test]
fn sending_nothing_is_still_ignored() {
    let mut fx = setup();
    let out = send_files(&mut fx, "   ");
    assert!(out.is_empty());
}

#[test]
fn a_failed_chip_blocks_the_send_and_keeps_the_composer() {
    let mut fx = setup();
    let id = attach(&mut fx, &["bad.png"])[0];
    staged(&mut fx, id, 100);
    upload_result(&mut fx, id, Err(rejected(ErrorCode::UnsupportedMedia)));
    fx.driver.send(Input::SetDraft {
        conversation: fx.general,
        text: "look".to_owned(),
    });
    let out = send_files(&mut fx, "look");
    assert!(journaled(&out).is_none());
    assert_eq!(
        fx.driver.core.notice(),
        Some("Remove or retry bad.png first.")
    );
    assert_eq!(chips(&fx).len(), 1);
    assert_eq!(fx.driver.core.draft(fx.general), "look");
    // Removing it unblocks the send.
    fx.driver.send(Input::RemoveAttachment {
        target: fx.target,
        attachment: id,
    });
    let out = send_files(&mut fx, "look");
    assert!(journaled(&out).is_some());
}

#[test]
fn the_journal_waits_for_the_copy_and_the_send_for_the_upload() {
    let mut fx = setup();
    let id = attach(&mut fx, &["a.txt"])[0];
    // Sent while the file is still being copied.
    let out = send_files(&mut fx, "see attached");
    assert!(
        journaled(&out).is_none(),
        "the entry must not name a file that is not there"
    );
    let operation = fx
        .driver
        .timeline(fx.general)
        .iter()
        .find_map(TimelineItem::operation_id)
        .expect("a row");
    let row = local_row(&fx, operation);
    assert_eq!(
        row.upload.as_ref().unwrap().files[0].state,
        PendingFileState::Reading
    );
    let out = staged(&mut fx, id, 640);
    let entry = journaled(&out).expect("journaled once the copy is in");
    assert_eq!(entry.attachments[0].size, 640);
    // The upload began as soon as the copy was in.
    assert_eq!(uploads(&out).len(), 1);
    let out = fx.driver.send(Input::SubmissionPersisted { operation });
    assert!(sent(&out).is_none(), "the file is not on the server yet");

    let generation = generation_of(&fx, id);
    fx.driver.send(Input::UploadProgress {
        attachment: id,
        generation,
        sent: 320,
        total: 640,
    });
    let row = local_row(&fx, operation);
    assert_eq!(
        row.upload.as_ref().unwrap().files[0].state,
        PendingFileState::Uploading {
            sent: 320,
            total: 640
        }
    );
    let out = fx.driver.send(Input::UploadResult {
        attachment: id,
        generation,
        result: Ok(AttachmentResponse {
            attachment: server_attachment(id, "a.txt", 640),
        }),
    });
    let (sent_operation, attachments, document) = sent(&out).expect("now it goes");
    assert_eq!(sent_operation, operation);
    assert_eq!(attachments, vec![id]);
    assert!(!document.has_no_content());
}

#[test]
fn messages_stay_in_order_behind_one_that_is_still_uploading() {
    let mut fx = setup();
    let id = attach(&mut fx, &["big.zip"])[0];
    staged(&mut fx, id, 5000);
    let first = journaled(&send_files(&mut fx, "files"))
        .unwrap()
        .operation_id;
    fx.driver
        .send(Input::SubmissionPersisted { operation: first });
    let second_out = send_files(&mut fx, "and then text");
    let second = journaled(&second_out).unwrap().operation_id;
    let out = fx
        .driver
        .send(Input::SubmissionPersisted { operation: second });
    assert!(sent(&out).is_none(), "the later message waits");
    let out = uploaded(&mut fx, id);
    assert_eq!(sent(&out).unwrap().0, first);
}

#[test]
fn the_confirmed_message_carries_the_servers_files_and_the_copies_are_released() {
    let mut fx = setup();
    let (operation, id) = send_one_uploaded(&mut fx);
    let message = committed(&fx, &[(id, "report.pdf", 1000)], "", 11);
    let out = fx.driver.send(Input::SendResult {
        operation,
        result: Ok(SendMessageResponse {
            message: message.clone(),
        }),
    });
    assert_eq!(released(&out), vec![id]);
    assert!(
        out.effects
            .contains(&Effect::SettleSubmission { operation })
    );
    let rows = fx.driver.timeline(fx.general);
    let row = rows
        .iter()
        .find(|item| item.message_id() == Some(message.id))
        .unwrap();
    assert!(row.delivery != Delivery::Pending);
    assert_eq!(row.upload, None);
    assert_eq!(row.attachments, message.attachments);
    assert!(row.document.has_no_content());
}

#[test]
fn a_refused_upload_fails_the_row_and_a_retry_uploads_the_same_files_again() {
    let mut fx = setup();
    let id = attach(&mut fx, &["pic.png"])[0];
    staged(&mut fx, id, 100);
    let operation = journaled(&send_files(&mut fx, "")).unwrap().operation_id;
    fx.driver.send(Input::SubmissionPersisted { operation });
    let out = upload_result(&mut fx, id, Err(rejected(ErrorCode::UnsupportedMedia)));
    let row = local_row(&fx, operation);
    let Delivery::Failed { reason, retry } = &row.delivery else {
        panic!("the row should have failed");
    };
    assert!(reason.starts_with("pic.png isn't a file Lazy can accept"));
    assert_eq!(*retry, RetryKind::NewOperation);
    // The journal entry is released (nothing was sent) but the copy is kept for a retry.
    assert!(
        out.effects
            .contains(&Effect::SettleSubmission { operation })
    );
    assert!(released(&out).is_empty());

    let out = fx.driver.send(Input::RetryOperation {
        conversation: fx.general,
        operation,
    });
    let entry = journaled(&out).expect("a new operation is journaled");
    assert_ne!(entry.operation_id, operation);
    assert_eq!(entry.attachments.len(), 1);
    assert_eq!(entry.attachments[0].id, id);
    let out = fx.driver.send(Input::SubmissionPersisted {
        operation: entry.operation_id,
    });
    // The server may have lost it: it goes up again before the message is sent.
    assert_eq!(
        uploads(&out).iter().map(|u| u.0).collect::<Vec<_>>(),
        vec![id]
    );
}

#[test]
fn a_retry_uploads_every_file_again_not_only_the_one_that_was_refused() {
    let mut fx = setup();
    let ok = attach(&mut fx, &["good.txt"])[0];
    let bad = attach(&mut fx, &["bad.png"])[0];
    staged(&mut fx, ok, 10);
    staged(&mut fx, bad, 10);
    uploaded(&mut fx, ok);
    let operation = journaled(&send_files(&mut fx, "")).unwrap().operation_id;
    fx.driver.send(Input::SubmissionPersisted { operation });
    upload_result(&mut fx, bad, Err(rejected(ErrorCode::UnsupportedMedia)));
    assert!(matches!(
        local_row(&fx, operation).delivery,
        Delivery::Failed { .. }
    ));

    let out = fx.driver.send(Input::RetryOperation {
        conversation: fx.general,
        operation,
    });
    let again = journaled(&out).expect("a new operation");
    let out = fx.driver.send(Input::SubmissionPersisted {
        operation: again.operation_id,
    });
    // The server may have forgotten the good one too: both go up again, none is skipped.
    let mut started: Vec<AttachmentId> = uploads(&out).iter().map(|u| u.0).collect();
    started.sort();
    let mut expected = vec![ok, bad];
    expected.sort();
    assert_eq!(started, expected);
}

#[test]
fn a_retry_copies_again_a_file_whose_copy_failed() {
    let mut fx = setup();
    let id = attach(&mut fx, &["flaky.bin"])[0];
    // Sent while the copy is made; the copy then fails, so nothing was ever journaled.
    let out = send_files(&mut fx, "see");
    assert!(journaled(&out).is_none());
    fx.driver.send(Input::FileStaged {
        attachment: id,
        generation: stage_generation(&fx.driver, id),
        result: Err(TransferError::Unreadable("Permission denied".into())),
    });
    let operation = fx
        .driver
        .timeline(fx.general)
        .iter()
        .find_map(TimelineItem::operation_id)
        .expect("a row");
    assert!(matches!(
        local_row(&fx, operation).delivery,
        Delivery::Failed { .. }
    ));
    // Retrying asks for the copy again from where it came from, and only then journals.
    let out = fx.driver.send(Input::RetryOperation {
        conversation: fx.general,
        operation,
    });
    assert_eq!(stage_effects(&out), vec![id]);
    assert!(journaled(&out).is_none());
    let out = staged(&mut fx, id, 50);
    assert!(journaled(&out).is_some());
}

#[test]
fn a_reply_to_a_thread_that_is_gone_gives_up_its_files() {
    let mut fx = setup();
    let root = fx.world.message(fx.general, &fx.world.other, 5, "root");
    fx.driver.send(fx.world.event(root.clone(), None));
    fx.driver.send(Input::OpenThread {
        conversation: fx.general,
        root: root.id,
    });
    let thread = ComposerTarget::Thread(root.id);
    fx.driver.send(Input::AttachFiles {
        target: thread,
        files: vec![path_file("t.txt")],
    });
    let id = fx.driver.core.composer_attachments(thread)[0].id;
    fx.driver.send(Input::FileStaged {
        attachment: id,
        generation: stage_generation(&fx.driver, id),
        result: Ok(StagedFile { size: 5 }),
    });
    let out = fx.driver.send(Input::SendReply {
        conversation: fx.general,
        root: root.id,
        text: String::new(),
        also_in_channel: false,
    });
    let operation = journaled(&out).unwrap().operation_id;
    fx.driver.send(Input::SubmissionPersisted { operation });
    let generation = generation_of(&fx, id);
    fx.driver.send(Input::UploadResult {
        attachment: id,
        generation,
        result: Ok(AttachmentResponse {
            attachment: server_attachment(id, "t.txt", 5),
        }),
    });
    // The root was deleted meanwhile: the server says the thread is not there.
    let out = fx.driver.send(Input::SendResult {
        operation,
        result: Err(RequestError::Rejected(ApiError::new(
            ErrorCode::NotFound,
            "no such thread",
        ))),
    });
    assert_eq!(released(&out), vec![id]);
    // Retrying a message of only files whose files are gone sends nothing.
    let out = fx.driver.send(Input::RetryOperation {
        conversation: fx.general,
        operation,
    });
    assert!(journaled(&out).is_none());
    assert!(sent(&out).is_none());
}

#[test]
fn when_a_thread_ends_the_files_in_its_reply_box_and_of_its_failed_replies_go() {
    let mut fx = setup();
    let root = fx.world.message(fx.general, &fx.world.other, 11, "root");
    fx.driver.send(fx.world.event(root.clone(), None));
    fx.driver.send(Input::OpenThread {
        conversation: fx.general,
        root: root.id,
    });
    let thread = ComposerTarget::Thread(root.id);
    // One reply fails on its file and waits for a retry; another file waits in the box.
    fx.driver.send(Input::AttachFiles {
        target: thread,
        files: vec![path_file("sent.png")],
    });
    let sent_file = fx.driver.core.composer_attachments(thread)[0].id;
    fx.driver.send(Input::FileStaged {
        attachment: sent_file,
        generation: stage_generation(&fx.driver, sent_file),
        result: Ok(StagedFile { size: 5 }),
    });
    let out = fx.driver.send(Input::SendReply {
        conversation: fx.general,
        root: root.id,
        text: String::new(),
        also_in_channel: false,
    });
    let operation = journaled(&out).unwrap().operation_id;
    fx.driver.send(Input::SubmissionPersisted { operation });
    let generation = generation_of(&fx, sent_file);
    fx.driver.send(Input::UploadResult {
        attachment: sent_file,
        generation,
        result: Err(rejected(ErrorCode::UnsupportedMedia)),
    });
    fx.driver.send(Input::AttachFiles {
        target: thread,
        files: vec![path_file("waiting.txt")],
    });
    let waiting = fx.driver.core.composer_attachments(thread)[0].id;
    fx.driver.send(Input::CloseThread);

    // The root is deleted and nothing is left of the thread.
    let out = fx
        .driver
        .send(fx.world.deleted_event(12, &fx.world.tombstone(&root)));
    let mut gone = released(&out);
    gone.sort();
    let mut expected = vec![sent_file, waiting];
    expected.sort();
    assert_eq!(gone, expected);
    assert!(fx.driver.core.composer_attachments(thread).is_empty());
}

#[test]
fn a_reply_that_fails_after_its_thread_ended_keeps_no_copies() {
    let mut fx = setup();
    let root = fx.world.message(fx.general, &fx.world.other, 11, "root");
    fx.driver.send(fx.world.event(root.clone(), None));
    fx.driver.send(Input::OpenThread {
        conversation: fx.general,
        root: root.id,
    });
    let thread = ComposerTarget::Thread(root.id);
    fx.driver.send(Input::AttachFiles {
        target: thread,
        files: vec![path_file("late.txt")],
    });
    let id = fx.driver.core.composer_attachments(thread)[0].id;
    // The reply is sent while the file is still being copied...
    fx.driver.send(Input::SendReply {
        conversation: fx.general,
        root: root.id,
        text: "see".to_owned(),
        also_in_channel: false,
    });
    // ...the root is deleted, and then the copy fails.
    fx.driver
        .send(fx.world.deleted_event(12, &fx.world.tombstone(&root)));
    let out = fx.driver.send(Input::FileStaged {
        attachment: id,
        generation: stage_generation(&fx.driver, id),
        result: Err(TransferError::Unreadable("Permission denied".into())),
    });
    assert_eq!(released(&out), vec![id]);
}

#[test]
fn discarding_a_failed_message_deletes_its_copies() {
    let mut fx = setup();
    let id = attach(&mut fx, &["pic.png"])[0];
    staged(&mut fx, id, 100);
    let operation = journaled(&send_files(&mut fx, "")).unwrap().operation_id;
    fx.driver.send(Input::SubmissionPersisted { operation });
    upload_result(&mut fx, id, Err(rejected(ErrorCode::UnsupportedMedia)));

    // A row still in flight cannot be discarded.
    let out = fx.driver.send(Input::DiscardOperation {
        conversation: fx.general,
        operation,
    });
    assert_eq!(released(&out), vec![id]);
    assert!(fx.driver.timeline(fx.general).is_empty());
    // And again is harmless.
    let out = fx.driver.send(Input::DiscardOperation {
        conversation: fx.general,
        operation,
    });
    assert!(out.is_empty());
}

#[test]
fn only_a_failed_row_can_be_discarded() {
    let mut fx = setup();
    let (operation, id) = send_one_uploaded(&mut fx);
    let out = fx.driver.send(Input::DiscardOperation {
        conversation: fx.general,
        operation,
    });
    assert!(out.is_empty());
    assert_eq!(fx.driver.timeline(fx.general).len(), 1);
    let _ = id;
}

#[test]
fn a_send_of_unknown_outcome_is_settled_without_uploading_again() {
    let mut fx = setup();
    let (operation, id) = send_one_uploaded(&mut fx);
    fx.driver.send(Input::SendResult {
        operation,
        result: Err(RequestError::Unknown("timed out".into())),
    });
    let out = fx.driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    // The file is known to be on the server; only the message goes again.
    assert!(uploads(&out).is_empty());
    let (resent, attachments, _) = sent(&out).expect("resent");
    assert_eq!(resent, operation);
    assert_eq!(attachments, vec![id]);
}

#[test]
fn a_session_that_ends_stops_the_uploads_and_the_message_can_be_tried_again() {
    let mut fx = setup();
    let id = attach(&mut fx, &["a.txt"])[0];
    staged(&mut fx, id, 100);
    let operation = journaled(&send_files(&mut fx, "")).unwrap().operation_id;
    fx.driver.send(Input::SubmissionPersisted { operation });
    let out = fx.driver.send(Input::SessionExpired);
    assert!(
        out.effects
            .contains(&Effect::CancelUpload { attachment: id })
    );
    let row = local_row(&fx, operation);
    assert!(matches!(
        row.delivery,
        Delivery::Failed {
            retry: RetryKind::SameOperation,
            ..
        }
    ));
    // The answer of the stopped transfer is ignored.
    let before = fx.driver.core.timeline(fx.general).to_vec();
    upload_result(&mut fx, id, Err(lost()));
    assert_eq!(fx.driver.core.timeline(fx.general), before.as_slice());
}

#[test]
fn leaving_a_conversation_releases_its_files() {
    let mut fx = setup();
    // One file goes with a message that is still being sent, another waits in the composer.
    let sent_file = attach(&mut fx, &["b.txt"])[0];
    staged(&mut fx, sent_file, 10);
    let out = send_files(&mut fx, "");
    assert!(journaled(&out).is_some());
    let composer = attach(&mut fx, &["a.txt"])[0];
    let out = fx.driver.send(fx.world_removed_event());
    let mut all = released(&out);
    all.sort();
    let mut expected = vec![composer, sent_file];
    expected.sort();
    assert_eq!(all, expected);
    assert!(chips(&fx).is_empty());
}

impl Fx {
    /// The person is removed from the conversation.
    fn world_removed_event(&self) -> Input {
        self.world.event_at(
            12,
            lazy_protocol::Event::MemberRemoved {
                conversation_id: self.general,
                user_id: self.world.me.id,
            },
        )
    }
}

#[test]
fn a_reply_of_only_files_goes_to_the_thread() {
    let mut fx = setup();
    let root = fx.world.message(fx.general, &fx.world.other, 5, "root");
    fx.driver.send(fx.world.event(root.clone(), None));
    fx.driver.send(Input::OpenThread {
        conversation: fx.general,
        root: root.id,
    });
    let thread = ComposerTarget::Thread(root.id);
    fx.driver.send(Input::AttachFiles {
        target: thread,
        files: vec![path_file("t.txt")],
    });
    let id = fx.driver.core.composer_attachments(thread)[0].id;
    fx.driver.send(Input::FileStaged {
        attachment: id,
        generation: stage_generation(&fx.driver, id),
        result: Ok(StagedFile { size: 5 }),
    });
    let out = fx.driver.send(Input::SendReply {
        conversation: fx.general,
        root: root.id,
        text: String::new(),
        also_in_channel: false,
    });
    let entry = journaled(&out).expect("journaled");
    assert_eq!(entry.thread_root, Some(root.id));
    assert_eq!(entry.attachments[0].id, id);
    assert!(fx.driver.core.composer_attachments(thread).is_empty());
    let view = fx.driver.core.thread(root.id).unwrap();
    assert_eq!(view.replies.len(), 1);
    assert_eq!(view.replies[0].attachments[0].name, "t.txt");
}

// ----- Restarts -----

fn restarted_with(entry: JournalEntry, world: &World) -> Driver {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        journal: vec![entry],
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    driver
}

fn journal_with_files(world: &World, files: Vec<JournalAttachment>) -> JournalEntry {
    JournalEntry {
        operation_id: OperationId::generate(),
        user_id: world.me.id,
        conversation_id: world.channel.id,
        document: MessageDocument::empty(),
        submitted_at: Timestamp::now(),
        kind: Default::default(),
        thread_root: None,
        also_in_channel: false,
        attachments: files,
    }
}

#[test]
fn a_restart_asks_about_the_send_then_uploads_what_the_server_may_lack_and_sends() {
    let world = World::new();
    let file = JournalAttachment {
        id: AttachmentId::generate(),
        name: "scan.pdf".to_owned(),
        size: 4096,
    };
    let entry = journal_with_files(&world, vec![file.clone()]);
    let operation = entry.operation_id;
    let mut driver = restarted_with(entry, &world);
    // The row is back, with its file, and the server is asked first.
    let row = driver
        .timeline(world.channel.id)
        .iter()
        .find(|item| item.operation_id() == Some(operation))
        .cloned()
        .unwrap();
    assert_eq!(row.delivery, Delivery::UnknownOutcome);
    assert_eq!(row.attachments[0].name, "scan.pdf");
    assert_eq!(row.attachments[0].size, 4096);
    assert!(
        driver
            .effects
            .contains(&Effect::QueryOperationStatus { operation })
    );
    assert!(
        driver
            .effects
            .iter()
            .all(|effect| !matches!(effect, Effect::UploadFile { .. }))
    );

    // No record of it: the file goes up (idempotent), then the message.
    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    let started = uploads(&out);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].0, file.id);
    assert!(sent(&out).is_none());
    let out = driver.send(Input::UploadResult {
        attachment: file.id,
        generation: started[0].1,
        result: Ok(AttachmentResponse {
            attachment: server_attachment(file.id, "scan.pdf", 4096),
        }),
    });
    let (resent, attachments, _) = sent(&out).expect("sent after the upload");
    assert_eq!(resent, operation);
    assert_eq!(attachments, vec![file.id]);
}

#[test]
fn a_restart_that_finds_the_send_committed_releases_the_copies() {
    let world = World::new();
    let file = JournalAttachment {
        id: AttachmentId::generate(),
        name: "scan.pdf".to_owned(),
        size: 4096,
    };
    let entry = journal_with_files(&world, vec![file.clone()]);
    let operation = entry.operation_id;
    let mut driver = restarted_with(entry, &world);
    let mut message = world.message(world.channel.id, &world.me, 12, "x");
    message.document = MessageDocument::empty();
    message.attachments = vec![server_attachment(file.id, "scan.pdf", 4096)];
    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::Committed { message }),
    });
    assert_eq!(released(&out), vec![file.id]);
    assert!(uploads(&out).is_empty());
}

#[test]
fn a_copy_that_is_gone_after_a_restart_fails_the_row_with_a_clear_reason() {
    let world = World::new();
    let file = JournalAttachment {
        id: AttachmentId::generate(),
        name: "scan.pdf".to_owned(),
        size: 4096,
    };
    let entry = journal_with_files(&world, vec![file.clone()]);
    let operation = entry.operation_id;
    let mut driver = restarted_with(entry, &world);
    let out = driver.send(Input::OperationStatusResult {
        operation,
        result: Ok(OperationStatus::NotFound),
    });
    let generation = uploads(&out)[0].1;
    driver.send(Input::UploadResult {
        attachment: file.id,
        generation,
        result: Err(TransferError::Missing),
    });
    let row = driver
        .timeline(world.channel.id)
        .iter()
        .find(|item| item.operation_id() == Some(operation))
        .cloned()
        .unwrap();
    let Delivery::Failed { reason, .. } = row.delivery else {
        panic!("the row should have failed");
    };
    assert_eq!(
        reason,
        "A file for this message is no longer available on this computer: scan.pdf."
    );
}

#[test]
fn a_journal_from_before_attachments_loads_as_a_send_without_files() {
    let world = World::new();
    let entry = journal_with_files(&world, vec![]);
    let mut json = serde_json::to_value(&entry).unwrap();
    // The previous shape has no `attachments` at all, and none is written for a send without.
    assert!(json.get("attachments").is_none());
    json.as_object_mut().unwrap().remove("attachments");
    let back: JournalEntry = serde_json::from_value(json).unwrap();
    assert!(back.attachments.is_empty());
    // With files, the ids are written and read back.
    let with = journal_with_files(
        &world,
        vec![JournalAttachment {
            id: AttachmentId::generate(),
            name: "a".into(),
            size: 1,
        }],
    );
    let text = serde_json::to_string(&with).unwrap();
    assert_eq!(serde_json::from_str::<JournalEntry>(&text).unwrap(), with);
}

// ----- Messages that arrive with files -----

#[test]
fn history_events_edits_and_deletions_carry_files() {
    let mut fx = setup();
    let id = AttachmentId::generate();
    let mut message = fx.world.message(fx.general, &fx.world.other, 11, "look");
    message.attachments = vec![server_attachment(id, "photo.png", 900)];
    fx.driver.send(fx.world.event(message.clone(), None));
    let row = fx.driver.timeline(fx.general)[0].clone();
    assert_eq!(row.attachments, message.attachments);

    // An edit keeps them, even down to no words at all.
    let mut edited = fx.world.edited(&message, "look again");
    edited.attachments = message.attachments.clone();
    fx.driver.send(fx.world.edited_event(12, &edited, None));
    let row = fx.driver.timeline(fx.general)[0].clone();
    assert_eq!(row.attachments, message.attachments);

    // A deleted message's files go with it.
    let tombstone = fx.world.tombstone(&edited);
    let out = fx.driver.send(fx.world.deleted_event(13, &tombstone));
    assert!(fx.driver.timeline(fx.general).is_empty());
    assert!(out.effects.contains(&Effect::ForgetAttachments {
        attachments: vec![id]
    }));
}

#[test]
fn a_message_of_only_files_reads_safely_everywhere_text_is_read() {
    let mut fx = setup();
    let mut message = fx.world.message(fx.general, &fx.world.other, 11, "x");
    message.document = MessageDocument::empty();
    message.attachments = vec![server_attachment(
        AttachmentId::generate(),
        "photo.png",
        900,
    )];
    fx.driver.send(fx.world.event(message.clone(), None));
    let row = fx.driver.timeline(fx.general)[0].clone();
    assert!(row.document.has_no_content());
    assert_eq!(row.document.plain_text(), "");
    assert_eq!(row.document.to_markdown(), "");
    assert_eq!(
        fx.driver.core.edit_text(fx.general, message.id),
        Some(String::new())
    );
    let excerpt = fx
        .driver
        .core
        .message_excerpt(fx.general, message.id)
        .unwrap();
    assert_eq!(excerpt.text, "photo.png");
    // Two files read as a count.
    let mut two = fx.world.message(fx.general, &fx.world.other, 12, "x");
    two.document = MessageDocument::empty();
    two.attachments = vec![
        server_attachment(AttachmentId::generate(), "a.png", 1),
        server_attachment(AttachmentId::generate(), "b.png", 1),
    ];
    fx.driver.send(fx.world.event(two.clone(), None));
    assert_eq!(
        fx.driver
            .core
            .message_excerpt(fx.general, two.id)
            .unwrap()
            .text,
        "2 files"
    );
    // It counts as a message like any other: it is unread for the person.
    assert_eq!(
        fx.driver
            .core
            .user_state(fx.general)
            .map(|state| state.unread_count),
        Some(2)
    );
}

#[test]
fn a_message_with_files_may_lose_its_words_but_one_without_may_not() {
    let mut fx = setup();
    let mut message = fx.world.message(fx.general, &fx.world.me, 11, "words");
    message.attachments = vec![server_attachment(
        AttachmentId::generate(),
        "photo.png",
        900,
    )];
    fx.driver.send(fx.world.event(message.clone(), None));
    let out = fx.driver.send(Input::EditMessage {
        conversation: fx.general,
        message: message.id,
        text: String::new(),
    });
    let journaled = out.effects.iter().find_map(|effect| match effect {
        Effect::PersistSubmission(entry) => Some(entry.clone()),
        _ => None,
    });
    let entry = journaled.expect("the edit is journaled");
    assert!(entry.document.has_no_content());
    assert!(fx.driver.core.notice().is_none());

    let plain = fx.world.message(fx.general, &fx.world.me, 12, "just words");
    fx.driver.send(fx.world.event(plain.clone(), None));
    let out = fx.driver.send(Input::EditMessage {
        conversation: fx.general,
        message: plain.id,
        text: String::new(),
    });
    assert!(out.effects.is_empty());
    assert_eq!(
        fx.driver.core.notice(),
        Some("A message can't be empty. Delete it instead.")
    );
}

#[test]
fn files_survive_in_the_saved_cache() {
    let mut fx = setup();
    let mut message = fx.world.message(fx.general, &fx.world.other, 11, "look");
    let id = AttachmentId::generate();
    message.attachments = vec![server_attachment(id, "photo.png", 900)];
    fx.driver.send(fx.world.event(message.clone(), None));
    let cache = fx.driver.last_cache().expect("a cache was saved").clone();
    let cached = cache
        .workspaces
        .iter()
        .flat_map(|w| w.timelines.iter())
        .flat_map(|t| t.messages.iter())
        .find(|m| m.id == message.id)
        .expect("the message is cached");
    assert_eq!(cached.attachments, message.attachments);
    // And back.
    let mut restarted = Driver::new();
    restarted.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache),
        ..Default::default()
    }));
    let row = restarted
        .timeline(fx.general)
        .iter()
        .find(|item| item.message_id() == Some(message.id))
        .cloned()
        .expect("the cached message is shown at once");
    assert_eq!(row.attachments, message.attachments);
}

// ----- Downloads -----

fn start_download(fx: &mut Fx, attachment: &Attachment) -> Output {
    fx.driver.send(Input::DownloadAttachment {
        attachment: attachment.clone(),
        destination: PathBuf::from("/home/jules/Downloads").join(&attachment.name),
    })
}

#[test]
fn a_download_reports_progress_and_the_path_it_was_saved_to() {
    let mut fx = setup();
    let attachment = server_attachment(AttachmentId::generate(), "report.pdf", 1000);
    let out = start_download(&mut fx, &attachment);
    assert!(out.effects.contains(&Effect::DownloadAttachment {
        attachment: attachment.clone(),
        destination: PathBuf::from("/home/jules/Downloads/report.pdf")
    }));
    assert_eq!(
        fx.driver.core.download(attachment.id),
        Some(&DownloadState::Downloading {
            received: 0,
            total: 1000
        })
    );
    // Asking again while it runs does nothing.
    assert!(start_download(&mut fx, &attachment).effects.is_empty());
    fx.driver.send(Input::DownloadProgress {
        attachment: attachment.id,
        received: 600,
        total: 1000,
    });
    fx.driver.send(Input::DownloadProgress {
        attachment: attachment.id,
        received: 300,
        total: 1000,
    });
    assert_eq!(
        fx.driver.core.download(attachment.id),
        Some(&DownloadState::Downloading {
            received: 600,
            total: 1000
        })
    );
    let out = fx.driver.send(Input::DownloadResult {
        attachment: attachment.id,
        result: Ok(PathBuf::from("/home/jules/Downloads/report.pdf")),
    });
    assert!(out.changes.contains(&Change::DownloadChanged {
        attachment: attachment.id
    }));
    assert_eq!(
        fx.driver.core.download(attachment.id),
        Some(&DownloadState::Done {
            path: PathBuf::from("/home/jules/Downloads/report.pdf")
        })
    );
    fx.driver.send(Input::DismissDownload(attachment.id));
    assert_eq!(fx.driver.core.download(attachment.id), None);
}

#[test]
fn a_download_that_fails_says_why_and_can_be_started_again() {
    let cases = [
        (
            TransferError::ChecksumMismatch,
            "x.bin didn't download correctly (its checksum doesn't match). Try again.",
        ),
        (
            TransferError::Disk("No space left on device".into()),
            "Couldn't save x.bin: No space left on device.",
        ),
        (
            rejected(ErrorCode::NotFound),
            "x.bin is no longer available.",
        ),
        (
            lost(),
            "Couldn't download x.bin. Check your connection and try again.",
        ),
    ];
    for (error, text) in cases {
        let mut fx = setup();
        let attachment = server_attachment(AttachmentId::generate(), "x.bin", 10);
        start_download(&mut fx, &attachment);
        fx.driver.send(Input::DownloadResult {
            attachment: attachment.id,
            result: Err(error),
        });
        assert_eq!(
            fx.driver.core.download(attachment.id),
            Some(&DownloadState::Failed {
                reason: text.to_owned()
            })
        );
        let out = start_download(&mut fx, &attachment);
        assert_eq!(out.effects.len(), 1);
    }
}

#[test]
fn cancelling_a_download_forgets_it_and_ignores_its_late_answer() {
    let mut fx = setup();
    let attachment = server_attachment(AttachmentId::generate(), "x.bin", 10);
    start_download(&mut fx, &attachment);
    let out = fx.driver.send(Input::CancelDownload(attachment.id));
    assert!(out.effects.contains(&Effect::CancelDownload {
        attachment: attachment.id
    }));
    assert_eq!(fx.driver.core.download(attachment.id), None);
    fx.driver.send(Input::DownloadResult {
        attachment: attachment.id,
        result: Ok(PathBuf::from("/x")),
    });
    assert_eq!(fx.driver.core.download(attachment.id), None);
}

// ----- Previews -----

fn image(len: usize) -> ImageData {
    ImageData::new(vec![1; len])
}

fn fetches(out: &Output) -> Vec<(AttachmentId, bool)> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::FetchImage { attachment, full } => Some((*attachment, *full)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_thumbnail_is_asked_for_once_and_then_ready() {
    let mut fx = setup();
    let id = AttachmentId::generate();
    let out = fx.driver.send(Input::LoadThumbnail(id));
    assert_eq!(fetches(&out), vec![(id, false)]);
    assert_eq!(fx.driver.core.thumbnail(id), Some(&ImageState::Loading));
    // Asking again, as a row does on every draw, costs nothing.
    assert!(fx.driver.send(Input::LoadThumbnail(id)).effects.is_empty());
    let out = fx.driver.send(Input::ImageLoaded {
        attachment: id,
        full: false,
        result: Ok(image(10)),
    });
    assert!(
        out.changes
            .contains(&Change::ImageChanged { attachment: id })
    );
    assert_eq!(
        fx.driver.core.thumbnail(id),
        Some(&ImageState::Ready(image(10)))
    );
    assert!(fx.driver.send(Input::LoadThumbnail(id)).effects.is_empty());
}

#[test]
fn a_failed_thumbnail_stays_failed_until_the_connection_comes_back() {
    let mut fx = setup();
    let id = AttachmentId::generate();
    fx.driver.send(Input::LoadThumbnail(id));
    fx.driver.send(Input::ImageLoaded {
        attachment: id,
        full: false,
        result: Err(lost()),
    });
    assert_eq!(fx.driver.core.thumbnail(id), Some(&ImageState::Failed));
    assert!(fx.driver.send(Input::LoadThumbnail(id)).effects.is_empty());
    fx.driver
        .send(Input::Frame(lazy_protocol::ServerFrame::CaughtUp {
            workspace_id: fx.world.workspace.id,
            seq: seq(10),
        }));
    assert_eq!(fx.driver.core.thumbnail(id), None);
    assert_eq!(
        fetches(&fx.driver.send(Input::LoadThumbnail(id))),
        vec![(id, false)]
    );
}

#[test]
fn asking_again_for_a_whole_image_that_failed_is_a_new_request() {
    let mut fx = setup();
    let id = AttachmentId::generate();
    fx.driver.send(Input::LoadFullImage(id));
    fx.driver.send(Input::ImageLoaded {
        attachment: id,
        full: true,
        result: Err(lost()),
    });
    assert_eq!(fx.driver.core.full_image(id), Some(&ImageState::Failed));
    let out = fx.driver.send(Input::LoadFullImage(id));
    assert_eq!(fetches(&out), vec![(id, true)]);
    assert_eq!(fx.driver.core.full_image(id), Some(&ImageState::Loading));
    fx.driver.send(Input::ImageLoaded {
        attachment: id,
        full: true,
        result: Ok(image(4)),
    });
    assert!(matches!(
        fx.driver.core.full_image(id),
        Some(ImageState::Ready(_))
    ));
}

#[test]
fn only_four_images_are_fetched_at_a_time() {
    let mut fx = setup();
    let all: Vec<AttachmentId> = (0..7).map(|_| AttachmentId::generate()).collect();
    let mut started = Vec::new();
    for id in &all {
        started.extend(fetches(&fx.driver.send(Input::LoadThumbnail(*id))));
    }
    assert_eq!(started.len(), 4);
    let out = fx.driver.send(Input::ImageLoaded {
        attachment: all[0],
        full: false,
        result: Ok(image(1)),
    });
    assert_eq!(fetches(&out), vec![(all[4], false)]);
}

#[test]
fn held_thumbnails_are_bounded_and_the_oldest_go_first() {
    let mut fx = setup();
    let count = lazy_client_core::MAX_HELD_THUMBNAILS + 5;
    let all: Vec<AttachmentId> = (0..count).map(|_| AttachmentId::generate()).collect();
    for id in &all {
        fx.driver.send(Input::LoadThumbnail(*id));
        fx.driver.send(Input::ImageLoaded {
            attachment: *id,
            full: false,
            result: Ok(image(1)),
        });
    }
    assert_eq!(fx.driver.core.thumbnail(all[0]), None);
    assert_eq!(fx.driver.core.thumbnail(all[4]), None);
    assert!(matches!(
        fx.driver.core.thumbnail(all[5]),
        Some(ImageState::Ready(_))
    ));
    assert!(matches!(
        fx.driver.core.thumbnail(all[count - 1]),
        Some(ImageState::Ready(_))
    ));
}

#[test]
fn held_thumbnails_are_bounded_in_bytes_too() {
    let mut fx = setup();
    let big = lazy_client_core::MAX_HELD_THUMBNAIL_BYTES / 3 + 1;
    let ids: Vec<AttachmentId> = (0..3).map(|_| AttachmentId::generate()).collect();
    for id in &ids {
        fx.driver.send(Input::LoadThumbnail(*id));
        fx.driver.send(Input::ImageLoaded {
            attachment: *id,
            full: false,
            result: Ok(image(big)),
        });
    }
    assert_eq!(fx.driver.core.thumbnail(ids[0]), None);
    assert!(matches!(
        fx.driver.core.thumbnail(ids[1]),
        Some(ImageState::Ready(_))
    ));
    assert!(matches!(
        fx.driver.core.thumbnail(ids[2]),
        Some(ImageState::Ready(_))
    ));
}

#[test]
fn full_images_are_few_and_can_be_released() {
    let mut fx = setup();
    let ids: Vec<AttachmentId> = (0..3).map(|_| AttachmentId::generate()).collect();
    for id in &ids {
        let out = fx.driver.send(Input::LoadFullImage(*id));
        assert_eq!(fetches(&out), vec![(*id, true)]);
        fx.driver.send(Input::ImageLoaded {
            attachment: *id,
            full: true,
            result: Ok(image(5)),
        });
    }
    assert_eq!(fx.driver.core.full_image(ids[0]), None);
    assert!(matches!(
        fx.driver.core.full_image(ids[2]),
        Some(ImageState::Ready(_))
    ));
    fx.driver.send(Input::ReleaseFullImage(ids[2]));
    assert_eq!(fx.driver.core.full_image(ids[2]), None);
}

#[test]
fn a_deleted_messages_previews_are_dropped() {
    let mut fx = setup();
    let id = AttachmentId::generate();
    let mut message = fx.world.message(fx.general, &fx.world.other, 11, "look");
    message.attachments = vec![server_attachment(id, "photo.png", 900)];
    fx.driver.send(fx.world.event(message.clone(), None));
    fx.driver.send(Input::LoadThumbnail(id));
    fx.driver.send(Input::ImageLoaded {
        attachment: id,
        full: false,
        result: Ok(image(3)),
    });
    let out = fx
        .driver
        .send(fx.world.deleted_event(12, &fx.world.tombstone(&message)));
    assert!(out.effects.contains(&Effect::ForgetAttachments {
        attachments: vec![id]
    }));
    assert_eq!(fx.driver.core.thumbnail(id), None);
}

#[test]
fn signing_out_forgets_every_file() {
    let mut fx = setup();
    let id = attach(&mut fx, &["a.txt"])[0];
    staged(&mut fx, id, 10);
    let attachment = server_attachment(AttachmentId::generate(), "x.bin", 10);
    start_download(&mut fx, &attachment);
    fx.driver.send(Input::LoadThumbnail(attachment.id));
    let out = fx.driver.send(Input::SignOut);
    assert!(released(&out).contains(&id));
    assert!(out.effects.contains(&Effect::CancelDownload {
        attachment: attachment.id
    }));
    assert!(fx.driver.core.composer_attachments(fx.target).is_empty());
    assert_eq!(fx.driver.core.download(attachment.id), None);
    assert_eq!(fx.driver.core.thumbnail(attachment.id), None);
}

#[allow(dead_code)]
fn unused(_: MessageId, _: MessageResponse) {}
