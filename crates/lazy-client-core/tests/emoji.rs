//! Custom emoji (slice 3B, D18): the list, `:name:` in messages, reactions, recent emoji, images,
//! and the admin actions.

mod common;

use std::path::PathBuf;

use common::{Driver, World, seq};
use lazy_client_core::{
    ActionKind, ActionOutcome, ActionState, Change, Effect, EmojiPrefs, FileBytes, FileSource,
    ImageData, ImageState, Input, LocalFile, Output, RequestError, StartupState, TransferError,
};
use lazy_domain::{
    ConversationId, EmojiId, MessageDocument, MessageId, ReactionEmoji, Timestamp, WorkspaceRole,
};
use lazy_protocol::{
    ApiError, CustomEmoji, CustomEmojiResponse, ErrorCode, Event, Message, MessageResponse,
    ServerFrame,
};

// ----- Helpers -----

fn custom(world: &World, name: &str) -> CustomEmoji {
    CustomEmoji {
        id: EmojiId::generate(),
        workspace_id: world.workspace.id,
        name: name.to_owned(),
        created_by: world.other.id,
        created_at: Timestamp::now(),
        deleted: false,
    }
}

fn deleted(emoji: &CustomEmoji) -> CustomEmoji {
    CustomEmoji {
        deleted: true,
        ..emoji.clone()
    }
}

struct Fx {
    world: World,
    driver: Driver,
    general: ConversationId,
}

/// A signed-in person with the given emoji in the workspace.
fn setup_with(role: WorkspaceRole, emoji: Vec<CustomEmoji>) -> Fx {
    let mut world = World::new();
    world.role = role;
    world.custom_emoji = emoji;
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(general, vec![], false);
    Fx {
        world,
        driver,
        general,
    }
}

fn setup(names: &[&str]) -> (Fx, Vec<CustomEmoji>) {
    let probe = World::new();
    let emoji: Vec<CustomEmoji> = names.iter().map(|name| custom(&probe, name)).collect();
    // The emoji must belong to the workspace of the fixture, so they are made for its id.
    let mut world = World::new();
    let emoji: Vec<CustomEmoji> = emoji
        .into_iter()
        .map(|e| CustomEmoji {
            workspace_id: world.workspace.id,
            ..e
        })
        .collect();
    world.custom_emoji = emoji.clone();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(general, vec![], false);
    (
        Fx {
            world,
            driver,
            general,
        },
        emoji,
    )
}

fn names(core: &lazy_client_core::ClientCore) -> Vec<String> {
    core.custom_emoji().iter().map(|e| e.name.clone()).collect()
}

fn persisted_document(out: &Output) -> MessageDocument {
    out.effects
        .iter()
        .find_map(|effect| match effect {
            Effect::PersistSubmission(entry) => Some(entry.document.clone()),
            _ => None,
        })
        .expect("a journal entry")
}

fn change_event(fx: &Fx, at: i64, emoji: &CustomEmoji) -> Input {
    fx.world.event_at(
        at,
        Event::EmojiChanged {
            emoji: emoji.clone(),
        },
    )
}

fn image(len: usize) -> ImageData {
    ImageData::new(vec![9; len])
}

fn rejected(code: ErrorCode) -> RequestError {
    RequestError::Rejected(ApiError::new(code, "no"))
}

fn lost() -> TransferError {
    TransferError::Request(RequestError::Unknown("timed out".into()))
}

fn fetches(out: &Output) -> Vec<EmojiId> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::FetchEmojiImage { emoji } => Some(*emoji),
            _ => None,
        })
        .collect()
}

fn png_file(len: usize) -> LocalFile {
    LocalFile {
        source: FileSource::Bytes(FileBytes::new(vec![1; len])),
        name: "party.png".to_owned(),
    }
}

fn path_file() -> LocalFile {
    LocalFile {
        source: FileSource::Path(PathBuf::from("/home/jules/party.png")),
        name: "party.png".to_owned(),
    }
}

fn uploads(out: &Output) -> Vec<(EmojiId, String)> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::UploadEmoji { emoji, name, .. } => Some((*emoji, name.as_str().to_owned())),
            _ => None,
        })
        .collect()
}

fn state(fx: &Fx, kind: ActionKind) -> ActionState {
    fx.driver.core.action(kind).clone()
}

fn failed_text(fx: &Fx, kind: ActionKind) -> String {
    match state(fx, kind) {
        ActionState::Failed(text) => text,
        other => panic!("expected a failure, got {other:?}"),
    }
}

// ----- The list -----

#[test]
fn the_bootstrap_seeds_the_list_ordered_by_name_with_deleted_ones_kept() {
    let (fx, emoji) = setup(&["party_parrot", "blob", "zed", "old_one"]);
    let mut fx = fx;
    // One of them is deleted.
    fx.driver.send(change_event(&fx, 11, &deleted(&emoji[3])));
    let core = &fx.driver.core;
    assert_eq!(names(core), vec!["blob", "old_one", "party_parrot", "zed"]);
    assert_eq!(
        core.live_custom_emoji()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        vec!["blob", "party_parrot", "zed"]
    );
    assert_eq!(core.custom_emoji_by_id(emoji[0].id), Some(&emoji[0]));
    // A deleted emoji keeps its name for display; one never heard of has none.
    assert_eq!(core.custom_emoji_label(emoji[3].id), Some("old_one"));
    assert_eq!(core.custom_emoji_label(EmojiId::generate()), None);
}

