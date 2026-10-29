//! Choosing the server (dogfood packaging): what an address may look like, when it may be
//! changed, what a check finds, and what moving to another server does.

mod common;

use common::{Driver, World};
use lazy_client_core::{
    Change, ClientCore, DEFAULT_SERVER_URL, Effect, Input, MAX_SERVER_URL_CHARS, RequestError,
    ServerCheck, ServerInfo, ServerSource, ServerUrlError, SessionPhase, StartupState,
    normalize_server_url,
};
use lazy_protocol::PROTOCOL_VERSION;
use proptest::prelude::*;
use url::Url;

// ----- The address -----

#[test]
fn what_is_typed_becomes_one_normal_address() {
    let table = [
        // A scheme is added: https, or http for a machine on the person's own network.
        ("chat.example.com", "https://chat.example.com"),
        ("  Chat.Example.COM/  ", "https://chat.example.com"),
        ("localhost", "http://localhost"),
        ("localhost:8080", "http://localhost:8080"),
        ("127.0.0.1:8080", "http://127.0.0.1:8080"),
        ("127.3.2.1", "http://127.3.2.1"),
        ("192.168.1.5", "http://192.168.1.5"),
        ("10.0.0.7:9000/lazy", "http://10.0.0.7:9000/lazy"),
        ("[::1]:8080", "http://[::1]:8080"),
        ("[::1]", "http://[::1]"),
        ("mac.local", "http://mac.local"),
        ("Mac.LOCAL:9000/x", "http://mac.local:9000/x"),
        // A scheme that was typed stays.
        ("https://localhost:8443", "https://localhost:8443"),
        ("https://192.168.1.5", "https://192.168.1.5"),
        ("HTTP://Example.com", "http://example.com"),
        ("http://chat.example.com", "http://chat.example.com"),
        // Slashes, default ports, and the host's case.
        ("https://chat.example.com/", "https://chat.example.com"),
        ("https://chat.example.com:443", "https://chat.example.com"),
        ("http://chat.example.com:80/", "http://chat.example.com"),
        (
            "https://chat.example.com:8443/",
            "https://chat.example.com:8443",
        ),
        ("https://CHAT.example.com:80", "https://chat.example.com:80"),
        // The port is the person's, whatever the scheme turns out to be.
        ("localhost:443", "http://localhost:443"),
        ("localhost:80", "http://localhost"),
        ("chat.example.com:80", "https://chat.example.com:80"),
        ("chat.example.com:443", "https://chat.example.com"),
        // A path prefix is allowed (a reverse proxy may serve Lazy under one).
        ("chat.example.com/lazy", "https://chat.example.com/lazy"),
        ("chat.example.com/lazy//", "https://chat.example.com/lazy"),
        ("https://x.test/a/b/", "https://x.test/a/b"),
        ("https://x.test/a/../b", "https://x.test/b"),
        // A name with letters outside ASCII is written the way DNS wants it.
        ("bücher.example", "https://xn--bcher-kva.example"),
    ];
    for (typed, expected) in table {
        assert_eq!(
            normalize_server_url(typed).as_deref(),
            Ok(expected),
            "{typed:?}"
        );
    }
}

