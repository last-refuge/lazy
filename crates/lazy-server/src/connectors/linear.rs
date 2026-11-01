//! Linear's deliveries (D34): `Linear-Signature` is the hex HMAC-SHA256 of the body under the
//! webhook's signing secret, `Linear-Delivery` names the delivery, and `webhookTimestamp` (Unix
//! milliseconds) must be recent, against replays.
//!
//! The payload is `{action, type, data, url, updatedFrom, actor}`, where `data` is the entity
//! (Issue, Comment, Project, ProjectUpdate). Linear does not document every field of `data`, so
//! each message falls back on what is always there: the entity's `url`, which for an issue or a
//! comment holds the issue's identifier (`/issue/ENG-12/...`), and so its team's key.

use axum::http::HeaderMap;
use serde_json::Value;
use time::OffsetDateTime;

use super::{Received, Summarized, header, hmac_hex_ok, s};
use crate::provider_text::{escape, excerpt};

/// How far a delivery's timestamp may be from now, either way (Linear suggests a minute; this
/// allows for clocks that disagree a little).
const FRESH_SECONDS: i64 = 300;

pub(super) fn signature_ok(secret: &[u8], headers: &HeaderMap, body: &[u8]) -> bool {
    hmac_hex_ok(secret, body, header(headers, "linear-signature"))
}

/// Whether the delivery was sent recently enough to be taken.
pub(super) fn fresh(payload: &Value, now: OffsetDateTime) -> bool {
    let Some(sent) = payload["webhookTimestamp"].as_i64() else {
        return false;
    };
    let now_ms = i64::try_from(now.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX);
    (now_ms - sent).abs() <= FRESH_SECONDS * 1000
}

pub(super) fn receive(headers: &HeaderMap, payload: &Value) -> Received {
    Received {
        delivery: header(headers, "linear-delivery")
            .unwrap_or_default()
            .to_owned(),
        summary: summarize(payload),
    }
}

