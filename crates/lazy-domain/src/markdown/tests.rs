use std::time::{Duration, Instant};

use proptest::prelude::*;

use crate::document::{Block, DocumentError, Inline, MAX_TEXT_BYTES, Marks, MessageDocument};

fn doc(text: &str) -> MessageDocument {
    MessageDocument::from_markdown(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
}

/// The inlines of a message that is a single paragraph.
fn inlines(text: &str) -> Vec<Inline> {
    match doc(text).blocks() {
        [Block::Paragraph { inlines }] => inlines.clone(),
        other => panic!("{text:?} is not one paragraph: {other:?}"),
    }
}

fn t(s: &str) -> Inline {
    Inline::text(s)
}
fn bold(s: &str) -> Inline {
    Inline::marked(
        s,
        Marks {
            bold: true,
            ..Marks::NONE
        },
    )
}
fn italic(s: &str) -> Inline {
    Inline::marked(
        s,
        Marks {
            italic: true,
            ..Marks::NONE
        },
    )
}
fn strike(s: &str) -> Inline {
    Inline::marked(
        s,
        Marks {
            strike: true,
            ..Marks::NONE
        },
    )
}
fn code(s: &str) -> Inline {
    Inline::marked(
        s,
        Marks {
            code: true,
            ..Marks::NONE
        },
    )
}
fn both(s: &str) -> Inline {
    Inline::marked(
        s,
        Marks {
            bold: true,
            italic: true,
            ..Marks::NONE
        },
    )
}
fn link(url: &str, text: &str) -> Inline {
    Inline::link(url, text)
}

// ---- Inline syntax --------------------------------------------------------------------------

#[test]
fn marks_in_single_and_double_form() {
    assert_eq!(
        inlines("*bold* _it_ ~gone~ `code`"),
        [
            bold("bold"),
            t(" "),
            italic("it"),
            t(" "),
            strike("gone"),
            t(" "),
            code("code")
        ]
    );
    assert_eq!(
        inlines("**bold** __it__ ~~gone~~"),
        [bold("bold"), t(" "), italic("it"), t(" "), strike("gone")]
    );
}

#[test]
fn markers_only_pair_at_word_boundaries() {
    for plain in [
        "snake_case_name",
        "2*3*4",
        "a*b",
        "a * b * c",
        "tilde~in~word",
        "*bold*text",
        "text*bold*",
    ] {
        assert_eq!(inlines(plain), [t(plain)], "{plain}");
    }
    assert_eq!(inlines("(*bold*)"), [t("("), bold("bold"), t(")")]);
    assert_eq!(inlines("*bold*, and"), [bold("bold"), t(", and")]);
    assert_eq!(inlines("日本 *語*。"), [t("日本 "), bold("語"), t("。")]);
    assert_eq!(inlines("_café_ au lait"), [italic("café"), t(" au lait")]);
}

#[test]
fn whitespace_just_inside_a_marker_prevents_formatting() {
    assert_eq!(inlines("x * not bold*"), [t("x * not bold*")]);
    assert_eq!(inlines("a *not bold *b"), [t("a *not bold *b")]);
}

#[test]
fn marks_nest() {
    assert_eq!(
        inlines("*bold _both_ bold*"),
        [bold("bold "), both("both"), bold(" bold")]
    );
    // The same mark inside itself is literal rather than recursing.
    assert_eq!(inlines("*a *b* c*"), [bold("a *b"), t(" c*")]);
}

#[test]
fn inline_code_is_literal_and_can_contain_backticks() {
    assert_eq!(inlines("`a*b*_c_`"), [code("a*b*_c_")]);
    assert_eq!(inlines("`` a`b ``"), [code(" a`b ")]);
    assert_eq!(inlines("```x```"), [code("x")]);
    assert_eq!(
        inlines("a ` b"),
        [t("a ` b")],
        "an unmatched backtick is plain"
    );
    assert_eq!(
        inlines("*bold `code` bold*"),
        [
            bold("bold "),
            Inline::marked(
                "code",
                Marks {
                    bold: true,
                    code: true,
                    ..Marks::NONE
                }
            ),
            bold(" bold")
        ]
    );
}

#[test]
fn backslash_escapes_punctuation_only() {
    assert_eq!(inlines(r"\*not bold\*"), [t("*not bold*")]);
    assert_eq!(inlines(r"\\"), [t(r"\")]);
    assert_eq!(
        inlines(r"a\b"),
        [t(r"a\b")],
        "a backslash before a letter is just a backslash"
    );
    assert_eq!(inlines(r"\`not code\`"), [t("`not code`")]);
}

#[test]
fn explicit_links() {
    assert_eq!(
        inlines("read [the docs](https://example.com/x?y=z) now"),
        [
            t("read "),
            link("https://example.com/x?y=z", "the docs"),
            t(" now")
        ]
    );
    assert_eq!(
        inlines("[mail](mailto:me@example.com)"),
        [link("mailto:me@example.com", "mail")]
    );
    assert_eq!(
        inlines(r"[a \] b](https://x.y)"),
        [link("https://x.y", "a ] b")]
    );
}

#[test]
fn unsafe_or_malformed_links_stay_plain_text() {
    for plain in [
        "[x](javascript:alert(1))",
        "[x](data:text/html,hi)",
        "[x](file:///etc/passwd)",
        "[](https://a.b)",
        "[x](https://a b)",
        "[x] (https://a.b)",
        "[x](https://a.b",
        "[unclosed",
    ] {
        // A bare address inside the text may still become a link, but never a link *labelled*
        // with other text, which is what the markup would have made.
        let parsed = inlines(plain);
        assert!(
            parsed
                .iter()
                .all(|i| !matches!(i, Inline::Link { url, text, .. } if url != text)),
            "{plain}: {parsed:?}"
        );
    }
}

#[test]
fn bare_addresses_become_links_without_the_trailing_sentence() {
    assert_eq!(
        inlines("see https://example.com/a, and (https://b.c/d)."),
        [
            t("see "),
            link("https://example.com/a", "https://example.com/a"),
            t(", and ("),
            link("https://b.c/d", "https://b.c/d"),
            t(").")
        ]
    );
    let wiki = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
    assert_eq!(
        inlines(wiki),
        [link(wiki, wiki)],
        "balanced parentheses are part of the address"
    );
    assert_eq!(
        inlines("*see https://a.b*"),
        [
            bold("see "),
            Inline::marked_link(
                "https://a.b",
                "https://a.b",
                Marks {
                    bold: true,
                    ..Marks::NONE
                }
            )
        ]
    );
    let permalink = "lazy://3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2c/3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2d/3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2e";
    assert_eq!(inlines(permalink), [link(permalink, permalink)]);
    assert_eq!(
        inlines("xhttps://a.b"),
        [t("xhttps://a.b")],
        "not at a word boundary"
    );
    assert_eq!(
        inlines("https://"),
        [t("https://")],
        "nothing after the scheme"
    );
}

// ---- Blocks ----------------------------------------------------------------------------------

#[test]
fn paragraphs_and_line_breaks() {
    assert_eq!(inlines("a\nb"), [t("a"), Inline::LineBreak, t("b")]);
    let two = doc("a\nb\n\nc");
    assert_eq!(two.blocks().len(), 2);
    assert_eq!(two.plain_text(), "a\nb\n\nc");
}

#[test]
fn quotes() {
    let quoted = doc("> a\n> b\n>\n> c");
    assert_eq!(
        quoted.blocks(),
        [Block::Quote {
            paragraphs: vec![vec![t("a"), Inline::LineBreak, t("b")], vec![t("c")]]
        }]
    );
    assert_eq!(quoted.plain_text(), "> a\n> b\n>\n> c");
}

#[test]
fn lists() {
    let bullets = doc("- one\n* two\n  continued\n- three");
    assert_eq!(
        bullets.blocks(),
        [Block::BulletList {
            items: vec![
                vec![t("one")],
                vec![t("two"), Inline::LineBreak, t("continued")],
                vec![t("three")]
            ]
        }]
    );
    let numbered = doc("3. a\n4) b");
    assert_eq!(
        numbered.blocks(),
        [Block::OrderedList {
            start: 3,
            items: vec![vec![t("a")], vec![t("b")]]
        }]
    );
    assert_eq!(numbered.plain_text(), "3. a\n4. b");
}

#[test]
fn emphasis_at_the_start_of_a_line_is_not_a_bullet() {
    assert_eq!(inlines("*bold* text"), [bold("bold"), t(" text")]);
    assert_eq!(
        doc("* real bullet").blocks(),
        [Block::BulletList {
            items: vec![vec![t("real bullet")]]
        }]
    );
    assert_eq!(inlines("-5 degrees"), [t("-5 degrees")]);
    assert_eq!(inlines("10.5 is a number"), [t("10.5 is a number")]);
}

#[test]
fn code_blocks() {
    let fenced = doc("```rust\nfn main() {}\n```\nafter");
    assert_eq!(
        fenced.blocks(),
        [
            Block::CodeBlock {
                language: Some("rust".into()),
                text: "fn main() {}".into()
            },
            Block::Paragraph {
                inlines: vec![t("after")]
            }
        ]
    );
    // An unterminated fence runs to the end; an empty one disappears.
    assert_eq!(
        doc("```\na\nb").blocks(),
        [Block::CodeBlock {
            language: None,
            text: "a\nb".into()
        }]
    );
    assert_eq!(doc("```\n```\nx").blocks().len(), 1);
    // Markup inside is literal, and whitespace is kept.
    assert_eq!(
        doc("```\n*a*  \n  - b\n```").blocks(),
        [Block::CodeBlock {
            language: None,
            text: "*a*  \n  - b".into()
        }]
    );
    // A longer fence can contain a shorter one.
    assert_eq!(
        doc("````\n```\ninner\n```\n````").blocks(),
        [Block::CodeBlock {
            language: None,
            text: "```\ninner\n```".into()
        }]
    );
    // A bad language tag is dropped, not fatal.
    assert_eq!(
        doc("```not a language\nx\n```").blocks(),
        [Block::CodeBlock {
            language: None,
            text: "x".into()
        }]
    );
}

#[test]
fn blocks_interrupt_paragraphs() {
    let mixed = doc("intro\n- a\n- b\n\noutro");
    assert!(matches!(
        mixed.blocks(),
        [
            Block::Paragraph { .. },
            Block::BulletList { .. },
            Block::Paragraph { .. }
        ]
    ));
}

#[test]
fn only_the_limits_are_errors() {
    assert_eq!(
        MessageDocument::from_markdown(""),
        Err(DocumentError::Empty)
    );
    assert_eq!(
        MessageDocument::from_markdown("  \n \t"),
        Err(DocumentError::Empty)
    );
    assert!(
        MessageDocument::from_markdown("***").is_ok(),
        "stray markers are text"
    );
    assert!(MessageDocument::from_markdown("`").is_ok());
    assert_eq!(
        MessageDocument::from_markdown(&"x".repeat(MAX_TEXT_BYTES + 1)),
        Err(DocumentError::TooLong)
    );
    let blocks = vec!["x"; 101].join("\n\n");
    assert_eq!(
        MessageDocument::from_markdown(&blocks),
        Err(DocumentError::TooManyBlocks)
    );
}

#[test]
fn control_characters_and_crlf_are_normalized() {
    assert_eq!(
        inlines("a\r\nb\u{7}c"),
        [t("a"), Inline::LineBreak, t("bc")]
    );
}

// ---- Serialization ----------------------------------------------------------------------------

#[test]
fn source_round_trips_for_representative_messages() {
    for source in [
        "*bold* _it_ ~gone~ `code`",
        "*bold _both_ bold*",
        "plain *and bold [link](https://a.b/c) more*",
        "see https://example.com/a, ok",
        "`` a`b `` and \\*literal\\*",
        "> quoted *text*\n>\n> second",
        "- one\n- two\n  more\n- *three*",
        "7. seven\n8. eight",
        "```rust\nfn main() {\n    println!(\"hi\");\n}\n```",
        "````\n```\ninner\n```\n````",
        "line one\nline two\n\n- list\n\n> quote\n\nend",
        "1\\. not a list and \\- not a bullet and \\> not a quote",
        "mixed `a` *b* _c_ ~d~ [e](https://f.g) https://h.i",
    ] {
        let parsed = doc(source);
        let rewritten = parsed.to_markdown();
        assert_eq!(
            doc(&rewritten),
            parsed,
            "{source:?} was rewritten as {rewritten:?}"
        );
    }
}

#[test]
fn editing_shows_readable_source() {
    // What a person sees when they edit a message: their own markup, not a pile of escapes.
    for source in [
        "*bold* and _it_",
        "snake_case and 2*3*4",
        "a [link](https://a.b) here",
        "> quote",
        "- a\n- b",
    ] {
        assert_eq!(doc(source).to_markdown(), source, "{source}");
    }
}

#[test]
fn a_link_whose_address_cannot_be_written_bare_falls_back_to_the_explicit_form() {
    // The trailing dot would be read as sentence punctuation if the address were written bare.
    let document = MessageDocument::new(vec![Block::Paragraph {
        inlines: vec![link("https://a.b/x.", "https://a.b/x.")],
    }])
    .unwrap();
    assert_eq!(doc(&document.to_markdown()), document);
}

// ---- Properties --------------------------------------------------------------------------------

fn markup_heavy() -> impl Strategy<Value = String> {
    let pieces = prop::sample::select(vec![
        "*",
        "**",
        "_",
        "__",
        "~",
        "`",
        "``",
        "```",
        "[",
        "]",
        "(",
        ")",
        "\\",
        ">",
        "- ",
        "* ",
        "1. ",
        "2) ",
        "\n",
        "\n\n",
        " ",
        "  ",
        "a",
        "b",
        "é",
        "日",
        "🎉",
        "x_y",
        "http://x.y",
        "https://a.b/c_d)",
        "lazy://a/b/c",
        "rust",
        "[t](https://u.v)",
        "[t](https://u.v/(w))",
    ]);
    prop::collection::vec(pieces, 0..40).prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4_000))]

    #[test]
    fn parsing_never_panics_and_always_yields_a_valid_document_or_a_limit_error(input in prop_oneof![any::<String>(), markup_heavy()]) {
        if let Ok(parsed) = MessageDocument::from_markdown(&input) {
            let projected = parsed.plain_text();
            prop_assert!(projected.chars().all(|c| c == '\n' || c == '\t' || !c.is_control()));
            // The document passes its own validation, including after a trip through JSON.
            let json = serde_json::to_string(&parsed).unwrap();
            prop_assert_eq!(serde_json::from_str::<MessageDocument>(&json).unwrap(), parsed);
        }
    }

    #[test]
    fn source_written_from_a_parsed_document_parses_back_to_it(input in markup_heavy()) {
        if let Ok(parsed) = MessageDocument::from_markdown(&input) {
            let rewritten = parsed.to_markdown();
            prop_assert_eq!(
                MessageDocument::from_markdown(&rewritten),
                Ok(parsed),
                "{:?} was rewritten as {:?}", input, rewritten
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_500))]

    #[test]
    fn arbitrary_text_round_trips_too(input in any::<String>()) {
        if let Ok(parsed) = MessageDocument::from_markdown(&input) {
            let rewritten = parsed.to_markdown();
            prop_assert_eq!(MessageDocument::from_markdown(&rewritten), Ok(parsed), "{:?} -> {:?}", input, rewritten);
        }
    }
}

// ---- Hostile input -------------------------------------------------------------------------------

/// Parses `input` and fails if it takes long enough to be an abuse vector. The bound is generous
/// (tests run unoptimized); the inputs below take seconds if parsing is quadratic.
fn assert_fast(name: &str, input: &str) {
    let started = Instant::now();
    let _ = MessageDocument::from_markdown(input).map(|d| d.to_markdown());
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "{name}: {} bytes took {took:?}",
        input.len()
    );
}

