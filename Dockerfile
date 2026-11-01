# syntax=docker/dockerfile:1.6
FROM rust:1-bookworm AS builder

WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev libsqlite3-dev cmake clang ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --locked --release --bin ninjanvr

FROM ghcr.io/blakeblackshear/frigate:0.18.2 AS upstream-assets

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates tzdata curl ffmpeg libsqlite3-0 libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=upstream-assets /usr/local/go2rtc/bin/go2rtc /usr/local/go2rtc/bin/go2rtc
COPY --from=upstream-assets /openvino-model /openvino-model
COPY --from=builder /src/target/release/ninjanvr /usr/local/bin/ninjanvr
COPY web /opt/ninjanvr/web

ENV CONFIG_FILE=/config/config.yml
ENV NINJANVR_WEB_DIR=/opt/ninjanvr/web
ENV NINJANVR_MEDIA_DIR=/media/frigate

EXPOSE 5000 8971 8554 8555/tcp 8555/udp

ENTRYPOINT ["/usr/local/bin/ninjanvr"]
