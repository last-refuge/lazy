//! A TCP proxy between a client and the server that can inject network faults.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// What the proxy does with new traffic.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Forward everything.
    Pass = 0,
    /// Forward requests, but when a message send (`POST /v1/conversations/...`) reaches the
    /// server, swallow its response and close the connection. The server commits; the client
    /// never hears about it: a lost acknowledgment.
    LoseSendAcks = 1,
    /// Refuse service: accept and immediately close every connection.
    Down = 2,
}

pub struct Proxy {
    pub url: String,
    mode: Arc<AtomicU8>,
    sever: watch::Sender<u64>,
    accept: JoinHandle<()>,
}

impl Proxy {
    pub async fn start(upstream: SocketAddr) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let addr = listener.local_addr().expect("proxy address");
        let mode = Arc::new(AtomicU8::new(Mode::Pass as u8));
        let (sever, severed) = watch::channel(0u64);
        let accept = tokio::spawn({
            let mode = mode.clone();
            async move {
                loop {
                    let Ok((client, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(connection(client, upstream, mode.clone(), severed.clone()));
                }
            }
        });
        Proxy {
            url: format!("http://{addr}"),
            mode,
            sever,
            accept,
        }
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode.store(mode as u8, Ordering::SeqCst);
    }

    /// Closes every connection that is currently open.
    pub fn sever_all(&self) {
        self.sever.send_modify(|generation| *generation += 1);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.accept.abort();
        self.sever_all();
    }
}

fn current(mode: &AtomicU8) -> Mode {
    match mode.load(Ordering::SeqCst) {
        1 => Mode::LoseSendAcks,
        2 => Mode::Down,
        _ => Mode::Pass,
    }
}

const SEND_PATTERN: &[u8] = b"POST /v1/conversations/";

async fn connection(
    client: TcpStream,
    upstream: SocketAddr,
    mode: Arc<AtomicU8>,
    mut severed: watch::Receiver<u64>,
) {
    // A cloned receiver inherits its source's "last seen" version, so mark the current one as
    // seen; otherwise a sever that happened before this connection would end it immediately.
    severed.borrow_and_update();
    if current(&mode) == Mode::Down {
        return;
    }
    let Ok(server) = TcpStream::connect(upstream).await else {
        return;
    };
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();
    let swallow = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let upstream_task = {
        let (mode, swallow) = (mode.clone(), swallow.clone());
        async move {
            let mut buffer = vec![0u8; 16 * 1024];
            let mut window: Vec<u8> = Vec::new();
            loop {
                let Ok(read) = client_read.read(&mut buffer).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                if current(&mode) == Mode::LoseSendAcks {
                    window.extend_from_slice(&buffer[..read]);
                    if window
                        .windows(SEND_PATTERN.len())
                        .any(|w| w == SEND_PATTERN)
                    {
                        swallow.store(true, Ordering::SeqCst);
                    }
                    let keep = SEND_PATTERN.len().min(window.len());
                    window.drain(..window.len() - keep);
                }
                if server_write.write_all(&buffer[..read]).await.is_err() {
                    return;
                }
            }
        }
    };
    let downstream_task = async move {
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            let Ok(read) = server_read.read(&mut buffer).await else {
                return;
            };
            if read == 0 {
                return;
            }
            if swallow.load(Ordering::SeqCst) {
                // The server has answered, so the message is committed. Drop the answer.
                let _ = client_write.shutdown().await;
                return;
            }
            if client_write.write_all(&buffer[..read]).await.is_err() {
                return;
            }
        }
    };

    tokio::select! {
        () = upstream_task => {}
        () = downstream_task => {}
        _ = severed.changed() => {}
    }
}
