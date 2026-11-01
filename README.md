<p align="center">
  <!-- The fragments make GitHub pick by its own theme setting, not the browser's. -->
  <img src="branding/logos/horizontal/lazy-logo-forest.svg#gh-light-mode-only" alt="Lazy" width="260">
  <img src="branding/logos/horizontal/lazy-logo-paper.svg#gh-dark-mode-only" alt="Lazy" width="260">
</p>

<p align="center">
  <strong>A native, Slack-familiar workplace chat app, written in Rust.</strong><br>
  A desktop client, a server you run yourself, and a shared core for future phone apps.
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#configure">Configure</a> ·
  <a href="#build-from-source">Build from source</a> ·
  <a href="#if-something-goes-wrong">Troubleshooting</a> ·
  <a href="llm-docs/DEPLOY.md">Deploy guide</a> ·
  <a href="https://github.com/last-refuge/lazy/releases/latest">Download</a>
</p>

---

Lazy has two parts, and you set each up once:

| | What it is | Who sets it up |
| --- | --- | --- |
| **The server** | Runs on a machine your team can reach, with a Postgres database behind it. | One person |
| **The desktop app** | Windows, macOS, and Linux. Pointed at the server with an address and an invitation code. | Everyone |

```mermaid
flowchart LR
    app["Desktop app<br/>(everyone)"] -- "HTTPS and WebSocket" --> proxy["Reverse proxy<br/>(Caddy, Coolify)"]
    proxy --> server["Lazy server<br/>port 8080"]
    server --> db[("Postgres")]
    server --> files["Attachments<br/>volume"]
```

## Quick start

### Step 1: run the server

