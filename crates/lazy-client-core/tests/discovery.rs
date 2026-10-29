//! Discovery (slice 3E, D21): saved messages, the people directory and profiles, the channel
//! directory, the quick switcher, and the recently visited conversations.

mod common;

use common::{Driver, World, channel, people, seq, user};
use lazy_client_core::{
    Change, Effect, Input, LoadStatus, MAX_RECENT_CONVERSATIONS, Output, QUICK_SWITCHER_LIMIT,
    QuickSwitcherTarget, RequestError, SAVED_PAGE, SessionPhase, StartupState,
};
use lazy_domain::notification::UserStatus;
use lazy_domain::{ConversationId, MessageId, Timestamp, WorkspaceRole};
use lazy_protocol::{
    ApiError, BrowseChannelsQuery, BrowseChannelsResponse, BrowsedChannel, ChannelSort,
    Conversation, ErrorCode, Event, PeoplePage, PeopleQuery, Person, PersonResponse, SavedItem,
    SavedPage, SavedQuery, SavedResponse,
};

// ----- Helpers -----

struct Fx {
    world: World,
    driver: Driver,
    general: ConversationId,
    head: i64,
}

fn setup_with(world: World) -> Fx {
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(general, vec![], false);
    let head = world.head.get();
    Fx {
        world,
        driver,
        general,
        head,
    }
}

fn setup() -> Fx {
    setup_with(World::new())
}

impl Fx {
    fn next(&mut self) -> i64 {
        self.head += 1;
        self.head
    }

    fn server_event(&mut self, event: Event) -> Output {
        let at = self.next();
        self.driver.send(self.world.event_at(at, event))
    }
}

fn rejected(code: ErrorCode, message: &str) -> RequestError {
    RequestError::Rejected(ApiError::new(code, message))
}

fn lost() -> RequestError {
    RequestError::Unknown("timed out".into())
}

fn first<T>(out: &Output, pick: impl Fn(&Effect) -> Option<T>) -> T {
    out.effects
        .iter()
        .find_map(pick)
        .unwrap_or_else(|| panic!("expected an effect in {:?}", out.effects))
}

fn saved_requests(out: &Output) -> Vec<(u64, SavedQuery)> {
    out.effects
        .iter()
        .filter_map(|e| match e {
            Effect::LoadSaved {
                generation, query, ..
            } => Some((*generation, query.clone())),
            _ => None,
        })
        .collect()
}

fn people_requests(out: &Output) -> Vec<(u64, PeopleQuery)> {
    out.effects
        .iter()
        .filter_map(|e| match e {
            Effect::LoadPeople {
                generation, query, ..
            } => Some((*generation, query.clone())),
            _ => None,
        })
        .collect()
}

fn browse_requests(out: &Output) -> Vec<(u64, BrowseChannelsQuery)> {
    out.effects
        .iter()
        .filter_map(|e| match e {
            Effect::LoadBrowse {
                generation, query, ..
            } => Some((*generation, query.clone())),
            _ => None,
        })
        .collect()
}

fn saved_item(world: &World, conversation: &Conversation, at: i64, cursor: u64) -> SavedItem {
    SavedItem {
        message: world.message(
            conversation.id,
            &world.other,
            at,
            &format!("saved {cursor}"),
        ),
        conversation: conversation.clone(),
        saved_at: Timestamp::now(),
        cursor,
    }
}

fn saved_page(items: Vec<SavedItem>, has_more: bool) -> Result<SavedPage, RequestError> {
    Ok(SavedPage { items, has_more })
}

fn person(user: &lazy_protocol::User, role: WorkspaceRole) -> Person {
    Person {
        user: user.clone(),
        role,
        joined_at: Timestamp::now(),
    }
}

fn open_saved_with(fx: &mut Fx, items: Vec<SavedItem>, has_more: bool) -> u64 {
    let out = fx.driver.send(Input::OpenSaved);
    let (generation, _) = saved_requests(&out).pop().expect("asked");
    fx.driver.send(Input::SavedLoaded {
        generation,
        result: saved_page(items, has_more),
    });
    generation
}

// ----- Saving a message -----

#[test]
fn the_bootstrap_marks_saved_messages_and_saving_flips_the_marker_at_once() {
    let mut world = World::new();
    let known = MessageId::generate();
    world.saved = vec![known];
    let mut fx = setup_with(world);
    assert!(fx.driver.core.is_saved(known));
    let other = MessageId::generate();
    assert!(!fx.driver.core.is_saved(other));

    let out = fx.driver.send(Input::SetMessageSaved {
        message_id: other,
        saved: true,
    });
    assert!(fx.driver.core.is_saved(other));
    assert!(out.changes.contains(&Change::SavedChanged));
    assert!(out.effects.iter().any(|e| matches!(
        e,
        Effect::SetMessageSaved { message, request } if *message == other && request.saved
    )));
    let out = fx.driver.send(Input::MessageSavedResult {
        message_id: other,
        result: Ok(SavedResponse {
            message_id: other,
            saved: true,
        }),
    });
    assert!(out.effects.is_empty());
    assert!(fx.driver.core.is_saved(other));

    // Removing one the bootstrap listed.
    fx.driver.send(Input::SetMessageSaved {
        message_id: known,
        saved: false,
    });
    assert!(!fx.driver.core.is_saved(known));
    // Asking for what already holds sends nothing.
    let out = fx.driver.send(Input::SetMessageSaved {
        message_id: other,
        saved: true,
    });
    assert!(out.effects.is_empty());
}

#[test]
fn a_wish_made_while_a_save_is_out_is_sent_after_its_answer() {
    let mut fx = setup();
    let message = MessageId::generate();
    fx.driver.send(Input::SetMessageSaved {
        message_id: message,
        saved: true,
    });
    let out = fx.driver.send(Input::SetMessageSaved {
        message_id: message,
        saved: false,
    });
    assert!(out.effects.is_empty(), "one at a time");
    assert!(!fx.driver.core.is_saved(message));
    let out = fx.driver.send(Input::MessageSavedResult {
        message_id: message,
        result: Ok(SavedResponse {
            message_id: message,
            saved: true,
        }),
    });
    assert!(matches!(
        out.effects.as_slice(),
        [Effect::SetMessageSaved { request, .. }] if !request.saved
    ));
    assert!(!fx.driver.core.is_saved(message), "never flickers back");
    fx.driver.send(Input::MessageSavedResult {
        message_id: message,
        result: Ok(SavedResponse {
            message_id: message,
            saved: false,
        }),
    });
    assert!(!fx.driver.core.is_saved(message));
}

#[test]
fn a_lost_save_is_repeated_three_times_then_reverted_with_a_notice() {
    let mut fx = setup();
    let message = MessageId::generate();
    fx.driver.send(Input::SetMessageSaved {
        message_id: message,
        saved: true,
    });
    for _ in 0..2 {
        let out = fx.driver.send(Input::MessageSavedResult {
            message_id: message,
            result: Err(lost()),
        });
        assert_eq!(out.effects.len(), 1);
        assert!(fx.driver.core.is_saved(message));
    }
    let out = fx.driver.send(Input::MessageSavedResult {
        message_id: message,
        result: Err(lost()),
    });
    assert!(out.effects.is_empty());
    assert!(!fx.driver.core.is_saved(message));
    assert!(fx.driver.core.notice().is_some());
    // The save that was thought lost had in fact worked: the event says so.
    fx.server_event(Event::SavedChanged {
        message_id: message,
        saved: true,
    });
    assert!(fx.driver.core.is_saved(message));
}

