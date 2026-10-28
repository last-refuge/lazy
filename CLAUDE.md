# Lazy: repository instructions

A Rust and GPUI Slack-style chat app: Axum and Postgres server, shared client core, GPUI desktop client. Plan and decisions are in `llm-docs/` (`PLAN.md`, `IMPLEMENTATION_PLAN.md`, `DECISIONS.md`, `SERVER.md`, `CLIENT.md`, `VERIFICATION.md`); run instructions are in `README.md`.

## Commands

```sh
scripts/test.sh                              # ALL test runs: whole workspace on the separate test Postgres (:54330)
cargo fmt --all --check
cargo clippy --workspace --all-targets
scripts/check-core-deps.sh                   # shared crates must not depend on UI, async runtime, HTTP, or SQL crates
scripts/run-server.sh                        # server on 127.0.0.1:8080 using the dev Postgres (:54329)
scripts/run-client.sh alice                  # desktop client with its own disposable profile
```

## Rules

- **Plan bookkeeping.** When a task or subtask in `llm-docs/IMPLEMENTATION_PLAN.md` is finished and verified, change its `[ ]` to `[X]`. Mark a parent only when every child is done. Record outcomes as measured, observed, failed, or unverified in `llm-docs/VERIFICATION.md`; a clean compile or unit test does not establish IME, screen reader, scaling, or long-session behavior.
- **Crate boundaries.** `lazy-domain`, `lazy-protocol`, and `lazy-client-core` stay free of GPUI, Tokio, HTTP, SQL, and keyring dependencies (phone clients reuse them). `lazy-ui` reaches the runtime only through the `EffectSink` trait. Nothing in `render` performs I/O.
- **Wire contract.** `lazy-protocol` is the only definition of what crosses the network. Change it deliberately and bump `PROTOCOL_VERSION` for incompatible changes.
- **Mutations.** A client generates an operation id per user action and journals it before sending. An unknown outcome is reconciled by asking the server; never create a fresh operation for it. Acknowledge only after commit.
- **Ordering.** Event positions are assigned under the workspace row lock (see `DECISIONS.md` D3). Never order by timestamp or a raw sequence.
- **SQL.** Use runtime-checked SQLx queries, not the `query!` macros, so builds need no database. Dynamic SQL needs `sqlx::AssertSqlSafe` and a reason.
- **GPUI.** `gpui` and `gpui_platform` stay on one pinned Zed revision (root `Cargo.toml`). Read the pinned source in `~/.cargo/git/checkouts/zed-*` before relying on an API. Scroll callbacks run while list state is borrowed: defer anything that splices it.
- **Two databases.** The dev Postgres (`:54329`) belongs to servers a person is running by hand. Never run tests against it, and never `stop` or `reset` it: doing so breaks their running server (a test run can also exhaust its connections). Tests and automated checks use `scripts/test.sh` or `LAZY_PG_INSTANCE=test scripts/dev-db.sh ...` (`:54330`).
- **Upgrades.** Anything saved on a person's machine must survive a protocol change. Persisted data that embeds protocol types is versioned (the content cache records `PROTOCOL_VERSION`); a regenerable file from an older version is discarded silently, and only real damage is reported. Test with a file in the *previous* shape, not just fresh profiles.
- **Tests.** Wait on events or conditions with bounded deadlines, not sleeps. End-to-end tests live in `crates/lazy-e2e`; start a client with its own profile directory and a `MemoryStorage`.
- **Looking at the UI.** Render it offscreen with `cargo run -p lazy-ui --example screens -- .dev/screens` (GPUI's headless wgpu renderer: no window, no display) and read the PNGs. `crates/lazy-e2e/tests/gui.rs` drives two real clients headlessly and can save their windows with `LAZY_SCREENSHOT_DIR`. Label interactive elements with `.debug_selector(..)` so tests can find and click them; it costs nothing outside tests.
- **Native automation.** Launch only your own test windows with disposable profiles and stop them by PID. Never capture the screen by region, and do not send input to windows you did not verify you own.
- **CI is manual.** The workflow runs only when someone starts it (`gh workflow run ci`, or Actions, ci, Run workflow); the owner does not want the full suite on every push. Do not add `push` or `pull_request` triggers, and do not start a run unless asked. Run the checks locally with `scripts/test.sh`, `cargo fmt --all --check`, and `cargo clippy --workspace --all-targets`.
- Do not commit unless asked.
