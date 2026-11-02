//! Link previews (llm-docs/DECISIONS.md, D41): the title, description, and site of the pages a
//! message links to, fetched by the server under strict rules and kept per workspace.
//!
//! # Asking
//!
//! `GET /v1/messages/{id}/previews` answers for one message, to someone who can read it now (the
//! same check as reading the message), so a preview is never shown outside the conversation it
//! belongs to. Previews are made for messages people write (not apps', which bring their own
//! blocks, D40): the first [`lazy_domain::document::MAX_PREVIEW_LINKS`] distinct `http`/`https` links of the current revision.
//! A link already fetched in the workspace in the last [`FRESH_HOURS`] (or failed in the last
//! [`RETRY_MINUTES`]) is answered from the cache.
//!
//! # Fetching
//!
//! Only `http` and `https` on their usual ports, with no credentials in the address. Every address
//! the name resolves to must be public: loopback, private, link-local (cloud metadata), shared,
//! multicast, documentation, and reserved ranges are refused, for literal addresses and for names
//! (the resolver drops them, so a name that also resolves to a public address connects only to
//! that one). Redirects are followed at most [`MAX_REDIRECTS`] times and each hop is checked the
//! same way. The whole fetch has [`FETCH_SECONDS`]; only HTML is read, and at most
//! [`MAX_BODY_BYTES`] of it. No cookies, no credentials, and nothing from the page reaches a
//! client but plain text (no images, no HTML).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use lazy_domain::{MessageId, UserId};
use lazy_protocol::LinkPreview;
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppError;

/// How long a fetched preview is used before it is fetched again.
pub const FRESH_HOURS: i32 = 24;
/// How long a failed fetch is remembered before it is tried again.
pub const RETRY_MINUTES: i32 = 60;
/// How many redirects are followed.
pub const MAX_REDIRECTS: usize = 3;
/// How long a fetch may take, from connecting to the last byte read.
pub const FETCH_SECONDS: u64 = 5;
/// The most of a page read.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Longest title, description, and site name kept, in characters.
const MAX_TITLE: usize = 200;
const MAX_DESCRIPTION: usize = 300;
const MAX_SITE: usize = 100;

/// Settings for link previews. The default is what the server runs with.
#[derive(Debug, Clone, Copy, Default)]
pub struct PreviewOptions {
    /// Allow this machine's own addresses (`127.0.0.0/8`, `::1`): for tests with a stand-in site
    /// only. Every other private range stays refused.
    pub allow_loopback: bool,
}

/// Whether an address is one a preview may connect to.
pub fn address_allowed(ip: IpAddr, options: PreviewOptions) -> bool {
    if options.allow_loopback && ip.is_loopback() {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => v4_public(v4),
        IpAddr::V6(v6) => v6_public(v6),
    }
}

fn v4_public(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        // Shared address space (carrier-grade NAT).
        || (a == 100 && (64..128).contains(&b))
        // IETF protocol assignments.
        || (a == 192 && b == 0 && c == 0)
        // Benchmarking.
        || (a == 198 && (b == 18 || b == 19))
        // Reserved, and the old class E.
        || a >= 240)
}

fn v6_public(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_public(v4);
    }
    let segments = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        // Unique local, fc00::/7.
        || (segments[0] & 0xfe00) == 0xfc00
        // Link-local, fe80::/10.
        || (segments[0] & 0xffc0) == 0xfe80
        // Documentation, 2001:db8::/32.
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        // NAT64 (64:ff9b::/96) and IPv4-compatible (::/96) can reach IPv4 addresses.
        || (segments[0] == 0x0064 && segments[1] == 0xff9b)
        || segments[..6].iter().all(|s| *s == 0))
}

/// Resolves names to their allowed addresses only; a name with none is an error.
struct PublicResolver {
    options: PreviewOptions,
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let options = self.options;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let allowed: Vec<SocketAddr> = found
                .filter(|addr| address_allowed(addr.ip(), options))
                .collect();
            if allowed.is_empty() {
                return Err("the name has no public address".into());
            }
            let addrs: Addrs = Box::new(allowed.into_iter());
            Ok(addrs)
        })
    }
}

