# Lazy server

How `crates/lazy-server` works and how to run it. The contracts it implements are in [DECISIONS.md](DECISIONS.md) (D2 sign-in, D3 ordering, D4 submission) and the rustdoc of `lazy-protocol`.

## Running it

```sh
scripts/dev-db.sh start                 # project-local Postgres on 127.0.0.1:54329
export DATABASE_URL=postgres://lazy@127.0.0.1:54329/lazy
cargo run -p lazy-server -- serve       # listens on 127.0.0.1:8080
```

`serve` applies the embedded migrations, starts the event-log pruner (see [Retention](#retention)), and, if no workspace exists, creates the workspace **Lazy** with a public `#general` channel. Both are safe to repeat and safe to run from several servers at once (an advisory lock guards the seed). Registering adds a person to that workspace and to its public channels; the first person to register becomes its owner.

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
| `messages` | Send (the transaction below), history paging, operation status, access facts |
| `bootstrap` | The consistent snapshot a client starts from |
| `events` | Appending to the log, reading it, filtering by current access |
| `dispatcher` | Tails the log and fans batches out to subscribers |
| `pruner` | Retention: deletes aged-out events and raises the workspace's `event_floor` |
| `realtime` | One WebSocket connection and its subscriptions |
| `crypto` | Argon2id hashing (off the executor, bounded concurrency), token generation and hashing, request fingerprints |
| `db`, `config`, `error` | Pool, migrations, seeding; configuration; the error type and its mapping to `ApiError` |
| `testing` (feature `testing`) | `spawn_test_server(pool)` for in-process end-to-end tests |

The service modules take a `PgPool`, so tests can call them directly (for example to simulate a lost acknowledgment).

## Schema

All in `migrations/0001_init.sql`. Queries are runtime-checked SQLx, so building never needs a database.

| Table | Notes |
| --- | --- |
| `users` | `username` stored lowercase with a unique constraint; Argon2id hash |
| `sessions` | Only `sha256(token)` is stored; `expires_at` (30 days) and `revoked` |
| `workspaces` | `event_head`: the newest position in the workspace's event log; `event_floor`: the highest pruned position (migration 0002) |
| `workspace_members` | Role: `owner`, `admin`, `member` |
| `conversations` | `kind = 'channel'`; name unique per workspace; `visibility`, `archived` |
| `conversation_members` | Channel membership |
| `messages` | Versioned `document` (jsonb), `plain_text` projection, `revision`, `seq`, `created_at`; unique `(conversation_id, seq DESC)` |
| `operation_receipts` | PK `(user_id, operation_id)`, `request_hash`, `message_id` (deferrable FK) |
| `events` | PK `(workspace_id, seq)`; `kind`, nullable `conversation_id`, `operation_id`, the serialized `Event` as `payload`. Positions `1..=event_floor` have been pruned |

## Ordering: how it is implemented

Every event-producing write runs in one transaction that calls `events::next_position`:

```sql
UPDATE workspaces SET event_head = event_head + 1 WHERE id = $1 RETURNING event_head, clock_timestamp()
```

The row lock is held until the transaction ends. A second writer's `UPDATE` waits, so position `n + 1` cannot be assigned until the transaction holding `n` has committed or rolled back. Hence positions are assigned in commit order, a rollback leaves no hole, and if event `n + 1` is visible to any reader, event `n` is too.

### The send transaction

1. Load the caller's access facts and evaluate `ChannelAccess`. A conversation the caller cannot read is `not_found` (so private channels do not reveal themselves); readable but not postable is `forbidden`.
2. Claim the receipt: `INSERT ... ON CONFLICT (user_id, operation_id) DO NOTHING`. If it was not inserted, the existing receipt decides: same request fingerprint returns the original message (identical response), a different fingerprint is `conflict`. Two simultaneous duplicates serialize on this insert; the second waits for the first's outcome.
3. `next_position`, then insert the message (`seq` = that position, `created_at` = the database clock read under the lock).
4. Insert the event and `pg_notify('lazy_events', workspace_id)`.
5. Commit. Only then is the acknowledgment returned.

The fingerprint covers the conversation id and the document, so an operation id reused anywhere else is a conflict. Rejected sends (authorization failures) create no receipt.

Registration is the same shape: insert the user, take the next position, add the memberships, insert a `member_joined` event, create the session, commit.

### Bootstrap

One `REPEATABLE READ`, read-only transaction reads the workspace's `event_head` together with conversations and members. Because an event and its `event_head` bump commit atomically, the snapshot contains exactly the effects of events up to `head`. A client that subscribes with `after = head` neither misses nor repeats anything.

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

**Access is checked on every delivery**, against the database, once per distinct conversation in a batch. Losing channel access stops that channel's events immediately (and its history on later replays). Losing workspace membership ends the subscription with a `forbidden` error frame.

**Back-pressure.** Each connection has a bounded outbound queue (256 frames). When it fills, the subscription task waits, stops draining its broadcast receiver, and recovers by replaying from the database once it is more than the channel capacity behind. A single socket write that takes longer than 15 seconds closes the connection. A queue that cannot even accept a ping closes it. The server pings every 20 seconds, drops a connection that has been silent for 60 seconds, and ends a connection whose session was revoked (checked on each ping).

Malformed or binary frames get an `error` frame (`invalid_request`) and the connection continues. Client messages are limited to 64 KiB.

## Tests

```sh
scripts/dev-db.sh start
cargo test -p lazy-server       # DATABASE_URL comes from crates/lazy-server/.env
```

`#[sqlx::test]` creates a throwaway database per test, so tests are isolated and parallel. 60 tests in all: 16 unit tests (crypto, configuration and CLI, pruner, dispatcher) and the integration suites (`tests/auth.rs`, `tests/messages.rs`, `tests/realtime.rs`, `tests/retention.rs`, and a `tests/process_crash.rs` that is not part of this document) run a real server on a loopback port and talk to it over HTTP and WebSocket.

`tests/retention.rs` covers pruning (the newest-1000 floor, the age window, repeat runs never lowering the floor, multi-chunk backlogs, shutdown), expired and boundary cursors, history/bootstrap/receipts being unaffected, and a replay racing a prune. The dispatcher's behavior when pruning overtakes it is a unit test in `dispatcher.rs`.

The test that matters most is `nothing_is_lost_or_duplicated_while_clients_connect_mid_stream`: 8 users send 400 messages and 4 people register concurrently while 32 subscribers connect at staggered moments, **and a pruner runs continuously, deleting 300 aged-out events from the front of the log as it goes**; every subscriber must see exactly the log's positions after its cursor, once, in order. It was checked for sensitivity by temporarily subscribing after the replay and disabling the contiguity guard (with both removed it fails with a lost event), and separately by removing the replay's floor check (the cursor-expiry test fails because a stale subscriber is handed events from position 17, silently skipping 1 to 16).

## Known limits

- **Write serialization.** Event-producing writes within one workspace serialize on the `event_head` row for the length of their transaction. This is intended (it is what makes positions commit-ordered) and fine at MVP scale. If it becomes a bottleneck, measure first, then consider per-conversation logs with a workspace-level merge.
- **Receipts are kept forever.** They are not pruned with the event log. A retention window and cleanup job are deferred to Phase 6, together with defining how long a client may rely on an operation id.
- **Retention knobs.** The window is configurable (`--event-retention-days`); the "newest 1000 events always kept" floor is a constant (`RetentionPolicy::DEFAULT_KEEP_NEWEST`), not a flag. Pruning removes a prefix, so one old event that precedes a young one is only removed once the young one ages out too (and a backwards clock step can briefly let a slightly-young event go with its prefix).
- **The between-pages race is tested at two levels.** That a prune landing between two replay pages is detected is tested deterministically on `fetch_after` and end to end only statistically (24 subscribers racing a prune); there is no hook to pause a live replay between pages.
- **A very long outage forces a bootstrap.** A client away for longer than the window (or past the newest 1000 events in a busy workspace) gets `cursor_expired`, which is the intended contract, but it makes the window the effective bound on offline reconnect.
- **Lag recovery is untested end to end.** It reuses the replay path that every subscription runs, which is covered, but forcing a real broadcast lag from a test client is not deterministic.
- **No rate limiting** on sign-in or sending.
- **No TLS** (terminate it in front of the server) and the SQLx build has no TLS feature for the database connection; enable one before connecting to a remote database.
- **No workspace or channel administration API** beyond registration. Tests create channels and change membership with SQL.
- **One dispatcher per server process.** Several server processes work against one database (each tails the same log), but that has not been exercised.
