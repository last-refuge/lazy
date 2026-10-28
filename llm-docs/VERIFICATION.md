# Verification record

Outcomes of checking the Phase 1 MVP, recorded as **measured** (a number from a defined run), **observed** (seen working, not benchmarked), **failed** (found broken, with what happened), or **unverified** (not checked; do not assume it works). Taken on 2026-10-28 on Linux (Hyprland/Wayland, one 5120×2160 output at 1.25× scale), Rust 1.99.0, PostgreSQL 18.6, GPUI at Zed revision `e5bdd01c`.

## Measured

| Check | Result |
| --- | --- |
| `cargo test --workspace` | 348 passed, 0 failed, 1 ignored (the real-keyring test, run separately) across 31 suites |
| `cargo fmt --all --check`, `cargo clippy --workspace --all-targets` | no differences, no warnings |
| Messaging end-to-end suite (`crates/lazy-e2e/tests/messaging.rs`, 8 tests) repeated | 30 of 30 runs passed |
| GUI end-to-end test (`crates/lazy-e2e/tests/gui.rs`) repeated | 10 of 10 runs passed, no leaked databases |
| Server ordering stress (`lazy-server/tests/realtime.rs`) | 400 messages from concurrent senders with 4 concurrent registrations and a continuously running pruner while 32 subscribers connected mid-stream: every committed event delivered exactly once, in order. The author confirmed the test fails when the subscribe-before-replay rule is removed |
| Client simulation (`lazy-client-core/tests/simulation.rs`) | 60,000 random interleavings of events, history pages, acknowledgments, reconnects, and crash points between cache writes: no duplicate rows, sorted by position, every operation settled once, commit order equals submission order, no message lost across a restart |
| Mounted rows in a 10,000-message timeline | fewer than 150 rows mounted, the newest included (`lazy-ui/tests/app_flow.rs`) |
| Position restore after older history loads | the watched message moved by at most 1 px when 50 older messages were prepended (it moved 78 px before the fix) |
| Shared-crate boundary (`scripts/check-core-deps.sh`) | `lazy-domain`, `lazy-protocol`, and `lazy-client-core` depend on no GPUI, Tokio, HTTP, SQL, or keyring crate |
| Authorization matrix (`lazy-domain/tests/authz_matrix.rs`) | all 32 combinations of role, membership, visibility, and archived state match `AUTHORIZATION.md` |
| Text contrast (`lazy-ui` theme tests) | every text role meets WCAG AA (4.5:1) on its surface in both themes |

## Observed

- **The real UI, rendered.** `cargo run -p lazy-ui --example screens` renders the real views offscreen with GPUI's headless wgpu renderer (the same renderer and text system the app uses, on the real GPU, with no window and nothing on any display) and writes PNGs. Looked at: sign-in (light, dark, with a validation error); the chat view at 1100×720, 820×600, and the 640×480 minimum, in light and dark; an empty channel; pending, in-doubt, and rejected messages; the reconnecting banner with content kept on screen; a multi-line composer; emoji and non-Latin scripts.
- **Two GUI clients through a real server.** In `gui.rs`, two clients, each a real `RootView` with the real runtime pump (`lazy_desktop::start_model`) behind it, register using only the keyboard (Tab to "Create an account", Enter, type, Tab, Enter), exchange a single-line and a two-line message, and end with the same two confirmed messages in the same order with the right authors. The final window images show both clients correctly.
- **Real clients against a real server (headless stacks).** A restarted client resumes its session and shows cached history immediately, with the network down, then resumes from its saved cursor without refetching the newest page. After the server prunes its log, the restarted client gets `cursor_expired` and falls back to a fresh start.
- **Lost acknowledgment.** A proxy lets the server commit a message and swallows the response. The client marks it in doubt, asks the server, and ends with one confirmed message on both clients and one row in the database.
- **Client killed mid-send.** The message commits, the client stops before hearing back, and after restart the journal brings it back in doubt and the server settles it. One message.
- **Dropped connection.** A client loses its connection, the other client sends three messages, and after reconnect the first has all of them once, in order.
- **Sent while unreachable.** The message stays in the timeline, nothing reaches the server, and after the server returns it is delivered exactly once with no action from the sender.
- **Rapid sends keep their order.** 130 back-to-back sends from one client commit in the order sent, and paging back through 133 messages while new ones arrive yields every message exactly once, in order.
- **Server process crash.** The real `lazy-server` binary is SIGKILLed right after acknowledging a message and restarted: the message, its position, and its operation record are intact.
- **Real keyring.** A secret round-tripped through this machine's Secret Service (GNOME Keyring).
- **UI interactions on GPUI's test platform.** Real clicks (located through labelled elements) on Send, Send again for a rejected message (which resubmits as a new operation), the hover Copy button (clipboard receives the text), and Sign out; keyboard-only operation (Tab to Send and Enter; Shift+Tab to a sidebar channel and Enter switches to it); Shift+Enter inserts a newline; a confirmed message updates its row instead of adding one; scrolling to the top requests the page before the oldest loaded message.
- **Desktop app starts.** `lazy-desktop` opened its window (title `Lazy`, 1100×720) and ran for about a minute with no warnings or errors in its log. Its window was not examined further (see the incident below).