#[test]
fn what_cannot_be_an_address_is_refused_with_the_reason() {
    let long = format!("https://{}.example.com", "a".repeat(MAX_SERVER_URL_CHARS));
    let table: [(&str, ServerUrlError); 22] = [
        ("", ServerUrlError::Empty),
        ("   \t ", ServerUrlError::Empty),
        (&long, ServerUrlError::TooLong),
        ("ftp://chat.example.com", ServerUrlError::Scheme),
        ("file:///etc/passwd", ServerUrlError::Scheme),
        ("ws://chat.example.com", ServerUrlError::Scheme),
        ("javascript:alert(1)", ServerUrlError::Invalid),
        ("https://user@chat.example.com", ServerUrlError::Credentials),
        ("user@chat.example.com", ServerUrlError::Credentials),
        (
            "https://user:hunter2@chat.example.com",
            ServerUrlError::Credentials,
        ),
        ("user:hunter2@chat.example.com", ServerUrlError::Credentials),
        (
            "https://:hunter2@chat.example.com",
            ServerUrlError::Credentials,
        ),
        (
            "https://chat.example.com?token=1",
            ServerUrlError::QueryOrFragment,
        ),
        ("chat.example.com/path?", ServerUrlError::QueryOrFragment),
        (
            "https://chat.example.com/#top",
            ServerUrlError::QueryOrFragment,
        ),
        ("chat.example.com#", ServerUrlError::QueryOrFragment),
        ("chat example.com", ServerUrlError::Invalid),
        ("chat.example.com\n", ServerUrlError::Empty),
        ("https://", ServerUrlError::Invalid),
        ("http://:80", ServerUrlError::Invalid),
        ("https://chat.example.com:99999", ServerUrlError::Invalid),
        ("chat\u{7}.example.com", ServerUrlError::Invalid),
    ];
    for (typed, expected) in table {
        let shown: String = typed.chars().take(40).collect();
        let got = normalize_server_url(typed);
        // A trailing newline is only space around the address, so that one is the exception.
        if shown == "chat.example.com\n" {
            assert_eq!(got.as_deref(), Ok("https://chat.example.com"));
            continue;
        }
        assert_eq!(got, Err(expected), "{shown:?}");
    }
    // What is said never repeats what was typed (it may hold a password).
    for error in [
        ServerUrlError::Empty,
        ServerUrlError::TooLong,
        ServerUrlError::Invalid,
        ServerUrlError::Scheme,
        ServerUrlError::Credentials,
        ServerUrlError::QueryOrFragment,
    ] {
        assert!(!error.message().is_empty());
    }
}

/// An address-like string built from parts that tend to matter.
fn address_like() -> impl Strategy<Value = String> {
    let scheme = prop::sample::select(vec![
        "", "http://", "https://", "HTTPS://", "ftp://", "ws://",
    ]);
    let userinfo = prop::sample::select(vec!["", "", "", "bob@", "bob:pw@", ":pw@"]);
    let host = prop::sample::select(vec![
        "chat.example.com",
        "Chat.Example.COM",
        "localhost",
        "LOCALHOST",
        "mac.local",
        "127.0.0.1",
        "192.168.0.12",
        "[::1]",
        "[2001:db8::1]",
        "bücher.example",
        "a",
        "",
        "exa mple.com",
        "-bad-",
    ]);
    let port = prop::sample::select(vec!["", "", ":80", ":443", ":8080", ":0", ":99999", ":x"]);
    let path = prop::sample::select(vec![
        "", "/", "//", "/lazy", "/lazy/", "/a/b/", "/a/../b", "/%20x", "/é", "/a b",
    ]);
    let tail = prop::sample::select(vec!["", "", "", "?", "?a=b", "#", "#frag"]);
    let space = prop::sample::select(vec!["", " ", "\t", "  "]);
    (
        space.clone(),
        scheme,
        userinfo,
        host,
        port,
        path,
        tail,
        space,
    )
        .prop_map(
            |(before, scheme, userinfo, host, port, path, tail, after)| {
                format!("{before}{scheme}{userinfo}{host}{port}{path}{tail}{after}")
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    /// Whatever is typed, an answer comes without a panic; an address that is accepted parses, is
    /// http or https, has no credentials, query, or fragment, no trailing slash, and normalises
    /// to itself.
    #[test]
    fn an_accepted_address_parses_is_plain_and_round_trips(typed in address_like()) {
        if let Ok(normal) = normalize_server_url(&typed) {
            let url = Url::parse(&normal).expect("the result parses");
            prop_assert!(matches!(url.scheme(), "http" | "https"), "{normal}");
            prop_assert_eq!(url.username(), "");
            prop_assert_eq!(url.password(), None);
            prop_assert_eq!(url.query(), None);
            prop_assert_eq!(url.fragment(), None);
            prop_assert!(url.host().is_some());
            prop_assert!(!normal.ends_with('/'), "{normal}");
            prop_assert!(normal.chars().count() <= 2 * MAX_SERVER_URL_CHARS);
            prop_assert_eq!(normalize_server_url(&normal), Ok(normal.clone()));
            // Spelling the scheme out changes nothing.
            if !typed.trim().contains("://") {
                let explicit = format!("{}://{}", url.scheme(), typed.trim());
                prop_assert_eq!(normalize_server_url(&explicit), Ok(normal));
            }
        }
    }

    /// An address with a user name, a password, a query, or a fragment is never accepted.
    #[test]
    fn credentials_queries_and_fragments_are_never_accepted(typed in address_like()) {
        let has_userinfo = typed.contains('@');
        let has_tail = typed.contains('?') || typed.contains('#');
        if has_userinfo || has_tail {
            prop_assert!(normalize_server_url(&typed).is_err(), "{typed:?}");
        }
    }

    /// Any text at all gets an answer.
    #[test]
    fn any_text_is_answered(typed in ".{0,400}") {
        let _ = normalize_server_url(&typed);
    }
}

// ----- The core -----

/// A core that has started signed out, on the server given.
fn signed_out(server: ServerInfo) -> Driver {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        server,
        ..Default::default()
    }));
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut { .. }
    ));
    driver
}