#[test]
fn pathological_input_is_parsed_in_linear_time() {
    let n = MAX_TEXT_BYTES;
    assert_fast("unmatched stars", &"*a ".repeat(n / 3));
    assert_fast("unmatched underscores", &"_a ".repeat(n / 3));
    assert_fast("unmatched tildes", &"~a ".repeat(n / 3));
    assert_fast("open brackets", &"[".repeat(n));
    assert_fast("open links", &"[a](".repeat(n / 4));
    assert_fast("backticks", &"`".repeat(n));
    assert_fast("alternating backticks", &"`a".repeat(n / 2));
    assert_fast("deep nesting attempts", &"*_~".repeat(n / 3));
    assert_fast("closers that never fit", &"*a*b".repeat(n / 4));
    assert_fast("many short lines", &"* a\n".repeat(n / 4));
    assert_fast("quote lines", &"> a\n".repeat(n / 4));
    assert_fast("fences", &"```\n".repeat(n / 4));
    assert_fast("urls", &"http://a.b ".repeat(n / 11));
    assert_fast("escapes", &"\\*".repeat(n / 2));
    assert_fast("one long word", &"a".repeat(n));
}

// ---- Soak ----------------------------------------------------------------------------------------

/// A long run of the round-trip property over markup-heavy input. The words must always survive;
/// the exact formatting may fail in a rare arrangement (see `inlines_to_markdown`), so a few
/// misses are tolerated and reported. Opt in with
/// `cargo test -p lazy-domain --release -- --ignored soak`.
#[test]
#[ignore = "long-running; run on demand"]
fn soak_round_trip() {
    use proptest::test_runner::{Config, TestRunner};
    use std::cell::Cell;

    let inexact = Cell::new(0_u32);
    let mut runner = TestRunner::new(Config {
        cases: 400_000,
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&markup_heavy(), |input| {
            if let Ok(parsed) = MessageDocument::from_markdown(&input) {
                let rewritten = parsed.to_markdown();
                let again = MessageDocument::from_markdown(&rewritten);
                if again.as_ref() != Ok(&parsed) {
                    inexact.set(inexact.get() + 1);
                    let words = again.as_ref().map(MessageDocument::plain_text);
                    prop_assert_eq!(
                        words,
                        Ok(parsed.plain_text()),
                        "{:?} -> {:?}",
                        input,
                        rewritten
                    );
                }
            }
            Ok(())
        })
        .unwrap();
    eprintln!("inexact round trips: {}", inexact.get());
    assert!(inexact.get() <= 3, "{} inexact round trips", inexact.get());
}

/// Found by the soak: a span closing right where a different one opens, with the same marker
/// reused, would merge into a longer run (`a__*`) unless the new spans open in another order.
#[test]
fn spans_changing_at_a_shared_marker_keep_their_marks() {
    for input in ["\\__a_*__~__* ", "_a_*__b__*", "_a_*_b_*"] {
        let parsed = doc(input);
        assert_eq!(
            MessageDocument::from_markdown(&parsed.to_markdown()),
            Ok(parsed.clone()),
            "{input:?} -> {:?}",
            parsed.to_markdown()
        );
    }
}

/// The one arrangement the soak has found that no plan writes out exactly (a bold-italic run whose
/// marks begin and end beside punctuation that is also a marker). The words must still survive.
#[test]
fn an_arrangement_no_plan_can_write_keeps_its_words() {
    let parsed = doc("**a**__**]]~a~ _**__");
    let rewritten = parsed.to_markdown();
    let again = doc(&rewritten);
    assert_eq!(again.plain_text(), parsed.plain_text(), "{rewritten:?}");
}

// ---- Mentions ------------------------------------------------------------------------------------

mod mentions {
    use super::*;
    use crate::{GroupId, MapDirectory, MentionTarget, UserId};

    struct People {
        alice: MentionTarget,
        bob: MentionTarget,
        team: MentionTarget,
        directory: MapDirectory,
    }

    fn people() -> People {
        let alice = MentionTarget::User {
            id: UserId::generate(),
        };
        let bob = MentionTarget::User {
            id: UserId::generate(),
        };
        let team = MentionTarget::Group {
            id: GroupId::generate(),
        };
        let mut directory = MapDirectory::new();
        directory
            .insert("alice", alice)
            .insert("bob", bob)
            .insert("alice.b", bob)
            .insert("a_b", alice)
            .insert("team-1", team)
            .insert("channel", MentionTarget::Channel);
        People {
            alice,
            bob,
            team,
            directory,
        }
    }

    fn parse(people: &People, text: &str) -> Vec<Inline> {
        match MessageDocument::from_markdown_with(text, &people.directory)
            .unwrap_or_else(|e| panic!("{text:?}: {e}"))
            .blocks()
        {
            [Block::Paragraph { inlines }] => inlines.clone(),
            other => panic!("{text:?} is not one paragraph: {other:?}"),
        }
    }

    fn mention(target: MentionTarget, handle: &str) -> Inline {
        Inline::mention(target, handle)
    }

    #[test]
    fn a_name_the_directory_knows_becomes_a_mention() {
        let p = people();
        assert_eq!(
            parse(&p, "hi @alice and @team-1, @channel!"),
            [
                t("hi "),
                mention(p.alice, "alice"),
                t(" and "),
                mention(p.team, "team-1"),
                t(", "),
                mention(MentionTarget::Channel, "channel"),
                t("!"),
            ]
        );
    }

    #[test]
    fn without_a_directory_nothing_is_a_mention() {
        assert_eq!(inlines("hi @alice @channel"), [t("hi @alice @channel")]);
    }

    #[test]
    fn a_mention_opens_at_a_word_start_and_never_inside_an_address() {
        let p = people();
        // An email address, a word, and a path keep their `@`.
        assert_eq!(parse(&p, "bob@alice.com"), [t("bob@alice.com")]);
        assert_eq!(parse(&p, "x@alice"), [t("x@alice")]);
        // After punctuation or a line start it opens.
        assert_eq!(
            parse(&p, "(@alice)"),
            [t("("), mention(p.alice, "alice"), t(")")]
        );
        assert_eq!(parse(&p, "@alice"), [mention(p.alice, "alice")],);
    }

    #[test]
    fn the_longest_known_name_wins_and_trailing_punctuation_is_left_alone() {
        let p = people();
        assert_eq!(
            parse(&p, "@alice.b is here"),
            [mention(p.bob, "alice.b"), t(" is here")]
        );
        assert_eq!(
            parse(&p, "thanks @alice."),
            [t("thanks "), mention(p.alice, "alice"), t(".")]
        );
        assert_eq!(parse(&p, "@alice-"), [mention(p.alice, "alice"), t("-")]);
        // A name the directory does not know stays text, even if a prefix of it is known.
        assert_eq!(
            parse(&p, "@alicebob"),
            [mention(p.alice, "alice"), t("bob")]
        );
        assert_eq!(parse(&p, "@nobody"), [t("@nobody")]);
        assert_eq!(parse(&p, "@"), [t("@")]);
        assert_eq!(parse(&p, "@@alice"), [t("@"), mention(p.alice, "alice")]);
    }

    #[test]
    fn names_are_case_insensitive_and_written_in_lowercase() {
        let p = people();
        let parsed = parse(&p, "@Alice and @BOB");
        assert_eq!(
            parsed,
            [mention(p.alice, "alice"), t(" and "), mention(p.bob, "bob")]
        );
    }

    #[test]
    fn a_mention_carries_formatting_and_ends_a_span() {
        let p = people();
        let bold_alice = Inline::Mention {
            target: p.alice,
            handle: "alice".to_owned(),
            marks: Marks {
                bold: true,
                ..Marks::NONE
            },
        };
        assert_eq!(parse(&p, "*@alice*"), [bold_alice]);
        // A handle that ends in a letter lets `_` close an italic span right after it.
        let italic_alice = Inline::Mention {
            target: p.alice,
            handle: "alice".to_owned(),
            marks: Marks {
                italic: true,
                ..Marks::NONE
            },
        };
        assert_eq!(parse(&p, "_@alice_"), [italic_alice]);
        // An underscore inside a handle is part of it.
        assert_eq!(parse(&p, "@a_b"), [mention(p.alice, "a_b")]);
    }

    #[test]
    fn escapes_code_and_links_keep_an_at_sign_literal() {
        let p = people();
        assert_eq!(parse(&p, "\\@alice"), [t("@alice")]);
        assert_eq!(parse(&p, "`@alice`"), [code("@alice")]);
        assert_eq!(
            parse(&p, "[@alice](https://x.y)"),
            [Inline::link("https://x.y", "@alice")]
        );
        let block = MessageDocument::from_markdown_with("```\n@alice\n```", &p.directory).unwrap();
        assert!(matches!(&block.blocks()[0], Block::CodeBlock { text, .. } if text == "@alice"));
    }

    #[test]
    fn mentions_work_inside_lists_and_quotes() {
        let p = people();
        let doc =
            MessageDocument::from_markdown_with("> ping @bob\n\n- @alice\n- @team-1", &p.directory)
                .unwrap();
        assert_eq!(doc.mentions(), [p.bob, p.alice, p.team]);
        assert_eq!(doc.plain_text(), "> ping @bob\n\n- @alice\n- @team-1");
    }

    #[test]
    fn a_message_of_only_a_mention_is_a_message() {
        let p = people();
        let doc = MessageDocument::from_markdown_with("@alice", &p.directory).unwrap();
        assert_eq!(doc.plain_text(), "@alice");
    }

    #[test]
    fn documents_with_mentions_are_version_three_and_others_stay_version_two() {
        let p = people();
        let with = MessageDocument::from_markdown_with("hi @alice", &p.directory).unwrap();
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["version"], 3);
        assert_eq!(
            json["blocks"][0]["inlines"][1],
            serde_json::json!({"type": "mention", "target": {"type": "user", "id": match p.alice { MentionTarget::User { id } => id.to_string(), _ => unreachable!() }}, "handle": "alice"})
        );
        assert_eq!(
            serde_json::from_value::<MessageDocument>(json).unwrap(),
            with
        );
        let without = MessageDocument::from_markdown("hi there").unwrap();
        assert_eq!(serde_json::to_value(&without).unwrap()["version"], 2);
    }

    #[test]
    fn invalid_mentions_are_refused_by_the_document() {
        let target = MentionTarget::Channel;
        for handle in ["", "a", "Bad", "ends-", "has space"] {
            let blocks = vec![Block::Paragraph {
                inlines: vec![Inline::mention(target, handle)],
            }];
            assert_eq!(
                MessageDocument::new(blocks),
                Err(DocumentError::InvalidMention),
                "{handle:?}"
            );
        }
        let coded = Inline::Mention {
            target,
            handle: "channel".to_owned(),
            marks: Marks {
                code: true,
                ..Marks::NONE
            },
        };
        assert_eq!(
            MessageDocument::new(vec![Block::Paragraph {
                inlines: vec![coded]
            }]),
            Err(DocumentError::InvalidMention)
        );
        let many = Block::Paragraph {
            inlines: (0..=crate::mention::MAX_MENTIONS)
                .map(|_| Inline::mention(target, "channel"))
                .collect(),
        };
        assert_eq!(
            MessageDocument::new(vec![many]),
            Err(DocumentError::TooManyMentions)
        );
    }

    #[test]
    fn mentions_round_trip_through_source() {
        let p = people();
        for input in [
            "hi @alice and @team-1, @channel!",
            "*@alice* _@bob_ ~@a_b~",
            "(@alice)",
            "@alice.b is here, @alice.",
            "\\@alice is not a mention",
            "email bob@alice.com and @bob",
            "> @alice said\n\n1. @bob\n2. @team-1",
            "**bold @alice and _both @bob_ here**",
            "@alice@bob",
            "[link](https://a.b) @alice",
            "https://a.b/c @alice",
            "@alice https://a.b/c",
        ] {
            let parsed = MessageDocument::from_markdown_with(input, &p.directory).unwrap();
            let source = parsed.to_markdown();
            assert_eq!(
                MessageDocument::from_markdown_with(&source, &p.directory),
                Ok(parsed),
                "{input:?} was rewritten as {source:?}"
            );
        }
    }

    #[test]
    fn an_at_sign_that_could_open_a_mention_is_escaped_when_written() {
        let doc = MessageDocument::from_markdown("ask @alice and mail bob@x.com").unwrap();
        let source = doc.to_markdown();
        assert_eq!(source, "ask \\@alice and mail bob@x.com");
        assert_eq!(MessageDocument::from_markdown(&source), Ok(doc));
    }

    fn mention_heavy() -> impl Strategy<Value = String> {
        let pieces = prop::sample::select(vec![
            "@",
            "@alice",
            "@bob",
            "@team-1",
            "@channel",
            "@a_b",
            "@alice.b",
            "@Alice",
            "@x",
            "*",
            "_",
            "~",
            "`",
            "\\",
            "[",
            "]",
            "(",
            ")",
            "-",
            ".",
            " ",
            "\n",
            "a",
            "b",
            "é",
            "> ",
            "- ",
            "1. ",
            "https://x.y",
            "[t](https://u.v)",
        ]);
        prop::collection::vec(pieces, 0..40).prop_map(|parts| parts.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4_000))]

        #[test]
        fn source_with_mentions_parses_back_to_the_same_document(input in mention_heavy()) {
            let p = people();
            if let Ok(parsed) = MessageDocument::from_markdown_with(&input, &p.directory) {
                let rewritten = parsed.to_markdown();
                prop_assert_eq!(
                    MessageDocument::from_markdown_with(&rewritten, &p.directory),
                    Ok(parsed),
                    "{:?} was rewritten as {:?}", input, rewritten
                );
            }
        }

        #[test]
        fn a_mention_never_changes_the_words(input in mention_heavy()) {
            let p = people();
            if let (Ok(with), Ok(without)) = (
                MessageDocument::from_markdown_with(&input, &p.directory),
                MessageDocument::from_markdown(&input),
            ) {
                // Whether `@alice` is a mention or plain text, the words are the same (a mention
                // is written in its lowercase form).
                prop_assert_eq!(with.plain_text().to_lowercase(), without.plain_text().to_lowercase());
            }
        }
    }

    #[test]
    fn many_at_signs_are_parsed_in_linear_time() {
        let p = people();
        for input in [
            "@".repeat(MAX_TEXT_BYTES),
            "@a".repeat(MAX_TEXT_BYTES / 2),
            "@alice".repeat(MAX_TEXT_BYTES / 6),
            "@alice.".repeat(MAX_TEXT_BYTES / 7),
            "@a_b_".repeat(MAX_TEXT_BYTES / 5),
            format!("@{}", "a".repeat(MAX_TEXT_BYTES)),
        ] {
            let started = Instant::now();
            let _ =
                MessageDocument::from_markdown_with(&input, &p.directory).map(|d| d.to_markdown());
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "{} bytes took {:?}",
                input.len(),
                started.elapsed()
            );
        }
    }

    /// Like the soak above, with mentions in the alphabet. Opt in with
    /// `cargo test -p lazy-domain --release -- --ignored soak`.
    #[test]
    #[ignore = "long-running; run on demand"]
    fn soak_round_trip_with_mentions() {
        use proptest::test_runner::{Config, TestRunner};
        use std::cell::Cell;

        let p = people();
        let inexact = Cell::new(0_u32);
        let mut runner = TestRunner::new(Config {
            cases: 400_000,
            failure_persistence: None,
            ..Config::default()
        });
        runner
            .run(&mention_heavy(), |input| {
                if let Ok(parsed) = MessageDocument::from_markdown_with(&input, &p.directory) {
                    let rewritten = parsed.to_markdown();
                    let again = MessageDocument::from_markdown_with(&rewritten, &p.directory);
                    if again.as_ref() != Ok(&parsed) {
                        inexact.set(inexact.get() + 1);
                        let words = again.as_ref().map(MessageDocument::plain_text);
                        prop_assert_eq!(
                            words,
                            Ok(parsed.plain_text()),
                            "{:?} -> {:?}",
                            input,
                            rewritten
                        );
                    }
                }
                Ok(())
            })
            .unwrap();
        eprintln!("inexact round trips: {}", inexact.get());
        assert!(inexact.get() <= 3, "{} inexact round trips", inexact.get());
    }
}

