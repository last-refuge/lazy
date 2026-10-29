# Lazy

A native, Slack-familiar workplace chat application written in Rust: a GPUI desktop client, an Axum and Postgres server, and a shared client core meant to be reused by future phone clients.

The product plan is in [llm-docs/PLAN.md](llm-docs/PLAN.md), the phased work breakdown is in [llm-docs/IMPLEMENTATION_PLAN.md](llm-docs/IMPLEMENTATION_PLAN.md), and recorded design decisions are in [llm-docs/DECISIONS.md](llm-docs/DECISIONS.md).

## Status

Phases 1 and 2 of the implementation plan are done (the MVP, then channels, direct and group messages, favorites, and unread counts; formatted messages, editing, deleting, and message links; emoji, reactions, mentions, and user groups; threads; text selection and copy, incoming webhooks, and clearer empty and error states), and Phase 3 is under way (attachments, custom emoji, search, notifications, discovery, workspace administration, several workspaces, profile pictures, commands and menus, and logging are done): create an account or sign in, open the `#general` channel, send messages with durable and idempotent delivery, receive messages from other clients in realtime, page older history, and recover after disconnects and restarts. Huddles, and everything in Phase 4 (integrations), are later.

The server, the client core and runtime, and the message-delivery guarantees are covered by automated tests, including fault injection (see [llm-docs/VERIFICATION.md](llm-docs/VERIFICATION.md)). **The desktop window itself has not been looked at on a real display by the developer**, so how it renders, feels, and handles real input methods is exactly what this first run is for. Please note anything that looks or behaves wrong.

## Requirements

- Rust 1.99 (pinned in `rust-toolchain.toml`; `rustup` installs it automatically)
- PostgreSQL server binaries on `PATH` (`initdb`, `pg_ctl`, `psql`, `createdb`); the dev database runs from a project-local directory and never touches a system instance
- Linux with Wayland or X11, Vulkan, and ALSA development libraries for GPUI
- A Secret Service keyring (for example GNOME Keyring) for storing the session token and local encryption key

## Run it

The first run compiles in release mode (GPUI is large), so expect a few minutes; later runs start quickly.

Start the server in one terminal. This starts the dev Postgres on `127.0.0.1:54329`, applies migrations, and seeds the `Lazy` workspace with `#general`:

```sh
scripts/run-server.sh
```

Start two desktop clients, each with its own disposable profile, in two more terminals:

```sh
scripts/run-client.sh alice
scripts/run-client.sh bob
```

In each window choose **Create account**, register a different name, and open `#general`. Messages sent in one window appear in the other. The first account to register becomes the workspace owner.

Profiles live in `.dev/profiles/<name>`. Delete a profile directory to start fresh, and run `scripts/dev-db.sh reset` to wipe the dev database. If your machine has no Secret Service keyring, set `LAZY_INSECURE_STORAGE=1` to keep the session in plain files inside the profile (development only).

## What to try, and what to look for

