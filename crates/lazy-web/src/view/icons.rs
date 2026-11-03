//! Icons as inline SVG, drawn in the text's colour. Symbols from fonts (★, ⋯, ＋) depend on what
//! the person's system has installed, and some have none; these draw the same everywhere.

macro_rules! svg {
    ($body:literal) => {
        concat!(
            r#"<svg class="ic" viewBox="0 0 24 24" width="1em" height="1em" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false">"#,
            $body,
            "</svg>"
        )
    };
}

macro_rules! svg_filled {
    ($body:literal) => {
        concat!(
            r#"<svg class="ic" viewBox="0 0 24 24" width="1em" height="1em" fill="currentColor" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" aria-hidden="true" focusable="false">"#,
            $body,
            "</svg>"
        )
    };
}

/// An icon by name, at the text's size.
pub(crate) fn icon(name: &str) -> &'static str {
    match name {
        "star" => svg!(
            r#"<path d="M12 3.5l2.6 5.3 5.9.9-4.3 4.1 1 5.8L12 16.9 6.8 19.6l1-5.8L3.5 9.7l5.9-.9z"/>"#
        ),
        "star-filled" => svg_filled!(
            r#"<path d="M12 3.5l2.6 5.3 5.9.9-4.3 4.1 1 5.8L12 16.9 6.8 19.6l1-5.8L3.5 9.7l5.9-.9z"/>"#
        ),
        "more" => svg_filled!(
            r#"<circle cx="5" cy="12" r="1.8"/><circle cx="12" cy="12" r="1.8"/><circle cx="19" cy="12" r="1.8"/>"#
        ),
        "plus" => svg!(r#"<path d="M12 5v14M5 12h14"/>"#),
        "caret" => svg!(r#"<path d="M7 10l5 5 5-5"/>"#),
        "search" => svg!(r#"<circle cx="11" cy="11" r="6.5"/><path d="M16 16l4.5 4.5"/>"#),
        "jump" => svg!(r#"<path d="M5 12h12M13 7l5 5-5 5"/>"#),
        "close" => svg!(r#"<path d="M6 6l12 12M18 6L6 18"/>"#),
        "menu" => svg!(r#"<path d="M4 7h16M4 12h16M4 17h16"/>"#),
        "attach" => svg!(
            r#"<path d="M20 11.5l-7.8 7.8a5 5 0 01-7.1-7.1l8.5-8.5a3.3 3.3 0 014.7 4.7l-8.5 8.5a1.7 1.7 0 01-2.4-2.4l7.8-7.8"/>"#
        ),
        "smile" => svg!(
            r#"<circle cx="12" cy="12" r="8.5"/><path d="M8.5 14.5a4.5 4.5 0 007 0"/><circle cx="9.2" cy="10" r=".6" fill="currentColor"/><circle cx="14.8" cy="10" r=".6" fill="currentColor"/>"#
        ),
        "reply" => svg!(
            r#"<path d="M20 15a3 3 0 01-3 3H9l-4 3v-3.5A3 3 0 013 15V7a3 3 0 013-3h11a3 3 0 013 3z"/>"#
        ),
        "edit" => svg!(r#"<path d="M4 20h4L19 9l-4-4L4 16z"/><path d="M14 6l4 4"/>"#),
        "delete" => svg!(r#"<path d="M5 7h14M10 7V4h4v3M7 7l1 13h8l1-13"/>"#),
        "lock" => svg!(
            r#"<rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 018 0v3"/>"#
        ),
        "hash" => svg!(r#"<path d="M9 4L7 20M17 4l-2 16M4.5 9h16M3.5 15h16"/>"#),
        "people" => svg!(
            r#"<circle cx="9" cy="8" r="3.5"/><path d="M2.5 20a6.5 6.5 0 0113 0"/><path d="M16 4.6a3.5 3.5 0 010 6.8M18 14a6.5 6.5 0 013.5 6"/>"#
        ),
        "file" => svg!(
            r#"<path d="M14 3H7a2 2 0 00-2 2v14a2 2 0 002 2h10a2 2 0 002-2V8z"/><path d="M14 3v5h5"/>"#
        ),
        "arrow" => svg!(r#"<path d="M5 12h13M13 7l5 5-5 5"/>"#),
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_an_svg() {
        for name in [
            "star",
            "star-filled",
            "more",
            "plus",
            "caret",
            "search",
            "jump",
            "close",
            "menu",
            "attach",
            "smile",
            "reply",
            "edit",
            "delete",
            "lock",
            "hash",
            "people",
            "file",
            "arrow",
        ] {
            let svg = icon(name);
            assert!(svg.starts_with("<svg class=\"ic\""), "{name}");
            assert!(svg.ends_with("</svg>"), "{name}");
        }
        assert_eq!(icon("nothing"), "");
    }
}
