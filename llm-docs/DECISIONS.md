# Lazy decisions

Answers to the open decisions in [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md), recorded when made. The wire contract itself lives in the rustdoc of `lazy-protocol`.

## D1. Toolchain and GPUI pin (task 0.1, 0.2)

- Rust `1.99.0`, edition 2024, pinned in `rust-toolchain.toml`.
- GPUI and `gpui_platform` are pinned to Zed revision `e5bdd01cd819d6348ae764e69218adfee3d7edab` in the workspace `Cargo.toml`. Both must always share one revision.
- Zed's workspace `[patch]` entries (`async-task`, `calloop`, `khronos-egl`, and others) are not applied. A window crate built against this revision without them compiled on Linux (Wayland and X11 features) on 2026-10-28. Re-check this on each GPUI upgrade, and on macOS and Windows when those targets are added.
- Desktop targets for the MVP: Linux (Wayland, X11). macOS and Windows follow after the MVP; no Linux-specific code goes into shared crates.

## D2. Sign-in method (task 1.1)

Username and password for the MVP.

- Passwords are 8 to 256 bytes, hashed with Argon2id. Usernames are unique case-insensitively.
- A session is an opaque random 256-bit token. The server stores only its SHA-256 hash, with an expiry (30 days) and a revoked flag. Clients send it as `Authorization: Bearer`.
- Registration is open in development. Registering adds the person to the default workspace and its public channels. Production sign-in (single sign-on or email links) replaces this later without changing the session mechanism.

## D3. Commit-safe ordering (task 1.5)

Each workspace has one durable, gapless event log.

- `workspaces.event_head` holds the last assigned position. Every mutation that produces an event runs `UPDATE workspaces SET event_head = event_head + 1 ... RETURNING event_head` inside its transaction, then inserts the event at that position.
- The row lock taken by the update is held until commit. A transaction that gets position `n + 1` therefore cannot start until the transaction holding `n` has committed or rolled back. Positions are assigned in commit order and rolled-back transactions leave no gap. If event `n + 1` is visible to a reader, event `n` is too.
- This serializes event-producing writes within one workspace, which is acceptable for the MVP. The documented escape hatch if it becomes a bottleneck is per-conversation logs with a workspace-level merge; do not optimize before measuring.
- Timestamps and raw database sequences are never used for ordering.

### Snapshots, subscriptions, replay

- Bootstrap reads `event_head` and the conversation and member lists in one `REPEATABLE READ` read-only transaction, so the snapshot corresponds exactly to `head`.
- A single dispatcher tails each workspace's durable log from the database (woken by `LISTEN/NOTIFY`, with a short polling fallback) and publishes events to bounded in-process broadcast channels. Notifications are only wake-up hints.
- A connection subscribes to the broadcast first, then replays from the database starting after the client's cursor, then drains the broadcast skipping positions it already sent. Everything dispatched before the subscription is committed, so the replay query sees it; everything after arrives by broadcast. No committed event can be skipped.
- A receiver that lags behind the broadcast falls back to replaying from the database from its last sent position.
- Each delivered event is checked against the subscriber's current access. A cursor ahead of the log head yields `snapshot_required`.

## D4. Submission and acknowledgment (task 1.4)

- Operation receipts are keyed by `(user_id, operation_id)` with a hash of the request. The same id with the same payload returns the original result; the same id with a different payload is a `conflict`.
- A send is one transaction: authorization check, receipt, event-position bump, message, event, `pg_notify`. The acknowledgment is sent only after commit.
- Receipt retention: kept indefinitely in the MVP. A retention window with cleanup is deferred (Phase 6).

## D5. Client architecture (tasks 1.7, 1.8)

- `lazy-client-core` is a synchronous, I/O-free state machine: inputs in, effects and read-model changes out. It has no GPUI, Tokio, or filesystem dependency.
- `lazy-client-runtime` executes effects on its own Tokio runtime: HTTP, WebSocket, the encrypted journal and drafts files, and secure storage. Results return to the core as inputs over a channel.
- The core lives in a GPUI entity on the foreground executor; the runtime never touches GPUI.
- A submission is durably journaled (written, flushed, and atomically renamed into place) before its HTTP request is issued.

## D6. Local development database (task 0.4)

`scripts/dev-db.sh` runs a project-local Postgres in `.dev/pgdata` on `127.0.0.1:54329` and never touches the system instance. A second, independent instance (`LAZY_PG_INSTANCE=test`, `.dev/pgdata-test`, port 54330) is used by `scripts/test.sh`, so test runs never share a database or its connection limit with a server someone is running by hand. Server code uses runtime-checked SQLx queries, so building does not require a live database.

## D7. MVP scope

