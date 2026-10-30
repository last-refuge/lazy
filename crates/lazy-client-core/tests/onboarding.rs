//! Joining for the first time (D29): what a server says about taking accounts, and what the
//! invitation code in the field is for. Neither creates or uses anything.

mod common;

use common::{Driver, World};
use lazy_client_core::{
    Change, Effect, Input, InvitationCheck, PreviewFailure, ServerCheck, ServerFacts, ServerInfo,
    ServerSource, StartupState,
};
use lazy_domain::DisplayName;
use lazy_protocol::{InvitationKind, InvitationPreview, PROTOCOL_VERSION, ServerRegistration};

const SERVER: &str = "https://chat.example.com";
const CANONICAL: &str = "K7QM4TXD9BWPH2VAZX3C";

fn signed_out(url: &str) -> Driver {
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        server: ServerInfo {
            url: url.to_owned(),
            source: ServerSource::Saved,
            trying: None,
            check: ServerCheck::Reachable {
                protocol_version: PROTOCOL_VERSION,
            },
        },
        ..Default::default()
    }));
    driver
}

fn facts(registration: ServerRegistration, setup_code_required: bool) -> ServerFacts {
    ServerFacts {
        registration,
        setup_code_required,
    }
}

fn preview(kind: InvitationKind) -> InvitationPreview {
    InvitationPreview {
        kind,
        workspace_name: "Zebrafish Collective".into(),
        inviter: Some(DisplayName::parse("Alice Example").unwrap()),
        member_count: 12,
    }
}

/// The only preview effect an output holds: the code it asks about and the question's number.
fn asked(effects: &[Effect]) -> Option<(String, u64)> {
    effects.iter().find_map(|effect| match effect {
        Effect::PreviewInvitation {
            request,
            generation,
        } => Some((request.code.expose().to_owned(), *generation)),
        _ => None,
    })
}

// ----- What the server says about taking accounts -----

