//! The command registry (D24): every action the person can start from more than one place.
//!
//! A [`Command`] has an identifier, a label, a group (File, Go, View, Workspace, Help), and an
//! optional shortcut written once, as `secondary-k`, where `secondary` is Ctrl on Linux and
//! Windows and Cmd on a Mac. Whether it can run now is [`availability`], a pure function of one
//! [`AppSnapshot`]: `Enabled`, `Disabled` with a reason the person is shown, or `Hidden`.
//!
//! Buttons, menu items, the quick switcher's `>` list, the keyboard shortcuts dialog, and the key
//! bindings all come from this table and ask the same rule, so a shortcut can never do what the
//! button for it would refuse, a disabled command says why wherever it is shown, and the list of
//! shortcuts cannot drift from the shortcuts that exist.
//!
//! # What a shortcut never does
//!
//! It never runs while a dialog other than its own is open, while an input method has text in
//! composition (the snapshot's `composing`), or on a key a text field needs: every shortcut here
//! is a chord with a modifier the text input does not bind, and a test compares the two tables.

use gpui::{Action, App, KeyBinding};
use lazy_client_core::ClientCore;
use lazy_domain::fuzzy::fuzzy_match;

use crate::keyboard::SHELL_CONTEXT;

/// Runs a command. Key bindings and the native menu bar dispatch it; the shell handles it.
#[derive(Debug, Clone, PartialEq, Action)]
#[action(namespace = lazy_commands, no_json)]
pub struct RunCommand(pub CommandId);

/// What a command is called inside the program and in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandId {
    QuickSwitcher,
    CommandPalette,
    Search,
    Threads,
    Saved,
    People,
    BrowseChannels,
    NewMessage,
    CreateChannel,
    PreviousConversation,
    NextConversation,
    PreviousUnread,
    NextUnread,
    /// The n-th workspace on the rail, 1 to 9.
    SwitchWorkspace(u8),
    AddWorkspace,
    Preferences,
    WorkspaceSettings,
    InvitePeople,
    UserGroups,
    CustomEmoji,
    ConversationDetails,
    ToggleFavorite,
    Shortcuts,
    Diagnostics,
    SignOut,
}

/// The menus the commands are grouped into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    File,
    Go,
    View,
    Workspace,
    Help,
}

impl Group {
    pub const ALL: [Group; 5] = [
        Group::File,
        Group::Go,
        Group::View,
        Group::Workspace,
        Group::Help,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::File => "File",
            Group::Go => "Go",
            Group::View => "View",
            Group::Workspace => "Workspace",
            Group::Help => "Help",
        }
    }
}

/// One entry of the registry.
#[derive(Debug, Clone, Copy)]
pub struct Command {
    pub id: CommandId,
    pub group: Group,
    pub title: &'static str,
    /// The shortcut as `modifier-…-key` with `secondary` for Ctrl (Cmd on a Mac).
    pub shortcut: Option<&'static str>,
}

const fn command(
    id: CommandId,
    group: Group,
    title: &'static str,
    shortcut: Option<&'static str>,
) -> Command {
    Command {
        id,
        group,
        title,
        shortcut,
    }
}

use CommandId as C;
use Group as G;