// ---- Custom emoji --------------------------------------------------------------------------------

mod custom_emoji {
    use super::*;
    use crate::{EmojiId, MapDirectory};

    struct Workspace {
        parrot: EmojiId,
        party: EmojiId,
        directory: MapDirectory,
    }

    fn workspace() -> Workspace {
        let (parrot, party) = (EmojiId::generate(), EmojiId::generate());
        let mut directory = MapDirectory::new();
        directory
            .insert_emoji("party_parrot", parrot)
            .insert_emoji("qz", party)
            .insert_emoji("q1", party);
        Workspace {
            parrot,
            party,
            directory,
        }
    }

    fn parse(w: &Workspace, text: &str) -> Vec<Inline> {
        match MessageDocument::from_markdown_with(text, &w.directory)
            .unwrap_or_else(|e| panic!("{text:?}: {e}"))
            .blocks()
        {
            [Block::Paragraph { inlines }] => inlines.clone(),
            other => panic!("{text:?} is not one paragraph: {other:?}"),
        }
    }

    #[test]
    fn a_name_the_workspace_has_becomes_an_inline_emoji() {
        let w = workspace();
        assert_eq!(
            parse(&w, "ship it :party_parrot: now :qz:"),
            [
                t("ship it "),
                Inline::emoji(w.parrot, "party_parrot"),
                t(" now "),
                Inline::emoji(w.party, "qz"),
            ]
        );
        // Names are case-insensitive and written lowercase.
        assert_eq!(
            parse(&w, ":Party_Parrot:"),
            [Inline::emoji(w.parrot, "party_parrot")]
        );
    }