#[test]
fn what_a_check_hears_is_kept_for_the_address_it_came_from_through_the_move() {
    let mut driver = signed_out("http://127.0.0.1:8080");
    assert_eq!(driver.core.server_facts(), None);

    let out = driver.send(Input::SetServerUrl("chat.example.com".into()));
    let generation = out
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::CheckServer { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the address is checked");
    // Heard just before the check's own answer, for the address being tried.
    let heard = facts(ServerRegistration::InviteOnly, false);
    let out = driver.send(Input::ServerFacts {
        url: SERVER.into(),
        facts: Some(heard),
    });
    assert!(out.changes.contains(&Change::ServerChanged));
    assert_eq!(driver.core.server_facts(), Some(heard));

    driver.send(Input::ServerChecked {
        generation,
        result: Ok(PROTOCOL_VERSION),
    });
    driver.send(Input::ServerSwitched {
        generation,
        result: Ok(Box::default()),
    });
    assert_eq!(driver.core.server().url, SERVER);
    assert_eq!(
        driver.core.server_facts(),
        Some(heard),
        "the move to the address does not forget what it said"
    );

    // What another address said is not this one's.
    driver.send(Input::ServerFacts {
        url: "https://elsewhere.example".into(),
        facts: Some(facts(ServerRegistration::Open, false)),
    });
    assert_eq!(driver.core.server_facts(), Some(heard));
    // A server that says nothing (an older one) is forgotten.
    let out = driver.send(Input::ServerFacts {
        url: SERVER.into(),
        facts: None,
    });
    assert!(out.changes.contains(&Change::ServerChanged));
    assert_eq!(driver.core.server_facts(), None);
    // Hearing the same thing again changes nothing.
    driver.send(Input::ServerFacts {
        url: SERVER.into(),
        facts: Some(heard),
    });
    let out = driver.send(Input::ServerFacts {
        url: SERVER.into(),
        facts: Some(heard),
    });
    assert!(out.changes.is_empty());
}

// ----- What a code is for -----

#[test]
fn a_code_is_asked_about_in_its_plain_form_and_the_answer_is_kept() {
    let mut driver = signed_out(SERVER);
    assert_eq!(driver.core.invitation_check(), &InvitationCheck::Idle);

    // Pasted the way it was shown: grouped, in lower case.
    let out = driver.send(Input::PreviewInvitation {
        code: " k7qm-4txd-9bwp-h2va-zx3c\n".into(),
    });
    let (code, generation) = asked(&out.effects).expect("the server is asked");
    assert_eq!(code, CANONICAL);
    assert_eq!(driver.core.invitation_check(), &InvitationCheck::Checking);
    assert!(out.changes.contains(&Change::InvitationChanged));

    let out = driver.send(Input::InvitationPreviewed {
        generation,
        result: Ok(preview(InvitationKind::Invitation)),
    });
    assert!(out.changes.contains(&Change::InvitationChanged));
    assert_eq!(
        driver.core.invitation_check(),
        &InvitationCheck::Valid(preview(InvitationKind::Invitation))
    );
    // Neither the code nor the names are printed.
    let printed = format!("{:?}", driver.core.invitation_check());
    assert!(
        !printed.contains("Zebrafish") && !printed.contains("Alice"),
        "{printed}"
    );
}

#[test]
fn only_the_newest_question_is_answered() {
    let mut driver = signed_out(SERVER);
    let (_, first) = asked(
        &driver
            .send(Input::PreviewInvitation {
                code: CANONICAL.into(),
            })
            .effects,
    )
    .unwrap();
    let (_, second) = asked(
        &driver
            .send(Input::PreviewInvitation {
                code: "ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ".into(),
            })
            .effects,
    )
    .unwrap();
    assert!(second > first);
    let out = driver.send(Input::InvitationPreviewed {
        generation: first,
        result: Ok(preview(InvitationKind::Invitation)),
    });
    assert!(out.changes.is_empty(), "the code has been changed since");
    assert_eq!(driver.core.invitation_check(), &InvitationCheck::Checking);
    driver.send(Input::InvitationPreviewed {
        generation: second,
        result: Err(PreviewFailure::Invalid),
    });
    assert_eq!(driver.core.invitation_check(), &InvitationCheck::Invalid);
    // An answer with nothing waiting for it is ignored.
    let out = driver.send(Input::InvitationPreviewed {
        generation: second,
        result: Ok(preview(InvitationKind::Invitation)),
    });
    assert!(out.changes.is_empty());
    assert_eq!(driver.core.invitation_check(), &InvitationCheck::Invalid);
}

#[test]
fn each_way_the_server_can_fail_to_say_is_its_own_state() {
    for (failure, expected) in [
        (PreviewFailure::Invalid, InvitationCheck::Invalid),
        (PreviewFailure::Closed, InvitationCheck::Closed),
        (PreviewFailure::Unsupported, InvitationCheck::Unchecked),
        (
            PreviewFailure::Failed("connection failed".into()),
            InvitationCheck::Failed("connection failed".into()),
        ),
    ] {
        let mut driver = signed_out(SERVER);
        let (_, generation) = asked(
            &driver
                .send(Input::PreviewInvitation {
                    code: CANONICAL.into(),
                })
                .effects,
        )
        .unwrap();
        driver.send(Input::InvitationPreviewed {
            generation,
            result: Err(failure),
        });
        assert_eq!(driver.core.invitation_check(), &expected);
    }
    // A reason is kept short and plain.
    let mut driver = signed_out(SERVER);
    let (_, generation) = asked(
        &driver
            .send(Input::PreviewInvitation {
                code: CANONICAL.into(),
            })
            .effects,
    )
    .unwrap();
    driver.send(Input::InvitationPreviewed {
        generation,
        result: Err(PreviewFailure::Failed(format!("{}\u{7}", "x".repeat(200)))),
    });
    let InvitationCheck::Failed(reason) = driver.core.invitation_check() else {
        panic!("{:?}", driver.core.invitation_check());
    };
    assert_eq!(reason.chars().count(), 80);
}

#[test]
fn a_code_that_cannot_be_one_is_answered_without_asking_and_blank_forgets() {
    let mut driver = signed_out(SERVER);
    for bad in [
        "short",
        "has space in it but is long enough",
        &"a".repeat(65),
    ] {
        let out = driver.send(Input::PreviewInvitation { code: bad.into() });
        assert!(out.effects.is_empty(), "{bad:?}");
        assert_eq!(driver.core.invitation_check(), &InvitationCheck::Invalid);
    }
    let out = driver.send(Input::PreviewInvitation { code: "  ".into() });
    assert!(out.effects.is_empty());
    assert_eq!(driver.core.invitation_check(), &InvitationCheck::Idle);
    // A code written any way that is still the new kind is asked about.
    let out = driver.send(Input::PreviewInvitation {
        code: "OOOO-IIII-LLLL-0000-1111".into(),
    });
    assert_eq!(
        asked(&out.effects).map(|(code, _)| code).as_deref(),
        Some("00001111111100001111")
    );
}

#[test]
fn nothing_is_asked_of_a_server_that_is_still_being_tried_or_after_signing_in() {
    // An address being tried is not the server in use; there is nobody to ask yet.
    let mut driver = signed_out("http://127.0.0.1:8080");
    driver.send(Input::SetServerUrl("chat.example.com".into()));
    assert!(driver.core.server().trying.is_some());
    let out = driver.send(Input::PreviewInvitation {
        code: CANONICAL.into(),
    });
    assert!(out.effects.is_empty());
    assert_eq!(driver.core.invitation_check(), &InvitationCheck::Idle);

    // Signed in: this is not a screen anyone is on.
    let mut ready = Driver::ready(&World::new());
    let out = ready.send(Input::PreviewInvitation {
        code: CANONICAL.into(),
    });
    assert!(out.effects.is_empty() && out.changes.is_empty());
}

// ----- How a code is sent -----

#[test]
fn registering_and_accepting_send_the_code_in_its_plain_form() {
    let mut driver = signed_out(SERVER);
    let out = driver.send(Input::Register {
        username: "newcomer".into(),
        display_name: "Newcomer".into(),
        password: "long enough".into(),
        invite_code: "k7qm 4txd 9bwp h2va zx3c".into(),
    });
    let [Effect::Register(request)] = &out.effects[..] else {
        panic!("{:?}", out.effects);
    };
    assert_eq!(request.invite_code.as_ref().unwrap().expose(), CANONICAL);

    let mut ready = Driver::ready(&World::new());
    let out = ready.send(Input::AcceptInvitation {
        code: "K7QM-4TXD-9BWP-H2VA-ZX3C".into(),
    });
    let sent = out.effects.iter().find_map(|effect| match effect {
        Effect::AcceptInvitation { request } => Some(request.code.expose().to_owned()),
        _ => None,
    });
    assert_eq!(sent.as_deref(), Some(CANONICAL));
}

#[test]
fn what_is_typed_never_reaches_a_printed_input_or_effect() {
    let typed = "k7qm-4txd-9bwp-h2va-zx3c";
    let input = Input::PreviewInvitation { code: typed.into() };
    assert!(!format!("{input:?}").contains("k7qm"));
    let mut driver = signed_out(SERVER);
    let out = driver.send(input);
    for effect in &out.effects {
        let printed = format!("{effect:?}");
        assert!(
            !printed.contains("K7QM") && !printed.contains("k7qm"),
            "{printed}"
        );
    }
    let answered = Input::InvitationPreviewed {
        generation: 1,
        result: Ok(preview(InvitationKind::Invitation)),
    };
    let printed = format!("{answered:?}");
    assert!(
        !printed.contains("Zebrafish") && !printed.contains("Alice"),
        "{printed}"
    );
}