/// Every command, in the order the menus show them.
const REGISTRY: &[Command] = &[
    // File
    command(C::AddWorkspace, G::File, "Add a workspace…", None),
    command(C::NewMessage, G::File, "New message…", Some("secondary-n")),
    command(C::CreateChannel, G::File, "Create a channel…", None),
    command(C::Preferences, G::File, "Preferences…", Some("secondary-,")),
    command(C::SignOut, G::File, "Sign out", None),
    // Go
    command(
        C::QuickSwitcher,
        G::Go,
        "Quick switcher…",
        Some("secondary-k"),
    ),
    command(
        C::CommandPalette,
        G::Go,
        "Command palette…",
        Some("secondary-shift-p"),
    ),
    command(C::Search, G::Go, "Search messages", Some("secondary-f")),
    command(C::Threads, G::Go, "Threads", Some("secondary-shift-t")),
    command(C::Saved, G::Go, "Saved messages", Some("secondary-shift-s")),
    command(C::People, G::Go, "People", Some("secondary-shift-e")),
    command(
        C::BrowseChannels,
        G::Go,
        "Browse channels…",
        Some("secondary-shift-l"),
    ),
    command(
        C::PreviousConversation,
        G::Go,
        "Previous conversation",
        Some("alt-up"),
    ),
    command(
        C::NextConversation,
        G::Go,
        "Next conversation",
        Some("alt-down"),
    ),
    command(
        C::PreviousUnread,
        G::Go,
        "Previous unread conversation",
        Some("alt-shift-up"),
    ),
    command(
        C::NextUnread,
        G::Go,
        "Next unread conversation",
        Some("alt-shift-down"),
    ),
    command(
        C::SwitchWorkspace(1),
        G::Go,
        "Switch to workspace 1",
        Some("secondary-1"),
    ),
    command(
        C::SwitchWorkspace(2),
        G::Go,
        "Switch to workspace 2",
        Some("secondary-2"),
    ),
    command(
        C::SwitchWorkspace(3),
        G::Go,
        "Switch to workspace 3",
        Some("secondary-3"),
    ),
    command(
        C::SwitchWorkspace(4),
        G::Go,
        "Switch to workspace 4",
        Some("secondary-4"),
    ),
    command(
        C::SwitchWorkspace(5),
        G::Go,
        "Switch to workspace 5",
        Some("secondary-5"),
    ),
    command(
        C::SwitchWorkspace(6),
        G::Go,
        "Switch to workspace 6",
        Some("secondary-6"),
    ),
    command(
        C::SwitchWorkspace(7),
        G::Go,
        "Switch to workspace 7",
        Some("secondary-7"),
    ),
    command(
        C::SwitchWorkspace(8),
        G::Go,
        "Switch to workspace 8",
        Some("secondary-8"),
    ),
    command(
        C::SwitchWorkspace(9),
        G::Go,
        "Switch to workspace 9",
        Some("secondary-9"),
    ),
    // View
    command(
        C::ConversationDetails,
        G::View,
        "Conversation details…",
        None,
    ),
    command(C::ToggleFavorite, G::View, "Favorite or unfavorite", None),
    // Workspace
    command(
        C::WorkspaceSettings,
        G::Workspace,
        "Workspace settings…",
        None,
    ),
    command(C::InvitePeople, G::Workspace, "Invite people…", None),
    command(C::UserGroups, G::Workspace, "User groups…", None),
    command(C::CustomEmoji, G::Workspace, "Custom emoji…", None),
    // Help
    command(
        C::Shortcuts,
        G::Help,
        "Keyboard shortcuts",
        Some("secondary-/"),
    ),
    command(C::Diagnostics, G::Help, "Diagnostics…", None),
];

/// Every command.
pub fn all() -> &'static [Command] {
    REGISTRY
}

/// The registry entry for `id`.
pub fn command_for(id: CommandId) -> &'static Command {
    REGISTRY
        .iter()
        .find(|command| command.id == id)
        .expect("every command id is in the registry")
}

// ----- Shortcuts as text and as keystrokes -----

/// The keystroke GPUI binds on this platform: `secondary` becomes Cmd on a Mac, else Ctrl.
pub fn keystroke(spec: &str) -> String {
    keystroke_for(spec, cfg!(target_os = "macos"))
}

fn keystroke_for(spec: &str, mac: bool) -> String {
    spec.split('-')
        .map(|part| match part {
            "secondary" if mac => "cmd",
            "secondary" => "ctrl",
            other => other,
        })
        .collect::<Vec<_>>()
        .join("-")
    // A "-" key is written out ("secondary-minus" is not used here), and the comma and slash
    // keys are plain characters.
}

