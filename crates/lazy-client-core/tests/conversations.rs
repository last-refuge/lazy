//! Conversations in the sidebar: kinds, titles, sections, what to offer, and the events that
//! change what the person belongs to.

mod common;

use common::{Driver, World, channel, people, read_state, seq, user};
use lazy_client_core::{ActionKind, Change, Effect, Input, SessionPhase};
use lazy_domain::{ChannelVisibility, ConversationId, WorkspaceRole};
use lazy_protocol::{BrowsedChannel, Conversation, Event};

/// A workspace with a default channel, #random, #alpha, direct messages with zoe and kai, and a
/// group of the three. `jules` is the signed-in person.
struct Fixture {
    world: World,
    kai: lazy_protocol::User,
    alpha: Conversation,
    dm_zoe: Conversation,
    dm_kai: Conversation,
    group: Conversation,
}

fn fixture() -> Fixture {
    let mut world = World::new();
    let kai = user("kai");
    let alpha = channel(world.workspace.id, "alpha");
    let dm_zoe = people(world.workspace.id, &[&world.me, &world.other]);
    let dm_kai = people(world.workspace.id, &[&world.me, &kai]);
    let group = people(world.workspace.id, &[&world.me, &world.other, &kai]);
    world.extra = vec![alpha.clone(), dm_zoe.clone(), dm_kai.clone(), group.clone()];
    world.extra_users = vec![kai.clone()];
    Fixture {
        world,
        kai,
        alpha,
        dm_zoe,
        dm_kai,
        group,
    }
}

fn titles(driver: &Driver, ids: &[ConversationId]) -> Vec<String> {
    ids.iter()
        .map(|id| driver.core.conversation_title(*id))
        .collect()
}

// ----- Titles and sections -----

#[test]
fn titles_name_a_channel_or_the_other_people() {
    let f = fixture();
    let driver = Driver::ready(&f.world);
    assert_eq!(driver.core.conversation_title(f.alpha.id), "alpha");
    assert_eq!(driver.core.conversation_title(f.dm_zoe.id), "ZOE", "not me");
    assert_eq!(driver.core.conversation_title(f.dm_kai.id), "KAI");
    // Alphabetical, whatever order the server listed the participants in.
    assert_eq!(driver.core.conversation_title(f.group.id), "KAI, ZOE");
    assert_eq!(
        driver.core.conversation_title(ConversationId::generate()),
        ""
    );
}

#[test]
fn a_participant_the_client_has_not_heard_of_gets_a_placeholder() {
    let mut f = fixture();
    let stranger = user("nobody");
    let dm = people(f.world.workspace.id, &[&f.world.me, &stranger]);
    f.world.extra.push(dm.clone());
    let driver = Driver::ready(&f.world);
    assert_eq!(
        driver.core.conversation_title(dm.id),
        lazy_client_core::UNKNOWN_MEMBER_NAME
    );
}

#[test]
fn the_sidebar_has_favorites_then_channels_by_name_then_people_by_recent_activity() {
    let mut f = fixture();
    // The group and #random are favorites; the group is not also listed with the people.
    f.world.user_states = vec![
        {
            let mut s = read_state(f.group.id, 0, 0);
            s.favorite = true;
            s
        },
        {
            let mut s = read_state(f.world.second_channel.id, 0, 0);
            s.favorite = true;
            s
        },
    ];
    let mut driver = Driver::ready(&f.world);

    // Kai wrote most recently, so his conversation comes first.
    driver.send(f.world.event(
        f.world.message(f.dm_zoe.id, &f.world.other, 20, "earlier"),
        None,
    ));
    driver.send(
        f.world
            .event(f.world.message(f.dm_kai.id, &f.kai, 30, "later"), None),
    );

    let sidebar = driver.core.sidebar();
    assert_eq!(titles(&driver, &sidebar.favorites), ["KAI, ZOE", "random"]);
    assert_eq!(titles(&driver, &sidebar.channels), ["alpha", "general"]);
    assert_eq!(sidebar.direct_and_group, vec![f.dm_kai.id, f.dm_zoe.id]);

    // Each conversation appears once, and the convenience accessors agree.
    let total = sidebar.favorites.len() + sidebar.channels.len() + sidebar.direct_and_group.len();
    assert_eq!(total, driver.core.conversations().len());
    assert_eq!(driver.core.favorites(), sidebar.favorites);
    assert_eq!(driver.core.channels(), sidebar.channels);
    assert_eq!(driver.core.direct_and_group(), sidebar.direct_and_group);
}

