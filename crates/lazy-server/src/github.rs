//! The GitHub connector (llm-docs/DECISIONS.md, D32): a workspace's own GitHub App, created from
//! Lazy, that posts a repository's activity into the channels that follow it.
//!
//! # Connecting
//!
//! An admin asks to connect ([`start_setup`]) and opens the one-time address it returns. That page
//! ([`setup_page`]) sends their browser to GitHub with a **manifest** describing the app: owned by
//! the organization, installable only there, reading pull requests, issues, contents and actions,
//! subscribed to the events below, with its webhook pointing back here. The admin confirms on
//! GitHub, which redirects to [`callback`] with a code; the server trades it
//! (`POST /app-manifests/{code}/conversions`) for the app's id, private key, webhook secret and
//! client secret, seals the secrets as credentials (D31), creates the app's bot, and sends the
//! browser on to install the app. Installations are recorded only from GitHub's signed
//! `installation` events, never from a redirect anyone could forge.
//!
//! # Receiving
//!
//! [`receive`] takes GitHub's webhook deliveries: the app is found by
//! `X-GitHub-Hook-Installation-Target-ID`, the body's `X-Hub-Signature-256` is checked with the
//! app's webhook secret in constant time, and each delivery the connector understands is reduced
//! to a short summary (its message text and where it applies, nothing more of the payload is kept)
//! and queued as a `github.event` job whose dedupe key is GitHub's delivery id, so a delivery
//! GitHub sends twice is handled once.
//!
//! # Posting
//!
//! [`handle_event`] posts the summary into every channel that follows the repository for that kind
//! of activity, as the app's bot, through the ordinary send. The operation id of each post is
//! derived from the subscription and the delivery, so a retried job never posts twice. A channel
//! that can no longer take it (archived, the bot removed) is skipped; a database failure retries.

use std::time::Duration;

use aws_lc_rs::hmac;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use lazy_domain::github::{GithubEvent, is_login, normalize_repository};
use lazy_domain::{
    ChannelName, ConversationId, ConversationKind, DisplayName, GithubSubscriptionId,
    MessageDocument, OperationId, Timestamp, UserId, Username, WorkspaceId,
};
use lazy_protocol::{
    CreateGithubSubscriptionRequest, ErrorCode, Event, GithubConnection, GithubDelivery,
    GithubDeliveryState, GithubInstallation, GithubStatusResponse, GithubSubscription,
    SendMessageRequest, User,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::access;
use crate::app::AppState;
use crate::conversations::{self, Lock};
use crate::credentials::{self, CredentialId, Secret};
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::groups;
use crate::jobs::{self, JobContext, NewJob, Outcome};
use crate::logging::Describe;
use crate::messages;

/// The job kind of a delivery to post.
pub const EVENT_JOB: &str = "github.event";
/// Largest webhook body read. GitHub caps deliveries at 25 MB; the events posted here are far
/// smaller, and a push of thousands of commits is summarized from its first few.
pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
/// How long a started connection's address works.
const SETUP_TTL_MINUTES: i64 = 60;
/// Credential kinds.
const PRIVATE_KEY: &str = "github.private_key";
const WEBHOOK_SECRET: &str = "github.webhook_secret";
const CLIENT_SECRET: &str = "github.client_secret";
/// Characters of a description or comment quoted in a message.
const EXCERPT_CHARS: usize = 280;
/// Commits listed for a push.
const COMMITS_LISTED: usize = 5;
/// Deliveries listed for admins.
const DELIVERIES_LISTED: i64 = 50;

/// Where GitHub is. Tests point these at a stand-in.
#[derive(Debug, Clone)]
pub struct GithubEndpoints {
    /// The REST API, e.g. `https://api.github.com`.
    pub api: String,
    /// The website, e.g. `https://github.com`.
    pub web: String,
}

impl Default for GithubEndpoints {
    fn default() -> Self {
        Self {
            api: "https://api.github.com".to_owned(),
            web: "https://github.com".to_owned(),
        }
    }
}

/// The HTTP client the server talks to GitHub with: GitHub's address only, a short timeout.
///
/// `None` (logged) when it cannot be built, typically because the machine has no CA certificates:
/// the server runs on and GitHub reports itself unavailable, since a client that cannot verify
/// GitHub must not be used and building a default one would panic for the same reason.
pub fn http_client() -> Option<reqwest::Client> {
    match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("lazy-server/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => Some(client),
        Err(_) => {
            tracing::warn!(
                "no HTTPS client (are CA certificates installed?): GitHub is unavailable"
            );
            None
        }
    }
}

/// Whether this server can connect GitHub: it can seal credentials and reach GitHub.
fn available(state: &AppState) -> bool {
    state.integrations.credentials.enabled() && state.integrations.http.is_some()
}

fn manage_error() -> AppError {
    AppError::forbidden("only admins and owners manage integrations")
}

async fn require_admin(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    actor: UserId,
) -> Result<(), AppError> {
    let role = access::require_member(conn, workspace, actor).await?;
    if role.can_manage_integrations() {
        Ok(())
    } else {
        Err(manage_error())
    }
}

/// Markdown-escapes text from GitHub (titles, names, comments), so it shows as written and can
/// neither format nor link.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A description or comment written in GitHub's Markdown, as plain lines: what a reader sees on
/// GitHub, without the syntax (which would otherwise show as typed, since it is escaped). Code
/// fences, HTML comments (pull request templates), and empty lines are dropped; list items get a
/// bullet; headings, quote markers, emphasis, code ticks, and link syntax are removed.
fn plain_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_comment = false;
    for raw in text.lines() {
        let mut line = raw.trim().to_owned();
        // HTML comments, possibly across lines.
        loop {
            if in_comment {
                match line.find("-->") {
                    Some(end) => {
                        line.replace_range(..end + 3, "");
                        in_comment = false;
                    }
                    None => {
                        line.clear();
                        break;
                    }
                }
            }
            match line.find("<!--") {
                Some(start) => {
                    let rest = line[start..].to_owned();
                    line.truncate(start);
                    match rest.find("-->") {
                        Some(end) => line.push_str(&rest[end + 3..]),
                        None => {
                            in_comment = true;
                            break;
                        }
                    }
                }
                None => break,
            }
        }
        let mut line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            continue;
        }
        while let Some(rest) = line.strip_prefix('>') {
            line = rest.trim_start();
        }
        let heading = line.trim_start_matches('#');
        if heading.len() < line.len() && (heading.is_empty() || heading.starts_with(' ')) {
            line = heading.trim_start();
        }
        let mut bullet = false;
        for marker in ["- ", "* ", "+ "] {
            if let Some(rest) = line.strip_prefix(marker) {
                line = ["[ ] ", "[x] ", "[X] "]
                    .iter()
                    .find_map(|check| rest.strip_prefix(check))
                    .unwrap_or(rest);
                bullet = true;
                break;
            }
        }
        let text = inline_plain(line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        out.push(if bullet {
            format!("• {text}")
        } else {
            text.to_owned()
        });
    }
    out
}

