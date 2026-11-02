use lazy_domain::{Block, Inline, MessageDocument};
use serde_json::json;

use super::*;

/// The message a body posts, as plain text (links show their address in parentheses).
fn shown(body: Value) -> String {
    let markdown = to_markdown(&body).expect("something to post");
    MessageDocument::from_markdown(&markdown)
        .expect("valid")
        .plain_text()
}

fn document(body: Value) -> MessageDocument {
    MessageDocument::from_markdown(&to_markdown(&body).unwrap()).unwrap()
}

fn links(document: &MessageDocument) -> Vec<(String, String)> {
    fn walk(inlines: &[Inline], out: &mut Vec<(String, String)>) {
        for inline in inlines {
            if let Inline::Link { url, text, .. } = inline {
                out.push((text.clone(), url.clone()));
            }
        }
    }
    let mut out = Vec::new();
    for block in document.blocks() {
        match block {
            Block::Paragraph { inlines } => walk(inlines, &mut out),
            Block::Quote { paragraphs } => paragraphs.iter().for_each(|p| walk(p, &mut out)),
            Block::BulletList { items } | Block::OrderedList { items, .. } => {
                items.iter().for_each(|i| walk(i, &mut out))
            }
            _ => {}
        }
    }
    out
}

#[test]
fn lazys_own_text_reads_as_before() {
    assert_eq!(shown(json!({"text": "Deploy *done*"})), "Deploy done");
    let doc = document(json!({"text": "see [the run](https://ci.example/1) now"}));
    assert_eq!(
        links(&doc),
        [("the run".into(), "https://ci.example/1".into())]
    );
    // Angle brackets that are not Slack's syntax stay as written.
    assert_eq!(
        shown(json!({"text": "a < b > c and <none>"})),
        "a < b > c and <none>"
    );
}

#[test]
fn slack_links_names_and_entities() {
    let doc = document(json!({
        "text": "Build <https://ci.example/runs/7?a=1&amp;b=2|#7> failed for <@U024BE7LH|alice> \
                 in <#C024BE7LR|deploys>. <!channel> see <https://x.example/(paren>, \
                 <mailto:ops@example.com|mail ops> &amp; 1 &lt; 2"
    }));
    assert_eq!(
        doc.plain_text(),
        "Build #7 (https://ci.example/runs/7?a=1&b=2) failed for @alice in #deploys. @channel \
         see https://x.example/(paren, mail ops (mailto:ops@example.com) & 1 < 2"
    );
    let links = links(&doc);
    assert!(links.contains(&("#7".into(), "https://ci.example/runs/7?a=1&b=2".into())));
    assert!(links.contains(&("mail ops".into(), "mailto:ops@example.com".into())));
    // Names never become mentions: a webhook notifies nobody.
    let markdown = to_markdown(&json!({"text": "<!here> <@U1> <!subteam^S1|@oncall>"})).unwrap();
    let doc = MessageDocument::from_markdown(&markdown).unwrap();
    assert!(
        doc.blocks().iter().all(|block| match block {
            Block::Paragraph { inlines } =>
                inlines.iter().all(|i| !matches!(i, Inline::Mention { .. })),
            _ => true,
        }),
        "{markdown}"
    );
    assert_eq!(doc.plain_text(), "@here @U1 @oncall");
    // A date shows its fallback.
    assert_eq!(
        shown(json!({"text": "Due <!date^1392734382^{date_short}|Feb 18, 2014>"})),
        "Due Feb 18, 2014"
    );
}

#[test]
fn plain_text_when_mrkdwn_is_off() {
    assert_eq!(
        shown(json!({"text": "*not bold* &amp; [x](https://a.b)", "mrkdwn": false})),
        "*not bold* & [x](https://a.b)"
    );
}

#[test]
fn one_line_code_fences_become_blocks() {
    let doc = document(json!({"text": "Output:\n```cargo test```"}));
    assert!(
        doc.blocks()
            .iter()
            .any(|b| matches!(b, Block::CodeBlock { text, .. } if text == "cargo test"))
    );
}

