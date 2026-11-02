//! `rtc-sfu`: the SFU as a process. Configured by flags or environment variables.

use std::net::{IpAddr, SocketAddr};

use clap::Parser;

#[derive(Parser)]
#[command(version, about = "The rtc media server (an SFU)")]
struct Args {
    /// The secret join tokens are signed with (at least 32 bytes), shared with the host app.
    #[arg(long, env = "RTC_SECRET", hide_env_values = true)]
    secret: String,
    /// Where signaling (HTTP and WebSocket) listens.
    #[arg(long, env = "RTC_HTTP", default_value = "0.0.0.0:7880")]
    http: SocketAddr,
    /// Where media (UDP) is received. Must be reachable from clients.
    #[arg(long, env = "RTC_UDP", default_value = "0.0.0.0:7881")]
    udp: SocketAddr,
    /// The public address clients send media to, if not this machine's own (NAT, containers).
    #[arg(long, env = "RTC_PUBLIC_IP", default_value = "")]
    public_ip: String,
    /// The most people in one room.
    #[arg(long, env = "RTC_MAX_PER_ROOM", default_value_t = 12)]
    max_per_room: usize,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rtc_sfu=info,str0m=warn".into()),
        )
        .init();
    let args = Args::parse();
    // Empty (as compose files leave it when unset) means this machine's own address.
    let public_ip = match args.public_ip.trim() {
        "" => None,
        ip => Some(ip.parse::<IpAddr>().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("RTC_PUBLIC_IP is not an IP address: {e}"),
            )
        })?),
    };
    let sfu = rtc_sfu::Sfu::start(rtc_sfu::SfuConfig {
        secret: args.secret.into_bytes(),
        udp_bind: args.udp,
        public_ip,
        max_per_room: args.max_per_room,
    })?;
    let listener = tokio::net::TcpListener::bind(args.http).await?;
    tracing::info!(http = %args.http, media = %sfu.media_addr(), "rtc-sfu listening");
    axum::serve(listener, sfu.router())
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await
}