#[test]
fn people_are_ordered_by_the_newest_message_even_when_history_is_loaded_later() {
    let f = fixture();
    let mut driver = Driver::ready(&f.world);
    driver.send(Input::SelectConversation(f.dm_zoe.id));
    driver.load_history(
        f.dm_zoe.id,
        vec![
            f.world
                .message(f.dm_zoe.id, &f.world.other, 50, "old but newest known"),
        ],
        false,
    );
    assert_eq!(driver.core.direct_and_group()[0], f.dm_zoe.id);
    driver.send(
        f.world
            .event(f.world.message(f.dm_kai.id, &f.kai, 60, "now"), None),
    );
    assert_eq!(driver.core.direct_and_group()[0], f.dm_kai.id);
}

#[test]
fn the_default_channel_opens_first() {
    let f = fixture();
    let driver = Driver::ready(&f.world);
    assert_eq!(
        driver.core.selected_conversation(),
        Some(f.world.channel.id)
    );
}

#[test]
fn members_lists_everyone_known_in_a_stable_order() {
    let f = fixture();
    let mut driver = Driver::ready(&f.world);
    let names: Vec<_> = driver
        .core
        .members()
        .iter()
        .map(|u| u.display_name.as_str().to_owned())
        .collect();
    assert_eq!(names, ["JULES", "KAI", "ZOE"]);
    driver.send(Input::Frame(lazy_protocol::ServerFrame::Event(
        lazy_protocol::EventEnvelope {
            workspace_id: f.world.workspace.id,
            seq: seq(11),
            occurred_at: lazy_domain::Timestamp::now(),
            event: Event::MemberJoined { user: user("amy") },
        },
    )));
    assert_eq!(driver.core.members()[0].display_name.as_str(), "AMY");
}

// ----- What to offer -----

fn offered(driver: &Driver, id: ConversationId) -> [bool; 7] {
    let core = &driver.core;
    [
        core.can_leave(id),
        core.can_add_members(id),
        core.can_remove_members(id),
        core.can_rename(id),
        core.can_edit_details(id),
        core.can_archive(id),
        core.can_unarchive(id),
    ]
}

#[test]
fn what_to_offer_follows_the_role_and_whether_the_person_created_the_channel() {
    let mut f = fixture();
    let (me, workspace) = (f.world.me.id, f.world.workspace.id);
    let mine = Conversation {
        created_by: Some(me),
        ..channel(workspace, "mine")
    };
    let theirs = Conversation {
        created_by: Some(f.world.other.id),
        ..channel(workspace, "theirs")
    };
    f.world.extra.extend([mine.clone(), theirs.clone()]);

    // [leave, add members, remove members, rename, edit details, archive, unarchive]
    let member = Driver::ready(&f.world);
    assert_eq!(
        offered(&member, mine.id),
        [true, true, true, true, true, true, false],
        "creator"
    );
    assert_eq!(
        offered(&member, theirs.id),
        [true, true, false, false, true, false, false],
        "plain member"
    );
    assert_eq!(
        offered(&member, f.world.channel.id),
        [false, true, false, false, true, false, false],
        "default channel"
    );
    assert_eq!(
        offered(&member, f.dm_zoe.id),
        [false; 7],
        "people conversations are fixed"
    );
    assert_eq!(offered(&member, f.group.id), [false; 7]);

    f.world.role = WorkspaceRole::Admin;
    let admin = Driver::ready(&f.world);
    assert_eq!(
        offered(&admin, theirs.id),
        [true, true, true, true, true, true, false],
        "admin"
    );
    assert_eq!(
        offered(&admin, f.world.channel.id),
        [false, true, false, true, true, false, false],
        "renamed but never archived"
    );
}