    #[test]
    fn unknown_names_times_and_code_stay_text() {
        let w = workspace();
        assert_eq!(parse(&w, ":no_such:"), [t(":no_such:")]);
        assert_eq!(parse(&w, "at 10:30:45 sharp"), [t("at 10:30:45 sharp")]);
        assert_eq!(parse(&w, "word:qz:"), [t("word:qz:")]);
        assert_eq!(parse(&w, "\\:qz:"), [t(":qz:")]);
        assert_eq!(parse(&w, "`:qz:`"), [code(":qz:")]);
        assert_eq!(parse(&w, ":q:"), [t(":q:")]);
        assert_eq!(parse(&w, "::qz:"), [t(":"), Inline::emoji(w.party, "qz")]);
        assert_eq!(
            inlines("no directory :party_parrot:"),
            [t("no directory :party_parrot:")]
        );
    }

    #[test]
    fn emoji_carry_formatting_and_sit_next_to_punctuation() {
        let w = workspace();
        let bold_party = Inline::Emoji {
            id: w.party,
            name: "qz".to_owned(),
            marks: Marks {
                bold: true,
                ..Marks::NONE
            },
        };
        assert_eq!(parse(&w, "*:qz:*"), [bold_party]);
        assert_eq!(
            parse(&w, "(:qz:)!"),
            [t("("), Inline::emoji(w.party, "qz"), t(")!")]
        );
        assert_eq!(
            parse(&w, ":qz::q1:"),
            [Inline::emoji(w.party, "qz"), Inline::emoji(w.party, "q1")]
        );
    }

