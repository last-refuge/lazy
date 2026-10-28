# Lazy client: core and runtime

Two crates implement the client's behavior without any UI code:

| Crate | Role | Depends on |
| --- | --- | --- |
| `lazy-client-core` | A synchronous, I/O-free state machine. Person actions and runtime reports go in; effects and change notifications come out. | `lazy-domain`, `lazy-protocol` |
| `lazy-client-runtime` | Executes the core's effects: HTTP, realtime WebSocket, encrypted local files, secure storage. Reports results back as inputs. | `lazy-client-core`, Tokio, reqwest, tokio-tungstenite |

The UI (GPUI) owns a `ClientCore` and a `Runtime`, and only moves values between them. Neither crate knows about GPUI, so phone clients can reuse both.

```
person ──Input──▶ ClientCore ──Output { effects, changes }──▶ host
                      ▲                                         │
                      │                                         ├─ changes ▶ refresh views
         Input ◀──────┴──── Receiver<Input> ◀── Runtime ◀──────┘ effects
```

## Vocabulary

### `Input` (what goes into `ClientCore::handle`)

Person actions, sent by the UI:

| Input | Meaning |
| --- | --- |
| `Register { username, display_name, password }` | Raw text; the core validates before any request. |
| `SignIn { username, password }` | |
| `SignOut` | Revokes the session and deletes all local data and secrets. |
| `RetryLoad` | After `SessionPhase::Unreachable`. |
| `SelectConversation(id)` | Fetches the newest history page the first time (and again after a failure). |
| `SetDraft { conversation, text }` | Composer text changed. Saved with a debounce. |
| `SendMessage { conversation, text }` | Empty text is ignored; an accepted send clears the draft. |
| `RetryOperation { conversation, operation }` | For rows in `Delivery::Failed`. |
| `LoadOlder { conversation }` | Next page of older history. |
| `DismissNotice` | |