/// The issue identifier (`ENG-12`) in an issue or comment address.
fn identifier_in(url: &str) -> Option<&str> {
    let rest = &url[url.find("/issue/")? + "/issue/".len()..];
    let id = rest.split(['/', '#', '?']).next()?;
    let (team, number) = id.split_once('-')?;
    (!team.is_empty() && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
        .then_some(id)
}

fn team_of(identifier: &str) -> String {
    identifier
        .split_once('-')
        .map(|(team, _)| team.to_ascii_uppercase())
        .unwrap_or_default()
}

/// Who did it: the actor's name, or the entity's author, or "Someone".
fn actor(payload: &Value) -> String {
    let name = [
        s(payload, &["actor", "name"]),
        s(payload, &["data", "user", "name"]),
        s(payload, &["data", "creator", "name"]),
    ]
    .into_iter()
    .find(|name| !name.trim().is_empty())
    .unwrap_or("Someone");
    escape(name)
}

/// `[ENG-12 Title](url)`, with whatever of the two is known.
fn issue_link(identifier: &str, title: &str, url: &str) -> String {
    let shown = match (identifier.is_empty(), title.trim().is_empty()) {
        (false, false) => format!("{identifier} {title}"),
        (false, true) => identifier.to_owned(),
        (true, false) => title.to_owned(),
        (true, true) => "an issue".to_owned(),
    };
    lazy_domain::markdown::link_source(url, &shown)
}

fn with_excerpt(mut text: String, body: &str) -> String {
    if let Some(quote) = excerpt(body) {
        text.push_str("\n\n");
        text.push_str(&quote);
    }
    text
}

pub(super) fn summarize(payload: &Value) -> Option<Summarized> {
    let action = s(payload, &["action"]);
    let data = &payload["data"];
    let url = match s(payload, &["url"]) {
        "" => s(data, &["url"]),
        url => url,
    };
    let who = actor(payload);
    match s(payload, &["type"]) {
        "Issue" => {
            let identifier = match s(data, &["identifier"]) {
                "" => identifier_in(url).unwrap_or_default(),
                id => id,
            };
            let team = match s(data, &["team", "key"]) {
                "" => team_of(identifier),
                key => key.to_ascii_uppercase(),
            };
            let link = issue_link(identifier, s(data, &["title"]), url);
            let scopes = if team.is_empty() { vec![] } else { vec![team] };
            let from = &payload["updatedFrom"];
            match action {
                "create" => {
                    let mut text = format!("**{who}** created {link}");
                    if let Some(team) = Some(s(data, &["team", "name"])).filter(|t| !t.is_empty()) {
                        text.push_str(&format!(" in {}", escape(team)));
                    }
                    Some(Summarized {
                        kind: "issue_created",
                        scopes,
                        label: format!("{identifier} created").trim().to_owned(),
                        text: with_excerpt(text, s(data, &["description"])),
                    })
                }
                "update" if from.get("stateId").is_some() => {
                    let state = s(data, &["state", "name"]);
                    let text = if state.is_empty() {
                        format!("**{who}** changed the status of {link}")
                    } else {
                        format!("**{who}** moved {link} to **{}**", escape(state))
                    };
                    Some(Summarized {
                        kind: "issue_status",
                        scopes,
                        label: format!("{identifier} status change").trim().to_owned(),
                        text,
                    })
                }
                "update" if from.get("assigneeId").is_some() => {
                    let assignee = s(data, &["assignee", "name"]);
                    let text = if data["assigneeId"].is_null() && assignee.is_empty() {
                        format!("**{who}** unassigned {link}")
                    } else if assignee.is_empty() {
                        format!("**{who}** reassigned {link}")
                    } else {
                        format!("**{who}** assigned {link} to **{}**", escape(assignee))
                    };
                    Some(Summarized {
                        kind: "issue_assigned",
                        scopes,
                        label: format!("{identifier} assignment").trim().to_owned(),
                        text,
                    })
                }
                _ => None,
            }
        }
        "Comment" if action == "create" => {
            let identifier = match s(data, &["issue", "identifier"]) {
                "" => identifier_in(url).unwrap_or_default(),
                id => id,
            };
            let team = match s(data, &["issue", "team", "key"]) {
                "" => team_of(identifier),
                key => key.to_ascii_uppercase(),
            };
            let link = issue_link(identifier, s(data, &["issue", "title"]), url);
            Some(Summarized {
                kind: "comment",
                scopes: if team.is_empty() { vec![] } else { vec![team] },
                label: format!("{identifier} comment").trim().to_owned(),
                text: with_excerpt(format!("**{who}** commented on {link}"), s(data, &["body"])),
            })
        }
        "Project" => {
            let name = match s(data, &["name"]) {
                "" => "a project",
                name => name,
            };
            let link = lazy_domain::markdown::link_source(url, name);
            let scopes = teams_of(data);
            let from = &payload["updatedFrom"];
            let text = match action {
                "create" => format!("**{who}** created project {link}"),
                "update" if from.get("statusId").is_some() || from.get("state").is_some() => {
                    let status = match s(data, &["status", "name"]) {
                        "" => s(data, &["state"]),
                        status => status,
                    };
                    if status.is_empty() {
                        format!("**{who}** changed the status of project {link}")
                    } else {
                        format!("**{who}** moved project {link} to **{}**", escape(status))
                    }
                }
                _ => return None,
            };
            Some(Summarized {
                kind: "project",
                scopes,
                label: format!("Project {name}"),
                text,
            })
        }
        "ProjectUpdate" if action == "create" => {
            let project = &data["project"];
            let name = match s(project, &["name"]) {
                "" => "a project",
                name => name,
            };
            let link = lazy_domain::markdown::link_source(url, name);
            let health = match s(data, &["health"]) {
                "onTrack" => " (on track)",
                "atRisk" => " (at risk)",
                "offTrack" => " (off track)",
                _ => "",
            };
            Some(Summarized {
                kind: "project",
                scopes: teams_of(project),
                label: format!("Update on {name}"),
                text: with_excerpt(
                    format!("**{who}** posted an update on {link}{health}"),
                    s(data, &["body"]),
                ),
            })
        }
        _ => None,
    }
}

/// The keys of a project's teams, if the payload names them.
fn teams_of(project: &Value) -> Vec<String> {
    let mut keys: Vec<String> = project["teams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|team| team["key"].as_str())
        .filter(|key| !key.is_empty())
        .map(str::to_ascii_uppercase)
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
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

    #[test]
    fn freshness_allows_a_little_skew_only() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let at = |ms: i64| json!({ "webhookTimestamp": ms });
        assert!(fresh(&at(1_700_000_000_000), now));
        assert!(fresh(&at(1_700_000_000_000 - 299_000), now));
        assert!(fresh(&at(1_700_000_000_000 + 299_000), now));
        assert!(!fresh(&at(1_700_000_000_000 - 301_000), now));
        assert!(!fresh(&json!({}), now));
    }

    #[test]
    fn issues() {
        let created = json!({
            "action": "create", "type": "Issue",
            "actor": {"name": "Ada *L*"},
            "url": "https://linear.app/acme/issue/ENG-12/fix-login",
            "data": {"title": "Fix @login [now]", "description": "Steps:\n- `open` it",
                     "team": {"key": "eng", "name": "Engineering"}}
        });
        let summary = summarize(&created).unwrap();
        assert_eq!(
            (summary.kind, summary.scopes.clone()),
            ("issue_created", vec!["ENG".to_owned()])
        );
        assert_eq!(
            shown(&summary),
            "Ada *L* created ENG-12 Fix @login [now] (https://linear.app/acme/issue/ENG-12/fix-login) \
             in Engineering\n\n> Steps:\n> • open it"
        );
        assert_eq!(summary.label, "ENG-12 created");

        let moved = json!({
            "action": "update", "type": "Issue", "actor": {"name": "Ada"},
            "url": "https://linear.app/acme/issue/OPS-3/x",
            "data": {"identifier": "OPS-3", "title": "Disk", "state": {"name": "Done"}},
            "updatedFrom": {"stateId": "old", "updatedAt": "t"}
        });
        let summary = summarize(&moved).unwrap();
        assert_eq!(
            (summary.kind, summary.scopes.clone()),
            ("issue_status", vec!["OPS".to_owned()])
        );
        assert_eq!(
            shown(&summary),
            "Ada moved OPS-3 Disk (https://linear.app/acme/issue/OPS-3/x) to Done"
        );

        let assigned = json!({
            "action": "update", "type": "Issue", "actor": {"name": "Ada"},
            "url": "https://linear.app/acme/issue/OPS-3/x",
            "data": {"title": "Disk", "assigneeId": "u2", "assignee": {"name": "Bo"}},
            "updatedFrom": {"assigneeId": null}
        });
        assert_eq!(
            shown(&summarize(&assigned).unwrap()),
            "Ada assigned OPS-3 Disk (https://linear.app/acme/issue/OPS-3/x) to Bo"
        );
        let unassigned = json!({
            "action": "update", "type": "Issue", "actor": {"name": "Ada"},
            "url": "https://linear.app/acme/issue/OPS-3/x",
            "data": {"title": "Disk", "assigneeId": null},
            "updatedFrom": {"assigneeId": "u2"}
        });
        assert!(shown(&summarize(&unassigned).unwrap()).starts_with("Ada unassigned OPS-3"));
        // Other changes, and removals, post nothing.
        let renamed =
            json!({"action": "update", "type": "Issue", "data": {}, "updatedFrom": {"title": "x"}});
        assert_eq!(summarize(&renamed), None);
        assert_eq!(
            summarize(&json!({"action": "remove", "type": "Issue", "data": {}})),
            None
        );
    }

    #[test]
    fn comments_use_the_documented_shape() {
        // Linear's own example: no issue object, only the address.
        let comment = json!({
            "action": "create", "type": "Comment",
            "actor": {"id": "b5ea", "type": "user", "name": "Linear Orbit"},
            "data": {"body": "Indeed, I think this is **definitely** an improvement.",
                     "issueId": "5390", "userId": "aacd"},
            "url": "https://linear.app/issue/LIN-1778/foo-bar#comment-77217de3",
            "webhookTimestamp": 1676056940508_i64
        });
        let summary = summarize(&comment).unwrap();
        assert_eq!(
            (summary.kind, summary.scopes.clone()),
            ("comment", vec!["LIN".to_owned()])
        );
        assert_eq!(
            shown(&summary),
            "Linear Orbit commented on LIN-1778 \
             (https://linear.app/issue/LIN-1778/foo-bar#comment-77217de3)\n\n\
             > Indeed, I think this is definitely an improvement."
        );
    }

    #[test]
    fn projects_and_updates() {
        let update = json!({
            "action": "create", "type": "ProjectUpdate", "actor": {"name": "Ada"},
            "url": "https://linear.app/acme/project/launch/updates",
            "data": {"body": "Shipping Friday.", "health": "atRisk",
                     "project": {"name": "Launch", "teams": [{"key": "eng"}, {"key": "WEB"}]}}
        });
        let summary = summarize(&update).unwrap();
        assert_eq!(summary.kind, "project");
        assert_eq!(summary.scopes, ["ENG", "WEB"]);
        assert_eq!(
            shown(&summary),
            "Ada posted an update on Launch (https://linear.app/acme/project/launch/updates) \
             (at risk)\n\n> Shipping Friday."
        );
        let moved = json!({
            "action": "update", "type": "Project", "actor": {"name": "Ada"},
            "url": "https://linear.app/acme/project/launch",
            "data": {"name": "Launch", "status": {"name": "Completed"}},
            "updatedFrom": {"statusId": "s1"}
        });
        assert!(shown(&summarize(&moved).unwrap()).ends_with("to Completed"));
        assert_eq!(
            summarize(&json!({"action": "create", "type": "Cycle", "data": {}})),
            None
        );
    }
}