#[test]
fn a_refused_save_reverts_in_words_and_an_expired_session_ends_the_session() {
    let mut fx = setup();
    let message = MessageId::generate();
    fx.driver.send(Input::SetMessageSaved {
        message_id: message,
        saved: true,
    });
    fx.driver.send(Input::MessageSavedResult {
        message_id: message,
        result: Err(rejected(ErrorCode::NotFound, "no")),
    });
    assert!(!fx.driver.core.is_saved(message));
    assert!(fx.driver.core.notice().unwrap().contains("can't be saved"));
    fx.driver.send(Input::SetMessageSaved {
        message_id: message,
        saved: true,
    });
    fx.driver.send(Input::MessageSavedResult {
        message_id: message,
        result: Err(rejected(ErrorCode::Unauthenticated, "expired")),
    });
    assert!(matches!(
        fx.driver.core.phase(),
        SessionPhase::SignedOut { .. }
    ));
}

#[test]
fn events_set_the_marker_but_never_flip_a_change_that_is_not_saved() {
    let mut fx = setup();
    let message = MessageId::generate();
    let out = fx.server_event(Event::SavedChanged {
        message_id: message,
        saved: true,
    });
    assert!(fx.driver.core.is_saved(message));
    assert!(out.changes.contains(&Change::SavedChanged));
    // The person removes it; an older event still says saved.
    fx.driver.send(Input::SetMessageSaved {
        message_id: message,
        saved: false,
    });
    fx.server_event(Event::SavedChanged {
        message_id: message,
        saved: true,
    });
    assert!(!fx.driver.core.is_saved(message));
    fx.driver.send(Input::MessageSavedResult {
        message_id: message,
        result: Ok(SavedResponse {
            message_id: message,
            saved: false,
        }),
    });
    assert!(!fx.driver.core.is_saved(message));
}

// ----- The saved list -----

#[test]
fn the_saved_list_loads_pages_from_the_last_cursor_and_marks_what_it_lists() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let out = fx.driver.send(Input::OpenSaved);
    let (generation, query) = saved_requests(&out).pop().unwrap();
    assert_eq!(query.before, None);
    assert_eq!(query.limit, Some(SAVED_PAGE));
    assert!(fx.driver.core.saved().open);
    assert_eq!(fx.driver.core.saved().status, LoadStatus::Loading);
    let a = saved_item(&fx.world, &general, 20, 90);
    let b = saved_item(&fx.world, &general, 21, 80);
    fx.driver.send(Input::SavedLoaded {
        generation,
        result: saved_page(vec![a.clone(), b.clone()], true),
    });
    let view = fx.driver.core.saved();
    assert_eq!(view.items, vec![a.clone(), b.clone()]);
    assert!(view.has_more && view.status == LoadStatus::Ready);
    assert!(
        fx.driver.core.is_saved(a.message.id),
        "a listed message is saved"
    );

    let out = fx.driver.send(Input::LoadMoreSaved);
    let (more, query) = saved_requests(&out).pop().unwrap();
    assert_eq!(query.before, Some(80), "after the last cursor seen");
    assert!(saved_requests(&fx.driver.send(Input::LoadMoreSaved)).is_empty());
    let c = saved_item(&fx.world, &general, 22, 70);
    fx.driver.send(Input::SavedLoaded {
        generation: more,
        result: saved_page(vec![b.clone(), c.clone()], false),
    });
    assert_eq!(fx.driver.core.saved().items, vec![a, b, c]);
    assert!(saved_requests(&fx.driver.send(Input::LoadMoreSaved)).is_empty());
}

#[test]
fn an_older_answer_is_never_shown_and_closing_forgets_the_list() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let out = fx.driver.send(Input::OpenSaved);
    let (old, _) = saved_requests(&out).pop().unwrap();
    // Closing and opening again makes a newer request.
    let out = fx.driver.send(Input::CloseSaved);
    assert!(out.effects.contains(&Effect::CancelSaved));
    assert!(!fx.driver.core.saved().open);
    let out = fx.driver.send(Input::OpenSaved);
    let (new, _) = saved_requests(&out).pop().unwrap();
    assert!(new > old);
    fx.driver.send(Input::SavedLoaded {
        generation: old,
        result: saved_page(vec![saved_item(&fx.world, &general, 20, 5)], false),
    });
    assert!(fx.driver.core.saved().items.is_empty());
    assert_eq!(fx.driver.core.saved().status, LoadStatus::Loading);
    fx.driver.send(Input::SavedLoaded {
        generation: new,
        result: saved_page(vec![], false),
    });
    assert_eq!(fx.driver.core.saved().status, LoadStatus::Ready);
    // A late answer after closing finds nothing.
    let out = fx.driver.send(Input::CloseSaved);
    assert!(out.effects.is_empty(), "nothing was out");
    fx.driver.send(Input::SavedLoaded {
        generation: new,
        result: saved_page(vec![saved_item(&fx.world, &general, 20, 5)], false),
    });
    assert!(fx.driver.core.saved().items.is_empty());
}

#[test]
fn removing_a_saved_item_takes_it_off_the_list_and_a_failed_removal_asks_again() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let a = saved_item(&fx.world, &general, 20, 90);
    let b = saved_item(&fx.world, &general, 21, 80);
    open_saved_with(&mut fx, vec![a.clone(), b.clone()], false);
    fx.driver.send(Input::SetMessageSaved {
        message_id: a.message.id,
        saved: false,
    });
    assert_eq!(fx.driver.core.saved().items, vec![b.clone()]);
    // The removal is refused: the marker is back, and the list is asked for again.
    let out = fx.driver.send(Input::MessageSavedResult {
        message_id: a.message.id,
        result: Err(rejected(ErrorCode::Forbidden, "no")),
    });
    assert!(fx.driver.core.is_saved(a.message.id));
    let (generation, query) = saved_requests(&out)
        .pop()
        .expect("the list is asked for again");
    assert_eq!(query.before, None);
    assert_eq!(fx.driver.core.saved().status, LoadStatus::Ready, "quietly");
    fx.driver.send(Input::SavedLoaded {
        generation,
        result: saved_page(vec![a.clone(), b.clone()], false),
    });
    assert_eq!(fx.driver.core.saved().items.len(), 2);
    // An answer that lists what is waiting to be removed does not bring it back.
    fx.driver.send(Input::SetMessageSaved {
        message_id: b.message.id,
        saved: false,
    });
    let out = fx.driver.send(Input::LoadMoreSaved);
    assert!(saved_requests(&out).is_empty());
}

