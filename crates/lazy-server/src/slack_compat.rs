//! Slack-compatible incoming webhook bodies (llm-docs/DECISIONS.md, D33).
//!
//! Many tools post to chat through a "Slack" option that sends Slack's incoming-webhook body. This
//! reads that body and writes the Lazy message it describes, so pasting a Lazy webhook address into
//! such an option works. Read here:
//!
//! - `text` in Slack's `mrkdwn` (`*bold*`, `_italic_`, `~strike~`, code, quotes, which Lazy's
//!   own syntax shares), with `mrkdwn: false` meaning plain text;
//! - Slack's angle-bracket syntax: `<url|label>` and `<url>` links, `<@U…|name>` and `<#C…|name>`,
//!   `<!channel>`/`<!here>`/`<!everyone>`, `<!date^…|fallback>`, and `&amp;`, `&lt;`, `&gt;`;
//! - `blocks` (Block Kit: header, section with fields, context, image, actions with link buttons,
//!   rich text, divider), shown instead of `text` as Slack does;
//! - legacy `attachments` (pretext, author, title, text, fields, image, footer, and any blocks),
//!   each shown as a quote, the nearest thing Lazy has to Slack's colored bar;
//! - a form-encoded body whose `payload` field holds the JSON, which older tools send.
//!
//! What cannot apply here is dropped quietly, as Slack itself drops it: `channel`, `username`,
//! `icon_*` (the webhook's own name and channel are used), colors, interactive elements without a
//! link, and unknown block types. Mentions are text: a webhook notifies nobody (D16), so
//! `<!channel>` reads "@channel" and pings no one.

use lazy_domain::markdown::{escape_plain, link_source};
use serde_json::Value;

/// Reads a webhook body as JSON, or as a form whose `payload` field is JSON.
pub fn parse_body(content_type: Option<&str>, bytes: &[u8]) -> Option<Value> {
    let form = content_type.is_some_and(|value| {
        value.split(';').next().is_some_and(|kind| {
            kind.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        })
    });
    if form || bytes.starts_with(b"payload=") {
        let payload = form_urlencoded::parse(bytes)
            .find(|(key, _)| key == "payload")
            .map(|(_, value)| value.into_owned())?;
        return serde_json::from_str(&payload).ok();
    }
    serde_json::from_slice(bytes).ok()
}

/// The Lazy Markdown for a Slack-style body, or `None` if it holds nothing to show.
pub fn to_markdown(body: &Value) -> Option<String> {
    let mut parts = Vec::new();
    let text = body["text"].as_str().unwrap_or_default();
    let blocks = blocks(&body["blocks"]);
    // As in Slack: blocks are the message, and `text` is only their notification fallback.
    match blocks {
        Some(blocks) => parts.push(blocks),
        None if !text.trim().is_empty() => parts.push(if body["mrkdwn"] == Value::Bool(false) {
            escape_plain(&decode_entities(text))
        } else {
            mrkdwn(text)
        }),
        None => {}
    }
    for attachment in body["attachments"].as_array().into_iter().flatten() {
        if let Some(shown) = attachment_markdown(attachment) {
            parts.push(shown);
        }
    }
    let joined = parts.join("\n\n");
    (!joined.trim().is_empty()).then_some(joined)
}

// ----- Slack's mrkdwn ----------------------------------------------------------------------------

/// Slack `mrkdwn` as Lazy Markdown. The two share emphasis, code, and quote syntax; this rewrites
/// what Slack writes differently: its angle-bracket links and names, its three HTML entities, and
/// code fences written on one line.
pub fn mrkdwn(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        out.push_str(&fence_line(line));
    }
    let text = out;
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(open) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..open]));
        let after = &rest[open + 1..];
        match after.find(['>', '\n', '<']) {
            Some(close) if after.as_bytes()[close] == b'>' && close > 0 => {
                out.push_str(&angle(&after[..close]));
                rest = &after[close + 1..];
            }
            _ => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(&decode_entities(rest));
    out
}

/// "```code```" on one line, which Slack accepts, as a fenced block Lazy reads.
fn fence_line(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.len() > 6 && trimmed.starts_with("```") && trimmed.ends_with("```") {
        let inner = &trimmed[3..trimmed.len() - 3];
        if !inner.contains("```") {
            return format!("```\n{inner}\n```");
        }
    }
    line.to_owned()
}