#[test]
fn archived_channels_offer_only_unarchiving() {
    let mut f = fixture();
    let archived = Conversation {
        archived: true,
        created_by: Some(f.world.me.id),
        ..channel(f.world.workspace.id, "old")
    };
    f.world.extra.push(archived.clone());
    let driver = Driver::ready(&f.world);
    assert_eq!(
        offered(&driver, archived.id),
        [true, false, true, false, false, false, true]
    );
}

#[test]
fn only_public_unjoined_channels_offer_joining() {
    let f = fixture();
    let driver = Driver::ready(&f.world);
    let public = channel(f.world.workspace.id, "open");
    let private = Conversation {
        visibility: ChannelVisibility::Private,
        ..channel(f.world.workspace.id, "secret")
    };
    let entry = |conversation: &Conversation, joined| BrowsedChannel {
        conversation: conversation.clone(),
        joined,
    };
    assert!(driver.core.can_join(&entry(&public, false)));
    assert!(!driver.core.can_join(&entry(&public, true)), "already in");
    assert!(
        !driver.core.can_join(&entry(&private, false)),
        "needs an invitation"
    );
}

// ----- Membership events -----

#[test]
fn being_added_to_a_channel_shows_it_and_fetches_its_newest_page() {
    let f = fixture();
    let mut driver = Driver::ready(&f.world);
    let ideas = channel(f.world.workspace.id, "ideas");

    // Messages from a channel the client does not know yet are ignored...
    driver.send(f.world.event(
        f.world.message(ideas.id, &f.world.other, 11, "before"),
        None,
    ));
    assert!(driver.core.timeline(ideas.id).is_empty());
    assert!(driver.core.conversation(ideas.id).is_none());

    // ...and the event that adds the person brings the channel in, without taking over.
    let before = driver.history_requests().len();
    driver.send(f.world.event_at(
        12,
        Event::MemberAdded {
            conversation: ideas.clone(),
            user_id: f.world.me.id,
        },
    ));
    assert!(driver.core.conversation(ideas.id).is_some());
    assert!(driver.core.channels().contains(&ideas.id));
    assert_eq!(
        driver.core.selected_conversation(),
        Some(f.world.channel.id)
    );
    assert_eq!(
        driver.history_requests()[before..],
        [ideas.id],
        "its newest page is fetched"
    );

    // Everything before the join counts as read; what follows counts.
    let state = driver.core.user_state(ideas.id).unwrap();
    assert_eq!((state.last_read, state.unread_count), (seq(12), 0));
    driver.send(
        f.world
            .event(f.world.message(ideas.id, &f.world.other, 13, "after"), None),
    );
    assert_eq!(driver.core.user_state(ideas.id).unwrap().unread_count, 1);
}

#[test]
fn someone_else_joining_updates_the_member_count() {
    let f = fixture();
    let mut driver = Driver::ready(&f.world);
    let grown = Conversation {
        member_count: 7,
        ..f.alpha.clone()
    };
    driver.send(f.world.event_at(
        11,
        Event::MemberAdded {
            conversation: grown,
            user_id: f.kai.id,
        },
    ));
    assert_eq!(
        driver.core.conversation(f.alpha.id).unwrap().member_count,
        7
    );
    driver.send(f.world.event_at(
        12,
        Event::MemberRemoved {
            conversation_id: f.alpha.id,
            user_id: f.kai.id,
        },
    ));
    assert_eq!(
        driver.core.conversation(f.alpha.id).unwrap().member_count,
        6
    );
    assert!(
        driver.core.conversation(f.alpha.id).is_some(),
        "still a member"
    );
}