#[test]
fn asking_to_save_waits_for_the_server_before_the_open_list_is_asked_for_again() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let listed = saved_item(&fx.world, &general, 20, 90);
    open_saved_with(&mut fx, vec![listed.clone()], false);
    let message = MessageId::generate();
    // The person asks: the marker flips, but the server has not heard yet.
    let out = fx.driver.send(Input::SetMessageSaved {
        message_id: message,
        saved: true,
    });
    assert!(
        saved_requests(&out).is_empty(),
        "too early to ask for the list"
    );
    // The server has it now: the list is asked for, quietly.
    let out = fx.driver.send(Input::MessageSavedResult {
        message_id: message,
        result: Ok(SavedResponse {
            message_id: message,
            saved: true,
        }),
    });
    let (generation, query) = saved_requests(&out).pop().expect("asked for the list");
    assert_eq!(query.before, None);
    assert_eq!(fx.driver.core.saved().items.len(), 1, "quietly");
    let newcomer = saved_item(&fx.world, &general, 25, 99);
    fx.driver.send(Input::SavedLoaded {
        generation,
        result: saved_page(vec![newcomer, listed.clone()], false),
    });
    assert_eq!(fx.driver.core.saved().items.len(), 2);
    // Confirming one that is listed already asks for nothing.
    fx.driver.send(Input::SetMessageSaved {
        message_id: listed.message.id,
        saved: true,
    });
    let out = fx.driver.send(Input::MessageSavedResult {
        message_id: listed.message.id,
        result: Ok(SavedResponse {
            message_id: listed.message.id,
            saved: true,
        }),
    });
    assert!(saved_requests(&out).is_empty());
}

#[test]
fn saved_events_for_other_messages_update_an_open_list() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let a = saved_item(&fx.world, &general, 20, 90);
    open_saved_with(&mut fx, vec![a.clone()], false);
    // Saved on another device: the new item's place is the server's to say.
    let newcomer = MessageId::generate();
    let out = fx.server_event(Event::SavedChanged {
        message_id: newcomer,
        saved: true,
    });
    let (generation, _) = saved_requests(&out).pop().expect("asked again");
    assert_eq!(fx.driver.core.saved().items.len(), 1, "quietly");
    fx.driver.send(Input::SavedLoaded {
        generation,
        result: saved_page(
            vec![saved_item(&fx.world, &general, 25, 99), a.clone()],
            false,
        ),
    });
    assert_eq!(fx.driver.core.saved().items.len(), 2);
    // Removed on another device.
    fx.server_event(Event::SavedChanged {
        message_id: a.message.id,
        saved: false,
    });
    assert_eq!(fx.driver.core.saved().items.len(), 1);
    assert!(!fx.driver.core.is_saved(a.message.id));
    // Nothing is asked when the list is closed.
    fx.driver.send(Input::CloseSaved);
    let out = fx.server_event(Event::SavedChanged {
        message_id: MessageId::generate(),
        saved: true,
    });
    assert!(saved_requests(&out).is_empty());
}

#[test]
fn a_page_cannot_bring_back_an_item_whose_removal_is_waiting() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let a = saved_item(&fx.world, &general, 20, 90);
    open_saved_with(&mut fx, vec![a.clone()], false);
    // The person removes it; the removal is out. Something else is saved on another device, so
    // the list is asked for, and the server has not heard of the removal when it answers.
    fx.driver.send(Input::SetMessageSaved {
        message_id: a.message.id,
        saved: false,
    });
    let other = MessageId::generate();
    let out = fx.server_event(Event::SavedChanged {
        message_id: other,
        saved: true,
    });
    let (generation, _) = saved_requests(&out).pop().expect("asked for the list");
    let b = saved_item(&fx.world, &general, 25, 99);
    fx.driver.send(Input::SavedLoaded {
        generation,
        result: saved_page(vec![b.clone(), a.clone()], false),
    });
    let ids: Vec<MessageId> = fx
        .driver
        .core
        .saved()
        .items
        .iter()
        .map(|i| i.message.id)
        .collect();
    assert_eq!(ids, vec![b.message.id], "the removed one stays off");
    assert!(!fx.driver.core.is_saved(a.message.id));
}

#[test]
fn a_saved_item_follows_edits_deletions_and_leaving_the_conversation() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let random = fx.world.second_channel.clone();
    let a = saved_item(&fx.world, &general, 20, 90);
    let b = saved_item(&fx.world, &random, 21, 80);
    let c = saved_item(&fx.world, &general, 22, 70);
    open_saved_with(&mut fx, vec![a.clone(), b.clone(), c.clone()], false);
    let edited = fx.world.edited(&a.message, "reworded");
    let at = fx.next();
    fx.driver.send(fx.world.edited_event(at, &edited, None));
    assert_eq!(fx.driver.core.saved().items[0].message, edited);
    let at = fx.next();
    fx.driver
        .send(fx.world.deleted_event(at, &fx.world.tombstone(&c.message)));
    assert_eq!(fx.driver.core.saved().items.len(), 2);
    let me = fx.world.me.id;
    fx.server_event(Event::MemberRemoved {
        conversation_id: random.id,
        user_id: me,
    });
    let ids: Vec<MessageId> = fx
        .driver
        .core
        .saved()
        .items
        .iter()
        .map(|i| i.message.id)
        .collect();
    assert_eq!(ids, vec![a.message.id]);
}

#[test]
fn saved_failures_say_what_happened_and_a_resync_asks_again() {
    let mut fx = setup();
    let general = fx.world.channel.clone();
    let out = fx.driver.send(Input::OpenSaved);
    let (generation, _) = saved_requests(&out).pop().unwrap();
    fx.driver.send(Input::SavedLoaded {
        generation,
        result: Err(lost()),
    });
    assert_eq!(
        fx.driver.core.saved().status,
        LoadStatus::Failed {
            reason: "Couldn't reach the server.".into(),
            can_retry: true
        }
    );
    // Closing and opening again is the retry.
    fx.driver.send(Input::CloseSaved);
    let a = saved_item(&fx.world, &general, 20, 90);
    open_saved_with(&mut fx, vec![a.clone()], false);
    // The log is skipped: what is shown may be stale.
    fx.driver
        .send(Input::Frame(lazy_protocol::ServerFrame::SnapshotRequired {
            workspace_id: fx.world.workspace.id,
            reason: lazy_protocol::SnapshotReason::CursorExpired,
        }));
    let mut bootstrap = fx.world.bootstrap();
    bootstrap.workspaces[0].saved = vec![a.message.id];
    let out = fx.driver.send(Input::BootstrapLoaded(Box::new(bootstrap)));
    let (again, query) = saved_requests(&out).pop().expect("asked again");
    assert_eq!(query.before, None);
    assert_eq!(fx.driver.core.saved().items.len(), 1, "quietly");
    assert!(fx.driver.core.is_saved(a.message.id), "the bootstrap's ids");
    fx.driver.send(Input::SavedLoaded {
        generation: again,
        result: saved_page(vec![], false),
    });
    assert!(fx.driver.core.saved().items.is_empty());
    // Signing out forgets all of it.
    fx.driver.send(Input::SignOut);
    assert!(!fx.driver.core.is_saved(a.message.id));
    assert!(!fx.driver.core.saved().open);
}

