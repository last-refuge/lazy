# Using Lazy for real work: the dogfood guide

This is for the first people who use Lazy instead of (or next to) their usual chat. It is the
exit criterion of Phase 3 (task 3.9): dogfood users complete their normal work without developer
help. If you had to ask a developer how to do something, that is a bug in Lazy or in this
guide: please report it.

## What you need

- **The client.** Download the installer for your system from the project's GitHub releases (see
  `RELEASE.md`): a `.deb` or tarball for Linux, a `.dmg` for macOS, a Setup `.exe` for Windows. The
  files are not code-signed yet, so macOS and Windows ask you to confirm the first run (`RELEASE.md`
  says how). Linux needs a Vulkan driver and a Secret Service keyring (GNOME Keyring, KWallet,
  KeePassXC) to keep your sign-in. Desktop notifications work on Linux only so far.
- **A server.** One person runs it, on a machine everyone can reach, with Docker Compose or Coolify
  (`DEPLOY.md`); it must be on HTTPS if it is reachable from the internet. They give you its address.
  Lazy's first screen asks for it (it is remembered).
- **An account.** The person who set up the server creates the owner account first, with the server's
  setup code typed into the **Invitation code** field of the first screen. Everyone else joins with an
  **invitation code**: the owner opens **Invite people…** under **Menu**, **Workspace**, creates an
  invitation (an hour to 30 days, one use or many), and sends the code, which is shown once.

For development instead (everything on one machine, no installers): `scripts/run-server.sh` and
`scripts/run-client.sh <name>` as in `README.md`.

Your data lives in the server's database and attachment folder and, for your client, in its profile
directory (see **Where things are** below). Message history is not end-to-end encrypted; whoever runs
the server can read it.

## A first day, as a checklist

Try these in your real work rather than as a test, and note what slowed you down.

1. Sign up (with an invitation code if you have one), open `#general`, and say hello.
2. Join the channels you need: **Browse channels**, search the directory, **Join**.
3. Start a direct message with a colleague (**People**, or **Ctrl+K** and type their name).
4. Use **Ctrl+K** all day to move around. `>` in it lists commands. **Ctrl+/** lists the keyboard shortcuts.
5. Write with formatting (bold, italic, code, lists, quotes, links), paste a code block, `@mention`
   a person, mention a user group, add an emoji, react to a message.
6. Reply in a thread; follow and unfollow one; read the **Threads** inbox.
7. Attach a file and an image (drag and drop, paste, and the file dialog), and open the image viewer.
8. Search for something you know was said (**Ctrl+F**) and jump to the result; **Save for later** a message
   and find it under **Saved**.
9. Edit and delete a message of your own; copy a link to a message and open it.
10. Set your status, pause notifications for an hour, set quiet hours, mute a noisy channel.
    Check that desktop notifications appear, group by conversation, and open the right message when clicked.
11. Set your name and picture, and the theme, in **Preferences…**.
12. If you run a workspace: change its settings, invite someone, change a role, deactivate and reactivate a
    test account.
13. Use more than one workspace: **+** on the rail, **Ctrl+1** to **Ctrl+9**.
14. Close the app mid-sentence, reopen it: you should come back where you were with your draft.
15. Unplug the network (or stop the server) for a minute while composing and sending, then reconnect: nothing
    should be lost or duplicated.

## What is known to be unverified

These were built and tested in headless runs and have **not** been seen on a real desktop; they
are where problems are most likely, so please look at them first and say what you saw:

- Desktop notifications on your notification daemon (replace, withdraw, click to open) and the launcher count.
- Dragging files in, pasting images and file lists, and the file dialogs, on your compositor.
- Input methods (IME) for languages you actually type, in the composer, search, and every dialog field; screen readers.
- Display scaling and multiple monitors; window position restoration (not possible on Wayland).
- Long sessions (days open), very large workspaces or channels, and slow networks.

The full record, including every measurement behind these statements, is `llm-docs/VERIFICATION.md`.

## Where things are

- **Logs:** `logs/lazy.log` in your profile directory (and the three previous files). They contain
  no passwords, messages, or names, but read them before sharing anyone else's.
- **Profile directory:** printed in **Help, Diagnostics…** and at startup; `scripts/run-client.sh <name>` uses
  `.dev/profiles/<name>`. Delete it to start fresh (you will sign in again; nothing on the server is lost).
- **Diagnostics:** **Help, Diagnostics…, Copy diagnostics** gives a report with versions, connection state, and
  counts; paste it into any report.
- **Server logs:** on the terminal that runs the server; `LAZY_LOG=lazy=debug` for more and
  `LAZY_LOG_FORMAT=json` for machine-readable lines.

## How to report a problem

Open a GitHub issue with the **Dogfood report** template (`.github/ISSUE_TEMPLATE/dogfood.md`). The
most useful reports say what you were trying to do, what happened, how bad it was, and include the
diagnostics. Triage order: lost or duplicated messages and data first, then anything that blocks
work, then confusion, then polish. A message that did not arrive, arrived twice, or changed
without you doing it is always the highest priority: say exactly what you did just before.

## For the person running the dogfood

- Keep the dev database backed up if people depend on it (`pg_dump` the database at `127.0.0.1:54329`).
  Never run the test suite against it: tests use `scripts/test.sh` and the separate database on port 54330.
- When the server is upgraded it applies migrations at start; protocol changes that older clients cannot
  read are refused with a clear message, so update clients and server together.
- The Phase 3 gate (task 3.9) is judged by watching people finish normal work without help; count the
  questions they had to ask and fix those first.
