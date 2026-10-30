//! `lazy-server healthcheck` and the refusal of a setup code that is too short: the parts of
//! deployment that are the binary's own.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::*;
use lazy_server::health;
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A server that answers every connection with `answer` (after reading the request), and tells
/// what request line it saw.
async fn fake_server(
    answer: &'static [u8],
) -> (
    std::net::SocketAddr,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (seen, seen_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut request = vec![0u8; 2048];
                let n = stream.read(&mut request).await.unwrap_or(0);
                let line = String::from_utf8_lossy(&request[..n])
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_owned();
                let _ = seen.send(line);
                if answer.is_empty() {
                    // Never answers: holds the connection open.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                } else {
                    let _ = stream.write_all(answer).await;
                    let _ = stream.shutdown().await;
                }
            });
        }
    });
    (addr, seen_rx)
}

/// An address nothing listens on.
async fn dead_address() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

/// Runs the server binary (off the runtime's thread, which the test server needs).
async fn cli(args: &[&str], env: &[(&str, &str)]) -> (Option<i32>, String, String, Duration) {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lazy-server"));
        command
            .args(&args)
            .env_remove("LAZY_LISTEN")
            .env_remove("LAZY_SETUP_CODE")
            .env_remove("DATABASE_URL")
            .env_remove("RUST_LOG")
            .env_remove("LAZY_LOG");
        for (key, value) in &env {
            command.env(key, value);
        }
        let output = command.output().expect("the server binary runs");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            started.elapsed(),
        )
    })
    .await
    .unwrap()
}