// ----- The people directory and profiles -----

#[test]
fn the_people_directory_pages_searches_and_keeps_only_the_newest_query() {
    let mut fx = setup();
    let zoe = fx.world.other.clone();
    let out = fx.driver.send(Input::OpenPeople);
    let (generation, query) = people_requests(&out).pop().unwrap();
    assert_eq!(query.query, None);
    assert_eq!(query.limit, Some(lazy_client_core::DIRECTORY_PAGE));
    assert!(fx.driver.core.people().open);
    let ada = user("ada");
    fx.driver.send(Input::PeopleLoaded {
        generation,
        result: Ok(PeoplePage {
            people: vec![
                person(&ada, WorkspaceRole::Admin),
                person(&zoe, WorkspaceRole::Member),
            ],
            has_more: true,
        }),
    });
    assert_eq!(fx.driver.core.people().people.len(), 2);

    let out = fx.driver.send(Input::LoadMorePeople);
    let (more, query) = people_requests(&out).pop().unwrap();
    assert_eq!(query.offset, Some(2));
    assert!(people_requests(&fx.driver.send(Input::LoadMorePeople)).is_empty());
    let kim = user("kim");
    fx.driver.send(Input::PeopleLoaded {
        generation: more,
        result: Ok(PeoplePage {
            people: vec![
                person(&zoe, WorkspaceRole::Member),
                person(&kim, WorkspaceRole::Owner),
            ],
            has_more: false,
        }),
    });
    assert_eq!(fx.driver.core.people().people.len(), 3, "no one twice");

    // Typing: each text asks for the first page; only the newest is believed.
    let out = fx.driver.send(Input::SetPeopleQuery("a".into()));
    let (first, query) = people_requests(&out).pop().unwrap();
    assert_eq!(query.query.as_deref(), Some("a"));
    assert!(
        fx.driver.core.people().people.is_empty(),
        "cleared while it loads"
    );
    let out = fx.driver.send(Input::SetPeopleQuery("ad".into()));
    let (second, _) = people_requests(&out).pop().unwrap();
    assert!(second > first);
    fx.driver.send(Input::PeopleLoaded {
        generation: first,
        result: Ok(PeoplePage {
            people: vec![person(&kim, WorkspaceRole::Owner)],
            has_more: false,
        }),
    });
    assert!(fx.driver.core.people().people.is_empty());
    fx.driver.send(Input::PeopleLoaded {
        generation: second,
        result: Ok(PeoplePage {
            people: vec![person(&ada, WorkspaceRole::Admin)],
            has_more: false,
        }),
    });
    assert_eq!(fx.driver.core.people().people.len(), 1);
    // The same text again asks for nothing.
    assert!(people_requests(&fx.driver.send(Input::SetPeopleQuery("ad".into()))).is_empty());
    // A text over the limit asks for nothing and says so.
    let out = fx.driver.send(Input::SetPeopleQuery("x".repeat(101)));
    assert!(people_requests(&out).is_empty());
    assert!(matches!(
        fx.driver.core.people().status,
        LoadStatus::Failed {
            can_retry: false,
            ..
        }
    ));
}

#[test]
fn opening_a_list_that_failed_to_load_asks_again() {
    let mut fx = setup();
    let out = fx.driver.send(Input::OpenSaved);
    let (generation, _) = saved_requests(&out).pop().unwrap();
    // Open and loading: nothing more to ask.
    assert!(saved_requests(&fx.driver.send(Input::OpenSaved)).is_empty());
    fx.driver.send(Input::SavedLoaded {
        generation,
        result: Err(lost()),
    });
    let out = fx.driver.send(Input::OpenSaved);
    let (retry, query) = saved_requests(&out).pop().expect("asked again");
    assert!(retry > generation && query.before.is_none());
    assert_eq!(fx.driver.core.saved().status, LoadStatus::Loading);
    fx.driver.send(Input::SavedLoaded {
        generation: retry,
        result: saved_page(vec![], false),
    });
    assert!(
        saved_requests(&fx.driver.send(Input::OpenSaved)).is_empty(),
        "loaded"
    );

    let out = fx.driver.send(Input::OpenPeople);
    let (generation, _) = people_requests(&out).pop().unwrap();
    assert!(people_requests(&fx.driver.send(Input::OpenPeople)).is_empty());
    fx.driver.send(Input::PeopleLoaded {
        generation,
        result: Err(lost()),
    });
    let out = fx.driver.send(Input::OpenPeople);
    assert_eq!(people_requests(&out).len(), 1, "asked again");
}

#[test]
fn people_failures_a_status_event_and_closing() {
    let mut fx = setup();
    let zoe = fx.world.other.clone();
    let out = fx.driver.send(Input::OpenPeople);
    let (generation, _) = people_requests(&out).pop().unwrap();
    fx.driver.send(Input::PeopleLoaded {
        generation,
        result: Err(lost()),
    });
    assert!(matches!(
        fx.driver.core.people().status,
        LoadStatus::Failed {
            can_retry: true,
            ..
        }
    ));
    fx.driver.send(Input::ClosePeople);
    let out = fx.driver.send(Input::OpenPeople);
    let (generation, _) = people_requests(&out).pop().unwrap();
    fx.driver.send(Input::PeopleLoaded {
        generation,
        result: Ok(PeoplePage {
            people: vec![person(&zoe, WorkspaceRole::Member)],
            has_more: false,
        }),
    });
    let status = UserStatus::new("Out today", None, None).unwrap();
    let out = fx.server_event(Event::UserStatusChanged {
        user_id: zoe.id,
        status: Some(status.clone()),
    });
    assert!(out.changes.contains(&Change::PeopleChanged));
    assert_eq!(fx.driver.core.people().people[0].user.status, Some(status));
    // Closing stops what is out; nothing is asked when closed.
    let out = fx.driver.send(Input::SetPeopleQuery("z".into()));
    assert_eq!(people_requests(&out).len(), 1);
    let out = fx.driver.send(Input::ClosePeople);
    assert!(out.effects.contains(&Effect::CancelPeople));
    assert!(people_requests(&fx.driver.send(Input::SetPeopleQuery("q".into()))).is_empty());
}