    #[test]
    fn a_document_with_one_is_version_four_and_says_what_it_used() {
        let w = workspace();
        let doc = MessageDocument::from_markdown_with("go :party_parrot:", &w.directory).unwrap();
        assert_eq!(serde_json::to_value(&doc).unwrap()["version"], 4);
        assert_eq!(doc.custom_emoji(), [w.parrot]);
        assert_eq!(doc.plain_text(), "go :party_parrot:");
        let json = serde_json::to_string(&doc).unwrap();
        assert_eq!(serde_json::from_str::<MessageDocument>(&json).unwrap(), doc);
        // A message that is only an emoji is a message.
        assert!(MessageDocument::from_markdown_with(":qz:", &w.directory).is_ok());
        // Mentions alone stay at version three.
        let mention = MessageDocument::new(vec![Block::Paragraph {
            inlines: vec![Inline::mention(crate::MentionTarget::Channel, "channel")],
        }])
        .unwrap();
        assert_eq!(serde_json::to_value(&mention).unwrap()["version"], 3);
    }

    #[test]
    fn invalid_emoji_are_refused_by_the_document() {
        let id = EmojiId::generate();
        for name in ["", "a", "Bad", "_x", "has space", "ends_"] {
            let blocks = vec![Block::Paragraph {
                inlines: vec![Inline::emoji(id, name)],
            }];
            assert_eq!(
                MessageDocument::new(blocks),
                Err(DocumentError::InvalidEmoji),
                "{name:?}"
            );
        }
        let many = Block::Paragraph {
            inlines: (0..=crate::emoji::MAX_EMOJI_PER_MESSAGE)
                .map(|_| Inline::emoji(id, "qz"))
                .collect(),
        };
        assert_eq!(
            MessageDocument::new(vec![many]),
            Err(DocumentError::TooManyEmoji)
        );
    }