/// How a shortcut is written for people: `Ctrl+Shift+T`, or `⇧⌘T` on a Mac.
pub fn shortcut_text(spec: &str) -> String {
    shortcut_text_for(spec, cfg!(target_os = "macos"))
}

fn shortcut_text_for(spec: &str, mac: bool) -> String {
    let parts: Vec<&str> = spec.split('-').collect();
    let (key, modifiers) = parts.split_last().expect("a shortcut has a key");
    let key_text = match (*key, mac) {
        ("up", true) => "↑".to_owned(),
        ("down", true) => "↓".to_owned(),
        ("up", false) => "Up".to_owned(),
        ("down", false) => "Down".to_owned(),
        (other, _) => other.to_uppercase(),
    };
    if mac {
        // Control, Option, Shift, Command: the order the Mac's menus use.
        let mut out = String::new();
        for (name, glyph) in [
            ("ctrl", "⌃"),
            ("alt", "⌥"),
            ("shift", "⇧"),
            ("secondary", "⌘"),
        ] {
            if modifiers.contains(&name) {
                out.push_str(glyph);
            }
        }
        out.push_str(&key_text);
        out
    } else {
        let mut out = String::new();
        for (name, word) in [
            ("secondary", "Ctrl"),
            ("ctrl", "Ctrl"),
            ("alt", "Alt"),
            ("shift", "Shift"),
        ] {
            if modifiers.contains(&name) {
                out.push_str(word);
                out.push('+');
            }
        }
        out.push_str(&key_text);
        out
    }
}

/// The shortcut of `id` as people read it, if it has one.
pub fn shortcut_label(id: CommandId) -> Option<String> {
    command_for(id).shortcut.map(shortcut_text)
}

/// Registers every command's shortcut in the shell's key context. Called once from
/// [`crate::init`].
pub fn bind_shortcuts(cx: &mut App) {
    let bindings: Vec<KeyBinding> = REGISTRY
        .iter()
        .filter_map(|command| {
            command.shortcut.map(|spec| {
                // Bound once at startup: the few dozen strings live as long as the program.
                let keys: &'static str = Box::leak(keystroke(spec).into_boxed_str());
                KeyBinding::new(keys, RunCommand(command.id), Some(SHELL_CONTEXT))
            })
        })
        .collect();
    cx.bind_keys(bindings);
}

// ----- The snapshot and the rule -----

/// What is open over the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogState {
    Closed,
    /// The dialog this command opened: the command may still run (and closes it).
    Own(CommandId),
    /// Any other dialog, picker, or menu.
    Other,
}

/// Which view stands in the conversation's place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainView {
    Conversation,
    Search,
    Threads,
    Saved,
    People,
}

/// Everything availability depends on, read once.
#[derive(Debug, Clone)]
pub struct AppSnapshot {
    pub signed_in: bool,
    pub dialog: DialogState,
    /// An input method has text in composition.
    pub composing: bool,
    pub view: MainView,
    pub conversation_selected: bool,
    pub conversations: usize,
    /// Some conversation other than the open one has unread messages.
    pub other_unread: bool,
    pub workspace_names: Vec<String>,
    /// Which of them is open (0-based).
    pub active_workspace: Option<usize>,
    pub can_manage_workspace: bool,
    pub can_manage_groups: bool,
    pub can_manage_emoji: bool,
    pub can_create_channel: bool,
}

