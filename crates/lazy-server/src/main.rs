use std::net::SocketAddr;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use lazy_server::logging::LogFormat;
use lazy_server::{Config, db};

#[derive(Parser)]
#[command(name = "lazy-server", version, about = "The Lazy chat server")]
struct Cli {
    /// How log events are written: `text` (default) or `json` (one object per line). The filter is
    /// `LAZY_LOG` (or `RUST_LOG`), default `info,sqlx=warn`. Nothing logged holds a credential or
    /// content.
    #[arg(
        long,
        global = true,
        env = "LAZY_LOG_FORMAT",
        default_value = "text",
        value_parser = clap::value_parser!(LogFormat),
    )]
    log_format: LogFormat,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve(ServeArgs),
    /// Apply database migrations and exit.
    Migrate(DatabaseArgs),
    /// Run only the job worker (integration deliveries and other background work), for a process
    /// of its own. Start the server with `--run-jobs false` alongside it.
    Worker(WorkerArgs),
    /// Re-seal every integration credential under the current credentials key, so that previous
    /// keys can be removed. Safe to run while the server runs, and to run again.
    RewrapCredentials(RewrapArgs),
    /// Exit 0 only if a running server answers `GET /healthz` with 200 within 3 seconds. For a
    /// container's HEALTHCHECK; needs no curl and no database.
    Healthcheck(HealthcheckArgs),
}

#[derive(Args)]
struct HealthcheckArgs {
    /// The server's address (`http://host:port`, optionally with a path; the default path is
    /// `/healthz`). Defaults to the address in `LAZY_LISTEN`, with an unspecified host (0.0.0.0, [::])
    /// read as 127.0.0.1.
    #[arg(long)]
    url: Option<String>,
    /// Where the server listens, to derive the address from.
    #[arg(long, env = "LAZY_LISTEN", default_value = "127.0.0.1:8080")]
    listen: String,
}

/// The keys integration credentials are sealed with (D31).
#[derive(Args)]
struct KeyArgs {
    /// The current credentials key: 64 hex digits (`openssl rand -hex 32`). Without it, the server
    /// runs and integrations that need credentials are unavailable. Keep it outside the database
    /// and its backups; losing it loses every stored credential. Prefer the environment variable.
    #[arg(long, env = "LAZY_CREDENTIALS_KEY", hide_env_values = true)]
    credentials_key: Option<String>,
    /// Previous credentials keys, comma-separated, that only open what they sealed (for a
    /// rotation; remove them after `rewrap-credentials`).
    #[arg(long, env = "LAZY_CREDENTIALS_OLD_KEYS", hide_env_values = true)]
    credentials_old_keys: Option<String>,
}

impl KeyArgs {
    fn keys(&self) -> Result<lazy_server::credentials::CredentialKeys, lazy_server::AppError> {
        lazy_server::credentials::CredentialKeys::parse(
            self.credentials_key.as_deref(),
            self.credentials_old_keys.as_deref(),
        )
        .map_err(|reason| {
            lazy_server::AppError::internal(format!(
                "{reason} (LAZY_CREDENTIALS_KEY or LAZY_CREDENTIALS_OLD_KEYS); the server did not start"
            ))
        })
    }
}

#[derive(Args)]
struct WorkerArgs {
    #[command(flatten)]
    database: DatabaseArgs,
    #[command(flatten)]
    keys: KeyArgs,
    /// Jobs at once.
    #[arg(long, env = "LAZY_JOB_CONCURRENCY", default_value_t = 4)]
    concurrency: usize,
}

#[derive(Args)]
struct RewrapArgs {
    #[command(flatten)]
    database: DatabaseArgs,
    #[command(flatten)]
    keys: KeyArgs,
}

#[derive(Args)]
struct DatabaseArgs {
    /// Postgres connection URL.
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    database_url: String,
}