fn default_server() -> ServerInfo {
    ServerInfo::default()
}

fn saved(url: &str) -> ServerInfo {
    ServerInfo {
        url: url.to_owned(),
        source: ServerSource::Saved,
        trying: None,
        check: ServerCheck::Unchecked,
    }
}

fn locked(url: &str) -> ServerInfo {
    ServerInfo {
        source: ServerSource::Environment,
        ..saved(url)
    }
}

/// The only effect of a kind.
fn probes(effects: &[Effect]) -> Vec<(String, u64)> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::CheckServer { url, generation } => Some((url.clone(), *generation)),
            _ => None,
        })
        .collect()
}

fn moves(effects: &[Effect]) -> Vec<(String, u64)> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::UseServer { url, generation } => Some((url.clone(), *generation)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_new_core_uses_the_default_server_until_told_otherwise() {
    let core = ClientCore::new();
    assert_eq!(core.server().url, DEFAULT_SERVER_URL);
    assert_eq!(core.server().source, ServerSource::Default);
    assert_eq!(core.server().check, ServerCheck::Unchecked);
    assert!(!core.server().locked());
    let driver = signed_out(saved("https://chat.example.com"));
    assert_eq!(driver.core.server().url, "https://chat.example.com");
    assert_eq!(driver.core.server().source, ServerSource::Saved);
    assert!(
        driver
            .core
            .diagnostics()
            .report()
            .contains("server: saved, unchecked")
    );
}

#[test]
fn another_server_is_checked_first_and_only_then_moved_to() {
    let mut driver = signed_out(default_server());
    let out = driver.send(Input::SetServerUrl("  Chat.Example.com/lazy/ ".into()));
    let asked = probes(&out.effects);
    assert_eq!(asked.len(), 1, "{:?}", out.effects);
    assert_eq!(asked[0].0, "https://chat.example.com/lazy");
    assert!(
        moves(&out.effects).is_empty(),
        "nothing moves before the answer"
    );
    assert!(out.changes.contains(&Change::ServerChanged));
    let server = driver.core.server();
    assert_eq!(server.url, DEFAULT_SERVER_URL, "still the one in use");
    assert_eq!(
        server.trying.as_deref(),
        Some("https://chat.example.com/lazy")
    );
    assert_eq!(server.check, ServerCheck::Checking);

    // A server speaking this protocol: the core asks to move, and is still checking.
    let out = driver.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION),
    });
    assert_eq!(
        moves(&out.effects),
        vec![("https://chat.example.com/lazy".to_owned(), asked[0].1)]
    );
    assert_eq!(driver.core.server().url, DEFAULT_SERVER_URL);
    assert_eq!(driver.core.server().check, ServerCheck::Checking);

    // The runtime moved and loaded what that server's profile holds.
    let out = driver.send(Input::ServerSwitched {
        generation: asked[0].1,
        result: Ok(Box::default()),
    });
    assert!(out.changes.contains(&Change::ServerChanged));
    let server = driver.core.server();
    assert_eq!(server.url, "https://chat.example.com/lazy");
    assert_eq!(server.source, ServerSource::Saved);
    assert_eq!(server.trying, None);
    assert_eq!(
        server.check,
        ServerCheck::Reachable {
            protocol_version: PROTOCOL_VERSION
        }
    );
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut { last_error: None }
    ));
    assert!(
        driver
            .core
            .diagnostics()
            .report()
            .contains("server: saved, reachable")
    );
    // And signing in goes ahead.
    let out = driver.send(Input::SignIn {
        username: "someone".into(),
        password: "a password".into(),
    });
    assert!(
        out.effects.iter().any(|e| matches!(e, Effect::Login(_))),
        "{:?}",
        out.effects
    );
}

