# syntax=docker/dockerfile:1.6
FROM oven/bun:1-slim AS ui-builder

WORKDIR /src/web-v2
COPY web-v2/package.json web-v2/bun.lock ./
RUN bun install --frozen-lockfile
COPY web-v2 ./
RUN bun run typecheck && bun run build

FROM rust:1-bookworm AS builder

WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev libsqlite3-dev cmake clang ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ARG NINJANVR_RELEASE_VERSION=""
ENV NINJANVR_RELEASE_VERSION=${NINJANVR_RELEASE_VERSION}
RUN cargo build --release --bin ninjanvr

FROM ghcr.io/blakeblackshear/frigate:0.18.2 AS upstream-assets

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates tzdata curl libsqlite3-0 libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=upstream-assets /usr/local/go2rtc/bin/go2rtc /usr/local/go2rtc/bin/go2rtc
COPY --from=upstream-assets /usr/lib/ffmpeg/8.0/bin/ffmpeg /usr/local/bin/ffmpeg
COPY --from=upstream-assets /openvino-model /openvino-model
COPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libopenvino_c.so.2541 /usr/local/lib/ninjanvr/openvino/
COPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libopenvino.so.2541 /usr/local/lib/ninjanvr/openvino/
COPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libopenvino_intel_cpu_plugin.so /usr/local/lib/ninjanvr/openvino/
COPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libopenvino_ir_frontend.so.2541 /usr/local/lib/ninjanvr/openvino/
COPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libopenvino_onnx_frontend.so.2541 /usr/local/lib/ninjanvr/openvino/
COPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libtbb.so.12 /usr/local/lib/ninjanvr/openvino/
COPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libhwloc.so.15 /usr/local/lib/ninjanvr/openvino/
COPY --from=builder /src/target/release/ninjanvr /usr/local/bin/ninjanvr
COPY web /opt/ninjanvr/web
COPY --from=ui-builder /src/web-v2/dist /opt/ninjanvr/web-v2

ENV CONFIG_FILE=/config/config.yml
ENV NINJANVR_WEB_DIR=/opt/ninjanvr/web
ENV NINJANVR_WEB_V2_DIR=/opt/ninjanvr/web-v2
ENV NINJANVR_MEDIA_DIR=/media/frigate
ENV NINJANVR_OPENVINO_LIB_DIR=/usr/local/lib/ninjanvr/openvino
ENV LD_LIBRARY_PATH=/usr/local/lib/ninjanvr/openvino
ENV OMP_NUM_THREADS=1
ENV OV_CPU_THREADS_NUM=1

EXPOSE 5000 8971 8554 8555/tcp 8555/udp

ENTRYPOINT ["/usr/local/bin/ninjanvr"]
