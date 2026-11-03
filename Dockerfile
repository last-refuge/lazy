# syntax=docker/dockerfile:1.7
#
# The Lazy server. Build from the repository root:   docker build -t lazy-server .
# Run it with a Postgres (see docker-compose.yml, or llm-docs/DEPLOY.md for Coolify).
#
# Only the server's crates and the web client's (D47) are copied in (.dockerignore keeps the rest of
# the repository out of the build): lazy-domain, lazy-protocol, and lazy-server, and lazy-client-core
# and lazy-web, which the server serves under /web/ as WebAssembly. The workspace's
# `members = ["crates/*"]` then names just those five, so the desktop client and its GPUI
# dependency are never fetched or compiled.

FROM rust:1.99-bookworm AS build
WORKDIR /src
# (rust-toolchain.toml is left out on purpose: the image's own 1.99 toolchain is the one to use, and
# the file would make rustup fetch rustfmt and clippy for nothing.)
# The web client's tools, before the sources so this layer is reused: the WebAssembly target and
# the wasm-bindgen CLI at exactly the version in Cargo.lock (scripts/build-web.sh checks).
ARG WASM_BINDGEN_VERSION=0.2.129
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    rustup target add wasm32-unknown-unknown \
 && cargo install wasm-bindgen-cli --version "$WASM_BINDGEN_VERSION" --locked --root /usr/local
COPY Cargo.toml Cargo.lock ./
COPY crates/lazy-domain ./crates/lazy-domain
COPY crates/lazy-protocol ./crates/lazy-protocol
COPY crates/lazy-server ./crates/lazy-server
COPY crates/lazy-client-core ./crates/lazy-client-core
COPY crates/lazy-web ./crates/lazy-web
# The one crate of the media library the server uses (drop-in join tokens), with its workspace.
COPY media/Cargo.toml ./media/Cargo.toml
COPY media/crates/rtc-proto ./media/crates/rtc-proto
# The web client's build script, and the icon and fonts it shares with the desktop app (copied
# outside crates/: a crates/lazy-ui with no manifest would stop the workspace from loading).
COPY scripts/build-web.sh ./scripts/build-web.sh
COPY branding/icons/favicon.svg ./web-assets/favicon.svg
COPY crates/lazy-ui/assets/fonts ./web-assets/fonts
# The cache mounts keep downloaded crates and compiled dependencies between builds on this machine.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --profile dist -p lazy-server \
 && install -D target/dist/lazy-server /out/lazy-server \
 && LAZY_WEB_FONTS=web-assets/fonts LAZY_WEB_FAVICON=web-assets/favicon.svg scripts/build-web.sh /out/web

# Debian slim has the same glibc as the builder, so the binary runs as built. CA certificates are
# for the server's own HTTPS requests (the GitHub connector); without them it runs with GitHub off.
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /nonexistent --no-create-home --shell /usr/sbin/nologin lazy \
 && install -d -o lazy -g lazy /data/attachments
COPY --from=build /out/lazy-server /usr/local/bin/lazy-server
COPY --from=build /out/web /usr/share/lazy/web

ENV LAZY_LISTEN=0.0.0.0:8080 \
    LAZY_ATTACHMENTS_DIR=/data/attachments \
    LAZY_WEB_DIR=/usr/share/lazy/web
# Attachment files live here; keep this volume with the database (back both up together).
VOLUME ["/data/attachments"]
EXPOSE 8080
USER lazy
HEALTHCHECK --interval=30s --timeout=5s --start-period=40s --retries=3 \
  CMD ["lazy-server", "healthcheck"]
ENTRYPOINT ["lazy-server"]
CMD ["serve"]