## Failed, and fixed

- **Rapid sends committed out of order.** The client launched every send as a separate concurrent request. Fixed with a per-conversation send queue.
- **Paging gap after a snapshot refresh.** A message you sent could be acknowledged below the newest page, and paging from that row skipped messages. Fixed; regression test added.
- **Cache could lose a message after restart.** The client simulation found a case where a restored position ran ahead of the cached messages. Fixed; the cursor and messages are now persisted together in one atomic write.
- **Send button pushed out of the window at the 640 px minimum.** The composer's hint text overflowed and clipped Send. Fixed; the hint now truncates and Send stays fixed. The sidebar also narrows and the rail hides below 760 px.
- **No semibold weight.** The system's default font lacks the weights the design asks for, so names and titles rendered regular. Fixed by bundling IBM Plex Sans (SIL Open Font License; license included).
- **Older history did not load if a flick skipped the threshold.** The scroll handler's range trails the layout by one event and a flick covers about 18 rows, so a 5-row threshold could be skipped entirely. Fixed by prefetching at 30 rows.
- **The reader's place jumped 78 px when older history loaded.** The old first row loses its day separator and author header once it has a predecessor, so it shrinks. Fixed by restoring the watched row's position after the new layout is measured.
- **The sign-in form's "Create an account" link was mouse-only.** The form's buttons are now tab stops operable with Enter or Space; so are the sidebar channels, Sign out, Send, and a failed message's retry button.
- **A too-short username said "Enter your username."** It now explains the rule.
- **Test harness bugs** (not product bugs): a client was only pumped while a test waited on it; the fault-injection proxy ended every connection after the first sever; a stale element label in a test.

## Incident

While trying to see the running app, I took one screenshot of a screen region computed from the window's reported geometry. The image showed other windows on the desktop, not Lazy. I deleted it immediately and took no further captures or desktop input. The real window's appearance on the desktop was therefore not observed; the offscreen renderer above is how the UI was examined instead. Native checks must capture only the app's own window; never use region capture for that.

## Unverified

- **The real window on a real compositor**: how it is presented, window decorations, behavior when moved between displays, fractional scaling (this monitor is at 1.25×), sleep/wake, long-session stability, cold start. The offscreen renders use the same renderer but not a compositor.
- **Real input methods.** Composition, candidate-window placement, and Enter-during-composition on a real IME. Enter-during-composition is proven only on GPUI's test platform with a fixed-width font. Plan task 6.2 covers this.
- **Screen readers.** The text input exposes role, label, and value; caret and selection are not exposed. Nothing was run against a real screen reader. The timeline is not yet accessibility-labelled.
- **Hover and keyboard focus appearance on a real display.** Focus rings are specified and the focus order is tested, but the focus styling was not looked at in a render.
- **macOS and Windows**, and Linux X11. Secure storage on those platforms is untested.
- **CI.** `.github/workflows/ci.yml` and `deny.toml` have never run; `cargo-deny` is not installed here.
- **Performance targets** other than bounded mounted rows (input-to-frame p95 under 16 ms, acknowledgment p95 under 300 ms): not measured.
- **TLS.** The client is built with rustls but has only been exercised over plain HTTP on loopback.
- **Behavior under load** and multi-device use.
- **Mobile builds** of the shared crates (iOS and Android targets are not installed here).

## Known gaps in the MVP

- Only `#general` exists; there is no UI to create channels, direct messages, threads, reactions, search, attachments, or notifications.
- Timeline text cannot be selected; each message has a hover **Copy** button instead, which is mouse-only.
- Some emoji render as small monochrome glyphs (for example ❤️, ☕, and family sequences such as 👩‍👩‍👧‍👦) because GPUI's font fallback picks a text font for them; most emoji, skin tones, and flags render in color. To be revisited with the Phase 2 emoji work.
- No caret blink in the composer. Double-click-drag selects by character, not word.
- Event-log retention defaults to 30 days and keeps the newest 1,000 events; receipts are kept indefinitely.
- Registration is open to anyone who can reach the server; the first registrant becomes the workspace owner. There is no rate limiting.
