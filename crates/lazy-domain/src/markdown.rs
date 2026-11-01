//! The composer's text syntax, and its inverse.
//!
//! People type formatting the way they do in Slack, and [`MessageDocument::from_markdown`] turns
//! it into a structured document. [`MessageDocument::to_markdown`] goes the other way, so editing
//! a message shows its source, and re-parsing that source gives back the same document.
//!
//! # Syntax
//!
//! Inline, within one line:
//!
//! * `*bold*` or `**bold**`, `_italic_` or `__italic__`, `~strike~` or `~~strike~~`
//! * `` `code` `` (use a longer run of backticks to include a backtick)
//! * `[text](https://example.com)` and bare `http://`, `https://`, and `lazy://` addresses
//! * `\*` and the other backslash-escapes of ASCII punctuation produce the literal character
//!
//! A marker opens only at the start of a word and closes only at the end of one, so `snake_case`,
//! `2*3*4`, and `a*b` stay plain. Anything that does not form a pair is plain text.
//!
//! Blocks, one per run of lines:
//!
//! * a line starting with `> ` is a quote
//! * lines starting with `- ` or `* ` are a bulleted list; `1. ` or `1) ` a numbered one
//!   (indent a following line two spaces to continue an item)
//! * a fence of three or more backticks, optionally followed by a language, starts a code block
//!   that ends at a fence of at least the same length or at the end of the message
//! * everything else is a paragraph; a blank line ends one, a single newline is a line break
//!
//! Parsing never fails for a reason of syntax: input that is not valid markup is plain text.
//! The only errors are the document limits (empty, too long, too many blocks).

use crate::document::{
    Block, DocumentError, Inline, MAX_URL_BYTES, Marks, MessageDocument, language_is_valid,
    link_is_allowed, sanitize,
};
use crate::mention::{
    MAX_HANDLE_LEN, MapDirectory, MentionDirectory, NoMentions, handle_is_valid, is_handle_char,
};

impl MessageDocument {
    /// Parses composer text. See the [module documentation](self) for the syntax. `@names` stay
    /// plain text; use [`from_markdown_with`](Self::from_markdown_with) to turn them into
    /// mentions.
    pub fn from_markdown(text: &str) -> Result<Self, DocumentError> {
        Self::from_markdown_with(text, &NoMentions)
    }

    /// Parses composer text, turning `@name` into a mention wherever `directory` resolves the
    /// name. A mention opens at the start of a word, like a formatting marker, and takes the
    /// longest name (letters, digits, `.`, `_`, `-`, ending in a letter or digit) the directory
    /// knows, so `@alice.` ends a sentence and `@alice_` ends an italic span. `\@alice` is the
    /// literal text.
    pub fn from_markdown_with(
        text: &str,
        directory: &dyn MentionDirectory,
    ) -> Result<Self, DocumentError> {
        Self::new(parse_blocks(&sanitize(text), directory))
    }

    /// Writes the document as composer text that [`from_markdown`](Self::from_markdown) parses
    /// back to the same document (for any document that `from_markdown` produced).
    ///
    /// A very rare arrangement of adjacent formatting and punctuation has no source that parses
    /// back exactly (about one in 40 million markup-dense random inputs). Its words are still
    /// written unchanged; only its formatting is lost. A mention is written as `@handle`; reading
    /// it back as a mention needs a directory that resolves the handle.
    pub fn to_markdown(&self) -> String {
        self.blocks()
            .iter()
            .map(block_to_markdown)
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

// ---------------------------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------------------------

/// Backtick count and language if `line` opens a code fence.
fn fence_open(line: &str) -> Option<(usize, Option<String>)> {
    let rest = line.trim_start();
    let ticks = rest.chars().take_while(|c| *c == '`').count();
    if ticks < 3 {
        return None;
    }
    let after = &rest[ticks..];
    // A run of backticks later on the same line is inline code, not a fence.
    if after.contains("```") {
        return None;
    }
    let language = after.trim();
    Some((
        ticks,
        language_is_valid(language).then(|| language.to_owned()),
    ))
}

fn is_fence_close(line: &str, ticks: usize) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= ticks && trimmed.chars().all(|c| c == '`')
}

fn quote_rest(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

fn leading_spaces(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

fn bullet_item(line: &str) -> Option<&str> {
    if leading_spaces(line) > 3 {
        return None;
    }
    let rest = line.trim_start().strip_prefix(['-', '*'])?;
    let mut chars = rest.chars();
    chars.next().filter(|c| c.is_whitespace())?;
    let item = rest.trim();
    (!item.is_empty()).then_some(item)
}

fn ordered_item(line: &str) -> Option<(u32, &str)> {
    if leading_spaces(line) > 3 {
        return None;
    }
    let rest = line.trim_start();
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if !(1..=9).contains(&digits) {
        return None;
    }
    let (number, after) = rest.split_at(digits);
    let after = after.strip_prefix(['.', ')'])?;
    after.chars().next().filter(|c| c.is_whitespace())?;
    let item = after.trim();
    (!item.is_empty()).then(|| (number.parse().unwrap_or(1), item))
}

fn starts_block(line: &str) -> bool {
    fence_open(line).is_some()
        || quote_rest(line).is_some()
        || bullet_item(line).is_some()
        || ordered_item(line).is_some()
}

fn is_continuation(line: &str) -> bool {
    !line.trim().is_empty()
        && (line.starts_with("  ") || line.starts_with('\t'))
        && !starts_block(line)
}

/// Inline content from lines: each trimmed, joined by line breaks.
fn lines_to_inlines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    directory: &dyn MentionDirectory,
) -> Vec<Inline> {
    let joined = lines
        .into_iter()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n");
    parse_inlines(&joined, directory)
}

fn parse_blocks(text: &str, directory: &dyn MentionDirectory) -> Vec<Block> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }

