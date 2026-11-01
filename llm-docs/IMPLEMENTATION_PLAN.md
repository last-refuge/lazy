# Lazy implementation plan

This document breaks [PLAN.md](PLAN.md) into phases, tasks, and subtasks. Phases follow the delivery milestones in PLAN.md, so each phase gate restates that milestone's exit criteria. PLAN.md remains the source for product behavior, architecture, and standards; this document tracks the work.

## How to use this plan

- `[ ]` marks incomplete work and `[X]` marks completed work. Mark a parent `[X]` only when every child is `[X]`.
- Tasks are numbered `phase.task` (for example `1.4`) so they can be referenced from commits and reviews. Subtasks are unnumbered.
- A phase is complete when its gate task is complete. Gate tasks record each outcome as **measured**, **observed**, **failed**, or **unverified**. A clean compile or unit test does not establish IME, screen-reader, fractional-scaling, suspend/resume, or long-session behavior.
- Before starting each subsystem, re-read the relevant Rust best practices and GPUI skill references and the exact pinned dependency source (PLAN.md, "Skill guidance").
- Every task is done only when formatting, Clippy, workspace tests, and migration checks pass on the supported platform and feature matrix.
- Introduce a crate, trait, or dependency only when it has a concrete responsibility. Keep dependency and toolchain upgrades separate from feature work unless required.
- Resolve the items in [Open decisions](#open-decisions) by the task that needs them, and record each answer in `llm-docs/`.

## Phase order

The delivery order is Phases 0 through 7. Two prototypes start earlier than their phases: the huddle audio proof in Phase 0 and a webhook-posting prototype in Phase 2. Phases 4 and 5 are mostly server and media work and can overlap with Phases 2 and 3 if there is capacity, but each phase gate is still checked in order.

---

## [ ] Phase 0: Foundation

Source: PLAN.md "Requirements and platform constraints", "Architecture", "Huddles", "Native phone preparation".

Goal: a compiling, CI-checked workspace that launches a native desktop window and has settled the largest unknowns (GPUI pin, Android path, media stack).

- [ ] 0.1 Repository and toolchain
  - [X] Initialize the git repository and add `.gitignore`
  - [X] Pin a supported Rust toolchain and edition in `rust-toolchain.toml`
  - [ ] Create the Cargo workspace with shared `workspace.dependencies` and lint policy
  - [X] Write repository instructions (build, test, conventions, dependency policy) for contributors and agents
  - [ ] Add `cargo-deny` for licenses, advisories, and duplicate versions
- [ ] 0.2 GPUI pin
  - [ ] Decide the desktop OS targets and minimum versions
  - [X] Select and record a full Zed revision for GPUI
  - [X] Review standalone build requirements and dependency patches at that revision
  - [ ] Open a minimal GPUI window on each desktop target
  - [ ] Document the procedure for upgrading the pinned revision
- [ ] 0.3 Core crates, in dependency order
  - [X] `lazy-domain`: typed identities (newtypes) and structured error types
  - [X] `lazy-protocol`: versioned envelope, command/response/event skeleton, version negotiation
  - [X] `lazy-server`: Tokio, Axum, and SQLx skeleton with configuration, health endpoint, and a defined owner for every spawned task and its shutdown
  - [X] `lazy-client-core`: state-machine skeleton with command/effect/event boundaries
  - [X] `lazy-client-runtime`: transport and platform-interface skeleton
  - [ ] `lazy-ui` and `lazy-desktop`: window lifecycle, asset loading, theme-token plumbing
  - [ ] Add a CI check that `lazy-domain`, `lazy-protocol`, and `lazy-client-core` do not depend on GPUI or desktop-only crates
- [ ] 0.4 Postgres and migrations
  - [ ] Add a reproducible local Postgres setup with a pinned major version
  - [ ] Choose migration tooling and a naming and forward-only policy
  - [X] Build a test harness that gives each test an isolated database
  - [X] Set up SQLx query metadata so CI builds without a live database where possible
  - [ ] Apply migrations to an empty database in CI
- [ ] 0.5 Continuous integration
  - [ ] Run `cargo fmt --all --check`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace`
  - [ ] Run dependency, license, and migration checks
  - [ ] Build the desktop targets in an OS matrix
  - [ ] Compile the shared crates for iOS and Android targets
- [ ] 0.6 Product semantics and authorization specs
  - [ ] Define workspace roles and channel-level permissions
  - [X] Write the authorization matrix for mutations, history reads, search, attachments, realtime subscriptions, and huddle joins
  - [X] Specify thread notifications, deleted root messages, archived channels, private-channel invitations, and membership removal
  - [X] Specify concurrent message edits and unread behavior
  - [ ] Specify group-conversation history rules when participants are added
  - [ ] Turn each specification into a named test case to implement in Phases 1 and 2
- [ ] 0.7 Design foundations
  - [ ] Define semantic design tokens for light and dark themes (color, type, spacing, radius, motion)
  - [ ] Specify the core components: button, input, list row, message row, composer, dialog, avatar, badge, menu
  - [ ] Specify the shell layout: workspace rail, sidebar, timeline, resizable thread pane, huddle dock
  - [ ] Produce representative screens at wide, medium, and narrow widths, with simulated content visibly labeled
- [ ] 0.8 Mobile feasibility
  - [ ] Confirm the shared crates compile for iOS and Android, and keep that check in CI
  - [ ] Run a minimal GPUI screen on the iOS backend and note rendering, input, accessibility, lifecycle, and power findings
  - [ ] Check Android backend status at the pinned revision
  - [ ] If no suitable Android backend exists, estimate the cost of maintaining one
  - [ ] Document the Android path and the decision it requires
- [ ] 0.9 Huddle media spike
  - [ ] Inventory the native libraries, codecs, and OS APIs needed by media adapters, with packaging and licensing notes
  - [ ] Evaluate str0m for the SFU and webrtc-rs for the client transport at candidate versions
  - [ ] Evaluate TURN options, including a Rust implementation
  - [ ] Build a prototype with signaling through the server and reliable two-person audio between two clients
  - [ ] Record findings, risks, and the media stack decision
- [ ] 0.10 Verification conventions
  - [X] Define the verification record format (measured, observed, failed, unverified)
  - [ ] Name the reference hardware, network conditions, and region used for performance targets
  - [X] Write the native-automation rules: only test windows and processes launched for the run, disposable profiles, cleanup of owned processes
- [ ] 0.11 Phase gate
  - [ ] Native launch on every desktop target
  - [ ] Shared-core mobile builds pass in CI
  - [ ] Android path documented
  - [ ] Two-client audio proof recorded
  - [ ] CI is green and verification outcomes are recorded

---

## [X] Phase 1: Messaging slice

Source: PLAN.md "Durable messaging and synchronization", "Experience and visual design", "Engineering standards".

Goal: sign in, open one workspace and one channel, send durably, receive in realtime. Nothing is lost or duplicated across restarts or lost acknowledgments.

- [X] 1.1 Identity and sign-in
  - [X] Decide the sign-in method (see Open decisions)
  - [X] Add user and session schema with expiry and revocation
  - [X] Add authentication middleware that establishes the caller identity for HTTP and WebSocket
  - [X] Store credentials in platform secure storage on the client
  - [X] Build the sign-in and sign-out flow, including sign-out cleanup of drafts, journal, and cache
- [X] 1.2 Workspace and channel baseline
  - [X] Schema for workspaces, memberships with roles, conversations, and channel membership
  - [X] Pure authorization functions in `lazy-domain`, with tests from the Phase 0 matrix
  - [X] Minimal bootstrap: create a workspace and a channel, join as a member
- [X] 1.3 Message model
  - [X] Versioned message document in `lazy-domain` with validation and size bounds
  - [X] Plain-text projection, tested with Unicode and emoji
  - [X] Messages table with explicit relational identity, ordering, and integrity constraints; the document stored versioned and the projection stored for search
- [X] 1.4 Submission and acknowledgment
  - [X] Client-assigned operation IDs
  - [X] Operation receipts table with a uniqueness constraint; define scope and retention
  - [X] `send_message` command: authenticate, authorize, validate, then one transaction writing message, receipt, and outbox event
  - [X] Acknowledge only after commit, identifying the committed revision
  - [X] Operation-status query for clients that lost an acknowledgment
- [X] 1.5 Ordering and replay
  - [X] Design and document commit-safe ordering and the relationship between snapshots, subscriptions, and replay
  - [X] Write a concurrency test showing a concurrently committed event cannot be skipped during catch-up
  - [X] Durable events table with cursors and retention; expired cursors trigger a snapshot refresh
  - [X] Dispatcher woken by `LISTEN/NOTIFY` that queries durable state on startup and reconnect
  - [X] Enforce current access permissions on replay and live delivery
- [X] 1.6 Realtime API
  - [X] WebSocket endpoint with subscribe, snapshot, replay, and heartbeat
  - [X] Bounded per-connection queues with defined full-queue behavior
  - [X] Cursor-based history paging
  - [X] Defined ownership of connection tasks and shutdown draining
- [X] 1.7 Client core
  - [X] Timeline state model with message states: pending, confirmed, failed, unknown outcome
  - [X] Optimistic send reconciled with authoritative events by operation ID
  - [X] Unknown outcomes query operation status and never create a fresh operation automatically
  - [X] Synchronization engine: snapshot, subscribe, replay, persisted cursor, deduplication by event ID
  - [X] Per-conversation drafts
  - [X] Guard asynchronous results with target identity, generation, and operation tokens
- [X] 1.8 Client runtime
  - [X] HTTPS and WebSocket transport with bounded reconnect backoff
  - [X] Encrypted draft and submission-journal files: atomic replacement, durable-write acknowledgment, interrupted-write recovery, schema versioning, size bounds
  - [X] Keys from platform secure storage, with desktop implementations behind an interface
  - [X] Regenerable content cache with limits
  - [X] Defined bridge between the Tokio and GPUI executors, including task ownership and shutdown
- [X] 1.9 UI slice
  - [X] Desktop shell with workspace rail, sidebar, header, and timeline
  - [X] Virtualized variable-height timeline with stable identities, scroll anchoring, and older-history loading
  - [X] Composer that grows naturally, with UTF-16-correct platform input handling (Enter during composition does not submit; verified on GPUI's test platform, real input methods are verified in task 6.2)
  - [X] Message rows for pending, failed, and unknown-outcome states, with retry and check-status actions
  - [X] Reconnecting state that keeps content on screen
  - [X] Keyboard navigation and focus order for the shell
- [X] 1.10 Tests
  - [X] Domain and client-state tests: permissions, optimistic updates, stale results
  - [X] Postgres integration tests: transactions, idempotency, concurrent commits, ordering, replay, authorization
  - [X] End-to-end tests with real clients, server, and persistence: submit, settle, restart, reconnect, history paging
  - [X] Fault injection: lost acknowledgment, server crash after commit, interrupted client write, storage failure
- [X] 1.11 Phase gate
  - [X] Two real clients exchange messages
  - [X] Restart and lost acknowledgments produce no loss or duplicates
  - [X] Every acknowledged message survives a server-process crash
  - [X] Verification outcomes recorded

---

## [X] Phase 2: Familiar chat

Source: PLAN.md "Product semantics", "Experience and visual design".

Goal: the chat behaviors people expect from Slack, with permissions enforced end to end.

Delivery slices. Each slice is built across server, client, and UI, verified, and left runnable before the next begins:

- **2A Conversations and read state**: tasks 2.1, 2.6, 2.7, and the sidebar, dialogs, unread, and empty-state parts of 2.8. Decisions in `DECISIONS.md` D11.
- **2B Message lifecycle and rich text**: task 2.2 (edit, delete, formatting, permalinks) and the edit-in-place and hover-action parts of 2.8.
- **2C Emoji, reactions, mentions, groups**: tasks 2.4 and 2.5, in two rounds. 2C-1 is emoji and reactions (decisions in `DECISIONS.md` D13); 2C-2 is mentions and user groups.
- **2D Threads**: task 2.3 and the thread pane.
- **2E Integration prototype and phase gate**: tasks 2.9, 2.10, 2.11.

- [X] 2.1 Conversation types
  - [X] Public and private channels with topics and descriptions
  - [X] Create, join, leave, and membership management
  - [X] Private-channel invitations
  - [X] Archive and unarchive
  - [X] Favorites
  - [X] Direct messages
  - [X] Group messages; adding participants creates a new conversation following the specified history rules
  - [X] Membership removal applied to history, subscriptions, and replay
  - [X] Membership removal applied to threads (slice 2D)
- [X] 2.2 Message lifecycle
  - [X] Editing with a revision table; a newer revision stays pending while an older one persists
  - [X] Concurrent-edit handling as specified in Phase 0
  - [X] Deletion by the author or a moderator, as a tombstone with purged revisions
  - [X] Deleted root messages that have replies (slice 2D)
  - [X] Permalinks
  - [X] Formatting, links, and code blocks in the message document, projection, and composer
- [X] 2.3 Threads
  - [X] Root and reply relationships; replies inherit conversation access
  - [X] Optional reference into the channel that points at the existing reply without duplicating content or identity
  - [X] Thread subscriptions and per-thread unread state
  - [X] Thread following rules as specified (who follows, auto-follow, unfollow)
  - Thread notification delivery is part of task 3.4 (thread inbox, desktop notifications)
- [X] 2.4 Reactions and Unicode emoji
  - [X] Reactions table with uniqueness per user, message, and emoji, and events for add and remove
  - [X] Pinned Unicode emoji dataset with skin tones and search keywords
  - [X] Searchable picker
  - [X] Autocomplete in the composer
  - Custom workspace emoji is delivered in Phase 3 (task 3.2) because it depends on attachment storage
- [X] 2.5 Mentions and user groups
  - [X] User mentions with permission-aware autocomplete
  - [X] User groups with membership and group mentions
  - [X] Define channel-wide mention semantics
- [X] 2.6 Read state
  - [X] Read cursors per conversation
  - [X] Read cursors per thread (slice 2D)
  - [X] Unread markers
  - [X] Mention counts (slice 2C)
  - [X] Cross-device read-state sync through events
- [X] 2.7 Authorization hardening
  - [X] Revoke membership while a client is disconnected; reconnect must not leak history or live events
  - [X] Verify private-channel access on replay and history
  - [X] Verify private-channel access on thread reads (slice 2D)
  - [X] Verify edits and deletions during disconnection
  - [X] Verify reaction changes during disconnection (slice 2C)
- [X] 2.8 UI
  - [X] Sidebar sections for channels, direct messages, and favorites, with clear unread indicators
  - [X] Resizable thread pane; opening a thread preserves the channel's reading position
  - [X] Hover actions that are keyboard-accessible
  - [X] Edit in place
  - [X] Reaction chips and the emoji picker (slice 2C)
  - [X] Per-conversation draft and scroll restoration
  - [X] Per-conversation selection restoration
  - [X] Selection in a document model so copy includes offscreen messages
  - [X] Empty states and error messages that explain the next action
- [X] 2.9 Early integration prototype
  - [X] Incoming webhook posts into a channel through a bot identity on the normal message path
  - [X] Record what the prototype shows about bot identity, authorization, and rendering
- [X] 2.10 Tests
  - [X] Property tests for the unread state model
  - [X] Property tests for the thread state model (slice 2D)
  - [X] Unicode, emoji, editing, selection, and IME composition tests
  - [X] End-to-end tests for threads, reactions, and membership changes
- [X] 2.11 Phase gate
  - [X] Core semantics pass end-to-end tests
  - [X] Private access remains enforced during reconnect
  - [X] Verification outcomes recorded

---

## [ ] Phase 3: Daily use

Source: PLAN.md "Client persistence and search", "Product semantics" (Attention, Discovery), "Experience and visual design".

Goal: dogfood users can do normal work without developer assistance.

- [ ] 3.1 Attachments
  - [X] Decide the storage backend (see Open decisions)
  - [X] Metadata and access-relationship schema
  - [X] Authorized upload and download through Lazy
  - [X] Bounded upload sizes; validate declared types against content
  - [X] Cleanup job for abandoned uploads
  - [X] Image previews generated off executor threads
  - [X] Client UI: drag and drop, paste, progress, cancel, and retry (built and tested with simulated events)
  - [ ] Verify drag and drop, paste (images and file lists), and file dialogs on a real display (manual)
- [X] 3.2 Custom emoji
  - [X] Upload, naming rules, and validation
  - [X] Delivery in the picker, autocomplete, and reactions
  - [X] Admin management
- [X] 3.3 Search
  - [X] Full-text index over the plain-text projection
  - [X] Authorization filters applied at query time
  - [X] Snippets and attachment results that respect conversation access
  - [X] Reindex on edit and delete
  - [X] Review query plans on representative data
  - [X] Search UI with result navigation
- [ ] 3.4 Notifications and attention
  - [X] Decide where notification decisions are computed
  - [X] Notification preferences: per workspace, per conversation, mute
  - [X] Status and do not disturb
  - [X] Thread inbox
  - [X] Desktop notifications through a platform interface (built and tested with a fake notifier)
  - [ ] Verify desktop notifications on a real display: a notification daemon, replace and withdraw, click to open, and the launcher count (manual)
  - [X] Badge counts
- [X] 3.5 Discovery
  - [X] Channel directory
  - [X] People directory and profiles
  - [X] Quick switcher
  - [X] Saved items
- [ ] 3.6 Workspaces, administration, and settings
  - [X] Multi-workspace switching in the rail
  - [X] Invitations, deactivation, and role management
  - [X] Profile editing and avatars
  - [X] User settings: theme, notifications, reduced motion
  - [X] Workspace settings
  - [ ] Verify on a real display: the avatar file dialog, input methods in the new text fields, and the rail with several real workspaces (manual)
- [ ] 3.7 Commands and desktop integration
  - [X] One command system so buttons, shortcuts, and menus share availability rules
  - [X] Keyboard shortcuts and menus
  - [X] Window state restoration
  - [X] Dialogs with focus trapping, dismissal, and focus restoration that does not interfere with text editing or IME
  - [ ] Verify on a real display: the macOS menu bar, shortcuts on non-US keyboard layouts, window size and position restoration, and an input method inside dialogs (manual)
- [ ] 3.8 Dogfooding
  - [X] Structured logging with no credentials or message content
  - [X] Dogfood guide (`llm-docs/DOGFOOD.md`) and issue template (`.github/ISSUE_TEMPLATE/dogfood.md`)
  - [X] Server container, Docker Compose, and Coolify deployment (the `server` workflow passed image build, fresh-database smoke test, and restart persistence, run 41155102683; user confirmed on 2026-10-31 that the server is running successfully on Coolify and the Windows client connects to it; pushing to ghcr.io remains unverified)
  - [ ] Client installers for Linux, macOS, and Windows from the manual `release` workflow (written: `.deb`, tarball, `.dmg`, Windows installer and zip, `llm-docs/RELEASE.md`; the rebuilt 0.6.0 draft passed builds on all three platforms and checksum inspection, run 41161781734; user confirmed Windows client use against the Coolify server on 2026-10-31; macOS installation is next, Linux installation outside CI remains unverified)
  - [X] Choose the server in the app, so installed clients need no environment variable (tested in CI on Linux, `ci` run on `1f99b8f`; not tried on an installed app)
  - [ ] First run: address and invitation code, then a short, branded account creation (plan in `llm-docs/ONBOARDING.md`, D29; the invitee's and the owner's paths are written and tested in CI on Linux; the first walkthrough on real machines is still to do)
    - [X] Server: public server info, previewing an invitation without using it, a readable invitation code, Copy message to send (tested in CI against a real Postgres, `ci` run on `1f99b8f`)
    - [X] Client core and runtime: what the server says, the preview, forgiving code input, the first-run screens for a new install (tested in CI: the core and the screens' logic directly, the runtime against a stub server, never a real one)
    - [ ] UI: the five screens, Moss at onboarding size, motion, dark theme (rendered and read in `screens`, `bdb1b45`; the motion itself not seen); the owner's two extra screens (name the workspace, the first invitation) are done; a warning for plain `http://` and accessibility passes are not done
    - [ ] Looked at in `screens` scenes (light, dark, wide, narrow, every error: all 24 `first-*` scenes read, `bdb1b45`) and walked through on Windows, macOS, and Linux with a fresh server (manual: not done)
  - [ ] Apply the Soft Signal brand to the client and the packages (written, not compiled or rendered: `llm-docs/DECISIONS.md` D26)
  - [ ] Onboard dogfood users
  - [ ] Track and triage issues from real use
- [ ] 3.9 Phase gate
  - [ ] Dogfood users complete normal work without developer assistance
  - [ ] Verification outcomes recorded

---

## [ ] Phase 4: Integrations

Source: PLAN.md "Outside integrations".

Goal: generic webhooks and one complete provider integration, with predictable recovery from duplicates and failures.

- [ ] 4.1 Data model and credentials
  - [ ] Schema for installations with explicit scopes, subscriptions, deliveries, and credential references (credentials and jobs, D31; installations and subscriptions come with GitHub)
  - [X] Credential storage with encryption at rest and rotation (D31)
  - [ ] Dedicated bot identities
- [ ] 4.2 Job system
  - [X] Postgres-backed queue with leases, ownership, and bounded concurrency (D31)
  - [X] Shutdown draining and worker restart recovery (D31)
  - [X] Failure classes: retryable, definitive, unknown outcome (D31)
  - [ ] Bounded retries with visible failures (bounded retries done, D31; failures are recorded but shown to admins only once a connector enqueues jobs)
  - [ ] Worker process mode for isolation (`lazy-server worker` written, D31; the worker is tested, the separate process has not been run)
- [ ] 4.3 Incoming webhooks
  - [X] Per-channel webhook endpoints with secrets (built in 2.9, D16; a new secret on request, D30)
  - [ ] Signature verification, timestamp checks, and replay protection
  - [X] Payload size bounds and rate limits (built in 2.9, D16)
  - [X] Duplicate-event handling (`Idempotency-Key`, D16; duplicates are recorded as such in the delivery log, D30)
- [ ] 4.4 Rich messages and actions
  - [ ] Versioned declarative block schema rendered with native components
  - [ ] No arbitrary remote HTML
  - [ ] Interactive actions with authorization rechecked on each use
- [ ] 4.5 First provider integration (GitHub)
  - [ ] Installation flow and scopes
  - [ ] Provider webhook signature verification
  - [ ] Event-to-rich-message mapping
  - [ ] Channel subscriptions
  - [ ] Deduplication by provider delivery ID
  - [ ] Provider-specific idempotency or reconciliation for any provider action
  - [ ] Credential revocation
- [ ] 4.6 Bots and commands
  - [ ] Bot tokens and scopes
  - [ ] Slash commands
  - [ ] Posting API with rate limits
- [ ] 4.7 Link previews
  - [ ] Fetcher with outbound network restrictions
  - [ ] Authorization recheck and size, time, and redirect bounds
  - [ ] Cached previews that respect conversation access
- [ ] 4.8 Administration UI
  - [ ] Install and configure integrations (incoming webhooks: workspace Integrations dialog with add, list, and health, D30; provider installs still to do)
  - [ ] Manage channel subscriptions
  - [ ] View deliveries and failures, and retry (incoming webhooks: delivery log and recent failures, D30; there is nothing to retry for them because Lazy keeps no payload; retry comes with the job system)
  - [ ] Rotate and revoke credentials (incoming webhooks: new address and delete, D30; provider credentials still to do)
- [ ] 4.9 Tests
  - [ ] Duplicate provider events
  - [ ] Permission changes and credential revocation
  - [ ] Unknown delivery outcomes
  - [ ] Worker restart and fault injection
  - [ ] Logs contain no credentials or sensitive payloads
- [ ] 4.10 Phase gate
  - [ ] Duplicate events and failures recover predictably
  - [ ] Credentials can be revoked
  - [ ] Verification outcomes recorded

---

## [ ] Phase 5: Huddles

Source: PLAN.md "Huddles".

Goal: group audio, video, and screen sharing at a declared capacity, with media failures isolated from text messaging.

- [ ] 5.1 Server signaling and authorization
  - [ ] Huddle metadata schema and events
  - [ ] Signaling over the realtime connection
  - [ ] Expiring room credentials
  - [ ] Enforce conversation membership when joining and while connected
- [ ] 5.2 Media service
  - [ ] `lazy-media` process, configuration, and lifecycle
  - [ ] Room and session state machines free of GPUI and platform dependencies
  - [ ] Decide where shared session state lives so mobile can reuse it without server dependencies
  - [ ] Authorization and signaling interface to `lazy-server`
- [ ] 5.3 Reliable two-person audio
  - [ ] Productionize the Phase 0 prototype
  - [ ] Join, leave, and reconnect behavior
- [ ] 5.4 Multi-person audio through the SFU
  - [ ] str0m integration (or the stack chosen in Phase 0)
  - [ ] Forwarding, participant lifecycle, and resource limits
  - [ ] TURN deployment for restricted networks
- [ ] 5.5 Devices and audio quality
  - [ ] Device enumeration, selection, and permissions
  - [ ] Mute, echo cancellation, and noise handling
  - [ ] Reconnect, device removal, Bluetooth, and speakerphone behavior
  - [ ] Media processing outside the GPUI foreground thread, with bounded buffers
- [ ] 5.6 Screen sharing
  - [ ] Capture adapter for each desktop OS
  - [ ] Source picker, permissions, and a visible sharing indicator
  - [ ] Encoding and subscriber handling
- [ ] 5.7 Camera video and adaptive quality
  - [ ] Camera adapters
  - [ ] Codec selection with licensing review
  - [ ] Adaptive quality and per-subscriber layers
- [ ] 5.8 Huddle UI
  - [ ] Persistent dock that remains available while navigating
  - [ ] Participant views for audio, video, and screen share
  - [ ] Join, leave, and device controls with keyboard access and an accessibility pass
  - [ ] Huddle presence in the sidebar and notifications
- [ ] 5.9 Failure handling
  - [ ] Report and recover from media failure without interrupting text messaging
  - [ ] Sleep/wake, network loss, and permission denial handling
- [ ] 5.10 Capacity and tests
  - [ ] Declare launch capacity (such as 12 participants)
  - [ ] Synthetic-client load tests at capacity
  - [ ] Network tests: loss, latency, and restricted networks
  - [ ] Device tests: Bluetooth, speakerphone echo, device removal, permission denial, sleep/wake
  - [ ] Measure huddle join latency on the reference network
- [ ] 5.11 Security description
  - [ ] Document transport encryption accurately, without claiming end-to-end encryption
- [ ] 5.12 Phase gate
  - [ ] Declared participant capacity passes network and device tests
  - [ ] Huddle join p95 below 3 seconds on the reference network
  - [ ] Verification outcomes recorded

---

## [ ] Phase 6: Release

Source: PLAN.md "Experience and visual design", "Verification and release gates".

Goal: a signed, supportable desktop release.

- [ ] 6.1 Visual refinement
  - [ ] Refinement pass on every screen in light and dark themes
  - [ ] Layout checks at wide, medium, and narrow desktop widths
  - [ ] Motion review, including reduced-motion behavior
  - [ ] Empty-state and error copy review
- [ ] 6.2 Accessibility
  - [ ] Inspect the actual accessibility tree on each platform
  - [ ] Test with the native screen reader on each platform
  - [ ] Keyboard-only walkthrough of every workflow
  - [ ] IME verification on each OS, including candidate window placement
  - [ ] Display scaling and contrast checks
- [ ] 6.3 Performance validation
  - [ ] Timeline with 10,000 loaded messages and bounded mounted rows
  - [ ] Input to frame submission p95 below 16 ms
  - [ ] Message acknowledgment p95 below 300 ms in the reference region
  - [ ] Record build, hardware, workload, network, sample size, and method
- [ ] 6.4 Durability and recovery
  - [ ] Full crash and fault-injection suite at durable boundaries
  - [ ] Backup restoration drill
  - [ ] Schema upgrade compatibility, including client and server version skew
  - [ ] Retention and deletion behavior
  - [ ] Rollback procedures
- [ ] 6.5 Packaging and updates
  - [ ] Installers for each desktop OS
  - [ ] Code signing and, where required, notarization
  - [ ] Signed update mechanism with rollback
  - [ ] Runtime dependency and license inventory
  - [ ] Installed-app verification on clean machines
- [ ] 6.6 Operations
  - [ ] Deployment topology and secrets handling
  - [ ] Metrics, tracing, and logs without sensitive data
  - [ ] Runbooks, alerts, and incident process
  - [ ] Operational targets from load and recovery exercises
  - [ ] Support procedures
- [ ] 6.7 Security review
  - [ ] Authorization audit across mutations, history, search, attachments, subscriptions, and huddle joins
  - [ ] Fuzz parsers at trust boundaries (message documents, webhook payloads)
  - [ ] Dependency advisory review
  - [ ] Rate limiting and abuse controls
- [ ] 6.8 Soak and platform QA
  - [ ] Sleep/wake and long-session stability runs
  - [ ] Fractional scaling checks
  - [ ] Native QA pass on every supported OS
- [ ] 6.9 Phase gate
  - [ ] Signed packages
  - [ ] Verified recovery
  - [ ] Native QA complete
  - [ ] Support procedures defined
  - [ ] Verification outcomes recorded

---

## [ ] Phase 7: Phone delivery

Source: PLAN.md "Native phone preparation".

Goal: native iOS and Android clients on the shared core. Estimate the calendar after Phase 0 establishes the media and Android costs.

- [ ] 7.1 Platform decisions
  - [ ] Resolve the Android feasibility gate from Phase 0 against the current GPUI state
  - [ ] If no suitable backend exists, estimate and approve the maintenance work as its own project
  - [ ] Any proposal to replace GPUI goes back to the product owner as a requirement change
- [ ] 7.2 Platform interface implementations
  - [ ] Secure credential storage
  - [ ] Notifications and push registration
  - [ ] Application lifecycle and background suspension
  - [ ] Audio routing, camera, and screen capture
  - [ ] File selection, sharing, clipboard, and deep links
- [ ] 7.3 Synchronization and push
  - [ ] Resume correctly after suspension
  - [ ] Server push delivery through the notification worker, independent of a foreground WebSocket
  - [ ] Decide push payload content with privacy in mind
- [ ] 7.4 Mobile UI
  - [ ] Navigation from conversation list to timeline to thread
  - [ ] Touch-sized controls, safe areas, and keyboard avoidance
  - [ ] Compact call surface
  - [ ] Shared design tokens with adapted layouts
- [ ] 7.5 iOS delivery
  - [ ] Build, sign, and distribute test builds
  - [ ] Physical-device verification
- [ ] 7.6 Android delivery
  - [ ] Build, sign, and distribute test builds
  - [ ] Physical-device verification
- [ ] 7.7 Mobile huddles
  - [ ] Platform media adapters for audio routing, camera, and screen capture
  - [ ] Interruption, route change, and background behavior
- [ ] 7.8 Mobile quality
  - [ ] VoiceOver and TalkBack verification
  - [ ] Power and thermal checks
  - [ ] Lifecycle tests: suspend, resume, and process death
- [ ] 7.9 Phase gate
  - [ ] Physical-device messaging, push, lifecycle, accessibility, and huddle verification
  - [ ] Verification outcomes recorded

---

## Open decisions

Resolve each by the task that needs it and record the answer in `llm-docs/`.

| Decision | Needed by |
| --- | --- |
| Desktop OS targets and minimum versions | 0.2 |
| Android path (use, maintain, or revisit a backend) | 0.8, 7.1 |
| Media stack: str0m, webrtc-rs, and TURN implementation | 0.9, 5.4 |
| Reference hardware, network conditions, and region | 0.10 |
| Sign-in method | 1.1 |
| Commit-safe ordering mechanism | 1.5 |
| Channel-wide mention semantics (decided, D14) | 2.5 |
| Attachment storage backend (decided, D17) | 3.1 |
| Where notification decisions are computed | 3.4 |
| First provider integration (GitHub proposed) | 4.5 |
| Video codecs | 5.7 |
| Hosting and deployment target | 6.6 |
| Update mechanism | 6.5 |
