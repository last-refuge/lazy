# syntax=docker/dockerfile:1.7
#
# The media library's SFU (rtc-sfu). Build from this directory:   docker build -t rtc-sfu media
# It needs a secret shared with the host app (RTC_SECRET), and its UDP port reachable from clients;
# see the README and Lazy's llm-docs/DEPLOY.md ("Drop-ins").

FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release -p rtc-sfu \
 && install -D target/release/rtc-sfu /out/rtc-sfu

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /nonexistent --no-create-home --shell /usr/sbin/nologin rtc
COPY --from=build /out/rtc-sfu /usr/local/bin/rtc-sfu
ENV RTC_HTTP=0.0.0.0:7880 \
    RTC_UDP=0.0.0.0:7881
# Signaling (HTTP and WebSocket; put it behind the HTTPS proxy) and media (UDP, published directly).
EXPOSE 7880/tcp 7881/udp
USER rtc
ENTRYPOINT ["rtc-sfu"]