#[derive(Args)]
struct ServeArgs {
    #[command(flatten)]
    database: DatabaseArgs,
    /// Address to listen on.
    #[arg(long, env = "LAZY_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Do not apply migrations at startup.
    #[arg(long)]
    no_migrate: bool,
    /// Do not create the default workspace when none exists.
    #[arg(long)]
    no_seed: bool,
    /// Days of event history to keep before pruning (the newest 1000 events per workspace are
    /// always kept). 0 keeps everything.
    #[arg(long, env = "LAZY_EVENT_RETENTION_DAYS", default_value_t = lazy_server::RetentionPolicy::DEFAULT_DAYS)]
    event_retention_days: u32,
    /// Whether anyone who can reach the server may register.
    #[arg(long, env = "LAZY_OPEN_REGISTRATION", default_value_t = true, action = clap::ArgAction::Set)]
    open_registration: bool,
    /// Whether any signed-in person may create workspaces of their own.
    #[arg(long, env = "LAZY_OPEN_WORKSPACE_CREATION", default_value_t = true, action = clap::ArgAction::Set)]
    open_workspace_creation: bool,
    /// The address clients reach this server at, which incoming webhooks are shown under
    /// (`<public-url>/hooks/<id>/<secret>`). Defaults to http:// and the listening address.
    #[arg(long, env = "LAZY_PUBLIC_URL")]
    public_url: Option<String>,
    /// The address to use when `LAZY_PUBLIC_URL` is unset or empty: a deployment's generated
    /// address (Coolify's `SERVICE_URL_LAZY`), which can lag behind a domain set later, so a
    /// `LAZY_PUBLIC_URL` someone sets always wins.
    #[arg(long, env = "LAZY_DEFAULT_PUBLIC_URL", hide = true)]
    default_public_url: Option<String>,
    /// The directory attachment files are kept in. It is created if it does not exist; keep it on
    /// a disk that survives restarts, and back it up with the database.
    #[arg(long, env = "LAZY_ATTACHMENTS_DIR", default_value = "attachments")]
    attachments_dir: std::path::PathBuf,
    /// The largest attachment, in bytes.
    #[arg(long, env = "LAZY_MAX_ATTACHMENT_BYTES", default_value_t = lazy_domain::attachment::MAX_ATTACHMENT_BYTES)]
    max_attachment_bytes: u64,
    /// The workspace's registration policy when the server creates it at its first start: `open`
    /// (default) or `invite_only`. Ignored once a workspace exists.
    #[arg(long, env = "LAZY_SEED_REGISTRATION", default_value = "open", value_parser = parse_registration)]
    seed_registration: lazy_domain::workspace::RegistrationPolicy,
    /// A secret (20 to 64 letters, digits, '-' and '_'; `openssl rand -hex 24` makes one) that the
    /// first account's registration must carry in the "invitation code" field while the workspace
    /// has no members; with it set, a stranger cannot become the owner by registering first. It
    /// does nothing once anyone is in. Prefer the environment variable: a command line is visible
    /// to other users of the machine.
    #[arg(long, env = "LAZY_SETUP_CODE", hide_env_values = true)]
    setup_code: Option<String>,
    #[command(flatten)]
    keys: KeyArgs,
    /// Run the job worker in this process. Set it to false when `lazy-server worker` runs it.
    #[arg(long, env = "LAZY_RUN_JOBS", default_value_t = true, action = clap::ArgAction::Set)]
    run_jobs: bool,
}

fn parse_registration(text: &str) -> Result<lazy_domain::workspace::RegistrationPolicy, String> {
    match text.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "open" => Ok(lazy_domain::workspace::RegistrationPolicy::Open),
        "invite_only" => Ok(lazy_domain::workspace::RegistrationPolicy::InviteOnly),
        _ => Err("expected `open` or `invite_only`".to_owned()),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // The health check says one line of its own and logs nothing.
    if let Command::Healthcheck(args) = &cli.command {
        return healthcheck(args).await;
    }
    lazy_server::logging::init(cli.log_format);

    let result = match cli.command {
        Command::Serve(args) => serve(args).await,
        Command::Migrate(args) => migrate(args).await,
        Command::Worker(args) => worker(args).await,
        Command::RewrapCredentials(args) => rewrap(args).await,
        Command::Healthcheck(_) => unreachable!("handled above"),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "lazy-server failed");
            ExitCode::FAILURE
        }
    }
}

