# syntax=docker/dockerfile:1.7
#
# The Lazy server. Build from the repository root:   docker build -t lazy-server .
# Run it with a Postgres (see docker-compose.yml, or llm-docs/DEPLOY.md for Coolify).
#
# Only the three server crates are copied in (.dockerignore keeps the rest of the repository out of
# the build), and the workspace's `members = ["crates/*"]` then names just those three, so the
# desktop client and its GPUI dependency are never fetched or compiled.

FROM rust:1.99-bookworm AS build
WORKDIR /src
# (rust-toolchain.toml is left out on purpose: the image's own 1.99 toolchain is the one to use, and
# the file would make rustup fetch rustfmt and clippy for nothing.)
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The cache mounts keep downloaded crates and compiled dependencies between builds on this machine.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --profile dist -p lazy-server \
 && install -D target/dist/lazy-server /out/lazy-server

# Debian slim has the same glibc as the builder, so the binary runs as built. The server makes no
# outbound requests, so there are no CA certificates to carry either.
FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /nonexistent --no-create-home --shell /usr/sbin/nologin lazy \
 && install -d -o lazy -g lazy /data/attachments
COPY --from=build /out/lazy-server /usr/local/bin/lazy-server

ENV LAZY_LISTEN=0.0.0.0:8080 \
    LAZY_ATTACHMENTS_DIR=/data/attachments
# Attachment files live here; keep this volume with the database (back both up together).
VOLUME ["/data/attachments"]
EXPOSE 8080
USER lazy
HEALTHCHECK --interval=30s --timeout=5s --start-period=40s --retries=3 \
  CMD ["lazy-server", "healthcheck"]
ENTRYPOINT ["lazy-server"]
CMD ["serve"]
