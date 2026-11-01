//! Writing a provider's text into a Lazy message (GitHub, D32; webhook connectors, D34): names
//! and titles escaped so they show as written, and descriptions or comments reduced to a short
//! plain-text quote.

/// How much of a description or comment is quoted.
pub const EXCERPT_CHARS: usize = 280;

/// Markdown-escapes text from a provider (titles, names, comments), so it shows as written and can
/// neither format nor link.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A description or comment written in GitHub's Markdown, as plain lines: what a reader sees on
/// GitHub, without the syntax (which would otherwise show as typed, since it is escaped). Code
/// fences, HTML comments (pull request templates), and empty lines are dropped; list items get a
/// bullet; headings, quote markers, emphasis, code ticks, and link syntax are removed.
pub fn plain_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_comment = false;
    for raw in text.lines() {
        let mut line = raw.trim().to_owned();
        // HTML comments, possibly across lines.
        loop {
            if in_comment {
                match line.find("-->") {
                    Some(end) => {
                        line.replace_range(..end + 3, "");
                        in_comment = false;
                    }
                    None => {
                        line.clear();
                        break;
                    }
                }
            }
            match line.find("<!--") {
                Some(start) => {
                    let rest = line[start..].to_owned();
                    line.truncate(start);
                    match rest.find("-->") {
                        Some(end) => line.push_str(&rest[end + 3..]),
                        None => {
                            in_comment = true;
                            break;
                        }
                    }
                }
                None => break,
            }
        }
        let mut line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            continue;
        }
        while let Some(rest) = line.strip_prefix('>') {
            line = rest.trim_start();
        }
        let heading = line.trim_start_matches('#');
        if heading.len() < line.len() && (heading.is_empty() || heading.starts_with(' ')) {
            line = heading.trim_start();
        }
        let mut bullet = false;
        for marker in ["- ", "* ", "+ "] {
            if let Some(rest) = line.strip_prefix(marker) {
                line = ["[ ] ", "[x] ", "[X] "]
                    .iter()
                    .find_map(|check| rest.strip_prefix(check))
                    .unwrap_or(rest);
                bullet = true;
                break;
            }
        }
        let text = inline_plain(line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        out.push(if bullet {
            format!("• {text}")
        } else {
            text.to_owned()
        });
    }
    out
}

/// Inline Markdown as its text: `[text](url)` and `![alt](url)` become their text, and code
/// ticks and emphasis markers (`**`, `__`, `~~`) are dropped. Single `*` and `_` stay, since
/// they are as often part of a word or an expression.
pub fn inline_plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        let bang = rest.starts_with("![");
        if bang || rest.starts_with('[') {
            let open = if bang { 2 } else { 1 };
            if let Some(close) = rest[open..].find("](")
                && let Some(end) = rest[open + close + 2..].find(')')
            {
                out.push_str(&rest[open..open + close]);
                rest = &rest[open + close + 2 + end + 1..];
                continue;
            }
        }
        if let Some(after) = ["**", "__", "~~"].iter().find_map(|m| rest.strip_prefix(m)) {
            rest = after;
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        if c != '`' {
            out.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// The first `EXCERPT_CHARS` characters of a description or comment, as plain text (see
/// [`plain_lines`]) in a quote of at most three lines, or nothing if it is empty.
pub fn excerpt(text: &str) -> Option<String> {
    let plain = plain_lines(text).join("\n");
    let text = plain.trim();
    if text.is_empty() {
        return None;
    }
    let mut short: String = text.chars().take(EXCERPT_CHARS).collect();
    let cut = short.len() < text.len();
    let lines: Vec<&str> = short
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect();
    let more = cut || text.lines().filter(|l| !l.trim().is_empty()).count() > lines.len();
    short = lines
        .iter()
        .map(|line| format!("> {}", escape(line)))
        .collect::<Vec<_>>()
        .join("\n");
    if more {
        short.push_str(" …");
    }
    Some(short)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazy_domain::MessageDocument;

    #[test]
    fn text_from_github_cannot_format_link_or_mention() {
        assert_eq!(
            escape("fix *all* the [things](x) @bob"),
            "fix \\*all\\* the \\[things\\]\\(x\\) \\@bob"
        );
        let doc =
            MessageDocument::from_markdown(&escape("**bold** `code` [a](https://x.y)")).unwrap();
        assert_eq!(doc.plain_text(), "**bold** `code` [a](https://x.y)");
    }

    #[test]
    fn excerpts_read_as_github_shows_them() {
        let body = "<!-- Describe the change -->\n## Summary\nUses `lazy-notify.yml` and \
                    **bold** [docs](https://x.y).\n\n- Deletes `a.yml`.\n* [ ] check\n```sh\nrm\n```";
        assert_eq!(
            plain_lines(body),
            [
                "Summary",
                "Uses lazy-notify.yml and bold docs.",
                "• Deletes a.yml.",
                "• check",
                "rm",
            ]
        );
        assert_eq!(plain_lines("<!--\nmulti\nline -->\nafter"), ["after"]);
        assert_eq!(plain_lines("> quoted\n#hashtag"), ["quoted", "#hashtag"]);
        assert_eq!(
            inline_plain("snake_case and 2*3 ![alt](i.png)"),
            "snake_case and 2*3 alt"
        );
        // What is left is still escaped: nothing in it formats, links, or mentions.
        let doc = MessageDocument::from_markdown(&excerpt("@bob *hi* <b>").unwrap()).unwrap();
        assert_eq!(doc.plain_text(), "> @bob *hi* <b>");
    }

    #[test]
    fn excerpts_are_short_quotes() {
        assert_eq!(excerpt("  "), None);
        assert_eq!(excerpt("one line").unwrap(), "> one line");
        let many = excerpt("a\n\nb\nc\nd").unwrap();
        assert_eq!(many, "> a\n> b\n> c …");
        let long = excerpt(&"x".repeat(1000)).unwrap();
        assert!(long.ends_with(" …") && long.chars().count() < 300);
    }
}