#[test]
fn signing_in_waits_for_an_address_that_was_typed_to_be_settled() {
    let mut driver = signed_out(default_server());
    driver.send(Input::SetServerUrl("chat.example.com".into()));
    for input in [
        Input::SignIn {
            username: "someone".into(),
            password: "a password".into(),
        },
        Input::Register {
            username: "someone".into(),
            display_name: "Someone".into(),
            password: "a password".into(),
            invite_code: String::new(),
        },
    ] {
        let out = driver.send(input);
        assert!(
            !out.effects
                .iter()
                .any(|e| matches!(e, Effect::Login(_) | Effect::Register(_))),
            "{:?}",
            out.effects
        );
        assert!(
            matches!(
                driver.core.phase(),
                SessionPhase::SignedOut {
                    last_error: Some(_)
                }
            ),
            "{:?}",
            driver.core.phase()
        );
    }
    // Going back to the server in use clears the way.
    driver.send(Input::SetServerUrl(DEFAULT_SERVER_URL.into()));
    assert_eq!(driver.core.server().trying, None);
    let out = driver.send(Input::SignIn {
        username: "someone".into(),
        password: "a password".into(),
    });
    assert!(out.effects.iter().any(|e| matches!(e, Effect::Login(_))));
}

#[test]
fn a_server_that_cannot_be_used_is_not_moved_to() {
    let mut driver = signed_out(saved("https://old.example.com"));
    let asked = |driver: &mut Driver, text: &str| {
        let out = driver.send(Input::SetServerUrl(text.into()));
        probes(&out.effects)[0].1
    };

    // Another protocol, newer or older.
    for server in [PROTOCOL_VERSION + 1, PROTOCOL_VERSION - 1] {
        let generation = asked(&mut driver, "new.example.com");
        let out = driver.send(Input::ServerChecked {
            generation,
            result: Ok(server),
        });
        assert!(moves(&out.effects).is_empty());
        assert_eq!(
            driver.core.server().check,
            ServerCheck::Incompatible {
                server,
                client: PROTOCOL_VERSION
            }
        );
        assert_eq!(driver.core.server().url, "https://old.example.com");
        assert_eq!(
            driver.core.server().trying.as_deref(),
            Some("https://new.example.com")
        );
    }

    // Nobody there.
    let generation = asked(&mut driver, "new.example.com");
    let out = driver.send(Input::ServerChecked {
        generation,
        result: Err("connection failed".into()),
    });
    assert!(moves(&out.effects).is_empty());
    assert_eq!(
        driver.core.server().check,
        ServerCheck::Unreachable("connection failed".into())
    );
    let recorded: Vec<_> = driver
        .core
        .diagnostics()
        .recent_errors
        .into_iter()
        .map(|e| (e.category, e.code))
        .collect();
    assert!(
        recorded.contains(&("server".into(), "incompatible".into())),
        "{recorded:?}"
    );
    assert!(
        recorded.contains(&("server".into(), "unreachable".into())),
        "{recorded:?}"
    );

    // A reason that is too long, or holds control characters, is trimmed.
    let generation = asked(&mut driver, "new.example.com");
    driver.send(Input::ServerChecked {
        generation,
        result: Err(format!("a\nb{}", "x".repeat(500))),
    });
    let ServerCheck::Unreachable(reason) = &driver.core.server().check else {
        panic!("{:?}", driver.core.server().check)
    };
    assert!(
        reason.chars().count() <= 80 && !reason.contains('\n'),
        "{reason:?}"
    );

    // Asking again asks the same address again; the answer can then be good.
    let out = driver.send(Input::CheckServer);
    let again = probes(&out.effects);
    assert_eq!(again[0].0, "https://new.example.com");
    let out = driver.send(Input::ServerChecked {
        generation: again[0].1,
        result: Ok(PROTOCOL_VERSION),
    });
    assert_eq!(moves(&out.effects).len(), 1);
}

