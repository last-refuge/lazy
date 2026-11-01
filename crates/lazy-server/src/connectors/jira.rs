//! Jira Cloud's deliveries (D34), from a webhook an admin makes in Settings, System, WebHooks:
//! `X-Hub-Signature: sha256=<hex>` is the HMAC of the body under the secret Lazy made (pasted into
//! the webhook's Secret field), and `X-Atlassian-Webhook-Identifier` names the delivery, the same
//! across Jira's retries.
//!
//! The payload is `{webhookEvent, user, issue, changelog, comment}`, where `issue` has the REST
//! shape (`key`, `self`, `fields.summary`, `fields.project.key`, ...). It carries no browse
//! address, so one is made from the host of `issue.self`: `https://{host}/browse/{key}`.
//! Descriptions and comments may be wiki text or an Atlassian document; both are read as text.

use axum::http::HeaderMap;
use serde_json::Value;

use super::{Received, Summarized, header, s};
use crate::provider_text::{escape, excerpt};

pub(super) fn signature_ok(secret: &[u8], headers: &HeaderMap, body: &[u8]) -> bool {
    crate::github::signature_ok(secret, body, header(headers, "x-hub-signature"))
}

pub(super) fn receive(headers: &HeaderMap, payload: &Value) -> Received {
    Received {
        delivery: header(headers, "x-atlassian-webhook-identifier")
            .unwrap_or_default()
            .to_owned(),
        summary: summarize(payload),
    }
}

/// The issue's page: `https://{host}/browse/{key}`, the host taken from its REST address. Empty
/// if either is missing or odd.
fn browse_url(issue: &Value) -> String {
    let key = s(issue, &["key"]);
    let host = s(issue, &["self"])
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default();
    let host_ok = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'));
    let key_ok = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    if host_ok && key_ok {
        format!("https://{host}/browse/{key}")
    } else {
        String::new()
    }
}

/// Text from a field that is either a string (wiki markup) or an Atlassian document (JSON).
fn text_of(value: &Value) -> String {
    fn walk(node: &Value, out: &mut String) {
        if let Some(text) = node["text"].as_str() {
            out.push_str(text);
        }
        if let Some(children) = node["content"].as_array() {
            for child in children {
                walk(child, out);
            }
            if matches!(
                node["type"].as_str(),
                Some("paragraph" | "heading" | "listItem" | "codeBlock")
            ) {
                out.push('\n');
            }
        }
    }
    match value {
        Value::String(text) => wiki_plain(text),
        Value::Object(_) => {
            let mut out = String::new();
            walk(value, &mut out);
            out
        }
        _ => String::new(),
    }
}

