//! The words for states a person can be left looking at that the views own: a load that failed,
//! a list still being fetched. They live here, apart from the views, so the wording can be
//! checked as text; each is paired with an action in the view that shows it, and a test that the
//! action does what it says.
//!
//! Words that come from the client core (notices, the text of a failed action) are not repeated
//! here: the core owns those, and the views only place them.

/// The label of the button that asks again after a failed load.
pub const TRY_AGAIN: &str = "Try again";

/// Ends `detail` as a sentence, so it can follow another sentence.
pub fn sentence(detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() || detail.ends_with(['.', '!', '?']) {
        detail.to_owned()
    } else {
        format!("{detail}.")
    }
}

/// The headline over an empty timeline whose first page of history failed to load.
pub fn history_failed_title(thread: bool) -> &'static str {
    if thread {
        "The thread didn't load"
    } else {
        "The messages didn't load"
    }
}

/// The bar over a timeline that has messages but could not fetch more: the ones before them, or
/// (in a thread, where the root is always there) the replies.
pub fn older_history_failed(thread: bool, detail: &str) -> String {
    let what = if thread {
        "The replies didn't load."
    } else {
        "Earlier messages didn't load."
    };
    format!("{what} {}", sentence(detail))
}

/// The line over a channel's list of integrations that could not be read.
pub fn integrations_failed(detail: &str) -> String {
    format!("The integrations didn't load. {}", sentence(detail))
}

/// The hint under the composer while someone is typing an `@` in a channel whose people have not
/// arrived yet, so the list of people may be short.
pub fn members_loading(channel: &str) -> String {
    format!("Still loading who is in #{channel}, so some people may be missing from the list.")
}

// ----- Custom emoji -----

/// What the name field of the custom emoji form makes of what was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmojiNameCheck {
    /// Nothing typed: no complaint yet, but nothing to save.
    Empty,
    /// A name that can be saved (without colons, lowercased).
    Good(String),
    /// Why what was typed cannot be used.
    Problem(String),
}

/// Checks a custom emoji name as it is typed: the domain's rules, then whether another live emoji
/// already has it. `taken` is the names of the other live emoji (without colons). Colons around the
/// name and capitals are forgiven, as the client core forgives them.
pub fn check_emoji_name(typed: &str, taken: &[&str]) -> EmojiNameCheck {
    use lazy_domain::{EmojiName, NameError};
    let name = typed.trim().trim_matches(':');
    if name.is_empty() {
        return EmojiNameCheck::Empty;
    }
    match EmojiName::parse(name) {
        Ok(parsed) => {
            if taken.iter().any(|other| *other == parsed.as_str()) {
                EmojiNameCheck::Problem(format!("There's already an emoji named :{parsed}:."))
            } else {
                EmojiNameCheck::Good(parsed.to_string())
            }
        }
        Err(NameError::TooShort { .. }) => {
            EmojiNameCheck::Problem("A name needs at least 2 characters.".to_owned())
        }
        Err(NameError::TooLong { .. }) => {
            EmojiNameCheck::Problem("A name can have at most 32 characters.".to_owned())
        }
        Err(NameError::InvalidCharacter { found, .. }) => EmojiNameCheck::Problem(format!(
            "A name can only use letters, digits, and underscores, not {}.",
            if found == ' ' {
                "a space".to_owned()
            } else {
                format!("“{found}”")
            }
        )),
        Err(NameError::InvalidStart { .. }) => {
            let lowered = name.to_ascii_lowercase();
            if lazy_domain::emoji::by_shortcode(&lowered).is_some() {
                EmojiNameCheck::Problem(format!(
                    "{lowered} is the name of a standard emoji (:{lowered}:), so it can't be used."
                ))
            } else {
                EmojiNameCheck::Problem(
                    "A name must start and end with a letter or digit.".to_owned(),
                )
            }
        }
    }
}

/// The name a chosen file suggests: its stem, lowercased, with anything a name cannot hold turned
/// into an underscore.
pub fn emoji_name_from_file(file_name: &str) -> String {
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem);
    let cleaned: String = stem
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect();
    cleaned.trim_matches('_').chars().take(32).collect()
}