impl AppSnapshot {
    /// What the core knows. The shell fills in `dialog` and `composing`, which it owns.
    pub fn from_core(core: &ClientCore) -> Self {
        let sidebar = core.sidebar();
        let all: Vec<_> = sidebar
            .favorites
            .iter()
            .chain(&sidebar.channels)
            .chain(&sidebar.direct_and_group)
            .copied()
            .collect();
        let selected = core.selected_conversation();
        let view = if core.search().is_some() {
            MainView::Search
        } else if core.inbox().is_some() {
            MainView::Threads
        } else if core.saved().open {
            MainView::Saved
        } else if core.people().open {
            MainView::People
        } else {
            MainView::Conversation
        };
        Self {
            signed_in: core.me().is_some(),
            dialog: DialogState::Closed,
            composing: false,
            view,
            conversation_selected: selected.is_some(),
            conversations: all.len(),
            other_unread: all.iter().any(|id| {
                Some(*id) != selected
                    && core
                        .user_state(*id)
                        .is_some_and(|state| state.unread_count > 0 || state.mention_count > 0)
            }),
            workspace_names: core
                .workspace_rail()
                .into_iter()
                .map(|item| item.name)
                .collect(),
            active_workspace: core.workspace_rail().iter().position(|item| item.active),
            can_manage_workspace: core.can_manage_workspace(),
            can_manage_groups: core.can_manage_groups(),
            can_manage_emoji: core.can_manage_emoji(),
            can_create_channel: core.can_create_channel(),
        }
    }
}

/// Whether a command can run now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Enabled,
    /// Shown dimmed with this reason; never run.
    Disabled(&'static str),
    Hidden,
}

impl Availability {
    pub fn is_enabled(self) -> bool {
        self == Availability::Enabled
    }
}

/// The rule: one function, asked by every button, shortcut, menu item, and palette row.
pub fn availability(id: CommandId, snapshot: &AppSnapshot) -> Availability {
    use Availability::{Disabled, Enabled, Hidden};
    if !snapshot.signed_in {
        return Hidden;
    }
    if let CommandId::SwitchWorkspace(n) = id {
        // Only the workspaces there are, and not the one that is open.
        if usize::from(n) > snapshot.workspace_names.len()
            || snapshot.active_workspace == Some(usize::from(n) - 1)
        {
            return Hidden;
        }
    }
    match snapshot.dialog {
        DialogState::Closed => {}
        DialogState::Own(own) if own == id => return Enabled,
        _ => return Disabled("Close the open dialog first."),
    }
    if snapshot.composing {
        return Disabled("Finish the text being composed first.");
    }
    match id {
        CommandId::CreateChannel if !snapshot.can_create_channel => {
            Disabled(crate::states::CHANNELS_ADMINS_ONLY_NOTE)
        }
        CommandId::WorkspaceSettings | CommandId::InvitePeople
            if !snapshot.can_manage_workspace =>
        {
            Disabled("Only admins and owners can do this.")
        }
        CommandId::UserGroups if !snapshot.can_manage_groups => {
            Disabled("Only admins and owners manage user groups.")
        }
        CommandId::CustomEmoji if !snapshot.can_manage_emoji => {
            Disabled("Only admins and owners manage custom emoji.")
        }
        CommandId::ConversationDetails | CommandId::ToggleFavorite
            if !(snapshot.conversation_selected && snapshot.view == MainView::Conversation) =>
        {
            Disabled("Open a conversation first.")
        }
        CommandId::PreviousConversation | CommandId::NextConversation
            if snapshot.conversations < 2 =>
        {
            Disabled("There is no other conversation.")
        }
        CommandId::PreviousUnread | CommandId::NextUnread if !snapshot.other_unread => {
            Disabled("Nothing else is unread.")
        }
        _ => Enabled,
    }
}

/// The title as shown now: a workspace switch names the workspace.
pub fn title(id: CommandId, snapshot: &AppSnapshot) -> String {
    match id {
        CommandId::SwitchWorkspace(n) => snapshot
            .workspace_names
            .get(usize::from(n).saturating_sub(1))
            .map_or_else(
                || command_for(id).title.to_owned(),
                |name| format!("Switch to {name}"),
            ),
        _ => command_for(id).title.to_owned(),
    }
}

// ----- Menus and the palette -----

/// One row of a menu or of the palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: CommandId,
    pub title: String,
    pub shortcut: Option<String>,
    pub availability: Availability,
}