        if let Some((ticks, language)) = fence_open(line) {
            i += 1;
            let mut body = Vec::new();
            while i < lines.len() && !is_fence_close(lines[i], ticks) {
                body.push(lines[i]);
                i += 1;
            }
            i += 1; // the closing fence, or one past the end
            while body.first().is_some_and(|l| l.trim().is_empty()) {
                body.remove(0);
            }
            while body.last().is_some_and(|l| l.trim().is_empty()) {
                body.pop();
            }
            if !body.is_empty() {
                blocks.push(Block::CodeBlock {
                    language,
                    text: body.join("\n"),
                });
            }
            continue;
        }

        if quote_rest(line).is_some() {
            let mut paragraphs: Vec<Vec<Inline>> = Vec::new();
            let mut current: Vec<&str> = Vec::new();
            while i < lines.len() {
                let Some(rest) = quote_rest(lines[i]) else {
                    break;
                };
                if rest.trim().is_empty() {
                    if !current.is_empty() {
                        paragraphs.push(lines_to_inlines(current.drain(..), directory));
                    }
                } else {
                    current.push(rest);
                }
                i += 1;
            }
            if !current.is_empty() {
                paragraphs.push(lines_to_inlines(current.drain(..), directory));
            }
            paragraphs.retain(|p| !p.is_empty());
            if !paragraphs.is_empty() {
                blocks.push(Block::Quote { paragraphs });
            }
            continue;
        }

        if bullet_item(line).is_some() || ordered_item(line).is_some() {
            let ordered = bullet_item(line).is_none();
            let start = ordered_item(line).map_or(1, |(n, _)| n);
            let mut items: Vec<Vec<&str>> = Vec::new();
            while i < lines.len() {
                let current = lines[i];
                let head = if ordered {
                    ordered_item(current).map(|(_, text)| text)
                } else {
                    bullet_item(current)
                };
                if let Some(text) = head {
                    items.push(vec![text]);
                } else if is_continuation(current) && !items.is_empty() {
                    items.last_mut().expect("checked non-empty").push(current);
                } else {
                    break;
                }
                i += 1;
            }
            let items: Vec<Vec<Inline>> = items
                .into_iter()
                .map(|lines| lines_to_inlines(lines, directory))
                .filter(|item| !item.is_empty())
                .collect();
            if !items.is_empty() {
                blocks.push(if ordered {
                    Block::OrderedList { start, items }
                } else {
                    Block::BulletList { items }
                });
            }
            continue;
        }

        let mut paragraph = vec![line];
        i += 1;
        while i < lines.len() && !lines[i].trim().is_empty() && !starts_block(lines[i]) {
            paragraph.push(lines[i]);
            i += 1;
        }
        let inlines = lines_to_inlines(paragraph, directory);
        if !inlines.is_empty() {
            blocks.push(Block::Paragraph { inlines });
        }
    }
    blocks
}

// ---------------------------------------------------------------------------------------------
// Inlines
// ---------------------------------------------------------------------------------------------

fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

fn run_len(chars: &[char], at: usize) -> usize {
    let c = chars[at];
    chars[at..].iter().take_while(|x| **x == c).count()
}

/// Accumulates inline items, merging adjacent text with equal marks.
struct Out {
    items: Vec<Inline>,
    text: String,
}

impl Out {
    fn flush(&mut self, marks: Marks) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        self.push(Inline::Text { text, marks });
    }

    fn push(&mut self, item: Inline) {
        if let (
            Some(Inline::Text {
                text: last,
                marks: last_marks,
            }),
            Inline::Text { text, marks },
        ) = (self.items.last_mut(), &item)
            && last_marks == marks
        {
            last.push_str(text);
            return;
        }
        self.items.push(item);
    }
}

/// Positions already known to have no partner, so that unmatched markers cost one scan per line
/// instead of one per marker.
#[derive(Default)]
struct Memo {
    no_closer: [bool; 6],
    no_code_run: Vec<usize>,
    no_bracket: bool,
    /// The first `]` found by an earlier scan, valid for any later `[` before it.
    bracket_at: Option<usize>,
}