/// Jira wiki markup's most common decoration removed: `{code}`/`{noformat}` and `{quote}` lines,
/// `h1.` headings, and `*bold*`-style markers are left to the plain-text excerpt, which already
/// drops Markdown's; this drops the brace macros it does not know.
fn wiki_plain(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let line = line.trim();
            !(line.starts_with('{') && line.ends_with('}') && !line.contains(' '))
        })
        .map(|line| {
            let trimmed = line.trim_start();
            match trimmed.split_once(". ") {
                Some((tag, rest))
                    if tag.len() == 2
                        && tag.starts_with('h')
                        && tag.as_bytes()[1].is_ascii_digit() =>
                {
                    rest
                }
                _ => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn issue_link(issue: &Value) -> String {
    let key = s(issue, &["key"]);
    let summary = s(issue, &["fields", "summary"]);
    let shown = match (key.is_empty(), summary.trim().is_empty()) {
        (false, false) => format!("{key} {summary}"),
        (false, true) => key.to_owned(),
        (true, false) => summary.to_owned(),
        (true, true) => "an issue".to_owned(),
    };
    lazy_domain::markdown::link_source(&browse_url(issue), &shown)
}

fn project_of(issue: &Value) -> Vec<String> {
    let key = match s(issue, &["fields", "project", "key"]) {
        "" => s(issue, &["key"]).split('-').next().unwrap_or_default(),
        key => key,
    };
    if key.is_empty() {
        vec![]
    } else {
        vec![key.to_ascii_uppercase()]
    }
}

fn person(value: &Value) -> Option<&str> {
    value["displayName"]
        .as_str()
        .or_else(|| value["name"].as_str())
        .filter(|name| !name.trim().is_empty())
}

pub(super) fn summarize(payload: &Value) -> Option<Summarized> {
    let issue = &payload["issue"];
    let key = s(issue, &["key"]);
    let who = escape(person(&payload["user"]).unwrap_or("Someone"));
    let link = issue_link(issue);
    let scopes = project_of(issue);
    match s(payload, &["webhookEvent"]) {
        "jira:issue_created" => {
            let mut text = format!("**{who}** created {link}");
            if let Some(project) = Some(s(issue, &["fields", "project", "name"]))
                .filter(|name| !name.trim().is_empty())
            {
                text.push_str(&format!(" in {}", escape(project)));
            }
            if let Some(quote) = excerpt(&text_of(&issue["fields"]["description"])) {
                text.push_str("\n\n");
                text.push_str(&quote);
            }
            Some(Summarized {
                kind: "issue_created",
                scopes,
                label: format!("{key} created").trim().to_owned(),
                text,
            })
        }
        "jira:issue_updated" => {
            let items = payload["changelog"]["items"].as_array()?;
            let change = |field: &str| {
                items
                    .iter()
                    .find(|item| item["field"].as_str() == Some(field))
            };
            if let Some(status) = change("status") {
                let to = s(status, &["toString"]);
                let text = if to.is_empty() {
                    format!("**{who}** changed the status of {link}")
                } else {
                    format!("**{who}** moved {link} to **{}**", escape(to))
                };
                return Some(Summarized {
                    kind: "issue_status",
                    scopes,
                    label: format!("{key} status change").trim().to_owned(),
                    text,
                });
            }
            if let Some(assignee) = change("assignee") {
                let to = s(assignee, &["toString"]);
                let text = if to.is_empty() {
                    format!("**{who}** unassigned {link}")
                } else {
                    format!("**{who}** assigned {link} to **{}**", escape(to))
                };
                return Some(Summarized {
                    kind: "issue_assigned",
                    scopes,
                    label: format!("{key} assignment").trim().to_owned(),
                    text,
                });
            }
            None
        }
        "comment_created" => {
            let comment = &payload["comment"];
            let author = person(&comment["author"])
                .map(escape)
                .unwrap_or_else(|| who.clone());
            let mut text = format!("**{author}** commented on {link}");
            if let Some(quote) = excerpt(&text_of(&comment["body"])) {
                text.push_str("\n\n");
                text.push_str(&quote);
            }
            Some(Summarized {
                kind: "comment",
                scopes,
                label: format!("{key} comment").trim().to_owned(),
                text,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use lazy_domain::MessageDocument;
    use serde_json::json;

    use super::*;

    fn shown(summary: &Summarized) -> String {
        MessageDocument::from_markdown(&summary.text)
            .unwrap()
            .plain_text()
    }

    fn issue() -> Value {
        json!({
            "id": "99291", "key": "WEB-42",
            "self": "https://acme.atlassian.net/rest/api/2/issue/99291",
            "fields": {"summary": "Checkout *fails*", "project": {"key": "WEB", "name": "Website"},
                       "description": "h2. Steps\n{code}\nclick pay\n{code}\nIt @breaks."}
        })
    }

    #[test]
    fn the_signature_is_websub_sha256() {
        // Atlassian's documented test vector.
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hub-signature",
            "sha256=a4771c39fbe90f317c7824e83ddef3caae9cb3d976c214ace1f2937e133263c9"
                .parse()
                .unwrap(),
        );
        assert!(signature_ok(
            b"It's a Secret to Everybody",
            &headers,
            b"Hello World!"
        ));
        assert!(!signature_ok(b"wrong", &headers, b"Hello World!"));
    }

    #[test]
    fn created_updated_and_commented() {
        let created = json!({"webhookEvent": "jira:issue_created",
                             "user": {"displayName": "Ada"}, "issue": issue()});
        let summary = summarize(&created).unwrap();
        assert_eq!(
            (summary.kind, summary.scopes.clone()),
            ("issue_created", vec!["WEB".to_owned()])
        );
        assert_eq!(
            shown(&summary),
            "Ada created WEB-42 Checkout *fails* (https://acme.atlassian.net/browse/WEB-42) in \
             Website\n\n> Steps\n> click pay\n> It @breaks."
        );
        let moved = json!({"webhookEvent": "jira:issue_updated", "user": {"displayName": "Ada"},
            "issue": issue(),
            "changelog": {"items": [{"field": "status", "fromString": "To Do", "toString": "In Progress"}]}});
        let summary = summarize(&moved).unwrap();
        assert_eq!(summary.kind, "issue_status");
        assert!(
            shown(&summary).ends_with("to In Progress"),
            "{}",
            shown(&summary)
        );
        let assigned = json!({"webhookEvent": "jira:issue_updated", "user": {"displayName": "Ada"},
            "issue": issue(),
            "changelog": {"items": [{"field": "assignee", "fromString": null, "toString": "Bo"}]}});
        assert_eq!(summarize(&assigned).unwrap().kind, "issue_assigned");
        let relabeled = json!({"webhookEvent": "jira:issue_updated", "issue": issue(),
            "changelog": {"items": [{"field": "labels", "toString": "x"}]}});
        assert_eq!(summarize(&relabeled), None);
        // A comment whose body is an Atlassian document.
        let comment = json!({"webhookEvent": "comment_created", "issue": issue(),
            "comment": {"author": {"displayName": "Bo"},
                        "body": {"type": "doc", "content": [{"type": "paragraph",
                                 "content": [{"type": "text", "text": "Fixed in "},
                                             {"type": "text", "text": "v2"}]}]}}});
        let summary = summarize(&comment).unwrap();
        assert_eq!(summary.kind, "comment");
        assert_eq!(
            shown(&summary),
            "Bo commented on WEB-42 Checkout *fails* (https://acme.atlassian.net/browse/WEB-42)\
             \n\n> Fixed in v2"
        );
        assert_eq!(
            summarize(&json!({"webhookEvent": "jira:issue_deleted", "issue": issue()})),
            None
        );
    }

    #[test]
    fn an_odd_address_is_not_linked() {
        let odd =
            json!({"key": "WEB-1", "self": "javascript:alert(1)", "fields": {"summary": "x"}});
        assert_eq!(browse_url(&odd), "");
        assert!(!issue_link(&odd).contains("]("), "{}", issue_link(&odd));
    }
}