The MVP is the Phase 1 slice: register or sign in, one workspace with one channel (`#general`), composer, durable send with idempotent retry, realtime receive, history paging, reconnect with replay, and a GPUI desktop client. Everything after Phase 1 is out of scope until the MVP is verified.

## D8. Typography (task 0.7)

IBM Plex Sans (regular, italic, semibold, semibold italic) is bundled in `lazy-ui/assets/fonts/ibm-plex-sans` under the SIL Open Font License 1.1, with its license, copied unmodified from the pinned Zed revision. Without it the text follows whichever system font matches, which on this machine had no semibold weight. Emoji and scripts the face does not cover fall back to system fonts. Views use `FontWeight::SEMIBOLD` as their heaviest weight because that is the heaviest bundled.

## D9. Event retention, the content cache, and the cursor invariant (tasks 1.5, 1.7, 1.8)

- The server prunes events older than 30 days (`--event-retention-days`, `0` disables) but always keeps the newest 1,000 per workspace. `workspaces.event_floor` records the highest pruned position; a subscriber whose cursor is below it receives `snapshot_required` with reason `cursor_expired`. See `SERVER.md` for why pruning does not weaken the ordering guarantee.
- The client keeps one encrypted, bounded cache file per profile (`cache.enc`: at most 200 messages per conversation, 50 conversations, 4 MiB). It shows cached history at once on restart and resumes the realtime subscription from the cached cursor.
- **Invariant:** the cursor and the messages it covers are written together in one atomic write, and the cursor never runs ahead of the cached messages. Persisting them separately could silently lose messages after a crash.
- `snapshot_required` (expired or ahead) discards the cache and starts fresh. A corrupt cache is set aside and reported. See `CLIENT.md`.

## D10. Keyboard operation (task 1.9)

Tab and Shift+Tab move between tab-stop controls through GPUI's focus order; Enter or Space activates a focused control. Tab stops: the composer, Send, Sign out, sidebar channels, a failed message's retry button, and the sign-in form's fields and buttons. The hover Copy button is mouse-only for now.

## D11. Conversations and read state, slice 2A (tasks 2.1, 2.6, 2.7)

The wire types are in `lazy-protocol` (`PROTOCOL_VERSION` 2); the rules are in `lazy-domain::conversation`. Decisions that cut across layers:

**What the sidebar holds.** Bootstrap lists the conversations the caller is a *member* of. Public channels the caller has not joined are found by browsing (`GET .../channels/browse`) and joined explicitly. A new account joins only the default channel (`#general`, `is_default`), not every public channel.

**Who receives which event.** Message events go to a conversation's members only. `MemberAdded` and `MemberRemoved` also go to the person they are about, including a person who was just removed. `UserStateChanged` goes only to its owner. The server stores the audience with each event (workspace, conversation members, or one user) instead of inferring it at delivery time. Replay and live delivery use *current* membership: after removal, nothing further about the conversation reaches the removed person, not even events that committed earlier but had not been delivered. Someone who joins a channel receives its older events when they replay past them; clients ignore events for conversations they do not know yet and fetch history on `MemberAdded`, deduplicating by message id.

**Direct and group conversations.** Two people make a `direct` conversation, three to nine a `group`. Both are private, unnamed, and list their `participants`. Opening is idempotent per exact participant set (`ParticipantSet`). "Adding someone" to a group is opening a *new* group with the larger set: it starts with no history and the old group is untouched. Direct and group conversations cannot be left or archived.

**Channel management** follows `ConversationFacts` (see its tests): any member sets the topic and description and adds people; the creator or an admin renames, archives, unarchives, and removes members; admins can add people to public channels they have not joined but never bypass private channels; nobody leaves, archives, or is removed from the default channel. Archived channels stay readable and cannot be posted to. A name already in use is `name_taken`.

**Read state.** A person's read position in a conversation is a message position (`last_read`, 0 for none). Unread count is messages from *other* people after it. The server computes the baseline in bootstrap. Clients then maintain it incrementally: a message from someone else in a member conversation adds one; a message the person authored moves their read position to that message (the server does the same in the send transaction, without an event); `UserStateChanged` replaces the state (another device read or starred). `MarkRead` only moves forward and is clamped to the conversation's newest message. A client marks a conversation read when it is selected, its window is active, and the newest message is on screen. The timeline shows a "New messages" divider where the read position was when the conversation was opened, and keeps it until the person leaves the conversation.

**Idempotency.** Creating a channel and opening a conversation carry an operation id like sends do, and a lost response is reconciled through the operation status (`conversation_committed`). The other management commands are naturally idempotent (they set a state), so they carry none.

**Favorites** are per person (`ConversationUserState::favorite`) and sync across devices through `UserStateChanged`.