#[test]
fn blocks_replace_text_as_in_slack() {
    let body = json!({
        "text": "fallback only",
        "blocks": [
            {"type": "header", "text": {"type": "plain_text", "text": "Deployment *finished*"}},
            {"type": "section", "text": {"type": "mrkdwn", "text": "App <https://app.example|web> is live"},
             "fields": [
                {"type": "mrkdwn", "text": "*Branch:*\nmain"},
                {"type": "plain_text", "text": "Took 3m"}
             ],
             "accessory": {"type": "button", "text": {"type": "plain_text", "text": "Logs"}, "url": "https://logs.example"}},
            {"type": "divider"},
            {"type": "context", "elements": [
                {"type": "image", "image_url": "https://i.example/x.png", "alt_text": "x"},
                {"type": "mrkdwn", "text": "by coolify"}, {"type": "plain_text", "text": "11:02"}
            ]},
            {"type": "image", "image_url": "https://i.example/chart.png", "alt_text": "CPU chart"},
            {"type": "actions", "elements": [
                {"type": "button", "text": {"type": "plain_text", "text": "Open"}, "url": "https://open.example"},
                {"type": "button", "text": {"type": "plain_text", "text": "Approve"}, "action_id": "approve"}
            ]},
            {"type": "input", "label": {"type": "plain_text", "text": "ignored"}}
        ]
    });
    let doc = document(body.clone());
    let text = doc.plain_text();
    assert!(!text.contains("fallback only"), "{text}");
    assert!(text.starts_with("Deployment *finished*"), "{text}");
    for expected in [
        "App web",
        "Branch:",
        "main",
        "Took 3m",
        "Logs",
        "by coolify · 11:02",
        "CPU chart",
        "Open",
    ] {
        assert!(text.contains(expected), "{expected} in {text}");
    }
    assert!(
        !text.contains("Approve") && !text.contains("ignored"),
        "{text}"
    );
    let links = links(&doc);
    for url in [
        "https://app.example",
        "https://logs.example",
        "https://i.example/chart.png",
        "https://open.example",
    ] {
        assert!(links.iter().any(|(_, u)| u == url), "{url} in {links:?}");
    }
    // Empty blocks fall back to the text.
    assert_eq!(
        shown(json!({"text": "hi", "blocks": [{"type": "divider"}]})),
        "hi"
    );
}

#[test]
fn rich_text_blocks() {
    let body = json!({"blocks": [{"type": "rich_text", "elements": [
        {"type": "rich_text_section", "elements": [
            {"type": "text", "text": "Hello "},
            {"type": "text", "text": "world", "style": {"bold": true}},
            {"type": "text", "text": " and "},
            {"type": "text", "text": "x = 1", "style": {"code": true}},
            {"type": "text", "text": " "},
            {"type": "link", "url": "https://a.example", "text": "a link"},
            {"type": "text", "text": " "},
            {"type": "emoji", "name": "tada", "unicode": "1f389"},
            {"type": "user", "user_id": "U1"}
        ]},
        {"type": "rich_text_list", "style": "ordered", "elements": [
            {"type": "rich_text_section", "elements": [{"type": "text", "text": "one"}]},
            {"type": "rich_text_section", "elements": [{"type": "text", "text": "two"}]}
        ]},
        {"type": "rich_text_quote", "elements": [{"type": "text", "text": "quoted"}]},
        {"type": "rich_text_preformatted", "elements": [{"type": "text", "text": "let x = 1;"}]}
    ]}]});
    let doc = document(body);
    let text = doc.plain_text();
    assert!(
        text.starts_with("Hello world and x = 1 a link (https://a.example) 🎉@U1"),
        "{text}"
    );
    assert!(
        text.contains("one") && text.contains("two") && text.contains("quoted"),
        "{text}"
    );
    assert!(
        doc.blocks()
            .iter()
            .any(|b| matches!(b, Block::CodeBlock { text, .. } if text == "let x = 1;"))
    );
    assert!(
        doc.blocks()
            .iter()
            .any(|b| matches!(b, Block::OrderedList { .. }))
    );
    assert!(
        doc.blocks()
            .iter()
            .any(|b| matches!(b, Block::Quote { .. }))
    );
}