    #[test]
    fn emoji_round_trip_through_source_and_literal_shortcodes_stay_literal() {
        let w = workspace();
        for input in [
            "ship it :party_parrot: now",
            ":qz::q1:",
            "*:qz: bold* _:party_parrot:_",
            "(:qz:)",
            "> quoted :qz:\n\n- :party_parrot: item",
            "\\:qz: is literal and :qz: is not",
            "at 10:30:45 and :qz:",
            "https://a.b/c :qz:",
        ] {
            let parsed = MessageDocument::from_markdown_with(input, &w.directory).unwrap();
            let source = parsed.to_markdown();
            assert_eq!(
                MessageDocument::from_markdown_with(&source, &w.directory),
                Ok(parsed),
                "{input:?} was rewritten as {source:?}"
            );
        }
        // Writing a literal `:name:` escapes it, so editing never turns text into an emoji (or,
        // in the composer, into a standard emoji).
        let doc = MessageDocument::from_markdown("keep :tada: as text, 10:30:45 too").unwrap();
        assert_eq!(doc.to_markdown(), "keep \\:tada: as text, 10:30:45 too");
        assert_eq!(MessageDocument::from_markdown(&doc.to_markdown()), Ok(doc));
    }

    fn emoji_heavy() -> impl Strategy<Value = String> {
        let pieces = prop::sample::select(vec![
            ":",
            "::",
            ":qz:",
            ":q1:",
            ":party_parrot:",
            ":Party_Parrot:",
            ":no_such:",
            ":a:",
            "qz",
            "party_parrot",
            "_",
            "*",
            "~",
            "`",
            "\\",
            "[",
            "]",
            "(",
            ")",
            " ",
            "\n",
            "a",
            "b",
            "1",
            "@",
            "> ",
            "- ",
            "https://x.y",
        ]);
        prop::collection::vec(pieces, 0..40).prop_map(|parts| parts.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4_000))]

        #[test]
        fn source_with_emoji_parses_back_to_the_same_document(input in emoji_heavy()) {
            let w = workspace();
            if let Ok(parsed) = MessageDocument::from_markdown_with(&input, &w.directory) {
                let rewritten = parsed.to_markdown();
                prop_assert_eq!(
                    MessageDocument::from_markdown_with(&rewritten, &w.directory),
                    Ok(parsed),
                    "{:?} was rewritten as {:?}", input, rewritten
                );
            }
        }
    }

    #[test]
    fn many_colons_are_parsed_in_linear_time() {
        let w = workspace();
        for input in [
            ":".repeat(MAX_TEXT_BYTES),
            ":a".repeat(MAX_TEXT_BYTES / 2),
            ":qz".repeat(MAX_TEXT_BYTES / 3),
            ":qz:".repeat(MAX_TEXT_BYTES / 4),
            format!(":{}:", "a".repeat(MAX_TEXT_BYTES)),
            ":a_".repeat(MAX_TEXT_BYTES / 3),
        ] {
            let started = Instant::now();
            let _ =
                MessageDocument::from_markdown_with(&input, &w.directory).map(|d| d.to_markdown());
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "{} bytes took {:?}",
                input.len(),
                started.elapsed()
            );
        }
    }

    /// Like the soaks above, with custom emoji in the alphabet. Opt in with
    /// `cargo test -p lazy-domain --release -- --ignored soak`.
    #[test]
    #[ignore = "long-running; run on demand"]
    fn soak_round_trip_with_emoji() {
        use proptest::test_runner::{Config, TestRunner};
        use std::cell::Cell;

        let w = workspace();
        let inexact = Cell::new(0_u32);
        let mut runner = TestRunner::new(Config {
            cases: 400_000,
            failure_persistence: None,
            ..Config::default()
        });
        runner
            .run(&emoji_heavy(), |input| {
                if let Ok(parsed) = MessageDocument::from_markdown_with(&input, &w.directory) {
                    let rewritten = parsed.to_markdown();
                    let again = MessageDocument::from_markdown_with(&rewritten, &w.directory);
                    if again.as_ref() != Ok(&parsed) {
                        inexact.set(inexact.get() + 1);
                        let words = again.as_ref().map(MessageDocument::plain_text);
                        prop_assert_eq!(
                            words,
                            Ok(parsed.plain_text()),
                            "{:?} -> {:?}",
                            input,
                            rewritten
                        );
                    }
                }
                Ok(())
            })
            .unwrap();
        eprintln!("inexact round trips: {}", inexact.get());
        assert!(inexact.get() <= 3, "{} inexact round trips", inexact.get());
    }
}