impl Memo {
    fn reset(&mut self) {
        *self = Memo::default();
    }
}

fn parse_inlines(text: &str, directory: &dyn MentionDirectory) -> Vec<Inline> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Out {
        items: Vec::new(),
        text: String::new(),
    };
    parse_range(&chars, Marks::NONE, directory, &mut out);
    out.flush(Marks::NONE);
    out.items
}

fn delimiter_kind(c: char) -> Option<usize> {
    match c {
        '*' => Some(0),
        '_' => Some(1),
        '~' => Some(2),
        _ => None,
    }
}

fn apply(marks: Marks, kind: usize) -> Option<Marks> {
    let mut next = marks;
    let slot = match kind {
        0 => &mut next.bold,
        1 => &mut next.italic,
        _ => &mut next.strike,
    };
    if *slot {
        return None;
    }
    *slot = true;
    Some(next)
}

/// If a code span opens at `at`, its content range and the index after its closing run.
fn code_span(chars: &[char], at: usize, memo: &mut Memo) -> Option<(usize, usize, usize)> {
    let n = run_len(chars, at);
    if memo.no_code_run.contains(&n) {
        return None;
    }
    let mut j = at + n;
    while j < chars.len() && chars[j] != '\n' {
        if chars[j] == '`' {
            let m = run_len(chars, j);
            if m == n {
                return (j > at + n).then_some((at + n, j, j + m));
            }
            j += m;
        } else {
            j += 1;
        }
    }
    memo.no_code_run.push(n);
    None
}

/// The index of the closing delimiter run for an opener of `len` copies of `c` that ends at
/// `from`, or `None`.
fn find_closer(chars: &[char], from: usize, c: char, len: usize, memo: &mut Memo) -> Option<usize> {
    let memo_index = delimiter_kind(c)? * 2 + (len - 1);
    if memo.no_closer[memo_index] {
        return None;
    }
    // Skipped tokens are found exactly as the parser itself finds them, with one memo for the
    // whole scan so that unmatched brackets and backticks cost one pass, not one per occurrence.
    let mut scan = Memo::default();
    let mut j = from;
    while j < chars.len() && chars[j] != '\n' {
        if let Some(after) = skip_atom(chars, j, &mut scan) {
            j = after;
            continue;
        }
        match chars[j] {
            x if x == c => {
                let run = run_len(chars, j);
                let end = j + run;
                if run == len
                    && j > from
                    && !chars[j - 1].is_whitespace()
                    && (end >= chars.len() || !is_word(chars[end]))
                {
                    return Some(j);
                }
                j = end;
            }
            _ => j += 1,
        }
    }
    memo.no_closer[memo_index] = true;
    None
}

/// If a token that cannot contain a closing marker starts at `j` (an escaped character, a code
/// span or a run of backticks, a link, a bare address), the index after it. The scan for a closer
/// uses this to step over exactly what the parser would consume, so the two always agree about
/// where a marker is.
fn skip_atom(chars: &[char], j: usize, memo: &mut Memo) -> Option<usize> {
    match chars[j] {
        '\\' if chars.get(j + 1).is_some_and(char::is_ascii_punctuation) => Some(j + 2),
        '`' => Some(match code_span(chars, j, memo) {
            Some((_, _, after)) => after,
            None => j + run_len(chars, j),
        }),
        '[' => explicit_link(chars, j, memo).map(|(_, after)| after),
        'h' | 'l' if j == 0 || !is_word(chars[j - 1]) => {
            bare_link(chars, j).map(|(_, after)| after)
        }
        _ => None,
    }
}

fn unescape(chars: &[char]) -> String {
    let mut text = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && chars.get(i + 1).is_some_and(char::is_ascii_punctuation) {
            i += 1;
        }
        text.push(chars[i]);
        i += 1;
    }
    text
}

/// A `[text](url)` link starting at `at`: the link and the index after it.
fn explicit_link(chars: &[char], at: usize, memo: &mut Memo) -> Option<(Inline, usize)> {
    if memo.no_bracket {
        return None;
    }
    let close = match memo.bracket_at {
        Some(cached) if cached > at => cached,
        _ => {
            let mut j = at + 1;
            loop {
                match chars.get(j) {
                    None | Some('\n') => {
                        memo.no_bracket = true;
                        return None;
                    }
                    Some('\\') => j += 2,
                    Some(']') => break j,
                    Some(_) => j += 1,
                }
            }
        }
    };
    memo.bracket_at = Some(close);
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut j = close + 1;
    let end = loop {
        match chars.get(j)? {
            '\n' => return None,
            // An escaped character is part of the address and never closes or nests it.
            '\\' if chars.get(j + 1).is_some_and(char::is_ascii_punctuation) => {
                j += 2;
                continue;
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break j;
                }
            }
            _ => {}
        }
        j += 1;
    };
    let url = unescape(&chars[close + 2..end]).trim().to_owned();
    let text = unescape(&chars[at + 1..close]);
    (!text.trim().is_empty() && link_is_allowed(&url)).then(|| (Inline::link(url, text), end + 1))
}