#[test]
fn a_profile_shows_what_is_known_while_it_loads_and_says_when_nobody_is_there() {
    let mut fx = setup();
    let zoe = fx.world.other.clone();
    let out = fx.driver.send(Input::OpenProfile { user_id: zoe.id });
    let (generation, user) = first(&out, |e| match e {
        Effect::LoadProfile {
            generation, user, ..
        } => Some((*generation, *user)),
        _ => None,
    });
    assert_eq!(user, zoe.id);
    let view = fx.driver.core.profile().unwrap();
    assert_eq!((view.user_id, view.person.is_none()), (zoe.id, true));
    assert_eq!(view.status, LoadStatus::Loading);
    assert!(
        fx.driver.core.member(zoe.id).is_some(),
        "what is known meanwhile"
    );
    let loaded = person(&zoe, WorkspaceRole::Admin);
    fx.driver.send(Input::ProfileLoaded {
        generation,
        result: Ok(PersonResponse {
            person: loaded.clone(),
        }),
    });
    let view = fx.driver.core.profile().unwrap();
    assert_eq!(view.person, Some(loaded));
    assert_eq!(view.status, LoadStatus::Ready);
    // A status event reaches the profile.
    let status = UserStatus::new("Lunch", None, None).unwrap();
    let out = fx.server_event(Event::UserStatusChanged {
        user_id: zoe.id,
        status: Some(status.clone()),
    });
    assert!(out.changes.contains(&Change::ProfileChanged));
    assert_eq!(
        fx.driver
            .core
            .profile()
            .unwrap()
            .person
            .as_ref()
            .unwrap()
            .user
            .status,
        Some(status)
    );

    // Someone who is not here.
    let stranger = lazy_domain::UserId::generate();
    let out = fx.driver.send(Input::OpenProfile { user_id: stranger });
    let generation = first(&out, |e| match e {
        Effect::LoadProfile { generation, .. } => Some(*generation),
        _ => None,
    });
    fx.driver.send(Input::ProfileLoaded {
        generation,
        result: Err(rejected(ErrorCode::NotFound, "no")),
    });
    assert!(matches!(
        &fx.driver.core.profile().unwrap().status,
        LoadStatus::Failed { reason, can_retry: false } if reason.contains("isn't in this workspace")
    ));
    // An answer for a profile closed meanwhile is dropped.
    let out = fx.driver.send(Input::OpenProfile { user_id: zoe.id });
    let late = first(&out, |e| match e {
        Effect::LoadProfile { generation, .. } => Some(*generation),
        _ => None,
    });
    let out = fx.driver.send(Input::CloseProfile);
    assert!(out.effects.contains(&Effect::CancelProfile));
    fx.driver.send(Input::ProfileLoaded {
        generation: late,
        result: Ok(PersonResponse {
            person: person(&zoe, WorkspaceRole::Member),
        }),
    });
    assert!(fx.driver.core.profile().is_none());
}

// ----- The channel directory -----

fn directory(fx: &Fx, names: &[&str]) -> Vec<BrowsedChannel> {
    names
        .iter()
        .map(|name| BrowsedChannel {
            conversation: channel(fx.world.workspace.id, name),
            joined: false,
        })
        .collect()
}

#[test]
fn the_directory_asks_with_its_settings_and_pages_from_the_rows_loaded() {
    let mut fx = setup();
    let out = fx.driver.send(Input::BrowseChannels);
    let (generation, query) = browse_requests(&out).pop().unwrap();
    assert_eq!(
        query,
        BrowseChannelsQuery {
            query: None,
            joined: None,
            include_archived: None,
            sort: Some(ChannelSort::Name),
            offset: None,
            limit: Some(lazy_client_core::DIRECTORY_PAGE),
        }
    );
    let rows = directory(&fx, &["alpha", "beta"]);
    fx.driver.send(Input::BrowseLoaded {
        generation,
        result: Ok(BrowseChannelsResponse {
            channels: rows.clone(),
            has_more: true,
        }),
    });
    assert_eq!(fx.driver.core.browse().channels, rows);

    let out = fx.driver.send(Input::LoadMoreBrowse);
    let (more, query) = browse_requests(&out).pop().unwrap();
    assert_eq!(query.offset, Some(2));
    assert!(browse_requests(&fx.driver.send(Input::LoadMoreBrowse)).is_empty());
    let more_rows = directory(&fx, &["gamma"]);
    fx.driver.send(Input::BrowseLoaded {
        generation: more,
        result: Ok(BrowseChannelsResponse {
            channels: [rows[1].clone(), more_rows[0].clone()].to_vec(),
            has_more: false,
        }),
    });
    assert_eq!(
        fx.driver.core.browse().channels.len(),
        3,
        "a row twice is shown once"
    );
    assert!(browse_requests(&fx.driver.send(Input::LoadMoreBrowse)).is_empty());
}

#[test]
fn changing_the_query_the_filter_or_the_sort_asks_again_and_the_newest_wins() {
    let mut fx = setup();
    let out = fx.driver.send(Input::BrowseChannels);
    let (open, _) = browse_requests(&out).pop().unwrap();
    let out = fx.driver.send(Input::SetBrowseQuery("  des  ".into()));
    let (typed, query) = browse_requests(&out).pop().unwrap();
    assert_eq!(query.query.as_deref(), Some("des"), "trimmed");
    let out = fx.driver.send(Input::SetBrowseJoined(Some(false)));
    let (filtered, query) = browse_requests(&out).pop().unwrap();
    assert_eq!(
        (query.joined, query.query.as_deref()),
        (Some(false), Some("des"))
    );
    let out = fx.driver.send(Input::SetBrowseSort(ChannelSort::Members));
    let (sorted, query) = browse_requests(&out).pop().unwrap();
    assert_eq!(query.sort, Some(ChannelSort::Members));
    assert!(open < typed && typed < filtered && filtered < sorted);
    let view = fx.driver.core.browse();
    assert_eq!(
        (view.query.as_str(), view.joined, view.sort),
        ("  des  ", Some(false), ChannelSort::Members)
    );
    for old in [open, typed, filtered] {
        fx.driver.send(Input::BrowseLoaded {
            generation: old,
            result: Ok(BrowseChannelsResponse {
                channels: directory(&fx, &["stale"]),
                has_more: true,
            }),
        });
    }
    assert!(fx.driver.core.browse().channels.is_empty());
    assert_eq!(fx.driver.core.browse().status, LoadStatus::Loading);
    fx.driver.send(Input::BrowseLoaded {
        generation: sorted,
        result: Ok(BrowseChannelsResponse {
            channels: directory(&fx, &["design"]),
            has_more: false,
        }),
    });
    assert_eq!(fx.driver.core.browse().channels.len(), 1);
    // Opening the directory again keeps the settings.
    let out = fx.driver.send(Input::BrowseChannels);
    let (_, query) = browse_requests(&out).pop().unwrap();
    assert_eq!(query.query.as_deref(), Some("des"));
    // Closing forgets them.
    let out = fx.driver.send(Input::CloseBrowse);
    assert!(out.effects.contains(&Effect::CancelBrowse));
    assert_eq!(fx.driver.core.browse().query, "");
}