#[test]
fn an_answer_to_an_older_question_is_ignored() {
    let mut driver = signed_out(default_server());
    let first = probes(
        &driver
            .send(Input::SetServerUrl("one.example.com".into()))
            .effects,
    )[0]
    .1;
    let second = probes(
        &driver
            .send(Input::SetServerUrl("two.example.com".into()))
            .effects,
    )[0]
    .1;
    assert_ne!(first, second);
    let out = driver.send(Input::ServerChecked {
        generation: first,
        result: Ok(PROTOCOL_VERSION),
    });
    assert!(out.effects.is_empty(), "{:?}", out.effects);
    assert_eq!(driver.core.server().check, ServerCheck::Checking);
    assert_eq!(
        driver.core.server().trying.as_deref(),
        Some("https://two.example.com")
    );
    let out = driver.send(Input::ServerChecked {
        generation: second,
        result: Ok(PROTOCOL_VERSION),
    });
    assert_eq!(moves(&out.effects)[0].0, "https://two.example.com");
    // A move that nobody asked for changes nothing.
    let mut other = signed_out(default_server());
    other.send(Input::ServerSwitched {
        generation: 99,
        result: Ok(Box::default()),
    });
    assert_eq!(other.core.server().url, DEFAULT_SERVER_URL);
}

#[test]
fn asking_twice_asks_once() {
    let mut driver = signed_out(default_server());
    let out = driver.send(Input::SetServerUrl("chat.example.com".into()));
    assert_eq!(probes(&out.effects).len(), 1);
    // The screen sends the address and then asks: the second is nothing new.
    let out = driver.send(Input::CheckServer);
    assert!(out.effects.is_empty(), "{:?}", out.effects);
    let out = driver.send(Input::SetServerUrl("https://chat.example.com/".into()));
    assert!(
        out.effects.is_empty(),
        "the same address, spelled another way"
    );
    // Asking the server in use is also one question.
    let mut driver = signed_out(default_server());
    assert_eq!(probes(&driver.send(Input::CheckServer).effects).len(), 1);
    assert!(driver.send(Input::CheckServer).effects.is_empty());
}