#[test]
fn the_picker_search_ranks_names_that_start_with_the_query_before_those_that_contain_it() {
    let (fx, _) = setup(&[
        "blob",
        "big_blob",
        "party_blob",
        "blobby",
        "other",
        "deadblob",
    ]);
    let core = &fx.driver.core;
    let found = |query: &str, limit: usize| -> Vec<String> {
        core.custom_emoji_search(query, limit)
            .iter()
            .map(|e| e.name.clone())
            .collect()
    };
    // Prefix matches first, each group by name.
    assert_eq!(
        found("blob", 10),
        vec!["blob", "blobby", "big_blob", "deadblob", "party_blob"]
    );
    assert_eq!(found("BLOB", 2), vec!["blob", "blobby"]);
    assert_eq!(found(":blob:", 1), vec!["blob"]);
    // Nothing matches, and an empty query is everything.
    assert!(found("renamed_one", 10).is_empty());
    assert_eq!(found("", 100).len(), 6);
    // Deleted ones are not found.
    let (mut fx, emoji) = setup(&["blob", "blobby"]);
    fx.driver.send(change_event(&fx, 11, &deleted(&emoji[0])));
    assert_eq!(
        fx.driver
            .core
            .custom_emoji_search("blob", 10)
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        vec!["blobby"]
    );
}

#[test]
fn autocomplete_after_a_colon_offers_live_names_by_prefix_only() {
    let (mut fx, emoji) = setup(&["na_na", "name", "nap", "banana", "named_one"]);
    let core = &fx.driver.core;
    let offered = |prefix: &str, limit: usize| -> Vec<String> {
        core.custom_emoji_suggestions(prefix, limit)
            .iter()
            .map(|e| e.name.clone())
            .collect()
    };
    assert_eq!(offered("na", 10), vec!["na_na", "name", "named_one", "nap"]);
    assert_eq!(offered("na", 2), vec!["na_na", "name"]);
    assert_eq!(offered("NAM", 10), vec!["name", "named_one"]);
    assert_eq!(offered(":nam", 10), vec!["name", "named_one"]);
    // A name merely containing it is not offered, and nothing is for an empty prefix.
    assert!(!offered("ana", 10).contains(&"banana".to_owned()));
    assert!(offered("", 10).is_empty());
    // Deleted names are not offered.
    fx.driver.send(change_event(&fx, 11, &deleted(&emoji[1])));
    assert_eq!(
        fx.driver
            .core
            .custom_emoji_suggestions("nam", 10)
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        vec!["named_one"]
    );
}

#[test]
fn events_add_rename_and_delete_by_identity_and_a_deletion_is_final() {
    let (mut fx, emoji) = setup(&["blob"]);
    // A new one.
    let fresh = custom(&fx.world, "wave");
    let out = fx.driver.send(change_event(&fx, 11, &fresh));
    assert!(out.changes.contains(&Change::CustomEmojiChanged));
    assert_eq!(names(&fx.driver.core), vec!["blob", "wave"]);
    // Renamed: the same id under another name, the list in order again.
    let renamed = CustomEmoji {
        name: "aaa".to_owned(),
        ..emoji[0].clone()
    };
    fx.driver.send(change_event(&fx, 12, &renamed));
    assert_eq!(names(&fx.driver.core), vec!["aaa", "wave"]);
    assert_eq!(fx.driver.core.custom_emoji_label(emoji[0].id), Some("aaa"));
    // The same event again changes nothing.
    let out = fx.driver.send(change_event(&fx, 13, &renamed));
    assert!(!out.changes.contains(&Change::CustomEmojiChanged));
    // Deleted: still listed, marked, and a late older event does not bring it back.
    fx.driver.send(change_event(&fx, 14, &deleted(&renamed)));
    assert!(
        fx.driver
            .core
            .custom_emoji_by_id(emoji[0].id)
            .unwrap()
            .deleted
    );
    fx.driver.send(change_event(&fx, 15, &emoji[0]));
    assert!(
        fx.driver
            .core
            .custom_emoji_by_id(emoji[0].id)
            .unwrap()
            .deleted
    );
    assert_eq!(fx.driver.core.live_custom_emoji().count(), 1);
}

#[test]
fn an_event_for_another_workspace_is_ignored() {
    let (mut fx, _) = setup(&["blob"]);
    let foreign = CustomEmoji {
        workspace_id: lazy_domain::WorkspaceId::generate(),
        ..custom(&fx.world, "alien")
    };
    fx.driver.send(change_event(&fx, 11, &foreign));
    assert_eq!(names(&fx.driver.core), vec!["blob"]);
}

#[test]
fn a_fresh_bootstrap_replaces_the_list_and_drops_images_of_emoji_it_no_longer_lists() {
    let (mut fx, emoji) = setup(&["blob", "wave"]);
    fx.driver.send(Input::LoadEmojiImage(emoji[1].id));
    fx.driver.send(Input::EmojiImageLoaded {
        emoji: emoji[1].id,
        result: Ok(image(4)),
    });
    assert!(matches!(
        fx.driver.core.emoji_image(emoji[1].id),
        Some(ImageState::Ready(_))
    ));
    fx.driver.send(Input::Frame(ServerFrame::SnapshotRequired {
        workspace_id: fx.world.workspace.id,
        reason: lazy_protocol::SnapshotReason::CursorExpired,
    }));
    fx.world.custom_emoji = vec![emoji[0].clone()];
    fx.driver
        .send(Input::BootstrapLoaded(Box::new(fx.world.bootstrap())));
    assert_eq!(names(&fx.driver.core), vec!["blob"]);
    assert_eq!(fx.driver.core.emoji_image(emoji[1].id), None);
}