/// Inline Markdown as its text: `[text](url)` and `![alt](url)` become their text, and code
/// ticks and emphasis markers (`**`, `__`, `~~`) are dropped. Single `*` and `_` stay, since
/// they are as often part of a word or an expression.
fn inline_plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        let bang = rest.starts_with("![");
        if bang || rest.starts_with('[') {
            let open = if bang { 2 } else { 1 };
            if let Some(close) = rest[open..].find("](")
                && let Some(end) = rest[open + close + 2..].find(')')
            {
                out.push_str(&rest[open..open + close]);
                rest = &rest[open + close + 2 + end + 1..];
                continue;
            }
        }
        if let Some(after) = ["**", "__", "~~"].iter().find_map(|m| rest.strip_prefix(m)) {
            rest = after;
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        if c != '`' {
            out.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// The first `EXCERPT_CHARS` characters of a description or comment, as plain text (see
/// [`plain_lines`]) in a quote of at most three lines, or nothing if it is empty.
fn excerpt(text: &str) -> Option<String> {
    let plain = plain_lines(text).join("\n");
    let text = plain.trim();
    if text.is_empty() {
        return None;
    }
    let mut short: String = text.chars().take(EXCERPT_CHARS).collect();
    let cut = short.len() < text.len();
    let lines: Vec<&str> = short
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect();
    let more = cut || text.lines().filter(|l| !l.trim().is_empty()).count() > lines.len();
    short = lines
        .iter()
        .map(|line| format!("> {}", escape(line)))
        .collect::<Vec<_>>()
        .join("\n");
    if more {
        short.push_str(" …");
    }
    Some(short)
}

// ----- Connecting ---------------------------------------------------------------------------

fn state_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn new_state_token() -> Result<String, AppError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| AppError::internal(format!("no randomness: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Starts connecting GitHub for `workspace` in `organization`: returns the address to open.
pub async fn start_setup(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
    organization: &str,
) -> Result<String, AppError> {
    let mut conn = state.pool.acquire().await?;
    require_admin(&mut conn, workspace, actor).await?;
    if !state.integrations.credentials.enabled() {
        return Err(AppError::new(
            ErrorCode::Unavailable,
            "GitHub needs this server's credentials key (LAZY_CREDENTIALS_KEY); ask whoever runs \
             it to set one",
        ));
    }
    if state.integrations.http.is_none() {
        return Err(AppError::new(
            ErrorCode::Unavailable,
            "this server cannot make HTTPS requests (no CA certificates); ask whoever runs it",
        ));
    }
    let organization = organization.trim();
    if !is_login(organization) {
        return Err(AppError::invalid("that is not a GitHub organization name"));
    }
    let connected: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM github_apps WHERE workspace_id = $1 AND disconnected_at IS NULL)",
    )
    .bind(workspace.as_uuid())
    .fetch_one(&mut *conn)
    .await?;
    if connected {
        return Err(AppError::conflict(
            "GitHub is already connected; disconnect it first to connect another app",
        ));
    }
    let token = new_state_token()?;
    sqlx::query(
        "INSERT INTO github_setups (state_hash, workspace_id, user_id, organization, expires_at) \
         VALUES ($1, $2, $3, $4, now() + make_interval(mins => $5))",
    )
    .bind(&state_hash(&token)[..])
    .bind(workspace.as_uuid())
    .bind(actor.as_uuid())
    .bind(organization)
    .bind(i32::try_from(SETUP_TTL_MINUTES).unwrap_or(60))
    .execute(&mut *conn)
    .await?;
    Ok(format!(
        "{}/github/setup/{token}",
        state.integrations.public_url
    ))
}

/// What GitHub is asked to create.
fn manifest(public_url: &str) -> Value {
    let host = public_url
        .split("://")
        .nth(1)
        .unwrap_or(public_url)
        .split('/')
        .next()
        .unwrap_or("Lazy");
    // GitHub app names are unique across GitHub and at most 34 characters; the admin can edit it
    // on GitHub's page if this one is taken.
    let mut name = format!("Lazy {host}");
    name.truncate(34);
    json!({
        "name": name,
        "url": public_url,
        "description": "Posts this organization's GitHub activity into Lazy channels.",
        "hook_attributes": { "url": format!("{public_url}/github/webhook"), "active": true },
        "redirect_url": format!("{public_url}/github/callback"),
        "setup_url": format!("{public_url}/github/installed"),
        "setup_on_update": false,
        "public": false,
        "default_permissions": {
            "metadata": "read",
            "contents": "read",
            "pull_requests": "read",
            "issues": "read",
            "actions": "read"
        },
        "default_events": [
            "pull_request", "pull_request_review", "issues", "issue_comment", "push", "release",
            "workflow_run"
        ]
    })
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A plain page for the admin's browser.
fn page(status: StatusCode, title: &str, body_html: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width\">\
         <meta name=\"referrer\" content=\"no-referrer\"><title>{title}</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:36rem;margin:4rem auto;\
         padding:0 1rem;color:#1d3a2f;background:#f7f4ec;line-height:1.5}}\
         button{{font:inherit;padding:.6rem 1.2rem;border-radius:999px;border:0;\
         background:#1d3a2f;color:#f7f4ec;cursor:pointer}}</style></head>\
         <body><h1>{title}</h1>{body_html}</body></html>",
        title = html_escape(title)
    );
    (status, Html(html)).into_response()
}

#[derive(sqlx::FromRow)]
struct SetupRow {
    organization: String,
    expired: bool,
    used: bool,
}

async fn find_setup(conn: &mut PgConnection, token: &str) -> Result<Option<SetupRow>, AppError> {
    Ok(sqlx::query_as(
        "SELECT organization, expires_at < now() AS expired, \
                used_at IS NOT NULL AS used \
         FROM github_setups WHERE state_hash = $1",
    )
    .bind(&state_hash(token)[..])
    .fetch_optional(conn)
    .await?)
}

fn link_gone() -> Response {
    page(
        StatusCode::NOT_FOUND,
        "This link has expired",
        "<p>Connecting GitHub starts from Lazy: open <b>Integrations</b>, choose \
         <b>Connect GitHub</b>, and use the new link within an hour.</p>",
    )
}

/// `GET /github/setup/{state}`: sends the browser to GitHub with the app's manifest.
pub async fn setup_page(state: &AppState, token: &str) -> Response {
    let found = match state.pool.acquire().await {
        Ok(mut conn) => find_setup(&mut conn, token).await,
        Err(error) => Err(error.into()),
    };
    let setup = match found {
        Ok(Some(setup)) if !setup.expired && !setup.used => setup,
        Ok(_) => return link_gone(),
        Err(error) => return error.into_response(),
    };
    let action = format!(
        "{}/organizations/{}/settings/apps/new?state={}",
        state.integrations.github.web, setup.organization, token
    );
    let manifest = manifest(&state.integrations.public_url).to_string();
    let body = format!(
        "<p>Lazy will create a GitHub App owned by <b>{org}</b>. On the next page, check its name \
         (change it if GitHub says it is taken) and choose <b>Create GitHub App</b>. Then install \
         it on <b>{org}</b>.</p>\
         <form id=\"go\" method=\"post\" action=\"{action}\">\
         <input type=\"hidden\" name=\"manifest\" value=\"{manifest}\">\
         <button type=\"submit\">Continue to GitHub</button></form>\
         <script>document.getElementById('go').submit()</script>",
        org = html_escape(&setup.organization),
        action = html_escape(&action),
        manifest = html_escape(&manifest),
    );
    page(StatusCode::OK, "Connecting GitHub to Lazy", &body)
}

/// What GitHub answers when a manifest code is converted.
#[derive(Deserialize)]
struct Conversion {
    id: i64,
    slug: String,
    html_url: String,
    owner: Owner,
    pem: String,
    webhook_secret: Option<String>,
    client_secret: Option<String>,
}

#[derive(Deserialize)]
struct Owner {
    login: String,
}

/// `GET /github/callback?code=..&state=..`: GitHub created the app; keep its secrets and send the
/// browser on to install it.
pub async fn callback(state: &AppState, code: Option<&str>, token: Option<&str>) -> Response {
    let (Some(code), Some(token)) = (code, token) else {
        return link_gone();
    };
    match finish_setup(state, code, token).await {
        Ok(Finished::Created { install_url }) => Redirect::to(&install_url).into_response(),
        Ok(Finished::Gone) => link_gone(),
        Ok(Finished::AlreadyConnected) => page(
            StatusCode::CONFLICT,
            "GitHub is already connected",
            "<p>This workspace already has a GitHub App. The one just created on GitHub is not \
             used: delete it in the organization's settings, under Developer settings, GitHub \
             Apps.</p>",
        ),
        Err(error) => {
            tracing::warn!(error = %error.describe(), "finishing a GitHub connection failed");
            page(
                StatusCode::BAD_GATEWAY,
                "GitHub could not be connected",
                "<p>Lazy could not get the new app's details from GitHub. Start again from Lazy \
                 (<b>Integrations</b>, <b>Connect GitHub</b>). If GitHub created an app anyway, \
                 delete it in the organization's settings first.</p>",
            )
        }
    }
}

enum Finished {
    Created { install_url: String },
    Gone,
    AlreadyConnected,
}

async fn finish_setup(state: &AppState, code: &str, token: &str) -> Result<Finished, AppError> {
    // The state is used once: claim it before anything else.
    let mut tx = state.pool.begin().await?;
    let claimed: Option<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE github_setups SET used_at = now() \
         WHERE state_hash = $1 AND used_at IS NULL AND expires_at > now() \
         RETURNING workspace_id, user_id",
    )
    .bind(&state_hash(token)[..])
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let Some((workspace, user)) = claimed else {
        return Ok(Finished::Gone);
    };
    let (workspace, user) = (WorkspaceId::from_uuid(workspace), UserId::from_uuid(user));
    if code.is_empty() || code.len() > 200 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Ok(Finished::Gone);
    }
    let url = format!(
        "{}/app-manifests/{code}/conversions",
        state.integrations.github.api
    );
    let Some(http) = &state.integrations.http else {
        return Err(AppError::internal("this server cannot make HTTPS requests"));
    };
    let response = http
        .post(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|e| AppError::internal(format!("GitHub did not answer: {}", e.without_url())))?;
    if !response.status().is_success() {
        return Err(AppError::internal(format!(
            "GitHub refused the conversion with status {}",
            response.status().as_u16()
        )));
    }
    let conversion: Conversion = response
        .json()
        .await
        .map_err(|_| AppError::internal("GitHub's answer could not be read"))?;

    let keys = &state.integrations.credentials;
    let mut tx = state.pool.begin().await?;
    // One live app per workspace; the workspace row lock orders two finishing at once.
    sqlx::query("SELECT 1 FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.as_uuid())
        .execute(&mut *tx)
        .await?;
    let live: Option<Option<OffsetDateTime>> =
        sqlx::query_scalar("SELECT disconnected_at FROM github_apps WHERE workspace_id = $1")
            .bind(workspace.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
    match live {
        Some(None) => return Ok(Finished::AlreadyConnected),
        Some(Some(_)) => {
            sqlx::query("DELETE FROM github_apps WHERE workspace_id = $1")
                .bind(workspace.as_uuid())
                .execute(&mut *tx)
                .await?;
        }
        None => {}
    }
    let private_key = credentials::store(
        &mut tx,
        keys,
        workspace,
        PRIVATE_KEY,
        &Secret::from_text(&conversion.pem),
    )
    .await?;
    let webhook_secret = match conversion.webhook_secret.as_deref() {
        Some(secret) => Some(
            credentials::store(
                &mut tx,
                keys,
                workspace,
                WEBHOOK_SECRET,
                &Secret::from_text(secret),
            )
            .await?,
        ),
        None => None,
    };
    let client_secret = match conversion.client_secret.as_deref() {
        Some(secret) => Some(
            credentials::store(
                &mut tx,
                keys,
                workspace,
                CLIENT_SECRET,
                &Secret::from_text(secret),
            )
            .await?,
        ),
        None => None,
    };
    let bot = create_bot(&mut tx, workspace).await?;
    sqlx::query(
        "INSERT INTO github_apps (workspace_id, app_id, slug, owner_login, html_url, bot_user_id, \
                                  private_key_id, webhook_secret_id, client_secret_id, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(workspace.as_uuid())
    .bind(conversion.id)
    .bind(&conversion.slug)
    .bind(&conversion.owner.login)
    .bind(&conversion.html_url)
    .bind(bot.id.as_uuid())
    .bind(private_key.0)
    .bind(webhook_secret.map(|c| c.0))
    .bind(client_secret.map(|c| c.0))
    .bind(user.as_uuid())
    .execute(&mut *tx)
    .await?;
    events::append_all(
        &mut tx,
        workspace,
        vec![PendingEvent {
            kind: "member_joined",
            conversation_id: None,
            operation_id: None,
            audience: Audience::Workspace,
            event: Event::MemberJoined { user: bot },
        }],
    )
    .await?;
    tx.commit().await?;
    tracing::info!(
        workspace_id = %workspace,
        github_app_id = conversion.id,
        "GitHub app connected"
    );
    Ok(Finished::Created {
        install_url: format!("{}/installations/new", conversion.html_url),
    })
}

/// Creates the bot that posts GitHub's activity: a workspace member named "GitHub" that cannot
/// sign in, like a webhook's bot (D16).
async fn create_bot(conn: &mut PgConnection, workspace: WorkspaceId) -> Result<User, AppError> {
    for _ in 0..5 {
        let id = UserId::generate();
        let simple = Uuid::new_v4().simple().to_string();
        let username = format!("bot-{}", &simple[simple.len() - 8..]);
        groups::lock_handle(conn, &username).await?;
        let taken: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE username = $1)")
                .bind(&username)
                .fetch_one(&mut *conn)
                .await?;
        if taken || groups::handle_names_a_group(conn, &username).await? {
            continue;
        }
        let display = DisplayName::parse("GitHub")
            .map_err(|_| AppError::internal("the bot's name is invalid"))?;
        sqlx::query(
            "INSERT INTO users (id, username, display_name, password_hash, is_bot) \
             VALUES ($1, $2, $3, '!', true)",
        )
        .bind(id.as_uuid())
        .bind(&username)
        .bind(display.as_str())
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'member')",
        )
        .bind(workspace.as_uuid())
        .bind(id.as_uuid())
        .execute(&mut *conn)
        .await?;
        return Ok(User {
            id,
            username: Username::parse(&username)
                .map_err(|_| AppError::internal("generated username is invalid"))?,
            display_name: display,
            is_bot: true,
            status: None,
            deactivated: false,
            avatar_version: 0,
        });
    }
    Err(AppError::internal("no free username for the GitHub bot"))
}

