//! A broad scenario run under a capturing subscriber at the most verbose level, in both log formats
//! (`tests/log_text.rs`, `tests/log_json.rs`): nothing secret and no content may appear in what the
//! server logs, while the request lines do appear, with route patterns and ids.

#![allow(dead_code, clippy::too_many_lines)]

use std::sync::{Mutex, Once};
use std::time::Duration;

use lazy_domain::OperationId;
use lazy_protocol::api::paths;
use lazy_protocol::{ErrorCode, Event, SessionResponse};
use lazy_server::logging::{self, LogFormat};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tracing_subscriber::fmt::MakeWriter;

use super::*;

pub static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

pub struct CaptureWriter;

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct Capture;

impl<'a> MakeWriter<'a> for Capture {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> CaptureWriter {
        CaptureWriter
    }
}

/// Everything, from every crate, at TRACE, written the way the server writes (apart from the
/// target). The directive that keeps `axum::rejection` off is the server's own and stays on.
pub fn capture(format: LogFormat) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let (filter, _) = logging::filter(Some("trace"));
        let subscriber = logging::build(format, filter, Capture, false);
        tracing::subscriber::set_global_default(subscriber).expect("one subscriber per binary");
    });
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn named_registration(
    app: &App,
    username: &str,
    display_name: &str,
    password: &str,
    invite_code: Option<&str>,
) -> reqwest::Response {
    app.http
        .post(app.url(paths::REGISTER))
        .json(&serde_json::json!({
            "username": username,
            "display_name": display_name,
            "password": password,
            "invite_code": invite_code,
        }))
        .send()
        .await
        .expect("request")
}

async fn account_of(response: reqwest::Response) -> Account {
    assert!(response.status().is_success(), "{}", response.status());
    let session: SessionResponse = response.json().await.unwrap();
    Account {
        token: session.token.expose().to_owned(),
        user: session.user,
    }
}

/// A request line as the server logged it.
#[derive(Debug, Clone, Default)]
pub struct Line {
    pub method: String,
    pub route: String,
    pub status: u16,
    pub request_id: String,
    pub user_id: Option<String>,
    pub error_code: Option<String>,
}

fn unquote(text: &str) -> &str {
    text.trim_matches('"')
}

/// The request lines in `logs`, and the request ids of the lines that report a failed request.
pub fn read_logs(logs: &str, format: LogFormat) -> (Vec<Line>, Vec<String>) {
    let mut requests = Vec::new();
    let mut failed = Vec::new();
    for line in logs.lines() {
        match format {
            LogFormat::Json => {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                    panic!("a log line that is not JSON: {line}");
                };
                let fields = &value["fields"];
                if value["message"] == "request" && value["target"] == "lazy_server::logging" {
                    requests.push(Line {
                        method: fields["method"].as_str().unwrap_or("").to_owned(),
                        route: fields["route"].as_str().unwrap_or("").to_owned(),
                        status: fields["status"].as_u64().unwrap_or(0) as u16,
                        request_id: fields["request_id"].as_str().unwrap_or("").to_owned(),
                        user_id: fields["user_id"].as_str().map(str::to_owned),
                        error_code: fields["error_code"].as_str().map(str::to_owned),
                    });
                } else if value["message"] == "request failed" {
                    let id = value["spans"][0]["request_id"].as_str().unwrap_or("");
                    failed.push(id.to_owned());
                }
            }
            LogFormat::Text => {
                if let Some((_, rest)) = line.split_once("lazy_server::logging: request ") {
                    let mut entry = Line::default();
                    for token in rest.split_whitespace() {
                        let Some((key, value)) = token.split_once('=') else {
                            continue;
                        };
                        let value = unquote(value);
                        match key {
                            "method" => entry.method = value.to_owned(),
                            "route" => entry.route = value.to_owned(),
                            "status" => entry.status = value.parse().unwrap_or(0),
                            "request_id" => entry.request_id = value.to_owned(),
                            "user_id" => entry.user_id = Some(value.to_owned()),
                            "error_code" => entry.error_code = Some(value.to_owned()),
                            _ => {}
                        }
                    }
                    requests.push(entry);
                } else if line.contains("request failed")
                    && let Some(start) = line.find("request_id=")
                {
                    let id: String = line[start + "request_id=".len()..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                        .collect();
                    failed.push(id);
                }
            }
        }
    }
    (requests, failed)
}