/// A bare address starting at `at`: the link and the index after it.
fn bare_link(chars: &[char], at: usize) -> Option<(Inline, usize)> {
    let starts = |prefix: &str| {
        prefix
            .chars()
            .enumerate()
            .all(|(n, p)| chars.get(at + n) == Some(&p))
    };
    let scheme = ["https://", "http://", "lazy://"]
        .into_iter()
        .find(|p| starts(p))?;
    let mut end = at;
    // A backslash ends the address: it is how the serializer escapes punctuation that follows a
    // link, and a real address does not contain one.
    while end < chars.len()
        && !chars[end].is_whitespace()
        && !matches!(chars[end], '<' | '>' | '\\')
    {
        end += 1;
    }
    // Trailing punctuation and unbalanced closers belong to the sentence, not the address.
    while end > at + scheme.len() {
        let last = chars[end - 1];
        let unbalanced = |open: char| {
            let inside = &chars[at..end - 1];
            inside.iter().filter(|c| **c == open).count()
                < inside.iter().filter(|c| **c == last).count() + 1
        };
        let trim = matches!(
            last,
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '*' | '_' | '~'
        ) || (last == ')' && unbalanced('('))
            || (last == ']' && unbalanced('['))
            || (last == '}' && unbalanced('{'));
        if trim {
            end -= 1;
        } else {
            break;
        }
    }
    let url: String = chars[at..end].iter().collect();
    (url.len() > scheme.len() && url.len() <= MAX_URL_BYTES && link_is_allowed(&url))
        .then(|| (Inline::link(url.clone(), url), end))
}

/// How many characters of name follow a `:` at `at` before the closing `:`, if the text there
/// reads `:name:` with a name of 2 to 32 letters, digits, and underscores.
fn emoji_name_length(chars: &[char], at: usize) -> Option<usize> {
    let run = chars[at + 1..]
        .iter()
        .take(33)
        .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
        .count();
    ((2..=32).contains(&run) && chars.get(at + 1 + run) == Some(&':')).then_some(run)
}

/// A custom emoji at `at` (a `:`): the emoji the directory knows by that name, with how many
/// characters it spans including both colons.
fn emoji_at(
    chars: &[char],
    at: usize,
    directory: &dyn MentionDirectory,
) -> Option<(crate::EmojiId, String, usize)> {
    let run = emoji_name_length(chars, at)?;
    let typed: String = chars[at + 1..=at + run].iter().collect();
    let (id, name) = directory.resolve_emoji(&typed)?;
    Some((id, name, run + 2))
}

/// A mention at `at` (a `@`): the longest typed name the directory resolves, with how many
/// characters it spans including the `@`.
fn mention_at(
    chars: &[char],
    at: usize,
    directory: &dyn MentionDirectory,
) -> Option<(Inline, usize)> {
    let run = chars[at + 1..]
        .iter()
        .take(MAX_HANDLE_LEN)
        .take_while(|c| is_handle_char(**c))
        .count();
    (1..=run).rev().find_map(|len| {
        let last = chars[at + len];
        if !last.is_ascii_alphanumeric() {
            return None;
        }
        let typed: String = chars[at + 1..=at + len].iter().collect();
        let (target, handle) = directory.resolve(&typed)?;
        handle_is_valid(&handle).then(|| {
            (
                Inline::Mention {
                    target,
                    handle,
                    marks: Marks::NONE,
                },
                len + 1,
            )
        })
    })
}