#[test]
fn the_server_in_use_can_be_checked_without_moving() {
    let mut driver = signed_out(saved("https://chat.example.com"));
    let out = driver.send(Input::SetServerUrl("https://chat.example.com".into()));
    let asked = probes(&out.effects);
    assert_eq!(asked[0].0, "https://chat.example.com");
    assert_eq!(driver.core.server().trying, None, "it is the one in use");
    let out = driver.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION),
    });
    assert!(moves(&out.effects).is_empty());
    assert_eq!(
        driver.core.server().check,
        ServerCheck::Reachable {
            protocol_version: PROTOCOL_VERSION
        }
    );
}

#[test]
fn while_the_runtime_is_moving_nothing_else_about_the_server_is_taken() {
    let mut driver = signed_out(default_server());
    let asked = probes(
        &driver
            .send(Input::SetServerUrl("chat.example.com".into()))
            .effects,
    );
    let out = driver.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION),
    });
    assert_eq!(moves(&out.effects).len(), 1, "the move is out");
    // The move cannot be taken back, and the core has to hear how it ended: another address,
    // an address that is not one, or asking again would all lose track of it.
    for input in [
        Input::SetServerUrl("other.example.com".into()),
        Input::SetServerUrl("not an address".into()),
        Input::CheckServer,
    ] {
        let out = driver.send(input);
        assert!(out.effects.is_empty(), "{:?}", out.effects);
        assert_eq!(
            driver.core.server().trying.as_deref(),
            Some("https://chat.example.com")
        );
        assert_eq!(driver.core.server().check, ServerCheck::Checking);
    }
    driver.send(Input::ServerSwitched {
        generation: asked[0].1,
        result: Ok(Box::default()),
    });
    assert_eq!(driver.core.server().url, "https://chat.example.com");
    assert_eq!(driver.core.server().trying, None);
}

#[test]
fn a_stored_session_is_loaded_again_when_its_server_answers() {
    let world = World::new();
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        server: saved("https://chat.example.com"),
        ..Default::default()
    }));
    driver.send(Input::BootstrapFailed(RequestError::Unknown(
        "connection failed".into(),
    )));
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::Unreachable { .. }
    ));
    let out = driver.send(Input::CheckServer);
    let asked = probes(&out.effects);
    let out = driver.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION),
    });
    assert_eq!(driver.core.phase(), &SessionPhase::Loading);
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::LoadBootstrap)),
        "{:?}",
        out.effects
    );
    let _ = world;

    // Another protocol does not retry.
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        ..Default::default()
    }));
    driver.send(Input::BootstrapFailed(RequestError::Unknown(
        "connection failed".into(),
    )));
    let asked = probes(&driver.send(Input::CheckServer).effects);
    driver.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION + 1),
    });
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::Unreachable { .. }
    ));
}

#[test]
fn the_server_can_only_be_changed_when_nobody_is_signed_in() {
    // Ready.
    let world = World::new();
    let mut driver = Driver::ready(&world);
    // Loading (before Started), and signing in.
    let mut loading = Driver::new();
    // Signing in.
    let mut signing_in = signed_out(default_server());
    signing_in.send(Input::SignIn {
        username: "someone".into(),
        password: "a password".into(),
    });
    assert_eq!(signing_in.core.phase(), &SessionPhase::SigningIn);

    for (what, driver) in [
        ("ready", &mut driver),
        ("loading", &mut loading),
        ("signing in", &mut signing_in),
    ] {
        let before = driver.core.server().clone();
        for input in [
            Input::SetServerUrl("chat.example.com".into()),
            Input::CheckServer,
        ] {
            let out = driver.send(input);
            assert!(out.effects.is_empty(), "{what}: {:?}", out.effects);
            assert!(driver.core.notice().is_some(), "{what}: says so");
            assert_eq!(driver.core.server(), &before, "{what}");
            driver.send(Input::DismissNotice);
        }
    }

    // An answer that arrives after sign-in began changes nothing either.
    let mut late = signed_out(default_server());
    let asked = probes(
        &late
            .send(Input::SetServerUrl("chat.example.com".into()))
            .effects,
    );
    // The person goes back to the server in use and signs in, then the old answer comes.
    late.send(Input::SetServerUrl(DEFAULT_SERVER_URL.into()));
    late.send(Input::SignIn {
        username: "someone".into(),
        password: "a password".into(),
    });
    assert_eq!(late.core.phase(), &SessionPhase::SigningIn);
    let out = late.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION),
    });
    assert!(moves(&out.effects).is_empty());
    assert_eq!(late.core.server().url, DEFAULT_SERVER_URL);
}

