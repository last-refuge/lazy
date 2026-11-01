//! PagerDuty's deliveries (D37), from a Generic Webhook (v3) an admin adds in PagerDuty's
//! Integrations: `X-PagerDuty-Signature` holds one or more `v1=<hex>` HMAC-SHA256 signatures of the
//! body (several during a secret's rotation; any one matching is enough) under the secret PagerDuty
//! shows when the webhook is made (pasted into Lazy). `X-Webhook-Id` names the delivery, the same
//! across retries.
//!
//! Incidents name their service (`data.service.id`), which is the subscription's scope; a note
//! names only its incident, so it reaches channels that follow everything.

use axum::http::HeaderMap;
use serde_json::Value;

use super::{Received, Summarized, header, hmac_hex_ok, s};
use crate::provider_text::{escape, excerpt};

pub(super) fn signature_ok(secret: &[u8], headers: &HeaderMap, body: &[u8]) -> bool {
    let Some(value) = header(headers, "x-pagerduty-signature") else {
        return false;
    };
    value
        .split(',')
        .filter_map(|part| part.trim().strip_prefix("v1="))
        .any(|hex| hmac_hex_ok(secret, body, Some(hex)))
}

pub(super) fn receive(headers: &HeaderMap, payload: &Value) -> Received {
    Received {
        delivery: header(headers, "x-webhook-id")
            .unwrap_or_default()
            .to_owned(),
        summary: summarize(payload),
    }
}

pub(super) fn summarize(payload: &Value) -> Option<Summarized> {
    let event = &payload["event"];
    let kind_name = s(event, &["event_type"]);
    let data = &event["data"];
    let who = event["agent"]["summary"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .map(escape);
    if kind_name == "incident.annotated" {
        let incident = &data["incident"];
        let text = format!(
            "**{}** added a note to {}",
            who.unwrap_or_else(|| "Someone".to_owned()),
            lazy_domain::markdown::link_source(
                s(incident, &["html_url"]),
                match s(incident, &["summary"]) {
                    "" => "an incident",
                    summary => summary,
                }
            )
        );
        let text = match excerpt(s(data, &["content"])) {
            Some(quote) => format!("{text}\n\n{quote}"),
            None => text,
        };
        return Some(Summarized {
            kind: "note",
            scopes: vec![],
            label: "Incident note".to_owned(),
            text,
        });
    }
    let number = data["number"].as_i64();
    let title = s(data, &["title"]);
    let shown = match number {
        Some(n) => format!("#{n} {title}"),
        None => title.to_owned(),
    };
    let incident = lazy_domain::markdown::link_source(s(data, &["html_url"]), &shown);
    let service = s(data, &["service", "summary"]);
    let on_service = if service.is_empty() {
        String::new()
    } else {
        format!(" on **{}**", escape(service))
    };
    let scopes = match s(data, &["service", "id"]) {
        "" => vec![],
        id => vec![id.to_ascii_uppercase()],
    };
    let by = |verb: &str| match &who {
        Some(who) => format!("**{who}** {verb} {incident}{on_service}"),
        None => format!("{incident}{on_service} was {verb}"),
    };
    let (kind, text) = match kind_name {
        "incident.triggered" => {
            let urgency = s(data, &["urgency"]);
            let mut text = format!("Triggered: {incident}{on_service}");
            if !urgency.is_empty() {
                text.push_str(&format!(" · {} urgency", escape(urgency)));
            }
            ("triggered", text)
        }
        "incident.acknowledged" => ("acknowledged", by("acknowledged")),
        "incident.resolved" => ("resolved", by("resolved")),
        "incident.escalated" => ("escalated", format!("{incident}{on_service} was escalated")),
        "incident.reassigned" => {
            let to: Vec<String> = data["assignees"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a["summary"].as_str())
                .map(escape)
                .collect();
            let base = by("reassigned");
            let text = if to.is_empty() {
                base
            } else {
                format!("{base} to **{}**", to.join(", "))
            };
            ("escalated", text)
        }
        _ => return None,
    };
    Some(Summarized {
        kind,
        scopes,
        label: format!(
            "{} {}",
            number
                .map(|n| format!("#{n}"))
                .unwrap_or_else(|| "Incident".to_owned()),
            kind_name.trim_start_matches("incident.")
        ),
        text,
    })
}

#[cfg(test)]
mod tests {
    use aws_lc_rs::hmac;
    use lazy_domain::MessageDocument;
    use serde_json::json;

    use super::*;

    fn shown(summary: &Summarized) -> String {
        MessageDocument::from_markdown(&summary.text)
            .unwrap()
            .plain_text()
    }

    fn incident(event_type: &str) -> Value {
        json!({"event": {"event_type": event_type, "agent": {"summary": "Ada"},
            "data": {"number": 12, "title": "Checkout down", "html_url": "https://acme.pagerduty.com/incidents/Q1",
                     "urgency": "high", "service": {"id": "PXYZ123", "summary": "Payments"},
                     "assignees": [{"summary": "Bo"}]}}})
    }

    #[test]
    fn any_of_several_signatures_is_enough() {
        let body = br#"{"event":{}}"#;
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"new-secret");
        let good: String = hmac::sign(&key, body)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-pagerduty-signature",
            format!("v1=deadbeef, v1={good}").parse().unwrap(),
        );
        assert!(signature_ok(b"new-secret", &headers, body));
        assert!(!signature_ok(b"other", &headers, body));
        headers.insert("x-pagerduty-signature", good.parse().unwrap());
        assert!(
            !signature_ok(b"new-secret", &headers, body),
            "the v1= prefix is required"
        );
    }

    #[test]
    fn incidents() {
        let summary = summarize(&incident("incident.triggered")).unwrap();
        assert_eq!(
            (summary.kind, summary.scopes.clone()),
            ("triggered", vec!["PXYZ123".to_owned()])
        );
        assert_eq!(
            shown(&summary),
            "Triggered: #12 Checkout down (https://acme.pagerduty.com/incidents/Q1) on Payments · high urgency"
        );
        assert!(
            shown(&summarize(&incident("incident.acknowledged")).unwrap())
                .starts_with("Ada acknowledged #12")
        );
        assert_eq!(
            summarize(&incident("incident.resolved")).unwrap().kind,
            "resolved"
        );
        assert!(shown(&summarize(&incident("incident.reassigned")).unwrap()).ends_with("to Bo"));
        assert_eq!(summarize(&incident("incident.priority_updated")), None);
        assert_eq!(
            summarize(&json!({"event": {"event_type": "pagey.ping"}})),
            None
        );
        let note = json!({"event": {"event_type": "incident.annotated", "agent": {"summary": "Ada"},
            "data": {"content": "Rolled back *v2*", "incident": {"summary": "Checkout down", "html_url": "https://x.pagerduty.com/i/1"}}}});
        let summary = summarize(&note).unwrap();
        assert_eq!((summary.kind, summary.scopes.len()), ("note", 0));
        assert_eq!(
            shown(&summary),
            "Ada added a note to Checkout down (https://x.pagerduty.com/i/1)\n\n> Rolled back *v2*"
        );
    }
}
