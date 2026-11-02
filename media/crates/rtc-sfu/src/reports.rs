//! Sending room reports ([`rtc_proto::webhook`]) to the host application.
//!
//! One task sends them in the order they were made, so the host never sees an older state of a
//! room after a newer one from this process. A report that cannot be delivered is tried a few
//! times and then dropped: the next one (at the latest the next heartbeat) carries the same state.

use std::time::Duration;

use rtc_proto::webhook::{self, RoomReport};
use tokio::sync::mpsc::UnboundedReceiver;

/// How long one delivery may take.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Waits between attempts at one report.
const RETRY_AFTER: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

pub(crate) async fn deliver(
    url: String,
    secret: Vec<u8>,
    mut reports: UnboundedReceiver<RoomReport>,
) {
    let client = match reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("rtc-sfu/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::error!("room reports are off: could not make an HTTP client: {error}");
            return;
        }
    };
    while let Some(report) = reports.recv().await {
        // A newer report of the same room makes an older one still queued pointless.
        let mut report = report;
        while let Ok(next) = reports.try_recv() {
            if next.room == report.room {
                report = next;
            } else {
                send(&client, &url, &secret, &report).await;
                report = next;
            }
        }
        send(&client, &url, &secret, &report).await;
    }
}

async fn send(client: &reqwest::Client, url: &str, secret: &[u8], report: &RoomReport) {
    let body = match serde_json::to_vec(report) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!("could not encode a room report: {error}");
            return;
        }
    };
    let signature = match webhook::sign(&body, secret) {
        Ok(signature) => signature,
        Err(error) => {
            tracing::error!("could not sign a room report: {error}");
            return;
        }
    };
    let mut waits = RETRY_AFTER.iter();
    loop {
        let result = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(webhook::SIGNATURE_HEADER, &signature)
            .body(body.clone())
            .send()
            .await;
        let failure = match result {
            Ok(response) if response.status().is_success() => return,
            // Refused: sending it again will not change the answer.
            Ok(response) if response.status().is_client_error() => {
                tracing::warn!(
                    room = report.room,
                    status = %response.status(),
                    "the host refused a room report"
                );
                return;
            }
            Ok(response) => format!("the host answered {}", response.status()),
            Err(error) => error.to_string(),
        };
        let Some(wait) = waits.next() else {
            tracing::warn!(
                room = report.room,
                "a room report was not delivered: {failure}"
            );
            return;
        };
        tracing::debug!(room = report.room, "retrying a room report: {failure}");
        tokio::time::sleep(*wait).await;
    }
}
