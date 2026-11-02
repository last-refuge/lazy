# Drop-ins: the plan

Status: decided 2026-11-01 (D39): all-Rust media, built as a standalone, reusable library.
Lazy's drop-ins are its first user; a later standalone Zoom-like client, and other projects, are
the next. Whatever is built here must be liftable into its own repository unchanged.

## Status (2026-11-01): the MVP (D42)

A vertical slice works end to end, video first: the `media/` workspace (`rtc-proto`, `rtc-sfu`,
`rtc-client`, `rtc-video`, `rtc-audio`), the server's join-token route, and the drop-in stage in
the desktop client (a tile per person, mute, video on or off, leave). It is not M0 to M3 done:
see D42 for what it is and what it leaves out, and `DEPLOY.md`, "Drop-ins", for running it. The
codec changed from the plan: H.264 through OpenH264 built from source, not VP8 (D42, with the
licensing question it raises).

Device choice (2026-11-02): the stage's **Devices** menu has a tab each for the camera,
microphone, and speakers ("Automatic" and every device the computer offers); a choice switches the
running call and is remembered per computer (`drop-in-devices.json`, like the appearance). On
Linux, audio goes through PulseAudio when a server answers (PipeWire desktops), so devices have
their system names; ALSA otherwise. Cameras are chosen by name; "Automatic" tries every camera,
webcams before raw laptop sensor feeds (Intel IPU6), which need libcamera or PipeWire (not yet).

Seen from outside (2026-11-02, D44): the SFU reports who is in each room to Lazy (signed room
reports to `RTC_REPORT_URL`, `/v1/rtc/report`), so the first person in posts "started a drop-in"
(which notifies the other person in a direct conversation), the sidebar marks the conversation
**Live**, the header and the notice offer **Join**, and the notice says how long it lasted once
everyone has left. Joins, leaves, and changes to a channel are posted as quiet notices too.

## Two products, one boundary