/// The line under an empty list of custom emoji.
pub const NO_CUSTOM_EMOJI: &str =
    "No custom emoji yet. Add one to use it in messages and reactions.";

/// The note in the picker's Custom section when there are none, for someone who cannot add any.
pub const NO_CUSTOM_EMOJI_PICKER: &str = "No custom emoji yet.";

/// What a row says when its picture could not be fetched.
pub const EMOJI_PICTURE_UNAVAILABLE: &str = "Picture unavailable";

/// What deleting an emoji does, for the confirmation.
pub fn delete_emoji_prompt(name: &str) -> String {
    format!(
        "Delete :{name}:? Messages and reactions that used it keep showing :{name}: as plain text, \
         and the name becomes free for a new emoji."
    )
}

// ----- Search -----

/// What the results area says before anything has been searched for and there are no recent
/// searches to offer.
pub const SEARCH_IDLE: &str = "Type to search messages and files. Try from:alice or in:design.";

/// The filters, as a hint under the empty search field.
pub const SEARCH_FILTER_HINT: &str = "Narrow a search with from:alice, in:design, after:2026-09-01, \
     before:2026-10-01, has:file, or has:image. Put \"quotes\" around a phrase and a - before a \
     word to leave it out.";

/// The heading over the list of recent searches.
pub const RECENT_SEARCHES: &str = "Recent searches";

/// The note above results that were ranked among only the newest matches.
pub const SEARCH_TRUNCATED: &str =
    "Showing the best of the newest matches. Narrow your search to see older ones.";

/// The note after the last result when a search can page no further.
pub const SEARCH_AT_LIMIT: &str =
    "You've reached the end of what search can show. Narrow your search to see more.";

/// The label of the button that asks for the next page.
pub const SEARCH_SHOW_MORE: &str = "Show more";

/// The line in place of the "Show more" button while the next page loads.
pub const SEARCH_LOADING_MORE: &str = "Loading more results…";

/// The note while a chosen result is being looked for (the message may not be loaded yet).
pub const SEARCH_OPENING: &str = "Looking for the message…";

/// The headline over a first page that failed.
pub const SEARCH_FAILED_TITLE: &str = "The search didn't finish";

/// Why a query that parses badly cannot be searched, in words that end the sentence: the
/// parser's own text, capitalized.
pub fn search_error_text(error: &lazy_domain::search::SearchError) -> String {
    let text = error.to_string();
    let mut chars = text.chars();
    let capitalized = chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect::<String>())
        .unwrap_or_default();
    sentence(&capitalized)
}

/// What a search with no results says, naming the filters that were in it (`from:alice`) so the
/// person knows what to remove.
pub fn search_no_results(typed: &str, filters: &[String]) -> String {
    let mut text = format!(
        "No results for “{}”. Try fewer words, check spelling, or remove a filter.",
        typed.trim()
    );
    if !filters.is_empty() {
        text.push_str(&format!(" Filters in use: {}.", filters.join(", ")));
    }
    text
}

/// What to do about a first page that failed, after the core's reason: a failed connection is
/// worth asking again, the rest the reason already explains.
pub fn search_failed_hint(reason: &str) -> Option<&'static str> {
    reason
        .starts_with("Couldn't reach")
        .then_some("Check your connection and try again.")
}

/// The line beside "Try again" when the next page failed to load.
pub fn search_more_failed(reason: &str) -> String {
    format!("More results didn't load. {}", sentence(reason))
}

// ----- Notifications, status, and the thread inbox -----

/// What the preferences dialog says when notifications are not paused.
pub const NOT_PAUSED: &str = "Notifications are on.";

/// What it says while quiet hours hold them back and nothing else has paused them.
pub const PAUSED_BY_QUIET_HOURS: &str = "Notifications are paused during your quiet hours.";

/// What it says while they are paused for a while, with the end in the person's time.
pub fn paused_until(when: &str) -> String {
    format!("Notifications are paused until {when}.")
}

/// What it says when the pause was "until I turn it back on".
pub fn paused_until_resumed() -> String {
    "Notifications are paused until you turn them back on.".to_owned()
}

/// The tooltip beside the person's name while notifications are paused.
pub fn paused_tooltip(when: Option<&str>) -> String {
    match when {
        Some(when) => format!("Notifications paused until {when}"),
        None => "Notifications paused".to_owned(),
    }
}

