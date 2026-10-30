//! Which server the client talks to (dogfood packaging): the address, where it came from, what the
//! last check of it found, and the rules an address typed by a person must satisfy.
//!
//! An installed application cannot rely on an environment variable, so the person chooses the
//! server in the application. Only the address is ever stored or shown, and by construction it
//! has no user name, password, query, or fragment, so it can be logged and copied.

use lazy_protocol::ServerRegistration;
use url::{Host, Url};

/// Used when neither the environment nor the person has chosen a server.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8080";

/// The longest address accepted, in characters.
pub const MAX_SERVER_URL_CHARS: usize = 300;

/// Where the address in use came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServerSource {
    /// [`DEFAULT_SERVER_URL`]: nothing was chosen.
    #[default]
    Default,
    /// The person chose it in the application and it was saved.
    Saved,
    /// The environment (`LAZY_SERVER_URL`) or the host that started the runtime set it. It wins
    /// over a saved choice and cannot be changed in the application: the address is *locked*.
    Environment,
}

/// What the last check of an address found. The words in `Unreachable` are fixed phrases (never
/// the system's own text, which can quote an address or a path).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ServerCheck {
    #[default]
    Unchecked,
    Checking,
    /// A Lazy server speaking this build's protocol answered.
    Reachable {
        protocol_version: u32,
    },
    /// A Lazy server answered but speaks another protocol: this build needs a newer client when
    /// `server > client`, and an older one (or the server needs updating) when it is less.
    Incompatible {
        server: u32,
        client: u32,
    },
    /// Nothing usable answered, or the address was refused; why, in a short phrase.
    Unreachable(String),
}

/// What a server says, before anyone has signed in, about taking accounts (D29): it tells the first
/// screens whether to ask for an invitation code, and whether that code is the setup code. A server
/// built before this existed says nothing, and the client then asks for a code and lets the server
/// decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerFacts {
    pub registration: ServerRegistration,
    /// The server is waiting for its first account, which needs the setup code.
    pub setup_code_required: bool,
}

/// The server in use, and what is known about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// The address in use, normalised (see [`normalize_server_url`]).
    pub url: String,
    pub source: ServerSource,
    /// An address being tried instead, which is not in use: it was set with `SetServerUrl` and
    /// has not yet been checked (`check` is `Checking`) or did not pass (`check` says why).
    /// `None` when the address in use is the one on screen.
    pub trying: Option<String>,
    /// The check of `trying` when there is one, otherwise of `url`.
    pub check: ServerCheck,
}

impl ServerInfo {
    /// The address is fixed by the environment, so the person cannot change it.
    pub fn locked(&self) -> bool {
        self.source == ServerSource::Environment
    }
}

impl Default for ServerInfo {
    fn default() -> Self {
        Self {
            url: DEFAULT_SERVER_URL.to_owned(),
            source: ServerSource::Default,
            trying: None,
            check: ServerCheck::Unchecked,
        }
    }
}

/// Why an address typed by a person was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerUrlError {
    Empty,
    TooLong,
    /// Not an address (spaces, no host, a bad port).
    Invalid,
    /// A scheme other than `http` and `https`.
    Scheme,
    /// A user name or a password in the address.
    Credentials,
    /// A query (`?...`) or a fragment (`#...`).
    QueryOrFragment,
}

impl ServerUrlError {
    /// What to tell the person. Never repeats what they typed: it may hold a password.
    pub fn message(self) -> &'static str {
        match self {
            Self::Empty => "Enter the server's address, like chat.example.com.",
            Self::TooLong => "That address is too long.",
            Self::Invalid => "That doesn't look like a server address.",
            Self::Scheme => "Only http and https addresses work.",
            Self::Credentials => "The address can't include a user name or password.",
            Self::QueryOrFragment => "The address can't include a query (?) or a fragment (#).",
        }
    }
}

/// A machine on the person's own network: `localhost`, a name ending in `.local`, or an address.
fn is_local(url: &Url) -> bool {
    match url.host() {
        None => false,
        Some(Host::Ipv4(_) | Host::Ipv6(_)) => true,
        Some(Host::Domain(name)) => name == "localhost" || name.ends_with(".local"),
    }
}

/// Turns what a person typed into the address the client uses, or says why it cannot be one.
///
/// Surrounding space is ignored. Without a scheme the address is `https://`, or `http://` for a
/// machine on the person's own network (`localhost`, a name ending in `.local`, or an IP
/// address). A trailing slash is dropped, the host is lowercased, and a default port is left
/// out. A path prefix is kept (a reverse proxy may serve Lazy under one). A user name, a
/// password, a query, a fragment, a scheme other than `http` and `https`, spaces, and anything
/// over [`MAX_SERVER_URL_CHARS`] are refused. The result parses, has none of those, and
/// normalises to itself.
pub fn normalize_server_url(input: &str) -> Result<String, ServerUrlError> {
    let text = input.trim();
    if text.is_empty() {
        return Err(ServerUrlError::Empty);
    }
    if text.chars().count() > MAX_SERVER_URL_CHARS {
        return Err(ServerUrlError::TooLong);
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ServerUrlError::Invalid);
    }
    let url = if text.contains("://") {
        Url::parse(text).map_err(|_| ServerUrlError::Invalid)?
    } else {
        // The scheme decides which port is the default one (and so is left out), so it is
        // decided from the host first, and the text is read again with it.
        let tentative =
            Url::parse(&format!("https://{text}")).map_err(|_| ServerUrlError::Invalid)?;
        let scheme = if is_local(&tentative) {
            "http"
        } else {
            "https"
        };
        Url::parse(&format!("{scheme}://{text}")).map_err(|_| ServerUrlError::Invalid)?
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ServerUrlError::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ServerUrlError::Credentials);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ServerUrlError::QueryOrFragment);
    }
    if url.host().is_none() {
        return Err(ServerUrlError::Invalid);
    }
    let path = url.path().trim_end_matches('/');
    Ok(format!("{}{}", url.origin().ascii_serialization(), path))
}