New in slices 3H and 3I: the keyboard, the menu, and diagnostics. Every action has one name wherever it appears. **Ctrl+Shift+P** (or `>` in **Ctrl+K**) lists commands that can run right now, each with its shortcut; **Ctrl+/** lists the shortcuts: Ctrl+K quick switcher, Ctrl+F search, Ctrl+N new message, Ctrl+, Preferences, Ctrl+Shift+T / S / E / L for Threads / Saved / People / Browse channels, Alt+Up and Alt+Down for the previous and next conversation, Alt+Shift+Up and Alt+Shift+Down for the previous and next unread one, Ctrl+1 to Ctrl+9 for workspaces (Cmd instead of Ctrl on a Mac). A **Menu** button at the top of the sidebar opens the same commands grouped (File, Go, View, Workspace, Help); an unavailable one says why. The window reopens at the size and maximized state you left it (position too, except on Wayland). **Help, Diagnostics…** shows a report with no secrets (versions, connection state, counts, the folders) and **Copy diagnostics** puts it on the clipboard for a bug report. Logs are in `logs/lazy.log` in your profile (about 5 MiB each, three kept) and on the server's terminal, and contain no passwords, messages, names, or file names; `LAZY_LOG=lazy=debug` shows more, `LAZY_LOG_FORMAT=json` makes the server's JSON lines. Please try the shortcuts on your keyboard layout and with an input method editor, and read `llm-docs/DOGFOOD.md` before inviting anyone: those are what has not been verified.

New in slice 3G: more than one workspace, and you. The slim column at the left edge is the **rail**: one square per workspace you belong to (with a number when something there needs you and a dot when there is unread chatter), the **+** at the end adds one (create your own, or join with an invitation code from someone else), and **Ctrl+1** to **Ctrl+9** (Cmd on a Mac) jump between them. Each workspace keeps its own channels, people, settings, and saved messages, and a workspace you are not looking at still notifies you (the title starts with its name). In **Preferences…** the Profile section changes your display name and picture, and Appearance chooses light, dark, or the system theme and turns animation off; both are kept on this device only. Please try several workspaces with two accounts, a picture from your real file dialog, and an input method editor in the new fields, which are what has not been verified.

New in slice 3F: running a workspace. The first person to register owns it. Owners and admins see **Workspace settings…** and **Invite people…** at the bottom of the sidebar. Settings has the workspace's name, who can create accounts (anyone, or by invitation only) and who can create channels (everyone, or admins only). **Invite people…** makes an invitation code that lasts from an hour to 30 days and can be used once or many times; the code is shown only once, so copy it then, and share it; the person types it into the **Invitation code** field when creating their account. Open someone's profile (People, or their name in a message) to change their role or deactivate them; a deactivated person cannot sign in to the workspace and receives nothing, but their old messages stay, and an owner can reactivate them. Please try these with two accounts, and the keyboard alone, which is what has not been verified.

New in slice 3E: finding things. **Ctrl+K** opens the quick switcher: type a few letters of a channel, a person, or `threads`, `saved`, or `search`, and Enter goes there (with no text it lists the places you were most recently, and choosing a person you have no conversation with opens one). **People** in the sidebar lists everyone in the workspace with a search box; click a person (or a name in the timeline) for their profile, with a Message button. **Browse channels** now has a search box, an All / Not joined filter, a Name / Members sort, and Load more. In a message's menu, **Save for later** keeps it; saved messages show a star, and **Saved** in the sidebar lists them, newest first, and takes you to the message. Please try the switcher and the lists by keyboard alone and with an input method editor, which are what have not been verified.

New in slice 3D: desktop notifications and a thread inbox. By default you are notified for direct messages, for being @mentioned or named through a group, for `@channel`, and for replies in threads you follow; other channel messages do not notify (change this with the bell button in a channel's header, or **Preferences…** at the bottom of the sidebar, which also has pause, quiet hours, and your status). Muting a channel dims it and silences it. Notifications are grouped per conversation, show the sender and text unless you turn that off, and clicking one opens the message. The window title shows a count like `(3) Lazy` of what needs you (mentions, direct messages, and followed threads). **Threads** in the sidebar lists the threads you follow, most recently active first, with a count of unread replies and **Mark all read**. A status (an emoji and a few words, optionally clearing itself) shows beside your name. Please check the notifications on your real desktop: whether they appear, are replaced rather than piled up, and open the right message when clicked, are exactly what has not been verified.

New in slice 3C: press **Ctrl+F** (or use **Search** at the top of the sidebar) and type. Words match as prefixes (`deplo` finds `deploy`), `"quotes"` find a phrase, `-word` excludes one, and `from:alice`, `in:design`, `after:2026-09-01`, `before:2026-10-01`, and `has:file` or `has:image` narrow it; the filters appear as chips you can remove. Search finds messages and file names in conversations you are in and in public channels, never in private ones you are not in. Up and Down move through the results, Enter opens one (a reply opens its thread), and Escape closes search. There is no accent folding yet (`cafe` does not find `café`).

New in slice 3B: workspace admins and owners can add their own emoji with **Custom emoji…** at the bottom of the sidebar (choose an image of up to 512 KiB, give it a name such as `party_parrot`). Everyone can then type `:party_parrot:` in a message (suggestions appear after `:`), react with it, and find it in a **Custom** section of the emoji picker. Renaming an emoji changes how it reads everywhere; deleting one leaves its name in old messages and reactions as muted `:name:` text. Names cannot be a standard emoji name such as `tada`. Animated images show only their first frame for now.

New in slice 3A: attach files to a message with the paperclip beside the emoji button, by dropping them on the window, or by pasting an image (or, on Wayland, copied files). Each file appears as a chip above the message box with its progress; Enter sends, even with no text. Images show a preview in the timeline and open in a viewer on click or Enter (Left and Right move between a message's pictures, Escape closes it); other files show a chip with **Download**, which asks where to save and checks the file against its checksum. The server keeps the files in `.dev/attachments` (set `LAZY_ATTACHMENTS_DIR` to move them), up to 25 MiB each and ten per message, and decides what each file is from its contents, so a file named like a picture that is not one is only offered as a download. Please try drag and drop, pasting, and the file dialogs on your real desktop: those are exactly what has not been verified yet.

New in slice 2E: you can now select text in the timeline with the mouse, across messages and past the edges of the window (drag near the top or bottom to scroll), then copy it; double-click selects a word, triple-click a message, Shift+click extends, Ctrl+A selects everything loaded, and with the keyboard Shift+Up and Shift+Down extend by message and Ctrl+C copies. The app reopens on the conversation you last had open, with the cursor where you left it in each draft. Admins and owners can open **Details** in a channel and use **Integrations** to make an incoming webhook: it shows an address once, and anything that can send an HTTP POST with `{"text": "..."}` to it posts into the channel as an app (for example `curl -X POST -H 'Content-Type: application/json' -d '{"text":"*deploy* finished :tada:"}' <address>`; run the server with `LAZY_PUBLIC_URL` set if clients reach it by another address). Empty screens and failures now say what happened and what to do, with a **Try again** where one helps.

New in slice 2D: reply to a message in a thread with the **Reply** button on hover (or Enter on a message, then **Reply in thread**). The thread opens in a pane beside the conversation, which you can drag wider or narrower (or focus its handle and use Left and Right); in a narrow window it replaces the conversation and **Back** returns. The root shows "N replies" with the repliers' avatars, and a dot when you follow the thread and it has replies you have not read. You follow a thread when you reply to it, when you started it and someone else replies, or when someone mentions you in it, and **Unfollow thread** is in the pane's header. Tick **Also send to #channel** to show a reply in the conversation too. If a message with replies is deleted it stays as "This message was deleted" with its replies until the last reply is deleted.

New in slice 2C: react to a message with the **React** button on hover (or Enter on a message, then **Add reaction…**), and click a chip to add or remove your own reaction. The picker has search, group tabs, skin tones, and recently used emoji, and the smiley beside **Send** opens it for the composer; typing `:tada:` or starting `:tad` in the composer converts or suggests the emoji. Type `@` followed by the start of a name to mention someone (Enter or Tab accepts the suggestion); `@channel` mentions everyone in a channel (in `#general` only workspace admins may use it). A message that mentions you is tinted with a bar at its left edge and counts as `@N` next to the conversation until you read it. Workspace admins and owners see **User groups…** at the bottom of the sidebar, to make groups such as `@design-team` that mention several people at once. Emoji are drawn in colour by a bundled font, so please look at the ones that used to show as grey outlines (❤️, ☕, ☀️, family sequences).

New in slice 2B: messages are formatted with Slack-style text, and the composer sends it as you type it: `*bold*`, `_italic_`, `~strike~`, `` `code` ``, `[text](https://example.com)` or a bare web address, a fenced code block (three backticks, optionally followed by a language), `> ` for a quote, `- ` for a bulleted list, `1. ` for a numbered one, and a backslash to keep a character literal. Ctrl+B, Ctrl+I, and Ctrl+Shift+X wrap the selection. Hover a message for **Edit**, **Delete**, **Copy link**, and **Copy** (you can edit your own messages, delete your own, and workspace admins can delete anyone's); with the keyboard, Tab into the timeline, use the arrow keys to move between messages, and press Enter for the same actions. Up arrow in an empty composer edits your latest message. An edited message shows "(edited)", and if someone changed a message while you were editing it you are shown their version with your text kept. A link of the form `lazy://workspace/conversation/message` (**Copy link**) opens that message, loading older history if needed.

New in slice 2A: create a channel from the **+** next to Channels, browse and join channels from the same menu, start a direct or group message from the **+** next to Direct messages, star a conversation to pin it under Favorites, and use **Details** in a channel's header to set its topic, add people, leave, or archive it. Unread conversations are bold with a count, and a "New messages" line marks where you left off. Removing someone else from a channel has no button yet; it is covered by the automated tests.

Covered by automated tests, so surprises here are bugs worth reporting:

- Send a message in one window and watch it arrive in the other. Send several quickly: they arrive in the order you sent them.
- Stop the server (Ctrl-C) and send a message: it stays in the timeline marked as sending or being checked, and the connection banner shows reconnecting. Start the server again and the message is delivered once.
- Quit a client right after sending, reopen it with the same profile: you are still signed in, cached history appears immediately, and nothing is duplicated.
- Scroll up in a long conversation: older messages load before you reach the top, and your reading position holds when they arrive.

Looked at in offscreen renders (see `llm-docs/VERIFICATION.md`), but not yet on a real display, so please look closely:

- Overall look, spacing, and both light and dark themes (the app follows the system appearance).
- Typing with an input method (Japanese, Chinese, Korean): pressing Enter while composing should commit the composition and not send, and the candidate window should appear at the caret. This has only been tested on GPUI's test platform.
- Resizing the window down to its minimum size, and scrolling smoothness with a few hundred messages.
- Text selection with the mouse and keyboard in the composer, and the hover **Copy** button on messages.
- Keyboard-only use: Tab and Shift+Tab move between the channels, the timeline, the composer, Send, and Sign out; Enter or Space activates the focused control. Check that the focus ring is visible where you expect it.
- Some emoji (❤️, ☕, and family sequences) show as small monochrome glyphs; most others render in color.

Known gaps (by design for this MVP) are listed at the end of [llm-docs/VERIFICATION.md](llm-docs/VERIFICATION.md).

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets
scripts/test.sh                   # runs the workspace tests on a separate Postgres (port 54330),
                                  # so a server you are running by hand is never disturbed
cargo run -p lazy-ui --example screens -- .dev/screens   # render the real views offscreen to PNGs
```

| Crate | Responsibility |
| --- | --- |
| `lazy-domain` | Typed identities, message documents, and pure authorization rules |
| `lazy-protocol` | Versioned HTTP and WebSocket wire types |
| `lazy-server` | Authentication, durable messaging, event log, realtime delivery |
| `lazy-client-core` | Synchronous client state machine: no I/O, no UI toolkit |
| `lazy-client-runtime` | Executes the core's effects: HTTP, WebSocket, encrypted local storage |
| `lazy-ui` | GPUI views, theme, and the message composer |
| `lazy-desktop` | The desktop application binary |
| `lazy-e2e` | End-to-end tests: real client stacks against a real server and Postgres, with fault injection |