/// One `<…>` item.
fn angle(inner: &str) -> String {
    let (target, label) = match inner.split_once('|') {
        Some((target, label)) => (target, Some(decode_entities(label))),
        None => (inner, None),
    };
    let label = label.filter(|label| !label.trim().is_empty());
    if let Some(special) = target.strip_prefix('!') {
        let name = special.split('^').next().unwrap_or_default();
        return match name {
            "channel" | "here" | "everyone" => escape_plain(&format!("@{name}")),
            // A date's fallback text, a user group's handle, and anything else Slack names.
            _ => label.map(|label| escape_plain(&label)).unwrap_or_default(),
        };
    }
    if let Some(user) = target.strip_prefix('@') {
        let name = label.unwrap_or_else(|| user.to_owned());
        return escape_plain(&format!("@{}", name.trim_start_matches('@')));
    }
    if let Some(channel) = target.strip_prefix('#') {
        let name = label.unwrap_or_else(|| channel.to_owned());
        return escape_plain(&format!("#{}", name.trim_start_matches('#')));
    }
    let url = decode_entities(target);
    if !lazy_domain::document::link_is_allowed(&url) {
        // Not Slack syntax at all ("a <b> c", "<none>"): the text as it was written.
        return format!("<{inner}>");
    }
    let shown = label.unwrap_or_else(|| url.trim_start_matches("mailto:").to_owned());
    link_source(&url, &shown)
}

/// Slack escapes exactly these three in text.
fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// A Block Kit text object (`mrkdwn` or `plain_text`) as Markdown.
fn text_object(value: &Value) -> Option<String> {
    let text = value["text"].as_str()?;
    if text.trim().is_empty() {
        return None;
    }
    Some(if value["type"] == "plain_text" {
        escape_plain(text)
    } else {
        mrkdwn(text)
    })
}

// ----- Blocks ------------------------------------------------------------------------------------

/// A `blocks` array as Markdown paragraphs, or `None` if it shows nothing.
fn blocks(value: &Value) -> Option<String> {
    let shown: Vec<String> = value
        .as_array()?
        .iter()
        .filter_map(block)
        .filter(|text| !text.trim().is_empty())
        .collect();
    (!shown.is_empty()).then(|| shown.join("\n\n"))
}

fn block(block: &Value) -> Option<String> {
    match block["type"].as_str()? {
        "header" => {
            let text = block["text"]["text"].as_str()?.trim();
            (!text.is_empty()).then(|| format!("*{}*", escape_plain(text)))
        }
        "section" => {
            let mut lines = Vec::new();
            lines.extend(text_object(&block["text"]));
            for field in block["fields"].as_array().into_iter().flatten() {
                lines.extend(text_object(field));
            }
            lines.extend(element(&block["accessory"]));
            (!lines.is_empty()).then(|| lines.join("\n"))
        }
        "context" => {
            let items: Vec<String> = block["elements"]
                .as_array()?
                .iter()
                .filter_map(|item| match item["type"].as_str() {
                    Some("image") => None,
                    _ => text_object(item),
                })
                .collect();
            (!items.is_empty()).then(|| items.join(" · "))
        }
        "image" => {
            let url = block["image_url"].as_str()?;
            let title = block["title"]["text"]
                .as_str()
                .or_else(|| block["alt_text"].as_str())
                .filter(|text| !text.trim().is_empty())
                .unwrap_or("Image");
            Some(link_source(url, title))
        }
        "actions" => {
            let items: Vec<String> = block["elements"]
                .as_array()?
                .iter()
                .filter_map(element)
                .collect();
            (!items.is_empty()).then(|| items.join(" · "))
        }
        "rich_text" => {
            let parts: Vec<String> = block["elements"]
                .as_array()?
                .iter()
                .filter_map(rich_element)
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n\n"))
        }
        _ => None,
    }
}

/// An interactive element, if it is a button with a link (a webhook cannot answer clicks).
fn element(value: &Value) -> Option<String> {
    if value["type"] != "button" {
        return None;
    }
    let url = value["url"].as_str()?;
    let label = value["text"]["text"].as_str().unwrap_or(url);
    Some(link_source(url, label))
}