/// Prints `healthy` or `unhealthy: <kind of failure>` and exits 0 or 1.
async fn healthcheck(args: &HealthcheckArgs) -> ExitCode {
    let url = match &args.url {
        Some(url) => Ok(url.clone()),
        None => lazy_server::health::default_url(&args.listen),
    };
    let outcome = match url {
        Ok(url) => lazy_server::health::check(&url, lazy_server::health::TIMEOUT).await,
        Err(reason) => Err(reason),
    };
    match outcome {
        Ok(()) => {
            println!("healthy");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            println!("unhealthy: {reason}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(args: ServeArgs) -> Result<(), lazy_server::AppError> {
    // Refuse a setup code the app could not send before anything else, saying why and not what it
    // was. (One already used for the first account does nothing, which the message says.)
    let setup_code = match args.setup_code.as_deref().filter(|code| !code.is_empty()) {
        Some(code) => Some(lazy_server::SetupCode::new(code).map_err(|reason| {
            lazy_server::AppError::internal(format!(
                "{reason} (LAZY_SETUP_CODE or --setup-code); the server did not start. If \
                 people have signed up here already, the setup code does nothing: remove it"
            ))
        })?),
        None => None,
    };
    let mut config = Config::new(args.listen, args.database.database_url);
    config.setup_code = setup_code;
    config.seed_registration = args.seed_registration;
    config.migrate = !args.no_migrate;
    config.seed = !args.no_seed;
    config.open_registration = args.open_registration;
    config.open_workspace_creation = args.open_workspace_creation;
    config.event_retention_days = args.event_retention_days;
    config.public_url = public_url(args.public_url, args.default_public_url);
    config.attachments_dir = args.attachments_dir;
    config.max_attachment_bytes = args.max_attachment_bytes;
    config.credential_keys = args.keys.keys()?;
    config.run_jobs = args.run_jobs;
    lazy_server::run(config, shutdown_signal()).await
}

async fn worker(args: WorkerArgs) -> Result<(), lazy_server::AppError> {
    // Checked so a bad key stops this process as it would the server, though no handler uses it
    // yet.
    let _keys = args.keys.keys()?;
    let pool = db::connect(&args.database.database_url, 4).await?;
    let stop = tokio_util::sync::CancellationToken::new();
    let worker = lazy_server::jobs::spawn(
        pool.clone(),
        lazy_server::app::job_handlers(),
        lazy_server::jobs::WorkerOptions {
            concurrency: args.concurrency.max(1),
            ..lazy_server::jobs::WorkerOptions::default()
        },
        stop.clone(),
    );
    shutdown_signal().await;
    stop.cancel();
    worker.join().await;
    pool.close().await;
    Ok(())
}

async fn rewrap(args: RewrapArgs) -> Result<(), lazy_server::AppError> {
    let keys = args.keys.keys()?;
    let pool = db::connect(&args.database.database_url, 2).await?;
    let report = lazy_server::credentials::rewrap_all(&pool, &keys).await?;
    pool.close().await;
    tracing::info!(
        rewrapped = report.rewrapped,
        current = report.current,
        missing_key = report.missing_key,
        "credentials re-sealed under the current key"
    );
    if report.missing_key > 0 {
        return Err(lazy_server::AppError::internal(format!(
            "{} credentials were sealed by a key that is not configured; they were left as they are",
            report.missing_key
        )));
    }
    Ok(())
}

/// The first of the given addresses that is set and not blank.
fn public_url(given: Option<String>, default: Option<String>) -> Option<String> {
    [given, default]
        .into_iter()
        .flatten()
        .map(|url| url.trim().to_owned())
        .find(|url| !url.is_empty())
}

async fn migrate(args: DatabaseArgs) -> Result<(), lazy_server::AppError> {
    let pool = db::connect(&args.database_url, 2).await?;
    db::migrate(&pool).await?;
    pool.close().await;
    tracing::info!("migrations applied");
    Ok(())
}

/// Completes on Ctrl-C or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutdown requested");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serve_args(extra: &[&str]) -> ServeArgs {
        let mut argv = vec![
            "lazy-server",
            "serve",
            "--database-url",
            "postgres://example/db",
        ];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).expect("valid arguments").command {
            Command::Serve(args) => args,
            Command::Migrate(_)
            | Command::Healthcheck(_)
            | Command::Worker(_)
            | Command::RewrapCredentials(_) => panic!("expected the serve command"),
        }
    }

    #[test]
    fn the_seed_policy_is_open_unless_asked_and_the_setup_code_is_absent_unless_given() {
        use lazy_domain::workspace::RegistrationPolicy::{InviteOnly, Open};
        let args = serve_args(&[]);
        assert_eq!(
            (args.seed_registration, args.setup_code.is_none()),
            (Open, true)
        );
        for (text, policy) in [
            ("open", Open),
            ("invite_only", InviteOnly),
            ("Invite-Only", InviteOnly),
        ] {
            assert_eq!(
                serve_args(&["--seed-registration", text]).seed_registration,
                policy
            );
        }
        let argv = [
            "lazy-server",
            "serve",
            "--database-url",
            "x",
            "--seed-registration",
            "closed",
        ];
        assert!(Cli::try_parse_from(argv).is_err());
        let args = serve_args(&["--setup-code", "a-long-enough-setup-code"]);
        assert_eq!(args.setup_code.as_deref(), Some("a-long-enough-setup-code"));
    }

    #[test]
    fn a_set_public_url_wins_over_the_generated_one_and_blank_means_unset() {
        let some = |s: &str| Some(s.to_owned());
        let generated = some("http://lazy-abc.203.0.113.7.sslip.io");
        assert_eq!(
            public_url(some("https://chat.example.com"), generated.clone()).as_deref(),
            Some("https://chat.example.com")
        );
        assert_eq!(public_url(some("  "), generated.clone()), generated);
        assert_eq!(public_url(None, generated.clone()), generated);
        assert_eq!(public_url(None, some("")), None);
        let args = serve_args(&[
            "--public-url",
            "https://chat.example.com",
            "--default-public-url",
            "http://generated",
        ]);
        assert_eq!(
            public_url(args.public_url, args.default_public_url).as_deref(),
            Some("https://chat.example.com")
        );
    }

    #[test]
    fn event_retention_defaults_to_thirty_days() {
        assert_eq!(serve_args(&[]).event_retention_days, 30);
    }

    #[test]
    fn zero_retention_days_is_accepted_and_disables_pruning() {
        let args = serve_args(&["--event-retention-days", "0"]);
        assert_eq!(args.event_retention_days, 0);
        assert!(!lazy_server::RetentionPolicy::from_days(args.event_retention_days).enabled());
    }

    #[test]
    fn retention_must_be_a_whole_number_of_days() {
        for bad in ["-1", "soon", "1.5"] {
            let argv = [
                "lazy-server",
                "serve",
                "--database-url",
                "x",
                "--event-retention-days",
                bad,
            ];
            assert!(Cli::try_parse_from(argv).is_err(), "{bad}");
        }
    }
}