1. **The media library** (working name `rtc`; final name and license are the owner's call): a
   family of Rust crates for real-time audio, video, and screen sharing. Its own server (an SFU),
   its own client, its own signaling protocol and token format, its own docs and tests. It knows
   nothing about Lazy: no `lazy-*` dependency, no Lazy types, no Lazy database.
2. **Lazy's drop-ins**: Lazy's rules and experience on top of the library. Who may join, drop-in
   state and events, the dock and the drop-in window, the "started" post, ringing. Lazy issues
   the library's join tokens, receives its webhooks, and draws its frames.

The split is the same one hosted services use (Slack builds huddles on the Amazon Chime SDK;
Zed builds calls on LiveKit): the media layer is a product with an API, and the chat app is a
customer of it. Here we also own the media layer.

## The feature set (Lazy's drop-ins)

| Feature | Notes | Milestone |
| --- | --- | --- |
| Start or join a drop-in from a conversation's header | One click; audio only; anyone who can read the conversation | M3 |
| Who is in it, at a glance | Avatars and a headphones mark on the conversation in the sidebar and header | M3 |
| A drop-in dock that follows you | Bottom of the sidebar: the conversation, participants, mute, devices, leave | M3 |
| Mute, speaking indicators, device choice | Input and output devices with a level meter; mute visible to all | M3 |
| "Drop-in started" and "ended" posts | Join from the post; "Drop-in ended · 23 min · 4 people" (built, D44) | M3 |
| Reconnect without drama | Network blips, sleep and wake, devices unplugged | M3 |
| Screen sharing and the drop-in window | Pick a screen or window; others see it large; a "you are sharing" mark | M4 |
| Camera video | Optional per person; tiles; quality adapts | M5 |
| Ringing in direct messages, the drop-in thread, ringing someone in | Incoming-call card and ringtone; messages during a drop-in collect in a thread | M6 |
| Keyboard | Start or join (`Ctrl/Cmd+Shift+H`), mute toggle, leave; every control labelled | M3 to M6 |

Not planned: recording, transcripts, drawing on a shared screen, dial-in. Launch capacity is 12
per drop-in, raised only after load tests.

## The library

### Shape

```
rtc-proto     signaling messages, token claims, errors          (serde only; no runtime)
rtc-core      room, participant, track, and session state       (sans-IO; no runtime, no sockets)
rtc-sfu       the server: rooms, forwarding, auth, admin API,    (str0m + an async runtime
              webhooks, TURN-less ICE-TCP fallback               adapter, tokio first)
rtc-turn      a TURN server for restricted networks              (Rust; evaluated, M2)
rtc-client    the client transport: one str0m peer to the SFU,   (sans-IO core + runtime adapter)
              signaling, reconnect (ICE restart), subscriptions
rtc-audio     capture and playback (cpal), Opus, jitter buffer,  (codec and AEC behind traits)
              packet-loss concealment, echo cancellation,
              noise suppression, gain control, level meter
rtc-video     capture traits, VP8/VP9 encode and decode,          (codecs behind traits)
              simulcast layers, frame types (I420/BGRA)
rtc-gpui      drawing remote video and screen frames in GPUI      (optional; for GPUI hosts)
rtc-test      a headless client (tone in, tone checked) and       (for every repo that uses it)
              a synthetic-load driver
```

Principles, so the library survives being lifted out:

- **Sans-IO cores.** State machines and protocol logic take inputs and return outputs; sockets,
  timers, threads, and audio devices live in thin adapters (str0m works the same way). Phone
  clients and other runtimes reuse the cores.
- **Its own signaling and auth.** The SFU has its own WebSocket endpoint and verifies signed
  join tokens (room, identity, what may be published, expiry). Any host app issues tokens with a
  shared secret, as Lazy's server will; the SFU reports joins and leaves to the host through
  signed webhooks and takes orders (remove someone, end a room) through an admin API.
- **Replaceable hard parts.** Codecs and audio processing sit behind traits, so a pure-Rust
  implementation can replace a binding later without touching anything else.
- **No Lazy in it.** A check script (like `check-core-deps.sh`) fails the build if any `rtc-*`
  crate depends on a `lazy-*` one.
- **Where it lives.** A `media/` directory in this repository with its own Cargo workspace,
  lockfile, README, and CI job; Lazy depends on it by path. Moving it to its own repository is
  then a copy, and Lazy switches to a git or crates.io dependency.

### What "all Rust" means here, honestly

Our code is Rust end to end, servers and clients. Three pieces have no production-quality
pure-Rust implementation today, and every Rust media project links C/C++ libraries for them:

| Need | Plan | Pure-Rust status |
| --- | --- | --- |
| Opus (voice codec) | libopus through Rust bindings (BSD) | No mature pure-Rust encoder |
| Echo cancellation, noise suppression, gain | WebRTC's audio-processing module through Rust bindings (BSD) | Experimental at best; the biggest quality risk |
| H.264 (video; the plan said VP8/VP9, D42 changed it) | OpenH264 compiled from source (BSD; the owner is signing the AVC pool's patent licence) | No mature real-time pure-Rust encoder or decoder; the owner wants one later, behind the same traits |

They sit behind the library's traits; the spike (M0) confirms the binding crates' state,
licenses, and how they build on Linux, macOS (both architectures), and Windows. AV1 (rav1e for
encoding) is a later option for screen sharing, not a launch dependency.

### What str0m gives us, and what it does not

str0m (MIT or Apache-2.0; production SFU at Lookback; version 0.24 as of October 2026) gives:
ICE, DTLS/SRTP, RTP and RTCP, transport-wide congestion control and bandwidth estimation,
simulcast, NACK, data channels, and packetizing for Opus, VP8, VP9, and H.264. It is sans-IO
and works as a client as well as an SFU.

It does not give, so we build or bind: sockets and network interfaces; TURN (`rtc-turn`, and
relay candidates in the client); capture, codecs, playback; the jitter buffer and packet-loss
concealment; echo cancellation. Its peer-to-peer mode is less tested than its SFU mode, which is
why the client always talks to the SFU (even for two people) rather than directly to the other
person.

## Lazy's side

- **`lazy-server` drop-ins module.** Tables for drop-ins and participants; one live drop-in per
  conversation; `POST /v1/conversations/{id}/drop-in` checks the person can read the conversation
  and answers with the SFU's address and a short-lived join token; a webhook receiver for the
  SFU's signed participant events (the source of truth for who is in), reconciled through the
  admin API; removal through the admin API when someone loses access or is deactivated.
- **Protocol.** Today an older client cannot ignore an event type it does not know, so step one
  (its own client release, before drop-ins) adds an `Unknown` fallback to events. Drop-ins then
  bump `PROTOCOL_VERSION` once (14 to 15).
- **Client.** Drop-in state in `lazy-client-core` (Lazy's rules: which drop-in, who, mine); the
  runtime drives `rtc-client` and `rtc-audio` through effects; `lazy-ui` draws the dock and
  window, using `rtc-gpui` for frames. Media never runs on GPUI's foreground thread.
- **Deployment.** The SFU is its own process and container (`rtc-sfu`), next to `lazy-server`
  on Coolify. Media cannot go through Coolify's proxy: the host needs one UDP port for media (the
  SFU multiplexes on a single port) and one TCP port for the fallback; signaling sits behind
  HTTPS at `rtc.<domain>`. TURN, when built, needs its own port (and TLS for restrictive
  networks).
- **Encryption, described accurately.** Media is encrypted in transit (DTLS-SRTP) between each
  client and the SFU, and the SFU can read it. That is not end-to-end encryption, and neither the
  app nor the library will call it that. (End-to-end encryption through insertable streams is a
  possible later library feature.)

## Milestones

Each ends in something demonstrable; M3 is the first Lazy release with drop-ins.

**M0 — Spike: audio through a str0m SFU (decision gates).** A minimal SFU and two headless
clients in one process, then two real desktop clients on two machines: two-person audio with
cpal, Opus, and WebRTC audio processing. Measure mouth-to-ear latency, echo with laptop
speakers, CPU, and build size and time on the three OSes. Confirm the binding crates (Opus,
audio processing, VP8/VP9) build and are licensed acceptably. Record outcomes in
`VERIFICATION.md`. *Gate: if echo cancellation or a codec binding fails, choose its replacement
here.*

**M1 — Library foundations.** `rtc-proto`, `rtc-core`, `rtc-sfu` (rooms, audio forwarding,
tokens, admin API, webhooks), `rtc-client`, `rtc-test`; the `media/` workspace with its own CI
and the no-`lazy-*` check; a tiny example app in `media/examples` that is not Lazy (proof of
reuse). Tests: protocol round trips, state machines, a multi-client room in one test process.

**M2 — Audio quality and networks.** Jitter buffer, Opus FEC and DTX, packet-loss concealment,
device changes mid-call, automatic reconnect (ICE restart), ICE-TCP fallback, `rtc-turn` (or a
decision to defer TURN), level meters and speaking detection, a 12-person synthetic load test.

**M3 — Drop-ins in Lazy (audio).** Forward-compatible events (shipped first), the server module,
tokens, webhooks, removal; the dock, header button, sidebar presence, posts, devices, mute,
reconnect, keyboard and screen-reader labels; the SFU on Coolify and the `DEPLOY.md` steps.
First release with drop-ins.

**M4 — Screen sharing.** Capture (GPUI's `ScreenCaptureSource` on the desktop host, behind the
library's capture trait), VP8/VP9 encode, video forwarding with bandwidth estimation,
`rtc-gpui` drawing, the drop-in window, sharing indicators, macOS screen-recording permission.

**M5 — Camera video.** Camera capture adapters per OS, simulcast layers chosen per viewer,
tiles, adaptive quality.

**M6 — The rest of Lazy's experience.** DM ringing, the drop-in thread, ringing someone in,
notification settings, the accessibility pass, soak tests, and the device and network matrix.

This is substantially more work than using LiveKit, by the owner's choice, in exchange for a
media stack the owner fully controls and can reuse. The plan front-loads the risky parts (echo,
codecs, networks) so they surface in M0 to M2, before Lazy's UI depends on them.

## Testing

- Library: unit tests on the sans-IO cores; in-process rooms with headless clients (tones sent
  and detected, loss and jitter injected); synthetic load at capacity; its own CI.
- Lazy: server tests against a real SFU started by the test harness (as Postgres is): tokens,
  webhook signatures, reconciliation, removal; client-core tests of drop-in state; UI tests of
  the dock.
- Manual matrix, recorded in `VERIFICATION.md`: Bluetooth headsets, laptop speakers (echo),
  unplugging devices, denying permissions, sleep and wake, restrictive networks (TCP only),
  join time (target p95 under 3 seconds).

## Decisions still open (the owner's)

- The library's name and license (MIT OR Apache-2.0, like str0m, keeps it reusable anywhere).
- Whether it stays in `media/` here until M3, or gets its own repository from M1.
- Where the Coolify host can expose a UDP and a TCP port for the SFU, and the `rtc.` domain.