/// Whether a URL may be fetched (or redirected to): http or https, the usual port, no
/// credentials, and a literal address (if it is one) that is allowed. Names are checked when
/// they are resolved.
pub fn url_allowed(url: &Url, options: PreviewOptions) -> bool {
    let scheme_ok = matches!(url.scheme(), "http" | "https");
    let port_ok = url.port().is_none()
        || (options.allow_loopback && url.host_str().is_some_and(is_loopback_host));
    let host_ok = match url.host() {
        Some(url::Host::Ipv4(ip)) => address_allowed(IpAddr::V4(ip), options),
        Some(url::Host::Ipv6(ip)) => address_allowed(IpAddr::V6(ip), options),
        Some(url::Host::Domain(name)) => {
            // `localhost` never leaves the machine, whatever the resolver says.
            options.allow_loopback || !is_loopback_host(name)
        }
        None => false,
    };
    scheme_ok && port_ok && host_ok && url.username().is_empty() && url.password().is_none()
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The client previews are fetched with.
#[derive(Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    options: PreviewOptions,
}

impl Fetcher {
    /// `None` when no client can be built (no CA certificates): previews are then off.
    pub fn new(options: PreviewOptions) -> Option<Self> {
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if !url_allowed(attempt.url(), options) {
                attempt.error("redirected to an address that is not allowed")
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(FETCH_SECONDS))
            .connect_timeout(Duration::from_secs(3))
            .redirect(policy)
            .dns_resolver(Arc::new(PublicResolver { options }))
            .user_agent(concat!(
                "Lazy link previews/",
                env!("CARGO_PKG_VERSION"),
                " (like Slackbot-LinkExpanding)"
            ))
            .build()
            .ok()?;
        Some(Self { client, options })
    }

    /// Fetches a page and reads its preview; `None` for anything that is not an HTML page with a
    /// title or description, or that breaks a rule.
    pub async fn fetch(&self, url: &str) -> Option<Fetched> {
        let url = Url::parse(url).ok()?;
        if !url_allowed(&url, self.options) {
            return None;
        }
        let mut response = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let html = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                let kind = v.split(';').next().unwrap_or_default().trim();
                kind.eq_ignore_ascii_case("text/html")
                    || kind.eq_ignore_ascii_case("application/xhtml+xml")
            });
        if !html {
            return None;
        }
        let final_url = response.url().clone();
        let mut body = Vec::new();
        while body.len() < MAX_BODY_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(_) => return None,
            }
        }
        body.truncate(MAX_BODY_BYTES);
        let page = String::from_utf8_lossy(&body);
        let mut fetched = extract(&page);
        if fetched.title.is_none() && fetched.description.is_none() {
            return None;
        }
        if fetched.site_name.is_none() {
            fetched.site_name = final_url
                .host_str()
                .map(|h| h.trim_start_matches("www.").to_owned());
        }
        Some(fetched)
    }
}

/// What a page says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetched {
    pub title: Option<String>,
    pub description: Option<String>,
    pub site_name: Option<String>,
}

/// Reads Open Graph and Twitter card tags, the page title, and the description meta tag.
pub fn extract(html: &str) -> Fetched {
    let mut og_title = None;
    let mut og_description = None;
    let mut og_site = None;
    let mut meta_description = None;
    let mut twitter_title = None;
    let mut twitter_description = None;
    let lower = html.to_ascii_lowercase();
    let mut at = 0;
    while let Some(found) = lower[at..].find("<meta") {
        let start = at + found;
        let Some(end) = lower[start..].find('>').map(|e| start + e) else {
            break;
        };
        let attributes = attributes(&html[start + 5..end]);
        let key = attributes
            .iter()
            .find(|(name, _)| name == "property" || name == "name")
            .map(|(_, value)| value.to_ascii_lowercase());
        let content = attributes
            .iter()
            .find(|(name, _)| name == "content")
            .map(|(_, value)| value.clone());
        if let (Some(key), Some(content)) = (key, content) {
            let slot = match key.as_str() {
                "og:title" => &mut og_title,
                "og:description" => &mut og_description,
                "og:site_name" => &mut og_site,
                "description" => &mut meta_description,
                "twitter:title" => &mut twitter_title,
                "twitter:description" => &mut twitter_description,
                _ => {
                    at = end;
                    continue;
                }
            };
            if slot.is_none() {
                *slot = Some(content);
            }
        }
        at = end;
    }
    let title_tag = lower.find("<title").and_then(|start| {
        let open_end = start + lower[start..].find('>')? + 1;
        let close = open_end + lower[open_end..].find("</title")?;
        Some(html[open_end..close].to_owned())
    });
    let clean = |text: Option<String>, max: usize| {
        text.map(|t| clipped(&decode_entities(&t), max))
            .filter(|t| !t.is_empty())
    };
    Fetched {
        title: clean(og_title.or(twitter_title).or(title_tag), MAX_TITLE),
        description: clean(
            og_description.or(twitter_description).or(meta_description),
            MAX_DESCRIPTION,
        ),
        site_name: clean(og_site, MAX_SITE),
    }
}

