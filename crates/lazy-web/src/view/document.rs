//! A message document as HTML.
//!
//! Every piece of text is escaped. A link becomes an anchor only for `http`, `https`, and `mailto`
//! (opened in a new tab, with no referrer and no access to this page); a `lazy:` permalink becomes
//! a button that opens the message here. A custom emoji is its picture, from the server, while it
//! exists, and its `:name:` otherwise.

use lazy_client_core::ClientCore;
use lazy_domain::mention::MentionTarget;
use lazy_domain::{Block, Inline, Marks, MessageDocument};
use lazy_protocol::api::paths;

use super::html::{esc, push};

/// What a document's mentions and emoji are read against.
pub(crate) struct Context<'a> {
    pub core: &'a ClientCore,
}

pub(crate) fn document(ctx: &Context<'_>, doc: &MessageDocument) -> String {
    let mut out = String::new();
    for block in doc.blocks() {
        match block {
            Block::Paragraph { inlines } => {
                out.push_str("<p>");
                inlines_html(ctx, inlines, &mut out);
                out.push_str("</p>");
            }
            Block::CodeBlock { language, text } => {
                match language {
                    Some(language) => push!(out, "<pre data-language=\"{}\">", esc(language)),
                    None => out.push_str("<pre>"),
                }
                push!(out, "<code>{}</code></pre>", esc(text));
            }
            Block::Quote { paragraphs } => {
                out.push_str("<blockquote>");
                for paragraph in paragraphs {
                    out.push_str("<p>");
                    inlines_html(ctx, paragraph, &mut out);
                    out.push_str("</p>");
                }
                out.push_str("</blockquote>");
            }
            Block::BulletList { items } => {
                out.push_str("<ul>");
                for item in items {
                    out.push_str("<li>");
                    inlines_html(ctx, item, &mut out);
                    out.push_str("</li>");
                }
                out.push_str("</ul>");
            }
            Block::OrderedList { start, items } => {
                if *start == 1 {
                    out.push_str("<ol>");
                } else {
                    push!(out, "<ol start=\"{start}\">");
                }
                for item in items {
                    out.push_str("<li>");
                    inlines_html(ctx, item, &mut out);
                    out.push_str("</li>");
                }
                out.push_str("</ol>");
            }
        }
    }
    out
}

fn open_marks(marks: Marks, out: &mut String) {
    if marks.bold {
        out.push_str("<strong>");
    }
    if marks.italic {
        out.push_str("<em>");
    }
    if marks.strike {
        out.push_str("<s>");
    }
    if marks.code {
        out.push_str("<code>");
    }
}

fn close_marks(marks: Marks, out: &mut String) {
    if marks.code {
        out.push_str("</code>");
    }
    if marks.strike {
        out.push_str("</s>");
    }
    if marks.italic {
        out.push_str("</em>");
    }
    if marks.bold {
        out.push_str("</strong>");
    }
}

/// Whether a link may be an anchor in this page.
fn safe_web_link(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
}

fn inlines_html(ctx: &Context<'_>, inlines: &[Inline], out: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text { text, marks } => {
                open_marks(*marks, out);
                out.push_str(&esc(text));
                close_marks(*marks, out);
            }
            Inline::LineBreak => out.push_str("<br>"),
            Inline::Link { url, text, marks } => {
                open_marks(*marks, out);
                if safe_web_link(url) {
                    push!(
                        out,
                        "<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer nofollow\">{}</a>",
                        esc(url),
                        esc(text)
                    );
                } else if lazy_domain::Permalink::parse(url).is_some() {
                    push!(
                        out,
                        "<button type=\"button\" class=\"link\" data-action=\"permalink\" data-url=\"{}\">{}</button>",
                        esc(url),
                        esc(text)
                    );
                } else {
                    out.push_str(&esc(text));
                }
                close_marks(*marks, out);
            }
            Inline::Emoji { id, name, marks } => {
                open_marks(*marks, out);
                let live = ctx
                    .core
                    .custom_emoji_by_id(*id)
                    .is_some_and(|emoji| !emoji.deleted);
                if live {
                    let label = ctx.core.custom_emoji_label(*id).unwrap_or(name);
                    push!(
                        out,
                        "<img class=\"emoji\" src=\"{}\" alt=\":{label}:\" title=\":{label}:\">",
                        esc(&paths::emoji(*id)),
                        label = esc(label)
                    );
                } else {
                    push!(out, ":{}:", esc(name));
                }
                close_marks(*marks, out);
            }
            Inline::Mention {
                target,
                handle,
                marks,
            } => {
                open_marks(*marks, out);
                let me = ctx.core.me().map(|user| user.id);
                let mine = match target {
                    MentionTarget::User { id } => Some(*id) == me,
                    MentionTarget::Channel => true,
                    MentionTarget::Group { .. } => false,
                };
                let shown = match target {
                    MentionTarget::User { id } => ctx
                        .core
                        .member(*id)
                        .map(|user| user.display_name.as_str().to_owned())
                        .unwrap_or_else(|| handle.clone()),
                    _ => handle.clone(),
                };
                push!(
                    out,
                    "<span class=\"mention{}\">@{}</span>",
                    if mine { " mine" } else { "" },
                    esc(&shown)
                );
                close_marks(*marks, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(markdown: &str) -> String {
        let core = ClientCore::new();
        let doc = MessageDocument::from_markdown(markdown).unwrap();
        document(&Context { core: &core }, &doc)
    }

    #[test]
    fn formatting_becomes_markup_and_text_stays_text() {
        assert_eq!(
            render("**bold** _it_ ~~gone~~ `x<y`"),
            "<p><strong>bold</strong> <em>it</em> <s>gone</s> <code>x&lt;y</code></p>"
        );
        assert_eq!(
            render("<script>alert(1)</script>"),
            "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>"
        );
        assert_eq!(
            render("```rust\nfn main() {}\n```"),
            "<pre data-language=\"rust\"><code>fn main() {}</code></pre>"
        );
        assert_eq!(render("> quoted"), "<blockquote><p>quoted</p></blockquote>");
        assert_eq!(render("- a\n- b"), "<ul><li>a</li><li>b</li></ul>");
        assert_eq!(
            render("3. c\n4. d"),
            "<ol start=\"3\"><li>c</li><li>d</li></ol>"
        );
    }

    #[test]
    fn only_web_links_are_anchors() {
        let html = render("see https://example.com/a?b=1&c=2");
        assert!(
            html.contains(
                "<a href=\"https://example.com/a?b=1&amp;c=2\" target=\"_blank\" rel=\"noopener noreferrer nofollow\">"
            ),
            "{html}"
        );
        let core = ClientCore::new();
        let doc = MessageDocument::new(vec![Block::Paragraph {
            inlines: vec![Inline::link("mailto:a@example.com", "mail")],
        }])
        .unwrap();
        assert!(document(&Context { core: &core }, &doc).contains("href=\"mailto:a@example.com\""));
        assert!(!safe_web_link("javascript:alert(1)"));
        assert!(!safe_web_link("data:text/html,x"));
    }
}