pub const QUIET_BAD_TIME: &str = "Write a time like 22:00: hours and minutes, on a 24-hour clock.";
pub const QUIET_SAME_TIME: &str = "The start and the end can't be the same time.";
pub const QUIET_NEEDS_A_DAY: &str = "Pick at least one day, or turn quiet hours off.";
pub const QUIET_ABOUT: &str =
    "During these hours nothing pops up, but unread messages and mentions still count.";

/// The status text over its limit, as the counter reports it.
pub fn status_too_long(max: usize) -> String {
    format!("A status can be at most {max} characters.")
}

/// What the thread inbox says when the person follows no threads.
pub const INBOX_EMPTY: &str = "You're not following any threads yet. You follow a thread when you reply to it, start it and someone replies, or are mentioned in it.";

/// What it says with "Unread" chosen and nothing is unread.
pub const INBOX_NO_UNREAD: &str = "No unread threads.";

/// The headline over a first page of threads that failed.
pub const INBOX_FAILED_TITLE: &str = "The threads didn't load";

/// What a row shows for a thread whose first message was deleted.
pub const INBOX_DELETED_ROOT: &str = "This message was deleted";

/// The line beside "Try again" when the next page of threads failed to load.
pub fn inbox_more_failed(reason: &str) -> String {
    format!("More threads didn't load. {}", sentence(reason))
}

/// How many replies a thread has, for its row.
pub fn replies_label(count: u32) -> String {
    if count == 1 {
        "1 reply".to_owned()
    } else {
        format!("{count} replies")
    }
}

// ----- Discovery: the channel directory, people, the quick switcher, saved items -----

/// What the channel directory says when the workspace has no public channels at all.
pub const BROWSE_EMPTY: &str =
    "There are no public channels to browse yet. Create one from the sidebar.";

/// What it says when the filters leave nothing: `query` is what was typed, `not_joined` whether
/// "Not joined" is chosen.
pub fn browse_no_match(query: &str, not_joined: bool) -> String {
    let query = query.trim();
    match (query.is_empty(), not_joined) {
        (true, true) => "You're in every public channel.".to_owned(),
        (true, false) => BROWSE_EMPTY.to_owned(),
        (false, true) => format!("No channel you haven't joined matches \u{201c}{query}\u{201d}."),
        (false, false) => format!("No channels match \u{201c}{query}\u{201d}."),
    }
}

/// The headline over a first page of channels that failed.
pub const BROWSE_FAILED_TITLE: &str = "The channels didn't load";

/// The line beside "Try again" when the next page of channels failed to load.
pub fn browse_more_failed(reason: &str) -> String {
    format!("More channels didn't load. {}", sentence(reason))
}

/// What the people directory says when nobody matches `query`.
pub fn people_no_match(query: &str) -> String {
    let query = query.trim();
    if query.is_empty() {
        "There is no one here yet.".to_owned()
    } else {
        format!("No one matches \u{201c}{query}\u{201d}.")
    }
}

/// The headline over a first page of people that failed.
pub const PEOPLE_FAILED_TITLE: &str = "The people didn't load";

/// The line beside "Try again" when the next page of people failed to load.
pub fn people_more_failed(reason: &str) -> String {
    format!("More people didn't load. {}", sentence(reason))
}

/// What a profile says when the full profile (role and join date) could not be fetched: the
/// name and status the member list already holds are still shown above it.
pub fn profile_failed(reason: &str) -> String {
    format!("The rest of this profile didn't load. {}", sentence(reason))
}

/// What a profile says about a person who has left the workspace or never was in it.
pub const PROFILE_GONE: &str = "This person is no longer in the workspace.";

/// The badge beside an admin's or owner's name; members have none.
pub fn role_label(role: lazy_domain::WorkspaceRole) -> Option<&'static str> {
    match role {
        lazy_domain::WorkspaceRole::Owner => Some("Owner"),
        lazy_domain::WorkspaceRole::Admin => Some("Admin"),
        lazy_domain::WorkspaceRole::Member => None,
    }
}

/// "Member since March 14, 2025", from the day they joined in the person's zone.
pub fn member_since(day: jiff::civil::Date) -> String {
    format!("Member since {}", day.strftime("%B %-d, %Y"))
}