/// The commands of `group` that are not hidden, in order.
pub fn group_rows(group: Group, snapshot: &AppSnapshot) -> Vec<Row> {
    REGISTRY
        .iter()
        .filter(|command| command.group == group)
        .filter_map(|command| {
            let availability = availability(command.id, snapshot);
            (availability != Availability::Hidden).then(|| Row {
                id: command.id,
                title: title(command.id, snapshot),
                shortcut: command.shortcut.map(shortcut_text),
                availability,
            })
        })
        .collect()
}

/// What `>` lists in the quick switcher: the commands that can run now whose title matches
/// `query` (fuzzy), best first, ties in the registry's order. An empty query lists them all.
pub fn palette_rows(query: &str, snapshot: &AppSnapshot) -> Vec<Row> {
    let mut matched: Vec<(i32, usize, Row)> = REGISTRY
        .iter()
        .enumerate()
        .filter(|(_, command)| availability(command.id, snapshot).is_enabled())
        .filter_map(|(index, command)| {
            let title = title(command.id, snapshot);
            fuzzy_match(query, &title).map(|found| {
                (
                    found.score,
                    index,
                    Row {
                        id: command.id,
                        title,
                        shortcut: command.shortcut.map(shortcut_text),
                        availability: Availability::Enabled,
                    },
                )
            })
        })
        .collect();
    matched.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    matched.into_iter().map(|(_, _, row)| row).collect()
}

/// The shortcuts dialog's rows: each group's commands that have a shortcut, the nine workspace
/// switches as one row.
pub fn shortcut_rows() -> Vec<(Group, Vec<(String, String)>)> {
    Group::ALL
        .iter()
        .filter_map(|group| {
            let mut rows: Vec<(String, String)> = Vec::new();
            for command in REGISTRY.iter().filter(|command| command.group == *group) {
                let Some(spec) = command.shortcut else {
                    continue;
                };
                match command.id {
                    CommandId::SwitchWorkspace(1) => rows.push((
                        "Switch to workspace 1 to 9".to_owned(),
                        format!("{} to 9", shortcut_text(spec)),
                    )),
                    CommandId::SwitchWorkspace(_) => {}
                    _ => rows.push((command.title.to_owned(), shortcut_text(spec))),
                }
            }
            (!rows.is_empty()).then_some((*group, rows))
        })
        .collect()
}

