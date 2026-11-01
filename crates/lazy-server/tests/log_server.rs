//! What the server logs when it starts, and what its request layer does around a handler that
//! panics: the parts of logging that the broad scenario (`log_text.rs`, `log_json.rs`) does not
//! reach, because that one runs the server inside the test.

use std::net::SocketAddr;
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use lazy_protocol::ErrorCode;
use lazy_server::logging::{self, LogFormat};
use lazy_server::{Config, run};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tracing_subscriber::fmt::MakeWriter;

static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct CaptureWriter;

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Capture;

impl<'a> MakeWriter<'a> for Capture {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> CaptureWriter {
        CaptureWriter
    }
}

/// The process-wide subscriber, at TRACE, with the server's panic hook.
fn capture() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let (filter, _) = logging::filter(Some("trace"));
        let subscriber = logging::build(LogFormat::Text, filter, Capture, false);
        tracing::subscriber::set_global_default(subscriber).expect("one subscriber per binary");
    });
}

fn logs() -> String {
    String::from_utf8_lossy(&LOGS.lock().unwrap()).into_owned()
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[sqlx::test]
async fn startup_logs_the_configuration_and_no_credential(
    _pool_options: PgPoolOptions,
    connect_options: PgConnectOptions,
) {
    capture();
    let database = connect_options.get_database().unwrap().to_owned();
    let url = format!(
        "postgres://{}:SECRETDBPASSWORD@{}:{}/{database}",
        connect_options.get_username(),
        connect_options.get_host(),
        connect_options.get_port(),
    );
    let mut config = Config::new("127.0.0.1:0".parse::<SocketAddr>().unwrap(), url);
    config.setup_code = Some(lazy_server::SetupCode::new("SECRETSETUPCODE-startup-1").unwrap());
    config.seed_registration = lazy_domain::workspace::RegistrationPolicy::InviteOnly;
    config.public_url =
        Some("https://svc:SECRETPUBLICPW@chat.example.com:8443/SECRETPREFIX?k=SECRETQ".to_owned());
    let scratch = tempfile::tempdir().unwrap();
    config.attachments_dir = scratch.path().to_owned();

    // What `Debug` shows of the configuration is the same safe account.
    let shown = format!("{config:?}");
    assert!(!shown.contains("SECRETSETUPCODE"), "{shown}");
    assert!(
        !shown.contains("SECRETDBPASSWORD") && !shown.contains("SECRETPUBLICPW"),
        "{shown}"
    );
    assert!(shown.contains("chat.example.com:8443") && shown.contains(&database));

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(run(config, async move {
        let _ = stopped.await;
    }));
    until("the server to start listening", || {
        logs().contains("lazy-server listening")
    })
    .await;
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();

    // The workspace the server seeded has the policy it was asked for.
    let check = PgPoolOptions::new()
        .connect_with(connect_options.clone())
        .await
        .unwrap();
    let settings: serde_json::Value = sqlx::query_scalar("SELECT settings FROM workspaces")
        .fetch_one(&check)
        .await
        .unwrap();
    assert_eq!(settings["registration"], "invite_only");
    check.close().await;

    let logs = logs();
    for secret in [
        "SECRETDBPASSWORD",
        "SECRETSETUPCODE",
        "SECRETPUBLICPW",
        "SECRETPREFIX",
        "SECRETQ",
    ] {
        assert!(!logs.contains(secret), "the startup log contains {secret}");
    }
    let starting = logs
        .lines()
        .find(|l| l.contains(" starting "))
        .expect("a starting line");
    assert!(
        starting.contains(&format!(
            "database={}:{}/{database}",
            connect_options.get_host(),
            connect_options.get_port()
        )),
        "{starting}"
    );
    assert!(
        starting.contains("public_url=\"https://chat.example.com:8443\""),
        "{starting}"
    );
    assert!(
        starting.contains("open_registration=true") && starting.contains("protocol_version="),
        "{starting}"
    );
    assert!(
        starting.contains("setup_code_set=true")
            && starting.contains("seed_registration=\"invite_only\""),
        "{starting}"
    );
    assert!(
        logs.contains("waiting for the first account: registration needs the setup code"),
        "the server says what the first visitor will find"
    );
    let ready = logs
        .lines()
        .find(|l| l.contains("database ready"))
        .expect("a ready line");
    assert!(ready.contains("migration_version=20"), "{ready}");
}

#[tokio::test]
async fn a_handler_that_panics_is_a_logged_500_that_says_nothing_of_the_panic_or_the_request() {
    capture();
    // The server's hook, which withholds the message of every panic from here on (this test's own
    // failures included, so it is installed only by the test that needs it).
    logging::install_panic_hook();
    let router = Router::new()
        .route(
            "/boom/{secret}",
            get(|| async {
                // An error that quotes data, `expect`ed: the usual shape of a panic with content.
                let value = "PANICSECRETMESSAGE"
                    .parse::<u8>()
                    .map_err(|_| "PANICSECRETMESSAGE");
                value.expect("PANICSECRETEXPECT").to_string()
            }),
        )
        .route("/fine/{id}", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn(logging::log_requests));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    let client = reqwest::Client::new();

    let response = client
        .get(format!(
            "http://{addr}/boom/PATHSECRETVALUE?q=QUERYSECRETVALUE"
        ))
        .header("x-request-id", "panic-request-0001")
        .header("authorization", "Bearer HEADERSECRETVALUE")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 500);
    assert_eq!(response.headers()["x-request-id"], "panic-request-0001");
    let body: lazy_protocol::ApiError = response.json().await.unwrap();
    assert_eq!(body.code, ErrorCode::Internal);
    assert!(!body.message.contains("PANIC"), "{}", body.message);
    // The server carries on.
    let after = client
        .get(format!("http://{addr}/fine/abc"))
        .send()
        .await
        .unwrap();
    assert_eq!(after.status().as_u16(), 200);
    assert!(!after.headers()["x-request-id"].is_empty());

    until("the request lines", || logs().contains("route=/fine/{id}")).await;
    let logs = logs();
    for secret in [
        "PANICSECRETMESSAGE",
        "PANICSECRETEXPECT",
        "PATHSECRETVALUE",
        "QUERYSECRETVALUE",
        "HEADERSECRETVALUE",
    ] {
        assert!(!logs.contains(secret), "the log contains {secret}");
    }
    // The panic is reported by place; the request by its pattern, status and id.
    assert!(logs.contains("panic (the message is withheld)") && logs.contains("log_server.rs"));
    let line = logs
        .lines()
        .find(|l| {
            l.contains("lazy_server::logging: request ") && l.contains("route=/boom/{secret}")
        })
        .expect("the panicking request is logged");
    assert!(
        line.contains("status=500") && line.contains("request_id=panic-request-0001"),
        "{line}"
    );
    assert!(line.contains("WARN"), "a 5xx is a warning: {line}");
    assert!(
        logs.lines()
            .any(|l| l.contains("request failed") && l.contains("request_id=panic-request-0001")),
        "the internal error is logged inside the request"
    );
}
