# rtc: real-time audio and video in Rust

A media library with its own server (an SFU), client, signaling, and join tokens. Lazy's drop-ins
are its first user (`../llm-docs/DROP_INS.md`, D39 and D42); nothing here depends on Lazy, so it can
move to its own repository unchanged. License: MIT OR Apache-2.0 (the final name and license are
the owner's call).

| Crate | What it is |
| --- | --- |
| `rtc-proto` | Signaling messages (JSON on the SFU's WebSocket, `/rtc`), join tokens (`rtc1.<claims>.<HMAC-SHA256>`), and signed room reports (`webhook`). Serde only. |
| `rtc-sfu` | The server: one UDP port for all media (ICE-lite), rooms, forwarding, keyframe requests. A library and the `rtc-sfu` binary. |
| `rtc-client` | One connection to the SFU per call: sends a camera and a microphone, receives everyone else into a `FrameStore` and a mixer. |
| `rtc-video` | Frames (BGRA), the frame store, sources (camera through nokhwa, a test pattern), H.264 through OpenH264 built from source. |
| `rtc-audio` | Opus (libopus transpiled to Rust), microphone and speakers through cpal, mixing, resampling. |

```sh
cargo test --workspace                      # includes a room with two clients over loopback UDP
cargo run -p rtc-sfu -- --secret "$(openssl rand -hex 32)" --public-ip 203.0.113.7
docker build -t rtc-sfu .
```

A host application signs a token for each person (`rtc_proto::token::sign`, with the secret the
SFU was started with) and gives the client the SFU's address and the token; see
`rtc_client::Call::start`.

Room reports: started with `--report-url` (`RTC_REPORT_URL`), the SFU `POST`s who is in a room
(`rtc_proto::webhook::RoomReport`, signed with the same secret, header `rtc-signature`) whenever
someone joins or leaves, and every 30 seconds for every room with someone in it. Each report is
the room's whole state, so a lost one is corrected by the next; a host treats a room it has not
heard about for a few heartbeats as ended. `rtc_proto::webhook::verify` checks one.

Building on Linux needs ALSA's headers (`libasound2-dev`) and libclang (`libclang-dev`, for the
camera bindings); the SFU alone needs neither.

Devices: `rtc_video::source::cameras()`, `rtc_audio::microphones()` and `rtc_audio::speakers()`
list what a machine offers (on Linux, audio through PulseAudio when a server answers, else ALSA);
a call starts with any of them and switches mid-call (`Call::set_video_input`, `set_audio_input`,
`set_speakers`). `cargo run -p rtc-client --features devices --example devices` prints the lists.

Not here yet: echo cancellation and noise suppression, packet-loss concealment,
reconnecting after a network change, TURN and TCP for restrictive networks, simulcast, screen
sharing, and an admin API (removing someone, ending a room).
