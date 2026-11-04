//! Building HTML as text. Everything that came from a person or the server goes through [`esc`]:
//! the views never put unescaped text into markup.

use std::fmt::Write as _;

/// Text made safe for an element's content or a quoted attribute.
pub(crate) fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Appends formatted text to a buffer (`push!(out, "<p>{}</p>", esc(x))`).
macro_rules! push {
    ($out:expr, $($arg:tt)*) => {{
        #[allow(unused_imports)]
        use ::std::fmt::Write as _;
        let _ = $out.write_fmt(format_args!($($arg)*));
    }};
}
pub(crate) use push;

/// A count for a badge: up to 99, then "99+".
pub(crate) fn count(n: u32) -> String {
    if n > 99 {
        "99+".to_owned()
    } else {
        n.to_string()
    }
}

/// Up to two letters standing for a name.
pub(crate) fn initials(name: &str) -> String {
    let mut letters = name
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .flat_map(char::to_uppercase);
    let mut out = String::new();
    if let Some(first) = letters.next() {
        out.push(first);
    }
    if let Some(second) = letters.next() {
        out.push(second);
    }
    if out.is_empty() {
        out.push('?');
    }
    out
}

/// One of eight colours for a person, from their id, so a name has the same colour everywhere.
pub(crate) fn tint(seed: &str) -> u32 {
    let mut hash: u32 = 2_166_136_261;
    for byte in seed.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash % 8
}

/// A size in words: "812 bytes", "4.2 KB", "16 MB".
pub(crate) fn size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} bytes")
    } else if value < KB * KB {
        format!("{:.1} KB", value / KB)
    } else if value < KB * KB * KB {
        format!("{:.1} MB", value / KB / KB)
    } else {
        format!("{:.1} GB", value / KB / KB / KB)
    }
}

/// An attribute list of `data-` values for an action button.
pub(crate) fn data(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (name, value) in pairs {
        let _ = write!(out, " data-{name}=\"{}\"", esc(value));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_in_text_is_escaped() {
        assert_eq!(
            esc(r#"<img src=x onerror="alert('1')"> & co"#),
            "&lt;img src=x onerror=&quot;alert(&#39;1&#39;)&quot;&gt; &amp; co"
        );
    }

    #[test]
    fn initials_counts_and_sizes_read_well() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("grace"), "G");
        assert_eq!(initials("  "), "?");
        assert_eq!(initials("Ünal  Örs extra"), "ÜÖ");
        assert_eq!(count(5), "5");
        assert_eq!(count(120), "99+");
        assert_eq!(size(812), "812 bytes");
        assert_eq!(size(4300), "4.2 KB");
        assert_eq!(size(16 * 1024 * 1024), "16.0 MB");
        assert_eq!(data(&[("id", "a\"b")]), " data-id=\"a&quot;b\"");
        assert!(tint("x") < 8);
    }
}

thread_local! {
    static IN_SHELL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the page runs in a phone app's shell (D49), which says so in its user agent.
pub(crate) fn in_shell() -> bool {
    IN_SHELL.with(std::cell::Cell::get)
}

/// Set once, when the page starts.
pub(crate) fn set_in_shell(shell: bool) {
    IN_SHELL.with(|cell| cell.set(shell));
}