Conversation and read-state inputs (create a channel, open a conversation, browse, join, leave, add or remove people, edit, archive, favorite, `WindowActive`, `TimelineAtBottom`, `DismissAction`) are listed under [Conversations and read state](#conversations-and-read-state-slice-2a).

Runtime reports (the host forwards these from the `Receiver<Input>`; the UI never creates them): `Started`, `Authenticated`, `AuthFailed`, `SessionExpired`, `BootstrapLoaded`, `BootstrapFailed`, `HistoryLoaded`, `SubmissionPersisted`, `SubmissionPersistFailed`, `SendResult`, `OperationStatusResult`, `Socket(SocketEvent)`, `Frame(ServerFrame)`, `StorageProblem`.

`Started` is always the first input the runtime delivers. It carries the unsettled journal, saved drafts, the saved content cache (if one could be read), whether a session was restored, and any storage problems found while loading.

Request results use `RequestError`: `Rejected(ApiError)` means the server received the request and refused it (nothing committed); `Unknown(detail)` means the outcome may be anything (timeout, dropped connection, 5xx, unparseable 2xx).

### `Effect` (what comes out for the runtime)

`Register`, `Login`, `SignOut`, `ForgetSession`, `LoadBootstrap`, `Subscribe { workspace, after }`, `FetchHistory { conversation, before, limit, generation }`, `PersistSubmission(entry)`, `SendMessage { operation, conversation, document }`, `QueryOperationStatus { operation }`, `SettleSubmission { operation }`, `PersistDrafts(all drafts)`, `PersistCache(cache)`, `DiscardCache`, and for conversations: `CreateChannel`, `OpenConversation`, `BrowseChannels`, `JoinChannel`, `LeaveChannel`, `AddMembers`, `RemoveMember`, `UpdateConversation`, `ArchiveConversation`, `UnarchiveConversation`, `SetFavorite`, `MarkRead` (see [Conversations and read state](#conversations-and-read-state-slice-2a)).

Effects from one `handle` call may run concurrently. The core never emits an effect that depends on another's completion: a send is emitted only after its journal write is reported durable.

### `Change` (what the UI refreshes)

`PhaseChanged`, `ConnectionChanged`, `DirectoryChanged` (workspaces, conversations, members, names), `SelectionChanged`, `DraftChanged { conversation }`, `TimelineReset { conversation }`, `TimelineSplice { conversation, at, removed, inserted }`, `TimelineItemUpdated { conversation, index }`, `HistoryStatusChanged { conversation }`, `NoticeChanged`.

**Apply the changes of one `Output` in order, then read the final state from the core.** A timeline change describes the row list as it was at that point in the sequence, so replaying splices in order keeps a virtualized list's length structure in step with the model; row *contents* are read from the core's final state at render time. In GPUI terms: `ListState::splice(at..at + removed, inserted)` for a splice, `remeasure`/invalidate for `TimelineItemUpdated`, and reset the list state for `TimelineReset`. Prepending older history is always one splice at `0`.

### Read model (on `ClientCore`)

`phase()`, `connection()`, `me()`, `workspaces()`, `primary_workspace()`, `conversations()`, `conversation(id)`, `selected_conversation()`, `member(id)`, `display_name(id)` (falls back to `UNKNOWN_MEMBER_NAME`), `draft(conversation)`, `timeline(conversation)`, `history_status(conversation)`, `notice()`, `unsettled_operations()`.

Only call `SetDraft` from the composer's change events. Do not push `draft()` back into the composer on every `DraftChanged`; do it when the selected conversation changes, and after a send clears it.

## Timelines

`timeline(conversation)` is a `&[TimelineItem]`: confirmed messages sorted by event-log position (`seq`, never timestamps), then messages still being delivered in submission order. Each row has:

* `key: ItemKey`: `Operation(id)` for messages this client sent, which stays the key after confirmation so the row keeps its identity; `Message(id)` for everything else.
* `delivery`: `Confirmed { message_id, seq, created_at, revision }`, `Pending`, `UnknownOutcome` (asking the server whether it committed), or `Failed { reason, retry }`.
* `retry`: `NewOperation` (the server definitively refused; retrying composes a fresh operation from the same text) or `SameOperation` (outcome unknown; retrying asks the server first, then resends the *same* operation if needed).

Invariants, checked after every step of the randomized simulation: confirmed rows precede unconfirmed rows, confirmed rows are strictly ordered by `seq`, no message id appears twice, no row key appears twice.

## Sending: the state machine

```
SendMessage ─ empty ─▶ ignored (no operation)
     │
     ▼
 Pending ── PersistSubmission ─▶ (journal flushed) ── SubmissionPersisted ─▶ Queued
     │                                  │ SubmissionPersistFailed              │ its turn (see "The send queue")
     │                                  ▼                                      ▼
     │                         Failed{NewOperation}  (journal entry settled)   SendMessage effect
     ▼
 SendResult
   Ok(ack) ───────────────────────────▶ Confirmed ── SettleSubmission
   Rejected (definitive) ─────────────▶ Failed{NewOperation} ── SettleSubmission
   Unknown (timeout / 5xx / garbled) ─▶ UnknownOutcome ── QueryOperationStatus
                                              │
                    Committed{message} ───────┼──▶ Confirmed ── SettleSubmission
                    NotFound ─────────────────┼──▶ Queued again: resend the SAME operation id (≤ 3 times)
                    query itself failed ──────┴──▶ Failed{SameOperation}  (journal entry kept)
```

### The send queue

Messages in a conversation go out **one at a time, in the order they were submitted**, so the server commits them in that order. Every submission is still journaled immediately and appears as `Pending` in submission order; only its `SendMessage` effect waits for its turn.

* The head of a conversation's queue is its earliest operation that is not `Failed`. Only the head may have a send in flight. If the head is still being journaled, being sent, or being checked with the server (`UnknownOutcome`, including a resend after "no record"), everything behind it waits, so a later message can never overtake an earlier one whose outcome is not yet known.
* Conversations have independent queues.
* A `Failed` message does not hold anything up, whether the server definitively rejected it or the client gave up with an unknown outcome. Its successor is released the moment it fails.
* After a restart, recovered journal entries are queued first, ordered by `submitted_at` (ties keep journal order), before any message typed after startup. All of them are checked with the server first, in parallel; the resends that follow still go one at a time in that order.
* Retrying a `NewOperation` failure composes a new submission at the back of the queue (its row is replaced by a new one at the bottom). Retrying a `SameOperation` failure asks the server first, and if the server has no record, resends the **same** operation id **at the back of the queue**: the row moves to the bottom of the timeline too, because later messages may already have been sent while it was given up on.
* Nothing is released while the session is not `Ready`; queued messages go out when the app is ready again. Session expiry fails every unfinished message instead of releasing the queue into refused requests.
* Tested by 12 scenario tests (20 rapid sends one at a time and in order with no row moving; slow journal write; failure and unknown outcome behind and ahead of others; recovery order; both retry kinds; expiry; unreachable server) and by the randomized simulation, which asserts that, for one sender, committed order equals submission order for every message the client never gave up on. With the queue disabled the simulation fails ("committed out of submission order").

Rules the code enforces and the tests cover:

* The journal write is durable before the HTTP request exists.
* An unknown outcome never produces a new operation id. Only `RetryOperation` on a `NewOperation` failure composes a new one.
* The live `MessageCreated { operation_id }` event and the acknowledgment may arrive in either order; the message appears once and `SettleSubmission` is emitted once. A history page that already contains the message is merged into the local row.
* After a restart, journal entries for the signed-in person appear as `UnknownOutcome` rows and are checked with the server before anything is resent. Entries for other people are ignored (and kept).
* `Unauthenticated` anywhere signs the person out (`ForgetSession`) and turns unfinished sends into `Failed{SameOperation}`, so signing in again and retrying reconciles them.

## Synchronizing

1. **First run** (no usable cache). `LoadBootstrap` → `BootstrapLoaded`. The core resets confirmed content (rows still being sent survive), records `head` as the applied position, emits `Subscribe { workspace, after: head }`, selects `#general` (or the first conversation), and fetches its newest page.
2. Events are applied only if `seq` is greater than the applied position, then deduplicated by message id. `CaughtUp` advances the position and makes the connection `Live`.
3. `SnapshotRequired` (cursor expired **or** ahead of the server's log): the core discards the content cache (`DiscardCache`), clears confirmed content, and asks for a fresh bootstrap (not duplicated if one is in flight); the answer is handled as a first run. Nothing is cached in between, because the cursor on hand is known to be bad.
4. **Restart with a cache**: see below.
5. The runtime tracks the last position it forwarded per workspace and resubscribes from it on reconnect, so the core does not drive reconnects. A repeated `Subscribe` for a workspace replaces the position, on the open connection if there is one. After a `snapshot_required` frame the runtime does **not** reconnect on its own: the position it holds is known to be refused, so it waits for the core's fresh `Subscribe` (otherwise it would just be refused again).

### History

`FetchHistory` carries a per-conversation `generation`. A response is applied to the conversation it was requested for (never to whatever is selected), and only if its generation is still current; resets and newer requests supersede older ones. `LoadOlder` pages from the *history floor*, the lower bound of the contiguous range loaded from history, **not** from the oldest row: a message sent from this client can be acknowledged at a position below the newest page, and paging from that row would skip the gap (found by the simulation; regression-tested).

## The content cache

The client keeps a bounded, encrypted, **regenerable** cache so a restart shows the last conversation instantly and resumes from where it stopped. The cache can be absent, trimmed, or thrown away at any time; the client then behaves as on a first run.

**What it holds** (`ContentCache`): who it belongs to; per workspace, the directory from the last bootstrap (workspace, role, conversations, members), the **applied event cursor**, and, for each conversation whose history was loaded, the newest ≤ 200 *confirmed* messages plus a `has_more` flag for older ones. Pending and failed messages are never cached (the journal owns those). A conversation that has only seen live events is left out: its timeline is partial, so it fetches its newest page when opened, as on a first run.

**The invariant: the cursor never runs ahead of the messages it covers.** For every cached conversation, all messages between the oldest cached one and the cursor are present; if older ones exist, `has_more` is true. Messages *beyond* the cursor may be present (replay re-delivers and deduplicates them); a cursor *behind* the messages is always safe. This is guaranteed three ways: the cursor and messages are one value written in one atomic file replacement (`cache.enc`), so a crash leaves the old cache or the new one, never a mixture; the core only caches contiguous ranges (from the *history floor* up, so a message acknowledged below the loaded range is excluded); and a restored timeline's floor accounts for the cursor (a message acknowledged beyond the cursor must not be mistaken for the bottom of the loaded range, or replay would later deliver messages below it that a cache write would then exclude, found by the simulation and regression-tested).

**Bounds and eviction** (deterministic): ≤ 200 messages per conversation (the newest; older are dropped and `has_more` set), ≤ 50 conversations with messages (least recently active evicted first, ties by conversation id), ≤ 4 MiB for the file. The core applies the count limits when it builds the cache and again when it loads one. The runtime enforces the byte limit when it saves: while the sealed file is too large it evicts the least recently active whole timeline and tries again; dropping a timeline never makes the cache inconsistent, and a directory too large to fit leaves no cache at all.

**Writing.** The core emits `Effect::PersistCache` at the end of any `handle` call that changed what the cache holds, never from render. The runtime keeps only the latest pending cache and writes it after `save_debounce`, flushing on shutdown. Building the cache costs time proportional to its size and runs on the UI thread, so live events on a big cache are batched (one write per 500 cached messages' worth of events; structural changes, a loaded page, the directory, members, are saved at once). Skipping a write is always safe: an older cache just means a slightly longer replay. Measured with a full cache (50 × 200 messages): about 0.09 ms per live event in a release build (0.18 ms unoptimized), against 2.2 ms (4.5 ms) per event if every event rebuilt it.

**Starting with a session and a cache.**

1. `Started` shows the cached directory and timelines **immediately**: the phase is `Ready`, the connection is `Connecting`, and `TimelineReset` / `DirectoryChanged` / `SelectionChanged` fire so the UI draws content before any network result.
2. The core emits `Subscribe { after: cached cursor }` (not the bootstrap head) and `LoadBootstrap` to revalidate. Replay fills the gap. Nothing is refetched for a cached conversation; a selected conversation that was not cached fetches its newest page as usual.
3. `BootstrapLoaded` revalidates: the directory is refreshed (new conversations and members appear); conversations the person can no longer read lose their content (and the selection moves if it was one of them). If the bootstrap shows the cache cannot be trusted (another person, a workspace missing, or a cached cursor *ahead* of the server's `head`), it is handled as a first run.
4. If the network is down, cached content stays on screen (no error screen); the realtime connection keeps retrying by itself, and every time it opens the bootstrap is requested again (never duplicated). `Unauthenticated` signs the person out as usual.
5. Journal recovery happens at once (the person is known from the cache): recovered operations are checked with the server, ordered first, and the per-conversation send queue and its ordering guarantee are unchanged.
6. A cache belonging to someone else is never shown: it is ignored without a session, and another person signing in discards it. Sign-out deletes it with everything else. A corrupt cache is set aside and reported in `Started.problems`; a cache from a newer format is left untouched and not used.

**Edits and deletes (not in the protocol yet).** A cached message the server later edits or deletes is an *event* with a position greater than the cursor, so replay applies it through the same path that applies new messages (matched by message id) and the next cache write carries the result; nothing in the cache design needs to change. An edit or delete that happened while the client was away is covered because replay runs from the cached cursor. If the server has pruned those events (`snapshot_required`, cursor expired), the whole cache is discarded, so a stale cached message can never outlive the events that would have corrected it. When those events are added they must be applied by message id to timeline rows *and* mark the cache dirty; that is the only requirement.

## Conversations and read state (slice 2A)

The contract is `llm-docs/DECISIONS.md` D11 and `PROTOCOL_VERSION` 2. This section is what the client does with it.

### What the UI calls

**Read model** (all on `ClientCore`):

| Call | Meaning |
| --- | --- |
| `sidebar()` → `Sidebar { favorites, channels, direct_and_group }` | Every conversation the person belongs to, once, in display order: favorites (any kind, by title), other channels (by name), other direct/group conversations (most recent message first). `favorites()`, `channels()`, `direct_and_group()` return one section each. |
| `conversations()`, `conversation(id)` | The directory. `Conversation` has `kind`, `name` (channels only), `topic`, `description`, `archived`, `is_default`, `created_by`, `participants`, `member_count`. |
| `conversation_title(id)` | A channel's name (no `#`), or the *other* participants' display names, alphabetical, joined with commas (a placeholder for people the client has not heard of). |
| `user_state(id)` → `Option<&ConversationUserState>` | `favorite`, `last_read`, `unread_count`, `mention_count`. `total_unread()` sums the counts for a badge. |
| `unread_marker(id)` → `Option<ItemKey>` | The timeline row the "New messages" divider goes above. |
| `browse_results()` | The public channel directory from the last `BrowseChannels`, joined or not. |
| `action(kind)` → `&ActionState` | `Idle`, `InFlight`, or `Failed(message)` for each `ActionKind`. |
| `can_join(&BrowsedChannel)`, `can_leave(id)`, `can_add_members(id)`, `can_remove_members(id)`, `can_rename(id)`, `can_edit_details(id)`, `can_archive(id)`, `can_unarchive(id)`; `facts(id)`, `browsed_facts(&BrowsedChannel)` | What to **offer**, from `ConversationFacts` with the caller's workspace role and whether they created the channel. The server decides; these only avoid offering the impossible. |
| `members()` | Everyone known in the workspace, including the signed-in person, ordered by display name. For pickers. |
| `window_active()` | The window state the core was last told. |

**Inputs from the UI**: `CreateChannel { name, visibility, topic, description }` (raw text; an empty topic or description means none; the name is trimmed, lowercased, and spaces become hyphens), `OpenConversation { participants }`, `BrowseChannels`, `JoinChannel(id)`, `LeaveChannel(id)`, `AddMembers { id, users }`, `RemoveMember { id, user }`, `UpdateConversation { id, name, topic, description }` (`None` = unchanged, `Some("")` clears), `ArchiveConversation(id)`, `UnarchiveConversation(id)`, `SetFavorite { id, favorite }`, `DismissAction(kind)`, and two pieces of view state the core cannot know: **`WindowActive(bool)`** (assumed true) and **`TimelineAtBottom { conversation, at_bottom }`** (assumed false until reported, and forgotten when the person leaves the conversation). Without those two reports nothing is ever marked read.

**Changes**: `ConversationStateChanged { conversation }` (title, member count, archived, favorite, unread counts, or marker), `BrowseChanged`, `ActionChanged(kind)`, and `ConversationOpened(id)`. The latter means a created channel or an opened direct/group conversation is ready and **already selected**; navigate to it. Joining does not navigate (the UI may send `SelectConversation` afterwards). `DirectoryChanged`, `SelectionChanged`, `TimelineReset`, and `NoticeChanged` fire as before.

**Runtime reports** (not for the UI): `ConversationCommandResult { operation, result }` for create and open, `ActionResult { kind, result }` for everything else, `MarkReadResult { conversation, up_to, result }`.

### Actions

Each `ActionKind` has one `ActionState`. Starting an action that is already `InFlight` is ignored (a double click cannot send twice). Input that can be checked locally fails immediately with a readable message and sends nothing: a channel name that is empty, too long, or uses other characters; a topic over 250 characters or with line breaks; a description over 1,000; an open with no other person or more than nine. Server refusals are turned into words (`name_taken` → "That name is already taken. Choose another."; `forbidden` → "You're not allowed to do that."; an unreachable server → "Couldn't reach the server. Check your connection and try again."). `Failed` stays until the action is started again or `DismissAction`. An expired session ends the session instead of showing a dialog error.

**Create and open carry an operation id and follow the same rules as sends.** If the answer is lost, the core asks `QueryOperationStatus`; `conversation_committed` finishes the action, `not_found` resends the **same** request with the **same** operation id (at most three times), and it never creates anew with a fresh id. After giving up the action is `Failed("... Try again to check.")` but the operation is kept: starting the same create again continues it (asking first); starting a different one begins a new operation. A late "it committed" answer is accepted whatever the request is doing. The other commands set a state, so repeating them is harmless and they carry no id. Successful create/open adds the conversation, selects it, fetches its newest page, and emits `ConversationOpened`.

### Unread counting

The server computes each conversation's baseline at a known log position; the client then keeps it current by applying events in order:

* a message by someone else in a conversation the person belongs to adds one, unless it is at or before the read position;
* a message by the person moves the read position to it and leaves nothing unread (the server does the same in the send transaction, without an event);
* `UserStateChanged` replaces the whole state, since the server computed it at its own position;
* events for a conversation the client does not know are ignored (they still advance the cursor), and no timeline is created for them.

Every state also remembers the log position it is **current to** (`as_of`, saved in the cache). Events at or before it are never counted again. That is what makes a message count exactly once across replay after a cached restart, a reconnect, and a bootstrap that teaches the client a conversation the cache did not know. The answer to a mark-read request recounts from the timeline, but only messages up to the applied position, because a history page can hold messages whose events have not arrived yet.

### Marking read

Evaluated after every input, never from render. The core emits `MarkRead { conversation, up_to }` when the conversation is **selected**, the **window is active**, the timeline reports it is **at the bottom**, its history is **loaded**, it has **unread messages**, and the newest loaded message is beyond the read position. `up_to` is the newest loaded message position, so requests only ever move forward and are never sent because of the person's own message. At most one request per conversation is in flight (that is the debounce): messages arriving meanwhile are covered by one follow-up when the answer returns. A failed request is not retried for the same position until something changes (a new message, the window or bottom state, or reselecting).

### The "New messages" divider

When a conversation is selected the core captures its read position (if it has unread messages) and `unread_marker` returns the first confirmed row after it, as soon as that row is loaded. It stays while the conversation is marked read underneath it and is dropped only when the person selects another conversation. If nothing was unread on opening but messages arrive while the person is not looking (window inactive or scrolled up), the divider starts at the first of them.

### Membership events

* `ConversationCreated`: adds an unknown conversation (everything before it counts as read) or replaces a known one. `ConversationUpdated`: replaces a known conversation, ignores an unknown one.
* `MemberAdded` for me: adds the conversation, **fetches its newest page**, does not select it, starts its read position at the event. `MemberAdded` for someone else: replaces the snapshot (it carries the current member count).
* `MemberRemoved` for me: the conversation, its timeline, draft, read state, divider, unsent messages (their journal entries are released), and cache entry are dropped; if it was open, the default conversation opens instead and a notice says "You were removed from #name". If the person asked to leave (`LeaveChannel`), whichever of the answer and the event arrives first removes it, silently. `MemberRemoved` for someone else decrements the member count (a later `ConversationUpdated` snapshot is authoritative).

### Cache and persistence

The cache carries the directory (with the new conversation fields), each conversation's read state **and the position it is current to**, under the same atomic cursor-and-messages write, so a restart shows favorites and unread counts at once and replay counts only what is new. On revalidation the cached state is kept (replay brings it up to date); conversations the cache did not know take the bootstrap's baseline, current to its head; a cache from before read state existed supplies none and the bootstrap does. Conversations the person can no longer read lose their state with their content. Drafts remain per conversation and are removed with a removed conversation; sign-out deletes everything. Writes caused only by counting a live message are batched like other live events.

## Runtime

```rust
let (runtime, inputs) = Runtime::start(RuntimeConfig::from_env())?;  // inputs: async_channel::Receiver<Input>
runtime.execute(output.effects);                                      // never blocks
runtime.shutdown();                                                   // call outside any async context
```

`RuntimeConfig` fields: `server_url` (default from `LAZY_SERVER_URL`, fallback `http://127.0.0.1:8080`), `profile_dir` (`None` → `LAZY_PROFILE_DIR` → the platform data directory), `secure_storage`, `request_timeout`, `read_attempts`, `reconnect` (`BackoffPolicy`), `idle_timeout`, `save_debounce`.

### Ownership

`Runtime` owns a two-thread Tokio runtime and everything spawned on it. HTTP tasks live in a `JoinSet` and stop when the cancellation token fires; each realtime subscription has a child token and a command channel and ends on sign-out, expiry, or shutdown; the journal actor applies journal commands in order (including discarding the cache) and the saver coalesces drafts and the cache. `shutdown()` cancels, lets the journal actor finish queued commands, flushes pending saves, awaits all tasks (aborting after 5 seconds), and stops the runtime. Task panics are logged when tasks are reaped. A mutation interrupted by shutdown stays in the journal.

The input channel is bounded (4096). A host that stops draining it slows the realtime reader rather than growing memory.

### Failure classification

Reads that are safe to repeat (bootstrap, history, operation status) are retried up to `read_attempts` with backoff, then reported as `Unknown`. Mutations (register, login, send) are attempted exactly once. A 4xx with an `ApiError` body whose code is a definitive refusal is `Rejected`; a timeout, dropped connection, 5xx, or unparseable body is `Unknown`. A 401 on the WebSocket upgrade becomes `Input::SessionExpired`.

### Conversation requests

Created and opened conversations use `POST` once, never repeated by the runtime (the core reconciles). Browsing is a safe read and is retried. The other management calls are single attempts: join, archive, unarchive (`POST`, no body, answered with the conversation), leave (`POST`) and remove member (`DELETE`) where any 2xx status counts and the body is ignored (the contract does not say what they return), add members (`POST` with `user_ids`), update (`PATCH`, absent fields unchanged), favorite (`PUT`), and mark read (`POST` with `up_to`). All use `lazy_protocol::paths` and the same failure classification: a 4xx `ApiError` with a definitive code is a refusal, anything else is an unknown outcome.

### Secrets

The session token and the 256-bit key protecting local files live in `SecureStorage`. Entries are namespaced by SHA-256 of (canonical profile directory, normalized server URL), under keys `session-token:<namespace>` and `state-key:<namespace>`, so two clients on one machine (`LAZY_PROFILE_DIR=a`, `LAZY_PROFILE_DIR=b`) never share, overwrite, or delete each other's secrets, and a token is never offered to a different server. Sign-out deletes only its own namespace.

* `KeyringStorage` uses the OS credential store (Secret Service on Linux; verified working here).
* `InsecureFileStorage` stores plain files in `<profile>/insecure-secrets/` and is used only with `SecureStorageChoice::InsecureFile` (`LAZY_INSECURE_STORAGE=1`). It is never chosen automatically. If the credential store is unavailable the runtime reports it in `Started.problems`, and refuses to journal (so sends fail with an explanation) rather than writing anything unprotected.

### Local files

`<profile>/journal.enc`, `drafts.enc`, `cache.enc`: `LZST` magic, format version, kind, 24-byte nonce, XChaCha20-Poly1305 ciphertext of JSON, with the header authenticated. Writes go to `<name>.tmp`, are fsynced, renamed over the target, and the directory is fsynced; the acknowledgment (`SubmissionPersisted`) is sent only after that. Limits: journal 256 entries / 16 MiB, drafts 200 / 64 KiB each / 8 MiB, cache 200 messages per conversation / 50 conversations / 4 MiB (evicted by whole timelines when over). An unreadable file is renamed to `<name>.corrupt-<timestamp>` (at most three kept), reported through `Started.problems`, and never silently dropped; a file with a newer format is left untouched and its kind is locked against writes. Files are mode 0600 and the profile directory 0700.

## Wiring it into GPUI

Sketch checked against the pinned revision's `Context::spawn` (`AsyncFnOnce(WeakEntity<T>, &mut AsyncApp)`); adjust names to the actual code.

```rust
struct AppModel {
    core: ClientCore,
    runtime: Runtime,
    // list states, views, ... keyed by conversation
}

impl AppModel {
    fn new(cx: &mut Context<Self>) -> anyhow::Result<Self> {
        let (runtime, inputs) = Runtime::start(RuntimeConfig::from_env())?;
        // Drain runtime reports on GPUI's foreground executor. The task is retained in a field
        // (or detached on purpose) so it lives as long as the model.
        cx.spawn(async move |this, cx| {
            while let Ok(input) = inputs.recv().await {
                if this.update(cx, |model, cx| model.apply(input, cx)).is_err() {
                    break; // the model was dropped
                }
            }
        })
        .detach();
        Ok(Self { core: ClientCore::new(), runtime })
    }

    /// The single entry point for person actions and runtime reports.
    fn apply(&mut self, input: Input, cx: &mut Context<Self>) {
        let output = self.core.handle(input);
        self.runtime.execute(output.effects);
        for change in output.changes {
            self.update_list_state(change); // splice / remeasure / reset, in order
        }
        cx.notify();
    }
}

// From a view: composer submit, sidebar click, ...
model.update(cx, |m, cx| m.apply(Input::SendMessage { conversation, text }, cx));
```

On window close or app quit, call `runtime.shutdown()` from a non-async context so the pending drafts and cache are flushed and connections close cleanly.

## Verification status

| Claim | Status |
| --- | --- |
| Core behavior rules above (send machine and queue, recovery, sync, history, sign-out) | **Measured**: 50 scenario tests plus a property test of random interleavings (events, replays, stale history pages, lost acknowledgments and requests, rejections, snapshot refreshes, reconnects, **app kills restarting from any cache write that could have survived**) that asserts commit order equals submission order, no lost or duplicated message after any restart, and the cache invariant on every cache write, run at 300 cases by default, 60,000 after each core change, and 300,000 once (353,551 restarts from caches with messages, 6.28 million cache writes checked); it found and fixed the history-floor bug and the restored-floor bug above, and fails if the cache is built wrongly or resumed past its cursor (both mutation-checked) |
| Cache scenarios: instant restart, resume from the cached cursor without a newest-page fetch, revalidation, membership loss, selection move, snapshot discard (expired and ahead), cursor ahead of the log, other person's cache, offline start, cache with journal recovery and the send queue, bounds and eviction, cost | **Measured** (26 core tests) |
| Conversations and read state: sections and ordering, titles, what to offer by role and creator, unread counting (others, own, replaced state, baselines, duplicates, unknown conversations), the mark-read policy (every condition, one in flight, monotonic, failures), the divider, membership events and removal while open, browse, every management action with validation, readable errors, double submit, stale results, and create/open unknown-outcome reconciliation | **Measured** (80 scenario tests across `readstate`, `conversations`, `actions`, and the cache tests) |
| Read-state property: unread never exceeds what other people wrote, the sidebar lists each conversation once, every saved unread count is bounded by what existed at its position, and after everything is delivered the read position equals the server's and the unread count equals both the server's count and the loaded rows from other people after it, through restarts from any surviving cache write, reconnect replays, snapshot refreshes, reads on another device, lost answers, and mark-read in every window and scroll state | **Measured** (random runs: 60,000 cases with 19,315 mark-reads answered and 59,036 read-state events; mutation-checked: a double count is caught; the applied-position bound is pinned by a direct test) |
| Every new HTTP call: route, method, body, success shape (including bodiless leave/remove), refusal vs unknown classification, mutations never repeated, browse retried | **Measured** (runtime transport tests against the Axum stub) |
| Behavior against the real server for conversations | **Unverified** here; covered by the coordinator's end-to-end tests once the server lands |
| Cache file: round trip, atomic replacement under a crash at each write step (old or new, never a mixture), size-limit eviction by whole timelines, load-time bounds, corruption, newer format, discard, wipe, encryption and permissions through the real `Runtime` | **Measured** (store and transport tests) |
| File store: round trip, crash at each write step, truncation, tampering, wrong key, wrong kind, newer format, size bounds, permissions, wipe | **Measured** (unit tests) |
| Secret namespacing per profile and server; sign-out deletes only its own secrets | **Measured** (`MemoryStorage` tests, plus a two-runtime transport test) |
| Transport against an in-process Axum stub: auth header, error classification, retry of reads only, WebSocket subscribe/forward, reconnect from last position, no reconnect with a refused cursor, backoff bounded, expired-session handling, clean shutdown, durability before acknowledgment, restart recovery | **Measured** (24 tests, repeated 25 times without a failure) |
| End to end against the real server and Postgres: a restart shows cached history with the network down, then resumes from the cached cursor without refetching the newest page; a pruned event log makes the restart fall back cleanly | **Measured** (`crates/lazy-e2e`, 8 messaging tests plus the GUI test, the messaging suite repeated 12 times without a failure) |
| Real Secret Service round trip on this machine | **Observed** once (opt-in ignored test) |
| Behavior against the real `lazy-server` | **Unverified** here; covered by the coordinator's cross-crate end-to-end tests |
| macOS Keychain, Windows Credential Manager, Wayland/X11 specifics | **Unverified** |
