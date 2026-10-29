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
    /// The directory attachment files are kept in. It is created if it does not exist; keep it on
    /// a disk that survives restarts, and back it up with the database.
    #[arg(long, env = "LAZY_ATTACHMENTS_DIR", default_value = "attachments")]
    attachments_dir: std::path::PathBuf,
    /// The largest attachment, in bytes.
    #[arg(long, env = "LAZY_MAX_ATTACHMENT_BYTES", default_value_t = lazy_domain::attachment::MAX_ATTACHMENT_BYTES)]
    max_attachment_bytes: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    lazy_server::logging::init(cli.log_format);

    let result = match cli.command {
        Command::Serve(args) => serve(args).await,
        Command::Migrate(args) => migrate(args).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "lazy-server failed");
            ExitCode::FAILURE
        }
    }
}

async fn serve(args: ServeArgs) -> Result<(), lazy_server::AppError> {
    let mut config = Config::new(args.listen, args.database.database_url);
    config.migrate = !args.no_migrate;
    config.seed = !args.no_seed;
    config.open_registration = args.open_registration;
    config.open_workspace_creation = args.open_workspace_creation;
    config.event_retention_days = args.event_retention_days;
    config.public_url = args.public_url;
    config.attachments_dir = args.attachments_dir;
    config.max_attachment_bytes = args.max_attachment_bytes;
    lazy_server::run(config, shutdown_signal()).await
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
            Command::Migrate(_) => panic!("expected the serve command"),
        }
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