#[test]
fn a_check_that_finishes_after_the_phase_moved_on_does_not_move() {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        ..Default::default()
    }));
    driver.send(Input::BootstrapFailed(RequestError::Unknown(
        "connection failed".into(),
    )));
    let asked = probes(
        &driver
            .send(Input::SetServerUrl("chat.example.com".into()))
            .effects,
    );
    driver.send(Input::RetryLoad);
    assert_eq!(driver.core.phase(), &SessionPhase::Loading);
    let out = driver.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION),
    });
    assert!(moves(&out.effects).is_empty());
    assert_eq!(driver.core.server().url, DEFAULT_SERVER_URL);
    assert_eq!(driver.core.server().trying, None);
    assert_eq!(driver.core.server().check, ServerCheck::Unchecked);
}

#[test]
fn a_server_set_by_the_environment_cannot_be_changed_here() {
    let mut driver = signed_out(locked("https://env.example.com"));
    assert!(driver.core.server().locked());
    let out = driver.send(Input::SetServerUrl("chat.example.com".into()));
    assert!(out.effects.is_empty());
    assert!(
        driver
            .core
            .notice()
            .is_some_and(|n| n.contains("environment"))
    );
    assert_eq!(driver.core.server().url, "https://env.example.com");
    // It can still be looked at.
    let out = driver.send(Input::CheckServer);
    let asked = probes(&out.effects);
    assert_eq!(asked[0].0, "https://env.example.com");
    driver.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION + 2),
    });
    assert!(matches!(
        driver.core.server().check,
        ServerCheck::Incompatible { .. }
    ));
    assert_eq!(driver.core.server().source, ServerSource::Environment);
}

#[test]
fn an_address_that_cannot_be_one_leaves_the_server_alone_and_says_why() {
    let mut driver = signed_out(saved("https://chat.example.com"));
    let typed = "https://someone:hunter2-secret@elsewhere.example.com";
    let out = driver.send(Input::SetServerUrl(typed.into()));
    assert!(out.effects.is_empty());
    assert_eq!(driver.core.server().url, "https://chat.example.com");
    assert_eq!(driver.core.server().trying, None);
    let ServerCheck::Unreachable(why) = &driver.core.server().check else {
        panic!("{:?}", driver.core.server().check)
    };
    assert_eq!(why, ServerUrlError::Credentials.message());
    let shown = format!(
        "{:?} {}",
        driver.core.server(),
        driver.core.diagnostics().report()
    );
    assert!(
        !shown.contains("hunter2") && !shown.contains("someone"),
        "{shown}"
    );
    // It also stops an earlier check from deciding anything.
    let mut driver = signed_out(default_server());
    let asked = probes(
        &driver
            .send(Input::SetServerUrl("chat.example.com".into()))
            .effects,
    );
    driver.send(Input::SetServerUrl("not an address".into()));
    let out = driver.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION),
    });
    assert!(moves(&out.effects).is_empty());
    assert_eq!(driver.core.server().url, DEFAULT_SERVER_URL);
}

