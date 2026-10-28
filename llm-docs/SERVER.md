# Lazy server

How `crates/lazy-server` works and how to run it. The contracts it implements are in [DECISIONS.md](DECISIONS.md) (D2 sign-in, D3 ordering, D4 submission, D11 conversations and read state) and the rustdoc of `lazy-protocol` (protocol version 2).

## Running it

```sh
scripts/dev-db.sh start                 # project-local Postgres on 127.0.0.1:54329
export DATABASE_URL=postgres://lazy@127.0.0.1:54329/lazy
cargo run -p lazy-server -- serve       # listens on 127.0.0.1:8080
```

`serve` applies the embedded migrations, starts the event-log pruner (see [Retention](#retention)), and, if no workspace exists, creates the workspace **Lazy** with a public `#general` channel. Both are safe to repeat and safe to run from several servers at once (an advisory lock guards the seed). Registering adds a person to that workspace and to its **default channel** (`#general`) only; other public channels are found by browsing and joined explicitly. The first person to register becomes the workspace's owner.

| Option | Environment | Default |
| --- | --- | --- |
| `--database-url` | `DATABASE_URL` | required |
| `--listen` | `LAZY_LISTEN` | `127.0.0.1:8080` |
| `--open-registration <bool>` | `LAZY_OPEN_REGISTRATION` | `true` |
| `--event-retention-days <n>` | `LAZY_EVENT_RETENTION_DAYS` | `30` (`0` keeps every event) |
| `--no-migrate`, `--no-seed` | | migrate and seed |

`lazy-server migrate` applies migrations and exits. `RUST_LOG` controls logging (default `info,sqlx=warn`). Passwords, tokens, database URLs, and message bodies are never logged. `SIGINT` or `SIGTERM` starts a graceful shutdown: the listener stops, every WebSocket gets a close frame (code 1001), connections are given up to 10 seconds to finish, and the dispatcher and pruner stop.

A smoke test from a shell:

```sh
curl -s localhost:8080/healthz
curl -s -X POST localhost:8080/v1/auth/register -H 'content-type: application/json' \
  -d '{"username":"ada","display_name":"Ada","password":"correct horse battery"}'
```

## Modules

| Module | Responsibility |
| --- | --- |
| `app` | `run` (pool, migrate, seed, serve) and `serve` (an already-bound listener and pool); shared `AppState`; shutdown |
| `http` | Routes, extractors (JSON, query, path, bearer auth) that report failures as `ApiError`, handlers |
| `accounts` | Register, login, logout, session authentication |
| `messages` | Send (the transaction below), history paging, operation status |
| `conversations` | Creating channels, opening direct and group conversations, browsing, joining, leaving, adding and removing members, renaming, archiving, favorites, mark-read ([Conversations](#conversations-and-read-state)) |
| `receipts` | Idempotency records shared by sends, channel creation, and opening conversations |
| `bootstrap` | The consistent snapshot a client starts from |
| `events` | Appending to the log, reading it, and filtering each event by its stored [audience](#event-audiences) against current membership |
| `dispatcher` | Tails the log and fans batches out to subscribers |
| `pruner` | Retention: deletes aged-out events and raises the workspace's `event_floor` |
| `realtime` | One WebSocket connection and its subscriptions |
| `crypto` | Argon2id hashing (off the executor, bounded concurrency), token generation and hashing, request fingerprints |
| `db`, `config`, `error` | Pool, migrations, seeding; configuration; the error type and its mapping to `ApiError` |
| `testing` (feature `testing`) | `spawn_test_server(pool)` for in-process end-to-end tests |

The service modules take a `PgPool`, so tests can call them directly (for example to simulate a lost acknowledgment).

## Schema

Migrations `0001_init.sql` (messaging), `0002_event_floor.sql` (retention), and `0003_conversations.sql` (conversations, read state, audiences). Queries are runtime-checked SQLx, so building never needs a database.

| Table | Notes |
| --- | --- |
| `users` | `username` stored lowercase with a unique constraint; Argon2id hash |
| `sessions` | Only `sha256(token)` is stored; `expires_at` (30 days) and `revoked` |
| `workspaces` | `event_head`: the newest position in the workspace's event log; `event_floor`: the highest pruned position (migration 0002) |
| `workspace_members` | Role: `owner`, `admin`, `member` |
| `conversations` | `kind` is `channel`, `direct`, or `group`. `name` is present exactly for channels and unique per workspace; `description`, `created_by`, `is_default` (unique per workspace), `visibility` (direct and group are always private), `archived`, `member_count`, and `participant_key` for direct and group conversations, unique per workspace |
| `conversation_members` | Membership of every kind of conversation |
| `conversation_user_state` | Per person per conversation: `favorite` and `last_read` (a message position, 0 for none). A row exists exactly while the person is a member |
| `messages` | Versioned `document` (jsonb), `plain_text` projection, `revision`, `seq`, `created_at`; unique, covering `(conversation_id, seq DESC) INCLUDE (author_id)` |
| `operation_receipts` | PK `(user_id, operation_id)`, `kind` (`send_message`, `create_channel`, `open_conversation`), `request_hash`, and either `message_id` or `conversation_id` (both deferrable foreign keys) |
| `events` | PK `(workspace_id, seq)`; `kind`, nullable `conversation_id`, `operation_id`, the stored `audience` and `target_user_id`, and the serialized `Event` as `payload`. Positions `1..=event_floor` have been pruned |

## Ordering: how it is implemented

Every event-producing write runs in one transaction that calls `events::next_position`:

```sql
UPDATE workspaces SET event_head = event_head + 1 WHERE id = $1 RETURNING event_head, clock_timestamp()
```

The row lock is held until the transaction ends. A second writer's `UPDATE` waits, so position `n + 1` cannot be assigned until the transaction holding `n` has committed or rolled back. Hence positions are assigned in commit order, a rollback leaves no hole, and if event `n + 1` is visible to any reader, event `n` is too.

### The send transaction

1. Lock the conversation row `FOR SHARE`, then load the caller's facts and evaluate `ChannelAccess`. A conversation the caller cannot read is `not_found` (so private channels do not reveal themselves); readable but not postable is `forbidden`.
2. Claim the receipt: `INSERT ... ON CONFLICT (user_id, operation_id) DO NOTHING`. If it was not inserted, the existing receipt decides: same request fingerprint returns the original message (identical response), a different fingerprint is `conflict`. Two simultaneous duplicates serialize on this insert; the second waits for the first's outcome.
3. `next_position`, then insert the message (`seq` = that position, `created_at` = the database clock read under the lock).
4. Move the author's `last_read` to the message's position, in this transaction and **without an event**: someone who has just written has read everything before it.
5. Insert the event (audience `members`) and `pg_notify('lazy_events', workspace_id)`.
6. Commit. Only then is the acknowledgment returned.

Because a send holds the conversation row `FOR SHARE`, any number of sends run together, but never alongside an archive or a membership change (which take it exclusively). No message can therefore appear in the log after the event that archived its channel, or after its author's removal.

The fingerprint covers the conversation id and the document, so an operation id reused anywhere else is a conflict. Rejected sends (authorization failures) create no receipt.

Registration takes an advisory lock so registrations take turns (the first owns the workspace even when two arrive together), inserts the user and membership, joins the default channel, appends `member_joined` (audience `workspace`) and `member_added` for `#general` (audience `members`), creates the session, and commits.

### Bootstrap

One `REPEATABLE READ`, read-only transaction reads the workspace's `event_head` together with the caller's conversations (member conversations only, with participants for direct and group ones), their favorite and read state and unread counts, and the members. Because an event and its `event_head` bump commit atomically, the snapshot contains exactly the effects of events up to `head`. A client that subscribes with `after = head` neither misses nor repeats anything.

## Conversations and read state

`conversations.rs` implements every command in `lazy_protocol::api::paths` beyond messaging. They share one shape (the function docs have the detail):

1. **Lock the conversation row**: `FOR NO KEY UPDATE` to change it, `FOR SHARE` to act inside it.
2. **Load facts, then authorize** with the domain's `ConversationFacts`. A conversation the caller cannot see (a private channel they are not in, or one outside their workspace) is `not_found`, for everyone including the owner and admins. One they can see but may not do this to is `forbidden`.
3. **Write rows**: memberships, state rows, the conversation, `member_count`.
4. **Append events last** with `events::append_all`, which takes the workspace row lock for the position bump.

**Lock order is conversation row, then workspace row, always**, and nothing waits on anything else once it holds the workspace lock. Operations that wait on a unique index (creating a channel, opening a conversation) do so before that point. This is why the concurrent cases below cannot deadlock.

| Command | Allowed | Events appended |
| --- | --- | --- |
| Create channel | any workspace member | `conversation_created` (members) |
| Open direct/group | any workspace member, everyone named in the workspace, 2 to 9 people | `conversation_created` (members), only if it was created |
| Update name | the creator or an admin/owner; not archived | `conversation_updated` |
| Update topic/description | a member; not archived | `conversation_updated` |
| Join | anyone in the workspace, public and unarchived | `member_added` |
| Leave | a member of a channel, never the default channel | `member_removed` (members and the leaver), `conversation_updated` (new count) |
| Add members | a member; or an admin/owner for a *public* channel they are not in; not archived | one `member_added` per person newly added |
| Remove member | the creator or an admin/owner, never from the default channel | as leave |
| Archive / unarchive | the creator or an admin/owner; never the default channel | `conversation_updated` |
| Favorite | a member | `user_state_changed` (that person), if it changed |
| Mark read | a member | `user_state_changed` (that person), if it advanced |

Repeating a command that is already in effect (joining a channel you are in, archiving an archived channel, setting the same favorite) succeeds and appends nothing. A request that changes nothing (a topic equal to the current one, an empty update) appends nothing. A private channel is never reachable by an admin who is not in it, for any command.

**Idempotency.** Creating a channel and opening a conversation claim a receipt like a send does (`kind` distinguishes the commands, so an operation id reused across commands is a `conflict`), and `GET /v1/operations/{id}` answers `conversation_committed` for them. Channel names are arbitrated by the unique `(workspace_id, name)`: of simultaneous creates exactly one wins and the others get `name_taken`. Opening is arbitrated by the unique `(workspace_id, participant_key)` with `ON CONFLICT DO NOTHING`: the loser waits for the winner, finds its row, and returns it; only the winner appends `conversation_created`. Different operation ids for the same set, from either participant, all return the one conversation.

**Read state.** `last_read` is a message position. The unread count is messages from *other* people after it. A person's own send moves it (step 4 above). `MarkRead` takes the maximum of the current position and `min(up_to, newest message)`, so it only moves forward and cannot point past the end; it returns the resulting state. Someone who joins (or is added) starts with `last_read` at the conversation's newest message, so history from before they arrived is not "unread" (the protocol leaves this open; the alternative would show a new member every old message as unread). State rows are deleted on removal, so rejoining starts fresh, favorite included.

**`member_count`** is maintained by the same transactions that change membership, under the conversation row lock, and a test checks after concurrent churn that it equals `count(*)` of the members and that state rows exist exactly for the members. A removal appends a `conversation_updated` carrying the new count (the `member_removed` event has none); an addition's `member_added` already carries the count as of that person.

### Unread query plans

The bootstrap unread count for one conversation is `SELECT count(*) FROM messages WHERE conversation_id = $1 AND seq > $2 AND author_id <> $3`, run once per member conversation inside the bootstrap snapshot, with the person's read position as a concrete value. Measured on a seeded database with one 100,000-message conversation (80% from others) and 200 other conversations of 500 messages:

| Case | Plan | Buffers | Time |
| --- | --- | --- | --- |
| 10 unread in the 100k conversation | Index Only Scan on `messages_conversation_seq_idx`, 0 heap fetches | 4 | 0.04 ms |
| 100 unread among 200 other conversations | Index Only Scan, 0 heap fetches | 5 | 0.04 ms |
| 80,000 unread (nothing ever read) | Parallel Seq Scan (the right plan: 80% of the table matches) | 3637 | 11.7 ms |

Two things made this fast, both found by looking at the plan. The unread count was first written as a correlated subquery inside the member-conversations query; the planner cannot see the outer row's `last_read` and assumes a third of the table matches, so it scanned all 100,000 messages to find 8 (2128 buffers, 2.9 ms). Running one query per conversation with the actual position fixes that. And `messages_conversation_seq_idx` now carries `author_id` (`INCLUDE`), so the author filter needs no heap access.

## Event audiences

Each event stores who may receive it, when it is appended, and both replay and live delivery use that column. It is always evaluated against **current** membership, read from the database after the events were read and once per distinct conversation in a batch (`events::visibility`).

| Audience | Delivered to | Events |
| --- | --- | --- |
| `workspace` | every workspace member | `member_joined` |
| `members` | the current members of the event's conversation | `message_created`, `conversation_created`, `conversation_updated`, `member_added` |
| `members_and_user` | the current members, and the named person even if they are not (or no longer) one | `member_removed` |
| `user` | the named person, and only while they are a member of the event's conversation | `user_state_changed` |

What follows from evaluating at delivery time:

- Someone removed from a conversation receives their own `member_removed` and nothing else about it afterwards, **including events that committed before the removal but were not yet delivered, and events replayed later from an old cursor.** Their stream may or may not contain earlier events that were delivered live before the removal committed; what can never happen is anything after it.
- Someone who joins later receives the conversation's older events when they replay past them (members see the full history). A client already connected does not: it fetches history on `member_added`.
- A `member_added` for a person who has since left is not delivered to them, so a replay never shows a conversation flashing into the sidebar and out again.
- `user_state_changed` is invisible to everyone but its owner, and stops reaching the owner when they leave the conversation.
- Pruning is unaffected: the pruner deletes a prefix regardless of audience.

## The dispatcher

One task tails the log; a second feeds it wake-ups.

- A listener task holds a `LISTEN lazy_events` connection (reconnecting with backoff) and pokes the dispatcher on each notification. Notifications are hints; nothing depends on receiving them.
- The dispatch loop wakes on a poke or every 500 ms. For each workspace that has had a subscriber it reads `events WHERE seq > dispatched ORDER BY seq LIMIT 200`, publishes the page as a `Batch { after, last_seq, events }` on a bounded broadcast channel (256 batches), and advances `dispatched`. It advances only after a successful read, so a database error is simply retried on the next tick.
- A workspace's stream starts at the log head when its first subscriber arrives. Older history is served from the database, not the dispatcher.

Because the log is gapless and commit-ordered, the dispatcher's batches are contiguous: each batch's `after` equals the previous batch's `last_seq`.

## Retention

Without a bound the event log would grow forever, so the server prunes it.

**What is kept.** Events older than the retention window (default 30 days) are deleted, except that the newest 1000 events of every workspace are always kept, however old. `workspaces.event_floor` records the highest position deleted: events `1..=event_floor` no longer exist, and positions stay gapless from `event_floor + 1` to `event_head`. A cursor `after` is replayable if and only if `after >= event_floor`. Anything older cannot be completed from the log, so the subscriber is sent `snapshot_required` with reason `cursor_expired` and bootstraps again.

**The pruner** (`pruner.rs`) is a task the server owns and stops on shutdown, like the dispatcher. It runs once at startup and then every hour (`RetentionPolicy::interval`, shortened in tests). For each workspace it runs bounded transactions of at most 5000 deletions each; one transaction:

1. `SELECT event_head, event_floor ... FOR NO KEY UPDATE`: the lock a writer's position bump takes, so pruning and writing exclude each other. (`FOR UPDATE` would also block the foreign-key checks that inserts make against the row.)
2. Work out the protected top: positions above `event_head - 1000` are never touched.
3. Of the oldest candidates above the current floor, take the newest whose `occurred_at` is outside the window. That position becomes the new floor.
4. `DELETE` every event at or below it, and set `event_floor`. The floor only ever rises, because candidates start above the old one.

A pass that is told to stop finishes its current chunk (a started server always completes at least one chunk of one workspace) and does no more. A database error is logged and the next interval retries.

**Why the ordering guarantees still hold.**

- *Positions are unchanged.* Pruning deletes a prefix; it never renumbers, never assigns a position, and never touches the newest 1000 events, so D3's argument (gapless, commit-ordered, `n + 1` visible implies `n` visible) applies to the surviving range exactly as before.
- *Pruning and writing never interleave.* A writer holds the workspace row lock from its position bump to its commit; the pruner takes the same lock. A prune therefore never sees a half-committed event and a writer never sees a half-pruned log.
- *Readers cannot see a prune half done.* The delete and the floor update commit together. `events::fetch_after` reads the floor and the page of events in **one SQL statement** (a `LEFT JOIN LATERAL`, which also keeps the read an index range scan bounded by `LIMIT`), so it sees the log and floor either both before the prune or both after. If the floor it returns is above the cursor, the read did not cover `(cursor, floor]`, and the caller must not treat the page as contiguous.
- *Replay cannot silently skip.* Every page of a replay checks that. A prune landing between two pages raises the floor above the cursor, so the next page reports `cursor_expired` rather than continuing from after the hole.
- *Live delivery cannot skip either.* The dispatcher reads with the same function. If pruning ever overtakes it (it stalled for longer than the log retains), its next batch starts at the floor instead of claiming contiguity with its old position. A subscriber then sees `batch.after > cursor`, replays, and is told to bootstrap. In normal operation the dispatcher is within one poll interval of the head, far above the floor.
- *What is unaffected.* History pages come from `messages`, bootstrap from the current tables, and operation status from `operation_receipts`, none of which the pruner touches. Receipts keep their current retention (indefinite; see Known limits).

## Realtime connections

`GET /v1/realtime` authenticates the `Authorization: Bearer` header before upgrading. Each `Subscribe` spawns a task owned by the connection (a `JoinSet`), which:

1. confirms workspace membership (otherwise an `error` frame with `forbidden`, and the connection stays open);
2. **subscribes to the broadcast first**;
3. answers a cursor beyond the head with `snapshot_required` (`cursor_ahead`);
4. replays committed events after the cursor from the database in pages, answering `snapshot_required` (`cursor_expired`) if retention has pruned events after the cursor, including between two pages of the same replay;
5. sends `caught_up` with its cursor;
6. drains the broadcast, skipping anything at or before the cursor.

Everything the dispatcher published before step 2 was committed before step 4's reads, so replay covers it; everything after arrives on the broadcast. There are two further guards, both reusing the replay path: a `Lagged` notice from the broadcast, and a batch whose `after` is beyond the cursor, each trigger a replay from the database.

**Delivery is checked on every batch** against each event's stored [audience](#event-audiences) and the viewer's current membership, so no per-connection cache can go stale. Losing membership of a conversation stops its events immediately (and its history on later replays). Losing workspace membership ends the subscription with a `forbidden` error frame.

**Back-pressure.** Each connection has a bounded outbound queue (256 frames). When it fills, the subscription task waits, stops draining its broadcast receiver, and recovers by replaying from the database once it is more than the channel capacity behind. A single socket write that takes longer than 15 seconds closes the connection. A queue that cannot even accept a ping closes it. The server pings every 20 seconds, drops a connection that has been silent for 60 seconds, and ends a connection whose session was revoked (checked on each ping).

Malformed or binary frames get an `error` frame (`invalid_request`) and the connection continues. Client messages are limited to 64 KiB.

## Tests

```sh
scripts/dev-db.sh start
cargo test -p lazy-server       # DATABASE_URL comes from crates/lazy-server/.env
```

`#[sqlx::test]` creates a throwaway database per test, so tests are isolated and parallel. 98 tests in all: 19 unit tests (crypto, configuration and CLI, pruner, dispatcher, audiences) and integration suites that run a real server on a loopback port and talk to it over HTTP and WebSocket.

| Suite | Covers |
| --- | --- |
| `auth.rs`, `messages.rs` | Sign-in, sessions, send idempotency and conflicts, authorization, history paging, bootstrap consistency, lost acknowledgments |
| `realtime.rs` | Replay then live, cursors, forbidden subscriptions, malformed frames, shutdown, and the stress test below |
| `retention.rs` | Pruning, expired and boundary cursors, a replay racing a prune |
| `conversations.rs` | Creating and opening, validation, operation status, and the concurrency cases (exactly one winner for a contested name, exactly one conversation and one event for simultaneous opens, repeats of one create) |
| `membership.rs` | The authorization matrix for update, join, leave, add, remove, archive and unarchive across owner, admin, creator, member and stranger, public/private/archived/default; `member_count` after concurrent churn |
| `read_state.rs` | Unread baseline, own sends advancing the position without an event, monotonic and clamped mark-read, favorites, delivery only to the owner's devices, late joiners |
| `delivery.rs` | The 2.7 hardening cases: a disconnected subscriber removed from a private channel replaying an **old cursor** gets only their removal while another member's device receives everything; removal while connected; leaving; late joiner replay; personal state; being added back; pruning personal events |
| `process_crash.rs` | Not part of this document |

The test that matters most is `nothing_is_lost_or_duplicated_while_clients_connect_mid_stream`: 8 users send 400 messages and 4 people register concurrently while 32 subscribers connect at staggered moments, **a pruner deletes 300 aged-out events from the front of the log as it goes, and a private channel's membership churns for the whole run** (an owner adds a person, posts, removes them, posts, 25 times). Every subscriber, none of whom is in the churn channel, must see exactly the events visible to it, once each, in order: any leak from the churn channel fails the comparison. The person being churned must receive each of their 25 removals exactly once and nothing about the channel after the last; the owner's replay must show all 125 channel events in order; the churned person's own fresh replay must show only the removals.

It was checked for sensitivity by mutation: subscribing after the replay with the contiguity guard disabled (fails with a lost event); removing the replay's floor check (a stale subscriber is handed events from position 17, silently skipping 1 to 16); delivering `members` events to every workspace member (the stress test fails with the churn events in a watcher's stream and five of the seven delivery tests fail); and dropping the removal-reaches-its-subject and personal-state-needs-membership rules (four delivery tests fail).

## Known limits

- **Write serialization.** Event-producing writes within one workspace serialize on the `event_head` row for the length of their transaction. This is intended (it is what makes positions commit-ordered) and fine at MVP scale. If it becomes a bottleneck, measure first, then consider per-conversation logs with a workspace-level merge.
- **Receipts are kept forever.** They are not pruned with the event log. A retention window and cleanup job are deferred to Phase 6, together with defining how long a client may rely on an operation id.
- **Retention knobs.** The window is configurable (`--event-retention-days`); the "newest 1000 events always kept" floor is a constant (`RetentionPolicy::DEFAULT_KEEP_NEWEST`), not a flag. Pruning removes a prefix, so one old event that precedes a young one is only removed once the young one ages out too (and a backwards clock step can briefly let a slightly-young event go with its prefix).
- **The between-pages race is tested at two levels.** That a prune landing between two replay pages is detected is tested deterministically on `fetch_after` and end to end only statistically (24 subscribers racing a prune); there is no hook to pause a live replay between pages.
- **A very long outage forces a bootstrap.** A client away for longer than the window (or past the newest 1000 events in a busy workspace) gets `cursor_expired`, which is the intended contract, but it makes the window the effective bound on offline reconnect.
- **Lag recovery is untested end to end.** It reuses the replay path that every subscription runs, which is covered, but forcing a real broadcast lag from a test client is not deterministic.
- **No rate limiting** on sign-in or sending.
- **No TLS** (terminate it in front of the server) and the SQLx build has no TLS feature for the database connection; enable one before connecting to a remote database.
- **No workspace administration API.** Channels are managed through the API, but workspace roles are changed only in the database (the tests do so with SQL), there is no invitation flow, and channels cannot be deleted.
- **Replay filters after reading.** A page of 200 log rows is read and then filtered by audience, so a person in few conversations of a busy workspace pages through many events they will not receive. Pushing the audience test into the query (join on membership) is the fix if it matters; correctness does not depend on it.
- **Sends and membership changes wait for each other.** A send holds the conversation row `FOR SHARE`, and joins, leaves, adds, removes, archives and renames hold it exclusively, so a burst of membership changes in one very busy channel briefly queues its sends (and the reverse). That buys the property that nothing is posted after an archive or by someone already removed.
- **`mention_count` is always 0** until mentions exist, and unread counts treat every message from another person as unread.
- **Browsing** returns at most 1000 public channels, archived ones included (clients filter on `archived`).
- **`GET /v1/operations/{id}` for a created conversation** returns the conversation as it is now, not as it was created, and does not re-check that the caller can still see it.
- **One dispatcher per server process.** Several server processes work against one database (each tails the same log), but that has not been exercised.