#[test]
fn a_conversation_someone_opens_with_me_appears_and_counts_what_follows() {
    let f = fixture();
    let mut driver = Driver::ready(&f.world);
    let new_dm = people(f.world.workspace.id, &[&f.world.me, &f.kai]);
    driver.send(f.world.event_at(
        11,
        Event::ConversationCreated {
            conversation: new_dm.clone(),
        },
    ));
    assert!(driver.core.direct_and_group().contains(&new_dm.id));
    assert_eq!(driver.core.conversation_title(new_dm.id), "KAI");
    driver.send(
        f.world
            .event(f.world.message(new_dm.id, &f.kai, 12, "hi"), None),
    );
    assert_eq!(driver.core.user_state(new_dm.id).unwrap().unread_count, 1);
}

#[test]
fn updates_replace_what_is_known_and_unknown_conversations_stay_unknown() {
    let f = fixture();
    let mut driver = Driver::ready(&f.world);
    let renamed = Conversation {
        name: Some(lazy_domain::ChannelName::parse("omega").unwrap()),
        topic: Some("new topic".into()),
        archived: true,
        ..f.alpha.clone()
    };
    let out = driver.send(f.world.event_at(
        11,
        Event::ConversationUpdated {
            conversation: renamed,
        },
    ));
    assert_eq!(driver.core.conversation_title(f.alpha.id), "omega");
    assert!(driver.core.conversation(f.alpha.id).unwrap().archived);
    assert!(out.changes.contains(&Change::ConversationStateChanged {
        conversation: f.alpha.id
    }));

    let unknown = channel(f.world.workspace.id, "elsewhere");
    driver.send(f.world.event_at(
        12,
        Event::ConversationUpdated {
            conversation: unknown.clone(),
        },
    ));
    assert!(driver.core.conversation(unknown.id).is_none());
}

#[test]
fn removal_while_the_conversation_is_open_leaves_nothing_behind() {
    let f = fixture();
    let team = f.alpha.clone();
    let mut driver = Driver::ready(&f.world);
    driver.send(Input::SelectConversation(team.id));
    driver.load_history(
        team.id,
        vec![f.world.message(team.id, &f.world.other, 4, "hello")],
        false,
    );
    // A message still on its way to the channel, and a draft of the next one.
    let out = driver.send(Input::SendMessage {
        conversation: team.id,
        text: "sending".into(),
    });
    let Some(Effect::PersistSubmission(entry)) = out.effects.first().cloned() else {
        panic!("expected a journal write");
    };
    driver.send(Input::SetDraft {
        conversation: team.id,
        text: "half a thought".into(),
    });

    let out = driver.send(f.world.event_at(
        11,
        Event::MemberRemoved {
            conversation_id: team.id,
            user_id: f.world.me.id,
        },
    ));

    assert!(driver.core.conversation(team.id).is_none());
    assert!(driver.core.user_state(team.id).is_none());
    assert!(driver.core.timeline(team.id).is_empty());
    assert_eq!(driver.core.draft(team.id), "");
    assert_eq!(driver.core.unread_marker(team.id), None);
    assert_eq!(
        driver.core.selected_conversation(),
        Some(f.world.channel.id)
    );
    assert_eq!(driver.core.notice(), Some("You were removed from #alpha"));
    assert!(out.changes.contains(&Change::TimelineReset {
        conversation: team.id
    }));
    assert!(out.changes.contains(&Change::SelectionChanged));
    assert!(out.changes.contains(&Change::DirectoryChanged));
    // The doomed message's journal entry is released rather than left to retry forever.
    assert!(out.effects.contains(&Effect::SettleSubmission {
        operation: entry.operation_id
    }));
    assert_eq!(driver.core.unsettled_operations(), 0);
    // The draft is gone from disk too.
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::PersistDrafts(d) if d.is_empty()))
    );
    // And so is the conversation from the cache.
    let cache = driver.last_cache().unwrap();
    assert!(
        cache.workspaces[0]
            .conversations
            .iter()
            .all(|c| c.id != team.id)
    );
    assert!(
        cache.workspaces[0]
            .user_state
            .iter()
            .all(|s| s.state.conversation_id != team.id)
    );
    assert!(
        cache.workspaces[0]
            .timelines
            .iter()
            .all(|t| t.conversation != team.id)
    );
    // Later results for it are dropped.
    driver.send(
        f.world
            .event(f.world.message(team.id, &f.world.other, 12, "ghost"), None),
    );
    assert!(driver.core.timeline(team.id).is_empty());
}