/// Runs the scenario and checks the log.
pub async fn run(
    pool_options: PgPoolOptions,
    connect_options: PgConnectOptions,
    format: LogFormat,
) {
    capture(format);
    // The server's pool as the server makes its own (statement logging off).
    let pool = pool_options
        .max_connections(10)
        .connect_with(lazy_server::db::quiet(connect_options))
        .await
        .unwrap();
    LOGS.lock().unwrap().clear();

    let app = App::start(pool).await;
    let mut secrets: Vec<String> = Vec::new();
    let mut secret = |text: &str| {
        secrets.push(text.to_owned());
        text.to_owned()
    };

    // --- Accounts, sign-in, invitations -------------------------------------------------------
    let alice_password = secret("hunter2-Zq8vSECRETPWONE");
    let alice_name = secret("Ysolde Quillfeather");
    let alice_username = secret("unamekestrel42");
    let alice = account_of(
        named_registration(&app, &alice_username, &alice_name, &alice_password, None).await,
    )
    .await;
    secret(&alice.token);
    secret(&alice.token[..24]);
    secret(&hex_of(
        &lazy_server::crypto::token_hash_if_wellformed(&alice.token).expect("a token"),
    ));
    assert!(
        app.try_login(&alice_username, &alice_password)
            .await
            .status()
            .is_success()
    );
    let wrong_password = secret("wrongpass-SECRETPWTWO");
    assert_eq!(
        app.try_login(&alice_username, &wrong_password)
            .await
            .status()
            .as_u16(),
        401
    );
    assert_eq!(
        app.try_login(&secret("ghostuser77"), &wrong_password)
            .await
            .status()
            .as_u16(),
        401
    );
    let weak = secret("short");
    assert_eq!(
        named_registration(&app, "weakling", "Weak", &weak, None)
            .await
            .status()
            .as_u16(),
        400
    );

    let invite = app
        .create_invitation(&alice, serde_json::json!({"max_uses": 1}))
        .await;
    let code = secret(invite.code.expose());
    secret(&hex_of(&lazy_server::crypto::invite_code_hash(&code)));
    let bob_name = secret("Zoë Ωmegaprime");
    let bob_username = secret("bobbarnacle99");
    let bob_password = secret("bobspassword-SECRETPWTHREE");
    let bob = account_of(
        named_registration(&app, &bob_username, &bob_name, &bob_password, Some(&code)).await,
    )
    .await;
    secret(&bob.token);
    let carol =
        account_of(named_registration(&app, "carolcrane", "Carol", &alice_password, None).await)
            .await;
    secret(&carol.token);
    // The same code again, a guess, a malformed one, a revoked one, an expired one: one answer.
    let guess = secret("lzy_inv_SECRETGUESSCODEabcdefghijklmnop");
    for attempt in [code.as_str(), guess.as_str(), "SECRET-malformed code!"] {
        let response =
            named_registration(&app, "latecomer", "Late", &alice_password, Some(attempt)).await;
        assert_eq!(response.status().as_u16(), 400);
    }
    let elsewhere = app.other_workspace(&[(&alice, "owner")]).await;
    let revoked = app
        .invitation_to(&alice, elsewhere, serde_json::json!({}))
        .await;
    secret(revoked.code.expose());
    let response = app
        .call(
            reqwest::Method::DELETE,
            &paths::invitation(revoked.invitation.id),
            &alice,
            None,
        )
        .await;
    assert!(response.status().is_success());
    assert_eq!(
        app.accept_raw(&carol, revoked.code.expose())
            .await
            .status()
            .as_u16(),
        400
    );
    let expired = app
        .invitation_to(&alice, elsewhere, serde_json::json!({}))
        .await;
    secret(expired.code.expose());
    sqlx::query(
        "UPDATE workspace_invitations SET expires_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(expired.invitation.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        app.accept_raw(&carol, expired.code.expose())
            .await
            .status()
            .as_u16(),
        400
    );

    // --- Profile, picture, workspace, channels -------------------------------------------------
    let new_name = secret("Barnabas Thistlewick");
    app.rename_me(&alice, &new_name).await;
    let picture = png_bytes(64, 64, 255);
    app.set_avatar(&alice, picture.clone()).await;
    assert_eq!(
        app.set_avatar_raw(&alice, vec![7; 5 * 1024 * 1024 + 1], "image/png")
            .await
            .status()
            .as_u16(),
        413
    );
    assert_eq!(
        app.set_avatar_raw(
            &alice,
            b"SECRETGARBAGEBYTES not an image".to_vec(),
            "image/png"
        )
        .await
        .status()
        .as_u16(),
        415
    );
    secret("SECRETGARBAGEBYTES");
    assert!(
        app.avatar_raw(&bob, alice.user.id, Some(1), &[])
            .await
            .status()
            .is_success()
    );
    let workspace_name = secret("Ws-Aubergine-Cathedral");
    let created = app.create_workspace(&alice, &workspace_name).await;
    let renamed = secret("Renamed-Marmalade-Observatory");
    assert!(
        app.patch_workspace_raw(
            &alice,
            created.workspace.id,
            serde_json::json!({"name": renamed})
        )
        .await
        .status()
        .is_success()
    );
    let channel_name = secret("chan-pelican-ledger");
    let topic = secret("topic-marzipan-lantern");
    let description = secret("description-cormorant-bramble");
    let channel = app
        .call(
            reqwest::Method::POST,
            &paths::channels(app.workspace_id),
            &alice,
            Some(serde_json::json!({
                "operation_id": OperationId::generate(), "name": channel_name,
                "visibility": "public", "topic": topic, "description": description,
            })),
        )
        .await;
    assert!(channel.status().is_success(), "{}", channel.status());
    let channel = channel
        .json::<lazy_protocol::ConversationResponse>()
        .await
        .unwrap()
        .conversation;
    let group_handle = secret("grp_saffron_dune");
    app.create_group(&alice, &group_handle, &[&bob]).await;
    app.set_status(&alice, &secret("status-kumquat-oracle"), None)
        .await;
    app.upload_emoji(
        &alice,
        app.workspace_id,
        &secret("emoji_tapioca"),
        png_bytes(32, 32, 255),
    )
    .await;

    // --- Messages, files, search, saved ----------------------------------------------------------
    let first = app
        .send(&alice, app.general, &secret("quokkazanzibar hello there"))
        .await;
    let reply = app
        .reply(&bob, &first, &secret("glockenspielfjord answering"))
        .await;
    app.edit(&alice, &first, &secret("narwhalspatula after the edit"))
        .await;
    let doomed = app
        .send(&bob, app.general, &secret("marmotkestrel to delete"))
        .await;
    app.delete(&bob, doomed.id).await;
    app.react(&alice, reply.id, "👍", true).await;
    assert!(!app.find(&alice, "narwhalspatula").await.hits.is_empty());
    let search_query = secret("queryvelociraptorlemon");
    assert!(app.find(&alice, &search_query).await.hits.is_empty());
    app.set_saved(&alice, first.id, true).await;
    let _ = app.people(&alice, &[("query", "Thistle")]).await;
    let dm = app.open(&alice, &[&bob]).await;
    app.send(&alice, dm.id, &secret("pomegranate-whisper direct message"))
        .await;
    app.send(&alice, channel.id, &secret("lemurs-in-the-channel text"))
        .await;
    let file_name = secret("SECRETreport-Pomegranate-2026.pdf");
    secret(&percent_encode(&file_name));
    let file = b"%PDF-1.4 SECRETFILECONTENTSZZ".to_vec();
    secret("SECRETFILECONTENTSZZ");
    let attachment = app
        .upload(
            &alice,
            app.general,
            file.clone(),
            "application/pdf",
            &file_name,
        )
        .await;
    secret(&attachment.sha256);
    secret(&sha_hex(&file));
    let unicode_name = secret("résumé-Λάμβδα-secretdoc.txt");
    secret(&percent_encode(&unicode_name));
    app.upload(
        &alice,
        app.general,
        b"plain text".to_vec(),
        "text/plain",
        &unicode_name,
    )
    .await;
    assert!(
        app.download_raw(&alice, attachment.id, false, false, &[])
            .await
            .status()
            .is_success()
    );
    let image = app
        .upload(
            &alice,
            app.general,
            png_bytes(900, 400, 255),
            "image/png",
            "photo.png",
        )
        .await;
    assert!(
        app.download_raw(&alice, image.id, true, false, &[])
            .await
            .status()
            .is_success()
    );

    // --- A webhook: its address is a secret ------------------------------------------------------
    let hook = app
        .create_webhook(&alice, channel.id, &secret("hook-name-bellflower"))
        .await;
    let hook_secret = hook.url.rsplit('/').next().unwrap().to_owned();
    secret(&hook_secret);
    secret(&hook.url);
    secret(&hex_of(&lazy_server::crypto::webhook_secret_hash(
        &hook_secret,
    )));
    let delivered = app
        .deliver_raw(
            &hook.url,
            &secret("wombatturnstile deploy finished"),
            &[("Idempotency-Key", &secret("idem-key-SECRETNINE"))],
        )
        .await;
    assert!(delivered.status().is_success(), "{}", delivered.status());
    let wrong_secret = secret(&"feedface".repeat(8));
    for url in [
        format!(
            "{}/hooks/{}/{wrong_secret}",
            app.server.base_url, hook.webhook.id
        ),
        format!(
            "{}/hooks/{}/{wrong_secret}",
            app.server.base_url,
            lazy_domain::WebhookId::generate()
        ),
        format!("{}/extra/{wrong_secret}?token={hook_secret}", hook.url),
    ] {
        assert!(
            !app.deliver_raw(&url, "hello", &[])
                .await
                .status()
                .is_success()
        );
    }
    assert!(
        app.delete_webhook_raw(&alice, hook.webhook.id)
            .await
            .status()
            .is_success()
    );

    // --- Requests that fail in many ways ------------------------------------------------------------
    let bad_token = secret(&format!("lzy_{}", "deadbeef".repeat(8)));
    let anonymous = app
        .http
        .get(app.url(paths::BOOTSTRAP))
        .bearer_auth(&bad_token)
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status().as_u16(), 401);
    assert_eq!(
        app.http
            .get(app.url(paths::BOOTSTRAP))
            .header("authorization", "Bearer SECRET-not-a-token-at-all")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    secret("SECRET-not-a-token-at-all");
    // Bodies that do not parse, quoting what was sent: syntax, shape, size.
    for (path, body) in [
        (
            paths::LOGIN.to_owned(),
            "{\"username\": \"SECRETSYNTAXUSER\", \"password\": ".to_owned(),
        ),
        (
            paths::LOGIN.to_owned(),
            "{\"username\": [\"SECRETARRAYNAME\"], \"password\": 5}".to_owned(),
        ),
        (
            paths::REGISTER.to_owned(),
            format!("{{\"username\": \"{}\"}}", "SECRETTOOLONG".repeat(4000)),
        ),
    ] {
        let response = app
            .http
            .post(app.url(&path))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_client_error());
    }
    for body in [
        serde_json::json!({"display_name": ["SECRETARRAYNAME2"]}),
        serde_json::json!({"display_name": {"SECRETKEY": 1}}),
    ] {
        assert!(
            app.patch_me_raw(&alice, body)
                .await
                .status()
                .is_client_error()
        );
    }
    assert!(
        app.set_status_raw(
            &alice,
            app.workspace_id,
            serde_json::json!({"status": "SECRETSTATUSSTRING"})
        )
        .await
        .status()
        .is_client_error()
    );
    for fragment in [
        "SECRETARRAYNAME",
        "SECRETARRAYNAME2",
        "SECRETSYNTAXUSER",
        "SECRETKEY",
        "SECRETSTATUSSTRING",
        "SECRETTOOLONG",
    ] {
        secret(fragment);
    }
    // Queries and paths that do not parse, unknown routes, wrong methods.
    let bad_query = app
        .search_raw(
            &alice,
            app.workspace_id,
            &[("q", "SECRETQUERYBAD"), ("limit", "SECRETLIMIT")],
        )
        .await;
    assert_eq!(bad_query.status().as_u16(), 400);
    secret("SECRETQUERYBAD");
    secret("SECRETLIMIT");
    let bad_path = app
        .call(
            reqwest::Method::GET,
            "/v1/conversations/SECRETPATHID/messages",
            &alice,
            None,
        )
        .await;
    assert!(bad_path.status().is_client_error());
    secret("SECRETPATHID");
    let custom_id = "req-scenario-0001";
    let unmatched = app
        .http
        .get(app.url("/v1/does-not-exist/SECRETUNMATCHED?x=SECRETUNMATCHEDQUERY"))
        .header("x-request-id", custom_id)
        .send()
        .await
        .unwrap();
    assert_eq!(unmatched.status().as_u16(), 404);
    assert_eq!(unmatched.headers()["x-request-id"], custom_id);
    secret("SECRETUNMATCHED");
    secret("SECRETUNMATCHEDQUERY");
    let long_id = "x".repeat(200);
    let replaced = app
        .http
        .get(app.url("/healthz"))
        .header("x-request-id", &long_id)
        .send()
        .await
        .unwrap();
    let fresh = replaced.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        fresh != long_id && logging::request_id_is_well_formed(&fresh),
        "{fresh}"
    );
    let wrong_method = app
        .http
        .delete(app.url(paths::LOGIN))
        .bearer_auth(&alice.token)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_method.status().as_u16(), 405);
    assert_eq!(
        app.get_message_raw(&alice, lazy_domain::MessageId::generate())
            .await
            .status()
            .as_u16(),
        404
    );
    assert_eq!(
        app.send_raw(
            &carol,
            channel.id,
            OperationId::generate(),
            &secret("zebra-finch-text")
        )
        .await
        .status()
        .as_u16(),
        403
    );

    // --- A person who is deactivated asks for everything --------------------------------------------
    app.deactivate(&alice, &bob, true).await;
    let _ = app.history(&bob, app.general, &[]).await;
    let _ = app
        .send_raw(
            &bob,
            app.general,
            OperationId::generate(),
            &secret("sandpiper-text"),
        )
        .await;
    let _ = app
        .search_raw(&bob, app.workspace_id, &[("q", "narwhalspatula")])
        .await;
    let _ = app.bootstrap(&bob.token).await;
    let _ = app.avatar_raw(&bob, alice.user.id, Some(1), &[]).await;
    let _ = app
        .download_raw(&bob, attachment.id, false, false, &[])
        .await;
    let _ = app.accept_raw(&bob, &code).await;

    // --- Requests that fail inside the server ------------------------------------------------------------
    // A database error whose message quotes data: a trigger that refuses with a message of its own.
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_save() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'SECRETDBMESSAGE'; END $$; \
         CREATE TRIGGER refuse_save BEFORE INSERT ON saved_messages \
         FOR EACH ROW EXECUTE FUNCTION refuse_save()",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    secret("SECRETDBMESSAGE");
    let refused = app.set_saved_raw(&alice, reply.id, true).await;
    assert_eq!(refused.status().as_u16(), 500);
    let refused_id = refused.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    // A stored value that is no longer valid.
    let corrupted = format!("snow{}man", '\u{2603}');
    sqlx::query("UPDATE users SET username = $2 WHERE id = $1")
        .bind(carol.user.id.as_uuid())
        .bind(&corrupted)
        .execute(&app.pool)
        .await
        .unwrap();
    secret(&corrupted);
    secret("\u{2603}");
    let broken = app.bootstrap(&carol.token).await;
    assert_eq!(broken.status().as_u16(), 500);
    let broken_id = broken.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();

    // --- The socket: a bad frame quoting content, a good subscription ------------------------------------
    let mut ws = app.connect_ws(&alice).await;
    ws.send_text("{\"type\": \"subscribe\", \"workspace_id\": \"SECRETFRAMEVALUE\", \"after\": 0}")
        .await;
    ws.send_text("SECRETFRAMEGARBAGE not json").await;
    ws.send_text("{\"type\": \"SECRETFRAMETYPE\"}").await;
    secret("SECRETFRAMETYPE");
    secret("SECRETFRAMEVALUE");
    secret("SECRETFRAMEGARBAGE");
    // Each bad frame is answered with an error frame that says nothing of what was sent.
    for _ in 0..3 {
        match ws.next().await {
            lazy_protocol::ServerFrame::Error { message, .. } => {
                assert!(!message.contains("SECRETFRAME"), "{message}");
            }
            other => panic!("expected an error frame, got {other:?}"),
        }
    }
    ws.subscribe(app.workspace_id, lazy_domain::EventSeq::ZERO)
        .await;
    let (events, _) = ws.replay().await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, Event::MessageCreated { .. }))
    );
    let mut deactivated = app.connect_ws(&bob).await;
    deactivated
        .subscribe(app.workspace_id, lazy_domain::EventSeq::ZERO)
        .await;
    let _ = deactivated.next_or_closed().await;
    drop(ws);
    drop(deactivated);

    // The server stops: its shutdown, dispatcher and cleanup logs are part of the scenario.
    app.server.shutdown().await.unwrap();

    // --- The checks ---------------------------------------------------------------------------------------
    let bytes = LOGS.lock().unwrap().clone();
    let logs = String::from_utf8_lossy(&bytes).into_owned();
    if let Ok(path) = std::env::var("LAZY_TEST_LOG_DUMP") {
        let _ = std::fs::write(path, &bytes);
    }
    assert!(
        logs.len() > 2_000,
        "the capture saw almost nothing: {} bytes",
        logs.len()
    );
    let leaked: Vec<&String> = secrets
        .iter()
        .filter(|secret| !secret.is_empty() && logs.contains(secret.as_str()))
        .collect();
    assert!(leaked.is_empty(), "logged: {leaked:?}");
    for marker in ["$argon2", "Authorization", "authorization", "sqlx::query"] {
        assert!(!logs.contains(marker), "the log contains {marker:?}");
    }
    assert!(
        !bytes.windows(4).any(|w| w == b"\x89PNG"),
        "picture bytes were logged"
    );

    // ...and the requests are in it, by route pattern, with ids.
    let (requests, failed) = read_logs(&logs, format);
    let routes: std::collections::HashSet<&str> =
        requests.iter().map(|l| l.route.as_str()).collect();
    for expected in [
        paths::REGISTER,
        paths::LOGIN,
        paths::BOOTSTRAP,
        paths::MESSAGES_ROUTE,
        paths::MESSAGE_ROUTE,
        paths::SEARCH_ROUTE,
        paths::HOOK_ROUTE,
        paths::ME_AVATAR_ROUTE,
        paths::USER_AVATAR_ROUTE,
        paths::ATTACHMENT_UPLOAD_ROUTE,
        paths::ATTACHMENT_ROUTE,
        paths::EMOJI_UPLOAD_ROUTE,
        paths::ACCEPT_INVITATION_ROUTE,
        paths::WORKSPACE_ROUTE,
        paths::REALTIME,
        paths::HEALTH,
        logging::UNMATCHED_ROUTE,
    ] {
        assert!(
            routes.contains(expected),
            "no request line for {expected}; saw {routes:?}"
        );
    }
    assert!(
        requests
            .iter()
            .all(|l| !l.route.contains(&hook_secret) && !l.request_id.is_empty()),
        "a request line without an id or with a secret in its route"
    );
    // An authenticated request names its person; the id is the only thing said about them.
    assert!(requests.iter().any(|l| {
        l.route == paths::MESSAGES_ROUTE
            && l.method == "POST"
            && l.status == 200
            && l.user_id.as_deref() == Some(&alice.user.id.to_string())
    }));
    assert!(
        requests
            .iter()
            .any(|l| l.user_id.as_deref() == Some(&bob.user.id.to_string()))
    );
    // Refusals say which code, and the hook's failures are logged as the pattern too.
    assert!(
        requests
            .iter()
            .any(|l| l.route == paths::HOOK_ROUTE && l.status == 404)
    );
    assert!(
        requests
            .iter()
            .any(|l| l.error_code.as_deref() == Some("unauthenticated"))
    );
    assert!(
        requests
            .iter()
            .any(|l| l.error_code.as_deref() == Some("invitation_invalid"))
    );
    // The client's request id, and the replaced one, are in the log.
    assert!(
        requests
            .iter()
            .any(|l| l.request_id == custom_id && l.route == logging::UNMATCHED_ROUTE)
    );
    assert!(
        requests
            .iter()
            .any(|l| l.request_id == fresh && l.route == paths::HEALTH)
    );
    // The request that failed inside the server: its error line carries its id, and says no more
    // than what kind of failure it was.
    assert!(
        failed.contains(&broken_id),
        "no error line for request {broken_id}: {failed:?}"
    );
    assert!(
        requests
            .iter()
            .any(|l| l.request_id == broken_id && l.status == 500)
    );
    assert!(
        failed.contains(&refused_id),
        "no error line for request {refused_id}: {failed:?}"
    );
    assert!(
        logs.contains("sqlstate P0001"),
        "the database error is described by its code"
    );
    let _ = (ErrorCode::Internal, Duration::ZERO);
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