fn parse_range(chars: &[char], marks: Marks, directory: &dyn MentionDirectory, out: &mut Out) {
    let mut memo = Memo::default();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                out.flush(marks);
                out.push(Inline::LineBreak);
                memo.reset();
                i += 1;
            }
            '\\' if chars.get(i + 1).is_some_and(char::is_ascii_punctuation) => {
                out.text.push(chars[i + 1]);
                i += 2;
            }
            '`' => {
                if let Some((from, to, after)) = code_span(chars, i, &mut memo) {
                    out.flush(marks);
                    let text: String = chars[from..to].iter().collect();
                    out.push(Inline::Text {
                        text,
                        marks: Marks {
                            code: true,
                            ..marks
                        },
                    });
                    i = after;
                } else {
                    let n = run_len(chars, i);
                    out.text.extend(std::iter::repeat_n('`', n));
                    i += n;
                }
            }
            ':' if i == 0 || !is_word(chars[i - 1]) => {
                if let Some((id, name, length)) = emoji_at(chars, i, directory) {
                    out.flush(marks);
                    out.push(Inline::Emoji { id, name, marks });
                    i += length;
                } else {
                    out.text.push(':');
                    i += 1;
                }
            }
            '@' if i == 0 || !is_word(chars[i - 1]) => {
                if let Some((Inline::Mention { target, handle, .. }, length)) =
                    mention_at(chars, i, directory)
                {
                    out.flush(marks);
                    out.push(Inline::Mention {
                        target,
                        handle,
                        marks,
                    });
                    i += length;
                } else {
                    out.text.push('@');
                    i += 1;
                }
            }
            '[' => {
                if let Some((link, after)) = explicit_link(chars, i, &mut memo) {
                    out.flush(marks);
                    out.push(inside(link, marks));
                    i = after;
                } else {
                    out.text.push('[');
                    i += 1;
                }
            }
            '*' | '_' | '~' => {
                let run = run_len(chars, i);
                let kind = delimiter_kind(c).expect("a delimiter");
                let opens = run <= 2
                    && (i == 0 || !is_word(chars[i - 1]))
                    && chars.get(i + run).is_some_and(|next| !next.is_whitespace());
                let opened = opens
                    .then(|| apply(marks, kind))
                    .flatten()
                    .and_then(|inner| {
                        find_closer(chars, i + run, c, run, &mut memo).map(|close| (inner, close))
                    });
                if let Some((inner, close)) = opened {
                    out.flush(marks);
                    parse_range(&chars[i + run..close], inner, directory, out);
                    out.flush(inner);
                    i = close + run;
                } else {
                    out.text.extend(std::iter::repeat_n(c, run));
                    i += run;
                }
            }
            _ => {
                if (c == 'h' || c == 'l')
                    && (i == 0 || !is_word(chars[i - 1]))
                    && let Some((link, after)) = bare_link(chars, i)
                {
                    out.flush(marks);
                    out.push(inside(link, marks));
                    i = after;
                } else {
                    out.text.push(c);
                    i += 1;
                }
            }
        }
    }
    out.flush(marks);
}

// ---------------------------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------------------------

/// Backslash-escapes characters in plain text that would otherwise read as markup.
fn escape_text(text: &str, at_line_start: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        let intraword =
            i > 0 && is_word(chars[i - 1]) && chars.get(i + 1).is_some_and(|n| is_word(*n));
        let escape = match c {
            '\\' | '`' | '[' => true,
            // A mention opens at the start of a word, so an `@` that could be one is escaped. The
            // character before the first one belongs to another item, so it is assumed to be a
            // word start.
            '@' => i == 0 || !is_word(chars[i - 1]),
            // `:name:` can open a custom emoji (or, in the composer, a standard shortcode) at the
            // start of a word, so a literal one is escaped.
            ':' => (i == 0 || !is_word(chars[i - 1])) && emoji_name_length(&chars, i).is_some(),
            '*' | '_' | '~' => !intraword,
            // A line that would start a quote or a bullet.
            '>' | '-' => at_line_start && i == 0,
            // `1.` or `1)` at the start of a line would start a numbered list.
            '.' | ')' => {
                at_line_start && i > 0 && i <= 9 && chars[..i].iter().all(char::is_ascii_digit)
            }
            _ => false,
        };
        if escape {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn code_span_to_markdown(text: &str) -> String {
    let longest = {
        let mut best = 0;
        let mut run = 0;
        for c in text.chars() {
            run = if c == '`' { run + 1 } else { 0 };
            best = best.max(run);
        }
        best
    };
    let fence = "`".repeat(longest + 1);
    format!("{fence}{text}{fence}")
}

fn open_marks(marks: Marks) -> Vec<(char, usize)> {
    [(marks.bold, '*'), (marks.italic, '_'), (marks.strike, '~')]
        .into_iter()
        .enumerate()
        .filter_map(|(kind, (set, symbol))| set.then_some((symbol, kind)))
        .collect()
}

/// How spans are nested when the same marks appear on neighboring items. The document records
/// which marks each item has, not where one span ends and the next begins, and the syntax's
/// whitespace rules (nothing closes after a space or opens before one) mean different documents
/// need different nestings. Serialization tries each, in order, and keeps the first whose output
/// parses back to the same document.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Nesting {
    /// Keep every span open as long as its mark is wanted; new spans open inside.
    Persist,
    /// The span that lasts longest nests outermost, closing and reopening shorter ones.
    Longest,
    /// The span that lasts shortest nests outermost, so it can end before a longer one that began
    /// inside it.
    Shortest,
    /// Every change of marks closes all spans and opens the new ones afresh.
    Isolated,
}

/// One way of writing spans: a [`Nesting`], with new spans opened in either the usual order or the
/// reverse. Which one parses back correctly can depend on the neighbors: closing a span and
/// opening another of the same kind directly after it would merge the two markers into a longer
/// run, and the reverse order puts a different marker between them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Plan {
    nesting: Nesting,
    reversed: bool,
}

const PLANS: [Plan; 8] = {
    const fn plan(nesting: Nesting, reversed: bool) -> Plan {
        Plan { nesting, reversed }
    }
    [
        plan(Nesting::Persist, false),
        plan(Nesting::Longest, false),
        plan(Nesting::Shortest, false),
        plan(Nesting::Isolated, false),
        plan(Nesting::Persist, true),
        plan(Nesting::Longest, true),
        plan(Nesting::Shortest, true),
        plan(Nesting::Isolated, true),
    ]
};

