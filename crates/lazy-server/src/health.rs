//! `lazy-server healthcheck`: whether a running server answers `GET /healthz` with 200, for a
//! container's `HEALTHCHECK` (an image with no `curl`). A plain TCP connection and an HTTP/1.0
//! request, so no HTTP client is needed, and everything it says is content-free.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How long the whole check may take.
pub const TIMEOUT: Duration = Duration::from_secs(3);

/// The largest answer read (only the status line is used).
const MAX_ANSWER_BYTES: u64 = 64 * 1024;

/// The base URL a server listening on `listen` (`host:port`, as `LAZY_LISTEN` is written) is
/// reached at from the same machine: an unspecified host (`0.0.0.0`, `[::]`) is `127.0.0.1`.
pub fn default_url(listen: &str) -> Result<String, String> {
    let address: SocketAddr = listen
        .trim()
        .parse()
        .map_err(|_| "the listening address is not host:port".to_owned())?;
    let ip = if address.ip().is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        address.ip()
    };
    Ok(match ip {
        IpAddr::V4(ip) => format!("http://{ip}:{}", address.port()),
        IpAddr::V6(ip) => format!("http://[{ip}]:{}", address.port()),
    })
}

struct Target {
    /// `host:port` to connect to.
    authority: String,
    path: String,
}

/// `http://host[:port][/path]`; no path (or `/`) means `/healthz`.
fn parse(url: &str) -> Result<Target, String> {
    if url.starts_with("https://") {
        return Err("https is not supported; use the plain listening address".to_owned());
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "the URL must start with http://".to_owned())?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains('@') {
        return Err("the URL needs a host and no credentials".to_owned());
    }
    let authority = if authority
        .rsplit_once(':')
        .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
    {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    let path = if path == "/" { "/healthz" } else { path };
    if path.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err("the URL has characters that are not allowed".to_owned());
    }
    Ok(Target {
        authority,
        path: path.to_owned(),
    })
}

/// `Ok` if `url` answers 200 within `timeout`; otherwise a short reason (a kind of failure, never
/// anything the server sent).
pub async fn check(url: &str, timeout: Duration) -> Result<(), String> {
    let target = parse(url)?;
    tokio::time::timeout(timeout, ask(&target))
        .await
        .map_err(|_| "timed out".to_owned())?
}

async fn ask(target: &Target) -> Result<(), String> {
    let mut stream = TcpStream::connect(&target.authority)
        .await
        .map_err(|e| format!("cannot connect ({:?})", e.kind()))?;
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
        target.path, target.authority
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("cannot send ({:?})", e.kind()))?;
    let mut answer = Vec::new();
    stream
        .take(MAX_ANSWER_BYTES)
        .read_to_end(&mut answer)
        .await
        .map_err(|e| format!("cannot read ({:?})", e.kind()))?;
    let line = answer.split(|b| *b == b'\n').next().unwrap_or(&[]);
    let line = String::from_utf8_lossy(line);
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or("");
    let status = parts.next().and_then(|code| code.parse::<u16>().ok());
    match status {
        Some(200) if version.starts_with("HTTP/1.") => Ok(()),
        Some(code) if version.starts_with("HTTP/1.") => Err(format!("status {code}")),
        _ => Err("the answer was not HTTP".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_address_is_the_listening_one_with_an_unspecified_host_made_local() {
        for (listen, url) in [
            ("0.0.0.0:8080", "http://127.0.0.1:8080"),
            ("[::]:9000", "http://127.0.0.1:9000"),
            ("127.0.0.1:1", "http://127.0.0.1:1"),
            ("192.168.1.5:80", "http://192.168.1.5:80"),
            ("[::1]:8080", "http://[::1]:8080"),
            (" 0.0.0.0:8080 ", "http://127.0.0.1:8080"),
        ] {
            assert_eq!(default_url(listen).unwrap(), url, "{listen}");
        }
        for bad in ["", "localhost:80", "8080", "0.0.0.0"] {
            assert!(default_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn urls_are_http_with_a_host_and_default_to_the_health_path() {
        let target = parse("http://127.0.0.1:8080").unwrap();
        assert_eq!(
            (target.authority.as_str(), target.path.as_str()),
            ("127.0.0.1:8080", "/healthz")
        );
        let target = parse("http://chat.internal/").unwrap();
        assert_eq!(
            (target.authority.as_str(), target.path.as_str()),
            ("chat.internal:80", "/healthz")
        );
        let target = parse("http://[::1]:9/ready").unwrap();
        assert_eq!(
            (target.authority.as_str(), target.path.as_str()),
            ("[::1]:9", "/ready")
        );
        for bad in [
            "https://x:1",
            "ftp://x",
            "x:1",
            "http://",
            "http://u:p@h:1",
            "http://h:1/a b",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
