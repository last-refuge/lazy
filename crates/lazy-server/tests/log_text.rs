//! What the server logs, as text lines (see `common/log_scenario.rs`).

mod common;
#[path = "common/log_scenario.rs"]
mod log_scenario;

use common::*;
use lazy_server::logging::LogFormat;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[sqlx::test]
async fn nothing_secret_and_no_content_reaches_the_text_log(
    pool_options: PgPoolOptions,
    connect_options: PgConnectOptions,
) {
    log_scenario::run(pool_options, connect_options, LogFormat::Text).await;
}