#[test]
fn joining_or_leaving_updates_the_loaded_rows_without_a_request() {
    let mut fx = setup();
    let out = fx.driver.send(Input::BrowseChannels);
    let (generation, _) = browse_requests(&out).pop().unwrap();
    let rows = directory(&fx, &["alpha", "beta"]);
    fx.driver.send(Input::BrowseLoaded {
        generation,
        result: Ok(BrowseChannelsResponse {
            channels: rows.clone(),
            has_more: false,
        }),
    });
    let alpha = rows[0].conversation.clone();
    let me = fx.world.me.id;
    let out = fx.server_event(Event::MemberAdded {
        conversation: alpha.clone(),
        user_id: me,
    });
    assert!(browse_requests(&out).is_empty());
    assert!(fx.driver.core.browse().channels[0].joined);
    fx.server_event(Event::MemberRemoved {
        conversation_id: alpha.id,
        user_id: me,
    });
    assert!(!fx.driver.core.browse().channels[0].joined);
    // A row that is joined by the time the answer comes shows as joined.
    let out = fx.driver.send(Input::SetBrowseQuery("al".into()));
    let (generation, _) = browse_requests(&out).pop().unwrap();
    fx.server_event(Event::MemberAdded {
        conversation: alpha.clone(),
        user_id: me,
    });
    fx.driver.send(Input::BrowseLoaded {
        generation,
        result: Ok(BrowseChannelsResponse {
            channels: vec![rows[0].clone()],
            has_more: false,
        }),
    });
    assert!(fx.driver.core.browse().channels[0].joined);
}

#[test]
fn directory_failures_a_long_text_and_a_resync() {
    let mut fx = setup();
    let out = fx.driver.send(Input::BrowseChannels);
    let (generation, _) = browse_requests(&out).pop().unwrap();
    fx.driver.send(Input::BrowseLoaded {
        generation,
        result: Err(rejected(ErrorCode::RateLimited, "slow")),
    });
    assert!(matches!(
        fx.driver.core.browse().status,
        LoadStatus::Failed {
            can_retry: true,
            ..
        }
    ));
    let out = fx.driver.send(Input::SetBrowseQuery("x".repeat(101)));
    assert!(browse_requests(&out).is_empty());
    assert!(matches!(
        fx.driver.core.browse().status,
        LoadStatus::Failed {
            can_retry: false,
            ..
        }
    ));
    let out = fx.driver.send(Input::SetBrowseQuery("fine".into()));
    let (generation, _) = browse_requests(&out).pop().unwrap();
    fx.driver.send(Input::BrowseLoaded {
        generation,
        result: Ok(BrowseChannelsResponse {
            channels: directory(&fx, &["fine-one"]),
            has_more: false,
        }),
    });
    fx.driver
        .send(Input::Frame(lazy_protocol::ServerFrame::SnapshotRequired {
            workspace_id: fx.world.workspace.id,
            reason: lazy_protocol::SnapshotReason::CursorExpired,
        }));
    let out = fx
        .driver
        .send(Input::BootstrapLoaded(Box::new(fx.world.bootstrap())));
    let (_, query) = browse_requests(&out).pop().expect("asked again");
    assert_eq!(query.query.as_deref(), Some("fine"));
    assert_eq!(fx.driver.core.browse().channels.len(), 1, "quietly");
}

// ----- Recently visited conversations -----

#[test]
fn selecting_a_conversation_by_any_route_is_remembered_and_saved_most_recent_first() {
    let mut world = World::new();
    let dm = people(world.workspace.id, &[&world.me, &world.other]);
    world.extra.push(dm.clone());
    let random = world.second_channel.id;
    let mut fx = setup_with(world);
    // The default selection at start counts.
    assert_eq!(fx.driver.core.recent_conversations(), [fx.general]);
    let persisted = |out: &Output| {
        out.effects.iter().find_map(|e| match e {
            Effect::PersistRecentConversations(ids) => Some(ids.clone()),
            _ => None,
        })
    };
    let out = fx.driver.send(Input::SelectConversation(random));
    assert_eq!(persisted(&out), Some(vec![random, fx.general]));
    let out = fx.driver.send(Input::SelectConversation(dm.id));
    assert_eq!(persisted(&out), Some(vec![dm.id, random, fx.general]));
    let out = fx.driver.send(Input::SelectConversation(fx.general));
    assert_eq!(persisted(&out), Some(vec![fx.general, dm.id, random]));
    // Selecting what is selected saves nothing.
    let out = fx.driver.send(Input::SelectConversation(fx.general));
    assert_eq!(persisted(&out), None);
    // Opening a conversation counts too.
    let out = fx.driver.send(Input::SelectConversation(random));
    assert_eq!(fx.driver.core.recent_conversations()[0], random);
    let _ = out;
}

#[test]
fn the_recent_list_is_bounded_restored_repaired_and_forgotten_at_sign_out() {
    let mut world = World::new();
    let extra: Vec<Conversation> = (0..25)
        .map(|n| channel(world.workspace.id, &format!("c{n}")))
        .collect();
    world.extra = extra.clone();
    let mut fx = setup_with(world);
    for conversation in &extra {
        fx.driver.send(Input::SelectConversation(conversation.id));
    }
    let recent = fx.driver.core.recent_conversations();
    assert_eq!(recent.len(), MAX_RECENT_CONVERSATIONS);
    assert_eq!(recent[0], extra[24].id);
    fx.driver.send(Input::SignOut);
    assert!(fx.driver.core.recent_conversations().is_empty());

    // What a previous run saved: repeats dropped, bounded.
    let a = ConversationId::generate();
    let b = ConversationId::generate();
    let mut saved = vec![a, b, a];
    saved.extend((0..30).map(|_| ConversationId::generate()));
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        recent_conversations: saved,
        ..Default::default()
    }));
    let recent = driver.core.recent_conversations();
    assert_eq!(recent.len(), MAX_RECENT_CONVERSATIONS);
    assert_eq!(&recent[..2], [a, b]);
}

// ----- The quick switcher -----

struct Switcher {
    world: World,
    driver: Driver,
    dm: Conversation,
    group: Conversation,
    design: Conversation,
    engineering: Conversation,
    old: Conversation,
    ada: lazy_protocol::User,
    morgan: lazy_protocol::User,
}

/// `#general`, `#random`, `#design`, `#desert`, `#engineering`, an archived `#old`, a direct
/// conversation with Zoe, a group, and Ada (display name different from her username) and Morgan
/// with no conversation yet.
fn switcher() -> Switcher {
    let mut world = World::new();
    let ada = {
        let mut u = user("ada");
        u.display_name = lazy_domain::DisplayName::parse("Ada Lovelace").unwrap();
        u
    };
    let morgan = user("morgan");
    let third = user("kim");
    world
        .extra_users
        .extend([ada.clone(), morgan.clone(), third.clone()]);
    let dm = people(world.workspace.id, &[&world.me, &world.other]);
    let group = people(world.workspace.id, &[&world.me, &world.other, &third]);
    let design = channel(world.workspace.id, "design");
    let desert = channel(world.workspace.id, "desert");
    let engineering = channel(world.workspace.id, "engineering");
    let mut old = channel(world.workspace.id, "old");
    old.archived = true;
    world.extra.extend([
        dm.clone(),
        group.clone(),
        design.clone(),
        desert.clone(),
        engineering.clone(),
        old.clone(),
    ]);
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(general, vec![], false);
    Switcher {
        world,
        driver,
        dm,
        group,
        design,
        engineering,
        old,
        ada,
        morgan,
    }
}

fn titles(items: &[lazy_client_core::QuickSwitcherItem]) -> Vec<String> {
    items.iter().map(|i| i.title.clone()).collect()
}

