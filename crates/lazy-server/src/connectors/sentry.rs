//! Sentry's deliveries (D37), from an internal integration an admin makes in Sentry's Settings,
//! Developer Settings: `Sentry-Hook-Signature` is the hex HMAC-SHA256 of the body under the
//! integration's Client Secret (pasted into Lazy), `Sentry-Hook-Resource` says what it is about,
//! and `Request-ID` names the delivery.
//!
//! Issues name their project (`data.issue.project.slug`) and metric alerts theirs
//! (`data.metric_alert.projects`); an issue alert names only a numeric project id, so it reaches
//! channels that follow everything.

use axum::http::HeaderMap;
use serde_json::Value;

use super::{Received, Summarized, header, hmac_hex_ok, s};
use crate::provider_text::{escape, excerpt};

pub(super) fn signature_ok(secret: &[u8], headers: &HeaderMap, body: &[u8]) -> bool {
    hmac_hex_ok(secret, body, header(headers, "sentry-hook-signature"))
}

pub(super) fn receive(headers: &HeaderMap, payload: &Value) -> Received {
    Received {
        delivery: header(headers, "request-id").unwrap_or_default().to_owned(),
        summary: summarize(
            header(headers, "sentry-hook-resource").unwrap_or_default(),
            payload,
        ),
    }
}

fn link(url: &str, text: &str) -> String {
    lazy_domain::markdown::link_source(url, text)
}

