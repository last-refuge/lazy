//! Phase 3I end to end: with real clients, a real server, and real Postgres in one process, and a
//! capturing subscriber at the most verbose level for every crate, nothing a person typed or a
//! secret they used reaches a log line (D24). The server and client crates test this on their own;
//! this is the whole of it together.

mod support;

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lazy_client_core::{
    ActionKind, ActionState, AttachmentState, ComposerTarget, FileSource, Input, LoadStatus,
    LocalFile, SessionPhase,
};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::notification::UserStatus;
use lazy_domain::workspace::{ChannelCreationPolicy, RegistrationPolicy, WorkspaceSettings};
use lazy_domain::{ChannelVisibility, MessageId};
use sqlx::PgPool;
use support::{Client, PASSWORD, World};
use tracing_subscriber::Layer as _;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

fn start(world: &World, dir: &std::path::Path, name: &'static str) -> Client {
    Client::start(
        name,
        &world.server.base_url,
        &dir.join(name),
        MemoryStorage::new(),
    )
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn nothing_typed_and_no_secret_reaches_a_log_line(pool: PgPool) {
    // Two captures of the same events, each through the filter a program really uses when asked
    // for everything: the server's and the desktop's.
    let (server_log, desktop_log) = (Capture::default(), Capture::default());
    let (server_filter, _) = lazy_server::logging::filter(Some("trace"));
    let desktop_filter = lazy_desktop::logging::filter_from(Some("trace"), None);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(server_log.clone())
                .with_filter(server_filter),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(desktop_log.clone())
                .with_filter(desktop_filter),
        )
        .try_init()
        .expect("this test file is the only one in its process and installs the subscriber");

    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob, carol) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
        start(&world, dir.path(), "carol"),
    );
    alice.register("alice").await;
    bob.register("bob").await;

    // Registration by invitation: the code is a secret.
    alice.act(Input::UpdateWorkspace {
        name: None,
        settings: Some(WorkspaceSettings {
            registration: RegistrationPolicy::InviteOnly,
            channel_creation: ChannelCreationPolicy::Everyone,
        }),
    });
    alice
        .wait_until("invitation-only", |core| {
            core.workspace_settings().registration == RegistrationPolicy::InviteOnly
        })
        .await;
    alice.act(Input::OpenInvitations);
    alice
        .wait_until("the invitations", |core| {
            matches!(core.invitations().status, LoadStatus::Ready)
        })
        .await;
    alice.act(Input::CreateInvitation {
        expires_in_secs: Some(3600),
        max_uses: Some(1),
    });
    alice
        .wait_until("the code", |core| core.invitations().created.is_some())
        .await;
    let code = alice
        .read(|core| core.invitations().created.clone().unwrap())
        .code
        .expose()
        .to_owned();
    carol
        .wait_until("the sign-in form", |core| {
            matches!(core.phase(), SessionPhase::SignedOut { .. })
        })
        .await;
    carol.act(Input::Register {
        username: "carol".to_owned(),
        display_name: "Carol Quillfeather".to_owned(),
        password: PASSWORD.to_owned(),
        invite_code: code.clone(),
    });
    carol.wait_until_ready().await;

    // A wrong password, and a registration with a code that is no longer good.
    let stranger = start(&world, dir.path(), "stranger");
    stranger
        .wait_until("the sign-in form", |core| {
            matches!(core.phase(), SessionPhase::SignedOut { .. })
        })
        .await;
    let wrong = "hunter2-wrong-password-9";
    stranger.act(Input::SignIn {
        username: "alice".to_owned(),
        password: wrong.to_owned(),
    });
    stranger
        .wait_until("a refusal", |core| {
            matches!(
                core.phase(),
                SessionPhase::SignedOut {
                    last_error: Some(_)
                }
            )
        })
        .await;
    stranger.act(Input::Register {
        username: "stranger".to_owned(),
        display_name: "Stranger".to_owned(),
        password: PASSWORD.to_owned(),
        invite_code: code.clone(),
    });
    stranger
        .wait_until("a second refusal", |core| {
            matches!(
                core.phase(),
                SessionPhase::SignedOut {
                    last_error: Some(message)
                } if message.contains("valid")
            )
        })
        .await;

    // Messages, a file with a distinctive name, a topic, a status, a search, a save, and a request
    // that is refused.
    let message = "zebra-unique-phrase-31415 hello";
    alice.send(message);
    alice.wait_confirmed(message).await;
    let needle_message = "quasar-needle-7 is the thing";
    alice.send(needle_message);
    alice.wait_confirmed(needle_message).await;
    bob.wait_for_text(needle_message).await;
    let file_name = "secret-roadmap-2026.txt";
    let path: PathBuf = dir.path().join(file_name);
    std::fs::write(&path, b"contents-needle-424242").unwrap();
    let general = alice.read(|core| core.selected_conversation().unwrap());
    let target = ComposerTarget::Conversation(general);
    alice.act(Input::AttachFiles {
        target,
        files: vec![LocalFile {
            source: FileSource::Path(path),
            name: file_name.to_owned(),
        }],
    });
    alice
        .wait_until("the upload", move |core| {
            let chips = core.composer_attachments(target);
            chips.len() == 1
                && chips
                    .iter()
                    .all(|c| matches!(c.state, AttachmentState::Uploaded))
        })
        .await;
    alice.send("see the file");
    alice.wait_confirmed("see the file").await;
    alice.act(Input::CreateChannel {
        name: "plans".to_owned(),
        visibility: ChannelVisibility::Public,
        topic: "topic-needle-2718".to_owned(),
        description: "description-needle-1618".to_owned(),
    });
    alice
        .wait_until("the channel", |core| {
            *core.action(ActionKind::CreateChannel) == ActionState::Idle
                && core.conversations().iter().any(|c| !c.is_default)
        })
        .await;
    alice.act(Input::SetStatus(Some(
        UserStatus::new("gone fishing 8675309", None, None).unwrap(),
    )));
    alice
        .wait_until("the status", |core| {
            core.me()
                .and_then(|me| core.member(me.id))
                .is_some_and(|u| u.status.is_some())
        })
        .await;
    alice.act(Input::OpenSearch);
    alice.act(Input::SetSearchQuery {
        text: "quasar-needle-7".to_owned(),
    });
    alice.act(Input::RunSearch);
    alice
        .wait_until("a search result", |core| {
            core.search().is_some_and(|s| !s.hits.is_empty())
        })
        .await;
    let saved: MessageId = alice.read(|core| {
        core.timeline(general)
            .iter()
            .find(|i| i.document.plain_text() == needle_message)
            .and_then(|i| i.message_id())
            .unwrap()
    });
    alice.act(Input::SetMessageSaved {
        message_id: saved,
        saved: true,
    });
    alice
        .wait_until("the saved marker settled", move |core| core.is_saved(saved))
        .await;
    carol.act(Input::UpdateWorkspace {
        name: Some("name-needle-1123".to_owned()),
        settings: None,
    });
    carol
        .wait_until("a refusal", |core| {
            matches!(
                core.action(ActionKind::UpdateWorkspace),
                ActionState::Failed(_)
            )
        })
        .await;
    alice.act(Input::UpdateProfile {
        display_name: Some("Alicia Wombat-Needle".to_owned()),
    });
    bob.wait_until("the new name", |core| {
        core.members()
            .iter()
            .any(|u| u.display_name.as_str() == "Alicia Wombat-Needle")
    })
    .await;

    // Everything that was logged, through each filter.
    for (which, capture) in [("server", &server_log), ("desktop", &desktop_log)] {
        let text = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
        assert!(
            text.len() > 10_000,
            "a TRACE capture of all that through the {which} filter should be large, got {} bytes",
            text.len()
        );
        // The requests were logged, by pattern, not by path.
        assert!(
            text.contains("/v1/conversations/{conversation_id}/messages"),
            "{which}: request lines name the route pattern"
        );
        let secrets_and_content = [
            PASSWORD,
            wrong,
            code.as_str(),
            message,
            "zebra-unique-phrase-31415",
            "quasar-needle-7",
            file_name,
            "contents-needle-424242",
            "topic-needle-2718",
            "description-needle-1618",
            "gone fishing 8675309",
            "name-needle-1123",
            "Carol Quillfeather",
            "Alicia Wombat-Needle",
            "Wombat",
        ];
        for needle in secrets_and_content {
            assert!(
                !text.contains(needle),
                "{which}: the logs contain {needle:?}; the line is:\n{}",
                text.lines()
                    .find(|line| line.contains(needle))
                    .unwrap_or_default()
            );
        }
    }
}