/// A rich text element: a section, list, quote, or preformatted run.
fn rich_element(value: &Value) -> Option<String> {
    let items = value["elements"].as_array()?;
    match value["type"].as_str()? {
        "rich_text_section" => Some(rich_inlines(items)),
        "rich_text_quote" => Some(
            rich_inlines(items)
                .split('\n')
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        "rich_text_preformatted" => {
            let code: String = items
                .iter()
                .filter_map(|item| item["text"].as_str().or_else(|| item["url"].as_str()))
                .collect();
            Some(format!("```\n{}\n```", code.trim_end_matches('\n')))
        }
        "rich_text_list" => {
            let ordered = value["style"] == "ordered";
            let lines: Vec<String> = items
                .iter()
                .enumerate()
                .filter_map(|(n, item)| {
                    let text = rich_inlines(item["elements"].as_array()?);
                    Some(if ordered {
                        format!("{}. {text}", n + 1)
                    } else {
                        format!("- {text}")
                    })
                })
                .collect();
            Some(lines.join("\n"))
        }
        _ => None,
    }
}

/// The inline items of a rich text section.
fn rich_inlines(items: &[Value]) -> String {
    let mut out = String::new();
    for item in items {
        let piece = match item["type"].as_str().unwrap_or_default() {
            "text" => {
                let text = item["text"].as_str().unwrap_or_default();
                let style = &item["style"];
                if style["code"] == true {
                    format!("`{}`", text.replace('`', "'"))
                } else {
                    // Line breaks stay outside the markers, which close within a line.
                    text.split('\n')
                        .map(|line| styled(&escape_plain(line), style))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            "link" => {
                let url = item["url"].as_str().unwrap_or_default();
                let text = item["text"].as_str().unwrap_or(url);
                styled(&link_source(url, text), &item["style"])
            }
            "user" => escape_plain(&format!(
                "@{}",
                item["user_id"].as_str().unwrap_or("someone")
            )),
            "usergroup" => escape_plain("@group"),
            "channel" => escape_plain(&format!(
                "#{}",
                item["channel_id"].as_str().unwrap_or_default()
            )),
            "broadcast" => escape_plain(&format!("@{}", item["range"].as_str().unwrap_or("here"))),
            "emoji" => item["unicode"]
                .as_str()
                .and_then(unicode_from_hex)
                .unwrap_or_else(|| format!(":{}:", item["name"].as_str().unwrap_or_default())),
            _ => String::new(),
        };
        out.push_str(&piece);
    }
    out
}

/// Wraps Markdown in the emphasis a rich text style asks for (whitespace stays outside, since a
/// marker must touch its word).
fn styled(text: &str, style: &Value) -> String {
    let core = text.trim();
    if core.is_empty() {
        return text.to_owned();
    }
    let (lead, trail) = (
        &text[..text.len() - text.trim_start().len()],
        &text[text.trim_end().len()..],
    );
    let mut core = core.to_owned();
    for (key, mark) in [("strike", '~'), ("italic", '_'), ("bold", '*')] {
        if style[key] == true {
            core = format!("{mark}{core}{mark}");
        }
    }
    format!("{lead}{core}{trail}")
}

/// Slack's emoji `unicode` field: code points in hex joined by `-`.
fn unicode_from_hex(hex: &str) -> Option<String> {
    hex.split('-')
        .map(|part| u32::from_str_radix(part, 16).ok().and_then(char::from_u32))
        .collect()
}

// ----- Attachments -------------------------------------------------------------------------------

/// A legacy attachment: its pretext, then the rest as a quote.
fn attachment_markdown(value: &Value) -> Option<String> {
    let mut shown = Vec::new();
    if let Some(pretext) = nonempty(&value["pretext"]) {
        shown.push(mrkdwn(pretext));
    }
    let mut quoted: Vec<String> = Vec::new();
    if let Some(author) = nonempty(&value["author_name"]) {
        quoted.push(match value["author_link"].as_str() {
            Some(link) => link_source(link, author),
            None => escape_plain(author),
        });
    }
    if let Some(title) = nonempty(&value["title"]) {
        let title = match value["title_link"].as_str() {
            Some(link) => link_source(link, title),
            None => escape_plain(title),
        };
        quoted.push(format!("*{title}*"));
    }
    if let Some(text) = nonempty(&value["text"]) {
        quoted.push(mrkdwn(text));
    }
    for field in value["fields"].as_array().into_iter().flatten() {
        let title = nonempty(&field["title"]);
        let body = nonempty(&field["value"]).map(mrkdwn);
        quoted.push(match (title, body) {
            (Some(title), Some(body)) => format!("*{}:* {body}", escape_plain(title)),
            (Some(title), None) => format!("*{}*", escape_plain(title)),
            (None, Some(body)) => body,
            (None, None) => continue,
        });
    }
    if let Some(blocks) = blocks(&value["blocks"]) {
        quoted.push(blocks);
    }
    if let Some(url) = nonempty(&value["image_url"]) {
        quoted.push(link_source(url, "Image"));
    }
    if let Some(footer) = nonempty(&value["footer"]) {
        quoted.push(format!("_{}_", mrkdwn(footer).trim()));
    }
    if quoted.is_empty()
        && shown.is_empty()
        && let Some(fallback) = nonempty(&value["fallback"])
    {
        quoted.push(mrkdwn(fallback));
    }
    if !quoted.is_empty() {
        shown.push(quote(&quoted.join("\n")));
    }
    (!shown.is_empty()).then(|| shown.join("\n\n"))
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.trim().is_empty())
}

/// Markdown as a quote: every line prefixed, blank lines dropped (a quote's paragraphs). Code
/// fences inside lose their fences, which a quote cannot hold, and keep their lines.
fn quote(markdown: &str) -> String {
    markdown
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with("```"))
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests;