#[test]
fn leaving_myself_is_not_announced_as_a_removal() {
    for event_first in [true, false] {
        let f = fixture();
        let team = f.alpha.clone();
        let mut driver = Driver::ready(&f.world);
        let out = driver.send(Input::LeaveChannel(team.id));
        assert!(out.effects.contains(&Effect::LeaveChannel {
            conversation: team.id
        }));
        assert_eq!(
            *driver.core.action(ActionKind::Leave(team.id)),
            lazy_client_core::ActionState::InFlight
        );

        let removal = f.world.event_at(
            11,
            Event::MemberRemoved {
                conversation_id: team.id,
                user_id: f.world.me.id,
            },
        );
        let response = Input::ActionResult {
            kind: ActionKind::Leave(team.id),
            result: Ok(lazy_client_core::ActionOutcome::Done),
        };
        let (first, second) = if event_first {
            (removal, response)
        } else {
            (response, removal)
        };
        driver.send(first);
        driver.send(second);
        assert!(
            driver.core.conversation(team.id).is_none(),
            "event_first = {event_first}"
        );
        assert!(
            driver.core.notice().is_none(),
            "no surprise: event_first = {event_first}"
        );
        assert_eq!(
            *driver.core.action(ActionKind::Leave(team.id)),
            lazy_client_core::ActionState::Idle
        );
    }
}

#[test]
fn losing_the_only_other_conversations_still_leaves_a_valid_selection() {
    let f = fixture();
    let mut driver = Driver::ready(&f.world);
    for (at, id) in [
        f.world.second_channel.id,
        f.alpha.id,
        f.dm_zoe.id,
        f.dm_kai.id,
        f.group.id,
        f.world.channel.id,
    ]
    .into_iter()
    .enumerate()
    {
        driver.send(f.world.event_at(
            11 + at as i64,
            Event::MemberRemoved {
                conversation_id: id,
                user_id: f.world.me.id,
            },
        ));
    }
    assert!(driver.core.conversations().is_empty());
    assert_eq!(driver.core.selected_conversation(), None);
    assert_eq!(*driver.core.phase(), SessionPhase::Ready);
}

// ----- Browsing -----

#[test]
fn browsing_lists_public_channels_and_tracks_whether_they_are_joined() {
    let f = fixture();
    let mut driver = Driver::ready(&f.world);
    let open = channel(f.world.workspace.id, "open");
    let listing = vec![
        BrowsedChannel {
            conversation: f.alpha.clone(),
            joined: true,
        },
        BrowsedChannel {
            conversation: open.clone(),
            joined: false,
        },
    ];
    driver.send(Input::BrowseChannels);
    let out = driver.send(Input::ActionResult {
        kind: ActionKind::Browse,
        result: Ok(lazy_client_core::ActionOutcome::Browse(listing)),
    });
    assert!(out.changes.contains(&Change::BrowseChanged));
    assert_eq!(driver.core.browse_results().len(), 2);
    assert!(driver.core.can_join(&driver.core.browse_results()[1]));

    // Joining elsewhere (another device) flips the entry.
    driver.send(f.world.event_at(
        11,
        Event::MemberAdded {
            conversation: open.clone(),
            user_id: f.world.me.id,
        },
    ));
    assert!(driver.core.browse_results()[1].joined);
    assert!(!driver.core.can_join(&driver.core.browse_results()[1]));

    // And being removed flips it back.
    driver.send(f.world.event_at(
        12,
        Event::MemberRemoved {
            conversation_id: open.id,
            user_id: f.world.me.id,
        },
    ));
    assert!(!driver.core.browse_results()[1].joined);
}
