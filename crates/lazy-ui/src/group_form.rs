//! Checking what is typed into the user group editor, as the person types.
//!
//! These explain a problem in words before Save is pressed; the client core and the server still
//! validate. The rules are the domain's (`GroupHandle`, `DisplayName`), so what passes here is what
//! the server accepts, bar a handle that is already taken, which only the server can say.

use lazy_domain::mention::{MAX_HANDLE_LEN, MIN_HANDLE_LEN, handle_is_reserved};
use lazy_domain::{
    DisplayName, GroupHandle, MAX_GROUP_MEMBERS, NameError, UserId, mention::handle_is_valid,
};

/// What the handle field holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleCheck {
    /// Nothing typed yet.
    Empty,
    /// A usable handle. `changed` says what was typed differs from it: a leading `@` was taken off
    /// or capitals were lowercased.
    Valid { handle: GroupHandle, changed: bool },
    /// Not usable, and why.
    Problem(String),
}

/// Checks the handle field. A leading `@` is allowed (people type it) and capitals are lowercased.
pub fn check_handle(input: &str) -> HandleCheck {
    let typed = input.trim();
    let bare = typed.strip_prefix('@').unwrap_or(typed);
    if bare.is_empty() {
        return HandleCheck::Empty;
    }
    if handle_is_reserved(bare) {
        return HandleCheck::Problem(format!(
            "“{}” is reserved. Choose another handle.",
            bare.to_ascii_lowercase()
        ));
    }
    match GroupHandle::parse(bare) {
        Ok(handle) => HandleCheck::Valid {
            changed: handle.as_str() != typed,
            handle,
        },
        Err(error) => HandleCheck::Problem(match error {
            NameError::TooShort { .. } => {
                format!("A handle needs at least {MIN_HANDLE_LEN} characters.")
            }
            NameError::TooLong { .. } => {
                format!("A handle can have at most {MAX_HANDLE_LEN} characters.")
            }
            NameError::InvalidCharacter { found, .. } => format!(
                "A handle can't contain {found:?}. Use letters, digits, dots, underscores, and dashes."
            ),
            NameError::InvalidStart { .. } => {
                debug_assert!(!handle_is_valid(&bare.to_ascii_lowercase()));
                "A handle must start and end with a letter or digit.".to_owned()
            }
        }),
    }
}

/// What the name field holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameCheck {
    Empty,
    Valid(DisplayName),
    Problem(String),
}

/// Checks the name field.
pub fn check_name(input: &str) -> NameCheck {
    if input.trim().is_empty() {
        return NameCheck::Empty;
    }
    match DisplayName::parse(input) {
        Ok(name) => NameCheck::Valid(name),
        Err(NameError::TooLong { max, .. }) => {
            NameCheck::Problem(format!("A name can have at most {max} characters."))
        }
        Err(_) => NameCheck::Problem("That name has a character that can't be used.".to_owned()),
    }
}

/// A group the editor can save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupDraft {
    pub handle: GroupHandle,
    pub name: DisplayName,
    pub members: Vec<UserId>,
}

/// What stops Save, in words, or the group to save. `None` for the problem while a field is still
/// empty: Save is simply disabled and nothing is said yet.
pub fn draft(handle: &str, name: &str, members: &[UserId]) -> Result<GroupDraft, Option<String>> {
    let handle = match check_handle(handle) {
        HandleCheck::Valid { handle, .. } => handle,
        HandleCheck::Empty => return Err(None),
        HandleCheck::Problem(why) => return Err(Some(why)),
    };
    let name = match check_name(name) {
        NameCheck::Valid(name) => name,
        NameCheck::Empty => return Err(None),
        NameCheck::Problem(why) => return Err(Some(why)),
    };
    if members.len() > MAX_GROUP_MEMBERS {
        return Err(Some(format!(
            "A group can have at most {MAX_GROUP_MEMBERS} people."
        )));
    }
    Ok(GroupDraft {
        handle,
        name,
        members: members.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(input: &str) -> String {
        match check_handle(input) {
            HandleCheck::Problem(why) => why,
            other => panic!("{input:?}: {other:?}"),
        }
    }

    #[test]
    fn a_good_handle_is_valid_and_says_when_it_was_adjusted() {
        assert_eq!(
            check_handle("design-team"),
            HandleCheck::Valid {
                handle: GroupHandle::parse("design-team").unwrap(),
                changed: false
            }
        );
        let HandleCheck::Valid { handle, changed } = check_handle("@Design-Team") else {
            panic!()
        };
        assert_eq!(handle.as_str(), "design-team");
        assert!(changed, "the at sign and capitals were taken off");
    }

    #[test]
    fn nothing_typed_is_empty_not_a_problem() {
        assert_eq!(check_handle(""), HandleCheck::Empty);
        assert_eq!(check_handle("  "), HandleCheck::Empty);
        assert_eq!(check_handle("@"), HandleCheck::Empty);
        assert_eq!(check_name("  "), NameCheck::Empty);
    }

    #[test]
    fn every_rule_has_its_own_explanation() {
        assert!(problem("a").contains("at least 2"));
        assert!(problem(&"a".repeat(33)).contains("at most 32"));
        assert!(problem("design team").contains("' '"));
        assert!(problem("café").contains("é"));
        assert!(problem("-design").contains("start and end"));
        assert!(problem("design.").contains("start and end"));
        assert!(problem("channel").contains("reserved"));
        assert!(problem("@Here").contains("“here” is reserved"));
        assert!(problem("everyone").contains("reserved"));
    }

    #[test]
    fn a_name_is_checked_against_the_display_name_rules() {
        assert!(matches!(check_name("Design Team"), NameCheck::Valid(_)));
        assert!(matches!(check_name(&"n".repeat(65)), NameCheck::Problem(_)));
        assert!(matches!(check_name("a\u{7}b"), NameCheck::Problem(_)));
    }

    #[test]
    fn a_draft_needs_both_fields_and_a_sane_membership() {
        let people: Vec<UserId> = (0..3).map(|_| UserId::generate()).collect();
        let ok = draft("design", "Design Team", &people).unwrap();
        assert_eq!(ok.handle.as_str(), "design");
        assert_eq!(ok.members, people);
        assert_eq!(draft("", "Design", &people), Err(None));
        assert_eq!(draft("design", "", &people), Err(None));
        assert!(matches!(draft("x", "Design", &people), Err(Some(_))));
        // A group with nobody in it is allowed.
        assert!(draft("design", "Design", &[]).is_ok());
        let too_many: Vec<UserId> = (0..=MAX_GROUP_MEMBERS)
            .map(|_| UserId::generate())
            .collect();
        assert!(matches!(
            draft("design", "Design", &too_many),
            Err(Some(why)) if why.contains("200")
        ));
    }
}