#[test]
fn legacy_attachments_become_quotes() {
    // The shape Grafana, Alertmanager, Jenkins and older Sentry send.
    let body = json!({
        "username": "Grafana", "icon_emoji": ":chart:", "channel": "#alerts",
        "attachments": [{
            "fallback": "[Alerting] CPU high",
            "color": "#D63232",
            "pretext": "Alert fired",
            "author_name": "Grafana", "author_link": "https://grafana.example",
            "title": "[Alerting] CPU high", "title_link": "https://grafana.example/d/1",
            "text": "CPU above *90%* for 5m",
            "fields": [
                {"title": "Host", "value": "web-1", "short": true},
                {"title": "Value", "value": "93.2", "short": true}
            ],
            "image_url": "https://grafana.example/render.png",
            "footer": "Grafana v11", "ts": 1730459000
        }]
    });
    let doc = document(body);
    assert!(
        matches!(doc.blocks()[0], Block::Paragraph { .. }),
        "pretext above"
    );
    assert!(
        matches!(doc.blocks()[1], Block::Quote { .. }),
        "the rest quoted"
    );
    let text = doc.plain_text();
    for expected in [
        "Alert fired",
        "Grafana",
        "[Alerting] CPU high",
        "CPU above 90% for 5m",
        "Host: web-1",
        "Value: 93.2",
        "Image",
        "Grafana v11",
    ] {
        assert!(text.contains(expected), "{expected} in {text}");
    }
    let links = links(&doc);
    assert!(
        links.contains(&(
            "[Alerting] CPU high".into(),
            "https://grafana.example/d/1".into()
        )),
        "{links:?}"
    );
    // Only a fallback: it is shown.
    assert_eq!(
        shown(json!({"attachments": [{"fallback": "just this", "color": "good"}]})),
        "> just this"
    );
    // Text and attachments both show, text first.
    let text = shown(json!({"text": "Heads up", "attachments": [{"text": "detail"}]}));
    assert!(
        text.starts_with("Heads up") && text.ends_with("detail"),
        "{text}"
    );
}

#[test]
fn nothing_to_post() {
    for body in [
        json!({}),
        json!({"text": "  "}),
        json!({"attachments": [{"color": "good"}]}),
        json!({"blocks": [{"type": "divider"}]}),
        json!({"text": "", "blocks": []}),
    ] {
        assert_eq!(to_markdown(&body), None, "{body}");
    }
}

#[test]
fn bodies_as_json_or_a_payload_form() {
    let json_body = br#"{"text":"hi"}"#;
    assert_eq!(
        parse_body(Some("application/json"), json_body),
        Some(json!({"text": "hi"}))
    );
    assert_eq!(parse_body(None, json_body), Some(json!({"text": "hi"})));
    let form = b"payload=%7B%22text%22%3A%22hi+%26+bye%22%7D";
    let expected = Some(json!({"text": "hi & bye"}));
    assert_eq!(
        parse_body(
            Some("application/x-www-form-urlencoded; charset=utf-8"),
            form
        ),
        expected
    );
    assert_eq!(parse_body(None, form), expected);
    assert_eq!(
        parse_body(Some("application/x-www-form-urlencoded"), b"text=hi"),
        None
    );
    assert_eq!(parse_body(None, b"not json"), None);
}