#[test]
fn unsent_messages_keep_the_person_on_their_server() {
    use lazy_client_core::JournalEntry;
    use lazy_domain::{ConversationId, MessageDocument, OperationId, Timestamp, UserId};
    let entry = JournalEntry {
        operation_id: OperationId::generate(),
        user_id: UserId::generate(),
        conversation_id: ConversationId::generate(),
        document: MessageDocument::from_plain_text("not sent yet").unwrap(),
        submitted_at: Timestamp::now(),
        kind: Default::default(),
        thread_root: None,
        also_in_channel: false,
        attachments: Vec::new(),
    };
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        journal: vec![entry],
        server: saved("https://chat.example.com"),
        ..Default::default()
    }));
    let out = driver.send(Input::SetServerUrl("other.example.com".into()));
    assert!(out.effects.is_empty(), "{:?}", out.effects);
    assert!(
        driver
            .core
            .notice()
            .is_some_and(|n| n.contains("haven't sent"))
    );
    assert_eq!(driver.core.server().trying, None);
    // The same server can still be asked.
    let out = driver.send(Input::SetServerUrl("https://chat.example.com".into()));
    assert_eq!(probes(&out.effects).len(), 1);
}

#[test]
fn a_move_that_could_not_be_saved_leaves_the_server_in_use() {
    let mut driver = signed_out(default_server());
    let asked = probes(
        &driver
            .send(Input::SetServerUrl("chat.example.com".into()))
            .effects,
    );
    driver.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION),
    });
    driver.send(Input::ServerSwitched {
        generation: asked[0].1,
        result: Err("the choice could not be saved".into()),
    });
    assert_eq!(driver.core.server().url, DEFAULT_SERVER_URL);
    assert_eq!(driver.core.server().source, ServerSource::Default);
    assert_eq!(
        driver.core.server().trying.as_deref(),
        Some("https://chat.example.com")
    );
    assert_eq!(
        driver.core.server().check,
        ServerCheck::Unreachable("the choice could not be saved".into())
    );
}

#[test]
fn what_the_old_server_taught_the_core_is_gone_after_a_move() {
    // A session that ended keeps the screen's data until someone signs out; a move must not.
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(Input::SessionExpired);
    assert!(matches!(
        driver.core.phase(),
        SessionPhase::SignedOut { .. }
    ));
    assert!(
        !driver.core.conversations().is_empty(),
        "the premise: it was kept"
    );
    assert!(driver.core.me().is_some());
    let asked = probes(
        &driver
            .send(Input::SetServerUrl("chat.example.com".into()))
            .effects,
    );
    driver.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION),
    });
    driver.send(Input::ServerSwitched {
        generation: asked[0].1,
        result: Ok(Box::default()),
    });
    assert!(driver.core.conversations().is_empty());
    assert!(driver.core.me().is_none());
    assert!(driver.core.workspaces().is_empty());
}

#[test]
fn moving_forgets_the_old_server_and_takes_up_the_new_ones_state() {
    let world = World::new();
    let conversation = world.channel.id;
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        drafts: [(conversation, "a draft for the old server".to_owned())].into(),
        recent_searches: vec!["old search".into()],
        ..Default::default()
    }));
    driver.send(Input::BootstrapFailed(RequestError::Unknown(
        "connection failed".into(),
    )));
    assert_eq!(
        driver.core.draft(conversation),
        "a draft for the old server"
    );
    let asked = probes(
        &driver
            .send(Input::SetServerUrl("chat.example.com".into()))
            .effects,
    );
    driver.send(Input::ServerChecked {
        generation: asked[0].1,
        result: Ok(PROTOCOL_VERSION),
    });
    // The other server's profile held a session: it loads at once.
    let out = driver.send(Input::ServerSwitched {
        generation: asked[0].1,
        result: Ok(Box::new(StartupState {
            session_restored: true,
            ..Default::default()
        })),
    });
    assert_eq!(
        driver.core.draft(conversation),
        "",
        "the old server's draft is gone"
    );
    assert!(driver.core.recent_searches().is_empty());
    assert_eq!(driver.core.phase(), &SessionPhase::Loading);
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::LoadBootstrap)),
        "{:?}",
        out.effects
    );
    assert_eq!(driver.core.server().url, "https://chat.example.com");
}
