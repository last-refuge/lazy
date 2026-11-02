//! The rtc media library's server: a selective forwarding unit (SFU) on str0m.
//!
//! Each participant has one WebRTC connection to the SFU. They send their own microphone and
//! camera; the SFU forwards every track to everyone else in the same room, unchanged (it never
//! decodes media). All media shares one UDP port; signaling is JSON on a WebSocket
//! ([`rtc_proto::SIGNAL_PATH`]), authorized by a join token the host application signed
//! ([`rtc_proto::token`]).
//!
//! The SFU is ICE-lite: it has a public address and only answers; clients behind NAT reach it
//! from wherever they are. Media is encrypted between each client and the SFU (DTLS-SRTP); the
//! SFU can read it. That is not end-to-end encryption.
//!
//! ```no_run
//! # async fn run() -> std::io::Result<()> {
//! let sfu = rtc_sfu::Sfu::start(rtc_sfu::SfuConfig {
//!     secret: b"a secret of at least thirty-two bytes".to_vec(),
//!     udp_bind: "0.0.0.0:7881".parse().unwrap(),
//!     public_ip: Some("203.0.113.7".parse().unwrap()),
//!     max_per_room: 12,
//! })?;
//! let listener = tokio::net::TcpListener::bind("0.0.0.0:7880").await?;
//! axum::serve(listener, sfu.router()).await
//! # }
//! ```

mod room;
mod signaling;

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;

use axum::Router;
use axum::routing::get;

pub use room::Command;

/// How the SFU runs.
#[derive(Clone, Debug)]
pub struct SfuConfig {
    /// Shared with the host application, which signs join tokens with it.
    pub secret: Vec<u8>,
    /// Where to receive media.
    pub udp_bind: SocketAddr,
    /// The address clients reach the UDP port at, if not the bound one (behind NAT or in a
    /// container). The port is always the bound one.
    pub public_ip: Option<IpAddr>,
    /// The most people in one room.
    pub max_per_room: usize,
}

/// A running SFU. Dropping it does not stop the media thread; the process ends it.
#[derive(Clone)]
pub struct Sfu {
    inner: Arc<Inner>,
}

struct Inner {
    secret: Vec<u8>,
    commands: mpsc::Sender<Command>,
    next_participant: AtomicU64,
    media_addr: SocketAddr,
}

impl Sfu {
    /// Binds the UDP port and starts the media thread.
    pub fn start(config: SfuConfig) -> std::io::Result<Sfu> {
        if config.secret.len() < rtc_proto::token::MIN_SECRET_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "the secret must be at least {} bytes",
                    rtc_proto::token::MIN_SECRET_BYTES
                ),
            ));
        }
        // str0m's DTLS and SRTP use the pure-Rust crypto provider.
        str0m::crypto::from_feature_flags().install_process_default();
        let socket = UdpSocket::bind(config.udp_bind)?;
        let bound = socket.local_addr()?;
        let ip = match config.public_ip {
            Some(ip) => ip,
            None if bound.ip().is_unspecified() => host_address(),
            None => bound.ip(),
        };
        let media_addr = SocketAddr::new(ip, bound.port());
        let (commands, receiver) = mpsc::channel();
        let max = config.max_per_room;
        std::thread::Builder::new()
            .name("rtc-sfu-media".into())
            .spawn(move || room::run(socket, media_addr, receiver, max))?;
        tracing::info!(%media_addr, "SFU media port ready");
        Ok(Sfu {
            inner: Arc::new(Inner {
                secret: config.secret,
                commands,
                next_participant: AtomicU64::new(1),
                media_addr,
            }),
        })
    }

    /// The address media is received at, as clients are told.
    pub fn media_addr(&self) -> SocketAddr {
        self.inner.media_addr
    }

    /// The HTTP routes: the signaling WebSocket and `/healthz`.
    pub fn router(&self) -> Router {
        Router::new()
            .route(rtc_proto::SIGNAL_PATH, get(signaling::upgrade))
            .route("/healthz", get(|| async { "ok" }))
            .with_state(self.clone())
    }
}

/// This machine's address on its default route, or loopback if it has none.
fn host_address() -> IpAddr {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("192.0.2.1:9")?;
            s.local_addr()
        })
        .map(|a| a.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]))
}