#[test]
fn hostile_input_stays_text() {
    // Script and odd schemes never become links; a label cannot smuggle markup.
    let doc = document(
        json!({"text": "<javascript:alert(1)|click> <https://ok.example|[x](https://evil.example)>"}),
    );
    let links = links(&doc);
    assert_eq!(
        links,
        [(
            "[x](https://evil.example)".into(),
            "https://ok.example".into()
        )]
    );
    // Deep nesting and long input do not blow up.
    let deep = "<".repeat(5000) + &">".repeat(5000);
    assert!(to_markdown(&json!({"text": deep})).is_some());
}

#[test]
fn blocks_become_rich_content_with_buttons_for_apps_only() {
    let body = json!({
        "text": "Deploy 42 is waiting",
        "blocks": [
            {"type": "header", "text": {"type": "plain_text", "text": "Deploy   42\nwaiting"}},
            {"type": "section", "text": {"type": "mrkdwn", "text": "*api* to <https://p.example|prod>"},
             "fields": [{"type": "mrkdwn", "text": "*By*\nalice"}],
             "accessory": {"type": "button", "text": {"type": "plain_text", "text": "Logs"},
                           "url": "https://ci.example/42"}},
            {"type": "divider"},
            {"type": "image", "image_url": "https://img.example/a.png", "alt_text": "graph"},
            {"type": "context", "elements": [{"type": "image", "image_url": "x"},
                                             {"type": "mrkdwn", "text": "3 min ago"}]},
            {"type": "actions", "block_id": "deploy", "elements": [
                {"type": "button", "action_id": "ok", "style": "primary", "value": "42",
                 "text": {"type": "plain_text", "text": "Approve"}},
                {"type": "button", "action_id": "no", "style": "danger",
                 "text": {"type": "plain_text", "text": "Reject"}},
                {"type": "static_select", "action_id": "pick"}
            ]},
            {"type": "video", "title": "unknown kind"}
        ],
        "attachments": [{"title": "Older", "text": "kept"}]
    });
    let rich = to_rich(&body, true).expect("rich");
    let blocks = rich.blocks();
    assert_eq!(
        blocks[0],
        RichBlock::Header {
            text: "Deploy 42 waiting".into()
        }
    );
    let RichBlock::Section {
        text: Some(text),
        fields,
        accessory: Some(logs),
    } = &blocks[1]
    else {
        panic!("section: {:?}", blocks[1]);
    };
    assert_eq!(text.plain_text(), "api to prod (https://p.example)");
    assert_eq!(fields.len(), 1);
    assert_eq!(logs.url.as_deref(), Some("https://ci.example/42"));
    assert_eq!(blocks[2], RichBlock::Divider);
    // An image is a link, never fetched.
    let RichBlock::Section {
        text: Some(image), ..
    } = &blocks[3]
    else {
        panic!("image");
    };
    assert_eq!(image.plain_text(), "graph (https://img.example/a.png)");
    assert!(matches!(&blocks[4], RichBlock::Context { items } if items.len() == 1));
    let RichBlock::Actions { block_id, buttons } = &blocks[5] else {
        panic!("actions");
    };
    assert_eq!(block_id.as_deref(), Some("deploy"));
    assert_eq!(buttons.len(), 2);
    assert_eq!(buttons[0].style, ButtonStyle::Primary);
    assert_eq!(buttons[0].value.as_deref(), Some("42"));
    assert_eq!(buttons[1].style, ButtonStyle::Danger);
    assert!(rich.button(5, "ok").is_some_and(Button::is_action));
    // The legacy attachment follows, so nothing the text shows is lost.
    assert_eq!(blocks.len(), 7);

    // A webhook cannot answer presses: its action buttons go, link buttons stay.
    let plain = to_rich(&body, false).unwrap();
    assert!(!plain.has_actions());
    assert!(
        !plain
            .blocks()
            .iter()
            .any(|block| matches!(block, RichBlock::Actions { .. }))
    );
    assert!(plain.button(1, "button-1").is_some());

    // Text alone, or blocks that show nothing, are not rich.
    assert_eq!(to_rich(&json!({"text": "hi"}), true), None);
    assert_eq!(to_rich(&json!({"blocks": [{"type": "video"}]}), true), None);
}