#[test]
fn the_list_is_cached_and_a_cache_without_it_still_reads() {
    let (mut fx, emoji) = setup(&["blob", "wave"]);
    fx.driver.send(change_event(&fx, 11, &deleted(&emoji[1])));
    let cache = fx.driver.last_cache().expect("a cache was saved").clone();
    assert_eq!(cache.workspaces[0].custom_emoji.len(), 2);
    // A restart from the cache has the list at once, deleted ones included.
    let mut restarted = Driver::new();
    restarted.send(Input::Started(StartupState {
        session_restored: true,
        cache: Some(cache.clone()),
        ..Default::default()
    }));
    assert_eq!(names(&restarted.core), vec!["blob", "wave"]);
    assert!(
        restarted
            .core
            .custom_emoji_by_id(emoji[1].id)
            .unwrap()
            .deleted
    );
    // A cache written before the field existed has none.
    let mut json = serde_json::to_value(&cache).unwrap();
    for workspace in json["workspaces"].as_array_mut().unwrap() {
        workspace.as_object_mut().unwrap().remove("custom_emoji");
    }
    let old: lazy_client_core::ContentCache = serde_json::from_value(json).unwrap();
    assert!(old.workspaces[0].custom_emoji.is_empty());
}

#[test]
fn only_admins_and_owners_can_manage_emoji() {
    for (role, can) in [
        (WorkspaceRole::Member, false),
        (WorkspaceRole::Admin, true),
        (WorkspaceRole::Owner, true),
    ] {
        let fx = setup_with(role, vec![]);
        assert_eq!(fx.driver.core.can_manage_emoji(), can, "{role:?}");
    }
}

// ----- Messages -----

#[test]
fn a_named_emoji_in_text_becomes_an_emoji_in_the_message_exactly_when_it_is_live() {
    let (mut fx, emoji) = setup(&["party_parrot", "gone"]);
    fx.driver.send(change_event(&fx, 11, &deleted(&emoji[1])));
    let send = |fx: &mut Fx, text: &str| {
        fx.driver.send(Input::SendMessage {
            conversation: fx.general,
            text: text.to_owned(),
        })
    };
    // Live: one emoji, named by its identity.
    let document = persisted_document(&send(&mut fx, "hi :party_parrot: and :party_parrot:"));
    assert_eq!(document.custom_emoji(), vec![emoji[0].id]);
    assert_eq!(
        document.plain_text(),
        "hi :party_parrot: and :party_parrot:"
    );
    // Case does not matter to the directory, and the name is kept as the directory has it.
    let document = persisted_document(&send(&mut fx, ":Party_Parrot:"));
    assert_eq!(document.custom_emoji(), vec![emoji[0].id]);
    // Deleted, unknown, or escaped: plain text.
    for text in [":gone:", ":nobody:", "\\:party_parrot:"] {
        let document = persisted_document(&send(&mut fx, text));
        assert!(document.custom_emoji().is_empty(), "{text}");
    }
    // A standard shortcode is still the standard emoji.
    let document = persisted_document(&send(&mut fx, ":tada:"));
    assert!(document.custom_emoji().is_empty());
    assert_eq!(document.plain_text(), "🎉");
}

#[test]
fn replies_and_edits_resolve_names_too_and_nothing_waits_for_a_list_of_people() {
    let (mut fx, emoji) = setup(&["blob"]);
    let general = fx.general;
    let theirs = fx.world.message(general, &fx.world.other, 11, "root");
    fx.driver.send(fx.world.event(theirs.clone(), None));
    fx.driver.send(Input::OpenThread {
        conversation: general,
        root: theirs.id,
    });
    let out = fx.driver.send(Input::SendReply {
        conversation: general,
        root: theirs.id,
        text: ":blob:".to_owned(),
        also_in_channel: false,
    });
    assert_eq!(persisted_document(&out).custom_emoji(), vec![emoji[0].id]);

    // An edit of the person's own message.
    let mine = fx.world.message(general, &fx.world.me, 12, "plain");
    fx.driver.send(fx.world.event(mine.clone(), None));
    let out = fx.driver.send(Input::EditMessage {
        conversation: general,
        message: mine.id,
        text: "now :blob:".to_owned(),
    });
    assert_eq!(persisted_document(&out).custom_emoji(), vec![emoji[0].id]);
}

#[test]
fn editing_writes_an_emoji_back_as_its_name_and_a_literal_colon_name_stays_literal() {
    let (mut fx, emoji) = setup(&["blob"]);
    let general = fx.general;
    let dir = fx.driver.core.mention_directory(general);
    let mut mine = fx.world.message(general, &fx.world.me, 11, "x");
    mine.document = MessageDocument::from_markdown_with("hi :blob: \\:blob:", &dir).unwrap();
    assert_eq!(mine.document.custom_emoji(), vec![emoji[0].id]);
    fx.driver.send(fx.world.event(mine.clone(), None));
    let text = fx.driver.core.edit_text(general, mine.id).unwrap();
    assert_eq!(text, "hi :blob: \\:blob:");
    // Saving the text back unchanged changes nothing: the emoji stays an emoji and the literal
    // stays text.
    let out = fx.driver.send(Input::EditMessage {
        conversation: general,
        message: mine.id,
        text,
    });
    assert!(out.effects.is_empty(), "{out:?}");
    // Renamed meanwhile, the document still names the emoji by identity and reads as typed.
    fx.driver.send(change_event(
        &fx,
        12,
        &CustomEmoji {
            name: "blobby".to_owned(),
            ..emoji[0].clone()
        },
    ));
    assert_eq!(
        fx.driver.core.edit_text(general, mine.id).unwrap(),
        "hi :blob: \\:blob:"
    );
}