/// The attributes of a tag, names lowercased, values unquoted.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = tag.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == '/') {
            i += 1;
        }
        let name_start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '=' && chars[i] != '/' {
            i += 1;
        }
        let name: String = chars[name_start..i]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < chars.len() && chars[i] == '=' {
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
                let quote = chars[i];
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != quote {
                    i += 1;
                }
                value = chars[start..i].iter().collect();
                i += 1;
            } else {
                let start = i;
                while i < chars.len() && !chars[i].is_whitespace() {
                    i += 1;
                }
                value = chars[start..i].iter().collect();
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        } else {
            i += 1;
        }
    }
    out
}

/// Decodes the HTML entities pages use in titles: the named five, `&nbsp;`, and numeric ones.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// One line, without control characters, cut to `max` characters with an ellipsis.
fn clipped(text: &str, max: usize) -> String {
    let line = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>();
    if line.chars().count() <= max {
        return line;
    }
    let mut cut: String = line.chars().take(max - 1).collect();
    cut.push('\u{2026}');
    cut
}

#[derive(sqlx::FromRow)]
struct CachedRow {
    url: String,
    ok: bool,
    title: Option<String>,
    description: Option<String>,
    site_name: Option<String>,
}

/// `GET /v1/messages/{id}/previews`: the previews of a message the caller can read now.
pub async fn previews(
    pool: &PgPool,
    fetcher: Option<&Fetcher>,
    actor: UserId,
    id: MessageId,
) -> Result<Vec<LinkPreview>, AppError> {
    // Visible to the caller now, or not found: the same check as reading it.
    let message = crate::messages::get_message(pool, actor, id).await?;
    let Some(fetcher) = fetcher else {
        return Ok(Vec::new());
    };
    if message.deleted || message.rich.is_some() {
        return Ok(Vec::new());
    }
    let is_bot: bool = sqlx::query_scalar("SELECT is_bot FROM users WHERE id = $1")
        .bind(message.author_id.as_uuid())
        .fetch_one(pool)
        .await?;
    let links = message.document.preview_links();
    if is_bot || links.is_empty() {
        return Ok(Vec::new());
    }
    let workspace: Uuid =
        sqlx::query_scalar("SELECT workspace_id FROM conversations WHERE id = $1")
            .bind(message.conversation_id.as_uuid())
            .fetch_one(pool)
            .await?;
    let cached: Vec<CachedRow> = sqlx::query_as(
        "SELECT url, ok, title, description, site_name FROM link_previews \
         WHERE workspace_id = $1 AND url = ANY($2) \
           AND fetched_at > now() - CASE WHEN ok THEN make_interval(hours => $3) \
                                         ELSE make_interval(mins => $4) END",
    )
    .bind(workspace)
    .bind(&links)
    .bind(FRESH_HOURS)
    .bind(RETRY_MINUTES)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for url in links {
        let row = match cached.iter().find(|row| row.url == url) {
            Some(row) => CachedRow {
                url: row.url.clone(),
                ok: row.ok,
                title: row.title.clone(),
                description: row.description.clone(),
                site_name: row.site_name.clone(),
            },
            None => {
                let fetched = fetcher.fetch(&url).await;
                let ok = fetched.is_some();
                let fetched = fetched.unwrap_or_default();
                sqlx::query(
                    "INSERT INTO link_previews (workspace_id, url, ok, title, description, site_name, fetched_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, now()) \
                     ON CONFLICT (workspace_id, url) DO UPDATE SET ok = excluded.ok, \
                       title = excluded.title, description = excluded.description, \
                       site_name = excluded.site_name, fetched_at = excluded.fetched_at",
                )
                .bind(workspace)
                .bind(&url)
                .bind(ok)
                .bind(&fetched.title)
                .bind(&fetched.description)
                .bind(&fetched.site_name)
                .execute(pool)
                .await?;
                // Previews nobody has asked for in a week go, so the table stays the size of
                // what is being read.
                sqlx::query(
                    "DELETE FROM link_previews WHERE workspace_id = $1 \
                     AND fetched_at < now() - interval '7 days'",
                )
                .bind(workspace)
                .execute(pool)
                .await?;
                CachedRow {
                    url,
                    ok,
                    title: fetched.title,
                    description: fetched.description,
                    site_name: fetched.site_name,
                }
            }
        };
        if row.ok {
            out.push(LinkPreview {
                url: row.url,
                title: row.title,
                description: row.description,
                site_name: row.site_name,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazy_domain::MessageDocument;

    const STRICT: PreviewOptions = PreviewOptions {
        allow_loopback: false,
    };

    #[test]
    fn only_public_addresses_are_allowed() {
        for blocked in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "::a00:1",
            "2001:db8::1",
        ] {
            assert!(
                !address_allowed(blocked.parse().unwrap(), STRICT),
                "{blocked}"
            );
        }
        for allowed in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(
                address_allowed(allowed.parse().unwrap(), STRICT),
                "{allowed}"
            );
        }
        let tests = PreviewOptions {
            allow_loopback: true,
        };
        assert!(address_allowed("127.0.0.1".parse().unwrap(), tests));
        assert!(!address_allowed("10.0.0.1".parse().unwrap(), tests));
    }

    #[test]
    fn urls_are_checked_before_any_connection() {
        let ok = |u: &str| url_allowed(&Url::parse(u).unwrap(), STRICT);
        assert!(ok("https://miro.com/app/board/uXjVabc=/"));
        assert!(ok("http://example.com/a?b=c"));
        assert!(!ok("https://example.com:8443/"));
        assert!(!ok("https://user:pw@example.com/"));
        assert!(!ok("ftp://example.com/"));
        assert!(!ok("file:///etc/passwd"));
        assert!(!ok("http://localhost/"));
        assert!(!ok("http://api.localhost./"));
        assert!(!ok("http://127.0.0.1/"));
        assert!(!ok("http://[::1]/"));
        assert!(!ok("http://169.254.169.254/latest/meta-data/"));
        assert!(
            !ok("http://2130706433/"),
            "a decimal IPv4 is still 127.0.0.1"
        );
        assert!(!ok("http://0x7f.1/"));
    }

    #[test]
    fn pages_say_what_they_are() {
        let page = r#"<!doctype html><html><head>
            <title>Fallback &amp; title</title>
            <meta name="description" content="Plain description">
            <meta property='og:title' content="Roadmap Q4 &#8211; Miro" />
            <meta content="The team&#39;s board" property="og:description">
            <meta property="og:site_name" content="Miro">
            </head><body><meta property="og:title" content="ignored"></body></html>"#;
        assert_eq!(
            extract(page),
            Fetched {
                title: Some("Roadmap Q4 \u{2013} Miro".into()),
                description: Some("The team's board".into()),
                site_name: Some("Miro".into()),
            }
        );
        let plain = "<html><head><TITLE>\n  Just   a title\n</TITLE></head></html>";
        assert_eq!(extract(plain).title.as_deref(), Some("Just a title"));
        assert_eq!(extract("no tags here"), Fetched::default());
        let long = format!("<title>{}</title>", "x".repeat(500));
        assert_eq!(extract(&long).title.unwrap().chars().count(), MAX_TITLE);
    }

    #[test]
    fn links_people_wrote_are_previewed_not_quotes_or_code() {
        let document = MessageDocument::from_markdown(
            "See https://a.example/1 and [this](https://b.example/2), again https://a.example/1\n\n\
             > https://quoted.example\n\n\
             ```\nhttps://code.example\n```\n\n\
             - https://c.example/3\n- https://d.example/4\n\n\
             mailto:x@example.com",
        )
        .unwrap();
        assert_eq!(
            document.preview_links(),
            [
                "https://a.example/1",
                "https://b.example/2",
                "https://c.example/3"
            ]
        );
    }
}
