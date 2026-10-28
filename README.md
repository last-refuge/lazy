# Lazy

A native, Slack-familiar workplace chat application written in Rust: a GPUI desktop client, an Axum and Postgres server, and a shared client core meant to be reused by future phone clients.

The product plan is in [llm-docs/PLAN.md](llm-docs/PLAN.md), the phased work breakdown is in [llm-docs/IMPLEMENTATION_PLAN.md](llm-docs/IMPLEMENTATION_PLAN.md), and recorded design decisions are in [llm-docs/DECISIONS.md](llm-docs/DECISIONS.md).

## Status

The first MVP covers Phase 1 of the implementation plan: create an account or sign in, open the `#general` channel, send messages with durable and idempotent delivery, receive messages from other clients in realtime, page older history, and recover after disconnects and restarts. Channels beyond `#general`, threads, reactions, search, attachments, integrations, and huddles are later phases.

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
- Keyboard-only use: Tab and Shift+Tab move between the channels, composer, Send, and Sign out; Enter or Space activates the focused control. Check that the focus ring is visible where you expect it.
- Some emoji (❤️, ☕, and family sequences) show as small monochrome glyphs; most others render in color.

Known gaps (by design for this MVP) are listed at the end of [llm-docs/VERIFICATION.md](llm-docs/VERIFICATION.md).

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo test --workspace            # the dev database must be running: scripts/dev-db.sh start
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
