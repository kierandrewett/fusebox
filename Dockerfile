# syntax=docker/dockerfile:1

ARG RUST_VERSION=1.94

FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY assets ./assets
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --locked --release && \
    cp /app/target/release/fusebox /usr/local/bin/fusebox

FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

RUN groupadd --system --gid 10001 fusebox && \
    useradd \
        --system \
        --uid 10001 \
        --gid fusebox \
        --home-dir /data \
        --create-home \
        --shell /usr/sbin/nologin \
        fusebox

COPY --from=build /usr/local/bin/fusebox /usr/local/bin/fusebox

ENV FUSEBOX_BIND=0.0.0.0:8787 \
    FUSEBOX_STATE_PATH=/data/state.json \
    RUST_LOG=info

EXPOSE 8787
VOLUME ["/data"]
USER fusebox

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8787/health || exit 1

ENTRYPOINT ["/usr/local/bin/fusebox"]