/// The spans item `index` sits inside, outermost first, for the chosen nesting.
fn desired_spans(
    inlines: &[Inline],
    index: usize,
    open: &[(char, usize)],
    plan: Plan,
) -> Vec<(char, usize)> {
    let mut wanted = wanted_spans(&inlines[index]);
    if plan.reversed {
        wanted.reverse();
    }
    let already_open = |kind: usize| open.iter().position(|(_, k)| *k == kind);
    match plan.nesting {
        Nesting::Isolated
            if open.len() != wanted.len()
                || !open
                    .iter()
                    .all(|(_, kind)| wanted.iter().any(|(_, k)| k == kind)) =>
        {
            // The spans change, so all of them close and open afresh, in the order wanted rather
            // than the order they happened to be open in.
            wanted
        }
        Nesting::Persist | Nesting::Isolated => {
            // Spans that are already open keep their place; new ones follow in a fixed order.
            let mut kept: Vec<(char, usize)> = open
                .iter()
                .filter(|(_, kind)| wanted.iter().any(|(_, k)| k == kind))
                .copied()
                .collect();
            kept.extend(
                wanted
                    .iter()
                    .filter(|(_, kind)| already_open(*kind).is_none()),
            );
            kept
        }
        Nesting::Longest | Nesting::Shortest => {
            let mut ordered = wanted;
            let lasts = |kind: usize| {
                inlines[index..]
                    .iter()
                    .take_while(|item| wanted_spans(item).iter().any(|(_, k)| *k == kind))
                    .count()
            };
            let longest_first = plan.nesting == Nesting::Longest;
            // The sort is stable, so spans that tie keep the order they were wanted in.
            ordered.sort_by_key(|(_, kind)| {
                let run = lasts(*kind);
                (
                    if longest_first { usize::MAX - run } else { run },
                    already_open(*kind).unwrap_or(usize::MAX),
                )
            });
            ordered
        }
    }
}

/// The span markers to write between two items, given the spans open now and the spans the next
/// item should sit inside, outermost first: close back to the part of the nesting that still
/// matches, then open the rest. Also returns the resulting open spans.
fn span_transition(
    open: &[(char, usize)],
    desired: &[(char, usize)],
    plan: Plan,
) -> (String, String, Vec<(char, usize)>) {
    let same_spans = open.len() == desired.len()
        && open
            .iter()
            .all(|(_, k)| desired.iter().any(|(_, d)| d == k));
    let keep = if plan.nesting == Nesting::Isolated && !same_spans {
        0
    } else {
        open.iter()
            .zip(desired)
            .take_while(|(have, want)| have.1 == want.1)
            .count()
    };
    let mut now = open.to_vec();
    let mut closers = String::new();
    while now.len() > keep {
        let (symbol, _) = now.pop().expect("checked length");
        closers.push(symbol);
    }
    let mut openers = String::new();
    for &(symbol, kind) in &desired[keep..] {
        openers.push(symbol);
        now.push((symbol, kind));
    }
    (closers, openers, now)
}

/// The spans an item sits inside. A line break ends every span.
fn wanted_spans(inline: &Inline) -> Vec<(char, usize)> {
    match inline {
        Inline::Text { marks, .. }
        | Inline::Link { marks, .. }
        | Inline::Mention { marks, .. }
        | Inline::Emoji { marks, .. } => open_marks(*marks),
        Inline::LineBreak => Vec::new(),
    }
}

/// A link found while parsing inside spans carries those spans' formatting.
fn inside(link: Inline, marks: Marks) -> Inline {
    match link {
        Inline::Link { url, text, .. } => Inline::Link { url, text, marks },
        other => other,
    }
}

/// What a run of inline items says, whatever its formatting: the text, each link's address and
/// display text, and the line breaks.
fn words(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for item in inlines {
        match item {
            Inline::Text { text, .. } => out.push_str(text),
            Inline::Link { url, text, .. } => {
                out.push_str(&format!("\u{1}{url}\u{2}{text}\u{3}"));
            }
            Inline::LineBreak => out.push('\n'),
            Inline::Mention { handle, .. } => {
                out.push('@');
                out.push_str(handle);
            }
            Inline::Emoji { name, .. } => {
                out.push(':');
                out.push_str(name);
                out.push(':');
            }
        }
    }
    out
}