/// `GET /github/installed`: where GitHub sends the browser after installing.
pub fn installed_page() -> Response {
    page(
        StatusCode::OK,
        "GitHub is connected",
        "<p>You can close this tab. In Lazy, open <b>Integrations</b>, <b>GitHub</b>, and choose \
         which channels follow which repositories.</p>",
    )
}

// ----- Receiving ----------------------------------------------------------------------------

/// The live app GitHub addressed, and its webhook secret.
#[derive(sqlx::FromRow)]
struct AppRow {
    workspace_id: Uuid,
    app_id: i64,
    webhook_secret_id: Option<Uuid>,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn plain(status: StatusCode, text: &'static str) -> Response {
    (status, text).into_response()
}

/// Checks `X-Hub-Signature-256: sha256=<hex>` against the body in constant time.
pub fn signature_ok(secret: &[u8], body: &[u8], signature: Option<&str>) -> bool {
    let Some(hex) = signature.and_then(|s| s.strip_prefix("sha256=")) else {
        return false;
    };
    if hex.len() != 64 {
        return false;
    }
    let mut tag = [0u8; 32];
    for (i, byte) in tag.iter_mut().enumerate() {
        match u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16) {
            Ok(value) => *byte = value,
            Err(_) => return false,
        }
    }
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::verify(&key, body, &tag).is_ok()
}