#[test]
fn with_no_text_the_switcher_lists_recent_conversations_then_unread_ones() {
    let mut s = switcher();
    for conversation in [&s.design, &s.dm, &s.engineering] {
        s.driver.send(Input::SelectConversation(conversation.id));
    }
    // An unread conversation not visited, a muted one with unread, and an archived one visited.
    let general = s.world.channel.id;
    let random = s.world.second_channel.id;
    for (id, at) in [(random, 11), (s.group.id, 12)] {
        s.driver.send(s.world.event_at(
            at,
            Event::UserStateChanged {
                state: lazy_protocol::ConversationUserState {
                    conversation_id: id,
                    favorite: false,
                    last_read: seq(0),
                    unread_count: 3,
                    mention_count: u32::from(id == random),
                },
            },
        ));
    }
    s.driver.send(Input::SetConversationNotificationPrefs {
        conversation: s.group.id,
        level: None,
        muted: true,
    });
    s.driver.send(Input::SelectConversation(s.old.id));
    let items = s.driver.core.quick_switcher_results("");
    let titles = titles(&items);
    // Recent, live ones first (most recent first), then unread ones, archived last.
    assert_eq!(
        titles,
        [
            "#engineering",
            "ZOE",
            "#design",
            "#general",
            "#random",
            "#old"
        ]
    );
    let random_item = items.iter().find(|i| i.title == "#random").unwrap();
    assert_eq!((random_item.unread, random_item.mentions), (3, 1));
    assert!(items.last().unwrap().archived);
    assert!(
        !titles.iter().any(|t| t.contains("MORGAN")),
        "the muted group is not offered for its unread messages"
    );
    let _ = general;
    // Nothing is offered that the person is not in, however long the list.
    assert!(items.len() <= QUICK_SWITCHER_LIMIT);
}

#[test]
fn text_finds_channels_conversations_people_and_places_best_first() {
    let s = switcher();
    // Channels by name; `gen` finds `general` before `engineering`.
    let items = s.driver.core.quick_switcher_results("gen");
    assert_eq!(items[0].title, "#general");
    assert_eq!(items[0].matched, vec![1, 2, 3], "after the `#`");
    assert!(titles(&items).contains(&"#engineering".to_owned()));
    // The last result offers the directory.
    assert_eq!(
        items.last().unwrap().target,
        QuickSwitcherTarget::BrowseChannels {
            query: "gen".into()
        }
    );
    assert_eq!(
        items.last().unwrap().title,
        "Browse channels matching \"gen\""
    );

    // A direct conversation by the person's name or username.
    let zoe = s.driver.core.quick_switcher_results("zoe");
    assert_eq!(zoe[0].target, QuickSwitcherTarget::Conversation(s.dm.id));
    assert_eq!(zoe[0].title, "ZOE");
    assert_eq!(zoe[0].matched, vec![0, 1, 2]);

    // A person with no conversation yet; matched by display name, or by username.
    let ada = s.driver.core.quick_switcher_results("love");
    assert_eq!(ada[0].target, QuickSwitcherTarget::Person(s.ada.id));
    assert_eq!(ada[0].subtitle.as_deref(), Some("@ada"));
    assert!(!ada[0].matched.is_empty());
    let by_username = s.driver.core.quick_switcher_results("ada");
    assert!(
        by_username
            .iter()
            .any(|i| i.target == QuickSwitcherTarget::Person(s.ada.id))
    );
    let morgan = s.driver.core.quick_switcher_results("morg");
    assert_eq!(morgan[0].target, QuickSwitcherTarget::Person(s.morgan.id));
    // Someone who has a direct conversation is the conversation, not a person.
    assert!(
        !zoe.iter()
            .any(|i| i.target == QuickSwitcherTarget::Person(s.world.other.id))
    );
    // The person themselves and bots are never offered.
    let me = s.driver.core.quick_switcher_results("jul");
    assert!(
        !me.iter()
            .any(|i| matches!(i.target, QuickSwitcherTarget::Person(_)))
    );

    // A group by its people; a username that is not in the title.
    let kim = s.driver.core.quick_switcher_results("kim");
    assert!(
        kim.iter()
            .any(|i| i.target == QuickSwitcherTarget::Conversation(s.group.id))
    );

    // The places.
    for (text, target) in [
        ("thr", QuickSwitcherTarget::Threads),
        ("sav", QuickSwitcherTarget::Saved),
        ("sea", QuickSwitcherTarget::Search),
    ] {
        let items = s.driver.core.quick_switcher_results(text);
        assert_eq!(items[0].target, target, "{text}");
    }
    // Nothing matches: only the directory is offered.
    let none = s.driver.core.quick_switcher_results("qqqq");
    assert_eq!(none.len(), 1);
}

#[test]
fn recency_and_unread_break_ties_muted_gets_no_bonus_and_archived_comes_after() {
    let mut s = switcher();
    // `des` matches `design` and `desert` equally; the one visited later is first.
    let plain = s.driver.core.quick_switcher_results("des");
    let first_two: Vec<_> = plain.iter().take(2).map(|i| i.title.clone()).collect();
    assert_eq!(
        first_two,
        ["#desert", "#design"],
        "by name when nothing else says"
    );
    s.driver.send(Input::SelectConversation(s.design.id));
    s.driver.send(Input::SelectConversation(s.world.channel.id));
    let visited = s.driver.core.quick_switcher_results("des");
    assert_eq!(visited[0].title, "#design");
    // Unread breaks the tie too, except when muted.
    let mut s = switcher();
    let unread = |s: &mut Switcher, id: ConversationId, at: i64| {
        s.driver.send(s.world.event_at(
            at,
            Event::UserStateChanged {
                state: lazy_protocol::ConversationUserState {
                    conversation_id: id,
                    favorite: false,
                    last_read: seq(0),
                    unread_count: 2,
                    mention_count: 0,
                },
            },
        ));
    };
    let design = s.design.id;
    unread(&mut s, design, 11);
    assert_eq!(
        s.driver.core.quick_switcher_results("des")[0].title,
        "#design"
    );
    s.driver.send(Input::SetConversationNotificationPrefs {
        conversation: s.design.id,
        level: None,
        muted: true,
    });
    let muted = s.driver.core.quick_switcher_results("des");
    assert_eq!(muted[0].title, "#desert", "muted gets no bonus");
    assert!(muted.iter().find(|i| i.title == "#design").unwrap().muted);
    // An archived channel comes after live ones even when it matches better.
    let old = s.driver.core.quick_switcher_results("old");
    let live_after = old
        .iter()
        .position(|i| i.title == "#old")
        .zip(old.iter().rposition(|i| {
            !i.archived && matches!(i.target, QuickSwitcherTarget::Conversation(_))
        }));
    if let Some((archived, live)) = live_after {
        assert!(archived > live, "archived after live: {:?}", titles(&old));
    }
    assert!(old.iter().find(|i| i.title == "#old").unwrap().archived);
}