/// Markdown for a sequence of inline items that parses back to the same items, found by trying
/// each [`Plan`].
///
/// A few arrangements of adjacent marks and punctuation have no plan that survives (the soak test
/// found one in about 40 million markup-dense random inputs). Then the first plan that at least
/// keeps every word is used, and failing that the formatting is dropped: the text of a message is
/// never altered by being written out as source, only, in those rare cases, its formatting.
fn inlines_to_markdown(inlines: &[Inline]) -> String {
    // Written text is checked by reading it back, and a mention only reads back as a mention with
    // a directory that knows it: the mentions of these very items.
    let mut directory = MapDirectory::new();
    for item in inlines {
        match item {
            Inline::Mention { target, handle, .. } => {
                directory.insert(handle, *target);
            }
            Inline::Emoji { id, name, .. } => {
                directory.insert_emoji(name, *id);
            }
            _ => {}
        }
    }
    let mut keeps_words = None;
    for plan in PLANS {
        let candidate = emit_inlines(inlines, plan);
        let reparsed = parse_inlines(&candidate, &directory);
        if reparsed == inlines {
            return candidate;
        }
        if keeps_words.is_none() && words(&reparsed) == words(inlines) {
            keeps_words = Some(candidate);
        }
    }
    keeps_words.unwrap_or_else(|| {
        let plain: Vec<Inline> = inlines
            .iter()
            .map(|item| match item {
                Inline::Text { text, .. } => Inline::text(text.clone()),
                Inline::Link { url, text, .. } => Inline::link(url.clone(), text.clone()),
                Inline::Mention { handle, .. } => Inline::text(format!("@{handle}")),
                Inline::Emoji { name, .. } => Inline::text(format!(":{name}:")),
                Inline::LineBreak => Inline::LineBreak,
            })
            .collect();
        emit_inlines(&plain, PLANS[0])
    })
}

fn emit_inlines(inlines: &[Inline], plan: Plan) -> String {
    let mut out = String::new();
    // Spans currently open, outermost first.
    let mut open: Vec<(char, usize)> = Vec::new();
    let mut at_line_start = true;
    // Set after a bare address whose following text needs a backslash to stay separate from it.
    let mut separate_next = false;
    for (index, inline) in inlines.iter().enumerate() {
        let desired = desired_spans(inlines, index, &open, plan);
        let (closers, openers, now) = span_transition(&open, &desired, plan);
        out.push_str(&closers);
        out.push_str(&openers);
        open = now;
        match inline {
            Inline::Text { text, marks } => {
                if marks.code {
                    out.push_str(&code_span_to_markdown(text));
                } else {
                    if separate_next {
                        out.push('\\');
                    }
                    out.push_str(&escape_text(text, at_line_start));
                }
                at_line_start = false;
                separate_next = false;
            }
            Inline::LineBreak => {
                out.push('\n');
                at_line_start = true;
                separate_next = false;
            }
            Inline::Link { url, text, .. } => {
                // The address scanner takes every character up to whitespace, so whatever is
                // written directly after a bare address can be absorbed into it: not only the next
                // text but also the span markers written before it. Ordinary trailing punctuation
                // is trimmed from the address and stays readable. Plain text starting with other
                // punctuation gets a backslash to keep it apart; anything else (an emoji, a code
                // span, a span marker) cannot be escaped, so the link is written in its explicit
                // form instead. A span also cannot open directly after a letter, which is what a
                // bare address ends in.
                let mut allow_bare = true;
                separate_next = false;
                if let Some(next) = inlines.get(index + 1) {
                    let next_desired = desired_spans(inlines, index + 1, &open, plan);
                    let (closers, openers, _) = span_transition(&open, &next_desired, plan);
                    let tail = tail_until_stopper(inlines, index + 1, &open, plan);
                    let plain = matches!(next, Inline::Text { marks, .. } if !marks.code);
                    if !openers.is_empty() {
                        allow_bare = false;
                    }
                    if allow_bare && !tail.is_empty() && !bare_survives(url, &tail) {
                        let escapable = plain
                            && closers.is_empty()
                            && openers.is_empty()
                            && tail
                                .chars()
                                .next()
                                .is_some_and(|c| c.is_ascii_punctuation());
                        if escapable {
                            separate_next = true;
                        } else {
                            allow_bare = false;
                        }
                    }
                }
                let written = link_to_markdown(url, text, allow_bare);
                // The separator is only needed if the link was in fact written bare.
                separate_next &= written == *url;
                out.push_str(&written);
                at_line_start = false;
            }
            Inline::Mention { handle, .. } => {
                out.push('@');
                out.push_str(handle);
                at_line_start = false;
                // A `.` or `-` written straight after a mention would be read as part of a longer
                // name (`@alice` then `.b` is `@alice.b`), so a backslash keeps it apart. Other
                // text that continues a name (letters, digits) cannot be separated, and is
                // read the same way from the source it came from.
                separate_next = matches!(
                    inlines.get(index + 1),
                    Some(Inline::Text { text, marks })
                        if !marks.code && text.starts_with(['.', '-'])
                );
            }
            Inline::Emoji { name, .. } => {
                out.push(':');
                out.push_str(name);
                out.push(':');
                at_line_start = false;
                separate_next = false;
            }
        }
    }
    while let Some((symbol, _)) = open.pop() {
        out.push(symbol);
    }
    out
}

/// Characters that end a bare address.
fn is_address_stopper(c: char) -> bool {
    c.is_whitespace() || matches!(c, '<' | '>' | '\\')
}