/// The quick switcher's field, and what it says when nothing matches `query`.
pub const QUICK_PLACEHOLDER: &str = "Jump to a conversation, a person, or a place";
pub fn quick_no_match(query: &str) -> String {
    format!("Nothing matches \u{201c}{}\u{201d}.", query.trim())
}

/// The sidebar's hint for the quick switcher, with the key this platform uses.
pub fn quick_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd+K"
    } else {
        "Ctrl+K"
    }
}

/// What the saved list says when nothing is saved.
pub const SAVED_EMPTY: &str =
    "Nothing saved yet. Choose \u{201c}Save for later\u{201d} on a message to keep it here.";

/// The headline over a first page of saved messages that failed.
pub const SAVED_FAILED_TITLE: &str = "The saved messages didn't load";

/// The line beside "Try again" when the next page of saved messages failed to load.
pub fn saved_more_failed(reason: &str) -> String {
    format!("More saved messages didn't load. {}", sentence(reason))
}

/// The message actions' words for saving.
pub const SAVE_FOR_LATER: &str = "Save for later";
pub const REMOVE_FROM_SAVED: &str = "Remove from saved";

// ----- Administration: workspace settings, invitations, roles, deactivation (D22) -----

/// The names of the two registration policies, as the settings dialog offers them.
pub const REGISTRATION_OPEN: &str = "Anyone can register";
pub const REGISTRATION_INVITE_ONLY: &str = "Invitation only";
/// What each policy means, under the control.
pub const REGISTRATION_OPEN_DETAIL: &str =
    "Anyone who can reach the server can create an account and join.";
pub const REGISTRATION_INVITE_ONLY_DETAIL: &str =
    "A new account needs an invitation code from an admin.";

/// The names of the two channel-creation policies.
pub const CHANNELS_EVERYONE: &str = "Everyone";
pub const CHANNELS_ADMINS_ONLY: &str = "Admins only";

/// What the new-channel control says when the workspace lets only admins create channels.
pub const CHANNELS_ADMINS_ONLY_NOTE: &str = "Only admins can create channels in this workspace.";

/// The line under the name field when the text is not a usable workspace name: it names the
/// limit instead of echoing the domain's error.
pub fn workspace_name_problem(error: &lazy_domain::NameError) -> String {
    match error {
        lazy_domain::NameError::TooShort { .. } => "Give the workspace a name.".to_owned(),
        lazy_domain::NameError::TooLong { max, .. } => {
            format!("A workspace name can be at most {max} characters.")
        }
        lazy_domain::NameError::InvalidCharacter { .. }
        | lazy_domain::NameError::InvalidStart { .. } => {
            "A workspace name can't contain control characters.".to_owned()
        }
    }
}

/// What the settings dialog says once a change is saved.
pub const SETTINGS_SAVED: &str = "Saved.";

/// What the invitations dialog says when there are none.
pub const INVITATIONS_EMPTY: &str =
    "No invitations yet. Create one to let someone join without an open registration.";

/// The headline over a list of invitations that failed to load.
pub const INVITATIONS_FAILED_TITLE: &str = "The invitations didn't load";

/// What the panel over a new invitation says about its code.
pub const INVITE_SHOWN_ONCE: &str =
    "This code is shown only once. Copy it now and send it to the person you are inviting.";

/// What the panel says after Copy.
pub const INVITE_COPIED: &str = "Copied.";

/// What revoking an invitation asks, and what it does.
pub fn revoke_invitation_question() -> &'static str {
    "Revoke this invitation?"
}
pub const REVOKE_INVITATION_DETAIL: &str =
    "The code stops working at once. People who already joined with it stay.";

/// The question over a deactivation, naming the person.
pub fn deactivate_question(name: &str) -> String {
    format!("Deactivate {name}?")
}

/// What deactivating does, in one sentence a person can weigh.
pub fn deactivate_detail(name: &str, workspace: &str) -> String {
    format!(
        "{name} loses access to {workspace} at once and is signed out of it. Their messages and \
         profile stay. You can reactivate them later and everything comes back."
    )
}