/// The native menu bar for a Mac: an application menu with Preferences, then one menu per group.
/// Items dispatch [`RunCommand`]; the menu bar shows each one's shortcut from the key bindings.
/// Compiled and structure-tested on every platform, but only a Mac shows it.
pub fn native_menus() -> Vec<gpui::Menu> {
    use gpui::{Menu, MenuItem};
    let item = |id: CommandId| {
        MenuItem::action(
            command_for(id).title.trim_end_matches('…').to_owned() + "…",
            RunCommand(id),
        )
    };
    let plain = |id: CommandId| MenuItem::action(command_for(id).title, RunCommand(id));
    let mut menus = vec![Menu {
        name: "Lazy".into(),
        items: vec![item(CommandId::Preferences), MenuItem::separator()],
        disabled: false,
    }];
    // The application menu's Preferences is the same command as File's; File keeps the rest.
    for group in Group::ALL {
        let items: Vec<MenuItem> = REGISTRY
            .iter()
            .filter(|command| command.group == group)
            .filter(|command| !matches!(command.id, CommandId::SwitchWorkspace(_)))
            .filter(|command| command.id != CommandId::Preferences)
            .map(|command| {
                if command.title.ends_with('…') {
                    item(command.id)
                } else {
                    plain(command.id)
                }
            })
            .collect();
        if !items.is_empty() {
            menus.push(Menu {
                name: group.title().into(),
                items,
                disabled: false,
            });
        }
    }
    menus
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> AppSnapshot {
        AppSnapshot {
            signed_in: true,
            dialog: DialogState::Closed,
            composing: false,
            view: MainView::Conversation,
            conversation_selected: true,
            conversations: 5,
            other_unread: true,
            workspace_names: vec!["Lazy".into(), "Guild".into()],
            active_workspace: Some(0),
            can_manage_workspace: true,
            can_manage_groups: true,
            can_manage_emoji: true,
            can_create_channel: true,
        }
    }

    #[test]
    fn every_command_is_in_the_registry_once() {
        let ids: Vec<_> = all().iter().map(|command| command.id).collect();
        for (index, id) in ids.iter().enumerate() {
            assert!(!ids[index + 1..].contains(id), "{id:?} is listed twice");
        }
        assert_eq!(
            command_for(CommandId::Search).shortcut,
            Some("secondary-f"),
            "the existing shortcuts are unchanged"
        );
        assert_eq!(
            command_for(CommandId::QuickSwitcher).shortcut,
            Some("secondary-k")
        );
        for n in 1..=9u8 {
            assert_eq!(
                command_for(CommandId::SwitchWorkspace(n)).shortcut,
                Some(
                    [
                        "secondary-1",
                        "secondary-2",
                        "secondary-3",
                        "secondary-4",
                        "secondary-5",
                        "secondary-6",
                        "secondary-7",
                        "secondary-8",
                        "secondary-9"
                    ][usize::from(n) - 1]
                )
            );
        }
    }

    #[test]
    fn no_two_commands_share_a_shortcut() {
        let mut seen: Vec<&str> = Vec::new();
        for command in all() {
            if let Some(spec) = command.shortcut {
                assert!(!seen.contains(&spec), "{spec} is bound twice");
                seen.push(spec);
            }
        }
    }

    #[test]
    fn every_shortcut_has_a_modifier_and_is_written_in_a_form_gpui_parses() {
        for command in all() {
            let Some(spec) = command.shortcut else {
                continue;
            };
            let (key, modifiers) = spec.rsplit_once('-').map_or((spec, ""), |(m, k)| (k, m));
            // `rsplit_once('-')` on "secondary-," is fine: the key is the last part.
            assert!(
                !modifiers.is_empty(),
                "{spec} has no modifier, so it would take a typed key"
            );
            for stroke in [keystroke_for(spec, false), keystroke_for(spec, true)] {
                // `parse` wants a string that lives forever; a test can afford the leak.
                let stroke: &'static str = Box::leak(stroke.into_boxed_str());
                let parsed = gpui::Keystroke::parse(stroke)
                    .unwrap_or_else(|error| panic!("{stroke} does not parse: {error}"));
                assert_eq!(parsed.key, key);
            }
        }
    }

    #[test]
    fn shortcuts_are_written_per_platform() {
        assert_eq!(shortcut_text_for("secondary-k", false), "Ctrl+K");
        assert_eq!(shortcut_text_for("secondary-k", true), "⌘K");
        assert_eq!(
            shortcut_text_for("secondary-shift-t", false),
            "Ctrl+Shift+T"
        );
        assert_eq!(shortcut_text_for("secondary-shift-t", true), "⇧⌘T");
        assert_eq!(shortcut_text_for("alt-shift-down", false), "Alt+Shift+Down");
        assert_eq!(shortcut_text_for("alt-up", true), "⌥↑");
        assert_eq!(shortcut_text_for("secondary-,", false), "Ctrl+,");
        assert_eq!(keystroke_for("secondary-k", true), "cmd-k");
        assert_eq!(keystroke_for("secondary-k", false), "ctrl-k");
    }

    #[test]
    fn a_dialog_over_the_shell_disables_everything_but_its_own_command() {
        let mut s = snapshot();
        s.dialog = DialogState::Own(CommandId::Preferences);
        assert_eq!(
            availability(CommandId::Preferences, &s),
            Availability::Enabled
        );
        assert!(matches!(
            availability(CommandId::Search, &s),
            Availability::Disabled(_)
        ));
        s.dialog = DialogState::Other;
        assert!(matches!(
            availability(CommandId::Preferences, &s),
            Availability::Disabled(_)
        ));
    }

    #[test]
    fn composition_disables_every_command() {
        let mut s = snapshot();
        s.composing = true;
        for command in all() {
            if availability(command.id, &snapshot()) == Availability::Hidden {
                continue;
            }
            assert!(
                matches!(availability(command.id, &s), Availability::Disabled(_)),
                "{:?} ran during composition",
                command.id
            );
        }
    }

    #[test]
    fn rules_follow_the_role_the_policy_and_what_is_open() {
        let mut s = snapshot();
        s.can_create_channel = false;
        s.can_manage_workspace = false;
        s.can_manage_groups = false;
        s.can_manage_emoji = false;
        for id in [
            CommandId::CreateChannel,
            CommandId::WorkspaceSettings,
            CommandId::InvitePeople,
            CommandId::UserGroups,
            CommandId::CustomEmoji,
        ] {
            assert!(
                matches!(availability(id, &s), Availability::Disabled(_)),
                "{id:?}"
            );
        }
        s.view = MainView::People;
        assert!(matches!(
            availability(CommandId::ConversationDetails, &s),
            Availability::Disabled(_)
        ));
        s.conversations = 1;
        s.other_unread = false;
        for id in [
            CommandId::NextConversation,
            CommandId::PreviousConversation,
            CommandId::NextUnread,
            CommandId::PreviousUnread,
        ] {
            assert!(
                matches!(availability(id, &s), Availability::Disabled(_)),
                "{id:?}"
            );
        }
        s.signed_in = false;
        assert_eq!(availability(CommandId::Search, &s), Availability::Hidden);
    }

    #[test]
    fn a_workspace_switch_exists_only_for_workspaces_that_exist() {
        let s = snapshot();
        assert_eq!(
            availability(CommandId::SwitchWorkspace(2), &s),
            Availability::Enabled
        );
        assert_eq!(
            availability(CommandId::SwitchWorkspace(3), &s),
            Availability::Hidden
        );
        assert_eq!(title(CommandId::SwitchWorkspace(2), &s), "Switch to Guild");
        assert_eq!(
            availability(CommandId::SwitchWorkspace(1), &s),
            Availability::Hidden,
            "there is nothing to switch to in the workspace that is open"
        );
    }

    #[test]
    fn the_palette_lists_only_what_can_run_and_matches_fuzzily() {
        let mut s = snapshot();
        s.can_manage_emoji = false;
        let all_rows = palette_rows("", &s);
        assert!(all_rows.iter().all(|row| row.availability.is_enabled()));
        assert!(!all_rows.iter().any(|row| row.id == CommandId::CustomEmoji));
        let found = palette_rows("pref", &s);
        assert_eq!(
            found.first().map(|row| row.id),
            Some(CommandId::Preferences)
        );
        assert!(found[0].shortcut.is_some());
        assert!(palette_rows("zzzzqq", &s).is_empty());
    }

    #[test]
    fn the_shortcut_rows_list_every_shortcut_with_the_workspaces_as_one() {
        let rows = shortcut_rows();
        let flat: Vec<&(String, String)> = rows.iter().flat_map(|(_, rows)| rows).collect();
        let with_shortcuts = all()
            .iter()
            .filter(|command| command.shortcut.is_some())
            .count();
        assert_eq!(
            flat.len(),
            with_shortcuts - 8,
            "nine switches share one row"
        );
        assert!(flat.iter().any(|(title, _)| title.contains("1 to 9")));
    }

    #[test]
    fn the_native_menus_cover_the_groups() {
        let menus = native_menus();
        let names: Vec<String> = menus.iter().map(|menu| menu.name.to_string()).collect();
        assert_eq!(names, ["Lazy", "File", "Go", "View", "Workspace", "Help"]);
        assert!(menus.iter().all(|menu| !menu.items.is_empty()));
    }
}