/// The markdown that follows the item before `from`, up to the first character that would end a
/// bare address. Links further on are taken in their explicit form, which starts with `[`.
fn tail_until_stopper(
    inlines: &[Inline],
    from: usize,
    open: &[(char, usize)],
    plan: Plan,
) -> String {
    let mut out = String::new();
    let mut open = open.to_vec();
    for index in from..inlines.len() {
        let desired = desired_spans(inlines, index, &open, plan);
        let (closers, openers, now) = span_transition(&open, &desired, plan);
        out.push_str(&closers);
        out.push_str(&openers);
        open = now;
        match &inlines[index] {
            Inline::Text { text, marks } if marks.code => {
                out.push_str(&code_span_to_markdown(text))
            }
            Inline::Text { text, .. } => out.push_str(&escape_text(text, false)),
            Inline::Link { url, text, .. } => out.push_str(&link_to_markdown(url, text, false)),
            Inline::Mention { handle, .. } => {
                out.push('@');
                out.push_str(handle);
            }
            Inline::Emoji { name, .. } => {
                out.push(':');
                out.push_str(name);
                out.push(':');
            }
            Inline::LineBreak => out.push('\n'),
        }
        if out.chars().any(is_address_stopper) {
            break;
        }
    }
    match out.find(is_address_stopper) {
        Some(end) => out[..end].to_owned(),
        None => out,
    }
}

/// Whether the bare address `url` is still read as exactly that address when `following` comes
/// directly after it.
fn bare_survives(url: &str, following: &str) -> bool {
    let chars: Vec<char> = url.chars().chain(following.chars()).collect();
    matches!(
        bare_link(&chars, 0),
        Some((Inline::Link { url: found, .. }, end)) if found == url && end == url.chars().count()
    )
}

/// Markdown source that shows `text` exactly as written: every character that would read as markup
/// (emphasis, code, links, mentions, emoji names, and quotes or lists at a line's start) is
/// escaped. Line breaks are kept.
pub fn escape_plain(text: &str) -> String {
    text.split('\n')
        .map(|line| escape_text(line, true))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Markdown source for a link showing `text` and opening `url`: `[text](url)` with both escaped
/// so that it parses back to exactly that link. If no source can (an address that is not allowed,
/// or one with an unbalanced parenthesis), the text alone, escaped.
pub fn link_source(url: &str, text: &str) -> String {
    if !crate::document::link_is_allowed(url) || text.trim().is_empty() {
        return escape_text(text, false);
    }
    link_to_markdown(url, text, false)
}

/// A link as text that parses back to the same link, or, if no such text exists (an address
/// containing an unbalanced parenthesis, say), just its escaped display text.
fn link_to_markdown(url: &str, text: &str, allow_bare: bool) -> String {
    let link = Inline::link(url, text);
    let candidates = [
        (allow_bare && text == url).then(|| url.to_owned()),
        Some(format!(
            "[{}]({})",
            escape_text(text, false).replace(']', "\\]"),
            escape_address(url)
        )),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|candidate| parse_inlines(candidate, &NoMentions) == [link.clone()])
        .unwrap_or_else(|| escape_text(text, false))
}

/// An address inside `(...)` of an explicit link: backslashes are escaped, and so are parentheses
/// unless they are balanced (a balanced pair, as in a Wikipedia address, reads fine as it is).
fn escape_address(url: &str) -> String {
    let mut depth = 0i64;
    let mut balanced = true;
    for c in url.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                balanced &= depth >= 0;
            }
            _ => {}
        }
    }
    balanced &= depth == 0;
    let mut out = String::with_capacity(url.len());
    for c in url.chars() {
        if c == '\\' || (!balanced && (c == '(' || c == ')')) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn indent_continuations(text: &str) -> String {
    text.replace('\n', "\n  ")
}

fn block_to_markdown(block: &Block) -> String {
    match block {
        Block::Paragraph { inlines } => inlines_to_markdown(inlines),
        Block::CodeBlock { language, text } => {
            let longest_line_fence = text
                .split('\n')
                .map(|line| line.trim().chars().take_while(|c| *c == '`').count())
                .max()
                .unwrap_or(0);
            let fence = "`".repeat((longest_line_fence + 1).max(3));
            format!(
                "{fence}{}\n{text}\n{fence}",
                language.as_deref().unwrap_or("")
            )
        }
        Block::Quote { paragraphs } => paragraphs
            .iter()
            .map(|paragraph| {
                inlines_to_markdown(paragraph)
                    .split('\n')
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n>\n"),
        Block::BulletList { items } => items
            .iter()
            .map(|item| format!("- {}", indent_continuations(&inlines_to_markdown(item))))
            .collect::<Vec<_>>()
            .join("\n"),
        Block::OrderedList { start, items } => items
            .iter()
            .enumerate()
            .map(|(n, item)| {
                format!(
                    "{}. {}",
                    u64::from(*start) + n as u64,
                    indent_continuations(&inlines_to_markdown(item))
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[cfg(test)]
mod tests;
