# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# Build stage
# ---------------------------------------------------------------------------
# libopus is statically linked from bundled source (opus2's `static` feature),
# so the builder needs cmake plus a C toolchain (present in the rust image).
# Note: the host's .cargo/config.toml is intentionally not copied — its
# Windows-only CMAKE_GENERATOR pin would break the Linux build.
FROM rust:1.89-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/blabber-bot

# Build dependencies first so they cache across source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---------------------------------------------------------------------------
# Runtime stage
# ---------------------------------------------------------------------------
# node:22 supplies the JavaScript runtime that yt-dlp needs for YouTube's
# player challenges; python3 runs yt-dlp itself. Audio decoding/encoding is
# pure Rust (symphonia + opus), so ffmpeg is not required.
FROM node:22-bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 python3-pip ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && pip3 install --no-cache-dir --break-system-packages -U yt-dlp yt-dlp-ejs \
    && useradd --create-home --uid 10001 bot \
    # Pre-create the data dir so the named volume inherits bot's ownership.
    && mkdir -p /home/bot/data /home/bot/.cache/yt-dlp \
    && chown -R bot:bot /home/bot/data /home/bot/.cache

COPY --from=builder /usr/src/blabber-bot/target/release/blabber-bot /usr/local/bin/blabber-bot
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

USER bot
ENV RUST_LOG=info

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