#[test]
fn the_switcher_never_offers_what_the_person_is_not_in_and_is_bounded() {
    let mut world = World::new();
    let many: Vec<Conversation> = (0..30)
        .map(|n| channel(world.workspace.id, &format!("team{n}")))
        .collect();
    world.extra = many.clone();
    let mut driver = Driver::ready(&world);
    let items = driver.core.quick_switcher_results("team");
    assert_eq!(items.len(), QUICK_SWITCHER_LIMIT);
    assert!(matches!(
        items.last().unwrap().target,
        QuickSwitcherTarget::BrowseChannels { .. }
    ));
    // A channel in the directory that the person has not joined is not here.
    assert!(driver.core.quick_switcher_results("zzz-unjoined").len() == 1);
    // The person is not offered to themselves, with or without a conversation of their own.
    let me = driver.core.quick_switcher_results("jul");
    assert!(
        !me.iter()
            .any(|i| matches!(i.target, QuickSwitcherTarget::Person(_)))
    );
    // Leaving a conversation takes it out, though its id may still be remembered.
    let gone = many[0].clone();
    driver.send(Input::SelectConversation(gone.id));
    let me = world.me.id;
    let at = world.head.get() + 1;
    driver.send(world.event_at(
        at,
        Event::MemberRemoved {
            conversation_id: gone.id,
            user_id: me,
        },
    ));
    assert!(driver.core.recent_conversations().contains(&gone.id));
    let after = driver.core.quick_switcher_results("");
    assert!(
        !after
            .iter()
            .any(|i| i.target == QuickSwitcherTarget::Conversation(gone.id))
    );
}

#[test]
fn choosing_a_result_goes_where_it_says() {
    let mut s = switcher();
    // A conversation: select it.
    s.driver.send(Input::ChooseQuickSwitcher(
        QuickSwitcherTarget::Conversation(s.design.id),
    ));
    assert_eq!(s.driver.core.selected_conversation(), Some(s.design.id));
    // One the person is not in: nothing.
    s.driver.send(Input::ChooseQuickSwitcher(
        QuickSwitcherTarget::Conversation(ConversationId::generate()),
    ));
    assert_eq!(s.driver.core.selected_conversation(), Some(s.design.id));
    // A person: open (or create) the direct conversation.
    let out = s
        .driver
        .send(Input::ChooseQuickSwitcher(QuickSwitcherTarget::Person(
            s.morgan.id,
        )));
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::OpenConversation { .. }))
    );
    // The places.
    let out = s
        .driver
        .send(Input::ChooseQuickSwitcher(QuickSwitcherTarget::Threads));
    assert!(
        out.effects
            .iter()
            .any(|e| matches!(e, Effect::LoadInbox { .. }))
    );
    assert!(s.driver.core.inbox().is_some());
    let out = s
        .driver
        .send(Input::ChooseQuickSwitcher(QuickSwitcherTarget::Saved));
    assert_eq!(saved_requests(&out).len(), 1);
    assert!(s.driver.core.saved().open);
    s.driver
        .send(Input::ChooseQuickSwitcher(QuickSwitcherTarget::Search));
    let search = s.driver.core.search().expect("the search view opens");
    assert!(search.query.is_empty());
    let out = s.driver.send(Input::ChooseQuickSwitcher(
        QuickSwitcherTarget::BrowseChannels {
            query: "eng".into(),
        },
    ));
    let (_, query) = browse_requests(&out).pop().unwrap();
    assert_eq!(query.query.as_deref(), Some("eng"));
    assert_eq!(s.driver.core.browse().query, "eng");
}

mod property {
    use super::*;
    use lazy_domain::fuzzy::fuzzy_match;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(300))]

        /// Whatever is typed, every result matches it (or is the offer to browse), at most 12 are
        /// given, none is a conversation the person is not in, none repeats, a muted
        /// conversation never gets the unread bonus, and archived ones come after live ones.
        #[test]
        fn every_result_matches_and_the_rules_hold(
            text in "[a-zA-Z# ]{0,6}",
            visits in prop::collection::vec(0usize..8, 0..6),
            unread in prop::collection::vec((0usize..8, 0u32..4, any::<bool>()), 0..4),
        ) {
            let mut s = switcher();
            let all: Vec<ConversationId> = s.driver.core.conversations().iter().map(|c| c.id).collect();
            for (n, v) in visits.iter().enumerate() {
                s.driver.send(Input::SelectConversation(all[v % all.len()]));
                let _ = n;
            }
            for (index, count, muted) in &unread {
                let id = all[index % all.len()];
                s.driver.send(s.world.event_at(
                    11 + *index as i64,
                    Event::UserStateChanged {
                        state: lazy_protocol::ConversationUserState {
                            conversation_id: id,
                            favorite: false,
                            last_read: seq(0),
                            unread_count: *count,
                            mention_count: 0,
                        },
                    },
                ));
                if *muted {
                    s.driver.send(Input::SetConversationNotificationPrefs {
                        conversation: id,
                        level: None,
                        muted: true,
                    });
                }
            }
            let items = s.driver.core.quick_switcher_results(&text);
            prop_assert!(items.len() <= QUICK_SWITCHER_LIMIT);
            let mut targets = std::collections::HashSet::new();
            let query = text.trim();
            for (index, item) in items.iter().enumerate() {
                prop_assert!(targets.insert(format!("{:?}", item.target)), "a result twice");
                match &item.target {
                    QuickSwitcherTarget::BrowseChannels { query: q } => {
                        prop_assert_eq!(q.as_str(), query);
                        prop_assert_eq!(index, items.len() - 1);
                        prop_assert!(!query.is_empty(), "offered only for text");
                    }
                    QuickSwitcherTarget::Conversation(id) => {
                        let conversation = s.driver.core.conversation(*id);
                        prop_assert!(conversation.is_some(), "a conversation the person is not in");
                        prop_assert_eq!(item.archived, conversation.unwrap().archived);
                        if !query.is_empty() {
                            let bare = item.title.trim_start_matches('#');
                            let username_match = item.subtitle.as_deref().is_some_and(|sub| {
                                fuzzy_match(query, sub.trim_start_matches('@')).is_some()
                            });
                            prop_assert!(fuzzy_match(query, bare).is_some() || username_match,
                                "{} does not match {query:?}", item.title);
                        }
                    }
                    other => {
                        if !query.is_empty() {
                            let by_title = fuzzy_match(query, &item.title).is_some();
                            let by_username = item.subtitle.as_deref().is_some_and(|sub| {
                                fuzzy_match(query, sub.trim_start_matches('@')).is_some()
                            });
                            prop_assert!(by_title || by_username, "{other:?} does not match {query:?}");
                        }
                    }
                }
                for position in &item.matched {
                    prop_assert!(*position < item.title.chars().count());
                }
                prop_assert!(item.matched.windows(2).all(|w| w[0] < w[1]));
            }
            // Archived after live (the offer to browse aside).
            let body: Vec<_> = items
                .iter()
                .filter(|i| !matches!(i.target, QuickSwitcherTarget::BrowseChannels { .. }))
                .collect();
            let first_archived = body.iter().position(|i| i.archived);
            if let Some(at) = first_archived {
                prop_assert!(body[at..].iter().all(|i| i.archived));
            }
        }
    }
}