You need a machine with [Docker](https://docs.docker.com/engine/install/) and its Compose plugin.

```sh
git clone https://github.com/last-refuge/lazy.git && cd lazy
cp .env.example .env
```

Open `.env` and fill in three lines:

| Setting | What to put |
| --- | --- |
| `POSTGRES_PASSWORD` | A random password for the database. Letters and digits only. |
| `LAZY_SETUP_CODE` | A second random secret, 20 to 64 letters and digits. It lets you create the first account, the owner, and does nothing afterwards. |
| `LAZY_PUBLIC_URL` | The address people will use, such as `https://chat.example.com`. To try Lazy on one machine, leave `http://localhost:8080`. |

> [!TIP]
> `openssl rand -hex 24` makes a good secret for both of the first two.

Start it. The first build compiles the server and takes a few minutes:

```sh
docker compose up -d --build
curl -s localhost:8080/healthz          # {"protocol_version":...}
```

> [!IMPORTANT]
> **Put HTTPS in front of the server before anyone outside your network connects.** Sessions are bearer tokens, so plain `http://` over the internet is not safe. [Caddy](https://caddyserver.com/) does it with a two-line file and looks after the certificates:
>
> ```
> chat.example.com {
>     reverse_proxy 127.0.0.1:8080
> }
> ```
>
> Then set `LAZY_PUBLIC_URL=https://chat.example.com` to match.

[llm-docs/DEPLOY.md](llm-docs/DEPLOY.md) has the details: Coolify, backups, and what to watch.

### Step 2: install the app

Download **Lazy 0.8.0** from the [latest release](https://github.com/last-refuge/lazy/releases/latest):

| Your computer | Download |
| --- | --- |
| **Windows** 10 or 11 (64-bit) | `Lazy-Setup-0.8.0-windows-x64.exe`, or the portable `lazy-0.8.0-windows-x64.zip` |
| **macOS** 11 or later (Apple silicon or Intel) | `Lazy-0.8.0-macos-universal.dmg` |
| **Linux** (64-bit Intel or AMD) | `lazy-chat_0.8.0_amd64.deb` or `lazy-0.8.0-linux-x86_64.tar.gz` |

- **Windows.** Run the setup program. It installs for you alone and needs no administrator. If Windows says "Windows protected your PC", choose **More info**, then **Run anyway**.
- **macOS.** Open the disk image and drag **Lazy** to **Applications**. The first time, if macOS says it cannot verify Lazy, open **System Settings, Privacy & Security** and choose **Open Anyway**. On macOS 14 or earlier, right-click **Lazy**, choose **Open**, then **Open**.
- **Linux.** On Debian and Ubuntu, `sudo apt install ./lazy-chat_0.8.0_amd64.deb`. On anything else, unpack the tarball and run `./install.sh` (`--system` for everyone on the machine, `--uninstall` to remove it). You need a Vulkan driver and a keyring such as GNOME Keyring or KWallet.

The installers are not code-signed yet, which is why Windows and macOS ask you to confirm once. `SHA256SUMS` on the release page lists a checksum for every file. Desktop notifications work on Linux only for now; on macOS and Windows you get unread counts and the title badge.

### Step 3: create the owner account, then invite everyone else

**You, the person who set up the server.** Start Lazy and give it the server's address and your `LAZY_SETUP_CODE`. Lazy checks both, then walks you through your name, username, and password, a picture if you like, a name for the workspace, and your first invitation.

> [!NOTE]
> The first-run screens came with 0.6.0. If you are still on Lazy 0.5.0, it opens on a sign-in card instead: type the server's address in its **Server** row at the bottom, choose **Create account**, and put the setup code in the invitation field.

**Everyone else.** Whoever runs the server gives them two things: the server's **address** and an **invitation code**. They install Lazy and enter both on the first screen. Lazy shows which workspace they were invited to before they choose a name and a password. Someone who already has an account chooses **I already have an account**.

To make a code, open **Menu, Workspace, Invite people…**. **Copy message to send** copies everything a new person needs (where to get Lazy, your server's address, and the code) as one message:

```text
Join us on Lazy.

1. Get Lazy: https://github.com/last-refuge/lazy/releases/latest
2. Server: chat.example.com
3. Invitation code: K7QM-4TXD-9BWP-H2VA-ZX3C

The code works for up to 25 people until Nov 6 3:45 PM.
```

A code lasts from an hour to 30 days and can be used once or many times. It is shown only once, so copy it then. Capitals, small letters, spaces, and dashes all work when someone types it.

### Updates

Lazy looks for a newer release every 30 minutes while it runs, and a bar across the top of the window says when there is one. **Update now** downloads it (checked against the release's `SHA256SUMS`) while you keep working, and **Restart to update** installs it. **Menu, Help, Check for updates…** asks right away and has the switch for the automatic checks. A portable copy or one built from source opens the release page instead. Release 0.5.0 has no updater, so if you have it, install 0.6.0 by hand, once.

To upgrade the server, back up first (migrations only go forward), then `git pull && docker compose up -d --build`.

## Configure

### The server

Set these in `.env` (Docker Compose) or in the environment, or pass them as options (`lazy-server serve --help`).

**Set these first**

| Setting | Meaning |
| --- | --- |
| `POSTGRES_PASSWORD` | The database password (Compose). Without Compose, set `DATABASE_URL` to `postgres://user:password@host:5432/db`. |
| `LAZY_SETUP_CODE` | Needed to create the first account: 20 to 64 letters, digits, `-` and `_`. The server refuses to start with anything else, and says why. Set it on any server people can reach, or the first stranger to register becomes the owner. |
| `LAZY_PUBLIC_URL` | The address people reach the server at. Default `http://localhost:8080`. |

**Optional**

| Setting | Default | Meaning |
| --- | --- | --- |
| `LAZY_SEED_REGISTRATION` | `invite_only` in Compose | Who may create an account: `invite_only` (the owner invites people) or `open` (anyone who can reach the server). Applies only when the server first creates the workspace; change it later in **Workspace settings…**. |
| `LAZY_PORT` | `8080` | The port published on the machine (Compose). |
| `LAZY_EVENT_RETENTION_DAYS` | `30` | Days of history kept. `0` keeps everything. |
| `LAZY_MAX_ATTACHMENT_BYTES` | 25 MiB | The largest upload. |
| `LAZY_LOG`, `LAZY_LOG_FORMAT` | `info,sqlx=warn`, `text` | Logging. Passwords, codes, and message text are never logged. |

Back up the database and the attachments volume together: the database says which files exist, and the volume holds their bytes. The full list of settings is in [llm-docs/DEPLOY.md](llm-docs/DEPLOY.md) and [llm-docs/SERVER.md](llm-docs/SERVER.md).

### The app

You normally configure nothing: Lazy remembers the server you entered. These settings are for development and unusual setups.

| Setting | Meaning |
| --- | --- |
| `LAZY_SERVER_URL` | Fixes the server address. It can no longer be changed in the app, and the first-run screens are skipped. |
| `LAZY_PROFILE_DIR` | Where this copy keeps its local state, so several can share one machine. The default is your user data folder; **Help, Diagnostics…** shows it. |
| `LAZY_INSECURE_STORAGE=1` | Keeps the session in plain files inside the profile on a machine with no keyring. Development only. |
| `LAZY_UPDATE_CHECK=0` | Turns off the update checks. |
| `LAZY_LOG=lazy=debug` | More detail in `logs/lazy.log` in your profile. It never holds passwords, messages, names, or file names. |

## Build from source

**You need**

- **Rust 1.99.** It is pinned in `rust-toolchain.toml`, and [`rustup`](https://rustup.rs/) installs it for you.
- **PostgreSQL server programs on your `PATH`:** `initdb`, `pg_ctl`, `psql`, and `createdb`. The development database runs from a folder inside the project and never touches a system Postgres.
- **On Linux, the libraries GPUI needs.** On Debian and Ubuntu:
  ```sh
  sudo apt install libasound2-dev libwayland-dev libxkbcommon-x11-dev libx11-xcb-dev \
    libvulkan-dev libfontconfig-dev libssl-dev libdbus-1-dev pkg-config cmake clang
  ```
  You also need a Secret Service keyring (GNOME Keyring, say) for the session token.

The macOS and Windows installers are built on GitHub by the `release` workflow ([llm-docs/RELEASE.md](llm-docs/RELEASE.md)). Building the client yourself is documented for Linux.

### Run it on your machine

The first run compiles in release mode (GPUI is large), so expect a few minutes. Later runs start quickly.

```sh
scripts/run-server.sh            # terminal 1: starts the dev Postgres on 127.0.0.1:54329, applies
                                 #   migrations, seeds the "Lazy" workspace, serves on 127.0.0.1:8080
scripts/run-client.sh alice      # terminal 2: a client with its own disposable profile
scripts/run-client.sh bob        # terminal 3: a second one
```

In each window choose **Create account**. The first account becomes the workspace owner, and messages sent in one window appear in the other.

To go through the first-run screens as a new person would, use a server that needs codes and a client that is not told where the server is:

```sh
# On a fresh database:
LAZY_SETUP_CODE=$(openssl rand -hex 24) scripts/run-server.sh --seed-registration invite_only

# In another terminal:
env -u LAZY_SERVER_URL LAZY_PROFILE_DIR=.dev/profiles/newcomer cargo run -p lazy-desktop --release
```

Enter `http://127.0.0.1:8080` and the setup code. Profiles live in `.dev/profiles/<name>`: delete one to start fresh. `scripts/dev-db.sh reset` wipes the dev database.

### Build the programs

```sh
cargo build --release -p lazy-server -p lazy-desktop
# target/release/lazy-server   and   target/release/lazy-desktop
```

To run the server without Docker, give it a Postgres: `lazy-server serve --database-url postgres://user:password@host:5432/db`, with `LAZY_SETUP_CODE` and `LAZY_PUBLIC_URL` set as above. It applies its migrations and creates the workspace the first time it starts.

## If something goes wrong

| What you see | What to do |
| --- | --- |
| The server will not start and mentions the setup code | The code must be 20 to 64 letters, digits, `-` and `_`; `openssl rand -hex 24` makes one. If people have signed up already, remove `LAZY_SETUP_CODE`: it does nothing now. |
| "Can't reach this server" | Check the address, that the server is running (`curl <address>/healthz`), and that HTTPS and any proxy are set up. If the app or the server is newer than the other, update the older one. |
| "That invitation code isn't valid" | It may have expired, been revoked, or been used up. Ask for a new one. |
| The app cannot store its session | Start a keyring such as GNOME Keyring, or set `LAZY_INSECURE_STORAGE=1` for development. |
| Anything else | **Menu, Help, Diagnostics…** shows a report with no secrets. **Copy diagnostics** puts it on the clipboard for a bug report. |

## Status

Phases 1 to 3 of the implementation plan are built: the MVP; channels, direct and group messages, favorites, and unread counts; formatted messages, editing, deleting, and message links; emoji, reactions, mentions, and user groups; threads; text selection and copy; incoming webhooks; attachments, custom emoji, and search; notifications; discovery; workspace administration; several workspaces and profile pictures; commands, menus, and logging. You can create an account or sign in, send messages with durable and idempotent delivery, receive messages from other clients in realtime, page older history, and recover after disconnects and restarts. What is left of Phase 3 is using it with real people ([llm-docs/DOGFOOD.md](llm-docs/DOGFOOD.md)). Huddles, and everything in Phase 4 (integrations), are later.

The server, the client core and runtime, and the message-delivery guarantees are covered by automated tests, including fault injection (see [llm-docs/VERIFICATION.md](llm-docs/VERIFICATION.md)). The desktop client has been run and looked at on Linux. The macOS and Windows builds come from the same code but had never been run on those systems before this release, and input methods, screen readers, file dialogs, and different display scales have not been checked anywhere yet. The Docker setup builds, starts, and keeps its data across restarts in a test run on GitHub, but it is new and has not had much use on anyone's own server. Please report anything that looks or behaves wrong, with the report from **Menu, Help, Diagnostics…**.

A tour of what each part of the app does, with what to try on a real desktop, is in [llm-docs/TOUR.md](llm-docs/TOUR.md). Known gaps are at the end of [llm-docs/VERIFICATION.md](llm-docs/VERIFICATION.md).

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets
scripts/check-core-deps.sh        # the shared crates must not depend on the UI, an async runtime, HTTP, or SQL
scripts/test.sh                   # runs the workspace tests on a separate Postgres (port 54330),
                                  #   so a server you are running by hand is never disturbed
cargo run -p lazy-ui --example screens -- .dev/screens   # render the real views offscreen to PNGs
```

The GitHub workflows (`ci`, `release`, `server`, `screens`) run only when you start them: **Actions, the workflow, Run workflow**, or `gh workflow run ci`.

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

The product plan is in [llm-docs/PLAN.md](llm-docs/PLAN.md), the phased work breakdown in [llm-docs/IMPLEMENTATION_PLAN.md](llm-docs/IMPLEMENTATION_PLAN.md), and recorded design decisions in [llm-docs/DECISIONS.md](llm-docs/DECISIONS.md).