/// The question over a reactivation, naming the person.
pub fn reactivate_question(name: &str) -> String {
    format!("Reactivate {name}?")
}

/// What reactivating does.
pub fn reactivate_detail(name: &str) -> String {
    format!(
        "{name} can sign in to the workspace again, with their conversations and role as they were."
    )
}

/// The badge beside a deactivated person.
pub const DEACTIVATED: &str = "Deactivated";

/// The words on the people directory's toggle for admins.
pub const SHOW_DEACTIVATED: &str = "Show deactivated";

/// The tooltip and note on a deactivated person's row or profile.
pub const DEACTIVATED_NOTE: &str =
    "Deactivated: they can't sign in to this workspace. Their old messages stay.";

/// What the Profile section says about a display name that cannot be saved.
pub fn display_name_problem(error: &lazy_domain::NameError) -> String {
    match error {
        lazy_domain::NameError::TooShort { .. } => "Give yourself a name.".to_owned(),
        lazy_domain::NameError::TooLong { max, .. } => {
            format!("A display name can be at most {max} characters.")
        }
        lazy_domain::NameError::InvalidCharacter { .. }
        | lazy_domain::NameError::InvalidStart { .. } => {
            "A display name can't contain control characters.".to_owned()
        }
    }
}

/// The formats and size an avatar may be, under "Choose picture…".
pub const AVATAR_FORMATS: &str = "PNG, JPEG, WebP or GIF, up to 5 MB. It is cropped to a square.";

/// What the reduced-motion control says about "System".
pub const MOTION_NOTE: &str = "Reduced motion stops every animation. Lazy can't read the system's own setting yet, so \u{201c}System\u{201d} keeps animations on.";

/// The register form's optional field, and what it says when the code was refused.
pub const INVITE_CODE_LABEL: &str = "Invitation code (optional)";
pub const INVITE_CODE_PLACEHOLDER: &str = "Paste the code you were given";

