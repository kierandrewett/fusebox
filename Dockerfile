# syntax=docker/dockerfile:1

ARG RUST_VERSION=1.94
ARG NODE_VERSION=22

# --- Build the web bundle (embedded into the binary via include_str!) ---
FROM node:${NODE_VERSION}-bookworm-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

# --- Build the Rust binary ---
FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /app
# The web bundle is prebuilt in the `web` stage and copied in below, so
# build.rs must not try to run npm itself.
ENV FUSEBOX_SKIP_WEB_BUILD=1

COPY Cargo.toml Cargo.lock build.rs ./
COPY assets ./assets
COPY src ./src
# include_str!("../web/dist/app.js") needs the bundle present at compile time.
COPY --from=web /web/dist ./web/dist

RUN --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --locked --release && \
    cp /app/target/release/fusebox /usr/local/bin/fusebox

FROM debian:bookworm-slim AS runtime

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl tzdata && \
    rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 10001 fusebox && \
    useradd \
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