#[sqlx::test]
async fn the_health_check_passes_for_a_running_server(pool: PgPool) {
    let app = App::start(pool).await;
    let base = format!("http://{}", app.server.addr);
    health::check(&base, health::TIMEOUT).await.unwrap();
    health::check(&format!("{base}/healthz"), health::TIMEOUT)
        .await
        .unwrap();
    // The path it asks for is the health path, over HTTP/1.0.
    let (addr, mut seen) = fake_server(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
    health::check(&format!("http://{addr}"), health::TIMEOUT)
        .await
        .unwrap();
    assert_eq!(seen.recv().await.unwrap(), "GET /healthz HTTP/1.0");
    app.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_health_check_fails_for_everything_that_is_not_a_200_in_time() {
    let dead = dead_address().await;
    let reason = health::check(&format!("http://{dead}"), health::TIMEOUT)
        .await
        .unwrap_err();
    assert!(reason.starts_with("cannot connect"), "{reason}");

    for (answer, expected) in [
        (
            &b"HTTP/1.1 503 Service Unavailable\r\n\r\n"[..],
            "status 503",
        ),
        (&b"HTTP/1.0 404 Not Found\r\n\r\n"[..], "status 404"),
        (
            &b"HTTP/1.1 301 Moved\r\nlocation: /elsewhere\r\n\r\n"[..],
            "status 301",
        ),
        (&b"SSH-2.0-OpenSSH_9.0\r\n"[..], "the answer was not HTTP"),
        (&b"200 OK\r\n"[..], "the answer was not HTTP"),
        (&b"\x00\x01\x02"[..], "the answer was not HTTP"),
    ] {
        let (addr, _seen) = fake_server(answer).await;
        let reason = health::check(&format!("http://{addr}"), health::TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(reason, expected);
    }
    // Accepts and says nothing: the time limit ends it.
    let (silent, _seen) = fake_server(b"").await;
    let started = Instant::now();
    let reason = health::check(&format!("http://{silent}"), Duration::from_millis(300))
        .await
        .unwrap_err();
    assert_eq!(reason, "timed out");
    assert!(started.elapsed() < Duration::from_secs(5));
    // Not something it can ask.
    assert!(
        health::check("https://127.0.0.1:1", health::TIMEOUT)
            .await
            .is_err()
    );
    assert!(health::check("127.0.0.1:1", health::TIMEOUT).await.is_err());
}

#[sqlx::test]
async fn the_healthcheck_command_exits_zero_only_for_a_healthy_server(pool: PgPool) {
    let app = App::start(pool).await;
    let addr = app.server.addr;
    let url = format!("http://{addr}");

    let (code, out, err, _) = cli(&["healthcheck", "--url", &url], &[]).await;
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (Some(0), "healthy\n", "")
    );
    // From LAZY_LISTEN, with an unspecified host read as this machine.
    for listen in [
        format!("0.0.0.0:{}", addr.port()),
        format!("[::]:{}", addr.port()),
        addr.to_string(),
    ] {
        let (code, out, _, _) = cli(&["healthcheck"], &[("LAZY_LISTEN", &listen)]).await;
        assert_eq!((code, out.as_str()), (Some(0), "healthy\n"), "{listen}");
    }

    let dead = dead_address().await;
    let (code, out, err, took) =
        cli(&["healthcheck", "--url", &format!("http://{dead}")], &[]).await;
    assert_eq!(code, Some(1));
    assert!(out.starts_with("unhealthy: cannot connect"), "{out}");
    assert_eq!(out.lines().count(), 1);
    assert_eq!(err, "");
    assert!(took < Duration::from_secs(3), "{took:?}");
    let (code, out, _, _) = cli(
        &["healthcheck"],
        &[("LAZY_LISTEN", &format!("0.0.0.0:{}", dead.port()))],
    )
    .await;
    assert_eq!((code, out.starts_with("unhealthy:")), (Some(1), true));
    let (code, out, _, _) = cli(&["healthcheck"], &[("LAZY_LISTEN", "not an address")]).await;
    assert_eq!(
        (code, out.as_str()),
        (
            Some(1),
            "unhealthy: the listening address is not host:port\n"
        )
    );
    let (code, out, _, _) = cli(&["healthcheck", "--url", "https://example.com"], &[]).await;
    assert_eq!(code, Some(1));
    assert!(out.contains("https is not supported"), "{out}");

    // A server that has stopped is unhealthy.
    app.server.shutdown().await.unwrap();
    let (code, _, _, _) = cli(&["healthcheck", "--url", &url], &[]).await;
    assert_eq!(code, Some(1));
}

#[tokio::test]
async fn a_setup_code_the_app_cannot_send_stops_the_server_before_it_starts_and_is_not_repeated() {
    let short = "tooshortcode1";
    let unicode = "éééééééé"; // 8 characters, 16 bytes
    // Long enough, and what `openssl rand -base64` makes: not letters and digits.
    let base64 = "Zm9vYmFyYmF6cXV4Zm9vYg+/==";
    let database = "postgres://nobody@127.0.0.1:1/none";
    for (args, env) in [
        (
            vec!["serve", "--database-url", database, "--setup-code", short],
            vec![],
        ),
        (
            vec!["serve", "--database-url", database],
            vec![("LAZY_SETUP_CODE", short)],
        ),
        (
            vec!["serve", "--database-url", database, "--setup-code", unicode],
            vec![],
        ),
        (
            vec!["serve", "--database-url", database],
            vec![("LAZY_SETUP_CODE", "fifteen-chars-1")],
        ),
        (
            // The old minimum was 16: this one was a setup code, and is not now.
            vec!["serve", "--database-url", database],
            vec![("LAZY_SETUP_CODE", "nineteen-chars-1234")],
        ),
        (
            vec!["serve", "--database-url", database],
            vec![("LAZY_SETUP_CODE", base64)],
        ),
    ] {
        let (code, out, err, took) = cli(&args, &env).await;
        assert_eq!(code, Some(1), "{args:?}: {err}");
        assert!(
            err.contains("20 to 64 characters") && err.contains("did not start"),
            "{err}"
        );
        for secret in [
            short,
            unicode,
            "fifteen-chars-1",
            "nineteen-chars-1234",
            base64,
        ] {
            assert!(
                !err.contains(secret) && !out.contains(secret),
                "the code was repeated: {err}"
            );
        }
        // It stopped before it looked for the database.
        assert!(!err.contains("database"), "{err}");
        assert!(took < Duration::from_secs(5), "{took:?}");
    }
    // Of the right shape, it gets past the check (and stops at the database address, which is
    // not one).
    let (code, _, err, _) = cli(
        &["serve", "--database-url", "not a postgres url"],
        &[("LAZY_SETUP_CODE", "exactly-20-chars-aaa")],
    )
    .await;
    assert_eq!(code, Some(1));
    assert!(
        !err.contains("20 to 64 characters") && !err.contains("exactly-20-chars-aaa"),
        "{err}"
    );
    // A policy that is not one is refused by the command line.
    let (code, _, err, _) = cli(
        &[
            "serve",
            "--database-url",
            database,
            "--seed-registration",
            "closed",
        ],
        &[],
    )
    .await;
    assert_eq!(code, Some(2));
    assert!(err.contains("open") && err.contains("invite_only"), "{err}");
}