/// What the screen of a person who lost access says.
pub fn no_access_title(workspace: &str) -> String {
    format!("You no longer have access to {workspace}")
}
/// What the screen says when the server lists no workspace for the person and the client does
/// not know which one it was (after a restart with nothing saved).
pub const NO_WORKSPACE_TITLE: &str = "You don't have access to a workspace";
pub const NO_WORKSPACE_DETAIL: &str = "Your membership may have been deactivated. Ask an admin to reactivate you, then check again. Your account is untouched.";
pub const NO_ACCESS_DETAIL: &str = "An admin deactivated your membership. If that is a mistake, ask an admin to reactivate you, then check again. Your account is untouched.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_names_are_checked_as_they_are_typed() {
        use EmojiNameCheck::*;
        assert_eq!(check_emoji_name("", &[]), Empty);
        assert_eq!(check_emoji_name("  ::  ", &[]), Empty);
        assert_eq!(
            check_emoji_name(":Party_Parrot:", &[]),
            Good("party_parrot".to_owned())
        );
        assert!(matches!(check_emoji_name("a", &[]), Problem(text) if text.contains("at least 2")));
        let long = "a".repeat(33);
        assert!(
            matches!(check_emoji_name(&long, &[]), Problem(text) if text.contains("at most 32"))
        );
        assert!(
            matches!(check_emoji_name("two words", &[]), Problem(text) if text.contains("a space"))
        );
        assert!(
            matches!(check_emoji_name("caf\u{e9}", &[]), Problem(text) if text.contains("digits, and underscores"))
        );
        assert!(
            matches!(check_emoji_name("_hidden", &[]), Problem(text) if text.contains("start and end"))
        );
        // A standard shortcode is never an emoji name.
        assert!(
            matches!(check_emoji_name("tada", &[]), Problem(text) if text.contains("standard emoji"))
        );
        // A live emoji already has it.
        assert!(matches!(
            check_emoji_name("shipit", &["shipit", "other"]),
            Problem(text) if text == "There's already an emoji named :shipit:."
        ));
        assert_eq!(
            check_emoji_name("shipit", &["other"]),
            Good("shipit".to_owned())
        );
    }

    #[test]
    fn a_file_name_suggests_a_usable_emoji_name() {
        assert_eq!(emoji_name_from_file("Party Parrot.gif"), "party_parrot");
        assert_eq!(emoji_name_from_file("ship-it!.png"), "ship_it");
        assert_eq!(emoji_name_from_file("..png"), "");
        assert_eq!(
            emoji_name_from_file(&format!("{}.png", "x".repeat(50))).len(),
            32
        );
    }

    #[test]
    fn the_delete_prompt_says_what_stays() {
        let text = delete_emoji_prompt("shipit");
        assert!(text.contains(":shipit:"), "{text}");
        assert!(text.contains("keep showing"), "{text}");
        assert!(text.contains("name becomes free"), "{text}");
    }

    #[test]
    fn sentences_end_once() {
        assert_eq!(sentence("not found"), "not found.");
        assert_eq!(
            sentence("Couldn't reach the server (x)."),
            "Couldn't reach the server (x)."
        );
        assert_eq!(sentence("  Try later!  "), "Try later!");
        assert_eq!(sentence(""), "");
    }

    #[test]
    fn a_failed_first_page_names_what_did_not_load() {
        assert!(history_failed_title(false).contains("messages"));
        assert!(history_failed_title(true).contains("thread"));
    }

    #[test]
    fn a_failed_older_page_says_messages_are_missing_above_and_why() {
        let text = older_history_failed(false, "Couldn't reach the server (timed out)");
        assert!(text.starts_with("Earlier messages didn't load."), "{text}");
        assert!(text.ends_with("(timed out)."), "{text}");
        let thread = older_history_failed(true, "not found");
        assert_eq!(thread, "The replies didn't load. not found.");
    }

    #[test]
    fn a_failed_integrations_list_says_so_before_the_reason() {
        let text = integrations_failed("Couldn't reach the server");
        assert_eq!(
            text,
            "The integrations didn't load. Couldn't reach the server."
        );
    }

    #[test]
    fn loading_people_names_the_channel_and_the_consequence() {
        let text = members_loading("general");
        assert!(text.contains("#general"));
        assert!(text.contains("missing"));
    }

    #[test]
    fn a_search_that_cannot_run_says_why_in_a_sentence() {
        use lazy_domain::search::SearchQuery;
        let text = |typed: &str| search_error_text(&SearchQuery::parse(typed).unwrap_err());
        assert_eq!(text("!!!"), "Type something to search for.");
        assert_eq!(
            text("after:soon"),
            "\"soon\" is not a date; write it as year-month-day, like 2026-10-01."
        );
        assert_eq!(
            text("has:video"),
            "Has: can be file or image, not \"video\"."
        );
        assert!(text(&"x".repeat(300)).starts_with("A search can be at most 200 characters"));
    }

    #[test]
    fn no_results_says_what_to_try_and_names_the_filters() {
        assert_eq!(
            search_no_results(" deploy ", &[]),
            "No results for “deploy”. Try fewer words, check spelling, or remove a filter."
        );
        let with = search_no_results(
            "deploy from:alice in:ops",
            &["from:alice".into(), "in:ops".into()],
        );
        assert!(
            with.ends_with("Filters in use: from:alice, in:ops."),
            "{with}"
        );
    }

    #[test]
    fn a_failed_search_hints_at_the_connection_only_when_that_was_the_problem() {
        assert!(search_failed_hint("Couldn't reach the server.").is_some());
        assert!(
            search_failed_hint("That search took too long. Narrow it with in: or from:.").is_none()
        );
        assert_eq!(
            search_more_failed("Couldn't reach the server."),
            "More results didn't load. Couldn't reach the server."
        );
    }

    #[test]
    fn the_notes_say_what_to_do() {
        assert!(SEARCH_TRUNCATED.contains("Narrow your search"));
        assert!(SEARCH_AT_LIMIT.contains("Narrow your search"));
        assert!(SEARCH_IDLE.contains("from:alice") && SEARCH_IDLE.contains("in:design"));
    }

    #[test]
    fn pausing_is_worded_for_a_while_forever_and_not_at_all() {
        assert_eq!(
            paused_until("3:30 PM"),
            "Notifications are paused until 3:30 PM."
        );
        assert!(paused_until_resumed().contains("turn them back on"));
        assert_eq!(NOT_PAUSED, "Notifications are on.");
        assert_eq!(
            paused_tooltip(Some("3:30 PM")),
            "Notifications paused until 3:30 PM"
        );
        assert_eq!(paused_tooltip(None), "Notifications paused");
    }

    #[test]
    fn the_inbox_says_what_makes_a_thread_followed_and_that_nothing_is_unread() {
        assert!(INBOX_EMPTY.contains("reply to it") && INBOX_EMPTY.contains("mentioned"));
        assert_eq!(INBOX_NO_UNREAD, "No unread threads.");
        assert_eq!(replies_label(1), "1 reply");
        assert_eq!(replies_label(12), "12 replies");
        assert_eq!(
            inbox_more_failed("Couldn't reach the server."),
            "More threads didn't load. Couldn't reach the server."
        );
    }

    #[test]
    fn quiet_hours_problems_say_what_to_write() {
        assert!(PAUSED_BY_QUIET_HOURS.contains("quiet hours"));
        assert!(QUIET_BAD_TIME.contains("22:00"));
        assert!(QUIET_SAME_TIME.contains("same"));
        assert!(QUIET_NEEDS_A_DAY.contains("one day"));
        assert!(status_too_long(100).contains("100"));
    }

    #[test]
    fn the_directory_says_what_the_filters_left() {
        assert_eq!(browse_no_match("", false), BROWSE_EMPTY);
        assert_eq!(
            browse_no_match(" ", true),
            "You're in every public channel."
        );
        assert_eq!(
            browse_no_match(" des ", false),
            "No channels match \u{201c}des\u{201d}."
        );
        assert!(browse_no_match("des", true).contains("haven't joined"));
        assert_eq!(
            browse_more_failed("Couldn't reach the server"),
            "More channels didn't load. Couldn't reach the server."
        );
    }

    #[test]
    fn people_and_profiles_have_words_for_each_state() {
        assert_eq!(people_no_match("zo"), "No one matches \u{201c}zo\u{201d}.");
        assert!(people_no_match("").contains("no one"));
        assert!(people_more_failed("x").starts_with("More people didn't load."));
        assert!(profile_failed("x").contains("rest of this profile"));
        assert_eq!(role_label(lazy_domain::WorkspaceRole::Owner), Some("Owner"));
        assert_eq!(role_label(lazy_domain::WorkspaceRole::Admin), Some("Admin"));
        assert_eq!(role_label(lazy_domain::WorkspaceRole::Member), None);
        assert_eq!(
            member_since(jiff::civil::date(2025, 3, 4)),
            "Member since March 4, 2025"
        );
    }

    #[test]
    fn the_quick_switcher_and_saved_have_their_words() {
        assert_eq!(
            quick_no_match(" xyz "),
            "Nothing matches \u{201c}xyz\u{201d}."
        );
        assert!(quick_hint().ends_with("+K"));
        assert!(SAVED_EMPTY.contains("Save for later"));
        assert!(saved_more_failed("x").starts_with("More saved messages didn't load."));
        assert_ne!(SAVE_FOR_LATER, REMOVE_FROM_SAVED);
    }

    #[test]
    fn administration_words_name_the_person_and_the_consequence() {
        assert_eq!(deactivate_question("Bob Okafor"), "Deactivate Bob Okafor?");
        assert!(deactivate_detail("Bob", "Lazy").contains("loses access to Lazy at once"));
        assert!(deactivate_detail("Bob", "Lazy").contains("reactivate"));
        assert_eq!(reactivate_question("Bob"), "Reactivate Bob?");
        assert!(reactivate_detail("Bob").starts_with("Bob can sign in"));
        assert_eq!(no_access_title("Lazy"), "You no longer have access to Lazy");
        assert!(INVITE_SHOWN_ONCE.contains("only once"));
        assert_ne!(REGISTRATION_OPEN, REGISTRATION_INVITE_ONLY);
    }

    #[test]
    fn a_bad_workspace_name_says_what_to_change() {
        use lazy_domain::WorkspaceName;
        let problem = |text: &str| workspace_name_problem(&WorkspaceName::parse(text).unwrap_err());
        assert_eq!(problem("   "), "Give the workspace a name.");
        assert!(problem(&"x".repeat(65)).contains("at most 64"));
        assert!(problem("a\u{7}b").contains("control characters"));
    }
}