/// `POST /github/webhook`: one delivery from GitHub.
pub async fn receive(state: &AppState, headers: &HeaderMap, body: &[u8]) -> Response {
    match receive_inner(state, headers, body).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn receive_inner(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, AppError> {
    let Some(app_id) =
        header(headers, "x-github-hook-installation-target-id").and_then(|v| v.parse::<i64>().ok())
    else {
        return Ok(plain(StatusCode::NOT_FOUND, "unknown app"));
    };
    let mut conn = state.pool.acquire().await?;
    let app: Option<AppRow> = sqlx::query_as(
        "SELECT workspace_id, app_id, webhook_secret_id FROM github_apps \
         WHERE app_id = $1 AND disconnected_at IS NULL",
    )
    .bind(app_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(app) = app else {
        return Ok(plain(StatusCode::NOT_FOUND, "unknown app"));
    };
    let Some(secret_id) = app.webhook_secret_id else {
        return Ok(plain(StatusCode::UNAUTHORIZED, "no webhook secret"));
    };
    let secret = credentials::load(
        &mut conn,
        &state.integrations.credentials,
        CredentialId(secret_id),
    )
    .await?;
    if !signature_ok(
        secret.expose(),
        body,
        header(headers, "x-hub-signature-256"),
    ) {
        tracing::warn!(
            github_app_id = app_id,
            "a GitHub delivery's signature did not match"
        );
        return Ok(plain(StatusCode::UNAUTHORIZED, "signature does not match"));
    }
    drop(secret);
    let workspace = WorkspaceId::from_uuid(app.workspace_id);
    let event = header(headers, "x-github-event").unwrap_or_default();
    let delivery = header(headers, "x-github-delivery").unwrap_or_default();
    let payload: Value =
        serde_json::from_slice(body).map_err(|_| AppError::invalid("the body must be JSON"))?;
    match event {
        "ping" => return Ok(plain(StatusCode::OK, "pong")),
        "installation" | "installation_repositories" => {
            record_installation(&mut conn, workspace, app.app_id, event, &payload).await?;
            return Ok(plain(StatusCode::OK, "recorded"));
        }
        _ => {}
    }
    let Some(summary) = summarize(event, &payload) else {
        return Ok(plain(StatusCode::OK, "ignored"));
    };
    if delivery.is_empty() || delivery.len() > 100 {
        return Err(AppError::invalid("X-GitHub-Delivery is missing"));
    }
    let job = NewJob::new(
        EVENT_JOB,
        serde_json::to_value(&summary).unwrap_or(Value::Null),
    )
    .workspace(workspace)
    .dedupe(delivery)
    .max_attempts(6);
    let enqueued = jobs::enqueue(&mut conn, job).await?;
    if enqueued.created {
        state.integrations.jobs.wake();
        Ok(plain(StatusCode::ACCEPTED, "queued"))
    } else {
        Ok(plain(StatusCode::OK, "already received"))
    }
}

async fn record_installation(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    app_id: i64,
    event: &str,
    payload: &Value,
) -> Result<(), AppError> {
    let action = payload["action"].as_str().unwrap_or_default();
    let installation = &payload["installation"];
    let Some(id) = installation["id"].as_i64() else {
        return Err(AppError::invalid("the installation has no id"));
    };
    let account = installation["account"]["login"]
        .as_str()
        .unwrap_or_default();
    let selection = installation["repository_selection"]
        .as_str()
        .unwrap_or("selected");
    if event == "installation" && action == "deleted" {
        sqlx::query(
            "UPDATE github_installations SET removed_at = now(), updated_at = now() \
             WHERE installation_id = $1 AND workspace_id = $2",
        )
        .bind(id)
        .bind(workspace.as_uuid())
        .execute(conn)
        .await?;
        return Ok(());
    }
    let suspended = match (event, action) {
        ("installation", "suspend") => Some(true),
        ("installation", "unsuspend") => Some(false),
        _ => None,
    };
    sqlx::query(
        "INSERT INTO github_installations \
             (installation_id, workspace_id, app_id, account_login, repository_selection) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (installation_id) DO UPDATE SET \
             account_login = EXCLUDED.account_login, \
             repository_selection = EXCLUDED.repository_selection, \
             removed_at = NULL, updated_at = now(), \
             suspended_at = CASE WHEN $6::boolean IS NULL THEN github_installations.suspended_at \
                                 WHEN $6 THEN now() ELSE NULL END \
         WHERE github_installations.workspace_id = EXCLUDED.workspace_id",
    )
    .bind(id)
    .bind(workspace.as_uuid())
    .bind(app_id)
    .bind(account)
    .bind(selection)
    .bind(suspended)
    .execute(conn)
    .await?;
    Ok(())
}

/// What is kept of a delivery: where it applies and the message to post. Nothing else of
/// GitHub's payload is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub event: String,
    pub action: Option<String>,
    /// `owner/name`, lower case.
    pub repository: String,
    pub kind: GithubEvent,
    /// Markdown, with everything from GitHub escaped.
    pub text: String,
}

fn s<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    path.iter()
        .fold(value, |v, key| &v[*key])
        .as_str()
        .unwrap_or_default()
}

/// Reduces a delivery to its summary, or `None` for events and actions that post nothing.
pub fn summarize(event: &str, payload: &Value) -> Option<Summary> {
    let action = payload["action"].as_str().map(str::to_owned);
    let repo_name = s(payload, &["repository", "full_name"]);
    let repository = normalize_repository(repo_name)?;
    // The repository is named, not linked: the item itself is the one link on the line, so a
    // channel of activity is not a wall of underlines.
    let repo_link = escape(repo_name);
    let actor = escape(s(payload, &["sender", "login"]));
    let act = action.as_deref().unwrap_or_default();
    let (kind, text) = match event {
        "pull_request" => {
            let pr = &payload["pull_request"];
            let verb = match act {
                "opened" if pr["draft"].as_bool() == Some(true) => "opened a draft of",
                "opened" => "opened",
                "reopened" => "reopened",
                "ready_for_review" => "marked ready for review",
                "closed" if pr["merged"].as_bool() == Some(true) => "merged",
                "closed" => "closed",
                _ => return None,
            };
            let mut text = format!(
                "**{actor}** {verb} pull request [#{} {}]({}) in {repo_link}",
                pr["number"].as_i64().unwrap_or_default(),
                escape(s(pr, &["title"])),
                s(pr, &["html_url"]),
            );
            if act == "opened"
                && let Some(quote) = excerpt(s(pr, &["body"]))
            {
                text.push_str("\n\n");
                text.push_str(&quote);
            }
            (GithubEvent::PullRequest, text)
        }
        "pull_request_review" => {
            if act != "submitted" {
                return None;
            }
            let review = &payload["review"];
            let pr = &payload["pull_request"];
            let verb = match s(review, &["state"]) {
                "approved" => "approved",
                "changes_requested" => "requested changes on",
                "commented" => "reviewed",
                _ => return None,
            };
            let mut text = format!(
                "**{actor}** {verb} [#{} {}]({}) in {repo_link}",
                pr["number"].as_i64().unwrap_or_default(),
                escape(s(pr, &["title"])),
                s(review, &["html_url"]),
            );
            if let Some(quote) = excerpt(s(review, &["body"])) {
                text.push_str("\n\n");
                text.push_str(&quote);
            }
            (GithubEvent::Review, text)
        }
        "issues" => {
            let issue = &payload["issue"];
            let verb = match act {
                "opened" => "opened",
                "reopened" => "reopened",
                "closed" if s(issue, &["state_reason"]) == "not_planned" => "closed as not planned",
                "closed" => "closed",
                _ => return None,
            };
            let mut text = format!(
                "**{actor}** {verb} issue [#{} {}]({}) in {repo_link}",
                issue["number"].as_i64().unwrap_or_default(),
                escape(s(issue, &["title"])),
                s(issue, &["html_url"]),
            );
            if act == "opened"
                && let Some(quote) = excerpt(s(issue, &["body"]))
            {
                text.push_str("\n\n");
                text.push_str(&quote);
            }
            (GithubEvent::Issues, text)
        }
        "issue_comment" => {
            if act != "created" {
                return None;
            }
            let issue = &payload["issue"];
            let what = if issue["pull_request"].is_object() {
                "pull request"
            } else {
                "issue"
            };
            let mut text = format!(
                "**{actor}** commented on {what} [#{} {}]({}) in {repo_link}",
                issue["number"].as_i64().unwrap_or_default(),
                escape(s(issue, &["title"])),
                s(payload, &["comment", "html_url"]),
            );
            if let Some(quote) = excerpt(s(payload, &["comment", "body"])) {
                text.push_str("\n\n");
                text.push_str(&quote);
            }
            (GithubEvent::Comments, text)
        }
        "push" => {
            let default = s(payload, &["repository", "default_branch"]);
            let branch = s(payload, &["ref"]).strip_prefix("refs/heads/")?;
            if branch != default || payload["deleted"].as_bool() == Some(true) {
                return None;
            }
            let commits = payload["commits"].as_array()?;
            if commits.is_empty() {
                return None;
            }
            let pusher = escape(s(payload, &["pusher", "name"]));
            let who = if pusher.is_empty() {
                actor.clone()
            } else {
                pusher
            };
            let count = commits.len();
            let mut text = format!(
                "**{who}** pushed [{count} commit{}]({}) to **{}** in {repo_link}",
                if count == 1 { "" } else { "s" },
                s(payload, &["compare"]),
                escape(branch),
            );
            for commit in commits.iter().take(COMMITS_LISTED) {
                let id = s(commit, &["id"]);
                let first = s(commit, &["message"]).lines().next().unwrap_or_default();
                // A link cannot hold code, so the short id is plain.
                text.push_str(&format!(
                    "\n- [{}]({}) {}",
                    id.get(..7).unwrap_or(id),
                    s(commit, &["url"]),
                    escape(first),
                ));
            }
            if count > COMMITS_LISTED {
                text.push_str(&format!("\n- and {} more", count - COMMITS_LISTED));
            }
            (GithubEvent::Push, text)
        }
        "release" => {
            if act != "published" {
                return None;
            }
            let release = &payload["release"];
            let name = match s(release, &["name"]) {
                "" => s(release, &["tag_name"]),
                name => name,
            };
            let kind = if release["prerelease"].as_bool() == Some(true) {
                "pre-release"
            } else {
                "release"
            };
            (
                GithubEvent::Releases,
                format!(
                    "**{actor}** published {kind} [{}]({}) in {repo_link}",
                    escape(name),
                    s(release, &["html_url"])
                ),
            )
        }
        "workflow_run" => {
            if act != "completed" {
                return None;
            }
            let run = &payload["workflow_run"];
            let verb = match s(run, &["conclusion"]) {
                "failure" => "failed",
                "timed_out" => "timed out",
                "startup_failure" => "could not start",
                _ => return None,
            };
            (
                GithubEvent::Ci,
                format!(
                    "Workflow [{} #{}]({}) {verb} on **{}** in {repo_link}",
                    escape(s(run, &["name"])),
                    run["run_number"].as_i64().unwrap_or_default(),
                    s(run, &["html_url"]),
                    escape(s(run, &["head_branch"])),
                ),
            )
        }
        _ => return None,
    };
    Some(Summary {
        event: event.to_owned(),
        action,
        repository,
        kind,
        text,
    })
}

// ----- Posting ------------------------------------------------------------------------------

/// Posts one delivery into every channel that follows it. The job's handler.
pub async fn handle_event(ctx: JobContext) -> Outcome {
    let Some(workspace) = ctx.workspace else {
        return Outcome::fail("the delivery has no workspace");
    };
    let summary: Summary = match serde_json::from_value(ctx.payload.clone()) {
        Ok(summary) => summary,
        Err(_) => return Outcome::fail("the stored delivery could not be read"),
    };
    let delivery = ctx.id;
    match post(&ctx.pool, workspace, delivery, &summary).await {
        Ok(()) => Outcome::Done,
        Err(PostError::Disconnected) => Outcome::fail("GitHub is no longer connected"),
        Err(PostError::Retry(error)) => Outcome::retry(error),
    }
}

enum PostError {
    Disconnected,
    Retry(String),
}

impl From<AppError> for PostError {
    fn from(error: AppError) -> Self {
        Self::Retry(format!("the database failed ({})", error.describe()))
    }
}

async fn post(
    pool: &PgPool,
    workspace: WorkspaceId,
    delivery: i64,
    summary: &Summary,
) -> Result<(), PostError> {
    let bot: Option<Uuid> = sqlx::query_scalar(
        "SELECT bot_user_id FROM github_apps WHERE workspace_id = $1 AND disconnected_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .fetch_optional(pool)
    .await
    .map_err(AppError::from)?;
    let Some(bot) = bot.map(UserId::from_uuid) else {
        return Err(PostError::Disconnected);
    };
    let targets: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, conversation_id FROM github_subscriptions \
         WHERE workspace_id = $1 AND repository = $2 AND $3 = ANY(events) ORDER BY id",
    )
    .bind(workspace.as_uuid())
    .bind(&summary.repository)
    .bind(summary.kind.as_str())
    .fetch_all(pool)
    .await
    .map_err(AppError::from)?;
    let document = MessageDocument::from_markdown(&summary.text)
        .map_err(|_| PostError::Retry("the message could not be made".to_owned()))?;
    let mut failure = None;
    for (subscription, conversation) in targets {
        let request = SendMessageRequest {
            // The same delivery to the same subscription is the same message, however often the
            // job runs.
            operation_id: OperationId::from_uuid(Uuid::new_v5(
                &subscription,
                delivery.to_string().as_bytes(),
            )),
            document: document.clone(),
            thread_root: None,
            also_in_channel: false,
            attachment_ids: Vec::new(),
        };
        match messages::send_message(pool, bot, ConversationId::from_uuid(conversation), request)
            .await
        {
            Ok(_) => {}
            // The channel can no longer take it (archived, the bot was removed, it is gone):
            // nothing to retry.
            Err(AppError::Api(api))
                if matches!(
                    api.code,
                    ErrorCode::Forbidden | ErrorCode::NotFound | ErrorCode::Conflict
                ) => {}
            Err(other) => failure = Some(other),
        }
    }
    match failure {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

// ----- Managing -----------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct ConnectionRow {
    app_id: i64,
    slug: String,
    owner_login: String,
    html_url: String,
    bot_user_id: Uuid,
    created_by: Uuid,
    created_at: OffsetDateTime,
}

/// The workspace's GitHub connection, installations, subscriptions it can see, and recent
/// deliveries. Admins and owners only.
pub async fn status(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
) -> Result<GithubStatusResponse, AppError> {
    let mut conn = state.pool.acquire().await?;
    require_admin(&mut conn, workspace, actor).await?;
    let connection: Option<ConnectionRow> = sqlx::query_as(
        "SELECT app_id, slug, owner_login, html_url, bot_user_id, created_by, created_at \
         FROM github_apps WHERE workspace_id = $1 AND disconnected_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    let installations: Vec<(String, String, bool)> = match &connection {
        Some(app) => {
            sqlx::query_as(
                "SELECT account_login, repository_selection, suspended_at IS NOT NULL \
                 FROM github_installations \
                 WHERE workspace_id = $1 AND app_id = $2 AND removed_at IS NULL \
                 ORDER BY account_login",
            )
            .bind(workspace.as_uuid())
            .bind(app.app_id)
            .fetch_all(&mut *conn)
            .await?
        }
        None => Vec::new(),
    };
    let subscriptions = subscriptions_visible(&mut conn, workspace, actor).await?;
    let deliveries: Vec<(i64, Value, String, i32, Option<String>, OffsetDateTime)> =
        sqlx::query_as(
            "SELECT id, payload, state, attempts, last_error, created_at FROM jobs \
             WHERE workspace_id = $1 AND kind = $2 ORDER BY id DESC LIMIT $3",
        )
        .bind(workspace.as_uuid())
        .bind(EVENT_JOB)
        .bind(DELIVERIES_LISTED)
        .fetch_all(&mut *conn)
        .await?;
    Ok(GithubStatusResponse {
        available: available(state),
        connection: connection.map(|app| GithubConnection {
            app_slug: app.slug,
            html_url: app.html_url,
            owner: app.owner_login,
            bot_user_id: UserId::from_uuid(app.bot_user_id),
            created_by: UserId::from_uuid(app.created_by),
            created_at: Timestamp::from(app.created_at),
        }),
        installations: installations
            .into_iter()
            .map(|(account, selection, suspended)| GithubInstallation {
                account,
                all_repositories: selection == "all",
                suspended,
            })
            .collect(),
        subscriptions,
        deliveries: deliveries
            .into_iter()
            .map(
                |(id, payload, state, attempts, last_error, created_at)| GithubDelivery {
                    id,
                    event: s(&payload, &["event"]).to_owned(),
                    action: payload["action"].as_str().map(str::to_owned),
                    repository: payload["repository"].as_str().map(str::to_owned),
                    state: match state.as_str() {
                        "queued" => GithubDeliveryState::Queued,
                        "running" => GithubDeliveryState::Running,
                        "succeeded" => GithubDeliveryState::Posted,
                        "failed" => GithubDeliveryState::Failed,
                        "unknown" => GithubDeliveryState::Unknown,
                        _ => GithubDeliveryState::Other,
                    },
                    attempts: u32::try_from(attempts).unwrap_or(0),
                    last_error,
                    received_at: Timestamp::from(created_at),
                },
            )
            .collect(),
    })
}

#[derive(sqlx::FromRow)]
struct SubscriptionRow {
    id: Uuid,
    conversation_id: Uuid,
    channel_name: Option<String>,
    repository: String,
    events: Vec<String>,
    created_by: Uuid,
    created_at: OffsetDateTime,
}

impl SubscriptionRow {
    fn into_dto(self) -> Result<GithubSubscription, AppError> {
        Ok(GithubSubscription {
            id: GithubSubscriptionId::from_uuid(self.id),
            conversation_id: ConversationId::from_uuid(self.conversation_id),
            channel_name: self
                .channel_name
                .as_deref()
                .and_then(|name| ChannelName::parse(name).ok())
                .ok_or_else(|| AppError::internal("a subscription's channel has no name"))?,
            repository: self.repository,
            events: self
                .events
                .iter()
                .map(|name| GithubEvent::from_name(name).unwrap_or(GithubEvent::Unknown))
                .collect(),
            created_by: UserId::from_uuid(self.created_by),
            created_at: Timestamp::from(self.created_at),
        })
    }
}

async fn subscriptions_visible(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    actor: UserId,
) -> Result<Vec<GithubSubscription>, AppError> {
    let rows: Vec<SubscriptionRow> = sqlx::query_as(
        "SELECT s.id, s.conversation_id, c.name AS channel_name, s.repository, s.events, \
                s.created_by, s.created_at \
         FROM github_subscriptions s JOIN conversations c ON c.id = s.conversation_id \
         WHERE s.workspace_id = $1 \
           AND ((c.kind = 'channel' AND c.visibility = 'public') OR EXISTS \
                (SELECT 1 FROM conversation_members cm \
                 WHERE cm.conversation_id = c.id AND cm.user_id = $2)) \
         ORDER BY c.name, s.repository",
    )
    .bind(workspace.as_uuid())
    .bind(actor.as_uuid())
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(SubscriptionRow::into_dto).collect()
}

/// Subscribes a channel to a repository, or changes what it follows. The bot joins the channel.
pub async fn subscribe(
    state: &AppState,
    actor: UserId,
    conversation: ConversationId,
    request: CreateGithubSubscriptionRequest,
) -> Result<GithubSubscription, AppError> {
    let repository = normalize_repository(&request.repository).ok_or_else(|| {
        AppError::invalid("a repository is written owner/name, like last-refuge/lazy")
    })?;
    let mut events: Vec<GithubEvent> = request
        .events
        .into_iter()
        .filter(|event| *event != GithubEvent::Unknown)
        .collect();
    events.sort_unstable();
    events.dedup();
    if events.is_empty() {
        return Err(AppError::invalid(
            "choose at least one kind of activity to follow",
        ));
    }
    let (mut tx, loaded) =
        conversations::begin_visible(&state.pool, actor, conversation, Lock::Exclusive).await?;
    if !loaded
        .facts
        .workspace_role
        .is_some_and(|role| role.can_manage_integrations())
    {
        return Err(manage_error());
    }
    if loaded.facts.kind != ConversationKind::Channel {
        return Err(AppError::invalid("GitHub posts into channels only"));
    }
    if loaded.facts.archived {
        return Err(AppError::conflict("this channel is archived"));
    }
    let app: Option<(i64, Uuid)> = sqlx::query_as(
        "SELECT app_id, bot_user_id FROM github_apps \
         WHERE workspace_id = $1 AND disconnected_at IS NULL",
    )
    .bind(loaded.workspace_id.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((app_id, bot)) = app else {
        return Err(AppError::conflict(
            "connect GitHub before choosing repositories",
        ));
    };
    let owner = repository.split('/').next().unwrap_or_default();
    let installed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM github_installations WHERE workspace_id = $1 AND app_id = $2 \
                        AND lower(account_login) = $3 AND removed_at IS NULL)",
    )
    .bind(loaded.workspace_id.as_uuid())
    .bind(app_id)
    .bind(owner)
    .fetch_one(&mut *tx)
    .await?;
    if !installed {
        return Err(AppError::invalid(format!(
            "the GitHub app is not installed on {owner}; install it there first"
        )));
    }
    let names: Vec<&str> = events.iter().map(|event| event.as_str()).collect();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO github_subscriptions (id, workspace_id, conversation_id, repository, events, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (conversation_id, repository) DO UPDATE SET events = EXCLUDED.events \
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(loaded.workspace_id.as_uuid())
    .bind(conversation.as_uuid())
    .bind(&repository)
    .bind(&names)
    .bind(actor.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    let row: SubscriptionRow = sqlx::query_as(
        "SELECT s.id, s.conversation_id, c.name AS channel_name, s.repository, s.events, \
                s.created_by, s.created_at \
         FROM github_subscriptions s JOIN conversations c ON c.id = s.conversation_id \
         WHERE s.id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let bot = UserId::from_uuid(bot);
    let added = conversations::add_members(&mut tx, conversation, &[bot]).await?;
    if !added.is_empty() {
        let dto = conversations::conversation_dto(&mut tx, conversation).await?;
        events::append_all(
            &mut tx,
            loaded.workspace_id,
            vec![PendingEvent {
                kind: "member_added",
                conversation_id: Some(conversation),
                operation_id: None,
                audience: Audience::Members,
                event: Event::MemberAdded {
                    conversation: dto,
                    user_id: bot,
                },
            }],
        )
        .await?;
    }
    tx.commit().await?;
    row.into_dto()
}

/// Removes a subscription; the bot leaves the channel when nothing there follows GitHub any more.
/// Gone already is a quiet success; one in a channel the caller cannot read is `not_found`.
pub async fn unsubscribe(
    state: &AppState,
    actor: UserId,
    id: GithubSubscriptionId,
) -> Result<(), AppError> {
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT conversation_id FROM github_subscriptions WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(&state.pool)
            .await?;
    let Some(conversation) = found.map(ConversationId::from_uuid) else {
        return Ok(());
    };
    let (mut tx, loaded) =
        conversations::begin_visible(&state.pool, actor, conversation, Lock::Exclusive).await?;
    if !loaded
        .facts
        .workspace_role
        .is_some_and(|role| role.can_manage_integrations())
    {
        return Err(manage_error());
    }
    sqlx::query("DELETE FROM github_subscriptions WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
    let remaining: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM github_subscriptions WHERE conversation_id = $1)",
    )
    .bind(conversation.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    if !remaining {
        let bot: Option<Uuid> =
            sqlx::query_scalar("SELECT bot_user_id FROM github_apps WHERE workspace_id = $1")
                .bind(loaded.workspace_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(bot) = bot {
            conversations::remove_and_announce(&mut tx, &loaded, UserId::from_uuid(bot)).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// A connected app's bot and secrets, for disconnecting.
#[derive(sqlx::FromRow)]
struct LiveApp {
    bot_user_id: Uuid,
    private_key_id: Option<Uuid>,
    webhook_secret_id: Option<Uuid>,
    client_secret_id: Option<Uuid>,
}

/// Disconnects GitHub: the secrets are revoked, deliveries are refused from now on, the
/// subscriptions go, and the bot leaves every channel. The app itself stays on GitHub until an
/// organization owner deletes it there (GitHub has no API for that). Not connected is a quiet
/// success.
pub async fn disconnect(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
) -> Result<(), AppError> {
    {
        let mut conn = state.pool.acquire().await?;
        require_admin(&mut conn, workspace, actor).await?;
    }
    let mut tx = state.pool.begin().await?;
    let app: Option<LiveApp> = sqlx::query_as(
        "SELECT bot_user_id, private_key_id, webhook_secret_id, client_secret_id \
         FROM github_apps WHERE workspace_id = $1 AND disconnected_at IS NULL FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(LiveApp {
        bot_user_id: bot,
        private_key_id: key,
        webhook_secret_id: secret,
        client_secret_id: client,
    }) = app
    else {
        return Ok(());
    };
    for credential in [key, secret, client].into_iter().flatten() {
        credentials::revoke(&mut tx, CredentialId(credential)).await?;
    }
    sqlx::query("UPDATE github_apps SET disconnected_at = now() WHERE workspace_id = $1")
        .bind(workspace.as_uuid())
        .execute(&mut *tx)
        .await?;
    let channels: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM github_subscriptions WHERE workspace_id = $1 RETURNING conversation_id",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    // The bot leaves each channel it was in, in the channel's own transaction, loaded as the bot
    // (a member of every one of them, whether or not the admin can read it).
    let bot = UserId::from_uuid(bot);
    let mut channels = channels;
    channels.sort_unstable();
    channels.dedup();
    for channel in channels {
        let conversation = ConversationId::from_uuid(channel);
        let Ok((mut tx, loaded)) =
            conversations::begin_visible(&state.pool, bot, conversation, Lock::Exclusive).await
        else {
            continue;
        };
        conversations::remove_and_announce(&mut tx, &loaded, bot).await?;
        tx.commit().await?;
    }
    tracing::info!(workspace_id = %workspace, "GitHub disconnected");
    Ok(())
}

/// Runs a failed or unknown delivery again. Admins and owners of its workspace.
pub async fn retry_delivery(
    state: &AppState,
    actor: UserId,
    delivery: i64,
) -> Result<(), AppError> {
    let workspace: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT workspace_id FROM jobs WHERE id = $1 AND kind = $2")
            .bind(delivery)
            .bind(EVENT_JOB)
            .fetch_optional(&state.pool)
            .await?;
    let Some(Some(workspace)) = workspace else {
        return Err(AppError::not_found("delivery not found"));
    };
    {
        let mut conn = state.pool.acquire().await?;
        require_admin(&mut conn, WorkspaceId::from_uuid(workspace), actor)
            .await
            .map_err(|error| match error {
                AppError::Api(api) if api.code == ErrorCode::NotFound => {
                    AppError::not_found("delivery not found")
                }
                other => other,
            })?;
    }
    if jobs::retry(&state.pool, delivery).await? {
        state.integrations.jobs.wake();
        Ok(())
    } else {
        Err(AppError::conflict(
            "only a delivery that failed, or whose outcome is unknown, can be tried again",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_from_github_cannot_format_link_or_mention() {
        assert_eq!(
            escape("fix *all* the [things](x) @bob"),
            "fix \\*all\\* the \\[things\\]\\(x\\) \\@bob"
        );
        let doc =
            MessageDocument::from_markdown(&escape("**bold** `code` [a](https://x.y)")).unwrap();
        assert_eq!(doc.plain_text(), "**bold** `code` [a](https://x.y)");
    }

    #[test]
    fn excerpts_read_as_github_shows_them() {
        let body = "<!-- Describe the change -->\n## Summary\nUses `lazy-notify.yml` and \
                    **bold** [docs](https://x.y).\n\n- Deletes `a.yml`.\n* [ ] check\n```sh\nrm\n```";
        assert_eq!(
            plain_lines(body),
            [
                "Summary",
                "Uses lazy-notify.yml and bold docs.",
                "• Deletes a.yml.",
                "• check",
                "rm",
            ]
        );
        assert_eq!(plain_lines("<!--\nmulti\nline -->\nafter"), ["after"]);
        assert_eq!(plain_lines("> quoted\n#hashtag"), ["quoted", "#hashtag"]);
        assert_eq!(
            inline_plain("snake_case and 2*3 ![alt](i.png)"),
            "snake_case and 2*3 alt"
        );
        // What is left is still escaped: nothing in it formats, links, or mentions.
        let doc = MessageDocument::from_markdown(&excerpt("@bob *hi* <b>").unwrap()).unwrap();
        assert_eq!(doc.plain_text(), "> @bob *hi* <b>");
    }

    #[test]
    fn excerpts_are_short_quotes() {
        assert_eq!(excerpt("  "), None);
        assert_eq!(excerpt("one line").unwrap(), "> one line");
        let many = excerpt("a\n\nb\nc\nd").unwrap();
        assert_eq!(many, "> a\n> b\n> c …");
        let long = excerpt(&"x".repeat(1000)).unwrap();
        assert!(long.ends_with(" …") && long.chars().count() < 300);
    }

    #[test]
    fn signatures_are_checked() {
        let body = br#"{"zen":"Keep it logically awesome."}"#;
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"secret");
        let tag = hmac::sign(&key, body);
        let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert!(signature_ok(
            b"secret",
            body,
            Some(&format!("sha256={hex}"))
        ));
        assert!(!signature_ok(
            b"other",
            body,
            Some(&format!("sha256={hex}"))
        ));
        assert!(!signature_ok(
            b"secret",
            b"{}",
            Some(&format!("sha256={hex}"))
        ));
        assert!(!signature_ok(b"secret", body, Some(&hex)));
        assert!(!signature_ok(b"secret", body, None));
        assert!(!signature_ok(b"secret", body, Some("sha256=zz")));
    }

    #[test]
    fn the_manifest_points_back_here_and_is_private() {
        let m = manifest("https://chat.lazyto.day");
        assert_eq!(
            m["hook_attributes"]["url"],
            "https://chat.lazyto.day/github/webhook"
        );
        assert_eq!(m["redirect_url"], "https://chat.lazyto.day/github/callback");
        assert_eq!(m["public"], false);
        assert_eq!(m["name"], "Lazy chat.lazyto.day");
        assert!(m["name"].as_str().unwrap().len() <= 34);
        assert_eq!(m["default_permissions"]["pull_requests"], "read");
    }
}