/// Who did it, unless Sentry itself did.
fn actor(payload: &Value) -> Option<String> {
    let actor = &payload["actor"];
    if actor["id"] == "sentry" || actor["type"] == "application" {
        return None;
    }
    actor["name"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .map(escape)
}

pub(super) fn summarize(resource: &str, payload: &Value) -> Option<Summarized> {
    let action = s(payload, &["action"]);
    let data = &payload["data"];
    match resource {
        "issue" => {
            let issue = &data["issue"];
            let short = s(issue, &["shortId"]);
            let title = s(issue, &["title"]);
            let url = match s(issue, &["web_url"]) {
                "" => s(issue, &["permalink"]),
                url => url,
            };
            let shown = if short.is_empty() {
                title.to_owned()
            } else {
                format!("{short} {title}")
            };
            let issue_link = link(url, &shown);
            let project = s(issue, &["project", "slug"]);
            let project_name = match s(issue, &["project", "name"]) {
                "" => project,
                name => name,
            };
            let scopes = if project.is_empty() {
                vec![]
            } else {
                vec![project.to_ascii_uppercase()]
            };
            let who = actor(payload);
            let (kind, text) = match action {
                "created" => {
                    let mut text =
                        format!("New issue in **{}**: {issue_link}", escape(project_name));
                    let level = s(issue, &["level"]);
                    if !level.is_empty() {
                        text.push_str(&format!(" ({})", escape(level)));
                    }
                    let culprit = s(issue, &["culprit"]);
                    if let Some(quote) = excerpt(culprit) {
                        text.push_str("\n\n");
                        text.push_str(&quote);
                    }
                    ("issue_created", text)
                }
                "unresolved" => (
                    "issue_unresolved",
                    format!("Back again in **{}**: {issue_link}", escape(project_name)),
                ),
                "resolved" => (
                    "issue_resolved",
                    match who {
                        Some(who) => format!("**{who}** resolved {issue_link}"),
                        None => format!("Resolved: {issue_link}"),
                    },
                ),
                "assigned" => {
                    let to = issue["assignedTo"]["name"]
                        .as_str()
                        .or_else(|| issue["assignedTo"]["email"].as_str())
                        .filter(|name| !name.trim().is_empty());
                    let text = match (who, to) {
                        (Some(who), Some(to)) => {
                            format!("**{who}** assigned {issue_link} to **{}**", escape(to))
                        }
                        (None, Some(to)) => {
                            format!("{issue_link} was assigned to **{}**", escape(to))
                        }
                        (Some(who), None) => format!("**{who}** assigned {issue_link}"),
                        (None, None) => format!("{issue_link} was assigned"),
                    };
                    ("issue_assigned", text)
                }
                _ => return None,
            };
            Some(Summarized {
                kind,
                scopes,
                label: format!(
                    "{} {action}",
                    if short.is_empty() { "Issue" } else { short }
                ),
                text,
            })
        }
        "event_alert" if action == "triggered" => {
            let event = &data["event"];
            let rule = s(data, &["triggered_rule"]);
            let title = s(event, &["title"]);
            let url = match s(event, &["web_url"]) {
                "" => s(event, &["issue_url"]),
                url => url,
            };
            let text = if rule.is_empty() {
                format!("Alert: {}", link(url, title))
            } else {
                format!("Alert **{}**: {}", escape(rule), link(url, title))
            };
            Some(Summarized {
                kind: "alert",
                scopes: vec![],
                label: format!("Alert {rule}").trim().to_owned(),
                text,
            })
        }
        "metric_alert" if matches!(action, "critical" | "warning" | "resolved") => {
            let title = match s(data, &["description_title"]) {
                "" => s(data, &["metric_alert", "title"]),
                title => title,
            };
            let mut text = format!("**{}**", escape(title));
            let url = s(data, &["web_url"]);
            if !url.is_empty() {
                text.push_str(&format!(" ({})", link(url, "view")));
            }
            if let Some(quote) = excerpt(s(data, &["description_text"])) {
                text.push_str("\n\n");
                text.push_str(&quote);
            }
            let mut scopes: Vec<String> = data["metric_alert"]["projects"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_ascii_uppercase)
                .collect();
            scopes.sort_unstable();
            scopes.dedup();
            Some(Summarized {
                kind: "alert",
                scopes,
                label: format!("Metric alert {action}"),
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
        json!({"shortId": "WEB-3K", "title": "TypeError: x is *undefined*",
               "web_url": "https://acme.sentry.io/issues/42/", "level": "error",
               "culprit": "app/checkout.ts in pay",
               "project": {"slug": "web-app", "name": "Web app"}})
    }

    #[test]
    fn issues() {
        let created = json!({"action": "created", "data": {"issue": issue()},
                             "actor": {"type": "application", "id": "sentry", "name": "Sentry"}});
        let summary = summarize("issue", &created).unwrap();
        assert_eq!(
            (summary.kind, summary.scopes.clone()),
            ("issue_created", vec!["WEB-APP".to_owned()])
        );
        assert_eq!(
            shown(&summary),
            "New issue in Web app: WEB-3K TypeError: x is *undefined* \
             (https://acme.sentry.io/issues/42/) (error)\n\n> app/checkout.ts in pay"
        );
        let resolved = json!({"action": "resolved", "data": {"issue": issue()},
                              "actor": {"type": "user", "id": "1", "name": "Ada"}});
        assert!(shown(&summarize("issue", &resolved).unwrap()).starts_with("Ada resolved WEB-3K"));
        let mut assigned_issue = issue();
        assigned_issue["assignedTo"] = json!({"type": "user", "name": "Bo"});
        let assigned = json!({"action": "assigned", "data": {"issue": assigned_issue},
                              "actor": {"type": "user", "id": "1", "name": "Ada"}});
        assert!(shown(&summarize("issue", &assigned).unwrap()).ends_with("to Bo"));
        let back = json!({"action": "unresolved", "data": {"issue": issue()}});
        assert_eq!(summarize("issue", &back).unwrap().kind, "issue_unresolved");
        assert_eq!(
            summarize(
                "issue",
                &json!({"action": "archived", "data": {"issue": issue()}})
            ),
            None
        );
        assert_eq!(
            summarize("installation", &json!({"action": "created"})),
            None
        );
    }

    #[test]
    fn alerts() {
        let event = json!({"action": "triggered", "data": {
            "triggered_rule": "Checkout errors", "event": {"title": "Boom", "web_url": "https://acme.sentry.io/e/1/", "project": 7}}});
        let summary = summarize("event_alert", &event).unwrap();
        assert_eq!((summary.kind, summary.scopes.len()), ("alert", 0));
        assert_eq!(
            shown(&summary),
            "Alert Checkout errors: Boom (https://acme.sentry.io/e/1/)"
        );
        let metric = json!({"action": "critical", "data": {
            "description_title": "Critical: Too many errors", "description_text": "1000 events in 1h",
            "web_url": "https://acme.sentry.io/alerts/1/", "metric_alert": {"projects": ["web-app"]}}});
        let summary = summarize("metric_alert", &metric).unwrap();
        assert_eq!(summary.scopes, ["WEB-APP"]);
        assert_eq!(
            shown(&summary),
            "Critical: Too many errors (view (https://acme.sentry.io/alerts/1/))\n\n> 1000 events in 1h"
        );
    }
}