#[test]
fn a_message_with_an_emoji_reads_safely_everywhere_text_is_read() {
    let (mut fx, emoji) = setup(&["blob"]);
    let general = fx.general;
    let dir = fx.driver.core.mention_directory(general);
    let mut theirs = fx.world.message(general, &fx.world.other, 11, "x");
    theirs.document = MessageDocument::from_markdown_with(":blob:", &dir).unwrap();
    fx.driver.send(fx.world.event(theirs.clone(), None));
    let excerpt = fx.driver.core.message_excerpt(general, theirs.id).unwrap();
    assert_eq!(excerpt.text, ":blob:");
    assert_eq!(
        fx.driver
            .core
            .user_state(general)
            .map(|state| state.unread_count),
        Some(1)
    );
    let _ = emoji;
}

// ----- Reactions -----

fn react(fx: &mut Fx, message: &Message, emoji: ReactionEmoji) -> Output {
    fx.driver.send(Input::ToggleReaction {
        conversation: fx.general,
        message: message.id,
        emoji,
    })
}

fn set_reactions(out: &Output) -> Vec<(ReactionEmoji, bool)> {
    out.effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::SetReaction { request, .. } => Some((request.emoji, request.present)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_custom_emoji_reaction_works_like_a_standard_one() {
    let (mut fx, emoji) = setup(&["blob"]);
    let general = fx.general;
    let theirs = fx.world.message(general, &fx.world.other, 11, "words");
    fx.driver.send(fx.world.event(theirs.clone(), None));
    let blob = ReactionEmoji::custom(emoji[0].id);
    let out = react(&mut fx, &theirs, blob);
    assert_eq!(set_reactions(&out), vec![(blob, true)]);
    // It is shown at once, as the person's.
    let row = fx.driver.timeline(general)[0].clone();
    assert_eq!(row.reactions.len(), 1);
    assert_eq!(row.reactions[0].emoji, blob);
    assert!(row.reacted_by(fx.world.me.id, blob));
    // The answer carries the custom id and the list keeps it.
    let mut after = theirs.clone();
    after.reactions = row.reactions.clone();
    after.reactions_as_of = seq(12);
    fx.driver.send(Input::ReactionResult {
        message: theirs.id,
        emoji: blob,
        result: Ok(MessageResponse { message: after }),
    });
    assert_eq!(fx.driver.timeline(general)[0].reactions[0].emoji, blob);
}

#[test]
fn a_deleted_emoji_cannot_be_newly_used_but_a_reaction_with_it_can_be_withdrawn() {
    let (mut fx, emoji) = setup(&["blob"]);
    let general = fx.general;
    let blob = ReactionEmoji::custom(emoji[0].id);
    let mut theirs = fx.world.message(general, &fx.world.other, 11, "words");
    theirs.reactions = vec![lazy_protocol::ReactionSummary {
        emoji: blob,
        users: vec![fx.world.me.id],
    }];
    fx.driver.send(fx.world.event(theirs.clone(), None));
    fx.driver.send(change_event(&fx, 12, &deleted(&emoji[0])));
    // Withdrawing is allowed...
    let out = react(&mut fx, &theirs, blob);
    assert_eq!(set_reactions(&out), vec![(blob, false)]);
    // ...adding it afresh is not, and neither is an emoji nobody has heard of.
    let other = fx.world.message(general, &fx.world.other, 13, "more words");
    fx.driver.send(fx.world.event(other.clone(), None));
    let out = react(&mut fx, &other, blob);
    assert!(set_reactions(&out).is_empty());
    assert_eq!(
        fx.driver.core.notice(),
        Some("That emoji has been deleted, so it can't be used.")
    );
    let out = react(&mut fx, &other, ReactionEmoji::custom(EmojiId::generate()));
    assert!(set_reactions(&out).is_empty());
}

#[test]
fn custom_emoji_count_among_the_fifty_kinds_a_message_may_hold() {
    let names: Vec<String> = (0..51).map(|n| format!("emoji_{n:02}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let (mut fx, emoji) = setup(&names);
    let general = fx.general;
    let mut theirs = fx.world.message(general, &fx.world.other, 11, "words");
    theirs.reactions = emoji[..50]
        .iter()
        .map(|e| lazy_protocol::ReactionSummary {
            emoji: ReactionEmoji::custom(e.id),
            users: vec![fx.world.other.id],
        })
        .collect();
    fx.driver.send(fx.world.event(theirs.clone(), None));
    let out = react(&mut fx, &theirs, ReactionEmoji::custom(emoji[50].id));
    assert!(set_reactions(&out).is_empty());
    assert!(fx.driver.core.notice().is_some());
    // One that is already there is fine.
    let out = react(&mut fx, &theirs, ReactionEmoji::custom(emoji[3].id));
    assert_eq!(set_reactions(&out).len(), 1);
}

// ----- Recently used emoji -----

#[test]
fn recent_custom_emoji_show_only_while_they_are_live_and_the_saved_list_keeps_the_rest() {
    let (mut fx, emoji) = setup(&["blob", "wave"]);
    let blob = ReactionEmoji::custom(emoji[0].id);
    let wave = ReactionEmoji::custom(emoji[1].id);
    let tada = ReactionEmoji::parse("🎉").unwrap();
    for e in [wave, tada, blob] {
        fx.driver.send(Input::RecordEmojiUse(e));
    }
    assert_eq!(fx.driver.core.emoji_prefs().recent, vec![blob, tada, wave]);
    // One is deleted: it leaves what is shown, and the change is announced.
    let out = fx.driver.send(change_event(&fx, 11, &deleted(&emoji[1])));
    assert!(out.changes.contains(&Change::EmojiPrefsChanged));
    assert_eq!(fx.driver.core.emoji_prefs().recent, vec![blob, tada]);
    // What is saved still has it, until pushed out.
    let out = fx.driver.send(Input::RecordEmojiUse(tada));
    let saved = out.effects.iter().find_map(|effect| match effect {
        Effect::PersistEmojiPrefs(prefs) => Some(prefs.clone()),
        _ => None,
    });
    assert_eq!(saved.unwrap().recent, vec![tada, blob, wave]);
}

#[test]
fn saved_recents_naming_an_emoji_the_core_has_not_heard_of_are_hidden_not_lost() {
    let probe = World::new();
    let stranger = ReactionEmoji::custom(EmojiId::generate());
    let tada = ReactionEmoji::parse("🎉").unwrap();
    let mut world = World::new();
    world.custom_emoji = vec![];
    let mut driver = Driver::new();
    driver.send(Input::Started(StartupState {
        session_restored: true,
        emoji_prefs: EmojiPrefs {
            skin_tone: 2,
            recent: vec![stranger, tada],
        },
        ..Default::default()
    }));
    driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
    assert_eq!(driver.core.emoji_prefs().recent, vec![tada]);
    assert_eq!(driver.core.emoji_prefs().skin_tone, 2);
    let out = driver.send(Input::SetSkinTone(3));
    let saved = out.effects.iter().find_map(|effect| match effect {
        Effect::PersistEmojiPrefs(prefs) => Some(prefs.clone()),
        _ => None,
    });
    assert_eq!(saved.unwrap().recent, vec![stranger, tada]);
    let _ = probe;
}

// ----- Images -----

#[test]
fn an_image_is_asked_for_once_and_then_ready_and_a_rename_keeps_it() {
    let (mut fx, emoji) = setup(&["blob"]);
    let id = emoji[0].id;
    let out = fx.driver.send(Input::LoadEmojiImage(id));
    assert_eq!(fetches(&out), vec![id]);
    assert_eq!(fx.driver.core.emoji_image(id), Some(&ImageState::Loading));
    assert!(fx.driver.send(Input::LoadEmojiImage(id)).effects.is_empty());
    let out = fx.driver.send(Input::EmojiImageLoaded {
        emoji: id,
        result: Ok(image(10)),
    });
    assert!(
        out.changes
            .contains(&Change::EmojiImageChanged { emoji: id })
    );
    assert_eq!(
        fx.driver.core.emoji_image(id),
        Some(&ImageState::Ready(image(10)))
    );
    assert!(fx.driver.send(Input::LoadEmojiImage(id)).effects.is_empty());
    // Renamed: the picture is the same.
    fx.driver.send(change_event(
        &fx,
        11,
        &CustomEmoji {
            name: "renamed".to_owned(),
            ..emoji[0].clone()
        },
    ));
    assert!(matches!(
        fx.driver.core.emoji_image(id),
        Some(ImageState::Ready(_))
    ));
}

#[test]
fn a_deleted_emoji_loses_its_image_and_cannot_be_asked_for() {
    let (mut fx, emoji) = setup(&["blob"]);
    let id = emoji[0].id;
    fx.driver.send(Input::LoadEmojiImage(id));
    fx.driver.send(Input::EmojiImageLoaded {
        emoji: id,
        result: Ok(image(10)),
    });
    let out = fx.driver.send(change_event(&fx, 11, &deleted(&emoji[0])));
    assert!(
        out.effects
            .contains(&Effect::ForgetEmojiImages { emojis: vec![id] })
    );
    assert_eq!(fx.driver.core.emoji_image(id), None);
    // Not asked for again, and neither is one nobody knows.
    assert!(fx.driver.send(Input::LoadEmojiImage(id)).effects.is_empty());
    assert!(
        fx.driver
            .send(Input::LoadEmojiImage(EmojiId::generate()))
            .effects
            .is_empty()
    );
}

#[test]
fn an_answer_for_an_emoji_deleted_while_it_loaded_is_ignored() {
    let (mut fx, emoji) = setup(&["blob"]);
    let id = emoji[0].id;
    fx.driver.send(Input::LoadEmojiImage(id));
    fx.driver.send(change_event(&fx, 11, &deleted(&emoji[0])));
    fx.driver.send(Input::EmojiImageLoaded {
        emoji: id,
        result: Ok(image(10)),
    });
    assert_eq!(fx.driver.core.emoji_image(id), None);
}

#[test]
fn a_failed_image_stays_failed_until_the_connection_comes_back() {
    let (mut fx, emoji) = setup(&["blob"]);
    let id = emoji[0].id;
    fx.driver.send(Input::LoadEmojiImage(id));
    fx.driver.send(Input::EmojiImageLoaded {
        emoji: id,
        result: Err(lost()),
    });
    assert_eq!(fx.driver.core.emoji_image(id), Some(&ImageState::Failed));
    assert!(fx.driver.send(Input::LoadEmojiImage(id)).effects.is_empty());
    let out = fx.driver.send(Input::Frame(ServerFrame::CaughtUp {
        workspace_id: fx.world.workspace.id,
        seq: seq(10),
    }));
    assert!(
        out.changes
            .contains(&Change::EmojiImageChanged { emoji: id })
    );
    assert_eq!(fx.driver.core.emoji_image(id), None);
    assert_eq!(
        fetches(&fx.driver.send(Input::LoadEmojiImage(id))),
        vec![id]
    );
}

#[test]
fn only_four_images_are_fetched_at_a_time_and_the_held_ones_are_bounded() {
    let names: Vec<String> = (0..7).map(|n| format!("emoji_{n}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let (mut fx, emoji) = setup(&names);
    let mut started = Vec::new();
    for e in &emoji {
        started.extend(fetches(&fx.driver.send(Input::LoadEmojiImage(e.id))));
    }
    assert_eq!(started.len(), 4);
    let out = fx.driver.send(Input::EmojiImageLoaded {
        emoji: emoji[0].id,
        result: Ok(image(1)),
    });
    assert_eq!(fetches(&out), vec![emoji[4].id]);
    // By bytes: three images of a third of the bound, and the oldest goes.
    let big = lazy_client_core::MAX_HELD_EMOJI_IMAGE_BYTES / 3 + 1;
    for e in &emoji[1..4] {
        fx.driver.send(Input::EmojiImageLoaded {
            emoji: e.id,
            result: Ok(image(big)),
        });
    }
    // 1 byte + 3 big > the bound: the oldest ready one is dropped.
    assert_eq!(fx.driver.core.emoji_image(emoji[0].id), None);
    assert!(matches!(
        fx.driver.core.emoji_image(emoji[3].id),
        Some(ImageState::Ready(_))
    ));
}

// ----- Admin actions -----

fn admin(names: &[&str]) -> (Fx, Vec<CustomEmoji>) {
    let mut world = World::new();
    world.role = WorkspaceRole::Admin;
    let emoji: Vec<CustomEmoji> = names.iter().map(|name| custom(&world, name)).collect();
    world.custom_emoji = emoji.clone();
    let general = world.channel.id;
    let mut driver = Driver::ready(&world);
    driver.load_history(general, vec![], false);
    (
        Fx {
            world,
            driver,
            general,
        },
        emoji,
    )
}

fn add(fx: &mut Fx, name: &str, file: LocalFile) -> Output {
    fx.driver.send(Input::AddCustomEmoji {
        name: name.to_owned(),
        file,
    })
}

#[test]
fn adding_uploads_the_image_under_a_new_id_and_the_answer_adds_it_to_the_list() {
    let (mut fx, _) = admin(&["blob"]);
    let kind = ActionKind::AddCustomEmoji;
    let out = add(&mut fx, " :Party_Time: ", png_file(2000));
    let started = uploads(&out);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].1, "party_time");
    assert_eq!(state(&fx, kind), ActionState::InFlight);
    // A second add while this one runs is ignored.
    assert!(add(&mut fx, "other", png_file(10)).effects.is_empty());
    let id = started[0].0;
    let created = CustomEmoji {
        id,
        ..custom(&fx.world, "party_time")
    };
    let out = fx.driver.send(Input::CustomEmojiUploaded {
        emoji: id,
        result: Ok(CustomEmojiResponse { emoji: created }),
    });
    assert!(out.changes.contains(&Change::CustomEmojiChanged));
    assert_eq!(state(&fx, kind), ActionState::Idle);
    assert_eq!(names(&fx.driver.core), vec!["blob", "party_time"]);
    // And the next add is free to start.
    assert_eq!(uploads(&add(&mut fx, "again", png_file(10))).len(), 1);
}

#[test]
fn an_add_is_refused_before_asking_when_it_cannot_work() {
    let (mut fx, _) = admin(&["blob"]);
    let kind = ActionKind::AddCustomEmoji;
    let long = "a".repeat(33);
    let cases: Vec<(&str, LocalFile, &str)> = vec![
        (
            "x",
            png_file(10),
            "Give the emoji a name of at least 2 characters.",
        ),
        (
            "has space",
            png_file(10),
            "An emoji name can only use lowercase letters, numbers, and underscores.",
        ),
        (
            "_leading",
            png_file(10),
            "An emoji name must start and end with a letter or number, and can't be the name of a standard emoji.",
        ),
        (
            "tada",
            png_file(10),
            "An emoji name must start and end with a letter or number, and can't be the name of a standard emoji.",
        ),
        (
            &long,
            png_file(10),
            "An emoji name can be at most 32 characters.",
        ),
        (
            "blob",
            png_file(10),
            "There's already an emoji named :blob:.",
        ),
        (
            "fine_name",
            png_file(600 * 1024),
            "That image is 600 KiB; an emoji image can be at most 512 KiB.",
        ),
    ];
    for (name, file, text) in cases {
        let out = add(&mut fx, name, file);
        assert!(uploads(&out).is_empty(), "{name}");
        assert_eq!(failed_text(&fx, kind), text, "{name}");
        fx.driver.send(Input::DismissAction(kind));
        assert_eq!(state(&fx, kind), ActionState::Idle);
    }
    // A name a deleted emoji used is free.
    let gone = custom(&fx.world, "free_now");
    fx.driver.send(change_event(&fx, 11, &deleted(&gone)));
    assert_eq!(uploads(&add(&mut fx, "free_now", png_file(10))).len(), 1);
}

#[test]
fn a_member_cannot_add_rename_or_delete() {
    let mut fx = setup_with(WorkspaceRole::Member, vec![]);
    let emoji = custom(&fx.world, "blob");
    fx.driver.send(change_event(&fx, 11, &emoji));
    for out in [
        add(&mut fx, "new_one", png_file(10)),
        fx.driver.send(Input::RenameCustomEmoji {
            id: emoji.id,
            name: "other".into(),
        }),
        fx.driver.send(Input::DeleteCustomEmoji(emoji.id)),
    ] {
        assert!(out.effects.is_empty());
    }
    assert_eq!(
        failed_text(&fx, ActionKind::AddCustomEmoji),
        "Only admins and owners can manage custom emoji."
    );
}

#[test]
fn the_limit_of_five_hundred_live_emoji_is_checked_before_asking() {
    let names: Vec<String> = (0..500).map(|n| format!("emoji_{n:03}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let (mut fx, _) = admin(&names);
    let out = add(&mut fx, "one_more", png_file(10));
    assert!(uploads(&out).is_empty());
    assert_eq!(
        failed_text(&fx, ActionKind::AddCustomEmoji),
        "A workspace can have at most 500 custom emoji."
    );
}

#[test]
fn an_upload_of_unknown_outcome_is_repeated_with_the_same_id_then_given_up() {
    let (mut fx, _) = admin(&[]);
    let kind = ActionKind::AddCustomEmoji;
    let id = uploads(&add(&mut fx, "party_time", path_file()))[0].0;
    let lost_answer = |fx: &mut Fx| {
        fx.driver.send(Input::CustomEmojiUploaded {
            emoji: id,
            result: Err(lost()),
        })
    };
    assert_eq!(
        uploads(&lost_answer(&mut fx)),
        vec![(id, "party_time".to_owned())]
    );
    assert_eq!(uploads(&lost_answer(&mut fx)).len(), 1);
    // The third unknown outcome is the last.
    assert!(uploads(&lost_answer(&mut fx)).is_empty());
    assert_eq!(
        failed_text(&fx, kind),
        "Couldn't reach the server. Check your connection and try again."
    );
    // Adding again is a new emoji with a new id.
    let again = uploads(&add(&mut fx, "party_time", path_file()));
    assert_ne!(again[0].0, id);
    // A late answer for the abandoned one changes nothing.
    fx.driver.send(Input::CustomEmojiUploaded {
        emoji: id,
        result: Ok(CustomEmojiResponse {
            emoji: CustomEmoji {
                id,
                ..custom(&fx.world, "party_time")
            },
        }),
    });
    assert!(names(&fx.driver.core).is_empty());
}

#[test]
fn the_server_refusing_an_add_says_why() {
    let cases = [
        (
            ErrorCode::NameTaken,
            "There's already an emoji with that name.",
        ),
        (
            ErrorCode::LimitReached,
            "A workspace can have at most 500 custom emoji.",
        ),
        (
            ErrorCode::UnsupportedMedia,
            "That image can't be used. Use a PNG, JPEG, GIF, or WebP picture.",
        ),
        (
            ErrorCode::PayloadTooLarge,
            "That image is too large. An emoji image can be at most 512 KiB.",
        ),
        (
            ErrorCode::Forbidden,
            "Only admins and owners can manage custom emoji.",
        ),
    ];
    for (code, text) in cases {
        let (mut fx, _) = admin(&[]);
        let id = uploads(&add(&mut fx, "party_time", path_file()))[0].0;
        let out = fx.driver.send(Input::CustomEmojiUploaded {
            emoji: id,
            result: Err(TransferError::Request(rejected(code))),
        });
        // A refusal is not repeated.
        assert!(uploads(&out).is_empty(), "{code:?}");
        assert_eq!(
            failed_text(&fx, ActionKind::AddCustomEmoji),
            text,
            "{code:?}"
        );
    }
}

#[test]
fn a_file_the_runtime_cannot_use_is_reported_in_words() {
    let (mut fx, _) = admin(&[]);
    let id = uploads(&add(&mut fx, "party_time", path_file()))[0].0;
    fx.driver.send(Input::CustomEmojiUploaded {
        emoji: id,
        result: Err(TransferError::TooLarge { size: 700 * 1024 }),
    });
    assert_eq!(
        failed_text(&fx, ActionKind::AddCustomEmoji),
        "That image is 700 KiB; an emoji image can be at most 512 KiB."
    );
    let id = uploads(&add(&mut fx, "party_time", path_file()))[0].0;
    fx.driver.send(Input::CustomEmojiUploaded {
        emoji: id,
        result: Err(TransferError::Unreadable("Permission denied".into())),
    });
    assert_eq!(
        failed_text(&fx, ActionKind::AddCustomEmoji),
        "The image couldn't be read: Permission denied."
    );
}

#[test]
fn an_expired_session_during_an_add_ends_the_session() {
    let (mut fx, _) = admin(&[]);
    let id = uploads(&add(&mut fx, "party_time", path_file()))[0].0;
    fx.driver.send(Input::CustomEmojiUploaded {
        emoji: id,
        result: Err(TransferError::Request(rejected(ErrorCode::Unauthenticated))),
    });
    assert_eq!(state(&fx, ActionKind::AddCustomEmoji), ActionState::Idle);
    assert!(matches!(
        fx.driver.core.phase(),
        lazy_client_core::SessionPhase::SignedOut { .. }
    ));
}

#[test]
fn renaming_asks_checks_the_name_and_applies_the_answer() {
    let (mut fx, emoji) = admin(&["blob", "waving"]);
    let id = emoji[0].id;
    let kind = ActionKind::RenameCustomEmoji(id);
    // Refused before asking.
    for (name, text) in [
        ("waving", "There's already an emoji named :waving:."),
        ("x", "Give the emoji a name of at least 2 characters."),
    ] {
        let out = fx.driver.send(Input::RenameCustomEmoji {
            id,
            name: name.into(),
        });
        assert!(out.effects.is_empty());
        assert_eq!(failed_text(&fx, kind), text);
    }
    // The same name is nothing to do.
    let out = fx.driver.send(Input::RenameCustomEmoji {
        id,
        name: "BLOB".into(),
    });
    assert!(out.effects.is_empty());
    // A real rename.
    let out = fx.driver.send(Input::RenameCustomEmoji {
        id,
        name: " :renamed_one: ".into(),
    });
    let request = out
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::RenameEmoji { emoji, request } => {
                Some((*emoji, request.name.as_str().to_owned()))
            }
            _ => None,
        })
        .expect("a rename request");
    assert_eq!(request, (id, "renamed_one".to_owned()));
    assert_eq!(state(&fx, kind), ActionState::InFlight);
    let renamed = CustomEmoji {
        name: "renamed_one".to_owned(),
        ..emoji[0].clone()
    };
    fx.driver.send(Input::ActionResult {
        kind,
        result: Ok(ActionOutcome::CustomEmoji(renamed)),
    });
    assert_eq!(state(&fx, kind), ActionState::Idle);
    assert_eq!(names(&fx.driver.core), vec!["renamed_one", "waving"]);
    // The server refusing says why.
    fx.driver.send(Input::RenameCustomEmoji {
        id,
        name: "taken_now".into(),
    });
    fx.driver.send(Input::ActionResult {
        kind,
        result: Err(rejected(ErrorCode::NameTaken)),
    });
    assert_eq!(
        failed_text(&fx, kind),
        "There's already an emoji with that name."
    );
}

#[test]
fn deleting_marks_the_emoji_deleted_drops_its_image_and_a_second_delete_is_gone_already() {
    let (mut fx, emoji) = admin(&["blob"]);
    let id = emoji[0].id;
    let kind = ActionKind::DeleteCustomEmoji(id);
    fx.driver.send(Input::LoadEmojiImage(id));
    fx.driver.send(Input::EmojiImageLoaded {
        emoji: id,
        result: Ok(image(10)),
    });
    let out = fx.driver.send(Input::DeleteCustomEmoji(id));
    assert!(out.effects.contains(&Effect::DeleteEmoji { emoji: id }));
    // A double click sends once.
    assert!(
        fx.driver
            .send(Input::DeleteCustomEmoji(id))
            .effects
            .is_empty()
    );
    let out = fx.driver.send(Input::ActionResult {
        kind,
        result: Ok(ActionOutcome::Done),
    });
    assert!(
        out.effects
            .contains(&Effect::ForgetEmojiImages { emojis: vec![id] })
    );
    assert!(fx.driver.core.custom_emoji_by_id(id).unwrap().deleted);
    assert_eq!(fx.driver.core.emoji_image(id), None);
    assert_eq!(state(&fx, kind), ActionState::Idle);
    // Already deleted: nothing to ask.
    assert!(
        fx.driver
            .send(Input::DeleteCustomEmoji(id))
            .effects
            .is_empty()
    );
}

#[test]
fn a_delete_the_server_says_is_not_found_leaves_the_emoji_deleted() {
    let (mut fx, emoji) = admin(&["blob"]);
    let id = emoji[0].id;
    let kind = ActionKind::DeleteCustomEmoji(id);
    fx.driver.send(Input::DeleteCustomEmoji(id));
    fx.driver.send(Input::ActionResult {
        kind,
        result: Err(rejected(ErrorCode::NotFound)),
    });
    assert!(fx.driver.core.custom_emoji_by_id(id).unwrap().deleted);
    assert_eq!(state(&fx, kind), ActionState::Idle);
    // Other failures keep the emoji and say why.
    let (mut fx, emoji) = admin(&["blob"]);
    let id = emoji[0].id;
    let kind = ActionKind::DeleteCustomEmoji(id);
    fx.driver.send(Input::DeleteCustomEmoji(id));
    fx.driver.send(Input::ActionResult {
        kind,
        result: Err(rejected(ErrorCode::Forbidden)),
    });
    assert!(!fx.driver.core.custom_emoji_by_id(id).unwrap().deleted);
    assert_eq!(
        failed_text(&fx, kind),
        "Only admins and owners can manage custom emoji."
    );
}

#[test]
fn signing_out_forgets_the_emoji_and_their_images() {
    let (mut fx, emoji) = admin(&["blob"]);
    let id = emoji[0].id;
    fx.driver.send(Input::LoadEmojiImage(id));
    fx.driver.send(Input::EmojiImageLoaded {
        emoji: id,
        result: Ok(image(10)),
    });
    fx.driver.send(Input::SignOut);
    assert!(fx.driver.core.custom_emoji().is_empty());
    assert_eq!(fx.driver.core.emoji_image(id), None);
    assert!(fx.driver.core.emoji_prefs().recent.is_empty());
}

#[allow(dead_code)]
fn unused(_: MessageId) {}
